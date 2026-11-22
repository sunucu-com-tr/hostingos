//! Login sessions: cookie → `sessions` row → [`SessionInfo`] (+ [`Principal`]).
//!
//! * Tenant users use the `hos_session` cookie (`__Host-hos_session` over
//!   https); platform operators use a separate `hos_psession` cookie so both
//!   can be signed in on the same host.
//! * Only the SHA-256 of the token is stored; expiry is absolute
//!   (`expires_at`) **and** idle (`last_seen_at`), see
//!   [`hos_auth::session::SessionConfig`]. "Remember me" is kept in
//!   `sessions.data.remember`.
//! * [`load_session`] (middleware) puts `Arc<SessionInfo>` into the request
//!   extensions when the cookie is valid; extractors in [`crate::extract`]
//!   read it. Stale cookies are cleared.
//! * [`start_session`] always creates a **new** row (call it after password /
//!   OAuth / 2FA success and delete the previous one: fixation defence).

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{DateTime, Utc};
use hos_auth::rbac::{Impersonation, PermissionSet, Principal};
use hos_auth::session::{
    CookieAttributes, SessionStatus, SessionTimes, build_cookie, clear_cookie, cookie_value,
    hash_session_token, is_well_formed_token,
};
use hos_core::domain::UserKind;
use hos_core::ids::{ClientId, SessionId, TenantId};
use hos_db::enums::{AccountStatus, ImpersonatorKind};
use hos_db::repo::identity::{self, NewSession, SessionOwner, SessionRow, UserRow};
use hos_db::repo::{clients, platform};
use http::HeaderValue;
use http::header::SET_COOKIE;
use serde_json::{Value, json};

use crate::error::AppResult;
use crate::state::AppState;
use crate::tenant::{Tenant, path_without_locale};

/// Platform session cookie (plain http).
pub const PLATFORM_COOKIE: &str = "hos_psession";
/// Platform session cookie (https).
pub const PLATFORM_COOKIE_SECURE: &str = "__Host-hos_psession";

/// Which cookie / scope a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionArea {
    /// Tenant users (clients + staff).
    Tenant,
    /// Platform operators (`/platform`).
    Platform,
}

impl SessionArea {
    /// Area of a request path (`/platform…` → platform).
    pub fn for_path(path: &str) -> Self {
        let p = path_without_locale(path);
        if p == "/platform" || p.starts_with("/platform/") {
            Self::Platform
        } else {
            Self::Tenant
        }
    }
}

/// The authenticated (or half-authenticated) session of a request.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// `sessions.id`.
    pub id: SessionId,
    /// `sessions.token_hash` (also the CSRF binding).
    pub token_hash: String,
    /// Cookie scope.
    pub area: SessionArea,
    /// Who is acting. `Anonymous` while a 2FA challenge is pending.
    pub principal: Principal,
    /// The tenant user (also set while `mfa_pending`).
    pub user: Option<UserRow>,
    /// The platform operator.
    pub admin: Option<platform::PlatformAdminRow>,
    /// Password accepted, TOTP code still required (`/auth/2fa`).
    pub mfa_pending: bool,
    /// Staff user without TOTP while the tenant requires it
    /// (`security.staff_2fa_required`): `/admin` sends them to `/auth/2fa/setup`
    /// and the API refuses the session.
    pub mfa_setup_required: bool,
    /// Started by someone else on the user's behalf (staff "log in as
    /// client", platform "sign in as owner").
    pub impersonated: bool,
    /// "Remember me" session.
    pub remember: bool,
    /// Free-form session data.
    pub data: Value,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Absolute expiry.
    pub expires_at: DateTime<Utc>,
}

impl SessionInfo {
    /// Fully logged in (2FA passed when required).
    pub fn is_authenticated(&self) -> bool {
        self.principal.is_authenticated()
    }

    /// Whether the account holder is the one signed in, so password, e-mail,
    /// 2FA, OAuth links and API keys may be changed (not while a staff member
    /// or a platform operator impersonates the user).
    pub fn may_change_credentials(&self) -> bool {
        self.principal.may_change_credentials() && !self.impersonated
    }

    /// Name for the UI.
    pub fn display_name(&self) -> String {
        if let Some(u) = &self.user {
            return u.display_name();
        }
        self.admin
            .as_ref()
            .map(|a| {
                if a.name.is_empty() {
                    a.email.clone()
                } else {
                    a.name.clone()
                }
            })
            .unwrap_or_default()
    }

    /// E-mail of the user / operator.
    pub fn email(&self) -> &str {
        self.user
            .as_ref()
            .map(|u| u.email.as_str())
            .or(self.admin.as_ref().map(|a| a.email.as_str()))
            .unwrap_or("")
    }

    /// The client account a client principal acts for.
    pub fn client_id(&self) -> Option<ClientId> {
        match &self.principal {
            Principal::Client { client_id, .. } => Some(*client_id),
            _ => None,
        }
    }

