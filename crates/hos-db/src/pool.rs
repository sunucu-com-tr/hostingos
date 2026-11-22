//! Connection pool and migrations.
//!
//! Neon / PgBouncer notes:
//!
//! * The application uses the **pooled** URL (`DATABASE_URL`, Neon `-pooler`
//!   host, PgBouncer in transaction mode). sqlx's named prepared statements work
//!   there because Neon's PgBouncer (≥ 1.21, `max_prepared_statements` set)
//!   tracks protocol-level prepared statements. Nothing in this crate relies on
//!   session state (no `SET`, `LISTEN`, session advisory locks or temp tables).
//! * Migrations take a session-level advisory lock, so they must run over the
//!   **direct** URL (`DATABASE_URL_DIRECT`) — see `hos-migrate`.
//! * Serverless computes suspend when idle: `min_connections` defaults to 0, idle
//!   connections are closed after a few minutes, every connection is pinged
//!   before use, and the acquire timeout allows for a cold start.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use hos_core::config::{AppConfig, redact_url_password, strip_channel_binding};
use hos_core::secret::SecretString;
use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// The embedded migrations (`crates/hos-db/migrations`).
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Pool settings. Build with [`DbConfig::new`], [`DbConfig::from_app_config`]
/// or [`DbConfig::for_migrations`], then adjust with the `with_*` methods.
#[derive(Clone)]
pub struct DbConfig {
    /// Postgres URL (`channel_binding` already stripped).
    pub url: SecretString,
    /// Upper bound of open connections.
    pub max_connections: u32,
    /// Connections kept open while idle (0 lets a serverless compute suspend).
    pub min_connections: u32,
    /// How long `acquire()` waits (covers Neon cold starts).
    pub acquire_timeout: Duration,
    /// Close connections idle for longer than this.
    pub idle_timeout: Option<Duration>,
    /// Recycle connections older than this.
    pub max_lifetime: Option<Duration>,
    /// Prepared statement cache per connection (0 disables named statements).
    pub statement_cache_capacity: usize,
    /// `application_name` reported to Postgres.
    pub application_name: String,
}

impl fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DbConfig")
            .field("url", &redact_url_password(self.url.expose()))
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .field("statement_cache_capacity", &self.statement_cache_capacity)
            .field("application_name", &self.application_name)
            .finish()
    }
}

impl DbConfig {
    /// Defaults for an application pool on `url` (`channel_binding` is stripped).
    pub fn new(url: &str) -> Self {
        Self {
            url: SecretString::new(strip_channel_binding(url.trim())),
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(15),
            idle_timeout: Some(Duration::from_secs(300)),
            max_lifetime: Some(Duration::from_secs(1800)),
            statement_cache_capacity: 100,
            application_name: "hostingos".to_owned(),
        }
    }

    /// Application pool over the pooled `DATABASE_URL`.
    pub fn from_app_config(cfg: &AppConfig) -> Self {
        Self::new(cfg.database_url.expose())
    }

    /// Small pool over `DATABASE_URL_DIRECT` for migrations and maintenance.
    pub fn for_migrations(cfg: &AppConfig) -> Self {
        Self::new(cfg.database_url_direct.expose())
            .with_max_connections(2)
            .with_application_name("hostingos-migrate")
    }

    /// Sets `max_connections` (at least 1).
    #[must_use]
    pub fn with_max_connections(mut self, n: u32) -> Self {
        self.max_connections = n.max(1);
        self.min_connections = self.min_connections.min(self.max_connections);
        self
    }

    /// Sets `min_connections` (capped at `max_connections`).
    #[must_use]
    pub fn with_min_connections(mut self, n: u32) -> Self {
        self.min_connections = n.min(self.max_connections);
        self
    }

    /// Sets the acquire timeout.
    #[must_use]
    pub fn with_acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    /// Sets the per-connection prepared statement cache size.
    #[must_use]
    pub fn with_statement_cache_capacity(mut self, capacity: usize) -> Self {
        self.statement_cache_capacity = capacity;
        self
    }

    /// Sets `application_name`.
    #[must_use]
    pub fn with_application_name(mut self, name: &str) -> Self {
        self.application_name = name.to_owned();
        self
    }

