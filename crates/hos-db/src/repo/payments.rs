//! Payments: gateway transactions (idempotent), the client credit ledger and
//! refunds.
//!
//! [`record_payment`] is the one entry point for money coming in (gateway
//! webhook, return URL, manual entry). It is idempotent on
//! `(tenant_id, integration_id, gateway_txn_id)`: replaying the same webhook
//! returns the stored transaction with `duplicate = true` and changes nothing.
//!
//! ## Credit created by a payment
//!
//! Every credit entry that originates from a payment carries its
//! `transaction_id`: the overpayment part (`overpayment`, written by
//! [`record_payment`]) and "add funds" top-ups (`manual`, written by
//! [`add_payment_credit`] once the top-up invoice is paid). A refund of the
//! payment ([`settle_refund`]) first takes back the remaining overpayment
//! credit, then lowers the invoice's `amount_paid` by the rest (taking back
//! top-up credit bought with it) — see [`split_refund`].

use chrono::{DateTime, Utc};
use hos_core::CoreError;
use hos_core::domain::InvoiceStatus;
use hos_core::ids::{
    ClientId, CreditId, IntegrationId, InvoiceId, RefundId, TenantId, TransactionId, UserId,
};
use hos_core::integrations::gateway::RefundStatus;
use hos_core::money::Currency;
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};

use super::invoices::InvoiceRow;
use super::{Json, empty_object};
use crate::enums::{CreditKind, TransactionKind, TransactionStatus};
use crate::error::{DbResult, conflict_on_unique, not_found};
use crate::page::{Page, Paged, fetch_paged};

