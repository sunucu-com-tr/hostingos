//! Platform console (`/platform/*`) — the SaaS operator's area. Tenantless:
//! works on any host, uses its own session cookie (`hos_psession`) and the
//! `platform_admins` table.
//!
//! | Module | Pages |
//! |---|---|
//! | [`login`] | sign in (+ TOTP challenge), sign out |
//! | [`dashboard`] | tenant counts, platform MRR, per-tenant volume, queue health, sign-ups |
//! | [`tenants`] | list / wizard / detail (status, plan + limits, usage, domains, features, notes), impersonation, deletion |
//! | [`plans`] | `platform_plans` CRUD, `tenant_subscriptions` overview |
//! | [`signup`] | self-service tenant sign-up (public) |
//! | [`system`] | settings, operators, own 2FA, jobs + scheduled tasks, global audit log, version / health |
//! | [`domains`] | custom-domain DNS verification, `/internal/tls-allow` |
//! | [`limits`] | [`limits::check_plan_limit`] for the tenant areas |
//! | [`provision`] | tenant provisioning shared with the CLI |
//! | [`handoff`] | `/platform/handoff` — lands an impersonation on the tenant host |
//! | [`updates`] | self-update: version, releases, install / deploy, history, settings; maintenance mode |
//!
//! `login`, `logout`, `signup` and `handoff` are public; everything else sits
//! behind [`guards::require_platform_admin`] and takes [`PlatformAdmin`].
//! Every state change is written to `audit_log` (`actor_kind = platform_admin`).

pub mod dashboard;
pub mod domains;
pub mod handoff;
pub mod limits;
pub mod login;
pub mod menu;
pub mod plans;
pub mod provision;
pub mod signup;
pub mod system;
pub mod tenants;
pub mod updates;

use axum::Router;
use axum::middleware::from_fn;
use axum::routing::{get, post};
use hos_core::ids::TenantId;
use hos_db::enums::ActorKind;
use hos_db::repo::{audit, saas};
use serde_json::{Value, json};

use crate::error::AppResult;
use crate::extract::{ClientIp, PlatformAdmin, UserAgent};
use crate::routes::guards;
use crate::state::AppState;

pub use provision::tenant_origin;

/// Routes of the platform console.
pub fn router(_state: &AppState) -> Router<AppState> {
    let protected = Router::new()
        .route("/platform", get(dashboard::dashboard))
        .route(
            "/platform/tenants",
            get(tenants::list).post(tenants::create),
        )
        .route("/platform/tenants/new", get(tenants::new_form))
        .route("/platform/tenants/{id}", get(tenants::detail))
        .route("/platform/tenants/{id}/status", post(tenants::set_status))
        .route("/platform/tenants/{id}/notes", post(tenants::save_notes))
        .route(
            "/platform/tenants/{id}/features",
            post(tenants::save_features),
        )
        .route("/platform/tenants/{id}/limits", post(tenants::save_limits))
        .route("/platform/tenants/{id}/domains", post(tenants::add_domain))
        .route(
            "/platform/tenants/{id}/domains/{domain_id}/verify",
            post(tenants::verify_domain),
        )
        .route(
            "/platform/tenants/{id}/domains/{domain_id}/delete",
            post(tenants::delete_domain),
        )
        .route(
            "/platform/tenants/{id}/subscription",
            post(tenants::subscription_action),
        )
        .route(
            "/platform/tenants/{id}/impersonate",
            post(tenants::impersonate),
        )
        .route("/platform/tenants/{id}/delete", post(tenants::delete))
        .route("/platform/plans", get(plans::list).post(plans::create))
        .route("/platform/plans/new", get(plans::new_form))
        .route(
            "/platform/plans/{id}",
            get(plans::edit_form).post(plans::update),
        )
        .route("/platform/plans/{id}/delete", post(plans::delete))
        .route("/platform/subscriptions", get(plans::subscriptions))
        .route(
            "/platform/settings",
            get(system::settings).post(system::save_settings),
        )
        .route(
            "/platform/admins",
            get(system::admins).post(system::add_admin),
        )
        .route("/platform/admins/{id}/delete", post(system::delete_admin))
        .route("/platform/account", get(system::account))
        .route("/platform/account/2fa", post(system::confirm_two_factor))
        .route(
            "/platform/account/2fa/disable",
            post(system::disable_two_factor),
        )
        .route("/platform/jobs", get(system::jobs))
        .route("/platform/jobs/{id}/retry", post(system::retry_job))
        .route("/platform/jobs/{id}/cancel", post(system::cancel_job))
        .route("/platform/audit", get(system::audit_log))
        .route("/platform/system", get(system::health))
        .route("/platform/updates", get(updates::page))
        .route("/platform/updates/check", post(updates::check))
        .route("/platform/updates/settings", post(updates::save_settings))
        .route("/platform/updates/apply", post(updates::apply))
        .route("/platform/updates/deploy", post(updates::deploy))
        .route(
            "/platform/updates/maintenance/off",
            post(updates::maintenance_off),
        )
        .route("/platform/updates/runs/{id}", get(updates::run_page))
        .route(
            "/platform/updates/runs/{id}/status",
            get(updates::run_status),
        )
        .route(
            "/platform/updates/notifications/{id}/dismiss",
            post(updates::dismiss),
        )
        .route_layer(from_fn(guards::require_platform_admin));
    Router::new()
        .route("/platform/login", get(login::form).post(login::submit))
        .route(
            "/platform/login/2fa",
            get(login::challenge_form).post(login::challenge_submit),
        )
        .route("/platform/logout", post(login::logout))
        .route("/platform/signup", get(signup::form).post(signup::submit))
        .route("/platform/signup/slug", get(signup::check_slug))
        .route("/platform/signup/verify", get(signup::verify))
        .route("/platform/signup/sent", get(signup::sent))
        .route("/platform/handoff", get(handoff::handoff))
        .merge(protected)
}

