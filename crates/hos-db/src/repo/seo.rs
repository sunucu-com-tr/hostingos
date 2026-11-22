//! SEO / GEO / AI tooling: per-entity SEO metadata, SEO audits and AI content
//! generations.

use chrono::{DateTime, Utc};
use hos_core::domain::Locale;
use hos_core::ids::{IntegrationId, TenantId, UserId};
use hos_core::money::Currency;
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgExecutor, Postgres};
use uuid::Uuid;

use super::{Json, empty_object};
use crate::enums::{AiGenerationStatus, AiPurpose, RunStatus, SeoEntityType};
use crate::error::DbResult;
use crate::ids::{AiGenerationId, SeoAuditId, SeoMetaId};
use crate::page::{Page, Paged, fetch_paged};

// ---------------------------------------------------------------------------
// seo_meta
// ---------------------------------------------------------------------------

/// `seo_meta` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SeoMetaRow {
    pub id: SeoMetaId,
    pub tenant_id: TenantId,
    pub entity_type: SeoEntityType,
    pub entity_id: Option<Uuid>,
    pub route: Option<String>,
    pub locale: Locale,
    pub title: Option<String>,
    pub description: Option<String>,
    pub canonical: Option<String>,
    pub keywords: Vec<String>,
    pub og: Json,
    pub jsonld: Option<Json>,
    pub noindex: bool,
    pub nofollow: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a `seo_meta` row is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeoTarget {
    /// Storefront home page.
    Home,
    /// A static route such as `/domains`.
    Route(String),
    /// An entity (`entity_type` must not be `Home` / `Route`).
    Entity(SeoEntityType, Uuid),
}

impl SeoTarget {
    fn parts(&self) -> (SeoEntityType, Option<Uuid>, Option<&str>) {
        match self {
            Self::Home => (SeoEntityType::Home, None, None),
            Self::Route(r) => (SeoEntityType::Route, None, Some(r.as_str())),
            Self::Entity(t, id) => (*t, Some(*id), None),
        }
    }
}

/// Fields of [`upsert_seo_meta`].
#[derive(Debug, Clone, Default)]
pub struct SeoMetaInput {
    pub title: Option<String>,
    pub description: Option<String>,
    pub canonical: Option<String>,
    pub keywords: Vec<String>,
    pub og: Option<Json>,
    pub jsonld: Option<Json>,
    pub noindex: bool,
    pub nofollow: bool,
}

/// Inserts or replaces the SEO metadata of a target in a locale.
pub async fn upsert_seo_meta(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    target: &SeoTarget,
    locale: Locale,
    m: &SeoMetaInput,
) -> DbResult<SeoMetaRow> {
    let (entity_type, entity_id, route) = target.parts();
    Ok(sqlx::query_as::<_, SeoMetaRow>(
        "insert into seo_meta (id, tenant_id, entity_type, entity_id, route, locale, title, description, canonical,
                               keywords, og, jsonld, noindex, nofollow)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         on conflict (tenant_id, entity_type, entity_id, route, locale) do update
            set title = excluded.title, description = excluded.description, canonical = excluded.canonical,
                keywords = excluded.keywords, og = excluded.og, jsonld = excluded.jsonld,
                noindex = excluded.noindex, nofollow = excluded.nofollow
         returning *",
    )
    .bind(SeoMetaId::new())
    .bind(tenant_id)
    .bind(entity_type)
    .bind(entity_id)
    .bind(route)
    .bind(locale)
    .bind(&m.title)
    .bind(&m.description)
    .bind(&m.canonical)
    .bind(&m.keywords)
    .bind(m.og.clone().unwrap_or_else(empty_object))
    .bind(&m.jsonld)
    .bind(m.noindex)
    .bind(m.nofollow)
    .fetch_one(db)
    .await?)
}

/// SEO metadata of a target in a locale.
pub async fn get_seo_meta(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    target: &SeoTarget,
    locale: Locale,
) -> DbResult<Option<SeoMetaRow>> {
    let (entity_type, entity_id, route) = target.parts();
    Ok(sqlx::query_as::<_, SeoMetaRow>(
        "select * from seo_meta
         where tenant_id = $1 and entity_type = $2 and entity_id is not distinct from $3
           and route is not distinct from $4 and locale = $5",
    )
    .bind(tenant_id)
    .bind(entity_type)
    .bind(entity_id)
    .bind(route)
    .bind(locale)
    .fetch_optional(db)
    .await?)
}

/// All SEO rows of an entity type (admin overview / audits).
pub async fn list_seo_meta(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    entity_type: Option<SeoEntityType>,
) -> DbResult<Vec<SeoMetaRow>> {
    Ok(sqlx::query_as::<_, SeoMetaRow>(
        "select * from seo_meta where tenant_id = $1 and ($2::text is null or entity_type = $2)
         order by entity_type, route nulls first, locale",
    )
    .bind(tenant_id)
    .bind(entity_type)
    .fetch_all(db)
    .await?)
}

