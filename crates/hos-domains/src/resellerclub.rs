//! ResellerClub / LogicBoxes HTTP API (`https://httpapi.com/api/`, test
//! `https://test.httpapi.com/api/`).
//!
//! Reference: ResellerClub KB "HTTP API" (`manage.resellerclub.com/kb/answer/<id>`;
//! the same articles are mirrored on every LogicBoxes brand, e.g.
//! `cp.us2.net/kb/answer/<id>`). Article ids are cited next to each call.
//!
//! ## Transport
//!
//! * Authentication is `auth-userid` + `api-key` as request parameters.
//!   Read-only calls are `GET` (the key travels in the query string, as the API
//!   requires); every mutating call is a form-encoded `POST` with **all**
//!   parameters — including the key — in the body (KB 753: production rejects
//!   `GET` for mutations).
//! * Errors come back as HTTP 500 with `{"status":"ERROR","message":..}`
//!   (DNS calls use an `error` key). Those are business errors and are mapped
//!   to non-retryable variants; "Access Denied" (wrong key **or** the server IP
//!   is not whitelisted in the reseller panel) maps to
//!   [`IntegrationError::Auth`] with a hint.
//!
//! ## Customers and contacts
//!
//! Every order belongs to a ResellerClub *customer*; contacts belong to a
//! customer. The registrant's email identifies the customer
//! (`customers/details.json?username=`), created with `customers/signup.json`
//! (random password — the customer never logs in at ResellerClub) when missing,
//! unless `default_customer_id` is configured. Contacts are searched under the
//! customer (`contacts/search.json`) and reused when name, company, email,
//! address and phone match exactly; otherwise `contacts/add.json`. Contact
//! roles that a TLD does not use are sent as `-1` (KB 752).
//!
//! ## Extra data
//!
//! * `DomainRegistration::extra.attributes` / `DomainTransfer::extra.attributes`
//!   — raw order `attr-nameN/attr-valueN` pairs.
//! * `ContactInfo::extra.attributes` — raw contact attributes; the neutral keys
//!   `us_purpose`, `us_nexus_category`, `ca_legal_type`, `eu_citizenship_country`
//!   are translated to `purpose`, `category`, `CPR`, `countryOfCitizenship`
//!   (KB 790).
//! * `extra.idn_language_code` — `idnLanguageCode` for IDN registrations
//!   (defaults to `tur` for Turkish-script `.com`/`.net` names).
//! * `extra.invoice_option` — overrides `invoice-option` (default `NoInvoice`).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Months, TimeZone, Utc};
use hos_core::IntegrationError;
use hos_core::domain::DomainStatus;
use hos_core::integrations::registrar::{
    ContactInfo, DnsRecord, DnsRecordType, DomainAvailability, DomainContacts, DomainInfo,
    DomainOperation, DomainOperationStatus, DomainRegistrar, DomainRegistration, DomainRenewal,
    DomainTransfer, TldPrice,
};
use hos_core::integrations::{ConnectionInfo, FieldReader, PhoneNumber, PostalAddress, kinds};
use hos_core::money::{Currency, Money};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use rand::RngExt as _;
use reqwest::Method;
use serde_json::{Value, json};
use url::Url;

use crate::common;
use crate::http::{HttpClient, RawResponse, truncate};
use crate::name::{self, DomainName};
use crate::tld_rules;

/// Live endpoint.
pub const LIVE_URL: &str = "https://httpapi.com/api/";
/// Test (demo) endpoint.
pub const TEST_URL: &str = "https://test.httpapi.com/api/";

/// TLDs for which ResellerClub does not sell privacy protection (KB 752).
const NO_PRIVACY: &[&str] = &[
    "asia", "au", "ca", "cl", "cn", "org.co", "mil.co", "gov.co", "edu.co", "de", "es", "eu", "fr",
    "in", "nl", "nz", "pro", "ru", "sx", "tel", "uk", "us",
];

/// Record types searchable through `dns/manage/search-records.json` (KB 1106).
const DNS_TYPES: &[DnsRecordType] = &[
    DnsRecordType::A,
    DnsRecordType::Aaaa,
    DnsRecordType::Cname,
    DnsRecordType::Mx,
    DnsRecordType::Ns,
    DnsRecordType::Txt,
    DnsRecordType::Srv,
];

/// Where `tld_pricing` reads prices from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    /// `products/reseller-cost-price.json` — what the reseller pays (default).
    ResellerCost,
    /// `products/customer-price.json` — selling prices configured in the panel.
    CustomerPrice,
}

/// Connection settings.
#[derive(Debug, Clone)]
pub struct ResellerClubConfig {
    /// API base (ends with `/api/`).
    pub base_url: Url,
    /// Reseller id (`auth-userid`).
    pub reseller_id: String,
    /// API key.
    pub api_key: SecretString,
    /// Request timeout.
    pub timeout: Duration,
    /// Currency of returned prices; `None` = ask `resellers/details.json`.
    pub currency: Option<Currency>,
    /// Put every order under this customer instead of one per registrant email.
    pub default_customer_id: Option<String>,
    /// Nameservers used when a registration has none.
    pub default_nameservers: Vec<String>,
    /// Price source for [`DomainRegistrar::tld_pricing`].
    pub price_source: PriceSource,
    /// `lang-pref` for created customers.
    pub language: String,
    /// Which destinations the client may contact (default: the process-wide
    /// `HOS_OUTBOUND_POLICY`; tests against a local mock server use
    /// [`OutboundPolicy::Unrestricted`]).
    pub outbound: OutboundPolicy,
}

impl ResellerClubConfig {
    /// Minimal config against `base_url` (tests, custom brands).
    pub fn new(base_url: Url, reseller_id: impl Into<String>, api_key: SecretString) -> Self {
        Self {
            base_url,
            reseller_id: reseller_id.into(),
            api_key,
            outbound: OutboundPolicy::global(),
            timeout: crate::http::DEFAULT_TIMEOUT,
            currency: None,
            default_customer_id: None,
            default_nameservers: Vec::new(),
            price_source: PriceSource::ResellerCost,
            language: "en".into(),
        }
    }

    /// Reads `integrations.config` / `secrets` (see [`crate::descriptors`]).
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let sandbox = c.bool_or("sandbox", false)?;
        let base_url = match c.opt_url("base_url")? {
            Some(u) => u,
            None => Url::parse(if sandbox { TEST_URL } else { LIVE_URL })
                .map_err(|e| IntegrationError::Config(e.to_string()))?,
        };
        let mut cfg = Self::new(
            ensure_slash(base_url),
            c.str("reseller_id")?,
            s.secret("api_key")?,
        );
        cfg.timeout = Duration::from_secs(c.u64_or("timeout_secs", 30)?);
        cfg.currency = c
            .opt_str("currency")?
            .map(|code| {
                Currency::new(code).map_err(|_| {
                    IntegrationError::Config("config.currency must be an ISO 4217 code".into())
                })
            })
            .transpose()?;
        cfg.default_customer_id = c.opt_str("default_customer_id")?.map(str::to_owned);
        cfg.default_nameservers = common::nameservers(&common::split_list(
            c.opt_str("default_nameservers")?,
        ))
        .map_err(|e| IntegrationError::Config(format!("config.default_nameservers: {e}")))?;
        cfg.price_source = match c.string_or("price_source", "reseller_cost")?.as_str() {
            "customer_price" => PriceSource::CustomerPrice,
            _ => PriceSource::ResellerCost,
        };
        cfg.language = c.string_or("language", "en")?;
        if !cfg.reseller_id.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(IntegrationError::Config(
                "config.reseller_id must be numeric".into(),
            ));
        }
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

/// Request parameters (repeated keys allowed).
#[derive(Debug, Default, Clone)]
struct Params(Vec<(String, String)>);

impl Params {
    fn new() -> Self {
        Self::default()
    }
    fn add(mut self, key: &str, value: impl Into<String>) -> Self {
        self.0.push((key.to_owned(), value.into()));
        self
    }
    fn add_all<I: IntoIterator<Item = S>, S: Into<String>>(mut self, key: &str, values: I) -> Self {
        for v in values {
            self.0.push((key.to_owned(), v.into()));
        }
        self
    }
    fn attrs(mut self, attrs: &[(String, String)]) -> Self {
        for (i, (k, v)) in attrs.iter().enumerate() {
            self.0.push((format!("attr-name{}", i + 1), k.clone()));
            self.0.push((format!("attr-value{}", i + 1), v.clone()));
        }
        self
    }
}

