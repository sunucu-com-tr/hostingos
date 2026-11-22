//! Site audit (`/admin/seo/audits*`) and GEO analysis (`/admin/seo/geo*`).
//!
//! An audit renders the storefront pages listed in the sitemap through the
//! application router in-process (correct `Host`, same middleware, no
//! network), runs `hos_seo::audit::audit_site` and `hos_seo::geo::analyze`
//! on them and stores the result in `seo_audits.issues` as
//! `{"site": SiteReport, "geo": [..], "found", "capped", "skipped"}`.
//! It runs in a background tokio task (not the job queue); the row carries
//! the status and `pages_done / pages_total` for the progress bar. Runs that
//! stop reporting (server restart) are failed after [`STALE_MINUTES`].

use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_core::domain::Locale;
use hos_core::ids::TenantId;
use hos_db::enums::{AiPurpose, RunStatus};
use hos_db::ids::SeoAuditId;
use hos_db::repo::content as content_repo;
use hos_db::repo::seo::{self as seo_repo, AiGenerationRow, SeoAuditRow};
use hos_seo::audit::{
    AuditOptions, AuditPage, Finding, PageStats, Severity, SiteReport, audit_site,
};
use hos_seo::geo::{CheckStatus, GeoInput, GeoReport};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::super::ai_studio::{self, StartError};
use super::super::common::{self, Audit, Ui as _};
use super::super::content::Kind;
use super::{fetch_local, local_path, nav, sitemap_urls, tone};
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, escape_html, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/seo/audits", get(list).post(start))
        .route("/admin/seo/audits/{id}", get(report))
        .route("/admin/seo/audits/{id}/progress", get(progress))
        .route("/admin/seo/audits/{id}/plan", post(plan))
        .route("/admin/seo/geo", get(geo_list))
        .route("/admin/seo/geo/page", get(geo_page))
}

/// Pages rendered per audit at most.
pub const MAX_PAGES: usize = 60;
/// Minutes after which a pending / running audit counts as dead.
pub const STALE_MINUTES: i32 = 20;

// ---------------------------------------------------------------------------
// Stored result
// ---------------------------------------------------------------------------

/// GEO analysis of one audited page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoPage {
    pub url: String,
    pub locale: String,
    pub report: GeoReport,
}

/// A sitemap URL that could not be audited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedPage {
    pub url: String,
    pub status: u16,
}

/// `seo_audits.issues` of a site audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAudit {
    pub site: SiteReport,
    #[serde(default)]
    pub geo: Vec<GeoPage>,
    #[serde(default)]
    pub found: usize,
    #[serde(default)]
    pub capped: bool,
    #[serde(default)]
    pub skipped: Vec<SkippedPage>,
}

impl StoredAudit {
    /// Parses a completed audit row.
    pub fn from_row(a: &SeoAuditRow) -> Option<Self> {
        serde_json::from_value(a.issues.clone()).ok()
    }

    /// Mean GEO score.
    pub fn geo_average(&self) -> Option<i64> {
        if self.geo.is_empty() {
            return None;
        }
        let sum: i64 = self.geo.iter().map(|g| i64::from(g.report.score)).sum();
        Some(sum / i64::try_from(self.geo.len()).unwrap_or(1))
    }

    /// `(url, finding id)` keys of every finding (`""` url = site-wide).
    fn keys(&self) -> BTreeMap<(String, String), Finding> {
        let mut out = BTreeMap::new();
        for p in &self.site.pages {
            for f in &p.findings {
                out.insert((p.url.clone(), f.id.clone()), f.clone());
            }
        }
        for f in &self.site.site_findings {
            out.insert((String::new(), f.id.clone()), f.clone());
        }
        out
    }
}

/// Audit key (`seo_audits.url`) of a storefront.
pub fn site_key(origin: &str) -> String {
    format!("{origin}/")
}

