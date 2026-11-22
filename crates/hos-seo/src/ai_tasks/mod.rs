//! High-level AI content tasks with structured, strictly validated output.
//!
//! Every task:
//!
//! 1. builds a carefully scoped system prompt (role, task rules, output
//!    language, plus [common rules](prompts::COMMON_RULES): untrusted-input
//!    handling, facts-only, JSON-only);
//! 2. puts tenant / customer content inside nonce-suffixed
//!    `<untrusted_…>` elements (prompt-injection fencing);
//! 3. asks for JSON (structured outputs via `AiRequest::output_schema` when
//!    enabled) and parses it with serde (`deny_unknown_fields`);
//! 4. validates it (lengths, counts, allowed URLs, and a *number guard*: numbers
//!    and amounts in the output must appear in the supplied facts / content);
//! 5. on a parse or validation failure makes **one** repair attempt, sending the
//!    previous reply and the exact problem back to the model.
//!
//! Refusals ([`AiTaskError::Refused`]) and truncated output
//! ([`AiTaskError::Incomplete`]) are never repaired.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use chrono::{Datelike, Utc};
use hos_core::IntegrationError;
use hos_core::domain::Locale;
use hos_core::integrations::ai::{
    AiEffort, AiMessage, AiProvider, AiRequest, AiStopReason, AiUsage, DEFAULT_MAX_TOKENS,
};
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::jsonld::FaqEntry;
use crate::llms::{cycle_label, format_money};
use crate::markdown::{MarkdownOptions, html_to_markdown};
use crate::meta;
use crate::text;

mod prompts;
mod types;

use prompts::Fence;
pub use types::*;

/// Options shared by all tasks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskOptions {
    /// Model override (`None` = provider default).
    pub model: Option<String>,
    /// Effort override (`None` = per-task default).
    pub effort: Option<AiEffort>,
    /// Do not send `output_schema` (for providers without structured outputs);
    /// the schema is then included in the prompt instead.
    pub disable_structured_output: bool,
    /// Opaque end-user id forwarded to the provider.
    pub user_id: Option<String>,
    /// Output token cap override (default 16 000).
    pub max_tokens: Option<u32>,
}

/// A task result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutput<T> {
    /// Validated value.
    pub value: T,
    /// Model that produced it.
    pub model: String,
    /// Token usage summed over all attempts (store in `ai_generations`).
    pub usage: AiUsage,
    /// Number of provider calls (1 or 2).
    pub attempts: u8,
    /// Whether the repair attempt was needed.
    pub repaired: bool,
}

/// Task failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AiTaskError {
    /// Provider / transport error.
    #[error(transparent)]
    Provider(#[from] IntegrationError),
    /// The model declined (safety refusal); do not retry automatically.
    #[error("the AI model declined the request")]
    Refused,
    /// Output was cut off or the turn did not finish (`max_tokens`, `pause_turn`, …).
    #[error("the AI output is incomplete (stop reason: {0})")]
    Incomplete(String),
    /// Output was still invalid after the repair attempt.
    #[error("the AI output is invalid: {0}")]
    InvalidOutput(String),
    /// Bad task input.
    #[error("invalid task input: {0}")]
    InvalidInput(String),
}

struct Spec {
    system: String,
    user: String,
    schema: Value,
    effort: AiEffort,
}

/// AI task runner over any [`AiProvider`].
#[derive(Clone)]
pub struct AiTasks {
    provider: Arc<dyn AiProvider>,
    options: TaskOptions,
}

impl std::fmt::Debug for AiTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiTasks")
            .field("provider", &self.provider.kind())
            .field("options", &self.options)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

fn re(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

static NUM: OnceLock<Option<Regex>> = OnceLock::new();
static LIST_MARKER: OnceLock<Option<Regex>> = OnceLock::new();
static URL_RE: OnceLock<Option<Regex>> = OnceLock::new();
static MONEY_RE: OnceLock<Option<Regex>> = OnceLock::new();
static MD_LINK: OnceLock<Option<Regex>> = OnceLock::new();

fn normalize_number(token: &str) -> String {
    let digits: String = token.chars().filter(char::is_ascii_digit).collect();
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.to_owned()
    }
}

fn strip_noise(s: &str) -> String {
    let s = re(&URL_RE, r"https?://[^\s)\]>]+")
        .map_or_else(|| s.to_owned(), |r| r.replace_all(s, " ").into_owned());
    re(&LIST_MARKER, r"(?m)^\s*\d+[.)]\s")
        .map_or(s.clone(), |r| r.replace_all(&s, " ").into_owned())
}

fn number_set(sources: &[&str]) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(r) = re(&NUM, r"\d+(?:[.,]\d+)*") {
        for s in sources {
            for m in r.find_iter(s) {
                set.insert(normalize_number(m.as_str()));
            }
        }
    }
    set
}

/// Numbers in `output` that appear in none of `sources` (small numbers ≤ 10
/// and the current year ± 1 are always allowed; URLs and list markers are ignored).
fn unsupported_numbers(output: &str, sources: &[&str]) -> Vec<String> {
    let allowed = number_set(sources);
    let year = i64::from(Utc::now().year());
    let mut bad = Vec::new();
    let cleaned = strip_noise(output);
    if let Some(r) = re(&NUM, r"\d+(?:[.,]\d+)*") {
        for m in r.find_iter(&cleaned) {
            let token = m.as_str().trim_end_matches(['.', ',']);
            let n = normalize_number(token);
            let small = n.len() <= 2
                && n.parse::<u32>().is_ok_and(|v| v <= 10)
                && !token.contains([',', '.']);
            let is_year = n.len() == 4
                && n.parse::<i64>()
                    .is_ok_and(|y| (year - 1..=year + 1).contains(&y));
            if !(small || is_year || allowed.contains(&n)) && !bad.contains(&token.to_owned()) {
                bad.push(token.to_owned());
            }
        }
    }
    bad
}