/// ResellerClub registrar. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct ResellerClub {
    cfg: ResellerClubConfig,
    http: HttpClient,
}

impl ResellerClub {
    /// Builds the client.
    pub fn new(cfg: ResellerClubConfig) -> Result<Self, IntegrationError> {
        let http = HttpClient::new("resellerclub", cfg.timeout, &[&cfg.api_key], cfg.outbound)?;
        http.check_endpoint(&cfg.base_url)?;
        Ok(Self { cfg, http })
    }

    // ------------------------------------------------------------------
    // transport
    // ------------------------------------------------------------------

    async fn call(
        &self,
        method: Method,
        path: &str,
        params: Params,
        op: &'static str,
        extra_secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        let url = self
            .cfg
            .base_url
            .join(path)
            .map_err(|e| IntegrationError::Config(format!("invalid ResellerClub URL: {e}")))?;
        let mut all = vec![
            ("auth-userid".to_owned(), self.cfg.reseller_id.clone()),
            (
                "api-key".to_owned(),
                self.cfg.api_key.expose_str().to_owned(),
            ),
        ];
        all.extend(params.0);
        let client = self.http.client();
        let req = if method == Method::GET {
            client.get(url).query(&all)
        } else {
            client.post(url).form(&all)
        };
        let resp = self.http.execute(req, op).await?;
        self.parse(resp, extra_secrets)
    }

    async fn get(
        &self,
        path: &str,
        params: Params,
        op: &'static str,
    ) -> Result<Value, IntegrationError> {
        self.call(Method::GET, path, params, op, &[]).await
    }

    async fn post(
        &self,
        path: &str,
        params: Params,
        op: &'static str,
    ) -> Result<Value, IntegrationError> {
        self.call(Method::POST, path, params, op, &[]).await
    }

    fn parse(&self, resp: RawResponse, extra_secrets: &[&str]) -> Result<Value, IntegrationError> {
        let json: Option<Value> = serde_json::from_str(resp.body.trim()).ok();
        let error_message = json.as_ref().and_then(error_message);
        if let Some(msg) = error_message {
            return Err(classify(
                resp.status,
                &self.http.scrub_with(&msg, extra_secrets),
            ));
        }
        if resp.status == 429 {
            return Err(crate::http::status_error(429, &resp.headers, ""));
        }
        if !(200..300).contains(&resp.status) {
            // No recognised error field: describe, never quote, the body.
            let msg = format!(
                "HTTP {}: {}",
                resp.status,
                crate::http::describe_body(&resp.body)
            );
            return Err(match resp.status {
                401 | 403 => auth_error(&msg),
                s if s >= 500 => IntegrationError::remote(Some(s), msg),
                s => IntegrationError::from_http_status(s, msg),
            });
        }
        json.ok_or_else(|| {
            IntegrationError::Transport(format!(
                "resellerclub: response is not JSON ({})",
                crate::http::describe_body(&resp.body)
            ))
        })
    }

    // ------------------------------------------------------------------
    // lookups
    // ------------------------------------------------------------------

    /// `domains/orderid.json` (KB 763) — bare integer.
    async fn order_id(&self, domain: &DomainName) -> Result<String, IntegrationError> {
        let v = self
            .get(
                "domains/orderid.json",
                Params::new().add("domain-name", domain.as_str()),
                "domains.orderid",
            )
            .await
            .map_err(|e| not_found_as(e, domain))?;
        scalar(&v).ok_or_else(|| {
            IntegrationError::Transport("resellerclub: unexpected orderid response".into())
        })
    }

    /// `domains/details-by-name.json` (KB 1755).
    async fn details(
        &self,
        domain: &DomainName,
        options: &[&str],
    ) -> Result<Value, IntegrationError> {
        self.get(
            "domains/details-by-name.json",
            Params::new()
                .add("domain-name", domain.as_str())
                .add_all("options", options.iter().copied()),
            "domains.details",
        )
        .await
        .map_err(|e| not_found_as(e, domain))
    }

    // ------------------------------------------------------------------
    // customers & contacts
    // ------------------------------------------------------------------

