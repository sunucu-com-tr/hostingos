//! `/admin/affiliates` — the affiliate programme: affiliates with their
//! figures, programme settings, per-product commission rules, the payout
//! queue (approve to credit / mark paid, reject), the anti-fraud review
//! queue (release or reverse held commissions) and per-affiliate overrides.
//!
//! Reading needs `affiliates.view`, every change `affiliates.manage`; every
//! change is audited. Money moves only through hos-billing.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_auth::rbac::Permission;
use hos_billing::affiliates::AffiliateSettings;
use hos_core::ids::{ClientId, ProductId};
use hos_db::Page;
use hos_db::repo::affiliates::{
    self as arepo, AffiliateId, AffiliateListRow, AffiliateStatus, CommissionId, CommissionRow,
    FlaggedReferral, PayoutId, PayoutListRow, PayoutRow, ProductRateRow, ReferralId, ReferralView,
};
use hos_db::repo::{catalog, platform};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, PageQuery, PermissionMarker, RequirePermission};
use crate::routes::client::affiliate::status_tone;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, Pager, render};

/// `affiliates.view`.
#[derive(Debug, Clone, Copy)]
pub struct AffiliatesView;
impl PermissionMarker for AffiliatesView {
    const PERMISSION: Permission = Permission::AFFILIATES_VIEW;
}
/// `affiliates.manage`.
#[derive(Debug, Clone, Copy)]
pub struct AffiliatesManage;
impl PermissionMarker for AffiliatesManage {
    const PERMISSION: Permission = Permission::AFFILIATES_MANAGE;
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/affiliates", get(list))
        .route("/admin/affiliates/activate", post(activate))
        .route(
            "/admin/affiliates/settings",
            get(settings_form).post(settings_save),
        )
        .route("/admin/affiliates/rates", get(rates).post(rate_save))
        .route(
            "/admin/affiliates/rates/{product}/delete",
            post(rate_delete),
        )
        .route("/admin/affiliates/payouts", get(payouts))
        .route(
            "/admin/affiliates/payouts/{id}/approve",
            post(payout_approve),
        )
        .route("/admin/affiliates/payouts/{id}/reject", post(payout_reject))
        .route("/admin/affiliates/review", get(review))
        .route("/admin/affiliates/referrals/{id}/release", post(release))
        .route("/admin/affiliates/commissions/{id}/reverse", post(reverse))
        .route("/admin/affiliates/{id}", get(show).post(update))
}

fn tabs(ctx: &PageCtx) -> Vec<NavItem> {
    common::tabs(
        ctx,
        &[
            (
                "affiliates.admin.tab_affiliates",
                "/admin/affiliates".into(),
            ),
            (
                "affiliates.admin.tab_payouts",
                "/admin/affiliates/payouts".into(),
            ),
            (
                "affiliates.admin.tab_review",
                "/admin/affiliates/review".into(),
            ),
            (
                "affiliates.admin.tab_rates",
                "/admin/affiliates/rates".into(),
            ),
            (
                "affiliates.admin.tab_settings",
                "/admin/affiliates/settings".into(),
            ),
        ],
    )
}

fn head(ctx: &mut PageCtx, staff: &CurrentStaff, title_key: &str) {
    let title = ctx.t(title_key);
    common::page(
        ctx,
        staff,
        &title,
        &[("affiliates.menu", "/admin/affiliates")],
    );
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/affiliates/list.html")]
struct ListPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    rows: Vec<AffiliateListRow>,
    pager: Pager,
    q: String,
    totals: (i64, i64),
    money: Vec<(hos_core::money::Currency, Decimal, Decimal, Decimal)>,
    enabled: bool,
}
crate::page!(ListPage);

