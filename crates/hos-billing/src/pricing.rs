//! Cart pricing: product + billing cycle + configurable options + addons +
//! domain lines → a complete [`Quote`].
//!
//! The engine is split in two:
//!
//! * [`price`] — **pure** and deterministic: resolved lines + coupon + tax
//!   rates in, quote out. Unit-tested without a database.
//! * [`Billing::quote`] / [`Billing::quote_cart`] — resolve catalog prices,
//!   config options, TLD prices, the coupon and the client's tax rules from
//!   the database, then call [`price`].
//!
//! ## Rounding
//!
//! Every money component of a line (base = quantity × unit price, setup fee,
//! discount) is rounded half-away-from-zero to the currency's minor unit
//! **per line**. Taxes are then computed exactly like the invoice will compute
//! them (`hos_db::repo::invoices::compute_totals`: one rounding per tax rate on
//! the taxable base), so the quote total always equals the invoice total.
//!
//! ## Tax-inclusive prices
//!
//! With `billing.prices_include_tax`, catalog prices are gross. Each taxable
//! component is converted to net by dividing by the combined tax factor
//! (compound rates included) before rounding; the resulting total can differ
//! from the sum of gross prices by at most one minor unit per component.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use hos_core::CoreError;
use hos_core::domain::Locale;
use hos_core::ids::{ClientId, CouponId, ProductAddonId, ProductId, TaxRuleId, TenantId};
use hos_core::money::{BillingCycle, Currency, Money};
use hos_db::enums::{ClientKind, ConfigOptionType, DiscountType, LineItemType, PayType, TldAction};
use hos_db::repo::catalog::{self, CouponRow, TaxRuleRow};
use hos_db::repo::clients::ClientRow;
use hos_db::repo::invoices::{TotalsLine, TotalsTax, compute_totals};
use hos_db::repo::{billing_support, carts, clients, domains};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;

use crate::settings::BillingSettings;
use crate::{Billing, BillingResult};

// ---------------------------------------------------------------------------
// pure engine: inputs
// ---------------------------------------------------------------------------

/// A configurable option charge folded into a product line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigOptionCharge {
    pub option_id: hos_core::ids::ConfigOptionId,
    pub option_name: String,
    pub value_id: hos_db::ids::ConfigOptionValueId,
    pub value_label: String,
    /// Module value (`config_option_values.value`).
    pub value: String,
    /// Provisioning key (`config_options.module_key`).
    pub module_key: Option<String>,
    /// Units (quantity options), else 1.
    pub quantity: i32,
    /// Price per unit per period (catalog, gross when prices include tax).
    pub price: Decimal,
    /// Setup fee per unit.
    pub setup_fee: Decimal,
}

/// A resolved line to price.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceLine {
    /// Index of the parent line (addons, bundled domains); must precede this line.
    pub parent: Option<usize>,
    pub item_type: LineItemType,
    pub product_id: Option<ProductId>,
    pub addon_id: Option<ProductAddonId>,
    pub domain_name: Option<String>,
    /// TLD of domain lines (`com`, `com.tr`), for coupon restrictions.
    pub tld: Option<String>,
    pub description: String,
    pub billing_cycle: Option<BillingCycle>,
    pub years: Option<i16>,
    pub quantity: i32,
    /// First-period price per unit (config options included).
    pub unit_price: Decimal,
    /// Recurring price per unit and period (domains: renewal price for `years`).
    pub recurring_unit_price: Decimal,
    /// Setup fee for the whole line.
    pub setup_fee: Decimal,
    pub taxable: bool,
    pub config_options: Vec<ConfigOptionCharge>,
}

impl PriceLine {
    /// A simple product line (tests, admin quotes).
    pub fn product(product_id: ProductId, cycle: BillingCycle, price: Decimal) -> Self {
        Self {
            parent: None,
            item_type: LineItemType::Product,
            product_id: Some(product_id),
            addon_id: None,
            domain_name: None,
            tld: None,
            description: String::new(),
            billing_cycle: Some(cycle),
            years: None,
            quantity: 1,
            unit_price: price,
            recurring_unit_price: price,
            setup_fee: Decimal::ZERO,
            taxable: true,
            config_options: Vec::new(),
        }
    }
}

/// Coupon terms relevant for pricing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CouponSpec {
    pub id: Option<CouponId>,
    pub code: String,
    pub discount_type: DiscountType,
    /// Percent (0–100) or amount in `currency`.
    pub value: Decimal,
    pub currency: Option<Currency>,
    pub product_ids: Vec<ProductId>,
    pub addon_ids: Vec<ProductAddonId>,
    pub tlds: Vec<String>,
    pub billing_cycles: Vec<BillingCycle>,
    pub applies_to_setup: bool,
    /// Also discounts renewals (else first period only).
    pub recurring: bool,
    /// Renewals discounted when recurring (0 = forever).
    pub recurring_limit: i32,
    /// Minimum pre-discount subtotal.
    pub min_order_amount: Option<Decimal>,
    /// Bundle coupon: applies only when every product of `product_ids` is
    /// in the order.
    pub requires_all_products: bool,
    /// Items of the product bundle this coupon belongs to (`(product,
    /// cycle)`; empty for other coupons). A bundle discount needs one line
    /// per item with that product and cycle and discounts exactly one unit
    /// of that line per item — never a second unit or another cycle.
    pub bundle_items: Vec<(ProductId, BillingCycle)>,
}

impl CouponSpec {
    /// A coupon applying `discount_type`/`value` to everything.
    pub fn simple(code: &str, discount_type: DiscountType, value: Decimal) -> Self {
        Self {
            id: None,
            code: code.to_owned(),
            discount_type,
            value,
            currency: None,
            product_ids: Vec::new(),
            addon_ids: Vec::new(),
            tlds: Vec::new(),
            billing_cycles: Vec::new(),
            applies_to_setup: false,
            recurring: false,
            recurring_limit: 0,
            min_order_amount: None,
            requires_all_products: false,
            bundle_items: Vec::new(),
        }
    }

    /// For a bundle: the line discounted for each bundle item (a product
    /// line with the item's product and cycle, each line used once);
    /// `None` when an item has no such line.
    pub fn bundle_lines(&self, lines: &[PriceLine]) -> Option<Vec<usize>> {
        let mut used: Vec<usize> = Vec::with_capacity(self.bundle_items.len());
        for (product, cycle) in &self.bundle_items {
            let i = lines.iter().enumerate().position(|(i, l)| {
                l.item_type == LineItemType::Product
                    && l.product_id == Some(*product)
                    && l.billing_cycle == Some(*cycle)
                    && !used.contains(&i)
            })?;
            used.push(i);
        }
        Some(used)
    }

    /// For a bundle coupon: whether every listed product (bundle: every
    /// item with its cycle) is among `lines`.
    pub fn bundle_complete(&self, lines: &[PriceLine]) -> bool {
        if !self.bundle_items.is_empty() {
            return self.bundle_lines(lines).is_some();
        }
        !self.requires_all_products
            || self.product_ids.iter().all(|p| {
                lines
                    .iter()
                    .any(|l| l.item_type == LineItemType::Product && l.product_id == Some(*p))
            })
    }

    fn unrestricted(&self) -> bool {
        self.product_ids.is_empty() && self.addon_ids.is_empty() && self.tlds.is_empty()
    }

    /// Whether the coupon covers a line. With no product/addon/TLD lists the
    /// coupon covers everything; otherwise only listed items.
    pub fn applies_to(&self, line: &PriceLine) -> bool {
        if let Some(cycle) = line.billing_cycle
            && !self.billing_cycles.is_empty()
            && !self.billing_cycles.contains(&cycle)
        {
            return false;
        }
        if self.unrestricted() {
            return true;
        }
        match line.item_type {
            LineItemType::Product => line
                .product_id
                .is_some_and(|p| self.product_ids.contains(&p)),
            LineItemType::Addon => line.addon_id.is_some_and(|a| self.addon_ids.contains(&a)),
            LineItemType::DomainRegister
            | LineItemType::DomainTransfer
            | LineItemType::DomainRenew => line.tld.as_deref().is_some_and(|t| {
                self.tlds
                    .iter()
                    .any(|c| c.trim_start_matches('.').eq_ignore_ascii_case(t))
            }),
            LineItemType::Other => false,
        }
    }
}

