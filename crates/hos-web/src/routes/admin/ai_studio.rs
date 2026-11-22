//! `/admin/ai/*` — the AI studio.
//!
//! Pick a task (meta, product description, FAQ, blog draft, KB article, GEO
//! rewrite, translation tr ↔ en, keyword ideas), fill in the target / brief,
//! and the request is queued as an `ai_generation` job (`hos-jobs`) and
//! recorded in `ai_generations` (request, result, model, tokens, status,
//! who). The result screen polls with htmx, shows the proposal next to the
//! current content and applies it only on an explicit "apply" — always as a
//! **draft** of the target (or a new draft). Company facts given to the
//! model come from the database only (products, prices, features).
//!
//! Without a configured provider (no `ai` integration and no
//! `ANTHROPIC_API_KEY`) every screen shows an empty state pointing to
//! `/admin/integrations`. Generations are rate-limited per tenant.

use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{Datelike, Duration, TimeZone, Utc};
use hos_auth::rbac::Permission;
use hos_core::domain::Locale;
use hos_core::ids::{ProductId, UserId};
use hos_core::money::Money;
use hos_db::Page;
use hos_db::enums::{AiGenerationStatus, AiPurpose, ContentStatus, KbArticleKind, SeoEntityType};
use hos_db::ids::AiGenerationId;
use hos_db::repo::billing_support;
use hos_db::repo::catalog::{self, ProductInput, ProductRow, translation};
use hos_db::repo::content::{self as content_repo, ContentGroupFilter};
use hos_db::repo::identity;
use hos_db::repo::seo::{self as seo_repo, AiGenerationRow, NewAiGeneration, SeoTarget};
use hos_seo::ai_tasks::{
    BlogPost, Fact, GeoRewrite, KbArticle, KeywordSuggestion, LinkRef, MetaSuggestion, PriceFact,
    ProductCopy, ProductFacts,
};
use hos_seo::jsonld::FaqEntry;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _};
use super::content::{self, Kind, MetaFields, Row};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, PageQuery, RequirePermission, perm};
use crate::forms::FormErrors;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, escape_html, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/ai", get(studio))
        .route("/admin/ai/new", get(new_form))
        .route("/admin/ai/generate", post(generate))
        .route("/admin/ai/translate", post(translate_quick))
        .route("/admin/ai/history", get(history))
        .route("/admin/ai/generations/{id}", get(result))
        .route("/admin/ai/generations/{id}/status", get(status))
        .route("/admin/ai/generations/{id}/apply", post(apply))
        .route("/admin/ai/generations/{id}/discard", post(discard))
}

/// Studio tasks (`ai_generations.input.task`).
pub const TASKS: &[&str] = &[
    "meta",
    "product_description",
    "faq",
    "blog_post",
    "kb_article",
    "geo_rewrite",
    "translate",
    "keywords",
];

/// Writing tones (`hos_seo::ai_tasks::Tone`).
const TONES: &[&str] = &["professional", "friendly", "technical", "persuasive"];

/// Generations per tenant in ten minutes.
const RATE_10_MIN: i64 = 20;
/// Generations per tenant in a day.
const RATE_DAY: i64 = 300;

// ---------------------------------------------------------------------------
// Provider / start (shared with support and SEO)
// ---------------------------------------------------------------------------

/// Whether an AI provider is configured for the tenant.
pub async fn provider_ready(state: &AppState, tenant: &Tenant) -> bool {
    match state.billing.registry().ai(tenant.id).await {
        Ok(p) => p.is_some(),
        Err(e) => {
            tracing::warn!(error = %e, "loading the AI provider failed");
            false
        }
    }
}

/// Why a generation was not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// No provider configured.
    NotConfigured,
    /// Tenant rate limit reached.
    RateLimited,
}

/// Records a generation and queues its job.
#[allow(clippy::too_many_arguments)]
pub async fn start_generation(
    state: &AppState,
    tenant: &Tenant,
    user: UserId,
    purpose: AiPurpose,
    entity: Option<(&str, Uuid)>,
    locale: Option<Locale>,
    label: &str,
    input: Value,
) -> AppResult<Result<AiGenerationId, StartError>> {
    let provider = match state.billing.registry().ai(tenant.id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(Err(StartError::NotConfigured)),
        Err(e) => {
            tracing::warn!(error = %e, "loading the AI provider failed");
            return Ok(Err(StartError::NotConfigured));
        }
    };
    let now = Utc::now();
    let (recent, _, _) =
        seo_repo::ai_usage_since(&state.pool, tenant.id, now - Duration::minutes(10)).await?;
    let (day, _, _) =
        seo_repo::ai_usage_since(&state.pool, tenant.id, now - Duration::hours(24)).await?;
    if recent >= RATE_10_MIN || day >= RATE_DAY {
        return Ok(Err(StartError::RateLimited));
    }
    let row = seo_repo::create_ai_generation(
        &state.pool,
        tenant.id,
        &NewAiGeneration {
            user_id: Some(user),
            integration_id: provider.0,
            purpose,
            entity: entity.map(|(t, id)| (t.to_owned(), id)),
            locale,
            model: provider.1.default_model().to_owned(),
            prompt: label.to_owned(),
            input,
        },
    )
    .await?;
    let mut conn = state.pool.acquire().await?;
    hos_billing::jobs::enqueue_ai_generation(&mut conn, tenant.id, row.id).await?;
    Ok(Ok(row.id))
}

fn start_error_key(e: StartError) -> &'static str {
    match e {
        StartError::NotConfigured => "admin_content.ai.not_configured",
        StartError::RateLimited => "admin_content.ai.rate_limited",
    }
}

/// `task` of a generation (`input.task`).
pub fn task_of(g: &AiGenerationRow) -> String {
    g.input
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or("other")
        .to_owned()
}

