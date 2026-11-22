//! `/{slug}` (product group = category landing, CMS page, `.md` twins,
//! IndexNow key) and `/{group}/{product}` (product detail, `.md` twin).

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::catalog::{self, CatalogGroup};
use hos_db::repo::content::{self, ContentTable};
use hos_seo::jsonld::{Article, ArticleKind, Entity, FaqEntry, FaqPage};
use hos_seo::llms::{Body, Document, page_markdown};
use http::StatusCode;
use http::header::LOCATION;
use rust_decimal::Decimal;

use super::blocks::{self, Block};
use super::landing::{self, Landing};
use super::{ProductCard, annual_saving, files, localized};
use crate::error::{AppError, AppResult};
use crate::markdown::{self, Heading};
use crate::seo::{self, SeoPage, SeoTarget};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, NavItem, PageCtx, render};

/// One row of the comparison table.
#[derive(Debug, Clone)]
pub struct CompareRow {
    /// Row header.
    pub label: String,
    /// The header is for screen readers only (a positional feature row
    /// whose cells share no common noun).
    pub label_hidden: bool,
    /// One cell per product.
    pub cells: Vec<String>,
}

/// A question with its answer (generated product FAQ).
#[derive(Debug, Clone)]
pub struct Qa {
    /// Question.
    pub q: String,
    /// Answer (plain text).
    pub a: String,
}

#[derive(Template)]
#[template(path = "storefront/group.html")]
struct GroupPage {
    ctx: PageCtx,
    name: String,
    lead: String,
    tagline: String,
    icon: String,
    description_html: String,
    cards: Vec<ProductCard>,
    saving: Option<i64>,
    compare: Vec<CompareRow>,
    other_groups: Vec<NavItem>,
    crumbs: Vec<Crumb>,
    blocks: Vec<Block>,
    updated_at: DateTime<Utc>,
    md_href: String,
    from: Option<Decimal>,
}
crate::page!(GroupPage);

#[derive(Template)]
#[template(path = "storefront/page.html")]
struct CmsPage {
    ctx: PageCtx,
    title: String,
    blocks: Vec<Block>,
    toc: Vec<Heading>,
    legal: bool,
    updated_at: DateTime<Utc>,
    crumbs: Vec<Crumb>,
    md_href: String,
}
crate::page!(CmsPage);

/// `GET /{slug}`.
pub(super) async fn slug_page(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(slug): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    if let Some(key) = slug.strip_suffix(".txt") {
        return match files::indexnow_key(&state, &tenant).await? {
            Some(k) if k == key => Ok(files::text(hos_seo::indexnow::key_file_content(&k))),
            _ => Err(AppError::NotFound),
        };
    }
    if let Some(page_slug) = slug.strip_suffix(".md") {
        return page_md(&state, &tenant, &mut ctx, page_slug).await;
    }
    if let Some(route) = landing::reserved_route(&slug) {
        // `home`, `domains`, `contact`: copy of a route page, one URL only.
        return Ok(moved_permanently(&ctx.url(route)));
    }
    let catalog = catalog::catalog(&state.pool, tenant.id, ctx.currency, false).await?;
    if let Some(group) = catalog.iter().find(|g| g.group.slug == slug) {
        return group_page(&state, &tenant, &catalog, group, ctx).await;
    }
    if let Some(page) = content::published_page(&state.pool, tenant.id, ctx.locale, &slug).await? {
        return cms_page(&state, &tenant, ctx, page, &catalog).await;
    }
    Err(AppError::NotFound)
}

/// `301 Moved Permanently` to `to`.
pub fn moved_permanently(to: &str) -> Response {
    (StatusCode::MOVED_PERMANENTLY, [(LOCATION, to.to_owned())]).into_response()
}

fn price_cell(ctx: &PageCtx, card: &ProductCard, cycle: BillingCycle) -> String {
    card.all_prices
        .iter()
        .find(|p| p.billing_cycle == cycle)
        .map(|p| ctx.money(&p.price))
        .unwrap_or_else(|| "—".into())
}

