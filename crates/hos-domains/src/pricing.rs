//! Selling prices from registrar cost prices — pure functions over
//! [`Money`]/[`Decimal`] (never `f64`).
//!
//! Pipeline applied by [`PricingRule::apply`]:
//!
//! 1. **convert** the cost into the selling currency with the given rate
//!    (`1 cost.currency = rate × target`), optionally padded by
//!    [`PricingRule::fx_buffer_percent`] against exchange-rate drift;
//! 2. **margin**: percentage and/or fixed amount (in the selling currency);
//! 3. **floors**: [`PricingRule::min_profit`] and [`PricingRule::min_price`];
//! 4. **rounding**: [`Rounding`] (e.g. psychological `.99`), never below the
//!    floors;
//! 5. final rounding to the currency's minor unit.
//!
//! [`PricingPolicy`] adds per-TLD overrides and prices a whole
//! [`TldPrice`] (register / renew / transfer / restore).

use std::collections::BTreeMap;

use hos_core::integrations::registrar::TldPrice;
use hos_core::money::{Currency, Money, MoneyError};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

/// Errors from price calculation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PricingError {
    /// Cost and target currency differ and no exchange rate was given.
    #[error("no exchange rate from {from} to {to}")]
    MissingRate {
        /// Cost currency.
        from: Currency,
        /// Selling currency.
        to: Currency,
    },
    /// Exchange rate is zero or negative.
    #[error("exchange rate must be positive")]
    InvalidRate,
    /// A rule value is invalid (negative margin, rounding step ≤ 0, ...).
    #[error("invalid pricing rule: {0}")]
    InvalidRule(String),
    /// Arithmetic / currency error.
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// Margin added on top of the (converted) cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Margin {
    /// No margin (sell at cost).
    None,
    /// `cost × (1 + percent / 100)`.
    Percent {
        /// Percentage, e.g. `25` for 25 %.
        percent: Decimal,
    },
    /// `cost + amount` (amount in the selling currency).
    Fixed {
        /// Amount added.
        amount: Decimal,
    },
    /// `cost × (1 + percent / 100) + amount`.
    PercentPlusFixed {
        /// Percentage.
        percent: Decimal,
        /// Amount in the selling currency.
        amount: Decimal,
    },
}

/// Rounding of the final selling price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Rounding {
    /// Only round to the currency's minor unit (half away from zero).
    MinorUnit,
    /// Round **up** to the next multiple of `step` (`step = 0.5` → 12.30 → 12.50).
    UpToStep {
        /// Step, > 0.
        step: Decimal,
    },
    /// Psychological price: the smallest value `≥ price` that ends in
    /// `ending` (`ending = 0.99`: 12.30 → 12.99, 12.99 → 12.99, 13.00 → 13.99).
    /// For zero-decimal currencies (JPY) the ending applies to the last digit
    /// of the integer part: `ending = 0.99` behaves like "ends in 9" (1234 → 1239).
    Ending {
        /// Fractional ending in `[0, 1)`, e.g. `0.99`, `0.90`, `0.49`.
        ending: Decimal,
    },
}

impl Rounding {
    /// `.99` endings.
    pub fn psychological_99() -> Self {
        Rounding::Ending {
            ending: Decimal::new(99, 2),
        }
    }
}

/// How to derive a selling price from a cost price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingRule {
    /// Margin.
    pub margin: Margin,
    /// Rounding.
    pub rounding: Rounding,
    /// Extra percentage applied to the exchange rate (safety buffer), e.g. `2`.
    #[serde(default)]
    pub fx_buffer_percent: Decimal,
    /// Minimum profit over the converted cost (selling currency amount).
    #[serde(default)]
    pub min_profit: Option<Decimal>,
    /// Absolute minimum selling price (selling currency amount).
    #[serde(default)]
    pub min_price: Option<Decimal>,
}

impl Default for PricingRule {
    fn default() -> Self {
        Self {
            margin: Margin::None,
            rounding: Rounding::MinorUnit,
            fx_buffer_percent: Decimal::ZERO,
            min_profit: None,
            min_price: None,
        }
    }
}

impl PricingRule {
    /// Percentage margin with `.99` rounding.
    pub fn percent_99(percent: Decimal) -> Self {
        Self {
            margin: Margin::Percent { percent },
            rounding: Rounding::psychological_99(),
            ..Self::default()
        }
    }

