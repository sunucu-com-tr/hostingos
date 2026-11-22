//! Login sessions.
//!
//! * The browser gets a random 256-bit token ([`hos_core::crypto::generate_token`])
//!   in an `HttpOnly; SameSite=Lax; Path=/` cookie (`Secure` + `__Host-` name
//!   prefix when `HOS_BASE_URL` is https). The database stores only
//!   [`hash_session_token`] (SHA-256), so a DB leak does not leak sessions.
//! * Expiry is two-fold: an **absolute** lifetime fixed at login
//!   (`sessions.expires_at`) and an **idle** timeout measured from
//!   `sessions.last_seen_at`. "Remember me" switches to the longer pair and
//!   makes the cookie persistent (`Max-Age`).
//! * [`SessionConfig::evaluate`] tells `hos-web` whether a loaded row is still
//!   valid and whether `last_seen_at` should be bumped (throttled by
//!   `touch_interval` to avoid a DB write on every request).
//! * Always issue a **new** session on login / privilege change (fixation) and
//!   delete the row on logout.
//!
//! Lookup flow in `hos-web`: `cookie_token(header)` → `hash_session_token` →
//! `select … where token_hash = $1 and tenant_id = $2` → `evaluate(times, now)`.

use chrono::{DateTime, Duration, Utc};
use hos_core::secret::SecretString;
use url::Url;

/// Cookie name over plain http (development).
pub const SESSION_COOKIE: &str = "hos_session";
/// Cookie name over https. The `__Host-` prefix makes browsers require
/// `Secure`, `Path=/` and no `Domain`, so subdomains cannot plant it.
pub const SESSION_COOKIE_SECURE: &str = "__Host-hos_session";

/// Length of a token produced by [`hos_core::crypto::generate_token`].
pub const TOKEN_LEN: usize = 43;

/// Session lifetimes and cookie flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// Adds `Secure` and the `__Host-` prefix (derive from `HOS_BASE_URL`).
    pub secure: bool,
    /// Max inactivity for a normal session.
    pub idle_timeout: Duration,
    /// Max total lifetime for a normal session.
    pub absolute_timeout: Duration,
    /// Max inactivity for a "remember me" session.
    pub remember_idle_timeout: Duration,
    /// Max total lifetime for a "remember me" session.
    pub remember_absolute_timeout: Duration,
    /// Minimum interval between `last_seen_at` updates.
    pub touch_interval: Duration,
}

impl Default for SessionConfig {
    /// Secure cookies, 1 h idle / 12 h absolute, remember-me 14 d idle / 30 d absolute.
    fn default() -> Self {
        Self {
            secure: true,
            idle_timeout: Duration::hours(1),
            absolute_timeout: Duration::hours(12),
            remember_idle_timeout: Duration::days(14),
            remember_absolute_timeout: Duration::days(30),
            touch_interval: Duration::minutes(5),
        }
    }
}

/// A freshly created session. Put `token` in the cookie (via
/// [`SessionConfig::set_cookie`]) and insert the rest into `sessions`.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// Plain token for the cookie. Never store or log it.
    pub token: SecretString,
    /// SHA-256 hex of the token — the `sessions.token_hash` column.
    pub token_hash: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Initial `last_seen_at` (= `created_at`).
    pub last_seen_at: DateTime<Utc>,
    /// Absolute expiry (`sessions.expires_at`).
    pub expires_at: DateTime<Utc>,
    /// Whether this is a "remember me" session.
    pub remember_me: bool,
}

/// The timing columns of a stored session row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimes {
    /// `sessions.created_at`.
    pub created_at: DateTime<Utc>,
    /// `sessions.last_seen_at`.
    pub last_seen_at: DateTime<Utc>,
    /// `sessions.expires_at` (absolute expiry).
    pub expires_at: DateTime<Utc>,
    /// `sessions.remember_me`.
    pub remember_me: bool,
}

/// Result of [`SessionConfig::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Valid. When `touch` is true, update `last_seen_at = now`.
    Active {
        /// Whether `last_seen_at` should be written.
        touch: bool,
    },
    /// Inactive longer than the idle timeout: delete the row, clear the cookie.
    IdleExpired,
    /// Past the absolute lifetime: delete the row, clear the cookie.
    Expired,
}

impl SessionStatus {
    /// `true` for [`SessionStatus::Active`].
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

impl SessionConfig {
    /// Defaults with `secure` derived from the base URL scheme (`https` → secure).
    pub fn for_base_url(base_url: &Url) -> Self {
        Self {
            secure: base_url.scheme() == "https",
            ..Self::default()
        }
    }

    /// Cookie name in use (`__Host-hos_session` when secure).
    pub fn cookie_name(&self) -> &'static str {
        if self.secure {
            SESSION_COOKIE_SECURE
        } else {
            SESSION_COOKIE
        }
    }

