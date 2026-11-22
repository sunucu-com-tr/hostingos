//! Support desk: departments, tickets, messages, attachments and canned responses.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use hos_core::CoreError;
use hos_core::domain::{Locale, TicketPriority, TicketStatus};
use hos_core::ids::{
    ClientId, DomainId, ServiceId, TenantId, TicketDepartmentId, TicketId, TicketMessageId, UserId,
};
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};

use super::{Json, empty_object};
use crate::enums::{AuthorKind, MessageSource};
use crate::error::{DbResult, conflict_on_unique, like_contains, not_found};
use crate::ids::{CannedResponseId, TicketAttachmentId};
use crate::page::{Page, Paged, fetch_paged};

// ---------------------------------------------------------------------------
// departments
// ---------------------------------------------------------------------------

/// `ticket_departments` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TicketDepartmentRow {
    pub id: TicketDepartmentId,
    pub tenant_id: TenantId,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub translations: Json,
    pub email: Option<String>,
    pub clients_only: bool,
    pub auto_respond: bool,
    pub is_hidden: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a department.
#[allow(clippy::too_many_arguments)]
pub async fn create_department(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
    name: &str,
    description: Option<&str>,
    translations: &Json,
    email: Option<&str>,
    sort_order: i32,
) -> DbResult<TicketDepartmentRow> {
    sqlx::query_as::<_, TicketDepartmentRow>(
        "insert into ticket_departments (id, tenant_id, slug, name, description, translations, email, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning *",
    )
    .bind(TicketDepartmentId::new())
    .bind(tenant_id)
    .bind(slug)
    .bind(name)
    .bind(description)
    .bind(translations)
    .bind(email)
    .bind(sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("department"))
}

/// Departments in display order.
pub async fn list_departments(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    include_hidden: bool,
) -> DbResult<Vec<TicketDepartmentRow>> {
    Ok(sqlx::query_as::<_, TicketDepartmentRow>(
        "select * from ticket_departments where tenant_id = $1 and ($2 or not is_hidden) order by sort_order, name",
    )
    .bind(tenant_id)
    .bind(include_hidden)
    .fetch_all(db)
    .await?)
}

