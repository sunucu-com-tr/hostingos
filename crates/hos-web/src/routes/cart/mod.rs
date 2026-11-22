//! Shopping cart (`/cart`) and checkout (`/checkout`); guests welcome.
//!
//! A guest's cart is found by the `hos_cart` cookie (random token, stored as
//! its SHA-256 in `carts.token_hash`). Once a client logs in the guest cart
//! is attached to (or merged into) their own open cart. Prices are never
//! computed here: every total comes from `hos_billing::Billing::quote` /
//! `quote_cart`, and additions are validated by quoting them first.
//!
//! | Route | |
//! |---|---|
//! | `GET /cart` | cart page (htmx swaps `#cart` after each change) |
//! | `POST /cart/add` | add a product (simple ones directly) or a domain |
//! | `GET/POST /cart/configure/{slug}` | product configuration ([`configure`]) |
//! | `POST /cart/items/{id}` / `…/remove` | change cycle / years, remove |
//! | `POST /cart/coupon`, `/cart/coupon/remove`, `/cart/currency` | |
//! | `/checkout*` | sign-in, billing details, review, place order ([`checkout`]) |

pub mod checkout;
pub mod configure;

use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, Request, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use hos_billing::{Quote, QuoteItem, QuoteRequest};
use hos_core::CoreError;
use hos_core::ids::{ClientId, TenantId, UserId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::{DomainRequirement, LineItemType};
use hos_db::ids::CartItemId;
use hos_db::repo::carts::{self, CartLine, CartRow, NewCartItem};
use hos_db::repo::{catalog, clients, domains as domain_repo};
use hos_domains::name::DomainName;
use http::header::{COOKIE, SET_COOKIE};
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::extract::Htmx;
use crate::flash::{self, SetFlash};
use crate::routes::client::{friendly_error, no_store};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// Guest cart cookie.
pub const CART_COOKIE: &str = "hos_cart";
const CART_TTL_SECS: i64 = 30 * 86_400;

/// Cart + checkout routes. `/checkout/*` places orders and may spend the
/// account's credit, so a client sub-user needs the `orders` area there.
pub fn router(state: &AppState) -> Router<AppState> {
    let checkout = Router::new()
        .route("/checkout", get(checkout::start))
        .route("/checkout/details", post(checkout::save_details))
        .route("/checkout/review", get(checkout::review))
        .route("/checkout/place", post(checkout::place))
        .route_layer(from_fn_with_state(state.clone(), require_orders_area));
    Router::new()
        .route("/cart", get(show))
        .route("/cart/add", post(add))
        .route(
            "/cart/configure/{slug}",
            get(configure::form).post(configure::submit),
        )
        .route("/cart/configure/{slug}/quote", get(configure::live_quote))
        .route("/cart/domain", post(add_domain))
        .route("/cart/items/{id}", post(update_item))
        .route("/cart/items/{id}/remove", post(remove_item))
        .route("/cart/coupon", post(apply_coupon))
        .route("/cart/coupon/remove", post(remove_coupon))
        .route("/cart/currency", post(set_currency))
        .route("/cart/empty", post(empty_cart))
        .merge(checkout)
}

/// Middleware on `/checkout/*`: client sub-users need the `orders` area.
async fn require_orders_area(State(state): State<AppState>, req: Request, next: Next) -> Response {
    crate::routes::client::require_member_area(&state, "orders", req, next).await
}

// ---------------------------------------------------------------------------
// Cart resolution
// ---------------------------------------------------------------------------

/// The logged-in client (user, client) of the request, if any.
pub fn client_of(ctx: &PageCtx, tenant: TenantId) -> Option<(UserId, ClientId)> {
    ctx.session
        .as_ref()
        .filter(|s| s.is_authenticated())
        .and_then(|s| s.principal.require_client(tenant).ok())
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| hos_auth::session::cookie_value(h, CART_COOKIE).map(str::to_owned))
        .filter(|t| !t.is_empty() && t.len() <= 128)
}

fn set_cart_cookie(state: &AppState, token: &str) -> String {
    hos_auth::session::build_cookie(
        CART_COOKIE,
        token,
        &hos_auth::session::CookieAttributes {
            secure: state.web.secure,
            path: "/",
            max_age_secs: Some(CART_TTL_SECS),
            same_site: hos_auth::session::SameSite::Lax,
        },
        Utc::now(),
    )
}

fn clear_cart_cookie(state: &AppState) -> String {
    hos_auth::session::clear_cookie(CART_COOKIE, state.web.secure, "/")
}

/// The visitor's cart and the cookies the response must set.
pub struct Resolved {
    /// Open cart (none yet for a visitor who never added anything).
    pub cart: Option<CartRow>,
    /// `Set-Cookie` values.
    pub cookies: Vec<String>,
}

impl Resolved {
    /// Adds the cookies to a response.
    pub fn apply(&self, mut res: Response) -> Response {
        for c in &self.cookies {
            if let Ok(v) = HeaderValue::from_str(c) {
                res.headers_mut().append(SET_COOKIE, v);
            }
        }
        no_store(res)
    }

    /// The cart or `404`.
    pub fn require(&self) -> AppResult<&CartRow> {
        self.cart.as_ref().ok_or(AppError::NotFound)
    }
}