fn compare_rows(ctx: &PageCtx, cards: &[ProductCard]) -> Vec<CompareRow> {
    let mut compare = vec![
        CompareRow {
            label_hidden: false,
            label: ctx.t("common.cycle.monthly"),
            cells: cards
                .iter()
                .map(|c| price_cell(ctx, c, BillingCycle::Monthly))
                .collect(),
        },
        CompareRow {
            label_hidden: false,
            label: ctx.t("common.cycle.annual"),
            cells: cards
                .iter()
                .map(|c| price_cell(ctx, c, BillingCycle::Annual))
                .collect(),
        },
    ];
    let setup: Vec<String> = cards
        .iter()
        .map(|c| {
            c.all_prices
                .iter()
                .map(|p| p.setup_fee)
                .max()
                .filter(|f| !f.is_zero())
                .map(|f| ctx.money(&f))
                .unwrap_or_else(|| ctx.t("storefront.catalog.free"))
        })
        .collect();
    compare.push(CompareRow {
        label_hidden: false,
        label: ctx.t("storefront.catalog.setup"),
        cells: setup,
    });
    let rows = cards.iter().map(|c| c.features.len()).max().unwrap_or(0);
    for i in 0..rows {
        let cells: Vec<String> = cards
            .iter()
            .map(|c| c.features.get(i).cloned().unwrap_or_else(|| "—".into()))
            .collect();
        let common = common_noun(&cells);
        compare.push(CompareRow {
            label_hidden: common.is_none(),
            label: common.unwrap_or_else(|| ctx.tv("storefront.catalog.feature_n", "n", i + 1)),
            cells,
        });
    }
    compare
}

/// The words every cell of a feature row ends with, when the cells differ
/// only in their amount ("10 GB NVMe disk" / "Sınırsız NVMe disk" →
/// "NVMe disk"), capitalised; `None` when there is no such noun.
fn common_noun(cells: &[String]) -> Option<String> {
    let rows: Vec<Vec<&str>> = cells
        .iter()
        .filter(|c| c.as_str() != "—")
        .map(|c| c.split_whitespace().collect())
        .collect();
    if rows.len() < 2 {
        return None;
    }
    let shortest = rows.iter().map(Vec::len).min().unwrap_or(0);
    let mut n = 0;
    while n < shortest {
        let word = rows[0][rows[0].len() - 1 - n];
        if rows
            .iter()
            .all(|r| r[r.len() - 1 - n].eq_ignore_ascii_case(word))
        {
            n += 1;
        } else {
            break;
        }
    }
    // Every cell must keep an amount in front of the shared words, and
    // the shared part must be a noun phrase, not a lone unit ("GB").
    if n == 0 || rows.iter().any(|r| r.len() == n) {
        return None;
    }
    let mut words = &rows[0][rows[0].len() - n..];
    // "10 GB NVMe disk": the unit belongs to the amount, not the label.
    while let [unit, rest @ ..] = words
        && !rest.is_empty()
        && ["kb", "mb", "gb", "tb", "gib", "tib"].contains(&unit.to_lowercase().as_str())
    {
        words = rest;
    }
    let phrase = words.join(" ");
    if phrase.chars().count() < 3 || !phrase.chars().any(char::is_alphabetic) {
        return None;
    }
    // Sentence case, but leave product spellings ("vCPU", "NVMe") alone.
    if words[0].chars().all(|c| !c.is_uppercase()) {
        let mut chars = phrase.chars();
        let first = chars.next()?;
        Some(first.to_uppercase().chain(chars).collect())
    } else {
        Some(phrase)
    }
}

/// Splits the first paragraph off the first Markdown block (the hero lead
/// of a landing) and returns it as plain text.
fn take_lead(list: &mut Vec<Block>) -> String {
    let Some(first) = list.first_mut().filter(|b| b.kind == "markdown") else {
        return String::new();
    };
    let source = first.source.clone();
    let mut parts = source.splitn(2, "\n\n");
    let head = parts.next().unwrap_or("").trim();
    if head.is_empty() || head.starts_with('#') || head.starts_with('|') || head.starts_with('-') {
        return String::new();
    }
    let rest = parts.next().unwrap_or("").trim().to_owned();
    let lead = markdown::plain_text(head);
    if rest.is_empty() {
        list.remove(0);
    } else {
        first.html = markdown::render(&rest);
        first.source = rest;
    }
    lead
}

