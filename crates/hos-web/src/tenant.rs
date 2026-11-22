//! Tenant resolution: `Host` → [`TenantContext`].
//!
//! * The `Host` value must be strictly `hostname[:port]` (letters, digits,
//!   `-`, `.`) or `[ipv6][:port]`; anything else is a `400`. The parsed,
//!   normalised host is stored as [`RequestHost`] and is the only source of
//!   the request origin (`csrf::request_origin`, `PageCtx.origin`): links in
//!   e-mails, canonical tags, sitemap and llms.txt never come from a second,
//!   looser parse of the raw header.
//! * `localhost` / `127.0.0.1` / `::1` → the `HOS_DEV_TENANT` slug, **only in
//!   debug mode** (`HOS_DEBUG`, on in debug builds).
//! * otherwise `tenant_domains` (custom domains must be verified), then the
//!   `<slug>.<HOS_BASE_DOMAIN>` fallback — except the platform's own label
//!   (`HOS_BASE_URL` = `https://<label>.<HOS_BASE_DOMAIN>`), which is never a
//!   tenant slug.
//! * Results (including misses) are cached for a short TTL.
//! * Single-company mode (`HOS_MODE=single`, the default): every host is
//!   the one company and `/platform` is a 404 ([`crate::mode`]).
//! * Unknown host (multi-company mode) → plain 404; suspended / cancelled
//!   tenant → an info page (503).
//! * `/platform/*`, `/healthz`, `/readyz` and `/static/*` do not need a tenant.

use std::collections::HashMap;
use std::convert::Infallible;
use std::ops::Deref;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{FromRequestParts, OptionalFromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use hos_core::tenant::TenantContext;
use hos_db::repo::platform;
use http::StatusCode;
use http::request::Parts;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// The tenant serving the request (request extension, also an extractor).
#[derive(Debug, Clone)]
pub struct Tenant(pub Arc<TenantContext>);

impl Deref for Tenant {
    type Target = TenantContext;

    fn deref(&self) -> &TenantContext {
        &self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Tenant {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Tenant>()
            .cloned()
            .ok_or(AppError::NotFound)
    }
}

impl<S: Send + Sync> OptionalFromRequestParts<S> for Tenant {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Option<Self>, Infallible> {
        Ok(parts.extensions.get::<Tenant>().cloned())
    }
}

/// The validated `Host` of the request (request extension set by
/// [`resolve_tenant`]; absent when the request carried no host at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHost {
    /// Lower-case host name without a trailing dot, or an IP literal (IPv6
    /// without brackets).
    pub name: String,
    /// Explicit port.
    pub port: Option<u16>,
}

impl RequestHost {
    /// Parses a `Host` value strictly: `hostname[:port]` where the name is
    /// dot-separated non-empty labels of ASCII letters, digits and `-`, or
    /// `[ipv6][:port]`. Userinfo (`a:@b`), paths, spaces, `_` and every
    /// other character are rejected.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > 260 {
            return None;
        }
        let (name, port) = if let Some(rest) = raw.strip_prefix('[') {
            let (inner, after) = rest.split_once(']')?;
            let ip: std::net::Ipv6Addr = inner.parse().ok()?;
            let port = match after {
                "" => None,
                p => Some(parse_port(p.strip_prefix(':')?)?),
            };
            (ip.to_string(), port)
        } else {
            let (name, port) = match raw.split_once(':') {
                Some((n, p)) => (n, Some(parse_port(p)?)),
                None => (raw, None),
            };
            let name = name.strip_suffix('.').unwrap_or(name);
            let labels_ok = !name.is_empty()
                && name.len() <= 253
                && name.split('.').all(|l| {
                    !l.is_empty()
                        && l.len() <= 63
                        && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
            if !labels_ok {
                return None;
            }
            (name.to_ascii_lowercase(), port)
        };
        Some(Self { name, port })
    }

    /// `host[:port]` for building URLs (IPv6 in brackets).
    pub fn authority(&self) -> String {
        let host = if self.name.contains(':') {
            format!("[{}]", self.name)
        } else {
            self.name.clone()
        };
        match self.port {
            Some(p) => format!("{host}:{p}"),
            None => host,
        }
    }
}

fn parse_port(p: &str) -> Option<u16> {
    if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    p.parse::<u16>().ok().filter(|p| *p != 0)
}

impl<S: Send + Sync> FromRequestParts<S> for RequestHost {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<RequestHost>()
            .cloned()
            .ok_or(AppError::NotFound)
    }
}

