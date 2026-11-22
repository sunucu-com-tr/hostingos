//! REST API (`/api/v1/*`).
//!
//! * **Auth** — `Authorization: Bearer hos_live_…` API keys (tenant scoped,
//!   optional scopes, expiry / revocation / IP allow-list checked) or the
//!   browser session cookie (then CSRF applies to unsafe methods). Staff keys
//!   act with the user's permissions ∩ key scopes; client keys only ever see
//!   their own client's records (others' ids answer `404`).
//! * **Client sub-users** — a client principal (key or session) acts with
//!   its membership of the client account, read at request time
//!   (`client_users.permissions`, [`MemberAccess`]): every route maps to the
//!   member areas it needs ([`member_areas`]) and the gate answers `403` for
//!   an area the member is not granted, so a key never exceeds what its user
//!   may do in the client area *now*. [`ApiAuth::scope`] checks it again.
//! * **2FA policy** — a staff session that still has to set up 2FA
//!   (`security.staff_2fa_required`) is refused (`403`).
//! * **Errors** — `application/problem+json` ([`ApiError`]), validation
//!   failures are `422` with field-level `errors`.
//! * **Rate limit** — `state.limits.api` per key / user / IP, reported in
//!   `RateLimit-Limit` / `RateLimit-Remaining` / `RateLimit-Reset`.
//! * **Idempotency** — every `POST` honours an `Idempotency-Key` header: the
//!   first response (status < 500) is stored for 24 h and replayed for the
//!   same key + principal; the same key with a different body is `422`.
//! * **Lists** — `limit` / `offset` pagination with `total`, `sort`
//!   (`-created_at`, whitelisted per resource), `status`, `q`, `client_id`
//!   (staff), `created_from` / `created_to`. Money is a decimal string next
//!   to a `currency` code.
//!
//! Routes are declared once in [`build`]; [`route_table`] exposes them so
//! the OpenAPI document ([`openapi`]) is tested against the real router.

mod affiliates;
mod billing;
mod catalog;
mod clients;
mod documents;
mod domains;
mod dto;
mod meta;
pub mod openapi;
mod orders;
mod quotes;
mod services;
mod status;
mod tasks;
mod tickets;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use chrono::{DateTime, Utc};
use hos_auth::ratelimit::RateDecision;
use hos_auth::rbac::{Permission, PermissionSet, Principal};
use hos_core::CoreError;
use hos_core::domain::UserKind;
use hos_core::ids::{ApiKeyId, ClientId, TenantId, UserId};
use hos_db::Page;
use hos_db::Paged;
use hos_db::repo::api::{ApiFilter, ApiSort};
use hos_db::repo::idempotency::{self, IdempotencyBegin};
use hos_db::repo::{clients as clients_repo, identity};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::extract::ClientIp;
use crate::routes::client::MemberAccess;
use crate::session::SessionInfo;
use crate::state::AppState;
use crate::tenant::Tenant;

/// Largest JSON body accepted by the API.
const MAX_BODY: usize = 1024 * 1024;
/// `Idempotency-Key` header.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

/// Routes + the `(METHOD, path)` list they were built from.
#[derive(Default)]
struct Table {
    router: Router<AppState>,
    routes: Vec<(&'static str, &'static str)>,
}

impl Table {
    fn add(mut self, method: &'static str, path: &'static str, mr: MethodRouter<AppState>) -> Self {
        self.router = self.router.route(path, mr);
        self.routes.push((method, path));
        self
    }
}

