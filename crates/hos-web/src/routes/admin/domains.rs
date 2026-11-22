//! `/admin/domains` — domain list, expiring view and detail with registrar
//! commands (register, transfer, renew, nameservers, lock, EPP code,
//! privacy, auto-renew, sync). Registrar calls go through `hos-billing`
//! (direct calls or queued jobs with retries).

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_billing::jobs::{self, JobKind};
use hos_core::domain::DomainStatus;
use hos_core::ids::DomainId;
use hos_db::repo::clients::{self, ClientRow};
use hos_db::repo::domains::{self, DomainFilter, DomainRow};
use hos_db::repo::integrations::{self, IntegrationRow};
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
        .route("/admin/domains", get(list))
        .route("/admin/domains/expiring", get(expiring))
        .route("/admin/domains/{id}", get(show))
        .route("/admin/domains/{id}/command", post(command))
        .route("/admin/domains/{id}/nameservers", post(nameservers))
        .route("/admin/domains/{id}/epp", post(epp))
}

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    status: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/domains/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<DomainRow>,
    pager: Pager,
    q: String,
    status: String,
    statuses: Vec<(String, String)>,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::DomainsManage>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<DomainStatus> = f.status.as_deref().and_then(|s| s.parse().ok());
    let paged = domains::list_domains(
        &state.pool,
        tenant.id,
        &DomainFilter {
            client_id: None,
            status,
            search: q.q.clone(),
        },
        q.db_page(),
    )
    .await?;
    let title = ctx.t("admin.domains.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let (qv, sv) = (
        q.q.clone().unwrap_or_default(),
        f.status.clone().unwrap_or_default(),
    );
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/domains"),
        &[("q", qv.as_str()), ("status", sv.as_str())],
    );
    let statuses = common::enum_options(&ctx, DomainStatus::ALL, |s| s.as_str(), "admin.status");
    render(&ListPage {
        rows: paged.items,
        pager,
        q: qv,
        status: sv,
        statuses,
        ctx,
    })
}

#[derive(Debug, Default, Deserialize)]
struct ExpiringQuery {
    days: Option<i32>,
}

#[derive(Template)]
#[template(path = "admin/domains/expiring.html")]
struct ExpiringPage {
    ctx: PageCtx,
    rows: Vec<DomainRow>,
    days: i32,
}
crate::page!(ExpiringPage);

async fn expiring(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::DomainsManage>,
    Query(eq): Query<ExpiringQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let days = eq.days.unwrap_or(60).clamp(1, 365);
    let rows = domains::domains_expiring(&state.pool, tenant.id, days, false).await?;
    let title = ctx.t("admin.domains.expiring");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.domains.title", "/admin/domains")],
    );
    render(&ExpiringPage { ctx, rows, days })
}

