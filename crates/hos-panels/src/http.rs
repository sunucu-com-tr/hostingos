//! Shared HTTP plumbing for every panel client: timeouts, TLS options, transport
//! and status error mapping, and scrubbing of secrets from error messages.
//!
//! Nothing here logs request URLs, query strings or bodies — panels put
//! passwords and tokens there. The only log line per call is a `debug!` with
//! the panel, the operation name, the HTTP status and the elapsed time.
//!
//! The panel host is tenant-configured, so every client enforces the
//! outbound policy ([`hos_core::net::OutboundPolicy`], `HOS_OUTBOUND_POLICY`):
//! the base URL when the client is built, each request URL before sending and
//! every DNS answer at connect time. Redirects are not followed, bodies are
//! capped at [`MAX_BODY_BYTES`] and error messages never quote raw response
//! bodies — only messages the adapters parsed out of the panel's API format.

use std::error::Error as _;
use std::time::{Duration, Instant};

use hos_core::IntegrationError;
use hos_core::net::{BodyError, OutboundPolicy, outbound_error_in, read_body_limited};
use hos_core::secret::SecretString;
use reqwest::header::HeaderMap;
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use url::Url;

/// `User-Agent` sent to panels.
pub(crate) const USER_AGENT: &str = concat!("HostingOS-hos-panels/", env!("CARGO_PKG_VERSION"));

/// Longest error message (in chars) copied from a panel response.
const MAX_MESSAGE_CHARS: usize = 400;

/// Connect timeout; the overall request timeout is configurable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest panel response accepted (account lists of big servers included).
pub(crate) const MAX_BODY_BYTES: usize = hos_core::net::MAX_API_BODY;

/// Where and how to reach a panel's API.
#[derive(Debug, Clone)]
pub(crate) struct Endpoint {
    /// `scheme://host:port/` of the API.
    pub base: Url,
    /// Whole-request timeout.
    pub timeout: Duration,
    /// Accept self-signed / mismatching certificates.
    pub skip_tls_verify: bool,
    /// Which destinations may be contacted.
    pub policy: OutboundPolicy,
}

/// Builds `scheme://host:port/` from a host setting.
///
/// `host` may be a bare hostname / IPv4 / IPv6 address (then `https` decides
/// the scheme and `port` is used) or a full `http(s)://host[:port]` URL (its
/// scheme wins; `port` only fills a missing port).
pub(crate) fn base_url(host: &str, port: u16, https: bool) -> Result<Url, IntegrationError> {
    let host = host.trim().trim_end_matches('/');
    let invalid = || IntegrationError::Config("config.host must be a hostname, IP or URL".into());
    if host.is_empty() {
        return Err(invalid());
    }
    let mut url = if host.contains("://") {
        let url = Url::parse(host).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(invalid());
        }
        url
    } else {
        if host.contains(['/', '?', '#', '@', ' ']) {
            return Err(invalid());
        }
        let bracketed = if host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let scheme = if https { "https" } else { "http" };
        Url::parse(&format!("{scheme}://{bracketed}/")).map_err(|_| invalid())?
    };
    if url.port().is_none() {
        url.set_port(Some(port)).map_err(|()| invalid())?;
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// A received HTTP response (status checking is up to the panel adapter,
/// because several panels report errors inside HTTP 200 bodies).
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Reply {
    /// `Content-Type` header (lower-cased), or `""`.
    pub fn content_type(&self) -> String {
        self.headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// Whether the body looks like an HTML page (login form, proxy error page).
    pub fn is_html(&self) -> bool {
        self.content_type().contains("text/html") || looks_like_html(&self.body)
    }
}

/// HTTP client bound to one panel endpoint.
#[derive(Clone)]
pub(crate) struct PanelHttp {
    client: Client,
    policy: OutboundPolicy,
    base: Url,
    panel: &'static str,
    /// Integration credentials, scrubbed from every error message.
    credentials: Vec<SecretString>,
}

impl std::fmt::Debug for PanelHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelHttp")
            .field("panel", &self.panel)
            .field("base", &self.base.as_str())
            .finish_non_exhaustive()
    }
}

impl PanelHttp {
    /// Creates the client. `credentials` are never sent anywhere by this type;
    /// they are only used to scrub error messages.
    pub fn new(
        panel: &'static str,
        endpoint: &Endpoint,
        credentials: Vec<SecretString>,
    ) -> Result<Self, IntegrationError> {
        endpoint.policy.check_url(&endpoint.base)?;
        let client = endpoint
            .policy
            .client_builder()
            .user_agent(USER_AGENT)
            .timeout(endpoint.timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(endpoint.timeout))
            .danger_accept_invalid_certs(endpoint.skip_tls_verify)
            .build()
            .map_err(|e| IntegrationError::Config(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            client,
            policy: endpoint.policy,
            base: endpoint.base.clone(),
            panel,
            credentials: credentials.into_iter().filter(|s| !s.is_empty()).collect(),
        })
    }

