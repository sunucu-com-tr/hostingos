//! Queries used by the billing engine (`hos-billing`) and the job runtime
//! (`hos-jobs`) that the core repositories do not cover: payment attempts,
//! stored payment methods, plan changes, recurring promotions, clock-driven
//! reminder bookkeeping and a few lookups (migration 0013).
//!
//! Every function follows the crate conventions: tenant id right after the
//! executor, runtime queries only, `CoreError` results.

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::domain::ServiceStatus;
use hos_core::ids::{
    CartId, ClientId, CouponId, DomainId, IntegrationId, InvoiceId, OrderId, ProductId, ServiceId,
    TenantId, UserId,
};
use hos_core::money::{BillingCycle, Currency};
use rust_decimal::Decimal;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use super::Json;
use super::carts::CartRow;
use super::domains::DomainRow;
use super::invoices::InvoiceRow;
use super::jobs::JobRow;
use super::mail::EmailLogRow;
use super::orders::OrderRow;
use super::seo::AiGenerationRow;
use super::services::ServiceRow;
use crate::error::DbResult;
use crate::ids::{AiGenerationId, EmailLogId};

// ---------------------------------------------------------------------------
// payment attempts
// ---------------------------------------------------------------------------

/// `payment_attempts` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PaymentAttemptRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub invoice_id: InvoiceId,
    pub integration_id: Option<IntegrationId>,
    pub reference: String,
    pub gateway_ref: Option<String>,
    pub amount: Decimal,
    pub currency: Currency,
    /// `started | pending | succeeded | failed`.
    pub status: String,
    pub save_payment_method: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_payment_attempt`].
#[derive(Debug, Clone)]
pub struct NewPaymentAttempt {
    pub invoice_id: InvoiceId,
    pub integration_id: Option<IntegrationId>,
    /// Alphanumeric, 8–64 characters, globally unique.
    pub reference: String,
    pub amount: Decimal,
    pub currency: Currency,
    pub save_payment_method: bool,
}

/// Records the start of a hosted payment.
pub async fn create_payment_attempt(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    a: &NewPaymentAttempt,
) -> DbResult<PaymentAttemptRow> {
    Ok(sqlx::query_as::<_, PaymentAttemptRow>(
        "insert into payment_attempts (id, tenant_id, invoice_id, integration_id, reference, amount, currency,
                                       save_payment_method)
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning *",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(a.invoice_id)
    .bind(a.integration_id)
    .bind(&a.reference)
    .bind(a.amount)
    .bind(a.currency)
    .bind(a.save_payment_method)
    .fetch_one(db)
    .await?)
}

/// Stores the gateway's session / order reference of an attempt.
pub async fn set_attempt_gateway_ref(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    gateway_ref: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update payment_attempts set gateway_ref = $3 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(gateway_ref)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Sets an attempt's status (`started | pending | succeeded | failed`).
pub async fn set_attempt_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    status: &str,
) -> DbResult<bool> {
    let res =
        sqlx::query("update payment_attempts set status = $3 where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .bind(status)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Attempt by its merchant reference.
pub async fn attempt_by_reference(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    reference: &str,
) -> DbResult<Option<PaymentAttemptRow>> {
    Ok(sqlx::query_as::<_, PaymentAttemptRow>(
        "select * from payment_attempts where tenant_id = $1 and reference = $2",
    )
    .bind(tenant_id)
    .bind(reference)
    .fetch_optional(db)
    .await?)
}

/// Latest attempt of an integration with this gateway reference.
pub async fn attempt_by_gateway_ref(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    gateway_ref: &str,
) -> DbResult<Option<PaymentAttemptRow>> {
    Ok(sqlx::query_as::<_, PaymentAttemptRow>(
        "select * from payment_attempts
         where tenant_id = $1 and integration_id = $2 and gateway_ref = $3
         order by created_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .bind(gateway_ref)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// stored payment methods
// ---------------------------------------------------------------------------

/// `payment_methods` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PaymentMethodRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub integration_id: IntegrationId,
    pub token_enc: Vec<u8>,
    pub token_hash: String,
    pub label: Option<String>,
    pub is_default: bool,
    pub last_used_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`save_payment_method`]. The caller generates `id` (it is part of
/// the encryption AAD).
#[derive(Debug, Clone)]
pub struct NewPaymentMethod {
    pub id: Uuid,
    pub client_id: ClientId,
    pub integration_id: IntegrationId,
    pub token_enc: Vec<u8>,
    pub token_hash: String,
    pub label: Option<String>,
}

/// Stores a payment method and makes it the client's default; an already
/// stored token (same hash) of the same client is only re-marked as default.
/// A token stored for another client is never moved: `None`, nothing changes.
pub async fn save_payment_method(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    m: &NewPaymentMethod,
) -> DbResult<Option<PaymentMethodRow>> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query(
        "update payment_methods set is_default = false where tenant_id = $1 and client_id = $2",
    )
    .bind(tenant_id)
    .bind(m.client_id)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query_as::<_, PaymentMethodRow>(
        "insert into payment_methods (id, tenant_id, client_id, integration_id, token_enc, token_hash, label, is_default)
         values ($1, $2, $3, $4, $5, $6, $7, true)
         on conflict (tenant_id, integration_id, token_hash) do update
            set is_default = true, last_error = null
          where payment_methods.client_id = excluded.client_id
         returning *",
    )
    .bind(m.id)
    .bind(tenant_id)
    .bind(m.client_id)
    .bind(m.integration_id)
    .bind(&m.token_enc)
    .bind(&m.token_hash)
    .bind(&m.label)
    .fetch_optional(&mut *tx)
    .await?;
    // Another client's token: the default flags stay as they were.
    if row.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(row)
}

/// The client's default stored payment method, if any.
pub async fn default_payment_method(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Option<PaymentMethodRow>> {
    Ok(sqlx::query_as::<_, PaymentMethodRow>(
        "select * from payment_methods where tenant_id = $1 and client_id = $2
         order by is_default desc, created_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_optional(db)
    .await?)
}

/// Stored payment methods of a client (default first).
pub async fn list_payment_methods(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<PaymentMethodRow>> {
    Ok(sqlx::query_as::<_, PaymentMethodRow>(
        "select * from payment_methods where tenant_id = $1 and client_id = $2
         order by is_default desc, created_at desc",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// Removes a stored payment method of a client.
pub async fn delete_payment_method(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    id: Uuid,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from payment_methods where tenant_id = $1 and client_id = $2 and id = $3",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Records a charge attempt with a stored method (`error = None` on success).
pub async fn record_payment_method_use(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    at: DateTime<Utc>,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update payment_methods set last_used_at = $3, last_error = $4 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(at)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Open invoices due on or before `as_of` whose client has a stored payment
/// method (automatic renewal charges), oldest first.
pub async fn invoices_for_autocharge(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    as_of: NaiveDate,
    limit: i64,
) -> DbResult<Vec<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select i.* from invoices i
         where i.tenant_id = $1 and i.status in ('unpaid', 'collections') and i.balance > 0 and i.due_date <= $2
           and exists (select 1 from payment_methods m where m.tenant_id = i.tenant_id and m.client_id = i.client_id)
           and not exists (select 1 from mass_payments mp where mp.tenant_id = i.tenant_id and mp.invoice_id = i.id)
         order by i.due_date, i.id limit $3",
    )
    .bind(tenant_id)
    .bind(as_of)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// plan changes / promotions
// ---------------------------------------------------------------------------

/// `service_upgrades` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ServiceUpgradeRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub service_id: ServiceId,
    pub invoice_id: Option<InvoiceId>,
    pub from_product_id: ProductId,
    pub to_product_id: ProductId,
    pub from_cycle: BillingCycle,
    pub to_cycle: BillingCycle,
    pub currency: Currency,
    pub amount: Decimal,
    pub new_recurring_amount: Decimal,
    pub config_selections: Json,
    /// `pending | applied | cancelled | failed`.
    pub status: String,
    pub error: Option<String>,
    pub applied_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_service_upgrade`].
