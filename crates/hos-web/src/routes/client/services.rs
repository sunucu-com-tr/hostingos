//! Client services: list, detail (status, billing, usage, credentials,
//! control panel SSO, addons, history), plan change (prorated quote → order)
//! and cancellation requests. Server (VPS / cloud) management lives in
//! [`super::vm`].

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::NaiveDate;
use hos_core::domain::{IntegrationCategory, ProductType, ServiceStatus};
use hos_core::ids::{ProductAddonId, ProductId, ServiceId};
use hos_core::integrations::panel::{AccountUsage, SsoService, SsoTarget};
use hos_core::money::BillingCycle;
use hos_db::enums::{CancellationType, PayType};
use hos_db::repo::catalog::{self, ProductRow};
use hos_db::repo::services::{ServiceEventRow, ServiceFilter, ServiceListRow, ServiceRow};
use hos_db::repo::{integrations, services as service_repo};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::{
    Fmt as _, MemberAccess, audit_client, friendly_error, no_store, outcome, parse_id, setup,
};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, PageQuery, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, Pager, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/services", get(list))
        .route("/client/services/{id}", get(show))
        .route("/client/services/{id}/password", get(password))
        .route("/client/services/{id}/sso", post(sso))
        .route(
            "/client/services/{id}/upgrade",
            get(upgrade_form).post(upgrade),
        )
        .route("/client/services/{id}/cancel", post(cancel))
        .route("/client/services/{id}/addons", post(order_addon))
}

/// The client's service or `404`.
pub async fn own_service(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    id: &str,
) -> AppResult<ServiceRow> {
    let id: ServiceId = parse_id(id)?;
    service_repo::get_service(&state.pool, tenant.id, id)
        .await?
        .filter(|s| s.client_id == client.client_id)
        .ok_or(AppError::NotFound)
}

/// Whether a product is a server (VM management applies).
pub fn is_server(t: ProductType) -> bool {
    matches!(
        t,
        ProductType::Vps | ProductType::Cloud | ProductType::Dedicated
    )
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    #[serde(default)]
    status: String,
}

#[derive(Template)]
#[template(path = "client/services.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ServiceListRow>,
    pager: Pager,
    status: String,
    q: String,
    statuses: Vec<(String, String)>,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    pq: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<ServiceStatus> = f.status.parse().ok();
    let q = pq.q.clone().unwrap_or_default().trim().to_owned();
    let paged = service_repo::list_services(
        &state.pool,
        tenant.id,
        &ServiceFilter {
            client_id: Some(client.client_id),
            status,
            product_id: None,
            product_type: None,
            search: (!q.is_empty()).then(|| q.clone()),
        },
        pq.db_page(),
    )
    .await?;
    let status_s = status.map(|s| s.as_str().to_owned()).unwrap_or_default();
    let pager = Pager::new(
        &paged,
        &ctx.url("/client/services"),
        &[("status", status_s.as_str()), ("q", q.as_str())],
    );
    let statuses = std::iter::once((String::new(), ctx.t("client.services.all")))
        .chain(
            [
                ServiceStatus::Active,
                ServiceStatus::Pending,
                ServiceStatus::Suspended,
                ServiceStatus::Cancelled,
                ServiceStatus::Terminated,
            ]
            .iter()
            .map(|s| {
                (
                    s.as_str().to_owned(),
                    ctx.t(&format!("common.status.{}", s.as_str())),
                )
            }),
        )
        .collect();
    let title = ctx.t("client.services.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    render(&ListPage {
        ctx,
        rows: paged.items,
        pager,
        status: status_s,
        q,
        statuses,
    })
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

/// A usage gauge.
pub struct Gauge {
    /// Label.
    pub label: String,
    /// "1,2 GB / 10 GB".
    pub text: String,
    /// 0–100 (none: no limit).
    pub percent: Option<u8>,
}

