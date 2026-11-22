//! Public demo mode (`HOS_DEMO_MODE=1`). The design, every block and the
//! residual risks are written down in `docs/DEMO.md`; keep both in sync.
//!
//! * [`DemoConfig`] — read once at startup ([`DemoConfig::from_env`]); off
//!   unless `HOS_DEMO_MODE` is exactly truthy. Independent of `HOS_DEBUG`
//!   (the server refuses to start with both).
//! * [`verdict`] — which requests are refused (pure function, unit-tested).
//! * [`layer`] (whole application) — the reset gate, the per-IP request /
//!   write / expensive-page limits, `X-Robots-Tag`, and the blocks of the
//!   tenantless endpoints (inbound mail webhooks).
//! * [`guard`] (tenant stack, after the session) — refuses blocked requests
//!   with a localized page, toast or flash, protects the shared demo logins
//!   and greets visitors the reset caught mid-way ([`GEN_COOKIE`]).
//! * [`reset`] / [`spawn`] — deletes the demo tenant and seeds it again, at
//!   startup and every `HOS_DEMO_RESET_MINUTES` (default 30); `hostingos
//!   demo-reset` runs it from another process ([`follow_external_reset`]).
//! * [`check_database`] — refuses a database that holds anything else.
//! * `/demo/mailbox` ([`router`]) — the e-mails the demo "sent" (nothing
//!   leaves the server: the process is offline, `hos_core::net::go_offline`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use askama::Template;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use hos_auth::ratelimit::{RateDecision, RateLimit, RateLimiter};
use hos_core::crypto::integration_aad;
use hos_core::domain::{IntegrationCategory, Locale, ProductType, UserKind};
use hos_core::ids::{IntegrationId, TenantId};
use hos_core::money::Currency;
use hos_db::ids::EmailLogId;
use hos_db::repo::{catalog, clients, domains, identity, integrations, platform};
use hos_db::{Page, PgPool};
use http::{HeaderValue, Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, Htmx, MaybeUser};
use crate::flash::{self, SetFlash};
use crate::state::AppState;
use crate::tenant::{RequestHost, Tenant, path_without_locale};
use crate::view::{PageCtx, render};
use crate::worker::ShutdownSignal;

/// Switches demo mode on (`1`, `true`, `yes`, `on`).
pub const MODE_ENV: &str = "HOS_DEMO_MODE";
/// Minutes between two resets (5–1440, default 30).
pub const RESET_MINUTES_ENV: &str = "HOS_DEMO_RESET_MINUTES";
/// Starts demo mode on a database that also holds other tenants or platform
/// operators. Only for a deliberate, throw-away setup.
pub const ALLOW_EXISTING_ENV: &str = "HOS_DEMO_ALLOW_EXISTING_DATA";
/// Default reset interval.
pub const DEFAULT_RESET_MINUTES: u64 = 30;

/// File types a demo visitor may upload (ticket attachments, downloads):
/// raster images only. Outside the demo PDF, text and archives are accepted
/// too; SVG, HTML and scripts never are. Brand images have their own,
/// stricter check (`sniff_image`: raster formats with dimension limits).
pub const ALLOWED_UPLOAD_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// How long the reset waits for running requests; then it resets anyway
/// (new requests are held back throughout).
const GATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Retry delay after a failed reset.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// Whether a sniffed upload type may be stored: in the demo only raster
/// images (no documents or archives other visitors could download).
pub fn upload_type_allowed(state: &AppState, content_type: &str) -> bool {
    state.web.demo.is_none() || demo_upload_type(content_type)
}

fn demo_upload_type(content_type: &str) -> bool {
    let base = content_type.split(';').next().unwrap_or("").trim();
    ALLOWED_UPLOAD_TYPES.contains(&base)
}

fn flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// A documented demo login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoAccount {
    /// Login e-mail.
    pub email: String,
    /// Clear-text password (public: shown on the login pages).
    pub password: String,
}

impl DemoAccount {
    fn new(email: &str, password: &str) -> Self {
        Self {
            email: email.to_owned(),
            password: password.to_owned(),
        }
    }
}

/// Demo settings (`HOS_DEMO_*`).
#[derive(Debug, Clone)]
pub struct DemoConfig {
    /// Slug of the demo tenant (`demo`; tests use throw-away slugs).
    pub tenant_slug: String,
    /// Time between two resets.
    pub reset_every: Duration,
    /// `HOS_DEMO_ALLOW_EXISTING_DATA`: skip [`check_database`].
    pub allow_existing_data: bool,
    /// Tenant owner (staff, `/admin`).
    pub staff: DemoAccount,
    /// Client (`/client`).
    pub client: DemoAccount,
    /// The operator-console account of earlier demo versions (no longer
    /// created; removed at every reset).
    pub platform: DemoAccount,
}

impl DemoConfig {
    /// The documented demo (docs/DEMO.md): tenant `demo`, reset every 30
    /// minutes.
    pub fn standard() -> Self {
        Self {
            tenant_slug: hos_db::seed::DEMO_TENANT_SLUG.to_owned(),
            reset_every: Duration::from_secs(DEFAULT_RESET_MINUTES * 60),
            allow_existing_data: false,
            staff: DemoAccount::new(crate::seed_demo::STAFF_EMAIL, "demo-admin"),
            client: DemoAccount::new(crate::seed_demo::CLIENT_EMAIL, "demo-client"),
            platform: DemoAccount::new(crate::seed_demo::PLATFORM_EMAIL, "demo-platform"),
        }
    }

    /// `Some` when `HOS_DEMO_MODE` is truthy. An out-of-range
    /// `HOS_DEMO_RESET_MINUTES` keeps the default (logged).
    pub fn from_env() -> Option<Self> {
        if !flag(MODE_ENV) {
            return None;
        }
        let mut cfg = Self::standard();
        if let Ok(raw) = std::env::var(RESET_MINUTES_ENV)
            && !raw.trim().is_empty()
        {
            match raw.trim().parse::<u64>() {
                Ok(m) if (5..=1440).contains(&m) => cfg.reset_every = Duration::from_secs(m * 60),
                _ => tracing::warn!(
                    value = %raw.chars().take(20).collect::<String>(),
                    "{RESET_MINUTES_ENV} must be a number of minutes between 5 and 1440; using {DEFAULT_RESET_MINUTES}"
                ),
            }
        }
        cfg.allow_existing_data = flag(ALLOW_EXISTING_ENV);
        Some(cfg)
    }

    /// The reset interval in whole minutes.
    pub fn reset_minutes(&self) -> u64 {
        (self.reset_every.as_secs() / 60).max(1)
    }

    /// Whether `email` is the demo staff or client login (no lockout, no
    /// changes to e-mail, password or 2FA).
    pub fn is_protected_login(&self, email: &str) -> bool {
        let email = email.trim();
        self.staff.email.eq_ignore_ascii_case(email)
            || self.client.email.eq_ignore_ascii_case(email)
    }
}