impl From<&CouponRow> for CouponSpec {
    fn from(c: &CouponRow) -> Self {
        Self {
            id: Some(c.id),
            code: c.code.clone(),
            discount_type: c.discount_type,
            value: c.value,
            currency: c.currency,
            product_ids: c.product_ids.clone(),
            addon_ids: c.addon_ids.clone(),
            tlds: c.tlds.clone(),
            billing_cycles: c
                .billing_cycles
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect(),
            applies_to_setup: c.applies_to_setup,
            recurring: c.recurring,
            recurring_limit: c.recurring_limit,
            min_order_amount: c.min_order_amount,
            requires_all_products: c.requires_all_products,
            bundle_items: Vec::new(),
        }
    }
}

/// A tax rate to apply (snapshot of a `tax_rules` row), in application order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxRate {
    pub tax_rule_id: Option<TaxRuleId>,
    pub name: String,
    /// Percent.
    pub rate: Decimal,
    pub is_compound: bool,
}

impl From<&TaxRuleRow> for TaxRate {
    fn from(r: &TaxRuleRow) -> Self {
        Self {
            tax_rule_id: Some(r.id),
            name: r.name.clone(),
            rate: r.rate,
            is_compound: r.is_compound,
        }
    }
}

/// Everything [`price`] needs.
#[derive(Debug, Clone, PartialEq)]
pub struct PricingInput {
    pub currency: Currency,
    pub lines: Vec<PriceLine>,
    pub coupon: Option<CouponSpec>,
    /// Applicable rates (empty = no tax / exempt).
    pub taxes: Vec<TaxRate>,
    pub prices_include_tax: bool,
}

// ---------------------------------------------------------------------------
// pure engine: output
// ---------------------------------------------------------------------------

/// A priced line. All amounts exclude tax and are rounded to the currency.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteLine {
    pub index: usize,
    pub parent: Option<usize>,
    pub item_type: LineItemType,
    pub product_id: Option<ProductId>,
    pub addon_id: Option<ProductAddonId>,
    pub domain_name: Option<String>,
    pub description: String,
    pub billing_cycle: Option<BillingCycle>,
    pub years: Option<i16>,
    pub quantity: i32,
    /// Net unit price (6 decimals).
    pub unit_price: Decimal,
    /// `round(quantity × unit_price)`.
    pub base_amount: Decimal,
    /// Net setup fee.
    pub setup_fee: Decimal,
    /// Coupon discount (positive).
    pub discount: Decimal,
    /// `base_amount + setup_fee - discount`.
    pub amount: Decimal,
    /// Renewal price per period without discount.
    pub list_recurring_amount: Decimal,
    /// Renewal price per period after a recurring coupon.
    pub recurring_amount: Decimal,
    pub taxable: bool,
    pub coupon_applied: bool,
    pub config_options: Vec<ConfigOptionCharge>,
}

/// One tax rate of the quote.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuoteTax {
    pub tax_rule_id: Option<TaxRuleId>,
    pub name: String,
    pub rate: Decimal,
    pub is_compound: bool,
    pub taxable_amount: Decimal,
    pub tax_amount: Decimal,
}

/// The coupon as applied.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppliedCoupon {
    pub id: Option<CouponId>,
    pub code: String,
    pub discount_type: DiscountType,
    /// Total first-period discount.
    pub discount: Decimal,
    pub recurring: bool,
    pub recurring_limit: i32,
}

/// Recurring charges per period length.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecurringTotal {
    /// Period length in months (domains: years × 12).
    pub months: u32,
    /// Sum of renewal prices (excl. tax).
    pub amount: Decimal,
}

/// A full price breakdown.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Quote {
    pub currency: Currency,
    pub lines: Vec<QuoteLine>,
    pub coupon: Option<AppliedCoupon>,
    /// Why the requested coupon was not applied (quote is priced without it).
    pub coupon_error: Option<String>,
    pub prices_include_tax: bool,
    /// Σ line amounts (after discounts, before tax).
    pub subtotal: Decimal,
    pub discount_total: Decimal,
    pub setup_total: Decimal,
    pub taxes: Vec<QuoteTax>,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub recurring: Vec<RecurringTotal>,
}

impl Quote {
    /// Total as money.
    pub fn total_money(&self) -> Money {
        Money::new(self.total, self.currency)
    }

