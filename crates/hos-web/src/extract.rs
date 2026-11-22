//! Request extractors.
//!
//! | Extractor | Gives | Rejection |
//! |---|---|---|
//! | [`Tenant`](crate::tenant::Tenant) | the tenant of the host | 404 |
//! | [`ReqLocale`](crate::locale::ReqLocale) | effective locale + default + source | — |
//! | [`PageCtx`](crate::view::PageCtx) | everything templates need | — |
//! | [`MaybeUser`] | `Option<Arc<SessionInfo>>` | — |
//! | [`CurrentUser`] | any logged-in tenant user | 401 → login redirect |
//! | [`CurrentClient`] | client principal (`user_id`, `client_id`) | 401 / 403 |
//! | [`CurrentStaff`] | staff principal (+ permissions) | 401 / 403 |
//! | [`RequirePermission<P>`] | staff with permission `P` (`perm::BillingManage`, …) | 401 / 403 |
//! | [`PlatformAdmin`] | platform operator | 401 → `/platform/login` |
//! | [`CsrfToken`](crate::csrf::CsrfToken) | the CSRF token to embed | — |
//! | [`Flash`](crate::flash::Flash) | incoming flash messages | — |
//! | [`ClientIp`], [`UserAgent`] | client address / UA | — |
//! | [`Htmx`] | `HX-Request` & friends | — |
//! | [`PageQuery`] | `?page=&per_page=&sort=&dir=&q=` | — (bad values ignored) |

use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, Query};
use hos_auth::rbac::{Permission, PermissionSet, Principal};
use hos_core::ids::{ClientId, UserId};
use hos_db::Page;
use hos_db::repo::identity::UserRow;
use hos_db::repo::platform::PlatformAdminRow;
use http::request::Parts;
use serde::Deserialize;

use crate::error::AppError;
use crate::session::SessionInfo;
use crate::state::AppState;
use crate::tenant::Tenant;

/// Client IP: the socket peer, or — behind `HOS_TRUST_PROXY=1`, when the
/// peer is one of `HOS_TRUSTED_PROXIES` (any peer while that list is empty,
/// see [`WebConfig::forwarded_headers_trusted`]) — the address named by
/// `HOS_CLIENT_IP_HEADER` (e.g. Cloudflare's `CF-Connecting-IP`) when that
/// is set and valid, else the `X-Forwarded-For` entry written by the
/// outermost trusted proxy (`HOS_TRUSTED_PROXY_HOPS` places from the right,
/// see [`WebConfig::proxy_hops`]). The leftmost entries are
/// client-controlled and never trusted on their own.
///
/// Why a header: a proxy that does not trust the forwarded headers it
/// receives (Traefik's default) replaces `X-Forwarded-For` with its own
/// peer, so behind a CDN every visitor would have the CDN's address.
///
/// This is the only place that reads the client address: rate limits, the
/// demo limits, sessions, login history and the audit log all go through
/// [`from_config`](Self::from_config) / [`for_request`](Self::for_request).
///
/// `.0` is the address that is stored and shown (audit log, sessions,
/// orders); in the public demo it is masked ([`crate::demo::mask_ip`]).
/// Rate limits ([`key`](Self::key)) always use the real address.
///
/// [`WebConfig::forwarded_headers_trusted`]: crate::config::WebConfig::forwarded_headers_trusted
/// [`WebConfig::proxy_hops`]: crate::config::WebConfig::proxy_hops
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr, IpAddr);

impl ClientIp {
    /// The client address under `web`'s proxy settings (see the type docs).
    /// Without a TCP peer (a request built in-process) the fallback is
    /// `127.0.0.1`.
    pub fn from_config(parts: &Parts, web: &crate::config::WebConfig) -> Self {
        let peer = peer_ip(&parts.extensions);
        let forwarded = if web.forwarded_headers_trusted(peer) {
            web.client_ip_header
                .as_ref()
                .and_then(|name| header_ip(&parts.headers, name))
                .or_else(|| forwarded_client(&parts.headers, web.proxy_hops()))
        } else {
            None
        };
        let ip = forwarded
            .or(peer)
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        Self(ip, ip)
    }