    /// Resolves (or creates) the customer owning orders of `registrant`.
    async fn customer_for(&self, registrant: &ContactInfo) -> Result<String, IntegrationError> {
        if let Some(id) = &self.cfg.default_customer_id {
            return Ok(id.clone());
        }
        let email = registrant.email.trim().to_ascii_lowercase();
        match self
            .get(
                "customers/details.json",
                Params::new().add("username", email.clone()),
                "customers.details",
            )
            .await
        {
            Ok(v) => {
                return field_str(&v, "customerid").ok_or_else(|| {
                    IntegrationError::Transport(
                        "resellerclub: customer details without customerid".into(),
                    )
                });
            }
            Err(IntegrationError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        let password = generate_password();
        let addr = &registrant.address;
        let params = Params::new()
            .add("username", email)
            .add("passwd", password.expose_str())
            .add("name", rc_name(registrant))
            .add("company", rc_company(registrant, "Contact"))
            .add("address-line-1", addr.line1.trim())
            .add_all(
                "address-line-2",
                addr.line2.iter().filter(|s| !s.trim().is_empty()).cloned(),
            )
            .add("city", addr.city.trim())
            .add("state", rc_state(addr))
            .add("country", addr.country_code.trim().to_ascii_uppercase())
            .add("zipcode", rc_zip(addr))
            .add("phone-cc", registrant.phone.country_code.clone())
            .add("phone", registrant.phone.number.clone())
            .add("lang-pref", self.cfg.language.clone());
        let v = self
            .call(
                Method::POST,
                "customers/signup.json",
                params,
                "customers.signup",
                &[password.expose_str()],
            )
            .await?;
        scalar(&v).ok_or_else(|| {
            IntegrationError::Transport("resellerclub: unexpected signup response".into())
        })
    }

    /// Finds an identical contact under `customer_id` or creates it.
    async fn contact_id(
        &self,
        customer_id: &str,
        contact: &ContactInfo,
        contact_type: &str,
        attributes: &[(String, String)],
        cache: &mut Vec<(ContactInfo, String, String)>,
    ) -> Result<String, IntegrationError> {
        if let Some((_, _, id)) = cache
            .iter()
            .find(|(c, t, _)| c == contact && t == contact_type)
        {
            return Ok(id.clone());
        }
        let wanted = ContactFingerprint::from_contact(contact, contact_type);
        let found = self
            .get(
                "contacts/search.json",
                Params::new()
                    .add("customer-id", customer_id)
                    .add("no-of-records", "50")
                    .add("page-no", "1")
                    .add("email", contact.email.trim())
                    .add("type", contact_type),
                "contacts.search",
            )
            .await;
        let existing = match found {
            Ok(v) => v.get("result").and_then(Value::as_array).and_then(|rows| {
                rows.iter()
                    .filter(|r| {
                        !matches!(
                            field_str(r, "entity.currentstatus").as_deref(),
                            Some("Deleted" | "Archived")
                        )
                    })
                    .find(|r| ContactFingerprint::from_search(r) == wanted)
                    .and_then(|r| {
                        field_str(r, "entity.entityid")
                            .or_else(|| field_str(r, "contact.contactid"))
                    })
            }),
            Err(IntegrationError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        let id = match existing {
            Some(id) => id,
            None => {
                let addr = &contact.address;
                let mut params = Params::new()
                    .add("name", rc_name(contact))
                    .add("company", rc_company(contact, contact_type))
                    .add("email", contact.email.trim())
                    .add("address-line-1", addr.line1.trim())
                    .add_all(
                        "address-line-2",
                        addr.line2.iter().filter(|s| !s.trim().is_empty()).cloned(),
                    )
                    .add("city", addr.city.trim())
                    .add("state", rc_state(addr))
                    .add("country", addr.country_code.trim().to_ascii_uppercase())
                    .add("zipcode", rc_zip(addr))
                    .add("phone-cc", contact.phone.country_code.clone())
                    .add("phone", contact.phone.number.clone())
                    .add("customer-id", customer_id)
                    .add("type", contact_type);
                if let Some(fax) = contact.fax.as_ref().filter(|f| !f.number.is_empty()) {
                    params = params
                        .add("fax-cc", fax.country_code.clone())
                        .add("fax", fax.number.clone());
                }
                let v = self
                    .post(
                        "contacts/add.json",
                        params.attrs(attributes),
                        "contacts.add",
                    )
                    .await?;
                scalar(&v).ok_or_else(|| {
                    IntegrationError::Transport(
                        "resellerclub: unexpected contacts/add response".into(),
                    )
                })?
            }
        };
        cache.push((contact.clone(), contact_type.to_owned(), id.clone()));
        Ok(id)
    }

    /// Resolves the four contact ids for `domain` (`-1` for unused roles).
    async fn contact_ids(
        &self,
        customer_id: &str,
        domain: &DomainName,
        contacts: &DomainContacts,
        registration_extra: &Value,
    ) -> Result<[String; 4], IntegrationError> {
        let tld = domain.tld();
        let roles = RoleUsage::for_tld(tld);
        let ctype = contact_type(tld);
        let mut cache = Vec::new();
        let reg_attrs = registrant_attributes(tld, &contacts.registrant, registration_extra);
        common::validate_contact("registrant", &contacts.registrant)?;
        let registrant = self
            .contact_id(
                customer_id,
                &contacts.registrant,
                ctype,
                &reg_attrs,
                &mut cache,
            )
            .await?;
        let mut ids = [registrant, String::new(), String::new(), String::new()];
        for (i, (role, used, contact)) in [
            ("admin", roles.admin, &contacts.admin),
            ("tech", roles.tech, &contacts.tech),
            ("billing", roles.billing, &contacts.billing),
        ]
        .into_iter()
        .enumerate()
        {
            ids[i + 1] = if used {
                common::validate_contact(role, contact)?;
                let attrs = common::raw_attributes(&contact.extra);
                // Same person as the registrant → same contact object.
                if contact == &contacts.registrant {
                    ids[0].clone()
                } else {
                    self.contact_id(customer_id, contact, ctype, &attrs, &mut cache)
                        .await?
                }
            } else {
                "-1".to_owned()
            };
        }
        Ok(ids)
    }

    async fn default_ns(&self, customer_id: &str) -> Result<Vec<String>, IntegrationError> {
        if !self.cfg.default_nameservers.is_empty() {
            return Ok(self.cfg.default_nameservers.clone());
        }
        // KB 788 — JSON array of strings.
        let v = self
            .get(
                "domains/customer-default-ns.json",
                Params::new().add("customer-id", customer_id),
                "domains.customer_default_ns",
            )
            .await?;
        let ns: Vec<String> = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        common::required_nameservers(&ns)
    }

    async fn currency(&self, cost: bool) -> Result<Currency, IntegrationError> {
        if let Some(c) = self.cfg.currency {
            return Ok(c);
        }
        let v = self
            .get("resellers/details.json", Params::new(), "resellers.details")
            .await?;
        let code = if cost {
            field_str(&v, "parentsellingcurrencysymbol")
                .or_else(|| field_str(&v, "sellingcurrencysymbol"))
        } else {
            field_str(&v, "sellingcurrencysymbol")
        };
        code.and_then(|c| Currency::new(&c).ok()).ok_or_else(|| {
            IntegrationError::Config(
                "cannot determine the ResellerClub account currency; set config.currency".into(),
            )
        })
    }

    fn privacy_supported(tld: &str) -> bool {
        !NO_PRIVACY.contains(&tld)
            && !NO_PRIVACY.iter().any(|t| tld.ends_with(&format!(".{t}")))
            && tld_rules::rule_for(tld).privacy_allowed
    }

    fn invoice_option(extra: &Value) -> String {
        tld_rules::extra_str(extra, "invoice_option").unwrap_or_else(|| "NoInvoice".into())
    }

    // ------------------------------------------------------------------
    // DNS helpers
    // ------------------------------------------------------------------

    async fn search_records(
        &self,
        domain: &DomainName,
        rtype: DnsRecordType,
    ) -> Result<Vec<DnsRecord>, IntegrationError> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let v = self
                .get(
                    "dns/manage/search-records.json",
                    Params::new()
                        .add("domain-name", domain.as_str())
                        .add("type", rtype.as_str())
                        .add("no-of-records", "50")
                        .add("page-no", page.to_string()),
                    "dns.search_records",
                )
                .await?;
            let total = field_str(&v, "recsindb")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let mut keys: Vec<(u32, &Value)> = v
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, rec)| k.parse::<u32>().ok().map(|n| (n, rec)))
                        .collect()
                })
                .unwrap_or_default();
            keys.sort_by_key(|(n, _)| *n);
            let got = keys.len();
            for (_, rec) in keys {
                if let Some(r) = record_from_rc(domain, rtype, rec) {
                    out.push(r);
                }
            }
            if got == 0 || out.len() >= total || page >= 20 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    async fn dns_mutation(
        &self,
        domain: &DomainName,
        path: String,
        params: Params,
        op: &'static str,
    ) -> Result<(), IntegrationError> {
        match self.post(&path, params.clone(), op).await {
            Ok(_) => Ok(()),
            Err(
                first @ (IntegrationError::NotFound(_)
                | IntegrationError::Remote { status: None, .. }
                | IntegrationError::InvalidInput(_)),
            ) if op == "dns.add" => {
                // The zone may not exist yet: activate DNS for the order (KB 830) and retry once.
                let order_id = self.order_id(domain).await?;
                if let Err(e) = self
                    .post(
                        "dns/activate.json",
                        Params::new().add("order-id", order_id),
                        "dns.activate",
                    )
                    .await
                {
                    tracing::debug!(error = %e, "resellerclub dns/activate failed");
                    return Err(first);
                }
                self.post(&path, params, op).await.map(|_| ())
            }
            Err(e) => Err(e),
        }
    }
}

// ----------------------------------------------------------------------
// helpers (pure)
// ----------------------------------------------------------------------

/// Extracts an error message from a ResellerClub JSON body, if it is an error.
fn error_message(v: &Value) -> Option<String> {
    let obj = v.as_object()?;
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let pick = || {
        ["message", "error", "msg", "actionstatusdesc"]
            .iter()
            .find_map(|k| match obj.get(*k) {
                Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
                Some(other @ (Value::Object(_) | Value::Array(_))) => Some(other.to_string()),
                _ => None,
            })
    };
    if status.eq_ignore_ascii_case("error") || status.eq_ignore_ascii_case("failed") {
        return Some(pick().unwrap_or_else(|| "unspecified error".into()));
    }
    if obj
        .get("actionstatus")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("failed"))
    {
        return Some(pick().unwrap_or_else(|| "action failed".into()));
    }
    if !status.eq_ignore_ascii_case("success")
        && let Some(Value::String(e)) = obj.get("error")
        && !e.trim().is_empty()
    {
        return Some(e.trim().to_owned());
    }
    None
}

fn auth_error(msg: &str) -> IntegrationError {
    IntegrationError::Auth(format!(
        "{msg} — check the reseller id / API key and that this server's IP address is whitelisted in the ResellerClub panel (Settings → API)"
    ))
}

/// Maps a business error message to the most specific variant.
fn classify(http_status: u16, msg: &str) -> IntegrationError {
    let lower = msg.to_ascii_lowercase();
    let msg = truncate(msg, 400);
    if http_status == 401
        || http_status == 403
        || lower.contains("access denied")
        || lower.contains("not allowed to perform")
        || lower.contains("whitelist")
        || lower.contains("invalid api key")
        || lower.contains("authentication")
        || lower.contains("auth-userid")
    {
        return auth_error(&msg);
    }
    if lower.contains("too many") || lower.contains("rate limit") {
        return IntegrationError::RateLimited { retry_after: None };
    }
    if lower.contains("not found")
        || lower.contains("no entity found")
        || lower.contains("doesn't exist")
        || lower.contains("does not exist")
        || lower.contains("no such")
    {
        return IntegrationError::NotFound(msg);
    }
    if lower.contains("already registered")
        || lower.contains("already exists")
        || lower.contains("not available")
        || lower.contains("is not available for registration")
    {
        return IntegrationError::Conflict(msg);
    }
    if lower.contains("insufficient") || lower.contains("funds") || lower.contains("balance") {
        return IntegrationError::remote_with_code(None, "insufficient_funds", msg);
    }
    if lower.starts_with('{')
        || lower.contains("invalid")
        || lower.contains("required")
        || lower.contains("must be")
    {
        return IntegrationError::InvalidInput(msg);
    }
    // Business errors arrive as HTTP 500 but are not transient.
    IntegrationError::remote(None, msg)
}

