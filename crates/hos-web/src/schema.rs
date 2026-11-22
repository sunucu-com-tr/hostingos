//! The database schema against this binary (docs/DEPLOYMENT.md,
//! "Migrations"):
//!
//! * [`migrate_on_start`] — `hostingos serve --migrate`,
//!   `HOS_MIGRATE_ON_START=1`, and always in demo mode: pending migrations
//!   are applied before the listener opens, under the migration advisory
//!   lock ([`hos_db::MIGRATION_LOCK_KEY`]; replicas starting together and a
//!   separate `hostingos migrate` take turns). A failed migration stops the
//!   start. A database only *ahead* of the binary is left alone.
//! * [`SchemaGate`] (in `AppState`) — whether the database is *behind* this
//!   binary (embedded migrations not applied). Then every page answers `503`
//!   with the maintenance page ([`layer`]) and `/readyz` reports not ready,
//!   instead of `500`s from missing tables; the gate re-checks (pages: at
//!   most every [`RECHECK_EVERY`], `/readyz`: every probe) and serves again
//!   as soon as the migrations are in, without a restart. A database
//!   *ahead* of the binary (a newer release migrated it) is logged and
//!   served: an older release running on an additively migrated schema is
//!   how rollbacks work (docs/UPDATES.md).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use hos_core::config::AppConfig;
use hos_db::{PgPool, SchemaStatus};

use crate::state::AppState;

/// Migrate at startup (`1`, `true`, `yes`, `on`); same as `serve --migrate`.
pub const MIGRATE_ON_START_ENV: &str = "HOS_MIGRATE_ON_START";

/// How often a page request re-checks a schema that was behind.
pub const RECHECK_EVERY: Duration = Duration::from_secs(5);

/// `HOS_MIGRATE_ON_START` is truthy.
pub fn migrate_on_start_from_env() -> bool {
    std::env::var(MIGRATE_ON_START_ENV).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Applies pending migrations before the server starts (see the module
/// docs). Errors stop the start.
pub async fn migrate_on_start(config: &AppConfig) -> anyhow::Result<()> {
    let pool = hos_db::connect(&hos_db::DbConfig::for_migrations(config))
        .await
        .context("connecting to the database for the startup migrations")?;
    let outcome = hos_db::migrate_if_behind(&pool).await;
    pool.close().await;
    let outcome = outcome.context(
        "applying migrations at startup failed; the server does not start (fix the cause, \
         then start again or run `hostingos migrate`)",
    )?;
    if outcome.applied.is_empty() {
        tracing::info!("startup migrations: the database schema is current");
    } else {
        tracing::info!(
            count = outcome.applied.len(),
            versions = ?outcome.applied,
            "startup migrations applied"
        );
    }
    Ok(())
}

/// The schema state the server serves under (see the module docs). Until a
/// check has run it lets everything through.
#[derive(Debug, Default)]
pub struct SchemaGate {
    behind: AtomicBool,
    ahead_logged: AtomicBool,
    last_check: Mutex<Option<Instant>>,
    checking: tokio::sync::Mutex<()>,
}

impl SchemaGate {
    /// Whether the last check found the database behind.
    pub fn is_behind(&self) -> bool {
        self.behind.load(Ordering::Relaxed)
    }

    /// Compares the database with the embedded migrations and updates the
    /// gate; logs when it closes or opens.
    pub async fn check(&self, pool: &PgPool) -> Result<SchemaStatus, sqlx::Error> {
        let status = hos_db::schema_status(pool).await?;
        self.record(&status);
        Ok(status)
    }

    fn record(&self, status: &SchemaStatus) {
        if let Ok(mut last) = self.last_check.lock() {
            *last = Some(Instant::now());
        }
        let was_behind = self.behind.swap(status.is_behind(), Ordering::Relaxed);
        match (was_behind, status.is_behind()) {
            (false, true) => tracing::error!(
                pending = status.missing.len(),
                versions = ?status.missing,
                "the database schema is behind this release: migrations are not applied. \
                 Pages answer 503 and /readyz reports not ready until they are; run \
                 `hostingos migrate` (or start with `hostingos serve --migrate` / \
                 HOS_MIGRATE_ON_START=1). No restart is needed afterwards"
            ),
            (true, false) => {
                tracing::info!("the database schema is current now; serving requests again");
            }
            _ => {}
        }
        if status.is_ahead() && !self.ahead_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                versions = ?status.unknown,
                "the database has migrations this release does not know: a newer release \
                 migrated it. Serving anyway (an older release runs on an additively migrated \
                 schema); if pages fail, deploy the newer release again"
            );
        }
    }

    /// Whether pages must wait for migrations: re-checks a behind schema at
    /// most every [`RECHECK_EVERY`] (one check at a time; a database error
    /// keeps the last result).
    pub async fn blocks_pages(&self, pool: &PgPool) -> bool {
        if !self.is_behind() {
            return false;
        }
        let due = self
            .last_check
            .lock()
            .map(|l| l.is_none_or(|t| t.elapsed() >= RECHECK_EVERY))
            .unwrap_or(true);
        // `_one` is held while the check runs: one re-check at a time.
        if due
            && let Ok(_one) = self.checking.try_lock()
            && let Err(e) = self.check(pool).await
        {
            tracing::warn!(error = %e, "re-checking the database schema failed");
        }
        self.is_behind()
    }
}

/// Paths served while the schema is behind (no database needed).
fn exempt(path: &str) -> bool {
    path == "/healthz"
        || path == "/readyz"
        || path == "/favicon.ico"
        || path.starts_with("/static/")
        || path.starts_with("/assets/")
}

/// Middleware: the maintenance page (`503`, `Retry-After`) for everything
/// but health checks and assets while the database is behind.
pub async fn layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if exempt(req.uri().path()) || !state.schema.blocks_pages(&state.pool).await {
        return next.run(req).await;
    }
    crate::routes::platform::updates::maintenance::unavailable(&state, &req, 30)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_opens_and_closes_with_the_status() {
        let gate = SchemaGate::default();
        assert!(!gate.is_behind(), "unchecked: open");
        gate.record(&SchemaStatus {
            missing: vec![52],
            unknown: vec![],
        });
        assert!(gate.is_behind());
        gate.record(&SchemaStatus::default());
        assert!(!gate.is_behind());
        // Ahead only: served.
        gate.record(&SchemaStatus {
            missing: vec![],
            unknown: vec![99],
        });
        assert!(!gate.is_behind());
    }

    #[test]
    fn health_checks_and_assets_stay_reachable() {
        for p in [
            "/healthz",
            "/readyz",
            "/static/css/app.css",
            "/assets/app.css",
        ] {
            assert!(exempt(p), "{p}");
        }
        for p in ["/", "/admin", "/auth/login", "/api/v1/me", "/platform"] {
            assert!(!exempt(p), "{p}");
        }
    }
}
