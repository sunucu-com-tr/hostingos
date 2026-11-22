//! `GET /platform/handoff?token=` — the tenant-host half of "sign in as
//! tenant owner".
//!
//! The console (`tenants::impersonate`) stores a one-time token (60 s,
//! `platform_handoffs`) and redirects the operator's browser to the tenant's
//! own host. Cookies are host-only, so the tenant session can only be set
//! there: this handler resolves the tenant from `Host`, consumes the token
//! (single use, bound to that tenant), starts a staff session for the owner
//! with `impersonator = platform_admin` and writes `auth.impersonation_started`
//! into the tenant's audit log.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use hos_core::domain::UserKind;
use hos_db::enums::{AccountStatus, ActorKind, ImpersonatorKind};
use hos_db::repo::identity::{self, SessionOwner};
use hos_db::repo::{audit, saas};
use http::header::SET_COOKIE;
use serde::Deserialize;
use serde_json::json;

use super::AuditCtx;
use crate::error::{AppError, AppResult};
use crate::session::{self, StartSession};
use crate::state::AppState;
use crate::tenant::RequestHost;

/// Query of [`handoff`].
#[derive(Debug, Default, Deserialize)]
pub struct HandoffQuery {
    #[serde(default)]
    token: String,
}

/// Hash stored for a hand-off token.
pub fn token_hash(token: &str) -> String {
    // Pre-rename "hf" label, kept on purpose: stored token hashes depend on it.
    hos_core::crypto::sha256_hex(format!("hf-handoff:{}", token.trim()).as_bytes())
}

/// `GET /platform/handoff?token=`.
pub async fn handoff(
    State(state): State<AppState>,
    req: AuditCtx,
    host: RequestHost,
    Query(q): Query<HandoffQuery>,
) -> AppResult<Response> {
    if q.token.trim().is_empty() {
        return Err(AppError::NotFound);
    }
    let tenant = state
        .tenants
        .resolve(&state, &host.name)
        .await?
        .filter(|t| t.is_operational())
        .ok_or(AppError::NotFound)?;
    let handoff = saas::consume_handoff(&state.pool, &token_hash(&q.token))
        .await?
        .ok_or(AppError::NotFound)?;
    if handoff.tenant_id != tenant.id {
        return Err(AppError::NotFound);
    }
    let user = identity::get_user(&state.pool, tenant.id, handoff.user_id)
        .await?
        .filter(|u| u.kind == UserKind::Staff && u.status == AccountStatus::Active)
        .ok_or(AppError::NotFound)?;
    let (_, cookie) = session::start_session(
        &state,
        StartSession {
            owner: SessionOwner::User {
                tenant_id: tenant.id,
                user_id: user.id,
            },
            remember: false,
            mfa_verified: true,
            ip: Some(req.ip.0),
            user_agent: req.ua.0.clone(),
            impersonator: Some((handoff.admin_id.into(), ImpersonatorKind::PlatformAdmin)),
            data: json!({ "platform_impersonation": true }),
        },
    )
    .await?;
    let entry = audit::AuditEntry::new(
        Some(tenant.id),
        ActorKind::PlatformAdmin,
        Some(handoff.admin_id.into()),
        "auth.impersonation_started",
    )
    .entity("user", user.id)
    .request(Some(req.ip.0), req.ua.0.as_deref())
    .metadata(json!({ "via": "platform", "user_email": user.email }));
    audit::write(&state.pool, &entry).await?;
    Ok(([(SET_COOKIE, cookie)], Redirect::to("/admin")).into_response())
}
