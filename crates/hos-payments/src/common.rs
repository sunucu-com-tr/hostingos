//! Helpers shared by every gateway: HTTP client + error mapping, JSON amount
//! parsing, basket checks and signed browser-return URLs.

use std::str::FromStr;
use std::time::Duration;

use hos_core::crypto;
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::CheckoutRequest;
use hos_core::integrations::{FieldReader, FieldSpec, SelectOption};
use hos_core::money::{Currency, Money};
use hos_core::net::{
    BodyError, MAX_API_BODY, OutboundPolicy, outbound_error_in, read_body_limited,
};
use rust_decimal::Decimal;
use serde_json::Value;

/// Default timeout of every gateway HTTP call.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("HostingOS/", env!("CARGO_PKG_VERSION"), " (hos-payments)");

/// Test (sandbox) or live credentials, from the `mode` config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Sandbox / test credentials; no real money moves.
    Test,
    /// Live credentials.
    Live,
}

impl Mode {
    /// Reads `config.mode` (`test` | `live`, default `test`).
    pub(crate) fn from_config(reader: &FieldReader<'_>) -> Result<Self, IntegrationError> {
        match reader.string_or("mode", "test")?.as_str() {
            "test" | "sandbox" => Ok(Self::Test),
            "live" => Ok(Self::Live),
            _ => Err(IntegrationError::Config(
                "config.mode must be `test` or `live`".into(),
            )),
        }
    }

    /// `true` for [`Mode::Live`].
    pub fn is_live(self) -> bool {
        self == Self::Live
    }
}

/// The `mode` select shared by all online gateways.
pub(crate) fn mode_field() -> FieldSpec {
    FieldSpec::select(
        "mode",
        "Mode",
        vec![
            SelectOption::new("test", "Test / sandbox"),
            SelectOption::new("live", "Live"),
        ],
    )
    .required()
    .default_value("test")
}

/// The outbound policy gateway clients enforce: the process-wide
/// [`OutboundPolicy::global`]; this crate's own unit tests talk to wiremock on
/// loopback and use `Unrestricted` (never compiled into a release build).
pub(crate) fn outbound_policy() -> OutboundPolicy {
    #[cfg(test)]
    {
        OutboundPolicy::Unrestricted
    }
    #[cfg(not(test))]
    {
        OutboundPolicy::global()
    }
}

/// Builds a timeout-bounded HTTP client enforcing the outbound policy: DNS
/// answers are checked at connect time, redirects are not followed and
/// system proxies are ignored. Every gateway API host is a fixed provider
/// host name (never an IP literal), so all requests reach the policy resolver.
pub(crate) fn http_client(timeout: Duration) -> Result<reqwest::Client, IntegrationError> {
    outbound_policy()
        .client_builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| IntegrationError::Config(format!("HTTP client: {}", e.without_url())))
}

/// Config key that points a gateway's API base at a local mock server.
/// Honoured only while the process-wide outbound policy is `Unrestricted`,
/// which only integration-test harnesses can select
/// ([`OutboundPolicy::set_global`]); `HOS_OUTBOUND_POLICY` cannot. Not part
/// of any descriptor, so the admin form never shows it.
pub(crate) const API_BASE_OVERRIDE: &str = "api_base_override";

