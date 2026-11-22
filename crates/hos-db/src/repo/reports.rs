//! Financial reports (hos-billing `reports`): revenue by period / product /
//! gateway, churn, tax, receivables aging and best sellers.
//!
//! Amounts are per currency (no conversion); periods are half-open
//! `[from, to)` on `timestamptz` columns, or inclusive dates where noted.

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::ids::{ProductId, TenantId};
use hos_core::money::Currency;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::{FromRow, PgExecutor};

use crate::error::DbResult;

/// Bucket size of [`revenue_by_period`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// Calendar days.
    Day,
    /// Calendar weeks (ISO, starting Monday).
    Week,
    /// Calendar months.
    Month,
    /// Calendar years.
    Year,
}

impl Granularity {
    fn as_sql(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// Revenue of one bucket and currency: payments in, refunds and chargebacks out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct RevenueRow {
    /// First day of the bucket (UTC).
    pub period: NaiveDate,
    pub currency: Currency,
    pub payments: Decimal,
    pub refunds: Decimal,
    pub chargebacks: Decimal,
    pub fees: Decimal,
    /// `payments - refunds - chargebacks`.
    pub net: Decimal,
    pub payment_count: i64,
}

/// Succeeded money movements grouped by period and currency.
pub async fn revenue_by_period(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    granularity: Granularity,
) -> DbResult<Vec<RevenueRow>> {
    Ok(sqlx::query_as::<_, RevenueRow>(
        "select (date_trunc($4, occurred_at at time zone 'UTC'))::date as period, currency,
                coalesce(sum(amount) filter (where kind = 'payment'), 0) as payments,
                coalesce(sum(amount) filter (where kind = 'refund'), 0) as refunds,
                coalesce(sum(amount) filter (where kind = 'chargeback'), 0) as chargebacks,
                coalesce(sum(fee), 0) as fees,
                coalesce(sum(case kind when 'payment' then amount else -amount end), 0) as net,
                count(*) filter (where kind = 'payment') as payment_count
         from transactions
         where tenant_id = $1 and status = 'succeeded' and occurred_at >= $2 and occurred_at < $3
         group by 1, 2 order by 1, 2",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .bind(granularity.as_sql())
    .fetch_all(db)
    .await?)
}

/// Revenue of one product (or `None` for lines without a product: domains,
/// late fees, manual lines) — invoice lines of invoices paid in the period,
/// excluding tax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct ProductRevenueRow {
    pub product_id: Option<ProductId>,
    pub product_name: String,
    pub currency: Currency,
    pub amount: Decimal,
    pub line_count: i64,
}

/// Paid invoice lines grouped by the product of their service.
pub async fn revenue_by_product(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<Vec<ProductRevenueRow>> {
    Ok(sqlx::query_as::<_, ProductRevenueRow>(
        "select p.id as product_id,
                coalesce(p.name, case when ii.domain_id is not null or ii.item_type = 'domain' then 'Domains'
                                      else 'Other' end) as product_name,
                i.currency, sum(ii.amount) as amount, count(*) as line_count
         from invoice_items ii
         join invoices i on i.id = ii.invoice_id
         left join services s on s.id = ii.service_id
         left join products p on p.id = s.product_id
         where i.tenant_id = $1 and i.status in ('paid', 'partially_refunded')
           and i.paid_at >= $2 and i.paid_at < $3
         group by 1, 2, 3 order by 4 desc",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?)
}

/// Payments of one gateway kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct GatewayRevenueRow {
    pub gateway: String,
    pub currency: Currency,
    pub amount: Decimal,
    pub fees: Decimal,
    pub payment_count: i64,
}

/// Succeeded payments grouped by gateway.
pub async fn revenue_by_gateway(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<Vec<GatewayRevenueRow>> {
    Ok(sqlx::query_as::<_, GatewayRevenueRow>(
        "select gateway, currency, sum(amount) as amount, coalesce(sum(fee), 0) as fees, count(*) as payment_count
         from transactions
         where tenant_id = $1 and kind = 'payment' and status = 'succeeded'
           and occurred_at >= $2 and occurred_at < $3
         group by 1, 2 order by 3 desc",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?)
}

/// Service churn over a period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, FromRow)]
pub struct ChurnRow {
    /// Recurring services live (active / suspended) at the start of the period.
    pub active_at_start: i64,
    /// Recurring services registered during the period.
    pub new_services: i64,
    /// Recurring services terminated or cancelled during the period.
    pub churned: i64,
}

/// Counts for the churn rate (`churned / active_at_start`).
pub async fn churn(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<ChurnRow> {
    Ok(sqlx::query_as::<_, ChurnRow>(
        "select
            count(*) filter (where s.created_at < $2 and s.status <> 'pending'
                               and coalesce(s.terminated_at, s.cancelled_at, 'infinity') >= $2) as active_at_start,
            count(*) filter (where s.created_at >= $2 and s.created_at < $3) as new_services,
            count(*) filter (where coalesce(s.terminated_at, s.cancelled_at) >= $2
                               and coalesce(s.terminated_at, s.cancelled_at) < $3) as churned
         from services s
         where s.tenant_id = $1 and s.billing_cycle <> 'one_time'",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_one(db)
    .await?)
}

/// Collected tax per rate and currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct TaxReportRow {
    pub name: String,
    pub rate: Decimal,
    pub currency: Currency,
    pub taxable_amount: Decimal,
    pub tax_amount: Decimal,
    pub invoice_count: i64,
}

/// Tax lines of invoices issued in `[from, to]` (dates, inclusive) that are
/// paid (`paid_only`) or simply issued (accrual basis).
pub async fn tax_report(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: NaiveDate,
    to: NaiveDate,
    paid_only: bool,
) -> DbResult<Vec<TaxReportRow>> {
    Ok(sqlx::query_as::<_, TaxReportRow>(
        "select t.name, t.rate, i.currency, sum(t.taxable_amount) as taxable_amount,
                sum(t.tax_amount) as tax_amount, count(distinct i.id) as invoice_count
         from invoice_tax_lines t join invoices i on i.id = t.invoice_id
         where i.tenant_id = $1 and i.issue_date between $2 and $3
           and i.status not in ('draft', 'cancelled')
           and (not $4 or i.status in ('paid', 'partially_refunded'))
         group by 1, 2, 3 order by 1, 2, 3",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .bind(paid_only)
    .fetch_all(db)
    .await?)
}

/// Open receivables by age (days past due) and currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct AgingRow {
    pub currency: Currency,
    /// Not yet due.
    pub current: Decimal,
    pub days_1_30: Decimal,
    pub days_31_60: Decimal,
    pub days_61_90: Decimal,
    pub days_over_90: Decimal,
    pub total: Decimal,
    pub invoice_count: i64,
}

/// Aging of open invoice balances as of `as_of`.
pub async fn aging(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    as_of: NaiveDate,
) -> DbResult<Vec<AgingRow>> {
    Ok(sqlx::query_as::<_, AgingRow>(
        "select currency,
                coalesce(sum(balance) filter (where $2 - due_date <= 0), 0) as current,
                coalesce(sum(balance) filter (where $2 - due_date between 1 and 30), 0) as days_1_30,
                coalesce(sum(balance) filter (where $2 - due_date between 31 and 60), 0) as days_31_60,
                coalesce(sum(balance) filter (where $2 - due_date between 61 and 90), 0) as days_61_90,
                coalesce(sum(balance) filter (where $2 - due_date > 90), 0) as days_over_90,
                coalesce(sum(balance), 0) as total,
                count(*) as invoice_count
         from invoices
         where tenant_id = $1 and status in ('unpaid', 'payment_pending', 'collections') and balance > 0
         group by currency order by currency",
    )
    .bind(tenant_id)
    .bind(as_of)
    .fetch_all(db)
    .await?)
}

/// A best-selling product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct TopProductRow {
    pub product_id: ProductId,
    pub product_name: String,
    pub currency: Currency,
    pub order_count: i64,
    pub quantity: i64,
    pub revenue: Decimal,
}

/// Products of accepted orders created in the period, by quantity sold.
pub async fn top_products(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: i64,
) -> DbResult<Vec<TopProductRow>> {
    Ok(sqlx::query_as::<_, TopProductRow>(
        "select p.id as product_id, p.name as product_name, o.currency,
                count(distinct o.id) as order_count, sum(oi.quantity)::bigint as quantity,
                sum(oi.amount) as revenue
         from order_items oi
         join orders o on o.id = oi.order_id
         join products p on p.id = oi.product_id
         where o.tenant_id = $1 and o.status = 'accepted' and oi.item_type = 'product'
           and o.created_at >= $2 and o.created_at < $3
         group by 1, 2, 3 order by 5 desc, 6 desc limit $4",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(db)
    .await?)
}
