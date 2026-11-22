//! `/platform/plans*` (SaaS plan CRUD) and `/platform/subscriptions`
//! (tenant subscription overview; actions live on the tenant page).

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use hos_core::ids::PlatformPlanId;
use hos_core::money::Currency;
use hos_db::enums::SubscriptionStatus;
use hos_db::repo::platform::PlatformPlanRow;
use hos_db::repo::saas::{self, PlanInput, SubscriptionListRow};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::limits::Limit;
use super::{AuditCtx, audit_platform, menu};
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, PlatformAdmin};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::state::AppState;
use crate::view::{Crumb, PageCtx, Pager, render, render_status};

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

struct PlanView {
    plan: PlatformPlanRow,
    subscriptions: i64,
    limits: String,
    features: String,
}

#[derive(Template)]
#[template(path = "platform/plans.html")]
struct PlansPage {
    ctx: PageCtx,
    plans: Vec<PlanView>,
}
crate::page!(PlansPage);

fn limits_text(ctx: &PageCtx, limits: &Value) -> String {
    Limit::ALL
        .iter()
        .map(|l| {
            let v = limits
                .get(l.key())
                .and_then(Value::as_i64)
                .map_or_else(|| "∞".to_owned(), |n| ctx.num(n));
            format!("{}: {v}", ctx.t(&format!("platform.limits.{}", l.key())))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn features_text(features: &Value) -> String {
    features
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// `GET /platform/plans`.
pub async fn list(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = saas::list_all_plans(&state.pool).await?;
    let mut plans = Vec::with_capacity(rows.len());
    for plan in rows {
        plans.push(PlanView {
            subscriptions: saas::plan_subscription_count(&state.pool, plan.id).await?,
            limits: limits_text(&ctx, &plan.limits),
            features: features_text(&plan.features),
            plan,
        });
    }
    let title = ctx.t("platform.plans.title");
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(&ctx, vec![Crumb::current(title)]));
    render(&PlansPage { ctx, plans })
}

// ---------------------------------------------------------------------------
// form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlanForm {
    #[serde(default)]
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    price_monthly: String,
    #[serde(default)]
    price_annual: String,
    #[serde(default)]
    trial_days: String,
    #[serde(default)]
    max_clients: String,
    #[serde(default)]
    max_services: String,
    #[serde(default)]
    max_staff: String,
    #[serde(default)]
    custom_domains: String,
    #[serde(default)]
    integrations: String,
    #[serde(default)]
    features: String,
    #[serde(default)]
    is_public: Option<String>,
    #[serde(default)]
    is_active: Option<String>,
    #[serde(default)]
    sort_order: String,
}

impl PlanForm {
    fn from_row(p: &PlatformPlanRow) -> Self {
        let limit = |k: &str| {
            p.limits
                .get(k)
                .and_then(Value::as_i64)
                .map(|n| n.to_string())
                .unwrap_or_default()
        };
        Self {
            code: p.code.clone(),
            name: p.name.clone(),
            description: p.description.clone().unwrap_or_default(),
            currency: p.currency.as_str().to_owned(),
            price_monthly: p.price_monthly.normalize().to_string(),
            price_annual: p.price_annual.normalize().to_string(),
            trial_days: p.trial_days.to_string(),
            max_clients: limit("max_clients"),
            max_services: limit("max_services"),
            max_staff: limit("max_staff"),
            custom_domains: limit("custom_domains"),
            integrations: limit("integrations"),
            features: features_text(&p.features),
            is_public: p.is_public.then(|| "1".to_owned()),
            is_active: p.is_active.then(|| "1".to_owned()),
            sort_order: p.sort_order.to_string(),
        }
    }

    fn public(&self) -> bool {
        forms::checked(self.is_public.as_deref())
    }

    fn active(&self) -> bool {
        forms::checked(self.is_active.as_deref())
    }

    fn validate(&self, ctx: &PageCtx) -> Result<PlanInput, FormErrors> {
        let mut errors = FormErrors::default();
        let code = self.code.trim().to_ascii_lowercase();
        if code.is_empty()
            || code.len() > 40
            || !code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            errors.add("code", ctx.t("platform.plans.code_invalid"));
        }
        if self.name.trim().is_empty() {
            errors.add("name", ctx.t("platform.errors.required"));
        }
        let currency: Option<Currency> = self.currency.trim().to_ascii_uppercase().parse().ok();
        if currency.is_none() {
            errors.add("currency", ctx.t("platform.errors.currency"));
        }
        let money = |field: &str, raw: &str, errors: &mut FormErrors| -> Decimal {
            match raw.trim().replace(',', ".").parse::<Decimal>() {
                Ok(d) if d >= Decimal::ZERO => d,
                _ => {
                    errors.add(field, ctx.t("platform.errors.number"));
                    Decimal::ZERO
                }
            }
        };
        let monthly = money("price_monthly", &self.price_monthly, &mut errors);
        let annual = money("price_annual", &self.price_annual, &mut errors);
        let int = |field: &str, raw: &str, errors: &mut FormErrors| -> i32 {
            match raw.trim().parse::<i32>() {
                Ok(n) if n >= 0 => n,
                _ if raw.trim().is_empty() => 0,
                _ => {
                    errors.add(field, ctx.t("platform.errors.number"));
                    0
                }
            }
        };
        let trial_days = int("trial_days", &self.trial_days, &mut errors);
        let sort_order = int("sort_order", &self.sort_order, &mut errors);
        let mut limits = Map::new();
        for (key, raw) in [
            ("max_clients", &self.max_clients),
            ("max_services", &self.max_services),
            ("max_staff", &self.max_staff),
            ("custom_domains", &self.custom_domains),
            ("integrations", &self.integrations),
        ] {
            match raw.trim() {
                "" => {
                    limits.insert(key.to_owned(), Value::Null);
                }
                n => match n.parse::<i64>() {
                    Ok(n) if n >= 0 => {
                        limits.insert(key.to_owned(), json!(n));
                    }
                    _ => errors.add(key, ctx.t("platform.errors.number")),
                },
            }
        }
        let features: Vec<String> = self
            .features
            .split([',', '\n'])
            .map(|f| f.trim().to_ascii_lowercase())
            .filter(|f| !f.is_empty())
            .collect();
        if errors.any() {
            return Err(errors);
        }
        Ok(PlanInput {
            code,
            name: forms::clean(&self.name, 100),
            description: Some(self.description.trim().to_owned()).filter(|d| !d.is_empty()),
            currency: currency.unwrap_or(Currency::USD),
            price_monthly: monthly,
            price_annual: annual,
            trial_days,
            limits: Value::Object(limits),
            features: json!(features),
            is_public: self.public(),
            is_active: self.active(),
            sort_order,
        })
    }
}

#[derive(Template)]
#[template(path = "platform/plan_form.html")]
struct PlanFormPage {
    ctx: PageCtx,
    form: PlanForm,
    errors: FormErrors,
    action: String,
    editing: bool,
    is_public: bool,
    is_active: bool,
    known_features: String,
}
crate::page!(PlanFormPage);

fn form_page(
    mut ctx: PageCtx,
    form: PlanForm,
    errors: FormErrors,
    id: Option<PlatformPlanId>,
) -> PlanFormPage {
    let title = if id.is_some() {
        ctx.tv("platform.plans.edit_title", "name", &form.name)
    } else {
        ctx.t("platform.plans.new_title")
    };
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(
        &ctx,
        vec![
            Crumb::link(ctx.t("platform.plans.title"), "/platform/plans"),
            Crumb::current(title),
        ],
    ));
    PlanFormPage {
        action: id.map_or_else(
            || "/platform/plans".to_owned(),
            |i| format!("/platform/plans/{i}"),
        ),
        editing: id.is_some(),
        is_public: form.public(),
        is_active: form.active(),
        known_features: super::tenants::FEATURES.join(", "),
        ctx,
        form,
        errors,
    }
}