/// An addon of the service.
pub struct AddonRowView {
    /// Name.
    pub name: String,
    /// Status.
    pub status: String,
    /// Price text.
    pub price: String,
    /// Next due.
    pub next_due: Option<NaiveDate>,
}

/// An addon the client can still order for the service.
pub struct AddonOffer {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Price text (per period / one time / free, setup fee).
    pub price: String,
}

#[derive(Template)]
#[template(path = "client/service.html")]
struct DetailPage {
    ctx: PageCtx,
    s: ServiceRow,
    product: ProductRow,
    server: bool,
    has_panel: bool,
    can_upgrade: bool,
    usage: Vec<Gauge>,
    usage_at: String,
    addons: Vec<AddonRowView>,
    offers: Vec<AddonOffer>,
    events: Vec<ServiceEventRow>,
    ips: Vec<String>,
}
crate::page!(DetailPage);

impl DetailPage {
    fn active(&self) -> bool {
        self.s.status == ServiceStatus::Active
    }
    fn cancel_requested(&self) -> bool {
        self.s.cancellation_requested_at.is_some()
    }
    fn can_cancel(&self) -> bool {
        matches!(
            self.s.status,
            ServiceStatus::Active | ServiceStatus::Suspended | ServiceStatus::Pending
        ) && !self.cancel_requested()
    }
    fn recurring(&self) -> bool {
        self.s.billing_cycle.is_recurring()
    }
    /// History line of an event: a localized label for the known kinds
    /// (`client.services.events.*`), the generic failure text for errors
    /// (provider messages are not for customers), else the stored message.
    fn event_text(&self, e: &ServiceEventRow) -> String {
        if e.level.as_str() == "error" {
            return self.t("client.services.event_failed");
        }
        let key = format!("client.services.events.{}", e.event);
        if self.cat.has(&key) {
            self.t(&key)
        } else {
            e.message.clone()
        }
    }
}

fn mb(ctx: &PageCtx, v: u64) -> String {
    if v >= 1024 {
        let gb = rust_decimal::Decimal::from(v * 10 / 1024) / rust_decimal::Decimal::from(10);
        format!("{} GB", crate::i18n::format_decimal(gb, 1, ctx.locale))
    } else {
        format!("{v} MB")
    }
}

