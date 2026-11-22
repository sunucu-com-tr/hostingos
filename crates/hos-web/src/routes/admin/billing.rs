//! Billing setup: coupons (`/admin/billing/coupons`), tax rules
//! (`/admin/billing/taxes`), currencies + exchange rates
//! (`/admin/billing/currencies`) and the gateway log
//! (`/admin/billing/gateway-log`: incoming webhooks + payment attempts).

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::domain::IntegrationCategory;
use hos_core::ids::{CouponId, IntegrationId, ProductId, TaxRuleId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::Page;
use hos_db::enums::DiscountType;
use hos_db::repo::admin::{self as arepo, AttemptRow, CouponUpdate};
use hos_db::repo::catalog::{self, CouponInput, CouponRow, CurrencyInput, CurrencyRow, TaxRuleRow};
use hos_db::repo::integrations::{self, IntegrationRow};
use hos_db::repo::webhooks::WebhookInRow;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/billing/coupons", get(coupons).post(coupon_create))
        .route("/admin/billing/coupons/new", get(coupon_new))
        .route(
            "/admin/billing/coupons/{id}",
            get(coupon_edit).post(coupon_update),
        )
        .route("/admin/billing/coupons/{id}/delete", post(coupon_delete))
        .route("/admin/billing/taxes", get(taxes).post(tax_create))
        .route("/admin/billing/taxes/{id}", post(tax_update))
        .route("/admin/billing/taxes/{id}/delete", post(tax_delete))
        .route(
            "/admin/billing/currencies",
            get(currencies).post(currency_save),
        )
        .route(
            "/admin/billing/currencies/{code}/default",
            post(currency_default),
        )
        .route(
            "/admin/billing/currencies/{code}/active",
            post(currency_active),
        )
        .route("/admin/billing/gateway-log", get(gateway_log))
}

// ---------------------------------------------------------------------------
// Coupons
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/billing/coupons.html")]
struct CouponsPage {
    ctx: PageCtx,
    rows: Vec<CouponRow>,
}
crate::page!(CouponsPage);

