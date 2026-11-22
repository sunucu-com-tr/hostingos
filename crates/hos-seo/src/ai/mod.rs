//! `hos_core::integrations::AiProvider` implementations and the integration
//! factory for the `ai` category.
//!
//! | kind | provider |
//! |---|---|
//! | `anthropic` ([`kinds::ANTHROPIC`]) | [`AnthropicProvider`] — Anthropic Messages API, default model `claude-opus-5-5` |
//! | `openai_compatible` ([`OPENAI_COMPATIBLE`]) | [`OpenAiCompatProvider`] — any OpenAI-compatible Chat Completions API |
//!
//! `config` / `secrets` keys are documented by [`descriptors`].

use std::sync::Arc;
use std::time::Duration;

use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::ai::{AiEffort, AiProvider};
use hos_core::integrations::{FieldReader, FieldSpec, IntegrationDescriptor, SelectOption, kinds};
pub use hos_core::net::OutboundPolicy;
use serde_json::Value;

pub mod anthropic;
mod http;
pub mod openai;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use http::RetryPolicy;
pub use openai::{MaxTokensParam, OpenAiCompatConfig, OpenAiCompatProvider, StructuredOutputMode};

/// `integrations.kind` of the OpenAI-compatible provider (not in
/// `hos_core::integrations::kinds` yet).
pub const OPENAI_COMPATIBLE: &str = "openai_compatible";

/// Default Claude model.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