/// The label of the platform console under the base domain: `console` for
/// `HOS_BASE_URL=https://console.example.com` and `HOS_BASE_DOMAIN=example.com`.
/// `None` when the console is not on a direct sub-domain of the base domain.
/// That label is never resolved as (or given to) a tenant slug.
pub fn platform_host_label(config: &hos_core::config::AppConfig) -> Option<String> {
    let host = config
        .base_url
        .host_str()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let base = config
        .base_domain
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let label = host.strip_suffix(&base)?.strip_suffix('.')?;
    (!label.is_empty() && !label.contains('.')).then(|| label.to_owned())
}

#[derive(Debug, Clone)]
struct Cached {
    at: Instant,
    value: Option<Arc<TenantContext>>,
}

/// Short-TTL cache `normalized host → tenant`.
#[derive(Debug)]
pub struct TenantCache {
    ttl: Duration,
    negative_ttl: Duration,
    map: RwLock<HashMap<String, Cached>>,
}

impl TenantCache {
    /// Cache with `ttl` for hits (misses are kept for at most 10 s).
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            negative_ttl: ttl.min(Duration::from_secs(10)),
            map: RwLock::new(HashMap::new()),
        }
    }

    /// Drops every cached entry (after tenant settings / domains change).
    pub fn invalidate_all(&self) {
        self.map.write().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn get(&self, host: &str) -> Option<Option<Arc<TenantContext>>> {
        let map = self.map.read().unwrap_or_else(|e| e.into_inner());
        let c = map.get(host)?;
        let ttl = if c.value.is_some() {
            self.ttl
        } else {
            self.negative_ttl
        };
        (c.at.elapsed() < ttl).then(|| c.value.clone())
    }

    fn put(&self, host: String, value: Option<Arc<TenantContext>>) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        if map.len() > 10_000 {
            map.clear();
        }
        map.insert(
            host,
            Cached {
                at: Instant::now(),
                value,
            },
        );
    }

    /// Resolves `host` (raw `Host` header accepted).
    pub async fn resolve(
        &self,
        state: &AppState,
        raw_host: &str,
    ) -> AppResult<Option<Arc<TenantContext>>> {
        let host = platform::normalize_host(raw_host);
        if host.is_empty() {
            return Ok(None);
        }
        if let Some(hit) = self.get(&host) {
            return Ok(hit);
        }
        let ctx = lookup(state, &host).await?;
        let ctx = ctx.map(Arc::new);
        self.put(host, ctx.clone());
        Ok(ctx)
    }
}

async fn lookup(state: &AppState, host: &str) -> AppResult<Option<TenantContext>> {
    // Single-company mode (the public demo included): every host that
    // reaches the app (the `HOS_BASE_URL` host, `www.`, an IP address, a
    // proxy's own name) shows the one company.
    if state.web.single() {
        return match state.web.company.as_deref() {
            Some(slug) => Ok(platform::tenant_context_by_slug(&state.pool, slug).await?),
            None => Ok(None),
        };
    }
    lookup_host(state, host).await
}

