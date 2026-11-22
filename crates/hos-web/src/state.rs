//! Shared application state.
//!
//! [`AppState`] is cheap to clone (one `Arc`) and derefs to
//! [`AppStateInner`], so handlers write `state.pool`, `state.i18n`, …

use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use hos_auth::csrf::CsrfProtector;
use hos_auth::ratelimit::{
    LockoutPolicy, LockoutTracker, RateDecision, RateLimit, RateLimiter, presets,
};
use hos_auth::session::SessionConfig;
use hos_core::config::AppConfig;
use hos_core::crypto::{SecretBox, hmac_sha256};
use hos_core::integrations::mailer::Mailer;
use hos_db::{DbConfig, PgPool};
use hos_domains::RdapClient;

use crate::assets::Assets;
use crate::config::WebConfig;
use crate::i18n::I18n;
use crate::tenant::TenantCache;
use crate::view::StoreChromeCache;

/// Shared state handed to every handler (`State<AppState>`).
#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);

impl Deref for AppState {
    type Target = AppStateInner;

    fn deref(&self) -> &AppStateInner {
        &self.0
    }
}

/// Per-action rate limiters (per client IP unless noted).
#[derive(Debug)]
pub struct Limits {
    /// `POST /auth/login`, `/platform/login`.
    pub login: RateLimiter,
    /// `POST /auth/register`.
    pub register: RateLimiter,
    /// Forgot / reset password.
    pub password_reset: RateLimiter,
    /// 2FA code entry.
    pub two_factor: RateLimiter,
    /// Verification e-mail re-send.
    pub email_verification: RateLimiter,
    /// OAuth start / callback.
    pub oauth: RateLimiter,
    /// Storefront contact form (guest tickets).
    pub contact: RateLimiter,
    /// Domain availability checks.
    pub domain_check: RateLimiter,
    /// RDAP / whois lookups.
    pub whois: RateLimiter,
    /// "Was this helpful?" votes.
    pub vote: RateLimiter,
    /// REST API (per key / session).
    pub api: RateLimiter,
    /// Verified gateway / registrar webhooks (per integration). Taken only
    /// after the signature check, so forged calls cannot use it up.
    pub webhook: RateLimiter,
    /// Webhook and inbound-mail requests that failed authentication (per
    /// client IP); an IP over it is answered `429` before any work.
    pub webhook_failed: RateLimiter,
    /// Gateway webhooks whose verification calls the provider (PayPal's
    /// verification API; per integration), taken before that call.
    pub webhook_remote: RateLimiter,
    /// Self-service tenant sign-up (`/platform/signup`, per IP).
    pub signup: RateLimiter,
    /// Secret-link pages (`/quote/{id}?token=`, per IP).
    pub public_token: RateLimiter,
    /// Referral link visits (`/r/{code}`, per IP).
    pub referral: RateLimiter,
    /// Authenticated inbound e-mail webhook calls (per tenant).
    pub inbound_mail: RateLimiter,
    /// Public file downloads (`/downloads/{id}/file`, per IP).
    pub downloads: RateLimiter,
    /// VM actions from the client area / API (power, ISO, rescue, rDNS,
    /// firewall, snapshots; per user + service).
    pub vm_action: RateLimiter,
    /// VM root password resets (per service; hos-billing also enforces a
    /// per-service limit in the database).
    pub vm_password: RateLimiter,
    /// Console session tokens (per user).
    pub vm_console: RateLimiter,
    /// Generated share images (`/og/{key}.png`, per IP).
    pub og: RateLimiter,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            login: RateLimiter::new(presets::LOGIN),
            register: RateLimiter::new(presets::REGISTER),
            password_reset: RateLimiter::new(presets::PASSWORD_RESET),
            two_factor: RateLimiter::new(presets::TWO_FACTOR),
            email_verification: RateLimiter::new(presets::EMAIL_VERIFICATION),
            oauth: RateLimiter::new(presets::OAUTH),
            contact: RateLimiter::new(RateLimit::new(3, Duration::from_secs(900))),
            domain_check: RateLimiter::new(RateLimit::new(150, Duration::from_secs(300))),
            whois: RateLimiter::new(RateLimit::new(20, Duration::from_secs(300))),
            vote: RateLimiter::new(RateLimit::new(10, Duration::from_secs(600))),
            api: RateLimiter::new(presets::API),
            webhook: RateLimiter::new(RateLimit::new(300, Duration::from_secs(60))),
            webhook_failed: RateLimiter::new(RateLimit::new(20, Duration::from_secs(300))),
            webhook_remote: RateLimiter::new(RateLimit::new(60, Duration::from_secs(60))),
            signup: RateLimiter::new(RateLimit::new(5, Duration::from_secs(3600))),
            public_token: RateLimiter::new(RateLimit::new(60, Duration::from_secs(600))),
            referral: RateLimiter::new(RateLimit::new(30, Duration::from_secs(600))),
            inbound_mail: RateLimiter::new(RateLimit::new(120, Duration::from_secs(60))),
            downloads: RateLimiter::new(RateLimit::new(30, Duration::from_secs(600))),
            vm_action: RateLimiter::new(RateLimit::new(30, Duration::from_secs(300))),
            vm_password: RateLimiter::new(RateLimit::new(3, Duration::from_secs(3600))),
            vm_console: RateLimiter::new(RateLimit::new(12, Duration::from_secs(300))),
            og: RateLimiter::new(RateLimit::new(60, Duration::from_secs(60))),
        }
    }
}

