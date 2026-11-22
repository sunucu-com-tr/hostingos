//! Platform tables: tenants, their host names and settings, platform operators,
//! SaaS plans and tenant subscriptions. This is the only module with
//! tenant-less queries (docs/ARCHITECTURE.md §3).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hos_core::domain::{Locale, TenantStatus};
use hos_core::ids::{
    PlatformAdminId, PlatformPlanId, TenantDomainId, TenantId, TenantSubscriptionId,
};
use hos_core::money::{BillingCycle, Currency};
use hos_core::tenant::{Branding, TenantContext};
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};

use super::{Json, empty_object};
use crate::enums::{AccountStatus, SubscriptionStatus, TenantDomainKind, TlsStatus};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::ids::TenantSettingId;
use crate::page::{Page, Paged, fetch_paged};

// ---------------------------------------------------------------------------
// tenants
// ---------------------------------------------------------------------------

/// `tenants` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TenantRow {
    pub id: TenantId,
    pub slug: String,
    pub name: String,
    pub status: TenantStatus,
    pub default_locale: Locale,
    pub default_currency: Currency,
    pub timezone: String,
    /// [`Branding`] JSON; read it with [`TenantRow::branding`].
    pub branding: Json,
    pub legal_name: Option<String>,
    pub tax_office: Option<String>,
    pub tax_number: Option<String>,
    pub address: Json,
    pub contact_email: Option<String>,
    pub contact_phone: Option<String>,
    pub trial_ends_at: Option<DateTime<Utc>>,
    pub suspended_at: Option<DateTime<Utc>>,
    pub suspended_reason: Option<String>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub metadata: Json,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TenantRow {
    /// Parsed branding; invalid JSON (e.g. a malformed colour) falls back to defaults.
    pub fn branding(&self) -> Branding {
        serde_json::from_value(self.branding.clone()).unwrap_or_default()
    }

    /// Builds the request context (`primary_domain` from `tenant_domains`).
    pub fn to_context(&self, primary_domain: Option<String>) -> TenantContext {
        TenantContext {
            id: self.id,
            slug: self.slug.clone(),
            name: self.name.clone(),
            status: self.status,
            default_locale: self.default_locale,
            default_currency: self.default_currency,
            timezone: self.timezone.clone(),
            primary_domain,
            branding: self.branding(),
        }
    }
}

#[derive(FromRow)]
struct TenantWithPrimary {
    #[sqlx(flatten)]
    tenant: TenantRow,
    primary_domain: Option<String>,
}

/// Input of [`create_tenant`].
#[derive(Debug, Clone)]
pub struct NewTenant {
    pub slug: String,
    pub name: String,
    pub status: TenantStatus,
    pub default_locale: Locale,
    pub default_currency: Currency,
    pub timezone: String,
    pub branding: Branding,
    pub contact_email: Option<String>,
    pub trial_ends_at: Option<DateTime<Utc>>,
}

impl NewTenant {
    /// Trial tenant with Turkish defaults (`tr`, `TRY`, `Europe/Istanbul`).
    pub fn new(slug: &str, name: &str) -> Self {
        Self {
            slug: slug.trim().to_ascii_lowercase(),
            name: name.trim().to_owned(),
            status: TenantStatus::Trial,
            default_locale: Locale::Tr,
            default_currency: Currency::TRY,
            timezone: "Europe/Istanbul".to_owned(),
            branding: Branding::default(),
            contact_email: None,
            trial_ends_at: None,
        }
    }
}

/// Inserts a tenant. Duplicate slug → `CoreError::Conflict`.
pub async fn create_tenant(db: impl PgExecutor<'_>, new: &NewTenant) -> DbResult<TenantRow> {
    let branding = serde_json::to_value(&new.branding).unwrap_or_else(|_| empty_object());
    sqlx::query_as::<_, TenantRow>(
        "insert into tenants (id, slug, name, status, default_locale, default_currency, timezone,
                              branding, contact_email, trial_ends_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         returning *",
    )
    .bind(TenantId::new())
    .bind(&new.slug)
    .bind(&new.name)
    .bind(new.status)
    .bind(new.default_locale)
    .bind(new.default_currency)
    .bind(&new.timezone)
    .bind(branding)
    .bind(&new.contact_email)
    .bind(new.trial_ends_at)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("tenant slug"))
}