fn other_groups(
    ctx: &PageCtx,
    catalog: &[CatalogGroup],
    except: Option<uuid::Uuid>,
) -> Vec<NavItem> {
    let (locale, default) = (ctx.locale, ctx.default_locale);
    catalog
        .iter()
        .filter(|o| Some(o.group.id.into_uuid()) != except)
        .map(|o| NavItem {
            label: localized(
                &o.group.name,
                &o.group.translations,
                locale,
                default,
                "name",
            ),
            href: ctx.url(format!("/{}", o.group.slug)),
            description: localized(
                o.group.tagline.as_deref().unwrap_or_default(),
                &o.group.translations,
                locale,
                default,
                "tagline",
            ),
            icon: o.group.icon.clone().unwrap_or_else(|| "box".into()),
        })
        .collect()
}

pub fn faq_node(url: &str, entries: Vec<(String, String)>) -> Option<FaqPage> {
    (!entries.is_empty()).then(|| FaqPage {
        id: Some(format!("{url}#faq")),
        url: Some(url.to_owned()),
        entries: entries
            .into_iter()
            .map(|(q, a)| FaqEntry::new(q, a))
            .collect(),
    })
}

async fn group_page(
    state: &AppState,
    tenant: &Tenant,
    catalog: &[CatalogGroup],
    group: &CatalogGroup,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let (locale, default) = (ctx.locale, ctx.default_locale);
    let g = &group.group;
    let mut name = localized(&g.name, &g.translations, locale, default, "name");
    let tagline = localized(
        g.tagline.as_deref().unwrap_or_default(),
        &g.translations,
        locale,
        default,
        "tagline",
    );
    let description = localized(
        g.description.as_deref().unwrap_or_default(),
        &g.translations,
        locale,
        default,
        "description",
    );
    let path = format!("/{}", g.slug);
    let cards = landing::group_cards(&ctx, group);

    // Editable landing copy: the CMS page with the group's slug.
    let landing: Option<Landing> = landing::load(state, tenant, &ctx, &g.slug).await?;
    let mut list = landing.as_ref().map(Landing::body).unwrap_or_default();
    landing::resolve(state, tenant, &ctx, &mut list, Some(catalog)).await?;
    let lead = take_lead(&mut list);
    let seo_title = match &landing {
        Some(l) => {
            name = l.page.title.clone();
            l.page.title.clone()
        }
        None => name.clone(),
    };
    let mut updated_at = group
        .products
        .iter()
        .map(|p| p.product.updated_at)
        .fold(g.updated_at, DateTime::max);
    if let Some(l) = &landing {
        updated_at = updated_at.max(l.page.updated_at);
    }

    let origin = ctx.origin.clone();
    let brand = ctx.site_name.clone();
    let meta_description = if !lead.is_empty() {
        lead.clone()
    } else if description.is_empty() {
        group_meta_description(&ctx, &name, &tagline, &cards)
    } else {
        markdown::plain_text(&description)
    };
    let abs_url = format!("{origin}{}", ctx.url(&path));
    let mut page = SeoPage::new(seo_title, path.clone())
        .description(meta_description)
        .target(SeoTarget::Entity(
            hos_db::enums::SeoEntityType::ProductGroup,
            g.id.into_uuid(),
        ));
    let image = ctx.theme.share_image_url(&origin, &path);
    for c in &cards {
        let mut node = c.json_ld(&format!("{origin}{}", c.href), ctx.currency, &brand);
        node.category = Some(localized(&g.name, &g.translations, locale, default, "name"));
        node.images = vec![image.clone()];
        page = page.node(&node);
    }
    if let Some(faq) = faq_node(&abs_url, blocks::faq_entries(&list)) {
        page = page.node(&faq);
    }
    seo::apply(&mut ctx, page).await?;
    let crumbs = vec![
        Crumb::link(ctx.t("common.nav.home"), ctx.url("/")),
        Crumb::current(name.clone()),
    ];
    render(&GroupPage {
        saving: annual_saving(&cards),
        description_html: markdown::render(&description),
        compare: compare_rows(&ctx, &cards),
        other_groups: other_groups(&ctx, catalog, Some(g.id.into_uuid())),
        md_href: ctx.url(format!("{path}.md")),
        from: cards.iter().filter_map(|c| c.from).min(),
        icon: g.icon.clone().unwrap_or_else(|| "box".into()),
        ctx,
        name,
        lead,
        tagline,
        cards,
        crumbs,
        blocks: list,
        updated_at,
    })
}

