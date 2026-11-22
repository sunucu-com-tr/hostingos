//! Outbound network policy for **tenant-configured endpoints** (SSRF guard).
//!
//! Integration settings let a tenant admin type a host or URL that HostingOS
//! then connects to (hypervisor `api_url`, panel `host`, SMTP `host`,
//! registrar `base_url`, RDAP `bootstrap_url`, AI `base_url`). Without a guard
//! that turns the platform into a proxy into its own network: the loopback
//! interface, link-local cloud metadata services (`169.254.169.254`) or —
//! on a multi-tenant SaaS — other customers' private networks.
//!
//! The policy comes from `HOS_OUTBOUND_POLICY`, read once per process:
//!
//! | Value | Allowed destinations |
//! |---|---|
//! | `private` (default) | public **and** private ranges (RFC 1918, CGNAT, ULA): a self-hosted hosting company reaches Proxmox, vCenter and panels on its LAN |
//! | `public` | public unicast addresses only — recommended for anyone hosting multiple unrelated tenants |
//!
//! Both modes always refuse loopback, link-local (`169.254.0.0/16`,
//! `fe80::/10`), cloud metadata addresses, unspecified, multicast, broadcast
//! and reserved addresses, including their IPv4-mapped IPv6 forms.
//! [`OutboundPolicy::Unrestricted`] exists for tests (wiremock on
//! `127.0.0.1`); it can only be chosen in code, never through the
//! environment.
//!
//! Enforcement (feature `net`):
//!
//! * [`OutboundPolicy::client_builder`] returns a `reqwest` builder whose DNS
//!   resolver ([`PolicyResolver`]) checks **every** resolved address at
//!   connect time, so a DNS answer that changes between validation and use
//!   (rebinding) is still refused. It also disables redirects and system
//!   proxies (a proxy would do its own DNS lookup and bypass the check).
//! * IP-literal hosts never reach a resolver, so callers also run
//!   [`OutboundPolicy::check_url`] on each request URL (cheap, no I/O).
//! * [`OutboundPolicy::resolve_host`] validates and pins addresses for
//!   non-HTTP protocols (SMTP).
//! * [`read_body_limited`] reads a response body with a size cap.
//!
//! This is the workspace's one address classifier: outgoing tenant webhooks
//! (`hos_billing::notify::is_public_ip`) use `OutboundPolicy::Public`
//! [`allows_ip`](OutboundPolicy::allows_ip) too.
//!
//! **Offline switch.** [`go_offline`] turns every check of every policy
//! (also `Public` / `Private` used explicitly, but not the tests-only
//! `Unrestricted`) into a refusal with [`OutboundError::Offline`], before any
//! DNS lookup. The public demo (`HOS_DEMO_MODE`, docs/DEMO.md) switches it on
//! at startup; it cannot be switched off again in the same process.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static OFFLINE: AtomicBool = AtomicBool::new(false);

/// Refuses every outbound connection of this process from now on (demo
/// mode). One-way: there is no way back short of a restart.
pub fn go_offline() {
    OFFLINE.store(true, Ordering::SeqCst);
}

/// Whether [`go_offline`] was called in this process.
pub fn is_offline() -> bool {
    OFFLINE.load(Ordering::SeqCst)
}

use crate::error::IntegrationError;

/// Environment variable holding the policy (`private` | `public`).
pub const POLICY_ENV: &str = "HOS_OUTBOUND_POLICY";

/// Body cap for provider API responses (JSON / XML).
pub const MAX_API_BODY: usize = 16 * 1024 * 1024;

/// Which destinations tenant-configured endpoints may resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OutboundPolicy {
    /// Public and private ranges; no loopback, link-local, metadata,
    /// multicast, broadcast, unspecified or reserved addresses. Default.
    #[default]
    Private,
    /// Public unicast addresses only.
    Public,
    /// No address check at all. **Tests only** (wiremock on `127.0.0.1`);
    /// cannot be selected through `HOS_OUTBOUND_POLICY`.
    Unrestricted,
}

