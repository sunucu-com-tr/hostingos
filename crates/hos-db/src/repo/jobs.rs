//! Postgres job queue and cron task table (used by `hos-jobs`).
//!
//! Lifecycle: `pending` → (claimed) `running` → `completed`; on error either
//! back to `pending` with a later `run_at` (retry with exponential backoff) or
//! `failed` once `attempts` reaches `max_attempts`. Workers claim with
//! `FOR UPDATE SKIP LOCKED`, so concurrent workers never receive the same job.
//! Jobs of a worker that died are returned to the queue by
//! [`recover_stuck_jobs`].
//!
//! Jobs and scheduled tasks are system tables: `tenant_id` is optional (null
//! for platform-wide jobs) and part of the job, not a filter argument.

use std::time::Duration;

use chrono::{DateTime, Utc};
use hos_core::ids::{JobId, TenantId};
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};

use super::{Json, empty_object};
use crate::enums::{JobStatus, TaskRunStatus};
use crate::error::DbResult;
use crate::ids::ScheduledTaskId;
use crate::page::{Page, Paged, fetch_paged};

/// Default queue name.
pub const DEFAULT_QUEUE: &str = "default";

/// Queue namespace of this process (see [`isolate_queue`]).
static QUEUE_NAMESPACE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Puts every queue this process uses into the namespace `ns` (first call
/// wins): [`enqueue`] stores `ns.<queue>` and [`claim_batch`] claims
/// `ns.<queue>`, so workers of this process only see their own jobs and
/// other processes on the same database never see them. Meant for test
/// processes that share a database with a running `hostingos` worker,
/// which would otherwise claim (and fail, with its own secret key) the test
/// tenants' jobs. `billing_support::claim_tenant_jobs` (the tests'
/// `run_tenant_until_idle`) claims a tenant's jobs whatever their queue.
pub fn isolate_queue(ns: &str) {
    let _ = QUEUE_NAMESPACE.set(ns.to_owned());
}

/// `queue` in this process's namespace.
fn namespaced(queue: &str) -> String {
    match QUEUE_NAMESPACE.get() {
        Some(ns) => format!("{ns}.{queue}"),
        None => queue.to_owned(),
    }
}

/// `jobs` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct JobRow {
    pub id: JobId,
    pub tenant_id: Option<TenantId>,
    pub queue: String,
    pub kind: String,
    pub payload: Json,
    pub status: JobStatus,
    pub priority: i16,
    pub run_at: DateTime<Utc>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub locked_by: Option<String>,
    pub locked_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub result: Option<Json>,
    pub dedupe_key: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A job to enqueue. Build with [`NewJob::new`] and the builder methods.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub tenant_id: Option<TenantId>,
    pub queue: String,
    pub kind: String,
    pub payload: Json,
    pub priority: i16,
    /// `None` = now.
    pub run_at: Option<DateTime<Utc>>,
    pub max_attempts: i32,
    /// Global idempotency key: a second enqueue with the same key is a no-op
    /// while the first job row exists.
    pub dedupe_key: Option<String>,
}

impl NewJob {
    /// Job of `kind` on the default queue, 5 attempts, priority 0.
    pub fn new(kind: &str, payload: Json) -> Self {
        Self {
            tenant_id: None,
            queue: DEFAULT_QUEUE.to_owned(),
            kind: kind.to_owned(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: 5,
            dedupe_key: None,
        }
    }

    /// Sets the tenant.
    #[must_use]
    pub fn tenant(mut self, tenant_id: TenantId) -> Self {
        self.tenant_id = Some(tenant_id);
        self
    }

    /// Sets the queue.
    #[must_use]
    pub fn queue(mut self, queue: &str) -> Self {
        self.queue = queue.to_owned();
        self
    }

    /// Sets the priority (higher first).
    #[must_use]
    pub fn priority(mut self, priority: i16) -> Self {
        self.priority = priority;
        self
    }

    /// Delays the job.
    #[must_use]
    pub fn run_at(mut self, at: DateTime<Utc>) -> Self {
        self.run_at = Some(at);
        self
    }

    /// Sets `max_attempts` (at least 1).
    #[must_use]
    pub fn max_attempts(mut self, n: i32) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    /// Sets the dedupe key.
    #[must_use]
    pub fn dedupe(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }
}

/// Result of [`enqueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// A new job was created.
    Created(JobId),
    /// A job with the same dedupe key already exists.
    Duplicate(JobId),
}

impl Enqueued {
    /// The job id (new or existing).
    pub fn id(&self) -> JobId {
        match self {
            Self::Created(id) | Self::Duplicate(id) => *id,
        }
    }

