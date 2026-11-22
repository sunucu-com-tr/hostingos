//! `/admin/status` — the network status page: components (grouped, optionally
//! bound to a server integration so affected clients can be found),
//! incidents and maintenance windows with their update timeline, and the
//! one-off e-mail to clients whose services are affected.
//!
//! Everything needs `status.manage`; every change is audited. Bodies are
//! Markdown rendered through the sanitising renderer.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_auth::rbac::Permission;
use hos_core::ids::IntegrationId;
use hos_db::repo::integrations;
use hos_db::repo::status::{
    self as srepo, ComponentInput, ComponentRow, ComponentStatus, IncidentId, IncidentImpact,
    IncidentInput, IncidentKind, IncidentRow, IncidentStatus, IncidentUpdateRow, StatusComponentId,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{PermissionMarker, RequirePermission};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

/// `status.manage`.
#[derive(Debug, Clone, Copy)]
pub struct StatusManage;
impl PermissionMarker for StatusManage {
    const PERMISSION: Permission = Permission::STATUS_MANAGE;
}

/// Longest incident update body.
const MAX_BODY: usize = 20_000;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/status", get(overview))
        .route("/admin/status/components", post(component_create))
        .route("/admin/status/components/new", get(component_new))
        .route(
            "/admin/status/components/{id}",
            get(component_edit).post(component_update),
        )
        .route(
            "/admin/status/components/{id}/status",
            post(component_status),
        )
        .route(
            "/admin/status/components/{id}/delete",
            post(component_delete),
        )
        .route("/admin/status/incidents", post(incident_create))
        .route("/admin/status/incidents/new", get(incident_new))
        .route(
            "/admin/status/incidents/{id}",
            get(incident_show).post(incident_update),
        )
        .route(
            "/admin/status/incidents/{id}/updates",
            post(incident_add_update),
        )
        .route("/admin/status/incidents/{id}/notify", post(incident_notify))
        .route("/admin/status/incidents/{id}/delete", post(incident_delete))
}

/// Badge tone of a component status.
pub fn component_tone(s: &str) -> &'static str {
    match s {
        "operational" => "success",
        "maintenance" => "info",
        "degraded" | "partial_outage" => "warning",
        "major_outage" => "danger",
        _ => "",
    }
}

/// Badge tone of an incident status.
pub fn incident_tone(s: &str) -> &'static str {
    match s {
        "resolved" | "completed" => "success",
        "scheduled" | "in_progress" | "monitoring" => "info",
        "investigating" | "identified" => "warning",
        _ => "",
    }
}

fn component_options(ctx: &PageCtx) -> Vec<(String, String)> {
    common::enum_options(
        ctx,
        ComponentStatus::ALL,
        |s| s.as_str(),
        "netstatus.component",
    )
}

// ---------------------------------------------------------------------------
// overview
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/status/overview.html")]
struct OverviewPage {
    ctx: PageCtx,
    components: Vec<ComponentRow>,
    open: Vec<IncidentRow>,
    recent: Vec<IncidentRow>,
    statuses: Vec<(String, String)>,
}
crate::page!(OverviewPage);

impl OverviewPage {
    fn cbadge(&self, s: &ComponentStatus) -> askama::filters::Safe<String> {
        self.badge(
            self.lbl("netstatus.component", s.as_str()),
            component_tone(s.as_str()),
        )
    }
    fn ibadge(&self, s: &IncidentStatus) -> askama::filters::Safe<String> {
        self.badge(
            self.lbl("netstatus.incident", s.as_str()),
            incident_tone(s.as_str()),
        )
    }
}

