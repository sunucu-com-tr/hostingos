//! TOTP two-factor authentication.
//!
//! * `GET/POST /auth/2fa` — the challenge after a correct password (session
//!   is "MFA pending"; a TOTP code or a one-time recovery code completes it
//!   and the session id is rotated).
//! * `GET /auth/2fa/setup` — QR code (inline SVG) + manual key; `POST`
//!   confirms with a first code and shows ten recovery codes once.
//! * `POST /auth/2fa/disable` — needs the current password (or a code for
//!   password-less OAuth accounts).
//!
//! Secrets are stored encrypted (`SecretBox`, AAD bound to the user id);
//! replayed codes are rejected through `totp_secrets.last_used_step`.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use hos_auth::totp::{
    TotpSecret, generate_recovery_codes, otpauth_uri, verify_code, verify_recovery_code,
};
use hos_core::ids::UserId;
use hos_db::repo::identity::{self, SessionOwner, TotpSecretRow, UserRow};
use http::StatusCode;
use http::header::SET_COOKIE;
use serde::Deserialize;
use serde_json::json;

use super::landing;
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentUser, MaybeUser, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::FormErrors;
use crate::session::{self, StartSession};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

fn aad(user_id: UserId) -> Vec<u8> {
    // Pre-rename "hf" label, kept on purpose: AAD of stored TOTP secrets.
    let mut v = b"hf-web/totp/v1:".to_vec();
    v.extend_from_slice(user_id.as_uuid().as_bytes());
    v
}

fn open_secret(state: &AppState, row: &TotpSecretRow) -> AppResult<TotpSecret> {
    let bytes = state
        .secret_box
        .decrypt(&row.secret_enc, &aad(row.user_id))
        .map_err(AppError::internal)?;
    Ok(TotpSecret::from_bytes(bytes)?)
}

fn unix_now() -> u64 {
    u64::try_from(Utc::now().timestamp()).unwrap_or_default()
}

/// QR code of `uri` as an inline SVG (no XML prologue).
pub fn qr_svg(uri: &str) -> AppResult<String> {
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(AppError::internal)?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .dark_color(qrcode::render::svg::Color("#0b1220"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    let start = svg.find("<svg").unwrap_or(0);
    Ok(svg[start..].replacen("<svg ", "<svg role=\"img\" aria-label=\"QR\" ", 1))
}

/// Re-authentication of a staff user with two-factor sign-in (single-company
/// server area, `crate::mode`): checks and consumes a TOTP or recovery code.
/// `false` without a confirmed secret.
pub async fn verify_staff_code(
    state: &AppState,
    tenant_id: hos_core::ids::TenantId,
    user_id: hos_core::ids::UserId,
    input: &str,
) -> AppResult<bool> {
    match identity::get_totp_secret(&state.pool, tenant_id, user_id)
        .await?
        .filter(|r| r.confirmed_at.is_some())
    {
        Some(row) => check_code(state, tenant_id, &row, input).await,
        None => Ok(false),
    }
}

/// Checks a TOTP or recovery code; consumes it (replay-safe).
async fn check_code(
    state: &AppState,
    tenant_id: hos_core::ids::TenantId,
    row: &TotpSecretRow,
    input: &str,
) -> AppResult<bool> {
    let compact: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() == 6 && compact.bytes().all(|b| b.is_ascii_digit()) {
        let secret = open_secret(state, row)?;
        let last = row.last_used_step.and_then(|s| u64::try_from(s).ok());
        let Some(step) = verify_code(&secret, &compact, unix_now(), last) else {
            return Ok(false);
        };
        let step = i64::try_from(step).map_err(AppError::internal)?;
        return Ok(identity::use_totp_step(&state.pool, tenant_id, row.user_id, step).await?);
    }
    match verify_recovery_code(&compact, &row.recovery_code_hashes) {
        Some(i) => {
            let hash = &row.recovery_code_hashes[i];
            Ok(identity::use_recovery_code(&state.pool, tenant_id, row.user_id, hash).await?)
        }
        None => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Challenge
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "auth/two_factor.html")]
struct ChallengePage {
    ctx: PageCtx,
    errors: FormErrors,
}
crate::page!(ChallengePage);

/// The code field.
#[derive(Debug, Default, Deserialize)]
pub(super) struct CodeForm {
    #[serde(default)]
    code: String,
}

/// `GET /auth/2fa`.
pub(super) async fn challenge_form(user: MaybeUser, mut ctx: PageCtx) -> AppResult<Response> {
    if !user.0.as_ref().is_some_and(|s| s.mfa_pending) {
        return Ok(Redirect::to(&ctx.home_url()).into_response());
    }
    let title = ctx.t("auth.two_factor.title");
    ctx.set_title(title);
    render(&ChallengePage {
        ctx,
        errors: FormErrors::default(),
    })
}

/// `POST /auth/2fa`.
pub(super) async fn challenge_submit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ua: UserAgent,
    user: MaybeUser,
    mut ctx: PageCtx,
    Form(form): Form<CodeForm>,
) -> AppResult<Response> {
    let Some(pending) = user.0.as_ref().filter(|s| s.mfa_pending).cloned() else {
        return Ok(Redirect::to(&ctx.url("/auth/login")).into_response());
    };
    let account: UserRow = pending.user.clone().ok_or(AppError::Unauthorized)?;
    check_rate(&state.limits.two_factor, &ip.key("2fa"))?;
    check_rate(
        &state.limits.two_factor,
        &format!("2fa-user:{}", account.id),
    )?;
    let title = ctx.t("auth.two_factor.title");
    ctx.set_title(title);
    let row = identity::get_totp_secret(&state.pool, tenant.id, account.id)
        .await?
        .filter(|r| r.confirmed_at.is_some());
    let ok = match &row {
        Some(r) => check_code(&state, tenant.id, r, &form.code).await?,
        None => false,
    };
    if !ok {
        let mut errors = FormErrors::default();
        errors.add("code", ctx.t("auth.two_factor.invalid"));
        return render_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            &ChallengePage { ctx, errors },
        );
    }
    // Rotate: a fresh, fully authenticated session replaces the pending one.
    let next = pending
        .data
        .get("next")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let (_, cookie) = session::start_session(
        &state,
        StartSession {
            owner: SessionOwner::User {
                tenant_id: tenant.id,
                user_id: account.id,
            },
            remember: pending.remember,
            mfa_verified: true,
            ip: Some(ip.0),
            user_agent: ua.0.clone(),
            impersonator: None,
            data: json!({}),
        },
    )
    .await?;
    identity::delete_session(&state.pool, &pending.token_hash).await?;
    identity::record_login_success(&state.pool, tenant.id, account.id, Some(ip.0)).await?;
    let target = landing(&ctx, &account, next.as_deref());
    Ok(([(SET_COOKIE, cookie)], Redirect::to(&target)).into_response())
}