/// Department by id.
pub async fn get_department(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TicketDepartmentId,
) -> DbResult<Option<TicketDepartmentRow>> {
    Ok(sqlx::query_as::<_, TicketDepartmentRow>(
        "select * from ticket_departments where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Every field of a department (admin create / edit).
#[derive(Debug, Clone, Default)]
pub struct DepartmentInput {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    /// `{"en": {"name": .., "description": ..}}`
    pub translations: Json,
    pub email: Option<String>,
    pub clients_only: bool,
    pub auto_respond: bool,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Creates a department with every field.
pub async fn create_department_full(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    d: &DepartmentInput,
) -> DbResult<TicketDepartmentRow> {
    sqlx::query_as::<_, TicketDepartmentRow>(
        "insert into ticket_departments (id, tenant_id, slug, name, description, translations, email,
                                         clients_only, auto_respond, is_hidden, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning *",
    )
    .bind(TicketDepartmentId::new())
    .bind(tenant_id)
    .bind(&d.slug)
    .bind(&d.name)
    .bind(&d.description)
    .bind(&d.translations)
    .bind(&d.email)
    .bind(d.clients_only)
    .bind(d.auto_respond)
    .bind(d.is_hidden)
    .bind(d.sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("department"))
}

/// Updates a department.
pub async fn update_department(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TicketDepartmentId,
    d: &DepartmentInput,
) -> DbResult<Option<TicketDepartmentRow>> {
    sqlx::query_as::<_, TicketDepartmentRow>(
        "update ticket_departments set slug = $3, name = $4, description = $5, translations = $6, email = $7,
                clients_only = $8, auto_respond = $9, is_hidden = $10, sort_order = $11
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&d.slug)
    .bind(&d.name)
    .bind(&d.description)
    .bind(&d.translations)
    .bind(&d.email)
    .bind(d.clients_only)
    .bind(d.auto_respond)
    .bind(d.is_hidden)
    .bind(d.sort_order)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("department"))
}

/// Number of tickets per department (admin list; departments with tickets
/// cannot be deleted).
pub async fn department_ticket_counts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(TicketDepartmentId, i64)>> {
    Ok(sqlx::query_as::<_, (TicketDepartmentId, i64)>(
        "select department_id, count(*) from tickets where tenant_id = $1 group by department_id",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Deletes a department without tickets (`Ok(false)` when it has tickets or
/// does not exist).
pub async fn delete_department(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TicketDepartmentId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from ticket_departments d where d.tenant_id = $1 and d.id = $2
           and not exists (select 1 from tickets t where t.department_id = d.id)",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Department by slug.
pub async fn department_by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
) -> DbResult<Option<TicketDepartmentRow>> {
    Ok(sqlx::query_as::<_, TicketDepartmentRow>(
        "select * from ticket_departments where tenant_id = $1 and slug = $2",
    )
    .bind(tenant_id)
    .bind(slug)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// tickets & messages
// ---------------------------------------------------------------------------

/// `tickets` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TicketRow {
    pub id: TicketId,
    pub tenant_id: TenantId,
    pub number: String,
    pub department_id: TicketDepartmentId,
    pub client_id: Option<ClientId>,
    pub user_id: Option<UserId>,
    pub guest_name: Option<String>,
    pub guest_email: Option<String>,
    pub subject: String,
    pub status: TicketStatus,
    pub priority: TicketPriority,
    pub service_id: Option<ServiceId>,
    pub domain_id: Option<DomainId>,
    pub assigned_to: Option<UserId>,
    pub last_reply_at: DateTime<Utc>,
    pub last_reply_by: AuthorKind,
    pub closed_at: Option<DateTime<Utc>>,
    pub rating: Option<i16>,
    pub is_flagged: bool,
    pub tags: Vec<String>,
    pub cc_emails: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `ticket_messages` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TicketMessageRow {
    pub id: TicketMessageId,
    pub tenant_id: TenantId,
    pub ticket_id: TicketId,
    pub author_kind: AuthorKind,
    pub user_id: Option<UserId>,
    pub author_name: String,
    pub author_email: Option<String>,
    pub body: String,
    pub is_internal: bool,
    pub source: MessageSource,
    pub message_id: Option<String>,
    pub ip: Option<IpAddr>,
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// E-mail messages: whether sender authentication passed for the From
    /// domain (`None`: not an e-mail, or not recorded).
    pub sender_verified: Option<bool>,
}

/// Records whether the sender of an e-mail message was authenticated.
pub async fn set_message_sender_verified(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    message_id: TicketMessageId,
    verified: bool,
) -> DbResult<()> {
    sqlx::query("update ticket_messages set sender_verified = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(message_id)
        .bind(verified)
        .execute(db)
        .await?;
    Ok(())
}

/// A message to add to a ticket.
#[derive(Debug, Clone)]
pub struct NewTicketMessage {
    pub author_kind: AuthorKind,
    pub user_id: Option<UserId>,
    pub author_name: String,
    pub author_email: Option<String>,
    pub body: String,
    /// Staff-only note (does not change status / last reply).
    pub is_internal: bool,
    pub source: MessageSource,
    pub message_id: Option<String>,
    pub ip: Option<IpAddr>,
}

/// Input of [`open_ticket`].
#[derive(Debug, Clone)]
pub struct NewTicket {
    pub department_id: TicketDepartmentId,
    pub client_id: Option<ClientId>,
    pub user_id: Option<UserId>,
    pub guest_name: Option<String>,
    pub guest_email: Option<String>,
    pub subject: String,
    pub priority: TicketPriority,
    pub service_id: Option<ServiceId>,
    pub domain_id: Option<DomainId>,
    pub first_message: NewTicketMessage,
}

fn random_ticket_number() -> String {
    let bytes: [u8; 4] = hos_core::crypto::random_bytes();
    format!("TCK-{:06}", u32::from_le_bytes(bytes) % 1_000_000)
}

async fn insert_message(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    ticket_id: TicketId,
    m: &NewTicketMessage,
) -> DbResult<TicketMessageRow> {
    Ok(sqlx::query_as::<_, TicketMessageRow>(
        "insert into ticket_messages (id, tenant_id, ticket_id, author_kind, user_id, author_name, author_email,
                                      body, is_internal, source, message_id, ip)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) returning *",
    )
    .bind(TicketMessageId::new())
    .bind(tenant_id)
    .bind(ticket_id)
    .bind(m.author_kind)
    .bind(m.user_id)
    .bind(&m.author_name)
    .bind(&m.author_email)
    .bind(&m.body)
    .bind(m.is_internal)
    .bind(m.source)
    .bind(&m.message_id)
    .bind(m.ip)
    .fetch_one(&mut *conn)
    .await?)
}

/// Opens a ticket with its first message (one transaction). The public number
/// (`TCK-123456`) is random and unique per tenant.
pub async fn open_ticket(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    t: &NewTicket,
) -> DbResult<(TicketRow, TicketMessageRow)> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let mut ticket = None;
    for _ in 0..10 {
        ticket = sqlx::query_as::<_, TicketRow>(
            "insert into tickets (id, tenant_id, number, department_id, client_id, user_id, guest_name, guest_email,
                                  subject, status, priority, service_id, domain_id, last_reply_by)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'open', $10, $11, $12, $13)
             on conflict (tenant_id, number) do nothing
             returning *",
        )
        .bind(TicketId::new())
        .bind(tenant_id)
        .bind(random_ticket_number())
        .bind(t.department_id)
        .bind(t.client_id)
        .bind(t.user_id)
        .bind(&t.guest_name)
        .bind(&t.guest_email)
        .bind(&t.subject)
        .bind(t.priority)
        .bind(t.service_id)
        .bind(t.domain_id)
        .bind(t.first_message.author_kind)
        .fetch_optional(&mut *tx)
        .await
        .map_err(conflict_on_unique("ticket"))?;
        if ticket.is_some() {
            break;
        }
    }
    let ticket =
        ticket.ok_or_else(|| CoreError::Internal("could not allocate a ticket number".into()))?;
    let msg = insert_message(&mut tx, tenant_id, ticket.id, &t.first_message).await?;
    tx.commit().await?;
    Ok((ticket, msg))
}

/// Adds a reply. A staff reply sets `answered`; a client / guest reply sets
/// `customer_reply` (re-opening closed tickets); internal notes and system
/// messages leave the status alone. Returns the message and the updated ticket.
pub async fn reply_to_ticket(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    ticket_id: TicketId,
    m: &NewTicketMessage,
) -> DbResult<(TicketMessageRow, TicketRow)> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query("select id from tickets where tenant_id = $1 and id = $2 for update")
        .bind(tenant_id)
        .bind(ticket_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| not_found("ticket", ticket_id))?;
    let msg = insert_message(&mut tx, tenant_id, ticket_id, m).await?;
    let new_status: Option<TicketStatus> = if m.is_internal {
        None
    } else {
        match m.author_kind {
            AuthorKind::Staff => Some(TicketStatus::Answered),
            AuthorKind::Client | AuthorKind::Guest => Some(TicketStatus::CustomerReply),
            AuthorKind::System => None,
        }
    };
    let ticket = sqlx::query_as::<_, TicketRow>(
        "update tickets set
                status = coalesce($3, status),
                closed_at = case when $3 is not null then null else closed_at end,
                last_reply_at = case when $5 then last_reply_at else now() end,
                last_reply_by = case when $5 then last_reply_by else $4 end
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(ticket_id)
    .bind(new_status)
    .bind(m.author_kind)
    .bind(m.is_internal)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((msg, ticket))
}

/// Ticket by id.
pub async fn get_ticket(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TicketId,
) -> DbResult<Option<TicketRow>> {
    Ok(
        sqlx::query_as::<_, TicketRow>("select * from tickets where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Ticket by public number.
pub async fn ticket_by_number(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    number: &str,
) -> DbResult<Option<TicketRow>> {
    Ok(sqlx::query_as::<_, TicketRow>(
        "select * from tickets where tenant_id = $1 and number = upper($2)",
    )
    .bind(tenant_id)
    .bind(number.trim())
    .fetch_optional(db)
    .await?)
}

/// Messages of a ticket, oldest first (`include_internal` for staff views).
pub async fn ticket_messages(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
    include_internal: bool,
) -> DbResult<Vec<TicketMessageRow>> {
    Ok(sqlx::query_as::<_, TicketMessageRow>(
        "select * from ticket_messages where tenant_id = $1 and ticket_id = $2 and ($3 or not is_internal)
         order by created_at, id",
    )
    .bind(tenant_id)
    .bind(ticket_id)
    .bind(include_internal)
    .fetch_all(db)
    .await?)
}

/// Filters of [`list_tickets`].
#[derive(Debug, Clone, Default)]
pub struct TicketFilter {
    pub status: Option<TicketStatus>,
    /// Only non-closed tickets.
    pub open_only: bool,
    /// Only tickets waiting for staff (`open`, `customer_reply`, `in_progress`).
    pub awaiting_staff: bool,
    pub client_id: Option<ClientId>,
    pub department_id: Option<TicketDepartmentId>,
    pub assigned_to: Option<UserId>,
    pub priority: Option<TicketPriority>,
    /// Matches number, subject, guest e-mail.
    pub search: Option<String>,
    /// Only tickets without an assigned staff member.
    pub unassigned: bool,
}

/// Ticket list row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TicketListRow {
    #[sqlx(flatten)]
    pub ticket: TicketRow,
    pub department_name: String,
    pub client_name: Option<String>,
    pub message_count: i64,
}

/// Tickets, most recent activity first.
pub async fn list_tickets<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &TicketFilter,
    page: Page,
) -> DbResult<Paged<TicketListRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        "select t.*, d.name as department_name,
                coalesce(nullif(c.company_name, ''), nullif(trim(c.first_name || ' ' || c.last_name), ''), t.guest_name)
                    as client_name,
                (select count(*) from ticket_messages m where m.ticket_id = t.id and not m.is_internal) as message_count",
        |qb| {
            qb.push(
                "from tickets t join ticket_departments d on d.id = t.department_id
                 left join clients c on c.id = t.client_id where t.tenant_id = ",
            )
            .push_bind(tenant_id);
            if let Some(s) = f.status {
                qb.push(" and t.status = ").push_bind(s);
            }
            if f.open_only {
                qb.push(" and t.status <> 'closed'");
            }
            if f.awaiting_staff {
                qb.push(" and t.status in ('open', 'customer_reply', 'in_progress')");
            }
            if let Some(c) = f.client_id {
                qb.push(" and t.client_id = ").push_bind(c);
            }
            if let Some(d) = f.department_id {
                qb.push(" and t.department_id = ").push_bind(d);
            }
            if let Some(u) = f.assigned_to {
                qb.push(" and t.assigned_to = ").push_bind(u);
            }
            if f.unassigned {
                qb.push(" and t.assigned_to is null");
            }
            if let Some(p) = f.priority {
                qb.push(" and t.priority = ").push_bind(p);
            }
            if let Some(s) = &search {
                qb.push(" and (t.number ilike ")
                    .push_bind(s.clone())
                    .push(" or t.subject ilike ")
                    .push_bind(s.clone())
                    .push(" or t.guest_email::text ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        "t.last_reply_at desc, t.id desc",
        page,
    )
    .await
}

/// Sets the status (`closed` stamps `closed_at`). The change is checked
/// against the ticket state machine ([`TicketStatus::transition`]): an invalid
/// move (e.g. `closed` → `answered`) is a [`CoreError::InvalidTransition`];
/// setting the current status again is a no-op. `Ok(false)` when the ticket
/// does not exist.
pub async fn set_ticket_status<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    id: TicketId,
    status: TicketStatus,
) -> DbResult<bool> {
    let mut conn = db.acquire().await?;
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let current: Option<TicketStatus> = sqlx::query_scalar(
        "select status from tickets where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Ok(false);
    };
    if current == status {
        return Ok(true);
    }
    current.transition(status)?;
    let res = sqlx::query(
        "update tickets set status = $3,
                closed_at = case when $3 = 'closed' then coalesce(closed_at, now()) else null end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(res.rows_affected() == 1)
}

/// Number of tickets per status (admin queue chips).
pub async fn ticket_status_counts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(TicketStatus, i64)>> {
    Ok(sqlx::query_as::<_, (TicketStatus, i64)>(
        "select status, count(*) from tickets where tenant_id = $1 group by status",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Assigns (or unassigns) a staff member.
pub async fn assign_ticket(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TicketId,
    staff: Option<UserId>,
) -> DbResult<bool> {
    let res = sqlx::query("update tickets set assigned_to = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(staff)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Changes priority / department.
pub async fn update_ticket_routing(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TicketId,
    priority: TicketPriority,
    department_id: TicketDepartmentId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tickets set priority = $3, department_id = $4 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(priority)
    .bind(department_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Closes `answered` tickets without a client reply for `hours` hours.
pub async fn close_inactive_tickets(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    hours: i32,
) -> DbResult<u64> {
    let res = sqlx::query(
        "update tickets set status = 'closed', closed_at = now()
         where tenant_id = $1 and status = 'answered' and last_reply_at < now() - make_interval(hours => $2)",
    )
    .bind(tenant_id)
    .bind(hours)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// attachments / canned responses
// ---------------------------------------------------------------------------

/// `ticket_attachments` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TicketAttachmentRow {
    pub id: TicketAttachmentId,
    pub tenant_id: TenantId,
    pub ticket_id: TicketId,
    pub message_id: Option<TicketMessageId>,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_key: String,
    pub sha256: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Records an uploaded attachment.
#[allow(clippy::too_many_arguments)]
pub async fn add_attachment(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
    message_id: Option<TicketMessageId>,
    filename: &str,
    content_type: &str,
    size_bytes: i64,
    storage_key: &str,
    sha256: Option<&str>,
) -> DbResult<TicketAttachmentRow> {
    Ok(sqlx::query_as::<_, TicketAttachmentRow>(
        "insert into ticket_attachments (id, tenant_id, ticket_id, message_id, filename, content_type, size_bytes,
                                         storage_key, sha256)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning *",
    )
    .bind(TicketAttachmentId::new())
    .bind(tenant_id)
    .bind(ticket_id)
    .bind(message_id)
    .bind(filename)
    .bind(content_type)
    .bind(size_bytes)
    .bind(storage_key)
    .bind(sha256)
    .fetch_one(db)
    .await?)
}

/// Attachments of a ticket.
pub async fn ticket_attachments(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
) -> DbResult<Vec<TicketAttachmentRow>> {
    Ok(sqlx::query_as::<_, TicketAttachmentRow>(
        "select id, tenant_id, ticket_id, message_id, filename, content_type, size_bytes, storage_key,
                sha256, created_at
         from ticket_attachments where tenant_id = $1 and ticket_id = $2 order by created_at",
    )
    .bind(tenant_id)
    .bind(ticket_id)
    .fetch_all(db)
    .await?)
}

/// Stores an uploaded attachment **in the database** (`content`, migration
/// 0019); `storage_key` becomes `db:<id>`.
#[allow(clippy::too_many_arguments)]
pub async fn add_attachment_content(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
    message_id: Option<TicketMessageId>,
    filename: &str,
    content_type: &str,
    content: &[u8],
    sha256: &str,
) -> DbResult<TicketAttachmentRow> {
    let id = TicketAttachmentId::new();
    Ok(sqlx::query_as::<_, TicketAttachmentRow>(
        "insert into ticket_attachments (id, tenant_id, ticket_id, message_id, filename, content_type, size_bytes,
                                         storage_key, sha256, content)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         returning id, tenant_id, ticket_id, message_id, filename, content_type, size_bytes, storage_key,
                   sha256, created_at",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(ticket_id)
    .bind(message_id)
    .bind(filename)
    .bind(content_type)
    .bind(i64::try_from(content.len()).unwrap_or(i64::MAX))
    .bind(format!("db:{id}"))
    .bind(sha256)
    .bind(content)
    .fetch_one(db)
    .await?)
}

/// An attachment of `ticket_id` with its stored bytes (`None` when it does
/// not exist, belongs to another ticket or lives outside the database).
pub async fn attachment_content(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
    id: TicketAttachmentId,
) -> DbResult<Option<(TicketAttachmentRow, Vec<u8>)>> {
    attachment_content_where(db, tenant_id, ticket_id, id, false).await
}

/// Like [`attachment_content`], for the client side: only attachments of a
/// message the client can see (never of an internal staff note, never one
/// without a message).
pub async fn public_attachment_content(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
    id: TicketAttachmentId,
) -> DbResult<Option<(TicketAttachmentRow, Vec<u8>)>> {
    attachment_content_where(db, tenant_id, ticket_id, id, true).await
}

async fn attachment_content_where(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ticket_id: TicketId,
    id: TicketAttachmentId,
    public_only: bool,
) -> DbResult<Option<(TicketAttachmentRow, Vec<u8>)>> {
    #[derive(FromRow)]
    struct WithContent {
        #[sqlx(flatten)]
        row: TicketAttachmentRow,
        content: Option<Vec<u8>>,
    }
    let found = sqlx::query_as::<_, WithContent>(
        "select a.id, a.tenant_id, a.ticket_id, a.message_id, a.filename, a.content_type,
                a.size_bytes, a.storage_key, a.sha256, a.created_at, a.content
         from ticket_attachments a
         where a.tenant_id = $1 and a.ticket_id = $2 and a.id = $3
           and (not $4 or exists (
                select 1 from ticket_messages m
                where m.tenant_id = a.tenant_id and m.ticket_id = a.ticket_id
                  and m.id = a.message_id and not m.is_internal))",
    )
    .bind(tenant_id)
    .bind(ticket_id)
    .bind(id)
    .bind(public_only)
    .fetch_optional(db)
    .await?;
    Ok(found.and_then(|f| f.content.map(|c| (f.row, c))))
}

/// Stores the client's satisfaction rating (1–5) of a ticket.
pub async fn set_ticket_rating(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TicketId,
    rating: i16,
) -> DbResult<bool> {
    if !(1..=5).contains(&rating) {
        return Err(CoreError::validation("rating must be between 1 and 5"));
    }
    let res = sqlx::query("update tickets set rating = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(rating)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// `canned_responses` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CannedResponseRow {
    pub id: CannedResponseId,
    pub tenant_id: TenantId,
    pub title: String,
    pub body: String,
    pub locale: Locale,
    pub category: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a canned response.
pub async fn create_canned_response(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    title: &str,
    body: &str,
    locale: Locale,
    created_by: Option<UserId>,
) -> DbResult<CannedResponseRow> {
    Ok(sqlx::query_as::<_, CannedResponseRow>(
        "insert into canned_responses (id, tenant_id, title, body, locale, created_by)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(CannedResponseId::new())
    .bind(tenant_id)
    .bind(title)
    .bind(body)
    .bind(locale)
    .bind(created_by)
    .fetch_one(db)
    .await?)
}

/// Canned responses (optionally of one locale).
pub async fn list_canned_responses(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    locale: Option<Locale>,
) -> DbResult<Vec<CannedResponseRow>> {
    Ok(sqlx::query_as::<_, CannedResponseRow>(
        "select * from canned_responses where tenant_id = $1 and ($2::text is null or locale = $2) order by title",
    )
    .bind(tenant_id)
    .bind(locale)
    .fetch_all(db)
    .await?)
}

/// Canned response by id.
pub async fn get_canned_response(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CannedResponseId,
) -> DbResult<Option<CannedResponseRow>> {
    Ok(sqlx::query_as::<_, CannedResponseRow>(
        "select * from canned_responses where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Updates a canned response (also sets its category).
pub async fn update_canned_response(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CannedResponseId,
    title: &str,
    body: &str,
    locale: Locale,
    category: Option<&str>,
) -> DbResult<Option<CannedResponseRow>> {
    Ok(sqlx::query_as::<_, CannedResponseRow>(
        "update canned_responses set title = $3, body = $4, locale = $5, category = $6
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(title)
    .bind(body)
    .bind(locale)
    .bind(category)
    .fetch_optional(db)
    .await?)
}

/// Deletes a canned response.
pub async fn delete_canned_response(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CannedResponseId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from canned_responses where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Default translations object (`{}`), for callers building departments.
pub fn no_translations() -> Json {
    empty_object()
}

#[cfg(test)]
mod tests {
    #[test]
    fn ticket_number_format() {
        let n = super::random_ticket_number();
        assert!(n.starts_with("TCK-"));
        assert_eq!(n.len(), 10);
    }
}
