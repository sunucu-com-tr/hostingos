//! The record shape both connectors produce: one WHMCS table row, keyed by
//! the **WHMCS database column names** (`tblclients.firstname`, …). The API
//! connector translates its JSON into the same names, so there is exactly
//! one mapper per entity.
//!
//! Getters are lenient the way WHMCS data needs: `0000-00-00` dates are
//! `None`, `'on'` / `'1'` / `1` are true, numbers may arrive as text,
//! `-1.00` prices mean "disabled" (handled by the mappers).

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use rust_decimal::Decimal;

/// A column value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Int(i64::from(v))
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Null, Into::into)
    }
}
impl From<&serde_json::Value> for Value {
    fn from(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Int(i64::from(*b)),
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(Self::Int)
                .or_else(|| n.as_f64().map(Self::Float))
                .unwrap_or(Self::Null),
            serde_json::Value::String(s) => Self::Text(s.clone()),
            other => Self::Text(other.to_string()),
        }
    }
}

/// Pseudo column listing the columns a source cut ([`Row::cap`]).
pub const TRUNCATED: &str = "__truncated";

/// Longest text value (characters) / binary value (bytes) a source hands
/// over; longer ones are cut and reported. 256 Ki characters is far above
/// any WHMCS field a mapper keeps (ticket bodies are cut much shorter).
pub const MAX_FIELD_CHARS: usize = 256 * 1024;

/// One source row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    cols: BTreeMap<String, Value>,
}

static NULL: Value = Value::Null;

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.is_empty() || s.starts_with("0000-00-00") {
        return None;
    }
    let head = s.get(..10).unwrap_or(s);
    NaiveDate::parse_from_str(head, "%Y-%m-%d")
        .ok()
        .or_else(|| NaiveDate::parse_from_str(head, "%d/%m/%Y").ok())
}

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    if s.is_empty() || s.starts_with("0000-00-00") {
        return None;
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok())
        .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok())
        .or_else(|| DateTime::parse_from_rfc3339(s).ok().map(|d| d.naive_utc()))
        .or_else(|| parse_date(s).and_then(|d| d.and_hms_opt(0, 0, 0)))
}

impl Row {
    /// Empty row.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: sets `col`.
    #[must_use]
    pub fn with(mut self, col: &str, v: impl Into<Value>) -> Self {
        self.set(col, v);
        self
    }

    /// Sets `col`.
    pub fn set(&mut self, col: &str, v: impl Into<Value>) {
        self.cols.insert(col.to_owned(), v.into());
    }

    /// Raw value (`Null` when absent).
    pub fn get(&self, col: &str) -> &Value {
        self.cols.get(col).unwrap_or(&NULL)
    }

    /// Whether the column exists (even when NULL).
    pub fn has(&self, col: &str) -> bool {
        self.cols.contains_key(col)
    }

    /// Approximate memory the values take (bytes of text / binary values,
    /// 16 per other value and per column name).
    pub fn approx_bytes(&self) -> usize {
        self.cols
            .iter()
            .map(|(k, v)| {
                k.len()
                    + match v {
                        Value::Text(s) => s.len(),
                        Value::Bytes(b) => b.len(),
                        _ => 16,
                    }
            })
            .sum()
    }

    /// Cuts text values longer than `max_chars` characters and binary
    /// values longer than `max_chars` bytes to that length; the names of
    /// the cut columns are recorded in [`TRUNCATED`] for the report.
    pub fn cap(&mut self, max_chars: usize) {
        let mut cut = Vec::new();
        for (k, v) in &mut self.cols {
            match v {
                Value::Text(s) if s.len() > max_chars => {
                    if let Some((at, _)) = s.char_indices().nth(max_chars) {
                        s.truncate(at);
                        cut.push(k.clone());
                    }
                }
                Value::Bytes(b) if b.len() > max_chars => {
                    b.truncate(max_chars);
                    cut.push(k.clone());
                }
                _ => {}
            }
        }
        if !cut.is_empty() {
            self.set(TRUNCATED, cut.join(","));
        }
    }

    /// Columns [`Row::cap`] cut, comma-separated.
    pub fn truncated(&self) -> Option<String> {
        self.str(TRUNCATED)
    }

    /// Column names.
    pub fn columns(&self) -> impl Iterator<Item = &str> {
        self.cols.keys().map(String::as_str)
    }

    /// `id` (0 when missing).
    pub fn id(&self) -> i64 {
        self.int("id").unwrap_or(0)
    }

    /// Raw text (untrimmed); bytes decode lossily.
    pub fn raw_str(&self, col: &str) -> Option<String> {
        match self.get(col) {
            Value::Null => None,
            Value::Text(s) => Some(s.clone()),
            Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Int(i) => Some(i.to_string()),
            Value::Float(f) => Some(f.to_string()),
            Value::Date(d) => Some(d.to_string()),
            Value::DateTime(d) => Some(d.format("%Y-%m-%d %H:%M:%S").to_string()),
        }
    }

