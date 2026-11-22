//! Integration registry: turns `integrations` rows into live provider objects.
//!
//! * loads the row (tenant scoped), decrypts `secrets_enc` with the
//!   [`SecretBox`] and [`integration_aad`], and calls the owning crate's
//!   `build(kind, config, secrets)`;
//! * caches built integrations for a short TTL (default 60 s) and invalidates
//!   on [`IntegrationRegistry::save_integration`] / [`IntegrationRegistry::invalidate`];
//! * exposes the union of all crates' `descriptors()` for the admin form and
//!   connection tests.
//!
//! ## Mock providers
//!
//! The `mock` gateway / hypervisor / panel / registrar simulate everything
//! (a mock gateway lets anyone "pay" any invoice). They are refused — when
//! saved and when an integration is resolved — unless explicitly enabled with
//! `HOS_ALLOW_MOCK_INTEGRATIONS=1` (demo deployments, tests) or
//! [`IntegrationRegistry::set_mock_integrations_allowed`] /
//! [`BillingConfig::allow_mock_integrations`](crate::BillingConfig::allow_mock_integrations).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use hos_core::CoreError;
use hos_core::crypto::{SecretBox, integration_aad};
use hos_core::domain::IntegrationCategory;
use hos_core::error::IntegrationError;
use hos_core::ids::{IntegrationId, TenantId};
use hos_core::integrations::{
    AiProvider, AnyIntegration, ConnectionInfo, DomainRegistrar, FieldKind, HostingPanel,
    IntegrationDescriptor, Mailer, PaymentGateway, VirtProvider,
};
use hos_core::secret::SecretString;
use hos_db::PgPool;
use hos_db::repo::integrations::{self as repo, IntegrationRow, NewIntegration};
use serde_json::{Map, Value};

use crate::BillingResult;

/// Default cache lifetime of a built integration.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Environment variable enabling the `mock` providers (truthy: `1`, `true`,
/// `yes`, `on`).
pub const ALLOW_MOCK_INTEGRATIONS_ENV: &str = "HOS_ALLOW_MOCK_INTEGRATIONS";

/// Whether `HOS_ALLOW_MOCK_INTEGRATIONS` is set to a truthy value (read once
/// per process).
pub fn mock_integrations_allowed_by_env() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| {
        std::env::var(ALLOW_MOCK_INTEGRATIONS_ENV).is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    })
}

/// Whether `kind` is a simulated provider of a category where it would move
/// money or resources for free (gateway, hypervisor, panel, registrar). The
/// `log` mailer is not a mock in this sense.
pub fn is_mock_kind(category: IntegrationCategory, kind: &str) -> bool {
    let mock = match category {
        IntegrationCategory::Gateway => hos_payments::MOCK,
        IntegrationCategory::Virt => hos_virt::MOCK,
        IntegrationCategory::Panel => hos_panels::MOCK,
        IntegrationCategory::Registrar => hos_domains::mock::KIND,
        IntegrationCategory::Einvoice => hos_einvoice::mock::KIND,
        _ => return false,
    };
    kind.trim().eq_ignore_ascii_case(mock)
}

fn mock_refused(category: IntegrationCategory) -> CoreError {
    CoreError::validation(format!(
        "mock {category} integrations are for demos and tests only; set \
         {ALLOW_MOCK_INTEGRATIONS_ENV}=1 to enable them"
    ))
}

/// Whether `kind` may be saved and used while the registry is in mock-only
/// mode (the public demo, docs/DEMO.md): the `mock` providers and the `log`
/// mailer, nothing that talks to a real provider.
pub fn allowed_in_mock_only(category: IntegrationCategory, kind: &str) -> bool {
    is_mock_kind(category, kind)
        || (category == IntegrationCategory::Mail
            && kind.trim().eq_ignore_ascii_case(hos_mail::kinds::LOG))
}

fn real_provider_refused(category: IntegrationCategory, kind: &str) -> CoreError {
    CoreError::validation(format!(
        "{category} integration `{}` is disabled on this server: the demo uses only the built-in \
         mock providers",
        kind.chars().take(40).collect::<String>()
    ))
}

#[derive(Clone)]
struct Cached {
    integration: AnyIntegration,
    category: IntegrationCategory,
    kind: String,
    name: String,
    loaded: Instant,
}

