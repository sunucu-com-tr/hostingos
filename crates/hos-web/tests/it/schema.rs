//! The database schema against the binary (`hos_web::schema`,
//! docs/DEPLOYMENT.md "Migrations"): migrations at startup under the
//! migration lock, and pages / `/readyz` while the database is behind.
//!
//! The shared test database is always fully migrated. A database that is
//! behind (or ahead) is simulated per session: a *temporary* table named
//! `_sqlx_migrations` shadows the real one for that connection only (the
//! session's temporary schema is searched first), so nothing shared is
//! changed and nothing is left behind.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use hos_db::{DbConfig, PgPool};
use http::{Request, StatusCode};
use tower::ServiceExt;

use crate::common::{self, Res, TestApp};

async fn send(router: &Router, host: &str, path: &str, accept: &str) -> Res {
    let res = router
        .clone()
        .oneshot(
            Request::get(path)
                .header("host", host)
                .header("accept", accept)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    Res {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// A one-connection pool whose `_sqlx_migrations` is a session-local copy
/// holding `versions`; `None` (test skipped) when the migration URL goes
/// through a pooler.
async fn pool_with_history(app: &TestApp, versions: &[i64]) -> Option<PgPool> {
    // Only over a direct connection: a transaction-mode pooler runs each
    // statement on whichever server session is free, and a temporary table
    // created there would outlive this test in the pooler's session and
    // shadow `_sqlx_migrations` for other clients.
    let cfg = DbConfig::for_migrations(&app.state.config)
        .with_max_connections(1)
        .with_application_name("hos-web-tests");
    if cfg.is_pooled() {
        eprintln!("DATABASE_URL_DIRECT is not a direct connection — skipping");
        return None;
    }
    let pool = hos_db::connect(&cfg).await.expect("connect");
    sqlx::query(
        "create temporary table _sqlx_migrations (version bigint primary key, success boolean not null)",
    )
    .execute(&pool)
    .await
    .expect("temporary migration history");
    for v in versions {
        record(&pool, *v).await;
    }
    Some(pool)
}

/// Removes the session's copy (closing the session would, too).
async fn forget_history(pool: PgPool) {
    sqlx::query("drop table if exists pg_temp._sqlx_migrations")
        .execute(&pool)
        .await
        .expect("drop temporary history");
    pool.close().await;
}

async fn record(pool: &PgPool, version: i64) {
    sqlx::query("insert into pg_temp._sqlx_migrations (version, success) values ($1, true)")
        .bind(version)
        .execute(pool)
        .await
        .expect("record version");
}

#[tokio::test]
async fn pages_wait_for_pending_migrations_and_recover_without_restart() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let versions = hos_db::embedded_versions();
    let (last, earlier) = versions.split_last().expect("migrations");
    let Some(pool) = pool_with_history(&app, earlier).await else {
        app.cleanup().await;
        return;
    };
    let state = common::new_state(
        pool.clone(),
        app.state.config.clone(),
        app.state.web.clone(),
    );
    let router = hos_web::app(state.clone());
    let html = "text/html";

    // The startup check of `hostingos serve`.
    let status = state.schema.check(&pool).await.expect("check");
    assert_eq!(status.missing, [*last]);
    assert!(state.schema.is_behind());

    // Pages: the maintenance page with 503, not a 500 from a missing table.
    let page = send(&router, &app.host, "/", html).await;
    assert_eq!(
        page.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        page.body
    );
    assert!(page.body.contains("Sistemi güncelliyoruz"), "{}", page.body);
    assert_eq!(page.headers["retry-after"], "30");
    let api = send(&router, &app.host, "/api/v1/me", "application/json").await;
    assert_eq!(api.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(api.body.contains("\"maintenance\""), "{}", api.body);
    // Probes: alive, not ready.
    let ready = send(&router, &app.host, "/readyz", html).await;
    assert_eq!(ready.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        ready.body.contains("\"schema\":\"behind\""),
        "{}",
        ready.body
    );
    assert_eq!(
        send(&router, &app.host, "/healthz", html).await.status,
        StatusCode::OK
    );

    // `hostingos migrate` runs elsewhere; the next probe sees it.
    record(&pool, *last).await;
    let ready = send(&router, &app.host, "/readyz", html).await;
    assert_eq!(ready.status, StatusCode::OK, "{}", ready.body);
    assert!(
        ready.body.contains("\"schema\":\"current\""),
        "{}",
        ready.body
    );
    assert!(!state.schema.is_behind());
    let page = send(&router, &app.host, "/", html).await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "served again without a restart"
    );
    assert!(!page.body.contains("Sistemi güncelliyoruz"));

    // A newer release migrated the database further: still ready.
    record(&pool, i64::MAX).await;
    let ready = send(&router, &app.host, "/readyz", html).await;
    assert_eq!(ready.status, StatusCode::OK, "{}", ready.body);
    assert!(
        ready.body.contains("\"schema\":\"ahead\""),
        "{}",
        ready.body
    );
    assert_eq!(
        send(&router, &app.host, "/", html).await.status,
        StatusCode::OK
    );

    drop(router);
    drop(state);
    forget_history(pool).await;
    app.cleanup().await;
}

/// Without a startup check (and with a current schema) nothing is gated.
#[tokio::test]
async fn a_current_schema_is_ready() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let ready = app.get("/readyz").await;
    assert_eq!(ready.status, StatusCode::OK, "{}", ready.body);
    assert!(
        ready.body.contains("\"schema\":\"current\""),
        "{}",
        ready.body
    );
    assert_eq!(app.get("/").await.status, StatusCode::OK);
    app.cleanup().await;
}

/// `serve --migrate` on a database that is already migrated is a no-op, also
/// for several replicas at once; it waits for a migration run of another
/// process (the shared advisory lock) instead of racing it.
#[tokio::test]
async fn startup_migrations_take_turns_with_other_runs() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let config = app.state.config.clone();
    let migrations = hos_db::connect(&DbConfig::for_migrations(&config).with_max_connections(4))
        .await
        .expect("connect for migrations");

    // Three replicas starting together.
    let runs =
        futures::future::join_all((0..3).map(|_| hos_db::migrate_if_behind(&migrations))).await;
    for run in runs {
        let outcome = run.expect("startup migration");
        assert!(outcome.applied.is_empty(), "{outcome:?}");
    }
    hos_web::schema::migrate_on_start(&config)
        .await
        .expect("serve --migrate on a current database");

    // Another process (`hostingos migrate`) holds the lock: the startup
    // migration waits for it.
    let mut other = migrations.acquire().await.expect("acquire");
    sqlx::query("select pg_advisory_lock($1)")
        .bind(hos_db::MIGRATION_LOCK_KEY)
        .execute(&mut *other)
        .await
        .expect("lock");
    let waiting = tokio::spawn({
        let pool = migrations.clone();
        async move { hos_db::migrate_if_behind(&pool).await }
    });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !waiting.is_finished(),
        "ran while another run held the lock"
    );
    sqlx::query("select pg_advisory_unlock($1)")
        .bind(hos_db::MIGRATION_LOCK_KEY)
        .execute(&mut *other)
        .await
        .expect("unlock");
    drop(other);
    let outcome = tokio::time::timeout(Duration::from_secs(20), waiting)
        .await
        .expect("ran once the lock was free")
        .expect("task")
        .expect("startup migration");
    assert!(outcome.applied.is_empty());
    migrations.close().await;
    app.cleanup().await;
}

/// An older release starting with `--migrate` on a database a newer release
/// migrated: nothing to apply, so it starts (plain sqlx would refuse the
/// unknown version).
#[tokio::test]
async fn startup_migrations_leave_a_newer_schema_alone() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let mut versions = hos_db::embedded_versions();
    versions.push(i64::MAX);
    let Some(pool) = pool_with_history(&app, &versions).await else {
        app.cleanup().await;
        return;
    };
    let outcome = hos_db::migrate_if_behind(&pool)
        .await
        .expect("an ahead-only database starts");
    assert!(outcome.applied.is_empty());
    assert_eq!(outcome.unknown, [i64::MAX]);
    let status = hos_db::schema_status(&pool).await.expect("status");
    assert!(!status.is_behind() && status.is_ahead());
    forget_history(pool).await;
    app.cleanup().await;
}
