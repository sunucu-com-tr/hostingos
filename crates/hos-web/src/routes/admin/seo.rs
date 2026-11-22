//! `/admin/seo/*` — SEO / GEO / AI visibility of the tenant storefront.
//!
//! * overview (last audit score + trend, GEO average, indexable pages,
//!   missing meta / translations, quick actions),
//! * site audit ([`audit`]: storefront pages rendered in-process from the
//!   sitemap, `hos_seo::audit::audit_site`, stored in `seo_audits`; report
//!   with severity filter, per-page detail, comparison with the previous
//!   run and an AI action plan) and GEO analysis per page,
//! * crawler policy / `robots.txt`, `llms.txt` texts and IndexNow
//!   ([`settings`] — the same `tenant_settings` keys the storefront reads),
//! * redirects ([`redirects`]), product / group / home metadata ([`meta`])
//!   and the sitemap status.
//!
//! Every route needs `seo.manage`.

pub mod audit;
pub mod meta;
pub mod redirects;
pub mod settings;

use askama::Template;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use hos_db::enums::RunStatus;
use hos_db::repo::content as content_repo;
use hos_db::repo::seo as seo_repo;
use tower::ServiceExt as _;

use super::common;
use crate::error::AppResult;
use crate::extract::{RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/seo", get(overview))
        .route("/admin/seo/sitemap", get(sitemap_status))
        .merge(audit::routes())
        .merge(settings::routes())
        .merge(redirects::routes())
        .merge(meta::routes())
}

/// SEO tab bar + the active tab href.
pub fn nav(ctx: &PageCtx, path: &str) -> (Vec<NavItem>, String) {
    let tabs = common::tabs(
        ctx,
        &[
            ("admin_content.seo.tabs.overview", "/admin/seo".to_owned()),
            (
                "admin_content.seo.tabs.audits",
                "/admin/seo/audits".to_owned(),
            ),
            ("admin_content.seo.tabs.geo", "/admin/seo/geo".to_owned()),
            ("admin_content.seo.tabs.meta", "/admin/seo/meta".to_owned()),
            (
                "admin_content.seo.tabs.crawlers",
                "/admin/seo/crawlers".to_owned(),
            ),
            ("admin_content.seo.tabs.llms", "/admin/seo/llms".to_owned()),
            (
                "admin_content.seo.tabs.indexnow",
                "/admin/seo/indexnow".to_owned(),
            ),
            (
                "admin_content.seo.tabs.redirects",
                "/admin/seo/redirects".to_owned(),
            ),
            (
                "admin_content.seo.tabs.sitemap",
                "/admin/seo/sitemap".to_owned(),
            ),
        ],
    );
    (tabs, ctx.url(path))
}

/// Score tone for `.score--*` / trend bars.
pub fn tone(score: i64) -> &'static str {
    if score >= 80 {
        "good"
    } else if score >= 50 {
        "mid"
    } else {
        "bad"
    }
}

// ---------------------------------------------------------------------------
// In-process storefront requests
// ---------------------------------------------------------------------------

/// A storefront response rendered in-process.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

/// `Host` header (host[:port]) of an origin.
pub fn host_of(origin: &str) -> Option<String> {
    let u = url::Url::parse(origin).ok()?;
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    })
}

