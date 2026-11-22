//! Authentication for tenant users (clients and staff share one form; the
//! user's kind decides where they land: `/client` or `/admin`).
//!
//! | Route | |
//! |---|---|
//! | `GET/POST /auth/login`, `POST /auth/logout` | [`login`] |
//! | `GET/POST /auth/register` | [`register`] (user + client account) |
//! | `GET /auth/verify?token=`, `POST /auth/verify/resend` | [`verify`] |
//! | `GET/POST /auth/forgot`, `GET/POST /auth/reset?token=` | [`password`] |
//! | `GET /auth/oauth/{google,github}` (+ `/callback`), `POST …/link` | [`oauth`] |
//! | `GET/POST /auth/2fa`, `GET/POST /auth/2fa/setup`, `POST /auth/2fa/disable` | [`two_factor`] |
//! | `GET/POST /auth/invite?token=` | [`invite`] (staff invitation acceptance) |
//!
//! Security notes: rate limits per IP ([`crate::state::Limits`]), per-email
//! lockout (memory + DB counters), dummy password verification for unknown
//! e-mails, a fresh session id after every login step, generic error
//! messages (no account enumeration), tokens stored hashed only.

pub mod invite;
pub mod login;
pub mod oauth;
pub mod password;
pub mod register;
pub mod two_factor;
pub mod verify;

use axum::Router;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use hos_core::domain::UserKind;
use hos_db::repo::identity::{self, SessionOwner, UserRow};
use http::header::SET_COOKIE;
use serde_json::json;

use crate::error::AppResult;
use crate::extract::{ClientIp, UserAgent};
use crate::session::{self, SessionInfo, StartSession};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::PageCtx;

/// Auth routes.
pub fn router(_state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(login::form).post(login::submit))
        .route("/auth/logout", post(login::logout))
        .route("/auth/register", get(register::form).post(register::submit))
        .route("/auth/verify", get(verify::verify))
        .route("/auth/verify/resend", post(verify::resend))
        .route(
            "/auth/forgot",
            get(password::forgot_form).post(password::forgot_submit),
        )
        .route(
            "/auth/reset",
            get(password::reset_form).post(password::reset_submit),
        )
        .route("/auth/oauth/{provider}", get(oauth::start))
        .route("/auth/oauth/{provider}/callback", get(oauth::callback))
        .route("/auth/oauth/{provider}/link", post(oauth::link_start))
        .route(
            "/auth/2fa",
            get(two_factor::challenge_form).post(two_factor::challenge_submit),
        )
        .route(
            "/auth/2fa/setup",
            get(two_factor::setup_form).post(two_factor::setup_confirm),
        )
        .route("/auth/2fa/disable", post(two_factor::disable))
        .route("/auth/invite", get(invite::form).post(invite::submit))
}

/// A safe in-app `next` target (no external URLs — also not once the locale
/// prefix is stripped, `/en//evil.com` — and never back into `/auth`).
pub fn safe_next(next: Option<&str>) -> Option<String> {
    let next = next?.trim();
    let path = crate::locale::safe_local_target(next)?;
    let bare = crate::tenant::path_without_locale(path.split('?').next().unwrap_or(path));
    if bare == "/auth" || bare.starts_with("/auth/") {
        return None;
    }
    Some(path.to_owned())
}

/// Area home of a user kind.
pub fn home_for(kind: UserKind) -> &'static str {
    match kind {
        UserKind::Staff => "/admin",
        UserKind::Client => "/client",
    }
}

/// Where to go after a completed login: `next` when it belongs to the
/// user's area (or the storefront), otherwise the area home.
pub fn landing(ctx: &PageCtx, user: &UserRow, next: Option<&str>) -> String {
    let home = home_for(user.kind);
    if let Some(n) = next.and_then(|n| safe_next(Some(n))) {
        let bare = crate::tenant::path_without_locale(&n).to_owned();
        let foreign = match user.kind {
            UserKind::Staff => bare.starts_with("/client"),
            UserKind::Client => bare.starts_with("/admin"),
        };
        if !foreign && !bare.starts_with("/platform") {
            return n;
        }
    }
    ctx.url(home)
}

/// Parameters of [`begin_session`].
pub struct LoginOutcome<'a> {
    /// The authenticated user.
    pub user: &'a UserRow,
    /// "Remember me".
    pub remember: bool,
    /// Requested `next` path.
    pub next: Option<String>,
    /// The session being replaced (fixation defence).
    pub previous: Option<&'a SessionInfo>,
}

/// Starts a **new** session after the password / OAuth step. Users with 2FA
/// get a half-authenticated session and are sent to `/auth/2fa`; everyone
/// else lands on [`landing`]. While the tenant requires verified e-mails
/// (`auth.require_email_verification`) an unverified account gets no
/// session at all (the password form explains it and re-sends the link
/// itself; this is the backstop for every other sign-in path).
pub async fn begin_session(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    ip: ClientIp,
    ua: &UserAgent,
    o: LoginOutcome<'_>,
) -> AppResult<Response> {
    if o.user.email_verified_at.is_none() && requires_verification(state, tenant).await? {
        let key = unverified_notice(state, tenant, ctx, o.user).await?;
        return Ok(crate::flash::redirect(
            &ctx.url("/auth/login"),
            crate::flash::SetFlash::error(ctx.t(key)),
        ));
    }
    if let Some(prev) = o
        .previous
        .filter(|s| s.area == session::SessionArea::Tenant)
    {
        session::end_session(state, prev).await?;
    }
    let needs_mfa = o.user.totp_enabled;
    let (_, cookie) = session::start_session(
        state,
        StartSession {
            owner: SessionOwner::User {
                tenant_id: tenant.id,
                user_id: o.user.id,
            },
            remember: o.remember,
            mfa_verified: !needs_mfa,
            ip: Some(ip.0),
            user_agent: ua.0.clone(),
            impersonator: None,
            data: json!({ "next": o.next }),
        },
    )
    .await?;
    let target = if needs_mfa {
        ctx.url("/auth/2fa")
    } else {
        identity::record_login_success(&state.pool, tenant.id, o.user.id, Some(ip.0)).await?;
        landing(ctx, o.user, o.next.as_deref())
    };
    Ok(([(SET_COOKIE, cookie)], Redirect::to(&target)).into_response())
}

