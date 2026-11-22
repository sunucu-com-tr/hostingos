//! Crawler policy / `robots.txt` (`/admin/seo/crawlers`), `llms.txt` texts
//! (`/admin/seo/llms`) and IndexNow (`/admin/seo/indexnow`).
//!
//! These screens write the `tenant_settings` keys the storefront reads
//! (`routes/storefront/files.rs`): `seo.ai_crawlers` (an
//! [`AiPolicy`] object), `seo.robots_disallow` (extra paths),
//! `seo.default_description` / `seo.llms_details` (`{"tr", "en"}`),
//! `seo.indexnow`, `seo.indexnow_auto`, `seo.indexnow_key`.

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::domain::Locale;
use hos_db::Page;
use hos_db::enums::JobStatus;
use hos_db::repo::jobs::{self, JobFilter};
use hos_db::repo::platform;
use hos_seo::robots::{AI_BOTS, AiPolicy, BotRule, RobotsConfig};
use serde_json::{Value, json};

use super::super::common::{self, Audit, FormData, Ui as _};
use super::super::content::queue_indexnow;
use super::{fetch_local, nav};
use crate::error::AppResult;
use crate::extract::{RequirePermission, perm};
use crate::forms::FormErrors;
use crate::routes::storefront::files::{ai_policy, base_disallow, indexnow_key};
use crate::seo::localized_setting;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, escape_html, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/seo/crawlers", get(crawlers).post(save_crawlers))
        .route("/admin/seo/crawlers/preview", post(crawlers_preview))
        .route("/admin/seo/llms", get(llms).post(save_llms))
        .route("/admin/seo/indexnow", get(indexnow).post(save_indexnow))
        .route("/admin/seo/indexnow/key", post(new_key))
        .route("/admin/seo/indexnow/submit", post(submit_urls))
}

// ---------------------------------------------------------------------------
// Crawlers / robots.txt
// ---------------------------------------------------------------------------

/// One catalogued AI bot.
pub struct BotRow {
    pub token: &'static str,
    pub operator: &'static str,
    pub purpose: &'static str,
    pub legacy: bool,
    pub control_only: bool,
    pub choice: String,
    pub allowed: bool,
}

/// Crawler form values.
#[derive(Debug, Clone)]
pub struct CrawlerForm {
    pub policy: AiPolicy,
    pub custom: String,
    pub disallow: String,
}

impl CrawlerForm {
    fn from_settings(policy: AiPolicy, extra: &[String]) -> Self {
        let custom = policy
            .overrides
            .iter()
            .filter(|(t, _)| hos_seo::robots::find_bot(t).is_none())
            .map(|(t, r)| format!("{t}: {}", rule_str(*r)))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            policy,
            custom,
            disallow: extra.join("\n"),
        }
    }

    fn read(f: &FormData, errors: &mut FormErrors, ctx: &PageCtx) -> Self {
        let mut overrides = BTreeMap::new();
        for b in AI_BOTS {
            match f.raw(&format!("bot_{}", b.token)) {
                "allow" => {
                    overrides.insert(b.token.to_owned(), BotRule::Allow);
                }
                "disallow" => {
                    overrides.insert(b.token.to_owned(), BotRule::Disallow);
                }
                _ => {}
            }
        }
        let custom = f.body("custom", 4000);
        for line in custom.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (token, rule) = line.split_once(':').unwrap_or((line, "disallow"));
            let token = token.trim();
            let rule = match rule.trim().to_ascii_lowercase().as_str() {
                "allow" | "izin" => Some(BotRule::Allow),
                "disallow" | "engel" | "block" => Some(BotRule::Disallow),
                _ => None,
            };
            let valid = !token.is_empty()
                && token.len() <= 64
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            match (valid, rule) {
                (true, Some(r)) => {
                    overrides.insert(token.to_owned(), r);
                }
                _ => errors.add(
                    "custom",
                    ctx.tv("admin_content.seo.crawlers.custom_invalid", "line", line),
                ),
            }
        }
        let disallow = f.body("disallow", 6000);
        Self {
            policy: AiPolicy {
                allow_search: f.flag("allow_search"),
                allow_user_fetch: f.flag("allow_user_fetch"),
                allow_training: f.flag("allow_training"),
                overrides,
            },
            custom,
            disallow,
        }
    }

    /// Validated extra disallow paths.
    fn paths(&self, errors: &mut FormErrors, ctx: &PageCtx) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for line in self
            .disallow
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            let ok = line.starts_with('/')
                && line.len() <= 200
                && !line.chars().any(char::is_whitespace)
                && !line.contains("//");
            if ok {
                if !out.iter().any(|p| p == line) {
                    out.push(line.to_owned());
                }
            } else {
                errors.add(
                    "disallow",
                    ctx.tv("admin_content.seo.crawlers.path_invalid", "path", line),
                );
            }
        }
        if out.len() > 50 {
            errors.add(
                "disallow",
                ctx.t("admin_content.seo.crawlers.too_many_paths"),
            );
        }
        out
    }
}