fn locale_of(path: &str, default: Locale) -> Locale {
    for l in Locale::ALL {
        let prefix = format!("/{}", l.as_str());
        if *l != default
            && path
                .strip_prefix(&prefix)
                .is_some_and(|r| r.is_empty() || r.starts_with('/'))
        {
            return *l;
        }
    }
    default
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

struct Run {
    state: AppState,
    tenant_id: TenantId,
    audit_id: SeoAuditId,
    origin: String,
    brand: String,
    default_locale: Locale,
}

/// Starts an audit of the storefront at `origin` in the background.
/// `Err(row)` when one is already running.
pub async fn start_audit(
    state: &AppState,
    tenant: &Tenant,
    origin: &str,
) -> AppResult<Result<SeoAuditRow, SeoAuditRow>> {
    let pool = &state.pool;
    seo_repo::fail_stale_seo_audits(pool, tenant.id, STALE_MINUTES, "timed out").await?;
    if let Some(running) = seo_repo::recent_seo_audits(pool, tenant.id, 10)
        .await?
        .into_iter()
        .find(|a| matches!(a.status, RunStatus::Pending | RunStatus::Running))
    {
        return Ok(Err(running));
    }
    let row = seo_repo::create_seo_audit(pool, tenant.id, &site_key(origin), None, None).await?;
    let run = Run {
        state: state.clone(),
        tenant_id: tenant.id,
        audit_id: row.id,
        origin: origin.to_owned(),
        brand: tenant.display_name().to_owned(),
        default_locale: tenant.default_locale,
    };
    tokio::spawn(async move {
        if let Err(e) = execute(&run).await {
            tracing::warn!(error = %e, audit = %run.audit_id, "SEO audit failed");
            let _ = seo_repo::finish_seo_audit(
                &run.state.pool,
                run.tenant_id,
                run.audit_id,
                None,
                &json!({}),
                None,
                Some(&e.to_string()),
            )
            .await;
        }
    });
    Ok(Ok(row))
}

async fn execute(run: &Run) -> AppResult<()> {
    let pool = &run.state.pool;
    let (tid, id) = (run.tenant_id, run.audit_id);
    seo_repo::start_seo_audit(pool, tid, id).await?;
    let app = crate::app(run.state.clone());
    let Some((urls, _)) = sitemap_urls(&app, &run.origin).await? else {
        seo_repo::finish_seo_audit(
            pool,
            tid,
            id,
            None,
            &json!({}),
            None,
            Some("sitemap.xml could not be rendered"),
        )
        .await?;
        return Ok(());
    };
    let mut seen = BTreeSet::new();
    let mut targets: Vec<(String, String)> = urls
        .into_iter()
        .filter_map(|u| local_path(&run.origin, &u).map(|p| (u, p)))
        .filter(|(_, p)| seen.insert(p.clone()))
        .collect();
    let found = targets.len();
    // Default-language pages first so a capped run still covers every section.
    targets.sort_by_key(|(_, p)| locale_of(p, run.default_locale) != run.default_locale);
    let capped = targets.len() > MAX_PAGES;
    targets.truncate(MAX_PAGES);
    let total = i32::try_from(targets.len()).unwrap_or(i32::MAX);
    seo_repo::set_seo_audit_progress(pool, tid, id, 0, total).await?;

    let mut pages = Vec::new();
    let mut geo = Vec::new();
    let mut skipped = Vec::new();
    for (i, (url, path)) in targets.iter().enumerate() {
        match fetch_local(&app, &run.origin, path).await {
            Some(f) if f.status == 200 && f.content_type.starts_with("text/html") => {
                let locale = locale_of(path, run.default_locale);
                let mut input = GeoInput::new(&f.body, locale);
                input.brand = Some(&run.brand);
                input.now = Utc::now();
                geo.push(GeoPage {
                    url: url.clone(),
                    locale: locale.as_str().to_owned(),
                    report: hos_seo::geo::analyze(&input),
                });
                pages.push(AuditPage {
                    url: url.clone(),
                    html: f.body,
                    keywords: Vec::new(),
                });
            }
            Some(f) => skipped.push(SkippedPage {
                url: url.clone(),
                status: f.status,
            }),
            None => skipped.push(SkippedPage {
                url: url.clone(),
                status: 0,
            }),
        }
        let done = i32::try_from(i + 1).unwrap_or(i32::MAX);
        seo_repo::set_seo_audit_progress(pool, tid, id, done, total).await?;
    }
    let opts = AuditOptions {
        locale: run.default_locale,
        ..AuditOptions::default()
    };
    let site = tokio::task::spawn_blocking(move || audit_site(&pages, &opts))
        .await
        .map_err(AppError::internal)?;
    let score = i16::from(site.summary.score);
    let stored = StoredAudit {
        site,
        geo,
        found,
        capped,
        skipped,
    };
    let issues = serde_json::to_value(&stored).map_err(AppError::internal)?;
    seo_repo::finish_seo_audit(pool, tid, id, Some(score), &issues, None, None).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// List / start / progress
// ---------------------------------------------------------------------------

/// An audit in the list.
pub struct AuditView {
    pub a: SeoAuditRow,
    pub pages: usize,
    pub tone: &'static str,
}

#[derive(Template)]
#[template(path = "admin/seo/audits.html")]
struct AuditsPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<AuditView>,
    running: bool,
    max_pages: usize,
}
crate::page!(AuditsPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_content.seo.audits.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/audits");
    seo_repo::fail_stale_seo_audits(&state.pool, tenant.id, STALE_MINUTES, "timed out").await?;
    let rows: Vec<AuditView> = seo_repo::recent_seo_audits(&state.pool, tenant.id, 30)
        .await?
        .into_iter()
        .map(|a| AuditView {
            pages: StoredAudit::from_row(&a).map_or(0, |s| s.site.summary.page_count),
            tone: tone(a.score.map_or(0, i64::from)),
            a,
        })
        .collect();
    let running = rows
        .iter()
        .any(|r| matches!(r.a.status, RunStatus::Pending | RunStatus::Running));
    render(&AuditsPage {
        tabs,
        active,
        rows,
        running,
        max_pages: MAX_PAGES,
        ctx,
    })
}

async fn start(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
) -> AppResult<Response> {
    match start_audit(&state, &tenant, &ctx.origin).await? {
        Ok(row) => {
            au.log(
                &state,
                "seo.audit_start",
                Some(("seo_audit", row.id.into())),
                json!({"url": row.url}),
            )
            .await?;
            Ok(common::done(
                &ctx,
                &format!("/admin/seo/audits/{}", row.id),
                "admin_content.seo.audits.started",
            ))
        }
        Err(running) => Ok(common::failed(
            &ctx,
            &format!("/admin/seo/audits/{}", running.id),
            ctx.t("admin_content.seo.audits.already_running"),
        )),
    }
}

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<SeoAuditRow> {
    seo_repo::get_seo_audit(&state.pool, tenant.id, SeoAuditId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

fn percent(a: &SeoAuditRow) -> i64 {
    if a.pages_total <= 0 {
        return 0;
    }
    (i64::from(a.pages_done) * 100 / i64::from(a.pages_total)).clamp(0, 100)
}

/// The polling progress box of a running audit.
fn progress_box(ctx: &PageCtx, a: &SeoAuditRow) -> String {
    let pct = percent(a);
    let text = if a.pages_total > 0 {
        ctx.tv2(
            "admin_content.seo.audits.progress",
            "done",
            a.pages_done,
            "total",
            a.pages_total,
        )
    } else {
        ctx.t("admin_content.seo.audits.preparing")
    };
    format!(
        "<div class=\"stack-sm\" role=\"status\" aria-live=\"polite\" hx-get=\"{url}\" hx-trigger=\"every 2s\" hx-swap=\"outerHTML\">\
         <div class=\"ai-pending\"><span class=\"spinner\" aria-hidden=\"true\"></span><span>{text}</span></div>\
         <progress class=\"w-full\" max=\"100\" value=\"{pct}\">{pct}%</progress></div>",
        url = escape_html(&ctx.url(format!("/admin/seo/audits/{}/progress", a.id))),
        text = escape_html(&text),
    )
}

/// `GET /admin/seo/audits/{id}/progress` (htmx): the progress box, or a
/// page refresh once the audit finished.
async fn progress(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    seo_repo::fail_stale_seo_audits(&state.pool, tenant.id, STALE_MINUTES, "timed out").await?;
    let a = load(&state, &tenant, id).await?;
    if matches!(a.status, RunStatus::Pending | RunStatus::Running) {
        return Ok(common::fragment(progress_box(&ctx, &a)));
    }
    let mut res = common::fragment(String::new());
    res.headers_mut()
        .insert("hx-refresh", http::HeaderValue::from_static("true"));
    Ok(res)
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// A finding prepared for display.
pub struct FindingView {
    pub id: String,
    pub severity: &'static str,
    pub tone: &'static str,
    pub message: String,
    pub hint: String,
    pub urls: Vec<String>,
    pub url: String,
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Notice => "notice",
    }
}

fn severity_tone(s: Severity) -> &'static str {
    match s {
        Severity::Critical | Severity::Error => "danger",
        Severity::Warning => "warning",
        Severity::Notice => "info",
    }
}

fn finding_view(ctx: &PageCtx, url: &str, f: &Finding) -> FindingView {
    FindingView {
        id: f.id.clone(),
        severity: severity_str(f.severity),
        tone: severity_tone(f.severity),
        message: f.message(ctx.locale),
        hint: f.hint(ctx.locale).unwrap_or_default(),
        urls: f.urls.iter().take(20).cloned().collect(),
        url: url.to_owned(),
    }
}

/// One audited page.
pub struct PageView {
    pub url: String,
    pub score: i64,
    pub tone: &'static str,
    pub findings: Vec<FindingView>,
    pub total_findings: usize,
    pub detail_href: String,
}

/// Selected page details.
pub struct PageDetail {
    pub url: String,
    pub score: i64,
    pub tone: &'static str,
    pub stats: PageStats,
    pub findings: Vec<FindingView>,
    pub geo_score: Option<i64>,
    pub geo_href: String,
}

/// Comparison with the previous run.
pub struct Comparison {
    pub previous_id: SeoAuditId,
    pub previous_score: i64,
    pub delta: i64,
    pub fixed: Vec<FindingView>,
    pub added: Vec<FindingView>,
    pub fixed_total: usize,
    pub added_total: usize,
}

/// A frequent issue.
pub struct TopIssue {
    pub severity: &'static str,
    pub tone: &'static str,
    pub message: String,
    pub count: usize,
}

#[derive(Debug, Default, Deserialize)]
struct ReportQuery {
    #[serde(default)]
    severity: String,
    #[serde(default)]
    url: String,
}

#[derive(Template)]
#[template(path = "admin/seo/audit.html")]
struct ReportPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    a: SeoAuditRow,
    running: bool,
    progress_html: String,
    has_report: bool,
    score: i64,
    tone: &'static str,
    counts: [usize; 4],
    page_count: usize,
    found: usize,
    capped: bool,
    skipped: Vec<SkippedPage>,
    severity: String,
    severities: Vec<(String, String)>,
    site_findings: Vec<FindingView>,
    pages: Vec<PageView>,
    top: Vec<TopIssue>,
    detail: Option<PageDetail>,
    comparison: Option<Comparison>,
    plan: Option<AiGenerationRow>,
    ai_ready: bool,
    base: String,
}
crate::page!(ReportPage);

impl ReportPage {
    fn sev_label(&self, s: &str) -> String {
        self.t(&format!("admin_content.seo.severity.{s}"))
    }

    fn count_label(&self, i: usize) -> String {
        let key = ["critical", "error", "warning", "notice"][i.min(3)];
        format!("{}: {}", self.sev_label(key), self.counts[i.min(3)])
    }
}

#[allow(clippy::too_many_lines)]
async fn report(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    Path(id): Path<Uuid>,
    Query(q): Query<ReportQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    seo_repo::fail_stale_seo_audits(&state.pool, tenant.id, STALE_MINUTES, "timed out").await?;
    let a = load(&state, &tenant, id).await?;
    let title = ctx.tv(
        "admin_content.seo.audits.report_title",
        "date",
        ctx.datetime(&a.created_at),
    );
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[
            ("admin_content.seo.title", "/admin/seo"),
            ("admin_content.seo.audits.title", "/admin/seo/audits"),
        ],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/audits");
    let running = matches!(a.status, RunStatus::Pending | RunStatus::Running);
    let progress_html = if running {
        progress_box(&ctx, &a)
    } else {
        String::new()
    };
    let stored = (a.status == RunStatus::Completed)
        .then(|| StoredAudit::from_row(&a))
        .flatten();
    let base = format!("/admin/seo/audits/{}", a.id);
    let severity: String = match q.severity.as_str() {
        "critical" | "error" | "warning" | "notice" => q.severity.clone(),
        _ => String::new(),
    };
    let keep = |f: &Finding| severity.is_empty() || severity_str(f.severity) == severity;
    let severities = ["critical", "error", "warning", "notice"]
        .iter()
        .map(|s| {
            (
                (*s).to_owned(),
                ctx.t(&format!("admin_content.seo.severity.{s}")),
            )
        })
        .collect();

    let (mut site_findings, mut pages, mut top, mut detail, mut comparison) =
        (Vec::new(), Vec::new(), Vec::new(), None, None);
    let (mut counts, mut page_count, mut found, mut capped, mut skipped) =
        ([0usize; 4], 0usize, 0usize, false, Vec::new());
    if let Some(s) = &stored {
        let sum = &s.site.summary;
        counts = [
            sum.counts.critical,
            sum.counts.error,
            sum.counts.warning,
            sum.counts.notice,
        ];
        page_count = sum.page_count;
        found = s.found;
        capped = s.capped;
        skipped = s.skipped.clone();
        site_findings = s
            .site
            .site_findings
            .iter()
            .filter(|f| keep(f))
            .map(|f| finding_view(&ctx, "", f))
            .collect();
        let mut pv: Vec<PageView> = s
            .site
            .pages
            .iter()
            .map(|p| PageView {
                url: p.url.clone(),
                score: i64::from(p.score),
                tone: tone(i64::from(p.score)),
                findings: p
                    .findings
                    .iter()
                    .filter(|f| keep(f))
                    .map(|f| finding_view(&ctx, &p.url, f))
                    .collect(),
                total_findings: p.findings.len(),
                detail_href: format!(
                    "{base}?{}",
                    common::query_string(
                        &[("url", p.url.clone()), ("severity", severity.clone()),]
                    )
                ),
            })
            .filter(|p| severity.is_empty() || !p.findings.is_empty())
            .collect();
        pv.sort_by_key(|p| (p.score, p.url.clone()));
        pages = pv;
        top = sum
            .top_issues
            .iter()
            .filter_map(|i| {
                let sample = s
                    .site
                    .pages
                    .iter()
                    .flat_map(|p| p.findings.iter())
                    .chain(s.site.site_findings.iter())
                    .find(|f| f.id == i.id)?;
                Some(TopIssue {
                    severity: severity_str(i.severity),
                    tone: severity_tone(i.severity),
                    message: sample.message(ctx.locale),
                    count: i.count,
                })
            })
            .collect();
        if !q.url.is_empty()
            && let Some(p) = s.site.pages.iter().find(|p| p.url == q.url)
        {
            let geo = s.geo.iter().find(|g| g.url == p.url);
            detail = Some(PageDetail {
                url: p.url.clone(),
                score: i64::from(p.score),
                tone: tone(i64::from(p.score)),
                stats: p.stats.clone(),
                findings: p
                    .findings
                    .iter()
                    .map(|f| finding_view(&ctx, &p.url, f))
                    .collect(),
                geo_score: geo.map(|g| i64::from(g.report.score)),
                geo_href: format!(
                    "/admin/seo/geo/page?{}",
                    common::query_string(&[("audit", a.id.to_string()), ("url", p.url.clone()),])
                ),
            });
        }
        if let Some(prev) = seo_repo::previous_seo_audit(&state.pool, tenant.id, &a).await?
            && let Some(ps) = StoredAudit::from_row(&prev)
        {
            let now_keys = s.keys();
            let prev_keys = ps.keys();
            let fixed: Vec<FindingView> = prev_keys
                .iter()
                .filter(|(k, _)| !now_keys.contains_key(*k))
                .map(|((u, _), f)| finding_view(&ctx, u, f))
                .collect();
            let added: Vec<FindingView> = now_keys
                .iter()
                .filter(|(k, _)| !prev_keys.contains_key(*k))
                .map(|((u, _), f)| finding_view(&ctx, u, f))
                .collect();
            let previous_score = prev.score.map_or(0, i64::from);
            comparison = Some(Comparison {
                previous_id: prev.id,
                previous_score,
                delta: a.score.map_or(0, i64::from) - previous_score,
                fixed_total: fixed.len(),
                added_total: added.len(),
                fixed: fixed.into_iter().take(25).collect(),
                added: added.into_iter().take(25).collect(),
            });
        }
    }
    let plan =
        seo_repo::latest_ai_generation_for(&state.pool, tenant.id, "seo_audit", a.id.into(), None)
            .await?;
    let score = a.score.map_or(0, i64::from);
    render(&ReportPage {
        tabs,
        active,
        running,
        progress_html,
        has_report: stored.is_some(),
        score,
        tone: tone(score),
        counts,
        page_count,
        found,
        capped,
        skipped,
        severity,
        severities,
        site_findings,
        pages,
        top,
        detail,
        comparison,
        plan,
        ai_ready: ai_studio::provider_ready(&state, &tenant).await,
        base,
        a,
        ctx,
    })
}

/// `POST /admin/seo/audits/{id}/plan` — AI action plan of a completed audit.
async fn plan(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let a = load(&state, &tenant, id).await?;
    let back = format!("/admin/seo/audits/{}", a.id);
    if a.status != RunStatus::Completed {
        return Ok(common::failed(
            &ctx,
            &back,
            ctx.t("admin_content.seo.audits.not_completed"),
        ));
    }
    let started = ai_studio::start_generation(
        &state,
        &tenant,
        staff.user_id,
        AiPurpose::Other,
        Some(("seo_audit", a.id.into())),
        Some(ctx.locale),
        "summarize_audit",
        json!({"task": "summarize_audit", "audit_id": a.id}),
    )
    .await?;
    match started {
        Ok(gid) => {
            au.log(
                &state,
                "seo.audit_plan",
                Some(("seo_audit", a.id.into())),
                json!({"generation": gid}),
            )
            .await?;
            Ok(common::done(
                &ctx,
                &format!("/admin/ai/generations/{gid}"),
                "admin_content.ai.queued",
            ))
        }
        Err(e) => Ok(common::failed(
            &ctx,
            &back,
            ctx.t(match e {
                StartError::NotConfigured => "admin_content.ai.not_configured",
                StartError::RateLimited => "admin_content.ai.rate_limited",
            }),
        )),
    }
}

// ---------------------------------------------------------------------------
// GEO
// ---------------------------------------------------------------------------

/// A page in the GEO list.
pub struct GeoRow {
    pub url: String,
    pub score: i64,
    pub tone: &'static str,
    pub failed: usize,
    pub top: String,
    pub href: String,
}

#[derive(Template)]
#[template(path = "admin/seo/geo.html")]
struct GeoListPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    audit: Option<SeoAuditRow>,
    average: i64,
    avg_tone: &'static str,
    rows: Vec<GeoRow>,
    checks: Vec<(String, usize)>,
}
crate::page!(GeoListPage);

