//! Blog (`/blog`, `/blog/{slug}` + `.md`, Atom / RSS feeds) and
//! announcements (`/announcements`, `/announcements/{slug}`).

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use hos_db::Page;
use hos_db::enums::SeoEntityType;
use hos_db::repo::content::{self, BlogPostRow, ContentTable};
use hos_seo::escape::xml as xml_escape;
use hos_seo::jsonld::{Article, ArticleKind, Entity};
use hos_seo::llms::{Body, Document, page_markdown};
use serde::Deserialize;

use super::files;
use crate::error::{AppError, AppResult};
use crate::extract::PageQuery;
use crate::markdown::{self, Heading};
use crate::seo::{self, SeoPage, SeoTarget};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, Pager, render};

/// A post / announcement teaser.
#[derive(Debug, Clone)]
pub struct Teaser {
    /// Title.
    pub title: String,
    /// Excerpt.
    pub excerpt: String,
    /// Localized URL.
    pub href: String,
    /// Publication date.
    pub published_at: DateTime<Utc>,
    /// Reading minutes.
    pub minutes: i32,
    /// Category label.
    pub category: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Pinned (announcements).
    pub pinned: bool,
}

fn post_teaser(ctx: &PageCtx, p: &BlogPostRow) -> Teaser {
    Teaser {
        title: p.title.clone(),
        excerpt: p.excerpt.clone().unwrap_or_else(|| {
            hos_seo::meta::description_from_text(&markdown::plain_text(&p.body), 180)
        }),
        href: ctx.url(format!("/blog/{}", p.slug)),
        published_at: p.published_at.unwrap_or(p.created_at),
        minutes: p
            .reading_minutes
            .unwrap_or_else(|| i32::try_from(markdown::reading_minutes(&p.body)).unwrap_or(1)),
        category: p.category.clone().unwrap_or_default(),
        tags: p.tags.clone(),
        pinned: false,
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TagQuery {
    #[serde(default)]
    tag: Option<String>,
}

#[derive(Template)]
#[template(path = "storefront/blog_index.html")]
struct IndexPage {
    ctx: PageCtx,
    heading: String,
    lead: String,
    posts: Vec<Teaser>,
    pager: Pager,
    tag: String,
    feed_href: String,
    kind: &'static str,
}
crate::page!(IndexPage);

/// `GET /blog?tag=&page=`.
pub(super) async fn index(
    State(state): State<AppState>,
    tenant: Tenant,
    Query(tq): Query<TagQuery>,
    query: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let tag = tq
        .tag
        .map(|t| crate::forms::clean(&t, 60))
        .filter(|t| !t.is_empty());
    let paged = content::published_blog_posts(
        &state.pool,
        tenant.id,
        ctx.locale,
        tag.as_deref(),
        Page::number(query.page, 12),
    )
    .await?;
    let base = ctx.url("/blog");
    let pager = Pager::new(&paged, &base, &[("tag", tag.as_deref().unwrap_or(""))]);
    let posts = paged.items.iter().map(|p| post_teaser(&ctx, p)).collect();
    let heading = match &tag {
        Some(t) => ctx.tv("storefront.blog.tagged", "tag", t),
        None => ctx.t("storefront.blog.title"),
    };
    let mut page =
        SeoPage::new(heading.clone(), "/blog").description(ctx.t("storefront.blog.lead"));
    // Tag / page>1 listings are thin duplicates: keep them out of the index.
    page.noindex = tag.is_some() || query.page > 1;
    seo::apply(&mut ctx, page).await?;
    render(&IndexPage {
        feed_href: ctx.url("/blog/feed.xml"),
        lead: ctx.t("storefront.blog.lead"),
        ctx,
        heading,
        posts,
        pager,
        tag: tag.unwrap_or_default(),
        kind: "blog",
    })
}

#[derive(Template)]
#[template(path = "storefront/blog_post.html")]
struct PostPage {
    ctx: PageCtx,
    title: String,
    lead: String,
    body_html: String,
    toc: Vec<Heading>,
    published_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    minutes: i32,
    author: String,
    tags: Vec<String>,
    cover: String,
    crumbs: Vec<Crumb>,
    md_href: String,
    back_href: String,
    back_label: String,
}
crate::page!(PostPage);

/// `GET /blog/{slug}` (and `.md`).
pub(super) async fn post(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(slug): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let locale = ctx.locale;
    let (slug, as_md) = match slug.strip_suffix(".md") {
        Some(s) => (s.to_owned(), true),
        None => (slug, false),
    };
    let p = content::published_blog_post(&state.pool, tenant.id, locale, &slug)
        .await?
        .ok_or(AppError::NotFound)?;
    let path = format!("/blog/{}", p.slug);
    let url = format!("{}{}", ctx.origin, ctx.url(&path));
    if as_md {
        let doc = Document {
            title: p.title.clone(),
            url: Some(url),
            updated: Some(p.updated_at),
            body: Body::Markdown(p.body.clone()),
        };
        return Ok(files::markdown_response(page_markdown(&doc, locale)));
    }
    ctx.load_store().await?;
    let teaser = post_teaser(&ctx, &p);
    let (body_html, toc) = markdown::render_with_toc(&p.body);
    let author = p
        .author_name
        .clone()
        .unwrap_or_else(|| ctx.site_name.clone());
    let mut page = SeoPage::new(p.title.clone(), path.clone())
        .description(teaser.excerpt.clone())
        .target(SeoTarget::Entity(SeoEntityType::BlogPost, p.id.into_uuid()))
        .crumb(ctx.t("storefront.blog.title"), "/blog");
    page.og_type = hos_seo::meta::OgType::Article;
    page.image = p.cover_image_url.clone();
    page.article = Some(hos_seo::meta::ArticleMeta {
        published_time: Some(teaser.published_at),
        modified_time: Some(p.updated_at),
        section: p.category.clone(),
        tags: p.tags.clone(),
        ..hos_seo::meta::ArticleMeta::default()
    });
    page = page.node(&Article {
        kind: ArticleKind::BlogPosting,
        id: Some(format!("{url}#article")),
        headline: p.title.clone(),
        description: Some(teaser.excerpt.clone()),
        url: Some(url.clone()),
        images: vec![p.cover_image_url.clone().unwrap_or_else(|| {
            ctx.theme
                .share_image_url(&ctx.origin, &format!("/blog/{}", p.slug))
        })],
        date_published: Some(teaser.published_at),
        date_modified: Some(p.updated_at),
        authors: vec![Entity::organization(author.clone())],
        publisher: Some(Entity::organization(ctx.site_name.clone())),
        in_language: Some(locale.as_str().to_owned()),
        keywords: p.tags.clone(),
        section: p.category.clone(),
        word_count: u32::try_from(markdown::plain_text(&p.body).split_whitespace().count()).ok(),
    });
    page.alternates = Some(
        content::translations(
            &state.pool,
            tenant.id,
            ContentTable::BlogPosts,
            p.translation_group_id,
        )
        .await?
        .into_iter()
        .map(|t| (t.locale, format!("/blog/{}", t.slug)))
        .collect(),
    );
    seo::apply(&mut ctx, page).await?;
    let crumbs = vec![
        Crumb::link(ctx.t("common.nav.home"), ctx.url("/")),
        Crumb::link(ctx.t("storefront.blog.title"), ctx.url("/blog")),
        Crumb::current(p.title.clone()),
    ];
    render(&PostPage {
        md_href: ctx.url(format!("{path}.md")),
        back_href: ctx.url("/blog"),
        back_label: ctx.t("storefront.blog.back"),
        ctx,
        title: p.title,
        lead: teaser.excerpt,
        body_html,
        toc,
        published_at: teaser.published_at,
        updated_at: p.updated_at,
        minutes: teaser.minutes,
        author,
        tags: p.tags,
        cover: p.cover_image_url.unwrap_or_default(),
        crumbs,
    })
}

// ---------------------------------------------------------------------------
// Feeds
// ---------------------------------------------------------------------------

async fn feed_posts(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
) -> AppResult<Vec<BlogPostRow>> {
    Ok(content::published_blog_posts(
        &state.pool,
        tenant.id,
        ctx.locale,
        None,
        Page::number(1, 20),
    )
    .await?
    .items)
}

fn feed_response(body: String, content_type: &'static str) -> Response {
    let mut res = Response::new(axum::body::Body::from(body));
    res.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(content_type),
    );
    res.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("public, max-age=900"),
    );
    res
}

