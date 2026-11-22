//! `HostingPanel` implementations for HostingOS (docs/ARCHITECTURE.md §4).
//!
//! | kind | Module | API |
//! |---|---|---|
//! | `cpanel` | [`cpanel`] | WHM API 1 (`/json-api/*`, API token) |
//! | `plesk` | [`plesk`] | REST API v2 (`/api/v2/*`) + its CLI gateway, XML API for plan listing |
//! | `directadmin` | [`directadmin`] | legacy `CMD_API_*` + `/api/login-keys/urls` |
//! | `cyberpanel` | [`cyberpanel`] | `/api/*` JSON endpoints |
//! | `mock` | [`mock`] | in-memory simulation |
//!
//! Factories: [`build`] and [`descriptors`]. Helpers for callers:
//! [`username`] (panel-compliant username generation) and [`password`]
//! (panel-compliant strong passwords).
//!
//! Security: credentials and account passwords are never logged; they travel
//! in headers or request bodies (never URLs) and are scrubbed from every error
//! message built from a panel response.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;

use hos_core::IntegrationError;
use hos_core::integrations::{HostingPanel, IntegrationDescriptor, kinds};
pub use hos_core::net::OutboundPolicy;
use serde_json::Value;

mod http;
mod settings;
mod units;

pub mod cpanel;
pub mod cyberpanel;
pub mod directadmin;
pub mod mock;
pub mod password;
pub mod plesk;
pub mod username;

/// `integrations.kind` of the simulated panel ([`mock`]). hos-core's
/// [`kinds`] has no constant for it.
pub const MOCK: &str = mock::KIND;

/// Every panel kind this crate can build.
pub const SUPPORTED_KINDS: &[&str] = &[
    kinds::CPANEL,
    kinds::PLESK,
    kinds::DIRECTADMIN,
    kinds::CYBERPANEL,
    MOCK,
];

/// Builds a panel client from an `integrations` row. `config` is the plain
/// JSON config, `secrets` the decrypted secrets JSON. Both are validated
/// against the kind's [`descriptors`] entry first. The panel host is checked
/// against the process-wide outbound policy (`HOS_OUTBOUND_POLICY`, see
/// [`hos_core::net`]).
pub fn build(
    kind: &str,
    config: &Value,
    secrets: &Value,
) -> Result<Arc<dyn HostingPanel>, IntegrationError> {
    build_with_policy(kind, config, secrets, OutboundPolicy::global())
}

/// [`build`] with an explicit outbound policy.
pub fn build_with_policy(
    kind: &str,
    config: &Value,
    secrets: &Value,
    policy: OutboundPolicy,
) -> Result<Arc<dyn HostingPanel>, IntegrationError> {
    let descriptor = descriptor(kind)
        .ok_or_else(|| IntegrationError::Config(format!("unknown panel kind `{kind}`")))?;
    descriptor.validate(config, secrets)?;
    Ok(match kind {
        kinds::CPANEL => Arc::new(cpanel::CpanelPanel::from_config_with_policy(
            config, secrets, policy,
        )?),
        kinds::PLESK => Arc::new(plesk::PleskPanel::from_config_with_policy(
            config, secrets, policy,
        )?),
        kinds::DIRECTADMIN => Arc::new(directadmin::DirectAdminPanel::from_config_with_policy(
            config, secrets, policy,
        )?),
        kinds::CYBERPANEL => Arc::new(cyberpanel::CyberPanel::from_config_with_policy(
            config, secrets, policy,
        )?),
        _ => Arc::new(mock::MockPanel::from_config(config)?),
    })
}

/// Admin form schemas of every panel kind.
pub fn descriptors() -> Vec<IntegrationDescriptor> {
    vec![
        cpanel::descriptor(),
        plesk::descriptor(),
        directadmin::descriptor(),
        cyberpanel::descriptor(),
        mock::descriptor(),
    ]
}

/// Schema of one kind.
pub fn descriptor(kind: &str) -> Option<IntegrationDescriptor> {
    descriptors().into_iter().find(|d| d.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::domain::IntegrationCategory;
    use serde_json::json;

    #[test]
    fn descriptors_cover_every_kind() {
        let ds = descriptors();
        assert_eq!(ds.len(), SUPPORTED_KINDS.len());
        for kind in SUPPORTED_KINDS {
            let d = descriptor(kind).unwrap();
            assert_eq!(d.category, IntegrationCategory::Panel);
            // Secret-looking keys must never be plain config fields.
            for f in &d.config_fields {
                assert!(
                    ![
                        "password",
                        "api_token",
                        "api_key",
                        "login_key",
                        "admin_pass"
                    ]
                    .contains(&f.key),
                    "{kind}: {} must be a secret field",
                    f.key
                );
            }
            serde_json::to_value(&d).unwrap();
        }
    }

    #[test]
    fn build_rejects_unknown_and_invalid() {
        assert!(matches!(
            build("ispconfig", &json!({}), &json!({})),
            Err(IntegrationError::Config(_))
        ));
        // Missing required host/token.
        assert!(matches!(
            build(kinds::CPANEL, &json!({}), &json!({})),
            Err(IntegrationError::Config(_))
        ));
        let p = build(
            kinds::CPANEL,
            &json!({"host": "whm.example.com"}),
            &json!({"api_token": "TOKEN"}),
        )
        .unwrap();
        assert_eq!(p.kind(), kinds::CPANEL);
        let m = build(MOCK, &json!({}), &json!({})).unwrap();
        assert_eq!(m.kind(), MOCK);
    }
}