    fn timeouts(&self, remember_me: bool) -> (Duration, Duration) {
        if remember_me {
            (self.remember_idle_timeout, self.remember_absolute_timeout)
        } else {
            (self.idle_timeout, self.absolute_timeout)
        }
    }

    /// Creates a new session token and its row values.
    pub fn start(&self, now: DateTime<Utc>, remember_me: bool) -> NewSession {
        let token = hos_core::crypto::generate_token();
        let (_, absolute) = self.timeouts(remember_me);
        NewSession {
            token_hash: hash_session_token(&token),
            token: SecretString::from(token),
            created_at: now,
            last_seen_at: now,
            expires_at: now + absolute,
            remember_me,
        }
    }

    /// Decides whether a stored session is still usable at `now`.
    pub fn evaluate(&self, times: &SessionTimes, now: DateTime<Utc>) -> SessionStatus {
        let (idle, absolute) = self.timeouts(times.remember_me);
        // Both the stored expiry and the current policy apply (a policy
        // tightened after login takes effect immediately).
        if now >= times.expires_at || now >= times.created_at + absolute {
            return SessionStatus::Expired;
        }
        if now >= times.last_seen_at + idle {
            return SessionStatus::IdleExpired;
        }
        SessionStatus::Active {
            touch: now >= times.last_seen_at + self.touch_interval,
        }
    }

    /// `Set-Cookie` value for a session. Non-remember sessions get a browser
    /// session cookie; remember-me sessions persist until `expires_at`.
    pub fn set_cookie(
        &self,
        token: &str,
        remember_me: bool,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> String {
        let max_age = remember_me.then(|| (expires_at - now).num_seconds().max(0));
        build_cookie(
            self.cookie_name(),
            token,
            &CookieAttributes {
                secure: self.secure,
                max_age_secs: max_age,
                ..CookieAttributes::default()
            },
            now,
        )
    }

    /// `Set-Cookie` value that deletes the session cookie.
    pub fn clear_cookie(&self) -> String {
        clear_cookie(self.cookie_name(), self.secure, "/")
    }

    /// Extracts a well-formed session token from a `Cookie` request header.
    pub fn cookie_token<'a>(&self, cookie_header: &'a str) -> Option<&'a str> {
        cookie_value(cookie_header, self.cookie_name()).filter(|t| is_well_formed_token(t))
    }
}

/// SHA-256 hex of a session token (the `sessions.token_hash` column).
pub fn hash_session_token(token: &str) -> String {
    hos_core::crypto::hash_token(token)
}

/// Whether `token` looks like a [`hos_core::crypto::generate_token`] output
/// (43 base64url chars). Reject anything else before touching the DB.
pub fn is_well_formed_token(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// Generic cookie helpers (also used for the OAuth pending-auth cookie)
// ---------------------------------------------------------------------------

/// `SameSite` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// `SameSite=Lax` (default; survives top-level GET navigations, e.g. OAuth callbacks).
    Lax,
    /// `SameSite=Strict`.
    Strict,
}

/// Attributes for [`build_cookie`]. `HttpOnly` is always set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieAttributes<'a> {
    /// Add `Secure`.
    pub secure: bool,
    /// `Path` (default `/`).
    pub path: &'a str,
    /// `Max-Age` (+ `Expires`) in seconds; `None` = session cookie.
    pub max_age_secs: Option<i64>,
    /// `SameSite`.
    pub same_site: SameSite,
}

impl Default for CookieAttributes<'_> {
    fn default() -> Self {
        Self {
            secure: true,
            path: "/",
            max_age_secs: None,
            same_site: SameSite::Lax,
        }
    }
}

/// Builds a `Set-Cookie` header value. `value` must already be cookie-safe
/// (base64url tokens are); other bytes are dropped defensively.
pub fn build_cookie(
    name: &str,
    value: &str,
    attrs: &CookieAttributes<'_>,
    now: DateTime<Utc>,
) -> String {
    let safe_value: String = value
        .chars()
        .filter(|c| c.is_ascii_graphic() && !matches!(c, '"' | ',' | ';' | '\\'))
        .collect();
    let mut cookie = format!("{name}={safe_value}; Path={}; HttpOnly", attrs.path);
    cookie.push_str(match attrs.same_site {
        SameSite::Lax => "; SameSite=Lax",
        SameSite::Strict => "; SameSite=Strict",
    });
    if attrs.secure {
        cookie.push_str("; Secure");
    }
    if let Some(max_age) = attrs.max_age_secs {
        let expires = now + Duration::seconds(max_age);
        cookie.push_str(&format!(
            "; Max-Age={max_age}; Expires={}",
            expires.format("%a, %d %b %Y %H:%M:%S GMT")
        ));
    }
    cookie
}

