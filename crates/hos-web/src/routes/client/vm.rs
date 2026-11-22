//! VPS / cloud server management (`/client/services/{id}/server`): live
//! state and metric charts (htmx polling, inline SVG), power actions,
//! reinstall, snapshots, console access and the IP list. Every action goes
//! through `hos_billing` (`vm_*`), which checks ownership and service state.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use hos_core::integrations::virt::{
    ConsoleKind, PowerAction, VirtCapabilities, VmInfo, VmMetrics, VmStatus,
};
use hos_db::repo::services::{IpAddressRow, ServiceRow, VmRow};
use serde::Deserialize;

use super::services::own_service;
use super::{Fmt as _, MemberAccess, audit_client, friendly_error, no_store, setup};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, UserAgent};
use crate::flash::{self, SetFlash};
use crate::routes::cart::configure::valid_ssh_key;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/services/{id}/server", get(page))
        .route("/client/services/{id}/server/live", get(live))
        .route("/client/services/{id}/server/power", post(power))
        .route("/client/services/{id}/server/reinstall", post(reinstall))
        .route(
            "/client/services/{id}/server/snapshots",
            post(snapshot_create),
        )
        .route(
            "/client/services/{id}/server/snapshots/{sid}/rollback",
            post(snapshot_rollback),
        )
        .route(
            "/client/services/{id}/server/snapshots/{sid}/delete",
            post(snapshot_delete),
        )
        .route("/client/services/{id}/server/console", post(console))
        .merge(super::vm_manage::routes())
        .merge(super::vm_ssh_keys::routes())
}

const HISTORY: usize = 30;

/// One metric series for an inline SVG sparkline.
pub struct Series {
    /// Field name carrying the history between polls.
    pub key: &'static str,
    /// Label.
    pub label: String,
    /// Latest value, formatted.
    pub current: String,
    /// Comma-separated history (hidden input).
    pub history: String,
    /// SVG polyline points (viewBox 0 0 300 60).
    pub points: String,
}

/// The live panel (fragment, polled).
pub struct LiveView {
    /// `running`, `stopped`, …
    pub status: String,
    /// In a transitional state (faster polling, progress bar).
    pub busy: bool,
    /// Uptime text.
    pub uptime: String,
    /// Charts.
    pub series: Vec<Series>,
    /// Disk used / total text.
    pub disk: String,
    /// Disk used percent.
    pub disk_percent: Option<u8>,
    /// Bandwidth used this period.
    pub bandwidth: String,
    /// Metrics unavailable (reason).
    pub error: String,
}

fn parse_history(raw: Option<&str>) -> Vec<f64> {
    raw.unwrap_or("")
        .split(',')
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .collect()
}

fn points(values: &[f64], max: f64) -> String {
    if values.is_empty() {
        return String::new();
    }
    let max = if max > 0.0 { max } else { 1.0 };
    let step = 300.0 / (HISTORY.max(2) - 1) as f64;
    let offset = HISTORY.saturating_sub(values.len()) as f64 * step;
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = offset + i as f64 * step;
            let y = 58.0 - (v / max).clamp(0.0, 1.0) * 56.0;
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn rate(v: f64) -> String {
    let (n, unit) = if v >= 1e9 {
        (v / 1e9, "Gbit/s")
    } else if v >= 1e6 {
        (v / 1e6, "Mbit/s")
    } else if v >= 1e3 {
        (v / 1e3, "kbit/s")
    } else {
        (v, "bit/s")
    };
    format!("{n:.1} {unit}")
}

fn series(
    key: &'static str,
    label: String,
    prev: &[f64],
    value: Option<f64>,
    fixed_max: Option<f64>,
    fmt: impl Fn(f64) -> String,
) -> Series {
    let mut h: Vec<f64> = prev.to_vec();
    if let Some(v) = value {
        h.push(v);
    }
    if h.len() > HISTORY {
        h.drain(..h.len() - HISTORY);
    }
    let max = fixed_max.unwrap_or_else(|| h.iter().copied().fold(0.0, f64::max) * 1.2);
    Series {
        key,
        label,
        current: value.map_or_else(|| "—".to_owned(), &fmt),
        history: h
            .iter()
            .map(|v| format!("{v:.2}"))
            .collect::<Vec<_>>()
            .join(","),
        points: points(&h, max),
    }
}

fn uptime(ctx: &PageCtx, secs: Option<u64>) -> String {
    match secs {
        Some(s) if s >= 86_400 => ctx.tn("client.vm.uptime_days", (s / 86_400) as i64),
        Some(s) if s >= 3600 => ctx.tn("client.vm.uptime_hours", (s / 3600) as i64),
        Some(s) => ctx.tn("client.vm.uptime_minutes", (s / 60) as i64),
        None => "—".to_owned(),
    }
}

fn busy(status: VmStatus) -> bool {
    matches!(
        status,
        VmStatus::Starting | VmStatus::Stopping | VmStatus::Provisioning | VmStatus::Migrating
    )
}

async fn live_view(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    client: &CurrentClient,
    s: &ServiceRow,
    history: &[(String, String)],
) -> AppResult<LiveView> {
    let h = |k: &str| {
        parse_history(
            history
                .iter()
                .rev()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str()),
        )
    };
    match state
        .billing
        .vm_metrics(tenant.id, s.id, Some(client.client_id))
        .await
    {
        Ok(m) => Ok(metrics_view(ctx, &m, h)),
        Err(e) => {
            let error = friendly_error(ctx, &e).ok_or_else(|| AppError::from(e))?;
            Ok(LiveView {
                status: "unknown".into(),
                busy: false,
                uptime: "—".into(),
                series: Vec::new(),
                disk: "—".into(),
                disk_percent: None,
                bandwidth: "—".into(),
                error,
            })
        }
    }
}

