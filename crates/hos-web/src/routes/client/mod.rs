//! Client area (`/client/*`).
//!
//! Every route sits behind [`guards::require_client`] and the
//! [`member_access`] layer (sub-users of a client account only reach the
//! areas their `client_users.permissions` allow). Handlers take
//! [`CurrentClient`] and **always** scope queries by `client.client_id`: an
//! object of another client answers `404`, never `403`.
//!
//! | Module | Pages |
//! |---|---|
//! | (here) | dashboard, impersonation exit, shared helpers |
//! | [`services`] | services, credentials, SSO, plan change, cancellation |
//! | [`vm`] | VPS / cloud: live state, metrics, power, reinstall, snapshots, console |
//! | [`domains`] | domains, renewal, nameservers, lock, privacy, EPP, contacts, DNS |
//! | [`billing`] | invoices (+ print, pay, return), credit, transactions, payment methods, orders |
//! | [`invoice_files`] | invoice PDF and legal e-Fatura / e-Arşiv PDF downloads |
//! | [`support`] | tickets, replies, attachments, rating, KB suggestions |
//! | [`account`] | profile, security, sessions, users, contacts, API keys, privacy |
//!
//! Templates live in `templates/client/` (pages extend `client/_layout.html`,
//! which adds the impersonation banner and `static/css/client.css`),
//! translations in `locales/{tr,en}/client.json`.

pub mod account;
pub mod affiliate;
pub mod billing;
pub mod domains;
pub mod downloads;
pub mod invoice_files;
pub mod menu;
pub mod quotes;
pub mod services;
pub mod support;
pub mod vm;
pub(crate) mod vm_manage;
mod vm_ssh_keys;

use std::convert::Infallible;
use std::str::FromStr;

use askama::Template;
use axum::Router;
use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::{NaiveDate, Utc};
use hos_core::CoreError;
use hos_core::domain::{DomainStatus, InvoiceStatus, Locale, ServiceStatus};
use hos_core::error::IntegrationError;
use hos_db::Page;
use hos_db::enums::ActorKind;
use hos_db::repo::audit::{self, AuditEntry};
use hos_db::repo::domains::DomainRow;
use hos_db::repo::invoices::{self, InvoiceFilter, InvoiceRow};
use hos_db::repo::services::ServiceListRow;
use hos_db::repo::stats::{self, ClientSummary};
use hos_db::repo::support::{self as support_repo, TicketFilter, TicketListRow};
use hos_db::repo::{clients, content, domains as domain_repo, services as service_repo};
use http::header::{CACHE_CONTROL, SET_COOKIE};
use http::request::Parts;
use http::{HeaderValue, StatusCode};

use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, UserAgent};
use crate::flash::{self, SetFlash};
use crate::routes::guards;
use crate::session::SessionInfo;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render};

/// Routes of the client area.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/client", get(dashboard))
        .route("/client/impersonation/end", post(end_impersonation))
        .merge(services::routes())
        .merge(vm::routes())
        .merge(domains::routes())
        .merge(billing::routes())
        .merge(invoice_files::routes())
        .merge(support::routes())
        .merge(account::routes())
        .merge(quotes::routes())
        .merge(affiliate::routes())
        .merge(downloads::routes())
        .route_layer(from_fn_with_state(state.clone(), member_access))
        .route_layer(from_fn(guards::require_client))
}

// ---------------------------------------------------------------------------
// Sub-user permissions
// ---------------------------------------------------------------------------

/// What the logged-in user may do in this client account
/// (`client_users.permissions`: empty = everything).
#[derive(Debug, Clone, Default)]
pub struct MemberAccess {
    /// Account owner (users, privacy, deletion).
    pub owner: bool,
    /// Granted areas (`services`, `domains`, `invoices`, `orders`, `tickets`,
    /// `profile`, `contacts`); empty = all.
    pub permissions: Vec<String>,
}

