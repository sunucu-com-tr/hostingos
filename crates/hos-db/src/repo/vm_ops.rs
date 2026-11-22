//! VPS management (migration 0050): console relay sessions, VM operations
//! with progress, client SSH keys and the staff-approved ISO list.
//! Every function is tenant-scoped.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use hos_core::ids::{ClientId, IntegrationId, ServiceId, TenantId, UserId};
use serde_json::Value as Json;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use crate::error::DbResult;

// ---------------------------------------------------------------------------
// console sessions
// ---------------------------------------------------------------------------

/// `vm_console_sessions` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ConsoleSessionRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub service_id: ServiceId,
    pub user_id: UserId,
    pub is_staff: bool,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub connected_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub close_reason: Option<String>,
}

const SESSION_COLS: &str = "id, tenant_id, service_id, user_id, is_staff, ip, user_agent, \
    issued_at, expires_at, connected_at, ended_at, bytes_in, bytes_out, close_reason";

/// A new console session (the token itself is never stored).
#[derive(Debug, Clone)]
pub struct NewConsoleSession {
    pub service_id: ServiceId,
    pub user_id: UserId,
    pub is_staff: bool,
    pub token_hash: Vec<u8>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub ttl_secs: i64,
}

/// Inserts a session (token valid for `ttl_secs`).
pub async fn create_console_session(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    s: &NewConsoleSession,
) -> DbResult<ConsoleSessionRow> {
    let sql = format!(
        "insert into vm_console_sessions
             (tenant_id, service_id, user_id, is_staff, token_hash, ip, user_agent, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(secs => $8))
         returning {SESSION_COLS}"
    );
    Ok(sqlx::query_as::<_, ConsoleSessionRow>(&sql)
        .bind(tenant_id)
        .bind(s.service_id)
        .bind(s.user_id)
        .bind(s.is_staff)
        .bind(&s.token_hash)
        .bind(s.ip)
        .bind(
            s.user_agent
                .as_deref()
                .map(|u| u.chars().take(300).collect::<String>()),
        )
        .bind(s.ttl_secs.clamp(1, 600) as f64)
        .fetch_one(db)
        .await?)
}

/// Result of [`claim_console_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleClaim {
    /// The token was valid; the session is now connected.
    Claimed(ConsoleSessionRow),
    /// Unknown, used or expired token.
    Invalid,
    /// The user already has `cap` open sessions; this one was closed.
    CapReached(ConsoleSessionRow),
}

/// Consumes a token (single use) and marks the session connected, unless the
/// user already has `cap` sessions open (connected within `max_age_secs` and
/// not ended). Runs in one transaction with a per-user advisory lock, so
/// concurrent connects cannot both slip under the cap.
pub async fn claim_console_session(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    token_hash: &[u8],
    cap: i64,
    max_age_secs: i64,
) -> DbResult<ConsoleClaim> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let sql = format!(
        "update vm_console_sessions set connected_at = now()
          where tenant_id = $1 and token_hash = $2 and connected_at is null
            and ended_at is null and expires_at > now()
          returning {SESSION_COLS}"
    );
    let Some(row) = sqlx::query_as::<_, ConsoleSessionRow>(&sql)
        .bind(tenant_id)
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?
    else {
        tx.commit().await?;
        return Ok(ConsoleClaim::Invalid);
    };
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1::text, 7150))")
        .bind(row.user_id)
        .execute(&mut *tx)
        .await?;
    let open: i64 = sqlx::query_scalar(
        "select count(*) from vm_console_sessions
          where tenant_id = $1 and user_id = $2 and id <> $3
            and connected_at is not null and ended_at is null
            and connected_at > now() - make_interval(secs => $4)",
    )
    .bind(tenant_id)
    .bind(row.user_id)
    .bind(row.id)
    .bind(max_age_secs as f64)
    .fetch_one(&mut *tx)
    .await?;
    if open >= cap {
        let sql = format!(
            "update vm_console_sessions set ended_at = now(), close_reason = 'session_cap'
              where tenant_id = $1 and id = $2 returning {SESSION_COLS}"
        );
        let closed = sqlx::query_as::<_, ConsoleSessionRow>(&sql)
            .bind(tenant_id)
            .bind(row.id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(ConsoleClaim::CapReached(closed));
    }
    tx.commit().await?;
    Ok(ConsoleClaim::Claimed(row))
}