/// What kind of address an IP is, as far as the policy cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressClass {
    /// Globally routable unicast.
    Public,
    /// RFC 1918, CGNAT `100.64.0.0/10`, unique local `fc00::/7`,
    /// site-local `fec0::/10`, local-use NAT64.
    Private,
    /// Documentation, benchmarking and IETF protocol-assignment ranges and
    /// well-known NAT64 of a public address: not public, not dangerous.
    Special,
    /// `127.0.0.0/8`, `::1`, `localhost`.
    Loopback,
    /// `169.254.0.0/16`, `fe80::/10`.
    LinkLocal,
    /// Cloud instance metadata services.
    Metadata,
    /// `0.0.0.0/8`, `::`.
    Unspecified,
    /// `224.0.0.0/4`, `ff00::/8`.
    Multicast,
    /// `255.255.255.255`.
    Broadcast,
    /// `240.0.0.0/4`, IPv4-compatible / translated IPv6 and the rest of `::/64`.
    Reserved,
}

impl AddressClass {
    /// Short human-readable name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Special => "special-purpose",
            Self::Loopback => "loopback",
            Self::LinkLocal => "link-local",
            Self::Metadata => "cloud metadata",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::Reserved => "reserved",
        }
    }
}

impl fmt::Display for AddressClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a destination is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutboundError {
    /// `HOS_OUTBOUND_POLICY` has an unknown value.
    #[error("invalid HOS_OUTBOUND_POLICY value {0:?}: expected `private` or `public`")]
    InvalidPolicy(String),
    /// Not an `http(s)` URL with a host, or not a valid host name / IP.
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    /// The host is, or resolves to, an address the policy does not allow.
    #[error(
        "endpoint {host} is not allowed: it is or resolves to a {class} address \
         (HOS_OUTBOUND_POLICY={policy})"
    )]
    Blocked {
        /// Host as configured.
        host: String,
        /// Class of the first refused address.
        class: AddressClass,
        /// Policy in force.
        policy: OutboundPolicy,
    },
    /// The host name does not resolve.
    #[error("cannot resolve endpoint host {0}")]
    Unresolvable(String),
    /// Outbound connections are switched off for this process
    /// ([`go_offline`], demo mode).
    #[error("outbound connections are disabled on this server (demo mode)")]
    Offline,
}

impl From<OutboundError> for IntegrationError {
    /// Refused / invalid destinations are configuration errors (not
    /// retryable); a failed lookup is a transport error (retryable).
    fn from(err: OutboundError) -> Self {
        match err {
            OutboundError::Unresolvable(_) => Self::Transport(err.to_string()),
            other => Self::Config(other.to_string()),
        }
    }
}

static GLOBAL: OnceLock<OutboundPolicy> = OnceLock::new();