async fn active_currency(
    state: &AppState,
    tenant: &Tenant,
    wanted: Currency,
) -> AppResult<Currency> {
    let list = catalog::list_currencies(&state.pool, tenant.id, true).await?;
    Ok(
        if list.iter().any(|c| c.code == wanted) || list.is_empty() {
            wanted
        } else {
            list.iter()
                .find(|c| c.is_default)
                .or(list.first())
                .map_or(tenant.default_currency, |c| c.code)
        },
    )
}

/// Finds (and with `create`, makes) the visitor's cart. A logged-in client's
/// guest cart is attached to the account, or merged into the account's open
/// cart (and the guest cookie cleared).
pub async fn resolve_cart(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    headers: &HeaderMap,
    create: bool,
) -> AppResult<Resolved> {
    let pool = &state.pool;
    let token = cookie_token(headers);
    let guest = match &token {
        Some(t) => carts::cart_by_token(pool, tenant.id, &hos_core::crypto::hash_token(t)).await?,
        None => None,
    };
    let mut cookies = Vec::new();
    if let Some((user_id, client_id)) = client_of(ctx, tenant.id) {
        let guest = guest.filter(|g| g.user_id.is_none_or(|u| u == user_id));
        let mut own = carts::open_cart_for_user(pool, tenant.id, user_id).await?;
        match (&guest, &own) {
            (Some(g), None) => {
                carts::assign_cart_owner(pool, tenant.id, g.id, user_id, Some(client_id)).await?;
                own = carts::get_cart(pool, tenant.id, g.id).await?;
                cookies.push(clear_cart_cookie(state));
            }
            (Some(g), Some(o)) if g.id != o.id => {
                merge_carts(state, tenant.id, g, o).await?;
                own = carts::get_cart(pool, tenant.id, o.id).await?;
                cookies.push(clear_cart_cookie(state));
            }
            (Some(_), Some(_)) => cookies.push(clear_cart_cookie(state)),
            _ => {}
        }
        if let Some(o) = &own
            && o.client_id.is_none()
        {
            carts::assign_cart_owner(pool, tenant.id, o.id, user_id, Some(client_id)).await?;
            own = carts::get_cart(pool, tenant.id, o.id).await?;
        }
        if own.is_none() && create {
            let wanted = clients::get_client(pool, tenant.id, client_id)
                .await?
                .map_or(tenant.default_currency, |c| c.currency);
            let currency = active_currency(state, tenant, wanted).await?;
            own = Some(
                carts::create_cart(
                    pool,
                    tenant.id,
                    currency,
                    Some(user_id),
                    Some(client_id),
                    None,
                )
                .await?,
            );
        }
        return Ok(Resolved { cart: own, cookies });
    }
    // Guests (and staff browsing the store) use the cookie cart; a cart that
    // already belongs to an account is never reused anonymously.
    let guest = guest.filter(|g| g.user_id.is_none());
    if guest.is_none() && create {
        let token = hos_core::crypto::generate_token();
        let currency = active_currency(state, tenant, ctx.currency).await?;
        let cart = carts::create_cart(
            pool,
            tenant.id,
            currency,
            None,
            None,
            Some(&hos_core::crypto::hash_token(&token)),
        )
        .await?;
        cookies.push(set_cart_cookie(state, &token));
        return Ok(Resolved {
            cart: Some(cart),
            cookies,
        });
    }
    Ok(Resolved {
        cart: guest,
        cookies,
    })
}

/// Moves the items (and coupon) of `from` into `into`, skipping domains that
/// are already there, then empties `from`.
async fn merge_carts(
    state: &AppState,
    tenant_id: TenantId,
    from: &CartRow,
    into: &CartRow,
) -> AppResult<()> {
    let mut tx = state.pool.begin().await?;
    let existing = carts::cart_items(&mut *tx, tenant_id, into.id).await?;
    let items = carts::cart_items(&mut *tx, tenant_id, from.id).await?;
    let mut map: HashMap<CartItemId, CartItemId> = HashMap::new();
    let ordered = items
        .iter()
        .filter(|i| i.parent_item_id.is_none())
        .chain(items.iter().filter(|i| i.parent_item_id.is_some()));
    for it in ordered {
        let parent = match it.parent_item_id {
            Some(p) => match map.get(&p) {
                Some(np) => Some(*np),
                None => continue,
            },
            None => None,
        };
        let is_domain = matches!(
            it.item_type,
            LineItemType::DomainRegister | LineItemType::DomainTransfer | LineItemType::DomainRenew
        );
        if is_domain
            && existing
                .iter()
                .any(|e| e.domain_name == it.domain_name && e.item_type == it.item_type)
        {
            continue;
        }
        let new = carts::add_item(
            &mut *tx,
            tenant_id,
            into.id,
            &NewCartItem {
                parent_item_id: parent,
                item_type: it.item_type,
                product_id: it.product_id,
                addon_id: it.addon_id,
                domain_name: it.domain_name.clone(),
                billing_cycle: it.billing_cycle,
                years: it.years,
                quantity: it.quantity,
                config_selections: it.config_selections.clone(),
                custom_fields: it.custom_fields.clone(),
                hostname: it.hostname.clone(),
                epp_code_enc: it.epp_code_enc.clone(),
            },
        )
        .await?;
        map.insert(it.id, new.id);
    }
    if into.coupon_id.is_none()
        && let (Some(id), Some(code)) = (from.coupon_id, from.coupon_code.as_deref())
    {
        carts::set_cart_coupon(&mut *tx, tenant_id, into.id, Some((id, code))).await?;
    }
    carts::clear_cart(&mut *tx, tenant_id, from.id).await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Quoting helpers
// ---------------------------------------------------------------------------

/// Quote line order of `parents` (every root followed by its children), as
/// indices into the item list — mirrors `hos_billing::pricing`.
pub fn parent_first_order(parents: &[Option<usize>]) -> Vec<usize> {
    let mut order = Vec::with_capacity(parents.len());
    for (i, p) in parents.iter().enumerate() {
        if p.is_none() {
            order.push(i);
            order.extend(
                parents
                    .iter()
                    .enumerate()
                    .filter(|(_, pp)| **pp == Some(i))
                    .map(|(j, _)| j),
            );
        }
    }
    order
}

/// A quote request for the cart's items (as `hos_billing` builds it).
pub fn quote_request(
    cart: &CartRow,
    lines: &[CartLine],
    client_id: Option<ClientId>,
) -> QuoteRequest {
    let index: HashMap<CartItemId, usize> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| (l.item.id, i))
        .collect();
    QuoteRequest {
        currency: cart.currency,
        items: lines
            .iter()
            .map(|l| QuoteItem {
                parent: l.item.parent_item_id.and_then(|p| index.get(&p).copied()),
                item_type: l.item.item_type,
                product_id: l.item.product_id,
                addon_id: l.item.addon_id,
                domain_name: l.item.domain_name.clone(),
                billing_cycle: l.item.billing_cycle,
                years: l.item.years,
                quantity: l.item.quantity,
                config_selections: l.item.config_selections.clone(),
            })
            .collect(),
        coupon_code: cart.coupon_code.clone(),
        client_id: client_id.or(cart.client_id),
        country_code: None,
        region: None,
    }
}

