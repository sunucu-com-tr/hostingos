//! Error types.
//!
//! * [`IntegrationError`] — returned by **every** integration trait method
//!   (virt, panel, registrar, gateway, mail, ai). Library crates map their HTTP
//!   client / parse errors into it; [`IntegrationError::is_retryable`] drives the
//!   job queue's retry policy.
//! * [`CoreError`] — application-level error used by repositories, billing and
//!   web handlers. [`CoreError::http_status`] gives the canonical HTTP mapping.
//! * [`ParseEnumError`] — failure to parse a [`string_enum!`](crate::string_enum).
//!
//! Module-specific errors live next to their types:
//! [`MoneyError`], [`CryptoError`], [`ConfigError`], [`ParseIdError`].

use std::time::Duration;

use crate::config::ConfigError;
use crate::crypto::CryptoError;
use crate::ids::ParseIdError;
use crate::money::MoneyError;

/// Shorthand for `Result<T, IntegrationError>`.
pub type IntegrationResult<T> = Result<T, IntegrationError>;

/// Shorthand for `Result<T, CoreError>`.
pub type CoreResult<T> = Result<T, CoreError>;

/// Error returned by integration (provider) implementations.
///
/// Messages must never contain secrets (API keys, passwords, auth codes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntegrationError {
    /// Credentials were rejected (HTTP 401/403, invalid API key, IP not whitelisted).
    #[error("authentication with the provider failed: {0}")]
    Auth(String),
    /// The referenced remote object (VM, account, domain, transaction) does not exist.
    #[error("not found at provider: {0}")]
    NotFound(String),
    /// The remote object already exists or is in a conflicting state
    /// (username taken, domain already registered, VM locked).
    #[error("conflict at provider: {0}")]
    Conflict(String),
    /// The provider throttled us. `retry_after` comes from `Retry-After` or similar.
    #[error("rate limited by provider{}", fmt_retry_after(.retry_after))]
    RateLimited {
        /// How long to wait before retrying, when the provider said so.
        retry_after: Option<Duration>,
    },
    /// The provider (or this implementation) does not support the operation.
    /// The payload names the operation, e.g. `"list_dns_records"`.
    #[error("operation not supported by this provider: {0}")]
    Unsupported(&'static str),
    /// The request was invalid before or after reaching the provider
    /// (bad hostname, unsupported TLD, amount below minimum, ...).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The provider answered with an error that maps to none of the above.
    #[error("provider error{}{}: {message}", fmt_status(.status), fmt_code(.code))]
    Remote {
        /// HTTP status when the error came from an HTTP response.
        status: Option<u16>,
        /// Provider-specific error code (e.g. Namecheap `2019166`, Stripe `card_declined`).
        code: Option<String>,
        /// Human readable message from the provider.
        message: String,
    },
    /// Network / TLS / connection failure, or an unparsable response body.
    #[error("transport error: {0}")]
    Transport(String),
    /// The request timed out.
    #[error("request to provider timed out")]
    Timeout,
    /// The integration's `config`/`secrets` are missing or malformed.
    #[error("integration misconfigured: {0}")]
    Config(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Longest provider message kept in [`IntegrationError::summary`].
pub const SUMMARY_DETAIL_CHARS: usize = 160;

/// `text` on a single line: control characters become spaces, whitespace
/// runs collapse, and it is cut to at most `max` characters (`…` marks a cut).
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match flat.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &flat[..idx]),
        None => flat,
    }
}

fn fmt_retry_after(d: &Option<Duration>) -> String {
    d.map(|d| format!(" (retry after {}s)", d.as_secs()))
        .unwrap_or_default()
}

fn fmt_status(s: &Option<u16>) -> String {
    s.map(|s| format!(" (HTTP {s})")).unwrap_or_default()
}

fn fmt_code(c: &Option<String>) -> String {
    c.as_ref().map(|c| format!(" [{c}]")).unwrap_or_default()
}

impl IntegrationError {
    /// Builds [`IntegrationError::Remote`] without a provider error code.
    pub fn remote(status: Option<u16>, message: impl Into<String>) -> Self {
        Self::Remote {
            status,
            code: None,
            message: message.into(),
        }
    }

    /// Builds [`IntegrationError::Remote`] with a provider error code.
    pub fn remote_with_code(
        status: Option<u16>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Remote {
            status,
            code: Some(code.into()),
            message: message.into(),
        }
    }

    /// Maps a bare HTTP status + body message to the most specific variant
    /// (401/403 → `Auth`, 404 → `NotFound`, 409 → `Conflict`, 429 → `RateLimited`,
    /// 400/422 → `InvalidInput`, 408/504 → `Timeout`, everything else → `Remote`).
    pub fn from_http_status(status: u16, message: impl Into<String>) -> Self {
        let message = message.into();
        match status {
            401 | 403 => Self::Auth(message),
            404 => Self::NotFound(message),
            409 => Self::Conflict(message),
            429 => Self::RateLimited { retry_after: None },
            400 | 422 => Self::InvalidInput(message),
            408 | 504 => Self::Timeout,
            _ => Self::remote(Some(status), message),
        }
    }

