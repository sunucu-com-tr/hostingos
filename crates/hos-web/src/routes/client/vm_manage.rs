//! VPS management tabs of the client area (`/client/services/{id}/server/…`)
//! next to the overview of [`super::vm`]: embedded console (noVNC over the
//! HostingOS relay, [`crate::console_relay`]), usage graphs and bandwidth,
//! snapshots and backups, network (reverse DNS, firewall), recovery (root
//! password reset, rescue mode, ISO / boot order, reinstall) and the
//! activity log of VM operations.
//!
//! Every handler re-loads the service as the signed-in client's
//! ([`own_service`]); hos-billing checks ownership, status, allow-lists and
//! limits again for every action. All paths are under `/client/services`,
//! so sub-users need the `services` area. Mutations are CSRF-protected by
//! the form middleware, rate-limited (`vm_action`, `vm_password`,
//! `vm_console`) and audited.

use askama::Template;
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hos_billing::vm_ops::{
    ConsoleRequester, IpPtr, IsoOption, MAX_FIREWALL_RULES, default_firewall_rules,
};
use hos_core::CoreError;
use hos_core::error::IntegrationError;
use hos_core::integrations::virt::{
    BackupInfo, FirewallDirection, FirewallProtocol, FirewallRule, FirewallState, SnapshotInfo,
    TemplateInfo, VirtFeatures, VmExtras,
};
use hos_db::repo::services::ServiceRow;
use hos_db::repo::vm_ops::{SshKeyRow, VmOperationRow};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::services::own_service;
use super::{MemberAccess, audit_client, friendly_error, no_store, setup};
use crate::console_relay::{self, RelayRequest};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, UserAgent};
use crate::flash::{self, SetFlash};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render};
use crate::vm_charts::{self, BandwidthBar, Chart};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/client/services/{id}/server/console/view",
            get(console_view),
        )
        .route(
            "/client/services/{id}/server/console/token",
            post(console_token),
        )
        .route("/client/services/{id}/server/console/ws", get(console_ws))
        .route("/client/services/{id}/server/graphs", get(graphs))
        .route("/client/services/{id}/server/backups", get(backups))
        .route(
            "/client/services/{id}/server/backups/restore",
            post(backup_restore),
        )
        .route("/client/services/{id}/server/network", get(network))
        .route("/client/services/{id}/server/rdns", post(rdns_save))
        .route("/client/services/{id}/server/firewall", post(firewall_save))
        .route("/client/services/{id}/server/recovery", get(recovery))
        .route(
            "/client/services/{id}/server/password",
            post(password_reset),
        )
        .route("/client/services/{id}/server/rescue", post(rescue))
        .route("/client/services/{id}/server/iso", post(iso_mount))
        .route("/client/services/{id}/server/iso/eject", post(iso_unmount))
        .route("/client/services/{id}/server/activity", get(activity))
        .route(
            "/client/services/{id}/server/activity/feed",
            get(activity_feed),
        )
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

/// An operation for display.
pub struct OpView {
    /// Translated kind.
    pub kind: String,
    /// Detail text.
    pub detail: String,
    /// `queued|running|succeeded|failed`.
    pub status: String,
    /// Progress 0–100 when known.
    pub progress: Option<i16>,
    /// Started.
    pub created: String,
    /// Duration / finish text.
    pub finished: String,
    /// Error text (staff see the provider's; clients a generic one).
    pub error: String,
}

/// Operations for a template (`staff`: show provider error texts).
pub fn op_views(ctx: &PageCtx, ops: &[VmOperationRow], staff: bool) -> Vec<OpView> {
    ops.iter()
        .map(|o| OpView {
            kind: ctx.t(&format!("client.vm.op.{}", o.kind)),
            detail: o.detail.clone(),
            status: o.status.clone(),
            progress: o.progress,
            created: ctx.datetime(&o.created_at),
            finished: o.finished_at.map(|f| ctx.datetime(&f)).unwrap_or_default(),
            error: match (&o.error, staff) {
                (Some(e), true) => e.clone(),
                (Some(_), false) => ctx.t("client.vm.op_failed_hint"),
                (None, _) => String::new(),
            },
        })
        .collect()
}

