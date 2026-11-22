//! Platform console (SaaS operator) queries: tenant provisioning, platform
//! settings, plans and subscriptions management, usage and plan limits,
//! dashboard aggregates, platform operators, global audit / scheduled tasks,
//! self-service sign-ups, impersonation hand-offs and custom-domain checks.
//!
//! Like [`super::platform`] this module is tenant-less by design
//! (docs/ARCHITECTURE.md §3); every query that touches a tenant-owned table
//! either takes the tenant id explicitly or aggregates for the operator.

use std::collections::BTreeMap;
use std::net::IpAddr;

use chrono::{DateTime, Duration, Utc};
use hos_core::domain::{Locale, TenantStatus, UserKind};
use hos_core::ids::{
    PlatformAdminId, PlatformPlanId, TenantDomainId, TenantId, TenantSubscriptionId,
};
use hos_core::money::{BillingCycle, Currency};
use hos_core::tenant::Branding;
use rust_decimal::Decimal;
use sqlx::{Acquire, Connection, FromRow, PgConnection, PgExecutor, Postgres};
use uuid::Uuid;

use super::Json;
use super::audit::AuditLogRow;
use super::identity::{self, NewUser, UserRow};
use super::jobs::ScheduledTaskRow;
use super::platform::{
    self, NewTenant, PlatformAdminRow, PlatformPlanRow, TenantDomainRow, TenantFilter, TenantRow,
    TenantSubscriptionRow,
};
use crate::enums::{ActorKind, SubscriptionStatus, TenantDomainKind};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::page::{Page, Paged, fetch_paged};

// ---------------------------------------------------------------------------
// provisioning
// ---------------------------------------------------------------------------

/// The first staff account of a new tenant (role `owner`).
#[derive(Debug, Clone)]
pub struct ProvisionOwner {
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    /// Argon2 hash; `None` → the owner sets a password through an invitation link.
    pub password_hash: Option<String>,
    pub locale: Locale,
}

/// Input of [`provision_tenant`].
#[derive(Debug, Clone)]
pub struct ProvisionTenant {
    pub slug: String,
    pub name: String,
    /// `HOS_BASE_DOMAIN`; the tenant gets `<slug>.<base_domain>` as its primary host.
    pub base_domain: String,
    /// Optional custom domain (added unverified; DNS check verifies it).
    pub custom_domain: Option<String>,
    pub default_locale: Locale,
    pub default_currency: Currency,
    pub timezone: String,
    pub contact_email: Option<String>,
    pub branding: Branding,
    /// Plan of the subscription; `None` → no subscription row.
    pub plan_id: Option<PlatformPlanId>,
    pub billing_cycle: BillingCycle,
    /// Trial length; `None` → the plan's `trial_days`. `0` → active right away.
    pub trial_days: Option<i64>,
    pub owner: Option<ProvisionOwner>,
}

impl ProvisionTenant {
    /// Defaults: Turkish locale, TRY, `Europe/Istanbul`, monthly, plan trial.
    pub fn new(slug: &str, name: &str, base_domain: &str) -> Self {
        Self {
            slug: slug.trim().to_ascii_lowercase(),
            name: name.trim().to_owned(),
            base_domain: base_domain.trim().trim_matches('.').to_ascii_lowercase(),
            custom_domain: None,
            default_locale: Locale::Tr,
            default_currency: Currency::TRY,
            timezone: "Europe/Istanbul".to_owned(),
            contact_email: None,
            branding: Branding::default(),
            plan_id: None,
            billing_cycle: BillingCycle::Monthly,
            trial_days: None,
            owner: None,
        }
    }
}

/// Result of [`provision_tenant`].
#[derive(Debug, Clone)]
pub struct ProvisionedTenant {
    pub tenant: TenantRow,
    pub primary_domain: TenantDomainRow,
    pub custom_domain: Option<TenantDomainRow>,
    pub subscription: Option<TenantSubscriptionRow>,
    pub owner: Option<UserRow>,
}

