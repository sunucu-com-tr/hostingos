//! Small helpers shared by the providers.

use std::net::IpAddr;
use std::time::Duration;

use hos_core::IntegrationError;
use hos_core::integrations::virt::{IpConfig, OsFamily, VmSpec};
use hos_core::secret::SecretString;
use rand::RngExt;
use serde_json::Value;

pub(crate) const MIB: u64 = 1024 * 1024;
pub(crate) const GIB: u64 = 1024 * MIB;

/// Lenient accessors over provider JSON (numbers may arrive as strings,
/// booleans as `0`/`1`).
pub(crate) trait JsonExt {
    /// String field (non-empty).
    fn str_at(&self, key: &str) -> Option<&str>;
    /// Non-negative integer field (number, float or numeric string).
    fn u64_at(&self, key: &str) -> Option<u64>;
    /// Float field (number or numeric string).
    fn f64_at(&self, key: &str) -> Option<f64>;
    /// Boolean field (`true`, `1`, `"1"`, `"true"`).
    fn bool_at(&self, key: &str) -> Option<bool>;
}

impl JsonExt for Value {
    fn str_at(&self, key: &str) -> Option<&str> {
        self.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn u64_at(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(value_u64)
    }

    fn f64_at(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn bool_at(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => n.as_i64().map(|n| n != 0),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" | "" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Non-negative integer from a number, float or numeric string.
pub(crate) fn value_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<u64>().ok().or_else(|| {
                s.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite() && *f >= 0.0)
                    .map(|f| f as u64)
            })
        }
        _ => None,
    }
}

/// Bytes → MiB (rounded down).
pub(crate) fn bytes_to_mib(bytes: u64) -> u64 {
    bytes / MIB
}

/// Bytes → GiB (rounded to nearest).
pub(crate) fn bytes_to_gib(bytes: u64) -> u64 {
    (bytes + GIB / 2) / GIB
}

/// Bytes → GiB as float.
pub(crate) fn bytes_to_gib_f64(bytes: f64) -> f64 {
    bytes / GIB as f64
}

/// Parses a provider size string such as `32G`, `512M`, `1T`, `2048` (bytes)
/// into bytes.
pub(crate) fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = s
        .find(|c: char| c.is_ascii_alphabetic())
        .map_or((s, ""), |i| (&s[..i], &s[i..]));
    let value: f64 = num.trim().parse().ok()?;
    let mul = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1u64,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => MIB,
        "G" | "GB" | "GIB" => GIB,
        "T" | "TB" | "TIB" => GIB * 1024,
        _ => return None,
    };
    (value.is_finite() && value >= 0.0).then_some((value * mul as f64) as u64)
}

/// Splits addresses into `(ipv4, ipv6)`, skipping link-local / loopback.
pub(crate) fn split_ips<'a>(
    addrs: impl IntoIterator<Item = &'a str>,
) -> (Vec<IpAddr>, Vec<IpAddr>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for raw in addrs {
        let host = raw.split('/').next().unwrap_or(raw).trim();
        let Ok(ip) = host.parse::<IpAddr>() else {
            continue;
        };
        if ip.is_loopback() || ip.is_unspecified() {
            continue;
        }
        match ip {
            IpAddr::V4(a) if a.is_link_local() => {}
            IpAddr::V4(_) => {
                if !v4.contains(&ip) {
                    v4.push(ip);
                }
            }
            IpAddr::V6(a) if (a.segments()[0] & 0xffc0) == 0xfe80 => {}
            IpAddr::V6(_) => {
                if !v6.contains(&ip) {
                    v6.push(ip);
                }
            }
        }
    }
    (v4, v6)
}

/// Static addresses requested by a spec (known before the provider answers).
pub(crate) fn static_ips(spec: &VmSpec) -> (Vec<IpAddr>, Vec<IpAddr>) {
    let pick = |c: &Option<IpConfig>| match c {
        Some(IpConfig::Static(s)) => vec![s.address],
        _ => Vec::new(),
    };
    (pick(&spec.network.ipv4), pick(&spec.network.ipv6))
}

/// Generates a random password of `len` characters (letters + digits, at
/// least one of each class) using the thread-local CSPRNG.
pub(crate) fn generate_password(len: usize) -> SecretString {
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGITS: &[u8] = b"23456789";
    let len = len.max(8);
    let mut rng = rand::rng();
    let all: Vec<u8> = [LOWER, UPPER, DIGITS].concat();
    let mut out: Vec<u8> = (0..len)
        .map(|_| all[rng.random_range(0..all.len())])
        .collect();
    // Guarantee every class once (positions 0..3 are as random as the rest).
    out[0] = LOWER[rng.random_range(0..LOWER.len())];
    out[1] = UPPER[rng.random_range(0..UPPER.len())];
    out[2] = DIGITS[rng.random_range(0..DIGITS.len())];
    for i in (1..out.len()).rev() {
        let j = rng.random_range(0..=i);
        out.swap(i, j);
    }
    SecretString::from(String::from_utf8_lossy(&out).into_owned())
}

