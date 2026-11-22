//! Integration ("module") model — the WHMCS module equivalent (docs/ARCHITECTURE.md §4).
//!
//! One trait per category, each in its own file:
//!
//! | Category | Trait | Implemented in |
//! |---|---|---|
//! | `virt` | [`VirtProvider`] | `hos-virt` |
//! | `panel` | [`HostingPanel`] | `hos-panels` |
//! | `registrar` | [`DomainRegistrar`] | `hos-domains` |
//! | `gateway` | [`PaymentGateway`] | `hos-payments` |
//! | `mail` | [`Mailer`] | (SMTP impl, see report) |
//! | `ai` | [`AiProvider`] | `hos-seo` (Anthropic Messages API) |
//! | `einvoice` | [`EInvoiceProvider`] | `hos-einvoice` (Paraşüt, Nilvera) |
//!
//! ## Conventions for implementors
//!
//! * Every method is `async` (via `#[async_trait]`), every trait is `Send + Sync`
//!   and every fallible method returns `Result<_, IntegrationError>`.
//! * Unsupported operations return [`IntegrationError::Unsupported`] — never
//!   panic, never `todo!()`. Optional operations already have such defaults.
//! * Each integration crate exposes
//!   `pub fn build(kind: &str, config: &serde_json::Value, secrets: &serde_json::Value) -> Result<Arc<dyn Trait>, IntegrationError>`
//!   and `pub fn descriptors() -> Vec<IntegrationDescriptor>`. Use [`FieldReader`]
//!   inside `build` and the constants in [`kinds`] for `kind` strings.
//! * `config` is the plain `integrations.config` JSON; `secrets` is the decrypted
//!   `integrations.secrets_enc` JSON. Both are flat objects keyed by
//!   [`FieldSpec::key`]. Admin forms submit strings, so readers accept `"8006"`
//!   for numbers and `"true"`/`"on"` for booleans.
//! * Every HTTP client must have a timeout; map timeouts to
//!   [`IntegrationError::Timeout`] and connection errors to
//!   [`IntegrationError::Transport`]; [`IntegrationError::from_http_status`] covers
//!   the common status mapping.
//! * Provider-specific knobs go into the `extra: serde_json::Value` fields of the
//!   request types instead of new struct fields.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::IntegrationCategory;
use crate::error::IntegrationError;
use crate::secret::SecretString;

pub mod ai;
pub mod einvoice;
pub mod gateway;
pub mod mailer;
pub mod panel;
pub mod registrar;
pub mod virt;

pub use ai::AiProvider;
pub use einvoice::EInvoiceProvider;
pub use gateway::PaymentGateway;
pub use http::HeaderMap;
pub use mailer::Mailer;
pub use panel::HostingPanel;
pub use registrar::DomainRegistrar;
pub use virt::VirtProvider;

/// Canonical `integrations.kind` strings. Use these instead of literals.
pub mod kinds {
    /// Proxmox VE (`virt`).
    pub const PROXMOX: &str = "proxmox";
    /// VMware vSphere / ESXi (`virt`).
    pub const VMWARE: &str = "vmware";
    /// Virtualizor (`virt`).
    pub const VIRTUALIZOR: &str = "virtualizor";
    /// Hetzner Cloud (`virt`).
    pub const HETZNER: &str = "hetzner";
    /// cPanel / WHM (`panel`).
    pub const CPANEL: &str = "cpanel";
    /// Plesk (`panel`).
    pub const PLESK: &str = "plesk";
    /// DirectAdmin (`panel`).
    pub const DIRECTADMIN: &str = "directadmin";
    /// CyberPanel (`panel`).
    pub const CYBERPANEL: &str = "cyberpanel";
    /// ResellerClub (`registrar`).
    pub const RESELLERCLUB: &str = "resellerclub";
    /// Namecheap (`registrar`).
    pub const NAMECHEAP: &str = "namecheap";
    /// OpenProvider (`registrar`).
    pub const OPENPROVIDER: &str = "openprovider";
    /// Stripe (`gateway`).
    pub const STRIPE: &str = "stripe";
    /// iyzico (`gateway`).
    pub const IYZICO: &str = "iyzico";
    /// PayTR (`gateway`).
    pub const PAYTR: &str = "paytr";
    /// PayPal (`gateway`).
    pub const PAYPAL: &str = "paypal";
    /// Manual bank transfer / EFT (`gateway`, offline).
    pub const BANK_TRANSFER: &str = "bank_transfer";
    /// NestPay / Payten (EST) bank virtual POS, 3D Pay Hosting (`gateway`).
    pub const NESTPAY: &str = "nestpay";
    /// Paynet (`gateway`).
    pub const PAYNET: &str = "paynet";
    /// Papara Checkout (`gateway`).
    pub const PAPARA: &str = "papara";
    /// SMTP (`mail`).
    pub const SMTP: &str = "smtp";
    /// Anthropic Claude (`ai`).
    pub const ANTHROPIC: &str = "anthropic";
    /// Google OAuth 2.0 / OpenID Connect application (`oauth`).
    pub const GOOGLE: &str = "google";
    /// GitHub OAuth application (`oauth`).
    pub const GITHUB: &str = "github";
    /// Paraşüt API v4 (`einvoice`).
    pub const PARASUT: &str = "parasut";
    /// Nilvera (`einvoice`).
    pub const NILVERA: &str = "nilvera";
}

