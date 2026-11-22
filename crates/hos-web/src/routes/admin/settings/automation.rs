//! Scheduled tasks of the tenant (`/admin/settings/automation`): the cron
//! jobs of `hos_jobs::scheduler::TENANT_TASKS` (daily billing automation,
//! domain / TLD / usage syncs) with schedule, on/off, last and next run, last
//! result and "run now" (enqueues the job immediately).

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_billing::jobs::{self as bjobs, JobKind, JobRequest};
use hos_db::ids::ScheduledTaskId;
use hos_db::repo::admin_settings as srepo;
use hos_db::repo::jobs::{self as jobrepo, ScheduledTaskRow};
use hos_jobs::scheduler::{Scheduler, SchedulerConfig, next_fire};
use serde_json::json;

use super::{page_chrome, tabs};
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::routes::admin::common::{Audit, FormData, Ui as _, done, failed};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render};

const BACK: &str = "/admin/settings/automation";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/automation", get(list))
        .route("/admin/settings/automation/{id}", post(update))
        .route("/admin/settings/automation/{id}/run", post(run_now))
}

#[derive(Template)]
#[template(path = "admin/settings/automation.html")]
struct AutomationPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<ScheduledTaskRow>,
}
crate::page!(AutomationPage);

impl AutomationPage {
    fn task_name(&self, name: &str) -> String {
        self.lbl("admin_settings.automation.tasks", name)
    }

    fn task_help(&self, name: &str) -> String {
        let key = format!("admin_settings.automation.help.{name}");
        if self.cat.has(&key) {
            self.t(&key)
        } else {
            String::new()
        }
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let mut rows = jobrepo::list_scheduled_tasks(&state.pool, Some(tenant.id)).await?;
    if rows.is_empty() {
        // A tenant created after the scheduler's last start has no rows yet.
        Scheduler::new(state.pool.clone(), SchedulerConfig::default())
            .ensure_tenant_tasks(tenant.id)
            .await?;
        rows = jobrepo::list_scheduled_tasks(&state.pool, Some(tenant.id)).await?;
    }
    page_chrome(&mut ctx, &staff, "admin_settings.tabs.automation");
    render(&AutomationPage {
        tabs: tabs(&ctx),
        active: ctx.url(BACK),
        rows,
        ctx,
    })
}

async fn load(state: &AppState, tenant: &Tenant, id: &str) -> AppResult<ScheduledTaskRow> {
    let id: ScheduledTaskId = id.parse().map_err(|_| AppError::NotFound)?;
    srepo::tenant_task(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let task = load(&state, &tenant, &id).await?;
    let cron = f.text("cron", 100);
    let active = f.flag("is_active");
    let fields = cron.split_whitespace().count();
    if !(5..=6).contains(&fields) {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.automation.invalid_cron"),
        ));
    }
    let next = match next_fire(&cron, &task.timezone, Utc::now()) {
        Ok(n) => n,
        Err(_) => {
            return Ok(failed(
                &ctx,
                BACK,
                ctx.t("admin_settings.automation.invalid_cron"),
            ));
        }
    };
    srepo::update_tenant_task(&state.pool, tenant.id, task.id, &cron, next, active).await?;
    au.log(
        &state,
        "settings.task_update",
        None,
        json!({
            "task": task.name,
            "cron": {"from": task.cron, "to": cron},
            "is_active": {"from": task.is_active, "to": active},
        }),
    )
    .await?;
    Ok(done(&ctx, BACK, "admin.common.saved"))
}

async fn run_now(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let task = load(&state, &tenant, &id).await?;
    let Ok(kind) = task.kind.parse::<JobKind>() else {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.automation.unknown_kind"),
        ));
    };
    let mut conn = state.pool.acquire().await?;
    let req = JobRequest::new(kind, Some(tenant.id), &task.payload).dedupe(format!(
        "manual:{}:{}",
        task.id,
        Utc::now().timestamp() / 60
    ));
    bjobs::enqueue(&mut conn, req).await?;
    au.log(
        &state,
        "settings.task_run",
        None,
        json!({"task": task.name, "kind": task.kind}),
    )
    .await?;
    Ok(done(&ctx, BACK, "admin_settings.automation.queued"))
}