impl OutboundPolicy {
    /// Parses an `HOS_OUTBOUND_POLICY` value. Empty → [`Private`](Self::Private).
    /// `Unrestricted` is deliberately not accepted.
    pub fn parse(value: &str) -> Result<Self, OutboundError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "private" => Ok(Self::Private),
            "public" => Ok(Self::Public),
            _ => Err(OutboundError::InvalidPolicy(
                value.chars().take(40).collect(),
            )),
        }
    }

    /// Reads and validates `HOS_OUTBOUND_POLICY` (unset → `Private`). Call it
    /// at startup to fail fast on a typo; [`global`](Self::global) itself
    /// fails closed (`Public`) on an invalid value.
    pub fn from_env() -> Result<Self, OutboundError> {
        match std::env::var(POLICY_ENV) {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Private),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(OutboundError::InvalidPolicy("<non-UTF-8>".into()))
            }
        }
    }

    /// The process-wide policy: `HOS_OUTBOUND_POLICY`, read once. An invalid
    /// value selects the strictest policy (`Public`).
    pub fn global() -> Self {
        *GLOBAL.get_or_init(|| Self::from_env().unwrap_or(Self::Public))
    }

    /// Fixes the process-wide policy before anything read it, for
    /// integration-test harnesses that run providers against a local mock
    /// server. Returns the policy already in force when it was read or set
    /// earlier (then nothing changes).
    pub fn set_global(policy: Self) -> Result<(), Self> {
        GLOBAL.set(policy).map_err(|_| Self::global())
    }

    /// `private` | `public` | `unrestricted`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
            Self::Unrestricted => "unrestricted",
        }
    }

    /// Whether an address of `class` may be contacted (never while the
    /// process is [offline](go_offline), except under `Unrestricted`).
    pub fn allows(self, class: AddressClass) -> bool {
        self.allows_when(is_offline(), class)
    }

    /// [`allows`](Self::allows) with the offline state given explicitly.
    pub const fn allows_when(self, offline: bool, class: AddressClass) -> bool {
        if offline && !matches!(self, Self::Unrestricted) {
            return false;
        }
        match self {
            Self::Unrestricted => true,
            Self::Public => matches!(class, AddressClass::Public),
            Self::Private => matches!(
                class,
                AddressClass::Public | AddressClass::Private | AddressClass::Special
            ),
        }
    }

    /// Whether `ip` may be contacted.
    pub fn allows_ip(self, ip: IpAddr) -> bool {
        self.allows(classify(ip))
    }

    fn check_ip_for(self, host: &str, ip: IpAddr) -> Result<(), OutboundError> {
        let class = classify(ip);
        if self.allows(class) {
            Ok(())
        } else {
            Err(OutboundError::Blocked {
                host: host.chars().take(253).collect(),
                class,
                policy: self,
            })
        }
    }

    /// Static check of a host (no DNS): IP literals — in any notation a URL
    /// parser accepts, e.g. `2130706433`, `0177.0.0.1`, `0x7f.1`, `[::1]` —
    /// are classified; `localhost` / `*.localhost` count as loopback.
    /// Returns the IP for literals, `None` for names (which still have to be
    /// resolved and checked, see `resolve_host`).
    pub fn check_host(self, host: &str) -> Result<Option<IpAddr>, OutboundError> {
        if is_offline() && self != Self::Unrestricted {
            return Err(OutboundError::Offline);
        }
        let invalid = || OutboundError::InvalidEndpoint(format!("invalid host {host:?}"));
        let trimmed = host.trim();
        if trimmed.is_empty() {
            return Err(invalid());
        }
        // Bare IPv6 (`::1`) is not a URL host; everything else goes through
        // the WHATWG host parser, which normalises numeric IPv4 notations.
        let parsed = match trimmed.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => url::Host::<String>::Ipv4(v4),
            Ok(IpAddr::V6(v6)) => url::Host::Ipv6(v6),
            Err(_) => url::Host::parse(trimmed).map_err(|_| invalid())?,
        };
        match parsed {
            url::Host::Ipv4(v4) => self
                .check_ip_for(trimmed, IpAddr::V4(v4))
                .map(|()| Some(IpAddr::V4(v4))),
            url::Host::Ipv6(v6) => {
                self.check_ip_for(trimmed, IpAddr::V6(v6))?;
                // Connect to `::ffff:a.b.c.d` over IPv4.
                Ok(Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)))
            }
            url::Host::Domain(name) => {
                let name = name.trim_end_matches('.');
                if self != Self::Unrestricted
                    && (name == "localhost" || name.ends_with(".localhost"))
                {
                    return Err(OutboundError::Blocked {
                        host: trimmed.to_owned(),
                        class: AddressClass::Loopback,
                        policy: self,
                    });
                }
                Ok(None)
            }
        }
    }

    /// Static check of an endpoint URL (no DNS): `http`/`https` with a host,
    /// no credentials in the URL, and [`check_host`](Self::check_host).
    pub fn check_url(self, url: &url::Url) -> Result<(), OutboundError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(OutboundError::InvalidEndpoint(
                "only http(s) URLs are allowed".into(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OutboundError::InvalidEndpoint(
                "credentials in the URL are not allowed".into(),
            ));
        }
        let host = match url.host() {
            Some(url::Host::Domain(d)) => d.to_owned(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            None => return Err(OutboundError::InvalidEndpoint("URL has no host".into())),
        };
        self.check_host(&host).map(|_| ())
    }
}