async fn overview(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let components = srepo::list_components(&state.pool, tenant.id, false).await?;
    let open = srepo::list_incidents(&state.pool, tenant.id, false, true, 100).await?;
    let recent: Vec<IncidentRow> = srepo::list_incidents(&state.pool, tenant.id, false, false, 30)
        .await?
        .into_iter()
        .filter(|i| i.status.is_closed())
        .collect();
    let title = ctx.t("netstatus.admin.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&OverviewPage {
        statuses: component_options(&ctx),
        components,
        open,
        recent,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// components
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/status/component.html")]
struct ComponentPage {
    ctx: PageCtx,
    id: Option<StatusComponentId>,
    c: ComponentInput,
    statuses: Vec<(String, String)>,
    integrations: Vec<(String, String)>,
    error: String,
}
crate::page!(ComponentPage);

impl ComponentPage {
    fn integration_value(&self) -> String {
        self.c
            .integration_id
            .map(|i| i.to_string())
            .unwrap_or_default()
    }
}

async fn component_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    id: Option<StatusComponentId>,
    c: ComponentInput,
    error: String,
) -> AppResult<Response> {
    let title = if id.is_some() {
        ctx.tv("netstatus.admin.component_edit", "name", &c.name)
    } else {
        ctx.t("netstatus.admin.component_new")
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("netstatus.admin.title", "/admin/status")],
    );
    let integrations = integrations::list_integrations(&state.pool, tenant.id, None)
        .await?
        .into_iter()
        .map(|i| (i.id.to_string(), format!("{} ({})", i.name, i.kind)))
        .collect();
    let page = ComponentPage {
        statuses: component_options(&ctx),
        integrations,
        id,
        c,
        error: error.clone(),
        ctx,
    };
    if error.is_empty() {
        render(&page)
    } else {
        render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)
    }
}

fn component_from(f: &FormData) -> Result<ComponentInput, &'static str> {
    let name = f.text("name", 120);
    if name.trim().is_empty() {
        return Err("netstatus.admin.name_required");
    }
    let status = f
        .parse::<ComponentStatus>("status")
        .ok_or("netstatus.admin.invalid")?;
    let integration_id = match f.text("integration_id", 40).as_str() {
        "" => None,
        raw => Some(
            raw.parse::<Uuid>()
                .map(IntegrationId::from)
                .map_err(|_| "netstatus.admin.invalid")?,
        ),
    };
    let sort_order = f
        .int("sort_order")
        .map_err(|()| "netstatus.admin.invalid")?
        .unwrap_or(0)
        .clamp(-10_000, 10_000) as i32;
    Ok(ComponentInput {
        name: name.trim().to_owned(),
        group_name: f.text("group_name", 120).trim().to_owned(),
        description: f.opt("description", 500),
        status,
        integration_id,
        is_public: f.flag("is_public"),
        sort_order,
    })
}

async fn component_new(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = ComponentInput {
        name: String::new(),
        group_name: String::new(),
        description: None,
        status: ComponentStatus::Operational,
        integration_id: None,
        is_public: true,
        sort_order: 0,
    };
    component_page(&state, &tenant, &staff, ctx, None, c, String::new()).await
}

async fn component_save(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    au: &Audit,
    ctx: PageCtx,
    id: Option<StatusComponentId>,
    f: &FormData,
) -> AppResult<Response> {
    let c = match component_from(f) {
        Ok(c) => c,
        Err(key) => {
            let draft = ComponentInput {
                name: f.text("name", 120),
                group_name: f.text("group_name", 120),
                description: f.opt("description", 500),
                status: f.parse("status").unwrap_or(ComponentStatus::Operational),
                integration_id: None,
                is_public: f.flag("is_public"),
                sort_order: 0,
            };
            let msg = ctx.t(key);
            return component_page(state, tenant, staff, ctx, id, draft, msg).await;
        }
    };
    if let Some(iid) = c.integration_id
        && integrations::get_integration(&state.pool, tenant.id, iid)
            .await?
            .is_none()
    {
        return Err(AppError::NotFound);
    }
    let row = srepo::save_component(&state.pool, tenant.id, id, &c).await?;
    au.log(
        state,
        if id.is_some() {
            "status.component_update"
        } else {
            "status.component_create"
        },
        Some(("status_component", row.id.into_uuid())),
        json!({"name": row.name, "status": row.status, "public": row.is_public}),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/status",
        "netstatus.admin.component_saved",
    ))
}

