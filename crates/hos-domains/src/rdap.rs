//! RDAP (RFC 9082/9083) availability and registration lookups.
//!
//! Used as a registrar-independent fallback for storefront searches of tenants
//! without a registrar integration, and for the public "whois" page.
//!
//! * The RDAP server of a TLD comes from the IANA bootstrap file
//!   (`https://data.iana.org/rdap/dns.json`, RFC 9224), cached in memory
//!   (default 24 h; a stale copy is used when a refresh fails).
//! * `200` → [`RdapResult::Registered`], `404` → [`RdapResult::Available`]
//!   (*probably* available: reserved and premium names also return 404),
//!   anything else → [`RdapResult::Unknown`].
//! * Bootstrap URL and per-TLD servers can be overridden (tests, private
//!   registries).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use hos_core::IntegrationError;
use hos_core::integrations::registrar::DomainAvailability;
use hos_core::net::OutboundPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use url::Url;

use crate::http::{HttpClient, status_error};
use crate::name::DomainName;

/// IANA RDAP bootstrap registry for DNS.
pub const IANA_BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";

/// Default bootstrap cache lifetime.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(24 * 3600);

/// Registration data found via RDAP.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdapRegistration {
    /// Domain (ASCII) as returned by the server.
    pub domain: String,
    /// Sponsoring registrar name.
    pub registrar: Option<String>,
    /// Registrar IANA id.
    pub registrar_iana_id: Option<String>,
    /// Registration date.
    pub created_at: Option<DateTime<Utc>>,
    /// Expiry date.
    pub expires_at: Option<DateTime<Utc>>,
    /// Last change.
    pub updated_at: Option<DateTime<Utc>>,
    /// Nameservers (lower-case).
    pub nameservers: Vec<String>,
    /// RDAP statuses (`client transfer prohibited`, ...).
    pub statuses: Vec<String>,
}

/// Outcome of an RDAP lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RdapResult {
    /// The registry has no object (404) — probably available.
    Available,
    /// Registered.
    Registered(RdapRegistration),
    /// Could not be determined (no RDAP server for the TLD, unexpected status).
    Unknown {
        /// Why.
        reason: String,
    },
}

#[derive(Debug, Clone)]
struct CachedBootstrap {
    fetched_at: Instant,
    servers: Arc<HashMap<String, Url>>,
}

/// RDAP client with a cached IANA bootstrap.
#[derive(Debug, Clone)]
pub struct RdapClient {
    http: HttpClient,
    bootstrap_url: Url,
    cache_ttl: Duration,
    overrides: HashMap<String, Url>,
    cache: Arc<RwLock<Option<CachedBootstrap>>>,
    concurrency: usize,
}

impl RdapClient {
    /// Client against the IANA bootstrap with default timeout (15 s).
    pub fn new() -> Result<Self, IntegrationError> {
        Self::with_options(None, Duration::from_secs(15))
    }

    /// Client with a custom bootstrap URL and timeout, under the process-wide
    /// outbound policy (`HOS_OUTBOUND_POLICY`).
    pub fn with_options(
        bootstrap_url: Option<Url>,
        timeout: Duration,
    ) -> Result<Self, IntegrationError> {
        Self::with_options_and_policy(bootstrap_url, timeout, OutboundPolicy::global())
    }

    /// [`with_options`](Self::with_options) with an explicit outbound policy.
    /// The policy also applies to the RDAP servers named by the bootstrap
    /// file (a tenant-supplied bootstrap could point anywhere).
    pub fn with_options_and_policy(
        bootstrap_url: Option<Url>,
        timeout: Duration,
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        let bootstrap_url = match bootstrap_url {
            Some(u) => u,
            None => Url::parse(IANA_BOOTSTRAP_URL)
                .map_err(|e| IntegrationError::Config(format!("bootstrap url: {e}")))?,
        };
        let http = HttpClient::new("rdap", timeout, &[], policy)?;
        http.check_endpoint(&bootstrap_url)?;
        Ok(Self {
            http,
            bootstrap_url,
            cache_ttl: DEFAULT_CACHE_TTL,
            overrides: HashMap::new(),
            cache: Arc::new(RwLock::new(None)),
            concurrency: 8,
        })
    }

