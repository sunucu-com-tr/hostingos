//! Content: knowledge base (articles + FAQ), announcements, CMS pages, blog
//! posts and redirects. Public queries only return *published* rows:
//! `status = 'published'` (and `published_at` not in the future) or
//! `status = 'scheduled'` whose `published_at` has passed.

use chrono::{DateTime, Utc};
use hos_core::domain::Locale;
use hos_core::ids::{AnnouncementId, BlogPostId, KbArticleId, PageId, TenantId, UserId};
use sqlx::{Acquire, FromRow, PgExecutor, Postgres};
use uuid::Uuid;

use super::Json;
use crate::enums::{ContentStatus, KbArticleKind};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::ids::{KbCategoryId, RedirectId};
use crate::page::{Page, Paged, fetch_paged};

/// SQL predicate: row is publicly visible now.
const PUBLISHED: &str =
    "((status = 'published' and (published_at is null or published_at <= now()))
                          or (status = 'scheduled' and published_at <= now()))";

// ---------------------------------------------------------------------------
// knowledge base
// ---------------------------------------------------------------------------

/// `kb_categories` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct KbCategoryRow {
    pub id: KbCategoryId,
    pub tenant_id: TenantId,
    pub parent_id: Option<KbCategoryId>,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub translations: Json,
    pub icon: Option<String>,
    pub is_hidden: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `kb_articles` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct KbArticleRow {
    pub id: KbArticleId,
    pub tenant_id: TenantId,
    pub category_id: Option<KbCategoryId>,
    pub kind: KbArticleKind,
    pub slug: String,
    pub locale: Locale,
    pub translation_group_id: Uuid,
    pub title: String,
    pub excerpt: Option<String>,
    pub body: String,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub is_featured: bool,
    pub sort_order: i32,
    pub views: i64,
    pub helpful_yes: i32,
    pub helpful_no: i32,
    pub author_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a KB category.
#[allow(clippy::too_many_arguments)]
pub async fn create_kb_category(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    parent_id: Option<KbCategoryId>,
    slug: &str,
    name: &str,
    description: Option<&str>,
    translations: &Json,
    sort_order: i32,
) -> DbResult<KbCategoryRow> {
    sqlx::query_as::<_, KbCategoryRow>(
        "insert into kb_categories (id, tenant_id, parent_id, slug, name, description, translations, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning *",
    )
    .bind(KbCategoryId::new())
    .bind(tenant_id)
    .bind(parent_id)
    .bind(slug)
    .bind(name)
    .bind(description)
    .bind(translations)
    .bind(sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("kb category"))
}

/// KB category with the number of published articles in a locale.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct KbCategoryWithCount {
    #[sqlx(flatten)]
    pub category: KbCategoryRow,
    pub article_count: i64,
}

/// Visible categories with published article counts for `locale`.
pub async fn kb_categories(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
) -> DbResult<Vec<KbCategoryWithCount>> {
    let sql = format!(
        "select c.*, (select count(*) from kb_articles a
                      where a.category_id = c.id and a.locale = $2 and {PUBLISHED}) as article_count
         from kb_categories c where c.tenant_id = $1 and not c.is_hidden
         order by c.sort_order, c.name"
    );
    Ok(sqlx::query_as::<_, KbCategoryWithCount>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .fetch_all(db)
        .await?)
}

/// KB category by slug.
pub async fn kb_category_by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
) -> DbResult<Option<KbCategoryRow>> {
    Ok(sqlx::query_as::<_, KbCategoryRow>(
        "select * from kb_categories where tenant_id = $1 and slug = $2",
    )
    .bind(tenant_id)
    .bind(slug)
    .fetch_optional(db)
    .await?)
}

/// Input of [`create_kb_article`].
#[derive(Debug, Clone)]
pub struct KbArticleInput {
    pub category_id: Option<KbCategoryId>,
    pub kind: KbArticleKind,
    pub slug: String,
    pub locale: Locale,
    /// Shared by translations of the same article (`None` = new group).
    pub translation_group_id: Option<Uuid>,
    pub title: String,
    pub excerpt: Option<String>,
    pub body: String,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub is_featured: bool,
    pub sort_order: i32,
    pub author_id: Option<UserId>,
}

/// Creates a KB article / FAQ entry.
pub async fn create_kb_article(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    a: &KbArticleInput,
) -> DbResult<KbArticleRow> {
    sqlx::query_as::<_, KbArticleRow>(
        "insert into kb_articles (id, tenant_id, category_id, kind, slug, locale, translation_group_id, title,
                                  excerpt, body, status, published_at, tags, is_featured, sort_order, author_id)
         values ($1, $2, $3, $4, $5, $6, coalesce($7, hf_uuid_v7()), $8, $9, $10, $11, $12, $13, $14, $15, $16)
         returning *",
    )
    .bind(KbArticleId::new())
    .bind(tenant_id)
    .bind(a.category_id)
    .bind(a.kind)
    .bind(&a.slug)
    .bind(a.locale)
    .bind(a.translation_group_id)
    .bind(&a.title)
    .bind(&a.excerpt)
    .bind(&a.body)
    .bind(a.status)
    .bind(a.published_at)
    .bind(&a.tags)
    .bind(a.is_featured)
    .bind(a.sort_order)
    .bind(a.author_id)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("kb article slug"))
}