/// Ends a session (idempotent: the first end wins).
pub async fn end_console_session(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    bytes_in: i64,
    bytes_out: i64,
    reason: &str,
) -> DbResult<()> {
    sqlx::query(
        "update vm_console_sessions
            set ended_at = coalesce(ended_at, now()), bytes_in = $3, bytes_out = $4,
                close_reason = coalesce(close_reason, $5)
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(bytes_in)
    .bind(bytes_out)
    .bind(reason.chars().take(64).collect::<String>())
    .execute(db)
    .await?;
    Ok(())
}

/// Latest console sessions of a service (audit view).
pub async fn console_sessions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    limit: i64,
) -> DbResult<Vec<ConsoleSessionRow>> {
    let sql = format!(
        "select {SESSION_COLS} from vm_console_sessions
          where tenant_id = $1 and service_id = $2
          order by issued_at desc limit $3"
    );
    Ok(sqlx::query_as::<_, ConsoleSessionRow>(&sql)
        .bind(tenant_id)
        .bind(service_id)
        .bind(limit.clamp(1, 200))
        .fetch_all(db)
        .await?)
}

// ---------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------

/// `vm_operations` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct VmOperationRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub service_id: ServiceId,
    pub kind: String,
    pub status: String,
    pub progress: Option<i16>,
    pub detail: String,
    pub task: Option<Json>,
    pub error: Option<String>,
    pub actor_kind: Option<String>,
    pub actor_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl VmOperationRow {
    /// Still queued or running.
    pub fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }
}

/// A new operation.
#[derive(Debug, Clone)]
pub struct NewVmOperation<'a> {
    pub service_id: ServiceId,
    pub kind: &'a str,
    pub status: &'a str,
    pub detail: &'a str,
    pub task: Option<Json>,
    pub error: Option<&'a str>,
    pub actor_kind: Option<&'a str>,
    pub actor_id: Option<Uuid>,
}

/// Inserts an operation; `finished_at` is set for final states.
pub async fn create_operation(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    op: &NewVmOperation<'_>,
) -> DbResult<VmOperationRow> {
    Ok(sqlx::query_as::<_, VmOperationRow>(
        "insert into vm_operations
             (tenant_id, service_id, kind, status, detail, task, error, actor_kind, actor_id,
              progress, finished_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                 case when $4 = 'succeeded' then 100 end,
                 case when $4 in ('succeeded', 'failed') then now() end)
         returning *",
    )
    .bind(tenant_id)
    .bind(op.service_id)
    .bind(op.kind)
    .bind(op.status)
    .bind(op.detail.chars().take(200).collect::<String>())
    .bind(&op.task)
    .bind(op.error.map(|e| e.chars().take(500).collect::<String>()))
    .bind(op.actor_kind)
    .bind(op.actor_id)
    .fetch_one(db)
    .await?)
}

/// Updates progress / final state of an operation.
pub async fn update_operation(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    status: &str,
    progress: Option<i16>,
    error: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "update vm_operations
            set status = $3,
                progress = coalesce($4, case when $3 = 'succeeded' then 100 else progress end),
                error = coalesce($5, error),
                finished_at = case when $3 in ('succeeded', 'failed') then coalesce(finished_at, now()) end
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(progress.map(|p| p.clamp(0, 100)))
    .bind(error.map(|e| e.chars().take(500).collect::<String>()))
    .execute(db)
    .await?;
    Ok(())
}