/// Tenant by id.
pub async fn get_tenant(db: impl PgExecutor<'_>, id: TenantId) -> DbResult<Option<TenantRow>> {
    Ok(
        sqlx::query_as::<_, TenantRow>("select * from tenants where id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Tenant by slug (case-insensitive input).
pub async fn get_tenant_by_slug(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> DbResult<Option<TenantRow>> {
    Ok(
        sqlx::query_as::<_, TenantRow>("select * from tenants where slug = lower($1)")
            .bind(slug.trim())
            .fetch_optional(db)
            .await?,
    )
}

/// Normalises a `Host` header value: lower-case, port and trailing dot removed
/// (`Demo.Example.com:8080.` → `demo.example.com`, `[::1]:80` → `::1`).
pub fn normalize_host(raw: &str) -> String {
    let h = raw.trim();
    let without_port = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else if h.matches(':').count() == 1 {
        h.split(':').next().unwrap_or(h)
    } else {
        h
    };
    without_port.trim_end_matches('.').to_ascii_lowercase()
}

const CONTEXT_SELECT: &str = "select t.*,
        (select pd.host from tenant_domains pd
          where pd.tenant_id = t.id and pd.is_primary limit 1) as primary_domain";

/// Resolves the tenant serving `host` (raw `Host` header accepted) through
/// `tenant_domains`. Custom domains must be verified; subdomains always match.
/// Returns the tenant even when it is not operational — check
/// [`TenantContext::is_operational`].
pub async fn tenant_context_by_host(
    db: impl PgExecutor<'_>,
    host: &str,
) -> DbResult<Option<TenantContext>> {
    let sql = format!(
        "{CONTEXT_SELECT}
         from tenant_domains d
         join tenants t on t.id = d.tenant_id
         where d.host = $1 and (d.kind = 'subdomain' or d.verified_at is not null)"
    );
    let row = sqlx::query_as::<_, TenantWithPrimary>(&sql)
        .bind(normalize_host(host))
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.tenant.to_context(r.primary_domain)))
}

/// Context of a tenant by slug (development `HOS_DEV_TENANT`, `<slug>.<base>` fallback).
pub async fn tenant_context_by_slug(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> DbResult<Option<TenantContext>> {
    let sql = format!("{CONTEXT_SELECT} from tenants t where t.slug = lower($1)");
    let row = sqlx::query_as::<_, TenantWithPrimary>(&sql)
        .bind(slug.trim())
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.tenant.to_context(r.primary_domain)))
}

/// Context of a tenant by id.
pub async fn tenant_context(
    db: impl PgExecutor<'_>,
    id: TenantId,
) -> DbResult<Option<TenantContext>> {
    let sql = format!("{CONTEXT_SELECT} from tenants t where t.id = $1");
    let row = sqlx::query_as::<_, TenantWithPrimary>(&sql)
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.tenant.to_context(r.primary_domain)))
}

/// Filters of [`list_tenants`].
#[derive(Debug, Clone, Default)]
pub struct TenantFilter {
    /// Matches slug, name or contact e-mail.
    pub search: Option<String>,
    pub status: Option<TenantStatus>,
}

