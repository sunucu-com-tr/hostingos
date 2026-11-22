//! Domains. Staff: `domains.manage`; clients: their own domains.
//! Availability is public (registrar module, RDAP fallback); registration
//! and renewal create orders / invoices through hos-billing.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use hos_auth::rbac::Permission;
use hos_core::domain::IntegrationCategory;
use hos_core::ids::{ClientId, DomainId, IntegrationId};
use hos_db::repo::{api as api_repo, domains as domains_repo};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;

use super::orders::{ItemBody, OrderBody, order_domain};
use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, Scope, Sorts, dto, list_query, paged,
    parse_body, parse_id,
};
use crate::extract::{ClientIp, UserAgent};
use crate::state::AppState;

const SORTS: Sorts = &[
    ("created_at", "created_at"),
    ("name", "name"),
    ("expires_at", "expires_at"),
];
const STATUSES: &[&str] = &[
    "pending",
    "pending_registration",
    "pending_transfer",
    "active",
    "grace",
    "redemption",
    "expired",
    "transferred_away",
    "cancelled",
    "fraud",
];

/// `GET /api/v1/domains`.
pub async fn list(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::DOMAINS_MANAGE)?;
    let q = list_query(&raw, SORTS, STATUSES, scope)?;
    let page = api_repo::list_domains(&state.pool, tenant.id, &q.filter, q.sort, q.page).await?;
    Ok(paged(&page, dto::domain))
}

async fn load(
    state: &AppState,
    tenant: &ApiTenant,
    scope: Scope,
    raw: &str,
) -> ApiResult<hos_db::repo::domains::DomainRow> {
    let id: DomainId = parse_id(raw)?;
    let d = domains_repo::get_domain(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    scope.owns(Some(d.client_id))?;
    Ok(d)
}

/// `GET /api/v1/domains/{id}`.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::DOMAINS_MANAGE)?;
    let d = load(&state, &tenant, scope, &id).await?;
    Ok(ApiReply::ok(dto::domain(&d)))
}