/// Runs a quote; user-facing failures become `Err(message)`.
pub async fn try_quote(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    req: &QuoteRequest,
) -> AppResult<Result<Quote, String>> {
    match state.billing.quote(tenant.id, req).await {
        Ok(q) => Ok(Ok(q)),
        Err(e) => user_error(ctx, e).map(Err),
    }
}

/// A business error as a message for the visitor, or the error itself.
pub fn user_error(ctx: &PageCtx, e: CoreError) -> AppResult<String> {
    match friendly_error(ctx, &e) {
        Some(m) => Ok(m),
        None => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// View model
// ---------------------------------------------------------------------------

/// One cart row.
pub struct LineView {
    /// Cart item id.
    pub id: String,
    /// Child row (addon / domain of a product).
    pub child: bool,
    /// Title.
    pub title: String,
    /// Small print (options, hostname, domain…).
    pub details: Vec<String>,
    /// Selected cycle / years value.
    pub selected: String,
    /// `(value, label)` choices: billing cycles or registration years.
    pub choices: Vec<(String, String)>,
    /// `cycle` or `years` (form field name) — empty when not editable.
    pub choice_field: &'static str,
    /// Amount due now.
    pub amount: String,
    /// Setup fee (empty when zero).
    pub setup: String,
    /// Discount (empty when zero).
    pub discount: String,
    /// Renewal ("₺100,00 /ay"; empty for one-off lines).
    pub renews: String,
    /// Can be removed on its own.
    pub removable: bool,
    /// Icon.
    pub icon: &'static str,
}

/// Totals block.
pub struct TotalsView {
    /// Σ lines.
    pub subtotal: String,
    /// Setup fees (empty when zero).
    pub setup: String,
    /// Discount (empty when zero).
    pub discount: String,
    /// `(label, amount)`.
    pub taxes: Vec<(String, String)>,
    /// Due today.
    pub total: String,
    /// Renewal totals ("₺1.200,00 / yıllık").
    pub recurring: Vec<String>,
    /// Prices include tax.
    pub prices_include_tax: bool,
    /// Raw total is zero.
    pub free: bool,
}

/// The cart as shown.
pub struct CartView {
    /// Rows.
    pub lines: Vec<LineView>,
    /// Totals (none when the cart could not be priced).
    pub totals: Option<TotalsView>,
    /// Pricing problem shown above the summary.
    pub error: String,
    /// Applied coupon code (empty: none).
    pub coupon: String,
    /// Why the coupon does not apply.
    pub coupon_error: String,
    /// Cart currency.
    pub currency: String,
    /// Active currencies `(code, label)`.
    pub currencies: Vec<(String, String)>,
    /// Can proceed to checkout.
    pub can_checkout: bool,
}

impl CartView {
    /// No items.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Builds the cart view (quoting it for `client_id`).
pub async fn cart_view(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    cart: Option<&CartRow>,
    client_id: Option<ClientId>,
) -> AppResult<CartView> {
    let currencies: Vec<(String, String)> = catalog::list_currencies(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .map(|c| {
            (
                c.code.as_str().to_owned(),
                format!("{} ({})", c.code.as_str(), c.symbol),
            )
        })
        .collect();
    let Some(cart) = cart else {
        return Ok(CartView {
            lines: Vec::new(),
            totals: None,
            error: String::new(),
            coupon: String::new(),
            coupon_error: String::new(),
            currency: ctx.currency.as_str().to_owned(),
            currencies,
            can_checkout: false,
        });
    };
    let lines = carts::cart_lines(&state.pool, tenant.id, cart.id).await?;
    let mut error = String::new();
    let quote = if lines.is_empty() {
        None
    } else {
        match state
            .billing
            .quote_cart(tenant.id, cart.id, client_id, None)
            .await
        {
            Ok(q) => Some(q),
            Err(e) => {
                error = user_error(ctx, e)?;
                None
            }
        }
    };
    let parents: Vec<Option<usize>> = {
        let index: HashMap<CartItemId, usize> = lines
            .iter()
            .enumerate()
            .map(|(i, l)| (l.item.id, i))
            .collect();
        lines
            .iter()
            .map(|l| l.item.parent_item_id.and_then(|p| index.get(&p).copied()))
            .collect()
    };
    let order = parent_first_order(&parents);
    let cur = cart.currency;
    let mut views = Vec::with_capacity(lines.len());
    for (k, &i) in order.iter().enumerate() {
        let l = &lines[i];
        let q = quote.as_ref().and_then(|q| q.lines.get(k));
        views.push(line_view(state, tenant, ctx, cur, l, q).await?);
    }
    let totals = quote.as_ref().map(|q| totals_view(ctx, q));
    Ok(CartView {
        can_checkout: quote.is_some() && !views.is_empty(),
        coupon_error: quote
            .as_ref()
            .and_then(|q| q.coupon_error.clone())
            .unwrap_or_default(),
        coupon: cart.coupon_code.clone().unwrap_or_default(),
        lines: views,
        totals,
        error,
        currency: cur.as_str().to_owned(),
        currencies,
    })
}

/// Totals of a quote for display.
pub fn totals_view(ctx: &PageCtx, q: &Quote) -> TotalsView {
    let cur = q.currency;
    TotalsView {
        subtotal: ctx.money_in(&q.subtotal, &cur),
        setup: nonzero(ctx, q.setup_total, cur),
        discount: nonzero(ctx, q.discount_total, cur),
        taxes: q
            .taxes
            .iter()
            .map(|t| {
                (
                    format!("{} (%{})", t.name, t.rate.normalize()),
                    ctx.money_in(&t.tax_amount, &cur),
                )
            })
            .collect(),
        total: ctx.money_in(&q.total, &cur),
        recurring: q
            .recurring
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    ctx.money_in(&r.amount, &cur),
                    months_label(ctx, r.months)
                )
            })
            .collect(),
        prices_include_tax: q.prices_include_tax,
        free: q.total.is_zero(),
    }
}

/// Formatted amount, empty when zero.
pub fn nonzero(ctx: &PageCtx, v: rust_decimal::Decimal, cur: Currency) -> String {
    if v.is_zero() {
        String::new()
    } else {
        ctx.money_in(&v, &cur)
    }
}

/// "/ ay", "/ yıl"… for a renewal period in months.
pub fn months_label(ctx: &PageCtx, months: u32) -> String {
    use BillingCycle as C;
    let cycle = match months {
        1 => C::Monthly,
        3 => C::Quarterly,
        6 => C::Semiannual,
        12 => C::Annual,
        24 => C::Biennial,
        36 => C::Triennial,
        _ => return ctx.tn("cart.summary.every_months", i64::from(months)),
    };
    ctx.per(&cycle)
}

async fn line_view(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    cur: Currency,
    l: &CartLine,
    q: Option<&hos_billing::pricing::QuoteLine>,
) -> AppResult<LineView> {
    let it = &l.item;
    let mut details = Vec::new();
    let mut choices = Vec::new();
    let mut choice_field = "";
    let mut selected = String::new();
    let (title, icon) = match it.item_type {
        LineItemType::Product => {
            if let Some(pid) = it.product_id {
                for p in catalog::product_pricing(&state.pool, tenant.id, pid).await? {
                    if p.currency == cur && p.is_enabled {
                        choices.push((
                            p.billing_cycle.as_str().to_owned(),
                            ctx.cycle(&p.billing_cycle),
                        ));
                    }
                }
                if choices.len() > 1 {
                    choice_field = "cycle";
                }
            }
            selected = it
                .billing_cycle
                .map(|c| c.as_str().to_owned())
                .unwrap_or_default();
            if let Some(d) = &it.domain_name {
                details.push(ctx.tv(
                    "cart.line.domain",
                    "domain",
                    hos_domains::name::to_unicode(d),
                ));
            }
            if let Some(h) = &it.hostname {
                details.push(ctx.tv("cart.line.hostname", "hostname", h));
            }
            if it
                .custom_fields
                .get("ssh_keys")
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty())
            {
                details.push(ctx.t("cart.line.ssh_key"));
            }
            if let Some(q) = q {
                for c in &q.config_options {
                    if c.quantity > 1 {
                        details.push(format!(
                            "{}: {} × {}",
                            c.option_name, c.quantity, c.value_label
                        ));
                    } else {
                        details.push(format!("{}: {}", c.option_name, c.value_label));
                    }
                }
            }
            (
                l.product_name
                    .clone()
                    .unwrap_or_else(|| ctx.t("cart.line.product")),
                "server",
            )
        }
        LineItemType::Addon => (
            l.addon_name
                .clone()
                .unwrap_or_else(|| ctx.t("cart.line.addon")),
            "plus",
        ),
        LineItemType::DomainRegister | LineItemType::DomainTransfer | LineItemType::DomainRenew => {
            let name = it
                .domain_name
                .as_deref()
                .map(hos_domains::name::to_unicode)
                .unwrap_or_default();
            let kind = match it.item_type {
                LineItemType::DomainTransfer => "cart.line.transfer",
                LineItemType::DomainRenew => "cart.line.renew",
                _ => "cart.line.register",
            };
            details.push(ctx.t(kind));
            if it.item_type == LineItemType::DomainTransfer && it.epp_code_enc.is_some() {
                details.push(ctx.t("cart.line.epp_given"));
            }
            if let Some(d) = &it.domain_name
                && let Some(tld) = domain_repo::match_tld(&state.pool, tenant.id, d).await?
            {
                let (min, max) = (
                    tld.min_years.max(1),
                    tld.max_years.max(tld.min_years.max(1)),
                );
                if it.item_type == LineItemType::DomainRegister && max > min {
                    choice_field = "years";
                    choices = (min..=max.min(10))
                        .map(|y| (y.to_string(), ctx.tn("cart.line.years", i64::from(y))))
                        .collect();
                }
            }
            selected = it.years.unwrap_or(1).to_string();
            if choice_field.is_empty() {
                details.push(ctx.tn("cart.line.years", i64::from(it.years.unwrap_or(1))));
            }
            (name, "globe")
        }
        LineItemType::Other => (ctx.t("cart.line.other"), "box"),
    };
    let (amount, setup, discount, renews) = match q {
        Some(q) => (
            ctx.money_in(&q.amount, &cur),
            nonzero(ctx, q.setup_fee, cur),
            nonzero(ctx, q.discount, cur),
            match q.billing_cycle {
                Some(c) if c.is_recurring() && !q.recurring_amount.is_zero() => {
                    format!(
                        "{} {}",
                        ctx.money_in(&q.recurring_amount, &cur),
                        ctx.per(&c)
                    )
                }
                _ if q.years.is_some() && !q.recurring_amount.is_zero() => {
                    format!(
                        "{} {}",
                        ctx.money_in(&q.recurring_amount, &cur),
                        months_label(ctx, 12)
                    )
                }
                _ => String::new(),
            },
        ),
        None => (
            String::from("—"),
            String::new(),
            String::new(),
            String::new(),
        ),
    };
    Ok(LineView {
        id: it.id.to_string(),
        child: it.parent_item_id.is_some(),
        title,
        details,
        selected,
        choices,
        choice_field,
        amount,
        setup,
        discount,
        renews,
        removable: true,
        icon,
    })
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "cart/cart.html")]
struct CartPage {
    ctx: PageCtx,
    view: CartView,
    tld_hint: String,
    /// Domain pre-filled for a transfer (`/cart?transfer=`, from `/domains`).
    transfer: String,
}