/// `transactions` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TransactionRow {
    pub id: TransactionId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub invoice_id: Option<InvoiceId>,
    pub integration_id: Option<IntegrationId>,
    pub gateway: String,
    pub gateway_txn_id: Option<String>,
    pub kind: TransactionKind,
    pub status: TransactionStatus,
    pub amount: Decimal,
    pub fee: Decimal,
    pub currency: Currency,
    pub description: Option<String>,
    pub raw: Json,
    pub created_by: Option<UserId>,
    pub occurred_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `credits` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CreditRow {
    pub id: CreditId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub amount: Decimal,
    pub currency: Currency,
    pub kind: CreditKind,
    pub description: String,
    pub invoice_id: Option<InvoiceId>,
    pub transaction_id: Option<TransactionId>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

/// `refunds` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct RefundRow {
    pub id: RefundId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub transaction_id: TransactionId,
    pub invoice_id: Option<InvoiceId>,
    pub refund_transaction_id: Option<TransactionId>,
    pub amount: Decimal,
    pub currency: Currency,
    pub reason: Option<String>,
    pub status: RefundStatus,
    pub to_credit: bool,
    pub gateway_refund_id: Option<String>,
    pub error: Option<String>,
    pub created_by: Option<UserId>,
    pub processed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Columns of [`RefundRow`] — listed explicitly (never `*`) so the result
/// type of a prepared statement does not change when a column is added
/// (poolers keep server-side plans across migrations).
const REFUND_COLUMNS: &str = "id, tenant_id, client_id, transaction_id, invoice_id, refund_transaction_id, \
     amount, currency, reason, status, to_credit, gateway_refund_id, error, created_by, processed_at, \
     created_at, updated_at";

/// A successful incoming payment.
#[derive(Debug, Clone)]
pub struct NewPayment {
    pub client_id: ClientId,
    pub invoice_id: Option<InvoiceId>,
    /// Gateway integration (`None` for manual entries).
    pub integration_id: Option<IntegrationId>,
    /// Gateway kind (`stripe`, `bank_transfer`, ...) or `manual`.
    pub gateway: String,
    /// Provider transaction id — the idempotency key (with `integration_id`).
    pub gateway_txn_id: Option<String>,
    pub amount: Decimal,
    pub fee: Decimal,
    pub currency: Currency,
    pub description: Option<String>,
    pub raw: Json,
    pub occurred_at: Option<DateTime<Utc>>,
    pub created_by: Option<UserId>,
}

impl NewPayment {
    /// Payment of `amount` for `invoice_id` through a gateway.
    pub fn gateway(
        client_id: ClientId,
        invoice_id: InvoiceId,
        integration_id: IntegrationId,
        gateway: &str,
        gateway_txn_id: &str,
        amount: Decimal,
        currency: Currency,
    ) -> Self {
        Self {
            client_id,
            invoice_id: Some(invoice_id),
            integration_id: Some(integration_id),
            gateway: gateway.to_owned(),
            gateway_txn_id: Some(gateway_txn_id.to_owned()),
            amount,
            fee: Decimal::ZERO,
            currency,
            description: None,
            raw: empty_object(),
            occurred_at: None,
            created_by: None,
        }
    }
}

/// Outcome of [`record_payment`].
#[derive(Debug, Clone, PartialEq)]
pub struct PaymentOutcome {
    pub transaction: TransactionRow,
    /// The transaction already existed (webhook replay); nothing was changed.
    pub duplicate: bool,
    /// The invoice after applying the payment.
    pub invoice: Option<InvoiceRow>,
    /// The invoice became fully paid by this payment.
    pub invoice_paid: bool,
    /// Part of the payment that exceeded the balance and went to client credit.
    pub credited: Decimal,
}

/// Records a successful payment and applies it to its invoice, atomically:
///
/// 1. inserts the transaction (`ON CONFLICT DO NOTHING` on the gateway id);
///    a replay returns `duplicate = true` without further changes,
/// 2. locks the invoice, adds `min(amount, balance)` to `amount_paid` and marks
///    it `paid` (with `paid_at`) when the balance reaches zero,
/// 3. puts any excess — or the whole amount if the invoice is no longer open —
///    on the client's credit balance (`credits`, kind `overpayment`).
pub async fn record_payment(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    p: &NewPayment,
) -> DbResult<PaymentOutcome> {
    if p.amount <= Decimal::ZERO {
        return Err(CoreError::validation("payment amount must be positive"));
    }
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;

    let inserted = sqlx::query_as::<_, TransactionRow>(
        "insert into transactions (id, tenant_id, client_id, invoice_id, integration_id, gateway, gateway_txn_id,
                                   kind, status, amount, fee, currency, description, raw, created_by, occurred_at)
         values ($1, $2, $3, $4, $5, $6, $7, 'payment', 'succeeded', $8, $9, $10, $11, $12, $13, coalesce($14, now()))
         on conflict (tenant_id, integration_id, gateway_txn_id) where gateway_txn_id is not null do nothing
         returning *",
    )
    .bind(TransactionId::new())
    .bind(tenant_id)
    .bind(p.client_id)
    .bind(p.invoice_id)
    .bind(p.integration_id)
    .bind(&p.gateway)
    .bind(&p.gateway_txn_id)
    .bind(p.amount)
    .bind(p.fee)
    .bind(p.currency)
    .bind(&p.description)
    .bind(&p.raw)
    .bind(p.created_by)
    .bind(p.occurred_at)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(txn) = inserted else {
        let existing = sqlx::query_as::<_, TransactionRow>(
            "select * from transactions
             where tenant_id = $1 and integration_id is not distinct from $2 and gateway_txn_id = $3",
        )
        .bind(tenant_id)
        .bind(p.integration_id)
        .bind(&p.gateway_txn_id)
        .fetch_one(&mut *tx)
        .await?;
        let invoice = match existing.invoice_id {
            Some(id) => {
                sqlx::query_as::<_, InvoiceRow>(
                    "select * from invoices where tenant_id = $1 and id = $2",
                )
                .bind(tenant_id)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };
        tx.commit().await?;
        return Ok(PaymentOutcome {
            transaction: existing,
            duplicate: true,
            invoice,
            invoice_paid: false,
            credited: Decimal::ZERO,
        });
    };

    let mut outcome = PaymentOutcome {
        transaction: txn.clone(),
        duplicate: false,
        invoice: None,
        invoice_paid: false,
        credited: Decimal::ZERO,
    };

    let mut to_credit = p.amount;
    if let Some(invoice_id) = p.invoice_id {
        let inv = sqlx::query_as::<_, InvoiceRow>(
            "select * from invoices where tenant_id = $1 and id = $2 for update",
        )
        .bind(tenant_id)
        .bind(invoice_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| not_found("invoice", invoice_id))?;
        if inv.client_id != p.client_id {
            return Err(CoreError::validation(
                "payment client does not match the invoice",
            ));
        }
        if inv.currency != p.currency {
            return Err(CoreError::validation(format!(
                "payment currency {} does not match invoice currency {}",
                p.currency, inv.currency
            )));
        }
        let payable = matches!(
            inv.status,
            InvoiceStatus::Unpaid | InvoiceStatus::PaymentPending | InvoiceStatus::Collections
        );
        if payable && inv.balance > Decimal::ZERO {
            let applied = p.amount.min(inv.balance);
            to_credit = p.amount - applied;
            let updated = sqlx::query_as::<_, InvoiceRow>(
                "update invoices set amount_paid = amount_paid + $3,
                        status = case when total - (amount_paid + $3) - credit_applied <= 0 then 'paid'
                                      when status = 'payment_pending' then 'unpaid'
                                      else status end,
                        paid_at = case when total - (amount_paid + $3) - credit_applied <= 0 then now() else paid_at end
                 where tenant_id = $1 and id = $2 returning *",
            )
            .bind(tenant_id)
            .bind(invoice_id)
            .bind(applied)
            .fetch_one(&mut *tx)
            .await?;
            outcome.invoice_paid = updated.status == InvoiceStatus::Paid;
            outcome.invoice = Some(updated);
        } else {
            outcome.invoice = Some(inv);
        }
    }

    if to_credit > Decimal::ZERO {
        insert_credit(
            &mut tx,
            tenant_id,
            p.client_id,
            to_credit,
            p.currency,
            CreditKind::Overpayment,
            "Overpayment",
            p.invoice_id,
            Some(txn.id),
            p.created_by,
        )
        .await?;
        outcome.credited = to_credit;
    }

    tx.commit().await?;
    Ok(outcome)
}

/// Inserts a non-payment transaction (pending, failed, chargeback, ...)
/// idempotently. Returns `(row, inserted)`.
#[allow(clippy::too_many_arguments)]
pub async fn record_transaction(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    invoice_id: Option<InvoiceId>,
    integration_id: Option<IntegrationId>,
    gateway: &str,
    gateway_txn_id: Option<&str>,
    kind: TransactionKind,
    status: TransactionStatus,
    amount: Decimal,
    currency: Currency,
    raw: &Json,
) -> DbResult<(TransactionRow, bool)> {
    let row: Option<(TransactionRow, bool)> = sqlx::query_as::<_, TransactionRowFlag>(
        "with ins as (
             insert into transactions (id, tenant_id, client_id, invoice_id, integration_id, gateway, gateway_txn_id,
                                       kind, status, amount, currency, raw)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             on conflict (tenant_id, integration_id, gateway_txn_id) where gateway_txn_id is not null do nothing
             returning *
         )
         select ins.*, true as inserted from ins
         union all
         select t.*, false as inserted from transactions t
         where not exists (select 1 from ins) and t.tenant_id = $2
           and t.integration_id is not distinct from $5 and t.gateway_txn_id = $7",
    )
    .bind(TransactionId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(invoice_id)
    .bind(integration_id)
    .bind(gateway)
    .bind(gateway_txn_id)
    .bind(kind)
    .bind(status)
    .bind(amount)
    .bind(currency)
    .bind(raw)
    .fetch_optional(db)
    .await?
    .map(|r| (r.txn, r.inserted));
    row.ok_or_else(|| CoreError::Internal("transaction insert returned no row".into()))
}

#[derive(FromRow)]
struct TransactionRowFlag {
    #[sqlx(flatten)]
    txn: TransactionRow,
    inserted: bool,
}

/// Transaction by gateway id (webhook lookups).
pub async fn transaction_by_gateway_id(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: Option<IntegrationId>,
    gateway_txn_id: &str,
) -> DbResult<Option<TransactionRow>> {
    Ok(sqlx::query_as::<_, TransactionRow>(
        "select * from transactions
         where tenant_id = $1 and integration_id is not distinct from $2 and gateway_txn_id = $3",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .bind(gateway_txn_id)
    .fetch_optional(db)
    .await?)
}

/// Transactions of an invoice (oldest first).
pub async fn invoice_transactions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
) -> DbResult<Vec<TransactionRow>> {
    Ok(sqlx::query_as::<_, TransactionRow>(
        "select * from transactions where tenant_id = $1 and invoice_id = $2 order by occurred_at, id",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_all(db)
    .await?)
}

/// Filters of [`list_transactions`].
#[derive(Debug, Clone, Default)]
pub struct TransactionFilter {
    pub client_id: Option<ClientId>,
    pub kind: Option<TransactionKind>,
    pub gateway: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// Transactions of a tenant, newest first.
pub async fn list_transactions<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &TransactionFilter,
    page: Page,
) -> DbResult<Paged<TransactionRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from transactions where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(c) = f.client_id {
                qb.push(" and client_id = ").push_bind(c);
            }
            if let Some(k) = f.kind {
                qb.push(" and kind = ").push_bind(k);
            }
            if let Some(g) = &f.gateway {
                qb.push(" and gateway = ").push_bind(g.clone());
            }
            if let Some(d) = f.from {
                qb.push(" and occurred_at >= ").push_bind(d);
            }
            if let Some(d) = f.to {
                qb.push(" and occurred_at < ").push_bind(d);
            }
        },
        "occurred_at desc, id desc",
        page,
    )
    .await
}

// ---------------------------------------------------------------------------
// credits
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn insert_credit(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    client_id: ClientId,
    amount: Decimal,
    currency: Currency,
    kind: CreditKind,
    description: &str,
    invoice_id: Option<InvoiceId>,
    transaction_id: Option<TransactionId>,
    created_by: Option<UserId>,
) -> DbResult<CreditRow> {
    Ok(sqlx::query_as::<_, CreditRow>(
        "insert into credits (id, tenant_id, client_id, amount, currency, kind, description, invoice_id,
                              transaction_id, created_by)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning *",
    )
    .bind(CreditId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(amount)
    .bind(currency)
    .bind(kind)
    .bind(description)
    .bind(invoice_id)
    .bind(transaction_id)
    .bind(created_by)
    .fetch_one(&mut *conn)
    .await?)
}