async fn coupons(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = catalog::list_coupons(&state.pool, tenant.id).await?;
    let title = ctx.t("admin.coupons.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&CouponsPage { ctx, rows })
}

impl CouponsPage {
    fn value_text(&self, c: &CouponRow) -> String {
        match c.discount_type {
            DiscountType::Percent => format!("%{}", self.dec(&c.value)),
            DiscountType::FreeSetup => self.t("admin.coupons.types.free_setup"),
            _ => match c.currency {
                Some(cur) => self.m(&c.value, &cur),
                None => self.dec(&c.value),
            },
        }
    }
}

/// Form state of a coupon.
#[derive(Debug, Clone, Default)]
pub struct CouponForm {
    pub code: String,
    pub description: String,
    pub discount_type: String,
    pub value: String,
    pub currency: String,
    pub product_ids: Vec<String>,
    pub cycles: Vec<String>,
    pub applies_to_setup: bool,
    pub recurring: bool,
    pub recurring_limit: String,
    pub new_clients_only: bool,
    pub once_per_client: bool,
    pub min_order: String,
    pub max_uses: String,
    pub starts_at: String,
    pub expires_at: String,
    pub is_active: bool,
}

impl CouponForm {
    fn from_row(c: &CouponRow, tz: &str) -> Self {
        Self {
            code: c.code.clone(),
            description: c.description.clone().unwrap_or_default(),
            discount_type: c.discount_type.as_str().into(),
            value: c.value.normalize().to_string(),
            currency: c.currency.map(|c| c.to_string()).unwrap_or_default(),
            product_ids: c.product_ids.iter().map(ToString::to_string).collect(),
            cycles: c.billing_cycles.clone(),
            applies_to_setup: c.applies_to_setup,
            recurring: c.recurring,
            recurring_limit: c.recurring_limit.to_string(),
            new_clients_only: c.new_clients_only,
            once_per_client: c.once_per_client,
            min_order: c
                .min_order_amount
                .map(|d| d.normalize().to_string())
                .unwrap_or_default(),
            max_uses: c.max_uses.map(|m| m.to_string()).unwrap_or_default(),
            starts_at: common::local_input(&c.starts_at, tz),
            expires_at: common::local_input(&c.expires_at, tz),
            is_active: c.is_active,
        }
    }

    fn read(f: &FormData) -> Self {
        Self {
            code: f.text("code", 64).to_uppercase(),
            description: f.text("description", 500),
            discount_type: f.text("discount_type", 32),
            value: f.text("value", 32),
            currency: f.text("currency", 3).to_uppercase(),
            product_ids: f.all("product_ids"),
            cycles: f.all("cycles"),
            applies_to_setup: f.flag("applies_to_setup"),
            recurring: f.flag("recurring"),
            recurring_limit: f.text("recurring_limit", 6),
            new_clients_only: f.flag("new_clients_only"),
            once_per_client: f.flag("once_per_client"),
            min_order: f.text("min_order", 32),
            max_uses: f.text("max_uses", 9),
            starts_at: f.text("starts_at", 32),
            expires_at: f.text("expires_at", 32),
            is_active: f.flag("is_active"),
        }
    }

    fn validate(&self, tz: &str) -> Result<CouponUpdate, &'static str> {
        let discount_type: DiscountType = self
            .discount_type
            .parse()
            .map_err(|_| "admin.common.invalid_value")?;
        let value = common::parse_decimal(&self.value)
            .map_err(|()| "admin.common.invalid_amount")?
            .unwrap_or_default();
        if value < Decimal::ZERO
            || (discount_type == DiscountType::Percent && value > Decimal::ONE_HUNDRED)
        {
            return Err("admin.common.invalid_amount");
        }
        let currency = if self.currency.is_empty() {
            None
        } else {
            Some(
                self.currency
                    .parse::<Currency>()
                    .map_err(|_| "admin.common.invalid_value")?,
            )
        };
        if matches!(
            discount_type,
            DiscountType::Fixed | DiscountType::PriceOverride
        ) && currency.is_none()
        {
            return Err("admin.coupons.currency_required");
        }
        let product_ids = self
            .product_ids
            .iter()
            .filter_map(|p| Uuid::parse_str(p).ok().map(ProductId::from))
            .collect();
        let billing_cycles = self
            .cycles
            .iter()
            .filter(|c| c.parse::<BillingCycle>().is_ok())
            .cloned()
            .collect();
        let int = |s: &str| -> Result<Option<i32>, &'static str> {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                s.trim()
                    .parse::<i32>()
                    .ok()
                    .filter(|n| *n >= 0)
                    .map(Some)
                    .ok_or("admin.common.invalid_value")
            }
        };
        let starts_at = common::parse_local_datetime(&self.starts_at, tz)
            .map_err(|()| "admin.common.invalid_date")?;
        let expires_at = common::parse_local_datetime(&self.expires_at, tz)
            .map_err(|()| "admin.common.invalid_date")?;
        if let (Some(s), Some(e)) = (starts_at, expires_at)
            && e <= s
        {
            return Err("admin.coupons.dates_order");
        }
        Ok(CouponUpdate {
            description: Some(self.description.clone()).filter(|d| !d.is_empty()),
            discount_type,
            value,
            currency,
            product_ids,
            billing_cycles,
            applies_to_setup: self.applies_to_setup,
            recurring: self.recurring,
            recurring_limit: int(&self.recurring_limit)?.unwrap_or(0),
            new_clients_only: self.new_clients_only,
            once_per_client: self.once_per_client,
            min_order_amount: common::parse_decimal(&self.min_order)
                .map_err(|()| "admin.common.invalid_amount")?,
            max_uses: int(&self.max_uses)?,
            starts_at,
            expires_at,
            is_active: self.is_active,
        })
    }
}

