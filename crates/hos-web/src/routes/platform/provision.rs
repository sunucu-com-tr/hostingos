//! Tenant provisioning shared by `hostingos tenant create`, the platform
//! console wizard and the self-service sign-up. The data work is
//! [`hos_db::repo::saas::provision_tenant`] (one transaction); this adds the
//! web-level parts: slug rules, password hashing, plan / trial defaults from
//! the platform settings, the owner invitation e-mail, scheduled tasks and
//! cache invalidation.

use chrono::{Duration, Utc};
use hos_auth::tokens::{TokenPurpose, issue_token_with_ttl};
use hos_core::domain::Locale;
use hos_core::money::{BillingCycle, Currency};
use hos_core::tenant::Branding;
use hos_db::repo::saas::{self, ProvisionOwner, ProvisionTenant, ProvisionedTenant};
use hos_db::repo::{identity, platform};
use serde_json::json;

use super::PlatformSettings;
use crate::error::{AppError, AppResult};
use crate::mail::{Recipient, send_email_logged};
use crate::state::AppState;

/// Sub-domain labels tenants cannot take.
pub const RESERVED_SLUGS: &[&str] = &[
    "www",
    "api",
    "app",
    "admin",
    "platform",
    "panel",
    "dashboard",
    "console",
    "auth",
    "login",
    "signup",
    "register",
    "account",
    "accounts",
    "billing",
    "pay",
    "payment",
    "checkout",
    "mail",
    "email",
    "smtp",
    "imap",
    "pop",
    "pop3",
    "webmail",
    "ftp",
    "sftp",
    "ssh",
    "dns",
    "ns",
    "ns1",
    "ns2",
    "ns3",
    "mx",
    "static",
    "assets",
    "cdn",
    "media",
    "img",
    "status",
    "help",
    "support",
    "docs",
    "doc",
    "blog",
    "news",
    "internal",
    "root",
    "system",
    "hostingos",
    "cpanel",
    "whm",
    "plesk",
    "localhost",
    "test",
    "staging",
    "dev",
    "beta",
    "proxy",
    "gateway",
    "webhooks",
    "www2",
];

/// Why a slug cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlugProblem {
    /// Not 3–40 chars of `a-z`, `0-9`, inner `-`.
    Invalid,
    /// In [`RESERVED_SLUGS`].
    Reserved,
    /// Used by a tenant or a pending sign-up.
    Taken,
}

impl SlugProblem {
    /// i18n key under `platform.slug.*`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Invalid => "platform.slug.invalid",
            Self::Reserved => "platform.slug.reserved",
            Self::Taken => "platform.slug.taken",
        }
    }
}

