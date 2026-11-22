//! Provisioned services, service addons, the service event log, VMs and IP
//! address management, plus the billing-cycle queries used by the cron jobs
//! (renewal invoices, suspension, termination).

use std::net::IpAddr;

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::domain::{ProductType, ServiceStatus};
use hos_core::ids::{
    ClientId, IntegrationId, IpAddressId, IpPoolId, JobId, OrderId, ProductAddonId, ProductId,
    ServiceAddonId, ServiceId, TenantId, VmId,
};
use hos_core::integrations::virt::VmStatus;
use hos_core::money::{BillingCycle, Currency};
use ipnet::IpNet;
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};
use uuid::Uuid;

use super::{Json, empty_object};
use crate::enums::{ActorKind, CancellationType, EventLevel, IpStatus};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::ids::ServiceEventId;
use crate::page::{Page, Paged, fetch_paged};

/// `services` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ServiceRow {
    pub id: ServiceId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub product_id: ProductId,
    pub order_id: Option<OrderId>,
    pub status: ServiceStatus,
    pub billing_cycle: BillingCycle,
    pub first_payment_amount: Decimal,
    pub recurring_amount: Decimal,
    pub currency: Currency,
    pub quantity: i32,
    pub registration_date: NaiveDate,
    pub next_due_date: Option<NaiveDate>,
    pub next_invoice_date: Option<NaiveDate>,
    pub anchor_day: Option<i16>,
    pub integration_id: Option<IntegrationId>,
    pub external_id: Option<String>,
    pub username: Option<String>,
    pub password_enc: Option<Vec<u8>>,
    pub domain: Option<String>,
    pub server_hostname: Option<String>,
    pub dedicated_ip: Option<IpAddr>,
    pub assigned_ips: Vec<IpAddr>,
    pub module_data: Json,
    pub config_selections: Json,
    pub custom_fields: Json,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub override_suspend_until: Option<NaiveDate>,
    pub suspended_at: Option<DateTime<Utc>>,
    pub suspended_reason: Option<String>,
    pub terminated_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub cancellation_requested_at: Option<DateTime<Utc>>,
    pub cancellation_type: Option<CancellationType>,
    pub cancellation_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_service`] (status starts at `pending`).
#[derive(Debug, Clone)]
pub struct NewService {
    pub client_id: ClientId,
    pub product_id: ProductId,
    pub order_id: Option<OrderId>,
    pub billing_cycle: BillingCycle,
    pub first_payment_amount: Decimal,
    pub recurring_amount: Decimal,
    pub currency: Currency,
    pub quantity: i32,
    pub registration_date: NaiveDate,
    pub next_due_date: Option<NaiveDate>,
    pub next_invoice_date: Option<NaiveDate>,
    pub integration_id: Option<IntegrationId>,
    pub domain: Option<String>,
    pub username: Option<String>,
    pub config_selections: Json,
    pub custom_fields: Json,
}

/// Creates a service in `pending`. `anchor_day` = day of `registration_date`.
pub async fn create_service(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    s: &NewService,
) -> DbResult<ServiceRow> {
    Ok(sqlx::query_as::<_, ServiceRow>(
        "insert into services (id, tenant_id, client_id, product_id, order_id, billing_cycle, first_payment_amount,
                               recurring_amount, currency, quantity, registration_date, next_due_date,
                               next_invoice_date, anchor_day, integration_id, domain, username, config_selections,
                               custom_fields)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, extract(day from $11::date)::smallint,
                 $14, $15, $16, $17, $18)
         returning *",
    )
    .bind(ServiceId::new())
    .bind(tenant_id)
    .bind(s.client_id)
    .bind(s.product_id)
    .bind(s.order_id)
    .bind(s.billing_cycle)
    .bind(s.first_payment_amount)
    .bind(s.recurring_amount)
    .bind(s.currency)
    .bind(s.quantity)
    .bind(s.registration_date)
    .bind(s.next_due_date)
    .bind(s.next_invoice_date)
    .bind(s.integration_id)
    .bind(&s.domain)
    .bind(&s.username)
    .bind(&s.config_selections)
    .bind(&s.custom_fields)
    .fetch_one(db)
    .await?)
}

/// Service by id.
pub async fn get_service(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
) -> DbResult<Option<ServiceRow>> {
    Ok(
        sqlx::query_as::<_, ServiceRow>("select * from services where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Service with product / client info for lists.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ServiceListRow {
    #[sqlx(flatten)]
    pub service: ServiceRow,
    pub product_name: String,
    pub product_type: ProductType,
    pub client_name: String,
}

/// Filters of [`list_services`].
#[derive(Debug, Clone, Default)]
pub struct ServiceFilter {
    pub client_id: Option<ClientId>,
    pub status: Option<ServiceStatus>,
    pub product_id: Option<ProductId>,
    pub product_type: Option<ProductType>,
    /// Matches domain, username, external id, dedicated IP.
    pub search: Option<String>,
}

const SERVICE_LIST_SELECT: &str = "select s.*, p.name as product_name, p.product_type,
        coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name";

/// Services of a tenant, newest first.
pub async fn list_services<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ServiceFilter,
    page: Page,
) -> DbResult<Paged<ServiceListRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        SERVICE_LIST_SELECT,
        |qb| {
            qb.push(
                "from services s join products p on p.id = s.product_id join clients c on c.id = s.client_id
                 where s.tenant_id = ",
            )
            .push_bind(tenant_id);
            if let Some(c) = f.client_id {
                qb.push(" and s.client_id = ").push_bind(c);
            }
            if let Some(st) = f.status {
                qb.push(" and s.status = ").push_bind(st);
            }
            if let Some(p) = f.product_id {
                qb.push(" and s.product_id = ").push_bind(p);
            }
            if let Some(t) = f.product_type {
                qb.push(" and p.product_type = ").push_bind(t);
            }
            if let Some(s) = &search {
                qb.push(" and (s.domain ilike ")
                    .push_bind(s.clone())
                    .push(" or s.username ilike ")
                    .push_bind(s.clone())
                    .push(" or s.external_id ilike ")
                    .push_bind(s.clone())
                    .push(" or host(s.dedicated_ip) ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        "s.created_at desc, s.id desc",
        page,
    )
    .await
}

/// All services of a client (client area), newest first.
pub async fn client_services(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<ServiceListRow>> {
    let sql = format!(
        "{SERVICE_LIST_SELECT}
         from services s join products p on p.id = s.product_id join clients c on c.id = s.client_id
         where s.tenant_id = $1 and s.client_id = $2
         order by (s.status in ('active', 'suspended', 'provisioning', 'pending')) desc, s.created_at desc"
    );
    Ok(sqlx::query_as::<_, ServiceListRow>(&sql)
        .bind(tenant_id)
        .bind(client_id)
        .fetch_all(db)
        .await?)
}

/// Moves a service `from` → `to` only if it is still in `from` (optimistic
/// concurrency; transitions `ServiceStatus::transition` forbids are rejected
/// with `CoreError::InvalidTransition`). Stamps
/// `suspended_at`/`suspended_reason` (cleared on reactivation), `terminated_at`
/// and `cancelled_at`.
pub async fn transition_service(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    from: ServiceStatus,
    to: ServiceStatus,
    reason: Option<&str>,
) -> DbResult<bool> {
    from.transition(to)?;
    let res = sqlx::query(
        "update services set status = $4,
                suspended_at = case when $4 = 'suspended' then now() when $4 = 'active' then null else suspended_at end,
                suspended_reason = case when $4 = 'suspended' then $5 when $4 = 'active' then null else suspended_reason end,
                terminated_at = case when $4 = 'terminated' then now() else terminated_at end,
                cancelled_at = case when $4 = 'cancelled' then now() else cancelled_at end
         where tenant_id = $1 and id = $2 and status = $3",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(from)
    .bind(to)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Provisioning result written back by a module job.
#[derive(Debug, Clone, Default)]
pub struct ProvisioningUpdate {
    pub integration_id: Option<IntegrationId>,
    pub external_id: Option<String>,
    pub username: Option<String>,
    pub password_enc: Option<Vec<u8>>,
    pub server_hostname: Option<String>,
    pub dedicated_ip: Option<IpAddr>,
    pub assigned_ips: Option<Vec<IpAddr>>,
    /// Merged (`||`) into `module_data`.
    pub module_data: Option<Json>,
}

/// Writes provisioning details (only the `Some` fields change).
pub async fn update_provisioning(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    u: &ProvisioningUpdate,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set integration_id = coalesce($3, integration_id), external_id = coalesce($4, external_id),
                username = coalesce($5, username), password_enc = coalesce($6, password_enc),
                server_hostname = coalesce($7, server_hostname), dedicated_ip = coalesce($8, dedicated_ip),
                assigned_ips = coalesce($9, assigned_ips), module_data = module_data || coalesce($10, '{}'::jsonb)
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(u.integration_id)
    .bind(&u.external_id)
    .bind(&u.username)
    .bind(&u.password_enc)
    .bind(&u.server_hostname)
    .bind(u.dedicated_ip)
    .bind(&u.assigned_ips)
    .bind(&u.module_data)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Moves `next_due_date` from `current` to `next` (and sets
/// `next_invoice_date`) — only if it still equals `current`, so a renewal is
/// never applied twice.
pub async fn advance_due_date(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    current: NaiveDate,
    next: NaiveDate,
    next_invoice_date: Option<NaiveDate>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set next_due_date = $4, next_invoice_date = $5
         where tenant_id = $1 and id = $2 and next_due_date = $3",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(current)
    .bind(next)
    .bind(next_invoice_date)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Records a cancellation request from the client.
pub async fn request_cancellation(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    kind: CancellationType,
    reason: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set cancellation_requested_at = now(), cancellation_type = $3, cancellation_reason = $4
         where tenant_id = $1 and id = $2 and status in ('pending', 'active', 'suspended')",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(kind)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Updates staff notes / suspension override / recurring amount.
pub async fn update_service_admin(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    recurring_amount: Decimal,
    override_suspend_until: Option<NaiveDate>,
    admin_notes: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set recurring_amount = $3, override_suspend_until = $4, admin_notes = $5
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(recurring_amount)
    .bind(override_suspend_until)
    .bind(admin_notes)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// billing-cycle queries (cron)
// ---------------------------------------------------------------------------

/// Billable services (`active`/`suspended`, recurring) whose renewal invoice is
/// due: `coalesce(next_invoice_date, next_due_date - days_before) <= as_of`
/// and no non-cancelled invoice line exists yet for the period starting at
/// `next_due_date`. Oldest due date first.
pub async fn services_due_for_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    as_of: NaiveDate,
    days_before: i32,
    limit: i64,
) -> DbResult<Vec<ServiceRow>> {
    Ok(sqlx::query_as::<_, ServiceRow>(
        "select s.* from services s
         where s.tenant_id = $1 and s.status in ('active', 'suspended') and s.billing_cycle <> 'one_time'
           and s.next_due_date is not null
           and coalesce(s.next_invoice_date, s.next_due_date - $3) <= $2
           and not (s.cancellation_type = 'end_of_period' and s.cancellation_requested_at is not null)
           and not exists (
               select 1 from invoice_items ii join invoices i on i.id = ii.invoice_id
               where ii.service_id = s.id and ii.period_start = s.next_due_date and i.status <> 'cancelled')
         order by s.next_due_date, s.id
         limit $4",
    )
    .bind(tenant_id)
    .bind(as_of)
    .bind(days_before)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// A service to suspend / terminate and the oldest overdue invoice behind it.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct OverdueService {
    #[sqlx(flatten)]
    pub service: ServiceRow,
    pub overdue_invoice_id: Option<hos_core::ids::InvoiceId>,
    pub overdue_since: Option<NaiveDate>,
}

/// Active services with an open invoice line whose invoice is more than
/// `grace_days` past due on `as_of` (respecting `override_suspend_until`).
pub async fn services_to_suspend(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    as_of: NaiveDate,
    grace_days: i32,
    limit: i64,
) -> DbResult<Vec<OverdueService>> {
    Ok(sqlx::query_as::<_, OverdueService>(
        "select s.*, o.id as overdue_invoice_id, o.due_date as overdue_since
         from services s
         join lateral (
             select i.id, i.due_date from invoice_items ii join invoices i on i.id = ii.invoice_id
             where ii.tenant_id = s.tenant_id and ii.service_id = s.id and i.status in ('unpaid', 'payment_pending', 'collections')
               and i.due_date + $3 < $2
             order by i.due_date limit 1
         ) o on true
         where s.tenant_id = $1 and s.status = 'active'
           and (s.override_suspend_until is null or s.override_suspend_until < $2)
         order by o.due_date, s.id
         limit $4",
    )
    .bind(tenant_id)
    .bind(as_of)
    .bind(grace_days)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Services to terminate on `as_of`: suspended ones whose overdue invoice is
/// more than `days_after_due` old, plus end-of-period cancellations whose paid
/// period has ended.
pub async fn services_to_terminate(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    as_of: NaiveDate,
    days_after_due: i32,
    limit: i64,
) -> DbResult<Vec<OverdueService>> {
    Ok(sqlx::query_as::<_, OverdueService>(
        "select s.*, o.id as overdue_invoice_id, o.due_date as overdue_since
         from services s
         left join lateral (
             select i.id, i.due_date from invoice_items ii join invoices i on i.id = ii.invoice_id
             where ii.tenant_id = s.tenant_id and ii.service_id = s.id and i.status in ('unpaid', 'payment_pending', 'collections')
             order by i.due_date limit 1
         ) o on true
         where s.tenant_id = $1
           and (
               (s.status = 'suspended' and o.due_date is not null and o.due_date + $3 < $2)
               or (s.status in ('active', 'suspended') and s.cancellation_type = 'end_of_period'
                   and s.cancellation_requested_at is not null and s.next_due_date is not null
                   and s.next_due_date <= $2)
               or (s.status in ('active', 'suspended') and s.cancellation_type = 'immediate'
                   and s.cancellation_requested_at is not null)
           )
         order by s.next_due_date nulls first, s.id
         limit $4",
    )
    .bind(tenant_id)
    .bind(as_of)
    .bind(days_after_due)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Pending services whose order invoice has been paid (auto-setup on payment).
pub async fn paid_pending_services(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<ServiceRow>> {
    Ok(sqlx::query_as::<_, ServiceRow>(
        "select s.* from services s
         join orders o on o.id = s.order_id
         join invoices i on i.id = o.invoice_id
         where s.tenant_id = $1 and s.status = 'pending' and i.status = 'paid'
         order by s.created_at limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// service addons
// ---------------------------------------------------------------------------

/// `service_addons` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ServiceAddonRow {
    pub id: ServiceAddonId,
    pub tenant_id: TenantId,
    pub service_id: ServiceId,
    pub addon_id: ProductAddonId,
    pub order_id: Option<OrderId>,
    pub status: ServiceStatus,
    pub billing_cycle: BillingCycle,
    pub setup_fee: Decimal,
    pub recurring_amount: Decimal,
    pub currency: Currency,
    pub registration_date: NaiveDate,
    pub next_due_date: Option<NaiveDate>,
    pub next_invoice_date: Option<NaiveDate>,
    pub module_data: Json,
    pub notes: Option<String>,
    pub suspended_at: Option<DateTime<Utc>>,
    pub terminated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Attaches an addon to a service (status `pending`).
#[allow(clippy::too_many_arguments)]
pub async fn create_service_addon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    addon_id: ProductAddonId,
    order_id: Option<OrderId>,
    billing_cycle: BillingCycle,
    setup_fee: Decimal,
    recurring_amount: Decimal,
    currency: Currency,
    next_due_date: Option<NaiveDate>,
) -> DbResult<ServiceAddonRow> {
    Ok(sqlx::query_as::<_, ServiceAddonRow>(
        "insert into service_addons (id, tenant_id, service_id, addon_id, order_id, billing_cycle, setup_fee,
                                     recurring_amount, currency, next_due_date)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning *",
    )
    .bind(ServiceAddonId::new())
    .bind(tenant_id)
    .bind(service_id)
    .bind(addon_id)
    .bind(order_id)
    .bind(billing_cycle)
    .bind(setup_fee)
    .bind(recurring_amount)
    .bind(currency)
    .bind(next_due_date)
    .fetch_one(db)
    .await?)
}

/// Addons of a service.
pub async fn service_addons(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<Vec<ServiceAddonRow>> {
    Ok(sqlx::query_as::<_, ServiceAddonRow>(
        "select * from service_addons where tenant_id = $1 and service_id = $2 order by created_at",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_all(db)
    .await?)
}

/// Addons created by an order.
pub async fn order_service_addons(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    order_id: OrderId,
) -> DbResult<Vec<ServiceAddonRow>> {
    Ok(sqlx::query_as::<_, ServiceAddonRow>(
        "select * from service_addons where tenant_id = $1 and order_id = $2 order by created_at",
    )
    .bind(tenant_id)
    .bind(order_id)
    .fetch_all(db)
    .await?)
}

/// Sets a service addon's status.
pub async fn set_service_addon_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceAddonId,
    status: ServiceStatus,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update service_addons set status = $3,
                suspended_at = case when $3 = 'suspended' then now() when $3 = 'active' then null else suspended_at end,
                terminated_at = case when $3 = 'terminated' then now() else terminated_at end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// service events
// ---------------------------------------------------------------------------

/// `service_events` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ServiceEventRow {
    pub id: ServiceEventId,
    pub tenant_id: TenantId,
    pub service_id: ServiceId,
    pub event: String,
    pub level: EventLevel,
    pub message: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub job_id: Option<JobId>,
    pub actor_kind: Option<ActorKind>,
    pub actor_id: Option<Uuid>,
    pub data: Json,
    pub created_at: DateTime<Utc>,
}

/// An event to append to a service's log.
#[derive(Debug, Clone)]
pub struct NewServiceEvent {
    pub event: String,
    pub level: EventLevel,
    pub message: String,
    pub from_status: Option<ServiceStatus>,
    pub to_status: Option<ServiceStatus>,
    pub job_id: Option<JobId>,
    pub actor_kind: Option<ActorKind>,
    pub actor_id: Option<Uuid>,
    pub data: Json,
}

impl NewServiceEvent {
    /// Info-level event without actor.
    pub fn info(event: &str, message: &str) -> Self {
        Self {
            event: event.to_owned(),
            level: EventLevel::Info,
            message: message.to_owned(),
            from_status: None,
            to_status: None,
            job_id: None,
            actor_kind: None,
            actor_id: None,
            data: empty_object(),
        }
    }

    /// Error-level event.
    pub fn error(event: &str, message: &str) -> Self {
        Self {
            level: EventLevel::Error,
            ..Self::info(event, message)
        }
    }
}

/// Appends to the service log.
pub async fn log_event(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    e: &NewServiceEvent,
) -> DbResult<ServiceEventId> {
    let id = ServiceEventId::new();
    sqlx::query(
        "insert into service_events (id, tenant_id, service_id, event, level, message, from_status, to_status,
                                     job_id, actor_kind, actor_id, data)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(service_id)
    .bind(&e.event)
    .bind(e.level)
    .bind(&e.message)
    .bind(e.from_status.map(|s| s.as_str()))
    .bind(e.to_status.map(|s| s.as_str()))
    .bind(e.job_id)
    .bind(e.actor_kind)
    .bind(e.actor_id)
    .bind(&e.data)
    .execute(db)
    .await?;
    Ok(id)
}

/// Latest events of a service.
pub async fn service_events(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
    limit: i64,
) -> DbResult<Vec<ServiceEventRow>> {
    Ok(sqlx::query_as::<_, ServiceEventRow>(
        "select * from service_events where tenant_id = $1 and service_id = $2
         order by created_at desc, id desc limit $3",
    )
    .bind(tenant_id)
    .bind(service_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// VMs
// ---------------------------------------------------------------------------

/// `vms` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct VmRow {
    pub id: VmId,
    pub tenant_id: TenantId,
    pub service_id: Option<ServiceId>,
    pub integration_id: IntegrationId,
    pub node: Option<String>,
    pub external_id: String,
    pub name: String,
    pub hostname: Option<String>,
    pub os_template: Option<String>,
    pub cpu_cores: Option<i32>,
    pub memory_mb: Option<i32>,
    pub disk_gb: Option<i32>,
    pub bandwidth_gb: Option<i32>,
    pub status: VmStatus,
    pub primary_ipv4: Option<IpAddr>,
    pub primary_ipv6: Option<IpAddr>,
    pub root_password_enc: Option<Vec<u8>>,
    pub metadata: Json,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// VM data from a provider (create or sync).
#[derive(Debug, Clone)]
pub struct VmInput {
    pub service_id: Option<ServiceId>,
    pub integration_id: IntegrationId,
    pub node: Option<String>,
    pub external_id: String,
    pub name: String,
    pub hostname: Option<String>,
    pub os_template: Option<String>,
    pub cpu_cores: Option<i32>,
    pub memory_mb: Option<i32>,
    pub disk_gb: Option<i32>,
    pub status: VmStatus,
    pub primary_ipv4: Option<IpAddr>,
    pub primary_ipv6: Option<IpAddr>,
    pub metadata: Json,
}

/// Inserts or updates a VM keyed by `(integration_id, external_id)`.
///
/// A record is only ever updated for the service it belongs to. Providers
/// reuse ids (a Proxmox vmid is handed out again once its VM is gone), so a
/// stored record of the same id that belongs to another service, or whose
/// machine was deleted, is a different machine: it is retired (its id gets
/// a `#retired:` suffix, it stays deleted and with its old service, stored
/// credentials are dropped) and a fresh record is created instead of the new
/// service adopting the old one's password, metadata or history.
pub async fn upsert_vm<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    v: &VmInput,
) -> DbResult<VmRow> {
    let mut tx = db.begin().await?;
    if let Some(service_id) = v.service_id {
        sqlx::query(
            "update vms set external_id = external_id || '#retired:' || id::text,
                    deleted_at = coalesce(deleted_at, now()), root_password_enc = null, updated_at = now()
              where tenant_id = $1 and integration_id = $2 and external_id = $3
                and (deleted_at is not null or service_id is distinct from $4)",
        )
        .bind(tenant_id)
        .bind(v.integration_id)
        .bind(&v.external_id)
        .bind(service_id)
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query_as::<_, VmRow>(
        "insert into vms (id, tenant_id, service_id, integration_id, node, external_id, name, hostname, os_template,
                          cpu_cores, memory_mb, disk_gb, status, primary_ipv4, primary_ipv6, metadata, last_synced_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, now())
         on conflict (integration_id, external_id) do update
            set service_id = coalesce(excluded.service_id, vms.service_id), node = excluded.node,
                name = excluded.name, hostname = excluded.hostname, os_template = excluded.os_template,
                cpu_cores = excluded.cpu_cores, memory_mb = excluded.memory_mb, disk_gb = excluded.disk_gb,
                status = excluded.status, primary_ipv4 = excluded.primary_ipv4, primary_ipv6 = excluded.primary_ipv6,
                metadata = vms.metadata || excluded.metadata, last_synced_at = now(), deleted_at = null
            where vms.tenant_id = excluded.tenant_id
         returning *",
    )
    .bind(VmId::new())
    .bind(tenant_id)
    .bind(v.service_id)
    .bind(v.integration_id)
    .bind(&v.node)
    .bind(&v.external_id)
    .bind(&v.name)
    .bind(&v.hostname)
    .bind(&v.os_template)
    .bind(v.cpu_cores)
    .bind(v.memory_mb)
    .bind(v.disk_gb)
    .bind(v.status)
    .bind(v.primary_ipv4)
    .bind(v.primary_ipv6)
    .bind(&v.metadata)
    .fetch_one(&mut *tx)
    .await
    .map_err(conflict_on_unique("vm"))?;
    tx.commit().await?;
    Ok(row)
}