    /// [`from_config`](Self::from_config) with the state's proxy settings;
    /// masked for storage in demo mode.
    pub fn for_request(parts: &Parts, state: &AppState) -> Self {
        let Self(ip, real) = Self::from_config(parts, &state.web);
        if state.web.demo.is_some() {
            Self(crate::demo::mask_ip(ip), real)
        } else {
            Self(ip, real)
        }
    }

    /// Rate-limit key for `action` (the unmasked address).
    pub fn key(&self, action: &str) -> String {
        hos_auth::ratelimit::rate_key(self.1, action)
    }
}

/// The TCP peer of the request (the listener's `ConnectInfo`), with an
/// IPv4-mapped IPv6 address (dual-stack socket) shown as IPv4. `None` for
/// requests built in-process.
pub fn peer_ip(extensions: &http::Extensions) -> Option<IpAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_canonical())
}

/// One address as proxies write it: `203.0.113.7`, `2001:db8::7`,
/// `[2001:db8::7]`, with a port (`203.0.113.7:4711`, `[2001:db8::7]:443`).
/// Anything else — names, `unknown`, zone ids (`fe80::1%eth0`), lists — is
/// `None`, so the caller falls back to the next source.
fn parse_ip_token(raw: &str) -> Option<IpAddr> {
    let s = raw.trim();
    // The longest valid form: `[` + 45-character IPv6 + `]:65535`.
    if s.is_empty() || s.len() > 54 {
        return None;
    }
    let ip = s
        .parse::<IpAddr>()
        .ok()
        .or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .or_else(|| {
            s.strip_prefix('[')
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|r| r.parse::<std::net::Ipv6Addr>().ok())
                .map(IpAddr::V6)
        })?;
    Some(ip.to_canonical())
}

/// A single address in header `name` (exactly one header line).
fn header_ip(headers: &http::HeaderMap, name: &http::HeaderName) -> Option<IpAddr> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    parse_ip_token(value.to_str().ok()?)
}

/// The `X-Forwarded-For` entry `hops` places from the right (all header
/// lines in order, comma separated). Fewer entries than hops → the leftmost
/// one (every entry was then written by a trusted proxy). `None` when
/// `hops == 0`, without the header or when the entry is not an IP address.
fn forwarded_client(headers: &http::HeaderMap, hops: usize) -> Option<IpAddr> {
    if hops == 0 {
        return None;
    }
    let entries: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let entry = entries.get(entries.len().saturating_sub(hops))?;
    parse_ip_token(entry)
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        Ok(Self::for_request(parts, state))
    }
}

/// `User-Agent` (truncated to 512 chars).
#[derive(Debug, Clone, Default)]
pub struct UserAgent(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for UserAgent {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Self(
            parts
                .headers
                .get(http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.chars().take(512).collect()),
        ))
    }
}

/// htmx request headers.
#[derive(Debug, Clone, Default)]
pub struct Htmx {
    /// `HX-Request: true` — render a fragment instead of a full page.
    pub request: bool,
    /// `HX-Boosted`.
    pub boosted: bool,
    /// `HX-Target` (element id).
    pub target: Option<String>,
    /// `HX-Trigger` (element id).
    pub trigger: Option<String>,
}

impl Htmx {
    /// From request headers.
    pub fn from_headers(h: &http::HeaderMap) -> Self {
        let s = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        Self {
            request: s("hx-request").is_some_and(|v| v == "true"),
            boosted: s("hx-boosted").is_some_and(|v| v == "true"),
            target: s("hx-target"),
            trigger: s("hx-trigger"),
        }
    }

    /// A fragment should be returned (htmx request that is not a boosted navigation).
    pub fn wants_fragment(&self) -> bool {
        self.request && !self.boosted
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Htmx {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Self::from_headers(&parts.headers))
    }
}

/// The session, if any (also half-authenticated 2FA sessions).
#[derive(Debug, Clone)]
pub struct MaybeUser(pub Option<Arc<SessionInfo>>);

impl MaybeUser {
    /// The fully authenticated session.
    pub fn authenticated(&self) -> Option<&Arc<SessionInfo>> {
        self.0.as_ref().filter(|s| s.is_authenticated())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for MaybeUser {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Self(parts.extensions.get::<Arc<SessionInfo>>().cloned()))
    }
}

