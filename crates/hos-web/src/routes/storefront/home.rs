//! `/` — hero + domain search, product groups, featured plans, FAQ and
//! trust facts. Everything shown comes from the tenant's database (no
//! invented testimonials or statistics).

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use chrono::{DateTime, Utc};
use hos_db::Page;
use hos_db::repo::{catalog, content, domains, platform};
use hos_seo::jsonld::{FaqEntry, FaqPage, ItemList, ListEntry};
use rust_decimal::Decimal;

use super::blocks::{self, Block, Link};
use super::{ProductCard, landing, localized};
use crate::error::AppResult;
use crate::markdown;
use crate::seo::{self, SeoPage, SeoTarget, localized_setting};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// A product group tile.
#[derive(Debug, Clone)]
pub struct GroupCard {
    /// Localized name.
    pub name: String,
    /// Localized tagline.
    pub tagline: String,
    /// Icon id.
    pub icon: String,
    /// Localized URL.
    pub href: String,
    /// Lowest monthly price in the group.
    pub from: Option<Decimal>,
    /// Number of plans.
    pub count: usize,
}

/// A TLD chip under the domain search.
#[derive(Debug, Clone)]
pub struct TldChip {
    /// `com.tr` (without dot).
    pub tld: String,
    /// Registration price (1 year).
    pub price: Option<Decimal>,
}

/// A FAQ entry with rendered answer.
#[derive(Debug, Clone)]
pub struct FaqItem {
    /// Question.
    pub question: String,
    /// Answer (sanitized HTML).
    pub answer_html: String,
    /// Answer as plain text (JSON-LD).
    pub answer_text: String,
}

/// A blog teaser.
#[derive(Debug, Clone)]
pub struct PostCard {
    /// Title.
    pub title: String,
    /// Excerpt.
    pub excerpt: String,
    /// Localized URL.
    pub href: String,
    /// Publication date.
    pub published_at: DateTime<Utc>,
    /// Reading time.
    pub minutes: i32,
}

/// Real numbers about the catalog.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// Orderable plans.
    pub plans: usize,
    /// Domain extensions on sale.
    pub tlds: usize,
    /// Published help articles.
    pub articles: i64,
    /// Support e-mail.
    pub support_email: String,
}

#[derive(Template)]
#[template(path = "storefront/home.html")]
struct HomePage {
    ctx: PageCtx,
    hero_title: String,
    /// The hero paragraph: the subtitle, followed by the definition
    /// sentence unless the subtitle already names the business.
    hero_lead: String,
    /// One-sentence definition of the business (direct answer for AI search).
    summary: String,
    tlds: Vec<TldChip>,
    groups: Vec<GroupCard>,
    featured: Vec<ProductCard>,
    saving: Option<i64>,
    faqs: Vec<FaqItem>,
    posts: Vec<PostCard>,
    facts: Facts,
    /// Hero buttons (the `hero` block of the `home` page, or defaults).
    hero_links: Vec<Link>,
    /// Editable sections (the `home` CMS page without its hero).
    blocks: Vec<Block>,
    /// Whether `blocks` end with their own call to action.
    has_cta: bool,
}
crate::page!(HomePage);

