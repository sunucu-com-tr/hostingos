//! E-mail piping into tickets (`mail_inbound_settings`,
//! `ticket_reply_tokens`, `mail_inbound_messages`; migration 0044; sender
//! authentication settings and the Mailgun replay set `mail_inbound_replay`:
//! migration 0051).

use chrono::{DateTime, Utc};
use hos_core::ids::{TenantId, TicketDepartmentId, TicketId, UserId};
use hos_core::string_enum;
use sqlx::{FromRow, PgExecutor};

use crate::error::DbResult;

hos_core::define_id! {
    /// `mail_inbound_messages`.
    pub struct InboundMessageId;
}

string_enum! {
    /// What happens to mail from an address that belongs to no client.
    pub enum UnknownSenders {
        /// Kept for staff review (default).
        Quarantine => "quarantine",
        /// Opens a guest ticket.
        Guest => "guest",
        /// Dropped (logged).
        Reject => "reject",
    }
}

string_enum! {
    /// `mail_inbound_messages.source`.
    pub enum InboundSource {
        Mailgun => "mailgun",
        Raw => "raw",
    }
}

string_enum! {
    /// `mail_inbound_messages.status`.
    pub enum InboundStatus {
        TicketCreated => "ticket_created",
        ReplyAdded => "reply_added",
        Quarantined => "quarantined",
        Rejected => "rejected",
        /// Quarantined, then turned into a ticket by staff.
        Released => "released",
        /// Quarantined, then dropped by staff.
        Discarded => "discarded",
    }
}

/// `mail_inbound_settings` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct InboundSettingsRow {
    pub tenant_id: TenantId,
    pub enabled: bool,
    pub mailgun_key_enc: Option<Vec<u8>>,
    pub raw_token_hash: Option<String>,
    pub unknown_senders: UnknownSenders,
    pub default_department_id: Option<TicketDepartmentId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// authserv-id whose `Authentication-Results` header (raw endpoint) is
    /// trusted; `None`: no raw-endpoint sender is authenticated.
    pub trusted_authserv_id: Option<String>,
    /// Trust Mailgun's `X-Mailgun-Spf` header (the Mailgun domain marks
    /// spam with MIME headers).
    pub mailgun_trust_spf: bool,
}

