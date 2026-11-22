//! `/admin/content/*` — CMS pages and blog posts, plus the shared
//! translation-group editor that the knowledge base and announcements
//! (`kb.rs`) use too.
//!
//! One editor screen holds every language version of a piece of content
//! (`translation_group_id`) side by side: title, slug (validated, conflicts
//! reported), Markdown body with live preview, draft / scheduled / published
//! / archived, and the `seo_meta` fields (title, description, canonical,
//! noindex / nofollow, OG image, keywords) with a live SERP preview and
//! length counters (`static/js/admin_content.js`). Saving a live version
//! queues an IndexNow ping when the tenant enabled it, and renaming the slug
//! of a published version offers a one-click 301 redirect.

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use hos_auth::rbac::Permission;
use hos_core::domain::Locale;
use hos_core::ids::{AnnouncementId, BlogPostId, KbArticleId, PageId, UserId};
use hos_db::enums::{ContentStatus, KbArticleKind, SeoEntityType};
use hos_db::ids::KbCategoryId;
use hos_db::repo::content::{
    self, AnnouncementInput, BlogPostInput, ContentGroupFilter, ContentGroupRow, ContentTable,
    KbArticleInput, KbCategoryRow, PageInput,
};
use hos_db::repo::platform;
use hos_db::repo::seo::{self as seo_repo, SeoMetaInput, SeoMetaRow, SeoTarget};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, PageQuery};
use crate::forms::FormErrors;
use crate::locale::localize_path;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .merge(kind_router(Kind::Page))
        .merge(kind_router(Kind::Post))
}

// ---------------------------------------------------------------------------
// Content kinds
// ---------------------------------------------------------------------------

/// The four localized, slugged, publishable content tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// CMS page (`pages`).
    Page,
    /// Blog post (`blog_posts`).
    Post,
    /// Knowledge-base article / FAQ entry (`kb_articles`).
    Kb,
    /// Announcement (`announcements`).
    News,
}

impl Kind {
    /// Admin base path.
    pub const fn base(self) -> &'static str {
        match self {
            Self::Page => "/admin/content/pages",
            Self::Post => "/admin/content/posts",
            Self::Kb => "/admin/support/kb",
            Self::News => "/admin/support/announcements",
        }
    }

    /// Table.
    pub const fn table(self) -> ContentTable {
        match self {
            Self::Page => ContentTable::Pages,
            Self::Post => ContentTable::BlogPosts,
            Self::Kb => ContentTable::KbArticles,
            Self::News => ContentTable::Announcements,
        }
    }

    /// `seo_meta.entity_type`.
    pub const fn seo_type(self) -> SeoEntityType {
        match self {
            Self::Page => SeoEntityType::Page,
            Self::Post => SeoEntityType::BlogPost,
            Self::Kb => SeoEntityType::KbArticle,
            Self::News => SeoEntityType::Announcement,
        }
    }

    /// Entity name in `audit_log` / `ai_generations`.
    pub const fn entity(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Post => "blog_post",
            Self::Kb => "kb_article",
            Self::News => "announcement",
        }
    }

    /// Kind from an entity name.
    pub fn from_entity(s: &str) -> Option<Self> {
        match s {
            "page" => Some(Self::Page),
            "blog_post" => Some(Self::Post),
            "kb_article" => Some(Self::Kb),
            "announcement" => Some(Self::News),
            _ => None,
        }
    }

    /// i18n key segment (`admin_content.kinds.<key>.*`).
    pub const fn key(self) -> &'static str {
        match self {
            Self::Page => "pages",
            Self::Post => "posts",
            Self::Kb => "kb",
            Self::News => "news",
        }
    }

    /// Permission needed to edit.
    pub const fn permission(self) -> Permission {
        match self {
            Self::Page | Self::Post => Permission::CONTENT_MANAGE,
            Self::Kb | Self::News => Permission::SUPPORT_MANAGE,
        }
    }

    /// Unprefixed public path of a slug.
    pub fn public_path(self, slug: &str) -> String {
        match self {
            Self::Page => format!("/{slug}"),
            Self::Post => format!("/blog/{slug}"),
            Self::Kb => format!("/kb/{slug}"),
            Self::News => format!("/announcements/{slug}"),
        }
    }

    /// Kind of a public path prefix (`/blog/x` → `Post`, `x`).
    pub fn from_public_path(path: &str) -> Option<(Self, &str)> {
        let p = path.trim_end_matches('/');
        for (prefix, kind) in [
            ("/blog/", Self::Post),
            ("/kb/", Self::Kb),
            ("/announcements/", Self::News),
        ] {
            if let Some(slug) = p.strip_prefix(prefix) {
                return (!slug.contains('/') && !slug.is_empty()).then_some((kind, slug));
            }
        }
        let slug = p.strip_prefix('/')?;
        (!slug.is_empty() && !slug.contains('/')).then_some((Self::Page, slug))
    }
}

/// Slugs a CMS page may not use (storefront and area routes).
const RESERVED_PAGE_SLUGS: &[&str] = &[
    "admin",
    "api",
    "announcements",
    "assets",
    "auth",
    "blog",
    "cart",
    "checkout",
    "client",
    "contact",
    "domains",
    "en",
    "healthz",
    "internal",
    "kb",
    "lang",
    "platform",
    "readyz",
    "static",
    "tr",
    "webhooks",
    "whois",
];

