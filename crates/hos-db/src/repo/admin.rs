//! Queries of the staff admin panel (`/admin`) that the per-table modules do
//! not cover: sortable lists, small CRUD updates / deletes, queues, the
//! global search and dashboard health figures. Every function is tenant
//! scoped (`tenant_id` first) and uses runtime queries only.

use chrono::{DateTime, Utc};
use hos_core::domain::{ServiceStatus, TicketStatus};
use hos_core::ids::{
    ClientGroupId, ClientId, ConfigOptionGroupId, ConfigOptionId, CouponId, IntegrationId,
    InvoiceId, OrderId, ProductAddonId, ProductGroupId, ProductId, ServiceId, TaxRuleId, TenantId,
    TldId,
};
use hos_core::money::{BillingCycle, Currency};
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgExecutor, PgPool, Postgres};
use uuid::Uuid;

use super::Json;
use super::catalog::{CouponRow, ProductAddonRow, ProductGroupInput};
use super::clients::{ClientFilter, ClientListRow};
use super::identity::UserRow;
use super::invoices::InvoiceRow;
use super::jobs::JobRow;
use super::services::{IpAddressRow, ServiceFilter, ServiceListRow};
use super::webhooks::WebhookInRow;
use crate::enums::{DiscountType, IpStatus};
use crate::error::{DbResult, like_contains};
use crate::ids::{ClientNoteId, ConfigOptionValueId};
use crate::page::{Page, Paged, fetch_paged};

fn dir(desc: bool) -> &'static str {
    if desc { "desc" } else { "asc" }
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// Clients with the same aggregates as `clients::list_clients`, sorted by an
/// allow-listed key (`name`, `email`, `company`, `created`, `services`,
/// `unpaid`; default newest first).
pub async fn list_clients<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ClientFilter,
    sort: Option<&str>,
    desc: bool,
    page: Page,
) -> DbResult<Paged<ClientListRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    let order = match sort {
        Some("name") => format!("c.first_name {0}, c.last_name {0}", dir(desc)),
        Some("email") => format!("c.email {}", dir(desc)),
        Some("company") => format!("c.company_name {} nulls last", dir(desc)),
        Some("created") => format!("c.created_at {}", dir(desc)),
        Some("services") => format!("active_services {}", dir(desc)),
        Some("unpaid") => format!("unpaid_total {}", dir(desc)),
        _ => "c.created_at desc".to_owned(),
    };
    fetch_paged(
        db,
        "select c.*,
                (select count(*) from services s
                  where s.tenant_id = c.tenant_id and s.client_id = c.id and s.status = 'active') as active_services,
                (select coalesce(sum(i.balance), 0) from invoices i
                  where i.tenant_id = c.tenant_id and i.client_id = c.id
                    and i.status in ('unpaid', 'payment_pending', 'collections')) as unpaid_total",
        |qb| {
            qb.push("from clients c where c.tenant_id = ")
                .push_bind(tenant_id);
            if let Some(s) = &search {
                qb.push(" and (c.email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or (c.first_name || ' ' || c.last_name) ilike ")
                    .push_bind(s.clone())
                    .push(" or c.company_name ilike ")
                    .push_bind(s.clone())
                    .push(" or c.phone ilike ")
                    .push_bind(s.clone())
                    .push(" or c.tax_number ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
            if let Some(st) = f.status {
                qb.push(" and c.status = ").push_bind(st);
            }
            if let Some(g) = f.group_id {
                qb.push(" and c.group_id = ").push_bind(g);
            }
        },
        &format!("{order}, c.id desc"),
        page,
    )
    .await
}

/// A login (user) of a client account.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientLogin {
    #[sqlx(flatten)]
    pub user: UserRow,
    pub is_owner: bool,
}

