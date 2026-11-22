//! Built-in order screening rules, run next to the pluggable
//! [`crate::FraudCheck`] hook when an order is accepted.
//!
//! Each rule adds to a risk score; at or above `fraud.hold_score` the order
//! is moved to `fraud` (held for staff review, nothing is provisioned — the
//! existing order flow). Rules:
//!
//! | rule | score |
//! |---|---|
//! | e-mail, e-mail domain or IP on the ban list (`fraud_bans`) | 100 |
//! | disposable e-mail domain (built-in list) | 60 |
//! | stored card also held by another client | 50 |
//! | more than `max_orders_per_ip` orders from the IP in 24 h | 40 |
//! | more than `max_clients_per_ip` other accounts ordered from the IP in 24 h | 40 |
//! | more than `max_orders_per_client` orders of the client in 24 h | 30 |
//!
//! Settings (`fraud.*`) are off by default. No external provider is called:
//! a third-party scoring service plugs in through [`crate::FraudCheck`].
//! Country / IP mismatch needs an IP geolocation database, which the
//! platform does not ship; it is not part of these rules.

use std::collections::BTreeMap;

use chrono::Duration;
use hos_core::ids::TenantId;
use hos_db::repo::clients::ClientRow;
use hos_db::repo::fraud as frepo;
use hos_db::repo::orders::OrderRow;
use hos_db::repo::platform;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgExecutor};

use crate::{Billing, BillingResult, FraudVerdict};

/// Common disposable / throw-away mailbox domains.
pub const DISPOSABLE: &[&str] = &[
    "10minutemail.com",
    "33mail.com",
    "dispostable.com",
    "fakeinbox.com",
    "getnada.com",
    "guerrillamail.com",
    "guerrillamail.net",
    "maildrop.cc",
    "mailinator.com",
    "mailnesia.com",
    "mintemail.com",
    "mohmal.com",
    "sharklasers.com",
    "spamgourmet.com",
    "temp-mail.org",
    "tempmail.com",
    "tempmailo.com",
    "throwawaymail.com",
    "trashmail.com",
    "yopmail.com",
];

/// Whether `domain` (or a parent domain) is a known disposable provider.
pub fn is_disposable(domain: &str) -> bool {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    DISPOSABLE
        .iter()
        .any(|x| d == *x || d.ends_with(&format!(".{x}")))
}

/// Screening settings (`fraud.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FraudSettings {
    /// Run the built-in rules.
    pub enabled: bool,
    /// Score at which an order is held.
    pub hold_score: i64,
    /// Score disposable e-mail domains.
    pub block_disposable: bool,
    /// Orders from one IP in 24 h before the IP rule fires.
    pub max_orders_per_ip: i64,
    /// Other client accounts from one IP in 24 h before the rule fires.
    pub max_clients_per_ip: i64,
    /// Orders of one client in 24 h before the rule fires.
    pub max_orders_per_client: i64,
}

impl Default for FraudSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            hold_score: 50,
            block_disposable: true,
            max_orders_per_ip: 3,
            max_clients_per_ip: 1,
            max_orders_per_client: 5,
        }
    }
}

impl FraudSettings {
    /// From stored settings (`fraud.` prefix), clamped.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Self {
        let mut s = Self::default();
        let get = |k: &str| map.get(&format!("fraud.{k}"));
        let int =
            |k: &str, lo: i64, hi: i64| get(k).and_then(Value::as_i64).map(|v| v.clamp(lo, hi));
        if let Some(v) = get("enabled").and_then(Value::as_bool) {
            s.enabled = v;
        }
        if let Some(v) = get("block_disposable").and_then(Value::as_bool) {
            s.block_disposable = v;
        }
        if let Some(v) = int("hold_score", 1, 1000) {
            s.hold_score = v;
        }
        if let Some(v) = int("max_orders_per_ip", 1, 1000) {
            s.max_orders_per_ip = v;
        }
        if let Some(v) = int("max_clients_per_ip", 0, 1000) {
            s.max_clients_per_ip = v;
        }
        if let Some(v) = int("max_orders_per_client", 1, 1000) {
            s.max_orders_per_client = v;
        }
        s
    }

    /// Loads the tenant's settings.
    pub async fn load(db: impl PgExecutor<'_>, tenant_id: TenantId) -> BillingResult<Self> {
        Ok(Self::from_map(
            &platform::get_settings(db, tenant_id, "fraud.").await?,
        ))
    }

    /// `(key, value)` pairs to store.
    pub fn to_pairs(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("fraud.enabled", json!(self.enabled)),
            ("fraud.hold_score", json!(self.hold_score)),
            ("fraud.block_disposable", json!(self.block_disposable)),
            ("fraud.max_orders_per_ip", json!(self.max_orders_per_ip)),
            ("fraud.max_clients_per_ip", json!(self.max_clients_per_ip)),
            (
                "fraud.max_orders_per_client",
                json!(self.max_orders_per_client),
            ),
        ]
    }
}