/// Normalised slug (`" Acme-Host "` → `acme-host`).
pub fn normalize_slug(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Syntax + reserved-name check (no database).
pub fn check_slug_syntax(slug: &str) -> Result<(), SlugProblem> {
    let ok_len = (3..=40).contains(&slug.len());
    let ok_chars = slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let ok_edges = !slug.starts_with('-') && !slug.ends_with('-') && !slug.contains("--");
    if !(ok_len && ok_chars && ok_edges) {
        return Err(SlugProblem::Invalid);
    }
    if RESERVED_SLUGS.contains(&slug) {
        return Err(SlugProblem::Reserved);
    }
    Ok(())
}

/// Full check including the database. Besides [`RESERVED_SLUGS`], the
/// label of the platform console's own host (`HOS_BASE_URL` under
/// `HOS_BASE_DOMAIN`, see [`crate::tenant::platform_host_label`]) is reserved.
pub async fn check_slug(state: &AppState, slug: &str) -> AppResult<Result<(), SlugProblem>> {
    if let Err(p) = check_slug_syntax(slug) {
        return Ok(Err(p));
    }
    if crate::tenant::platform_host_label(&state.config).as_deref() == Some(slug) {
        return Ok(Err(SlugProblem::Reserved));
    }
    if saas::slug_taken(&state.pool, slug).await? {
        return Ok(Err(SlugProblem::Taken));
    }
    Ok(Ok(()))
}

/// What to create.
#[derive(Debug, Clone)]
pub struct NewTenantRequest {
    pub slug: String,
    pub name: String,
    pub custom_domain: Option<String>,
    /// Mark the custom domain verified right away (CLI, self-host).
    pub verify_custom_domain: bool,
    pub owner_email: String,
    pub owner_first_name: String,
    pub owner_last_name: String,
    /// Clear-text password; `None` → invitation e-mail with a set-password link.
    pub owner_password: Option<String>,
    /// Already hashed password (self-service sign-up); wins over `owner_password`.
    pub owner_password_hash: Option<String>,
    /// Plan code; `None` → platform default plan (`signup.default_plan`).
    pub plan_code: Option<String>,
    pub billing_cycle: BillingCycle,
    /// `None` → platform default trial (`signup.trial_days`).
    pub trial_days: Option<i64>,
    pub locale: Locale,
    pub currency: Currency,
    pub contact_email: Option<String>,
}

impl NewTenantRequest {
    /// Minimal request (Turkish defaults, platform default plan + trial).
    pub fn new(slug: &str, name: &str, owner_email: &str) -> Self {
        Self {
            slug: normalize_slug(slug),
            name: name.trim().to_owned(),
            custom_domain: None,
            verify_custom_domain: false,
            owner_email: owner_email.trim().to_lowercase(),
            owner_first_name: String::new(),
            owner_last_name: String::new(),
            owner_password: None,
            owner_password_hash: None,
            plan_code: None,
            billing_cycle: BillingCycle::Monthly,
            trial_days: None,
            locale: Locale::Tr,
            currency: Currency::TRY,
            contact_email: None,
        }
    }
}

/// Result of [`provision`].
#[derive(Debug, Clone)]
pub struct ProvisionResult {
    pub provisioned: ProvisionedTenant,
    /// `scheme://<slug>.<base>[:port]`.
    pub tenant_url: String,
    /// Set-password link sent to the owner (no password given).
    pub invite_url: Option<String>,
}

/// `scheme://host[:port]` for a tenant host, scheme and port taken from
/// `HOS_BASE_URL` (so `http://localhost:8080` gives `http://acme.localhost:8080`).
pub fn tenant_origin(config: &hos_core::config::AppConfig, host: &str) -> String {
    let base = &config.base_url;
    match base.port() {
        Some(p) => format!("{}://{host}:{p}", base.scheme()),
        None => format!("{}://{host}", base.scheme()),
    }
}

async fn hash_password(password: &str) -> AppResult<String> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hos_auth::password::hash_password(&password))
        .await?
        .map_err(|e| AppError::internal(format!("hashing password: {e}")))
}

/// Creates a tenant (see module docs). Slug / host conflicts → `409`,
/// invalid input → `400`.
pub async fn provision(state: &AppState, req: &NewTenantRequest) -> AppResult<ProvisionResult> {
    let slug = normalize_slug(&req.slug);
    if let Err(p) = check_slug(state, &slug).await? {
        return Err(match p {
            SlugProblem::Taken => AppError::Conflict(format!("slug {slug} is taken")),
            SlugProblem::Reserved => AppError::BadRequest(format!("slug {slug} is reserved")),
            SlugProblem::Invalid => AppError::BadRequest(format!("slug {slug} is invalid")),
        });
    }
    if req.name.trim().is_empty() {
        return Err(AppError::bad_request("company name is required"));
    }
    if !crate::forms::valid_email(&req.owner_email) {
        return Err(AppError::bad_request("owner e-mail is invalid"));
    }
    let settings = PlatformSettings::load(state).await?;
    let plan_code = req.plan_code.clone().or(settings.default_plan.clone());
    let plan = match plan_code.as_deref().filter(|c| !c.is_empty()) {
        Some(code) => Some(
            platform::get_plan_by_code(&state.pool, code)
                .await?
                .ok_or_else(|| AppError::BadRequest(format!("unknown plan {code}")))?,
        ),
        None => None,
    };
    let password_hash = match (&req.owner_password_hash, &req.owner_password) {
        (Some(h), _) => Some(h.clone()),
        (None, Some(p)) if !p.is_empty() => Some(hash_password(p).await?),
        _ => None,
    };
    let custom_domain = req
        .custom_domain
        .as_deref()
        .map(platform::normalize_host)
        .filter(|d| !d.is_empty());
    let mut p = ProvisionTenant::new(&slug, &req.name, &state.config.base_domain);
    p.custom_domain = custom_domain;
    p.default_locale = req.locale;
    p.default_currency = req.currency;
    p.contact_email = req
        .contact_email
        .clone()
        .or_else(|| Some(req.owner_email.clone()));
    p.branding = Branding {
        company_name: Some(req.name.trim().to_owned()),
        support_email: Some(req.owner_email.clone()),
        ..Branding::default()
    };
    p.plan_id = plan.as_ref().map(|pl| pl.id);
    p.billing_cycle = req.billing_cycle;
    p.trial_days = Some(req.trial_days.unwrap_or(settings.trial_days).max(0));
    p.owner = Some(ProvisionOwner {
        email: req.owner_email.clone(),
        first_name: if req.owner_first_name.trim().is_empty() {
            req.name.trim().to_owned()
        } else {
            req.owner_first_name.clone()
        },
        last_name: req.owner_last_name.clone(),
        password_hash: password_hash.clone(),
        locale: req.locale,
    });
    let mut conn = state.pool.acquire().await?;
    let provisioned = saas::provision_tenant(&mut conn, &p).await?;
    drop(conn);
    if req.verify_custom_domain
        && let Some(d) = &provisioned.custom_domain
    {
        saas::record_domain_check(&state.pool, d.id, true, None).await?;
    }
    let tenant_url = tenant_origin(&state.config, &provisioned.primary_domain.host);

    // Default cron tasks right away (the scheduler re-ensures them anyway).
    let scheduler =
        hos_jobs::Scheduler::new(state.pool.clone(), hos_jobs::SchedulerConfig::default());
    if let Err(e) = scheduler.ensure_tenant_tasks(provisioned.tenant.id).await {
        tracing::warn!(error = %e, "ensuring tenant scheduled tasks failed");
    }
    state.tenants.invalidate_all();

    let invite_url = match (&provisioned.owner, password_hash) {
        (Some(owner), None) => Some(invite_owner(state, &provisioned, owner, &tenant_url).await?),
        _ => None,
    };
    Ok(ProvisionResult {
        provisioned,
        tenant_url,
        invite_url,
    })
}

