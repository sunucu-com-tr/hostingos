//! `GET/POST /auth/register` — creates the user, a client account owned by
//! it and an e-mail verification token.
//!
//! When the tenant requires verified e-mails (`auth.require_email_verification`,
//! default) the visitor is sent to "check your inbox" both for new and for
//! already registered addresses (the existing owner gets a password reset
//! link instead), so the form does not reveal which e-mails have accounts.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::Response;
use hos_core::domain::UserKind;
use hos_db::repo::{clients, identity, platform};
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::login::NextQuery;
use super::oauth::{ProviderButton, buttons};
use super::{
    LoginOutcome, begin_session, hash_password, password_problem, redirect_if_logged_in,
    requires_verification, safe_next, send_verification,
};
use crate::error::AppResult;
use crate::extract::{ClientIp, MaybeUser, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::mail::{Recipient, send_email_logged};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

/// Registration form.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RegisterForm {
    /// First name.
    #[serde(default)]
    pub first_name: String,
    /// Last name.
    #[serde(default)]
    pub last_name: String,
    /// E-mail.
    #[serde(default)]
    pub email: String,
    /// Password.
    #[serde(default)]
    pub password: String,
    /// Terms accepted.
    #[serde(default)]
    pub terms: Option<String>,
    /// Return path.
    #[serde(default)]
    pub next: Option<String>,
    /// Honeypot.
    #[serde(default)]
    pub website: Option<String>,
}

#[derive(Template)]
#[template(path = "auth/register.html")]
struct RegisterPage {
    ctx: PageCtx,
    form: RegisterForm,
    errors: FormErrors,
    providers: Vec<ProviderButton>,
    open: bool,
    next: String,
    terms_href: String,
}
crate::page!(RegisterPage);

async fn registration_open(state: &AppState, tenant: &Tenant) -> AppResult<bool> {
    Ok(
        platform::get_setting(&state.pool, tenant.id, "auth.allow_registration")
            .await?
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
    )
}

async fn page(
    state: &AppState,
    tenant: &Tenant,
    mut ctx: PageCtx,
    form: RegisterForm,
    errors: FormErrors,
) -> AppResult<RegisterPage> {
    let title = ctx.t("auth.register.title");
    ctx.set_title(title);
    ctx.load_store().await?;
    let next = safe_next(form.next.as_deref()).unwrap_or_default();
    let providers = buttons(state, tenant, &ctx, Some(&next)).await?;
    let terms_href = ctx
        .store()
        .footer_pages
        .first()
        .map(|p| p.href.clone())
        .unwrap_or_default();
    Ok(RegisterPage {
        open: registration_open(state, tenant).await?,
        ctx,
        form,
        errors,
        providers,
        next,
        terms_href,
    })
}

/// `GET /auth/register`.
pub(super) async fn form(
    State(state): State<AppState>,
    tenant: Tenant,
    Query(q): Query<NextQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if let Some(r) = redirect_if_logged_in(&ctx) {
        return Ok(r);
    }
    let form = RegisterForm {
        next: q.next,
        ..RegisterForm::default()
    };
    render(&page(&state, &tenant, ctx, form, FormErrors::default()).await?)
}

