//! Knowledge base: `/kb`, `/kb/search`, `/kb/category/{slug}`,
//! `/kb/{slug}` (+ `.md` twin) and the "was this helpful?" vote.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use hos_db::enums::{KbArticleKind, SeoEntityType};
use hos_db::repo::content::{self, ContentTable, KbArticleRow};
use hos_seo::jsonld::{Article, ArticleKind, Entity, HowTo, HowToStep};
use hos_seo::llms::{Body, Document, page_markdown};
use serde::Deserialize;

use super::files;
use super::localized;
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, Htmx, PageQuery};
use crate::flash::{self, SetFlash};
use crate::markdown::{self, Heading};
use crate::seo::{self, SeoPage, SeoTarget};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, Pager, render};

/// A KB category tile.
#[derive(Debug, Clone)]
pub struct CategoryCard {
    /// Localized name.
    pub name: String,
    /// Localized description.
    pub description: String,
    /// Icon id.
    pub icon: String,
    /// Localized URL.
    pub href: String,
    /// Published articles in this locale.
    pub count: i64,
}

/// An article link.
#[derive(Debug, Clone)]
pub struct ArticleLink {
    /// Title.
    pub title: String,
    /// Excerpt / first sentence.
    pub excerpt: String,
    /// Localized URL.
    pub href: String,
}

fn article_link(ctx: &PageCtx, a: &KbArticleRow) -> ArticleLink {
    let excerpt = a
        .excerpt
        .clone()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| {
            hos_seo::meta::description_from_text(&markdown::plain_text(&a.body), 140)
        });
    ArticleLink {
        title: a.title.clone(),
        excerpt,
        href: ctx.url(format!("/kb/{}", a.slug)),
    }
}

#[derive(Template)]
#[template(path = "storefront/kb_index.html")]
struct IndexPage {
    ctx: PageCtx,
    categories: Vec<CategoryCard>,
    popular: Vec<ArticleLink>,
    faqs: Vec<(String, String)>,
}
crate::page!(IndexPage);

