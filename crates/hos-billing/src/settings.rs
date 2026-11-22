//! Tenant billing settings (`tenant_settings`), with the defaults seeded by
//! `hos_db::seed` plus the keys the billing engine adds.
//!
//! | Key | Default | Meaning |
//! |---|---|---|
//! | `billing.invoice_days_before_due` | 7 | renewal invoices are issued N days before the due date |
//! | `billing.payment_terms_days` | 7 | due date of manually created invoices (today + N) |
//! | `billing.order_invoice_due_days` | 0 | due date of order invoices (today + N) |
//! | `billing.reminder_days_before_due` | [3, 1] | payment reminders before the due date |
//! | `billing.overdue_notice_days` | [1, 3, 7] | 1st / 2nd / 3rd overdue notices (days past due) |
//! | `billing.suspend_after_days` | 5 | suspend services N days after the due date (0 = never) |
//! | `billing.terminate_after_days` | 30 | terminate N days after the due date (0 = never) |
//! | `billing.late_fee` | `{"type":"percent","amount":0,"min_amount":0,"after_days":0}` | optional late fee |
//! | `billing.apply_credit_automatically` | true | use client credit on new invoices |
//! | `billing.prices_include_tax` | false | catalog prices are gross (tax-inclusive) |
//! | `billing.tax_home_country` | `"TR"` | the tenant's own country (B2B exemption) |
//! | `billing.b2b_tax_exempt` | false | foreign companies with a tax number are not taxed (reverse charge) |
//! | `billing.tax_domains` | true | domain lines are taxable |
//! | `billing.suspend_on_chargeback` | true | suspend the services of a charged-back invoice |
//! | `billing.auto_charge_stored` | true | charge stored payment methods on the due date |
//! | `orders.auto_accept` | true | accept paid orders automatically (else staff accepts) |
//! | `domains.expiry_notice_days` | [30, 14, 7, 1] | domain expiry notices |
//! | `domains.default_nameservers` | [] | nameservers for new registrations |
//! | `domains.price_margin` | `{"percent": 20, "fixed": 0}` | TLD price sync margin over registrar cost |

use std::collections::BTreeMap;

use hos_core::ids::TenantId;
use hos_db::repo::platform;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::PgExecutor;

use crate::BillingResult;

/// How a late fee is computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LateFee {
    /// `true`: `amount` percent of the invoice total; `false`: fixed amount.
    pub percent: bool,
    /// Percent or fixed amount (0 disables).
    pub amount: Decimal,
    /// Lower bound of a percent fee.
    pub min_amount: Decimal,
    /// Days past due before the fee is added.
    pub after_days: i32,
}

impl LateFee {
    /// Whether a fee is configured.
    pub fn enabled(&self) -> bool {
        self.amount > Decimal::ZERO
    }
}

/// TLD price margin over the registrar's cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceMargin {
    /// Percent added on top of the cost.
    pub percent: Decimal,
    /// Fixed amount added after the percent.
    pub fixed: Decimal,
}

/// Billing behaviour of a tenant.
#[derive(Debug, Clone, PartialEq)]
pub struct BillingSettings {
    pub invoice_days_before_due: i32,
    pub payment_terms_days: i32,
    pub order_invoice_due_days: i32,
    pub reminder_days_before_due: Vec<i32>,
    pub overdue_notice_days: Vec<i32>,
    pub suspend_after_days: i32,
    pub terminate_after_days: i32,
    pub late_fee: LateFee,
    pub apply_credit_automatically: bool,
    pub prices_include_tax: bool,
    pub tax_home_country: String,
    pub b2b_tax_exempt: bool,
    pub tax_domains: bool,
    pub suspend_on_chargeback: bool,
    pub auto_charge_stored: bool,
    pub auto_accept_orders: bool,
    pub domain_expiry_notice_days: Vec<i32>,
    pub default_nameservers: Vec<String>,
    pub price_margin: PriceMargin,
}

impl Default for BillingSettings {
    fn default() -> Self {
        Self::from_map(&BTreeMap::new())
    }
}

fn int(map: &BTreeMap<String, Value>, key: &str, default: i32) -> i32 {
    map.get(key)
        .and_then(|v| match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(default)
}

fn boolean(map: &BTreeMap<String, Value>, key: &str, default: bool) -> bool {
    match map.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim(), "true" | "1" | "on" | "yes"),
        Some(Value::Number(n)) => n.as_i64().is_some_and(|n| n != 0),
        _ => default,
    }
}

