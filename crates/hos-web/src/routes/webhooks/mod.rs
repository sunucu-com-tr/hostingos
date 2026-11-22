//! Incoming webhooks (`/webhooks/{gateway|registrar}/{integration_id}`).
//!
//! * Served **outside** the tenant stack ([`public_router`], merged in
//!   `routes::app`): the tenant is resolved from the `integrations` row, not
//!   from `Host` — gateways call the URL they were given (`HOS_BASE_URL`,
//!   usually the platform domain). CSRF and sessions do not apply.
//! * `gateway`: `GET` and `POST`; the raw body, headers and raw query string
//!   go unchanged into [`WebhookRequest`] → `billing.handle_webhook`
//!   (signature check + idempotent application, every event recorded in
//!   `webhooks_in`). The gateway's [`WebhookReply`] is applied as is
//!   (`Text` → `text/plain`, `Json`, `Redirect` → `303`, `Empty` → `200`).
//!   Invalid signature → `401`; other client errors → `4xx`; retryable
//!   failures (database, upstream) → `503` so the gateway retries. Requests
//!   that produced no recorded event (rejected, failed, ignored) are stored in
//!   `webhooks_in` too (`req:<uuid>`), so every call is auditable.
//! * `registrar`: generic receiver — the call is recorded and a
//!   `sync_domain_status` job is queued for the tenant (the job asks the
//!   registrar API, so a forged call can only trigger a sync).
//! * Order of checks (gateway): a client IP with too many failed calls
//!   ([`crate::state::Limits::webhook_failed`]) gets `429` first; then the
//!   signature is verified (a local computation for every gateway but
//!   PayPal's webhooks, which call PayPal's verification API and keep a
//!   per-integration limit in front, `webhook_remote`); only verified
//!   requests take from the integration's own bucket (`webhook`), so forged
//!   calls cannot lock out genuine callbacks and browser returns.
//! * What is stored: a request that fails verification only as size +
//!   SHA-256 (never its body, which may carry card / 3-D Secure data), and
//!   at most [`MAX_FAILED_ROWS`] of them per integration; verified requests
//!   without events with card fields removed (`hos_payments::storable_body`).
//! * Bodies are limited to [`MAX_BODY_BYTES`]. Errors are
//!   `application/problem+json`.

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hos_auth::ratelimit::RateDecision;
use hos_billing::jobs::{JobKind, JobRequest, TenantPayload};
use hos_core::CoreError;
use hos_core::domain::IntegrationCategory;
use hos_core::ids::{IntegrationId, TenantId};
use hos_core::integrations::gateway::{WebhookReply, WebhookRequest};
use hos_db::enums::WebhookInStatus;
use hos_db::repo::{saas, webhooks};
use http::header::{CONTENT_TYPE, LOCATION, RETRY_AFTER};
use http::{HeaderMap, HeaderValue, StatusCode, Uri};
use serde_json::{Value, json};

use crate::extract::ClientIp;
use crate::state::AppState;

/// Maximum webhook body size (bytes).
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Requests that failed verification kept per integration (newest).
pub const MAX_FAILED_ROWS: i64 = 200;

pub mod mail;

/// Tenant-host webhooks (tenant from `Host`): inbound e-mail ([`mail`]).
/// Gateway / registrar webhooks live in [`public_router`].
pub fn router(_state: &AppState) -> Router<AppState> {
    mail::routes()
}

/// Webhook routes, served without tenant resolution.
pub fn public_router(_state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/webhooks/gateway/{integration_id}",
            get(gateway).post(gateway),
        )
        .route(
            "/webhooks/registrar/{integration_id}",
            get(registrar).post(registrar),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// RFC 9457 problem response (same shape as `middleware::error_pages`).
pub fn problem(status: StatusCode, code: &str, detail: Option<&str>) -> Response {
    let body = json!({
        "type": "about:blank",
        "title": status.canonical_reason().unwrap_or("Error"),
        "status": status.as_u16(),
        "code": code,
        "detail": detail,
    });
    let mut res = (status, body.to_string()).into_response();
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    res
}

fn rate_limited(retry_after: std::time::Duration) -> Response {
    let mut res = problem(StatusCode::TOO_MANY_REQUESTS, "rate_limited", None);
    if let Ok(v) = HeaderValue::from_str(&retry_after.as_secs().max(1).to_string()) {
        res.headers_mut().insert(RETRY_AFTER, v);
    }
    res
}

fn unavailable() -> Response {
    let mut res = problem(StatusCode::SERVICE_UNAVAILABLE, "unavailable", None);
    res.headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("60"));
    res
}

