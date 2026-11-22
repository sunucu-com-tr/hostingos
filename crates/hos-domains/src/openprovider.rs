//! OpenProvider REST API v1beta (`https://api.openprovider.eu/v1beta`, sandbox
//! `https://api.sandbox.openprovider.nl/v1beta`).
//!
//! Reference: OpenAPI spec `github.com/openprovider/api-documentation`
//! (`swagger.json`, rendered at <https://docs.openprovider.com/doc/all>).
//!
//! * `POST /auth/login` returns a bearer token valid for 48 h; it is cached for
//!   12 h and refreshed early, and once more when a call answers code `196`
//!   (authentication failed / token expired) or HTTP 401.
//! * Every response is `{code, desc, data, warnings}`; `code != 0` is an error
//!   regardless of the HTTP status and is mapped by code (196/10005/10006 →
//!   `Auth`, 320 → `NotFound`, 311/346 → `Conflict`, 4007 → `RateLimited`, ...).
//! * Domains are addressed by numeric id; ids are resolved with
//!   `GET /domains?full_name=` and cached in memory.
//! * Contacts are *customer handles*: an identical existing handle (searched by
//!   e-mail) is reused, otherwise `POST /customers` creates one.
//! * DNS zones: records have no ids (id = `TYPE|name|value|prio`), names are
//!   written zone-relative and read back fully qualified; only the TTLs 900,
//!   3600, 10800, 21600, 43200 and 86400 are accepted.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Months, NaiveDate, NaiveTime, TimeZone, Utc};
use hos_core::IntegrationError;
use hos_core::domain::DomainStatus;
use hos_core::integrations::registrar::{
    ContactInfo, DnsRecord, DnsRecordType, DomainAvailability, DomainContacts, DomainInfo,
    DomainOperation, DomainOperationStatus, DomainRegistrar, DomainRegistration, DomainRenewal,
    DomainTransfer, TldPrice,
};
use hos_core::integrations::{ConnectionInfo, FieldReader, kinds};
use hos_core::money::Money;
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::Method;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use url::Url;

use crate::common;
use crate::http::{HttpClient, truncate};
use crate::name::DomainName;
use crate::tld_rules;

/// Production endpoint.
pub const LIVE_URL: &str = "https://api.openprovider.eu/v1beta/";
/// Sandbox endpoint (separate sandbox account).
pub const SANDBOX_URL: &str = "https://api.sandbox.openprovider.nl/v1beta/";

/// TTLs accepted by OpenProvider DNS.
const ALLOWED_TTLS: &[u32] = &[900, 3600, 10800, 21600, 43200, 86400];

/// Connection settings.
#[derive(Debug, Clone)]
pub struct OpenProviderConfig {
    /// API base (ends with `/v1beta/`).
    pub base_url: Url,
    /// API username.
    pub username: String,
    /// API password.
    pub password: SecretString,
    /// Request timeout.
    pub timeout: Duration,
    /// How long a token is reused before logging in again.
    pub token_ttl: Duration,
    /// Nameserver group used when a registration has no nameservers.
    pub ns_group: Option<String>,
    /// Nameservers used when a registration has none (preferred over `ns_group`).
    pub default_nameservers: Vec<String>,
    /// Which destinations the client may contact (default: the process-wide
    /// `HOS_OUTBOUND_POLICY`; tests against a local mock server use
    /// [`OutboundPolicy::Unrestricted`]).
    pub outbound: OutboundPolicy,
}

impl OpenProviderConfig {
    /// Minimal config.
    pub fn new(base_url: Url, username: impl Into<String>, password: SecretString) -> Self {
        Self {
            base_url,
            username: username.into(),
            password,
            outbound: OutboundPolicy::global(),
            timeout: crate::http::DEFAULT_TIMEOUT,
            token_ttl: Duration::from_secs(12 * 3600),
            ns_group: None,
            default_nameservers: Vec::new(),
        }
    }

    /// Reads `integrations.config` / `secrets` (see [`crate::descriptors`]).
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let sandbox = c.bool_or("sandbox", false)?;
        let base_url = match c.opt_url("base_url")? {
            Some(u) => u,
            None => Url::parse(if sandbox { SANDBOX_URL } else { LIVE_URL })
                .map_err(|e| IntegrationError::Config(e.to_string()))?,
        };
        let mut cfg = Self::new(
            ensure_slash(base_url),
            c.str("username")?,
            s.secret("password")?,
        );
        cfg.timeout = Duration::from_secs(c.u64_or("timeout_secs", 30)?);
        cfg.ns_group = c.opt_str("ns_group")?.map(str::to_owned);
        cfg.default_nameservers = common::nameservers(&common::split_list(
            c.opt_str("default_nameservers")?,
        ))
        .map_err(|e| IntegrationError::Config(format!("config.default_nameservers: {e}")))?;
        Ok(cfg)
    }
}

fn ensure_slash(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    url
}

#[derive(Debug)]
struct Token {
    value: SecretString,
    obtained: Instant,
}

/// OpenProvider registrar. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct OpenProvider {
    cfg: OpenProviderConfig,
    http: HttpClient,
    token: Arc<Mutex<Option<Token>>>,
    ids: Arc<std::sync::Mutex<HashMap<String, i64>>>,
}

