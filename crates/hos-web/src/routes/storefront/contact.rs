//! `/contact` (guest ticket with honeypot + rate limit) and `/lang/{code}`
//! (language switch). The cart lives in `routes::cart`.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use hos_core::domain::{Locale, TicketPriority};
use hos_db::enums::{AuthorKind, MessageSource};
use hos_db::repo::{identity, platform, support};
use http::StatusCode;
use http::header::SET_COOKIE;
use serde::Deserialize;

use crate::error::AppResult;
use crate::extract::{ClientIp, MaybeUser};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::locale::{LOCALE_COOKIE, localize_path, safe_local_target, split_locale_prefix};
use crate::seo::{self, SeoPage};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

/// Contact form values.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ContactForm {
    /// Name.
    #[serde(default)]
    pub name: String,
    /// E-mail.
    #[serde(default)]
    pub email: String,
    /// Department slug.
    #[serde(default)]
    pub department: String,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// Message.
    #[serde(default)]
    pub message: String,
    /// Honeypot (must stay empty).
    #[serde(default)]
    pub website: Option<String>,
}

#[derive(Template)]
#[template(path = "storefront/contact.html")]
struct ContactPage {
    ctx: PageCtx,
    form: ContactForm,
    errors: FormErrors,
    departments: Vec<(String, String)>,
    guests_allowed: bool,
    sent: bool,
    /// Title and lead (the `contact` CMS page or the defaults).
    title: String,
    lead: String,
    /// Editable copy below the form (`contact` CMS page).
    blocks: Vec<super::blocks::Block>,
}
crate::page!(ContactPage);

async fn departments(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    logged_in: bool,
) -> AppResult<Vec<(String, String)>> {
    Ok(support::list_departments(&state.pool, tenant.id, false)
        .await?
        .into_iter()
        .filter(|d| logged_in || !d.clients_only)
        .map(|d| {
            let name = super::localized(
                &d.name,
                &d.translations,
                ctx.locale,
                ctx.default_locale,
                "name",
            );
            (d.slug, name)
        })
        .collect())
}

async fn guests_allowed(state: &AppState, tenant: &Tenant) -> AppResult<bool> {
    Ok(
        platform::get_setting(&state.pool, tenant.id, "support.allow_guest_tickets")
            .await?
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
    )
}

async fn page(
    state: &AppState,
    tenant: &Tenant,
    mut ctx: PageCtx,
    form: ContactForm,
    errors: FormErrors,
    sent: bool,
) -> AppResult<ContactPage> {
    ctx.load_store().await?;
    let logged_in = ctx.logged_in();
    let departments = departments(state, tenant, &ctx, logged_in).await?;
    let guests_allowed = guests_allowed(state, tenant).await?;
    let copy = super::landing::load(state, tenant, &ctx, "contact").await?;
    let title = copy
        .as_ref()
        .map(|c| c.page.title.clone())
        .unwrap_or_else(|| ctx.t("storefront.contact.title"));
    let mut blocks = copy
        .as_ref()
        .map(super::landing::Landing::body)
        .unwrap_or_default();
    let lead = super::blocks::lead_text(&blocks);
    if !lead.is_empty()
        && let Some(first) = blocks.first_mut().filter(|b| b.kind == "markdown")
    {
        let rest = first
            .source
            .split_once("\n\n")
            .map(|(_, r)| r.trim().to_owned())
            .unwrap_or_default();
        first.html = crate::markdown::render(&rest);
        first.source = rest;
    }
    blocks.retain(|b| !(b.kind == "markdown" && b.source.is_empty()));
    let lead = if lead.is_empty() {
        ctx.t("storefront.contact.lead")
    } else {
        lead
    };
    let origin = ctx.origin.clone();
    let abs = format!("{origin}{}", ctx.url("/contact"));
    let mut page = SeoPage::new(title.clone(), "/contact").description(lead.clone());
    if let Some(faq) = super::catalog::faq_node(&abs, super::blocks::faq_entries(&blocks)) {
        page = page.node(&faq);
    }
    seo::apply(&mut ctx, page).await?;
    Ok(ContactPage {
        ctx,
        form,
        errors,
        departments,
        guests_allowed,
        sent,
        title,
        lead,
        blocks,
    })
}