/// Latest completed audit with GEO data.
async fn latest_with_geo(
    state: &AppState,
    tenant: &Tenant,
) -> AppResult<Option<(SeoAuditRow, StoredAudit)>> {
    for a in seo_repo::recent_seo_audits(&state.pool, tenant.id, 30).await? {
        if a.status == RunStatus::Completed
            && let Some(s) = StoredAudit::from_row(&a)
            && !s.geo.is_empty()
        {
            return Ok(Some((a, s)));
        }
    }
    Ok(None)
}

fn check_title(ctx: &PageCtx, id: &str) -> String {
    let key = format!("admin_content.seo.geo_checks.{id}");
    if ctx.cat.has(&key) {
        ctx.t(&key)
    } else {
        id.replace('_', " ")
    }
}

async fn geo_list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_content.seo.geo.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/geo");
    let found = latest_with_geo(&state, &tenant).await?;
    let (audit, average, rows, checks) = match found {
        Some((a, s)) => {
            let mut rows: Vec<GeoRow> = s
                .geo
                .iter()
                .map(|g| GeoRow {
                    url: g.url.clone(),
                    score: i64::from(g.report.score),
                    tone: tone(i64::from(g.report.score)),
                    failed: g
                        .report
                        .checks
                        .iter()
                        .filter(|c| c.status == CheckStatus::Fail)
                        .count(),
                    top: g
                        .report
                        .improvements()
                        .first()
                        .map(|c| check_title(&ctx, &c.id))
                        .unwrap_or_default(),
                    href: format!(
                        "/admin/seo/geo/page?{}",
                        common::query_string(&[
                            ("audit", a.id.to_string()),
                            ("url", g.url.clone()),
                        ])
                    ),
                })
                .collect();
            rows.sort_by_key(|r| (r.score, r.url.clone()));
            // Most frequently failing checks across the site.
            let mut fails: BTreeMap<String, usize> = BTreeMap::new();
            for g in &s.geo {
                for c in &g.report.checks {
                    if matches!(c.status, CheckStatus::Fail | CheckStatus::Warn) {
                        *fails.entry(c.id.clone()).or_default() += 1;
                    }
                }
            }
            let mut checks: Vec<(String, usize)> = fails
                .into_iter()
                .map(|(id, n)| (check_title(&ctx, &id), n))
                .collect();
            checks.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            checks.truncate(6);
            (Some(a), s.geo_average().unwrap_or(0), rows, checks)
        }
        None => (None, 0, Vec::new(), Vec::new()),
    };
    render(&GeoListPage {
        tabs,
        active,
        audit,
        average,
        avg_tone: tone(average),
        rows,
        checks,
        ctx,
    })
}

