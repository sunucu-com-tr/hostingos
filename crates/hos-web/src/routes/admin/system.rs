//! System pages: audit log (`/admin/system/audit`, filtered) and the
//! tenant's job queue (`/admin/system/jobs`: failed jobs retry / delete).

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::ids::JobId;
use hos_db::enums::JobStatus;
use hos_db::repo::admin as arepo;
use hos_db::repo::audit::{self, AuditFilter};
use hos_db::repo::jobs::{self as jobrepo, JobRow};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, Ui as _, done, failed};
use super::dashboard::{Activity, activity_lines};
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/system/audit", get(audit_log))
        .route("/admin/system/jobs", get(jobs))
        .route("/admin/system/jobs/{id}/retry", post(job_retry))
        .route("/admin/system/jobs/{id}/delete", post(job_delete))
}

#[derive(Debug, Default, Deserialize)]
struct AuditQuery {
    action: Option<String>,
    entity: Option<String>,
    entity_id: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/system/audit.html")]
struct AuditPage {
    ctx: PageCtx,
    rows: Vec<(Activity, String, String)>,
    pager: Pager,
    action: String,
    entity: String,
}
crate::page!(AuditPage);

async fn audit_log(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    q: PageQuery,
    Query(f): Query<AuditQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let action = f.action.clone().unwrap_or_default();
    let entity = f.entity.clone().unwrap_or_default();
    let paged = audit::list(
        &state.pool,
        tenant.id,
        &AuditFilter {
            actor_id: None,
            entity_type: Some(entity.clone()).filter(|e| !e.is_empty()),
            entity_id: f.entity_id.as_deref().and_then(|e| Uuid::parse_str(e).ok()),
            action_prefix: Some(action.clone()).filter(|a| !a.is_empty()),
        },
        q.db_page(),
    )
    .await?;
    let details: Vec<(String, String)> = paged
        .items
        .iter()
        .map(|r| {
            let data = if r.changes.as_object().is_some_and(|o| !o.is_empty()) {
                serde_json::to_string(&r.changes).unwrap_or_default()
            } else {
                String::new()
            };
            (
                common::ellipsis(&data, 400),
                r.ip.map(|i| i.to_string()).unwrap_or_default(),
            )
        })
        .collect();
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/system/audit"),
        &[("action", action.as_str()), ("entity", entity.as_str())],
    );
    let lines = activity_lines(&state, &ctx, &tenant, paged.items).await?;
    let rows = lines
        .into_iter()
        .zip(details)
        .map(|(a, (d, ip))| (a, d, ip))
        .collect();
    let title = ctx.t("admin.system.audit_title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&AuditPage {
        ctx,
        rows,
        pager,
        action,
        entity,
    })
}

#[derive(Debug, Default, Deserialize)]
struct JobsQuery {
    status: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/system/jobs.html")]
struct JobsPage {
    ctx: PageCtx,
    rows: Vec<JobRow>,
    pager: Pager,
    status: String,
    statuses: Vec<(String, String)>,
}
crate::page!(JobsPage);

async fn jobs(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    q: PageQuery,
    Query(f): Query<JobsQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<JobStatus> = f.status.as_deref().and_then(|s| s.parse().ok());
    let paged = arepo::tenant_jobs(&state.pool, tenant.id, status, q.db_page()).await?;
    let sv = f.status.clone().unwrap_or_default();
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/system/jobs"),
        &[("status", sv.as_str())],
    );
    let title = ctx.t("admin.system.jobs_title");
    common::page(&mut ctx, &staff, &title, &[]);
    let statuses = common::enum_options(&ctx, JobStatus::ALL, |s| s.as_str(), "admin.status");
    render(&JobsPage {
        rows: paged.items,
        pager,
        status: sv,
        statuses,
        ctx,
    })
}

async fn load_job(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<JobRow> {
    arepo::tenant_job(&state.pool, tenant.id, JobId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

async fn job_retry(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let job = load_job(&state, &tenant, id).await?;
    let back = "/admin/system/jobs?status=failed";
    if !jobrepo::retry_job(&state.pool, job.id).await? {
        return Ok(failed(&ctx, back, ctx.t("admin.system.not_retryable")));
    }
    au.log(
        &state,
        "job.retry",
        None,
        json!({"job": job.id, "kind": job.kind}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.system.retried"))
}

async fn job_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let job = load_job(&state, &tenant, id).await?;
    let back = "/admin/system/jobs?status=failed";
    if !arepo::delete_job(&state.pool, tenant.id, job.id).await? {
        return Ok(failed(&ctx, back, ctx.t("admin.system.not_deletable")));
    }
    au.log(
        &state,
        "job.delete",
        None,
        json!({"job": job.id, "kind": job.kind}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.common.deleted"))
}