/// The test-harness API base from `config`, if allowed (see [`API_BASE_OVERRIDE`]).
pub(crate) fn api_base_override(config: &Value) -> Option<String> {
    if OutboundPolicy::global() != OutboundPolicy::Unrestricted {
        return None;
    }
    config
        .get(API_BASE_OVERRIDE)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Maps a `reqwest` failure. URLs are stripped so query strings never reach logs.
pub(crate) fn map_reqwest(err: reqwest::Error) -> IntegrationError {
    if let Some(refused) = outbound_error_in(&err) {
        return refused.clone().into();
    }
    if err.is_timeout() {
        return IntegrationError::Timeout;
    }
    let connect = err.is_connect();
    let err = err.without_url();
    if connect {
        IntegrationError::Transport(format!("connection failed: {err}"))
    } else {
        IntegrationError::Transport(err.to_string())
    }
}

/// A fully read HTTP response.
#[derive(Debug)]
pub(crate) struct Response {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Body as JSON; an unparsable body is a transport error (or the HTTP
    /// status error when the status is not 2xx).
    pub fn json(&self) -> Result<Value, IntegrationError> {
        serde_json::from_slice(&self.body).map_err(|_| {
            if self.is_success() {
                IntegrationError::Transport(format!(
                    "unparsable response body (HTTP {})",
                    self.status
                ))
            } else {
                status_error(
                    self.status,
                    self.retry_after,
                    format!("HTTP {} with a non-JSON body", self.status),
                    None,
                )
            }
        })
    }
}

/// Sends a request and reads the whole body (at most [`MAX_API_BODY`] bytes).
pub(crate) async fn send(request: reqwest::RequestBuilder) -> Result<Response, IntegrationError> {
    let response = request.send().await.map_err(map_reqwest)?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = read_body_limited(response, MAX_API_BODY)
        .await
        .map_err(|e| match e {
            BodyError::TooLarge { limit } => {
                IntegrationError::Transport(format!("response body exceeds {limit} bytes"))
            }
            BodyError::Transport(e) => map_reqwest(e),
        })?;
    Ok(Response {
        status,
        retry_after,
        body,
    })
}

/// Maps an HTTP error status (plus the provider's message/code) to an [`IntegrationError`].
pub(crate) fn status_error(
    status: u16,
    retry_after: Option<Duration>,
    message: String,
    code: Option<String>,
) -> IntegrationError {
    match status {
        429 => IntegrationError::RateLimited { retry_after },
        401 | 403 => IntegrationError::Auth(message),
        404 => IntegrationError::NotFound(message),
        409 => IntegrationError::Conflict(message),
        400 | 422 => IntegrationError::InvalidInput(match code {
            Some(code) => format!("{code}: {message}"),
            None => message,
        }),
        408 | 504 => IntegrationError::Timeout,
        _ => IntegrationError::Remote {
            status: Some(status),
            code,
            message,
        },
    }
}

/// Shorthand for an authentication failure of an inbound webhook.
pub(crate) fn auth(message: impl Into<String>) -> IntegrationError {
    IntegrationError::Auth(message.into())
}

/// Current Unix time in seconds.
pub(crate) fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Parses a decimal from a JSON string or number (`"10.50"`, `10.5`, `1e2`).
pub(crate) fn json_decimal(value: &Value) -> Option<Decimal> {
    let text = match value {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Decimal::from_str(&text)
        .ok()
        .or_else(|| Decimal::from_scientific(&text).ok())
}

/// `json_decimal` as [`Money`].
pub(crate) fn json_money(value: &Value, currency: Currency) -> Option<Money> {
    json_decimal(value).map(|amount| Money::new(amount, currency))
}

/// A JSON string or integer as a string (ids are sometimes numbers).
pub(crate) fn json_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Decimal without trailing fractional zeros (`10.50` → `10.5`, `10.00` → `10`).
pub(crate) fn trim_zeros(amount: Decimal) -> String {
    amount.normalize().to_string()
}

/// Recursively removes `keys` from every JSON object in `value`. Used to keep
/// card details (BIN, last four, expiry, wallet/card objects) out of
/// `PaymentEvent::raw`, which is persisted.
pub(crate) fn strip_keys(mut value: Value, keys: &[&str]) -> Value {
    fn walk(value: &mut Value, keys: &[&str]) {
        match value {
            Value::Object(map) => {
                for key in keys {
                    map.remove(*key);
                }
                map.values_mut().for_each(|child| walk(child, keys));
            }
            Value::Array(items) => items.iter_mut().for_each(|child| walk(child, keys)),
            _ => {}
        }
    }
    walk(&mut value, keys);
    value
}

/// Whether `s` is non-empty ASCII alphanumeric.
pub(crate) fn is_alphanumeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// At most `max` characters of `s`.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Splits a full name into (given names, surname). Single words are used for both.
pub(crate) fn split_name(full: &str) -> (String, String) {
    let full = full.trim();
    match full.rsplit_once(char::is_whitespace) {
        Some((first, last)) if !first.trim().is_empty() => {
            (first.trim().to_owned(), last.trim().to_owned())
        }
        _ => (full.to_owned(), full.to_owned()),
    }
}

/// Whether the basket can be sent line by line: every line positive and in the
/// request currency, and the rounded line totals add up to the rounded amount.
/// Gateways fall back to a single summary line otherwise (discount lines,
/// rounding differences, foreign-currency lines).
pub(crate) fn itemizable(request: &CheckoutRequest) -> bool {
    if request.items.is_empty() {
        return false;
    }
    let currency = request.amount.currency;
    let mut sum = Money::zero(currency);
    for item in &request.items {
        let total = item.total.round();
        if item.quantity == 0 || total.currency != currency || !total.is_positive() {
            return false;
        }
        sum = match sum.try_add(&total) {
            Ok(s) => s,
            Err(_) => return false,
        };
    }
    sum == request.amount.round()
}

/// Rejects non-positive or non-finite amounts before they reach a gateway.
pub(crate) fn ensure_positive(amount: &Money) -> Result<(), IntegrationError> {
    if amount.round().is_positive() {
        Ok(())
    } else {
        Err(IntegrationError::InvalidInput(
            "amount must be greater than zero".into(),
        ))
    }
}

/// Escapes text for an HTML attribute value or text node.
pub(crate) fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Appends query pairs to a URL string.
pub(crate) fn with_query(base: &str, pairs: &[(&str, &str)]) -> Result<String, IntegrationError> {
    let mut url = http_url(base, "webhook_url")?;
    {
        let mut query = url.query_pairs_mut();
        for (k, v) in pairs {
            query.append_pair(k, v);
        }
    }
    Ok(url.into())
}

/// First value of a query pair.
pub(crate) fn pair<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Parses an `http(s)` URL supplied by the caller.
pub(crate) fn http_url(value: &str, what: &str) -> Result<url::Url, IntegrationError> {
    url::Url::parse(value.trim())
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
        .ok_or_else(|| IntegrationError::InvalidInput(format!("{what} must be an http(s) URL")))
}

// ---------------------------------------------------------------------------
// Signed browser-return URLs
// ---------------------------------------------------------------------------

const RETURN_OK: &str = "hos_ok";
const RETURN_FAIL: &str = "hos_fail";
const RETURN_SIG: &str = "hos_sig";

/// Where to send the customer after a browser POST-back / return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReturnTargets {
    pub ok: String,
    pub fail: String,
}

/// Signs the success/cancel URLs into the query string of the webhook URL, so a
/// browser return that lands on `/webhooks/gateway/{id}` (iyzico callback,
/// PayPal approval return) can be redirected without an open redirect and
/// without server-side session state. The key is derived from a gateway secret
/// with domain separation, so the raw secret is never used for this purpose.
#[derive(Clone)]
pub(crate) struct ReturnSigner {
    key: [u8; 32],
}

impl std::fmt::Debug for ReturnSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReturnSigner([REDACTED])")
    }
}