/// Translated task name.
pub fn task_title(ctx: &PageCtx, task: &str) -> String {
    let key = format!("admin_content.ai.tasks.{task}.title");
    if ctx.cat.has(&key) {
        ctx.t(&key)
    } else {
        task.replace('_', " ")
    }
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// A generation target.
#[derive(Debug, Clone)]
pub struct Entity {
    pub entity_type: String,
    pub id: Uuid,
    pub kind: Option<Kind>,
    pub row: Option<Row>,
    pub product: Option<ProductRow>,
    pub locale: Option<Locale>,
    pub title: String,
    pub body: String,
    pub admin_url: String,
}

impl Entity {
    /// `type:id`.
    pub fn value(&self) -> String {
        format!("{}:{}", self.entity_type, self.id)
    }
}

/// Loads `type:uuid` (content row, product or product group) of the tenant.
pub async fn load_entity(
    state: &AppState,
    tenant: &Tenant,
    raw: &str,
) -> AppResult<Option<Entity>> {
    let Some((t, id)) = raw.split_once(':') else {
        return Ok(None);
    };
    let Ok(id) = Uuid::parse_str(id.trim()) else {
        return Ok(None);
    };
    if let Some(kind) = Kind::from_entity(t) {
        let Some(group) =
            content_repo::content_group_of(&state.pool, tenant.id, kind.table(), id).await?
        else {
            return Ok(None);
        };
        let rows = content::rows_in_group(state, tenant, kind, group).await?;
        let Some(row) = rows.into_iter().find(|r| r.id == Some(id)) else {
            return Ok(None);
        };
        return Ok(Some(Entity {
            entity_type: kind.entity().into(),
            id,
            kind: Some(kind),
            locale: Some(row.locale),
            title: row.title.clone(),
            body: row.body.clone(),
            admin_url: format!("{}/{group}", kind.base()),
            row: Some(row),
            product: None,
        }));
    }
    match t {
        "product" => {
            let Some(p) = catalog::get_product(&state.pool, tenant.id, ProductId::from(id)).await?
            else {
                return Ok(None);
            };
            Ok(Some(Entity {
                entity_type: "product".into(),
                id,
                kind: None,
                row: None,
                locale: None,
                title: p.name.clone(),
                body: p.description.clone().unwrap_or_default(),
                admin_url: format!("/admin/catalog/products/{id}"),
                product: Some(p),
            }))
        }
        "product_group" => {
            let groups = catalog::list_product_groups(&state.pool, tenant.id, true).await?;
            Ok(groups
                .into_iter()
                .find(|g| Uuid::from(g.id) == id)
                .map(|g| Entity {
                    entity_type: "product_group".into(),
                    id,
                    kind: None,
                    row: None,
                    product: None,
                    locale: None,
                    title: g.name.clone(),
                    body: format!(
                        "{}\n\n{}",
                        g.tagline.clone().unwrap_or_default(),
                        g.description.clone().unwrap_or_default()
                    ),
                    admin_url: format!("/admin/seo/meta/product_group/{id}"),
                }))
        }
        _ => Ok(None),
    }
}

/// A `<optgroup>` of targets.
pub struct OptGroup {
    pub label: String,
    pub options: Vec<(String, String)>,
}

async fn entity_options(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    kinds: &[Kind],
    products: bool,
    groups: bool,
) -> AppResult<Vec<OptGroup>> {
    let mut out = Vec::new();
    for kind in kinds {
        let paged = content_repo::list_content_groups(
            &state.pool,
            tenant.id,
            kind.table(),
            &ContentGroupFilter::default(),
            Page::first(100),
        )
        .await?;
        let mut options = Vec::new();
        for r in paged.items {
            for (id, title, l) in [(r.tr_id, &r.tr_title, "TR"), (r.en_id, &r.en_title, "EN")] {
                if let (Some(id), Some(title)) = (id, title) {
                    options.push((format!("{}:{id}", kind.entity()), format!("[{l}] {title}")));
                }
            }
        }
        if !options.is_empty() {
            out.push(OptGroup {
                label: ctx.t(&format!("admin_content.kinds.{}.title", kind.key())),
                options,
            });
        }
    }
    if products {
        let options: Vec<(String, String)> = catalog::list_products(
            &state.pool,
            tenant.id,
            &catalog::ProductFilter::default(),
            Page::first(200),
        )
        .await?
        .items
        .into_iter()
        .map(|p| (format!("product:{}", p.id), p.name))
        .collect();
        if !options.is_empty() {
            out.push(OptGroup {
                label: ctx.t("admin_content.ai.products"),
                options,
            });
        }
    }
    if groups {
        let options: Vec<(String, String)> =
            catalog::list_product_groups(&state.pool, tenant.id, true)
                .await?
                .into_iter()
                .map(|g| (format!("product_group:{}", g.id), g.name))
                .collect();
        if !options.is_empty() {
            out.push(OptGroup {
                label: ctx.t("admin_content.ai.product_groups"),
                options,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Facts (database only)
// ---------------------------------------------------------------------------

fn product_text(
    p: &ProductRow,
    locale: Locale,
    default: Locale,
    field: &str,
    base: &str,
) -> String {
    if locale == default {
        base.to_owned()
    } else {
        translation(&p.translations, locale.as_str(), field)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(base)
            .to_owned()
    }
}

fn product_features(p: &ProductRow, locale: Locale, default: Locale) -> Vec<String> {
    let list = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    if locale != default
        && let Some(f) = p
            .translations
            .get(locale.as_str())
            .and_then(|t| t.get("features"))
    {
        let l = list(f);
        if !l.is_empty() {
            return l;
        }
    }
    list(&p.features)
}

/// Everything the database knows about a product (names, features, enabled
/// prices in the tenant currency).
pub async fn product_facts(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    p: &ProductRow,
    locale: Locale,
) -> AppResult<ProductFacts> {
    let default = tenant.default_locale;
    let mut facts = Vec::new();
    let tagline = product_text(
        p,
        locale,
        default,
        "tagline",
        p.tagline.as_deref().unwrap_or(""),
    );
    if !tagline.is_empty() {
        facts.push(Fact::new(ctx.t("admin_content.ai.fact_tagline"), tagline));
    }
    for f in product_features(p, locale, default) {
        facts.push(Fact::new(ctx.t("admin_content.ai.fact_feature"), f));
    }
    let prices = catalog::product_pricing(&state.pool, tenant.id, p.id)
        .await?
        .into_iter()
        .filter(|x| x.is_enabled && x.currency == tenant.default_currency)
        .map(|x| PriceFact {
            cycle: x.billing_cycle,
            price: Money::new(x.price.round_dp(2), x.currency),
            setup_fee: (!x.setup_fee.is_zero())
                .then(|| Money::new(x.setup_fee.round_dp(2), x.currency)),
        })
        .collect();
    Ok(ProductFacts {
        name: product_text(p, locale, default, "name", &p.name),
        brand: tenant.display_name().to_owned(),
        product_type: Some(p.product_type),
        audience: None,
        facts,
        prices,
    })
}

/// Product facts flattened into labelled facts (blog / FAQ / KB briefs).
fn flatten_facts(ctx: &PageCtx, pf: &ProductFacts, locale: Locale) -> Vec<Fact> {
    let mut out = vec![Fact::new(
        ctx.t("admin_content.ai.fact_product"),
        pf.name.clone(),
    )];
    out.extend(pf.facts.iter().cloned());
    for p in &pf.prices {
        let mut v = format!(
            "{} / {}",
            hos_seo::llms::format_money(&p.price, locale),
            hos_seo::llms::cycle_label(p.cycle, locale)
        );
        if let Some(s) = &p.setup_fee {
            v.push_str(&format!(
                " (+{} {})",
                hos_seo::llms::format_money(s, locale),
                ctx.t("admin_content.ai.fact_setup")
            ));
        }
        out.push(Fact::new(ctx.t("admin_content.ai.fact_price"), v));
    }
    out
}

/// Storefront links the model may use (product group pages).
async fn internal_links(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    locale: Locale,
) -> AppResult<Vec<LinkRef>> {
    Ok(catalog::list_product_groups(&state.pool, tenant.id, false)
        .await?
        .into_iter()
        .take(12)
        .map(|g| LinkRef {
            title: if locale == tenant.default_locale {
                g.name.clone()
            } else {
                translation(&g.translations, locale.as_str(), "name")
                    .unwrap_or(&g.name)
                    .to_owned()
            },
            url: format!(
                "{}{}",
                ctx.origin,
                crate::locale::localize_path(
                    &format!("/{}", g.slug),
                    locale,
                    tenant.default_locale
                )
            ),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Studio home
// ---------------------------------------------------------------------------

/// A task tile.
pub struct TaskCard {
    pub key: &'static str,
    pub title: String,
    pub text: String,
    pub icon: &'static str,
}

/// A generation in a list.
pub struct GenView {
    pub g: AiGenerationRow,
    pub task: String,
    pub user: String,
}

/// Monthly usage line.
pub struct UsageRow {
    pub month: String,
    pub generations: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Template)]
#[template(path = "admin/ai/studio.html")]
struct StudioPage {
    ctx: PageCtx,
    ready: bool,
    tasks: Vec<TaskCard>,
    recent: Vec<GenView>,
    month: UsageRow,
    months: Vec<UsageRow>,
}
crate::page!(StudioPage);

fn task_icon(task: &str) -> &'static str {
    match task {
        "meta" => "search",
        "product_description" => "box",
        "faq" => "help-circle",
        "blog_post" => "newspaper",
        "kb_article" => "book",
        "geo_rewrite" => "sparkles",
        "translate" => "globe",
        _ => "tag",
    }
}

async fn gen_views(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    rows: Vec<AiGenerationRow>,
) -> AppResult<Vec<GenView>> {
    let names: HashMap<UserId, String> = identity::list_staff(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|u| (u.id, u.display_name()))
        .collect();
    Ok(rows
        .into_iter()
        .map(|g| GenView {
            task: task_title(ctx, &task_of(&g)),
            user: g
                .user_id
                .and_then(|u| names.get(&u).cloned())
                .unwrap_or_else(|| "—".into()),
            g,
        })
        .collect())
}

fn month_label(ctx: &PageCtx, d: chrono::DateTime<Utc>) -> String {
    crate::i18n::format_month(d.date_naive(), ctx.locale)
}

async fn studio(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_content.ai.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let ready = provider_ready(&state, &tenant).await;
    let tasks = TASKS
        .iter()
        .map(|k| TaskCard {
            key: k,
            title: task_title(&ctx, k),
            text: ctx.t(&format!("admin_content.ai.tasks.{k}.text")),
            icon: task_icon(k),
        })
        .collect();
    let recent_rows =
        seo_repo::list_ai_generations(&state.pool, tenant.id, None, Page::first(8)).await?;
    let recent = gen_views(&state, &tenant, &ctx, recent_rows.items).await?;
    let now = Utc::now();
    let start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(now);
    let (n, i, o) = seo_repo::ai_usage_since(&state.pool, tenant.id, start).await?;
    let month = UsageRow {
        month: month_label(&ctx, start),
        generations: n,
        input_tokens: i,
        output_tokens: o,
    };
    let months = seo_repo::ai_usage_by_month(&state.pool, tenant.id, 6)
        .await?
        .into_iter()
        .rev()
        .map(|(m, n, i, o)| UsageRow {
            month: month_label(&ctx, m),
            generations: n,
            input_tokens: i,
            output_tokens: o,
        })
        .collect();
    render(&StudioPage {
        ctx,
        ready,
        tasks,
        recent,
        month,
        months,
    })
}

#[derive(Template)]
#[template(path = "admin/ai/history.html")]
struct HistoryPage {
    ctx: PageCtx,
    rows: Vec<GenView>,
    pager: Pager,
}
crate::page!(HistoryPage);

async fn history(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    q: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_content.ai.history");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_content.ai.title", "/admin/ai")],
    );
    let paged = seo_repo::list_ai_generations(&state.pool, tenant.id, None, q.db_page()).await?;
    let pager = Pager::new(&paged, &ctx.url("/admin/ai/history"), &[]);
    let rows = gen_views(&state, &tenant, &ctx, paged.items).await?;
    render(&HistoryPage { ctx, rows, pager })
}

// ---------------------------------------------------------------------------
// New generation
// ---------------------------------------------------------------------------

/// Task form values.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TaskForm {
    pub task: String,
    pub entity: String,
    pub locale: String,
    pub to: String,
    pub keywords: String,
    pub topic: String,
    pub audience: String,
    pub tone: String,
    pub n: String,
    pub notes: String,
    pub context: String,
    pub product: String,
    pub words: String,
    pub outline: String,
    pub seeds: String,
}

impl TaskForm {
    fn read(f: &FormData) -> Self {
        Self {
            task: f.text("task", 40),
            entity: f.text("entity", 80),
            locale: f.text("locale", 5),
            to: f.text("to", 5),
            keywords: f.text("keywords", 400),
            topic: f.text("topic", 300),
            audience: f.text("audience", 200),
            tone: Some(f.text("tone", 20))
                .filter(|t| TONES.contains(&t.as_str()))
                .unwrap_or_default(),
            n: f.text("n", 4),
            notes: f.body("notes", 6000),
            context: f.body("context", 8000),
            product: f.text("product", 80),
            words: f.text("words", 6),
            outline: f.body("outline", 2000),
            seeds: f.text("seeds", 400),
        }
    }
}

#[derive(Template)]
#[template(path = "admin/ai/new.html")]
struct NewPage {
    ctx: PageCtx,
    ready: bool,
    task: String,
    task_title: String,
    task_text: String,
    form: TaskForm,
    errors: FormErrors,
    targets: Vec<OptGroup>,
    products: Vec<(String, String)>,
    tones: Vec<(String, String)>,
    locales: Vec<(String, String)>,
    needs_target: bool,
    optional_target: bool,
}
crate::page!(NewPage);

impl NewPage {
    fn task_keys(&self) -> &'static [&'static str] {
        TASKS
    }

    fn task_label(&self, task: &str) -> String {
        task_title(&self.ctx, task)
    }

    fn target_hint(&self) -> String {
        self.t(&format!("admin_content.ai.target_hint.{}", self.task))
    }

    fn topic_label(&self) -> String {
        self.t(&format!("admin_content.ai.field_topic.{}", self.task))
    }
}

async fn new_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    mut form: TaskForm,
    errors: FormErrors,
) -> AppResult<NewPage> {
    if !TASKS.contains(&form.task.as_str()) {
        form.task = "meta".into();
    }
    let task = form.task.clone();
    let title = task_title(&ctx, &task);
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_content.ai.title", "/admin/ai")],
    );
    let content_kinds = [Kind::Page, Kind::Post, Kind::Kb, Kind::News];
    let targets = match task.as_str() {
        "meta" | "keywords" => {
            entity_options(state, tenant, &ctx, &content_kinds, true, true).await?
        }
        "product_description" => entity_options(state, tenant, &ctx, &[], true, false).await?,
        "geo_rewrite" | "translate" => {
            entity_options(state, tenant, &ctx, &content_kinds, false, false).await?
        }
        _ => Vec::new(),
    };
    let products = catalog::list_products(
        &state.pool,
        tenant.id,
        &catalog::ProductFilter::default(),
        Page::first(200),
    )
    .await?
    .items
    .into_iter()
    .map(|p| (p.id.to_string(), p.name))
    .collect();
    if form.locale.is_empty() {
        form.locale = tenant.default_locale.as_str().into();
    }
    let tones = TONES
        .iter()
        .map(|t| {
            (
                (*t).to_owned(),
                ctx.t(&format!("admin_content.ai.tones.{t}")),
            )
        })
        .collect();
    let locales = Locale::ALL
        .iter()
        .map(|l| {
            (
                l.as_str().to_owned(),
                ctx.t(&format!("admin.common.lang_{}", l.as_str())),
            )
        })
        .collect();
    Ok(NewPage {
        ready: provider_ready(state, tenant).await,
        task_text: ctx.t(&format!("admin_content.ai.tasks.{task}.text")),
        task_title: title,
        needs_target: matches!(
            task.as_str(),
            "meta" | "product_description" | "geo_rewrite" | "translate"
        ),
        optional_target: task == "keywords",
        task,
        form,
        errors,
        targets,
        products,
        tones,
        locales,
        ctx,
    })
}

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    Query(form): Query<TaskForm>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let page = new_page(&state, &tenant, &staff, ctx, form, FormErrors::default()).await?;
    render(&page)
}

