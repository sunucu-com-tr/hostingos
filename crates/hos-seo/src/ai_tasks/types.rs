//! Inputs and outputs of the AI tasks, with the JSON schemas sent as
//! structured-output formats. Output structs reject unknown fields.

use chrono::{DateTime, Utc};
use hos_core::domain::ProductType;
use hos_core::money::{BillingCycle, Money};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audit::{PageReport, SiteReport};

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// A labelled fact (`"Disk"` → `"20 GB NVMe"`). The only source of
/// company-specific claims the model may use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    /// Label.
    pub label: String,
    /// Value.
    pub value: String,
}

impl Fact {
    /// Creates a fact.
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// One price of a product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceFact {
    /// Billing cycle.
    pub cycle: BillingCycle,
    /// Price per cycle.
    pub price: Money,
    /// Setup fee.
    #[serde(default)]
    pub setup_fee: Option<Money>,
}

/// Everything known about a product.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProductFacts {
    /// Product name.
    pub name: String,
    /// Brand (tenant name).
    pub brand: String,
    /// Product type.
    pub product_type: Option<ProductType>,
    /// Target audience ("small businesses", "agencies").
    pub audience: Option<String>,
    /// Specs / features.
    pub facts: Vec<Fact>,
    /// Prices.
    pub prices: Vec<PriceFact>,
}

/// Writing tone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    /// Clear, trustworthy, businesslike.
    #[default]
    Professional,
    /// Warm and approachable.
    Friendly,
    /// Precise, for technical buyers.
    Technical,
    /// Benefit-driven, conversion-oriented (still factual).
    Persuasive,
}

impl Tone {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Professional => "professional — clear, trustworthy and businesslike",
            Self::Friendly => "friendly — warm and approachable, still precise",
            Self::Technical => {
                "technical — precise and specific for technically experienced buyers"
            }
            Self::Persuasive => {
                "persuasive — benefit-driven and conversion-oriented, but strictly factual"
            }
        }
    }
}

/// FAQ subject.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FaqTopic {
    /// Topic or product name.
    pub topic: String,
    /// Facts the answers may use.
    pub facts: Vec<Fact>,
    /// Page content / policy text the answers may use.
    pub context: Option<String>,
}

/// A link the model may use (internal linking).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRef {
    /// Anchor / page title.
    pub title: String,
    /// Absolute URL.
    pub url: String,
}

/// Blog post brief.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BlogBrief {
    /// Topic / working title.
    pub topic: String,
    /// Audience.
    pub audience: Option<String>,
    /// Target keywords (first = primary).
    pub keywords: Vec<String>,
    /// Company facts the post may mention.
    pub facts: Vec<Fact>,
    /// Approximate length.
    pub target_words: Option<u32>,
    /// Tone.
    pub tone: Tone,
    /// Brand name.
    pub brand: Option<String>,
    /// Internal links the post may use (no other links are allowed).
    pub links: Vec<LinkRef>,
}

/// Knowledge-base article brief.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KbBrief {
    /// The question / task ("cPanel'de e-posta hesabı nasıl oluşturulur?").
    pub question: String,
    /// Product / panel it applies to.
    pub product: Option<String>,
    /// Facts (menu names, limits, ports …).
    pub facts: Vec<Fact>,
    /// Staff notes / raw procedure.
    pub notes: Option<String>,
    /// Links the article may use.
    pub links: Vec<LinkRef>,
}

/// Format of content passed to rewrite / translate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentFormat {
    /// HTML.
    Html,
    /// Markdown.
    #[default]
    Markdown,
    /// Plain text.
    Text,
}

/// Who wrote a ticket message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketAuthor {
    /// The customer.
    Customer,
    /// Support staff.
    Staff,
}

/// One message of a ticket thread (oldest first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketMessage {
    /// Author kind.
    pub author: TicketAuthor,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Plain-text body.
    pub body: String,
    /// Timestamp.
    #[serde(default)]
    pub sent_at: Option<DateTime<Utc>>,
}

/// Knowledge-base excerpt for support replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KbSnippet {
    /// Article title.
    pub title: String,
    /// Article URL.
    pub url: String,
    /// Relevant text.
    pub content: String,
}

