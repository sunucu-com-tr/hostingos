//! `/healthz` (process up) and `/readyz` (database reachable and migrated).

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use http::StatusCode;
use serde_json::json;

use crate::state::AppState;

/// Health routes (outside the tenant stack).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}

async fn healthz(State(state): State<AppState>) -> Response {
    (
        [(http::header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "status": "ok",
            // No version here: it is shown to platform admins only
            // (/platform/system, /platform/updates), not to anonymous probes.
            "uptime_secs": state.started_at.elapsed().as_secs(),
        })),
    )
        .into_response()
}

/// Ready = the database answers and has every migration this release needs
/// (`crate::schema`; a schema ahead of the release is ready). Each probe
/// re-checks, so the server turns ready once migrations are applied.
async fn readyz(State(state): State<AppState>) -> Response {
    let db = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        state.schema.check(&state.pool),
    )
    .await;
    match db {
        Ok(Ok(schema)) if schema.is_behind() => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(http::header::CACHE_CONTROL, "no-store")],
            Json(json!({"status": "unavailable", "database": "ok", "schema": "behind"})),
        )
            .into_response(),
        Ok(Ok(schema)) => (
            [(http::header::CACHE_CONTROL, "no-store")],
            Json(json!({
                "status": "ready",
                "database": "ok",
                "schema": if schema.is_ahead() { "ahead" } else { "current" },
            })),
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "readiness: database error");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "unavailable", "database": "error"})),
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable", "database": "timeout"})),
        )
            .into_response(),
    }
}
