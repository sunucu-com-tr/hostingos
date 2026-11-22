//! Namecheap XML API (`https://api.namecheap.com/xml.response`, sandbox
//! `https://api.sandbox.namecheap.com/xml.response`).
//!
//! Reference: <https://www.namecheap.com/support/api/methods/> and the
//! official Go SDK (`github.com/namecheap/go-namecheap-sdk`).
//!
//! * Every command is a form-encoded `POST` with the global parameters
//!   (`ApiUser`, `ApiKey`, `UserName`, `ClientIp`, `Command`) in the body — as
//!   the official SDK does — so the key never appears in a URL.
//! * Responses are parsed with `quick-xml`'s serde support after a
//!   normalisation pass that lower-cases every element/attribute name and drops
//!   namespaces: live responses and the docs disagree on casing
//!   (`<host>` vs `<Host>`, `Domainname` vs `DomainName`).
//! * `<Errors><Error Number="…">` is mapped by number: API key / client IP
//!   problems → [`IntegrationError::Auth`] (with a whitelisting hint), unknown
//!   domain → `NotFound`, unavailable → `Conflict`, validation → `InvalidInput`.
//!   HTTP 405 is Namecheap's rate-limit answer.
//! * The API has no command that returns an EPP code (it is e-mailed to the
//!   registrant), so [`DomainRegistrar::get_auth_code`] is `Unsupported`.
//! * `domains.dns.setHosts` replaces the whole host list: upsert/delete read
//!   the current list (`getHosts`), modify it and write it back.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use hos_core::IntegrationError;
use hos_core::domain::DomainStatus;
use hos_core::integrations::registrar::{
    ContactInfo, DnsRecord, DnsRecordType, DomainAvailability, DomainContacts, DomainInfo,
    DomainOperation, DomainOperationStatus, DomainRegistrar, DomainRegistration, DomainRenewal,
    DomainTransfer, TldPrice,
};
use hos_core::integrations::{ConnectionInfo, FieldReader, kinds};
use hos_core::money::{Currency, Money};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use quick_xml::events::Event;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

use crate::common;
use crate::http::{HttpClient, retry_after, truncate};
use crate::name::DomainName;
use crate::tld_rules;

/// Production endpoint.
pub const LIVE_URL: &str = "https://api.namecheap.com/xml.response";
/// Sandbox endpoint.
pub const SANDBOX_URL: &str = "https://api.sandbox.namecheap.com/xml.response";

/// TLDs that `domains.transfer.create` accepts (Namecheap docs).
pub const TRANSFER_TLDS: &[&str] = &[
    "biz", "ca", "cc", "co", "com", "com.es", "com.pe", "es", "in", "info", "me", "mobi", "net",
    "net.pe", "nom.es", "org", "org.es", "org.pe", "pe", "tv", "us",
];

/// Error numbers meaning "credentials / API access / client IP".
const AUTH_ERRORS: &[&str] = &[
    "1010101", "1010102", "1011101", "1011102", "1011150", "1016103", "1017101", "1017105",
    "1017150",
];
/// Domain unknown or not in this account.
const NOT_FOUND_ERRORS: &[&str] = &["2019166", "2016166", "3016166", "4011103"];
/// Domain not available / conflicting state.
const CONFLICT_ERRORS: &[&str] = &["3019166", "4019166", "2020166"];
/// Validation errors.
const INPUT_ERRORS: &[&str] = &["2011322", "2015182", "2030280", "2515610", "2515623"];

/// Connection settings.
#[derive(Debug, Clone)]
pub struct NamecheapConfig {
    /// Endpoint (`…/xml.response`).
    pub endpoint: Url,
    /// `ApiUser`.
    pub api_user: String,
    /// `UserName` (defaults to `ApiUser`).
    pub username: String,
    /// `ApiKey`.
    pub api_key: SecretString,
    /// `ClientIp` — the whitelisted IPv4 address of this server.
    pub client_ip: String,
    /// Request timeout.
    pub timeout: Duration,
    /// Account currency for prices without a currency attribute.
    pub currency: Currency,
    /// Address WHOIS-privacy mail is forwarded to when enabling privacy and
    /// Namecheap reports none.
    pub privacy_forward_email: Option<String>,
    /// Which destinations the client may contact (default: the process-wide
    /// `HOS_OUTBOUND_POLICY`; tests against a local mock server use
    /// [`OutboundPolicy::Unrestricted`]).
    pub outbound: OutboundPolicy,
}

impl NamecheapConfig {
    /// Minimal config.
    pub fn new(
        endpoint: Url,
        api_user: impl Into<String>,
        api_key: SecretString,
        client_ip: impl Into<String>,
    ) -> Self {
        let api_user = api_user.into();
        Self {
            endpoint,
            username: api_user.clone(),
            api_user,
            api_key,
            client_ip: client_ip.into(),
            timeout: crate::http::DEFAULT_TIMEOUT,
            currency: Currency::USD,
            privacy_forward_email: None,
            outbound: OutboundPolicy::global(),
        }
    }

    /// Reads `integrations.config` / `secrets` (see [`crate::descriptors`]).
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let sandbox = c.bool_or("sandbox", false)?;
        let endpoint = match c.opt_url("base_url")? {
            Some(u) => u,
            None => Url::parse(if sandbox { SANDBOX_URL } else { LIVE_URL })
                .map_err(|e| IntegrationError::Config(e.to_string()))?,
        };
        let client_ip = c.str("client_ip")?;
        if client_ip.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(IntegrationError::Config(
                "config.client_ip must be an IPv4 address".into(),
            ));
        }
        let mut cfg = Self::new(
            endpoint,
            c.str("api_user")?,
            s.secret("api_key")?,
            client_ip,
        );
        if let Some(u) = c.opt_str("username")? {
            cfg.username = u.to_owned();
        }
        cfg.timeout = Duration::from_secs(c.u64_or("timeout_secs", 30)?);
        cfg.currency = Currency::new(&c.string_or("currency", "USD")?).map_err(|_| {
            IntegrationError::Config("config.currency must be an ISO 4217 code".into())
        })?;
        cfg.privacy_forward_email = c.opt_str("privacy_forward_email")?.map(str::to_owned);
        Ok(cfg)
    }
}

/// Namecheap registrar. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Namecheap {
    cfg: NamecheapConfig,
    http: HttpClient,
}

// ----------------------------------------------------------------------
// XML model (names lower-cased by `normalize_xml`)
// ----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(rename = "@status", default)]
    status: String,
    #[serde(default)]
    errors: Option<Errors>,
    #[serde(rename = "commandresponse")]
    command_response: Option<T>,
}

