//! Roles (`/admin/staff/roles*`, `staff.manage`): built-in roles are shown
//! read-only; custom roles get CRUD with the area × action permission matrix.
//!
//! A role may only contain permissions the editor holds ([`super::covers`]),
//! both before and after the change, so editing a role is never a way to
//! hand out (or strip) access the editor does not have.

use std::collections::BTreeSet;

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_auth::rbac::{Permission, PermissionSet};
use hos_core::ids::RoleId;
use hos_db::repo::admin_settings as srepo;
use hos_db::repo::identity::{self, RoleRow};
use serde_json::json;

use super::super::common::{self, Audit, FormData, done, failed};
use super::{covers, is_owner_role, role_label, tabs};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::forms::FormErrors;
use crate::markdown::slugify;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render, render_status};

const BACK: &str = "/admin/staff/roles";
const ACTIONS: [&str; 2] = ["view", "manage"];

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(BACK, get(list))
        .route("/admin/staff/roles/new", get(new_form).post(create))
        .route("/admin/staff/roles/{id}", get(edit_form).post(update))
        .route("/admin/staff/roles/{id}/delete", post(delete))
}

/// `"Billing: manage"` style label of a catalog permission.
pub fn permission_label(ctx: &PageCtx, p: Permission) -> String {
    format!(
        "{}: {}",
        ctx.t(&format!("admin_settings.roles.areas.{}", p.area())),
        ctx.t(&format!("admin_settings.roles.actions.{}", p.action()))
    )
}

/// One matrix cell (`None` = the catalog has no such permission).
#[derive(Debug, Clone)]
pub struct Cell {
    pub value: String,
    pub checked: bool,
    /// The editor does not hold this permission.
    pub locked: bool,
}

/// One matrix row (an area).
#[derive(Debug, Clone)]
pub struct MatrixRow {
    pub area: String,
    pub label: String,
    pub cells: Vec<Option<Cell>>,
}

/// The area × action grid for `grants`, cells locked where `actor` lacks
/// the permission.
fn matrix(ctx: &PageCtx, grants: &PermissionSet, actor: &PermissionSet) -> Vec<MatrixRow> {
    Permission::areas()
        .into_iter()
        .map(|area| MatrixRow {
            area: area.to_owned(),
            label: ctx.t(&format!("admin_settings.roles.areas.{area}")),
            cells: ACTIONS
                .iter()
                .map(|action| {
                    Permission::parse(&format!("{area}.{action}")).map(|p| Cell {
                        value: p.as_str().to_owned(),
                        checked: grants.allows(p),
                        locked: !actor.allows(p),
                    })
                })
                .collect(),
        })
        .collect()
}

/// One row of the roles list.
#[derive(Debug, Clone)]
pub struct RoleView {
    pub role: RoleRow,
    pub label: String,
    pub members: i64,
    pub summary: String,
    pub owner: bool,
}

#[derive(Template)]
#[template(path = "admin/staff/roles.html")]
struct ListPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<RoleView>,
}
crate::page!(ListPage);