    /// Absolute URL of an API path (relative to the base, leading `/` optional).
    pub fn url(&self, path: &str) -> Result<Url, IntegrationError> {
        self.base
            .join(path.trim_start_matches('/'))
            .map_err(|_| IntegrationError::InvalidInput(format!("invalid API path `{path}`")))
    }

    /// Starts a request to an API path.
    pub fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, IntegrationError> {
        Ok(self.client.request(method, self.url(path)?))
    }

    /// Sends a request and reads the whole body. Transport failures and
    /// timeouts are mapped; HTTP status codes are left to the caller.
    pub async fn send(
        &self,
        op: &'static str,
        req: RequestBuilder,
    ) -> Result<Reply, IntegrationError> {
        let request = req.build().map_err(|e| self.transport_error(e))?;
        // IP-literal hosts never reach the policy's DNS resolver.
        self.policy.check_url(request.url())?;
        let started = Instant::now();
        let result = async {
            let resp = self.client.execute(request).await?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = read_body_limited(resp, MAX_BODY_BYTES).await?;
            Ok::<_, BodyError>(Reply {
                status,
                headers,
                body: String::from_utf8_lossy(&body).into_owned(),
            })
        }
        .await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(reply) => {
                tracing::debug!(
                    panel = self.panel,
                    op,
                    status = reply.status.as_u16(),
                    elapsed_ms,
                    "panel API call"
                );
                Ok(reply)
            }
            Err(err) => {
                let mapped = match err {
                    BodyError::TooLarge { limit } => IntegrationError::Other(format!(
                        "{}: response body exceeds {} MiB",
                        self.panel,
                        limit / (1024 * 1024)
                    )),
                    BodyError::Transport(err) => self.transport_error(err),
                };
                tracing::warn!(
                    panel = self.panel,
                    op,
                    elapsed_ms,
                    error = %mapped,
                    "panel API call failed"
                );
                Err(mapped)
            }
        }
    }

    fn transport_error(&self, err: reqwest::Error) -> IntegrationError {
        // The URL may carry query parameters; drop it before formatting.
        let err = err.without_url();
        if let Some(refused) = outbound_error_in(&err) {
            return refused.clone().into();
        }
        if err.is_timeout() {
            return IntegrationError::Timeout;
        }
        let mut detail = err.to_string();
        let mut source = err.source();
        while let Some(s) = source {
            detail.push_str(": ");
            detail.push_str(&s.to_string());
            source = s.source();
        }
        let host = self.base.host_str().unwrap_or("panel");
        let what = if err.is_connect() {
            format!("{}: cannot connect to {host}", self.panel)
        } else if err.is_decode() {
            format!("{}: cannot read response from {host}", self.panel)
        } else {
            format!("{}: request to {host} failed", self.panel)
        };
        IntegrationError::Transport(self.scrub(&format!("{what}: {detail}"), &[]))
    }

    /// Maps a non-success HTTP status to an error. `message` is the panel's
    /// own error text when the adapter could extract one; otherwise a summary
    /// of the body is used. `extra` lists request secrets to scrub.
    pub fn status_error(
        &self,
        reply: &Reply,
        message: Option<&str>,
        extra: &[&str],
    ) -> IntegrationError {
        let summary;
        let message = match message {
            Some(m) if !m.trim().is_empty() => m,
            _ => {
                summary = summarize_body(&reply.body);
                summary.as_str()
            }
        };
        let message = self.scrub(
            &format!(
                "{} returned HTTP {}: {message}",
                self.panel,
                reply.status.as_u16()
            ),
            extra,
        );
        match reply.status.as_u16() {
            429 => IntegrationError::RateLimited {
                retry_after: retry_after(&reply.headers),
            },
            s @ (502..=504) => IntegrationError::remote(Some(s), message),
            s => IntegrationError::from_http_status(s, message),
        }
    }

    /// Error for a body that could not be parsed.
    pub fn parse_error(
        &self,
        op: &str,
        reply: &Reply,
        detail: impl std::fmt::Display,
    ) -> IntegrationError {
        if reply.is_html() {
            return IntegrationError::Transport(format!(
                "{}: {op}: expected an API response but received an HTML page; \
                 check host/port and credentials",
                self.panel,
            ));
        }
        IntegrationError::Transport(self.scrub(
            &format!(
                "{}: {op}: unparsable response ({detail}): {}",
                self.panel,
                summarize_body(&reply.body)
            ),
            &[],
        ))
    }

    /// Classifies a panel's business error text (see [`classify_message`]) and
    /// scrubs secrets from it.
    pub fn business_error(
        &self,
        message: &str,
        code: Option<String>,
        extra: &[&str],
    ) -> IntegrationError {
        let message = self.scrub(message.trim(), extra);
        let message = if message.is_empty() {
            format!("{} reported an unspecified error", self.panel)
        } else {
            message
        };
        classify_message(message, code)
    }

    /// Removes credentials and the given request secrets from a message,
    /// strips control characters and truncates it.
    pub fn scrub(&self, message: &str, extra: &[&str]) -> String {
        let secrets = self
            .credentials
            .iter()
            .map(SecretString::expose_str)
            .chain(extra.iter().copied());
        scrub_message(message, secrets)
    }
}