#[derive(Debug, Deserialize, Default)]
struct Errors {
    #[serde(rename = "error", default)]
    error: Vec<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(rename = "@number", default)]
    number: String,
    #[serde(rename = "$text", default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct Ignored {}

#[derive(Debug, Deserialize)]
struct CheckResponse {
    #[serde(rename = "domaincheckresult", default)]
    results: Vec<CheckResult>,
}

#[derive(Debug, Deserialize)]
struct CheckResult {
    #[serde(rename = "@domain", default)]
    domain: String,
    #[serde(rename = "@available", default)]
    available: String,
    #[serde(rename = "@errorno", default)]
    error_no: String,
    #[serde(rename = "@description", default)]
    description: String,
    #[serde(rename = "@ispremiumname", default)]
    is_premium: String,
    #[serde(rename = "@premiumregistrationprice", default)]
    premium_registration_price: String,
    #[serde(rename = "@premiumrenewalprice", default)]
    premium_renewal_price: String,
}

#[derive(Debug, Deserialize)]
struct CreateResponse {
    #[serde(rename = "domaincreateresult")]
    result: CreateResult,
}

#[derive(Debug, Deserialize)]
struct CreateResult {
    #[serde(rename = "@registered", default)]
    registered: String,
    #[serde(rename = "@chargedamount", default)]
    charged_amount: String,
    #[serde(rename = "@domainid", default)]
    domain_id: String,
    #[serde(rename = "@orderid", default)]
    order_id: String,
    #[serde(rename = "@nonrealtimedomain", default)]
    non_real_time: String,
}

#[derive(Debug, Deserialize)]
struct TransferResponse {
    #[serde(rename = "domaintransfercreateresult")]
    result: TransferResult,
}

#[derive(Debug, Deserialize)]
struct TransferResult {
    #[serde(rename = "@transfer", default)]
    transfer: String,
    #[serde(rename = "@transferid", default)]
    transfer_id: String,
    #[serde(rename = "@orderid", default)]
    order_id: String,
    #[serde(rename = "@chargedamount", default)]
    charged_amount: String,
    #[serde(rename = "@statuscode", default)]
    status_code: String,
}

#[derive(Debug, Deserialize)]
struct RenewResponse {
    #[serde(rename = "domainrenewresult")]
    result: RenewResult,
}

#[derive(Debug, Deserialize)]
struct RenewResult {
    #[serde(rename = "@renew", default)]
    renew: String,
    #[serde(rename = "@domainid", default)]
    domain_id: String,
    #[serde(rename = "@chargedamount", default)]
    charged_amount: String,
    #[serde(rename = "domaindetails", default)]
    details: Option<DomainDetails>,
}

#[derive(Debug, Deserialize)]
struct ReactivateResponse {
    #[serde(rename = "domainreactivateresult")]
    result: ReactivateResult,
}

#[derive(Debug, Deserialize)]
struct ReactivateResult {
    #[serde(rename = "@issuccess", default)]
    is_success: String,
    #[serde(rename = "@chargedamount", default)]
    charged_amount: String,
    #[serde(rename = "@orderid", default)]
    order_id: String,
}

#[derive(Debug, Deserialize, Default)]
struct DomainDetails {
    #[serde(rename = "createddate", default)]
    created: Option<String>,
    #[serde(rename = "expireddate", default)]
    expired: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InfoResponse {
    #[serde(rename = "domaingetinforesult")]
    result: InfoResult,
}

#[derive(Debug, Deserialize)]
struct InfoResult {
    #[serde(rename = "@status", default)]
    status: String,
    #[serde(rename = "@id", default)]
    id: String,
    #[serde(rename = "@isowner", default)]
    is_owner: String,
    #[serde(rename = "@ispremium", default)]
    is_premium: String,
    #[serde(rename = "domaindetails", default)]
    details: DomainDetails,
    #[serde(rename = "whoisguard", default)]
    whoisguard: Option<WhoisGuard>,
    #[serde(rename = "dnsdetails", default)]
    dns: Option<DnsDetails>,
}

#[derive(Debug, Deserialize)]
struct WhoisGuard {
    #[serde(rename = "@enabled", default)]
    enabled: String,
    #[serde(rename = "id", default)]
    id: Option<String>,
    #[serde(rename = "emaildetails", default)]
    email: Option<WhoisGuardEmail>,
}

#[derive(Debug, Deserialize)]
struct WhoisGuardEmail {
    #[serde(rename = "@forwardedto", default)]
    forwarded_to: String,
}

#[derive(Debug, Deserialize)]
struct DnsDetails {
    #[serde(rename = "@providertype", default)]
    provider_type: String,
    #[serde(rename = "@isusingourdns", default)]
    using_our_dns: String,
    #[serde(rename = "nameserver", default)]
    nameservers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DnsListResponse {
    #[serde(rename = "domaindnsgetlistresult")]
    result: DnsListResult,
}

#[derive(Debug, Deserialize)]
struct DnsListResult {
    #[serde(rename = "nameserver", default)]
    nameservers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SuccessResponse {
    #[serde(
        rename = "domaindnssetcustomresult",
        alias = "domainsetregistrarlockresult",
        alias = "domainsetcontactresult",
        alias = "domaindnssethostsresult",
        alias = "whoisguardenableresult",
        alias = "whoisguarddisableresult"
    )]
    result: SuccessResult,
}

#[derive(Debug, Deserialize)]
struct SuccessResult {
    #[serde(rename = "@issuccess", alias = "@updated", default)]
    success: String,
}

#[derive(Debug, Deserialize)]
struct LockResponse {
    #[serde(rename = "domaingetregistrarlockresult")]
    result: LockResult,
}

#[derive(Debug, Deserialize)]
struct LockResult {
    #[serde(rename = "@registrarlockstatus", default)]
    locked: String,
}

#[derive(Debug, Deserialize)]
struct HostsResponse {
    #[serde(rename = "domaindnsgethostsresult")]
    result: HostsResult,
}

#[derive(Debug, Deserialize)]
struct HostsResult {
    #[serde(rename = "@emailtype", default)]
    email_type: String,
    #[serde(rename = "@isusingourdns", default)]
    using_our_dns: String,
    #[serde(rename = "host", default)]
    hosts: Vec<Host>,
}

#[derive(Debug, Deserialize, Clone)]
struct Host {
    #[serde(rename = "@hostid", default)]
    host_id: String,
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@type", default)]
    record_type: String,
    #[serde(rename = "@address", default)]
    address: String,
    #[serde(rename = "@mxpref", default)]
    mx_pref: String,
    #[serde(rename = "@ttl", default)]
    ttl: String,
}

#[derive(Debug, Deserialize)]
struct PricingResponse {
    #[serde(rename = "usergetpricingresult")]
    result: PricingResult,
}

#[derive(Debug, Deserialize)]
struct PricingResult {
    #[serde(rename = "producttype", default)]
    product_types: Vec<ProductType>,
}

#[derive(Debug, Deserialize)]
struct ProductType {
    #[serde(rename = "productcategory", default)]
    categories: Vec<ProductCategory>,
}

#[derive(Debug, Deserialize)]
struct ProductCategory {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "product", default)]
    products: Vec<Product>,
}

