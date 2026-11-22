//! Staff and roles (`/admin/staff*`, `staff.manage`).
//!
//! * Staff list + e-mail invitations (`staff_invites`, purpose-bound one-time
//!   token, `staff_invite` e-mail → `/auth/invite`), plan limit `Staff`.
//! * Per member: roles, deactivate / reactivate (all sessions end), 2FA reset,
//!   last login.
//! * Roles ([`roles`]): built-in roles (read-only) + custom role CRUD with
//!   the area × action permission matrix.
//! * Notifications bell of the admin top bar ([`notifications`]).
//!
//! Guards (all enforced here, not only in the UI):
//! * nobody grants or revokes what they do not hold themselves
//!   ([`covers`]: `*` only by holders of `*`, `area.*` only by holders of
//!   `area.*`/`*`, a permission only when allowed);
//! * nobody raises their own permissions;
//! * the last active owner can neither lose the owner role nor be deactivated;
//! * members with more permissions than the actor cannot be deactivated or
//!   have their 2FA reset by them.

pub mod notifications;
pub mod roles;

use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_auth::rbac::{Permission, PermissionSet};
use hos_auth::tokens::{TokenPurpose, issue_token};
use hos_core::domain::{Locale, UserKind};
use hos_core::ids::{RoleId, UserId};
use hos_db::enums::AccountStatus;
use hos_db::repo::admin_settings::{self as srepo, NewStaffInvite, StaffInviteRow};
use hos_db::repo::identity::{self, RoleRow, UserRow};
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use super::settings::has_room;
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::forms;
use crate::mail::{Recipient, send_email};
use crate::routes::platform::limits::Limit;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render};

const BACK: &str = "/admin/staff";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/staff", get(list))
        .route("/admin/staff/invite", post(invite))
        .route("/admin/staff/invites/{id}/revoke", post(revoke_invite))
        .route("/admin/staff/invites/{id}/resend", post(resend_invite))
        .route("/admin/staff/{id}", get(show))
        .route("/admin/staff/{id}/roles", post(set_roles))
        .route("/admin/staff/{id}/deactivate", post(deactivate))
        .route("/admin/staff/{id}/activate", post(activate))
        .route("/admin/staff/{id}/reset-2fa", post(reset_2fa))
        .merge(roles::routes())
        .merge(notifications::routes())
}

// ---------------------------------------------------------------------------
// permission guards
// ---------------------------------------------------------------------------

/// Whether `actor` may hand out (or take away) `grant`.
pub fn covers_grant(actor: &PermissionSet, grant: &str) -> bool {
    let has = |g: &str| actor.grants().any(|x| x == g);
    match grant {
        "*" => has("*"),
        g if g.ends_with(".*") => has("*") || has(g),
        g => Permission::parse(g).is_some_and(|p| actor.allows(p)),
    }
}

/// Whether `actor` may hand out every one of `grants` (no escalation).
pub fn covers<S: AsRef<str>>(actor: &PermissionSet, grants: &[S]) -> bool {
    grants.iter().all(|g| covers_grant(actor, g.as_ref()))
}

/// `next` allows something `current` does not (catalog permissions, plus
/// the bare `*` wildcard which also covers future permissions).
pub fn escalates(current: &PermissionSet, next: &PermissionSet) -> bool {
    let star = |s: &PermissionSet| s.grants().any(|g| g == "*");
    (star(next) && !star(current))
        || Permission::ALL
            .iter()
            .any(|p| next.allows(*p) && !current.allows(*p))
}

/// The built-in owner role.
pub fn is_owner_role(r: &RoleRow) -> bool {
    r.is_system && r.slug == "owner"
}

/// Grants of every role of the tenant.
pub(crate) async fn role_grants(
    state: &AppState,
    tenant: &Tenant,
    roles: &[RoleRow],
) -> AppResult<BTreeMap<RoleId, Vec<String>>> {
    let mut out = BTreeMap::new();
    for r in roles {
        out.insert(
            r.id,
            identity::role_permissions(&state.pool, tenant.id, r.id).await?,
        );
    }
    Ok(out)
}

fn union(grants: &BTreeMap<RoleId, Vec<String>>, ids: &BTreeSet<RoleId>) -> PermissionSet {
    PermissionSet::from_grants(
        ids.iter()
            .filter_map(|id| grants.get(id))
            .flat_map(|g| g.iter().cloned()),
    )
}