/// URL slug: lowercase ASCII letters / digits separated by single hyphens.
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 120
        && s.split('-').all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Languages in display order (the tenant's default first).
pub fn locales(default: Locale) -> [Locale; 2] {
    if default == Locale::En {
        [Locale::En, Locale::Tr]
    } else {
        [Locale::Tr, Locale::En]
    }
}

/// Whether a row with this status / date is publicly visible now.
pub fn is_live(status: ContentStatus, published_at: Option<DateTime<Utc>>) -> bool {
    let now = Utc::now();
    match status {
        ContentStatus::Published => published_at.is_none_or(|p| p <= now),
        ContentStatus::Scheduled => published_at.is_some_and(|p| p <= now),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Rows (one view over the four tables)
// ---------------------------------------------------------------------------

/// A content row of any [`Kind`].
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub id: Option<Uuid>,
    pub group: Option<Uuid>,
    pub locale: Locale,
    pub slug: String,
    pub title: String,
    pub excerpt: Option<String>,
    pub body: String,
    pub status: ContentStatus,
    pub published_at: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub category: Option<String>,
    pub cover: Option<String>,
    pub author_name: Option<String>,
    pub kb_category: Option<KbCategoryId>,
    pub kb_kind: KbArticleKind,
    pub featured: bool,
    pub sort_order: i32,
    pub pinned: bool,
    pub show_in_nav: bool,
    pub show_in_footer: bool,
    pub nav_order: i32,
    pub template: String,
    pub author_id: Option<UserId>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Row {
    /// Visible on the storefront now.
    pub fn live(&self) -> bool {
        self.id.is_some() && is_live(self.status, self.published_at)
    }
}

/// Rows of a translation group.
pub async fn rows_in_group(
    state: &AppState,
    tenant: &Tenant,
    kind: Kind,
    group: Uuid,
) -> AppResult<Vec<Row>> {
    let pool = &state.pool;
    let base = |id: Uuid, group: Uuid, locale: Locale| Row {
        id: Some(id),
        group: Some(group),
        locale,
        template: "default".into(),
        ..Row::default()
    };
    Ok(match kind {
        Kind::Page => content::pages_in_group(pool, tenant.id, group)
            .await?
            .into_iter()
            .map(|r| Row {
                slug: r.slug,
                title: r.title,
                body: r.body,
                status: r.status,
                published_at: r.published_at,
                show_in_nav: r.show_in_nav,
                show_in_footer: r.show_in_footer,
                nav_order: r.nav_order,
                template: r.template,
                author_id: r.author_id,
                updated_at: Some(r.updated_at),
                ..base(r.id.into(), r.translation_group_id, r.locale)
            })
            .collect(),
        Kind::Post => content::blog_posts_in_group(pool, tenant.id, group)
            .await?
            .into_iter()
            .map(|r| Row {
                slug: r.slug,
                title: r.title,
                excerpt: r.excerpt,
                body: r.body,
                status: r.status,
                published_at: r.published_at,
                tags: r.tags,
                category: r.category,
                cover: r.cover_image_url,
                author_name: r.author_name,
                author_id: r.author_id,
                updated_at: Some(r.updated_at),
                ..base(r.id.into(), r.translation_group_id, r.locale)
            })
            .collect(),
        Kind::Kb => content::kb_articles_in_group(pool, tenant.id, group)
            .await?
            .into_iter()
            .map(|r| Row {
                slug: r.slug,
                title: r.title,
                excerpt: r.excerpt,
                body: r.body,
                status: r.status,
                published_at: r.published_at,
                tags: r.tags,
                kb_category: r.category_id,
                kb_kind: r.kind,
                featured: r.is_featured,
                sort_order: r.sort_order,
                author_id: r.author_id,
                updated_at: Some(r.updated_at),
                ..base(r.id.into(), r.translation_group_id, r.locale)
            })
            .collect(),
        Kind::News => content::announcements_in_group(pool, tenant.id, group)
            .await?
            .into_iter()
            .map(|r| Row {
                slug: r.slug,
                title: r.title,
                excerpt: r.excerpt,
                body: r.body,
                status: r.status,
                published_at: r.published_at,
                pinned: r.is_pinned,
                author_id: r.author_id,
                updated_at: Some(r.updated_at),
                ..base(r.id.into(), r.translation_group_id, r.locale)
            })
            .collect(),
    })
}

/// Rows of the group that `id` belongs to (404 for another tenant's id).
pub async fn group_of_row(
    state: &AppState,
    tenant: &Tenant,
    kind: Kind,
    id: Uuid,
) -> AppResult<Uuid> {
    content::content_group_of(&state.pool, tenant.id, kind.table(), id)
        .await?
        .ok_or(AppError::NotFound)
}

/// Creates (`row.id == None`) or updates a row; returns its id and group.
pub async fn save_row(
    state: &AppState,
    tenant: &Tenant,
    kind: Kind,
    row: &Row,
) -> AppResult<(Uuid, Uuid)> {
    let pool = &state.pool;
    let r = row.clone();
    let not_found = || AppError::NotFound;
    Ok(match kind {
        Kind::Page => {
            let input = PageInput {
                slug: r.slug,
                locale: r.locale,
                translation_group_id: r.group,
                title: r.title,
                body: r.body,
                template: if r.template.is_empty() {
                    "default".into()
                } else {
                    r.template
                },
                status: r.status,
                published_at: r.published_at,
                show_in_nav: r.show_in_nav,
                show_in_footer: r.show_in_footer,
                nav_order: r.nav_order,
                author_id: r.author_id,
            };
            let saved = match row.id {
                Some(id) => content::update_page(pool, tenant.id, PageId::from(id), &input)
                    .await?
                    .ok_or_else(not_found)?,
                None => content::create_page(pool, tenant.id, &input).await?,
            };
            (saved.id.into(), saved.translation_group_id)
        }
        Kind::Post => {
            let minutes = i32::try_from(crate::markdown::reading_minutes(&r.body)).unwrap_or(1);
            let input = BlogPostInput {
                slug: r.slug,
                locale: r.locale,
                translation_group_id: r.group,
                title: r.title,
                excerpt: r.excerpt,
                body: r.body,
                cover_image_url: r.cover,
                category: r.category,
                tags: r.tags,
                status: r.status,
                published_at: r.published_at,
                reading_minutes: Some(minutes.max(1)),
                author_id: r.author_id,
                author_name: r.author_name,
            };
            let saved = match row.id {
                Some(id) => {
                    content::update_blog_post(pool, tenant.id, BlogPostId::from(id), &input)
                        .await?
                        .ok_or_else(not_found)?
                }
                None => content::create_blog_post(pool, tenant.id, &input).await?,
            };
            (saved.id.into(), saved.translation_group_id)
        }
        Kind::Kb => {
            let input = KbArticleInput {
                category_id: r.kb_category,
                kind: r.kb_kind,
                slug: r.slug,
                locale: r.locale,
                translation_group_id: r.group,
                title: r.title,
                excerpt: r.excerpt,
                body: r.body,
                status: r.status,
                published_at: r.published_at,
                tags: r.tags,
                is_featured: r.featured,
                sort_order: r.sort_order,
                author_id: r.author_id,
            };
            let saved = match row.id {
                Some(id) => {
                    content::update_kb_article(pool, tenant.id, KbArticleId::from(id), &input)
                        .await?
                        .ok_or_else(not_found)?
                }
                None => content::create_kb_article(pool, tenant.id, &input).await?,
            };
            (saved.id.into(), saved.translation_group_id)
        }
        Kind::News => {
            let input = AnnouncementInput {
                slug: r.slug,
                locale: r.locale,
                translation_group_id: r.group,
                title: r.title,
                excerpt: r.excerpt,
                body: r.body,
                status: r.status,
                published_at: r.published_at,
                is_pinned: r.pinned,
                author_id: r.author_id,
            };
            let saved = match row.id {
                Some(id) => {
                    content::update_announcement(pool, tenant.id, AnnouncementId::from(id), &input)
                        .await?
                        .ok_or_else(not_found)?
                }
                None => content::create_announcement_full(pool, tenant.id, &input).await?,
            };
            (saved.id.into(), saved.translation_group_id)
        }
    })
}

/// A free slug for `kind` in `locale` starting from `wanted`
/// (`slug`, `slug-2`, `slug-3` …).
pub async fn free_slug(
    state: &AppState,
    tenant: &Tenant,
    kind: Kind,
    locale: Locale,
    wanted: &str,
    except: Option<Uuid>,
) -> AppResult<String> {
    let base = if valid_slug(wanted) {
        wanted.to_owned()
    } else {
        let s = crate::markdown::slugify(wanted);
        if valid_slug(&s) { s } else { "content".into() }
    };
    for n in 1..50 {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        if !slug_taken(state, tenant, kind, locale, &candidate, except).await? {
            return Ok(candidate);
        }
    }
    Ok(format!(
        "{base}-{}",
        &Uuid::now_v7().simple().to_string()[24..]
    ))
}

async fn slug_taken(
    state: &AppState,
    tenant: &Tenant,
    kind: Kind,
    locale: Locale,
    slug: &str,
    except: Option<Uuid>,
) -> AppResult<bool> {
    if content::content_by_slug(&state.pool, tenant.id, kind.table(), locale, slug, except)
        .await?
        .is_some()
    {
        return Ok(true);
    }
    if kind == Kind::Page {
        if RESERVED_PAGE_SLUGS.contains(&slug) {
            return Ok(true);
        }
        let groups =
            hos_db::repo::catalog::list_product_groups(&state.pool, tenant.id, true).await?;
        if groups.iter().any(|g| g.slug == slug) {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// SEO meta fields (shared with /admin/seo/meta)
// ---------------------------------------------------------------------------

/// The editable `seo_meta` fields of one target + language.
#[derive(Debug, Clone, Default)]
pub struct MetaFields {
    pub title: String,
    pub description: String,
    pub canonical: String,
    pub og_image: String,
    pub keywords: String,
    pub noindex: bool,
    pub nofollow: bool,
}

impl MetaFields {
    /// From a stored row.
    pub fn from_row(m: Option<&SeoMetaRow>) -> Self {
        let Some(m) = m else {
            return Self::default();
        };
        Self {
            title: m.title.clone().unwrap_or_default(),
            description: m.description.clone().unwrap_or_default(),
            canonical: m.canonical.clone().unwrap_or_default(),
            og_image: m
                .og
                .get("image")
                .and_then(|i| i.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            keywords: m.keywords.join(", "),
            noindex: m.noindex,
            nofollow: m.nofollow,
        }
    }

    /// From form fields `<prefix>meta_title`, … .
    pub fn read(f: &FormData, prefix: &str) -> Self {
        Self {
            title: f.text(&format!("{prefix}meta_title"), 200),
            description: f.text(&format!("{prefix}meta_description"), 400),
            canonical: f.text(&format!("{prefix}canonical"), 500),
            og_image: f.text(&format!("{prefix}og_image"), 500),
            keywords: f.text(&format!("{prefix}keywords"), 600),
            noindex: f.flag(&format!("{prefix}noindex")),
            nofollow: f.flag(&format!("{prefix}nofollow")),
        }
    }

    /// Nothing set.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty()
            && self.description.is_empty()
            && self.canonical.is_empty()
            && self.og_image.is_empty()
            && self.keywords.is_empty()
            && !self.noindex
            && !self.nofollow
    }

    /// Validates into `errors` (field names with `prefix`).
    pub fn validate(&self, ctx: &PageCtx, errors: &mut FormErrors, prefix: &str) {
        if self.title.chars().count() > 120 {
            errors.add(
                &format!("{prefix}meta_title"),
                ctx.t("admin_content.meta.title_too_long"),
            );
        }
        if self.description.chars().count() > 320 {
            errors.add(
                &format!("{prefix}meta_description"),
                ctx.t("admin_content.meta.description_too_long"),
            );
        }
        if !self.canonical.is_empty() && !is_http_url(&self.canonical) {
            errors.add(
                &format!("{prefix}canonical"),
                ctx.t("admin_content.meta.url_invalid"),
            );
        }
        if !self.og_image.is_empty() && !is_http_url(&self.og_image) {
            errors.add(
                &format!("{prefix}og_image"),
                ctx.t("admin_content.meta.url_invalid"),
            );
        }
        if self.keyword_list().len() > 20 {
            errors.add(
                &format!("{prefix}keywords"),
                ctx.t("admin_content.meta.too_many_keywords"),
            );
        }
    }

    /// Keywords as a list.
    pub fn keyword_list(&self) -> Vec<String> {
        self.keywords
            .split(',')
            .map(|k| k.trim().chars().take(80).collect::<String>())
            .filter(|k| !k.is_empty())
            .collect()
    }

    fn to_input(&self, existing: Option<&SeoMetaRow>) -> SeoMetaInput {
        let mut og = existing
            .map(|e| e.og.clone())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(obj) = og.as_object_mut() {
            if self.og_image.is_empty() {
                obj.remove("image");
            } else {
                obj.insert("image".into(), json!({ "url": self.og_image }));
            }
        }
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        SeoMetaInput {
            title: opt(&self.title),
            description: opt(&self.description),
            canonical: opt(&self.canonical),
            keywords: self.keyword_list(),
            og: Some(og),
            jsonld: existing.and_then(|e| e.jsonld.clone()),
            noindex: self.noindex,
            nofollow: self.nofollow,
        }
    }
}

/// An absolute `http(s)` URL.
pub fn is_http_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
}

/// Saves (or clears) the metadata of `target` in `locale`.
pub async fn save_meta(
    state: &AppState,
    tenant: &Tenant,
    target: &SeoTarget,
    locale: Locale,
    m: &MetaFields,
) -> AppResult<()> {
    let existing = seo_repo::get_seo_meta(&state.pool, tenant.id, target, locale).await?;
    if m.is_empty() {
        if let Some(e) = &existing {
            if e.jsonld.is_none() {
                seo_repo::delete_seo_meta(&state.pool, tenant.id, target, locale).await?;
            } else {
                seo_repo::upsert_seo_meta(
                    &state.pool,
                    tenant.id,
                    target,
                    locale,
                    &m.to_input(Some(e)),
                )
                .await?;
            }
        }
        return Ok(());
    }
    seo_repo::upsert_seo_meta(
        &state.pool,
        tenant.id,
        target,
        locale,
        &m.to_input(existing.as_ref()),
    )
    .await?;
    Ok(())
}

/// Title suffix the storefront appends (`seo.title_suffix`, else ` | Site`).
pub async fn title_suffix(state: &AppState, tenant: &Tenant) -> AppResult<String> {
    Ok(
        platform::get_setting(&state.pool, tenant.id, "seo.title_suffix")
            .await?
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!(" | {}", tenant.display_name())),
    )
}

// ---------------------------------------------------------------------------
// IndexNow
// ---------------------------------------------------------------------------

/// Whether published content is pinged automatically (`seo.indexnow` and
/// `seo.indexnow_auto`).
pub async fn indexnow_auto(state: &AppState, tenant: &Tenant) -> AppResult<bool> {
    let s = platform::get_settings(&state.pool, tenant.id, "seo.indexnow").await?;
    let flag = |k: &str| s.get(k).and_then(Value::as_bool).unwrap_or(false);
    Ok(flag("seo.indexnow") && flag("seo.indexnow_auto"))
}

/// Queues an IndexNow submission of `urls` (absolute, on the request host).
/// `Ok(false)` when IndexNow is disabled or nothing is left to send.
pub async fn queue_indexnow(
    state: &AppState,
    tenant: &Tenant,
    origin: &str,
    urls: Vec<String>,
) -> AppResult<bool> {
    let Some(key) = crate::routes::storefront::files::indexnow_key(state, tenant).await? else {
        return Ok(false);
    };
    let Some(host) = url::Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    else {
        return Ok(false);
    };
    let urls: Vec<String> = urls
        .into_iter()
        .filter(|u| url::Url::parse(u).is_ok_and(|p| p.host_str() == Some(host.as_str())))
        .take(hos_seo::indexnow::MAX_URLS_PER_REQUEST)
        .collect();
    if urls.is_empty() {
        return Ok(false);
    }
    let mut conn = state.pool.acquire().await?;
    hos_billing::jobs::enqueue_indexnow(
        &mut conn,
        tenant.id,
        &hos_billing::jobs::IndexNowPayload {
            host,
            key,
            key_location: None,
            urls,
        },
    )
    .await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Routes per kind
// ---------------------------------------------------------------------------

/// Routes of one content kind (list, new, edit, delete).
pub(super) fn kind_router(kind: Kind) -> Router<AppState> {
    let base = kind.base();
    Router::new()
        .route(
            base,
            get(
                move |state: State<AppState>,
                      tenant: Tenant,
                      staff: CurrentStaff,
                      q: PageQuery,
                      f: Query<ListFilter>,
                      ctx: PageCtx| list(kind, state, tenant, staff, q, f, ctx),
            )
            .post(
                move |state: State<AppState>,
                      tenant: Tenant,
                      staff: CurrentStaff,
                      au: Audit,
                      ctx: PageCtx,
                      f: FormData| {
                    save(kind, state, tenant, staff, au, None, ctx, f)
                },
            ),
        )
        .route(
            &format!("{base}/new"),
            get(
                move |state: State<AppState>, tenant: Tenant, staff: CurrentStaff, ctx: PageCtx| {
                    new_form(kind, state, tenant, staff, ctx)
                },
            ),
        )
        .route(
            &format!("{base}/{{group}}"),
            get(
                move |state: State<AppState>,
                      tenant: Tenant,
                      staff: CurrentStaff,
                      Path(group): Path<Uuid>,
                      Query(sq): Query<SuggestQuery>,
                      ctx: PageCtx| {
                    edit_form(kind, state, tenant, staff, group, sq, ctx)
                },
            )
            .post(
                move |state: State<AppState>,
                      tenant: Tenant,
                      staff: CurrentStaff,
                      au: Audit,
                      Path(group): Path<Uuid>,
                      ctx: PageCtx,
                      f: FormData| {
                    save(kind, state, tenant, staff, au, Some(group), ctx, f)
                },
            ),
        )
        .route(
            &format!("{base}/{{group}}/delete"),
            post(
                move |state: State<AppState>,
                      tenant: Tenant,
                      staff: CurrentStaff,
                      au: Audit,
                      Path(group): Path<Uuid>,
                      ctx: PageCtx| {
                    delete_group(kind, state, tenant, staff, au, group, None, ctx)
                },
            ),
        )
        .route(
            &format!("{base}/{{group}}/{{locale}}/delete"),
            post(
                move |state: State<AppState>,
                      tenant: Tenant,
                      staff: CurrentStaff,
                      au: Audit,
                      Path((group, locale)): Path<(Uuid, String)>,
                      ctx: PageCtx| {
                    delete_group(kind, state, tenant, staff, au, group, Some(locale), ctx)
                },
            ),
        )
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// Query filters of a content list.
#[derive(Debug, Default, Deserialize)]
pub struct ListFilter {
    #[serde(default)]
    status: String,
    #[serde(default)]
    missing: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    kind: String,
}

/// One language column of a list row.
pub struct LangCell {
    pub locale: &'static str,
    pub status: String,
    pub live_url: String,
}

/// One list row (a translation group).
pub struct GroupView {
    pub group: Uuid,
    pub title: String,
    pub slug: String,
    pub cells: Vec<LangCell>,
    pub updated_at: DateTime<Utc>,
    pub missing: bool,
}

#[derive(Template)]
#[template(path = "admin/content/list.html")]
struct ListPage {
    ctx: PageCtx,
    base: &'static str,
    heading: String,
    lead: String,
    new_label: String,
    empty_title: String,
    empty_text: String,
    rows: Vec<GroupView>,
    pager: Pager,
    q: String,
    status: String,
    missing: bool,
    category: String,
    kb_kind: String,
    statuses: Vec<(String, String)>,
    categories: Vec<KbCategoryRow>,
    is_kb: bool,
    can_edit: bool,
}
crate::page!(ListPage);

fn group_view(ctx: &PageCtx, kind: Kind, r: &ContentGroupRow) -> GroupView {
    let mut cells = Vec::new();
    for l in locales(ctx.default_locale) {
        let (status, slug, published_at) = match l {
            Locale::Tr => (r.tr_status, r.tr_slug.as_deref(), r.tr_published_at),
            Locale::En => (r.en_status, r.en_slug.as_deref(), r.en_published_at),
        };
        let live_url = match (status, slug) {
            (Some(s), Some(slug)) if is_live(s, published_at) => format!(
                "{}{}",
                ctx.origin,
                localize_path(&kind.public_path(slug), l, ctx.default_locale)
            ),
            _ => String::new(),
        };
        cells.push(LangCell {
            locale: l.as_str(),
            status: status.map(|s| s.as_str().to_owned()).unwrap_or_default(),
            live_url,
        });
    }
    let (first_title, first_slug, other_title, other_slug) = if ctx.default_locale == Locale::En {
        (&r.en_title, &r.en_slug, &r.tr_title, &r.tr_slug)
    } else {
        (&r.tr_title, &r.tr_slug, &r.en_title, &r.en_slug)
    };
    GroupView {
        group: r.translation_group_id,
        title: first_title
            .clone()
            .or_else(|| other_title.clone())
            .unwrap_or_default(),
        slug: first_slug
            .clone()
            .or_else(|| other_slug.clone())
            .unwrap_or_default(),
        missing: r.tr_id.is_none() || r.en_id.is_none(),
        cells,
        updated_at: r.updated_at,
    }
}

#[allow(clippy::too_many_arguments)]
async fn list(
    kind: Kind,
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    staff.require(kind.permission())?;
    let filter = ContentGroupFilter {
        search: q.q.clone().filter(|s| !s.trim().is_empty()),
        status: f.status.parse().ok(),
        kb_category: Uuid::parse_str(&f.category).ok().map(KbCategoryId::from),
        kb_kind: f.kind.parse().ok(),
        missing_translation: matches!(f.missing.as_str(), "1" | "on" | "true"),
    };
    let paged =
        content::list_content_groups(&state.pool, tenant.id, kind.table(), &filter, q.db_page())
            .await?;
    let k = kind.key();
    let heading = ctx.t(&format!("admin_content.kinds.{k}.title"));
    common::page(&mut ctx, &staff, &heading, &[]);
    let missing_str = if filter.missing_translation { "1" } else { "" };
    let pager = Pager::new(
        &paged,
        &ctx.url(kind.base()),
        &[
            ("q", q.q.as_deref().unwrap_or("")),
            ("status", &f.status),
            ("missing", missing_str),
            ("category", &f.category),
            ("kind", &f.kind),
        ],
    );
    let categories = if kind == Kind::Kb {
        content::list_kb_categories_admin(&state.pool, tenant.id)
            .await?
            .into_iter()
            .map(|c| c.category)
            .collect()
    } else {
        Vec::new()
    };
    let rows = paged
        .items
        .iter()
        .map(|r| group_view(&ctx, kind, r))
        .collect();
    render(&ListPage {
        base: kind.base(),
        lead: ctx.t(&format!("admin_content.kinds.{k}.lead")),
        new_label: ctx.t(&format!("admin_content.kinds.{k}.new")),
        empty_title: ctx.t(&format!("admin_content.kinds.{k}.empty_title")),
        empty_text: ctx.t(&format!("admin_content.kinds.{k}.empty_text")),
        heading,
        rows,
        pager,
        q: q.q.unwrap_or_default(),
        status: f.status,
        missing: filter.missing_translation,
        category: f.category,
        kb_kind: f.kind,
        statuses: status_options(&ctx),
        categories,
        is_kb: kind == Kind::Kb,
        can_edit: true,
        ctx,
    })
}

/// `(value, label)` of every content status.
pub fn status_options(ctx: &PageCtx) -> Vec<(String, String)> {
    common::enum_options(ctx, ContentStatus::ALL, |s| s.as_str(), "admin.status")
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// One language version in the editor.
#[derive(Debug, Clone, Default)]
pub struct Version {
    pub locale: String,
    pub locale_label: String,
    pub id: Option<Uuid>,
    pub title: String,
    pub slug: String,
    pub excerpt: String,
    pub body: String,
    pub status: String,
    pub published_at: String,
    pub tags: String,
    pub category: String,
    pub meta: MetaFields,
    pub live_url: String,
    pub serp_url: String,
    pub old_slug: String,
}

impl Version {
    fn exists(&self) -> bool {
        self.id.is_some()
    }
}

/// Fields shared by every language version.
#[derive(Debug, Clone, Default)]
pub struct Shared {
    pub cover: String,
    pub author_name: String,
    pub kb_category: String,
    pub kb_kind: String,
    pub featured: bool,
    pub sort_order: String,
    pub pinned: bool,
    pub show_in_nav: bool,
    pub show_in_footer: bool,
    pub nav_order: String,
}

/// Redirect offered after a published slug changed.
#[derive(Debug, Default, Deserialize)]
pub struct SuggestQuery {
    #[serde(default)]
    suggest_from: String,
    #[serde(default)]
    suggest_to: String,
}

#[derive(Template)]
#[template(path = "admin/content/edit.html")]
struct EditPage {
    ctx: PageCtx,
    kind_key: &'static str,
    entity: &'static str,
    base: &'static str,
    action: String,
    group: Option<Uuid>,
    versions: Vec<Version>,
    shared: Shared,
    errors: FormErrors,
    statuses: Vec<(String, String)>,
    categories: Vec<KbCategoryRow>,
    title_suffix: String,
    suggest_from: String,
    suggest_to: String,
    ai_ready: bool,
    has_excerpt: bool,
    is_page: bool,
    is_post: bool,
    is_kb: bool,
    is_news: bool,
}
crate::page!(EditPage);

impl EditPage {
    fn other_exists(&self, locale: &str) -> bool {
        self.versions
            .iter()
            .any(|v| v.locale != locale && v.exists())
    }

    fn kind_lead(&self) -> String {
        self.t(&format!("admin_content.kinds.{}.lead", self.kind_key))
    }

    /// `type:id` of the other, existing language version (AI translation source).
    fn other_entity(&self, locale: &str) -> String {
        self.versions
            .iter()
            .find(|v| v.locale != locale && v.exists())
            .and_then(|v| v.id)
            .map(|id| format!("{}:{id}", self.entity))
            .unwrap_or_default()
    }

    /// `type:id` of a version (empty when not saved yet).
    fn entity_of(&self, v: &Version) -> String {
        v.id.map(|id| format!("{}:{id}", self.entity))
            .unwrap_or_default()
    }

    /// Error of `<locale>_<field>`.
    fn err(&self, v: &Version, field: &str) -> &str {
        self.errors.get(&format!("{}_{field}", v.locale))
    }

    /// Absolute URL prefix of a version's slug (SERP preview).
    fn url_prefix(&self, v: &Version) -> String {
        let kind = Kind::from_entity(self.entity).unwrap_or(Kind::Page);
        let l: Locale = v.locale.parse().unwrap_or(self.ctx.default_locale);
        let p = localize_path(&kind.public_path("x"), l, self.ctx.default_locale);
        format!("{}{}", self.ctx.origin, p.strip_suffix('x').unwrap_or(&p))
    }
}

fn version_from_row(ctx: &PageCtx, kind: Kind, r: &Row, meta: Option<&SeoMetaRow>) -> Version {
    let path = localize_path(&kind.public_path(&r.slug), r.locale, ctx.default_locale);
    Version {
        locale: r.locale.as_str().into(),
        locale_label: ctx.t(&format!("admin.common.lang_{}", r.locale.as_str())),
        id: r.id,
        title: r.title.clone(),
        slug: r.slug.clone(),
        excerpt: r.excerpt.clone().unwrap_or_default(),
        body: r.body.clone(),
        status: r.status.as_str().into(),
        published_at: common::local_input(&r.published_at, &ctx.timezone),
        tags: r.tags.join(", "),
        category: r.category.clone().unwrap_or_default(),
        meta: MetaFields::from_row(meta),
        live_url: if r.live() {
            format!("{}{path}", ctx.origin)
        } else {
            String::new()
        },
        serp_url: format!("{}{path}", ctx.origin),
        old_slug: r.slug.clone(),
    }
}

fn empty_version(ctx: &PageCtx, l: Locale) -> Version {
    Version {
        locale: l.as_str().into(),
        locale_label: ctx.t(&format!("admin.common.lang_{}", l.as_str())),
        status: "draft".into(),
        serp_url: format!(
            "{}{}",
            ctx.origin,
            localize_path("/…", l, ctx.default_locale)
        ),
        ..Version::default()
    }
}

fn shared_from_rows(rows: &[Row]) -> Shared {
    let Some(r) = rows.first() else {
        return Shared {
            kb_kind: "article".into(),
            sort_order: "0".into(),
            nav_order: "0".into(),
            ..Shared::default()
        };
    };
    Shared {
        cover: r.cover.clone().unwrap_or_default(),
        author_name: r.author_name.clone().unwrap_or_default(),
        kb_category: r.kb_category.map(|c| c.to_string()).unwrap_or_default(),
        kb_kind: r.kb_kind.as_str().into(),
        featured: r.featured,
        sort_order: r.sort_order.to_string(),
        pinned: r.pinned,
        show_in_nav: r.show_in_nav,
        show_in_footer: r.show_in_footer,
        nav_order: r.nav_order.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn editor(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    kind: Kind,
    mut ctx: PageCtx,
    group: Option<Uuid>,
    versions: Vec<Version>,
    shared: Shared,
    errors: FormErrors,
    suggest: SuggestQuery,
) -> AppResult<EditPage> {
    let k = kind.key();
    let list_title = format!("admin_content.kinds.{k}.title");
    let title = match versions.iter().find(|v| v.exists()) {
        Some(v) if !v.title.is_empty() => v.title.clone(),
        _ => ctx.t(&format!("admin_content.kinds.{k}.new")),
    };
    common::page(&mut ctx, staff, &title, &[(&list_title, kind.base())]);
    let categories = if kind == Kind::Kb {
        content::list_kb_categories_admin(&state.pool, tenant.id)
            .await?
            .into_iter()
            .map(|c| c.category)
            .collect()
    } else {
        Vec::new()
    };
    let ai_ready =
        staff.can(Permission::SEO_MANAGE) && super::ai_studio::provider_ready(state, tenant).await;
    let valid_path = |p: &str| p.starts_with('/') && !p.contains(['\r', '\n', ' ']);
    let (suggest_from, suggest_to) =
        if valid_path(&suggest.suggest_from) && valid_path(&suggest.suggest_to) {
            (suggest.suggest_from, suggest.suggest_to)
        } else {
            (String::new(), String::new())
        };
    Ok(EditPage {
        kind_key: k,
        entity: kind.entity(),
        base: kind.base(),
        action: match group {
            Some(g) => format!("{}/{g}", kind.base()),
            None => kind.base().to_owned(),
        },
        group,
        versions,
        shared,
        errors,
        statuses: status_options(&ctx),
        categories,
        title_suffix: title_suffix(state, tenant).await?,
        suggest_from,
        suggest_to,
        ai_ready,
        has_excerpt: kind != Kind::Page,
        is_page: kind == Kind::Page,
        is_post: kind == Kind::Post,
        is_kb: kind == Kind::Kb,
        is_news: kind == Kind::News,
        ctx,
    })
}

async fn new_form(
    kind: Kind,
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    ctx: PageCtx,
) -> AppResult<Response> {
    staff.require(kind.permission())?;
    let versions = locales(ctx.default_locale)
        .iter()
        .map(|l| empty_version(&ctx, *l))
        .collect();
    let page = editor(
        &state,
        &tenant,
        &staff,
        kind,
        ctx,
        None,
        versions,
        shared_from_rows(&[]),
        FormErrors::default(),
        SuggestQuery::default(),
    )
    .await?;
    render(&page)
}

async fn load_versions(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    kind: Kind,
    rows: &[Row],
) -> AppResult<Vec<Version>> {
    let mut out = Vec::new();
    for l in locales(ctx.default_locale) {
        match rows.iter().find(|r| r.locale == l) {
            Some(r) => {
                let id = r.id.ok_or(AppError::NotFound)?;
                let meta = seo_repo::get_seo_meta(
                    &state.pool,
                    tenant.id,
                    &SeoTarget::Entity(kind.seo_type(), id),
                    l,
                )
                .await?;
                out.push(version_from_row(ctx, kind, r, meta.as_ref()));
            }
            None => out.push(empty_version(ctx, l)),
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn edit_form(
    kind: Kind,
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    group: Uuid,
    sq: SuggestQuery,
    ctx: PageCtx,
) -> AppResult<Response> {
    staff.require(kind.permission())?;
    let rows = rows_in_group(&state, &tenant, kind, group).await?;
    if rows.is_empty() {
        return Err(AppError::NotFound);
    }
    let versions = load_versions(&state, &tenant, &ctx, kind, &rows).await?;
    let page = editor(
        &state,
        &tenant,
        &staff,
        kind,
        ctx,
        Some(group),
        versions,
        shared_from_rows(&rows),
        FormErrors::default(),
        sq,
    )
    .await?;
    render(&page)
}

fn read_version(ctx: &PageCtx, f: &FormData, l: Locale) -> Version {
    let p = l.as_str();
    let k = |name: &str| format!("{p}_{name}");
    Version {
        locale: p.into(),
        locale_label: ctx.t(&format!("admin.common.lang_{p}")),
        title: f.text(&k("title"), 200),
        slug: f.text(&k("slug"), 120).to_lowercase(),
        excerpt: f.body(&k("excerpt"), 600),
        body: f.body(&k("body"), 200_000),
        status: f.text(&k("status"), 20),
        published_at: f.text(&k("published_at"), 20),
        tags: f.text(&k("tags"), 500),
        category: f.text(&k("category"), 100),
        meta: MetaFields::read(f, &format!("{p}_")),
        ..Version::default()
    }
}

fn read_shared(f: &FormData) -> Shared {
    Shared {
        cover: f.text("cover", 500),
        author_name: f.text("author_name", 120),
        kb_category: f.text("kb_category", 40),
        kb_kind: f.text("kb_kind", 20),
        featured: f.flag("featured"),
        sort_order: f.text("sort_order", 10),
        pinned: f.flag("pinned"),
        show_in_nav: f.flag("show_in_nav"),
        show_in_footer: f.flag("show_in_footer"),
        nav_order: f.text("nav_order", 10),
    }
}

/// Comma-separated tags (trimmed, unique, max 20).
pub fn tag_list(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in s.split(',') {
        let t: String = t.trim().chars().take(60).collect();
        if !t.is_empty() && !out.iter().any(|x| x.eq_ignore_ascii_case(&t)) {
            out.push(t);
        }
    }
    out.truncate(20);
    out
}

#[allow(clippy::too_many_arguments)]
async fn save(
    kind: Kind,
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    au: Audit,
    group: Option<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    staff.require(kind.permission())?;
    let existing = match group {
        Some(g) => {
            let rows = rows_in_group(&state, &tenant, kind, g).await?;
            if rows.is_empty() {
                return Err(AppError::NotFound);
            }
            rows
        }
        None => Vec::new(),
    };
    let shared = read_shared(&f);
    let mut errors = FormErrors::default();
    let mut versions: Vec<Version> = Vec::new();
    let mut planned: Vec<(Row, MetaFields, Option<Row>)> = Vec::new();
    let sort_order: i32 = shared.sort_order.trim().parse().unwrap_or(0);
    let nav_order: i32 = shared.nav_order.trim().parse().unwrap_or(0);
    if !shared.cover.is_empty() && !is_http_url(&shared.cover) {
        errors.add("cover", ctx.t("admin_content.meta.url_invalid"));
    }
    let kb_category: Option<KbCategoryId> = Uuid::parse_str(&shared.kb_category)
        .ok()
        .map(KbCategoryId::from);
    if let Some(c) = kb_category
        && content::get_kb_category(&state.pool, tenant.id, c)
            .await?
            .is_none()
    {
        errors.add("kb_category", ctx.t("admin.common.invalid_value"));
    }
    let mut slugs_in_form: BTreeMap<String, String> = BTreeMap::new();
    for l in locales(ctx.default_locale) {
        let p = l.as_str();
        let before = existing.iter().find(|r| r.locale == l).cloned();
        let mut v = read_version(&ctx, &f, l);
        v.id = before.as_ref().and_then(|b| b.id);
        v.old_slug = before.as_ref().map(|b| b.slug.clone()).unwrap_or_default();
        v.serp_url = format!(
            "{}{}",
            ctx.origin,
            localize_path(&kind.public_path(&v.slug), l, ctx.default_locale)
        );
        let active = before.is_some() || !v.title.is_empty();
        if !active {
            versions.push(v);
            continue;
        }
        if v.title.is_empty() {
            errors.add(&format!("{p}_title"), ctx.t("admin.common.required"));
        }
        if v.slug.is_empty() {
            v.slug = crate::markdown::slugify(&v.title);
        }
        if !valid_slug(&v.slug) {
            errors.add(
                &format!("{p}_slug"),
                ctx.t("admin_content.editor.slug_invalid"),
            );
        } else if slug_taken(&state, &tenant, kind, l, &v.slug, v.id).await? {
            errors.add(
                &format!("{p}_slug"),
                ctx.t("admin_content.editor.slug_taken"),
            );
        }
        slugs_in_form.insert(p.into(), v.slug.clone());
        let mut status: ContentStatus = v.status.parse().unwrap_or(ContentStatus::Draft);
        let mut published_at = match common::parse_local_datetime(&v.published_at, &ctx.timezone) {
            Ok(d) => d,
            Err(()) => {
                errors.add(
                    &format!("{p}_published_at"),
                    ctx.t("admin.common.invalid_date"),
                );
                None
            }
        };
        match status {
            ContentStatus::Scheduled if published_at.is_none() => {
                errors.add(
                    &format!("{p}_published_at"),
                    ctx.t("admin_content.editor.schedule_needs_date"),
                );
            }
            ContentStatus::Published if published_at.is_some_and(|d| d > Utc::now()) => {
                status = ContentStatus::Scheduled;
            }
            ContentStatus::Published if published_at.is_none() => {
                published_at = Some(
                    before
                        .as_ref()
                        .and_then(|b| b.published_at)
                        .filter(|d| *d <= Utc::now())
                        .unwrap_or_else(Utc::now),
                );
            }
            _ => {}
        }
        v.status = status.as_str().into();
        if v.body.trim().is_empty() && kind != Kind::Page && status != ContentStatus::Draft {
            errors.add(
                &format!("{p}_body"),
                ctx.t("admin_content.editor.body_required"),
            );
        }
        v.meta.validate(&ctx, &mut errors, &format!("{p}_"));
        let base_row = before.clone().unwrap_or_else(|| Row {
            locale: l,
            group,
            author_id: Some(staff.user_id),
            template: "default".into(),
            ..Row::default()
        });
        let row = Row {
            slug: v.slug.clone(),
            title: v.title.clone(),
            excerpt: (!v.excerpt.is_empty()).then(|| v.excerpt.clone()),
            body: v.body.clone(),
            status,
            published_at,
            tags: tag_list(&v.tags),
            category: (!v.category.is_empty()).then(|| v.category.clone()),
            cover: (!shared.cover.is_empty()).then(|| shared.cover.clone()),
            author_name: (!shared.author_name.is_empty()).then(|| shared.author_name.clone()),
            kb_category,
            kb_kind: shared.kb_kind.parse().unwrap_or(KbArticleKind::Article),
            featured: shared.featured,
            sort_order,
            pinned: shared.pinned,
            show_in_nav: shared.show_in_nav,
            show_in_footer: shared.show_in_footer,
            nav_order,
            ..base_row
        };
        planned.push((row, v.meta.clone(), before));
        versions.push(v);
    }
    if planned.is_empty() {
        errors.add_general(ctx.t("admin_content.editor.need_one_language"));
    }
    if errors.any() {
        let page = editor(
            &state,
            &tenant,
            &staff,
            kind,
            ctx,
            group,
            versions,
            shared,
            errors,
            SuggestQuery::default(),
        )
        .await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }

    let mut group_id = group;
    let mut saved_ids = Vec::new();
    let mut ping = Vec::new();
    let mut suggestion: Option<(String, String)> = None;
    for (mut row, meta, before) in planned {
        if row.group.is_none() {
            row.group = group_id;
        }
        let (id, g) = save_row(&state, &tenant, kind, &row).await?;
        group_id = Some(g);
        saved_ids.push(id);
        save_meta(
            &state,
            &tenant,
            &SeoTarget::Entity(kind.seo_type(), id),
            row.locale,
            &meta,
        )
        .await?;
        row.id = Some(id);
        let path = localize_path(&kind.public_path(&row.slug), row.locale, ctx.default_locale);
        if row.live() {
            ping.push(format!("{}{path}", ctx.origin));
        }
        if let Some(b) = &before
            && b.live()
            && b.slug != row.slug
            && suggestion.is_none()
        {
            suggestion = Some((kind.public_path(&b.slug), path));
        }
    }
    let group_id = group_id.ok_or_else(|| AppError::internal("content saved without a group"))?;
    if kind == Kind::Page {
        state.store_chrome.invalidate_all();
    }
    let mut pinged = false;
    if !ping.is_empty() && indexnow_auto(&state, &tenant).await? {
        pinged = queue_indexnow(&state, &tenant, &ctx.origin, ping).await?;
    }
    au.log(
        &state,
        &format!("{}.save", kind.entity()),
        Some((kind.entity(), group_id)),
        json!({"rows": saved_ids, "slugs": slugs_in_form, "indexnow": pinged}),
    )
    .await?;
    let mut target = format!("{}/{group_id}", kind.base());
    if let Some((from, to)) = suggestion {
        target.push('?');
        target.push_str(&common::query_string(&[
            ("suggest_from", from),
            ("suggest_to", to),
        ]));
    }
    let key = if pinged {
        "admin_content.editor.saved_pinged"
    } else {
        "admin.common.saved"
    };
    Ok(common::done(&ctx, &target, key))
}

#[allow(clippy::too_many_arguments)]
async fn delete_group(
    kind: Kind,
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    au: Audit,
    group: Uuid,
    locale: Option<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    staff.require(kind.permission())?;
    let rows = rows_in_group(&state, &tenant, kind, group).await?;
    if rows.is_empty() {
        return Err(AppError::NotFound);
    }
    let only: Option<Locale> = match &locale {
        Some(l) => Some(l.parse().map_err(|_| AppError::NotFound)?),
        None => None,
    };
    let mut deleted = Vec::new();
    for r in rows.iter().filter(|r| only.is_none_or(|l| l == r.locale)) {
        if let Some(id) = r.id
            && content::delete_content(&state.pool, tenant.id, kind.table(), id).await?
        {
            deleted.push(json!({"id": id, "locale": r.locale.as_str(), "slug": r.slug}));
        }
    }
    if deleted.is_empty() {
        return Err(AppError::NotFound);
    }
    if kind == Kind::Page {
        state.store_chrome.invalidate_all();
    }
    au.log(
        &state,
        &format!("{}.delete", kind.entity()),
        Some((kind.entity(), group)),
        json!({"deleted": deleted}),
    )
    .await?;
    let remaining = rows.len() > deleted.len();
    let back = if remaining {
        format!("{}/{group}", kind.base())
    } else {
        kind.base().to_owned()
    };
    Ok(common::done(&ctx, &back, "admin.common.deleted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert!(valid_slug("vps-kampanya-2025"));
        assert!(!valid_slug("Vps"));
        assert!(!valid_slug("a--b"));
        assert!(!valid_slug("-a"));
        assert!(!valid_slug("a/b"));
        assert!(!valid_slug(""));
    }

    #[test]
    fn public_paths() {
        assert_eq!(Kind::from_public_path("/blog/x"), Some((Kind::Post, "x")));
        assert_eq!(Kind::from_public_path("/kb/y/"), Some((Kind::Kb, "y")));
        assert_eq!(
            Kind::from_public_path("/about"),
            Some((Kind::Page, "about"))
        );
        assert_eq!(Kind::from_public_path("/"), None);
        assert_eq!(Kind::from_public_path("/a/b"), None);
        assert_eq!(Kind::Post.public_path("x"), "/blog/x");
    }

    #[test]
    fn tags() {
        assert_eq!(tag_list("vps, VPS , linux,,"), vec!["vps", "linux"]);
    }
}