/// Whether the demo-protected login `email` of tenant `slug` is exempt from
/// the per-account lockout (anyone could otherwise lock every visitor out by
/// typing wrong passwords). Per-IP rate limits still apply.
pub fn lockout_exempt(state: &AppState, tenant_slug: &str, email: &str) -> bool {
    state
        .web
        .demo
        .as_ref()
        .is_some_and(|c| c.tenant_slug == tenant_slug && c.is_protected_login(email))
}

/// [`lockout_exempt`] for the platform login.
pub fn platform_lockout_exempt(state: &AppState, email: &str) -> bool {
    state
        .web
        .demo
        .as_ref()
        .is_some_and(|c| c.platform.email.eq_ignore_ascii_case(email.trim()))
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

/// Argon2 hashes of the demo passwords (computed once per process).
#[derive(Debug, Clone)]
struct Hashes {
    staff: String,
    client: String,
}

/// The reset gate. A request passes `admit` (briefly) and holds `running`
/// while it is handled; a reset closes `admit`, so new requests wait, and
/// then waits — at most a given time — until `running` is free. Requests
/// still running then (stragglers) keep running while the demo is reset
/// under them; the reset never waits for them indefinitely.
#[derive(Debug, Default)]
struct Gate {
    admit: tokio::sync::RwLock<()>,
    running: tokio::sync::RwLock<()>,
}

/// A closed [`Gate`]: new requests wait until it is dropped.
struct Closed<'a> {
    _admit: tokio::sync::RwLockWriteGuard<'a, ()>,
    running: Option<tokio::sync::RwLockWriteGuard<'a, ()>>,
}

impl Closed<'_> {
    /// Requests were still running when the wait ended.
    fn stragglers(&self) -> bool {
        self.running.is_none()
    }
}

impl Gate {
    /// Waits while a reset runs; the guard marks the request as running.
    async fn enter(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        let _admitted = self.admit.read().await;
        self.running.read().await
    }

    /// Holds new requests back and waits up to `wait` for running ones.
    async fn close(&self, wait: Duration) -> Closed<'_> {
        let admit = self.admit.write().await;
        // Every admitted request holds `running` by now: `enter` keeps
        // `admit` until it has it.
        let running = tokio::time::timeout(wait, self.running.write()).await.ok();
        Closed {
            _admit: admit,
            running,
        }
    }
}

/// Per-process demo state (part of `AppState`; idle unless demo mode is on).
#[derive(Debug)]
pub struct DemoRuntime {
    /// No request starts while the demo is being reset, and the reset waits
    /// for running requests (bounded, [`GATE_TIMEOUT`]).
    gate: Gate,
    /// Unix time of the next reset (0 = unknown).
    next_reset: AtomicI64,
    /// Identifies the data set visitors see: the time (ms) the last reset
    /// finished. Browsers carry the value they saw ([`GEN_COOKIE`]); a stale
    /// one means the demo was reset under them ([`guard`]).
    generation: AtomicI64,
    hashes: OnceLock<Hashes>,
    /// Every request per IP.
    requests: RateLimiter,
    /// State-changing requests per IP.
    writes: RateLimiter,
    /// PDF / CSV / report / search / image pages per IP.
    heavy: RateLimiter,
}

impl Default for DemoRuntime {
    fn default() -> Self {
        Self {
            gate: Gate::default(),
            next_reset: AtomicI64::new(0),
            generation: AtomicI64::new(0),
            hashes: OnceLock::new(),
            requests: RateLimiter::new(RateLimit::new(240, Duration::from_secs(60))),
            writes: RateLimiter::new(RateLimit::new(60, Duration::from_secs(600))),
            heavy: RateLimiter::new(RateLimit::new(20, Duration::from_secs(300))),
        }
    }
}

impl DemoRuntime {
    /// Unix time of the next scheduled reset, if known.
    pub fn next_reset(&self) -> Option<i64> {
        Some(self.next_reset.load(Ordering::Relaxed)).filter(|t| *t > 0)
    }

    /// Whether requests are being handled right now (they hold the reset
    /// gate; a request still sending its body does not).
    pub fn handling_requests(&self) -> bool {
        self.gate.running.try_write().is_err()
    }

    /// The current data generation (0 before the first reset).
    pub fn generation(&self) -> i64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn schedule_in(&self, after: Duration) {
        let at = Utc::now().timestamp() + i64::try_from(after.as_secs()).unwrap_or(i64::MAX / 2);
        self.next_reset.store(at, Ordering::Relaxed);
    }

    fn until_next_reset(&self) -> Duration {
        let left = self.next_reset.load(Ordering::Relaxed) - Utc::now().timestamp();
        Duration::from_secs(u64::try_from(left).unwrap_or(0))
    }

    async fn hashes(&self, cfg: &DemoConfig) -> anyhow::Result<Hashes> {
        if let Some(h) = self.hashes.get() {
            return Ok(h.clone());
        }
        let passwords = [cfg.staff.password.clone(), cfg.client.password.clone()];
        let [staff, client] = tokio::task::spawn_blocking(move || {
            passwords.map(|p| hos_auth::password::hash_password(&p))
        })
        .await
        .context("hashing task")?;
        let hashes = Hashes {
            staff: staff.map_err(|e| anyhow::anyhow!("hashing: {e}"))?,
            client: client.map_err(|e| anyhow::anyhow!("hashing: {e}"))?,
        };
        Ok(self.hashes.get_or_init(|| hashes).clone())
    }
}

/// Masks a visitor address before it is stored or shown (audit log, session
/// list, login history): IPv4 to its `/24`, IPv6 to its `/48`. Demo visitors
/// share accounts, so they would otherwise see each other's addresses.
pub fn mask_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            IpAddr::V4(Ipv4Addr::new(a, b, c, 0))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => mask_ip(IpAddr::V4(v4)),
            None => {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], 0, 0, 0, 0, 0))
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Request policy
// ---------------------------------------------------------------------------

/// What demo mode does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Served normally.
    Allow,
    /// Refused ("disabled in the demo").
    Block,
}

/// Refused whatever the method: real outbound traffic, sign-up and account
/// recovery (the demo logins are shared), imports, update management,
/// environment details, tenant creation.
const ALWAYS_BLOCKED: &[&str] = &[
    "/auth/register",
    "/auth/forgot",
    "/auth/reset",
    "/auth/oauth/**",
    "/whois",
    "/webhooks/mail/**",
    "/admin/tools/import/**",
    "/admin/_styleguide",
    "/platform/jobs/**",
    "/platform/signup/**",
    "/platform/handoff",
];

/// State-changing requests that stay allowed although a broader rule below
/// would refuse them.
const WRITE_ALLOWED: &[&str] = &[
    "/platform/login",
    "/platform/login/2fa",
    "/platform/logout",
    // Renders a preview for the requesting session only / sends to the
    // captured mailbox.
    "/admin/settings/email/templates/*/*/preview",
    "/admin/settings/email/templates/*/*/test",
];