    /// Tax lines for `hos_db::repo::invoices::NewInvoice::taxes`.
    pub fn tax_inputs(&self) -> Vec<hos_db::repo::invoices::TaxLineInput> {
        self.taxes
            .iter()
            .map(|t| hos_db::repo::invoices::TaxLineInput {
                tax_rule_id: t.tax_rule_id,
                name: t.name.clone(),
                rate: t.rate,
                is_compound: t.is_compound,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// pure engine: algorithm
// ---------------------------------------------------------------------------

fn round_money(d: Decimal, currency: Currency) -> Decimal {
    Money::new(d, currency).round().amount
}

fn round6(d: Decimal) -> Decimal {
    d.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero)
}

/// Combined multiplier of the tax rates (`1 + Σ`, compound rates on top of the
/// previous taxes) — the gross/net ratio.
pub fn tax_factor(taxes: &[TaxRate]) -> Decimal {
    let mut acc = Decimal::ZERO;
    for t in taxes {
        let r = t.rate / Decimal::ONE_HUNDRED;
        acc += if t.is_compound {
            (Decimal::ONE + acc) * r
        } else {
            r
        };
    }
    Decimal::ONE + acc
}

/// Discount of `amount` (quantity `qty`) under the coupon terms.
fn coupon_discount(
    c: &CouponSpec,
    base: Decimal,
    setup: Decimal,
    qty: i32,
    factor: Decimal,
    currency: Currency,
) -> Decimal {
    let q = Decimal::from(qty);
    let cap_with_setup = if c.applies_to_setup {
        base + setup
    } else {
        base
    };
    let d = match c.discount_type {
        DiscountType::Percent => {
            let pct = c.value.min(Decimal::ONE_HUNDRED) / Decimal::ONE_HUNDRED;
            round_money(base * pct, currency)
                + if c.applies_to_setup {
                    round_money(setup * pct, currency)
                } else {
                    Decimal::ZERO
                }
        }
        DiscountType::Fixed => round_money(c.value / factor * q, currency).min(cap_with_setup),
        DiscountType::PriceOverride => {
            let new_base = round_money(q * round6(c.value / factor), currency);
            (base - new_base).max(Decimal::ZERO)
        }
        DiscountType::FreeSetup => setup,
    };
    d.max(Decimal::ZERO).min(base + setup)
}

/// Prices resolved lines. Errors only on invalid input (bad quantity, a
/// parent index that does not precede its line, negative prices).
pub fn price(input: &PricingInput) -> Result<Quote, CoreError> {
    let cur = input.currency;
    let factor_all = if input.prices_include_tax && !input.taxes.is_empty() {
        tax_factor(&input.taxes)
    } else {
        Decimal::ONE
    };

    // 1. net components per line
    struct Net {
        unit: Decimal,
        recurring_unit: Decimal,
        base: Decimal,
        setup: Decimal,
        list_recurring: Decimal,
        factor: Decimal,
    }
    let mut nets = Vec::with_capacity(input.lines.len());
    for (i, l) in input.lines.iter().enumerate() {
        if l.quantity <= 0 {
            return Err(CoreError::validation(format!(
                "line {i}: quantity must be positive"
            )));
        }
        if l.parent.is_some_and(|p| p >= i) {
            return Err(CoreError::validation(format!(
                "line {i}: parent must precede the line"
            )));
        }
        if l.unit_price < Decimal::ZERO
            || l.setup_fee < Decimal::ZERO
            || l.recurring_unit_price < Decimal::ZERO
        {
            return Err(CoreError::validation(format!(
                "line {i}: prices must not be negative"
            )));
        }
        let factor = if l.taxable { factor_all } else { Decimal::ONE };
        let q = Decimal::from(l.quantity);
        let unit = round6(l.unit_price / factor);
        let recurring_unit = round6(l.recurring_unit_price / factor);
        nets.push(Net {
            unit,
            recurring_unit,
            base: round_money(q * unit, cur),
            setup: round_money(l.setup_fee / factor, cur),
            list_recurring: round_money(q * recurring_unit, cur),
            factor,
        });
    }

    // 2. coupon
    let mut coupon_error = None;
    let mut coupon = input.coupon.clone();
    if let Some(c) = &coupon {
        let pre_discount: Decimal = nets.iter().map(|n| n.base + n.setup).sum();
        if matches!(
            c.discount_type,
            DiscountType::Fixed | DiscountType::PriceOverride
        ) && c.currency.is_some_and(|cc| cc != cur)
        {
            coupon_error = Some(format!("coupon {} is not valid in {cur}", c.code));
        } else if c.min_order_amount.is_some_and(|m| pre_discount < m) {
            coupon_error = Some(format!(
                "coupon {} requires a minimum order of {} {cur}",
                c.code,
                c.min_order_amount.unwrap_or_default()
            ));
        } else if !input.lines.iter().any(|l| c.applies_to(l)) {
            coupon_error = Some(format!(
                "coupon {} does not apply to any item in the cart",
                c.code
            ));
        } else if !c.bundle_complete(&input.lines) {
            coupon_error = Some(format!(
                "coupon {} needs every product of its bundle in the cart",
                c.code
            ));
        }
        if coupon_error.is_some() {
            coupon = None;
        }
    }
    // A bundle discounts one unit of one line per bundle item.
    let bundle_lines: Option<Vec<usize>> = coupon
        .as_ref()
        .filter(|c| !c.bundle_items.is_empty())
        .map(|c| c.bundle_lines(&input.lines).unwrap_or_default());

    // 3. lines
    let mut lines = Vec::with_capacity(input.lines.len());
    let mut discount_sum = Decimal::ZERO;
    for (i, (l, n)) in input.lines.iter().zip(&nets).enumerate() {
        let (discount, recurring_discount, applied) = match &coupon {
            Some(c) if bundle_lines.is_some() => {
                if bundle_lines.as_ref().is_some_and(|b| b.contains(&i)) {
                    let one = round_money(n.unit, cur);
                    let d = coupon_discount(c, one, n.setup, 1, n.factor, cur);
                    let rd = if c.recurring {
                        let one_recurring = round_money(n.recurring_unit, cur);
                        coupon_discount(c, one_recurring, Decimal::ZERO, 1, n.factor, cur)
                    } else {
                        Decimal::ZERO
                    };
                    (d, rd, true)
                } else {
                    (Decimal::ZERO, Decimal::ZERO, false)
                }
            }
            Some(c) if c.applies_to(l) => {
                let d = coupon_discount(c, n.base, n.setup, l.quantity, n.factor, cur);
                let rd = if c.recurring {
                    coupon_discount(
                        c,
                        n.list_recurring,
                        Decimal::ZERO,
                        l.quantity,
                        n.factor,
                        cur,
                    )
                } else {
                    Decimal::ZERO
                };
                (d, rd, true)
            }
            _ => (Decimal::ZERO, Decimal::ZERO, false),
        };
        discount_sum += discount;
        lines.push(QuoteLine {
            index: i,
            parent: l.parent,
            item_type: l.item_type,
            product_id: l.product_id,
            addon_id: l.addon_id,
            domain_name: l.domain_name.clone(),
            description: l.description.clone(),
            billing_cycle: l.billing_cycle,
            years: l.years,
            quantity: l.quantity,
            unit_price: n.unit,
            base_amount: n.base,
            setup_fee: n.setup,
            discount,
            amount: n.base + n.setup - discount,
            list_recurring_amount: n.list_recurring,
            recurring_amount: (n.list_recurring - recurring_discount).max(Decimal::ZERO),
            taxable: l.taxable,
            coupon_applied: applied,
            config_options: l.config_options.clone(),
        });
    }

    // 4. taxes exactly as the invoice computes them
    let mut totals_lines = Vec::with_capacity(lines.len() * 3);
    for l in &lines {
        totals_lines.push(TotalsLine {
            amount: l.base_amount,
            taxable: l.taxable,
            is_discount: false,
        });
        if !l.setup_fee.is_zero() {
            totals_lines.push(TotalsLine {
                amount: l.setup_fee,
                taxable: l.taxable,
                is_discount: false,
            });
        }
        if !l.discount.is_zero() {
            totals_lines.push(TotalsLine {
                amount: -l.discount,
                taxable: l.taxable,
                is_discount: true,
            });
        }
    }
    let totals_taxes: Vec<TotalsTax> = input
        .taxes
        .iter()
        .map(|t| TotalsTax {
            rate: t.rate,
            is_compound: t.is_compound,
        })
        .collect();
    let totals = compute_totals(cur, &totals_lines, &totals_taxes);
    let taxes = input
        .taxes
        .iter()
        .zip(&totals.taxes)
        .map(|(t, (base, amount))| QuoteTax {
            tax_rule_id: t.tax_rule_id,
            name: t.name.clone(),
            rate: t.rate,
            is_compound: t.is_compound,
            taxable_amount: *base,
            tax_amount: *amount,
        })
        .collect();

    // 5. recurring summary
    let mut recurring: Vec<RecurringTotal> = Vec::new();
    for l in &lines {
        let months = match (l.billing_cycle, l.years) {
            (Some(c), _) => c.months(),
            (None, Some(y)) if y > 0 => u32::try_from(y).ok().map(|y| y * 12),
            _ => None,
        };
        let Some(months) = months else { continue };
        if l.recurring_amount.is_zero() {
            continue;
        }
        match recurring.iter_mut().find(|r| r.months == months) {
            Some(r) => r.amount += l.recurring_amount,
            None => recurring.push(RecurringTotal {
                months,
                amount: l.recurring_amount,
            }),
        }
    }
    recurring.sort_by_key(|r| r.months);

    let applied_coupon = coupon.map(|c| AppliedCoupon {
        id: c.id,
        code: c.code,
        discount_type: c.discount_type,
        discount: discount_sum,
        recurring: c.recurring,
        recurring_limit: c.recurring_limit,
    });

    Ok(Quote {
        currency: cur,
        setup_total: lines.iter().map(|l| l.setup_fee).sum(),
        lines,
        coupon: applied_coupon,
        coupon_error,
        prices_include_tax: input.prices_include_tax,
        subtotal: totals.subtotal,
        discount_total: totals.discount_total,
        taxes,
        tax_total: totals.tax_total,
        total: totals.total,
        recurring,
    })
}

// ---------------------------------------------------------------------------
// prorating
// ---------------------------------------------------------------------------

/// Proration of a plan change in the middle of a billing period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Proration {
    /// Days of the current period still unused.
    pub remaining_days: i64,
    /// Length of the current period in days.
    pub period_days: i64,
    /// Unused value of the current plan (credit).
    pub credit: Decimal,
    /// Cost of the new plan for the remaining days.
    pub charge: Decimal,
    /// `charge - credit` (negative = the client gets credit).
    pub amount: Decimal,
}

/// Prorates switching from `old_price` to `new_price` (both per the same
/// period `[period_start, period_end)`) on `today`. Rounded to the currency.
pub fn prorate(
    old_price: Decimal,
    new_price: Decimal,
    period_start: chrono::NaiveDate,
    period_end: chrono::NaiveDate,
    today: chrono::NaiveDate,
    currency: Currency,
) -> Result<Proration, CoreError> {
    let period_days = (period_end - period_start).num_days();
    if period_days <= 0 {
        return Err(CoreError::validation("billing period is empty"));
    }
    let remaining_days = (period_end - today).num_days().clamp(0, period_days);
    let credit = Money::new(old_price, currency)
        .prorate(remaining_days, period_days)?
        .round()
        .amount;
    let charge = Money::new(new_price, currency)
        .prorate(remaining_days, period_days)?
        .round()
        .amount;
    Ok(Proration {
        remaining_days,
        period_days,
        credit,
        charge,
        amount: charge - credit,
    })
}

/// Longest span [`charge_until`] prices (100 years of monthly periods).
const MAX_PERIODS: u32 = 1200;

/// First charge of a recurring item at `price` per `months`-month period
/// that runs from `today` until `end` (the due date of the service it
/// renews with): the whole periods counted back from `end` at full price,
/// plus the period holding `today` by its remaining days. A service paid
/// several periods ahead therefore pays for each of them, never for one.
/// `end` on or before `today`: one full period.
pub fn charge_until(
    price: Decimal,
    months: u32,
    today: chrono::NaiveDate,
    end: chrono::NaiveDate,
    currency: Currency,
) -> Result<Decimal, CoreError> {
    if end <= today || months == 0 {
        return Ok(price);
    }
    let mut period_end = end;
    for whole in 0..MAX_PERIODS {
        let start = period_end
            .checked_sub_months(chrono::Months::new(months))
            .ok_or_else(|| CoreError::validation("billing period out of range"))?;
        if start <= today {
            let part = prorate(Decimal::ZERO, price, start, period_end, today, currency)?.charge;
            let full = Money::new(price * Decimal::from(whole), currency)
                .round()
                .amount;
            return Ok(full + part);
        }
        period_end = start;
    }
    Err(CoreError::validation("the service is paid too far ahead"))
}

// ---------------------------------------------------------------------------
// labels
// ---------------------------------------------------------------------------

/// Human label of a billing cycle.
pub fn cycle_label(cycle: BillingCycle, locale: Locale) -> &'static str {
    match (cycle, locale) {
        (BillingCycle::Monthly, Locale::Tr) => "Aylık",
        (BillingCycle::Quarterly, Locale::Tr) => "3 Aylık",
        (BillingCycle::Semiannual, Locale::Tr) => "6 Aylık",
        (BillingCycle::Annual, Locale::Tr) => "Yıllık",
        (BillingCycle::Biennial, Locale::Tr) => "2 Yıllık",
        (BillingCycle::Triennial, Locale::Tr) => "3 Yıllık",
        (BillingCycle::OneTime, Locale::Tr) => "Tek seferlik",
        (BillingCycle::Monthly, Locale::En) => "Monthly",
        (BillingCycle::Quarterly, Locale::En) => "Quarterly",
        (BillingCycle::Semiannual, Locale::En) => "Semi-annually",
        (BillingCycle::Annual, Locale::En) => "Annually",
        (BillingCycle::Biennial, Locale::En) => "Biennially",
        (BillingCycle::Triennial, Locale::En) => "Triennially",
        (BillingCycle::OneTime, Locale::En) => "One time",
    }
}

fn domain_label(action: LineItemType, name: &str, years: i16, locale: Locale) -> String {
    let (what, unit) = match (action, locale) {
        (LineItemType::DomainTransfer, Locale::Tr) => ("Alan adı transferi", "yıl"),
        (LineItemType::DomainRenew, Locale::Tr) => ("Alan adı yenileme", "yıl"),
        (_, Locale::Tr) => ("Alan adı kaydı", "yıl"),
        (LineItemType::DomainTransfer, Locale::En) => ("Domain transfer", "year(s)"),
        (LineItemType::DomainRenew, Locale::En) => ("Domain renewal", "year(s)"),
        (_, Locale::En) => ("Domain registration", "year(s)"),
    };
    format!("{what} - {name} ({years} {unit})")
}

// ---------------------------------------------------------------------------
// database-backed resolution
// ---------------------------------------------------------------------------

/// One item to quote (mirrors a cart item).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteItem {
    /// Index (in the request) of the parent product line, for addons.
    #[serde(default)]
    pub parent: Option<usize>,
    pub item_type: LineItemType,
    #[serde(default)]
    pub product_id: Option<ProductId>,
    #[serde(default)]
    pub addon_id: Option<ProductAddonId>,
    #[serde(default)]
    pub domain_name: Option<String>,
    #[serde(default)]
    pub billing_cycle: Option<BillingCycle>,
    #[serde(default)]
    pub years: Option<i16>,
    #[serde(default = "one")]
    pub quantity: i32,
    /// `{"<config_option_id>": {"value_id": "<uuid>", "quantity": n}}`.
    #[serde(default)]
    pub config_selections: Value,
}

