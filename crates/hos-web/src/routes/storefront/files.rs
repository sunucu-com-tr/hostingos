//! Machine-readable files: `/sitemap.xml` (+ `/sitemaps/{n}.xml`),
//! `/robots.txt`, `/llms.txt`, `/llms-full.txt`, the IndexNow key file,
//! and `/favicon.ico` (`/brand.css` is `crate::theme::stylesheet`).

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use hos_core::domain::Locale;
use hos_core::money::Money;
use hos_db::Page;
use hos_db::repo::{catalog, content, platform};
use hos_seo::jsonld::FaqEntry;
use hos_seo::llms::{
    Body, Document, LlmsFull, LlmsLink, LlmsSection, LlmsTxt, PriceLine, ProductEntry,
    render_llms_full,
};
use hos_seo::meta::Alternate;
use hos_seo::robots::{AiPolicy, RobotsConfig};
use hos_seo::sitemap::{Limits, SitemapImage, SitemapSet, SitemapUrl, build_sitemaps};
use http::HeaderValue;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use serde_json::Value;

use super::{localized, localized_features};
use crate::error::{AppError, AppResult};
use crate::locale::localize_path;
use crate::markdown;
use crate::seo::localized_setting;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::PageCtx;

fn with_type(
    body: impl Into<axum::body::Body>,
    content_type: &'static str,
    cache: &'static str,
) -> Response {
    let mut res = Response::new(body.into());
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    res
}

/// `text/plain` response (cached for an hour).
pub fn text(body: String) -> Response {
    with_type(body, "text/plain; charset=utf-8", "public, max-age=3600")
}

/// `text/markdown` response for the `.md` twins of pages.
pub fn markdown_response(body: String) -> Response {
    with_type(body, "text/markdown; charset=utf-8", "public, max-age=600")
}

fn xml(body: String) -> Response {
    with_type(
        body,
        "application/xml; charset=utf-8",
        "public, max-age=3600",
    )
}

/// The tenant's IndexNow key when IndexNow is enabled (`seo.indexnow`):
/// the `seo.indexnow_key` setting, else a stable key derived from the
/// server secret and the tenant id.
pub async fn indexnow_key(state: &AppState, tenant: &Tenant) -> AppResult<Option<String>> {
    let settings = platform::get_settings(&state.pool, tenant.id, "seo.indexnow").await?;
    let enabled = settings
        .get("seo.indexnow")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }
    if let Some(k) = settings
        .get("seo.indexnow_key")
        .and_then(Value::as_str)
        .filter(|k| hos_seo::indexnow::validate_key(k).is_ok())
    {
        return Ok(Some(k.to_owned()));
    }
    let mac = hos_core::crypto::hmac_sha256(
        &state.cookie_key,
        format!("indexnow:{}", tenant.id).as_bytes(),
    );
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    Ok(Some(hex[..32].to_owned()))
}

// ---------------------------------------------------------------------------
// sitemap
// ---------------------------------------------------------------------------