async fn lookup_host(state: &AppState, host: &str) -> AppResult<Option<TenantContext>> {
    if is_local_host(host) {
        // Development convenience only: production never maps a bare
        // loopback host onto a tenant.
        if !state.web.debug {
            return Ok(None);
        }
        return Ok(platform::tenant_context_by_slug(&state.pool, &state.config.dev_tenant).await?);
    }
    if let Some(ctx) = platform::tenant_context_by_host(&state.pool, host).await? {
        return Ok(Some(ctx));
    }
    let base = state.config.base_domain.as_str();
    if let Some(label) = host.strip_suffix(base).and_then(|s| s.strip_suffix('.'))
        && !label.is_empty()
        && !label.contains('.')
    {
        // The console's own host is not a tenant sub-domain.
        if platform_host_label(&state.config).as_deref() == Some(label) {
            return Ok(None);
        }
        return Ok(platform::tenant_context_by_slug(&state.pool, label).await?);
    }
    Ok(None)
}

/// `localhost`, `127.0.0.1`, `::1`.
pub fn is_local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Path with a leading `/tr` or `/en` segment removed (for area checks that
/// run before the locale middleware).
pub fn path_without_locale(path: &str) -> &str {
    for code in ["/tr", "/en"] {
        if let Some(rest) = path.strip_prefix(code)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return if rest.is_empty() { "/" } else { rest };
        }
    }
    path
}

/// Paths served without a tenant.
pub fn is_tenantless_path(path: &str) -> bool {
    let p = path_without_locale(path);
    p == "/platform"
        || p.starts_with("/platform/")
        || p == "/healthz"
        || p == "/readyz"
        || p.starts_with("/static/")
}

/// The request host (`Host` header, else the URI authority).
pub fn request_host(req_headers: &http::HeaderMap, uri: &http::Uri) -> String {
    req_headers
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .or_else(|| uri.authority().map(|a| a.as_str().to_owned()))
        .unwrap_or_default()
}

/// Middleware: validates the `Host` ([`RequestHost`], `400` when malformed),
/// resolves the tenant and stores [`Tenant`] in the request.
pub async fn resolve_tenant(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let raw = request_host(req.headers(), req.uri());
    let host = if raw.is_empty() {
        None
    } else {
        match RequestHost::parse(&raw) {
            Some(h) => Some(h),
            None => return bad_host_page(),
        }
    };
    if let Some(h) = &host {
        req.extensions_mut().insert(h.clone());
    }
    let single = state.web.single();
    if single && is_console_path(req.uri().path()) {
        // No operator console in single-company mode: the normal 404 page.
        if let Ok(uri) = "/__not-found".parse() {
            *req.uri_mut() = uri;
        }
    } else if is_tenantless_path(req.uri().path()) {
        return next.run(req).await;
    }
    let key = match &host {
        Some(h) => h.name.clone(),
        // One company: the host does not matter.
        None if single => "localhost".to_owned(),
        None => return unknown_host_page(),
    };
    match state.tenants.resolve(&state, &key).await {
        Ok(Some(ctx)) if ctx.is_operational() => {
            // Theme (cached per tenant) + staff preview bookkeeping.
            let preview = crate::theme::attach(&state, &ctx, &mut req).await;
            req.extensions_mut().insert(Tenant(ctx));
            let res = next.run(req).await;
            crate::theme::finish(&state, preview, res)
        }
        Ok(Some(ctx)) => suspended_page(&state, &ctx),
        Ok(None) if single => unavailable_page(),
        Ok(None) => unknown_host_page(),
        Err(e) if single => {
            // Database unreachable, or not migrated yet on a first start.
            tracing::error!(error = %e, "looking up the company failed");
            unavailable_page()
        }
        Err(e) => e.into_response(),
    }
}

fn bad_host_page() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"robots\" content=\"noindex\"><title>Bad request</title></head>\
             <body><h1>Bad request</h1><p>Invalid Host header.</p></body></html>",
        ),
    )
        .into_response()
}

/// Paths that exist only in multi-company mode (with or without a locale
/// prefix): the operator console and the custom domains of a company.
fn is_console_path(path: &str) -> bool {
    let p = path_without_locale(path);
    ["/platform", "/admin/settings/domains"]
        .iter()
        .any(|root| p == *root || p.strip_prefix(root).is_some_and(|r| r.starts_with('/')))
}