    /// Home of the principal (`/admin`, `/client`, `/platform`).
    pub fn home_path(&self) -> &'static str {
        match &self.principal {
            Principal::Staff { .. } => "/admin",
            Principal::Client { .. } => "/client",
            Principal::Platform { .. } => "/platform",
            Principal::Anonymous => "/auth/login",
        }
    }
}

/// Cookie name for an area.
pub fn cookie_name(state: &AppState, area: SessionArea) -> &'static str {
    match (area, state.sessions.secure) {
        (SessionArea::Tenant, _) => state.sessions.cookie_name(),
        (SessionArea::Platform, true) => PLATFORM_COOKIE_SECURE,
        (SessionArea::Platform, false) => PLATFORM_COOKIE,
    }
}

/// Session token from the `Cookie` headers.
pub fn token_from_headers<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| cookie_value(h, name))
        .filter(|t| is_well_formed_token(t))
}

/// Middleware: loads the session (see module docs).
pub async fn load_session(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let area = SessionArea::for_path(req.uri().path());
    let tenant_id = req.extensions().get::<Tenant>().map(|t| t.id);
    if area == SessionArea::Tenant && tenant_id.is_none() {
        return next.run(req).await;
    }
    let name = cookie_name(&state, area);
    let Some(token) = token_from_headers(req.headers(), name).map(str::to_owned) else {
        return next.run(req).await;
    };
    let mut clear = false;
    match load(&state, area, tenant_id, &token).await {
        Ok(Some(info)) => {
            req.extensions_mut().insert(Arc::new(info));
        }
        Ok(None) => clear = true,
        Err(e) => tracing::error!(error = %e, "session lookup failed"),
    }
    let mut res = next.run(req).await;
    if clear && !sets_cookie(&res, name) {
        let cookie = clear_cookie(name, state.sessions.secure, "/");
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            res.headers_mut().append(SET_COOKIE, v);
        }
    }
    res
}

/// The active platform operator whose console session cookie this request
/// carries (same browser, same host), whatever area the request is for.
/// Used to tie a tenant page to the operator by a real session, never by
/// matching e-mail addresses.
pub async fn platform_admin_of(
    state: &AppState,
    headers: &http::HeaderMap,
) -> AppResult<Option<hos_db::repo::platform::PlatformAdminRow>> {
    let name = cookie_name(state, SessionArea::Platform);
    let Some(token) = token_from_headers(headers, name) else {
        return Ok(None);
    };
    Ok(load(state, SessionArea::Platform, None, token)
        .await?
        .and_then(|info| info.admin))
}

/// Whether a response already sets cookie `name`.
pub fn sets_cookie(res: &Response, name: &str) -> bool {
    res.headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.starts_with(&format!("{name}=")))
}

