//! Staff to-do list (`staff_tasks`).

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::ids::{ClientId, TenantId, UserId};
use hos_core::string_enum;
use sqlx::{FromRow, PgExecutor};

use crate::error::{DbResult, conflict_on_unique, not_found};

hos_core::define_id! {
    /// `staff_tasks`.
    pub struct TaskId;
}

string_enum! {
    /// `staff_tasks.status`.
    pub enum TaskStatus {
        Open => "open",
        Done => "done",
    }
}

string_enum! {
    /// `staff_tasks.priority`.
    pub enum TaskPriority {
        Low => "low",
        Normal => "normal",
        High => "high",
    }
}

/// `staff_tasks` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct TaskRow {
    pub id: TaskId,
    pub tenant_id: TenantId,
    pub title: String,
    pub body: Option<String>,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub due_date: Option<NaiveDate>,
    pub assigned_to: Option<UserId>,
    pub client_id: Option<ClientId>,
    pub created_by: Option<UserId>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A list row with display names.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct TaskListRow {
    #[sqlx(flatten)]
    pub task: TaskRow,
    pub assignee_name: Option<String>,
    pub client_name: Option<String>,
}

/// Editable fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInput {
    pub title: String,
    pub body: Option<String>,
    pub priority: TaskPriority,
    pub due_date: Option<NaiveDate>,
    pub assigned_to: Option<UserId>,
    pub client_id: Option<ClientId>,
}

/// Creates a task.
pub async fn create_task(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    t: &TaskInput,
    created_by: Option<UserId>,
) -> DbResult<TaskRow> {
    sqlx::query_as::<_, TaskRow>(
        "insert into staff_tasks (id, tenant_id, title, body, priority, due_date, assigned_to, client_id, created_by)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning *",
    )
    .bind(TaskId::new())
    .bind(tenant_id)
    .bind(&t.title)
    .bind(&t.body)
    .bind(t.priority)
    .bind(t.due_date)
    .bind(t.assigned_to)
    .bind(t.client_id)
    .bind(created_by)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("task"))
}

/// Updates a task's fields.
pub async fn update_task(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TaskId,
    t: &TaskInput,
) -> DbResult<TaskRow> {
    sqlx::query_as::<_, TaskRow>(
        "update staff_tasks set title = $3, body = $4, priority = $5, due_date = $6,
                assigned_to = $7, client_id = $8
          where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&t.title)
    .bind(&t.body)
    .bind(t.priority)
    .bind(t.due_date)
    .bind(t.assigned_to)
    .bind(t.client_id)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("task"))?
    .ok_or_else(|| not_found("task", id))
}

/// Marks a task done / open again.
pub async fn set_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TaskId,
    status: TaskStatus,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update staff_tasks set status = $3,
                completed_at = case when $3 = 'done' then coalesce(completed_at, $4) end
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(now)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Deletes a task.
pub async fn delete_task(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TaskId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from staff_tasks where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// One task.
pub async fn get_task(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TaskId,
) -> DbResult<Option<TaskRow>> {
    Ok(
        sqlx::query_as::<_, TaskRow>("select * from staff_tasks where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// List filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskFilter {
    /// `None`: all.
    pub status: Option<TaskStatus>,
    /// Only tasks assigned to this user.
    pub assigned_to: Option<UserId>,
    /// Only tasks of this client.
    pub client_id: Option<ClientId>,
}

/// Tasks: open first, by due date (no date last), then newest.
pub async fn list_tasks(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    f: &TaskFilter,
    limit: i64,
) -> DbResult<Vec<TaskListRow>> {
    Ok(sqlx::query_as::<_, TaskListRow>(
        "select t.*,
                nullif(trim(u.first_name || ' ' || u.last_name), '') as assignee_name,
                coalesce(nullif(c.company_name, ''), c.first_name || ' ' || c.last_name) as client_name
           from staff_tasks t
           left join users u on u.tenant_id = t.tenant_id and u.id = t.assigned_to
           left join clients c on c.tenant_id = t.tenant_id and c.id = t.client_id
          where t.tenant_id = $1
            and ($2::text is null or t.status = $2)
            and ($3::uuid is null or t.assigned_to = $3)
            and ($4::uuid is null or t.client_id = $4)
          order by t.status = 'done', t.due_date asc nulls last,
                   case t.priority when 'high' then 0 when 'normal' then 1 else 2 end, t.created_at desc
          limit $5",
    )
    .bind(tenant_id)
    .bind(f.status)
    .bind(f.assigned_to)
    .bind(f.client_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Open tasks of `user` due on or before `day` (dashboard reminder).
pub async fn due_count(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user: UserId,
    day: NaiveDate,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from staff_tasks
          where tenant_id = $1 and status = 'open' and assigned_to = $2 and due_date <= $3",
    )
    .bind(tenant_id)
    .bind(user)
    .bind(day)
    .fetch_one(db)
    .await?)
}
