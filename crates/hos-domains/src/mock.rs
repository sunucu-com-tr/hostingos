//! In-memory, deterministic registrar for demos and end-to-end tests.
//!
//! Behaviour is driven by the second-level label:
//!
//! | SLD | behaviour |
//! |---|---|
//! | `taken-*` | registered elsewhere: unavailable, transferable with any auth code except `invalid` |
//! | `reserved-*` | unavailable (reserved by the registry), cannot be transferred |
//! | `premium-*` | available as premium (10× the base price); registration needs `premium_price` |
//! | `pending-*` | register/transfer return `pending`; the first `info` reports pending, later calls active |
//! | `fail-*` | every mutating call fails with a provider error |
//! | anything else | available if the TLD is in the price list |
//!
//! Domains registered/transferred through the mock live in memory for the
//! lifetime of the instance; DNS records get ids `mock-1`, `mock-2`, ...
//! Auth codes are a stable hash of the name. All prices are in the configured
//! currency (default USD).

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Months, Utc};
use hos_core::IntegrationError;
use hos_core::domain::DomainStatus;
use hos_core::integrations::registrar::{
    DnsRecord, DomainAvailability, DomainContacts, DomainInfo, DomainOperation,
    DomainOperationStatus, DomainRegistrar, DomainRegistration, DomainRenewal, DomainTransfer,
    TldPrice,
};
use hos_core::integrations::{ConnectionInfo, FieldReader};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::common;
use crate::name::DomainName;
use crate::tld_rules;

/// `integrations.kind` of the mock registrar.
pub const KIND: &str = "mock";

/// Base prices in hundredths: (tld, register, renew, transfer, restore).
const PRICE_LIST: &[(&str, i64, i64, i64, i64)] = &[
    ("com", 920, 920, 920, 8000),
    ("net", 1110, 1110, 1110, 8000),
    ("org", 1050, 1050, 1050, 8000),
    ("info", 350, 2000, 2000, 8000),
    ("xyz", 199, 1200, 1200, 8000),
    ("io", 3500, 3500, 3500, 9000),
    ("co", 2500, 2500, 2500, 9000),
    ("dev", 1300, 1300, 1300, 8000),
    ("app", 1500, 1500, 1500, 8000),
    ("tr", 1200, 1200, 0, 0),
    ("com.tr", 900, 900, 0, 0),
    ("net.tr", 900, 900, 0, 0),
    ("org.tr", 900, 900, 0, 0),
    ("uk", 600, 600, 0, 0),
    ("co.uk", 600, 600, 0, 0),
    ("eu", 700, 700, 700, 4000),
    ("us", 750, 750, 750, 8000),
    ("de", 600, 600, 0, 4000),
];

#[derive(Debug, Clone)]
struct MockDomain {
    status: DomainStatus,
    pending_polls: u32,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    nameservers: Vec<String>,
    locked: bool,
    privacy: bool,
    auto_renew: bool,
    contacts: DomainContacts,
    order_id: String,
    dns: Vec<DnsRecord>,
}

#[derive(Debug, Default)]
struct State {
    domains: BTreeMap<String, MockDomain>,
    next_id: u64,
}

/// See the [module docs](self).
#[derive(Debug)]
pub struct MockRegistrar {
    currency: Currency,
    fixed_now: Option<DateTime<Utc>>,
    default_nameservers: Vec<String>,
    state: Mutex<State>,
}

impl Default for MockRegistrar {
    fn default() -> Self {
        Self::new(Currency::USD)
    }
}

impl MockRegistrar {
    /// Mock quoting prices in `currency`.
    pub fn new(currency: Currency) -> Self {
        Self {
            currency,
            fixed_now: None,
            default_nameservers: vec!["ns1.mock-dns.test".into(), "ns2.mock-dns.test".into()],
            state: Mutex::new(State::default()),
        }
    }

