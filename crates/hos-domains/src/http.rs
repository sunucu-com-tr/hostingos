//! Shared HTTP plumbing for the registrar clients.
//!
//! * Every client has a request timeout (and a shorter connect timeout).
//! * `reqwest` errors are mapped to [`IntegrationError`] **without** their URL —
//!   ResellerClub and Namecheap carry the API key in the query string, so a URL
//!   must never reach an error message or a log line.
//! * Provider messages are scrubbed: known secret values (and their
//!   percent-encoded form) are replaced and `key=value` pairs whose key looks
//!   sensitive are masked, as defence in depth.
//! * Logging is `tracing::debug!` with method, host + path, status and latency;
//!   bodies are never logged (they may contain EPP auth codes).
//! * API base URLs and the RDAP bootstrap are tenant-configurable, so every
//!   client enforces the outbound policy ([`hos_core::net::OutboundPolicy`],
//!   `HOS_OUTBOUND_POLICY`): each request URL is checked before sending and
//!   every DNS answer at connect time. Redirects are not followed and bodies
//!   are capped at [`MAX_BODY_BYTES`].

use std::time::{Duration, Instant};

use hos_core::IntegrationError;
use hos_core::net::{BodyError, OutboundPolicy, outbound_error_in, read_body_limited};
use hos_core::secret::SecretString;
use reqwest::header::HeaderMap;
use url::Url;

/// Default per-request timeout for registrar APIs.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for the connect phase.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest response accepted from a registrar / RDAP server (TLD price lists
/// included).
pub const MAX_BODY_BYTES: usize = hos_core::net::MAX_API_BODY;

/// Placeholder written where a secret was removed.
pub const REDACTED: &str = "[REDACTED]";

/// Query/form keys whose values are always masked in text that leaves the crate.
const SENSITIVE_KEYS: &[&str] = &[
    "api-key",
    "apikey",
    "api_key",
    "password",
    "passwd",
    "auth-code",
    "auth_code",
    "authcode",
    "eppcode",
    "token",
    "domsecret",
    "secret",
];

/// User-Agent sent to providers.
pub const USER_AGENT: &str = concat!("HostingOS-hos-domains/", env!("CARGO_PKG_VERSION"));

/// A response that has been fully read.
#[derive(Debug)]
pub struct RawResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderMap,
    /// Body as text (lossy UTF-8).
    pub body: String,
}

/// `reqwest::Client` plus the secrets that must never appear in errors.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    policy: OutboundPolicy,
    provider: &'static str,
    secrets: Vec<String>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("provider", &self.provider)
            .field("secrets", &self.secrets.len())
            .finish()
    }
}

impl HttpClient {
    /// Builds a client with the given timeout under the outbound `policy`;
    /// `secrets` are scrubbed from every error message produced through this
    /// client.
    pub fn new(
        provider: &'static str,
        timeout: Duration,
        secrets: &[&SecretString],
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        let timeout = if timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            timeout
        };
        let inner = policy
            .client_builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(CONNECT_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| IntegrationError::Config(format!("cannot build HTTP client: {e}")))?;
        let secrets = secrets
            .iter()
            .map(|s| s.expose_str().to_owned())
            .filter(|s| s.len() >= 4)
            .collect();
        Ok(Self {
            inner,
            policy,
            provider,
            secrets,
        })
    }

    /// Static policy check of a configured endpoint (fails early with a
    /// configuration error instead of on the first request).
    pub fn check_endpoint(&self, url: &Url) -> Result<(), IntegrationError> {
        Ok(self.policy.check_url(url)?)
    }

    /// The underlying `reqwest` client (to build requests).
    pub fn client(&self) -> &reqwest::Client {
        &self.inner
    }

    /// Provider name used in logs.
    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// Removes secrets from `text` (see the [module docs](self)).
    pub fn scrub(&self, text: &str) -> String {
        scrub_text(text, self.secrets.iter().map(String::as_str))
    }

    /// Like [`scrub`](Self::scrub) with additional per-call secrets (auth codes).
    pub fn scrub_with(&self, text: &str, extra: &[&str]) -> String {
        scrub_text(
            text,
            self.secrets
                .iter()
                .map(String::as_str)
                .chain(extra.iter().copied()),
        )
    }

    /// Maps a transport-level `reqwest` error, never including the URL.
    pub fn map_error(&self, err: reqwest::Error) -> IntegrationError {
        if let Some(refused) = outbound_error_in(&err) {
            return refused.clone().into();
        }
        if err.is_timeout() {
            return IntegrationError::Timeout;
        }
        let kind = if err.is_connect() {
            "connection failed"
        } else if err.is_body() || err.is_decode() {
            "invalid response body"
        } else if err.is_builder() {
            "invalid request"
        } else {
            "request failed"
        };
        let err = err.without_url();
        let mut msg = format!("{}: {kind}", self.provider);
        // The source chain carries the useful hint ("dns error", "connection refused").
        let mut source = std::error::Error::source(&err);
        let mut depth = 0;
        while let Some(s) = source {
            msg.push_str(": ");
            msg.push_str(&s.to_string());
            source = s.source();
            depth += 1;
            if depth >= 4 {
                break;
            }
        }
        IntegrationError::Transport(self.scrub(&msg))
    }

    /// Sends `request`, reads the whole body and logs a one-line summary.
    /// Any HTTP status is returned as `Ok`; callers decide what is an error.
    pub async fn execute(
        &self,
        request: reqwest::RequestBuilder,
        operation: &str,
    ) -> Result<RawResponse, IntegrationError> {
        let request = request.build().map_err(|e| self.map_error(e))?;
        // IP-literal hosts never reach the policy's DNS resolver.
        self.policy.check_url(request.url())?;
        let method = request.method().clone();
        let target = mask_url(request.url());
        let started = Instant::now();
        let response = match self.inner.execute(request).await {
            Ok(r) => r,
            Err(e) => {
                let err = self.map_error(e);
                tracing::warn!(
                    provider = self.provider,
                    operation,
                    %method,
                    target = %target,
                    error = %err,
                    "registrar request failed"
                );
                return Err(err);
            }
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = read_body_limited(response, MAX_BODY_BYTES)
            .await
            .map_err(|e| match e {
                BodyError::TooLarge { limit } => IntegrationError::Other(format!(
                    "{}: response body exceeds {} MiB",
                    self.provider,
                    limit / (1024 * 1024)
                )),
                BodyError::Transport(e) => self.map_error(e),
            })?;
        let body = String::from_utf8_lossy(&bytes).into_owned();
        tracing::debug!(
            provider = self.provider,
            operation,
            %method,
            target = %target,
            status,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "registrar request"
        );
        Ok(RawResponse {
            status,
            headers,
            body,
        })
    }
}