fn int_list(map: &BTreeMap<String, Value>, key: &str, default: &[i32]) -> Vec<i32> {
    let mut out: Vec<i32> = match map.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_i64().and_then(|n| i32::try_from(n).ok()))
            .filter(|n| *n >= 0)
            .collect(),
        _ => default.to_vec(),
    };
    out.sort_unstable();
    out.dedup();
    out
}

fn decimal(v: Option<&Value>) -> Option<Decimal> {
    match v? {
        Value::Number(n) => n.to_string().parse().ok(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl BillingSettings {
    /// Builds settings from `tenant_settings` key/values (missing → defaults).
    pub fn from_map(map: &BTreeMap<String, Value>) -> Self {
        let late = map.get("billing.late_fee");
        let late_fee = LateFee {
            percent: late
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .is_none_or(|t| t != "fixed"),
            amount: decimal(late.and_then(|v| v.get("amount"))).unwrap_or(Decimal::ZERO),
            min_amount: decimal(late.and_then(|v| v.get("min_amount"))).unwrap_or(Decimal::ZERO),
            after_days: late
                .and_then(|v| v.get("after_days"))
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0),
        };
        let margin = map.get("domains.price_margin");
        Self {
            invoice_days_before_due: int(map, "billing.invoice_days_before_due", 7).max(0),
            payment_terms_days: int(map, "billing.payment_terms_days", 7).max(0),
            order_invoice_due_days: int(map, "billing.order_invoice_due_days", 0).max(0),
            reminder_days_before_due: int_list(map, "billing.reminder_days_before_due", &[3, 1]),
            overdue_notice_days: int_list(map, "billing.overdue_notice_days", &[1, 3, 7]),
            suspend_after_days: int(map, "billing.suspend_after_days", 5).max(0),
            terminate_after_days: int(map, "billing.terminate_after_days", 30).max(0),
            late_fee,
            apply_credit_automatically: boolean(map, "billing.apply_credit_automatically", true),
            prices_include_tax: boolean(map, "billing.prices_include_tax", false),
            tax_home_country: map
                .get("billing.tax_home_country")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_ascii_uppercase())
                .filter(|s| s.len() == 2)
                .unwrap_or_else(|| "TR".to_owned()),
            b2b_tax_exempt: boolean(map, "billing.b2b_tax_exempt", false),
            tax_domains: boolean(map, "billing.tax_domains", true),
            suspend_on_chargeback: boolean(map, "billing.suspend_on_chargeback", true),
            auto_charge_stored: boolean(map, "billing.auto_charge_stored", true),
            auto_accept_orders: boolean(map, "orders.auto_accept", true),
            domain_expiry_notice_days: int_list(map, "domains.expiry_notice_days", &[30, 14, 7, 1]),
            default_nameservers: map
                .get("domains.default_nameservers")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            price_margin: PriceMargin {
                percent: decimal(margin.and_then(|m| m.get("percent")))
                    .unwrap_or(Decimal::from(20)),
                fixed: decimal(margin.and_then(|m| m.get("fixed"))).unwrap_or(Decimal::ZERO),
            },
        }
    }

    /// Loads the settings of a tenant.
    pub async fn load(db: impl PgExecutor<'_>, tenant_id: TenantId) -> BillingResult<Self> {
        let map = platform::get_settings(db, tenant_id, "").await?;
        Ok(Self::from_map(&map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_overrides() {
        let d = BillingSettings::default();
        assert_eq!(d.invoice_days_before_due, 7);
        assert_eq!(d.overdue_notice_days, vec![1, 3, 7]);
        assert!(!d.late_fee.enabled());
        assert!(d.auto_accept_orders);
        let mut m = BTreeMap::new();
        m.insert("billing.suspend_after_days".into(), json!("3"));
        m.insert(
            "billing.late_fee".into(),
            json!({"type": "fixed", "amount": "25.5", "after_days": 2}),
        );
        m.insert("domains.expiry_notice_days".into(), json!([7, 30, 7]));
        m.insert("orders.auto_accept".into(), json!(false));
        let s = BillingSettings::from_map(&m);
        assert_eq!(s.suspend_after_days, 3);
        assert!(!s.late_fee.percent);
        assert_eq!(s.late_fee.amount, Decimal::new(255, 1));
        assert_eq!(s.late_fee.after_days, 2);
        assert_eq!(s.domain_expiry_notice_days, vec![7, 30]);
        assert!(!s.auto_accept_orders);
    }
}