/// Creates a tenant with everything it needs, in one transaction: the tenant
/// row, `<slug>.<base_domain>` (primary, verified) and an optional custom
/// domain, the defaults of [`crate::seed::seed_tenant_defaults`] (currencies,
/// tax rule, invoice numbering, roles, departments, settings, e-mail
/// templates), the plan subscription (trialing or active) and the owner
/// account. Duplicate slug / host → `Conflict`.
pub async fn provision_tenant(
    conn: &mut PgConnection,
    p: &ProvisionTenant,
) -> DbResult<ProvisionedTenant> {
    let mut tx = Connection::begin(&mut *conn).await?;
    let plan = match p.plan_id {
        Some(id) => Some(
            get_plan(&mut *tx, id)
                .await?
                .ok_or_else(|| crate::error::not_found("platform plan", id))?,
        ),
        None => None,
    };
    let trial_days = p
        .trial_days
        .or_else(|| plan.as_ref().map(|pl| i64::from(pl.trial_days)))
        .unwrap_or(0)
        .max(0);
    let now = Utc::now();
    let trial_end = (trial_days > 0).then(|| now + Duration::days(trial_days));

    let mut new = NewTenant::new(&p.slug, &p.name);
    new.status = if trial_end.is_some() {
        TenantStatus::Trial
    } else {
        TenantStatus::Active
    };
    new.default_locale = p.default_locale;
    new.default_currency = p.default_currency;
    new.timezone = p.timezone.clone();
    new.branding = p.branding.clone();
    new.contact_email = p.contact_email.clone();
    new.trial_ends_at = trial_end;
    let tenant = platform::create_tenant(&mut *tx, &new).await?;

    let host = format!("{}.{}", tenant.slug, p.base_domain);
    let primary_domain = platform::add_tenant_domain(
        &mut tx,
        tenant.id,
        &host,
        TenantDomainKind::Subdomain,
        true,
        true,
    )
    .await?;
    let custom_domain = match p
        .custom_domain
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(d) => Some(
            platform::add_tenant_domain(
                &mut tx,
                tenant.id,
                d,
                TenantDomainKind::Custom,
                false,
                false,
            )
            .await?,
        ),
        None => None,
    };

    crate::seed::seed_tenant_defaults(&mut tx, tenant.id).await?;

    let subscription = match &plan {
        Some(plan) => {
            let price = match p.billing_cycle {
                BillingCycle::Annual => plan.price_annual,
                _ => plan.price_monthly,
            };
            let (status, period_end) = match trial_end {
                Some(end) => (SubscriptionStatus::Trialing, Some(end)),
                None => (
                    SubscriptionStatus::Active,
                    Some(period_end_after(now, p.billing_cycle)),
                ),
            };
            Some(
                platform::create_subscription(
                    &mut *tx,
                    tenant.id,
                    plan.id,
                    status,
                    p.billing_cycle,
                    price,
                    plan.currency,
                    period_end,
                    trial_end,
                )
                .await?,
            )
        }
        None => None,
    };

    let owner = match &p.owner {
        Some(o) => {
            let user = identity::create_user(
                &mut *tx,
                tenant.id,
                &NewUser {
                    email: o.email.trim().to_lowercase(),
                    password_hash: o.password_hash.clone(),
                    kind: UserKind::Staff,
                    first_name: o.first_name.trim().to_owned(),
                    last_name: o.last_name.trim().to_owned(),
                    phone: None,
                    locale: o.locale,
                    email_verified: true,
                },
            )
            .await?;
            let role = identity::role_by_slug(&mut *tx, tenant.id, "owner")
                .await?
                .ok_or_else(|| crate::error::not_found("role", "owner"))?;
            identity::assign_role(&mut *tx, tenant.id, user.id, role.id).await?;
            Some(user)
        }
        None => None,
    };

    tx.commit().await?;
    Ok(ProvisionedTenant {
        tenant,
        primary_domain,
        custom_domain,
        subscription,
        owner,
    })
}

/// End of a billing period starting at `start`.
pub fn period_end_after(start: DateTime<Utc>, cycle: BillingCycle) -> DateTime<Utc> {
    let months = match cycle {
        BillingCycle::Annual => 12,
        BillingCycle::Quarterly => 3,
        BillingCycle::Semiannual => 6,
        BillingCycle::Biennial => 24,
        BillingCycle::Triennial => 36,
        _ => 1,
    };
    start
        .checked_add_months(chrono::Months::new(months))
        .unwrap_or(start + Duration::days(30 * i64::from(months)))
}

/// The oldest active staff user holding the `owner` role.
pub async fn tenant_owner(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Option<UserRow>> {
    Ok(sqlx::query_as::<_, UserRow>(
        "select u.* from users u
           join user_roles ur on ur.user_id = u.id and ur.tenant_id = u.tenant_id
           join roles r on r.id = ur.role_id and r.tenant_id = u.tenant_id
          where u.tenant_id = $1 and u.kind = 'staff' and u.status = 'active' and r.slug = 'owner'
          order by u.created_at, u.id
          limit 1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?)
}

/// Whether `slug` is used by a tenant or reserved by an unexpired sign-up.
pub async fn slug_taken(db: impl PgExecutor<'_>, slug: &str) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists (select 1 from tenants where slug = lower($1))
             or exists (select 1 from platform_signups
                         where slug = lower($1) and verified_at is null and expires_at > now())",
    )
    .bind(slug.trim())
    .fetch_one(db)
    .await?)
}

/// Replaces one top-level key of `tenants.metadata` (`notes`, `features`, `limits`).
pub async fn set_tenant_metadata_key(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
    value: &Json,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenants set metadata = coalesce(metadata, '{}'::jsonb) || jsonb_build_object($2::text, $3::jsonb)
          where id = $1",
    )
    .bind(tenant_id)
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// platform settings
// ---------------------------------------------------------------------------

/// One platform setting.
pub async fn get_platform_setting(db: impl PgExecutor<'_>, key: &str) -> DbResult<Option<Json>> {
    Ok(
        sqlx::query_scalar::<_, Json>("select value from platform_settings where key = $1")
            .bind(key)
            .fetch_optional(db)
            .await?,
    )
}

