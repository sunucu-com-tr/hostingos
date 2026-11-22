//! Orders and order lines. [`create_order`] writes the order and all its lines
//! in one transaction (and optionally marks the source cart converted).

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use hos_core::CoreError;
use hos_core::domain::OrderStatus;
use hos_core::ids::{
    CartId, ClientId, CouponId, DomainId, IntegrationId, InvoiceId, OrderId, OrderItemId,
    ProductAddonId, ProductId, ServiceAddonId, ServiceId, TenantId, UserId,
};
use hos_core::money::{BillingCycle, Currency};
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};

use super::{Json, empty_object};
use crate::enums::LineItemType;
use crate::error::{DbResult, like_contains};
use crate::page::{Page, Paged, fetch_paged};

/// `orders` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct OrderRow {
    pub id: OrderId,
    pub tenant_id: TenantId,
    pub order_number: String,
    pub client_id: ClientId,
    pub user_id: Option<UserId>,
    pub status: OrderStatus,
    pub currency: Currency,
    pub subtotal: Decimal,
    pub discount_total: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub coupon_id: Option<CouponId>,
    pub coupon_code: Option<String>,
    pub payment_integration_id: Option<IntegrationId>,
    pub invoice_id: Option<InvoiceId>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub fraud_score: Option<Decimal>,
    pub fraud_details: Json,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `order_items` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct OrderItemRow {
    pub id: OrderItemId,
    pub tenant_id: TenantId,
    pub order_id: OrderId,
    pub parent_item_id: Option<OrderItemId>,
    pub item_type: LineItemType,
    pub product_id: Option<ProductId>,
    pub addon_id: Option<ProductAddonId>,
    pub service_id: Option<ServiceId>,
    pub service_addon_id: Option<ServiceAddonId>,
    pub domain_id: Option<DomainId>,
    pub domain_name: Option<String>,
    pub billing_cycle: Option<BillingCycle>,
    pub years: Option<i16>,
    pub quantity: i32,
    pub description: String,
    pub unit_price: Decimal,
    pub setup_fee: Decimal,
    pub discount: Decimal,
    pub amount: Decimal,
    pub config_selections: Json,
    pub custom_fields: Json,
    pub hostname: Option<String>,
    pub epp_code_enc: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One line of [`NewOrder`]. `amount = quantity × unit_price + setup_fee − discount`.
#[derive(Debug, Clone)]
pub struct NewOrderItem {
    /// Index (into `NewOrder::items`) of the parent line, which must come earlier.
    pub parent_index: Option<usize>,
    pub item_type: LineItemType,
    pub product_id: Option<ProductId>,
    pub addon_id: Option<ProductAddonId>,
    pub domain_name: Option<String>,
    pub billing_cycle: Option<BillingCycle>,
    pub years: Option<i16>,
    pub quantity: i32,
    pub description: String,
    pub unit_price: Decimal,
    pub setup_fee: Decimal,
    pub discount: Decimal,
    pub config_selections: Json,
    pub custom_fields: Json,
    pub hostname: Option<String>,
    pub epp_code_enc: Option<Vec<u8>>,
}

impl NewOrderItem {
    /// A product line.
    pub fn product(
        product_id: ProductId,
        cycle: BillingCycle,
        description: &str,
        unit_price: Decimal,
    ) -> Self {
        Self {
            parent_index: None,
            item_type: LineItemType::Product,
            product_id: Some(product_id),
            addon_id: None,
            domain_name: None,
            billing_cycle: Some(cycle),
            years: None,
            quantity: 1,
            description: description.to_owned(),
            unit_price,
            setup_fee: Decimal::ZERO,
            discount: Decimal::ZERO,
            config_selections: empty_object(),
            custom_fields: empty_object(),
            hostname: None,
            epp_code_enc: None,
        }
    }

    /// A domain line.
    pub fn domain(
        item_type: LineItemType,
        domain_name: &str,
        years: i16,
        description: &str,
        price: Decimal,
    ) -> Self {
        Self {
            parent_index: None,
            item_type,
            product_id: None,
            addon_id: None,
            domain_name: Some(domain_name.trim().to_ascii_lowercase()),
            billing_cycle: None,
            years: Some(years),
            quantity: 1,
            description: description.to_owned(),
            unit_price: price,
            setup_fee: Decimal::ZERO,
            discount: Decimal::ZERO,
            config_selections: empty_object(),
            custom_fields: empty_object(),
            hostname: None,
            epp_code_enc: None,
        }
    }

    /// `quantity × unit_price + setup_fee − discount`.
    pub fn amount(&self) -> Decimal {
        Decimal::from(self.quantity) * self.unit_price + self.setup_fee - self.discount
    }
}

/// Input of [`create_order`]. `tax_total` comes from hos-billing.
#[derive(Debug, Clone)]
pub struct NewOrder {
    pub client_id: ClientId,
    pub user_id: Option<UserId>,
    pub currency: Currency,
    pub coupon_id: Option<CouponId>,
    pub coupon_code: Option<String>,
    pub payment_integration_id: Option<IntegrationId>,
    pub tax_total: Decimal,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub notes: Option<String>,
    /// Cart to mark as converted in the same transaction.
    pub cart_id: Option<CartId>,
    pub items: Vec<NewOrderItem>,
}

/// A random 10-digit public order number.
fn random_order_number() -> String {
    let bytes: [u8; 8] = hos_core::crypto::random_bytes();
    let n = u64::from_le_bytes(bytes) % 9_000_000_000 + 1_000_000_000;
    n.to_string()
}

/// Creates an order with its lines atomically. Totals: `subtotal = Σ amount`,
/// `discount_total = Σ discount`, `total = subtotal + tax_total`.
pub async fn create_order(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    new: &NewOrder,
) -> DbResult<(OrderRow, Vec<OrderItemRow>)> {
    if new.items.is_empty() {
        return Err(CoreError::validation("an order needs at least one item"));
    }
    if let Some((i, _)) = new
        .items
        .iter()
        .enumerate()
        .find(|(i, it)| it.parent_index.is_some_and(|p| p >= *i))
    {
        return Err(CoreError::validation(format!(
            "order item {i}: parent must precede the item"
        )));
    }
    let subtotal: Decimal = new.items.iter().map(NewOrderItem::amount).sum();
    let discount_total: Decimal = new.items.iter().map(|i| i.discount).sum();
    let total = subtotal + new.tax_total;

    let mut tx = sqlx::Connection::begin(&mut *conn).await?;

    let mut order: Option<OrderRow> = None;
    for _ in 0..8 {
        order = sqlx::query_as::<_, OrderRow>(
            "insert into orders (id, tenant_id, order_number, client_id, user_id, status, currency, subtotal,
                                 discount_total, tax_total, total, coupon_id, coupon_code, payment_integration_id,
                                 ip, user_agent, notes)
             values ($1, $2, $3, $4, $5, 'pending', $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             on conflict (tenant_id, order_number) do nothing
             returning *",
        )
        .bind(OrderId::new())
        .bind(tenant_id)
        .bind(random_order_number())
        .bind(new.client_id)
        .bind(new.user_id)
        .bind(new.currency)
        .bind(subtotal)
        .bind(discount_total)
        .bind(new.tax_total)
        .bind(total)
        .bind(new.coupon_id)
        .bind(&new.coupon_code)
        .bind(new.payment_integration_id)
        .bind(new.ip)
        .bind(&new.user_agent)
        .bind(&new.notes)
        .fetch_optional(&mut *tx)
        .await?;
        if order.is_some() {
            break;
        }
    }
    let order =
        order.ok_or_else(|| CoreError::Internal("could not allocate an order number".into()))?;

    let mut items: Vec<OrderItemRow> = Vec::with_capacity(new.items.len());
    for it in &new.items {
        let parent = it.parent_index.and_then(|p| items.get(p)).map(|p| p.id);
        let row = sqlx::query_as::<_, OrderItemRow>(
            "insert into order_items (id, tenant_id, order_id, parent_item_id, item_type, product_id, addon_id,
                                      domain_name, billing_cycle, years, quantity, description, unit_price,
                                      setup_fee, discount, amount, config_selections, custom_fields, hostname,
                                      epp_code_enc)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
             returning *",
        )
        .bind(OrderItemId::new())
        .bind(tenant_id)
        .bind(order.id)
        .bind(parent)
        .bind(it.item_type)
        .bind(it.product_id)
        .bind(it.addon_id)
        .bind(&it.domain_name)
        .bind(it.billing_cycle)
        .bind(it.years)
        .bind(it.quantity)
        .bind(&it.description)
        .bind(it.unit_price)
        .bind(it.setup_fee)
        .bind(it.discount)
        .bind(it.amount())
        .bind(&it.config_selections)
        .bind(&it.custom_fields)
        .bind(&it.hostname)
        .bind(&it.epp_code_enc)
        .fetch_one(&mut *tx)
        .await?;
        items.push(row);
    }

    if let Some(cart_id) = new.cart_id {
        sqlx::query("update carts set converted_order_id = $3 where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(cart_id)
            .bind(order.id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok((order, items))
}

/// Order by id.
pub async fn get_order(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: OrderId,
) -> DbResult<Option<OrderRow>> {
    Ok(
        sqlx::query_as::<_, OrderRow>("select * from orders where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Order by public number.
pub async fn order_by_number(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    number: &str,
) -> DbResult<Option<OrderRow>> {
    Ok(sqlx::query_as::<_, OrderRow>(
        "select * from orders where tenant_id = $1 and order_number = $2",
    )
    .bind(tenant_id)
    .bind(number)
    .fetch_optional(db)
    .await?)
}

/// Lines of an order (parents before children).
pub async fn order_items(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    order_id: OrderId,
) -> DbResult<Vec<OrderItemRow>> {
    Ok(sqlx::query_as::<_, OrderItemRow>(
        "select * from order_items where tenant_id = $1 and order_id = $2 order by created_at, id",
    )
    .bind(tenant_id)
    .bind(order_id)
    .fetch_all(db)
    .await?)
}

/// Filters of [`list_orders`].
#[derive(Debug, Clone, Default)]
pub struct OrderFilter {
    pub status: Option<OrderStatus>,
    pub client_id: Option<ClientId>,
    /// Order number prefix / client e-mail / name.
    pub search: Option<String>,
}

/// Order list row (with client display name).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct OrderListRow {
    #[sqlx(flatten)]
    pub order: OrderRow,
    pub client_name: String,
    pub client_email: String,
    pub item_count: i64,
}

/// Orders of a tenant, newest first.
pub async fn list_orders<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &OrderFilter,
    page: Page,
) -> DbResult<Paged<OrderListRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        "select o.*,
                coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name,
                c.email::text as client_email,
                (select count(*) from order_items oi where oi.order_id = o.id) as item_count",
        |qb| {
            qb.push("from orders o join clients c on c.id = o.client_id where o.tenant_id = ")
                .push_bind(tenant_id);
            if let Some(s) = f.status {
                qb.push(" and o.status = ").push_bind(s);
            }
            if let Some(c) = f.client_id {
                qb.push(" and o.client_id = ").push_bind(c);
            }
            if let Some(s) = &search {
                qb.push(" and (o.order_number ilike ")
                    .push_bind(s.clone())
                    .push(" or c.email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or (c.first_name || ' ' || c.last_name) ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        "o.created_at desc, o.id desc",
        page,
    )
    .await
}

/// Moves an order from `from` to `to` if it is still in `from` (optimistic).
/// Rejects transitions `OrderStatus::transition` forbids. Stamps `accepted_at` / `cancelled_at`.
pub async fn transition_order(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: OrderId,
    from: OrderStatus,
    to: OrderStatus,
) -> DbResult<bool> {
    from.transition(to)?;
    let res = sqlx::query(
        "update orders set status = $4,
                accepted_at = case when $4 = 'accepted' then now() else accepted_at end,
                cancelled_at = case when $4 = 'cancelled' then now() else cancelled_at end
         where tenant_id = $1 and id = $2 and status = $3",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(from)
    .bind(to)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Links the order's invoice.
pub async fn set_order_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: OrderId,
    invoice_id: InvoiceId,
) -> DbResult<bool> {
    let res = sqlx::query("update orders set invoice_id = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(invoice_id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Records the service / service addon / domain created for an order line.
pub async fn link_order_item(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    item_id: OrderItemId,
    service_id: Option<ServiceId>,
    service_addon_id: Option<ServiceAddonId>,
    domain_id: Option<DomainId>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update order_items set service_id = coalesce($3, service_id),
                service_addon_id = coalesce($4, service_addon_id), domain_id = coalesce($5, domain_id)
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(item_id)
    .bind(service_id)
    .bind(service_addon_id)
    .bind(domain_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Stores the fraud check outcome.
pub async fn set_fraud_result(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: OrderId,
    score: Decimal,
    details: &Json,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update orders set fraud_score = $3, fraud_details = $4 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(score)
    .bind(details)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_numbers_have_ten_digits() {
        for _ in 0..100 {
            let n = random_order_number();
            assert_eq!(n.len(), 10);
            assert!(n.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn item_amount() {
        let mut it = NewOrderItem::product(
            ProductId::new(),
            BillingCycle::Monthly,
            "x",
            Decimal::new(1000, 2),
        );
        it.quantity = 3;
        it.setup_fee = Decimal::new(500, 2);
        it.discount = Decimal::new(250, 2);
        assert_eq!(it.amount(), Decimal::new(3250, 2));
    }
}