    /// Computes the selling price for `cost` in `target`.
    ///
    /// `rate` is `1 cost.currency = rate target`; it may be `None` only when
    /// the currencies are equal.
    pub fn apply(
        &self,
        cost: &Money,
        target: Currency,
        rate: Option<Decimal>,
    ) -> Result<Money, PricingError> {
        self.validate()?;
        let converted = convert(cost, target, rate, self.fx_buffer_percent)?;
        let mut price = apply_margin(&converted, &self.margin)?;
        let mut floor = Decimal::MIN;
        if let Some(p) = self.min_profit {
            floor = floor.max(
                converted
                    .amount
                    .checked_add(p)
                    .ok_or(MoneyError::Overflow)?,
            );
        }
        if let Some(m) = self.min_price {
            floor = floor.max(m);
        }
        if price.amount < floor {
            price.amount = floor;
        }
        let rounded = round_price(&price, &self.rounding)?;
        // Rounding never goes below the floor (charm endings move up anyway,
        // but MinorUnit could shave a fraction).
        let mut out = rounded.round();
        if out.amount < floor {
            out = Money::new(floor, target)
                .round_with(target.minor_units(), RoundingStrategy::AwayFromZero);
        }
        Ok(out)
    }

    fn validate(&self) -> Result<(), PricingError> {
        let neg = |d: Decimal| d.is_sign_negative() && !d.is_zero();
        match &self.margin {
            Margin::Percent { percent } if *percent <= Decimal::from(-100) => {
                return Err(PricingError::InvalidRule("percent must be > -100".into()));
            }
            Margin::PercentPlusFixed { percent, .. } if *percent <= Decimal::from(-100) => {
                return Err(PricingError::InvalidRule("percent must be > -100".into()));
            }
            _ => {}
        }
        match &self.rounding {
            Rounding::UpToStep { step } if *step <= Decimal::ZERO => {
                return Err(PricingError::InvalidRule(
                    "rounding step must be > 0".into(),
                ));
            }
            Rounding::Ending { ending } if neg(*ending) || *ending >= Decimal::ONE => {
                return Err(PricingError::InvalidRule("ending must be in [0, 1)".into()));
            }
            _ => {}
        }
        if neg(self.fx_buffer_percent) {
            return Err(PricingError::InvalidRule("fx buffer must be ≥ 0".into()));
        }
        Ok(())
    }
}

/// Converts `cost` into `target`. With equal currencies the rate is ignored
/// (and the FX buffer not applied).
pub fn convert(
    cost: &Money,
    target: Currency,
    rate: Option<Decimal>,
    fx_buffer_percent: Decimal,
) -> Result<Money, PricingError> {
    if cost.currency == target {
        return Ok(*cost);
    }
    let rate = rate.ok_or(PricingError::MissingRate {
        from: cost.currency,
        to: target,
    })?;
    if rate <= Decimal::ZERO {
        return Err(PricingError::InvalidRate);
    }
    let factor = Decimal::ONE
        .checked_add(
            fx_buffer_percent
                .checked_div(Decimal::ONE_HUNDRED)
                .ok_or(MoneyError::Overflow)?,
        )
        .ok_or(MoneyError::Overflow)?;
    let effective = rate.checked_mul(factor).ok_or(MoneyError::Overflow)?;
    Ok(cost.convert(effective, target)?)
}

/// Adds the margin (no rounding).
pub fn apply_margin(cost: &Money, margin: &Margin) -> Result<Money, PricingError> {
    let with_percent = |p: Decimal| -> Result<Money, PricingError> {
        let extra = cost.percent(p)?;
        Ok(cost.try_add(&extra)?)
    };
    Ok(match margin {
        Margin::None => *cost,
        Margin::Percent { percent } => with_percent(*percent)?,
        Margin::Fixed { amount } => cost.try_add(&Money::new(*amount, cost.currency))?,
        Margin::PercentPlusFixed { percent, amount } => {
            with_percent(*percent)?.try_add(&Money::new(*amount, cost.currency))?
        }
    })
}

