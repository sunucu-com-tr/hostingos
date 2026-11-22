//! Web-layer settings that are not part of [`hos_core::config::AppConfig`].
//!
//! | Variable | Default |
//! |---|---|
//! | `HOS_STATIC_DIR` | `./static`, else `<workspace>/static` |
//! | `HOS_LOCALES_DIR` | `./locales`, else `<workspace>/locales` |
//! | `HOS_DEBUG` | on in debug builds (`/admin/_styleguide`, template reload hints) |
//! | `HOS_TRUST_PROXY` | off — when `1`, the client IP is taken from `X-Forwarded-For` |
//! | `HOS_TRUSTED_PROXY_HOPS` | 1 — reverse proxies in front of the app that append to `X-Forwarded-For` (with `HOS_TRUST_PROXY=1`); the client IP is the entry that many places from the right, `0` ignores the header |
//! | `HOS_MODE` | `single` — one hosting company: every host shows it, no `/platform` console ([`AppMode`]); `multi` serves many companies by host name with the operator console |
//! | `HOS_CLIENT_IP_HEADER` | unset — with `HOS_TRUST_PROXY=1`, a header that carries the visitor's address set by the outermost proxy (e.g. `CF-Connecting-IP` behind Cloudflare); used before `X-Forwarded-For` when it holds a valid address |
//! | `HOS_TRUSTED_PROXIES` | unset — comma-separated addresses / CIDR ranges (IPv4 and IPv6) of the reverse proxies that connect to the app; with `HOS_TRUST_PROXY=1` the forwarding headers are believed only from these peers ([`TrustedProxies`]). Unset: from any peer (logged at startup). A malformed entry stops the server |
//! | `HOS_MAX_BODY_BYTES` | 2 MiB |
//! | `HOS_HTTP_HEADER_TIMEOUT_SECS` | 120 — receiving a request's headers; also closes keep-alive connections idle this long ([`HttpTimeouts`]) |
//! | `HOS_HTTP_BODY_TIMEOUT_SECS` | 60 — receiving a request's body (`408`) |
//! | `HOS_HTTP_REQUEST_TIMEOUT_SECS` | 120 — producing the response once the request is in (`503`) |
//!
//! Process-wide variables read at startup by [`crate::state::AppState::from_env`]:
//! `HOS_OUTBOUND_POLICY` (`private` | `public`, see [`hos_core::net`]; an
//! invalid value stops the process) and `HOS_ALLOW_MOCK_INTEGRATIONS` (the
//! `mock` gateway / hypervisor / panel / registrar; always allowed in debug
//! mode, with a warning at startup — see [`WebConfig::mock_integrations`]).
//! `HOS_MIGRATE_ON_START` is read by `hostingos serve` ([`crate::schema`]).
//!
//! `HOS_DEMO_MODE`, `HOS_DEMO_RESET_MINUTES`, `HOS_DEMO_ALLOW_EXISTING_DATA`:
//! the public demo ([`crate::demo`], docs/DEMO.md).

use std::path::{Path, PathBuf};

use hos_core::config::AppConfig;

/// Web settings.
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Directory served at `/static`.
    pub static_dir: PathBuf,
    /// Directory with `tr/*.json` and `en/*.json`.
    pub locales_dir: PathBuf,
    /// Development extras (styleguide, verbose errors in logs).
    pub debug: bool,
    /// Honour `X-Forwarded-For` / `X-Forwarded-Proto`.
    pub trust_proxy: bool,
    /// Trusted reverse proxies that append to `X-Forwarded-For`
    /// (`HOS_TRUSTED_PROXY_HOPS`, default 1). See [`WebConfig::proxy_hops`].
    pub trusted_proxy_hops: usize,
    /// `HOS_CLIENT_IP_HEADER`: a header naming the visitor's address, set by
    /// the outermost proxy (`CF-Connecting-IP` behind Cloudflare). Honoured
    /// only with `HOS_TRUST_PROXY`; see [`crate::extract::ClientIp`].
    pub client_ip_header: Option<http::HeaderName>,
    /// `HOS_TRUSTED_PROXIES`: the peers whose forwarding headers are
    /// believed (with `HOS_TRUST_PROXY`); empty = any peer. See
    /// [`WebConfig::forwarded_headers_trusted`].
    pub trusted_proxies: TrustedProxies,
    /// `Secure` cookies + HSTS (derived from `HOS_BASE_URL` being https).
    pub secure: bool,
    /// Request body limit.
    pub max_body_bytes: usize,
    /// Header / body / request timeouts of the HTTP server.
    pub http_timeouts: HttpTimeouts,
    /// Whether the `mock` integrations may be saved and used, and why
    /// ([`WebConfig::mock_integrations`]). Tests may override it.
    pub mock_integrations: MockIntegrations,
    /// Replacement OAuth endpoints (integration tests against a mock
    /// provider). Never read from the environment; `None` = the real
    /// Google / GitHub endpoints.
    pub oauth_endpoints: Option<OAuthEndpoints>,
    /// Public demo mode (`HOS_DEMO_MODE`, [`crate::demo`]); `None` = off.
    pub demo: Option<crate::demo::DemoConfig>,
    /// `HOS_MODE`: one company (default) or many.
    pub mode: AppMode,
    /// Single-company mode: the company's internal slug (found or checked
    /// at startup, [`crate::mode`]). `None` in multi-company mode.
    pub company: Option<String>,
}