/// A published article by locale + slug.
pub async fn published_kb_article(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
    slug: &str,
) -> DbResult<Option<KbArticleRow>> {
    let sql = format!(
        "select * from kb_articles where tenant_id = $1 and locale = $2 and slug = $3 and {PUBLISHED}"
    );
    Ok(sqlx::query_as::<_, KbArticleRow>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .bind(slug)
        .fetch_optional(db)
        .await?)
}

/// Published articles of a locale (optionally one category / kind), in order.
pub async fn published_kb_articles<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    locale: Locale,
    category_id: Option<KbCategoryId>,
    kind: Option<KbArticleKind>,
    page: Page,
) -> DbResult<Paged<KbArticleRow>> {
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from kb_articles where tenant_id = ")
                .push_bind(tenant_id)
                .push(" and locale = ")
                .push_bind(locale)
                .push(" and ")
                .push(PUBLISHED);
            if let Some(c) = category_id {
                qb.push(" and category_id = ").push_bind(c);
            }
            if let Some(k) = kind {
                qb.push(" and kind = ").push_bind(k);
            }
        },
        "is_featured desc, sort_order, published_at desc nulls last, id",
        page,
    )
    .await
}

/// Published FAQ entries of a locale (FAQPage JSON-LD).
pub async fn faq_entries(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
    limit: i64,
) -> DbResult<Vec<KbArticleRow>> {
    let sql = format!(
        "select * from kb_articles where tenant_id = $1 and locale = $2 and kind = 'faq' and {PUBLISHED}
         order by sort_order, title limit $3"
    );
    Ok(sqlx::query_as::<_, KbArticleRow>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .bind(limit)
        .fetch_all(db)
        .await?)
}

/// Simple KB search (title / excerpt / body / tags, case-insensitive).
pub async fn search_kb(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
    query: &str,
    limit: i64,
) -> DbResult<Vec<KbArticleRow>> {
    let sql = format!(
        "select * from kb_articles
         where tenant_id = $1 and locale = $2 and {PUBLISHED}
           and (title ilike $3 or excerpt ilike $3 or body ilike $3 or array_to_string(tags, ' ') ilike $3)
         order by (title ilike $3) desc, views desc, id limit $4"
    );
    Ok(sqlx::query_as::<_, KbArticleRow>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .bind(like_contains(query))
        .bind(limit)
        .fetch_all(db)
        .await?)
}

/// Counts a view.
pub async fn increment_kb_views(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: KbArticleId,
) -> DbResult<()> {
    sqlx::query("update kb_articles set views = views + 1 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Records a "was this helpful?" vote.
pub async fn vote_kb_article(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: KbArticleId,
    helpful: bool,
) -> DbResult<()> {
    sqlx::query(
        "update kb_articles set helpful_yes = helpful_yes + case when $3 then 1 else 0 end,
                helpful_no = helpful_no + case when $3 then 0 else 1 end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(helpful)
    .execute(db)
    .await?;
    Ok(())
}

/// A published translation of a piece of content (hreflang alternates).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Translation {
    pub locale: Locale,
    pub slug: String,
}

/// Published translations in a translation group of `table`
/// (`kb_articles`, `announcements`, `pages`, `blog_posts`).
pub async fn translations(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    table: ContentTable,
    translation_group_id: Uuid,
) -> DbResult<Vec<Translation>> {
    let sql = format!(
        "select locale, slug from {} where tenant_id = $1 and translation_group_id = $2 and {PUBLISHED} order by locale",
        table.as_sql()
    );
    Ok(sqlx::query_as::<_, Translation>(&sql)
        .bind(tenant_id)
        .bind(translation_group_id)
        .fetch_all(db)
        .await?)
}

/// Tables with localized, slugged, publishable content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentTable {
    KbArticles,
    Announcements,
    Pages,
    BlogPosts,
}

impl ContentTable {
    /// Table name.
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::KbArticles => "kb_articles",
            Self::Announcements => "announcements",
            Self::Pages => "pages",
            Self::BlogPosts => "blog_posts",
        }
    }
}

// ---------------------------------------------------------------------------
// announcements
// ---------------------------------------------------------------------------

/// `announcements` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct AnnouncementRow {
    pub id: AnnouncementId,
    pub tenant_id: TenantId,
    pub slug: String,
    pub locale: Locale,
    pub translation_group_id: Uuid,
    pub title: String,
    pub excerpt: Option<String>,
    pub body: String,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub is_pinned: bool,
    pub author_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates an announcement.