/// Integration of the URL, checked for category and state. The error is the
/// finished problem response (built once per rejected request).
#[allow(clippy::result_large_err)]
async fn resolve(
    state: &AppState,
    raw_id: &str,
    category: IntegrationCategory,
) -> Result<(IntegrationId, TenantId), Response> {
    let not_found = || problem(StatusCode::NOT_FOUND, "not_found", None);
    let id: IntegrationId = raw_id.parse().map_err(|_| not_found())?;
    let owner = match saas::integration_owner(&state.pool, id).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "webhook integration lookup failed");
            return Err(unavailable());
        }
    };
    match owner {
        Some(o)
            if o.category == category
                && o.is_active
                && o.tenant_status != hos_core::domain::TenantStatus::Cancelled =>
        {
            Ok((id, o.tenant_id))
        }
        _ => Err(not_found()),
    }
}

/// `429` when the client IP has used up its failed-call budget.
pub(crate) fn ip_blocked(state: &AppState, ip: &ClientIp) -> Option<Response> {
    state
        .limits
        .webhook_failed
        .is_limited(&ip.key("webhook"))
        .then(|| rate_limited(std::time::Duration::from_secs(60)))
}

/// Counts a call that failed authentication against its client IP.
pub(crate) fn note_failure(state: &AppState, ip: &ClientIp) {
    let _ = state.limits.webhook_failed.check(&ip.key("webhook"));
}

/// Headers worth keeping for diagnosis (no cookies / authorization), each
/// value capped.
fn recorded_headers(headers: &HeaderMap) -> Value {
    headers
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            k.starts_with("content-")
                || k.contains("signature")
                || k == "user-agent"
                || (k.starts_with("x-") && !k.contains("token") && !k.contains("auth"))
        })
        .take(30)
        .map(|(k, v)| {
            let v: String = v.to_str().unwrap_or_default().chars().take(200).collect();
            (k.to_string(), json!(v))
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// What is kept of a request that was not verified: sizes and hashes only.
fn unverified_summary(body: &[u8], query: Option<&str>) -> Value {
    let query = query.unwrap_or_default();
    json!({
        "body_bytes": body.len(),
        "body_sha256": hos_core::crypto::sha256_hex(body),
        "query_bytes": query.len(),
        "query_sha256": (!query.is_empty()).then(|| hos_core::crypto::sha256_hex(query.as_bytes())),
    })
}

/// What is kept of a verified request: the body and query without card /
/// 3-D Secure fields (a body that cannot be checked: size + hash).
fn recorded_payload(body: &[u8], query: Option<&str>) -> Value {
    let body = hos_payments::storable_body(body).unwrap_or_else(
        || json!({"bytes": body.len(), "sha256": hos_core::crypto::sha256_hex(body)}),
    );
    json!({ "body": body, "query": query.map(hos_payments::storable_query) })
}

/// Stores a request that failed (or skipped) authentication: summary only,
/// at most [`MAX_FAILED_ROWS`] per integration.
#[allow(clippy::too_many_arguments)]
async fn record_unverified(
    state: &AppState,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    event_type: &str,
    status: WebhookInStatus,
    headers: &HeaderMap,
    body: &[u8],
    query: Option<&str>,
    error: Option<&str>,
) {
    let result = async {
        let mut conn = state.pool.acquire().await?;
        webhooks::record_unverified(
            &mut conn,
            tenant_id,
            integration_id,
            event_type,
            status,
            &unverified_summary(body, query),
            &recorded_headers(headers),
            error,
            MAX_FAILED_ROWS,
        )
        .await?;
        Ok::<_, CoreError>(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, %integration_id, "recording webhook request failed");
    }
}

/// Stores one `webhooks_in` row for a verified request (status `status`).
#[allow(clippy::too_many_arguments)]
async fn record_request(
    state: &AppState,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    event_type: &str,
    headers: &HeaderMap,
    body: &[u8],
    query: Option<&str>,
    status: WebhookInStatus,
    error: Option<&str>,
) {
    let key = format!("req:{}", uuid::Uuid::now_v7());
    let result = async {
        let (row, _) = webhooks::record_incoming(
            &state.pool,
            tenant_id,
            integration_id,
            &key,
            Some(event_type),
            &recorded_payload(body, query),
            &recorded_headers(headers),
            true,
        )
        .await?;
        webhooks::set_webhook_status(&state.pool, tenant_id, row.id, status, error).await?;
        Ok::<_, CoreError>(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, %integration_id, "recording webhook request failed");
    }
}

/// Rewrites stored `webhooks_in` payloads without card / 3-D Secure fields
/// (`hostingos webhooks scrub`): JSON payloads lose [`hos_payments::CARD_FIELDS`];
/// bodies kept as raw text are replaced by size + hash when they hold such a
/// field. `tenant_id`: one tenant only (else all). Returns the number of
/// rows rewritten.
pub async fn scrub_stored_payloads(
    pool: &hos_db::PgPool,
    tenant_id: Option<TenantId>,
) -> Result<u64, CoreError> {
    let mut after = None;
    let mut changed = 0;
    loop {
        let page = webhooks::payload_page(pool, tenant_id, after, 500).await?;
        let Some((last, _, _)) = page.last() else {
            break;
        };
        after = Some(*last);
        for (id, tenant_id, payload) in page {
            if let Some(scrubbed) = scrubbed_payload(&payload)
                && webhooks::replace_payload(pool, tenant_id, id, &scrubbed).await?
            {
                changed += 1;
            }
        }
    }
    Ok(changed)
}

/// The scrubbed copy of a stored payload, `None` when nothing changes.
fn scrubbed_payload(payload: &Value) -> Option<Value> {
    let mut out = payload.clone();
    let mut changed = hos_payments::scrub_card_fields(&mut out);
    for key in ["body", "query"] {
        let Some(Value::String(text)) = out.get(key) else {
            continue;
        };
        let carries_card = url::form_urlencoded::parse(text.as_bytes())
            .any(|(k, _)| hos_payments::is_card_field(&k));
        if carries_card {
            let summary = json!({
                "bytes": text.len(),
                "sha256": hos_core::crypto::sha256_hex(text.as_bytes()),
            });
            out[key] = summary;
            changed = true;
        }
    }
    changed.then_some(out)
}

/// Applies a gateway's [`WebhookReply`].
pub fn reply_response(reply: &WebhookReply) -> Response {
    match reply {
        WebhookReply::Empty => StatusCode::OK.into_response(),
        WebhookReply::Text(text) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "text/plain; charset=utf-8")],
            text.clone(),
        )
            .into_response(),
        WebhookReply::Json(v) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            v.to_string(),
        )
            .into_response(),
        WebhookReply::Redirect(url) => match HeaderValue::from_str(url) {
            Ok(v) => {
                let mut res = StatusCode::SEE_OTHER.into_response();
                res.headers_mut().insert(LOCATION, v);
                res
            }
            Err(_) => StatusCode::OK.into_response(),
        },
    }
}