impl ReturnSigner {
    pub fn new(secret: &[u8], domain: &str) -> Self {
        // Pre-rename "hf" label, kept on purpose: signs return URLs already issued.
        let label = format!("hf-payments/return-url/v1/{domain}");
        Self {
            key: crypto::hmac_sha256(secret, label.as_bytes()),
        }
    }

    fn message(ok: &str, fail: &str) -> String {
        // Length prefix: unambiguous for any URL contents.
        format!("{}:{ok}{fail}", ok.len())
    }

    /// `base` (the webhook URL) with `hos_ok`, `hos_fail` and `hos_sig` appended.
    pub fn signed_url(&self, base: &str, ok: &str, fail: &str) -> Result<String, IntegrationError> {
        let mut url = http_url(base, "webhook_url")?;
        http_url(ok, "return_url")?;
        http_url(fail, "cancel_url")?;
        let sig = crypto::hmac_sha256_hex(&self.key, Self::message(ok, fail).as_bytes());
        url.query_pairs_mut()
            .append_pair(RETURN_OK, ok)
            .append_pair(RETURN_FAIL, fail)
            .append_pair(RETURN_SIG, &sig);
        Ok(url.into())
    }

    fn tag_message(purpose: &str, values: &[&str]) -> Vec<u8> {
        // `tag:` prefix: never collides with a return-URL message (starts
        // with a digit); every value is length-prefixed.
        let mut message = format!("tag:{}:{purpose}", purpose.len()).into_bytes();
        for value in values {
            message.extend_from_slice(format!("|{}:", value.len()).as_bytes());
            message.extend_from_slice(value.as_bytes());
        }
        message
    }