/// `GET /api/v1/domains/availability?domain=example.com` (no auth).
pub async fn availability(
    State(state): State<AppState>,
    tenant: ApiTenant,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let name = hos_db::repo::platform::normalize_host(
        raw.get("domain").map(String::as_str).unwrap_or_default(),
    );
    if name.split('.').count() < 2 || name.len() > 253 {
        return Err(ApiError::field(
            "domain",
            "must be a domain name like example.com",
        ));
    }
    let tld_row = domains_repo::match_tld(&state.pool, tenant.id, &name).await?;
    let Some(tld) = tld_row else {
        return Ok(ApiReply::ok(json!({
            "domain": name,
            "status": "unsupported",
            "available": false,
        })));
    };
    let mut status = "unknown";
    let mut premium: Option<Decimal> = None;
    let mut source = "rdap";
    let registrar_id: Option<IntegrationId> = match tld.registrar_integration_id {
        Some(id) => Some(id),
        None => state
            .billing
            .registry()
            .default_id(tenant.id, IntegrationCategory::Registrar)
            .await
            .unwrap_or(None),
    };
    if let Some(id) = registrar_id
        && let Ok(reg) = state.billing.registry().registrar(tenant.id, id).await
    {
        match reg.check_availability(std::slice::from_ref(&name)).await {
            Ok(list) => {
                if let Some(a) = list.iter().find(|a| a.domain.eq_ignore_ascii_case(&name)) {
                    status = if a.available { "available" } else { "taken" };
                    if a.premium {
                        premium = a.register_price.as_ref().map(|m| m.amount);
                    }
                    source = "registrar";
                }
            }
            Err(e) => tracing::warn!(error = %e, "api: registrar availability failed; using RDAP"),
        }
    }
    if source == "rdap" {
        status = match state.rdap.lookup(&name).await {
            Ok(hos_domains::RdapResult::Available) => "available",
            Ok(hos_domains::RdapResult::Registered(_)) => "taken",
            _ => "unknown",
        };
    }
    let cur = tenant.default_currency;
    let price = domains_repo::tld_price_table(&state.pool, tenant.id, cur)
        .await?
        .into_iter()
        .find(|r| r.tld_id == tld.id);
    Ok(ApiReply::ok(json!({
        "domain": name,
        "tld": tld.tld,
        "status": status,
        "available": status == "available",
        "premium": premium.is_some(),
        "source": source,
        "currency": cur.as_str(),
        "register": premium.or_else(|| price.as_ref().and_then(|p| p.register)).map(dto::money),
        "transfer": price.as_ref().and_then(|p| p.transfer).map(dto::money),
        "renew": price.as_ref().and_then(|p| p.renew).map(dto::money),
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterBody {
    domain: String,
    #[serde(default)]
    years: Option<i16>,
    /// `register` (default) or `transfer`.
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    client_id: Option<ClientId>,
    #[serde(default)]
    payment_integration_id: Option<IntegrationId>,
    #[serde(default)]
    currency: Option<String>,
}

/// `POST /api/v1/domains/register` — an order with one domain line.
pub async fn register(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    ip: ClientIp,
    ua: UserAgent,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let input: RegisterBody = parse_body(&body)?;
    let order = OrderBody {
        client_id: input.client_id,
        currency: input.currency,
        items: vec![ItemBody {
            product_id: None,
            billing_cycle: None,
            quantity: None,
            hostname: None,
            config_options: None,
            domain: Some(input.domain),
            action: input.action,
            years: input.years,
        }],
        coupon_code: None,
        payment_integration_id: input.payment_integration_id,
        notes: None,
    };
    let out = order_domain(&state, &tenant, &auth, order, &ip, &ua)
        .await
        .map_err(|mut e| {
            // Report `items[0].domain` as `domain` for this endpoint.
            for f in &mut e.errors {
                if let Some(rest) = f.field.strip_prefix("items[0].") {
                    f.field = rest.to_owned();
                }
            }
            e
        })?;
    Ok(ApiReply::created(out))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenewBody {
    #[serde(default)]
    years: Option<i16>,
}

/// `POST /api/v1/domains/{id}/renew` — creates a renewal invoice.
pub async fn renew(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::DOMAINS_MANAGE)?;
    let d = load(&state, &tenant, scope, &id).await?;
    let input: RenewBody = parse_body(&body)?;
    let years = input.years.unwrap_or(1);
    if !(1..=10).contains(&years) {
        return Err(ApiError::field("years", "must be 1–10"));
    }
    let invoice_id = state
        .billing
        .order_domain_renewal(tenant.id, d.id, scope.client(), years)
        .await?;
    Ok(ApiReply::created(json!({
        "domain_id": d.id,
        "years": years,
        "invoice_id": invoice_id,
        "next": { "pay": format!("/api/v1/invoices/{invoice_id}/pay") },
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NameserversBody {
    nameservers: Vec<String>,
}

/// `PUT /api/v1/domains/{id}/nameservers` — 2–13 host names.
pub async fn nameservers(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::DOMAINS_MANAGE)?;
    let d = load(&state, &tenant, scope, &id).await?;
    let input: NameserversBody = parse_body(&body)?;
    let ns: Vec<String> = input
        .nameservers
        .iter()
        .map(|n| hos_db::repo::platform::normalize_host(n))
        .filter(|n| !n.is_empty())
        .collect();
    if !(2..=13).contains(&ns.len()) || ns.iter().any(|n| !n.contains('.') || n.len() > 253) {
        return Err(ApiError::field(
            "nameservers",
            "2–13 host names like ns1.example.com",
        ));
    }
    state
        .billing
        .domain_set_nameservers(tenant.id, d.id, scope.client(), &ns)
        .await?;
    Ok(ApiReply::ok(
        json!({ "domain_id": d.id, "nameservers": ns }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockBody {
    locked: bool,
}

/// `PUT /api/v1/domains/{id}/lock` — registrar (transfer) lock.
pub async fn lock(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::DOMAINS_MANAGE)?;
    let d = load(&state, &tenant, scope, &id).await?;
    let input: LockBody = parse_body(&body)?;
    state
        .billing
        .domain_set_lock(tenant.id, d.id, scope.client(), input.locked)
        .await?;
    Ok(ApiReply::ok(
        json!({ "domain_id": d.id, "locked": input.locked }),
    ))
}