/// Current credit balance of a client in `currency`.
pub async fn credit_balance(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    currency: Currency,
) -> DbResult<Decimal> {
    Ok(sqlx::query_scalar::<_, Decimal>(
        "select coalesce(sum(amount), 0) from credits where tenant_id = $1 and client_id = $2 and currency = $3",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(currency)
    .fetch_one(db)
    .await?)
}

/// Adds (positive) or removes (negative) credit. Removing more than the
/// balance fails with `Validation`. Serialised per client (row lock).
#[allow(clippy::too_many_arguments)]
pub async fn adjust_credit(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    client_id: ClientId,
    amount: Decimal,
    currency: Currency,
    kind: CreditKind,
    description: &str,
    created_by: Option<UserId>,
) -> DbResult<CreditRow> {
    if amount.is_zero() {
        return Err(CoreError::validation("credit amount must not be zero"));
    }
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    lock_client(&mut tx, tenant_id, client_id).await?;
    if amount < Decimal::ZERO {
        let balance = credit_balance(&mut *tx, tenant_id, client_id, currency).await?;
        if balance + amount < Decimal::ZERO {
            return Err(CoreError::validation("insufficient credit balance"));
        }
    }
    let row = insert_credit(
        &mut tx,
        tenant_id,
        client_id,
        amount,
        currency,
        kind,
        description,
        None,
        None,
        created_by,
    )
    .await?;
    tx.commit().await?;
    Ok(row)
}

async fn lock_client(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<()> {
    sqlx::query("select id from clients where tenant_id = $1 and id = $2 for update")
        .bind(tenant_id)
        .bind(client_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| not_found("client", client_id))?;
    Ok(())
}

/// Pays (part of) an open invoice from the client's credit balance. Applies
/// `min(max_amount, balance, credit)`; returns the updated invoice and the
/// amount applied (zero when there is nothing to apply).
pub async fn apply_credit_to_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
    max_amount: Option<Decimal>,
    created_by: Option<UserId>,
) -> DbResult<(InvoiceRow, Decimal)> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let inv = sqlx::query_as::<_, InvoiceRow>(
        "select * from invoices where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| not_found("invoice", invoice_id))?;
    if !inv.status.is_open() || inv.balance <= Decimal::ZERO {
        tx.commit().await?;
        return Ok((inv, Decimal::ZERO));
    }
    lock_client(&mut tx, tenant_id, inv.client_id).await?;
    let credit = credit_balance(&mut *tx, tenant_id, inv.client_id, inv.currency).await?;
    let mut applied = inv.balance.min(credit);
    if let Some(m) = max_amount {
        applied = applied.min(m);
    }
    if applied <= Decimal::ZERO {
        tx.commit().await?;
        return Ok((inv, Decimal::ZERO));
    }
    // Written in the client's language, like the other ledger lines
    // (top-ups, mass payments) the client area shows verbatim.
    let locale: Option<String> =
        sqlx::query_scalar("select locale::text from clients where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(inv.client_id)
            .fetch_optional(&mut *tx)
            .await?;
    let number = inv.number.as_deref().unwrap_or("-");
    let description = if locale.as_deref() == Some("tr") {
        format!("{number} numaralı faturaya uygulandı")
    } else {
        format!("Applied to invoice {number}")
    };
    insert_credit(
        &mut tx,
        tenant_id,
        inv.client_id,
        -applied,
        inv.currency,
        CreditKind::InvoicePayment,
        &description,
        Some(invoice_id),
        None,
        created_by,
    )
    .await?;
    let updated = sqlx::query_as::<_, InvoiceRow>(
        "update invoices set credit_applied = credit_applied + $3,
                status = case when total - amount_paid - (credit_applied + $3) <= 0 then 'paid' else status end,
                paid_at = case when total - amount_paid - (credit_applied + $3) <= 0 then now() else paid_at end
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .bind(applied)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((updated, applied))
}

/// Why [`cancel_invoice_with_credit`] left an invoice alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelBlock {
    /// Not a draft / unpaid / payment-pending / collections invoice.
    NotOpen,
    /// A gateway reported the payment as pending (status `payment_pending` or
    /// a `pending` attempt): the money may still arrive.
    PaymentInFlight,
    /// Money was paid on the invoice: refund it first, or cancel with
    /// `paid_to_credit`.
    AmountPaid(Decimal),
}

/// Result of [`cancel_invoice_with_credit`].
#[derive(Debug, Clone, PartialEq)]
pub enum CancelOutcome {
    Cancelled {
        invoice: Box<InvoiceRow>,
        /// Applied credit given back to the client's balance.
        credit_returned: Decimal,
        /// Amount paid moved to the client's balance (`paid_to_credit`).
        paid_to_credit: Decimal,
    },
    Blocked(CancelBlock),
}

/// Cancels an open invoice without stranding money, in one transaction
/// (lock order invoice → client, as in [`apply_credit_to_invoice`]):
///
/// * credit applied to the invoice goes back to the client's balance
///   (`reversal`, linked to the invoice);
/// * an invoice with money paid on it is refused ([`CancelBlock::AmountPaid`])
///   unless staff chose `paid_to_credit`: the amount paid then becomes
///   `overpayment` credit linked to the paying transactions, exactly as a
///   payment arriving for a cancelled invoice would, so a later gateway
///   refund of that payment takes the credit back first ([`split_refund`]);
/// * an invoice whose payment is in flight at a gateway is never cancelled.
///
/// The cancelled invoice keeps `amount_paid = credit_applied = 0`; the ledger
/// rows and transactions keep the history.
pub async fn cancel_invoice_with_credit(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
    paid_to_credit: bool,
    created_by: Option<UserId>,
) -> DbResult<CancelOutcome> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let inv = sqlx::query_as::<_, InvoiceRow>(
        "select * from invoices where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| not_found("invoice", invoice_id))?;
    let block = if !matches!(
        inv.status,
        InvoiceStatus::Draft
            | InvoiceStatus::Unpaid
            | InvoiceStatus::Collections
            | InvoiceStatus::PaymentPending
    ) {
        Some(CancelBlock::NotOpen)
    } else if inv.status == InvoiceStatus::PaymentPending
        || sqlx::query_scalar::<_, bool>(
            "select exists(select 1 from payment_attempts
                            where tenant_id = $1 and invoice_id = $2 and status = 'pending')",
        )
        .bind(tenant_id)
        .bind(invoice_id)
        .fetch_one(&mut *tx)
        .await?
    {
        Some(CancelBlock::PaymentInFlight)
    } else if inv.amount_paid > Decimal::ZERO && !paid_to_credit {
        Some(CancelBlock::AmountPaid(inv.amount_paid))
    } else {
        None
    };
    if let Some(b) = block {
        tx.commit().await?;
        return Ok(CancelOutcome::Blocked(b));
    }
    lock_client(&mut tx, tenant_id, inv.client_id).await?;
    let locale: Option<String> =
        sqlx::query_scalar("select locale::text from clients where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(inv.client_id)
            .fetch_optional(&mut *tx)
            .await?;
    let number = inv.number.as_deref().unwrap_or("-");
    let tr = locale.as_deref() == Some("tr");
    let credit_returned = inv.credit_applied.max(Decimal::ZERO);
    if credit_returned > Decimal::ZERO {
        let description = if tr {
            format!("İptal edilen {number} numaralı faturadan iade")
        } else {
            format!("Returned from cancelled invoice {number}")
        };
        insert_credit(
            &mut tx,
            tenant_id,
            inv.client_id,
            credit_returned,
            inv.currency,
            CreditKind::Reversal,
            &description,
            Some(invoice_id),
            None,
            created_by,
        )
        .await?;
    }
    let moved = inv.amount_paid.max(Decimal::ZERO);
    if moved > Decimal::ZERO {
        let description = if tr {
            format!("İptal edilen {number} numaralı faturanın ödemesi bakiyeye aktarıldı")
        } else {
            format!("Payment of cancelled invoice {number} moved to credit")
        };
        // What each payment still holds on the invoice: refunds take its
        // overpayment credit first ([`split_refund`]), so the invoice part
        // is the amount minus the larger of overpayment and refunds.
        let payments: Vec<(TransactionId, Decimal)> = sqlx::query_as(
            "select t.id,
                    t.amount - greatest(
                      coalesce((select sum(c.amount) from credits c
                                 where c.tenant_id = t.tenant_id and c.transaction_id = t.id
                                   and c.amount > 0 and c.kind = 'overpayment'), 0),
                      coalesce((select sum(r.amount) from refunds r
                                 where r.tenant_id = t.tenant_id and r.transaction_id = t.id
                                   and r.status in ('succeeded', 'pending')), 0))
               from transactions t
              where t.tenant_id = $1 and t.invoice_id = $2 and t.kind = 'payment'
                and t.status = 'succeeded' and t.currency = $3
              order by t.occurred_at desc, t.id desc",
        )
        .bind(tenant_id)
        .bind(invoice_id)
        .bind(inv.currency)
        .fetch_all(&mut *tx)
        .await?;
        let mut left = moved;
        for (txn, held) in payments {
            if left <= Decimal::ZERO {
                break;
            }
            let take = left.min(held);
            if take <= Decimal::ZERO {
                continue;
            }
            insert_credit(
                &mut tx,
                tenant_id,
                inv.client_id,
                take,
                inv.currency,
                CreditKind::Overpayment,
                &description,
                Some(invoice_id),
                Some(txn),
                created_by,
            )
            .await?;
            left -= take;
        }
        if left > Decimal::ZERO {
            // Paid without a recorded transaction (e.g. imported history).
            insert_credit(
                &mut tx,
                tenant_id,
                inv.client_id,
                left,
                inv.currency,
                CreditKind::Overpayment,
                &description,
                Some(invoice_id),
                None,
                created_by,
            )
            .await?;
        }
    }
    let invoice = sqlx::query_as::<_, InvoiceRow>(
        "update invoices set status = 'cancelled', cancelled_at = now(),
                amount_paid = 0, credit_applied = 0
          where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CancelOutcome::Cancelled {
        invoice: Box::new(invoice),
        credit_returned,
        paid_to_credit: moved,
    })
}

/// Adds (positive) credit bought with a payment — the "add funds" top-up of a
/// paid invoice — linked to the paying transaction, so a later refund of that
/// payment takes it back. Serialised per client (row lock).
#[allow(clippy::too_many_arguments)]
pub async fn add_payment_credit(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    client_id: ClientId,
    amount: Decimal,
    currency: Currency,
    description: &str,
    invoice_id: Option<InvoiceId>,
    transaction_id: Option<TransactionId>,
) -> DbResult<CreditRow> {
    if amount <= Decimal::ZERO {
        return Err(CoreError::validation("credit amount must be positive"));
    }
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    lock_client(&mut tx, tenant_id, client_id).await?;
    let row = insert_credit(
        &mut tx,
        tenant_id,
        client_id,
        amount,
        currency,
        CreditKind::Manual,
        description,
        invoice_id,
        transaction_id,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Credit a payment created: `(overpayment, add-funds top-ups)` — the positive
/// `overpayment` and `manual` entries linked to the transaction.
pub async fn payment_credit_origin(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    transaction_id: TransactionId,
) -> DbResult<(Decimal, Decimal)> {
    let (over, funds): (Decimal, Decimal) = sqlx::query_as(
        "select coalesce(sum(amount) filter (where kind = 'overpayment'), 0),
                coalesce(sum(amount) filter (where kind = 'manual'), 0)
         from credits
         where tenant_id = $1 and transaction_id = $2 and amount > 0",
    )
    .bind(tenant_id)
    .bind(transaction_id)
    .fetch_one(db)
    .await?;
    Ok((over, funds))
}

/// Credit ledger of a client, newest first.
pub async fn list_credits(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<CreditRow>> {
    Ok(sqlx::query_as::<_, CreditRow>(
        "select * from credits where tenant_id = $1 and client_id = $2 order by created_at desc, id desc",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// refunds
// ---------------------------------------------------------------------------

/// How a refund of a payment is taken apart (see [`split_refund`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefundSplit {
    /// Taken back from the overpayment credit the payment created (never
    /// touches the invoice).
    pub from_credit: Decimal,
    /// Taken off the invoice's `amount_paid`.
    pub from_invoice: Decimal,
    /// Of `from_invoice`: the part the paid invoice had turned into client
    /// credit ("add funds" top-up), taken back from the credit balance too.
    pub from_funds: Decimal,
}

impl RefundSplit {
    /// Credit to take back from the client when the money goes back to the
    /// card / account.
    pub fn credit_reversal(&self) -> Decimal {
        self.from_credit + self.from_funds
    }
}

/// Splits a refund of `amount` of a payment that created `overpayment` credit
/// and bought `funds` of top-up credit, after `refunded_before` was already
/// refunded. Refunds consume the overpayment credit first, then the part that
/// was applied to the invoice.
pub fn split_refund(
    amount: Decimal,
    overpayment: Decimal,
    funds: Decimal,
    refunded_before: Decimal,
) -> RefundSplit {
    let zero = Decimal::ZERO;
    let amount = amount.max(zero);
    let overpayment = overpayment.max(zero);
    let funds = funds.max(zero);
    let before = refunded_before.max(zero);
    let over_left = overpayment - before.min(overpayment);
    let from_credit = amount.min(over_left);
    let from_invoice = amount - from_credit;
    let invoice_before = (before - overpayment).max(zero);
    let funds_left = funds - invoice_before.min(funds);
    RefundSplit {
        from_credit,
        from_invoice,
        from_funds: from_invoice.min(funds_left),
    }
}

async fn lock_payment(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    transaction_id: TransactionId,
) -> DbResult<TransactionRow> {
    sqlx::query_as::<_, TransactionRow>(
        "select * from transactions where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(transaction_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| not_found("transaction", transaction_id))
}

async fn refunded_sum(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    transaction_id: TransactionId,
    statuses: &[&str],
    except: Option<RefundId>,
) -> DbResult<Decimal> {
    let statuses: Vec<String> = statuses.iter().map(|s| (*s).to_owned()).collect();
    Ok(sqlx::query_scalar(
        "select coalesce(sum(amount), 0) from refunds
         where tenant_id = $1 and transaction_id = $2 and status = any($3)
           and id is distinct from $4",
    )
    .bind(tenant_id)
    .bind(transaction_id)
    .bind(&statuses)
    .bind(except)
    .fetch_one(&mut *conn)
    .await?)
}

/// Validates a new refund of `txn` (locked) and inserts it as `pending`.
#[allow(clippy::too_many_arguments)]
async fn insert_refund(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    txn: &TransactionRow,
    amount: Decimal,
    reason: Option<&str>,
    to_credit: bool,
    gateway_refund_id: Option<&str>,
    created_by: Option<UserId>,
) -> DbResult<RefundRow> {
    if txn.kind != TransactionKind::Payment || txn.status != TransactionStatus::Succeeded {
        return Err(CoreError::validation(
            "only succeeded payments can be refunded",
        ));
    }
    let already = refunded_sum(conn, tenant_id, txn.id, &["pending", "succeeded"], None).await?;
    if already + amount > txn.amount {
        return Err(CoreError::validation(
            "refund exceeds the refundable amount",
        ));
    }
    let sql = format!(
        "insert into refunds (id, tenant_id, client_id, transaction_id, invoice_id, amount, currency, reason,
                              to_credit, gateway_refund_id, integration_id, created_by)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) returning {REFUND_COLUMNS}"
    );
    sqlx::query_as::<_, RefundRow>(&sql)
        .bind(RefundId::new())
        .bind(tenant_id)
        .bind(txn.client_id)
        .bind(txn.id)
        .bind(txn.invoice_id)
        .bind(amount)
        .bind(txn.currency)
        .bind(reason)
        .bind(to_credit)
        .bind(gateway_refund_id)
        .bind(txn.integration_id)
        .bind(created_by)
        .fetch_one(&mut *conn)
        .await
        .map_err(conflict_on_unique("a refund with this gateway refund id"))
}

/// Opens a refund request against a payment (staff action). The amount may not
/// exceed what is left of the payment after earlier (pending or succeeded)
/// refunds. A refund back to the card / account must also be able to take back
/// the credit the payment created: when the client already spent that credit
/// the request fails with `Validation` (nothing is sent to the gateway).
pub async fn create_refund(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    transaction_id: TransactionId,
    amount: Decimal,
    reason: Option<&str>,
    to_credit: bool,
    created_by: Option<UserId>,
) -> DbResult<RefundRow> {
    if amount <= Decimal::ZERO {
        return Err(CoreError::validation("refund amount must be positive"));
    }
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let txn = lock_payment(&mut tx, tenant_id, transaction_id).await?;
    if !to_credit {
        let (over, funds) = payment_credit_origin(&mut *tx, tenant_id, txn.id).await?;
        let before =
            refunded_sum(&mut tx, tenant_id, txn.id, &["pending", "succeeded"], None).await?;
        let split = split_refund(amount, over, funds, before);
        let reversal = split.credit_reversal();
        if reversal > Decimal::ZERO {
            // Credit a mass payment applied to other invoices comes back
            // from them when the refund settles.
            let traced = mass_pay_reversible(&mut tx, tenant_id, txn.invoice_id)
                .await?
                .min(split.from_funds);
            lock_client(&mut tx, tenant_id, txn.client_id).await?;
            let balance =
                credit_balance(&mut *tx, tenant_id, txn.client_id, txn.currency).await? + traced;
            if balance < reversal {
                return Err(CoreError::validation(format!(
                    "this refund takes back {} {cur} of client credit created by the payment, \
                     but the client's credit balance is only {} {cur} (the credit was already \
                     used); refund less or add credit first",
                    reversal.normalize(),
                    balance.max(Decimal::ZERO).normalize(),
                    cur = txn.currency,
                )));
            }
        }
    }
    let row = insert_refund(
        &mut tx, tenant_id, &txn, amount, reason, to_credit, None, created_by,
    )
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Records a refund that already happened at the gateway (webhook, refund made
/// in the gateway dashboard) as `pending`, with its gateway refund id (unique
/// per tenant + integration). No credit check: the money is gone either way —
/// [`settle_refund`] reports any credit shortfall.
pub async fn create_gateway_refund(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    transaction_id: TransactionId,
    amount: Decimal,
    reason: Option<&str>,
    gateway_refund_id: Option<&str>,
) -> DbResult<RefundRow> {
    if amount <= Decimal::ZERO {
        return Err(CoreError::validation("refund amount must be positive"));
    }
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let txn = lock_payment(&mut tx, tenant_id, transaction_id).await?;
    let gateway_refund_id = gateway_refund_id.map(str::trim).filter(|r| !r.is_empty());
    let row = insert_refund(
        &mut tx,
        tenant_id,
        &txn,
        amount,
        reason,
        false,
        gateway_refund_id,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Refund by its gateway refund id (refunds of payments through `integration_id`).
pub async fn refund_by_gateway_id(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    gateway_refund_id: &str,
) -> DbResult<Option<RefundRow>> {
    let sql = format!(
        "select {REFUND_COLUMNS} from refunds
         where tenant_id = $1 and gateway_refund_id = $3
           and coalesce(integration_id,
                        (select t.integration_id from transactions t where t.id = refunds.transaction_id)) = $2
         order by created_at limit 1"
    );
    Ok(sqlx::query_as::<_, RefundRow>(&sql)
        .bind(tenant_id)
        .bind(integration_id)
        .bind(gateway_refund_id)
        .fetch_optional(db)
        .await?)
}

/// Stores the gateway refund id of a refund that has none yet (a gateway that
/// completes refunds asynchronously), so its webhook is matched to it.
/// Returns `None` when nothing changed (already set, or the id is taken).
pub async fn set_refund_gateway_id(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RefundId,
    gateway_refund_id: &str,
) -> DbResult<Option<RefundRow>> {
    let gateway_refund_id = gateway_refund_id.trim();
    if gateway_refund_id.is_empty() {
        return Ok(None);
    }
    let sql = format!(
        "update refunds set gateway_refund_id = $3
         where tenant_id = $1 and id = $2 and gateway_refund_id is null
         returning {REFUND_COLUMNS}"
    );
    match sqlx::query_as::<_, RefundRow>(&sql)
        .bind(tenant_id)
        .bind(id)
        .bind(gateway_refund_id)
        .fetch_optional(db)
        .await
    {
        Ok(row) => Ok(row),
        Err(e) if crate::error::unique_violation(&e).is_some() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Refund by id.
pub async fn get_refund(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RefundId,
) -> DbResult<Option<RefundRow>> {
    let sql = format!("select {REFUND_COLUMNS} from refunds where tenant_id = $1 and id = $2");
    Ok(sqlx::query_as::<_, RefundRow>(&sql)
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// Outcome of [`settle_refund`].
#[derive(Debug, Clone, PartialEq)]
pub struct RefundSettlement {
    /// The refund after settling.
    pub refund: RefundRow,
    /// `false` when the refund was no longer pending (nothing changed).
    pub settled: bool,
    /// How a succeeded refund was taken apart (zero otherwise).
    pub split: RefundSplit,
    /// Credit taken back from the client (negative `reversal` entry).
    pub credit_reversed: Decimal,
    /// Part of `credit_reversed` the client no longer had: the balance went
    /// negative (the client owes it). Staff must be told.
    pub shortfall: Decimal,
    /// Invoices a mass payment had paid with this payment's credit, and how
    /// much of that was undone ([`MemberReversal::Refund`]).
    pub members: Vec<(InvoiceId, Decimal)>,
}

/// What happens to the invoices a mass payment paid when its payment is
/// taken back (see [`settle_refund`], [`settle_chargeback`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberReversal {
    /// Refund: the invoice is marked `refunded` / `partially_refunded`.
    Refund,
    /// Chargeback: the invoice is reopened (`unpaid`).
    Reopen,
}

/// Member invoices of the mass payment whose combined invoice is `funding`,
/// with credit applied and not yet undone, newest first: `(item, invoice,
/// undoable)`.
async fn mass_pay_applications(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    funding: InvoiceId,
) -> DbResult<Vec<(uuid::Uuid, InvoiceId, Decimal)>> {
    Ok(sqlx::query_as::<_, (uuid::Uuid, InvoiceId, Decimal)>(
        "select mi.id, mi.invoice_id, coalesce(mi.applied, 0) - mi.reversed
           from mass_payment_items mi
           join mass_payments mp on mp.tenant_id = mi.tenant_id and mp.id = mi.mass_payment_id
          where mi.tenant_id = $1 and mp.invoice_id = $2 and mi.applied_at is not null
            and coalesce(mi.applied, 0) > mi.reversed
          order by mi.applied_at desc, mi.id desc",
    )
    .bind(tenant_id)
    .bind(funding)
    .fetch_all(&mut *conn)
    .await?)
}

/// Credit of a combined invoice that a refund can win back from the
/// invoices it paid (zero for any other invoice).
async fn mass_pay_reversible(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    funding: Option<InvoiceId>,
) -> DbResult<Decimal> {
    let Some(funding) = funding else {
        return Ok(Decimal::ZERO);
    };
    Ok(mass_pay_applications(conn, tenant_id, funding)
        .await?
        .iter()
        .map(|(_, _, open)| *open)
        .sum())
}

/// Undoes up to `amount` of the credit the combined invoice of `original`
/// (a mass payment) applied to the invoices it paid, newest first: each
/// member's `credit_applied` goes down, the member is marked refunded or
/// reopened (`mode`), the credit comes back to the client (`reversal`
/// entry linked to the payment, so the caller's credit reversal nets out)
/// and `mass_payment_items.reversed` grows. Lock order invoice → client.
async fn unwind_mass_payment(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    original: &TransactionRow,
    amount: Decimal,
    mode: MemberReversal,
) -> DbResult<Vec<(InvoiceId, Decimal)>> {
    let Some(funding) = original.invoice_id else {
        return Ok(Vec::new());
    };
    let mut left = amount.max(Decimal::ZERO);
    let mut undone = Vec::new();
    for (item, invoice_id, open) in mass_pay_applications(conn, tenant_id, funding).await? {
        if left <= Decimal::ZERO {
            break;
        }
        let take = left.min(open);
        let sql = match mode {
            MemberReversal::Refund => {
                "update invoices set credit_applied = greatest(credit_applied - $3, 0),
                        status = case when status in ('paid', 'partially_refunded')
                                      then case when amount_paid + credit_applied - $3 <= 0
                                                then 'refunded' else 'partially_refunded' end
                                      else status end,
                        refunded_at = now()
                  where tenant_id = $1 and id = $2 returning number"
            }
            MemberReversal::Reopen => {
                "update invoices set credit_applied = greatest(credit_applied - $3, 0),
                        status = case when status in ('paid', 'partially_refunded') then 'unpaid' else status end,
                        paid_at = case when status in ('paid', 'partially_refunded') then null else paid_at end
                  where tenant_id = $1 and id = $2 returning number"
            }
        };
        let number: Option<Option<String>> = sqlx::query_scalar(sql)
            .bind(tenant_id)
            .bind(invoice_id)
            .bind(take)
            .fetch_optional(&mut *conn)
            .await?;
        sqlx::query(
            "update mass_payment_items set reversed = reversed + $3 where tenant_id = $1 and id = $2",
        )
        .bind(tenant_id)
        .bind(item)
        .bind(take)
        .execute(&mut *conn)
        .await?;
        undone.push((invoice_id, take, number.flatten()));
        left -= take;
    }
    if undone.is_empty() {
        return Ok(Vec::new());
    }
    lock_client(conn, tenant_id, original.client_id).await?;
    let label = original
        .gateway_txn_id
        .clone()
        .unwrap_or_else(|| original.id.to_string());
    for (invoice_id, take, number) in &undone {
        insert_credit(
            conn,
            tenant_id,
            original.client_id,
            *take,
            original.currency,
            CreditKind::Reversal,
            &format!(
                "Payment {label} taken back: credit applied to invoice {} returned",
                number.as_deref().unwrap_or("-")
            ),
            Some(*invoice_id),
            Some(original.id),
            None,
        )
        .await?;
    }
    Ok(undone.into_iter().map(|(i, t, _)| (i, t)).collect())
}

/// Settles a pending refund. On success:
///
/// * the part paid from the payment's overpayment credit is taken back from
///   the credit balance and does not touch the invoice;
/// * the invoice's `amount_paid` is lowered only by the part that was applied
///   to it (status `refunded` / `partially_refunded`); top-up credit bought
///   with it is taken back too;
/// * a `refund` transaction is written (refund to the card / account), or a
///   `refund` credit entry for the invoice part (`to_credit`).
///
/// Taking back credit the client already spent leaves the balance negative and
/// is reported as `shortfall`. On failure stores `error`. Settlements of one
/// payment are serialised (payment row lock).
pub async fn settle_refund(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    refund_id: RefundId,
    succeeded: bool,
    gateway_refund_id: Option<&str>,
    error: Option<&str>,
) -> DbResult<RefundSettlement> {
    let gateway_refund_id = gateway_refund_id.map(str::trim).filter(|r| !r.is_empty());
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let sql =
        format!("select {REFUND_COLUMNS} from refunds where tenant_id = $1 and id = $2 for update");
    let refund = sqlx::query_as::<_, RefundRow>(&sql)
        .bind(tenant_id)
        .bind(refund_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| not_found("refund", refund_id))?;
    let unchanged = |refund: RefundRow| RefundSettlement {
        refund,
        settled: false,
        split: RefundSplit::default(),
        credit_reversed: Decimal::ZERO,
        shortfall: Decimal::ZERO,
        members: Vec::new(),
    };
    if refund.status != RefundStatus::Pending {
        tx.commit().await?;
        return Ok(unchanged(refund));
    }
    if !succeeded {
        let sql = format!(
            "update refunds set status = 'failed', error = $3, processed_at = now()
             where tenant_id = $1 and id = $2 returning {REFUND_COLUMNS}"
        );
        let row = sqlx::query_as::<_, RefundRow>(&sql)
            .bind(tenant_id)
            .bind(refund_id)
            .bind(error)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(RefundSettlement {
            settled: true,
            ..unchanged(row)
        });
    }

    let original = lock_payment(&mut tx, tenant_id, refund.transaction_id).await?;
    let (over, funds) = payment_credit_origin(&mut *tx, tenant_id, original.id).await?;
    let before = refunded_sum(
        &mut tx,
        tenant_id,
        original.id,
        &["succeeded"],
        Some(refund.id),
    )
    .await?;
    let split = split_refund(refund.amount, over, funds, before);

    // Invoice first (lock order invoice → client, as in `apply_credit_to_invoice`).
    if let Some(invoice_id) = refund.invoice_id
        && split.from_invoice > Decimal::ZERO
    {
        sqlx::query(
            "update invoices set amount_paid = greatest(amount_paid - $3, 0),
                    status = case when status in ('paid', 'partially_refunded')
                                  then case when amount_paid - $3 <= 0 then 'refunded' else 'partially_refunded' end
                                  else status end,
                    refunded_at = now()
             where tenant_id = $1 and id = $2",
        )
        .bind(tenant_id)
        .bind(invoice_id)
        .bind(split.from_invoice)
        .execute(&mut *tx)
        .await?;
    }

    let mut credit_reversed = Decimal::ZERO;
    let mut shortfall = Decimal::ZERO;
    let mut members = Vec::new();
    let label = original
        .gateway_txn_id
        .clone()
        .unwrap_or_else(|| original.id.to_string());
    let refund_txn_id = if refund.to_credit {
        let to_credit = split.from_invoice - split.from_funds;
        if to_credit > Decimal::ZERO {
            lock_client(&mut tx, tenant_id, refund.client_id).await?;
            insert_credit(
                &mut tx,
                tenant_id,
                refund.client_id,
                to_credit,
                refund.currency,
                CreditKind::Refund,
                "Refund to credit",
                refund.invoice_id,
                Some(original.id),
                refund.created_by,
            )
            .await?;
        }
        None
    } else {
        // A mass payment's credit paid other invoices: give it back from
        // them first (they are refunded), so only untraceable spending
        // can leave the balance negative.
        members = unwind_mass_payment(
            &mut tx,
            tenant_id,
            &original,
            split.from_funds,
            MemberReversal::Refund,
        )
        .await?;
        let reversal = split.credit_reversal();
        if reversal > Decimal::ZERO {
            lock_client(&mut tx, tenant_id, refund.client_id).await?;
            let balance =
                credit_balance(&mut *tx, tenant_id, refund.client_id, refund.currency).await?;
            shortfall = (reversal - balance.max(Decimal::ZERO)).max(Decimal::ZERO);
            insert_credit(
                &mut tx,
                tenant_id,
                refund.client_id,
                -reversal,
                refund.currency,
                CreditKind::Reversal,
                &format!("Refund of payment {label}: credit taken back"),
                refund.invoice_id,
                Some(original.id),
                refund.created_by,
            )
            .await?;
            credit_reversed = reversal;
        }
        // The refund transaction's gateway id must not collide with the
        // payment's own (some gateways answer with the payment id).
        let txn_ref = gateway_refund_id
            .or(refund.gateway_refund_id.as_deref())
            .filter(|r| original.gateway_txn_id.as_deref() != Some(*r));
        let id = TransactionId::new();
        sqlx::query(
            "insert into transactions (id, tenant_id, client_id, invoice_id, integration_id, gateway, gateway_txn_id,
                                       kind, status, amount, currency, description)
             values ($1, $2, $3, $4, $5, $6, $7, 'refund', 'succeeded', $8, $9, $10)",
        )
        .bind(id)
        .bind(tenant_id)
        .bind(refund.client_id)
        .bind(refund.invoice_id)
        .bind(original.integration_id)
        .bind(&original.gateway)
        .bind(txn_ref)
        .bind(refund.amount)
        .bind(refund.currency)
        .bind(refund.reason.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(conflict_on_unique("a refund transaction with this gateway id"))?;
        Some(id)
    };

    let sql = format!(
        "update refunds set status = 'succeeded', gateway_refund_id = coalesce($3, gateway_refund_id),
                refund_transaction_id = $4, integration_id = coalesce(integration_id, $5),
                processed_at = now()
         where tenant_id = $1 and id = $2 returning {REFUND_COLUMNS}"
    );
    let row = sqlx::query_as::<_, RefundRow>(&sql)
        .bind(tenant_id)
        .bind(refund_id)
        .bind(gateway_refund_id)
        .bind(refund_txn_id)
        .bind(original.integration_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(conflict_on_unique("a refund with this gateway refund id"))?;
    tx.commit().await?;
    Ok(RefundSettlement {
        refund: row,
        settled: true,
        split,
        credit_reversed,
        shortfall,
        members,
    })
}

/// Outcome of [`settle_chargeback`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChargebackSettlement {
    /// The `chargeback` transaction; `None` when nothing of the payment was
    /// left to charge back (refunded / charged back before).
    pub transaction: Option<TransactionRow>,
    /// Amount charged back (capped at what was left of the payment).
    pub amount: Decimal,
    /// How it was taken apart (as a refund of the same amount).
    pub split: RefundSplit,
    /// The invoice after its payment was reversed (`None` when the invoice
    /// part was zero or the payment had no invoice).
    pub invoice: Option<InvoiceRow>,
    /// Credit taken back from the client (negative `reversal` entry).
    pub credit_reversed: Decimal,
    /// Part of `credit_reversed` the client no longer had: the balance went
    /// negative (the client owes it). Staff must be told.
    pub shortfall: Decimal,
    /// Invoices a mass payment had paid with this payment's credit, reopened
    /// ([`MemberReversal::Reopen`]), with the amount undone on each.
    pub members: Vec<(InvoiceId, Decimal)>,
}

/// Records a chargeback (dispute) of `amount` of the payment `payment_id`,
/// the way [`settle_refund`] settles a gateway-initiated refund — the money
/// left with the cardholder, whatever happened to the credit since:
///
/// * the amount is capped at what is left of the payment after earlier
///   (succeeded) refunds and chargebacks;
/// * the part paid from the payment's overpayment credit and the top-up
///   credit it bought is taken back from the credit balance, even when that
///   leaves it negative (`shortfall`);
/// * the invoice's `amount_paid` is lowered only by the part applied to it,
///   and a paid invoice is reopened (`unpaid`) — except an invoice that only
///   bought credit (add funds, a mass payment's combined invoice): it is
///   closed as `refunded` / `partially_refunded`, so the client does not
///   owe the same money twice (reopened invoice + negative credit);
/// * credit a mass payment applied to other invoices is undone on them
///   first, newest first: they are reopened ([`MemberReversal::Reopen`]);
/// * a `chargeback` transaction is written (`raw.original_transaction_id`
///   links it to the payment).
///
/// Serialised per payment (payment row lock). `payment_id` must be a
/// succeeded payment.
#[allow(clippy::too_many_arguments)]
pub async fn settle_chargeback(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    payment_id: TransactionId,
    amount: Decimal,
    integration_id: Option<IntegrationId>,
    gateway: &str,
    raw: &Json,
) -> DbResult<ChargebackSettlement> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let original = lock_payment(&mut tx, tenant_id, payment_id).await?;
    if original.kind != TransactionKind::Payment {
        return Err(CoreError::validation("only payments can be charged back"));
    }
    let (over, funds) = payment_credit_origin(&mut *tx, tenant_id, original.id).await?;
    let refunded = refunded_sum(&mut tx, tenant_id, original.id, &["succeeded"], None).await?;
    let charged_back: Decimal = sqlx::query_scalar(
        "select coalesce(sum(amount), 0) from transactions
         where tenant_id = $1 and kind = 'chargeback' and status = 'succeeded'
           and raw ->> 'original_transaction_id' = $2::text",
    )
    .bind(tenant_id)
    .bind(original.id)
    .fetch_one(&mut *tx)
    .await?;
    let before = refunded + charged_back;
    let amount = amount
        .max(Decimal::ZERO)
        .min((original.amount - before).max(Decimal::ZERO));
    if amount <= Decimal::ZERO {
        tx.commit().await?;
        return Ok(ChargebackSettlement {
            transaction: None,
            amount: Decimal::ZERO,
            split: RefundSplit::default(),
            invoice: None,
            credit_reversed: Decimal::ZERO,
            shortfall: Decimal::ZERO,
            members: Vec::new(),
        });
    }
    let split = split_refund(amount, over, funds, before);

    // Invoice first (lock order invoice → client, as in `settle_refund`).
    let mut invoice = None;
    if let Some(invoice_id) = original.invoice_id
        && split.from_invoice > Decimal::ZERO
    {
        invoice = if split.from_funds >= split.from_invoice {
            // Only credit was bought: close it, never reopen it.
            sqlx::query_as::<_, InvoiceRow>(
                "update invoices set amount_paid = greatest(amount_paid - $3, 0),
                        status = case when status in ('paid', 'partially_refunded')
                                      then case when amount_paid - $3 <= 0 then 'refunded' else 'partially_refunded' end
                                      else status end,
                        refunded_at = now()
                  where tenant_id = $1 and id = $2 returning *",
            )
            .bind(tenant_id)
            .bind(invoice_id)
            .bind(split.from_invoice)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            super::billing_support::reverse_invoice_payment(
                &mut *tx,
                tenant_id,
                invoice_id,
                split.from_invoice,
            )
            .await?
        };
    }
    let members = unwind_mass_payment(
        &mut tx,
        tenant_id,
        &original,
        split.from_funds,
        MemberReversal::Reopen,
    )
    .await?;

    let mut credit_reversed = Decimal::ZERO;
    let mut shortfall = Decimal::ZERO;
    let reversal = split.credit_reversal();
    if reversal > Decimal::ZERO {
        lock_client(&mut tx, tenant_id, original.client_id).await?;
        let balance =
            credit_balance(&mut *tx, tenant_id, original.client_id, original.currency).await?;
        shortfall = (reversal - balance.max(Decimal::ZERO)).max(Decimal::ZERO);
        let label = original
            .gateway_txn_id
            .clone()
            .unwrap_or_else(|| original.id.to_string());
        insert_credit(
            &mut tx,
            tenant_id,
            original.client_id,
            -reversal,
            original.currency,
            CreditKind::Reversal,
            &format!("Chargeback of payment {label}: credit taken back"),
            original.invoice_id,
            Some(original.id),
            None,
        )
        .await?;
        credit_reversed = reversal;
    }

    let mut raw = match raw {
        serde_json::Value::Object(m) => m.clone(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("raw".into(), other.clone());
            m
        }
    };
    raw.insert(
        "original_transaction_id".into(),
        serde_json::Value::String(original.id.to_string()),
    );
    let (transaction, _) = record_transaction(
        &mut *tx,
        tenant_id,
        original.client_id,
        original.invoice_id,
        integration_id.or(original.integration_id),
        gateway,
        None,
        TransactionKind::Chargeback,
        TransactionStatus::Succeeded,
        amount,
        original.currency,
        &serde_json::Value::Object(raw),
    )
    .await?;
    tx.commit().await?;
    Ok(ChargebackSettlement {
        transaction: Some(transaction),
        amount,
        split,
        invoice,
        credit_reversed,
        shortfall,
        members,
    })
}

/// Settles a pending refund ([`settle_refund`]) and returns the refund row.
pub async fn complete_refund(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    refund_id: RefundId,
    succeeded: bool,
    gateway_refund_id: Option<&str>,
    error: Option<&str>,
) -> DbResult<RefundRow> {
    Ok(settle_refund(
        conn,
        tenant_id,
        refund_id,
        succeeded,
        gateway_refund_id,
        error,
    )
    .await?
    .refund)
}

/// Refunds of a payment.
pub async fn transaction_refunds(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    transaction_id: TransactionId,
) -> DbResult<Vec<RefundRow>> {
    let sql = format!(
        "select {REFUND_COLUMNS} from refunds where tenant_id = $1 and transaction_id = $2
         order by created_at, id"
    );
    Ok(sqlx::query_as::<_, RefundRow>(&sql)
        .bind(tenant_id)
        .bind(transaction_id)
        .fetch_all(db)
        .await?)
}

/// Open (unpaid / payment pending) credit top-up invoices of a client in
/// `currency`: their total counts against the credit balance cap.
pub async fn open_topups_total(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    currency: Currency,
) -> DbResult<Decimal> {
    Ok(sqlx::query_scalar::<_, Decimal>(
        "select coalesce(sum(i.balance), 0) from invoices i
          where i.tenant_id = $1 and i.client_id = $2 and i.currency = $3
            and i.status in ('unpaid', 'payment_pending')
            and exists(select 1 from invoice_items ii
                        where ii.tenant_id = i.tenant_id and ii.invoice_id = i.id and ii.item_type = 'credit')
            and not exists(select 1 from mass_payments mp where mp.tenant_id = i.tenant_id and mp.invoice_id = i.id)",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(currency)
    .fetch_one(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn refunds_take_overpayment_credit_first() {
        // 150 paid for a 120 invoice: 30 overpayment credit.
        let s = split_refund(d("20"), d("30"), d("0"), d("0"));
        assert_eq!(
            (s.from_credit, s.from_invoice, s.from_funds),
            (d("20"), d("0"), d("0"))
        );
        let s = split_refund(d("130"), d("30"), d("0"), d("20"));
        assert_eq!((s.from_credit, s.from_invoice), (d("10"), d("120")));
        assert_eq!(s.credit_reversal(), d("10"));
        let s = split_refund(d("150"), d("30"), d("0"), d("0"));
        assert_eq!((s.from_credit, s.from_invoice), (d("30"), d("120")));
    }

    #[test]
    fn refunds_of_top_ups_take_the_bought_credit_back() {
        // 100 paid for a 100 "add funds" invoice.
        let s = split_refund(d("40"), d("0"), d("100"), d("0"));
        assert_eq!(
            (s.from_credit, s.from_invoice, s.from_funds),
            (d("0"), d("40"), d("40"))
        );
        let s = split_refund(d("60"), d("0"), d("100"), d("40"));
        assert_eq!((s.from_invoice, s.from_funds), (d("60"), d("60")));
        // Payments without credit only touch the invoice.
        let s = split_refund(d("50"), d("0"), d("0"), d("0"));
        assert_eq!(s.credit_reversal(), d("0"));
        assert_eq!(s.from_invoice, d("50"));
    }
}
