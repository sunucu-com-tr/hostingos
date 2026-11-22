//! Translations and locale-aware formatting.
//!
//! Catalogs live in `locales/<locale>/<area>.json` (`common`, `storefront`,
//! `auth`, `client`, `admin`, `platform`, … — every `*.json` file is loaded).
//! Keys are namespaced by file name: `locales/tr/auth.json` →
//! `{"login": {"title": "Giriş yap"}}` is `auth.login.title`.
//!
//! * Placeholders: `"Hoş geldin, {name}"` → [`Catalog::tv`].
//! * Plurals: an object with `one` / `other` (optional `zero`) →
//!   [`Catalog::tn`] (`{count}` is substituted).
//! * A missing key renders as the key itself (and is logged at `debug`).
//!
//! Formatting: [`format_money`] (`1.234,56 ₺` / `₺1,234.56`), [`format_date`]
//! (`16 Kasım 2026` / `November 16, 2026`), [`format_number`].

use std::collections::{BTreeSet, HashMap};
use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use chrono::{DateTime, Datelike, Timelike, Utc};
use hos_core::domain::Locale;
use hos_core::money::{BillingCycle, Currency, Money};
use rust_decimal::Decimal;
use serde_json::Value;

/// One translated message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Plain text (may contain `{placeholders}`).
    Text(String),
    /// Plural forms.
    Plural {
        /// Used for `0` when present.
        zero: Option<String>,
        /// Used for `1`.
        one: Option<String>,
        /// Everything else.
        other: String,
    },
}

/// All messages of one locale.
#[derive(Debug, Clone)]
pub struct Catalog {
    locale: Locale,
    messages: HashMap<String, Message>,
}

impl Catalog {
    /// Empty catalog (tests).
    pub fn empty(locale: Locale) -> Self {
        Self {
            locale,
            messages: HashMap::new(),
        }
    }

    /// Builds a catalog from `(area, json)` pairs.
    pub fn from_json(locale: Locale, areas: &[(String, Value)]) -> anyhow::Result<Self> {
        let mut messages = HashMap::new();
        for (area, value) in areas {
            flatten(area, value, &mut messages)
                .with_context(|| format!("locale {locale} area {area}"))?;
        }
        Ok(Self { locale, messages })
    }

    /// The catalog's locale.
    pub fn locale(&self) -> Locale {
        self.locale
    }

    /// Whether `key` exists.
    pub fn has(&self, key: &str) -> bool {
        self.messages.contains_key(key)
    }

    /// Every key (sorted).
    pub fn keys(&self) -> BTreeSet<&str> {
        self.messages.keys().map(String::as_str).collect()
    }

    /// Text of `key` (the `other` form for plurals); the key itself when missing.
    pub fn t(&self, key: &str) -> String {
        match self.messages.get(key) {
            Some(Message::Text(s)) => s.clone(),
            Some(Message::Plural { other, .. }) => other.clone(),
            None => {
                tracing::debug!(key, locale = %self.locale, "missing translation");
                key.to_owned()
            }
        }
    }

    /// `key` with `{name}` placeholders replaced.
    pub fn tv(&self, key: &str, vars: &[(&str, &dyn Display)]) -> String {
        interpolate(&self.t(key), vars)
    }

    /// Plural form of `key` for `count` (`{count}` substituted).
    pub fn tn(&self, key: &str, count: i64) -> String {
        let raw = match self.messages.get(key) {
            Some(Message::Plural { zero, one, other }) => match count {
                0 => zero.as_ref().unwrap_or(other),
                1 => one.as_ref().unwrap_or(other),
                _ => other,
            }
            .clone(),
            Some(Message::Text(s)) => s.clone(),
            None => key.to_owned(),
        };
        interpolate(&raw, &[("count", &format_int(count, self.locale))])
    }
}

