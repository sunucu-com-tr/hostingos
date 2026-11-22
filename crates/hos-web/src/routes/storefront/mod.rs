//! Public storefront: server-rendered, SEO / GEO first, tr + en.
//!
//! Every page handler follows the same shape:
//!
//! ```ignore
//! async fn page(State(state): State<AppState>, tenant: Tenant, mut ctx: PageCtx) -> AppResult<Response> {
//!     ctx.load_store().await?;                      // header / footer data (cached)
//!     let data = repo::…(&state.pool, tenant.id, ctx.locale, …).await?;
//!     seo::apply(&mut ctx, SeoPage::new(title, "/path").description(…).node(&json_ld)).await?;
//!     render(&MyPage { ctx, data })
//! }
//! ```
//!
//! Sub-modules: [`home`], [`catalog`] (product groups, products, CMS pages
//! through the `/{slug}` dispatcher), [`domains`] (search, availability,
//! whois), [`kb`], [`blog`] (+ announcements, feeds), [`contact`] (+ language
//! switch) and [`files`] (sitemap, robots, llms.txt,
//! theme stylesheet `/brand.css` (crate::theme), IndexNow key).

pub mod blocks;
pub mod blog;
pub mod bundles;
pub mod catalog;
pub mod contact;
pub mod domains;
pub mod downloads;
pub mod files;
pub mod home;
pub mod kb;
pub mod landing;
pub mod og;
pub mod quote;
pub mod referral;
pub mod status;

use axum::Router;
use axum::routing::{get, post};
use hos_core::domain::Locale;
use hos_core::money::{BillingCycle, Currency, Money};
use hos_db::repo::catalog::{CatalogProduct, PriceTag, translation};
use hos_seo::jsonld::{Offer, Offers, Product};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::state::AppState;

/// Storefront routes.
pub fn router(_state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/", get(home::show))
        .route("/lang/{code}", get(contact::switch_language))
        .route("/currency/{code}", post(contact::switch_currency))
        .route("/downloads", get(downloads::list))
        .route("/downloads/{id}/file", get(downloads::file))
        .route("/bundles", get(bundles::list))
        .route("/bundles/{slug}/order", post(bundles::order))
        .route("/domains", get(domains::search))
        .route("/domains/check", get(domains::check))
        .route("/whois", get(domains::whois))
        .route("/kb", get(kb::index))
        .route("/kb/search", get(kb::search))
        .route("/kb/category/{slug}", get(kb::category))
        .route("/kb/{slug}", get(kb::article))
        .route("/kb/{slug}/vote", post(kb::vote))
        .route("/blog", get(blog::index))
        .route("/blog/feed.xml", get(blog::atom))
        .route("/blog/rss.xml", get(blog::rss))
        .route("/blog/{slug}", get(blog::post))
        .route("/announcements", get(blog::announcements))
        .route("/announcements/{slug}", get(blog::announcement))
        .route("/contact", get(contact::form).post(contact::submit))
        .route("/sitemap.xml", get(files::sitemap))
        .route("/sitemaps/{file}", get(files::sitemap_part))
        .route("/robots.txt", get(files::robots))
        .route("/llms.txt", get(files::llms))
        .route("/llms-full.txt", get(files::llms_full))
        .route("/brand.css", get(crate::theme::stylesheet))
        .route("/favicon.ico", get(files::favicon))
        .route("/og/{file}", get(og::image))
        .merge(quote::routes())
        .route("/r/{code}", get(referral::visit))
        .route("/status", get(status::page))
        .route("/status/feed.xml", get(status::feed))
        .route("/status/incidents/{id}", get(status::incident))
        .route("/{slug}", get(catalog::slug_page))
        .route("/{group}/{product}", get(catalog::product))
}

// ---------------------------------------------------------------------------
// Catalog view models (shared by home, group and product pages)
// ---------------------------------------------------------------------------

/// Field of a catalog row in `locale`: the base column holds the tenant's
/// default language, `translations.<locale>.<field>` the others.
pub fn localized(
    base: &str,
    translations: &Value,
    locale: Locale,
    default: Locale,
    field: &str,
) -> String {
    if locale == default {
        return base.to_owned();
    }
    translation(translations, locale.as_str(), field)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(base)
        .to_owned()
}

/// Feature bullets of a product in `locale`.
pub fn localized_features(p: &CatalogProduct, locale: Locale, default: Locale) -> Vec<String> {
    let from = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    if locale != default
        && let Some(f) = p
            .product
            .translations
            .get(locale.as_str())
            .and_then(|t| t.get("features"))
    {
        let list = from(f);
        if !list.is_empty() {
            return list;
        }
    }
    from(&p.product.features)
}