/// Areas a sub-user can be granted.
pub const MEMBER_AREAS: &[&str] = &[
    "services", "domains", "invoices", "orders", "tickets", "profile", "contacts",
];

impl MemberAccess {
    /// Whether the user may use `area` (`"owner"`: account owner only).
    pub fn allows(&self, area: &str) -> bool {
        if area == "owner" {
            return self.owner;
        }
        self.owner || self.permissions.is_empty() || self.permissions.iter().any(|p| p == area)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for MemberAccess {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(parts
            .extensions
            .get::<MemberAccess>()
            .cloned()
            .unwrap_or_default())
    }
}

/// The permission area a client path belongs to.
pub fn area_of(path: &str) -> Option<&'static str> {
    let under = |p: &str| path == p || path.starts_with(&format!("{p}/"));
    if under("/client/services") {
        Some("services")
    } else if under("/client/domains") {
        Some("domains")
    } else if under("/client/invoices") || under("/client/billing") {
        Some("invoices")
    } else if under("/client/orders") || under("/client/quotes") {
        Some("orders")
    } else if under("/client/tickets") || under("/client/kb-suggest") {
        Some("tickets")
    } else if under("/client/account/users")
        || under("/client/account/privacy")
        || under("/client/account/api-keys")
        || under("/client/affiliate")
    {
        // API keys act for the whole account: only its owner manages them.
        Some("owner")
    } else if under("/client/account/contacts") {
        Some("contacts")
    } else if under("/client/downloads") {
        // Product downloads come with a service.
        Some("services")
    } else if under("/client/account/ssh-keys") {
        // Keys give root on the account's servers (reinstall, rescue).
        Some("services")
    } else if under("/client/account/billing") {
        Some("profile")
    } else {
        None
    }
}

/// The membership of `user_id` in `client_id` as [`MemberAccess`], read at
/// request time (`None`: not, or no longer, a member). Shared by the client
/// area, the checkout and the REST API gate.
pub async fn load_member_access(
    state: &AppState,
    tenant_id: hos_core::ids::TenantId,
    client_id: hos_core::ids::ClientId,
    user_id: hos_core::ids::UserId,
) -> AppResult<Option<MemberAccess>> {
    Ok(
        clients::client_membership(&state.pool, tenant_id, client_id, user_id)
            .await?
            .map(|m| MemberAccess {
                owner: m.is_owner,
                permissions: m.permissions,
            }),
    )
}

/// Middleware for routes outside `/client` that act for the signed-in
/// client account (`/checkout/*` places orders and spends credit): a client
/// sub-user needs the `area` permission. Guests and staff pass through (the
/// handlers deal with them).
pub async fn require_member_area(
    state: &AppState,
    area: &'static str,
    req: Request,
    next: Next,
) -> Response {
    let tenant = req.extensions().get::<Tenant>().map(|t| t.id);
    let who = req
        .extensions()
        .get::<std::sync::Arc<SessionInfo>>()
        .filter(|s| s.is_authenticated())
        .and_then(|s| s.principal.require_client(tenant?).ok());
    if let (Some(tenant_id), Some((user_id, client_id))) = (tenant, who) {
        match load_member_access(state, tenant_id, client_id, user_id).await {
            Ok(Some(access)) if access.allows(area) => {}
            Ok(Some(_)) => {
                return AppError::forbidden(format!("client area {area} not granted"))
                    .into_response();
            }
            Ok(None) => return AppError::forbidden("not a member of this client").into_response(),
            Err(e) => return e.into_response(),
        }
    }
    next.run(req).await
}

