//! `/admin/tasks` — the staff to-do list: tasks with due date, priority,
//! assignee (a staff member of this tenant) and an optional client. Needs
//! `tasks.manage`; changes are audited.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_auth::rbac::Permission;
use hos_core::domain::UserKind;
use hos_core::ids::{ClientId, UserId};
use hos_db::repo::tasks::{
    self as trepo, TaskFilter, TaskId, TaskInput, TaskListRow, TaskPriority, TaskRow, TaskStatus,
};
use hos_db::repo::{clients, identity};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, PermissionMarker, RequirePermission};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// `tasks.manage`.
#[derive(Debug, Clone, Copy)]
pub struct TasksManage;
impl PermissionMarker for TasksManage {
    const PERMISSION: Permission = Permission::TASKS_MANAGE;
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/tasks", get(list).post(create))
        .route("/admin/tasks/bulk", post(bulk))
        .route("/admin/tasks/{id}", get(edit).post(update))
        .route("/admin/tasks/{id}/status", post(set_status))
        .route("/admin/tasks/{id}/delete", post(delete))
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    show: String,
    #[serde(default)]
    client: String,
}

#[derive(Template)]
#[template(path = "admin/tasks.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<TaskListRow>,
    staff_options: Vec<(String, String)>,
    show: String,
    client: Option<(ClientId, String)>,
    editing: Option<TaskRow>,
    today: chrono::NaiveDate,
}
crate::page!(ListPage);