fn metrics_view(ctx: &PageCtx, m: &VmMetrics, h: impl Fn(&str) -> Vec<f64>) -> LiveView {
    let mem = match (m.memory_used_mb, m.memory_total_mb) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 * 100.0 / t as f64),
        _ => None,
    };
    let pct = |v: f64| format!("%{v:.0}");
    let series = vec![
        series(
            "h_cpu",
            ctx.t("client.vm.cpu"),
            &h("h_cpu"),
            m.cpu_usage_percent,
            Some(100.0),
            pct,
        ),
        series(
            "h_mem",
            ctx.t("client.vm.memory"),
            &h("h_mem"),
            mem,
            Some(100.0),
            pct,
        ),
        series(
            "h_in",
            ctx.t("client.vm.net_in"),
            &h("h_in"),
            m.net_in_bps,
            None,
            rate,
        ),
        series(
            "h_out",
            ctx.t("client.vm.net_out"),
            &h("h_out"),
            m.net_out_bps,
            None,
            rate,
        ),
    ];
    let (disk, disk_percent) = match (m.disk_used_gb, m.disk_total_gb) {
        (Some(u), Some(t)) if t > 0.0 => (
            format!("{u:.1} GB / {t:.0} GB"),
            Some(((u / t) * 100.0).clamp(0.0, 100.0) as u8),
        ),
        (Some(u), _) => (format!("{u:.1} GB"), None),
        _ => ("—".into(), None),
    };
    LiveView {
        status: m.status.as_str().to_owned(),
        busy: busy(m.status),
        uptime: uptime(ctx, m.uptime_secs),
        series,
        disk,
        disk_percent,
        bandwidth: m
            .bandwidth_used_gb
            .map_or_else(|| "—".to_owned(), |b| format!("{b:.2} GB")),
        error: String::new(),
    }
}

#[derive(Template)]
#[template(path = "client/vm.html")]
struct VmPage {
    ctx: PageCtx,
    s: ServiceRow,
    product_name: String,
    vm: Option<VmRow>,
    info: Option<VmInfo>,
    caps: Option<VirtCapabilities>,
    live: LiveView,
    ips: Vec<IpAddressRow>,
    error: String,
    hostname: String,
    live_url: String,
}
crate::page!(VmPage);

impl VmPage {
    fn running(&self) -> bool {
        self.live.status == "running"
    }
    fn console_enabled(&self) -> bool {
        self.caps
            .as_ref()
            .is_some_and(|c| !c.console_kinds.is_empty())
    }
}

#[derive(Template)]
#[template(path = "client/_vm_live.html")]
struct LiveFragment {
    ctx: PageCtx,
    live_url: String,
    live: LiveView,
}
crate::page!(LiveFragment);