/// One displayed price of a card (per billing cycle).
#[derive(Debug, Clone)]
pub struct CardPrice {
    /// `monthly` / `annual` / `fallback` (CSS cycle switch).
    pub slot: &'static str,
    /// Cycle charged.
    pub cycle: BillingCycle,
    /// Amount shown big (per month for multi-month cycles).
    pub per_month: Decimal,
    /// Amount charged per cycle.
    pub total: Decimal,
    /// One-off setup fee.
    pub setup_fee: Decimal,
}

/// Product card / detail view model.
#[derive(Debug, Clone)]
pub struct ProductCard {
    /// Product slug.
    pub slug: String,
    /// Localized name.
    pub name: String,
    /// Localized tagline.
    pub tagline: String,
    /// Localized feature bullets.
    pub features: Vec<String>,
    /// Highlighted ("popular").
    pub featured: bool,
    /// Localized detail URL.
    pub href: String,
    /// Prices for the monthly / annual switch (+ a fallback when neither exists).
    pub prices: Vec<CardPrice>,
    /// Every enabled cycle (detail page).
    pub all_prices: Vec<PriceTag>,
    /// Lowest per-month price.
    pub from: Option<Decimal>,
    /// Small label above the name instead of the "popular" ribbon (the
    /// group on the home page); empty = none.
    pub eyebrow: String,
}

impl ProductCard {
    /// Builds the card (`href`: localized detail URL).
    pub fn new(p: &CatalogProduct, locale: Locale, default: Locale, href: String) -> Self {
        let price_of = |c: BillingCycle| p.prices.iter().find(|x| x.billing_cycle == c);
        let card_price = |slot: &'static str, t: &PriceTag| {
            let months = t.billing_cycle.months().unwrap_or(1).max(1);
            CardPrice {
                slot,
                cycle: t.billing_cycle,
                per_month: (t.price / Decimal::from(months)).round_dp(2),
                total: t.price,
                setup_fee: t.setup_fee,
            }
        };
        let prices = match (
            price_of(BillingCycle::Monthly),
            price_of(BillingCycle::Annual),
        ) {
            (Some(m), Some(a)) => vec![card_price("monthly", m), card_price("annual", a)],
            // Without a monthly/annual pair one price serves both switch positions.
            (Some(only), None) | (None, Some(only)) => vec![card_price("fallback", only)],
            (None, None) => p
                .prices
                .first()
                .map(|t| vec![card_price("fallback", t)])
                .unwrap_or_default(),
        };
        Self {
            slug: p.product.slug.clone(),
            name: localized(
                &p.product.name,
                &p.product.translations,
                locale,
                default,
                "name",
            ),
            tagline: localized(
                p.product.tagline.as_deref().unwrap_or_default(),
                &p.product.translations,
                locale,
                default,
                "tagline",
            ),
            features: localized_features(p, locale, default),
            featured: p.product.is_featured,
            href,
            prices,
            all_prices: p.prices.clone(),
            from: p.starting_monthly().map(|d| d.round_dp(2)),
            eyebrow: String::new(),
        }
    }

    /// `Product` JSON-LD with one `Offer` per billing cycle.
    pub fn json_ld(&self, absolute_url: &str, currency: Currency, brand: &str) -> Product {
        let offers: Vec<Offer> = self
            .all_prices
            .iter()
            .map(|p| {
                let mut o = Offer::new(Money::new(p.price, currency)).with_cycle(p.billing_cycle);
                if !p.setup_fee.is_zero() {
                    o.setup_fee = Some(Money::new(p.setup_fee, currency));
                }
                o.url = Some(absolute_url.to_owned());
                o
            })
            .collect();
        Product {
            id: Some(format!("{absolute_url}#product")),
            name: self.name.clone(),
            description: (!self.tagline.is_empty()).then(|| self.tagline.clone()),
            url: Some(absolute_url.to_owned()),
            sku: Some(self.slug.clone()),
            brand: Some(brand.to_owned()),
            offers: match offers.len() {
                0 => None,
                1 => offers.into_iter().next().map(Offers::Single),
                _ => Some(Offers::Multiple(offers)),
            },
            ..Product::default()
        }
    }
}

/// Largest saving (percent, rounded down) of annual over monthly billing
/// among `cards`; `None` when no card has both prices or nothing is saved.
pub fn annual_saving(cards: &[ProductCard]) -> Option<i64> {
    use rust_decimal::prelude::ToPrimitive as _;
    cards
        .iter()
        .filter_map(|c| {
            let monthly = c.prices.iter().find(|p| p.slot == "monthly")?.total;
            let annual = c.prices.iter().find(|p| p.slot == "annual")?.total;
            let full = monthly * Decimal::from(12);
            if full.is_zero() || annual >= full {
                return None;
            }
            ((full - annual) * Decimal::from(100) / full)
                .floor()
                .to_i64()
        })
        .max()
        .filter(|p| *p > 0)
}
