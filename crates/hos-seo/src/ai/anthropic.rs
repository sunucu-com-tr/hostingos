//! Anthropic Claude provider — Messages API (`POST {base}/v1/messages`).
//!
//! Request shape (API version `2026-10-21` by default; configurable):
//!
//! ```json
//! { "model": "claude-opus-5-5", "max_tokens": 16000,
//!   "system": "...", "messages": [{"role": "user", "content": "..."}],
//!   "stop_sequences": [...],
//!   "output_config": {"effort": "low", "format": {"type": "json_schema", "schema": {...}}},
//!   "metadata": {"user_id": "..."},
//!   "fallbacks": "default" }
//! ```
//!
//! * No `temperature` / `top_p` / `top_k` (rejected on API version
//!   `2026-10-21` and by current models) and no `thinking` field: Claude Opus
//!   5.5 always thinks adaptively and is steered with `output_config.effort`
//!   (its default is `medium`).
//! * `fallbacks: "default"` (beta header `server-side-fallback-2026-07-01`)
//!   lets the API re-run a safety-classifier refusal on Anthropic's
//!   recommended fallback model inside the same call. Sent only for models
//!   that support it and only when `refusal_fallback` is on.
//! * Only `text` content blocks are returned; `thinking` / `fallback` blocks
//!   are skipped. A `refusal` stop reason is surfaced as
//!   [`AiStopReason::Refusal`].
//! * 429 / 5xx / 529 are retried a bounded number of times, honouring
//!   `retry-after` and `x-should-retry`.

use std::time::Duration;

use async_trait::async_trait;
use hos_core::IntegrationError;
use hos_core::integrations::ConnectionInfo;
use hos_core::integrations::ai::{
    AiEffort, AiProvider, AiRequest, AiResponse, AiStopReason, AiUsage,
};
use hos_core::integrations::kinds;
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::http::{self, RetryPolicy};

/// Default API host.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Default `anthropic-version` header.
pub const DEFAULT_API_VERSION: &str = "2026-10-21";
/// Beta header enabling `fallbacks`.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

const RETRY_STATUSES: &[u16] = &[429, 500, 502, 503, 529];

/// Whether `model` accepts `fallbacks: "default"`.
pub fn supports_server_fallback(model: &str) -> bool {
    matches!(
        model,
        "claude-opus-5-5"
            | "claude-opus-5"
            | "claude-sonnet-5-5"
            | "claude-fable-5-1"
            | "claude-fable-5-2"
            | "claude-mythos-5-1"
            | "claude-mythos-5-2"
    )
}

/// Provider configuration.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// API key (`x-api-key`).
    pub api_key: SecretString,
    /// API host, overridable for tests / gateways.
    pub base_url: String,
    /// Default model.
    pub model: String,
    /// `anthropic-version` header.
    pub api_version: String,
    /// Default effort when a request has none (`None` = model default).
    pub default_effort: Option<AiEffort>,
    /// Send `fallbacks: "default"` for refusal recovery (supported models only).
    pub refusal_fallback: bool,
    /// Whole-request timeout (generation with thinking can take minutes).
    pub timeout: Duration,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// Which destinations `base_url` may resolve to (default: the
    /// process-wide `HOS_OUTBOUND_POLICY`).
    pub outbound: OutboundPolicy,
}

impl AnthropicConfig {
    /// Configuration with defaults (`claude-opus-5-5`, 300 s timeout, 2 retries).
    pub fn new(api_key: impl Into<SecretString>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.into(),
            model: super::DEFAULT_MODEL.into(),
            api_version: DEFAULT_API_VERSION.into(),
            default_effort: None,
            refusal_fallback: true,
            timeout: Duration::from_secs(300),
            retry: RetryPolicy::default(),
            outbound: OutboundPolicy::global(),
        }
    }
}