/// Context for support drafts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportContext {
    /// Company name.
    pub company: String,
    /// Ticket subject.
    pub subject: Option<String>,
    /// Name of the staff member who will send it.
    pub agent_name: Option<String>,
    /// Signature block.
    pub signature: Option<String>,
}

/// What [`super::AiTasks::summarize_audit`] summarises.
#[derive(Debug, Clone, Copy)]
pub enum AuditSubject<'a> {
    /// A whole site.
    Site(&'a SiteReport),
    /// One page.
    Page(&'a PageReport),
}

impl<'a> From<&'a SiteReport> for AuditSubject<'a> {
    fn from(r: &'a SiteReport) -> Self {
        Self::Site(r)
    }
}

impl<'a> From<&'a PageReport> for AuditSubject<'a> {
    fn from(r: &'a PageReport) -> Self {
        Self::Page(r)
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// Generated title + meta description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetaSuggestion {
    /// `<title>` text without brand suffix (≤ 60 graphemes).
    pub title: String,
    /// Meta description (≤ 160 graphemes).
    pub description: String,
}

/// Product copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductCopy {
    /// One-line headline.
    pub headline: String,
    /// 1–2 sentences.
    pub short_description: String,
    /// Long description (Markdown, no H1).
    pub long_description_markdown: String,
    /// 3–8 feature bullets.
    pub bullet_points: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FaqOutput {
    pub items: Vec<FaqItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FaqItem {
    pub question: String,
    pub answer: String,
}

/// Blog post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlogPost {
    /// Title (H1).
    pub title: String,
    /// URL slug (ASCII, hyphenated).
    pub slug: String,
    /// Excerpt (≤ 200 graphemes).
    pub excerpt: String,
    /// Body in Markdown (no H1).
    pub body_markdown: String,
    /// SEO metadata.
    pub meta: MetaSuggestion,
}

/// A HowTo step of a KB article.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KbStep {
    /// Short step name.
    pub name: String,
    /// Instruction.
    pub text: String,
}

/// Knowledge-base article.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KbArticle {
    /// Title.
    pub title: String,
    /// Slug.
    pub slug: String,
    /// One-paragraph answer.
    pub summary: String,
    /// Body (Markdown, no H1).
    pub body_markdown: String,
    /// Steps for `HowTo` JSON-LD (empty when not procedural).
    pub steps: Vec<KbStep>,
    /// SEO metadata.
    pub meta: MetaSuggestion,
}

/// One change made by the GEO rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeNote {
    /// What changed.
    pub change: String,
    /// Why it helps.
    pub reason: String,
}

/// GEO rewrite result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeoRewrite {
    /// Rewritten content (Markdown).
    pub content_markdown: String,
    /// Change log with reasons.
    pub changes: Vec<ChangeNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TranslationOutput {
    pub content: String,
}

/// Search intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchIntent {
    /// Wants to learn.
    Informational,
    /// Comparing options.
    Commercial,
    /// Ready to buy.
    Transactional,
    /// Looking for a specific site.
    Navigational,
}

/// A keyword idea.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeywordSuggestion {
    /// The query.
    pub keyword: String,
    /// Intent.
    pub intent: SearchIntent,
    /// Why searchers use it.
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeywordOutput {
    pub keywords: Vec<KeywordSuggestion>,
}

/// Low / medium / high.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// High.
    High,
    /// Medium.
    Medium,
    /// Low.
    Low,
}

/// One action of the audit plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionItem {
    /// Priority.
    pub priority: Level,
    /// Short title.
    pub title: String,
    /// What to do and why.
    pub detail: String,
    /// Estimated effort.
    pub effort: Level,
    /// URLs from the report.
    pub affected_urls: Vec<String>,
    /// Finding ids from the report.
    pub issue_ids: Vec<String>,
}

/// Prioritised action plan from an audit report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditActionPlan {
    /// Overall assessment.
    pub summary: String,
    /// Actions, most important first.
    pub actions: Vec<ActionItem>,
}

