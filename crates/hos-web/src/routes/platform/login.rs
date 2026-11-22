//! Platform sign in with optional TOTP second factor.
//!
//! * `POST /platform/login` checks the password (rate limit + lockout). Without
//!   2FA the operator session starts right away. With 2FA a short-lived,
//!   HMAC-signed `hos_p2fa` cookie (operator id + expiry, path `/platform`)
//!   remembers the half-done login and the browser goes to `/platform/login/2fa`.
//! * `POST /platform/login/2fa` accepts a TOTP code (replay-safe through
//!   `platform_admins.totp_last_step`) or a one-time recovery code, then
//!   starts the session and clears the cookie.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use hos_auth::session::{CookieAttributes, build_cookie, clear_cookie};
use hos_auth::totp::{TotpSecret, verify_code, verify_recovery_code};
use hos_core::crypto::{hmac_sha256, verify_hmac_sha256};
use hos_core::ids::PlatformAdminId;
use hos_db::enums::AccountStatus;
use hos_db::repo::identity::SessionOwner;
use hos_db::repo::platform::{self, PlatformAdminRow};
use hos_db::repo::saas;
use http::header::SET_COOKIE;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AuditCtx;
use crate::error::{AppError, AppResult};
use crate::extract::MaybeUser;
use crate::forms::{self, FormErrors};
use crate::session::{self, SessionArea, StartSession};
use crate::state::{AppState, check_rate};
use crate::view::{PageCtx, render, render_status};

/// Cookie of a login waiting for its second factor.
pub const PENDING_COOKIE: &str = "hos_p2fa";
/// Lifetime of the pending login (seconds).
pub const PENDING_TTL_SECS: i64 = 300;

/// AAD of an operator's encrypted TOTP secret.
pub fn totp_aad(id: PlatformAdminId) -> Vec<u8> {
    // Pre-rename "hf" label, kept on purpose: AAD of stored TOTP secrets.
    let mut v = b"hf-web/platform-totp/v1:".to_vec();
    v.extend_from_slice(id.as_uuid().as_bytes());
    v
}

fn pending_value(state: &AppState, id: PlatformAdminId, exp: i64) -> String {
    let payload = format!("{id}.{exp}");
    let mac = URL_SAFE_NO_PAD.encode(hmac_sha256(
        &state.cookie_key,
        format!("p2fa:{payload}").as_bytes(),
    ));
    format!("{payload}.{mac}")
}

fn read_pending(state: &AppState, headers: &HeaderMap) -> Option<PlatformAdminId> {
    let raw = headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| hos_auth::session::cookie_value(h, PENDING_COOKIE))?;
    let (payload, mac) = raw.rsplit_once('.')?;
    let tag = URL_SAFE_NO_PAD.decode(mac).ok()?;
    if !verify_hmac_sha256(
        &state.cookie_key,
        format!("p2fa:{payload}").as_bytes(),
        &tag,
    ) {
        return None;
    }
    let (id, exp) = payload.split_once('.')?;
    let exp: i64 = exp.parse().ok()?;
    if exp < Utc::now().timestamp() {
        return None;
    }
    id.parse().ok()
}