fn build() -> Table {
    use axum::routing::{delete, patch, post, put};
    Table::default()
        // meta
        .add("GET", "/api/v1/ping", get(meta::ping))
        .add("GET", "/api/v1/me", get(meta::me))
        .add("GET", "/api/v1/stats/dashboard", get(meta::dashboard_stats))
        .add("GET", "/api/v1/integrations", get(meta::integrations))
        .add("GET", "/api/v1/webhook-endpoints", get(meta::webhook_endpoints))
        .add("POST", "/api/v1/webhook-endpoints", post(meta::create_webhook_endpoint))
        .add(
            "DELETE",
            "/api/v1/webhook-endpoints/{id}",
            delete(meta::delete_webhook_endpoint),
        )
        // catalog
        .add("GET", "/api/v1/catalog", get(catalog::public_catalog))
        .add("GET", "/api/v1/tlds", get(catalog::tlds))
        .add("GET", "/api/v1/products", get(catalog::list_products))
        .add("GET", "/api/v1/products/{id}", get(catalog::get_product))
        // clients
        .add("GET", "/api/v1/clients", get(clients::list))
        .add("POST", "/api/v1/clients", post(clients::create))
        .add("GET", "/api/v1/clients/{id}", get(clients::show))
        .add("PATCH", "/api/v1/clients/{id}", patch(clients::update))
        // orders
        .add("GET", "/api/v1/orders", get(orders::list))
        .add("POST", "/api/v1/orders", post(orders::create))
        .add("POST", "/api/v1/orders/quote", post(orders::quote))
        .add("GET", "/api/v1/orders/{id}", get(orders::show))
        // sales quotes
        .add("GET", "/api/v1/quotes", get(quotes::list))
        .add("GET", "/api/v1/quotes/{id}", get(quotes::show))
        .add("POST", "/api/v1/quotes/{id}/accept", post(quotes::accept))
        .add("POST", "/api/v1/quotes/{id}/decline", post(quotes::decline))
        // invoices / transactions
        .add("GET", "/api/v1/invoices", get(billing::list_invoices))
        .add("GET", "/api/v1/invoices/{id}", get(billing::show_invoice))
        .add("GET", "/api/v1/invoices/{id}/pdf", get(documents::invoice_pdf))
        .add("POST", "/api/v1/invoices/{id}/pay", post(billing::pay))
        .add("POST", "/api/v1/invoices/{id}/payments", post(billing::manual_payment))
        .add("GET", "/api/v1/transactions", get(billing::list_transactions))
        // services
        .add("GET", "/api/v1/services", get(services::list))
        .add("GET", "/api/v1/services/{id}", get(services::show))
        .add("POST", "/api/v1/services/{id}/suspend", post(services::suspend))
        .add("POST", "/api/v1/services/{id}/unsuspend", post(services::unsuspend))
        .add("POST", "/api/v1/services/{id}/terminate", post(services::terminate))
        .add("POST", "/api/v1/services/{id}/power", post(services::power))
        .add("POST", "/api/v1/services/{id}/reinstall", post(services::reinstall))
        .add("GET", "/api/v1/services/{id}/snapshots", get(services::snapshots))
        .add("POST", "/api/v1/services/{id}/snapshots", post(services::create_snapshot))
        .add(
            "POST",
            "/api/v1/services/{id}/snapshots/{snapshot}/rollback",
            post(services::rollback_snapshot),
        )
        .add(
            "DELETE",
            "/api/v1/services/{id}/snapshots/{snapshot}",
            delete(services::delete_snapshot),
        )
        .add("POST", "/api/v1/services/{id}/console", post(services::console))
        .add("POST", "/api/v1/services/{id}/password-reset", post(services::password_reset))
        .add("GET", "/api/v1/services/{id}/backups", get(services::backups))
        .add(
            "POST",
            "/api/v1/services/{id}/backups/{backup}/restore",
            post(services::restore_backup),
        )
        .add("GET", "/api/v1/services/{id}/reverse-dns", get(services::reverse_dns))
        .add("PUT", "/api/v1/services/{id}/reverse-dns", put(services::set_reverse_dns))
        .add("GET", "/api/v1/services/{id}/isos", get(services::isos))
        .add("POST", "/api/v1/services/{id}/iso", post(services::mount_iso))
        .add("DELETE", "/api/v1/services/{id}/iso", delete(services::unmount_iso))
        .add("POST", "/api/v1/services/{id}/rescue", post(services::rescue))
        .add("GET", "/api/v1/services/{id}/firewall", get(services::firewall))
        .add("PUT", "/api/v1/services/{id}/firewall", put(services::set_firewall))
        .add("GET", "/api/v1/services/{id}/metrics", get(services::metrics))
        .add("GET", "/api/v1/services/{id}/bandwidth", get(services::bandwidth))
        .add("GET", "/api/v1/services/{id}/operations", get(services::operations))
        .add("GET", "/api/v1/ssh-keys", get(services::ssh_keys))
        .add("POST", "/api/v1/ssh-keys", post(services::add_ssh_key))
        .add("DELETE", "/api/v1/ssh-keys/{id}", delete(services::delete_ssh_key))
        // domains
        .add("GET", "/api/v1/domains", get(domains::list))
        .add("GET", "/api/v1/domains/availability", get(domains::availability))
        .add("POST", "/api/v1/domains/register", post(domains::register))
        .add("GET", "/api/v1/domains/{id}", get(domains::show))
        .add("POST", "/api/v1/domains/{id}/renew", post(domains::renew))
        .add("PUT", "/api/v1/domains/{id}/nameservers", put(domains::nameservers))
        .add("PUT", "/api/v1/domains/{id}/lock", put(domains::lock))
        // support
        .add("GET", "/api/v1/departments", get(tickets::departments))
        .add("GET", "/api/v1/tickets", get(tickets::list))
        .add("POST", "/api/v1/tickets", post(tickets::create))
        .add("GET", "/api/v1/tickets/{id}", get(tickets::show))
        .add("GET", "/api/v1/tickets/{id}/messages", get(tickets::messages))
        .add("POST", "/api/v1/tickets/{id}/messages", post(tickets::reply))
        // network status (public)
        .add("GET", "/api/v1/status", get(status::summary))
        .add("GET", "/api/v1/status/incidents", get(status::incidents))
        .add("GET", "/api/v1/status/incidents/{id}", get(status::incident))
        // the caller's affiliate account (client owner)
        .add("GET", "/api/v1/affiliate", get(affiliates::show))
        .add("GET", "/api/v1/affiliate/commissions", get(affiliates::commissions))
        .add("GET", "/api/v1/affiliate/referrals", get(affiliates::referrals))
        .add("GET", "/api/v1/affiliate/payouts", get(affiliates::payouts))
        // staff to-do list
        .add("GET", "/api/v1/tasks", get(tasks::list))
        .add("POST", "/api/v1/tasks", post(tasks::create))
        .add("GET", "/api/v1/tasks/{id}", get(tasks::show))
        .add("PATCH", "/api/v1/tasks/{id}", patch(tasks::update))
        .add("DELETE", "/api/v1/tasks/{id}", delete(tasks::delete))
}

/// API routes (inside the tenant stack).
pub fn router(state: &AppState) -> Router<AppState> {
    build()
        .router
        .route_layer(from_fn_with_state(state.clone(), gate))
}

/// Every `(METHOD, path)` the API router serves (tenant routes only).
pub fn route_table() -> Vec<(&'static str, &'static str)> {
    build().routes
}

/// Tenant-independent API routes: the OpenAPI document and its HTML page.
pub fn public_router(_state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/api/v1/openapi.json", get(openapi::document_handler))
        .route("/api/docs", get(openapi::docs_page))
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// One invalid field of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// JSON pointer-ish field name (`items[0].product_id`, `limit` …).
    pub field: String,
    /// Human readable reason.
    pub message: String,
}

/// An RFC 9457 problem response.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Stable machine code (`not_found`, `validation_failed` …).
    pub code: &'static str,
    /// Safe-to-show detail.
    pub detail: String,
    /// Field-level validation errors.
    pub errors: Vec<FieldError>,
    /// `Retry-After` seconds.
    pub retry_after: Option<u64>,
}