/// A built integration together with its row metadata.
#[derive(Clone)]
pub struct LoadedIntegration {
    /// Row id.
    pub id: IntegrationId,
    /// Category.
    pub category: IntegrationCategory,
    /// `integrations.kind`.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// The provider object.
    pub integration: AnyIntegration,
}

impl std::fmt::Debug for LoadedIntegration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedIntegration")
            .field("id", &self.id)
            .field("category", &self.category)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .finish()
    }
}

/// Input of [`IntegrationRegistry::save_integration`].
#[derive(Debug, Clone)]
pub struct SaveIntegration {
    /// `None` creates a new integration.
    pub id: Option<IntegrationId>,
    pub category: IntegrationCategory,
    pub kind: String,
    pub name: String,
    /// Plain settings (`FieldSpec` keys).
    pub config: Value,
    /// Secret settings. `None` keeps the stored secrets; in an object, blank
    /// values keep the stored value of that key (admin forms never echo secrets).
    pub secrets: Option<Value>,
    pub is_active: bool,
    pub sort_order: i32,
}

struct Inner {
    pool: PgPool,
    secret_box: SecretBox,
    ttl: Duration,
    cache: Mutex<HashMap<(TenantId, IntegrationId), Cached>>,
    env_ai_key: Option<SecretString>,
    env_ai: Mutex<Option<Arc<dyn AiProvider>>>,
    allow_mock: AtomicBool,
    /// Demo mode: only [`allowed_in_mock_only`] kinds, no platform AI key,
    /// always the `log` mailer.
    mock_only: AtomicBool,
}

/// Cheap-to-clone registry of built integrations.
#[derive(Clone)]
pub struct IntegrationRegistry {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for IntegrationRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntegrationRegistry")
            .field("ttl", &self.inner.ttl)
            .finish_non_exhaustive()
    }
}

fn wrong_category(id: IntegrationId, expected: IntegrationCategory) -> CoreError {
    CoreError::validation(format!("integration {id} is not a {expected} integration"))
}

impl IntegrationRegistry {
    /// A registry with the default TTL; `ANTHROPIC_API_KEY` is read from the
    /// environment as the AI fallback, and mock providers are allowed only
    /// when `HOS_ALLOW_MOCK_INTEGRATIONS` is truthy.
    pub fn new(pool: PgPool, secret_box: SecretBox) -> Self {
        let env_ai_key = std::env::var(hos_core::config::env::ANTHROPIC_API_KEY)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .map(SecretString::new);
        Self {
            inner: Arc::new(Inner {
                pool,
                secret_box,
                ttl: DEFAULT_TTL,
                cache: Mutex::new(HashMap::new()),
                env_ai_key,
                env_ai: Mutex::new(None),
                allow_mock: AtomicBool::new(mock_integrations_allowed_by_env()),
                mock_only: AtomicBool::new(false),
            }),
        }
    }

    /// Same registry with another cache TTL (zero disables caching).
    #[must_use]
    pub fn with_ttl(self, ttl: Duration) -> Self {
        let inner = Arc::try_unwrap(self.inner).unwrap_or_else(|arc| Inner {
            pool: arc.pool.clone(),
            secret_box: arc.secret_box.clone(),
            ttl: arc.ttl,
            cache: Mutex::new(HashMap::new()),
            env_ai_key: arc.env_ai_key.clone(),
            env_ai: Mutex::new(None),
            allow_mock: AtomicBool::new(arc.allow_mock.load(Ordering::Relaxed)),
            mock_only: AtomicBool::new(arc.mock_only.load(Ordering::Relaxed)),
        });
        Self {
            inner: Arc::new(Inner { ttl, ..inner }),
        }
    }

    /// Same registry with mock providers allowed or refused.
    #[must_use]
    pub fn with_mock_integrations(self, allowed: bool) -> Self {
        self.set_mock_integrations_allowed(allowed);
        self
    }

    /// Allows or refuses the `mock` providers (demo seed, tests). Shared by
    /// every clone of this registry.
    pub fn set_mock_integrations_allowed(&self, allowed: bool) {
        self.inner.allow_mock.store(allowed, Ordering::Relaxed);
        if !allowed {
            self.cache()
                .retain(|_, c| !is_mock_kind(c.category, &c.kind));
        }
    }

    /// Whether the `mock` providers may be saved and resolved.
    pub fn mock_integrations_allowed(&self) -> bool {
        self.inner.allow_mock.load(Ordering::Relaxed)
    }

