//! Generic OpenAI-compatible Chat Completions provider
//! (`POST {base}/chat/completions`), for tenants that bring their own
//! provider (OpenAI, OpenRouter, Groq, Together, Mistral, vLLM, Ollama, …).
//!
//! Endpoint rule: a base URL without a path (`https://api.openai.com`) gets
//! `/v1/chat/completions`; a base URL with a path
//! (`https://openrouter.ai/api/v1`) gets `/chat/completions`.
//!
//! Structured output, the token-limit parameter name and `reasoning_effort`
//! differ between servers, so they are configurable.

use std::time::Duration;

use async_trait::async_trait;
use hos_core::IntegrationError;
use hos_core::integrations::ConnectionInfo;
use hos_core::integrations::ai::{
    AiEffort, AiProvider, AiRequest, AiResponse, AiRole, AiStopReason, AiUsage,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::OPENAI_COMPATIBLE;
use super::http::{self, RetryPolicy};

const RETRY_STATUSES: &[u16] = &[429, 500, 502, 503];

/// How to request JSON output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputMode {
    /// `response_format: {type: "json_schema", json_schema: {..., strict: true}}`.
    #[default]
    JsonSchema,
    /// `response_format: {type: "json_object"}`.
    JsonObject,
    /// Nothing (prompt-only; the caller validates).
    None,
}

/// Name of the output-token limit parameter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensParam {
    /// `max_tokens` (most compatible servers).
    #[default]
    MaxTokens,
    /// `max_completion_tokens` (current OpenAI models).
    MaxCompletionTokens,
    /// Do not send a limit.
    None,
}

/// Provider configuration.
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// Base URL (see the module docs for the path rule).
    pub base_url: String,
    /// Bearer token (optional for local servers).
    pub api_key: Option<SecretString>,
    /// Model id.
    pub model: String,
    /// JSON output mode.
    pub structured_output: StructuredOutputMode,
    /// Token limit parameter.
    pub max_tokens_param: MaxTokensParam,
    /// Send `reasoning_effort` (`low` / `medium` / `high`) from [`AiRequest::effort`].
    pub send_reasoning_effort: bool,
    /// Request timeout.
    pub timeout: Duration,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// Which destinations `base_url` may resolve to (default: the
    /// process-wide `HOS_OUTBOUND_POLICY`). Self-hosted models on `localhost`
    /// are refused under every environment policy; run them on a LAN address.
    pub outbound: OutboundPolicy,
}

impl OpenAiCompatConfig {
    /// Configuration with defaults.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            structured_output: StructuredOutputMode::default(),
            max_tokens_param: MaxTokensParam::default(),
            send_reasoning_effort: false,
            timeout: Duration::from_secs(300),
            retry: RetryPolicy::default(),
            outbound: OutboundPolicy::global(),
        }
    }
}

/// [`AiProvider`] for OpenAI-compatible Chat Completions APIs.
#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    http: reqwest::Client,
    cfg: OpenAiCompatConfig,
    base: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<ChatMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    refusal: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

impl OpenAiCompatProvider {
    /// Creates the provider.
    pub fn new(cfg: OpenAiCompatConfig) -> Result<Self, IntegrationError> {
        let parsed = url::Url::parse(cfg.base_url.trim())
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
            .ok_or_else(|| {
                IntegrationError::Config("config.base_url must be an http(s) URL".into())
            })?;
        if cfg.model.trim().is_empty() {
            return Err(IntegrationError::Config("config.model is required".into()));
        }
        let trimmed = cfg.base_url.trim().trim_end_matches('/').to_owned();
        let base = if parsed.path().trim_matches('/').is_empty() {
            format!("{trimmed}/v1")
        } else {
            trimmed
        };
        let http = http::client_builder(cfg.outbound, &base)?
            .timeout(cfg.timeout)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("HostingOS/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| IntegrationError::Transport(e.to_string()))?;
        Ok(Self { http, cfg, base })
    }

    /// Full chat-completions URL.
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base)
    }

    /// The JSON body sent for `req`.
    pub fn request_body(&self, req: &AiRequest) -> Value {
        let model = req
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| self.cfg.model.clone());
        let mut messages = Vec::with_capacity(req.messages.len() + 1);
        if let Some(system) = req.system.as_deref().filter(|s| !s.trim().is_empty()) {
            messages.push(json!({"role": "system", "content": system}));
        }
        for m in &req.messages {
            let role = match m.role {
                AiRole::User => "user",
                AiRole::Assistant => "assistant",
            };
            messages.push(json!({"role": role, "content": m.content}));
        }
        let mut body = json!({"model": model, "messages": messages});
        match self.cfg.max_tokens_param {
            MaxTokensParam::MaxTokens => body["max_tokens"] = json!(req.max_tokens),
            MaxTokensParam::MaxCompletionTokens => {
                body["max_completion_tokens"] = json!(req.max_tokens);
            }
            MaxTokensParam::None => {}
        }
        if !req.stop_sequences.is_empty() {
            body["stop"] = json!(req.stop_sequences.iter().take(4).collect::<Vec<_>>());
        }
        if let Some(schema) = &req.output_schema {
            match self.cfg.structured_output {
                StructuredOutputMode::JsonSchema => {
                    body["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": {"name": "output", "schema": schema, "strict": true}
                    });
                }
                StructuredOutputMode::JsonObject => {
                    body["response_format"] = json!({"type": "json_object"});
                }
                StructuredOutputMode::None => {}
            }
        }
        if self.cfg.send_reasoning_effort
            && let Some(effort) = req.effort
        {
            let e = match effort {
                AiEffort::Low => "low",
                AiEffort::Medium => "medium",
                AiEffort::High | AiEffort::XHigh | AiEffort::Max => "high",
            };
            body["reasoning_effort"] = json!(e);
        }
        if let Some(uid) = req.user_id.as_deref().filter(|u| !u.is_empty()) {
            body["user"] = json!(uid);
        }
        body
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, url);
        if let Some(key) = self.cfg.api_key.as_ref().filter(|k| !k.is_empty()) {
            rb = rb.bearer_auth(key.expose_str());
        }
        rb
    }

    async fn error_from(resp: reqwest::Response) -> IntegrationError {
        let status = resp.status().as_u16();
        let retry_after = http::retry_after(resp.headers());
        let text = http::read_text(resp).await.unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let err = parsed.as_ref().and_then(|v| v.get("error"));
        let message = err
            .and_then(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .or_else(|| e.as_str())
            })
            .map(str::to_owned)
            .unwrap_or_else(|| http::unparsed_error(status, &text));
        let code = err.and_then(|e| {
            e.get("code")
                .and_then(Value::as_str)
                .or_else(|| e.get("type").and_then(Value::as_str))
        });
        http::map_status(status, code, message, retry_after)
    }
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

