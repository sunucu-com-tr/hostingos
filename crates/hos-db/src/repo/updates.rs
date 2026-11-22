//! Self-update (migration 0045, docs/UPDATES.md): `update_state`,
//! `update_releases`, `update_runs`, `update_trusted_keys`,
//! `platform_notifications`, the cluster-wide update advisory lock and the
//! maintenance flag. Platform-level: no tenant scope.

use chrono::{DateTime, Utc};
use hos_core::ids::PlatformAdminId;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use super::Json;
use crate::error::{DbResult, conflict_on_unique};

/// Key of the PostgreSQL advisory lock held while an update is applied
/// (any node of the cluster).
pub const UPDATE_LOCK_KEY: i64 = 0x486f_5355_7064_0045;

// ---------------------------------------------------------------------------
// state / settings
// ---------------------------------------------------------------------------

/// `update_state` (singleton).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct UpdateStateRow {
    pub channel: String,
    pub auto_check: bool,
    pub notify_email: Option<String>,
    pub webhook_url: Option<String>,
    pub webhook_method: String,
    pub webhook_secret_enc: Option<Vec<u8>>,
    pub last_check_at: Option<DateTime<Utc>>,
    pub last_check_error: Option<String>,
    pub last_check_problems: Json,
    pub next_check_at: Option<DateTime<Utc>>,
    pub notified_version: Option<String>,
    pub maintenance: bool,
    pub maintenance_reason: Option<String>,
    pub maintenance_since: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

const STATE_COLUMNS: &str = "channel, auto_check, notify_email::text as notify_email, webhook_url, \
     webhook_method, webhook_secret_enc, last_check_at, last_check_error, last_check_problems, \
     next_check_at, notified_version, maintenance, maintenance_reason, maintenance_since, updated_at";

/// The state row (created by the migration; re-created if deleted).
pub async fn state(conn: &mut PgConnection) -> DbResult<UpdateStateRow> {
    sqlx::query("insert into update_state (id) values (true) on conflict do nothing")
        .execute(&mut *conn)
        .await?;
    Ok(sqlx::query_as::<_, UpdateStateRow>(&format!(
        "select {STATE_COLUMNS} from update_state where id"
    ))
    .fetch_one(&mut *conn)
    .await?)
}

/// Settings edited in `/platform/updates`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsInput {
    /// `stable` / `beta`.
    pub channel: String,
    /// Daily automatic checks.
    pub auto_check: bool,
    /// Extra notification address.
    pub notify_email: Option<String>,
    /// Deploy webhook URL.
    pub webhook_url: Option<String>,
    /// `GET` / `POST`.
    pub webhook_method: String,
    /// `None` = keep, `Some(None)` = clear, `Some(Some(enc))` = replace.
    pub webhook_secret_enc: Option<Option<Vec<u8>>>,
}

/// Saves the settings.
pub async fn save_settings(db: impl PgExecutor<'_>, s: &SettingsInput) -> DbResult<()> {
    let (replace_secret, secret) = match &s.webhook_secret_enc {
        None => (false, None),
        Some(v) => (true, v.clone()),
    };
    sqlx::query(
        "update update_state set channel = $1, auto_check = $2, notify_email = $3::citext,
                webhook_url = $4, webhook_method = $5,
                webhook_secret_enc = case when $6 then $7 else webhook_secret_enc end,
                updated_at = now()
          where id",
    )
    .bind(&s.channel)
    .bind(s.auto_check)
    .bind(&s.notify_email)
    .bind(&s.webhook_url)
    .bind(&s.webhook_method)
    .bind(replace_secret)
    .bind(secret)
    .execute(db)
    .await?;
    Ok(())
}

/// Claims the scheduled check: `true` (and `next_check_at` := `next`) when
/// automatic checks are on and the check is due. Only one node wins.
pub async fn claim_scheduled_check(
    db: impl PgExecutor<'_>,
    now: DateTime<Utc>,
    next: DateTime<Utc>,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "update update_state set next_check_at = $2
          where id and auto_check and (next_check_at is null or next_check_at <= $1)
          returning true",
    )
    .bind(now)
    .bind(next)
    .fetch_optional(db)
    .await?
    .unwrap_or(false))
}

