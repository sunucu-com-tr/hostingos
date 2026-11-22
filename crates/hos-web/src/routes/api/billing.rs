//! Invoices and transactions. Staff need `billing.view` (read) /
//! `billing.manage` (manual payments, paying on a client's behalf); client
//! keys see and pay their own invoices.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use hos_auth::rbac::Permission;
use hos_billing::StartPayment;
use hos_billing::payments::ManualPayment;
use hos_core::ids::{IntegrationId, InvoiceId};
use hos_db::repo::{api as api_repo, invoices, payments};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, Scope, Sorts, dto, list_query, paged,
    parse_body, parse_id,
};
use crate::extract::ClientIp;
use crate::state::AppState;

const INVOICE_SORTS: Sorts = &[
    ("created_at", "created_at"),
    ("due_date", "due_date"),
    ("issue_date", "issue_date"),
    ("total", "total"),
    ("balance", "balance"),
];
const INVOICE_STATUSES: &[&str] = &[
    "draft",
    "unpaid",
    "payment_pending",
    "paid",
    "partially_refunded",
    "refunded",
    "cancelled",
    "collections",
];

/// `GET /api/v1/invoices`.
pub async fn list_invoices(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::BILLING_VIEW)?;
    let q = list_query(&raw, INVOICE_SORTS, INVOICE_STATUSES, scope)?;
    let page = api_repo::list_invoices(&state.pool, tenant.id, &q.filter, q.sort, q.page).await?;
    Ok(paged(&page, |i| dto::invoice(i, None, None)))
}

async fn load_invoice(
    state: &AppState,
    tenant: &ApiTenant,
    scope: Scope,
    raw: &str,
) -> ApiResult<hos_db::repo::invoices::InvoiceRow> {
    let id: InvoiceId = parse_id(raw)?;
    let inv = invoices::get_invoice(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    // Drafts are staff-only.
    if scope.client().is_some() && inv.status.as_str() == "draft" {
        return Err(ApiError::not_found());
    }
    scope.owns(Some(inv.client_id))?;
    Ok(inv)
}

/// `GET /api/v1/invoices/{id}` — with items and transactions.
pub async fn show_invoice(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::BILLING_VIEW)?;
    let inv = load_invoice(&state, &tenant, scope, &id).await?;
    let items = invoices::invoice_items(&state.pool, tenant.id, inv.id).await?;
    let txns = payments::invoice_transactions(&state.pool, tenant.id, inv.id).await?;
    Ok(ApiReply::ok(dto::invoice(&inv, Some(&items), Some(&txns))))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PayBody {
    #[serde(default)]
    integration_id: Option<IntegrationId>,
    #[serde(default)]
    return_url: Option<String>,
    #[serde(default)]
    cancel_url: Option<String>,
    /// Installment count (gateways that take it from the merchant).
    #[serde(default)]
    installments: Option<u32>,
}

/// Tenant setting: extra hosts (exact names) payment return / cancel URLs
/// may point to, besides the tenant's own hosts (a JSON array of names, or
/// one string with names separated by commas, spaces or new lines).
pub const RETURN_URL_HOSTS_SETTING: &str = "payments.return_url_hosts";

/// Hosts a payment return / cancel URL may point to: the tenant's platform
/// subdomain, its primary domain, its verified custom domains and the
/// [`RETURN_URL_HOSTS_SETTING`] allow-list. The gateway return signer signs
/// the URL and the webhook endpoint redirects to it, so anything else
/// would be a signed open redirect.
pub async fn allowed_return_hosts(
    state: &AppState,
    tenant: &crate::tenant::Tenant,
) -> ApiResult<Vec<String>> {
    let mut hosts = vec![format!("{}.{}", tenant.slug, state.config.base_domain)];
    hosts.extend(tenant.primary_domain.clone());
    for d in hos_db::repo::platform::list_tenant_domains(&state.pool, tenant.id).await? {
        if d.kind == hos_db::enums::TenantDomainKind::Subdomain || d.verified_at.is_some() {
            hosts.push(d.host);
        }
    }
    match hos_db::repo::platform::get_setting(&state.pool, tenant.id, RETURN_URL_HOSTS_SETTING)
        .await?
    {
        Some(serde_json::Value::Array(items)) => {
            hosts.extend(items.iter().filter_map(|v| v.as_str()).map(str::to_owned));
        }
        Some(serde_json::Value::String(list)) => hosts.extend(
            list.split(|c: char| c == ',' || c.is_whitespace())
                .filter(|h| !h.is_empty())
                .map(str::to_owned),
        ),
        _ => {}
    }
    Ok(hosts
        .into_iter()
        .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect())
}

/// An absolute http(s) URL on one of `hosts` (no user info).
fn check_url(field: &str, raw: Option<String>, hosts: &[String]) -> ApiResult<Option<String>> {
    match raw.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(u) => match url::Url::parse(&u) {
            Ok(p) if matches!(p.scheme(), "http" | "https") => {
                let host = p
                    .host_str()
                    .map(|h| h.trim_end_matches('.').to_ascii_lowercase());
                if !p.username().is_empty()
                    || p.password().is_some()
                    || !host.is_some_and(|h| hosts.contains(&h))
                {
                    return Err(ApiError::field(
                        field,
                        "must be a URL on one of the shop's own domains (or an allowed return host)",
                    ));
                }
                Ok(Some(u))
            }
            _ => Err(ApiError::field(field, "must be an absolute http(s) URL")),
        },
    }
}