#[derive(Template)]
#[template(path = "admin/billing/coupon_form.html")]
struct CouponFormPage {
    ctx: PageCtx,
    id: Option<CouponId>,
    form: CouponForm,
    error: String,
    types: Vec<(String, String)>,
    cycles: Vec<(String, String)>,
    products: Vec<(String, String)>,
    currencies: Vec<CurrencyRow>,
    uses: i32,
}
crate::page!(CouponFormPage);

impl CouponFormPage {
    fn has_product(&self, id: &str) -> bool {
        self.form.product_ids.iter().any(|p| p == id)
    }
    fn has_cycle(&self, c: &str) -> bool {
        self.form.cycles.iter().any(|p| p == c)
    }
}

#[allow(clippy::too_many_arguments)]
async fn coupon_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    id: Option<CouponId>,
    form: CouponForm,
    error: String,
    uses: i32,
) -> AppResult<Response> {
    let title = if id.is_some() {
        ctx.tv("admin.coupons.edit_title", "code", &form.code)
    } else {
        ctx.t("admin.coupons.new_title")
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin.coupons.title", "/admin/billing/coupons")],
    );
    let types = common::enum_options(
        &ctx,
        DiscountType::ALL,
        |t| t.as_str(),
        "admin.coupons.types",
    );
    let cycles = common::enum_options(&ctx, BillingCycle::ALL, |t| t.as_str(), "common.cycle");
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
    let currencies = catalog::list_currencies(&state.pool, tenant.id, false).await?;
    let page = CouponFormPage {
        ctx,
        id,
        form,
        types,
        cycles,
        products,
        currencies,
        uses,
        error: String::new(),
    };
    if error.is_empty() {
        render(&page)
    } else {
        let error = page.ctx.t(&error);
        crate::view::render_status(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            &CouponFormPage { error, ..page },
        )
    }
}

async fn coupon_new(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = CouponForm {
        discount_type: "percent".into(),
        is_active: true,
        recurring_limit: "0".into(),
        ..CouponForm::default()
    };
    coupon_page(&state, &tenant, &staff, ctx, None, form, String::new(), 0).await
}

async fn coupon_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = CouponForm::read(&f);
    let valid_code = !form.code.is_empty()
        && form
            .code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let checked = if valid_code {
        form.validate(&ctx.timezone)
    } else {
        Err("admin.coupons.invalid_code")
    };
    let upd = match checked {
        Ok(u) => u,
        Err(key) => {
            return coupon_page(&state, &tenant, &staff, ctx, None, form, key.into(), 0).await;
        }
    };
    let created = catalog::create_coupon(
        &state.pool,
        tenant.id,
        &CouponInput {
            code: form.code.clone(),
            description: upd.description.clone(),
            discount_type: upd.discount_type,
            value: upd.value,
            currency: upd.currency,
            product_ids: upd.product_ids.clone(),
            recurring: upd.recurring,
            new_clients_only: upd.new_clients_only,
            once_per_client: upd.once_per_client,
            max_uses: upd.max_uses,
            starts_at: upd.starts_at,
            expires_at: upd.expires_at,
        },
    )
    .await;
    let coupon = match created {
        Ok(c) => c,
        Err(e) if e.http_status() == 409 => {
            return coupon_page(
                &state,
                &tenant,
                &staff,
                ctx,
                None,
                form,
                "admin.coupons.code_taken".into(),
                0,
            )
            .await;
        }
        Err(e) => return Err(e.into()),
    };
    // The remaining fields (cycles, setup, limits, active flag).
    arepo::update_coupon(&state.pool, tenant.id, coupon.id, &upd).await?;
    au.log(
        &state,
        "coupon.create",
        Some(("coupon", coupon.id.into())),
        json!({"code": coupon.code}),
    )
    .await?;
    Ok(done(&ctx, "/admin/billing/coupons", "admin.common.created"))
}

