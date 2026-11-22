//! Limits of client credit top-ups ("add funds"): per top-up minimum and
//! maximum and a cap on the resulting credit balance (`billing.add_funds_min`,
//! `billing.add_funds_max`, `billing.credit_balance_max`; `0` = no cap).
//! The absolute bounds 1 – 1,000,000 always apply.

use std::collections::BTreeMap;

use hos_core::CoreError;
use hos_core::ids::TenantId;
use hos_db::repo::platform;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::PgExecutor;

use crate::BillingResult;

/// Top-up limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundsLimits {
    pub min: Decimal,
    pub max: Decimal,
    /// Largest credit balance a top-up may lead to; zero = no cap.
    pub balance_cap: Decimal,
}

impl Default for FundsLimits {
    fn default() -> Self {
        Self {
            min: Decimal::ONE,
            max: Decimal::from(1_000_000),
            balance_cap: Decimal::ZERO,
        }
    }
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

impl FundsLimits {
    /// From stored settings, clamped to the absolute bounds.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Self {
        let d = Self::default();
        let get = |k: &str| map.get(k).and_then(dec).filter(|v| *v >= Decimal::ZERO);
        let min = get("billing.add_funds_min")
            .unwrap_or(d.min)
            .clamp(d.min, d.max);
        let max = get("billing.add_funds_max")
            .unwrap_or(d.max)
            .clamp(min, d.max);
        Self {
            min,
            max,
            balance_cap: get("billing.credit_balance_max").unwrap_or(Decimal::ZERO),
        }
    }

    /// Loads the tenant's limits.
    pub async fn load(db: impl PgExecutor<'_>, tenant_id: TenantId) -> BillingResult<Self> {
        Ok(Self::from_map(
            &platform::get_settings(db, tenant_id, "billing.").await?,
        ))
    }

    /// Checks a top-up of `amount` on a current credit balance of `balance`.
    pub fn check(&self, amount: Decimal, balance: Decimal) -> Result<(), CoreError> {
        if amount < self.min || amount > self.max {
            return Err(CoreError::validation(format!(
                "the amount must be between {} and {}",
                self.min.normalize(),
                self.max.normalize()
            )));
        }
        if self.balance_cap > Decimal::ZERO && balance + amount > self.balance_cap {
            return Err(CoreError::validation(format!(
                "the credit balance may not exceed {}",
                self.balance_cap.normalize()
            )));
        }
        Ok(())
    }

    /// Part of `amount` of credit arriving outside a top-up (what a mass
    /// payment's combined invoice no longer needed) that a top-up of the
    /// same amount would not have been allowed: over the per-top-up maximum
    /// or over the balance cap, on a balance of `balance` before it. The
    /// minimum does not apply (a small leftover is fine).
    pub fn excess(&self, amount: Decimal, balance: Decimal) -> Decimal {
        let amount = amount.max(Decimal::ZERO);
        let over_max = amount - self.max;
        let over_cap = if self.balance_cap > Decimal::ZERO {
            balance.max(Decimal::ZERO) + amount - self.balance_cap
        } else {
            Decimal::ZERO
        };
        over_max.max(over_cap).clamp(Decimal::ZERO, amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits() {
        let mut m = BTreeMap::new();
        assert_eq!(FundsLimits::from_map(&m), FundsLimits::default());
        m.insert("billing.add_funds_min".into(), json!("50"));
        m.insert("billing.add_funds_max".into(), json!("20"));
        m.insert("billing.credit_balance_max".into(), json!("500"));
        let l = FundsLimits::from_map(&m);
        assert_eq!(l.min, Decimal::from(50));
        assert_eq!(l.max, Decimal::from(50), "max never below min");
        assert!(l.check(Decimal::from(50), Decimal::from(400)).is_ok());
        assert!(
            l.check(Decimal::from(50), Decimal::from(451)).is_err(),
            "cap"
        );
        assert!(l.check(Decimal::from(49), Decimal::ZERO).is_err());
        m.insert("billing.add_funds_min".into(), json!("-5"));
        assert_eq!(FundsLimits::from_map(&m).min, Decimal::ONE);
    }

    #[test]
    fn excess_of_leftover_credit() {
        let mut m = BTreeMap::new();
        m.insert("billing.add_funds_max".into(), json!("200"));
        m.insert("billing.credit_balance_max".into(), json!("500"));
        let l = FundsLimits::from_map(&m);
        let d = |v: i64| Decimal::from(v);
        assert_eq!(l.excess(d(5), d(0)), d(0), "no minimum for leftovers");
        assert_eq!(l.excess(d(150), d(300)), d(0));
        assert_eq!(l.excess(d(150), d(400)), d(50), "over the cap");
        assert_eq!(l.excess(d(260), d(0)), d(60), "over the top-up maximum");
        assert_eq!(
            l.excess(d(100), d(-50)),
            d(0),
            "a negative balance counts as zero"
        );
        assert_eq!(FundsLimits::default().excess(d(900_000), d(0)), d(0));
    }
}
