//! `/platform/tenants*` — tenant list, creation wizard, detail page and the
//! operator actions (status, subscription, limits, features, notes, custom
//! domains, impersonation, deletion).

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use hos_core::domain::{Locale, TenantStatus};
use hos_core::ids::{TenantDomainId, TenantId, TenantSubscriptionId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::Page;
use hos_db::enums::{SubscriptionStatus, TenantDomainKind};
use hos_db::repo::audit::{self, AuditFilter, AuditLogRow};
use hos_db::repo::identity::UserRow;
use hos_db::repo::platform::{self, PlatformPlanRow, TenantFilter, TenantRow};
use hos_db::repo::saas::{self, SubscriptionListRow, TenantSummary, TenantUsage};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::limits::Limit;
use super::provision::{self, NewTenantRequest, tenant_origin};
use super::{AuditCtx, audit_platform, domains, handoff, menu};
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, PlatformAdmin};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::state::AppState;
use crate::view::{Crumb, PageCtx, Pager, render, render_status};

/// Feature flags known to the console (plan `features` + per-tenant overrides).
pub const FEATURES: &[&str] = &[
    "storefront",
    "client_area",
    "billing",
    "support_desk",
    "knowledge_base",
    "seo_basics",
    "api",
    "ai_content",
    "seo_audits",
    "white_label",
    "priority_support",
    "sla",
    "sso",
];

/// Localised subscription status label.
pub(super) fn sub_status_label(ctx: &PageCtx, s: &SubscriptionStatus) -> String {
    ctx.t(&format!("platform.subscription_status.{}", s.as_str()))
}

/// Badge tone for a subscription status.
pub(super) fn sub_status_tone(s: &SubscriptionStatus) -> &'static str {
    match s {
        SubscriptionStatus::Active => "success",
        SubscriptionStatus::Trialing => "info",
        SubscriptionStatus::PastDue => "warning",
        SubscriptionStatus::Cancelled | SubscriptionStatus::Expired => "danger",
    }
}

fn parse_tenant(raw: &str) -> AppResult<TenantId> {
    raw.parse().map_err(|_| AppError::NotFound)
}

async fn load_tenant(state: &AppState, raw: &str) -> AppResult<TenantRow> {
    platform::get_tenant(&state.pool, parse_tenant(raw)?)
        .await?
        .ok_or(AppError::NotFound)
}

fn detail_url(id: TenantId) -> String {
    format!("/platform/tenants/{id}")
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    status: String,
}

#[derive(Template)]
#[template(path = "platform/tenants.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<TenantSummary>,
    pager: Pager,
    total: i64,
    q: String,
    status: String,
    statuses: Vec<(String, String)>,
}
crate::page!(ListPage);

impl ListPage {
    fn sub_status(&self, s: &SubscriptionStatus) -> String {
        sub_status_label(&self.ctx, s)
    }
}

fn status_options(ctx: &PageCtx) -> Vec<(String, String)> {
    std::iter::once((String::new(), ctx.t("platform.tenants.all_statuses")))
        .chain(TenantStatus::ALL.iter().map(|s| {
            (
                s.as_str().to_owned(),
                ctx.t(&format!("platform.tenant_status.{}", s.as_str())),
            )
        }))
        .collect()
}

