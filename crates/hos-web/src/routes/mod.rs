//! The router.
//!
//! Each area lives in its own module with a `router(state) -> Router<AppState>`
//! function; [`app`] merges them and wraps the result in the middleware stack.
//!
//! Middleware order (outermost first):
//!
//! ```text
//! CatchPanic → SetRequestId → Trace → PropagateRequestId → security headers
//!   → request limits (body read in full: size + timeout; request timeout)
//!   → demo layer (demo mode only) → compression → body limit
//!   → schema check (503 while migrations are pending) → maintenance (503 during updates)
//!   ├─ /static/*, /assets/app.css, /healthz, /readyz      (no tenant, no session)
//!   └─ everything else:
//!        resolve_tenant → locale_prefix (strips /en) → load_session
//!        → server area (single mode) → error_pages → flash → demo guard → csrf_protect → cache_control
//!        → CatchPanic → routes
//! ```
//!
//! The tenant stack wraps the whole area router *as a service* so that
//! `locale_prefix` can rewrite the URI before routing happens.

pub mod admin;
pub mod api;
pub mod auth;
pub mod cart;
pub mod client;
pub mod guards;
pub mod health;
pub mod platform;
pub mod storefront;
pub mod webhooks;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::{from_fn, from_fn_with_state};
use http::Request;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use crate::state::AppState;
use crate::{csrf, flash, locale, middleware, session, tenant};

/// The area routers merged (without middleware) — handy for tests that
/// build their own stack.
pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(storefront::router(state))
        .merge(auth::router(state))
        .merge(client::router(state))
        .merge(cart::router(state))
        .merge(admin::router(state))
        .merge(platform::router(state))
        .merge(api::router(state))
        .merge(webhooks::router(state))
        .merge(crate::demo::router())
        .fallback(middleware::not_found)
}

/// The complete application.
pub fn app(state: AppState) -> Router {
    let tenant_routes = routes(&state).with_state(state.clone());
    let tenant_stack = ServiceBuilder::new()
        .layer(from_fn_with_state(state.clone(), tenant::resolve_tenant))
        .layer(from_fn_with_state(state.clone(), locale::locale_prefix))
        .layer(from_fn_with_state(state.clone(), session::load_session))
        // Single-company mode: the owner's server pages (/admin/server).
        .layer(from_fn_with_state(state.clone(), crate::mode::layer))
        .layer(from_fn_with_state(state.clone(), middleware::error_pages))
        // Outside the demo guard, so its notices reach the visitor.
        .layer(from_fn_with_state(state.clone(), flash::flash_layer))
        // Public demo only (docs/DEMO.md): refuses blocked requests, handles
        // visitors caught by a reset.
        .layer(from_fn_with_state(state.clone(), crate::demo::guard))
        .layer(from_fn_with_state(state.clone(), csrf::csrf_protect))
        .layer(from_fn(middleware::cache_control))
        .layer(CatchPanicLayer::custom(middleware::panic_response))
        .service(tenant_routes);

    let max_body = state.web.max_body_bytes;
    Router::new()
        .merge(crate::assets::router(&state.assets))
        .merge(health::router().with_state(state.clone()))
        // Tenant-less public endpoints: gateway/registrar webhooks (tenant from
        // the integration row), the TLS allow-list for the reverse proxy and
        // the API description (`/api/v1/openapi.json`, `/api/docs`).
        .merge(webhooks::public_router(&state).with_state(state.clone()))
        .merge(platform::public_router(&state).with_state(state.clone()))
        .merge(api::public_router(&state).with_state(state.clone()))
        .fallback_service(tenant_stack)
        // 503 while an update runs (docs/UPDATES.md); the platform console,
        // health checks and assets stay reachable.
        .layer(from_fn_with_state(
            state.clone(),
            platform::updates::maintenance::layer,
        ))
        // 503 instead of "relation does not exist" while the database is
        // behind this binary (`crate::schema`); health checks stay reachable.
        .layer(from_fn_with_state(state.clone(), crate::schema::layer))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(CompressionLayer::new())
        // Public demo only: reset gate, per-IP limits, `X-Robots-Tag`.
        .layer(from_fn_with_state(state.clone(), crate::demo::layer))
        // Outside the demo gate: the body is in memory before a request
        // can hold up a demo reset (`crate::server`).
        .layer(from_fn_with_state(
            crate::server::RequestLimits::from(&state.web),
            crate::server::limits,
        ))
        .layer(from_fn_with_state(
            state.clone(),
            middleware::security_headers,
        ))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &Request<axum::body::Body>| {
                let request_id = req
                    .headers()
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                tracing::info_span!(
                    "http",
                    method = %req.method(),
                    uri = %req.uri().path(),
                    request_id = %request_id,
                )
            }),
        )
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(CatchPanicLayer::custom(middleware::panic_response))
}