// ---------------------------------------------------------------------------
// Setup / disable
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "auth/two_factor_setup.html")]
struct SetupPage {
    ctx: PageCtx,
    enabled: bool,
    qr: String,
    secret: String,
    recovery_left: usize,
    has_password: bool,
    errors: FormErrors,
    back_href: String,
}
crate::page!(SetupPage);

#[derive(Template)]
#[template(path = "auth/recovery_codes.html")]
struct RecoveryPage {
    ctx: PageCtx,
    codes: Vec<String>,
    codes_text: String,
    back_href: String,
}
crate::page!(RecoveryPage);

fn grouped(secret: &str) -> String {
    secret
        .chars()
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

async fn setup_page(
    state: &AppState,
    tenant: &Tenant,
    ctx: PageCtx,
    current: &CurrentUser,
    errors: FormErrors,
) -> AppResult<SetupPage> {
    let user = &current.user;
    let back_href = ctx.home_url();
    if user.totp_enabled {
        let left = identity::get_totp_secret(&state.pool, tenant.id, user.id)
            .await?
            .map(|r| r.recovery_code_hashes.len())
            .unwrap_or(0);
        return Ok(SetupPage {
            ctx,
            enabled: true,
            qr: String::new(),
            secret: String::new(),
            recovery_left: left,
            has_password: user.password_hash.is_some(),
            errors,
            back_href,
        });
    }
    let existing = identity::get_totp_secret(&state.pool, tenant.id, user.id)
        .await?
        .filter(|r| r.confirmed_at.is_none());
    // A fresh visit gets a fresh secret; a failed confirmation keeps the QR.
    let secret = match (errors.any(), existing) {
        (true, Some(row)) => open_secret(state, &row)?,
        _ => new_secret(state, tenant, user).await?,
    };
    let uri = otpauth_uri(&secret, &ctx.site_name, &user.email)?;
    Ok(SetupPage {
        qr: qr_svg(&uri)?,
        secret: grouped(secret.to_base32().expose_str()),
        ctx,
        enabled: false,
        recovery_left: 0,
        has_password: user.password_hash.is_some(),
        errors,
        back_href,
    })
}

async fn new_secret(state: &AppState, tenant: &Tenant, user: &UserRow) -> AppResult<TotpSecret> {
    let secret = TotpSecret::generate();
    let enc = state
        .secret_box
        .encrypt(secret.as_bytes(), &aad(user.id))
        .map_err(AppError::internal)?;
    identity::upsert_totp_secret(&state.pool, tenant.id, user.id, &enc, &[]).await?;
    Ok(secret)
}

/// `403` unless the account holder is signed in: an impersonating staff
/// member or platform operator must not set up, replace or remove the
/// user's second factor (same rule as `client::require_own_session`).
fn require_account_holder(current: &CurrentUser) -> AppResult<()> {
    if current.session.may_change_credentials() {
        Ok(())
    } else {
        Err(AppError::forbidden(
            "2FA changes are disabled while impersonating",
        ))
    }
}

/// `GET /auth/2fa/setup`.
pub(super) async fn setup_form(
    State(state): State<AppState>,
    tenant: Tenant,
    current: CurrentUser,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    require_account_holder(&current)?;
    let title = ctx.t("auth.two_factor.setup_title");
    ctx.set_title(title);
    render(&setup_page(&state, &tenant, ctx, &current, FormErrors::default()).await?)
}

/// `POST /auth/2fa/setup` — confirm with a first code.
pub(super) async fn setup_confirm(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    current: CurrentUser,
    mut ctx: PageCtx,
    Form(form): Form<CodeForm>,
) -> AppResult<Response> {
    require_account_holder(&current)?;
    check_rate(&state.limits.two_factor, &ip.key("2fa-setup"))?;
    let title = ctx.t("auth.two_factor.setup_title");
    ctx.set_title(title);
    let user = &current.user;
    let row = identity::get_totp_secret(&state.pool, tenant.id, user.id)
        .await?
        .filter(|r| r.confirmed_at.is_none());
    let Some(row) = row else {
        return Ok(Redirect::to(&ctx.url("/auth/2fa/setup")).into_response());
    };
    let secret = open_secret(&state, &row)?;
    let code: String = form.code.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(step) = verify_code(&secret, &code, unix_now(), None) else {
        let mut errors = FormErrors::default();
        errors.add("code", ctx.t("auth.two_factor.invalid"));
        let page = setup_page(&state, &tenant, ctx, &current, errors).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    let codes = generate_recovery_codes();
    let mut tx = state.pool.begin().await?;
    identity::upsert_totp_secret(&mut *tx, tenant.id, user.id, &row.secret_enc, &codes.hashes)
        .await?;
    identity::confirm_totp(&mut *tx, tenant.id, user.id).await?;
    identity::use_totp_step(
        &mut *tx,
        tenant.id,
        user.id,
        i64::try_from(step).map_err(AppError::internal)?,
    )
    .await?;
    // This session already proved the second factor.
    identity::set_session_mfa_verified(&mut *tx, current.session.id).await?;
    tx.commit().await?;
    let plain: Vec<String> = codes
        .codes
        .iter()
        .map(|c| c.expose_str().to_owned())
        .collect();
    ctx.flashes.push(crate::flash::FlashMessage {
        kind: crate::flash::FlashKind::Success,
        text: ctx.t("auth.two_factor.enabled"),
    });
    render(&RecoveryPage {
        codes_text: plain.join("\n"),
        codes: plain,
        back_href: ctx.home_url(),
        ctx,
    })
}

/// Disable form.
#[derive(Debug, Default, Deserialize)]
pub(super) struct DisableForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

/// `POST /auth/2fa/disable`.
pub(super) async fn disable(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    current: CurrentUser,
    mut ctx: PageCtx,
    Form(form): Form<DisableForm>,
) -> AppResult<Response> {
    require_account_holder(&current)?;
    check_rate(&state.limits.two_factor, &ip.key("2fa-disable"))?;
    let user = &current.user;
    if user.kind == hos_core::domain::UserKind::Staff
        && crate::routes::admin::settings::security::policy(&state, tenant.id)
            .await?
            .staff_2fa_required
    {
        return Ok(flash::redirect(
            &ctx.url("/auth/2fa/setup"),
            SetFlash::error(ctx.t("admin_settings.security.cannot_disable_2fa")),
        ));
    }
    let ok = match &user.password_hash {
        Some(hash) => {
            let (pw, hash) = (form.password.clone(), hash.clone());
            tokio::task::spawn_blocking(move || {
                hos_auth::password::verify_user_password(&pw, Some(&hash))
            })
            .await?
        }
        None => match identity::get_totp_secret(&state.pool, tenant.id, user.id).await? {
            Some(row) => check_code(&state, tenant.id, &row, &form.code).await?,
            None => false,
        },
    };
    if !ok {
        let title = ctx.t("auth.two_factor.setup_title");
        ctx.set_title(title);
        let mut errors = FormErrors::default();
        errors.add(
            if user.password_hash.is_some() {
                "password"
            } else {
                "code"
            },
            ctx.t("auth.two_factor.confirm_failed"),
        );
        let page = setup_page(&state, &tenant, ctx, &current, errors).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    identity::disable_totp(&state.pool, tenant.id, user.id).await?;
    Ok(flash::redirect(
        &ctx.url("/auth/2fa/setup"),
        SetFlash::success(ctx.t("auth.two_factor.disabled")),
    ))
}