/// Applies a [`Rounding`] (result may still need [`Money::round`]).
pub fn round_price(price: &Money, rounding: &Rounding) -> Result<Money, PricingError> {
    let amount = price.amount;
    let currency = price.currency;
    if amount <= Decimal::ZERO {
        return Ok(price.round());
    }
    let out = match rounding {
        Rounding::MinorUnit => return Ok(price.round()),
        Rounding::UpToStep { step } => {
            let steps = amount
                .checked_div(*step)
                .ok_or(MoneyError::Overflow)?
                .ceil();
            steps.checked_mul(*step).ok_or(MoneyError::Overflow)?
        }
        Rounding::Ending { ending } => {
            if currency.minor_units() == 0 {
                // "Ends in 9" style: 1234 → 1239 (ending .99 → digit 9).
                let digit = (ending * Decimal::TEN).trunc();
                let base = (amount / Decimal::TEN).floor() * Decimal::TEN + digit;
                if base >= amount.ceil() {
                    base
                } else {
                    base + Decimal::TEN
                }
            } else {
                // Smallest whole.ending ≥ amount (compare at the minor unit).
                let amount = amount
                    .round_dp_with_strategy(currency.minor_units(), RoundingStrategy::AwayFromZero);
                let candidate = amount.floor() + *ending;
                if candidate >= amount {
                    candidate
                } else {
                    candidate + Decimal::ONE
                }
            }
        }
    };
    Ok(Money::new(out, currency))
}

/// Selling prices for one TLD (per year; transfer usually includes one year).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TldSellPrice {
    /// TLD.
    pub tld: String,
    /// Registration, per year.
    pub register: Option<Money>,
    /// Renewal, per year.
    pub renew: Option<Money>,
    /// Transfer.
    pub transfer: Option<Money>,
    /// Redemption restore.
    pub restore: Option<Money>,
    /// Minimum years.
    pub min_years: u32,
    /// Maximum years.
    pub max_years: u32,
}

/// A default rule plus per-TLD overrides (keys are TLDs without dot).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingPolicy {
    /// Rule for TLDs without an override.
    pub default: PricingRule,
    /// TLD-specific rules.
    #[serde(default)]
    pub overrides: BTreeMap<String, PricingRule>,
}

impl PricingPolicy {
    /// Rule for `tld` (override or default).
    pub fn rule_for(&self, tld: &str) -> &PricingRule {
        self.overrides
            .get(tld.trim_start_matches('.'))
            .unwrap_or(&self.default)
    }

    /// Prices every operation of a registrar [`TldPrice`].
    pub fn price_tld(
        &self,
        cost: &TldPrice,
        target: Currency,
        rate: Option<Decimal>,
    ) -> Result<TldSellPrice, PricingError> {
        let rule = self.rule_for(&cost.tld);
        let one = |m: &Option<Money>| m.as_ref().map(|m| rule.apply(m, target, rate)).transpose();
        Ok(TldSellPrice {
            tld: cost.tld.clone(),
            register: one(&cost.register)?,
            renew: one(&cost.renew)?,
            transfer: one(&cost.transfer)?,
            restore: one(&cost.restore)?,
            min_years: cost.min_years,
            max_years: cost.max_years,
        })
    }
}

