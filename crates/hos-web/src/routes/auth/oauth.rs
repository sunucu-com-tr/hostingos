//! Google / GitHub sign-in.
//!
//! Credentials: a tenant integration (`category = 'oauth'`, `kind = google |
//! github`, `config.client_id`, secret `client_secret`) takes precedence over
//! the process-wide `HOS_GOOGLE_*` / `HOS_GITHUB_*` settings. Providers without
//! credentials, or switched off in the registration settings
//! (`auth.oauth_<kind>`), are hidden. The pending state (state, PKCE verifier,
//! nonce, `next`) travels in an encrypted, 10-minute cookie scoped to
//! `/auth/oauth`.
//!
//! Linking: a signed-in user starts `POST /auth/oauth/{provider}/link`
//! (CSRF-checked); the pending cookie then carries `link_user_id`. The
//! callback links the provider account to that user only if the same user
//! is still signed in (and not impersonated), the account's own e-mail is
//! verified (an unverified account may have been pre-registered by someone
//! else), the provider e-mail is verified and the provider account is not
//! linked to someone else — it never logs in as, or creates, another account.
//!
//! Take-over: an OAuth sign-in with a verified e-mail that matches an
//! account whose e-mail was never verified reclaims it for the mailbox
//! owner: password, 2FA, sessions, other linked sign-in methods, API keys
//! and the client sub-users that account invited are all revoked
//! ([`super::revoke_unverified_account`]).

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::Utc;
use std::sync::Arc;

use hos_auth::oauth::{
    CallbackParams, ExistingAccount, GitHubProvider, GoogleProvider, LinkDecision, OAuthProfile,
    PENDING_COOKIE, PENDING_TTL_SECS, PendingAuth, ProviderKind, build_provider, complete,
    link_decision,
};
use hos_auth::session::{CookieAttributes, build_cookie, clear_cookie, cookie_value};
use hos_core::config::OAuthAppConfig;
use hos_core::crypto::integration_aad;
use hos_core::domain::{IntegrationCategory, UserKind};
use hos_core::secret::SecretString;
use hos_db::enums::OAuthProvider;
use hos_db::repo::{clients, identity, integrations, platform};
use http::header::SET_COOKIE;
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{LoginOutcome, begin_session, safe_next};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, MaybeUser, UserAgent};
use crate::flash::{self, SetFlash};
use crate::mail::{Recipient, send_email_logged};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::PageCtx;

const COOKIE_PATH: &str = "/auth/oauth";

/// A sign-in button.
#[derive(Debug, Clone)]
pub struct ProviderButton {
    /// `google` / `github` (icon id too).
    pub kind: &'static str,
    /// Button label.
    pub label: String,
    /// Start URL (with `next`).
    pub href: String,
}

async fn app_config(
    state: &AppState,
    tenant: &Tenant,
    kind: ProviderKind,
) -> AppResult<Option<OAuthAppConfig>> {
    if !crate::routes::admin::settings::oauth_enabled(state, tenant.id, kind.as_str()).await? {
        return Ok(None);
    }
    if let Some(row) = integrations::active_by_kind(
        &state.pool,
        tenant.id,
        IntegrationCategory::Oauth,
        kind.as_str(),
    )
    .await?
    {
        let client_id = row
            .config
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let secrets: Value = match &row.secrets_enc {
            Some(enc) => state
                .secret_box
                .decrypt_json(enc, &integration_aad(tenant.id, row.id))
                .map_err(AppError::internal)?,
            None => json!({}),
        };
        let secret = secrets
            .get("client_secret")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !client_id.is_empty() && !secret.is_empty() {
            return Ok(Some(OAuthAppConfig {
                client_id,
                client_secret: SecretString::from(secret),
            }));
        }
    }
    Ok(match kind {
        ProviderKind::Google => state.config.google_oauth.clone(),
        ProviderKind::GitHub => state.config.github_oauth.clone(),
    })
}