/// Records the outcome of a check.
pub async fn record_check(
    db: impl PgExecutor<'_>,
    error: Option<&str>,
    problems: &Json,
) -> DbResult<()> {
    sqlx::query(
        "update update_state set last_check_at = now(), last_check_error = $1,
                last_check_problems = $2, updated_at = now()
          where id",
    )
    .bind(error)
    .bind(problems)
    .execute(db)
    .await?;
    Ok(())
}

/// Remembers the newest version admins were notified about.
pub async fn set_notified_version(db: impl PgExecutor<'_>, version: &str) -> DbResult<()> {
    sqlx::query("update update_state set notified_version = $1 where id")
        .bind(version)
        .execute(db)
        .await?;
    Ok(())
}

/// Turns the cluster-wide maintenance mode on or off.
pub async fn set_maintenance(
    db: impl PgExecutor<'_>,
    on: bool,
    reason: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "update update_state set maintenance = $1,
                maintenance_reason = case when $1 then $2 else null end,
                maintenance_since = case when $1 then coalesce(maintenance_since, now()) else null end,
                updated_at = now()
          where id",
    )
    .bind(on)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// Whether maintenance mode is on.
pub async fn maintenance(db: impl PgExecutor<'_>) -> DbResult<bool> {
    Ok(
        sqlx::query_scalar::<_, bool>("select maintenance from update_state where id")
            .fetch_optional(db)
            .await?
            .unwrap_or(false),
    )
}

// ---------------------------------------------------------------------------
// releases
// ---------------------------------------------------------------------------