/// Result of API handlers.
pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// A problem with a status, code and detail.
    pub fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
            errors: Vec::new(),
            retry_after: None,
        }
    }

    /// `404`.
    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "The resource does not exist.",
        )
    }

    /// `401`.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication required: send `Authorization: Bearer <api key>`.",
        )
    }

    /// `403`.
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", detail)
    }

    /// `422` with field errors.
    pub fn validation(errors: Vec<FieldError>) -> Self {
        Self {
            errors,
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                "The request has invalid fields.",
            )
        }
    }

    /// `422` for a single field.
    pub fn field(field: &str, message: impl Into<String>) -> Self {
        Self::validation(vec![FieldError {
            field: field.to_owned(),
            message: message.into(),
        }])
    }

    fn body(&self) -> Value {
        let mut body = json!({
            "type": format!("urn:hostingos:problem:{}", self.code),
            "title": self.status.canonical_reason().unwrap_or("Error"),
            "status": self.status.as_u16(),
            "code": self.code,
            "detail": self.detail,
        });
        if !self.errors.is_empty() {
            body["errors"] = self
                .errors
                .iter()
                .map(|e| json!({ "field": e.field, "message": e.message }))
                .collect();
        }
        body
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut res = (self.status, self.body().to_string()).into_response();
        res.headers_mut().insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(secs) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            res.headers_mut().insert(http::header::RETRY_AFTER, v);
        }
        res
    }
}

impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        match err {
            CoreError::Validation(msg) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed", msg)
            }
            CoreError::NotFound { .. } => Self::not_found(),
            CoreError::Unauthorized => Self::unauthorized(),
            CoreError::Forbidden(msg) => Self::forbidden(msg),
            CoreError::Conflict(msg) => Self::new(StatusCode::CONFLICT, "conflict", msg),
            CoreError::InvalidTransition { .. } => {
                Self::new(StatusCode::CONFLICT, "invalid_state", err.to_string())
            }
            other => {
                let status = other.http_status();
                if status < 500 {
                    Self::new(
                        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                        "bad_request",
                        other.to_string(),
                    )
                } else if (502..=504).contains(&status) {
                    tracing::warn!(error = %other, "api: upstream integration failed");
                    Self {
                        retry_after: Some(30),
                        ..Self::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "upstream_unavailable",
                            "An upstream integration failed; retry later.",
                        )
                    }
                } else {
                    tracing::error!(error = %other, "api: internal error");
                    Self::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal",
                        "Internal error.",
                    )
                }
            }
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        CoreError::from(err).into()
    }
}

impl From<AppError> for ApiError {
    fn from(err: AppError) -> Self {
        match err {
            AppError::NotFound => Self::not_found(),
            AppError::Unauthorized => Self::unauthorized(),
            AppError::Forbidden(msg) => Self::forbidden(msg),
            AppError::Csrf => Self::forbidden("Missing or invalid CSRF token."),
            AppError::RateLimited { retry_after } => Self {
                retry_after: Some(retry_after.map_or(60, |d| d.as_secs().max(1))),
                ..Self::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                    "Too many requests.",
                )
            },
            AppError::BadRequest(msg) => Self::new(StatusCode::BAD_REQUEST, "bad_request", msg),
            AppError::Conflict(msg) => Self::new(StatusCode::CONFLICT, "conflict", msg),
            AppError::PayloadTooLarge => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "The request body is too large.",
            ),
            AppError::DemoDisabled => Self::new(
                StatusCode::FORBIDDEN,
                "demo_disabled",
                "This operation is disabled in the public demo.",
            ),
            AppError::Unavailable(msg) => {
                tracing::warn!(error = %msg, "api: unavailable");
                Self {
                    retry_after: Some(30),
                    ..Self::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "unavailable",
                        "Service unavailable; retry later.",
                    )
                }
            }
            AppError::Internal(msg) => {
                tracing::error!(error = %msg, "api: internal error");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "Internal error.",
                )
            }
        }
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(err: tokio::task::JoinError) -> Self {
        AppError::from(err).into()
    }
}

// ---------------------------------------------------------------------------
// replies
// ---------------------------------------------------------------------------

/// A JSON reply with a status.
#[derive(Debug, Clone)]
pub struct ApiReply {
    status: StatusCode,
    body: Value,
}

impl ApiReply {
    /// `200`.
    pub fn ok(body: Value) -> Self {
        Self {
            status: StatusCode::OK,
            body,
        }
    }

    /// `201`.
    pub fn created(body: Value) -> Self {
        Self {
            status: StatusCode::CREATED,
            body,
        }
    }

    /// `202` (work queued).
    pub fn accepted(body: Value) -> Self {
        Self {
            status: StatusCode::ACCEPTED,
            body,
        }
    }
}

impl IntoResponse for ApiReply {
    fn into_response(self) -> Response {
        (self.status, axum::Json(self.body)).into_response()
    }
}

/// `{ "data": [...], "pagination": {...} }`.
pub fn paged<T>(page: &Paged<T>, f: impl Fn(&T) -> Value) -> ApiReply {
    let next = page.offset + page.limit;
    let has_more = next < page.total;
    ApiReply::ok(json!({
        "data": page.items.iter().map(f).collect::<Vec<_>>(),
        "pagination": {
            "total": page.total,
            "limit": page.limit,
            "offset": page.offset,
            "has_more": has_more,
            "next_offset": if has_more { Some(next) } else { None },
        }
    }))
}