#[async_trait]
impl AiProvider for OpenAiCompatProvider {
    fn kind(&self) -> &'static str {
        OPENAI_COMPATIBLE
    }

    fn default_model(&self) -> &str {
        &self.cfg.model
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let url = format!("{}/models", self.base);
        let resp = http::send(
            &self.http,
            self.cfg.outbound,
            self.request(reqwest::Method::GET, &url),
        )
        .await?;
        if !resp.status().is_success() {
            return Err(Self::error_from(resp).await);
        }
        Ok(ConnectionInfo {
            server_version: None,
            details: Some(format!("model {}", self.cfg.model)),
        })
    }

    async fn complete(&self, request: &AiRequest) -> Result<AiResponse, IntegrationError> {
        request.validate()?;
        let body = self.request_body(request);
        let url = self.endpoint();
        let resp = http::send_with_retry(
            &self.http,
            self.cfg.outbound,
            || self.request(reqwest::Method::POST, &url).json(&body),
            &self.cfg.retry,
            RETRY_STATUSES,
            OPENAI_COMPATIBLE,
        )
        .await?;
        if !resp.status().is_success() {
            return Err(Self::error_from(resp).await);
        }
        let text = http::read_text(resp).await?;
        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
            IntegrationError::Transport(format!("unparsable chat completion response: {e}"))
        })?;
        let choice = parsed.choices.into_iter().next();
        let (content, refusal, finish) = match choice {
            Some(c) => {
                let msg = c.message;
                (
                    content_text(msg.as_ref().and_then(|m| m.content.as_ref())),
                    msg.and_then(|m| m.refusal).filter(|r| !r.is_empty()),
                    c.finish_reason,
                )
            }
            None => (String::new(), None, None),
        };
        let stop_reason = if refusal.is_some() {
            AiStopReason::Refusal
        } else {
            match finish.as_deref() {
                Some("stop") => AiStopReason::EndTurn,
                Some("length") => AiStopReason::MaxTokens,
                Some("content_filter") => AiStopReason::Refusal,
                Some("tool_calls" | "function_call") => AiStopReason::ToolUse,
                _ => AiStopReason::Other,
            }
        };
        let usage = parsed.usage.unwrap_or_default();
        Ok(AiResponse {
            id: parsed.id,
            model: parsed
                .model
                .unwrap_or_else(|| body["model"].as_str().unwrap_or_default().to_owned()),
            content: if stop_reason == AiStopReason::Refusal {
                String::new()
            } else {
                content
            },
            stop_reason,
            usage: AiUsage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.completion_tokens.unwrap_or(0),
                cache_read_input_tokens: usage
                    .prompt_tokens_details
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0),
                cache_creation_input_tokens: 0,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(server: &MockServer, base_path: &str) -> OpenAiCompatConfig {
        let mut c = OpenAiCompatConfig::new(format!("{}{base_path}", server.uri()), "gpt-test");
        c.outbound = OutboundPolicy::Unrestricted;
        c.api_key = Some("sk-test".into());
        c.retry = RetryPolicy {
            max_retries: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        };
        c
    }

    #[test]
    fn endpoint_rule() {
        let a = OpenAiCompatProvider::new(OpenAiCompatConfig::new("https://api.openai.com", "m"))
            .unwrap();
        assert_eq!(a.endpoint(), "https://api.openai.com/v1/chat/completions");
        let b = OpenAiCompatProvider::new(OpenAiCompatConfig::new(
            "https://openrouter.ai/api/v1/",
            "m",
        ))
        .unwrap();
        assert_eq!(
            b.endpoint(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert!(OpenAiCompatProvider::new(OpenAiCompatConfig::new("ftp://x", "m")).is_err());
        assert!(OpenAiCompatProvider::new(OpenAiCompatConfig::new("https://x", " ")).is_err());
    }

    #[tokio::test]
    async fn request_and_response_mapping() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_partial_json(json!({
                "model": "gpt-test",
                "messages": [{"role": "system", "content": "S"}, {"role": "user", "content": "U"}],
                "max_completion_tokens": 500,
                "response_format": {"type": "json_schema", "json_schema": {"name": "output", "strict": true}},
                "reasoning_effort": "high"
            })))
            .respond_with(
                ResponseTemplate::new(429).insert_header("retry-after-ms", "1"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-1", "model": "gpt-test-2025",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "{\"a\":1}", "refusal": null}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4, "prompt_tokens_details": {"cached_tokens": 2}}
            })))
            .mount(&server)
            .await;
        let mut c = cfg(&server, "");
        c.max_tokens_param = MaxTokensParam::MaxCompletionTokens;
        c.send_reasoning_effort = true;
        let p = OpenAiCompatProvider::new(c).unwrap();
        let mut req = AiRequest::new("U")
            .with_system("S")
            .with_effort(AiEffort::XHigh)
            .with_output_schema(json!({"type": "object"}));
        req.max_tokens = 500;
        let resp = p.complete(&req).await.unwrap();
        assert_eq!(resp.content, "{\"a\":1}");
        assert_eq!(resp.model, "gpt-test-2025");
        assert_eq!(resp.stop_reason, AiStopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.cache_read_input_tokens, 2);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert_eq!(p.kind(), "openai_compatible");
    }

    #[tokio::test]
    async fn finish_reasons_and_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": [{"type": "text", "text": "par"}, {"type": "text", "text": "tial"}]}, "finish_reason": "length"}]
            })))
            .mount(&server)
            .await;
        let p = OpenAiCompatProvider::new(cfg(&server, "/api/v1")).unwrap();
        let resp = p.complete(&AiRequest::new("x")).await.unwrap();
        assert_eq!(resp.content, "partial");
        assert_eq!(resp.stop_reason, AiStopReason::MaxTokens);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": null, "refusal": "I can't help with that."}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;
        let p = OpenAiCompatProvider::new(cfg(&server, "")).unwrap();
        assert_eq!(
            p.complete(&AiRequest::new("x")).await.unwrap().stop_reason,
            AiStopReason::Refusal
        );

        for (status, kind) in [
            (401, "auth"),
            (400, "invalid_input"),
            (404, "not_found"),
            (503, "remote"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(
                    json!({"error": {"message": "nope", "type": "x_error", "code": "x_code"}}),
                ))
                .mount(&server)
                .await;
            let p = OpenAiCompatProvider::new(cfg(&server, "")).unwrap();
            let err = p.complete(&AiRequest::new("x")).await.unwrap_err();
            assert_eq!(err.kind(), kind, "{status}");
            assert!(err.to_string().contains("nope"));
        }
    }

    #[test]
    fn base_url_is_checked_against_the_outbound_policy() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://2130706433/v1",
            "http://[::ffff:169.254.169.254]/v1",
            "http://169.254.169.254/latest",
            "http://localhost:8000",
        ] {
            for policy in [OutboundPolicy::Private, OutboundPolicy::Public] {
                let built = crate::ai::build_with_policy(
                    OPENAI_COMPATIBLE,
                    &json!({"base_url": url, "model": "m"}),
                    &json!({}),
                    policy,
                );
                match built {
                    Err(IntegrationError::Config(m)) => {
                        assert!(m.contains("HOS_OUTBOUND_POLICY"), "{url}: {m}")
                    }
                    Err(other) => panic!("{url}: expected Config, got {other:?}"),
                    Ok(_) => panic!("{url}: built under {policy}"),
                }
            }
        }
        let lan = json!({"base_url": "http://10.1.2.3:8000/v1", "model": "m"});
        assert!(
            crate::ai::build_with_policy(
                OPENAI_COMPATIBLE,
                &lan,
                &json!({}),
                OutboundPolicy::Private
            )
            .is_ok()
        );
        assert!(
            crate::ai::build_with_policy(
                OPENAI_COMPATIBLE,
                &lan,
                &json!({}),
                OutboundPolicy::Public
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn raw_bodies_are_not_quoted_and_redirects_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(502).set_body_string("<html>SECRET-INTERNAL-PAGE</html>"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/internal", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&server)
            .await;
        let mut c = cfg(&server, "/v1");
        c.retry.max_retries = 0;
        let p = OpenAiCompatProvider::new(c).unwrap();
        let err = p.complete(&AiRequest::new("hi")).await.unwrap_err();
        assert!(!err.to_string().contains("SECRET-INTERNAL-PAGE"), "{err}");
        assert!(err.to_string().contains("502"), "{err}");
        assert!(p.test_connection().await.is_err());
    }
}