/// `GET /platform/plans/new`.
pub async fn new_form(_admin: PlatformAdmin, ctx: PageCtx) -> AppResult<Response> {
    let form = PlanForm {
        currency: "USD".into(),
        price_monthly: "0".into(),
        price_annual: "0".into(),
        trial_days: "14".into(),
        is_public: Some("1".into()),
        is_active: Some("1".into()),
        sort_order: "100".into(),
        ..PlanForm::default()
    };
    render(&form_page(ctx, form, FormErrors::default(), None))
}

/// `POST /platform/plans`.
pub async fn create(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<PlanForm>,
) -> AppResult<Response> {
    let input = match form.validate(&ctx) {
        Ok(i) => i,
        Err(errors) => {
            return render_status(
                http::StatusCode::UNPROCESSABLE_ENTITY,
                &form_page(ctx, form, errors, None),
            );
        }
    };
    let plan = match saas::create_plan(&state.pool, &input).await {
        Ok(p) => p,
        Err(hos_core::CoreError::Conflict(_)) => {
            let mut errors = FormErrors::default();
            errors.add("code", ctx.t("platform.plans.code_taken"));
            return render_status(
                http::StatusCode::UNPROCESSABLE_ENTITY,
                &form_page(ctx, form, errors, None),
            );
        }
        Err(e) => return Err(e.into()),
    };
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.plan.created",
        Some(("platform_plan", plan.id.into())),
        json!({ "code": plan.code }),
    )
    .await;
    Ok(flash::redirect(
        "/platform/plans",
        SetFlash::success(ctx.t("platform.common.saved")),
    ))
}