/// Platform tenant list, newest first.
pub async fn list_tenants<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    filter: &TenantFilter,
    page: Page,
) -> DbResult<Paged<TenantRow>> {
    let search = filter.search.as_deref().map(like_contains);
    let status = filter.status;
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from tenants where true");
            if let Some(s) = &search {
                qb.push(" and (slug ilike ")
                    .push_bind(s.clone())
                    .push(" or name ilike ")
                    .push_bind(s.clone())
                    .push(" or contact_email::text ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
            if let Some(st) = status {
                qb.push(" and status = ").push_bind(st);
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}

/// Ids of tenants that are served (`trial` / `active`) — cron jobs iterate these
/// and call the tenant-scoped repository functions per tenant.
pub async fn operational_tenant_ids(db: impl PgExecutor<'_>) -> DbResult<Vec<TenantId>> {
    Ok(sqlx::query_scalar::<_, TenantId>(
        "select id from tenants where status in ('trial', 'active') order by created_at",
    )
    .fetch_all(db)
    .await?)
}

/// Changes the tenant status, stamping `suspended_at` / `cancelled_at`.
pub async fn set_tenant_status(
    db: impl PgExecutor<'_>,
    id: TenantId,
    status: TenantStatus,
    reason: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenants set status = $2,
                suspended_at = case when $2 = 'suspended' then now() else null end,
                suspended_reason = case when $2 = 'suspended' then $3 else null end,
                cancelled_at = case when $2 = 'cancelled' then coalesce(cancelled_at, now()) else cancelled_at end
         where id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Updates name / locale / currency / timezone.
pub async fn update_tenant_profile(
    db: impl PgExecutor<'_>,
    id: TenantId,
    name: &str,
    default_locale: Locale,
    default_currency: Currency,
    timezone: &str,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenants set name = $2, default_locale = $3, default_currency = $4, timezone = $5
         where id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(default_locale)
    .bind(default_currency)
    .bind(timezone)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Replaces the branding JSON.
pub async fn update_branding(
    db: impl PgExecutor<'_>,
    id: TenantId,
    branding: &Branding,
) -> DbResult<bool> {
    let value = serde_json::to_value(branding).unwrap_or_else(|_| empty_object());
    let res = sqlx::query("update tenants set branding = $2 where id = $1")
        .bind(id)
        .bind(value)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Deletes a tenant and (via `on delete cascade`) every row it owns.
pub async fn delete_tenant(db: impl PgExecutor<'_>, id: TenantId) -> DbResult<bool> {
    let res = sqlx::query("delete from tenants where id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// tenant_domains
// ---------------------------------------------------------------------------

/// `tenant_domains` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TenantDomainRow {
    pub id: TenantDomainId,
    pub tenant_id: TenantId,
    pub host: String,
    pub kind: TenantDomainKind,
    pub is_primary: bool,
    pub verification_token: Option<String>,
    pub verified_at: Option<DateTime<Utc>>,
    pub tls_status: TlsStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Adds a host name. With `is_primary` the previous primary is demoted in the
/// same transaction. `verified` stamps `verified_at` (subdomains, localhost).
pub async fn add_tenant_domain(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    host: &str,
    kind: TenantDomainKind,
    is_primary: bool,
    verified: bool,
) -> DbResult<TenantDomainRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    if is_primary {
        sqlx::query(
            "update tenant_domains set is_primary = false where tenant_id = $1 and is_primary",
        )
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query_as::<_, TenantDomainRow>(
        "insert into tenant_domains (id, tenant_id, host, kind, is_primary, verification_token, verified_at)
         values ($1, $2, $3, $4, $5, $6, case when $7 then now() end)
         returning *",
    )
    .bind(TenantDomainId::new())
    .bind(tenant_id)
    .bind(normalize_host(host))
    .bind(kind)
    .bind(is_primary)
    .bind(hos_core::crypto::generate_token())
    .bind(verified)
    .fetch_one(&mut *tx)
    .await
    .map_err(conflict_on_unique("host"))?;
    tx.commit().await?;
    Ok(row)
}

/// Host names of a tenant, primary first.
pub async fn list_tenant_domains(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<TenantDomainRow>> {
    Ok(sqlx::query_as::<_, TenantDomainRow>(
        "select * from tenant_domains where tenant_id = $1 order by is_primary desc, host",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Marks a custom domain as verified (DNS TXT check passed).
pub async fn mark_tenant_domain_verified(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TenantDomainId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenant_domains set verified_at = coalesce(verified_at, now())
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Removes a host name.
pub async fn delete_tenant_domain(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TenantDomainId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from tenant_domains where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// tenant_settings
// ---------------------------------------------------------------------------

/// `tenant_settings` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TenantSettingRow {
    pub id: TenantSettingId,
    pub tenant_id: TenantId,
    pub key: String,
    pub value: Json,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One setting value.
pub async fn get_setting(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
) -> DbResult<Option<Json>> {
    Ok(sqlx::query_scalar::<_, Json>(
        "select value from tenant_settings where tenant_id = $1 and key = $2",
    )
    .bind(tenant_id)
    .bind(key)
    .fetch_optional(db)
    .await?)
}

/// All settings whose key starts with `prefix` (`""` = all), keyed by full key.
pub async fn get_settings(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    prefix: &str,
) -> DbResult<BTreeMap<String, Json>> {
    let rows: Vec<(String, Json)> = sqlx::query_as(
        "select key, value from tenant_settings
         where tenant_id = $1 and starts_with(key, $2)
         order by key",
    )
    .bind(tenant_id)
    .bind(prefix)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Inserts or replaces a setting.
pub async fn set_setting(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
    value: &Json,
) -> DbResult<()> {
    sqlx::query(
        "insert into tenant_settings (id, tenant_id, key, value) values ($1, $2, $3, $4)
         on conflict (tenant_id, key) do update set value = excluded.value",
    )
    .bind(TenantSettingId::new())
    .bind(tenant_id)
    .bind(key)
    .bind(value)
    .execute(db)
    .await
    .map_err(conflict_on_unique("setting"))?;
    Ok(())
}

/// Removes a setting.
pub async fn delete_setting(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
) -> DbResult<bool> {
    let res = sqlx::query("delete from tenant_settings where tenant_id = $1 and key = $2")
        .bind(tenant_id)
        .bind(key)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// platform_admins
// ---------------------------------------------------------------------------

/// `platform_admins` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PlatformAdminRow {
    pub id: PlatformAdminId,
    pub email: String,
    pub name: String,
    pub password_hash: Option<String>,
    pub totp_secret_enc: Option<Vec<u8>>,
    pub totp_enabled: bool,
    pub status: AccountStatus,
    pub last_login_at: Option<DateTime<Utc>>,
    pub last_login_ip: Option<std::net::IpAddr>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a platform operator.
pub async fn create_platform_admin(
    db: impl PgExecutor<'_>,
    email: &str,
    name: &str,
    password_hash: Option<&str>,
) -> DbResult<PlatformAdminRow> {
    sqlx::query_as::<_, PlatformAdminRow>(
        "insert into platform_admins (id, email, name, password_hash) values ($1, $2, $3, $4) returning *",
    )
    .bind(PlatformAdminId::new())
    .bind(email.trim())
    .bind(name)
    .bind(password_hash)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("platform admin email"))
}

/// Platform operator by e-mail (case-insensitive).
pub async fn platform_admin_by_email(
    db: impl PgExecutor<'_>,
    email: &str,
) -> DbResult<Option<PlatformAdminRow>> {
    Ok(sqlx::query_as::<_, PlatformAdminRow>(
        "select * from platform_admins where email = $1::citext",
    )
    .bind(email.trim())
    .fetch_optional(db)
    .await?)
}

/// Platform operator by id.
pub async fn get_platform_admin(
    db: impl PgExecutor<'_>,
    id: PlatformAdminId,
) -> DbResult<Option<PlatformAdminRow>> {
    Ok(
        sqlx::query_as::<_, PlatformAdminRow>("select * from platform_admins where id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Records a successful platform admin login.
pub async fn touch_platform_admin_login(
    db: impl PgExecutor<'_>,
    id: PlatformAdminId,
    ip: Option<std::net::IpAddr>,
) -> DbResult<()> {
    sqlx::query(
        "update platform_admins set last_login_at = now(), last_login_ip = $2 where id = $1",
    )
    .bind(id)
    .bind(ip)
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// platform_plans / tenant_subscriptions
// ---------------------------------------------------------------------------

/// `platform_plans` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PlatformPlanRow {
    pub id: PlatformPlanId,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub currency: Currency,
    pub price_monthly: Decimal,
    pub price_annual: Decimal,
    pub trial_days: i32,
    pub limits: Json,
    pub features: Json,
    pub is_public: bool,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Active plans (optionally only public ones) in display order.
pub async fn list_plans(
    db: impl PgExecutor<'_>,
    public_only: bool,
) -> DbResult<Vec<PlatformPlanRow>> {
    Ok(sqlx::query_as::<_, PlatformPlanRow>(
        "select * from platform_plans where is_active and (not $1 or is_public) order by sort_order, price_monthly",
    )
    .bind(public_only)
    .fetch_all(db)
    .await?)
}

/// Plan by code.
pub async fn get_plan_by_code(
    db: impl PgExecutor<'_>,
    code: &str,
) -> DbResult<Option<PlatformPlanRow>> {
    Ok(
        sqlx::query_as::<_, PlatformPlanRow>("select * from platform_plans where code = $1")
            .bind(code)
            .fetch_optional(db)
            .await?,
    )
}

/// `tenant_subscriptions` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TenantSubscriptionRow {
    pub id: TenantSubscriptionId,
    pub tenant_id: TenantId,
    pub plan_id: PlatformPlanId,
    pub status: SubscriptionStatus,
    /// `monthly` or `annual`.
    pub billing_cycle: BillingCycle,
    pub price: Decimal,
    pub currency: Currency,
    pub current_period_start: Option<DateTime<Utc>>,
    pub current_period_end: Option<DateTime<Utc>>,
    pub trial_ends_at: Option<DateTime<Utc>>,
    pub cancel_at_period_end: bool,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub external_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The live (trialing / active / past_due) subscription of a tenant.
pub async fn current_subscription(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Option<TenantSubscriptionRow>> {
    Ok(sqlx::query_as::<_, TenantSubscriptionRow>(
        "select * from tenant_subscriptions
         where tenant_id = $1 and status in ('trialing', 'active', 'past_due')",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?)
}

/// Starts a subscription. Fails with `Conflict` if a live one exists.
#[allow(clippy::too_many_arguments)]
pub async fn create_subscription(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    plan_id: PlatformPlanId,
    status: SubscriptionStatus,
    billing_cycle: BillingCycle,
    price: Decimal,
    currency: Currency,
    current_period_end: Option<DateTime<Utc>>,
    trial_ends_at: Option<DateTime<Utc>>,
) -> DbResult<TenantSubscriptionRow> {
    sqlx::query_as::<_, TenantSubscriptionRow>(
        "insert into tenant_subscriptions
            (id, tenant_id, plan_id, status, billing_cycle, price, currency,
             current_period_start, current_period_end, trial_ends_at)
         values ($1, $2, $3, $4, $5, $6, $7, now(), $8, $9)
         returning *",
    )
    .bind(TenantSubscriptionId::new())
    .bind(tenant_id)
    .bind(plan_id)
    .bind(status)
    .bind(billing_cycle)
    .bind(price)
    .bind(currency)
    .bind(current_period_end)
    .bind(trial_ends_at)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("live subscription"))
}

/// Changes a subscription's status.
pub async fn set_subscription_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TenantSubscriptionId,
    status: SubscriptionStatus,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenant_subscriptions set status = $3,
                cancelled_at = case when $3 = 'cancelled' then coalesce(cancelled_at, now()) else cancelled_at end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_normalisation() {
        assert_eq!(normalize_host("Demo.Example.com:8080"), "demo.example.com");
        assert_eq!(normalize_host("localhost"), "localhost");
        assert_eq!(normalize_host("example.com."), "example.com");
        assert_eq!(normalize_host("[::1]:8080"), "::1");
        assert_eq!(normalize_host(" LOCALHOST:3000 "), "localhost");
    }
}