    /// Builds from integration config (`currency`, `default_nameservers`).
    pub fn from_config(config: &Value, _secrets: &Value) -> Result<Self, IntegrationError> {
        let r = FieldReader::config(config);
        let currency = Currency::new(&r.string_or("currency", "USD")?).map_err(|_| {
            IntegrationError::Config("config.currency must be an ISO 4217 code".into())
        })?;
        let mut mock = Self::new(currency);
        let ns = common::nameservers(&common::split_list(r.opt_str("default_nameservers")?))
            .map_err(|e| IntegrationError::Config(format!("config.default_nameservers: {e}")))?;
        if !ns.is_empty() {
            mock.default_nameservers = ns;
        }
        Ok(mock)
    }

    /// Freezes the clock (deterministic dates in tests).
    #[must_use]
    pub fn with_now(mut self, now: DateTime<Utc>) -> Self {
        self.fixed_now = Some(now);
        self
    }

    fn now(&self) -> DateTime<Utc> {
        self.fixed_now.unwrap_or_else(Utc::now)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock only means another test thread panicked; the data is
        // still consistent for a mock.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn price(&self, tld: &str) -> Option<(Money, Money, Option<Money>, Option<Money>)> {
        PRICE_LIST
            .iter()
            .find(|p| p.0 == tld)
            .map(|&(_, reg, ren, tr, rs)| {
                let m = |c: i64| Money::new(Decimal::new(c, 2), self.currency);
                (
                    m(reg),
                    m(ren),
                    (tr > 0).then(|| m(tr)),
                    (rs > 0).then(|| m(rs)),
                )
            })
    }

    /// Stable fake auth code for a domain.
    pub fn auth_code_for(domain: &str) -> String {
        format!("MOCK-{:010X}", common::fnv1a(domain) & 0xFF_FFFF_FFFF)
    }

    fn fail_if_requested(name: &DomainName, op: &str) -> Result<(), IntegrationError> {
        if name.sld().starts_with("fail-") {
            return Err(IntegrationError::remote_with_code(
                None,
                "MOCK_FAILURE",
                format!("simulated {op} failure for {name}"),
            ));
        }
        Ok(())
    }

    fn add_years(date: DateTime<Utc>, years: u32) -> Result<DateTime<Utc>, IntegrationError> {
        date.checked_add_months(Months::new(years.saturating_mul(12)))
            .ok_or_else(|| IntegrationError::InvalidInput("period out of range".into()))
    }

    fn next_id(state: &mut State, prefix: &str) -> String {
        state.next_id += 1;
        format!("{prefix}{}", state.next_id)
    }
}