/// State-changing requests refused in the demo. Everything else — products,
/// clients, orders, invoices, tickets, CMS pages, appearance … — is open to
/// visitors (docs/DEMO.md): it goes through the normal validation and
/// sanitising and is deleted by the next reset.
const WRITE_BLOCKED: &[&str] = &[
    // The platform console is read-only (it could suspend or delete the
    // demo tenant for everyone).
    "/platform/**",
    // The REST API is read-only: keys are bearer credentials for scripts.
    "/api/**",
    // The shared demo logins: no 2FA, password, linked identity, session
    // or account deletion.
    "/auth/2fa/setup",
    "/auth/2fa/disable",
    "/client/account/password",
    "/client/account/identities/**",
    "/client/account/sessions/**",
    "/client/account/privacy/delete",
    // Policies that would lock every visitor out (mandatory staff 2FA,
    // session lifetimes) or contradict the closed sign-up.
    "/admin/settings/security",
    "/admin/settings/registration",
    // Raw HTML (e-mail templates; preview and test stay allowed above).
    "/admin/settings/email/templates/**",
    // Real integrations and anything that reaches other hosts: the mocks
    // are preconfigured (testing them stays allowed), outgoing webhooks,
    // custom-domain DNS checks, e-invoice providers, inbound mail, AI
    // providers, IndexNow pings.
    "/admin/integrations",
    "/admin/integrations/*",
    "/admin/integrations/*/delete",
    "/admin/developer/webhooks/**",
    "/admin/settings/domains/**",
    "/admin/settings/einvoice",
    "/admin/support/inbound/**",
    "/admin/support/tickets/*/ai-draft",
    "/admin/ai/**",
    "/admin/seo/indexnow/**",
];

/// Segment pattern match: `*` is one segment, a trailing `**` the rest
/// (also nothing).
fn seg_match(pattern: &str, path: &str) -> bool {
    let mut pattern = pattern.trim_matches('/').split('/');
    let mut path = path.trim_matches('/').split('/');
    loop {
        match (pattern.next(), path.next()) {
            (Some("**"), _) => return true,
            (Some(p), Some(s)) => {
                if p != "*" && p != s {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn is_write(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The demo policy for `method` + `path` (with or without a locale prefix).
pub fn verdict(method: &Method, path: &str) -> Verdict {
    let path = path_without_locale(path);
    if ALWAYS_BLOCKED.iter().any(|p| seg_match(p, path)) {
        return Verdict::Block;
    }
    if !is_write(method) || WRITE_ALLOWED.iter().any(|p| seg_match(p, path)) {
        return Verdict::Allow;
    }
    if WRITE_BLOCKED.iter().any(|p| seg_match(p, path)) {
        return Verdict::Block;
    }
    Verdict::Allow
}

/// PDF, CSV, print, export, report, search and generated-image pages: the
/// ones that cost real CPU per request.
pub fn is_heavy(path: &str) -> bool {
    let path = path_without_locale(path);
    path.ends_with(".pdf")
        || path.ends_with("/pdf")
        || path.ends_with("/ubl")
        || path.ends_with("/print")
        || path.ends_with(".csv")
        || path.ends_with("/export")
        || path.ends_with("report.json")
        || path.starts_with("/og/")
        || path.starts_with("/admin/reports")
        || path.starts_with("/admin/search")
        || path.starts_with("/admin/dashboard/kpis")
        || path.starts_with("/kb/search")
        || path.starts_with("/domains/check")
}

/// Paths that are not metered or gated: assets and health checks.
fn is_infrastructure(path: &str) -> bool {
    path.starts_with("/static/")
        || path.starts_with("/assets/")
        || path == "/healthz"
        || path == "/readyz"
        || path == "/favicon.ico"
        || path == "/brand.css"
}

fn too_many(retry: Duration) -> Response {
    AppError::RateLimited {
        retry_after: Some(retry),
    }
    .into_response()
}

/// Middleware around the whole application (demo mode only): the reset
/// gate, per-IP limits, `X-Robots-Tag` and the tenantless blocks.
pub async fn layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if state.web.demo.is_none() {
        return next.run(req).await;
    }
    let path = req.uri().path().to_owned();
    if is_infrastructure(&path) {
        return with_robots(next.run(req).await);
    }
    let (parts, body) = req.into_parts();
    let ip = ClientIp::from_config(&parts, &state.web);
    let method = parts.method.clone();
    let req = Request::from_parts(parts, body);
    let mut checks = vec![(&state.demo.requests, "demo-requests")];
    if is_write(&method) {
        checks.push((&state.demo.writes, "demo-writes"));
    }
    if is_heavy(&path) {
        checks.push((&state.demo.heavy, "demo-heavy"));
    }
    for (limiter, action) in checks {
        if let RateDecision::Limited { retry_after } = limiter.check(&ip.key(action)) {
            return with_robots(too_many(retry_after));
        }
    }
    // Tenantless endpoints never reach `guard`.
    if path.starts_with("/webhooks/") && verdict(&method, &path) == Verdict::Block {
        return with_robots(AppError::DemoDisabled.into_response());
    }
    // The gate is taken only now, for the handling: the body is already in
    // memory (`crate::server::limits` runs before this layer), so a client
    // that trickles its upload cannot hold up a reset. A console websocket
    // lives for minutes; it does not take the gate at all.
    let _gate = if path.ends_with("/console/ws") {
        None
    } else {
        Some(state.demo.gate.enter().await)
    };
    with_robots(next.run(req).await)
}

fn with_robots(mut res: Response) -> Response {
    res.headers_mut().insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow, noarchive"),
    );
    res
}

/// A write aimed at one of the shared demo logins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// `/admin/clients/{id}/status` (suspend / close the demo client).
    Client(Uuid),
    /// `/admin/staff/{id}/…` (deactivate, roles, 2FA reset of the demo admin).
    Staff(Uuid),
}

fn protected_target(path: &str) -> Option<Target> {
    if let Some(rest) = path.strip_prefix("/admin/clients/") {
        let (id, tail) = rest.split_once('/')?;
        return (tail == "status")
            .then(|| id.parse().ok().map(Target::Client))
            .flatten();
    }
    let rest = path.strip_prefix("/admin/staff/")?;
    let id = rest.split('/').next()?;
    id.parse().ok().map(Target::Staff)
}

/// Whether `target` is one of the demo logins (errors count as yes).
async fn is_demo_login(
    state: &AppState,
    cfg: &DemoConfig,
    tenant: TenantId,
    target: Target,
) -> bool {
    match target {
        Target::Client(id) => {
            hos_db::repo::demo::client_has_login(&state.pool, tenant, id, &cfg.client.email)
                .await
                .unwrap_or(true)
        }
        Target::Staff(id) => {
            match identity::user_by_email(&state.pool, tenant, &cfg.staff.email).await {
                Ok(Some(staff)) => Uuid::from(staff.id) == id,
                Ok(None) => false,
                Err(_) => true,
            }
        }
    }
}

/// Cookie with the data generation the browser last saw.
pub const GEN_COOKIE: &str = "hos_demo_gen";

fn seen_generation(headers: &http::HeaderMap) -> Option<i64> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| hos_auth::session::cookie_value(h, GEN_COOKIE))
        .and_then(|v| v.parse().ok())
}

fn set_generation(state: &AppState, res: &mut Response, generation: i64) {
    let cookie = hos_auth::session::build_cookie(
        GEN_COOKIE,
        &generation.to_string(),
        &hos_auth::session::CookieAttributes {
            secure: state.web.secure,
            max_age_secs: Some(7 * 24 * 3600),
            ..hos_auth::session::CookieAttributes::default()
        },
        Utc::now(),
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(http::header::SET_COOKIE, v);
    }
}

/// Middleware in the tenant stack (after the session, inside the error-page
/// renderer and the flash layer, before the CSRF check): refuses what
/// [`verdict`] blocks and changes to the shared demo logins, and handles
/// visitors the reset caught in the middle of something.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(cfg) = state.web.demo.as_ref() else {
        return next.run(req).await;
    };
    let method = req.method().clone();
    let path = path_without_locale(req.uri().path()).to_owned();
    let mut blocked = verdict(&method, &path) == Verdict::Block;
    if !blocked
        && is_write(&method)
        && let Some(target) = protected_target(&path)
        && let Some(tenant) = req.extensions().get::<Tenant>()
    {
        blocked = is_demo_login(&state, cfg, tenant.id, target).await;
    }
    if blocked {
        tracing::info!(%method, path = %path, "demo mode: request refused");
        return refused(&state, req, &method, &path);
    }
    let api = path.starts_with("/api/") || path.starts_with("/webhooks/");
    let current = state.demo.generation();
    let seen = seen_generation(req.headers());
    if api || current == 0 || seen == Some(current) {
        return next.run(req).await;
    }
    let mut res = if seen.is_some() {
        after_reset(&state, cfg, req, next).await
    } else {
        next.run(req).await
    };
    set_generation(&state, &mut res, current);
    res
}

