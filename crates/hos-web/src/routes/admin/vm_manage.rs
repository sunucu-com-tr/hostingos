//! Staff VPS management next to the VM panel of [`super::services`]
//! (`/admin/services/{id}/vm/…`): embedded console over the HostingOS relay
//! (also for suspended services), usage graphs and bandwidth, backups,
//! reverse DNS, firewall, rescue, root password reset, the ISO allow-list of
//! the VM's integration, the operation log (with provider errors) and the
//! console session audit. `services.view` reads, `services.manage` acts;
//! every action is audited. Limits that bind clients (snapshot quota) do
//! not bind staff; allow-lists (ISO, templates) do.

use askama::Template;
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hos_billing::vm_ops::{ConsoleRequester, IpPtr, ProviderIso};
use hos_core::CoreError;
use hos_core::error::IntegrationError;
use hos_core::ids::ServiceId;
use hos_core::integrations::virt::{BackupInfo, FirewallState, VirtFeatures, VmExtras};
use hos_db::repo::services::{self, ServiceRow};
use hos_db::repo::vm_ops::ConsoleSessionRow;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _};
use crate::console_relay::{self, RelayRequest};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, RequirePermission, UserAgent, perm};
use crate::routes::client::vm_manage::{
    FwRow, OpView, chart_labels, fw_rows, op_views, parse_firewall_form,
};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};
use crate::vm_charts::{self, BandwidthBar, Chart};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/services/{id}/vm/manage", get(manage))
        .route("/admin/services/{id}/vm/console/view", get(console_view))
        .route("/admin/services/{id}/vm/console/token", post(console_token))
        .route("/admin/services/{id}/vm/console/ws", get(console_ws))
        .route("/admin/services/{id}/vm/password", post(password_reset))
        .route("/admin/services/{id}/vm/rescue", post(rescue))
        .route("/admin/services/{id}/vm/rdns", post(rdns))
        .route("/admin/services/{id}/vm/firewall", post(firewall))
        .route(
            "/admin/services/{id}/vm/backups/restore",
            post(backup_restore),
        )
        .route("/admin/services/{id}/vm/iso/approve", post(iso_approve))
        .route("/admin/services/{id}/vm/iso/remove", post(iso_remove))
        .route("/admin/services/{id}/vm/iso/mount", post(iso_mount))
        .route("/admin/services/{id}/vm/iso/eject", post(iso_eject))
}

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<ServiceRow> {
    services::get_service(&state.pool, tenant.id, ServiceId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

fn manage_url(s: &ServiceRow) -> String {
    format!("/admin/services/{}/vm/manage", s.id)
}

/// `Unsupported` → `None`; other errors → note text (server errors too:
/// one failing section must not hide the others from staff).
fn soft<T>(ctx: &PageCtx, res: Result<T, CoreError>, notes: &mut Vec<String>) -> Option<T> {
    match res {
        Ok(v) => Some(v),
        Err(CoreError::Integration(IntegrationError::Unsupported(_))) => None,
        Err(e) => {
            notes.push(ctx.tv("admin.common.failed", "reason", &e));
            None
        }
    }
}

/// A console session row for display.
struct SessionView {
    user: String,
    staff: bool,
    ip: String,
    issued: String,
    duration: String,
    traffic: String,
    reason: String,
}

#[derive(Template)]
#[template(path = "admin/services/vm_manage.html")]
struct ManagePage {
    ctx: PageCtx,
    s: ServiceRow,
    vm_name: String,
    features: VirtFeatures,
    notes: Vec<String>,
    range: String,
    charts: Vec<Chart>,
    bandwidth: Option<BandwidthBar>,
    backups: Vec<BackupInfo>,
    ptrs: Vec<IpPtr>,
    firewall: Option<FirewallState>,
    fw: Vec<FwRow>,
    extras: VmExtras,
    isos: Vec<ProviderIso>,
    ops: Vec<OpView>,
    sessions: Vec<SessionView>,
}
crate::page!(ManagePage);

impl ManagePage {
    fn base(&self) -> String {
        self.ctx.url(format!("/admin/services/{}/vm", self.s.id))
    }
}

#[derive(Debug, Deserialize)]
struct RangeQuery {
    #[serde(default)]
    range: Option<String>,
}

async fn manage(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesView>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let vm = services::vm_for_service(&state.pool, tenant.id, s.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let b = &state.billing;
    let mut notes = Vec::new();
    let features =
        soft(&ctx, b.vm_features(tenant.id, s.id, None).await, &mut notes).unwrap_or_default();
    let range = vm_charts::parse_range(q.range.as_deref());
    let charts = if features.metrics_history {
        soft(
            &ctx,
            b.vm_metrics_history(tenant.id, s.id, None, range).await,
            &mut notes,
        )
        .map(|h| vm_charts::charts(&h, &chart_labels(&ctx), |t| ctx.datetime(&t)))
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    let bandwidth = if features.bandwidth {
        soft(
            &ctx,
            b.vm_bandwidth(tenant.id, s.id, None).await,
            &mut notes,
        )
        .map(|v| vm_charts::bandwidth_bar(&v))
    } else {
        None
    };
    let backups = if features.backups {
        soft(&ctx, b.vm_backups(tenant.id, s.id, None).await, &mut notes).unwrap_or_default()
    } else {
        Vec::new()
    };
    let ptrs = soft(
        &ctx,
        b.vm_reverse_dns(tenant.id, s.id, None).await,
        &mut notes,
    )
    .unwrap_or_default();
    let firewall = if features.firewall {
        soft(&ctx, b.vm_firewall(tenant.id, s.id, None).await, &mut notes)
    } else {
        None
    };
    let fw = firewall.as_ref().map(|f| fw_rows(f, 3)).unwrap_or_default();
    let extras =
        soft(&ctx, b.vm_extras(tenant.id, s.id, None).await, &mut notes).unwrap_or_default();
    let isos = if features.iso {
        soft(&ctx, b.vm_provider_isos(tenant.id, s.id).await, &mut notes)
            .map(|(_, l)| l)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let ops = b.vm_operations(tenant.id, s.id, None, 50).await?;
    let sessions = b.vm_console_sessions(tenant.id, s.id, 25).await?;
    let sessions = session_views(&state, &tenant, &ctx, &sessions).await?;
    let title = ctx.t("admin.vm.manage");
    let back = format!("/admin/services/{}/vm", s.id);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[
            ("admin.services.title", "/admin/services"),
            ("admin.vm.title", back.as_str()),
        ],
    );
    let ops = op_views(&ctx, &ops, true);
    render(&ManagePage {
        vm_name: vm.name.clone(),
        range: range.as_str().to_owned(),
        s,
        features,
        notes,
        charts,
        bandwidth,
        backups,
        ptrs,
        firewall,
        fw,
        extras,
        isos,
        ops,
        sessions,
        ctx,
    })
}

async fn session_views(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    rows: &[ConsoleSessionRow],
) -> AppResult<Vec<SessionView>> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let user = hos_db::repo::identity::get_user(&state.pool, tenant.id, r.user_id)
            .await?
            .map(|u| u.email)
            .unwrap_or_else(|| r.user_id.to_string());
        let duration = match (r.connected_at, r.ended_at) {
            (Some(c), Some(e)) => format!("{} s", (e - c).num_seconds().max(0)),
            (Some(_), None) => ctx.t("admin.vm.session_open"),
            (None, _) => "—".into(),
        };
        out.push(SessionView {
            user,
            staff: r.is_staff,
            ip: r.ip.map(|i| i.to_string()).unwrap_or_default(),
            issued: ctx.datetime(&r.issued_at),
            duration,
            traffic: format!("↑ {} kB · ↓ {} kB", r.bytes_in / 1024, r.bytes_out / 1024),
            reason: r.close_reason.clone().unwrap_or_default(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// console
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/services/vm_console_live.html")]
struct ConsoleLive {
    ctx: PageCtx,
    s: ServiceRow,
    vm_name: String,
    relay: bool,
    token_url: String,
    error: String,
}
crate::page!(ConsoleLive);

async fn console_view(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let vm = services::vm_for_service(&state.pool, tenant.id, s.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let (relay, error) = match state.billing.vm_features(tenant.id, s.id, None).await {
        Ok(f) => (f.console_relay, String::new()),
        Err(e) => (false, common::failure_text(&ctx, &e)?),
    };
    let title = ctx.t("admin.vm.console");
    let back = format!("/admin/services/{}/vm", s.id);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[
            ("admin.services.title", "/admin/services"),
            ("admin.vm.title", back.as_str()),
        ],
    );
    let token_url = ctx.url(format!("/admin/services/{}/vm/console/token", s.id));
    let res = render(&ConsoleLive {
        vm_name: vm.name,
        relay,
        token_url,
        error,
        s,
        ctx,
    })?;
    Ok(crate::routes::client::no_store(res))
}

#[allow(clippy::too_many_arguments)]
async fn console_token(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    check_rate(&state.limits.vm_console, &format!("vmc:{}", staff.user_id))?;
    let who = ConsoleRequester {
        user_id: staff.user_id,
        client_id: None,
        ip: Some(ip.0),
        user_agent: ua.0,
    };
    match state
        .billing
        .vm_console_session(tenant.id, s.id, &who)
        .await
    {
        Ok(issued) => {
            au.log(
                &state,
                "vm.console",
                Some(("service", s.id.into())),
                json!({"session": issued.session_id}),
            )
            .await?;
            let ws = ctx.url(format!("/admin/services/{}/vm/console/ws", s.id));
            Ok(crate::routes::client::no_store(
                axum::Json(json!({"token": issued.token, "ws": ws})).into_response(),
            ))
        }
        Err(e) => {
            let m = common::failure_text(&ctx, &e)?;
            Ok((StatusCode::CONFLICT, axum::Json(json!({"error": m}))).into_response())
        }
    }
}

#[derive(Debug, Deserialize)]
struct WsQuery {
    #[serde(default)]
    token: String,
}

#[allow(clippy::too_many_arguments)]
async fn console_ws(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<Uuid>,
    Query(q): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    console_relay::upgrade(
        &state,
        ws,
        &headers,
        RelayRequest {
            tenant_id: tenant.id,
            service_id: s.id,
            who: ConsoleRequester {
                user_id: staff.user_id,
                client_id: None,
                ip: Some(ip.0),
                user_agent: ua.0,
            },
            session_id: staff.session.id,
            token: q.token,
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// actions
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/services/vm_secret.html")]
struct SecretPage {
    ctx: PageCtx,
    s: ServiceRow,
    heading: String,
    username: String,
    password: String,
}
crate::page!(SecretPage);

fn secret(
    mut ctx: PageCtx,
    staff: &crate::extract::CurrentStaff,
    s: ServiceRow,
    heading_key: &str,
    username: String,
    password: String,
) -> AppResult<Response> {
    let heading = ctx.t(heading_key);
    let back = format!("/admin/services/{}/vm/manage", s.id);
    common::page(
        &mut ctx,
        staff,
        &heading,
        &[
            ("admin.services.title", "/admin/services"),
            ("admin.vm.manage", back.as_str()),
        ],
    );
    Ok(crate::routes::client::no_store(render(&SecretPage {
        ctx,
        s,
        heading,
        username,
        password,
    })?))
}

async fn password_reset(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    if f.raw("confirm") != "yes" {
        return Ok(common::failed(
            &ctx,
            &manage_url(&s),
            ctx.t("admin.vm.confirm_needed"),
        ));
    }
    check_rate(&state.limits.vm_password, &format!("vmpw:{}", s.id))?;
    let user = f.opt("username", 32);
    match state
        .billing
        .vm_reset_password(tenant.id, s.id, None, user.as_deref())
        .await
    {
        Ok(r) => {
            au.log(
                &state,
                "vm.password_reset",
                Some(("service", s.id.into())),
                json!({"username": r.username}),
            )
            .await?;
            secret(
                ctx,
                &staff,
                s,
                "admin.vm.password_new",
                r.username,
                r.password.expose_str().to_owned(),
            )
        }
        Err(e) => Ok(common::failed(
            &ctx,
            &manage_url(&s),
            common::failure_text(&ctx, &e)?,
        )),
    }
}

async fn rescue(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let enable = f.raw("enable") == "1";
    match state
        .billing
        .vm_set_rescue(tenant.id, s.id, None, enable, &[])
        .await
    {
        Ok(r) => {
            au.log(
                &state,
                "vm.rescue",
                Some(("service", s.id.into())),
                json!({"enabled": enable}),
            )
            .await?;
            match (enable, r.password) {
                (true, Some(pw)) => secret(
                    ctx,
                    &staff,
                    s,
                    "admin.vm.rescue_enabled",
                    "root".into(),
                    pw.expose_str().to_owned(),
                ),
                _ => Ok(common::done(&ctx, &manage_url(&s), "admin.vm.saved")),
            }
        }
        Err(e) => Ok(common::failed(
            &ctx,
            &manage_url(&s),
            common::failure_text(&ctx, &e)?,
        )),
    }
}

async fn rdns(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let Ok(ip) = f.raw("ip").trim().parse() else {
        return Err(AppError::bad_request("ip"));
    };
    let ptr = f.opt("ptr", 253);
    let res = state
        .billing
        .vm_set_reverse_dns(tenant.id, s.id, None, ip, ptr.as_deref())
        .await;
    if res.is_ok() {
        au.log(
            &state,
            "vm.rdns",
            Some(("service", s.id.into())),
            json!({"ip": ip.to_string(), "ptr": ptr}),
        )
        .await?;
    }
    common::outcome(&ctx, &manage_url(&s), res, "admin.vm.saved")
}

async fn firewall(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let fw = match parse_firewall_form(&fields) {
        Ok(f) => f,
        Err(rule) => {
            return Ok(common::failed(
                &ctx,
                &manage_url(&s),
                ctx.tv("client.vm.firewall_invalid", "rule", rule),
            ));
        }
    };
    let res = state
        .billing
        .vm_set_firewall(tenant.id, s.id, None, &fw)
        .await;
    if let Ok(applied) = &res {
        au.log(
            &state,
            "vm.firewall",
            Some(("service", s.id.into())),
            json!({"enabled": applied.enabled, "rules": applied.rules.len()}),
        )
        .await?;
    }
    common::outcome(&ctx, &manage_url(&s), res, "admin.vm.saved")
}

async fn backup_restore(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    if f.raw("confirm") != "yes" {
        return Ok(common::failed(
            &ctx,
            &manage_url(&s),
            ctx.t("admin.vm.confirm_needed"),
        ));
    }
    let backup = f.text("backup", 255);
    let res = state
        .billing
        .vm_restore_backup(tenant.id, s.id, None, &backup)
        .await;
    if res.is_ok() {
        au.log(
            &state,
            "vm.backup_restore",
            Some(("service", s.id.into())),
            json!({"backup": backup}),
        )
        .await?;
    }
    common::outcome(&ctx, &manage_url(&s), res, "admin.vm.restore_started")
}

async fn iso_approve(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let iso = f.text("iso", 255);
    let res = state.billing.vm_approve_iso(tenant.id, s.id, &iso).await;
    if let Ok(row) = &res {
        au.log(
            &state,
            "vm.iso_approve",
            Some(("integration", row.integration_id.into())),
            json!({"iso": row.iso_ref}),
        )
        .await?;
    }
    common::outcome(&ctx, &manage_url(&s), res, "admin.vm.saved")
}

async fn iso_remove(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let Ok(iso_id) = f.raw("iso_id").parse::<Uuid>() else {
        return Err(AppError::bad_request("iso_id"));
    };
    let vm = services::vm_for_service(&state.pool, tenant.id, s.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let res = state
        .billing
        .vm_remove_iso(tenant.id, vm.integration_id, iso_id)
        .await;
    if res.is_ok() {
        au.log(
            &state,
            "vm.iso_remove",
            Some(("integration", vm.integration_id.into())),
            json!({"id": iso_id}),
        )
        .await?;
    }
    common::outcome(&ctx, &manage_url(&s), res, "admin.vm.saved")
}

async fn iso_mount(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let iso = f.text("iso", 255);
    let res = state
        .billing
        .vm_mount_iso(tenant.id, s.id, None, &iso, f.raw("boot") == "1")
        .await;
    if res.is_ok() {
        au.log(
            &state,
            "vm.iso_mount",
            Some(("service", s.id.into())),
            json!({"iso": iso}),
        )
        .await?;
    }
    common::outcome(&ctx, &manage_url(&s), res, "admin.vm.saved")
}

async fn iso_eject(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let res = state.billing.vm_unmount_iso(tenant.id, s.id, None).await;
    if res.is_ok() {
        au.log(
            &state,
            "vm.iso_unmount",
            Some(("service", s.id.into())),
            json!({}),
        )
        .await?;
    }
    common::outcome(&ctx, &manage_url(&s), res, "admin.vm.saved")
}
