//! `/admin/services` — service list (status / product / server filters),
//! detail with module commands (create, suspend, unsuspend, terminate,
//! change package, change password), field edits, due dates, auto-suspend
//! override, event log, provisioning retry; VM panel (power, console,
//! snapshots, reinstall, metrics) and cancellation requests.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::domain::{IntegrationCategory, ProductType, ServiceStatus};
use hos_core::ids::{IntegrationId, ProductId, ServiceId};
use hos_core::integrations::virt::{PowerAction, SnapshotInfo, TemplateInfo, VmMetrics};
use hos_core::secret::SecretString;
use hos_db::repo::admin as arepo;
use hos_db::repo::catalog::{self, ProductRow};
use hos_db::repo::clients::{self, ClientRow};
use hos_db::repo::integrations::{self, IntegrationRow};
use hos_db::repo::invoices::InvoiceRow;
use hos_db::repo::services::{
    self, ServiceEventRow, ServiceFilter, ServiceListRow, ServiceRow, VmRow,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, SortHeader, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/services", get(list))
        .route("/admin/services/cancellations", get(cancellations))
        .route("/admin/services/{id}", get(show).post(update))
        .route("/admin/services/{id}/command", post(command))
        .route("/admin/services/{id}/password", post(change_password))
        .route("/admin/services/{id}/upgrade", post(upgrade))
        .route("/admin/services/{id}/vm", get(vm_panel))
        .route("/admin/services/{id}/vm/power", post(vm_power))
        .route("/admin/services/{id}/vm/console", post(vm_console))
        .route(
            "/admin/services/{id}/vm/snapshots",
            post(vm_snapshot_create),
        )
        .route(
            "/admin/services/{id}/vm/snapshots/action",
            post(vm_snapshot_action),
        )
        .route("/admin/services/{id}/vm/reinstall", post(vm_reinstall))
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    status: Option<String>,
    product: Option<String>,
    server: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/services/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ServiceListRow>,
    pager: Pager,
    q: String,
    status: String,
    product: String,
    server: String,
    statuses: Vec<(String, String)>,
    products: Vec<(ProductId, String)>,
    servers: Vec<IntegrationRow>,
    counts: Vec<(String, String, i64)>,
    headers: Vec<SortHeader>,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesView>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<ServiceStatus> = f.status.as_deref().and_then(|s| s.parse().ok());
    let product_id = f
        .product
        .as_deref()
        .and_then(|p| Uuid::parse_str(p).ok())
        .map(ProductId::from);
    let server = f
        .server
        .as_deref()
        .and_then(|p| Uuid::parse_str(p).ok())
        .map(IntegrationId::from);
    let product_type: Option<ProductType> = f.kind.as_deref().and_then(|k| k.parse().ok());
    let desc = q.dir == crate::extract::SortDir::Desc;
    let paged = arepo::list_services(
        &state.pool,
        tenant.id,
        &ServiceFilter {
            client_id: None,
            status,
            product_id,
            product_type,
            search: q.q.clone(),
        },
        server,
        q.sort.as_deref(),
        desc,
        q.db_page(),
    )
    .await?;
    let counts = arepo::service_status_counts(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|(s, n)| {
            (
                s.as_str().to_owned(),
                (&ctx).lbl("admin.status", s.as_str()),
                n,
            )
        })
        .collect();
    let products = super::catalog::product_choices(&state, &tenant).await?;
    let mut servers =
        integrations::list_integrations(&state.pool, tenant.id, Some(IntegrationCategory::Panel))
            .await?;
    servers.extend(
        integrations::list_integrations(&state.pool, tenant.id, Some(IntegrationCategory::Virt))
            .await?,
    );
    let title = ctx.t("admin.services.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let (qv, sv, pv, srv) = (
        q.q.clone().unwrap_or_default(),
        f.status.clone().unwrap_or_default(),
        f.product.clone().unwrap_or_default(),
        f.server.clone().unwrap_or_default(),
    );
    let keep = [
        ("q", qv.clone()),
        ("status", sv.clone()),
        ("product", pv.clone()),
        ("server", srv.clone()),
    ];
    let headers = common::sort_headers(
        &ctx,
        "/admin/services",
        &q,
        &[
            ("product", "admin.services.col_product", false),
            ("client", "admin.orders.col_client", false),
            ("amount", "admin.services.col_amount", true),
            ("due", "admin.services.col_due", false),
            ("created", "admin.clients.col_created", false),
        ],
        &keep,
    );
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/services"),
        &[
            ("q", qv.as_str()),
            ("status", sv.as_str()),
            ("product", pv.as_str()),
            ("server", srv.as_str()),
            ("sort", q.sort.as_deref().unwrap_or("")),
        ],
    );
    let statuses = common::enum_options(&ctx, ServiceStatus::ALL, |s| s.as_str(), "admin.status");
    render(&ListPage {
        rows: paged.items,
        pager,
        q: qv,
        status: sv,
        product: pv,
        server: srv,
        statuses,
        products,
        servers,
        counts,
        headers,
        ctx,
    })
}