/// Single-company mode without its company: not set up yet (`hostingos
/// setup`), or the database cannot be reached.
fn unavailable_page() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Html(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"robots\" content=\"noindex\"><title>Temporarily unavailable</title></head>\
             <body><h1>Temporarily unavailable</h1><p>Please try again in a few minutes.</p></body></html>",
        ),
    )
        .into_response()
}

fn unknown_host_page() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"robots\" content=\"noindex\"><title>Site not found</title></head>\
             <body><h1>Site not found</h1><p>No site is configured for this address.</p></body></html>",
        ),
    )
        .into_response()
}

fn suspended_page(state: &AppState, ctx: &TenantContext) -> Response {
    let cat = state.i18n.catalog(ctx.default_locale);
    let name = crate::view::escape_html(ctx.display_name());
    let css = state.assets.css_url();
    let body = format!(
        "<!doctype html><html lang=\"{lang}\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{name}</title>\
         <link rel=\"stylesheet\" href=\"{css}\"></head>\
         <body class=\"center-screen\"><main class=\"notice-card\"><h1>{name}</h1><p>{msg}</p></main></body></html>",
        lang = ctx.default_locale.as_str(),
        msg = crate::view::escape_html(&cat.t("errors.suspended.text")),
    );
    let mut res = (StatusCode::SERVICE_UNAVAILABLE, Html(body)).into_response();
    res.headers_mut().insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("3600"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_prefix_and_tenantless() {
        assert_eq!(path_without_locale("/en/platform/login"), "/platform/login");
        assert_eq!(path_without_locale("/en"), "/");
        assert_eq!(path_without_locale("/entry"), "/entry");
        assert!(is_tenantless_path("/platform"));
        assert!(is_tenantless_path("/en/platform/x"));
        assert!(is_tenantless_path("/static/css/app.css"));
        assert!(!is_tenantless_path("/platformer"));
        assert!(!is_tenantless_path("/"));
        assert!(is_local_host("localhost"));
        assert!(!is_local_host("demo.localhost"));
    }

    #[test]
    fn host_header_is_parsed_strictly() {
        let h = RequestHost::parse("Acme.Example.COM.:8443").unwrap();
        assert_eq!(h.name, "acme.example.com");
        assert_eq!(h.port, Some(8443));
        assert_eq!(h.authority(), "acme.example.com:8443");
        let h = RequestHost::parse("[::1]:8080").unwrap();
        assert_eq!(h.name, "::1");
        assert_eq!(h.authority(), "[::1]:8080");
        assert_eq!(
            RequestHost::parse("127.0.0.1").unwrap().authority(),
            "127.0.0.1"
        );
        for bad in [
            "victim.example:@evil.com",
            "victim.example@evil.com",
            "victim.example:80@evil.com",
            "a:b:c",
            "evil.com/x",
            "evil.com:",
            "evil.com:0",
            "evil.com:99999",
            "evil..com",
            ".evil.com",
            "under_score.com",
            "spa ce.com",
            "[::1",
            "[not-ip]",
            "[::1]x",
            "::1",
            "",
        ] {
            assert_eq!(RequestHost::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn platform_label_is_derived_from_the_base_url() {
        let cfg = |url: &str, domain: &str| {
            hos_core::config::AppConfig::from_lookup(|k| match k {
                "DATABASE_URL" => Some("postgres://u:p@localhost/db".to_owned()),
                "HOS_SECRET_KEY" => Some(format!("{}=", "A".repeat(43))),
                "HOS_BASE_URL" => Some(url.to_owned()),
                "HOS_BASE_DOMAIN" => Some(domain.to_owned()),
                _ => None,
            })
            .unwrap()
        };
        assert_eq!(
            platform_host_label(&cfg("https://console.example.com", "example.com")).as_deref(),
            Some("console")
        );
        assert_eq!(
            platform_host_label(&cfg("https://example.com", "example.com")),
            None
        );
        assert_eq!(
            platform_host_label(&cfg("https://a.b.example.com", "example.com")),
            None
        );
        assert_eq!(
            platform_host_label(&cfg("https://ops.other.net", "example.com")),
            None
        );
    }
}