async fn load_staff(state: &AppState, tenant: &Tenant, id: &str) -> AppResult<UserRow> {
    let id: UserId = id.parse().map_err(|_| AppError::NotFound)?;
    identity::get_user(&state.pool, tenant.id, id)
        .await?
        .filter(|u| u.kind == UserKind::Staff)
        .ok_or(AppError::NotFound)
}

/// Active members holding the owner role.
async fn active_owners(state: &AppState, tenant: &Tenant, roles: &[RoleRow]) -> AppResult<i64> {
    let mut n = 0;
    for r in roles.iter().filter(|r| is_owner_role(r)) {
        n += srepo::active_role_members(&state.pool, tenant.id, r.id).await?;
    }
    Ok(n)
}

/// The actor's grants (the roles they hold now).
fn actor_set(staff: &CurrentStaff) -> &PermissionSet {
    &staff.permissions
}

// ---------------------------------------------------------------------------
// list + invite
// ---------------------------------------------------------------------------

/// One staff row.
#[derive(Debug, Clone)]
pub struct StaffRowView {
    pub user: UserRow,
    pub roles: Vec<String>,
    pub owner: bool,
    pub is_me: bool,
}

/// One open invitation.
#[derive(Debug, Clone)]
pub struct InviteView {
    pub invite: StaffInviteRow,
    pub role: String,
}

#[derive(Template)]
#[template(path = "admin/staff/list.html")]
struct ListPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<StaffRowView>,
    invites: Vec<InviteView>,
    roles: Vec<(String, String)>,
    can_invite: bool,
    limit_text: String,
}
crate::page!(ListPage);

/// Staff / roles tab bar.
pub(crate) fn tabs(ctx: &PageCtx) -> Vec<NavItem> {
    common::tabs(
        ctx,
        &[
            ("admin_settings.staff.tab_members", BACK.into()),
            (
                "admin_settings.staff.tab_roles",
                "/admin/staff/roles".into(),
            ),
        ],
    )
}

/// Display name of a role (built-in roles translated).
pub(crate) fn role_label(ctx: &PageCtx, r: &RoleRow) -> String {
    let key = format!("admin_settings.roles.builtin.{}", r.slug);
    if r.is_system && ctx.cat.has(&key) {
        ctx.t(&key)
    } else {
        r.name.clone()
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let users = identity::list_staff(&state.pool, tenant.id).await?;
    let roles = identity::list_roles(&state.pool, tenant.id).await?;
    let pairs = srepo::staff_role_pairs(&state.pool, tenant.id).await?;
    let by_id: BTreeMap<RoleId, &RoleRow> = roles.iter().map(|r| (r.id, r)).collect();
    let rows = users
        .into_iter()
        .map(|u| {
            let mine: Vec<&RoleRow> = pairs
                .iter()
                .filter(|(uid, _)| *uid == u.id)
                .filter_map(|(_, rid)| by_id.get(rid).copied())
                .collect();
            StaffRowView {
                roles: mine.iter().map(|r| role_label(&ctx, r)).collect(),
                owner: mine.iter().any(|r| is_owner_role(r)),
                is_me: u.id == staff.user_id,
                user: u,
            }
        })
        .collect();
    let invites = srepo::open_staff_invites(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|i| InviteView {
            role: by_id
                .get(&i.role_id)
                .map(|r| role_label(&ctx, r))
                .unwrap_or_default(),
            invite: i,
        })
        .collect();
    let grants = role_grants(&state, &tenant, &roles).await?;
    let role_opts = roles
        .iter()
        .filter(|r| {
            grants
                .get(&r.id)
                .is_some_and(|g| covers(actor_set(&staff), g))
        })
        .map(|r| (r.id.to_string(), role_label(&ctx, r)))
        .collect();
    let status =
        crate::routes::platform::limits::plan_limit_status(&state, tenant.id, Limit::Staff).await?;
    let limit_text = match status.max {
        Some(m) => ctx.tv2("admin_settings.staff.usage", "used", status.used, "max", m),
        None => ctx.tv("admin_settings.staff.usage_unlimited", "used", status.used),
    };
    let title = ctx.t("admin_settings.staff.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&ListPage {
        tabs: tabs(&ctx),
        active: ctx.url(BACK),
        rows,
        invites,
        roles: role_opts,
        can_invite: status.allows(1),
        limit_text,
        ctx,
    })
}