/// How many hosting companies one installation serves (`HOS_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// `single` (default): one hosting company. Every host that reaches the
    /// app shows it; the multi-company operator console (`/platform`) does
    /// not exist; the owner finds updates, maintenance and server status
    /// in the admin panel (`/admin/server`).
    Single,
    /// `multi`: companies are told apart by host name
    /// (`<slug>.HOS_BASE_DOMAIN`, custom domains) and run by the operator
    /// console at `/platform`.
    Multi,
}

impl AppMode {
    /// The environment variable.
    pub const ENV: &str = "HOS_MODE";

    /// Parses `HOS_MODE` (unset or empty → `single`).
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(Self::ENV) {
            Err(_) => Ok(Self::Single),
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" | "single" => Ok(Self::Single),
                "multi" => Ok(Self::Multi),
                other => Err(format!(
                    "{}={other:?} is not a mode: use `single` (one hosting company, the default) or `multi`",
                    Self::ENV
                )),
            },
        }
    }
}

/// Whether the simulated `mock` providers (a mock gateway lets anyone "pay"
/// any invoice) are enabled for this process, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockIntegrations {
    /// Refused (production default).
    Refused,
    /// `HOS_ALLOW_MOCK_INTEGRATIONS` is truthy.
    AllowedByEnv,
    /// Debug mode (`HOS_DEBUG`, on in debug builds): demo / dev servers.
    AllowedInDebug,
    /// Public demo mode (`HOS_DEMO_MODE`): mocks only, nothing real.
    AllowedByDemo,
}

