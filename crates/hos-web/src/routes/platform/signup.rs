//! Self-service tenant sign-up (`/platform/signup`): a hosting company
//! registers itself.
//!
//! 1. `GET /platform/signup` — form (company, slug with live availability
//!    check, plan, owner name / e-mail / password). Closed unless the
//!    `signup.enabled` platform setting is on.
//! 2. `POST /platform/signup` — honeypot + per-IP rate limit, validation
//!    (slug syntax / reserved names / availability, password policy), then a
//!    pending `platform_signups` row and a verification e-mail (platform
//!    mailer). The slug is reserved until the link expires (48 h).
//! 3. `GET /platform/signup/verify?token=` — provisions the tenant
//!    (`provision::provision`: trial of `signup.trial_days`, owner account with
//!    the chosen password) and shows the links to the new panel.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json, Redirect, Response};
use chrono::{Duration, Utc};
use hos_auth::password::PasswordPolicy;
use hos_core::domain::Locale;
use hos_core::integrations::mailer::{EmailMessage, Mailbox};
use hos_db::repo::platform::{self, PlatformPlanRow};
use hos_db::repo::saas::{self, NewSignup};
use serde::Deserialize;
use serde_json::json;

use super::PlatformSettings;
use super::provision::{self, NewTenantRequest};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, Htmx};
use crate::forms::{self, FormErrors};
use crate::state::{AppState, check_rate};
use crate::view::{PageCtx, render, render_status};

/// Hash stored for a sign-up verification token.
pub fn token_hash(token: &str) -> String {
    // Pre-rename "hf" label, kept on purpose: stored token hashes depend on it.
    hos_core::crypto::sha256_hex(format!("hf-signup:{}", token.trim()).as_bytes())
}

/// Validity of the verification link.
pub const LINK_TTL_HOURS: i64 = 48;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SignupForm {
    #[serde(default)]
    company: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    plan: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    terms: Option<String>,
    /// Honeypot.
    #[serde(default)]
    website: Option<String>,
}

#[derive(Template)]
#[template(path = "platform/signup.html")]
struct SignupPage {
    ctx: PageCtx,
    form: SignupForm,
    errors: FormErrors,
    plans: Vec<PlatformPlanRow>,
    base_domain: String,
    trial_days: i64,
    terms_checked: bool,
}
crate::page!(SignupPage);

#[derive(Template)]
#[template(path = "platform/signup_notice.html")]
struct NoticePage {
    ctx: PageCtx,
    kind: String,
    tenant_url: String,
    email: String,
}
crate::page!(NoticePage);

fn notice(mut ctx: PageCtx, kind: &str, tenant_url: String, email: String) -> AppResult<Response> {
    let title = ctx.t(&format!("platform.signup.notice_{kind}_title"));
    ctx.set_title(title);
    render(&NoticePage {
        ctx,
        kind: kind.to_owned(),
        tenant_url,
        email,
    })
}

async fn page(
    state: &AppState,
    settings: &PlatformSettings,
    mut ctx: PageCtx,
    form: SignupForm,
    errors: FormErrors,
) -> AppResult<SignupPage> {
    let title = ctx.t("platform.signup.title");
    ctx.set_title(title);
    let plans = platform::list_plans(&state.pool, true).await?;
    Ok(SignupPage {
        terms_checked: forms::checked(form.terms.as_deref()),
        ctx,
        form,
        errors,
        plans,
        base_domain: state.config.base_domain.clone(),
        trial_days: settings.trial_days,
    })
}

/// `GET /platform/signup`.
pub async fn form(State(state): State<AppState>, ctx: PageCtx) -> AppResult<Response> {
    let settings = PlatformSettings::load(&state).await?;
    if !settings.signup_open(&state) {
        return notice(ctx, "closed", String::new(), String::new());
    }
    let form = SignupForm {
        plan: settings.default_plan.clone().unwrap_or_default(),
        ..SignupForm::default()
    };
    render(&page(&state, &settings, ctx, form, FormErrors::default()).await?)
}

#[derive(Debug, Default, Deserialize)]
pub struct SlugQuery {
    #[serde(default)]
    slug: String,
}