/// Buttons for the providers this tenant can use.
pub async fn buttons(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    next: Option<&str>,
) -> AppResult<Vec<ProviderButton>> {
    let mut out = Vec::new();
    for (kind, id, key) in [
        (ProviderKind::Google, "google", "auth.oauth.google"),
        (ProviderKind::GitHub, "github", "auth.oauth.github"),
    ] {
        if app_config(state, tenant, kind).await?.is_some() {
            let mut href = format!("/auth/oauth/{id}");
            if let Some(n) = safe_next(next) {
                href.push_str("?next=");
                href.push_str(
                    &url::form_urlencoded::byte_serialize(n.as_bytes()).collect::<String>(),
                );
            }
            out.push(ProviderButton {
                kind: id,
                label: ctx.t(key),
                href,
            });
        }
    }
    Ok(out)
}

/// The provider client (real endpoints unless a test overrides them).
fn provider_client(
    state: &AppState,
    kind: ProviderKind,
    app: OAuthAppConfig,
) -> AppResult<Arc<dyn hos_auth::OAuthProvider>> {
    let overrides = state.web.oauth_endpoints.as_ref();
    let client: Arc<dyn hos_auth::OAuthProvider> = match kind {
        ProviderKind::GitHub => match overrides.and_then(|o| o.github.clone()) {
            Some(ep) => {
                Arc::new(GitHubProvider::with_endpoints(app, ep).map_err(AppError::internal)?)
            }
            None => build_provider(kind, app).map_err(AppError::internal)?,
        },
        ProviderKind::Google => match overrides.and_then(|o| o.google.clone()) {
            Some(ep) => {
                Arc::new(GoogleProvider::with_endpoints(app, ep).map_err(AppError::internal)?)
            }
            None => build_provider(kind, app).map_err(AppError::internal)?,
        },
    };
    Ok(client)
}

fn redirect_uri(ctx: &PageCtx, provider: &str) -> AppResult<url::Url> {
    url::Url::parse(&format!("{}/auth/oauth/{provider}/callback", ctx.origin))
        .map_err(AppError::internal)
}

/// `?next=`.
#[derive(Debug, Default, Deserialize)]
pub struct StartQuery {
    /// Return path after login.
    #[serde(default)]
    pub next: Option<String>,
}

/// `GET /auth/oauth/{provider}`.
pub(super) async fn start(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    Path(provider): Path<String>,
    Query(q): Query<StartQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    check_rate(&state.limits.oauth, &ip.key("oauth"))?;
    let kind: ProviderKind = provider.parse().map_err(|_| AppError::NotFound)?;
    let app = app_config(&state, &tenant, kind)
        .await?
        .ok_or(AppError::NotFound)?;
    let client = provider_client(&state, kind, app)?;
    let callback = redirect_uri(&ctx, kind.as_str())?;
    let (url, pending) = client.authorize_url(&callback);
    let pending = match safe_next(q.next.as_deref()) {
        Some(n) => pending.with_return_to(&n),
        None => pending,
    };
    to_provider(&state, &url, &pending)
}

/// Seals `pending` into the cookie and redirects to the provider.
fn to_provider(state: &AppState, url: &url::Url, pending: &PendingAuth) -> AppResult<Response> {
    let sealed = pending
        .seal(&state.secret_box)
        .map_err(AppError::internal)?;
    let cookie = build_cookie(
        PENDING_COOKIE,
        &sealed,
        &CookieAttributes {
            secure: state.web.secure,
            path: COOKIE_PATH,
            max_age_secs: Some(PENDING_TTL_SECS),
            ..CookieAttributes::default()
        },
        Utc::now(),
    );
    Ok(([(SET_COOKIE, cookie)], Redirect::to(url.as_str())).into_response())
}

/// Link form (`next` = page to return to).
#[derive(Debug, Default, Deserialize)]
pub struct LinkForm {
    /// Return path.
    #[serde(default)]
    pub next: Option<String>,
}

