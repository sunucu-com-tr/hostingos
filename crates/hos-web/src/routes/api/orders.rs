//! `/api/v1/orders` — list / show (staff `billing.view`, clients their own),
//! quote and create. Creating an order builds a cart from the items and runs
//! `Billing::checkout` (prices, coupon, tax, invoice, provisioning queue all
//! stay in hos-billing).

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use hos_auth::rbac::Permission;
use hos_billing::{CheckoutRequest, QuoteItem, QuoteRequest};
use hos_core::ids::{ClientId, IntegrationId, OrderId, ProductId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::LineItemType;
use hos_db::repo::carts::{self, NewCartItem};
use hos_db::repo::{api as api_repo, catalog, clients, orders};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, FieldError, Scope, Sorts, dto, list_query,
    paged, parse_body, parse_id,
};
use crate::extract::{ClientIp, UserAgent};
use crate::routes::platform::limits::{Limit, check_plan_limit};
use crate::state::AppState;

const SORTS: Sorts = &[("created_at", "created_at"), ("total", "total")];
const STATUSES: &[&str] = &["pending", "accepted", "cancelled", "fraud"];

/// `GET /api/v1/orders`.
pub async fn list(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::BILLING_VIEW)?;
    let q = list_query(&raw, SORTS, STATUSES, scope)?;
    let page = api_repo::list_orders(&state.pool, tenant.id, &q.filter, q.sort, q.page).await?;
    Ok(paged(&page, |o| dto::order(o, None)))
}

/// `GET /api/v1/orders/{id}` — with items.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::BILLING_VIEW)?;
    let id: OrderId = parse_id(&id)?;
    let o = orders::get_order(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    scope.owns(Some(o.client_id))?;
    let items = orders::order_items(&state.pool, tenant.id, id).await?;
    Ok(ApiReply::ok(dto::order(&o, Some(&items))))
}

/// One requested line.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemBody {
    /// Product (hosting, VPS …).
    #[serde(default)]
    pub product_id: Option<ProductId>,
    /// `monthly`, `annual` … (products).
    #[serde(default)]
    pub billing_cycle: Option<String>,
    /// Quantity (products allowing it).
    #[serde(default)]
    pub quantity: Option<i32>,
    /// Hostname for servers.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Configurable options (products):
    /// `{ "<option id>": { "value_id": "<value id>", "quantity": 1 } }`,
    /// ids from `GET /api/v1/catalog`.
    #[serde(default)]
    pub config_options: Option<Value>,
    /// Domain lines: the domain to register / transfer. Product lines: the
    /// domain the service is for (hosting).
    #[serde(default)]
    pub domain: Option<String>,
    /// `register` (default) or `transfer` (domain lines).
    #[serde(default)]
    pub action: Option<String>,
    /// Years (domain lines, default 1).
    #[serde(default)]
    pub years: Option<i16>,
}

/// `POST /api/v1/orders` / `/quote` body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderBody {
    /// Required for staff keys; client keys always order for themselves.
    #[serde(default)]
    pub client_id: Option<ClientId>,
    /// Defaults to the client's currency.
    #[serde(default)]
    pub currency: Option<String>,
    /// Lines.
    pub items: Vec<ItemBody>,
    /// Coupon code.
    #[serde(default)]
    pub coupon_code: Option<String>,
    /// Gateway integration for the invoice.
    #[serde(default)]
    pub payment_integration_id: Option<IntegrationId>,
    /// Order notes.
    #[serde(default)]
    pub notes: Option<String>,
}

/// A validated line.
#[derive(Debug, Clone)]
pub(super) enum Line {
    Product {
        id: ProductId,
        cycle: BillingCycle,
        quantity: i32,
        hostname: Option<String>,
        domain: Option<String>,
        config: Value,
    },
    Domain {
        kind: LineItemType,
        name: String,
        years: i16,
    },
}