async fn component_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    component_save(&state, &tenant, &staff, &au, ctx, None, &f).await
}

async fn component_edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = srepo::get_component(&state.pool, tenant.id, StatusComponentId::from(id))
        .await?
        .ok_or(AppError::NotFound)?;
    let c = ComponentInput {
        name: row.name,
        group_name: row.group_name,
        description: row.description,
        status: row.status,
        integration_id: row.integration_id,
        is_public: row.is_public,
        sort_order: row.sort_order,
    };
    component_page(&state, &tenant, &staff, ctx, Some(row.id), c, String::new()).await
}

async fn component_update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = StatusComponentId::from(id);
    srepo::get_component(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    component_save(&state, &tenant, &staff, &au, ctx, Some(id), &f).await
}

async fn component_status(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<StatusManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = StatusComponentId::from(id);
    let Some(status) = f.parse::<ComponentStatus>("status") else {
        return Ok(failed(
            &ctx,
            "/admin/status",
            ctx.t("netstatus.admin.invalid"),
        ));
    };
    if !srepo::set_component_status(&state.pool, tenant.id, id, status).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "status.component_status",
        Some(("status_component", id.into_uuid())),
        json!({"status": status}),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/status",
        "netstatus.admin.component_saved",
    ))
}

async fn component_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<StatusManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = StatusComponentId::from(id);
    if !srepo::delete_component(&state.pool, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "status.component_delete",
        Some(("status_component", id.into_uuid())),
        json!({}),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/status",
        "netstatus.admin.component_deleted",
    ))
}

// ---------------------------------------------------------------------------
// incidents
// ---------------------------------------------------------------------------

/// Editable incident fields as strings (form round trip).
#[derive(Debug, Clone, Default)]
struct IncidentForm {
    kind: String,
    title: String,
    impact: String,
    status: String,
    scheduled_start: String,
    scheduled_end: String,
    is_public: bool,
    components: Vec<String>,
    message: String,
}

#[derive(Template)]
#[template(path = "admin/status/incident.html")]
struct IncidentPage {
    ctx: PageCtx,
    id: Option<IncidentId>,
    incident: Option<IncidentRow>,
    form: IncidentForm,
    components: Vec<ComponentRow>,
    updates: Vec<IncidentUpdateRow>,
    impacts: Vec<(String, String)>,
    incident_statuses: Vec<(String, String)>,
    maintenance_statuses: Vec<(String, String)>,
    error: String,
}
crate::page!(IncidentPage);

impl IncidentPage {
    fn checked_component(&self, id: &StatusComponentId) -> bool {
        let id = id.to_string();
        self.form.components.contains(&id)
    }
    fn ibadge(&self, s: &IncidentStatus) -> askama::filters::Safe<String> {
        self.badge(
            self.lbl("netstatus.incident", s.as_str()),
            incident_tone(s.as_str()),
        )
    }
    fn statuses_for_kind(&self) -> &[(String, String)] {
        if self.form.kind == "maintenance" {
            &self.maintenance_statuses
        } else {
            &self.incident_statuses
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct NewQuery {
    #[serde(default)]
    kind: String,
}

#[allow(clippy::too_many_arguments)]
async fn incident_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    incident: Option<IncidentRow>,
    form: IncidentForm,
    error: String,
) -> AppResult<Response> {
    let id = incident.as_ref().map(|i| i.id);
    let title = match &incident {
        Some(i) => i.title.clone(),
        None if form.kind == "maintenance" => ctx.t("netstatus.admin.maintenance_new"),
        None => ctx.t("netstatus.admin.incident_new"),
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("netstatus.admin.title", "/admin/status")],
    );
    let components = srepo::list_components(&state.pool, tenant.id, false).await?;
    let updates = match id {
        Some(id) => srepo::incident_updates(&state.pool, tenant.id, id).await?,
        None => Vec::new(),
    };
    let opts = |list: &[IncidentStatus]| {
        list.iter()
            .map(|s| {
                (
                    s.as_str().to_owned(),
                    ctx.t(&format!("netstatus.incident.{}", s.as_str())),
                )
            })
            .collect::<Vec<_>>()
    };
    let page = IncidentPage {
        impacts: common::enum_options(
            &ctx,
            IncidentImpact::ALL,
            |s| s.as_str(),
            "netstatus.impact",
        ),
        incident_statuses: opts(IncidentStatus::INCIDENT),
        maintenance_statuses: opts(IncidentStatus::MAINTENANCE),
        id,
        incident,
        form,
        components,
        updates,
        error: error.clone(),
        ctx,
    };
    if error.is_empty() {
        render(&page)
    } else {
        render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)
    }
}