/// Tenant-less endpoints outside `/platform` (merged in `routes::app`).
pub fn public_router(_state: &AppState) -> Router<AppState> {
    Router::new().route("/internal/tls-allow", get(domains::tls_allow))
}

// ---------------------------------------------------------------------------
// platform settings
// ---------------------------------------------------------------------------

/// Whether `HOS_OUTBOUND_POLICY` rules out self-service sign-up. Under
/// `private` every tenant's integrations may connect to hosts on the
/// operator's private network (a self-hosted company's own Proxmox, panels,
/// SMTP); strangers who sign up must not get that, so sign-up needs
/// `public` (docs/DEPLOYMENT.md, "Outbound connections"). Debug mode
/// (development) only warns.
pub fn signup_blocked_by_policy(policy: hos_core::net::OutboundPolicy, debug: bool) -> bool {
    policy == hos_core::net::OutboundPolicy::Private && !debug
}

/// Why a server with sign-up enabled and the `private` policy does not start.
pub const SIGNUP_POLICY_ERROR: &str = "self-service tenant sign-up is enabled (platform setting \
     signup.enabled), but HOS_OUTBOUND_POLICY=private lets every tenant's integrations connect \
     to hosts on this server's private network. Set HOS_OUTBOUND_POLICY=public (a multi-tenant \
     SaaS), or turn sign-up off with `hostingos disable-signup` (docs/DEPLOYMENT.md, \
     \"Outbound connections\")";

/// Startup check of [`signup_blocked_by_policy`] against the stored setting:
/// an error (the server does not start) outside debug mode, a warning in it.
pub async fn check_signup_policy(state: &AppState) -> anyhow::Result<()> {
    // Single-company mode has no public company sign-up.
    if state.web.single() {
        return Ok(());
    }
    let policy = hos_core::net::OutboundPolicy::global();
    if policy != hos_core::net::OutboundPolicy::Private {
        return Ok(());
    }
    let settings = PlatformSettings::load(state)
        .await
        .map_err(|e| anyhow::anyhow!("reading the platform settings: {e}"))?;
    if !settings.signup_enabled {
        return Ok(());
    }
    if signup_blocked_by_policy(policy, state.web.debug) {
        anyhow::bail!(SIGNUP_POLICY_ERROR);
    }
    tracing::warn!("DEBUG MODE: {SIGNUP_POLICY_ERROR}; sign-up stays open in debug mode only");
    Ok(())
}

/// Operator settings (`platform_settings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformSettings {
    /// `signup.enabled` — self-service sign-up open (default off).
    pub signup_enabled: bool,
    /// `signup.trial_days` — trial of new tenants (default 14).
    pub trial_days: i64,
    /// `signup.default_plan` — plan code of new tenants (default `starter`).
    pub default_plan: Option<String>,
}

impl Default for PlatformSettings {
    fn default() -> Self {
        Self {
            signup_enabled: false,
            trial_days: 14,
            default_plan: Some("starter".to_owned()),
        }
    }
}

impl PlatformSettings {
    /// Setting keys.
    pub const SIGNUP_ENABLED: &'static str = "signup.enabled";
    /// Setting keys.
    pub const TRIAL_DAYS: &'static str = "signup.trial_days";
    /// Setting keys.
    pub const DEFAULT_PLAN: &'static str = "signup.default_plan";