fn not_found_as(err: IntegrationError, domain: &DomainName) -> IntegrationError {
    match err {
        IntegrationError::NotFound(_) => {
            IntegrationError::NotFound(format!("{domain} is not in this ResellerClub account"))
        }
        other => other,
    }
}

/// A bare integer/string response (`orderid`, `signup`, `contacts/add`).
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        _ => None,
    }
}

fn field_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(scalar)
}

fn field_bool(v: &Value, key: &str) -> Option<bool> {
    match v.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn epoch(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    let secs: i64 = field_str(v, key)?.parse().ok()?;
    (secs > 0)
        .then(|| Utc.timestamp_opt(secs, 0).single())
        .flatten()
}

fn rc_name(c: &ContactInfo) -> String {
    let n = c.full_name();
    if n.is_empty() {
        c.company.clone().unwrap_or_default().trim().to_owned()
    } else {
        n
    }
}

fn rc_company(c: &ContactInfo, contact_type: &str) -> String {
    match c.company.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s.to_owned(),
        // KB 790: "N/A" for individuals; EuContact uses "NA"; RuContact needs two words.
        _ => match contact_type {
            "EuContact" => "NA".into(),
            "RuContact" => "Not Applicable".into(),
            _ => "N/A".into(),
        },
    }
}

fn rc_state(a: &PostalAddress) -> String {
    a.state
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| "Not Applicable".to_owned(), str::to_owned)
}

fn rc_zip(a: &PostalAddress) -> String {
    a.postal_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| "00000".to_owned(), str::to_owned)
}

/// ResellerClub contact `type` for a TLD (KB 790).
fn contact_type(tld: &str) -> &'static str {
    let top = tld.rsplit('.').next().unwrap_or(tld);
    match top {
        "uk" => "UkContact",
        "eu" => "EuContact",
        "ca" => "CaContact",
        "cn" => "CnContact",
        "co" => "CoContact",
        "de" => "DeContact",
        "es" => "EsContact",
        "fr" => "FrContact",
        "nl" => "NlContact",
        "ru" => "RuContact",
        "nyc" => "NycContact",
        "cl" => "ClContact",
        "mx" | "lat" => "MxContact",
        _ => "Contact",
    }
}

/// Which optional roles a TLD uses (`false` → send `-1`; KB 752).
#[derive(Debug, Clone, Copy)]
struct RoleUsage {
    admin: bool,
    tech: bool,
    billing: bool,
}

impl RoleUsage {
    fn for_tld(tld: &str) -> Self {
        let top = tld.rsplit('.').next().unwrap_or(tld);
        Self {
            admin: !matches!(top, "eu" | "ru" | "uk"),
            tech: !matches!(top, "eu" | "fr" | "ru" | "uk"),
            billing: !matches!(
                top,
                "berlin" | "ca" | "eu" | "fr" | "nl" | "nz" | "ru" | "uk" | "london"
            ),
        }
    }
}

/// Registrant contact attributes: neutral keys (KB 790) + raw `attributes`.
fn registrant_attributes(
    tld: &str,
    registrant: &ContactInfo,
    registration_extra: &Value,
) -> Vec<(String, String)> {
    let mut attrs: Vec<(String, String)> = Vec::new();
    let get = |k: &str| {
        tld_rules::extra_str(&registrant.extra, k)
            .or_else(|| tld_rules::extra_str(registration_extra, k))
    };
    let top = tld.rsplit('.').next().unwrap_or(tld);
    match top {
        "us" => {
            if let Some(p) = get("us_purpose") {
                attrs.push(("purpose".into(), p.to_ascii_uppercase()));
            }
            if let Some(c) = get("us_nexus_category") {
                attrs.push(("category".into(), c.to_ascii_uppercase()));
            }
        }
        "ca" => {
            if let Some(c) = get("ca_legal_type") {
                attrs.push(("CPR".into(), c.to_ascii_uppercase()));
            }
        }
        "eu" => {
            if let Some(c) = get("eu_citizenship_country") {
                attrs.push(("countryOfCitizenship".into(), c.to_ascii_uppercase()));
            }
        }
        _ => {}
    }
    for (k, v) in common::raw_attributes(&registrant.extra) {
        if !attrs.iter().any(|(ek, _)| *ek == k) {
            attrs.push((k, v));
        }
    }
    attrs
}

/// Order attributes: raw `extra.attributes`, IDN language, premium flag.
fn order_attributes(domain: &DomainName, extra: &Value, premium: bool) -> Vec<(String, String)> {
    let mut attrs = common::raw_attributes(extra);
    let has = |attrs: &[(String, String)], k: &str| attrs.iter().any(|(ek, _)| ek == k);
    if domain.is_idn() && !has(&attrs, "idnLanguageCode") {
        let explicit = tld_rules::extra_str(extra, "idn_language_code");
        let default_tr = (matches!(domain.tld(), "com" | "net")
            && name::has_turkish_chars(&domain.sld_unicode()))
        .then(|| "tur".to_owned());
        if let Some(code) = explicit.or(default_tr) {
            attrs.push(("idnLanguageCode".into(), code));
        }
    }
    if premium && !has(&attrs, "premium") {
        attrs.push(("premium".into(), "true".into()));
    }
    attrs
}

fn generate_password() -> SecretString {
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGIT: &[u8] = b"23456789";
    const SPECIAL: &[u8] = b"!@#$%*";
    let mut rng = rand::rng();
    let mut pick = |set: &[u8]| char::from(set[rng.random_range(0..set.len())]);
    let mut chars: Vec<char> = vec![pick(LOWER), pick(UPPER), pick(DIGIT), pick(SPECIAL)];
    let all: Vec<u8> = [LOWER, UPPER, DIGIT].concat();
    while chars.len() < 15 {
        chars.push(pick(&all));
    }
    // Shuffle (Fisher–Yates).
    let mut rng = rand::rng();
    for i in (1..chars.len()).rev() {
        let j = rng.random_range(0..=i);
        chars.swap(i, j);
    }
    SecretString::from(chars.into_iter().collect::<String>())
}

/// Normalised comparable form of a contact.
#[derive(Debug, PartialEq, Eq)]
struct ContactFingerprint(Vec<String>);

impl ContactFingerprint {
    fn norm(s: &str) -> String {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    fn from_contact(c: &ContactInfo, contact_type: &str) -> Self {
        let a = &c.address;
        Self(
            [
                rc_name(c),
                rc_company(c, contact_type),
                c.email.clone(),
                a.line1.clone(),
                a.city.clone(),
                a.country_code.clone(),
                rc_zip(a),
                c.phone.country_code.clone(),
                c.phone.number.clone(),
            ]
            .iter()
            .map(|s| Self::norm(s))
            .collect(),
        )
    }

    fn from_search(row: &Value) -> Self {
        let f = |k: &str| field_str(row, k).unwrap_or_default();
        Self(
            [
                f("contact.name"),
                f("contact.company"),
                f("contact.emailaddr"),
                f("contact.address1"),
                f("contact.city"),
                f("contact.country"),
                f("contact.zip"),
                f("contact.telnocc"),
                f("contact.telno"),
            ]
            .iter()
            .map(|s| Self::norm(s))
            .collect(),
        )
    }
}

fn status_from_details(v: &Value, now: DateTime<Utc>) -> DomainStatus {
    let current = field_str(v, "currentstatus").unwrap_or_default();
    let expires = epoch(v, "endtime");
    let action = field_str(v, "actiontype")
        .unwrap_or_default()
        .to_ascii_lowercase();
    match current.as_str() {
        "Active" | "Suspended" => {
            if expires.is_some_and(|e| e < now) {
                DomainStatus::Grace
            } else {
                DomainStatus::Active
            }
        }
        "InActive" => {
            if action.contains("transfer") {
                DomainStatus::PendingTransfer
            } else {
                DomainStatus::PendingRegistration
            }
        }
        "Pending Delete Restorable" => DomainStatus::Redemption,
        "Deleted" | "Archived" => {
            if expires.is_some_and(|e| e < now) {
                DomainStatus::Expired
            } else {
                DomainStatus::Cancelled
            }
        }
        _ => DomainStatus::Pending,
    }
}

fn nameservers_from_details(v: &Value) -> Vec<String> {
    let mut ns: Vec<(u32, String)> = v
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| {
                    let n: u32 = k.strip_prefix("ns")?.parse().ok()?;
                    let host = val
                        .as_str()?
                        .trim()
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    (!host.is_empty()).then_some((n, host))
                })
                .collect()
        })
        .unwrap_or_default();
    ns.sort_by_key(|(n, _)| *n);
    ns.into_iter().map(|(_, h)| h).collect()
}

