//! Domain registrars: ResellerClub (HTTP API), Namecheap (XML API), OpenProvider (REST v1beta).
//!
//! Domains are addressed by their fully-qualified, lower-case, IDNA/punycode
//! name (`example.com.tr`). Registrar-internal ids (ResellerClub `order-id`,
//! OpenProvider domain id) are resolved inside the implementation; they may be
//! returned in [`DomainOperation::registrar_ref`] / [`DomainInfo::registrar_ref`]
//! for display and support purposes.
//!
//! Prices returned here are **cost prices** at the registrar, in the
//! registrar account's currency; selling prices live in `tld_pricing`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ConnectionInfo, PhoneNumber, PostalAddress};
use crate::domain::DomainStatus;
use crate::error::IntegrationError;
use crate::money::Money;
use crate::secret::SecretString;

/// A domain contact (registrant / admin / tech / billing).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactInfo {
    /// Given name.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Organisation (empty/`None` for individuals).
    pub company: Option<String>,
    /// Email.
    pub email: String,
    /// Voice phone.
    pub phone: PhoneNumber,
    /// Fax.
    pub fax: Option<PhoneNumber>,
    /// Postal address.
    pub address: PostalAddress,
    /// Tax / national id (TR: TCKN or VKN; required for some ccTLDs).
    pub tax_id: Option<String>,
    /// Registrar/TLD-specific fields (`.us` nexus, `.uk` legal type...).
    pub extra: Value,
}

impl ContactInfo {
    /// `"First Last"`.
    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
            .trim()
            .to_owned()
    }
}

/// The four contact roles. Most registrars require all four.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainContacts {
    /// Registrant (legal owner).
    pub registrant: ContactInfo,
    /// Administrative contact.
    pub admin: ContactInfo,
    /// Technical contact.
    pub tech: ContactInfo,
    /// Billing contact.
    pub billing: ContactInfo,
}

impl DomainContacts {
    /// Uses the same contact for all four roles.
    pub fn same(contact: ContactInfo) -> Self {
        Self {
            registrant: contact.clone(),
            admin: contact.clone(),
            tech: contact.clone(),
            billing: contact,
        }
    }
}

/// Availability of one domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainAvailability {
    /// Domain checked.
    pub domain: String,
    /// Can be registered now.
    pub available: bool,
    /// Premium / registry-priced name.
    #[serde(default)]
    pub premium: bool,
    /// Registration cost when premium (or when the registrar quotes per-domain).
    pub register_price: Option<Money>,
    /// Renewal cost when premium.
    pub renew_price: Option<Money>,
    /// Reason when unavailable / unknown (reserved, unsupported TLD...).
    pub reason: Option<String>,
}

/// Registrar cost prices for one TLD. Prices are per year.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TldPrice {
    /// TLD without leading dot (`com`, `com.tr`).
    pub tld: String,
    /// Registration price per year.
    pub register: Option<Money>,
    /// Renewal price per year.
    pub renew: Option<Money>,
    /// Transfer price (usually includes one year).
    pub transfer: Option<Money>,
    /// Redemption restore fee.
    pub restore: Option<Money>,
    /// Minimum registration period in years.
    pub min_years: u32,
    /// Maximum registration period in years.
    pub max_years: u32,
}

/// Registration request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DomainRegistration {
    /// Domain name.
    pub domain: String,
    /// Period in years.
    pub years: u32,
    /// Nameservers (2–13; empty = registrar defaults).
    pub nameservers: Vec<String>,
    /// Contacts.
    pub contacts: DomainContacts,
    /// Enable WHOIS privacy / ID protection.
    pub privacy: bool,
    /// Enable auto-renew **at the registrar** (HostingOS normally renews itself → `false`).
    pub auto_renew: bool,
    /// Accepted premium price; required to register a premium name.
    pub premium_price: Option<Money>,
    /// TLD-specific extension data.
    pub extra: Value,
}

/// Incoming transfer request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DomainTransfer {
    /// Domain name.
    pub domain: String,
    /// EPP auth code.
    pub auth_code: SecretString,
    /// Years added by the transfer (usually 1).
    pub years: u32,
    /// Nameservers to set after transfer (empty = keep).
    pub nameservers: Vec<String>,
    /// Contacts.
    pub contacts: DomainContacts,
    /// Enable WHOIS privacy.
    pub privacy: bool,
    /// Enable auto-renew at the registrar.
    pub auto_renew: bool,
    /// TLD-specific extension data.
    pub extra: Value,
}

/// Renewal request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DomainRenewal {
    /// Domain name.
    pub domain: String,
    /// Years to add.
    pub years: u32,
    /// Current expiry as known to HostingOS (ResellerClub requires `exp-date`;
    /// implementations fetch it when `None`).
    pub current_expiry: Option<DateTime<Utc>>,
    /// Also restore from redemption if needed (fees apply).
    pub restore_if_needed: bool,
}