/// A firewall form row.
pub struct FwRow {
    /// Row index (field names `r{idx}_…`).
    pub idx: usize,
    /// `in` / `out`.
    pub dir: String,
    /// `tcp` / `udp` / `icmp`.
    pub proto: String,
    /// Port or range.
    pub port: String,
    /// Networks, comma separated.
    pub src: String,
    /// Comment.
    pub note: String,
}

/// Rows of the firewall form: the current rules plus `blank` empty rows.
pub fn fw_rows(state: &FirewallState, blank: usize) -> Vec<FwRow> {
    let mut rows: Vec<FwRow> = state
        .rules
        .iter()
        .enumerate()
        .map(|(idx, r)| FwRow {
            idx,
            dir: r.direction.as_str().to_owned(),
            proto: r.protocol.as_str().to_owned(),
            port: r.port.clone().unwrap_or_default(),
            src: r.cidrs.join(", "),
            note: r.comment.clone().unwrap_or_default(),
        })
        .collect();
    let n = rows.len();
    for idx in n..(n + blank).min(MAX_FIREWALL_RULES) {
        rows.push(FwRow {
            idx,
            dir: "in".into(),
            proto: "tcp".into(),
            port: String::new(),
            src: String::new(),
            note: String::new(),
        });
    }
    rows
}

/// Parses the firewall form (`enabled`, `r{i}_dir|proto|port|src|note`);
/// rows without a port, network or comment on TCP/UDP are skipped.
pub fn parse_firewall_form(fields: &[(String, String)]) -> Result<FirewallState, String> {
    let get = |k: &str| {
        fields
            .iter()
            .rev()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.trim().to_owned())
            .unwrap_or_default()
    };
    let mut rules = Vec::new();
    for i in 0..MAX_FIREWALL_RULES + 10 {
        let dir = get(&format!("r{i}_dir"));
        if dir.is_empty() {
            continue;
        }
        let proto = get(&format!("r{i}_proto"));
        let port = get(&format!("r{i}_port"));
        let src = get(&format!("r{i}_src"));
        let note = get(&format!("r{i}_note"));
        let protocol: FirewallProtocol = proto.parse().map_err(|_| format!("rule {}", i + 1))?;
        if protocol != FirewallProtocol::Icmp
            && port.is_empty()
            && src.is_empty()
            && note.is_empty()
        {
            continue;
        }
        if get(&format!("r{i}_drop")) == "1" {
            continue;
        }
        rules.push(FirewallRule {
            direction: dir
                .parse::<FirewallDirection>()
                .map_err(|_| format!("rule {}", i + 1))?,
            protocol,
            port: (!port.is_empty()).then_some(port),
            cidrs: src
                .split([',', ' ', '\n'])
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_owned)
                .collect(),
            comment: (!note.is_empty()).then_some(note),
        });
    }
    Ok(FirewallState {
        enabled: get("enabled") == "1",
        rules,
    })
}

#[derive(Template)]
#[template(path = "client/vm_tab.html")]
struct TabPage {
    ctx: PageCtx,
    s: ServiceRow,
    tab: &'static str,
    hostname: String,
    features: VirtFeatures,
    notice: String,
    // graphs
    range: String,
    charts: Vec<Chart>,
    bandwidth: Option<BandwidthBar>,
    graphs_note: String,
    // snapshots / backups
    snapshots_supported: bool,
    snapshots: Vec<SnapshotInfo>,
    snapshot_quota: u32,
    backups: Vec<BackupInfo>,
    // network
    ptrs: Vec<IpPtr>,
    firewall: Option<FirewallState>,
    fw: Vec<FwRow>,
    // recovery
    extras: VmExtras,
    isos: Vec<IsoOption>,
    templates: Vec<TemplateInfo>,
    keys: Vec<SshKeyRow>,
    // activity
    ops: Vec<OpView>,
    busy: bool,
}
crate::page!(TabPage);

impl TabPage {
    fn base(&self) -> String {
        self.ctx
            .url(format!("/client/services/{}/server", self.s.id))
    }
    fn snapshot_full(&self) -> bool {
        self.snapshots.len() >= self.snapshot_quota as usize
    }
}

#[derive(Template)]
#[template(path = "client/_vm_ops.html")]
struct OpsFragment {
    ctx: PageCtx,
    feed_url: String,
    ops: Vec<OpView>,
    busy: bool,
}
crate::page!(OpsFragment);

