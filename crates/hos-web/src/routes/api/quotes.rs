//! `/api/v1/quotes` — sales quotes (staff `quotes.view`, client keys their
//! own sent quotes) and the client's answer. Accepting runs
//! `Billing::accept_quote` (order, services, invoice: all in hos-billing);
//! a second accept answers `409`. Totals always come from the stored quote.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use hos_auth::rbac::Permission;
use hos_db::Page;
use hos_db::repo::quotes::{
    self as qrepo, QuoteFilter, QuoteId, QuoteItemRow, QuoteRow, QuoteStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, Scope, collection, dto, parse_body, parse_id,
};
use crate::extract::ClientIp;
use crate::state::AppState;

/// The API view of a quote (staff notes and the link token never leave).
pub fn quote_json(q: &QuoteRow, items: Option<&[QuoteItemRow]>) -> Value {
    let mut v = json!({
        "id": q.id,
        "number": q.number(),
        "client_id": q.client_id,
        "subject": q.subject,
        "status": q.status.as_str(),
        "currency": q.currency.as_str(),
        "valid_until": q.valid_until,
        "subtotal": dto::money(q.subtotal),
        "discount_total": dto::money(q.discount_total),
        "tax_total": dto::money(q.tax_total),
        "total": dto::money(q.total),
        "notes": q.notes,
        "sent_at": q.sent_at,
        "accepted_at": q.accepted_at,
        "declined_at": q.declined_at,
        "order_id": q.order_id,
        "invoice_id": q.invoice_id,
        "created_at": q.created_at,
    });
    if let Some(items) = items {
        v["items"] = items
            .iter()
            .map(|i| {
                json!({
                    "description": i.description,
                    "product_id": i.product_id,
                    "billing_cycle": i.billing_cycle.map(|c| c.as_str()),
                    "quantity": i.quantity.normalize().to_string(),
                    "unit_price": dto::money(i.unit_price),
                    "discount_percent": i.discount_percent.normalize().to_string(),
                    "amount": dto::money(i.amount),
                    "taxable": i.taxable,
                })
            })
            .collect();
    }
    v
}

/// A quote the caller may see: staff any, clients their own non-drafts.
async fn visible(
    state: &AppState,
    tenant: &ApiTenant,
    scope: &Scope,
    raw: &str,
) -> ApiResult<QuoteRow> {
    let id: QuoteId = parse_id(raw)?;
    let q = qrepo::get_quote(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if scope.client().is_some() {
        scope.owns(q.client_id)?;
        if q.status == QuoteStatus::Draft {
            return Err(ApiError::not_found());
        }
    }
    Ok(q)
}

/// `GET /api/v1/quotes` — newest 100 (`?status=` filter).
pub async fn list(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::QUOTES_VIEW)?;
    let status = match raw.get("status").map(String::as_str) {
        None | Some("") => None,
        Some(s) => Some(
            s.parse::<QuoteStatus>()
                .map_err(|_| ApiError::field("status", "unknown status"))?,
        ),
    };
    let rows: Vec<QuoteRow> = match scope.client() {
        Some(client) => qrepo::client_quotes(&state.pool, tenant.id, client, 100)
            .await?
            .into_iter()
            .filter(|q| status.is_none_or(|s| q.status == s))
            .collect(),
        None => qrepo::list_quotes(
            &state.pool,
            tenant.id,
            &QuoteFilter {
                status,
                ..QuoteFilter::default()
            },
            "q.created_at desc",
            Page::first(100),
        )
        .await?
        .items
        .into_iter()
        .map(|r| r.quote)
        .collect(),
    };
    Ok(collection(
        rows.iter().map(|q| quote_json(q, None)).collect(),
    ))
}

/// `GET /api/v1/quotes/{id}` — with lines.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::QUOTES_VIEW)?;
    let q = visible(&state, &tenant, &scope, &id).await?;
    let items = qrepo::quote_items(&state.pool, tenant.id, q.id).await?;
    Ok(ApiReply::ok(quote_json(&q, Some(&items))))
}

/// `POST /api/v1/quotes/{id}/accept` — a client key accepts its own quote;
/// staff need `quotes.manage` and `billing.manage` and accept for the
/// quote's client.
pub async fn accept(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    ip: ClientIp,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::QUOTES_MANAGE)?;
    if matches!(scope, Scope::Staff(_)) {
        auth.staff(Permission::BILLING_MANAGE)?;
    }
    let q = visible(&state, &tenant, &scope, &id).await?;
    let client_id = q.client_id.ok_or_else(|| {
        ApiError::from(hos_core::CoreError::Conflict(
            "a prospect quote is accepted through its link".into(),
        ))
    })?;
    let user = match scope {
        Scope::Client { user_id, .. } => Some(user_id),
        Scope::Staff(_) => None,
    };
    let out = state
        .billing
        .accept_quote(tenant.id, q.id, client_id, user, Some(ip.0))
        .await?;
    Ok(ApiReply::created(json!({
        "quote_id": out.quote_id,
        "order_id": out.order_id,
        "invoice_id": out.invoice_id,
        "paid": out.paid,
        "next": if out.paid {
            Value::Null
        } else {
            json!({ "pay": format!("/api/v1/invoices/{}/pay", out.invoice_id) })
        },
    })))
}

/// `POST /api/v1/quotes/{id}/decline` body.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclineBody {
    /// Optional reason (max. 1000 characters).
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/v1/quotes/{id}/decline`.
pub async fn decline(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let input: DeclineBody = parse_body(&body)?;
    let scope = auth.scope(Permission::QUOTES_MANAGE)?;
    let q = visible(&state, &tenant, &scope, &id).await?;
    let reason: Option<String> = input
        .reason
        .map(|r| r.chars().take(1000).collect::<String>())
        .filter(|r| !r.trim().is_empty());
    let row = state
        .billing
        .decline_quote(tenant.id, q.id, scope.client(), reason.as_deref())
        .await?;
    Ok(ApiReply::ok(quote_json(&row, None)))
}