/// Percent-encodes everything except RFC 3986 unreserved characters
/// (`A-Z a-z 0-9 - _ . ~`); spaces become `%20`, never `+`.
pub(crate) fn percent_encode_strict(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Makes a DNS-label-safe name (`[a-z0-9-.]`, no leading/trailing `-`/`.`,
/// at most 63 chars per label, 253 overall).
pub(crate) fn dns_name(s: &str) -> String {
    let lowered = s.trim().to_ascii_lowercase();
    let labels: Vec<String> = lowered
        .split('.')
        .map(|label| {
            let cleaned: String = label
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let trimmed = cleaned.trim_matches('-');
            trimmed
                .chars()
                .take(63)
                .collect::<String>()
                .trim_end_matches('-')
                .to_owned()
        })
        .filter(|l| !l.is_empty())
        .collect();
    let mut out = labels.join(".");
    if out.len() > 253 {
        out.truncate(253);
        out = out.trim_end_matches(['.', '-']).to_owned();
    }
    out
}

/// Splits an FQDN into `(host, Some(domain))`.
pub(crate) fn split_fqdn(fqdn: &str) -> (&str, Option<&str>) {
    match fqdn.split_once('.') {
        Some((host, domain)) if !domain.is_empty() => (host, Some(domain)),
        _ => (fqdn, None),
    }
}

/// Snapshot name valid for Proxmox (`[A-Za-z][A-Za-z0-9_-]{1,39}`), also fine
/// for every other provider.
pub(crate) fn snapshot_name(raw: &str) -> String {
    let mut out: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !out.starts_with(|c: char| c.is_ascii_alphabetic()) {
        out.insert(0, 's');
    }
    out.truncate(40);
    if out.len() < 2 {
        out.push_str("nap");
    }
    out
}

/// Best-effort OS family from a template/image name.
pub(crate) fn guess_os_family(name: &str) -> Option<OsFamily> {
    let n = name.to_ascii_lowercase();
    if n.contains("win") {
        Some(OsFamily::Windows)
    } else if n.contains("bsd") {
        Some(OsFamily::Bsd)
    } else if [
        "ubuntu",
        "debian",
        "centos",
        "alma",
        "rocky",
        "fedora",
        "suse",
        "linux",
        "arch",
        "alpine",
        "oracle",
        "rhel",
        "cloudlinux",
        "gentoo",
        "devuan",
        "kali",
        "mint",
    ]
    .iter()
    .any(|d| n.contains(d))
    {
        Some(OsFamily::Linux)
    } else {
        None
    }
}

/// Reads an optional string from a spec's `extra` object.
pub(crate) fn extra_str<'a>(extra: &'a Value, key: &str) -> Option<&'a str> {
    extra.str_at(key)
}

/// Reads an optional integer from a spec's `extra` object; non-numbers are an error.
pub(crate) fn extra_u64(extra: &Value, key: &str) -> Result<Option<u64>, IntegrationError> {
    match extra.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => value_u64(v).map(Some).ok_or_else(|| {
            IntegrationError::InvalidInput(format!("extra.{key} must be a non-negative integer"))
        }),
    }
}

/// Reads an optional boolean from a spec's `extra` object.
pub(crate) fn extra_bool(extra: &Value, key: &str) -> Option<bool> {
    extra.bool_at(key)
}

