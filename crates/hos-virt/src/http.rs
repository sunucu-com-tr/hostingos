//! Shared HTTP plumbing: client construction, bounded retries, consistent
//! error mapping and secret-safe logging.
//!
//! Rules every provider follows through this module:
//!
//! * every client has a request timeout (at most [`MAX_TIMEOUT_SECS`]; long
//!   operations are asynchronous tasks that are polled) and a connect timeout;
//! * destinations are checked against the outbound policy
//!   ([`hos_core::net::OutboundPolicy`], `HOS_OUTBOUND_POLICY`): the configured
//!   API URL when the provider is built, every request URL before it is sent
//!   and every DNS answer at connect time (so DNS rebinding cannot reach
//!   loopback / metadata addresses);
//! * redirects are **not** followed (auth headers must never be replayed to
//!   another origin; a redirect almost always means a wrong scheme/port);
//! * response bodies are read with a size cap ([`MAX_BODY_BYTES`]);
//! * only requests the caller marks as idempotent are retried, and only on
//!   transport errors, timeouts and the provider's transient statuses;
//! * logs carry method, URL path (never the query string — Virtualizor puts
//!   its API key there), status, attempt and latency — never bodies or headers;
//! * `reqwest` errors are stripped of their URL before they are formatted.

use std::time::{Duration, Instant};

use hos_core::IntegrationError;
use hos_core::net::{BodyError, OutboundPolicy, outbound_error_in, read_body_limited};
use reqwest::header::HeaderMap;
use reqwest::{Client, Request, StatusCode};
use serde::de::DeserializeOwned;

/// `User-Agent` sent to every provider.
pub(crate) const USER_AGENT: &str = concat!("HostingOS-hos-virt/", env!("CARGO_PKG_VERSION"));

/// Longest provider message kept in an error (bodies can be large HTML pages).
const MAX_MESSAGE_LEN: usize = 600;

/// Upper bound of the configurable per-request timeout (`timeout_secs`).
/// Long-running work (clone, create, rebuild) is an asynchronous task that is
/// polled with its own limit (`task_timeout_secs`), never one HTTP call.
pub(crate) const MAX_TIMEOUT_SECS: u64 = 120;

/// Largest response body accepted from a hypervisor API.
pub(crate) const MAX_BODY_BYTES: usize = hos_core::net::MAX_API_BODY;

/// Options for [`build_client`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientOptions {
    /// Whole-request timeout.
    pub timeout: Duration,
    /// Skip TLS certificate verification (self-signed hypervisor certificates).
    pub accept_invalid_certs: bool,
    /// Which destinations may be contacted.
    pub policy: OutboundPolicy,
}

impl ClientOptions {
    /// Reads `timeout_secs` (clamped to 5–[`MAX_TIMEOUT_SECS`] s) and
    /// `skip_tls_verify` from a provider's config.
    pub fn from_config(
        cfg: &hos_core::integrations::FieldReader<'_>,
        default_timeout_secs: u64,
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        let secs = cfg
            .u64_or("timeout_secs", default_timeout_secs)?
            .clamp(5, MAX_TIMEOUT_SECS);
        Ok(Self {
            timeout: Duration::from_secs(secs),
            accept_invalid_certs: cfg.bool_or("skip_tls_verify", false)?,
            policy,
        })
    }
}

/// A `reqwest` client bound to an outbound policy (derefs to the client so
/// requests are built as usual; send them through [`execute`]).
#[derive(Debug, Clone)]
pub(crate) struct HttpClient {
    client: Client,
    policy: OutboundPolicy,
}

impl std::ops::Deref for HttpClient {
    type Target = Client;

    fn deref(&self) -> &Client {
        &self.client
    }
}

/// Builds a policy-enforcing `reqwest` client (timeouts, no redirects, no
/// system proxy, optional acceptance of invalid certificates) after checking
/// the configured API URL `base` against the policy.
pub(crate) fn build_client(
    opts: ClientOptions,
    base: &url::Url,
) -> Result<HttpClient, IntegrationError> {
    opts.policy.check_url(base)?;
    let mut builder = opts
        .policy
        .client_builder()
        .user_agent(USER_AGENT)
        .timeout(opts.timeout)
        .connect_timeout(opts.timeout.min(Duration::from_secs(15)));
    if opts.accept_invalid_certs {
        builder = builder
            .tls_danger_accept_invalid_certs(true)
            .tls_danger_accept_invalid_hostnames(true);
    }
    let client = builder.build().map_err(|err| {
        IntegrationError::Config(format!(
            "cannot build HTTP client: {}",
            error_chain(&err.without_url())
        ))
    })?;
    Ok(HttpClient {
        client,
        policy: opts.policy,
    })
}