#[derive(Template)]
#[template(path = "platform/_slug_status.html")]
struct SlugFragment {
    ctx: PageCtx,
    ok: bool,
    message: String,
    host: String,
}
crate::page!(SlugFragment);

/// `GET /platform/signup/slug?slug=` — availability (htmx fragment, or JSON
/// `{"slug", "available", "reason"}`).
pub async fn check_slug(
    State(state): State<AppState>,
    ip: ClientIp,
    htmx: Htmx,
    ctx: PageCtx,
    Query(q): Query<SlugQuery>,
) -> AppResult<Response> {
    check_rate(&state.limits.domain_check, &ip.key("signup-slug"))?;
    let slug = provision::normalize_slug(&q.slug);
    let result = provision::check_slug(&state, &slug).await?;
    let (ok, message) = match result {
        Ok(()) => (true, ctx.t("platform.slug.available")),
        Err(p) => (false, ctx.t(p.key())),
    };
    if !htmx.request {
        return Ok(Json(json!({
            "slug": slug,
            "available": ok,
            "reason": result.err().map(|p| p.key().trim_start_matches("platform.slug.")),
        }))
        .into_response());
    }
    let host = format!("{slug}.{}", state.config.base_domain);
    render(&SlugFragment {
        ctx,
        ok,
        message,
        host,
    })
}

