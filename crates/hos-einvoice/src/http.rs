//! Guarded HTTP client shared by the providers.
//!
//! * Built from [`hos_core::net::OutboundPolicy::client_builder`]: no
//!   redirects, no proxy, every DNS answer checked against the policy, and
//!   each request URL checked before sending.
//! * Request timeout plus a shorter connect timeout; response bodies capped
//!   ([`MAX_API_BODY`] for API calls, [`MAX_DOCUMENT_BODY`] for PDF / XML).
//! * Errors never contain the URL, the request body or a response body
//!   verbatim: provider messages are scrubbed of known secrets, and tax ids
//!   (10 / 11 digit runs) are masked before they reach an error string or a
//!   log line. Logging is `tracing::debug!` with method, host + path, status
//!   and latency only.

use std::time::{Duration, Instant};

use hos_core::IntegrationError;
use hos_core::net::{BodyError, OutboundPolicy, outbound_error_in, read_body_limited};
use hos_core::secret::SecretString;
use reqwest::header::HeaderMap;
use url::Url;

/// Largest JSON / SOAP response accepted.
pub const MAX_API_BODY: usize = 4 * 1024 * 1024;
/// Largest PDF / UBL document accepted.
pub const MAX_DOCUMENT_BODY: usize = 12 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const REDACTED: &str = "[REDACTED]";
const USER_AGENT: &str = concat!("HostingOS-hos-einvoice/", env!("CARGO_PKG_VERSION"));

/// A fully read response.
#[derive(Debug)]
pub struct RawResponse {
    /// HTTP status.
    pub status: u16,
    /// Headers.
    pub headers: HeaderMap,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl RawResponse {
    /// Body as (lossy) text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// 2xx.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// `reqwest::Client` under the outbound policy, plus scrubbing.
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
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// New client; `secrets` are removed from every error message.
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
        Ok(Self {
            inner,
            policy,
            provider,
            secrets: secrets
                .iter()
                .map(|s| s.expose_str().to_owned())
                .filter(|s| s.len() >= 4)
                .collect(),
        })
    }

    /// Fails early when a configured endpoint violates the policy.
    pub fn check_endpoint(&self, url: &Url) -> Result<(), IntegrationError> {
        Ok(self.policy.check_url(url)?)
    }

    /// The `reqwest` client to build requests with.
    pub fn client(&self) -> &reqwest::Client {
        &self.inner
    }

    /// Removes secrets and tax ids from `text` and cuts it to 300 chars.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for s in &self.secrets {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), REDACTED);
            }
        }
        truncate(&mask_tax_ids(&out), 300)
    }

    fn map_error(&self, err: reqwest::Error) -> IntegrationError {
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

    /// Sends `request` and reads at most `limit` bytes of the body. Every
    /// HTTP status is returned as `Ok`.
    pub async fn execute(
        &self,
        request: reqwest::RequestBuilder,
        operation: &'static str,
        limit: usize,
    ) -> Result<RawResponse, IntegrationError> {
        let request = request.build().map_err(|e| self.map_error(e))?;
        self.policy.check_url(request.url())?;
        let method = request.method().clone();
        let target = mask_url(request.url());
        let started = Instant::now();
        let response = match self.inner.execute(request).await {
            Ok(r) => r,
            Err(e) => {
                let err = self.map_error(e);
                tracing::warn!(provider = self.provider, operation, %method, target = %target, error = %err, "e-invoice request failed");
                return Err(err);
            }
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = read_body_limited(response, limit)
            .await
            .map_err(|e| match e {
                BodyError::TooLarge { limit } => IntegrationError::Other(format!(
                    "{}: response body exceeds {} MiB",
                    self.provider,
                    limit / (1024 * 1024)
                )),
                BodyError::Transport(e) => self.map_error(e),
            })?;
        tracing::debug!(
            provider = self.provider,
            operation,
            %method,
            target = %target,
            status,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "e-invoice request"
        );
        Ok(RawResponse {
            status,
            headers,
            body: body.to_vec(),
        })
    }

    /// Maps a non-2xx response with a provider `message` (already extracted
    /// from the body by the caller).
    pub fn status_error(&self, resp: &RawResponse, message: &str) -> IntegrationError {
        if resp.status == 429 {
            return IntegrationError::RateLimited {
                retry_after: resp
                    .headers
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok())
                    .map(Duration::from_secs),
            };
        }
        let message = if message.trim().is_empty() {
            format!("{}: HTTP {}", self.provider, resp.status)
        } else {
            format!("{}: {}", self.provider, self.scrub(message))
        };
        IntegrationError::from_http_status(resp.status, message)
    }
}

/// `scheme://host[:port]/path` with the query replaced by `?…`, and digit
/// runs that look like tax ids masked.
pub fn mask_url(url: &Url) -> String {
    let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?"));
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(&mask_tax_ids(url.path()));
    if url.query().is_some() {
        out.push_str("?…");
    }
    out
}

/// Replaces runs of exactly 10 or 11 digits (VKN / TCKN) with `***`.
pub fn mask_tax_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() == 10 || run.len() == 11 {
            out.push_str("***");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Truncates on a char boundary with `…`.
pub fn truncate(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// One-line description of an unexpected body, never quoting it.
pub fn describe_body(body: &[u8]) -> String {
    let head: String = String::from_utf8_lossy(&body[..body.len().min(64)])
        .trim()
        .to_ascii_lowercase();
    if body.is_empty() {
        "empty response".into()
    } else if head.starts_with("<!doctype html") || head.starts_with("<html") {
        "an HTML page".into()
    } else {
        format!("a {}-byte non-API response", body.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_tax_ids_and_secrets() {
        assert_eq!(
            mask_tax_ids("VKN 1234567890 / 12345678901 / 123"),
            "VKN *** / *** / 123"
        );
        assert_eq!(mask_tax_ids("2026000000042"), "2026000000042");
        let key = SecretString::from("super-secret-key");
        let c = HttpClient::new("t", Duration::ZERO, &[&key], OutboundPolicy::Private).unwrap();
        assert_eq!(
            c.scrub("bad key super-secret-key for 1234567890"),
            "bad key [REDACTED] for ***"
        );
        let url =
            Url::parse("https://api.example.test/general/Check/TaxNumber/1234567890?x=1").unwrap();
        assert_eq!(
            mask_url(&url),
            "https://api.example.test/general/Check/TaxNumber/***?…"
        );
    }
}