/// `GET /platform/tenants`.
pub async fn list(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    q: PageQuery,
    Query(lq): Query<ListQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<TenantStatus> = lq.status.parse().ok();
    let filter = TenantFilter {
        search: q.q.clone(),
        status,
    };
    let paged = saas::list_tenant_summaries(&state.pool, &filter, q.db_page()).await?;
    let title = ctx.t("platform.tenants.title");
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(&ctx, vec![Crumb::current(title)]));
    let qs = q.q.clone().unwrap_or_default();
    let st = status.map(|s| s.as_str().to_owned()).unwrap_or_default();
    let pager = Pager::new(&paged, "/platform/tenants", &[("q", &qs), ("status", &st)]);
    let statuses = status_options(&ctx);
    render(&ListPage {
        total: paged.total,
        rows: paged.items,
        pager,
        q: qs,
        status: st,
        statuses,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// creation wizard
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct NewTenantForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    plan: String,
    #[serde(default)]
    billing_cycle: String,
    #[serde(default)]
    trial_days: String,
    #[serde(default)]
    owner_email: String,
    #[serde(default)]
    owner_first_name: String,
    #[serde(default)]
    owner_last_name: String,
    #[serde(default)]
    custom_domain: String,
    #[serde(default)]
    locale: String,
    #[serde(default)]
    currency: String,
}

impl Default for NewTenantForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            slug: String::new(),
            plan: String::new(),
            billing_cycle: "monthly".into(),
            trial_days: String::new(),
            owner_email: String::new(),
            owner_first_name: String::new(),
            owner_last_name: String::new(),
            custom_domain: String::new(),
            locale: "tr".into(),
            currency: "TRY".into(),
        }
    }
}

#[derive(Template)]
#[template(path = "platform/tenant_new.html")]
struct NewPage {
    ctx: PageCtx,
    form: NewTenantForm,
    errors: FormErrors,
    plans: Vec<(String, String)>,
    cycles: Vec<(String, String)>,
    locales: Vec<(String, String)>,
    currencies: Vec<(String, String)>,
    base_domain: String,
    steps: Vec<String>,
}
crate::page!(NewPage);

/// `(code, "Name — price/mo")` of active plans.
pub(super) fn plan_options(ctx: &PageCtx, plans: &[PlatformPlanRow]) -> Vec<(String, String)> {
    plans
        .iter()
        .filter(|p| p.is_active)
        .map(|p| {
            (
                p.code.clone(),
                format!(
                    "{} — {}{}",
                    p.name,
                    ctx.money_in(&p.price_monthly, &p.currency),
                    ctx.per(&BillingCycle::Monthly)
                ),
            )
        })
        .collect()
}

fn cycle_options(ctx: &PageCtx) -> Vec<(String, String)> {
    [BillingCycle::Monthly, BillingCycle::Annual]
        .iter()
        .map(|c| (c.as_str().to_owned(), ctx.cycle(c)))
        .collect()
}

fn locale_options() -> Vec<(String, String)> {
    vec![
        ("tr".into(), "Türkçe".into()),
        ("en".into(), "English".into()),
    ]
}

fn currency_options() -> Vec<(String, String)> {
    ["TRY", "USD", "EUR", "GBP"]
        .iter()
        .map(|c| ((*c).to_owned(), (*c).to_owned()))
        .collect()
}

async fn new_page(
    state: &AppState,
    mut ctx: PageCtx,
    form: NewTenantForm,
    errors: FormErrors,
) -> AppResult<NewPage> {
    let plans = saas::list_all_plans(&state.pool).await?;
    let title = ctx.t("platform.tenant_new.title");
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(
        &ctx,
        vec![
            Crumb::link(ctx.t("platform.tenants.title"), "/platform/tenants"),
            Crumb::current(title),
        ],
    ));
    let steps = vec![
        ctx.t("platform.tenant_new.step_company"),
        ctx.t("platform.tenant_new.step_plan"),
        ctx.t("platform.tenant_new.step_owner"),
    ];
    Ok(NewPage {
        plans: plan_options(&ctx, &plans),
        cycles: cycle_options(&ctx),
        locales: locale_options(),
        currencies: currency_options(),
        base_domain: state.config.base_domain.clone(),
        steps,
        ctx,
        form,
        errors,
    })
}

/// `GET /platform/tenants/new`.
pub async fn new_form(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    ctx: PageCtx,
) -> AppResult<Response> {
    let settings = super::PlatformSettings::load(&state).await?;
    let form = NewTenantForm {
        plan: settings.default_plan.unwrap_or_default(),
        trial_days: settings.trial_days.to_string(),
        ..NewTenantForm::default()
    };
    render(&new_page(&state, ctx, form, FormErrors::default()).await?)
}

