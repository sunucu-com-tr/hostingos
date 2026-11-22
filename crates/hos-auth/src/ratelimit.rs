//! Rate limiting and login lockout.
//!
//! * [`RateLimiter`] — in-memory token bucket per key (use [`rate_key`]:
//!   client IP + action). Per process: behind several app instances each one
//!   enforces its own budget, which is acceptable for brute-force damping.
//!   Memory is bounded (`max_keys`); idle full buckets are pruned.
//! * [`LockoutPolicy`] — pure, persistable per-account policy: after
//!   `free_attempts` consecutive failures the account is locked for
//!   `base_lockout`, doubling per further failure up to `max_lockout`.
//!   State ([`LoginAttempts`]) can live in `users` columns or in the
//!   in-memory [`LockoutTracker`]. Key it by the normalized email **whether or
//!   not the account exists**, so lockout responses do not reveal accounts.
//!   Combine with an IP limiter so one attacker cannot lock many accounts
//!   cheaply, and with progressive delays rather than permanent locks.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `burst` requests per `period`, refilled continuously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Bucket size (max requests in a burst).
    pub burst: u32,
    /// Time to refill a full bucket.
    pub period: Duration,
}

impl RateLimit {
    /// `burst` requests per `period`.
    pub const fn new(burst: u32, period: Duration) -> Self {
        Self { burst, period }
    }

    fn rate_per_sec(&self) -> f64 {
        f64::from(self.burst.max(1)) / self.period.as_secs_f64().max(0.001)
    }
}

/// Suggested limits for auth endpoints (per IP).
pub mod presets {
    use super::RateLimit;
    use std::time::Duration;

    /// Password login: 10 per 5 minutes.
    pub const LOGIN: RateLimit = RateLimit::new(10, Duration::from_secs(300));
    /// TOTP / recovery code: 10 per 5 minutes.
    pub const TWO_FACTOR: RateLimit = RateLimit::new(10, Duration::from_secs(300));
    /// "Forgot password" emails: 5 per 15 minutes.
    pub const PASSWORD_RESET: RateLimit = RateLimit::new(5, Duration::from_secs(900));
    /// Registrations: 5 per hour.
    pub const REGISTER: RateLimit = RateLimit::new(5, Duration::from_secs(3600));
    /// Verification email resends: 3 per 15 minutes.
    pub const EMAIL_VERIFICATION: RateLimit = RateLimit::new(3, Duration::from_secs(900));
    /// OAuth starts/callbacks: 30 per 5 minutes.
    pub const OAUTH: RateLimit = RateLimit::new(30, Duration::from_secs(300));
    /// REST API (per key): 600 per minute.
    pub const API: RateLimit = RateLimit::new(600, Duration::from_secs(60));
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// Allowed; `remaining` whole requests left in the bucket.
    Allowed {
        /// Requests left right now.
        remaining: u32,
    },
    /// Rejected; retry after the given delay (`Retry-After` header).
    Limited {
        /// Time until one request is available again.
        retry_after: Duration,
    },
}

impl RateDecision {
    /// `true` when allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// In-memory token-bucket limiter. Share it via `Arc`.
#[derive(Debug)]
pub struct RateLimiter {
    limit: RateLimit,
    max_keys: usize,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Limiter tracking at most 100 000 keys.
    pub fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            max_keys: 100_000,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Changes the key capacity.
    #[must_use]
    pub fn with_max_keys(mut self, max_keys: usize) -> Self {
        self.max_keys = max_keys.max(1);
        self
    }

    /// The configured limit.
    pub fn limit(&self) -> RateLimit {
        self.limit
    }

    /// Consumes one request for `key` now.
    pub fn check(&self, key: &str) -> RateDecision {
        self.check_at(key, Instant::now())
    }

    /// Consumes one request for `key` at `now` (testable clock).
    pub fn check_at(&self, key: &str, now: Instant) -> RateDecision {
        let burst = f64::from(self.limit.burst);
        let rate = self.limit.rate_per_sec();
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(key) && map.len() >= self.max_keys {
            self.prune(&mut map, now);
        }
        let bucket = map.entry(key.to_owned()).or_insert(Bucket {
            tokens: burst,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(burst);
        bucket.updated = now.max(bucket.updated);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            RateDecision::Allowed {
                remaining: bucket.tokens.floor() as u32,
            }
        } else {
            let wait = ((1.0 - bucket.tokens) / rate).max(0.001);
            RateDecision::Limited {
                retry_after: Duration::from_secs_f64(wait),
            }
        }
    }

    /// Whether `key` is out of requests right now, without consuming one
    /// (check before the work, [`RateLimiter::check`] on failure only).
    pub fn is_limited(&self, key: &str) -> bool {
        self.is_limited_at(key, Instant::now())
    }

    /// [`RateLimiter::is_limited`] at `now` (testable clock).
    pub fn is_limited_at(&self, key: &str, now: Instant) -> bool {
        let map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bucket) = map.get(key) else {
            return false;
        };
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        let tokens =
            (bucket.tokens + elapsed * self.limit.rate_per_sec()).min(f64::from(self.limit.burst));
        tokens < 1.0
    }