#[derive(Template)]
#[template(path = "admin/services/cancellations.html")]
struct CancellationsPage {
    ctx: PageCtx,
    rows: Vec<ServiceListRow>,
}
crate::page!(CancellationsPage);

async fn cancellations(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = arepo::cancellation_requests(&state.pool, tenant.id).await?;
    let title = ctx.t("admin.services.cancellations");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.services.title", "/admin/services")],
    );
    render(&CancellationsPage { ctx, rows })
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/services/show.html")]
struct ShowPage {
    ctx: PageCtx,
    s: ServiceRow,
    product: ProductRow,
    client: ClientRow,
    integration: Option<IntegrationRow>,
    events: Vec<ServiceEventRow>,
    invoices: Vec<InvoiceRow>,
    has_vm: bool,
    password: String,
    is_panel: bool,
    is_virt: bool,
    upgrade_targets: Vec<(ProductId, String)>,
    config_json: String,
    custom_json: String,
}
crate::page!(ShowPage);

impl ShowPage {
    fn date_val(&self, d: &Option<chrono::NaiveDate>) -> String {
        d.map(|d| d.to_string()).unwrap_or_default()
    }
    fn ip_val(&self) -> String {
        self.s
            .dedicated_ip
            .map(|i| i.to_string())
            .unwrap_or_default()
    }
}

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<ServiceRow> {
    services::get_service(&state.pool, tenant.id, ServiceId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

fn pretty(v: &serde_json::Value) -> String {
    let empty = v.is_null()
        || v.as_object().is_some_and(serde_json::Map::is_empty)
        || v.as_array().is_some_and(Vec::is_empty);
    if empty {
        String::new()
    } else {
        serde_json::to_string_pretty(v).unwrap_or_default()
    }
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesView>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let product = catalog::get_product(&state.pool, tenant.id, s.product_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let client = clients::get_client(&state.pool, tenant.id, s.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let integration = match s.integration_id.or(product.integration_id) {
        Some(i) => integrations::get_integration(&state.pool, tenant.id, i).await?,
        None => None,
    };
    let is_panel = integration
        .as_ref()
        .is_some_and(|i| i.category == IntegrationCategory::Panel);
    let is_virt = integration
        .as_ref()
        .is_some_and(|i| i.category == IntegrationCategory::Virt);
    let events = services::service_events(&state.pool, tenant.id, s.id, 50).await?;
    let invoices = arepo::service_invoices(&state.pool, tenant.id, s.id).await?;
    let has_vm = services::vm_for_service(&state.pool, tenant.id, s.id)
        .await?
        .is_some();
    // Only staff who may manage services see stored credentials.
    let password =
        if staff.can(hos_auth::rbac::Permission::SERVICES_MANAGE) && s.password_enc.is_some() {
            match state.billing.service_password(tenant.id, s.id, None).await {
                Ok(Some(p)) => p.expose_str().to_owned(),
                _ => String::new(),
            }
        } else {
            String::new()
        };
    let upgrade_targets = super::catalog::product_choices(&state, &tenant)
        .await?
        .into_iter()
        .filter(|(pid, _)| *pid != s.product_id)
        .collect();
    let title = format!("{} · {}", product.name, client.display_name());
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.services.title", "/admin/services")],
    );
    render(&ShowPage {
        config_json: pretty(&s.config_selections),
        custom_json: pretty(&s.custom_fields),
        s,
        product,
        client,
        integration,
        events,
        invoices,
        has_vm,
        password,
        is_panel,
        is_virt,
        upgrade_targets,
        ctx,
    })
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let back = format!("/admin/services/{}", s.id);
    let amount = match f.decimal("recurring_amount") {
        Ok(Some(a)) if a >= Decimal::ZERO => a,
        _ => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount"))),
    };
    let (Ok(next_due), Ok(next_invoice), Ok(override_until)) = (
        f.date("next_due_date"),
        f.date("next_invoice_date"),
        f.date("override_suspend_until"),
    ) else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_date")));
    };
    let admin_notes = f.opt("admin_notes", 4000);
    services::update_service_admin(
        &state.pool,
        tenant.id,
        s.id,
        amount,
        override_until,
        admin_notes.as_deref(),
    )
    .await?;
    arepo::set_service_due_dates(&state.pool, tenant.id, s.id, next_due, next_invoice).await?;
    // Provisioning fields (username, domain/hostname, external id, IP).
    let ip = match f.opt("dedicated_ip", 64) {
        Some(v) => match v.parse() {
            Ok(ip) => Some(ip),
            Err(_) => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_ip"))),
        },
        None => None,
    };
    services::update_provisioning(
        &state.pool,
        tenant.id,
        s.id,
        &services::ProvisioningUpdate {
            username: f.opt("username", 100),
            external_id: f.opt("external_id", 200),
            server_hostname: f.opt("server_hostname", 255),
            dedicated_ip: ip,
            ..services::ProvisioningUpdate::default()
        },
    )
    .await?;
    au.log(
        &state,
        "service.update",
        Some(("service", s.id.into())),
        json!({
            "recurring_amount": amount.to_string(),
            "next_due_date": next_due,
            "override_suspend_until": override_until,
        }),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.common.saved"))
}

/// Module commands: queued through `hos-billing` (jobs) so the worker runs
/// them with retries and logs service events.
async fn command(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let back = format!("/admin/services/{}", s.id);
    let action = f.text("action", 20);
    let actor = Some(staff.user_id);
    let result = match action.as_str() {
        "create" => {
            if !matches!(s.status, ServiceStatus::Pending | ServiceStatus::Failed) {
                return Ok(failed(&ctx, &back, ctx.t("admin.services.cannot_create")));
            }
            state.billing.retry_provisioning(tenant.id, s.id).await
        }
        "suspend" => {
            let reason = f
                .opt("reason", 300)
                .unwrap_or_else(|| ctx.t("admin.services.default_suspend_reason"));
            state
                .billing
                .request_suspend(tenant.id, s.id, &reason, actor)
                .await
        }
        "unsuspend" => {
            state
                .billing
                .request_unsuspend(tenant.id, s.id, actor)
                .await
        }
        "terminate" => {
            state
                .billing
                .request_terminate(tenant.id, s.id, actor)
                .await
        }
        _ => return Err(AppError::bad_request("action")),
    };
    if result.is_ok() {
        au.log(
            &state,
            &format!("service.{action}"),
            Some(("service", s.id.into())),
            json!({"from": s.status.as_str()}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.services.queued")
}

async fn change_password(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let back = format!("/admin/services/{}", s.id);
    let pw = f.raw("password").to_owned();
    let result = state
        .billing
        .change_service_password(
            tenant.id,
            s.id,
            None,
            &SecretString::new(pw),
            Some(staff.user_id),
        )
        .await;
    if result.is_ok() {
        au.log(
            &state,
            "service.password",
            Some(("service", s.id.into())),
            json!({}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.services.password_changed")
}

async fn upgrade(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let back = format!("/admin/services/{}", s.id);
    let Some(to) = f.id::<ProductId>("product_id") else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.required")));
    };
    let result = state
        .billing
        .order_plan_change(tenant.id, s.id, None, to)
        .await;
    match result {
        Ok(order) => {
            au.log(
                &state,
                "service.change_package",
                Some(("service", s.id.into())),
                json!({"to": to, "upgrade": order.upgrade_id, "invoice": order.invoice_id}),
            )
            .await?;
            match order.invoice_id {
                Some(inv) => Ok(done(
                    &ctx,
                    &format!("/admin/invoices/{inv}"),
                    "admin.services.upgrade_invoiced",
                )),
                None => Ok(done(&ctx, &back, "admin.services.upgrade_queued")),
            }
        }
        Err(e) => Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
}

// ---------------------------------------------------------------------------
// VM
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/services/vm.html")]
struct VmPage {
    ctx: PageCtx,
    s: ServiceRow,
    vm: Option<VmRow>,
    live_status: String,
    error: String,
    metrics: Option<VmMetrics>,
    snapshots: Vec<SnapshotInfo>,
    templates: Vec<TemplateInfo>,
    can_snapshot: bool,
    can_reinstall: bool,
    powers: Vec<(String, String)>,
    confirm_value: String,
}
crate::page!(VmPage);

impl VmPage {
    fn pct(&self, v: &Option<f64>) -> String {
        v.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "—".into())
    }
    fn mb(&self, used: &Option<u64>, total: &Option<u64>) -> String {
        match (used, total) {
            (Some(u), Some(t)) => format!("{u} / {t} MB"),
            (Some(u), None) => format!("{u} MB"),
            _ => "—".into(),
        }
    }
    fn gb(&self, used: &Option<f64>, total: &Option<f64>) -> String {
        match (used, total) {
            (Some(u), Some(t)) => format!("{u:.1} / {t:.1} GB"),
            (Some(u), None) => format!("{u:.1} GB"),
            _ => "—".into(),
        }
    }
    fn uptime(&self, s: &Option<u64>) -> String {
        s.map(|s| {
            format!(
                "{}d {}h {}m",
                s / 86_400,
                (s % 86_400) / 3600,
                (s % 3600) / 60
            )
        })
        .unwrap_or_else(|| "—".into())
    }
}

/// Live VM panel (htmx-loaded into the service page, also a full page).
async fn vm_panel(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesView>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let mut vm = services::vm_for_service(&state.pool, tenant.id, s.id).await?;
    let (mut live_status, mut error) = (String::new(), String::new());
    let (mut metrics, mut snapshots, mut templates) = (None, Vec::new(), Vec::new());
    let (mut can_snapshot, mut can_reinstall) = (false, false);
    match state.billing.vm_info(tenant.id, s.id, None).await {
        Ok((row, info, caps)) => {
            vm = Some(row);
            live_status = info.status.as_str().to_owned();
            can_snapshot = caps.snapshots;
            can_reinstall = caps.reinstall;
            if caps.metrics {
                metrics = state.billing.vm_metrics(tenant.id, s.id, None).await.ok();
            }
            if caps.snapshots {
                snapshots = state
                    .billing
                    .vm_snapshots(tenant.id, s.id, None)
                    .await
                    .unwrap_or_default();
            }
            if caps.reinstall {
                templates = state
                    .billing
                    .vm_templates(tenant.id, s.id, None)
                    .await
                    .unwrap_or_default();
            }
        }
        Err(e) => error = common::failure_text(&ctx, &e)?,
    }
    let title = ctx.t("admin.vm.title");
    let back = format!("/admin/services/{}", s.id);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[
            ("admin.services.title", "/admin/services"),
            ("admin.vm.service", back.as_str()),
        ],
    );
    let powers = ["start", "shutdown", "reboot", "stop"]
        .iter()
        .map(|a| ((*a).to_owned(), ctx.t(&format!("admin.vm.power_{a}"))))
        .collect();
    let confirm_value = s.domain.clone().unwrap_or_else(|| s.id.to_string());
    render(&VmPage {
        ctx,
        powers,
        confirm_value,
        s,
        vm,
        live_status,
        error,
        metrics,
        snapshots,
        templates,
        can_snapshot,
        can_reinstall,
    })
}

fn vm_back(s: &ServiceRow) -> String {
    format!("/admin/services/{}/vm", s.id)
}

async fn vm_power(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let Some(action) = f.parse::<PowerAction>("action") else {
        return Err(AppError::bad_request("action"));
    };
    let result = state.billing.vm_power(tenant.id, s.id, None, action).await;
    if result.is_ok() {
        au.log(
            &state,
            "vm.power",
            Some(("service", s.id.into())),
            json!({"action": action.as_str()}),
        )
        .await?;
    }
    common::outcome(&ctx, &vm_back(&s), result, "admin.vm.power_sent")
}

#[derive(Template)]
#[template(path = "admin/services/console.html")]
struct ConsolePage {
    ctx: PageCtx,
    s: ServiceRow,
    kind: String,
    url: String,
    ticket: String,
    password: String,
    port: String,
    expires: String,
}
crate::page!(ConsolePage);

async fn vm_console(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let access = match state.billing.vm_console(tenant.id, s.id, None).await {
        Ok(a) => a,
        Err(e) => return Ok(failed(&ctx, &vm_back(&s), common::failure_text(&ctx, &e)?)),
    };
    au.log(
        &state,
        "vm.console",
        Some(("service", s.id.into())),
        json!({"kind": access.kind.as_str()}),
    )
    .await?;
    let title = ctx.t("admin.vm.console");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.services.title", "/admin/services")],
    );
    let expires = (&ctx).dtopt(&access.expires_at);
    render(&ConsolePage {
        kind: access.kind.as_str().to_owned(),
        url: access.url.clone(),
        ticket: access
            .ticket
            .as_ref()
            .map(|t| t.expose_str().to_owned())
            .unwrap_or_default(),
        password: access
            .password
            .as_ref()
            .map(|t| t.expose_str().to_owned())
            .unwrap_or_default(),
        port: access.port.map(|p| p.to_string()).unwrap_or_default(),
        expires,
        s,
        ctx,
    })
}

async fn vm_snapshot_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let name = f.text("name", 40);
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Ok(failed(
            &ctx,
            &vm_back(&s),
            ctx.t("admin.vm.snapshot_name_invalid"),
        ));
    }
    let result = state
        .billing
        .vm_create_snapshot(tenant.id, s.id, None, &name)
        .await;
    if result.is_ok() {
        au.log(
            &state,
            "vm.snapshot_create",
            Some(("service", s.id.into())),
            json!({"name": name}),
        )
        .await?;
    }
    common::outcome(&ctx, &vm_back(&s), result, "admin.vm.snapshot_created")
}

async fn vm_snapshot_action(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let snap = f.text("snapshot", 200);
    let action = f.text("action", 20);
    let result = match action.as_str() {
        "rollback" => {
            state
                .billing
                .vm_rollback_snapshot(tenant.id, s.id, None, &snap)
                .await
        }
        "delete" => {
            state
                .billing
                .vm_delete_snapshot(tenant.id, s.id, None, &snap)
                .await
        }
        _ => return Err(AppError::bad_request("action")),
    };
    if result.is_ok() {
        au.log(
            &state,
            &format!("vm.snapshot_{action}"),
            Some(("service", s.id.into())),
            json!({"snapshot": snap}),
        )
        .await?;
    }
    common::outcome(&ctx, &vm_back(&s), result, "admin.vm.snapshot_done")
}

async fn vm_reinstall(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let s = load(&state, &tenant, id).await?;
    let template = f.text("template", 200);
    if template.is_empty() {
        return Ok(failed(&ctx, &vm_back(&s), ctx.t("admin.common.required")));
    }
    if f.text("confirm", 300) != s.domain.clone().unwrap_or_else(|| s.id.to_string()) {
        return Ok(failed(
            &ctx,
            &vm_back(&s),
            ctx.t("admin.vm.reinstall_confirm_mismatch"),
        ));
    }
    let keys: Vec<String> = f
        .body("ssh_keys", 8000)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let result = state
        .billing
        .vm_reinstall(tenant.id, s.id, None, &template, keys)
        .await;
    if result.is_ok() {
        au.log(
            &state,
            "vm.reinstall",
            Some(("service", s.id.into())),
            json!({"template": template}),
        )
        .await?;
    }
    common::outcome(&ctx, &vm_back(&s), result, "admin.vm.reinstall_started")
}
