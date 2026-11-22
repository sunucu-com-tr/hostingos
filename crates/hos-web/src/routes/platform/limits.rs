//! SaaS plan limits for the tenant areas.
//!
//! Limits come from the tenant's live subscription plan
//! (`platform_plans.limits`, `null` / missing = unlimited) and can be
//! overridden per tenant on the platform console (`tenants.metadata.limits`).
//! Call [`check_plan_limit`] **before** creating the counted record:
//!
//! ```ignore
//! use crate::routes::platform::limits::{Limit, check_plan_limit};
//! check_plan_limit(&state, tenant.id, Limit::Clients).await?;   // 409 when full
//! ```

use hos_core::ids::TenantId;
use hos_db::repo::saas::{self, LimitStatus};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// A limited resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Client accounts that are not closed (`max_clients`).
    Clients,
    /// Services pending … suspended (`max_services`).
    Services,
    /// Staff users that are not disabled (`max_staff`).
    Staff,
    /// Custom domains of the tenant (`custom_domains`).
    CustomDomains,
    /// Configured integrations (`integrations`).
    Integrations,
}

impl Limit {
    /// Every limit.
    pub const ALL: &'static [Limit] = &[
        Self::Clients,
        Self::Services,
        Self::Staff,
        Self::CustomDomains,
        Self::Integrations,
    ];

    /// Key in `platform_plans.limits`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Clients => "max_clients",
            Self::Services => "max_services",
            Self::Staff => "max_staff",
            Self::CustomDomains => "custom_domains",
            Self::Integrations => "integrations",
        }
    }

    /// Limit by key.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.key() == key)
    }
}

/// Usage and maximum of `limit` for a tenant.
pub async fn plan_limit_status(
    state: &AppState,
    tenant_id: TenantId,
    limit: Limit,
) -> AppResult<LimitStatus> {
    let status = saas::plan_limit(&state.pool, tenant_id, limit.key()).await?;
    // Plans and their limits belong to multi-company operation: the one
    // company of a single-company installation is never limited.
    if state.web.single() {
        return Ok(LimitStatus {
            used: status.used,
            max: None,
        });
    }
    Ok(status)
}

/// `Ok` when one more `limit` item fits the tenant's plan, otherwise
/// `409 Conflict` with a message safe to show.
pub async fn check_plan_limit(
    state: &AppState,
    tenant_id: TenantId,
    limit: Limit,
) -> AppResult<()> {
    let status = plan_limit_status(state, tenant_id, limit).await?;
    if status.allows(1) {
        return Ok(());
    }
    Err(AppError::Conflict(format!(
        "plan limit reached: {} ({}/{})",
        limit.key(),
        status.used,
        status.max.unwrap_or_default()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        for l in Limit::ALL {
            assert_eq!(Limit::from_key(l.key()), Some(*l));
            assert!(hos_db::repo::saas::LIMIT_KEYS.contains(&l.key()));
        }
        assert_eq!(Limit::from_key("nope"), None);
    }
}