fn one() -> i32 {
    1
}

impl QuoteItem {
    /// A product line.
    pub fn product(product_id: ProductId, cycle: BillingCycle) -> Self {
        Self {
            parent: None,
            item_type: LineItemType::Product,
            product_id: Some(product_id),
            addon_id: None,
            domain_name: None,
            billing_cycle: Some(cycle),
            years: None,
            quantity: 1,
            config_selections: Value::Object(serde_json::Map::new()),
        }
    }

    /// A domain line (`DomainRegister`, `DomainTransfer`, `DomainRenew`).
    pub fn domain(item_type: LineItemType, name: &str, years: i16) -> Self {
        Self {
            parent: None,
            item_type,
            product_id: None,
            addon_id: None,
            domain_name: Some(name.trim().to_ascii_lowercase()),
            billing_cycle: None,
            years: Some(years),
            quantity: 1,
            config_selections: Value::Object(serde_json::Map::new()),
        }
    }
}

/// What to quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteRequest {
    pub currency: Currency,
    pub items: Vec<QuoteItem>,
    #[serde(default)]
    pub coupon_code: Option<String>,
    /// Logged-in client (tax country, exemption, coupon rules).
    #[serde(default)]
    pub client_id: Option<ClientId>,
    /// Guest billing country (ISO alpha-2) when there is no client.
    #[serde(default)]
    pub country_code: Option<String>,
    /// Guest billing region.
    #[serde(default)]
    pub region: Option<String>,
}

/// Taxation context of a buyer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxContext {
    pub country_code: String,
    pub region: Option<String>,
    pub exempt: bool,
}

impl TaxContext {
    /// From a client profile and the tenant settings (B2B exemption: a
    /// corporate client with a tax number outside the tenant's country when
    /// `billing.b2b_tax_exempt` is on).
    pub fn for_client(client: &ClientRow, settings: &BillingSettings) -> Self {
        let country = client.country_code.trim().to_ascii_uppercase();
        let b2b = settings.b2b_tax_exempt
            && client.kind == ClientKind::Corporate
            && client
                .tax_number
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty())
            && country != settings.tax_home_country;
        Self {
            country_code: country,
            region: client.state.clone().filter(|s| !s.trim().is_empty()),
            exempt: client.tax_exempt || b2b,
        }
    }
}

/// Applicable tax rates for a buyer (empty when exempt).
pub async fn tax_rates(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    ctx: &TaxContext,
) -> BillingResult<Vec<TaxRate>> {
    if ctx.exempt {
        return Ok(Vec::new());
    }
    let mut rules = catalog::applicable_tax_rules(
        &mut *conn,
        tenant_id,
        &ctx.country_code,
        ctx.region.as_deref(),
    )
    .await?;
    rules.sort_by_key(|r| (r.level, -r.priority));
    Ok(rules.iter().map(TaxRate::from).collect())
}

/// Positions of the request's items in the quote: `result[line] = item index`.
pub(crate) fn line_order(req: &QuoteRequest) -> Result<Vec<usize>, CoreError> {
    let parents: Vec<Option<usize>> = req.items.iter().map(|i| i.parent).collect();
    parent_first_order(&parents)
}

/// Orders items so that every child directly follows its parent; returns the
/// new order as indices into `parents` (`parents[i]` = parent index of item i).
fn parent_first_order(parents: &[Option<usize>]) -> Result<Vec<usize>, CoreError> {
    let mut order = Vec::with_capacity(parents.len());
    for (i, p) in parents.iter().enumerate() {
        match p {
            None => {
                order.push(i);
                order.extend(
                    parents
                        .iter()
                        .enumerate()
                        .filter(|(_, pp)| **pp == Some(i))
                        .map(|(j, _)| j),
                );
            }
            Some(p) if *p >= parents.len() || parents[*p].is_some() => {
                return Err(CoreError::validation(format!(
                    "item {i}: invalid parent item"
                )));
            }
            Some(_) => {}
        }
    }
    Ok(order)
}

struct Resolver<'a> {
    tenant_id: TenantId,
    currency: Currency,
    locale: Locale,
    settings: &'a BillingSettings,
    products: HashMap<ProductId, catalog::ProductRow>,
}