fn session(parts: &Parts) -> Result<Arc<SessionInfo>, AppError> {
    parts
        .extensions
        .get::<Arc<SessionInfo>>()
        .cloned()
        .filter(|s| s.is_authenticated())
        .ok_or(AppError::Unauthorized)
}

/// Any logged-in tenant user (client or staff).
#[derive(Debug, Clone)]
pub struct CurrentUser {
    /// Session.
    pub session: Arc<SessionInfo>,
    /// User row.
    pub user: UserRow,
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, AppError> {
        let tenant = parts.extensions.get::<Tenant>().ok_or(AppError::NotFound)?;
        let session = session(parts)?;
        session.principal.require_tenant(tenant.id)?;
        let user = session.user.clone().ok_or(AppError::Unauthorized)?;
        Ok(Self { session, user })
    }
}

/// A client user acting for a client account (`/client`).
#[derive(Debug, Clone)]
pub struct CurrentClient {
    /// Session.
    pub session: Arc<SessionInfo>,
    /// User row.
    pub user: UserRow,
    /// `users.id`.
    pub user_id: UserId,
    /// The client account.
    pub client_id: ClientId,
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentClient {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, AppError> {
        let tenant = parts.extensions.get::<Tenant>().ok_or(AppError::NotFound)?;
        let session = session(parts)?;
        let (user_id, client_id) = session.principal.require_client(tenant.id)?;
        let user = session.user.clone().ok_or(AppError::Unauthorized)?;
        Ok(Self {
            session,
            user,
            user_id,
            client_id,
        })
    }
}

/// A staff member of the tenant (`/admin`).
#[derive(Debug, Clone)]
pub struct CurrentStaff {
    /// Session.
    pub session: Arc<SessionInfo>,
    /// User row.
    pub user: UserRow,
    /// `users.id`.
    pub user_id: UserId,
    /// Effective permissions.
    pub permissions: PermissionSet,
}

impl CurrentStaff {
    /// Whether the staff member has `p`.
    pub fn can(&self, p: Permission) -> bool {
        self.permissions.allows(p)
    }

    /// `Ok` or 403.
    pub fn require(&self, p: Permission) -> Result<(), AppError> {
        self.session.principal.require(p).map_err(AppError::from)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentStaff {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, AppError> {
        let tenant = parts.extensions.get::<Tenant>().ok_or(AppError::NotFound)?;
        let session = session(parts)?;
        let user_id = session.principal.require_staff(tenant.id)?;
        let permissions = match &session.principal {
            Principal::Staff { permissions, .. } => permissions.clone(),
            _ => PermissionSet::empty(),
        };
        let user = session.user.clone().ok_or(AppError::Unauthorized)?;
        Ok(Self {
            session,
            user,
            user_id,
            permissions,
        })
    }
}

/// Marker types for [`RequirePermission`].
pub trait PermissionMarker: Send + Sync + 'static {
    /// The required permission.
    const PERMISSION: Permission;
}

/// Permission markers: `RequirePermission<perm::ClientsView>`.
pub mod perm {
    use super::{Permission, PermissionMarker};

    macro_rules! markers {
        ($($name:ident => $perm:ident),+ $(,)?) => {
            $(
                #[doc = concat!("Marker for `", stringify!($perm), "`.")]
                #[derive(Debug, Clone, Copy)]
                pub struct $name;
                impl PermissionMarker for $name {
                    const PERMISSION: Permission = Permission::$perm;
                }
            )+
        };
    }