    /// Forgets `key` (e.g. after a successful login).
    pub fn reset(&self, key: &str) {
        self.buckets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    /// Number of tracked keys.
    pub fn len(&self) -> usize {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// `true` when no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops buckets that are full again; if still at capacity, evicts the
    /// least recently used one.
    fn prune(&self, map: &mut HashMap<String, Bucket>, now: Instant) {
        let burst = f64::from(self.limit.burst);
        let rate = self.limit.rate_per_sec();
        map.retain(|_, b| {
            b.tokens + now.saturating_duration_since(b.updated).as_secs_f64() * rate < burst
        });
        if map.len() >= self.max_keys
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, b)| b.updated)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
}

/// Limiter key for `ip` + `action`. IPv6 addresses are reduced to their /64
/// (one subscriber usually owns a whole /64), IPv4-mapped IPv6 to IPv4.
pub fn rate_key(ip: IpAddr, action: &str) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => format!("{action}|{v4}"),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{action}|{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

// ---------------------------------------------------------------------------
// Login lockout
// ---------------------------------------------------------------------------

/// Persistable failed-login state of one account key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginAttempts {
    /// Consecutive failures.
    pub failures: u32,
    /// Time of the last failure.
    pub last_failure_at: Option<DateTime<Utc>>,
    /// Locked until this time.
    pub locked_until: Option<DateTime<Utc>>,
}

/// Progressive lockout policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockoutPolicy {
    /// Failures allowed before the first lock.
    pub free_attempts: u32,
    /// First lock duration; doubles per further failure.
    pub base_lockout: chrono::Duration,
    /// Upper bound of a lock.
    pub max_lockout: chrono::Duration,
    /// Failures older than this are forgotten.
    pub reset_after: chrono::Duration,
}

impl Default for LockoutPolicy {
    /// 5 free attempts, then 1 min, 2 min, 4 min ... capped at 1 h; reset after 1 h.
    fn default() -> Self {
        Self {
            free_attempts: 5,
            base_lockout: chrono::Duration::minutes(1),
            max_lockout: chrono::Duration::hours(1),
            reset_after: chrono::Duration::hours(1),
        }
    }
}

impl LockoutPolicy {
    /// `Err(remaining)` while locked. Check **before** verifying the password.
    pub fn check(&self, state: &LoginAttempts, now: DateTime<Utc>) -> Result<(), chrono::Duration> {
        match state.locked_until {
            Some(until) if until > now => Err(until - now),
            _ => Ok(()),
        }
    }

    fn lock_duration(&self, failures: u32) -> Option<chrono::Duration> {
        let over = failures.checked_sub(self.free_attempts)?;
        let factor = 1i32.checked_shl(over.min(30)).unwrap_or(i32::MAX);
        let lock = self
            .base_lockout
            .checked_mul(factor)
            .unwrap_or(self.max_lockout);
        Some(lock.min(self.max_lockout))
    }

    /// New state after a failed attempt at `now`.
    pub fn record_failure(&self, state: &LoginAttempts, now: DateTime<Utc>) -> LoginAttempts {
        let stale = state
            .last_failure_at
            .is_some_and(|t| now - t >= self.reset_after);
        let failures = if stale {
            1
        } else {
            state.failures.saturating_add(1)
        };
        LoginAttempts {
            failures,
            last_failure_at: Some(now),
            locked_until: self.lock_duration(failures).map(|d| now + d),
        }
    }

    /// State after a successful login (everything cleared).
    pub fn record_success(&self) -> LoginAttempts {
        LoginAttempts::default()
    }
}

/// In-memory [`LockoutPolicy`] store keyed by account (normalized email).
#[derive(Debug)]
pub struct LockoutTracker {
    policy: LockoutPolicy,
    max_keys: usize,
    states: Mutex<HashMap<String, LoginAttempts>>,
}

impl LockoutTracker {
    /// Tracker for at most 100 000 keys.
    pub fn new(policy: LockoutPolicy) -> Self {
        Self {
            policy,
            max_keys: 100_000,
            states: Mutex::new(HashMap::new()),
        }
    }

    /// `Err(remaining)` while `key` is locked.
    pub fn check(&self, key: &str, now: DateTime<Utc>) -> Result<(), chrono::Duration> {
        let map = self.states.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key).map_or(Ok(()), |s| self.policy.check(s, now))
    }

