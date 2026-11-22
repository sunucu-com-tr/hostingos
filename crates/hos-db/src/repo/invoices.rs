//! Invoices, invoice lines, tax breakdown and gap-free numbering.
//!
//! ## Numbering
//!
//! Draft invoices have no number. A number is assigned when the invoice is
//! issued ([`create_invoice`] with `issue = true`, or [`issue_invoice`]) by
//! [`allocate_invoice_number`], which locks the tenant's `invoice_sequences`
//! row with `FOR UPDATE` inside the same transaction that writes the invoice.
//! A rollback therefore also rolls the counter back: numbers are strictly
//! consecutive per tenant, without gaps, even under concurrency (concurrent
//! issuers queue on the row lock).
//!
//! ## Totals
//!
//! Line `amount`s are rounded to the currency's minor unit when written.
//! [`recalculate_invoice`] recomputes `subtotal`, `discount_total`, each tax
//! line (`invoice_tax_lines`: non-compound lines on the taxable base, compound
//! lines on base + preceding taxes) and `total = subtotal + tax_total`; see
//! [`compute_totals`].

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use hos_core::CoreError;
use hos_core::domain::InvoiceStatus;
use hos_core::ids::{
    ClientId, DomainId, IntegrationId, InvoiceId, InvoiceItemId, OrderItemId, ServiceAddonId,
    ServiceId, TaxRuleId, TenantId,
};
use hos_core::money::{Currency, Money};
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres, QueryBuilder};

use super::Json;
use super::catalog::TaxRuleRow;
use crate::enums::InvoiceItemType;
use crate::error::{DbResult, like_contains, not_found};
use crate::ids::{InvoiceSequenceId, InvoiceTaxLineId};
use crate::page::{Page, Paged, fetch_paged};

/// Statuses that still expect money.
pub const OPEN_STATUSES: [InvoiceStatus; 3] = [
    InvoiceStatus::Unpaid,
    InvoiceStatus::PaymentPending,
    InvoiceStatus::Collections,
];

/// `invoices` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct InvoiceRow {
    pub id: InvoiceId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub number: Option<String>,
    pub seq: Option<i64>,
    pub status: InvoiceStatus,
    pub currency: Currency,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    pub paid_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub refunded_at: Option<DateTime<Utc>>,
    pub subtotal: Decimal,
    pub discount_total: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub amount_paid: Decimal,
    pub credit_applied: Decimal,
    /// `total - amount_paid - credit_applied` (generated column).
    pub balance: Decimal,
    pub payment_integration_id: Option<IntegrationId>,
    pub billing_snapshot: Json,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub reminder_count: i32,
    pub last_reminder_at: Option<DateTime<Utc>>,
    pub late_fee_applied_at: Option<DateTime<Utc>>,
    pub external_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl InvoiceRow {
    /// Open and past due as of `today`.
    pub fn is_overdue(&self, today: NaiveDate) -> bool {
        self.status.is_overdue(self.due_date, today)
    }

    /// Balance as [`Money`].
    pub fn balance_money(&self) -> Money {
        Money::new(self.balance, self.currency)
    }

    /// Total as [`Money`].
    pub fn total_money(&self) -> Money {
        Money::new(self.total, self.currency)
    }
}

