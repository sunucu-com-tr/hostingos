//! Custom-domain verification and the reverse-proxy TLS allow-list.
//!
//! A tenant's custom domain is verified when one of these DNS records exists
//! (checked with `hickory-resolver`):
//!
//! * `TXT _hostingos.<domain>` = `hostingos-verify=<verification_token>`;
//! * `CNAME <domain>` → `<slug>.<HOS_BASE_DOMAIN>` (or any host under it);
//! * `A`/`AAAA <domain>` → one of the addresses of `<slug>.<HOS_BASE_DOMAIN>`
//!   or of `HOS_PUBLIC_IPS` (comma separated).
//!
//! [`verify_domain`] runs one check (platform console button);
//! [`check_due_domains`] is the periodic retry run by the maintenance loop
//! (`crate::worker`). TLS certificates are issued by the reverse proxy (Caddy
//! on-demand TLS), which asks [`tls_allow`] (`GET /internal/tls-allow?domain=`)
//! before issuing: `200` only for tenant subdomains and verified custom
//! domains. The endpoint answers loopback peers (without proxy headers) or
//! callers presenting `HOS_TLS_ASK_SECRET` (`?secret=` or `X-HostingOS-Secret`).

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use axum::Extension;
use axum::extract::{ConnectInfo, Query, State};
use axum::response::{IntoResponse, Response};
use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::{RData, RecordType};
use hos_db::repo::saas::{self, DomainCheckRow};
use http::{HeaderMap, StatusCode};
use serde::Deserialize;

use crate::error::AppResult;
use crate::state::AppState;

/// Label of the TXT verification record.
pub const TXT_LABEL: &str = "_hostingos";
/// Prefix of the TXT verification value.
pub const TXT_VALUE_PREFIX: &str = "hostingos-verify=";
/// Minimum time between two automatic checks of one domain.
pub const RECHECK_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// `_hostingos.<host>`.
pub fn txt_name(host: &str) -> String {
    format!("{TXT_LABEL}.{host}")
}

/// `hostingos-verify=<token>`.
pub fn txt_value(token: &str) -> String {
    format!("{TXT_VALUE_PREFIX}{token}")
}

fn resolver() -> Option<&'static TokioResolver> {
    static RESOLVER: OnceLock<Option<TokioResolver>> = OnceLock::new();
    RESOLVER
        .get_or_init(
            || match TokioResolver::builder_tokio().and_then(|b| b.build()) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(error = %e, "DNS resolver unavailable");
                    None
                }
            },
        )
        .as_ref()
}

fn fqdn(name: &str) -> String {
    format!("{}.", name.trim_end_matches('.'))
}