    /// Records a failure; returns the new state.
    pub fn record_failure(&self, key: &str, now: DateTime<Utc>) -> LoginAttempts {
        let mut map = self.states.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(key) && map.len() >= self.max_keys {
            let reset_after = self.policy.reset_after;
            map.retain(|_, s| s.last_failure_at.is_some_and(|t| now - t < reset_after));
            if map.len() >= self.max_keys
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, s)| s.last_failure_at)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        let current = map.get(key).copied().unwrap_or_default();
        let next = self.policy.record_failure(&current, now);
        map.insert(key.to_owned(), next);
        next
    }

    /// Clears `key` after a successful login.
    pub fn record_success(&self, key: &str) {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn token_bucket() {
        let rl = RateLimiter::new(RateLimit::new(3, Duration::from_secs(60)));
        let t0 = Instant::now();
        assert_eq!(rl.check_at("k", t0), RateDecision::Allowed { remaining: 2 });
        assert_eq!(rl.check_at("k", t0), RateDecision::Allowed { remaining: 1 });
        assert_eq!(rl.check_at("k", t0), RateDecision::Allowed { remaining: 0 });
        match rl.check_at("k", t0) {
            RateDecision::Limited { retry_after } => {
                assert!(
                    (19..=20).contains(&retry_after.as_secs()),
                    "{retry_after:?}"
                );
            }
            other => panic!("expected limited, got {other:?}"),
        }
        // Other keys are independent.
        assert!(rl.check_at("other", t0).is_allowed());
        // One token refills after period/burst = 20 s.
        assert!(rl.check_at("k", t0 + Duration::from_secs(21)).is_allowed());
        assert!(!rl.check_at("k", t0 + Duration::from_secs(21)).is_allowed());
        // Full refill after a long pause, capped at burst.
        assert_eq!(
            rl.check_at("k", t0 + Duration::from_secs(3600)),
            RateDecision::Allowed { remaining: 2 }
        );
        rl.reset("k");
        assert_eq!(rl.len(), 1);
    }

    #[test]
    fn is_limited_peeks_without_consuming() {
        let rl = RateLimiter::new(RateLimit::new(2, Duration::from_secs(60)));
        let t0 = Instant::now();
        assert!(!rl.is_limited_at("k", t0), "unknown key");
        for _ in 0..10 {
            assert!(!rl.is_limited_at("k", t0), "peeking consumes nothing");
        }
        assert!(rl.check_at("k", t0).is_allowed());
        assert!(!rl.is_limited_at("k", t0));
        assert!(rl.check_at("k", t0).is_allowed());
        assert!(rl.is_limited_at("k", t0));
        assert!(
            !rl.is_limited_at("k", t0 + Duration::from_secs(30)),
            "refilled"
        );
    }

    #[test]
    fn bounded_memory() {
        let rl = RateLimiter::new(RateLimit::new(2, Duration::from_secs(10))).with_max_keys(3);
        let t0 = Instant::now();
        for k in ["a", "b", "c"] {
            rl.check_at(k, t0);
        }
        // All buckets still partially used → LRU eviction keeps the size bounded.
        rl.check_at("d", t0 + Duration::from_millis(1));
        assert_eq!(rl.len(), 3);
        // Later, refilled buckets are pruned.
        rl.check_at("e", t0 + Duration::from_secs(60));
        assert!(rl.len() <= 3);
        assert!(!rl.is_empty());
    }

    #[test]
    fn keys() {
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(rate_key(v4, "login"), "login|203.0.113.7");
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(rate_key(mapped, "login"), "login|203.0.113.7");
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        assert_eq!(rate_key(a, "login"), rate_key(b, "login"));
        assert_eq!(rate_key(a, "login"), "login|2001:db8:1:2::/64");
    }

    #[test]
    fn lockout_progression() {
        let p = LockoutPolicy::default();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut s = LoginAttempts::default();
        for i in 0..5 {
            assert!(p.check(&s, t0).is_ok(), "attempt {i}");
            s = p.record_failure(&s, t0);
        }
        // 5th failure → 1 minute lock.
        assert_eq!(s.failures, 5);
        assert_eq!(p.check(&s, t0), Err(chrono::Duration::minutes(1)));
        assert!(p.check(&s, t0 + chrono::Duration::minutes(1)).is_ok());
        s = p.record_failure(&s, t0 + chrono::Duration::minutes(1));
        assert_eq!(
            s.locked_until,
            Some(t0 + chrono::Duration::minutes(1) + chrono::Duration::minutes(2))
        );
        // Capped at max.
        for _ in 0..40 {
            s = p.record_failure(&s, t0 + chrono::Duration::minutes(2));
        }
        assert_eq!(
            s.locked_until,
            Some(t0 + chrono::Duration::minutes(2) + chrono::Duration::hours(1))
        );
        // Old failures are forgotten.
        let later = t0 + chrono::Duration::hours(5);
        let fresh = p.record_failure(&s, later);
        assert_eq!(fresh.failures, 1);
        assert_eq!(fresh.locked_until, None);
        assert_eq!(p.record_success(), LoginAttempts::default());
    }

    #[test]
    fn tracker() {
        let t = LockoutTracker::new(LockoutPolicy {
            free_attempts: 2,
            ..LockoutPolicy::default()
        });
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert!(t.check("a@x.io", now).is_ok());
        t.record_failure("a@x.io", now);
        assert!(t.check("a@x.io", now).is_ok());
        let s = t.record_failure("a@x.io", now);
        assert_eq!(s.failures, 2);
        assert!(t.check("a@x.io", now).is_err());
        assert!(t.check("b@x.io", now).is_ok());
        t.record_success("a@x.io");
        assert!(t.check("a@x.io", now).is_ok());
    }
}