/// Latest operations of a service.
pub async fn operations(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    limit: i64,
) -> DbResult<Vec<VmOperationRow>> {
    Ok(sqlx::query_as::<_, VmOperationRow>(
        "select * from vm_operations where tenant_id = $1 and service_id = $2
          order by created_at desc limit $3",
    )
    .bind(tenant_id)
    .bind(service_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(db)
    .await?)
}

/// Queued / running operations of a service started within the last two
/// hours (an operation whose poll job died does not block the VM forever).
pub async fn open_operations(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<Vec<VmOperationRow>> {
    Ok(sqlx::query_as::<_, VmOperationRow>(
        "select * from vm_operations where tenant_id = $1 and service_id = $2
            and status in ('queued', 'running') and created_at > now() - interval '2 hours'
          order by created_at",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_all(db)
    .await?)
}

/// One operation.
pub async fn get_operation(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<Option<VmOperationRow>> {
    Ok(sqlx::query_as::<_, VmOperationRow>(
        "select * from vm_operations where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Operations of `kind` on a service since `since` (rate limits).
pub async fn count_operations_since(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    kind: &str,
    since: DateTime<Utc>,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from vm_operations
          where tenant_id = $1 and service_id = $2 and kind = $3 and created_at >= $4",
    )
    .bind(tenant_id)
    .bind(service_id)
    .bind(kind)
    .bind(since)
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// SSH keys
// ---------------------------------------------------------------------------

/// `client_ssh_keys` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SshKeyRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub name: String,
    pub public_key: String,
    pub fingerprint: String,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

/// Keys of a client, newest first.
pub async fn ssh_keys(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<SshKeyRow>> {
    Ok(sqlx::query_as::<_, SshKeyRow>(
        "select * from client_ssh_keys where tenant_id = $1 and client_id = $2
          order by created_at desc",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// Adds a key; `None` when the client already has a key with this fingerprint.
pub async fn add_ssh_key(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    name: &str,
    public_key: &str,
    fingerprint: &str,
    created_by: Option<UserId>,
) -> DbResult<Option<SshKeyRow>> {
    Ok(sqlx::query_as::<_, SshKeyRow>(
        "insert into client_ssh_keys (tenant_id, client_id, name, public_key, fingerprint, created_by)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (tenant_id, client_id, fingerprint) do nothing
         returning *",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(name)
    .bind(public_key)
    .bind(fingerprint)
    .bind(created_by)
    .fetch_optional(db)
    .await?)
}

/// Deletes a key of `client_id` (`false`: no such key of that client).
pub async fn delete_ssh_key(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    id: Uuid,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from client_ssh_keys where tenant_id = $1 and client_id = $2 and id = $3",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// The keys among `ids` that belong to `client_id` (others are ignored).
pub async fn ssh_keys_by_ids(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    ids: &[Uuid],
) -> DbResult<Vec<SshKeyRow>> {
    Ok(sqlx::query_as::<_, SshKeyRow>(
        "select * from client_ssh_keys where tenant_id = $1 and client_id = $2 and id = any($3)
          order by created_at",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(ids)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// ISO allow-list
// ---------------------------------------------------------------------------

/// `vm_iso_images` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct IsoRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub integration_id: IntegrationId,
    pub iso_ref: String,
    pub name: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

/// Approved ISOs of an integration (`active_only`: what clients may mount).
pub async fn isos(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    active_only: bool,
) -> DbResult<Vec<IsoRow>> {
    Ok(sqlx::query_as::<_, IsoRow>(
        "select * from vm_iso_images where tenant_id = $1 and integration_id = $2
            and (is_active or not $3)
          order by name",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .bind(active_only)
    .fetch_all(db)
    .await?)
}

/// Approves an ISO (or renames / re-activates an approved one).
pub async fn upsert_iso(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    iso_ref: &str,
    name: &str,
) -> DbResult<IsoRow> {
    Ok(sqlx::query_as::<_, IsoRow>(
        "insert into vm_iso_images (tenant_id, integration_id, iso_ref, name)
         values ($1, $2, $3, $4)
         on conflict (tenant_id, integration_id, iso_ref)
           do update set name = excluded.name, is_active = true
         returning *",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .bind(iso_ref)
    .bind(name)
    .fetch_one(db)
    .await?)
}

/// Removes an ISO from the approved list.
pub async fn delete_iso(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    id: Uuid,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from vm_iso_images where tenant_id = $1 and integration_id = $2 and id = $3",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// IP addresses (rDNS kept in the IP-pool model)
// ---------------------------------------------------------------------------

/// Sets the PTR stored for an address **assigned to `service_id`**
/// (`false`: not such an address).
pub async fn set_service_ip_ptr(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    address: IpAddr,
    ptr: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update ip_addresses set ptr = $4, updated_at = now()
          where tenant_id = $1 and service_id = $2 and address = $3",
    )
    .bind(tenant_id)
    .bind(service_id)
    .bind(address)
    .bind(ptr)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Forgets the stored (encrypted) root / panel password of a service and its
/// VM — after a password reset it is stale, and the new one is shown once.
pub async fn clear_stored_vm_password(
    db: &mut PgConnection,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<()> {
    let mut tx = sqlx::Connection::begin(&mut *db).await?;
    sqlx::query("update services set password_enc = null where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(service_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update vms set root_password_enc = null where tenant_id = $1 and service_id = $2")
        .bind(tenant_id)
        .bind(service_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
