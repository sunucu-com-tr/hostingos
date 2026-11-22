//! E-mail templates (per key × locale) and the outgoing e-mail log.

use chrono::{DateTime, Utc};
use hos_core::domain::Locale;
use hos_core::ids::{ClientId, EmailTemplateId, TenantId, UserId};
use sqlx::{Acquire, FromRow, PgExecutor, Postgres};
use uuid::Uuid;

use crate::enums::{EmailCategory, EmailStatus};
use crate::error::DbResult;
use crate::ids::EmailLogId;
use crate::page::{Page, Paged, fetch_paged};

/// `email_templates` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct EmailTemplateRow {
    pub id: EmailTemplateId,
    pub tenant_id: TenantId,
    pub key: String,
    pub locale: Locale,
    pub category: EmailCategory,
    pub name: String,
    pub subject: String,
    pub body_html: String,
    pub body_text: String,
    pub is_active: bool,
    pub is_system: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Template content for [`upsert_template`].
#[derive(Debug, Clone)]
pub struct EmailTemplateInput {
    pub key: String,
    pub locale: Locale,
    pub category: EmailCategory,
    pub name: String,
    pub subject: String,
    pub body_html: String,
    pub body_text: String,
    pub is_system: bool,
}

/// Inserts or replaces a template (`overwrite = false` keeps an existing one —
/// used by seeding so staff edits survive).
pub async fn upsert_template(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    t: &EmailTemplateInput,
    overwrite: bool,
) -> DbResult<Option<EmailTemplateRow>> {
    let sql = if overwrite {
        "insert into email_templates (id, tenant_id, key, locale, category, name, subject, body_html, body_text, is_system)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         on conflict (tenant_id, key, locale) do update
            set category = excluded.category, name = excluded.name, subject = excluded.subject,
                body_html = excluded.body_html, body_text = excluded.body_text
         returning *"
    } else {
        "insert into email_templates (id, tenant_id, key, locale, category, name, subject, body_html, body_text, is_system)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         on conflict (tenant_id, key, locale) do nothing
         returning *"
    };
    Ok(sqlx::query_as::<_, EmailTemplateRow>(sql)
        .bind(EmailTemplateId::new())
        .bind(tenant_id)
        .bind(&t.key)
        .bind(t.locale)
        .bind(t.category)
        .bind(&t.name)
        .bind(&t.subject)
        .bind(&t.body_html)
        .bind(&t.body_text)
        .bind(t.is_system)
        .fetch_optional(db)
        .await?)
}

/// The active template for `key`, preferring `locale` and falling back to the
/// other language.
pub async fn get_template(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
    locale: Locale,
) -> DbResult<Option<EmailTemplateRow>> {
    Ok(sqlx::query_as::<_, EmailTemplateRow>(
        "select * from email_templates where tenant_id = $1 and key = $2 and is_active
         order by (locale = $3) desc limit 1",
    )
    .bind(tenant_id)
    .bind(key)
    .bind(locale)
    .fetch_optional(db)
    .await?)
}

/// All templates of a tenant (admin).
pub async fn list_templates(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<EmailTemplateRow>> {
    Ok(sqlx::query_as::<_, EmailTemplateRow>(
        "select * from email_templates where tenant_id = $1 order by category, key, locale",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Enables / disables a template.
pub async fn set_template_active(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: EmailTemplateId,
    active: bool,
) -> DbResult<bool> {
    let res =
        sqlx::query("update email_templates set is_active = $3 where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .bind(active)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Columns of [`EmailLogRow`] — listed explicitly (never `*`): the sealed body
/// stays out of reads, and the result type of prepared statements does not
/// change when a column is added (poolers keep server-side plans).
pub(crate) const EMAIL_LOG_COLUMNS: &str = "id, tenant_id, user_id, client_id, template_key, \
     to_address, cc, bcc, from_address, subject, body_html, body_text, status, provider_message_id, \
     error, related_type, related_id, sent_at, created_at, updated_at";

/// `email_log` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct EmailLogRow {
    pub id: EmailLogId,
    pub tenant_id: TenantId,
    pub user_id: Option<UserId>,
    pub client_id: Option<ClientId>,
    pub template_key: Option<String>,
    pub to_address: String,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub from_address: Option<String>,
    pub subject: String,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    pub status: EmailStatus,
    pub provider_message_id: Option<String>,
    pub error: Option<String>,
    pub related_type: Option<String>,
    pub related_id: Option<Uuid>,
    pub sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An e-mail to log (status `queued`).
#[derive(Debug, Clone, Default)]
pub struct NewEmailLog {
    pub user_id: Option<UserId>,
    pub client_id: Option<ClientId>,
    pub template_key: Option<String>,
    pub to_address: String,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub from_address: Option<String>,
    pub subject: String,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    pub related: Option<(String, Uuid)>,
}

/// Logs an outgoing e-mail before sending.
pub async fn log_email(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    e: &NewEmailLog,
) -> DbResult<EmailLogId> {
    let id = EmailLogId::new();
    sqlx::query(
        "insert into email_log (id, tenant_id, user_id, client_id, template_key, to_address, cc, bcc, from_address,
                                subject, body_html, body_text, related_type, related_id)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(e.user_id)
    .bind(e.client_id)
    .bind(&e.template_key)
    .bind(&e.to_address)
    .bind(&e.cc)
    .bind(&e.bcc)
    .bind(&e.from_address)
    .bind(&e.subject)
    .bind(&e.body_html)
    .bind(&e.body_text)
    .bind(e.related.as_ref().map(|r| r.0.as_str()))
    .bind(e.related.as_ref().map(|r| r.1))
    .execute(db)
    .await?;
    Ok(id)
}

/// Stores (or with `None` clears) the encrypted unredacted message of an
/// e-mail whose logged body was redacted (one-time links).
pub async fn set_sealed_body(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: EmailLogId,
    sealed: Option<&[u8]>,
) -> DbResult<bool> {
    let res = sqlx::query("update email_log set sealed_body = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(sealed)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// The encrypted unredacted message of an e-mail, if one is stored.
pub async fn sealed_body(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: EmailLogId,
) -> DbResult<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar::<_, Option<Vec<u8>>>(
        "select sealed_body from email_log where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?
    .flatten())
}

/// Marks an e-mail sent.
pub async fn mark_email_sent(
    db: impl PgExecutor<'_>,
    id: EmailLogId,
    provider_message_id: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update email_log set status = 'sent', sent_at = now(), provider_message_id = $2, error = null where id = $1",
    )
    .bind(id)
    .bind(provider_message_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Marks an e-mail failed / bounced.
pub async fn mark_email_failed(
    db: impl PgExecutor<'_>,
    id: EmailLogId,
    status: EmailStatus,
    error: &str,
) -> DbResult<bool> {
    let res = sqlx::query("update email_log set status = $2, error = $3 where id = $1")
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// E-mail log of a tenant (optionally of one client), newest first.
pub async fn list_email_log<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    client_id: Option<ClientId>,
    page: Page,
) -> DbResult<Paged<EmailLogRow>> {
    fetch_paged(
        db,
        &format!("select {EMAIL_LOG_COLUMNS}"),
        |qb| {
            qb.push("from email_log where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(c) = client_id {
                qb.push(" and client_id = ").push_bind(c);
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}
