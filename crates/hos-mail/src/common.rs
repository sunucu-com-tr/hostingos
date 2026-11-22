//! Helpers shared by the mailer implementations.

use std::time::Duration;

use hos_core::error::IntegrationError;
use hos_core::integrations::FieldReader;
use hos_core::integrations::mailer::{EmailMessage, Mailbox};
use hos_core::net::{BodyError, OutboundPolicy, outbound_error_in, read_body_limited};
use serde_json::Value;

/// Default HTTP/SMTP timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
const MAX_TIMEOUT_SECS: u64 = 120;
const MAX_ERROR_MESSAGE: usize = 300;
/// Largest response accepted from a mail API (they answer with small JSON).
pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Header names an extra header may not override (the mailer sets them).
const RESERVED_HEADERS: &[&str] = &[
    "from",
    "sender",
    "to",
    "cc",
    "bcc",
    "reply-to",
    "subject",
    "date",
    "message-id",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    "return-path",
];

/// `User-Agent` sent by the HTTP mailers.
pub(crate) const USER_AGENT: &str = concat!("HostingOS-hos-mail/", env!("CARGO_PKG_VERSION"));

/// Basic syntactic check of an email address (no display name).
pub(crate) fn is_plausible_email(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && email.len() <= 254
        && !email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>\"(),;:\\[]".contains(c))
}

/// Reads `from_email` / `from_name` from an integration config.
pub(crate) fn read_sender(
    config: &FieldReader<'_>,
    required: bool,
) -> Result<Option<Mailbox>, IntegrationError> {
    let email = if required {
        Some(config.str("from_email")?)
    } else {
        config.opt_str("from_email")?
    };
    let Some(email) = email else {
        return Ok(None);
    };
    if !is_plausible_email(email) {
        return Err(IntegrationError::Config(
            "config.from_email must be an email address".into(),
        ));
    }
    let mailbox = match config.opt_str("from_name")? {
        Some(name) => Mailbox::named(name, email),
        None => Mailbox::new(email),
    };
    check_mailbox(&mailbox).map_err(|_| {
        IntegrationError::Config("config.from_name must not contain line breaks".into())
    })?;
    Ok(Some(mailbox))
}

/// Reads `timeout_secs` (default 15, clamped to 1..=120).
pub(crate) fn read_timeout(config: &FieldReader<'_>) -> Result<Duration, IntegrationError> {
    let secs = config
        .u64_or("timeout_secs", DEFAULT_TIMEOUT_SECS)?
        .clamp(1, MAX_TIMEOUT_SECS);
    Ok(Duration::from_secs(secs))
}

fn check_mailbox(m: &Mailbox) -> Result<(), IntegrationError> {
    let bad_name = m
        .name
        .as_deref()
        .is_some_and(|n| n.chars().any(|c| c.is_control()));
    if bad_name || !is_plausible_email(&m.email) {
        return Err(IntegrationError::InvalidInput(format!(
            "invalid email address {:?}",
            m.email.chars().take(80).collect::<String>()
        )));
    }
    Ok(())
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 76 && name.bytes().all(|b| b.is_ascii_graphic() && b != b':')
}

/// Runs [`EmailMessage::validate`] plus stricter checks (address syntax, no
/// control characters in display names, valid and non-reserved extra header
/// names) and resolves the sender (`message.from`, else `default_from`).
pub(crate) fn prepare(
    message: &EmailMessage,
    default_from: Option<&Mailbox>,
) -> Result<Mailbox, IntegrationError> {
    message.validate()?;
    let from = message
        .from
        .as_ref()
        .or(default_from)
        .cloned()
        .ok_or_else(|| {
            IntegrationError::InvalidInput("email has no sender and no default sender".into())
        })?;
    for m in std::iter::once(&from)
        .chain(&message.reply_to)
        .chain(&message.to)
        .chain(&message.cc)
        .chain(&message.bcc)
    {
        check_mailbox(m)?;
    }
    for (name, value) in &message.headers {
        if !valid_header_name(name) {
            return Err(IntegrationError::InvalidInput(format!(
                "invalid header name {:?}",
                name.chars().take(40).collect::<String>()
            )));
        }
        if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(IntegrationError::InvalidInput(format!(
                "header {name} is set by the mailer and cannot be overridden"
            )));
        }
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(IntegrationError::InvalidInput(format!(
                "header {name} contains control characters"
            )));
        }
    }
    Ok(from)
}

/// Builds the shared `reqwest` client (timeouts, user agent) enforcing the
/// outbound policy: DNS answers are checked at connect time, redirects are
/// not followed and system proxies are ignored.
pub(crate) fn http_client(
    timeout: Duration,
    policy: OutboundPolicy,
) -> Result<reqwest::Client, IntegrationError> {
    policy
        .client_builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| IntegrationError::Config(format!("cannot build HTTP client: {e}")))
}