impl fmt::Display for OutboundPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cloud metadata endpoints that are not already link-local.
const METADATA_V4: [Ipv4Addr; 4] = [
    Ipv4Addr::new(169, 254, 169, 254), // AWS, GCP, Azure, OpenStack, Hetzner, DO, Oracle
    Ipv4Addr::new(169, 254, 170, 2),   // AWS ECS task metadata
    Ipv4Addr::new(100, 100, 100, 200), // Alibaba Cloud (inside CGNAT)
    Ipv4Addr::new(192, 0, 0, 192),     // Oracle Cloud classic
];
const METADATA_V6: [Ipv6Addr; 1] = [
    Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254), // AWS IMDS IPv6 (inside ULA)
];

/// Classifies an address. IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is classified
/// as the embedded IPv4 address.
pub fn classify(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr) -> AddressClass {
    let o = ip.octets();
    if METADATA_V4.contains(&ip) {
        AddressClass::Metadata
    } else if o[0] == 0 {
        AddressClass::Unspecified
    } else if o[0] == 127 {
        AddressClass::Loopback
    } else if o[0] == 169 && o[1] == 254 {
        AddressClass::LinkLocal
    } else if ip.is_broadcast() {
        AddressClass::Broadcast
    } else if (224..=239).contains(&o[0]) {
        AddressClass::Multicast
    } else if o[0] >= 240 {
        AddressClass::Reserved
    } else if o[0] == 10
        || (o[0] == 172 && (o[1] & 0xf0) == 16)
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
    {
        AddressClass::Private
    } else if (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
    {
        AddressClass::Special
    } else {
        AddressClass::Public
    }
}

fn classify_v6(ip: Ipv6Addr) -> AddressClass {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    let s = ip.segments();
    if ip.is_unspecified() {
        AddressClass::Unspecified
    } else if ip.is_loopback() {
        AddressClass::Loopback
    } else if METADATA_V6.contains(&ip) {
        AddressClass::Metadata
    } else if s[..4] == [0, 0, 0, 0] {
        // IPv4-compatible (::a.b.c.d), IPv4-translated (::ffff:0:a.b.c.d) …
        AddressClass::Reserved
    } else if s[0] == 0x0064 && s[1] == 0xff9b {
        if s[2..6] == [0, 0, 0, 0] {
            // Well-known NAT64 prefix: judge the embedded IPv4 destination.
            let [a, b] = s[6].to_be_bytes();
            let [c, d] = s[7].to_be_bytes();
            match classify_v4(Ipv4Addr::new(a, b, c, d)) {
                AddressClass::Public => AddressClass::Special,
                other => other,
            }
        } else {
            AddressClass::Private
        }
    } else if (s[0] & 0xff00) == 0xff00 {
        AddressClass::Multicast
    } else if (s[0] & 0xffc0) == 0xfe80 {
        AddressClass::LinkLocal
    } else if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfec0 {
        AddressClass::Private
    } else if s[0] == 0x2001 && s[1] == 0x0db8 {
        AddressClass::Special
    } else {
        AddressClass::Public
    }
}

#[cfg(feature = "net")]
pub use enforce::{BodyError, PolicyResolver, outbound_error_in, read_body_limited};

#[cfg(feature = "net")]
mod enforce {
    use std::net::SocketAddr;

    use super::{OutboundError, OutboundPolicy};

    impl OutboundPolicy {
        /// Resolves `host` and checks **every** address (a name with one bad
        /// address is refused as a whole). IP literals are checked without a
        /// lookup. The returned addresses are the ones to connect to.
        pub async fn resolve_host(
            self,
            host: &str,
            port: u16,
        ) -> Result<Vec<SocketAddr>, OutboundError> {
            let host = host.trim().trim_start_matches('[').trim_end_matches(']');
            if let Some(ip) = self.check_host(host)? {
                return Ok(vec![SocketAddr::new(ip, port)]);
            }
            let name = match url::Host::parse(host) {
                Ok(url::Host::Domain(d)) => d,
                _ => {
                    return Err(OutboundError::InvalidEndpoint(format!(
                        "invalid host {host:?}"
                    )));
                }
            };
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), port))
                .await
                .map_err(|_| OutboundError::Unresolvable(name.clone()))?
                .collect();
            if addrs.is_empty() {
                return Err(OutboundError::Unresolvable(name));
            }
            for addr in &addrs {
                self.check_ip_for(&name, addr.ip())?;
            }
            Ok(addrs)
        }

        /// [`check_url`](Self::check_url) plus DNS: every address of the
        /// URL's host must be allowed.
        pub async fn validate_url(self, url: &url::Url) -> Result<Vec<SocketAddr>, OutboundError> {
            self.check_url(url)?;
            let port = url.port_or_known_default().unwrap_or(443);
            let host = url
                .host_str()
                .ok_or_else(|| OutboundError::InvalidEndpoint("URL has no host".into()))?;
            self.resolve_host(host, port).await
        }

        /// The DNS resolver enforcing this policy.
        pub const fn resolver(self) -> PolicyResolver {
            PolicyResolver { policy: self }
        }

        /// A `reqwest` client builder for tenant-configured endpoints: DNS
        /// answers are checked at connect time ([`PolicyResolver`]),
        /// redirects are not followed and system proxies are ignored.
        /// Callers add timeouts / TLS options and run
        /// [`check_url`](Self::check_url) on IP-literal request URLs, which
        /// never reach the resolver.
        pub fn client_builder(self) -> reqwest::ClientBuilder {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .dns_resolver(self.resolver())
        }
    }

    /// `reqwest` DNS resolver that refuses names resolving to addresses the
    /// policy does not allow.
    #[derive(Debug, Clone, Copy)]
    pub struct PolicyResolver {
        policy: OutboundPolicy,
    }

    impl PolicyResolver {
        /// The policy enforced.
        pub const fn policy(&self) -> OutboundPolicy {
            self.policy
        }
    }

    impl reqwest::dns::Resolve for PolicyResolver {
        fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let policy = self.policy;
            let host = name.as_str().to_owned();
            Box::pin(async move {
                let addrs = policy.resolve_host(&host, 0).await?;
                let addrs: reqwest::dns::Addrs = Box::new(addrs.into_iter());
                Ok(addrs)
            })
        }
    }

    /// The [`OutboundError`] behind a (`reqwest`) error, if the request was
    /// refused by a [`PolicyResolver`].
    pub fn outbound_error_in<'a>(
        err: &'a (dyn std::error::Error + 'static),
    ) -> Option<&'a OutboundError> {
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
        while let Some(e) = current {
            if let Some(found) = e.downcast_ref::<OutboundError>() {
                return Some(found);
            }
            current = e.source();
        }
        None
    }

    /// Failure of [`read_body_limited`].
    #[derive(Debug, thiserror::Error)]
    pub enum BodyError {
        /// The body is larger than the limit.
        #[error("response body exceeds {limit} bytes")]
        TooLarge {
            /// The limit in bytes.
            limit: usize,
        },
        /// Reading the body failed (timeout, connection reset, ...).
        #[error(transparent)]
        Transport(#[from] reqwest::Error),
    }

    /// Reads the whole body, refusing more than `limit` bytes (checked
    /// against `Content-Length` first, then while streaming).
    pub async fn read_body_limited(
        mut resp: reqwest::Response,
        limit: usize,
    ) -> Result<Vec<u8>, BodyError> {
        let announced = resp.content_length();
        if announced.is_some_and(|len| len > limit as u64) {
            return Err(BodyError::TooLarge { limit });
        }
        let capacity = announced
            .and_then(|len| usize::try_from(len).ok())
            .unwrap_or(0)
            .min(limit)
            .min(1024 * 1024);
        let mut body = Vec::with_capacity(capacity);
        while let Some(chunk) = resp.chunk().await? {
            if body.len().saturating_add(chunk.len()) > limit {
                return Err(BodyError::TooLarge { limit });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(ip: &str) -> AddressClass {
        classify(ip.parse().unwrap())
    }

    /// The offline switch (demo mode) refuses every class under the real
    /// policies; only the tests-only `Unrestricted` ignores it. (Tested
    /// through `allows_when`: `go_offline` is process-wide and one-way and
    /// would break the other tests of this binary.)
    #[test]
    fn offline_refuses_everything_but_unrestricted() {
        use AddressClass as C;
        for class in [C::Public, C::Private, C::Special, C::Loopback, C::Metadata] {
            for policy in [OutboundPolicy::Private, OutboundPolicy::Public] {
                assert!(!policy.allows_when(true, class), "{policy} {class}");
            }
            assert!(OutboundPolicy::Unrestricted.allows_when(true, class));
        }
        assert!(OutboundPolicy::Public.allows_when(false, C::Public));
        assert!(OutboundPolicy::Private.allows_when(false, C::Private));
        assert!(!is_offline(), "no unit test may switch the process offline");
        assert_eq!(
            IntegrationError::from(OutboundError::Offline),
            IntegrationError::Config(OutboundError::Offline.to_string())
        );
    }

    #[test]
    fn classifies_every_address_class() {
        use AddressClass as C;
        let cases = [
            ("93.184.216.34", C::Public),
            ("1.1.1.1", C::Public),
            ("2606:4700:4700::1111", C::Public),
            ("10.1.2.3", C::Private),
            ("172.16.0.1", C::Private),
            ("172.31.255.255", C::Private),
            ("172.32.0.1", C::Public),
            ("192.168.1.1", C::Private),
            ("100.64.0.1", C::Private),
            ("100.127.255.254", C::Private),
            ("100.128.0.1", C::Public),
            ("fd12:3456::1", C::Private),
            ("fc00::1", C::Private),
            ("fec0::1", C::Private),
            ("192.0.2.10", C::Special),
            ("198.51.100.7", C::Special),
            ("203.0.113.9", C::Special),
            ("198.18.0.1", C::Special),
            ("192.0.0.8", C::Special),
            ("2001:db8::1", C::Special),
            ("127.0.0.1", C::Loopback),
            ("127.255.0.9", C::Loopback),
            ("::1", C::Loopback),
            ("169.254.1.1", C::LinkLocal),
            ("fe80::1", C::LinkLocal),
            ("febf::1", C::LinkLocal),
            ("169.254.169.254", C::Metadata),
            ("169.254.170.2", C::Metadata),
            ("100.100.100.200", C::Metadata),
            ("192.0.0.192", C::Metadata),
            ("fd00:ec2::254", C::Metadata),
            ("0.0.0.0", C::Unspecified),
            ("0.1.2.3", C::Unspecified),
            ("::", C::Unspecified),
            ("224.0.0.1", C::Multicast),
            ("239.255.255.250", C::Multicast),
            ("ff02::1", C::Multicast),
            ("255.255.255.255", C::Broadcast),
            ("240.0.0.1", C::Reserved),
            ("::7f00:1", C::Reserved),
            ("::ffff:0:7f00:1", C::Reserved),
            // IPv4-mapped IPv6 is judged as the embedded IPv4 address.
            ("::ffff:127.0.0.1", C::Loopback),
            ("::ffff:169.254.169.254", C::Metadata),
            ("::ffff:10.0.0.1", C::Private),
            ("::ffff:0.0.0.0", C::Unspecified),
            ("::ffff:224.0.0.1", C::Multicast),
            ("::ffff:8.8.8.8", C::Public),
            // NAT64 well-known prefix.
            ("64:ff9b::7f00:1", C::Loopback),
            ("64:ff9b::a9fe:a9fe", C::Metadata),
            ("64:ff9b::808:808", C::Special),
            ("64:ff9b:1::1", C::Private),
        ];
        for (ip, want) in cases {
            assert_eq!(class(ip), want, "{ip}");
        }
    }

    #[test]
    fn policies_allow_the_documented_classes() {
        use AddressClass as C;
        let always_blocked = [
            C::Loopback,
            C::LinkLocal,
            C::Metadata,
            C::Unspecified,
            C::Multicast,
            C::Broadcast,
            C::Reserved,
        ];
        for c in always_blocked {
            assert!(!OutboundPolicy::Private.allows(c), "{c}");
            assert!(!OutboundPolicy::Public.allows(c), "{c}");
            assert!(OutboundPolicy::Unrestricted.allows(c), "{c}");
        }
        for c in [C::Private, C::Special] {
            assert!(OutboundPolicy::Private.allows(c), "{c}");
            assert!(!OutboundPolicy::Public.allows(c), "{c}");
        }
        for p in [
            OutboundPolicy::Private,
            OutboundPolicy::Public,
            OutboundPolicy::Unrestricted,
        ] {
            assert!(p.allows(C::Public));
        }
    }

    /// `public` mode: what outgoing tenant webhooks may reach
    /// (`hos_billing::notify::is_public_ip` delegates here).
    #[test]
    fn public_mode_matches_webhook_guard() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::808:808",
            "2001:db8::1",
            "198.18.0.1",
            "192.0.0.1",
        ] {
            assert!(
                !OutboundPolicy::Public.allows_ip(ip.parse().unwrap()),
                "{ip}"
            );
        }
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                OutboundPolicy::Public.allows_ip(ip.parse().unwrap()),
                "{ip}"
            );
        }
    }

    #[test]
    fn host_literals_in_every_notation_are_checked() {
        let p = OutboundPolicy::Private;
        for host in [
            "127.0.0.1",
            "2130706433",   // decimal
            "0177.0.0.1",   // octal
            "0x7f.0.0.1",   // hex
            "0x7f.1",       // short form
            "017700000001", // octal, single number
            "127.1",
            "[::1]",
            "::1",
            "[::ffff:127.0.0.1]",
            "::ffff:7f00:1",
            "0",
            "169.254.169.254",
            "0xa9.0xfe.0xa9.0xfe",
            "2852039166", // 169.254.169.254
            "localhost",
            "LOCALHOST.",
            "api.localhost",
        ] {
            assert!(
                matches!(p.check_host(host), Err(OutboundError::Blocked { .. })),
                "{host}: {:?}",
                p.check_host(host)
            );
        }
        assert_eq!(
            p.check_host("10.0.0.5").unwrap(),
            Some("10.0.0.5".parse().unwrap())
        );
        assert_eq!(
            p.check_host("012.0.0.5").unwrap(),
            Some("10.0.0.5".parse().unwrap())
        );
        assert_eq!(p.check_host("pve.example.com").unwrap(), None);
        assert!(matches!(
            OutboundPolicy::Public.check_host("167772165"), // 10.0.0.5
            Err(OutboundError::Blocked {
                class: AddressClass::Private,
                ..
            })
        ));
        assert!(matches!(
            p.check_host("bad host"),
            Err(OutboundError::InvalidEndpoint(_))
        ));
        assert!(matches!(
            p.check_host(""),
            Err(OutboundError::InvalidEndpoint(_))
        ));
        assert_eq!(
            OutboundPolicy::Unrestricted
                .check_host("127.0.0.1")
                .unwrap(),
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(
            OutboundPolicy::Unrestricted
                .check_host("localhost")
                .unwrap(),
            None
        );
    }

    #[test]
    fn urls_are_checked() {
        let p = OutboundPolicy::Private;
        let url = |s: &str| url::Url::parse(s).unwrap();
        assert!(
            p.check_url(&url("https://pve.example.com:8006/api2/json"))
                .is_ok()
        );
        assert!(p.check_url(&url("https://10.0.0.5:8006/")).is_ok());
        for bad in [
            "http://127.0.0.1:8080/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://localhost:9000/",
        ] {
            assert!(
                matches!(p.check_url(&url(bad)), Err(OutboundError::Blocked { .. })),
                "{bad}"
            );
        }
        assert!(matches!(
            OutboundPolicy::Public.check_url(&url("https://192.168.0.10/")),
            Err(OutboundError::Blocked { .. })
        ));
        assert!(matches!(
            p.check_url(&url("ftp://example.com/")),
            Err(OutboundError::InvalidEndpoint(_))
        ));
        assert!(matches!(
            p.check_url(&url("https://u:p@example.com/")),
            Err(OutboundError::InvalidEndpoint(_))
        ));
    }

    #[test]
    fn policy_parsing() {
        assert_eq!(OutboundPolicy::parse("").unwrap(), OutboundPolicy::Private);
        assert_eq!(
            OutboundPolicy::parse(" Private ").unwrap(),
            OutboundPolicy::Private
        );
        assert_eq!(
            OutboundPolicy::parse("PUBLIC").unwrap(),
            OutboundPolicy::Public
        );
        for bad in ["unrestricted", "off", "none", "allow-all", "x"] {
            assert!(OutboundPolicy::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(OutboundPolicy::default(), OutboundPolicy::Private);
    }

    #[test]
    fn errors_map_to_integration_errors() {
        let blocked = OutboundError::Blocked {
            host: "127.0.0.1".into(),
            class: AddressClass::Loopback,
            policy: OutboundPolicy::Private,
        };
        let e = IntegrationError::from(blocked);
        assert_eq!(e.kind(), "config");
        assert!(!e.is_retryable());
        assert!(e.to_string().contains("loopback"));
        assert!(e.to_string().contains("HOS_OUTBOUND_POLICY=private"));
        let e = IntegrationError::from(OutboundError::Unresolvable("x.invalid".into()));
        assert_eq!(e.kind(), "transport");
    }

    #[cfg(feature = "net")]
    mod enforcement {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn resolve_host_checks_literals_and_names() {
            let p = OutboundPolicy::Private;
            assert!(matches!(
                p.resolve_host("localhost", 25).await,
                Err(OutboundError::Blocked { .. })
            ));
            assert!(matches!(
                p.resolve_host("[::1]", 25).await,
                Err(OutboundError::Blocked { .. })
            ));
            assert!(matches!(
                p.resolve_host("2130706433", 25).await,
                Err(OutboundError::Blocked { .. })
            ));
            let ok = p.resolve_host("10.0.0.7", 587).await.unwrap();
            assert_eq!(ok, vec!["10.0.0.7:587".parse().unwrap()]);
            let mapped = OutboundPolicy::Unrestricted
                .resolve_host("::ffff:127.0.0.1", 25)
                .await
                .unwrap();
            assert_eq!(mapped, vec!["127.0.0.1:25".parse().unwrap()]);
            assert!(matches!(
                p.resolve_host("no-such-host.invalid", 25).await,
                Err(OutboundError::Unresolvable(_))
            ));
        }

        #[tokio::test]
        async fn guarded_client_refuses_names_resolving_to_loopback() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
            let port = server.address().port();
            let client = OutboundPolicy::Private.client_builder().build().unwrap();
            // `localhost` goes through the resolver (IP literals do not).
            let err = client
                .get(format!("http://localhost:{port}/"))
                .send()
                .await
                .unwrap_err();
            let found = outbound_error_in(&err).expect("policy error in the chain");
            assert!(matches!(
                found,
                OutboundError::Blocked {
                    class: AddressClass::Loopback,
                    ..
                }
            ));
            assert!(server.received_requests().await.unwrap().is_empty());

            let open = OutboundPolicy::Unrestricted
                .client_builder()
                .build()
                .unwrap();
            let resp = open
                .get(format!("http://localhost:{port}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }

        #[tokio::test]
        async fn guarded_client_does_not_follow_redirects() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/start"))
                .respond_with(
                    ResponseTemplate::new(302)
                        .insert_header("location", "http://169.254.169.254/latest/meta-data/"),
                )
                .mount(&server)
                .await;
            let client = OutboundPolicy::Unrestricted
                .client_builder()
                .build()
                .unwrap();
            let resp = client
                .get(format!("{}/start", server.uri()))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 302);
        }

        #[tokio::test]
        async fn bodies_are_capped() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/big"))
                .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(4096)))
                .mount(&server)
                .await;
            let client = OutboundPolicy::Unrestricted
                .client_builder()
                .build()
                .unwrap();
            let get = || client.get(format!("{}/big", server.uri())).send();
            let small = read_body_limited(get().await.unwrap(), 4096).await.unwrap();
            assert_eq!(small.len(), 4096);
            let err = read_body_limited(get().await.unwrap(), 1024)
                .await
                .unwrap_err();
            assert!(matches!(err, BodyError::TooLarge { limit: 1024 }));
        }
    }
}