/// Sends (or re-sends) an invitation e-mail with a fresh token.
#[allow(clippy::too_many_arguments)]
async fn send_invite(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    staff: &CurrentStaff,
    email: &str,
    first_name: &str,
    last_name: &str,
    role: &RoleRow,
    locale: Locale,
) -> AppResult<StaffInviteRow> {
    let issued = issue_token(TokenPurpose::Invite, Utc::now());
    let row = srepo::create_staff_invite(
        &state.pool,
        tenant.id,
        &NewStaffInvite {
            email: email.to_owned(),
            first_name: first_name.to_owned(),
            last_name: last_name.to_owned(),
            role_id: role.id,
            token_hash: issued.token_hash.clone(),
            invited_by: Some(staff.user_id),
            locale,
            expires_at: issued.expires_at,
        },
    )
    .await?;
    let link = format!(
        "{}{}?token={}",
        ctx.origin,
        ctx.url_in("/auth/invite", &locale),
        issued.token.expose_str()
    );
    let role_name = {
        let cat = state.i18n.catalog(locale);
        let key = format!("admin_settings.roles.builtin.{}", role.slug);
        if role.is_system && cat.has(&key) {
            cat.t(&key)
        } else {
            role.name.clone()
        }
    };
    let name = format!("{first_name} {last_name}").trim().to_owned();
    send_email(
        state,
        tenant,
        hos_mail::defaults::keys::STAFF_INVITE,
        locale,
        &Recipient::new(email, (!name.is_empty()).then_some(name)),
        json!({
            "inviter": {"name": staff.user.display_name()},
            "role": {"name": role_name},
            "invitee": {"email": email},
            "action_url": link,
            "expires_in_days": TokenPurpose::Invite.default_ttl().num_days(),
        }),
    )
    .await?;
    Ok(row)
}

async fn invite(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let email = forms::normalize_email(&f.text("email", 254));
    let first = f.text("first_name", 80);
    let last = f.text("last_name", 80);
    let locale: Locale = f.parse("locale").unwrap_or(tenant.default_locale);
    if !forms::valid_email(&email) {
        return Ok(failed(&ctx, BACK, ctx.t("admin.common.invalid_email")));
    }
    let role_id: Option<RoleId> = f.id("role_id");
    let roles = identity::list_roles(&state.pool, tenant.id).await?;
    let Some(role) = roles.iter().find(|r| Some(r.id) == role_id) else {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.staff.errors.role_required"),
        ));
    };
    let grants = identity::role_permissions(&state.pool, tenant.id, role.id).await?;
    if !covers(actor_set(&staff), &grants) {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.staff.errors.escalation"),
        ));
    }
    if identity::user_by_email(&state.pool, tenant.id, &email)
        .await?
        .is_some()
    {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.staff.errors.email_taken"),
        ));
    }
    if !has_room(&state, tenant.id, Limit::Staff).await? {
        return Ok(failed(&ctx, BACK, ctx.t("admin_settings.limits.staff")));
    }
    let row = send_invite(
        &state, &tenant, &ctx, &staff, &email, &first, &last, role, locale,
    )
    .await?;
    au.log(
        &state,
        "staff.invite",
        Some(("staff_invite", row.id)),
        json!({"email": email, "role": role.slug}),
    )
    .await?;
    Ok(done(&ctx, BACK, "admin_settings.staff.invited"))
}

async fn load_invite(state: &AppState, tenant: &Tenant, id: &str) -> AppResult<StaffInviteRow> {
    let id = Uuid::parse_str(id).map_err(|_| AppError::NotFound)?;
    srepo::get_staff_invite(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn revoke_invite(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let inv = load_invite(&state, &tenant, &id).await?;
    srepo::revoke_staff_invite(&state.pool, tenant.id, inv.id).await?;
    au.log(
        &state,
        "staff.invite_revoke",
        Some(("staff_invite", inv.id)),
        json!({"email": inv.email}),
    )
    .await?;
    Ok(done(&ctx, BACK, "admin_settings.staff.invite_revoked"))
}

async fn resend_invite(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let inv = load_invite(&state, &tenant, &id).await?;
    if inv.accepted_at.is_some() {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.staff.errors.invite_used"),
        ));
    }
    let role = srepo::get_role(&state.pool, tenant.id, inv.role_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let grants = identity::role_permissions(&state.pool, tenant.id, role.id).await?;
    if !covers(actor_set(&staff), &grants) {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.staff.errors.escalation"),
        ));
    }
    let row = send_invite(
        &state,
        &tenant,
        &ctx,
        &staff,
        &inv.email,
        &inv.first_name,
        &inv.last_name,
        &role,
        inv.locale,
    )
    .await?;
    au.log(
        &state,
        "staff.invite_resend",
        Some(("staff_invite", row.id)),
        json!({"email": inv.email}),
    )
    .await?;
    Ok(done(&ctx, BACK, "admin_settings.staff.invited"))
}