/// The settings of a tenant (`None`: never saved, i.e. disabled).
pub async fn settings(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Option<InboundSettingsRow>> {
    Ok(sqlx::query_as::<_, InboundSettingsRow>(
        "select * from mail_inbound_settings where tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?)
}

/// Saves the switch and routing (keys untouched).
pub async fn save_settings(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    enabled: bool,
    unknown_senders: UnknownSenders,
    default_department_id: Option<TicketDepartmentId>,
) -> DbResult<()> {
    sqlx::query(
        "insert into mail_inbound_settings (tenant_id, enabled, unknown_senders, default_department_id)
         values ($1, $2, $3, $4)
         on conflict (tenant_id) do update set enabled = excluded.enabled,
                unknown_senders = excluded.unknown_senders,
                default_department_id = excluded.default_department_id",
    )
    .bind(tenant_id)
    .bind(enabled)
    .bind(unknown_senders)
    .bind(default_department_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Saves how senders are authenticated (see migration 0051).
pub async fn save_sender_auth(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    trusted_authserv_id: Option<&str>,
    mailgun_trust_spf: bool,
) -> DbResult<()> {
    sqlx::query(
        "insert into mail_inbound_settings (tenant_id, trusted_authserv_id, mailgun_trust_spf)
         values ($1, $2, $3)
         on conflict (tenant_id) do update set trusted_authserv_id = excluded.trusted_authserv_id,
                mailgun_trust_spf = excluded.mailgun_trust_spf",
    )
    .bind(tenant_id)
    .bind(trusted_authserv_id)
    .bind(mailgun_trust_spf)
    .execute(db)
    .await?;
    Ok(())
}

/// Claims a Mailgun webhook token (its SHA-256) until `expires_at`:
/// `false` when it was already honoured (a replayed request). Expired
/// claims of the tenant are pruned on the way.
pub async fn claim_replay_token(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> DbResult<bool> {
    sqlx::query("delete from mail_inbound_replay where tenant_id = $1 and expires_at < now()")
        .bind(tenant_id)
        .execute(&mut *conn)
        .await?;
    let res = sqlx::query(
        "insert into mail_inbound_replay (tenant_id, token_hash, expires_at) values ($1, $2, $3)
         on conflict (tenant_id, token_hash) do nothing",
    )
    .bind(tenant_id)
    .bind(token_hash)
    .bind(expires_at)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Gives a claimed token back (processing failed: Mailgun's retry must be
/// accepted).
pub async fn release_replay_token(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    token_hash: &str,
) -> DbResult<()> {
    sqlx::query("delete from mail_inbound_replay where tenant_id = $1 and token_hash = $2")
        .bind(tenant_id)
        .bind(token_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Stores (or clears) the sealed Mailgun webhook signing key.
pub async fn set_mailgun_key(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    sealed: Option<&[u8]>,
) -> DbResult<()> {
    sqlx::query(
        "insert into mail_inbound_settings (tenant_id, mailgun_key_enc) values ($1, $2)
         on conflict (tenant_id) do update set mailgun_key_enc = excluded.mailgun_key_enc",
    )
    .bind(tenant_id)
    .bind(sealed)
    .execute(db)
    .await?;
    Ok(())
}

/// Stores (or clears) the SHA-256 of the raw endpoint's bearer token.
pub async fn set_raw_token_hash(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    hash: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "insert into mail_inbound_settings (tenant_id, raw_token_hash) values ($1, $2)
         on conflict (tenant_id) do update set raw_token_hash = excluded.raw_token_hash",
    )
    .bind(tenant_id)
    .bind(hash)
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reply tokens
// ---------------------------------------------------------------------------

/// Records a reply token (its SHA-256) sent to `recipient` for a ticket.
pub async fn create_reply_token(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
    token_hash: &str,
    recipient: &str,
    expires_at: DateTime<Utc>,
) -> DbResult<()> {
    sqlx::query(
        "insert into ticket_reply_tokens (tenant_id, ticket_id, token_hash, recipient, expires_at)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(ticket_id)
    .bind(token_hash)
    .bind(recipient)
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok(())
}

/// The ticket of an unexpired token of this tenant, with the address the
/// token was sent to.
pub async fn ticket_for_token(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    token_hash: &str,
    now: DateTime<Utc>,
) -> DbResult<Option<(TicketId, Option<String>)>> {
    Ok(sqlx::query_as::<_, (TicketId, Option<String>)>(
        "select ticket_id, recipient::text from ticket_reply_tokens
          where tenant_id = $1 and token_hash = $2 and expires_at > $3",
    )
    .bind(tenant_id)
    .bind(token_hash)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

/// Deletes expired tokens (all tenants); returns how many.
pub async fn prune_reply_tokens(db: impl PgExecutor<'_>, now: DateTime<Utc>) -> DbResult<u64> {
    Ok(
        sqlx::query("delete from ticket_reply_tokens where expires_at <= $1")
            .bind(now)
            .execute(db)
            .await?
            .rows_affected(),
    )
}

// ---------------------------------------------------------------------------
// Message log / quarantine
// ---------------------------------------------------------------------------

/// `mail_inbound_messages` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct InboundMessageRow {
    pub id: InboundMessageId,
    pub tenant_id: TenantId,
    pub source: InboundSource,
    pub message_id: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub subject: Option<String>,
    pub status: InboundStatus,
    pub reason: Option<String>,
    pub ticket_id: Option<TicketId>,
    pub body_text: Option<String>,
    pub attachment_names: Vec<String>,
    pub size_bytes: i32,
    pub reviewed_by: Option<UserId>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Whether sender authentication passed for the From domain.
    pub sender_verified: Option<bool>,
}

const COLS: &str = "id, tenant_id, source, message_id, sender::text as sender,
        recipient::text as recipient, subject, status, reason, ticket_id, body_text,
        attachment_names, size_bytes, reviewed_by, reviewed_at, created_at, updated_at,
        sender_verified";

/// A message to log (body only kept for quarantined mail).
#[derive(Debug, Clone)]
pub struct NewInbound {
    pub source: InboundSource,
    pub message_id: Option<String>,
    pub sender: Option<String>,
    pub recipient: Option<String>,
    pub subject: Option<String>,
    pub status: InboundStatus,
    pub reason: Option<String>,
    pub ticket_id: Option<TicketId>,
    pub body_text: Option<String>,
    pub attachment_names: Vec<String>,
    pub size_bytes: i32,
    /// Whether sender authentication passed (`None`: not determined).
    pub sender_verified: Option<bool>,
}

/// Logs a message. `None`: a message with this Message-ID was already
/// logged for the tenant (duplicate / replay).
pub async fn record(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    m: &NewInbound,
) -> DbResult<Option<InboundMessageId>> {
    Ok(sqlx::query_scalar::<_, InboundMessageId>(
        "insert into mail_inbound_messages (id, tenant_id, source, message_id, sender, recipient, subject,
                                            status, reason, ticket_id, body_text, attachment_names, size_bytes,
                                            sender_verified)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         on conflict (tenant_id, message_id) where message_id is not null do nothing
         returning id",
    )
    .bind(InboundMessageId::new())
    .bind(tenant_id)
    .bind(m.source)
    .bind(&m.message_id)
    .bind(&m.sender)
    .bind(&m.recipient)
    .bind(&m.subject)
    .bind(m.status)
    .bind(&m.reason)
    .bind(m.ticket_id)
    .bind(&m.body_text)
    .bind(&m.attachment_names)
    .bind(m.size_bytes)
    .bind(m.sender_verified)
    .fetch_optional(db)
    .await?)
}

/// Whether a message with this Message-ID was already logged.
pub async fn seen(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    message_id: &str,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from mail_inbound_messages where tenant_id = $1 and message_id = $2)",
    )
    .bind(tenant_id)
    .bind(message_id)
    .fetch_one(db)
    .await?)
}

/// Messages from `sender` logged since `since` (flood / loop guard).
pub async fn count_from(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    sender: &str,
    since: DateTime<Utc>,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from mail_inbound_messages
          where tenant_id = $1 and sender = $2::citext and created_at >= $3",
    )
    .bind(tenant_id)
    .bind(sender)
    .bind(since)
    .fetch_one(db)
    .await?)
}

/// Latest messages, optionally of one status.
pub async fn list(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    status: Option<InboundStatus>,
    limit: i64,
) -> DbResult<Vec<InboundMessageRow>> {
    Ok(sqlx::query_as::<_, InboundMessageRow>(&format!(
        "select {COLS} from mail_inbound_messages
          where tenant_id = $1 and ($2::text is null or status = $2)
          order by created_at desc limit $3"
    ))
    .bind(tenant_id)
    .bind(status)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// One message.
pub async fn get(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InboundMessageId,
) -> DbResult<Option<InboundMessageRow>> {
    Ok(sqlx::query_as::<_, InboundMessageRow>(&format!(
        "select {COLS} from mail_inbound_messages where tenant_id = $1 and id = $2"
    ))
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Closes a quarantined message (`released`, or `discarded`: the stored
/// body is dropped; a released one drops it in [`set_ticket`]). `false`: not quarantined
/// (already reviewed, or unknown).
pub async fn review(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InboundMessageId,
    status: InboundStatus,
    ticket_id: Option<TicketId>,
    reviewed_by: UserId,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update mail_inbound_messages
            set status = $3, ticket_id = $4, reviewed_by = $5, reviewed_at = $6,
                body_text = case when $3 = 'discarded' then null else body_text end
          where tenant_id = $1 and id = $2 and status = 'quarantined'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(ticket_id)
    .bind(reviewed_by)
    .bind(now)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Counts per status (admin tabs).
pub async fn status_counts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(InboundStatus, i64)>> {
    Ok(sqlx::query_as::<_, (InboundStatus, i64)>(
        "select status, count(*) from mail_inbound_messages where tenant_id = $1 group by status",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// Sender matching
// ---------------------------------------------------------------------------

/// Client accounts (not closed) that the active client user with this
/// e-mail may open tickets for (owner, or a member with the `tickets`
/// area or no area limits), owner accounts first: `(client, user)`.
pub async fn ticket_accounts_of(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    email: &str,
) -> DbResult<Vec<(hos_core::ids::ClientId, UserId)>> {
    Ok(sqlx::query_as::<_, (hos_core::ids::ClientId, UserId)>(
        "select cu.client_id, u.id
           from users u
           join client_users cu on cu.tenant_id = u.tenant_id and cu.user_id = u.id
           join clients c on c.tenant_id = cu.tenant_id and c.id = cu.client_id
          where u.tenant_id = $1 and u.email = $2::citext and u.kind = 'client'
            and u.status = 'active' and c.status <> 'closed'
            and (cu.is_owner or cardinality(cu.permissions) = 0 or 'tickets' = any(cu.permissions))
          order by cu.is_owner desc, c.created_at",
    )
    .bind(tenant_id)
    .bind(email)
    .fetch_all(db)
    .await?)
}

/// A client (not closed) whose account e-mail is `email`.
pub async fn client_by_email(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    email: &str,
) -> DbResult<Option<hos_core::ids::ClientId>> {
    Ok(sqlx::query_scalar::<_, hos_core::ids::ClientId>(
        "select id from clients where tenant_id = $1 and email = $2::citext and status <> 'closed'
          order by created_at limit 1",
    )
    .bind(tenant_id)
    .bind(email)
    .fetch_optional(db)
    .await?)
}

/// Links a released message to the ticket made from it (its text now
/// lives in the ticket).
pub async fn set_ticket(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InboundMessageId,
    ticket_id: TicketId,
) -> DbResult<()> {
    sqlx::query(
        "update mail_inbound_messages set ticket_id = $3, body_text = null
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(ticket_id)
    .execute(db)
    .await?;
    Ok(())
}
