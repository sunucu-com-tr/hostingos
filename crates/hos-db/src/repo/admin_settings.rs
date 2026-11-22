//! Queries behind the admin settings, staff, developer and report screens:
//! staff invitations (`staff_invites`), uploaded brand assets
//! (`tenant_assets`), the company profile on `tenants`, the primary custom
//! domain, tenant-scoped edits of `scheduled_tasks` / `webhook_endpoints`,
//! webhook delivery history (the `deliver_webhook` jobs) and the extra
//! report aggregates (product groups, client acquisition, service flow,
//! domain portfolio).
//!
//! Every function takes the tenant id first and filters by it.

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::domain::Locale;
use hos_core::ids::{
    ClientId, DomainId, NotificationId, RoleId, TenantDomainId, TenantId, UserId, WebhookEndpointId,
};
use hos_core::money::Currency;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use super::Json;
use super::identity::RoleRow;
use super::invoices::InvoiceSequenceRow;
use super::jobs::{JobRow, ScheduledTaskRow};
use super::mail::EmailTemplateRow;
use super::notifications::NotificationRow;
use super::webhooks::WebhookEndpointRow;
use crate::error::{DbResult, conflict_on_unique};
use crate::ids::ScheduledTaskId;

// ---------------------------------------------------------------------------
// staff invitations
// ---------------------------------------------------------------------------

/// `staff_invites` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct StaffInviteRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub role_id: RoleId,
    pub token_hash: String,
    pub invited_by: Option<UserId>,
    pub locale: Locale,
    pub expires_at: DateTime<Utc>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub accepted_user_id: Option<UserId>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl StaffInviteRow {
    /// Neither accepted, revoked nor expired.
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        self.accepted_at.is_none() && self.revoked_at.is_none() && self.expires_at > now
    }
}

/// Input of [`create_staff_invite`].
#[derive(Debug, Clone)]
pub struct NewStaffInvite {
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub role_id: RoleId,
    pub token_hash: String,
    pub invited_by: Option<UserId>,
    pub locale: Locale,
    pub expires_at: DateTime<Utc>,
}

/// Creates an invitation; earlier open invitations of the same address are
/// revoked in the same statement (only the newest link works).
pub async fn create_staff_invite(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    new: &NewStaffInvite,
) -> DbResult<StaffInviteRow> {
    sqlx::query_as::<_, StaffInviteRow>(
        "with old as (
             update staff_invites set revoked_at = now()
             where tenant_id = $2 and email = $3::citext and accepted_at is null and revoked_at is null
         )
         insert into staff_invites (id, tenant_id, email, first_name, last_name, role_id, token_hash,
                                    invited_by, locale, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         returning *",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(new.email.trim())
    .bind(&new.first_name)
    .bind(&new.last_name)
    .bind(new.role_id)
    .bind(&new.token_hash)
    .bind(new.invited_by)
    .bind(new.locale)
    .bind(new.expires_at)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("invite"))
}

