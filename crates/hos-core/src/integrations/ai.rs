//! AI text generation (SEO meta, product copy, ticket reply drafts).
//!
//! The default implementation (in `hos-seo`) calls the Anthropic Messages API
//! (`POST /v1/messages`) with model `HOS_AI_MODEL` (default `claude-opus-5-5`).
//! Notes for implementors of that provider, reflecting the current API:
//!
//! * Do not send `temperature`/`top_p`/`top_k` or `thinking.budget_tokens` —
//!   current Claude models reject them; that is why [`AiRequest`] has no
//!   sampling knobs. Map [`AiRequest::effort`] to `output_config.effort`.
//! * [`AiRequest::output_schema`] maps to structured outputs
//!   (`output_config.format` with a JSON schema).
//! * Always check `stop_reason` — a `refusal` comes back as HTTP 200 and must be
//!   surfaced as [`AiStopReason::Refusal`], not as content.
//! * Report `usage` (input/output and cache token counts) for `ai_generations`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ConnectionInfo;
use crate::error::IntegrationError;

/// Default `max_tokens` for non-streaming requests.
pub const DEFAULT_MAX_TOKENS: u32 = 16_000;

crate::string_enum! {
    /// Conversation role.
    pub enum AiRole {
        /// End user / application prompt.
        User => "user",
        /// Model output (for multi-turn context).
        Assistant => "assistant",
    }
}

/// One conversation turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiMessage {
    /// Role.
    pub role: AiRole,
    /// Text content.
    pub content: String,
}

impl AiMessage {
    /// A user turn.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: AiRole::User,
            content: content.into(),
        }
    }

    /// An assistant turn.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: AiRole::Assistant,
            content: content.into(),
        }
    }
}

crate::string_enum! {
    /// How much reasoning / token spend the model should invest
    /// (Anthropic `output_config.effort`).
    pub enum AiEffort {
        /// Fast and cheap (classification, short meta descriptions).
        Low => "low",
        /// Balanced.
        Medium => "medium",
        /// Thorough.
        High => "high",
        /// Very thorough.
        XHigh => "xhigh",
        /// Maximum.
        Max => "max",
    }
}

/// A text generation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiRequest {
    /// Model id; `None` = the provider's default (`HOS_AI_MODEL`).
    pub model: Option<String>,
    /// System prompt.
    pub system: Option<String>,
    /// Conversation; must start with a user turn and must not end with an
    /// assistant turn (prefill is not supported by current models).
    pub messages: Vec<AiMessage>,
    /// Output token cap.
    pub max_tokens: u32,
    /// Effort level; `None` = provider default.
    pub effort: Option<AiEffort>,
    /// Custom stop sequences.
    pub stop_sequences: Vec<String>,
    /// JSON schema the answer must conform to (structured output). When set,
    /// [`AiResponse::content`] is the JSON document as text.
    pub output_schema: Option<Value>,
    /// Opaque end-user id for abuse monitoring (never an email or name).
    pub user_id: Option<String>,
}

impl Default for AiRequest {
    fn default() -> Self {
        Self {
            model: None,
            system: None,
            messages: Vec::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            effort: None,
            stop_sequences: Vec::new(),
            output_schema: None,
            user_id: None,
        }
    }
}

impl AiRequest {
    /// Single-turn request with the default token cap.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![AiMessage::user(prompt)],
            ..Self::default()
        }
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets the effort.
    #[must_use]
    pub fn with_effort(mut self, effort: AiEffort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Requests structured JSON output matching `schema`.
    #[must_use]
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Basic shape checks (non-empty, starts with user, does not end with assistant, max_tokens > 0).
    pub fn validate(&self) -> Result<(), IntegrationError> {
        match (self.messages.first(), self.messages.last()) {
            (Some(first), Some(last))
                if first.role == AiRole::User && last.role == AiRole::User => {}
            _ => {
                return Err(IntegrationError::InvalidInput(
                    "AI request must start and end with a user message".into(),
                ));
            }
        }
        if self.max_tokens == 0 {
            return Err(IntegrationError::InvalidInput(
                "max_tokens must be > 0".into(),
            ));
        }
        Ok(())
    }
}

crate::string_enum! {
    /// Why generation stopped.
    pub enum AiStopReason {
        /// Natural end.
        EndTurn => "end_turn",
        /// Hit `max_tokens` — output is truncated.
        MaxTokens => "max_tokens",
        /// Hit a stop sequence.
        StopSequence => "stop_sequence",
        /// Model requested a tool (not used by HostingOS yet).
        ToolUse => "tool_use",
        /// Long-running turn paused; resend to continue.
        PauseTurn => "pause_turn",
        /// The model declined; `content` must not be used.
        Refusal => "refusal",
        /// Unrecognised reason.
        Other => "other",
    }
}

impl AiStopReason {
    /// Lenient mapping from a provider string (unknown → [`AiStopReason::Other`]).
    pub fn from_provider(value: &str) -> Self {
        value.parse().unwrap_or(Self::Other)
    }

    /// Whether `content` is a complete, usable answer.
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::EndTurn | Self::StopSequence)
    }
}

/// Token accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiUsage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens (including any hidden reasoning that is billed).
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_creation_input_tokens: u64,
}

/// A generation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiResponse {
    /// Provider response id.
    pub id: Option<String>,
    /// Model that actually answered.
    pub model: String,
    /// Concatenated text output.
    pub content: String,
    /// Stop reason.
    pub stop_reason: AiStopReason,
    /// Token usage.
    #[serde(default)]
    pub usage: AiUsage,
}

/// A text-generation provider. See the [module docs](self).
#[async_trait]
pub trait AiProvider: Send + Sync {
    /// The integration kind (e.g. [`kinds::ANTHROPIC`](super::kinds::ANTHROPIC)).
    fn kind(&self) -> &'static str;

    /// Model used when [`AiRequest::model`] is `None`.
    fn default_model(&self) -> &str;

    /// Verifies credentials (e.g. a models-list call). Default: `Unsupported`.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        Err(IntegrationError::Unsupported("test_connection"))
    }

    /// Generates a completion. A refusal is `Ok` with
    /// [`AiStopReason::Refusal`]; callers must check [`AiStopReason::is_complete`].
    async fn complete(&self, request: &AiRequest) -> Result<AiResponse, IntegrationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builders_and_validation() {
        let r = AiRequest::new("Write a meta description")
            .with_system("You are an SEO assistant")
            .with_effort(AiEffort::Low);
        assert_eq!(r.max_tokens, DEFAULT_MAX_TOKENS);
        r.validate().unwrap();
        let mut prefill = r.clone();
        prefill.messages.push(AiMessage::assistant("{"));
        assert!(prefill.validate().is_err());
        assert!(AiRequest::default().validate().is_err());
        assert_eq!(AiEffort::XHigh.as_str(), "xhigh");
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(
            AiStopReason::from_provider("end_turn"),
            AiStopReason::EndTurn
        );
        assert_eq!(
            AiStopReason::from_provider("refusal"),
            AiStopReason::Refusal
        );
        assert_eq!(
            AiStopReason::from_provider("something_new"),
            AiStopReason::Other
        );
        assert!(!AiStopReason::Refusal.is_complete());
        assert!(!AiStopReason::MaxTokens.is_complete());
    }
}
