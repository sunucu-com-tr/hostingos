//! HostingOS invoice → e-document lines, VAT subtotals and totals, reconciled
//! to the kuruş with the invoice stored by `hos-billing`.
//!
//! `hos-billing` rounds every line, then computes each tax once on the
//! taxable base (`hos_db::repo::invoices::compute_totals`). UBL-TR needs VAT
//! **per line** as well, and GİB rejects negative lines, so:
//!
//! 1. Negative lines (discounts, credits for downgrades) are folded into the
//!    positive lines of the same taxable group as line allowances,
//!    proportionally, with the largest-remainder method at the currency's
//!    minor unit. The sum of line nets therefore equals the invoice subtotal.
//! 2. The invoice's VAT amount (not a recomputation) is distributed over the
//!    taxable lines with the largest-remainder method, so the line VATs add up
//!    to exactly `invoice.tax_total`.
//! 3. Non-taxable lines (and fully discounted lines) are 0 % lines that carry
//!    the configured exemption (`TaxExemptionReasonCode`), as GİB requires for
//!    every zero VAT amount.
//!
//! Anything that cannot be mapped faithfully (several or compound tax rates,
//! totals that do not add up) is an error for staff, never a silently
//! different document.

use hos_core::integrations::einvoice::{InvoiceTypeCode, Line, TaxExemption, Totals, VatSubtotal};
use hos_core::money::Currency;
use rust_decimal::{Decimal, RoundingStrategy};

use crate::codes::{ExemptionClass, exemption_class};

/// A stored invoice line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLine {
    /// Description (already cleaned for printing).
    pub description: String,
    /// Quantity.
    pub quantity: Decimal,
    /// Unit price.
    pub unit_price: Decimal,
    /// Rounded line amount (negative for discounts).
    pub amount: Decimal,
    /// Whether the invoice's tax applies.
    pub taxable: bool,
}

/// A stored tax line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTax {
    /// Percent.
    pub rate: Decimal,
    /// Compound (tax on tax).
    pub compound: bool,
    /// Taxable base the invoice used.
    pub taxable_amount: Decimal,
    /// Rounded tax amount.
    pub amount: Decimal,
}

/// A stored invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInvoice {
    /// Currency.
    pub currency: Currency,
    /// Lines in display order.
    pub lines: Vec<SourceLine>,
    /// Tax lines.
    pub taxes: Vec<SourceTax>,
    /// `invoice.subtotal`.
    pub subtotal: Decimal,
    /// `invoice.tax_total`.
    pub tax_total: Decimal,
    /// `invoice.total`.
    pub total: Decimal,
}

/// Lines, subtotals, totals and the resulting invoice type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    /// Document lines.
    pub lines: Vec<Line>,
    /// VAT subtotals per rate.
    pub vat: Vec<VatSubtotal>,
    /// Totals.
    pub totals: Totals,
    /// `SATIS` or `ISTISNA` (from the exemption code).
    pub type_code: InvoiceTypeCode,
}

/// Why an invoice cannot be turned into an e-document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComposeError {
    /// More than one tax rate or a compound tax.
    #[error("the invoice has several or compound tax rates; e-documents support one KDV rate")]
    UnsupportedTaxes,
    /// Nothing positive to invoice.
    #[error("the invoice has no chargeable lines")]
    Empty,
    /// Zero-VAT lines without an exemption code.
    #[error("the invoice has lines without KDV; set a default VAT exemption code")]
    ExemptionRequired,
    /// Exemption code not usable by HostingOS.
    #[error("VAT exemption code {0} cannot be used for these invoices")]
    BadExemption(String),
    /// Discounts larger than the lines they apply to.
    #[error("discounts exceed the lines they apply to")]
    DiscountTooLarge,
    /// The stored totals do not add up.
    #[error("invoice totals do not add up ({0})")]
    Inconsistent(&'static str),
    /// A taxable line would carry 0 KDV after rounding.
    #[error("a line is too small to carry KDV after rounding")]
    VatRoundsToZero,
}