impl OpenProvider {
    /// Builds the client (no network call).
    pub fn new(cfg: OpenProviderConfig) -> Result<Self, IntegrationError> {
        let http = HttpClient::new("openprovider", cfg.timeout, &[&cfg.password], cfg.outbound)?;
        http.check_endpoint(&cfg.base_url)?;
        Ok(Self {
            cfg,
            http,
            token: Arc::new(Mutex::new(None)),
            ids: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    fn url(&self, path: &str) -> Result<Url, IntegrationError> {
        self.cfg
            .base_url
            .join(path.trim_start_matches('/'))
            .map_err(|e| IntegrationError::Config(format!("invalid OpenProvider URL: {e}")))
    }

    /// `POST /auth/login`.
    async fn login(&self) -> Result<SecretString, IntegrationError> {
        let body = json!({
            "username": self.cfg.username,
            "password": self.cfg.password.expose_str(),
            "ip": "0.0.0.0",
        });
        let req = self.http.client().post(self.url("auth/login")?).json(&body);
        let resp = self.http.execute(req, "auth.login").await?;
        let data = self.parse(resp.status, &resp.headers, &resp.body, &[])?;
        let token = data
            .get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| IntegrationError::Auth("OpenProvider login returned no token".into()))?;
        Ok(SecretString::from(token))
    }

    async fn token(&self, force: bool) -> Result<SecretString, IntegrationError> {
        let mut guard = self.token.lock().await;
        if !force
            && let Some(t) = guard.as_ref()
            && t.obtained.elapsed() < self.cfg.token_ttl
        {
            return Ok(t.value.clone());
        }
        let value = self.login().await?;
        *guard = Some(Token {
            value: value.clone(),
            obtained: Instant::now(),
        });
        Ok(value)
    }

    /// Authenticated call; returns `data`. Re-logs in once on code 196 / 401.
    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        op: &'static str,
        extra_secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        let mut retried = false;
        loop {
            let token = self.token(retried).await?;
            let mut req = self
                .http
                .client()
                .request(method.clone(), self.url(path)?)
                .bearer_auth(token.expose_str());
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = self.http.execute(req, op).await?;
            match self.parse(resp.status, &resp.headers, &resp.body, extra_secrets) {
                Err(IntegrationError::Auth(_))
                    if !retried && is_token_problem(resp.status, &resp.body) =>
                {
                    retried = true;
                }
                other => return other,
            }
        }
    }

    fn parse(
        &self,
        status: u16,
        headers: &reqwest::header::HeaderMap,
        body: &str,
        extra_secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        let json: Option<Value> = serde_json::from_str(body).ok();
        if let Some(v) = &json
            && let Some(code) = v.get("code").and_then(Value::as_i64)
        {
            if code == 0 && (200..300).contains(&status) {
                return Ok(v.get("data").cloned().unwrap_or(Value::Null));
            }
            if code != 0 {
                let desc = v
                    .get("desc")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_owned();
                let detail = v
                    .get("data")
                    .filter(|d| !d.is_null())
                    .map(|d| format!(" ({})", truncate(&d.to_string(), 200)))
                    .unwrap_or_default();
                return Err(map_code(
                    code,
                    &self
                        .http
                        .scrub_with(&format!("{desc}{detail}"), extra_secrets),
                    status,
                ));
            }
        }
        if status == 401 || status == 403 {
            return Err(IntegrationError::Auth(format!(
                "OpenProvider rejected the credentials (HTTP {status})"
            )));
        }
        if !(200..300).contains(&status) {
            return Err(crate::http::status_error(
                status,
                headers,
                crate::http::describe_body(body),
            ));
        }
        json.map(|v| v.get("data").cloned().unwrap_or(v))
            .ok_or_else(|| IntegrationError::Transport("openprovider: response is not JSON".into()))
    }

    async fn get(
        &self,
        path: &str,
        query: &[(&str, String)],
        op: &'static str,
    ) -> Result<Value, IntegrationError> {
        self.call(Method::GET, path, query, None, op, &[]).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: &Value,
        op: &'static str,
    ) -> Result<Value, IntegrationError> {
        self.call(method, path, &[], Some(body), op, &[]).await
    }

    /// Domain id via `GET /domains?full_name=` (cached).
    async fn domain_id(&self, name: &DomainName) -> Result<i64, IntegrationError> {
        if let Some(id) = self.cached_id(name) {
            return Ok(id);
        }
        let data = self
            .get(
                "domains",
                &[
                    ("full_name", name.as_str().to_owned()),
                    ("limit", "1".into()),
                ],
                "domains.list",
            )
            .await?;
        let id = data
            .get("results")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
            .and_then(|d| d.get("id"))
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                IntegrationError::NotFound(format!("{name} is not in this OpenProvider account"))
            })?;
        if let Ok(mut ids) = self.ids.lock() {
            ids.insert(name.as_str().to_owned(), id);
        }
        Ok(id)
    }

    fn cached_id(&self, name: &DomainName) -> Option<i64> {
        self.ids.lock().ok()?.get(name.as_str()).copied()
    }

    async fn domain(&self, name: &DomainName) -> Result<(i64, Value), IntegrationError> {
        let id = self.domain_id(name).await?;
        match self.get(&format!("domains/{id}"), &[], "domains.get").await {
            Err(IntegrationError::NotFound(m)) => {
                if let Ok(mut ids) = self.ids.lock() {
                    ids.remove(name.as_str());
                }
                Err(IntegrationError::NotFound(m))
            }
            other => other.map(|v| (id, v)),
        }
    }

    // ------------------------------------------------------------------
    // customers (handles)
    // ------------------------------------------------------------------

    async fn handle_for(
        &self,
        contact: &ContactInfo,
        tld: &str,
        cache: &mut Vec<(ContactInfo, String)>,
    ) -> Result<String, IntegrationError> {
        if let Some((_, h)) = cache.iter().find(|(c, _)| c == contact) {
            return Ok(h.clone());
        }
        let body = customer_body(contact, tld);
        let found = self
            .get(
                "customers",
                &[
                    ("email_pattern", contact.email.trim().to_owned()),
                    ("limit", "100".into()),
                ],
                "customers.list",
            )
            .await?;
        let wanted = fingerprint(&body);
        let existing = found
            .get("results")
            .and_then(Value::as_array)
            .and_then(|rows| rows.iter().find(|r| fingerprint(r) == wanted))
            .and_then(|r| r.get("handle").and_then(Value::as_str).map(str::to_owned));
        let handle = match existing {
            Some(h) => h,
            None => {
                let data = self
                    .send(Method::POST, "customers", &body, "customers.create")
                    .await?;
                data.get("handle")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        IntegrationError::Transport(
                            "openprovider: customer created without handle".into(),
                        )
                    })?
            }
        };
        cache.push((contact.clone(), handle.clone()));
        Ok(handle)
    }

    async fn handles(
        &self,
        contacts: &DomainContacts,
        tld: &str,
    ) -> Result<[String; 4], IntegrationError> {
        let mut cache = Vec::new();
        let mut out: [String; 4] = Default::default();
        for (i, (role, c)) in [
            ("registrant", &contacts.registrant),
            ("admin", &contacts.admin),
            ("tech", &contacts.tech),
            ("billing", &contacts.billing),
        ]
        .into_iter()
        .enumerate()
        {
            common::validate_contact(role, c)?;
            out[i] = self.handle_for(c, tld, &mut cache).await?;
        }
        Ok(out)
    }

    fn ns_body(&self, ns: &[String]) -> Option<(&'static str, Value)> {
        if !ns.is_empty() {
            return Some(("name_servers", ns_list(ns)));
        }
        if !self.cfg.default_nameservers.is_empty() {
            return Some(("name_servers", ns_list(&self.cfg.default_nameservers)));
        }
        self.cfg
            .ns_group
            .as_ref()
            .map(|g| ("ns_group", Value::String(g.clone())))
    }

    // ------------------------------------------------------------------
    // DNS
    // ------------------------------------------------------------------

    async fn zone_records(
        &self,
        name: &DomainName,
    ) -> Result<Option<Vec<Value>>, IntegrationError> {
        match self
            .get(
                &format!("dns/zones/{}", name.as_str()),
                &[("with_records", "true".into())],
                "dns.zone_get",
            )
            .await
        {
            Ok(data) => Ok(Some(
                data.get("records")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            )),
            Err(IntegrationError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn update_zone(&self, name: &DomainName, changes: Value) -> Result<(), IntegrationError> {
        let body = json!({ "name": name.as_str(), "records": changes });
        self.send(
            Method::PUT,
            &format!("dns/zones/{}", name.as_str()),
            &body,
            "dns.zone_update",
        )
        .await
        .map(|_| ())
    }
}

// ----------------------------------------------------------------------
// pure helpers
// ----------------------------------------------------------------------

fn is_token_problem(status: u16, body: &str) -> bool {
    status == 401
        || serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("code").and_then(Value::as_i64))
            == Some(196)
}

fn map_code(code: i64, desc: &str, status: u16) -> IntegrationError {
    let msg = format!("{} [{code}]", truncate(desc, 300));
    match code {
        196 => IntegrationError::Auth(format!(
            "{msg} — check the OpenProvider API username/password"
        )),
        10005 => IntegrationError::Auth(format!(
            "{msg} — access denied or IP blocked; check the API IP whitelist in the OpenProvider panel"
        )),
        10006 => IntegrationError::Auth(format!("{msg} — disable 2FA for the API user")),
        320 | 800 | 872 | 176 => IntegrationError::NotFound(msg),
        311 | 346 | 805 | 324 | 25001 | 366 => IntegrationError::Conflict(msg),
        193 | 195 | 331 | 333 | 316 | 318 | 347 | 379 => IntegrationError::InvalidInput(msg),
        4007 => IntegrationError::RateLimited {
            retry_after: Some(Duration::from_secs(300)),
        },
        18002 => IntegrationError::remote_with_code(Some(503), code.to_string(), msg),
        _ => {
            if status >= 500 && code == 0 {
                IntegrationError::remote(Some(status), msg)
            } else {
                IntegrationError::remote_with_code(None, code.to_string(), msg)
            }
        }
    }
}

/// Splits `"Atatürk Cad. 12"` into street and house number (OpenProvider has
/// separate fields). Without a trailing number the whole line is the street.
fn split_street(line1: &str) -> (String, String) {
    let line = line1.trim();
    if let Some((street, last)) = line.rsplit_once(' ')
        && last.chars().next().is_some_and(|c| c.is_ascii_digit())
        && last.len() <= 10
    {
        return (street.trim().to_owned(), last.to_owned());
    }
    (line.to_owned(), String::new())
}

/// `+CC` / area code (first three national digits) / subscriber number.
fn op_phone(country_code: &str, number: &str) -> Value {
    let (area, subscriber) = if number.len() > 7 {
        number.split_at(3)
    } else {
        ("", number)
    };
    json!({
        "country_code": format!("+{country_code}"),
        "area_code": area,
        "subscriber_number": subscriber,
    })
}

fn customer_body(c: &ContactInfo, tld: &str) -> Value {
    let a = &c.address;
    let (street, number) = split_street(&a.line1);
    let street = match a.line2.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(l2) => format!("{street}, {l2}"),
        None => street,
    };
    let mut body = json!({
        "name": { "first_name": c.first_name.trim(), "last_name": c.last_name.trim() },
        "address": {
            "street": street,
            "number": number,
            "zipcode": a.postal_code.clone().unwrap_or_default(),
            "city": a.city.trim(),
            "state": a.state.clone().unwrap_or_default(),
            "country": a.country_code.trim().to_ascii_uppercase(),
        },
        "phone": op_phone(&c.phone.country_code, &c.phone.number),
        "email": c.email.trim(),
    });
    if let Some(company) = c
        .company
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        body["company_name"] = json!(company);
    }
    if let Some(fax) = c.fax.as_ref().filter(|f| !f.number.is_empty()) {
        body["fax"] = op_phone(&fax.country_code, &fax.number);
    }
    if let Some(vat) = c.tax_id.as_deref().filter(|s| !s.trim().is_empty()) {
        body["vat"] = json!(vat.trim());
    }
    // Raw additional data (`extra.attributes`) and `.us` / `.ca` extension data.
    let raw = common::raw_attributes(&c.extra);
    if !raw.is_empty() {
        body["additional_data"] = Value::Object(
            raw.into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        );
    }
    let ext_get = |k: &str| tld_rules::extra_str(&c.extra, k);
    let top = tld.rsplit('.').next().unwrap_or(tld);
    let mut ext = Map::new();
    match top {
        "us" => {
            if let Some(v) = ext_get("us_nexus_category") {
                ext.insert("nexus_category".into(), json!(v.to_ascii_uppercase()));
            }
            if let Some(v) = ext_get("us_purpose") {
                ext.insert("applicant_purpose".into(), json!(v.to_ascii_uppercase()));
            }
        }
        "ca" => {
            if let Some(v) = ext_get("ca_legal_type") {
                ext.insert("legal_type".into(), json!(v.to_ascii_uppercase()));
            }
        }
        _ => {}
    }
    if !ext.is_empty() {
        body["extension_additional_data"] = json!([{ "name": top, "data": ext }]);
    }
    body
}