// ---------------------------------------------------------------------------
// Shared value types
// ---------------------------------------------------------------------------

/// Successful result of `test_connection`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    /// Provider/server version, when reported (e.g. `"pve-manager/8.2.4"`).
    pub server_version: Option<String>,
    /// Any extra human-readable detail (account name, balance, node count).
    pub details: Option<String>,
}

impl ConnectionInfo {
    /// One line for the admin UI / the stored test result: version and
    /// details joined with ` · `, cut like
    /// [`IntegrationError::summary`](crate::error::IntegrationError::summary).
    pub fn summary(&self) -> String {
        let joined = [self.server_version.as_deref(), self.details.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        crate::error::one_line(&joined, 2 * crate::error::SUMMARY_DETAIL_CHARS)
    }
}

/// A postal address (billing address, domain contact address).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostalAddress {
    /// Street address, first line.
    pub line1: String,
    /// Street address, second line.
    pub line2: Option<String>,
    /// City / district.
    pub city: String,
    /// State / province / region.
    pub state: Option<String>,
    /// Postal / ZIP code.
    pub postal_code: Option<String>,
    /// ISO 3166-1 alpha-2 country code, upper-case (`TR`).
    pub country_code: String,
}

/// A phone number split into country calling code and national number.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneNumber {
    /// Country calling code, digits only (`"90"`).
    pub country_code: String,
    /// National significant number, digits only (`"5551234567"`).
    pub number: String,
}

impl PhoneNumber {
    /// Creates a number, stripping every non-digit (`"+90"`, `"555 123 45 67"`).
    pub fn new(country_code: &str, number: &str) -> Self {
        let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
        Self {
            country_code: digits(country_code),
            number: digits(number),
        }
    }

    /// E.164 format: `+905551234567`.
    pub fn to_e164(&self) -> String {
        format!("+{}{}", self.country_code, self.number)
    }

    /// EPP / registrar format: `+90.5551234567`.
    pub fn to_epp(&self) -> String {
        format!("+{}.{}", self.country_code, self.number)
    }
}

// ---------------------------------------------------------------------------
// Descriptors (admin form schema)
// ---------------------------------------------------------------------------

/// Static description of an integration kind; the admin "add integration" form
/// is generated from it. Returned by each crate's `descriptors()`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntegrationDescriptor {
    /// `integrations.kind`, e.g. [`kinds::PROXMOX`].
    pub kind: &'static str,
    /// Human readable name, e.g. `"Proxmox VE"`.
    pub display_name: &'static str,
    /// Category.
    pub category: IntegrationCategory,
    /// Short description / notes for the admin (optional).
    pub description: Option<&'static str>,
    /// Link to the provider's API documentation (optional).
    pub docs_url: Option<&'static str>,
    /// Non-secret settings stored in `integrations.config`.
    pub config_fields: Vec<FieldSpec>,
    /// Secret settings stored encrypted in `integrations.secrets_enc`.
    pub secret_fields: Vec<FieldSpec>,
}

impl IntegrationDescriptor {
    /// Creates a descriptor with no fields.
    pub fn new(
        kind: &'static str,
        display_name: &'static str,
        category: IntegrationCategory,
    ) -> Self {
        Self {
            kind,
            display_name,
            category,
            description: None,
            docs_url: None,
            config_fields: Vec::new(),
            secret_fields: Vec::new(),
        }
    }

