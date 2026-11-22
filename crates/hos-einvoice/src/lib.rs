//! Turkish e-invoicing for HostingOS: `EInvoiceProvider` implementations
//! (category `einvoice`) and the shared UBL-TR 2.1 document builder.
//!
//! | kind | module | transport |
//! |---|---|---|
//! | `parasut` | [`parasut`] | Paraşüt API v4 (JSON:API, OAuth 2); Paraşüt numbers the documents |
//! | `nilvera` | [`nilvera`] | Nilvera REST, UBL-TR XML upload; HostingOS numbers the documents |
//! | `mock` | [`mock`] | in-memory (demo / tests; opt-in only) |
//!
//! Building blocks: [`taxid`] (VKN / TCKN check digits), [`codes`] (GİB code
//! lists, legal numbers), [`compose`] (HostingOS invoice → lines / VAT /
//! totals reconciled to the kuruş), [`ubl`] (XML + offline schematron
//! subset), [`xml`] (escaping writer, hardened reader), [`http`] (guarded
//! client).
//!
//! The crate never touches the database: [`build`] turns an `integrations`
//! row into an `Arc<dyn EInvoiceProvider>` and [`descriptors`] describes the
//! admin forms. Evaluated and **not** implemented (see `docs/EINVOICE.md`):
//! Uyumsoft and EDM (only a WSDL is published, no documented numbering /
//! duplicate / cancellation semantics), İzibiz and KolayBi (not enough
//! public contract detail), others whose documentation was unreachable.

#![warn(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::einvoice::EInvoiceProvider;
use hos_core::integrations::{FieldSpec, IntegrationDescriptor, SelectOption};
pub use hos_core::net::OutboundPolicy;
use serde_json::Value;

pub mod codes;
pub mod compose;
pub mod http;
pub mod mock;
pub mod nilvera;
pub mod parasut;
pub mod taxid;
pub mod ubl;
pub mod xml;

pub use mock::MockProvider;
pub use nilvera::{Nilvera, NilveraConfig};
pub use parasut::{Parasut, ParasutConfig};

/// Builds a provider from an `integrations` row under the process-wide
/// outbound policy.
pub fn build(
    kind: &str,
    config: &Value,
    secrets: &Value,
) -> Result<Arc<dyn EInvoiceProvider>, IntegrationError> {
    build_with_policy(kind, config, secrets, OutboundPolicy::global())
}

/// [`build`] with an explicit outbound policy (tests use `Unrestricted`).
pub fn build_with_policy(
    kind: &str,
    config: &Value,
    secrets: &Value,
    policy: OutboundPolicy,
) -> Result<Arc<dyn EInvoiceProvider>, IntegrationError> {
    let descriptor = descriptors()
        .into_iter()
        .find(|d| d.kind == kind)
        .ok_or_else(|| IntegrationError::Config(format!("unknown e-invoice kind {kind:?}")))?;
    descriptor.validate(config, secrets)?;
    Ok(match kind {
        parasut::KIND => {
            let mut cfg = ParasutConfig::from_values(config, secrets)?;
            cfg.outbound = policy;
            Arc::new(Parasut::new(cfg)?)
        }
        nilvera::KIND => {
            let mut cfg = NilveraConfig::from_values(config, secrets)?;
            cfg.outbound = policy;
            Arc::new(Nilvera::new(cfg)?)
        }
        mock::KIND => Arc::new(MockProvider::from_config(config)?),
        other => {
            return Err(IntegrationError::Config(format!(
                "unknown e-invoice kind {other:?}"
            )));
        }
    })
}

fn timeout_field() -> FieldSpec {
    FieldSpec::number("timeout_secs", "Request timeout (seconds)").default_value(30)
}

/// Admin form descriptors of every kind in this crate.
pub fn descriptors() -> Vec<IntegrationDescriptor> {
    vec![
        IntegrationDescriptor::new(parasut::KIND, "Paraşüt (e-Fatura / e-Arşiv)", IntegrationCategory::Einvoice)
            .description(
                "Paraşüt API v4. Paraşüt creates the contact and sales invoice, issues the e-Fatura or \
                 e-Arşiv document and assigns its number. Request API access (client id / secret) from \
                 Paraşüt; e-documents must be activated on the company.",
            )
            .docs_url("https://apidocs.parasut.com")
            .config_field(FieldSpec::text("company_id", "Company ID (firma no)").required())
            .config_field(FieldSpec::text("client_id", "OAuth client ID").required())
            .config_field(FieldSpec::text("username", "User e-mail").required())
            .config_field(
                FieldSpec::boolean("commercial_scenario", "Send e-Fatura as TICARIFATURA (commercial)")
                    .default_value(false)
                    .help("Off: TEMELFATURA (basic scenario)."),
            )
            .config_field(
                FieldSpec::boolean("test_company", "This is a Paraşüt test company")
                    .default_value(false)
                    .help("Documents of a test company are marked as test documents."),
            )
            .config_field(
                FieldSpec::url("base_url", "API base URL (override)")
                    .help("Leave empty for https://api.parasut.com."),
            )
            .config_field(timeout_field())
            .secret_field(FieldSpec::password("client_secret", "OAuth client secret").required())
            .secret_field(FieldSpec::password("password", "User password").required()),
        IntegrationDescriptor::new(nilvera::KIND, "Nilvera (e-Fatura / e-Arşiv)", IntegrationCategory::Einvoice)
            .description(
                "Nilvera private integrator. HostingOS builds the UBL-TR document and numbers it with \
                 the series configured in the e-invoice settings — reserve that series for HostingOS. \
                 Create the API key in the Nilvera portal (API Tanımları).",
            )
            .docs_url("https://developer.nilvera.com")
            .config_field(
                FieldSpec::select(
                    "environment",
                    "Environment",
                    vec![
                        SelectOption::new("test", "Test (apitest.nilvera.com)"),
                        SelectOption::new("live", "Live (api.nilvera.com)"),
                    ],
                )
                .required()
                .default_value("test"),
            )
            .config_field(
                FieldSpec::url("base_url", "API base URL (override)")
                    .help("Leave empty to use the official host of the environment."),
            )
            .config_field(timeout_field())
            .secret_field(FieldSpec::password("api_key", "API key").required()),
        IntegrationDescriptor::new(mock::KIND, "Mock e-invoice provider", IntegrationCategory::Einvoice)
            .description("In-memory provider for demos and tests. Issues no legal documents.")
            .config_field(
                FieldSpec::text("registered", "Tax ids that count as e-Fatura users")
                    .help("Comma separated."),
            )
            .config_field(FieldSpec::select(
                "fail",
                "Simulate failures",
                vec![
                    SelectOption::new("", "No"),
                    SelectOption::new("transient", "Transient (network)"),
                    SelectOption::new("permanent", "Permanent (refused)"),
                ],
            ))
            .config_field(FieldSpec::boolean("async", "Asynchronous status").default_value(false)),
    ]
}

/// Default request timeout of the HTTP providers.
pub const DEFAULT_TIMEOUT: Duration = http::DEFAULT_TIMEOUT;