/// `POST /api/v1/invoices/{id}/pay` — starts a gateway payment and returns
/// how to continue (redirect URL, form to post, offline instructions).
pub async fn pay(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    ip: ClientIp,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::BILLING_MANAGE)?;
    let inv = load_invoice(&state, &tenant, scope, &id).await?;
    let input: PayBody = parse_body(&body)?;
    let hosts = if input.return_url.is_some() || input.cancel_url.is_some() {
        allowed_return_hosts(&state, &tenant).await?
    } else {
        Vec::new()
    };
    let started = state
        .billing
        .start_payment(
            tenant.id,
            inv.id,
            &StartPayment {
                integration_id: input.integration_id,
                client_id: scope.client(),
                return_url: check_url("return_url", input.return_url, &hosts)?,
                cancel_url: check_url("cancel_url", input.cancel_url, &hosts)?,
                ip: Some(ip.0),
                save_payment_method: false,
                installments: input.installments,
            },
        )
        .await?;
    Ok(ApiReply::ok(json!({
        "invoice_id": inv.id,
        "reference": started.reference,
        "integration_id": started.integration_id,
        "amount": dto::money(started.amount.amount),
        "currency": started.amount.currency.as_str(),
        "session": dto::checkout_session(&started.session),
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManualBody {
    amount: String,
    #[serde(default)]
    integration_id: Option<IntegrationId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    occurred_at: Option<DateTime<Utc>>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/v1/invoices/{id}/payments` (`billing.manage`) — records an
/// offline payment (bank transfer, cash …).
pub async fn manual_payment(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let user = auth.staff(Permission::BILLING_MANAGE)?;
    let inv = load_invoice(&state, &tenant, Scope::Staff(user), &id).await?;
    let input: ManualBody = parse_body(&body)?;
    let amount: Decimal = input
        .amount
        .trim()
        .parse()
        .ok()
        .filter(|d: &Decimal| *d > Decimal::ZERO && d.scale() <= 2)
        .ok_or_else(|| ApiError::field("amount", "a positive decimal string like \"100.00\""))?;
    let clean = |s: Option<String>| s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let outcome = state
        .billing
        .record_manual_payment(
            tenant.id,
            &ManualPayment {
                invoice_id: inv.id,
                amount,
                integration_id: input.integration_id,
                method: clean(input.method),
                reference: clean(input.reference),
                occurred_at: input.occurred_at,
                note: clean(input.note),
                staff_user_id: Some(user),
            },
        )
        .await?;
    let invoice = match &outcome.invoice {
        Some(i) => dto::invoice(i, None, None),
        None => serde_json::Value::Null,
    };
    Ok(ApiReply::created(json!({
        "transaction": dto::transaction(&outcome.transaction),
        "duplicate": outcome.duplicate,
        "invoice_paid": outcome.invoice_paid,
        "credited": dto::money(outcome.credited),
        "invoice": invoice,
    })))
}

const TXN_SORTS: Sorts = &[
    ("occurred_at", "occurred_at"),
    ("created_at", "created_at"),
    ("amount", "amount"),
];
const TXN_STATUSES: &[&str] = &["pending", "succeeded", "failed"];

/// `GET /api/v1/transactions`.
pub async fn list_transactions(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::BILLING_VIEW)?;
    let q = list_query(&raw, TXN_SORTS, TXN_STATUSES, scope)?;
    let page =
        api_repo::list_transactions(&state.pool, tenant.id, &q.filter, q.sort, q.page).await?;
    Ok(paged(&page, dto::transaction))
}