/// Every platform setting, by key.
pub async fn platform_settings(db: impl PgExecutor<'_>) -> DbResult<BTreeMap<String, Json>> {
    let rows: Vec<(String, Json)> =
        sqlx::query_as("select key, value from platform_settings order by key")
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Inserts or replaces a platform setting.
pub async fn set_platform_setting(
    db: impl PgExecutor<'_>,
    key: &str,
    value: &Json,
) -> DbResult<()> {
    sqlx::query(
        "insert into platform_settings (key, value) values ($1, $2)
         on conflict (key) do update set value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// dashboard
// ---------------------------------------------------------------------------

/// Monthly recurring revenue of live subscriptions in one currency.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PlatformMrr {
    pub currency: Currency,
    /// Annual prices are divided by 12.
    pub amount: Decimal,
    pub subscriptions: i64,
}

/// Platform MRR from `active` and `past_due` tenant subscriptions.
pub async fn platform_mrr(db: impl PgExecutor<'_>) -> DbResult<Vec<PlatformMrr>> {
    Ok(sqlx::query_as::<_, PlatformMrr>(
        "select currency,
                coalesce(sum(case when billing_cycle = 'annual' then price / 12 else price end), 0)::numeric(20, 6) as amount,
                count(*) as subscriptions
           from tenant_subscriptions
          where status in ('active', 'past_due')
          group by currency
          order by currency",
    )
    .fetch_all(db)
    .await?)
}

/// Business volume of one tenant (dashboard table).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TenantVolume {
    pub tenant_id: TenantId,
    pub slug: String,
    pub name: String,
    pub status: TenantStatus,
    /// The tenant's default currency (revenue is summed in it only).
    pub currency: Currency,
    pub clients: i64,
    pub active_services: i64,
    /// Paid invoices in the default currency, last 30 days.
    pub revenue_30d: Decimal,
    /// Paid invoices in the default currency, all time.
    pub revenue_total: Decimal,
}

/// Per-tenant clients / services / revenue, busiest first.
pub async fn tenant_volumes(db: impl PgExecutor<'_>, limit: i64) -> DbResult<Vec<TenantVolume>> {
    Ok(sqlx::query_as::<_, TenantVolume>(
        "select t.id as tenant_id, t.slug, t.name, t.status, t.default_currency as currency,
                (select count(*) from clients c where c.tenant_id = t.id and c.status <> 'closed') as clients,
                (select count(*) from services s where s.tenant_id = t.id and s.status = 'active') as active_services,
                (select coalesce(sum(i.amount_paid), 0) from invoices i
                  where i.tenant_id = t.id and i.currency = t.default_currency and i.status = 'paid'
                    and i.paid_at >= now() - interval '30 days')::numeric(20, 6) as revenue_30d,
                (select coalesce(sum(i.amount_paid), 0) from invoices i
                  where i.tenant_id = t.id and i.currency = t.default_currency and i.status = 'paid')::numeric(20, 6) as revenue_total
           from tenants t
          order by active_services desc, clients desc, t.created_at
          limit $1",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(db)
    .await?)
}

/// Health of the job queue.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct JobHealth {
    pub pending: i64,
    /// Pending and already due.
    pub due: i64,
    pub running: i64,
    /// Running with a lock older than 10 minutes (no heartbeat).
    pub stale: i64,
    pub failed_24h: i64,
    pub completed_24h: i64,
    /// `run_at` of the oldest due pending job.
    pub oldest_due_at: Option<DateTime<Utc>>,
    /// Latest job completion (worker liveness hint).
    pub last_completed_at: Option<DateTime<Utc>>,
}

/// Queue-wide counters (all tenants).
pub async fn job_health(db: impl PgExecutor<'_>) -> DbResult<JobHealth> {
    Ok(sqlx::query_as::<_, JobHealth>(
        "select
            count(*) filter (where status = 'pending') as pending,
            count(*) filter (where status = 'pending' and run_at <= now()) as due,
            count(*) filter (where status = 'running') as running,
            count(*) filter (where status = 'running' and locked_at < now() - interval '10 minutes') as stale,
            count(*) filter (where status = 'failed' and failed_at >= now() - interval '24 hours') as failed_24h,
            count(*) filter (where status = 'completed' and completed_at >= now() - interval '24 hours') as completed_24h,
            min(run_at) filter (where status = 'pending' and run_at <= now()) as oldest_due_at,
            max(completed_at) as last_completed_at
         from jobs",
    )
    .fetch_one(db)
    .await?)
}

/// Scheduled task with its tenant slug (`None` = platform task).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ScheduledTaskListRow {
    #[sqlx(flatten)]
    pub task: ScheduledTaskRow,
    pub tenant_slug: Option<String>,
}