impl ListPage {
    fn overdue(&self, t: &TaskRow) -> bool {
        t.status == TaskStatus::Open && t.due_date.is_some_and(|d| d < self.today)
    }
    fn pbadge(&self, p: &TaskPriority) -> askama::filters::Safe<String> {
        let tone = match p {
            TaskPriority::High => "danger",
            TaskPriority::Normal => "info",
            TaskPriority::Low => "",
        };
        self.badge(self.lbl("tasks.priority", p.as_str()), tone)
    }
    fn tab_items(&self) -> Vec<(String, String, bool)> {
        [
            ("", "tasks.admin.show_open"),
            ("mine", "tasks.admin.show_mine"),
            ("done", "tasks.admin.show_done"),
            ("all", "tasks.admin.show_all"),
        ]
        .into_iter()
        .map(|(key, label)| {
            let href = if key.is_empty() {
                self.url("/admin/tasks")
            } else {
                self.url(format!("/admin/tasks?show={key}"))
            };
            let current = self.editing.is_none() && self.client.is_none() && self.show == key;
            (href, self.t(label), current)
        })
        .collect()
    }
    /// This list with its filter (where row and bulk actions return to).
    fn back(&self) -> String {
        format!(
            "/admin/tasks?{}",
            common::query_string(&[
                ("show", self.show.clone()),
                (
                    "client",
                    self.client
                        .as_ref()
                        .map(|c| c.0.to_string())
                        .unwrap_or_default()
                ),
            ])
        )
    }
    fn form_title(&self) -> String {
        self.t(if self.editing.is_some() {
            "tasks.admin.edit"
        } else {
            "tasks.admin.new"
        })
    }
    fn priority_options(&self) -> Vec<(String, String)> {
        common::enum_options(self, TaskPriority::ALL, |p| p.as_str(), "tasks.priority")
    }
    fn edit_value(&self, f: &str) -> String {
        let Some(t) = &self.editing else {
            return match f {
                "priority" => "normal".into(),
                "client" => self
                    .client
                    .as_ref()
                    .map(|c| c.0.to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
        };
        match f {
            "title" => t.title.clone(),
            "body" => t.body.clone().unwrap_or_default(),
            "priority" => t.priority.as_str().into(),
            "due" => t.due_date.map(|d| d.to_string()).unwrap_or_default(),
            "assignee" => t.assigned_to.map(|u| u.to_string()).unwrap_or_default(),
            "client" => t.client_id.map(|c| c.to_string()).unwrap_or_default(),
            _ => String::new(),
        }
    }
}

async fn staff_options(state: &AppState, tenant: &Tenant) -> AppResult<Vec<(String, String)>> {
    Ok(identity::list_staff(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|u| {
            (
                u.id.to_string(),
                format!("{} {}", u.first_name, u.last_name)
                    .trim()
                    .to_owned(),
            )
        })
        .collect())
}

#[allow(clippy::too_many_arguments)]
async fn render_list(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    show: String,
    client: Option<ClientId>,
    editing: Option<TaskRow>,
) -> AppResult<Response> {
    let filter = TaskFilter {
        status: match show.as_str() {
            "done" => Some(TaskStatus::Done),
            "all" => None,
            _ => Some(TaskStatus::Open),
        },
        assigned_to: (show == "mine").then_some(staff.user_id),
        client_id: client,
    };
    let rows = trepo::list_tasks(&state.pool, tenant.id, &filter, 300).await?;
    let client = match client {
        Some(id) => clients::get_client(&state.pool, tenant.id, id)
            .await?
            .map(|c| (c.id, c.display_name())),
        None => None,
    };
    let title = ctx.t("tasks.admin.title");
    common::page(&mut ctx, staff, &title, &[]);
    render(&ListPage {
        staff_options: staff_options(state, tenant).await?,
        today: common::today(&tenant.timezone),
        rows,
        show,
        client,
        editing,
        ctx,
    })
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<TasksManage>,
    Query(q): Query<ListQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let client = q.client.parse::<Uuid>().ok().map(ClientId::from);
    render_list(&state, &tenant, &staff, ctx, q.show, client, None).await
}

/// Validated input; the assignee must be staff of this tenant and the
/// client must belong to it.
async fn input(
    state: &AppState,
    tenant: &Tenant,
    f: &FormData,
) -> AppResult<Result<TaskInput, &'static str>> {
    let title = f.text("title", 200);
    if title.trim().is_empty() {
        return Ok(Err("tasks.admin.title_required"));
    }
    let Ok(due_date) = f.date("due_date") else {
        return Ok(Err("tasks.admin.invalid"));
    };
    let priority = f
        .parse::<TaskPriority>("priority")
        .unwrap_or(TaskPriority::Normal);
    let assigned_to = match f.id::<UserId>("assigned_to") {
        Some(u) => match identity::get_user(&state.pool, tenant.id, u).await? {
            Some(user) if user.kind == UserKind::Staff => Some(u),
            _ => return Err(AppError::NotFound),
        },
        None => None,
    };
    let client_id = match f.id::<ClientId>("client_id") {
        Some(c) => {
            clients::get_client(&state.pool, tenant.id, c)
                .await?
                .ok_or(AppError::NotFound)?;
            Some(c)
        }
        None => None,
    };
    Ok(Ok(TaskInput {
        title: title.trim().to_owned(),
        body: f.opt("body", 10_000),
        priority,
        due_date,
        assigned_to,
        client_id,
    }))
}

fn back_to(f: &FormData) -> String {
    match f.id::<ClientId>("client_id") {
        Some(c) if f.flag("from_client") => format!("/admin/tasks?client={c}"),
        _ => "/admin/tasks".into(),
    }
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<TasksManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = back_to(&f);
    let t = match input(&state, &tenant, &f).await? {
        Ok(t) => t,
        Err(key) => return Ok(failed(&ctx, &back, ctx.t(key))),
    };
    let row = trepo::create_task(&state.pool, tenant.id, &t, Some(staff.user_id)).await?;
    au.log(
        &state,
        "task.create",
        Some(("task", row.id.into_uuid())),
        json!({"title": row.title, "assigned_to": row.assigned_to}),
    )
    .await?;
    Ok(done(&ctx, &back, "tasks.admin.saved"))
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<TasksManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = trepo::get_task(&state.pool, tenant.id, TaskId::from(id))
        .await?
        .ok_or(AppError::NotFound)?;
    render_list(&state, &tenant, &staff, ctx, "all".into(), None, Some(row)).await
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<TasksManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = TaskId::from(id);
    trepo::get_task(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let back = format!("/admin/tasks/{id}");
    let t = match input(&state, &tenant, &f).await? {
        Ok(t) => t,
        Err(key) => return Ok(failed(&ctx, &back, ctx.t(key))),
    };
    let row = trepo::update_task(&state.pool, tenant.id, id, &t).await?;
    au.log(
        &state,
        "task.update",
        Some(("task", row.id.into_uuid())),
        json!({"title": row.title, "assigned_to": row.assigned_to}),
    )
    .await?;
    Ok(done(&ctx, "/admin/tasks", "tasks.admin.saved"))
}

async fn set_status(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<TasksManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = TaskId::from(id);
    let status = if f.text("status", 10) == "done" {
        TaskStatus::Done
    } else {
        TaskStatus::Open
    };
    if !trepo::set_status(&state.pool, tenant.id, id, status, Utc::now()).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "task.status",
        Some(("task", id.into_uuid())),
        json!({"status": status}),
    )
    .await?;
    let back = crate::locale::safe_local_target(f.text("back", 300).trim())
        .filter(|b| b.starts_with("/admin/tasks"))
        .unwrap_or("/admin/tasks")
        .to_owned();
    Ok(done(
        &ctx,
        &back,
        if status == TaskStatus::Done {
            "tasks.admin.completed"
        } else {
            "tasks.admin.reopened"
        },
    ))
}

/// `POST /admin/tasks/bulk` — completes or reopens the selected tasks
/// (ids of other tenants change nothing and count as skipped).
async fn bulk(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<TasksManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = crate::locale::safe_local_target(f.text("back", 300).trim())
        .filter(|b| b.starts_with("/admin/tasks"))
        .unwrap_or("/admin/tasks")
        .to_owned();
    let ids: Vec<TaskId> = f.ids("ids");
    if ids.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.nothing_selected")));
    }
    let status = match f.text("action", 20).as_str() {
        "complete" => TaskStatus::Done,
        "reopen" => TaskStatus::Open,
        _ => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_value"))),
    };
    let now = Utc::now();
    let (mut changed, mut skipped) = (0usize, 0usize);
    for id in &ids {
        if trepo::set_status(&state.pool, tenant.id, *id, status, now).await? {
            changed += 1;
        } else {
            skipped += 1;
        }
    }
    au.log(
        &state,
        "task.bulk",
        None,
        json!({"status": status, "tasks": ids, "done": changed, "skipped": skipped}),
    )
    .await?;
    Ok(crate::flash::redirect(
        &ctx.url(&back),
        crate::flash::SetFlash::success(ctx.tv2(
            "admin.common.bulk_done",
            "done",
            changed,
            "skipped",
            skipped,
        )),
    ))
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<TasksManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = TaskId::from(id);
    if !trepo::delete_task(&state.pool, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "task.delete",
        Some(("task", id.into_uuid())),
        json!({}),
    )
    .await?;
    Ok(done(&ctx, "/admin/tasks", "tasks.admin.deleted"))
}