impl Resolver<'_> {
    async fn product(
        &mut self,
        conn: &mut PgConnection,
        id: ProductId,
    ) -> BillingResult<catalog::ProductRow> {
        if let Some(p) = self.products.get(&id) {
            return Ok(p.clone());
        }
        let p = catalog::get_product(&mut *conn, self.tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("product", id))?;
        self.products.insert(id, p.clone());
        Ok(p)
    }

    async fn resolve(
        &mut self,
        conn: &mut PgConnection,
        item: &QuoteItem,
        parent_cycle: Option<BillingCycle>,
    ) -> BillingResult<PriceLine> {
        let cur = self.currency;
        match item.item_type {
            LineItemType::Product => {
                let pid = item
                    .product_id
                    .ok_or_else(|| CoreError::validation("product line without product"))?;
                let p = self.product(conn, pid).await?;
                if p.is_retired {
                    return Err(CoreError::validation(format!(
                        "{} is no longer sold",
                        p.name
                    )));
                }
                if item.quantity > 1 && !p.allow_quantity {
                    return Err(CoreError::validation(format!(
                        "{} cannot be ordered in quantity",
                        p.name
                    )));
                }
                let cycle = match p.pay_type {
                    PayType::OneTime => BillingCycle::OneTime,
                    PayType::Free => item.billing_cycle.unwrap_or(BillingCycle::OneTime),
                    PayType::Recurring => item.billing_cycle.ok_or_else(|| {
                        CoreError::validation(format!("{}: billing cycle is required", p.name))
                    })?,
                };
                let (mut unit, mut setup) = match p.pay_type {
                    PayType::Free => (Decimal::ZERO, Decimal::ZERO),
                    _ => {
                        let pr =
                            catalog::product_price(&mut *conn, self.tenant_id, pid, cur, cycle)
                                .await?
                                .ok_or_else(|| {
                                    CoreError::validation(format!(
                                        "{} is not available {} in {cur}",
                                        p.name,
                                        cycle_label(cycle, Locale::En).to_lowercase()
                                    ))
                                })?;
                        (pr.price, pr.setup_fee)
                    }
                };
                let charges = self
                    .config_charges(conn, pid, cycle, &item.config_selections)
                    .await?;
                for c in &charges {
                    let q = Decimal::from(c.quantity);
                    unit += c.price * q;
                    setup += c.setup_fee * q;
                }
                let qty = Decimal::from(item.quantity);
                let recurring = if cycle.is_recurring() {
                    unit
                } else {
                    Decimal::ZERO
                };
                Ok(PriceLine {
                    parent: item.parent,
                    item_type: LineItemType::Product,
                    product_id: Some(pid),
                    addon_id: None,
                    domain_name: item.domain_name.clone(),
                    tld: None,
                    description: format!("{} ({})", p.name, cycle_label(cycle, self.locale)),
                    billing_cycle: Some(cycle),
                    years: None,
                    quantity: item.quantity,
                    unit_price: unit,
                    recurring_unit_price: recurring,
                    setup_fee: setup * qty,
                    taxable: p.is_taxable,
                    config_options: charges,
                })
            }
            LineItemType::Addon => {
                let aid = item
                    .addon_id
                    .ok_or_else(|| CoreError::validation("addon line without addon"))?;
                let a = catalog::get_addon(&mut *conn, self.tenant_id, aid)
                    .await?
                    .ok_or_else(|| CoreError::not_found("addon", aid))?;
                let cycle = match a.pay_type {
                    PayType::OneTime => BillingCycle::OneTime,
                    _ => item
                        .billing_cycle
                        .or(parent_cycle)
                        .unwrap_or(BillingCycle::Monthly),
                };
                let (price, setup) = match a.pay_type {
                    PayType::Free => (Decimal::ZERO, Decimal::ZERO),
                    _ => catalog::addon_price(&mut *conn, self.tenant_id, aid, cur, cycle)
                        .await?
                        .ok_or_else(|| {
                            CoreError::validation(format!(
                                "addon {} is not available in {cur}",
                                a.name
                            ))
                        })?,
                };
                Ok(PriceLine {
                    parent: item.parent,
                    item_type: LineItemType::Addon,
                    product_id: None,
                    addon_id: Some(aid),
                    domain_name: None,
                    tld: None,
                    description: format!("{} ({})", a.name, cycle_label(cycle, self.locale)),
                    billing_cycle: Some(cycle),
                    years: None,
                    quantity: item.quantity,
                    unit_price: price,
                    recurring_unit_price: if cycle.is_recurring() {
                        price
                    } else {
                        Decimal::ZERO
                    },
                    setup_fee: setup * Decimal::from(item.quantity),
                    taxable: a.is_taxable,
                    config_options: Vec::new(),
                })
            }
            LineItemType::DomainRegister
            | LineItemType::DomainTransfer
            | LineItemType::DomainRenew => {
                let name = item
                    .domain_name
                    .as_deref()
                    .map(|n| n.trim().trim_end_matches('.').to_ascii_lowercase())
                    .filter(|n| n.contains('.'))
                    .ok_or_else(|| {
                        CoreError::validation("domain line without a valid domain name")
                    })?;
                let tld = domains::match_tld(&mut *conn, self.tenant_id, &name)
                    .await?
                    .ok_or_else(|| {
                        CoreError::validation(format!("the extension of {name} is not offered"))
                    })?;
                let years = item.years.unwrap_or(tld.min_years);
                if years < tld.min_years || years > tld.max_years {
                    return Err(CoreError::validation(format!(
                        ".{} can be registered for {}–{} years",
                        tld.tld, tld.min_years, tld.max_years
                    )));
                }
                let action = match item.item_type {
                    LineItemType::DomainTransfer => TldAction::Transfer,
                    LineItemType::DomainRenew => TldAction::Renew,
                    _ => TldAction::Register,
                };
                let price = self
                    .tld_price(conn, tld.id, action, years)
                    .await?
                    .ok_or_else(|| {
                        CoreError::validation(format!(
                            ".{} has no {action} price in {cur}",
                            tld.tld
                        ))
                    })?;
                let renew = self
                    .tld_price(conn, tld.id, TldAction::Renew, years)
                    .await?
                    .unwrap_or(price);
                Ok(PriceLine {
                    parent: item.parent,
                    item_type: item.item_type,
                    product_id: None,
                    addon_id: None,
                    domain_name: Some(name.clone()),
                    tld: Some(tld.tld.clone()),
                    description: domain_label(item.item_type, &name, years, self.locale),
                    billing_cycle: None,
                    years: Some(years),
                    quantity: 1,
                    unit_price: price,
                    recurring_unit_price: renew,
                    setup_fee: Decimal::ZERO,
                    taxable: self.settings.tax_domains,
                    config_options: Vec::new(),
                })
            }
            LineItemType::Other => Err(CoreError::validation(
                "custom lines cannot be priced from the catalog",
            )),
        }
    }

    /// TLD price for `years`, else the 1-year price × years.
    async fn tld_price(
        &self,
        conn: &mut PgConnection,
        tld_id: hos_core::ids::TldId,
        action: TldAction,
        years: i16,
    ) -> BillingResult<Option<Decimal>> {
        if let Some(p) = domains::tld_price(
            &mut *conn,
            self.tenant_id,
            tld_id,
            self.currency,
            action,
            years,
        )
        .await?
        {
            return Ok(Some(p));
        }
        if years > 1
            && let Some(p) =
                domains::tld_price(&mut *conn, self.tenant_id, tld_id, self.currency, action, 1)
                    .await?
        {
            return Ok(Some(p * Decimal::from(years)));
        }
        Ok(None)
    }

    async fn config_charges(
        &self,
        conn: &mut PgConnection,
        product_id: ProductId,
        cycle: BillingCycle,
        selections: &Value,
    ) -> BillingResult<Vec<ConfigOptionCharge>> {
        let Some(map) = selections.as_object().filter(|m| !m.is_empty()) else {
            return Ok(Vec::new());
        };
        let options =
            catalog::product_config_options(&mut *conn, self.tenant_id, product_id, self.currency)
                .await?;
        let mut out = Vec::with_capacity(map.len());
        for (option_key, sel) in map {
            let option = options
                .iter()
                .find(|o| o.option.id.to_string() == *option_key)
                .ok_or_else(|| {
                    CoreError::validation(format!("unknown configurable option {option_key}"))
                })?;
            let value_id = sel.get("value_id").and_then(Value::as_str).ok_or_else(|| {
                CoreError::validation(format!("{}: a value is required", option.option.name))
            })?;
            let value = option
                .values
                .iter()
                .find(|v| v.id.to_string() == value_id)
                .ok_or_else(|| {
                    CoreError::validation(format!("{}: unknown value", option.option.name))
                })?;
            let quantity = match option.option.option_type {
                ConfigOptionType::Quantity => {
                    let q = sel
                        .get("quantity")
                        .and_then(Value::as_i64)
                        .and_then(|q| i32::try_from(q).ok())
                        .unwrap_or(option.option.min_quantity);
                    let max = option.option.max_quantity;
                    if q < option.option.min_quantity || (max > 0 && q > max) {
                        return Err(CoreError::validation(format!(
                            "{}: quantity must be between {} and {}",
                            option.option.name, option.option.min_quantity, max
                        )));
                    }
                    q
                }
                _ => 1,
            };
            let tag = value.prices.iter().find(|t| t.billing_cycle == cycle);
            out.push(ConfigOptionCharge {
                option_id: option.option.id,
                option_name: option.option.name.clone(),
                value_id: value.id,
                value_label: value.label.clone(),
                value: value.value.clone(),
                module_key: option.option.module_key.clone(),
                quantity,
                price: tag.map_or(Decimal::ZERO, |t| t.price),
                setup_fee: tag.map_or(Decimal::ZERO, |t| t.setup_fee),
            });
        }
        out.sort_by(|a, b| a.option_name.cmp(&b.option_name));
        Ok(out)
    }
}

