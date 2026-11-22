//! Data import bookkeeping (`import_runs`, `import_id_map`,
//! `import_run_messages`; migration 0029) and the import-only queries of
//! `hos-import`: run lifecycle (claim / heartbeat / cancel), the source →
//! HostingOS id map, the run report, rollback (delete what a run created,
//! refusing while other rows reference it) and the cut-over helpers
//! (pausing the tenant's billing automation, grace markers).
//!
//! Entity writes live in [`import_write`](super::import_write).

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::CoreError;
use hos_core::ids::TenantId;
use serde::Serialize;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use super::Json;
use crate::error::DbResult;

// ---------------------------------------------------------------------------
// runs
// ---------------------------------------------------------------------------

/// Statuses in which a run is being (or about to be) worked on.
pub const ACTIVE_STATUSES: &[&str] = &["queued", "running", "rollback_queued", "rolling_back"];

/// `import_runs` row (without the sealed secrets).
#[derive(Debug, Clone, PartialEq, FromRow, Serialize)]
pub struct ImportRunRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub source_kind: String,
    pub source_key: String,
    pub source_label: String,
    pub source_config: Json,
    pub options: Json,
    pub mappings: Json,
    pub discovery: Json,
    pub phase: String,
    pub status: String,
    pub progress: Json,
    pub preview: Json,
    pub summary: Json,
    pub error: Option<String>,
    pub started_by: Option<Uuid>,
    pub locked_by: Option<String>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub dry_run_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub rolled_back_at: Option<DateTime<Utc>>,
    pub automation_paused: bool,
    pub secrets_wiped_at: Option<DateTime<Utc>>,
    pub has_secrets: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const RUN_COLS: &str = "id, tenant_id, source_kind, source_key, source_label, source_config, options,
    mappings, discovery, phase, status, progress, preview, summary, error, started_by, locked_by,
    heartbeat_at, cancel_requested_at, dry_run_at, started_at, finished_at, rolled_back_at,
    automation_paused, secrets_wiped_at, (secrets_enc is not null) as has_secrets, created_at, updated_at";

/// A new draft run.
#[derive(Debug, Clone)]
pub struct NewImportRun {
    pub id: Uuid,
    pub source_kind: String,
    pub source_key: String,
    pub source_label: String,
    pub source_config: Json,
    pub options: Json,
    pub started_by: Option<Uuid>,
}

/// Inserts a draft run (secrets are set separately, sealed with the run id).
pub async fn create_run(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    run: &NewImportRun,
) -> DbResult<ImportRunRow> {
    Ok(sqlx::query_as::<_, ImportRunRow>(&format!(
        "insert into import_runs (id, tenant_id, source_kind, source_key, source_label, source_config,
                                  options, started_by)
         values ($1, $2, $3, $4, $5, $6, $7, $8)
         returning {RUN_COLS}"
    ))
    .bind(run.id)
    .bind(tenant_id)
    .bind(&run.source_kind)
    .bind(&run.source_key)
    .bind(&run.source_label)
    .bind(&run.source_config)
    .bind(&run.options)
    .bind(run.started_by)
    .fetch_one(db)
    .await?)
}