    markers! {
        ClientsView => CLIENTS_VIEW,
        ClientsManage => CLIENTS_MANAGE,
        BillingView => BILLING_VIEW,
        BillingManage => BILLING_MANAGE,
        ProductsManage => PRODUCTS_MANAGE,
        ServicesView => SERVICES_VIEW,
        ServicesManage => SERVICES_MANAGE,
        DomainsManage => DOMAINS_MANAGE,
        SupportView => SUPPORT_VIEW,
        SupportManage => SUPPORT_MANAGE,
        ContentManage => CONTENT_MANAGE,
        SeoManage => SEO_MANAGE,
        IntegrationsManage => INTEGRATIONS_MANAGE,
        SettingsManage => SETTINGS_MANAGE,
        StaffManage => STAFF_MANAGE,
        ReportsView => REPORTS_VIEW,
    }
}

/// Staff with permission `P` (`RequirePermission<perm::BillingManage>`).
#[derive(Debug, Clone)]
pub struct RequirePermission<P: PermissionMarker> {
    /// The staff member.
    pub staff: CurrentStaff,
    _p: PhantomData<P>,
}

impl<P: PermissionMarker, S: Send + Sync> FromRequestParts<S> for RequirePermission<P> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        let staff = CurrentStaff::from_request_parts(parts, state).await?;
        staff.require(P::PERMISSION)?;
        Ok(Self {
            staff,
            _p: PhantomData,
        })
    }
}

/// A platform operator (`/platform`).
#[derive(Debug, Clone)]
pub struct PlatformAdmin {
    /// Session.
    pub session: Arc<SessionInfo>,
    /// Operator row (single-company mode: the company's internal operator
    /// record, [`crate::mode::ServerArea`]).
    pub admin: PlatformAdminRow,
    /// Single-company mode: the owner acting through `/admin/server`
    /// (re-authentication uses their password and two-factor code).
    pub staff: Option<UserRow>,
}

impl<S: Send + Sync> FromRequestParts<S> for PlatformAdmin {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, AppError> {
        if let Some(area) = parts.extensions.get::<crate::mode::ServerArea>().cloned() {
            return Ok(Self {
                session: session(parts)?,
                admin: area.operator,
                staff: Some(area.staff),
            });
        }
        let session = session(parts)?;
        if !session.principal.is_platform_admin() {
            return Err(AppError::Unauthorized);
        }
        let admin = session.admin.clone().ok_or(AppError::Unauthorized)?;
        Ok(Self {
            session,
            admin,
            staff: None,
        })
    }
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    /// Ascending.
    #[default]
    Asc,
    /// Descending.
    Desc,
}

impl SortDir {
    /// `asc` / `desc`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawPageQuery {
    page: Option<String>,
    per_page: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    q: Option<String>,
}

/// Pagination / sorting / search query parameters.
#[derive(Debug, Clone)]
pub struct PageQuery {
    /// 1-based page.
    pub page: u32,
    /// Items per page (1..=100, default 25).
    pub per_page: u32,
    /// Requested sort key (validate with [`PageQuery::sort_column`]).
    pub sort: Option<String>,
    /// Direction.
    pub dir: SortDir,
    /// Search term (trimmed, non-empty).
    pub q: Option<String>,
}

impl Default for PageQuery {
    fn default() -> Self {
        Self {
            page: 1,
            per_page: 25,
            sort: None,
            dir: SortDir::Asc,
            q: None,
        }
    }
}

impl PageQuery {
    /// The DB page.
    pub fn db_page(&self) -> Page {
        Page::number(self.page, self.per_page)
    }