#[derive(Debug, Deserialize)]
struct Product {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "price", default)]
    prices: Vec<Price>,
}

#[derive(Debug, Deserialize)]
struct Price {
    #[serde(rename = "@duration", default)]
    duration: String,
    #[serde(rename = "@durationtype", default)]
    duration_type: String,
    #[serde(rename = "@price", default)]
    price: String,
    #[serde(rename = "@additionalcost", default)]
    additional_cost: String,
    #[serde(rename = "@currency", default)]
    currency: String,
}

#[derive(Debug, Deserialize)]
struct BalancesResponse {
    #[serde(rename = "usergetbalancesresult")]
    result: Balances,
}

#[derive(Debug, Deserialize)]
struct Balances {
    #[serde(rename = "@currency", default)]
    currency: String,
    #[serde(rename = "@availablebalance", default)]
    available: String,
}

// ----------------------------------------------------------------------
// XML helpers
// ----------------------------------------------------------------------

/// Rewrites `xml` with lower-case local element/attribute names and without
/// namespace declarations, comments, PIs and the XML declaration. Text and
/// attribute values are copied verbatim (still escaped).
pub fn normalize_xml(xml: &str) -> Result<String, IntegrationError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::with_capacity(xml.len());
    let bad = |e: &dyn std::fmt::Display| {
        IntegrationError::Transport(format!("namecheap: invalid XML: {e}"))
    };
    loop {
        let event = reader.read_event().map_err(|e| bad(&e))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                out.push('<');
                out.push_str(&e.local_name().as_ref().to_ascii_lowercase());
                for attr in e.attributes() {
                    let attr = attr.map_err(|err| bad(&err))?;
                    let key = attr.key.as_ref();
                    if key == "xmlns" || key.starts_with("xmlns:") {
                        continue;
                    }
                    out.push(' ');
                    out.push_str(&attr.key.local_name().as_ref().to_ascii_lowercase());
                    out.push_str("=\"");
                    out.push_str(&attr.value.replace('"', "&quot;"));
                    out.push('"');
                }
                out.push_str(if empty { "/>" } else { ">" });
            }
            Event::End(e) => {
                out.push_str("</");
                out.push_str(&e.local_name().as_ref().to_ascii_lowercase());
                out.push('>');
            }
            Event::Text(t) => out.push_str(&t),
            Event::GeneralRef(r) => {
                out.push('&');
                out.push_str(&r);
                out.push(';');
            }
            Event::CData(c) => {
                out.push_str("<![CDATA[");
                out.push_str(&c);
                out.push_str("]]>");
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
        }
    }
    Ok(out)
}

fn is_true(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1")
}

/// Namecheap dates: `MM/DD/YYYY` optionally followed by a time
/// (`4/30/2021 11:31:13 AM`, U+202F before AM/PM). Date only, UTC midnight.
fn parse_nc_date(s: &str) -> Option<DateTime<Utc>> {
    let date = s.split(char::is_whitespace).next()?.trim();
    let d = NaiveDate::parse_from_str(date, "%m/%d/%Y").ok()?;
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?).into()
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `EPPCode` value: plain when alphanumeric, else `base64:<…>` (docs).
fn epp_param(code: &str) -> String {
    if code.chars().all(|c| c.is_ascii_alphanumeric()) {
        code.to_owned()
    } else {
        format!("base64:{}", base64(code.as_bytes()))
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Appends the Namecheap contact parameters for `prefix` (Registrant, Tech, Admin, AuxBilling).
fn push_contact(params: &mut Vec<(String, String)>, prefix: &str, c: &ContactInfo) {
    let a = &c.address;
    let mut put = |k: &str, v: String| {
        if !v.trim().is_empty() {
            params.push((format!("{prefix}{k}"), v.trim().to_owned()));
        }
    };
    put("FirstName", c.first_name.clone());
    put("LastName", c.last_name.clone());
    put("OrganizationName", c.company.clone().unwrap_or_default());
    put("Address1", a.line1.clone());
    put("Address2", a.line2.clone().unwrap_or_default());
    put("City", a.city.clone());
    put(
        "StateProvince",
        a.state
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "N/A".into()),
    );
    put(
        "PostalCode",
        a.postal_code
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "00000".into()),
    );
    put("Country", a.country_code.to_ascii_uppercase());
    put("Phone", c.phone.to_epp());
    if let Some(f) = c.fax.as_ref().filter(|f| !f.number.is_empty()) {
        put("Fax", f.to_epp());
    }
    put("EmailAddress", c.email.clone());
}

/// TLD extended attributes (Namecheap "Extended attributes" page) from the
/// neutral [`tld_rules`] keys + raw `extra.attributes`.
fn extended_attributes(
    tld: &str,
    registrant: &ContactInfo,
    extra: &Value,
) -> Vec<(String, String)> {
    let get = |k: &str| {
        tld_rules::extra_str(&registrant.extra, k).or_else(|| tld_rules::extra_str(extra, k))
    };
    let mut out: Vec<(String, String)> = Vec::new();
    match tld {
        "us" => {
            if let Some(v) = get("us_nexus_category") {
                out.push(("RegistrantNexus".into(), v.to_ascii_uppercase()));
            }
            if let Some(v) = get("us_purpose") {
                out.push(("RegistrantPurpose".into(), v.to_ascii_uppercase()));
            }
            if let Some(v) = get("us_nexus_country") {
                out.push(("RegistrantNexusCountry".into(), v.to_ascii_uppercase()));
            }
        }
        "ca" => {
            if let Some(v) = get("ca_legal_type") {
                out.push(("CIRALegalType".into(), v.to_ascii_uppercase()));
            }
        }
        "co.uk" | "org.uk" | "me.uk" => {
            let prefix = match tld {
                "co.uk" => "COUK",
                "org.uk" => "ORGUK",
                _ => "MEUK",
            };
            if let Some(v) = get("uk_legal_type") {
                out.push((format!("{prefix}LegalType"), v.to_ascii_uppercase()));
            }
            if let Some(v) = get("uk_company_number") {
                out.push((format!("{prefix}CompanyID"), v));
            }
        }
        _ => {}
    }
    for source in [extra, &registrant.extra] {
        for (k, v) in common::raw_attributes(source) {
            if !out.iter().any(|(ek, _)| ek.eq_ignore_ascii_case(&k)) {
                out.push((k, v));
            }
        }
    }
    out
}