fn summary(ctx: &PageCtx, grants: &[String]) -> String {
    if grants.iter().any(|g| g == "*") {
        return ctx.t("admin_settings.roles.full_access");
    }
    let set = PermissionSet::from_grants(grants.iter().cloned());
    let n = set.effective().len();
    ctx.tv2(
        "admin_settings.roles.summary",
        "count",
        n,
        "total",
        Permission::ALL.len(),
    )
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let roles = identity::list_roles(&state.pool, tenant.id).await?;
    let mut rows = Vec::with_capacity(roles.len());
    for r in roles {
        let grants = identity::role_permissions(&state.pool, tenant.id, r.id).await?;
        rows.push(RoleView {
            label: role_label(&ctx, &r),
            members: srepo::role_member_count(&state.pool, tenant.id, r.id).await?,
            summary: summary(&ctx, &grants),
            owner: is_owner_role(&r),
            role: r,
        });
    }
    let title = ctx.t("admin_settings.roles.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_settings.staff.title", "/admin/staff")],
    );
    render(&ListPage {
        tabs: tabs(&ctx),
        active: ctx.url(BACK),
        rows,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// form
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/staff/role_form.html")]
struct FormPage {
    ctx: PageCtx,
    role: Option<RoleRow>,
    readonly: bool,
    role_name: String,
    description: String,
    matrix: Vec<MatrixRow>,
    actions: Vec<String>,
    extra_grants: Vec<String>,
    members: i64,
    errors: FormErrors,
}
crate::page!(FormPage);

impl FormPage {
    fn action_url(&self) -> String {
        match &self.role {
            Some(r) => format!("{BACK}/{}", r.id),
            None => format!("{BACK}/new"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn form_page(
    mut ctx: PageCtx,
    staff: &CurrentStaff,
    role: Option<RoleRow>,
    name: String,
    description: String,
    grants: &[String],
    members: i64,
    errors: FormErrors,
) -> AppResult<Response> {
    let readonly = role.as_ref().is_some_and(|r| r.is_system);
    let set = PermissionSet::from_grants(grants.iter().cloned());
    let title = match &role {
        Some(r) => role_label(&ctx, r),
        None => ctx.t("admin_settings.roles.new"),
    };
    // Wildcards / unknown grants the matrix cannot show (built-in roles).
    let extra_grants = grants
        .iter()
        .filter(|g| g.as_str() == "*" || g.ends_with(".*"))
        .cloned()
        .collect();
    let matrix = matrix(&ctx, &set, &staff.permissions);
    let actions = ACTIONS
        .iter()
        .map(|a| ctx.t(&format!("admin_settings.roles.actions.{a}")))
        .collect();
    common::page(
        &mut ctx,
        staff,
        &title,
        &[
            ("admin_settings.staff.title", "/admin/staff"),
            ("admin_settings.roles.title", BACK),
        ],
    );
    let page = FormPage {
        ctx,
        role,
        readonly,
        role_name: name,
        description,
        matrix,
        actions,
        extra_grants,
        members,
        errors,
    };
    if page.errors.any() {
        render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)
    } else {
        render(&page)
    }
}

async fn load(state: &AppState, tenant: &Tenant, id: &str) -> AppResult<RoleRow> {
    let id: RoleId = id.parse().map_err(|_| AppError::NotFound)?;
    srepo::get_role(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn new_form(
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    form_page(
        ctx,
        &staff,
        None,
        String::new(),
        String::new(),
        &[],
        0,
        FormErrors::default(),
    )
}

async fn edit_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let role = load(&state, &tenant, &id).await?;
    let grants = identity::role_permissions(&state.pool, tenant.id, role.id).await?;
    let members = srepo::role_member_count(&state.pool, tenant.id, role.id).await?;
    let name = role_label(&ctx, &role);
    let description = role.description.clone().unwrap_or_default();
    form_page(
        ctx,
        &staff,
        Some(role),
        name,
        description,
        &grants,
        members,
        FormErrors::default(),
    )
}

/// Parsed + validated form: `(name, description, grants)`.
fn parse(ctx: &PageCtx, f: &FormData, errors: &mut FormErrors) -> (String, String, Vec<String>) {
    let name = f.text("name", 80);
    let description = f.text("description", 300);
    if name.chars().count() < 2 {
        errors.add("name", ctx.t("admin_settings.roles.errors.name"));
    }
    let picked: BTreeSet<String> = f
        .all("perm")
        .into_iter()
        .filter_map(|v| Permission::parse(&v).map(|p| p.as_str().to_owned()))
        .collect();
    if picked.is_empty() {
        errors.add_general(ctx.t("admin_settings.roles.errors.empty"));
    }
    (name, description, picked.into_iter().collect())
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let (name, description, grants) = parse(&ctx, &f, &mut errors);
    if !covers(&staff.permissions, &grants) {
        errors.add_general(ctx.t("admin_settings.staff.errors.escalation"));
    }
    let slug = format!("custom-{}", slugify(&name));
    if !errors.any()
        && identity::role_by_slug(&state.pool, tenant.id, &slug)
            .await?
            .is_some()
    {
        errors.add("name", ctx.t("admin_settings.roles.errors.taken"));
    }
    if errors.any() {
        return form_page(ctx, &staff, None, name, description, &grants, 0, errors);
    }
    let mut tx = state.pool.begin().await?;
    let role = identity::create_role(
        &mut *tx,
        tenant.id,
        &slug,
        &name,
        (!description.is_empty()).then_some(description.as_str()),
        false,
    )
    .await?;
    identity::set_role_permissions(&mut tx, tenant.id, role.id, &grants).await?;
    tx.commit().await?;
    au.log(
        &state,
        "role.create",
        Some(("role", role.id.into())),
        json!({"name": name, "permissions": grants}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("{BACK}/{}", role.id),
        "admin_settings.roles.created",
    ))
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let role = load(&state, &tenant, &id).await?;
    let back = format!("{BACK}/{}", role.id);
    if role.is_system {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.roles.errors.system"),
        ));
    }
    let old = identity::role_permissions(&state.pool, tenant.id, role.id).await?;
    let members = srepo::role_member_count(&state.pool, tenant.id, role.id).await?;
    let mut errors = FormErrors::default();
    let (name, description, grants) = parse(&ctx, &f, &mut errors);
    // Old and new grants: neither adding nor stripping what one lacks.
    if !covers(&staff.permissions, &grants) || !covers(&staff.permissions, &old) {
        errors.add_general(ctx.t("admin_settings.staff.errors.escalation"));
    }
    if errors.any() {
        return form_page(
            ctx,
            &staff,
            Some(role),
            name,
            description,
            &grants,
            members,
            errors,
        );
    }
    let mut tx = state.pool.begin().await?;
    srepo::update_role(
        &mut *tx,
        tenant.id,
        role.id,
        &name,
        (!description.is_empty()).then_some(description.as_str()),
    )
    .await?;
    identity::set_role_permissions(&mut tx, tenant.id, role.id, &grants).await?;
    tx.commit().await?;
    au.log(
        &state,
        "role.update",
        Some(("role", role.id.into())),
        json!({
            "name": {"from": role.name, "to": name},
            "permissions": {"from": old, "to": grants},
        }),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.common.saved"))
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let role = load(&state, &tenant, &id).await?;
    let back = format!("{BACK}/{}", role.id);
    if role.is_system {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.roles.errors.system"),
        ));
    }
    let grants = identity::role_permissions(&state.pool, tenant.id, role.id).await?;
    if !covers(&staff.permissions, &grants) {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.staff.errors.escalation"),
        ));
    }
    if srepo::role_member_count(&state.pool, tenant.id, role.id).await? > 0 {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.roles.errors.in_use"),
        ));
    }
    identity::delete_role(&state.pool, tenant.id, role.id).await?;
    au.log(
        &state,
        "role.delete",
        Some(("role", role.id.into())),
        json!({"name": role.name, "permissions": grants}),
    )
    .await?;
    Ok(done(&ctx, BACK, "admin_settings.roles.deleted"))
}