async fn load(
    state: &AppState,
    area: SessionArea,
    tenant_id: Option<TenantId>,
    token: &str,
) -> AppResult<Option<SessionInfo>> {
    let hash = hash_session_token(token);
    let scope = match area {
        SessionArea::Tenant => tenant_id,
        SessionArea::Platform => None,
    };
    let Some(row) = identity::validate_session(&state.pool, scope, &hash).await? else {
        return Ok(None);
    };
    let remember = row
        .data
        .get("remember")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Tenant sessions follow the tenant's idle / absolute timeouts.
    let policy = match (area, row.tenant_id) {
        (SessionArea::Tenant, Some(tid)) => {
            Some(crate::routes::admin::settings::security::policy(state, tid).await?)
        }
        _ => None,
    };
    let config = match &policy {
        Some(p) => p.session_config(&state.sessions),
        None => state.sessions.clone(),
    };
    let status = config.evaluate(
        &SessionTimes {
            created_at: row.created_at,
            last_seen_at: row.last_seen_at,
            expires_at: row.expires_at,
            remember_me: remember,
        },
        Utc::now(),
    );
    if !status.is_active() {
        identity::delete_session(&state.pool, &hash).await?;
        return Ok(None);
    }
    debug_assert!(matches!(status, SessionStatus::Active { .. }));
    let info = match (area, row.tenant_id, row.user_id, row.platform_admin_id) {
        (SessionArea::Tenant, Some(tid), Some(uid), _) => {
            let Some(user) = identity::get_user(&state.pool, tid, uid).await? else {
                identity::delete_session(&state.pool, &hash).await?;
                return Ok(None);
            };
            if user.status != AccountStatus::Active {
                identity::delete_session(&state.pool, &hash).await?;
                return Ok(None);
            }
            let mfa_pending = user.totp_enabled && row.mfa_verified_at.is_none();
            let principal = if mfa_pending {
                Principal::Anonymous
            } else {
                build_principal(state, tid, &user, &row).await?
            };
            let mfa_setup_required = user.kind == UserKind::Staff
                && !user.totp_enabled
                && row.impersonator_id.is_none()
                && policy.as_ref().is_some_and(|p| p.staff_2fa_required);
            SessionInfo {
                id: row.id,
                token_hash: hash,
                area,
                principal,
                user: Some(user),
                admin: None,
                mfa_pending,
                mfa_setup_required,
                impersonated: row.impersonator_id.is_some(),
                remember,
                data: row.data,
                created_at: row.created_at,
                expires_at: row.expires_at,
            }
        }
        (SessionArea::Platform, None, None, Some(aid)) => {
            let Some(admin) = platform::get_platform_admin(&state.pool, aid).await? else {
                identity::delete_session(&state.pool, &hash).await?;
                return Ok(None);
            };
            if admin.status != AccountStatus::Active {
                identity::delete_session(&state.pool, &hash).await?;
                return Ok(None);
            }
            SessionInfo {
                id: row.id,
                token_hash: hash,
                area,
                principal: Principal::Platform { admin_id: aid },
                user: None,
                admin: Some(admin),
                mfa_pending: false,
                mfa_setup_required: false,
                impersonated: false,
                remember,
                data: row.data,
                created_at: row.created_at,
                expires_at: row.expires_at,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(info))
}

async fn build_principal(
    state: &AppState,
    tenant_id: TenantId,
    user: &UserRow,
    row: &SessionRow,
) -> AppResult<Principal> {
    Ok(match user.kind {
        UserKind::Staff => {
            let grants = identity::user_permissions(&state.pool, tenant_id, user.id).await?;
            Principal::Staff {
                user_id: user.id,
                tenant_id,
                permissions: PermissionSet::from_grants(grants),
            }
        }
        UserKind::Client => {
            let selected = row
                .data
                .get("client_id")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<ClientId>().ok());
            let client_id = match selected {
                Some(c)
                    if clients::client_membership(&state.pool, tenant_id, c, user.id)
                        .await?
                        .is_some() =>
                {
                    Some(c)
                }
                _ => clients::clients_of_user(&state.pool, tenant_id, user.id)
                    .await?
                    .first()
                    .map(|c| c.id),
            };
            match client_id {
                Some(client_id) => Principal::Client {
                    user_id: user.id,
                    client_id,
                    tenant_id,
                    impersonator: match (row.impersonator_id, row.impersonator_kind) {
                        (Some(id), Some(ImpersonatorKind::Staff)) => Some(Impersonation {
                            staff_user_id: id.into(),
                            started_at: row.created_at,
                        }),
                        _ => None,
                    },
                },
                None => Principal::Anonymous,
            }
        }
    })
}

/// Parameters of [`start_session`].
#[derive(Debug, Clone)]
pub struct StartSession {
    /// Owner.
    pub owner: SessionOwner,
    /// "Remember me".
    pub remember: bool,
    /// Second factor passed (or not required).
    pub mfa_verified: bool,
    /// Client IP.
    pub ip: Option<IpAddr>,
    /// User agent.
    pub user_agent: Option<String>,
    /// Staff impersonation.
    pub impersonator: Option<(uuid::Uuid, ImpersonatorKind)>,
    /// Extra data (merged into `sessions.data`).
    pub data: Value,
}

/// Creates a new session row and returns it with its `Set-Cookie` value.
pub async fn start_session(state: &AppState, s: StartSession) -> AppResult<(SessionRow, String)> {
    let now = Utc::now();
    let new = state.sessions.start(now, s.remember);
    let area = match s.owner {
        SessionOwner::User { .. } => SessionArea::Tenant,
        SessionOwner::PlatformAdmin(_) => SessionArea::Platform,
    };
    let mut row = identity::create_session(
        &state.pool,
        &NewSession {
            owner: s.owner,
            token_hash: new.token_hash.clone(),
            csrf_token: String::new(),
            expires_at: new.expires_at,
            ip: s.ip,
            user_agent: s.user_agent.map(|ua| ua.chars().take(512).collect()),
            mfa_verified: s.mfa_verified,
            impersonator: s.impersonator,
        },
    )
    .await?;
    let mut data = json!({ "remember": s.remember });
    if let (Some(obj), Value::Object(extra)) = (data.as_object_mut(), s.data) {
        obj.extend(extra);
    }
    identity::set_session_data(&state.pool, row.id, &data).await?;
    row.data = data;
    let cookie = build_cookie(
        cookie_name(state, area),
        new.token.expose_str(),
        &CookieAttributes {
            secure: state.sessions.secure,
            max_age_secs: s
                .remember
                .then(|| (new.expires_at - now).num_seconds().max(0)),
            ..CookieAttributes::default()
        },
        now,
    );
    Ok((row, cookie))
}

/// Deletes the session row and returns the clearing `Set-Cookie` value.
pub async fn end_session(state: &AppState, info: &SessionInfo) -> AppResult<String> {
    identity::delete_session(&state.pool, &info.token_hash).await?;
    Ok(clear_cookie(
        cookie_name(state, info.area),
        state.sessions.secure,
        "/",
    ))
}