#[allow(clippy::too_many_arguments)]
pub async fn create_announcement(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
    locale: Locale,
    translation_group_id: Option<Uuid>,
    title: &str,
    excerpt: Option<&str>,
    body: &str,
    status: ContentStatus,
    published_at: Option<DateTime<Utc>>,
) -> DbResult<AnnouncementRow> {
    sqlx::query_as::<_, AnnouncementRow>(
        "insert into announcements (id, tenant_id, slug, locale, translation_group_id, title, excerpt, body, status,
                                    published_at)
         values ($1, $2, $3, $4, coalesce($5, hf_uuid_v7()), $6, $7, $8, $9, $10) returning *",
    )
    .bind(AnnouncementId::new())
    .bind(tenant_id)
    .bind(slug)
    .bind(locale)
    .bind(translation_group_id)
    .bind(title)
    .bind(excerpt)
    .bind(body)
    .bind(status)
    .bind(published_at)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("announcement slug"))
}

/// Published announcements (pinned first, newest first).
pub async fn published_announcements<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    locale: Locale,
    page: Page,
) -> DbResult<Paged<AnnouncementRow>> {
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from announcements where tenant_id = ")
                .push_bind(tenant_id)
                .push(" and locale = ")
                .push_bind(locale)
                .push(" and ")
                .push(PUBLISHED);
        },
        "is_pinned desc, coalesce(published_at, created_at) desc, id desc",
        page,
    )
    .await
}

/// A published announcement by locale + slug.
pub async fn published_announcement(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
    slug: &str,
) -> DbResult<Option<AnnouncementRow>> {
    let sql = format!(
        "select * from announcements where tenant_id = $1 and locale = $2 and slug = $3 and {PUBLISHED}"
    );
    Ok(sqlx::query_as::<_, AnnouncementRow>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .bind(slug)
        .fetch_optional(db)
        .await?)
}

// ---------------------------------------------------------------------------
// pages
// ---------------------------------------------------------------------------

/// `pages` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PageRow {
    pub id: PageId,
    pub tenant_id: TenantId,
    pub parent_id: Option<PageId>,
    pub slug: String,
    pub locale: Locale,
    pub translation_group_id: Uuid,
    pub title: String,
    pub body: String,
    pub template: String,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub show_in_nav: bool,
    pub show_in_footer: bool,
    pub nav_order: i32,
    pub author_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_page`] / [`update_page`].
#[derive(Debug, Clone)]
pub struct PageInput {
    pub slug: String,
    pub locale: Locale,
    pub translation_group_id: Option<Uuid>,
    pub title: String,
    pub body: String,
    pub template: String,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub show_in_nav: bool,
    pub show_in_footer: bool,
    pub nav_order: i32,
    pub author_id: Option<UserId>,
}

/// Creates a CMS page.
pub async fn create_page(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    p: &PageInput,
) -> DbResult<PageRow> {
    sqlx::query_as::<_, PageRow>(
        "insert into pages (id, tenant_id, slug, locale, translation_group_id, title, body, template, status,
                            published_at, show_in_nav, show_in_footer, nav_order, author_id)
         values ($1, $2, $3, $4, coalesce($5, hf_uuid_v7()), $6, $7, $8, $9, $10, $11, $12, $13, $14) returning *",
    )
    .bind(PageId::new())
    .bind(tenant_id)
    .bind(&p.slug)
    .bind(p.locale)
    .bind(p.translation_group_id)
    .bind(&p.title)
    .bind(&p.body)
    .bind(&p.template)
    .bind(p.status)
    .bind(p.published_at)
    .bind(p.show_in_nav)
    .bind(p.show_in_footer)
    .bind(p.nav_order)
    .bind(p.author_id)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("page slug"))
}

/// Updates a CMS page (translation group unchanged).
pub async fn update_page(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: PageId,
    p: &PageInput,
) -> DbResult<Option<PageRow>> {
    sqlx::query_as::<_, PageRow>(
        "update pages set slug = $3, locale = $4, title = $5, body = $6, template = $7, status = $8,
                published_at = $9, show_in_nav = $10, show_in_footer = $11, nav_order = $12
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&p.slug)
    .bind(p.locale)
    .bind(&p.title)
    .bind(&p.body)
    .bind(&p.template)
    .bind(p.status)
    .bind(p.published_at)
    .bind(p.show_in_nav)
    .bind(p.show_in_footer)
    .bind(p.nav_order)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("page slug"))
}

/// A published page by locale + slug.
pub async fn published_page(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
    slug: &str,
) -> DbResult<Option<PageRow>> {
    let sql = format!(
        "select * from pages where tenant_id = $1 and locale = $2 and slug = $3 and {PUBLISHED}"
    );
    Ok(sqlx::query_as::<_, PageRow>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .bind(slug)
        .fetch_optional(db)
        .await?)
}

/// Published pages shown in the navigation / footer.
pub async fn nav_pages(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
) -> DbResult<Vec<PageRow>> {
    let sql = format!(
        "select * from pages where tenant_id = $1 and locale = $2 and (show_in_nav or show_in_footer) and {PUBLISHED}
         order by nav_order, title"
    );
    Ok(sqlx::query_as::<_, PageRow>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .fetch_all(db)
        .await?)
}

/// All pages (admin), by locale and slug.
pub async fn list_pages(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<Vec<PageRow>> {
    Ok(sqlx::query_as::<_, PageRow>(
        "select * from pages where tenant_id = $1 order by locale, slug",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// blog
// ---------------------------------------------------------------------------

/// `blog_posts` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct BlogPostRow {
    pub id: BlogPostId,
    pub tenant_id: TenantId,
    pub slug: String,
    pub locale: Locale,
    pub translation_group_id: Uuid,
    pub title: String,
    pub excerpt: Option<String>,
    pub body: String,
    pub cover_image_url: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub reading_minutes: Option<i32>,
    pub author_id: Option<UserId>,
    pub author_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_blog_post`] / [`update_blog_post`].