async fn page(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let product_name = hos_db::repo::catalog::get_product(&state.pool, tenant.id, s.product_id)
        .await?
        .map(|p| p.name)
        .unwrap_or_default();
    let mut error = String::new();
    let (vm, info, caps) = match state
        .billing
        .vm_info(tenant.id, s.id, Some(client.client_id))
        .await
    {
        Ok((vm, info, caps)) => (Some(vm), Some(info), Some(caps)),
        Err(e) => {
            error = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            (
                hos_db::repo::services::vm_for_service(&state.pool, tenant.id, s.id).await?,
                None,
                None,
            )
        }
    };
    if vm.is_none() {
        return Err(AppError::NotFound);
    }
    let live = if info.is_some() {
        live_view(&state, &tenant, &ctx, &client, &s, &[]).await?
    } else {
        LiveView {
            status: vm
                .as_ref()
                .map_or("unknown", |v| v.status.as_str())
                .to_owned(),
            busy: false,
            uptime: "—".into(),
            series: Vec::new(),
            disk: "—".into(),
            disk_percent: None,
            bandwidth: "—".into(),
            error: error.clone(),
        }
    };
    let ips = vm_ips(&state, &tenant, &s).await?;
    let hostname = vm
        .as_ref()
        .and_then(|v| v.hostname.clone())
        .or_else(|| s.server_hostname.clone())
        .or_else(|| s.domain.clone())
        .unwrap_or_default();
    let title = ctx.t("client.vm.title");
    let crumbs = vec![
        Crumb::link(ctx.t("client.services.title"), ctx.url("/client/services")),
        Crumb::link(
            product_name.clone(),
            ctx.url(format!("/client/services/{}", s.id)),
        ),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    let res = render(&VmPage {
        live_url: ctx.url(format!("/client/services/{}/server/live", s.id)),
        ctx,
        s,
        product_name,
        vm,
        info,
        caps,
        live,
        ips,
        error,
        hostname,
    })?;
    Ok(no_store(res))
}

async fn vm_ips(state: &AppState, tenant: &Tenant, s: &ServiceRow) -> AppResult<Vec<IpAddressRow>> {
    Ok(hos_db::repo::services::service_ips(&state.pool, tenant.id, s.id).await?)
}

/// `GET …/server/live` (htmx, every few seconds): state and charts; the
/// previous samples come back as hidden fields.
async fn live(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    axum::extract::Query(history): axum::extract::Query<Vec<(String, String)>>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let live = live_view(&state, &tenant, &ctx, &client, &s, &history).await?;
    let res = render(&LiveFragment {
        live_url: ctx.url(format!("/client/services/{}/server/live", s.id)),
        ctx,
        live,
    })?;
    Ok(no_store(res))
}

fn back(ctx: &PageCtx, s: &ServiceRow) -> String {
    ctx.url(format!("/client/services/{}/server", s.id))
}

fn back_to(ctx: &PageCtx, s: &ServiceRow, tab: &str) -> String {
    ctx.url(format!("/client/services/{}/server/{tab}", s.id))
}

#[allow(clippy::too_many_arguments)]
async fn act<T>(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    (ip, ua): (ClientIp, &UserAgent),
    ctx: &PageCtx,
    (s, to): (&ServiceRow, String),
    action: &str,
    res: Result<T, hos_core::CoreError>,
    ok: String,
) -> AppResult<Response> {
    match res {
        Ok(_) => {
            audit_client(
                state,
                tenant,
                client,
                ip,
                ua,
                action,
                Some(("service", s.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(&to, SetFlash::success(ok)))
        }
        Err(e) => match friendly_error(ctx, &e) {
            Some(m) => Ok(flash::redirect(&to, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

#[derive(Debug, Deserialize)]
struct PowerForm {
    action: String,
}

#[allow(clippy::too_many_arguments)]
async fn power(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<PowerForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let action: PowerAction = f
        .action
        .parse()
        .map_err(|_| AppError::bad_request("action"))?;
    let res = state
        .billing
        .vm_power(tenant.id, s.id, Some(client.client_id), action)
        .await;
    let ok = ctx.t(&format!("client.vm.power_done.{}", action.as_str()));
    act(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        (&s, back(&ctx, &s)),
        "client.vm.power",
        res,
        ok,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn reinstall(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let to = back_to(&ctx, &s, "recovery");
    let field = |k: &str| {
        fields
            .iter()
            .rev()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let (template, confirm_hostname, ssh_key) = (
        field("template"),
        field("confirm_hostname"),
        field("ssh_key"),
    );
    let key_ids: Vec<uuid::Uuid> = fields
        .iter()
        .filter(|(k, _)| k == "key_id")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    let vm = hos_db::repo::services::vm_for_service(&state.pool, tenant.id, s.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let expected = vm
        .hostname
        .clone()
        .or_else(|| s.server_hostname.clone())
        .or_else(|| s.domain.clone())
        .unwrap_or_else(|| vm.name.clone());
    if !confirm_hostname.trim().eq_ignore_ascii_case(&expected) {
        return Ok(flash::redirect(
            &to,
            SetFlash::error(ctx.t("client.vm.reinstall_confirm_mismatch")),
        ));
    }
    let key = ssh_key.trim();
    if !key.is_empty() && !valid_ssh_key(key) {
        return Ok(flash::redirect(
            &to,
            SetFlash::error(ctx.t("cart.errors.ssh_key")),
        ));
    }
    let templates = state
        .billing
        .vm_templates(tenant.id, s.id, Some(client.client_id))
        .await
        .unwrap_or_default();
    if !templates.iter().any(|t| t.id == template) {
        return Ok(flash::redirect(
            &to,
            SetFlash::error(ctx.t("client.vm.template_invalid")),
        ));
    }
    let mut keys = match state
        .billing
        .resolve_client_ssh_keys(tenant.id, client.client_id, &key_ids)
        .await
    {
        Ok(k) => k,
        Err(e) => {
            let m = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            return Ok(flash::redirect(&to, SetFlash::error(m)));
        }
    };
    if !key.is_empty() {
        keys.push(key.to_owned());
    }
    let res = state
        .billing
        .vm_reinstall(tenant.id, s.id, Some(client.client_id), &template, keys)
        .await;
    let ok = ctx.t("client.vm.reinstall_started");
    act(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        (&s, back_to(&ctx, &s, "recovery")),
        "client.vm.reinstall",
        res,
        ok,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct SnapshotForm {
    #[serde(default)]
    name: String,
}

#[allow(clippy::too_many_arguments)]
async fn snapshot_create(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<SnapshotForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let name: String = f
        .name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(40)
        .collect();
    if name.is_empty() {
        return Ok(flash::redirect(
            &back_to(&ctx, &s, "backups"),
            SetFlash::error(ctx.t("client.vm.snapshot_name_invalid")),
        ));
    }
    let res = state
        .billing
        .vm_create_snapshot(tenant.id, s.id, Some(client.client_id), &name)
        .await;
    let ok = ctx.tv("client.vm.snapshot_created", "name", &name);
    act(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        (&s, back_to(&ctx, &s, "backups")),
        "client.vm.snapshot_created",
        res,
        ok,
    )
    .await
}

async fn snapshot_rollback(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path((id, sid)): Path<(String, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let res = state
        .billing
        .vm_rollback_snapshot(tenant.id, s.id, Some(client.client_id), &sid)
        .await;
    let ok = ctx.t("client.vm.snapshot_restored");
    act(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        (&s, back_to(&ctx, &s, "backups")),
        "client.vm.snapshot_restored",
        res,
        ok,
    )
    .await
}

async fn snapshot_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path((id, sid)): Path<(String, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let res = state
        .billing
        .vm_delete_snapshot(tenant.id, s.id, Some(client.client_id), &sid)
        .await;
    let ok = ctx.t("client.vm.snapshot_deleted");
    act(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        (&s, back_to(&ctx, &s, "backups")),
        "client.vm.snapshot_deleted",
        res,
        ok,
    )
    .await
}

#[derive(Template)]
#[template(path = "client/vm_console.html")]
struct ConsolePage {
    ctx: PageCtx,
    service_id: String,
    kind: String,
    url: String,
    browser: bool,
    password: String,
    ticket: String,
    port: String,
    expires: String,
}
crate::page!(ConsolePage);

/// `POST …/server/console`: a short-lived console session. Browser
/// consoles (https) open directly; others show the connection details.
#[allow(clippy::too_many_arguments)]
async fn console(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let c = match state
        .billing
        .vm_console(tenant.id, s.id, Some(client.client_id))
        .await
    {
        Ok(c) => c,
        Err(hos_core::CoreError::Integration(hos_core::IntegrationError::Unsupported(_)))
            if state
                .billing
                .vm_features(tenant.id, s.id, Some(client.client_id))
                .await
                .is_ok_and(|f| f.console_relay) =>
        {
            return Ok(Redirect::to(&back_to(&ctx, &s, "console/view")).into_response());
        }
        Err(e) => {
            let m = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            return Ok(flash::redirect(&back(&ctx, &s), SetFlash::error(m)));
        }
    };
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.vm.console",
        Some(("service", s.id.into_uuid())),
    )
    .await;
    let browser = c.url.starts_with("https://");
    if browser && c.password.is_none() && c.ticket.is_none() {
        return Ok(no_store(Redirect::to(&c.url).into_response()));
    }
    let title = ctx.t("client.vm.console");
    let crumbs = vec![
        Crumb::link(ctx.t("client.services.title"), ctx.url("/client/services")),
        Crumb::link(ctx.t("client.vm.title"), back(&ctx, &s)),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    let kind = match c.kind {
        ConsoleKind::NoVnc => "noVNC".to_owned(),
        other => other.as_str().to_uppercase(),
    };
    let res = render(&ConsolePage {
        service_id: s.id.to_string(),
        kind,
        browser,
        password: c
            .password
            .as_ref()
            .map(|p| p.expose_str().to_owned())
            .unwrap_or_default(),
        ticket: c
            .ticket
            .as_ref()
            .map(|p| p.expose_str().to_owned())
            .unwrap_or_default(),
        port: c.port.map(|p| p.to_string()).unwrap_or_default(),
        expires: c.expires_at.map(|e| ctx.datetime(&e)).unwrap_or_default(),
        url: c.url,
        ctx,
    })?;
    Ok(no_store(res))
}