/// Revokes everything a pre-registration may have planted on an account
/// whose e-mail was never verified, once the mailbox owner proves control
/// (OAuth sign-in with a verified e-mail, completed password reset): 2FA,
/// every session, every linked sign-in method except `keep` (the one being
/// used right now), every API key and the client sub-user memberships /
/// invitations the account created. The password is the caller's business
/// (cleared for OAuth, replaced by a reset). Run it in the caller's
/// transaction.
pub async fn revoke_unverified_account(
    conn: &mut sqlx::PgConnection,
    tenant_id: hos_core::ids::TenantId,
    user_id: hos_core::ids::UserId,
    keep: Option<(hos_db::enums::OAuthProvider, &str)>,
) -> AppResult<()> {
    identity::disable_totp(&mut *conn, tenant_id, user_id).await?;
    identity::delete_user_sessions(&mut *conn, tenant_id, user_id, None).await?;
    let identities = identity::delete_user_identities(&mut *conn, tenant_id, user_id, keep).await?;
    let keys = identity::revoke_user_api_keys(&mut *conn, tenant_id, user_id).await?;
    let members =
        hos_db::repo::clients::remove_members_invited_by(&mut *conn, tenant_id, user_id).await?;
    hos_db::repo::audit::write(
        &mut *conn,
        &hos_db::repo::audit::AuditEntry::new(
            Some(tenant_id),
            hos_db::enums::ActorKind::System,
            None,
            "auth.unverified_account_reclaimed",
        )
        .entity("user", user_id)
        .metadata(json!({
            "identities_removed": identities,
            "api_keys_revoked": keys,
            "members_removed": members,
        })),
    )
    .await?;
    Ok(())
}

/// Already signed in? Then auth pages redirect home.
pub fn redirect_if_logged_in(ctx: &PageCtx) -> Option<Response> {
    ctx.logged_in()
        .then(|| Redirect::to(&ctx.home_url()).into_response())
}

/// Password policy violations as one message (`None` when acceptable). The
/// rules come from the tenant's security settings
/// (`security.password_*`, see `admin::settings::security`).
pub async fn password_problem(
    ctx: &PageCtx,
    password: &str,
    user_inputs: &[&str],
) -> AppResult<Option<String>> {
    let policy = match &ctx.tenant {
        Some(t) => crate::routes::admin::settings::security::policy(&ctx.state, t.id)
            .await?
            .password_policy(),
        None => hos_auth::password::PasswordPolicy::default(),
    };
    Ok(policy.validate(password, user_inputs).err().map(|v| {
        v.iter()
            .map(|p| p.message(ctx.locale))
            .collect::<Vec<_>>()
            .join(" ")
    }))
}

/// Hashes a password off the async runtime.
pub async fn hash_password(password: &str) -> AppResult<String> {
    let password = password.to_owned();
    Ok(tokio::task::spawn_blocking(move || hos_auth::password::hash_password(&password)).await??)
}

/// Whether the tenant requires a verified e-mail before the first login.
pub async fn requires_verification(state: &AppState, tenant: &Tenant) -> AppResult<bool> {
    Ok(hos_db::repo::platform::get_setting(
        &state.pool,
        tenant.id,
        "auth.require_email_verification",
    )
    .await?
    .and_then(|v| v.as_bool())
    .unwrap_or(true))
}

/// A sign-in of `user` was refused because its e-mail is unverified:
/// re-sends the verification link (at most 3 per 15 minutes per account,
/// `limits.email_verification`) and returns the message key to show.
pub async fn unverified_notice(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    user: &UserRow,
) -> AppResult<&'static str> {
    let key = format!("verify-login:{}:{}", tenant.id, user.id);
    let send = matches!(
        state.limits.email_verification.check(&key),
        hos_auth::ratelimit::RateDecision::Allowed { .. }
    );
    if send {
        send_verification(state, tenant, ctx, user).await?;
        Ok("auth.errors.email_unverified")
    } else {
        Ok("auth.errors.email_unverified_wait")
    }
}

/// Issues an e-mail verification token for `user` and mails the link.
pub async fn send_verification(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    user: &UserRow,
) -> AppResult<()> {
    use hos_auth::tokens::{TokenPurpose, issue_token};
    let issued = issue_token(TokenPurpose::EmailVerification, chrono::Utc::now());
    identity::create_email_verification(
        &state.pool,
        tenant.id,
        user.id,
        &user.email,
        &issued.token_hash,
        issued.expires_at,
    )
    .await?;
    let link = format!(
        "{}{}?token={}",
        ctx.origin,
        ctx.url("/auth/verify"),
        issued.token.expose_str()
    );
    let mut to = crate::mail::Recipient::new(user.email.clone(), Some(user.display_name()));
    to.user_id = Some(user.id);
    crate::mail::send_email_logged(
        state,
        tenant,
        hos_mail::defaults::keys::EMAIL_VERIFICATION,
        user.locale,
        &to,
        json!({ "action_url": link, "expires_in_hours": TokenPurpose::EmailVerification.default_ttl().num_hours() }),
    )
    .await;
    Ok(())
}