/// `invoice_items` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct InvoiceItemRow {
    pub id: InvoiceItemId,
    pub tenant_id: TenantId,
    pub invoice_id: InvoiceId,
    pub item_type: InvoiceItemType,
    pub service_id: Option<ServiceId>,
    pub service_addon_id: Option<ServiceAddonId>,
    pub domain_id: Option<DomainId>,
    pub order_item_id: Option<OrderItemId>,
    pub description: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub amount: Decimal,
    pub taxable: bool,
    pub period_start: Option<NaiveDate>,
    pub period_end: Option<NaiveDate>,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `invoice_tax_lines` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct InvoiceTaxLineRow {
    pub id: InvoiceTaxLineId,
    pub tenant_id: TenantId,
    pub invoice_id: InvoiceId,
    pub tax_rule_id: Option<TaxRuleId>,
    pub name: String,
    pub rate: Decimal,
    pub is_compound: bool,
    pub sort_order: i32,
    pub taxable_amount: Decimal,
    pub tax_amount: Decimal,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `invoice_sequences` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct InvoiceSequenceRow {
    pub id: InvoiceSequenceId,
    pub tenant_id: TenantId,
    pub prefix: String,
    pub next_value: i64,
    pub padding: i16,
    pub reset_yearly: bool,
    pub current_year: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One line of a new invoice. `amount = round(quantity × unit_price)`.
#[derive(Debug, Clone)]
pub struct NewInvoiceItem {
    pub item_type: InvoiceItemType,
    pub service_id: Option<ServiceId>,
    pub service_addon_id: Option<ServiceAddonId>,
    pub domain_id: Option<DomainId>,
    pub order_item_id: Option<OrderItemId>,
    pub description: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub taxable: bool,
    pub period_start: Option<NaiveDate>,
    pub period_end: Option<NaiveDate>,
}

impl NewInvoiceItem {
    /// A taxable manual line with quantity 1.
    pub fn manual(description: &str, amount: Decimal) -> Self {
        Self {
            item_type: InvoiceItemType::Manual,
            service_id: None,
            service_addon_id: None,
            domain_id: None,
            order_item_id: None,
            description: description.to_owned(),
            quantity: Decimal::ONE,
            unit_price: amount,
            taxable: true,
            period_start: None,
            period_end: None,
        }
    }

    /// A taxable service renewal line for `[period_start, period_end]`.
    pub fn service(
        service_id: ServiceId,
        description: &str,
        amount: Decimal,
        period_start: NaiveDate,
        period_end: Option<NaiveDate>,
    ) -> Self {
        Self {
            item_type: InvoiceItemType::Service,
            service_id: Some(service_id),
            period_start: Some(period_start),
            period_end,
            ..Self::manual(description, amount)
        }
    }
}

/// A tax rate applied to an invoice (snapshot of a tax rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxLineInput {
    pub tax_rule_id: Option<TaxRuleId>,
    pub name: String,
    /// Percent, e.g. `20` for KDV %20.
    pub rate: Decimal,
    pub is_compound: bool,
}

impl From<&TaxRuleRow> for TaxLineInput {
    fn from(r: &TaxRuleRow) -> Self {
        Self {
            tax_rule_id: Some(r.id),
            name: r.name.clone(),
            rate: r.rate,
            is_compound: r.is_compound,
        }
    }
}

/// Input of [`create_invoice`].
#[derive(Debug, Clone)]
pub struct NewInvoice {
    pub client_id: ClientId,
    pub currency: Currency,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    /// `true`: issue now (status `unpaid`, number assigned); `false`: draft.
    pub issue: bool,
    pub payment_integration_id: Option<IntegrationId>,
    pub notes: Option<String>,
    /// Client name / address / tax ids at issue time.
    pub billing_snapshot: Json,
    pub items: Vec<NewInvoiceItem>,
    /// Tax rates in application order (empty for tax-exempt clients).
    pub taxes: Vec<TaxLineInput>,
}

// ---------------------------------------------------------------------------
// totals (pure)
// ---------------------------------------------------------------------------

/// A line as seen by [`compute_totals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TotalsLine {
    pub amount: Decimal,
    pub taxable: bool,
    pub is_discount: bool,
}

/// A tax rate as seen by [`compute_totals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TotalsTax {
    pub rate: Decimal,
    pub is_compound: bool,
}

/// Result of [`compute_totals`]; `taxes[i] = (taxable_amount, tax_amount)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceTotals {
    pub subtotal: Decimal,
    pub discount_total: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub taxes: Vec<(Decimal, Decimal)>,
}