/// Open (not accepted / revoked / expired) invitations, newest first.
pub async fn open_staff_invites(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<StaffInviteRow>> {
    Ok(sqlx::query_as::<_, StaffInviteRow>(
        "select * from staff_invites
         where tenant_id = $1 and accepted_at is null and revoked_at is null and expires_at > now()
         order by created_at desc",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Invitation by token hash (any state; callers check [`StaffInviteRow::is_open`]).
pub async fn staff_invite_by_hash(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    token_hash: &str,
) -> DbResult<Option<StaffInviteRow>> {
    Ok(sqlx::query_as::<_, StaffInviteRow>(
        "select * from staff_invites where tenant_id = $1 and token_hash = $2",
    )
    .bind(tenant_id)
    .bind(token_hash)
    .fetch_optional(db)
    .await?)
}

/// Invitation by id.
pub async fn get_staff_invite(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<Option<StaffInviteRow>> {
    Ok(sqlx::query_as::<_, StaffInviteRow>(
        "select * from staff_invites where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Revokes an open invitation.
pub async fn revoke_staff_invite(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update staff_invites set revoked_at = now()
         where tenant_id = $1 and id = $2 and accepted_at is null and revoked_at is null",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Marks an open invitation accepted by `user_id`; `false` when it was not
/// open any more (accepted concurrently, revoked or expired).
pub async fn accept_staff_invite(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    user_id: UserId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update staff_invites set accepted_at = now(), accepted_user_id = $3
         where tenant_id = $1 and id = $2 and accepted_at is null and revoked_at is null and expires_at > now()",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// roles
// ---------------------------------------------------------------------------

/// Role by id.
pub async fn get_role(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RoleId,
) -> DbResult<Option<RoleRow>> {
    Ok(
        sqlx::query_as::<_, RoleRow>("select * from roles where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Renames / re-describes a role.
pub async fn update_role(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RoleId,
    name: &str,
    description: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update roles set name = $3, description = $4
         where tenant_id = $1 and id = $2 and not is_system",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(name)
    .bind(description)
    .execute(db)
    .await
    .map_err(conflict_on_unique("role"))?;
    Ok(res.rows_affected() == 1)
}

/// `(user, role)` pairs of every staff user of the tenant.
pub async fn staff_role_pairs(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(UserId, RoleId)>> {
    Ok(sqlx::query_as::<_, (UserId, RoleId)>(
        "select ur.user_id, ur.role_id from user_roles ur
         join users u on u.id = ur.user_id
         where ur.tenant_id = $1 and u.kind = 'staff'",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Active staff users holding `role_id`.
pub async fn active_role_members(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    role_id: RoleId,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from user_roles ur join users u on u.id = ur.user_id
         where ur.tenant_id = $1 and ur.role_id = $2 and u.kind = 'staff' and u.status = 'active'",
    )
    .bind(tenant_id)
    .bind(role_id)
    .fetch_one(db)
    .await?)
}

/// Users (any status) holding `role_id`.
pub async fn role_member_count(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    role_id: RoleId,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from user_roles where tenant_id = $1 and role_id = $2",
    )
    .bind(tenant_id)
    .bind(role_id)
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// tenants: company profile, localisation
// ---------------------------------------------------------------------------

/// Company facts printed on invoices, the storefront footer and e-mails.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompanyProfile {
    pub name: String,
    pub legal_name: Option<String>,
    pub tax_office: Option<String>,
    pub tax_number: Option<String>,
    /// `{"line1", "city", "state", "postal_code", "country_code"}`.
    pub address: Json,
    pub contact_email: Option<String>,
    pub contact_phone: Option<String>,
}

/// Updates the company profile columns of `tenants`.
pub async fn update_company_profile(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    p: &CompanyProfile,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update tenants set name = $2, legal_name = $3, tax_office = $4, tax_number = $5, address = $6,
                contact_email = $7, contact_phone = $8
         where id = $1",
    )
    .bind(tenant_id)
    .bind(&p.name)
    .bind(&p.legal_name)
    .bind(&p.tax_office)
    .bind(&p.tax_number)
    .bind(&p.address)
    .bind(&p.contact_email)
    .bind(&p.contact_phone)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// The tenant's invoice numbering row (`None` before the first invoice / setup).
pub async fn invoice_sequence(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Option<InvoiceSequenceRow>> {
    Ok(sqlx::query_as::<_, InvoiceSequenceRow>(
        "select * from invoice_sequences where tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// tenant_assets
// ---------------------------------------------------------------------------

/// `tenant_assets` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TenantAssetRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub kind: String,
    pub content_type: String,
    pub data: Vec<u8>,
    pub sha256: String,
    pub size_bytes: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Inserts or replaces the asset of `kind` (`logo`, `logo_dark`, `favicon`).
pub async fn put_tenant_asset(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    kind: &str,
    content_type: &str,
    data: &[u8],
    sha256: &str,
) -> DbResult<()> {
    sqlx::query(
        "insert into tenant_assets (id, tenant_id, kind, content_type, data, sha256, size_bytes)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (tenant_id, kind) do update
            set content_type = excluded.content_type, data = excluded.data,
                sha256 = excluded.sha256, size_bytes = excluded.size_bytes",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(kind)
    .bind(content_type)
    .bind(data)
    .bind(sha256)
    .bind(i32::try_from(data.len()).unwrap_or(i32::MAX))
    .execute(db)
    .await?;
    Ok(())
}

/// The asset of `kind`.
pub async fn get_tenant_asset(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    kind: &str,
) -> DbResult<Option<TenantAssetRow>> {
    Ok(sqlx::query_as::<_, TenantAssetRow>(
        "select * from tenant_assets where tenant_id = $1 and kind = $2",
    )
    .bind(tenant_id)
    .bind(kind)
    .fetch_optional(db)
    .await?)
}

/// Removes the asset of `kind`.
pub async fn delete_tenant_asset(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    kind: &str,
) -> DbResult<bool> {
    let res = sqlx::query("delete from tenant_assets where tenant_id = $1 and kind = $2")
        .bind(tenant_id)
        .bind(kind)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// email_templates
// ---------------------------------------------------------------------------

/// Removes the tenant's copy of a template (the built-in default applies again).
pub async fn delete_email_template(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
    locale: Locale,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from email_templates where tenant_id = $1 and key = $2 and locale = $3",
    )
    .bind(tenant_id)
    .bind(key)
    .bind(locale)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// The tenant's row for `(key, locale)` regardless of `is_active`.
pub async fn email_template(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key: &str,
    locale: Locale,
) -> DbResult<Option<EmailTemplateRow>> {
    Ok(sqlx::query_as::<_, EmailTemplateRow>(
        "select * from email_templates where tenant_id = $1 and key = $2 and locale = $3",
    )
    .bind(tenant_id)
    .bind(key)
    .bind(locale)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// tenant_domains
// ---------------------------------------------------------------------------

/// Makes `id` the primary host name (subdomains and verified custom domains
/// only). `false` when the row is missing or not eligible.
pub async fn set_primary_domain(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: TenantDomainId,
) -> DbResult<bool> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let eligible: Option<bool> = sqlx::query_scalar(
        "select kind = 'subdomain' or verified_at is not null from tenant_domains
         where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if eligible != Some(true) {
        return Ok(false);
    }
    sqlx::query("update tenant_domains set is_primary = false where tenant_id = $1 and is_primary")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update tenant_domains set is_primary = true where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// scheduled_tasks (tenant scoped)
// ---------------------------------------------------------------------------

/// A scheduled task of the tenant.
pub async fn tenant_task(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ScheduledTaskId,
) -> DbResult<Option<ScheduledTaskRow>> {
    Ok(sqlx::query_as::<_, ScheduledTaskRow>(
        "select * from scheduled_tasks where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Changes schedule / state of a tenant task (`next_run_at` computed by the caller).
pub async fn update_tenant_task(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ScheduledTaskId,
    cron: &str,
    next_run_at: Option<DateTime<Utc>>,
    is_active: bool,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update scheduled_tasks set cron = $3, next_run_at = $4, is_active = $5
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(cron)
    .bind(next_run_at)
    .bind(is_active)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// webhook_endpoints (tenant scoped)
// ---------------------------------------------------------------------------

/// An endpoint of the tenant.
pub async fn get_endpoint(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookEndpointId,
) -> DbResult<Option<WebhookEndpointRow>> {
    Ok(sqlx::query_as::<_, WebhookEndpointRow>(
        "select * from webhook_endpoints where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Updates URL, description, events and state.
pub async fn update_endpoint(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookEndpointId,
    url: &str,
    description: Option<&str>,
    events: &[String],
    is_active: bool,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update webhook_endpoints set url = $3, description = $4, events = $5, is_active = $6,
                failure_count = case when $6 and not is_active then 0 else failure_count end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(url)
    .bind(description)
    .bind(events)
    .bind(is_active)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Replaces the (encrypted) signing secret.
pub async fn set_endpoint_secret(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookEndpointId,
    secret_enc: &[u8],
) -> DbResult<bool> {
    let res = sqlx::query(
        "update webhook_endpoints set secret_enc = $3 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(secret_enc)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// The `deliver_webhook` jobs of an endpoint (its delivery log), newest first.
pub async fn endpoint_deliveries(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookEndpointId,
    limit: i64,
) -> DbResult<Vec<JobRow>> {
    Ok(sqlx::query_as::<_, JobRow>(
        "select * from jobs
         where tenant_id = $1 and kind = 'deliver_webhook' and payload->>'endpoint_id' = $2
         order by created_at desc limit $3",
    )
    .bind(tenant_id)
    .bind(id.to_string())
    .bind(limit.clamp(1, 200))
    .fetch_all(db)
    .await?)
}

/// One delivery job of an endpoint.
pub async fn endpoint_delivery(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookEndpointId,
    job_id: Uuid,
) -> DbResult<Option<JobRow>> {
    Ok(sqlx::query_as::<_, JobRow>(
        "select * from jobs
         where tenant_id = $1 and kind = 'deliver_webhook' and payload->>'endpoint_id' = $2 and id = $3",
    )
    .bind(tenant_id)
    .bind(id.to_string())
    .bind(job_id)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// reports
// ---------------------------------------------------------------------------

/// Validated `date_trunc` unit (`day`, `week`, `month`, `year`).
fn trunc_unit(unit: &str) -> &'static str {
    match unit {
        "day" => "day",
        "week" => "week",
        "year" => "year",
        _ => "month",
    }
}

/// Revenue of a product group (paid invoice lines, excluding tax).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct GroupRevenueRow {
    /// `None`: lines without a product group (see `is_domain`).
    pub group_name: Option<String>,
    /// Ungrouped domain lines (registrations, renewals, transfers).
    pub is_domain: bool,
    pub currency: Currency,
    pub amount: Decimal,
    pub line_count: i64,
}

/// Paid invoice lines grouped by the product group of their service.
pub async fn revenue_by_product_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<Vec<GroupRevenueRow>> {
    Ok(sqlx::query_as::<_, GroupRevenueRow>(
        "select g.name as group_name,
                (g.id is null and (ii.domain_id is not null or ii.item_type = 'domain')) as is_domain,
                i.currency, sum(ii.amount) as amount, count(*) as line_count
         from invoice_items ii
         join invoices i on i.id = ii.invoice_id
         left join services s on s.id = ii.service_id
         left join products p on p.id = s.product_id
         left join product_groups g on g.id = p.group_id
         where i.tenant_id = $1 and i.status in ('paid', 'partially_refunded')
           and i.paid_at >= $2 and i.paid_at < $3
         group by g.id, g.name, 2, i.currency order by amount desc",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?)
}

/// New clients and new / ended recurring services of one period bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, FromRow)]
pub struct FlowRow {
    /// First day of the bucket (UTC).
    pub period: NaiveDate,
    pub new_clients: i64,
    pub new_services: i64,
    pub ended_services: i64,
}

/// Client acquisition and service flow per `unit` bucket over `[from, to)`.
pub async fn acquisition_flow(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    unit: &str,
) -> DbResult<Vec<FlowRow>> {
    Ok(sqlx::query_as::<_, FlowRow>(
        "with ev as (
             select date_trunc($4, created_at at time zone 'UTC')::date as period, 1 as c, 0 as n, 0 as e
             from clients where tenant_id = $1 and created_at >= $2 and created_at < $3
             union all
             select date_trunc($4, created_at at time zone 'UTC')::date, 0, 1, 0
             from services where tenant_id = $1 and billing_cycle <> 'one_time'
               and created_at >= $2 and created_at < $3
             union all
             select date_trunc($4, coalesce(terminated_at, cancelled_at) at time zone 'UTC')::date, 0, 0, 1
             from services where tenant_id = $1 and billing_cycle <> 'one_time'
               and coalesce(terminated_at, cancelled_at) >= $2 and coalesce(terminated_at, cancelled_at) < $3
         )
         select period, sum(c)::bigint as new_clients, sum(n)::bigint as new_services,
                sum(e)::bigint as ended_services
         from ev group by period order by period",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .bind(trunc_unit(unit))
    .fetch_all(db)
    .await?)
}

/// Domain count of one TLD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct TldShareRow {
    pub tld: String,
    pub total: i64,
    pub active: i64,
    pub auto_renew: i64,
}

/// Live domains per TLD (largest first).
pub async fn domain_tld_shares(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<TldShareRow>> {
    Ok(sqlx::query_as::<_, TldShareRow>(
        "select tld, count(*) as total,
                count(*) filter (where status = 'active') as active,
                count(*) filter (where auto_renew) as auto_renew
         from domains
         where tenant_id = $1 and status not in ('expired', 'transferred_away', 'cancelled', 'fraud')
         group by tld order by 2 desc, 1",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// A domain expiring soon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct RenewalRow {
    pub id: DomainId,
    pub name: String,
    pub client_id: ClientId,
    pub client_name: String,
    pub status: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub recurring_amount: Decimal,
    pub currency: Currency,
}

/// Live domains expiring within `days` (soonest first).
pub async fn upcoming_renewals(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    days: i32,
    limit: i64,
) -> DbResult<Vec<RenewalRow>> {
    Ok(sqlx::query_as::<_, RenewalRow>(
        "select d.id, d.name::text as name, d.client_id,
                coalesce(nullif(btrim(c.company_name), ''), btrim(c.first_name || ' ' || c.last_name)) as client_name,
                d.status, d.expires_at, d.auto_renew, d.recurring_amount, d.currency
         from domains d join clients c on c.id = d.client_id
         where d.tenant_id = $1 and d.status in ('active', 'grace', 'redemption')
           and d.expires_at is not null and d.expires_at < now() + make_interval(days => $2)
         order by d.expires_at limit $3",
    )
    .bind(tenant_id)
    .bind(days.clamp(1, 3650))
    .bind(limit.clamp(1, 500))
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// notifications
// ---------------------------------------------------------------------------

/// One notification of `user_id` (the bell's "open" action).
pub async fn notification(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    id: NotificationId,
) -> DbResult<Option<NotificationRow>> {
    Ok(sqlx::query_as::<_, NotificationRow>(
        "select * from notifications where tenant_id = $1 and user_id = $2 and id = $3",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// storefront
// ---------------------------------------------------------------------------

/// Top-level items in the visitor's open cart (the header's cart badge): the
/// signed-in client's newest open cart, else the guest cart of the cookie
/// token (only when it does not belong to an account).
pub async fn cart_item_count(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: Option<UserId>,
    token_hash: Option<&str>,
) -> DbResult<i64> {
    if user_id.is_none() && token_hash.is_none() {
        return Ok(0);
    }
    Ok(sqlx::query_scalar::<_, i64>(
        "with c as (
            (select id from carts
              where tenant_id = $1 and $2::uuid is not null and user_id = $2
                and converted_order_id is null and expires_at > now()
              order by updated_at desc limit 1)
            union all
            (select id from carts
              where tenant_id = $1 and $3::text is not null and token_hash = $3 and user_id is null
                and converted_order_id is null and expires_at > now()
              limit 1)
            limit 1
         )
         select count(*) from cart_items
         where tenant_id = $1 and parent_item_id is null and cart_id in (select id from c)",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(token_hash)
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// support notifications
// ---------------------------------------------------------------------------

/// Staff to tell about client activity on a ticket of `department_id`: the
/// department's active staff (`ticket_department_staff`), or — when nobody is
/// assigned — every active staff member whose roles grant support access.
pub async fn ticket_staff_recipients(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    department_id: hos_core::ids::TicketDepartmentId,
) -> DbResult<Vec<super::identity::UserRow>> {
    Ok(sqlx::query_as::<_, super::identity::UserRow>(
        "with dept as (
            select u.* from ticket_department_staff ds join users u on u.id = ds.user_id
            where ds.tenant_id = $1 and ds.department_id = $2
              and u.kind = 'staff' and u.status = 'active'
         )
         select * from dept
         union all
         select u.* from users u
         where u.tenant_id = $1 and u.kind = 'staff' and u.status = 'active'
           and not exists (select 1 from dept)
           and exists (
             select 1 from user_roles ur join role_permissions rp on rp.role_id = ur.role_id
             where ur.tenant_id = $1 and ur.user_id = u.id
               and rp.permission in ('*', 'support.*', 'support.view', 'support.manage'))",
    )
    .bind(tenant_id)
    .bind(department_id)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// staff API keys
// ---------------------------------------------------------------------------

/// API keys owned by staff users of the tenant (newest first).
pub async fn staff_api_keys(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<super::identity::ApiKeyRow>> {
    Ok(sqlx::query_as::<_, super::identity::ApiKeyRow>(
        "select k.* from api_keys k join users u on u.id = k.user_id
         where k.tenant_id = $1 and u.kind = 'staff'
         order by k.revoked_at is not null, k.created_at desc",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// One staff-owned API key.
pub async fn staff_api_key(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: hos_core::ids::ApiKeyId,
) -> DbResult<Option<super::identity::ApiKeyRow>> {
    Ok(sqlx::query_as::<_, super::identity::ApiKeyRow>(
        "select k.* from api_keys k join users u on u.id = k.user_id
         where k.tenant_id = $1 and k.id = $2 and u.kind = 'staff'",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}