crate::string_enum! {
    /// Outcome of a register/transfer/renew call.
    pub enum DomainOperationStatus {
        /// Done.
        Completed => "completed",
        /// Accepted, completes later (transfers, some ccTLDs); poll with `info`.
        Pending => "pending",
    }
}

/// Result of register/transfer/renew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainOperation {
    /// Domain name.
    pub domain: String,
    /// Completed or pending.
    pub status: DomainOperationStatus,
    /// Registrar-side id (order id / domain id).
    pub registrar_ref: Option<String>,
    /// New expiry, when known.
    pub expires_at: Option<DateTime<Utc>>,
    /// Amount charged by the registrar, when reported.
    pub cost: Option<Money>,
    /// Provider message.
    pub message: Option<String>,
}

/// Registrar view of a domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainInfo {
    /// Domain name.
    pub domain: String,
    /// Normalised status.
    pub status: DomainStatus,
    /// Raw EPP statuses (`clientTransferProhibited`, ...).
    #[serde(default)]
    pub epp_statuses: Vec<String>,
    /// Registrar-side id.
    pub registrar_ref: Option<String>,
    /// Registration date.
    pub created_at: Option<DateTime<Utc>>,
    /// Expiry date.
    pub expires_at: Option<DateTime<Utc>>,
    /// Current nameservers.
    #[serde(default)]
    pub nameservers: Vec<String>,
    /// Transfer lock.
    pub locked: Option<bool>,
    /// WHOIS privacy.
    pub privacy: Option<bool>,
    /// Auto-renew at the registrar.
    pub auto_renew: Option<bool>,
    /// Contacts, when the registrar returns them cheaply.
    pub contacts: Option<DomainContacts>,
    /// Provider-specific data.
    #[serde(default)]
    pub extra: Value,
}

crate::string_enum! {
    /// DNS record type (stored upper-case as in zone files).
    pub enum DnsRecordType {
        /// IPv4 address.
        A => "A",
        /// IPv6 address.
        Aaaa => "AAAA",
        /// Canonical name.
        Cname => "CNAME",
        /// Mail exchanger.
        Mx => "MX",
        /// Text.
        Txt => "TXT",
        /// Nameserver.
        Ns => "NS",
        /// Service locator.
        Srv => "SRV",
        /// Certificate authority authorization.
        Caa => "CAA",
    }
}

/// A DNS record in registrar-hosted DNS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecord {
    /// Registrar record id (`None` when creating).
    pub id: Option<String>,
    /// Host relative to the zone (`@`, `www`, `_dmarc`).
    pub name: String,
    /// Type.
    pub record_type: DnsRecordType,
    /// Value (IP, target host, text; SRV: `weight port target`).
    pub value: String,
    /// TTL in seconds.
    pub ttl: Option<u32>,
    /// MX / SRV priority.
    pub priority: Option<u16>,
}

/// A domain registrar. See the [module docs](self).
#[async_trait]
pub trait DomainRegistrar: Send + Sync {
    /// The integration kind (e.g. [`kinds::NAMECHEAP`](super::kinds::NAMECHEAP)).
    fn kind(&self) -> &'static str;

    /// Verifies credentials (and IP whitelisting) by calling a cheap endpoint.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError>;

    /// Checks availability of several domains in as few calls as possible.
    /// Returns one entry per input domain, in input order.
    async fn check_availability(
        &self,
        domains: &[String],
    ) -> Result<Vec<DomainAvailability>, IntegrationError>;

    /// Cost prices; `tlds` empty = every TLD the registrar offers.
    async fn tld_pricing(&self, tlds: &[String]) -> Result<Vec<TldPrice>, IntegrationError>;

    /// Registers a domain.
    async fn register(
        &self,
        request: &DomainRegistration,
    ) -> Result<DomainOperation, IntegrationError>;

    /// Starts an incoming transfer.
    async fn transfer(&self, request: &DomainTransfer)
    -> Result<DomainOperation, IntegrationError>;

    /// Renews a domain.
    async fn renew(&self, request: &DomainRenewal) -> Result<DomainOperation, IntegrationError>;

    /// Current registrar state (used by the daily sync job).
    async fn info(&self, domain: &str) -> Result<DomainInfo, IntegrationError>;

    /// Current nameservers.
    async fn get_nameservers(&self, domain: &str) -> Result<Vec<String>, IntegrationError>;

    /// Replaces the nameservers.
    async fn set_nameservers(
        &self,
        domain: &str,
        nameservers: &[String],
    ) -> Result<(), IntegrationError>;

    /// Sets / clears the transfer lock.
    async fn set_lock(&self, domain: &str, locked: bool) -> Result<(), IntegrationError>;

