//! [`Billing`]: the shared handle every billing operation runs on (pool,
//! integration registry, clock, URLs, hooks).

use std::sync::Arc;

use hos_core::async_trait;
use hos_core::crypto::SecretBox;
use hos_core::ids::TenantId;
use hos_core::tenant::TenantContext;
use hos_db::PgPool;
use hos_db::repo::clients::ClientRow;
use hos_db::repo::orders::OrderRow;
use rust_decimal::Decimal;
use serde_json::Value;

use crate::BillingResult;
use crate::clock::{Clock, SystemClock};
use crate::registry::IntegrationRegistry;

/// Result of a [`FraudCheck`].
#[derive(Debug, Clone, PartialEq)]
pub struct FraudVerdict {
    /// Risk score (0 = clean); stored in `orders.fraud_score`.
    pub score: Decimal,
    /// Provider details; stored in `orders.fraud_details`.
    pub details: Value,
    /// `true`: the order is moved to `fraud` and nothing is provisioned.
    pub flagged: bool,
}

impl FraudVerdict {
    /// A clean verdict.
    pub fn clean() -> Self {
        Self {
            score: Decimal::ZERO,
            details: Value::Object(serde_json::Map::new()),
            flagged: false,
        }
    }
}

/// Hook run before an order is accepted (MaxMind, FraudLabs, custom rules...).
#[async_trait]
pub trait FraudCheck: Send + Sync + 'static {
    /// Scores an order.
    async fn check(&self, order: &OrderRow, client: &ClientRow) -> BillingResult<FraudVerdict>;
}

/// The default hook: every order is clean.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFraudCheck;

#[async_trait]
impl FraudCheck for NoFraudCheck {
    async fn check(&self, _order: &OrderRow, _client: &ClientRow) -> BillingResult<FraudVerdict> {
        Ok(FraudVerdict::clean())
    }
}

/// Static configuration.
#[derive(Debug, Clone)]
pub struct BillingConfig {
    /// Public base URL (`HOS_BASE_URL`, e.g. `https://panel.example.com`), used
    /// for gateway webhooks and links in e-mails when a tenant has no domain
    /// or `use_tenant_domains` is off.
    pub base_url: String,
    /// Build tenant URLs as `https://{tenants.primary_domain}` (production).
    /// Off in development, where every tenant is served from `base_url`.
    pub use_tenant_domains: bool,
    /// Delay between polls of an asynchronous hypervisor task (default 10 s;
    /// tests use zero).
    pub vm_poll_interval: std::time::Duration,
    /// Allow the simulated `mock` gateway / hypervisor / panel / registrar
    /// (demo seed, tests). Defaults to `HOS_ALLOW_MOCK_INTEGRATIONS`; when set,
    /// [`Billing::with_parts`] enables them on the registry.
    pub allow_mock_integrations: bool,
}

impl BillingConfig {
    /// Config with `base_url` and tenant domains disabled; mock providers
    /// follow `HOS_ALLOW_MOCK_INTEGRATIONS`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            use_tenant_domains: false,
            vm_poll_interval: std::time::Duration::from_secs(10),
            allow_mock_integrations: crate::registry::mock_integrations_allowed_by_env(),
        }
    }

    /// Public base URL of a tenant (no trailing slash).
    pub fn tenant_url(&self, tenant: Option<&TenantContext>) -> String {
        match tenant.and_then(|t| t.primary_domain.as_deref()) {
            Some(domain) if self.use_tenant_domains && !domain.trim().is_empty() => {
                format!("https://{}", domain.trim().trim_end_matches('/'))
            }
            _ => self.base_url.clone(),
        }
    }
}

struct Inner {
    pool: PgPool,
    registry: IntegrationRegistry,
    clock: Arc<dyn Clock>,
    config: BillingConfig,
    fraud: Arc<dyn FraudCheck>,
}

/// Shared billing engine handle (cheap to clone). Operations are methods in
/// the [`checkout`](crate::checkout), [`payments`](crate::payments),
/// [`provisioning`](crate::provisioning), [`lifecycle`](crate::lifecycle),
/// [`automation`](crate::automation), [`notify`](crate::notify) and
/// [`reports`](crate::reports) modules.
#[derive(Clone)]
pub struct Billing {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Billing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Billing")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl Billing {
    /// Engine over `pool` with the system clock and no fraud check.
    pub fn new(pool: PgPool, secret_box: SecretBox, config: BillingConfig) -> Self {
        let registry = IntegrationRegistry::new(pool.clone(), secret_box);
        Self::with_parts(
            pool,
            registry,
            Arc::new(SystemClock),
            config,
            Arc::new(NoFraudCheck),
        )
    }

    /// Engine from explicit parts. `config.allow_mock_integrations` enables
    /// the mock providers on `registry` (it never disables them).
    pub fn with_parts(
        pool: PgPool,
        registry: IntegrationRegistry,
        clock: Arc<dyn Clock>,
        config: BillingConfig,
        fraud: Arc<dyn FraudCheck>,
    ) -> Self {
        if config.allow_mock_integrations {
            registry.set_mock_integrations_allowed(true);
        }
        Self {
            inner: Arc::new(Inner {
                pool,
                registry,
                clock,
                config,
                fraud,
            }),
        }
    }

    /// Same engine with another clock.
    #[must_use]
    pub fn with_clock(&self, clock: Arc<dyn Clock>) -> Self {
        Self::with_parts(
            self.inner.pool.clone(),
            self.inner.registry.clone(),
            clock,
            self.inner.config.clone(),
            self.inner.fraud.clone(),
        )
    }

    /// Same engine with another fraud hook.
    #[must_use]
    pub fn with_fraud_check(&self, fraud: Arc<dyn FraudCheck>) -> Self {
        Self::with_parts(
            self.inner.pool.clone(),
            self.inner.registry.clone(),
            self.inner.clock.clone(),
            self.inner.config.clone(),
            fraud,
        )
    }

    /// Database pool.
    pub fn pool(&self) -> &PgPool {
        &self.inner.pool
    }

    /// Integration registry.
    pub fn registry(&self) -> &IntegrationRegistry {
        &self.inner.registry
    }

    /// Time source.
    pub fn clock(&self) -> &dyn Clock {
        self.inner.clock.as_ref()
    }

    /// Static configuration.
    pub fn config(&self) -> &BillingConfig {
        &self.inner.config
    }

    /// Fraud hook.
    pub fn fraud(&self) -> &dyn FraudCheck {
        self.inner.fraud.as_ref()
    }

    /// Secret box (`HOS_SECRET_KEY`).
    pub fn secret_box(&self) -> &SecretBox {
        self.inner.registry.secret_box()
    }

    /// Tenant context (branding, domain, locale).
    pub async fn tenant(&self, tenant_id: TenantId) -> BillingResult<TenantContext> {
        hos_db::repo::platform::tenant_context(&self.inner.pool, tenant_id)
            .await?
            .ok_or_else(|| hos_core::CoreError::not_found("tenant", tenant_id))
    }

    /// Public base URL of a tenant.
    pub async fn tenant_url(&self, tenant_id: TenantId) -> BillingResult<String> {
        if !self.inner.config.use_tenant_domains {
            return Ok(self.inner.config.base_url.clone());
        }
        let ctx = self.tenant(tenant_id).await?;
        Ok(self.inner.config.tenant_url(Some(&ctx)))
    }
}
