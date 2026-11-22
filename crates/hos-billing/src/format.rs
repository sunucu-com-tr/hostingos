//! Locale-aware formatting of amounts and dates for e-mails and notifications
//! (hos-mail templates receive pre-formatted strings).

use chrono::NaiveDate;
use hos_core::domain::Locale;
use hos_core::money::{Currency, Money};

fn symbol(currency: Currency) -> Option<&'static str> {
    match currency.as_str() {
        "TRY" => Some("₺"),
        "USD" => Some("$"),
        "EUR" => Some("€"),
        "GBP" => Some("£"),
        _ => None,
    }
}

fn group(int_part: &str, sep: char) -> String {
    let digits: Vec<char> = int_part.chars().collect();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(*c);
    }
    out
}

/// `₺1.250,00` (tr) / `₺1,250.00` (en); unknown currencies get the code as suffix.
pub fn money(m: &Money, locale: Locale) -> String {
    let s = m.abs().to_decimal_string();
    let (int_part, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_owned(), Some(f.to_owned())),
        None => (s, None),
    };
    let (thousands, decimal) = match locale {
        Locale::Tr => ('.', ','),
        Locale::En => (',', '.'),
    };
    let mut number = group(&int_part, thousands);
    if let Some(f) = frac {
        number.push(decimal);
        number.push_str(&f);
    }
    let sign = if m.is_negative() { "-" } else { "" };
    match symbol(m.currency) {
        Some(sym) => format!("{sign}{sym}{number}"),
        None => format!("{sign}{number} {}", m.currency),
    }
}

/// `30.11.2026` (tr) / `2026-11-30` (en).
pub fn date(d: NaiveDate, locale: Locale) -> String {
    match locale {
        Locale::Tr => d.format("%d.%m.%Y").to_string(),
        Locale::En => d.format("%Y-%m-%d").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn formats() {
        let m = Money::new(dec!(1250), Currency::TRY);
        assert_eq!(money(&m, Locale::Tr), "₺1.250,00");
        assert_eq!(money(&m, Locale::En), "₺1,250.00");
        let big = Money::new(dec!(-1234567.891), Currency::USD);
        assert_eq!(money(&big, Locale::En), "-$1,234,567.89");
        let jpy = Money::new(dec!(500), Currency::new("JPY").unwrap());
        assert_eq!(money(&jpy, Locale::En), "500 JPY");
        let d = NaiveDate::from_ymd_opt(2026, 11, 30).unwrap();
        assert_eq!(date(d, Locale::Tr), "30.11.2026");
        assert_eq!(date(d, Locale::En), "2026-11-30");
    }
}