/// `{ "data": [...] }` for short, unpaginated collections.
pub fn collection(items: Vec<Value>) -> ApiReply {
    ApiReply::ok(json!({ "total": items.len(), "data": items }))
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

/// Who is calling the API (inserted by [`gate`]).
#[derive(Debug, Clone)]
pub struct ApiAuth {
    /// Effective principal (API key permissions are limited to its scopes).
    pub principal: Principal,
    /// The API key used (`None` for session cookies / anonymous calls).
    pub key_id: Option<ApiKeyId>,
    /// A client principal's membership of its client account (loaded by the
    /// gate per request; `None` for staff and anonymous callers).
    pub member: Option<MemberAccess>,
}

/// What a request may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Staff user with the required permission.
    Staff(UserId),
    /// A client principal: only its own records.
    Client {
        /// Acting user.
        user_id: UserId,
        /// Its client.
        client_id: ClientId,
    },
}

impl Scope {
    /// Client restriction (`None` for staff).
    pub fn client(&self) -> Option<ClientId> {
        match self {
            Self::Staff(_) => None,
            Self::Client { client_id, .. } => Some(*client_id),
        }
    }

    /// Acting user.
    pub fn user(&self) -> UserId {
        match self {
            Self::Staff(u) | Self::Client { user_id: u, .. } => *u,
        }
    }

    /// `404` unless the record belongs to the client (staff see everything).
    pub fn owns(&self, owner: Option<ClientId>) -> ApiResult<()> {
        match self.client() {
            None => Ok(()),
            Some(c) if owner == Some(c) => Ok(()),
            Some(_) => Err(ApiError::not_found()),
        }
    }
}

impl ApiAuth {
    /// Staff with `p`, or a client (restricted to its own data) whose member
    /// account grants an area matching `p` ([`permission_areas`]).
    pub fn scope(&self, p: Permission) -> ApiResult<Scope> {
        match &self.principal {
            Principal::Client {
                user_id, client_id, ..
            } => {
                let member = self
                    .member
                    .as_ref()
                    .ok_or_else(|| ApiError::forbidden("Client membership not loaded."))?;
                let areas = permission_areas(p);
                if !areas.iter().any(|a| member.allows(a)) {
                    return Err(area_denied(areas.first().copied().unwrap_or("owner")));
                }
                Ok(Scope::Client {
                    user_id: *user_id,
                    client_id: *client_id,
                })
            }
            Principal::Staff { user_id, .. } => {
                self.principal.require(p).map_err(|_| missing(p))?;
                Ok(Scope::Staff(*user_id))
            }
            _ => Err(ApiError::unauthorized()),
        }
    }

    /// Staff with `p` only (clients get `403`).
    pub fn staff(&self, p: Permission) -> ApiResult<UserId> {
        match &self.principal {
            Principal::Staff { user_id, .. } => {
                self.principal.require(p).map_err(|_| missing(p))?;
                Ok(*user_id)
            }
            Principal::Client { .. } => Err(ApiError::forbidden(
                "This endpoint is only available to staff API keys.",
            )),
            _ => Err(ApiError::unauthorized()),
        }
    }

    /// `Idempotency-Key` namespace of this caller.
    fn principal_key(&self) -> Option<String> {
        if let Some(k) = self.key_id {
            return Some(format!("key:{k}"));
        }
        self.principal.user_id().map(|u| format!("user:{u}"))
    }
}

fn missing(p: Permission) -> ApiError {
    ApiError::forbidden(format!("Missing permission `{}`.", p.as_str()))
}

fn area_denied(area: &str) -> ApiError {
    ApiError::forbidden(format!(
        "This client user is not granted the `{area}` area of the client account."
    ))
}

/// Client-area member permissions ([`MemberAccess`], `client_users.permissions`)
/// an API route needs, keyed by the route pattern of [`build`]. Public and
/// staff-only routes need none (the handlers refuse client principals on
/// staff-only routes). `None`: unknown route — the gate then requires the
/// account owner (fail closed).
pub fn member_areas(path: &str) -> Option<&'static [&'static str]> {
    const NONE: &[&str] = &[];
    let areas: &'static [&'static str] = match path {
        "/api/v1/ping"
        | "/api/v1/me"
        | "/api/v1/catalog"
        | "/api/v1/tlds"
        | "/api/v1/domains/availability"
        | "/api/v1/status"
        | "/api/v1/status/incidents"
        | "/api/v1/status/incidents/{id}" => NONE,
        // staff only
        "/api/v1/tasks" | "/api/v1/tasks/{id}" => NONE,
        "/api/v1/stats/dashboard"
        | "/api/v1/integrations"
        | "/api/v1/webhook-endpoints"
        | "/api/v1/webhook-endpoints/{id}"
        | "/api/v1/products"
        | "/api/v1/products/{id}" => NONE,
        "/api/v1/clients" | "/api/v1/clients/{id}" => &["profile"],
        "/api/v1/orders" | "/api/v1/orders/quote" | "/api/v1/orders/{id}" => &["orders"],
        "/api/v1/quotes"
        | "/api/v1/quotes/{id}"
        | "/api/v1/quotes/{id}/accept"
        | "/api/v1/quotes/{id}/decline" => &["orders"],
        "/api/v1/invoices"
        | "/api/v1/invoices/{id}"
        | "/api/v1/invoices/{id}/pdf"
        | "/api/v1/invoices/{id}/pay"
        | "/api/v1/invoices/{id}/payments"
        | "/api/v1/transactions" => &["invoices"],
        // Registering is a purchase of a domain.
        "/api/v1/domains/register" => &["domains", "orders"],
        "/api/v1/domains"
        | "/api/v1/domains/{id}"
        | "/api/v1/domains/{id}/renew"
        | "/api/v1/domains/{id}/nameservers"
        | "/api/v1/domains/{id}/lock" => &["domains"],
        "/api/v1/departments"
        | "/api/v1/tickets"
        | "/api/v1/tickets/{id}"
        | "/api/v1/tickets/{id}/messages" => &["tickets"],
        p if p == "/api/v1/services" || p.starts_with("/api/v1/services/") => &["services"],
        // Keys give root on the account's servers.
        "/api/v1/ssh-keys" | "/api/v1/ssh-keys/{id}" => &["services"],
        // Like `/client/affiliate`: the account owner only.
        "/api/v1/affiliate"
        | "/api/v1/affiliate/commissions"
        | "/api/v1/affiliate/referrals"
        | "/api/v1/affiliate/payouts" => &["owner"],
        _ => return None,
    };
    Some(areas)
}