impl ListPage {
    fn abadge(&self, s: &AffiliateStatus) -> askama::filters::Safe<String> {
        self.badge(
            self.lbl("affiliates.affiliate_status", s.as_str()),
            if *s == AffiliateStatus::Active {
                "success"
            } else {
                "danger"
            },
        )
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesView>,
    q: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let paged = arepo::list_affiliates(&state.pool, tenant.id, q.q.as_deref(), q.db_page()).await?;
    let title = ctx.t("affiliates.admin.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let qv = q.q.clone().unwrap_or_default();
    let pager = Pager::new(&paged, &ctx.url("/admin/affiliates"), &[("q", qv.as_str())]);
    let totals = arepo::programme_totals(&state.pool, tenant.id).await?;
    let money = arepo::programme_money(&state.pool, tenant.id).await?;
    let enabled = AffiliateSettings::load(&state.pool, tenant.id)
        .await?
        .enabled;
    render(&ListPage {
        tabs: tabs(&ctx),
        rows: paged.items,
        pager,
        q: qv,
        totals,
        money,
        enabled,
        ctx,
    })
}

async fn activate(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let Some(client_id) = f.id::<ClientId>("client_id") else {
        return Err(AppError::BadRequest("client_id".into()));
    };
    let back = format!("/admin/clients/{client_id}");
    match state.billing.activate_affiliate(tenant.id, client_id).await {
        Ok(a) => {
            au.log(
                &state,
                "affiliate.activate",
                Some(("affiliate", a.id.into_uuid())),
                json!({"client": client_id}),
            )
            .await?;
            Ok(done(
                &ctx,
                &format!("/admin/affiliates/{}", a.id),
                "affiliates.admin.activated",
            ))
        }
        Err(e) => Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/affiliates/settings.html")]
struct SettingsPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    s: AffiliateSettings,
    error: String,
}
crate::page!(SettingsPage);

async fn settings_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    head(&mut ctx, &staff, "affiliates.admin.settings_title");
    let s = AffiliateSettings::load(&state.pool, tenant.id).await?;
    render(&SettingsPage {
        tabs: tabs(&ctx),
        s,
        error: String::new(),
        ctx,
    })
}

async fn settings_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    mut ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let int =
        |k: &str, min: i64, max: i64| f.int(k).ok().flatten().filter(|v| (min..=max).contains(v));
    let kind = f.text("commission_type", 10);
    let value = f.decimal("commission_value").ok().flatten();
    let min_payout = f.decimal("min_payout").ok().flatten();
    let s = AffiliateSettings {
        enabled: f.flag("enabled"),
        self_signup: f.flag("self_signup"),
        cookie_days: int("cookie_days", 1, 365).unwrap_or(-1),
        last_click: f.text("attribution", 5) == "last",
        commission_type: kind.clone(),
        commission_value: value.unwrap_or(Decimal::NEGATIVE_ONE),
        recurring: f.flag("recurring"),
        clearance_days: int("clearance_days", 0, 365).unwrap_or(-1),
        min_payout: min_payout.unwrap_or(Decimal::NEGATIVE_ONE),
        payout_credit: f.flag("payout_credit"),
        payout_manual: f.flag("payout_manual"),
    };
    let valid = matches!(kind.as_str(), "percent" | "fixed")
        && s.cookie_days > 0
        && s.clearance_days >= 0
        && s.commission_value >= Decimal::ZERO
        && (kind != "percent" || s.commission_value <= Decimal::ONE_HUNDRED)
        && s.min_payout >= Decimal::ZERO;
    if !valid {
        head(&mut ctx, &staff, "affiliates.admin.settings_title");
        let page = SettingsPage {
            tabs: tabs(&ctx),
            s,
            error: ctx.t("affiliates.admin.settings_invalid"),
            ctx,
        };
        return crate::view::render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    for (k, v) in s.to_pairs() {
        platform::set_setting(&state.pool, tenant.id, k, &v).await?;
    }
    au.log(
        &state,
        "settings.affiliates",
        None,
        json!(
            s.to_pairs()
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        ),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/affiliates/settings",
        "affiliates.admin.settings_saved",
    ))
}

// ---------------------------------------------------------------------------
// product rates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/affiliates/rates.html")]
struct RatesPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    rows: Vec<ProductRateRow>,
    products: Vec<(String, String)>,
}
crate::page!(RatesPage);

async fn rates(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    head(&mut ctx, &staff, "affiliates.admin.rates_title");
    let rows = arepo::product_rates(&state.pool, tenant.id).await?;
    let products = catalog::list_products(
        &state.pool,
        tenant.id,
        &catalog::ProductFilter::default(),
        Page::first(500),
    )
    .await?
    .items
    .into_iter()
    .map(|p| (p.id.to_string(), p.name))
    .collect();
    render(&RatesPage {
        tabs: tabs(&ctx),
        rows,
        products,
        ctx,
    })
}