/// Comma/space separated list → trimmed non-empty items.
pub(crate) fn split_list(s: &str) -> Vec<String> {
    s.split([',', ';', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Polls `check` every `interval` until it returns `Some`, or fails with
/// [`IntegrationError::Timeout`] after `timeout`.
pub(crate) async fn poll_until<T, F, Fut>(
    timeout: Duration,
    interval: Duration,
    mut check: F,
) -> Result<T, IntegrationError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>, IntegrationError>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = check().await? {
            return Ok(value);
        }
        if tokio::time::Instant::now() + interval > deadline {
            return Err(IntegrationError::Timeout);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Validates the common parts of a [`VmSpec`].
pub(crate) fn validate_spec(spec: &VmSpec) -> Result<(), IntegrationError> {
    if spec.template.trim().is_empty() {
        return Err(IntegrationError::InvalidInput(
            "template is required".into(),
        ));
    }
    if spec.name.trim().is_empty() && spec.hostname.trim().is_empty() {
        return Err(IntegrationError::InvalidInput(
            "name or hostname is required".into(),
        ));
    }
    if spec.cpu_cores == 0 {
        return Err(IntegrationError::InvalidInput(
            "cpu_cores must be at least 1".into(),
        ));
    }
    if spec.memory_mb < 64 {
        return Err(IntegrationError::InvalidInput(
            "memory_mb must be at least 64".into(),
        ));
    }
    for key in &spec.ssh_keys {
        validate_ssh_key(key)?;
    }
    Ok(())
}

/// Rejects obviously malformed OpenSSH public keys (and private keys pasted by mistake).
pub(crate) fn validate_ssh_key(key: &str) -> Result<(), IntegrationError> {
    let key = key.trim();
    let mut parts = key.split_whitespace();
    let (Some(kind), Some(body)) = (parts.next(), parts.next()) else {
        return Err(IntegrationError::InvalidInput(
            "malformed SSH public key".into(),
        ));
    };
    let known = kind.starts_with("ssh-")
        || kind.starts_with("ecdsa-sha2-")
        || kind.starts_with("sk-ssh-")
        || kind.starts_with("sk-ecdsa-");
    if !known || body.len() < 16 || key.contains('\n') || key.contains("PRIVATE KEY") {
        return Err(IntegrationError::InvalidInput(
            "malformed SSH public key".into(),
        ));
    }
    Ok(())
}

/// `type base64` part of an OpenSSH key (comment dropped) for comparisons.
pub(crate) fn ssh_key_identity(key: &str) -> String {
    key.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lenient_json() {
        let v = json!({"a": "12", "b": 3.7, "c": "1", "d": 0, "e": "", "f": "x"});
        assert_eq!(v.u64_at("a"), Some(12));
        assert_eq!(v.u64_at("b"), Some(3));
        assert_eq!(v.bool_at("c"), Some(true));
        assert_eq!(v.bool_at("d"), Some(false));
        assert_eq!(v.str_at("e"), None);
        assert_eq!(v.u64_at("f"), None);
        assert_eq!(v.f64_at("a"), Some(12.0));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("32G"), Some(32 * GIB));
        assert_eq!(parse_size("512M"), Some(512 * MIB));
        assert_eq!(parse_size("1T"), Some(1024 * GIB));
        assert_eq!(parse_size("2048"), Some(2048));
        assert_eq!(parse_size("1.5G"), Some(GIB + GIB / 2));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(bytes_to_gib(32 * GIB), 32);
        assert_eq!(bytes_to_mib(2 * GIB), 2048);
    }

    #[test]
    fn strict_percent_encoding() {
        assert_eq!(
            percent_encode_strict("ssh-ed25519 AAAA+/= me@host\nx"),
            "ssh-ed25519%20AAAA%2B%2F%3D%20me%40host%0Ax"
        );
    }

    #[test]
    fn names() {
        assert_eq!(dns_name("Svc_01 Web.Example.COM"), "svc-01-web.example.com");
        assert_eq!(dns_name("--a--"), "a");
        assert_eq!(snapshot_name("before upgrade!"), "before_upgrade_");
        assert_eq!(snapshot_name("1st"), "s1st");
        assert_eq!(snapshot_name(""), "snap");
        assert_eq!(snapshot_name(&"x".repeat(80)).len(), 40);
        assert_eq!(
            split_fqdn("vps1.example.com"),
            ("vps1", Some("example.com"))
        );
        assert_eq!(split_fqdn("vps1"), ("vps1", None));
    }

    #[test]
    fn passwords_are_random_and_mixed() {
        let a = generate_password(20);
        let b = generate_password(20);
        assert_eq!(a.expose_str().len(), 20);
        assert_ne!(a.expose_str(), b.expose_str());
        let s = a.expose_str();
        assert!(s.chars().any(|c| c.is_ascii_lowercase()));
        assert!(s.chars().any(|c| c.is_ascii_uppercase()));
        assert!(s.chars().any(|c| c.is_ascii_digit()));
    }

    #[test]
    fn ip_split() {
        let (v4, v6) = split_ips([
            "203.0.113.5/24",
            "fe80::1",
            "2001:db8::5",
            "127.0.0.1",
            "nope",
        ]);
        assert_eq!(v4, vec!["203.0.113.5".parse::<IpAddr>().unwrap()]);
        assert_eq!(v6, vec!["2001:db8::5".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn ssh_key_validation() {
        assert!(validate_ssh_key("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGx me@x").is_ok());
        assert!(validate_ssh_key("-----BEGIN OPENSSH PRIVATE KEY-----").is_err());
        assert!(validate_ssh_key("hello").is_err());
        assert_eq!(
            ssh_key_identity("ssh-ed25519 AAAA comment here"),
            "ssh-ed25519 AAAA"
        );
    }

    #[test]
    fn os_family_guess() {
        assert_eq!(guess_os_family("Ubuntu 24.04"), Some(OsFamily::Linux));
        assert_eq!(guess_os_family("win2022-std"), Some(OsFamily::Windows));
        assert_eq!(guess_os_family("FreeBSD 14"), Some(OsFamily::Bsd));
        assert_eq!(guess_os_family("custom"), None);
    }

    #[tokio::test]
    async fn poll_until_times_out() {
        let mut n = 0;
        let r: Result<(), _> =
            poll_until(Duration::from_millis(30), Duration::from_millis(10), || {
                n += 1;
                async { Ok(None) }
            })
            .await;
        assert_eq!(r, Err(IntegrationError::Timeout));
        assert!(n >= 2);
        let r = poll_until(Duration::from_secs(1), Duration::from_millis(1), || async {
            Ok(Some(5))
        })
        .await;
        assert_eq!(r, Ok(5));
    }
}