fn list_of(s: &str) -> Vec<String> {
    s.split([',', '\n'])
        .map(|x| x.trim().chars().take(120).collect::<String>())
        .filter(|x| !x.is_empty())
        .take(30)
        .collect()
}

/// What to queue for a validated task form.
struct Planned {
    purpose: AiPurpose,
    entity: Option<(String, Uuid)>,
    locale: Locale,
    input: Value,
}

async fn plan(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    form: &TaskForm,
    errors: &mut FormErrors,
) -> AppResult<Option<Planned>> {
    let mut locale: Locale = form.locale.parse().unwrap_or(tenant.default_locale);
    let entity = if form.entity.is_empty() {
        None
    } else {
        let e = load_entity(state, tenant, &form.entity).await?;
        if e.is_none() {
            errors.add("entity", ctx.t("admin_content.ai.target_missing"));
        }
        e
    };
    let product = match Uuid::parse_str(&form.product) {
        Ok(id) => catalog::get_product(&state.pool, tenant.id, ProductId::from(id)).await?,
        Err(_) => None,
    };
    let product_facts_flat = match &product {
        Some(p) => {
            let pf = product_facts(state, tenant, ctx, p, locale).await?;
            flatten_facts(ctx, &pf, locale)
        }
        None => Vec::new(),
    };
    let req = |errors: &mut FormErrors, k: &str, v: &str| {
        if v.trim().is_empty() {
            errors.add(k, ctx.t("admin.common.required"));
        }
    };
    let ent_ref = entity.as_ref().map(|e| (e.entity_type.clone(), e.id));
    let planned = match form.task.as_str() {
        "meta" => {
            let Some(e) = &entity else {
                errors.add("entity", ctx.t("admin_content.ai.target_required"));
                return Ok(None);
            };
            if let Some(l) = e.locale {
                locale = l;
            }
            let text = match &e.product {
                Some(p) => {
                    let pf = product_facts(state, tenant, ctx, p, locale).await?;
                    let mut s = format!("# {}\n\n", pf.name);
                    s.push_str(&product_text(
                        p,
                        locale,
                        tenant.default_locale,
                        "description",
                        p.description.as_deref().unwrap_or(""),
                    ));
                    for f in flatten_facts(ctx, &pf, locale) {
                        s.push_str(&format!("\n- {}: {}", f.label, f.value));
                    }
                    s
                }
                None => format!("# {}\n\n{}", e.title, e.body),
            };
            if text.trim().len() < 20 {
                errors.add("entity", ctx.t("admin_content.ai.target_empty"));
                return Ok(None);
            }
            Planned {
                purpose: AiPurpose::SeoMeta,
                entity: ent_ref,
                locale,
                input: json!({"task": "meta", "content": text, "keywords": list_of(&form.keywords)}),
            }
        }
        "product_description" => {
            let Some(p) = entity.as_ref().and_then(|e| e.product.clone()).or(product) else {
                errors.add("entity", ctx.t("admin_content.ai.target_required"));
                return Ok(None);
            };
            let mut facts = product_facts(state, tenant, ctx, &p, locale).await?;
            if !form.audience.is_empty() {
                facts.audience = Some(form.audience.clone());
            }
            let tone = if form.tone.is_empty() {
                "professional".to_owned()
            } else {
                form.tone.clone()
            };
            Planned {
                purpose: AiPurpose::ProductDescription,
                entity: Some(("product".into(), p.id.into())),
                locale,
                input: json!({"task": "product_description", "facts": facts, "tone": tone}),
            }
        }
        "faq" => {
            req(errors, "topic", &form.topic);
            let n: usize = form.n.trim().parse().unwrap_or(6).clamp(1, 20);
            Planned {
                purpose: AiPurpose::Faq,
                entity: product.as_ref().map(|p| ("product".into(), p.id.into())),
                locale,
                input: json!({"task": "faq", "n": n, "topic": {
                    "topic": form.topic,
                    "facts": product_facts_flat,
                    "context": (!form.context.is_empty()).then_some(&form.context),
                }}),
            }
        }
        "blog_post" => {
            req(errors, "topic", &form.topic);
            let words: u32 = form.words.trim().parse().unwrap_or(1200).clamp(300, 4000);
            let outline = list_of(&form.outline);
            Planned {
                purpose: AiPurpose::BlogPost,
                entity: None,
                locale,
                input: json!({"task": "blog_post", "brief": {
                    "topic": form.topic,
                    "audience": (!form.audience.is_empty()).then_some(&form.audience),
                    "keywords": list_of(&form.keywords),
                    "facts": product_facts_flat,
                    "target_words": words,
                    "tone": if form.tone.is_empty() { "professional" } else { form.tone.as_str() },
                    "brand": tenant.display_name(),
                    "links": internal_links(state, tenant, ctx, locale).await?,
                }, "outline": (!outline.is_empty()).then_some(outline)}),
            }
        }
        "kb_article" => {
            req(errors, "topic", &form.topic);
            Planned {
                purpose: AiPurpose::KbArticle,
                entity: None,
                locale,
                input: json!({"task": "kb_article", "brief": {
                    "question": form.topic,
                    "product": product.as_ref().map(|p| p.name.clone()),
                    "facts": product_facts_flat,
                    "notes": (!form.notes.is_empty()).then_some(&form.notes),
                    "links": internal_links(state, tenant, ctx, locale).await?,
                }}),
            }
        }
        "geo_rewrite" => {
            let Some(e) = entity.as_ref().filter(|e| e.kind.is_some()) else {
                errors.add("entity", ctx.t("admin_content.ai.target_required"));
                return Ok(None);
            };
            if e.body.trim().is_empty() {
                errors.add("entity", ctx.t("admin_content.ai.target_empty"));
                return Ok(None);
            }
            Planned {
                purpose: AiPurpose::Other,
                entity: ent_ref,
                locale: e.locale.unwrap_or(locale),
                input: json!({"task": "geo_rewrite", "content": e.body, "format": "markdown",
                              "facts": product_facts_flat}),
            }
        }
        "translate" => {
            let Some(e) = entity.as_ref().filter(|e| e.kind.is_some()) else {
                errors.add("entity", ctx.t("admin_content.ai.target_required"));
                return Ok(None);
            };
            let from = e.locale.unwrap_or(tenant.default_locale);
            let to = if from == Locale::Tr {
                Locale::En
            } else {
                Locale::Tr
            };
            Planned {
                purpose: AiPurpose::Translation,
                entity: ent_ref,
                locale: to,
                input: json!({"task": "translate", "content": format!("# {}\n\n{}", e.title, e.body),
                              "format": "markdown", "from": from.as_str(), "to": to.as_str()}),
            }
        }
        "keywords" => {
            let topic = if form.topic.is_empty() {
                entity.as_ref().map(|e| e.title.clone()).unwrap_or_default()
            } else {
                form.topic.clone()
            };
            req(errors, "topic", &topic);
            if let Some(l) = entity.as_ref().and_then(|e| e.locale) {
                locale = l;
            }
            let n: usize = form.n.trim().parse().unwrap_or(20).clamp(1, 50);
            Planned {
                purpose: AiPurpose::Other,
                entity: ent_ref,
                locale,
                input: json!({"task": "keywords", "topic": topic, "seeds": list_of(&form.seeds), "n": n}),
            }
        }
        _ => return Err(AppError::NotFound),
    };
    Ok((!errors.any()).then_some(planned))
}