/// `POST /auth/oauth/{provider}/link`: a signed-in user links a provider
/// account to their own account (explicit intent, CSRF-checked form).
pub(super) async fn link_start(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    user: MaybeUser,
    Path(provider): Path<String>,
    ctx: PageCtx,
    axum::Form(f): axum::Form<LinkForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.oauth, &ip.key("oauth"))?;
    let kind: ProviderKind = provider.parse().map_err(|_| AppError::NotFound)?;
    let Some(owner) = link_owner(&user, &tenant) else {
        return Ok(Redirect::to(&ctx.url("/auth/login")).into_response());
    };
    if owner.email_verified_at.is_none() {
        let back = safe_next(f.next.as_deref()).unwrap_or_else(|| home_for_link(&owner));
        return Ok(flash::redirect(
            &ctx.url(&back),
            SetFlash::error(ctx.t("auth.verify.required_for_link")),
        ));
    }
    let app = app_config(&state, &tenant, kind)
        .await?
        .ok_or(AppError::NotFound)?;
    let client = provider_client(&state, kind, app)?;
    let callback = redirect_uri(&ctx, kind.as_str())?;
    let (url, pending) = client.authorize_url(&callback);
    let back = safe_next(f.next.as_deref()).unwrap_or_else(|| home_for_link(&owner));
    let pending = pending.with_return_to(&back).with_link_user(owner.id);
    to_provider(&state, &url, &pending)
}

/// The signed-in, non-impersonated user of this tenant who may link.
fn link_owner(user: &MaybeUser, tenant: &Tenant) -> Option<identity::UserRow> {
    user.authenticated()
        .filter(|s| s.may_change_credentials())
        .and_then(|s| s.user.clone())
        .filter(|u| u.tenant_id == tenant.id)
}

fn home_for_link(user: &identity::UserRow) -> String {
    match user.kind {
        UserKind::Staff => "/admin".to_owned(),
        _ => "/client/account/security".to_owned(),
    }
}

/// Callback of a link intent (see the module docs).
async fn finish_link(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    user: &MaybeUser,
    pending: &PendingAuth,
    db_profile: &identity::OAuthProfile,
    profile: &OAuthProfile,
) -> AppResult<Response> {
    let back = ctx.url(
        pending
            .return_to
            .as_deref()
            .unwrap_or("/client/account/security"),
    );
    let flash_to = |ok: bool, key: &str| {
        let msg = ctx.t(key);
        flash::redirect(
            &back,
            if ok {
                SetFlash::success(msg)
            } else {
                SetFlash::error(msg)
            },
        )
    };
    // Session owner check: the same user must still be signed in.
    let Some(owner) = link_owner(user, tenant).filter(|u| Some(u.id) == pending.link_user_id)
    else {
        return Ok(flash::redirect(
            &ctx.url("/auth/login"),
            SetFlash::error(ctx.t("admin_settings.link.session")),
        ));
    };
    if owner.email_verified_at.is_none() {
        return Ok(flash_to(false, "auth.verify.required_for_link"));
    }
    let verified = profile.email_verified
        && profile
            .email
            .as_deref()
            .is_some_and(|e| e.contains('@') && !e.trim().is_empty());
    if !verified {
        return Ok(flash_to(false, "admin_settings.link.unverified"));
    }
    match identity::user_by_identity(
        &state.pool,
        tenant.id,
        db_profile.provider,
        &db_profile.provider_user_id,
    )
    .await?
    {
        Some(other) if other.id != owner.id => {
            return Ok(flash_to(false, "admin_settings.link.taken"));
        }
        Some(_) => return Ok(flash_to(true, "admin_settings.link.done")),
        None => {}
    }
    match identity::link_identity(&state.pool, tenant.id, owner.id, db_profile).await {
        Ok(_) => {}
        Err(hos_core::CoreError::Conflict(_)) => {
            return Ok(flash_to(false, "admin_settings.link.taken"));
        }
        Err(e) => return Err(e.into()),
    }
    hos_db::repo::audit::write(
        &state.pool,
        &hos_db::repo::audit::AuditEntry::new(
            Some(tenant.id),
            hos_db::enums::ActorKind::User,
            Some(owner.id.into()),
            "auth.identity_link",
        )
        .entity("user", owner.id)
        .changes(json!({"provider": db_profile.provider.as_str()})),
    )
    .await?;
    Ok(flash_to(true, "admin_settings.link.done"))
}

fn pending_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| cookie_value(h, PENDING_COOKIE))
        .map(str::to_owned)
}