    /// Mock-only mode (the public demo): every real provider is refused on
    /// save and on use, the platform AI key is ignored and the mailer is
    /// always the `log` mailer. Turning it on also allows the mocks. Shared
    /// by every clone of this registry.
    pub fn set_mock_only(&self, on: bool) {
        self.inner.mock_only.store(on, Ordering::Relaxed);
        if on {
            self.inner.allow_mock.store(true, Ordering::Relaxed);
            self.cache()
                .retain(|_, c| allowed_in_mock_only(c.category, &c.kind));
        }
    }

    /// Whether [`set_mock_only`](Self::set_mock_only) is on.
    pub fn mock_only(&self) -> bool {
        self.inner.mock_only.load(Ordering::Relaxed)
    }

    fn check_mock(&self, category: IntegrationCategory, kind: &str) -> BillingResult<()> {
        if self.mock_only() && !allowed_in_mock_only(category, kind) {
            return Err(real_provider_refused(category, kind));
        }
        if is_mock_kind(category, kind) && !self.mock_integrations_allowed() {
            return Err(mock_refused(category));
        }
        Ok(())
    }

    /// The secret box used for `integrations.secrets_enc`.
    pub fn secret_box(&self) -> &SecretBox {
        &self.inner.secret_box
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<(TenantId, IntegrationId), Cached>> {
        self.inner
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Drops a cached integration (call after editing it outside the registry).
    pub fn invalidate(&self, tenant_id: TenantId, id: IntegrationId) {
        self.cache().remove(&(tenant_id, id));
    }

    /// Drops every cached integration of a tenant.
    pub fn invalidate_tenant(&self, tenant_id: TenantId) {
        self.cache().retain(|(t, _), _| *t != tenant_id);
    }

    /// Decrypted secrets of a row (`{}` when none are stored).
    pub fn decrypt_secrets(&self, row: &IntegrationRow) -> BillingResult<Value> {
        match &row.secrets_enc {
            Some(sealed) => Ok(self
                .inner
                .secret_box
                .decrypt_json(sealed, &integration_aad(row.tenant_id, row.id))?),
            None => Ok(Value::Object(Map::new())),
        }
    }

    /// Loads and builds an integration (cached). Inactive integrations, and
    /// mock providers unless allowed, are rejected with `Validation`.
    pub async fn get(
        &self,
        tenant_id: TenantId,
        id: IntegrationId,
    ) -> BillingResult<LoadedIntegration> {
        let cached = self
            .cache()
            .get(&(tenant_id, id))
            .filter(|c| c.loaded.elapsed() < self.inner.ttl)
            .cloned();
        if let Some(c) = cached {
            self.check_mock(c.category, &c.kind)?;
            return Ok(LoadedIntegration {
                id,
                category: c.category,
                kind: c.kind,
                name: c.name,
                integration: c.integration,
            });
        }
        let row = repo::get_integration(&self.inner.pool, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("integration", id))?;
        if !row.is_active {
            return Err(CoreError::validation(format!(
                "integration {} is disabled",
                row.name
            )));
        }
        self.check_mock(row.category, &row.kind)?;
        let secrets = self.decrypt_secrets(&row)?;
        let integration = Self::build(row.category, &row.kind, &row.config, &secrets)?;
        if !self.inner.ttl.is_zero() {
            self.cache().insert(
                (tenant_id, id),
                Cached {
                    integration: integration.clone(),
                    category: row.category,
                    kind: row.kind.clone(),
                    name: row.name.clone(),
                    loaded: Instant::now(),
                },
            );
        }
        Ok(LoadedIntegration {
            id,
            category: row.category,
            kind: row.kind,
            name: row.name,
            integration,
        })
    }

    /// Virtualization provider.
    pub async fn virt(
        &self,
        tenant_id: TenantId,
        id: IntegrationId,
    ) -> BillingResult<Arc<dyn VirtProvider>> {
        match self.get(tenant_id, id).await?.integration {
            AnyIntegration::Virt(v) => Ok(v),
            _ => Err(wrong_category(id, IntegrationCategory::Virt)),
        }
    }

    /// Hosting panel.
    pub async fn panel(
        &self,
        tenant_id: TenantId,
        id: IntegrationId,
    ) -> BillingResult<Arc<dyn HostingPanel>> {
        match self.get(tenant_id, id).await?.integration {
            AnyIntegration::Panel(p) => Ok(p),
            _ => Err(wrong_category(id, IntegrationCategory::Panel)),
        }
    }

    /// Domain registrar.
    pub async fn registrar(
        &self,
        tenant_id: TenantId,
        id: IntegrationId,
    ) -> BillingResult<Arc<dyn DomainRegistrar>> {
        match self.get(tenant_id, id).await?.integration {
            AnyIntegration::Registrar(r) => Ok(r),
            _ => Err(wrong_category(id, IntegrationCategory::Registrar)),
        }
    }

    /// Payment gateway.
    pub async fn gateway(
        &self,
        tenant_id: TenantId,
        id: IntegrationId,
    ) -> BillingResult<Arc<dyn PaymentGateway>> {
        match self.get(tenant_id, id).await?.integration {
            AnyIntegration::Gateway(g) => Ok(g),
            _ => Err(wrong_category(id, IntegrationCategory::Gateway)),
        }
    }

    /// e-invoice provider (`einvoice` category).
    pub async fn einvoice(
        &self,
        tenant_id: TenantId,
        id: IntegrationId,
    ) -> BillingResult<Arc<dyn hos_core::integrations::EInvoiceProvider>> {
        match self.get(tenant_id, id).await?.integration {
            AnyIntegration::Einvoice(e) => Ok(e),
            _ => Err(wrong_category(id, IntegrationCategory::Einvoice)),
        }
    }

    /// The tenant's default (else first active) integration of a category.
    pub async fn default_id(
        &self,
        tenant_id: TenantId,
        category: IntegrationCategory,
    ) -> BillingResult<Option<IntegrationId>> {
        Ok(
            repo::default_integration(&self.inner.pool, tenant_id, category)
                .await?
                .map(|r| r.id),
        )
    }

    /// The tenant's mailer: its default `mail` integration, else a `log`
    /// mailer (never fails — a broken mail integration is logged and replaced
    /// by the log mailer so callers can record the failure on send).
    pub async fn mailer(&self, tenant_id: TenantId) -> Arc<dyn Mailer> {
        let fallback = || -> Arc<dyn Mailer> { Arc::new(hos_mail::LogMailer::new(None)) };
        if self.mock_only() {
            // Demo: messages stay in `email_log` (the demo mailbox).
            return fallback();
        }
        let id = match self.default_id(tenant_id, IntegrationCategory::Mail).await {
            Ok(Some(id)) => id,
            Ok(None) => return fallback(),
            Err(e) => {
                tracing::warn!(%tenant_id, error = %e, "loading mail integration failed");
                return fallback();
            }
        };
        match self.get(tenant_id, id).await {
            Ok(LoadedIntegration {
                integration: AnyIntegration::Mail(m),
                ..
            }) => m,
            Ok(_) => fallback(),
            Err(e) => {
                tracing::warn!(%tenant_id, integration_id = %id, error = %e, "building mailer failed");
                fallback()
            }
        }
    }

    /// The tenant's AI provider (its default `ai` integration), else the
    /// platform's `ANTHROPIC_API_KEY`; `None` when neither exists. Returns the
    /// integration id when it came from the tenant.
    pub async fn ai(
        &self,
        tenant_id: TenantId,
    ) -> BillingResult<Option<(Option<IntegrationId>, Arc<dyn AiProvider>)>> {
        if let Some(id) = self.default_id(tenant_id, IntegrationCategory::Ai).await? {
            return match self.get(tenant_id, id).await?.integration {
                AnyIntegration::Ai(a) => Ok(Some((Some(id), a))),
                _ => Err(wrong_category(id, IntegrationCategory::Ai)),
            };
        }
        let Some(key) = self.inner.env_ai_key.as_ref().filter(|_| !self.mock_only()) else {
            return Ok(None);
        };
        let mut guard = self
            .inner
            .env_ai
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            let provider = hos_seo::build(
                hos_core::integrations::kinds::ANTHROPIC,
                &Value::Object(Map::new()),
                &serde_json::json!({ "api_key": key.expose_str() }),
            )?;
            *guard = Some(provider);
        }
        Ok(guard.clone().map(|p| (None, p)))
    }

    // -----------------------------------------------------------------------
    // descriptors / building
    // -----------------------------------------------------------------------

    /// Every integration kind of every category (admin "add integration" form).
    pub fn descriptors() -> Vec<IntegrationDescriptor> {
        let mut all = Vec::new();
        all.extend(hos_virt::descriptors());
        all.extend(hos_panels::descriptors());
        all.extend(hos_domains::descriptors());
        all.extend(hos_payments::descriptors());
        all.extend(hos_mail::descriptors());
        all.extend(hos_seo::descriptors());
        all.extend(hos_einvoice::descriptors());
        all.extend(oauth_descriptors());
        all
    }

    /// Descriptor of one kind.
    pub fn descriptor(category: IntegrationCategory, kind: &str) -> Option<IntegrationDescriptor> {
        Self::descriptors()
            .into_iter()
            .find(|d| d.category == category && d.kind == kind)
    }

    /// Builds a provider object with the owning crate's factory.
    pub fn build(
        category: IntegrationCategory,
        kind: &str,
        config: &Value,
        secrets: &Value,
    ) -> Result<AnyIntegration, IntegrationError> {
        Ok(match category {
            IntegrationCategory::Virt => {
                AnyIntegration::Virt(hos_virt::build(kind, config, secrets)?)
            }
            IntegrationCategory::Panel => {
                AnyIntegration::Panel(hos_panels::build(kind, config, secrets)?)
            }
            IntegrationCategory::Registrar => {
                AnyIntegration::Registrar(hos_domains::build(kind, config, secrets)?)
            }
            IntegrationCategory::Gateway => {
                AnyIntegration::Gateway(hos_payments::build(kind, config, secrets)?)
            }
            IntegrationCategory::Mail => {
                AnyIntegration::Mail(hos_mail::build(kind, config, secrets)?)
            }
            IntegrationCategory::Ai => AnyIntegration::Ai(hos_seo::build(kind, config, secrets)?),
            IntegrationCategory::Einvoice => {
                AnyIntegration::Einvoice(hos_einvoice::build(kind, config, secrets)?)
            }
            IntegrationCategory::Oauth => {
                return Err(IntegrationError::Config(
                    "oauth integrations are handled by hos-auth".into(),
                ));
            }
        })
    }

    /// Tests settings that are not saved yet (admin form "Test connection").
    pub async fn test_settings(
        category: IntegrationCategory,
        kind: &str,
        config: &Value,
        secrets: &Value,
    ) -> BillingResult<ConnectionInfo> {
        let built = Self::build(category, kind, config, secrets)?;
        Ok(built.test_connection().await?)
    }

    /// Tests a saved integration and records the result on the row. Only the
    /// classified summary is stored ([`IntegrationError::summary`],
    /// [`CoreError::integration_summary`], [`ConnectionInfo::summary`]) —
    /// never a provider's raw error text.
    pub async fn test_connection(
        &self,
        tenant_id: TenantId,
        id: IntegrationId,
    ) -> BillingResult<Result<ConnectionInfo, IntegrationError>> {
        self.invalidate(tenant_id, id);
        let loaded = match self.get(tenant_id, id).await {
            Ok(l) => l,
            // No row to record the result on.
            Err(e @ CoreError::NotFound { .. }) => return Err(e),
            Err(e) => {
                if !e.is_user_facing() && !matches!(e, CoreError::Integration(_)) {
                    tracing::warn!(%tenant_id, integration_id = %id, error = %e,
                        "integration connection test failed");
                }
                repo::record_test_result(
                    &self.inner.pool,
                    tenant_id,
                    id,
                    false,
                    Some(&e.integration_summary()),
                )
                .await?;
                return match e {
                    CoreError::Integration(e) => Ok(Err(e)),
                    other => Err(other),
                };
            }
        };
        let result = loaded.integration.test_connection().await;
        let message = match &result {
            Ok(info) => info.summary(),
            Err(e) => e.summary(),
        };
        repo::record_test_result(
            &self.inner.pool,
            tenant_id,
            id,
            result.is_ok(),
            (!message.is_empty()).then_some(message.as_str()),
        )
        .await?;
        Ok(result)
    }

    /// Creates or updates an integration: validates against the descriptor,
    /// encrypts the secrets (merging blank fields with the stored ones) and
    /// invalidates the cache. Mock providers are refused unless allowed.
    pub async fn save_integration(
        &self,
        tenant_id: TenantId,
        input: &SaveIntegration,
    ) -> BillingResult<IntegrationRow> {
        self.check_mock(input.category, &input.kind)?;
        let descriptor = Self::descriptor(input.category, &input.kind).ok_or_else(|| {
            CoreError::validation(format!(
                "unknown {} integration kind {:?}",
                input.category, input.kind
            ))
        })?;
        if input.name.trim().is_empty() {
            return Err(CoreError::validation("integration name is required"));
        }
        let config = match &input.config {
            Value::Null => Value::Object(Map::new()),
            Value::Object(_) => input.config.clone(),
            _ => return Err(CoreError::validation("config must be a JSON object")),
        };
        let existing = match input.id {
            Some(id) => Some(
                repo::get_integration(&self.inner.pool, tenant_id, id)
                    .await?
                    .ok_or_else(|| CoreError::not_found("integration", id))?,
            ),
            None => None,
        };
        if let Some(row) = &existing
            && (row.category != input.category || row.kind != input.kind)
        {
            return Err(CoreError::validation(
                "the category and kind of an integration cannot be changed",
            ));
        }
        // Stored secrets follow the endpoint they were entered for: when an
        // update points the integration elsewhere (another API host), they
        // must be entered again, or a changed URL would send them there.
        let stored = match &existing {
            Some(row) if !endpoint_changed(&descriptor, &row.config, &config) => {
                self.decrypt_secrets(row)?
            }
            _ => Value::Object(Map::new()),
        };
        let secrets = merge_secrets(&stored, input.secrets.as_ref())?;
        descriptor.validate(&config, &secrets)?;

        let id = input.id.unwrap_or_else(IntegrationId::new);
        let sealed = if secrets.as_object().is_some_and(|m| !m.is_empty()) {
            Some(
                self.inner
                    .secret_box
                    .encrypt_json(&secrets, &integration_aad(tenant_id, id))?,
            )
        } else {
            None
        };
        let row = match existing {
            None => {
                repo::create_integration(
                    &self.inner.pool,
                    tenant_id,
                    &NewIntegration {
                        id,
                        category: input.category,
                        kind: input.kind.clone(),
                        name: input.name.trim().to_owned(),
                        config,
                        secrets_enc: sealed,
                        is_active: input.is_active,
                        sort_order: input.sort_order,
                    },
                )
                .await?
            }
            Some(_) => {
                let mut tx = self.inner.pool.begin().await?;
                repo::update_integration(
                    &mut *tx,
                    tenant_id,
                    id,
                    input.name.trim(),
                    &config,
                    input.is_active,
                )
                .await?;
                repo::set_integration_secrets(&mut *tx, tenant_id, id, sealed.as_deref()).await?;
                let row = repo::get_integration(&mut *tx, tenant_id, id)
                    .await?
                    .ok_or_else(|| CoreError::not_found("integration", id))?;
                tx.commit().await?;
                row
            }
        };
        self.invalidate(tenant_id, id);
        Ok(row)
    }
}

/// Whether `new` points an integration at another endpoint than `old`: a URL
/// field (other than the client-facing `login_url`) with another origin, or
/// another `host`.
fn endpoint_changed(descriptor: &IntegrationDescriptor, old: &Value, new: &Value) -> bool {
    let origin = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| match url::Url::parse(s) {
                Ok(u) => u.origin().ascii_serialization(),
                Err(_) => s.trim_end_matches('.').to_ascii_lowercase(),
            })
    };
    descriptor
        .config_fields
        .iter()
        .filter(|f| (f.kind == FieldKind::Url && f.key != "login_url") || f.key == "host")
        .any(|f| origin(old, f.key) != origin(new, f.key))
}