#[async_trait]
impl DomainRegistrar for MockRegistrar {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let n = self.lock().domains.len();
        Ok(ConnectionInfo {
            server_version: Some("mock-registrar/1".into()),
            details: Some(format!(
                "{n} domains in memory, prices in {}",
                self.currency
            )),
        })
    }

    async fn check_availability(
        &self,
        domains: &[String],
    ) -> Result<Vec<DomainAvailability>, IntegrationError> {
        let state = self.lock();
        Ok(domains
            .iter()
            .map(|input| {
                let mut out = DomainAvailability {
                    domain: input.trim().to_ascii_lowercase(),
                    available: false,
                    premium: false,
                    register_price: None,
                    renew_price: None,
                    reason: None,
                };
                let name = match DomainName::parse(input) {
                    Ok(n) => n,
                    Err(e) => {
                        out.reason = Some(e.to_string());
                        return out;
                    }
                };
                out.domain = name.as_str().to_owned();
                let Some((reg, ren, _, _)) = self.price(name.tld()) else {
                    out.reason = Some(format!(".{} is not offered", name.tld()));
                    return out;
                };
                let sld = name.sld();
                if state.domains.contains_key(name.as_str()) || sld.starts_with("taken-") {
                    out.reason = Some("registered".into());
                } else if sld.starts_with("reserved-") {
                    out.reason = Some("reserved by the registry".into());
                } else if sld.starts_with("premium-") {
                    out.available = true;
                    out.premium = true;
                    out.register_price = reg.try_mul(Decimal::TEN).ok();
                    out.renew_price = Some(ren);
                } else {
                    out.available = true;
                }
                out
            })
            .collect())
    }

    async fn tld_pricing(&self, tlds: &[String]) -> Result<Vec<TldPrice>, IntegrationError> {
        let wanted: Vec<String> = tlds
            .iter()
            .map(|t| t.trim().trim_start_matches('.').to_ascii_lowercase())
            .collect();
        Ok(PRICE_LIST
            .iter()
            .filter(|p| wanted.is_empty() || wanted.iter().any(|w| w == p.0))
            .filter_map(|p| {
                let (reg, ren, tr, rs) = self.price(p.0)?;
                let rule = tld_rules::rule_for(p.0);
                Some(TldPrice {
                    tld: p.0.to_owned(),
                    register: Some(reg),
                    renew: Some(ren),
                    transfer: tr,
                    restore: rs,
                    min_years: rule.min_years,
                    max_years: rule.max_years,
                })
            })
            .collect())
    }

    async fn register(
        &self,
        request: &DomainRegistration,
    ) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        tld_rules::rule_for(name.tld()).validate_years(request.years)?;
        for (role, c) in [
            ("registrant", &request.contacts.registrant),
            ("admin", &request.contacts.admin),
            ("tech", &request.contacts.tech),
            ("billing", &request.contacts.billing),
        ] {
            common::validate_contact(role, c)?;
        }
        let ns = common::nameservers(&request.nameservers)?;
        Self::fail_if_requested(&name, "registration")?;
        let availability = self
            .check_availability(std::slice::from_ref(&request.domain))
            .await?
            .remove(0);
        if !availability.available {
            return Err(IntegrationError::Conflict(format!(
                "{name} is not available: {}",
                availability.reason.unwrap_or_default()
            )));
        }
        let unit = availability
            .register_price
            .or_else(|| self.price(name.tld()).map(|p| p.0))
            .ok_or_else(|| {
                IntegrationError::InvalidInput(format!(".{} is not offered", name.tld()))
            })?;
        if availability.premium {
            let accepted = request.premium_price.ok_or_else(|| {
                IntegrationError::InvalidInput(format!(
                    "{name} is a premium name; premium_price must be accepted"
                ))
            })?;
            if accepted
                .try_cmp(&unit)
                .map_err(|e| IntegrationError::InvalidInput(e.to_string()))?
                == std::cmp::Ordering::Less
            {
                return Err(IntegrationError::InvalidInput(format!(
                    "accepted premium price {accepted} is below the registry price {unit}"
                )));
            }
        }
        let now = self.now();
        let pending = name.sld().starts_with("pending-");
        let cost = unit
            .try_mul(Decimal::from(request.years))
            .map_err(|e| IntegrationError::Other(e.to_string()))?;
        let expires_at = Self::add_years(now, request.years)?;
        let mut state = self.lock();
        let order_id = Self::next_id(&mut state, "order-");
        state.domains.insert(
            name.as_str().to_owned(),
            MockDomain {
                status: if pending {
                    DomainStatus::PendingRegistration
                } else {
                    DomainStatus::Active
                },
                pending_polls: u32::from(pending),
                created_at: now,
                expires_at,
                nameservers: if ns.is_empty() {
                    self.default_nameservers.clone()
                } else {
                    ns
                },
                locked: true,
                privacy: request.privacy && tld_rules::rule_for(name.tld()).privacy_allowed,
                auto_renew: request.auto_renew,
                contacts: request.contacts.clone(),
                order_id: order_id.clone(),
                dns: Vec::new(),
            },
        );
        Ok(DomainOperation {
            domain: name.into_string(),
            status: if pending {
                DomainOperationStatus::Pending
            } else {
                DomainOperationStatus::Completed
            },
            registrar_ref: Some(order_id),
            expires_at: (!pending).then_some(expires_at),
            cost: Some(cost),
            message: Some("registered by mock registrar".into()),
        })
    }

    async fn transfer(
        &self,
        request: &DomainTransfer,
    ) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        let ns = common::nameservers(&request.nameservers)?;
        if request.auth_code.expose_str().trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "auth code is required".into(),
            ));
        }
        Self::fail_if_requested(&name, "transfer")?;
        let price = self.price(name.tld()).ok_or_else(|| {
            IntegrationError::InvalidInput(format!(".{} is not offered", name.tld()))
        })?;
        if self.lock().domains.contains_key(name.as_str()) {
            return Err(IntegrationError::Conflict(format!(
                "{name} is already in this account"
            )));
        }
        let sld = name.sld();
        if !(sld.starts_with("taken-") || sld.starts_with("pending-")) {
            return Err(IntegrationError::InvalidInput(format!(
                "{name} is not registered and cannot be transferred"
            )));
        }
        if request.auth_code.expose_str() == "invalid" {
            return Err(IntegrationError::remote_with_code(
                None,
                "AUTH_CODE_INVALID",
                "the auth code is not valid for this domain",
            ));
        }
        let now = self.now();
        let mut state = self.lock();
        let order_id = Self::next_id(&mut state, "order-");
        state.domains.insert(
            name.as_str().to_owned(),
            MockDomain {
                status: DomainStatus::PendingTransfer,
                pending_polls: 1,
                created_at: now,
                expires_at: Self::add_years(now, request.years.max(1))?,
                nameservers: if ns.is_empty() {
                    self.default_nameservers.clone()
                } else {
                    ns
                },
                locked: false,
                privacy: request.privacy,
                auto_renew: request.auto_renew,
                contacts: request.contacts.clone(),
                order_id: order_id.clone(),
                dns: Vec::new(),
            },
        );
        Ok(DomainOperation {
            domain: name.into_string(),
            status: DomainOperationStatus::Pending,
            registrar_ref: Some(order_id),
            expires_at: None,
            cost: price.2,
            message: Some("transfer requested".into()),
        })
    }

    async fn renew(&self, request: &DomainRenewal) -> Result<DomainOperation, IntegrationError> {
        let name = common::domain(&request.domain)?;
        let rule = tld_rules::rule_for(name.tld());
        rule.validate_years(request.years)?;
        Self::fail_if_requested(&name, "renewal")?;
        let (_, renew_price, _, restore_price) = self.price(name.tld()).ok_or_else(|| {
            IntegrationError::InvalidInput(format!(".{} is not offered", name.tld()))
        })?;
        let now = self.now();
        let mut state = self.lock();
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        if let Some(expected) = request.current_expiry
            && expected.date_naive() != d.expires_at.date_naive()
        {
            return Err(IntegrationError::Conflict(format!(
                "expiry mismatch for {name}: registrar has {}",
                d.expires_at.date_naive()
            )));
        }
        let mut cost = renew_price
            .try_mul(Decimal::from(request.years))
            .map_err(|e| IntegrationError::Other(e.to_string()))?;
        if d.status == DomainStatus::Redemption {
            if !request.restore_if_needed {
                return Err(IntegrationError::Conflict(format!(
                    "{name} is in redemption; restore required"
                )));
            }
            if let Some(fee) = restore_price {
                cost = cost
                    .try_add(&fee)
                    .map_err(|e| IntegrationError::Other(e.to_string()))?;
            }
        }
        let new_expiry = Self::add_years(d.expires_at, request.years)?;
        let cap = Self::add_years(now, rule.max_years)?;
        if new_expiry > cap {
            return Err(IntegrationError::InvalidInput(format!(
                "renewal would exceed the maximum of {} years",
                rule.max_years
            )));
        }
        d.expires_at = new_expiry;
        d.status = DomainStatus::Active;
        Ok(DomainOperation {
            domain: name.into_string(),
            status: DomainOperationStatus::Completed,
            registrar_ref: Some(d.order_id.clone()),
            expires_at: Some(new_expiry),
            cost: Some(cost),
            message: None,
        })
    }

    async fn info(&self, domain: &str) -> Result<DomainInfo, IntegrationError> {
        let name = common::domain(domain)?;
        let now = self.now();
        let mut state = self.lock();
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        let status = if d.pending_polls > 0 {
            d.pending_polls -= 1;
            d.status
        } else {
            if matches!(
                d.status,
                DomainStatus::PendingRegistration | DomainStatus::PendingTransfer
            ) {
                d.status = DomainStatus::Active;
            }
            if d.status == DomainStatus::Active && d.expires_at < now {
                DomainStatus::Grace
            } else {
                d.status
            }
        };
        let mut epp = Vec::new();
        if d.locked {
            epp.push("clientTransferProhibited".to_owned());
        }
        Ok(DomainInfo {
            domain: name.into_string(),
            status,
            epp_statuses: epp,
            registrar_ref: Some(d.order_id.clone()),
            created_at: Some(d.created_at),
            expires_at: Some(d.expires_at),
            nameservers: d.nameservers.clone(),
            locked: Some(d.locked),
            privacy: Some(d.privacy),
            auto_renew: Some(d.auto_renew),
            contacts: Some(d.contacts.clone()),
            extra: json!({ "mock": true }),
        })
    }

    async fn get_nameservers(&self, domain: &str) -> Result<Vec<String>, IntegrationError> {
        let name = common::domain(domain)?;
        self.lock()
            .domains
            .get(name.as_str())
            .map(|d| d.nameservers.clone())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))
    }

    async fn set_nameservers(
        &self,
        domain: &str,
        nameservers: &[String],
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let ns = common::required_nameservers(nameservers)?;
        Self::fail_if_requested(&name, "nameserver update")?;
        let mut state = self.lock();
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        d.nameservers = ns;
        Ok(())
    }

    async fn set_lock(&self, domain: &str, locked: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let mut state = self.lock();
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        d.locked = locked;
        Ok(())
    }

    async fn get_auth_code(&self, domain: &str) -> Result<SecretString, IntegrationError> {
        let name = common::domain(domain)?;
        if !self.lock().domains.contains_key(name.as_str()) {
            return Err(IntegrationError::NotFound(name.to_string()));
        }
        Ok(SecretString::from(Self::auth_code_for(name.as_str())))
    }

    async fn set_privacy(&self, domain: &str, enabled: bool) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        if enabled && !tld_rules::rule_for(name.tld()).privacy_allowed {
            return Err(IntegrationError::InvalidInput(format!(
                "WHOIS privacy is not available for .{}",
                name.tld()
            )));
        }
        let mut state = self.lock();
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        d.privacy = enabled;
        Ok(())
    }

    async fn update_contacts(
        &self,
        domain: &str,
        contacts: &DomainContacts,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        common::validate_contact("registrant", &contacts.registrant)?;
        let mut state = self.lock();
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        d.contacts = contacts.clone();
        Ok(())
    }

    async fn list_dns_records(&self, domain: &str) -> Result<Vec<DnsRecord>, IntegrationError> {
        let name = common::domain(domain)?;
        self.lock()
            .domains
            .get(name.as_str())
            .map(|d| d.dns.clone())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))
    }

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
        let mut state = self.lock();
        let new_id = Self::next_id(&mut state, "mock-");
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        let mut stored = record.clone();
        match &record.id {
            Some(id) => {
                let existing = d
                    .dns
                    .iter_mut()
                    .find(|r| r.id.as_deref() == Some(id))
                    .ok_or_else(|| IntegrationError::NotFound(format!("DNS record {id}")))?;
                *existing = stored.clone();
            }
            None => {
                stored.id = Some(new_id);
                d.dns.push(stored.clone());
            }
        }
        Ok(stored)
    }

    async fn delete_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<(), IntegrationError> {
        let name = common::domain(domain)?;
        let mut state = self.lock();
        let d = state
            .domains
            .get_mut(name.as_str())
            .ok_or_else(|| IntegrationError::NotFound(name.to_string()))?;
        let before = d.dns.len();
        d.dns.retain(|r| match &record.id {
            Some(id) => r.id.as_deref() != Some(id),
            None => {
                !(r.name == record.name
                    && r.record_type == record.record_type
                    && r.value == record.value)
            }
        });
        if d.dns.len() == before {
            return Err(IntegrationError::NotFound("DNS record".into()));
        }
        Ok(())
    }
}