fn fail(ctx: &PageCtx, clear: &str, key: &str) -> Response {
    let mut res = flash::redirect(&ctx.url("/auth/login"), SetFlash::error(ctx.t(key)));
    if let Ok(v) = HeaderValue::from_str(clear) {
        res.headers_mut().append(SET_COOKIE, v);
    }
    res
}

/// `GET /auth/oauth/{provider}/callback`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn callback(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ua: UserAgent,
    user: MaybeUser,
    Path(provider): Path<String>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
    ctx: PageCtx,
) -> AppResult<Response> {
    check_rate(&state.limits.oauth, &ip.key("oauth"))?;
    let clear = clear_cookie(PENDING_COOKIE, state.web.secure, COOKIE_PATH);
    let kind: ProviderKind = provider.parse().map_err(|_| AppError::NotFound)?;
    let Some(sealed) = pending_from(&headers) else {
        return Ok(fail(&ctx, &clear, "auth.oauth.expired"));
    };
    let pending = match PendingAuth::open(&state.secret_box, &sealed, Utc::now()) {
        Ok(p) if p.provider == kind => p,
        _ => return Ok(fail(&ctx, &clear, "auth.oauth.expired")),
    };
    let Some(app) = app_config(&state, &tenant, kind).await? else {
        return Err(AppError::NotFound);
    };
    let client = provider_client(&state, kind, app)?;
    let callback = redirect_uri(&ctx, kind.as_str())?;
    let profile = match complete(client.as_ref(), &params, &pending, &callback, Utc::now()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(error = %e, provider = kind.as_str(), "oauth callback failed");
            return Ok(fail(&ctx, &clear, "auth.oauth.failed"));
        }
    };
    let db_provider = match kind {
        ProviderKind::Google => OAuthProvider::Google,
        ProviderKind::GitHub => OAuthProvider::Github,
    };
    if pending.link_user_id.is_some() {
        let db_profile = identity::OAuthProfile {
            provider: db_provider,
            provider_user_id: profile.provider_user_id.clone(),
            email: profile.email.clone(),
            display_name: profile.name.clone(),
            avatar_url: profile.avatar_url.clone(),
            raw: serde_json::to_value(&profile).unwrap_or(Value::Null),
        };
        let mut res = finish_link(
            &state,
            &tenant,
            &ctx,
            &user,
            &pending,
            &db_profile,
            &profile,
        )
        .await?;
        if let Ok(v) = HeaderValue::from_str(&clear) {
            res.headers_mut().append(SET_COOKIE, v);
        }
        return Ok(res);
    }
    let by_identity = identity::user_by_identity(
        &state.pool,
        tenant.id,
        db_provider,
        &profile.provider_user_id,
    )
    .await?;
    let by_email = match profile.email.as_deref() {
        Some(e) => {
            identity::user_by_email(&state.pool, tenant.id, &crate::forms::normalize_email(e))
                .await?
        }
        None => None,
    };
    let decision = link_decision(
        &profile,
        by_identity.as_ref().map(|u| u.id),
        by_email.as_ref().map(|u| ExistingAccount {
            user_id: u.id,
            email_verified: u.email_verified_at.is_some(),
        }),
    );
    let db_profile = identity::OAuthProfile {
        provider: db_provider,
        provider_user_id: profile.provider_user_id.clone(),
        email: profile.email.clone(),
        display_name: profile.name.clone(),
        avatar_url: profile.avatar_url.clone(),
        raw: serde_json::to_value(&profile).unwrap_or(Value::Null),
    };
    let account = match decision {
        LinkDecision::Login { user_id } => {
            identity::get_user(&state.pool, tenant.id, user_id).await?
        }
        LinkDecision::LinkToExisting {
            user_id,
            revoke_local_credentials,
        } => {
            let mut tx = state.pool.begin().await?;
            if revoke_local_credentials {
                // The unverified local account may have been pre-registered by
                // someone else: drop its password and everything it may have
                // planted (2FA, sessions, other sign-in methods, API keys,
                // invited sub-users).
                identity::set_password_hash(&mut *tx, tenant.id, user_id, None).await?;
                super::revoke_unverified_account(
                    &mut tx,
                    tenant.id,
                    user_id,
                    Some((db_provider, &profile.provider_user_id)),
                )
                .await?;
            }
            identity::link_identity(&mut *tx, tenant.id, user_id, &db_profile).await?;
            identity::mark_email_verified(&mut *tx, tenant.id, user_id).await?;
            tx.commit().await?;
            identity::get_user(&state.pool, tenant.id, user_id).await?
        }
        LinkDecision::CreateNew => {
            let allowed = platform::get_setting(&state.pool, tenant.id, "auth.allow_registration")
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !allowed {
                return Ok(fail(&ctx, &clear, "auth.register.closed"));
            }
            if !crate::routes::admin::settings::has_room(
                &state,
                tenant.id,
                crate::routes::platform::limits::Limit::Clients,
            )
            .await?
            {
                return Ok(fail(
                    &ctx,
                    &clear,
                    "admin_settings.limits.signup_unavailable",
                ));
            }
            let created = create_from_profile(&state, &tenant, &ctx, &profile, &db_profile).await?;
            let referral = crate::routes::storefront::referral::ReferralCookie(
                headers
                    .get_all(axum::http::header::COOKIE)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .find_map(|h| {
                        hos_auth::session::cookie_value(
                            h,
                            crate::routes::storefront::referral::COOKIE_NAME,
                        )
                    })
                    .and_then(crate::routes::storefront::referral::parse_cookie),
            );
            crate::routes::storefront::referral::attribute_new_client(
                &state, tenant.id, created.id, &referral,
            )
            .await;
            Some(created)
        }
        LinkDecision::RejectUnverifiedEmail => {
            return Ok(fail(&ctx, &clear, "auth.oauth.unverified"));
        }
    };
    let Some(account) = account.filter(|u| u.can_login(Utc::now())) else {
        return Ok(fail(&ctx, &clear, "auth.errors.invalid_credentials"));
    };
    let mut res = begin_session(
        &state,
        &tenant,
        &ctx,
        ip,
        &ua,
        LoginOutcome {
            user: &account,
            remember: false,
            next: pending.return_to.clone(),
            previous: user.0.as_deref(),
        },
    )
    .await?;
    if let Ok(v) = HeaderValue::from_str(&clear) {
        res.headers_mut().append(SET_COOKIE, v);
    }
    Ok(res)
}