// ---------------------------------------------------------------------------
// member
// ---------------------------------------------------------------------------

/// A role checkbox on the member page.
#[derive(Debug, Clone)]
pub struct RoleChoice {
    pub id: String,
    pub label: String,
    pub checked: bool,
    /// The actor may not change this role (holds less than it grants).
    pub locked: bool,
    pub owner: bool,
}

#[derive(Template)]
#[template(path = "admin/staff/show.html")]
struct ShowPage {
    ctx: PageCtx,
    user: UserRow,
    roles: Vec<RoleChoice>,
    sessions: usize,
    is_me: bool,
    outranks: bool,
    effective: Vec<String>,
}
crate::page!(ShowPage);

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let user = load_staff(&state, &tenant, &id).await?;
    let roles = identity::list_roles(&state.pool, tenant.id).await?;
    let grants = role_grants(&state, &tenant, &roles).await?;
    let held: BTreeSet<RoleId> = identity::user_roles(&state.pool, tenant.id, user.id)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();
    let target_set = union(&grants, &held);
    let choices = roles
        .iter()
        .map(|r| RoleChoice {
            id: r.id.to_string(),
            label: role_label(&ctx, r),
            checked: held.contains(&r.id),
            locked: !grants
                .get(&r.id)
                .is_some_and(|g| covers(actor_set(&staff), g)),
            owner: is_owner_role(r),
        })
        .collect();
    let sessions = identity::list_user_sessions(&state.pool, tenant.id, user.id)
        .await?
        .len();
    let effective = target_set
        .effective()
        .iter()
        .map(|p| roles::permission_label(&ctx, *p))
        .collect();
    let outranks = !covers(
        actor_set(&staff),
        &target_set.grants().map(str::to_owned).collect::<Vec<_>>(),
    );
    let title = user.display_name();
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_settings.staff.title", BACK)],
    );
    render(&ShowPage {
        is_me: user.id == staff.user_id,
        user,
        roles: choices,
        sessions,
        outranks,
        effective,
        ctx,
    })
}

async fn set_roles(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let user = load_staff(&state, &tenant, &id).await?;
    let back = format!("{BACK}/{}", user.id);
    let roles = identity::list_roles(&state.pool, tenant.id).await?;
    let grants = role_grants(&state, &tenant, &roles).await?;
    let wanted: BTreeSet<RoleId> = f
        .ids::<RoleId>("role_ids")
        .into_iter()
        .filter(|id| grants.contains_key(id))
        .collect();
    let held: BTreeSet<RoleId> = identity::user_roles(&state.pool, tenant.id, user.id)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();
    if wanted.is_empty() {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.staff.errors.role_required"),
        ));
    }
    let changed: Vec<RoleId> = wanted.symmetric_difference(&held).copied().collect();
    if changed.is_empty() {
        return Ok(done(&ctx, &back, "admin.common.saved"));
    }
    // 1. Only roles the actor fully holds may be added or removed.
    if changed
        .iter()
        .any(|id| !grants.get(id).is_some_and(|g| covers(actor_set(&staff), g)))
    {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.staff.errors.escalation"),
        ));
    }
    // 2. Nobody raises their own permissions.
    if user.id == staff.user_id && escalates(actor_set(&staff), &union(&grants, &wanted)) {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.staff.errors.self_escalation"),
        ));
    }
    // 3. The last active owner keeps the owner role.
    let owner_ids: BTreeSet<RoleId> = roles
        .iter()
        .filter(|r| is_owner_role(r))
        .map(|r| r.id)
        .collect();
    let loses_owner =
        held.iter().any(|r| owner_ids.contains(r)) && !wanted.iter().any(|r| owner_ids.contains(r));
    if loses_owner
        && user.status == AccountStatus::Active
        && active_owners(&state, &tenant, &roles).await? <= 1
    {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.staff.errors.last_owner"),
        ));
    }
    let mut tx = state.pool.begin().await?;
    for id in wanted.difference(&held) {
        identity::assign_role(&mut *tx, tenant.id, user.id, *id).await?;
    }
    for id in held.difference(&wanted) {
        identity::revoke_role(&mut *tx, tenant.id, user.id, *id).await?;
    }
    tx.commit().await?;
    let slug = |ids: &BTreeSet<RoleId>| -> Vec<String> {
        roles
            .iter()
            .filter(|r| ids.contains(&r.id))
            .map(|r| r.slug.clone())
            .collect()
    };
    au.log(
        &state,
        "staff.roles",
        Some(("user", user.id.into())),
        json!({"from": slug(&held), "to": slug(&wanted)}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.common.saved"))
}

/// Shared checks of deactivate / 2FA reset: not oneself, not someone who
/// holds more than the actor.
async fn may_act_on(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    user: &UserRow,
) -> AppResult<Result<(), &'static str>> {
    if user.id == staff.user_id {
        return Ok(Err("admin_settings.staff.errors.self"));
    }
    let grants = identity::user_permissions(&state.pool, tenant.id, user.id).await?;
    if !covers(actor_set(staff), &grants) {
        return Ok(Err("admin_settings.staff.errors.outranked"));
    }
    Ok(Ok(()))
}