/// `GET /blog/feed.xml` (Atom 1.0).
pub(super) async fn atom(
    State(state): State<AppState>,
    tenant: Tenant,
    ctx: PageCtx,
) -> AppResult<Response> {
    let posts = feed_posts(&state, &tenant, &ctx).await?;
    let abs = |p: &str| format!("{}{}", ctx.origin, ctx.url(p));
    let updated = posts
        .iter()
        .map(|p| p.updated_at)
        .max()
        .unwrap_or_else(Utc::now);
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:lang=\"{}\">\n<title>{} — {}</title>\n<id>{}</id>\n<link rel=\"self\" href=\"{}\"/>\n<link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n<updated>{}</updated>\n",
        ctx.lang,
        xml_escape(&ctx.site_name),
        xml_escape(&ctx.t("storefront.blog.title")),
        xml_escape(&abs("/blog")),
        xml_escape(&abs("/blog/feed.xml")),
        xml_escape(&abs("/blog")),
        updated.to_rfc3339(),
    ));
    for p in &posts {
        let url = abs(&format!("/blog/{}", p.slug));
        let summary = p.excerpt.clone().unwrap_or_else(|| {
            hos_seo::meta::description_from_text(&markdown::plain_text(&p.body), 280)
        });
        out.push_str(&format!(
            "<entry>\n<title>{}</title>\n<id>{}</id>\n<link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n<published>{}</published>\n<updated>{}</updated>\n<author><name>{}</name></author>\n<summary>{}</summary>\n<content type=\"html\">{}</content>\n</entry>\n",
            xml_escape(&p.title),
            xml_escape(&url),
            xml_escape(&url),
            p.published_at.unwrap_or(p.created_at).to_rfc3339(),
            p.updated_at.to_rfc3339(),
            xml_escape(p.author_name.as_deref().unwrap_or(&ctx.site_name)),
            xml_escape(&summary),
            xml_escape(&markdown::render(&p.body)),
        ));
    }
    out.push_str("</feed>\n");
    Ok(feed_response(out, "application/atom+xml; charset=utf-8"))
}