/// Deletes the metadata of a target in a locale.
pub async fn delete_seo_meta(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    target: &SeoTarget,
    locale: Locale,
) -> DbResult<bool> {
    let (entity_type, entity_id, route) = target.parts();
    let res = sqlx::query(
        "delete from seo_meta where tenant_id = $1 and entity_type = $2 and entity_id is not distinct from $3
           and route is not distinct from $4 and locale = $5",
    )
    .bind(tenant_id)
    .bind(entity_type)
    .bind(entity_id)
    .bind(route)
    .bind(locale)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// seo_audits
// ---------------------------------------------------------------------------

/// `seo_audits` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SeoAuditRow {
    pub id: SeoAuditId,
    pub tenant_id: TenantId,
    pub url: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub locale: Option<Locale>,
    pub status: RunStatus,
    pub score: Option<i16>,
    pub issues: Json,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Pages to audit (site audits; migration 0021).
    pub pages_total: i32,
    /// Pages audited so far.
    pub pages_done: i32,
}

/// Queues an audit of `url`.
pub async fn create_seo_audit(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    url: &str,
    entity: Option<(&str, Uuid)>,
    locale: Option<Locale>,
) -> DbResult<SeoAuditRow> {
    Ok(sqlx::query_as::<_, SeoAuditRow>(
        "insert into seo_audits (id, tenant_id, url, entity_type, entity_id, locale)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(SeoAuditId::new())
    .bind(tenant_id)
    .bind(url)
    .bind(entity.map(|e| e.0))
    .bind(entity.map(|e| e.1))
    .bind(locale)
    .fetch_one(db)
    .await?)
}

/// Marks an audit running.
pub async fn start_seo_audit(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: SeoAuditId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update seo_audits set status = 'running', started_at = now() where tenant_id = $1 and id = $2 and status = 'pending'",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Stores an audit result (`error` = failed).
pub async fn finish_seo_audit(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: SeoAuditId,
    score: Option<i16>,
    issues: &Json,
    summary: Option<&str>,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update seo_audits set status = case when $6::text is null then 'completed' else 'failed' end,
                score = $3, issues = $4, summary = $5, error = $6, completed_at = now()
         where tenant_id = $1 and id = $2 and status in ('pending', 'running')",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(score)
    .bind(issues)
    .bind(summary)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Latest audits.
pub async fn recent_seo_audits(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<SeoAuditRow>> {
    Ok(sqlx::query_as::<_, SeoAuditRow>(
        "select * from seo_audits where tenant_id = $1 order by created_at desc limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// ai_generations
// ---------------------------------------------------------------------------

/// `ai_generations` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct AiGenerationRow {
    pub id: AiGenerationId,
    pub tenant_id: TenantId,
    pub user_id: Option<UserId>,
    pub integration_id: Option<IntegrationId>,
    pub purpose: AiPurpose,
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub locale: Option<Locale>,
    pub model: String,
    pub prompt: String,
    pub input: Json,
    pub output: Option<String>,
    pub output_json: Option<Json>,
    pub status: AiGenerationStatus,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub cost: Option<Decimal>,
    pub cost_currency: Option<Currency>,
    pub error: Option<String>,
    pub applied_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_ai_generation`].
#[derive(Debug, Clone)]
pub struct NewAiGeneration {
    pub user_id: Option<UserId>,
    pub integration_id: Option<IntegrationId>,
    pub purpose: AiPurpose,
    pub entity: Option<(String, Uuid)>,
    pub locale: Option<Locale>,
    pub model: String,
    pub prompt: String,
    pub input: Json,
}

/// Records a generation request (status `pending`).
pub async fn create_ai_generation(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    g: &NewAiGeneration,
) -> DbResult<AiGenerationRow> {
    Ok(sqlx::query_as::<_, AiGenerationRow>(
        "insert into ai_generations (id, tenant_id, user_id, integration_id, purpose, entity_type, entity_id, locale,
                                     model, prompt, input)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning *",
    )
    .bind(AiGenerationId::new())
    .bind(tenant_id)
    .bind(g.user_id)
    .bind(g.integration_id)
    .bind(g.purpose)
    .bind(g.entity.as_ref().map(|e| e.0.as_str()))
    .bind(g.entity.as_ref().map(|e| e.1))
    .bind(g.locale)
    .bind(&g.model)
    .bind(&g.prompt)
    .bind(&g.input)
    .fetch_one(db)
    .await?)
}

/// Stores the model output and usage.
#[allow(clippy::too_many_arguments)]
pub async fn complete_ai_generation(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AiGenerationId,
    output: &str,
    output_json: Option<&Json>,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    cost: Option<(Decimal, Currency)>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update ai_generations set status = 'completed', output = $3, output_json = $4, input_tokens = $5,
                output_tokens = $6, cost = $7, cost_currency = $8
         where tenant_id = $1 and id = $2 and status = 'pending'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(output)
    .bind(output_json)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(cost.map(|c| c.0))
    .bind(cost.map(|c| c.1))
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Sets a terminal status: `failed` (with `error`), `applied` or `discarded`.
pub async fn set_ai_generation_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AiGenerationId,
    status: AiGenerationStatus,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update ai_generations set status = $3, error = coalesce($4, error),
                applied_at = case when $3 = 'applied' then now() else applied_at end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Generations of a tenant (optionally for one entity), newest first.
pub async fn list_ai_generations<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    entity_id: Option<Uuid>,
    page: Page,
) -> DbResult<Paged<AiGenerationRow>> {
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from ai_generations where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(e) = entity_id {
                qb.push(" and entity_id = ").push_bind(e);
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}

/// Token usage since a point in time: `(generations, input_tokens, output_tokens)`.
pub async fn ai_usage_since(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    since: DateTime<Utc>,
) -> DbResult<(i64, i64, i64)> {
    Ok(sqlx::query_as::<_, (i64, i64, i64)>(
        "select count(*), coalesce(sum(input_tokens), 0)::bigint, coalesce(sum(output_tokens), 0)::bigint
         from ai_generations where tenant_id = $1 and created_at >= $2",
    )
    .bind(tenant_id)
    .bind(since)
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// admin helpers
// ---------------------------------------------------------------------------

/// Audit by id.
pub async fn get_seo_audit(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: SeoAuditId,
) -> DbResult<Option<SeoAuditRow>> {
    Ok(sqlx::query_as::<_, SeoAuditRow>(
        "select * from seo_audits where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Records crawl progress of a running site audit.
pub async fn set_seo_audit_progress(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: SeoAuditId,
    pages_done: i32,
    pages_total: i32,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update seo_audits set pages_done = $3, pages_total = $4
         where tenant_id = $1 and id = $2 and status in ('pending', 'running')",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(pages_done.max(0))
    .bind(pages_total.max(0))
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Marks audits that have been pending / running for longer than `minutes`
/// as failed (the process that ran them stopped). Returns how many.
pub async fn fail_stale_seo_audits(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    minutes: i32,
    error: &str,
) -> DbResult<u64> {
    let res = sqlx::query(
        "update seo_audits set status = 'failed', error = $3, completed_at = now()
         where tenant_id = $1 and status in ('pending', 'running')
           and coalesce(started_at, created_at) < now() - make_interval(mins => $2)",
    )
    .bind(tenant_id)
    .bind(minutes)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Completed audits of `url`, newest first (trend / comparison).
pub async fn completed_seo_audits(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    url: &str,
    limit: i64,
) -> DbResult<Vec<SeoAuditRow>> {
    Ok(sqlx::query_as::<_, SeoAuditRow>(
        "select * from seo_audits where tenant_id = $1 and url = $2 and status = 'completed'
         order by created_at desc limit $3",
    )
    .bind(tenant_id)
    .bind(url)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// The completed audit of the same `url` before `audit` (comparison).
pub async fn previous_seo_audit(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    audit: &SeoAuditRow,
) -> DbResult<Option<SeoAuditRow>> {
    Ok(sqlx::query_as::<_, SeoAuditRow>(
        "select * from seo_audits where tenant_id = $1 and url = $2 and status = 'completed'
           and created_at < $3 and id <> $4
         order by created_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(&audit.url)
    .bind(audit.created_at)
    .bind(audit.id)
    .fetch_optional(db)
    .await?)
}

/// Records the model that actually produced a generation.
pub async fn set_ai_generation_model(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AiGenerationId,
    model: &str,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update ai_generations set model = $3 where tenant_id = $1 and id = $2 and $3 <> ''",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(model)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// The newest generation for an entity (optionally of one purpose).
pub async fn latest_ai_generation_for(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    entity_type: &str,
    entity_id: Uuid,
    purpose: Option<AiPurpose>,
) -> DbResult<Option<AiGenerationRow>> {
    Ok(sqlx::query_as::<_, AiGenerationRow>(
        "select * from ai_generations
         where tenant_id = $1 and entity_type = $2 and entity_id = $3 and ($4::text is null or purpose = $4)
         order by created_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(entity_type)
    .bind(entity_id)
    .bind(purpose)
    .fetch_optional(db)
    .await?)
}

/// Token usage per calendar month (UTC) for the last `months` months:
/// `(month start, generations, input_tokens, output_tokens)`, oldest first.
pub async fn ai_usage_by_month(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    months: i32,
) -> DbResult<Vec<(DateTime<Utc>, i64, i64, i64)>> {
    Ok(sqlx::query_as::<_, (DateTime<Utc>, i64, i64, i64)>(
        "select date_trunc('month', created_at at time zone 'UTC') at time zone 'UTC' as m, count(*),
                coalesce(sum(input_tokens), 0)::bigint, coalesce(sum(output_tokens), 0)::bigint
         from ai_generations
         where tenant_id = $1
           and created_at >= date_trunc('month', now() at time zone 'UTC') at time zone 'UTC'
                             - make_interval(months => $2 - 1)
         group by m order by m",
    )
    .bind(tenant_id)
    .bind(months.max(1))
    .fetch_all(db)
    .await?)
}