fn incident_form_of(f: &FormData) -> IncidentForm {
    IncidentForm {
        kind: f.text("kind", 20),
        title: f.text("title", 200),
        impact: f.text("impact", 20),
        status: f.text("status", 20),
        scheduled_start: f.text("scheduled_start", 20),
        scheduled_end: f.text("scheduled_end", 20),
        is_public: f.flag("is_public"),
        components: f.all("components"),
        message: f.body("message", MAX_BODY),
    }
}

fn incident_input(f: &FormData, tz: &str) -> Result<IncidentInput, &'static str> {
    let kind = f
        .parse::<IncidentKind>("kind")
        .ok_or("netstatus.admin.invalid")?;
    let title = f.text("title", 200);
    if title.trim().is_empty() {
        return Err("netstatus.admin.title_required");
    }
    let impact = f
        .parse::<IncidentImpact>("impact")
        .ok_or("netstatus.admin.invalid")?;
    let status = match f.parse::<IncidentStatus>("status") {
        Some(s) if s.fits(kind) => s,
        _ => return Err("netstatus.admin.invalid"),
    };
    let start = f
        .datetime("scheduled_start", tz)
        .map_err(|()| "netstatus.admin.invalid_date")?;
    let end = f
        .datetime("scheduled_end", tz)
        .map_err(|()| "netstatus.admin.invalid_date")?;
    if kind == IncidentKind::Maintenance && start.is_none() {
        return Err("netstatus.admin.start_required");
    }
    if let (Some(s), Some(e)) = (start, end)
        && e <= s
    {
        return Err("netstatus.admin.invalid_date");
    }
    Ok(IncidentInput {
        kind,
        title: title.trim().to_owned(),
        impact,
        status,
        scheduled_start: start,
        scheduled_end: end,
        is_public: f.flag("is_public"),
        components: f.ids::<StatusComponentId>("components"),
    })
}

async fn incident_new(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    Query(q): Query<NewQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let maintenance = q.kind == "maintenance";
    let form = IncidentForm {
        kind: if maintenance {
            "maintenance"
        } else {
            "incident"
        }
        .into(),
        impact: if maintenance { "minor" } else { "major" }.into(),
        status: if maintenance {
            "scheduled"
        } else {
            "investigating"
        }
        .into(),
        is_public: true,
        ..IncidentForm::default()
    };
    incident_page(&state, &tenant, &staff, ctx, None, form, String::new()).await
}

async fn incident_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = incident_form_of(&f);
    let input = match incident_input(&f, &tenant.timezone) {
        Ok(i) if !form.message.trim().is_empty() => i,
        Ok(_) => {
            let msg = ctx.t("netstatus.admin.message_required");
            return incident_page(&state, &tenant, &staff, ctx, None, form, msg).await;
        }
        Err(key) => {
            let msg = ctx.t(key);
            return incident_page(&state, &tenant, &staff, ctx, None, form, msg).await;
        }
    };
    let mut conn = state.pool.acquire().await?;
    let row = srepo::create_incident(
        &mut conn,
        tenant.id,
        &input,
        form.message.trim(),
        Some(staff.user_id),
        Utc::now(),
    )
    .await?;
    au.log(
        &state,
        "status.incident_create",
        Some(("status_incident", row.id.into_uuid())),
        json!({"kind": row.kind, "title": row.title, "impact": row.impact, "public": row.is_public}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/status/incidents/{}", row.id),
        "netstatus.admin.incident_saved",
    ))
}