async fn alternates(
    state: &AppState,
    tenant: &Tenant,
    table: ContentTable,
    group: uuid::Uuid,
    prefix: &str,
) -> AppResult<Vec<(hos_core::domain::Locale, String)>> {
    Ok(content::translations(&state.pool, tenant.id, table, group)
        .await?
        .into_iter()
        .map(|t| (t.locale, format!("{prefix}{}", t.slug)))
        .collect())
}

async fn cms_page(
    state: &AppState,
    tenant: &Tenant,
    mut ctx: PageCtx,
    page: content::PageRow,
    catalog: &[CatalogGroup],
) -> AppResult<Response> {
    ctx.load_store().await?;
    let mut list = landing::parse_for(&ctx, &page.body);
    landing::resolve(state, tenant, &ctx, &mut list, Some(catalog)).await?;
    let legal = page.template == "legal";
    let toc: Vec<Heading> = if legal {
        list.iter()
            .filter(|b| b.kind == "markdown")
            .flat_map(|b| markdown::render_with_toc(&b.source).1)
            .collect()
    } else {
        Vec::new()
    };
    let path = format!("/{}", page.slug);
    let origin = ctx.origin.clone();
    let url = format!("{origin}{}", ctx.url(&path));
    let text = blocks::plain_text(&list);
    let lead = blocks::lead_text(&list);
    let description = if lead.chars().count() >= 60 {
        lead
    } else {
        text.clone()
    };
    let article = Article {
        kind: ArticleKind::Article,
        id: Some(format!("{url}#article")),
        headline: page.title.clone(),
        description: Some(hos_seo::meta::description_from_text(
            &description,
            hos_seo::meta::DESCRIPTION_MAX,
        )),
        url: Some(url.clone()),
        images: vec![ctx.theme.share_image_url(&origin, &path)],
        date_published: page.published_at,
        date_modified: Some(page.updated_at),
        authors: vec![
            Entity::organization(ctx.site_name.clone()).with_id(format!("{origin}/#organization")),
        ],
        publisher: Some(
            Entity::organization(ctx.site_name.clone()).with_id(format!("{origin}/#organization")),
        ),
        in_language: Some(ctx.locale.as_str().to_owned()),
        word_count: u32::try_from(text.split_whitespace().count()).ok(),
        ..Article::default()
    };
    let mut seo_page = SeoPage::new(page.title.clone(), path.clone())
        .description(description)
        .target(SeoTarget::Entity(
            hos_db::enums::SeoEntityType::Page,
            page.id.into_uuid(),
        ))
        .node(&article);
    if let Some(faq) = faq_node(&url, blocks::faq_entries(&list)) {
        seo_page = seo_page.node(&faq);
    }
    seo_page.alternates = Some(
        alternates(
            state,
            tenant,
            ContentTable::Pages,
            page.translation_group_id,
            "/",
        )
        .await?,
    );
    seo::apply(&mut ctx, seo_page).await?;
    let crumbs = vec![
        Crumb::link(ctx.t("common.nav.home"), ctx.url("/")),
        Crumb::current(page.title.clone()),
    ];
    if legal {
        ctx.body_class = "page-legal".into();
    }
    render(&CmsPage {
        md_href: ctx.url(format!("{path}.md")),
        ctx,
        title: page.title,
        blocks: list,
        toc,
        legal,
        updated_at: page.updated_at,
        crumbs,
    })
}