/// `GET /cart` query.
#[derive(Debug, Default, Deserialize)]
pub struct CartQuery {
    /// Domain to transfer (pre-fills the domain form).
    #[serde(default)]
    transfer: Option<String>,
}
crate::page!(CartPage);

#[derive(Template)]
#[template(path = "cart/_cart.html")]
struct CartFragment {
    ctx: PageCtx,
    view: CartView,
}
crate::page!(CartFragment);

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    Query(q): Query<CartQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let transfer = q
        .transfer
        .as_deref()
        .and_then(|d| DomainName::from_user_input(d).ok())
        .map(|n| n.to_unicode())
        .unwrap_or_default();
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, false).await?;
    let client = client_of(&ctx, tenant.id).map(|(_, c)| c);
    let view = cart_view(&state, &tenant, &ctx, resolved.cart.as_ref(), client).await?;
    ctx.load_store().await?;
    let title = ctx.t("cart.title");
    ctx.set_title(title);
    let tld_hint = domain_repo::list_tlds(&state.pool, tenant.id, true)
        .await
        .map(|l| {
            l.iter()
                .filter(|t| t.is_active)
                .take(6)
                .map(|t| format!(".{}", t.tld))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let res = render(&CartPage {
        ctx,
        view,
        tld_hint,
        transfer,
    })?;
    Ok(resolved.apply(res))
}

/// After a cart change: the refreshed `#cart` fragment for htmx (with the
/// message as a toast), otherwise a redirect to `/cart` with a flash.
async fn after_change(
    state: &AppState,
    tenant: &Tenant,
    ctx: PageCtx,
    htmx: &Htmx,
    resolved: &Resolved,
    msg: SetFlash,
) -> AppResult<Response> {
    if htmx.request {
        let client = client_of(&ctx, tenant.id).map(|(_, c)| c);
        let view = cart_view(state, tenant, &ctx, resolved.cart.as_ref(), client).await?;
        let page = render(&CartFragment { ctx, view })?;
        return Ok(resolved.apply((msg, page).into_response()));
    }
    Ok(resolved.apply(flash::redirect(&ctx.url("/cart"), msg)))
}

// ---------------------------------------------------------------------------
// Adding
// ---------------------------------------------------------------------------

/// `POST /cart/add` (storefront buttons).
#[derive(Debug, Deserialize)]
pub struct AddForm {
    product: Option<String>,
    cycle: Option<String>,
    domain: Option<String>,
    /// `register` (default) / `transfer`.
    action: Option<String>,
    years: Option<i16>,
    epp: Option<String>,
    return_to: Option<String>,
}

/// A same-site path to go back to (`/cart` otherwise).
fn back_to(ctx: &PageCtx, raw: Option<&str>) -> String {
    raw.and_then(crate::locale::safe_local_target)
        .map(str::to_owned)
        .unwrap_or_else(|| ctx.url("/cart"))
}

/// Whether a product needs the configuration page before it can be added.
pub fn needs_configuration(
    product: &catalog::ProductRow,
    options: &[catalog::ProductConfigOption],
    addons: &[catalog::CatalogAddon],
) -> bool {
    use hos_core::domain::ProductType as P;
    options.iter().any(|o| !o.option.is_hidden)
        || addons
            .iter()
            .any(|a| a.addon.show_on_order && !a.addon.is_hidden)
        || product.requires_domain != DomainRequirement::None
        || matches!(product.product_type, P::Vps | P::Cloud | P::Dedicated)
}

async fn add(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    ctx: PageCtx,
    Form(f): Form<AddForm>,
) -> AppResult<Response> {
    let back = back_to(&ctx, f.return_to.as_deref());
    if let Some(slug) = f.product.as_deref().filter(|s| !s.trim().is_empty()) {
        let product = catalog::product_by_slug(&state.pool, tenant.id, slug.trim())
            .await?
            .filter(|p| !p.is_retired)
            .ok_or(AppError::NotFound)?;
        let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
        let cart = resolved.require()?.clone();
        let options =
            catalog::product_config_options(&state.pool, tenant.id, product.id, cart.currency)
                .await?;
        let addons =
            catalog::addons_for_product(&state.pool, tenant.id, product.id, cart.currency).await?;
        let cycle_q = f.cycle.as_deref().unwrap_or("");
        if needs_configuration(&product, &options, &addons) {
            let mut to = ctx.url(format!("/cart/configure/{}", product.slug));
            if !cycle_q.is_empty() {
                to.push_str("?cycle=");
                to.push_str(
                    &url::form_urlencoded::byte_serialize(cycle_q.as_bytes()).collect::<String>(),
                );
            }
            return Ok(resolved.apply(axum::response::Redirect::to(&to).into_response()));
        }
        let pricing = catalog::product_pricing(&state.pool, tenant.id, product.id).await?;
        let cycle = cycle_q
            .parse::<BillingCycle>()
            .ok()
            .filter(|c| {
                pricing
                    .iter()
                    .any(|p| p.billing_cycle == *c && p.currency == cart.currency && p.is_enabled)
            })
            .or_else(|| {
                pricing
                    .iter()
                    .find(|p| p.currency == cart.currency && p.is_enabled)
                    .map(|p| p.billing_cycle)
            });
        let Some(cycle) = cycle else {
            return Ok(resolved.apply(flash::redirect(
                &back,
                SetFlash::error(ctx.t("cart.errors.not_available")),
            )));
        };
        let req = QuoteRequest {
            currency: cart.currency,
            items: vec![QuoteItem::product(product.id, cycle)],
            coupon_code: None,
            client_id: None,
            country_code: None,
            region: None,
        };
        if let Err(msg) = try_quote(&state, &tenant, &ctx, &req).await? {
            return Ok(resolved.apply(flash::redirect(&back, SetFlash::error(msg))));
        }
        carts::add_item(
            &state.pool,
            tenant.id,
            cart.id,
            &NewCartItem {
                parent_item_id: None,
                item_type: LineItemType::Product,
                product_id: Some(product.id),
                addon_id: None,
                domain_name: None,
                billing_cycle: Some(cycle),
                years: None,
                quantity: 1,
                config_selections: json!({}),
                custom_fields: json!({}),
                hostname: None,
                epp_code_enc: None,
            },
        )
        .await?;
        return Ok(resolved.apply(flash::redirect(
            &ctx.url("/cart"),
            SetFlash::success(ctx.tv("cart.flash.added", "name", &product.name)),
        )));
    }
    if let Some(domain) = f.domain.as_deref().filter(|d| !d.trim().is_empty()) {
        let transfer = f.action.as_deref() == Some("transfer");
        let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
        let msg = add_domain_item(
            &state,
            &tenant,
            &ctx,
            resolved.require()?,
            domain,
            transfer,
            f.years,
            f.epp.as_deref(),
        )
        .await?;
        let to = if matches!(msg, AddOutcome::Added(_) | AddOutcome::Duplicate(_)) {
            ctx.url("/cart")
        } else {
            back
        };
        return Ok(resolved.apply(flash::redirect(&to, msg.flash())));
    }
    Ok(flash::redirect(
        &back,
        SetFlash::error(ctx.t("cart.errors.nothing")),
    ))
}

/// Result of adding a domain.
pub enum AddOutcome {
    /// Added (message).
    Added(String),
    /// Already in the cart (message).
    Duplicate(String),
    /// Rejected (message).
    Rejected(String),
}

impl AddOutcome {
    /// As a flash message.
    pub fn flash(&self) -> SetFlash {
        match self {
            Self::Added(m) => SetFlash::success(m.clone()),
            Self::Duplicate(m) => SetFlash::info(m.clone()),
            Self::Rejected(m) => SetFlash::error(m.clone()),
        }
    }
}

/// Validates and adds a domain registration / transfer to the cart.
#[allow(clippy::too_many_arguments)]
pub async fn add_domain_item(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    cart: &CartRow,
    raw: &str,
    transfer: bool,
    years: Option<i16>,
    epp: Option<&str>,
) -> AppResult<AddOutcome> {
    let Ok(name) = DomainName::from_user_input(raw) else {
        return Ok(AddOutcome::Rejected(ctx.t("cart.errors.domain_invalid")));
    };
    let ascii = name.as_str().to_owned();
    let display = name.to_unicode();
    let Some(tld) = domain_repo::match_tld(&state.pool, tenant.id, &ascii)
        .await?
        .filter(|t| t.is_active)
    else {
        return Ok(AddOutcome::Rejected(ctx.tv(
            "cart.errors.tld_unsupported",
            "domain",
            &display,
        )));
    };
    let lines = carts::cart_lines(&state.pool, tenant.id, cart.id).await?;
    if lines.iter().any(|l| {
        l.item.domain_name.as_deref() == Some(ascii.as_str())
            && matches!(
                l.item.item_type,
                LineItemType::DomainRegister | LineItemType::DomainTransfer
            )
    }) {
        return Ok(AddOutcome::Duplicate(ctx.tv(
            "cart.flash.domain_duplicate",
            "domain",
            &display,
        )));
    }
    let epp = epp.map(str::trim).filter(|e| !e.is_empty());
    if transfer && tld.requires_epp && epp.is_none() {
        return Ok(AddOutcome::Rejected(ctx.tv(
            "cart.errors.epp_required",
            "domain",
            &display,
        )));
    }
    if epp.is_some_and(|e| e.len() > 128) {
        return Ok(AddOutcome::Rejected(ctx.t("cart.errors.epp_invalid")));
    }
    let min = tld.min_years.max(1);
    let years = if transfer {
        1
    } else {
        years.unwrap_or(min).clamp(min, tld.max_years.max(min))
    };
    let item_type = if transfer {
        LineItemType::DomainTransfer
    } else {
        LineItemType::DomainRegister
    };
    let req = QuoteRequest {
        currency: cart.currency,
        items: vec![QuoteItem::domain(item_type, &ascii, years)],
        coupon_code: None,
        client_id: None,
        country_code: None,
        region: None,
    };
    if let Err(msg) = try_quote(state, tenant, ctx, &req).await? {
        return Ok(AddOutcome::Rejected(msg));
    }
    let epp_code_enc = match epp {
        Some(code) => Some(hos_billing::secrets::seal(
            &state.secret_box,
            code,
            &hos_billing::secrets::epp_code_aad(tenant.id, &ascii),
        )?),
        None => None,
    };
    carts::add_item(
        &state.pool,
        tenant.id,
        cart.id,
        &NewCartItem {
            parent_item_id: None,
            item_type,
            product_id: None,
            addon_id: None,
            domain_name: Some(ascii),
            billing_cycle: None,
            years: Some(years),
            quantity: 1,
            config_selections: json!({}),
            custom_fields: json!({}),
            hostname: None,
            epp_code_enc,
        },
    )
    .await?;
    Ok(AddOutcome::Added(ctx.tv(
        if transfer {
            "cart.flash.transfer_added"
        } else {
            "cart.flash.domain_added"
        },
        "domain",
        &display,
    )))
}

/// `POST /cart/domain` (the cart's own "add a domain" form).
#[derive(Debug, Deserialize)]
pub struct DomainForm {
    domain: String,
    action: Option<String>,
    epp: Option<String>,
}

async fn add_domain(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    htmx: Htmx,
    ctx: PageCtx,
    Form(f): Form<DomainForm>,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
    let out = add_domain_item(
        &state,
        &tenant,
        &ctx,
        resolved.require()?,
        &f.domain,
        f.action.as_deref() == Some("transfer"),
        None,
        f.epp.as_deref(),
    )
    .await?;
    after_change(&state, &tenant, ctx, &htmx, &resolved, out.flash()).await
}

// ---------------------------------------------------------------------------
// Changing
// ---------------------------------------------------------------------------

/// `POST /cart/items/{id}`.
#[derive(Debug, Deserialize)]
pub struct ItemForm {
    cycle: Option<String>,
    years: Option<i16>,
}

async fn find_line(
    state: &AppState,
    tenant: &Tenant,
    cart: &CartRow,
    id: &str,
) -> AppResult<(Vec<CartLine>, usize)> {
    let id: CartItemId = id.parse().map_err(|_| AppError::NotFound)?;
    let lines = carts::cart_lines(&state.pool, tenant.id, cart.id).await?;
    let pos = lines
        .iter()
        .position(|l| l.item.id == id)
        .ok_or(AppError::NotFound)?;
    Ok((lines, pos))
}

async fn update_item(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    htmx: Htmx,
    ctx: PageCtx,
    Path(id): Path<String>,
    Form(f): Form<ItemForm>,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, false).await?;
    let cart = resolved.require()?.clone();
    let (mut lines, pos) = find_line(&state, &tenant, &cart, &id).await?;
    {
        let item = &mut lines[pos].item;
        if let Some(c) = f.cycle.as_deref().filter(|c| !c.is_empty()) {
            if item.item_type != LineItemType::Product {
                return Err(AppError::bad_request("cycle only applies to products"));
            }
            item.billing_cycle = Some(c.parse().map_err(|_| AppError::bad_request("cycle"))?);
        }
        if let Some(y) = f.years {
            if item.item_type != LineItemType::DomainRegister {
                return Err(AppError::bad_request("years only applies to registrations"));
            }
            item.years = Some(y);
        }
    }
    let client = client_of(&ctx, tenant.id).map(|(_, c)| c);
    let req = quote_request(&cart, &lines, client);
    let msg = match try_quote(&state, &tenant, &ctx, &req).await? {
        Ok(_) => {
            let item = &lines[pos].item;
            carts::update_item(
                &state.pool,
                tenant.id,
                cart.id,
                item.id,
                item.quantity,
                item.billing_cycle,
                item.years,
            )
            .await?;
            SetFlash::success(ctx.t("cart.flash.updated"))
        }
        Err(m) => SetFlash::error(m),
    };
    after_change(&state, &tenant, ctx, &htmx, &resolved, msg).await
}