fn to_record(h: &Host) -> Option<DnsRecord> {
    let record_type: DnsRecordType = h.record_type.to_ascii_uppercase().parse().ok()?;
    Some(DnsRecord {
        id: Some(h.host_id.clone()).filter(|s| !s.is_empty()),
        name: if h.name.is_empty() {
            "@".into()
        } else {
            h.name.clone()
        },
        record_type,
        value: h.address.clone(),
        ttl: h.ttl.parse().ok(),
        priority: if record_type == DnsRecordType::Mx {
            h.mx_pref.parse().ok()
        } else {
            None
        },
    })
}

/// Namecheap accepts TTLs of 60–60000 seconds (default 1800).
fn clamp_ttl(ttl: Option<u32>) -> u32 {
    ttl.unwrap_or(1800).clamp(60, 60_000)
}

impl Namecheap {
    /// Builds the client.
    pub fn new(cfg: NamecheapConfig) -> Result<Self, IntegrationError> {
        let http = HttpClient::new("namecheap", cfg.timeout, &[&cfg.api_key], cfg.outbound)?;
        http.check_endpoint(&cfg.endpoint)?;
        Ok(Self { cfg, http })
    }

    /// Sends a command and deserializes `<CommandResponse>`.
    async fn command<T: DeserializeOwned>(
        &self,
        command: &str,
        params: Vec<(String, String)>,
        extra_secrets: &[&str],
    ) -> Result<T, IntegrationError> {
        let mut form: Vec<(String, String)> = vec![
            ("ApiUser".into(), self.cfg.api_user.clone()),
            ("ApiKey".into(), self.cfg.api_key.expose_str().to_owned()),
            ("UserName".into(), self.cfg.username.clone()),
            ("ClientIp".into(), self.cfg.client_ip.clone()),
            ("Command".into(), command.to_owned()),
        ];
        form.extend(params);
        let req = self
            .http
            .client()
            .post(self.cfg.endpoint.clone())
            .form(&form);
        let op: &'static str = "namecheap.command";
        let resp = self.http.execute(req, op).await?;
        tracing::debug!(command, status = resp.status, "namecheap command");
        if resp.status == 405 || resp.status == 429 {
            return Err(IntegrationError::RateLimited {
                retry_after: retry_after(&resp.headers),
            });
        }
        let scrub = |s: &str| self.http.scrub_with(s, extra_secrets);
        let normalized = match normalize_xml(&resp.body) {
            Ok(x) if x.contains("<apiresponse") => x,
            _ => {
                let msg = crate::http::describe_body(&resp.body);
                return Err(if (200..300).contains(&resp.status) {
                    IntegrationError::Transport(format!("namecheap: unexpected response: {msg}"))
                } else {
                    crate::http::status_error(resp.status, &resp.headers, msg)
                });
            }
        };
        let head: Envelope<Ignored> = quick_xml::de::from_str(&normalized).map_err(|e| {
            IntegrationError::Transport(format!("namecheap: cannot parse response: {e}"))
        })?;
        let errors = head.errors.map(|e| e.error).unwrap_or_default();
        if !head.status.eq_ignore_ascii_case("ok") || !errors.is_empty() {
            let first = errors.first();
            let number = first
                .map(|e| e.number.trim().to_owned())
                .unwrap_or_default();
            let message = scrub(
                &first
                    .map(|e| e.message.trim().to_owned())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| format!("command {command} failed")),
            );
            return Err(map_error(&number, &message));
        }
        let env: Envelope<T> = quick_xml::de::from_str(&normalized).map_err(|e| {
            IntegrationError::Transport(format!("namecheap: unexpected {command} response: {e}"))
        })?;
        env.command_response.ok_or_else(|| {
            IntegrationError::Transport(format!(
                "namecheap: {command} response without CommandResponse"
            ))
        })
    }

    fn sld_tld(name: &DomainName) -> Vec<(String, String)> {
        vec![
            ("SLD".into(), name.sld().to_owned()),
            ("TLD".into(), name.tld().to_owned()),
        ]
    }

    fn money(&self, amount: &str, currency: &str) -> Option<Money> {
        let cur = if currency.trim().is_empty() {
            self.cfg.currency.as_str().to_owned()
        } else {
            currency.trim().to_owned()
        };
        common::money(common::parse_decimal(amount).filter(|d| !d.is_zero()), &cur)
    }

    async fn get_info(&self, name: &DomainName) -> Result<InfoResult, IntegrationError> {
        let r: InfoResponse = self
            .command(
                "namecheap.domains.getInfo",
                vec![("DomainName".into(), name.as_str().to_owned())],
                &[],
            )
            .await?;
        Ok(r.result)
    }

    async fn get_hosts(&self, name: &DomainName) -> Result<HostsResult, IntegrationError> {
        let r: HostsResponse = self
            .command("namecheap.domains.dns.getHosts", Self::sld_tld(name), &[])
            .await?;
        Ok(r.result)
    }

    async fn set_hosts(
        &self,
        name: &DomainName,
        hosts: &[Host],
        current_email_type: &str,
    ) -> Result<(), IntegrationError> {
        let mut params = Self::sld_tld(name);
        for (i, h) in hosts.iter().enumerate() {
            let n = i + 1;
            params.push((
                format!("HostName{n}"),
                if h.name.is_empty() {
                    "@".into()
                } else {
                    h.name.clone()
                },
            ));
            params.push((format!("RecordType{n}"), h.record_type.to_ascii_uppercase()));
            params.push((format!("Address{n}"), h.address.clone()));
            if h.record_type.eq_ignore_ascii_case("MX") {
                params.push((
                    format!("MXPref{n}"),
                    if h.mx_pref.is_empty() {
                        "10".into()
                    } else {
                        h.mx_pref.clone()
                    },
                ));
            }
            params.push((format!("TTL{n}"), clamp_ttl(h.ttl.parse().ok()).to_string()));
        }
        let has_mx = hosts
            .iter()
            .any(|h| h.record_type.eq_ignore_ascii_case("MX"));
        // MX records require EmailType=MX; otherwise keep the domain's current
        // mail setting (forwarding, private email, ...).
        let email_type = if has_mx {
            "MX".to_owned()
        } else {
            current_email_type.trim().to_ascii_uppercase()
        };
        if !email_type.is_empty() {
            params.push(("EmailType".into(), email_type));
        }
        let r: SuccessResponse = self
            .command("namecheap.domains.dns.setHosts", params, &[])
            .await?;
        ensure_success(&r, "setHosts")
    }
}