#[derive(Template)]
#[template(path = "client/vm_secret.html")]
struct SecretPage {
    ctx: PageCtx,
    heading: String,
    lead: String,
    username: String,
    password: String,
    back: String,
}
crate::page!(SecretPage);

#[derive(Template)]
#[template(path = "client/vm_console_live.html")]
struct ConsoleLive {
    ctx: PageCtx,
    s: ServiceRow,
    hostname: String,
    relay: bool,
    token_url: String,
    error: String,
}
crate::page!(ConsoleLive);

fn hostname(s: &ServiceRow, vm: Option<&hos_db::repo::services::VmRow>) -> String {
    vm.and_then(|v| v.hostname.clone())
        .or_else(|| s.server_hostname.clone())
        .or_else(|| s.domain.clone())
        .or_else(|| vm.map(|v| v.name.clone()))
        .unwrap_or_default()
}

fn tab_url(ctx: &PageCtx, s: &ServiceRow, tab: &str) -> String {
    if tab == "overview" {
        ctx.url(format!("/client/services/{}/server", s.id))
    } else {
        ctx.url(format!("/client/services/{}/server/{tab}", s.id))
    }
}

/// `Unsupported` → `None` (the section shows "not supported"); other user
/// errors → notice text; server errors propagate.
fn soft<T>(ctx: &PageCtx, res: Result<T, CoreError>, notice: &mut String) -> AppResult<Option<T>> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(CoreError::Integration(IntegrationError::Unsupported(_))) => Ok(None),
        Err(e) => {
            let m = friendly_error(ctx, &e).ok_or_else(|| AppError::from(e))?;
            if notice.is_empty() {
                *notice = m;
            }
            Ok(None)
        }
    }
}

struct Loaded {
    s: ServiceRow,
    hostname: String,
    features: VirtFeatures,
    notice: String,
}