    /// Sets the bootstrap cache lifetime.
    #[must_use]
    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Uses `base` as the RDAP server for `tld` (top-level label), bypassing
    /// the bootstrap.
    #[must_use]
    pub fn server_override(mut self, tld: &str, base: Url) -> Self {
        self.overrides.insert(
            tld.trim_start_matches('.').to_ascii_lowercase(),
            ensure_trailing_slash(base),
        );
        self
    }

    /// Maximum parallel lookups in [`check_availability`](Self::check_availability).
    #[must_use]
    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    /// The RDAP base URL for a top-level label (`tr`, `com`), if any.
    pub async fn server_for(&self, top_level: &str) -> Result<Option<Url>, IntegrationError> {
        let key = top_level.trim_start_matches('.').to_ascii_lowercase();
        if let Some(u) = self.overrides.get(&key) {
            return Ok(Some(u.clone()));
        }
        let servers = self.bootstrap().await?;
        Ok(servers.get(&key).cloned())
    }

    async fn bootstrap(&self) -> Result<Arc<HashMap<String, Url>>, IntegrationError> {
        {
            let guard = self.cache.read().await;
            if let Some(c) = guard.as_ref()
                && c.fetched_at.elapsed() < self.cache_ttl
            {
                return Ok(c.servers.clone());
            }
        }
        let mut guard = self.cache.write().await;
        if let Some(c) = guard.as_ref()
            && c.fetched_at.elapsed() < self.cache_ttl
        {
            return Ok(c.servers.clone());
        }
        match self.fetch_bootstrap().await {
            Ok(servers) => {
                let servers = Arc::new(servers);
                *guard = Some(CachedBootstrap {
                    fetched_at: Instant::now(),
                    servers: servers.clone(),
                });
                Ok(servers)
            }
            Err(e) => match guard.as_ref() {
                Some(stale) => {
                    tracing::warn!(error = %e, "RDAP bootstrap refresh failed; using stale copy");
                    Ok(stale.servers.clone())
                }
                None => Err(e),
            },
        }
    }

    async fn fetch_bootstrap(&self) -> Result<HashMap<String, Url>, IntegrationError> {
        let req = self
            .http
            .client()
            .get(self.bootstrap_url.clone())
            .header(reqwest::header::ACCEPT, "application/json");
        let resp = self.http.execute(req, "rdap.bootstrap").await?;
        if resp.status != 200 {
            return Err(status_error(
                resp.status,
                &resp.headers,
                "RDAP bootstrap unavailable",
            ));
        }
        parse_bootstrap(&resp.body)
    }

    /// Looks up one domain. Transport errors and rate limiting are errors; all
    /// other inconclusive outcomes are [`RdapResult::Unknown`].
    pub async fn lookup(&self, domain: &str) -> Result<RdapResult, IntegrationError> {
        let name = DomainName::parse(domain)?;
        let Some(base) = self.server_for(name.top_level()).await? else {
            return Ok(RdapResult::Unknown {
                reason: format!("no RDAP service for .{}", name.top_level()),
            });
        };
        let url = base
            .join(&format!("domain/{}", name.as_str()))
            .map_err(|e| IntegrationError::Config(format!("RDAP url: {e}")))?;
        let req = self.http.client().get(url).header(
            reqwest::header::ACCEPT,
            "application/rdap+json, application/json",
        );
        let resp = self.http.execute(req, "rdap.domain").await?;
        match resp.status {
            200 => {
                let v: Value = serde_json::from_str(&resp.body)
                    .map_err(|e| IntegrationError::Transport(format!("rdap: invalid JSON: {e}")))?;
                Ok(RdapResult::Registered(parse_domain(&v, name.as_str())))
            }
            404 => Ok(RdapResult::Available),
            429 => Err(status_error(429, &resp.headers, "")),
            s => Ok(RdapResult::Unknown {
                reason: format!("RDAP server answered HTTP {s}"),
            }),
        }
    }