/// Middleware: loads the user's membership of the client account into the
/// request ([`MemberAccess`]) and rejects areas it does not grant (`403`).
pub async fn member_access(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let tenant = req.extensions().get::<Tenant>().map(|t| t.id);
    let who = req
        .extensions()
        .get::<std::sync::Arc<SessionInfo>>()
        .and_then(|s| s.principal.require_client(tenant?).ok());
    let (Some(tenant_id), Some((user_id, client_id))) = (tenant, who) else {
        return next.run(req).await;
    };
    let access = match load_member_access(&state, tenant_id, client_id, user_id).await {
        Ok(Some(access)) => access,
        Ok(None) => return AppError::forbidden("not a member of this client").into_response(),
        Err(e) => return e.into_response(),
    };
    if let Some(area) = area_of(req.uri().path())
        && !access.allows(area)
    {
        return AppError::forbidden(format!("client area {area} not granted")).into_response();
    }
    req.extensions_mut().insert(access);
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Title, breadcrumbs, sidebar and the user's time zone for a client page.
pub fn setup(
    ctx: &mut PageCtx,
    client: &CurrentClient,
    access: &MemberAccess,
    title: impl Into<String>,
    crumbs: Vec<Crumb>,
) {
    if let Some(tz) = client
        .user
        .timezone
        .as_deref()
        .filter(|t| !t.trim().is_empty())
    {
        ctx.timezone = tz.to_owned();
    }
    ctx.set_title(title);
    ctx.panel = Some(menu::chrome(ctx, access, crumbs));
}

/// Formatting helpers shared by client-area templates (bring into scope with
/// `use super::Fmt as _;` next to a template struct).
pub trait Fmt: std::ops::Deref<Target = PageCtx> {
    /// Long date of a calendar day.
    fn day(&self, d: &NaiveDate) -> String {
        billing::day(self, d)
    }
    /// Long date or `—`.
    fn day_opt(&self, d: &Option<NaiveDate>) -> String {
        billing::day_opt(self, d)
    }
    /// Invoice number (or short id of a proforma).
    fn invoice_number(&self, i: &InvoiceRow) -> String {
        billing::invoice_number(i)
    }
    /// Domain name for display (IDN in Unicode).
    fn domain_label(&self, name: &str) -> String {
        hos_domains::name::to_unicode(name)
    }
    /// Markdown written by users, sanitized.
    fn user_md(&self, md: &str) -> String {
        user_markdown(&self.state, md)
    }
    /// Status badge; states without a shared label (`payment_pending`,
    /// `provisioning`, VM states …) use `client.status.*`.
    fn st(&self, status: &str) -> askama::filters::Safe<String> {
        let key = format!("client.status.{status}");
        if self.cat.has(&key) {
            self.badge(self.t(&key), crate::view::status_tone(status_alias(status)))
        } else {
            self.status_badge(status)
        }
    }
}

/// Maps client-only states onto the shared tone table.
fn status_alias(status: &str) -> &str {
    match status {
        "running" | "succeeded" | "accepted" => "active",
        "payment_pending"
        | "provisioning"
        | "pending_registration"
        | "starting"
        | "stopping"
        | "migrating"
        | "paused" => "pending",
        "collections" | "error" => "overdue",
        "stopped" | "transferred_away" | "partially_refunded" | "unknown" => "closed",
        other => other,
    }
}

impl<T: std::ops::Deref<Target = PageCtx>> Fmt for T {}

/// Parses an id from the path; malformed ids are a `404`.
pub fn parse_id<T: FromStr>(raw: &str) -> AppResult<T> {
    raw.parse().map_err(|_| AppError::NotFound)
}

/// A message for a failed business operation that may be shown to the
/// client (`None` for server-side failures, which propagate as `5xx`).
pub fn friendly_error(ctx: &PageCtx, err: &CoreError) -> Option<String> {
    match err {
        CoreError::Integration(IntegrationError::Unsupported(_)) => {
            Some(ctx.t("client.errors.unsupported"))
        }
        CoreError::Integration(IntegrationError::Timeout)
        | CoreError::Integration(IntegrationError::Transport(_))
        | CoreError::Integration(IntegrationError::RateLimited { .. }) => {
            Some(ctx.t("client.errors.provider_unavailable"))
        }
        CoreError::Integration(IntegrationError::InvalidInput(m))
        | CoreError::Integration(IntegrationError::Conflict(m))
        | CoreError::Validation(m)
        | CoreError::Conflict(m) => Some(ctx.tv("client.errors.rejected", "detail", m)),
        CoreError::Forbidden(m) => Some(ctx.tv("client.errors.not_allowed", "detail", m)),
        CoreError::NotFound { .. } => Some(ctx.t("client.errors.not_found")),
        CoreError::InvalidTransition { .. } => Some(ctx.t("client.errors.state")),
        CoreError::Integration(_) => Some(ctx.t("client.errors.provider_failed")),
        _ => None,
    }
}

/// Redirects back with the outcome of a business call: `ok` as a success
/// toast, a user-facing failure as an error toast; server failures propagate.
pub fn outcome<T>(
    ctx: &PageCtx,
    to: &str,
    res: Result<T, CoreError>,
    ok: impl FnOnce(T) -> String,
) -> AppResult<Response> {
    match res {
        Ok(v) => Ok(flash::redirect(to, SetFlash::success(ok(v)))),
        Err(e) => match friendly_error(ctx, &e) {
            Some(msg) => Ok(flash::redirect(to, SetFlash::error(msg))),
            None => Err(e.into()),
        },
    }
}

/// `Cache-Control: no-store` (pages showing secrets, carts, payment hand-offs).
pub fn no_store(mut res: Response) -> Response {
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// Sets this response's Content-Security-Policy: the global policy
/// ([`crate::middleware::CSP`]) with directives replaced / added
/// (`("form-action", "'self' https:")`). `security_headers` keeps a CSP
/// that a handler already set.
pub fn with_csp(state: &AppState, mut res: Response, overrides: &[(&str, String)]) -> Response {
    let mut directives: Vec<(String, String)> = crate::middleware::CSP
        .split(';')
        .filter_map(|d| {
            let d = d.trim();
            let (name, value) = d.split_once(' ').unwrap_or((d, ""));
            (!name.is_empty()).then(|| (name.to_owned(), value.trim().to_owned()))
        })
        .collect();
    for (name, value) in overrides {
        match directives.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value.clone(),
            None => directives.push(((*name).to_owned(), value.clone())),
        }
    }
    let mut csp = directives
        .iter()
        .map(|(n, v)| format!("{n} {v}"))
        .collect::<Vec<_>>()
        .join("; ");
    if state.web.secure {
        csp.push_str("; upgrade-insecure-requests");
    }
    if let Ok(v) = HeaderValue::from_str(&csp) {
        res.headers_mut().insert("content-security-policy", v);
    }
    res
}

/// The CSP override for pages whose forms start a hosted payment: the
/// gateway answers with a redirect to its own (https) origin.
pub fn payment_form_csp(state: &AppState) -> Vec<(&'static str, String)> {
    let base = state.config.base_url.origin().ascii_serialization();
    vec![("form-action", format!("'self' {base} https:"))]
}

/// Writes a client-area audit entry (actor: the user; the staff member
/// behind an impersonated session is recorded as impersonator).
pub async fn audit_client(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    ip: ClientIp,
    ua: &UserAgent,
    action: &str,
    entity: Option<(&str, uuid::Uuid)>,
) {
    let mut entry = AuditEntry::new(
        Some(tenant.id),
        ActorKind::User,
        Some(client.user_id.into_uuid()),
        action,
    )
    .request(Some(ip.0), ua.0.as_deref())
    .metadata(serde_json::json!({ "client_id": client.client_id }))
    .impersonator(
        client
            .session
            .principal
            .impersonator()
            .map(|i| i.staff_user_id.into_uuid()),
    );
    if let Some((kind, id)) = entity {
        entry = entry.entity(kind, id);
    }
    if let Err(e) = audit::write(&state.pool, &entry).await {
        tracing::warn!(error = %e, action, "client audit entry failed");
    }
}

/// Markdown written by users (tickets) → HTML: raw HTML is shown as text,
/// unsafe URLs are neutralised, images are links (never loaded) and every
/// link gets `rel="nofollow noopener"`. In the public demo (`crate::demo`)
/// there are no links at all: visitors read each other's text there.
pub fn user_markdown(state: &AppState, md: &str) -> String {
    if state.web.demo.is_some() {
        return crate::markdown::render_without_links(md);
    }
    crate::markdown::render_without_images(md)
        .replace("<a rel=\"noopener\" href=", "<a href=")
        .replace(
            "<a href=",
            "<a rel=\"nofollow noopener\" target=\"_blank\" href=",
        )
}

/// Whether the session is a staff member impersonating the client: then
/// credentials (password, e-mail, 2FA, OAuth links, API keys) are read-only.
pub fn credentials_locked(client: &CurrentClient) -> bool {
    !client.session.may_change_credentials()
}

/// `403` while impersonating (credential changes).
pub fn require_own_session(client: &CurrentClient) -> AppResult<()> {
    if credentials_locked(client) {
        return Err(AppError::forbidden(
            "credential changes are disabled while impersonating",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

/// A dashboard row: upcoming renewal / expiring domain.
pub struct Upcoming {
    /// Label.
    pub label: String,
    /// Link.
    pub href: String,
    /// Due / expiry date.
    pub date: NaiveDate,
    /// Amount (none for domains without a price).
    pub amount: Option<String>,
    /// Days left.
    pub days: i64,
}

#[derive(Template)]
#[template(path = "client/dashboard.html")]
struct DashboardPage {
    ctx: PageCtx,
    access: MemberAccess,
    summary: ClientSummary,
    first_name: String,
    email_verified: bool,
    services: Vec<ServiceListRow>,
    invoices: Vec<InvoiceRow>,
    renewals: Vec<Upcoming>,
    expiring: Vec<DomainRow>,
    tickets: Vec<TicketListRow>,
    announcements: Vec<content::AnnouncementRow>,
    /// Open incidents / maintenance affecting this client's services.
    notices: Vec<(hos_db::repo::status::IncidentRow, Vec<String>)>,
    today: NaiveDate,
}
crate::page!(DashboardPage);

impl DashboardPage {
    fn notice_status(&self, s: &str) -> String {
        self.ctx.t(&format!("netstatus.incident.{s}"))
    }

    fn days_left(&self, d: &DomainRow) -> i64 {
        d.expires_at
            .map(|e| (e.date_naive() - self.today).num_days())
            .unwrap_or(0)
    }
}

async fn dashboard(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let pool = &state.pool;
    let summary = stats::client_summary(pool, tenant.id, client.client_id).await?;
    let today = Utc::now().date_naive();
    let all_services = if access.allows("services") {
        service_repo::client_services(pool, tenant.id, client.client_id).await?
    } else {
        Vec::new()
    };
    let mut renewals: Vec<Upcoming> = all_services
        .iter()
        .filter(|s| s.service.status == ServiceStatus::Active)
        .filter_map(|s| {
            let due = s.service.next_due_date?;
            let days = (due - today).num_days();
            (s.service.billing_cycle.is_recurring() && (0..=45).contains(&days)).then(|| Upcoming {
                label: s.product_name.clone(),
                href: ctx.url(format!("/client/services/{}", s.service.id)),
                date: due,
                amount: Some(ctx.money_in(&s.service.recurring_amount, &s.service.currency)),
                days,
            })
        })
        .collect();
    let services: Vec<ServiceListRow> = all_services
        .into_iter()
        .filter(|s| {
            matches!(
                s.service.status,
                ServiceStatus::Active
                    | ServiceStatus::Pending
                    | ServiceStatus::Provisioning
                    | ServiceStatus::Suspended
            )
        })
        .take(6)
        .collect();
    let expiring = if access.allows("domains") {
        let list = domain_repo::client_domains(pool, tenant.id, client.client_id).await?;
        for d in &list {
            if let (Some(due), true) = (d.next_due_date, d.auto_renew)
                && d.status == DomainStatus::Active
            {
                let days = (due - today).num_days();
                if (0..=45).contains(&days) {
                    renewals.push(Upcoming {
                        label: hos_domains::name::to_unicode(&d.name),
                        href: ctx.url(format!("/client/domains/{}", d.id)),
                        date: due,
                        amount: Some(ctx.money_in(&d.recurring_amount, &d.currency)),
                        days,
                    });
                }
            }
        }
        list.into_iter()
            .filter(|d| {
                matches!(d.status, DomainStatus::Active | DomainStatus::Grace)
                    && d.expires_at.is_some_and(|e| {
                        let days = (e.date_naive() - today).num_days();
                        (0..=60).contains(&days)
                    })
            })
            .take(5)
            .collect()
    } else {
        Vec::new()
    };
    renewals.sort_by_key(|r| r.date);
    renewals.truncate(6);
    let invoices = if access.allows("invoices") {
        let mut open = Vec::new();
        for status in [
            InvoiceStatus::Unpaid,
            InvoiceStatus::PaymentPending,
            InvoiceStatus::Collections,
        ] {
            let page = invoices::list_invoices(
                pool,
                tenant.id,
                &InvoiceFilter {
                    client_id: Some(client.client_id),
                    status: Some(status),
                    ..InvoiceFilter::default()
                },
                Page::first(5),
            )
            .await?;
            open.extend(page.items.into_iter().map(|r| r.invoice));
        }
        open.sort_by_key(|i| i.due_date);
        open.truncate(5);
        open
    } else {
        Vec::new()
    };
    let tickets = if access.allows("tickets") {
        support_repo::list_tickets(
            pool,
            tenant.id,
            &TicketFilter {
                client_id: Some(client.client_id),
                open_only: true,
                ..TicketFilter::default()
            },
            Page::first(5),
        )
        .await?
        .items
    } else {
        Vec::new()
    };
    let announcements =
        content::published_announcements(pool, tenant.id, ctx.locale, Page::first(3))
            .await?
            .items;
    let notices = if access.allows("services") {
        hos_db::repo::status::incidents_for_client(pool, tenant.id, client.client_id).await?
    } else {
        Vec::new()
    };
    let title = ctx.t("client.dashboard.title");
    setup(&mut ctx, &client, &access, title, Vec::new());
    render(&DashboardPage {
        first_name: client.user.first_name.clone(),
        email_verified: client.user.email_verified_at.is_some(),
        ctx,
        access,
        summary,
        services,
        invoices,
        renewals,
        expiring,
        tickets,
        announcements,
        notices,
        today,
    })
}

// ---------------------------------------------------------------------------
// Impersonation
// ---------------------------------------------------------------------------

/// `POST /client/impersonation/end`: ends a staff member's "log in as client"
/// session and returns to the admin area (whose login page takes over when
/// the staff session is gone).
async fn end_impersonation(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
) -> AppResult<Response> {
    if !client.session.principal.is_impersonated() {
        return Ok(Redirect::to(&ctx.url("/client")).into_response());
    }
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.impersonation.ended",
        Some(("client", client.client_id.into_uuid())),
    )
    .await;
    let cookie = crate::session::end_session(&state, &client.session).await?;
    let mut res = Redirect::to(&format!("/admin/clients/{}", client.client_id)).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(SET_COOKIE, v);
    }
    Ok(res)
}

/// Locale of the request for e-mails / exports.
pub fn locale_of(ctx: &PageCtx) -> Locale {
    ctx.locale
}

/// `422` re-render shorthand.
pub fn unprocessable<T: Template>(page: &T) -> AppResult<Response> {
    crate::view::render_status(StatusCode::UNPROCESSABLE_ENTITY, page)
}