/// `GET /contact`.
pub(super) async fn form(
    State(state): State<AppState>,
    tenant: Tenant,
    user: MaybeUser,
    ctx: PageCtx,
) -> AppResult<Response> {
    let mut form = ContactForm::default();
    if let Some(u) = user.authenticated().and_then(|s| s.user.clone()) {
        form.name = u.display_name();
        form.email = u.email;
    }
    let sent = ctx.query.split('&').any(|p| p == "sent=1");
    render(&page(&state, &tenant, ctx, form, FormErrors::default(), sent).await?)
}

/// `POST /contact`.
pub(super) async fn submit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    user: MaybeUser,
    ctx: PageCtx,
    Form(mut form): Form<ContactForm>,
) -> AppResult<Response> {
    let done = || {
        flash::redirect(
            &format!("{}?sent=1", ctx.url("/contact")),
            SetFlash::success(ctx.t("storefront.contact.sent")),
        )
    };
    if forms::honeypot_tripped(form.website.as_deref()) {
        // Pretend success to bots.
        return Ok(done());
    }
    check_rate(&state.limits.contact, &ip.key("contact"))?;
    let session_user = user.authenticated().and_then(|s| s.user.clone());
    let client_id = user.authenticated().and_then(|s| s.client_id());
    form.name = forms::clean(&form.name, 120);
    form.email = forms::normalize_email(&form.email);
    form.subject = forms::clean(&form.subject, 200);
    form.message = form.message.trim().chars().take(10_000).collect();
    let mut errors = FormErrors::default();
    if form.name.chars().count() < 2 {
        errors.add("name", ctx.t("storefront.contact.errors.name"));
    }
    if !forms::valid_email(&form.email) {
        errors.add("email", ctx.t("auth.errors.email_invalid"));
    }
    if form.subject.chars().count() < 3 {
        errors.add("subject", ctx.t("storefront.contact.errors.subject"));
    }
    if form.message.chars().count() < 10 {
        errors.add("message", ctx.t("storefront.contact.errors.message"));
    }
    let deps = support::list_departments(&state.pool, tenant.id, false).await?;
    let dept = deps
        .iter()
        .filter(|d| session_user.is_some() || !d.clients_only)
        .find(|d| d.slug == form.department)
        .or_else(|| deps.iter().find(|d| !d.clients_only));
    if dept.is_none() {
        errors.add_general(ctx.t("storefront.contact.errors.no_department"));
    }
    if session_user.is_none() && !guests_allowed(&state, &tenant).await? {
        errors.add_general(ctx.t("storefront.contact.errors.login_required"));
    }
    let (Some(dept), false) = (dept, errors.any()) else {
        let p = page(&state, &tenant, ctx, form, errors, false).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &p);
    };
    let mut conn = state.pool.acquire().await?;
    let (ticket, _) = support::open_ticket(
        &mut conn,
        tenant.id,
        &support::NewTicket {
            department_id: dept.id,
            client_id,
            user_id: session_user.as_ref().map(|u| u.id),
            guest_name: session_user.is_none().then(|| form.name.clone()),
            guest_email: session_user.is_none().then(|| form.email.clone()),
            subject: form.subject.clone(),
            priority: TicketPriority::Medium,
            service_id: None,
            domain_id: None,
            first_message: support::NewTicketMessage {
                author_kind: if client_id.is_some() {
                    AuthorKind::Client
                } else {
                    AuthorKind::Guest
                },
                user_id: session_user.as_ref().map(|u| u.id),
                author_name: form.name.clone(),
                author_email: Some(form.email.clone()),
                body: form.message.clone(),
                is_internal: false,
                source: MessageSource::Web,
                message_id: None,
                ip: Some(ip.0),
            },
        },
    )
    .await?;
    tracing::info!(tenant = %tenant.slug, ticket = %ticket.number, "contact form ticket opened");
    Ok(done())
}

// ---------------------------------------------------------------------------
// Language switch
// ---------------------------------------------------------------------------

/// `?next=`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct NextQuery {
    #[serde(default)]
    next: Option<String>,
}