/// `GET /{slug}.md`: Markdown twin of a category landing or a CMS page.
async fn page_md(
    state: &AppState,
    tenant: &Tenant,
    ctx: &mut PageCtx,
    slug: &str,
) -> AppResult<Response> {
    ctx.load_store().await?;
    // Markdown twins are public, cacheable files: the default currency.
    ctx.currency = tenant.default_currency;
    let catalog = catalog::catalog(&state.pool, tenant.id, ctx.currency, false).await?;
    let currency = ctx.currency.as_str().to_owned();
    if let Some(group) = catalog.iter().find(|g| g.group.slug == slug) {
        let (locale, default) = (ctx.locale, ctx.default_locale);
        let g = &group.group;
        let landing = landing::load(state, tenant, ctx, &g.slug).await?;
        let mut list = vec![Block {
            kind: "pricing",
            arg: g.slug.clone(),
            ..Block::default()
        }];
        let mut title = localized(&g.name, &g.translations, locale, default, "name");
        let mut updated = g.updated_at;
        if let Some(l) = &landing {
            title = l.page.title.clone();
            updated = updated.max(l.page.updated_at);
            let mut body = l.body();
            body.retain(|b| !(b.kind == "pricing" && b.arg == g.slug));
            list.splice(0..0, body);
        } else {
            let tagline = localized(
                g.tagline.as_deref().unwrap_or_default(),
                &g.translations,
                locale,
                default,
                "tagline",
            );
            list.insert(
                0,
                Block {
                    kind: "markdown",
                    source: tagline,
                    ..Block::default()
                },
            );
        }
        landing::resolve(state, tenant, ctx, &mut list, Some(&catalog)).await?;
        let doc = Document {
            title,
            url: Some(format!("{}{}", ctx.origin, ctx.url(format!("/{}", g.slug)))),
            updated: Some(updated),
            body: Body::Markdown(blocks::to_markdown(&list, &currency)),
        };
        return Ok(files::markdown_response(page_markdown(&doc, ctx.locale)));
    }
    let page = content::published_page(&state.pool, tenant.id, ctx.locale, slug)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut list = landing::parse_for(ctx, &page.body);
    landing::resolve(state, tenant, ctx, &mut list, Some(&catalog)).await?;
    let path = landing::reserved_route(&page.slug)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("/{}", page.slug));
    let doc = Document {
        title: page.title.clone(),
        url: Some(format!("{}{}", ctx.origin, ctx.url(&path))),
        updated: Some(page.updated_at),
        body: Body::Markdown(blocks::to_markdown(&list, &currency)),
    };
    Ok(files::markdown_response(page_markdown(&doc, ctx.locale)))
}

// ---------------------------------------------------------------------------
// Product detail
// ---------------------------------------------------------------------------

/// A configurable option shown on the product page.
#[derive(Debug, Clone)]
pub struct OptionView {
    /// Localized name.
    pub name: String,
    /// Values with their monthly surcharge (if any).
    pub values: Vec<(String, Option<Decimal>)>,
}

/// An addon shown on the product page.
#[derive(Debug, Clone)]
pub struct AddonView {
    /// Localized name.
    pub name: String,
    /// Localized description.
    pub description: String,
    /// Cheapest price and its cycle.
    pub price: Option<(Decimal, BillingCycle)>,
}

/// One billing cycle row of the product price table.
#[derive(Debug, Clone)]
pub struct CycleRow {
    /// Cycle.
    pub cycle: BillingCycle,
    /// Charged per cycle.
    pub price: Decimal,
    /// Equivalent per month.
    pub per_month: Decimal,
    /// Setup fee.
    pub setup_fee: Decimal,
}

#[derive(Template)]
#[template(path = "storefront/product.html")]
struct ProductPage {
    ctx: PageCtx,
    card: ProductCard,
    group_name: String,
    group_href: String,
    lead: String,
    description_html: String,
    options: Vec<OptionView>,
    addons: Vec<AddonView>,
    default_cycle: String,
    cycles: Vec<CycleRow>,
    faq: Vec<Qa>,
    crumbs: Vec<Crumb>,
    siblings: Vec<ProductCard>,
    siblings_saving: Option<i64>,
    updated_at: DateTime<Utc>,
    md_href: String,
}
crate::page!(ProductPage);

fn cycle_rows(card: &ProductCard) -> Vec<CycleRow> {
    card.all_prices
        .iter()
        .map(|p| {
            let months = p.billing_cycle.months().unwrap_or(1).max(1);
            CycleRow {
                cycle: p.billing_cycle,
                price: p.price,
                per_month: (p.price / Decimal::from(months)).round_dp(2),
                setup_fee: p.setup_fee,
            }
        })
        .collect()
}

