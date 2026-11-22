//! Financial reports for the admin area: revenue (per period / product /
//! gateway), MRR / ARR / churn, tax, receivables aging and top sellers.
//! Amounts are never converted between currencies: every row carries its
//! currency.

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::ids::TenantId;
use hos_core::money::Currency;
use hos_db::repo::reports::{
    self, AgingRow, ChurnRow, GatewayRevenueRow, Granularity, ProductRevenueRow, RevenueRow,
    TaxReportRow, TopProductRow,
};
use hos_db::repo::stats;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::{Billing, BillingResult};

/// Recurring revenue in one currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecurringRevenue {
    pub currency: Currency,
    /// Monthly recurring revenue of active services and addons.
    pub mrr: Decimal,
    /// `12 × MRR`.
    pub arr: Decimal,
}

/// Subscription metrics of a period.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubscriptionMetrics {
    pub recurring: Vec<RecurringRevenue>,
    pub churn: ChurnRow,
    /// `churned / active_at_start` (0 when nothing was active), in percent.
    pub churn_rate_percent: Decimal,
}

/// Churn rate in percent, rounded to two decimals.
pub fn churn_rate(c: &ChurnRow) -> Decimal {
    if c.active_at_start <= 0 {
        return Decimal::ZERO;
    }
    (Decimal::from(c.churned) * Decimal::ONE_HUNDRED / Decimal::from(c.active_at_start)).round_dp(2)
}

impl Billing {
    /// Payments, refunds, chargebacks and fees per day / week / month / year.
    pub async fn report_revenue(
        &self,
        tenant_id: TenantId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        granularity: Granularity,
    ) -> BillingResult<Vec<RevenueRow>> {
        reports::revenue_by_period(self.pool(), tenant_id, from, to, granularity).await
    }

    /// Paid invoice lines per product.
    pub async fn report_revenue_by_product(
        &self,
        tenant_id: TenantId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> BillingResult<Vec<ProductRevenueRow>> {
        reports::revenue_by_product(self.pool(), tenant_id, from, to).await
    }

    /// Payments per gateway.
    pub async fn report_revenue_by_gateway(
        &self,
        tenant_id: TenantId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> BillingResult<Vec<GatewayRevenueRow>> {
        reports::revenue_by_gateway(self.pool(), tenant_id, from, to).await
    }

    /// MRR / ARR per currency now, and churn over `[from, to)`.
    pub async fn report_subscriptions(
        &self,
        tenant_id: TenantId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> BillingResult<SubscriptionMetrics> {
        let recurring = stats::mrr_by_currency(self.pool(), tenant_id)
            .await?
            .into_iter()
            .map(|m| RecurringRevenue {
                currency: m.currency,
                mrr: m.amount.round_dp(2),
                arr: (m.amount * Decimal::from(12)).round_dp(2),
            })
            .collect();
        let churn = reports::churn(self.pool(), tenant_id, from, to).await?;
        Ok(SubscriptionMetrics {
            churn_rate_percent: churn_rate(&churn),
            recurring,
            churn,
        })
    }

    /// Tax collected per rate (`paid_only`: cash basis; else accrual).
    pub async fn report_taxes(
        &self,
        tenant_id: TenantId,
        from: NaiveDate,
        to: NaiveDate,
        paid_only: bool,
    ) -> BillingResult<Vec<TaxReportRow>> {
        reports::tax_report(self.pool(), tenant_id, from, to, paid_only).await
    }

    /// Open balances by age (default: the clock's today).
    pub async fn report_aging(
        &self,
        tenant_id: TenantId,
        as_of: Option<NaiveDate>,
    ) -> BillingResult<Vec<AgingRow>> {
        let as_of = as_of.unwrap_or_else(|| self.clock().today());
        reports::aging(self.pool(), tenant_id, as_of).await
    }

    /// Best-selling products.
    pub async fn report_top_products(
        &self,
        tenant_id: TenantId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i64,
    ) -> BillingResult<Vec<TopProductRow>> {
        reports::top_products(self.pool(), tenant_id, from, to, limit.clamp(1, 100)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn churn_rate_percent() {
        let c = ChurnRow {
            active_at_start: 40,
            new_services: 5,
            churned: 3,
        };
        assert_eq!(churn_rate(&c), Decimal::new(750, 2));
        let empty = ChurnRow {
            active_at_start: 0,
            new_services: 2,
            churned: 0,
        };
        assert_eq!(churn_rate(&empty), Decimal::ZERO);
    }
}