/// Currency amounts in `output` that appear in none of `sources`.
fn unsupported_amounts(output: &str, sources: &[&str]) -> Vec<String> {
    let allowed = number_set(sources);
    let Some(r) = re(
        &MONEY_RE,
        r"(?i)(?:[$€£₺]\s?(\d+(?:[.,]\d+)*)|(\d+(?:[.,]\d+)*)\s?(?:tl\b|try\b|usd\b|eur\b|gbp\b|[$€£₺]|lira|dolar|euro))",
    ) else {
        return Vec::new();
    };
    let mut bad = Vec::new();
    for c in r.captures_iter(output) {
        let token = c
            .get(1)
            .or_else(|| c.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        if !token.is_empty() && !allowed.contains(&normalize_number(token)) {
            bad.push(c[0].trim().to_owned());
        }
    }
    bad
}

fn guard_numbers(output: &str, sources: &[&str]) -> Result<(), String> {
    let bad = unsupported_numbers(output, sources);
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "these numbers do not appear in the supplied facts or content and must be removed or corrected: {}",
            bad.join(", ")
        ))
    }
}

fn guard_amounts(output: &str, sources: &[&str]) -> Result<(), String> {
    let bad = unsupported_amounts(output, sources);
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "these prices do not appear in the supplied facts and must be removed: {}",
            bad.join(", ")
        ))
    }
}

fn markdown_links(md: &str) -> Vec<String> {
    re(&MD_LINK, r"\]\(\s*<?([^)\s>]+)>?(?:\s+[^)]*)?\)")
        .map(|r| r.captures_iter(md).map(|c| c[1].to_owned()).collect())
        .unwrap_or_default()
}

fn guard_links(md: &str, allowed: &[&str]) -> Result<(), String> {
    let bad: Vec<String> = markdown_links(md)
        .into_iter()
        .filter(|l| !allowed.contains(&l.as_str()))
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "these links are not in the allowed link list and must be removed: {}",
            bad.join(", ")
        ))
    }
}

fn no_h1(md: &str, field: &str) -> Result<(), String> {
    if md.lines().any(|l| l.trim_start().starts_with("# ")) {
        Err(format!("{field} must not contain an H1 (\"# \") heading"))
    } else {
        Ok(())
    }
}

fn non_empty(v: &str, field: &str) -> Result<(), String> {
    if v.trim().is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn max_len(v: &str, max: usize, field: &str) -> Result<(), String> {
    let n = text::grapheme_len(v.trim());
    if n > max {
        Err(format!(
            "{field} is {n} characters; it must be at most {max}"
        ))
    } else {
        Ok(())
    }
}

fn check_meta(m: &mut MetaSuggestion, sources: &[&str], prefix: &str) -> Result<(), String> {
    m.title = text::normalize_ws(&m.title);
    m.description = text::normalize_ws(&m.description);
    non_empty(&m.title, &format!("{prefix}title"))?;
    non_empty(&m.description, &format!("{prefix}description"))?;
    max_len(&m.title, 70, &format!("{prefix}title (target ≤ 60)"))?;
    max_len(
        &m.description,
        200,
        &format!("{prefix}description (target 120–155)"),
    )?;
    if text::grapheme_len(&m.description) < 50 {
        return Err(format!(
            "{prefix}description is too short; write 120–155 characters"
        ));
    }
    guard_numbers(&format!("{} {}", m.title, m.description), sources)?;
    m.title = meta::truncate(&m.title, meta::TITLE_MAX);
    m.description = meta::truncate(&m.description, meta::DESCRIPTION_MAX);
    Ok(())
}

fn finish_slug(slug: &str, fallback: &str, max_words: usize) -> Result<String, String> {
    let base = if text::slugify(slug).is_empty() {
        fallback
    } else {
        slug
    };
    let s: Vec<String> = text::slugify(base)
        .split('-')
        .filter(|p| !p.is_empty())
        .take(max_words)
        .map(str::to_owned)
        .collect();
    if s.is_empty() {
        Err("slug must contain at least one ASCII letter or digit".into())
    } else {
        Ok(s.join("-"))
    }
}

/// Extracts the JSON object from a reply (tolerates code fences / prose).
fn extract_json(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| &s[start..=end])
}