fn pending_cookie(state: &AppState, value: Option<&str>) -> String {
    match value {
        Some(v) => build_cookie(
            PENDING_COOKIE,
            v,
            &CookieAttributes {
                secure: state.sessions.secure,
                path: "/platform",
                max_age_secs: Some(PENDING_TTL_SECS),
                ..CookieAttributes::default()
            },
            Utc::now(),
        ),
        None => clear_cookie(PENDING_COOKIE, state.sessions.secure, "/platform"),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
}

#[derive(Template)]
#[template(path = "platform/login.html")]
struct LoginPage {
    ctx: PageCtx,
    email: String,
    errors: FormErrors,
}
crate::page!(LoginPage);

#[derive(Template)]
#[template(path = "platform/login_2fa.html")]
struct ChallengePage {
    ctx: PageCtx,
    errors: FormErrors,
}
crate::page!(ChallengePage);

/// `GET /platform/login`.
pub async fn form(user: MaybeUser, mut ctx: PageCtx) -> AppResult<Response> {
    if user
        .authenticated()
        .is_some_and(|s| s.principal.is_platform_admin())
    {
        return Ok(Redirect::to("/platform").into_response());
    }
    let title = ctx.t("platform.login.title");
    ctx.set_title(title);
    render(&LoginPage {
        ctx,
        email: String::new(),
        errors: FormErrors::default(),
    })
}

/// `POST /platform/login`.
pub async fn submit(
    State(state): State<AppState>,
    req: AuditCtx,
    user: MaybeUser,
    mut ctx: PageCtx,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.login, &req.ip.key("platform-login"))?;
    let email = forms::normalize_email(&form.email);
    let lock_key = format!("platform:{email}");
    let title = ctx.t("platform.login.title");
    ctx.set_title(title);
    let mut errors = FormErrors::default();
    // Public demo: the shared read-only operator login cannot be locked.
    let lockout = !crate::demo::platform_lockout_exempt(&state, &email);
    if lockout && state.lockout.check(&lock_key, Utc::now()).is_err() {
        errors.add_general(ctx.t("auth.errors.locked"));
        return render_status(
            StatusCode::TOO_MANY_REQUESTS,
            &LoginPage { ctx, email, errors },
        );
    }
    let admin = platform::platform_admin_by_email(&state.pool, &email).await?;
    let stored = admin.as_ref().and_then(|a| a.password_hash.clone());
    let password = form.password.clone();
    let ok = tokio::task::spawn_blocking(move || {
        hos_auth::password::verify_user_password(&password, stored.as_deref())
    })
    .await?;
    let admin = match admin {
        Some(a) if ok && a.status == AccountStatus::Active => a,
        _ => {
            if lockout {
                state.lockout.record_failure(&lock_key, Utc::now());
            }
            errors.add_general(ctx.t("auth.errors.invalid_credentials"));
            return render_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                &LoginPage { ctx, email, errors },
            );
        }
    };
    state.lockout.record_success(&lock_key);
    if admin.totp_enabled {
        let exp = Utc::now().timestamp() + PENDING_TTL_SECS;
        let cookie = pending_cookie(&state, Some(&pending_value(&state, admin.id, exp)));
        return Ok(([(SET_COOKIE, cookie)], Redirect::to("/platform/login/2fa")).into_response());
    }
    start(&state, &req, &user, &admin, None).await
}

/// Starts the operator session (fixation-safe) and redirects to `/platform`.
async fn start(
    state: &AppState,
    req: &AuditCtx,
    user: &MaybeUser,
    admin: &PlatformAdminRow,
    extra_cookie: Option<String>,
) -> AppResult<Response> {
    if let Some(old) = user.0.as_ref().filter(|s| s.area == SessionArea::Platform) {
        session::end_session(state, old).await?;
    }
    let (_, cookie) = session::start_session(
        state,
        StartSession {
            owner: SessionOwner::PlatformAdmin(admin.id),
            remember: false,
            mfa_verified: true,
            ip: Some(req.ip.0),
            user_agent: req.ua.0.clone(),
            impersonator: None,
            data: json!({}),
        },
    )
    .await?;
    platform::touch_platform_admin_login(&state.pool, admin.id, Some(req.ip.0)).await?;
    let entry = hos_db::repo::audit::AuditEntry::new(
        None,
        hos_db::enums::ActorKind::PlatformAdmin,
        Some(admin.id.into()),
        "platform.login",
    )
    .request(Some(req.ip.0), req.ua.0.as_deref())
    .metadata(json!({ "mfa": admin.totp_enabled }));
    if let Err(e) = hos_db::repo::audit::write(&state.pool, &entry).await {
        tracing::warn!(error = %e, "audit platform.login");
    }
    let mut res = Redirect::to("/platform").into_response();
    for c in std::iter::once(cookie).chain(extra_cookie) {
        if let Ok(v) = HeaderValue::from_str(&c) {
            res.headers_mut().append(SET_COOKIE, v);
        }
    }
    Ok(res)
}