/// Every scheduled task (platform first, then by next run).
pub async fn list_all_scheduled_tasks(
    db: impl PgExecutor<'_>,
    limit: i64,
) -> DbResult<Vec<ScheduledTaskListRow>> {
    Ok(sqlx::query_as::<_, ScheduledTaskListRow>(
        "select st.*, t.slug as tenant_slug
           from scheduled_tasks st left join tenants t on t.id = st.tenant_id
          order by st.tenant_id nulls first, st.next_run_at nulls last, st.name
          limit $1",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// tenant list / usage / limits
// ---------------------------------------------------------------------------

/// Tenant list row with its live subscription and size.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TenantSummary {
    #[sqlx(flatten)]
    pub tenant: TenantRow,
    pub plan_name: Option<String>,
    pub subscription_status: Option<SubscriptionStatus>,
    pub primary_host: Option<String>,
    pub clients: i64,
    pub active_services: i64,
}

/// Platform tenant list with plan and size, newest first.
pub async fn list_tenant_summaries<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    filter: &TenantFilter,
    page: Page,
) -> DbResult<Paged<TenantSummary>> {
    let search = filter.search.as_deref().map(like_contains);
    let status = filter.status;
    fetch_paged(
        db,
        "select t.*,
                (select p.name from tenant_subscriptions s join platform_plans p on p.id = s.plan_id
                  where s.tenant_id = t.id and s.status in ('trialing', 'active', 'past_due') limit 1) as plan_name,
                (select s.status from tenant_subscriptions s
                  where s.tenant_id = t.id and s.status in ('trialing', 'active', 'past_due') limit 1) as subscription_status,
                (select d.host from tenant_domains d where d.tenant_id = t.id and d.is_primary limit 1) as primary_host,
                (select count(*) from clients c where c.tenant_id = t.id and c.status <> 'closed') as clients,
                (select count(*) from services s where s.tenant_id = t.id and s.status = 'active') as active_services",
        |qb| {
            qb.push("from tenants t where true");
            if let Some(s) = &search {
                qb.push(" and (t.slug ilike ")
                    .push_bind(s.clone())
                    .push(" or t.name ilike ")
                    .push_bind(s.clone())
                    .push(" or t.contact_email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or exists (select 1 from tenant_domains d where d.tenant_id = t.id and d.host ilike ")
                    .push_bind(s.clone())
                    .push("))");
            }
            if let Some(st) = status {
                qb.push(" and t.status = ").push_bind(st);
            }
        },
        "t.created_at desc, t.id desc",
        page,
    )
    .await
}

/// What a tenant uses (detail page, plan limits).
#[derive(Debug, Clone, Default, PartialEq, FromRow)]
pub struct TenantUsage {
    /// Clients not closed.
    pub clients: i64,
    pub active_services: i64,
    /// Services counted against `max_services` (pending … suspended).
    pub live_services: i64,
    /// Staff users not disabled.
    pub staff: i64,
    pub custom_domains: i64,
    pub integrations: i64,
    pub domains: i64,
    pub open_tickets: i64,
    pub unpaid_invoices: i64,
    /// Paid invoices in the tenant's default currency.
    pub revenue_total: Decimal,
}

/// Usage counters of one tenant.
pub async fn tenant_usage(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<TenantUsage> {
    Ok(sqlx::query_as::<_, TenantUsage>(
        "select
            (select count(*) from clients where tenant_id = $1 and status <> 'closed') as clients,
            (select count(*) from services where tenant_id = $1 and status = 'active') as active_services,
            (select count(*) from services where tenant_id = $1
                and status in ('pending', 'provisioning', 'active', 'suspended')) as live_services,
            (select count(*) from users where tenant_id = $1 and kind = 'staff' and status <> 'disabled') as staff,
            (select count(*) from tenant_domains where tenant_id = $1 and kind = 'custom') as custom_domains,
            (select count(*) from integrations where tenant_id = $1) as integrations,
            (select count(*) from domains where tenant_id = $1) as domains,
            (select count(*) from tickets where tenant_id = $1 and status <> 'closed') as open_tickets,
            (select count(*) from invoices where tenant_id = $1 and status in ('unpaid', 'payment_pending')) as unpaid_invoices,
            (select coalesce(sum(i.amount_paid), 0) from invoices i join tenants t on t.id = i.tenant_id
              where i.tenant_id = $1 and i.status = 'paid' and i.currency = t.default_currency)::numeric(20, 6) as revenue_total",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await?)
}

/// Plan limit keys (`platform_plans.limits`).
pub const LIMIT_KEYS: &[&str] = &[
    "max_clients",
    "max_services",
    "max_staff",
    "custom_domains",
    "integrations",
];

/// Current usage against one plan limit.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct LimitStatus {
    /// Usage counted for the limit.
    pub used: i64,
    /// `None` = unlimited (no live subscription, key missing or `null`).
    pub max: Option<i64>,
}

impl LimitStatus {
    /// Whether `additional` more fit under the limit.
    pub fn allows(&self, additional: i64) -> bool {
        self.max.is_none_or(|m| self.used + additional <= m)
    }
}

/// Usage of `key` (one of [`LIMIT_KEYS`]) and its maximum: the tenant
/// override `tenants.metadata.limits.<key>` wins over the live plan's
/// `limits.<key>`. Unknown keys → `Validation`.
pub async fn plan_limit(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
) -> DbResult<LimitStatus> {
    let used_sql = match key {
        "max_clients" => "select count(*) from clients where tenant_id = $1 and status <> 'closed'",
        "max_services" => {
            "select count(*) from services where tenant_id = $1
                and status in ('pending', 'provisioning', 'active', 'suspended')"
        }
        "max_staff" => {
            "select count(*) from users where tenant_id = $1 and kind = 'staff' and status <> 'disabled'"
        }
        "custom_domains" => {
            "select count(*) from tenant_domains where tenant_id = $1 and kind = 'custom'"
        }
        "integrations" => "select count(*) from integrations where tenant_id = $1",
        other => {
            return Err(hos_core::CoreError::validation(format!(
                "unknown plan limit {other}"
            )));
        }
    };
    let sql = format!(
        "select ({used_sql}) as used,
                (select case
                    when t.metadata -> 'limits' ? $2 then
                        case when jsonb_typeof(t.metadata -> 'limits' -> $2) = 'number'
                             then (t.metadata -> 'limits' ->> $2)::numeric::bigint end
                    else
                        (select case when jsonb_typeof(p.limits -> $2) = 'number'
                                     then (p.limits ->> $2)::numeric::bigint end
                           from tenant_subscriptions s join platform_plans p on p.id = s.plan_id
                          where s.tenant_id = t.id and s.status in ('trialing', 'active', 'past_due')
                          limit 1)
                    end
                   from tenants t where t.id = $1) as max"
    );
    Ok(sqlx::query_as::<_, LimitStatus>(&sql)
        .bind(tenant_id)
        .bind(key)
        .fetch_one(db)
        .await?)
}

// ---------------------------------------------------------------------------
// plans
// ---------------------------------------------------------------------------

/// Input of [`create_plan`] / [`update_plan`].
#[derive(Debug, Clone)]
pub struct PlanInput {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub currency: Currency,
    pub price_monthly: Decimal,
    pub price_annual: Decimal,
    pub trial_days: i32,
    /// `{"max_clients": 500, ...}`; `null` values = unlimited.
    pub limits: Json,
    /// `["api", "white_label", ...]`.
    pub features: Json,
    pub is_public: bool,
    pub is_active: bool,
    pub sort_order: i32,
}

/// Every plan (inactive ones too), display order.
pub async fn list_all_plans(db: impl PgExecutor<'_>) -> DbResult<Vec<PlatformPlanRow>> {
    Ok(sqlx::query_as::<_, PlatformPlanRow>(
        "select * from platform_plans order by is_active desc, sort_order, price_monthly, code",
    )
    .fetch_all(db)
    .await?)
}

/// Plan by id.
pub async fn get_plan(
    db: impl PgExecutor<'_>,
    id: PlatformPlanId,
) -> DbResult<Option<PlatformPlanRow>> {
    Ok(
        sqlx::query_as::<_, PlatformPlanRow>("select * from platform_plans where id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Creates a plan. Duplicate code → `Conflict`.
pub async fn create_plan(db: impl PgExecutor<'_>, p: &PlanInput) -> DbResult<PlatformPlanRow> {
    sqlx::query_as::<_, PlatformPlanRow>(
        "insert into platform_plans (id, code, name, description, currency, price_monthly, price_annual,
                                     trial_days, limits, features, is_public, is_active, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         returning *",
    )
    .bind(PlatformPlanId::new())
    .bind(&p.code)
    .bind(&p.name)
    .bind(&p.description)
    .bind(p.currency)
    .bind(p.price_monthly)
    .bind(p.price_annual)
    .bind(p.trial_days)
    .bind(&p.limits)
    .bind(&p.features)
    .bind(p.is_public)
    .bind(p.is_active)
    .bind(p.sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("plan code"))
}

/// Updates a plan (existing subscriptions keep their price).
pub async fn update_plan(
    db: impl PgExecutor<'_>,
    id: PlatformPlanId,
    p: &PlanInput,
) -> DbResult<Option<PlatformPlanRow>> {
    sqlx::query_as::<_, PlatformPlanRow>(
        "update platform_plans set code = $2, name = $3, description = $4, currency = $5,
                price_monthly = $6, price_annual = $7, trial_days = $8, limits = $9, features = $10,
                is_public = $11, is_active = $12, sort_order = $13
          where id = $1
          returning *",
    )
    .bind(id)
    .bind(&p.code)
    .bind(&p.name)
    .bind(&p.description)
    .bind(p.currency)
    .bind(p.price_monthly)
    .bind(p.price_annual)
    .bind(p.trial_days)
    .bind(&p.limits)
    .bind(&p.features)
    .bind(p.is_public)
    .bind(p.is_active)
    .bind(p.sort_order)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("plan code"))
}

/// Subscriptions (any status) on a plan.
pub async fn plan_subscription_count(db: impl PgExecutor<'_>, id: PlatformPlanId) -> DbResult<i64> {
    Ok(
        sqlx::query_scalar::<_, i64>(
            "select count(*) from tenant_subscriptions where plan_id = $1",
        )
        .bind(id)
        .fetch_one(db)
        .await?,
    )
}

/// Deletes a plan that was never subscribed to (`Conflict` otherwise).
pub async fn delete_plan(db: impl PgExecutor<'_>, id: PlatformPlanId) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from platform_plans p where p.id = $1
            and not exists (select 1 from tenant_subscriptions s where s.plan_id = p.id)",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// subscriptions
// ---------------------------------------------------------------------------

/// Subscription with tenant and plan names.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SubscriptionListRow {
    #[sqlx(flatten)]
    pub subscription: TenantSubscriptionRow,
    pub tenant_slug: String,
    pub tenant_name: String,
    pub plan_code: String,
    pub plan_name: String,
}

const SUBSCRIPTION_SELECT: &str = "select s.*, t.slug as tenant_slug, t.name as tenant_name,
        p.code as plan_code, p.name as plan_name";

/// All subscriptions, filtered by status, ending soonest first.
pub async fn list_subscriptions<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    status: Option<SubscriptionStatus>,
    page: Page,
) -> DbResult<Paged<SubscriptionListRow>> {
    fetch_paged(
        db,
        SUBSCRIPTION_SELECT,
        |qb| {
            qb.push(
                "from tenant_subscriptions s join tenants t on t.id = s.tenant_id
                 join platform_plans p on p.id = s.plan_id where true",
            );
            if let Some(st) = status {
                qb.push(" and s.status = ").push_bind(st);
            }
        },
        "coalesce(s.trial_ends_at, s.current_period_end) asc nulls last, s.created_at desc",
        page,
    )
    .await
}

/// A tenant's subscription history, newest first.
pub async fn tenant_subscriptions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<SubscriptionListRow>> {
    let sql = format!(
        "{SUBSCRIPTION_SELECT}
           from tenant_subscriptions s join tenants t on t.id = s.tenant_id
           join platform_plans p on p.id = s.plan_id
          where s.tenant_id = $1
          order by s.created_at desc"
    );
    Ok(sqlx::query_as::<_, SubscriptionListRow>(&sql)
        .bind(tenant_id)
        .fetch_all(db)
        .await?)
}

/// Extends a subscription by `days`: the trial end while trialing (the
/// tenant's `trial_ends_at` follows), the current period end otherwise.
pub async fn extend_subscription(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: TenantSubscriptionId,
    days: i32,
) -> DbResult<bool> {
    let mut tx = Connection::begin(&mut *conn).await?;
    let row = sqlx::query_as::<_, TenantSubscriptionRow>(
        "update tenant_subscriptions set
            trial_ends_at = case when status = 'trialing'
                                 then greatest(coalesce(trial_ends_at, now()), now()) + make_interval(days => $3)
                                 else trial_ends_at end,
            current_period_end = greatest(coalesce(current_period_end, now()), now()) + make_interval(days => $3)
          where tenant_id = $1 and id = $2
          returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(days)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    if row.status == SubscriptionStatus::Trialing {
        sqlx::query("update tenants set trial_ends_at = $2 where id = $1")
            .bind(tenant_id)
            .bind(row.trial_ends_at)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Moves a live subscription to another plan / cycle at the plan's price.
pub async fn change_subscription_plan(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TenantSubscriptionId,
    plan: &PlatformPlanRow,
    cycle: BillingCycle,
) -> DbResult<bool> {
    let price = match cycle {
        BillingCycle::Annual => plan.price_annual,
        _ => plan.price_monthly,
    };
    let res = sqlx::query(
        "update tenant_subscriptions set plan_id = $3, billing_cycle = $4, price = $5, currency = $6
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(plan.id)
    .bind(cycle)
    .bind(price)
    .bind(plan.currency)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Marks a subscription paid / active for a new period (manual activation;
/// platform invoicing is out of scope).
pub async fn activate_subscription(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TenantSubscriptionId,
    period_end: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenant_subscriptions set status = 'active', current_period_start = now(),
                current_period_end = $3, cancel_at_period_end = false
          where tenant_id = $1 and id = $2 and status in ('trialing', 'active', 'past_due')",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(period_end)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// platform operators
// ---------------------------------------------------------------------------

/// Every platform operator, by e-mail.
pub async fn list_platform_admins(db: impl PgExecutor<'_>) -> DbResult<Vec<PlatformAdminRow>> {
    Ok(
        sqlx::query_as::<_, PlatformAdminRow>("select * from platform_admins order by email")
            .fetch_all(db)
            .await?,
    )
}

/// Deletes an operator unless it is the last active one (`Conflict`).
pub async fn delete_platform_admin(conn: &mut PgConnection, id: PlatformAdminId) -> DbResult<bool> {
    let mut tx = Connection::begin(&mut *conn).await?;
    sqlx::query("lock table platform_admins in share row exclusive mode")
        .execute(&mut *tx)
        .await?;
    let others: i64 = sqlx::query_scalar(
        "select count(*) from platform_admins where status = 'active' and id <> $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if others == 0 {
        return Err(hos_core::CoreError::Conflict(
            "the last platform operator cannot be removed".into(),
        ));
    }
    let res = sqlx::query("delete from platform_admins where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected() == 1)
}

/// TOTP state of an operator.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PlatformTotp {
    pub totp_secret_enc: Option<Vec<u8>>,
    pub totp_enabled: bool,
    pub totp_last_step: Option<i64>,
    pub totp_recovery_hashes: Vec<String>,
}

/// TOTP state of an operator.
pub async fn platform_admin_totp(
    db: impl PgExecutor<'_>,
    id: PlatformAdminId,
) -> DbResult<Option<PlatformTotp>> {
    Ok(sqlx::query_as::<_, PlatformTotp>(
        "select totp_secret_enc, totp_enabled, totp_last_step, totp_recovery_hashes
           from platform_admins where id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Stores a (pending or confirmed) TOTP secret; `enabled` switches 2FA on.
pub async fn set_platform_admin_totp(
    db: impl PgExecutor<'_>,
    id: PlatformAdminId,
    secret_enc: Option<&[u8]>,
    enabled: bool,
    recovery_hashes: &[String],
) -> DbResult<bool> {
    let res = sqlx::query(
        "update platform_admins set totp_secret_enc = $2, totp_enabled = $3,
                totp_recovery_hashes = $4, totp_last_step = case when $3 then totp_last_step end
          where id = $1",
    )
    .bind(id)
    .bind(secret_enc)
    .bind(enabled)
    .bind(recovery_hashes)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Records a used TOTP step; `false` if it (or a later one) was used already.
pub async fn use_platform_totp_step(
    db: impl PgExecutor<'_>,
    id: PlatformAdminId,
    step: i64,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update platform_admins set totp_last_step = $2
          where id = $1 and (totp_last_step is null or totp_last_step < $2)",
    )
    .bind(id)
    .bind(step)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Consumes a recovery code hash; `false` if unknown.
pub async fn use_platform_recovery_code(
    db: impl PgExecutor<'_>,
    id: PlatformAdminId,
    code_hash: &str,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update platform_admins set totp_recovery_hashes = array_remove(totp_recovery_hashes, $2)
          where id = $1 and $2 = any(totp_recovery_hashes)",
    )
    .bind(id)
    .bind(code_hash)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// global audit log
// ---------------------------------------------------------------------------

/// Filters of [`list_audit_global`].
#[derive(Debug, Clone, Default)]
pub struct GlobalAuditFilter {
    pub tenant_id: Option<TenantId>,
    /// Only platform-level entries (`tenant_id is null`).
    pub platform_only: bool,
    pub actor_kind: Option<ActorKind>,
    /// `action like '<prefix>%'`.
    pub action_prefix: Option<String>,
}

/// Audit entry with its tenant slug.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct GlobalAuditRow {
    #[sqlx(flatten)]
    pub entry: AuditLogRow,
    pub tenant_slug: Option<String>,
}

/// The audit log across tenants, newest first.
pub async fn list_audit_global<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    filter: &GlobalAuditFilter,
    page: Page,
) -> DbResult<Paged<GlobalAuditRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select a.*, t.slug as tenant_slug",
        move |qb| {
            qb.push("from audit_log a left join tenants t on t.id = a.tenant_id where true");
            if let Some(t) = f.tenant_id {
                qb.push(" and a.tenant_id = ").push_bind(t);
            }
            if f.platform_only {
                qb.push(" and a.tenant_id is null");
            }
            if let Some(k) = f.actor_kind {
                qb.push(" and a.actor_kind = ").push_bind(k);
            }
            if let Some(p) = f.action_prefix.as_deref().filter(|p| !p.trim().is_empty()) {
                let mut pattern = like_contains(p);
                pattern.remove(0);
                qb.push(" and a.action like ").push_bind(pattern);
            }
        },
        "a.created_at desc, a.id desc",
        page,
    )
    .await
}

// ---------------------------------------------------------------------------
// self-service sign-ups
// ---------------------------------------------------------------------------

/// `platform_signups` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SignupRow {
    pub id: Uuid,
    pub email: String,
    pub company_name: String,
    pub owner_name: String,
    pub slug: String,
    pub plan_id: Option<PlatformPlanId>,
    pub password_hash: Option<String>,
    pub locale: Locale,
    pub token_hash: String,
    pub ip: Option<IpAddr>,
    pub expires_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
    pub tenant_id: Option<TenantId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_signup`].
#[derive(Debug, Clone)]
pub struct NewSignup {
    pub email: String,
    pub company_name: String,
    pub owner_name: String,
    pub slug: String,
    pub plan_id: Option<PlatformPlanId>,
    pub password_hash: Option<String>,
    pub locale: Locale,
    pub token_hash: String,
    pub ip: Option<IpAddr>,
    pub expires_at: DateTime<Utc>,
}

/// Stores a pending sign-up.
pub async fn create_signup(db: impl PgExecutor<'_>, s: &NewSignup) -> DbResult<SignupRow> {
    Ok(sqlx::query_as::<_, SignupRow>(
        "insert into platform_signups (id, email, company_name, owner_name, slug, plan_id, password_hash,
                                       locale, token_hash, ip, expires_at)
         values ($1, $2, $3, $4, lower($5), $6, $7, $8, $9, $10, $11)
         returning *",
    )
    .bind(Uuid::now_v7())
    .bind(s.email.trim())
    .bind(s.company_name.trim())
    .bind(s.owner_name.trim())
    .bind(s.slug.trim())
    .bind(s.plan_id)
    .bind(&s.password_hash)
    .bind(s.locale)
    .bind(&s.token_hash)
    .bind(s.ip)
    .bind(s.expires_at)
    .fetch_one(db)
    .await?)
}

/// Unverified, unexpired sign-up by token hash.
pub async fn pending_signup_by_token(
    db: impl PgExecutor<'_>,
    token_hash: &str,
) -> DbResult<Option<SignupRow>> {
    Ok(sqlx::query_as::<_, SignupRow>(
        "select * from platform_signups
          where token_hash = $1 and verified_at is null and expires_at > now()",
    )
    .bind(token_hash)
    .fetch_optional(db)
    .await?)
}

/// Claims a pending sign-up for provisioning (stamps `verified_at`; single
/// use). `false` when already used or expired.
pub async fn claim_signup(db: impl PgExecutor<'_>, id: Uuid) -> DbResult<bool> {
    let res = sqlx::query(
        "update platform_signups set verified_at = now()
          where id = $1 and verified_at is null and expires_at > now()",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Releases a claim after a failed provisioning.
pub async fn unclaim_signup(db: impl PgExecutor<'_>, id: Uuid) -> DbResult<()> {
    sqlx::query(
        "update platform_signups set verified_at = null where id = $1 and tenant_id is null",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Links the provisioned tenant and forgets the password hash.
pub async fn link_signup_tenant(
    db: impl PgExecutor<'_>,
    id: Uuid,
    tenant_id: TenantId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update platform_signups set tenant_id = $2, password_hash = null where id = $1",
    )
    .bind(id)
    .bind(tenant_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Latest sign-ups (dashboard).
pub async fn recent_signups(db: impl PgExecutor<'_>, limit: i64) -> DbResult<Vec<SignupRow>> {
    Ok(sqlx::query_as::<_, SignupRow>(
        "select * from platform_signups order by created_at desc limit $1",
    )
    .bind(limit.clamp(1, 100))
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// impersonation hand-offs
// ---------------------------------------------------------------------------

/// Stores a one-time hand-off token (hash) for "sign in as tenant owner".
pub async fn create_handoff(
    db: impl PgExecutor<'_>,
    token_hash: &str,
    admin_id: PlatformAdminId,
    tenant_id: TenantId,
    user_id: hos_core::ids::UserId,
    expires_at: DateTime<Utc>,
) -> DbResult<()> {
    sqlx::query(
        "insert into platform_handoffs (id, token_hash, admin_id, tenant_id, user_id, expires_at)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(token_hash)
    .bind(admin_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok(())
}

/// A consumed hand-off.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Handoff {
    pub admin_id: PlatformAdminId,
    pub tenant_id: TenantId,
    pub user_id: hos_core::ids::UserId,
}

/// Consumes an unexpired, unused hand-off token (single use).
pub async fn consume_handoff(
    db: impl PgExecutor<'_>,
    token_hash: &str,
) -> DbResult<Option<Handoff>> {
    Ok(sqlx::query_as::<_, Handoff>(
        "update platform_handoffs set used_at = now()
          where token_hash = $1 and used_at is null and expires_at > now()
          returning admin_id, tenant_id, user_id",
    )
    .bind(token_hash)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// incoming webhooks
// ---------------------------------------------------------------------------

/// Owner of an integration (webhooks resolve the tenant from it, not from
/// the `Host` header: gateways may call the platform domain).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct IntegrationOwner {
    pub tenant_id: TenantId,
    pub category: hos_core::domain::IntegrationCategory,
    pub kind: String,
    pub is_active: bool,
    pub tenant_status: TenantStatus,
}

/// Tenant and state of an integration by id (tenant-less lookup).
pub async fn integration_owner(
    db: impl PgExecutor<'_>,
    id: hos_core::ids::IntegrationId,
) -> DbResult<Option<IntegrationOwner>> {
    Ok(sqlx::query_as::<_, IntegrationOwner>(
        "select i.tenant_id, i.category, i.kind, i.is_active, t.status as tenant_status
           from integrations i join tenants t on t.id = i.tenant_id
          where i.id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// custom domains
// ---------------------------------------------------------------------------

/// `tenant_domains` row with the check bookkeeping and tenant slug.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct DomainCheckRow {
    #[sqlx(flatten)]
    pub domain: TenantDomainRow,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub check_error: Option<String>,
    pub tenant_slug: String,
}

const DOMAIN_CHECK_SELECT: &str = "select d.*, t.slug as tenant_slug
      from tenant_domains d join tenants t on t.id = d.tenant_id";

/// Host names of a tenant with their check state, primary first.
pub async fn tenant_domain_checks(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<DomainCheckRow>> {
    let sql =
        format!("{DOMAIN_CHECK_SELECT} where d.tenant_id = $1 order by d.is_primary desc, d.host");
    Ok(sqlx::query_as::<_, DomainCheckRow>(&sql)
        .bind(tenant_id)
        .fetch_all(db)
        .await?)
}

/// One host name of a tenant.
pub async fn get_tenant_domain(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TenantDomainId,
) -> DbResult<Option<DomainCheckRow>> {
    let sql = format!("{DOMAIN_CHECK_SELECT} where d.tenant_id = $1 and d.id = $2");
    Ok(sqlx::query_as::<_, DomainCheckRow>(&sql)
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// Unverified custom domains due for a DNS check (not checked in the last
/// `min_interval`), least recently checked first. Tenants that are cancelled
/// are skipped.
pub async fn domains_due_for_check(
    db: impl PgExecutor<'_>,
    min_interval: std::time::Duration,
    limit: i64,
) -> DbResult<Vec<DomainCheckRow>> {
    let secs = i64::try_from(min_interval.as_secs()).unwrap_or(i64::MAX / 2);
    let sql = format!(
        "{DOMAIN_CHECK_SELECT}
          where d.kind = 'custom' and d.verified_at is null and t.status <> 'cancelled'
            and (d.last_checked_at is null or d.last_checked_at < now() - make_interval(secs => $1))
          order by d.last_checked_at nulls first, d.created_at
          limit $2"
    );
    Ok(sqlx::query_as::<_, DomainCheckRow>(&sql)
        .bind(secs as f64)
        .bind(limit.clamp(1, 1000))
        .fetch_all(db)
        .await?)
}

/// Records the outcome of a DNS check (`ok` stamps `verified_at`).
pub async fn record_domain_check(
    db: impl PgExecutor<'_>,
    id: TenantDomainId,
    ok: bool,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenant_domains set last_checked_at = now(), check_error = $3,
                verified_at = case when $2 then coalesce(verified_at, now()) else verified_at end
          where id = $1",
    )
    .bind(id)
    .bind(ok)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Whether a TLS certificate may be issued for `host`: a tenant subdomain or
/// a verified custom domain of a tenant that is not cancelled.
pub async fn tls_allowed(db: impl PgExecutor<'_>, host: &str) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists (
            select 1 from tenant_domains d join tenants t on t.id = d.tenant_id
             where d.host = $1 and (d.kind = 'subdomain' or d.verified_at is not null)
               and t.status <> 'cancelled')",
    )
    .bind(platform::normalize_host(host))
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// system
// ---------------------------------------------------------------------------

/// Applied schema migrations (`_sqlx_migrations`).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct MigrationStatus {
    pub applied: i64,
    pub latest: Option<i64>,
    pub latest_description: Option<String>,
}

/// Count and latest version of the successfully applied migrations.
pub async fn migration_status(db: impl PgExecutor<'_>) -> DbResult<MigrationStatus> {
    Ok(sqlx::query_as::<_, MigrationStatus>(
        "select count(*) as applied, max(version) as latest,
                (select description from _sqlx_migrations where success order by version desc limit 1)
                    as latest_description
           from _sqlx_migrations where success",
    )
    .fetch_one(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn period_end() {
        let start = Utc.with_ymd_and_hms(2026, 1, 31, 12, 0, 0).unwrap();
        assert_eq!(
            period_end_after(start, BillingCycle::Monthly),
            Utc.with_ymd_and_hms(2026, 2, 28, 12, 0, 0).unwrap()
        );
        assert_eq!(
            period_end_after(start, BillingCycle::Annual),
            Utc.with_ymd_and_hms(2027, 1, 31, 12, 0, 0).unwrap()
        );
    }

    #[test]
    fn limit_status() {
        let s = LimitStatus {
            used: 2,
            max: Some(3),
        };
        assert!(s.allows(1));
        assert!(!s.allows(2));
        assert!(
            LimitStatus {
                used: 10,
                max: None
            }
            .allows(100)
        );
    }
}
