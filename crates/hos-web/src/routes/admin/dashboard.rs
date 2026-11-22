//! `/admin` — the dashboard: money figures (billing/report staff only), work
//! queues, 12-month revenue chart, failed jobs, recent activity and system
//! health (job queue lag, integration status). The KPI cards load from
//! `/admin/dashboard/kpis?range=7d|30d|90d|12m` (htmx, skeleton first):
//! trend sparklines and the change against the previous period of the same
//! length ([`stats::kpi_trends`]).

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use hos_auth::rbac::Permission;
use hos_db::Page;
use hos_db::repo::admin::{self as arepo, QueueHealth, WorkCounts};
use hos_db::repo::audit::{self, AuditFilter, AuditLogRow};
use hos_db::repo::identity;
use hos_db::repo::integrations::{self, IntegrationRow};
use hos_db::repo::jobs::JobRow;
use hos_db::repo::stats::{self, DashboardStats};
use std::collections::{HashMap, HashSet};

use super::chart::{self, Point};
use super::common::{self, Ui as _};
use crate::error::AppResult;
use crate::extract::CurrentStaff;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// One activity line.
pub struct Activity {
    pub at: chrono::DateTime<chrono::Utc>,
    pub actor: String,
    pub action: String,
    pub link: String,
}

#[derive(Template)]
#[template(path = "admin/dashboard.html")]
struct DashboardPage {
    ctx: PageCtx,
    stats: Option<DashboardStats>,
    chart_svg: String,
    revenue: Vec<Point>,
    first_name: String,
    work: WorkCounts,
    queue: QueueHealth,
    failed: Vec<JobRow>,
    activity: Vec<Activity>,
    integrations: Vec<IntegrationRow>,
    /// KPI range (`7d|30d|90d|12m`).
    range: &'static str,
}
crate::page!(DashboardPage);

/// Link of an audited entity (when it has an admin page).
pub fn entity_link(entity_type: Option<&str>, id: Option<uuid::Uuid>) -> String {
    let Some(id) = id else { return String::new() };
    let base = match entity_type.unwrap_or_default() {
        "client" => "/admin/clients/",
        "invoice" => "/admin/invoices/",
        "order" => "/admin/orders/",
        "service" => "/admin/services/",
        "domain" => "/admin/domains/",
        "ticket" => "/admin/support/tickets/",
        "product" => "/admin/catalog/products/",
        "bundle" => "/admin/catalog/bundles/",
        "integration" => "/admin/integrations/",
        "page" => "/admin/content/pages/",
        "blog_post" => "/admin/content/posts/",
        "kb_article" => "/admin/support/kb/",
        "announcement" => "/admin/support/announcements/",
        "quote" => "/admin/quotes/",
        "task" => "/admin/tasks/",
        "affiliate" => "/admin/affiliates/",
        "status_incident" => "/admin/status/incidents/",
        "status_component" => "/admin/status/components/",
        "mail_inbound_message" => "/admin/support/inbound/",
        _ => return String::new(),
    };
    format!("{base}{id}")
}

/// Audit rows → activity lines with staff names.
pub async fn activity_lines(
    state: &AppState,
    ctx: &PageCtx,
    tenant: &Tenant,
    rows: Vec<AuditLogRow>,
) -> AppResult<Vec<Activity>> {
    let names: HashMap<uuid::Uuid, String> = identity::list_staff(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|u| (u.id.into(), u.display_name()))
        .collect();
    // Entries of deleted records stay plain text (their page is a 404).
    let mut wanted: HashMap<&str, Vec<uuid::Uuid>> = HashMap::new();
    for r in &rows {
        if let (Some(t), Some(id)) = (r.entity_type.as_deref(), r.entity_id) {
            wanted.entry(t).or_default().push(id);
        }
    }
    let mut existing: HashSet<(String, uuid::Uuid)> = HashSet::new();
    for (t, ids) in wanted {
        for id in audit::existing_entities(&state.pool, tenant.id, t, &ids).await? {
            existing.insert((t.to_owned(), id));
        }
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            let actor = r
                .actor_id
                .and_then(|a| names.get(&a).cloned())
                .unwrap_or_else(|| ctx.t("admin.activity.system"));
            let key = format!("admin.audit_actions.{}", r.action.replace('.', "_"));
            let action = if ctx.cat.has(&key) {
                ctx.t(&key)
            } else {
                r.action.clone()
            };
            let link = match (&r.entity_type, r.entity_id) {
                (Some(t), Some(id)) if existing.contains(&(t.clone(), id)) => {
                    entity_link(Some(t), Some(id))
                }
                _ => String::new(),
            };
            Activity {
                at: r.created_at,
                actor,
                action,
                link: if link.is_empty() {
                    link
                } else {
                    ctx.url(&link)
                },
            }
        })
        .collect())
}

