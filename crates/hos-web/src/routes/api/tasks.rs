//! `/api/v1/tasks` — the staff to-do list (`tasks.manage`, staff only):
//! list, create, read, update (fields and `status`) and delete. The
//! assignee must be a staff user of the tenant and the client one of its
//! clients, as in the admin.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use chrono::{NaiveDate, Utc};
use hos_auth::rbac::Permission;
use hos_core::domain::UserKind;
use hos_core::ids::{ClientId, UserId};
use hos_db::repo::tasks::{
    self as trepo, TaskFilter, TaskId, TaskInput, TaskPriority, TaskRow, TaskStatus,
};
use hos_db::repo::{clients, identity};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, collection, parse_body, parse_id};
use crate::state::AppState;

fn task_json(t: &TaskRow) -> Value {
    json!({
        "id": t.id,
        "title": t.title,
        "body": t.body,
        "status": t.status.as_str(),
        "priority": t.priority.as_str(),
        "due_date": t.due_date,
        "assigned_to": t.assigned_to,
        "client_id": t.client_id,
        "created_by": t.created_by,
        "completed_at": t.completed_at,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
    })
}

/// `GET /api/v1/tasks` — up to 300: open first by due date
/// (`?status=open|done`, `?assigned_to=`, `?client_id=`).
pub async fn list(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::TASKS_MANAGE)?;
    let opt = |k: &str| raw.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let filter = TaskFilter {
        status: opt("status")
            .map(|s| {
                s.parse::<TaskStatus>()
                    .map_err(|_| ApiError::field("status", "open or done"))
            })
            .transpose()?,
        assigned_to: opt("assigned_to")
            .map(|s| {
                s.parse::<UserId>()
                    .map_err(|_| ApiError::field("assigned_to", "not an id"))
            })
            .transpose()?,
        client_id: opt("client_id")
            .map(|s| {
                s.parse::<ClientId>()
                    .map_err(|_| ApiError::field("client_id", "not an id"))
            })
            .transpose()?,
    };
    let rows = trepo::list_tasks(&state.pool, tenant.id, &filter, 300).await?;
    Ok(collection(
        rows.iter()
            .map(|r| {
                let mut v = task_json(&r.task);
                v["assignee_name"] = json!(r.assignee_name);
                v["client_name"] = json!(r.client_name);
                v
            })
            .collect(),
    ))
}

/// Body of create / update (`PATCH` keeps omitted fields).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBody {
    pub title: Option<String>,
    #[serde(default, with = "double_option")]
    pub body: Option<Option<String>>,
    pub priority: Option<String>,
    #[serde(default, with = "double_option")]
    pub due_date: Option<Option<NaiveDate>>,
    #[serde(default, with = "double_option")]
    pub assigned_to: Option<Option<UserId>>,
    #[serde(default, with = "double_option")]
    pub client_id: Option<Option<ClientId>>,
    pub status: Option<String>,
}

/// `null` → `Some(None)`, absent → `None`.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// Applies `b` over `base`, checking the assignee and client belong to the
/// tenant (`422` otherwise).
async fn merged(
    state: &AppState,
    tenant: &ApiTenant,
    base: TaskInput,
    b: &TaskBody,
) -> ApiResult<TaskInput> {
    let mut t = base;
    if let Some(title) = &b.title {
        t.title = crate::forms::clean(title, 200);
    }
    if t.title.is_empty() {
        return Err(ApiError::field("title", "required"));
    }
    if let Some(body) = &b.body {
        t.body = body
            .as_deref()
            .map(|s| s.trim().chars().take(10_000).collect::<String>())
            .filter(|s| !s.is_empty());
    }
    if let Some(p) = &b.priority {
        t.priority = p
            .parse::<TaskPriority>()
            .map_err(|_| ApiError::field("priority", "low, normal or high"))?;
    }
    if let Some(d) = b.due_date {
        t.due_date = d;
    }
    if let Some(a) = b.assigned_to {
        if let Some(u) = a {
            let staff = identity::get_user(&state.pool, tenant.id, u)
                .await?
                .is_some_and(|u| u.kind == UserKind::Staff);
            if !staff {
                return Err(ApiError::field("assigned_to", "unknown staff user"));
            }
        }
        t.assigned_to = a;
    }
    if let Some(c) = b.client_id {
        if let Some(id) = c
            && clients::get_client(&state.pool, tenant.id, id)
                .await?
                .is_none()
        {
            return Err(ApiError::field("client_id", "unknown client"));
        }
        t.client_id = c;
    }
    Ok(t)
}

fn status_of(b: &TaskBody) -> ApiResult<Option<TaskStatus>> {
    b.status
        .as_deref()
        .map(|s| {
            s.parse::<TaskStatus>()
                .map_err(|_| ApiError::field("status", "open or done"))
        })
        .transpose()
}

/// `POST /api/v1/tasks`.
pub async fn create(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let user = auth.staff(Permission::TASKS_MANAGE)?;
    let b: TaskBody = parse_body(&body)?;
    let status = status_of(&b)?;
    let input = merged(
        &state,
        &tenant,
        TaskInput {
            title: String::new(),
            body: None,
            priority: TaskPriority::Normal,
            due_date: None,
            assigned_to: None,
            client_id: None,
        },
        &b,
    )
    .await?;
    let mut row = trepo::create_task(&state.pool, tenant.id, &input, Some(user)).await?;
    if status == Some(TaskStatus::Done) {
        trepo::set_status(&state.pool, tenant.id, row.id, TaskStatus::Done, Utc::now()).await?;
        row = trepo::get_task(&state.pool, tenant.id, row.id)
            .await?
            .ok_or_else(ApiError::not_found)?;
    }
    Ok(ApiReply::created(task_json(&row)))
}

async fn own(state: &AppState, tenant: &ApiTenant, raw: &str) -> ApiResult<TaskRow> {
    let id: TaskId = parse_id(raw)?;
    trepo::get_task(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// `GET /api/v1/tasks/{id}`.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(raw): Path<String>,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::TASKS_MANAGE)?;
    Ok(ApiReply::ok(task_json(&own(&state, &tenant, &raw).await?)))
}

/// `PATCH /api/v1/tasks/{id}` — changes the given fields; `status`
/// (`open` / `done`) reopens or completes the task.
pub async fn update(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(raw): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::TASKS_MANAGE)?;
    let current = own(&state, &tenant, &raw).await?;
    let b: TaskBody = parse_body(&body)?;
    let status = status_of(&b)?;
    let input = merged(
        &state,
        &tenant,
        TaskInput {
            title: current.title.clone(),
            body: current.body.clone(),
            priority: current.priority,
            due_date: current.due_date,
            assigned_to: current.assigned_to,
            client_id: current.client_id,
        },
        &b,
    )
    .await?;
    trepo::update_task(&state.pool, tenant.id, current.id, &input).await?;
    if let Some(s) = status.filter(|s| *s != current.status) {
        trepo::set_status(&state.pool, tenant.id, current.id, s, Utc::now()).await?;
    }
    let row = trepo::get_task(&state.pool, tenant.id, current.id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(ApiReply::ok(task_json(&row)))
}

/// `DELETE /api/v1/tasks/{id}`.
pub async fn delete(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(raw): Path<String>,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::TASKS_MANAGE)?;
    let t = own(&state, &tenant, &raw).await?;
    trepo::delete_task(&state.pool, tenant.id, t.id).await?;
    Ok(ApiReply::ok(json!({ "id": t.id, "deleted": true })))
}