/// GETs `path` from the storefront of the tenant at `origin` through the
/// application router (no network round trip; same middleware stack).
pub async fn fetch_local(app: &Router, origin: &str, path: &str) -> Option<Fetched> {
    let host = host_of(origin)?;
    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(path)
        .header(http::header::HOST, host)
        .header(
            http::header::ACCEPT,
            "text/html,text/plain,application/xml;q=0.9",
        )
        .header(http::header::USER_AGENT, "HostingOS-SEO-Audit/1.0")
        .body(Body::empty())
        .ok()?;
    let res = app.clone().oneshot(req).await.ok()?;
    let status = res.status().as_u16();
    let content_type = res
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .ok()?;
    Some(Fetched {
        status,
        content_type,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// `<loc>` values of a sitemap / sitemap index.
pub fn sitemap_locs(xml: &str) -> Vec<String> {
    xml.split("<loc>")
        .skip(1)
        .filter_map(|part| part.split_once("</loc>").map(|(loc, _)| loc.trim()))
        .map(|loc| {
            loc.replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// Path + query of an absolute URL on `origin` (`None` for other hosts).
pub fn local_path(origin: &str, url: &str) -> Option<String> {
    let base = url::Url::parse(origin).ok()?;
    let u = url::Url::parse(url).ok()?;
    if u.host_str() != base.host_str() || u.port_or_known_default() != base.port_or_known_default()
    {
        return None;
    }
    Some(match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_owned(),
    })
}

/// Every page URL of the sitemap (child sitemaps of an index followed).
pub async fn sitemap_urls(app: &Router, origin: &str) -> AppResult<Option<(Vec<String>, usize)>> {
    let Some(root) = fetch_local(app, origin, "/sitemap.xml").await else {
        return Ok(None);
    };
    if root.status != 200 {
        return Ok(None);
    }
    if !root.body.contains("<sitemapindex") {
        return Ok(Some((sitemap_locs(&root.body), 1)));
    }
    let children = sitemap_locs(&root.body);
    let mut urls = Vec::new();
    for child in &children {
        if let Some(path) = local_path(origin, child)
            && let Some(f) = fetch_local(app, origin, &path).await
            && f.status == 200
        {
            urls.extend(sitemap_locs(&f.body));
        }
    }
    Ok(Some((urls, children.len())))
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

/// A bar of the score trend.
pub struct TrendBar {
    pub score: i64,
    pub tone: &'static str,
    pub label: String,
    pub height: i64,
}

#[derive(Template)]
#[template(path = "admin/seo/overview.html")]
struct OverviewPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    last: Option<seo_repo::SeoAuditRow>,
    last_score: i64,
    last_tone: &'static str,
    last_pages: i64,
    last_critical: i64,
    last_errors: i64,
    trend: Vec<TrendBar>,
    running: Option<seo_repo::SeoAuditRow>,
    geo_avg: Option<i64>,
    geo_tone: &'static str,
    indexable: i64,
    stats: content_repo::ContentSeoStats,
    meta_products_missing: i64,
    ai_ready: bool,
}
crate::page!(OverviewPage);

async fn overview(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_content.seo.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let (tabs, active) = nav(&ctx, "/admin/seo");
    let pool = &state.pool;
    seo_repo::fail_stale_seo_audits(pool, tenant.id, audit::STALE_MINUTES, "timed out").await?;
    let recent = seo_repo::recent_seo_audits(pool, tenant.id, 30).await?;
    let running = recent
        .iter()
        .find(|a| matches!(a.status, RunStatus::Pending | RunStatus::Running))
        .cloned();
    let completed: Vec<&seo_repo::SeoAuditRow> = recent
        .iter()
        .filter(|a| a.status == RunStatus::Completed)
        .collect();
    let last = completed.first().map(|a| (*a).clone());
    let parsed = last.as_ref().and_then(audit::StoredAudit::from_row);
    let (last_pages, last_critical, last_errors) = parsed
        .as_ref()
        .map(|p| {
            let s = &p.site.summary;
            (
                i64::try_from(s.page_count).unwrap_or(0),
                i64::try_from(s.counts.critical).unwrap_or(0),
                i64::try_from(s.counts.error).unwrap_or(0),
            )
        })
        .unwrap_or_default();
    let geo_avg = parsed.as_ref().and_then(audit::StoredAudit::geo_average);
    let last_score = last
        .as_ref()
        .and_then(|a| a.score)
        .map(i64::from)
        .unwrap_or(0);
    let trend = completed
        .iter()
        .take(10)
        .rev()
        .filter_map(|a| {
            a.score.map(|s| TrendBar {
                score: i64::from(s),
                tone: tone(i64::from(s)),
                label: ctx.date(&a.created_at),
                height: i64::from(s).clamp(4, 100),
            })
        })
        .collect();
    let stats = content_repo::content_seo_stats(pool, tenant.id).await?;
    let indexable =
        i64::try_from(content_repo::sitemap_entries(pool, tenant.id).await?.len()).unwrap_or(0);
    let meta_products_missing = meta::missing_count(&state, &tenant).await?;
    render(&OverviewPage {
        tabs,
        active,
        last_tone: tone(last_score),
        last_score,
        last_pages,
        last_critical,
        last_errors,
        last,
        trend,
        running,
        geo_tone: tone(geo_avg.unwrap_or(0)),
        geo_avg,
        indexable,
        stats,
        meta_products_missing,
        ai_ready: super::ai_studio::provider_ready(&state, &tenant).await,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// Sitemap status
// ---------------------------------------------------------------------------

/// URLs of the sitemap by section.
pub struct SitemapGroup {
    pub label: String,
    pub count: usize,
}

#[derive(Template)]
#[template(path = "admin/seo/sitemap.html")]
struct SitemapPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    ok: bool,
    total: usize,
    files: usize,
    groups: Vec<SitemapGroup>,
    sample: Vec<String>,
    robots_ok: bool,
    robots_lists_sitemap: bool,
    llms_ok: bool,
    origin: String,
}
crate::page!(SitemapPage);

fn section_of(path: &str) -> &'static str {
    let unprefixed = ["/en", "/tr"]
        .iter()
        .find_map(|l| {
            path.strip_prefix(l)
                .filter(|r| r.is_empty() || r.starts_with('/'))
        })
        .unwrap_or(path);
    let p = unprefixed.trim_start_matches('/');
    let first = p.split('/').next().unwrap_or_default();
    match first {
        "" => "home",
        "blog" => "blog",
        "kb" => "kb",
        "announcements" => "announcements",
        "domains" | "whois" | "contact" => "static",
        _ if p.contains('/') => "products",
        _ => "pages",
    }
}

async fn sitemap_status(
    State(state): State<AppState>,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_content.seo.sitemap.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/sitemap");
    let app = crate::app(state.clone());
    let origin = ctx.origin.clone();
    let found = sitemap_urls(&app, &origin).await?;
    let (ok, urls, files) = match found {
        Some((urls, files)) => (true, urls, files),
        None => (false, Vec::new(), 0),
    };
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for u in &urls {
        let path = local_path(&origin, u).unwrap_or_default();
        let s = section_of(&path);
        match counts.iter_mut().find(|(k, _)| *k == s) {
            Some((_, n)) => *n += 1,
            None => counts.push((s, 1)),
        }
    }
    let groups = counts
        .into_iter()
        .map(|(k, count)| SitemapGroup {
            label: ctx.t(&format!("admin_content.seo.sitemap.sections.{k}")),
            count,
        })
        .collect();
    let robots = fetch_local(&app, &origin, "/robots.txt").await;
    let llms = fetch_local(&app, &origin, "/llms.txt").await;
    render(&SitemapPage {
        tabs,
        active,
        ok,
        total: urls.len(),
        files,
        groups,
        sample: urls.iter().take(15).cloned().collect(),
        robots_ok: robots.as_ref().is_some_and(|r| r.status == 200),
        robots_lists_sitemap: robots.as_ref().is_some_and(|r| r.body.contains("Sitemap:")),
        llms_ok: llms.as_ref().is_some_and(|r| r.status == 200),
        origin,
        ctx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locs() {
        let xml = "<urlset><url><loc>https://a.test/x?a=1&amp;b=2</loc></url><url><loc> https://a.test/y </loc></url></urlset>";
        assert_eq!(
            sitemap_locs(xml),
            vec!["https://a.test/x?a=1&b=2", "https://a.test/y"]
        );
    }

    #[test]
    fn paths() {
        assert_eq!(
            local_path("http://a.test:8080", "http://a.test:8080/blog/x?p=1").as_deref(),
            Some("/blog/x?p=1")
        );
        assert_eq!(local_path("http://a.test", "http://b.test/x"), None);
        assert_eq!(
            host_of("http://a.test:8080").as_deref(),
            Some("a.test:8080")
        );
        assert_eq!(section_of("/en/blog/x"), "blog");
        assert_eq!(section_of("/vps/small"), "products");
        assert_eq!(section_of("/"), "home");
    }
}