    /// Fetches (or regenerates) the EPP auth code.
    async fn get_auth_code(&self, domain: &str) -> Result<SecretString, IntegrationError>;

    /// Enables / disables WHOIS privacy.
    async fn set_privacy(&self, domain: &str, enabled: bool) -> Result<(), IntegrationError>;

    /// Replaces the contacts.
    async fn update_contacts(
        &self,
        domain: &str,
        contacts: &DomainContacts,
    ) -> Result<(), IntegrationError>;

    /// Lists registrar-hosted DNS records. Default: `Unsupported`.
    async fn list_dns_records(&self, domain: &str) -> Result<Vec<DnsRecord>, IntegrationError> {
        let _ = domain;
        Err(IntegrationError::Unsupported("list_dns_records"))
    }

    /// Creates (`id == None`) or updates a record; returns it with its id.
    /// Default: `Unsupported`.
    async fn upsert_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<DnsRecord, IntegrationError> {
        let _ = (domain, record);
        Err(IntegrationError::Unsupported("upsert_dns_record"))
    }

    /// Deletes a record (matched by id, or by name+type+value when the API has no
    /// ids). Default: `Unsupported`.
    async fn delete_dns_record(
        &self,
        domain: &str,
        record: &DnsRecord,
    ) -> Result<(), IntegrationError> {
        let _ = (domain, record);
        Err(IntegrationError::Unsupported("delete_dns_record"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoDns;

    #[async_trait]
    impl DomainRegistrar for NoDns {
        fn kind(&self) -> &'static str {
            "test"
        }
        async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
            Ok(ConnectionInfo::default())
        }
        async fn check_availability(
            &self,
            d: &[String],
        ) -> Result<Vec<DomainAvailability>, IntegrationError> {
            Ok(d.iter()
                .map(|domain| DomainAvailability {
                    domain: domain.clone(),
                    available: true,
                    premium: false,
                    register_price: None,
                    renew_price: None,
                    reason: None,
                })
                .collect())
        }
        async fn tld_pricing(&self, _: &[String]) -> Result<Vec<TldPrice>, IntegrationError> {
            Ok(vec![])
        }
        async fn register(
            &self,
            _: &DomainRegistration,
        ) -> Result<DomainOperation, IntegrationError> {
            Err(IntegrationError::Unsupported("register"))
        }
        async fn transfer(&self, _: &DomainTransfer) -> Result<DomainOperation, IntegrationError> {
            Err(IntegrationError::Unsupported("transfer"))
        }
        async fn renew(&self, _: &DomainRenewal) -> Result<DomainOperation, IntegrationError> {
            Err(IntegrationError::Unsupported("renew"))
        }
        async fn info(&self, d: &str) -> Result<DomainInfo, IntegrationError> {
            Err(IntegrationError::NotFound(d.to_owned()))
        }
        async fn get_nameservers(&self, _: &str) -> Result<Vec<String>, IntegrationError> {
            Ok(vec![])
        }
        async fn set_nameservers(&self, _: &str, _: &[String]) -> Result<(), IntegrationError> {
            Ok(())
        }
        async fn set_lock(&self, _: &str, _: bool) -> Result<(), IntegrationError> {
            Ok(())
        }
        async fn get_auth_code(&self, _: &str) -> Result<SecretString, IntegrationError> {
            Ok(SecretString::from("code"))
        }
        async fn set_privacy(&self, _: &str, _: bool) -> Result<(), IntegrationError> {
            Ok(())
        }
        async fn update_contacts(
            &self,
            _: &str,
            _: &DomainContacts,
        ) -> Result<(), IntegrationError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn dns_defaults_are_unsupported_and_trait_is_object_safe() {
        let r: std::sync::Arc<dyn DomainRegistrar> = std::sync::Arc::new(NoDns);
        assert_eq!(
            r.list_dns_records("example.com").await,
            Err(IntegrationError::Unsupported("list_dns_records"))
        );
        let rec = DnsRecord {
            id: None,
            name: "@".into(),
            record_type: DnsRecordType::A,
            value: "192.0.2.1".into(),
            ttl: Some(300),
            priority: None,
        };
        assert!(matches!(
            r.upsert_dns_record("example.com", &rec).await,
            Err(IntegrationError::Unsupported(_))
        ));
        let any = super::super::AnyIntegration::Registrar(r);
        assert_eq!(any.kind(), "test");
        assert!(any.test_connection().await.is_ok());
        assert_eq!(DnsRecordType::Aaaa.as_str(), "AAAA");
    }

    #[test]
    fn contacts_helpers() {
        let c = ContactInfo {
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            ..ContactInfo::default()
        };
        assert_eq!(c.full_name(), "Ada Lovelace");
        let all = DomainContacts::same(c.clone());
        assert_eq!(all.billing, c);
    }
}