async fn incident_show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = IncidentId::from(id);
    let row = srepo::get_incident(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let comps = srepo::incident_components(&state.pool, tenant.id, id).await?;
    let local = |d: &Option<chrono::DateTime<Utc>>| common::local_input(d, &tenant.timezone);
    let form = IncidentForm {
        kind: row.kind.as_str().into(),
        title: row.title.clone(),
        impact: row.impact.as_str().into(),
        status: row.status.as_str().into(),
        scheduled_start: local(&row.scheduled_start),
        scheduled_end: local(&row.scheduled_end),
        is_public: row.is_public,
        components: comps.iter().map(ToString::to_string).collect(),
        message: String::new(),
    };
    incident_page(&state, &tenant, &staff, ctx, Some(row), form, String::new()).await
}

async fn incident_update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = IncidentId::from(id);
    let current = srepo::get_incident(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    // The status moves through timeline updates only; keep the current one.
    let mut fields = f.0.clone();
    fields.retain(|(k, _)| k != "status");
    fields.push(("status".into(), current.status.as_str().into()));
    let f = FormData(fields);
    let form = incident_form_of(&f);
    let input = match incident_input(&f, &tenant.timezone) {
        Ok(i) if i.kind == current.kind => i,
        Ok(_) => {
            let msg = ctx.t("netstatus.admin.invalid");
            return incident_page(&state, &tenant, &staff, ctx, Some(current), form, msg).await;
        }
        Err(key) => {
            let msg = ctx.t(key);
            return incident_page(&state, &tenant, &staff, ctx, Some(current), form, msg).await;
        }
    };
    let mut conn = state.pool.acquire().await?;
    let row = srepo::update_incident(&mut conn, tenant.id, id, &input).await?;
    au.log(
        &state,
        "status.incident_update",
        Some(("status_incident", row.id.into_uuid())),
        json!({"title": row.title, "impact": row.impact, "public": row.is_public}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/status/incidents/{id}"),
        "netstatus.admin.incident_saved",
    ))
}

async fn incident_add_update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<StatusManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = IncidentId::from(id);
    let back = format!("/admin/status/incidents/{id}");
    let current = srepo::get_incident(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let body = f.body("message", MAX_BODY);
    let status = match f.parse::<IncidentStatus>("status") {
        Some(s) if s.fits(current.kind) => s,
        _ => return Ok(failed(&ctx, &back, ctx.t("netstatus.admin.invalid"))),
    };
    if body.trim().is_empty() {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("netstatus.admin.message_required"),
        ));
    }
    let mut conn = state.pool.acquire().await?;
    let row = srepo::add_update(
        &mut conn,
        tenant.id,
        id,
        status,
        body.trim(),
        Some(staff.user_id),
        Utc::now(),
    )
    .await?;
    au.log(
        &state,
        "status.incident_progress",
        Some(("status_incident", row.id.into_uuid())),
        json!({"status": status}),
    )
    .await?;
    Ok(done(&ctx, &back, "netstatus.admin.update_added"))
}

async fn incident_notify(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<StatusManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = IncidentId::from(id);
    let back = format!("/admin/status/incidents/{id}");
    srepo::get_incident(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    match state.billing.notify_incident(tenant.id, id).await {
        Ok(n) => {
            au.log(
                &state,
                "status.incident_notify",
                Some(("status_incident", id.into_uuid())),
                json!({"clients": n}),
            )
            .await?;
            Ok(crate::flash::redirect(
                &ctx.url(&back),
                crate::flash::SetFlash::success(ctx.tn("netstatus.admin.notified", n)),
            ))
        }
        Err(e) => Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
}

async fn incident_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<StatusManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = IncidentId::from(id);
    if !srepo::delete_incident(&state.pool, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "status.incident_delete",
        Some(("status_incident", id.into_uuid())),
        json!({}),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/status",
        "netstatus.admin.incident_deleted",
    ))
}