pub(super) async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let range = stats::KpiRange::parse(q.get("range").map_or("30d", String::as_str)).as_str();
    let title = ctx.t("admin.dashboard.title");
    ctx.set_title(title);
    ctx.panel = Some(common::chrome(&ctx, &staff, &[]));
    let money = staff.can(Permission::REPORTS_VIEW) || staff.can(Permission::BILLING_VIEW);
    let (stats, revenue) = if money {
        let s = stats::dashboard(&state.pool, tenant.id).await?;
        let months = stats::monthly_revenue(&state.pool, tenant.id, 12).await?;
        let pts = months
            .iter()
            .map(|m| Point {
                label: crate::i18n::format_month(m.month, ctx.locale)
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(3)
                    .collect(),
                value: m.amount,
                display: ctx.money_in(&m.amount, &s.currency),
            })
            .collect::<Vec<_>>();
        (Some(s), pts)
    } else {
        (None, Vec::new())
    };
    let chart_svg = if revenue.is_empty() {
        String::new()
    } else {
        chart::columns(
            &revenue,
            &ctx.t("admin.dashboard.revenue_title"),
            ctx.locale == hos_core::domain::Locale::Tr,
        )
    };
    let work = arepo::work_counts(&state.pool, tenant.id).await?;
    let queue = arepo::queue_health(&state.pool, tenant.id).await?;
    let failed = arepo::failed_jobs(&state.pool, tenant.id, 5).await?;
    let audit_rows = audit::list(
        &state.pool,
        tenant.id,
        &AuditFilter::default(),
        Page::first(8),
    )
    .await?
    .items;
    let activity = activity_lines(&state, &ctx, &tenant, audit_rows).await?;
    let integrations = if staff.can(Permission::INTEGRATIONS_MANAGE) {
        integrations::list_integrations(&state.pool, tenant.id, None).await?
    } else {
        Vec::new()
    };
    render(&DashboardPage {
        first_name: staff.user.first_name.clone(),
        ctx,
        stats,
        chart_svg,
        revenue,
        work,
        queue,
        failed,
        activity,
        integrations,
        range,
    })
}

impl DashboardPage {
    fn lag(&self) -> String {
        match self.queue.lag_seconds {
            None | Some(..=0) => self.t("admin.dashboard.queue_ok"),
            Some(s) if s < 120 => self.tv("admin.dashboard.lag_seconds", "n", s),
            Some(s) => self.tv("admin.dashboard.lag_minutes", "n", s / 60),
        }
    }

    fn lag_tone(&self) -> &'static str {
        match self.queue.lag_seconds {
            Some(s) if s > 900 => "danger",
            Some(s) if s > 120 => "warning",
            _ => "success",
        }
    }
}

/// One KPI card with trend.
pub struct KpiCard {
    pub label: String,
    pub icon: &'static str,
    pub value: String,
    /// "+12 %" / "−3 %" / "—".
    pub delta: String,
    /// `up` (good), `down` (bad) or "" (flat / no comparison).
    pub tone: &'static str,
    /// "previous: …".
    pub foot: String,
    pub spark_line: String,
    pub spark_area: String,
    pub href: String,
}

#[derive(Template)]
#[template(path = "admin/_kpis.html")]
struct KpiFragment {
    ctx: PageCtx,
    range: &'static str,
    ranges: Vec<(&'static str, String)>,
    cards: Vec<KpiCard>,
}
crate::page!(KpiFragment);

/// `points` of a sparkline (viewBox 0 0 100 30) and of its area.
fn spark(points: &[f64]) -> (String, String) {
    if points.len() < 2 {
        return (String::new(), String::new());
    }
    let (min, max) = points
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    let span = max - min;
    let last = (points.len() - 1) as f64;
    let line: Vec<String> = points
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 / last * 100.0;
            let y = if span > 0.0 {
                28.0 - (v - min) / span * 26.0
            } else {
                15.0
            };
            format!("{x:.1},{y:.1}")
        })
        .collect();
    let area = format!("0,30 {} 100,30", line.join(" "));
    (line.join(" "), area)
}