/// A GEO check prepared for display.
pub struct CheckView {
    pub title: String,
    pub status: &'static str,
    pub cls: &'static str,
    pub icon: &'static str,
    pub message: String,
    pub hint: String,
    pub weight: u8,
}

#[derive(Debug, Default, Deserialize)]
struct GeoQuery {
    #[serde(default)]
    audit: String,
    #[serde(default)]
    url: String,
}

#[derive(Template)]
#[template(path = "admin/seo/geo_page.html")]
struct GeoDetailPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    audit_id: SeoAuditId,
    page_url: String,
    score: i64,
    tone: &'static str,
    checks: Vec<CheckView>,
    rewrite_href: String,
    ai_ready: bool,
}
crate::page!(GeoDetailPage);

impl GeoDetailPage {
    fn url_enc(&self) -> String {
        url::form_urlencoded::byte_serialize(self.page_url.as_bytes()).collect()
    }
}

/// `type:id` of the content row behind a storefront URL.
pub async fn entity_for_url(
    state: &AppState,
    tenant: &Tenant,
    origin: &str,
    url: &str,
) -> AppResult<Option<String>> {
    let Some(path) = local_path(origin, url) else {
        return Ok(None);
    };
    let path = path.split('?').next().unwrap_or_default().to_owned();
    let locale = locale_of(&path, tenant.default_locale);
    let unprefixed = if locale == tenant.default_locale {
        path.clone()
    } else {
        path[1 + locale.as_str().len()..].to_owned()
    };
    let Some((kind, slug)) = Kind::from_public_path(&unprefixed) else {
        return Ok(None);
    };
    Ok(
        content_repo::content_by_slug(&state.pool, tenant.id, kind.table(), locale, slug, None)
            .await?
            .map(|id| format!("{}:{id}", kind.entity())),
    )
}