/// Member areas that satisfy a permission a handler asks a client principal
/// for in [`ApiAuth::scope`] (any of them). Coarser than [`member_areas`]
/// (`billing.*` covers invoices and orders); unknown areas need the owner.
fn permission_areas(p: Permission) -> &'static [&'static str] {
    match p.area() {
        "services" => &["services"],
        "domains" => &["domains"],
        "support" => &["tickets"],
        "clients" => &["profile"],
        "billing" => &["invoices", "orders"],
        "quotes" => &["orders"],
        _ => &["owner"],
    }
}

/// Loads a client principal's membership and checks the areas the matched
/// route needs. Staff and anonymous callers pass unchanged.
async fn authorize_member(
    state: &AppState,
    tenant_id: TenantId,
    auth: &mut ApiAuth,
    route: Option<&str>,
) -> ApiResult<()> {
    let Principal::Client {
        user_id, client_id, ..
    } = &auth.principal
    else {
        return Ok(());
    };
    let access = crate::routes::client::load_member_access(state, tenant_id, *client_id, *user_id)
        .await?
        .ok_or_else(|| {
            ApiError::forbidden("The user is no longer a member of this client account.")
        })?;
    let areas = route.and_then(member_areas).unwrap_or(&["owner"]);
    if let Some(denied) = areas.iter().find(|a| !access.allows(a)) {
        return Err(area_denied(denied));
    }
    auth.member = Some(access);
    Ok(())
}

impl FromRequestParts<AppState> for ApiAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &AppState) -> ApiResult<Self> {
        match parts.extensions.get::<ApiAuth>() {
            Some(a) if a.principal.is_authenticated() => Ok(a.clone()),
            _ => Err(ApiError::unauthorized()),
        }
    }
}

/// The current tenant, as an API extractor (`404` problem when missing).
pub struct ApiTenant(pub Tenant);

impl FromRequestParts<AppState> for ApiTenant {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &AppState) -> ApiResult<Self> {
        parts
            .extensions
            .get::<Tenant>()
            .cloned()
            .map(ApiTenant)
            .ok_or_else(ApiError::not_found)
    }
}

impl std::ops::Deref for ApiTenant {
    type Target = Tenant;

    fn deref(&self) -> &Tenant {
        &self.0
    }
}

/// Grants of a scoped staff key: `scopes ∩ grants` (see [`from_api_key`]).
fn scoped_grants(grants: &[String], scopes: &[String]) -> Vec<String> {
    let user = PermissionSet::from_grants(grants.iter().cloned());
    let scope = PermissionSet::from_grants(scopes.iter().cloned());
    let star = |s: &PermissionSet| s.grants().any(|g| g == "*");
    if star(&user) && star(&scope) {
        return vec!["*".to_owned()];
    }
    hos_auth::rbac::Permission::ALL
        .iter()
        .filter(|p| user.allows(**p) && scope.allows(**p))
        .map(|p| p.as_str().to_owned())
        .collect()
}

async fn from_api_key(
    state: &AppState,
    tenant_id: TenantId,
    token: &str,
    ip: ClientIp,
) -> ApiResult<ApiAuth> {
    let parsed = hos_auth::tokens::parse_api_key(token).map_err(|_| ApiError::unauthorized())?;
    let hash = hos_auth::tokens::hash_api_key(token);
    let key = identity::api_key_by_hash(&state.pool, tenant_id, &hash)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let now = Utc::now();
    if !hos_auth::tokens::verify_api_key(&parsed, &key.key_hash)
        || key.revoked_at.is_some()
        || key.expires_at.is_some_and(|e| e <= now)
        || (!key.allowed_ips.is_empty() && !key.allowed_ips.iter().any(|n| n.contains(&ip.0)))
    {
        return Err(ApiError::unauthorized());
    }
    let user_id = key.user_id.ok_or_else(ApiError::unauthorized)?;
    let user = identity::get_user(&state.pool, tenant_id, user_id)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    if user.status != hos_db::enums::AccountStatus::Active {
        return Err(ApiError::unauthorized());
    }
    let principal = match user.kind {
        UserKind::Staff => {
            let grants = identity::user_permissions(&state.pool, tenant_id, user.id).await?;
            // A scoped key gets the intersection of its scopes and the
            // user's current grants, evaluated per catalog permission so
            // wildcards (`*`, `area.*`) and "manage implies view" count on
            // both sides; `*` survives only when both sides hold it.
            let grants: Vec<String> = if key.scopes.is_empty() {
                grants
            } else {
                scoped_grants(&grants, &key.scopes)
            };
            Principal::Staff {
                user_id: user.id,
                tenant_id,
                permissions: PermissionSet::from_grants(grants),
            }
        }
        UserKind::Client => {
            let client = clients_repo::clients_of_user(&state.pool, tenant_id, user.id)
                .await?
                .into_iter()
                .next()
                .ok_or_else(ApiError::unauthorized)?;
            Principal::Client {
                user_id: user.id,
                client_id: client.id,
                tenant_id,
                impersonator: None,
            }
        }
    };
    identity::touch_api_key(&state.pool, key.id, Some(ip.0)).await?;
    Ok(ApiAuth {
        principal,
        key_id: Some(key.id),
        member: None,
    })
}

// ---------------------------------------------------------------------------
// gate: auth → rate limit → idempotency
// ---------------------------------------------------------------------------