/// `Err(429)` (with `Retry-After`) when `limiter` rejects `key`.
///
/// ```ignore
/// check_rate(&state.limits.contact, &ip.key("contact"))?;
/// ```
pub fn check_rate(limiter: &RateLimiter, key: &str) -> crate::AppResult<()> {
    match limiter.check(key) {
        RateDecision::Allowed { .. } => Ok(()),
        RateDecision::Limited { retry_after } => Err(crate::AppError::RateLimited {
            retry_after: Some(retry_after),
        }),
    }
}

/// The state behind [`AppState`].
pub struct AppStateInner {
    /// Pooled database connection.
    pub pool: PgPool,
    /// Process configuration (`.env`).
    pub config: AppConfig,
    /// Web settings (static dir, debug, proxies …).
    pub web: WebConfig,
    /// AES-GCM box keyed with `HOS_SECRET_KEY` (integration secrets, TOTP, OAuth cookie).
    pub secret_box: SecretBox,
    /// Billing / automation engine (pricing, checkout, payments, provisioning,
    /// lifecycle, reports) and its integration registry (`billing.registry()`).
    pub billing: hos_billing::Billing,
    /// CSRF token issuer / verifier.
    pub csrf: CsrfProtector,
    /// Session lifetimes and cookie flags.
    pub sessions: SessionConfig,
    /// Rate limiters.
    pub limits: Limits,
    /// Per-email login lockout (in memory; DB counters are kept too).
    pub lockout: LockoutTracker,
    /// Translations.
    pub i18n: I18n,
    /// Static asset manifest (cache-busting URLs).
    pub assets: Assets,
    /// `Host` → tenant cache.
    pub tenants: TenantCache,
    /// Storefront navigation cache.
    pub store_chrome: StoreChromeCache,
    /// RDAP client (whois page, availability fallback).
    pub rdap: RdapClient,
    /// Outgoing mail transport (development: [`hos_mail::LogMailer`]).
    pub mailer: Arc<dyn Mailer>,
    /// HMAC key for signed cookies (flash, locale …), derived from `HOS_SECRET_KEY`.
    pub cookie_key: [u8; 32],
    /// Process start (for `/healthz`).
    pub started_at: Instant,
    /// Self-update runtime (docs/UPDATES.md): build info, install type,
    /// release source, maintenance-mode cache.
    pub updates: crate::routes::platform::updates::UpdateRuntime,
    /// Public demo runtime (reset gate, limits; idle unless `web.demo`).
    pub demo: crate::demo::DemoRuntime,
    /// Whether the database is behind this binary (pages wait with `503`).
    /// Checked by `hostingos serve` at startup and by `/readyz`.
    pub schema: crate::schema::SchemaGate,
}