/// Retry policy for idempotent requests.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// Total attempts (first try included) for idempotent requests.
    pub max_attempts: u32,
    /// Backoff before the second attempt; doubles afterwards.
    pub base_delay: Duration,
    /// Longest wait we are willing to do inside one call. A `Retry-After`
    /// above this is returned to the caller as `RateLimited` instead.
    pub max_delay: Duration,
    /// HTTP statuses considered transient for this provider.
    pub retry_statuses: &'static [u16],
}

impl RetryPolicy {
    /// Statuses that are transient for well-behaved REST APIs.
    pub const STANDARD_STATUSES: &'static [u16] = &[429, 500, 502, 503, 504];

    /// Default policy: 3 attempts, 250 ms → 500 ms backoff, at most 5 s wait.
    pub const fn standard() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
            retry_statuses: Self::STANDARD_STATUSES,
        }
    }

    /// Same policy with another set of transient statuses.
    pub const fn with_statuses(mut self, statuses: &'static [u16]) -> Self {
        self.retry_statuses = statuses;
        self
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32 << attempt.saturating_sub(1).min(6);
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// A fully read HTTP response.
#[derive(Debug, Clone)]
pub(crate) struct RawResponse {
    /// HTTP status.
    pub status: StatusCode,
    /// Non-canonical HTTP/1 reason phrase, when the server sent one.
    pub reason: Option<String>,
    /// Response headers.
    pub headers: HeaderMap,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl RawResponse {
    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Decodes the body as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, IntegrationError> {
        serde_json::from_slice(&self.body).map_err(|err| {
            IntegrationError::Transport(format!(
                "unexpected response body (HTTP {}): {err}",
                self.status.as_u16()
            ))
        })
    }

    /// Best human-readable status text: the server's reason phrase, else the
    /// canonical one.
    pub fn status_text(&self) -> String {
        match &self.reason {
            Some(reason) if !reason.trim().is_empty() => reason.trim().to_owned(),
            _ => self
                .status
                .canonical_reason()
                .unwrap_or("unknown status")
                .to_owned(),
        }
    }

    /// Delay requested by the server: `Retry-After` (seconds) or a
    /// `RateLimit-Reset` UNIX timestamp (Hetzner).
    pub fn retry_after(&self) -> Option<Duration> {
        let header = |name: &str| {
            self.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
        };
        if let Some(secs) = header("retry-after").and_then(|v| v.parse::<u64>().ok()) {
            return Some(Duration::from_secs(secs));
        }
        let reset = header("ratelimit-reset").and_then(|v| v.parse::<i64>().ok())?;
        let now = chrono::Utc::now().timestamp();
        Some(Duration::from_secs(u64::try_from(reset - now).unwrap_or(0)))
    }

    /// Maps a non-success response to the most specific [`IntegrationError`]
    /// (via [`IntegrationError::from_http_status`]), keeping the provider's
    /// error code and the `Retry-After` delay.
    pub fn status_error(
        &self,
        message: impl Into<String>,
        code: Option<String>,
    ) -> IntegrationError {
        let status = self.status.as_u16();
        let message = truncate(&message.into());
        match IntegrationError::from_http_status(status, message) {
            IntegrationError::RateLimited { .. } => IntegrationError::RateLimited {
                retry_after: self.retry_after(),
            },
            IntegrationError::Remote {
                status, message, ..
            } => IntegrationError::Remote {
                status,
                code,
                message,
            },
            other => other,
        }
    }
}

/// Sends `request`, retrying idempotent requests on transient failures.
///
/// Non-2xx responses are returned as `Ok` — the provider turns them into
/// errors because only it knows its error body format.
pub(crate) async fn execute(
    client: &HttpClient,
    request: Request,
    idempotent: bool,
    policy: &RetryPolicy,
    provider: &'static str,
) -> Result<RawResponse, IntegrationError> {
    // IP-literal hosts never reach the policy's DNS resolver.
    client.policy.check_url(request.url())?;
    let method = request.method().clone();
    let path = request.url().path().to_owned();
    let max_attempts = if idempotent {
        policy.max_attempts.max(1)
    } else {
        1
    };
    let mut original = Some(request);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let clone = if attempt < max_attempts {
            original.as_ref().and_then(Request::try_clone)
        } else {
            None
        };
        let (req, last) = match clone {
            Some(req) => (req, false),
            None => match original.take() {
                Some(req) => (req, true),
                None => {
                    return Err(IntegrationError::Other(
                        "internal error: request consumed before retry".into(),
                    ));
                }
            },
        };
        let started = Instant::now();
        let outcome = client.execute(req).await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(resp) => {
                let status = resp.status();
                let reason = resp
                    .extensions()
                    .get::<hyper::ext::ReasonPhrase>()
                    .map(|r| String::from_utf8_lossy(r.as_bytes()).into_owned());
                let headers = resp.headers().clone();
                let body = match read_body_limited(resp, MAX_BODY_BYTES).await {
                    Ok(bytes) => bytes,
                    Err(BodyError::TooLarge { limit }) => {
                        return Err(IntegrationError::Other(format!(
                            "{provider}: response body exceeds {} MiB",
                            limit / (1024 * 1024)
                        )));
                    }
                    Err(BodyError::Transport(err)) => {
                        let err = map_reqwest_error(err);
                        if !last && err.is_retryable() {
                            tokio::time::sleep(policy.backoff(attempt)).await;
                            continue;
                        }
                        return Err(err);
                    }
                };
                tracing::debug!(
                    provider,
                    method = %method,
                    path = %path,
                    status = status.as_u16(),
                    attempt,
                    elapsed_ms,
                    "provider request"
                );
                let raw = RawResponse {
                    status,
                    reason,
                    headers,
                    body,
                };
                if !last && policy.retry_statuses.contains(&status.as_u16()) {
                    let delay = raw.retry_after().unwrap_or_else(|| policy.backoff(attempt));
                    if delay <= policy.max_delay {
                        tracing::warn!(
                            provider,
                            method = %method,
                            path = %path,
                            status = status.as_u16(),
                            attempt,
                            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                            "transient provider error, retrying"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                }
                return Ok(raw);
            }
            Err(err) => {
                let err = map_reqwest_error(err);
                tracing::debug!(
                    provider,
                    method = %method,
                    path = %path,
                    attempt,
                    elapsed_ms,
                    error_kind = err.kind(),
                    "provider request failed"
                );
                if !last && err.is_retryable() {
                    tokio::time::sleep(policy.backoff(attempt)).await;
                    continue;
                }
                return Err(err);
            }
        }
    }
}

/// Maps a `reqwest` error (URL removed) to `Timeout` / `Transport`; a
/// destination refused by the outbound policy becomes `Config` (not retried).
pub(crate) fn map_reqwest_error(err: reqwest::Error) -> IntegrationError {
    let err = err.without_url();
    if let Some(refused) = outbound_error_in(&err) {
        return refused.clone().into();
    }
    if err.is_timeout() {
        return IntegrationError::Timeout;
    }
    let detail = truncate(&error_chain(&err));
    if err.is_connect() {
        IntegrationError::Transport(format!("connection failed: {detail}"))
    } else if err.is_builder() {
        IntegrationError::Config(format!("invalid request: {detail}"))
    } else {
        IntegrationError::Transport(detail)
    }
}

/// `err: source: source...` in one line.
pub(crate) fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = inner.source();
    }
    out
}