    /// Hex HMAC binding `values` for `purpose` (amounts / references carried
    /// through a browser form or a provider callback URL).
    pub fn tag(&self, purpose: &str, values: &[&str]) -> String {
        crypto::hmac_sha256_hex(&self.key, &Self::tag_message(purpose, values))
    }

    /// Constant-time check of a [`tag`](Self::tag).
    pub fn verify_tag(&self, purpose: &str, values: &[&str], tag: &str) -> bool {
        crypto::verify_hmac_sha256_hex(&self.key, &Self::tag_message(purpose, values), tag)
    }

    /// The verified targets from query pairs, or `None` when absent / forged.
    pub fn verify(&self, pairs: &[(String, String)]) -> Option<ReturnTargets> {
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let (ok, fail, sig) = (get(RETURN_OK)?, get(RETURN_FAIL)?, get(RETURN_SIG)?);
        crypto::verify_hmac_sha256_hex(&self.key, Self::message(ok, fail).as_bytes(), sig).then(
            || ReturnTargets {
                ok: ok.to_owned(),
                fail: fail.to_owned(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    #[test]
    fn decimals_from_json() {
        assert_eq!(json_decimal(&json!("10.50")), Some(dec!(10.50)));
        assert_eq!(json_decimal(&json!(10.5)), Some(dec!(10.5)));
        assert_eq!(json_decimal(&json!(100)), Some(dec!(100)));
        assert_eq!(json_decimal(&json!(null)), None);
        assert_eq!(trim_zeros(dec!(10.50)), "10.5");
        assert_eq!(trim_zeros(dec!(10.00)), "10");
        assert_eq!(trim_zeros(dec!(10.510)), "10.51");
        assert_eq!(json_id(&json!(12345)), Some("12345".into()));
    }

    #[test]
    fn names_and_strings() {
        assert_eq!(
            split_name("Ada Lovelace King"),
            ("Ada Lovelace".into(), "King".into())
        );
        assert_eq!(split_name("Cher"), ("Cher".into(), "Cher".into()));
        assert!(is_alphanumeric("INV2026A42"));
        assert!(!is_alphanumeric("INV-42"));
        assert!(!is_alphanumeric(""));
        assert_eq!(truncate("çğüşö", 3), "çğü");
    }

    #[test]
    fn return_signer_roundtrip_and_tamper() {
        let signer = ReturnSigner::new(b"secret", "test");
        let url = signer
            .signed_url(
                "https://shop.example/webhooks/gateway/1",
                "https://shop.example/client/invoices/7?paid=1",
                "https://shop.example/client/invoices/7",
            )
            .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
        let targets = signer.verify(&pairs).unwrap();
        assert_eq!(targets.ok, "https://shop.example/client/invoices/7?paid=1");

        let mut forged = pairs.clone();
        forged[0].1 = "https://evil.example/".into();
        assert_eq!(signer.verify(&forged), None);
        assert_eq!(ReturnSigner::new(b"other", "test").verify(&pairs), None);
        assert_eq!(ReturnSigner::new(b"secret", "other").verify(&pairs), None);
        assert!(
            signer
                .signed_url(
                    "https://x.example/w",
                    "javascript:alert(1)",
                    "https://x.example"
                )
                .is_err()
        );
    }
}
