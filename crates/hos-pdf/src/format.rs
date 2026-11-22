//! Number, money and date formatting for documents.
//!
//! These are byte-for-byte the rules of the web layer's printable invoice
//! (`hos_web::i18n::format_money` / `format_decimal` / `format_date`), so a
//! PDF and the HTML invoice show the same strings; `hos-web` has a test that
//! compares both implementations.

use chrono::{Datelike, NaiveDate};
use hos_core::domain::Locale;
use hos_core::money::{Currency, Money};
use rust_decimal::Decimal;

/// Display symbol of a currency (`₺`, `$`, `€`, `£`, else empty).
pub fn currency_symbol(currency: Currency) -> &'static str {
    match currency.as_str() {
        "TRY" => "₺",
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        _ => "",
    }
}

fn group_digits(int: &str, sep: char) -> String {
    let mut grouped = String::with_capacity(int.len() + int.len() / 3);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i).is_multiple_of(3) {
            grouped.push(sep);
        }
        grouped.push(c);
    }
    grouped
}

/// `1.234,50` (tr) / `1,234.50` (en) with `dp` decimals.
pub fn decimal(value: Decimal, dp: u32, locale: Locale) -> String {
    let rounded = value.round_dp(dp);
    let s = format!("{:.*}", dp as usize, rounded.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let (gs, ds) = match locale {
        Locale::Tr => ('.', ','),
        Locale::En => (',', '.'),
    };
    let mut out = String::new();
    if rounded.is_sign_negative() && !rounded.is_zero() {
        out.push('-');
    }
    out.push_str(&group_digits(int, gs));
    if !frac.is_empty() {
        out.push(ds);
        out.push_str(frac);
    }
    out
}

/// Quantity: up to 4 decimals, trailing zeros dropped (`1`, `2,5`).
pub fn quantity(value: Decimal, locale: Locale) -> String {
    let n = value.round_dp(4).normalize();
    decimal(n, n.scale(), locale)
}

/// Money: `1.234,56 ₺` (tr) / `₺1,234.56` (en); unknown currencies get the
/// ISO code (`1.234,56 CHF`).
pub fn money(amount: Decimal, currency: Currency, locale: Locale) -> String {
    let m = Money::new(amount, currency).round();
    let digits = decimal(m.amount, currency.minor_units(), locale);
    match (locale, currency_symbol(currency)) {
        (_, "") => format!("{digits} {}", currency.as_str()),
        (Locale::Tr, s) => format!("{digits} {s}"),
        (Locale::En, s) => match digits.strip_prefix('-') {
            Some(rest) => format!("-{s}{rest}"),
            None => format!("{s}{digits}"),
        },
    }
}

const MONTHS_TR: [&str; 12] = [
    "Ocak", "Şubat", "Mart", "Nisan", "Mayıs", "Haziran", "Temmuz", "Ağustos", "Eylül", "Ekim",
    "Kasım", "Aralık",
];
const MONTHS_EN: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `16 Kasım 2026` / `November 16, 2026` (a calendar date, no time zone).
pub fn date(d: NaiveDate, locale: Locale) -> String {
    let idx = d.month0() as usize;
    match locale {
        Locale::Tr => format!("{} {} {}", d.day(), MONTHS_TR[idx], d.year()),
        Locale::En => format!("{} {}, {}", MONTHS_EN[idx], d.day(), d.year()),
    }
}

/// Tax rate label: `%20` / `%18,5` (as the HTML invoice: `rate.normalize()`).
pub fn percent(rate: Decimal) -> String {
    format!("%{}", rate.normalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn money_formats() {
        assert_eq!(money(dec!(1234.5), Currency::TRY, Locale::Tr), "1.234,50 ₺");
        assert_eq!(money(dec!(1234.5), Currency::TRY, Locale::En), "₺1,234.50");
        assert_eq!(money(dec!(-12.345), Currency::USD, Locale::En), "-$12.35");
        assert_eq!(money(dec!(-12.345), Currency::USD, Locale::Tr), "-12,35 $");
        let chf = Currency::new("CHF").unwrap();
        assert_eq!(money(dec!(1000), chf, Locale::Tr), "1.000,00 CHF");
        let jpy = Currency::new("JPY").unwrap();
        assert_eq!(money(dec!(1500.4), jpy, Locale::En), "1,500 JPY");
        assert_eq!(money(dec!(0), Currency::EUR, Locale::Tr), "0,00 €");
    }

    #[test]
    fn quantities_and_dates() {
        assert_eq!(quantity(dec!(1.0000), Locale::Tr), "1");
        assert_eq!(quantity(dec!(2.50), Locale::Tr), "2,5");
        assert_eq!(quantity(dec!(1234.125), Locale::En), "1,234.125");
        let d = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        assert_eq!(date(d, Locale::Tr), "16 Kasım 2026");
        assert_eq!(date(d, Locale::En), "November 16, 2026");
        assert_eq!(percent(dec!(20.0000)), "%20");
    }
}
