//! `GET/POST /auth/login`, `POST /auth/logout`.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use hos_db::repo::identity;
use http::StatusCode;
use http::header::SET_COOKIE;
use serde::Deserialize;

use super::oauth::{ProviderButton, buttons};
use super::{
    LoginOutcome, begin_session, redirect_if_logged_in, requires_verification, safe_next,
    unverified_notice,
};
use crate::error::AppResult;
use crate::extract::{ClientIp, MaybeUser, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::session;
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

/// Failed attempts (DB counter) before the account is locked.
const DB_LOCK_AFTER: i32 = 10;
/// DB lock duration.
const DB_LOCK_SECS: i64 = 15 * 60;

/// Login form.
#[derive(Debug, Default, Deserialize)]
pub struct LoginForm {
    /// E-mail.
    #[serde(default)]
    pub email: String,
    /// Password.
    #[serde(default)]
    pub password: String,
    /// "Remember me" checkbox.
    #[serde(default)]
    pub remember: Option<String>,
    /// Return path.
    #[serde(default)]
    pub next: Option<String>,
}

/// `?next=`.
#[derive(Debug, Default, Deserialize)]
pub struct NextQuery {
    /// Return path.
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Template)]
#[template(path = "auth/login.html")]
struct LoginPage {
    ctx: PageCtx,
    email: String,
    remember: bool,
    next: String,
    errors: FormErrors,
    providers: Vec<ProviderButton>,
}
crate::page!(LoginPage);

async fn page(
    state: &AppState,
    tenant: &Tenant,
    mut ctx: PageCtx,
    email: String,
    remember: bool,
    next: Option<String>,
    errors: FormErrors,
) -> AppResult<LoginPage> {
    let title = ctx.t("auth.login.title");
    ctx.set_title(title);
    let next = safe_next(next.as_deref()).unwrap_or_default();
    let providers = buttons(state, tenant, &ctx, Some(&next)).await?;
    Ok(LoginPage {
        ctx,
        email,
        remember,
        next,
        errors,
        providers,
    })
}

/// `GET /auth/login`.
pub(super) async fn form(
    State(state): State<AppState>,
    tenant: Tenant,
    Query(q): Query<NextQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if let Some(r) = redirect_if_logged_in(&ctx) {
        return Ok(r);
    }
    render(
        &page(
            &state,
            &tenant,
            ctx,
            String::new(),
            false,
            q.next,
            FormErrors::default(),
        )
        .await?,
    )
}