/// Comparable projection of a customer object (request body or search row).
fn fingerprint(v: &Value) -> Vec<String> {
    let s = |p: &[&str]| -> String {
        let mut cur = v;
        for k in p {
            match cur.get(*k) {
                Some(x) => cur = x,
                None => return String::new(),
            }
        }
        match cur {
            Value::String(s) => s
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    };
    vec![
        s(&["name", "first_name"]),
        s(&["name", "last_name"]),
        s(&["company_name"]),
        s(&["email"]),
        s(&["address", "street"]),
        s(&["address", "number"]),
        s(&["address", "zipcode"]),
        s(&["address", "city"]),
        s(&["address", "country"]),
        s(&["phone", "country_code"]),
        s(&["phone", "area_code"]),
        s(&["phone", "subscriber_number"]),
    ]
}

fn ns_list(ns: &[String]) -> Value {
    Value::Array(
        ns.iter()
            .enumerate()
            .map(|(i, n)| json!({ "name": n, "seq_nr": i + 1 }))
            .collect(),
    )
}

/// `"2019-01-27 0:55:17"` (hours may be unpadded; `0000-00-00…` = none), UTC.
fn op_date(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let s = v?.as_str()?.trim();
    let (date, time) = s.split_once(' ').unwrap_or((s, "00:00:00"));
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let mut parts = time.split(':').map(|p| p.parse::<u32>().ok());
    let t = NaiveTime::from_hms_opt(
        parts.next().flatten().unwrap_or(0),
        parts.next().flatten().unwrap_or(0),
        parts.next().flatten().unwrap_or(0),
    )?;
    Some(Utc.from_utc_datetime(&date.and_time(t)))
}

fn status_from(d: &Value, now: DateTime<Utc>) -> DomainStatus {
    let status = d.get("status").and_then(Value::as_str).unwrap_or_default();
    let transfer = d
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("transfer"));
    let expires = op_date(d.get("expiration_date"));
    match status {
        "ACT" => {
            if expires.is_some_and(|e| e < now) {
                DomainStatus::Grace
            } else {
                DomainStatus::Active
            }
        }
        "REQ" | "SCH" | "PEN" => {
            if transfer {
                DomainStatus::PendingTransfer
            } else {
                DomainStatus::PendingRegistration
            }
        }
        "RRQ" => DomainStatus::Redemption,
        "DEL" => {
            if op_date(d.get("restorable_until")).is_some_and(|r| r > now) {
                DomainStatus::Redemption
            } else {
                DomainStatus::Expired
            }
        }
        "FAI" => DomainStatus::Cancelled,
        _ => DomainStatus::Pending,
    }
}