    /// Whether retrying the same request later may succeed.
    ///
    /// `true` for `RateLimited`, `Transport`, `Timeout` and `Remote` errors with
    /// a 5xx status, 408 or 429; `false` otherwise (including `Remote` without a
    /// status — provider-level business errors are not transient).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport(_) | Self::Timeout => true,
            Self::Remote {
                status: Some(s), ..
            } => *s >= 500 || *s == 408 || *s == 429,
            _ => false,
        }
    }

    /// Suggested delay before retrying, when the provider gave one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// Classified, admin-facing one-line summary of a failed integration call:
    /// error category, HTTP status and provider code when known, and a short
    /// single-line provider message (at most [`SUMMARY_DETAIL_CHARS`]
    /// characters, control characters removed). This — never the raw
    /// [`Display`](std::fmt::Display) text — is what connection tests store and
    /// show. The integration crates only put messages parsed from a provider's
    /// API error format (or their own text) into errors, never raw response
    /// bodies (a tenant-configured endpoint must not be usable to read
    /// internal pages back); the cut here is a second line of defence.
    pub fn summary(&self) -> String {
        let (label, detail): (&str, Option<&str>) = match self {
            Self::Auth(m) => ("authentication failed", Some(m)),
            Self::NotFound(m) => ("not found", Some(m)),
            Self::Conflict(m) => ("conflict", Some(m)),
            Self::RateLimited { .. } => ("rate limited by the provider", None),
            Self::Unsupported(m) => ("not supported", Some(m)),
            Self::InvalidInput(m) => ("request rejected", Some(m)),
            Self::Remote { message, .. } => ("provider error", Some(message)),
            Self::Transport(m) => ("connection failed", Some(m)),
            Self::Timeout => ("timed out", None),
            Self::Config(m) => ("configuration error", Some(m)),
            Self::Other(m) => ("error", Some(m)),
        };
        let mut out = label.to_owned();
        if let Self::Remote { status, code, .. } = self {
            if let Some(status) = status {
                out.push_str(&format!(" (HTTP {status})"));
            }
            if let Some(code) = code {
                out.push_str(&format!(" [{}]", one_line(code, 40)));
            }
        }
        if let Some(detail) = detail.map(|d| one_line(d, SUMMARY_DETAIL_CHARS))
            && !detail.is_empty()
        {
            out.push_str(": ");
            out.push_str(&detail);
        }
        out
    }

    /// Stable machine-readable name of the variant (for logs, `service_events`, metrics).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Auth(_) => "auth",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::RateLimited { .. } => "rate_limited",
            Self::Unsupported(_) => "unsupported",
            Self::InvalidInput(_) => "invalid_input",
            Self::Remote { .. } => "remote",
            Self::Transport(_) => "transport",
            Self::Timeout => "timeout",
            Self::Config(_) => "config",
            Self::Other(_) => "other",
        }
    }
}

/// A string could not be parsed into a [`string_enum!`](crate::string_enum) type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {type_name} value: {value:?}")]
pub struct ParseEnumError {
    /// Name of the target enum.
    pub type_name: &'static str,
    /// The rejected input (truncated to 64 chars).
    pub value: String,
}

impl ParseEnumError {
    /// Creates the error; long inputs are truncated.
    pub fn new(type_name: &'static str, value: &str) -> Self {
        Self {
            type_name,
            value: value.chars().take(64).collect(),
        }
    }
}

