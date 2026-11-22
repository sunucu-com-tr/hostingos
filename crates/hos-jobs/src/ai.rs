//! The `ai_generation` job: runs an `hos_seo::ai_tasks` task described by an
//! `ai_generations` row and stores the result (status `completed`) or the
//! error (`failed`). Staff then apply or discard it in the admin.
//!
//! `ai_generations.input` selects the task:
//!
//! ```json
//! {"task": "meta", "content": "...", "keywords": ["vps"]}
//! {"task": "product_description", "facts": {...}, "tone": "friendly"}
//! {"task": "faq", "topic": {...}, "n": 6}
//! {"task": "blog_post", "brief": {...}, "outline": ["..."]}
//! {"task": "kb_article", "brief": {...}}
//! {"task": "geo_rewrite", "content": "...", "format": "markdown", "facts": []}
//! {"task": "translate", "content": "...", "format": "html", "from": "tr", "to": "en"}
//! {"task": "keywords", "topic": "...", "seeds": [], "n": 20}
//! {"task": "support_reply", "thread": [...], "kb": [...], "context": {...}}
//! {"task": "summarize_audit", "audit_id": "..."}
//! ```
//!
//! `summarize_audit` reads the stored site report (`seo_audits.issues.site`).
//!
//! The locale is `ai_generations.locale` (default: the tenant's default locale).

use hos_billing::Billing;
use hos_core::domain::Locale;
use hos_core::ids::TenantId;
use hos_db::enums::AiGenerationStatus;
use hos_db::ids::{AiGenerationId, SeoAuditId};
use hos_db::repo::{billing_support, seo};
use hos_seo::ai_tasks::{
    AiTaskError, AiTasks, AuditSubject, BlogBrief, ContentFormat, Fact, FaqTopic, KbBrief,
    KbSnippet, ProductFacts, SupportContext, TaskOutput, TicketMessage, Tone,
};
use hos_seo::audit::SiteReport;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::handlers::{HandlerError, Outcome};

/// Task selected by `ai_generations.input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "task", rename_all = "snake_case")]
pub enum AiJobInput {
    Meta {
        content: String,
        #[serde(default)]
        keywords: Vec<String>,
    },
    ProductDescription {
        facts: ProductFacts,
        #[serde(default)]
        tone: Tone,
    },
    Faq {
        topic: FaqTopic,
        #[serde(default = "default_faq")]
        n: usize,
    },
    BlogPost {
        brief: BlogBrief,
        #[serde(default)]
        outline: Option<Vec<String>>,
    },
    KbArticle {
        brief: KbBrief,
    },
    GeoRewrite {
        content: String,
        #[serde(default)]
        format: ContentFormat,
        #[serde(default)]
        facts: Vec<Fact>,
    },
    Translate {
        content: String,
        #[serde(default)]
        format: ContentFormat,
        from: Locale,
        to: Locale,
    },
    Keywords {
        topic: String,
        #[serde(default)]
        seeds: Vec<String>,
        #[serde(default = "default_keywords")]
        n: usize,
    },
    /// Draft reply to a ticket (staff reviews and edits it; never sent automatically).
    SupportReply {
        thread: Vec<TicketMessage>,
        #[serde(default)]
        kb: Vec<KbSnippet>,
        #[serde(default)]
        context: SupportContext,
    },
    /// Action plan from a stored site audit.
    SummarizeAudit {
        audit_id: SeoAuditId,
    },
}

fn default_faq() -> usize {
    6
}

fn default_keywords() -> usize {
    20
}

struct Generated {
    text: String,
    json: Value,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
}

fn pack<T: Serialize>(out: TaskOutput<T>, text: Option<String>) -> Generated {
    let json = serde_json::to_value(&out.value).unwrap_or(Value::Null);
    Generated {
        text: text.unwrap_or_else(|| serde_json::to_string_pretty(&json).unwrap_or_default()),
        json,
        model: out.model,
        input_tokens: out.usage.input_tokens
            + out.usage.cache_read_input_tokens
            + out.usage.cache_creation_input_tokens,
        output_tokens: out.usage.output_tokens,
    }
}