/// `GET /blog/rss.xml` (RSS 2.0).
pub(super) async fn rss(
    State(state): State<AppState>,
    tenant: Tenant,
    ctx: PageCtx,
) -> AppResult<Response> {
    let posts = feed_posts(&state, &tenant, &ctx).await?;
    let abs = |p: &str| format!("{}{}", ctx.origin, ctx.url(p));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\">\n<channel>\n",
    );
    out.push_str(&format!(
        "<title>{} — {}</title>\n<link>{}</link>\n<description>{}</description>\n<language>{}</language>\n<atom:link href=\"{}\" rel=\"self\" type=\"application/rss+xml\"/>\n",
        xml_escape(&ctx.site_name),
        xml_escape(&ctx.t("storefront.blog.title")),
        xml_escape(&abs("/blog")),
        xml_escape(&ctx.t("storefront.blog.lead")),
        ctx.lang,
        xml_escape(&abs("/blog/rss.xml")),
    ));
    for p in &posts {
        let url = abs(&format!("/blog/{}", p.slug));
        let summary = p.excerpt.clone().unwrap_or_else(|| {
            hos_seo::meta::description_from_text(&markdown::plain_text(&p.body), 280)
        });
        out.push_str(&format!(
            "<item>\n<title>{}</title>\n<link>{}</link>\n<guid isPermaLink=\"true\">{}</guid>\n<pubDate>{}</pubDate>\n<description>{}</description>\n</item>\n",
            xml_escape(&p.title),
            xml_escape(&url),
            xml_escape(&url),
            p.published_at.unwrap_or(p.created_at).to_rfc2822(),
            xml_escape(&summary),
        ));
    }
    out.push_str("</channel>\n</rss>\n");
    Ok(feed_response(out, "application/rss+xml; charset=utf-8"))
}