/// Computes invoice totals, rounding every tax amount half-away-from-zero to
/// the currency's minor unit (hos-core `Money::round`).
pub fn compute_totals(
    currency: Currency,
    lines: &[TotalsLine],
    taxes: &[TotalsTax],
) -> InvoiceTotals {
    let round = |d: Decimal| Money::new(d, currency).round().amount;
    let subtotal = round(lines.iter().map(|l| l.amount).sum());
    let discount_total = round(
        -lines
            .iter()
            .filter(|l| l.is_discount)
            .map(|l| l.amount)
            .sum::<Decimal>(),
    );
    let base = round(lines.iter().filter(|l| l.taxable).map(|l| l.amount).sum());
    let mut out = Vec::with_capacity(taxes.len());
    let mut tax_total = Decimal::ZERO;
    for t in taxes {
        let taxable = if t.is_compound {
            base + tax_total
        } else {
            base
        };
        let amount = round(taxable * t.rate / Decimal::ONE_HUNDRED);
        tax_total += amount;
        out.push((taxable, amount));
    }
    InvoiceTotals {
        subtotal,
        discount_total: discount_total.max(Decimal::ZERO),
        tax_total,
        total: subtotal + tax_total,
        taxes: out,
    }
}

/// Formats an invoice number: `<prefix>[<year>]<zero-padded value>`.
pub fn format_invoice_number(
    prefix: &str,
    year: Option<i32>,
    value: i64,
    padding: usize,
) -> String {
    match year {
        Some(y) => format!("{prefix}{y}{value:0>padding$}"),
        None => format!("{prefix}{value:0>padding$}"),
    }
}

// ---------------------------------------------------------------------------
// numbering
// ---------------------------------------------------------------------------

/// Allocates the next invoice number of the tenant. **Must run inside the
/// transaction that writes the invoice** (the sequence row stays locked until
/// commit; a rollback returns the number). Returns `(number, seq)`.
pub async fn allocate_invoice_number(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    issue_date: NaiveDate,
) -> DbResult<(String, i64)> {
    sqlx::query("insert into invoice_sequences (id, tenant_id) values ($1, $2) on conflict (tenant_id) do nothing")
        .bind(InvoiceSequenceId::new())
        .bind(tenant_id)
        .execute(&mut *conn)
        .await?;
    let seq = sqlx::query_as::<_, InvoiceSequenceRow>(
        "select * from invoice_sequences where tenant_id = $1 for update",
    )
    .bind(tenant_id)
    .fetch_one(&mut *conn)
    .await?;
    let year = issue_date.year();
    let value = if seq.reset_yearly && seq.current_year != Some(year) {
        1
    } else {
        seq.next_value
    };
    sqlx::query("update invoice_sequences set next_value = $2, current_year = $3 where id = $1")
        .bind(seq.id)
        .bind(value + 1)
        .bind(year)
        .execute(&mut *conn)
        .await?;
    let padding = usize::try_from(seq.padding).unwrap_or(6);
    let number = format_invoice_number(
        &seq.prefix,
        seq.reset_yearly.then_some(year),
        value,
        padding,
    );
    Ok((number, value))
}