async fn coupon_edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = arepo::get_coupon(&state.pool, tenant.id, id.into())
        .await?
        .ok_or(AppError::NotFound)?;
    let form = CouponForm::from_row(&c, &ctx.timezone);
    coupon_page(
        &state,
        &tenant,
        &staff,
        ctx,
        Some(c.id),
        form,
        String::new(),
        c.uses_count,
    )
    .await
}

async fn coupon_update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = arepo::get_coupon(&state.pool, tenant.id, id.into())
        .await?
        .ok_or(AppError::NotFound)?;
    let mut form = CouponForm::read(&f);
    form.code.clone_from(&c.code);
    let upd = match form.validate(&ctx.timezone) {
        Ok(u) => u,
        Err(key) => {
            return coupon_page(
                &state,
                &tenant,
                &staff,
                ctx,
                Some(c.id),
                form,
                key.into(),
                c.uses_count,
            )
            .await;
        }
    };
    arepo::update_coupon(&state.pool, tenant.id, c.id, &upd).await?;
    au.log(
        &state,
        "coupon.update",
        Some(("coupon", c.id.into())),
        json!({"code": c.code, "active": upd.is_active}),
    )
    .await?;
    Ok(done(&ctx, "/admin/billing/coupons", "admin.common.saved"))
}

async fn coupon_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = arepo::get_coupon(&state.pool, tenant.id, id.into())
        .await?
        .ok_or(AppError::NotFound)?;
    if !arepo::delete_coupon(&state.pool, tenant.id, c.id).await? {
        return Ok(failed(
            &ctx,
            "/admin/billing/coupons",
            ctx.t("admin.coupons.in_use"),
        ));
    }
    au.log(
        &state,
        "coupon.delete",
        Some(("coupon", c.id.into())),
        json!({"code": c.code}),
    )
    .await?;
    Ok(done(&ctx, "/admin/billing/coupons", "admin.common.deleted"))
}

// ---------------------------------------------------------------------------
// Tax rules
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/billing/taxes.html")]
struct TaxesPage {
    ctx: PageCtx,
    rows: Vec<TaxRuleRow>,
}
crate::page!(TaxesPage);

async fn taxes(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = catalog::list_tax_rules(&state.pool, tenant.id).await?;
    let title = ctx.t("admin.taxes.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&TaxesPage { ctx, rows })
}

struct TaxInput {
    name: String,
    level: i16,
    country: String,
    region: Option<String>,
    rate: Decimal,
    compound: bool,
}

fn read_tax(f: &FormData) -> Result<TaxInput, &'static str> {
    let name = f.text("name", 100);
    if name.is_empty() {
        return Err("admin.common.required");
    }
    let level = match f.int("level") {
        Ok(Some(l @ 1..=2)) => i16::try_from(l).unwrap_or(1),
        Ok(None) => 1,
        _ => return Err("admin.common.invalid_value"),
    };
    let country = f.text("country_code", 2).to_uppercase();
    if !country.is_empty() && !country.chars().all(|c| c.is_ascii_uppercase()) {
        return Err("admin.clients.invalid_country");
    }
    let rate = match f.decimal("rate") {
        Ok(Some(r)) if r >= Decimal::ZERO && r <= Decimal::ONE_HUNDRED => r,
        _ => return Err("admin.common.invalid_amount"),
    };
    Ok(TaxInput {
        name,
        level,
        country,
        region: f.opt("region", 100),
        rate,
        compound: f.flag("is_compound"),
    })
}

