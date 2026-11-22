//! "Log in as client" (staff impersonation).
//!
//! * `POST /admin/clients/{id}/impersonate` (`clients.manage`): checks the
//!   target login belongs to the client, validates with
//!   [`hos_auth::rbac::start_impersonation`], creates a **separate** session
//!   row flagged with the staff user (`sessions.impersonator_id`), parks the
//!   staff session token in an HttpOnly cookie and writes `audit_log`.
//! * `GET|POST /admin/impersonation/end` (outside the staff guard — the
//!   caller is the impersonated client session): deletes the impersonation
//!   session, restores the parked staff session (if it is still valid and
//!   belongs to the impersonating staff member) and audits the end.

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use hos_auth::rbac::start_impersonation;
use hos_auth::session::{CookieAttributes, build_cookie, clear_cookie, hash_session_token};
use hos_core::ids::UserId;
use hos_db::enums::{AccountStatus, ActorKind, ImpersonatorKind};
use hos_db::repo::audit::{self, AuditEntry};
use hos_db::repo::clients;
use hos_db::repo::identity::{self, SessionOwner};
use http::header::SET_COOKIE;
use http::{HeaderMap, HeaderValue};
use serde_json::json;
use uuid::Uuid;

use super::common::{Audit, FormData, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, MaybeUser, RequirePermission, UserAgent, perm};
use crate::flash::{self, SetFlash};
use crate::session::{self, SessionArea, StartSession, token_from_headers};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// Cookie that keeps the staff session token while impersonating.
fn parked_cookie(state: &AppState) -> &'static str {
    if state.sessions.secure {
        "__Host-hos_staff_return"
    } else {
        "hos_staff_return"
    }
}