/// [`provision`] for a claimed self-service sign-up: the sign-up row is
/// claimed first (single use, releases its slug reservation), linked to the
/// tenant on success and released again on failure.
pub async fn provision_reserved(
    state: &AppState,
    req: &NewTenantRequest,
    signup_id: uuid::Uuid,
) -> AppResult<ProvisionResult> {
    if !saas::claim_signup(&state.pool, signup_id).await? {
        return Err(AppError::Conflict("sign-up already used".into()));
    }
    match provision(state, req).await {
        Ok(r) => {
            saas::link_signup_tenant(&state.pool, signup_id, r.provisioned.tenant.id).await?;
            Ok(r)
        }
        Err(e) => {
            if let Err(err) = saas::unclaim_signup(&state.pool, signup_id).await {
                tracing::warn!(error = %err, "releasing sign-up claim failed");
            }
            Err(e)
        }
    }
}

/// Issues a 7-day set-password link for the owner and queues the
/// `staff_invite` e-mail. Returns the link.
pub async fn invite_owner(
    state: &AppState,
    provisioned: &ProvisionedTenant,
    owner: &identity::UserRow,
    tenant_url: &str,
) -> AppResult<String> {
    let issued = issue_token_with_ttl(TokenPurpose::PasswordReset, Utc::now(), Duration::days(7));
    identity::create_password_reset(
        &state.pool,
        provisioned.tenant.id,
        owner.id,
        &issued.token_hash,
        issued.expires_at,
        None,
    )
    .await?;
    let link = format!(
        "{tenant_url}/auth/reset?token={}",
        issued.token.expose_str()
    );
    let ctx = platform::tenant_context(&state.pool, provisioned.tenant.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut to = Recipient::new(owner.email.clone(), Some(owner.display_name()));
    to.user_id = Some(owner.id);
    send_email_logged(
        state,
        &ctx,
        hos_mail::defaults::keys::STAFF_INVITE,
        owner.locale,
        &to,
        json!({
            "action_url": link,
            "inviter": { "name": "HostingOS" },
            "invitee": { "email": owner.email },
            "role": { "name": match owner.locale { Locale::Tr => "Sahip", Locale::En => "Owner" } },
            "expires_in_days": 7,
        }),
    )
    .await;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_rules() {
        assert_eq!(check_slug_syntax("acme"), Ok(()));
        assert_eq!(check_slug_syntax("acme-host-2"), Ok(()));
        assert_eq!(check_slug_syntax("ab"), Err(SlugProblem::Invalid));
        assert_eq!(check_slug_syntax("-acme"), Err(SlugProblem::Invalid));
        assert_eq!(check_slug_syntax("acme-"), Err(SlugProblem::Invalid));
        assert_eq!(check_slug_syntax("ac--me"), Err(SlugProblem::Invalid));
        assert_eq!(check_slug_syntax("Acme"), Err(SlugProblem::Invalid));
        assert_eq!(check_slug_syntax("acme.com"), Err(SlugProblem::Invalid));
        assert_eq!(check_slug_syntax("www"), Err(SlugProblem::Reserved));
        assert_eq!(check_slug_syntax("platform"), Err(SlugProblem::Reserved));
        assert_eq!(normalize_slug(" Acme "), "acme");
    }
}
