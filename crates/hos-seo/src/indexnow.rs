//! IndexNow client (<https://www.indexnow.org/documentation>).
//!
//! * [`generate_key`] creates a 32-character hex key; the site must serve it
//!   as plain text at `https://{host}/{key}.txt` ([`key_file_path`],
//!   [`key_file_content`]) or at a custom `keyLocation` on the same host.
//! * [`IndexNowClient::submit`] POSTs URLs (≤ 10 000 per request, batched
//!   automatically) to `https://api.indexnow.org/indexnow`, which forwards
//!   them to all participating engines (Bing, Yandex, Seznam, Naver, Yep…).
//!
//! Response codes: `200` OK, `202` accepted (key validation pending),
//! `400` bad request, `403` key not valid, `422` URLs don't belong to the host
//! or key mismatch, `429` too many requests.
//!
//! **Google is not supported here on purpose:** Google does not participate
//! in IndexNow and retired its sitemap "ping" endpoint (deprecated June 2023,
//! removed by early 2024). For Google, keep `lastmod` accurate in the sitemap,
//! list the sitemap in `robots.txt` and submit it once in Search Console.

use std::time::Duration;

use hos_core::IntegrationError;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Shared IndexNow endpoint.
pub const ENDPOINT: &str = "https://api.indexnow.org/indexnow";
/// Maximum URLs per POST.
pub const MAX_URLS_PER_REQUEST: usize = 10_000;

/// Generates a random 32-character lowercase hex key (128 bits).
pub fn generate_key() -> String {
    hos_core::crypto::random_bytes::<16>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Validates a key: 8–128 characters of `a-z`, `A-Z`, `0-9` and `-`.
pub fn validate_key(key: &str) -> Result<(), IntegrationError> {
    let ok = (8..=128).contains(&key.len())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(IntegrationError::InvalidInput(
            "IndexNow key must be 8-128 characters of a-z, A-Z, 0-9 or '-'".into(),
        ))
    }
}

/// Path the key file must be served at: `/{key}.txt`.
pub fn key_file_path(key: &str) -> String {
    format!("/{key}.txt")
}

/// Body of the key file (the key itself, UTF-8 `text/plain`).
pub fn key_file_content(key: &str) -> String {
    key.to_owned()
}

/// Outcome of one accepted batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    /// `200 OK`.
    Ok,
    /// `202 Accepted`: received, key validation pending.
    Accepted,
}

/// Result of [`IndexNowClient::submit`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitReport {
    /// URLs sent.
    pub submitted: usize,
    /// Status per batch.
    pub batches: Vec<BatchStatus>,
    /// URLs not sent because they are not absolute http(s) URLs on `host`.
    pub rejected: Vec<String>,
}

/// IndexNow HTTP client.
#[derive(Debug, Clone)]
pub struct IndexNowClient {
    http: reqwest::Client,
    endpoint: String,
}

impl IndexNowClient {
    /// Client for the shared endpoint with a 15 s timeout.
    pub fn new() -> Result<Self, IntegrationError> {
        Self::with_endpoint(ENDPOINT)
    }