/// `POST /platform/signup`.
pub async fn submit(
    State(state): State<AppState>,
    ip: ClientIp,
    ctx: PageCtx,
    Form(form): Form<SignupForm>,
) -> AppResult<Response> {
    let settings = PlatformSettings::load(&state).await?;
    if !settings.signup_open(&state) {
        return notice(ctx, "closed", String::new(), String::new());
    }
    if forms::honeypot_tripped(form.website.as_deref()) {
        // Pretend success; nothing is stored.
        return Ok(Redirect::to("/platform/signup/sent").into_response());
    }
    check_rate(&state.limits.signup, &ip.key("platform-signup"))?;
    let mut errors = FormErrors::default();
    let company = forms::clean(&form.company, 120);
    if company.is_empty() {
        errors.add("company", ctx.t("platform.errors.required"));
    }
    let owner_name = forms::clean(&form.name, 120);
    if owner_name.is_empty() {
        errors.add("name", ctx.t("platform.errors.required"));
    }
    let email = forms::normalize_email(&form.email);
    if !forms::valid_email(&email) {
        errors.add("email", ctx.t("auth.errors.email_invalid"));
    }
    let slug = provision::normalize_slug(&form.slug);
    if let Err(p) = provision::check_slug(&state, &slug).await? {
        errors.add("slug", ctx.t(p.key()));
    }
    // No choice (e.g. the plan list is hidden) → the platform default plan.
    let plan_code = match form.plan.trim() {
        "" => settings.default_plan.clone().unwrap_or_default(),
        code => code.to_owned(),
    };
    let plan = match platform::get_plan_by_code(&state.pool, &plan_code).await? {
        Some(p) if p.is_active && p.is_public => Some(p),
        _ => {
            errors.add("plan", ctx.t("platform.signup.plan_invalid"));
            None
        }
    };
    if let Err(violations) =
        PasswordPolicy::default().validate(&form.password, &[&email, &owner_name, &company])
    {
        let msg = violations
            .iter()
            .map(|v| v.message(ctx.locale))
            .collect::<Vec<_>>()
            .join(" ");
        errors.add("password", msg);
    }
    if !forms::checked(form.terms.as_deref()) {
        errors.add("terms", ctx.t("platform.signup.terms_required"));
    }
    if errors.any() {
        let mut form = form;
        form.password.clear();
        let page = page(&state, &settings, ctx, form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let password = form.password.clone();
    let hash = tokio::task::spawn_blocking(move || hos_auth::password::hash_password(&password))
        .await?
        .map_err(|e| AppError::internal(format!("hashing password: {e}")))?;
    let token = hos_core::crypto::generate_token();
    saas::create_signup(
        &state.pool,
        &NewSignup {
            email: email.clone(),
            company_name: company.clone(),
            owner_name: owner_name.clone(),
            slug: slug.clone(),
            plan_id: plan.map(|p| p.id),
            password_hash: Some(hash),
            locale: ctx.locale,
            token_hash: token_hash(&token),
            ip: Some(ip.0),
            expires_at: Utc::now() + Duration::hours(LINK_TTL_HOURS),
        },
    )
    .await?;
    let link = format!(
        "{}/platform/signup/verify?token={token}",
        state.config.base_url.as_str().trim_end_matches('/')
    );
    send_verification(&state, ctx.locale, &email, &owner_name, &company, &link).await;
    Ok(Redirect::to("/platform/signup/sent").into_response())
}

/// The verification e-mail (platform mailer; tenant templates do not exist yet).
async fn send_verification(
    state: &AppState,
    locale: Locale,
    email: &str,
    name: &str,
    company: &str,
    link: &str,
) {
    let cat = state.i18n.catalog(locale);
    let subject = cat.t("platform.signup.email_subject");
    let greeting = cat.tv("platform.signup.email_greeting", &[("name", &name)]);
    let body = cat.tv("platform.signup.email_body", &[("company", &company)]);
    let cta = cat.t("platform.signup.email_cta");
    let expiry = cat.tv(
        "platform.signup.email_expiry",
        &[("hours", &LINK_TTL_HOURS)],
    );
    let esc = crate::view::escape_html;
    let html = format!(
        "<p>{}</p><p>{}</p><p><a href=\"{}\">{}</a></p><p>{}</p>",
        esc(&greeting),
        esc(&body),
        esc(link),
        esc(&cta),
        esc(&expiry)
    );
    let text = format!("{greeting}\n\n{body}\n\n{cta}: {link}\n\n{expiry}\n");
    let message = EmailMessage {
        to: vec![Mailbox::named(name, email)],
        subject,
        html_body: Some(html),
        text_body: Some(text),
        tags: vec!["platform_signup".into()],
        ..EmailMessage::default()
    };
    if let Err(e) = state.mailer.send(&message).await {
        tracing::error!(error = %e, "sending sign-up verification failed");
    }
}

/// `GET /platform/signup/sent`.
pub async fn sent(ctx: PageCtx) -> AppResult<Response> {
    notice(ctx, "sent", String::new(), String::new())
}

#[derive(Debug, Default, Deserialize)]
pub struct VerifyQuery {
    #[serde(default)]
    token: String,
}

/// `GET /platform/signup/verify?token=`.
pub async fn verify(
    State(state): State<AppState>,
    ctx: PageCtx,
    Query(q): Query<VerifyQuery>,
) -> AppResult<Response> {
    let Some(signup) = saas::pending_signup_by_token(&state.pool, &token_hash(&q.token)).await?
    else {
        return notice(ctx, "invalid", String::new(), String::new());
    };
    let plan_code = match signup.plan_id {
        Some(id) => saas::get_plan(&state.pool, id).await?.map(|p| p.code),
        None => None,
    };
    let (first, last) = signup
        .owner_name
        .split_once(' ')
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .unwrap_or_else(|| (signup.owner_name.clone(), String::new()));
    let mut req = NewTenantRequest::new(&signup.slug, &signup.company_name, &signup.email);
    req.owner_first_name = first;
    req.owner_last_name = last;
    req.owner_password_hash = signup.password_hash.clone();
    req.plan_code = plan_code;
    req.locale = signup.locale;
    req.currency = match signup.locale {
        Locale::Tr => hos_core::money::Currency::TRY,
        Locale::En => hos_core::money::Currency::USD,
    };
    // The pending row reserves the slug; release it for the provisioning check.
    let result = match provision::provision_reserved(&state, &req, signup.id).await {
        Ok(r) => r,
        Err(AppError::Conflict(_)) => {
            return notice(ctx, "invalid", String::new(), String::new());
        }
        Err(e) => return Err(e),
    };
    let entry = hos_db::repo::audit::AuditEntry::new(
        Some(result.provisioned.tenant.id),
        hos_db::enums::ActorKind::System,
        None,
        "platform.tenant.signup",
    )
    .entity("tenant", result.provisioned.tenant.id)
    .metadata(json!({ "email": signup.email, "slug": signup.slug }));
    if let Err(e) = hos_db::repo::audit::write(&state.pool, &entry).await {
        tracing::warn!(error = %e, "audit platform.tenant.signup");
    }
    notice(ctx, "done", result.tenant_url, signup.email)
}