async fn rate_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/affiliates/rates";
    let Some(product_id) = f.id::<ProductId>("product_id") else {
        return Ok(failed(&ctx, back, ctx.t("affiliates.admin.rate_invalid")));
    };
    if catalog::get_product(&state.pool, tenant.id, product_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    let kind = f.text("commission_type", 10);
    let value = f
        .decimal("commission_value")
        .ok()
        .flatten()
        .unwrap_or(Decimal::ZERO);
    let ok = match kind.as_str() {
        "percent" => value >= Decimal::ZERO && value <= Decimal::ONE_HUNDRED,
        "fixed" => value >= Decimal::ZERO,
        "none" => true,
        _ => false,
    };
    if !ok {
        return Ok(failed(&ctx, back, ctx.t("affiliates.admin.rate_invalid")));
    }
    arepo::set_product_rate(
        &state.pool,
        tenant.id,
        product_id,
        &kind,
        value,
        f.flag("recurring"),
    )
    .await?;
    au.log(
        &state,
        "affiliate.rate_set",
        Some(("product", product_id.into_uuid())),
        json!({"type": kind, "value": value}),
    )
    .await?;
    Ok(done(&ctx, back, "affiliates.admin.rate_saved"))
}

async fn rate_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    Path(product): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if arepo::delete_product_rate(&state.pool, tenant.id, ProductId::from(product)).await? {
        au.log(
            &state,
            "affiliate.rate_delete",
            Some(("product", product)),
            json!({}),
        )
        .await?;
    }
    Ok(done(
        &ctx,
        "/admin/affiliates/rates",
        "affiliates.admin.rate_deleted",
    ))
}

// ---------------------------------------------------------------------------
// payouts
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/affiliates/payouts.html")]
struct PayoutsPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    rows: Vec<PayoutListRow>,
}
crate::page!(PayoutsPage);

impl PayoutsPage {
    fn pbadge(&self, s: &str) -> askama::filters::Safe<String> {
        self.badge(self.lbl("affiliates.payout_status", s), status_tone(s))
    }
}

async fn payouts(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    head(&mut ctx, &staff, "affiliates.admin.payouts_title");
    let rows = arepo::list_payouts(&state.pool, tenant.id, None, 200).await?;
    render(&PayoutsPage {
        tabs: tabs(&ctx),
        rows,
        ctx,
    })
}

async fn payout_approve(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/affiliates/payouts";
    let id = PayoutId::from(id);
    arepo::get_payout(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let note = f.opt("note", 500);
    match state
        .billing
        .approve_affiliate_payout(tenant.id, id, Some(staff.user_id), note.as_deref())
        .await
    {
        Ok(p) => {
            au.log(
                &state,
                "affiliate.payout_paid",
                Some(("affiliate_payout", p.id.into_uuid())),
                json!({"amount": p.amount, "currency": p.currency, "method": p.method}),
            )
            .await?;
            Ok(done(&ctx, back, "affiliates.admin.payout_paid"))
        }
        Err(e) => Ok(failed(&ctx, back, common::failure_text(&ctx, &e)?)),
    }
}

async fn payout_reject(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/affiliates/payouts";
    let id = PayoutId::from(id);
    arepo::get_payout(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let note = f.opt("note", 500);
    match state
        .billing
        .reject_affiliate_payout(tenant.id, id, Some(staff.user_id), note.as_deref())
        .await
    {
        Ok(p) => {
            au.log(
                &state,
                "affiliate.payout_rejected",
                Some(("affiliate_payout", p.id.into_uuid())),
                json!({}),
            )
            .await?;
            Ok(done(&ctx, back, "affiliates.admin.payout_rejected"))
        }
        Err(e) => Ok(failed(&ctx, back, common::failure_text(&ctx, &e)?)),
    }
}

// ---------------------------------------------------------------------------
// review queue
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/affiliates/review.html")]
struct ReviewPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    rows: Vec<FlaggedReferral>,
}
crate::page!(ReviewPage);

async fn review(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    head(&mut ctx, &staff, "affiliates.admin.review_title");
    let rows = arepo::flagged_referrals(&state.pool, tenant.id, 200).await?;
    render(&ReviewPage {
        tabs: tabs(&ctx),
        rows,
        ctx,
    })
}

async fn release(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = ReferralId::from(id);
    let exists: bool = sqlx::query_scalar(
        "select exists(select 1 from affiliate_referrals where tenant_id = $1 and id = $2)",
    )
    .bind(tenant.id)
    .bind(id)
    .fetch_one(&state.pool)
    .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    let approve = f.text("decision", 10) == "approve";
    let mut conn = state.pool.acquire().await?;
    let n = arepo::release_held(&mut conn, tenant.id, id, approve, chrono::Utc::now()).await?;
    au.log(
        &state,
        if approve {
            "affiliate.referral_released"
        } else {
            "affiliate.referral_rejected"
        },
        Some(("affiliate_referral", id.into_uuid())),
        json!({"commissions": n}),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/affiliates/review",
        if approve {
            "affiliates.admin.released"
        } else {
            "affiliates.admin.reversed"
        },
    ))
}