/// [`AiProvider`] for the Anthropic Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    http: reqwest::Client,
    cfg: AnthropicConfig,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl AnthropicProvider {
    /// Creates the provider.
    pub fn new(cfg: AnthropicConfig) -> Result<Self, IntegrationError> {
        if cfg.api_key.is_empty() {
            return Err(IntegrationError::Config(
                "secrets.api_key is required".into(),
            ));
        }
        url::Url::parse(&cfg.base_url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| {
                IntegrationError::Config("config.base_url must be an http(s) URL".into())
            })?;
        let http = http::client_builder(cfg.outbound, &cfg.base_url)?
            .timeout(cfg.timeout)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("HostingOS/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| IntegrationError::Transport(e.to_string()))?;
        Ok(Self { http, cfg })
    }

    /// Platform default from the environment: `ANTHROPIC_API_KEY` and
    /// `HOS_AI_MODEL` (default `claude-opus-5-5`).
    pub fn from_env() -> Result<Self, IntegrationError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| IntegrationError::Config("ANTHROPIC_API_KEY is not set".into()))?;
        let mut cfg = AnthropicConfig::new(key.trim());
        cfg.model = super::default_model_from_env();
        Self::new(cfg)
    }

    /// The configuration.
    pub fn config(&self) -> &AnthropicConfig {
        &self.cfg
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.base_url.trim_end_matches('/'))
    }

    fn use_fallback(&self, model: &str) -> bool {
        self.cfg.refusal_fallback && supports_server_fallback(model)
    }

    /// The JSON body sent for `req` (exposed for logging / inspection).
    pub fn request_body(&self, req: &AiRequest) -> Value {
        let model = req
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| self.cfg.model.clone());
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| json!({"role": m.role.as_str(), "content": m.content}))
            .collect();
        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "messages": messages,
        });
        if let Some(system) = req.system.as_deref().filter(|s| !s.trim().is_empty()) {
            body["system"] = json!(system);
        }
        if !req.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(req.stop_sequences);
        }
        let mut output_config = Map::new();
        if let Some(effort) = req.effort.or(self.cfg.default_effort) {
            output_config.insert("effort".into(), json!(effort.as_str()));
        }
        if let Some(schema) = &req.output_schema {
            output_config.insert(
                "format".into(),
                json!({"type": "json_schema", "schema": schema}),
            );
        }
        if !output_config.is_empty() {
            body["output_config"] = Value::Object(output_config);
        }
        if let Some(uid) = req.user_id.as_deref().filter(|u| !u.is_empty()) {
            body["metadata"] = json!({"user_id": uid});
        }
        if self.use_fallback(&model) {
            body["fallbacks"] = json!("default");
        }
        body
    }

    fn request(&self, method: reqwest::Method, path: &str, beta: bool) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .request(method, self.url(path))
            .header("x-api-key", self.cfg.api_key.expose_str())
            .header("anthropic-version", &self.cfg.api_version)
            .header("content-type", "application/json");
        if beta {
            rb = rb.header("anthropic-beta", FALLBACK_BETA);
        }
        rb
    }

    async fn error_from(resp: reqwest::Response) -> IntegrationError {
        let status = resp.status().as_u16();
        let retry_after = http::retry_after(resp.headers());
        let header_request_id = resp
            .headers()
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let text = http::read_text(resp).await.unwrap_or_default();
        let (kind, message, request_id) = match serde_json::from_str::<ErrorEnvelope>(&text) {
            Ok(env) => (
                env.error.kind,
                env.error.message.unwrap_or_default(),
                env.request_id.or(header_request_id),
            ),
            Err(_) => (None, http::unparsed_error(status, &text), header_request_id),
        };
        let mut message = match &kind {
            Some(k) if !message.is_empty() => format!("{k}: {message}"),
            Some(k) => k.clone(),
            None => message,
        };
        if let Some(id) = request_id {
            message.push_str(&format!(" (request_id: {id})"));
        }
        http::map_status(status, kind.as_deref(), message, retry_after)
    }
}