/// `update_releases` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ReleaseRow {
    pub version: String,
    pub channel: String,
    pub trust: String,
    pub trust_detail: Option<String>,
    pub severity: String,
    pub published_at: DateTime<Utc>,
    pub min_upgradable_from: Option<String>,
    pub migrations: bool,
    pub rollback_safe: bool,
    pub blocked: Option<String>,
    pub notes_md: String,
    pub container_image: Option<String>,
    pub release_url: Option<String>,
    pub manifest: String,
    pub signature: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Input of [`upsert_release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInput {
    pub version: String,
    pub channel: String,
    pub trust: String,
    pub trust_detail: Option<String>,
    pub severity: String,
    pub published_at: DateTime<Utc>,
    pub min_upgradable_from: Option<String>,
    pub migrations: bool,
    pub rollback_safe: bool,
    pub blocked: Option<String>,
    pub notes_md: String,
    pub container_image: Option<String>,
    pub release_url: Option<String>,
    pub manifest: String,
    pub signature: Option<String>,
}

/// Inserts or refreshes a release. Returns `true` when the version is new.
pub async fn upsert_release(db: impl PgExecutor<'_>, r: &ReleaseInput) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "insert into update_releases (version, channel, trust, trust_detail, severity, published_at,
                min_upgradable_from, migrations, rollback_safe, blocked, notes_md, container_image,
                release_url, manifest, signature)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         on conflict (version) do update set
                channel = excluded.channel, trust = excluded.trust, trust_detail = excluded.trust_detail,
                severity = excluded.severity, published_at = excluded.published_at,
                min_upgradable_from = excluded.min_upgradable_from, migrations = excluded.migrations,
                rollback_safe = excluded.rollback_safe, blocked = excluded.blocked,
                notes_md = excluded.notes_md, container_image = excluded.container_image,
                release_url = excluded.release_url, manifest = excluded.manifest,
                signature = excluded.signature, last_seen_at = now()
         returning (xmax = 0)",
    )
    .bind(&r.version)
    .bind(&r.channel)
    .bind(&r.trust)
    .bind(&r.trust_detail)
    .bind(&r.severity)
    .bind(r.published_at)
    .bind(&r.min_upgradable_from)
    .bind(r.migrations)
    .bind(r.rollback_safe)
    .bind(&r.blocked)
    .bind(&r.notes_md)
    .bind(&r.container_image)
    .bind(&r.release_url)
    .bind(&r.manifest)
    .bind(&r.signature)
    .fetch_one(db)
    .await?)
}

/// Every recorded release (newest first by discovery; callers sort by semver).
pub async fn list_releases(db: impl PgExecutor<'_>) -> DbResult<Vec<ReleaseRow>> {
    Ok(sqlx::query_as::<_, ReleaseRow>(
        "select * from update_releases order by published_at desc, version desc",
    )
    .fetch_all(db)
    .await?)
}

/// One release.
pub async fn get_release(db: impl PgExecutor<'_>, version: &str) -> DbResult<Option<ReleaseRow>> {
    Ok(
        sqlx::query_as::<_, ReleaseRow>("select * from update_releases where version = $1")
            .bind(version)
            .fetch_optional(db)
            .await?,
    )
}

/// Removes releases not in `keep` (pulled releases, versions now installed).
pub async fn retain_releases(db: impl PgExecutor<'_>, keep: &[String]) -> DbResult<u64> {
    Ok(
        sqlx::query("delete from update_releases where not (version = any($1))")
            .bind(keep)
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// `published_at` of the verified manifests seen so far (replay protection).
pub async fn seen_published(db: impl PgExecutor<'_>) -> DbResult<Vec<(String, DateTime<Utc>)>> {
    Ok(sqlx::query_as::<_, (String, DateTime<Utc>)>(
        "select version, published_at from update_releases where trust = 'verified'",
    )
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// runs
// ---------------------------------------------------------------------------

/// `update_runs` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct RunRow {
    pub id: Uuid,
    pub kind: String,
    pub from_version: String,
    pub to_version: String,
    pub status: String,
    pub step: Option<String>,
    pub message: Option<String>,
    pub log: Json,
    pub requested_by: Option<Uuid>,
    pub requested_by_email: Option<String>,
    pub node: Option<String>,
    pub backup: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Input of [`create_run`].
#[derive(Debug, Clone)]
pub struct NewRun<'a> {
    pub kind: &'a str,
    pub from_version: &'a str,
    pub to_version: &'a str,
    pub status: &'a str,
    pub requested_by: Option<PlatformAdminId>,
    pub requested_by_email: Option<&'a str>,
    pub node: &'a str,
}

/// Creates a run. `Conflict` while another run is active.
pub async fn create_run(db: impl PgExecutor<'_>, r: &NewRun<'_>) -> DbResult<RunRow> {
    sqlx::query_as::<_, RunRow>(
        "insert into update_runs (kind, from_version, to_version, status, requested_by,
                requested_by_email, node, finished_at)
         values ($1, $2, $3, $4, $5, $6, $7,
                 case when $4 in ('succeeded', 'failed', 'rolled_back') then now() end)
         returning *",
    )
    .bind(r.kind)
    .bind(r.from_version)
    .bind(r.to_version)
    .bind(r.status)
    .bind(r.requested_by.map(Uuid::from))
    .bind(r.requested_by_email)
    .bind(r.node)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("an active update run"))
}

/// Appends a progress entry and sets the current step.
pub async fn log_step(db: impl PgExecutor<'_>, id: Uuid, step: &str, detail: &str) -> DbResult<()> {
    let entry = serde_json::json!([{ "at": Utc::now(), "step": step, "detail": detail }]);
    sqlx::query(
        "update update_runs set step = $2, log = log || $3::jsonb, updated_at = now() where id = $1",
    )
    .bind(id)
    .bind(step)
    .bind(entry)
    .execute(db)
    .await?;
    Ok(())
}

/// Sets status + message (`finished_at` for final states).
pub async fn set_run_status(
    db: impl PgExecutor<'_>,
    id: Uuid,
    status: &str,
    message: Option<&str>,
) -> DbResult<bool> {
    Ok(sqlx::query(
        "update update_runs set status = $2, message = coalesce($3, message), updated_at = now(),
                finished_at = case when $2 in ('succeeded', 'failed', 'rolled_back')
                                   then coalesce(finished_at, now()) end
          where id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(message)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

/// Records the backup description.
pub async fn set_run_backup(db: impl PgExecutor<'_>, id: Uuid, backup: &str) -> DbResult<()> {
    sqlx::query("update update_runs set backup = $2, updated_at = now() where id = $1")
        .bind(id)
        .bind(backup)
        .execute(db)
        .await?;
    Ok(())
}

/// One run.
pub async fn get_run(db: impl PgExecutor<'_>, id: Uuid) -> DbResult<Option<RunRow>> {
    Ok(
        sqlx::query_as::<_, RunRow>("select * from update_runs where id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Latest runs.
pub async fn list_runs(db: impl PgExecutor<'_>, limit: i64) -> DbResult<Vec<RunRow>> {
    Ok(
        sqlx::query_as::<_, RunRow>("select * from update_runs order by created_at desc limit $1")
            .bind(limit)
            .fetch_all(db)
            .await?,
    )
}

/// The active run (`running` / `restarting`), if any.
pub async fn active_run(db: impl PgExecutor<'_>) -> DbResult<Option<RunRow>> {
    Ok(sqlx::query_as::<_, RunRow>(
        "select * from update_runs where status in ('running', 'restarting')
          order by created_at desc limit 1",
    )
    .fetch_optional(db)
    .await?)
}

/// Marks runs of `node` still `running` as failed (the process stopped
/// mid-way), and any `running` run older than `stale_before`.
pub async fn fail_interrupted_runs(
    db: impl PgExecutor<'_>,
    node: &str,
    stale_before: DateTime<Utc>,
) -> DbResult<u64> {
    Ok(sqlx::query(
        "update update_runs set status = 'failed', finished_at = now(), updated_at = now(),
                message = coalesce(message, 'the process stopped before the update finished')
          where status = 'running' and (node = $1 or updated_at < $2)",
    )
    .bind(node)
    .bind(stale_before)
    .execute(db)
    .await?
    .rows_affected())
}

/// A deploy-webhook run whose version is now running: succeeded.
pub async fn complete_requested_runs(db: impl PgExecutor<'_>, version: &str) -> DbResult<u64> {
    Ok(sqlx::query(
        "update update_runs set status = 'succeeded', finished_at = now(), updated_at = now()
          where status = 'requested' and to_version = $1",
    )
    .bind(version)
    .execute(db)
    .await?
    .rows_affected())
}

// ---------------------------------------------------------------------------
// endorsed keys
// ---------------------------------------------------------------------------

/// `update_trusted_keys` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EndorsedKeyRow {
    pub key_id: String,
    pub public_key: String,
    pub endorsed_by: String,
    pub endorsed_in: String,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Keys announced by verified manifests and not revoked.
pub async fn endorsed_keys(db: impl PgExecutor<'_>) -> DbResult<Vec<EndorsedKeyRow>> {
    Ok(sqlx::query_as::<_, EndorsedKeyRow>(
        "select key_id, public_key, endorsed_by, endorsed_in, revoked_at
           from update_trusted_keys where revoked_at is null order by created_at",
    )
    .fetch_all(db)
    .await?)
}

/// Stores an endorsed key (first endorsement wins).
pub async fn add_endorsed_key(
    db: impl PgExecutor<'_>,
    key_id: &str,
    public_key: &str,
    endorsed_by: &str,
    endorsed_in: &str,
) -> DbResult<()> {
    sqlx::query(
        "insert into update_trusted_keys (key_id, public_key, endorsed_by, endorsed_in)
         values ($1, $2, $3, $4) on conflict (key_id) do nothing",
    )
    .bind(key_id)
    .bind(public_key)
    .bind(endorsed_by)
    .bind(endorsed_in)
    .execute(db)
    .await?;
    Ok(())
}

/// Revokes an endorsed key.
pub async fn revoke_key(db: impl PgExecutor<'_>, key_id: &str) -> DbResult<()> {
    sqlx::query(
        "update update_trusted_keys set revoked_at = coalesce(revoked_at, now()) where key_id = $1",
    )
    .bind(key_id)
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// platform notifications
// ---------------------------------------------------------------------------

/// `platform_notifications` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PlatformNotificationRow {
    pub id: Uuid,
    pub kind: String,
    pub data: Json,
    pub link_url: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Creates a notification for every platform admin; `false` when
/// `dedupe_key` already exists.
pub async fn notify_platform(
    db: impl PgExecutor<'_>,
    kind: &str,
    data: &Json,
    link_url: Option<&str>,
    dedupe_key: Option<&str>,
) -> DbResult<bool> {
    Ok(sqlx::query(
        "insert into platform_notifications (kind, data, link_url, dedupe_key)
         values ($1, $2, $3, $4) on conflict (dedupe_key) do nothing",
    )
    .bind(kind)
    .bind(data)
    .bind(link_url)
    .bind(dedupe_key)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

/// Unread notifications of an admin, newest first.
pub async fn unread_notifications(
    db: impl PgExecutor<'_>,
    admin: PlatformAdminId,
    limit: i64,
) -> DbResult<Vec<PlatformNotificationRow>> {
    Ok(sqlx::query_as::<_, PlatformNotificationRow>(
        "select n.id, n.kind, n.data, n.link_url, n.created_at
           from platform_notifications n
          where not exists (select 1 from platform_notification_reads r
                             where r.notification_id = n.id and r.platform_admin_id = $1)
            and n.created_at > now() - interval '90 days'
          order by n.created_at desc limit $2",
    )
    .bind(Uuid::from(admin))
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Marks one notification read for an admin.
pub async fn mark_notification_read(
    db: impl PgExecutor<'_>,
    id: Uuid,
    admin: PlatformAdminId,
) -> DbResult<()> {
    sqlx::query(
        "insert into platform_notification_reads (notification_id, platform_admin_id)
         select id, $2 from platform_notifications where id = $1
         on conflict do nothing",
    )
    .bind(id)
    .bind(Uuid::from(admin))
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// locks, jobs
// ---------------------------------------------------------------------------

/// Takes the cluster-wide update lock on this connection (session level:
/// released when the connection closes). `false` when another session holds it.
pub async fn try_update_lock(conn: &mut PgConnection) -> DbResult<bool> {
    Ok(
        sqlx::query_scalar::<_, bool>("select pg_try_advisory_lock($1)")
            .bind(UPDATE_LOCK_KEY)
            .fetch_one(conn)
            .await?,
    )
}

/// Releases the update lock held by this connection.
pub async fn release_update_lock(conn: &mut PgConnection) -> DbResult<()> {
    sqlx::query("select pg_advisory_unlock($1)")
        .bind(UPDATE_LOCK_KEY)
        .execute(conn)
        .await?;
    Ok(())
}

/// Versions of the successfully applied migrations.
pub async fn applied_migrations(db: impl PgExecutor<'_>) -> DbResult<Vec<i64>> {
    Ok(
        sqlx::query_scalar::<_, i64>("select version from _sqlx_migrations where success")
            .fetch_all(db)
            .await?,
    )
}

/// `select 1`.
pub async fn ping(db: impl PgExecutor<'_>) -> DbResult<()> {
    sqlx::query_scalar::<_, i32>("select 1")
        .fetch_one(db)
        .await?;
    Ok(())
}

/// Jobs currently running (any tenant).
pub async fn running_jobs(db: impl PgExecutor<'_>) -> DbResult<i64> {
    Ok(
        sqlx::query_scalar::<_, i64>("select count(*) from jobs where status = 'running'")
            .fetch_one(db)
            .await?,
    )
}