impl MockIntegrations {
    /// Mocks may be saved and resolved.
    pub fn allowed(self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// OAuth endpoint overrides (tests only, see [`WebConfig::oauth_endpoints`]).
#[derive(Debug, Clone, Default)]
pub struct OAuthEndpoints {
    /// Google endpoints.
    pub google: Option<hos_auth::oauth::GoogleEndpoints>,
    /// GitHub endpoints.
    pub github: Option<hos_auth::oauth::GitHubEndpoints>,
}

impl WebConfig {
    /// Reads the environment (after `.env` has been loaded).
    pub fn from_env(app: &AppConfig) -> Self {
        let flag = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        };
        let debug = flag("HOS_DEBUG").unwrap_or(cfg!(debug_assertions));
        let demo = crate::demo::DemoConfig::from_env();
        let mode = AppMode::from_env().unwrap_or(AppMode::Single);
        Self {
            static_dir: locate("HOS_STATIC_DIR", "static"),
            locales_dir: locate("HOS_LOCALES_DIR", "locales"),
            debug,
            mock_integrations: if demo.is_some() {
                MockIntegrations::AllowedByDemo
            } else {
                Self::mock_integrations(debug)
            },
            company: demo.as_ref().map(|d| d.tenant_slug.clone()),
            demo,
            mode,
            trust_proxy: flag("HOS_TRUST_PROXY").unwrap_or(false),
            trusted_proxy_hops: std::env::var("HOS_TRUSTED_PROXY_HOPS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(1),
            client_ip_header: std::env::var("HOS_CLIENT_IP_HEADER")
                .ok()
                .map(|v| v.trim().to_ascii_lowercase())
                .filter(|v| !v.is_empty())
                .and_then(|v| http::HeaderName::from_bytes(v.as_bytes()).ok()),
            trusted_proxies: TrustedProxies::parse(
                &std::env::var(TrustedProxies::ENV).unwrap_or_default(),
            ),
            secure: app.base_url.scheme() == "https",
            max_body_bytes: std::env::var("HOS_MAX_BODY_BYTES")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(2 * 1024 * 1024),
            http_timeouts: HttpTimeouts::from_env(),
            oauth_endpoints: None,
        }
    }

    /// Whether the forwarding headers (`HOS_CLIENT_IP_HEADER`,
    /// `X-Forwarded-For`, `X-Forwarded-Proto`) of a request whose TCP peer
    /// is `peer` are believed: only with `HOS_TRUST_PROXY`, and — when
    /// `HOS_TRUSTED_PROXIES` names any — only from those proxies. Without a
    /// list every peer is believed (the behaviour before the list existed;
    /// the server warns at startup). An unknown peer (a request that did not
    /// come through the listener) is never in a list.
    pub fn forwarded_headers_trusted(&self, peer: Option<std::net::IpAddr>) -> bool {
        if !self.trust_proxy {
            return false;
        }
        if self.trusted_proxies.is_empty() {
            return true;
        }
        peer.is_some_and(|ip| self.trusted_proxies.contains(ip))
    }

    /// Mock integrations: allowed when `HOS_ALLOW_MOCK_INTEGRATIONS` is truthy,
    /// and always in debug mode (a demo / dev server needs no extra
    /// variable; the server logs a warning at startup). Production
    /// (non-debug) needs the variable.
    pub fn mock_integrations(debug: bool) -> MockIntegrations {
        if hos_billing::registry::mock_integrations_allowed_by_env() {
            MockIntegrations::AllowedByEnv
        } else if debug {
            MockIntegrations::AllowedInDebug
        } else {
            MockIntegrations::Refused
        }
    }

    /// Single-company mode?
    pub fn single(&self) -> bool {
        self.mode == AppMode::Single
    }

    /// How many `X-Forwarded-For` entries, counted from the right, were
    /// written by trusted proxies: the client IP is the entry at that
    /// position. `0` (also whenever `HOS_TRUST_PROXY` is off) means the header
    /// is ignored and the socket peer address is used.
    pub fn proxy_hops(&self) -> usize {
        if self.trust_proxy {
            self.trusted_proxy_hops
        } else {
            0
        }
    }
}

/// `HOS_TRUSTED_PROXIES`: the reverse proxies allowed to set the forwarding
/// headers — comma-separated IPv4 / IPv6 addresses or CIDR ranges
/// (`10.0.1.0/24`, `127.0.0.1`, `fd00::/8`, `[::1]`). Matching ignores the
/// IPv4-mapped IPv6 form of a peer (`::ffff:10.0.1.5` is `10.0.1.5`).
/// Entries that do not parse are kept in [`invalid`](Self::invalid): the
/// server refuses to start with them (`AppState::from_env`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies {
    nets: Vec<ipnet::IpNet>,
    invalid: Vec<String>,
}

impl TrustedProxies {
    /// The environment variable.
    pub const ENV: &str = "HOS_TRUSTED_PROXIES";

    /// Parses a comma (or whitespace) separated list; empty entries are
    /// skipped.
    pub fn parse(raw: &str) -> Self {
        let mut out = Self::default();
        for entry in raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            let bare = entry
                .strip_prefix('[')
                .and_then(|e| e.strip_suffix(']'))
                .unwrap_or(entry);
            let net = bare.parse::<ipnet::IpNet>().ok().or_else(|| {
                bare.parse::<std::net::IpAddr>()
                    .ok()
                    .map(|ip| ipnet::IpNet::from(ip.to_canonical()))
            });
            match net {
                Some(net) => out.nets.push(canonical_net(net)),
                None => out.invalid.push(entry.chars().take(64).collect()),
            }
        }
        out
    }

    /// No proxy named.
    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    /// Whether `ip` is one of the proxies.
    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.nets.iter().any(|n| n.contains(&ip))
    }

    /// Entries that are not an address or a CIDR range.
    pub fn invalid(&self) -> &[String] {
        &self.invalid
    }

    /// The ranges, for the startup log.
    pub fn nets(&self) -> &[ipnet::IpNet] {
        &self.nets
    }
}