/// Application-level error shared by repositories, services and handlers.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// User input failed validation. Message is safe to show to the user.
    #[error("validation failed: {0}")]
    Validation(String),
    /// An entity does not exist (or is not visible to this tenant).
    #[error("{entity} not found: {id}")]
    NotFound {
        /// Entity name, e.g. `"invoice"`.
        entity: &'static str,
        /// Identifier that was looked up.
        id: String,
    },
    /// Uniqueness or concurrency conflict (duplicate email, stale update).
    #[error("conflict: {0}")]
    Conflict(String),
    /// No authenticated principal.
    #[error("authentication required")]
    Unauthorized,
    /// Authenticated but not allowed.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// A state machine refused a transition (see `domain::ServiceStatus::transition`).
    #[error("invalid {entity} status transition: {from} -> {to}")]
    InvalidTransition {
        /// Entity name, e.g. `"service"`.
        entity: &'static str,
        /// Current status.
        from: &'static str,
        /// Requested status.
        to: &'static str,
    },
    /// Money arithmetic / currency error.
    #[error(transparent)]
    Money(#[from] MoneyError),
    /// Encryption / decoding error.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// Configuration error.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// An integration call failed.
    #[error(transparent)]
    Integration(#[from] IntegrationError),
    /// A string enum failed to parse (usually corrupt DB data or bad input).
    #[error(transparent)]
    ParseEnum(#[from] ParseEnumError),
    /// An id failed to parse.
    #[error(transparent)]
    ParseId(#[from] ParseIdError),
    /// Database failure. With the `sqlx` feature, `From<sqlx::Error>` maps
    /// `RowNotFound` to [`CoreError::NotFound`] and everything else here.
    #[error("database error: {0}")]
    Database(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Unexpected internal failure. Message is for logs, not for users.
    #[error("internal error: {0}")]
    Internal(String),
}

impl CoreError {
    /// Shorthand for [`CoreError::NotFound`].
    pub fn not_found(entity: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            entity,
            id: id.to_string(),
        }
    }

    /// Shorthand for [`CoreError::Validation`].
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    /// Canonical HTTP status for this error (used by `hos-web`).
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Validation(_) | Self::ParseEnum(_) | Self::ParseId(_) => 400,
            Self::Money(MoneyError::CurrencyMismatch { .. }) => 400,
            Self::Unauthorized => 401,
            Self::Forbidden(_) => 403,
            Self::NotFound { .. } => 404,
            Self::Conflict(_) | Self::InvalidTransition { .. } => 409,
            Self::Integration(IntegrationError::InvalidInput(_)) => 422,
            Self::Integration(IntegrationError::RateLimited { .. }) => 503,
            Self::Integration(IntegrationError::Timeout) => 504,
            Self::Integration(_) => 502,
            Self::Money(_)
            | Self::Crypto(_)
            | Self::Config(_)
            | Self::Database(_)
            | Self::Internal(_) => 500,
        }
    }

    /// Whether the message may be shown to end users verbatim.
    pub fn is_user_facing(&self) -> bool {
        self.http_status() < 500
    }

    /// [`IntegrationError::summary`] for an integration call that went
    /// through a service layer: integration errors are classified, other
    /// user-facing errors are cut to one line, anything else (database,
    /// crypto, internal) becomes a generic text — callers log the details.
    pub fn integration_summary(&self) -> String {
        match self {
            Self::Integration(e) => e.summary(),
            other if other.is_user_facing() => one_line(&other.to_string(), SUMMARY_DETAIL_CHARS),
            _ => "internal error (details in the server log)".into(),
        }
    }
}

#[cfg(feature = "sqlx")]
impl From<sqlx::Error> for CoreError {
    fn from(err: sqlx::Error) -> Self {
        match err {
            sqlx::Error::RowNotFound => Self::NotFound {
                entity: "row",
                id: String::new(),
            },
            other => Self::Database(Box::new(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(IntegrationError::Timeout.is_retryable());
        assert!(IntegrationError::Transport("reset".into()).is_retryable());
        assert!(
            IntegrationError::RateLimited {
                retry_after: Some(Duration::from_secs(3))
            }
            .is_retryable()
        );
        assert!(IntegrationError::remote(Some(503), "down").is_retryable());
        assert!(!IntegrationError::remote(Some(400), "bad").is_retryable());
        assert!(!IntegrationError::remote(None, "biz").is_retryable());
        assert!(!IntegrationError::Auth("x".into()).is_retryable());
        assert!(!IntegrationError::Unsupported("dns").is_retryable());
    }

    #[test]
    fn from_http_status_maps() {
        assert_eq!(IntegrationError::from_http_status(401, "x").kind(), "auth");
        assert_eq!(
            IntegrationError::from_http_status(404, "x").kind(),
            "not_found"
        );
        assert_eq!(
            IntegrationError::from_http_status(500, "x"),
            IntegrationError::remote(Some(500), "x")
        );
    }

    #[test]
    fn summaries_are_classified_single_line_and_short() {
        let e = IntegrationError::remote_with_code(Some(502), "bad_gateway", "upstream down");
        assert_eq!(
            e.summary(),
            "provider error (HTTP 502) [bad_gateway]: upstream down"
        );
        assert_eq!(IntegrationError::Timeout.summary(), "timed out");
        let long = format!("line1\r\n<html>{}</html>", "x".repeat(5000));
        let text = IntegrationError::Transport(long).summary();
        assert!(
            text.starts_with("connection failed: line1 <html>"),
            "{text}"
        );
        assert!(!text.contains('\n') && !text.contains('\r'));
        assert!(text.chars().count() <= "connection failed: ".len() + SUMMARY_DETAIL_CHARS + 1);
        assert_eq!(one_line("a\tb\n\n c", 10), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc…");

        let core = CoreError::Integration(IntegrationError::Auth("token rejected".into()));
        assert_eq!(
            core.integration_summary(),
            "authentication failed: token rejected"
        );
        assert_eq!(
            CoreError::validation("integration X is disabled").integration_summary(),
            "validation failed: integration X is disabled"
        );
        assert_eq!(
            CoreError::Internal("db password=hunter2".into()).integration_summary(),
            "internal error (details in the server log)"
        );
    }

    #[test]
    fn display_formats() {
        let e = IntegrationError::remote_with_code(Some(502), "E1", "boom");
        assert_eq!(e.to_string(), "provider error (HTTP 502) [E1]: boom");
        let e = IntegrationError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(e.to_string(), "rate limited by provider (retry after 7s)");
    }

    #[test]
    fn core_error_status() {
        assert_eq!(CoreError::not_found("invoice", "x").http_status(), 404);
        assert_eq!(
            CoreError::from(IntegrationError::Timeout).http_status(),
            504
        );
        assert!(CoreError::validation("x").is_user_facing());
        assert!(!CoreError::Internal("x".into()).is_user_facing());
    }
}