/// Coupon validity for a client at `now` (dates, limits, client rules).
pub(crate) async fn check_coupon(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    code: &str,
    client_id: Option<ClientId>,
    now: DateTime<Utc>,
) -> BillingResult<Result<CouponRow, String>> {
    let Some(c) = catalog::redeemable_coupon(&mut *conn, tenant_id, code.trim()).await? else {
        return Ok(Err(format!("coupon {code} is invalid or expired")));
    };
    if c.starts_at.is_some_and(|s| s > now) || c.expires_at.is_some_and(|e| e <= now) {
        return Ok(Err(format!("coupon {code} is invalid or expired")));
    }
    if let Some(client) = client_id {
        if c.new_clients_only
            && billing_support::client_has_orders(&mut *conn, tenant_id, client).await?
        {
            return Ok(Err(format!("coupon {code} is for new clients only")));
        }
        if c.once_per_client
            && billing_support::client_used_coupon(&mut *conn, tenant_id, client, c.id).await?
        {
            return Ok(Err(format!("coupon {code} was already used")));
        }
    }
    Ok(Ok(c))
}

impl Billing {
    /// Prices arbitrary items for a buyer (storefront "configure" page, API).
    pub async fn quote(&self, tenant_id: TenantId, req: &QuoteRequest) -> BillingResult<Quote> {
        let mut conn = self.pool().acquire().await?;
        self.quote_on(&mut conn, tenant_id, req).await
    }

    /// Prices a cart (its items, coupon and currency) for `client_id` (or the
    /// cart's client, or a guest in `country_code`).
    pub async fn quote_cart(
        &self,
        tenant_id: TenantId,
        cart_id: hos_core::ids::CartId,
        client_id: Option<ClientId>,
        country_code: Option<&str>,
    ) -> BillingResult<Quote> {
        let mut conn = self.pool().acquire().await?;
        let req = cart_request(&mut conn, tenant_id, cart_id, client_id, country_code).await?;
        self.quote_on(&mut conn, tenant_id, &req).await
    }

    /// [`Billing::quote`] on a given connection / transaction.
    pub async fn quote_on(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        req: &QuoteRequest,
    ) -> BillingResult<Quote> {
        let settings = BillingSettings::load(&mut *conn, tenant_id).await?;
        let client = match req.client_id {
            Some(id) => Some(
                clients::get_client(&mut *conn, tenant_id, id)
                    .await?
                    .ok_or_else(|| CoreError::not_found("client", id))?,
            ),
            None => None,
        };
        let locale = match &client {
            Some(c) => c.locale,
            None => self.tenant(tenant_id).await?.default_locale,
        };
        let tax_ctx = match &client {
            Some(c) => TaxContext::for_client(c, &settings),
            None => TaxContext {
                country_code: req
                    .country_code
                    .as_deref()
                    .map(|c| c.trim().to_ascii_uppercase())
                    .filter(|c| c.len() == 2)
                    .unwrap_or_else(|| settings.tax_home_country.clone()),
                region: req.region.clone(),
                exempt: false,
            },
        };
        let taxes = tax_rates(conn, tenant_id, &tax_ctx).await?;

        let parents: Vec<Option<usize>> = req.items.iter().map(|i| i.parent).collect();
        let order = parent_first_order(&parents)?;
        let mut new_index = vec![0usize; req.items.len()];
        for (pos, old) in order.iter().enumerate() {
            new_index[*old] = pos;
        }
        let mut resolver = Resolver {
            tenant_id,
            currency: req.currency,
            locale,
            settings: &settings,
            products: HashMap::new(),
        };
        let mut lines: Vec<PriceLine> = Vec::with_capacity(order.len());
        for old in &order {
            let mut item = req.items[*old].clone();
            item.parent = item.parent.map(|p| new_index[p]);
            let parent_cycle = item
                .parent
                .and_then(|p| lines.get(p))
                .and_then(|l: &PriceLine| l.billing_cycle);
            lines.push(resolver.resolve(conn, &item, parent_cycle).await?);
        }

        let (coupon, coupon_error) = match req.coupon_code.as_deref().map(str::trim) {
            Some(code) if !code.is_empty() => {
                match check_coupon(conn, tenant_id, code, req.client_id, self.clock().now()).await?
                {
                    Ok(c) => {
                        let mut spec = CouponSpec::from(&c);
                        if c.requires_all_products {
                            spec.bundle_items =
                                hos_db::repo::bundles::coupon_items(&mut *conn, tenant_id, c.id)
                                    .await?;
                        }
                        (Some(spec), None)
                    }
                    Err(e) => (None, Some(e)),
                }
            }
            _ => (None, None),
        };
        let mut quote = price(&PricingInput {
            currency: req.currency,
            lines,
            coupon,
            taxes,
            prices_include_tax: settings.prices_include_tax,
        })?;
        if quote.coupon_error.is_none() {
            quote.coupon_error = coupon_error;
        }
        Ok(quote)
    }
}