/// An IPv4-mapped IPv6 range (`::ffff:10.0.0.0/104`) as the IPv4 range it
/// stands for, so it matches peers after [`std::net::IpAddr::to_canonical`].
fn canonical_net(net: ipnet::IpNet) -> ipnet::IpNet {
    if let ipnet::IpNet::V6(v6) = net
        && let Some(v4) = v6.addr().to_ipv4_mapped()
        && v6.prefix_len() >= 96
        && let Ok(n) = ipnet::Ipv4Net::new(v4, v6.prefix_len() - 96)
    {
        return ipnet::IpNet::V4(n);
    }
    net
}

/// Timeouts of the HTTP server (docs/DEPLOYMENT.md). `None` = no limit
/// (the variable set to `0`).
///
/// * `header` (`HOS_HTTP_HEADER_TIMEOUT_SECS`, default 120 s): a client
///   must send a request's complete headers within this time. hyper also
///   applies it while a keep-alive connection waits for its next request,
///   so it closes idle connections too; the default stays above the idle
///   time reverse proxies keep backend connections for (Traefik: 90 s), so
///   the proxy, not the app, closes them and never reuses a closed one.
/// * `body` (`HOS_HTTP_BODY_TIMEOUT_SECS`, default 60 s): the whole request
///   body must arrive within this time, else `408` (`crate::http_limits`).
///   Raise it together with `HOS_MAX_BODY_BYTES` for large uploads over
///   slow links.
/// * `request` (`HOS_HTTP_REQUEST_TIMEOUT_SECS`, default 120 s): once the
///   request is in, the response (its headers) must be ready within this
///   time, else `503` and the request's work is dropped. Streaming
///   response bodies and upgraded connections (the VM console websocket)
///   are not limited by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpTimeouts {
    /// Receiving the request headers.
    pub header: Option<std::time::Duration>,
    /// Receiving the request body.
    pub body: Option<std::time::Duration>,
    /// Producing the response.
    pub request: Option<std::time::Duration>,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            header: Some(std::time::Duration::from_secs(120)),
            body: Some(std::time::Duration::from_secs(60)),
            request: Some(std::time::Duration::from_secs(120)),
        }
    }
}

impl HttpTimeouts {
    /// Header timeout variable.
    pub const HEADER_ENV: &str = "HOS_HTTP_HEADER_TIMEOUT_SECS";
    /// Body timeout variable.
    pub const BODY_ENV: &str = "HOS_HTTP_BODY_TIMEOUT_SECS";
    /// Request timeout variable.
    pub const REQUEST_ENV: &str = "HOS_HTTP_REQUEST_TIMEOUT_SECS";

    /// From the environment; an unparsable value keeps the default (logged).
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            header: Self::read(Self::HEADER_ENV, d.header),
            body: Self::read(Self::BODY_ENV, d.body),
            request: Self::read(Self::REQUEST_ENV, d.request),
        }
    }

    fn read(key: &str, default: Option<std::time::Duration>) -> Option<std::time::Duration> {
        let Ok(raw) = std::env::var(key) else {
            return default;
        };
        if raw.trim().is_empty() {
            return default;
        }
        match Self::parse_secs(&raw) {
            Some(v) => v,
            None => {
                tracing::warn!(
                    value = %raw.chars().take(20).collect::<String>(),
                    "{key} must be a whole number of seconds (0 = no limit); using the default"
                );
                default
            }
        }
    }

    /// `"30"` → 30 s, `"0"` → no limit (`None` = not a number).
    fn parse_secs(raw: &str) -> Option<Option<std::time::Duration>> {
        let secs: u64 = raw.trim().parse().ok()?;
        Some((secs > 0).then(|| std::time::Duration::from_secs(secs.min(24 * 3600))))
    }
}

/// Workspace root as seen at compile time (`crates/hos-web/../..`).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn locate(env_key: &str, name: &str) -> PathBuf {
    if let Ok(v) = std::env::var(env_key)
        && !v.trim().is_empty()
    {
        return PathBuf::from(v.trim());
    }
    // Release layout (docs/UPDATES.md): `releases/<version>/{hostingos,static,
    // locales}` — a process keeps serving the assets of its own release while
    // `current` already points to a newly installed one.
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join(name)))
        .filter(|d| d.is_dir())
    {
        return dir;
    }
    let local = PathBuf::from(name);
    if local.is_dir() {
        return local;
    }
    workspace_root().join(name)
}

