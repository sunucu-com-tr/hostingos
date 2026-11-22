//! Client affiliate dashboard (`/client/affiliate`, account owner only):
//! activation, referral link, figures, commissions, referrals (masked) and
//! payout requests. Amounts are computed by hos-billing / hos-db, never
//! taken from the form.

use askama::Template;
use axum::Router;
use axum::extract::{Form, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_billing::affiliates::AffiliateSettings;
use hos_db::repo::affiliates::{
    self as arepo, AffiliateListRow, CommissionRow, PayoutMethod, PayoutRow, ReferralView,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::{MemberAccess, audit_client, friendly_error, no_store, setup};
use crate::error::AppResult;
use crate::extract::{ClientIp, CurrentClient, UserAgent};
use crate::flash::{self, SetFlash};
use crate::routes::admin::common::Ui as _;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/affiliate", get(dashboard))
        .route("/client/affiliate/activate", post(activate))
        .route("/client/affiliate/payout", post(payout))
}

/// Badge tone of a commission / payout status.
pub fn status_tone(s: &str) -> &'static str {
    match s {
        "available" | "paid" => "success",
        "pending" | "requested" | "withdrawn" => "warning",
        "held" => "info",
        "reversed" | "rejected" => "danger",
        _ => "",
    }
}

#[derive(Template)]
#[template(path = "client/affiliate.html")]
struct Page {
    ctx: PageCtx,
    settings: AffiliateSettings,
    stats: Option<AffiliateListRow>,
    link: String,
    commissions: Vec<CommissionRow>,
    referrals: Vec<ReferralView>,
    payouts: Vec<PayoutRow>,
    open_payout: bool,
    can_request: bool,
    rate_label: String,
}
crate::page!(Page);

impl Page {
    fn sbadge2(&self, prefix: &str, s: &str) -> askama::filters::Safe<String> {
        self.badge(self.lbl(prefix, s), status_tone(s))
    }
}

async fn dashboard(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let settings = AffiliateSettings::load(&state.pool, tenant.id).await?;
    let affiliate = arepo::affiliate_for_client(&state.pool, tenant.id, client.client_id).await?;
    let title = ctx.t("affiliates.client.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    let (stats, link, commissions, referrals, payouts) = match &affiliate {
        Some(a) => (
            arepo::affiliate_stats(&state.pool, tenant.id, a.id).await?,
            ctx.abs(&format!("/r/{}", a.code)),
            arepo::commissions_of(&state.pool, tenant.id, a.id, 50).await?,
            arepo::referrals_of(&state.pool, tenant.id, a.id, 50).await?,
            arepo::payouts_of(&state.pool, tenant.id, a.id, 20).await?,
        ),
        None => (None, String::new(), Vec::new(), Vec::new(), Vec::new()),
    };
    let open_payout = payouts.iter().any(|p| p.status.as_str() == "requested");
    let can_request = stats.as_ref().is_some_and(|s| {
        s.balance > Decimal::ZERO
            && s.balance >= settings.min_payout
            && s.affiliate.status.as_str() == "active"
    }) && !open_payout
        && (settings.payout_credit || settings.payout_manual);
    let rate = match affiliate
        .as_ref()
        .and_then(|a| a.commission_type.clone().zip(a.commission_value))
    {
        Some((k, v)) => (k, v),
        None => (settings.commission_type.clone(), settings.commission_value),
    };
    let rate_label = if rate.0 == "percent" {
        ctx.tv(
            "affiliates.client.rate_percent",
            "value",
            rate.1.normalize(),
        )
    } else {
        ctx.tv(
            "affiliates.client.rate_fixed",
            "value",
            ctx.money_in(
                &rate.1,
                &affiliate.as_ref().map_or(ctx.currency, |a| a.currency),
            ),
        )
    };
    Ok(no_store(render(&Page {
        settings,
        stats,
        link,
        commissions,
        referrals,
        payouts,
        open_payout,
        can_request,
        rate_label,
        ctx,
    })?))
}

async fn activate(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
) -> AppResult<Response> {
    let back = ctx.url("/client/affiliate");
    let settings = AffiliateSettings::load(&state.pool, tenant.id).await?;
    if !settings.enabled || !settings.self_signup {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("affiliates.client.closed")),
        ));
    }
    match state
        .billing
        .activate_affiliate(tenant.id, client.client_id)
        .await
    {
        Ok(a) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "affiliate.activate",
                Some(("affiliate", a.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(
                &back,
                SetFlash::success(ctx.t("affiliates.client.activated")),
            ))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(msg) => Ok(flash::redirect(&back, SetFlash::error(msg))),
            None => Err(e.into()),
        },
    }
}

#[derive(Debug, Deserialize)]
struct PayoutForm {
    #[serde(default)]
    method: String,
    #[serde(default)]
    details: String,
}

async fn payout(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<PayoutForm>,
) -> AppResult<Response> {
    let back = ctx.url("/client/affiliate");
    let Some(a) = arepo::affiliate_for_client(&state.pool, tenant.id, client.client_id).await?
    else {
        return Err(crate::error::AppError::NotFound);
    };
    let Ok(method) = f.method.parse::<PayoutMethod>() else {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("affiliates.client.method_invalid")),
        ));
    };
    let details: String = f.details.chars().take(500).collect();
    match state
        .billing
        .request_affiliate_payout(tenant.id, a.id, method, Some(&details))
        .await
    {
        Ok(p) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "affiliate.payout_request",
                Some(("affiliate_payout", p.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(
                &back,
                SetFlash::success(ctx.t("affiliates.client.payout_requested")),
            ))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(msg) => Ok(flash::redirect(&back, SetFlash::error(msg))),
            None => Err(e.into()),
        },
    }
}