async fn create_from_profile(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    profile: &OAuthProfile,
    db_profile: &identity::OAuthProfile,
) -> AppResult<identity::UserRow> {
    let email = crate::forms::normalize_email(profile.email.as_deref().unwrap_or_default());
    let name = profile.name.clone().unwrap_or_default();
    let (first, last) = match name.trim().rsplit_once(' ') {
        Some((f, l)) => (f.trim().to_owned(), l.trim().to_owned()),
        None if !name.trim().is_empty() => (name.trim().to_owned(), String::new()),
        None => (
            email.split('@').next().unwrap_or_default().to_owned(),
            String::new(),
        ),
    };
    let mut tx = state.pool.begin().await?;
    let user = identity::create_user(
        &mut *tx,
        tenant.id,
        &identity::NewUser {
            email: email.clone(),
            password_hash: None,
            kind: UserKind::Client,
            first_name: first.clone(),
            last_name: last.clone(),
            phone: None,
            locale: ctx.locale,
            email_verified: true,
        },
    )
    .await?;
    clients::create_client_with_owner(
        &mut tx,
        tenant.id,
        &clients::ClientInput::individual(&first, &last, &email, tenant.default_currency),
        user.id,
    )
    .await?;
    identity::link_identity(&mut *tx, tenant.id, user.id, db_profile).await?;
    tx.commit().await?;
    let mut to = Recipient::new(email, Some(user.display_name()));
    to.user_id = Some(user.id);
    send_email_logged(
        state,
        tenant,
        hos_mail::defaults::keys::WELCOME,
        ctx.locale,
        &to,
        json!({ "action_url": format!("{}{}", ctx.origin, ctx.url("/client")) }),
    )
    .await;
    Ok(user)
}