fn parse_plan(raw: &str) -> AppResult<PlatformPlanId> {
    raw.parse().map_err(|_| AppError::NotFound)
}

/// `GET /platform/plans/{id}`.
pub async fn edit_form(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let plan = saas::get_plan(&state.pool, parse_plan(&id)?)
        .await?
        .ok_or(AppError::NotFound)?;
    render(&form_page(
        ctx,
        PlanForm::from_row(&plan),
        FormErrors::default(),
        Some(plan.id),
    ))
}

/// `POST /platform/plans/{id}`.
pub async fn update(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(form): Form<PlanForm>,
) -> AppResult<Response> {
    let id = parse_plan(&id)?;
    let input = match form.validate(&ctx) {
        Ok(i) => i,
        Err(errors) => {
            return render_status(
                http::StatusCode::UNPROCESSABLE_ENTITY,
                &form_page(ctx, form, errors, Some(id)),
            );
        }
    };
    match saas::update_plan(&state.pool, id, &input).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err(AppError::NotFound),
        Err(hos_core::CoreError::Conflict(_)) => {
            let mut errors = FormErrors::default();
            errors.add("code", ctx.t("platform.plans.code_taken"));
            return render_status(
                http::StatusCode::UNPROCESSABLE_ENTITY,
                &form_page(ctx, form, errors, Some(id)),
            );
        }
        Err(e) => return Err(e.into()),
    }
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.plan.updated",
        Some(("platform_plan", id.into())),
        json!({ "code": input.code, "limits": input.limits, "features": input.features }),
    )
    .await;
    Ok(flash::redirect(
        "/platform/plans",
        SetFlash::success(ctx.t("platform.common.saved")),
    ))
}

/// `POST /platform/plans/{id}/delete` — only never-subscribed plans;
/// others must be deactivated instead.
pub async fn delete(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = parse_plan(&id)?;
    if !saas::delete_plan(&state.pool, id).await? {
        return Ok(flash::redirect(
            "/platform/plans",
            SetFlash::error(ctx.t("platform.plans.in_use")),
        ));
    }
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.plan.deleted",
        Some(("platform_plan", id.into())),
        json!({}),
    )
    .await;
    Ok(flash::redirect(
        "/platform/plans",
        SetFlash::success(ctx.t("platform.plans.deleted")),
    ))
}

// ---------------------------------------------------------------------------
// subscriptions
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct SubsQuery {
    #[serde(default)]
    status: String,
}

#[derive(Template)]
#[template(path = "platform/subscriptions.html")]
struct SubscriptionsPage {
    ctx: PageCtx,
    rows: Vec<SubscriptionListRow>,
    pager: Pager,
    total: i64,
    status: String,
    statuses: Vec<(String, String)>,
}
crate::page!(SubscriptionsPage);

impl SubscriptionsPage {
    fn sub_status(&self, s: &SubscriptionStatus) -> String {
        super::tenants::sub_status_label(&self.ctx, s)
    }

    fn sub_tone(&self, s: &SubscriptionStatus) -> &'static str {
        super::tenants::sub_status_tone(s)
    }
}

/// Localised subscription status options (`""` = all).
pub(super) fn subscription_statuses(ctx: &PageCtx) -> Vec<(String, String)> {
    std::iter::once((String::new(), ctx.t("platform.subscriptions.all")))
        .chain(SubscriptionStatus::ALL.iter().map(|s| {
            (
                s.as_str().to_owned(),
                ctx.t(&format!("platform.subscription_status.{}", s.as_str())),
            )
        }))
        .collect()
}

/// `GET /platform/subscriptions`.
pub async fn subscriptions(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    q: PageQuery,
    Query(sq): Query<SubsQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<SubscriptionStatus> = sq.status.parse().ok();
    let paged = saas::list_subscriptions(&state.pool, status, q.db_page()).await?;
    let title = ctx.t("platform.subscriptions.title");
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(&ctx, vec![Crumb::current(title)]));
    let st = status.map(|s| s.as_str().to_owned()).unwrap_or_default();
    let pager = Pager::new(&paged, "/platform/subscriptions", &[("status", &st)]);
    let statuses = subscription_statuses(&ctx);
    render(&SubscriptionsPage {
        total: paged.total,
        rows: paged.items,
        pager,
        status: st,
        statuses,
        ctx,
    })
}
