//! E-mail verification: `GET /auth/verify?token=` (without a token: the
//! "check your inbox" page) and `POST /auth/verify/resend`.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;
use hos_auth::tokens::{TokenPurpose, hash_one_time_token};
use hos_db::repo::identity;
use serde::Deserialize;

use super::send_verification;
use crate::error::AppResult;
use crate::extract::{ClientIp, CurrentUser};
use crate::flash::{self, SetFlash};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// `?token=`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

#[derive(Template)]
#[template(path = "auth/notice.html")]
pub(super) struct NoticePage {
    pub ctx: PageCtx,
    pub icon_name: &'static str,
    pub heading: String,
    pub text: String,
    pub action_href: String,
    pub action_label: String,
}
crate::page!(NoticePage);

/// `GET /auth/verify`.
pub(super) async fn verify(
    State(state): State<AppState>,
    tenant: Tenant,
    Query(q): Query<TokenQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let Some(token) = q.token.filter(|t| !t.trim().is_empty()) else {
        let heading = ctx.t("auth.verify.check_title");
        ctx.set_title(heading.clone());
        return render(&NoticePage {
            icon_name: "mail",
            heading,
            text: ctx.t("auth.verify.check_text"),
            action_href: ctx.url("/auth/login"),
            action_label: ctx.t("auth.login.title"),
            ctx,
        });
    };
    let hash = hash_one_time_token(TokenPurpose::EmailVerification, token.trim());
    match identity::consume_email_verification(&state.pool, tenant.id, &hash).await? {
        Some(user_id) => {
            identity::mark_email_verified(&state.pool, tenant.id, user_id).await?;
            let target = if ctx.logged_in() {
                ctx.home_url()
            } else {
                ctx.url("/auth/login")
            };
            Ok(flash::redirect(
                &target,
                SetFlash::success(ctx.t("auth.verify.done")),
            ))
        }
        None => {
            let heading = ctx.t("auth.verify.invalid_title");
            ctx.set_title(heading.clone());
            render(&NoticePage {
                icon_name: "alert-triangle",
                heading,
                text: ctx.t("auth.verify.invalid_text"),
                action_href: ctx.url("/auth/login"),
                action_label: ctx.t("auth.login.title"),
                ctx,
            })
        }
    }
}

/// `POST /auth/verify/resend` (logged-in, unverified users).
pub(super) async fn resend(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    current: CurrentUser,
    ctx: PageCtx,
) -> AppResult<Response> {
    check_rate(&state.limits.email_verification, &ip.key("verify-resend"))?;
    let back = ctx.home_url();
    if current.user.email_verified_at.is_some() {
        return Ok(flash::redirect(
            &back,
            SetFlash::info(ctx.t("auth.verify.already")),
        ));
    }
    send_verification(&state, &tenant, &ctx, &current.user).await?;
    Ok(flash::redirect(
        &back,
        SetFlash::success(ctx.t("auth.verify.resent")),
    ))
}