    /// Whether visitors can sign up now: enabled and allowed by the
    /// outbound policy ([`signup_blocked_by_policy`]).
    pub fn signup_open(&self, state: &AppState) -> bool {
        self.signup_enabled
            && !signup_blocked_by_policy(hos_core::net::OutboundPolicy::global(), state.web.debug)
    }

    /// Loads the settings (defaults for missing keys).
    pub async fn load(state: &AppState) -> AppResult<Self> {
        let all = saas::platform_settings(&state.pool).await?;
        let d = Self::default();
        Ok(Self {
            signup_enabled: all
                .get(Self::SIGNUP_ENABLED)
                .and_then(Value::as_bool)
                .unwrap_or(d.signup_enabled),
            trial_days: all
                .get(Self::TRIAL_DAYS)
                .and_then(Value::as_i64)
                .unwrap_or(d.trial_days)
                .clamp(0, 365),
            default_plan: match all.get(Self::DEFAULT_PLAN) {
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(_) => None,
                None => d.default_plan,
            },
        })
    }

    /// Stores every setting.
    pub async fn save(&self, state: &AppState) -> AppResult<()> {
        saas::set_platform_setting(
            &state.pool,
            Self::SIGNUP_ENABLED,
            &json!(self.signup_enabled),
        )
        .await?;
        saas::set_platform_setting(&state.pool, Self::TRIAL_DAYS, &json!(self.trial_days)).await?;
        saas::set_platform_setting(
            &state.pool,
            Self::DEFAULT_PLAN,
            &json!(self.default_plan.clone().unwrap_or_default()),
        )
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// audit
// ---------------------------------------------------------------------------

/// Request metadata for audit entries.
#[derive(Debug, Clone)]
pub struct AuditCtx {
    /// Client IP.
    pub ip: ClientIp,
    /// User agent.
    pub ua: UserAgent,
}

/// Writes a platform audit entry (`actor_kind = platform_admin`). Failures
/// are logged, never surfaced.
pub async fn audit_platform(
    state: &AppState,
    admin: &PlatformAdmin,
    req: &AuditCtx,
    tenant_id: Option<TenantId>,
    action: &str,
    entity: Option<(&str, uuid::Uuid)>,
    changes: Value,
) {
    // Single-company mode: the owner acted through /admin/server; the entry
    // lands in the company's audit log under the owner's name, as
    // `server.…` instead of `platform.…`.
    let (tenant_id, changes, actor_kind, actor_id, action) = match &admin.staff {
        Some(user) => {
            let mut changes = changes;
            let by = serde_json::json!({"user_id": user.id, "email": user.email});
            match changes.as_object_mut() {
                Some(map) => {
                    map.insert("by".into(), by);
                }
                None => changes = serde_json::json!({"by": by, "value": changes}),
            }
            let action = action
                .strip_prefix("platform.")
                .map_or_else(|| action.to_owned(), |rest| format!("server.{rest}"));
            (
                tenant_id.or(Some(user.tenant_id)),
                changes,
                ActorKind::User,
                uuid::Uuid::from(user.id),
                action,
            )
        }
        None => (
            tenant_id,
            changes,
            ActorKind::PlatformAdmin,
            admin.admin.id.into(),
            action.to_owned(),
        ),
    };
    let action = action.as_str();
    let mut entry = audit::AuditEntry::new(tenant_id, actor_kind, Some(actor_id), action)
        .request(Some(req.ip.0), req.ua.0.as_deref())
        .changes(changes);
    if let Some((kind, id)) = entity {
        entry = entry.entity(kind, id);
    }
    if let Err(e) = audit::write(&state.pool, &entry).await {
        tracing::error!(error = %e, action, "writing platform audit entry failed");
    }
}

impl axum::extract::FromRequestParts<AppState> for AuditCtx {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ua = parts
            .headers
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(512).collect::<String>());
        Ok(Self {
            ip: ClientIp::for_request(parts, state),
            ua: UserAgent(ua),
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn signup_needs_the_public_outbound_policy() {
        use hos_core::net::OutboundPolicy;
        assert!(super::signup_blocked_by_policy(
            OutboundPolicy::Private,
            false
        ));
        assert!(
            !super::signup_blocked_by_policy(OutboundPolicy::Private, true),
            "debug mode only warns"
        );
        assert!(!super::signup_blocked_by_policy(
            OutboundPolicy::Public,
            false
        ));
    }
}