fn ensure_success(r: &SuccessResponse, what: &str) -> Result<(), IntegrationError> {
    if is_true(&r.result.success) {
        Ok(())
    } else {
        Err(IntegrationError::remote(
            None,
            format!("Namecheap {what} reported no success"),
        ))
    }
}

fn map_error(number: &str, message: &str) -> IntegrationError {
    let msg = truncate(message, 400);
    let lower = msg.to_ascii_lowercase();
    if AUTH_ERRORS.contains(&number) || lower.contains("api key") || lower.contains("request ip") {
        return IntegrationError::Auth(format!(
            "{msg} [{number}] — check ApiUser/ApiKey, that API access is enabled and that ClientIp is whitelisted in the Namecheap panel (Profile → Tools → API Access)"
        ));
    }
    if NOT_FOUND_ERRORS.contains(&number) {
        return IntegrationError::NotFound(format!("{msg} [{number}]"));
    }
    if CONFLICT_ERRORS.contains(&number) {
        return IntegrationError::Conflict(format!("{msg} [{number}]"));
    }
    if INPUT_ERRORS.contains(&number) {
        return IntegrationError::InvalidInput(format!("{msg} [{number}]"));
    }
    if number.is_empty() {
        IntegrationError::remote(None, msg)
    } else {
        IntegrationError::remote_with_code(None, number, msg)
    }
}