    /// Whether the URL points at a connection pooler (Neon `-pooler` host or
    /// an explicit `pgbouncer=true` parameter).
    pub fn is_pooled(&self) -> bool {
        is_pooler_url(self.url.expose())
    }

    /// Connect options derived from this config.
    pub fn connect_options(&self) -> Result<PgConnectOptions, sqlx::Error> {
        let opts = PgConnectOptions::from_str(self.url.expose())?
            .application_name(&self.application_name)
            .statement_cache_capacity(self.statement_cache_capacity);
        Ok(opts)
    }
}

/// `true` for Neon pooler hosts (`ep-xxx-pooler.region.aws.neon.tech`) and URLs
/// carrying `pgbouncer=true`.
pub fn is_pooler_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let host_is_pooler = parsed
        .host_str()
        .and_then(|h| h.split('.').next())
        .is_some_and(|label| label.ends_with("-pooler"));
    host_is_pooler
        || parsed
            .query_pairs()
            .any(|(k, v)| k.eq_ignore_ascii_case("pgbouncer") && v.eq_ignore_ascii_case("true"))
}

/// Opens a connection pool. The first connection is established eagerly so a
/// wrong URL / password fails here rather than on the first request.
pub async fn connect(cfg: &DbConfig) -> Result<PgPool, sqlx::Error> {
    let options = cfg.connect_options()?;
    PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .min_connections(cfg.min_connections)
        .acquire_timeout(cfg.acquire_timeout)
        .idle_timeout(cfg.idle_timeout)
        .max_lifetime(cfg.max_lifetime)
        .test_before_acquire(true)
        .connect_with(options)
        .await
}

/// Key of the session-level advisory lock every migration run of HostingOS
/// holds ([`migrate`], [`migrate_if_behind`]: `hostingos migrate`,
/// `hostingos serve --migrate`, `setup`, demo start), in addition to sqlx's
/// own: the status check and the run form one critical section, so replicas
/// starting together and a separate `hostingos migrate` take turns.
pub const MIGRATION_LOCK_KEY: i64 = 0x686f_735f_6d69_6772; // "hos_migr"

/// Which of the embedded migrations the database has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaStatus {
    /// Embedded versions that are not (successfully) applied: this binary
    /// needs them and fails on the missing tables / columns.
    pub missing: Vec<i64>,
    /// Applied versions this binary does not know: a newer release has
    /// migrated the database (normal while an older release still runs, or
    /// after going back to one).
    pub unknown: Vec<i64>,
}

impl SchemaStatus {
    /// Some embedded migration is not applied.
    pub fn is_behind(&self) -> bool {
        !self.missing.is_empty()
    }

    /// The database has migrations this binary does not know.
    pub fn is_ahead(&self) -> bool {
        !self.unknown.is_empty()
    }

    /// Compares the successfully applied versions with `expected`.
    pub fn compare(applied: &[i64], expected: &[i64]) -> Self {
        Self {
            missing: expected
                .iter()
                .copied()
                .filter(|v| !applied.contains(v))
                .collect(),
            unknown: applied
                .iter()
                .copied()
                .filter(|v| !expected.contains(v))
                .collect(),
        }
    }
}

/// The versions of [`MIGRATOR`] (up migrations).
pub fn embedded_versions() -> Vec<i64> {
    MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| m.version)
        .collect()
}

/// Successfully applied migration versions; empty on a database that has
/// never been migrated (no `_sqlx_migrations` table).
pub async fn applied_versions(db: impl sqlx::PgExecutor<'_>) -> Result<Vec<i64>, sqlx::Error> {
    match sqlx::query_scalar::<_, i64>(
        "select version from _sqlx_migrations where success order by version",
    )
    .fetch_all(db)
    .await
    {
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P01") => Ok(Vec::new()),
        other => other,
    }
}

/// [`SchemaStatus`] of the database against [`MIGRATOR`].
pub async fn schema_status(db: impl sqlx::PgExecutor<'_>) -> Result<SchemaStatus, sqlx::Error> {
    let applied = applied_versions(db).await?;
    Ok(SchemaStatus::compare(&applied, &embedded_versions()))
}

