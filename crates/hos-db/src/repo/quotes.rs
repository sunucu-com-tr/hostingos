//! Quotes (estimates): `quotes`, `quote_items`.
//!
//! A quote is addressed to a client or to a prospect e-mail. Totals are
//! computed by the caller (hos-billing `quotes`) and stored with the lines in
//! one transaction ([`save_quote`]). The accept link's token is only stored
//! as its SHA-256 ([`set_token`], [`quote_by_token_hash`]).

use std::net::IpAddr;

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::ids::{ClientId, InvoiceId, OrderId, ProductId, TenantId, UserId};
use hos_core::money::{BillingCycle, Currency};
use hos_core::string_enum;
use rust_decimal::Decimal;
use sqlx::{FromRow, PgConnection, PgExecutor};

use super::Json;
use crate::error::{DbResult, not_found};
use crate::page::{Page, Paged, fetch_paged};

hos_core::define_id! {
    /// `quotes`.
    pub struct QuoteId;
}
hos_core::define_id! {
    /// `quote_items`.
    pub struct QuoteItemId;
}

string_enum! {
    /// `quotes.status`.
    pub enum QuoteStatus {
        /// Being prepared; not visible to the recipient.
        Draft => "draft",
        /// Sent; the recipient can accept or decline it.
        Sent => "sent",
        /// Accepted and converted into an order + invoice.
        Accepted => "accepted",
        /// Declined by the recipient.
        Declined => "declined",
        /// Not answered before `valid_until`.
        Expired => "expired",
        /// Withdrawn by staff.
        Cancelled => "cancelled",
    }
}

impl QuoteStatus {
    /// Staff may still edit the quote.
    pub fn is_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Sent)
    }
}

/// `quotes` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct QuoteRow {
    pub id: QuoteId,
    pub tenant_id: TenantId,
    pub seq: i64,
    pub client_id: Option<ClientId>,
    pub prospect_name: Option<String>,
    pub prospect_email: Option<String>,
    pub prospect_company: Option<String>,
    pub subject: String,
    pub status: QuoteStatus,
    pub currency: Currency,
    pub valid_until: NaiveDate,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub subtotal: Decimal,
    pub discount_total: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub tax_lines: Json,
    pub token_hash: Option<String>,
    pub token_expires_at: Option<DateTime<Utc>>,
    pub sent_at: Option<DateTime<Utc>>,
    pub viewed_at: Option<DateTime<Utc>>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub declined_at: Option<DateTime<Utc>>,
    pub decline_reason: Option<String>,
    pub accepted_ip: Option<IpAddr>,
    pub accepted_by_user_id: Option<UserId>,
    pub order_id: Option<OrderId>,
    pub invoice_id: Option<InvoiceId>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl QuoteRow {
    /// Display number (`Q-00042`).
    pub fn number(&self) -> String {
        format!("Q-{:05}", self.seq)
    }

    /// Whether `valid_until` has passed on `today`.
    pub fn is_past_validity(&self, today: NaiveDate) -> bool {
        self.valid_until < today
    }
}

/// `quote_items` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct QuoteItemRow {
    pub id: QuoteItemId,
    pub tenant_id: TenantId,
    pub quote_id: QuoteId,
    pub product_id: Option<ProductId>,
    pub billing_cycle: Option<BillingCycle>,
    pub description: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub discount_percent: Decimal,
    pub amount: Decimal,
    pub taxable: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A line of [`QuoteInput`] (amount already computed and rounded).
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteItemInput {
    pub product_id: Option<ProductId>,
    pub billing_cycle: Option<BillingCycle>,
    pub description: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub discount_percent: Decimal,
    pub amount: Decimal,
    pub taxable: bool,
}

/// Header + lines + totals written by [`save_quote`].
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteInput {
    pub client_id: Option<ClientId>,
    pub prospect_name: Option<String>,
    pub prospect_email: Option<String>,
    pub prospect_company: Option<String>,
    pub subject: String,
    pub currency: Currency,
    pub valid_until: NaiveDate,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub subtotal: Decimal,
    pub discount_total: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub tax_lines: Json,
    pub items: Vec<QuoteItemInput>,
}