fn rule_str(r: BotRule) -> &'static str {
    match r {
        BotRule::Allow => "allow",
        BotRule::Disallow => "disallow",
    }
}

fn robots_preview(tenant: &Tenant, origin: &str, policy: &AiPolicy, extra: &[String]) -> String {
    let mut disallow = base_disallow(tenant.default_locale);
    disallow.extend(extra.iter().cloned());
    hos_seo::robots::render(&RobotsConfig {
        ai: policy.clone(),
        sitemaps: vec![format!("{origin}/sitemap.xml")],
        disallow,
        allow: Vec::new(),
        block_all: !tenant.is_operational(),
    })
}

async fn stored_crawlers(state: &AppState, tenant: &Tenant) -> AppResult<(AiPolicy, Vec<String>)> {
    let policy = ai_policy(
        platform::get_setting(&state.pool, tenant.id, "seo.ai_crawlers")
            .await?
            .as_ref(),
    );
    let extra = platform::get_setting(&state.pool, tenant.id, "seo.robots_disallow")
        .await?
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    Ok((policy, extra))
}

#[derive(Template)]
#[template(path = "admin/seo/crawlers.html")]
struct CrawlersPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    form: CrawlerForm,
    bots: Vec<BotRow>,
    preview: String,
    errors: FormErrors,
}
crate::page!(CrawlersPage);

impl CrawlersPage {
    fn purpose_label(&self, p: &str) -> String {
        self.t(&format!("admin_content.seo.crawlers.purpose.{p}"))
    }
}

fn crawlers_page(
    staff: &crate::extract::CurrentStaff,
    tenant: &Tenant,
    mut ctx: PageCtx,
    form: CrawlerForm,
    extra: &[String],
    errors: FormErrors,
) -> CrawlersPage {
    let title = ctx.t("admin_content.seo.crawlers.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/crawlers");
    let bots = AI_BOTS
        .iter()
        .map(|b| {
            let choice = form
                .policy
                .overrides
                .iter()
                .find(|(t, _)| t.eq_ignore_ascii_case(b.token))
                .map(|(_, r)| rule_str(*r).to_owned())
                .unwrap_or_default();
            BotRow {
                token: b.token,
                operator: b.operator,
                purpose: b.purpose.as_str(),
                legacy: b.legacy,
                control_only: b.control_token_only,
                choice,
                allowed: form.policy.allows(b.token),
            }
        })
        .collect();
    let preview = robots_preview(tenant, &ctx.origin, &form.policy, extra);
    CrawlersPage {
        tabs,
        active,
        form,
        bots,
        preview,
        errors,
        ctx,
    }
}

async fn crawlers(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let (policy, extra) = stored_crawlers(&state, &tenant).await?;
    let form = CrawlerForm::from_settings(policy, &extra);
    render(&crawlers_page(
        &staff,
        &tenant,
        ctx,
        form,
        &extra,
        FormErrors::default(),
    ))
}