async fn generate(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = TaskForm::read(&f);
    if !TASKS.contains(&form.task.as_str()) {
        return Err(AppError::NotFound);
    }
    let mut errors = FormErrors::default();
    let planned = plan(&state, &tenant, &ctx, &form, &mut errors).await?;
    let Some(p) = planned else {
        let page = new_page(&state, &tenant, &staff, ctx, form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    let started = start_generation(
        &state,
        &tenant,
        staff.user_id,
        p.purpose,
        p.entity.as_ref().map(|(t, id)| (t.as_str(), *id)),
        Some(p.locale),
        &form.task,
        p.input,
    )
    .await?;
    match started {
        Ok(id) => {
            au.log(
                &state,
                "ai.generate",
                Some(("ai_generation", id.into())),
                json!({"task": form.task, "entity": p.entity.map(|(t, id)| format!("{t}:{id}"))}),
            )
            .await?;
            Ok(common::done(
                &ctx,
                &format!("/admin/ai/generations/{id}"),
                "admin_content.ai.queued",
            ))
        }
        Err(e) => {
            errors.add_general(ctx.t(start_error_key(e)));
            let page = new_page(&state, &tenant, &staff, ctx, form, errors).await?;
            render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)
        }
    }
}

/// `POST /admin/ai/translate` — one click: translate a content row into its
/// missing language (applied as a draft after review).
async fn translate_quick(
    state: State<AppState>,
    tenant: Tenant,
    rp: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut data = f.0.clone();
    data.retain(|(k, _)| k != "task");
    data.push(("task".into(), "translate".into()));
    generate(state, tenant, rp, au, ctx, FormData(data)).await
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// The generated proposal, prepared for display.
#[derive(Debug, Clone, Default)]
pub struct Proposal {
    pub title: String,
    pub description: String,
    pub excerpt: String,
    pub slug: String,
    pub body_html: String,
    pub items: Vec<(String, String)>,
    pub bullets: Vec<String>,
    pub changes: Vec<(String, String)>,
    pub keywords: Vec<(String, String, String)>,
    pub actions: Vec<(String, String, String)>,
    pub note: String,
}

fn proposal(task: &str, g: &AiGenerationRow) -> Proposal {
    let j = g.output_json.clone().unwrap_or(Value::Null);
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let md = |t: &str| crate::markdown::render(t);
    let mut p = Proposal::default();
    match task {
        "meta" => {
            p.title = s(&j, "title");
            p.description = s(&j, "description");
        }
        "product_description" => {
            p.title = s(&j, "headline");
            p.excerpt = s(&j, "short_description");
            p.body_html = md(&s(&j, "long_description_markdown"));
            p.bullets = j
                .get("bullet_points")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
        }
        "faq" => {
            p.items = serde_json::from_value::<Vec<FaqEntry>>(j)
                .unwrap_or_default()
                .into_iter()
                .map(|e| (e.question, e.answer))
                .collect();
        }
        "blog_post" | "kb_article" => {
            p.title = s(&j, "title");
            p.slug = s(&j, "slug");
            p.excerpt = if task == "blog_post" {
                s(&j, "excerpt")
            } else {
                s(&j, "summary")
            };
            p.body_html = md(&s(&j, "body_markdown"));
            if let Some(m) = j.get("meta") {
                p.description = s(m, "description");
                p.note = s(m, "title");
            }
            p.items = j
                .get("steps")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|x| (s(x, "name"), s(x, "text"))).collect())
                .unwrap_or_default();
        }
        "geo_rewrite" => {
            p.body_html = md(&s(&j, "content_markdown"));
            p.changes = j
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|x| (s(x, "change"), s(x, "reason"))).collect())
                .unwrap_or_default();
        }
        "translate" => {
            let text = j
                .as_str()
                .map(str::to_owned)
                .or_else(|| g.output.clone())
                .unwrap_or_default();
            let (title, body) = split_title(&text);
            p.title = title;
            p.body_html = md(&body);
        }
        "keywords" => {
            p.keywords = j
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| (s(x, "keyword"), s(x, "intent"), s(x, "rationale")))
                        .collect()
                })
                .unwrap_or_default();
        }
        "summarize_audit" => {
            p.description = s(&j, "summary");
            p.actions = j
                .get("actions")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|x| (s(x, "priority"), s(x, "title"), s(x, "detail")))
                        .collect()
                })
                .unwrap_or_default();
        }
        "support_reply" => {
            p.body_html = escape_html(&s(&j, "reply")).replace('\n', "<br>");
            p.note = s(&j, "internal_note");
        }
        _ => {
            p.body_html = escape_html(g.output.as_deref().unwrap_or_default());
        }
    }
    p
}