/// `POST /platform/tenants` — runs the wizard.
pub async fn create(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<NewTenantForm>,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let slug = provision::normalize_slug(&form.slug);
    if form.name.trim().is_empty() {
        errors.add("name", ctx.t("platform.errors.required"));
    }
    match provision::check_slug(&state, &slug).await? {
        Ok(()) => {}
        Err(p) => errors.add("slug", ctx.t(p.key())),
    }
    if !forms::valid_email(&form.owner_email) {
        errors.add("owner_email", ctx.t("auth.errors.email_invalid"));
    }
    let trial_days = match form.trial_days.trim() {
        "" => None,
        t => match t.parse::<i64>() {
            Ok(n) if (0..=365).contains(&n) => Some(n),
            _ => {
                errors.add("trial_days", ctx.t("platform.errors.number"));
                None
            }
        },
    };
    let custom_domain = platform::normalize_host(&form.custom_domain);
    if !custom_domain.is_empty() && !valid_host(&custom_domain) {
        errors.add("custom_domain", ctx.t("platform.errors.host"));
    }
    if errors.any() {
        let page = new_page(&state, ctx, form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let mut request = NewTenantRequest::new(&slug, &form.name, &form.owner_email);
    request.owner_first_name = forms::clean(&form.owner_first_name, 100);
    request.owner_last_name = forms::clean(&form.owner_last_name, 100);
    request.custom_domain = (!custom_domain.is_empty()).then_some(custom_domain);
    request.plan_code = Some(form.plan.trim().to_owned()).filter(|p| !p.is_empty());
    request.billing_cycle = form.billing_cycle.parse().unwrap_or(BillingCycle::Monthly);
    request.trial_days = trial_days;
    request.locale = form.locale.parse().unwrap_or(Locale::Tr);
    request.currency = form.currency.parse().unwrap_or(Currency::TRY);
    let result = match provision::provision(&state, &request).await {
        Ok(r) => r,
        Err(AppError::Conflict(msg) | AppError::BadRequest(msg)) => {
            errors.add_general(msg);
            let page = new_page(&state, ctx, form, errors).await?;
            return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
        }
        Err(e) => return Err(e),
    };
    let tenant = &result.provisioned.tenant;
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        "platform.tenant.created",
        Some(("tenant", tenant.id.into())),
        json!({ "slug": tenant.slug, "name": tenant.name, "plan": request.plan_code, "owner": request.owner_email }),
    )
    .await;
    Ok(flash::redirect(
        &detail_url(tenant.id),
        SetFlash::success(ctx.tv("platform.tenant_new.created", "slug", &tenant.slug)),
    ))
}

/// Host name syntax: labels of `a-z0-9-`, at least one dot.
pub fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

// ---------------------------------------------------------------------------
// detail
// ---------------------------------------------------------------------------

struct LimitView {
    input: String,
    label: String,
    used: i64,
    max: String,
    percent: i64,
    over: bool,
    override_value: String,
}

struct FeatureView {
    key: String,
    enabled: bool,
    in_plan: bool,
    overridden: bool,
}

struct DomainView {
    id: String,
    host: String,
    kind: String,
    primary: bool,
    verified: bool,
    verified_at: Option<DateTime<Utc>>,
    last_checked_at: Option<DateTime<Utc>>,
    error: String,
    txt_name: String,
    txt_value: String,
    cname_target: String,
    url: String,
}

#[derive(Template)]
#[template(path = "platform/tenant.html")]
struct DetailPage {
    ctx: PageCtx,
    row: TenantRow,
    usage: TenantUsage,
    limits: Vec<LimitView>,
    features: Vec<FeatureView>,
    domains: Vec<DomainView>,
    subscription: Option<SubscriptionListRow>,
    history: Vec<SubscriptionListRow>,
    plans: Vec<(String, String)>,
    cycles: Vec<(String, String)>,
    owner: Option<UserRow>,
    notes: String,
    tenant_url: String,
    can_delete: bool,
    audit: Vec<AuditLogRow>,
}
crate::page!(DetailPage);

impl DetailPage {
    fn sub_status(&self, s: &SubscriptionStatus) -> String {
        sub_status_label(&self.ctx, s)
    }

    fn sub_tone(&self, s: &SubscriptionStatus) -> &'static str {
        sub_status_tone(s)
    }
}

