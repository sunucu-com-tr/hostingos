//! Turkish e-documents (`einvoice_documents`, `einvoice_sequences`, migration
//! 0034). Every query is scoped by `tenant_id`.
//!
//! Concurrency: a worker [`claim`]s a document (short `locked_until` lease)
//! before talking to the provider, so two deliveries of the same job never
//! submit in parallel; the unique `(tenant_id, invoice_id, purpose)` key makes
//! [`ensure_document`] idempotent.

use chrono::{DateTime, Utc};
use hos_core::ids::{ClientId, IntegrationId, InvoiceId, TenantId};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use super::Json;
use crate::error::DbResult;

hos_core::define_id! {
    /// `einvoice_documents`.
    pub struct EInvoiceDocumentId;
}

/// `einvoice_documents` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct EInvoiceDocumentRow {
    pub id: EInvoiceDocumentId,
    pub tenant_id: TenantId,
    pub invoice_id: InvoiceId,
    pub integration_id: Option<IntegrationId>,
    /// `sale` or `return`.
    pub purpose: String,
    /// `efatura` / `earsiv` once decided.
    pub kind: Option<String>,
    pub profile: Option<String>,
    pub type_code: Option<String>,
    pub ettn: Uuid,
    pub number: Option<String>,
    pub series: Option<String>,
    /// `pending`, `processing`, `issued`, `failed`, `rejected`, `cancelled`, `skipped`.
    pub status: String,
    pub provider_id: Option<String>,
    pub checkpoint: Json,
    /// Frozen `EDocumentRequest` (JSON) of the first submission.
    pub request: Option<Json>,
    pub is_test: bool,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub message: Option<String>,
    pub attention: Option<String>,
    pub attention_at: Option<DateTime<Utc>>,
    pub cancellable_until: Option<DateTime<Utc>>,
    pub locked_until: Option<DateTime<Utc>>,
    pub submitted_at: Option<DateTime<Utc>>,
    pub issued_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The document of `invoice_id` for `purpose`, if any.