    /// Trimmed, non-empty text.
    pub fn str(&self, col: &str) -> Option<String> {
        self.raw_str(col)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// Trimmed text (`""` when absent).
    pub fn text(&self, col: &str) -> String {
        self.str(col).unwrap_or_default()
    }

    /// Integer (text is parsed; `"12.00"` → 12).
    pub fn int(&self, col: &str) -> Option<i64> {
        match self.get(col) {
            Value::Int(i) => Some(*i),
            #[allow(clippy::cast_possible_truncation)]
            Value::Float(f) if f.is_finite() => Some(f.trunc() as i64),
            Value::Text(_) | Value::Bytes(_) => {
                let s = self.str(col)?;
                s.parse::<i64>().ok().or_else(|| {
                    s.parse::<Decimal>()
                        .ok()
                        .and_then(|d| d.trunc().try_into().ok())
                })
            }
            _ => None,
        }
    }

    /// Positive integer reference (`0` / missing → `None`).
    pub fn reference(&self, col: &str) -> Option<i64> {
        self.int(col).filter(|v| *v > 0)
    }

    /// Decimal.
    pub fn dec(&self, col: &str) -> Option<Decimal> {
        match self.get(col) {
            Value::Int(i) => Some(Decimal::from(*i)),
            Value::Float(f) => Decimal::try_from(*f).ok(),
            Value::Text(_) | Value::Bytes(_) => {
                let s = self.str(col)?;
                s.replace(',', "").parse::<Decimal>().ok()
            }
            _ => None,
        }
    }

    /// Decimal, zero when missing.
    pub fn money(&self, col: &str) -> Decimal {
        self.dec(col).unwrap_or(Decimal::ZERO)
    }

    /// Date (`0000-00-00` → `None`).
    pub fn date(&self, col: &str) -> Option<NaiveDate> {
        match self.get(col) {
            Value::Date(d) => Some(*d),
            Value::DateTime(d) => Some(d.date()),
            _ => parse_date(&self.raw_str(col)?),
        }
    }

    /// Timestamp, read as UTC.
    pub fn datetime(&self, col: &str) -> Option<DateTime<Utc>> {
        match self.get(col) {
            Value::DateTime(d) => Some(d.and_utc()),
            Value::Date(d) => d.and_hms_opt(0, 0, 0).map(|d| d.and_utc()),
            _ => parse_datetime(&self.raw_str(col)?).map(|d| d.and_utc()),
        }
    }

    /// WHMCS flags: `on`, `1`, `yes`, `true`, `Y` (case-insensitive).
    pub fn flag(&self, col: &str) -> bool {
        match self.get(col) {
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            _ => self.str(col).is_some_and(|s| {
                matches!(
                    s.to_ascii_lowercase().as_str(),
                    "on" | "1" | "yes" | "true" | "y"
                )
            }),
        }
    }

    /// Raw bytes (secrets, attachments).
    pub fn bytes(&self, col: &str) -> Option<Vec<u8>> {
        match self.get(col) {
            Value::Bytes(b) => Some(b.clone()),
            Value::Text(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_values_are_cut_and_listed() {
        let mut r = Row::new()
            .with("id", 7)
            .with("message", "ğ".repeat(12))
            .with("short", "ok")
            .with("blob", Value::Bytes(vec![1; 20]));
        r.cap(10);
        assert_eq!(r.text("message"), "ğ".repeat(10), "cut on a char boundary");
        assert_eq!(r.bytes("blob").map(|b| b.len()), Some(10));
        assert_eq!(r.text("short"), "ok");
        assert_eq!(r.truncated().as_deref(), Some("blob,message"));
        let mut small = Row::new().with("a", "x");
        small.cap(10);
        assert_eq!(small.truncated(), None);
        assert!(r.approx_bytes() >= 20 + 10);
    }

    #[test]
    fn lenient_getters() {
        let r = Row::new()
            .with("id", "42")
            .with("name", "  Ada ")
            .with("blank", "   ")
            .with("price", "-1.00")
            .with("big", "1,234.50")
            .with("zero_date", "0000-00-00")
            .with("date", "2024-02-29")
            .with("ts", "2024-02-29 13:14:15")
            .with("on", "on")
            .with("off", "")
            .with("num_flag", 1)
            .with("float_int", 12.0);
        assert_eq!(r.id(), 42);
        assert_eq!(r.str("name").as_deref(), Some("Ada"));
        assert_eq!(r.str("blank"), None);
        assert_eq!(r.text("missing"), "");
        assert_eq!(r.dec("price"), Some(Decimal::new(-100, 2)));
        assert_eq!(r.dec("big"), Some(Decimal::new(123_450, 2)));
        assert_eq!(r.date("zero_date"), None);
        assert_eq!(r.datetime("zero_date"), None);
        assert_eq!(r.date("date"), NaiveDate::from_ymd_opt(2024, 2, 29));
        assert_eq!(
            r.datetime("ts").unwrap().to_rfc3339(),
            "2024-02-29T13:14:15+00:00"
        );
        assert_eq!(r.date("ts"), NaiveDate::from_ymd_opt(2024, 2, 29));
        assert!(r.flag("on") && !r.flag("off") && r.flag("num_flag") && !r.flag("missing"));
        assert_eq!(r.int("float_int"), Some(12));
        assert_eq!(r.reference("missing"), None);
        assert_eq!(Row::new().with("x", 0).reference("x"), None);
        assert!(r.has("blank") && !r.has("nope"));
    }

    #[test]
    fn json_values() {
        let v = serde_json::json!({"a": 1, "b": "x", "c": null, "d": true, "e": 1.5});
        assert_eq!(Value::from(&v["a"]), Value::Int(1));
        assert_eq!(Value::from(&v["b"]), Value::Text("x".into()));
        assert_eq!(Value::from(&v["c"]), Value::Null);
        assert_eq!(Value::from(&v["d"]), Value::Int(1));
        assert_eq!(Value::from(&v["e"]), Value::Float(1.5));
    }
}