/// Middleware on every API route.
async fn gate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let Some(tenant) = parts.extensions.get::<Tenant>().cloned() else {
        return ApiError::not_found().into_response();
    };
    let ip = ClientIp::for_request(&parts, &state);
    let bearer = parts
        .headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(hos_auth::tokens::bearer_token)
        .map(str::to_owned);

    let (auth, rate_key) = if let Some(token) = bearer {
        match from_api_key(&state, tenant.id, &token, ip).await {
            Ok(a) => {
                let key = format!(
                    "api-key:{}",
                    a.key_id.map(|k| k.to_string()).unwrap_or_default()
                );
                (a, key)
            }
            Err(e) => {
                // Failed keys are throttled per IP so guessing is slow.
                if let RateDecision::Limited { retry_after } =
                    state.limits.api.check(&ip.key("api-bad-key"))
                {
                    return ApiError::from(AppError::RateLimited {
                        retry_after: Some(retry_after),
                    })
                    .into_response();
                }
                return e.into_response();
            }
        }
    } else {
        let session = parts
            .extensions
            .get::<Arc<SessionInfo>>()
            .filter(|s| s.is_authenticated())
            .filter(|s| s.principal.tenant_id() == Some(tenant.id))
            .cloned();
        match session {
            // The tenant requires 2FA for staff and this user has not set it
            // up yet: `/admin` only offers the setup page, the API nothing.
            Some(s) if s.mfa_setup_required => {
                return ApiError::new(
                    StatusCode::FORBIDDEN,
                    "mfa_setup_required",
                    "Set up two-factor authentication before using the API.",
                )
                .into_response();
            }
            Some(s) => {
                let key = format!(
                    "api-user:{}",
                    s.principal
                        .user_id()
                        .map(|u| u.to_string())
                        .unwrap_or_default()
                );
                (
                    ApiAuth {
                        principal: s.principal.clone(),
                        key_id: None,
                        member: None,
                    },
                    key,
                )
            }
            None => (
                ApiAuth {
                    principal: Principal::Anonymous,
                    key_id: None,
                    member: None,
                },
                ip.key("api"),
            ),
        }
    };
    let mut auth = auth;

    let limit = state.limits.api.limit();
    let decision = state.limits.api.check(&rate_key);
    let headers = rate_headers(limit.burst, limit.period.as_secs(), &decision);
    if let RateDecision::Limited { retry_after } = decision {
        let mut res = ApiError::from(AppError::RateLimited {
            retry_after: Some(retry_after),
        })
        .into_response();
        res.headers_mut().extend(headers);
        return res;
    }

    let route = parts
        .extensions
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned());
    if let Err(e) = authorize_member(&state, tenant.id, &mut auth, route.as_deref()).await {
        let mut res = e.into_response();
        res.headers_mut().extend(headers);
        return res;
    }
    parts.extensions.insert(auth.clone());
    let idem_key = parts
        .headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned());
    let mut res = match (parts.method == Method::POST, idem_key, auth.principal_key()) {
        (true, Some(key), Some(principal)) => {
            idempotent(&state, tenant.id, &principal, key, parts, body, next).await
        }
        _ => next.run(Request::from_parts(parts, body)).await,
    };
    res.headers_mut().extend(headers);
    res
}

fn rate_headers(burst: u32, period_secs: u64, decision: &RateDecision) -> HeaderMap {
    let mut h = HeaderMap::new();
    let (remaining, reset) = match decision {
        RateDecision::Allowed { remaining } => {
            let used = u64::from(burst.saturating_sub(*remaining));
            // Token bucket: time until the bucket is full again.
            let reset = (used * period_secs).div_ceil(u64::from(burst.max(1)));
            (*remaining, reset)
        }
        RateDecision::Limited { retry_after } => (0, retry_after.as_secs().max(1)),
    };
    let pairs = [
        ("ratelimit-limit", burst.to_string()),
        ("ratelimit-remaining", remaining.to_string()),
        ("ratelimit-reset", reset.to_string()),
        ("ratelimit-policy", format!("{burst};w={period_secs}")),
    ];
    for (name, value) in pairs {
        if let Ok(v) = HeaderValue::from_str(&value) {
            h.insert(name, v);
        }
    }
    h
}

async fn idempotent(
    state: &AppState,
    tenant_id: TenantId,
    principal: &str,
    key: String,
    parts: Parts,
    body: Body,
    next: Next,
) -> Response {
    if key.is_empty() || key.len() > 255 || !key.chars().all(|c| c.is_ascii_graphic()) {
        return ApiError::field(
            "Idempotency-Key",
            "must be 1–255 printable ASCII characters",
        )
        .into_response();
    }
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return ApiError::from(AppError::PayloadTooLarge).into_response(),
    };
    let method = parts.method.as_str().to_owned();
    let path = parts.uri.path().to_owned();
    let mut material = Vec::with_capacity(bytes.len() + path.len() + 8);
    material.extend_from_slice(method.as_bytes());
    material.push(b'\n');
    material.extend_from_slice(path.as_bytes());
    material.push(b'\n');
    material.extend_from_slice(&bytes);
    let hash = hos_core::crypto::sha256_hex(&material);

    let begun = async {
        let mut conn = state.pool.acquire().await?;
        idempotency::begin(&mut conn, tenant_id, principal, &key, &method, &path, &hash).await
    }
    .await;
    let id = match begun {
        Ok(IdempotencyBegin::New(id)) => id,
        Ok(IdempotencyBegin::Replay { status, body }) => {
            let status = u16::try_from(status)
                .ok()
                .and_then(|s| StatusCode::from_u16(s).ok())
                .unwrap_or(StatusCode::OK);
            let mut res = (status, body.to_string()).into_response();
            let ct = if status.as_u16() >= 400 {
                "application/problem+json"
            } else {
                "application/json"
            };
            res.headers_mut()
                .insert(http::header::CONTENT_TYPE, HeaderValue::from_static(ct));
            res.headers_mut()
                .insert("idempotent-replayed", HeaderValue::from_static("true"));
            return res;
        }
        Ok(IdempotencyBegin::InProgress) => {
            return ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_in_progress",
                "A request with this Idempotency-Key is still being processed.",
            )
            .into_response();
        }
        Ok(IdempotencyBegin::Mismatch) => {
            return ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "This Idempotency-Key was used with a different request.",
            )
            .into_response();
        }
        Err(e) => return ApiError::from(e).into_response(),
    };

    let res = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    let status = res.status();
    if status.is_server_error() {
        if let Err(e) = idempotency::release(&state.pool, id).await {
            tracing::warn!(error = %e, "idempotency release failed");
        }
        return res;
    }
    let (rparts, rbody) = res.into_parts();
    let rbytes: Bytes = match axum::body::to_bytes(rbody, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "idempotency: response body unreadable");
            let _ = idempotency::release(&state.pool, id).await;
            return ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal error.",
            )
            .into_response();
        }
    };
    let stored: Value = serde_json::from_slice(&rbytes).unwrap_or(Value::Null);
    if let Err(e) =
        idempotency::complete(&state.pool, id, i32::from(status.as_u16()), &stored).await
    {
        tracing::warn!(error = %e, "idempotency complete failed");
    }
    Response::from_parts(rparts, Body::from(rbytes))
}