/// `HOS_AI_MODEL` or [`DEFAULT_MODEL`].
pub fn default_model_from_env() -> String {
    std::env::var("HOS_AI_MODEL")
        .ok()
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

fn effort_options() -> Vec<SelectOption> {
    vec![
        SelectOption::new("low", "Low"),
        SelectOption::new("medium", "Medium"),
        SelectOption::new("high", "High"),
        SelectOption::new("xhigh", "Extra high"),
        SelectOption::new("max", "Max"),
    ]
}

/// Admin-form descriptors of the AI integrations.
pub fn descriptors() -> Vec<IntegrationDescriptor> {
    vec![
        IntegrationDescriptor::new(kinds::ANTHROPIC, "Anthropic Claude", IntegrationCategory::Ai)
            .description("Claude via the Anthropic Messages API (SEO meta, product copy, FAQ, blog, translation, support drafts).")
            .docs_url("https://platform.claude.com/docs/en/api/messages")
            .config_field(
                FieldSpec::text("model", "Model")
                    .default_value(DEFAULT_MODEL)
                    .placeholder(DEFAULT_MODEL)
                    .help("Model id, e.g. claude-opus-5-5. Empty = HOS_AI_MODEL or claude-opus-5-5."),
            )
            .config_field(
                FieldSpec::select("default_effort", "Default effort", effort_options())
                    .help("output_config.effort when a task does not set one (empty = model default)."),
            )
            .config_field(
                FieldSpec::boolean("refusal_fallback", "Refusal fallback")
                    .default_value(true)
                    .help("Send fallbacks: \"default\" so a safety refusal is retried on Anthropic's recommended fallback model."),
            )
            .config_field(
                FieldSpec::url("base_url", "API base URL")
                    .placeholder(anthropic::DEFAULT_BASE_URL)
                    .help("Only change for a gateway / proxy."),
            )
            .config_field(
                FieldSpec::text("api_version", "anthropic-version")
                    .default_value(anthropic::DEFAULT_API_VERSION),
            )
            .config_field(FieldSpec::number("timeout_secs", "Timeout (s)").default_value(300))
            .config_field(FieldSpec::number("max_retries", "Max retries").default_value(2))
            .secret_field(FieldSpec::password("api_key", "API key").required()),
        IntegrationDescriptor::new(OPENAI_COMPATIBLE, "OpenAI-compatible API", IntegrationCategory::Ai)
            .description("Any Chat Completions compatible endpoint (OpenAI, OpenRouter, Groq, Mistral, vLLM, Ollama …).")
            .docs_url("https://platform.openai.com/docs/api-reference/chat")
            .config_field(
                FieldSpec::url("base_url", "Base URL")
                    .required()
                    .placeholder("https://api.openai.com/v1")
                    .help("Without a path, /v1 is appended; /chat/completions is always appended."),
            )
            .config_field(FieldSpec::text("model", "Model").required())
            .config_field(
                FieldSpec::select(
                    "structured_output",
                    "JSON output mode",
                    vec![
                        SelectOption::new("json_schema", "JSON schema (strict)"),
                        SelectOption::new("json_object", "JSON object"),
                        SelectOption::new("none", "Prompt only"),
                    ],
                )
                .default_value("json_schema"),
            )
            .config_field(
                FieldSpec::select(
                    "max_tokens_param",
                    "Token limit parameter",
                    vec![
                        SelectOption::new("max_tokens", "max_tokens"),
                        SelectOption::new("max_completion_tokens", "max_completion_tokens"),
                        SelectOption::new("none", "Do not send"),
                    ],
                )
                .default_value("max_tokens"),
            )
            .config_field(FieldSpec::boolean("send_reasoning_effort", "Send reasoning_effort").default_value(false))
            .config_field(FieldSpec::number("timeout_secs", "Timeout (s)").default_value(300))
            .config_field(FieldSpec::number("max_retries", "Max retries").default_value(2))
            .secret_field(FieldSpec::password("api_key", "API key")),
    ]
}

fn retry_from(r: &FieldReader<'_>) -> Result<RetryPolicy, IntegrationError> {
    let max_retries = u32::try_from(r.u64_or("max_retries", 2)?.min(10)).unwrap_or(2);
    Ok(RetryPolicy {
        max_retries,
        ..RetryPolicy::default()
    })
}

fn timeout_from(r: &FieldReader<'_>) -> Result<Duration, IntegrationError> {
    Ok(Duration::from_secs(
        r.u64_or("timeout_secs", 300)?.clamp(5, 1800),
    ))
}

/// Builds an AI provider from `integrations` row data. A configured
/// `base_url` is checked against the process-wide outbound policy
/// (`HOS_OUTBOUND_POLICY`, see [`hos_core::net`]).
pub fn build(
    kind: &str,
    config: &Value,
    secrets: &Value,
) -> Result<Arc<dyn AiProvider>, IntegrationError> {
    build_with_policy(kind, config, secrets, OutboundPolicy::global())
}

/// [`build`] with an explicit outbound policy.
pub fn build_with_policy(
    kind: &str,
    config: &Value,
    secrets: &Value,
    policy: OutboundPolicy,
) -> Result<Arc<dyn AiProvider>, IntegrationError> {
    let descriptor = descriptors()
        .into_iter()
        .find(|d| d.kind == kind)
        .ok_or_else(|| IntegrationError::Config(format!("unknown AI integration kind {kind:?}")))?;
    descriptor.validate(config, secrets)?;
    let c = FieldReader::config(config);
    let s = FieldReader::secrets(secrets);
    match kind {
        k if k == kinds::ANTHROPIC => {
            let mut cfg = AnthropicConfig::new(s.secret("api_key")?);
            cfg.model = c
                .opt_str("model")?
                .map(str::to_owned)
                .unwrap_or_else(default_model_from_env);
            if let Some(url) = c.opt_url("base_url")? {
                cfg.base_url = url.to_string();
            }
            cfg.api_version = c.string_or("api_version", anthropic::DEFAULT_API_VERSION)?;
            cfg.default_effort = c
                .opt_str("default_effort")?
                .map(|e| e.parse::<AiEffort>())
                .transpose()
                .map_err(|_| {
                    IntegrationError::Config("config.default_effort has an invalid value".into())
                })?;
            cfg.refusal_fallback = c.bool_or("refusal_fallback", true)?;
            cfg.timeout = timeout_from(&c)?;
            cfg.retry = retry_from(&c)?;
            cfg.outbound = policy;
            Ok(Arc::new(AnthropicProvider::new(cfg)?))
        }
        _ => {
            let mut cfg = OpenAiCompatConfig::new(c.url("base_url")?.to_string(), c.str("model")?);
            cfg.api_key = s.opt_secret("api_key")?;
            cfg.structured_output = match c.string_or("structured_output", "json_schema")?.as_str()
            {
                "json_object" => StructuredOutputMode::JsonObject,
                "none" => StructuredOutputMode::None,
                _ => StructuredOutputMode::JsonSchema,
            };
            cfg.max_tokens_param = match c.string_or("max_tokens_param", "max_tokens")?.as_str() {
                "max_completion_tokens" => MaxTokensParam::MaxCompletionTokens,
                "none" => MaxTokensParam::None,
                _ => MaxTokensParam::MaxTokens,
            };
            cfg.send_reasoning_effort = c.bool_or("send_reasoning_effort", false)?;
            cfg.timeout = timeout_from(&c)?;
            cfg.retry = retry_from(&c)?;
            cfg.outbound = policy;
            Ok(Arc::new(OpenAiCompatProvider::new(cfg)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_anthropic() {
        let p = build(
            "anthropic",
            &json!({"model": "claude-sonnet-5-5", "default_effort": "low", "max_retries": "1"}),
            &json!({"api_key": "sk-ant-x"}),
        )
        .unwrap();
        assert_eq!(p.kind(), "anthropic");
        assert_eq!(p.default_model(), "claude-sonnet-5-5");
        let err = build("anthropic", &json!({}), &json!({})).err().unwrap();
        assert_eq!(
            err,
            IntegrationError::Config("secrets.api_key is required".into())
        );
        assert!(
            build(
                "anthropic",
                &json!({"default_effort": "extreme"}),
                &json!({"api_key": "k"})
            )
            .is_err()
        );
    }

    #[test]
    fn builds_openai_compatible() {
        let p = build(
            "openai_compatible",
            &json!({"base_url": "https://openrouter.ai/api/v1", "model": "meta/llama", "structured_output": "json_object"}),
            &json!({}),
        )
        .unwrap();
        assert_eq!(p.kind(), "openai_compatible");
        assert_eq!(p.default_model(), "meta/llama");
        assert!(build("openai_compatible", &json!({"model": "m"}), &json!({})).is_err());
        assert!(matches!(
            build("nope", &json!({}), &json!({})),
            Err(IntegrationError::Config(_))
        ));
    }

    #[test]
    fn descriptors_cover_both_kinds() {
        let d = descriptors();
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|x| x.category == IntegrationCategory::Ai));
        assert!(
            d[0].secret_fields
                .iter()
                .any(|f| f.key == "api_key" && f.required)
        );
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v[0]["kind"], "anthropic");
        assert_eq!(v[1]["kind"], "openai_compatible");
    }
}