fn contact_from_details(v: &Value) -> Option<ContactInfo> {
    let obj = v.as_object()?;
    let s = |k: &str| obj.get(k).and_then(scalar).unwrap_or_default();
    let name = s("name");
    let (first, last) = match name.split_once(' ') {
        Some((f, l)) => (f.to_owned(), l.trim().to_owned()),
        None => (name.clone(), String::new()),
    };
    let company = s("company");
    Some(ContactInfo {
        first_name: first,
        last_name: last,
        company: (!company.is_empty() && company != "N/A" && company != "NA").then_some(company),
        email: s("emailaddr"),
        phone: PhoneNumber::new(&s("telnocc"), &s("telno")),
        fax: None,
        address: PostalAddress {
            line1: s("address1"),
            line2: Some(s("address2")).filter(|x| !x.is_empty()),
            city: s("city"),
            state: Some(s("state")).filter(|x| !x.is_empty() && x != "Not Applicable"),
            postal_code: Some(s("zip")).filter(|x| !x.is_empty()),
            country_code: s("country"),
        },
        tax_id: None,
        extra: json!({ "resellerclub_contact_id": s("contactid") }),
    })
}

/// DNS record id: `TYPE|host|value` (ResellerClub records have no ids).
fn record_id(rtype: DnsRecordType, host: &str, value: &str) -> String {
    format!("{}|{host}|{value}", rtype.as_str())
}

fn parse_record_id(id: &str) -> Option<(DnsRecordType, String, String)> {
    let mut parts = id.splitn(3, '|');
    let t: DnsRecordType = parts.next()?.parse().ok()?;
    Some((t, parts.next()?.to_owned(), parts.next()?.to_owned()))
}

/// `host` as stored by ResellerClub → zone-relative name (`@` for the apex).
fn relative_name(domain: &DomainName, host: &str) -> String {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let d = domain.as_str();
    if h.is_empty() || h == "@" || h == d {
        "@".into()
    } else if let Some(prefix) = h.strip_suffix(&format!(".{d}")) {
        prefix.to_owned()
    } else {
        h
    }
}

fn record_from_rc(domain: &DomainName, rtype: DnsRecordType, rec: &Value) -> Option<DnsRecord> {
    let host = field_str(rec, "host").unwrap_or_default();
    let value = field_str(rec, "value")?;
    let ttl = field_str(rec, "timetolive").and_then(|t| t.parse().ok());
    let priority = field_str(rec, "priority").and_then(|p| p.parse().ok());
    let display_value = if rtype == DnsRecordType::Srv {
        let weight = field_str(rec, "weight").unwrap_or_else(|| "0".into());
        let port = field_str(rec, "port").unwrap_or_else(|| "0".into());
        format!("{weight} {port} {value}")
    } else {
        value.clone()
    };
    Some(DnsRecord {
        id: Some(record_id(rtype, &host, &value)),
        name: relative_name(domain, &host),
        record_type: rtype,
        value: display_value,
        ttl,
        priority,
    })
}

fn dns_slug(t: DnsRecordType) -> Result<&'static str, IntegrationError> {
    Ok(match t {
        DnsRecordType::A => "ipv4",
        DnsRecordType::Aaaa => "ipv6",
        DnsRecordType::Cname => "cname",
        DnsRecordType::Mx => "mx",
        DnsRecordType::Ns => "ns",
        DnsRecordType::Txt => "txt",
        DnsRecordType::Srv => "srv",
        DnsRecordType::Caa => {
            return Err(IntegrationError::InvalidInput(
                "ResellerClub DNS does not support CAA records".into(),
            ));
        }
    })
}

/// SRV `value` = `weight port target`.
fn split_srv(value: &str) -> Result<(String, String, String), IntegrationError> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    match parts.as_slice() {
        [w, p, t] => Ok((
            (*w).to_owned(),
            (*p).to_owned(),
            (*t).trim_end_matches('.').to_owned(),
        )),
        _ => Err(IntegrationError::InvalidInput(
            "SRV value must be \"weight port target\"".into(),
        )),
    }
}

fn host_param(name: &str) -> Option<String> {
    let n = name.trim();
    (!(n.is_empty() || n == "@")).then(|| n.to_owned())
}

// ----------------------------------------------------------------------
// trait
// ----------------------------------------------------------------------