    /// Availability for many domains (bounded concurrency), one entry per
    /// input in input order. Inconclusive results have `available = false` and
    /// a `reason`.
    pub async fn check_availability(&self, domains: &[String]) -> Vec<DomainAvailability> {
        let mut out = Vec::with_capacity(domains.len());
        for chunk in domains.chunks(self.concurrency) {
            let mut set = tokio::task::JoinSet::new();
            for (i, d) in chunk.iter().enumerate() {
                let this = self.clone();
                let d = d.clone();
                set.spawn(async move { (i, this.lookup(&d).await) });
            }
            let mut results: Vec<Option<Result<RdapResult, IntegrationError>>> =
                (0..chunk.len()).map(|_| None).collect();
            while let Some(joined) = set.join_next().await {
                if let Ok((i, r)) = joined {
                    results[i] = Some(r);
                }
            }
            for (d, r) in chunk.iter().zip(results) {
                out.push(to_availability(d, r));
            }
        }
        out
    }
}

fn to_availability(
    domain: &str,
    result: Option<Result<RdapResult, IntegrationError>>,
) -> DomainAvailability {
    let normalized = DomainName::parse(domain)
        .map(DomainName::into_string)
        .unwrap_or_else(|_| domain.to_owned());
    let (available, reason) = match result {
        Some(Ok(RdapResult::Available)) => (true, None),
        Some(Ok(RdapResult::Registered(_))) => (false, Some("registered".to_owned())),
        Some(Ok(RdapResult::Unknown { reason })) => (false, Some(reason)),
        Some(Err(e)) => (false, Some(e.to_string())),
        None => (false, Some("lookup aborted".to_owned())),
    };
    DomainAvailability {
        domain: normalized,
        available,
        premium: false,
        register_price: None,
        renew_price: None,
        reason,
    }
}

fn ensure_trailing_slash(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    url
}

/// Parses an RFC 9224 bootstrap file into `tld → base URL` (HTTPS preferred).
pub fn parse_bootstrap(body: &str) -> Result<HashMap<String, Url>, IntegrationError> {
    #[derive(Deserialize)]
    struct Bootstrap {
        services: Vec<(Vec<String>, Vec<String>)>,
    }
    let b: Bootstrap = serde_json::from_str(body)
        .map_err(|e| IntegrationError::Transport(format!("rdap: invalid bootstrap file: {e}")))?;
    let mut map = HashMap::new();
    for (tlds, urls) in b.services {
        let chosen = urls
            .iter()
            .find(|u| u.starts_with("https://"))
            .or_else(|| urls.first())
            .and_then(|u| Url::parse(u).ok())
            .map(ensure_trailing_slash);
        if let Some(url) = chosen {
            for tld in tlds {
                map.insert(tld.to_ascii_lowercase(), url.clone());
            }
        }
    }
    Ok(map)
}