/// Takes [`MIGRATION_LOCK_KEY`] on `conn`, saying so when another process
/// holds it.
async fn lock_migrations(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    let free: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await?;
    if !free {
        tracing::info!("another process is applying migrations; waiting for it to finish");
        sqlx::query("select pg_advisory_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Ends a locked migration session: unlocks after success; after a failure
/// the connection is closed instead, which also drops the lock sqlx does
/// not release on its error paths.
async fn unlock_migrations(mut conn: sqlx::pool::PoolConnection<sqlx::Postgres>, ok: bool) {
    let unlocked = ok
        && sqlx::query("select pg_advisory_unlock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *conn)
            .await
            .is_ok();
    if !unlocked {
        let _ = conn.close().await;
    }
}

/// Applies all pending embedded migrations. Use a pool built from the direct
/// (non-pooled) URL: the run holds session-level advisory locks
/// ([`MIGRATION_LOCK_KEY`] and sqlx's own).
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    let mut conn = pool.acquire().await?;
    lock_migrations(&mut conn).await?;
    // `run_direct` (what `run` calls) on the connection itself: `run`'s
    // `Acquire` bound makes the future not `Send`-provable in spawned tasks.
    let result = MIGRATOR.run_direct(&mut *conn).await;
    unlock_migrations(conn, result.is_ok()).await;
    result
}

/// What [`migrate_if_behind`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// Versions applied now (empty: the schema was current).
    pub applied: Vec<i64>,
    /// Applied versions this binary does not know (see [`SchemaStatus`]).
    pub unknown: Vec<i64>,
}

/// Applies the pending embedded migrations, if any, under
/// [`MIGRATION_LOCK_KEY`]: the start-up migration of `hostingos serve`.
/// Unlike [`migrate`] it leaves a database that is only *ahead* of this
/// binary alone (nothing to apply; sqlx would refuse the unknown versions),
/// so an older release can still start on it. A database that is behind
/// and has unknown versions is handed to sqlx, which refuses it.
pub async fn migrate_if_behind(pool: &PgPool) -> Result<MigrationOutcome, MigrateError> {
    let mut conn = pool.acquire().await?;
    lock_migrations(&mut conn).await?;
    let result = async {
        let before = schema_status(&mut *conn).await?;
        if before.is_behind() {
            MIGRATOR.run_direct(&mut *conn).await?;
        }
        Ok(MigrationOutcome {
            applied: before.missing,
            unknown: before.unknown,
        })
    }
    .await;
    unlock_migrations(conn, result.is_ok()).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pooler_detection() {
        assert!(is_pooler_url(
            "postgres://u:p@ep-cool-name-123-pooler.eu-central-1.aws.neon.tech/db?sslmode=require"
        ));
        assert!(!is_pooler_url(
            "postgres://u:p@ep-cool-name-123.eu-central-1.aws.neon.tech/db?sslmode=require"
        ));
        assert!(is_pooler_url(
            "postgres://u:p@localhost:6432/db?pgbouncer=true"
        ));
        assert!(!is_pooler_url("not a url"));
    }

    #[test]
    fn config_strips_channel_binding_and_redacts() {
        let cfg =
            DbConfig::new("postgres://u:secretpw@h/db?sslmode=require&channel_binding=require")
                .with_max_connections(0);
        assert_eq!(
            cfg.url.expose(),
            "postgres://u:secretpw@h/db?sslmode=require"
        );
        assert_eq!(cfg.max_connections, 1);
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("secretpw"));
        assert!(cfg.connect_options().is_ok());
    }

    #[test]
    fn migrations_are_embedded_in_order() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert!(versions.len() >= 11);
        assert!(versions.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(embedded_versions(), versions);
    }

    #[test]
    fn schema_status_tells_behind_from_ahead() {
        let expected = [1, 2, 3];
        let current = SchemaStatus::compare(&[1, 2, 3], &expected);
        assert!(!current.is_behind() && !current.is_ahead());
        let behind = SchemaStatus::compare(&[1, 2], &expected);
        assert_eq!(behind.missing, [3]);
        assert!(behind.is_behind() && !behind.is_ahead());
        let ahead = SchemaStatus::compare(&[1, 2, 3, 4], &expected);
        assert_eq!(ahead.unknown, [4]);
        assert!(ahead.is_ahead() && !ahead.is_behind());
        let empty = SchemaStatus::compare(&[], &expected);
        assert_eq!(empty.missing, expected);
    }
}