/// `POST /auth/register`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn submit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ua: UserAgent,
    user: MaybeUser,
    referral: crate::routes::storefront::referral::ReferralCookie,
    ctx: PageCtx,
    Form(mut form): Form<RegisterForm>,
) -> AppResult<Response> {
    let check_inbox = |ctx: &PageCtx| {
        flash::redirect(
            &ctx.url("/auth/verify"),
            SetFlash::info(ctx.t("auth.register.check_inbox")),
        )
    };
    if forms::honeypot_tripped(form.website.as_deref()) {
        return Ok(check_inbox(&ctx));
    }
    check_rate(&state.limits.register, &ip.key("register"))?;
    if !registration_open(&state, &tenant).await? {
        let mut errors = FormErrors::default();
        errors.add_general(ctx.t("auth.register.closed"));
        let p = page(&state, &tenant, ctx, form, errors).await?;
        return render_status(StatusCode::FORBIDDEN, &p);
    }
    form.first_name = forms::clean(&form.first_name, 80);
    form.last_name = forms::clean(&form.last_name, 80);
    form.email = forms::normalize_email(&form.email);
    let mut errors = FormErrors::default();
    if form.first_name.is_empty() {
        errors.add("first_name", ctx.t("auth.errors.required"));
    }
    if form.last_name.is_empty() {
        errors.add("last_name", ctx.t("auth.errors.required"));
    }
    if !forms::valid_email(&form.email) {
        errors.add("email", ctx.t("auth.errors.email_invalid"));
    }
    if let Some(msg) = password_problem(
        &ctx,
        &form.password,
        &[&form.email, &form.first_name, &form.last_name],
    )
    .await?
    {
        errors.add("password", msg);
    }
    if !forms::checked(form.terms.as_deref()) {
        errors.add("terms", ctx.t("auth.register.terms_required"));
    }
    if errors.any() {
        form.password.clear();
        let p = page(&state, &tenant, ctx, form, errors).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &p);
    }

    let verify_first = requires_verification(&state, &tenant).await?;
    if let Some(existing) = identity::user_by_email(&state.pool, tenant.id, &form.email).await? {
        if verify_first {
            // Same answer as a fresh signup; the owner gets a reset link.
            super::password::send_reset_link(&state, &tenant, &ctx, &existing, Some(ip.0)).await?;
            return Ok(check_inbox(&ctx));
        }
        errors.add("email", ctx.t("auth.register.email_taken"));
        form.password.clear();
        let p = page(&state, &tenant, ctx, form, errors).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &p);
    }
    // Plan limit (`Clients`): a generic message that says nothing about the
    // store's plan.
    if !crate::routes::admin::settings::has_room(
        &state,
        tenant.id,
        crate::routes::platform::limits::Limit::Clients,
    )
    .await?
    {
        errors.add_general(ctx.t("admin_settings.limits.signup_unavailable"));
        form.password.clear();
        let p = page(&state, &tenant, ctx, form, errors).await?;
        return render_status(StatusCode::SERVICE_UNAVAILABLE, &p);
    }

    let hash = hash_password(&form.password).await?;
    let mut tx = state.pool.begin().await?;
    let new_user = identity::create_user(
        &mut *tx,
        tenant.id,
        &identity::NewUser {
            email: form.email.clone(),
            password_hash: Some(hash),
            kind: UserKind::Client,
            first_name: form.first_name.clone(),
            last_name: form.last_name.clone(),
            phone: None,
            locale: ctx.locale,
            email_verified: false,
        },
    )
    .await?;
    clients::create_client_with_owner(
        &mut tx,
        tenant.id,
        &clients::ClientInput::individual(
            &form.first_name,
            &form.last_name,
            &form.email,
            tenant.default_currency,
        ),
        new_user.id,
    )
    .await?;
    tx.commit().await?;
    crate::routes::storefront::referral::attribute_new_client(
        &state,
        tenant.id,
        new_user.id,
        &referral,
    )
    .await;

    send_verification(&state, &tenant, &ctx, &new_user).await?;
    let mut to = Recipient::new(new_user.email.clone(), Some(new_user.display_name()));
    to.user_id = Some(new_user.id);
    send_email_logged(
        &state,
        &tenant,
        hos_mail::defaults::keys::WELCOME,
        ctx.locale,
        &to,
        json!({ "action_url": format!("{}{}", ctx.origin, ctx.url("/client")) }),
    )
    .await;

    if verify_first {
        return Ok(check_inbox(&ctx));
    }
    begin_session(
        &state,
        &tenant,
        &ctx,
        ip,
        &ua,
        LoginOutcome {
            user: &new_user,
            remember: false,
            next: safe_next(form.next.as_deref()),
            previous: user.0.as_deref(),
        },
    )
    .await
}
