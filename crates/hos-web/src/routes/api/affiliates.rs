//! `/api/v1/affiliate*` — the calling client's own affiliate account
//! (read-only): the account with its figures and referral link, its
//! commissions, its referrals (masked names, as on `/client/affiliate`) and
//! its payouts. Client principals only, and only the account owner (member
//! area `owner`, like the client page); staff see affiliates in the admin.

use axum::extract::State;
use hos_db::repo::affiliates::{self as arepo, AffiliateRow};
use serde_json::{Value, json};

use super::{ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, collection, dto};
use crate::state::AppState;

/// Most rows a list returns (newest first).
const LIMIT: i64 = 100;

/// The caller's client account (client principals only).
fn client_of(auth: &ApiAuth) -> ApiResult<hos_core::ids::ClientId> {
    match &auth.principal {
        hos_auth::rbac::Principal::Client { client_id, .. } => Ok(*client_id),
        hos_auth::rbac::Principal::Staff { .. } => Err(ApiError::forbidden(
            "This endpoint is only available to client API keys.",
        )),
        _ => Err(ApiError::unauthorized()),
    }
}

/// The caller's affiliate account (`404` when not enrolled).
async fn own(state: &AppState, tenant: &ApiTenant, auth: &ApiAuth) -> ApiResult<AffiliateRow> {
    let client = client_of(auth)?;
    arepo::affiliate_for_client(&state.pool, tenant.id, client)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// `GET /api/v1/affiliate` — the account, its figures and referral link.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    let a = own(&state, &tenant, &auth).await?;
    let stats = arepo::affiliate_stats(&state.pool, tenant.id, a.id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let base = state.billing.tenant_url(tenant.id).await?;
    Ok(ApiReply::ok(json!({
        "id": a.id,
        "code": a.code,
        "status": a.status.as_str(),
        "currency": a.currency.as_str(),
        "link": format!("{}/r/{}", base.trim_end_matches('/'), a.code),
        "referrals": stats.referrals,
        "clicks": stats.clicks,
        "pending": dto::money(stats.pending),
        "balance": dto::money(stats.balance),
        "paid_out": dto::money(stats.paid_out),
        "created_at": a.created_at,
    })))
}

/// `GET /api/v1/affiliate/commissions` — newest 100.
pub async fn commissions(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    let a = own(&state, &tenant, &auth).await?;
    let rows = arepo::commissions_of(&state.pool, tenant.id, a.id, LIMIT).await?;
    Ok(collection(
        rows.iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "kind": c.kind,
                    "base_amount": dto::money(c.base_amount),
                    "amount": dto::money(c.amount),
                    "currency": c.currency.as_str(),
                    "status": c.status.as_str(),
                    "clears_at": c.clears_at,
                    "reversed_at": c.reversed_at,
                    "created_at": c.created_at,
                })
            })
            .collect::<Vec<Value>>(),
    ))
}

/// `GET /api/v1/affiliate/referrals` — newest 100 (first name and the
/// initial of the last name only).
pub async fn referrals(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    let a = own(&state, &tenant, &auth).await?;
    let rows = arepo::referrals_of(&state.pool, tenant.id, a.id, LIMIT).await?;
    Ok(collection(
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "name": format!("{} {}.", r.first_name, r.last_initial).trim().to_owned(),
                    "earned": dto::money(r.earned),
                    "currency": a.currency.as_str(),
                    "created_at": r.created_at,
                })
            })
            .collect::<Vec<Value>>(),
    ))
}

/// `GET /api/v1/affiliate/payouts` — newest 100.
pub async fn payouts(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    let a = own(&state, &tenant, &auth).await?;
    let rows = arepo::payouts_of(&state.pool, tenant.id, a.id, LIMIT).await?;
    Ok(collection(
        rows.iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "amount": dto::money(p.amount),
                    "currency": p.currency.as_str(),
                    "method": p.method.as_str(),
                    "status": p.status.as_str(),
                    "processed_at": p.processed_at,
                    "created_at": p.created_at,
                })
            })
            .collect::<Vec<Value>>(),
    ))
}