/// Loads `.env` from the current directory or the workspace root (tests run
/// with the crate directory as CWD). Missing files are fine.
pub fn load_dotenv() {
    if dotenvy::dotenv().is_err() {
        let _ = dotenvy::from_path(workspace_root().join(".env"));
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::time::Duration;

    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn trusted_proxies_parse_ranges_and_addresses() {
        let p =
            TrustedProxies::parse(" 10.0.1.0/24, 127.0.0.1 ,[::1], fd00::/8,,::ffff:192.0.2.0/120");
        assert!(p.invalid().is_empty(), "{:?}", p.invalid());
        assert_eq!(p.nets().len(), 5);
        assert!(p.contains(ip("10.0.1.200")));
        assert!(!p.contains(ip("10.0.2.1")));
        assert!(p.contains(ip("127.0.0.1")));
        assert!(!p.contains(ip("127.0.0.2")));
        assert!(p.contains(ip("::1")));
        assert!(p.contains(ip("fd12::5")));
        // An IPv4 peer seen on a dual-stack socket.
        assert!(p.contains(ip("::ffff:10.0.1.7")));
        // A mapped range matches the IPv4 peer it stands for.
        assert!(p.contains(ip("192.0.2.9")));
        assert!(!p.contains(ip("203.0.113.9")));
        // Whitespace separates as well.
        assert_eq!(TrustedProxies::parse("10.0.0.1 10.0.0.2").nets().len(), 2);
    }

    #[test]
    fn trusted_proxies_keep_malformed_entries_for_the_startup_check() {
        let p = TrustedProxies::parse("10.0.0.0/8, 10.0.0.0/33, proxy.local, fe80::1%eth0, 1.2.3");
        assert_eq!(p.nets().len(), 1);
        assert_eq!(
            p.invalid(),
            ["10.0.0.0/33", "proxy.local", "fe80::1%eth0", "1.2.3"]
        );
        assert!(TrustedProxies::parse("").is_empty());
        assert!(TrustedProxies::parse(" , ").invalid().is_empty());
    }

    #[test]
    fn forwarded_headers_need_trust_proxy_and_a_listed_peer() {
        let mut web = WebConfig {
            static_dir: PathBuf::new(),
            locales_dir: PathBuf::new(),
            debug: false,
            trust_proxy: false,
            trusted_proxy_hops: 1,
            client_ip_header: None,
            trusted_proxies: TrustedProxies::default(),
            secure: false,
            max_body_bytes: 1024,
            http_timeouts: HttpTimeouts::default(),
            mock_integrations: MockIntegrations::Refused,
            oauth_endpoints: None,
            demo: None,
            mode: AppMode::Single,
            company: None,
        };
        let proxy = Some(ip("10.0.1.5"));
        let stranger = Some(ip("203.0.113.9"));
        assert!(!web.forwarded_headers_trusted(proxy), "HOS_TRUST_PROXY off");
        web.trust_proxy = true;
        // No list: every peer, as before the list existed.
        assert!(web.forwarded_headers_trusted(stranger));
        assert!(web.forwarded_headers_trusted(None));
        web.trusted_proxies = TrustedProxies::parse("10.0.1.0/24");
        assert!(web.forwarded_headers_trusted(proxy));
        assert!(!web.forwarded_headers_trusted(stranger));
        assert!(!web.forwarded_headers_trusted(None), "unknown peer");
    }

    #[test]
    fn http_timeouts_parse_seconds() {
        assert_eq!(
            HttpTimeouts::parse_secs("30"),
            Some(Some(Duration::from_secs(30)))
        );
        assert_eq!(HttpTimeouts::parse_secs(" 0 "), Some(None));
        assert_eq!(HttpTimeouts::parse_secs("1m"), None);
        assert_eq!(HttpTimeouts::parse_secs("-5"), None);
        let d = HttpTimeouts::default();
        assert_eq!(d.header, Some(Duration::from_secs(120)));
        assert_eq!(d.body, Some(Duration::from_secs(60)));
        assert_eq!(d.request, Some(Duration::from_secs(120)));
    }
}