// ---------------------------------------------------------------------------
// request helpers
// ---------------------------------------------------------------------------

/// Parses a JSON body (`422` with the parser message on failure).
pub fn parse_body<T: DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    if body.is_empty() {
        return serde_json::from_str("{}").map_err(|e| ApiError::field("body", e.to_string()));
    }
    serde_json::from_slice(body).map_err(|e| {
        let msg = e.to_string();
        let field = msg
            .split('`')
            .nth(1)
            .filter(|_| msg.starts_with("missing field") || msg.starts_with("unknown field"))
            .unwrap_or("body")
            .to_owned();
        ApiError::validation(vec![FieldError {
            field,
            message: msg,
        }])
    })
}

/// Parses a path id (`404` when malformed).
pub fn parse_id<T: std::str::FromStr>(raw: &str) -> ApiResult<T> {
    raw.parse().map_err(|_| ApiError::not_found())
}

/// Parsed list query.
pub struct ListQuery {
    /// Filters.
    pub filter: ApiFilter,
    /// Sort.
    pub sort: ApiSort,
    /// Page.
    pub page: Page,
}

/// Allowed `sort` keys: `(name, column)`; the first is the default
/// (descending).
pub type Sorts = &'static [(&'static str, &'static str)];

/// Parses `limit`, `offset`, `sort`, `status`, `q`, `client_id`,
/// `created_from`, `created_to` and applies the caller's client scope.
pub fn list_query(
    raw: &HashMap<String, String>,
    sorts: Sorts,
    statuses: &[&str],
    scope: Scope,
) -> ApiResult<ListQuery> {
    let mut errors = Vec::new();
    let mut bad = |field: &str, message: String| {
        errors.push(FieldError {
            field: field.to_owned(),
            message,
        });
    };
    let limit = match raw.get("limit").map(|s| s.trim()) {
        None | Some("") => Page::DEFAULT_LIMIT,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if (1..=Page::MAX_LIMIT).contains(&n) => n,
            _ => {
                bad("limit", format!("must be an integer 1–{}", Page::MAX_LIMIT));
                Page::DEFAULT_LIMIT
            }
        },
    };
    let offset = match raw.get("offset").map(|s| s.trim()) {
        None | Some("") => 0,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n >= 0 => n,
            _ => {
                bad("offset", "must be a non-negative integer".into());
                0
            }
        },
    };
    let (default_name, default_col) = sorts
        .first()
        .copied()
        .unwrap_or(("created_at", "created_at"));
    let sort = match raw.get("sort").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => ApiSort {
            column: default_col,
            desc: true,
        },
        Some(s) => {
            let (desc, name) = match s.strip_prefix('-') {
                Some(n) => (true, n),
                None => (false, s.strip_prefix('+').unwrap_or(s)),
            };
            match sorts.iter().find(|(n, _)| *n == name) {
                Some((_, col)) => ApiSort { column: col, desc },
                None => {
                    let allowed: Vec<String> = sorts.iter().map(|(n, _)| (*n).to_owned()).collect();
                    bad(
                        "sort",
                        format!(
                            "unknown sort key; use one of {} (prefix `-` for descending)",
                            allowed.join(", ")
                        ),
                    );
                    ApiSort {
                        column: default_col,
                        desc: true,
                    }
                }
            }
        }
    };
    let _ = default_name;
    let status = raw
        .get("status")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    if let Some(s) = &status
        && !statuses.is_empty()
        && !statuses.contains(&s.as_str())
    {
        bad("status", format!("must be one of {}", statuses.join(", ")));
    }
    let mut client_id: Option<ClientId> = None;
    if let Some(raw_c) = raw
        .get("client_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        match raw_c.parse() {
            Ok(c) => client_id = Some(c),
            Err(_) => bad("client_id", "must be a UUID".into()),
        }
    }
    let mut date = |field: &str| -> Option<DateTime<Utc>> {
        let v = raw.get(field).map(|s| s.trim()).filter(|s| !s.is_empty())?;
        match DateTime::parse_from_rfc3339(v) {
            Ok(d) => Some(d.with_timezone(&Utc)),
            Err(_) => {
                bad(field, "must be an RFC 3339 timestamp".into());
                None
            }
        }
    };
    let created_from = date("created_from");
    let created_to = date("created_to");
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    // A client key can never widen its scope.
    let client_id = match scope.client() {
        Some(own) => Some(own),
        None => client_id,
    };
    Ok(ListQuery {
        filter: ApiFilter {
            client_id,
            status,
            search: raw
                .get("q")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty()),
            created_from,
            created_to,
        },
        sort,
        page: Page { limit, offset },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn every_route_has_member_areas() {
        for (method, path) in route_table() {
            let areas = member_areas(path)
                .unwrap_or_else(|| panic!("{method} {path} has no member-area mapping"));
            for a in areas {
                // `owner`: the account owner only (as `client::area_of`).
                assert!(
                    crate::routes::client::MEMBER_AREAS.contains(a) || *a == "owner",
                    "{path}: unknown area {a}"
                );
            }
        }
        assert_eq!(
            member_areas("/api/v1/services/{id}/power"),
            Some(&["services"][..])
        );
        assert_eq!(
            member_areas("/api/v1/ssh-keys/{id}"),
            Some(&["services"][..])
        );
        assert_eq!(member_areas("/api/v1/affiliate"), Some(&["owner"][..]));
        assert_eq!(member_areas("/api/v1/status"), Some(&[][..]));
        assert_eq!(member_areas("/api/v1/nope"), None);
    }

    #[test]
    fn client_scope_requires_a_granted_area() {
        let principal = Principal::Client {
            user_id: UserId::new(),
            client_id: ClientId::new(),
            tenant_id: TenantId::new(),
            impersonator: None,
        };
        let auth = |permissions: &[&str]| ApiAuth {
            principal: principal.clone(),
            key_id: None,
            member: Some(MemberAccess {
                owner: false,
                permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
            }),
        };
        let tickets_only = auth(&["tickets"]);
        assert!(tickets_only.scope(Permission::SUPPORT_VIEW).is_ok());
        assert_eq!(
            tickets_only
                .scope(Permission::SERVICES_VIEW)
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        assert!(auth(&["orders"]).scope(Permission::BILLING_MANAGE).is_ok());
        assert!(
            auth(&[]).scope(Permission::SERVICES_MANAGE).is_ok(),
            "empty = all"
        );
        let unloaded = ApiAuth {
            member: None,
            ..auth(&[])
        };
        assert!(unloaded.scope(Permission::SUPPORT_VIEW).is_err());
    }

    #[test]
    fn scoped_key_grants_respect_wildcards() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        // Wildcard grants used to be dropped by a literal string match.
        assert_eq!(
            scoped_grants(&s(&["clients.*", "billing.view"]), &s(&["clients.view"])),
            s(&["clients.view"])
        );
        assert_eq!(
            scoped_grants(&s(&["clients.manage"]), &s(&["clients.view"])),
            s(&["clients.view"])
        );
        // A scope beyond the user's grants gives nothing.
        assert!(scoped_grants(&s(&["billing.view"]), &s(&["settings.manage"])).is_empty());
        assert_eq!(scoped_grants(&s(&["*"]), &s(&["*"])), s(&["*"]));
        assert_eq!(
            scoped_grants(&s(&["*"]), &s(&["reports.view"])),
            s(&["reports.view"])
        );
    }

    const SORTS: Sorts = &[("created_at", "created_at"), ("total", "total")];

    #[test]
    fn list_query_defaults_and_scope() {
        let staff = Scope::Staff(UserId::new());
        let l = list_query(&q(&[]), SORTS, &[], staff).unwrap();
        assert_eq!(l.page.limit, Page::DEFAULT_LIMIT);
        assert_eq!(l.page.offset, 0);
        assert!(l.sort.desc);
        assert_eq!(l.sort.column, "created_at");

        let own = ClientId::new();
        let other = ClientId::new();
        let client = Scope::Client {
            user_id: UserId::new(),
            client_id: own,
        };
        let l = list_query(&q(&[("client_id", &other.to_string())]), SORTS, &[], client).unwrap();
        assert_eq!(l.filter.client_id, Some(own), "client scope wins");
    }

    #[test]
    fn list_query_validates_fields() {
        let staff = Scope::Staff(UserId::new());
        let err = list_query(
            &q(&[("limit", "0"), ("sort", "nope"), ("status", "x")]),
            SORTS,
            &["paid"],
            staff,
        )
        .err()
        .unwrap();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        let fields: Vec<_> = err.errors.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(fields, ["limit", "sort", "status"]);
        let l = list_query(&q(&[("sort", "total")]), SORTS, &[], staff).unwrap();
        assert_eq!(l.sort.column, "total");
        assert!(!l.sort.desc);
    }

    #[test]
    fn rate_headers_report_remaining() {
        let h = rate_headers(600, 60, &RateDecision::Allowed { remaining: 590 });
        assert_eq!(h["ratelimit-limit"], "600");
        assert_eq!(h["ratelimit-remaining"], "590");
        assert_eq!(h["ratelimit-reset"], "1");
        let h = rate_headers(
            600,
            60,
            &RateDecision::Limited {
                retry_after: std::time::Duration::from_secs(3),
            },
        );
        assert_eq!(h["ratelimit-remaining"], "0");
        assert_eq!(h["ratelimit-reset"], "3");
    }

    #[test]
    fn problem_body_lists_field_errors() {
        let e = ApiError::field("items", "required");
        let b = e.body();
        assert_eq!(b["status"], 422);
        assert_eq!(b["errors"][0]["field"], "items");
    }

    #[test]
    fn scope_hides_foreign_records() {
        let own = ClientId::new();
        let s = Scope::Client {
            user_id: UserId::new(),
            client_id: own,
        };
        assert!(s.owns(Some(own)).is_ok());
        assert_eq!(
            s.owns(Some(ClientId::new())).unwrap_err().status,
            StatusCode::NOT_FOUND
        );
        assert!(Scope::Staff(UserId::new()).owns(None).is_ok());
    }
}