async fn save_crawlers(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let form = CrawlerForm::read(&f, &mut errors, &ctx);
    let extra = form.paths(&mut errors, &ctx);
    if errors.any() {
        let page = crawlers_page(&staff, &tenant, ctx, form, &extra, errors);
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let (before, before_extra) = stored_crawlers(&state, &tenant).await?;
    let value = serde_json::to_value(&form.policy).map_err(crate::error::AppError::internal)?;
    platform::set_setting(&state.pool, tenant.id, "seo.ai_crawlers", &value).await?;
    if extra.is_empty() {
        platform::delete_setting(&state.pool, tenant.id, "seo.robots_disallow").await?;
    } else {
        platform::set_setting(&state.pool, tenant.id, "seo.robots_disallow", &json!(extra)).await?;
    }
    au.log(
        &state,
        "seo.crawlers",
        None,
        json!({"before": {"policy": before, "disallow": before_extra}, "after": {"policy": form.policy, "disallow": extra}}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/seo/crawlers",
        "admin.common.saved",
    ))
}

/// `POST /admin/seo/crawlers/preview` (htmx): robots.txt of the unsaved form.
async fn crawlers_preview(
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let form = CrawlerForm::read(&f, &mut errors, &ctx);
    let extra = form.paths(&mut errors, &ctx);
    let text = robots_preview(&tenant, &ctx.origin, &form.policy, &extra);
    let mut html = String::new();
    if errors.any() {
        html.push_str(&format!(
            "<p class=\"field__error\">{}</p>",
            escape_html(&ctx.t("admin_content.seo.crawlers.preview_invalid"))
        ));
    }
    html.push_str(&format!(
        "<pre class=\"code-block robots-preview\"><code>{}</code></pre>",
        escape_html(&text)
    ));
    Ok(common::fragment(html))
}

// ---------------------------------------------------------------------------
// llms.txt
// ---------------------------------------------------------------------------

/// llms.txt form values.
#[derive(Debug, Clone, Default)]
pub struct LlmsForm {
    pub summary_tr: String,
    pub summary_en: String,
    pub details_tr: String,
    pub details_en: String,
}

#[derive(Template)]
#[template(path = "admin/seo/llms.html")]
struct LlmsPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    form: LlmsForm,
    errors: FormErrors,
    preview_default: String,
    preview_other: String,
    preview_full: String,
    other_label: String,
    full_truncated: bool,
}
crate::page!(LlmsPage);

fn loc_text(v: Option<&Value>, l: Locale) -> String {
    match v {
        Some(Value::Object(m)) => m
            .get(l.as_str())
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some(other) if l == Locale::Tr => localized_setting(other, l).unwrap_or_default(),
        _ => String::new(),
    }
}

const PREVIEW_CHARS: usize = 12_000;

async fn llms_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    form: LlmsForm,
    errors: FormErrors,
) -> AppResult<LlmsPage> {
    let title = ctx.t("admin_content.seo.llms.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/llms");
    let app = crate::app(state.clone());
    let other = Locale::ALL
        .iter()
        .copied()
        .find(|l| *l != tenant.default_locale)
        .unwrap_or(Locale::En);
    let get = |path: String| {
        let app = app.clone();
        let origin = ctx.origin.clone();
        async move {
            match fetch_local(&app, &origin, &path).await {
                Some(f) if f.status == 200 => f.body,
                _ => String::new(),
            }
        }
    };
    let preview_default = get("/llms.txt".to_owned()).await;
    let preview_other = get(format!("/{}/llms.txt", other.as_str())).await;
    let full = get("/llms-full.txt".to_owned()).await;
    let full_truncated = full.chars().count() > PREVIEW_CHARS;
    Ok(LlmsPage {
        tabs,
        active,
        form,
        errors,
        preview_default,
        preview_other,
        preview_full: common::ellipsis(&full, PREVIEW_CHARS),
        other_label: format!("llms.txt · {}", other.as_str().to_uppercase()),
        full_truncated,
        ctx,
    })
}

