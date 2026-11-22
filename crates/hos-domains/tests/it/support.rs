//! Shared helpers for the wiremock-based registrar tests.
#![allow(dead_code)]

use hos_core::integrations::registrar::{ContactInfo, DomainContacts, DomainRegistration};
use hos_core::integrations::{PhoneNumber, PostalAddress};
use serde_json::Value;
use wiremock::Request;

/// A complete Turkish individual contact.
pub fn contact() -> ContactInfo {
    ContactInfo {
        first_name: "Ada".into(),
        last_name: "Yılmaz".into(),
        company: None,
        email: "ada@example.com".into(),
        phone: PhoneNumber::new("+90", "555 123 45 67"),
        fax: None,
        address: PostalAddress {
            line1: "Atatürk Cad. 12".into(),
            line2: None,
            city: "İstanbul".into(),
            state: Some("İstanbul".into()),
            postal_code: Some("34000".into()),
            country_code: "TR".into(),
        },
        tax_id: None,
        extra: Value::Null,
    }
}

/// A registration of `domain` for `years` with the same contact everywhere.
pub fn registration(domain: &str, years: u32) -> DomainRegistration {
    DomainRegistration {
        domain: domain.into(),
        years,
        nameservers: vec!["ns1.host.example".into(), "ns2.host.example".into()],
        contacts: DomainContacts::same(contact()),
        privacy: true,
        auto_renew: false,
        premium_price: None,
        extra: Value::Null,
    }
}

/// Decoded `application/x-www-form-urlencoded` body.
pub fn form(req: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&req.body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Decoded query string.
pub fn query(req: &Request) -> Vec<(String, String)> {
    req.url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// All values of `key` in a pair list.
pub fn values(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .collect()
}

/// First value of `key`.
pub fn value(pairs: &[(String, String)], key: &str) -> Option<String> {
    values(pairs, key).into_iter().next()
}

/// Request paths in arrival order.
pub fn paths(requests: &[Request]) -> Vec<String> {
    requests.iter().map(|r| r.url.path().to_owned()).collect()
}
