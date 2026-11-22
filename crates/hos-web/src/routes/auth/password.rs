//! Forgotten password: `GET/POST /auth/forgot` (always the same answer) and
//! `GET/POST /auth/reset?token=` (sets the password, signs out every session).

use std::net::IpAddr;

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::Response;
use chrono::Utc;
use hos_auth::tokens::{TokenPurpose, hash_one_time_token, issue_token};
use hos_db::enums::AccountStatus;
use hos_db::repo::identity::{self, UserRow};
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::{hash_password, password_problem};
use crate::error::AppResult;
use crate::extract::ClientIp;
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::mail::{Recipient, send_email_logged};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

/// Issues a reset token for `user` and mails the link.
pub async fn send_reset_link(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    user: &UserRow,
    ip: Option<IpAddr>,
) -> AppResult<()> {
    if user.status != AccountStatus::Active {
        return Ok(());
    }
    let issued = issue_token(TokenPurpose::PasswordReset, Utc::now());
    identity::create_password_reset(
        &state.pool,
        tenant.id,
        user.id,
        &issued.token_hash,
        issued.expires_at,
        ip,
    )
    .await?;
    let link = format!(
        "{}{}?token={}",
        ctx.origin,
        ctx.url("/auth/reset"),
        issued.token.expose_str()
    );
    let mut to = Recipient::new(user.email.clone(), Some(user.display_name()));
    to.user_id = Some(user.id);
    send_email_logged(
        state,
        tenant,
        hos_mail::defaults::keys::PASSWORD_RESET,
        user.locale,
        &to,
        json!({
            "action_url": link,
            "expires_in_minutes": TokenPurpose::PasswordReset.default_ttl().num_minutes(),
            "request": { "ip": ip.map(|i| i.to_string()) },
        }),
    )
    .await;
    Ok(())
}

#[derive(Template)]
#[template(path = "auth/forgot.html")]
struct ForgotPage {
    ctx: PageCtx,
    email: String,
    errors: FormErrors,
    sent: bool,
}
crate::page!(ForgotPage);

/// `email` field.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ForgotForm {
    #[serde(default)]
    email: String,
}

/// `GET /auth/forgot`.
pub(super) async fn forgot_form(mut ctx: PageCtx) -> AppResult<Response> {
    let title = ctx.t("auth.forgot.title");
    ctx.set_title(title);
    let sent = ctx.query.split('&').any(|p| p == "sent=1");
    render(&ForgotPage {
        ctx,
        email: String::new(),
        errors: FormErrors::default(),
        sent,
    })
}

/// `POST /auth/forgot`.
pub(super) async fn forgot_submit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    mut ctx: PageCtx,
    Form(form): Form<ForgotForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.password_reset, &ip.key("password-reset"))?;
    let email = forms::normalize_email(&form.email);
    if !forms::valid_email(&email) {
        let title = ctx.t("auth.forgot.title");
        ctx.set_title(title);
        let mut errors = FormErrors::default();
        errors.add("email", ctx.t("auth.errors.email_invalid"));
        return render_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            &ForgotPage {
                ctx,
                email,
                errors,
                sent: false,
            },
        );
    }
    if let Some(user) = identity::user_by_email(&state.pool, tenant.id, &email).await? {
        send_reset_link(&state, &tenant, &ctx, &user, Some(ip.0)).await?;
    }
    Ok(flash::redirect(
        &format!("{}?sent=1", ctx.url("/auth/forgot")),
        SetFlash::info(ctx.t("auth.forgot.sent")),
    ))
}

#[derive(Template)]
#[template(path = "auth/reset.html")]
struct ResetPage {
    ctx: PageCtx,
    token: String,
    errors: FormErrors,
}
crate::page!(ResetPage);

/// `?token=`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ResetQuery {
    #[serde(default)]
    token: String,
}

/// Reset form.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ResetForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password_confirm: String,
}

/// `GET /auth/reset?token=`.
pub(super) async fn reset_form(
    Query(q): Query<ResetQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("auth.reset.title");
    ctx.set_title(title);
    render(&ResetPage {
        ctx,
        token: q.token,
        errors: FormErrors::default(),
    })
}

/// `POST /auth/reset`.
pub(super) async fn reset_submit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    mut ctx: PageCtx,
    Form(form): Form<ResetForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.password_reset, &ip.key("password-reset"))?;
    let title = ctx.t("auth.reset.title");
    ctx.set_title(title);
    let mut errors = FormErrors::default();
    if let Some(msg) = password_problem(&ctx, &form.password, &[]).await? {
        errors.add("password", msg);
    } else if form.password != form.password_confirm {
        errors.add("password_confirm", ctx.t("auth.reset.mismatch"));
    }
    if errors.any() {
        return render_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            &ResetPage {
                ctx,
                token: form.token,
                errors,
            },
        );
    }
    let hash = hash_one_time_token(TokenPurpose::PasswordReset, form.token.trim());
    let Some(user_id) = identity::consume_password_reset(&state.pool, tenant.id, &hash).await?
    else {
        errors.add_general(ctx.t("auth.reset.invalid"));
        return render_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            &ResetPage {
                ctx,
                token: String::new(),
                errors,
            },
        );
    };
    let new_hash = hash_password(&form.password).await?;
    let was_unverified = identity::get_user(&state.pool, tenant.id, user_id)
        .await?
        .is_some_and(|u| u.email_verified_at.is_none());
    let mut tx = state.pool.begin().await?;
    identity::set_password_hash(&mut *tx, tenant.id, user_id, Some(&new_hash)).await?;
    // The link proves control of the mailbox.
    identity::mark_email_verified(&mut *tx, tenant.id, user_id).await?;
    identity::delete_user_sessions(&mut *tx, tenant.id, user_id, None).await?;
    if was_unverified {
        // Whoever set the account up before the mailbox owner did may have
        // left a second factor, sign-in methods, API keys or sub-users.
        super::revoke_unverified_account(&mut tx, tenant.id, user_id, None).await?;
    }
    tx.commit().await?;
    Ok(flash::redirect(
        &ctx.url("/auth/login"),
        SetFlash::success(ctx.t("auth.reset.done")),
    ))
}