async fn reverse(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = CommissionId::from(id);
    let affiliate: Option<AffiliateId> = sqlx::query_scalar(
        "select affiliate_id from affiliate_commissions where tenant_id = $1 and id = $2",
    )
    .bind(tenant.id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(affiliate) = affiliate else {
        return Err(AppError::NotFound);
    };
    let mut conn = state.pool.acquire().await?;
    let changed =
        arepo::reverse_commission(&mut conn, tenant.id, id, "staff", chrono::Utc::now()).await?;
    if changed {
        au.log(
            &state,
            "affiliate.commission_reversed",
            Some(("affiliate_commission", id.into_uuid())),
            json!({}),
        )
        .await?;
    }
    Ok(done(
        &ctx,
        &format!("/admin/affiliates/{affiliate}"),
        "affiliates.admin.reversed",
    ))
}

// ---------------------------------------------------------------------------
// one affiliate
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/affiliates/show.html")]
struct ShowPage {
    ctx: PageCtx,
    a: AffiliateListRow,
    commissions: Vec<CommissionRow>,
    referrals: Vec<ReferralView>,
    payouts: Vec<PayoutRow>,
    link: String,
}
crate::page!(ShowPage);

impl ShowPage {
    fn cbadge(&self, prefix: &str, s: &str) -> askama::filters::Safe<String> {
        self.badge(self.lbl(prefix, s), status_tone(s))
    }

    fn override_type(&self) -> String {
        self.a.affiliate.commission_type.clone().unwrap_or_default()
    }

    fn override_value(&self) -> String {
        self.a
            .affiliate
            .commission_value
            .map(|v| v.normalize().to_string())
            .unwrap_or_default()
    }

    fn override_recurring(&self) -> &'static str {
        match self.a.affiliate.commission_recurring {
            Some(true) => "yes",
            Some(false) => "no",
            None => "",
        }
    }
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<AffiliatesView>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let id = AffiliateId::from(id);
    let a = arepo::affiliate_stats(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let commissions = arepo::commissions_of(&state.pool, tenant.id, id, 200).await?;
    let referrals = arepo::referrals_of(&state.pool, tenant.id, id, 200).await?;
    let payouts = arepo::payouts_of(&state.pool, tenant.id, id, 50).await?;
    let title = ctx.tv("affiliates.admin.show_title", "name", &a.client_name);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("affiliates.menu", "/admin/affiliates")],
    );
    let link = ctx.abs(&format!("/r/{}", a.affiliate.code));
    render(&ShowPage {
        a,
        commissions,
        referrals,
        payouts,
        link,
        ctx,
    })
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<AffiliatesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = AffiliateId::from(id);
    let back = format!("/admin/affiliates/{id}");
    arepo::get_affiliate(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let status = if f.text("status", 10) == "suspended" {
        AffiliateStatus::Suspended
    } else {
        AffiliateStatus::Active
    };
    let kind = f.text("commission_type", 10);
    let commission = match kind.as_str() {
        "" => None,
        "percent" | "fixed" => match f.decimal("commission_value") {
            Ok(Some(v))
                if v >= Decimal::ZERO && (kind != "percent" || v <= Decimal::ONE_HUNDRED) =>
            {
                Some((kind.clone(), v))
            }
            _ => return Ok(failed(&ctx, &back, ctx.t("affiliates.admin.rate_invalid"))),
        },
        _ => return Ok(failed(&ctx, &back, ctx.t("affiliates.admin.rate_invalid"))),
    };
    let recurring = match f.text("recurring", 10).as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    };
    let row = arepo::update_affiliate(
        &state.pool,
        tenant.id,
        id,
        status,
        commission.clone(),
        recurring,
    )
    .await?;
    au.log(
        &state,
        "affiliate.update",
        Some(("affiliate", row.id.into_uuid())),
        json!({"status": status, "commission": commission, "recurring": recurring}),
    )
    .await?;
    Ok(done(&ctx, &back, "affiliates.admin.saved"))
}