/// Support reply draft (always reviewed by staff before sending).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportDraft {
    /// Reply text.
    pub reply: String,
    /// How well the sources support the reply.
    pub confidence: Level,
    /// Escalate to a human decision (refunds, account checks, incidents …).
    pub needs_human: bool,
    /// KB URLs used.
    pub used_kb_urls: Vec<String>,
    /// Note for the staff member.
    pub internal_note: String,
}

// ---------------------------------------------------------------------------
// Schemas (structured outputs: every object has additionalProperties=false
// and lists all properties as required)
// ---------------------------------------------------------------------------

fn obj(props: Value) -> Value {
    let required: Vec<String> = props
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn s() -> Value {
    json!({"type": "string"})
}

fn arr(items: Value) -> Value {
    json!({"type": "array", "items": items})
}

fn level() -> Value {
    json!({"type": "string", "enum": ["high", "medium", "low"]})
}

pub(crate) fn meta_schema() -> Value {
    obj(json!({"title": s(), "description": s()}))
}

pub(crate) fn product_schema() -> Value {
    obj(json!({
        "headline": s(),
        "short_description": s(),
        "long_description_markdown": s(),
        "bullet_points": arr(s())
    }))
}

pub(crate) fn faq_schema() -> Value {
    obj(json!({"items": arr(obj(json!({"question": s(), "answer": s()})))}))
}

pub(crate) fn blog_schema() -> Value {
    obj(json!({
        "title": s(), "slug": s(), "excerpt": s(), "body_markdown": s(), "meta": meta_schema()
    }))
}

pub(crate) fn kb_schema() -> Value {
    obj(json!({
        "title": s(), "slug": s(), "summary": s(), "body_markdown": s(),
        "steps": arr(obj(json!({"name": s(), "text": s()}))),
        "meta": meta_schema()
    }))
}

pub(crate) fn geo_schema() -> Value {
    obj(json!({
        "content_markdown": s(),
        "changes": arr(obj(json!({"change": s(), "reason": s()})))
    }))
}

pub(crate) fn translation_schema() -> Value {
    obj(json!({"content": s()}))
}

pub(crate) fn keyword_schema() -> Value {
    obj(json!({"keywords": arr(obj(json!({
        "keyword": s(),
        "intent": {"type": "string", "enum": ["informational", "commercial", "transactional", "navigational"]},
        "rationale": s()
    })))}))
}

pub(crate) fn audit_schema() -> Value {
    obj(json!({
        "summary": s(),
        "actions": arr(obj(json!({
            "priority": level(), "title": s(), "detail": s(), "effort": level(),
            "affected_urls": arr(s()), "issue_ids": arr(s())
        })))
    }))
}

pub(crate) fn support_schema() -> Value {
    obj(json!({
        "reply": s(), "confidence": level(), "needs_human": {"type": "boolean"},
        "used_kb_urls": arr(s()), "internal_note": s()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_strict(v: &Value, path: &str) {
        match v {
            Value::Object(m) => {
                if m.get("type") == Some(&json!("object")) {
                    assert_eq!(m.get("additionalProperties"), Some(&json!(false)), "{path}");
                    let props: Vec<&String> = m["properties"].as_object().unwrap().keys().collect();
                    let req: Vec<&str> = m["required"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| r.as_str().unwrap())
                        .collect();
                    for p in props {
                        assert!(req.contains(&p.as_str()), "{path}.{p} not required");
                    }
                }
                for (k, x) in m {
                    check_strict(x, &format!("{path}.{k}"));
                }
            }
            Value::Array(a) => a.iter().for_each(|x| check_strict(x, path)),
            _ => {}
        }
    }

    #[test]
    fn schemas_are_strict() {
        for (name, schema) in [
            ("meta", meta_schema()),
            ("product", product_schema()),
            ("faq", faq_schema()),
            ("blog", blog_schema()),
            ("kb", kb_schema()),
            ("geo", geo_schema()),
            ("translation", translation_schema()),
            ("keywords", keyword_schema()),
            ("audit", audit_schema()),
            ("support", support_schema()),
        ] {
            check_strict(&schema, name);
        }
    }
}