pub async fn for_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
    purpose: &str,
) -> DbResult<Option<EInvoiceDocumentRow>> {
    Ok(sqlx::query_as::<_, EInvoiceDocumentRow>(
        "select * from einvoice_documents where tenant_id = $1 and invoice_id = $2 and purpose = $3",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .bind(purpose)
    .fetch_optional(&mut *conn)
    .await?)
}

/// All documents of an invoice (sale first).
pub async fn list_for_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
) -> DbResult<Vec<EInvoiceDocumentRow>> {
    Ok(sqlx::query_as::<_, EInvoiceDocumentRow>(
        "select * from einvoice_documents where tenant_id = $1 and invoice_id = $2
          order by (purpose = 'sale') desc, created_at",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_all(&mut *conn)
    .await?)
}

/// One document.
pub async fn get_document(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
) -> DbResult<Option<EInvoiceDocumentRow>> {
    Ok(sqlx::query_as::<_, EInvoiceDocumentRow>(
        "select * from einvoice_documents where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Creates the document of `invoice_id` / `purpose` unless it exists;
/// returns the (existing or new) row and whether it was created.
pub async fn ensure_document(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
    purpose: &str,
    integration_id: Option<IntegrationId>,
) -> DbResult<(EInvoiceDocumentRow, bool)> {
    let created = sqlx::query_as::<_, EInvoiceDocumentRow>(
        "insert into einvoice_documents (id, tenant_id, invoice_id, purpose, integration_id, ettn)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (tenant_id, invoice_id, purpose) do nothing
         returning *",
    )
    .bind(EInvoiceDocumentId::new())
    .bind(tenant_id)
    .bind(invoice_id)
    .bind(purpose)
    .bind(integration_id)
    .bind(Uuid::now_v7())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(row) = created {
        return Ok((row, true));
    }
    let row = for_invoice(conn, tenant_id, invoice_id, purpose)
        .await?
        .ok_or_else(|| {
            hos_core::CoreError::Internal("e-document vanished after conflict".into())
        })?;
    Ok((row, false))
}

/// Takes a lease of `seconds` on a document that is not final and not
/// leased by someone else. `None`: busy, final, or not found.
pub async fn claim(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
    seconds: i32,
) -> DbResult<Option<EInvoiceDocumentRow>> {
    Ok(sqlx::query_as::<_, EInvoiceDocumentRow>(
        "update einvoice_documents
            set locked_until = now() + make_interval(secs => $3)
          where tenant_id = $1 and id = $2
            and status in ('pending', 'processing')
            and (locked_until is null or locked_until < now())
          returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(f64::from(seconds))
    .fetch_optional(&mut *conn)
    .await?)
}

/// Takes the lease regardless of status (staff actions on final documents:
/// cancel, refresh). `None` while another worker holds it.
pub async fn claim_any(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
    seconds: i32,
) -> DbResult<Option<EInvoiceDocumentRow>> {
    Ok(sqlx::query_as::<_, EInvoiceDocumentRow>(
        "update einvoice_documents
            set locked_until = now() + make_interval(secs => $3)
          where tenant_id = $1 and id = $2
            and (locked_until is null or locked_until < now())
          returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(f64::from(seconds))
    .fetch_optional(&mut *conn)
    .await?)
}

/// What a processing step writes back (always releases the lease).
#[derive(Debug, Clone, Default)]
pub struct DocumentUpdate {
    pub status: String,
    pub integration_id: Option<IntegrationId>,
    pub kind: Option<String>,
    pub profile: Option<String>,
    pub type_code: Option<String>,
    pub ettn: Option<Uuid>,
    pub number: Option<String>,
    pub series: Option<String>,
    pub provider_id: Option<String>,
    pub checkpoint: Json,
    pub request: Option<Json>,
    pub is_test: bool,
    pub attempted: bool,
    pub last_error: Option<String>,
    pub message: Option<String>,
    pub cancellable_until: Option<DateTime<Utc>>,
}

/// Writes `u` and releases the lease. `submitted_at` / `issued_at` /
/// `cancelled_at` are stamped on the first transition.
pub async fn save(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
    u: &DocumentUpdate,
) -> DbResult<EInvoiceDocumentRow> {
    Ok(sqlx::query_as::<_, EInvoiceDocumentRow>(
        "update einvoice_documents set
             status = $3,
             integration_id = coalesce($4, integration_id),
             kind = coalesce($5, kind),
             profile = coalesce($6, profile),
             type_code = coalesce($7, type_code),
             ettn = coalesce($8, ettn),
             number = coalesce($9, number),
             series = coalesce($10, series),
             provider_id = coalesce($11, provider_id),
             checkpoint = $12,
             request = coalesce(request, $13),
             is_test = $14,
             attempts = attempts + case when $15 then 1 else 0 end,
             last_error = $16,
             message = $17,
             cancellable_until = coalesce($18, cancellable_until),
             submitted_at = case when $3 in ('processing', 'issued', 'rejected', 'cancelled')
                                 then coalesce(submitted_at, now()) else submitted_at end,
             issued_at = case when $3 = 'issued' then coalesce(issued_at, now()) else issued_at end,
             cancelled_at = case when $3 = 'cancelled' then coalesce(cancelled_at, now()) else cancelled_at end,
             locked_until = null
         where tenant_id = $1 and id = $2
         returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&u.status)
    .bind(u.integration_id)
    .bind(&u.kind)
    .bind(&u.profile)
    .bind(&u.type_code)
    .bind(u.ettn)
    .bind(&u.number)
    .bind(&u.series)
    .bind(&u.provider_id)
    .bind(&u.checkpoint)
    .bind(&u.request)
    .bind(u.is_test)
    .bind(u.attempted)
    .bind(&u.last_error)
    .bind(&u.message)
    .bind(u.cancellable_until)
    .fetch_one(&mut *conn)
    .await?)
}

/// Releases a lease without other changes.
pub async fn release(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
) -> DbResult<()> {
    sqlx::query(
        "update einvoice_documents set locked_until = null where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Marks a document as needing staff (`None` clears the flag).
pub async fn set_attention(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
    attention: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "update einvoice_documents
            set attention = $3,
                attention_at = case when $3 is null then null else now() end
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(attention)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Resets a failed / rejected document to `pending` for another attempt
/// (staff "retry"). The ETTN, number and frozen request are kept.
pub async fn reset_for_retry(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
) -> DbResult<bool> {
    // A document a worker holds (its lease runs) is left alone: clearing the
    // lease would let a second worker submit it again.
    let n = sqlx::query(
        "update einvoice_documents
            set status = 'pending', last_error = null, locked_until = null
          where tenant_id = $1 and id = $2 and status in ('failed', 'pending')
            and (locked_until is null or locked_until < now())",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Next legal sequence value of `series` in `year` (gap-free: run it in the
/// transaction that stores the number and the frozen request on the
/// document).
pub async fn allocate_number(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    series: &str,
    year: i32,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "insert into einvoice_sequences (id, tenant_id, series, year, last_value)
         values ($1, $2, $3, $4, 1)
         on conflict (tenant_id, series, year)
         do update set last_value = einvoice_sequences.last_value + 1
         returning last_value",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(series)
    .bind(year)
    .fetch_one(&mut *conn)
    .await?)
}

/// Stores the allocated number on a document that has none yet.
pub async fn set_number(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: EInvoiceDocumentId,
    series: &str,
    number: &str,
) -> DbResult<bool> {
    let n = sqlx::query(
        "update einvoice_documents set number = $3, series = $4
          where tenant_id = $1 and id = $2 and number is null",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(number)
    .bind(series)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Documents the sweep should look at: unfinished and not leased, idle for
/// at least `idle_secs`.
pub async fn due_for_sweep(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    idle_secs: i32,
    limit: i64,
) -> DbResult<Vec<EInvoiceDocumentId>> {
    Ok(sqlx::query_scalar::<_, EInvoiceDocumentId>(
        "select id from einvoice_documents
          where tenant_id = $1 and status in ('pending', 'processing')
            and (locked_until is null or locked_until < now())
            and updated_at < now() - make_interval(secs => $2)
          order by updated_at
          limit $3",
    )
    .bind(tenant_id)
    .bind(f64::from(idle_secs))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?)
}

/// Issued documents whose final status should be re-checked daily (e-Fatura
/// answers, cancellations made in the provider panel).
pub async fn due_for_reconcile(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<EInvoiceDocumentId>> {
    Ok(sqlx::query_scalar::<_, EInvoiceDocumentId>(
        "select id from einvoice_documents
          where tenant_id = $1 and status = 'issued'
            and issued_at > now() - interval '45 days'
            and updated_at < now() - interval '20 hours'
            and (locked_until is null or locked_until < now())
          order by updated_at
          limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?)
}

/// Row of the admin list.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct DocumentListRow {
    pub id: EInvoiceDocumentId,
    pub invoice_id: InvoiceId,
    pub invoice_number: Option<String>,
    pub client_id: ClientId,
    pub client_name: String,
    pub purpose: String,
    pub kind: Option<String>,
    pub number: Option<String>,
    pub status: String,
    pub is_test: bool,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub message: Option<String>,
    pub attention: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// Admin list. `filter`: `problems` (failed, rejected or needing staff),
/// a status, or `all`.
pub async fn list_documents(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    filter: &str,
    limit: i64,
    offset: i64,
) -> DbResult<Vec<DocumentListRow>> {
    Ok(sqlx::query_as::<_, DocumentListRow>(
        "select d.id, d.invoice_id, i.number as invoice_number, i.client_id,
                coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name,
                d.purpose, d.kind, d.number, d.status, d.is_test, d.attempts, d.last_error,
                d.message, d.attention, d.updated_at
           from einvoice_documents d
           join invoices i on i.tenant_id = d.tenant_id and i.id = d.invoice_id
           join clients c on c.tenant_id = i.tenant_id and c.id = i.client_id
          where d.tenant_id = $1
            and case $2
                  when 'all' then true
                  when 'problems' then d.status in ('failed', 'rejected') or d.attention is not null
                  else d.status = $2
                end
          order by d.updated_at desc
          limit $3 offset $4",
    )
    .bind(tenant_id)
    .bind(filter)
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *conn)
    .await?)
}

/// Number of documents matching a [`list_documents`] filter.
pub async fn count_documents(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    filter: &str,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from einvoice_documents d
          where d.tenant_id = $1
            and case $2
                  when 'all' then true
                  when 'problems' then d.status in ('failed', 'rejected') or d.attention is not null
                  else d.status = $2
                end",
    )
    .bind(tenant_id)
    .bind(filter)
    .fetch_one(&mut *conn)
    .await?)
}

/// Number of documents needing staff.
pub async fn count_problems(conn: &mut PgConnection, tenant_id: TenantId) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from einvoice_documents
          where tenant_id = $1 and (status in ('failed', 'rejected') or attention is not null)",
    )
    .bind(tenant_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Numbered invoices in one of `statuses`, issued on or after `since`,
/// that have no `sale` document yet (oldest first).
pub async fn invoices_without_document(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    statuses: &[&str],
    since: chrono::NaiveDate,
    limit: i64,
) -> DbResult<Vec<InvoiceId>> {
    let statuses: Vec<String> = statuses.iter().map(|s| (*s).to_owned()).collect();
    Ok(sqlx::query_scalar::<_, InvoiceId>(
        "select i.id from invoices i
          where i.tenant_id = $1 and i.status = any($2) and i.issue_date >= $3
            and i.number is not null
            and not exists (select 1 from einvoice_documents d
                             where d.tenant_id = i.tenant_id and d.invoice_id = i.id and d.purpose = 'sale')
          order by i.issue_date, i.id
          limit $4",
    )
    .bind(tenant_id)
    .bind(&statuses)
    .bind(since)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?)
}

/// Issued sale documents whose invoice was refunded, cancelled or charged
/// back since: `(document, invoice status, charged back)`.
pub async fn issued_with_reversed_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<(EInvoiceDocumentId, String, bool)>> {
    Ok(sqlx::query_as::<_, (EInvoiceDocumentId, String, bool)>(
        "select d.id, i.status::text,
                exists (select 1 from transactions t
                         where t.tenant_id = i.tenant_id and t.invoice_id = i.id
                           and t.kind = 'chargeback') as charged_back
           from einvoice_documents d
           join invoices i on i.tenant_id = d.tenant_id and i.id = d.invoice_id
          where d.tenant_id = $1 and d.purpose = 'sale' and d.status = 'issued'
            and (i.status in ('refunded', 'partially_refunded', 'cancelled')
                 or exists (select 1 from transactions t
                             where t.tenant_id = i.tenant_id and t.invoice_id = i.id
                               and t.kind = 'chargeback'))
          order by d.updated_at
          limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?)
}

/// Records the legal number as the invoice's external reference (only when
/// none is set).
pub async fn set_invoice_external_ref(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
    reference: &str,
) -> DbResult<()> {
    sqlx::query(
        "update invoices set external_ref = $3
          where tenant_id = $1 and id = $2 and (external_ref is null or external_ref = '')",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .bind(reference)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