/// Truncates `s` to [`MAX_MESSAGE_LEN`] characters.
pub(crate) fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_MESSAGE_LEN {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(MAX_MESSAGE_LEN).collect();
        out.push('…');
        out
    }
}

/// Replaces every occurrence of each non-trivial secret in `message`.
pub(crate) fn redact(message: &str, secrets: &[&str]) -> String {
    let mut out = message.to_owned();
    for secret in secrets {
        if secret.len() >= 4 && out.contains(secret) {
            out = out.replace(secret, "[REDACTED]");
        }
    }
    out
}

/// Applies [`redact`] to the free-text parts of an error.
pub(crate) fn redact_error(err: IntegrationError, secrets: &[&str]) -> IntegrationError {
    use IntegrationError as E;
    match err {
        E::Auth(m) => E::Auth(redact(&m, secrets)),
        E::NotFound(m) => E::NotFound(redact(&m, secrets)),
        E::Conflict(m) => E::Conflict(redact(&m, secrets)),
        E::InvalidInput(m) => E::InvalidInput(redact(&m, secrets)),
        E::Transport(m) => E::Transport(redact(&m, secrets)),
        E::Config(m) => E::Config(redact(&m, secrets)),
        E::Other(m) => E::Other(redact(&m, secrets)),
        E::Remote {
            status,
            code,
            message,
        } => E::Remote {
            status,
            code,
            message: redact(&message, secrets),
        },
        other => other,
    }
}