#[allow(clippy::result_large_err)]
fn body_or_problem(body: Result<Bytes, BytesRejection>) -> Result<Bytes, Response> {
    body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", None)
        } else {
            problem(StatusCode::BAD_REQUEST, "bad_request", None)
        }
    })
}

/// `GET|POST /webhooks/gateway/{integration_id}`.
async fn gateway(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    ip: ClientIp,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(res) = ip_blocked(&state, &ip) {
        return res;
    }
    let (integration_id, tenant_id) =
        match resolve(&state, &raw_id, IntegrationCategory::Gateway).await {
            Ok(v) => v,
            Err(res) => {
                if res.status() == StatusCode::NOT_FOUND {
                    note_failure(&state, &ip);
                }
                return res;
            }
        };
    let body = match body_or_problem(body) {
        Ok(b) => b,
        Err(res) => return res,
    };
    let query = uri.query().map(str::to_owned);
    let request = WebhookRequest {
        headers: headers.clone(),
        body: body.to_vec(),
        query: query.clone(),
        remote_ip: Some(ip.0),
    };
    // A gateway that asks its provider to verify (PayPal webhooks) is the
    // only work an unverified request can trigger outside this process.
    match state
        .billing
        .webhook_verifies_remotely(tenant_id, integration_id, &request)
        .await
    {
        Ok(true) => {
            if let RateDecision::Limited { retry_after } = state
                .limits
                .webhook_remote
                .check(&format!("webhook-remote:{integration_id}"))
            {
                return rate_limited(retry_after);
            }
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(%tenant_id, %integration_id, error = %e, "gateway webhook: integration unavailable");
            return unavailable();
        }
    }
    let verified = match state
        .billing
        .verify_webhook(tenant_id, integration_id, &request)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let status = e.http_status();
            let message = e.to_string();
            if (400..500).contains(&status) {
                note_failure(&state, &ip);
            }
            let event = if status == 401 {
                "request.unauthorized"
            } else {
                "request.failed"
            };
            record_unverified(
                &state,
                tenant_id,
                integration_id,
                event,
                WebhookInStatus::Failed,
                &headers,
                &body,
                query.as_deref(),
                Some(&message),
            )
            .await;
            return error_response(status, tenant_id, integration_id, &message);
        }
    };
    // Verified traffic has its own bucket per integration.
    if let RateDecision::Limited { retry_after } = state
        .limits
        .webhook
        .check(&format!("webhook:{integration_id}"))
    {
        return rate_limited(retry_after);
    }
    let has_events = verified.event_count() > 0;
    match state
        .billing
        .apply_webhook(tenant_id, integration_id, verified)
        .await
    {
        Ok(result) => {
            if !has_events || result.events.is_empty() {
                record_request(
                    &state,
                    tenant_id,
                    integration_id,
                    "request.ignored",
                    &headers,
                    &body,
                    query.as_deref(),
                    WebhookInStatus::Ignored,
                    None,
                )
                .await;
            }
            tracing::info!(
                %tenant_id,
                %integration_id,
                events = result.events.len(),
                "gateway webhook processed"
            );
            reply_response(&result.reply)
        }
        Err(e) => {
            let status = e.http_status();
            let message = e.to_string();
            record_request(
                &state,
                tenant_id,
                integration_id,
                "request.failed",
                &headers,
                &body,
                query.as_deref(),
                WebhookInStatus::Failed,
                Some(&message),
            )
            .await;
            error_response(status, tenant_id, integration_id, &message)
        }
    }
}