    /// Sets the description.
    #[must_use]
    pub fn description(mut self, text: &'static str) -> Self {
        self.description = Some(text);
        self
    }

    /// Sets the documentation URL.
    #[must_use]
    pub fn docs_url(mut self, url: &'static str) -> Self {
        self.docs_url = Some(url);
        self
    }

    /// Appends a config field.
    #[must_use]
    pub fn config_field(mut self, field: FieldSpec) -> Self {
        self.config_fields.push(field);
        self
    }

    /// Appends a secret field.
    #[must_use]
    pub fn secret_field(mut self, field: FieldSpec) -> Self {
        self.secret_fields.push(field);
        self
    }

    /// Checks `config`/`secrets` against the field specs: required fields
    /// present and non-empty, numbers/booleans/URLs parse, select values valid.
    /// Unknown keys are ignored. Errors are [`IntegrationError::Config`].
    pub fn validate(&self, config: &Value, secrets: &Value) -> Result<(), IntegrationError> {
        validate_fields(&self.config_fields, config, "config")?;
        validate_fields(&self.secret_fields, secrets, "secrets")
    }
}

/// One form field of an [`IntegrationDescriptor`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldSpec {
    /// JSON key in `config`/`secrets`.
    pub key: &'static str,
    /// Form label.
    pub label: &'static str,
    /// Input type.
    pub kind: FieldKind,
    /// Whether a non-empty value is mandatory.
    pub required: bool,
    /// Help text shown under the input.
    pub help: Option<&'static str>,
    /// Pre-filled default value.
    pub default: Option<Value>,
    /// Input placeholder.
    pub placeholder: Option<&'static str>,
}

/// Input type of a [`FieldSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "options", rename_all = "snake_case")]
pub enum FieldKind {
    /// Single-line text.
    Text,
    /// Multi-line text (PEM certificates, SSH keys, notes). It may also
    /// hold structured JSON (an array or object stored as such, e.g. a
    /// package list), which factories read with `FieldReader::opt_json`.
    TextArea,
    /// Masked input; never echoed back to the browser.
    Password,
    /// Numeric input.
    Number,
    /// Checkbox.
    Bool,
    /// One of a fixed set of values.
    Select(Vec<SelectOption>),
    /// An `http(s)://` URL.
    Url,
}

/// A choice of a [`FieldKind::Select`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelectOption {
    /// Stored value.
    pub value: &'static str,
    /// Display label.
    pub label: &'static str,
}

impl SelectOption {
    /// Creates an option.
    pub const fn new(value: &'static str, label: &'static str) -> Self {
        Self { value, label }
    }
}

impl FieldSpec {
    /// Creates an optional field of the given kind.
    pub const fn new(key: &'static str, label: &'static str, kind: FieldKind) -> Self {
        Self {
            key,
            label,
            kind,
            required: false,
            help: None,
            default: None,
            placeholder: None,
        }
    }

    /// Single-line text field.
    pub const fn text(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Text)
    }

    /// Multi-line text field.
    pub const fn text_area(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::TextArea)
    }

    /// Password / API key field.
    pub const fn password(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Password)
    }

    /// Number field.
    pub const fn number(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Number)
    }

    /// Checkbox field.
    pub const fn boolean(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Bool)
    }

    /// URL field.
    pub const fn url(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Url)
    }

    /// Select field.
    pub fn select(key: &'static str, label: &'static str, options: Vec<SelectOption>) -> Self {
        Self::new(key, label, FieldKind::Select(options))
    }

    /// Marks the field as required.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Sets the help text.
    #[must_use]
    pub fn help(mut self, text: &'static str) -> Self {
        self.help = Some(text);
        self
    }

    /// Sets the default value.
    #[must_use]
    pub fn default_value(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Sets the placeholder.
    #[must_use]
    pub fn placeholder(mut self, text: &'static str) -> Self {
        self.placeholder = Some(text);
        self
    }
}

fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

fn parse_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn parse_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_http_url(s: &str) -> Option<url::Url> {
    url::Url::parse(s.trim())
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
}