/// `GET /platform/login/2fa`.
pub async fn challenge_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    if read_pending(&state, &headers).is_none() {
        return Ok(Redirect::to("/platform/login").into_response());
    }
    let title = ctx.t("platform.login.two_factor_title");
    ctx.set_title(title);
    render(&ChallengePage {
        ctx,
        errors: FormErrors::default(),
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct CodeForm {
    #[serde(default)]
    code: String,
}

/// Checks a TOTP or recovery code of `admin` and consumes it.
pub async fn check_admin_code(
    state: &AppState,
    admin: PlatformAdminId,
    input: &str,
) -> AppResult<bool> {
    let Some(totp) = saas::platform_admin_totp(&state.pool, admin).await? else {
        return Ok(false);
    };
    if !totp.totp_enabled {
        return Ok(false);
    }
    let compact: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() == 6 && compact.bytes().all(|b| b.is_ascii_digit()) {
        let Some(enc) = &totp.totp_secret_enc else {
            return Ok(false);
        };
        let bytes = state
            .secret_box
            .decrypt(enc, &totp_aad(admin))
            .map_err(AppError::internal)?;
        let secret = TotpSecret::from_bytes(bytes)?;
        let now = u64::try_from(Utc::now().timestamp()).unwrap_or_default();
        let last = totp.totp_last_step.and_then(|s| u64::try_from(s).ok());
        let Some(step) = verify_code(&secret, &compact, now, last) else {
            return Ok(false);
        };
        let step = i64::try_from(step).map_err(AppError::internal)?;
        return Ok(saas::use_platform_totp_step(&state.pool, admin, step).await?);
    }
    match verify_recovery_code(&compact, &totp.totp_recovery_hashes) {
        Some(i) => {
            Ok(
                saas::use_platform_recovery_code(&state.pool, admin, &totp.totp_recovery_hashes[i])
                    .await?,
            )
        }
        None => Ok(false),
    }
}

/// `POST /platform/login/2fa`.
pub async fn challenge_submit(
    State(state): State<AppState>,
    req: AuditCtx,
    user: MaybeUser,
    headers: HeaderMap,
    mut ctx: PageCtx,
    Form(form): Form<CodeForm>,
) -> AppResult<Response> {
    let Some(admin_id) = read_pending(&state, &headers) else {
        return Ok(Redirect::to("/platform/login").into_response());
    };
    check_rate(&state.limits.two_factor, &req.ip.key("platform-2fa"))?;
    check_rate(
        &state.limits.two_factor,
        &format!("platform-2fa:{admin_id}"),
    )?;
    let admin = platform::get_platform_admin(&state.pool, admin_id)
        .await?
        .filter(|a| a.status == AccountStatus::Active && a.totp_enabled)
        .ok_or(AppError::Unauthorized)?;
    if !check_admin_code(&state, admin.id, &form.code).await? {
        let title = ctx.t("platform.login.two_factor_title");
        ctx.set_title(title);
        let mut errors = FormErrors::default();
        errors.add("code", ctx.t("auth.two_factor.invalid"));
        return render_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            &ChallengePage { ctx, errors },
        );
    }
    start(
        &state,
        &req,
        &user,
        &admin,
        Some(pending_cookie(&state, None)),
    )
    .await
}

/// `POST /platform/logout`.
pub async fn logout(State(state): State<AppState>, user: MaybeUser) -> AppResult<Response> {
    let cookie = match user.0.as_ref().filter(|s| s.area == SessionArea::Platform) {
        Some(s) => session::end_session(&state, s).await?,
        None => hos_auth::session::clear_cookie(
            session::cookie_name(&state, SessionArea::Platform),
            state.sessions.secure,
            "/",
        ),
    };
    Ok(([(SET_COOKIE, cookie)], Redirect::to("/platform/login")).into_response())
}