/// `GET /kb`.
pub(super) async fn index(
    State(state): State<AppState>,
    tenant: Tenant,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let (locale, default) = (ctx.locale, ctx.default_locale);
    let categories = content::kb_categories(&state.pool, tenant.id, locale)
        .await?
        .into_iter()
        .filter(|c| c.article_count > 0)
        .map(|c| CategoryCard {
            name: localized(
                &c.category.name,
                &c.category.translations,
                locale,
                default,
                "name",
            ),
            description: localized(
                c.category.description.as_deref().unwrap_or_default(),
                &c.category.translations,
                locale,
                default,
                "description",
            ),
            icon: c.category.icon.clone().unwrap_or_else(|| "book".into()),
            href: ctx.url(format!("/kb/category/{}", c.category.slug)),
            count: c.article_count,
        })
        .collect();
    let popular = content::published_kb_articles(
        &state.pool,
        tenant.id,
        locale,
        None,
        Some(KbArticleKind::Article),
        hos_db::Page::number(1, 8),
    )
    .await?
    .items
    .iter()
    .map(|a| article_link(&ctx, a))
    .collect();
    let faq_rows = content::faq_entries(&state.pool, tenant.id, locale, 8).await?;
    let faqs: Vec<(String, String)> = faq_rows
        .iter()
        .map(|a| (a.title.clone(), markdown::render(&a.body)))
        .collect();
    let mut page =
        SeoPage::new(ctx.t("storefront.kb.title"), "/kb").description(ctx.t("storefront.kb.lead"));
    if !faq_rows.is_empty() {
        page = page.node(&hos_seo::jsonld::FaqPage {
            id: Some(format!("{}{}#faq", ctx.origin, ctx.url("/kb"))),
            url: Some(format!("{}{}", ctx.origin, ctx.url("/kb"))),
            entries: faq_rows
                .iter()
                .map(|a| {
                    hos_seo::jsonld::FaqEntry::new(a.title.clone(), markdown::plain_text(&a.body))
                })
                .collect(),
        });
    }
    seo::apply(&mut ctx, page).await?;
    render(&IndexPage {
        ctx,
        categories,
        popular,
        faqs,
    })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SearchQuery {
    #[serde(default)]
    q: String,
}

#[derive(Template)]
#[template(path = "storefront/kb_search.html")]
struct SearchPage {
    ctx: PageCtx,
    q: String,
    results: Vec<ArticleLink>,
}
crate::page!(SearchPage);

/// `GET /kb/search?q=`.
pub(super) async fn search(
    State(state): State<AppState>,
    tenant: Tenant,
    Query(query): Query<SearchQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let q = crate::forms::clean(&query.q, 120);
    let results = if q.chars().count() >= 2 {
        content::search_kb(&state.pool, tenant.id, ctx.locale, &q, 30)
            .await?
            .iter()
            .map(|a| article_link(&ctx, a))
            .collect()
    } else {
        Vec::new()
    };
    let mut page = SeoPage::new(ctx.t("storefront.kb.search_title"), "/kb/search")
        .crumb(ctx.t("storefront.kb.title"), "/kb");
    page.noindex = true;
    seo::apply(&mut ctx, page).await?;
    render(&SearchPage { ctx, q, results })
}

#[derive(Template)]
#[template(path = "storefront/kb_category.html")]
struct CategoryPage {
    ctx: PageCtx,
    name: String,
    description: String,
    articles: Vec<ArticleLink>,
    pager: Pager,
    crumbs: Vec<Crumb>,
}
crate::page!(CategoryPage);

/// `GET /kb/category/{slug}`.
pub(super) async fn category(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(slug): Path<String>,
    query: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let (locale, default) = (ctx.locale, ctx.default_locale);
    let cat = content::kb_category_by_slug(&state.pool, tenant.id, &slug)
        .await?
        .filter(|c| !c.is_hidden)
        .ok_or(AppError::NotFound)?;
    ctx.load_store().await?;
    let paged = content::published_kb_articles(
        &state.pool,
        tenant.id,
        locale,
        Some(cat.id),
        None,
        hos_db::Page::number(query.page, 20),
    )
    .await?;
    let path = format!("/kb/category/{}", cat.slug);
    let pager = Pager::new(&paged, &ctx.url(&path), &[]);
    let articles = paged.items.iter().map(|a| article_link(&ctx, a)).collect();
    let name = localized(&cat.name, &cat.translations, locale, default, "name");
    let description = localized(
        cat.description.as_deref().unwrap_or_default(),
        &cat.translations,
        locale,
        default,
        "description",
    );
    let page = SeoPage::new(name.clone(), path)
        .description(description.clone())
        .target(SeoTarget::Entity(
            SeoEntityType::KbCategory,
            cat.id.into_uuid(),
        ))
        .crumb(ctx.t("storefront.kb.title"), "/kb");
    seo::apply(&mut ctx, page).await?;
    let crumbs = vec![
        Crumb::link(ctx.t("common.nav.home"), ctx.url("/")),
        Crumb::link(ctx.t("storefront.kb.title"), ctx.url("/kb")),
        Crumb::current(name.clone()),
    ];
    render(&CategoryPage {
        ctx,
        name,
        description,
        articles,
        pager,
        crumbs,
    })
}

#[derive(Template)]
#[template(path = "storefront/kb_article.html")]
struct ArticlePage {
    ctx: PageCtx,
    title: String,
    slug: String,
    body_html: String,
    toc: Vec<Heading>,
    updated_at: DateTime<Utc>,
    minutes: u32,
    is_faq: bool,
    related: Vec<ArticleLink>,
    crumbs: Vec<Crumb>,
    md_href: String,
}
crate::page!(ArticlePage);

/// `GET /kb/{slug}` (and `/kb/{slug}.md`).
pub(super) async fn article(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(slug): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let (locale, default) = (ctx.locale, ctx.default_locale);
    let (slug, as_md) = match slug.strip_suffix(".md") {
        Some(s) => (s.to_owned(), true),
        None => (slug, false),
    };
    let a = content::published_kb_article(&state.pool, tenant.id, locale, &slug)
        .await?
        .ok_or(AppError::NotFound)?;
    let path = format!("/kb/{}", a.slug);
    let url = format!("{}{}", ctx.origin, ctx.url(&path));
    if as_md {
        let doc = Document {
            title: a.title.clone(),
            url: Some(url),
            updated: Some(a.updated_at),
            body: Body::Markdown(a.body.clone()),
        };
        return Ok(files::markdown_response(page_markdown(&doc, locale)));
    }
    ctx.load_store().await?;
    // Counting views must never fail the page.
    {
        let pool = state.pool.clone();
        let (tid, id) = (tenant.id, a.id);
        tokio::spawn(async move {
            if let Err(e) = content::increment_kb_views(&pool, tid, id).await {
                tracing::debug!(error = %e, "kb view counter");
            }
        });
    }
    let (body_html, toc) = markdown::render_with_toc(&a.body);
    let category = match a.category_id {
        Some(cid) => content::kb_categories(&state.pool, tenant.id, locale)
            .await?
            .into_iter()
            .find(|c| c.category.id == cid),
        None => None,
    };
    let related = match a.category_id {
        Some(cid) => content::published_kb_articles(
            &state.pool,
            tenant.id,
            locale,
            Some(cid),
            None,
            hos_db::Page::number(1, 6),
        )
        .await?
        .items
        .iter()
        .filter(|r| r.id != a.id)
        .take(5)
        .map(|r| article_link(&ctx, r))
        .collect(),
        None => Vec::new(),
    };
    let description = a
        .excerpt
        .clone()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| markdown::plain_text(&a.body));
    let mut page = SeoPage::new(a.title.clone(), path.clone())
        .description(description.clone())
        .target(SeoTarget::Entity(
            SeoEntityType::KbArticle,
            a.id.into_uuid(),
        ))
        .crumb(ctx.t("storefront.kb.title"), "/kb");
    page.og_type = hos_seo::meta::OgType::Article;
    let mut crumbs = vec![
        Crumb::link(ctx.t("common.nav.home"), ctx.url("/")),
        Crumb::link(ctx.t("storefront.kb.title"), ctx.url("/kb")),
    ];
    if let Some(c) = &category {
        let cname = localized(
            &c.category.name,
            &c.category.translations,
            locale,
            default,
            "name",
        );
        let cpath = format!("/kb/category/{}", c.category.slug);
        crumbs.push(Crumb::link(cname.clone(), ctx.url(&cpath)));
        page = page.crumb(cname, cpath);
    }
    crumbs.push(Crumb::current(a.title.clone()));
    page = page.node(&Article {
        kind: ArticleKind::TechArticle,
        id: Some(format!("{url}#article")),
        headline: a.title.clone(),
        description: Some(hos_seo::meta::description_from_text(
            &description,
            hos_seo::meta::DESCRIPTION_MAX,
        )),
        url: Some(url.clone()),
        date_published: a.published_at,
        date_modified: Some(a.updated_at),
        // The support team writes the knowledge base: the organisation is
        // the accountable author (same node as the site-wide Organization).
        images: vec![ctx.theme.share_image_url(&ctx.origin, &path)],
        authors: vec![
            Entity::organization(ctx.site_name.clone())
                .with_id(format!("{}/#organization", ctx.origin)),
        ],
        publisher: Some(Entity::organization(ctx.site_name.clone())),
        in_language: Some(locale.as_str().to_owned()),
        keywords: a.tags.clone(),
        section: category.as_ref().map(|c| c.category.name.clone()),
        word_count: u32::try_from(markdown::plain_text(&a.body).split_whitespace().count()).ok(),
    });
    let steps = markdown::ordered_steps(&a.body);
    if steps.len() >= 2 {
        page = page.node(&HowTo {
            id: Some(format!("{url}#howto")),
            name: a.title.clone(),
            description: Some(hos_seo::meta::description_from_text(
                &description,
                hos_seo::meta::DESCRIPTION_MAX,
            )),
            steps: steps
                .iter()
                .map(|s| HowToStep {
                    name: None,
                    text: s.clone(),
                    url: None,
                    image: None,
                })
                .collect(),
            ..HowTo::default()
        });
    }
    page.alternates = Some(
        content::translations(
            &state.pool,
            tenant.id,
            ContentTable::KbArticles,
            a.translation_group_id,
        )
        .await?
        .into_iter()
        .map(|t| (t.locale, format!("/kb/{}", t.slug)))
        .collect(),
    );
    seo::apply(&mut ctx, page).await?;
    render(&ArticlePage {
        md_href: ctx.url(format!("{path}.md")),
        ctx,
        minutes: markdown::reading_minutes(&a.body),
        is_faq: a.kind == KbArticleKind::Faq,
        title: a.title,
        slug: a.slug,
        body_html,
        toc,
        updated_at: a.updated_at,
        related,
        crumbs,
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct VoteForm {
    #[serde(default)]
    helpful: String,
}

/// `POST /kb/{slug}/vote` (htmx fragment, or redirect back without JS).
pub(super) async fn vote(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(slug): Path<String>,
    ip: ClientIp,
    htmx: Htmx,
    ctx: PageCtx,
    axum::Form(form): axum::Form<VoteForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.vote, &ip.key(&format!("kb-vote:{slug}")))?;
    let a = content::published_kb_article(&state.pool, tenant.id, ctx.locale, &slug)
        .await?
        .ok_or(AppError::NotFound)?;
    content::vote_kb_article(&state.pool, tenant.id, a.id, form.helpful == "yes").await?;
    let thanks = ctx.t("storefront.kb.thanks");
    if htmx.request {
        return Ok(axum::response::Html(format!(
            "<p class=\"helpful\" role=\"status\">{}</p>",
            crate::view::escape_html(&thanks)
        ))
        .into_response());
    }
    Ok(flash::redirect(
        &ctx.url(format!("/kb/{slug}")),
        SetFlash::success(thanks),
    ))
}