/// Configures the tenant's numbering (prefix, padding, yearly reset and,
/// optionally, the next value — e.g. when migrating from WHMCS).
pub async fn configure_invoice_sequence(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    prefix: &str,
    padding: i16,
    reset_yearly: bool,
    next_value: Option<i64>,
) -> DbResult<InvoiceSequenceRow> {
    Ok(sqlx::query_as::<_, InvoiceSequenceRow>(
        "insert into invoice_sequences (id, tenant_id, prefix, padding, reset_yearly, next_value)
         values ($1, $2, $3, $4, $5, coalesce($6, 1))
         on conflict (tenant_id) do update
            set prefix = excluded.prefix, padding = excluded.padding, reset_yearly = excluded.reset_yearly,
                next_value = coalesce($6, invoice_sequences.next_value)
         returning *",
    )
    .bind(InvoiceSequenceId::new())
    .bind(tenant_id)
    .bind(prefix)
    .bind(padding)
    .bind(reset_yearly)
    .bind(next_value)
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// create / modify
// ---------------------------------------------------------------------------

async fn insert_items(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
    currency: Currency,
    first_sort: i32,
    items: &[NewInvoiceItem],
) -> DbResult<Vec<InvoiceItemRow>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "insert into invoice_items (id, tenant_id, invoice_id, item_type, service_id, service_addon_id, domain_id,
                                    order_item_id, description, quantity, unit_price, amount, taxable,
                                    period_start, period_end, sort_order) ",
    );
    qb.push_values(items.iter().enumerate(), |mut b, (i, it)| {
        let amount = Money::new(it.quantity * it.unit_price, currency)
            .round()
            .amount;
        b.push_bind(InvoiceItemId::new())
            .push_bind(tenant_id)
            .push_bind(invoice_id)
            .push_bind(it.item_type)
            .push_bind(it.service_id)
            .push_bind(it.service_addon_id)
            .push_bind(it.domain_id)
            .push_bind(it.order_item_id)
            .push_bind(it.description.clone())
            .push_bind(it.quantity)
            .push_bind(it.unit_price)
            .push_bind(amount)
            .push_bind(it.taxable)
            .push_bind(it.period_start)
            .push_bind(it.period_end)
            .push_bind(first_sort + i32::try_from(i).unwrap_or(i32::MAX));
    });
    qb.push(" returning *");
    Ok(qb
        .build_query_as::<InvoiceItemRow>()
        .fetch_all(&mut *conn)
        .await?)
}

async fn insert_tax_lines(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
    taxes: &[TaxLineInput],
) -> DbResult<()> {
    if taxes.is_empty() {
        return Ok(());
    }
    let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new(
        "insert into invoice_tax_lines (id, tenant_id, invoice_id, tax_rule_id, name, rate, is_compound, sort_order) ",
    );
    qb.push_values(taxes.iter().enumerate(), |mut b, (i, t)| {
        b.push_bind(InvoiceTaxLineId::new())
            .push_bind(tenant_id)
            .push_bind(invoice_id)
            .push_bind(t.tax_rule_id)
            .push_bind(t.name.clone())
            .push_bind(t.rate)
            .push_bind(t.is_compound)
            .push_bind(i32::try_from(i).unwrap_or(i32::MAX));
    });
    qb.build().execute(&mut *conn).await?;
    Ok(())
}