fn validate_fields(
    fields: &[FieldSpec],
    value: &Value,
    scope: &str,
) -> Result<(), IntegrationError> {
    let empty = serde_json::Map::new();
    let obj = match value {
        Value::Object(map) => map,
        Value::Null => &empty,
        _ => {
            return Err(IntegrationError::Config(format!(
                "{scope} must be a JSON object"
            )));
        }
    };
    for field in fields {
        let key = field.key;
        let v = match obj.get(key) {
            Some(v) if !is_blank(v) => v,
            _ if field.required => {
                return Err(IntegrationError::Config(format!(
                    "{scope}.{key} is required"
                )));
            }
            _ => continue,
        };
        let ok = match &field.kind {
            FieldKind::Text | FieldKind::Password => v.is_string(),
            FieldKind::TextArea => v.is_string() || v.is_array() || v.is_object(),
            FieldKind::Number => parse_f64(v).is_some(),
            FieldKind::Bool => parse_bool(v).is_some(),
            FieldKind::Url => v.as_str().and_then(parse_http_url).is_some(),
            FieldKind::Select(options) => v
                .as_str()
                .is_some_and(|s| options.iter().any(|o| o.value == s)),
        };
        if !ok {
            return Err(IntegrationError::Config(format!(
                "{scope}.{key} has an invalid value"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FieldReader (for factories)
// ---------------------------------------------------------------------------

/// Typed, lenient reader over an integration's `config` or `secrets` JSON,
/// for use inside `build()`. Errors are [`IntegrationError::Config`] naming the
/// key but never the value.
///
/// ```
/// use hos_core::integrations::FieldReader;
/// let config = serde_json::json!({"host": "pve.example.com", "port": "8006", "verify_tls": "on"});
/// let r = FieldReader::config(&config);
/// assert_eq!(r.str("host").unwrap(), "pve.example.com");
/// assert_eq!(r.u64_or("port", 443).unwrap(), 8006);
/// assert!(r.bool_or("verify_tls", false).unwrap());
/// assert!(r.str("missing").is_err());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct FieldReader<'a> {
    value: &'a Value,
    scope: &'static str,
}

impl<'a> FieldReader<'a> {
    /// Reader over `integrations.config`.
    pub fn config(value: &'a Value) -> Self {
        Self {
            value,
            scope: "config",
        }
    }

    /// Reader over decrypted `integrations.secrets_enc`.
    pub fn secrets(value: &'a Value) -> Self {
        Self {
            value,
            scope: "secrets",
        }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.value.get(key).filter(|v| !is_blank(v))
    }

    fn missing(&self, key: &str) -> IntegrationError {
        IntegrationError::Config(format!("{}.{key} is required", self.scope))
    }

    fn invalid(&self, key: &str, expected: &str) -> IntegrationError {
        IntegrationError::Config(format!("{}.{key} must be {expected}", self.scope))
    }

    /// Whether a non-blank value exists for `key`.
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Required non-empty string (trimmed).
    pub fn str(&self, key: &str) -> Result<&'a str, IntegrationError> {
        match self.get(key) {
            Some(Value::String(s)) => Ok(s.trim()),
            Some(_) => Err(self.invalid(key, "a string")),
            None => Err(self.missing(key)),
        }
    }

    /// Optional string (blank → `None`); non-strings are an error.
    pub fn opt_str(&self, key: &str) -> Result<Option<&'a str>, IntegrationError> {
        match self.get(key) {
            Some(Value::String(s)) => Ok(Some(s.trim())),
            Some(_) => Err(self.invalid(key, "a string")),
            None => Ok(None),
        }
    }

    /// String with a default.
    pub fn string_or(&self, key: &str, default: &str) -> Result<String, IntegrationError> {
        Ok(self.opt_str(key)?.unwrap_or(default).to_owned())
    }

    /// Required secret string.
    pub fn secret(&self, key: &str) -> Result<SecretString, IntegrationError> {
        self.str(key).map(SecretString::from)
    }

    /// Optional secret string.
    pub fn opt_secret(&self, key: &str) -> Result<Option<SecretString>, IntegrationError> {
        Ok(self.opt_str(key)?.map(SecretString::from))
    }

    /// Boolean (`true/false`, `1/0`, `"yes"/"no"`, `"on"/"off"`) with a default.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool, IntegrationError> {
        match self.get(key) {
            Some(v) => parse_bool(v).ok_or_else(|| self.invalid(key, "a boolean")),
            None => Ok(default),
        }
    }

    /// Optional non-negative integer (number or numeric string).
    pub fn opt_u64(&self, key: &str) -> Result<Option<u64>, IntegrationError> {
        match self.get(key) {
            Some(Value::Number(n)) => n
                .as_u64()
                .map(Some)
                .ok_or_else(|| self.invalid(key, "a non-negative integer")),
            Some(Value::String(s)) => s
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| self.invalid(key, "a non-negative integer")),
            Some(_) => Err(self.invalid(key, "a non-negative integer")),
            None => Ok(None),
        }
    }

    /// Required non-negative integer.
    pub fn u64(&self, key: &str) -> Result<u64, IntegrationError> {
        self.opt_u64(key)?.ok_or_else(|| self.missing(key))
    }

    /// Non-negative integer with a default.
    pub fn u64_or(&self, key: &str, default: u64) -> Result<u64, IntegrationError> {
        Ok(self.opt_u64(key)?.unwrap_or(default))
    }

    /// Required `http(s)` URL.
    pub fn url(&self, key: &str) -> Result<url::Url, IntegrationError> {
        let s = self.str(key)?;
        parse_http_url(s).ok_or_else(|| self.invalid(key, "an http(s) URL"))
    }

    /// Optional `http(s)` URL.
    pub fn opt_url(&self, key: &str) -> Result<Option<url::Url>, IntegrationError> {
        match self.opt_str(key)? {
            Some(s) => parse_http_url(s)
                .map(Some)
                .ok_or_else(|| self.invalid(key, "an http(s) URL")),
            None => Ok(None),
        }
    }

    /// Deserializes the value at `key` (or `None` when blank).
    pub fn opt_json<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, IntegrationError> {
        self.get(key)
            .map(|v| T::deserialize(v).map_err(|_| self.invalid(key, "well-formed")))
            .transpose()
    }
}

// ---------------------------------------------------------------------------
// AnyIntegration
// ---------------------------------------------------------------------------

/// A built integration of any category — what `hos-web` keeps in its registry
/// after calling the right crate's `build()`.
#[derive(Clone)]
pub enum AnyIntegration {
    /// Virtualization.
    Virt(Arc<dyn VirtProvider>),
    /// Hosting panel.
    Panel(Arc<dyn HostingPanel>),
    /// Domain registrar.
    Registrar(Arc<dyn DomainRegistrar>),
    /// Payment gateway.
    Gateway(Arc<dyn PaymentGateway>),
    /// Mailer.
    Mail(Arc<dyn Mailer>),
    /// AI provider.
    Ai(Arc<dyn AiProvider>),
    /// e-invoice provider (e-Fatura / e-Arşiv).
    Einvoice(Arc<dyn EInvoiceProvider>),
}

impl AnyIntegration {
    /// Category of the wrapped integration.
    pub fn category(&self) -> IntegrationCategory {
        match self {
            Self::Virt(_) => IntegrationCategory::Virt,
            Self::Panel(_) => IntegrationCategory::Panel,
            Self::Registrar(_) => IntegrationCategory::Registrar,
            Self::Gateway(_) => IntegrationCategory::Gateway,
            Self::Mail(_) => IntegrationCategory::Mail,
            Self::Ai(_) => IntegrationCategory::Ai,
            Self::Einvoice(_) => IntegrationCategory::Einvoice,
        }
    }

    /// `kind` of the wrapped integration.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Virt(i) => i.kind(),
            Self::Panel(i) => i.kind(),
            Self::Registrar(i) => i.kind(),
            Self::Gateway(i) => i.kind(),
            Self::Mail(i) => i.kind(),
            Self::Ai(i) => i.kind(),
            Self::Einvoice(i) => i.kind(),
        }
    }

    /// Runs the category's `test_connection`.
    pub async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        match self {
            Self::Virt(i) => i.test_connection().await,
            Self::Panel(i) => i.test_connection().await,
            Self::Registrar(i) => i.test_connection().await,
            Self::Gateway(i) => i.test_connection().await,
            Self::Mail(i) => i.test_connection().await,
            Self::Ai(i) => i.test_connection().await,
            Self::Einvoice(i) => i.test_connection().await,
        }
    }
}