async fn sitemap_set(state: &AppState, tenant: &Tenant, ctx: &PageCtx) -> AppResult<SitemapSet> {
    let default = ctx.default_locale;
    let origin = &ctx.origin;
    let abs =
        |locale: Locale, path: &str| format!("{origin}{}", localize_path(path, locale, default));
    // Every URL carries its share image (image sitemap extension).
    let image = |path: &str| SitemapImage {
        loc: ctx.theme.share_image_url(origin, path),
    };
    let both = |path: &str, lastmod: Option<chrono::DateTime<chrono::Utc>>| -> Vec<SitemapUrl> {
        let alternates: Vec<Alternate> = Locale::ALL
            .iter()
            .map(|l| Alternate::new(l.as_str(), abs(*l, path)))
            .chain(std::iter::once(Alternate::new(
                "x-default",
                abs(default, path),
            )))
            .collect();
        Locale::ALL
            .iter()
            .map(|l| {
                let mut u = SitemapUrl::new(abs(*l, path));
                u.lastmod = lastmod;
                u.alternates = alternates.clone();
                u.images = vec![image(path)];
                u
            })
            .collect()
    };

    let catalog = catalog::catalog(&state.pool, tenant.id, tenant.default_currency, false).await?;
    let entries = content::sitemap_entries(&state.pool, tenant.id).await?;
    // CMS pages merged into a route or a category landing are not URLs of
    // their own; their edits move the host page's lastmod.
    let merged_into = |slug: &str| -> Option<String> {
        super::landing::reserved_route(slug)
            .map(str::to_owned)
            .or_else(|| {
                catalog
                    .iter()
                    .any(|g| g.group.slug == slug)
                    .then(|| format!("/{slug}"))
            })
    };
    let mut merged: BTreeMap<String, chrono::DateTime<chrono::Utc>> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.kind == "page") {
        if let Some(route) = merged_into(&e.slug) {
            let at = merged.entry(route).or_insert(e.updated_at);
            *at = (*at).max(e.updated_at);
        }
    }

    let mut urls: Vec<SitemapUrl> = Vec::new();
    for path in [
        "/",
        "/domains",
        "/whois",
        "/kb",
        "/blog",
        "/announcements",
        "/contact",
        "/status",
    ] {
        urls.extend(both(path, merged.get(path).copied()));
    }
    for c in content::kb_categories(&state.pool, tenant.id, default).await? {
        urls.extend(both(
            &format!("/kb/category/{}", c.category.slug),
            Some(c.category.updated_at),
        ));
    }

    // Translated content: group by translation group for hreflang alternates.
    let mut groups: BTreeMap<uuid::Uuid, Vec<(Locale, String, chrono::DateTime<chrono::Utc>)>> =
        BTreeMap::new();
    for e in &entries {
        let prefix = match e.kind.as_str() {
            "page" if merged_into(&e.slug).is_some() => continue,
            "page" => "/",
            "blog_post" => "/blog/",
            "kb_article" => "/kb/",
            "announcement" => "/announcements/",
            "product_group" => {
                let path = format!("/{}", e.slug);
                let lastmod = merged
                    .get(&path)
                    .map_or(e.updated_at, |m| (*m).max(e.updated_at));
                urls.extend(both(&path, Some(lastmod)));
                continue;
            }
            "product" => {
                if let Some(g) = catalog
                    .iter()
                    .find(|g| g.products.iter().any(|p| p.product.slug == e.slug))
                {
                    urls.extend(both(
                        &format!("/{}/{}", g.group.slug, e.slug),
                        Some(e.updated_at),
                    ));
                }
                continue;
            }
            _ => continue,
        };
        if let (Some(locale), Some(group)) = (e.locale, e.translation_group_id) {
            groups.entry(group).or_default().push((
                locale,
                format!("{prefix}{}", e.slug),
                e.updated_at,
            ));
        }
    }
    for members in groups.values() {
        let mut alternates: Vec<Alternate> = members
            .iter()
            .map(|(l, p, _)| Alternate::new(l.as_str(), abs(*l, p)))
            .collect();
        if let Some((l, p, _)) = members.iter().find(|(l, _, _)| *l == default) {
            alternates.push(Alternate::new("x-default", abs(*l, p)));
        }
        for (locale, path, updated) in members {
            let mut u = SitemapUrl::new(abs(*locale, path)).with_lastmod(*updated);
            if members.len() > 1 {
                u.alternates = alternates.clone();
            }
            if path.starts_with("/blog/")
                || path.starts_with("/announcements/")
                || prefix_is_page(path)
            {
                u.images = vec![image(path)];
            }
            urls.push(u);
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    urls.retain(|u| seen.insert(u.loc.clone()));
    Ok(build_sitemaps(urls, Limits::default()))
}

/// A CMS page path (`/slug`, one segment).
fn prefix_is_page(path: &str) -> bool {
    path.matches('/').count() == 1
}

/// `GET /sitemap.xml` — the sitemap, or an index when it had to be split.
pub(super) async fn sitemap(
    State(state): State<AppState>,
    tenant: Tenant,
    ctx: PageCtx,
) -> AppResult<Response> {
    let set = sitemap_set(&state, &tenant, &ctx).await?;
    if set.needs_index() {
        let origin = ctx.origin.clone();
        let index = set
            .index_xml(|n| format!("{origin}/sitemaps/{n}.xml"))
            .map_err(AppError::internal)?;
        return Ok(xml(index));
    }
    let body = set
        .files
        .into_iter()
        .next()
        .map(|f| f.xml)
        .ok_or_else(|| AppError::internal("empty sitemap set"))?;
    Ok(xml(body))
}

/// `GET /sitemaps/{n}.xml`.
pub(super) async fn sitemap_part(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(file): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let n: usize = file
        .strip_suffix(".xml")
        .and_then(|n| n.parse().ok())
        .ok_or(AppError::NotFound)?;
    let set = sitemap_set(&state, &tenant, &ctx).await?;
    let f = set
        .files
        .into_iter()
        .find(|f| f.number == n)
        .ok_or(AppError::NotFound)?;
    Ok(xml(f.xml))
}

// ---------------------------------------------------------------------------
// robots.txt
// ---------------------------------------------------------------------------

/// AI crawler policy from the `seo.ai_crawlers` setting: `"allow"` (all),
/// `"search"` (search + user fetches, no training), `"block"` (none), or an
/// [`AiPolicy`] object.
pub fn ai_policy(setting: Option<&Value>) -> AiPolicy {
    match setting {
        Some(Value::String(s)) => match s.as_str() {
            "allow" | "all" => AiPolicy {
                allow_search: true,
                allow_user_fetch: true,
                allow_training: true,
                ..AiPolicy::default()
            },
            "block" | "none" => AiPolicy {
                allow_search: false,
                allow_user_fetch: false,
                allow_training: false,
                ..AiPolicy::default()
            },
            "search" => AiPolicy {
                allow_search: true,
                allow_user_fetch: true,
                allow_training: false,
                ..AiPolicy::default()
            },
            _ => AiPolicy::default(),
        },
        Some(v @ Value::Object(_)) => serde_json::from_value(v.clone()).unwrap_or_default(),
        _ => AiPolicy::default(),
    }
}

/// Panel / API paths disallowed for every crawler (with locale prefixes).
pub fn base_disallow(default_locale: Locale) -> Vec<String> {
    let mut disallow = Vec::new();
    for area in [
        "/client",
        "/admin",
        "/platform",
        "/auth",
        "/api",
        "/cart",
        "/webhooks",
        "/lang",
    ] {
        disallow.push(area.to_owned());
        for l in Locale::ALL {
            if *l != default_locale && area != "/api" && area != "/webhooks" {
                disallow.push(format!("/{}{area}", l.as_str()));
            }
        }
    }
    disallow
}

/// `GET /robots.txt`.
pub(super) async fn robots(
    State(state): State<AppState>,
    tenant: Tenant,
    ctx: PageCtx,
) -> AppResult<Response> {
    let setting = platform::get_setting(&state.pool, tenant.id, "seo.ai_crawlers").await?;
    let mut disallow = base_disallow(ctx.default_locale);
    // Extra paths from the admin crawler settings (`seo.robots_disallow`).
    if let Some(Value::Array(extra)) =
        platform::get_setting(&state.pool, tenant.id, "seo.robots_disallow").await?
    {
        disallow.extend(
            extra
                .iter()
                .filter_map(Value::as_str)
                .filter(|p| p.starts_with('/') && !p.contains(['\r', '\n']))
                .map(str::to_owned),
        );
    }
    // The public demo is never indexed (docs/DEMO.md).
    let demo = state.web.demo.is_some();
    let cfg = RobotsConfig {
        ai: ai_policy(setting.as_ref()),
        sitemaps: if demo {
            Vec::new()
        } else {
            vec![format!("{}/sitemap.xml", ctx.origin)]
        },
        disallow,
        allow: Vec::new(),
        block_all: !tenant.is_operational() || demo,
    };
    Ok(text(hos_seo::robots::render(&cfg)))
}

// ---------------------------------------------------------------------------
// llms.txt
// ---------------------------------------------------------------------------

async fn summary(state: &AppState, tenant: &Tenant, locale: Locale) -> AppResult<String> {
    Ok(
        platform::get_setting(&state.pool, tenant.id, "seo.default_description")
            .await?
            .and_then(|v| localized_setting(&v, locale))
            .unwrap_or_else(|| tenant.display_name().to_owned()),
    )
}

/// Extra company details for llms.txt (`seo.llms_details`, localized).
async fn llms_details(
    state: &AppState,
    tenant: &Tenant,
    locale: Locale,
) -> AppResult<Option<String>> {
    Ok(
        platform::get_setting(&state.pool, tenant.id, "seo.llms_details")
            .await?
            .and_then(|v| localized_setting(&v, locale))
            .filter(|s| !s.trim().is_empty()),
    )
}

/// `GET /llms.txt` (in the tenant's default language; `/en/llms.txt` → English).
pub(super) async fn llms(
    State(state): State<AppState>,
    tenant: Tenant,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    // Public, shared-cacheable file: the default currency, never a guest's.
    ctx.currency = tenant.default_currency;
    let (locale, default) = (ctx.locale, ctx.default_locale);
    let abs = |p: &str| format!("{}{}", ctx.origin, ctx.url(p));
    let catalog = catalog::catalog(&state.pool, tenant.id, ctx.currency, false).await?;
    let mut sections = Vec::new();

    let mut products = Vec::new();
    for g in &catalog {
        let gname = localized(
            &g.group.name,
            &g.group.translations,
            locale,
            default,
            "name",
        );
        let mut link = LlmsLink::new(gname.clone(), abs(&format!("/{}.md", g.group.slug)));
        if let Some(t) = g.group.tagline.as_deref() {
            link = link.describe(localized(
                t,
                &g.group.translations,
                locale,
                default,
                "tagline",
            ));
        }
        products.push(link);
        for p in &g.products {
            let name = localized(
                &p.product.name,
                &p.product.translations,
                locale,
                default,
                "name",
            );
            let mut l = LlmsLink::new(
                format!("{gname} — {name}"),
                abs(&format!("/{}/{}.md", g.group.slug, p.product.slug)),
            );
            if let Some(from) = p.starting_monthly() {
                l = l.describe(format!(
                    "{} {}{}",
                    ctx.t("storefront.catalog.from"),
                    ctx.money(&from.round_dp(2)),
                    ctx.t("common.per.monthly")
                ));
            }
            products.push(l);
        }
    }
    products.push(
        LlmsLink::new(ctx.t("storefront.nav.domains"), abs("/domains"))
            .describe(ctx.t("storefront.domains.lead")),
    );
    sections.push(LlmsSection {
        title: ctx.t("storefront.nav.products"),
        links: products,
    });

    let kb: Vec<LlmsLink> = content::kb_categories(&state.pool, tenant.id, locale)
        .await?
        .into_iter()
        .filter(|c| c.article_count > 0)
        .map(|c| {
            let name = localized(
                &c.category.name,
                &c.category.translations,
                locale,
                default,
                "name",
            );
            LlmsLink::new(name, abs(&format!("/kb/category/{}", c.category.slug)))
                .describe(ctx.tn("storefront.kb.articles", c.article_count))
        })
        .collect();
    if !kb.is_empty() {
        sections.push(LlmsSection {
            title: ctx.t("storefront.nav.kb"),
            links: kb,
        });
    }
    let posts: Vec<LlmsLink> =
        content::published_blog_posts(&state.pool, tenant.id, locale, None, Page::number(1, 10))
            .await?
            .items
            .into_iter()
            .map(|p| {
                let mut l =
                    LlmsLink::new(p.title, format!("{}.md", abs(&format!("/blog/{}", p.slug))));
                if let Some(e) = p.excerpt {
                    l = l.describe(e);
                }
                l
            })
            .collect();
    if !posts.is_empty() {
        sections.push(LlmsSection {
            title: ctx.t("storefront.nav.blog"),
            links: posts,
        });
    }
    let store = ctx.store();
    // Every published page (its Markdown twin); legal pages separately.
    let mut company = Vec::new();
    let mut policies = Vec::new();
    for p in published_pages(&state, &tenant, locale).await? {
        // Group landings and the copy of merged routes (home, domains,
        // contact) are already listed with their own pages above.
        if catalog.iter().any(|g| g.group.slug == p.slug)
            || super::landing::reserved_route(&p.slug).is_some()
        {
            continue;
        }
        let link = LlmsLink::new(
            p.title.clone(),
            format!("{}.md", abs(&format!("/{}", p.slug))),
        );
        if p.template == "legal" {
            policies.push(link);
        } else {
            company.push(link);
        }
    }
    sections.push(LlmsSection {
        title: ctx.t("storefront.footer.company"),
        links: company,
    });
    if !policies.is_empty() {
        sections.push(LlmsSection {
            title: ctx.t("storefront.footer.legal"),
            links: policies,
        });
    }
    let c = &store.company;
    let mut details = Vec::new();
    if let Some(extra) = llms_details(&state, &tenant, locale).await? {
        details.push(extra);
    }
    if !c.legal_name.is_empty() {
        details.push(format!(
            "{}: {}",
            ctx.t("storefront.home.fact_company"),
            c.legal_name
        ));
    }
    if !c.address.is_empty() {
        details.push(format!(
            "{}: {}",
            ctx.t("storefront.contact.address"),
            c.address
        ));
    }
    if !c.email.is_empty() {
        details.push(format!(
            "{}: {}",
            ctx.t("storefront.contact.email"),
            c.email
        ));
    }
    if !c.phone.is_empty() {
        details.push(format!(
            "{}: {}",
            ctx.t("storefront.contact.phone"),
            c.phone
        ));
    }
    details.push(format!(
        "{}: {}",
        ctx.t("storefront.contact.title"),
        abs("/contact")
    ));
    let doc = LlmsTxt {
        site_name: tenant.display_name().to_owned(),
        summary: summary(&state, &tenant, locale).await?,
        details: Some(details.join("\n")),
        sections,
        optional: vec![
            LlmsLink::new("llms-full.txt", abs("/llms-full.txt"))
                .describe(ctx.t("storefront.files.llms_full")),
            LlmsLink::new("Sitemap", format!("{}/sitemap.xml", ctx.origin)),
        ],
    };
    Ok(text(doc.render()))
}

/// `GET /llms-full.txt` — products with prices, FAQ, KB articles and policies inline.
pub(super) async fn llms_full(
    State(state): State<AppState>,
    tenant: Tenant,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    // Public, shared-cacheable file: the default currency, never a guest's.
    ctx.currency = tenant.default_currency;
    let (locale, default, currency) = (ctx.locale, ctx.default_locale, ctx.currency);
    let abs = |p: &str| format!("{}{}", ctx.origin, ctx.url(p));
    let catalog = catalog::catalog(&state.pool, tenant.id, currency, false).await?;
    let mut products = Vec::new();
    for g in &catalog {
        let gname = localized(
            &g.group.name,
            &g.group.translations,
            locale,
            default,
            "name",
        );
        for p in &g.products {
            let tagline = localized(
                p.product.tagline.as_deref().unwrap_or_default(),
                &p.product.translations,
                locale,
                default,
                "tagline",
            );
            let features = localized_features(p, locale, default);
            let mut body = tagline;
            if !features.is_empty() {
                body.push_str("\n\n");
                body.push_str(
                    &features
                        .iter()
                        .map(|f| format!("- {f}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            products.push(ProductEntry {
                name: localized(
                    &p.product.name,
                    &p.product.translations,
                    locale,
                    default,
                    "name",
                ),
                group: Some(gname.clone()),
                url: Some(abs(&format!("/{}/{}", g.group.slug, p.product.slug))),
                description: Some(Body::Markdown(body)),
                prices: p
                    .prices
                    .iter()
                    .map(|x| PriceLine {
                        cycle: x.billing_cycle,
                        price: Money::new(x.price, currency),
                        setup_fee: (!x.setup_fee.is_zero())
                            .then(|| Money::new(x.setup_fee, currency)),
                    })
                    .collect(),
                features: Vec::new(),
            });
        }
    }
    let faqs = content::faq_entries(&state.pool, tenant.id, locale, 50)
        .await?
        .into_iter()
        .map(|a| FaqEntry::new(a.title, markdown::plain_text(&a.body)))
        .collect();
    let kb_articles = content::published_kb_articles(
        &state.pool,
        tenant.id,
        locale,
        None,
        Some(hos_db::enums::KbArticleKind::Article),
        Page::number(1, 100),
    )
    .await?
    .items
    .into_iter()
    .map(|a| Document {
        url: Some(abs(&format!("/kb/{}", a.slug))),
        updated: Some(a.updated_at),
        title: a.title,
        body: Body::Markdown(a.body),
    })
    .collect();
    let mut policies = Vec::new();
    let mut pages = Vec::new();
    for p in published_pages(&state, &tenant, locale).await? {
        let route = super::landing::reserved_route(&p.slug)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("/{}", p.slug));
        let mut list = super::blocks::parse(&super::landing::tokens(&ctx).apply(&p.body));
        super::landing::resolve(&state, &tenant, &ctx, &mut list, Some(&catalog)).await?;
        let doc = Document {
            title: p.title,
            url: Some(abs(&route)),
            updated: Some(p.updated_at),
            body: Body::Markdown(super::blocks::to_markdown(&list, currency.as_str())),
        };
        if p.template == "legal" {
            policies.push(doc);
        } else {
            pages.push(doc);
        }
    }
    let mut full_summary = summary(&state, &tenant, locale).await?;
    if let Some(extra) = llms_details(&state, &tenant, locale).await? {
        full_summary.push_str("\n\n");
        full_summary.push_str(&extra);
    }
    let full = LlmsFull {
        site_name: tenant.display_name().to_owned(),
        summary: full_summary,
        base_url: Some(ctx.origin.clone()),
        locale,
        products,
        faqs,
        kb_articles,
        policies,
        pages,
    };
    Ok(text(render_llms_full(&full)))
}

/// Published CMS pages in `locale` (sitemap order).
async fn published_pages(
    state: &AppState,
    tenant: &Tenant,
    locale: Locale,
) -> AppResult<Vec<content::PageRow>> {
    let mut out = Vec::new();
    for e in content::sitemap_entries(&state.pool, tenant.id).await? {
        if e.kind == "page"
            && e.locale == Some(locale)
            && let Some(p) =
                content::published_page(&state.pool, tenant.id, locale, &e.slug).await?
        {
            out.push(p);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// favicon (the theme stylesheet `/brand.css` lives in crate::theme)
// ---------------------------------------------------------------------------

/// `GET /favicon.ico` → the tenant's uploaded favicon (temporary redirect,
/// it can change) or the default SVG icon.
pub(super) async fn favicon(ctx: PageCtx) -> Response {
    match ctx.favicon_url() {
        Some(url) => Redirect::temporary(&url).into_response(),
        None => Redirect::permanent(&ctx.asset("img/favicon.svg")).into_response(),
    }
}