async fn geo_page(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    Query(q): Query<GeoQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let audit_id = Uuid::parse_str(&q.audit).map_err(|_| AppError::NotFound)?;
    let a = load(&state, &tenant, audit_id).await?;
    let s = StoredAudit::from_row(&a).ok_or(AppError::NotFound)?;
    let g = s
        .geo
        .iter()
        .find(|g| g.url == q.url)
        .ok_or(AppError::NotFound)?;
    let title = ctx.t("admin_content.seo.geo.page_title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[
            ("admin_content.seo.title", "/admin/seo"),
            ("admin_content.seo.geo.title", "/admin/seo/geo"),
        ],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/geo");
    let mut checks: Vec<CheckView> = g
        .report
        .checks
        .iter()
        .map(|c| {
            let (status, cls, icon) = match c.status {
                CheckStatus::Pass => ("pass", "check-pass", "check-circle"),
                CheckStatus::Warn => ("warn", "check-warn", "alert-triangle"),
                CheckStatus::Fail => ("fail", "check-fail", "x-circle"),
                CheckStatus::Skipped => ("skipped", "muted", "minus"),
            };
            CheckView {
                title: check_title(&ctx, &c.id),
                status,
                cls,
                icon,
                message: c.message(ctx.locale),
                hint: c.hint(ctx.locale).unwrap_or_default(),
                weight: c.weight,
            }
        })
        .collect();
    let order = |s: &str| match s {
        "fail" => 0,
        "warn" => 1,
        "pass" => 2,
        _ => 3,
    };
    checks.sort_by_key(|c| (order(c.status), std::cmp::Reverse(c.weight)));
    let ai_ready = ai_studio::provider_ready(&state, &tenant).await;
    // Shown without a provider too: the studio form then explains how to
    // configure one.
    let rewrite_href = entity_for_url(&state, &tenant, &ctx.origin, &g.url)
        .await?
        .map(|e| {
            format!(
                "/admin/ai/new?{}",
                common::query_string(&[("task", "geo_rewrite".to_owned()), ("entity", e)])
            )
        })
        .unwrap_or_default();
    render(&GeoDetailPage {
        tabs,
        active,
        audit_id: a.id,
        page_url: g.url.clone(),
        score: i64::from(g.report.score),
        tone: tone(i64::from(g.report.score)),
        checks,
        rewrite_href,
        ai_ready,
        ctx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales_from_paths() {
        assert_eq!(locale_of("/en/blog/x", Locale::Tr), Locale::En);
        assert_eq!(locale_of("/english", Locale::Tr), Locale::Tr);
        assert_eq!(locale_of("/en", Locale::Tr), Locale::En);
        assert_eq!(locale_of("/blog/x", Locale::Tr), Locale::Tr);
    }
}