async fn tax_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/billing/taxes";
    let t = match read_tax(&f) {
        Ok(t) => t,
        Err(k) => return Ok(failed(&ctx, back, ctx.t(k))),
    };
    let row = catalog::create_tax_rule(
        &state.pool,
        tenant.id,
        &t.name,
        t.level,
        Some(t.country.as_str()).filter(|c| !c.is_empty()),
        t.region.as_deref(),
        t.rate,
        t.compound,
    )
    .await?;
    au.log(
        &state,
        "tax_rule.create",
        Some(("tax_rule", row.id.into())),
        json!({"name": row.name, "rate": row.rate.to_string(), "country": row.country_code}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.common.created"))
}

async fn tax_update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/billing/taxes";
    let t = match read_tax(&f) {
        Ok(t) => t,
        Err(k) => return Ok(failed(&ctx, back, ctx.t(k))),
    };
    let id = TaxRuleId::from(id);
    let ok = arepo::update_tax_rule(
        &state.pool,
        tenant.id,
        id,
        &t.name,
        t.level,
        &t.country,
        t.region.as_deref(),
        t.rate,
        t.compound,
        f.flag("is_active"),
    )
    .await?;
    if !ok {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "tax_rule.update",
        Some(("tax_rule", id.into())),
        json!({"name": t.name, "rate": t.rate.to_string(), "active": f.flag("is_active")}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.common.saved"))
}

async fn tax_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = TaxRuleId::from(id);
    if !arepo::delete_tax_rule(&state.pool, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "tax_rule.delete",
        Some(("tax_rule", id.into())),
        json!({}),
    )
    .await?;
    Ok(done(&ctx, "/admin/billing/taxes", "admin.common.deleted"))
}

// ---------------------------------------------------------------------------
// Currencies
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/billing/currencies.html")]
struct CurrenciesPage {
    ctx: PageCtx,
    rows: Vec<CurrencyRow>,
}
crate::page!(CurrenciesPage);

async fn currencies(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = catalog::list_currencies(&state.pool, tenant.id, false).await?;
    let title = ctx.t("admin.currencies.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&CurrenciesPage { ctx, rows })
}

async fn currency_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/billing/currencies";
    let Ok(code) = f.text("code", 3).to_uppercase().parse::<Currency>() else {
        return Ok(failed(&ctx, back, ctx.t("admin.currencies.invalid_code")));
    };
    let rate = match f.decimal("exchange_rate") {
        Ok(Some(r)) if r > Decimal::ZERO => r,
        _ => return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_amount"))),
    };
    let existing = catalog::get_currency(&state.pool, tenant.id, code).await?;
    let decimals = match f.int("decimals") {
        Ok(Some(d @ 0..=4)) => i16::try_from(d).unwrap_or(2),
        Ok(None) => existing.as_ref().map_or_else(
            || i16::try_from(code.minor_units()).unwrap_or(2),
            |c| c.decimals,
        ),
        _ => return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_value"))),
    };
    let name = f
        .opt("name", 60)
        .or_else(|| existing.as_ref().map(|c| c.name.clone()))
        .unwrap_or_else(|| code.to_string());
    let input = CurrencyInput {
        code,
        symbol: f
            .opt("symbol", 8)
            .or_else(|| existing.as_ref().map(|c| c.symbol.clone()))
            .unwrap_or_else(|| code.to_string()),
        prefix: if f.has("prefix") {
            f.raw("prefix").chars().take(8).collect()
        } else {
            existing
                .as_ref()
                .map(|c| c.prefix.clone())
                .unwrap_or_default()
        },
        suffix: if f.has("suffix") {
            f.raw("suffix").chars().take(8).collect()
        } else {
            existing
                .as_ref()
                .map_or_else(|| format!(" {code}"), |c| c.suffix.clone())
        },
        name,
        decimals,
        exchange_rate: rate,
        is_default: existing.as_ref().is_some_and(|c| c.is_default),
        sort_order: existing.as_ref().map_or(0, |c| c.sort_order),
    };
    let row = catalog::upsert_currency(&state.pool, tenant.id, &input).await?;
    au.log(
        &state,
        if existing.is_some() {
            "currency.update"
        } else {
            "currency.create"
        },
        None,
        json!({"code": row.code, "rate": row.exchange_rate.to_string()}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.common.saved"))
}

async fn currency_default(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(code): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let code: Currency = code.parse().map_err(|_| AppError::NotFound)?;
    let cur = catalog::get_currency(&state.pool, tenant.id, code)
        .await?
        .ok_or(AppError::NotFound)?;
    if !cur.is_active {
        return Ok(failed(
            &ctx,
            "/admin/billing/currencies",
            ctx.t("admin.currencies.inactive_default"),
        ));
    }
    catalog::set_default_currency(&state.pool, tenant.id, code).await?;
    au.log(&state, "currency.default", None, json!({"code": code}))
        .await?;
    Ok(done(
        &ctx,
        "/admin/billing/currencies",
        "admin.common.saved",
    ))
}

async fn currency_active(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(code): Path<String>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let code: Currency = code.parse().map_err(|_| AppError::NotFound)?;
    let cur = catalog::get_currency(&state.pool, tenant.id, code)
        .await?
        .ok_or(AppError::NotFound)?;
    let active = f.flag("active");
    if cur.is_default && !active {
        return Ok(failed(
            &ctx,
            "/admin/billing/currencies",
            ctx.t("admin.currencies.default_locked"),
        ));
    }
    arepo::set_currency_active(&state.pool, tenant.id, code, active).await?;
    au.log(
        &state,
        "currency.active",
        None,
        json!({"code": code, "active": active}),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/billing/currencies",
        "admin.common.saved",
    ))
}

// ---------------------------------------------------------------------------
// Gateway log
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct LogFilter {
    tab: Option<String>,
    integration: Option<String>,
}

/// A webhook row with its pretty payload.
pub struct HookView {
    pub row: WebhookInRow,
    pub integration: String,
    pub payload: String,
}

#[derive(Template)]
#[template(path = "admin/billing/gateway_log.html")]
struct GatewayLogPage {
    ctx: PageCtx,
    tab: String,
    integration: String,
    integrations: Vec<IntegrationRow>,
    hooks: Vec<HookView>,
    attempts: Vec<AttemptRow>,
    pager: Pager,
}
crate::page!(GatewayLogPage);

async fn gateway_log(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    q: PageQuery,
    Query(f): Query<LogFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let tab = if f.tab.as_deref() == Some("attempts") {
        "attempts"
    } else {
        "webhooks"
    };
    let integration = f.integration.clone().unwrap_or_default();
    let integration_id = Uuid::parse_str(&integration).ok().map(IntegrationId::from);
    let all = integrations::list_integrations(&state.pool, tenant.id, None).await?;
    let name_of = |id: IntegrationId| {
        all.iter()
            .find(|i| i.id == id)
            .map(|i| i.name.clone())
            .unwrap_or_default()
    };
    let (hooks, attempts, pager) = if tab == "attempts" {
        let paged = arepo::list_payment_attempts(&state.pool, tenant.id, q.db_page()).await?;
        let pager = Pager::new(
            &paged,
            &ctx.url("/admin/billing/gateway-log"),
            &[("tab", "attempts")],
        );
        (Vec::new(), paged.items, pager)
    } else {
        let paged =
            arepo::list_webhooks_in(&state.pool, tenant.id, integration_id, q.db_page()).await?;
        let pager = Pager::new(
            &paged,
            &ctx.url("/admin/billing/gateway-log"),
            &[("tab", "webhooks"), ("integration", integration.as_str())],
        );
        let hooks = paged
            .items
            .into_iter()
            .map(|row| HookView {
                integration: name_of(row.integration_id),
                payload: serde_json::to_string_pretty(&row.payload).unwrap_or_default(),
                row,
            })
            .collect();
        (hooks, Vec::new(), pager)
    };
    let integrations = all
        .into_iter()
        .filter(|i| {
            !matches!(
                i.category,
                IntegrationCategory::Mail | IntegrationCategory::Ai | IntegrationCategory::Oauth
            )
        })
        .collect();
    let title = ctx.t("admin.billing.gateway_log");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&GatewayLogPage {
        ctx,
        tab: tab.into(),
        integration,
        integrations,
        hooks,
        attempts,
        pager,
    })
}