async fn load(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    id: &str,
    ctx: &mut PageCtx,
    tab: &'static str,
) -> AppResult<Loaded> {
    let s = own_service(state, tenant, client, id).await?;
    let vm = hos_db::repo::services::vm_for_service(&state.pool, tenant.id, s.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut notice = String::new();
    let features = soft(
        ctx,
        state
            .billing
            .vm_features(tenant.id, s.id, Some(client.client_id))
            .await,
        &mut notice,
    )?
    .unwrap_or_default();
    let title = ctx.t(&format!("client.vm.tab.{tab}"));
    let crumbs = vec![
        Crumb::link(ctx.t("client.services.title"), ctx.url("/client/services")),
        Crumb::link(ctx.t("client.vm.title"), tab_url(ctx, &s, "overview")),
        Crumb::current(title.clone()),
    ];
    setup(ctx, client, access, title, crumbs);
    Ok(Loaded {
        hostname: hostname(&s, Some(&vm)),
        s,
        features,
        notice,
    })
}

fn page(l: Loaded, ctx: PageCtx, tab: &'static str) -> TabPage {
    TabPage {
        ctx,
        s: l.s,
        tab,
        hostname: l.hostname,
        features: l.features,
        notice: l.notice,
        range: String::new(),
        charts: Vec::new(),
        bandwidth: None,
        graphs_note: String::new(),
        snapshots_supported: false,
        snapshots: Vec::new(),
        snapshot_quota: 0,
        backups: Vec::new(),
        ptrs: Vec::new(),
        firewall: None,
        fw: Vec::new(),
        extras: VmExtras::default(),
        isos: Vec::new(),
        templates: Vec::new(),
        keys: Vec::new(),
        ops: Vec::new(),
        busy: false,
    }
}

/// Redirect with a flash: audited success or a user-facing error.
#[allow(clippy::too_many_arguments)]
async fn finish<T>(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    (ip, ua): (ClientIp, &UserAgent),
    ctx: &PageCtx,
    to: String,
    action: &str,
    s: &ServiceRow,
    res: Result<T, CoreError>,
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

fn action_key(client: &CurrentClient, s: &ServiceRow) -> String {
    format!("vm:{}:{}", client.user_id, s.id)
}

// ---------------------------------------------------------------------------
// console
// ---------------------------------------------------------------------------

/// `GET …/server/console/view`: the embedded console (relay drivers) or the
/// provider console hand-off (vCenter WebMKS).
async fn console_view(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let l = load(&state, &tenant, &client, &access, &id, &mut ctx, "console").await?;
    let token_url = ctx.url(format!("/client/services/{}/server/console/token", l.s.id));
    let res = render(&ConsoleLive {
        relay: l.features.console_relay,
        error: l.notice,
        hostname: l.hostname,
        s: l.s,
        token_url,
        ctx,
    })?;
    Ok(no_store(res))
}

/// `POST …/server/console/token` (fetch): a one-time console token.
async fn console_token(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    check_rate(&state.limits.vm_console, &format!("vmc:{}", client.user_id))?;
    let who = ConsoleRequester {
        user_id: client.user_id,
        client_id: Some(client.client_id),
        ip: Some(ip.0),
        user_agent: ua.0.clone(),
    };
    match state
        .billing
        .vm_console_session(tenant.id, s.id, &who)
        .await
    {
        Ok(issued) => {
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
            let ws = ctx.url(format!("/client/services/{}/server/console/ws", s.id));
            Ok(no_store(
                axum::Json(
                    json!({"token": issued.token, "ws": ws, "expires_at": issued.expires_at}),
                )
                .into_response(),
            ))
        }
        Err(e) => {
            let m = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            Ok((StatusCode::CONFLICT, axum::Json(json!({"error": m}))).into_response())
        }
    }
}

#[derive(Debug, Deserialize)]
struct WsQuery {
    #[serde(default)]
    token: String,
}

/// `GET …/server/console/ws?token=`: the relay websocket.
#[allow(clippy::too_many_arguments)]
async fn console_ws(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    Query(q): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    console_relay::upgrade(
        &state,
        ws,
        &headers,
        RelayRequest {
            tenant_id: tenant.id,
            service_id: s.id,
            who: ConsoleRequester {
                user_id: client.user_id,
                client_id: Some(client.client_id),
                ip: Some(ip.0),
                user_agent: ua.0,
            },
            session_id: client.session.id,
            token: q.token,
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// graphs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RangeQuery {
    #[serde(default)]
    range: Option<String>,
}

async fn graphs(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    Query(q): Query<RangeQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let l = load(&state, &tenant, &client, &access, &id, &mut ctx, "graphs").await?;
    let range = vm_charts::parse_range(q.range.as_deref());
    let mut note = String::new();
    let mut charts = Vec::new();
    if l.features.metrics_history
        && let Some(h) = soft(
            &ctx,
            state
                .billing
                .vm_metrics_history(tenant.id, l.s.id, Some(client.client_id), range)
                .await,
            &mut note,
        )?
    {
        charts = vm_charts::charts(&h, &chart_labels(&ctx), |t| ctx.datetime(&t));
    }
    let bandwidth = if l.features.bandwidth {
        soft(
            &ctx,
            state
                .billing
                .vm_bandwidth(tenant.id, l.s.id, Some(client.client_id))
                .await,
            &mut note,
        )?
        .map(|b| vm_charts::bandwidth_bar(&b))
    } else {
        None
    };
    let mut p = page(l, ctx, "graphs");
    p.range = range.as_str().to_owned();
    p.charts = charts;
    p.bandwidth = bandwidth;
    p.graphs_note = note;
    Ok(no_store(render(&p)?))
}

/// Chart labels in the page's language.
pub fn chart_labels(ctx: &PageCtx) -> vm_charts::Labels {
    vm_charts::Labels {
        cpu: ctx.t("client.vm.cpu"),
        memory: ctx.t("client.vm.memory"),
        disk: ctx.t("client.vm.disk_io"),
        network: ctx.t("client.vm.traffic"),
        read: ctx.t("client.vm.read"),
        write: ctx.t("client.vm.write"),
        inbound: ctx.t("client.vm.net_in"),
        outbound: ctx.t("client.vm.net_out"),
    }
}

// ---------------------------------------------------------------------------
// snapshots and backups
// ---------------------------------------------------------------------------

async fn backups(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let l = load(&state, &tenant, &client, &access, &id, &mut ctx, "backups").await?;
    let mut notice = l.notice.clone();
    let snapshots = soft(
        &ctx,
        state
            .billing
            .vm_snapshots(tenant.id, l.s.id, Some(client.client_id))
            .await,
        &mut notice,
    )?;
    let quota = state
        .billing
        .vm_snapshot_quota(tenant.id, l.s.id, Some(client.client_id))
        .await
        .unwrap_or(0);
    let backups = if l.features.backups {
        soft(
            &ctx,
            state
                .billing
                .vm_backups(tenant.id, l.s.id, Some(client.client_id))
                .await,
            &mut notice,
        )?
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut p = page(l, ctx, "backups");
    p.notice = notice;
    p.snapshots_supported = snapshots.is_some();
    p.snapshots = snapshots.unwrap_or_default();
    p.snapshot_quota = quota;
    p.backups = backups;
    Ok(no_store(render(&p)?))
}

#[derive(Debug, Deserialize)]
struct RestoreForm {
    backup: String,
    #[serde(default)]
    confirm_hostname: String,
}

#[allow(clippy::too_many_arguments)]
async fn backup_restore(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<RestoreForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let to = tab_url(&ctx, &s, "backups");
    check_rate(&state.limits.vm_action, &action_key(&client, &s))?;
    let vm = hos_db::repo::services::vm_for_service(&state.pool, tenant.id, s.id).await?;
    if !f
        .confirm_hostname
        .trim()
        .eq_ignore_ascii_case(&hostname(&s, vm.as_ref()))
    {
        return Ok(flash::redirect(
            &to,
            SetFlash::error(ctx.t("client.vm.reinstall_confirm_mismatch")),
        ));
    }
    let res = state
        .billing
        .vm_restore_backup(tenant.id, s.id, Some(client.client_id), &f.backup)
        .await;
    let ok = ctx.t("client.vm.backup_restore_started");
    finish(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        to,
        "client.vm.backup_restore",
        &s,
        res,
        ok,
    )
    .await
}

// ---------------------------------------------------------------------------
// network
// ---------------------------------------------------------------------------

async fn network(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let l = load(&state, &tenant, &client, &access, &id, &mut ctx, "network").await?;
    let mut notice = l.notice.clone();
    let ptrs = soft(
        &ctx,
        state
            .billing
            .vm_reverse_dns(tenant.id, l.s.id, Some(client.client_id))
            .await,
        &mut notice,
    )?
    .unwrap_or_default();
    let firewall = if l.features.firewall {
        soft(
            &ctx,
            state
                .billing
                .vm_firewall(tenant.id, l.s.id, Some(client.client_id))
                .await,
            &mut notice,
        )?
    } else {
        None
    };
    let fw = match &firewall {
        Some(f) if f.rules.is_empty() => fw_rows(
            &FirewallState {
                enabled: f.enabled,
                rules: default_firewall_rules(),
            },
            2,
        ),
        Some(f) => fw_rows(f, 3),
        None => Vec::new(),
    };
    let mut p = page(l, ctx, "network");
    p.notice = notice;
    p.ptrs = ptrs;
    p.firewall = firewall;
    p.fw = fw;
    Ok(no_store(render(&p)?))
}

#[derive(Debug, Deserialize)]
struct PtrForm {
    ip: String,
    #[serde(default)]
    ptr: String,
}

#[allow(clippy::too_many_arguments)]
async fn rdns_save(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<PtrForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let to = tab_url(&ctx, &s, "network");
    check_rate(&state.limits.vm_action, &action_key(&client, &s))?;
    let Ok(addr) = f.ip.trim().parse() else {
        return Ok(flash::redirect(
            &to,
            SetFlash::error(ctx.t("client.errors.not_found")),
        ));
    };
    let ptr = f.ptr.trim();
    let res = state
        .billing
        .vm_set_reverse_dns(
            tenant.id,
            s.id,
            Some(client.client_id),
            addr,
            (!ptr.is_empty()).then_some(ptr),
        )
        .await;
    let ok = ctx.t("client.vm.rdns_saved");
    finish(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        to,
        "client.vm.rdns",
        &s,
        res,
        ok,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn firewall_save(
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
    let to = tab_url(&ctx, &s, "network");
    check_rate(&state.limits.vm_action, &action_key(&client, &s))?;
    let fw = match parse_firewall_form(&fields) {
        Ok(f) => f,
        Err(rule) => {
            return Ok(flash::redirect(
                &to,
                SetFlash::error(ctx.tv("client.vm.firewall_invalid", "rule", rule)),
            ));
        }
    };
    let res = state
        .billing
        .vm_set_firewall(tenant.id, s.id, Some(client.client_id), &fw)
        .await;
    let ok = ctx.t("client.vm.firewall_saved");
    finish(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        to,
        "client.vm.firewall",
        &s,
        res,
        ok,
    )
    .await
}

// ---------------------------------------------------------------------------
// recovery: password, rescue, ISO, reinstall
// ---------------------------------------------------------------------------

async fn recovery(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let l = load(&state, &tenant, &client, &access, &id, &mut ctx, "recovery").await?;
    let mut notice = l.notice.clone();
    let cid = Some(client.client_id);
    let extras = soft(
        &ctx,
        state.billing.vm_extras(tenant.id, l.s.id, cid).await,
        &mut notice,
    )?
    .unwrap_or_default();
    let isos = if l.features.iso {
        soft(
            &ctx,
            state.billing.vm_isos(tenant.id, l.s.id, cid).await,
            &mut notice,
        )?
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    let templates = soft(
        &ctx,
        state.billing.vm_templates(tenant.id, l.s.id, cid).await,
        &mut notice,
    )?
    .unwrap_or_default();
    let keys = state
        .billing
        .client_ssh_keys(tenant.id, client.client_id)
        .await?;
    let mut p = page(l, ctx, "recovery");
    p.notice = notice;
    p.extras = extras;
    p.isos = isos;
    p.templates = templates;
    p.keys = keys;
    Ok(no_store(render(&p)?))
}

#[derive(Debug, Deserialize)]
struct PasswordForm {
    #[serde(default)]
    confirm: String,
}

/// `POST …/server/password`: resets the root password and shows it once.
#[allow(clippy::too_many_arguments)]
async fn password_reset(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<PasswordForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let to = tab_url(&ctx, &s, "recovery");
    if f.confirm != "yes" {
        return Ok(flash::redirect(
            &to,
            SetFlash::error(ctx.t("client.vm.password_confirm_needed")),
        ));
    }
    check_rate(&state.limits.vm_password, &format!("vmpw:{}", s.id))?;
    let reset = match state
        .billing
        .vm_reset_password(tenant.id, s.id, Some(client.client_id), None)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let m = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            return Ok(flash::redirect(&to, SetFlash::error(m)));
        }
    };
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.vm.password_reset",
        Some(("service", s.id.into_uuid())),
    )
    .await;
    secret_page(
        ctx,
        &client,
        &access,
        s,
        "client.vm.password_new",
        "client.vm.password_new_lead",
        reset.username,
        reset.password.expose_str().to_owned(),
        to,
    )
}

#[allow(clippy::too_many_arguments)]
fn secret_page(
    mut ctx: PageCtx,
    client: &CurrentClient,
    access: &MemberAccess,
    s: ServiceRow,
    heading: &str,
    lead: &str,
    username: String,
    password: String,
    back: String,
) -> AppResult<Response> {
    let heading = ctx.t(heading);
    let crumbs = vec![
        Crumb::link(ctx.t("client.services.title"), ctx.url("/client/services")),
        Crumb::link(ctx.t("client.vm.title"), tab_url(&ctx, &s, "overview")),
        Crumb::current(heading.clone()),
    ];
    setup(&mut ctx, client, access, heading.clone(), crumbs);
    let lead = ctx.t(lead);
    Ok(no_store(render(&SecretPage {
        ctx,
        heading,
        lead,
        username,
        password,
        back,
    })?))
}

/// `POST …/server/rescue` (`enable=1|0`, `key=<uuid>`…).
#[allow(clippy::too_many_arguments)]
async fn rescue(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let to = tab_url(&ctx, &s, "recovery");
    check_rate(&state.limits.vm_action, &action_key(&client, &s))?;
    let enable = fields.iter().any(|(k, v)| k == "enable" && v == "1");
    let keys: Vec<Uuid> = fields
        .iter()
        .filter(|(k, _)| k == "key")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    let res = match state
        .billing
        .vm_set_rescue(tenant.id, s.id, Some(client.client_id), enable, &keys)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let m = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            return Ok(flash::redirect(&to, SetFlash::error(m)));
        }
    };
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        if enable {
            "client.vm.rescue_on"
        } else {
            "client.vm.rescue_off"
        },
        Some(("service", s.id.into_uuid())),
    )
    .await;
    match (enable, res.password) {
        (true, Some(pw)) => secret_page(
            ctx,
            &client,
            &access,
            s,
            "client.vm.rescue_enabled",
            "client.vm.rescue_enabled_lead",
            "root".into(),
            pw.expose_str().to_owned(),
            to,
        ),
        (true, None) => Ok(flash::redirect(
            &to,
            SetFlash::success(ctx.t("client.vm.rescue_enabled")),
        )),
        (false, _) => Ok(flash::redirect(
            &to,
            SetFlash::success(ctx.t("client.vm.rescue_disabled")),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct IsoForm {
    iso: String,
    #[serde(default)]
    boot: String,
}

#[allow(clippy::too_many_arguments)]
async fn iso_mount(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<IsoForm>,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let to = tab_url(&ctx, &s, "recovery");
    check_rate(&state.limits.vm_action, &action_key(&client, &s))?;
    let res = state
        .billing
        .vm_mount_iso(
            tenant.id,
            s.id,
            Some(client.client_id),
            &f.iso,
            f.boot == "1",
        )
        .await;
    let ok = ctx.t("client.vm.iso_mounted");
    finish(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        to,
        "client.vm.iso_mount",
        &s,
        res,
        ok,
    )
    .await
}

async fn iso_unmount(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let to = tab_url(&ctx, &s, "recovery");
    check_rate(&state.limits.vm_action, &action_key(&client, &s))?;
    let res = state
        .billing
        .vm_unmount_iso(tenant.id, s.id, Some(client.client_id))
        .await;
    let ok = ctx.t("client.vm.iso_ejected");
    finish(
        &state,
        &tenant,
        &client,
        (ip, &ua),
        &ctx,
        to,
        "client.vm.iso_unmount",
        &s,
        res,
        ok,
    )
    .await
}

// ---------------------------------------------------------------------------
// activity
// ---------------------------------------------------------------------------

async fn activity(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let l = load(&state, &tenant, &client, &access, &id, &mut ctx, "activity").await?;
    let ops = state
        .billing
        .vm_operations(tenant.id, l.s.id, Some(client.client_id), 50)
        .await
        .unwrap_or_default();
    let busy = ops.iter().any(VmOperationRow::is_open);
    let views = op_views(&ctx, &ops, false);
    let mut p = page(l, ctx, "activity");
    p.ops = views;
    p.busy = busy;
    Ok(no_store(render(&p)?))
}

/// `GET …/activity/feed` (htmx, polled while an operation runs).
async fn activity_feed(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = own_service(&state, &tenant, &client, &id).await?;
    let ops = state
        .billing
        .vm_operations(tenant.id, s.id, Some(client.client_id), 50)
        .await
        .unwrap_or_default();
    let busy = ops.iter().any(VmOperationRow::is_open);
    Ok(no_store(render(&OpsFragment {
        feed_url: ctx.url(format!("/client/services/{}/server/activity/feed", s.id)),
        ops: op_views(&ctx, &ops, false),
        busy,
        ctx,
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn firewall_form() {
        let st = parse_firewall_form(&f(&[
            ("enabled", "1"),
            ("r0_dir", "in"),
            ("r0_proto", "tcp"),
            ("r0_port", "22"),
            ("r0_src", "10.0.0.0/8, 192.0.2.1"),
            ("r1_dir", "in"),
            ("r1_proto", "tcp"),
            ("r2_dir", "in"),
            ("r2_proto", "icmp"),
            ("r3_dir", "out"),
            ("r3_proto", "udp"),
            ("r3_port", "53"),
            ("r3_drop", "1"),
        ]))
        .unwrap();
        assert!(st.enabled);
        assert_eq!(st.rules.len(), 2);
        assert_eq!(st.rules[0].cidrs, ["10.0.0.0/8", "192.0.2.1"]);
        assert_eq!(st.rules[1].protocol, FirewallProtocol::Icmp);
        assert!(parse_firewall_form(&f(&[("r0_dir", "in"), ("r0_proto", "gre")])).is_err());
        let rows = fw_rows(&st, 3);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[4].idx, 4);
    }
}
