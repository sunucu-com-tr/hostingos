//! Maintenance mode (cluster-wide flag `update_state.maintenance`, set
//! while an update is applied):
//!
//! * [`layer`] answers every request except the platform console, health
//!   checks and static assets with `503` + `Retry-After` (a small bilingual
//!   HTML page, or `application/problem+json` for `/api`, `/webhooks` and
//!   JSON clients — payment providers retry webhooks later);
//! * [`run_worker`] runs the job worker only while maintenance is off: when
//!   it turns on, the worker stops claiming jobs and hands unfinished ones
//!   back, on every node, so the update can wait for running jobs to drain.

use std::time::Duration;

use askama::Template;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use hos_jobs::Worker;
use http::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER};
use http::{HeaderValue, StatusCode};

use crate::state::AppState;
use crate::worker::{ShutdownSignal, worker_config};

/// Paths that stay available during maintenance.
pub fn exempt(path: &str) -> bool {
    path == "/platform"
        || path.starts_with("/platform/")
        || path.starts_with("/static/")
        || path.starts_with("/assets/")
        || path == "/healthz"
        || path == "/readyz"
        || path.starts_with("/internal/")
        || path == "/favicon.ico"
}

#[derive(Template)]
#[template(path = "platform/maintenance.html")]
struct MaintenancePage {
    css: String,
}

/// Single-company mode: the owner's way to the update pages
/// (`/admin/server`, also behind `/en` or `/tr`) and the staff login stay
/// available, the counterpart of `/platform` and `/platform/login` in
/// multi-company mode.
pub fn exempt_single(path: &str) -> bool {
    let p = ["/en", "/tr"]
        .iter()
        .find_map(|l| path.strip_prefix(l).filter(|r| r.starts_with('/')))
        .unwrap_or(path);
    let under = |root: &str| p == root || p.strip_prefix(root).is_some_and(|r| r.starts_with('/'));
    under(crate::mode::SERVER_PREFIX)
        || under("/auth/login")
        || p == "/auth/logout"
        || p == "/admin/update-indicator"
}

/// Middleware: `503` for everything but [`exempt`] paths (and
/// [`exempt_single`] ones in single-company mode) while maintenance mode is
/// on.
pub async fn layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if exempt(path)
        || (state.web.single() && exempt_single(path))
        || !state.updates.maintenance_on(&state.pool).await
    {
        return next.run(req).await;
    }
    unavailable(&state, &req, 120)
}

/// The maintenance answer: `503` with `Retry-After: retry_after_secs`, the
/// bilingual page (or problem+json for the API, webhooks and JSON clients).
/// Also used while the database waits for migrations (`crate::schema`).
pub fn unavailable(state: &AppState, req: &Request, retry_after_secs: u32) -> Response {
    let path = req.uri().path();
    let json = path == "/api"
        || path.starts_with("/api/")
        || path.starts_with("/webhooks/")
        || req
            .headers()
            .get(http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("application/json") && !a.contains("text/html"));
    let mut res = if json {
        let body = serde_json::json!({
            "type": "about:blank",
            "title": "Service Unavailable",
            "status": 503,
            "code": "maintenance",
            "detail": "HostingOS is being updated. Please retry in a few minutes.",
        });
        let mut r = (StatusCode::SERVICE_UNAVAILABLE, body.to_string()).into_response();
        r.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        r
    } else {
        let page = MaintenancePage {
            css: state.assets.css_url(),
        };
        match page.render() {
            Ok(html) => (StatusCode::SERVICE_UNAVAILABLE, Html(html)).into_response(),
            Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "Maintenance").into_response(),
        }
    };
    let h = res.headers_mut();
    h.insert(RETRY_AFTER, HeaderValue::from(retry_after_secs));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// The job worker, paused while maintenance mode is on and while the
/// database waits for migrations (`crate::schema`).
pub async fn run_worker(state: AppState, signal: ShutdownSignal) {
    loop {
        while state.updates.maintenance_on(&state.pool).await
            || state.schema.blocks_pages(&state.pool).await
        {
            tokio::select! {
                () = signal.clone().wait() => return,
                () = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
        }
        if signal.is_triggered() {
            return;
        }
        let worker = Worker::new(
            state.pool.clone(),
            state.billing.clone(),
            worker_config(&state),
        );
        let paused = {
            let state = state.clone();
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if state.updates.maintenance_on(&state.pool).await {
                        return;
                    }
                }
            }
        };
        let stop_signal = signal.clone();
        let stop = async move {
            tokio::select! {
                () = stop_signal.wait() => {}
                () = paused => tracing::warn!("maintenance mode: job worker paused"),
            }
        };
        if let Err(e) = worker.run(stop).await {
            tracing::error!(error = %e, "job worker stopped with an error");
            tokio::select! {
                () = signal.clone().wait() => return,
                () = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
        if signal.is_triggered() {
            return;
        }
    }
}