fn flatten(prefix: &str, value: &Value, out: &mut HashMap<String, Message>) -> anyhow::Result<()> {
    match value {
        Value::String(s) => {
            out.insert(prefix.to_owned(), Message::Text(s.clone()));
        }
        Value::Object(map) => {
            let is_plural = map.contains_key("other")
                && map
                    .keys()
                    .all(|k| matches!(k.as_str(), "zero" | "one" | "other"))
                && map.values().all(Value::is_string);
            if is_plural {
                let get = |k: &str| map.get(k).and_then(Value::as_str).map(str::to_owned);
                out.insert(
                    prefix.to_owned(),
                    Message::Plural {
                        zero: get("zero"),
                        one: get("one"),
                        other: get("other").unwrap_or_default(),
                    },
                );
            } else {
                for (k, v) in map {
                    flatten(&format!("{prefix}.{k}"), v, out)?;
                }
            }
        }
        other => bail!("{prefix}: expected string or object, got {other}"),
    }
    Ok(())
}

/// Replaces `{name}` placeholders.
pub fn interpolate(template: &str, vars: &[(&str, &dyn Display)]) -> String {
    if vars.is_empty() || !template.contains('{') {
        return template.to_owned();
    }
    let mut out = template.to_owned();
    for (name, value) in vars {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

/// Every locale's catalog.
#[derive(Debug, Clone)]
pub struct I18n {
    tr: Arc<Catalog>,
    en: Arc<Catalog>,
}

impl I18n {
    /// Loads `dir/tr/*.json` and `dir/en/*.json`.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            tr: Arc::new(load_locale(dir, Locale::Tr)?),
            en: Arc::new(load_locale(dir, Locale::En)?),
        })
    }

    /// Builds from ready catalogs.
    pub fn from_catalogs(tr: Catalog, en: Catalog) -> Self {
        Self {
            tr: Arc::new(tr),
            en: Arc::new(en),
        }
    }

    /// Catalog of `locale`.
    pub fn catalog(&self, locale: Locale) -> Arc<Catalog> {
        match locale {
            Locale::Tr => self.tr.clone(),
            Locale::En => self.en.clone(),
        }
    }

    /// Shorthand for `catalog(locale).t(key)`.
    pub fn t(&self, locale: Locale, key: &str) -> String {
        self.catalog(locale).t(key)
    }
}

fn load_locale(dir: &Path, locale: Locale) -> anyhow::Result<Catalog> {
    let ldir = dir.join(locale.as_str());
    let mut areas = Vec::new();
    let entries =
        std::fs::read_dir(&ldir).with_context(|| format!("reading {}", ldir.display()))?;
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let area = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        areas.push((area, value));
    }
    if areas.is_empty() {
        bail!("no locale files in {}", ldir.display());
    }
    Catalog::from_json(locale, &areas)
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Display symbol of a currency (`₺`, `$`, `€`, `£`, else the ISO code).
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