async fn remove_item(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    htmx: Htmx,
    ctx: PageCtx,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, false).await?;
    let cart = resolved.require()?.clone();
    let (lines, pos) = find_line(&state, &tenant, &cart, &id).await?;
    carts::remove_item(&state.pool, tenant.id, cart.id, lines[pos].item.id).await?;
    let msg = SetFlash::success(ctx.t("cart.flash.removed"));
    after_change(&state, &tenant, ctx, &htmx, &resolved, msg).await
}

async fn empty_cart(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    htmx: Htmx,
    ctx: PageCtx,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, false).await?;
    if let Some(cart) = &resolved.cart {
        carts::clear_cart(&state.pool, tenant.id, cart.id).await?;
        carts::set_cart_coupon(&state.pool, tenant.id, cart.id, None).await?;
    }
    let msg = SetFlash::success(ctx.t("cart.flash.emptied"));
    after_change(&state, &tenant, ctx, &htmx, &resolved, msg).await
}

/// `POST /cart/coupon`.
#[derive(Debug, Deserialize)]
pub struct CouponForm {
    code: String,
}

async fn apply_coupon(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    htmx: Htmx,
    ctx: PageCtx,
    Form(f): Form<CouponForm>,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, false).await?;
    let cart = resolved.require()?.clone();
    let code = f.code.trim().to_uppercase();
    let coupon = catalog::redeemable_coupon(&state.pool, tenant.id, &code)
        .await?
        .filter(|_| !code.is_empty() && code.len() <= 64);
    let bundle_only = match &coupon {
        Some(c) => hos_db::repo::bundles::is_bundle_coupon(&state.pool, tenant.id, c.id).await?,
        None => false,
    };
    let msg = match coupon {
        None => SetFlash::error(ctx.t("cart.errors.coupon_invalid")),
        // A bundle's code only works through the bundle's order button.
        Some(_) if bundle_only => SetFlash::error(ctx.t("cart.errors.coupon_bundle_only")),
        Some(c) => {
            carts::set_cart_coupon(&state.pool, tenant.id, cart.id, Some((c.id, &c.code))).await?;
            let client = client_of(&ctx, tenant.id).map(|(_, c)| c);
            match state
                .billing
                .quote_cart(tenant.id, cart.id, client, None)
                .await
            {
                Ok(q) if q.coupon_error.is_none() => {
                    SetFlash::success(ctx.tv("cart.flash.coupon_applied", "code", &c.code))
                }
                Ok(q) => {
                    carts::set_cart_coupon(&state.pool, tenant.id, cart.id, None).await?;
                    SetFlash::error(ctx.tv(
                        "cart.errors.coupon_rejected",
                        "detail",
                        q.coupon_error.unwrap_or_default(),
                    ))
                }
                Err(e) => {
                    carts::set_cart_coupon(&state.pool, tenant.id, cart.id, None).await?;
                    SetFlash::error(user_error(&ctx, e)?)
                }
            }
        }
    };
    after_change(&state, &tenant, ctx, &htmx, &resolved, msg).await
}