fn gauges(ctx: &PageCtx, s: &ServiceRow) -> (Vec<Gauge>, String) {
    let Some(u) = s
        .module_data
        .get("usage")
        .and_then(|v| serde_json::from_value::<AccountUsage>(v.clone()).ok())
    else {
        return (Vec::new(), String::new());
    };
    let gauge =
        |label: String, used: Option<u64>, limit: Option<u64>, fmt: &dyn Fn(u64) -> String| {
            used.map(|used| Gauge {
                label,
                text: match limit {
                    Some(l) if l > 0 => format!("{} / {}", fmt(used), fmt(l)),
                    _ => fmt(used),
                },
                percent: limit
                    .filter(|l| *l > 0)
                    .map(|l| ((used.saturating_mul(100) / l).min(100)) as u8),
            })
        };
    let size = |v: u64| mb(ctx, v);
    let count = |v: u64| v.to_string();
    let list = [
        gauge(
            ctx.t("client.services.disk"),
            u.disk_used_mb,
            u.limits.disk_mb,
            &size,
        ),
        gauge(
            ctx.t("client.services.bandwidth"),
            u.bandwidth_used_mb,
            u.limits.bandwidth_mb,
            &size,
        ),
        gauge(
            ctx.t("client.services.mailboxes"),
            u.email_accounts,
            u.limits.email_accounts,
            &count,
        ),
        gauge(
            ctx.t("client.services.databases"),
            u.databases,
            u.limits.databases,
            &count,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    (list, ctx.datetime(&u.collected_at))
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let product = catalog::get_product(&state.pool, tenant.id, s.product_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let category = match s.integration_id {
        Some(i) => integrations::get_integration(&state.pool, tenant.id, i)
            .await?
            .map(|r| r.category),
        None => None,
    };
    let server = is_server(product.product_type)
        && service_repo::vm_for_service(&state.pool, tenant.id, s.id)
            .await?
            .is_some();
    let panel = category == Some(IntegrationCategory::Panel) && s.external_id.is_some();
    let mut addons = Vec::new();
    let current = service_repo::service_addons(&state.pool, tenant.id, s.id).await?;
    let offers = if s.status == ServiceStatus::Active && access.allows("orders") {
        addon_offers(&state, &tenant, &ctx, &s, &current).await?
    } else {
        Vec::new()
    };
    for a in current {
        let name = catalog::get_addon(&state.pool, tenant.id, a.addon_id)
            .await?
            .map(|x| x.name)
            .unwrap_or_default();
        addons.push(AddonRowView {
            name,
            status: a.status.as_str().to_owned(),
            price: if a.billing_cycle.is_recurring() {
                format!(
                    "{} {}",
                    ctx.money_in(&a.recurring_amount, &a.currency),
                    ctx.per(&a.billing_cycle)
                )
            } else {
                ctx.money_in(&a.setup_fee, &a.currency)
            },
            next_due: a.next_due_date,
        });
    }
    let events = service_repo::service_events(&state.pool, tenant.id, s.id, 15).await?;
    let (usage, usage_at) = gauges(&ctx, &s);
    let mut ips: Vec<String> = s.dedicated_ip.iter().map(ToString::to_string).collect();
    for ip in &s.assigned_ips {
        let ip = ip.to_string();
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    let title = product.name.clone();
    let crumbs = vec![
        Crumb::link(ctx.t("client.services.title"), ctx.url("/client/services")),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    let res = render(&DetailPage {
        can_upgrade: s.status == ServiceStatus::Active && !product.upgrade_product_ids.is_empty(),
        ctx,
        s,
        product,
        server,
        has_panel: panel,
        usage,
        usage_at,
        addons,
        offers,
        events,
        ips,
    })?;
    Ok(no_store(res))
}

/// Orderable addons of the service's product not already on it.
async fn addon_offers(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    s: &ServiceRow,
    current: &[service_repo::ServiceAddonRow],
) -> AppResult<Vec<AddonOffer>> {
    let mut out = Vec::new();
    for c in catalog::addons_for_product(&state.pool, tenant.id, s.product_id, s.currency).await? {
        let a = &c.addon;
        if current.iter().any(|x| {
            x.addon_id == a.id
                && matches!(
                    x.status,
                    ServiceStatus::Active
                        | ServiceStatus::Suspended
                        | ServiceStatus::Provisioning
                        | ServiceStatus::Pending
                )
        }) {
            continue;
        }
        let tag = match a.pay_type {
            PayType::Free => None,
            PayType::OneTime => c
                .prices
                .iter()
                .find(|p| p.billing_cycle == BillingCycle::OneTime),
            PayType::Recurring => c.prices.iter().find(|p| p.billing_cycle == s.billing_cycle),
        };
        let price = match (a.pay_type, tag) {
            (PayType::Free, _) => ctx.t("client.services.addon_free"),
            (_, None) => continue,
            (PayType::OneTime, Some(p)) => ctx.tv(
                "client.services.addon_once",
                "price",
                ctx.money_in(&p.price, &s.currency),
            ),
            (_, Some(p)) => format!(
                "{} {}",
                ctx.money_in(&p.price, &s.currency),
                ctx.per(&p.billing_cycle)
            ),
        };
        let price = match tag.filter(|p| p.setup_fee > Decimal::ZERO) {
            Some(p) => format!(
                "{price} · {}",
                ctx.tv(
                    "client.services.addon_setup",
                    "fee",
                    ctx.money_in(&p.setup_fee, &s.currency)
                )
            ),
            None => price,
        };
        out.push(AddonOffer {
            id: a.id.to_string(),
            name: a.name.clone(),
            description: a.description.clone().filter(|d| !d.trim().is_empty()),
            price,
        });
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct AddonForm {
    #[serde(default)]
    addon: String,
}

/// Orders an addon for the service (order → invoice → activation on payment).
#[allow(clippy::too_many_arguments)]
async fn order_addon(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<AddonForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    if !access.allows("orders") {
        return Err(AppError::Forbidden(
            "addon order needs the orders area".into(),
        ));
    }
    let back = ctx.url(format!("/client/services/{}", s.id));
    let addon: ProductAddonId = parse_id(&f.addon)?;
    match state
        .billing
        .order_service_addon(
            tenant.id,
            s.id,
            Some(client.client_id),
            addon,
            Some(client.user_id),
            Some(ip.0),
        )
        .await
    {
        Ok(order) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.service.addon_order",
                Some(("service", s.id.into_uuid())),
            )
            .await;
            Ok(if order.paid {
                flash::redirect(
                    &back,
                    SetFlash::success(ctx.t("client.services.addon_active")),
                )
            } else {
                flash::redirect(
                    &ctx.url(format!("/client/invoices/{}#pay", order.invoice_id)),
                    SetFlash::success(ctx.t("client.services.addon_ordered")),
                )
            })
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

#[derive(Template)]
#[template(path = "client/_password.html")]
struct PasswordFragment {
    ctx: PageCtx,
    id: String,
    password: Option<String>,
    error: String,
}
crate::page!(PasswordFragment);

#[derive(Debug, Default, Deserialize)]
struct RevealQuery {
    #[serde(default)]
    hide: Option<String>,
}

/// `GET /client/services/{id}/password` (htmx): reveals / hides the
/// service password.
#[allow(clippy::too_many_arguments)]
async fn password(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    Query(q): Query<RevealQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let (password, error) = if q.hide.is_some() {
        (None, String::new())
    } else {
        match state
            .billing
            .service_password(tenant.id, s.id, Some(client.client_id))
            .await
        {
            Ok(Some(p)) => {
                audit_client(
                    &state,
                    &tenant,
                    &client,
                    ip,
                    &ua,
                    "client.service.password_viewed",
                    Some(("service", s.id.into_uuid())),
                )
                .await;
                (Some(p.expose_str().to_owned()), String::new())
            }
            Ok(None) => (None, ctx.t("client.services.no_password")),
            Err(e) => (
                None,
                friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?,
            ),
        }
    };
    let res = render(&PasswordFragment {
        ctx,
        id: s.id.to_string(),
        password,
        error,
    })?;
    Ok(no_store(res))
}

#[derive(Debug, Deserialize)]
struct SsoForm {
    #[serde(default)]
    target: String,
}

/// `POST /client/services/{id}/sso`: one-time login link into the panel.
#[allow(clippy::too_many_arguments)]
async fn sso(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<SsoForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/services/{}", s.id));
    let target = SsoTarget {
        service: if f.target == "webmail" {
            SsoService::Webmail
        } else {
            SsoService::ControlPanel
        },
        app: None,
        client_ip: Some(ip.0),
        locale: Some(ctx.locale),
    };
    match state
        .billing
        .panel_sso(tenant.id, s.id, Some(client.client_id), &target)
        .await
    {
        Ok(login) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.service.sso",
                Some(("service", s.id.into_uuid())),
            )
            .await;
            let url = login.url.expose_str().to_owned();
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(AppError::internal("panel returned an invalid SSO URL"));
            }
            Ok(no_store(Redirect::to(&url).into_response()))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------------------
// Plan change
// ---------------------------------------------------------------------------

/// An upgrade / downgrade option.
pub struct PlanOption {
    /// Product id.
    pub id: String,
    /// Name.
    pub name: String,
    /// New renewal price.
    pub recurring: String,
    /// Due now (prorated; may be a credit).
    pub due_now: String,
    /// Proration explanation.
    pub note: String,
    /// Credit instead of a charge.
    pub credit: bool,
    /// Could not be priced (reason).
    pub error: String,
}

#[derive(Template)]
#[template(path = "client/service_upgrade.html")]
struct UpgradePage {
    ctx: PageCtx,
    s: ServiceRow,
    product: ProductRow,
    options: Vec<PlanOption>,
}
crate::page!(UpgradePage);

async fn upgrade_form(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let product = catalog::get_product(&state.pool, tenant.id, s.product_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut options = Vec::new();
    for pid in &product.upgrade_product_ids {
        let Some(to) = catalog::get_product(&state.pool, tenant.id, *pid)
            .await?
            .filter(|p| !p.is_retired)
        else {
            continue;
        };
        let mut opt = PlanOption {
            id: to.id.to_string(),
            name: to.name.clone(),
            recurring: String::new(),
            due_now: String::new(),
            note: String::new(),
            credit: false,
            error: String::new(),
        };
        match state
            .billing
            .quote_plan_change(tenant.id, s.id, Some(client.client_id), to.id)
            .await
        {
            Ok(q) => {
                opt.recurring = format!(
                    "{} {}",
                    ctx.money_in(&q.new_recurring_amount, &s.currency),
                    ctx.per(&q.billing_cycle)
                );
                opt.credit = q.proration.amount.is_sign_negative();
                opt.due_now = ctx.money_in(&q.proration.amount.abs(), &s.currency);
                opt.note = ctx.tv2(
                    "client.upgrade.proration",
                    "days",
                    q.proration.remaining_days,
                    "period",
                    q.proration.period_days,
                );
            }
            Err(e) => opt.error = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?,
        }
        options.push(opt);
    }
    let title = ctx.t("client.upgrade.title");
    let crumbs = vec![
        Crumb::link(ctx.t("client.services.title"), ctx.url("/client/services")),
        Crumb::link(
            product.name.clone(),
            ctx.url(format!("/client/services/{}", s.id)),
        ),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    render(&UpgradePage {
        ctx,
        s,
        product,
        options,
    })
}

#[derive(Debug, Deserialize)]
struct UpgradeForm {
    product: String,
    confirm: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn upgrade(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<UpgradeForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/services/{}/upgrade", s.id));
    if !forms::checked(f.confirm.as_deref()) {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.common.confirm_required")),
        ));
    }
    let to: ProductId = parse_id(&f.product)?;
    match state
        .billing
        .order_plan_change(tenant.id, s.id, Some(client.client_id), to)
        .await
    {
        Ok(order) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.service.plan_change",
                Some(("service", s.id.into_uuid())),
            )
            .await;
            Ok(match order.invoice_id {
                Some(inv) => flash::redirect(
                    &ctx.url(format!("/client/invoices/{inv}")),
                    SetFlash::success(ctx.t("client.upgrade.ordered_invoice")),
                ),
                None => flash::redirect(
                    &ctx.url(format!("/client/services/{}", s.id)),
                    SetFlash::success(ctx.t("client.upgrade.ordered")),
                ),
            })
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CancelForm {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    reason: String,
    confirm: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn cancel(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<CancelForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/services/{}", s.id));
    if !forms::checked(f.confirm.as_deref()) {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.common.confirm_required")),
        ));
    }
    let kind = if f.kind == "immediate" {
        CancellationType::Immediate
    } else {
        CancellationType::EndOfPeriod
    };
    let reason = forms::clean(&f.reason, 1000);
    let res = state
        .billing
        .request_cancellation(
            tenant.id,
            s.id,
            Some(client.client_id),
            kind,
            (!reason.is_empty()).then_some(reason.as_str()),
        )
        .await;
    if res.is_ok() {
        audit_client(
            &state,
            &tenant,
            &client,
            ip,
            &ua,
            "client.service.cancellation_requested",
            Some(("service", s.id.into_uuid())),
        )
        .await;
    }
    outcome(&ctx, &back, res, |()| {
        ctx.t("client.services.cancel_requested")
    })
}
