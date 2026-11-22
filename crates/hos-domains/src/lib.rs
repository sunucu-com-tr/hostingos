//! `DomainRegistrar` implementations for HostingOS plus domain-name tooling.
//!
//! | kind | module | API |
//! |---|---|---|
//! | `resellerclub` | [`resellerclub`] | ResellerClub / LogicBoxes HTTP API (JSON) |
//! | `namecheap` | [`namecheap`] | Namecheap XML API |
//! | `openprovider` | [`openprovider`] | OpenProvider REST v1beta |
//! | `mock` | [`mock`] | in-memory, deterministic (demo / e2e tests) |
//! | `rdap` | [`rdap`] | read-only RDAP fallback (availability + public data) |
//!
//! Helpers: [`name`] (IDNA, SLD/TLD split with the embedded Public Suffix List,
//! search suggestions), [`rdap`] (RDAP client with cached IANA bootstrap),
//! [`pricing`] (selling prices from cost prices), [`tld_rules`] (TLD
//! requirements table).
//!
//! The crate never touches the database; [`build`] turns an `integrations`
//! row (`kind`, `config`, decrypted `secrets`) into an
//! `Arc<dyn DomainRegistrar>`, and [`descriptors`] describes the admin forms.

#![warn(missing_docs)]

use std::sync::Arc;

use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::registrar::DomainRegistrar;
use hos_core::integrations::{FieldSpec, IntegrationDescriptor, SelectOption, kinds};
pub use hos_core::net::OutboundPolicy;
use serde_json::Value;

pub mod common;
pub mod http;
pub mod mock;
pub mod name;
pub mod namecheap;
pub mod openprovider;
pub mod pricing;
pub mod rdap;
pub mod resellerclub;
pub mod tld_rules;

pub use mock::MockRegistrar;
pub use name::{DomainName, DomainNameError, suggest};
pub use namecheap::{Namecheap, NamecheapConfig};
pub use openprovider::{OpenProvider, OpenProviderConfig};
pub use pricing::{Margin, PricingPolicy, PricingRule, Rounding};
pub use rdap::{RdapClient, RdapRegistrar, RdapResult};
pub use resellerclub::{ResellerClub, ResellerClubConfig};
pub use tld_rules::{TldRule, TldRules};

/// Builds a registrar from an `integrations` row. `config`/`secrets` are first
/// validated against the kind's [`IntegrationDescriptor`]. Endpoints are
/// checked against the process-wide outbound policy (`HOS_OUTBOUND_POLICY`,
/// see [`hos_core::net`]).
pub fn build(
    kind: &str,
    config: &Value,
    secrets: &Value,
) -> Result<Arc<dyn DomainRegistrar>, IntegrationError> {
    build_with_policy(kind, config, secrets, OutboundPolicy::global())
}

/// [`build`] with an explicit outbound policy.
pub fn build_with_policy(
    kind: &str,
    config: &Value,
    secrets: &Value,
    policy: OutboundPolicy,
) -> Result<Arc<dyn DomainRegistrar>, IntegrationError> {
    let descriptor = descriptors()
        .into_iter()
        .find(|d| d.kind == kind)
        .ok_or_else(|| IntegrationError::Config(format!("unknown registrar kind {kind:?}")))?;
    descriptor.validate(config, secrets)?;
    Ok(match kind {
        kinds::RESELLERCLUB => {
            let mut cfg = ResellerClubConfig::from_values(config, secrets)?;
            cfg.outbound = policy;
            Arc::new(ResellerClub::new(cfg)?)
        }
        kinds::NAMECHEAP => {
            let mut cfg = NamecheapConfig::from_values(config, secrets)?;
            cfg.outbound = policy;
            Arc::new(Namecheap::new(cfg)?)
        }
        kinds::OPENPROVIDER => {
            let mut cfg = OpenProviderConfig::from_values(config, secrets)?;
            cfg.outbound = policy;
            Arc::new(OpenProvider::new(cfg)?)
        }
        mock::KIND => Arc::new(MockRegistrar::from_config(config, secrets)?),
        rdap::KIND => Arc::new(RdapRegistrar::from_config_with_policy(config, policy)?),
        other => {
            return Err(IntegrationError::Config(format!(
                "unknown registrar kind {other:?}"
            )));
        }
    })
}