/// Formats a decimal with `dp` decimals: `1.234,50` (tr) / `1,234.50` (en).
pub fn format_decimal(value: Decimal, dp: u32, locale: Locale) -> String {
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

/// Formats an integer with grouping (`12.500` / `12,500`).
pub fn format_int(value: i64, locale: Locale) -> String {
    format_decimal(Decimal::from(value), 0, locale)
}

/// Formats a number with up to 2 decimals, dropping `,00`.
pub fn format_number(value: Decimal, locale: Locale) -> String {
    if value.fract().is_zero() {
        format_decimal(value, 0, locale)
    } else {
        format_decimal(value, 2, locale)
    }
}

/// Formats money: `1.234,56 ₺` (tr) / `₺1,234.56` (en). Unknown currencies
/// get the ISO code (`1.234,56 CHF`).
pub fn format_money(amount: Decimal, currency: Currency, locale: Locale) -> String {
    let money = Money::new(amount, currency).round();
    let digits = format_decimal(money.amount, currency.minor_units(), locale);
    let symbol = currency_symbol(currency);
    match (locale, symbol) {
        (_, "") => format!("{digits} {}", currency.as_str()),
        (Locale::Tr, s) => format!("{digits} {s}"),
        (Locale::En, s) => {
            if let Some(rest) = digits.strip_prefix('-') {
                format!("-{s}{rest}")
            } else {
                format!("{s}{digits}")
            }
        }
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

/// Parses an IANA time zone, falling back to UTC.
pub fn parse_tz(tz: &str) -> chrono_tz::Tz {
    tz.parse().unwrap_or(chrono_tz::UTC)
}

/// `16 Kasım 2026` / `November 16, 2026` in the tenant time zone.
pub fn format_date(dt: DateTime<Utc>, tz: &str, locale: Locale) -> String {
    let local = dt.with_timezone(&parse_tz(tz));
    let month = local.month0() as usize;
    match locale {
        Locale::Tr => format!("{} {} {}", local.day(), MONTHS_TR[month], local.year()),
        Locale::En => format!("{} {}, {}", MONTHS_EN[month], local.day(), local.year()),
    }
}

/// Month and year (`Kasım 2026` / `November 2026`), e.g. for report labels.
pub fn format_month(date: chrono::NaiveDate, locale: Locale) -> String {
    let month = date.month0() as usize;
    match locale {
        Locale::Tr => format!("{} {}", MONTHS_TR[month], date.year()),
        Locale::En => format!("{} {}", MONTHS_EN[month], date.year()),
    }
}

/// Date plus `HH:MM` (24 h) in the tenant time zone.
pub fn format_datetime(dt: DateTime<Utc>, tz: &str, locale: Locale) -> String {
    let local = dt.with_timezone(&parse_tz(tz));
    format!(
        "{} {:02}:{:02}",
        format_date(dt, tz, locale),
        local.hour(),
        local.minute()
    )
}

/// Translation key of a billing cycle (`common.cycle.monthly`, …).
pub fn cycle_key(cycle: BillingCycle) -> String {
    format!("common.cycle.{}", cycle.as_str())
}

/// Translation key of a "per period" suffix (`common.per.monthly` → `/ay`).
pub fn per_cycle_key(cycle: BillingCycle) -> String {
    format!("common.per.{}", cycle.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;
    use serde_json::json;

    #[test]
    fn money_formats() {
        let d = Decimal::new(123_456, 2);
        assert_eq!(format_money(d, Currency::TRY, Locale::Tr), "1.234,56 ₺");
        assert_eq!(format_money(d, Currency::TRY, Locale::En), "₺1,234.56");
        assert_eq!(format_money(d, Currency::USD, Locale::En), "$1,234.56");
        assert_eq!(
            format_money(Decimal::new(899, 1), Currency::TRY, Locale::Tr),
            "89,90 ₺"
        );
        assert_eq!(
            format_money(Decimal::new(-5, 0), Currency::USD, Locale::En),
            "-$5.00"
        );
        assert_eq!(
            format_money(Decimal::new(1_000_000, 0), Currency::EUR, Locale::Tr),
            "1.000.000,00 €"
        );
        let chf = Currency::new("CHF").unwrap();
        assert_eq!(format_money(Decimal::ONE, chf, Locale::Tr), "1,00 CHF");
    }

    #[test]
    fn dates_format_in_timezone() {
        let dt = Utc.with_ymd_and_hms(2026, 11, 16, 22, 30, 0).unwrap();
        assert_eq!(format_date(dt, "UTC", Locale::Tr), "16 Kasım 2026");
        // 22:30 UTC is already the 17th in Istanbul (UTC+3).
        assert_eq!(
            format_date(dt, "Europe/Istanbul", Locale::Tr),
            "17 Kasım 2026"
        );
        assert_eq!(format_date(dt, "UTC", Locale::En), "November 16, 2026");
        assert_eq!(
            format_datetime(dt, "Europe/Istanbul", Locale::Tr),
            "17 Kasım 2026 01:30"
        );
        assert_eq!(
            format_date(dt, "Not/AZone", Locale::En),
            "November 16, 2026"
        );
    }

    #[test]
    fn catalog_lookup_plural_and_vars() {
        let cat = Catalog::from_json(
            Locale::Tr,
            &[(
                "common".into(),
                json!({
                    "hello": "Merhaba {name}",
                    "items": {"zero": "Hiç öğe yok", "one": "{count} öğe", "other": "{count} öğe"},
                    "nested": {"deep": {"key": "derin"}}
                }),
            )],
        )
        .unwrap();
        assert_eq!(cat.tv("common.hello", &[("name", &"Ayşe")]), "Merhaba Ayşe");
        assert_eq!(cat.tn("common.items", 0), "Hiç öğe yok");
        assert_eq!(cat.tn("common.items", 1250), "1.250 öğe");
        assert_eq!(cat.t("common.nested.deep.key"), "derin");
        assert_eq!(cat.t("common.missing"), "common.missing");
        assert!(cat.has("common.items"));
    }

    #[test]
    fn shipped_catalogs_have_identical_keys() {
        let dir = crate::config::workspace_root().join("locales");
        let i18n = I18n::load(&dir).expect("locales load");
        let tr = i18n.catalog(Locale::Tr);
        let en = i18n.catalog(Locale::En);
        let tr_keys = tr.keys();
        let en_keys = en.keys();
        let only_tr: Vec<_> = tr_keys.difference(&en_keys).collect();
        let only_en: Vec<_> = en_keys.difference(&tr_keys).collect();
        assert!(only_tr.is_empty(), "keys missing in en: {only_tr:?}");
        assert!(only_en.is_empty(), "keys missing in tr: {only_en:?}");
    }

    /// Collects literal keys of `t("…")`, `tv("…"`, `tn("…"` calls in `.rs`
    /// and `.html` files (comment lines and this file excluded).
    fn collect_used_keys(dir: &std::path::Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_used_keys(&path, out);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("rs" | "html")) || path.ends_with("src/i18n.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for line in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
                for call in ["t(\"", "tv(\"", "tn(\""] {
                    let mut rest = line;
                    while let Some(pos) = rest.find(call) {
                        let before = rest[..pos].chars().last();
                        rest = &rest[pos + call.len()..];
                        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                            continue;
                        }
                        let Some(end) = rest.find('"') else { break };
                        let key = &rest[..end];
                        let plausible = key.contains('.')
                            && key.chars().all(|c| {
                                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
                            });
                        if plausible {
                            out.insert(key.to_owned());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_used_key_is_translated() {
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut used = BTreeSet::new();
        collect_used_keys(&crate_dir.join("src"), &mut used);
        collect_used_keys(&crate_dir.join("templates"), &mut used);
        assert!(used.len() > 100, "scanner found only {} keys", used.len());
        // Keys built at runtime.
        for code in [
            "not_found",
            "unauthorized",
            "forbidden",
            "csrf",
            "rate_limited",
            "bad_request",
            "conflict",
            "payload_too_large",
            "unavailable",
            "internal",
        ] {
            used.insert(format!("errors.{code}.title"));
            used.insert(format!("errors.{code}.text"));
        }
        for cycle in [
            "monthly",
            "quarterly",
            "semiannual",
            "annual",
            "biennial",
            "triennial",
            "one_time",
        ] {
            used.insert(format!("common.cycle.{cycle}"));
            used.insert(format!("common.per.{cycle}"));
        }
        let i18n =
            I18n::load(&crate::config::workspace_root().join("locales")).expect("locales load");
        for locale in [Locale::Tr, Locale::En] {
            let cat = i18n.catalog(locale);
            let missing: Vec<_> = used.iter().filter(|k| !cat.has(k)).collect();
            assert!(
                missing.is_empty(),
                "{locale}: untranslated keys {missing:?}"
            );
        }
    }

    #[test]
    fn number_formats() {
        assert_eq!(format_int(1234567, Locale::Tr), "1.234.567");
        assert_eq!(format_int(1234567, Locale::En), "1,234,567");
        assert_eq!(format_number(Decimal::new(25, 1), Locale::Tr), "2,50");
        assert_eq!(format_number(Decimal::new(20, 0), Locale::En), "20");
    }
}