#[derive(Template)]
#[template(path = "admin/domains/show.html")]
struct ShowPage {
    ctx: PageCtx,
    d: DomainRow,
    client: ClientRow,
    registrar: Option<IntegrationRow>,
    nameservers: String,
    epp: String,
}
crate::page!(ShowPage);

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<DomainRow> {
    domains::get_domain(&state.pool, tenant.id, DomainId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

async fn show_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    d: DomainRow,
    epp: String,
) -> AppResult<Response> {
    let client = clients::get_client(&state.pool, tenant.id, d.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let registrar = match d.registrar_integration_id {
        Some(r) => integrations::get_integration(&state.pool, tenant.id, r).await?,
        None => None,
    };
    let title = d.name.clone();
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin.domains.title", "/admin/domains")],
    );
    render(&ShowPage {
        nameservers: d.nameservers.join("\n"),
        ctx,
        d,
        client,
        registrar,
        epp,
    })
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::DomainsManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let d = load(&state, &tenant, id).await?;
    show_page(&state, &tenant, &staff, ctx, d, String::new()).await
}

async fn command(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::DomainsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let d = load(&state, &tenant, id).await?;
    let back = format!("/admin/domains/{}", d.id);
    let action = f.text("action", 30);
    let years = f
        .int("years")
        .ok()
        .flatten()
        .and_then(|y| i16::try_from(y).ok())
        .filter(|y| (1..=10).contains(y));
    let (result, ok_key): (Result<(), hos_core::CoreError>, &str) = match action.as_str() {
        "register" | "transfer" | "renew_now" => {
            let kind = match action.as_str() {
                "register" => JobKind::RegisterDomain,
                "transfer" => JobKind::TransferDomain,
                _ => JobKind::RenewDomain,
            };
            let mut conn = state.pool.acquire().await?;
            let r = jobs::enqueue_domain_job(
                &mut conn,
                tenant.id,
                kind,
                d.id,
                if action == "renew_now" {
                    years.or(Some(1))
                } else {
                    None
                },
                Some(format!("admin:{action}:{}", d.id)),
            )
            .await
            .map(|_| ());
            (r, "admin.domains.queued")
        }
        "renew_invoice" => {
            match state
                .billing
                .order_domain_renewal(tenant.id, d.id, None, years.unwrap_or(1))
                .await
            {
                Ok(inv) => {
                    au.log(
                        &state,
                        "domain.renew_invoice",
                        Some(("domain", d.id.into())),
                        json!({"invoice": inv, "years": years.unwrap_or(1)}),
                    )
                    .await?;
                    return Ok(done(
                        &ctx,
                        &format!("/admin/invoices/{inv}"),
                        "admin.domains.renewal_invoiced",
                    ));
                }
                Err(e) => (Err(e), ""),
            }
        }
        "lock" | "unlock" => (
            state
                .billing
                .domain_set_lock(tenant.id, d.id, None, action == "lock")
                .await,
            "admin.common.saved",
        ),
        "privacy_on" | "privacy_off" => (
            state
                .billing
                .domain_set_privacy(tenant.id, d.id, None, action == "privacy_on")
                .await,
            "admin.common.saved",
        ),
        "auto_renew_on" | "auto_renew_off" => (
            state
                .billing
                .domain_set_auto_renew(tenant.id, d.id, None, action == "auto_renew_on")
                .await,
            "admin.common.saved",
        ),
        "sync" => (
            state.billing.sync_domain(tenant.id, d.id).await.map(|_| ()),
            "admin.domains.synced",
        ),
        _ => return Err(AppError::bad_request("action")),
    };
    if result.is_ok() {
        au.log(
            &state,
            &format!("domain.{action}"),
            Some(("domain", d.id.into())),
            json!({"name": d.name, "years": years}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, ok_key)
}

async fn nameservers(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::DomainsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let d = load(&state, &tenant, id).await?;
    let back = format!("/admin/domains/{}", d.id);
    let ns: Vec<String> = f
        .body("nameservers", 2000)
        .split(|c: char| c == '\n' || c == ',' || c.is_whitespace())
        .map(|s| s.trim().trim_end_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let valid = (2..=13).contains(&ns.len())
        && ns.iter().all(|n| {
            n.contains('.')
                && n.len() <= 253
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        });
    if !valid {
        return Ok(failed(&ctx, &back, ctx.t("admin.domains.ns_invalid")));
    }
    let result = state
        .billing
        .domain_set_nameservers(tenant.id, d.id, None, &ns)
        .await;
    if result.is_ok() {
        au.log(
            &state,
            "domain.nameservers",
            Some(("domain", d.id.into())),
            json!({"from": d.nameservers, "to": ns}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.domains.ns_saved")
}

/// Shows the transfer (EPP) code once, on the detail page; audited.
async fn epp(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::DomainsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let d = load(&state, &tenant, id).await?;
    match state.billing.domain_epp_code(tenant.id, d.id, None).await {
        Ok(code) => {
            au.log(
                &state,
                "domain.epp_view",
                Some(("domain", d.id.into())),
                json!({"name": d.name}),
            )
            .await?;
            let code = code.expose_str().to_owned();
            show_page(&state, &tenant, &staff, ctx, d, code).await
        }
        Err(e) => Ok(failed(
            &ctx,
            &format!("/admin/domains/{}", d.id),
            common::failure_text(&ctx, &e)?,
        )),
    }
}