/// Problem response for a failed gateway call (`5xx`: retry later).
fn error_response(
    status: u16,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    message: &str,
) -> Response {
    match status {
        401 => problem(StatusCode::UNAUTHORIZED, "unauthorized", None),
        403 => problem(StatusCode::FORBIDDEN, "forbidden", None),
        404 => problem(StatusCode::NOT_FOUND, "not_found", None),
        400..=499 => {
            tracing::warn!(%tenant_id, %integration_id, error = %message, "gateway webhook rejected");
            problem(StatusCode::BAD_REQUEST, "bad_request", None)
        }
        _ => {
            tracing::error!(%tenant_id, %integration_id, error = %message, "gateway webhook failed; asking for a retry");
            unavailable()
        }
    }
}

/// `GET|POST /webhooks/registrar/{integration_id}`.
///
/// Registrar notifications carry no signature: the call only queues a
/// (deduplicated) status sync that asks the registrar API. It is rate
/// limited per integration and stored as size + hash.
async fn registrar(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    ip: ClientIp,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Some(res) = ip_blocked(&state, &ip) {
        return res;
    }
    let (integration_id, tenant_id) =
        match resolve(&state, &raw_id, IntegrationCategory::Registrar).await {
            Ok(v) => v,
            Err(res) => {
                if res.status() == StatusCode::NOT_FOUND {
                    note_failure(&state, &ip);
                }
                return res;
            }
        };
    if let RateDecision::Limited { retry_after } = state
        .limits
        .webhook
        .check(&format!("webhook:{integration_id}"))
    {
        return rate_limited(retry_after);
    }
    let body = match body_or_problem(body) {
        Ok(b) => b,
        Err(res) => return res,
    };
    record_unverified(
        &state,
        tenant_id,
        integration_id,
        "registrar.notification",
        WebhookInStatus::Processed,
        &headers,
        &body,
        uri.query(),
        None,
    )
    .await;
    // One sync per tenant per 5 minutes, however many notifications arrive.
    let bucket = chrono::Utc::now().timestamp() / 300;
    let req = JobRequest::new(
        JobKind::SyncDomainStatus,
        Some(tenant_id),
        &TenantPayload::default(),
    )
    .dedupe(format!("sync_domain_status:{tenant_id}:webhook:{bucket}"));
    let queued = async {
        let mut conn = state.pool.acquire().await?;
        hos_billing::jobs::enqueue(&mut conn, req).await?;
        Ok::<_, CoreError>(())
    }
    .await;
    match queued {
        Ok(()) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "text/plain; charset=utf-8")],
            "OK",
        )
            .into_response(),
        Err(e) => {
            tracing::error!(%tenant_id, %integration_id, error = %e, "queueing domain sync failed");
            unavailable()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_map_to_responses() {
        assert_eq!(
            reply_response(&WebhookReply::Empty).status(),
            StatusCode::OK
        );
        let r = reply_response(&WebhookReply::Text("OK".into()));
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            r.headers()[CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        let r = reply_response(&WebhookReply::Redirect("https://x.test/return".into()));
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[LOCATION], "https://x.test/return");
        let r = reply_response(&WebhookReply::Json(json!({"ok": true})));
        assert_eq!(r.headers()[CONTENT_TYPE], "application/json");
    }

    #[test]
    fn recorded_headers_skip_secrets() {
        let mut h = HeaderMap::new();
        h.insert("cookie", HeaderValue::from_static("a=b"));
        h.insert("authorization", HeaderValue::from_static("Bearer x"));
        h.insert(
            "x-hos-mock-signature",
            HeaderValue::from_static("t=1,v1=ab"),
        );
        h.insert("content-type", HeaderValue::from_static("application/json"));
        let v = recorded_headers(&h);
        assert!(v.get("cookie").is_none());
        assert!(v.get("authorization").is_none());
        assert!(v.get("x-hos-mock-signature").is_some());
        assert!(v.get("content-type").is_some());
    }
}