fn with_cookies(mut res: Response, cookies: &[String]) -> Response {
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            res.headers_mut().append(SET_COOKIE, v);
        }
    }
    res
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn start(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    ip: ClientIp,
    UserAgent(ua): UserAgent,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let client = clients::get_client(&state.pool, tenant.id, id.into())
        .await?
        .ok_or(AppError::NotFound)?;
    let back = format!("/admin/clients/{}", client.id);
    let Some(user_id) = f.id::<UserId>("user_id") else {
        return Ok(failed(&ctx, &back, ctx.t("admin.impersonate.no_login")));
    };
    if clients::client_membership(&state.pool, tenant.id, client.id, user_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    let user = identity::get_user(&state.pool, tenant.id, user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if user.status != AccountStatus::Active {
        return Ok(failed(&ctx, &back, ctx.t("admin.impersonate.inactive")));
    }
    start_impersonation(
        &staff.session.principal,
        tenant.id,
        user_id,
        client.id,
        Utc::now(),
    )?;
    let cookie_name = session::cookie_name(&state, SessionArea::Tenant);
    let Some(staff_token) = token_from_headers(&headers, cookie_name).map(str::to_owned) else {
        return Err(AppError::Unauthorized);
    };
    let (_row, cookie) = session::start_session(
        &state,
        StartSession {
            owner: SessionOwner::User {
                tenant_id: tenant.id,
                user_id,
            },
            remember: false,
            mfa_verified: true,
            ip: Some(ip.0),
            user_agent: ua,
            impersonator: Some((staff.user_id.into(), ImpersonatorKind::Staff)),
            data: json!({ "client_id": client.id }),
        },
    )
    .await?;
    let parked = build_cookie(
        parked_cookie(&state),
        &staff_token,
        &CookieAttributes {
            secure: state.sessions.secure,
            ..CookieAttributes::default()
        },
        Utc::now(),
    );
    au.log(
        &state,
        "client.impersonate_start",
        Some(("client", client.id.into())),
        json!({"user": user_id, "email": user.email}),
    )
    .await?;
    let res = flash::redirect(
        &ctx.url("/client"),
        SetFlash::info(ctx.tv("admin.impersonate.started", "name", client.display_name())),
    );
    Ok(with_cookies(res, &[cookie, parked]))
}

#[derive(Template)]
#[template(path = "admin/impersonation_end.html")]
struct EndPage {
    ctx: PageCtx,
}
crate::page!(EndPage);

/// Confirmation page (reachable by URL from the client area).
pub(super) async fn end_page(
    MaybeUser(session): MaybeUser,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    if !session.is_some_and(|s| s.principal.is_impersonated() || is_platform_session(&s)) {
        return Ok(axum::response::Redirect::to(&ctx.url("/admin")).into_response());
    }
    let title = ctx.t("admin.impersonate.end_title");
    ctx.set_title(title);
    render(&EndPage { ctx })
}

/// A staff session opened by a platform operator (`/platform/handoff`).
fn is_platform_session(info: &session::SessionInfo) -> bool {
    info.data
        .get("platform_impersonation")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Ends a platform → tenant impersonation: deletes the session, audits the
/// end (actor = the platform operator) and returns to the console.
async fn end_platform(
    state: &AppState,
    tenant: &Tenant,
    info: &session::SessionInfo,
    ip: ClientIp,
    ua: Option<&str>,
) -> AppResult<Response> {
    let admin_id = identity::validate_session(&state.pool, Some(tenant.id), &info.token_hash)
        .await?
        .and_then(|row| row.impersonator_id);
    let cookie = session::end_session(state, info).await?;
    let mut entry = AuditEntry::new(
        Some(tenant.id),
        ActorKind::PlatformAdmin,
        admin_id,
        "auth.impersonation_ended",
    )
    .request(Some(ip.0), ua)
    .metadata(json!({ "via": "platform" }));
    if let Some(u) = &info.user {
        entry = entry.entity("user", u.id);
    }
    audit::write(&state.pool, &entry).await?;
    let to = format!(
        "{}/platform/tenants/{}",
        state.config.base_url.as_str().trim_end_matches('/'),
        tenant.id
    );
    let mut res = axum::response::Redirect::to(&to).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(SET_COOKIE, v);
    }
    Ok(res)
}

pub(super) async fn end(
    State(state): State<AppState>,
    tenant: Tenant,
    MaybeUser(session): MaybeUser,
    ip: ClientIp,
    UserAgent(ua): UserAgent,
    headers: HeaderMap,
    ctx: PageCtx,
) -> AppResult<Response> {
    let Some(info) = session.filter(|s| s.is_authenticated()) else {
        return Err(AppError::Unauthorized);
    };
    if is_platform_session(&info) {
        return end_platform(&state, &tenant, &info, ip, ua.as_deref()).await;
    }
    let Some(imp) = info.principal.impersonator().cloned() else {
        return Err(AppError::forbidden("not impersonating"));
    };
    let client_id = info.client_id();
    let mut cookies = vec![session::end_session(&state, &info).await?];
    let parked_name = parked_cookie(&state);
    let restored = match token_from_headers(&headers, parked_name) {
        Some(token) => {
            let hash = hash_session_token(token);
            identity::validate_session(&state.pool, Some(tenant.id), &hash)
                .await?
                .filter(|row| row.user_id == Some(imp.staff_user_id))
                .map(|_| token.to_owned())
        }
        None => None,
    };
    cookies.push(clear_cookie(parked_name, state.sessions.secure, "/"));
    let entry = AuditEntry::new(
        Some(tenant.id),
        ActorKind::User,
        Some(imp.staff_user_id.into()),
        "client.impersonate_end",
    )
    .request(Some(ip.0), ua.as_deref())
    .changes(json!({"user": info.principal.user_id(), "started_at": imp.started_at}));
    let entry = match client_id {
        Some(c) => entry.entity("client", c),
        None => entry,
    };
    audit::write(&state.pool, &entry).await?;
    match restored {
        Some(token) => {
            // Replaces the (just cleared) session cookie with the staff token.
            cookies.push(build_cookie(
                session::cookie_name(&state, SessionArea::Tenant),
                &token,
                &CookieAttributes {
                    secure: state.sessions.secure,
                    ..CookieAttributes::default()
                },
                Utc::now(),
            ));
            let to = client_id
                .map(|c| format!("/admin/clients/{c}"))
                .unwrap_or_else(|| "/admin".into());
            let res = flash::redirect(
                &ctx.url(&to),
                SetFlash::success(ctx.t("admin.impersonate.ended")),
            );
            Ok(with_cookies(res, &cookies))
        }
        None => {
            let res = flash::redirect(
                &ctx.url("/auth/login?next=/admin"),
                SetFlash::info(ctx.t("admin.impersonate.ended_login")),
            );
            Ok(with_cookies(res.into_response(), &cookies))
        }
    }
}