#[derive(Debug, Clone)]
pub struct BlogPostInput {
    pub slug: String,
    pub locale: Locale,
    pub translation_group_id: Option<Uuid>,
    pub title: String,
    pub excerpt: Option<String>,
    pub body: String,
    pub cover_image_url: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub reading_minutes: Option<i32>,
    pub author_id: Option<UserId>,
    pub author_name: Option<String>,
}

/// Creates a blog post.
pub async fn create_blog_post(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    p: &BlogPostInput,
) -> DbResult<BlogPostRow> {
    sqlx::query_as::<_, BlogPostRow>(
        "insert into blog_posts (id, tenant_id, slug, locale, translation_group_id, title, excerpt, body,
                                 cover_image_url, category, tags, status, published_at, reading_minutes, author_id,
                                 author_name)
         values ($1, $2, $3, $4, coalesce($5, hf_uuid_v7()), $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
         returning *",
    )
    .bind(BlogPostId::new())
    .bind(tenant_id)
    .bind(&p.slug)
    .bind(p.locale)
    .bind(p.translation_group_id)
    .bind(&p.title)
    .bind(&p.excerpt)
    .bind(&p.body)
    .bind(&p.cover_image_url)
    .bind(&p.category)
    .bind(&p.tags)
    .bind(p.status)
    .bind(p.published_at)
    .bind(p.reading_minutes)
    .bind(p.author_id)
    .bind(&p.author_name)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("blog post slug"))
}

/// Updates a blog post.
pub async fn update_blog_post(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: BlogPostId,
    p: &BlogPostInput,
) -> DbResult<Option<BlogPostRow>> {
    sqlx::query_as::<_, BlogPostRow>(
        "update blog_posts set slug = $3, locale = $4, title = $5, excerpt = $6, body = $7, cover_image_url = $8,
                category = $9, tags = $10, status = $11, published_at = $12, reading_minutes = $13,
                author_name = $14
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&p.slug)
    .bind(p.locale)
    .bind(&p.title)
    .bind(&p.excerpt)
    .bind(&p.body)
    .bind(&p.cover_image_url)
    .bind(&p.category)
    .bind(&p.tags)
    .bind(p.status)
    .bind(p.published_at)
    .bind(p.reading_minutes)
    .bind(&p.author_name)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("blog post slug"))
}

/// Published posts of a locale (optionally with a tag), newest first.
pub async fn published_blog_posts<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    locale: Locale,
    tag: Option<&str>,
    page: Page,
) -> DbResult<Paged<BlogPostRow>> {
    let tag = tag.map(str::to_owned);
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from blog_posts where tenant_id = ")
                .push_bind(tenant_id)
                .push(" and locale = ")
                .push_bind(locale)
                .push(" and ")
                .push(PUBLISHED);
            if let Some(t) = &tag {
                qb.push(" and ").push_bind(t.clone()).push(" = any(tags)");
            }
        },
        "coalesce(published_at, created_at) desc, id desc",
        page,
    )
    .await
}

/// A published post by locale + slug.
pub async fn published_blog_post(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Locale,
    slug: &str,
) -> DbResult<Option<BlogPostRow>> {
    let sql = format!(
        "select * from blog_posts where tenant_id = $1 and locale = $2 and slug = $3 and {PUBLISHED}"
    );
    Ok(sqlx::query_as::<_, BlogPostRow>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .bind(slug)
        .fetch_optional(db)
        .await?)
}