async fn deactivate(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let user = load_staff(&state, &tenant, &id).await?;
    let back = format!("{BACK}/{}", user.id);
    if let Err(key) = may_act_on(&state, &tenant, &staff, &user).await? {
        return Ok(failed(&ctx, &back, ctx.t(key)));
    }
    let roles = identity::list_roles(&state.pool, tenant.id).await?;
    let is_owner = identity::user_roles(&state.pool, tenant.id, user.id)
        .await?
        .iter()
        .any(is_owner_role);
    if is_owner
        && user.status == AccountStatus::Active
        && active_owners(&state, &tenant, &roles).await? <= 1
    {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.staff.errors.last_owner"),
        ));
    }
    let mut tx = state.pool.begin().await?;
    identity::set_user_status(&mut *tx, tenant.id, user.id, AccountStatus::Disabled).await?;
    let ended = identity::delete_user_sessions(&mut *tx, tenant.id, user.id, None).await?;
    tx.commit().await?;
    au.log(
        &state,
        "staff.deactivate",
        Some(("user", user.id.into())),
        json!({"email": user.email, "sessions_ended": ended}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin_settings.staff.deactivated"))
}

async fn activate(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let user = load_staff(&state, &tenant, &id).await?;
    let back = format!("{BACK}/{}", user.id);
    if let Err(key) = may_act_on(&state, &tenant, &staff, &user).await? {
        return Ok(failed(&ctx, &back, ctx.t(key)));
    }
    if user.status == AccountStatus::Disabled && !has_room(&state, tenant.id, Limit::Staff).await? {
        return Ok(failed(&ctx, &back, ctx.t("admin_settings.limits.staff")));
    }
    identity::set_user_status(&state.pool, tenant.id, user.id, AccountStatus::Active).await?;
    au.log(
        &state,
        "staff.activate",
        Some(("user", user.id.into())),
        json!({"email": user.email}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin_settings.staff.activated"))
}

async fn reset_2fa(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::StaffManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let user = load_staff(&state, &tenant, &id).await?;
    let back = format!("{BACK}/{}", user.id);
    if let Err(key) = may_act_on(&state, &tenant, &staff, &user).await? {
        return Ok(failed(&ctx, &back, ctx.t(key)));
    }
    let mut tx = state.pool.begin().await?;
    identity::disable_totp(&mut *tx, tenant.id, user.id).await?;
    identity::delete_user_sessions(&mut *tx, tenant.id, user.id, None).await?;
    tx.commit().await?;
    au.log(
        &state,
        "staff.reset_2fa",
        Some(("user", user.id.into())),
        json!({"email": user.email}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin_settings.staff.twofa_reset"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_coverage() {
        let owner = PermissionSet::from_grants(["*"]);
        let admin = hos_auth::rbac::default_roles()[1].permissions();
        let billing = PermissionSet::from_grants(["clients.view", "billing.*", "services.view"]);
        assert!(covers(&owner, &["*"]));
        assert!(!covers(&admin, &["*"]));
        assert!(covers(&admin, &["clients.*", "staff.manage"]));
        assert!(!covers(&billing, &["clients.manage"]));
        assert!(covers(&billing, &["billing.*", "billing.view"]));
        assert!(!covers(&billing, &["settings.manage"]));
        assert!(!covers(&billing, &["bogus.perm"]));
        assert!(escalates(
            &billing,
            &PermissionSet::from_grants(["settings.manage"])
        ));
        assert!(!escalates(
            &billing,
            &PermissionSet::from_grants(["billing.view"])
        ));
        assert!(escalates(&admin, &owner));
        assert!(!escalates(&owner, &admin));
    }
}