#[async_trait]
impl DomainRegistrar for ResellerClub {
    fn kind(&self) -> &'static str {
        kinds::RESELLERCLUB
    }

    /// `resellers/details.json` (KB 809).
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let v = self
            .get("resellers/details.json", Params::new(), "resellers.details")
            .await?;
        let currency = field_str(&v, "sellingcurrencysymbol").unwrap_or_default();
        Ok(ConnectionInfo {
            server_version: Some(format!(
                "ResellerClub HTTP API ({})",
                self.cfg.base_url.host_str().unwrap_or("?")
            )),
            details: Some(
                format!(
                    "reseller {} {}",
                    field_str(&v, "resellerid").unwrap_or_else(|| self.cfg.reseller_id.clone()),
                    if currency.is_empty() {
                        String::new()
                    } else {
                        format!("({currency})")
                    }
                )
                .trim()
                .to_owned(),
            ),
        })
    }

    /// `domains/available.json` (KB 764): names sharing a TLD set are batched,
    /// at most 5 names per call (Donuts registries answer `unknown` above that).
    async fn check_availability(
        &self,
        domains: &[String],
    ) -> Result<Vec<DomainAvailability>, IntegrationError> {
        let mut results: Vec<Option<DomainAvailability>> = vec![None; domains.len()];
        let mut groups: BTreeMap<Vec<String>, Vec<(String, Vec<usize>)>> = BTreeMap::new();
        let mut per_sld: BTreeMap<String, (Vec<String>, Vec<usize>)> = BTreeMap::new();
        let mut parsed: Vec<Option<DomainName>> = Vec::with_capacity(domains.len());
        for (i, d) in domains.iter().enumerate() {
            match DomainName::parse(d) {
                Ok(n) => {
                    let entry = per_sld.entry(n.sld().to_owned()).or_default();
                    if !entry.0.contains(&n.tld().to_owned()) {
                        entry.0.push(n.tld().to_owned());
                    }
                    entry.1.push(i);
                    parsed.push(Some(n));
                }
                Err(e) => {
                    results[i] = Some(DomainAvailability {
                        domain: d.trim().to_ascii_lowercase(),
                        available: false,
                        premium: false,
                        register_price: None,
                        renew_price: None,
                        reason: Some(e.to_string()),
                    });
                    parsed.push(None);
                }
            }
        }
        for (sld, (mut tlds, idx)) in per_sld {
            tlds.sort();
            groups.entry(tlds).or_default().push((sld, idx));
        }
        let currency = self.cfg.currency;
        for (tlds, slds) in groups {
            for chunk in slds.chunks(5) {
                let v = self
                    .get(
                        "domains/available.json",
                        Params::new()
                            .add_all("domain-name", chunk.iter().map(|(s, _)| s.clone()))
                            .add_all("tlds", tlds.iter().cloned()),
                        "domains.available",
                    )
                    .await?;
                for (_, idx) in chunk {
                    for &i in idx {
                        let Some(name) = parsed[i].as_ref() else {
                            continue;
                        };
                        let entry = v
                            .get(name.as_str())
                            .or_else(|| v.get(name.to_unicode().as_str()))
                            .or_else(|| {
                                v.as_object().and_then(|o| {
                                    o.iter()
                                        .find(|(k, _)| k.eq_ignore_ascii_case(name.as_str()))
                                        .map(|(_, val)| val)
                                })
                            });
                        results[i] = Some(availability_from(name, entry, currency));
                    }
                }
            }
        }
        Ok(results
            .into_iter()
            .zip(domains)
            .map(|(r, d)| {
                r.unwrap_or_else(|| DomainAvailability {
                    domain: d.clone(),
                    available: false,
                    premium: false,
                    register_price: None,
                    renew_price: None,
                    reason: Some("no answer".into()),
                })
            })
            .collect())
    }

    /// `products/category-keys-mapping.json` (KB 862) +
    /// `products/reseller-cost-price.json` (KB 1029) or
    /// `products/customer-price.json` (KB 864). The price under year `1` is
    /// taken as the per-year price.
    async fn tld_pricing(&self, tlds: &[String]) -> Result<Vec<TldPrice>, IntegrationError> {
        let wanted: Vec<String> = tlds
            .iter()
            .filter_map(|t| {
                name::normalize_tld(t)
                    .ok()
                    .or_else(|| Some(t.trim_start_matches('.').to_ascii_lowercase()))
            })
            .collect();
        let mapping = self
            .get(
                "products/category-keys-mapping.json",
                Params::new(),
                "products.category_keys",
            )
            .await?;
        let key_to_tlds = product_key_map(&mapping);
        let (path, op, cost) = match self.cfg.price_source {
            PriceSource::ResellerCost => (
                "products/reseller-cost-price.json",
                "products.reseller_cost_price",
                true,
            ),
            PriceSource::CustomerPrice => (
                "products/customer-price.json",
                "products.customer_price",
                false,
            ),
        };
        let prices = self.get(path, Params::new(), op).await?;
        let currency = self.currency(cost).await?;
        let mut out: BTreeMap<String, TldPrice> = BTreeMap::new();
        for (key, tld_list) in key_to_tlds {
            let Some(p) = prices.get(&key) else { continue };
            for tld in tld_list {
                if !wanted.is_empty() && !wanted.contains(&tld) {
                    continue;
                }
                if let Some(tp) = tld_price_from(&tld, p, currency) {
                    out.insert(tld.clone(), tp);
                }
            }
        }
        Ok(out.into_values().collect())
    }

    /// Customer + contacts, then `domains/register.json` (KB 752).
    async fn register(
        &self,
        request: &DomainRegistration,
    ) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        tld_rules::rule_for(name.tld()).validate_registration(request)?;
        common::validate_contact("registrant", &request.contacts.registrant)?;
        let ns = common::nameservers(&request.nameservers)?;
        let customer_id = self.customer_for(&request.contacts.registrant).await?;
        let [reg, admin, tech, billing] = self
            .contact_ids(&customer_id, &name, &request.contacts, &request.extra)
            .await?;
        let ns = if ns.is_empty() {
            self.default_ns(&customer_id).await?
        } else {
            ns
        };
        let privacy = request.privacy && Self::privacy_supported(name.tld());
        let mut params = Params::new()
            .add("domain-name", name.as_str())
            .add("years", request.years.to_string())
            .add_all("ns", ns)
            .add("customer-id", customer_id)
            .add("reg-contact-id", reg)
            .add("admin-contact-id", admin)
            .add("tech-contact-id", tech)
            .add("billing-contact-id", billing)
            .add("invoice-option", Self::invoice_option(&request.extra))
            .add("auto-renew", request.auto_renew.to_string());
        if privacy {
            params = params
                .add("purchase-privacy", "true")
                .add("protect-privacy", "true");
        }
        let attrs = order_attributes(&name, &request.extra, request.premium_price.is_some());
        let v = self
            .post(
                "domains/register.json",
                params.attrs(&attrs),
                "domains.register",
            )
            .await?;
        Ok(operation_from(&name, &v, false, None))
    }

    /// Customer + contacts, then `domains/transfer.json` (KB 758).
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
        common::validate_contact("registrant", &request.contacts.registrant)?;
        let ns = common::nameservers(&request.nameservers)?;
        let customer_id = self.customer_for(&request.contacts.registrant).await?;
        let [reg, admin, tech, billing] = self
            .contact_ids(&customer_id, &name, &request.contacts, &request.extra)
            .await?;
        let privacy = request.privacy && Self::privacy_supported(name.tld());
        let mut params = Params::new()
            .add("domain-name", name.as_str())
            .add("customer-id", customer_id)
            .add("reg-contact-id", reg)
            .add("admin-contact-id", admin)
            .add("tech-contact-id", tech)
            .add("billing-contact-id", billing)
            .add("invoice-option", Self::invoice_option(&request.extra))
            .add("auto-renew", request.auto_renew.to_string())
            .add_all("ns", ns);
        if !code.is_empty() {
            params = params.add("auth-code", code.clone());
        }
        if privacy {
            params = params
                .add("purchase-privacy", "true")
                .add("protect-privacy", "true");
        }
        let attrs = common::raw_attributes(&request.extra);
        let v = self
            .call(
                Method::POST,
                "domains/transfer.json",
                params.attrs(&attrs),
                "domains.transfer",
                &[&code],
            )
            .await?;
        Ok(operation_from(&name, &v, true, None))
    }

    /// `domains/renew.json` (KB 746) with `exp-date`; restores first via
    /// `domains/restore.json` (KB 760) when in redemption and allowed.
    async fn renew(&self, request: &DomainRenewal) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        tld_rules::rule_for(name.tld()).validate_years(request.years)?;
        let (order_id, expiry, status, auto_renew) =
            if request.current_expiry.is_some() && !request.restore_if_needed {
                (
                    self.order_id(&name).await?,
                    request.current_expiry,
                    None,
                    false,
                )
            } else {
                let d = self.details(&name, &["OrderDetails"]).await?;
                let order_id = field_str(&d, "orderid")
                    .or_else(|| field_str(&d, "entityid"))
                    .ok_or_else(|| {
                        IntegrationError::Transport("resellerclub: details without orderid".into())
                    })?;
                (
                    order_id,
                    request.current_expiry.or_else(|| epoch(&d, "endtime")),
                    field_str(&d, "currentstatus"),
                    field_bool(&d, "recurring").unwrap_or(false),
                )
            };
        if status.as_deref() == Some("Pending Delete Restorable") {
            if !request.restore_if_needed {
                return Err(IntegrationError::Conflict(format!(
                    "{name} is in redemption; a restore is required"
                )));
            }
            let v = self
                .post(
                    "domains/restore.json",
                    Params::new()
                        .add("order-id", order_id)
                        .add("invoice-option", "NoInvoice"),
                    "domains.restore",
                )
                .await?;
            let mut op = operation_from(&name, &v, false, None);
            op.message = Some(format!(
                "restored from redemption{}",
                op.message.map(|m| format!(": {m}")).unwrap_or_default()
            ));
            return Ok(op);
        }
        let expiry = expiry.ok_or_else(|| {
            IntegrationError::Transport("resellerclub: unknown expiry date".into())
        })?;
        let v = self
            .post(
                "domains/renew.json",
                Params::new()
                    .add("order-id", order_id)
                    .add("years", request.years.to_string())
                    .add("exp-date", expiry.timestamp().to_string())
                    .add("invoice-option", "NoInvoice")
                    .add("auto-renew", auto_renew.to_string()),
                "domains.renew",
            )
            .await?;
        let new_expiry = expiry.checked_add_months(Months::new(request.years.saturating_mul(12)));
        Ok(operation_from(&name, &v, false, new_expiry))
    }

    /// `domains/details-by-name.json?options=All` (KB 1755).
    async fn info(&self, domain: &str) -> Result<DomainInfo, IntegrationError> {
        let name = common::domain(domain)?;
        let d = self.details(&name, &["All"]).await?;
        let order_status = string_list(&d, "orderstatus");
        let domain_status = string_list(&d, "domainstatus");
        let mut epp: Vec<String> = Vec::new();
        if order_status.iter().any(|s| s == "transferlock") {
            epp.push("clientTransferProhibited".into());
        }
        let contacts = match (
            d.get("registrantcontact").and_then(contact_from_details),
            d.get("admincontact").and_then(contact_from_details),
            d.get("techcontact").and_then(contact_from_details),
            d.get("billingcontact").and_then(contact_from_details),
        ) {
            (Some(registrant), admin, tech, billing) => Some(DomainContacts {
                admin: admin.unwrap_or_else(|| registrant.clone()),
                tech: tech.unwrap_or_else(|| registrant.clone()),
                billing: billing.unwrap_or_else(|| registrant.clone()),
                registrant,
            }),
            _ => None,
        };
        Ok(DomainInfo {
            domain: name.as_str().to_owned(),
            status: status_from_details(&d, Utc::now()),
            epp_statuses: epp,
            registrar_ref: field_str(&d, "orderid").or_else(|| field_str(&d, "entityid")),
            created_at: epoch(&d, "creationtime"),
            expires_at: epoch(&d, "endtime"),
            nameservers: nameservers_from_details(&d),
            locked: Some(order_status.iter().any(|s| s == "transferlock")),
            privacy: field_bool(&d, "isprivacyprotected"),
            auto_renew: field_bool(&d, "recurring"),
            contacts,
            extra: json!({
                "currentstatus": field_str(&d, "currentstatus"),
                "orderstatus": order_status,
                "domainstatus": domain_status,
                "customerid": field_str(&d, "customerid"),
                "productkey": field_str(&d, "productkey"),
                "privacyprotectedallowed": field_bool(&d, "privacyprotectedallowed"),
                "raaVerificationStatus": field_str(&d, "raaVerificationStatus"),
            }),
        })
    }

    async fn get_nameservers(&self, domain: &str) -> Result<Vec<String>, IntegrationError> {
        let name = common::domain(domain)?;
        let d = self.details(&name, &["NsDetails"]).await?;
        Ok(nameservers_from_details(&d))
    }

    /// `domains/modify-ns.json` (KB 776).
    async fn set_nameservers(
        &self,
        domain: &str,
        nameservers: &[String],
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let ns = common::required_nameservers(nameservers)?;
        let order_id = self.order_id(&name).await?;
        self.post(
            "domains/modify-ns.json",
            Params::new().add("order-id", order_id).add_all("ns", ns),
            "domains.modify_ns",
        )
        .await?;
        Ok(())
    }

    /// `domains/enable-theft-protection.json` / `disable-…` (KB 902/903).
    async fn set_lock(&self, domain: &str, locked: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let order_id = self.order_id(&name).await?;
        let (path, op) = if locked {
            (
                "domains/enable-theft-protection.json",
                "domains.enable_theft_protection",
            )
        } else {
            (
                "domains/disable-theft-protection.json",
                "domains.disable_theft_protection",
            )
        };
        self.post(path, Params::new().add("order-id", order_id), op)
            .await?;
        Ok(())
    }

    /// `domsecret` from the order details (KB 1755).
    async fn get_auth_code(&self, domain: &str) -> Result<SecretString, IntegrationError> {
        let name = common::domain(domain)?;
        let d = self.details(&name, &["OrderDetails"]).await?;
        field_str(&d, "domsecret")
            .map(SecretString::from)
            .ok_or_else(|| IntegrationError::NotFound(format!("no auth code available for {name}")))
    }

    /// `domains/modify-privacy-protection.json` (KB 778); when enabling a
    /// domain without a privacy subscription, `domains/purchase-privacy.json`
    /// (KB 2085) is called first and the change retried once.
    async fn set_privacy(&self, domain: &str, enabled: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        if enabled && !Self::privacy_supported(name.tld()) {
            return Err(IntegrationError::InvalidInput(format!(
                "ResellerClub does not offer privacy protection for .{}",
                name.tld()
            )));
        }
        let order_id = self.order_id(&name).await?;
        let params = Params::new()
            .add("order-id", order_id.clone())
            .add("protect-privacy", enabled.to_string())
            .add("reason", "Requested by the domain owner via HostingOS");
        match self
            .post(
                "domains/modify-privacy-protection.json",
                params.clone(),
                "domains.modify_privacy",
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if enabled && e.to_string().to_ascii_lowercase().contains("purchase") => {
                self.post(
                    "domains/purchase-privacy.json",
                    Params::new()
                        .add("order-id", order_id)
                        .add("invoice-option", "NoInvoice"),
                    "domains.purchase_privacy",
                )
                .await?;
                self.post(
                    "domains/modify-privacy-protection.json",
                    params,
                    "domains.modify_privacy",
                )
                .await?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Contacts under the order's customer, then `domains/modify-contact.json`
    /// (KB 777). `contacts.registrant.extra.sixty_day_lock_optout` (bool) is
    /// passed through when present.
    async fn update_contacts(
        &self,
        domain: &str,
        contacts: &DomainContacts,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        common::validate_contact("registrant", &contacts.registrant)?;
        let d = self.details(&name, &["OrderDetails"]).await?;
        let order_id = field_str(&d, "orderid")
            .or_else(|| field_str(&d, "entityid"))
            .ok_or_else(|| {
                IntegrationError::Transport("resellerclub: details without orderid".into())
            })?;
        let customer_id = match field_str(&d, "customerid") {
            Some(c) => c,
            None => self.customer_for(&contacts.registrant).await?,
        };
        let [reg, admin, tech, billing] = self
            .contact_ids(&customer_id, &name, contacts, &Value::Null)
            .await?;
        let mut params = Params::new()
            .add("order-id", order_id)
            .add("reg-contact-id", reg)
            .add("admin-contact-id", admin)
            .add("tech-contact-id", tech)
            .add("billing-contact-id", billing);
        if let Some(opt_out) =
            common::extra_bool(&contacts.registrant.extra, "sixty_day_lock_optout")
        {
            params = params.add("sixty-day-lock-optout", opt_out.to_string());
        }
        self.post(
            "domains/modify-contact.json",
            params,
            "domains.modify_contact",
        )
        .await?;
        Ok(())
    }

    /// `dns/manage/search-records.json` for every supported type (KB 1106).
    async fn list_dns_records(&self, domain: &str) -> Result<Vec<DnsRecord>, IntegrationError> {
        let name = common::domain(domain)?;
        let mut out = Vec::new();
        for t in DNS_TYPES {
            out.extend(self.search_records(&name, *t).await?);
        }
        Ok(out)
    }

    /// `dns/manage/add-*-record.json` / `update-*-record.json`. Ids are
    /// `TYPE|host|value`. DNS is activated for the order on first use.
    async fn upsert_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<DnsRecord, IntegrationError> {
        let name = common::domain(domain)?;
        let slug = dns_slug(record.record_type)?;
        if record.value.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "record value is required".into(),
            ));
        }
        let mut params = Params::new().add("domain-name", name.as_str());
        let (target, srv) = if record.record_type == DnsRecordType::Srv {
            let (w, p, t) = split_srv(&record.value)?;
            (t, Some((w, p)))
        } else {
            (record.value.trim().to_owned(), None)
        };
        let host = host_param(&record.name);
        let existing = record
            .id
            .as_deref()
            .map(|id| {
                parse_record_id(id).ok_or_else(|| {
                    IntegrationError::InvalidInput(format!("invalid ResellerClub record id {id:?}"))
                })
            })
            .transpose()?;
        let (path, op) = match &existing {
            Some((old_type, old_host, old_value)) if *old_type == record.record_type => {
                params = params
                    .add("current-value", old_value.clone())
                    .add("new-value", target.clone());
                let h = if old_host.is_empty() {
                    host.clone()
                } else {
                    Some(old_host.clone())
                };
                if let Some(h) = h {
                    params = params.add("host", h);
                }
                (
                    format!("dns/manage/update-{slug}-record.json"),
                    "dns.update",
                )
            }
            Some((old_type, old_host, old_value)) => {
                // Type changed: delete the old record, then add the new one.
                let old = DnsRecord {
                    id: None,
                    name: if old_host.is_empty() {
                        "@".into()
                    } else {
                        old_host.clone()
                    },
                    record_type: *old_type,
                    value: old_value.clone(),
                    ttl: None,
                    priority: None,
                };
                self.delete_dns_record(domain, &old).await?;
                params = params.add("value", target.clone());
                if let Some(h) = host.clone() {
                    params = params.add("host", h);
                }
                (format!("dns/manage/add-{slug}-record.json"), "dns.add")
            }
            None => {
                params = params.add("value", target.clone());
                if let Some(h) = host.clone() {
                    params = params.add("host", h);
                }
                (format!("dns/manage/add-{slug}-record.json"), "dns.add")
            }
        };
        if let Some(ttl) = record.ttl {
            params = params.add("ttl", ttl.to_string());
        }
        if matches!(record.record_type, DnsRecordType::Mx | DnsRecordType::Srv) {
            params = params.add("priority", record.priority.unwrap_or(10).to_string());
        }
        if let Some((w, p)) = &srv {
            params = params.add("port", p.clone()).add("weight", w.clone());
        }
        self.dns_mutation(&name, path, params, op).await?;
        let stored_host = match &existing {
            Some((t, h, _)) if *t == record.record_type && !h.is_empty() => h.clone(),
            _ => host.unwrap_or_default(),
        };
        Ok(DnsRecord {
            id: Some(record_id(record.record_type, &stored_host, &target)),
            name: relative_name(&name, &stored_host),
            ..record.clone()
        })
    }

    /// `dns/manage/delete-*-record.json` (KB 1171–1180).
    async fn delete_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let (rtype, host, value) = match record.id.as_deref().and_then(parse_record_id) {
            Some((t, h, v)) => (t, host_param(&h), v),
            None => {
                let v = if record.record_type == DnsRecordType::Srv {
                    split_srv(&record.value)?.2
                } else {
                    record.value.trim().to_owned()
                };
                (record.record_type, host_param(&record.name), v)
            }
        };
        let slug = dns_slug(rtype)?;
        let mut params = Params::new()
            .add("domain-name", name.as_str())
            .add("value", value);
        if let Some(h) = host {
            params = params.add("host", h);
        }
        if rtype == DnsRecordType::Srv {
            let (w, p, _) = split_srv(&record.value)
                .unwrap_or_else(|_| ("0".into(), "0".into(), String::new()));
            params = params.add("port", p).add("weight", w);
        }
        self.dns_mutation(
            &name,
            format!("dns/manage/delete-{slug}-record.json"),
            params,
            "dns.delete",
        )
        .await
    }
}