/// All posts (admin), newest first.
pub async fn list_blog_posts<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    status: Option<ContentStatus>,
    page: Page,
) -> DbResult<Paged<BlogPostRow>> {
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from blog_posts where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(s) = status {
                qb.push(" and status = ").push_bind(s);
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}

/// Flips due `scheduled` rows of every content table to `published` (cron).
pub async fn publish_scheduled(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<u64> {
    let res = sqlx::query(
        "with a as (update kb_articles set status = 'published'
                    where tenant_id = $1 and status = 'scheduled' and published_at <= now() returning 1),
              b as (update announcements set status = 'published'
                    where tenant_id = $1 and status = 'scheduled' and published_at <= now() returning 1),
              c as (update pages set status = 'published'
                    where tenant_id = $1 and status = 'scheduled' and published_at <= now() returning 1),
              d as (update blog_posts set status = 'published'
                    where tenant_id = $1 and status = 'scheduled' and published_at <= now() returning 1)
         select (select count(*) from a) + (select count(*) from b) + (select count(*) from c) + (select count(*) from d)",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await?;
    let n: i64 = sqlx::Row::try_get(&res, 0)?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// One public URL for the sitemap.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SitemapEntry {
    /// `page`, `blog_post`, `kb_article`, `announcement`, `product`, `product_group`.
    pub kind: String,
    /// `None` for language-neutral entries (products use translations jsonb).
    pub locale: Option<Locale>,
    pub slug: String,
    pub translation_group_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

/// Whether `key` is the share-image key (`/og/{key}.png`) of a published
/// storefront page with content behind it: a CMS page, blog post, KB article
/// or category, announcement, visible product group or product, or public
/// status incident. The key is the page path without its outer slashes,
/// every character but `[A-Za-z0-9-]` as `_`, at most 120 characters (the
/// same as `hos_web::theme::share_image_key`). Fixed routes (`/`, `/blog`,
/// ...) are known to the caller.
pub async fn share_key_exists(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
) -> DbResult<bool> {
    let sql = format!(
        "select exists (
             select 1 from (
                 select '/' || slug as path from pages where tenant_id = $1 and {PUBLISHED}
                 union all
                 select '/blog/' || slug from blog_posts where tenant_id = $1 and {PUBLISHED}
                 union all
                 select '/kb/' || slug from kb_articles
                  where tenant_id = $1 and kind = 'article' and {PUBLISHED}
                 union all
                 select '/kb/category/' || slug from kb_categories
                  where tenant_id = $1 and not is_hidden
                 union all
                 select '/announcements/' || slug from announcements
                  where tenant_id = $1 and {PUBLISHED}
                 union all
                 select '/' || slug from product_groups where tenant_id = $1 and not is_hidden
                 union all
                 select '/' || g.slug || '/' || p.slug from products p
                   join product_groups g on g.id = p.group_id
                  where p.tenant_id = $1 and not p.is_hidden and not p.is_retired
                    and not g.is_hidden
                 union all
                 select '/status/incidents/' || id::text from status_incidents
                  where tenant_id = $1 and is_public
             ) p
             where left(regexp_replace(btrim(p.path, '/'), '[^A-Za-z0-9-]', '_', 'g'), 120) = $2)"
    );
    Ok(sqlx::query_scalar(&sql)
        .bind(tenant_id)
        .bind(key)
        .fetch_one(db)
        .await?)
}

/// Every publicly visible, indexable URL of a tenant (sitemap.xml / llms.txt).
/// Rows with `seo_meta.noindex` are excluded.
pub async fn sitemap_entries(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<SitemapEntry>> {
    let sql = format!(
        "select * from (
             select 'page' as kind, locale, slug, translation_group_id, updated_at, id from pages
              where tenant_id = $1 and {PUBLISHED}
             union all
             select 'blog_post', locale, slug, translation_group_id, updated_at, id from blog_posts
              where tenant_id = $1 and {PUBLISHED}
             union all
             select 'kb_article', locale, slug, translation_group_id, updated_at, id from kb_articles
              where tenant_id = $1 and kind = 'article' and {PUBLISHED}
             union all
             select 'announcement', locale, slug, translation_group_id, updated_at, id from announcements
              where tenant_id = $1 and {PUBLISHED}
             union all
             select 'product', null, slug, null, updated_at, id from products
              where tenant_id = $1 and not is_hidden and not is_retired
             union all
             select 'product_group', null, slug, null, updated_at, id from product_groups
              where tenant_id = $1 and not is_hidden
         ) e
         where not exists (select 1 from seo_meta m
                           where m.tenant_id = $1 and m.entity_id = e.id and m.noindex
                             and (e.locale is null or m.locale = e.locale))
         order by kind, slug, locale"
    );
    Ok(sqlx::query_as::<_, SitemapEntry>(&sql)
        .bind(tenant_id)
        .fetch_all(db)
        .await?)
}

// ---------------------------------------------------------------------------
// redirects
// ---------------------------------------------------------------------------

/// `redirects` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct RedirectRow {
    pub id: RedirectId,
    pub tenant_id: TenantId,
    pub from_path: String,
    pub to_url: String,
    pub status_code: i16,
    pub is_active: bool,
    pub hits: i64,
    pub last_hit_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates or replaces a redirect for `from_path`.
pub async fn upsert_redirect(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from_path: &str,
    to_url: &str,
    status_code: i16,
    note: Option<&str>,
) -> DbResult<RedirectRow> {
    sqlx::query_as::<_, RedirectRow>(
        "insert into redirects (id, tenant_id, from_path, to_url, status_code, note)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (tenant_id, from_path) do update
            set to_url = excluded.to_url, status_code = excluded.status_code, note = excluded.note, is_active = true
         returning *",
    )
    .bind(RedirectId::new())
    .bind(tenant_id)
    .bind(from_path)
    .bind(to_url)
    .bind(status_code)
    .bind(note)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("redirect"))
}

/// Looks up an active redirect for a request path and counts the hit.
pub async fn hit_redirect(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    path: &str,
) -> DbResult<Option<RedirectRow>> {
    Ok(sqlx::query_as::<_, RedirectRow>(
        "update redirects set hits = hits + 1, last_hit_at = now()
         where tenant_id = $1 and from_path = $2 and is_active returning *",
    )
    .bind(tenant_id)
    .bind(path)
    .fetch_optional(db)
    .await?)
}

/// Redirects of a tenant.
pub async fn list_redirects(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<RedirectRow>> {
    Ok(sqlx::query_as::<_, RedirectRow>(
        "select * from redirects where tenant_id = $1 order by from_path",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Deletes a redirect.
pub async fn delete_redirect(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RedirectId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from redirects where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Creates a redirect (`Conflict` when `from_path` already has one).
pub async fn create_redirect(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from_path: &str,
    to_url: &str,
    status_code: i16,
    note: Option<&str>,
) -> DbResult<RedirectRow> {
    sqlx::query_as::<_, RedirectRow>(
        "insert into redirects (id, tenant_id, from_path, to_url, status_code, note)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(RedirectId::new())
    .bind(tenant_id)
    .bind(from_path)
    .bind(to_url)
    .bind(status_code)
    .bind(note)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("redirect"))
}

/// Redirect by id.
pub async fn get_redirect(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RedirectId,
) -> DbResult<Option<RedirectRow>> {
    Ok(
        sqlx::query_as::<_, RedirectRow>(
            "select * from redirects where tenant_id = $1 and id = $2",
        )
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(db)
        .await?,
    )
}

/// Updates a redirect.
#[allow(clippy::too_many_arguments)]
pub async fn update_redirect(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RedirectId,
    from_path: &str,
    to_url: &str,
    status_code: i16,
    is_active: bool,
    note: Option<&str>,
) -> DbResult<Option<RedirectRow>> {
    sqlx::query_as::<_, RedirectRow>(
        "update redirects set from_path = $3, to_url = $4, status_code = $5, is_active = $6, note = $7
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(from_path)
    .bind(to_url)
    .bind(status_code)
    .bind(is_active)
    .bind(note)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("redirect"))
}

// ---------------------------------------------------------------------------
// admin: translation groups, editing, statistics
// ---------------------------------------------------------------------------

/// One translation group of a content table (admin lists): the `tr` and
/// `en` versions side by side.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ContentGroupRow {
    pub translation_group_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub tr_id: Option<Uuid>,
    pub tr_title: Option<String>,
    pub tr_slug: Option<String>,
    pub tr_status: Option<ContentStatus>,
    pub tr_published_at: Option<DateTime<Utc>>,
    pub en_id: Option<Uuid>,
    pub en_title: Option<String>,
    pub en_slug: Option<String>,
    pub en_status: Option<ContentStatus>,
    pub en_published_at: Option<DateTime<Utc>>,
}

/// Filters of [`list_content_groups`].
#[derive(Debug, Clone, Default)]
pub struct ContentGroupFilter {
    /// Title / slug in any language.
    pub search: Option<String>,
    /// Either language has this status.
    pub status: Option<ContentStatus>,
    /// `kb_articles` only.
    pub kb_category: Option<KbCategoryId>,
    /// `kb_articles` only.
    pub kb_kind: Option<KbArticleKind>,
    /// Only groups that miss a language.
    pub missing_translation: bool,
}

/// Translation groups of a content table, most recently updated first.
pub async fn list_content_groups<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    table: ContentTable,
    filter: &ContentGroupFilter,
    page: Page,
) -> DbResult<Paged<ContentGroupRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    let cols: String = ["tr", "en"]
        .iter()
        .map(|l| {
            format!(
                "(array_agg(id) filter (where locale = '{l}'))[1] as {l}_id,
                 max(title) filter (where locale = '{l}') as {l}_title,
                 max(slug) filter (where locale = '{l}') as {l}_slug,
                 max(status) filter (where locale = '{l}') as {l}_status,
                 max(published_at) filter (where locale = '{l}') as {l}_published_at"
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let table_sql = table.as_sql();
    fetch_paged(
        db,
        "select g.*",
        |qb| {
            qb.push(format!(
                "from (select translation_group_id, max(updated_at) as updated_at, {cols}
                       from {table_sql} where tenant_id = "
            ))
            .push_bind(tenant_id);
            if table == ContentTable::KbArticles {
                if let Some(c) = f.kb_category {
                    qb.push(" and category_id = ").push_bind(c);
                }
                if let Some(k) = f.kb_kind {
                    qb.push(" and kind = ").push_bind(k);
                }
            }
            qb.push(" group by translation_group_id) g where true");
            if let Some(s) = &search {
                qb.push(" and (g.tr_title ilike ")
                    .push_bind(s.clone())
                    .push(" or g.en_title ilike ")
                    .push_bind(s.clone())
                    .push(" or g.tr_slug ilike ")
                    .push_bind(s.clone())
                    .push(" or g.en_slug ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
            if let Some(st) = f.status {
                qb.push(" and (g.tr_status = ")
                    .push_bind(st)
                    .push(" or g.en_status = ")
                    .push_bind(st)
                    .push(")");
            }
            if f.missing_translation {
                qb.push(" and (g.tr_id is null or g.en_id is null)");
            }
        },
        "g.updated_at desc, g.translation_group_id",
        page,
    )
    .await
}

/// Translation group of a row (`None` when the row is not the tenant's).
pub async fn content_group_of(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    table: ContentTable,
    id: Uuid,
) -> DbResult<Option<Uuid>> {
    let sql = format!(
        "select translation_group_id from {} where tenant_id = $1 and id = $2",
        table.as_sql()
    );
    Ok(sqlx::query_scalar::<_, Uuid>(&sql)
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// Id of the row with `slug` in `locale` (any status), excluding `except`.
pub async fn content_by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    table: ContentTable,
    locale: Locale,
    slug: &str,
    except: Option<Uuid>,
) -> DbResult<Option<Uuid>> {
    let sql = format!(
        "select id from {} where tenant_id = $1 and locale = $2 and slug = $3
           and ($4::uuid is null or id <> $4) limit 1",
        table.as_sql()
    );
    Ok(sqlx::query_scalar::<_, Uuid>(&sql)
        .bind(tenant_id)
        .bind(locale)
        .bind(slug)
        .bind(except)
        .fetch_optional(db)
        .await?)
}

/// Deletes one row (and its `seo_meta`).
pub async fn delete_content(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    table: ContentTable,
    id: Uuid,
) -> DbResult<bool> {
    let sql = format!(
        "with d as (delete from {} where tenant_id = $1 and id = $2 returning id),
              m as (delete from seo_meta where tenant_id = $1 and entity_id in (select id from d))
         select count(*) from d",
        table.as_sql()
    );
    let n: i64 = sqlx::query_scalar(&sql)
        .bind(tenant_id)
        .bind(id)
        .fetch_one(db)
        .await?;
    Ok(n == 1)
}

/// Rows of a translation group.
macro_rules! in_group {
    ($name:ident, $row:ty, $table:literal) => {
        #[doc = concat!("Rows of a translation group of `", $table, "`.")]
        pub async fn $name(
            db: impl PgExecutor<'_>,
            tenant_id: TenantId,
            group: Uuid,
        ) -> DbResult<Vec<$row>> {
            Ok(sqlx::query_as::<_, $row>(concat!(
                "select * from ",
                $table,
                " where tenant_id = $1 and translation_group_id = $2 order by locale"
            ))
            .bind(tenant_id)
            .bind(group)
            .fetch_all(db)
            .await?)
        }
    };
}

in_group!(pages_in_group, PageRow, "pages");
in_group!(blog_posts_in_group, BlogPostRow, "blog_posts");
in_group!(kb_articles_in_group, KbArticleRow, "kb_articles");
in_group!(announcements_in_group, AnnouncementRow, "announcements");

/// Updates a KB article / FAQ entry (translation group unchanged).
pub async fn update_kb_article(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: KbArticleId,
    a: &KbArticleInput,
) -> DbResult<Option<KbArticleRow>> {
    sqlx::query_as::<_, KbArticleRow>(
        "update kb_articles set category_id = $3, kind = $4, slug = $5, locale = $6, title = $7, excerpt = $8,
                body = $9, status = $10, published_at = $11, tags = $12, is_featured = $13, sort_order = $14
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(a.category_id)
    .bind(a.kind)
    .bind(&a.slug)
    .bind(a.locale)
    .bind(&a.title)
    .bind(&a.excerpt)
    .bind(&a.body)
    .bind(a.status)
    .bind(a.published_at)
    .bind(&a.tags)
    .bind(a.is_featured)
    .bind(a.sort_order)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("kb article slug"))
}

/// Every field of an announcement (admin create / edit).
#[derive(Debug, Clone)]
pub struct AnnouncementInput {
    pub slug: String,
    pub locale: Locale,
    /// Shared by translations (`None` = new group).
    pub translation_group_id: Option<Uuid>,
    pub title: String,
    pub excerpt: Option<String>,
    pub body: String,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub is_pinned: bool,
    pub author_id: Option<UserId>,
}

/// Creates an announcement with every field.
pub async fn create_announcement_full(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    a: &AnnouncementInput,
) -> DbResult<AnnouncementRow> {
    sqlx::query_as::<_, AnnouncementRow>(
        "insert into announcements (id, tenant_id, slug, locale, translation_group_id, title, excerpt, body, status,
                                    published_at, is_pinned, author_id)
         values ($1, $2, $3, $4, coalesce($5, hf_uuid_v7()), $6, $7, $8, $9, $10, $11, $12) returning *",
    )
    .bind(AnnouncementId::new())
    .bind(tenant_id)
    .bind(&a.slug)
    .bind(a.locale)
    .bind(a.translation_group_id)
    .bind(&a.title)
    .bind(&a.excerpt)
    .bind(&a.body)
    .bind(a.status)
    .bind(a.published_at)
    .bind(a.is_pinned)
    .bind(a.author_id)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("announcement slug"))
}

/// Updates an announcement (translation group unchanged).
pub async fn update_announcement(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AnnouncementId,
    a: &AnnouncementInput,
) -> DbResult<Option<AnnouncementRow>> {
    sqlx::query_as::<_, AnnouncementRow>(
        "update announcements set slug = $3, locale = $4, title = $5, excerpt = $6, body = $7, status = $8,
                published_at = $9, is_pinned = $10
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&a.slug)
    .bind(a.locale)
    .bind(&a.title)
    .bind(&a.excerpt)
    .bind(&a.body)
    .bind(a.status)
    .bind(a.published_at)
    .bind(a.is_pinned)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("announcement slug"))
}

/// Content health numbers for the SEO overview.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, FromRow)]
pub struct ContentSeoStats {
    /// Publicly visible pages, posts, KB articles and announcements (all languages).
    pub published: i64,
    /// Published rows without a hand-written meta description in `seo_meta`.
    pub missing_meta: i64,
    /// Translation groups with a published row in only one language.
    pub missing_translation: i64,
    /// Rows in `draft`.
    pub drafts: i64,
}

/// [`ContentSeoStats`] of a tenant.
pub async fn content_seo_stats(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<ContentSeoStats> {
    let sql = format!(
        "with c as (
             select 'page' as t, id, translation_group_id, locale, status, published_at from pages where tenant_id = $1
             union all
             select 'blog_post', id, translation_group_id, locale, status, published_at from blog_posts where tenant_id = $1
             union all
             select 'kb_article', id, translation_group_id, locale, status, published_at from kb_articles
              where tenant_id = $1 and kind = 'article'
             union all
             select 'announcement', id, translation_group_id, locale, status, published_at from announcements
              where tenant_id = $1
         ), p as (select * from c where {PUBLISHED})
         select (select count(*) from p) as published,
                (select count(*) from p where not exists (
                     select 1 from seo_meta m where m.tenant_id = $1 and m.entity_id = p.id and m.locale = p.locale
                        and coalesce(m.description, '') <> '')) as missing_meta,
                (select count(*) from (select 1 from p group by t, translation_group_id
                                       having count(distinct locale) < 2) x) as missing_translation,
                (select count(*) from c where status = 'draft') as drafts"
    );
    Ok(sqlx::query_as::<_, ContentSeoStats>(&sql)
        .bind(tenant_id)
        .fetch_one(db)
        .await?)
}

/// Every field of a KB category (admin create / edit).
#[derive(Debug, Clone, Default)]
pub struct KbCategoryInput {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    /// `{"en": {"name": .., "description": ..}}`
    pub translations: Json,
    pub icon: Option<String>,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// All KB categories (hidden ones too) with their article count (any status).
pub async fn list_kb_categories_admin(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<KbCategoryWithCount>> {
    Ok(sqlx::query_as::<_, KbCategoryWithCount>(
        "select c.*, (select count(*) from kb_articles a where a.category_id = c.id) as article_count
         from kb_categories c where c.tenant_id = $1 order by c.sort_order, c.name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// KB category by id.
pub async fn get_kb_category(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: KbCategoryId,
) -> DbResult<Option<KbCategoryRow>> {
    Ok(sqlx::query_as::<_, KbCategoryRow>(
        "select * from kb_categories where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Creates a KB category with every field.
pub async fn create_kb_category_full(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    c: &KbCategoryInput,
) -> DbResult<KbCategoryRow> {
    sqlx::query_as::<_, KbCategoryRow>(
        "insert into kb_categories (id, tenant_id, slug, name, description, translations, icon, is_hidden, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning *",
    )
    .bind(KbCategoryId::new())
    .bind(tenant_id)
    .bind(&c.slug)
    .bind(&c.name)
    .bind(&c.description)
    .bind(&c.translations)
    .bind(&c.icon)
    .bind(c.is_hidden)
    .bind(c.sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("kb category"))
}

/// Updates a KB category.
pub async fn update_kb_category(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: KbCategoryId,
    c: &KbCategoryInput,
) -> DbResult<Option<KbCategoryRow>> {
    sqlx::query_as::<_, KbCategoryRow>(
        "update kb_categories set slug = $3, name = $4, description = $5, translations = $6, icon = $7,
                is_hidden = $8, sort_order = $9
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&c.slug)
    .bind(&c.name)
    .bind(&c.description)
    .bind(&c.translations)
    .bind(&c.icon)
    .bind(c.is_hidden)
    .bind(c.sort_order)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("kb category"))
}

/// Deletes a KB category (its articles become uncategorised).
pub async fn delete_kb_category(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: KbCategoryId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from kb_categories where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}