/// Stored secrets overlaid with the submitted ones; blank submitted values
/// keep the stored value.
fn merge_secrets(stored: &Value, submitted: Option<&Value>) -> BillingResult<Value> {
    let mut out = stored.as_object().cloned().unwrap_or_default();
    match submitted {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (k, v) in map {
                let blank = match v {
                    Value::Null => true,
                    Value::String(s) => s.trim().is_empty(),
                    _ => false,
                };
                if !blank {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        Some(_) => return Err(CoreError::validation("secrets must be a JSON object")),
    }
    Ok(Value::Object(out))
}

/// Tenant OAuth apps for "Sign in with Google / GitHub" (read by the web
/// layer's `/auth/oauth`; there is no provider object to build). Without a
/// descriptor `save_integration` rejected them, so they could not be stored.
fn oauth_descriptors() -> Vec<IntegrationDescriptor> {
    use hos_core::integrations::FieldSpec;
    let app = |kind: &'static str, name: &'static str, docs: &'static str| {
        IntegrationDescriptor::new(kind, name, IntegrationCategory::Oauth)
            .description("OAuth 2.0 client for customer sign-in.")
            .docs_url(docs)
            .config_field(FieldSpec::text("client_id", "Client ID").required())
            .secret_field(FieldSpec::password("client_secret", "Client secret").required())
    };
    vec![
        app(
            "google",
            "Google",
            "https://developers.google.com/identity/protocols/oauth2/web-server",
        ),
        app(
            "github",
            "GitHub",
            "https://docs.github.com/apps/oauth-apps/building-oauth-apps",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptors_cover_every_category() {
        let d = IntegrationRegistry::descriptors();
        for cat in [
            IntegrationCategory::Virt,
            IntegrationCategory::Panel,
            IntegrationCategory::Registrar,
            IntegrationCategory::Gateway,
            IntegrationCategory::Mail,
            IntegrationCategory::Ai,
        ] {
            assert!(d.iter().any(|x| x.category == cat), "{cat}");
        }
        assert!(IntegrationRegistry::descriptor(IntegrationCategory::Gateway, "mock").is_some());
        assert!(IntegrationRegistry::descriptor(IntegrationCategory::Gateway, "nope").is_none());
    }

    #[test]
    fn builds_mocks() {
        let p =
            IntegrationRegistry::build(IntegrationCategory::Panel, "mock", &json!({}), &json!({}))
                .unwrap();
        assert_eq!(p.kind(), "mock");
        let g = IntegrationRegistry::build(
            IntegrationCategory::Gateway,
            "mock",
            &json!({}),
            &json!({"webhook_secret": "s"}),
        )
        .unwrap();
        assert_eq!(g.category(), IntegrationCategory::Gateway);
        assert!(
            IntegrationRegistry::build(
                IntegrationCategory::Oauth,
                "google",
                &json!({}),
                &json!({})
            )
            .is_err()
        );
    }

    #[test]
    fn mock_kinds() {
        for cat in [
            IntegrationCategory::Gateway,
            IntegrationCategory::Virt,
            IntegrationCategory::Panel,
            IntegrationCategory::Registrar,
        ] {
            assert!(is_mock_kind(cat, "mock"), "{cat}");
            assert!(is_mock_kind(cat, " Mock "), "{cat}");
        }
        assert!(!is_mock_kind(IntegrationCategory::Gateway, "stripe"));
        assert!(!is_mock_kind(IntegrationCategory::Mail, "log"));
        assert!(!is_mock_kind(IntegrationCategory::Mail, "mock"));
    }

    /// Demo mode: only the mocks and the log mailer; every real provider of
    /// every category (including AI and OAuth) is refused.
    #[test]
    fn mock_only_allows_mocks_and_the_log_mailer() {
        use IntegrationCategory as C;
        for cat in [C::Gateway, C::Virt, C::Panel, C::Registrar, C::Einvoice] {
            assert!(allowed_in_mock_only(cat, "mock"), "{cat}");
        }
        assert!(allowed_in_mock_only(C::Mail, "log"));
        for (cat, kind) in [
            (C::Gateway, "stripe"),
            (C::Gateway, "iyzico"),
            (C::Virt, "proxmox"),
            (C::Panel, "cpanel"),
            (C::Registrar, "namecheap"),
            (C::Einvoice, "nilvera"),
            (C::Mail, "smtp"),
            (C::Mail, "mailgun"),
            (C::Ai, "anthropic"),
            (C::Oauth, "google"),
        ] {
            assert!(!allowed_in_mock_only(cat, kind), "{cat} {kind}");
        }
    }

    #[test]
    fn secrets_merge_keeps_blank_fields() {
        let stored = json!({"api_key": "old", "other": "x"});
        let merged = merge_secrets(&stored, Some(&json!({"api_key": "", "new": "n"}))).unwrap();
        assert_eq!(merged, json!({"api_key": "old", "other": "x", "new": "n"}));
        let replaced = merge_secrets(&stored, Some(&json!({"api_key": "fresh"}))).unwrap();
        assert_eq!(replaced["api_key"], "fresh");
        assert_eq!(merge_secrets(&stored, None).unwrap(), stored);
        assert!(merge_secrets(&stored, Some(&json!([1]))).is_err());
    }
}