fn string_list(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(scalar).collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn availability_from(
    name: &DomainName,
    entry: Option<&Value>,
    currency: Option<Currency>,
) -> DomainAvailability {
    let mut out = DomainAvailability {
        domain: name.as_str().to_owned(),
        available: false,
        premium: false,
        register_price: None,
        renew_price: None,
        reason: None,
    };
    let Some(entry) = entry else {
        out.reason = Some("not returned by ResellerClub".into());
        return out;
    };
    let status = field_str(entry, "status")
        .unwrap_or_default()
        .to_ascii_lowercase();
    match status.as_str() {
        "available" => out.available = true,
        "regthroughus" => out.reason = Some("registered (through this reseller account)".into()),
        "regthroughothers" => out.reason = Some("registered".into()),
        "unknown" => out.reason = Some("unknown (registry did not answer)".into()),
        other => out.reason = Some(format!("status {other}")),
    }
    if let Some(hash) = entry.get("costHash").filter(|h| h.is_object()) {
        out.premium = true;
        let cur = hash
            .as_object()
            .and_then(|o| {
                o.iter()
                    .find(|(k, _)| k.to_ascii_lowercase().contains("currency"))
            })
            .and_then(|(_, v)| v.as_str().map(str::to_owned))
            .or_else(|| currency.map(|c| c.as_str().to_owned()));
        if let Some(cur) = cur {
            out.register_price = common::money(hash.get("create").and_then(common::decimal), &cur);
            out.renew_price = common::money(hash.get("renew").and_then(common::decimal), &cur);
        }
    }
    out
}

/// `{"domorder":[{"domcno":["com"]},...]}` → product key → TLDs.
fn product_key_map(v: &Value) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Some(list) = v.get("domorder").and_then(Value::as_array) else {
        return out;
    };
    for item in list {
        if let Some(obj) = item.as_object() {
            for (key, tlds) in obj {
                let tlds: Vec<String> = tlds
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(|t| {
                                name::normalize_tld(t).unwrap_or_else(|_| {
                                    t.trim_start_matches('.').to_ascii_lowercase()
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.entry(key.clone()).or_default().extend(tlds);
            }
        }
    }
    out
}

fn tld_price_from(tld: &str, product: &Value, currency: Currency) -> Option<TldPrice> {
    let action = |k: &str| -> Option<(Option<Money>, Vec<u32>)> {
        let obj = product.get(k)?.as_object()?;
        let years: Vec<u32> = obj.keys().filter_map(|y| y.parse().ok()).collect();
        let one = obj
            .get("1")
            .or_else(|| years.iter().min().and_then(|y| obj.get(&y.to_string())))
            .and_then(common::decimal)
            .map(|d| Money::new(d, currency));
        Some((one, years))
    };
    let (register, years) = action("addnewdomain")?;
    let rule = tld_rules::rule_for(tld);
    Some(TldPrice {
        tld: tld.to_owned(),
        register,
        renew: action("renewdomain").and_then(|a| a.0),
        transfer: action("addtransferdomain").and_then(|a| a.0),
        restore: action("restoredomain").and_then(|a| a.0),
        min_years: years.iter().min().copied().unwrap_or(rule.min_years),
        max_years: years.iter().max().copied().unwrap_or(rule.max_years),
    })
}

fn operation_from(
    name: &DomainName,
    v: &Value,
    transfer: bool,
    expires: Option<DateTime<Utc>>,
) -> DomainOperation {
    let action_status = field_str(v, "actionstatus").unwrap_or_default();
    let completed = !transfer && action_status.eq_ignore_ascii_case("success");
    let cost = match (
        field_str(v, "sellingamount").and_then(|s| common::parse_decimal(&s)),
        field_str(v, "sellingcurrencysymbol"),
    ) {
        (Some(a), Some(c)) => common::money(Some(a.abs()), &c),
        _ => None,
    };
    DomainOperation {
        domain: name.as_str().to_owned(),
        status: if completed {
            DomainOperationStatus::Completed
        } else {
            DomainOperationStatus::Pending
        },
        registrar_ref: field_str(v, "entityid"),
        expires_at: if completed { expires } else { None },
        cost,
        message: field_str(v, "actionstatusdesc"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_detection_and_classification() {
        assert!(error_message(&json!({"status": "ERROR", "message": "x"})).is_some());
        assert!(error_message(&json!({"status": "error", "error": "y"})).is_some());
        assert!(
            error_message(&json!({"actionstatus": "Failed", "actionstatusdesc": "z"})).is_some()
        );
        assert!(error_message(&json!({"status": "Success"})).is_none());
        assert!(error_message(&json!(123)).is_none());
        assert_eq!(
            classify(
                500,
                "Access Denied. You are not allowed to perform this action"
            )
            .kind(),
            "auth"
        );
        assert_eq!(
            classify(500, "Customer x@y.com not found").kind(),
            "not_found"
        );
        assert_eq!(
            classify(500, "{telnocc=TelephoneNo Country Code is invalid}").kind(),
            "invalid_input"
        );
        let biz = classify(500, "Something odd happened");
        assert!(!biz.is_retryable());
    }

    #[test]
    fn role_usage_and_types() {
        let uk = RoleUsage::for_tld("co.uk");
        assert!(!uk.admin && !uk.tech && !uk.billing);
        let ca = RoleUsage::for_tld("ca");
        assert!(ca.admin && ca.tech && !ca.billing);
        assert_eq!(contact_type("co.uk"), "UkContact");
        assert_eq!(contact_type("com"), "Contact");
        assert!(!ResellerClub::privacy_supported("us"));
        assert!(!ResellerClub::privacy_supported("co.uk"));
        assert!(ResellerClub::privacy_supported("com"));
    }

    #[test]
    fn password_rules() {
        for _ in 0..20 {
            let p = generate_password();
            let s = p.expose_str();
            assert_eq!(s.len(), 15);
            assert!(s.chars().any(|c| c.is_ascii_lowercase()));
            assert!(s.chars().any(|c| c.is_ascii_uppercase()));
            assert!(s.chars().any(|c| c.is_ascii_digit()));
            assert!(s.chars().any(|c| "!@#$%*".contains(c)));
        }
    }

    #[test]
    fn details_parsing() {
        let name = DomainName::parse("example.com").unwrap();
        let d = json!({
            "orderid": "562994", "currentstatus": "Active", "endtime": "1924992000",
            "ns2": "NS2.EXAMPLE.NET", "ns1": "ns1.example.net", "noOfNameServers": "2",
        });
        assert_eq!(
            nameservers_from_details(&d),
            vec!["ns1.example.net", "ns2.example.net"]
        );
        assert_eq!(status_from_details(&d, Utc::now()), DomainStatus::Active);
        let rec = json!({"host": "www.example.com", "value": "192.0.2.1", "timetolive": "7200"});
        let r = record_from_rc(&name, DnsRecordType::A, &rec).unwrap();
        assert_eq!(r.name, "www");
        assert_eq!(
            parse_record_id(r.id.as_deref().unwrap()).unwrap().2,
            "192.0.2.1"
        );
        assert_eq!(relative_name(&name, "example.com"), "@");
    }
}