/// The product's definitional intro and data-derived questions (prices,
/// cycles, setup fee, contents). Nothing is claimed that the catalog does
/// not state.
fn product_copy(
    ctx: &PageCtx,
    card: &ProductCard,
    group_name: &str,
    cycles: &[CycleRow],
    upgrades: &[String],
) -> (String, Vec<Qa>) {
    let site = ctx.site_name.clone();
    let features: Vec<&str> = card.features.iter().take(3).map(String::as_str).collect();
    let mut lead = ctx
        .t("storefront.product.lead")
        .replace("{name}", &card.name)
        .replace("{group}", group_name)
        .replace("{site}", &site);
    if !features.is_empty() {
        lead.push(' ');
        lead.push_str(
            &ctx.t("storefront.product.lead_features")
                .replace("{features}", &features.join(", ")),
        );
    }
    let cheapest = cycles.iter().min_by(|a, b| a.per_month.cmp(&b.per_month));
    if let Some(c) = cheapest {
        lead.push(' ');
        lead.push_str(
            &ctx.t("storefront.product.lead_price")
                .replace("{amount}", &ctx.money(&c.per_month)),
        );
    }
    let mut faq = Vec::new();
    if !cycles.is_empty() {
        let list: Vec<String> = cycles
            .iter()
            .map(|c| {
                format!(
                    "{} {}",
                    ctx.cycle(&c.cycle).to_lowercase(),
                    ctx.money(&c.price)
                )
            })
            .collect();
        faq.push(Qa {
            q: ctx
                .t("storefront.product.faq_price_q")
                .replace("{name}", &card.name),
            a: ctx
                .t("storefront.product.faq_price_a")
                .replace("{name}", &card.name)
                .replace("{list}", &list.join(", ")),
        });
        let max_fee = cycles.iter().map(|c| c.setup_fee).max().unwrap_or_default();
        faq.push(Qa {
            q: ctx.t("storefront.product.faq_setup_q"),
            a: if max_fee.is_zero() {
                ctx.t("storefront.product.faq_setup_none")
            } else {
                ctx.t("storefront.product.faq_setup_fee")
                    .replace("{amount}", &ctx.money(&max_fee))
            },
        });
    }
    if !card.features.is_empty() {
        faq.push(Qa {
            q: ctx
                .t("storefront.product.faq_included_q")
                .replace("{name}", &card.name),
            a: ctx
                .t("storefront.product.faq_included_a")
                .replace("{name}", &card.name)
                .replace("{list}", &card.features.join(", ")),
        });
    }
    faq.push(Qa {
        q: ctx.t("storefront.product.faq_upgrade_q"),
        a: if upgrades.is_empty() {
            ctx.t("storefront.product.faq_upgrade_contact")
        } else {
            ctx.t("storefront.product.faq_upgrade_a")
                .replace("{list}", &upgrades.join(", "))
        },
    });
    (lead, faq)
}