/// Users linked to a client, owners first.
pub async fn client_logins(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<ClientLogin>> {
    Ok(sqlx::query_as::<_, ClientLogin>(
        "select u.*, cu.is_owner from client_users cu
           join users u on u.id = cu.user_id and u.tenant_id = cu.tenant_id
          where cu.tenant_id = $1 and cu.client_id = $2
          order by cu.is_owner desc, u.created_at",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// Billing behaviour flags of a client.
pub async fn set_client_billing_flags(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ClientId,
    late_fee_exempt: bool,
    overdue_notices: bool,
    separate_invoices: bool,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update clients set late_fee_exempt = $3, overdue_notices = $4, separate_invoices = $5,
                updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(late_fee_exempt)
    .bind(overdue_notices)
    .bind(separate_invoices)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Edits a client group.
pub async fn update_client_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ClientGroupId,
    name: &str,
    description: Option<&str>,
    color: Option<&str>,
    discount_percent: Decimal,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update client_groups set name = $3, description = $4, color = $5, discount_percent = $6,
                updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(color)
    .bind(discount_percent)
    .execute(db)
    .await
    .map_err(crate::error::conflict_on_unique(
        "client group name already exists",
    ))?;
    Ok(r.rows_affected() == 1)
}

/// Deletes a client group (members keep their account, `group_id` → null).
pub async fn delete_client_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ClientGroupId,
) -> DbResult<bool> {
    let r = sqlx::query("delete from client_groups where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Deletes a staff note of a client.
pub async fn delete_client_note(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    id: ClientNoteId,
) -> DbResult<bool> {
    let r =
        sqlx::query("delete from client_notes where tenant_id = $1 and client_id = $2 and id = $3")
            .bind(tenant_id)
            .bind(client_id)
            .bind(id)
            .execute(db)
            .await?;
    Ok(r.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// Orders / services
// ---------------------------------------------------------------------------

const SERVICE_SELECT: &str = "select s.*, p.name as product_name, p.product_type,
        coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name";

/// Marks a pending order as fraud and cancels its unpaid invoice.
pub async fn mark_order_fraud(pool: &PgPool, tenant_id: TenantId, id: OrderId) -> DbResult<bool> {
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "update orders set status = 'fraud', cancelled_at = now(), updated_at = now()
          where tenant_id = $1 and id = $2 and status = 'pending'",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query(
            "update invoices set status = 'cancelled', cancelled_at = now(), updated_at = now()
              where tenant_id = $1 and status in ('draft', 'unpaid', 'payment_pending')
                and id = (select invoice_id from orders where tenant_id = $1 and id = $2)",
        )
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "update services set status = 'cancelled', cancelled_at = now(), updated_at = now()
              where tenant_id = $1 and order_id = $2 and status in ('pending', 'failed')",
        )
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Services waiting for staff to provision them by hand
/// (`module_data.manual = true`, not active yet).
pub async fn manual_queue(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<ServiceListRow>> {
    Ok(sqlx::query_as::<_, ServiceListRow>(&format!(
        "{SERVICE_SELECT}
           from services s join products p on p.id = s.product_id join clients c on c.id = s.client_id
          where s.tenant_id = $1 and s.status in ('pending', 'provisioning', 'failed')
            and coalesce((s.module_data ->> 'manual')::boolean, false)
          order by s.created_at"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Services with sort + server (integration) filter.
pub async fn list_services<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ServiceFilter,
    integration_id: Option<IntegrationId>,
    sort: Option<&str>,
    desc: bool,
    page: Page,
) -> DbResult<Paged<ServiceListRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    let order = match sort {
        Some("product") => format!("p.name {}", dir(desc)),
        Some("client") => format!("client_name {}", dir(desc)),
        Some("due") => format!("s.next_due_date {} nulls last", dir(desc)),
        Some("amount") => format!("s.recurring_amount {}", dir(desc)),
        Some("created") => format!("s.created_at {}", dir(desc)),
        _ => "s.created_at desc".to_owned(),
    };
    fetch_paged(
        db,
        SERVICE_SELECT,
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
            if let Some(i) = integration_id {
                qb.push(" and s.integration_id = ").push_bind(i);
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
                    .push(" or p.name ilike ")
                    .push_bind(s.clone())
                    .push(" or c.email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or (c.first_name || ' ' || c.last_name) ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        &format!("{order}, s.id desc"),
        page,
    )
    .await
}

/// Services with an open cancellation request.
pub async fn cancellation_requests(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<ServiceListRow>> {
    Ok(sqlx::query_as::<_, ServiceListRow>(&format!(
        "{SERVICE_SELECT}
           from services s join products p on p.id = s.product_id join clients c on c.id = s.client_id
          where s.tenant_id = $1 and s.cancellation_requested_at is not null
            and s.status not in ('terminated', 'cancelled')
          order by s.cancellation_requested_at"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Invoices with a line for `service_id`, newest first.
pub async fn service_invoices(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<Vec<InvoiceRow>> {
    Ok(sqlx::query_as::<_, InvoiceRow>(
        "select i.* from invoices i
          where i.tenant_id = $1
            and exists (select 1 from invoice_items it
                         where it.tenant_id = i.tenant_id and it.invoice_id = i.id and it.service_id = $2)
          order by i.issue_date desc, i.created_at desc limit 50",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_all(db)
    .await?)
}

/// Sets the next due / invoice dates of a service directly (staff override).
pub async fn set_service_due_dates(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ServiceId,
    next_due_date: Option<chrono::NaiveDate>,
    next_invoice_date: Option<chrono::NaiveDate>,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update services set next_due_date = $3, next_invoice_date = $4, updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(next_due_date)
    .bind(next_invoice_date)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Count of services per status (filter chips).
pub async fn service_status_counts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(ServiceStatus, i64)>> {
    Ok(sqlx::query_as::<_, (ServiceStatus, i64)>(
        "select status, count(*) from services where tenant_id = $1 group by status",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// Edits a product group.
pub async fn update_product_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductGroupId,
    g: &ProductGroupInput,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update product_groups set slug = $3, name = $4, tagline = $5, description = $6,
                translations = $7, icon = $8, is_hidden = $9, sort_order = $10, updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&g.slug)
    .bind(&g.name)
    .bind(&g.tagline)
    .bind(&g.description)
    .bind(&g.translations)
    .bind(&g.icon)
    .bind(g.is_hidden)
    .bind(g.sort_order)
    .execute(db)
    .await
    .map_err(crate::error::conflict_on_unique(
        "product group slug already exists",
    ))?;
    Ok(r.rows_affected() == 1)
}

/// Deletes a product group (its products become ungrouped).
pub async fn delete_product_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductGroupId,
) -> DbResult<bool> {
    let r = sqlx::query("delete from product_groups where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Stock, quantity and retirement flags of a product.
pub async fn set_product_extras(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductId,
    stock_control: bool,
    stock_quantity: i32,
    allow_quantity: bool,
    is_retired: bool,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update products set stock_control = $3, stock_quantity = $4, allow_quantity = $5,
                is_retired = $6, updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(stock_control)
    .bind(stock_quantity.max(0))
    .bind(allow_quantity)
    .bind(is_retired)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Whether any service uses the product.
pub async fn product_in_use(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists (select 1 from services where tenant_id = $1 and product_id = $2)",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_one(db)
    .await?)
}

/// Deletes an unused product (pricing, option links cascade).
pub async fn delete_product(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductId,
) -> DbResult<bool> {
    let r = sqlx::query(
        "delete from products p where p.tenant_id = $1 and p.id = $2
            and not exists (select 1 from services s where s.tenant_id = p.tenant_id and s.product_id = p.id)",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Every addon of the tenant.
pub async fn list_addons(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<ProductAddonRow>> {
    Ok(sqlx::query_as::<_, ProductAddonRow>(
        "select * from product_addons where tenant_id = $1 order by sort_order, name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Edits an addon's descriptive fields.
#[allow(clippy::too_many_arguments)]
pub async fn update_addon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductAddonId,
    name: &str,
    description: Option<&str>,
    show_on_order: bool,
    auto_activate: bool,
    is_hidden: bool,
    welcome_email_template: Option<&str>,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update product_addons set name = $3, description = $4, show_on_order = $5,
                auto_activate = $6, is_hidden = $7, welcome_email_template = $8, updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(show_on_order)
    .bind(auto_activate)
    .bind(is_hidden)
    .bind(welcome_email_template)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Deletes an addon (fails while services use it).
pub async fn delete_addon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductAddonId,
) -> DbResult<bool> {
    let r = sqlx::query(
        "delete from product_addons a where a.tenant_id = $1 and a.id = $2
            and not exists (select 1 from service_addons sa where sa.tenant_id = a.tenant_id and sa.addon_id = a.id)",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Products an addon is offered with.
pub async fn addon_product_ids(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    addon_id: ProductAddonId,
) -> DbResult<Vec<ProductId>> {
    Ok(sqlx::query_scalar::<_, ProductId>(
        "select product_id from product_addon_products where tenant_id = $1 and addon_id = $2",
    )
    .bind(tenant_id)
    .bind(addon_id)
    .fetch_all(db)
    .await?)
}

/// Replaces the product links of an addon.
pub async fn set_addon_products(
    pool: &PgPool,
    tenant_id: TenantId,
    addon_id: ProductAddonId,
    products: &[ProductId],
) -> DbResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from product_addon_products where tenant_id = $1 and addon_id = $2")
        .bind(tenant_id)
        .bind(addon_id)
        .execute(&mut *tx)
        .await?;
    for p in products {
        sqlx::query(
            "insert into product_addon_products (tenant_id, addon_id, product_id)
             select $1, $2, id from products where tenant_id = $1 and id = $3
             on conflict do nothing",
        )
        .bind(tenant_id)
        .bind(addon_id)
        .bind(p)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// One stored price `(currency, cycle, price, setup)`.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PriceRow {
    pub currency: Currency,
    pub billing_cycle: BillingCycle,
    pub price: Decimal,
    pub setup_fee: Decimal,
}

/// Prices of an addon.
pub async fn addon_pricing(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    addon_id: ProductAddonId,
) -> DbResult<Vec<PriceRow>> {
    Ok(sqlx::query_as::<_, PriceRow>(
        "select currency, billing_cycle, price, setup_fee from product_addon_pricing
          where tenant_id = $1 and addon_id = $2",
    )
    .bind(tenant_id)
    .bind(addon_id)
    .fetch_all(db)
    .await?)
}

/// Removes one addon price.
pub async fn delete_addon_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    addon_id: ProductAddonId,
    currency: Currency,
    cycle: BillingCycle,
) -> DbResult<bool> {
    let r = sqlx::query(
        "delete from product_addon_pricing
          where tenant_id = $1 and addon_id = $2 and currency = $3 and billing_cycle = $4",
    )
    .bind(tenant_id)
    .bind(addon_id)
    .bind(currency)
    .bind(cycle)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// A configurable option group with its product count.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ConfigGroupRow {
    pub id: ConfigOptionGroupId,
    pub name: String,
    pub description: Option<String>,
    pub product_count: i64,
    pub option_count: i64,
}

/// Configurable option groups.
pub async fn config_groups(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<ConfigGroupRow>> {
    Ok(sqlx::query_as::<_, ConfigGroupRow>(
        "select g.id, g.name, g.description,
                (select count(*) from config_option_group_products gp where gp.group_id = g.id) as product_count,
                (select count(*) from config_options o where o.group_id = g.id) as option_count
           from config_option_groups g where g.tenant_id = $1 order by g.name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Option groups attached to a product.
pub async fn product_config_group_ids(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
) -> DbResult<Vec<ConfigOptionGroupId>> {
    Ok(sqlx::query_scalar::<_, ConfigOptionGroupId>(
        "select group_id from config_option_group_products where tenant_id = $1 and product_id = $2",
    )
    .bind(tenant_id)
    .bind(product_id)
    .fetch_all(db)
    .await?)
}

/// Replaces the option groups attached to a product.
pub async fn set_product_config_groups(
    pool: &PgPool,
    tenant_id: TenantId,
    product_id: ProductId,
    groups: &[ConfigOptionGroupId],
) -> DbResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "delete from config_option_group_products where tenant_id = $1 and product_id = $2",
    )
    .bind(tenant_id)
    .bind(product_id)
    .execute(&mut *tx)
    .await?;
    for g in groups {
        sqlx::query(
            "insert into config_option_group_products (tenant_id, group_id, product_id)
             select $1, id, $3 from config_option_groups where tenant_id = $1 and id = $2
             on conflict do nothing",
        )
        .bind(tenant_id)
        .bind(g)
        .bind(product_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// One option of a group with its values (prices in `currency`).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct OptionValueRow {
    pub option_id: ConfigOptionId,
    pub option_name: String,
    pub option_type: crate::enums::ConfigOptionType,
    pub module_key: Option<String>,
    pub value_id: Option<ConfigOptionValueId>,
    pub label: Option<String>,
    pub value: Option<String>,
    pub monthly: Option<Decimal>,
}

/// Options + values of a group (flat, ordered), with the monthly price in `currency`.
pub async fn group_option_values(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    group_id: ConfigOptionGroupId,
    currency: Currency,
) -> DbResult<Vec<OptionValueRow>> {
    Ok(sqlx::query_as::<_, OptionValueRow>(
        "select o.id as option_id, o.name as option_name, o.option_type, o.module_key,
                v.id as value_id, v.label, v.value,
                (select pr.price from config_option_pricing pr
                  where pr.value_id = v.id and pr.currency = $3 and pr.billing_cycle = 'monthly') as monthly
           from config_options o
           left join config_option_values v on v.option_id = o.id
          where o.tenant_id = $1 and o.group_id = $2
          order by o.sort_order, o.name, v.sort_order, v.label",
    )
    .bind(tenant_id)
    .bind(group_id)
    .bind(currency)
    .fetch_all(db)
    .await?)
}

/// One stored price of a configurable option value.
#[derive(Debug, Clone, FromRow)]
pub struct ValuePriceRow {
    pub value_id: ConfigOptionValueId,
    pub billing_cycle: BillingCycle,
    pub price: Decimal,
    pub setup_fee: Decimal,
}

/// All prices in `currency` of the option values of a group.
pub async fn group_value_prices(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    group_id: ConfigOptionGroupId,
    currency: Currency,
) -> DbResult<Vec<ValuePriceRow>> {
    Ok(sqlx::query_as::<_, ValuePriceRow>(
        "select pr.value_id, pr.billing_cycle, pr.price, pr.setup_fee
           from config_option_pricing pr
           join config_option_values v on v.id = pr.value_id
           join config_options o on o.id = v.option_id
          where pr.tenant_id = $1 and o.group_id = $2 and pr.currency = $3",
    )
    .bind(tenant_id)
    .bind(group_id)
    .bind(currency)
    .fetch_all(db)
    .await?)
}

/// Removes one price point of an option value.
pub async fn delete_config_value_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    value_id: ConfigOptionValueId,
    currency: Currency,
    cycle: BillingCycle,
) -> DbResult<bool> {
    let r = sqlx::query(
        "delete from config_option_pricing
          where tenant_id = $1 and value_id = $2 and currency = $3 and billing_cycle = $4",
    )
    .bind(tenant_id)
    .bind(value_id)
    .bind(currency)
    .bind(cycle)
    .execute(db)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Deletes an option group (options, values, prices cascade).
pub async fn delete_config_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ConfigOptionGroupId,
) -> DbResult<bool> {
    let r = sqlx::query("delete from config_option_groups where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Deletes one option (values cascade).
pub async fn delete_config_option(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ConfigOptionId,
) -> DbResult<bool> {
    let r = sqlx::query("delete from config_options where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Deletes one option value.
pub async fn delete_config_value(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ConfigOptionValueId,
) -> DbResult<bool> {
    let r = sqlx::query("delete from config_option_values where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Group of an option (tenant check before adding values).
pub async fn config_option_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    option_id: ConfigOptionId,
) -> DbResult<Option<ConfigOptionGroupId>> {
    Ok(sqlx::query_scalar::<_, ConfigOptionGroupId>(
        "select group_id from config_options where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(option_id)
    .fetch_optional(db)
    .await?)
}

/// Coupon fields not covered by `catalog::create_coupon`.
#[derive(Debug, Clone, PartialEq)]
pub struct CouponUpdate {
    pub description: Option<String>,
    pub discount_type: DiscountType,
    pub value: Decimal,
    pub currency: Option<Currency>,
    pub product_ids: Vec<ProductId>,
    pub billing_cycles: Vec<String>,
    pub applies_to_setup: bool,
    pub recurring: bool,
    pub recurring_limit: i32,
    pub new_clients_only: bool,
    pub once_per_client: bool,
    pub min_order_amount: Option<Decimal>,
    pub max_uses: Option<i32>,
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub is_active: bool,
}

/// Edits a coupon (the code stays).
pub async fn update_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CouponId,
    c: &CouponUpdate,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update coupons set description = $3, discount_type = $4, value = $5, currency = $6,
                product_ids = $7, billing_cycles = $8, applies_to_setup = $9, recurring = $10,
                recurring_limit = $11, new_clients_only = $12, once_per_client = $13,
                min_order_amount = $14, max_uses = $15, starts_at = $16, expires_at = $17,
                is_active = $18, updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&c.description)
    .bind(c.discount_type)
    .bind(c.value)
    .bind(c.currency)
    .bind(&c.product_ids)
    .bind(&c.billing_cycles)
    .bind(c.applies_to_setup)
    .bind(c.recurring)
    .bind(c.recurring_limit)
    .bind(c.new_clients_only)
    .bind(c.once_per_client)
    .bind(c.min_order_amount)
    .bind(c.max_uses)
    .bind(c.starts_at)
    .bind(c.expires_at)
    .bind(c.is_active)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// One coupon.
pub async fn get_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CouponId,
) -> DbResult<Option<CouponRow>> {
    Ok(
        sqlx::query_as::<_, CouponRow>("select * from coupons where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Deletes a coupon (orders keep the code text).
pub async fn delete_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CouponId,
) -> DbResult<bool> {
    let r = sqlx::query("delete from coupons where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Edits a tax rule.
#[allow(clippy::too_many_arguments)]
pub async fn update_tax_rule(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TaxRuleId,
    name: &str,
    level: i16,
    country_code: &str,
    region: Option<&str>,
    rate: Decimal,
    is_compound: bool,
    is_active: bool,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update tax_rules set name = $3, level = $4, country_code = nullif($5, ''), region = $6,
                rate = $7, is_compound = $8, is_active = $9, updated_at = now()
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(name)
    .bind(level)
    .bind(country_code)
    .bind(region)
    .bind(rate)
    .bind(is_compound)
    .bind(is_active)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Deletes a tax rule (issued invoices keep their tax lines).
pub async fn delete_tax_rule(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TaxRuleId,
) -> DbResult<bool> {
    let r = sqlx::query("delete from tax_rules where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Activates / deactivates a currency (the default one stays active).
pub async fn set_currency_active(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    code: Currency,
    active: bool,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update currencies set is_active = $3, updated_at = now()
          where tenant_id = $1 and code = $2 and (is_default = false or $3)",
    )
    .bind(tenant_id)
    .bind(code)
    .bind(active)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Incoming gateway / registrar webhooks, newest first.
pub async fn list_webhooks_in<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    integration_id: Option<IntegrationId>,
    page: Page,
) -> DbResult<Paged<WebhookInRow>> {
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from webhooks_in where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(i) = integration_id {
                qb.push(" and integration_id = ").push_bind(i);
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}

/// A payment attempt with its invoice number.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct AttemptRow {
    pub id: Uuid,
    pub invoice_id: InvoiceId,
    pub invoice_number: Option<String>,
    pub integration_id: Option<IntegrationId>,
    pub integration_name: Option<String>,
    pub reference: String,
    pub gateway_ref: Option<String>,
    pub amount: Decimal,
    pub currency: Currency,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// Payment attempts (checkout sessions), newest first.
pub async fn list_payment_attempts<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    page: Page,
) -> DbResult<Paged<AttemptRow>> {
    fetch_paged(
        db,
        "select a.id, a.invoice_id, i.number as invoice_number, a.integration_id,
                g.name as integration_name, a.reference, a.gateway_ref, a.amount, a.currency,
                a.status, a.created_at",
        |qb| {
            qb.push(
                "from payment_attempts a
                   join invoices i on i.id = a.invoice_id
                   left join integrations g on g.id = a.integration_id
                  where a.tenant_id = ",
            )
            .push_bind(tenant_id);
        },
        "a.created_at desc, a.id desc",
        page,
    )
    .await
}

// ---------------------------------------------------------------------------
// TLDs / IP pools
// ---------------------------------------------------------------------------

/// Enables / disables a TLD.
pub async fn set_tld_active(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: TldId,
    active: bool,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update tlds set is_active = $3, updated_at = now() where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(active)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// One TLD price cell.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TldPriceCell {
    pub tld_id: TldId,
    pub currency: Currency,
    pub action: crate::enums::TldAction,
    pub years: i16,
    pub price: Decimal,
}

/// Every one-year price of the tenant's TLDs in `currency`.
pub async fn tld_one_year_prices(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    currency: Currency,
) -> DbResult<Vec<TldPriceCell>> {
    Ok(sqlx::query_as::<_, TldPriceCell>(
        "select tld_id, currency, action, years, price from tld_pricing
          where tenant_id = $1 and currency = $2 and years = 1",
    )
    .bind(tenant_id)
    .bind(currency)
    .fetch_all(db)
    .await?)
}

/// Addresses of an IP pool.
pub async fn pool_addresses(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    pool_id: hos_core::ids::IpPoolId,
) -> DbResult<Vec<IpAddressRow>> {
    Ok(sqlx::query_as::<_, IpAddressRow>(
        "select * from ip_addresses where tenant_id = $1 and pool_id = $2 order by address limit 4096",
    )
    .bind(tenant_id)
    .bind(pool_id)
    .fetch_all(db)
    .await?)
}

/// Sets the status of a free / reserved / blocked address (assigned ones are
/// released through the service).
pub async fn set_ip_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: hos_core::ids::IpAddressId,
    status: IpStatus,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update ip_addresses set status = $3, updated_at = now()
          where tenant_id = $1 and id = $2 and status <> 'assigned' and $3 <> 'assigned'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Deletes an IP pool without assigned addresses.
pub async fn delete_ip_pool(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: hos_core::ids::IpPoolId,
) -> DbResult<bool> {
    let r = sqlx::query(
        "delete from ip_pools p where p.tenant_id = $1 and p.id = $2
            and not exists (select 1 from ip_addresses a where a.pool_id = p.id and a.status = 'assigned')",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// Global search
// ---------------------------------------------------------------------------

/// One search hit.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SearchHit {
    /// `client`, `invoice`, `domain`, `service`, `ticket`, `order`,
    /// `transaction` (id = its invoice) or `transaction_client` (id = its
    /// client; no invoice).
    pub kind: String,
    pub id: Uuid,
    pub title: String,
    pub subtitle: Option<String>,
}

/// Searches clients, invoices, orders, domains, services, tickets and
/// transactions by gateway id (5 each).
pub async fn search(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    term: &str,
) -> DbResult<Vec<SearchHit>> {
    let like = like_contains(term);
    Ok(sqlx::query_as::<_, SearchHit>(
        "(select 'client' as kind, c.id,
                 coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as title,
                 c.email::text as subtitle
            from clients c
           where c.tenant_id = $1 and (c.email::text ilike $2 or (c.first_name || ' ' || c.last_name) ilike $2
                 or c.company_name ilike $2 or c.phone ilike $2 or c.tax_number ilike $2)
           order by c.created_at desc limit 5)
         union all
         (select 'invoice', i.id, coalesce(i.number, '#' || left(i.id::text, 8)),
                 i.status || ' · ' || i.total::numeric(20,2)::text || ' ' || i.currency
            from invoices i
           where i.tenant_id = $1 and (i.number ilike $2 or i.id::text ilike $2)
           order by i.created_at desc limit 5)
         union all
         (select 'order', o.id, o.order_number, o.status
            from orders o where o.tenant_id = $1 and o.order_number ilike $2
           order by o.created_at desc limit 5)
         union all
         (select 'domain', d.id, d.name, d.status
            from domains d where d.tenant_id = $1 and d.name ilike $2
           order by d.created_at desc limit 5)
         union all
         (select 'service', s.id, p.name || coalesce(' · ' || s.domain, ''),
                 coalesce(s.username, host(s.dedicated_ip), s.status)
            from services s join products p on p.id = s.product_id
           where s.tenant_id = $1 and (s.domain ilike $2 or s.username ilike $2
                 or host(s.dedicated_ip) ilike $2 or s.external_id ilike $2)
           order by s.created_at desc limit 5)
         union all
         (select 'ticket', t.id, '#' || t.number || ' · ' || t.subject, t.status
            from tickets t where t.tenant_id = $1 and (t.number ilike $2 or t.subject ilike $2)
           order by t.created_at desc limit 5)
         union all
         (select case when x.invoice_id is null then 'transaction_client' else 'transaction' end,
                 coalesce(x.invoice_id, x.client_id), x.gateway_txn_id,
                 x.kind || ' · ' || x.amount::numeric(20,2)::text || ' ' || x.currency || ' · ' || x.status
            from transactions x
           where x.tenant_id = $1 and x.gateway_txn_id ilike $2
           order by x.occurred_at desc limit 5)",
    )
    .bind(tenant_id)
    .bind(like)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// Dashboard health / system
// ---------------------------------------------------------------------------

/// Queue health of a tenant.
#[derive(Debug, Clone, PartialEq, FromRow, Default)]
pub struct QueueHealth {
    pub pending: i64,
    pub running: i64,
    pub failed: i64,
    /// Seconds the oldest due pending job has been waiting.
    pub lag_seconds: Option<i64>,
}

/// Job counts and lag for the tenant's jobs.
pub async fn queue_health(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<QueueHealth> {
    Ok(sqlx::query_as::<_, QueueHealth>(
        "select count(*) filter (where status = 'pending') as pending,
                count(*) filter (where status = 'running') as running,
                count(*) filter (where status = 'failed') as failed,
                (extract(epoch from now() - min(run_at) filter (where status = 'pending' and run_at <= now())))::bigint
                    as lag_seconds
           from jobs where tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await?)
}

/// Failed (dead) jobs of the tenant, newest first.
pub async fn failed_jobs(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<JobRow>> {
    Ok(sqlx::query_as::<_, JobRow>(
        "select * from jobs where tenant_id = $1 and status = 'failed'
          order by coalesce(failed_at, updated_at) desc limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Jobs of the tenant (status filter), newest first.
pub async fn tenant_jobs<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    status: Option<crate::enums::JobStatus>,
    page: Page,
) -> DbResult<Paged<JobRow>> {
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from jobs where tenant_id = ").push_bind(tenant_id);
            if let Some(s) = status {
                qb.push(" and status = ").push_bind(s);
            }
        },
        "updated_at desc, id desc",
        page,
    )
    .await
}

/// A tenant job (tenant check for retry / delete).
pub async fn tenant_job(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: hos_core::ids::JobId,
) -> DbResult<Option<JobRow>> {
    Ok(
        sqlx::query_as::<_, JobRow>("select * from jobs where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Deletes a finished / failed / cancelled job of the tenant.
pub async fn delete_job(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: hos_core::ids::JobId,
) -> DbResult<bool> {
    let r = sqlx::query(
        "delete from jobs where tenant_id = $1 and id = $2 and status in ('failed', 'completed', 'cancelled')",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Pending-work counters for the dashboard.
#[derive(Debug, Clone, PartialEq, FromRow, Default)]
pub struct WorkCounts {
    pub pending_orders: i64,
    pub manual_provisioning: i64,
    pub failed_services: i64,
    pub cancellation_requests: i64,
    pub open_tickets: i64,
}

/// Counters of things waiting for staff.
pub async fn work_counts(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<WorkCounts> {
    Ok(sqlx::query_as::<_, WorkCounts>(
        "select
            (select count(*) from orders where tenant_id = $1 and status = 'pending') as pending_orders,
            (select count(*) from services where tenant_id = $1 and status in ('pending', 'provisioning', 'failed')
                and coalesce((module_data ->> 'manual')::boolean, false)) as manual_provisioning,
            (select count(*) from services where tenant_id = $1 and status = 'failed') as failed_services,
            (select count(*) from services where tenant_id = $1 and cancellation_requested_at is not null
                and status not in ('terminated', 'cancelled')) as cancellation_requests,
            (select count(*) from tickets where tenant_id = $1 and status <> $2) as open_tickets",
    )
    .bind(tenant_id)
    .bind(TicketStatus::Closed)
    .fetch_one(db)
    .await?)
}

/// Raw JSON of a tenant setting group (`prefix.*`) — re-exported helper for
/// forms that edit several keys.
pub async fn settings_json(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    prefix: &str,
) -> DbResult<Json> {
    let map = super::platform::get_settings(db, tenant_id, prefix).await?;
    Ok(Json::Object(map.into_iter().collect()))
}