/// Creates (`id = None`) or replaces an editable quote and its lines.
/// Updating a quote that is no longer editable fails with `Conflict`.
pub async fn save_quote(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: Option<QuoteId>,
    input: &QuoteInput,
    created_by: Option<UserId>,
) -> DbResult<QuoteRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let row = match id {
        None => {
            // Gap-tolerant per-tenant numbering, serialised per tenant.
            sqlx::query("select pg_advisory_xact_lock(hashtextextended('quotes:' || $1::text, 0))")
                .bind(tenant_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query_as::<_, QuoteRow>(
                "insert into quotes (id, tenant_id, seq, client_id, prospect_name, prospect_email,
                        prospect_company, subject, currency, valid_until, notes, admin_notes,
                        subtotal, discount_total, tax_total, total, tax_lines, created_by)
                 select $1, $2, coalesce(max(seq), 0) + 1, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                        $12, $13, $14, $15, $16, $17
                   from quotes where tenant_id = $2
                 returning *",
            )
            .bind(QuoteId::new())
            .bind(tenant_id)
            .bind(input.client_id)
            .bind(&input.prospect_name)
            .bind(&input.prospect_email)
            .bind(&input.prospect_company)
            .bind(&input.subject)
            .bind(input.currency)
            .bind(input.valid_until)
            .bind(&input.notes)
            .bind(&input.admin_notes)
            .bind(input.subtotal)
            .bind(input.discount_total)
            .bind(input.tax_total)
            .bind(input.total)
            .bind(&input.tax_lines)
            .bind(created_by)
            .fetch_one(&mut *tx)
            .await?
        }
        Some(id) => {
            let row = sqlx::query_as::<_, QuoteRow>(
                "update quotes set client_id = $3, prospect_name = $4, prospect_email = $5,
                        prospect_company = $6, subject = $7, currency = $8, valid_until = $9,
                        notes = $10, admin_notes = $11, subtotal = $12, discount_total = $13,
                        tax_total = $14, total = $15, tax_lines = $16
                  where tenant_id = $1 and id = $2 and status in ('draft', 'sent')
                 returning *",
            )
            .bind(tenant_id)
            .bind(id)
            .bind(input.client_id)
            .bind(&input.prospect_name)
            .bind(&input.prospect_email)
            .bind(&input.prospect_company)
            .bind(&input.subject)
            .bind(input.currency)
            .bind(input.valid_until)
            .bind(&input.notes)
            .bind(&input.admin_notes)
            .bind(input.subtotal)
            .bind(input.discount_total)
            .bind(input.tax_total)
            .bind(input.total)
            .bind(&input.tax_lines)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                return Err(match get_quote(&mut *tx, tenant_id, id).await? {
                    Some(_) => {
                        hos_core::CoreError::Conflict("the quote can no longer be edited".into())
                    }
                    None => not_found("quote", id),
                });
            };
            sqlx::query("delete from quote_items where tenant_id = $1 and quote_id = $2")
                .bind(tenant_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            row
        }
    };
    for (i, item) in input.items.iter().enumerate() {
        sqlx::query(
            "insert into quote_items (id, tenant_id, quote_id, product_id, billing_cycle, description,
                    quantity, unit_price, discount_percent, amount, taxable, sort_order)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(QuoteItemId::new())
        .bind(tenant_id)
        .bind(row.id)
        .bind(item.product_id)
        .bind(item.billing_cycle)
        .bind(&item.description)
        .bind(item.quantity)
        .bind(item.unit_price)
        .bind(item.discount_percent)
        .bind(item.amount)
        .bind(item.taxable)
        .bind(i32::try_from(i).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// One quote.
pub async fn get_quote(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: QuoteId,
) -> DbResult<Option<QuoteRow>> {
    Ok(
        sqlx::query_as::<_, QuoteRow>("select * from quotes where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// One quote, locked for update.
pub async fn lock_quote(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: QuoteId,
) -> DbResult<Option<QuoteRow>> {
    Ok(sqlx::query_as::<_, QuoteRow>(
        "select * from quotes where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The quote whose accept-link token hashes to `token_hash` (unexpired).
pub async fn quote_by_token_hash(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    token_hash: &str,
    now: DateTime<Utc>,
) -> DbResult<Option<QuoteRow>> {
    Ok(sqlx::query_as::<_, QuoteRow>(
        "select * from quotes where tenant_id = $1 and token_hash = $2
            and (token_expires_at is null or token_expires_at > $3)",
    )
    .bind(tenant_id)
    .bind(token_hash)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

/// Lines of a quote in order.
pub async fn quote_items(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    quote_id: QuoteId,
) -> DbResult<Vec<QuoteItemRow>> {
    Ok(sqlx::query_as::<_, QuoteItemRow>(
        "select * from quote_items where tenant_id = $1 and quote_id = $2 order by sort_order, created_at",
    )
    .bind(tenant_id)
    .bind(quote_id)
    .fetch_all(db)
    .await?)
}

/// List filter.
#[derive(Debug, Clone, Default)]
pub struct QuoteFilter {
    pub status: Option<QuoteStatus>,
    pub client_id: Option<ClientId>,
    /// Subject, prospect / client e-mail or number.
    pub search: Option<String>,
}

/// A list row (with the recipient's display name).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct QuoteListRow {
    #[sqlx(flatten)]
    pub quote: QuoteRow,
    pub recipient: String,
    pub recipient_email: String,
}

/// Paged quotes, newest first unless `order_by` says otherwise (allow-listed
/// by the caller).
pub async fn list_quotes(
    db: impl sqlx::Acquire<'_, Database = sqlx::Postgres>,
    tenant_id: TenantId,
    f: &QuoteFilter,
    order_by: &str,
    page: Page,
) -> DbResult<Paged<QuoteListRow>> {
    let f = f.clone();
    let search = f
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(crate::error::like_contains);
    let seq = f
        .search
        .as_deref()
        .map(|s| s.trim().trim_start_matches(['Q', 'q', '-', '#']).to_owned())
        .and_then(|s| s.parse::<i64>().ok());
    fetch_paged(
        db,
        "select q.*,
                coalesce(nullif(c.company_name, ''), nullif(trim(c.first_name || ' ' || c.last_name), ''),
                         nullif(q.prospect_company, ''), nullif(q.prospect_name, ''), q.prospect_email::text, '') as recipient,
                coalesce(c.email::text, q.prospect_email::text, '') as recipient_email",
        |qb| {
            qb.push(
                "from quotes q left join clients c on c.tenant_id = q.tenant_id and c.id = q.client_id
                 where q.tenant_id = ",
            )
            .push_bind(tenant_id);
            if let Some(s) = f.status {
                qb.push(" and q.status = ").push_bind(s);
            }
            if let Some(c) = f.client_id {
                qb.push(" and q.client_id = ").push_bind(c);
            }
            if let Some(s) = &search {
                qb.push(" and (q.subject ilike ")
                    .push_bind(s.clone())
                    .push(" or q.prospect_email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or c.email::text ilike ")
                    .push_bind(s.clone());
                if let Some(n) = seq {
                    qb.push(" or q.seq = ").push_bind(n);
                }
                qb.push(")");
            }
        },
        order_by,
        page,
    )
    .await
}

/// Quotes of a client that the client may see (everything but drafts).
pub async fn client_quotes(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    limit: i64,
) -> DbResult<Vec<QuoteRow>> {
    Ok(sqlx::query_as::<_, QuoteRow>(
        "select * from quotes where tenant_id = $1 and client_id = $2 and status <> 'draft'
          order by created_at desc limit $3",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Stores a new accept-link token hash, marks the quote sent.
pub async fn mark_sent(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: QuoteId,
    token_hash: &str,
    token_expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> DbResult<Option<QuoteRow>> {
    Ok(sqlx::query_as::<_, QuoteRow>(
        "update quotes set status = 'sent', token_hash = $3, token_expires_at = $4,
                sent_at = coalesce(sent_at, $5)
          where tenant_id = $1 and id = $2 and status in ('draft', 'sent')
         returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(token_hash)
    .bind(token_expires_at)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

/// First view by the recipient.
pub async fn mark_viewed(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: QuoteId,
    now: DateTime<Utc>,
) -> DbResult<()> {
    sqlx::query(
        "update quotes set viewed_at = $3 where tenant_id = $1 and id = $2 and viewed_at is null",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

/// Moves a quote from one of `from` to `to` (stamping the matching
/// timestamp); `None` when it was not in one of `from`.
pub async fn transition_quote(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: QuoteId,
    from: &[QuoteStatus],
    to: QuoteStatus,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> DbResult<Option<QuoteRow>> {
    let from: Vec<&str> = from.iter().map(QuoteStatus::as_str).collect();
    Ok(sqlx::query_as::<_, QuoteRow>(
        "update quotes set status = $4,
                declined_at = case when $4 = 'declined' then $6 else declined_at end,
                decline_reason = case when $4 = 'declined' then $5 else decline_reason end,
                token_hash = case when $4 in ('declined', 'expired', 'cancelled') then null else token_hash end
          where tenant_id = $1 and id = $2 and status = any($3)
         returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&from)
    .bind(to)
    .bind(reason)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

/// Records the acceptance and the documents it produced. Only a `sent`
/// quote is accepted (the caller holds the row lock).
#[allow(clippy::too_many_arguments)]
pub async fn mark_accepted(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: QuoteId,
    client_id: ClientId,
    order_id: OrderId,
    invoice_id: InvoiceId,
    ip: Option<IpAddr>,
    user_id: Option<UserId>,
    now: DateTime<Utc>,
) -> DbResult<Option<QuoteRow>> {
    Ok(sqlx::query_as::<_, QuoteRow>(
        "update quotes set status = 'accepted', accepted_at = $8, client_id = $3, order_id = $4,
                invoice_id = $5, accepted_ip = $6, accepted_by_user_id = $7, token_hash = null
          where tenant_id = $1 and id = $2 and status = 'sent'
         returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(client_id)
    .bind(order_id)
    .bind(invoice_id)
    .bind(ip)
    .bind(user_id)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

/// Expires every sent quote whose `valid_until` is before `today`; returns
/// the expired quotes.
pub async fn expire_due(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    today: NaiveDate,
) -> DbResult<Vec<QuoteRow>> {
    Ok(sqlx::query_as::<_, QuoteRow>(
        "update quotes set status = 'expired', token_hash = null
          where tenant_id = $1 and status = 'sent' and valid_until < $2
         returning *",
    )
    .bind(tenant_id)
    .bind(today)
    .fetch_all(db)
    .await?)
}

/// Deletes a draft quote (`false` when it is not a draft).
pub async fn delete_draft(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: QuoteId,
) -> DbResult<bool> {
    let res =
        sqlx::query("delete from quotes where tenant_id = $1 and id = $2 and status = 'draft'")
            .bind(tenant_id)
            .bind(id)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Quote counts by status (admin header).
pub async fn status_counts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(QuoteStatus, i64)>> {
    Ok(sqlx::query_as::<_, (QuoteStatus, i64)>(
        "select status, count(*) from quotes where tenant_id = $1 group by status",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// The oldest client with e-mail `email` (case-insensitive), for staff forms
/// that address a client by e-mail.
pub async fn client_for_email(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    email: &str,
) -> DbResult<Option<super::clients::ClientRow>> {
    Ok(sqlx::query_as::<_, super::clients::ClientRow>(
        "select * from clients where tenant_id = $1 and email = $2::citext
          order by created_at limit 1",
    )
    .bind(tenant_id)
    .bind(email.trim())
    .fetch_optional(db)
    .await?)
}