/// The demo was reset since this browser's last request: its session, cart
/// and anything it was editing are gone. A form post is not processed (its
/// CSRF token and ids belong to the deleted data); the visitor goes back to
/// the page with a notice and signs in again from there. A page view is
/// served with the notice.
async fn after_reset(state: &AppState, cfg: &DemoConfig, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let ctx = PageCtx::from_parts(&mut parts, state);
    let notice = SetFlash::info(ctx.tv("demo.reset.notice", "minutes", cfg.reset_minutes()));
    if is_write(&parts.method) {
        let back = same_host_referer(&parts).unwrap_or_else(|| ctx.url("/"));
        if Htmx::from_headers(&parts.headers).request {
            let mut res = (notice, StatusCode::NO_CONTENT).into_response();
            if let Ok(v) = HeaderValue::from_str(&back) {
                res.headers_mut().insert("hx-redirect", v);
            }
            return res;
        }
        return flash::redirect(&back, notice);
    }
    // Shown on this page, or after the redirect a page view may answer with
    // (the old session is gone: the login page).
    if let Some(incoming) = parts.extensions.get_mut::<flash::Flash>() {
        incoming.0.extend(notice.0.iter().cloned());
    }
    let res = next.run(Request::from_parts(parts, body)).await;
    if res.status().is_redirection() {
        (notice, res).into_response()
    } else {
        res
    }
}

/// A refused form post goes back to the page it came from (same host only)
/// with an error flash; everything else gets the 403 page / toast /
/// problem+json from the error renderer.
fn refused(state: &AppState, req: Request, method: &Method, path: &str) -> Response {
    let (mut parts, _) = req.into_parts();
    let api = path.starts_with("/api/") || path.starts_with("/webhooks/");
    if is_write(method)
        && !api
        && !Htmx::from_headers(&parts.headers).request
        && let Some(back) = same_host_referer(&parts)
    {
        let ctx = PageCtx::from_parts(&mut parts, state);
        let mut res = flash::redirect(&back, SetFlash::error(ctx.t("demo.blocked.text")));
        *res.status_mut() = StatusCode::SEE_OTHER;
        return res;
    }
    AppError::DemoDisabled.into_response()
}

/// Path + query of a `Referer` on the request's own host.
fn same_host_referer(parts: &http::request::Parts) -> Option<String> {
    let host = parts.extensions.get::<RequestHost>()?;
    let raw = parts.headers.get(http::header::REFERER)?.to_str().ok()?;
    let url = url::Url::parse(raw).ok()?;
    if url.host_str()?.to_ascii_lowercase() != host.name || url.port() != host.port {
        return None;
    }
    let mut back = url.path().to_owned();
    if let Some(q) = url.query() {
        back.push('?');
        back.push_str(q);
    }
    back.starts_with('/').then_some(back)
}

// ---------------------------------------------------------------------------
// Reset
// ---------------------------------------------------------------------------

/// What one [`reset`] did.
#[derive(Debug, Clone)]
pub struct ResetReport {
    /// The demo tenant.
    pub tenant_id: TenantId,
    /// Wall time of the reset.
    pub took: Duration,
}