    /// Client for a specific endpoint (an engine's own endpoint or a test server).
    pub fn with_endpoint(endpoint: impl Into<String>) -> Result<Self, IntegrationError> {
        // The endpoint is fixed (api.indexnow.org or an engine's own); still
        // never follow redirects with the submitted payload.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!(
                "HostingOS/",
                env!("CARGO_PKG_VERSION"),
                " (IndexNow)"
            ))
            .build()
            .map_err(|e| IntegrationError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            endpoint: endpoint.into(),
        })
    }

    /// Submits `urls` for `host` (e.g. `acme.example`). `key_location` is the
    /// absolute key-file URL when it is not `https://{host}/{key}.txt`.
    ///
    /// URLs on other hosts are dropped into [`SubmitReport::rejected`]
    /// (IndexNow would answer 422 for the whole batch). Stops at the first
    /// failing batch and returns its error.
    pub async fn submit(
        &self,
        host: &str,
        key: &str,
        key_location: Option<&str>,
        urls: &[String],
    ) -> Result<SubmitReport, IntegrationError> {
        if hos_core::net::is_offline() {
            // Demo mode: nothing leaves the server (docs/DEMO.md).
            return Err(hos_core::net::OutboundError::Offline.into());
        }
        validate_key(key)?;
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() || host.contains('/') {
            return Err(IntegrationError::InvalidInput(
                "host must be a bare hostname".into(),
            ));
        }
        let mut report = SubmitReport::default();
        let mut valid = Vec::new();
        for u in urls {
            let same_host = url::Url::parse(u.trim()).ok().is_some_and(|p| {
                matches!(p.scheme(), "http" | "https")
                    && p.host_str().is_some_and(|h| h.eq_ignore_ascii_case(&host))
            });
            if same_host {
                valid.push(u.trim().to_owned());
            } else {
                report.rejected.push(u.clone());
            }
        }
        valid.sort();
        valid.dedup();
        for chunk in valid.chunks(MAX_URLS_PER_REQUEST) {
            let mut body = json!({"host": host, "key": key, "urlList": chunk});
            if let Some(loc) = key_location {
                body["keyLocation"] = json!(loc);
            }
            let resp = self
                .http
                .post(&self.endpoint)
                .header("Content-Type", "application/json; charset=utf-8")
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        IntegrationError::Timeout
                    } else {
                        IntegrationError::Transport(e.to_string())
                    }
                })?;
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let batch = match status {
                200 => BatchStatus::Ok,
                202 => BatchStatus::Accepted,
                400 => {
                    return Err(IntegrationError::InvalidInput(
                        "IndexNow: bad request (invalid format)".into(),
                    ));
                }
                403 => {
                    return Err(IntegrationError::Auth(
                        "IndexNow: key not valid (key file missing or content mismatch)".into(),
                    ));
                }
                422 => {
                    return Err(IntegrationError::InvalidInput(
                        "IndexNow: URLs do not belong to the host or the key does not match".into(),
                    ));
                }
                429 => return Err(IntegrationError::RateLimited { retry_after }),
                s => {
                    // The body is not quoted (no response body is read at all).
                    return Err(IntegrationError::from_http_status(
                        s,
                        format!("IndexNow: unexpected HTTP {s}"),
                    ));
                }
            };
            report.submitted += chunk.len();
            report.batches.push(batch);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn keys() {
        let k = generate_key();
        assert_eq!(k.len(), 32);
        assert!(validate_key(&k).is_ok());
        assert_ne!(k, generate_key());
        assert!(validate_key("short").is_err());
        assert!(validate_key("has space 123456").is_err());
        assert_eq!(key_file_path("abc12345"), "/abc12345.txt");
        assert_eq!(key_file_content("abc12345"), "abc12345");
    }

    #[tokio::test]
    async fn submits_and_filters_foreign_hosts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/indexnow"))
            .and(header("content-type", "application/json; charset=utf-8"))
            .and(body_partial_json(json!({
                "host": "acme.example",
                "key": "0123456789abcdef",
                "keyLocation": "https://acme.example/0123456789abcdef.txt",
                "urlList": ["https://acme.example/a", "https://acme.example/b"]
            })))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;
        let client = IndexNowClient::with_endpoint(format!("{}/indexnow", server.uri())).unwrap();
        let report = client
            .submit(
                "acme.example",
                "0123456789abcdef",
                Some("https://acme.example/0123456789abcdef.txt"),
                &[
                    "https://acme.example/b".into(),
                    "https://acme.example/a".into(),
                    "https://evil.example/x".into(),
                    "https://acme.example/a".into(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(report.submitted, 2);
        assert_eq!(report.batches, vec![BatchStatus::Accepted]);
        assert_eq!(report.rejected, vec!["https://evil.example/x".to_owned()]);
    }

    #[tokio::test]
    async fn maps_error_codes() {
        for (code, kind) in [
            (400, "invalid_input"),
            (403, "auth"),
            (422, "invalid_input"),
            (429, "rate_limited"),
            (500, "remote"),
        ] {
            let server = MockServer::start().await;
            let mut resp = ResponseTemplate::new(code);
            if code == 429 {
                resp = resp.insert_header("retry-after", "30");
            }
            Mock::given(method("POST"))
                .respond_with(resp)
                .mount(&server)
                .await;
            let client = IndexNowClient::with_endpoint(server.uri()).unwrap();
            let err = client
                .submit(
                    "acme.example",
                    "0123456789abcdef",
                    None,
                    &["https://acme.example/".into()],
                )
                .await
                .unwrap_err();
            assert_eq!(err.kind(), kind, "HTTP {code}");
            if code == 429 {
                assert_eq!(err.retry_after(), Some(Duration::from_secs(30)));
            }
        }
    }

    #[tokio::test]
    async fn batches_large_lists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;
        let client = IndexNowClient::with_endpoint(server.uri()).unwrap();
        let urls: Vec<String> = (0..MAX_URLS_PER_REQUEST + 5)
            .map(|i| format!("https://acme.example/p/{i}"))
            .collect();
        let report = client
            .submit("acme.example", "0123456789abcdef", None, &urls)
            .await
            .unwrap();
        assert_eq!(report.batches, vec![BatchStatus::Ok, BatchStatus::Ok]);
        assert_eq!(report.submitted, urls.len());
    }
}