    /// Maps the requested sort key through an allow-list of
    /// `(key, sql_expression)`; falls back to `default`. Never interpolate
    /// `sort` into SQL directly.
    pub fn sort_column(
        &self,
        allowed: &[(&str, &'static str)],
        default: &'static str,
    ) -> &'static str {
        self.sort
            .as_deref()
            .and_then(|s| allowed.iter().find(|(k, _)| *k == s).map(|(_, c)| *c))
            .unwrap_or(default)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for PageQuery {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Infallible> {
        let raw = Query::<RawPageQuery>::from_request_parts(parts, state)
            .await
            .map(|q| q.0)
            .unwrap_or_default();
        Ok(Self {
            page: raw
                .page
                .and_then(|p| p.parse().ok())
                .filter(|p| *p >= 1)
                .unwrap_or(1),
            per_page: raw
                .per_page
                .and_then(|p| p.parse().ok())
                .map(|p: u32| p.clamp(1, 100))
                .unwrap_or(25),
            sort: raw.sort.filter(|s| {
                s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            }),
            dir: match raw.dir.as_deref() {
                Some("desc") => SortDir::Desc,
                _ => SortDir::Asc,
            },
            q: raw
                .q
                .map(|q| q.trim().chars().take(200).collect::<String>())
                .filter(|q| !q.is_empty()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xff(values: &[&str]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for v in values {
            h.append("x-forwarded-for", v.parse().unwrap());
        }
        h
    }

    #[test]
    fn forwarded_client_counts_trusted_hops_from_the_right() {
        let spoofed = xff(&["6.6.6.6, 203.0.113.7"]);
        // One trusted proxy: the entry it appended (rightmost), never the
        // client-supplied leftmost one.
        assert_eq!(
            forwarded_client(&spoofed, 1),
            Some("203.0.113.7".parse().unwrap())
        );
        assert_eq!(
            forwarded_client(&spoofed, 2),
            Some("6.6.6.6".parse().unwrap())
        );
        // More hops than entries: the leftmost entry.
        assert_eq!(
            forwarded_client(&spoofed, 5),
            Some("6.6.6.6".parse().unwrap())
        );
        // 0 = header ignored.
        assert_eq!(forwarded_client(&spoofed, 0), None);
        // Several header lines are one list; ports are tolerated.
        let lines = xff(&["6.6.6.6", "198.51.100.2:4711"]);
        assert_eq!(
            forwarded_client(&lines, 1),
            Some("198.51.100.2".parse().unwrap())
        );
        assert_eq!(forwarded_client(&xff(&["garbage"]), 1), None);
        assert_eq!(forwarded_client(&http::HeaderMap::new(), 1), None);
    }

    #[test]
    fn client_ip_header_needs_exactly_one_valid_address() {
        let name = http::HeaderName::from_static("cf-connecting-ip");
        let mut h = http::HeaderMap::new();
        assert_eq!(header_ip(&h, &name), None);
        h.insert(&name, " 2001:db8::7 ".parse().unwrap());
        assert_eq!(header_ip(&h, &name), Some("2001:db8::7".parse().unwrap()));
        // Two lines (one forged, one from the proxy) are ambiguous.
        h.append(&name, "203.0.113.7".parse().unwrap());
        assert_eq!(header_ip(&h, &name), None);
        // A list or a non-address is ignored.
        h.insert(&name, "6.6.6.6, 203.0.113.7".parse().unwrap());
        assert_eq!(header_ip(&h, &name), None);
        h.insert(&name, "unknown".parse().unwrap());
        assert_eq!(header_ip(&h, &name), None);
    }

    #[test]
    fn address_tokens_tolerate_brackets_and_ports_but_nothing_else() {
        let ok =
            |s: &str, want: &str| assert_eq!(parse_ip_token(s), Some(want.parse().unwrap()), "{s}");
        ok("203.0.113.7", "203.0.113.7");
        ok(" 2001:db8::7 ", "2001:db8::7");
        ok("[2001:db8::7]", "2001:db8::7");
        ok("[2001:db8::7]:443", "2001:db8::7");
        ok("203.0.113.7:4711", "203.0.113.7");
        ok("::ffff:203.0.113.7", "203.0.113.7");
        for bad in [
            "",
            "unknown",
            "fe80::1%eth0",
            "[fe80::1%eth0]:80",
            "2001:db8::7]",
            "[203.0.113.7]",
            "203.0.113.7, 198.51.100.1",
            "203.0.113",
            "_hidden",
            "proxy.example",
            &"1".repeat(4096),
            "[::1]:99999",
        ] {
            assert_eq!(parse_ip_token(bad), None, "{bad:?}");
        }
    }

    fn web(trust: bool, proxies: &str, header: Option<&'static str>) -> crate::config::WebConfig {
        crate::config::WebConfig {
            static_dir: std::path::PathBuf::new(),
            locales_dir: std::path::PathBuf::new(),
            debug: false,
            trust_proxy: trust,
            trusted_proxy_hops: 1,
            client_ip_header: header.map(http::HeaderName::from_static),
            trusted_proxies: crate::config::TrustedProxies::parse(proxies),
            secure: false,
            max_body_bytes: 1024,
            http_timeouts: crate::config::HttpTimeouts::default(),
            mock_integrations: crate::config::MockIntegrations::Refused,
            oauth_endpoints: None,
            demo: None,
            mode: crate::config::AppMode::Single,
            company: None,
        }
    }

    fn parts(peer: Option<&str>, headers: &[(&str, &str)]) -> Parts {
        let mut b = http::Request::get("/");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let (mut parts, ()) = b.body(()).unwrap().into_parts();
        if let Some(p) = peer {
            parts
                .extensions
                .insert(ConnectInfo(p.parse::<SocketAddr>().unwrap()));
        }
        parts
    }

    #[test]
    fn forwarding_headers_count_only_from_listed_proxies() {
        let forged = [
            ("cf-connecting-ip", "198.51.100.4"),
            ("x-forwarded-for", "198.51.100.5"),
        ];
        let ip =
            |w: &crate::config::WebConfig, p: Parts| ClientIp::from_config(&p, w).0.to_string();
        // A direct client outside the list: its own address, whatever it sends.
        let w = web(true, "10.0.1.0/24", Some("cf-connecting-ip"));
        assert_eq!(
            ip(&w, parts(Some("203.0.113.9:5555"), &forged)),
            "203.0.113.9"
        );
        // The proxy: the header it forwards.
        assert_eq!(
            ip(&w, parts(Some("10.0.1.5:40000"), &forged)),
            "198.51.100.4"
        );
        // The proxy on a dual-stack socket.
        assert_eq!(
            ip(&w, parts(Some("[::ffff:10.0.1.5]:40000"), &forged)),
            "198.51.100.4"
        );
        // Without the client-IP header setting: X-Forwarded-For, same rule.
        let w = web(true, "10.0.1.0/24", None);
        assert_eq!(ip(&w, parts(Some("10.0.1.5:1"), &forged)), "198.51.100.5");
        assert_eq!(ip(&w, parts(Some("203.0.113.9:1"), &forged)), "203.0.113.9");
        // No peer (in-process request) and a list: nothing is believed.
        assert_eq!(ip(&w, parts(None, &forged)), "127.0.0.1");
        // Empty list: every peer, as before HOS_TRUSTED_PROXIES existed.
        let w = web(true, "", Some("cf-connecting-ip"));
        assert_eq!(
            ip(&w, parts(Some("203.0.113.9:1"), &forged)),
            "198.51.100.4"
        );
        // HOS_TRUST_PROXY off: the list does not switch the headers on.
        let w = web(false, "10.0.1.0/24", Some("cf-connecting-ip"));
        assert_eq!(ip(&w, parts(Some("10.0.1.5:1"), &forged)), "10.0.1.5");
    }

    #[test]
    fn malformed_forwarding_headers_fall_back_to_the_next_source() {
        let w = web(true, "10.0.1.0/24", Some("cf-connecting-ip"));
        let proxy = Some("10.0.1.5:1");
        let ip = |h: &[(&str, &str)]| ClientIp::from_config(&parts(proxy, h), &w).0.to_string();
        // Bad client-IP header → X-Forwarded-For → the peer.
        assert_eq!(
            ip(&[
                ("cf-connecting-ip", "fe80::1%eth0"),
                ("x-forwarded-for", "198.51.100.5")
            ]),
            "198.51.100.5"
        );
        assert_eq!(
            ip(&[("cf-connecting-ip", "[2001:db8::9]:443")]),
            "2001:db8::9"
        );
        assert_eq!(ip(&[("x-forwarded-for", "unknown")]), "10.0.1.5");
        assert_eq!(ip(&[("x-forwarded-for", ",,,")]), "10.0.1.5");
        assert_eq!(ip(&[("x-forwarded-for", "[::1")]), "10.0.1.5");
        // Two client-IP header lines: ambiguous, X-Forwarded-For is used.
        let two = parts(
            proxy,
            &[
                ("cf-connecting-ip", "198.51.100.4"),
                ("cf-connecting-ip", "198.51.100.6"),
                ("x-forwarded-for", "198.51.100.5"),
            ],
        );
        assert_eq!(
            ClientIp::from_config(&two, &w).0.to_string(),
            "198.51.100.5"
        );
    }
}
