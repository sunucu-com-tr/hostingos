//! `ping`, `me`, dashboard stats, integrations (metadata) and outgoing
//! webhook endpoints.

use axum::body::Bytes;
use axum::extract::{Path, State};
use hos_auth::rbac::{Permission, Principal};
use hos_core::ids::WebhookEndpointId;
use hos_db::repo::{api as api_repo, integrations, stats, webhooks};
use serde::Deserialize;
use serde_json::json;

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, collection, dto, parse_body, parse_id,
};
use crate::state::AppState;

/// `GET /api/v1/ping` (no auth; no version — anonymous callers do not
/// learn which release runs).
pub async fn ping(tenant: ApiTenant) -> ApiReply {
    ApiReply::ok(json!({
        "ok": true,
        "tenant": tenant.slug,
    }))
}

/// `GET /api/v1/me`.
pub async fn me(tenant: ApiTenant, auth: ApiAuth) -> ApiResult<ApiReply> {
    let via = if auth.key_id.is_some() {
        "api_key"
    } else {
        "session"
    };
    let body = match &auth.principal {
        Principal::Staff {
            user_id,
            permissions,
            ..
        } => json!({
            "kind": "staff",
            "tenant": tenant.slug,
            "user_id": user_id,
            "permissions": Permission::ALL
                .iter()
                .filter(|p| permissions.allows(**p))
                .map(|p| p.as_str())
                .collect::<Vec<_>>(),
            "via": via,
        }),
        Principal::Client {
            user_id, client_id, ..
        } => json!({
            "kind": "client",
            "tenant": tenant.slug,
            "user_id": user_id,
            "client_id": client_id,
            "via": via,
        }),
        _ => return Err(ApiError::unauthorized()),
    };
    Ok(ApiReply::ok(body))
}

/// `GET /api/v1/stats/dashboard` (`reports.view`).
pub async fn dashboard_stats(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::REPORTS_VIEW)?;
    let s = stats::dashboard(&state.pool, tenant.id).await?;
    let body =
        serde_json::to_value(&s).map_err(|e| ApiError::from(crate::AppError::internal(e)))?;
    Ok(ApiReply::ok(body))
}

/// `GET /api/v1/integrations` (`integrations.manage`) — metadata only.
pub async fn integrations(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::INTEGRATIONS_MANAGE)?;
    let rows = integrations::list_integrations(&state.pool, tenant.id, None).await?;
    Ok(collection(rows.iter().map(dto::integration).collect()))
}

/// `GET /api/v1/webhook-endpoints` (`integrations.manage`).
pub async fn webhook_endpoints(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::INTEGRATIONS_MANAGE)?;
    let rows = webhooks::list_endpoints(&state.pool, tenant.id).await?;
    Ok(collection(rows.iter().map(dto::webhook_endpoint).collect()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointInput {
    url: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    events: Vec<String>,
}

/// `POST /api/v1/webhook-endpoints` — the signing secret is returned once.
pub async fn create_webhook_endpoint(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    body: Bytes,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::INTEGRATIONS_MANAGE)?;
    let input: EndpointInput = parse_body(&body)?;
    let events: Vec<String> = input
        .events
        .iter()
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    if events.iter().any(|e| {
        e.len() > 80
            || !e.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '*')
            })
    }) {
        return Err(ApiError::field(
            "events",
            "event names are lowercase dotted identifiers or `*`",
        ));
    }
    let description = input
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let (row, secret) = state
        .billing
        .create_webhook_endpoint(tenant.id, &input.url, description, &events)
        .await
        .map_err(|e| match e {
            hos_core::CoreError::Validation(m) => ApiError::field("url", m),
            other => other.into(),
        })?;
    let mut v = dto::webhook_endpoint(&row);
    v["secret"] = json!(secret.expose_str());
    Ok(ApiReply::created(v))
}

/// `DELETE /api/v1/webhook-endpoints/{id}`.
pub async fn delete_webhook_endpoint(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::INTEGRATIONS_MANAGE)?;
    let id: WebhookEndpointId = parse_id(&id)?;
    if !api_repo::delete_webhook_endpoint(&state.pool, tenant.id, id).await? {
        return Err(ApiError::not_found());
    }
    Ok(ApiReply::ok(json!({ "id": id, "deleted": true })))
}