/// See [`PanelHttp::scrub`].
pub(crate) fn scrub_message<'a>(
    message: &str,
    secrets: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut out: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    for secret in secrets {
        // Very short "secrets" would destroy the message; real ones are long.
        if secret.chars().count() >= 4 && out.contains(secret) {
            out = out.replace(secret, "[REDACTED]");
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, MAX_MESSAGE_CHARS)
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_owned(),
    }
}

fn looks_like_html(body: &str) -> bool {
    let head: String = body
        .trim_start()
        .chars()
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html") || head.starts_with("<head")
}

/// One-line description of a response body for error messages. It never
/// quotes the body: a misconfigured (or malicious) host must not be able to
/// read internal pages back through error messages.
pub(crate) fn summarize_body(body: &str) -> String {
    if looks_like_html(body) {
        return "an HTML page".into();
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty response".into()
    } else {
        format!("a {}-byte non-API response", trimmed.len())
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Maps a panel's error text (returned with HTTP 200 or without a useful
/// status) to the most specific [`IntegrationError`] by keyword. Panels phrase
/// errors freely, so this is a heuristic; unknown texts become `Remote`
/// without a status (not retryable).
pub(crate) fn classify_message(message: String, code: Option<String>) -> IntegrationError {
    let lower = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if has(&[
        "access denied",
        "permission denied",
        "not authorized",
        "unauthorized",
        "authentication failed",
        "authentication required",
        "invalid api token",
        "invalid token",
        "token denied",
        "login failed",
        "invalid login",
        "invalid credentials",
        "do not have permission",
        "don't have permission",
        "insufficient privileges",
        "api access is disabled",
    ]) {
        return IntegrationError::Auth(message);
    }
    if has(&[
        "does not exist",
        "doesn't exist",
        "do not exist",
        "not found",
        "no such",
        "not exist",
        "cannot find",
        "could not find",
        "can't find",
        "unknown user",
        "unknown account",
        "is not a valid user",
        "no account",
    ]) {
        return IntegrationError::NotFound(message);
    }
    if has(&[
        "already exist",
        "already has an account",
        "already owned",
        "already in use",
        "already taken",
        "already been taken",
        "already owns",
        "is taken",
        "is already",
        "exists on this server",
        "duplicate",
        "conflict",
    ]) {
        return IntegrationError::Conflict(message);
    }
    if has(&[
        "invalid",
        "not valid",
        "must be",
        "must contain",
        "must begin",
        "must start",
        "is required",
        "are required",
        "missing",
        "too long",
        "too short",
        "too weak",
        "is reserved",
        "not allowed",
        "not permitted",
        "cannot begin",
        "cannot start",
        "cannot contain",
    ]) {
        return IntegrationError::InvalidInput(message);
    }
    IntegrationError::Remote {
        status: None,
        code,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_variants() {
        assert_eq!(
            base_url("whm.example.com", 2087, true).unwrap().as_str(),
            "https://whm.example.com:2087/"
        );
        assert_eq!(
            base_url("10.0.0.5", 2222, false).unwrap().as_str(),
            "http://10.0.0.5:2222/"
        );
        assert_eq!(
            base_url("::1", 8443, true).unwrap().as_str(),
            "https://[::1]:8443/"
        );
        assert_eq!(
            base_url("http://127.0.0.1:9000/some/path?x=1", 2087, true)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9000/"
        );
        assert_eq!(
            base_url("https://p.example.com", 8443, false)
                .unwrap()
                .as_str(),
            "https://p.example.com:8443/"
        );
        assert!(base_url("", 1, true).is_err());
        assert!(base_url("ftp://x", 1, true).is_err());
        assert!(base_url("host/with/path", 1, true).is_err());
    }

    #[test]
    fn scrub_removes_secrets_and_truncates() {
        let s = scrub_message("bad password hunter22 for user\nbob", ["hunter22", "x"]);
        assert_eq!(s, "bad password [REDACTED] for user bob");
        let long = "a".repeat(1000);
        assert!(scrub_message(&long, []).chars().count() <= MAX_MESSAGE_CHARS + 1);
    }

    #[test]
    fn classify_keywords() {
        let k = |m: &str| classify_message(m.into(), None).kind();
        assert_eq!(k("Sorry, a user with that name already exists"), "conflict");
        assert_eq!(k("The user “bob” does not exist."), "not_found");
        assert_eq!(k("Access denied"), "auth");
        assert_eq!(k("The username must begin with a letter"), "invalid_input");
        assert_eq!(k("Something odd happened"), "remote");
    }

    #[test]
    fn body_summaries_never_quote_the_body() {
        let body = "<!DOCTYPE html><html><head><title>Login</title></head></html>";
        assert_eq!(summarize_body(body), "an HTML page");
        assert_eq!(summarize_body("  "), "empty response");
        let secret = "root:x:0:0:root:/root:/bin/bash";
        let summary = summarize_body(secret);
        assert!(!summary.contains("root"), "{summary}");
        assert_eq!(summary, "a 31-byte non-API response");
    }
}