/// `GET /{group}/{product}`.
pub(super) async fn product(
    State(state): State<AppState>,
    tenant: Tenant,
    Path((group_slug, product_slug)): Path<(String, String)>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let (locale, default) = (ctx.locale, ctx.default_locale);
    let (product_slug, as_markdown) = match product_slug.strip_suffix(".md") {
        Some(s) => (s.to_owned(), true),
        None => (product_slug, false),
    };
    if as_markdown {
        // Markdown twins are public, cacheable files: the default currency.
        ctx.currency = tenant.default_currency;
    }
    let currency: Currency = ctx.currency;
    let catalog = catalog::catalog(&state.pool, tenant.id, currency, false).await?;
    let group = catalog
        .iter()
        .find(|g| g.group.slug == group_slug)
        .ok_or(AppError::NotFound)?;
    let p = group
        .products
        .iter()
        .find(|p| p.product.slug == product_slug)
        .ok_or(AppError::NotFound)?;
    ctx.load_store().await?;
    let group_path = format!("/{}", group.group.slug);
    let path = format!("{group_path}/{}", p.product.slug);
    let card = ProductCard::new(p, locale, default, ctx.url(&path));
    let group_name = localized(
        &group.group.name,
        &group.group.translations,
        locale,
        default,
        "name",
    );
    let description = localized(
        p.product.description.as_deref().unwrap_or_default(),
        &p.product.translations,
        locale,
        default,
        "description",
    );
    let upgrades: Vec<String> = p
        .product
        .upgrade_product_ids
        .iter()
        .filter_map(|id| {
            catalog
                .iter()
                .flat_map(|g| g.products.iter())
                .find(|x| x.product.id == *id)
                .map(|x| {
                    localized(
                        &x.product.name,
                        &x.product.translations,
                        locale,
                        default,
                        "name",
                    )
                })
        })
        .collect();
    let cycles = cycle_rows(&card);
    let (lead, faq) = product_copy(&ctx, &card, &group_name, &cycles, &upgrades);
    let updated_at = p.product.updated_at;

    if as_markdown {
        let mut md = format!("{lead}\n\n");
        if !card.features.is_empty() {
            md.push_str(&format!("## {}\n\n", ctx.t("storefront.product.features")));
            for f in &card.features {
                md.push_str(&format!("- {f}\n"));
            }
            md.push('\n');
        }
        if !cycles.is_empty() {
            md.push_str(&format!(
                "## {}\n\n| {} | {} | {} | {} |\n|---|---|---|---|\n",
                ctx.t("storefront.product.prices_title"),
                ctx.t("storefront.product.col_cycle"),
                ctx.t("storefront.product.col_price"),
                ctx.t("storefront.product.col_per_month"),
                ctx.t("storefront.product.col_setup"),
            ));
            for c in &cycles {
                md.push_str(&format!(
                    "| {} | {} {cur} | {} {cur} | {} {cur} |\n",
                    ctx.cycle(&c.cycle),
                    c.price.round_dp(2),
                    c.per_month,
                    c.setup_fee.round_dp(2),
                    cur = currency.as_str()
                ));
            }
            md.push('\n');
        }
        if !description.trim().is_empty() {
            md.push_str(description.trim());
            md.push_str("\n\n");
        }
        md.push_str(&format!("## {}\n\n", ctx.t("storefront.product.faq_title")));
        for qa in &faq {
            md.push_str(&format!("### {}\n\n{}\n\n", qa.q, qa.a));
        }
        let doc = Document {
            title: format!("{} — {group_name}", card.name),
            url: Some(format!("{}{}", ctx.origin, card.href)),
            updated: Some(updated_at),
            body: Body::Markdown(md),
        };
        return Ok(files::markdown_response(page_markdown(&doc, locale)));
    }

    let options = catalog::product_config_options(&state.pool, tenant.id, p.product.id, currency)
        .await?
        .into_iter()
        .filter(|o| !o.option.is_hidden)
        .map(|o| OptionView {
            name: localized(
                &o.option.name,
                &o.option.translations,
                locale,
                default,
                "name",
            ),
            values: o
                .values
                .iter()
                .map(|v| {
                    let monthly = v
                        .prices
                        .iter()
                        .find(|x| x.billing_cycle == BillingCycle::Monthly)
                        .map(|x| x.price)
                        .filter(|x| !x.is_zero());
                    (
                        localized(&v.label, &v.translations, locale, default, "label"),
                        monthly,
                    )
                })
                .collect(),
        })
        .collect();
    let addons = catalog::addons_for_product(&state.pool, tenant.id, p.product.id, currency)
        .await?
        .into_iter()
        .filter(|a| !a.addon.is_hidden && a.addon.show_on_order)
        .map(|a| AddonView {
            name: localized(
                &a.addon.name,
                &a.addon.translations,
                locale,
                default,
                "name",
            ),
            description: localized(
                a.addon.description.as_deref().unwrap_or_default(),
                &a.addon.translations,
                locale,
                default,
                "description",
            ),
            price: a.prices.first().map(|x| (x.price, x.billing_cycle)),
        })
        .collect();
    let siblings: Vec<ProductCard> = group
        .products
        .iter()
        .filter(|s| s.product.id != p.product.id)
        .take(3)
        .map(|s| {
            ProductCard::new(
                s,
                locale,
                default,
                ctx.url(format!("{group_path}/{}", s.product.slug)),
            )
        })
        .collect();

    let origin = ctx.origin.clone();
    let brand = ctx.site_name.clone();
    let abs_url = format!("{origin}{}", card.href);
    let mut node = card.json_ld(&abs_url, currency, &brand);
    node.category = Some(group_name.clone());
    node.images = vec![ctx.theme.share_image_url(&origin, &path)];
    if node.description.is_none() {
        node.description = Some(lead.clone());
    }
    let mut page = SeoPage::new(format!("{} — {}", card.name, group_name), path.clone())
        .description(lead.clone())
        .target(SeoTarget::Entity(
            hos_db::enums::SeoEntityType::Product,
            p.product.id.into_uuid(),
        ))
        .crumb(group_name.clone(), group_path.clone())
        .node(&node);
    if let Some(f) = faq_node(
        &abs_url,
        faq.iter().map(|x| (x.q.clone(), x.a.clone())).collect(),
    ) {
        page = page.node(&f);
    }
    seo::apply(&mut ctx, page).await?;
    let crumbs = vec![
        Crumb::link(ctx.t("common.nav.home"), ctx.url("/")),
        Crumb::link(group_name.clone(), ctx.url(&group_path)),
        Crumb::current(card.name.clone()),
    ];
    let default_cycle = card
        .all_prices
        .iter()
        .find(|x| x.billing_cycle == BillingCycle::Annual)
        .or(card.all_prices.first())
        .map(|x| x.billing_cycle.as_str().to_owned())
        .unwrap_or_default();
    render(&ProductPage {
        group_href: ctx.url(&group_path),
        description_html: markdown::render(&description),
        md_href: ctx.url(format!("{path}.md")),
        ctx,
        card,
        group_name,
        lead,
        options,
        addons,
        default_cycle,
        cycles,
        faq,
        crumbs,
        siblings_saving: annual_saving(&siblings),
        siblings,
        updated_at,
    })
}