fn validate_items(items: &[ItemBody]) -> ApiResult<Vec<Line>> {
    let mut errors = Vec::new();
    let mut out = Vec::new();
    if items.is_empty() {
        return Err(ApiError::field("items", "at least one item is required"));
    }
    if items.len() > 50 {
        return Err(ApiError::field("items", "at most 50 items"));
    }
    for (i, it) in items.iter().enumerate() {
        let mut err = |f: &str, m: &str| {
            errors.push(FieldError {
                field: format!("items[{i}].{f}"),
                message: m.to_owned(),
            });
        };
        let valid_domain = |d: &str| {
            let name = hos_db::repo::platform::normalize_host(d);
            (name.split('.').count() >= 2 && name.len() <= 253).then_some(name)
        };
        match (it.product_id, it.domain.as_deref()) {
            (Some(id), product_domain) => {
                let cycle = match it.billing_cycle.as_deref() {
                    Some(c) => match c.parse::<BillingCycle>() {
                        Ok(c) => c,
                        Err(_) => {
                            err("billing_cycle", "unknown billing cycle");
                            continue;
                        }
                    },
                    None => {
                        err("billing_cycle", "required for product items");
                        continue;
                    }
                };
                let quantity = it.quantity.unwrap_or(1);
                if !(1..=100).contains(&quantity) {
                    err("quantity", "must be 1–100");
                    continue;
                }
                let domain = match product_domain.map(str::trim).filter(|d| !d.is_empty()) {
                    None => None,
                    Some(d) => match valid_domain(d) {
                        Some(n) => Some(n),
                        None => {
                            err("domain", "must be a domain name like example.com");
                            continue;
                        }
                    },
                };
                let config = match &it.config_options {
                    None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
                    Some(v @ Value::Object(_)) => v.clone(),
                    Some(_) => {
                        err("config_options", "must be an object keyed by option id");
                        continue;
                    }
                };
                out.push(Line::Product {
                    id,
                    cycle,
                    quantity,
                    domain,
                    config,
                    hostname: it
                        .hostname
                        .as_deref()
                        .map(str::trim)
                        .filter(|h| !h.is_empty())
                        .map(str::to_owned),
                });
            }
            (None, Some(d)) => {
                let Some(name) = valid_domain(d) else {
                    err("domain", "must be a domain name like example.com");
                    continue;
                };
                let kind = match it.action.as_deref().unwrap_or("register") {
                    "register" => LineItemType::DomainRegister,
                    "transfer" => LineItemType::DomainTransfer,
                    _ => {
                        err("action", "must be `register` or `transfer`");
                        continue;
                    }
                };
                let years = it.years.unwrap_or(1);
                if !(1..=10).contains(&years) {
                    err("years", "must be 1–10");
                    continue;
                }
                out.push(Line::Domain { kind, name, years });
            }
            (None, None) => err(
                "product_id",
                "give `product_id` (product) or `domain` (domain)",
            ),
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    Ok(out)
}

/// Resolves the ordering client: own client for client keys, `client_id`
/// (required, must exist) for staff with `billing.manage`.
async fn ordering_client(
    state: &AppState,
    tenant: &ApiTenant,
    auth: &ApiAuth,
    requested: Option<ClientId>,
) -> ApiResult<(Scope, hos_db::repo::clients::ClientRow)> {
    let scope = auth.scope(Permission::BILLING_MANAGE)?;
    let client_id = match scope {
        Scope::Client { client_id, .. } => client_id,
        Scope::Staff(_) => requested.ok_or_else(|| ApiError::field("client_id", "required"))?,
    };
    let client = clients::get_client(&state.pool, tenant.id, client_id)
        .await?
        .ok_or_else(|| ApiError::field("client_id", "unknown client"))?;
    Ok((scope, client))
}

fn order_currency(raw: Option<&str>, fallback: Currency) -> ApiResult<Currency> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(fallback),
        Some(c) => c
            .to_ascii_uppercase()
            .parse()
            .map_err(|_| ApiError::field("currency", "unknown ISO 4217 code")),
    }
}

/// `POST /api/v1/orders/quote` — prices without creating anything.
pub async fn quote(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let input: OrderBody = parse_body(&body)?;
    let lines = validate_items(&input.items)?;
    let (_, client) = ordering_client(&state, &tenant, &auth, input.client_id).await?;
    let currency = order_currency(input.currency.as_deref(), client.currency)?;
    let items = lines
        .iter()
        .map(|l| match l {
            Line::Product {
                id,
                cycle,
                quantity,
                config,
                ..
            } => QuoteItem {
                quantity: *quantity,
                config_selections: config.clone(),
                ..QuoteItem::product(*id, *cycle)
            },
            Line::Domain { kind, name, years } => QuoteItem::domain(*kind, name, *years),
        })
        .collect();
    let q = state
        .billing
        .quote(
            tenant.id,
            &QuoteRequest {
                currency,
                items,
                coupon_code: input.coupon_code.clone(),
                client_id: Some(client.id),
                country_code: Some(client.country_code.clone()),
                region: client.state.clone(),
            },
        )
        .await?;
    Ok(ApiReply::ok(dto::quote(&q)))
}