/// `# Title\n\nbody` → `(Title, body)`.
fn split_title(text: &str) -> (String, String) {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("# ") {
        let (first, body) = rest.split_once('\n').unwrap_or((rest, ""));
        (first.trim().to_owned(), body.trim().to_owned())
    } else {
        (String::new(), t.to_owned())
    }
}

#[derive(Template)]
#[template(path = "admin/ai/result.html")]
struct ResultPage {
    ctx: PageCtx,
    g: AiGenerationRow,
    task: String,
    task_title: String,
    user: String,
    target_title: String,
    target_url: String,
    current_title: String,
    current_desc: String,
    current_html: String,
    inputs: Vec<(String, String)>,
    p: Proposal,
    can_apply: bool,
    apply_note: String,
    pending: bool,
}
crate::page!(ResultPage);

async fn load_generation(
    state: &AppState,
    tenant: &Tenant,
    id: Uuid,
) -> AppResult<AiGenerationRow> {
    billing_support::get_ai_generation(&state.pool, tenant.id, AiGenerationId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

fn inputs_of(ctx: &PageCtx, g: &AiGenerationRow) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |key: &str, v: Option<&Value>| {
        let text = match v {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            Some(Value::Array(a)) if !a.is_empty() => a
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("label")
                        .and_then(Value::as_str)
                        .map(|l| {
                            format!(
                                "{l}: {}",
                                o.get("value").and_then(Value::as_str).unwrap_or_default()
                            )
                        })
                        .unwrap_or_default(),
                    other => other.to_string(),
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            Some(Value::Number(n)) => n.to_string(),
            _ => return,
        };
        out.push((
            ctx.t(&format!("admin_content.ai.input.{key}")),
            common::ellipsis(&text, 600),
        ));
    };
    let i = &g.input;
    let brief = i
        .get("brief")
        .or_else(|| i.get("topic").filter(|t| t.is_object()));
    push("topic", i.get("topic").filter(|t| t.is_string()));
    if let Some(b) = brief {
        push("topic", b.get("topic").or_else(|| b.get("question")));
        push("audience", b.get("audience"));
        push("keywords", b.get("keywords"));
        push("facts", b.get("facts"));
        push("notes", b.get("notes").or_else(|| b.get("context")));
    }
    push("keywords", i.get("keywords"));
    push("seeds", i.get("seeds"));
    push("tone", i.get("tone"));
    push("count", i.get("n"));
    if let Some(f) = i.get("facts") {
        if f.is_object() {
            push("facts", f.get("facts"));
        } else {
            push("facts", Some(f));
        }
    }
    out
}

async fn result(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let g = load_generation(&state, &tenant, id).await?;
    let task = task_of(&g);
    let title = task_title(&ctx, &task);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_content.ai.title", "/admin/ai")],
    );
    let entity = match (&g.entity_type, g.entity_id) {
        (Some(t), Some(eid)) => load_entity(&state, &tenant, &format!("{t}:{eid}")).await?,
        _ => None,
    };
    let (mut current_title, mut current_desc, mut current_html) =
        (String::new(), String::new(), String::new());
    if let Some(e) = &entity {
        match task.as_str() {
            "meta" | "keywords" => {
                let locale = e.locale.or(g.locale).unwrap_or(tenant.default_locale);
                if let Some(target) = seo_target(e) {
                    let m = seo_repo::get_seo_meta(&state.pool, tenant.id, &target, locale).await?;
                    let mf = MetaFields::from_row(m.as_ref());
                    current_title = if mf.title.is_empty() {
                        e.title.clone()
                    } else {
                        mf.title
                    };
                    current_desc = if task == "keywords" {
                        mf.keywords
                    } else {
                        mf.description
                    };
                }
            }
            "product_description" => {
                if let Some(p) = &e.product {
                    let locale = g.locale.unwrap_or(tenant.default_locale);
                    current_title = product_text(
                        p,
                        locale,
                        tenant.default_locale,
                        "tagline",
                        p.tagline.as_deref().unwrap_or(""),
                    );
                    current_html = crate::markdown::render(&product_text(
                        p,
                        locale,
                        tenant.default_locale,
                        "description",
                        p.description.as_deref().unwrap_or(""),
                    ));
                }
            }
            _ => {
                current_title = e.title.clone();
                current_html = crate::markdown::render(&e.body);
            }
        }
    }
    let user = match g.user_id {
        Some(u) => identity::get_user(&state.pool, tenant.id, u)
            .await?
            .map(|u| u.display_name())
            .unwrap_or_default(),
        None => String::new(),
    };
    let (can_apply, apply_note) = apply_capability(&ctx, &staff, &task, entity.as_ref());
    let pending = g.status == AiGenerationStatus::Pending;
    render(&ResultPage {
        p: proposal(&task, &g),
        inputs: inputs_of(&ctx, &g),
        task_title: title,
        target_title: entity.as_ref().map(|e| e.title.clone()).unwrap_or_default(),
        target_url: entity
            .as_ref()
            .map(|e| e.admin_url.clone())
            .unwrap_or_default(),
        can_apply: can_apply && g.status == AiGenerationStatus::Completed,
        apply_note,
        pending,
        current_title,
        current_desc,
        current_html,
        user,
        task,
        g,
        ctx,
    })
}