/// Meta description of a group without its own description: the tagline,
/// the plan names and the lowest monthly price.
fn group_meta_description(
    ctx: &PageCtx,
    group: &str,
    tagline: &str,
    cards: &[ProductCard],
) -> String {
    let plans: Vec<&str> = cards.iter().map(|c| c.name.as_str()).collect();
    let mut out = tagline.trim().trim_end_matches('.').to_owned();
    if !plans.is_empty() {
        if !out.is_empty() {
            out.push_str(". ");
        }
        out.push_str(
            &ctx.tv("storefront.catalog.meta_plans", "group", group)
                .replace("{plans}", &plans.join(", ")),
        );
    }
    if let Some(from) = cards.iter().filter_map(|c| c.from).min() {
        out.push(' ');
        out.push_str(&ctx.tv("storefront.catalog.meta_from", "amount", ctx.money(&from)));
    }
    out
}

#[cfg(test)]
mod compare_tests {
    use super::common_noun;

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn shared_nouns_label_feature_rows() {
        assert_eq!(
            common_noun(&row(&[
                "10 GB NVMe disk",
                "30 GB NVMe disk",
                "100 GB NVMe disk"
            ]))
            .as_deref(),
            Some("NVMe disk")
        );
        assert_eq!(
            common_noun(&row(&[
                "1 web sitesi",
                "10 web sitesi",
                "Sınırsız web sitesi"
            ]))
            .as_deref(),
            Some("Web sitesi")
        );
        // Missing cells are ignored.
        assert_eq!(
            common_noun(&row(&["2 vCPU", "—", "8 vCPU"])).as_deref(),
            Some("vCPU")
        );
    }

    #[test]
    fn unrelated_cells_have_no_label() {
        assert_eq!(
            common_noun(&row(&["Ücretsiz SSL", "Günlük yedekleme"])),
            None
        );
        // Identical cells: nothing varies, so no amount to compare.
        assert_eq!(common_noun(&row(&["Ücretsiz SSL", "Ücretsiz SSL"])), None);
        assert_eq!(common_noun(&row(&["10 GB", "20 GB"])), None);
        assert_eq!(common_noun(&row(&["Tek satır"])), None);
    }
}