pub(super) async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let (locale, default, currency) = (ctx.locale, ctx.default_locale, ctx.currency);
    let pool = &state.pool;

    let hero = platform::get_setting(pool, tenant.id, "storefront.hero").await?;
    let hero_part = |field: &str| {
        hero.as_ref()
            .and_then(|v| v.get(locale.as_str()).or_else(|| v.get(default.as_str())))
            .and_then(|v| v.get(field))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    // Editable copy: the `home` CMS page (hero block + sections), then the
    // `storefront.hero` setting, then the built-in defaults.
    let home_page = landing::load(&state, &tenant, &ctx, "home").await?;
    let cms_hero = home_page.as_ref().and_then(|l| l.hero().cloned());
    let hero_title = cms_hero
        .as_ref()
        .map(|b| b.title.clone())
        .filter(|t| !t.trim().is_empty())
        .or_else(|| hero_part("title"))
        .unwrap_or_else(|| ctx.t("storefront.home.hero_title"));
    // Without the final period: the summary sentence follows it.
    let hero_subtitle = cms_hero
        .as_ref()
        .map(|b| {
            markdown::plain_text(
                &b.source
                    .lines()
                    .filter(|l| !l.trim_start().starts_with('['))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
        .filter(|t| !t.trim().is_empty())
        .or_else(|| hero_part("subtitle"))
        .unwrap_or_else(|| ctx.t("storefront.home.hero_subtitle"))
        .trim()
        .trim_end_matches('.')
        .to_owned();
    let mut sections = home_page
        .as_ref()
        .map(landing::Landing::body)
        .unwrap_or_default();

    let tld_rows = domains::tld_price_table(pool, tenant.id, currency).await?;
    let tlds = tld_rows
        .iter()
        .filter(|r| r.is_featured)
        .take(6)
        .map(|r| TldChip {
            tld: r.tld.clone(),
            price: r.register,
        })
        .collect();

    let catalog = catalog::catalog(pool, tenant.id, currency, false).await?;
    let groups: Vec<GroupCard> = catalog
        .iter()
        .map(|g| GroupCard {
            name: localized(
                &g.group.name,
                &g.group.translations,
                locale,
                default,
                "name",
            ),
            tagline: localized(
                g.group.tagline.as_deref().unwrap_or_default(),
                &g.group.translations,
                locale,
                default,
                "tagline",
            ),
            icon: g.group.icon.clone().unwrap_or_else(|| "box".into()),
            href: ctx.url(format!("/{}", g.group.slug)),
            from: g
                .products
                .iter()
                .filter_map(|p| p.starting_monthly())
                .min()
                .map(|d| d.round_dp(2)),
            count: g.products.len(),
        })
        .collect();

    let featured: Vec<ProductCard> = catalog::featured_products(pool, tenant.id, currency, 3)
        .await?
        .iter()
        .filter_map(|p| {
            let group = catalog
                .iter()
                .find(|g| Some(g.group.id) == p.product.group_id)?;
            let href = ctx.url(format!("/{}/{}", group.group.slug, p.product.slug));
            let mut card = ProductCard::new(p, locale, default, href);
            card.eyebrow = localized(
                &group.group.name,
                &group.group.translations,
                locale,
                default,
                "name",
            );
            Some(card)
        })
        .collect();

    landing::resolve(&state, &tenant, &ctx, &mut sections, Some(&catalog)).await?;
    let cms_faq = blocks::faq_entries(&sections);
    // The KB FAQ only when the home copy has no FAQ block of its own.
    let faq_rows = if cms_faq.is_empty() {
        content::faq_entries(pool, tenant.id, locale, 6).await?
    } else {
        Vec::new()
    };
    let faqs: Vec<FaqItem> = faq_rows
        .into_iter()
        .map(|a| FaqItem {
            answer_html: markdown::render(&a.body),
            answer_text: markdown::plain_text(&a.body),
            question: a.title,
        })
        .collect();

    let posts = content::published_blog_posts(pool, tenant.id, locale, None, Page::number(1, 3))
        .await?
        .items
        .into_iter()
        .map(|p| PostCard {
            href: ctx.url(format!("/blog/{}", p.slug)),
            minutes: p
                .reading_minutes
                .unwrap_or_else(|| i32::try_from(markdown::reading_minutes(&p.body)).unwrap_or(1)),
            excerpt: p.excerpt.clone().unwrap_or_default(),
            published_at: p.published_at.unwrap_or(p.created_at),
            title: p.title,
        })
        .collect();

    let articles = content::kb_categories(pool, tenant.id, locale)
        .await?
        .iter()
        .map(|c| c.article_count)
        .sum();
    let facts = Facts {
        plans: catalog.iter().map(|g| g.products.len()).sum(),
        tlds: tld_rows.iter().filter(|r| r.register.is_some()).count(),
        articles,
        support_email: tenant.branding.support_email.clone().unwrap_or_default(),
    };

    let summary = summary_sentence(&ctx, &groups);
    // SEO: FAQPage + an ItemList of the product groups. The description is
    // the hero subtitle, extended with the summary when it is short.
    let origin = ctx.origin.clone();
    let description =
        if hero_subtitle.chars().count() >= 110 || hero_subtitle.contains(&ctx.site_name) {
            format!("{hero_subtitle}.")
        } else {
            format!("{hero_subtitle}. {summary}")
        };
    // `Site — Page` must stay within the ~60-character title budget; a long
    // marketing headline gives way to the short service summary.
    let seo_title = if hero_title.chars().count() + ctx.site_name.chars().count() + 3 <= 60 {
        hero_title.clone()
    } else {
        ctx.t("storefront.home.seo_title")
    };
    let mut page = SeoPage::new(seo_title, "/")
        .description(description)
        .target(SeoTarget::Home);
    page.home = true;
    let faq_entries: Vec<FaqEntry> = if cms_faq.is_empty() {
        faqs.iter()
            .map(|f| FaqEntry::new(f.question.clone(), f.answer_text.clone()))
            .collect()
    } else {
        cms_faq
            .iter()
            .map(|(q, a)| FaqEntry::new(q.clone(), a.clone()))
            .collect()
    };
    if !faq_entries.is_empty() {
        page = page.node(&FaqPage {
            id: Some(format!("{origin}{}#faq", ctx.url("/"))),
            url: Some(format!("{origin}{}", ctx.url("/"))),
            entries: faq_entries,
        });
    }
    if !groups.is_empty() {
        page = page.node(&ItemList {
            name: Some(ctx.t("storefront.home.groups_title")),
            items: groups
                .iter()
                .map(|g| ListEntry {
                    url: format!("{origin}{}", g.href),
                    name: Some(g.name.clone()),
                })
                .collect(),
        });
    }
    if let Some(desc) = platform::get_setting(pool, tenant.id, "seo.default_description")
        .await?
        .and_then(|v| localized_setting(&v, locale))
        && hero_subtitle.is_empty()
    {
        page = page.description(desc);
    }
    seo::apply(&mut ctx, page).await?;
    ctx.body_class = "page-home".into();

    let hero_links = cms_hero
        .map(|b| b.links)
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| {
            let mut links = Vec::new();
            if let Some(g) = groups.first() {
                links.push(Link {
                    label: ctx.t("storefront.home.cta_plans"),
                    href: g.href.clone(),
                });
            }
            links.push(Link {
                label: ctx.t("storefront.home.cta_contact"),
                href: ctx.url("/contact"),
            });
            links
        });
    let has_cta = sections.iter().any(|b| b.kind == "cta");
    let hero_lead = if hero_subtitle.is_empty() {
        summary.clone()
    } else if hero_subtitle.contains(&ctx.site_name) {
        format!("{hero_subtitle}.")
    } else {
        format!("{hero_subtitle}. {summary}")
    };
    render(&HomePage {
        hero_lead,
        hero_links,
        blocks: sections,
        has_cta,
        ctx,
        hero_title,
        summary,
        tlds,
        groups,
        saving: super::annual_saving(&featured),
        featured,
        faqs,
        posts,
        facts,
    })
}

/// "{site} is a hosting provider that offers A, B and C …" built from the
/// product group names.
fn summary_sentence(ctx: &PageCtx, groups: &[GroupCard]) -> String {
    let names: Vec<&str> = groups.iter().take(5).map(|g| g.name.as_str()).collect();
    let services = match names.as_slice() {
        [] => ctx.t("storefront.home.summary_default_services"),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!(
            "{} {} {last}",
            init.join(", "),
            ctx.t("storefront.home.and")
        ),
    };
    ctx.t("storefront.home.summary")
        .replace("{site}", &ctx.site_name)
        .replace("{services}", &services)
}