async fn run_task(
    tasks: &AiTasks,
    input: &AiJobInput,
    locale: Locale,
    audit: Option<&SiteReport>,
) -> Result<Generated, AiTaskError> {
    Ok(match input {
        AiJobInput::Meta { content, keywords } => {
            pack(tasks.generate_meta(content, locale, keywords).await?, None)
        }
        AiJobInput::ProductDescription { facts, tone } => pack(
            tasks
                .generate_product_description(facts, *tone, locale)
                .await?,
            None,
        ),
        AiJobInput::Faq { topic, n } => pack(tasks.generate_faq(topic, *n, locale).await?, None),
        AiJobInput::BlogPost { brief, outline } => pack(
            tasks
                .generate_blog_post(brief, outline.as_deref(), locale)
                .await?,
            None,
        ),
        AiJobInput::KbArticle { brief } => {
            pack(tasks.generate_kb_article(brief, locale).await?, None)
        }
        AiJobInput::GeoRewrite {
            content,
            format,
            facts,
        } => pack(
            tasks
                .rewrite_for_geo(content, *format, locale, facts)
                .await?,
            None,
        ),
        AiJobInput::Translate {
            content,
            format,
            from,
            to,
        } => {
            let out = tasks.translate(content, *format, *from, *to).await?;
            let text = out.value.clone();
            pack(out, Some(text))
        }
        AiJobInput::Keywords { topic, seeds, n } => pack(
            tasks.suggest_keywords(topic, seeds, *n, locale).await?,
            None,
        ),
        AiJobInput::SupportReply {
            thread,
            kb,
            context,
        } => {
            let out = tasks
                .support_reply_draft(thread, kb, context, locale)
                .await?;
            let text = out.value.reply.clone();
            pack(out, Some(text))
        }
        AiJobInput::SummarizeAudit { .. } => {
            let report = audit.ok_or_else(|| {
                AiTaskError::InvalidInput("the audit report is not available".into())
            })?;
            pack(
                tasks
                    .summarize_audit(AuditSubject::Site(report), locale)
                    .await?,
                None,
            )
        }
    })
}

/// Runs one generation; re-runs of a finished row are no-ops.
pub async fn run_generation(
    billing: &Billing,
    tenant_id: TenantId,
    id: AiGenerationId,
    final_attempt: bool,
) -> Result<Outcome, HandlerError> {
    let pool = billing.pool();
    let Some(row) = billing_support::get_ai_generation(pool, tenant_id, id).await? else {
        return Err(HandlerError::Permanent(format!(
            "ai generation {id} not found"
        )));
    };
    if row.status != AiGenerationStatus::Pending {
        return Ok(Outcome::Done(json!({"skipped": row.status.as_str()})));
    }
    let fail = |msg: String| async move {
        seo::set_ai_generation_status(pool, tenant_id, id, AiGenerationStatus::Failed, Some(&msg))
            .await
            .map_err(HandlerError::from)?;
        Err::<Outcome, _>(HandlerError::Permanent(msg))
    };
    let input: AiJobInput = match serde_json::from_value(row.input.clone()) {
        Ok(i) => i,
        Err(e) => return fail(format!("invalid generation input: {e}")).await,
    };
    let Some((_, provider)) = billing.registry().ai(tenant_id).await? else {
        return fail("no AI provider is configured".into()).await;
    };
    let locale = match row.locale {
        Some(l) => l,
        None => billing.tenant(tenant_id).await?.default_locale,
    };
    let audit = match &input {
        AiJobInput::SummarizeAudit { audit_id } => {
            match seo::get_seo_audit(pool, tenant_id, *audit_id).await? {
                Some(a) => a
                    .issues
                    .get("site")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<SiteReport>(v).ok()),
                None => None,
            }
        }
        _ => None,
    };
    let tasks = AiTasks::new(provider);
    match run_task(&tasks, &input, locale, audit.as_ref()).await {
        Ok(g) => {
            seo::set_ai_generation_model(pool, tenant_id, id, &g.model).await?;
            seo::complete_ai_generation(
                pool,
                tenant_id,
                id,
                &g.text,
                Some(&g.json),
                i32::try_from(g.input_tokens).ok(),
                i32::try_from(g.output_tokens).ok(),
                None,
            )
            .await?;
            Ok(Outcome::Done(json!({"model": g.model})))
        }
        Err(AiTaskError::Provider(e)) if e.is_retryable() && !final_attempt => {
            Err(HandlerError::Retry {
                message: e.to_string(),
                after: e.retry_after(),
            })
        }
        Err(e) => fail(e.to_string()).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_parse() {
        let v = json!({"task": "translate", "content": "Merhaba", "format": "text", "from": "tr", "to": "en"});
        assert!(matches!(
            serde_json::from_value::<AiJobInput>(v).unwrap(),
            AiJobInput::Translate { to: Locale::En, .. }
        ));
        let v = json!({"task": "faq", "topic": {}});
        assert!(matches!(
            serde_json::from_value::<AiJobInput>(v).unwrap(),
            AiJobInput::Faq { n: 6, .. }
        ));
        assert!(serde_json::from_value::<AiJobInput>(json!({"task": "nope"})).is_err());
    }
}