fn seo_target(e: &Entity) -> Option<SeoTarget> {
    match (e.kind, e.entity_type.as_str()) {
        (Some(k), _) => Some(SeoTarget::Entity(k.seo_type(), e.id)),
        (None, "product") => Some(SeoTarget::Entity(SeoEntityType::Product, e.id)),
        (None, "product_group") => Some(SeoTarget::Entity(SeoEntityType::ProductGroup, e.id)),
        _ => None,
    }
}

/// Whether staff may apply this task's result, and what applying does.
fn apply_capability(
    ctx: &PageCtx,
    staff: &CurrentStaff,
    task: &str,
    entity: Option<&Entity>,
) -> (bool, String) {
    let needs = |p: Permission| staff.can(p);
    let live = entity.and_then(|e| e.row.as_ref()).is_some_and(Row::live);
    match task {
        "meta" | "keywords" if entity.is_some() => {
            (true, ctx.t(&format!("admin_content.ai.apply_note.{task}")))
        }
        "product_description" if entity.is_some() => (
            needs(Permission::PRODUCTS_MANAGE),
            ctx.t("admin_content.ai.apply_note.product_description"),
        ),
        "faq" | "kb_article" => (
            needs(Permission::SUPPORT_MANAGE),
            ctx.t(&format!("admin_content.ai.apply_note.{task}")),
        ),
        "blog_post" => (
            needs(Permission::CONTENT_MANAGE),
            ctx.t("admin_content.ai.apply_note.blog_post"),
        ),
        "geo_rewrite" | "translate" => match entity.and_then(|e| e.kind) {
            Some(k) => {
                let key = if task == "geo_rewrite" && live {
                    "admin_content.ai.apply_note.geo_rewrite_live".to_owned()
                } else {
                    format!("admin_content.ai.apply_note.{task}")
                };
                (needs(k.permission()), ctx.t(&key))
            }
            None => (false, String::new()),
        },
        _ => (false, String::new()),
    }
}