fn unit(currency: Currency) -> Decimal {
    Decimal::new(1, currency.minor_units())
}

fn round(d: Decimal, currency: Currency) -> Decimal {
    d.round_dp_with_strategy(
        currency.minor_units(),
        RoundingStrategy::MidpointAwayFromZero,
    )
}

/// Splits `total` (a multiple of `unit`) over `weights` proportionally;
/// shares are multiples of `unit` and add up to `total` exactly.
fn largest_remainder(total: Decimal, weights: &[Decimal], unit: Decimal) -> Option<Vec<Decimal>> {
    let sum: Decimal = weights.iter().copied().sum();
    if weights.is_empty() || sum <= Decimal::ZERO {
        return (total.is_zero()).then(|| vec![Decimal::ZERO; weights.len()]);
    }
    let mut shares = Vec::with_capacity(weights.len());
    let mut fractions = Vec::with_capacity(weights.len());
    for (i, w) in weights.iter().enumerate() {
        let exact = total * *w / sum / unit;
        let floor = exact.floor();
        shares.push(floor * unit);
        fractions.push((exact - floor, i));
    }
    let assigned: Decimal = shares.iter().copied().sum();
    let left = ((total - assigned) / unit).round();
    let left = usize::try_from(left.mantissa())
        .ok()
        .filter(|_| left.scale() == 0)?;
    if left > weights.len() {
        return None;
    }
    // Largest fraction first; ties go to the earlier line.
    fractions.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, i) in fractions.into_iter().take(left) {
        shares[i] += unit;
    }
    Some(shares)
}

/// Unit price that reproduces `gross` for `quantity` (8 decimals at most);
/// falls back to quantity 1 when no such price exists.
fn line_price(
    quantity: Decimal,
    unit_price: Decimal,
    gross: Decimal,
    currency: Currency,
) -> (Decimal, Decimal) {
    if quantity > Decimal::ZERO && round(quantity * unit_price, currency) == gross {
        return (quantity, unit_price.normalize());
    }
    if quantity > Decimal::ZERO {
        let p = (gross / quantity).round_dp(8);
        if round(quantity * p, currency) == gross {
            return (quantity, p.normalize());
        }
    }
    (Decimal::ONE, gross)
}