#[derive(Debug, Clone)]
pub struct NewServiceUpgrade {
    pub service_id: ServiceId,
    pub invoice_id: Option<InvoiceId>,
    pub from_product_id: ProductId,
    pub to_product_id: ProductId,
    pub from_cycle: BillingCycle,
    pub to_cycle: BillingCycle,
    pub currency: Currency,
    pub amount: Decimal,
    pub new_recurring_amount: Decimal,
    pub config_selections: Json,
}

/// Records a plan change. A service has at most one `pending` plan change
/// (unique index): a second one fails with `Conflict`.
pub async fn create_service_upgrade(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    u: &NewServiceUpgrade,
) -> DbResult<ServiceUpgradeRow> {
    sqlx::query_as::<_, ServiceUpgradeRow>(
        "insert into service_upgrades (id, tenant_id, service_id, invoice_id, from_product_id, to_product_id,
                                       from_cycle, to_cycle, currency, amount, new_recurring_amount, config_selections)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) returning *",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(u.service_id)
    .bind(u.invoice_id)
    .bind(u.from_product_id)
    .bind(u.to_product_id)
    .bind(u.from_cycle)
    .bind(u.to_cycle)
    .bind(u.currency)
    .bind(u.amount)
    .bind(u.new_recurring_amount)
    .bind(&u.config_selections)
    .fetch_one(db)
    .await
    .map_err(|e| match crate::error::unique_violation(&e) {
        Some(_) => hos_core::CoreError::Conflict(
            "a plan change for this service is already pending".into(),
        ),
        None => hos_core::CoreError::from(e),
    })
}