fn metadata_object(tenant: &TenantRow, key: &str) -> Map<String, Value> {
    tenant
        .metadata
        .get(key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Whether `feature` is enabled for a tenant: per-tenant override
/// (`metadata.features.<key>`) or the live plan's `features`. Tenants
/// without a plan (self-host) have every feature.
pub async fn has_feature(state: &AppState, tenant_id: TenantId, feature: &str) -> AppResult<bool> {
    let tenant = platform::get_tenant(&state.pool, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(v) = metadata_object(&tenant, "features")
        .get(feature)
        .and_then(Value::as_bool)
    {
        return Ok(v);
    }
    let Some(sub) = platform::current_subscription(&state.pool, tenant_id).await? else {
        return Ok(true);
    };
    let plan = saas::get_plan(&state.pool, sub.plan_id).await?;
    Ok(plan.is_none_or(|p| plan_features(&p).iter().any(|f| f == feature)))
}

fn plan_features(plan: &PlatformPlanRow) -> Vec<String> {
    plan.features
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `GET /platform/tenants/{id}`.
pub async fn detail(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let usage = saas::tenant_usage(&state.pool, tenant.id).await?;
    let history = saas::tenant_subscriptions(&state.pool, tenant.id).await?;
    let subscription = history
        .iter()
        .find(|s| {
            matches!(
                s.subscription.status,
                SubscriptionStatus::Trialing
                    | SubscriptionStatus::Active
                    | SubscriptionStatus::PastDue
            )
        })
        .cloned();
    let plan = match &subscription {
        Some(s) => saas::get_plan(&state.pool, s.subscription.plan_id).await?,
        None => None,
    };
    let overrides = metadata_object(&tenant, "limits");
    let mut limits = Vec::new();
    for limit in Limit::ALL {
        let status = saas::plan_limit(&state.pool, tenant.id, limit.key()).await?;
        let percent = status
            .max
            .filter(|m| *m > 0)
            .map(|m| (status.used * 100 / m).min(100))
            .unwrap_or(0);
        limits.push(LimitView {
            input: format!("limit.{}", limit.key()),
            label: ctx.t(&format!("platform.limits.{}", limit.key())),
            used: status.used,
            max: status
                .max
                .map_or_else(|| ctx.t("platform.limits.unlimited"), |m| ctx.num(m)),
            percent,
            over: status.max.is_some_and(|m| status.used >= m),
            override_value: overrides
                .get(limit.key())
                .map(|v| match v {
                    Value::Null => "-".to_owned(),
                    other => other.to_string(),
                })
                .unwrap_or_default(),
        });
    }
    let in_plan = plan.as_ref().map(plan_features).unwrap_or_default();
    let feature_overrides = metadata_object(&tenant, "features");
    let features = FEATURES
        .iter()
        .map(|f| {
            let over = feature_overrides.get(*f).and_then(Value::as_bool);
            let from_plan = plan.is_none() || in_plan.iter().any(|p| p == f);
            FeatureView {
                key: (*f).to_owned(),
                enabled: over.unwrap_or(from_plan),
                in_plan: from_plan,
                overridden: over.is_some(),
            }
        })
        .collect();
    let base = state.config.base_domain.clone();
    let domains = saas::tenant_domain_checks(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|d| {
            let token = d.domain.verification_token.clone().unwrap_or_default();
            DomainView {
                id: d.domain.id.to_string(),
                url: tenant_origin(&state.config, &d.domain.host),
                txt_name: domains::txt_name(&d.domain.host),
                txt_value: domains::txt_value(&token),
                cname_target: format!("{}.{base}", tenant.slug),
                host: d.domain.host,
                kind: d.domain.kind.as_str().to_owned(),
                primary: d.domain.is_primary,
                verified: d.domain.verified_at.is_some(),
                verified_at: d.domain.verified_at,
                last_checked_at: d.last_checked_at,
                error: d.check_error.unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    let primary_host = domains
        .iter()
        .find(|d| d.primary)
        .map(|d| d.host.clone())
        .unwrap_or_else(|| format!("{}.{base}", tenant.slug));
    let owner = saas::tenant_owner(&state.pool, tenant.id).await?;
    let audit = audit::list(
        &state.pool,
        tenant.id,
        &AuditFilter::default(),
        Page::number(1, 10),
    )
    .await?
    .items;
    let all_plans = saas::list_all_plans(&state.pool).await?;
    let title = tenant.name.clone();
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(
        &ctx,
        vec![
            Crumb::link(ctx.t("platform.tenants.title"), "/platform/tenants"),
            Crumb::current(title),
        ],
    ));
    render(&DetailPage {
        notes: tenant
            .metadata
            .get("notes")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        can_delete: matches!(
            tenant.status,
            TenantStatus::Suspended | TenantStatus::Cancelled
        ),
        tenant_url: tenant_origin(&state.config, &primary_host),
        plans: plan_options(&ctx, &all_plans),
        cycles: cycle_options(&ctx),
        row: tenant,
        usage,
        limits,
        features,
        domains,
        subscription,
        history,
        owner,
        audit,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct StatusForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    reason: String,
}

/// `POST /platform/tenants/{id}/status` (`suspend`, `activate`, `trial`, `cancel`).
pub async fn set_status(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<StatusForm>,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let target = match form.action.as_str() {
        "suspend" => TenantStatus::Suspended,
        "activate" => TenantStatus::Active,
        "trial" => TenantStatus::Trial,
        "cancel" => TenantStatus::Cancelled,
        _ => return Err(AppError::bad_request("unknown action")),
    };
    let reason = forms::clean(&form.reason, 500);
    let reason = (!reason.is_empty()).then_some(reason);
    platform::set_tenant_status(&state.pool, tenant.id, target, reason.as_deref()).await?;
    if target == TenantStatus::Cancelled
        && let Some(sub) = platform::current_subscription(&state.pool, tenant.id).await?
    {
        platform::set_subscription_status(
            &state.pool,
            tenant.id,
            sub.id,
            SubscriptionStatus::Cancelled,
        )
        .await?;
    }
    state.tenants.invalidate_all();
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        &format!("platform.tenant.{}", form.action),
        Some(("tenant", tenant.id.into())),
        json!({ "from": tenant.status.as_str(), "to": target.as_str(), "reason": reason }),
    )
    .await;
    Ok(flash::redirect(
        &detail_url(tenant.id),
        SetFlash::success(ctx.t("platform.tenant.status_changed")),
    ))
}

// ---------------------------------------------------------------------------
// notes / features / limits
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct NotesForm {
    #[serde(default)]
    notes: String,
}

/// `POST /platform/tenants/{id}/notes`.
pub async fn save_notes(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<NotesForm>,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let notes: String = form.notes.trim().chars().take(10_000).collect();
    saas::set_tenant_metadata_key(&state.pool, tenant.id, "notes", &json!(notes)).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        "platform.tenant.notes",
        Some(("tenant", tenant.id.into())),
        json!({}),
    )
    .await;
    Ok(flash::redirect(
        &detail_url(tenant.id),
        SetFlash::success(ctx.t("platform.common.saved")),
    ))
}

/// `POST /platform/tenants/{id}/features` — `feature.<key>` = `plan|on|off`.
pub async fn save_features(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let mut overrides = Map::new();
    for (k, v) in &form {
        let Some(key) = k.strip_prefix("feature.") else {
            continue;
        };
        if !FEATURES.contains(&key) {
            continue;
        }
        match v.as_str() {
            "on" => {
                overrides.insert(key.to_owned(), json!(true));
            }
            "off" => {
                overrides.insert(key.to_owned(), json!(false));
            }
            _ => {}
        }
    }
    let value = Value::Object(overrides);
    saas::set_tenant_metadata_key(&state.pool, tenant.id, "features", &value).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        "platform.tenant.features",
        Some(("tenant", tenant.id.into())),
        value,
    )
    .await;
    Ok(flash::redirect(
        &detail_url(tenant.id),
        SetFlash::success(ctx.t("platform.common.saved")),
    ))
}

/// `POST /platform/tenants/{id}/limits` — `limit.<key>`: empty = plan
/// value, `-` = unlimited, a number = override.
pub async fn save_limits(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let mut overrides = Map::new();
    for (k, v) in &form {
        let Some(key) = k.strip_prefix("limit.") else {
            continue;
        };
        if Limit::from_key(key).is_none() {
            continue;
        }
        match v.trim() {
            "" => {}
            "-" => {
                overrides.insert(key.to_owned(), Value::Null);
            }
            n => match n.parse::<i64>() {
                Ok(n) if n >= 0 => {
                    overrides.insert(key.to_owned(), json!(n));
                }
                _ => {
                    return Ok(flash::redirect(
                        &detail_url(tenant.id),
                        SetFlash::error(ctx.t("platform.errors.number")),
                    ));
                }
            },
        }
    }
    let value = Value::Object(overrides);
    saas::set_tenant_metadata_key(&state.pool, tenant.id, "limits", &value).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        "platform.tenant.limits",
        Some(("tenant", tenant.id.into())),
        value,
    )
    .await;
    Ok(flash::redirect(
        &detail_url(tenant.id),
        SetFlash::success(ctx.t("platform.common.saved")),
    ))
}

// ---------------------------------------------------------------------------
// domains
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct DomainForm {
    #[serde(default)]
    host: String,
}

/// `POST /platform/tenants/{id}/domains` — adds an unverified custom domain.
pub async fn add_domain(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<DomainForm>,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let host = platform::normalize_host(&form.host);
    if !valid_host(&host) {
        return Ok(flash::redirect(
            &detail_url(tenant.id),
            SetFlash::error(ctx.t("platform.errors.host")),
        ));
    }
    let mut conn = state.pool.acquire().await?;
    match platform::add_tenant_domain(
        &mut conn,
        tenant.id,
        &host,
        TenantDomainKind::Custom,
        false,
        false,
    )
    .await
    {
        Ok(row) => {
            audit_platform(
                &state,
                &admin,
                &req,
                Some(tenant.id),
                "platform.tenant.domain_added",
                Some(("tenant_domain", row.id.into())),
                json!({ "host": host }),
            )
            .await;
            Ok(flash::redirect(
                &detail_url(tenant.id),
                SetFlash::success(ctx.t("platform.domains.added")),
            ))
        }
        Err(hos_core::CoreError::Conflict(_)) => Ok(flash::redirect(
            &detail_url(tenant.id),
            SetFlash::error(ctx.t("platform.domains.taken")),
        )),
        Err(e) => Err(e.into()),
    }
}

fn parse_domain(raw: &str) -> AppResult<TenantDomainId> {
    raw.parse().map_err(|_| AppError::NotFound)
}

/// `POST /platform/tenants/{id}/domains/{domain_id}/verify` — DNS check now.
pub async fn verify_domain(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path((id, domain_id)): Path<(String, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let row = saas::get_tenant_domain(&state.pool, tenant.id, parse_domain(&domain_id)?)
        .await?
        .ok_or(AppError::NotFound)?;
    let ok = domains::verify_domain(&state, &row).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        "platform.tenant.domain_checked",
        Some(("tenant_domain", row.domain.id.into())),
        json!({ "host": row.domain.host, "verified": ok }),
    )
    .await;
    let flash = if ok {
        SetFlash::success(ctx.t("platform.domains.verified"))
    } else {
        SetFlash::warning(ctx.t("platform.domains.not_verified"))
    };
    Ok(flash::redirect(&detail_url(tenant.id), flash))
}

/// `POST /platform/tenants/{id}/domains/{domain_id}/delete` (custom domains only).
pub async fn delete_domain(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path((id, domain_id)): Path<(String, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let row = saas::get_tenant_domain(&state.pool, tenant.id, parse_domain(&domain_id)?)
        .await?
        .ok_or(AppError::NotFound)?;
    if row.domain.kind != TenantDomainKind::Custom || row.domain.is_primary {
        return Ok(flash::redirect(
            &detail_url(tenant.id),
            SetFlash::error(ctx.t("platform.domains.cannot_delete")),
        ));
    }
    platform::delete_tenant_domain(&state.pool, tenant.id, row.domain.id).await?;
    state.tenants.invalidate_all();
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        "platform.tenant.domain_removed",
        Some(("tenant_domain", row.domain.id.into())),
        json!({ "host": row.domain.host }),
    )
    .await;
    Ok(flash::redirect(
        &detail_url(tenant.id),
        SetFlash::success(ctx.t("platform.domains.removed")),
    ))
}

// ---------------------------------------------------------------------------
// subscription
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct SubscriptionForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    subscription_id: String,
    #[serde(default)]
    days: String,
    #[serde(default)]
    plan: String,
    #[serde(default)]
    billing_cycle: String,
}

/// `POST /platform/tenants/{id}/subscription` — `extend` (days), `activate`
/// (new paid period), `cancel`, `change_plan` (plan, cycle), `start` (plan,
/// cycle: a new subscription when none is live). Platform invoicing of
/// tenants is out of scope; this only tracks the subscription state.
pub async fn subscription_action(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<SubscriptionForm>,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    let back = detail_url(tenant.id);
    let sub_id: Option<TenantSubscriptionId> = form.subscription_id.parse().ok();
    let cycle: BillingCycle = form.billing_cycle.parse().unwrap_or(BillingCycle::Monthly);
    let plan = match form.plan.trim() {
        "" => None,
        code => Some(
            platform::get_plan_by_code(&state.pool, code)
                .await?
                .ok_or_else(|| AppError::bad_request("unknown plan"))?,
        ),
    };
    let done = match (form.action.as_str(), sub_id) {
        ("extend", Some(sid)) => {
            let days = form
                .days
                .trim()
                .parse::<i32>()
                .ok()
                .filter(|d| (1..=3650).contains(d));
            let Some(days) = days else {
                return Ok(flash::redirect(
                    &back,
                    SetFlash::error(ctx.t("platform.errors.number")),
                ));
            };
            let mut conn = state.pool.acquire().await?;
            saas::extend_subscription(&mut conn, tenant.id, sid, days).await?
        }
        ("activate", Some(sid)) => {
            let end = saas::period_end_after(Utc::now(), cycle);
            let ok = saas::activate_subscription(&state.pool, tenant.id, sid, end).await?;
            if ok && tenant.status == TenantStatus::Trial {
                platform::set_tenant_status(&state.pool, tenant.id, TenantStatus::Active, None)
                    .await?;
                state.tenants.invalidate_all();
            }
            ok
        }
        ("cancel", Some(sid)) => {
            platform::set_subscription_status(
                &state.pool,
                tenant.id,
                sid,
                SubscriptionStatus::Cancelled,
            )
            .await?
        }
        ("change_plan", Some(sid)) => {
            let plan = plan.ok_or_else(|| AppError::bad_request("plan required"))?;
            saas::change_subscription_plan(&state.pool, tenant.id, sid, &plan, cycle).await?
        }
        ("start", _) => {
            let plan = plan.ok_or_else(|| AppError::bad_request("plan required"))?;
            let price = if cycle == BillingCycle::Annual {
                plan.price_annual
            } else {
                plan.price_monthly
            };
            let end = saas::period_end_after(Utc::now(), cycle);
            match platform::create_subscription(
                &state.pool,
                tenant.id,
                plan.id,
                SubscriptionStatus::Active,
                cycle,
                price,
                plan.currency,
                Some(end),
                None,
            )
            .await
            {
                Ok(_) => true,
                Err(hos_core::CoreError::Conflict(_)) => {
                    return Ok(flash::redirect(
                        &back,
                        SetFlash::error(ctx.t("platform.subscription.live_exists")),
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        }
        _ => return Err(AppError::bad_request("unknown action")),
    };
    if !done {
        return Err(AppError::NotFound);
    }
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        &format!("platform.subscription.{}", form.action),
        sub_id.map(|s| ("tenant_subscription", s.into())),
        json!({ "days": form.days, "plan": form.plan, "cycle": cycle.as_str() }),
    )
    .await;
    Ok(flash::redirect(
        &back,
        SetFlash::success(ctx.t("platform.subscription.updated")),
    ))
}

// ---------------------------------------------------------------------------
// impersonation / deletion
// ---------------------------------------------------------------------------

/// `POST /platform/tenants/{id}/impersonate` — one-time hand-off to the
/// tenant host (see [`handoff`]), audited on both sides.
pub async fn impersonate(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    if !tenant.status.is_operational() {
        return Ok(flash::redirect(
            &detail_url(tenant.id),
            SetFlash::error(ctx.t("platform.impersonate.not_operational")),
        ));
    }
    let Some(owner) = saas::tenant_owner(&state.pool, tenant.id).await? else {
        return Ok(flash::redirect(
            &detail_url(tenant.id),
            SetFlash::error(ctx.t("platform.impersonate.no_owner")),
        ));
    };
    let token = hos_core::crypto::generate_token();
    saas::create_handoff(
        &state.pool,
        &handoff::token_hash(&token),
        admin.admin.id,
        tenant.id,
        owner.id,
        Utc::now() + chrono::Duration::seconds(60),
    )
    .await?;
    audit_platform(
        &state,
        &admin,
        &req,
        Some(tenant.id),
        "platform.tenant.impersonate",
        Some(("user", owner.id.into())),
        json!({ "user_email": owner.email }),
    )
    .await;
    let host = platform::list_tenant_domains(&state.pool, tenant.id)
        .await?
        .into_iter()
        .find(|d| d.kind == TenantDomainKind::Subdomain)
        .map(|d| d.host)
        .unwrap_or_else(|| format!("{}.{}", tenant.slug, state.config.base_domain));
    let target = format!(
        "{}/platform/handoff?token={token}",
        tenant_origin(&state.config, &host)
    );
    Ok(Redirect::to(&target).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    confirm_slug: String,
}

/// `POST /platform/tenants/{id}/delete` — only suspended / cancelled
/// tenants, the slug must be typed again. Cascades every tenant row.
pub async fn delete(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<DeleteForm>,
) -> AppResult<Response> {
    let tenant = load_tenant(&state, &id).await?;
    if !matches!(
        tenant.status,
        TenantStatus::Suspended | TenantStatus::Cancelled
    ) {
        return Ok(flash::redirect(
            &detail_url(tenant.id),
            SetFlash::error(ctx.t("platform.delete.suspend_first")),
        ));
    }
    if form.confirm_slug.trim() != tenant.slug {
        return Ok(flash::redirect(
            &detail_url(tenant.id),
            SetFlash::error(ctx.t("platform.delete.slug_mismatch")),
        ));
    }
    platform::delete_tenant(&state.pool, tenant.id).await?;
    state.tenants.invalidate_all();
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.tenant.deleted",
        Some(("tenant", tenant.id.into())),
        json!({ "slug": tenant.slug, "name": tenant.name }),
    )
    .await;
    Ok(flash::redirect(
        "/platform/tenants",
        SetFlash::success(ctx.tv("platform.delete.done", "slug", &tenant.slug)),
    ))
}