/// One rule that fired.
fn hit(rule: &str, score: i64, detail: Value) -> Value {
    json!({ "rule": rule, "score": score, "detail": detail })
}

impl Billing {
    /// Runs the built-in rules for `order` of `client`. `None` when the rules
    /// are switched off.
    pub async fn screen_order(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        order: &OrderRow,
        client: &ClientRow,
    ) -> BillingResult<Option<(i64, Vec<Value>, bool)>> {
        let s = FraudSettings::load(&mut *conn, tenant_id).await?;
        if !s.enabled {
            return Ok(None);
        }
        let mut hits = Vec::new();
        for ban in frepo::matching_bans(&mut *conn, tenant_id, &client.email, order.ip).await? {
            hits.push(hit(
                "banned",
                100,
                json!({ "kind": ban.kind, "value": ban.value }),
            ));
        }
        let domain = client
            .email
            .rsplit_once('@')
            .map(|(_, d)| d)
            .unwrap_or_default();
        if s.block_disposable && is_disposable(domain) {
            hits.push(hit("disposable_email", 60, json!({ "domain": domain })));
        }
        let shared = frepo::clients_sharing_card(&mut *conn, tenant_id, client.id).await?;
        if shared > 0 {
            hits.push(hit("shared_card", 50, json!({ "other_clients": shared })));
        }
        let since = self.clock().now() - Duration::hours(24);
        if let Some(ip) = order.ip {
            let n = frepo::orders_from_ip(&mut *conn, tenant_id, ip, since, order.id).await?;
            if n >= s.max_orders_per_ip {
                hits.push(hit("ip_velocity", 40, json!({ "orders_24h": n })));
            }
            let others =
                frepo::clients_from_ip(&mut *conn, tenant_id, ip, since, client.id).await?;
            if others > s.max_clients_per_ip {
                hits.push(hit(
                    "ip_accounts",
                    40,
                    json!({ "other_clients_24h": others }),
                ));
            }
        }
        let mine =
            frepo::orders_of_client(&mut *conn, tenant_id, client.id, since, order.id).await?;
        if mine >= s.max_orders_per_client {
            hits.push(hit("client_velocity", 30, json!({ "orders_24h": mine })));
        }
        let score: i64 = hits
            .iter()
            .filter_map(|h| h.get("score").and_then(Value::as_i64))
            .sum();
        Ok(Some((score, hits, score >= s.hold_score)))
    }

    /// Combines the hook's verdict with the built-in rules (when enabled):
    /// scores add up, either side can hold the order, and the rule hits are
    /// stored under `rules` in the order's fraud details.
    pub(crate) async fn combined_fraud_verdict(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        order: &OrderRow,
        client: &ClientRow,
    ) -> BillingResult<FraudVerdict> {
        let mut verdict = self.fraud().check(order, client).await?;
        if let Some((score, hits, hold)) = self.screen_order(conn, tenant_id, order, client).await?
        {
            verdict.score += Decimal::from(score);
            verdict.flagged |= hold;
            let mut details = match verdict.details {
                Value::Object(m) => m,
                other => {
                    let mut m = serde_json::Map::new();
                    if !other.is_null() {
                        m.insert("hook".into(), other);
                    }
                    m
                }
            };
            details.insert("rules".into(), Value::Array(hits));
            verdict.details = Value::Object(details);
        }
        Ok(verdict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposable_domains() {
        assert!(is_disposable("mailinator.com"));
        assert!(is_disposable("x.mailinator.com"));
        assert!(is_disposable("YOPMAIL.COM."));
        assert!(!is_disposable("example.com"));
        assert!(!is_disposable("notmailinator.com"));
    }

    #[test]
    fn settings_round_trip_and_clamp() {
        let s = FraudSettings {
            enabled: true,
            hold_score: 70,
            block_disposable: false,
            max_orders_per_ip: 2,
            max_clients_per_ip: 0,
            max_orders_per_client: 9,
        };
        let map: BTreeMap<String, Value> = s
            .to_pairs()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        assert_eq!(FraudSettings::from_map(&map), s);
        let mut bad = map.clone();
        bad.insert("fraud.hold_score".into(), json!(-5));
        assert_eq!(FraudSettings::from_map(&bad).hold_score, 1);
        assert!(
            !FraudSettings::from_map(&BTreeMap::new()).enabled,
            "off by default"
        );
    }
}