impl fmt::Debug for AnyIntegration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnyIntegration({}:{})", self.category(), self.kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor() -> IntegrationDescriptor {
        IntegrationDescriptor::new(kinds::PROXMOX, "Proxmox VE", IntegrationCategory::Virt)
            .docs_url("https://pve.proxmox.com/pve-docs/api-viewer/")
            .config_field(FieldSpec::url("api_url", "API URL").required())
            .config_field(FieldSpec::number("port", "Port").default_value(8006))
            .config_field(FieldSpec::boolean("verify_tls", "Verify TLS").default_value(true))
            .config_field(FieldSpec::select(
                "auth",
                "Auth",
                vec![
                    SelectOption::new("token", "API token"),
                    SelectOption::new("ticket", "Ticket"),
                ],
            ))
            .secret_field(FieldSpec::password("token_secret", "Token secret").required())
    }

    #[test]
    fn descriptor_validation() {
        let d = descriptor();
        let ok_config = json!({"api_url": "https://pve:8006", "port": "8006", "verify_tls": "on", "auth": "token"});
        let secrets = json!({"token_secret": "x"});
        assert!(d.validate(&ok_config, &secrets).is_ok());

        let missing = d.validate(&json!({}), &secrets).unwrap_err();
        assert_eq!(
            missing,
            IntegrationError::Config("config.api_url is required".into())
        );
        let no_secret = d
            .validate(&ok_config, &json!({"token_secret": "  "}))
            .unwrap_err();
        assert_eq!(
            no_secret,
            IntegrationError::Config("secrets.token_secret is required".into())
        );
        assert!(
            d.validate(&json!({"api_url": "ftp://x"}), &secrets)
                .is_err()
        );
        assert!(
            d.validate(&json!({"api_url": "https://x", "port": "abc"}), &secrets)
                .is_err()
        );
        assert!(
            d.validate(&json!({"api_url": "https://x", "auth": "nope"}), &secrets)
                .is_err()
        );
        assert!(d.validate(&json!([1]), &secrets).is_err());
    }

    #[test]
    fn text_areas_accept_structured_json() {
        let d = IntegrationDescriptor::new("t", "T", IntegrationCategory::Panel)
            .config_field(FieldSpec::text_area("packages", "Packages"));
        let none = json!({});
        for ok in [json!("[]"), json!([{"id": "a"}]), json!({"a": 1})] {
            assert!(d.validate(&json!({"packages": ok}), &none).is_ok());
        }
        for bad in [json!(5), json!(true)] {
            assert!(d.validate(&json!({"packages": bad}), &none).is_err());
        }
    }

    #[test]
    fn descriptor_serializes_for_admin_form() {
        let v = serde_json::to_value(descriptor()).unwrap();
        assert_eq!(v["category"], "virt");
        assert_eq!(v["config_fields"][0]["kind"]["type"], "url");
        assert_eq!(
            v["config_fields"][3]["kind"]["options"][0]["value"],
            "token"
        );
        assert_eq!(v["config_fields"][1]["default"], 8006);
    }

    #[test]
    fn field_reader() {
        let cfg = json!({
            "host": " h.example.com ", "port": 8006, "port_s": "443", "neg": -1,
            "flag": "off", "blank": "", "url": "https://api.example.com/v1", "obj": {"a": 1}
        });
        let r = FieldReader::config(&cfg);
        assert_eq!(r.str("host").unwrap(), "h.example.com");
        assert_eq!(r.u64("port").unwrap(), 8006);
        assert_eq!(r.u64_or("port_s", 1).unwrap(), 443);
        assert_eq!(r.u64_or("absent", 7).unwrap(), 7);
        assert!(r.u64("neg").is_err());
        assert!(!r.bool_or("flag", true).unwrap());
        assert!(r.bool_or("blank", true).unwrap());
        assert_eq!(r.opt_str("blank").unwrap(), None);
        assert!(!r.has("blank"));
        assert_eq!(r.url("url").unwrap().host_str(), Some("api.example.com"));
        assert!(r.url("host").is_err());
        assert_eq!(r.string_or("absent", "d").unwrap(), "d");
        let obj: Option<std::collections::HashMap<String, i32>> = r.opt_json("obj").unwrap();
        assert_eq!(obj.unwrap()["a"], 1);
        let s = FieldReader::secrets(&cfg);
        let err = s.secret("api_key").unwrap_err();
        assert_eq!(
            err,
            IntegrationError::Config("secrets.api_key is required".into())
        );
        assert!(!format!("{:?}", r.secret("host").unwrap()).contains("example"));
    }

    #[test]
    fn phone_formats() {
        let p = PhoneNumber::new("+90", "555 123 45 67");
        assert_eq!(p.to_e164(), "+905551234567");
        assert_eq!(p.to_epp(), "+90.5551234567");
    }
}