#[async_trait]
impl AiProvider for AnthropicProvider {
    fn kind(&self) -> &'static str {
        kinds::ANTHROPIC
    }

    fn default_model(&self) -> &str {
        &self.cfg.model
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let resp = http::send_with_retry(
            &self.http,
            self.cfg.outbound,
            || self.request(reqwest::Method::GET, "/v1/models?limit=1", false),
            &RetryPolicy {
                max_retries: 0,
                ..self.cfg.retry
            },
            RETRY_STATUSES,
            kinds::ANTHROPIC,
        )
        .await?;
        if !resp.status().is_success() {
            return Err(Self::error_from(resp).await);
        }
        Ok(ConnectionInfo {
            server_version: Some(format!("anthropic-version {}", self.cfg.api_version)),
            details: Some(format!("default model {}", self.cfg.model)),
        })
    }

    async fn complete(&self, request: &AiRequest) -> Result<AiResponse, IntegrationError> {
        request.validate()?;
        let body = self.request_body(request);
        let beta = body.get("fallbacks").is_some();
        let resp = http::send_with_retry(
            &self.http,
            self.cfg.outbound,
            || {
                self.request(reqwest::Method::POST, "/v1/messages", beta)
                    .json(&body)
            },
            &self.cfg.retry,
            RETRY_STATUSES,
            kinds::ANTHROPIC,
        )
        .await?;
        if !resp.status().is_success() {
            return Err(Self::error_from(resp).await);
        }
        let text = http::read_text(resp).await?;
        let parsed: MessagesResponse = serde_json::from_str(&text).map_err(|e| {
            IntegrationError::Transport(format!("unparsable Messages API response: {e}"))
        })?;
        let content: String = parsed
            .content
            .iter()
            .filter(|b| b.kind == "text")
            .filter_map(|b| b.text.as_deref())
            .collect();
        let usage = parsed.usage.unwrap_or_default();
        let model = parsed
            .model
            .unwrap_or_else(|| body["model"].as_str().unwrap_or_default().to_owned());
        Ok(AiResponse {
            id: parsed.id,
            model,
            content,
            stop_reason: parsed
                .stop_reason
                .as_deref()
                .map_or(AiStopReason::Other, AiStopReason::from_provider),
            usage: AiUsage {
                input_tokens: usage.input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
                cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::integrations::ai::AiMessage;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn provider(server: &MockServer, max_retries: u32) -> AnthropicProvider {
        let mut cfg = AnthropicConfig::new("sk-ant-test");
        cfg.base_url = server.uri();
        cfg.outbound = OutboundPolicy::Unrestricted;
        cfg.retry = RetryPolicy {
            max_retries,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(20),
        };
        AnthropicProvider::new(cfg).unwrap()
    }

    fn ok_body(text: &str) -> Value {
        json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5-5",
            "content": [
                {"type": "thinking", "thinking": "", "signature": "sig"},
                {"type": "text", "text": text},
                {"type": "text", "text": "!"}
            ],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 12, "output_tokens": 34, "cache_creation_input_tokens": null, "cache_read_input_tokens": 5}
        })
    }

    #[tokio::test]
    async fn sends_current_request_shape_and_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", DEFAULT_API_VERSION))
            .and(header("anthropic-beta", FALLBACK_BETA))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "model": "claude-opus-5-5",
                "max_tokens": 1000,
                "system": "You are terse.",
                "messages": [{"role": "user", "content": "Hi"}],
                "output_config": {"effort": "low", "format": {"type": "json_schema", "schema": {"type": "object"}}},
                "metadata": {"user_id": "u-1"},
                "fallbacks": "default"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("Hello")))
            .expect(1)
            .mount(&server)
            .await;
        let p = provider(&server, 0);
        let mut req = AiRequest::new("Hi")
            .with_system("You are terse.")
            .with_effort(AiEffort::Low)
            .with_output_schema(json!({"type": "object"}));
        req.max_tokens = 1000;
        req.user_id = Some("u-1".into());
        let body = p.request_body(&req);
        for banned in ["temperature", "top_p", "top_k", "thinking"] {
            assert!(body.get(banned).is_none(), "{banned} must not be sent");
        }
        let resp = p.complete(&req).await.unwrap();
        assert_eq!(resp.content, "Hello!");
        assert_eq!(resp.id.as_deref(), Some("msg_01"));
        assert_eq!(resp.stop_reason, AiStopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 34);
        assert_eq!(resp.usage.cache_read_input_tokens, 5);
        assert_eq!(resp.usage.cache_creation_input_tokens, 0);
        assert_eq!(p.kind(), "anthropic");
        assert_eq!(p.default_model(), "claude-opus-5-5");
    }

    #[tokio::test]
    async fn no_fallback_for_unsupported_models_and_refusal_mapping() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_2", "model": "claude-haiku-5-5",
                "content": [], "stop_reason": "refusal",
                "stop_details": {"type": "refusal", "category": "cyber", "explanation": null},
                "usage": {"input_tokens": 3, "output_tokens": 0}
            })))
            .mount(&server)
            .await;
        let p = provider(&server, 0);
        let mut req = AiRequest::new("x");
        req.model = Some("claude-haiku-5-5".into());
        let body = p.request_body(&req);
        assert!(body.get("fallbacks").is_none());
        assert!(body.get("output_config").is_none());
        let resp = p.complete(&req).await.unwrap();
        assert_eq!(resp.stop_reason, AiStopReason::Refusal);
        assert!(!resp.stop_reason.is_complete());
        let received = server.received_requests().await.unwrap();
        assert!(received[0].headers.get("anthropic-beta").is_none());
    }

    struct Sequence {
        responses: std::sync::Mutex<Vec<ResponseTemplate>>,
    }

    impl Respond for Sequence {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let mut r = self.responses.lock().unwrap();
            if r.len() > 1 {
                r.remove(0)
            } else {
                r[0].clone()
            }
        }
    }

    fn seq(responses: Vec<ResponseTemplate>) -> Sequence {
        Sequence {
            responses: std::sync::Mutex::new(responses),
        }
    }

    fn err_body(kind: &str, msg: &str) -> Value {
        json!({"type": "error", "error": {"type": kind, "message": msg}, "request_id": "req_123"})
    }

    #[tokio::test]
    async fn retries_429_and_529_honouring_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(seq(vec![
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_json(err_body("rate_limit_error", "slow down")),
                ResponseTemplate::new(529)
                    .set_body_json(err_body("overloaded_error", "Overloaded")),
                ResponseTemplate::new(200).set_body_json(ok_body("ok")),
            ]))
            .expect(3)
            .mount(&server)
            .await;
        let resp = provider(&server, 2)
            .complete(&AiRequest::new("x"))
            .await
            .unwrap();
        assert_eq!(resp.content, "ok!");
    }

    #[tokio::test]
    async fn gives_up_after_bounded_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_json(err_body("rate_limit_error", "slow down")),
            )
            .expect(2)
            .mount(&server)
            .await;
        // retry-after (7 s) is capped by max_delay (20 ms) so the test stays fast.
        let err = provider(&server, 1)
            .complete(&AiRequest::new("x"))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            IntegrationError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn respects_x_should_retry_false() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(529)
                    .insert_header("x-should-retry", "false")
                    .set_body_json(err_body("overloaded_error", "Overloaded")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = provider(&server, 3)
            .complete(&AiRequest::new("x"))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            IntegrationError::Remote {
                status: Some(529),
                code: Some("overloaded_error".into()),
                message: "overloaded_error: Overloaded (request_id: req_123)".into()
            }
        );
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn maps_errors() {
        let cases = [
            (400, "invalid_request_error", "invalid_input"),
            (401, "authentication_error", "auth"),
            (403, "permission_error", "auth"),
            (404, "not_found_error", "not_found"),
            (413, "request_too_large", "invalid_input"),
            (402, "billing_error", "remote"),
            (500, "api_error", "remote"),
        ];
        for (status, kind, expected) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(err_body(kind, "boom")))
                .mount(&server)
                .await;
            let err = provider(&server, 0)
                .complete(&AiRequest::new("x"))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), expected, "HTTP {status}");
            assert!(
                err.to_string().contains("boom (request_id: req_123)"),
                "{err}"
            );
            assert!(!err.to_string().contains("sk-ant-test"));
        }
    }

    #[tokio::test]
    async fn rejects_prefill_and_bad_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let p = provider(&server, 0);
        let mut req = AiRequest::new("x");
        req.messages.push(AiMessage::assistant("{"));
        assert_eq!(p.complete(&req).await.unwrap_err().kind(), "invalid_input");
        assert_eq!(
            p.complete(&AiRequest::new("x")).await.unwrap_err().kind(),
            "transport"
        );
    }

    #[tokio::test]
    async fn test_connection_lists_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-ant-test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"id": "claude-opus-5-5"}], "has_more": true})),
            )
            .mount(&server)
            .await;
        let info = provider(&server, 0).test_connection().await.unwrap();
        assert_eq!(
            info.server_version.as_deref(),
            Some("anthropic-version 2026-10-21")
        );
    }
}