/// `scheme://host[:port]/path` with the query string replaced by `?…` — safe for
/// logs and error messages.
pub fn mask_url(url: &Url) -> String {
    let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?"));
    if let Some(port) = url.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(url.path());
    if url.query().is_some() {
        out.push_str("?…");
    }
    out
}

/// One-line description of an unparsable response body for error messages.
/// It never quotes the body, so a misconfigured (or malicious) endpoint cannot
/// read internal pages back through error messages.
pub fn describe_body(body: &str) -> String {
    let trimmed = body.trim();
    let head: String = trimmed
        .chars()
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    if trimmed.is_empty() {
        "empty response".into()
    } else if head.starts_with("<!doctype html") || head.starts_with("<html") {
        "an HTML page".into()
    } else {
        format!("a {}-byte non-API response", trimmed.len())
    }
}

/// Parses `Retry-After` (seconds form).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Generic mapping of a non-success HTTP status (used when the body has no
/// provider-specific error structure).
pub fn status_error(
    status: u16,
    headers: &HeaderMap,
    message: impl Into<String>,
) -> IntegrationError {
    if status == 429 {
        return IntegrationError::RateLimited {
            retry_after: retry_after(headers),
        };
    }
    let mut message = message.into();
    if message.trim().is_empty() {
        message = format!("HTTP {status}");
    }
    IntegrationError::from_http_status(status, truncate(&message, 300))
}

/// Truncates on a char boundary, appending `…` when shortened.
pub fn truncate(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn percent_encode_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn scrub_text<'a>(text: &str, secrets: impl Iterator<Item = &'a str>) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if secret.len() < 4 {
            continue;
        }
        if out.contains(secret) {
            out = out.replace(secret, REDACTED);
        }
        let encoded = percent_encode_component(secret);
        if encoded != secret && out.contains(&encoded) {
            out = out.replace(&encoded, REDACTED);
        }
    }
    mask_sensitive_pairs(&out)
}

/// Masks `key=value` occurrences for [`SENSITIVE_KEYS`] (case-insensitive).
fn mask_sensitive_pairs(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let mut matched = None;
        for key in SENSITIVE_KEYS {
            let pat_len = key.len() + 1;
            if lower[i..].starts_with(key)
                && lower.as_bytes().get(i + key.len()) == Some(&b'=')
                && (i == 0 || !is_key_char(bytes[i - 1]))
            {
                matched = Some(pat_len);
                break;
            }
        }
        if let Some(pat_len) = matched {
            out.push_str(&text[i..i + pat_len]);
            let mut j = i + pat_len;
            while j < bytes.len()
                && !matches!(
                    bytes[j],
                    b'&' | b' ' | b'"' | b'\'' | b')' | b'\n' | b'\r' | b'\t' | b',' | b';' | b'<'
                )
            {
                j += 1;
            }
            if j > i + pat_len {
                out.push_str(REDACTED);
            }
            i = j;
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn is_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_secret_values_and_pairs() {
        let key = SecretString::from("s3cr3t-KEY+/=");
        let client = HttpClient::new(
            "test",
            Duration::from_secs(5),
            &[&key],
            OutboundPolicy::Private,
        )
        .unwrap();
        let msg = "failed GET https://x/api?auth-userid=1&api-key=s3cr3t-KEY%2B%2F%3D&a=b raw s3cr3t-KEY+/=";
        let scrubbed = client.scrub(msg);
        assert!(!scrubbed.contains("s3cr3t"), "{scrubbed}");
        assert!(scrubbed.contains("api-key=[REDACTED]"));
        assert!(scrubbed.contains("auth-userid=1"));
        assert_eq!(
            client.scrub_with("EPPCode=abc123&x", &["abc123"]),
            "EPPCode=[REDACTED]&x"
        );
        assert_eq!(
            mask_sensitive_pairs("ApiKey=zzz&UserName=u"),
            "ApiKey=[REDACTED]&UserName=u"
        );
        assert_eq!(mask_sensitive_pairs("myapikey=zzz"), "myapikey=zzz");
        assert_eq!(
            mask_sensitive_pairs("şifre password=ğğ x"),
            "şifre password=[REDACTED] x"
        );
    }

    #[test]
    fn masks_urls() {
        let url = Url::parse("https://httpapi.com:8443/api/x.json?api-key=secret").unwrap();
        assert_eq!(mask_url(&url), "https://httpapi.com:8443/api/x.json?…");
    }

    #[test]
    fn status_mapping() {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "12".parse().unwrap());
        assert_eq!(
            status_error(429, &h, ""),
            IntegrationError::RateLimited {
                retry_after: Some(Duration::from_secs(12))
            }
        );
        assert_eq!(status_error(401, &h, "no").kind(), "auth");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