/// Change against the previous period; `higher_is_good` picks the tone.
fn delta(
    ctx: &PageCtx,
    current: f64,
    previous: f64,
    higher_is_good: bool,
) -> (String, &'static str) {
    if (current - previous).abs() < 1e-9 {
        return ("±0 %".into(), "");
    }
    if previous.abs() < 1e-9 {
        return (
            ctx.t("admin.dashboard.kpi_new"),
            if higher_is_good { "up" } else { "down" },
        );
    }
    let pct = (current - previous) / previous.abs() * 100.0;
    let up = pct > 0.0;
    let text = format!("{}{:.0} %", if up { "+" } else { "−" }, pct.abs());
    (text, if up == higher_is_good { "up" } else { "down" })
}

/// `GET /admin/dashboard/kpis?range=` (htmx fragment; money staff only).
pub(super) async fn kpis(
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if !(staff.can(Permission::REPORTS_VIEW) || staff.can(Permission::BILLING_VIEW)) {
        return Err(crate::error::AppError::Forbidden("reports".into()));
    }
    let range = stats::KpiRange::parse(q.get("range").map_or("30d", String::as_str));
    let k = stats::kpi_trends(&state.pool, tenant.id, range).await?;
    let currency = stats::dashboard_currency(&state.pool, tenant.id).await?;
    let money = |v: f64| {
        let d = rust_decimal::Decimal::from_f64_retain(v)
            .unwrap_or_default()
            .round_dp(2);
        ctx.money_in(&d, &currency)
    };
    let count = |v: f64| ctx.num(v.round() as i64);
    let prev = |text: String| ctx.tv("admin.dashboard.kpi_previous", "value", text);
    let mut cards = Vec::new();
    let mut card = |label: &str,
                    icon: &'static str,
                    series: &stats::KpiSeries,
                    fmt: &dyn Fn(f64) -> String,
                    good: bool,
                    href: &str| {
        let (d, tone) = delta(&ctx, series.current, series.previous, good);
        let (line, area) = spark(&series.points);
        cards.push(KpiCard {
            label: ctx.t(label),
            icon,
            value: fmt(series.current),
            delta: d,
            tone,
            foot: prev(fmt(series.previous)),
            spark_line: line,
            spark_area: area,
            href: if href.is_empty() {
                String::new()
            } else {
                ctx.url(href)
            },
        });
    };
    card(
        "admin.dashboard.mrr",
        "activity",
        &k.mrr,
        &money,
        true,
        "/admin/reports",
    );
    card(
        "admin.dashboard.kpi_mrr_net",
        "trending-up",
        &k.mrr_net,
        &money,
        true,
        "/admin/reports",
    );
    card(
        "admin.dashboard.active_services",
        "server",
        &k.active_services,
        &count,
        true,
        "/admin/services?status=active",
    );
    card(
        "admin.dashboard.kpi_new_clients",
        "user-plus",
        &k.new_clients,
        &count,
        true,
        "/admin/clients",
    );
    card(
        "admin.dashboard.kpi_cancellations",
        "trending-down",
        &k.cancellations,
        &count,
        false,
        "/admin/services?status=terminated",
    );
    card(
        "admin.dashboard.unpaid",
        "receipt",
        &k.unpaid_total,
        &money,
        false,
        "/admin/invoices?status=unpaid",
    );
    card(
        "admin.dashboard.kpi_overdue",
        "alert-triangle",
        &k.overdue_total,
        &money,
        false,
        "/admin/invoices?status=unpaid",
    );
    card(
        "admin.dashboard.tickets",
        "life-buoy",
        &k.open_tickets,
        &count,
        false,
        "/admin/support/tickets",
    );
    let ranges = ["7d", "30d", "90d", "12m"]
        .into_iter()
        .map(|r| (r, ctx.t(&format!("admin.dashboard.range_{r}"))))
        .collect();
    let res = render(&KpiFragment {
        range: range.as_str(),
        ranges,
        cards,
        ctx,
    })?;
    Ok(crate::routes::client::no_store(res))
}

#[cfg(test)]
mod kpi_tests {
    use super::spark;

    #[test]
    fn sparkline_points() {
        let (line, area) = spark(&[1.0, 3.0, 2.0]);
        assert_eq!(line, "0.0,28.0 50.0,2.0 100.0,15.0");
        assert!(area.starts_with("0,30 ") && area.ends_with(" 100,30"));
        assert_eq!(spark(&[5.0]).0, "");
        assert_eq!(spark(&[2.0, 2.0]).0, "0.0,15.0 100.0,15.0");
    }
}