async fn llms(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = platform::get_settings(&state.pool, tenant.id, "seo.").await?;
    let summary = s.get("seo.default_description");
    let details = s.get("seo.llms_details");
    let form = LlmsForm {
        summary_tr: loc_text(summary, Locale::Tr),
        summary_en: loc_text(summary, Locale::En),
        details_tr: loc_text(details, Locale::Tr),
        details_en: loc_text(details, Locale::En),
    };
    let page = llms_page(&state, &tenant, &staff, ctx, form, FormErrors::default()).await?;
    render(&page)
}

async fn save_llms(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = LlmsForm {
        summary_tr: f.body("summary_tr", 600),
        summary_en: f.body("summary_en", 600),
        details_tr: f.body("details_tr", 4000),
        details_en: f.body("details_en", 4000),
    };
    let mut errors = FormErrors::default();
    for (k, v, max) in [
        ("summary_tr", &form.summary_tr, 500),
        ("summary_en", &form.summary_en, 500),
        ("details_tr", &form.details_tr, 3000),
        ("details_en", &form.details_en, 3000),
    ] {
        if v.chars().count() > max {
            errors.add(k, ctx.tv("admin_content.seo.llms.too_long", "max", max));
        }
    }
    if errors.any() {
        let page = llms_page(&state, &tenant, &staff, ctx, form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let pair = |tr: &str, en: &str| -> Option<Value> {
        let mut m = serde_json::Map::new();
        if !tr.trim().is_empty() {
            m.insert("tr".into(), json!(tr.trim()));
        }
        if !en.trim().is_empty() {
            m.insert("en".into(), json!(en.trim()));
        }
        (!m.is_empty()).then_some(Value::Object(m))
    };
    for (key, value) in [
        (
            "seo.default_description",
            pair(&form.summary_tr, &form.summary_en),
        ),
        ("seo.llms_details", pair(&form.details_tr, &form.details_en)),
    ] {
        match value {
            Some(v) => platform::set_setting(&state.pool, tenant.id, key, &v).await?,
            None => {
                platform::delete_setting(&state.pool, tenant.id, key).await?;
            }
        }
    }
    au.log(
        &state,
        "seo.llms",
        None,
        json!({"summary_tr": form.summary_tr, "summary_en": form.summary_en}),
    )
    .await?;
    Ok(common::done(&ctx, "/admin/seo/llms", "admin.common.saved"))
}

// ---------------------------------------------------------------------------
// IndexNow
// ---------------------------------------------------------------------------

/// A recent IndexNow submission job.
pub struct PingRow {
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub status: JobStatus,
    pub urls: usize,
    pub submitted: String,
    pub detail: String,
}

#[derive(Template)]
#[template(path = "admin/seo/indexnow.html")]
struct IndexNowPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    enabled: bool,
    auto: bool,
    key: String,
    custom_key: bool,
    key_url: String,
    rows: Vec<PingRow>,
    urls: String,
    errors: FormErrors,
}
crate::page!(IndexNowPage);

async fn indexnow_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    urls: String,
    errors: FormErrors,
) -> AppResult<IndexNowPage> {
    let title = ctx.t("admin_content.seo.indexnow.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/indexnow");
    let s = platform::get_settings(&state.pool, tenant.id, "seo.indexnow").await?;
    let flag = |k: &str| s.get(k).and_then(Value::as_bool).unwrap_or(false);
    let key = indexnow_key(state, tenant).await?.unwrap_or_default();
    let custom_key = s
        .get("seo.indexnow_key")
        .and_then(Value::as_str)
        .is_some_and(|k| !k.is_empty());
    let rows = jobs::list_jobs(
        &state.pool,
        &JobFilter {
            tenant_id: Some(tenant.id),
            kind: Some("seo_indexnow_ping".into()),
            ..JobFilter::default()
        },
        Page::first(10),
    )
    .await?
    .items
    .into_iter()
    .map(|j| {
        let urls = j
            .payload
            .get("urls")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let submitted = j
            .result
            .as_ref()
            .and_then(|r| r.get("submitted"))
            .map(Value::to_string)
            .unwrap_or_default();
        let detail = match (&j.last_error, &j.result) {
            (Some(e), _) => e.clone(),
            (None, Some(r)) => r
                .get("batches")
                .and_then(Value::as_array)
                .map(|b| {
                    b.iter()
                        .map(|x| {
                            x.get("status")
                                .map(Value::to_string)
                                .unwrap_or_else(|| x.to_string())
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        PingRow {
            created_at: j.created_at,
            status: j.status,
            urls,
            submitted,
            detail: common::ellipsis(&detail, 200),
        }
    })
    .collect();
    Ok(IndexNowPage {
        tabs,
        active,
        enabled: flag("seo.indexnow"),
        auto: flag("seo.indexnow_auto"),
        key_url: if key.is_empty() {
            String::new()
        } else {
            format!("{}{}", ctx.origin, hos_seo::indexnow::key_file_path(&key))
        },
        key,
        custom_key,
        rows,
        urls,
        errors,
        ctx,
    })
}

async fn indexnow(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let page = indexnow_page(
        &state,
        &tenant,
        &staff,
        ctx,
        String::new(),
        FormErrors::default(),
    )
    .await?;
    render(&page)
}

async fn save_indexnow(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let enabled = f.flag("enabled");
    let auto = enabled && f.flag("auto");
    platform::set_setting(&state.pool, tenant.id, "seo.indexnow", &json!(enabled)).await?;
    platform::set_setting(&state.pool, tenant.id, "seo.indexnow_auto", &json!(auto)).await?;
    au.log(
        &state,
        "seo.indexnow",
        None,
        json!({"enabled": enabled, "auto": auto}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/seo/indexnow",
        "admin.common.saved",
    ))
}

/// `POST /admin/seo/indexnow/key` — a new random key (search engines then
/// verify it at `/{key}.txt`).
async fn new_key(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
) -> AppResult<Response> {
    let key = hos_seo::indexnow::generate_key();
    platform::set_setting(&state.pool, tenant.id, "seo.indexnow_key", &json!(key)).await?;
    au.log(&state, "seo.indexnow_key", None, json!({})).await?;
    Ok(common::done(
        &ctx,
        "/admin/seo/indexnow",
        "admin_content.seo.indexnow.key_renewed",
    ))
}

/// `POST /admin/seo/indexnow/submit` — submits URLs / paths by hand.
async fn submit_urls(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let raw = f.body("urls", 20_000);
    let mut errors = FormErrors::default();
    let mut urls = Vec::new();
    for line in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let abs = if line.starts_with('/') && !line.starts_with("//") {
            format!("{}{line}", ctx.origin)
        } else {
            line.to_owned()
        };
        match url::Url::parse(&abs) {
            Ok(u) if super::local_path(&ctx.origin, u.as_str()).is_some() => {
                urls.push(u.to_string());
            }
            _ => errors.add(
                "urls",
                ctx.tv("admin_content.seo.indexnow.url_invalid", "url", line),
            ),
        }
    }
    if urls.is_empty() && !errors.any() {
        errors.add("urls", ctx.t("admin.common.required"));
    }
    if urls.len() > 1000 {
        errors.add("urls", ctx.t("admin_content.seo.indexnow.too_many"));
    }
    if errors.any() {
        let page = indexnow_page(&state, &tenant, &staff, ctx, raw, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let count = urls.len();
    if !queue_indexnow(&state, &tenant, &ctx.origin, urls).await? {
        return Ok(common::failed(
            &ctx,
            "/admin/seo/indexnow",
            ctx.t("admin_content.seo.indexnow.disabled"),
        ));
    }
    au.log(&state, "seo.indexnow_submit", None, json!({"urls": count}))
        .await?;
    Ok(crate::flash::redirect(
        &ctx.url("/admin/seo/indexnow"),
        crate::flash::SetFlash::success(ctx.tv(
            "admin_content.seo.indexnow.queued",
            "count",
            count,
        )),
    ))
}