async fn remove_coupon(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    htmx: Htmx,
    ctx: PageCtx,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, false).await?;
    let cart = resolved.require()?;
    carts::set_cart_coupon(&state.pool, tenant.id, cart.id, None).await?;
    let msg = SetFlash::success(ctx.t("cart.flash.coupon_removed"));
    after_change(&state, &tenant, ctx, &htmx, &resolved, msg).await
}

/// `POST /cart/currency`.
#[derive(Debug, Deserialize)]
pub struct CurrencyForm {
    currency: String,
}

async fn set_currency(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    htmx: Htmx,
    ctx: PageCtx,
    Form(f): Form<CurrencyForm>,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
    let cart = resolved.require()?.clone();
    let wanted: Currency = f
        .currency
        .parse()
        .map_err(|_| AppError::bad_request("currency"))?;
    let active = catalog::list_currencies(&state.pool, tenant.id, true).await?;
    if !active.iter().any(|c| c.code == wanted) {
        return Err(AppError::bad_request("currency not offered"));
    }
    carts::update_cart_checkout(
        &state.pool,
        tenant.id,
        cart.id,
        wanted,
        cart.payment_integration_id,
        cart.notes.as_deref(),
    )
    .await?;
    let msg = SetFlash::success(ctx.tv("cart.flash.currency", "currency", wanted.as_str()));
    after_change(&state, &tenant, ctx, &htmx, &resolved, msg).await
}
