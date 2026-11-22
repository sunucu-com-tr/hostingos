//! Shared HTTP plumbing for the AI providers: bounded retries that honour
//! `retry-after` / `retry-after-ms` / `x-should-retry`, and error mapping.
//!
//! The API base URL is tenant-configurable (gateways, self-hosted models), so
//! every request goes through the outbound policy
//! ([`hos_core::net::OutboundPolicy`], `HOS_OUTBOUND_POLICY`): the request URL is
//! checked before sending and every DNS answer at connect time; redirects are
//! not followed (see [`client_builder`]) and bodies are capped at
//! [`MAX_BODY_BYTES`]. Error messages only quote the provider's own JSON error
//! fields, never raw bodies.

use std::time::Duration;

use hos_core::IntegrationError;
use hos_core::net::{BodyError, OutboundPolicy, outbound_error_in, read_body_limited};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};

/// Largest response accepted from an AI API.
pub(crate) const MAX_BODY_BYTES: usize = hos_core::net::MAX_API_BODY;

/// `reqwest` client builder for an AI provider under `policy` (checked DNS
/// answers, no redirects, no system proxy) after a static check of
/// `base_url`.
pub(crate) fn client_builder(
    policy: OutboundPolicy,
    base_url: &str,
) -> Result<reqwest::ClientBuilder, IntegrationError> {
    let parsed = url::Url::parse(base_url.trim())
        .map_err(|_| IntegrationError::Config("config.base_url must be an http(s) URL".into()))?;
    policy.check_url(&parsed)?;
    Ok(policy.client_builder())
}

/// Builds `request`, checks its URL against `outbound` (IP-literal hosts
/// never reach the policy's DNS resolver) and sends it once.
pub(crate) async fn send(
    client: &reqwest::Client,
    outbound: OutboundPolicy,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, IntegrationError> {
    let request = request.build().map_err(|e| map_reqwest(&e))?;
    outbound.check_url(request.url())?;
    client.execute(request).await.map_err(|e| map_reqwest(&e))
}

/// Reads a response body (lossy UTF-8) with the [`MAX_BODY_BYTES`] cap.
pub(crate) async fn read_text(resp: reqwest::Response) -> Result<String, IntegrationError> {
    match read_body_limited(resp, MAX_BODY_BYTES).await {
        Ok(body) => Ok(String::from_utf8_lossy(&body).into_owned()),
        Err(BodyError::TooLarge { limit }) => Err(IntegrationError::Other(format!(
            "AI provider response exceeds {} MiB",
            limit / (1024 * 1024)
        ))),
        Err(BodyError::Transport(e)) => Err(map_reqwest(&e)),
    }
}

/// Retry behaviour for throttling / overload responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Retries after the first attempt (0 disables retrying).
    pub max_retries: u32,
    /// First back-off when the server gives no `retry-after` (doubles each time).
    pub base_delay: Duration,
    /// Upper bound for any single wait (also caps `retry-after`).
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(1_000),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// `retry-after-ms` (OpenAI) or `retry-after` (seconds) header.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some(Duration::from_millis(ms as u64));
    }
    get("retry-after")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
}

pub(crate) fn map_reqwest(err: &reqwest::Error) -> IntegrationError {
    if let Some(refused) = outbound_error_in(err) {
        return refused.clone().into();
    }
    if err.is_timeout() {
        IntegrationError::Timeout
    } else {
        IntegrationError::Transport(err.to_string())
    }
}

/// Sends the request built by `build` (URL checked against `outbound`, see
/// [`send`]), retrying `retry_statuses` (and connection failures that
/// happened before the request was sent) up to `policy.max_retries` times.
/// Returns the final response whatever its status.
pub(crate) async fn send_with_retry(
    client: &reqwest::Client,
    outbound: OutboundPolicy,
    build: impl Fn() -> reqwest::RequestBuilder,
    policy: &RetryPolicy,
    retry_statuses: &[u16],
    provider: &'static str,
) -> Result<reqwest::Response, IntegrationError> {
    let mut attempt: u32 = 0;
    loop {
        let backoff = policy
            .base_delay
            .saturating_mul(2u32.saturating_pow(attempt))
            .min(policy.max_delay);
        let request = build().build().map_err(|e| map_reqwest(&e))?;
        outbound.check_url(request.url())?;
        match client.execute(request).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let should_retry_header = resp
                    .headers()
                    .get("x-should-retry")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.trim().eq_ignore_ascii_case("true"));
                let retryable = match should_retry_header {
                    Some(explicit) => explicit && (status == 429 || status >= 500),
                    None => retry_statuses.contains(&status),
                };
                if retryable && attempt < policy.max_retries {
                    let wait = retry_after(resp.headers())
                        .unwrap_or(backoff)
                        .min(policy.max_delay);
                    tracing::warn!(
                        provider,
                        status,
                        attempt,
                        wait_ms = wait.as_millis() as u64,
                        "AI provider throttled; retrying"
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                    continue;
                }
                return Ok(resp);
            }
            Err(e)
                if e.is_connect()
                    && outbound_error_in(&e).is_none()
                    && attempt < policy.max_retries =>
            {
                tracing::warn!(provider, attempt, "AI provider connection failed; retrying");
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(e) => return Err(map_reqwest(&e)),
        }
    }
}

/// Maps a non-success response to an [`IntegrationError`].
pub(crate) fn map_status(
    status: u16,
    error_type: Option<&str>,
    message: String,
    retry_after: Option<Duration>,
) -> IntegrationError {
    match status {
        400 | 413 | 422 => IntegrationError::InvalidInput(message),
        401 | 403 => IntegrationError::Auth(message),
        404 => IntegrationError::NotFound(message),
        408 | 504 => IntegrationError::Timeout,
        429 => IntegrationError::RateLimited { retry_after },
        _ => IntegrationError::Remote {
            status: Some(status),
            code: error_type.map(str::to_owned),
            message,
        },
    }
}

/// Error message for a response without the provider's JSON error object.
/// The body is never quoted: a tenant-supplied base URL must not be able to
/// read internal pages back through error messages.
pub(crate) fn unparsed_error(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status} ({}-byte non-JSON response)", trimmed.len())
    }
}
