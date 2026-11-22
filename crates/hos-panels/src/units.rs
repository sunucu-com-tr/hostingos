//! Lenient number / size parsing for panel responses, which mix numbers,
//! numeric strings, `"unlimited"`, and suffixed sizes (`"512M"`, `"1.5G"`).

use serde_json::Value;

const MIB: f64 = 1024.0 * 1024.0;

/// A count or MiB limit: numbers or numeric strings. `"unlimited"`, `"∞"`,
/// blank, `null` and negative values are `None` (unlimited / unknown).
pub(crate) fn limit(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(float_to_u64)
        }),
        Value::String(s) => limit_str(s),
        _ => None,
    }
}

/// String form of [`limit`].
pub(crate) fn limit_str(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || is_unlimited_word(s) {
        return None;
    }
    s.parse::<u64>().ok().or_else(|| {
        s.parse::<f64>()
            .ok()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(float_to_u64)
    })
}

fn is_unlimited_word(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "unlimited" | "unlim" | "∞" | "-1" | "none" | "null" | "n/a"
    )
}

/// A size with an optional unit suffix, in MiB. Bare numbers are taken to be
/// in `bare_unit_mib` units (1.0 = MiB). `"unlimited"` → `None`.
pub(crate) fn size_mib(v: &Value, bare_unit_mib: f64) -> Option<u64> {
    match v {
        Value::Number(n) => n
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| float_to_u64(f * bare_unit_mib)),
        Value::String(s) => size_mib_str(s, bare_unit_mib),
        _ => None,
    }
}

/// String form of [`size_mib`]: `"512M"`, `"1.5 GB"`, `"100k"`, `"2048"`.
pub(crate) fn size_mib_str(s: &str, bare_unit_mib: f64) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || is_unlimited_word(s) {
        return None;
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .parse()
        .ok()
        .filter(|f: &f64| f.is_finite() && *f >= 0.0)?;
    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "" => bare_unit_mib,
        "b" | "bytes" => 1.0 / MIB,
        "k" | "kb" | "kib" => 1.0 / 1024.0,
        "m" | "mb" | "mib" => 1.0,
        "g" | "gb" | "gib" => 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0,
        _ => return None,
    };
    Some(float_to_u64(num * factor))
}

/// Bytes → MiB (rounded to nearest).
pub(crate) fn bytes_to_mib(bytes: u64) -> u64 {
    float_to_u64(bytes as f64 / MIB)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn float_to_u64(f: f64) -> u64 {
    // `as` saturates for out-of-range floats; inputs are validated non-negative.
    f.round() as u64
}

/// A boolean flag in a panel response: `1`, `"1"`, `true`, `"yes"`, `"on"`.
pub(crate) fn flag(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
        Value::String(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "y"
        ),
        _ => false,
    }
}

/// Non-empty string value (numbers are stringified).
pub(crate) fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits() {
        assert_eq!(limit(&json!(10)), Some(10));
        assert_eq!(limit(&json!("25")), Some(25));
        assert_eq!(limit(&json!("unlimited")), None);
        assert_eq!(limit(&json!(null)), None);
        assert_eq!(limit(&json!("")), None);
        assert_eq!(limit(&json!(-1)), None);
        assert_eq!(limit(&json!(2.6)), Some(3));
    }

    #[test]
    fn sizes() {
        assert_eq!(size_mib(&json!("512M"), 1.0), Some(512));
        assert_eq!(size_mib(&json!("1.5G"), 1.0), Some(1536));
        assert_eq!(size_mib(&json!("2048K"), 1.0), Some(2));
        assert_eq!(size_mib(&json!("100"), 1.0), Some(100));
        assert_eq!(size_mib(&json!(3_145_728), 1.0 / MIB), Some(3));
        assert_eq!(size_mib(&json!("unlimited"), 1.0), None);
        assert_eq!(size_mib(&json!("12 parsecs"), 1.0), None);
        assert_eq!(bytes_to_mib(10 * 1024 * 1024), 10);
    }

    #[test]
    fn flags_and_text() {
        assert!(flag(&json!(1)));
        assert!(flag(&json!("yes")));
        assert!(!flag(&json!("0")));
        assert!(!flag(&json!(null)));
        assert_eq!(text(&json!(" a ")), Some("a".into()));
        assert_eq!(text(&json!(5)), Some("5".into()));
        assert_eq!(text(&json!("")), None);
    }
}