/// The (non-deleted) VM of a service.
pub async fn vm_for_service(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<Option<VmRow>> {
    Ok(sqlx::query_as::<_, VmRow>(
        "select * from vms where tenant_id = $1 and service_id = $2 and deleted_at is null
         order by created_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_optional(db)
    .await?)
}

/// VMs of a tenant (optionally of one integration).
pub async fn list_vms(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: Option<IntegrationId>,
) -> DbResult<Vec<VmRow>> {
    Ok(sqlx::query_as::<_, VmRow>(
        "select * from vms where tenant_id = $1 and ($2::uuid is null or integration_id = $2) and deleted_at is null
         order by name",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .fetch_all(db)
    .await?)
}

/// Updates a VM's power state.
pub async fn set_vm_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: VmId,
    status: VmStatus,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update vms set status = $3, last_synced_at = now() where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Marks a VM deleted (provider object removed) and drops its stored
/// credentials: the machine they opened is gone.
pub async fn mark_vm_deleted(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: VmId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update vms set deleted_at = now(), root_password_enc = null where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// IP pools / addresses
// ---------------------------------------------------------------------------

/// `ip_pools` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct IpPoolRow {
    pub id: IpPoolId,
    pub tenant_id: TenantId,
    pub integration_id: Option<IntegrationId>,
    pub name: String,
    pub network: IpNet,
    pub gateway: Option<IpAddr>,
    pub nameservers: Vec<IpAddr>,
    pub node: Option<String>,
    pub vlan: Option<i32>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `ip_addresses` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct IpAddressRow {
    pub id: IpAddressId,
    pub tenant_id: TenantId,
    pub pool_id: IpPoolId,
    pub address: IpAddr,
    pub status: IpStatus,
    pub service_id: Option<ServiceId>,
    pub vm_id: Option<VmId>,
    pub mac_address: Option<String>,
    pub ptr: Option<String>,
    pub notes: Option<String>,
    pub assigned_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates an IP pool.
pub async fn create_ip_pool(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    name: &str,
    network: IpNet,
    gateway: Option<IpAddr>,
    nameservers: &[IpAddr],
    integration_id: Option<IntegrationId>,
) -> DbResult<IpPoolRow> {
    sqlx::query_as::<_, IpPoolRow>(
        "insert into ip_pools (id, tenant_id, name, network, gateway, nameservers, integration_id)
         values ($1, $2, $3, $4, $5, $6, $7) returning *",
    )
    .bind(IpPoolId::new())
    .bind(tenant_id)
    .bind(name)
    .bind(network.trunc())
    .bind(gateway)
    .bind(nameservers)
    .bind(integration_id)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("ip pool"))
}

/// Adds addresses to a pool (existing ones are skipped). Returns the number added.
pub async fn add_ip_addresses(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    pool_id: IpPoolId,
    addresses: &[IpAddr],
) -> DbResult<u64> {
    let ids: Vec<IpAddressId> = addresses.iter().map(|_| IpAddressId::new()).collect();
    let res = sqlx::query(
        "insert into ip_addresses (id, tenant_id, pool_id, address)
         select u.id, $1, p.id, u.addr from unnest($3::uuid[], $4::inet[]) as u(id, addr)
         join ip_pools p on p.tenant_id = $1 and p.id = $2
         on conflict (pool_id, address) do nothing",
    )
    .bind(tenant_id)
    .bind(pool_id)
    .bind(&ids)
    .bind(addresses)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Assigns a free address of the pool to a service / VM. Concurrent callers get
/// different addresses (`FOR UPDATE SKIP LOCKED`). `None` when the pool is full.
pub async fn assign_free_ip(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    pool_id: IpPoolId,
    service_id: Option<ServiceId>,
    vm_id: Option<VmId>,
) -> DbResult<Option<IpAddressRow>> {
    Ok(sqlx::query_as::<_, IpAddressRow>(
        "update ip_addresses a set status = 'assigned', service_id = $3, vm_id = $4, assigned_at = now()
         where a.id = (select id from ip_addresses
                       where tenant_id = $1 and pool_id = $2 and status = 'free'
                       order by address limit 1 for update skip locked)
         returning a.*",
    )
    .bind(tenant_id)
    .bind(pool_id)
    .bind(service_id)
    .bind(vm_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Frees every address held by a service.
pub async fn release_service_ips(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<u64> {
    let res = sqlx::query(
        "update ip_addresses set status = 'free', service_id = null, vm_id = null, assigned_at = null
         where tenant_id = $1 and service_id = $2",
    )
    .bind(tenant_id)
    .bind(service_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Addresses assigned to a service (with their reverse DNS), primary first.
pub async fn service_ips(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<Vec<IpAddressRow>> {
    Ok(sqlx::query_as::<_, IpAddressRow>(
        "select * from ip_addresses where tenant_id = $1 and service_id = $2
         order by assigned_at nulls last, address",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_all(db)
    .await?)
}

/// Pool usage: `(pool, total, free)`.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct IpPoolUsage {
    #[sqlx(flatten)]
    pub pool: IpPoolRow,
    pub total: i64,
    pub free: i64,
}

/// Pools of a tenant with usage counters.
pub async fn ip_pool_usage(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<IpPoolUsage>> {
    Ok(sqlx::query_as::<_, IpPoolUsage>(
        "select p.*, count(a.id) as total, count(a.id) filter (where a.status = 'free') as free
         from ip_pools p left join ip_addresses a on a.pool_id = p.id
         where p.tenant_id = $1 group by p.id order by p.name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}