/// The pending plan change of a service, if any.
pub async fn pending_upgrade_for_service(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<Option<ServiceUpgradeRow>> {
    Ok(sqlx::query_as::<_, ServiceUpgradeRow>(
        "select * from service_upgrades where tenant_id = $1 and service_id = $2 and status = 'pending'
         order by created_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_optional(db)
    .await?)
}

/// Pending plan changes billed by an invoice.
pub async fn pending_upgrades_for_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
) -> DbResult<Vec<ServiceUpgradeRow>> {
    Ok(sqlx::query_as::<_, ServiceUpgradeRow>(
        "select * from service_upgrades where tenant_id = $1 and invoice_id = $2 and status = 'pending'
         order by created_at",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_all(db)
    .await?)
}

/// Plan change by id.
pub async fn get_service_upgrade(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
) -> DbResult<Option<ServiceUpgradeRow>> {
    Ok(sqlx::query_as::<_, ServiceUpgradeRow>(
        "select * from service_upgrades where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Moves a plan change out of `pending` (`applied | cancelled | failed`).
pub async fn finish_service_upgrade(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    status: &str,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update service_upgrades set status = $3, error = $4,
                applied_at = case when $3 = 'applied' then now() else applied_at end
         where tenant_id = $1 and id = $2 and status = 'pending'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Switches a service to another product / billing cycle and price.
pub async fn change_service_plan(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    product_id: ProductId,
    billing_cycle: BillingCycle,
    recurring_amount: Decimal,
    config_selections: Option<&Json>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set product_id = $3, billing_cycle = $4, recurring_amount = $5,
                list_recurring_amount = null, promo_coupon_id = null, promo_cycles_left = null,
                config_selections = coalesce($6, config_selections)
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(product_id)
    .bind(billing_cycle)
    .bind(recurring_amount)
    .bind(config_selections)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Attaches a recurring promotion: `recurring_amount` is the discounted price
/// for `cycles_left` renewals (`None` = forever), then `list_recurring_amount`.
pub async fn set_service_promo(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    coupon_id: Option<CouponId>,
    cycles_left: Option<i32>,
    list_recurring_amount: Option<Decimal>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set promo_coupon_id = $3, promo_cycles_left = $4, list_recurring_amount = $5
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(coupon_id)
    .bind(cycles_left)
    .bind(list_recurring_amount)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Uses one discounted renewal of a limited promotion; when none are left the
/// recurring price reverts to the list price. No-op without a limited promo.
pub async fn consume_promo_cycle(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set
                promo_cycles_left = case when promo_cycles_left <= 1 then null else promo_cycles_left - 1 end,
                recurring_amount = case when promo_cycles_left <= 1
                                        then coalesce(list_recurring_amount, recurring_amount)
                                        else recurring_amount end,
                promo_coupon_id = case when promo_cycles_left <= 1 then null else promo_coupon_id end,
                list_recurring_amount = case when promo_cycles_left <= 1 then null else list_recurring_amount end
         where tenant_id = $1 and id = $2 and promo_cycles_left is not null and promo_cycles_left > 0",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// `(promo_cycles_left, list_recurring_amount)` of a service.
pub async fn service_promo(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
) -> DbResult<Option<(Option<i32>, Option<Decimal>)>> {
    Ok(sqlx::query_as::<_, (Option<i32>, Option<Decimal>)>(
        "select promo_cycles_left, list_recurring_amount from services where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// services / orders
// ---------------------------------------------------------------------------

/// Services created by an order (oldest first).
pub async fn order_services(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    order_id: OrderId,
) -> DbResult<Vec<ServiceRow>> {
    Ok(sqlx::query_as::<_, ServiceRow>(
        "select * from services where tenant_id = $1 and order_id = $2 order by created_at, id",
    )
    .bind(tenant_id)
    .bind(order_id)
    .fetch_all(db)
    .await?)
}

/// Domains created by an order (oldest first).
pub async fn order_domains(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    order_id: OrderId,
) -> DbResult<Vec<DomainRow>> {
    Ok(sqlx::query_as::<_, DomainRow>(
        "select * from domains where tenant_id = $1 and order_id = $2 order by created_at, id",
    )
    .bind(tenant_id)
    .bind(order_id)
    .fetch_all(db)
    .await?)
}

/// The order whose invoice is `invoice_id`, if any.
pub async fn order_for_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
) -> DbResult<Option<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(
        "select * from orders where tenant_id = $1 and invoice_id = $2 order by created_at limit 1",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_optional(db)
    .await?)
}

/// Locks a service row (`FOR UPDATE`) — serialises provisioning / lifecycle
/// actions on one service. Call inside a transaction.
pub async fn lock_service(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: ServiceId,
) -> DbResult<Option<ServiceRow>> {
    Ok(sqlx::query_as::<_, ServiceRow>(
        "select * from services where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Sets the hosting domain / username chosen before provisioning.
pub async fn set_service_identity(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    domain: Option<&str>,
    username: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update services set domain = coalesce($3, domain), username = coalesce($4, username)
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(domain)
    .bind(username)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Open invoices (unpaid / payment pending / collections) with a line for the service.
pub async fn open_invoices_for_service(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<Vec<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select i.* from invoices i
         where i.tenant_id = $1 and i.status in ('unpaid', 'payment_pending', 'collections')
           and exists (select 1 from invoice_items ii where ii.invoice_id = i.id and ii.service_id = $2)
         order by i.due_date, i.id",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_all(db)
    .await?)
}

/// Services (any status in `statuses`) of a tenant that use an integration —
/// usage synchronisation.
pub async fn services_with_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    statuses: &[ServiceStatus],
    limit: i64,
) -> DbResult<Vec<ServiceRow>> {
    Ok(sqlx::query_as::<_, ServiceRow>(
        "select * from services
         where tenant_id = $1 and status = any($2) and integration_id is not null and external_id is not null
         order by id limit $3",
    )
    .bind(tenant_id)
    .bind(statuses)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// clients / users / carts
// ---------------------------------------------------------------------------

/// Whether the client has any order that was not cancelled / fraud.
pub async fn client_has_orders(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists (select 1 from orders where tenant_id = $1 and client_id = $2
                          and status not in ('cancelled', 'fraud'))",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_one(db)
    .await?)
}

/// Whether the client already redeemed the coupon (an order that was not cancelled).
pub async fn client_used_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    coupon_id: CouponId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists (select 1 from orders where tenant_id = $1 and client_id = $2 and coupon_id = $3
                          and status not in ('cancelled', 'fraud'))",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(coupon_id)
    .fetch_one(db)
    .await?)
}

/// Whether the client has a live (not terminated / cancelled / failed) service of the product.
pub async fn client_has_product(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    product_id: ProductId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists (select 1 from services where tenant_id = $1 and client_id = $2 and product_id = $3
                          and status not in ('terminated', 'cancelled', 'failed'))",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(product_id)
    .fetch_one(db)
    .await?)
}

/// A user linked to a client (notification recipient).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientRecipient {
    pub user_id: UserId,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub locale: hos_core::domain::Locale,
    pub is_owner: bool,
}

/// Active users of a client (owner first).
pub async fn client_recipients(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<ClientRecipient>> {
    Ok(sqlx::query_as::<_, ClientRecipient>(
        "select u.id as user_id, u.email::text as email, u.first_name, u.last_name, u.locale, cu.is_owner
         from client_users cu join users u on u.id = cu.user_id
         where cu.tenant_id = $1 and cu.client_id = $2 and u.status = 'active'
         order by cu.is_owner desc, u.created_at",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// Locks a cart (`FOR UPDATE`). Call inside a transaction.
pub async fn lock_cart(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    cart_id: CartId,
) -> DbResult<Option<CartRow>> {
    Ok(sqlx::query_as::<_, CartRow>(
        "select * from carts where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(cart_id)
    .fetch_optional(&mut *conn)
    .await?)
}

// ---------------------------------------------------------------------------
// invoices (clock-driven bookkeeping)
// ---------------------------------------------------------------------------

/// Counts a reminder sent at `at` (the billing clock, not `now()`).
pub async fn record_reminder_at(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
    at: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update invoices set reminder_count = reminder_count + 1, last_reminder_at = $3
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(at)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Marks the late fee of an invoice as applied (once).
pub async fn mark_late_fee_applied(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
    at: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update invoices set late_fee_applied_at = $3
         where tenant_id = $1 and id = $2 and late_fee_applied_at is null",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(at)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Reverses `amount` of an invoice's payments (chargeback): lowers
/// `amount_paid` and reopens a paid / partially refunded invoice as `unpaid`.
pub async fn reverse_invoice_payment(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: InvoiceId,
    amount: Decimal,
) -> DbResult<Option<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "update invoices set amount_paid = greatest(amount_paid - $3, 0),
                status = case when status in ('paid', 'partially_refunded') then 'unpaid' else status end,
                paid_at = case when status in ('paid', 'partially_refunded') then null else paid_at end
         where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(amount)
    .fetch_optional(db)
    .await?)
}

/// Locks an invoice row (`FOR UPDATE`). Call inside a transaction.
pub async fn lock_invoice_row(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: InvoiceId,
) -> DbResult<Option<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select * from invoices where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

// ---------------------------------------------------------------------------
// domains
// ---------------------------------------------------------------------------

/// Active / grace domains expiring within `[from, to]`.
pub async fn domains_expiring_between(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DbResult<Vec<DomainRow>> {
    Ok(sqlx::query_as::<_, DomainRow>(
        "select * from domains
         where tenant_id = $1 and status in ('active', 'grace') and expires_at between $2 and $3
         order by expires_at, id",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?)
}

/// Sets a domain's renewal dates.
pub async fn set_domain_due_dates(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    next_due_date: Option<NaiveDate>,
    next_invoice_date: Option<NaiveDate>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update domains set next_due_date = $3, next_invoice_date = $4 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(next_due_date)
    .bind(next_invoice_date)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Sets the registrar integration of a domain.
pub async fn set_domain_registrar(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    integration_id: Option<IntegrationId>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update domains set registrar_integration_id = $3 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(integration_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Sets the nameservers stored for a domain.
pub async fn set_domain_nameservers(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    nameservers: &[String],
) -> DbResult<bool> {
    let res = sqlx::query("update domains set nameservers = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(nameservers)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Domains handled by a registrar that should be synchronised (oldest sync first).
pub async fn domains_to_sync(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<DomainRow>> {
    Ok(sqlx::query_as::<_, DomainRow>(
        "select * from domains
         where tenant_id = $1 and registrar_integration_id is not null
           and status in ('pending_registration', 'pending_transfer', 'active', 'grace', 'redemption')
         order by last_synced_at nulls first, id limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// e-mail / AI / jobs
// ---------------------------------------------------------------------------

/// An `email_log` row of a tenant.
pub async fn get_email_log(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: EmailLogId,
) -> DbResult<Option<EmailLogRow>> {
    let sql = format!(
        "select {} from email_log where tenant_id = $1 and id = $2",
        super::mail::EMAIL_LOG_COLUMNS
    );
    Ok(sqlx::query_as::<_, EmailLogRow>(&sql)
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(db)
        .await?)
}

/// An `ai_generations` row of a tenant.
pub async fn get_ai_generation(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AiGenerationId,
) -> DbResult<Option<AiGenerationRow>> {
    Ok(sqlx::query_as::<_, AiGenerationRow>(
        "select * from ai_generations where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// `pg_notify(channel, payload)` — delivered when the surrounding transaction
/// commits. Wakes `LISTEN`ing job workers. Safe through PgBouncer.
pub async fn notify_channel(db: impl PgExecutor<'_>, channel: &str, payload: &str) -> DbResult<()> {
    sqlx::query("select pg_notify($1, $2)")
        .bind(channel)
        .bind(payload)
        .execute(db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// automation markers
// ---------------------------------------------------------------------------

/// Records `key` once per tenant; `true` the first time (the caller should
/// act), `false` when it was already marked.
pub async fn mark_once(db: impl PgExecutor<'_>, tenant_id: TenantId, key: &str) -> DbResult<bool> {
    let res = sqlx::query(
        "insert into automation_marks (tenant_id, key) values ($1, $2) on conflict do nothing",
    )
    .bind(tenant_id)
    .bind(key)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Deletes markers older than `age` (all tenants). Returns the number removed.
pub async fn purge_automation_marks(
    db: impl PgExecutor<'_>,
    age: std::time::Duration,
) -> DbResult<u64> {
    let res = sqlx::query(
        "delete from automation_marks where created_at < now() - make_interval(secs => $1)",
    )
    .bind(age.as_secs_f64())
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Claims up to `limit` due pending jobs **of one tenant** (any queue) for
/// `worker_id` — draining a tenant's queue on demand (admin "run now",
/// tests) without touching other tenants' jobs.
pub async fn claim_tenant_jobs(
    db: impl PgExecutor<'_>,
    worker_id: &str,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<JobRow>> {
    let mut jobs = sqlx::query_as::<_, JobRow>(
        "update jobs j
            set status = 'running', locked_by = $1, locked_at = now(), attempts = j.attempts + 1
           from (select id from jobs
                  where status = 'pending' and tenant_id = $2 and run_at <= now()
                  order by priority desc, run_at, id
                  limit $3
                  for update skip locked) c
          where j.id = c.id
         returning j.*",
    )
    .bind(worker_id)
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    jobs.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.run_at.cmp(&b.run_at))
            .then(a.id.cmp(&b.id))
    });
    Ok(jobs)
}

// ---------------------------------------------------------------------------
// card setups without payment (`payment_setups`, migration 0043)
// ---------------------------------------------------------------------------

/// `payment_setups` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct PaymentSetupRow {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub integration_id: IntegrationId,
    pub reference: String,
    pub gateway_ref: Option<String>,
    /// `pending | succeeded | failed | expired`.
    pub status: String,
    pub created_by: Option<UserId>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Records a card setup hand-off.
pub async fn create_payment_setup(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    integration_id: IntegrationId,
    reference: &str,
    created_by: Option<UserId>,
) -> DbResult<PaymentSetupRow> {
    Ok(sqlx::query_as::<_, PaymentSetupRow>(
        "insert into payment_setups (id, tenant_id, client_id, integration_id, reference, created_by)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(client_id)
    .bind(integration_id)
    .bind(reference)
    .bind(created_by)
    .fetch_one(db)
    .await?)
}

/// Stores the gateway session of a setup.
pub async fn set_payment_setup_gateway_ref(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    gateway_ref: Option<&str>,
) -> DbResult<()> {
    sqlx::query("update payment_setups set gateway_ref = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(gateway_ref)
        .execute(db)
        .await?;
    Ok(())
}

/// A setup by reference, locked.
pub async fn lock_payment_setup(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    reference: &str,
) -> DbResult<Option<PaymentSetupRow>> {
    Ok(sqlx::query_as::<_, PaymentSetupRow>(
        "select * from payment_setups where tenant_id = $1 and reference = $2 for update",
    )
    .bind(tenant_id)
    .bind(reference)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Moves a pending setup to `status` (`succeeded | failed | expired`):
/// `true` when this call did it.
pub async fn finish_payment_setup(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Uuid,
    status: &str,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update payment_setups set status = $3, completed_at = now()
          where tenant_id = $1 and id = $2 and status = 'pending'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}
