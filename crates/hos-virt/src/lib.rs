//! # hos-virt
//!
//! [`VirtProvider`] implementations for HostingOS (docs/ARCHITECTURE.md §4):
//!
//! | `kind` | Provider | Module |
//! |---|---|---|
//! | [`kinds::PROXMOX`] | Proxmox VE (`/api2/json`, API token; QEMU + LXC, clusters) | [`proxmox`] |
//! | [`kinds::VMWARE`] | VMware vCenter (vSphere Automation REST `/api`, optional VI/JSON) | [`vmware`] |
//! | [`kinds::VIRTUALIZOR`] | Virtualizor admin API | [`virtualizor`] |
//! | [`kinds::HETZNER`] | Hetzner Cloud API v1 | [`hetzner`] |
//! | [`MOCK`] | In-memory "Demo / Test" provider | [`mock`] |
//!
//! Integrations are created from their `integrations` row with [`build`]; the
//! admin form is generated from [`descriptors`]. This crate never touches the
//! database.
//!
//! All HTTP providers share the plumbing in `http`: request timeouts, no
//! redirect following, bounded retries for idempotent calls only (transport
//! errors, 429 and the provider's transient 5xx statuses), consistent
//! status → [`IntegrationError`] mapping, provider error-body parsing and logs
//! that never contain credentials, query strings or bodies.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;

use hos_core::IntegrationError;
use hos_core::integrations::{IntegrationDescriptor, VirtProvider, kinds};
pub use hos_core::net::OutboundPolicy;

mod cloudinit;
mod http;
mod util;

pub mod console;
pub mod hetzner;
pub mod mock;
pub mod proxmox;
pub mod virtualizor;
pub mod vmware;

pub use hetzner::HetznerProvider;
pub use mock::MockProvider;
pub use proxmox::ProxmoxProvider;
pub use virtualizor::VirtualizorProvider;
pub use vmware::VmwareProvider;

/// `integrations.kind` of the in-memory demo/test provider.
///
/// Not (yet) part of [`hos_core::integrations::kinds`]; see the crate report.
pub const MOCK: &str = "mock";

/// Every `kind` this crate can build.
pub const KINDS: &[&str] = &[
    kinds::PROXMOX,
    kinds::VMWARE,
    kinds::VIRTUALIZOR,
    kinds::HETZNER,
    MOCK,
];

/// Builds a provider from an `integrations` row.
///
/// `config` is the plain `integrations.config` JSON, `secrets` the decrypted
/// `integrations.secrets_enc` JSON. Both are validated against the kind's
/// [`IntegrationDescriptor`] first; problems are [`IntegrationError::Config`].
/// Destinations are checked against the process-wide outbound policy
/// (`HOS_OUTBOUND_POLICY`, see [`hos_core::net`]).
pub fn build(
    kind: &str,
    config: &serde_json::Value,
    secrets: &serde_json::Value,
) -> Result<Arc<dyn VirtProvider>, IntegrationError> {
    build_with_policy(kind, config, secrets, OutboundPolicy::global())
}

/// [`build`] with an explicit outbound policy.
pub fn build_with_policy(
    kind: &str,
    config: &serde_json::Value,
    secrets: &serde_json::Value,
    policy: OutboundPolicy,
) -> Result<Arc<dyn VirtProvider>, IntegrationError> {
    let descriptor = descriptor(kind).ok_or_else(|| {
        IntegrationError::Config(format!("unknown virtualization provider kind: {kind:?}"))
    })?;
    descriptor.validate(config, secrets)?;
    Ok(match kind {
        kinds::PROXMOX => Arc::new(ProxmoxProvider::from_config_with_policy(
            config, secrets, policy,
        )?),
        kinds::VMWARE => Arc::new(VmwareProvider::from_config_with_policy(
            config, secrets, policy,
        )?),
        kinds::VIRTUALIZOR => Arc::new(VirtualizorProvider::from_config_with_policy(
            config, secrets, policy,
        )?),
        kinds::HETZNER => Arc::new(HetznerProvider::from_config_with_policy(
            config, secrets, policy,
        )?),
        _ => Arc::new(MockProvider::from_config(config, secrets)?),
    })
}

/// Admin-form descriptors of every provider in this crate.
pub fn descriptors() -> Vec<IntegrationDescriptor> {
    vec![
        proxmox::descriptor(),
        vmware::descriptor(),
        virtualizor::descriptor(),
        hetzner::descriptor(),
        mock::descriptor(),
    ]
}

fn descriptor(kind: &str) -> Option<IntegrationDescriptor> {
    match kind {
        kinds::PROXMOX => Some(proxmox::descriptor()),
        kinds::VMWARE => Some(vmware::descriptor()),
        kinds::VIRTUALIZOR => Some(virtualizor::descriptor()),
        kinds::HETZNER => Some(hetzner::descriptor()),
        MOCK => Some(mock::descriptor()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::domain::IntegrationCategory;
    use serde_json::json;

    #[test]
    fn descriptors_cover_every_kind() {
        let ds = descriptors();
        assert_eq!(ds.len(), KINDS.len());
        for (d, kind) in ds.iter().zip(KINDS) {
            assert_eq!(d.kind, *kind);
            assert_eq!(d.category, IntegrationCategory::Virt);
            assert!(d.docs_url.is_some() || d.kind == MOCK);
            // Every descriptor must serialize for the admin form.
            serde_json::to_value(d).unwrap();
        }
        assert_eq!(mock::descriptor().display_name, "Demo / Test");
    }

    #[test]
    fn unknown_kind_is_config_error() {
        let err = build("libvirt", &json!({}), &json!({})).err().unwrap();
        assert_eq!(err.kind(), "config");
    }

    #[test]
    fn build_validates_required_fields() {
        let err = build(kinds::PROXMOX, &json!({}), &json!({})).err().unwrap();
        assert_eq!(
            err,
            IntegrationError::Config("config.api_url is required".into())
        );
        let err = build(kinds::HETZNER, &json!({}), &json!({"api_token": "  "}))
            .err()
            .unwrap();
        assert_eq!(
            err,
            IntegrationError::Config("secrets.api_token is required".into())
        );
    }

    #[test]
    fn builds_every_kind() {
        let cases = [
            (
                kinds::PROXMOX,
                json!({"api_url": "https://pve.example.com:8006", "token_id": "root@pam!hos"}),
                json!({"token_secret": "x"}),
            ),
            (
                kinds::VMWARE,
                json!({"api_url": "https://vc.example.com", "username": "a@vsphere.local"}),
                json!({"password": "x"}),
            ),
            (
                kinds::VIRTUALIZOR,
                json!({"api_url": "https://virt.example.com:4085"}),
                json!({"api_key": "k", "api_pass": "p"}),
            ),
            (kinds::HETZNER, json!({}), json!({"api_token": "t"})),
            (MOCK, json!({}), json!({})),
        ];
        for (kind, config, secrets) in cases {
            let p = build(kind, &config, &secrets).unwrap();
            assert_eq!(p.kind(), kind);
        }
    }
}