/// `GET /lang/{code}?next=/path`: remembers the choice (cookie + the user's
/// profile) and redirects to `next` in that language.
pub(super) async fn switch_language(
    State(state): State<AppState>,
    tenant: Tenant,
    user: MaybeUser,
    Path(code): Path<String>,
    Query(q): Query<NextQuery>,
) -> AppResult<Response> {
    let Ok(locale) = code.parse::<Locale>() else {
        return Ok(Redirect::to("/").into_response());
    };
    let next = q
        .next
        .as_deref()
        .and_then(safe_local_target)
        .unwrap_or("/")
        .to_owned();
    let (path, query) = match next.split_once('?') {
        Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
        None => (next.clone(), None),
    };
    let bare = split_locale_prefix(&path)
        .map(|(_, rest)| rest.to_owned())
        .unwrap_or(path);
    let mut target = localize_path(&bare, locale, tenant.default_locale);
    if let Some(q) = query {
        target = format!("{target}?{q}");
    }
    // Re-check after the prefix was swapped: never a scheme-relative URL.
    let target = safe_local_target(&target).unwrap_or("/").to_owned();
    if let Some(u) = user.authenticated().and_then(|s| s.user.clone())
        && u.locale != locale
    {
        identity::update_user_profile(
            &state.pool,
            tenant.id,
            u.id,
            &u.first_name,
            &u.last_name,
            u.phone.as_deref(),
            locale,
        )
        .await?;
    }
    let cookie = hos_auth::session::build_cookie(
        LOCALE_COOKIE,
        locale.as_str(),
        &hos_auth::session::CookieAttributes {
            secure: state.web.secure,
            max_age_secs: Some(365 * 24 * 3600),
            ..hos_auth::session::CookieAttributes::default()
        },
        chrono::Utc::now(),
    );
    Ok(([(SET_COOKIE, cookie)], Redirect::to(&target)).into_response())
}

/// `POST /currency/{code}?next=…` (a form with the CSRF token, never a
/// link: it changes the open cart): a guest's display currency (one of the
/// tenant's active currencies) in the `hos_currency` cookie; the guest's
/// open cart follows it. Signed-in clients keep their account currency.
/// Prices never move into the URL, so every page keeps one canonical URL.
pub(super) async fn switch_currency(
    State(state): State<AppState>,
    tenant: Tenant,
    user: MaybeUser,
    headers: http::HeaderMap,
    Path(code): Path<String>,
    Query(q): Query<NextQuery>,
) -> AppResult<Response> {
    let next = q
        .next
        .as_deref()
        .and_then(safe_local_target)
        .unwrap_or("/")
        .to_owned();
    let signed_in_client = user
        .authenticated()
        .is_some_and(|s| s.client_id().is_some());
    let wanted = hos_core::money::Currency::new(code.trim()).ok();
    let active = hos_db::repo::catalog::list_currencies(&state.pool, tenant.id, true).await?;
    let Some(currency) = wanted.filter(|c| active.iter().any(|a| a.code == *c)) else {
        return Ok(Redirect::to(&next).into_response());
    };
    if signed_in_client {
        return Ok(Redirect::to(&next).into_response());
    }
    // The guest's open cart is re-priced in the new currency.
    if let Some(token) = headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| hos_auth::session::cookie_value(h, crate::routes::cart::CART_COOKIE))
        .filter(|t| !t.is_empty() && t.len() <= 128)
        && let Some(cart) = hos_db::repo::carts::cart_by_token(
            &state.pool,
            tenant.id,
            &hos_core::crypto::hash_token(token),
        )
        .await?
        .filter(|c| c.user_id.is_none() && c.currency != currency)
    {
        hos_db::repo::carts::update_cart_checkout(
            &state.pool,
            tenant.id,
            cart.id,
            currency,
            cart.payment_integration_id,
            cart.notes.as_deref(),
        )
        .await?;
    }
    let cookie = hos_auth::session::build_cookie(
        crate::view::CURRENCY_COOKIE,
        currency.as_str(),
        &hos_auth::session::CookieAttributes {
            secure: state.web.secure,
            max_age_secs: Some(365 * 24 * 3600),
            ..hos_auth::session::CookieAttributes::default()
        },
        chrono::Utc::now(),
    );
    Ok(([(SET_COOKIE, cookie)], Redirect::to(&next)).into_response())
}