    /// Whether a new job was created.
    pub fn is_created(&self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// Enqueues a job; with a dedupe key, an existing job with that key wins.
pub async fn enqueue(db: impl PgExecutor<'_>, job: &NewJob) -> DbResult<Enqueued> {
    let (id, created): (JobId, bool) = sqlx::query_as(
        "with ins as (
             insert into jobs (id, tenant_id, queue, kind, payload, priority, run_at, max_attempts, dedupe_key)
             values ($1, $2, $3, $4, $5, $6, coalesce($7, now()), $8, $9)
             on conflict (dedupe_key) do nothing
             returning id
         )
         select id, true from ins
         union all
         select id, false from jobs where dedupe_key = $9 and not exists (select 1 from ins)
         limit 1",
    )
    .bind(JobId::new())
    .bind(job.tenant_id)
    .bind(namespaced(&job.queue))
    .bind(&job.kind)
    .bind(&job.payload)
    .bind(job.priority)
    .bind(job.run_at)
    .bind(job.max_attempts)
    .bind(&job.dedupe_key)
    .fetch_one(db)
    .await?;
    Ok(if created {
        Enqueued::Created(id)
    } else {
        Enqueued::Duplicate(id)
    })
}

/// Claims up to `limit` due pending jobs from `queues` for `worker_id`
/// (`FOR UPDATE SKIP LOCKED`), marking them `running` and incrementing
/// `attempts`. Returned in priority / `run_at` order.
pub async fn claim_batch(
    db: impl PgExecutor<'_>,
    worker_id: &str,
    queues: &[String],
    limit: i64,
) -> DbResult<Vec<JobRow>> {
    let mut jobs = sqlx::query_as::<_, JobRow>(
        "update jobs j
            set status = 'running', locked_by = $1, locked_at = now(), attempts = j.attempts + 1
           from (select id from jobs
                  where status = 'pending' and queue = any($2) and run_at <= now()
                  order by priority desc, run_at, id
                  limit $3
                  for update skip locked) c
          where j.id = c.id
         returning j.*",
    )
    .bind(worker_id)
    .bind(queues.iter().map(|q| namespaced(q)).collect::<Vec<_>>())
    .bind(limit)
    .fetch_all(db)
    .await?;
    jobs.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.run_at.cmp(&b.run_at))
            .then(a.id.cmp(&b.id))
    });
    Ok(jobs)
}

/// Marks a job completed — only if this worker still holds it.
pub async fn complete_job(
    db: impl PgExecutor<'_>,
    id: JobId,
    worker_id: &str,
    result: Option<&Json>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update jobs set status = 'completed', completed_at = now(), result = $3, last_error = null,
                locked_by = null, locked_at = null
         where id = $1 and status = 'running' and locked_by = $2",
    )
    .bind(id)
    .bind(worker_id)
    .bind(result)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// What to do with a failed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Retry with exponential backoff (`base · 2^(attempts-1)`, capped, ±10 % jitter)
    /// while attempts remain.
    Backoff {
        /// Delay after the first failure.
        base: Duration,
        /// Upper bound of the delay.
        max: Duration,
    },
    /// Retry after a fixed delay (e.g. provider `Retry-After`) while attempts remain.
    After(Duration),
    /// Permanent failure: no retry.
    Never,
}

impl Retry {
    /// 30 s, doubling, capped at 6 h.
    pub const DEFAULT: Retry = Retry::Backoff {
        base: Duration::from_secs(30),
        max: Duration::from_secs(6 * 3600),
    };
}

/// Records a failure. Returns the new status (`pending` = will retry,
/// `failed` = gave up), or `None` if this worker no longer holds the job.
pub async fn fail_job(
    db: impl PgExecutor<'_>,
    id: JobId,
    worker_id: &str,
    error: &str,
    retry: Retry,
) -> DbResult<Option<JobStatus>> {
    let (mode, base, max): (&str, f64, f64) = match retry {
        Retry::Backoff { base, max } => ("backoff", base.as_secs_f64(), max.as_secs_f64()),
        Retry::After(d) => ("after", d.as_secs_f64(), d.as_secs_f64()),
        Retry::Never => ("never", 0.0, 0.0),
    };
    let error: String = error.chars().take(4000).collect();
    Ok(sqlx::query_scalar::<_, JobStatus>(
        "update jobs set
                status = case when $3 <> 'never' and attempts < max_attempts then 'pending' else 'failed' end,
                run_at = case when $3 = 'backoff' and attempts < max_attempts
                                  then now() + make_interval(secs => least($4 * power(2, greatest(attempts - 1, 0)), $5)
                                                                     * (0.9 + random() * 0.2))
                              when $3 = 'after' and attempts < max_attempts then now() + make_interval(secs => $4)
                              else run_at end,
                failed_at = case when $3 <> 'never' and attempts < max_attempts then failed_at else now() end,
                last_error = $6, locked_by = null, locked_at = null
         where id = $1 and status = 'running' and locked_by = $2
         returning status",
    )
    .bind(id)
    .bind(worker_id)
    .bind(mode)
    .bind(base)
    .bind(max)
    .bind(error)
    .fetch_optional(db)
    .await?)
}

