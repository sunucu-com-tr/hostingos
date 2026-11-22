//! Shared helpers for the wiremock-based panel tests.

#![allow(dead_code)]

use std::sync::Arc;

use hos_core::integrations::HostingPanel;
use hos_core::integrations::panel::{AccountOwner, AccountSpec};
use hos_core::secret::SecretString;
use serde_json::{Value, json};
use wiremock::{Match, MockServer, Request};

/// Matches a form-encoded body containing (`exact = false`) or consisting of
/// exactly (`exact = true`) the given pairs, order-insensitive.
pub struct FormParams {
    expected: Vec<(String, String)>,
    exact: bool,
}

impl Match for FormParams {
    fn matches(&self, request: &Request) -> bool {
        let mut got: Vec<(String, String)> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        if self.exact {
            let mut want = self.expected.clone();
            got.sort();
            want.sort();
            got == want
        } else {
            self.expected.iter().all(|p| got.contains(p))
        }
    }
}

fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
    p.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// Body contains these form pairs.
pub fn form(p: &[(&str, &str)]) -> FormParams {
    FormParams {
        expected: pairs(p),
        exact: false,
    }
}

/// Body is exactly these form pairs.
pub fn form_exact(p: &[(&str, &str)]) -> FormParams {
    FormParams {
        expected: pairs(p),
        exact: true,
    }
}

/// Request body does NOT contain `needle` anywhere (e.g. a field name).
pub struct BodyLacks(pub &'static str);

impl Match for BodyLacks {
    fn matches(&self, request: &Request) -> bool {
        !String::from_utf8_lossy(&request.body).contains(self.0)
    }
}

/// Builds a panel pointed at the mock server (plain HTTP).
pub fn panel(
    kind: &str,
    server: &MockServer,
    mut config: Value,
    secrets: Value,
) -> Arc<dyn HostingPanel> {
    config["host"] = json!(server.uri());
    config["use_ssl"] = json!(false);
    hos_panels::build_with_policy(
        kind,
        &config,
        &secrets,
        hos_panels::OutboundPolicy::Unrestricted,
    )
    .expect("panel builds")
}

/// A typical account spec.
pub fn spec(username: &str, domain: &str, package: &str) -> AccountSpec {
    AccountSpec {
        username: username.into(),
        domain: domain.into(),
        password: SecretString::from("S3cure!Passw0rd-xyz"),
        package: package.into(),
        owner: AccountOwner {
            name: Some("Ayşe Yılmaz".into()),
            company: Some("Örnek Ltd".into()),
            email: "ayse@example.com".into(),
        },
        ..AccountSpec::default()
    }
}

/// The spec password (for scrubbing assertions).
pub const SPEC_PASSWORD: &str = "S3cure!Passw0rd-xyz";