/// Extracts registration data from an RDAP domain object.
pub fn parse_domain(v: &Value, fallback_name: &str) -> RdapRegistration {
    let mut reg = RdapRegistration {
        domain: v
            .get("ldhName")
            .and_then(Value::as_str)
            .map_or_else(|| fallback_name.to_owned(), str::to_ascii_lowercase),
        ..RdapRegistration::default()
    };
    if let Some(events) = v.get("events").and_then(Value::as_array) {
        for e in events {
            let action = e
                .get("eventAction")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let date = e
                .get("eventDate")
                .and_then(Value::as_str)
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                .map(|d| d.with_timezone(&Utc));
            match action {
                "registration" => reg.created_at = date,
                "expiration" => reg.expires_at = date,
                "last changed" => reg.updated_at = date,
                _ => {}
            }
        }
    }
    if let Some(ns) = v.get("nameservers").and_then(Value::as_array) {
        reg.nameservers = ns
            .iter()
            .filter_map(|n| n.get("ldhName").and_then(Value::as_str))
            .map(|s| s.trim_end_matches('.').to_ascii_lowercase())
            .collect();
    }
    if let Some(st) = v.get("status").and_then(Value::as_array) {
        reg.statuses = st
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    if let Some(entities) = v.get("entities").and_then(Value::as_array) {
        for ent in entities {
            let is_registrar = ent
                .get("roles")
                .and_then(Value::as_array)
                .is_some_and(|r| r.iter().any(|x| x.as_str() == Some("registrar")));
            if !is_registrar {
                continue;
            }
            reg.registrar = vcard_fn(ent);
            reg.registrar_iana_id = ent
                .get("publicIds")
                .and_then(Value::as_array)
                .and_then(|ids| {
                    ids.iter().find(|id| {
                        id.get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| t.eq_ignore_ascii_case("IANA Registrar ID"))
                    })
                })
                .and_then(|id| id.get("identifier"))
                .and_then(|i| match i {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .or_else(|| ent.get("handle").and_then(Value::as_str).map(str::to_owned));
            break;
        }
    }
    reg
}

/// `fn` property of an entity's jCard (`["vcard", [["fn", {}, "text", "Name"], ...]]`).
fn vcard_fn(entity: &Value) -> Option<String> {
    let props = entity.get("vcardArray")?.as_array()?.get(1)?.as_array()?;
    props.iter().find_map(|p| {
        let p = p.as_array()?;
        (p.first()?.as_str()? == "fn")
            .then(|| p.get(3)?.as_str().map(str::to_owned))
            .flatten()
            .filter(|s| !s.trim().is_empty())
    })
}

/// `integrations.kind` of the RDAP-only fallback registrar.
pub const KIND: &str = "rdap";

/// A read-only [`DomainRegistrar`] backed by RDAP: availability and public
/// registration data only; every mutating call is `Unsupported`. Lets tenants
/// without a registrar account run storefront searches through the same trait.
#[derive(Debug, Clone)]
pub struct RdapRegistrar {
    client: RdapClient,
}

impl RdapRegistrar {
    /// Wraps a client.
    pub fn new(client: RdapClient) -> Self {
        Self { client }
    }

    /// Builds from integration config (`bootstrap_url`, `timeout_secs`).
    pub fn from_config(config: &Value) -> Result<Self, IntegrationError> {
        Self::from_config_with_policy(config, OutboundPolicy::global())
    }

    /// [`from_config`](Self::from_config) with an explicit outbound policy.
    pub fn from_config_with_policy(
        config: &Value,
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        let r = hos_core::integrations::FieldReader::config(config);
        let timeout = Duration::from_secs(r.u64_or("timeout_secs", 15)?);
        Ok(Self::new(RdapClient::with_options_and_policy(
            r.opt_url("bootstrap_url")?,
            timeout,
            policy,
        )?))
    }

    /// The wrapped client.
    pub fn client(&self) -> &RdapClient {
        &self.client
    }
}

#[async_trait::async_trait]
impl hos_core::integrations::DomainRegistrar for RdapRegistrar {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn test_connection(
        &self,
    ) -> Result<hos_core::integrations::ConnectionInfo, IntegrationError> {
        let com = self.client.server_for("com").await?;
        Ok(hos_core::integrations::ConnectionInfo {
            server_version: Some("RDAP (RFC 9082/9083)".into()),
            details: com.map(|u| format!(".com RDAP server: {u}")),
        })
    }

    async fn check_availability(
        &self,
        domains: &[String],
    ) -> Result<Vec<DomainAvailability>, IntegrationError> {
        Ok(self.client.check_availability(domains).await)
    }

    async fn tld_pricing(
        &self,
        _tlds: &[String],
    ) -> Result<Vec<hos_core::integrations::registrar::TldPrice>, IntegrationError> {
        Err(IntegrationError::Unsupported("tld_pricing"))
    }

    async fn register(
        &self,
        _request: &hos_core::integrations::registrar::DomainRegistration,
    ) -> Result<hos_core::integrations::registrar::DomainOperation, IntegrationError> {
        Err(IntegrationError::Unsupported("register"))
    }

    async fn transfer(
        &self,
        _request: &hos_core::integrations::registrar::DomainTransfer,
    ) -> Result<hos_core::integrations::registrar::DomainOperation, IntegrationError> {
        Err(IntegrationError::Unsupported("transfer"))
    }

    async fn renew(
        &self,
        _request: &hos_core::integrations::registrar::DomainRenewal,
    ) -> Result<hos_core::integrations::registrar::DomainOperation, IntegrationError> {
        Err(IntegrationError::Unsupported("renew"))
    }

    /// Public registration data (no registrar-private fields).
    async fn info(
        &self,
        domain: &str,
    ) -> Result<hos_core::integrations::registrar::DomainInfo, IntegrationError> {
        match self.client.lookup(domain).await? {
            RdapResult::Registered(r) => Ok(hos_core::integrations::registrar::DomainInfo {
                domain: r.domain.clone(),
                status: hos_core::domain::DomainStatus::Active,
                epp_statuses: r.statuses.clone(),
                registrar_ref: None,
                created_at: r.created_at,
                expires_at: r.expires_at,
                nameservers: r.nameservers.clone(),
                locked: Some(r.statuses.iter().any(|s| s.contains("transfer prohibited"))),
                privacy: None,
                auto_renew: None,
                contacts: None,
                extra: serde_json::json!({
                    "registrar": r.registrar,
                    "registrar_iana_id": r.registrar_iana_id,
                    "source": "rdap",
                }),
            }),
            RdapResult::Available => Err(IntegrationError::NotFound(format!(
                "{domain} is not registered"
            ))),
            RdapResult::Unknown { reason } => Err(IntegrationError::remote(None, reason)),
        }
    }

    async fn get_nameservers(&self, domain: &str) -> Result<Vec<String>, IntegrationError> {
        Ok(self.info(domain).await?.nameservers)
    }

    async fn set_nameservers(
        &self,
        _domain: &str,
        _nameservers: &[String],
    ) -> Result<(), IntegrationError> {
        Err(IntegrationError::Unsupported("set_nameservers"))
    }

    async fn set_lock(&self, _domain: &str, _locked: bool) -> Result<(), IntegrationError> {
        Err(IntegrationError::Unsupported("set_lock"))
    }

    async fn get_auth_code(
        &self,
        _domain: &str,
    ) -> Result<hos_core::secret::SecretString, IntegrationError> {
        Err(IntegrationError::Unsupported("get_auth_code"))
    }

    async fn set_privacy(&self, _domain: &str, _enabled: bool) -> Result<(), IntegrationError> {
        Err(IntegrationError::Unsupported("set_privacy"))
    }

    async fn update_contacts(
        &self,
        _domain: &str,
        _contacts: &hos_core::integrations::registrar::DomainContacts,
    ) -> Result<(), IntegrationError> {
        Err(IntegrationError::Unsupported("update_contacts"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bootstrap_parsing_prefers_https() {
        let body = json!({
            "version": "1.0",
            "services": [
                [["com", "net"], ["http://rdap.verisign.com/com/v1", "https://rdap.verisign.com/com/v1/"]],
                [["org"], ["https://rdap.publicinterestregistry.org/rdap"]]
            ]
        })
        .to_string();
        let map = parse_bootstrap(&body).unwrap();
        assert_eq!(map["com"].as_str(), "https://rdap.verisign.com/com/v1/");
        assert_eq!(
            map["org"].as_str(),
            "https://rdap.publicinterestregistry.org/rdap/"
        );
        assert!(parse_bootstrap("{}").is_err());
    }

    #[test]
    fn domain_object_parsing() {
        let v = json!({
            "objectClassName": "domain",
            "ldhName": "EXAMPLE.COM",
            "status": ["client transfer prohibited"],
            "events": [
                {"eventAction": "registration", "eventDate": "1995-08-14T04:00:00Z"},
                {"eventAction": "expiration", "eventDate": "2026-08-13T04:00:00Z"}
            ],
            "nameservers": [{"ldhName": "A.IANA-SERVERS.NET"}, {"ldhName": "b.iana-servers.net."}],
            "entities": [{
                "roles": ["registrar"],
                "publicIds": [{"type": "IANA Registrar ID", "identifier": "376"}],
                "vcardArray": ["vcard", [["version", {}, "text", "4.0"], ["fn", {}, "text", "RESERVED-Internet Assigned Numbers Authority"]]]
            }]
        });
        let r = parse_domain(&v, "example.com");
        assert_eq!(r.domain, "example.com");
        assert_eq!(
            r.registrar.as_deref(),
            Some("RESERVED-Internet Assigned Numbers Authority")
        );
        assert_eq!(r.registrar_iana_id.as_deref(), Some("376"));
        assert_eq!(
            r.nameservers,
            vec!["a.iana-servers.net", "b.iana-servers.net"]
        );
        assert_eq!(
            r.expires_at.unwrap().to_rfc3339(),
            "2026-08-13T04:00:00+00:00"
        );
        assert_eq!(r.statuses, vec!["client transfer prohibited"]);
    }
}
