//! Product catalog: the public storefront catalog and TLD prices (no auth)
//! and the staff product list (`products.manage`, hidden products and all
//! prices included).

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use hos_auth::rbac::Permission;
use hos_core::ids::ProductId;
use hos_core::money::Currency;
use hos_db::repo::{api as api_repo, catalog, domains};
use serde_json::json;

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, Scope, Sorts, dto, list_query, paged,
    parse_id,
};
use crate::state::AppState;

/// `?currency=` or the tenant default.
fn currency(tenant: &ApiTenant, raw: &HashMap<String, String>) -> ApiResult<Currency> {
    match raw
        .get("currency")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        None => Ok(tenant.default_currency),
        Some(c) => c
            .to_ascii_uppercase()
            .parse()
            .map_err(|_| ApiError::field("currency", "unknown ISO 4217 currency code")),
    }
}

/// `GET /api/v1/catalog` — visible product groups with their products and
/// prices in `?currency=`.
pub async fn public_catalog(
    State(state): State<AppState>,
    tenant: ApiTenant,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let cur = currency(&tenant, &raw)?;
    let groups = catalog::catalog(&state.pool, tenant.id, cur, false).await?;
    let mut data = Vec::with_capacity(groups.len());
    for g in &groups {
        let mut products = Vec::with_capacity(g.products.len());
        for p in &g.products {
            let mut v = dto::catalog_product(p, cur.as_str());
            let options =
                catalog::product_config_options(&state.pool, tenant.id, p.product.id, cur).await?;
            v["config_options"] = options
                .iter()
                .filter(|o| !o.option.is_hidden)
                .map(|o| dto::config_option(o, cur.as_str()))
                .collect();
            products.push(v);
        }
        data.push(json!({
            "id": g.group.id,
            "slug": g.group.slug,
            "name": g.group.name,
            "tagline": g.group.tagline,
            "products": products,
        }));
    }
    Ok(ApiReply::ok(
        json!({ "currency": cur.as_str(), "groups": data }),
    ))
}

/// `GET /api/v1/tlds` — active TLDs with register / transfer / renew prices.
pub async fn tlds(
    State(state): State<AppState>,
    tenant: ApiTenant,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let cur = currency(&tenant, &raw)?;
    let rows = domains::tld_price_table(&state.pool, tenant.id, cur).await?;
    Ok(super::collection(
        rows.iter().map(|t| dto::tld(t, cur.as_str())).collect(),
    ))
}

const PRODUCT_SORTS: Sorts = &[
    ("created_at", "created_at"),
    ("name", "name"),
    ("sort_order", "sort_order"),
];

/// `GET /api/v1/products` (`products.manage`; `?retired=1` includes retired).
pub async fn list_products(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let user = auth.staff(Permission::PRODUCTS_MANAGE)?;
    let q = list_query(&raw, PRODUCT_SORTS, &[], Scope::Staff(user))?;
    let retired = matches!(raw.get("retired").map(String::as_str), Some("1" | "true"));
    let page =
        api_repo::list_products(&state.pool, tenant.id, &q.filter, retired, q.sort, q.page).await?;
    Ok(paged(&page, |p| dto::product(p, None)))
}

/// `GET /api/v1/products/{id}` (`products.manage`) — with all prices.
pub async fn get_product(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::PRODUCTS_MANAGE)?;
    let id: ProductId = parse_id(&id)?;
    let p = catalog::get_product(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let prices = catalog::product_pricing(&state.pool, tenant.id, id).await?;
    Ok(ApiReply::ok(dto::product(&p, Some(&prices))))
}