#[async_trait]
impl DomainRegistrar for Namecheap {
    fn kind(&self) -> &'static str {
        kinds::NAMECHEAP
    }

    /// `namecheap.users.getBalances`.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let r: BalancesResponse = self
            .command("namecheap.users.getBalances", Vec::new(), &[])
            .await?;
        Ok(ConnectionInfo {
            server_version: Some(format!(
                "Namecheap XML API ({})",
                self.cfg.endpoint.host_str().unwrap_or("?")
            )),
            details: Some(format!(
                "user {}, available balance {} {}",
                self.cfg.username, r.result.available, r.result.currency
            )),
        })
    }

    /// `namecheap.domains.check` in batches of 50.
    async fn check_availability(
        &self,
        domains: &[String],
    ) -> Result<Vec<DomainAvailability>, IntegrationError> {
        let parsed: Vec<Result<DomainName, String>> = domains
            .iter()
            .map(|d| DomainName::parse(d).map_err(|e| e.to_string()))
            .collect();
        let valid: Vec<&DomainName> = parsed.iter().filter_map(|p| p.as_ref().ok()).collect();
        let mut results: Vec<CheckResult> = Vec::new();
        for chunk in valid.chunks(50) {
            let list = chunk
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let r: CheckResponse = self
                .command(
                    "namecheap.domains.check",
                    vec![("DomainList".into(), list)],
                    &[],
                )
                .await?;
            results.extend(r.results);
        }
        Ok(parsed
            .into_iter()
            .zip(domains)
            .map(|(p, input)| match p {
                Err(e) => DomainAvailability {
                    domain: input.trim().to_ascii_lowercase(),
                    available: false,
                    premium: false,
                    register_price: None,
                    renew_price: None,
                    reason: Some(e),
                },
                Ok(name) => {
                    let unicode = name.to_unicode();
                    let hit = results.iter().find(|r| {
                        r.domain.eq_ignore_ascii_case(name.as_str())
                            || r.domain.to_lowercase() == unicode
                    });
                    match hit {
                        None => DomainAvailability {
                            domain: name.as_str().to_owned(),
                            available: false,
                            premium: false,
                            register_price: None,
                            renew_price: None,
                            reason: Some("not returned by Namecheap".into()),
                        },
                        Some(r) => {
                            let premium = is_true(&r.is_premium);
                            let err = r.error_no.trim();
                            DomainAvailability {
                                domain: name.as_str().to_owned(),
                                available: is_true(&r.available),
                                premium,
                                register_price: premium
                                    .then(|| self.money(&r.premium_registration_price, ""))
                                    .flatten(),
                                renew_price: premium
                                    .then(|| self.money(&r.premium_renewal_price, ""))
                                    .flatten(),
                                reason: if !err.is_empty() && err != "0" {
                                    Some(format!("{} [{err}]", r.description.trim()))
                                } else if is_true(&r.available) {
                                    None
                                } else {
                                    Some("registered".into())
                                },
                            }
                        }
                    }
                }
            })
            .collect())
    }

    /// `namecheap.users.getPricing` (ProductType=DOMAIN; one call per TLD when
    /// a list is given). Price = `Price` + `AdditionalCost` (ICANN fee) for a
    /// one-year duration.
    async fn tld_pricing(&self, tlds: &[String]) -> Result<Vec<TldPrice>, IntegrationError> {
        let mut responses = Vec::new();
        if tlds.is_empty() {
            let r: PricingResponse = self
                .command(
                    "namecheap.users.getPricing",
                    vec![("ProductType".into(), "DOMAIN".into())],
                    &[],
                )
                .await?;
            responses.push(r.result);
        } else {
            for tld in tlds {
                let tld = tld.trim().trim_start_matches('.').to_ascii_lowercase();
                let r: PricingResponse = self
                    .command(
                        "namecheap.users.getPricing",
                        vec![
                            ("ProductType".into(), "DOMAIN".into()),
                            ("ProductName".into(), tld),
                        ],
                        &[],
                    )
                    .await?;
                responses.push(r.result);
            }
        }
        let mut map: std::collections::BTreeMap<String, TldPrice> =
            std::collections::BTreeMap::new();
        for result in responses {
            for pt in result.product_types {
                for cat in pt.categories {
                    let category = cat.name.to_ascii_lowercase();
                    for product in cat.products {
                        let tld = product.name.to_ascii_lowercase();
                        let rule = tld_rules::rule_for(&tld);
                        let entry = map.entry(tld.clone()).or_insert_with(|| TldPrice {
                            tld: tld.clone(),
                            register: None,
                            renew: None,
                            transfer: None,
                            restore: None,
                            min_years: rule.min_years,
                            max_years: rule.max_years,
                        });
                        let yearly: Vec<&Price> = product
                            .prices
                            .iter()
                            .filter(|p| p.duration_type.eq_ignore_ascii_case("year"))
                            .collect();
                        let one = yearly
                            .iter()
                            .min_by_key(|p| p.duration.parse::<u32>().unwrap_or(u32::MAX))
                            .and_then(|p| {
                                let base = common::parse_decimal(&p.price)?;
                                let fee =
                                    common::parse_decimal(&p.additional_cost).unwrap_or_default();
                                let cur = if p.currency.is_empty() {
                                    self.cfg.currency.as_str()
                                } else {
                                    p.currency.as_str()
                                };
                                common::money(Some(base + fee), cur)
                            });
                        match category.as_str() {
                            "register" => {
                                entry.register = one;
                                let years: Vec<u32> = yearly
                                    .iter()
                                    .filter_map(|p| p.duration.parse().ok())
                                    .collect();
                                if let (Some(min), Some(max)) =
                                    (years.iter().min(), years.iter().max())
                                {
                                    entry.min_years = *min;
                                    entry.max_years = *max;
                                }
                            }
                            "renew" => entry.renew = one,
                            "transfer" => entry.transfer = one,
                            "reactivate" => entry.restore = one,
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(map
            .into_values()
            .filter(|p| p.register.is_some() || p.renew.is_some())
            .collect())
    }

    /// `namecheap.domains.create` with all four contact sets.
    async fn register(
        &self,
        request: &DomainRegistration,
    ) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        let rule = tld_rules::rule_for(name.tld());
        rule.validate_registration(request)?;
        let c = &request.contacts;
        for (role, contact) in [
            ("registrant", &c.registrant),
            ("admin", &c.admin),
            ("tech", &c.tech),
            ("billing", &c.billing),
        ] {
            common::validate_contact(role, contact)?;
        }
        let ns = common::nameservers(&request.nameservers)?;
        let mut params: Vec<(String, String)> = vec![
            ("DomainName".into(), name.as_str().to_owned()),
            ("Years".into(), request.years.to_string()),
        ];
        push_contact(&mut params, "Registrant", &c.registrant);
        push_contact(&mut params, "Tech", &c.tech);
        push_contact(&mut params, "Admin", &c.admin);
        push_contact(&mut params, "AuxBilling", &c.billing);
        if !ns.is_empty() {
            params.push(("Nameservers".into(), ns.join(",")));
        }
        let privacy = request.privacy && rule.privacy_allowed;
        params.push(("AddFreeWhoisguard".into(), yes_no(privacy).into()));
        params.push(("WGEnabled".into(), yes_no(privacy).into()));
        if let Some(p) = &request.premium_price {
            params.push(("IsPremiumDomain".into(), "true".into()));
            params.push(("PremiumPrice".into(), p.to_decimal_string()));
        }
        if let Some(code) = tld_rules::extra_str(&request.extra, "idn_language_code") {
            params.push(("IdnCode".into(), code));
        }
        params.extend(extended_attributes(
            name.tld(),
            &c.registrant,
            &request.extra,
        ));
        let r: CreateResponse = self
            .command("namecheap.domains.create", params, &[])
            .await?;
        if !is_true(&r.result.registered) {
            return Err(IntegrationError::remote(
                None,
                format!("Namecheap did not register {name}"),
            ));
        }
        let pending = is_true(&r.result.non_real_time);
        Ok(DomainOperation {
            domain: name.into_string(),
            status: if pending {
                DomainOperationStatus::Pending
            } else {
                DomainOperationStatus::Completed
            },
            registrar_ref: Some(r.result.domain_id).filter(|s| !s.is_empty()),
            expires_at: None,
            cost: self.money(&r.result.charged_amount, ""),
            message: Some(format!("order {}", r.result.order_id)),
        })
    }

    /// `namecheap.domains.transfer.create`. Namecheap uses the account's
    /// default contacts and keeps the current nameservers; `contacts` and
    /// `nameservers` of the request are not sent (update them after completion).
    async fn transfer(
        &self,
        request: &DomainTransfer,
    ) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        if !TRANSFER_TLDS.contains(&name.tld()) {
            return Err(IntegrationError::InvalidInput(format!(
                "Namecheap does not accept API transfers for .{}",
                name.tld()
            )));
        }
        let code = request.auth_code.expose_str().trim().to_owned();
        if code.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "auth code is required".into(),
            ));
        }
        let epp = epp_param(&code);
        let params = vec![
            ("DomainName".into(), name.as_str().to_owned()),
            ("Years".into(), "1".into()),
            ("EPPCode".into(), epp.clone()),
            ("AddFreeWhoisguard".into(), yes_no(request.privacy).into()),
            ("WGenable".into(), yes_no(request.privacy).into()),
        ];
        let r: TransferResponse = self
            .command("namecheap.domains.transfer.create", params, &[&code, &epp])
            .await?;
        if !is_true(&r.result.transfer) {
            return Err(IntegrationError::remote(
                None,
                format!("Namecheap did not accept the transfer of {name}"),
            ));
        }
        Ok(DomainOperation {
            domain: name.into_string(),
            status: DomainOperationStatus::Pending,
            registrar_ref: Some(r.result.transfer_id).filter(|s| !s.is_empty()),
            expires_at: None,
            cost: self.money(&r.result.charged_amount, ""),
            message: Some(format!(
                "transfer order {} (status {}); contacts/nameservers can be updated after completion",
                r.result.order_id, r.result.status_code
            )),
        })
    }

    /// `namecheap.domains.renew`; when the domain has expired (error 2020166)
    /// and `restore_if_needed`, `namecheap.domains.reactivate`.
    async fn renew(&self, request: &DomainRenewal) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        tld_rules::rule_for(name.tld()).validate_years(request.years)?;
        let renewed: Result<RenewResponse, IntegrationError> = self
            .command(
                "namecheap.domains.renew",
                vec![
                    ("DomainName".into(), name.as_str().to_owned()),
                    ("Years".into(), request.years.to_string()),
                ],
                &[],
            )
            .await;
        match renewed {
            Ok(r) => {
                if !is_true(&r.result.renew) {
                    return Err(IntegrationError::remote(
                        None,
                        format!("Namecheap did not renew {name}"),
                    ));
                }
                Ok(DomainOperation {
                    domain: name.into_string(),
                    status: DomainOperationStatus::Completed,
                    registrar_ref: Some(r.result.domain_id).filter(|s| !s.is_empty()),
                    expires_at: r
                        .result
                        .details
                        .and_then(|d| d.expired)
                        .and_then(|d| parse_nc_date(&d)),
                    cost: self.money(&r.result.charged_amount, ""),
                    message: None,
                })
            }
            Err(IntegrationError::Conflict(msg))
                if msg.contains("2020166") && request.restore_if_needed =>
            {
                let r: ReactivateResponse = self
                    .command(
                        "namecheap.domains.reactivate",
                        vec![
                            ("DomainName".into(), name.as_str().to_owned()),
                            ("YearsToAdd".into(), request.years.to_string()),
                        ],
                        &[],
                    )
                    .await?;
                if !is_true(&r.result.is_success) {
                    return Err(IntegrationError::remote(
                        None,
                        format!("Namecheap did not reactivate {name}"),
                    ));
                }
                Ok(DomainOperation {
                    domain: name.into_string(),
                    status: DomainOperationStatus::Completed,
                    registrar_ref: Some(r.result.order_id).filter(|s| !s.is_empty()),
                    expires_at: None,
                    cost: self.money(&r.result.charged_amount, ""),
                    message: Some("reactivated (restored) expired domain".into()),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// `namecheap.domains.getInfo` + `namecheap.domains.getRegistrarLock`.
    async fn info(&self, domain: &str) -> Result<DomainInfo, IntegrationError> {
        let name = common::domain(domain)?;
        let r = self.get_info(&name).await?;
        if !r.is_owner.is_empty() && !is_true(&r.is_owner) {
            return Err(IntegrationError::NotFound(format!(
                "{name} is not owned by this Namecheap account"
            )));
        }
        let lock: LockResponse = self
            .command(
                "namecheap.domains.getRegistrarLock",
                vec![("DomainName".into(), name.as_str().to_owned())],
                &[],
            )
            .await?;
        let locked = is_true(&lock.result.locked);
        let expires = r.details.expired.as_deref().and_then(parse_nc_date);
        let now = Utc::now();
        let status = if r.status.eq_ignore_ascii_case("expired") || expires.is_some_and(|e| e < now)
        {
            DomainStatus::Grace
        } else {
            DomainStatus::Active
        };
        let (privacy, wg_id) = match &r.whoisguard {
            Some(w) => (Some(is_true(&w.enabled)), w.id.clone()),
            None => (None, None),
        };
        let (ns, provider) = match &r.dns {
            Some(d) => (
                d.nameservers
                    .iter()
                    .map(|n| n.trim().trim_end_matches('.').to_ascii_lowercase())
                    .collect(),
                json!({ "provider_type": d.provider_type, "using_namecheap_dns": is_true(&d.using_our_dns) }),
            ),
            None => (Vec::new(), Value::Null),
        };
        Ok(DomainInfo {
            domain: name.into_string(),
            status,
            epp_statuses: if locked {
                vec!["clientTransferProhibited".into()]
            } else {
                Vec::new()
            },
            registrar_ref: Some(r.id).filter(|s| !s.is_empty()),
            created_at: r.details.created.as_deref().and_then(parse_nc_date),
            expires_at: expires,
            nameservers: ns,
            locked: Some(locked),
            privacy,
            auto_renew: None,
            contacts: None,
            extra: json!({
                "status": r.status,
                "is_premium": is_true(&r.is_premium),
                "whoisguard_id": wg_id,
                "dns": provider,
            }),
        })
    }

    /// `namecheap.domains.dns.getList`.
    async fn get_nameservers(&self, domain: &str) -> Result<Vec<String>, IntegrationError> {
        let name = common::domain(domain)?;
        let r: DnsListResponse = self
            .command("namecheap.domains.dns.getList", Self::sld_tld(&name), &[])
            .await?;
        Ok(r.result
            .nameservers
            .iter()
            .map(|n| n.trim().trim_end_matches('.').to_ascii_lowercase())
            .collect())
    }

    /// `namecheap.domains.dns.setCustom`.
    async fn set_nameservers(
        &self,
        domain: &str,
        nameservers: &[String],
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let ns = common::required_nameservers(nameservers)?;
        let mut params = Self::sld_tld(&name);
        params.push(("Nameservers".into(), ns.join(",")));
        let r: SuccessResponse = self
            .command("namecheap.domains.dns.setCustom", params, &[])
            .await?;
        ensure_success(&r, "dns.setCustom")
    }

    /// `namecheap.domains.setRegistrarLock`.
    async fn set_lock(&self, domain: &str, locked: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let r: SuccessResponse = self
            .command(
                "namecheap.domains.setRegistrarLock",
                vec![
                    ("DomainName".into(), name.as_str().to_owned()),
                    (
                        "LockAction".into(),
                        if locked { "LOCK" } else { "UNLOCK" }.into(),
                    ),
                ],
                &[],
            )
            .await?;
        ensure_success(&r, "setRegistrarLock")
    }

    /// Not available: the Namecheap API has no EPP-code command (the code is
    /// e-mailed to the registrant from the Namecheap panel).
    async fn get_auth_code(&self, domain: &str) -> Result<SecretString, IntegrationError> {
        let _ = domain;
        Err(IntegrationError::Unsupported("get_auth_code"))
    }

    /// `namecheap.whoisguard.enable` / `disable` with the WhoisGuard id from
    /// `getInfo`.
    async fn set_privacy(&self, domain: &str, enabled: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let info = self.get_info(&name).await?;
        let wg = info.whoisguard.ok_or_else(|| {
            IntegrationError::InvalidInput(format!(
                "{name} has no WhoisGuard / domain privacy subscription"
            ))
        })?;
        let id = wg
            .id
            .clone()
            .filter(|s| !s.trim().is_empty() && s.trim() != "0")
            .ok_or_else(|| {
                IntegrationError::InvalidInput(format!(
                    "no WhoisGuard subscription is allotted to {name} ({})",
                    wg.enabled
                ))
            })?;
        let r: SuccessResponse = if enabled {
            let forward = wg
                .email
                .as_ref()
                .map(|e| e.forwarded_to.trim().to_owned())
                .filter(|s| !s.is_empty())
                .or_else(|| self.cfg.privacy_forward_email.clone())
                .ok_or_else(|| {
                    IntegrationError::Config(
                        "config.privacy_forward_email is required to enable WhoisGuard".into(),
                    )
                })?;
            self.command(
                "namecheap.whoisguard.enable",
                vec![
                    ("WhoisguardID".into(), id),
                    ("ForwardedToEmail".into(), forward),
                ],
                &[],
            )
            .await?
        } else {
            self.command(
                "namecheap.whoisguard.disable",
                vec![("WhoisguardID".into(), id)],
                &[],
            )
            .await?
        };
        ensure_success(&r, "whoisguard")
    }

    /// `namecheap.domains.setContacts`.
    async fn update_contacts(
        &self,
        domain: &str,
        contacts: &DomainContacts,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        for (role, c) in [
            ("registrant", &contacts.registrant),
            ("admin", &contacts.admin),
            ("tech", &contacts.tech),
            ("billing", &contacts.billing),
        ] {
            common::validate_contact(role, c)?;
        }
        let mut params = vec![("DomainName".into(), name.as_str().to_owned())];
        push_contact(&mut params, "Registrant", &contacts.registrant);
        push_contact(&mut params, "Tech", &contacts.tech);
        push_contact(&mut params, "Admin", &contacts.admin);
        push_contact(&mut params, "AuxBilling", &contacts.billing);
        params.extend(extended_attributes(
            name.tld(),
            &contacts.registrant,
            &Value::Null,
        ));
        let r: SuccessResponse = self
            .command("namecheap.domains.setContacts", params, &[])
            .await?;
        ensure_success(&r, "setContacts")
    }

    /// `namecheap.domains.dns.getHosts`.
    async fn list_dns_records(&self, domain: &str) -> Result<Vec<DnsRecord>, IntegrationError> {
        let name = common::domain(domain)?;
        let hosts = self.get_hosts(&name).await?;
        Ok(hosts.hosts.iter().filter_map(to_record).collect())
    }

    /// Read-modify-write of the whole host list (`getHosts` → `setHosts`), then
    /// a re-read to return the record with its new `HostId`.
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
        let current = self.get_hosts(&name).await?;
        if !current.using_our_dns.is_empty() && !is_true(&current.using_our_dns) {
            return Err(IntegrationError::InvalidInput(format!(
                "{name} does not use Namecheap DNS; host records cannot be managed here"
            )));
        }
        let new_host = Host {
            host_id: String::new(),
            name: if record.name.trim().is_empty() {
                "@".into()
            } else {
                record.name.trim().to_owned()
            },
            record_type: record.record_type.as_str().to_owned(),
            address: record.value.trim().to_owned(),
            mx_pref: record.priority.map(|p| p.to_string()).unwrap_or_default(),
            ttl: clamp_ttl(record.ttl).to_string(),
        };
        let mut hosts = current.hosts.clone();
        match &record.id {
            Some(id) => {
                let slot = hosts
                    .iter_mut()
                    .find(|h| &h.host_id == id)
                    .ok_or_else(|| IntegrationError::NotFound(format!("DNS record {id}")))?;
                *slot = new_host.clone();
            }
            None => hosts.push(new_host.clone()),
        }
        self.set_hosts(&name, &hosts, &current.email_type).await?;
        let after = self.get_hosts(&name).await?;
        let stored = after
            .hosts
            .iter()
            .rev()
            .find(|h| {
                h.name.eq_ignore_ascii_case(&new_host.name)
                    && h.record_type.eq_ignore_ascii_case(&new_host.record_type)
                    && h.address.trim_end_matches('.') == new_host.address.trim_end_matches('.')
            })
            .and_then(to_record);
        Ok(stored.unwrap_or_else(|| DnsRecord {
            id: None,
            ..record.clone()
        }))
    }

    /// Read-modify-write without the record (matched by `HostId`, else by
    /// name + type + value).
    async fn delete_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let current = self.get_hosts(&name).await?;
        let target_name = if record.name.trim().is_empty() {
            "@"
        } else {
            record.name.trim()
        };
        let before = current.hosts.len();
        let hosts: Vec<Host> = current
            .hosts
            .iter()
            .filter(|h| match &record.id {
                Some(id) => &h.host_id != id,
                None => {
                    !(h.name.eq_ignore_ascii_case(target_name)
                        && h.record_type
                            .eq_ignore_ascii_case(record.record_type.as_str())
                        && h.address.trim_end_matches('.')
                            == record.value.trim().trim_end_matches('.'))
                }
            })
            .cloned()
            .collect();
        if hosts.len() == before {
            return Err(IntegrationError::NotFound("DNS record".into()));
        }
        self.set_hosts(&name, &hosts, &current.email_type).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_names_and_parses() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<ApiResponse Status="OK" xmlns="http://api.namecheap.com/xml.response">
  <Errors />
  <CommandResponse Type="namecheap.domains.dns.getHosts">
    <DomainDNSGetHostsResult Domain="example.com" EmailType="MX" IsUsingOurDNS="true">
      <host HostId="12" Name="@" Type="A" Address="1.2.3.4" MXPref="10" TTL="1800" />
      <Host HostId="14" Name="www" Type="CNAME" Address="a &amp; b" MXPref="10" TTL="1800" />
    </DomainDNSGetHostsResult>
  </CommandResponse>
</ApiResponse>"#;
        let n = normalize_xml(xml).unwrap();
        assert!(n.contains("<domaindnsgethostsresult domain=\"example.com\""));
        let env: Envelope<HostsResponse> = quick_xml::de::from_str(&n).unwrap();
        let hosts = env.command_response.unwrap().result.hosts;
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[1].address, "a & b");
    }

    #[test]
    fn dates_and_base64() {
        assert_eq!(
            parse_nc_date("4/30/2021 11:31:13\u{202f}AM")
                .unwrap()
                .to_rfc3339(),
            "2021-04-30T00:00:00+00:00"
        );
        assert_eq!(
            parse_nc_date("02/15/2016").unwrap().to_rfc3339(),
            "2016-02-15T00:00:00+00:00"
        );
        assert_eq!(base64(b"hello!"), "aGVsbG8h");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(epp_param("abc123"), "abc123");
        assert_eq!(epp_param("a&b"), "base64:YSZi");
    }

    #[test]
    fn error_mapping() {
        assert_eq!(
            map_error("1011150", "Invalid request IP: 1.2.3.4").kind(),
            "auth"
        );
        assert_eq!(map_error("2019166", "Domain not found").kind(), "not_found");
        assert_eq!(
            map_error("3019166", "Domain not available").kind(),
            "conflict"
        );
        assert_eq!(
            map_error("2015182", "Phone invalid").kind(),
            "invalid_input"
        );
        let other = map_error("9999999", "boom");
        assert_eq!(
            other,
            IntegrationError::remote_with_code(None, "9999999", "boom")
        );
    }
}