fn sandbox_field(label: &'static str, help: &'static str) -> FieldSpec {
    FieldSpec::boolean("sandbox", label)
        .default_value(false)
        .help(help)
}

fn base_url_field() -> FieldSpec {
    FieldSpec::url("base_url", "API base URL (override)")
        .help("Leave empty to use the official live/sandbox endpoint.")
}

fn timeout_field() -> FieldSpec {
    FieldSpec::number("timeout_secs", "Request timeout (seconds)").default_value(30)
}

fn nameservers_field() -> FieldSpec {
    FieldSpec::text_area("default_nameservers", "Default nameservers")
        .help("One per line or comma separated; used when an order has no nameservers.")
}

/// Admin form descriptors for every registrar kind of this crate.
pub fn descriptors() -> Vec<IntegrationDescriptor> {
    vec![
        IntegrationDescriptor::new(
            kinds::RESELLERCLUB,
            "ResellerClub / LogicBoxes",
            IntegrationCategory::Registrar,
        )
        .description(
            "ResellerClub HTTP API. Whitelist this server's IP address under Settings → API in the \
             reseller panel. Orders are created with invoice-option NoInvoice (paid from the \
             reseller balance).",
        )
        .docs_url("https://manage.resellerclub.com/kb/answer/744")
        .config_field(
            FieldSpec::text("reseller_id", "Reseller ID (auth-userid)")
                .required()
                .placeholder("123456"),
        )
        .config_field(sandbox_field(
            "Test mode",
            "Use https://test.httpapi.com/api/ with demo-account credentials.",
        ))
        .config_field(base_url_field())
        .config_field(
            FieldSpec::text("currency", "Account currency")
                .help("ISO code of returned prices; empty = read from resellers/details.json.")
                .placeholder("USD"),
        )
        .config_field(
            FieldSpec::text("default_customer_id", "Customer ID for all orders").help(
                "Empty = one ResellerClub customer per registrant e-mail (created on demand).",
            ),
        )
        .config_field(nameservers_field())
        .config_field(
            FieldSpec::select(
                "price_source",
                "TLD price source",
                vec![
                    SelectOption::new("reseller_cost", "Reseller cost price"),
                    SelectOption::new("customer_price", "Customer selling price"),
                ],
            )
            .default_value("reseller_cost"),
        )
        .config_field(
            FieldSpec::text("language", "Customer language (lang-pref)").default_value("en"),
        )
        .config_field(timeout_field())
        .secret_field(FieldSpec::password("api_key", "API key").required()),
        IntegrationDescriptor::new(
            kinds::NAMECHEAP,
            "Namecheap",
            IntegrationCategory::Registrar,
        )
        .description(
            "Namecheap XML API. Enable API access and whitelist the client IP under Profile → \
                 Tools → API Access. EPP codes cannot be retrieved through the API.",
        )
        .docs_url("https://www.namecheap.com/support/api/intro/")
        .config_field(FieldSpec::text("api_user", "API user (ApiUser)").required())
        .config_field(
            FieldSpec::text("username", "UserName")
                .help("Account the commands act on; defaults to the API user."),
        )
        .config_field(
            FieldSpec::text("client_ip", "Client IP (whitelisted IPv4)")
                .required()
                .placeholder("203.0.113.10"),
        )
        .config_field(sandbox_field(
            "Sandbox",
            "Use https://api.sandbox.namecheap.com (separate sandbox account).",
        ))
        .config_field(base_url_field())
        .config_field(FieldSpec::text("currency", "Account currency").default_value("USD"))
        .config_field(
            FieldSpec::text("privacy_forward_email", "WhoisGuard forwarding e-mail")
                .help("Used when enabling privacy on a domain without a forwarding address."),
        )
        .config_field(timeout_field())
        .secret_field(FieldSpec::password("api_key", "API key").required()),
        IntegrationDescriptor::new(
            kinds::OPENPROVIDER,
            "OpenProvider",
            IntegrationCategory::Registrar,
        )
        .description("OpenProvider REST API (v1beta). 2FA must be disabled for the API user.")
        .docs_url("https://docs.openprovider.com/doc/all")
        .config_field(FieldSpec::text("username", "API username").required())
        .config_field(sandbox_field(
            "Sandbox",
            "Use https://api.sandbox.openprovider.nl/v1beta (separate sandbox account).",
        ))
        .config_field(base_url_field())
        .config_field(
            FieldSpec::text("ns_group", "Nameserver group")
                .help("Used when an order has no nameservers and no defaults are set."),
        )
        .config_field(nameservers_field())
        .config_field(timeout_field())
        .secret_field(FieldSpec::password("password", "API password").required()),
        IntegrationDescriptor::new(
            mock::KIND,
            "Mock registrar (demo)",
            IntegrationCategory::Registrar,
        )
        .description(
            "In-memory registrar for demos and tests: taken-* names are registered, premium-* are \
             premium, fail-* fail. Nothing is persisted.",
        )
        .config_field(FieldSpec::text("currency", "Price currency").default_value("USD"))
        .config_field(nameservers_field()),
        IntegrationDescriptor::new(
            rdap::KIND,
            "RDAP lookup only",
            IntegrationCategory::Registrar,
        )
        .description(
            "Read-only availability and public registration data via RDAP; cannot register \
                 domains. For storefront searches without a registrar account.",
        )
        .docs_url("https://data.iana.org/rdap/dns.json")
        .config_field(
            FieldSpec::url("bootstrap_url", "Bootstrap URL")
                .help("Defaults to the IANA RDAP bootstrap file."),
        )
        .config_field(
            FieldSpec::number("timeout_secs", "Request timeout (seconds)").default_value(15),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptors_are_unique_and_valid() {
        let d = descriptors();
        let mut all: Vec<&str> = d.iter().map(|x| x.kind).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), d.len());
        for desc in &d {
            assert_eq!(desc.category, IntegrationCategory::Registrar);
            serde_json::to_value(desc).unwrap();
        }
        let rc = d.iter().find(|x| x.kind == kinds::RESELLERCLUB).unwrap();
        assert!(rc.config_fields.iter().any(|f| f.key == "sandbox"));
        assert!(
            rc.secret_fields
                .iter()
                .any(|f| f.key == "api_key" && f.required)
        );
    }

    #[test]
    fn build_validates_and_constructs() {
        let rc = build(
            kinds::RESELLERCLUB,
            &json!({"reseller_id": "12345", "sandbox": "on"}),
            &json!({"api_key": "k3y-value"}),
        )
        .unwrap();
        assert_eq!(rc.kind(), kinds::RESELLERCLUB);
        let missing = build(
            kinds::RESELLERCLUB,
            &json!({"reseller_id": "1"}),
            &json!({}),
        );
        assert!(matches!(missing, Err(IntegrationError::Config(_))));
        let nc = build(
            kinds::NAMECHEAP,
            &json!({"api_user": "u", "client_ip": "203.0.113.5", "sandbox": true}),
            &json!({"api_key": "x"}),
        )
        .unwrap();
        assert_eq!(nc.kind(), kinds::NAMECHEAP);
        let bad_ip = build(
            kinds::NAMECHEAP,
            &json!({"api_user": "u", "client_ip": "nope"}),
            &json!({"api_key": "x"}),
        );
        assert!(matches!(bad_ip, Err(IntegrationError::Config(_))));
        let op = build(
            kinds::OPENPROVIDER,
            &json!({"username": "u"}),
            &json!({"password": "p"}),
        )
        .unwrap();
        assert_eq!(op.kind(), kinds::OPENPROVIDER);
        assert_eq!(
            build("mock", &json!({}), &json!({})).unwrap().kind(),
            "mock"
        );
        assert_eq!(
            build("rdap", &json!({}), &json!({})).unwrap().kind(),
            "rdap"
        );
        assert!(matches!(
            build("nope", &json!({}), &json!({})),
            Err(IntegrationError::Config(_))
        ));
    }
}