async fn txt_records(r: &TokioResolver, name: &str) -> Vec<String> {
    match r.lookup(fqdn(name), RecordType::TXT).await {
        Ok(lookup) => lookup
            .answers()
            .iter()
            .filter_map(|rec| match &rec.data {
                RData::TXT(txt) => Some(
                    txt.txt_data
                        .iter()
                        .map(|part| String::from_utf8_lossy(part).into_owned())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

async fn cname_targets(r: &TokioResolver, name: &str) -> Vec<String> {
    match r.lookup(fqdn(name), RecordType::CNAME).await {
        Ok(lookup) => lookup
            .answers()
            .iter()
            .filter_map(|rec| match &rec.data {
                RData::CNAME(c) => Some(c.0.to_ascii().trim_end_matches('.').to_lowercase()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

async fn addresses(r: &TokioResolver, name: &str) -> HashSet<IpAddr> {
    match r.lookup_ip(fqdn(name)).await {
        Ok(ips) => ips.iter().collect(),
        Err(_) => HashSet::new(),
    }
}

/// `HOS_PUBLIC_IPS` (comma separated addresses of the platform's edge).
pub fn public_ips() -> HashSet<IpAddr> {
    std::env::var("HOS_PUBLIC_IPS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

/// Outcome of a DNS check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsCheck {
    /// Verified through `txt`, `cname` or `a`.
    Verified(&'static str),
    /// Not (yet) verifiable; the reason is shown to the operator.
    Failed(String),
}

/// Checks the DNS records of `host` (see module docs).
pub async fn check_dns(base_domain: &str, slug: &str, host: &str, token: &str) -> DnsCheck {
    // The public demo makes no outbound connections, DNS queries included.
    if hos_core::net::is_offline() {
        return DnsCheck::Failed("DNS lookups are disabled on this server (demo mode)".into());
    }
    let Some(r) = resolver() else {
        return DnsCheck::Failed("DNS resolver unavailable".into());
    };
    let expected = txt_value(token);
    if txt_records(r, &txt_name(host))
        .await
        .iter()
        .any(|t| t.trim() == expected)
    {
        return DnsCheck::Verified("txt");
    }
    let base = base_domain.trim_matches('.').to_lowercase();
    let tenant_host = format!("{slug}.{base}");
    if cname_targets(r, host)
        .await
        .iter()
        .any(|t| *t == tenant_host || *t == base || t.ends_with(&format!(".{base}")))
    {
        return DnsCheck::Verified("cname");
    }
    let own = addresses(r, host).await;
    if !own.is_empty() {
        let mut targets = public_ips();
        targets.extend(addresses(r, &tenant_host).await);
        if own.iter().any(|ip| targets.contains(ip)) {
            return DnsCheck::Verified("a");
        }
    }
    DnsCheck::Failed(format!(
        "no TXT {} = {expected}, CNAME → {tenant_host} or matching A/AAAA record found",
        txt_name(host)
    ))
}

/// Runs one check for `row` and records it; `true` when verified.
pub async fn verify_domain(state: &AppState, row: &DomainCheckRow) -> AppResult<bool> {
    let token = row.domain.verification_token.clone().unwrap_or_default();
    let outcome = check_dns(
        &state.config.base_domain,
        &row.tenant_slug,
        &row.domain.host,
        &token,
    )
    .await;
    let ok = matches!(outcome, DnsCheck::Verified(_));
    let error = match &outcome {
        DnsCheck::Verified(_) => None,
        DnsCheck::Failed(msg) => Some(msg.as_str()),
    };
    saas::record_domain_check(&state.pool, row.domain.id, ok, error).await?;
    if ok {
        tracing::info!(host = %row.domain.host, tenant = %row.tenant_slug, method = ?outcome, "custom domain verified");
        state.tenants.invalidate_all();
    }
    Ok(ok)
}

/// Checks up to `limit` unverified custom domains not checked recently.
pub async fn check_due_domains(state: &AppState, limit: i64) -> AppResult<usize> {
    let due = saas::domains_due_for_check(&state.pool, RECHECK_AFTER, limit).await?;
    for row in &due {
        if let Err(e) = verify_domain(state, row).await {
            tracing::warn!(host = %row.domain.host, error = %e, "domain check failed");
        }
    }
    Ok(due.len())
}

/// Query of [`tls_allow`].
#[derive(Debug, Default, Deserialize)]
pub struct TlsAllowQuery {
    #[serde(default)]
    domain: String,
    #[serde(default)]
    secret: Option<String>,
}

fn ask_authorized(peer: Option<IpAddr>, headers: &HeaderMap, secret: Option<&str>) -> bool {
    let configured = std::env::var("HOS_TLS_ASK_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if let Some(expected) = configured {
        let given = headers
            .get("x-hostingos-secret")
            .and_then(|v| v.to_str().ok())
            .or(secret)
            .unwrap_or("");
        if hos_core::crypto::constant_time_eq(given.as_bytes(), expected.trim().as_bytes()) {
            return true;
        }
    }
    // A loopback peer that did not come through the proxy.
    peer.is_some_and(|ip| ip.is_loopback())
        && !headers.contains_key("x-forwarded-for")
        && !headers.contains_key("forwarded")
}

/// `GET /internal/tls-allow?domain=<host>` — `200` when a certificate may be
/// issued for `host`, `404` otherwise, `403` for unauthorized callers.
pub async fn tls_allow(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Query(q): Query<TlsAllowQuery>,
) -> Response {
    let peer_ip = peer.map(|Extension(ConnectInfo(addr))| addr.ip());
    // Tests and in-process calls have no peer: treat as loopback.
    let peer_ip = peer_ip.or(Some(IpAddr::from([127, 0, 0, 1])));
    if !ask_authorized(peer_ip, &headers, q.secret.as_deref()) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let host = hos_db::repo::platform::normalize_host(&q.domain);
    if host.is_empty() || host.len() > 253 {
        return (StatusCode::BAD_REQUEST, "domain required").into_response();
    }
    let platform_host = state
        .config
        .base_url
        .host_str()
        .map(str::to_lowercase)
        .unwrap_or_default();
    if host == platform_host || host == state.config.base_domain.to_lowercase() {
        return (StatusCode::OK, "ok").into_response();
    }
    match saas::tls_allowed(&state.pool, &host).await {
        Ok(true) => (StatusCode::OK, "ok").into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "unknown domain").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "tls allow lookup failed");
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn record_names() {
        assert_eq!(txt_name("shop.example.com"), "_hostingos.shop.example.com");
        assert_eq!(txt_value("abc"), "hostingos-verify=abc");
    }

    #[test]
    fn ask_authorization() {
        let mut h = HeaderMap::new();
        assert!(ask_authorized(Some(IpAddr::from([127, 0, 0, 1])), &h, None));
        assert!(!ask_authorized(Some(IpAddr::from([10, 0, 0, 1])), &h, None));
        h.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.5"));
        assert!(!ask_authorized(
            Some(IpAddr::from([127, 0, 0, 1])),
            &h,
            None
        ));
    }
}