/// One run of the tenant.
pub async fn get_run(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<Option<ImportRunRow>> {
    Ok(sqlx::query_as::<_, ImportRunRow>(&format!(
        "select {RUN_COLS} from import_runs where tenant_id = $1 and id = $2"
    ))
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// The tenant's runs, newest first.
pub async fn list_runs(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<ImportRunRow>> {
    Ok(sqlx::query_as::<_, ImportRunRow>(&format!(
        "select {RUN_COLS} from import_runs where tenant_id = $1 order by created_at desc limit $2"
    ))
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// The sealed source secrets of a run (`None` once wiped).
pub async fn run_secrets(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar::<_, Option<Vec<u8>>>(
        "select secrets_enc from import_runs where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?
    .flatten())
}

/// Stores sealed secrets (draft runs only).
pub async fn set_run_secrets(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    sealed: &[u8],
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs set secrets_enc = $3, secrets_wiped_at = null
         where tenant_id = $1 and id = $2 and status = 'draft'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(sealed)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Removes the sealed secrets of a run (end of run, abandoned draft).
pub async fn wipe_run_secrets(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<()> {
    sqlx::query(
        "update import_runs set secrets_enc = null,
                secrets_wiped_at = coalesce(secrets_wiped_at, now())
         where tenant_id = $1 and id = $2 and secrets_enc is not null",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Wipes the secrets of draft runs untouched for `max_age_secs` and of every
/// finished run (any tenant). Returns the number of runs wiped.
pub async fn wipe_stale_secrets(db: impl PgExecutor<'_>, max_age_secs: i64) -> DbResult<u64> {
    let r = sqlx::query(
        "update import_runs set secrets_enc = null, secrets_wiped_at = now()
         where secrets_enc is not null
           and (status in ('completed', 'failed', 'cancelled', 'rolled_back')
                or (status = 'draft' and updated_at < now() - make_interval(secs => $1)))",
    )
    .bind(max_age_secs as f64)
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

/// Updates the editable parts of a draft run.
pub async fn update_draft(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    options: Option<&Json>,
    mappings: Option<&Json>,
    discovery: Option<&Json>,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs
            set options = coalesce($3, options),
                mappings = coalesce($4, mappings),
                discovery = coalesce($5, discovery)
          where tenant_id = $1 and id = $2 and status = 'draft'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(options)
    .bind(mappings)
    .bind(discovery)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Draft → queued for `phase` (`dry_run` | `import`). Fails with `Conflict`
/// while another run of the tenant is active (unique index).
pub async fn queue_run(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    phase: &str,
    started_by: Option<Uuid>,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs
            set status = 'queued', phase = $3, progress = '{}'::jsonb, error = null,
                cancel_requested_at = null, started_by = coalesce($4, started_by)
          where tenant_id = $1 and id = $2 and status = 'draft' and secrets_enc is not null",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(phase)
    .bind(started_by)
    .execute(db)
    .await
    .map_err(crate::error::conflict_on_unique("an active import"))?;
    Ok(r.rows_affected() == 1)
}

/// Finished import → rollback queued.
pub async fn queue_rollback(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs set status = 'rollback_queued', error = null
          where tenant_id = $1 and id = $2 and phase = 'import'
            and status in ('completed', 'failed', 'cancelled')",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await
    .map_err(crate::error::conflict_on_unique("an active import"))?;
    Ok(r.rows_affected() == 1)
}

/// Asks a queued / running run to stop (a queued one stops right away).
pub async fn request_cancel(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs
            set cancel_requested_at = coalesce(cancel_requested_at, now()),
                status = case when status = 'queued' then
                             case when phase = 'dry_run' then 'draft' else 'cancelled' end
                         else status end,
                finished_at = case when status = 'queued' and phase = 'import' then now() else finished_at end,
                secrets_enc = case when status = 'queued' and phase = 'import' then null else secrets_enc end,
                secrets_wiped_at = case when status = 'queued' and phase = 'import'
                                        then coalesce(secrets_wiped_at, now()) else secrets_wiped_at end
          where tenant_id = $1 and id = $2 and status in ('queued', 'running')",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Whether a stop was requested.
pub async fn cancel_requested(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select cancel_requested_at is not null from import_runs where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?
    .unwrap_or(true))
}

/// Claims the next run to work on (any tenant): a queued run or rollback,
/// or one whose worker stopped heart-beating for `stale_secs`.
pub async fn claim_next_run(
    db: impl PgExecutor<'_>,
    worker_id: &str,
    stale_secs: i64,
) -> DbResult<Option<ImportRunRow>> {
    Ok(sqlx::query_as::<_, ImportRunRow>(&format!(
        "update import_runs r
            set status = case when r.status in ('queued', 'running') then 'running' else 'rolling_back' end,
                locked_by = $1, heartbeat_at = now(),
                started_at = case when r.status = 'queued' then now() else r.started_at end
          where r.id = (
              select id from import_runs
               where status in ('queued', 'rollback_queued')
                  or (status in ('running', 'rolling_back')
                      and (heartbeat_at is null or heartbeat_at < now() - make_interval(secs => $2)))
               order by created_at
               limit 1
               for update skip locked)
          returning {}",
        RUN_COLS.replace("id, tenant_id", "r.id, r.tenant_id")
            .replace("(secrets_enc is not null)", "(r.secrets_enc is not null)")
    ))
    .bind(worker_id)
    .bind(stale_secs as f64)
    .fetch_optional(db)
    .await?)
}

/// Marks a run as worked on by `worker_id` (CLI / tests run it inline).
pub async fn start_inline(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    worker_id: &str,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs
            set status = case when status = 'queued' then 'running' else 'rolling_back' end,
                locked_by = $3, heartbeat_at = now(),
                started_at = case when status = 'queued' then now() else started_at end
          where tenant_id = $1 and id = $2 and status in ('queued', 'rollback_queued')",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(worker_id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Heartbeat + progress snapshot. `false` when the run is no longer ours.
pub async fn save_progress(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    worker_id: &str,
    progress: &Json,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs set progress = $4, heartbeat_at = now()
          where tenant_id = $1 and id = $2 and locked_by = $3
            and status in ('running', 'rolling_back')",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(worker_id)
    .bind(progress)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// What happened in the shop itself since the last completed import.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct LiveActivity {
    /// End of the last completed import.
    pub since: DateTime<Utc>,
    /// Invoices, payments / refunds, orders and credit entries created since
    /// then that did not come from an import.
    pub records: i64,
}

/// Live activity since the last completed import of the tenant (`None`: no
/// import finished yet, or nothing happened since). A re-import then needs
/// an explicit confirmation: the shop is no longer a copy of WHMCS.
pub async fn live_activity(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Option<LiveActivity>> {
    Ok(sqlx::query_as::<_, LiveActivity>(
        "with last as (
             select max(finished_at) as since from import_runs
              where tenant_id = $1 and phase = 'import' and status = 'completed'),
         native as (
             select t.id, t.created_at from invoices t where t.tenant_id = $1
             union all select t.id, t.created_at from transactions t where t.tenant_id = $1
             union all select t.id, t.created_at from orders t where t.tenant_id = $1
             union all select t.id, t.created_at from credits t where t.tenant_id = $1)
         select last.since, count(*) as records
           from last join native n on n.created_at > last.since
          where not exists (select 1 from import_id_map m
                             where m.tenant_id = $1 and m.target_id = n.id)
          group by last.since",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?)
}

/// Heartbeat only (the background beat of a working run). `false` when the
/// run is no longer ours.
pub async fn heartbeat(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    worker_id: &str,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs set heartbeat_at = now()
          where tenant_id = $1 and id = $2 and locked_by = $3
            and status in ('running', 'rolling_back')",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(worker_id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Inside a batch transaction: locks the run row (`FOR UPDATE`) and tells
/// whether `worker_id` still holds the run. While the batch transaction is
/// open no other worker can claim the run ([`claim_next_run`] skips locked
/// rows), and a worker that lost it writes nothing more.
pub async fn hold_lease(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: Uuid,
    worker_id: &str,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select locked_by is not distinct from $3 and status in ('running', 'rolling_back')
           from import_runs where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(worker_id)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or(false))
}

/// How a worked-on run ended.
#[derive(Debug, Clone)]
pub struct RunEnd<'a> {
    /// New status (`draft` after a dry run, `completed`, `failed`, `cancelled`, `rolled_back`, …).
    pub status: &'a str,
    pub progress: &'a Json,
    /// Stored in `preview` (dry run) or `summary` (import / rollback).
    pub result: &'a Json,
    pub error: Option<&'a str>,
}

/// Finishes the run the worker holds (`false`: it is no longer ours, nothing
/// changed); wipes the secrets unless it returns to `draft` (dry run done,
/// the import may follow).
pub async fn finish_run(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    worker_id: &str,
    end: &RunEnd<'_>,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update import_runs
            set status = $3, progress = $4,
                preview = case when phase = 'dry_run' and status = 'running' then $5 else preview end,
                summary = case when phase = 'import' or status = 'rolling_back' then $5 else summary end,
                dry_run_at = case when phase = 'dry_run' and status = 'running' then now() else dry_run_at end,
                error = $6, locked_by = null, heartbeat_at = null,
                finished_at = case when $3 = 'draft' then finished_at else now() end,
                rolled_back_at = case when $3 = 'rolled_back' then now() else rolled_back_at end,
                cancel_requested_at = case when $3 = 'draft' then null else cancel_requested_at end,
                secrets_enc = case when $3 = 'draft' then secrets_enc else null end,
                secrets_wiped_at = case when $3 = 'draft' then secrets_wiped_at
                                        else coalesce(secrets_wiped_at, now()) end
          where tenant_id = $1 and id = $2 and locked_by is not distinct from $7",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(end.status)
    .bind(end.progress)
    .bind(end.result)
    .bind(end.error)
    .bind(worker_id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Records that the run paused the tenant's automation.
pub async fn set_automation_paused(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    paused: bool,
) -> DbResult<()> {
    sqlx::query("update import_runs set automation_paused = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(paused)
        .execute(db)
        .await?;
    Ok(())
}

/// The last finished import of `source_key` (prefill for "sync again").
pub async fn last_import_of_source(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    source_key: &str,
) -> DbResult<Option<ImportRunRow>> {
    Ok(sqlx::query_as::<_, ImportRunRow>(&format!(
        "select {RUN_COLS} from import_runs
          where tenant_id = $1 and source_key = $2 and phase = 'import'
            and status in ('completed', 'failed', 'cancelled')
          order by created_at desc limit 1"
    ))
    .bind(tenant_id)
    .bind(source_key)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// id map
// ---------------------------------------------------------------------------

/// A mapped target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    pub target_id: Uuid,
    /// The row existed before the import and was only linked.
    pub adopted: bool,
}

/// Targets of `source_ids` of one entity.
pub async fn lookup_ids(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    source_key: &str,
    entity: &str,
    source_ids: &[String],
) -> DbResult<HashMap<String, Mapped>> {
    if source_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, Uuid, bool)> = sqlx::query_as(
        "select source_id, target_id, created_run_id is null
           from import_id_map
          where tenant_id = $1 and source_key = $2 and entity = $3 and source_id = any($4)",
    )
    .bind(tenant_id)
    .bind(source_key)
    .bind(entity)
    .bind(source_ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(s, t, adopted)| {
            (
                s,
                Mapped {
                    target_id: t,
                    adopted,
                },
            )
        })
        .collect())
}

/// Every mapping of one entity of a source (the engine's id cache).
pub async fn load_id_map(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    source_key: &str,
    entity: &str,
) -> DbResult<HashMap<String, Mapped>> {
    let rows: Vec<(String, Uuid, bool)> = sqlx::query_as(
        "select source_id, target_id, created_run_id is null
           from import_id_map
          where tenant_id = $1 and source_key = $2 and entity = $3",
    )
    .bind(tenant_id)
    .bind(source_key)
    .bind(entity)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(s, t, adopted)| {
            (
                s,
                Mapped {
                    target_id: t,
                    adopted,
                },
            )
        })
        .collect())
}

/// Records `source → target`. A row this run created keeps `created_run_id`
/// (also when an older entry pointed at a since-deleted row); an adopted
/// row has none.
#[allow(clippy::too_many_arguments)]
pub async fn record_id(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    source_key: &str,
    entity: &str,
    source_id: &str,
    target_table: &str,
    target_id: Uuid,
    run_id: Uuid,
    created: bool,
) -> DbResult<()> {
    sqlx::query(
        "insert into import_id_map (id, tenant_id, source_key, entity, source_id, target_table,
                                    target_id, created_run_id, last_run_id)
         values ($1, $2, $3, $4, $5, $6, $7, case when $9 then $8 end, $8)
         on conflict (tenant_id, source_key, entity, source_id) do update
            set target_id = excluded.target_id, target_table = excluded.target_table,
                last_run_id = excluded.last_run_id,
                created_run_id = case
                    when import_id_map.target_id is distinct from excluded.target_id
                    then excluded.created_run_id
                    else import_id_map.created_run_id end",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(source_key)
    .bind(entity)
    .bind(source_id)
    .bind(target_table)
    .bind(target_id)
    .bind(run_id)
    .bind(created)
    .execute(db)
    .await?;
    Ok(())
}

/// Number of id-map entries of the tenant (all sources).
pub async fn id_map_count(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from import_id_map where tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(db)
            .await?,
    )
}

// ---------------------------------------------------------------------------
// messages (report)
// ---------------------------------------------------------------------------

/// One report line.
#[derive(Debug, Clone, PartialEq, Eq, FromRow, Serialize)]
pub struct RunMessage {
    pub phase: String,
    pub entity: String,
    pub level: String,
    pub source_id: Option<String>,
    pub code: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
}

/// A message to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRunMessage {
    pub entity: String,
    pub level: &'static str,
    pub source_id: Option<String>,
    pub code: String,
    pub message: String,
}

/// Appends report lines.
pub async fn add_messages(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    run_id: Uuid,
    phase: &str,
    msgs: &[NewRunMessage],
) -> DbResult<()> {
    if msgs.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = msgs.iter().map(|_| Uuid::now_v7()).collect();
    let entities: Vec<&str> = msgs.iter().map(|m| m.entity.as_str()).collect();
    let levels: Vec<&str> = msgs.iter().map(|m| m.level).collect();
    let sources: Vec<Option<&str>> = msgs.iter().map(|m| m.source_id.as_deref()).collect();
    let codes: Vec<&str> = msgs.iter().map(|m| m.code.as_str()).collect();
    let texts: Vec<&str> = msgs.iter().map(|m| m.message.as_str()).collect();
    sqlx::query(
        "insert into import_run_messages (id, tenant_id, run_id, phase, entity, level, source_id, code, message)
         select u.id, $1, $2, $3, u.entity, u.level, u.source_id, u.code, u.message
           from unnest($4::uuid[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[])
                as u(id, entity, level, source_id, code, message)",
    )
    .bind(tenant_id)
    .bind(run_id)
    .bind(phase)
    .bind(&ids)
    .bind(&entities)
    .bind(&levels)
    .bind(&sources)
    .bind(&codes)
    .bind(&texts)
    .execute(db)
    .await?;
    Ok(())
}

/// Deletes the report lines of one phase (a new dry run / import starts clean).
pub async fn clear_messages(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    run_id: Uuid,
    phase: &str,
) -> DbResult<()> {
    sqlx::query(
        "delete from import_run_messages where tenant_id = $1 and run_id = $2 and phase = $3",
    )
    .bind(tenant_id)
    .bind(run_id)
    .bind(phase)
    .execute(db)
    .await?;
    Ok(())
}

/// Report lines of a run (optionally one phase), oldest first.
pub async fn list_messages(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    run_id: Uuid,
    phase: Option<&str>,
    limit: i64,
) -> DbResult<Vec<RunMessage>> {
    Ok(sqlx::query_as::<_, RunMessage>(
        "select phase, entity, level, source_id, code, message, created_at
           from import_run_messages
          where tenant_id = $1 and run_id = $2 and ($3::text is null or phase = $3)
          order by created_at, id
          limit $4",
    )
    .bind(tenant_id)
    .bind(run_id)
    .bind(phase)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// `(entity, level) → count` of a run phase.
pub async fn message_counts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    run_id: Uuid,
    phase: &str,
) -> DbResult<BTreeMap<(String, String), i64>> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "select entity, level, count(*) from import_run_messages
          where tenant_id = $1 and run_id = $2 and phase = $3
          group by entity, level",
    )
    .bind(tenant_id)
    .bind(run_id)
    .bind(phase)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(e, l, n)| ((e, l), n)).collect())
}

// ---------------------------------------------------------------------------
// rollback
// ---------------------------------------------------------------------------

/// Rows created by a run, per target table.
pub async fn created_by_run(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    run_id: Uuid,
) -> DbResult<BTreeMap<String, Vec<Uuid>>> {
    let rows: Vec<(String, Vec<Uuid>)> = sqlx::query_as(
        "select target_table, array_agg(target_id) from import_id_map
          where tenant_id = $1 and created_run_id = $2
          group by target_table",
    )
    .bind(tenant_id)
    .bind(run_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// A row outside the run that references a row the run created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockingReference {
    /// Table of the referencing rows.
    pub table: String,
    /// Its foreign-key column.
    pub column: String,
    /// The referenced (created) table.
    pub references: String,
    pub count: i64,
}

/// Child tables whose rows never block a rollback: they only exist because
/// someone logged in / was mailed and are removed (or nulled) with the parent.
const IGNORED_REFERRERS: &[&str] = &[
    "sessions",
    "password_resets",
    "email_verifications",
    "notifications",
    "email_log",
    "audit_log",
    "import_id_map",
    "import_run_messages",
    "import_runs",
    "automation_marks",
    "user_identities",
    "totp_secrets",
    "api_keys",
    "invoice_tax_lines",
    // Rows the import writes alongside its own (prices, links, memberships);
    // they cascade with the imported parent.
    "product_pricing",
    "product_addon_pricing",
    "config_option_pricing",
    "tld_pricing",
    "config_option_group_products",
    "product_addon_products",
    "client_users",
    "user_roles",
];

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && s.as_bytes()[0].is_ascii_lowercase()
}

/// Every reference into `created` (table → ids) from a row that the run did
/// not create (catalog-driven over all foreign keys of the schema).
pub async fn blocking_references(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    created: &BTreeMap<String, Vec<Uuid>>,
) -> DbResult<Vec<BlockingReference>> {
    let fks: Vec<(String, String, String)> = sqlx::query_as(
        "select cl.relname::text, a.attname::text, p.relname::text
           from pg_constraint c
           join pg_class cl on cl.oid = c.conrelid
           join pg_namespace n on n.oid = cl.relnamespace
           join pg_class p on p.oid = c.confrelid
           join pg_attribute a on a.attrelid = c.conrelid and a.attnum = any(c.conkey)
          where c.contype = 'f' and n.nspname = current_schema()
            and a.attname <> 'tenant_id'
            and p.relname = any($1)",
    )
    .bind(created.keys().cloned().collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    let with_id: Vec<String> = sqlx::query_scalar(
        "select table_name::text from information_schema.columns
          where table_schema = current_schema() and column_name = 'id'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let with_tenant: Vec<String> = sqlx::query_scalar(
        "select table_name::text from information_schema.columns
          where table_schema = current_schema() and column_name = 'tenant_id'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let empty = Vec::new();
    let mut out = Vec::new();
    for (child, col, parent) in fks {
        if IGNORED_REFERRERS.contains(&child.as_str())
            || !ident_ok(&child)
            || !ident_ok(&col)
            || !with_tenant.contains(&child)
        {
            continue;
        }
        let Some(parent_ids) = created.get(&parent) else {
            continue;
        };
        let own = created.get(&child).unwrap_or(&empty);
        let sql = if with_id.contains(&child) {
            format!(
                "select count(*) from \"{child}\" where tenant_id = $1 and \"{col}\" = any($2)
                   and not (id = any($3))"
            )
        } else {
            format!("select count(*) from \"{child}\" where tenant_id = $1 and \"{col}\" = any($2)")
        };
        let mut q = sqlx::query_scalar::<_, i64>(&sql)
            .bind(tenant_id)
            .bind(parent_ids);
        if with_id.contains(&child) {
            q = q.bind(own);
        }
        let count = q.fetch_one(&mut *conn).await?;
        if count > 0 {
            out.push(BlockingReference {
                table: child,
                column: col,
                references: parent,
                count,
            });
        }
    }
    Ok(out)
}

/// Deletes the rows in `created` in the given table order (children first).
/// Runs inside the caller's transaction; any FK violation aborts it.
pub async fn delete_created(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    created: &BTreeMap<String, Vec<Uuid>>,
    order: &[&str],
) -> DbResult<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    let mut tables: Vec<&str> = order.to_vec();
    for t in created.keys() {
        if !tables.contains(&t.as_str()) {
            tables.insert(0, t.as_str());
        }
    }
    for table in tables {
        let Some(ids) = created.get(table) else {
            continue;
        };
        if !ident_ok(table) {
            return Err(CoreError::Internal(format!("invalid table name {table:?}")));
        }
        let r = sqlx::query(&format!(
            "delete from \"{table}\" where tenant_id = $1 and id = any($2)"
        ))
        .bind(tenant_id)
        .bind(ids)
        .execute(&mut *conn)
        .await?;
        out.insert(table.to_owned(), r.rows_affected());
    }
    Ok(out)
}

/// Drops the id-map entries of rows a run created (after a rollback).
pub async fn forget_created(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    run_id: Uuid,
) -> DbResult<u64> {
    let r = sqlx::query("delete from import_id_map where tenant_id = $1 and created_run_id = $2")
        .bind(tenant_id)
        .bind(run_id)
        .execute(&mut *conn)
        .await?;
    Ok(r.rows_affected())
}

// ---------------------------------------------------------------------------
// cut-over: automation pause, grace markers
// ---------------------------------------------------------------------------

/// Scheduled tasks an import pauses: the daily billing automation (renewal
/// invoices, reminders, late fees, suspensions, terminations) and the TLD
/// price sync (it would overwrite the imported domain prices).
pub const PAUSED_TASKS: &[(&str, &str, &str)] = &[
    ("daily_automation", "daily_automation", "0 0 6 * * *"),
    ("sync_tld_pricing", "sync_tld_pricing", "0 30 4 * * *"),
];

/// Pauses (or resumes) the tenant's [`PAUSED_TASKS`]. A task the scheduler
/// has not created yet is created inactive (the scheduler keeps
/// `is_active` when it fills in the rest).
pub async fn set_automation_active(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    active: bool,
) -> DbResult<()> {
    let tz: String = sqlx::query_scalar("select timezone from tenants where id = $1")
        .bind(tenant_id)
        .fetch_one(&mut *conn)
        .await?;
    for (name, kind, cron) in PAUSED_TASKS {
        sqlx::query(
            "insert into scheduled_tasks (id, tenant_id, name, kind, cron, timezone, is_active)
             values ($1, $2, $3, $4, $5, $6, $7)
             on conflict (tenant_id, name) do update set is_active = excluded.is_active",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(name)
        .bind(kind)
        .bind(cron)
        .bind(&tz)
        .bind(active)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Whether the tenant's daily billing automation is active.
pub async fn automation_active(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select is_active from scheduled_tasks where tenant_id = $1 and name = 'daily_automation'",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?
    .unwrap_or(true))
}

/// Inserts once-only automation markers (`automation_marks`), so the daily
/// automation treats those reminders / notices as already sent.
pub async fn add_automation_marks(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    keys: &[String],
) -> DbResult<u64> {
    if keys.is_empty() {
        return Ok(0);
    }
    let r = sqlx::query(
        "insert into automation_marks (tenant_id, key)
         select $1, k from unnest($2::text[]) as k
         on conflict do nothing",
    )
    .bind(tenant_id)
    .bind(keys)
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

/// Cut-over figures of a tenant for the imported rows of `source_key`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, FromRow)]
pub struct CutoverStats {
    /// Imported services still protected from auto-suspension.
    pub services_in_grace: i64,
    /// Latest grace end date.
    pub grace_until: Option<NaiveDate>,
    /// Imported open invoices.
    pub open_invoices: i64,
    /// Imported suspended services whose oldest open invoice is older than
    /// `terminate_after_days`: resuming automation queues their termination.
    pub services_to_terminate: i64,
    /// Imported services that would be suspended right now without the grace date.
    pub services_overdue: i64,
}

/// [`CutoverStats`] for `source_key`.
pub async fn cutover_stats(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    source_key: &str,
    today: NaiveDate,
    suspend_after_days: i32,
    terminate_after_days: i32,
) -> DbResult<CutoverStats> {
    Ok(sqlx::query_as::<_, CutoverStats>(
        "with svc as (
             select s.* from services s
               join import_id_map m on m.tenant_id = s.tenant_id and m.target_id = s.id
                and m.source_key = $2 and m.entity = 'services'
              where s.tenant_id = $1),
         inv as (
             select i.* from invoices i
               join import_id_map m on m.tenant_id = i.tenant_id and m.target_id = i.id
                and m.source_key = $2 and m.entity = 'invoices'
              where i.tenant_id = $1 and i.status in ('unpaid', 'payment_pending', 'collections'))
         select
           (select count(*) from svc where override_suspend_until >= $3) as services_in_grace,
           (select max(override_suspend_until) from svc) as grace_until,
           (select count(*) from inv) as open_invoices,
           (select count(*) from svc s where s.status = 'suspended' and $5 > 0 and exists (
               select 1 from invoice_items ii join invoices i on i.id = ii.invoice_id
                where ii.tenant_id = s.tenant_id and ii.service_id = s.id
                  and i.status in ('unpaid', 'payment_pending', 'collections')
                  and i.due_date + $5 < $3)) as services_to_terminate,
           (select count(*) from svc s where s.status = 'active' and $4 > 0 and exists (
               select 1 from invoice_items ii join invoices i on i.id = ii.invoice_id
                where ii.tenant_id = s.tenant_id and ii.service_id = s.id
                  and i.status in ('unpaid', 'payment_pending', 'collections')
                  and i.due_date + $4 < $3)) as services_overdue",
    )
    .bind(tenant_id)
    .bind(source_key)
    .bind(today)
    .bind(suspend_after_days)
    .bind(terminate_after_days)
    .fetch_one(db)
    .await?)
}

/// Open (unpaid / payment pending / collections) invoices imported from
/// `source_key`: `(id, due_date)`.
pub async fn imported_open_invoices(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    source_key: &str,
) -> DbResult<Vec<(Uuid, NaiveDate)>> {
    Ok(sqlx::query_as::<_, (Uuid, NaiveDate)>(
        "select i.id, i.due_date from invoices i
           join import_id_map m on m.tenant_id = i.tenant_id and m.target_id = i.id
            and m.source_key = $2 and m.entity = 'invoices'
          where i.tenant_id = $1 and i.status in ('unpaid', 'payment_pending', 'collections')",
    )
    .bind(tenant_id)
    .bind(source_key)
    .fetch_all(db)
    .await?)
}

/// Domains imported from `source_key` with an expiry date: `(id, expires_at)`.
pub async fn imported_domain_expiries(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    source_key: &str,
) -> DbResult<Vec<(Uuid, DateTime<Utc>)>> {
    Ok(sqlx::query_as::<_, (Uuid, DateTime<Utc>)>(
        "select d.id, d.expires_at from domains d
           join import_id_map m on m.tenant_id = d.tenant_id and m.target_id = d.id
            and m.source_key = $2 and m.entity = 'domains'
          where d.tenant_id = $1 and d.expires_at is not null",
    )
    .bind(tenant_id)
    .bind(source_key)
    .fetch_all(db)
    .await?)
}

/// `(id, name)` of the tenant's products (mapping step of the wizard).
pub async fn product_choices(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as::<_, (Uuid, String)>(
        "select id, name from products where tenant_id = $1 and not is_retired order by name limit 1000",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Highest numeric invoice sequence of the tenant.
pub async fn max_invoice_seq(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Option<i64>> {
    Ok(
        sqlx::query_scalar::<_, Option<i64>>("select max(seq) from invoices where tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(db)
            .await?,
    )
}

/// Bumps the tenant's invoice sequence past `max_seq` (imported numbers keep
/// theirs; new invoices continue after the highest one). Returns the next value.
pub async fn bump_invoice_sequence(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    max_seq: i64,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "insert into invoice_sequences (id, tenant_id, next_value) values ($1, $2, greatest($3 + 1, 1))
         on conflict (tenant_id) do update
            set next_value = greatest(invoice_sequences.next_value, excluded.next_value)
         returning next_value",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(max_seq)
    .fetch_one(db)
    .await?)
}
