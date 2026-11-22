//! Money, currencies and billing cycles.
//!
//! Rules (docs/ARCHITECTURE.md §1): amounts are [`Decimal`], never `f64`; the
//! database stores them as `numeric(20,6)` next to a `text` ISO 4217 code.
//!
//! * Arithmetic never mixes currencies: [`Money::try_add`]/[`Money::try_sub`]
//!   return [`MoneyError::CurrencyMismatch`]. There are deliberately no `Add`/`Sub`
//!   operator impls.
//! * Arithmetic keeps full precision; call [`Money::round`] when a value becomes
//!   user-visible (invoice line totals, gateway amounts). Rounding is
//!   half-away-from-zero ("commercial") to the currency's minor unit.
//! * [`BillingCycle::next_due`] handles month ends (Jan 31 + 1 month = Feb 28/29);
//!   use [`BillingCycle::next_due_anchored`] to avoid drifting (Feb 28 → Mar 31).

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use chrono::{Datelike, Days, Months, NaiveDate};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Errors from money and currency operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    /// Not a three-letter alphabetic ISO 4217 code.
    #[error("invalid currency code: {0:?}")]
    InvalidCurrency(String),
    /// Two amounts with different currencies were combined.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch {
        /// Currency of the left operand.
        left: Currency,
        /// Currency of the right operand.
        right: Currency,
    },
    /// Decimal overflow (or a value that does not fit the target integer type).
    #[error("arithmetic overflow")]
    Overflow,
    /// Division by zero (e.g. prorating over zero days).
    #[error("division by zero")]
    DivisionByZero,
    /// A textual amount could not be parsed.
    #[error("invalid amount: {0:?}")]
    InvalidAmount(String),
}

// ---------------------------------------------------------------------------
// Currency
// ---------------------------------------------------------------------------

/// ISO 4217 alphabetic currency code (always stored upper-case, e.g. `TRY`).
///
/// Validation is syntactic (three ASCII letters); whether the tenant actually
/// accepts the currency is a business rule (`currencies` table).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Currency([u8; 3]);

impl Currency {
    /// Turkish lira.
    pub const TRY: Currency = Currency(*b"TRY");
    /// US dollar.
    pub const USD: Currency = Currency(*b"USD");
    /// Euro.
    pub const EUR: Currency = Currency(*b"EUR");
    /// Pound sterling.
    pub const GBP: Currency = Currency(*b"GBP");

    /// Parses and normalises a code (`"try"` → `TRY`).
    pub fn new(code: &str) -> Result<Self, MoneyError> {
        let code = code.trim();
        let bytes = code.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_alphabetic) {
            return Err(MoneyError::InvalidCurrency(code.chars().take(16).collect()));
        }
        Ok(Self([
            bytes[0].to_ascii_uppercase(),
            bytes[1].to_ascii_uppercase(),
            bytes[2].to_ascii_uppercase(),
        ]))
    }

    /// The upper-case code.
    pub const fn as_str(&self) -> &str {
        match std::str::from_utf8(&self.0) {
            Ok(s) => s,
            // Unreachable: constructors only accept ASCII letters.
            Err(_) => "XXX",
        }
    }

    /// Number of decimal places of the minor unit (ISO 4217 exponent):
    /// 0 for e.g. JPY/KRW, 3 for e.g. KWD/BHD, 4 for CLF, otherwise 2.
    ///
    /// Gateways with their own conventions (Stripe treats some currencies
    /// differently) must adapt in the gateway implementation.
    pub fn minor_units(&self) -> u32 {
        match &self.0 {
            b"BIF" | b"CLP" | b"DJF" | b"GNF" | b"ISK" | b"JPY" | b"KMF" | b"KRW" | b"PYG"
            | b"RWF" | b"UGX" | b"UYI" | b"VND" | b"VUV" | b"XAF" | b"XOF" | b"XPF" => 0,
            b"BHD" | b"IQD" | b"JOD" | b"KWD" | b"LYD" | b"OMR" | b"TND" => 3,
            b"CLF" | b"UYW" => 4,
            _ => 2,
        }
    }
}

impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Currency({})", self.as_str())
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Currency {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for Currency {
    type Error = MoneyError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl Serialize for Currency {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

crate::__sqlx_text_newtype!(Currency);

// ---------------------------------------------------------------------------
// Money
// ---------------------------------------------------------------------------

/// An amount in a specific currency.
///
/// Serialized as `{"amount": "12.50", "currency": "TRY"}` (amount as a string, so
/// JSON consumers never round-trip through floating point). Equality is numeric
/// (`1.0 TRY == 1.00 TRY`). There is no `PartialOrd`; use [`Money::try_cmp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Money {
    /// The amount (full precision; may have more decimals than the currency).
    pub amount: Decimal,
    /// ISO 4217 currency.
    pub currency: Currency,
}

impl Money {
    /// Creates a money value.
    pub const fn new(amount: Decimal, currency: Currency) -> Self {
        Self { amount, currency }
    }

    /// Zero in `currency`.
    pub const fn zero(currency: Currency) -> Self {
        Self::new(Decimal::ZERO, currency)
    }

    /// Builds from integer minor units (`1250` + `TRY` → `12.50 TRY`).
    pub fn from_minor_units(units: i64, currency: Currency) -> Self {
        Self::new(Decimal::new(units, currency.minor_units()), currency)
    }

    /// Rounds to the currency's minor unit and returns integer minor units
    /// (`12.505 TRY` → `1251`). Used by gateways such as Stripe/PayTR.
    pub fn to_minor_units(&self) -> Result<i64, MoneyError> {
        let factor = Decimal::from(10_i64.pow(self.currency.minor_units()));
        self.round()
            .amount
            .checked_mul(factor)
            .and_then(|d| d.to_i64())
            .ok_or(MoneyError::Overflow)
    }

    /// `amount == 0`.
    pub fn is_zero(&self) -> bool {
        self.amount.is_zero()
    }

    /// `amount > 0`.
    pub fn is_positive(&self) -> bool {
        self.amount > Decimal::ZERO
    }

    /// `amount < 0`.
    pub fn is_negative(&self) -> bool {
        self.amount < Decimal::ZERO
    }

    /// Errors unless both values share a currency.
    pub fn ensure_same_currency(&self, other: &Money) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            })
        }
    }

    /// `self + other`; fails on currency mismatch or overflow.
    pub fn try_add(&self, other: &Money) -> Result<Money, MoneyError> {
        self.ensure_same_currency(other)?;
        self.amount
            .checked_add(other.amount)
            .map(|a| Money::new(a, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// `self - other`; fails on currency mismatch or overflow.
    pub fn try_sub(&self, other: &Money) -> Result<Money, MoneyError> {
        self.ensure_same_currency(other)?;
        self.amount
            .checked_sub(other.amount)
            .map(|a| Money::new(a, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// Multiplies by a scalar (quantity, rate). Not rounded.
    pub fn try_mul(&self, factor: Decimal) -> Result<Money, MoneyError> {
        self.amount
            .checked_mul(factor)
            .map(|a| Money::new(a, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// `rate_percent` percent of this amount (`20` → 20 %). Not rounded.
    pub fn percent(&self, rate_percent: Decimal) -> Result<Money, MoneyError> {
        let a = self
            .amount
            .checked_mul(rate_percent)
            .and_then(|v| v.checked_div(Decimal::ONE_HUNDRED))
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(a, self.currency))
    }

    /// `self * numerator / denominator`, e.g. prorating by used/total days. Not rounded.
    pub fn prorate(&self, numerator: i64, denominator: i64) -> Result<Money, MoneyError> {
        if denominator == 0 {
            return Err(MoneyError::DivisionByZero);
        }
        let a = self
            .amount
            .checked_mul(Decimal::from(numerator))
            .and_then(|v| v.checked_div(Decimal::from(denominator)))
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(a, self.currency))
    }

    /// Converts with an exchange rate (`1 self.currency = rate to`). Not rounded.
    pub fn convert(&self, rate: Decimal, to: Currency) -> Result<Money, MoneyError> {
        self.amount
            .checked_mul(rate)
            .map(|a| Money::new(a, to))
            .ok_or(MoneyError::Overflow)
    }

    /// Rounds half-away-from-zero to the currency's minor unit (`12.345 TRY` → `12.35`).
    #[must_use]
    pub fn round(&self) -> Money {
        self.round_dp(self.currency.minor_units())
    }

    /// Rounds half-away-from-zero to `dp` decimal places.
    #[must_use]
    pub fn round_dp(&self, dp: u32) -> Money {
        self.round_with(dp, RoundingStrategy::MidpointAwayFromZero)
    }

    /// Rounds with an explicit strategy.
    #[must_use]
    pub fn round_with(&self, dp: u32, strategy: RoundingStrategy) -> Money {
        Money::new(
            self.amount.round_dp_with_strategy(dp, strategy),
            self.currency,
        )
    }

    /// Absolute value.
    #[must_use]
    pub fn abs(&self) -> Money {
        Money::new(self.amount.abs(), self.currency)
    }

    /// Negated value.
    #[must_use]
    pub fn negate(&self) -> Money {
        Money::new(-self.amount, self.currency)
    }

    /// Compares two amounts of the same currency.
    pub fn try_cmp(&self, other: &Money) -> Result<Ordering, MoneyError> {
        self.ensure_same_currency(other)?;
        Ok(self.amount.cmp(&other.amount))
    }

    /// Sums `items`, all of which must be in `currency` (empty → zero).
    pub fn sum<'a>(
        currency: Currency,
        items: impl IntoIterator<Item = &'a Money>,
    ) -> Result<Money, MoneyError> {
        items
            .into_iter()
            .try_fold(Money::zero(currency), |acc, m| acc.try_add(m))
    }

    /// The amount rounded to minor units as a plain string (`"12.50"`), the format
    /// most gateway APIs expect.
    pub fn to_decimal_string(&self) -> String {
        let mut rounded = self.round().amount;
        rounded.rescale(self.currency.minor_units());
        rounded.to_string()
    }
}

/// `"12.50 TRY"`. Not localized — formatting for humans happens in the web layer.
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.amount, self.currency)
    }
}

/// Parses `"12.50 TRY"` (amount, whitespace, currency).
impl FromStr for Money {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split_whitespace();
        let (Some(amount), Some(currency), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(MoneyError::InvalidAmount(s.chars().take(64).collect()));
        };
        let amount = Decimal::from_str_exact(amount)
            .or_else(|_| Decimal::from_str(amount))
            .map_err(|_| MoneyError::InvalidAmount(amount.chars().take(64).collect()))?;
        Ok(Money::new(amount, Currency::new(currency)?))
    }
}

// ---------------------------------------------------------------------------
// BillingCycle
// ---------------------------------------------------------------------------

crate::string_enum! {
    /// How often a product is billed (`product_pricing`, `services.billing_cycle`).
    pub enum BillingCycle {
        /// Every month.
        Monthly => "monthly",
        /// Every 3 months.
        Quarterly => "quarterly",
        /// Every 6 months.
        Semiannual => "semiannual",
        /// Every 12 months.
        Annual => "annual",
        /// Every 24 months.
        Biennial => "biennial",
        /// Every 36 months.
        Triennial => "triennial",
        /// Paid once, never renews.
        OneTime => "one_time",
    }
}

impl BillingCycle {
    /// Length of one period in months; `None` for [`BillingCycle::OneTime`].
    pub const fn months(&self) -> Option<u32> {
        match self {
            Self::Monthly => Some(1),
            Self::Quarterly => Some(3),
            Self::Semiannual => Some(6),
            Self::Annual => Some(12),
            Self::Biennial => Some(24),
            Self::Triennial => Some(36),
            Self::OneTime => None,
        }
    }

    /// `false` only for [`BillingCycle::OneTime`].
    pub const fn is_recurring(&self) -> bool {
        self.months().is_some()
    }

    /// Due date one period after `due`, clamping to the month end
    /// (`2024-01-31` monthly → `2024-02-29`). `None` for one-time or overflow.
    ///
    /// Chaining this drifts after a clamp (Feb 29 → Mar 29); for recurring
    /// schedules prefer [`next_due_anchored`](Self::next_due_anchored).
    pub fn next_due(&self, due: NaiveDate) -> Option<NaiveDate> {
        self.add_periods(due, 1)
    }

    /// Like [`next_due`](Self::next_due) but restores the service's original day
    /// of month (`anchor_day`, 1–31; usually the registration day) whenever the
    /// target month has it: Feb 29 with anchor 31 → Mar 31.
    pub fn next_due_anchored(&self, due: NaiveDate, anchor_day: u32) -> Option<NaiveDate> {
        let next = self.add_periods(due, 1)?;
        let day = anchor_day
            .clamp(1, 31)
            .min(days_in_month(next.year(), next.month())?);
        next.with_day(day)
    }

    /// `start` plus `periods` whole periods, clamped to month end. Computing from
    /// the original start date (instead of chaining) never drifts.
    pub fn add_periods(&self, start: NaiveDate, periods: u32) -> Option<NaiveDate> {
        let months = self.months()?.checked_mul(periods)?;
        start.checked_add_months(Months::new(months))
    }

    /// Last day covered by a period starting at `start` (`next_due - 1 day`),
    /// e.g. for invoice line descriptions. `None` for one-time.
    pub fn period_end(&self, start: NaiveDate) -> Option<NaiveDate> {
        self.next_due(start)?.checked_sub_days(Days::new(1))
    }
}

/// Number of days in the given month (`None` for an invalid year/month).
pub fn days_in_month(year: i32, month: u32) -> Option<u32> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = first.checked_add_months(Months::new(1))?;
    u32::try_from(next.signed_duration_since(first).num_days()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn currency_validation() {
        assert_eq!(Currency::new("try").unwrap(), Currency::TRY);
        assert_eq!(Currency::new(" usd ").unwrap().as_str(), "USD");
        assert!(Currency::new("TL").is_err());
        assert!(Currency::new("US1").is_err());
        assert!(Currency::new("EURO").is_err());
        assert!(Currency::new("ÜSD").is_err());
        assert_eq!(Currency::new("JPY").unwrap().minor_units(), 0);
        assert_eq!(Currency::new("KWD").unwrap().minor_units(), 3);
        assert_eq!(Currency::TRY.minor_units(), 2);
    }

    #[test]
    fn currency_serde() {
        assert_eq!(serde_json::to_string(&Currency::EUR).unwrap(), "\"EUR\"");
        let c: Currency = serde_json::from_str("\"gbp\"").unwrap();
        assert_eq!(c, Currency::GBP);
        assert!(serde_json::from_str::<Currency>("\"xx\"").is_err());
    }

    #[test]
    fn add_and_mismatch() {
        let a = Money::new(dec!(10.25), Currency::TRY);
        let b = Money::new(dec!(0.75), Currency::TRY);
        assert_eq!(a.try_add(&b).unwrap(), Money::new(dec!(11), Currency::TRY));
        assert_eq!(a.try_sub(&b).unwrap().amount, dec!(9.50));
        let usd = Money::new(dec!(1), Currency::USD);
        assert_eq!(
            a.try_add(&usd),
            Err(MoneyError::CurrencyMismatch {
                left: Currency::TRY,
                right: Currency::USD
            })
        );
        assert!(a.try_cmp(&usd).is_err());
        assert_eq!(a.try_cmp(&b).unwrap(), Ordering::Greater);
    }

    #[test]
    fn overflow_is_an_error() {
        let max = Money::new(Decimal::MAX, Currency::USD);
        assert_eq!(max.try_add(&max), Err(MoneyError::Overflow));
        assert_eq!(max.try_mul(dec!(2)), Err(MoneyError::Overflow));
    }

    #[test]
    fn sum_works() {
        let items = [
            Money::new(dec!(1.10), Currency::EUR),
            Money::new(dec!(2.20), Currency::EUR),
        ];
        assert_eq!(
            Money::sum(Currency::EUR, &items).unwrap().amount,
            dec!(3.30)
        );
        assert!(Money::sum(Currency::EUR, &[]).unwrap().is_zero());
        assert!(Money::sum(Currency::USD, &items).is_err());
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(
            Money::new(dec!(12.345), Currency::TRY).round().amount,
            dec!(12.35)
        );
        assert_eq!(
            Money::new(dec!(12.344), Currency::TRY).round().amount,
            dec!(12.34)
        );
        assert_eq!(
            Money::new(dec!(-0.005), Currency::TRY).round().amount,
            dec!(-0.01)
        );
        assert_eq!(
            Money::new(dec!(99.5), Currency::new("JPY").unwrap())
                .round()
                .amount,
            dec!(100)
        );
        assert_eq!(
            Money::new(dec!(1.0005), Currency::new("KWD").unwrap())
                .round()
                .amount,
            dec!(1.001)
        );
    }

    #[test]
    fn minor_units_roundtrip() {
        let m = Money::new(dec!(12.505), Currency::TRY);
        assert_eq!(m.to_minor_units().unwrap(), 1251);
        assert_eq!(
            Money::from_minor_units(1251, Currency::TRY).amount,
            dec!(12.51)
        );
        let jpy = Currency::new("JPY").unwrap();
        assert_eq!(Money::from_minor_units(500, jpy).amount, dec!(500));
        assert_eq!(Money::new(dec!(500), jpy).to_minor_units().unwrap(), 500);
        assert_eq!(
            Money::new(dec!(7), Currency::USD).to_decimal_string(),
            "7.00"
        );
        assert_eq!(
            Money::new(dec!(7.125), Currency::USD).to_decimal_string(),
            "7.13"
        );
    }

    #[test]
    fn percent_prorate_convert() {
        let m = Money::new(dec!(100), Currency::TRY);
        assert_eq!(m.percent(dec!(20)).unwrap().amount, dec!(20));
        assert_eq!(m.prorate(15, 30).unwrap().amount, dec!(50));
        assert_eq!(m.prorate(1, 0), Err(MoneyError::DivisionByZero));
        let usd = m.convert(dec!(0.03), Currency::USD).unwrap();
        assert_eq!(usd, Money::new(dec!(3), Currency::USD));
    }

    #[test]
    fn money_display_parse_serde() {
        let m = Money::new(dec!(12.50), Currency::TRY);
        assert_eq!(m.to_string(), "12.50 TRY");
        assert_eq!("12.50 TRY".parse::<Money>().unwrap(), m);
        assert!("12.50".parse::<Money>().is_err());
        assert!("abc TRY".parse::<Money>().is_err());
        let json = serde_json::to_value(m).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"amount": "12.50", "currency": "TRY"})
        );
        let back: Money = serde_json::from_value(json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn cycle_months_and_strings() {
        assert_eq!(BillingCycle::Quarterly.months(), Some(3));
        assert_eq!(BillingCycle::OneTime.months(), None);
        assert!(!BillingCycle::OneTime.is_recurring());
        assert_eq!(BillingCycle::Semiannual.as_str(), "semiannual");
        assert_eq!(
            "one_time".parse::<BillingCycle>().unwrap(),
            BillingCycle::OneTime
        );
        assert!("weekly".parse::<BillingCycle>().is_err());
        assert_eq!(BillingCycle::ALL.len(), 7);
    }

    #[test]
    fn next_due_plain() {
        assert_eq!(
            BillingCycle::Monthly.next_due(d(2026, 3, 15)),
            Some(d(2026, 4, 15))
        );
        assert_eq!(
            BillingCycle::Annual.next_due(d(2026, 3, 15)),
            Some(d(2027, 3, 15))
        );
        assert_eq!(
            BillingCycle::Triennial.next_due(d(2026, 1, 1)),
            Some(d(2029, 1, 1))
        );
        assert_eq!(
            BillingCycle::Quarterly.next_due(d(2026, 11, 30)),
            Some(d(2027, 2, 28))
        );
        assert_eq!(BillingCycle::OneTime.next_due(d(2026, 1, 1)), None);
    }

    #[test]
    fn next_due_month_end_clamps() {
        assert_eq!(
            BillingCycle::Monthly.next_due(d(2024, 1, 31)),
            Some(d(2024, 2, 29))
        );
        assert_eq!(
            BillingCycle::Monthly.next_due(d(2025, 1, 31)),
            Some(d(2025, 2, 28))
        );
        assert_eq!(
            BillingCycle::Monthly.next_due(d(2026, 3, 31)),
            Some(d(2026, 4, 30))
        );
        // Leap day + 1 year.
        assert_eq!(
            BillingCycle::Annual.next_due(d(2024, 2, 29)),
            Some(d(2025, 2, 28))
        );
        assert_eq!(
            BillingCycle::Biennial.next_due(d(2024, 2, 29)),
            Some(d(2026, 2, 28))
        );
        assert_eq!(
            BillingCycle::Semiannual.next_due(d(2026, 8, 31)),
            Some(d(2027, 2, 28))
        );
    }

    #[test]
    fn next_due_anchored_restores_day() {
        let c = BillingCycle::Monthly;
        let feb = c.next_due_anchored(d(2026, 1, 31), 31).unwrap();
        assert_eq!(feb, d(2026, 2, 28));
        let mar = c.next_due_anchored(feb, 31).unwrap();
        assert_eq!(mar, d(2026, 3, 31));
        let apr = c.next_due_anchored(mar, 31).unwrap();
        assert_eq!(apr, d(2026, 4, 30));
        assert_eq!(
            c.next_due_anchored(d(2026, 2, 28), 30),
            Some(d(2026, 3, 30))
        );
        assert_eq!(
            c.next_due_anchored(d(2026, 5, 10), 10),
            Some(d(2026, 6, 10))
        );
        assert_eq!(
            BillingCycle::OneTime.next_due_anchored(d(2026, 5, 10), 10),
            None
        );
    }

    #[test]
    fn add_periods_does_not_drift() {
        let start = d(2026, 1, 31);
        assert_eq!(
            BillingCycle::Monthly.add_periods(start, 2),
            Some(d(2026, 3, 31))
        );
        assert_eq!(BillingCycle::Monthly.add_periods(start, 0), Some(start));
        assert_eq!(
            BillingCycle::Monthly.period_end(d(2026, 1, 15)),
            Some(d(2026, 2, 14))
        );
    }

    #[test]
    fn days_in_month_works() {
        assert_eq!(days_in_month(2024, 2), Some(29));
        assert_eq!(days_in_month(2025, 2), Some(28));
        assert_eq!(days_in_month(2026, 12), Some(31));
        assert_eq!(days_in_month(2026, 13), None);
    }
}