/// `GET /admin/ai/generations/{id}/status` (htmx): the pending box, or a
/// full refresh once the job finished.
async fn status(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let g = load_generation(&state, &tenant, id).await?;
    if g.status != AiGenerationStatus::Pending {
        let mut res = common::fragment(String::new());
        res.headers_mut()
            .insert("hx-refresh", http::HeaderValue::from_static("true"));
        return Ok(res);
    }
    Ok(common::fragment(pending_box(&ctx, &g)))
}

/// The polling "still working" box.
pub fn pending_box(ctx: &PageCtx, g: &AiGenerationRow) -> String {
    format!(
        "<div class=\"ai-pending\" role=\"status\" aria-live=\"polite\" hx-get=\"{}\" hx-trigger=\"every 2s\" hx-swap=\"outerHTML\"><span class=\"spinner\" aria-hidden=\"true\"></span><span>{}</span></div>",
        escape_html(&ctx.url(format!("/admin/ai/generations/{}/status", g.id))),
        escape_html(&ctx.t("admin_content.ai.working"))
    )
}

// ---------------------------------------------------------------------------
// Apply / discard
// ---------------------------------------------------------------------------

async fn discard(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let g = load_generation(&state, &tenant, id).await?;
    if matches!(
        g.status,
        AiGenerationStatus::Completed | AiGenerationStatus::Failed
    ) {
        seo_repo::set_ai_generation_status(
            &state.pool,
            tenant.id,
            g.id,
            AiGenerationStatus::Discarded,
            None,
        )
        .await?;
        au.log(
            &state,
            "ai.discard",
            Some(("ai_generation", g.id.into())),
            json!({"task": task_of(&g)}),
        )
        .await?;
    }
    Ok(common::done(
        &ctx,
        "/admin/ai",
        "admin_content.ai.discarded",
    ))
}

fn output<T: serde::de::DeserializeOwned>(g: &AiGenerationRow) -> AppResult<T> {
    g.output_json
        .clone()
        .and_then(|j| serde_json::from_value(j).ok())
        .ok_or_else(|| AppError::Conflict("the AI output cannot be applied".into()))
}