// ---------------------------------------------------------------------------
// Announcements
// ---------------------------------------------------------------------------

/// `GET /announcements`.
pub(super) async fn announcements(
    State(state): State<AppState>,
    tenant: Tenant,
    query: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let paged = content::published_announcements(
        &state.pool,
        tenant.id,
        ctx.locale,
        Page::number(query.page, 12),
    )
    .await?;
    let pager = Pager::new(&paged, &ctx.url("/announcements"), &[]);
    let posts = paged
        .items
        .iter()
        .map(|a| Teaser {
            title: a.title.clone(),
            excerpt: a.excerpt.clone().unwrap_or_else(|| {
                hos_seo::meta::description_from_text(&markdown::plain_text(&a.body), 180)
            }),
            href: ctx.url(format!("/announcements/{}", a.slug)),
            published_at: a.published_at.unwrap_or(a.created_at),
            minutes: i32::try_from(markdown::reading_minutes(&a.body)).unwrap_or(1),
            category: String::new(),
            tags: Vec::new(),
            pinned: a.is_pinned,
        })
        .collect();
    let heading = ctx.t("storefront.announcements.title");
    let mut page = SeoPage::new(heading.clone(), "/announcements")
        .description(ctx.t("storefront.announcements.lead"));
    page.noindex = query.page > 1;
    seo::apply(&mut ctx, page).await?;
    render(&IndexPage {
        feed_href: String::new(),
        lead: ctx.t("storefront.announcements.lead"),
        ctx,
        heading,
        posts,
        pager,
        tag: String::new(),
        kind: "announcements",
    })
}

/// `GET /announcements/{slug}`.
pub(super) async fn announcement(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(slug): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let a = content::published_announcement(&state.pool, tenant.id, ctx.locale, &slug)
        .await?
        .ok_or(AppError::NotFound)?;
    ctx.load_store().await?;
    let path = format!("/announcements/{}", a.slug);
    let url = format!("{}{}", ctx.origin, ctx.url(&path));
    let (body_html, toc) = markdown::render_with_toc(&a.body);
    let published = a.published_at.unwrap_or(a.created_at);
    let lead = a.excerpt.clone().unwrap_or_else(|| {
        hos_seo::meta::description_from_text(&markdown::plain_text(&a.body), 180)
    });
    let mut page = SeoPage::new(a.title.clone(), path.clone())
        .description(lead.clone())
        .crumb(ctx.t("storefront.announcements.title"), "/announcements")
        .node(&Article {
            kind: ArticleKind::NewsArticle,
            id: Some(format!("{url}#article")),
            headline: a.title.clone(),
            description: Some(lead.clone()),
            url: Some(url),
            date_published: Some(published),
            date_modified: Some(a.updated_at),
            authors: vec![Entity::organization(ctx.site_name.clone())],
            publisher: Some(Entity::organization(ctx.site_name.clone())),
            in_language: Some(ctx.locale.as_str().to_owned()),
            ..Article::default()
        });
    page.og_type = hos_seo::meta::OgType::Article;
    page.alternates = Some(
        content::translations(
            &state.pool,
            tenant.id,
            ContentTable::Announcements,
            a.translation_group_id,
        )
        .await?
        .into_iter()
        .map(|t| (t.locale, format!("/announcements/{}", t.slug)))
        .collect(),
    );
    seo::apply(&mut ctx, page).await?;
    let crumbs = vec![
        Crumb::link(ctx.t("common.nav.home"), ctx.url("/")),
        Crumb::link(
            ctx.t("storefront.announcements.title"),
            ctx.url("/announcements"),
        ),
        Crumb::current(a.title.clone()),
    ];
    render(&PostPage {
        md_href: String::new(),
        back_href: ctx.url("/announcements"),
        back_label: ctx.t("storefront.announcements.back"),
        ctx,
        title: a.title,
        lead,
        body_html,
        toc,
        published_at: published,
        updated_at: a.updated_at,
        minutes: i32::try_from(markdown::reading_minutes(&a.body)).unwrap_or(1),
        author: String::new(),
        tags: Vec::new(),
        cover: String::new(),
        crumbs,
    })
}