/// Builds a cart for `client` from `lines` and checks it out.
pub(super) async fn place_order(
    state: &AppState,
    tenant: &ApiTenant,
    scope: Scope,
    client: &hos_db::repo::clients::ClientRow,
    input: &OrderBody,
    lines: &[Line],
    origin: (&ClientIp, &UserAgent),
) -> ApiResult<Value> {
    let (ip, ua) = origin;
    if lines.iter().any(|l| matches!(l, Line::Product { .. })) {
        check_plan_limit(state, tenant.id, Limit::Services).await?;
    }
    let currency = order_currency(input.currency.as_deref(), client.currency)?;
    let cart = carts::create_cart(
        &state.pool,
        tenant.id,
        currency,
        Some(scope.user()),
        Some(client.id),
        None,
    )
    .await?;
    for line in lines {
        let item = match line {
            Line::Product {
                id,
                cycle,
                quantity,
                hostname,
                domain,
                config,
            } => NewCartItem {
                quantity: *quantity,
                hostname: hostname.clone(),
                domain_name: domain.clone(),
                config_selections: config.clone(),
                ..NewCartItem::product(*id, *cycle)
            },
            Line::Domain { kind, name, years } => NewCartItem::domain(*kind, name, *years),
        };
        carts::add_item(&state.pool, tenant.id, cart.id, &item).await?;
    }
    if let Some(code) = input
        .coupon_code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let coupon = catalog::redeemable_coupon(&state.pool, tenant.id, code)
            .await?
            .ok_or_else(|| ApiError::field("coupon_code", "unknown or expired coupon"))?;
        if hos_db::repo::bundles::is_bundle_coupon(&state.pool, tenant.id, coupon.id).await? {
            return Err(ApiError::field(
                "coupon_code",
                "this code belongs to a product bundle and only applies through the bundle's order button",
            ));
        }
        carts::set_cart_coupon(&state.pool, tenant.id, cart.id, Some((coupon.id, code))).await?;
    }
    let outcome = state
        .billing
        .checkout(
            tenant.id,
            &CheckoutRequest {
                cart_id: cart.id,
                client_id: client.id,
                user_id: Some(scope.user()),
                payment_integration_id: input.payment_integration_id,
                ip: Some(ip.0),
                user_agent: ua.0.clone(),
                notes: input
                    .notes
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned),
            },
        )
        .await?;
    Ok(json!({
        "order_id": outcome.order_id,
        "order_number": outcome.order_number,
        "invoice_id": outcome.invoice_id,
        "invoice_number": outcome.invoice_number,
        "service_ids": outcome.services,
        "domain_ids": outcome.domains,
        "currency": outcome.quote.currency.as_str(),
        "total": dto::money(outcome.quote.total),
        "credit_applied": dto::money(outcome.credit_applied),
        "balance": dto::money(outcome.balance),
        "paid": outcome.paid,
        "quote": dto::quote(&outcome.quote),
        "next": if outcome.paid {
            Value::Null
        } else {
            json!({ "pay": format!("/api/v1/invoices/{}/pay", outcome.invoice_id) })
        },
    }))
}

/// `POST /api/v1/orders` — `201` with order, invoice and created services.
pub async fn create(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    ip: ClientIp,
    ua: UserAgent,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let input: OrderBody = parse_body(&body)?;
    let lines = validate_items(&input.items)?;
    let (scope, client) = ordering_client(&state, &tenant, &auth, input.client_id).await?;
    let out = place_order(&state, &tenant, scope, &client, &input, &lines, (&ip, &ua)).await?;
    Ok(ApiReply::created(out))
}

/// `POST /api/v1/domains/register` shares the order path.
pub(super) async fn order_domain(
    state: &AppState,
    tenant: &ApiTenant,
    auth: &ApiAuth,
    input: OrderBody,
    ip: &ClientIp,
    ua: &UserAgent,
) -> ApiResult<Value> {
    let lines = validate_items(&input.items)?;
    let (scope, client) = ordering_client(state, tenant, auth, input.client_id).await?;
    place_order(state, tenant, scope, &client, &input, &lines, (ip, ua)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(json: Value) -> ItemBody {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn items_are_validated_with_paths() {
        let pid = ProductId::new();
        let ok = validate_items(&[
            item(json!({ "product_id": pid, "billing_cycle": "monthly" })),
            item(json!({ "domain": "Example.COM", "years": 2 })),
        ])
        .unwrap();
        assert_eq!(ok.len(), 2);
        match &ok[1] {
            Line::Domain { name, years, kind } => {
                assert_eq!(name, "example.com");
                assert_eq!(*years, 2);
                assert_eq!(*kind, LineItemType::DomainRegister);
            }
            Line::Product { .. } => panic!("expected domain"),
        }
        let err = validate_items(&[
            item(json!({ "product_id": pid })),
            item(json!({ "domain": "nodot" })),
            item(json!({})),
        ])
        .unwrap_err();
        let fields: Vec<_> = err.errors.iter().map(|e| e.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "items[0].billing_cycle",
                "items[1].domain",
                "items[2].product_id"
            ]
        );
        assert!(validate_items(&[]).is_err());
    }
}