/// Builds a [`QuoteRequest`] from a cart.
pub(crate) async fn cart_request(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    cart_id: hos_core::ids::CartId,
    client_id: Option<ClientId>,
    country_code: Option<&str>,
) -> BillingResult<QuoteRequest> {
    let cart = carts::get_cart(&mut *conn, tenant_id, cart_id)
        .await?
        .ok_or_else(|| CoreError::not_found("cart", cart_id))?;
    let items = carts::cart_items(&mut *conn, tenant_id, cart_id).await?;
    let index: HashMap<_, _> = items.iter().enumerate().map(|(i, it)| (it.id, i)).collect();
    let req_items = items
        .iter()
        .map(|it| QuoteItem {
            parent: it.parent_item_id.and_then(|p| index.get(&p).copied()),
            item_type: it.item_type,
            product_id: it.product_id,
            addon_id: it.addon_id,
            domain_name: it.domain_name.clone(),
            billing_cycle: it.billing_cycle,
            years: it.years,
            quantity: it.quantity,
            config_selections: it.config_selections.clone(),
        })
        .collect();
    Ok(QuoteRequest {
        currency: cart.currency,
        items: req_items,
        coupon_code: cart.coupon_code.clone(),
        client_id: client_id.or(cart.client_id),
        country_code: country_code.map(str::to_owned),
        region: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rust_decimal_macros::dec;

    fn kdv() -> Vec<TaxRate> {
        vec![TaxRate {
            tax_rule_id: None,
            name: "KDV".into(),
            rate: dec!(20),
            is_compound: false,
        }]
    }

    fn input(lines: Vec<PriceLine>, taxes: Vec<TaxRate>, inclusive: bool) -> PricingInput {
        PricingInput {
            currency: Currency::TRY,
            lines,
            coupon: None,
            taxes,
            prices_include_tax: inclusive,
        }
    }

    #[test]
    fn tax_exclusive() {
        let p = ProductId::new();
        let mut line = PriceLine::product(p, BillingCycle::Monthly, dec!(100));
        line.setup_fee = dec!(10);
        let q = price(&input(vec![line], kdv(), false)).unwrap();
        assert_eq!(q.subtotal, dec!(110));
        assert_eq!(q.setup_total, dec!(10));
        assert_eq!(q.tax_total, dec!(22));
        assert_eq!(q.total, dec!(132));
        assert_eq!(q.lines[0].recurring_amount, dec!(100));
        assert_eq!(
            q.recurring,
            vec![RecurringTotal {
                months: 1,
                amount: dec!(100)
            }]
        );
    }

    #[test]
    fn tax_inclusive() {
        let p = ProductId::new();
        let q = price(&input(
            vec![PriceLine::product(p, BillingCycle::Annual, dec!(120))],
            kdv(),
            true,
        ))
        .unwrap();
        assert_eq!(q.lines[0].unit_price, dec!(100));
        assert_eq!(q.subtotal, dec!(100));
        assert_eq!(q.tax_total, dec!(20));
        assert_eq!(q.total, dec!(120));
        // A gross price that does not divide evenly stays within one cent.
        let q = price(&input(
            vec![PriceLine::product(p, BillingCycle::Monthly, dec!(99.99))],
            kdv(),
            true,
        ))
        .unwrap();
        assert_eq!(q.subtotal, dec!(83.33));
        assert_eq!(q.tax_total, dec!(16.67));
        assert_eq!(q.total, dec!(100.00));
        assert!((q.total - dec!(99.99)).abs() <= dec!(0.01));
        // Non-taxable lines are not grossed down.
        let mut free = PriceLine::product(p, BillingCycle::Monthly, dec!(50));
        free.taxable = false;
        let q = price(&input(vec![free], kdv(), true)).unwrap();
        assert_eq!(q.subtotal, dec!(50));
        assert_eq!(q.tax_total, dec!(0));
    }

    #[test]
    fn compound_tax_factor() {
        let taxes = vec![
            TaxRate {
                tax_rule_id: None,
                name: "A".into(),
                rate: dec!(10),
                is_compound: false,
            },
            TaxRate {
                tax_rule_id: None,
                name: "B".into(),
                rate: dec!(5),
                is_compound: true,
            },
        ];
        assert_eq!(tax_factor(&taxes), dec!(1.155));
        let q = price(&input(
            vec![PriceLine::product(
                ProductId::new(),
                BillingCycle::Monthly,
                dec!(200),
            )],
            taxes,
            false,
        ))
        .unwrap();
        assert_eq!(q.taxes[0].tax_amount, dec!(20));
        assert_eq!(q.taxes[1].taxable_amount, dec!(220));
        assert_eq!(q.taxes[1].tax_amount, dec!(11));
        assert_eq!(q.total, dec!(231));
    }

    #[test]
    fn exempt_has_no_tax() {
        let q = price(&input(
            vec![PriceLine::product(
                ProductId::new(),
                BillingCycle::Monthly,
                dec!(100),
            )],
            Vec::new(),
            false,
        ))
        .unwrap();
        assert_eq!(q.tax_total, dec!(0));
        assert_eq!(q.total, dec!(100));
    }

    #[test]
    fn rounding_is_per_line() {
        // 3 × 0.335 = 1.005 → 1.01 per line (half away from zero), twice.
        let p = ProductId::new();
        let mut a = PriceLine::product(p, BillingCycle::Monthly, dec!(0.335));
        a.quantity = 3;
        let b = a.clone();
        let q = price(&input(vec![a, b], kdv(), false)).unwrap();
        assert_eq!(q.lines[0].base_amount, dec!(1.01));
        assert_eq!(q.subtotal, dec!(2.02));
        // Tax on the rounded base: 2.02 × 20 % = 0.404 → 0.40.
        assert_eq!(q.tax_total, dec!(0.40));
        assert_eq!(q.total, dec!(2.42));
    }

    #[test]
    fn percent_coupon_first_period_only() {
        let p = ProductId::new();
        let mut line = PriceLine::product(p, BillingCycle::Monthly, dec!(100));
        line.setup_fee = dec!(50);
        let mut i = input(vec![line], kdv(), false);
        i.coupon = Some(CouponSpec::simple(
            "SAVE10",
            DiscountType::Percent,
            dec!(10),
        ));
        let q = price(&i).unwrap();
        assert_eq!(q.lines[0].discount, dec!(10));
        assert_eq!(q.discount_total, dec!(10));
        assert_eq!(q.subtotal, dec!(140));
        assert_eq!(q.tax_total, dec!(28));
        assert_eq!(q.lines[0].recurring_amount, dec!(100));
        assert_eq!(q.coupon.as_ref().unwrap().discount, dec!(10));

        // applies_to_setup + recurring
        let mut c = CouponSpec::simple("ALL", DiscountType::Percent, dec!(10));
        c.applies_to_setup = true;
        c.recurring = true;
        i.coupon = Some(c);
        let q = price(&i).unwrap();
        assert_eq!(q.lines[0].discount, dec!(15));
        assert_eq!(q.lines[0].recurring_amount, dec!(90));
        assert_eq!(q.lines[0].list_recurring_amount, dec!(100));
    }

    #[test]
    fn fixed_and_override_and_free_setup() {
        let p = ProductId::new();
        let mut line = PriceLine::product(p, BillingCycle::Monthly, dec!(30));
        line.setup_fee = dec!(20);
        let mut i = input(vec![line], Vec::new(), false);
        let mut fixed = CouponSpec::simple("F", DiscountType::Fixed, dec!(50));
        fixed.currency = Some(Currency::TRY);
        i.coupon = Some(fixed.clone());
        // capped at the base (setup not included)
        assert_eq!(price(&i).unwrap().lines[0].discount, dec!(30));
        fixed.applies_to_setup = true;
        i.coupon = Some(fixed.clone());
        assert_eq!(price(&i).unwrap().lines[0].discount, dec!(50));
        fixed.currency = Some(Currency::USD);
        i.coupon = Some(fixed);
        let q = price(&i).unwrap();
        assert!(q.coupon.is_none());
        assert!(q.coupon_error.is_some());

        let mut ov = CouponSpec::simple("O", DiscountType::PriceOverride, dec!(12.5));
        ov.currency = Some(Currency::TRY);
        i.coupon = Some(ov);
        assert_eq!(price(&i).unwrap().lines[0].discount, dec!(17.5));

        i.coupon = Some(CouponSpec::simple("S", DiscountType::FreeSetup, dec!(0)));
        let q = price(&i).unwrap();
        assert_eq!(q.lines[0].discount, dec!(20));
        assert_eq!(q.total, dec!(30));
    }

    #[test]
    fn coupon_restrictions() {
        let p1 = ProductId::new();
        let p2 = ProductId::new();
        let l1 = PriceLine::product(p1, BillingCycle::Monthly, dec!(10));
        let l2 = PriceLine::product(p2, BillingCycle::Annual, dec!(100));
        let mut i = input(vec![l1, l2], Vec::new(), false);
        let mut c = CouponSpec::simple("P2", DiscountType::Percent, dec!(50));
        c.product_ids = vec![p2];
        i.coupon = Some(c.clone());
        let q = price(&i).unwrap();
        assert_eq!(q.lines[0].discount, dec!(0));
        assert_eq!(q.lines[1].discount, dec!(50));

        c.product_ids.clear();
        c.billing_cycles = vec![BillingCycle::Monthly];
        i.coupon = Some(c.clone());
        let q = price(&i).unwrap();
        assert_eq!(q.lines[0].discount, dec!(5));
        assert_eq!(q.lines[1].discount, dec!(0));

        c.billing_cycles = vec![BillingCycle::Triennial];
        i.coupon = Some(c.clone());
        let q = price(&i).unwrap();
        assert!(q.coupon.is_none());
        assert!(q.coupon_error.unwrap().contains("does not apply"));

        c.billing_cycles.clear();
        c.min_order_amount = Some(dec!(1000));
        i.coupon = Some(c);
        assert!(price(&i).unwrap().coupon_error.unwrap().contains("minimum"));
    }

    #[test]
    fn bundle_coupon_needs_every_product() {
        let a = ProductId::new();
        let b = ProductId::new();
        let mut coupon = CouponSpec::simple("BUNDLE", DiscountType::Percent, dec!(10));
        coupon.product_ids = vec![a, b];
        coupon.requires_all_products = true;
        let input = |lines: Vec<PriceLine>| PricingInput {
            currency: Currency::TRY,
            lines,
            coupon: Some(coupon.clone()),
            taxes: Vec::new(),
            prices_include_tax: false,
        };
        let only_a = price(&input(vec![PriceLine::product(
            a,
            BillingCycle::Monthly,
            dec!(100),
        )]))
        .unwrap();
        assert!(only_a.coupon.is_none());
        assert!(only_a.coupon_error.unwrap().contains("bundle"));
        let both = price(&input(vec![
            PriceLine::product(a, BillingCycle::Monthly, dec!(100)),
            PriceLine::product(b, BillingCycle::Monthly, dec!(50)),
        ]))
        .unwrap();
        assert!(both.coupon_error.is_none());
        assert_eq!(both.discount_total, dec!(15));
    }

    #[test]
    fn bundle_discounts_one_unit_per_item_in_its_cycle() {
        let a = ProductId::new();
        let b = ProductId::new();
        let mut coupon = CouponSpec::simple("BUNDLE", DiscountType::Percent, dec!(10));
        coupon.product_ids = vec![a, b];
        coupon.requires_all_products = true;
        coupon.bundle_items = vec![(a, BillingCycle::Monthly), (b, BillingCycle::Annual)];
        let quote = |coupon: &CouponSpec, lines: Vec<PriceLine>| {
            price(&PricingInput {
                currency: Currency::TRY,
                lines,
                coupon: Some(coupon.clone()),
                taxes: Vec::new(),
                prices_include_tax: false,
            })
            .unwrap()
        };
        let mut many_a = PriceLine::product(a, BillingCycle::Monthly, dec!(100));
        many_a.quantity = 5;
        // Five units of A plus a second A line: only one unit of A and B.
        let q = quote(
            &coupon,
            vec![
                many_a,
                PriceLine::product(a, BillingCycle::Monthly, dec!(100)),
                PriceLine::product(b, BillingCycle::Annual, dec!(50)),
            ],
        );
        assert!(q.coupon_error.is_none());
        assert_eq!(
            q.discount_total,
            dec!(15),
            "10 % of one A (100) + one B (50)"
        );
        assert_eq!(q.lines[0].discount, dec!(10));
        assert_eq!(q.lines[1].discount, dec!(0));
        assert_eq!(q.lines[2].discount, dec!(5));
        // B in another cycle than the bundle's: the bundle is not complete.
        let q = quote(
            &coupon,
            vec![
                PriceLine::product(a, BillingCycle::Monthly, dec!(100)),
                PriceLine::product(b, BillingCycle::Monthly, dec!(5)),
            ],
        );
        assert!(q.coupon.is_none());
        assert!(q.coupon_error.unwrap().contains("bundle"));
        // A fixed discount larger than the price never makes a line negative.
        let mut fixed = coupon.clone();
        fixed.discount_type = DiscountType::Fixed;
        fixed.value = dec!(1000);
        fixed.currency = Some(Currency::TRY);
        let q = quote(
            &fixed,
            vec![
                PriceLine::product(a, BillingCycle::Monthly, dec!(100)),
                PriceLine::product(b, BillingCycle::Annual, dec!(50)),
            ],
        );
        assert!(q.lines.iter().all(|l| l.amount >= Decimal::ZERO));
        assert!(q.total >= Decimal::ZERO);
        assert_eq!(q.total, dec!(0));
    }

    #[test]
    fn domain_coupon_by_tld() {
        let mut d = PriceLine::product(ProductId::new(), BillingCycle::Annual, dec!(10));
        d.item_type = LineItemType::DomainRegister;
        d.product_id = None;
        d.billing_cycle = None;
        d.years = Some(2);
        d.tld = Some("com".into());
        let mut i = input(vec![d], Vec::new(), false);
        let mut c = CouponSpec::simple("COM", DiscountType::Percent, dec!(100));
        c.tlds = vec![".com".into()];
        i.coupon = Some(c);
        let q = price(&i).unwrap();
        assert_eq!(q.total, dec!(0));
        assert_eq!(
            q.recurring,
            vec![RecurringTotal {
                months: 24,
                amount: dec!(10)
            }]
        );
    }

    #[test]
    fn invalid_input() {
        let mut l = PriceLine::product(ProductId::new(), BillingCycle::Monthly, dec!(1));
        l.quantity = 0;
        assert!(price(&input(vec![l.clone()], Vec::new(), false)).is_err());
        l.quantity = 1;
        l.parent = Some(0);
        assert!(price(&input(vec![l], Vec::new(), false)).is_err());
    }

    #[test]
    fn proration() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        // 30-day period, 15 days left, 100 → 200: credit 50, charge 100.
        let p = prorate(
            dec!(100),
            dec!(200),
            d(4, 1),
            d(5, 1),
            d(4, 16),
            Currency::TRY,
        )
        .unwrap();
        assert_eq!(p.period_days, 30);
        assert_eq!(p.remaining_days, 15);
        assert_eq!(p.credit, dec!(50));
        assert_eq!(p.charge, dec!(100));
        assert_eq!(p.amount, dec!(50));
        // Downgrade gives a negative amount; rounding per side.
        let p = prorate(
            dec!(100),
            dec!(10),
            d(4, 1),
            d(5, 1),
            d(4, 21),
            Currency::TRY,
        )
        .unwrap();
        assert_eq!(p.credit, dec!(33.33));
        assert_eq!(p.charge, dec!(3.33));
        assert_eq!(p.amount, dec!(-30));
        // After the period nothing is left.
        let p = prorate(
            dec!(100),
            dec!(200),
            d(4, 1),
            d(5, 1),
            d(6, 1),
            Currency::TRY,
        )
        .unwrap();
        assert_eq!(p.amount, dec!(0));
        assert!(prorate(dec!(1), dec!(1), d(4, 1), d(4, 1), d(4, 1), Currency::TRY).is_err());
    }

    #[test]
    fn first_charge_until_the_due_date() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let c = |today, end| charge_until(dec!(30), 1, today, end, Currency::TRY).unwrap();
        // Due today or overdue: one full period.
        assert_eq!(c(d(4, 16), d(4, 16)), dec!(30));
        assert_eq!(c(d(4, 16), d(3, 1)), dec!(30));
        // One day left of a 30-day period.
        assert_eq!(c(d(4, 30), d(5, 1)), dec!(1));
        // Exactly one period left: the full price; half of it: half.
        assert_eq!(c(d(4, 1), d(5, 1)), dec!(30));
        assert_eq!(c(d(4, 16), d(5, 1)), dec!(15));
        // Paid ahead: every whole period counts, plus the running part.
        assert_eq!(c(d(4, 1), d(7, 1)), dec!(90));
        assert_eq!(c(d(4, 16), d(7, 1)), dec!(75));
        assert_eq!(c(d(4, 30), d(7, 1)), dec!(61));
        // Annual cycle, three years ahead, starting at a period start.
        let y = |year| NaiveDate::from_ymd_opt(year, 4, 1).unwrap();
        assert_eq!(
            charge_until(dec!(100), 12, y(2026), y(2029), Currency::TRY).unwrap(),
            dec!(300)
        );
        // Absurdly far ahead is refused, not looped over.
        assert!(
            charge_until(
                dec!(1),
                1,
                d(4, 1),
                NaiveDate::from_ymd_opt(2200, 1, 1).unwrap(),
                Currency::TRY
            )
            .is_err()
        );
    }

    #[test]
    fn parent_ordering() {
        assert_eq!(
            parent_first_order(&[Some(2), None, None, Some(1)]).unwrap(),
            vec![1, 3, 2, 0]
        );
        assert!(parent_first_order(&[Some(5)]).is_err());
        assert!(parent_first_order(&[None, Some(0), Some(1)]).is_err());
    }
}
