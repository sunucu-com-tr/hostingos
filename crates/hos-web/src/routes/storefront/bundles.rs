//! Product bundles on the storefront: `/bundles` lists the active bundles
//! with their price before and after the bundle discount (computed by the
//! pricing engine in the visitor's currency); `POST /bundles/{slug}/order`
//! puts every product of the bundle into the cart with the bundle's coupon.
//! The coupon only applies while every product is in the cart
//! (`requires_all_products`), so it cannot be reused for one product.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use hos_billing::pricing::{QuoteItem, QuoteRequest};
use hos_core::money::Currency;
use hos_db::enums::LineItemType;
use hos_db::repo::bundles::{self as brepo, BundleItemRow, BundleRow};
use hos_db::repo::carts::{self, NewCartItem};
use hos_db::repo::catalog;
use http::HeaderMap;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::extract::ClientIp;
use crate::flash::{self, SetFlash};
use crate::routes::cart::resolve_cart;
use crate::seo::{self, SeoPage};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// A bundle with its prices in the page currency.
pub struct BundleView {
    pub bundle: BundleRow,
    pub items: Vec<(BundleItemRow, String)>,
    /// Sum of the list prices (first period, before tax).
    pub list_total: String,
    /// After the bundle discount (first period, before tax).
    pub bundle_total: String,
    pub saving: String,
}

/// Active storefront bundles priced in `currency` (bundles with a product
/// not sold in that currency are left out).
pub async fn bundle_views(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    currency: Currency,
) -> AppResult<Vec<BundleView>> {
    let mut out = Vec::new();
    for b in brepo::storefront(&state.pool, tenant.id).await? {
        let items = brepo::items(&state.pool, tenant.id, b.id).await?;
        if items.len() < 2 {
            continue;
        }
        let req = QuoteRequest {
            currency,
            items: items
                .iter()
                .map(|i| QuoteItem::product(i.product_id, i.billing_cycle))
                .collect(),
            coupon_code: Some(b.coupon_code.clone()),
            client_id: None,
            country_code: None,
            region: None,
        };
        let Ok(quote) = state.billing.quote(tenant.id, &req).await else {
            continue;
        };
        if quote.coupon.is_none() {
            continue;
        }
        let net = quote.subtotal;
        let list = net + quote.discount_total;
        let lines = items
            .into_iter()
            .zip(quote.lines.iter())
            .map(|(i, l)| {
                let price = format!(
                    "{} {}",
                    ctx.money_in(&(l.amount + l.discount), &currency),
                    ctx.per(&i.billing_cycle)
                );
                (i, price)
            })
            .collect();
        out.push(BundleView {
            list_total: ctx.money_in(&list, &currency),
            bundle_total: ctx.money_in(&net, &currency),
            saving: ctx.money_in(&quote.discount_total.max(Decimal::ZERO), &currency),
            items: lines,
            bundle: b,
        });
    }
    Ok(out)
}

#[derive(Template)]
#[template(path = "storefront/bundles.html")]
struct BundlesPage {
    ctx: PageCtx,
    title: String,
    bundles: Vec<BundleView>,
}
crate::page!(BundlesPage);

/// `GET /bundles`.
pub(super) async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let bundles = bundle_views(&state, &tenant, &ctx, ctx.currency).await?;
    let title = ctx.t("bundles.title");
    let page = SeoPage::new(title.clone(), "/bundles")
        .description(ctx.t("bundles.lead"))
        .crumb(title.clone(), "/bundles");
    seo::apply(&mut ctx, page).await?;
    render(&BundlesPage {
        ctx,
        title,
        bundles,
    })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct OrderForm {}

/// `POST /bundles/{slug}/order`.
pub(super) async fn order(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    headers: HeaderMap,
    Path(slug): Path<String>,
    ctx: PageCtx,
    Form(_f): Form<OrderForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.public_token, &ip.key("bundle"))?;
    let bundle = brepo::by_slug(&state.pool, tenant.id, slug.trim())
        .await?
        .filter(|b| b.is_active && b.show_on_storefront)
        .ok_or(AppError::NotFound)?;
    let items = brepo::items(&state.pool, tenant.id, bundle.id).await?;
    let coupon = hos_db::repo::admin::get_coupon(&state.pool, tenant.id, bundle.coupon_id)
        .await?
        .filter(|c| c.is_active)
        .ok_or(AppError::NotFound)?;
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
    let cart = resolved.require()?.clone();
    let back = ctx.url("/bundles");
    // Every product must be sold in the cart currency and cycle.
    for i in &items {
        let priced = catalog::product_pricing(&state.pool, tenant.id, i.product_id)
            .await?
            .iter()
            .any(|p| {
                p.currency == cart.currency && p.billing_cycle == i.billing_cycle && p.is_enabled
            });
        if !priced {
            return Ok(resolved.apply(flash::redirect(
                &back,
                SetFlash::error(ctx.t("bundles.not_available")),
            )));
        }
    }
    let existing = carts::cart_items(&state.pool, tenant.id, cart.id).await?;
    for i in &items {
        if existing
            .iter()
            .any(|e| e.product_id == Some(i.product_id) && e.billing_cycle == Some(i.billing_cycle))
        {
            continue;
        }
        carts::add_item(
            &state.pool,
            tenant.id,
            cart.id,
            &NewCartItem {
                parent_item_id: None,
                item_type: LineItemType::Product,
                product_id: Some(i.product_id),
                addon_id: None,
                domain_name: None,
                billing_cycle: Some(i.billing_cycle),
                years: None,
                quantity: 1,
                config_selections: json!({}),
                custom_fields: json!({}),
                hostname: None,
                epp_code_enc: None,
            },
        )
        .await?;
    }
    carts::set_cart_coupon(
        &state.pool,
        tenant.id,
        cart.id,
        Some((coupon.id, coupon.code.as_str())),
    )
    .await?;
    Ok(resolved.apply(
        flash::redirect(
            &ctx.url("/cart"),
            SetFlash::success(ctx.tv("bundles.added", "name", &bundle.name)),
        )
        .into_response(),
    ))
}