/// Extends a long-running job's lock (`locked_at = now()`).
pub async fn heartbeat(db: impl PgExecutor<'_>, id: JobId, worker_id: &str) -> DbResult<bool> {
    let res = sqlx::query(
        "update jobs set locked_at = now() where id = $1 and status = 'running' and locked_by = $2",
    )
    .bind(id)
    .bind(worker_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Returns jobs whose lock is older than `lock_timeout` (crashed worker) to
/// `pending` — or `failed` when no attempts remain. Returns the number touched.
pub async fn recover_stuck_jobs(db: impl PgExecutor<'_>, lock_timeout: Duration) -> DbResult<u64> {
    let res = sqlx::query(
        "update jobs set
                status = case when attempts < max_attempts then 'pending' else 'failed' end,
                failed_at = case when attempts < max_attempts then failed_at else now() end,
                run_at = now(),
                last_error = coalesce(last_error || E'\\n', '') || 'lock expired (worker ' || coalesce(locked_by, '?') || ' lost)',
                locked_by = null, locked_at = null
         where status = 'running' and locked_at < now() - make_interval(secs => $1)",
    )
    .bind(lock_timeout.as_secs_f64())
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Cancels a pending job.
pub async fn cancel_job(db: impl PgExecutor<'_>, id: JobId) -> DbResult<bool> {
    let res =
        sqlx::query("update jobs set status = 'cancelled' where id = $1 and status = 'pending'")
            .bind(id)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Re-queues a failed / cancelled job now, with a fresh attempt budget.
pub async fn retry_job(db: impl PgExecutor<'_>, id: JobId) -> DbResult<bool> {
    let res = sqlx::query(
        "update jobs set status = 'pending', attempts = 0, run_at = now(), failed_at = null
         where id = $1 and status in ('failed', 'cancelled')",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Job by id.
pub async fn get_job(db: impl PgExecutor<'_>, id: JobId) -> DbResult<Option<JobRow>> {
    Ok(
        sqlx::query_as::<_, JobRow>("select * from jobs where id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Filters of [`list_jobs`].
#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    pub tenant_id: Option<TenantId>,
    pub status: Option<JobStatus>,
    pub queue: Option<String>,
    pub kind: Option<String>,
}

/// Jobs, newest first (admin / platform job monitor).
pub async fn list_jobs<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    filter: &JobFilter,
    page: Page,
) -> DbResult<Paged<JobRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from jobs where true");
            if let Some(t) = f.tenant_id {
                qb.push(" and tenant_id = ").push_bind(t);
            }
            if let Some(s) = f.status {
                qb.push(" and status = ").push_bind(s);
            }
            if let Some(q) = &f.queue {
                qb.push(" and queue = ").push_bind(q.clone());
            }
            if let Some(k) = &f.kind {
                qb.push(" and kind = ").push_bind(k.clone());
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}

/// `(queue, status, count)` over all jobs.
pub async fn job_counts(db: impl PgExecutor<'_>) -> DbResult<Vec<(String, JobStatus, i64)>> {
    Ok(sqlx::query_as::<_, (String, JobStatus, i64)>(
        "select queue, status, count(*) from jobs group by queue, status order by queue, status",
    )
    .fetch_all(db)
    .await?)
}

/// Deletes finished (completed / failed / cancelled) jobs older than `age`.
pub async fn purge_finished_jobs(db: impl PgExecutor<'_>, age: Duration) -> DbResult<u64> {
    let res = sqlx::query(
        "delete from jobs where status in ('completed', 'failed', 'cancelled')
           and updated_at < now() - make_interval(secs => $1)",
    )
    .bind(age.as_secs_f64())
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// scheduled tasks
// ---------------------------------------------------------------------------

/// `scheduled_tasks` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ScheduledTaskRow {
    pub id: ScheduledTaskId,
    pub tenant_id: Option<TenantId>,
    pub name: String,
    pub kind: String,
    pub queue: String,
    pub payload: Json,
    pub cron: String,
    pub timezone: String,
    pub is_active: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_status: Option<TaskRunStatus>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Definition for [`upsert_scheduled_task`].
#[derive(Debug, Clone)]
pub struct ScheduledTaskInput {
    pub tenant_id: Option<TenantId>,
    pub name: String,
    pub kind: String,
    pub queue: String,
    pub payload: Json,
    pub cron: String,
    pub timezone: String,
    /// First run (computed by the caller from `cron`).
    pub next_run_at: Option<DateTime<Utc>>,
}

impl ScheduledTaskInput {
    /// Platform-wide task on the default queue (UTC).
    pub fn new(name: &str, kind: &str, cron: &str) -> Self {
        Self {
            tenant_id: None,
            name: name.to_owned(),
            kind: kind.to_owned(),
            queue: DEFAULT_QUEUE.to_owned(),
            payload: empty_object(),
            cron: cron.to_owned(),
            timezone: "UTC".to_owned(),
            next_run_at: None,
        }
    }
}

/// Inserts or updates a task by `(tenant_id, name)`. `next_run_at` is only
/// replaced when the schedule changed or was never computed.
pub async fn upsert_scheduled_task(
    db: impl PgExecutor<'_>,
    t: &ScheduledTaskInput,
) -> DbResult<ScheduledTaskRow> {
    Ok(sqlx::query_as::<_, ScheduledTaskRow>(
        "insert into scheduled_tasks (id, tenant_id, name, kind, queue, payload, cron, timezone, next_run_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         on conflict (tenant_id, name) do update
            set kind = excluded.kind, queue = excluded.queue, payload = excluded.payload,
                next_run_at = case when scheduled_tasks.cron is distinct from excluded.cron
                                     or scheduled_tasks.timezone is distinct from excluded.timezone
                                     or scheduled_tasks.next_run_at is null
                                   then excluded.next_run_at else scheduled_tasks.next_run_at end,
                cron = excluded.cron, timezone = excluded.timezone
         returning *",
    )
    .bind(ScheduledTaskId::new())
    .bind(t.tenant_id)
    .bind(&t.name)
    .bind(&t.kind)
    .bind(&t.queue)
    .bind(&t.payload)
    .bind(&t.cron)
    .bind(&t.timezone)
    .bind(t.next_run_at)
    .fetch_one(db)
    .await?)
}

/// Locks up to `limit` due active tasks (`FOR UPDATE SKIP LOCKED`). **Call
/// inside a transaction**, enqueue their jobs and call [`mark_task_run`] for
/// each before committing; other schedulers skip the locked rows.
pub async fn lock_due_tasks(
    conn: &mut PgConnection,
    limit: i64,
) -> DbResult<Vec<ScheduledTaskRow>> {
    Ok(sqlx::query_as::<_, ScheduledTaskRow>(
        "select * from scheduled_tasks
         where is_active and next_run_at is not null and next_run_at <= now()
         order by next_run_at limit $1
         for update skip locked",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?)
}

/// [`lock_due_tasks`] restricted to one tenant's tasks (a scheduler scoped to
/// a tenant, e.g. in tests that share a database with other tenants).
pub async fn lock_due_tenant_tasks(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<ScheduledTaskRow>> {
    Ok(sqlx::query_as::<_, ScheduledTaskRow>(
        "select * from scheduled_tasks
         where tenant_id = $1 and is_active and next_run_at is not null and next_run_at <= now()
         order by next_run_at limit $2
         for update skip locked",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?)
}

/// Records a run and the next fire time.
pub async fn mark_task_run(
    db: impl PgExecutor<'_>,
    id: ScheduledTaskId,
    next_run_at: Option<DateTime<Utc>>,
    status: TaskRunStatus,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update scheduled_tasks set last_run_at = now(), next_run_at = $2, last_status = $3, last_error = $4
         where id = $1",
    )
    .bind(id)
    .bind(next_run_at)
    .bind(status)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Tasks of a tenant (`None` = platform tasks).
pub async fn list_scheduled_tasks(
    db: impl PgExecutor<'_>,
    tenant_id: Option<TenantId>,
) -> DbResult<Vec<ScheduledTaskRow>> {
    Ok(sqlx::query_as::<_, ScheduledTaskRow>(
        "select * from scheduled_tasks where tenant_id is not distinct from $1 order by name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Enables / disables a task.
pub async fn set_task_active(
    db: impl PgExecutor<'_>,
    id: ScheduledTaskId,
    active: bool,
) -> DbResult<bool> {
    let res = sqlx::query("update scheduled_tasks set is_active = $2 where id = $1")
        .bind(id)
        .bind(active)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}
