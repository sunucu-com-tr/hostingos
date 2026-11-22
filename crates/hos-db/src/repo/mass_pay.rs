//! Mass payments (`mass_payments`, `mass_payment_items`): one combined
//! invoice paying several invoices. See migration 0042.

use chrono::{DateTime, Utc};
use hos_core::ids::{ClientId, InvoiceId, TenantId};
use hos_core::string_enum;
use rust_decimal::Decimal;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use crate::error::{DbResult, conflict_on_unique};

hos_core::define_id! {
    /// `mass_payments`.
    pub struct MassPaymentId;
}

string_enum! {
    /// `mass_payments.status`.
    pub enum MassPaymentStatus {
        Open => "open",
        Applied => "applied",
        Cancelled => "cancelled",
    }
}

/// `mass_payments` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct MassPaymentRow {
    pub id: MassPaymentId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub invoice_id: InvoiceId,
    pub status: MassPaymentStatus,
    pub applied_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `mass_payment_items` row (+ the paid invoice's number).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct MassPaymentItemRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub mass_payment_id: MassPaymentId,
    pub invoice_id: InvoiceId,
    pub amount: Decimal,
    pub applied: Option<Decimal>,
    pub applied_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Part of `applied` undone by refunds / chargebacks of the payment
    /// (migration 0052).
    pub reversed: Decimal,
    pub number: Option<String>,
}

/// Records a mass payment for the combined `invoice_id`.
pub async fn create(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    client_id: ClientId,
    invoice_id: InvoiceId,
    items: &[(InvoiceId, Decimal)],
) -> DbResult<MassPaymentRow> {
    let row = sqlx::query_as::<_, MassPaymentRow>(
        "insert into mass_payments (id, tenant_id, client_id, invoice_id) values ($1, $2, $3, $4) returning *",
    )
    .bind(MassPaymentId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(invoice_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(conflict_on_unique("mass payment"))?;
    for (child, amount) in items {
        sqlx::query(
            "insert into mass_payment_items (id, tenant_id, mass_payment_id, invoice_id, amount)
             values ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(row.id)
        .bind(child)
        .bind(amount)
        .execute(&mut *conn)
        .await
        .map_err(conflict_on_unique("mass payment item"))?;
    }
    Ok(row)
}

/// The mass payment whose combined invoice is `invoice_id`.
pub async fn for_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
) -> DbResult<Option<MassPaymentRow>> {
    Ok(sqlx::query_as::<_, MassPaymentRow>(
        "select * from mass_payments where tenant_id = $1 and invoice_id = $2",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_optional(db)
    .await?)
}

/// Locks a mass payment row.
pub async fn lock(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: MassPaymentId,
) -> DbResult<Option<MassPaymentRow>> {
    Ok(sqlx::query_as::<_, MassPaymentRow>(
        "select * from mass_payments where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Lines of a mass payment.
pub async fn items(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: MassPaymentId,
) -> DbResult<Vec<MassPaymentItemRow>> {
    Ok(sqlx::query_as::<_, MassPaymentItemRow>(
        "select m.*, i.number from mass_payment_items m
           join invoices i on i.tenant_id = m.tenant_id and i.id = m.invoice_id
          where m.tenant_id = $1 and m.mass_payment_id = $2
          order by i.due_date, i.created_at",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// Marks a line applied (once): `true` when this call did it.
pub async fn mark_item_applied(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    item_id: Uuid,
    applied: Decimal,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update mass_payment_items set applied = $3, applied_at = $4
          where tenant_id = $1 and id = $2 and applied_at is null",
    )
    .bind(tenant_id)
    .bind(item_id)
    .bind(applied)
    .bind(now)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Moves a mass payment from `open` to `status`.
pub async fn close(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: MassPaymentId,
    status: MassPaymentStatus,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update mass_payments set status = $3,
                applied_at = case when $3 = 'applied' then $4 else applied_at end
          where tenant_id = $1 and id = $2 and status = 'open'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(now)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Open mass payments of `client` that include any of `invoices`.
pub async fn open_containing(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    invoices: &[InvoiceId],
) -> DbResult<Vec<MassPaymentRow>> {
    Ok(sqlx::query_as::<_, MassPaymentRow>(
        "select distinct m.* from mass_payments m
           join mass_payment_items x on x.tenant_id = m.tenant_id and x.mass_payment_id = m.id
          where m.tenant_id = $1 and m.client_id = $2 and m.status = 'open' and x.invoice_id = any($3)",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(invoices)
    .fetch_all(db)
    .await?)
}

/// Open mass payments whose combined invoice is still unpaid and older than
/// `before` (daily clean-up).
pub async fn stale(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    before: DateTime<Utc>,
) -> DbResult<Vec<MassPaymentRow>> {
    Ok(sqlx::query_as::<_, MassPaymentRow>(
        "select m.* from mass_payments m
           join invoices i on i.tenant_id = m.tenant_id and i.id = m.invoice_id
          where m.tenant_id = $1 and m.status = 'open' and m.created_at < $2
            and i.status = 'unpaid' and i.amount_paid = 0 and i.credit_applied = 0",
    )
    .bind(tenant_id)
    .bind(before)
    .fetch_all(db)
    .await?)
}

/// Those of `ids` that cannot join a mass payment: invoices with `credit`
/// lines (add funds, combined invoices).
pub async fn with_credit_lines(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ids: &[InvoiceId],
) -> DbResult<Vec<InvoiceId>> {
    Ok(sqlx::query_scalar::<_, InvoiceId>(
        "select distinct invoice_id from invoice_items
          where tenant_id = $1 and invoice_id = any($2) and item_type = 'credit'",
    )
    .bind(tenant_id)
    .bind(ids)
    .fetch_all(db)
    .await?)
}