/// `POST /auth/login`.
pub(super) async fn submit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ua: UserAgent,
    user: MaybeUser,
    ctx: PageCtx,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.login, &ip.key("login"))?;
    let email = forms::normalize_email(&form.email);
    let remember = forms::checked(form.remember.as_deref());
    let lock_key = format!("{}:{email}", tenant.id);
    let now = Utc::now();
    let mut errors = FormErrors::default();
    // Public demo: a shared login must not be lockable by a visitor typing
    // wrong passwords (the per-IP limit above still applies).
    let lockout = !crate::demo::lockout_exempt(&state, &tenant.slug, &email);

    if lockout && state.lockout.check(&lock_key, now).is_err() {
        errors.add_general(ctx.t("auth.errors.locked"));
        let p = page(&state, &tenant, ctx, email, remember, form.next, errors).await?;
        return render_status(StatusCode::TOO_MANY_REQUESTS, &p);
    }

    let row = if forms::valid_email(&email) {
        identity::user_by_email(&state.pool, tenant.id, &email).await?
    } else {
        None
    };
    // Always run one Argon2 verification (dummy for unknown users) so timing
    // does not reveal which e-mails exist.
    // Imported hashes (WHMCS bcrypt / MD5) are checked too; see `hos_auth::legacy`.
    let stored = row.as_ref().and_then(|u| u.password_hash.clone());
    let password = form.password.clone();
    let verdict = tokio::task::spawn_blocking(move || {
        hos_auth::legacy::verify_any(&password, stored.as_deref())
    })
    .await?;
    let ok = verdict == hos_auth::legacy::Verification::Valid;

    // Right password, but an imported weak (MD5) hash: no session; a reset
    // link is e-mailed instead (rate-limited per account).
    if verdict == hos_auth::legacy::Verification::ResetRequired
        && let Some(u) = row.as_ref().filter(|u| u.can_login(now))
    {
        state.lockout.record_success(&lock_key);
        let throttle = format!("legacy-reset:{}", u.id);
        if matches!(
            state.limits.password_reset.check(&throttle),
            hos_auth::ratelimit::RateDecision::Allowed { .. }
        ) {
            super::password::send_reset_link(&state, &tenant, &ctx, u, Some(ip.0)).await?;
        }
        errors.add_general(ctx.t("import.login.reset_required"));
        let p = page(&state, &tenant, ctx, email, remember, form.next, errors).await?;
        return render_status(StatusCode::FORBIDDEN, &p);
    }

    let user_row = match row {
        Some(u) if ok && u.can_login(now) => u,
        Some(u) => {
            if lockout {
                state.lockout.record_failure(&lock_key, now);
            }
            if !ok && lockout {
                identity::record_login_failure(
                    &state.pool,
                    tenant.id,
                    u.id,
                    DB_LOCK_AFTER,
                    DB_LOCK_SECS,
                )
                .await?;
            }
            let key = if ok && u.locked_until.is_some_and(|t| t > now) {
                "auth.errors.locked"
            } else {
                "auth.errors.invalid_credentials"
            };
            errors.add_general(ctx.t(key));
            let p = page(&state, &tenant, ctx, email, remember, form.next, errors).await?;
            return render_status(StatusCode::UNPROCESSABLE_ENTITY, &p);
        }
        None => {
            if lockout {
                state.lockout.record_failure(&lock_key, now);
            }
            errors.add_general(ctx.t("auth.errors.invalid_credentials"));
            let p = page(&state, &tenant, ctx, email, remember, form.next, errors).await?;
            return render_status(StatusCode::UNPROCESSABLE_ENTITY, &p);
        }
    };
    state.lockout.record_success(&lock_key);

    // Verified e-mail required (`auth.require_email_verification`): an
    // unverified account may have been registered by someone else, so the
    // password alone opens no session. The right password re-sends the link
    // (at most 3 per 15 minutes per account).
    if user_row.email_verified_at.is_none() && requires_verification(&state, &tenant).await? {
        let key = unverified_notice(&state, &tenant, &ctx, &user_row).await?;
        errors.add_general(ctx.t(key));
        let p = page(&state, &tenant, ctx, email, remember, form.next, errors).await?;
        return render_status(StatusCode::FORBIDDEN, &p);
    }

    // Upgrade old hashes transparently.
    if let Some(hash) = user_row.password_hash.as_deref()
        && hos_auth::password::needs_rehash(hash)
    {
        let password = form.password.clone();
        if let Ok(Ok(new_hash)) =
            tokio::task::spawn_blocking(move || hos_auth::password::hash_password(&password)).await
        {
            identity::set_password_hash(&state.pool, tenant.id, user_row.id, Some(&new_hash))
                .await?;
        }
    }

    begin_session(
        &state,
        &tenant,
        &ctx,
        ip,
        &ua,
        LoginOutcome {
            user: &user_row,
            remember,
            next: safe_next(form.next.as_deref()),
            previous: user.0.as_deref(),
        },
    )
    .await
}

/// `POST /auth/logout`.
pub(super) async fn logout(
    State(state): State<AppState>,
    user: MaybeUser,
    ctx: PageCtx,
) -> AppResult<Response> {
    let Some(s) = user
        .0
        .as_ref()
        .filter(|s| s.area == session::SessionArea::Tenant)
    else {
        return Ok(Redirect::to(&ctx.url("/")).into_response());
    };
    let cookie = session::end_session(&state, s).await?;
    let mut res = flash::redirect(
        &ctx.url("/auth/login"),
        SetFlash::success(ctx.t("auth.logout.done")),
    );
    if let Ok(v) = http::HeaderValue::from_str(&cookie) {
        res.headers_mut().append(SET_COOKIE, v);
    }
    Ok(res)
}
