//! CMS copy merged into route pages and category landings.
//!
//! A published CMS page whose slug matches a fixed route or a product group
//! supplies that page's editable copy (`blocks`):
//!
//! | slug | merged into |
//! |---|---|
//! | `home` | `/` — a `hero` block overrides the hero; other blocks follow the plans |
//! | `domains` | `/domains` — below the search (transfer steps, FAQ …) |
//! | `contact` | `/contact` — next to the form |
//! | `status`, `durum` | `/status` — below the live network status |
//! | `<group slug>` | `/<group slug>` — title, intro and blocks of the category landing |
//!
//! [`RESERVED`] slugs never render on their own: `/{slug}` redirects to the
//! route (no duplicate URL) and they stay out of the sitemap and menus.

use hos_core::money::BillingCycle;
use hos_db::repo::catalog::{self, CatalogGroup};
use hos_db::repo::{content, domains};

use super::blocks::{self, Block, Tokens};
use super::domains::TldPriceRow;
use super::{ProductCard, annual_saving, localized};
use crate::error::AppResult;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::PageCtx;

/// Page slugs that belong to a route (`slug` → unprefixed route path).
pub const RESERVED: &[(&str, &str)] = &[
    ("home", "/"),
    ("domains", "/domains"),
    ("contact", "/contact"),
    // Copy of the network status page (`/status`, see `super::status`).
    ("status", "/status"),
    ("durum", "/status"),
];

/// Route of a reserved slug.
pub fn reserved_route(slug: &str) -> Option<&'static str> {
    RESERVED.iter().find(|(s, _)| *s == slug).map(|(_, r)| *r)
}

/// A merged CMS page.
#[derive(Debug, Clone)]
pub struct Landing {
    /// The page row.
    pub page: content::PageRow,
    /// Parsed body (tokens applied; `todo` blocks only for content editors).
    pub blocks: Vec<Block>,
}

impl Landing {
    /// The `hero` block, if any.
    pub fn hero(&self) -> Option<&Block> {
        self.blocks.iter().find(|b| b.kind == "hero")
    }

    /// Every block except the hero.
    pub fn body(&self) -> Vec<Block> {
        self.blocks
            .iter()
            .filter(|b| b.kind != "hero")
            .cloned()
            .collect()
    }
}

/// Token values of the current tenant (store chrome must be loaded).
pub fn tokens(ctx: &PageCtx) -> Tokens {
    let store = ctx.store();
    Tokens {
        site_name: ctx.site_name.clone(),
        legal_name: store.company.legal_name.clone(),
        email: store.company.email.clone(),
        support_email: store.company.support_email.clone(),
        phone: store.company.phone.clone(),
        address: store.company.address.clone(),
        year: ctx.year(),
        url: ctx.origin.clone(),
        base: ctx.url("/").trim_end_matches('/').to_owned(),
    }
}

/// Parses `body` for the current visitor.
pub fn parse_for(ctx: &PageCtx, body: &str) -> Vec<Block> {
    let editor = ctx.can("content.manage");
    blocks::parse(&tokens(ctx).apply(body))
        .into_iter()
        .filter(|b| b.kind != "todo" || editor)
        .collect()
}

/// The published page `slug` in the current locale, parsed.
pub async fn load(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    slug: &str,
) -> AppResult<Option<Landing>> {
    Ok(
        content::published_page(&state.pool, tenant.id, ctx.locale, slug)
            .await?
            .map(|page| Landing {
                blocks: parse_for(ctx, &page.body),
                page,
            }),
    )
}

/// The TLD price table in the current currency.
pub async fn tld_rows(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
) -> AppResult<Vec<TldPriceRow>> {
    Ok(
        domains::tld_price_table(&state.pool, tenant.id, ctx.currency)
            .await?
            .into_iter()
            .map(|r| TldPriceRow {
                tld: r.tld,
                register: r.register,
                transfer: r.transfer,
                renew: r.renew,
                featured: r.is_featured,
            })
            .collect(),
    )
}

/// Plan cards of `group` in the current locale.
pub fn group_cards(ctx: &PageCtx, group: &CatalogGroup) -> Vec<ProductCard> {
    group
        .products
        .iter()
        .map(|p| {
            let href = ctx.url(format!("/{}/{}", group.group.slug, p.product.slug));
            ProductCard::new(p, ctx.locale, ctx.default_locale, href)
        })
        .collect()
}

/// Fills `pricing` and `tlds` blocks. `catalog` is loaded when `None` and a
/// block needs it.
pub async fn resolve(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    list: &mut [Block],
    catalog: Option<&[CatalogGroup]>,
) -> AppResult<()> {
    if list.iter().any(|b| b.kind == "pricing") {
        let owned;
        let groups = match catalog {
            Some(c) => c,
            None => {
                owned = catalog::catalog(&state.pool, tenant.id, ctx.currency, false).await?;
                &owned
            }
        };
        for b in list.iter_mut().filter(|b| b.kind == "pricing") {
            if let Some(g) = groups.iter().find(|g| g.group.slug == b.arg) {
                b.cards = group_cards(ctx, g);
                b.saving = annual_saving(&b.cards);
                b.href = ctx.url(format!("/{}", g.group.slug));
                if b.title.is_empty() {
                    b.title = localized(
                        &g.group.name,
                        &g.group.translations,
                        ctx.locale,
                        ctx.default_locale,
                        "name",
                    );
                    b.id = crate::markdown::slugify(&b.title);
                }
            }
        }
    }
    if list.iter().any(|b| b.kind == "tlds") {
        let rows = tld_rows(state, tenant, ctx).await?;
        for b in list.iter_mut().filter(|b| b.kind == "tlds") {
            b.tlds = rows.clone();
        }
    }
    Ok(())
}

/// The lowest monthly price among `cards` with its cycle (meta descriptions).
pub fn lowest(cards: &[ProductCard]) -> Option<(rust_decimal::Decimal, BillingCycle)> {
    cards
        .iter()
        .flat_map(|c| c.prices.iter())
        .map(|p| (p.per_month, p.cycle))
        .min_by(|a, b| a.0.cmp(&b.0))
}
