//! Small helpers shared by the registrar implementations.

use std::str::FromStr;

use hos_core::IntegrationError;
use hos_core::integrations::registrar::ContactInfo;
use hos_core::money::{Currency, Money};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::name::{self, DomainName};

/// Parses a registrable domain (strict) into an [`IntegrationError::InvalidInput`]
/// on failure.
pub fn domain(input: &str) -> Result<DomainName, IntegrationError> {
    Ok(DomainName::parse(input)?)
}

/// Validates and normalises nameservers: host-name syntax, lower-case ASCII,
/// de-duplicated (order kept), at most 13. Empty input stays empty.
pub fn nameservers(input: &[String]) -> Result<Vec<String>, IntegrationError> {
    let mut out: Vec<String> = Vec::with_capacity(input.len());
    for ns in input {
        if ns.trim().is_empty() {
            continue;
        }
        let host = name::normalize_hostname(ns).map_err(|e| {
            IntegrationError::InvalidInput(format!("invalid nameserver {:?}: {e}", ns.trim()))
        })?;
        if !out.contains(&host) {
            out.push(host);
        }
    }
    if out.len() > 13 {
        return Err(IntegrationError::InvalidInput(
            "at most 13 nameservers are allowed".into(),
        ));
    }
    Ok(out)
}

/// Like [`nameservers`] but requires at least one (replacing NS with nothing is
/// never what the caller wants).
pub fn required_nameservers(input: &[String]) -> Result<Vec<String>, IntegrationError> {
    let ns = nameservers(input)?;
    if ns.is_empty() {
        return Err(IntegrationError::InvalidInput(
            "at least one nameserver is required".into(),
        ));
    }
    Ok(ns)
}

/// Comma/whitespace separated list from config (`"ns1.x.com, ns2.x.com"`).
pub fn split_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split([',', ' ', '\n', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses a decimal from a provider string/number value.
pub fn decimal(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => parse_decimal(s),
        Value::Number(n) => parse_decimal(&n.to_string()),
        _ => None,
    }
}

/// Parses a decimal string (`"8.50"`, `"1e2"` not accepted).
pub fn parse_decimal(s: &str) -> Option<Decimal> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Decimal::from_str_exact(s)
        .or_else(|_| Decimal::from_str(s))
        .ok()
}

/// Money from a provider decimal and currency code.
pub fn money(amount: Option<Decimal>, currency: &str) -> Option<Money> {
    let amount = amount?;
    Currency::new(currency).ok().map(|c| Money::new(amount, c))
}

/// `extra.attributes` as raw `(name, value)` pairs — the escape hatch for
/// registrar-specific attributes not covered by [`crate::tld_rules`] keys.
pub fn raw_attributes(extra: &Value) -> Vec<(String, String)> {
    let Some(obj) = extra.get("attributes").and_then(Value::as_object) else {
        return Vec::new();
    };
    obj.iter()
        .filter_map(|(k, v)| {
            let v = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((k.clone(), v))
        })
        .collect()
}

/// Boolean option from an `extra` object (`true`, `"true"`, `"yes"`, `1`).
pub fn extra_bool(extra: &Value, key: &str) -> Option<bool> {
    match extra.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Some(true),
            "false" | "no" | "0" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Basic completeness check for a contact before sending it to a registrar.
pub fn validate_contact(role: &str, c: &ContactInfo) -> Result<(), IntegrationError> {
    let mut missing = Vec::new();
    if c.first_name.trim().is_empty() && c.company.as_deref().is_none_or(|s| s.trim().is_empty()) {
        missing.push("first_name");
    }
    if c.email.trim().is_empty() || !c.email.contains('@') {
        missing.push("email");
    }
    if c.phone.number.is_empty() || c.phone.country_code.is_empty() {
        missing.push("phone");
    }
    if c.address.line1.trim().is_empty() {
        missing.push("address.line1");
    }
    if c.address.city.trim().is_empty() {
        missing.push("address.city");
    }
    if c.address.country_code.trim().len() != 2 {
        missing.push("address.country_code");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(IntegrationError::InvalidInput(format!(
            "{role} contact is incomplete: {}",
            missing.join(", ")
        )))
    }
}

/// Stable 64-bit FNV-1a hash (deterministic across runs; not cryptographic).
pub fn fnv1a(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nameserver_normalisation() {
        let ns = nameservers(&[
            "NS1.Example.com.".into(),
            "ns1.example.com".into(),
            " ".into(),
            "ns2.example.com".into(),
        ])
        .unwrap();
        assert_eq!(ns, vec!["ns1.example.com", "ns2.example.com"]);
        assert!(nameservers(&["bad host".into()]).is_err());
        assert!(required_nameservers(&[]).is_err());
        let many: Vec<String> = (0..14).map(|i| format!("ns{i}.example.com")).collect();
        assert!(nameservers(&many).is_err());
        assert_eq!(
            split_list(Some("a.b.c, d.e.f\ng.h.i")),
            vec!["a.b.c", "d.e.f", "g.h.i"]
        );
    }

    #[test]
    fn value_helpers() {
        assert_eq!(decimal(&json!("8.50")), Some(Decimal::new(850, 2)));
        assert_eq!(decimal(&json!(8.5)), Some(Decimal::new(85, 1)));
        assert_eq!(decimal(&json!("x")), None);
        let attrs = raw_attributes(&json!({"attributes": {"a": "1", "b": 2, "c": null}}));
        assert_eq!(attrs.len(), 2);
        assert_eq!(extra_bool(&json!({"x": "yes"}), "x"), Some(true));
        assert_eq!(fnv1a("a"), fnv1a("a"));
    }
}