/// Builds the e-document body of `src`. `exemption` is the tenant's default
/// exemption for 0 % lines.
pub fn compose(
    src: &SourceInvoice,
    exemption: Option<&TaxExemption>,
) -> Result<Composed, ComposeError> {
    let cur = src.currency;
    let unit = unit(cur);
    let (rate, tax) = match src.taxes.as_slice() {
        [] => (Decimal::ZERO, None),
        [t] if !t.compound => (t.rate, Some(t)),
        _ => return Err(ComposeError::UnsupportedTaxes),
    };
    if src.lines.iter().map(|l| l.amount).sum::<Decimal>() != src.subtotal {
        return Err(ComposeError::Inconsistent("lines vs subtotal"));
    }
    if src.subtotal + src.tax_total != src.total {
        return Err(ComposeError::Inconsistent("subtotal + tax vs total"));
    }
    if tax.is_none() && !src.tax_total.is_zero() {
        return Err(ComposeError::Inconsistent("tax total without tax lines"));
    }

    // 1. Fold negative lines into positive lines of the same group.
    let taxed = |l: &SourceLine| l.taxable && rate > Decimal::ZERO;
    let mut allowance = vec![Decimal::ZERO; src.lines.len()];
    for group in [true, false] {
        let members: Vec<usize> = (0..src.lines.len())
            .filter(|&i| taxed(&src.lines[i]) == group)
            .collect();
        let negative: Decimal = -members
            .iter()
            .map(|&i| src.lines[i].amount)
            .filter(|a| *a < Decimal::ZERO)
            .sum::<Decimal>();
        if negative.is_zero() {
            continue;
        }
        let positive: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&i| src.lines[i].amount > Decimal::ZERO)
            .collect();
        let weights: Vec<Decimal> = positive.iter().map(|&i| src.lines[i].amount).collect();
        if negative > weights.iter().copied().sum::<Decimal>() {
            return Err(ComposeError::DiscountTooLarge);
        }
        let shares =
            largest_remainder(negative, &weights, unit).ok_or(ComposeError::DiscountTooLarge)?;
        for (&i, share) in positive.iter().zip(shares) {
            allowance[i] = share;
        }
    }

    // Lines that stay on the document (negative lines are folded away).
    let kept: Vec<usize> = (0..src.lines.len())
        .filter(|&i| src.lines[i].amount >= Decimal::ZERO)
        .collect();
    if kept.is_empty() || src.subtotal <= Decimal::ZERO {
        return Err(ComposeError::Empty);
    }
    let net = |i: usize| src.lines[i].amount - allowance[i];

    // 2. Distribute the stored VAT over taxed lines with a positive net.
    let vat_lines: Vec<usize> = kept
        .iter()
        .copied()
        .filter(|&i| taxed(&src.lines[i]) && net(i) > Decimal::ZERO)
        .collect();
    let base: Decimal = vat_lines.iter().map(|&i| net(i)).sum();
    if let Some(t) = tax {
        if t.taxable_amount != base {
            return Err(ComposeError::Inconsistent("taxable base"));
        }
        if t.amount != src.tax_total {
            return Err(ComposeError::Inconsistent("tax line vs tax total"));
        }
    }
    let weights: Vec<Decimal> = vat_lines.iter().map(|&i| net(i)).collect();
    let vat_shares = largest_remainder(src.tax_total, &weights, unit)
        .ok_or(ComposeError::Inconsistent("tax distribution"))?;
    let mut vat = vec![Decimal::ZERO; src.lines.len()];
    for (&i, share) in vat_lines.iter().zip(vat_shares) {
        if share.is_zero() {
            return Err(ComposeError::VatRoundsToZero);
        }
        vat[i] = share;
    }

    // 3. Lines.
    let mut lines = Vec::with_capacity(kept.len());
    let mut zero_base = Decimal::ZERO;
    let mut needs_exemption = false;
    for &i in &kept {
        let l = &src.lines[i];
        let with_vat = vat_lines.contains(&i);
        let (quantity, unit_price) = line_price(l.quantity, l.unit_price, l.amount, cur);
        let line_rate = if with_vat { rate } else { Decimal::ZERO };
        if !with_vat {
            needs_exemption = true;
            zero_base += net(i);
        }
        lines.push(Line {
            name: l.description.clone(),
            quantity,
            unit_code: "C62".into(),
            unit_price,
            allowance: allowance[i],
            line_extension: net(i),
            vat_rate: line_rate,
            vat_amount: vat[i],
            exemption: if with_vat { None } else { exemption.cloned() },
            withholding: None,
        });
    }

    let mut type_code = InvoiceTypeCode::Satis;
    if needs_exemption {
        let ex = exemption.ok_or(ComposeError::ExemptionRequired)?;
        match exemption_class(&ex.code) {
            ExemptionClass::Satis => {}
            ExemptionClass::Istisna => type_code = InvoiceTypeCode::Istisna,
            _ => return Err(ComposeError::BadExemption(ex.code.clone())),
        }
        if ex.reason.trim().is_empty() {
            return Err(ComposeError::ExemptionRequired);
        }
    }

    let mut subtotals = Vec::new();
    if !vat_lines.is_empty() {
        subtotals.push(VatSubtotal {
            rate,
            taxable: base,
            amount: src.tax_total,
            exemption: None,
        });
    }
    if needs_exemption {
        subtotals.push(VatSubtotal {
            rate: Decimal::ZERO,
            taxable: zero_base,
            amount: Decimal::ZERO,
            exemption: exemption.cloned(),
        });
    }

    let line_extension: Decimal = lines.iter().map(|l| l.line_extension).sum();
    if line_extension != src.subtotal {
        return Err(ComposeError::Inconsistent("line nets vs subtotal"));
    }
    let totals = Totals {
        line_extension,
        allowance_total: lines.iter().map(|l| l.allowance).sum(),
        tax_exclusive: line_extension,
        vat_total: src.tax_total,
        withholding_total: Decimal::ZERO,
        tax_inclusive: line_extension + src.tax_total,
        payable: line_extension + src.tax_total,
    };
    Ok(Composed {
        lines,
        vat: subtotals,
        totals,
        type_code,
    })
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn line(amount: Decimal, taxable: bool) -> SourceLine {
        SourceLine {
            description: "x".into(),
            quantity: Decimal::ONE,
            unit_price: amount,
            amount,
            taxable,
        }
    }

    fn try_src(lines: Vec<SourceLine>, rate: Decimal) -> SourceInvoice {
        let cur = Currency::TRY;
        let subtotal: Decimal = lines.iter().map(|l| l.amount).sum();
        let base: Decimal = lines.iter().filter(|l| l.taxable).map(|l| l.amount).sum();
        let tax = round(base * rate / dec!(100), cur);
        SourceInvoice {
            currency: cur,
            lines,
            taxes: if rate.is_zero() {
                vec![]
            } else {
                vec![SourceTax {
                    rate,
                    compound: false,
                    taxable_amount: base,
                    amount: tax,
                }]
            },
            subtotal,
            tax_total: tax,
            total: subtotal + tax,
        }
    }

    fn exemption(code: &str) -> TaxExemption {
        TaxExemption {
            code: code.into(),
            reason: "KDV İstisna Olmayan Diğer".into(),
        }
    }

    #[test]
    fn per_line_vat_adds_up_to_the_invoice() {
        // 3 × 33.33 at 20 %: per-line rounding would give 3 × 6.67 = 20.01,
        // the invoice says round(99.99 × 0.2) = 20.00.
        let src = try_src(vec![line(dec!(33.33), true); 3], dec!(20));
        assert_eq!(src.tax_total, dec!(20.00));
        let c = compose(&src, None).unwrap();
        let sum: Decimal = c.lines.iter().map(|l| l.vat_amount).sum();
        assert_eq!(sum, dec!(20.00));
        assert_eq!(c.totals.payable, src.total);
        assert_eq!(c.vat.len(), 1);
        assert_eq!(c.vat[0].taxable, dec!(99.99));
        assert_eq!(c.type_code, InvoiceTypeCode::Satis);
        // Ties go to the earlier lines: 6.67, 6.67, 6.66.
        assert_eq!(c.lines[0].vat_amount, dec!(6.67));
        assert_eq!(c.lines[2].vat_amount, dec!(6.66));
    }

    #[test]
    fn discounts_become_line_allowances() {
        let src = try_src(
            vec![
                line(dec!(100), true),
                line(dec!(50), true),
                line(dec!(-15), true),
            ],
            dec!(20),
        );
        let c = compose(&src, None).unwrap();
        assert_eq!(c.lines.len(), 2);
        assert_eq!(c.lines[0].allowance, dec!(10.00));
        assert_eq!(c.lines[1].allowance, dec!(5.00));
        assert_eq!(c.lines[0].line_extension, dec!(90.00));
        assert_eq!(c.totals.line_extension, dec!(135));
        assert_eq!(c.totals.allowance_total, dec!(15.00));
        assert_eq!(c.totals.vat_total, dec!(27.00));
        assert_eq!(c.totals.payable, dec!(162.00));
    }

    #[test]
    fn uneven_discount_keeps_kuruş_exact() {
        let src = try_src(
            vec![
                line(dec!(10.00), true),
                line(dec!(10.00), true),
                line(dec!(10.00), true),
                line(dec!(-0.10), true),
            ],
            dec!(20),
        );
        let c = compose(&src, None).unwrap();
        let allow: Vec<Decimal> = c.lines.iter().map(|l| l.allowance).collect();
        assert_eq!(allow, vec![dec!(0.04), dec!(0.03), dec!(0.03)]);
        assert_eq!(c.totals.line_extension, dec!(29.90));
        let vat: Decimal = c.lines.iter().map(|l| l.vat_amount).sum();
        assert_eq!(vat, src.tax_total);
    }

    #[test]
    fn non_taxable_lines_need_an_exemption() {
        let src = try_src(vec![line(dec!(100), true), line(dec!(30), false)], dec!(20));
        assert_eq!(compose(&src, None), Err(ComposeError::ExemptionRequired));
        let c = compose(&src, Some(&exemption("351"))).unwrap();
        assert_eq!(c.vat.len(), 2);
        assert_eq!(c.vat[1].taxable, dec!(30));
        assert_eq!(c.vat[1].exemption.as_ref().unwrap().code, "351");
        assert_eq!(c.type_code, InvoiceTypeCode::Satis);
        assert_eq!(c.lines[1].vat_rate, Decimal::ZERO);
    }

    #[test]
    fn exempt_invoice_is_istisna() {
        let src = try_src(vec![line(dec!(100), false)], Decimal::ZERO);
        let c = compose(&src, Some(&exemption("301"))).unwrap();
        assert_eq!(c.type_code, InvoiceTypeCode::Istisna);
        assert_eq!(c.totals.vat_total, Decimal::ZERO);
        assert_eq!(
            compose(&src, Some(&exemption("801"))),
            Err(ComposeError::BadExemption("801".into()))
        );
    }

    #[test]
    fn rejects_unmappable_invoices() {
        let mut src = try_src(vec![line(dec!(100), true)], dec!(20));
        src.taxes[0].compound = true;
        assert_eq!(compose(&src, None), Err(ComposeError::UnsupportedTaxes));
        let mut src = try_src(vec![line(dec!(100), true)], dec!(20));
        src.total += dec!(0.01);
        assert!(matches!(
            compose(&src, None),
            Err(ComposeError::Inconsistent(_))
        ));
        let src = try_src(vec![line(dec!(10), true), line(dec!(-20), true)], dec!(20));
        assert!(compose(&src, None).is_err());
        let src = try_src(
            vec![line(dec!(0.01), true), line(dec!(100), true)],
            dec!(20),
        );
        assert_eq!(compose(&src, None), Err(ComposeError::VatRoundsToZero));
    }

    #[test]
    fn quantity_and_price_reproduce_the_amount() {
        let mut l = line(dec!(29.97), true);
        l.quantity = dec!(3);
        l.unit_price = dec!(9.99);
        let mut odd = line(dec!(10.00), true);
        odd.quantity = dec!(3);
        odd.unit_price = dec!(3.333333333);
        let src = try_src(vec![l, odd], dec!(20));
        let c = compose(&src, None).unwrap();
        assert_eq!(
            (c.lines[0].quantity, c.lines[0].unit_price),
            (dec!(3), dec!(9.99))
        );
        let q = c.lines[1].quantity;
        let p = c.lines[1].unit_price;
        assert_eq!(round(q * p, Currency::TRY), dec!(10.00));
    }

    #[test]
    fn largest_remainder_is_exact() {
        let w = [dec!(1), dec!(1), dec!(1)];
        assert_eq!(
            largest_remainder(dec!(1.00), &w, dec!(0.01)).unwrap(),
            vec![dec!(0.34), dec!(0.33), dec!(0.33)]
        );
        assert_eq!(
            largest_remainder(Decimal::ZERO, &[], dec!(0.01)).unwrap(),
            Vec::<Decimal>::new()
        );
        assert!(largest_remainder(dec!(1), &[], dec!(0.01)).is_none());
    }
}