fn nameservers_of(d: &Value) -> Vec<String> {
    d.get("name_servers")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|n| n.get("name").and_then(Value::as_str))
                .map(|n| n.trim().trim_end_matches('.').to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn price_of(v: Option<&Value>) -> Option<Money> {
    let p = v?.get("reseller").or_else(|| v?.get("product"))?;
    common::money(
        p.get("price").and_then(common::decimal),
        p.get("currency")?.as_str()?,
    )
}

fn tld_price_from(t: &Value) -> Option<TldPrice> {
    let tld = t
        .get("name")?
        .as_str()?
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let prices = t.get("prices")?;
    let rule = tld_rules::rule_for(&tld);
    let period = |k: &str| {
        t.get(k)
            .or_else(|| prices.get(k))
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    Some(TldPrice {
        register: price_of(prices.get("create_price")),
        renew: price_of(prices.get("renew_price")),
        transfer: price_of(prices.get("transfer_price")),
        restore: price_of(prices.get("restore_price")),
        min_years: period("min_period").unwrap_or(rule.min_years),
        max_years: period("max_period").unwrap_or(rule.max_years),
        tld,
    })
}

fn snap_ttl(ttl: Option<u32>) -> u32 {
    let wanted = ttl.unwrap_or(3600);
    ALLOWED_TTLS
        .iter()
        .copied()
        .find(|t| *t >= wanted)
        .unwrap_or(86400)
}

fn relative(name: &DomainName, fqdn: &str) -> String {
    let h = fqdn.trim().trim_end_matches('.').to_ascii_lowercase();
    let d = name.as_str();
    if h.is_empty() || h == d || h == "@" {
        "@".into()
    } else if let Some(p) = h.strip_suffix(&format!(".{d}")) {
        p.to_owned()
    } else {
        h
    }
}

fn record_id(rtype: &str, name: &str, value: &str, prio: Option<u16>) -> String {
    format!(
        "{rtype}|{name}|{}|{value}",
        prio.map(|p| p.to_string()).unwrap_or_default()
    )
}

fn to_record(name: &DomainName, r: &Value) -> Option<DnsRecord> {
    let rtype_str = r.get("type")?.as_str()?.to_ascii_uppercase();
    let record_type: DnsRecordType = rtype_str.parse().ok()?;
    let rel = relative(
        name,
        r.get("name").and_then(Value::as_str).unwrap_or_default(),
    );
    let value = r.get("value")?.as_str()?.to_owned();
    let prio = r
        .get("prio")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0 || matches!(record_type, DnsRecordType::Mx | DnsRecordType::Srv));
    let prio = if matches!(record_type, DnsRecordType::Mx | DnsRecordType::Srv) {
        prio
    } else {
        None
    };
    Some(DnsRecord {
        id: Some(record_id(&rtype_str, &rel, &value, prio)),
        name: rel,
        record_type,
        value,
        ttl: r
            .get("ttl")
            .and_then(Value::as_u64)
            .and_then(|t| u32::try_from(t).ok()),
        priority: prio,
    })
}

/// Zone-relative write form (`""` for the apex).
fn to_op_record(r: &DnsRecord) -> Value {
    let name = if r.name.trim() == "@" {
        ""
    } else {
        r.name.trim()
    };
    let mut v = json!({
        "name": name,
        "type": r.record_type.as_str(),
        "value": r.value.trim(),
        "ttl": snap_ttl(r.ttl),
    });
    if matches!(r.record_type, DnsRecordType::Mx | DnsRecordType::Srv) {
        v["prio"] = json!(r.priority.unwrap_or(10));
    }
    v
}

#[async_trait]
impl DomainRegistrar for OpenProvider {
    fn kind(&self) -> &'static str {
        kinds::OPENPROVIDER
    }

    /// Logs in (forces a fresh token).
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        self.token(true).await?;
        Ok(ConnectionInfo {
            server_version: Some(format!(
                "OpenProvider REST v1beta ({})",
                self.cfg.base_url.host_str().unwrap_or("?")
            )),
            details: Some(format!("authenticated as {}", self.cfg.username)),
        })
    }

    /// `POST /domains/check` (`with_price`), 50 names per call.
    async fn check_availability(
        &self,
        domains: &[String],
    ) -> Result<Vec<DomainAvailability>, IntegrationError> {
        let parsed: Vec<Result<DomainName, String>> = domains
            .iter()
            .map(|d| DomainName::parse(d).map_err(|e| e.to_string()))
            .collect();
        let valid: Vec<&DomainName> = parsed.iter().filter_map(|p| p.as_ref().ok()).collect();
        let mut results: Vec<Value> = Vec::new();
        for chunk in valid.chunks(50) {
            let body = json!({
                "domains": chunk.iter().map(|d| json!({"name": d.sld(), "extension": d.tld()})).collect::<Vec<_>>(),
                "with_price": true,
            });
            let data = self
                .send(Method::POST, "domains/check", &body, "domains.check")
                .await?;
            results.extend(
                data.get("results")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        Ok(parsed
            .into_iter()
            .zip(domains)
            .map(|(p, input)| {
                let mut out = DomainAvailability {
                    domain: input.trim().to_ascii_lowercase(),
                    available: false,
                    premium: false,
                    register_price: None,
                    renew_price: None,
                    reason: None,
                };
                let name = match p {
                    Ok(n) => n,
                    Err(e) => {
                        out.reason = Some(e);
                        return out;
                    }
                };
                out.domain = name.as_str().to_owned();
                let unicode = name.to_unicode();
                let Some(r) = results.iter().find(|r| {
                    r.get("domain").and_then(Value::as_str).is_some_and(|d| {
                        d.eq_ignore_ascii_case(name.as_str()) || d.to_lowercase() == unicode
                    })
                }) else {
                    out.reason = Some("not returned by OpenProvider".into());
                    return out;
                };
                let status = r.get("status").and_then(Value::as_str).unwrap_or_default();
                out.available = status.eq_ignore_ascii_case("free");
                out.premium = r
                    .get("is_premium")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if out.premium {
                    let currency = r
                        .pointer("/premium/currency")
                        .and_then(Value::as_str)
                        .unwrap_or("EUR");
                    out.register_price = common::money(
                        r.pointer("/premium/price/create").and_then(common::decimal),
                        currency,
                    );
                    out.renew_price = common::money(
                        r.pointer("/premium/price/renew").and_then(common::decimal),
                        currency,
                    );
                } else {
                    out.register_price = price_of(r.get("price"));
                }
                if !out.available {
                    let reason = r.get("reason").and_then(Value::as_str).unwrap_or_default();
                    out.reason = Some(if reason.is_empty() {
                        status.to_owned()
                    } else {
                        format!("{status}: {reason}")
                    });
                }
                out
            })
            .collect())
    }

    /// `GET /tlds/{name}?with_price=true` per TLD, or paginated `GET /tlds`.
    async fn tld_pricing(&self, tlds: &[String]) -> Result<Vec<TldPrice>, IntegrationError> {
        let mut out = Vec::new();
        if tlds.is_empty() {
            let mut offset = 0usize;
            loop {
                let data = self
                    .get(
                        "tlds",
                        &[
                            ("with_price", "true".into()),
                            ("limit", "100".into()),
                            ("offset", offset.to_string()),
                        ],
                        "tlds.list",
                    )
                    .await?;
                let results = data
                    .get("results")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let total = data.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                offset += results.len();
                out.extend(results.iter().filter_map(tld_price_from));
                if results.is_empty() || offset >= total || offset > 10_000 {
                    break;
                }
            }
        } else {
            for tld in tlds {
                let tld = tld.trim().trim_start_matches('.').to_ascii_lowercase();
                let data = self
                    .get(
                        &format!("tlds/{tld}"),
                        &[("with_price", "true".into())],
                        "tlds.get",
                    )
                    .await?;
                if let Some(p) = tld_price_from(&data) {
                    out.push(p);
                }
            }
        }
        Ok(out)
    }

    /// Handles, then `POST /domains`.
    async fn register(
        &self,
        request: &DomainRegistration,
    ) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        let rule = tld_rules::rule_for(name.tld());
        rule.validate_registration(request)?;
        let ns = common::nameservers(&request.nameservers)?;
        let (ns_key, ns_value) = self.ns_body(&ns).ok_or_else(|| {
            IntegrationError::InvalidInput(
                "nameservers are required (or configure default_nameservers / ns_group)".into(),
            )
        })?;
        let [owner, admin, tech, billing] = self.handles(&request.contacts, name.tld()).await?;
        let mut body = json!({
            "domain": { "name": name.sld(), "extension": name.tld() },
            "period": request.years,
            "unit": "y",
            "owner_handle": owner,
            "admin_handle": admin,
            "tech_handle": tech,
            "billing_handle": billing,
            "autorenew": if request.auto_renew { "on" } else { "off" },
            "is_private_whois_enabled": request.privacy && rule.privacy_allowed,
        });
        body[ns_key] = ns_value;
        if let Some(p) = &request.premium_price {
            // JSON number from the decimal string (no f64 arithmetic on our side).
            body["accept_premium_fee"] =
                serde_json::from_str::<Value>(&p.round().amount.to_string()).unwrap_or(Value::Null);
        }
        let raw = common::raw_attributes(&request.extra);
        if !raw.is_empty() {
            body["additional_data"] = Value::Object(
                raw.into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect(),
            );
        }
        let data = self
            .send(Method::POST, "domains", &body, "domains.create")
            .await?;
        if let (Some(id), Ok(mut ids)) = (data.get("id").and_then(Value::as_i64), self.ids.lock()) {
            ids.insert(name.as_str().to_owned(), id);
        }
        let status = data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(DomainOperation {
            domain: name.as_str().to_owned(),
            status: if status == "ACT" {
                DomainOperationStatus::Completed
            } else {
                DomainOperationStatus::Pending
            },
            registrar_ref: data.get("id").map(|v| v.to_string()),
            expires_at: op_date(data.get("expiration_date")),
            cost: None,
            message: Some(format!("status {status}")),
        })
    }

    /// Handles, then `POST /domains/transfer`.
    async fn transfer(
        &self,
        request: &DomainTransfer,
    ) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        let rule = tld_rules::rule_for(name.tld());
        let code = request.auth_code.expose_str().trim().to_owned();
        if rule.transfer_requires_auth_code && code.is_empty() {
            return Err(IntegrationError::InvalidInput(format!(
                ".{} transfers require an auth code",
                name.tld()
            )));
        }
        let ns = common::nameservers(&request.nameservers)?;
        let [owner, admin, tech, billing] = self.handles(&request.contacts, name.tld()).await?;
        let mut body = json!({
            "domain": { "name": name.sld(), "extension": name.tld() },
            "auth_code": code,
            "owner_handle": owner,
            "admin_handle": admin,
            "tech_handle": tech,
            "billing_handle": billing,
            "autorenew": if request.auto_renew { "on" } else { "off" },
            "is_private_whois_enabled": request.privacy && rule.privacy_allowed,
        });
        if ns.is_empty() {
            body["import_nameservers_from_registry"] = json!(true);
        } else {
            body["name_servers"] = ns_list(&ns);
        }
        let data = self
            .call(
                Method::POST,
                "domains/transfer",
                &[],
                Some(&body),
                "domains.transfer",
                &[&code],
            )
            .await?;
        let status = data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if status == "FAI" {
            return Err(IntegrationError::remote(
                None,
                format!("OpenProvider reported the transfer of {name} as failed"),
            ));
        }
        Ok(DomainOperation {
            domain: name.as_str().to_owned(),
            status: if status == "ACT" {
                DomainOperationStatus::Completed
            } else {
                DomainOperationStatus::Pending
            },
            registrar_ref: data.get("id").map(|v| v.to_string()),
            expires_at: op_date(data.get("expiration_date")),
            cost: None,
            message: Some(format!("status {status}")),
        })
    }

    /// `POST /domains/{id}/renew`; `POST /domains/{id}/restore` first when the
    /// domain is deleted-but-restorable and `restore_if_needed`.
    async fn renew(&self, request: &DomainRenewal) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        tld_rules::rule_for(name.tld()).validate_years(request.years)?;
        let (id, d) = self.domain(&name).await?;
        let status = status_from(&d, Utc::now());
        if status == DomainStatus::Redemption {
            if !request.restore_if_needed {
                return Err(IntegrationError::Conflict(format!(
                    "{name} is in redemption; a restore is required"
                )));
            }
            let data = self
                .send(
                    Method::POST,
                    &format!("domains/{id}/restore"),
                    &json!({ "id": id }),
                    "domains.restore",
                )
                .await?;
            return Ok(DomainOperation {
                domain: name.as_str().to_owned(),
                status: DomainOperationStatus::Pending,
                registrar_ref: Some(id.to_string()),
                expires_at: None,
                cost: None,
                message: Some(format!(
                    "restore requested (status {})",
                    data.get("status").and_then(Value::as_str).unwrap_or("?")
                )),
            });
        }
        let data = self
            .send(
                Method::POST,
                &format!("domains/{id}/renew"),
                &json!({ "id": id, "period": request.years }),
                "domains.renew",
            )
            .await?;
        let base = request
            .current_expiry
            .or_else(|| op_date(d.get("expiration_date")));
        let new_expiry =
            base.and_then(|b| b.checked_add_months(Months::new(request.years.saturating_mul(12))));
        let st = data.get("status").and_then(Value::as_str).unwrap_or("ACT");
        Ok(DomainOperation {
            domain: name.as_str().to_owned(),
            status: if st == "ACT" {
                DomainOperationStatus::Completed
            } else {
                DomainOperationStatus::Pending
            },
            registrar_ref: Some(id.to_string()),
            expires_at: if st == "ACT" { new_expiry } else { None },
            cost: None,
            message: Some(format!("status {st}")),
        })
    }

    /// `GET /domains/{id}`.
    async fn info(&self, domain: &str) -> Result<DomainInfo, IntegrationError> {
        let name = common::domain(domain)?;
        let (id, d) = self.domain(&name).await?;
        let locked = d.get("is_locked").and_then(Value::as_bool);
        Ok(DomainInfo {
            domain: name.as_str().to_owned(),
            status: status_from(&d, Utc::now()),
            epp_statuses: if locked == Some(true) {
                vec!["clientTransferProhibited".into()]
            } else {
                Vec::new()
            },
            registrar_ref: Some(id.to_string()),
            created_at: op_date(d.get("creation_date")).or_else(|| op_date(d.get("order_date"))),
            expires_at: op_date(d.get("expiration_date")),
            nameservers: nameservers_of(&d),
            locked,
            privacy: d.get("is_private_whois_enabled").and_then(Value::as_bool),
            auto_renew: d
                .get("autorenew")
                .and_then(Value::as_str)
                .map(|a| a == "on"),
            contacts: None,
            extra: json!({
                "status": d.get("status"),
                "renewal_date": d.get("renewal_date"),
                "owner_handle": d.get("owner_handle"),
                "admin_handle": d.get("admin_handle"),
                "tech_handle": d.get("tech_handle"),
                "billing_handle": d.get("billing_handle"),
                "is_lockable": d.get("is_lockable"),
            }),
        })
    }

    async fn get_nameservers(&self, domain: &str) -> Result<Vec<String>, IntegrationError> {
        let name = common::domain(domain)?;
        let (_, d) = self.domain(&name).await?;
        Ok(nameservers_of(&d))
    }

    /// `PUT /domains/{id}` with `name_servers`.
    async fn set_nameservers(
        &self,
        domain: &str,
        nameservers: &[String],
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let ns = common::required_nameservers(nameservers)?;
        let id = self.domain_id(&name).await?;
        self.send(
            Method::PUT,
            &format!("domains/{id}"),
            &json!({ "name_servers": ns_list(&ns) }),
            "domains.update",
        )
        .await
        .map(|_| ())
    }

    /// `PUT /domains/{id}` with `is_locked`.
    async fn set_lock(&self, domain: &str, locked: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let id = self.domain_id(&name).await?;
        self.send(
            Method::PUT,
            &format!("domains/{id}"),
            &json!({ "is_locked": locked }),
            "domains.update",
        )
        .await
        .map(|_| ())
    }

    /// `GET /domains/{id}/authcode`.
    async fn get_auth_code(&self, domain: &str) -> Result<SecretString, IntegrationError> {
        let name = common::domain(domain)?;
        let id = self.domain_id(&name).await?;
        let data = self
            .get(&format!("domains/{id}/authcode"), &[], "domains.authcode")
            .await?;
        data.get("auth_code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(SecretString::from)
            .ok_or_else(|| IntegrationError::NotFound(format!("no auth code available for {name}")))
    }

    /// `PUT /domains/{id}` with `is_private_whois_enabled`.
    async fn set_privacy(&self, domain: &str, enabled: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let id = self.domain_id(&name).await?;
        self.send(
            Method::PUT,
            &format!("domains/{id}"),
            &json!({ "is_private_whois_enabled": enabled }),
            "domains.update",
        )
        .await
        .map(|_| ())
    }

    /// Handles, then `PUT /domains/{id}` with the four handles.
    async fn update_contacts(
        &self,
        domain: &str,
        contacts: &DomainContacts,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let id = self.domain_id(&name).await?;
        let [owner, admin, tech, billing] = self.handles(contacts, name.tld()).await?;
        self.send(
            Method::PUT,
            &format!("domains/{id}"),
            &json!({
                "owner_handle": owner,
                "admin_handle": admin,
                "tech_handle": tech,
                "billing_handle": billing,
            }),
            "domains.update",
        )
        .await
        .map(|_| ())
    }

    /// `GET /dns/zones/{domain}?with_records=true` (SOA omitted).
    async fn list_dns_records(&self, domain: &str) -> Result<Vec<DnsRecord>, IntegrationError> {
        let name = common::domain(domain)?;
        let records = self.zone_records(&name).await?.ok_or_else(|| {
            IntegrationError::NotFound(format!("no DNS zone for {name} at OpenProvider"))
        })?;
        Ok(records.iter().filter_map(|r| to_record(&name, r)).collect())
    }

    /// `PUT /dns/zones/{domain}` (`add` / `update`); creates the zone with
    /// `POST /dns/zones` when it does not exist.
    async fn upsert_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<DnsRecord, IntegrationError> {
        let name = common::domain(domain)?;
        if record.value.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "record value is required".into(),
            ));
        }
        let new = to_op_record(record);
        match self.zone_records(&name).await? {
            None => {
                let body = json!({
                    "domain": { "name": name.sld(), "extension": name.tld() },
                    "type": "master",
                    "records": [new],
                });
                self.send(Method::POST, "dns/zones", &body, "dns.zone_create")
                    .await?;
            }
            Some(records) => {
                let changes = match &record.id {
                    Some(id) => {
                        let original = records
                            .iter()
                            .find(|r| to_record(&name, r).and_then(|x| x.id).as_deref() == Some(id))
                            .ok_or_else(|| {
                                IntegrationError::NotFound(format!("DNS record {id}"))
                            })?;
                        let original = to_record(&name, original)
                            .map(|o| to_op_record(&o))
                            .ok_or_else(|| {
                                IntegrationError::NotFound(format!("DNS record {id}"))
                            })?;
                        json!({ "update": [{ "original_record": original, "record": new }] })
                    }
                    None => json!({ "add": [new] }),
                };
                self.update_zone(&name, changes).await?;
            }
        }
        let rel = if record.name.trim().is_empty() {
            "@".to_owned()
        } else {
            record.name.trim().to_ascii_lowercase()
        };
        let prio = matches!(record.record_type, DnsRecordType::Mx | DnsRecordType::Srv)
            .then(|| record.priority.unwrap_or(10));
        Ok(DnsRecord {
            id: Some(record_id(
                record.record_type.as_str(),
                &rel,
                record.value.trim(),
                prio,
            )),
            name: rel,
            ttl: Some(snap_ttl(record.ttl)),
            priority: prio,
            ..record.clone()
        })
    }

    /// `PUT /dns/zones/{domain}` with `remove`.
    async fn delete_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let records = self.zone_records(&name).await?.ok_or_else(|| {
            IntegrationError::NotFound(format!("no DNS zone for {name} at OpenProvider"))
        })?;
        let wanted_name = if record.name.trim().is_empty() {
            "@".to_owned()
        } else {
            record.name.trim().to_ascii_lowercase()
        };
        let original = records
            .iter()
            .filter_map(|r| to_record(&name, r))
            .find(|r| match &record.id {
                Some(id) => r.id.as_deref() == Some(id),
                None => {
                    r.name == wanted_name
                        && r.record_type == record.record_type
                        && r.value == record.value.trim()
                }
            })
            .ok_or_else(|| IntegrationError::NotFound("DNS record".into()))?;
        self.update_zone(&name, json!({ "remove": [to_op_record(&original)] }))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            split_street("Atatürk Cad. 12"),
            ("Atatürk Cad.".into(), "12".into())
        );
        assert_eq!(
            split_street("Main Street"),
            ("Main Street".into(), String::new())
        );
        assert_eq!(op_phone("90", "5551234567")["area_code"], "555");
        assert_eq!(snap_ttl(Some(300)), 900);
        assert_eq!(snap_ttl(Some(4000)), 10800);
        assert_eq!(snap_ttl(Some(999_999)), 86400);
        assert_eq!(
            op_date(Some(&json!("2019-01-27 0:55:17")))
                .unwrap()
                .to_rfc3339(),
            "2019-01-27T00:55:17+00:00"
        );
        assert!(op_date(Some(&json!("0000-00-00 00:00:00"))).is_none());
        assert_eq!(map_code(196, "x", 200).kind(), "auth");
        assert_eq!(map_code(320, "x", 200).kind(), "not_found");
        assert_eq!(map_code(311, "x", 200).kind(), "conflict");
        assert!(map_code(18002, "x", 500).is_retryable());
        assert!(!map_code(399, "registry", 500).is_retryable());
    }

    #[test]
    fn status_mapping() {
        let now = Utc::now();
        assert_eq!(
            status_from(&json!({"status": "REQ", "type": "TRANSFER"}), now),
            DomainStatus::PendingTransfer
        );
        assert_eq!(
            status_from(
                &json!({"status": "ACT", "expiration_date": "2000-01-01 00:00:00"}),
                now
            ),
            DomainStatus::Grace
        );
        assert_eq!(
            status_from(
                &json!({"status": "DEL", "restorable_until": "2999-01-01 00:00:00"}),
                now
            ),
            DomainStatus::Redemption
        );
    }
}