/// Price for `years` years at `per_year` (rounded to the minor unit).
pub fn multi_year(per_year: &Money, years: u32) -> Result<Money, PricingError> {
    Ok(per_year.try_mul(Decimal::from(years))?.round())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn usd(a: Decimal) -> Money {
        Money::new(a, Currency::USD)
    }

    #[test]
    fn psychological_rounding() {
        let r = Rounding::psychological_99();
        let cases = [
            (dec!(12.30), dec!(12.99)),
            (dec!(12.99), dec!(12.99)),
            (dec!(13.00), dec!(13.99)),
            (dec!(12.991), dec!(13.99)),
            (dec!(0.10), dec!(0.99)),
        ];
        for (input, expected) in cases {
            assert_eq!(
                round_price(&usd(input), &r).unwrap().amount,
                expected,
                "{input}"
            );
        }
        let r90 = Rounding::Ending { ending: dec!(0.90) };
        assert_eq!(
            round_price(&usd(dec!(10.95)), &r90).unwrap().amount,
            dec!(11.90)
        );
        let jpy = Currency::new("JPY").unwrap();
        assert_eq!(
            round_price(&Money::new(dec!(1234), jpy), &r)
                .unwrap()
                .amount,
            dec!(1239)
        );
        assert_eq!(
            round_price(&Money::new(dec!(1240), jpy), &r)
                .unwrap()
                .amount,
            dec!(1249)
        );
        let step = Rounding::UpToStep { step: dec!(0.5) };
        assert_eq!(
            round_price(&usd(dec!(12.30)), &step).unwrap().amount,
            dec!(12.5)
        );
    }

    #[test]
    fn margins_and_conversion() {
        let cost = usd(dec!(8.50));
        let rule = PricingRule {
            margin: Margin::PercentPlusFixed {
                percent: dec!(20),
                amount: dec!(1),
            },
            rounding: Rounding::MinorUnit,
            ..PricingRule::default()
        };
        // 8.50 * 1.2 + 1 = 11.20
        assert_eq!(
            rule.apply(&cost, Currency::USD, None).unwrap().amount,
            dec!(11.20)
        );
        // USD → TRY at 34.5: 8.50 * 34.5 = 293.25; * 1.2 + 1 = 352.90
        let try_price = rule.apply(&cost, Currency::TRY, Some(dec!(34.5))).unwrap();
        assert_eq!(try_price, Money::new(dec!(352.90), Currency::TRY));
        assert_eq!(
            rule.apply(&cost, Currency::TRY, None),
            Err(PricingError::MissingRate {
                from: Currency::USD,
                to: Currency::TRY
            })
        );
        assert_eq!(
            rule.apply(&cost, Currency::TRY, Some(dec!(0))),
            Err(PricingError::InvalidRate)
        );
        // FX buffer 2 %: 8.50 * 34.5 * 1.02 = 299.115 → .99 → 299.99
        let buffered = PricingRule {
            fx_buffer_percent: dec!(2),
            rounding: Rounding::psychological_99(),
            ..PricingRule::default()
        };
        assert_eq!(
            buffered
                .apply(&cost, Currency::TRY, Some(dec!(34.5)))
                .unwrap()
                .amount,
            dec!(299.99)
        );
    }

    #[test]
    fn floors_and_validation() {
        let rule = PricingRule {
            margin: Margin::Percent { percent: dec!(5) },
            min_profit: Some(dec!(2)),
            min_price: Some(dec!(9.99)),
            ..PricingRule::default()
        };
        // 5 % of 3.00 = 0.15 < min profit 2 → 5.00 < min price 9.99 → 9.99
        assert_eq!(
            rule.apply(&usd(dec!(3)), Currency::USD, None)
                .unwrap()
                .amount,
            dec!(9.99)
        );
        // 20.00 + 5 % = 21.00, profit 1.00 < 2 → 22.00
        assert_eq!(
            rule.apply(&usd(dec!(20)), Currency::USD, None)
                .unwrap()
                .amount,
            dec!(22.00)
        );
        let bad = PricingRule {
            rounding: Rounding::Ending { ending: dec!(1.5) },
            ..PricingRule::default()
        };
        assert!(matches!(
            bad.apply(&usd(dec!(1)), Currency::USD, None),
            Err(PricingError::InvalidRule(_))
        ));
    }

    #[test]
    fn policy_prices_tld() {
        let mut policy = PricingPolicy {
            default: PricingRule::percent_99(dec!(30)),
            overrides: BTreeMap::new(),
        };
        policy.overrides.insert(
            "com.tr".into(),
            PricingRule {
                margin: Margin::Fixed { amount: dec!(5) },
                ..PricingRule::default()
            },
        );
        let cost = TldPrice {
            tld: "com".into(),
            register: Some(usd(dec!(9.20))),
            renew: Some(usd(dec!(9.20))),
            transfer: None,
            restore: Some(usd(dec!(80))),
            min_years: 1,
            max_years: 10,
        };
        let sell = policy.price_tld(&cost, Currency::USD, None).unwrap();
        // 9.20 * 1.3 = 11.96 → 11.99 ; 80 * 1.3 = 104 → 104.99
        assert_eq!(sell.register.unwrap().amount, dec!(11.99));
        assert_eq!(sell.transfer, None);
        assert_eq!(sell.restore.unwrap().amount, dec!(104.99));
        let tr = TldPrice {
            tld: "com.tr".into(),
            ..cost
        };
        let sell = policy.price_tld(&tr, Currency::USD, None).unwrap();
        assert_eq!(sell.register.unwrap().amount, dec!(14.20));
        assert_eq!(
            multi_year(&usd(dec!(11.99)), 3).unwrap().amount,
            dec!(35.97)
        );
        let json = serde_json::to_value(&policy).unwrap();
        assert_eq!(json["default"]["margin"]["type"], "percent");
        let back: PricingPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(back, policy);
    }
}