/// Builds `request`, checks its URL against `policy` (IP literals never
/// reach the policy's DNS resolver) and sends it.
pub(crate) async fn send(
    client: &reqwest::Client,
    policy: OutboundPolicy,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, IntegrationError> {
    let request = request.build().map_err(map_reqwest)?;
    policy.check_url(request.url())?;
    client.execute(request).await.map_err(map_reqwest)
}

/// Maps a `reqwest` send/body error.
pub(crate) fn map_reqwest(err: reqwest::Error) -> IntegrationError {
    if let Some(refused) = outbound_error_in(&err) {
        return refused.clone().into();
    }
    if err.is_timeout() {
        IntegrationError::Timeout
    } else {
        IntegrationError::Transport(err.without_url().to_string())
    }
}

/// Reads a response body with the [`MAX_BODY_BYTES`] cap.
async fn read_body(resp: reqwest::Response) -> Result<Vec<u8>, IntegrationError> {
    read_body_limited(resp, MAX_BODY_BYTES)
        .await
        .map_err(|e| match e {
            BodyError::TooLarge { limit } => {
                IntegrationError::Other(format!("mail API response exceeds {} KiB", limit / 1024))
            }
            BodyError::Transport(e) => map_reqwest(e),
        })
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > MAX_ERROR_MESSAGE {
        let mut t: String = s.chars().take(MAX_ERROR_MESSAGE).collect();
        t.push('…');
        t
    } else {
        s.to_owned()
    }
}

/// Converts a non-success HTTP response into an [`IntegrationError`].
/// `extract` pulls `(code, message)` out of the provider's JSON error body.
pub(crate) async fn error_from_response(
    resp: reqwest::Response,
    extract: fn(&Value) -> (Option<String>, Option<String>),
) -> IntegrationError {
    let status = resp.status().as_u16();
    if status == 429 {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return IntegrationError::RateLimited { retry_after };
    }
    let body = read_body(resp).await.unwrap_or_default();
    // Only messages from the provider's JSON error format are quoted; any
    // other body (HTML error page, proxy text) is never echoed.
    let (code, message) = match serde_json::from_slice::<Value>(&body) {
        Ok(json) => extract(&json),
        Err(_) => (None, None),
    };
    let message = truncate(&message.unwrap_or_else(|| format!("HTTP {status}")));
    match (
        IntegrationError::from_http_status(status, message.clone()),
        code,
    ) {
        (IntegrationError::Remote { status, .. }, Some(code)) => IntegrationError::Remote {
            status,
            code: Some(code),
            message,
        },
        (IntegrationError::Auth(m), Some(code)) => IntegrationError::Auth(format!("[{code}] {m}")),
        (IntegrationError::InvalidInput(m), Some(code)) => {
            IntegrationError::InvalidInput(format!("[{code}] {m}"))
        }
        (other, _) => other,
    }
}

/// Parses a successful JSON response body.
pub(crate) async fn json_body(resp: reqwest::Response) -> Result<Value, IntegrationError> {
    let body = read_body(resp).await?;
    serde_json::from_slice::<Value>(&body)
        .map_err(|e| IntegrationError::Transport(format!("invalid JSON response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg() -> EmailMessage {
        EmailMessage {
            to: vec![Mailbox::new("to@example.com")],
            subject: "s".into(),
            text_body: Some("t".into()),
            ..EmailMessage::default()
        }
    }

    #[test]
    fn prepare_resolves_sender_and_rejects_bad_input() {
        let def = Mailbox::named("Acme", "noreply@acme.example");
        assert_eq!(prepare(&msg(), Some(&def)).unwrap(), def);
        let mut m = msg();
        m.from = Some(Mailbox::new("me@acme.example"));
        assert_eq!(prepare(&m, Some(&def)).unwrap().email, "me@acme.example");
        assert!(matches!(
            prepare(&msg(), None),
            Err(IntegrationError::InvalidInput(_))
        ));

        let mut m = msg();
        m.to = vec![Mailbox::named("Evil\r\nBcc: x@y.z", "a@example.com")];
        assert!(prepare(&m, Some(&def)).is_err());
        let mut m = msg();
        m.cc = vec![Mailbox::new("not-an-address")];
        assert!(prepare(&m, Some(&def)).is_err());
        let mut m = msg();
        m.headers = vec![("Bcc".into(), "x@example.com".into())];
        assert!(prepare(&m, Some(&def)).is_err());
        let mut m = msg();
        m.headers = vec![("Bad Name".into(), "v".into())];
        assert!(prepare(&m, Some(&def)).is_err());
        let mut m = msg();
        m.headers = vec![("X-Ok".into(), "a\u{0}b".into())];
        assert!(prepare(&m, Some(&def)).is_err());
        let mut m = msg();
        m.headers = vec![("X-HostingOS-Template".into(), "welcome".into())];
        assert!(prepare(&m, Some(&def)).is_ok());
    }

    #[test]
    fn sender_from_config() {
        let cfg = json!({"from_email": "noreply@acme.example", "from_name": "Acme"});
        let r = FieldReader::config(&cfg);
        assert_eq!(
            read_sender(&r, true).unwrap(),
            Some(Mailbox::named("Acme", "noreply@acme.example"))
        );
        let empty = json!({});
        assert!(read_sender(&FieldReader::config(&empty), true).is_err());
        assert_eq!(
            read_sender(&FieldReader::config(&empty), false).unwrap(),
            None
        );
        let bad = json!({"from_email": "nope"});
        assert!(read_sender(&FieldReader::config(&bad), true).is_err());
        let t = json!({"timeout_secs": "999"});
        assert_eq!(
            read_timeout(&FieldReader::config(&t)).unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(
            read_timeout(&FieldReader::config(&empty)).unwrap(),
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
    }

    #[test]
    fn email_plausibility() {
        assert!(is_plausible_email("a.b+c@sub.example.com"));
        for bad in [
            "",
            "a",
            "a@",
            "@b.c",
            "a@b",
            "a b@c.d",
            "a@b.c\r\n",
            "<a@b.c>",
            "a@.b",
        ] {
            assert!(!is_plausible_email(bad), "{bad:?}");
        }
    }
}