/// Locks the invoice row; returns `(currency, status)`.
async fn lock_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<(Currency, InvoiceStatus)> {
    sqlx::query_as::<_, (Currency, InvoiceStatus)>(
        "select currency, status from invoices where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| not_found("invoice", id))
}

/// Recomputes totals of a locked invoice (no transaction handling).
async fn recalc_locked(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
    currency: Currency,
) -> DbResult<InvoiceRow> {
    let lines: Vec<(Decimal, bool, InvoiceItemType)> = sqlx::query_as(
        "select amount, taxable, item_type from invoice_items where tenant_id = $1 and invoice_id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let taxes: Vec<(InvoiceTaxLineId, Decimal, bool)> = sqlx::query_as(
        "select id, rate, is_compound from invoice_tax_lines
         where tenant_id = $1 and invoice_id = $2 order by sort_order, id",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;

    let totals = compute_totals(
        currency,
        &lines
            .iter()
            .map(|(amount, taxable, kind)| TotalsLine {
                amount: *amount,
                taxable: *taxable,
                is_discount: *kind == InvoiceItemType::Discount,
            })
            .collect::<Vec<_>>(),
        &taxes
            .iter()
            .map(|(_, rate, is_compound)| TotalsTax {
                rate: *rate,
                is_compound: *is_compound,
            })
            .collect::<Vec<_>>(),
    );

    if !taxes.is_empty() {
        let ids: Vec<InvoiceTaxLineId> = taxes.iter().map(|t| t.0).collect();
        let bases: Vec<Decimal> = totals.taxes.iter().map(|t| t.0).collect();
        let amounts: Vec<Decimal> = totals.taxes.iter().map(|t| t.1).collect();
        sqlx::query(
            "update invoice_tax_lines t set taxable_amount = u.base, tax_amount = u.amount
             from unnest($1::uuid[], $2::numeric[], $3::numeric[]) as u(id, base, amount)
             where t.id = u.id",
        )
        .bind(&ids)
        .bind(&bases)
        .bind(&amounts)
        .execute(&mut *conn)
        .await?;
    }

    Ok(sqlx::query_as::<_, InvoiceRow>(
        "update invoices set subtotal = $3, discount_total = $4, tax_total = $5, total = $6
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(totals.subtotal)
    .bind(totals.discount_total)
    .bind(totals.tax_total)
    .bind(totals.total)
    .fetch_one(&mut *conn)
    .await?)
}

/// Creates an invoice with lines and tax lines and computes its totals, all in
/// one transaction. With `issue = true` the invoice is `unpaid` and numbered.
pub async fn create_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    new: &NewInvoice,
) -> DbResult<InvoiceRow> {
    if new.due_date < new.issue_date {
        return Err(CoreError::validation("due date is before the issue date"));
    }
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let (number, seq) = if new.issue {
        let (n, s) = allocate_invoice_number(&mut tx, tenant_id, new.issue_date).await?;
        (Some(n), Some(s))
    } else {
        (None, None)
    };
    let status = if new.issue {
        InvoiceStatus::Unpaid
    } else {
        InvoiceStatus::Draft
    };
    let id = InvoiceId::new();
    sqlx::query(
        "insert into invoices (id, tenant_id, client_id, number, seq, status, currency, issue_date, due_date,
                               payment_integration_id, billing_snapshot, notes)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(new.client_id)
    .bind(&number)
    .bind(seq)
    .bind(status)
    .bind(new.currency)
    .bind(new.issue_date)
    .bind(new.due_date)
    .bind(new.payment_integration_id)
    .bind(&new.billing_snapshot)
    .bind(&new.notes)
    .execute(&mut *tx)
    .await?;
    insert_items(&mut tx, tenant_id, id, new.currency, 0, &new.items).await?;
    insert_tax_lines(&mut tx, tenant_id, id, &new.taxes).await?;
    let invoice = recalc_locked(&mut tx, tenant_id, id, new.currency).await?;
    tx.commit().await?;
    Ok(invoice)
}

/// Issues a draft: assigns the next number and moves it to `unpaid`.
pub async fn issue_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<InvoiceRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let (_, status) = lock_invoice(&mut tx, tenant_id, id).await?;
    status.transition(InvoiceStatus::Unpaid)?;
    let issue_date: NaiveDate = sqlx::query_scalar("select issue_date from invoices where id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let (number, seq) = allocate_invoice_number(&mut tx, tenant_id, issue_date).await?;
    let row = sqlx::query_as::<_, InvoiceRow>(
        "update invoices set status = 'unpaid', number = $3, seq = $4 where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(number)
    .bind(seq)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Recomputes and stores the totals of an invoice (transactional, row-locked).
pub async fn recalculate_invoice(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<InvoiceRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let (currency, _) = lock_invoice(&mut tx, tenant_id, id).await?;
    let row = recalc_locked(&mut tx, tenant_id, id, currency).await?;
    tx.commit().await?;
    Ok(row)
}

fn ensure_editable(status: InvoiceStatus) -> DbResult<()> {
    if matches!(status, InvoiceStatus::Draft | InvoiceStatus::Unpaid) {
        Ok(())
    } else {
        Err(CoreError::validation(format!(
            "invoice lines cannot be changed while {status}"
        )))
    }
}

/// Appends lines to a draft / unpaid invoice and recalculates.
pub async fn add_invoice_items(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
    items: &[NewInvoiceItem],
) -> DbResult<InvoiceRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let (currency, status) = lock_invoice(&mut tx, tenant_id, id).await?;
    ensure_editable(status)?;
    let next_sort: i32 = sqlx::query_scalar(
        "select coalesce(max(sort_order) + 1, 0) from invoice_items where tenant_id = $1 and invoice_id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    insert_items(&mut tx, tenant_id, id, currency, next_sort, items).await?;
    let row = recalc_locked(&mut tx, tenant_id, id, currency).await?;
    tx.commit().await?;
    Ok(row)
}

/// Removes a line from a draft / unpaid invoice and recalculates.
pub async fn remove_invoice_item(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
    item_id: InvoiceItemId,
) -> DbResult<InvoiceRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let (currency, status) = lock_invoice(&mut tx, tenant_id, id).await?;
    ensure_editable(status)?;
    sqlx::query("delete from invoice_items where tenant_id = $1 and invoice_id = $2 and id = $3")
        .bind(tenant_id)
        .bind(id)
        .bind(item_id)
        .execute(&mut *tx)
        .await?;
    let row = recalc_locked(&mut tx, tenant_id, id, currency).await?;
    tx.commit().await?;
    Ok(row)
}

/// Replaces the tax lines of a draft / unpaid invoice and recalculates.
pub async fn set_tax_lines(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
    taxes: &[TaxLineInput],
) -> DbResult<InvoiceRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let (currency, status) = lock_invoice(&mut tx, tenant_id, id).await?;
    ensure_editable(status)?;
    sqlx::query("delete from invoice_tax_lines where tenant_id = $1 and invoice_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_tax_lines(&mut tx, tenant_id, id, taxes).await?;
    let row = recalc_locked(&mut tx, tenant_id, id, currency).await?;
    tx.commit().await?;
    Ok(row)
}

// ---------------------------------------------------------------------------
// queries
// ---------------------------------------------------------------------------

/// Invoice by id.
pub async fn get_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<Option<InvoiceRow>> {
    Ok(
        sqlx::query_as::<_, InvoiceRow>("select * from invoices where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Invoice by number.
pub async fn invoice_by_number(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    number: &str,
) -> DbResult<Option<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select * from invoices where tenant_id = $1 and number = $2",
    )
    .bind(tenant_id)
    .bind(number)
    .fetch_optional(db)
    .await?)
}

/// Lines of an invoice.
pub async fn invoice_items(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<Vec<InvoiceItemRow>> {
    Ok(sqlx::query_as::<_, InvoiceItemRow>(
        "select * from invoice_items where tenant_id = $1 and invoice_id = $2 order by sort_order, id",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// Tax breakdown of an invoice.
pub async fn invoice_tax_lines(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<Vec<InvoiceTaxLineRow>> {
    Ok(sqlx::query_as::<_, InvoiceTaxLineRow>(
        "select * from invoice_tax_lines where tenant_id = $1 and invoice_id = $2 order by sort_order, id",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// An invoice with its lines and tax breakdown.
#[derive(Debug, Clone, PartialEq)]
pub struct InvoiceDetail {
    pub invoice: InvoiceRow,
    pub items: Vec<InvoiceItemRow>,
    pub tax_lines: Vec<InvoiceTaxLineRow>,
}

/// Invoice + lines + taxes (three queries on one connection).
pub async fn invoice_detail<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<Option<InvoiceDetail>> {
    let mut conn = db.acquire().await?;
    let Some(invoice) = get_invoice(&mut *conn, tenant_id, id).await? else {
        return Ok(None);
    };
    let items = invoice_items(&mut *conn, tenant_id, id).await?;
    let tax_lines = invoice_tax_lines(&mut *conn, tenant_id, id).await?;
    Ok(Some(InvoiceDetail {
        invoice,
        items,
        tax_lines,
    }))
}

/// Filters of [`list_invoices`].
#[derive(Debug, Clone, Default)]
pub struct InvoiceFilter {
    pub client_id: Option<ClientId>,
    pub status: Option<InvoiceStatus>,
    /// Only open invoices due before this date.
    pub overdue_as_of: Option<NaiveDate>,
    /// Matches invoice number, client e-mail or name.
    pub search: Option<String>,
}

/// Invoice list row with the client's display name.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct InvoiceListRow {
    #[sqlx(flatten)]
    pub invoice: InvoiceRow,
    pub client_name: String,
}

/// Invoices of a tenant, newest first.
pub async fn list_invoices<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &InvoiceFilter,
    page: Page,
) -> DbResult<Paged<InvoiceListRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        "select i.*, coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name",
        |qb| {
            qb.push("from invoices i join clients c on c.id = i.client_id where i.tenant_id = ")
                .push_bind(tenant_id);
            if let Some(c) = f.client_id {
                qb.push(" and i.client_id = ").push_bind(c);
            }
            if let Some(s) = f.status {
                qb.push(" and i.status = ").push_bind(s);
            }
            if let Some(d) = f.overdue_as_of {
                qb.push(" and i.status in ('unpaid', 'payment_pending', 'collections') and i.due_date < ")
                    .push_bind(d);
            }
            if let Some(s) = &search {
                qb.push(" and (i.number ilike ")
                    .push_bind(s.clone())
                    .push(" or c.email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or (c.first_name || ' ' || c.last_name) ilike ")
                    .push_bind(s.clone())
                    .push(" or c.company_name ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        "i.issue_date desc, i.created_at desc, i.id desc",
        page,
    )
    .await
}

/// Open invoices whose due date is before `as_of` (oldest first) — overdue
/// notices, late fees, suspension.
pub async fn overdue_invoices(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    as_of: NaiveDate,
    limit: i64,
) -> DbResult<Vec<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select * from invoices
         where tenant_id = $1 and status in ('unpaid', 'payment_pending', 'collections') and due_date < $2
          and not exists (select 1 from mass_payments mp where mp.tenant_id = invoices.tenant_id and mp.invoice_id = invoices.id)
         order by due_date, id limit $3",
    )
    .bind(tenant_id)
    .bind(as_of)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Open invoices due within `[from, to]` (payment reminders).
pub async fn invoices_due_between(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: NaiveDate,
    to: NaiveDate,
) -> DbResult<Vec<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select * from invoices
         where tenant_id = $1 and status in ('unpaid', 'payment_pending') and due_date between $2 and $3
          and not exists (select 1 from mass_payments mp where mp.tenant_id = invoices.tenant_id and mp.invoice_id = invoices.id)
         order by due_date, id",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?)
}

/// The open invoice that bills `service_id` for the period starting at `period_start`, if any.
pub async fn open_invoice_for_service_period(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    period_start: NaiveDate,
) -> DbResult<Option<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select i.* from invoices i
         where i.tenant_id = $1 and i.status <> 'cancelled'
           and exists (select 1 from invoice_items ii
                       where ii.invoice_id = i.id and ii.service_id = $2 and ii.period_start = $3)
         order by i.created_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(service_id)
    .bind(period_start)
    .fetch_optional(db)
    .await?)
}

/// Moves an invoice from one of `from` to `to` (optimistic; every `from → to`
/// must be allowed by `InvoiceStatus::transition`, else `InvalidTransition`). Stamps `paid_at`, `cancelled_at`, `refunded_at`.
pub async fn transition_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
    from: &[InvoiceStatus],
    to: InvoiceStatus,
) -> DbResult<bool> {
    for f in from {
        f.transition(to)?;
    }
    let res = sqlx::query(
        "update invoices set status = $4,
                paid_at = case when $4 = 'paid' then coalesce(paid_at, now()) else paid_at end,
                cancelled_at = case when $4 = 'cancelled' then now() else cancelled_at end,
                refunded_at = case when $4 in ('refunded', 'partially_refunded') then now() else refunded_at end
         where tenant_id = $1 and id = $2 and status = any($3)",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(from)
    .bind(to)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Cancels a draft / open invoice that holds no money (nothing paid, no
/// credit applied); `false` otherwise. Staff cancels go through
/// [`super::payments::cancel_invoice_with_credit`], which gives applied
/// credit back and refuses while a payment is in flight.
pub async fn cancel_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<bool> {
    let from = [
        InvoiceStatus::Draft,
        InvoiceStatus::Unpaid,
        InvoiceStatus::PaymentPending,
        InvoiceStatus::Collections,
    ];
    for f in &from {
        f.transition(InvoiceStatus::Cancelled)?;
    }
    let res = sqlx::query(
        "update invoices set status = 'cancelled', cancelled_at = now()
         where tenant_id = $1 and id = $2 and status = any($3)
           and amount_paid = 0 and credit_applied = 0",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&from[..])
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Counts a reminder e-mail.
pub async fn record_reminder(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update invoices set reminder_count = reminder_count + 1, last_reminder_at = now()
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Updates client-visible / staff notes and the preferred gateway.
pub async fn update_invoice_meta(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
    notes: Option<&str>,
    admin_notes: Option<&str>,
    payment_integration_id: Option<IntegrationId>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update invoices set notes = $3, admin_notes = $4, payment_integration_id = $5 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(notes)
    .bind(admin_notes)
    .bind(payment_integration_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn line(amount: Decimal, taxable: bool) -> TotalsLine {
        TotalsLine {
            amount,
            taxable,
            is_discount: amount < Decimal::ZERO,
        }
    }

    #[test]
    fn kdv_20_percent() {
        let t = compute_totals(
            Currency::TRY,
            &[line(dec!(89.90), true), line(dec!(10.00), false)],
            &[TotalsTax {
                rate: dec!(20),
                is_compound: false,
            }],
        );
        assert_eq!(t.subtotal, dec!(99.90));
        assert_eq!(t.taxes, vec![(dec!(89.90), dec!(17.98))]);
        assert_eq!(t.tax_total, dec!(17.98));
        assert_eq!(t.total, dec!(117.88));
        assert_eq!(t.discount_total, dec!(0));
    }

    #[test]
    fn compound_tax_and_discount() {
        let t = compute_totals(
            Currency::USD,
            &[line(dec!(100), true), line(dec!(-10), true)],
            &[
                TotalsTax {
                    rate: dec!(10),
                    is_compound: false,
                },
                TotalsTax {
                    rate: dec!(5),
                    is_compound: true,
                },
            ],
        );
        assert_eq!(t.subtotal, dec!(90));
        assert_eq!(t.discount_total, dec!(10));
        // 10% of 90 = 9; 5% of (90 + 9) = 4.95.
        assert_eq!(
            t.taxes,
            vec![(dec!(90), dec!(9.00)), (dec!(99.00), dec!(4.95))]
        );
        assert_eq!(t.total, dec!(103.95));
    }

    #[test]
    fn rounding_half_away_from_zero() {
        let t = compute_totals(
            Currency::TRY,
            &[line(dec!(0.125), true)],
            &[TotalsTax {
                rate: dec!(20),
                is_compound: false,
            }],
        );
        // base rounds to 0.13, tax = 0.026 → 0.03.
        assert_eq!(t.taxes[0], (dec!(0.13), dec!(0.03)));
    }

    #[test]
    fn number_format() {
        assert_eq!(format_invoice_number("INV-", None, 42, 6), "INV-000042");
        assert_eq!(
            format_invoice_number("HOS", Some(2026), 7, 9),
            "HOS2026000000007"
        );
        assert_eq!(format_invoice_number("", None, 1234567, 3), "1234567");
    }
}