/// `Set-Cookie` value that removes cookie `name` (same `Path`/`Secure` as when set).
pub fn clear_cookie(name: &str, secure: bool, path: &str) -> String {
    let mut cookie = format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Lax; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// First value of cookie `name` in a `Cookie` request header.
pub fn cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k.trim() == name).then(|| v.trim().trim_matches('"'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap()
    }

    #[test]
    fn start_creates_hashed_token() {
        let cfg = SessionConfig::default();
        let s = cfg.start(t0(), false);
        assert!(is_well_formed_token(s.token.expose_str()));
        assert_eq!(s.token_hash, hash_session_token(s.token.expose_str()));
        assert_ne!(s.token_hash, s.token.expose_str());
        assert_eq!(s.expires_at, t0() + Duration::hours(12));
        let r = cfg.start(t0(), true);
        assert_eq!(r.expires_at, t0() + Duration::days(30));
        assert!(!format!("{s:?}").contains(s.token.expose_str()));
    }

    #[test]
    fn evaluate_idle_and_absolute() {
        let cfg = SessionConfig::default();
        let s = cfg.start(t0(), false);
        let times = SessionTimes {
            created_at: s.created_at,
            last_seen_at: s.last_seen_at,
            expires_at: s.expires_at,
            remember_me: false,
        };
        assert_eq!(
            cfg.evaluate(&times, t0() + Duration::minutes(1)),
            SessionStatus::Active { touch: false }
        );
        assert_eq!(
            cfg.evaluate(&times, t0() + Duration::minutes(10)),
            SessionStatus::Active { touch: true }
        );
        assert_eq!(
            cfg.evaluate(&times, t0() + Duration::minutes(61)),
            SessionStatus::IdleExpired
        );
        // Active user, but past the absolute lifetime.
        let busy = SessionTimes {
            last_seen_at: t0() + Duration::hours(12) - Duration::minutes(1),
            ..times
        };
        assert_eq!(
            cfg.evaluate(&busy, t0() + Duration::hours(12)),
            SessionStatus::Expired
        );

        // Remember me: long idle allowed.
        let remember = SessionTimes {
            remember_me: true,
            expires_at: t0() + Duration::days(30),
            ..times
        };
        assert!(
            cfg.evaluate(&remember, t0() + Duration::days(3))
                .is_active()
        );
        assert_eq!(
            cfg.evaluate(&remember, t0() + Duration::days(15)),
            SessionStatus::IdleExpired
        );
        // Policy tightened after login: stored expiry is not trusted alone.
        let strict = SessionConfig {
            remember_absolute_timeout: Duration::days(2),
            ..cfg.clone()
        };
        let recent = SessionTimes {
            last_seen_at: t0() + Duration::days(2),
            ..remember
        };
        assert_eq!(
            strict.evaluate(&recent, t0() + Duration::days(2) + Duration::minutes(1)),
            SessionStatus::Expired
        );
    }

    #[test]
    fn cookies() {
        let secure = SessionConfig::for_base_url(&Url::parse("https://panel.example.com").unwrap());
        let s = secure.start(t0(), false);
        let c = secure.set_cookie(s.token.expose_str(), false, s.expires_at, t0());
        assert!(c.starts_with(&format!(
            "__Host-hos_session={}; Path=/; HttpOnly",
            s.token.expose_str()
        )));
        assert!(c.contains("; SameSite=Lax"));
        assert!(c.contains("; Secure"));
        assert!(!c.contains("Max-Age"));

        let r = secure.set_cookie(s.token.expose_str(), true, t0() + Duration::days(30), t0());
        assert!(r.contains("; Max-Age=2592000; Expires=Tue, 31 Mar 2026 10:00:00 GMT"));

        let dev = SessionConfig::for_base_url(&Url::parse("http://localhost:8080").unwrap());
        let d = dev.set_cookie("tok", false, t0(), t0());
        assert!(d.starts_with("hos_session=tok;"));
        assert!(!d.contains("Secure"));

        let clear = secure.clear_cookie();
        assert!(clear.starts_with("__Host-hos_session=;"));
        assert!(clear.contains("Max-Age=0"));
        assert!(clear.contains("; Secure"));
    }

    #[test]
    fn cookie_parsing() {
        let cfg = SessionConfig::default();
        let token = hos_core::crypto::generate_token();
        let header = format!("theme=dark; __Host-hos_session={token}; other=1");
        assert_eq!(cfg.cookie_token(&header), Some(token.as_str()));
        assert_eq!(cfg.cookie_token("__Host-hos_session=short"), None);
        assert_eq!(cfg.cookie_token("theme=dark"), None);
        assert_eq!(
            cfg.cookie_token(&format!("__Host-hos_session={}", "a".repeat(42) + "$")),
            None
        );
        assert_eq!(cookie_value("a=1; b=\"2\"", "b"), Some("2"));
        // Injection characters are stripped from values.
        let c = build_cookie("x", "v;Domain=evil", &CookieAttributes::default(), t0());
        assert!(c.starts_with("x=vDomain=evil; Path=/"));
    }
}