#[allow(clippy::too_many_lines)]
async fn apply(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let g = load_generation(&state, &tenant, id).await?;
    let back = format!("/admin/ai/generations/{}", g.id);
    if g.status != AiGenerationStatus::Completed {
        return Ok(common::failed(
            &ctx,
            &back,
            ctx.t("admin_content.ai.not_applicable"),
        ));
    }
    let task = task_of(&g);
    let entity = match (&g.entity_type, g.entity_id) {
        (Some(t), Some(eid)) => load_entity(&state, &tenant, &format!("{t}:{eid}")).await?,
        _ => None,
    };
    let (allowed, _) = apply_capability(&ctx, &staff, &task, entity.as_ref());
    if !allowed {
        return Err(AppError::forbidden(format!("apply {task}")));
    }
    let locale = g.locale.unwrap_or(tenant.default_locale);
    let target: String = match task.as_str() {
        "meta" | "keywords" => {
            let e = entity.as_ref().ok_or(AppError::NotFound)?;
            let target = seo_target(e).ok_or(AppError::NotFound)?;
            let l = e.locale.unwrap_or(locale);
            let existing = seo_repo::get_seo_meta(&state.pool, tenant.id, &target, l).await?;
            let mut m = MetaFields::from_row(existing.as_ref());
            if task == "meta" {
                let s: MetaSuggestion = output(&g)?;
                m.title = s.title;
                m.description = s.description;
            } else {
                let list: Vec<KeywordSuggestion> = output(&g)?;
                let mut kw = m.keyword_list();
                for k in list {
                    if kw.len() >= 20 {
                        break;
                    }
                    if !kw.iter().any(|x| x.eq_ignore_ascii_case(&k.keyword)) {
                        kw.push(k.keyword);
                    }
                }
                m.keywords = kw.join(", ");
            }
            content::save_meta(&state, &tenant, &target, l, &m).await?;
            e.admin_url.clone()
        }
        "product_description" => {
            let e = entity.as_ref().ok_or(AppError::NotFound)?;
            let p = e.product.clone().ok_or(AppError::NotFound)?;
            let copy: ProductCopy = output(&g)?;
            apply_product_copy(&state, &tenant, &p, locale, &copy).await?;
            e.admin_url.clone()
        }
        "faq" => {
            let entries: Vec<FaqEntry> = output(&g)?;
            let mut created = 0;
            for e in entries {
                let slug = content::free_slug(
                    &state,
                    &tenant,
                    Kind::Kb,
                    locale,
                    &crate::markdown::slugify(&e.question),
                    None,
                )
                .await?;
                let row = Row {
                    locale,
                    slug,
                    title: e.question,
                    body: e.answer,
                    status: ContentStatus::Draft,
                    kb_kind: KbArticleKind::Faq,
                    author_id: Some(staff.user_id),
                    ..Row::default()
                };
                content::save_row(&state, &tenant, Kind::Kb, &row).await?;
                created += 1;
            }
            format!(
                "{}?kind=faq&status=draft&created={created}",
                Kind::Kb.base()
            )
        }
        "blog_post" | "kb_article" => {
            let (kind, title, slug, excerpt, body, meta) = if task == "blog_post" {
                let p: BlogPost = output(&g)?;
                (
                    Kind::Post,
                    p.title,
                    p.slug,
                    p.excerpt,
                    p.body_markdown,
                    p.meta,
                )
            } else {
                let a: KbArticle = output(&g)?;
                (
                    Kind::Kb,
                    a.title,
                    a.slug,
                    a.summary,
                    a.body_markdown,
                    a.meta,
                )
            };
            let slug = content::free_slug(&state, &tenant, kind, locale, &slug, None).await?;
            let row = Row {
                locale,
                slug,
                title,
                excerpt: Some(excerpt).filter(|x| !x.is_empty()),
                body,
                status: ContentStatus::Draft,
                author_id: Some(staff.user_id),
                ..Row::default()
            };
            let (rid, group) = content::save_row(&state, &tenant, kind, &row).await?;
            content::save_meta(
                &state,
                &tenant,
                &SeoTarget::Entity(kind.seo_type(), rid),
                locale,
                &MetaFields {
                    title: meta.title,
                    description: meta.description,
                    ..MetaFields::default()
                },
            )
            .await?;
            format!("{}/{group}", kind.base())
        }
        "geo_rewrite" => {
            let e = entity.as_ref().ok_or(AppError::NotFound)?;
            let (kind, mut row) = (
                e.kind.ok_or(AppError::NotFound)?,
                e.row.clone().ok_or(AppError::NotFound)?,
            );
            let r: GeoRewrite = output(&g)?;
            row.body = r.content_markdown;
            row.status = ContentStatus::Draft;
            content::save_row(&state, &tenant, kind, &row).await?;
            e.admin_url.clone()
        }
        "translate" => {
            let e = entity.as_ref().ok_or(AppError::NotFound)?;
            let (kind, source) = (
                e.kind.ok_or(AppError::NotFound)?,
                e.row.clone().ok_or(AppError::NotFound)?,
            );
            let text = g
                .output_json
                .as_ref()
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| g.output.clone())
                .unwrap_or_default();
            let (title, body) = split_title(&text);
            let group = source.group.ok_or(AppError::NotFound)?;
            let siblings = content::rows_in_group(&state, &tenant, kind, group).await?;
            let row = match siblings.into_iter().find(|r| r.locale == locale) {
                Some(mut existing) => {
                    if !title.is_empty() {
                        existing.title = title;
                    }
                    existing.body = body;
                    existing.status = ContentStatus::Draft;
                    existing
                }
                None => {
                    let slug =
                        content::free_slug(&state, &tenant, kind, locale, &source.slug, None)
                            .await?;
                    Row {
                        id: None,
                        locale,
                        slug,
                        title: if title.is_empty() {
                            source.title.clone()
                        } else {
                            title
                        },
                        body,
                        excerpt: None,
                        tags: Vec::new(),
                        category: None,
                        status: ContentStatus::Draft,
                        published_at: None,
                        author_id: Some(staff.user_id),
                        ..source.clone()
                    }
                }
            };
            content::save_row(&state, &tenant, kind, &row).await?;
            e.admin_url.clone()
        }
        _ => {
            return Ok(common::failed(
                &ctx,
                &back,
                ctx.t("admin_content.ai.not_applicable"),
            ));
        }
    };
    seo_repo::set_ai_generation_status(
        &state.pool,
        tenant.id,
        g.id,
        AiGenerationStatus::Applied,
        None,
    )
    .await?;
    au.log(
        &state,
        "ai.apply",
        Some(("ai_generation", g.id.into())),
        json!({"task": task, "entity": entity.map(|e| e.value()), "locale": locale.as_str()}),
    )
    .await?;
    Ok(common::done(&ctx, &target, "admin_content.ai.applied"))
}

/// Writes generated product copy into the product (default language →
/// columns, other language → `translations.<locale>`).
async fn apply_product_copy(
    state: &AppState,
    tenant: &Tenant,
    p: &ProductRow,
    locale: Locale,
    copy: &ProductCopy,
) -> AppResult<()> {
    let mut input = ProductInput {
        group_id: p.group_id,
        slug: p.slug.clone(),
        product_type: p.product_type,
        name: p.name.clone(),
        tagline: p.tagline.clone(),
        description: p.description.clone(),
        features: p.features.clone(),
        translations: p.translations.clone(),
        integration_id: p.integration_id,
        module_config: p.module_config.clone(),
        pay_type: p.pay_type,
        auto_setup: p.auto_setup,
        requires_domain: p.requires_domain,
        welcome_email_template: p.welcome_email_template.clone(),
        is_taxable: p.is_taxable,
        is_featured: p.is_featured,
        is_hidden: p.is_hidden,
        sort_order: p.sort_order,
    };
    if locale == tenant.default_locale {
        input.tagline = Some(copy.headline.clone());
        input.description = Some(copy.long_description_markdown.clone());
        input.features = json!(copy.bullet_points);
    } else {
        let mut tr = input.translations.as_object().cloned().unwrap_or_default();
        let mut entry = tr
            .get(locale.as_str())
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        entry.insert("tagline".into(), json!(copy.headline));
        entry.insert("description".into(), json!(copy.long_description_markdown));
        entry.insert("features".into(), json!(copy.bullet_points));
        tr.insert(locale.as_str().into(), Value::Object(entry));
        input.translations = Value::Object(tr);
    }
    catalog::update_product(&state.pool, tenant.id, p.id, &input)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

/// Response for a request that needs the AI provider when none is configured.
pub fn not_configured(ctx: &PageCtx, back: &str) -> Response {
    common::failed(ctx, back, ctx.t("admin_content.ai.not_configured")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_split() {
        assert_eq!(
            split_title("# Başlık\n\nGövde metni"),
            ("Başlık".to_owned(), "Gövde metni".to_owned())
        );
        assert_eq!(split_title("Gövde"), (String::new(), "Gövde".to_owned()));
    }

    #[test]
    fn lists() {
        assert_eq!(list_of("a, b\nc,,"), vec!["a", "b", "c"]);
    }
}