/// Appends path segments (percent-encoded) to `base`.
pub(crate) fn join_path(base: &url::Url, segments: &[&str]) -> Result<url::Url, IntegrationError> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|()| IntegrationError::Config("API URL cannot be a base URL".into()))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// Normalises a configured base URL: drops query/fragment and a trailing `/`,
/// and appends `suffix` path segments unless the path already ends with them.
pub(crate) fn normalize_base(
    mut url: url::Url,
    suffix: &[&str],
) -> Result<url::Url, IntegrationError> {
    url.set_query(None);
    url.set_fragment(None);
    let existing: Vec<String> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default();
    let has_suffix = !suffix.is_empty()
        && existing.len() >= suffix.len()
        && existing[existing.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(a, b)| a == b);
    {
        let mut segs = url
            .path_segments_mut()
            .map_err(|()| IntegrationError::Config("API URL cannot be a base URL".into()))?;
        segs.clear();
        segs.extend(existing.iter().map(String::as_str));
        if !has_suffix {
            segs.extend(suffix);
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_base_urls() {
        let u = url::Url::parse("https://pve:8006/").unwrap();
        assert_eq!(
            normalize_base(u, &["api2", "json"]).unwrap().as_str(),
            "https://pve:8006/api2/json"
        );
        let u = url::Url::parse("https://pve:8006/api2/json/?x=1").unwrap();
        assert_eq!(
            normalize_base(u, &["api2", "json"]).unwrap().as_str(),
            "https://pve:8006/api2/json"
        );
        let u = url::Url::parse("https://api.hetzner.cloud/v1/").unwrap();
        assert_eq!(
            normalize_base(u, &[]).unwrap().as_str(),
            "https://api.hetzner.cloud/v1"
        );
    }

    #[test]
    fn joins_and_encodes_segments() {
        let base = url::Url::parse("https://pve:8006/api2/json").unwrap();
        let u = join_path(
            &base,
            &[
                "nodes",
                "pve1",
                "tasks",
                "UPID:pve1:00001:x:y:qmstart:100:root@pam!t:",
            ],
        )
        .unwrap();
        assert_eq!(
            u.path(),
            "/api2/json/nodes/pve1/tasks/UPID:pve1:00001:x:y:qmstart:100:root@pam!t:"
        );
        let u = join_path(&base, &["a/b"]).unwrap();
        assert_eq!(u.path(), "/api2/json/a%2Fb");
    }

    #[test]
    fn redacts_secrets() {
        let e = IntegrationError::remote(Some(500), "bad value 'hunter22' for cipassword");
        let e = redact_error(e, &["hunter22", "x"]);
        assert_eq!(
            e.to_string(),
            "provider error (HTTP 500): bad value '[REDACTED]' for cipassword"
        );
    }

    #[test]
    fn status_error_keeps_code_and_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "7".parse().unwrap());
        let raw = RawResponse {
            status: StatusCode::TOO_MANY_REQUESTS,
            reason: None,
            headers,
            body: Vec::new(),
        };
        assert_eq!(
            raw.status_error("slow down", None),
            IntegrationError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        let raw = RawResponse {
            status: StatusCode::BAD_GATEWAY,
            reason: Some("Bad Gateway".into()),
            headers: HeaderMap::new(),
            body: Vec::new(),
        };
        assert_eq!(
            raw.status_error("down", Some("bad_gateway".into())),
            IntegrationError::remote_with_code(Some(502), "bad_gateway", "down")
        );
    }

    #[test]
    fn backoff_is_bounded() {
        let p = RetryPolicy::standard();
        assert_eq!(p.backoff(1), Duration::from_millis(250));
        assert_eq!(p.backoff(2), Duration::from_millis(500));
        assert_eq!(p.backoff(30), Duration::from_secs(5));
    }
}