fn render_facts(facts: &[Fact]) -> String {
    facts
        .iter()
        .filter(|f| !f.label.trim().is_empty() || !f.value.trim().is_empty())
        .map(|f| {
            format!(
                "- {}: {}",
                text::normalize_ws(&f.label),
                text::normalize_ws(&f.value)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_links(links: &[LinkRef]) -> String {
    links
        .iter()
        .filter_map(|l| {
            crate::escape::safe_http_url(&l.url)
                .map(|u| format!("- {} — {u}", text::normalize_ws(&l.title)))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn locale_name(l: Locale) -> &'static str {
    match l {
        Locale::Tr => "Turkish",
        Locale::En => "English",
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

impl AiTasks {
    /// Runner with default options.
    pub fn new(provider: Arc<dyn AiProvider>) -> Self {
        Self::with_options(provider, TaskOptions::default())
    }

    /// Runner with options.
    pub fn with_options(provider: Arc<dyn AiProvider>, options: TaskOptions) -> Self {
        Self { provider, options }
    }

    async fn run<T, F>(&self, spec: Spec, check: F) -> Result<TaskOutput<T>, AiTaskError>
    where
        T: DeserializeOwned,
        F: Fn(&mut T) -> Result<(), String>,
    {
        let structured = !self.options.disable_structured_output;
        let mut user = spec.user;
        if !structured {
            user.push_str("\n\nReply with JSON that matches this JSON Schema:\n");
            user.push_str(&spec.schema.to_string());
        }
        let mut req = AiRequest {
            model: self.options.model.clone(),
            system: Some(spec.system),
            messages: vec![AiMessage::user(user)],
            max_tokens: self.options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            effort: Some(self.options.effort.unwrap_or(spec.effort)),
            stop_sequences: Vec::new(),
            output_schema: structured.then_some(spec.schema),
            user_id: self.options.user_id.clone(),
        };
        let mut usage = AiUsage::default();
        for attempt in 1..=2u8 {
            let resp = self.provider.complete(&req).await?;
            usage.input_tokens += resp.usage.input_tokens;
            usage.output_tokens += resp.usage.output_tokens;
            usage.cache_read_input_tokens += resp.usage.cache_read_input_tokens;
            usage.cache_creation_input_tokens += resp.usage.cache_creation_input_tokens;
            match resp.stop_reason {
                AiStopReason::Refusal => return Err(AiTaskError::Refused),
                AiStopReason::EndTurn | AiStopReason::StopSequence => {}
                other => return Err(AiTaskError::Incomplete(other.as_str().to_owned())),
            }
            let parsed = extract_json(&resp.content)
                .ok_or_else(|| "the reply does not contain a JSON object".to_owned())
                .and_then(|j| {
                    serde_json::from_str::<T>(j)
                        .map_err(|e| format!("the JSON does not match the schema: {e}"))
                })
                .and_then(|mut v| check(&mut v).map(|()| v));
            match parsed {
                Ok(value) => {
                    return Ok(TaskOutput {
                        value,
                        model: resp.model,
                        usage,
                        attempts: attempt,
                        repaired: attempt > 1,
                    });
                }
                Err(problem) if attempt == 1 => {
                    tracing::info!(problem = %problem, "AI output invalid; attempting one repair");
                    let previous = if resp.content.trim().is_empty() {
                        "(empty reply)".to_owned()
                    } else {
                        resp.content
                    };
                    req.messages.push(AiMessage::assistant(previous));
                    req.messages.push(AiMessage::user(format!(
                        "Your previous reply cannot be used: {problem}.\n\nReply again with only the corrected JSON object for the same task, matching the schema. Keep everything that was valid, fix only this problem, and do not add facts that were not supplied."
                    )));
                }
                Err(problem) => return Err(AiTaskError::InvalidOutput(problem)),
            }
        }
        Err(AiTaskError::InvalidOutput("no valid output".into()))
    }

    // -----------------------------------------------------------------------
    // Tasks
    // -----------------------------------------------------------------------

    /// Title + meta description for a page (text or HTML content).
    pub async fn generate_meta(
        &self,
        page_content: &str,
        locale: Locale,
        keywords: &[String],
    ) -> Result<TaskOutput<MetaSuggestion>, AiTaskError> {
        let content = as_plain_markdown(page_content);
        if content.trim().is_empty() {
            return Err(AiTaskError::InvalidInput("page content is empty".into()));
        }
        let fence = Fence::new();
        let kw = keywords
            .iter()
            .map(|k| text::normalize_ws(k))
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let user = format!(
            "Write the title and meta description for the page below.\n\nKeywords (most important first):\n{}\n\nPage content:\n{}",
            fence.wrap("keywords", if kw.is_empty() { "(none)" } else { &kw }),
            fence.wrap("page_content", &content)
        );
        let sources = [content.as_str(), kw.as_str()];
        self.run(
            Spec {
                system: prompts::system(prompts::META, locale),
                user,
                schema: meta_schema(),
                effort: AiEffort::Low,
            },
            |m: &mut MetaSuggestion| check_meta(m, &sources, ""),
        )
        .await
    }

    /// Product copy from facts only.
    pub async fn generate_product_description(
        &self,
        facts: &ProductFacts,
        tone: Tone,
        locale: Locale,
    ) -> Result<TaskOutput<ProductCopy>, AiTaskError> {
        if facts.name.trim().is_empty() {
            return Err(AiTaskError::InvalidInput("product name is required".into()));
        }
        let mut lines = vec![format!("- Product: {}", text::normalize_ws(&facts.name))];
        if !facts.brand.trim().is_empty() {
            lines.push(format!("- Brand: {}", text::normalize_ws(&facts.brand)));
        }
        if let Some(t) = facts.product_type {
            lines.push(format!("- Product type: {}", t.as_str()));
        }
        if let Some(a) = facts.audience.as_deref().filter(|a| !a.trim().is_empty()) {
            lines.push(format!("- Audience: {}", text::normalize_ws(a)));
        }
        for p in &facts.prices {
            let mut line = format!(
                "- Price ({}): {}",
                cycle_label(p.cycle, locale),
                format_money(&p.price, locale)
            );
            if let Some(fee) = p.setup_fee.as_ref().filter(|f| f.is_positive()) {
                line.push_str(&format!(", setup fee {}", format_money(fee, locale)));
            }
            lines.push(line);
        }
        let facts_text = format!("{}\n{}", lines.join("\n"), render_facts(&facts.facts));
        let fence = Fence::new();
        let user = format!(
            "Write the product copy. Tone: {}.\n\nFacts:\n{}",
            tone.describe(),
            fence.wrap("facts", &facts_text)
        );
        let sources = [facts_text.as_str()];
        self.run(
            Spec {
                system: prompts::system(prompts::PRODUCT, locale),
                user,
                schema: product_schema(),
                effort: AiEffort::Medium,
            },
            |c: &mut ProductCopy| {
                c.headline = text::normalize_ws(&c.headline);
                c.short_description = text::normalize_ws(&c.short_description);
                non_empty(&c.headline, "headline")?;
                max_len(&c.headline, 120, "headline (target ≤ 80)")?;
                non_empty(&c.short_description, "short_description")?;
                max_len(
                    &c.short_description,
                    320,
                    "short_description (target ≤ 300)",
                )?;
                non_empty(&c.long_description_markdown, "long_description_markdown")?;
                no_h1(&c.long_description_markdown, "long_description_markdown")?;
                if !markdown_links(&c.long_description_markdown).is_empty() {
                    return Err("long_description_markdown must not contain links".into());
                }
                c.bullet_points = c
                    .bullet_points
                    .iter()
                    .map(|b| {
                        text::normalize_ws(b)
                            .trim_end_matches(['.', ';', ','])
                            .to_owned()
                    })
                    .filter(|b| !b.is_empty())
                    .collect();
                if c.bullet_points.len() < 3 {
                    return Err("bullet_points needs 3–8 items".into());
                }
                c.bullet_points.truncate(8);
                let all = format!(
                    "{}\n{}\n{}\n{}",
                    c.headline,
                    c.short_description,
                    c.long_description_markdown,
                    c.bullet_points.join("\n")
                );
                guard_numbers(&all, &sources)
            },
        )
        .await
    }

    /// Up to `n` FAQ entries about a topic / product.
    pub async fn generate_faq(
        &self,
        topic: &FaqTopic,
        n: usize,
        locale: Locale,
    ) -> Result<TaskOutput<Vec<FaqEntry>>, AiTaskError> {
        if !(1..=20).contains(&n) {
            return Err(AiTaskError::InvalidInput(
                "n must be between 1 and 20".into(),
            ));
        }
        if topic.topic.trim().is_empty() {
            return Err(AiTaskError::InvalidInput("topic is required".into()));
        }
        let facts_text = render_facts(&topic.facts);
        let context = topic
            .context
            .as_deref()
            .map(as_plain_markdown)
            .unwrap_or_default();
        let fence = Fence::new();
        let user = format!(
            "Write up to {n} FAQ entries about this topic:\n{}\n\nFacts:\n{}\n\nContext:\n{}",
            fence.wrap("topic", &text::normalize_ws(&topic.topic)),
            fence.wrap(
                "facts",
                if facts_text.is_empty() {
                    "(none)"
                } else {
                    &facts_text
                }
            ),
            fence.wrap(
                "context",
                if context.is_empty() {
                    "(none)"
                } else {
                    &context
                }
            ),
        );
        let topic_text = topic.topic.clone();
        let sources = [facts_text.as_str(), context.as_str(), topic_text.as_str()];
        let out = self
            .run(
                Spec {
                    system: prompts::system(prompts::FAQ, locale),
                    user,
                    schema: faq_schema(),
                    effort: AiEffort::Medium,
                },
                |o: &mut FaqOutput| {
                    let mut seen = HashSet::new();
                    o.items
                        .retain(|i| seen.insert(text::fold(&text::normalize_ws(&i.question))));
                    if o.items.is_empty() {
                        return Err("items must contain at least one question".into());
                    }
                    for (i, item) in o.items.iter_mut().enumerate() {
                        item.question = text::normalize_ws(&item.question);
                        item.answer = item.answer.trim().to_owned();
                        non_empty(&item.answer, &format!("items[{i}].answer"))?;
                        if !(item.question.ends_with('?') || item.question.ends_with('？')) {
                            return Err(format!(
                                "items[{i}].question must end with a question mark"
                            ));
                        }
                        max_len(
                            &item.answer,
                            1000,
                            &format!("items[{i}].answer (target ≤ 600)"),
                        )?;
                    }
                    o.items.truncate(n);
                    let all: String = o
                        .items
                        .iter()
                        .map(|i| format!("{} {}\n", i.question, i.answer))
                        .collect();
                    guard_numbers(&all, &sources)
                },
            )
            .await?;
        Ok(map_output(out, |o| {
            o.items
                .into_iter()
                .map(|i| FaqEntry::new(i.question, i.answer))
                .collect()
        }))
    }

    /// A full blog post from a brief (and optional outline).
    pub async fn generate_blog_post(
        &self,
        brief: &BlogBrief,
        outline: Option<&[String]>,
        locale: Locale,
    ) -> Result<TaskOutput<BlogPost>, AiTaskError> {
        if brief.topic.trim().is_empty() {
            return Err(AiTaskError::InvalidInput("topic is required".into()));
        }
        let fence = Fence::new();
        let facts_text = render_facts(&brief.facts);
        let links_text = render_links(&brief.links);
        let mut brief_lines = vec![format!("- Topic: {}", text::normalize_ws(&brief.topic))];
        if let Some(a) = brief.audience.as_deref().filter(|a| !a.trim().is_empty()) {
            brief_lines.push(format!("- Audience: {}", text::normalize_ws(a)));
        }
        if !brief.keywords.is_empty() {
            brief_lines.push(format!(
                "- Keywords (first is primary): {}",
                brief.keywords.join(", ")
            ));
        }
        if let Some(b) = brief.brand.as_deref().filter(|b| !b.trim().is_empty()) {
            brief_lines.push(format!("- Brand: {}", text::normalize_ws(b)));
        }
        let outline_text = outline
            .map(|o| {
                o.iter()
                    .map(|h| format!("- {}", text::normalize_ws(h)))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let user = format!(
            "Write the blog post. Tone: {}. Target length: about {} words.\n\nBrief:\n{}\n\nOutline to follow (may be empty):\n{}\n\nCompany facts:\n{}\n\nAllowed links:\n{}",
            brief.tone.describe(),
            brief.target_words.unwrap_or(1200).clamp(300, 4000),
            fence.wrap("brief", &brief_lines.join("\n")),
            fence.wrap(
                "outline",
                if outline_text.is_empty() {
                    "(none)"
                } else {
                    &outline_text
                }
            ),
            fence.wrap(
                "facts",
                if facts_text.is_empty() {
                    "(none)"
                } else {
                    &facts_text
                }
            ),
            fence.wrap(
                "links",
                if links_text.is_empty() {
                    "(none)"
                } else {
                    &links_text
                }
            ),
        );
        let allowed: Vec<String> = brief
            .links
            .iter()
            .map(|l| l.url.trim().to_owned())
            .collect();
        let sources_owned = format!("{facts_text}\n{}", brief_lines.join("\n"));
        let topic = brief.topic.clone();
        self.run(
            Spec {
                system: prompts::system(prompts::BLOG, locale),
                user,
                schema: blog_schema(),
                effort: AiEffort::High,
            },
            |p: &mut BlogPost| {
                p.title = text::normalize_ws(&p.title);
                non_empty(&p.title, "title")?;
                max_len(&p.title, 120, "title")?;
                p.slug = finish_slug(&p.slug, &p.title, 6)?;
                p.excerpt = text::normalize_ws(&p.excerpt);
                non_empty(&p.excerpt, "excerpt")?;
                max_len(&p.excerpt, 300, "excerpt (target ≤ 200)")?;
                p.excerpt = meta::truncate(&p.excerpt, 200);
                no_h1(&p.body_markdown, "body_markdown")?;
                if text::word_count(&p.body_markdown) < 150 {
                    return Err("body_markdown is too short; write a complete article".into());
                }
                if p.body_markdown
                    .lines()
                    .filter(|l| l.starts_with("## "))
                    .count()
                    < 2
                {
                    return Err("body_markdown needs at least two \"## \" sections".into());
                }
                let allowed_refs: Vec<&str> = allowed.iter().map(String::as_str).collect();
                guard_links(&p.body_markdown, &allowed_refs)?;
                let sources = [sources_owned.as_str(), topic.as_str()];
                guard_amounts(
                    &format!("{}\n{}\n{}", p.title, p.excerpt, p.body_markdown),
                    &sources,
                )?;
                check_meta(
                    &mut p.meta,
                    &[sources_owned.as_str(), p.body_markdown.as_str()],
                    "meta.",
                )
            },
        )
        .await
    }

    /// A knowledge-base article (with HowTo steps when procedural).
    pub async fn generate_kb_article(
        &self,
        brief: &KbBrief,
        locale: Locale,
    ) -> Result<TaskOutput<KbArticle>, AiTaskError> {
        if brief.question.trim().is_empty() {
            return Err(AiTaskError::InvalidInput("question is required".into()));
        }
        let fence = Fence::new();
        let facts_text = render_facts(&brief.facts);
        let notes = brief
            .notes
            .as_deref()
            .map(as_plain_markdown)
            .unwrap_or_default();
        let links_text = render_links(&brief.links);
        let product = brief
            .product
            .as_deref()
            .map(text::normalize_ws)
            .unwrap_or_default();
        let user = format!(
            "Write the knowledge-base article.\n\nQuestion / task:\n{}\n\nProduct:\n{}\n\nFacts:\n{}\n\nStaff notes:\n{}\n\nAllowed links:\n{}",
            fence.wrap("question", &text::normalize_ws(&brief.question)),
            fence.wrap(
                "product",
                if product.is_empty() {
                    "(unspecified)"
                } else {
                    &product
                }
            ),
            fence.wrap(
                "facts",
                if facts_text.is_empty() {
                    "(none)"
                } else {
                    &facts_text
                }
            ),
            fence.wrap("notes", if notes.is_empty() { "(none)" } else { &notes }),
            fence.wrap(
                "links",
                if links_text.is_empty() {
                    "(none)"
                } else {
                    &links_text
                }
            ),
        );
        let allowed: Vec<String> = brief
            .links
            .iter()
            .map(|l| l.url.trim().to_owned())
            .collect();
        let sources_owned = format!("{facts_text}\n{notes}\n{}\n{product}", brief.question);
        self.run(
            Spec {
                system: prompts::system(prompts::KB, locale),
                user,
                schema: kb_schema(),
                effort: AiEffort::Medium,
            },
            |a: &mut KbArticle| {
                a.title = text::normalize_ws(&a.title);
                non_empty(&a.title, "title")?;
                a.slug = finish_slug(&a.slug, &a.title, 8)?;
                a.summary = a.summary.trim().to_owned();
                non_empty(&a.summary, "summary")?;
                no_h1(&a.body_markdown, "body_markdown")?;
                if text::word_count(&a.body_markdown) < 25 {
                    return Err("body_markdown is too short".into());
                }
                for (i, s) in a.steps.iter().enumerate() {
                    non_empty(&s.text, &format!("steps[{i}].text"))?;
                }
                let allowed_refs: Vec<&str> = allowed.iter().map(String::as_str).collect();
                guard_links(&a.body_markdown, &allowed_refs)?;
                guard_amounts(
                    &format!("{}\n{}", a.summary, a.body_markdown),
                    &[sources_owned.as_str()],
                )?;
                check_meta(
                    &mut a.meta,
                    &[sources_owned.as_str(), a.body_markdown.as_str()],
                    "meta.",
                )
            },
        )
        .await
    }

    /// Rewrites content for AI-engine citability without changing its facts.
    pub async fn rewrite_for_geo(
        &self,
        content: &str,
        format: ContentFormat,
        locale: Locale,
        facts: &[Fact],
    ) -> Result<TaskOutput<GeoRewrite>, AiTaskError> {
        let original = match format {
            ContentFormat::Html => html_to_markdown(content, &MarkdownOptions::default()),
            ContentFormat::Markdown | ContentFormat::Text => content.trim().to_owned(),
        };
        if original.trim().is_empty() {
            return Err(AiTaskError::InvalidInput("content is empty".into()));
        }
        let fence = Fence::new();
        let facts_text = render_facts(facts);
        let user = format!(
            "Rewrite this content for AI-engine citability.\n\nOriginal content (Markdown):\n{}\n\nAdditional facts:\n{}",
            fence.wrap("content", &original),
            fence.wrap(
                "facts",
                if facts_text.is_empty() {
                    "(none)"
                } else {
                    &facts_text
                }
            ),
        );
        let original_words = text::word_count(&original);
        let original_links = markdown_links(&original);
        let sources = [original.as_str(), facts_text.as_str()];
        self.run(
            Spec {
                system: prompts::system(prompts::GEO_REWRITE, locale),
                user,
                schema: geo_schema(),
                effort: AiEffort::Medium,
            },
            |g: &mut GeoRewrite| {
                non_empty(&g.content_markdown, "content_markdown")?;
                if text::word_count(&g.content_markdown) * 10 < original_words * 6 {
                    return Err(
                        "content_markdown dropped too much of the original content; keep all facts"
                            .into(),
                    );
                }
                if g.changes.is_empty() {
                    return Err("changes must list at least one change with its reason".into());
                }
                g.changes.truncate(20);
                let refs: Vec<&str> = original_links.iter().map(String::as_str).collect();
                guard_links(&g.content_markdown, &refs)?;
                guard_numbers(&g.content_markdown, &sources)
            },
        )
        .await
    }

    /// Translates content between Turkish and English, preserving HTML /
    /// Markdown structure.
    pub async fn translate(
        &self,
        content: &str,
        format: ContentFormat,
        from: Locale,
        to: Locale,
    ) -> Result<TaskOutput<String>, AiTaskError> {
        if content.trim().is_empty() {
            return Err(AiTaskError::InvalidInput("content is empty".into()));
        }
        if from == to {
            return Err(AiTaskError::InvalidInput(
                "source and target language are the same".into(),
            ));
        }
        let fence = Fence::new();
        let fmt = match format {
            ContentFormat::Html => "HTML",
            ContentFormat::Markdown => "Markdown",
            ContentFormat::Text => "plain text",
        };
        let user = format!(
            "Translate the following {fmt} from {} to {}.\n\n{}",
            locale_name(from),
            locale_name(to),
            fence.wrap("content", content)
        );
        let source = content.to_owned();
        let out = self
            .run(
                Spec {
                    system: prompts::system(prompts::TRANSLATE, to),
                    user,
                    schema: translation_schema(),
                    effort: AiEffort::Medium,
                },
                |t: &mut TranslationOutput| {
                    non_empty(&t.content, "content")?;
                    check_structure(&source, &t.content, format)?;
                    guard_numbers(&t.content, &[source.as_str()])
                },
            )
            .await?;
        Ok(map_output(out, |t| t.content))
    }

    /// Keyword ideas (no volumes — the model has no such data).
    pub async fn suggest_keywords(
        &self,
        topic: &str,
        seeds: &[String],
        n: usize,
        locale: Locale,
    ) -> Result<TaskOutput<Vec<KeywordSuggestion>>, AiTaskError> {
        if topic.trim().is_empty() || !(1..=50).contains(&n) {
            return Err(AiTaskError::InvalidInput(
                "topic is required and n must be 1–50".into(),
            ));
        }
        let fence = Fence::new();
        let user = format!(
            "Suggest {n} search queries for this topic:\n{}\n\nSeed keywords:\n{}",
            fence.wrap("topic", &text::normalize_ws(topic)),
            fence.wrap(
                "seeds",
                &if seeds.is_empty() {
                    "(none)".to_owned()
                } else {
                    seeds.join("\n")
                }
            ),
        );
        let out = self
            .run(
                Spec {
                    system: prompts::system(prompts::KEYWORDS, locale),
                    user,
                    schema: keyword_schema(),
                    effort: AiEffort::Low,
                },
                |o: &mut KeywordOutput| {
                    let mut seen = HashSet::new();
                    o.keywords.retain(|k| {
                        let key = text::fold(&text::normalize_ws(&k.keyword));
                        !key.is_empty() && seen.insert(key)
                    });
                    if o.keywords.is_empty() {
                        return Err("keywords must not be empty".into());
                    }
                    for k in &mut o.keywords {
                        k.keyword = text::normalize_ws(&k.keyword);
                        max_len(&k.keyword, 120, "keyword")?;
                    }
                    o.keywords.truncate(n);
                    Ok(())
                },
            )
            .await?;
        Ok(map_output(out, |o| o.keywords))
    }

    /// Prioritised action list from an audit report.
    pub async fn summarize_audit(
        &self,
        subject: AuditSubject<'_>,
        locale: Locale,
    ) -> Result<TaskOutput<AuditActionPlan>, AiTaskError> {
        let (report_json, urls, ids) = audit_digest(subject, locale);
        let fence = Fence::new();
        let user = format!(
            "Create the action plan from this audit report (JSON):\n{}",
            fence.wrap("audit_report", &report_json)
        );
        self.run(
            Spec {
                system: prompts::system(prompts::AUDIT, locale),
                user,
                schema: audit_schema(),
                effort: AiEffort::Medium,
            },
            |p: &mut AuditActionPlan| {
                non_empty(&p.summary, "summary")?;
                if p.actions.is_empty() {
                    return Err("actions must not be empty".into());
                }
                for (i, a) in p.actions.iter().enumerate() {
                    non_empty(&a.title, &format!("actions[{i}].title"))?;
                    if let Some(u) = a.affected_urls.iter().find(|u| !urls.contains(u.as_str())) {
                        return Err(format!("actions[{i}].affected_urls contains a URL that is not in the report: {u}"));
                    }
                    if let Some(id) = a.issue_ids.iter().find(|x| !ids.contains(x.as_str())) {
                        return Err(format!("actions[{i}].issue_ids contains an id that is not in the report: {id}"));
                    }
                }
                p.actions.sort_by_key(|a| a.priority);
                p.actions.truncate(10);
                let all: String = p.actions.iter().map(|a| format!("{} {}\n", a.title, a.detail)).collect();
                guard_numbers(&format!("{}\n{all}", p.summary), &[report_json.as_str()])
            },
        )
        .await
    }

    /// Draft reply to the latest customer message of a ticket.
    pub async fn support_reply_draft(
        &self,
        thread: &[TicketMessage],
        kb_context: &[KbSnippet],
        ctx: &SupportContext,
        locale: Locale,
    ) -> Result<TaskOutput<SupportDraft>, AiTaskError> {
        if !thread
            .iter()
            .any(|m| m.author == TicketAuthor::Customer && !m.body.trim().is_empty())
        {
            return Err(AiTaskError::InvalidInput(
                "the thread has no customer message".into(),
            ));
        }
        let fence = Fence::new();
        let thread_text = thread
            .iter()
            .map(|m| {
                let who = match m.author {
                    TicketAuthor::Customer => "CUSTOMER",
                    TicketAuthor::Staff => "STAFF",
                };
                let when = m
                    .sent_at
                    .map(|t| format!(" ({})", t.format("%Y-%m-%d %H:%M UTC")))
                    .unwrap_or_default();
                let name = m
                    .name
                    .as_deref()
                    .map(|n| format!(" {}", text::normalize_ws(n)))
                    .unwrap_or_default();
                format!("[{who}{name}{when}]\n{}", m.body.trim())
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let kb_text = kb_context
            .iter()
            .map(|k| {
                format!(
                    "### {}\nURL: {}\n{}",
                    text::normalize_ws(&k.title),
                    k.url.trim(),
                    k.content.trim()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut ctx_lines = vec![format!("- Company: {}", text::normalize_ws(&ctx.company))];
        if let Some(s) = ctx.subject.as_deref() {
            ctx_lines.push(format!("- Ticket subject: {}", text::normalize_ws(s)));
        }
        if let Some(a) = ctx.agent_name.as_deref() {
            ctx_lines.push(format!("- Staff member: {}", text::normalize_ws(a)));
        }
        if let Some(sig) = ctx.signature.as_deref() {
            ctx_lines.push(format!("- Signature:\n{}", sig.trim()));
        }
        let ctx_text = ctx_lines.join("\n");
        let user = format!(
            "Draft the reply to the customer's latest message.\n\nContext:\n{}\n\nTicket thread (oldest first):\n{}\n\nKnowledge-base excerpts:\n{}",
            fence.wrap("context", &ctx_text),
            fence.wrap("ticket_thread", &thread_text),
            fence.wrap(
                "knowledge_base",
                if kb_text.is_empty() {
                    "(none)"
                } else {
                    &kb_text
                }
            ),
        );
        let kb_urls: Vec<String> = kb_context.iter().map(|k| k.url.trim().to_owned()).collect();
        let sources = [thread_text.as_str(), kb_text.as_str(), ctx_text.as_str()];
        self.run(
            Spec {
                system: prompts::system(prompts::SUPPORT, locale),
                user,
                schema: support_schema(),
                effort: AiEffort::Medium,
            },
            |d: &mut SupportDraft| {
                d.reply = d.reply.trim().to_owned();
                non_empty(&d.reply, "reply")?;
                if let Some(u) = d
                    .used_kb_urls
                    .iter()
                    .find(|u| !kb_urls.contains(&u.trim().to_owned()))
                {
                    return Err(format!(
                        "used_kb_urls contains a URL that is not one of the supplied excerpts: {u}"
                    ));
                }
                guard_links(
                    &d.reply,
                    &kb_urls.iter().map(String::as_str).collect::<Vec<_>>(),
                )?;
                guard_numbers(&d.reply, &sources)
            },
        )
        .await
    }
}

fn map_output<A, B>(o: TaskOutput<A>, f: impl FnOnce(A) -> B) -> TaskOutput<B> {
    TaskOutput {
        value: f(o.value),
        model: o.model,
        usage: o.usage,
        attempts: o.attempts,
        repaired: o.repaired,
    }
}

/// HTML → Markdown when the input looks like HTML; otherwise trimmed text.
fn as_plain_markdown(s: &str) -> String {
    let t = s.trim();
    if t.starts_with('<') && t.contains('>') {
        html_to_markdown(t, &MarkdownOptions::default())
    } else {
        t.to_owned()
    }
}

static TAG: OnceLock<Option<Regex>> = OnceLock::new();
static ATTR_URL: OnceLock<Option<Regex>> = OnceLock::new();

fn tag_sequence(html: &str) -> Vec<String> {
    re(&TAG, r"<\s*(/?)\s*([a-zA-Z][a-zA-Z0-9-]*)")
        .map(|r| {
            r.captures_iter(html)
                .map(|c| format!("{}{}", &c[1], c[2].to_ascii_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

fn attr_urls(html: &str) -> Vec<String> {
    re(
        &ATTR_URL,
        r#"(?i)\b(?:href|src|action)\s*=\s*["']([^"']*)["']"#,
    )
    .map(|r| r.captures_iter(html).map(|c| c[1].to_owned()).collect())
    .unwrap_or_default()
}

fn md_shape(md: &str) -> (Vec<usize>, usize, Vec<String>) {
    let mut headings = Vec::new();
    let mut fences = 0;
    for l in md.lines() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fences += 1;
        } else {
            let h = t.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&h) && t[h..].starts_with(' ') {
                headings.push(h);
            }
        }
    }
    (headings, fences, markdown_links(md))
}

fn check_structure(source: &str, out: &str, format: ContentFormat) -> Result<(), String> {
    match format {
        ContentFormat::Html => {
            if tag_sequence(source) != tag_sequence(out) {
                return Err("the HTML tag structure changed; keep every tag exactly as in the source and translate only text".into());
            }
            if attr_urls(source) != attr_urls(out) {
                return Err("href/src attribute values changed; URLs must be kept exactly".into());
            }
        }
        ContentFormat::Markdown => {
            let (h1, f1, l1) = md_shape(source);
            let (h2, f2, l2) = md_shape(out);
            if h1 != h2 {
                return Err("the Markdown heading structure changed; keep the same headings at the same levels".into());
            }
            if f1 != f2 {
                return Err("code blocks changed; keep code blocks unchanged".into());
            }
            if l1 != l2 {
                return Err(
                    "link URLs changed; translate only link text and keep URLs exactly".into(),
                );
            }
        }
        ContentFormat::Text => {}
    }
    Ok(())
}

/// Compact JSON of an audit for the prompt + the URLs and ids it contains.
fn audit_digest(
    subject: AuditSubject<'_>,
    locale: Locale,
) -> (String, HashSet<String>, HashSet<String>) {
    let mut urls = HashSet::new();
    let mut ids = HashSet::new();
    let finding = |f: &crate::audit::Finding,
                   ids: &mut HashSet<String>,
                   urls: &mut HashSet<String>| {
        ids.insert(f.id.clone());
        urls.extend(f.urls.iter().cloned());
        json!({"id": f.id, "severity": f.severity, "message": f.message(locale), "urls": f.urls})
    };
    let page = |p: &crate::audit::PageReport,
                ids: &mut HashSet<String>,
                urls: &mut HashSet<String>| {
        urls.insert(p.url.clone());
        let findings: Vec<Value> = p
            .findings
            .iter()
            .take(15)
            .map(|f| finding(f, ids, urls))
            .collect();
        json!({"url": p.url, "score": p.score, "word_count": p.stats.word_count, "findings": findings})
    };
    let doc = match subject {
        AuditSubject::Site(r) => {
            let site: Vec<Value> = r
                .site_findings
                .iter()
                .map(|f| finding(f, &mut ids, &mut urls))
                .collect();
            let mut pages: Vec<&crate::audit::PageReport> = r.pages.iter().collect();
            pages.sort_by_key(|p| p.score);
            let pages: Vec<Value> = pages
                .into_iter()
                .take(50)
                .map(|p| page(p, &mut ids, &mut urls))
                .collect();
            json!({"summary": r.summary, "site_findings": site, "pages": pages})
        }
        AuditSubject::Page(p) => json!({"pages": [page(p, &mut ids, &mut urls)]}),
    };
    (doc.to_string(), urls, ids)
}

#[cfg(test)]
mod tests;
