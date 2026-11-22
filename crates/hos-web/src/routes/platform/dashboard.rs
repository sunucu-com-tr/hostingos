//! `GET /platform` — operator overview.

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use hos_db::Page;
use hos_db::repo::platform::{self, TenantFilter, TenantRow};
use hos_db::repo::saas::{self, JobHealth, PlatformMrr, SignupRow, TenantVolume};
use hos_db::repo::stats::{self, PlatformOverview};

use super::menu;
use crate::error::AppResult;
use crate::extract::PlatformAdmin;
use crate::state::AppState;
use crate::view::{PageCtx, render};

#[derive(Template)]
#[template(path = "platform/dashboard.html")]
struct DashboardPage {
    ctx: PageCtx,
    overview: PlatformOverview,
    cancelled: i64,
    mrr: Vec<PlatformMrr>,
    volumes: Vec<TenantVolume>,
    jobs: JobHealth,
    queue_ok: bool,
    signups: Vec<SignupRow>,
    tenants: Vec<TenantRow>,
    admin_name: String,
    banner: super::updates::UpdateBanner,
}
crate::page!(DashboardPage);

/// `GET /platform`.
pub async fn dashboard(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let overview = stats::platform_overview(&state.pool).await?;
    let cancelled = (overview.tenants
        - overview.active_tenants
        - overview.trial_tenants
        - overview.suspended_tenants)
        .max(0);
    let mrr = saas::platform_mrr(&state.pool).await?;
    let volumes = saas::tenant_volumes(&state.pool, 10).await?;
    let jobs = saas::job_health(&state.pool).await?;
    let queue_ok = jobs.stale == 0
        && jobs
            .oldest_due_at
            .is_none_or(|t| (chrono::Utc::now() - t).num_minutes() < 10);
    let signups = saas::recent_signups(&state.pool, 5).await?;
    let tenants = platform::list_tenants(&state.pool, &TenantFilter::default(), Page::number(1, 8))
        .await?
        .items;
    let title = ctx.t("platform.dashboard.title");
    ctx.set_title(title);
    ctx.panel = Some(menu::chrome(&ctx, Vec::new()));
    let banner = super::updates::banner(&state, &admin, &ctx).await;
    render(&DashboardPage {
        ctx,
        overview,
        cancelled,
        mrr,
        volumes,
        jobs,
        queue_ok,
        signups,
        tenants,
        admin_name: admin.admin.name.clone(),
        banner,
    })
}
