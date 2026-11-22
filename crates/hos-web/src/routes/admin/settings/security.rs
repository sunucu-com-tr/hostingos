//! The tenant security policy (`security.*` settings): password rules,
//! mandatory staff 2FA and session lifetimes. Read on every authenticated
//! request (`crate::session`) and by the password forms, so it goes through
//! a short-TTL per-tenant cache that the settings screen invalidates.
//!
//! | Key | Default | Meaning |
//! |---|---|---|
//! | `security.password_min_length` | 10 | minimum password length (8–64) |
//! | `security.password_reject_common` | true | reject common / trivial passwords |
//! | `security.staff_2fa_required` | false | staff must set up TOTP before using `/admin` |
//! | `security.session_idle_minutes` | — (1 h) | idle timeout of normal sessions (5–1440) |
//! | `security.session_absolute_hours` | — (12 h) | absolute lifetime of normal sessions (1–720) |

use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use hos_auth::password::PasswordPolicy;
use hos_auth::session::SessionConfig;
use hos_core::ids::TenantId;
use hos_db::repo::platform;
use serde_json::Value;

use crate::error::AppResult;
use crate::state::AppState;

/// `security.password_min_length`.
pub const KEY_PASSWORD_MIN: &str = "security.password_min_length";
/// `security.password_reject_common`.
pub const KEY_PASSWORD_COMMON: &str = "security.password_reject_common";
/// `security.staff_2fa_required`.
pub const KEY_STAFF_2FA: &str = "security.staff_2fa_required";
/// `security.session_idle_minutes`.
pub const KEY_IDLE_MINUTES: &str = "security.session_idle_minutes";
/// `security.session_absolute_hours`.
pub const KEY_ABSOLUTE_HOURS: &str = "security.session_absolute_hours";

/// Allowed range of the minimum password length.
pub const PASSWORD_MIN_RANGE: (i64, i64) = (8, 64);
/// Allowed range of the idle timeout (minutes).
pub const IDLE_RANGE: (i64, i64) = (5, 1440);
/// Allowed range of the absolute lifetime (hours).
pub const ABSOLUTE_RANGE: (i64, i64) = (1, 720);

const TTL: Duration = Duration::from_secs(30);

/// Effective security settings of a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityPolicy {
    /// Minimum password length.
    pub password_min_length: usize,
    /// Reject common passwords.
    pub password_reject_common: bool,
    /// Staff must have TOTP enabled.
    pub staff_2fa_required: bool,
    /// Idle timeout of normal sessions (`None`: platform default).
    pub session_idle_minutes: Option<i64>,
    /// Absolute lifetime of normal sessions (`None`: platform default).
    pub session_absolute_hours: Option<i64>,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self::from_map(&BTreeMap::new())
    }
}

fn int(map: &BTreeMap<String, Value>, key: &str) -> Option<i64> {
    match map.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn boolean(map: &BTreeMap<String, Value>, key: &str, default: bool) -> bool {
    match map.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim(), "true" | "1" | "on" | "yes"),
        _ => default,
    }
}

fn in_range(v: Option<i64>, (lo, hi): (i64, i64)) -> Option<i64> {
    v.filter(|n| (lo..=hi).contains(n))
}

impl SecurityPolicy {
    /// From `tenant_settings` (invalid / out-of-range values → defaults).
    pub fn from_map(map: &BTreeMap<String, Value>) -> Self {
        Self {
            password_min_length: in_range(int(map, KEY_PASSWORD_MIN), PASSWORD_MIN_RANGE)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(10),
            password_reject_common: boolean(map, KEY_PASSWORD_COMMON, true),
            staff_2fa_required: boolean(map, KEY_STAFF_2FA, false),
            session_idle_minutes: in_range(int(map, KEY_IDLE_MINUTES), IDLE_RANGE),
            session_absolute_hours: in_range(int(map, KEY_ABSOLUTE_HOURS), ABSOLUTE_RANGE),
        }
    }

    /// Password rules for `hos_auth::password`.
    pub fn password_policy(&self) -> PasswordPolicy {
        PasswordPolicy {
            min_chars: self.password_min_length,
            reject_common: self.password_reject_common,
            ..PasswordPolicy::default()
        }
    }

    /// `base` with the tenant's timeouts for normal ("remember me" off) sessions.
    pub fn session_config(&self, base: &SessionConfig) -> SessionConfig {
        let mut cfg = base.clone();
        if let Some(m) = self.session_idle_minutes {
            cfg.idle_timeout = chrono::Duration::minutes(m);
        }
        if let Some(h) = self.session_absolute_hours {
            cfg.absolute_timeout = chrono::Duration::hours(h);
        }
        cfg
    }
}

type Cache = RwLock<HashMap<TenantId, (Instant, SecurityPolicy)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The tenant's policy (cached for a few seconds).
pub async fn policy(state: &AppState, tenant_id: TenantId) -> AppResult<SecurityPolicy> {
    if let Some((at, p)) = cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&tenant_id)
        && at.elapsed() < TTL
    {
        return Ok(p.clone());
    }
    let map = platform::get_settings(&state.pool, tenant_id, "security.").await?;
    let p = SecurityPolicy::from_map(&map);
    let mut w = cache().write().unwrap_or_else(|e| e.into_inner());
    if w.len() > 10_000 {
        w.clear();
    }
    w.insert(tenant_id, (Instant::now(), p.clone()));
    Ok(p)
}

/// Drops the cached policy of a tenant (after the settings change).
pub fn invalidate(tenant_id: TenantId) {
    cache()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&tenant_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_ranges() {
        let d = SecurityPolicy::default();
        assert_eq!(d.password_min_length, 10);
        assert!(d.password_reject_common && !d.staff_2fa_required);
        let mut m = BTreeMap::new();
        m.insert(KEY_PASSWORD_MIN.into(), json!(14));
        m.insert(KEY_STAFF_2FA.into(), json!(true));
        m.insert(KEY_IDLE_MINUTES.into(), json!("2")); // below range → default
        m.insert(KEY_ABSOLUTE_HOURS.into(), json!(8));
        let p = SecurityPolicy::from_map(&m);
        assert_eq!(p.password_min_length, 14);
        assert!(p.staff_2fa_required);
        assert_eq!(p.session_idle_minutes, None);
        let cfg = p.session_config(&SessionConfig::default());
        assert_eq!(cfg.absolute_timeout, chrono::Duration::hours(8));
        assert_eq!(cfg.idle_timeout, SessionConfig::default().idle_timeout);
        assert_eq!(p.password_policy().min_chars, 14);
    }
}