impl AppState {
    /// Builds the state from an already loaded configuration and pool.
    pub fn new(pool: PgPool, config: AppConfig, web: WebConfig) -> anyhow::Result<Self> {
        let master = *config.secret_key.expose();
        let mut sessions = SessionConfig::for_base_url(&config.base_url);
        sessions.secure = web.secure;
        let i18n = I18n::load(&web.locales_dir)
            .with_context(|| format!("loading locales from {}", web.locales_dir.display()))?;
        let assets = Assets::scan(&web.static_dir, web.debug);
        let rdap = RdapClient::new().map_err(|e| anyhow::anyhow!("rdap client: {e}"))?;
        let mut billing_config = hos_billing::BillingConfig::new(config.base_url.as_str());
        billing_config.allow_mock_integrations = web.mock_integrations.allowed();
        let billing = hos_billing::Billing::new(pool.clone(), config.secret_box(), billing_config);
        // `Billing::new` only ever turns mocks on; `web` decides both ways.
        billing
            .registry()
            .set_mock_integrations_allowed(web.mock_integrations.allowed());
        let updates = crate::routes::platform::updates::UpdateRuntime::from_env();
        if web.demo.is_some() {
            // Demo (docs/DEMO.md): mocks only, no platform AI key, mail stays
            // in `email_log`; no update checks.
            billing.registry().set_mock_only(true);
            updates.disable_source();
        }
        Ok(Self(Arc::new(AppStateInner {
            pool,
            billing,
            secret_box: config.secret_box(),
            csrf: CsrfProtector::new(&master),
            sessions,
            limits: Limits::default(),
            lockout: LockoutTracker::new(LockoutPolicy::default()),
            i18n,
            assets,
            tenants: TenantCache::new(Duration::from_secs(30)),
            store_chrome: StoreChromeCache::new(Duration::from_secs(60)),
            rdap,
            mailer: Arc::new(hos_mail::LogMailer::default()),
            // Pre-rename "hf" label, kept on purpose: KDF label of the cookie key.
            cookie_key: hmac_sha256(&master, b"hf-web key derivation: cookies v1"),
            started_at: Instant::now(),
            updates,
            demo: crate::demo::DemoRuntime::default(),
            schema: crate::schema::SchemaGate::default(),
            config,
            web,
        })))
    }

    /// Loads `.env`, the configuration and connects the pool. Fixes the
    /// process-wide outbound policy (`HOS_OUTBOUND_POLICY`) first: an invalid
    /// value is an error, so the process does not start. In single-company
    /// mode (`HOS_MODE`) the database must hold exactly one company
    /// ([`crate::mode::resolve_company`]).
    pub async fn from_env() -> anyhow::Result<Self> {
        Self::from_env_mode(None).await
    }

    /// [`from_env`](Self::from_env) with `HOS_MODE` replaced by `mode`
    /// (`hostingos setup` builds its state before the company exists).
    pub async fn from_env_mode(mode: Option<crate::config::AppMode>) -> anyhow::Result<Self> {
        crate::config::load_dotenv();
        let config = AppConfig::from_env().context("loading configuration")?;
        let policy = hos_core::net::OutboundPolicy::from_env()
            .map_err(|e| anyhow::anyhow!("{e} (unset means `private`)"))
            .context("invalid outbound policy")?;
        if let Err(active) = hos_core::net::OutboundPolicy::set_global(policy)
            && active != policy
        {
            anyhow::bail!(
                "outbound policy {active} was fixed before startup; HOS_OUTBOUND_POLICY asks for {policy}"
            );
        }
        let mut web = WebConfig::from_env(&config);
        anyhow::ensure!(
            web.trusted_proxies.invalid().is_empty(),
            "{} holds entries that are not an IP address or CIDR range: {:?} (example: \
             `10.0.1.0/24,127.0.0.1`)",
            crate::config::TrustedProxies::ENV,
            web.trusted_proxies.invalid()
        );
        web.mode = match mode {
            Some(m) => m,
            None => crate::config::AppMode::from_env().map_err(anyhow::Error::msg)?,
        };
        anyhow::ensure!(
            web.demo.is_none() || web.single(),
            "{} needs single-company mode: unset {} (or set it to `single`)",
            crate::demo::MODE_ENV,
            crate::config::AppMode::ENV
        );
        let pool = hos_db::connect(
            &DbConfig::from_app_config(&config)
                .with_max_connections(20)
                .with_application_name("hostingos-web"),
        )
        .await
        .context("connecting to the database")?;
        crate::mode::resolve_company(&pool, &mut web).await?;
        let state = Self::new(pool, config, web)?;
        if state.web.demo.is_some() {
            // Process-wide and one-way: nothing this process does may reach
            // another host (integrations, webhooks, RDAP, OAuth, IndexNow,
            // update checks, AI). Switched on once the state is built: the
            // RDAP client validates its (public) bootstrap URL when it is
            // created and is refused when it connects. Tests build demo
            // states with `new` instead.
            hos_core::net::go_offline();
            // Page bodies are visitor-written: no search-engine credit for
            // their links.
            crate::markdown::treat_links_as_untrusted();
        }
        Ok(state)
    }

    /// The configured public base URL (`HOS_BASE_URL`).
    pub fn base_url(&self) -> &url::Url {
        &self.config.base_url
    }
}