/// Tenant settings the demo always starts with: no sign-up (visitors use
/// the demo client), no IndexNow pings.
fn demo_settings() -> [(&'static str, Value); 3] {
    [
        ("auth.allow_registration", json!(false)),
        ("seo.indexnow", json!(false)),
        ("support.allow_guest_tickets", json!(true)),
    ]
}

/// Verified hosts of the demo tenant: the public host of `HOS_BASE_URL`
/// (primary; console and storefront share it) and, for the standard slug,
/// the local development names of `seed-demo`.
fn tenant_hosts(state: &AppState, cfg: &DemoConfig) -> Vec<(String, &'static str, bool)> {
    let mut hosts = Vec::new();
    if let Some(host) = state.config.base_url.host_str()
        && !crate::tenant::is_local_host(host)
        && !host.ends_with(".localhost")
    {
        hosts.push((host.to_ascii_lowercase(), "custom", true));
    }
    if cfg.tenant_slug == hos_db::seed::DEMO_TENANT_SLUG {
        let primary_taken = !hosts.is_empty();
        for &(host, kind, primary) in hos_db::seed::DEMO_TENANT_HOSTS {
            if !hosts.iter().any(|(h, _, _)| h == host) {
                hosts.push((host.to_owned(), kind, primary && !primary_taken));
            }
        }
    }
    hosts
}

/// Restores the demo tenant to the seeded state: deletes it with everything
/// visitors created (cascade), seeds it again under the same id, recreates
/// the demo logins with their documented passwords, restores the platform
/// operator and empties the caches. Keeps new requests waiting until it is
/// done and waits up to 30 s for running ones; requests still running then
/// do not postpone it (a visitor's request could otherwise hold the demo's
/// data past every reset), they finish against whatever data they find.
pub async fn reset(state: &AppState) -> anyhow::Result<ResetReport> {
    let cfg = state.web.demo.as_ref().context("demo mode is off")?;
    let started = Instant::now();
    let hashes = state.demo.hashes(cfg).await?;
    let gate = state.demo.gate.close(GATE_TIMEOUT).await;
    if gate.stragglers() {
        tracing::warn!(
            waited_secs = GATE_TIMEOUT.as_secs(),
            "demo reset: requests were still running; resetting anyway"
        );
    }
    let tenant_id = seed_tenant(state, cfg, &hashes).await?;
    // Outside the transaction: the scheduler and the checkout engine use
    // the pool. Requests are still held back by the gate.
    crate::seed_demo::keep_demo_tld_prices(&state.pool, tenant_id).await?;
    crate::seed_demo::seed_demo_order(&state.pool, &state.config, tenant_id, &cfg.client.email)
        .await?;
    // Last: a server that did not run this reset (`hostingos demo-reset`)
    // notices it by this value ([`follow_external_reset`]).
    let generation = Utc::now().timestamp_millis();
    platform::set_setting(
        &state.pool,
        tenant_id,
        GENERATION_SETTING,
        &json!(generation),
    )
    .await?;
    adopt_generation(state, cfg, tenant_id, generation);
    Ok(ResetReport {
        tenant_id,
        took: started.elapsed(),
    })
}

/// Tenant setting with the generation of the last reset (ms since 1970),
/// written by whichever process ran it.
const GENERATION_SETTING: &str = "demo.generation";
/// How often the server looks for a reset run by another process.
const FOLLOW_EVERY: Duration = Duration::from_secs(30);

/// After a reset (this process's or another's): empties the caches, makes
/// the demo logins usable again, starts the next interval and switches the
/// generation. Called behind the gate when this process resets, so no
/// request sees the new data with the old generation.
fn adopt_generation(state: &AppState, cfg: &DemoConfig, tenant_id: TenantId, generation: i64) {
    state.tenants.invalidate_all();
    state.store_chrome.invalidate_all();
    crate::theme::invalidate(tenant_id);
    state.billing.registry().invalidate_tenant(tenant_id);
    for email in [&cfg.staff.email, &cfg.client.email] {
        state
            .lockout
            .record_success(&format!("{tenant_id}:{email}"));
    }
    state
        .lockout
        .record_success(&format!("platform:{}", cfg.platform.email));
    state.demo.schedule_in(cfg.reset_every);
    state.demo.generation.store(generation, Ordering::Relaxed);
}

/// Picks up a reset another process ran (`hostingos demo-reset`): the
/// stored generation differs from this process's.
pub async fn follow_external_reset(state: &AppState) -> anyhow::Result<bool> {
    let Some(cfg) = state.web.demo.as_ref() else {
        return Ok(false);
    };
    let Some(tenant) = platform::tenant_context_by_slug(&state.pool, &cfg.tenant_slug).await?
    else {
        return Ok(false);
    };
    let stored = platform::get_setting(&state.pool, tenant.id, GENERATION_SETTING)
        .await?
        .and_then(|v| v.as_i64());
    match stored {
        Some(generation) if generation != state.demo.generation() => {
            // Running requests finish first (bounded, as in `reset`).
            let _gate = state.demo.gate.close(GATE_TIMEOUT).await;
            adopt_generation(state, cfg, tenant.id, generation);
            tracing::info!("demo reset by another process (hostingos demo-reset) picked up");
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// The transactional part of [`reset`].
async fn seed_tenant(
    state: &AppState,
    cfg: &DemoConfig,
    hashes: &Hashes,
) -> anyhow::Result<TenantId> {
    let mut tx = state.pool.begin().await?;
    hos_db::repo::demo::lock_reset(&mut tx).await?;
    let previous = hos_db::repo::demo::delete_tenant_by_slug(&mut *tx, &cfg.tenant_slug).await?;
    let hosts = tenant_hosts(state, cfg);
    let hosts: Vec<(&str, &str, bool)> =
        hosts.iter().map(|(h, k, p)| (h.as_str(), *k, *p)).collect();
    let report = hos_db::seed::seed_demo_tenant(
        &mut tx,
        &hos_db::seed::DemoTenant {
            slug: &cfg.tenant_slug,
            id: previous,
            hosts: &hosts,
        },
    )
    .await
    .context("seeding the demo tenant")?;
    let tenant_id = report.tenant_id.context("seed returned no tenant")?;
    for (key, value) in demo_settings() {
        platform::set_setting(&mut *tx, tenant_id, key, &value).await?;
    }
    // Self-service tenant sign-up stays closed through the request policy
    // (`/platform/signup` is refused), not through a platform setting.

    // Logins.
    let owner = identity::role_by_slug(&mut *tx, tenant_id, "owner")
        .await?
        .context("owner role missing")?;
    let staff = identity::create_user(
        &mut *tx,
        tenant_id,
        &identity::NewUser {
            email: cfg.staff.email.clone(),
            password_hash: Some(hashes.staff.clone()),
            kind: UserKind::Staff,
            first_name: "Demo".into(),
            last_name: "Yönetici".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await?;
    identity::assign_role(&mut *tx, tenant_id, staff.id, owner.id).await?;
    let client_user = identity::create_user(
        &mut *tx,
        tenant_id,
        &identity::NewUser {
            email: cfg.client.email.clone(),
            password_hash: Some(hashes.client.clone()),
            kind: UserKind::Client,
            first_name: "Ayşe".into(),
            last_name: "Yılmaz".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await?;
    clients::create_client_with_owner(
        &mut tx,
        tenant_id,
        &clients::ClientInput::individual("Ayşe", "Yılmaz", &cfg.client.email, Currency::TRY),
        client_user.id,
    )
    .await?;
    // The demo shows one hosting company (single-company mode): no
    // operator console account. Earlier demo versions had one.
    hos_db::repo::demo::delete_platform_admin(&mut *tx, &cfg.platform.email).await?;

    create_mock_integrations(state, &mut tx, tenant_id).await?;
    crate::seed_demo::seed_blog(&mut tx, tenant_id).await?;
    tx.commit().await?;
    Ok(tenant_id)
}

/// The mock hypervisor, panel, registrar and gateway, linked to the seeded
/// products and TLDs.
async fn create_mock_integrations(
    state: &AppState,
    tx: &mut sqlx::PgConnection,
    tenant_id: TenantId,
) -> anyhow::Result<()> {
    let specs = [
        (
            IntegrationCategory::Virt,
            hos_virt::MOCK,
            "Mock sanallaştırma",
            // The mock keeps VMs in memory per built instance; the registry
            // rebuilds integrations (cache TTL) and the process restarts, so
            // a VM it does not know is re-created as running.
            json!({"adopt_unknown_vms": true}),
            json!({}),
        ),
        (
            IntegrationCategory::Panel,
            hos_panels::MOCK,
            "Mock hosting paneli",
            json!({"packages": crate::seed_demo::demo_panel_packages()}),
            json!({}),
        ),
        (
            IntegrationCategory::Registrar,
            hos_domains::mock::KIND,
            "Mock kayıt firması",
            json!({"currency": "USD", "default_nameservers": "ns1.demo.test, ns2.demo.test"}),
            json!({}),
        ),
        (
            IntegrationCategory::Gateway,
            hos_payments::MOCK,
            "Mock ödeme sağlayıcısı",
            json!({}),
            json!({"webhook_secret": hos_core::crypto::generate_token()}),
        ),
    ];
    let mut ids: Vec<(IntegrationCategory, IntegrationId)> = Vec::new();
    for (category, kind, name, config, secrets) in specs {
        let id = IntegrationId::new();
        let sealed = state
            .secret_box
            .encrypt_json(&secrets, &integration_aad(tenant_id, id))
            .map_err(|e| anyhow::anyhow!("encrypting integration secrets: {e}"))?;
        integrations::create_integration(
            &mut *tx,
            tenant_id,
            &integrations::NewIntegration {
                id,
                category,
                kind: kind.to_owned(),
                name: name.to_owned(),
                config,
                secrets_enc: Some(sealed),
                is_active: true,
                sort_order: 0,
            },
        )
        .await?;
        ids.push((category, id));
    }
    let id_of = |c: IntegrationCategory| ids.iter().find(|(cat, _)| *cat == c).map(|(_, id)| *id);
    for (product, kind) in
        hos_db::repo::demo::products_without_integration(&mut *tx, tenant_id).await?
    {
        let target = match kind {
            ProductType::SharedHosting | ProductType::ResellerHosting | ProductType::Email => {
                id_of(IntegrationCategory::Panel)
            }
            ProductType::Vps
            | ProductType::Dedicated
            | ProductType::Cloud
            | ProductType::ManagedDb
            | ProductType::ObjectStorage
            | ProductType::ContainerApp => id_of(IntegrationCategory::Virt),
            _ => None,
        };
        if let Some(integration) = target {
            catalog::set_product_integration(&mut *tx, tenant_id, product, Some(integration))
                .await?;
        }
    }
    if let Some(registrar) = id_of(IntegrationCategory::Registrar) {
        domains::assign_default_registrar(&mut *tx, tenant_id, registrar).await?;
    }
    Ok(())
}

/// Refuses a database that holds anything besides the demo: other tenants
/// or other platform operators (a production database, a shared dev
/// database). `HOS_DEMO_ALLOW_EXISTING_DATA=1` overrides it.
pub async fn check_database(pool: &PgPool, cfg: &DemoConfig) -> anyhow::Result<()> {
    let others = hos_db::repo::demo::other_tenant_slugs(pool, &cfg.tenant_slug, 5).await?;
    let admins = hos_db::repo::demo::count_other_platform_admins(pool, &cfg.platform.email).await?;
    if others.is_empty() && admins == 0 {
        return Ok(());
    }
    if cfg.allow_existing_data {
        tracing::warn!(
            other_tenants = ?others,
            other_platform_admins = admins,
            "{ALLOW_EXISTING_ENV} is set: demo mode runs on a database with other data"
        );
        return Ok(());
    }
    anyhow::bail!(
        "{MODE_ENV} refuses to start: this database holds data that is not the demo's \
         (other companies: {others:?}; other operator accounts: {admins}). Demo mode deletes \
         and re-creates the demo company (`{slug}`) every {minutes} minutes and must have a \
         database of its own. Point DATABASE_URL at an empty database, or set \
         {ALLOW_EXISTING_ENV}=1 if this really is a throw-away database.",
        slug = cfg.tenant_slug,
        minutes = cfg.reset_minutes(),
    )
}

/// Demo mode, before anything is changed at startup: refuses debug mode and
/// a database with foreign data. Then the server migrates (demo mode always
/// migrates at startup, `crate::schema::migrate_on_start`: the demo
/// database is the demo's own) and calls [`start`].
pub async fn preflight(state: &AppState) -> anyhow::Result<()> {
    let Some(cfg) = state.web.demo.as_ref() else {
        return Ok(());
    };
    anyhow::ensure!(
        !state.web.debug,
        "{MODE_ENV} and HOS_DEBUG cannot be combined: debug mode shows internal error details. \
         Set HOS_DEBUG=0 (release builds default to off)."
    );
    let empty: bool = sqlx::query_scalar("select to_regclass('public.tenants') is null")
        .fetch_one(&state.pool)
        .await
        .context("reaching the database")?;
    if !empty {
        check_database(&state.pool, cfg).await?;
    }
    Ok(())
}

/// Startup in demo mode (after [`preflight`] and the migrations, before the
/// listener opens): checks the database again and resets once so the demo
/// always starts clean (the in-memory mock hypervisor of a new process knows
/// no VMs).
pub async fn start(state: &AppState) -> anyhow::Result<()> {
    let Some(cfg) = state.web.demo.as_ref() else {
        return Ok(());
    };
    check_database(&state.pool, cfg).await?;
    let report = reset(state).await.context("initial demo reset")?;
    tracing::warn!(
        tenant = %cfg.tenant_slug,
        reset_minutes = cfg.reset_minutes(),
        took_ms = u64::try_from(report.took.as_millis()).unwrap_or(u64::MAX),
        "DEMO MODE: public demo with fixed logins; all data is reset periodically; outbound \
         connections and real integrations are disabled (docs/DEMO.md)"
    );
    Ok(())
}

/// The reset loop (demo mode only; runs in the web process, whose requests
/// it gates).
pub fn spawn(state: &AppState, signal: &ShutdownSignal) -> Option<tokio::task::JoinHandle<()>> {
    state.web.demo.as_ref()?;
    let state = state.clone();
    let signal = signal.clone();
    Some(tokio::spawn(async move {
        loop {
            let wait = state.demo.until_next_reset();
            tokio::select! {
                () = tokio::time::sleep(wait.min(FOLLOW_EVERY)) => {}
                () = signal.clone().wait() => return,
            }
            if let Err(e) = follow_external_reset(&state).await {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "checking for an external demo reset"
                );
            }
            if !state.demo.until_next_reset().is_zero() {
                continue;
            }
            match reset(&state).await {
                Ok(r) => tracing::info!(
                    took_ms = u64::try_from(r.took.as_millis()).unwrap_or(u64::MAX),
                    "demo reset"
                ),
                Err(e) => {
                    tracing::error!(
                        error = format!("{e:#}"),
                        "demo reset failed; retrying in a minute"
                    );
                    state.demo.schedule_in(RETRY_AFTER_FAILURE);
                }
            }
        }
    }))
}

// ---------------------------------------------------------------------------
// Views: banner, login hints, captured mail
// ---------------------------------------------------------------------------

/// The banner on every page.
#[derive(Debug, Clone)]
pub struct Banner {
    /// Reset interval in minutes.
    pub every: u64,
    /// Minutes until the next reset (`None` = unknown).
    pub left: Option<i64>,
    /// Unix time of the next reset (`0` = unknown), for the live countdown
    /// in `static/js/app.js`.
    pub at: i64,
    /// Show the mailbox link (a logged-in tenant user).
    pub mailbox: bool,
}

/// A demo login offered on a login page.
#[derive(Debug, Clone)]
pub struct LoginHint {
    /// i18n key of the role label.
    pub label: &'static str,
    /// Login e-mail.
    pub email: String,
    /// Password.
    pub password: String,
}

/// The banner for this request, if demo mode is on.
pub fn banner(state: &AppState, logged_in_tenant_user: bool) -> Option<Banner> {
    let cfg = state.web.demo.as_ref()?;
    let at = state.demo.next_reset();
    let left = at.map(|t| ((t - Utc::now().timestamp()) / 60).max(0) + 1);
    Some(Banner {
        every: cfg.reset_minutes(),
        left,
        at: at.unwrap_or(0),
        mailbox: logged_in_tenant_user,
    })
}

/// Logins to offer on the tenant (`platform == false`) or platform login page.
pub fn login_hints(state: &AppState, tenant_slug: Option<&str>, platform: bool) -> Vec<LoginHint> {
    let Some(cfg) = state.web.demo.as_ref() else {
        return Vec::new();
    };
    let hint = |label, a: &DemoAccount| LoginHint {
        label,
        email: a.email.clone(),
        password: a.password.clone(),
    };
    // No operator console in the demo (single-company mode).
    if platform {
        return Vec::new();
    }
    if tenant_slug != Some(cfg.tenant_slug.as_str()) {
        return Vec::new();
    }
    vec![
        hint("demo.login.staff", &cfg.staff),
        hint("demo.login.client", &cfg.client),
    ]
}

/// Routes of demo mode (`/demo/mailbox`); `404` when demo mode is off.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/demo/mailbox", get(mailbox))
        .route("/demo/mailbox/{id}", get(mailbox_message))
}

/// One captured message in the list.
pub struct MailRow {
    id: EmailLogId,
    to: String,
    subject: String,
    at: chrono::DateTime<Utc>,
}

#[derive(Template)]
#[template(path = "demo/mailbox.html")]
struct MailboxPage {
    ctx: PageCtx,
    rows: Vec<MailRow>,
    open: Option<OpenMail>,
}
crate::page!(MailboxPage);

/// The message shown below the list.
struct OpenMail {
    to: String,
    from: String,
    subject: String,
    at: chrono::DateTime<Utc>,
    body: String,
}

/// Shows an address of a demo login in full, any other one masked
/// (`a***@example.com`): admin-typed addresses of other visitors stay
/// private.
fn mask_address(cfg: &DemoConfig, address: &str) -> String {
    let address = address.trim();
    if cfg.is_protected_login(address) || cfg.platform.email.eq_ignore_ascii_case(address) {
        return address.to_owned();
    }
    match address.split_once('@') {
        Some((local, domain)) => {
            let first: String = local.chars().take(1).collect();
            let domain_first: String = domain.chars().take(1).collect();
            let tld = domain.rsplit_once('.').map(|(_, t)| t).unwrap_or("");
            format!("{first}***@{domain_first}***.{tld}")
        }
        None => "***".to_owned(),
    }
}

/// Which messages a user may read: staff all of the tenant's, a client
/// those of its client account.
async fn mailbox_scope(
    state: &AppState,
    tenant: &Tenant,
    user: &MaybeUser,
) -> AppResult<Option<hos_core::ids::ClientId>> {
    let Some(session) = user.0.as_ref().filter(|s| s.is_authenticated()) else {
        return Err(AppError::Unauthorized);
    };
    let Some(u) = session.user.as_ref() else {
        return Err(AppError::Unauthorized);
    };
    if u.kind == UserKind::Staff {
        return Ok(None);
    }
    let client = clients::clients_of_user(&state.pool, tenant.id, u.id)
        .await?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)?;
    Ok(Some(client.id))
}

async fn mailbox_page(
    state: &AppState,
    tenant: &Tenant,
    user: &MaybeUser,
    mut ctx: PageCtx,
    open: Option<EmailLogId>,
) -> AppResult<Response> {
    let cfg = state.web.demo.as_ref().ok_or(AppError::NotFound)?;
    let scope = mailbox_scope(state, tenant, user).await?;
    let page = hos_db::repo::mail::list_email_log(
        &state.pool,
        tenant.id,
        scope,
        Page {
            limit: 50,
            offset: 0,
        },
    )
    .await?;
    let open = match open {
        None => None,
        Some(id) => {
            let row = hos_db::repo::billing_support::get_email_log(&state.pool, tenant.id, id)
                .await?
                .filter(|r| scope.is_none() || r.client_id == scope)
                .ok_or(AppError::NotFound)?;
            Some(OpenMail {
                to: mask_address(cfg, &row.to_address),
                from: row.from_address.unwrap_or_default(),
                subject: row.subject,
                at: row.created_at,
                body: row
                    .body_text
                    .or_else(|| row.body_html.map(|h| hos_mail::layout::html_to_text(&h)))
                    .unwrap_or_default(),
            })
        }
    };
    let rows = page
        .items
        .into_iter()
        .map(|r| MailRow {
            id: r.id,
            to: mask_address(cfg, &r.to_address),
            subject: r.subject,
            at: r.created_at,
        })
        .collect();
    let title = ctx.t("demo.mailbox.title");
    ctx.set_title(title);
    ctx.load_store().await?;
    render(&MailboxPage { ctx, rows, open })
}

/// `GET /demo/mailbox`.
async fn mailbox(
    State(state): State<AppState>,
    tenant: Tenant,
    user: MaybeUser,
    ctx: PageCtx,
) -> AppResult<Response> {
    mailbox_page(&state, &tenant, &user, ctx, None).await
}

/// `GET /demo/mailbox/{id}`.
async fn mailbox_message(
    State(state): State<AppState>,
    tenant: Tenant,
    user: MaybeUser,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    mailbox_page(&state, &tenant, &user, ctx, Some(id.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request that never finishes does not postpone the reset forever:
    /// the reset waits for it (bounded), then proceeds; new requests wait
    /// for the reset in any case.
    #[tokio::test]
    async fn the_reset_gate_waits_bounded_and_holds_new_requests_back() {
        let gate = Gate::default();
        // Nothing running: closed at once, nothing left over.
        let closed = gate.close(Duration::from_millis(50)).await;
        assert!(!closed.stragglers());
        drop(closed);

        let stuck = gate.enter().await;
        let started = Instant::now();
        let closed = gate.close(Duration::from_millis(200)).await;
        assert!(closed.stragglers(), "the stuck request is still running");
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(5));
        // While the reset runs, a new request waits.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), gate.enter())
                .await
                .is_err(),
            "admitted during a reset"
        );
        drop(closed);
        let next = tokio::time::timeout(Duration::from_secs(1), gate.enter())
            .await
            .expect("admitted after the reset");
        drop((next, stuck));

        // A request that ends within the wait: the reset takes the gate
        // cleanly as soon as it does.
        let short = gate.enter().await;
        let release = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(short);
        };
        let (closed, ()) = tokio::join!(gate.close(Duration::from_secs(5)), release);
        assert!(!closed.stragglers());
    }

    fn post(path: &str) -> Verdict {
        verdict(&Method::POST, path)
    }

    fn get_(path: &str) -> Verdict {
        verdict(&Method::GET, path)
    }

    #[test]
    fn segment_patterns() {
        assert!(seg_match("/admin/staff/**", "/admin/staff"));
        assert!(seg_match("/admin/staff/**", "/admin/staff/roles/1/delete"));
        assert!(!seg_match("/admin/staff/**", "/admin/staffing"));
        assert!(seg_match(
            "/admin/integrations/*",
            "/admin/integrations/abc"
        ));
        assert!(!seg_match(
            "/admin/integrations/*",
            "/admin/integrations/abc/test"
        ));
        assert!(seg_match("/admin/integrations", "/admin/integrations/"));
        assert!(!seg_match("/whois", "/whoisx"));
    }

    #[test]
    fn outbound_and_account_recovery_are_blocked() {
        for p in [
            "/auth/register",
            "/auth/forgot",
            "/auth/reset",
            "/auth/oauth/google",
            "/auth/oauth/github/callback",
            "/whois",
            "/webhooks/mail/mailgun-mime",
            "/webhooks/mail/raw",
            "/admin/tools/import/new",
            "/platform/jobs",
            "/platform/signup",
            "/en/auth/register",
        ] {
            assert_eq!(get_(p), Verdict::Block, "GET {p}");
            assert_eq!(post(p), Verdict::Block, "POST {p}");
        }
    }

    #[test]
    fn protected_writes_are_blocked() {
        for p in [
            "/platform/tenants",
            "/platform/tenants/1/delete",
            "/platform/settings",
            "/platform/account/2fa",
            "/platform/updates/check",
            "/platform/updates/apply",
            "/platform/updates/maintenance/off",
            "/api/v1/clients",
            "/api/v1/webhook-endpoints",
            "/auth/2fa/setup",
            "/auth/2fa/disable",
            "/client/account/password",
            "/client/account/sessions/revoke-others",
            "/client/account/identities/google/unlink",
            "/client/account/privacy/delete",
            "/admin/developer/webhooks",
            "/admin/developer/webhooks/new",
            "/admin/integrations",
            "/admin/integrations/0190",
            "/admin/integrations/0190/delete",
            "/admin/settings/security",
            "/admin/settings/registration",
            "/admin/settings/domains",
            "/admin/settings/domains/1/verify",
            "/admin/settings/einvoice",
            "/admin/settings/email/templates/invoice_created/tr",
            "/admin/settings/email/templates/invoice_created/tr/reset",
            "/admin/support/inbound/settings",
            "/admin/support/tickets/1/ai-draft",
            "/admin/ai/generate",
            "/admin/seo/indexnow/submit",
            "/en/admin/settings/security",
        ] {
            assert_eq!(post(p), Verdict::Block, "POST {p}");
        }
    }

    #[test]
    fn visitors_can_try_everything_else() {
        for p in [
            "/auth/login",
            "/auth/logout",
            "/auth/invite",
            "/platform/login",
            "/platform/login/2fa",
            "/platform/logout",
            "/cart/add",
            "/checkout/place",
            "/quote/1/accept",
            "/client/invoices/1/pay",
            "/client/tickets",
            "/client/services/1/server/power",
            "/client/account",
            "/client/account/users",
            "/client/account/api-keys",
            "/admin/clients",
            "/admin/clients/1/credit",
            "/admin/clients/1/logins",
            "/admin/orders/1/accept",
            "/admin/invoices/1/payment",
            "/admin/support/tickets/1/reply",
            "/admin/support/kb/categories",
            "/admin/support/announcements/new",
            "/admin/integrations/1/test",
            "/admin/staff/invite",
            "/admin/staff/roles/new",
            "/admin/developer/keys",
            "/admin/settings/general",
            "/admin/settings/general/assets/logo",
            "/admin/settings/orders",
            "/admin/settings/email",
            "/admin/settings/automation/1/run",
            "/admin/settings/appearance",
            "/admin/settings/appearance/preview",
            "/admin/settings/billing",
            "/admin/settings/email/templates/invoice_created/tr/preview",
            "/admin/settings/email/templates/invoice_created/tr/test",
            "/admin/catalog/products",
            "/admin/catalog/products/1",
            "/admin/catalog/tlds",
            "/admin/catalog/tlds/prices",
            "/admin/content/pages/new",
            "/admin/downloads",
            "/admin/status/incidents",
            "/admin/seo/redirects",
            "/admin/fraud/bans",
            "/contact",
        ] {
            assert_eq!(post(p), Verdict::Allow, "POST {p}");
        }
        for p in [
            "/",
            "/admin",
            "/admin/staff",
            "/admin/settings/general",
            "/admin/integrations",
            "/platform",
            "/platform/system",
            "/platform/updates",
            "/client/account/security",
            "/quote/1",
            "/api/v1/me",
        ] {
            assert_eq!(get_(p), Verdict::Allow, "GET {p}");
        }
    }

    #[test]
    fn heavy_pages() {
        for p in [
            "/admin/invoices/1/pdf",
            "/client/invoices/1/pdf",
            "/admin/clients/export.csv",
            "/admin/reports/revenue",
            "/admin/search",
            "/og/home.png",
            "/client/account/privacy/export",
            "/admin/invoices/1/print",
        ] {
            assert!(is_heavy(p), "{p}");
        }
        assert!(!is_heavy("/admin/invoices/1"));
    }

    #[test]
    fn ips_are_masked() {
        assert_eq!(
            mask_ip("203.0.113.77".parse().unwrap()),
            "203.0.113.0".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            mask_ip("2001:db8:1234:5678::1".parse().unwrap()),
            "2001:db8:1234::".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            mask_ip("::ffff:198.51.100.9".parse().unwrap()),
            "198.51.100.0".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn addresses_are_masked_except_demo_logins() {
        let cfg = DemoConfig::standard();
        assert_eq!(mask_address(&cfg, "jane.doe@example.org"), "j***@e***.org");
        assert_eq!(mask_address(&cfg, &cfg.client.email), cfg.client.email);
        assert_eq!(mask_address(&cfg, "nonsense"), "***");
    }

    #[test]
    fn demo_uploads_are_raster_images() {
        for ct in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(demo_upload_type(ct), "{ct}");
        }
        for ct in [
            "application/pdf",
            "text/plain; charset=utf-8",
            "application/zip",
            "image/svg+xml",
            "text/html",
            "",
        ] {
            assert!(!demo_upload_type(ct), "{ct}");
        }
    }

    #[test]
    fn demo_login_targets() {
        let id = Uuid::now_v7();
        assert_eq!(
            protected_target(&format!("/admin/clients/{id}/status")),
            Some(Target::Client(id))
        );
        assert_eq!(
            protected_target(&format!("/admin/clients/{id}/credit")),
            None
        );
        assert_eq!(protected_target("/admin/clients/x/status"), None);
        for tail in ["deactivate", "activate", "roles", "reset-2fa"] {
            assert_eq!(
                protected_target(&format!("/admin/staff/{id}/{tail}")),
                Some(Target::Staff(id))
            );
        }
        assert_eq!(protected_target("/admin/staff/invite"), None);
        assert_eq!(protected_target("/admin/staff/roles/new"), None);
    }
}
