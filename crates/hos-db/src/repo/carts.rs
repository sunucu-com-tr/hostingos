//! Shopping carts. A cart belongs to a guest (cookie token hash) or a user and
//! is converted into an order by `orders::create_order`.

use chrono::{DateTime, Utc};
use hos_core::ids::{
    CartId, ClientId, CouponId, IntegrationId, OrderId, ProductAddonId, ProductId, TenantId, UserId,
};
use hos_core::money::{BillingCycle, Currency};
use rust_decimal::Decimal;
use sqlx::{FromRow, PgExecutor};

use super::{Json, empty_object};
use crate::enums::LineItemType;
use crate::error::DbResult;
use crate::ids::CartItemId;

/// `carts` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CartRow {
    pub id: CartId,
    pub tenant_id: TenantId,
    pub user_id: Option<UserId>,
    pub client_id: Option<ClientId>,
    pub token_hash: Option<String>,
    pub currency: Currency,
    pub coupon_id: Option<CouponId>,
    pub coupon_code: Option<String>,
    pub payment_integration_id: Option<IntegrationId>,
    pub notes: Option<String>,
    pub converted_order_id: Option<OrderId>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `cart_items` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CartItemRow {
    pub id: CartItemId,
    pub tenant_id: TenantId,
    pub cart_id: CartId,
    pub parent_item_id: Option<CartItemId>,
    pub item_type: LineItemType,
    pub product_id: Option<ProductId>,
    pub addon_id: Option<ProductAddonId>,
    pub domain_name: Option<String>,
    pub billing_cycle: Option<BillingCycle>,
    pub years: Option<i16>,
    pub quantity: i32,
    pub config_selections: Json,
    pub custom_fields: Json,
    pub hostname: Option<String>,
    pub epp_code_enc: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`add_item`].
#[derive(Debug, Clone)]
pub struct NewCartItem {
    pub parent_item_id: Option<CartItemId>,
    pub item_type: LineItemType,
    pub product_id: Option<ProductId>,
    pub addon_id: Option<ProductAddonId>,
    pub domain_name: Option<String>,
    pub billing_cycle: Option<BillingCycle>,
    pub years: Option<i16>,
    pub quantity: i32,
    pub config_selections: Json,
    pub custom_fields: Json,
    pub hostname: Option<String>,
    pub epp_code_enc: Option<Vec<u8>>,
}

impl NewCartItem {
    /// A product line.
    pub fn product(product_id: ProductId, cycle: BillingCycle) -> Self {
        Self {
            parent_item_id: None,
            item_type: LineItemType::Product,
            product_id: Some(product_id),
            addon_id: None,
            domain_name: None,
            billing_cycle: Some(cycle),
            years: None,
            quantity: 1,
            config_selections: empty_object(),
            custom_fields: empty_object(),
            hostname: None,
            epp_code_enc: None,
        }
    }

    /// A domain line (`DomainRegister` / `DomainTransfer` / `DomainRenew`).
    pub fn domain(item_type: LineItemType, domain_name: &str, years: i16) -> Self {
        Self {
            parent_item_id: None,
            item_type,
            product_id: None,
            addon_id: None,
            domain_name: Some(domain_name.trim().to_ascii_lowercase()),
            billing_cycle: None,
            years: Some(years),
            quantity: 1,
            config_selections: empty_object(),
            custom_fields: empty_object(),
            hostname: None,
            epp_code_enc: None,
        }
    }
}

/// Creates a cart (guest: `token_hash`; logged in: `user_id`).
pub async fn create_cart(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    currency: Currency,
    user_id: Option<UserId>,
    client_id: Option<ClientId>,
    token_hash: Option<&str>,
) -> DbResult<CartRow> {
    Ok(sqlx::query_as::<_, CartRow>(
        "insert into carts (id, tenant_id, currency, user_id, client_id, token_hash)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(CartId::new())
    .bind(tenant_id)
    .bind(currency)
    .bind(user_id)
    .bind(client_id)
    .bind(token_hash)
    .fetch_one(db)
    .await?)
}

/// Cart by id.
pub async fn get_cart(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CartId,
) -> DbResult<Option<CartRow>> {
    Ok(
        sqlx::query_as::<_, CartRow>("select * from carts where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Open (unconverted, unexpired) guest cart by cookie token hash.
pub async fn cart_by_token(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    token_hash: &str,
) -> DbResult<Option<CartRow>> {
    Ok(sqlx::query_as::<_, CartRow>(
        "select * from carts where tenant_id = $1 and token_hash = $2
           and converted_order_id is null and expires_at > now()",
    )
    .bind(tenant_id)
    .bind(token_hash)
    .fetch_optional(db)
    .await?)
}

/// The user's most recent open cart.
pub async fn open_cart_for_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<Option<CartRow>> {
    Ok(sqlx::query_as::<_, CartRow>(
        "select * from carts where tenant_id = $1 and user_id = $2
           and converted_order_id is null and expires_at > now()
         order by updated_at desc limit 1",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?)
}

/// Attaches a guest cart to the user / client after login.
pub async fn assign_cart_owner(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
    user_id: UserId,
    client_id: Option<ClientId>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update carts set user_id = $3, client_id = $4 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(cart_id)
    .bind(user_id)
    .bind(client_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Sets or clears the coupon.
pub async fn set_cart_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
    coupon: Option<(CouponId, &str)>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update carts set coupon_id = $3, coupon_code = $4 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(cart_id)
    .bind(coupon.map(|c| c.0))
    .bind(coupon.map(|c| c.1))
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Changes currency / chosen gateway / notes.
pub async fn update_cart_checkout(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
    currency: Currency,
    payment_integration_id: Option<IntegrationId>,
    notes: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update carts set currency = $3, payment_integration_id = $4, notes = $5, expires_at = now() + interval '30 days'
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(cart_id)
    .bind(currency)
    .bind(payment_integration_id)
    .bind(notes)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Adds a line to a cart.
pub async fn add_item(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
    item: &NewCartItem,
) -> DbResult<CartItemRow> {
    Ok(sqlx::query_as::<_, CartItemRow>(
        "insert into cart_items (id, tenant_id, cart_id, parent_item_id, item_type, product_id, addon_id,
                                 domain_name, billing_cycle, years, quantity, config_selections, custom_fields,
                                 hostname, epp_code_enc)
         select $1, $2, c.id, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
         from carts c where c.tenant_id = $2 and c.id = $3
         returning *",
    )
    .bind(CartItemId::new())
    .bind(tenant_id)
    .bind(cart_id)
    .bind(item.parent_item_id)
    .bind(item.item_type)
    .bind(item.product_id)
    .bind(item.addon_id)
    .bind(&item.domain_name)
    .bind(item.billing_cycle)
    .bind(item.years)
    .bind(item.quantity)
    .bind(&item.config_selections)
    .bind(&item.custom_fields)
    .bind(&item.hostname)
    .bind(&item.epp_code_enc)
    .fetch_one(db)
    .await?)
}

/// Changes a line's quantity / cycle / years.
pub async fn update_item(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
    item_id: CartItemId,
    quantity: i32,
    billing_cycle: Option<BillingCycle>,
    years: Option<i16>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update cart_items set quantity = $4, billing_cycle = coalesce($5, billing_cycle), years = coalesce($6, years)
         where tenant_id = $1 and cart_id = $2 and id = $3",
    )
    .bind(tenant_id)
    .bind(cart_id)
    .bind(item_id)
    .bind(quantity)
    .bind(billing_cycle)
    .bind(years)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Removes a line (and its child lines).
pub async fn remove_item(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
    item_id: CartItemId,
) -> DbResult<bool> {
    let res =
        sqlx::query("delete from cart_items where tenant_id = $1 and cart_id = $2 and id = $3")
            .bind(tenant_id)
            .bind(cart_id)
            .bind(item_id)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Empties a cart.
pub async fn clear_cart(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
) -> DbResult<u64> {
    let res = sqlx::query("delete from cart_items where tenant_id = $1 and cart_id = $2")
        .bind(tenant_id)
        .bind(cart_id)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

/// Lines of a cart in insertion order.
pub async fn cart_items(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
) -> DbResult<Vec<CartItemRow>> {
    Ok(sqlx::query_as::<_, CartItemRow>(
        "select * from cart_items where tenant_id = $1 and cart_id = $2 order by created_at, id",
    )
    .bind(tenant_id)
    .bind(cart_id)
    .fetch_all(db)
    .await?)
}

/// A cart line with display name and list price in the cart currency
/// (`None` when no price exists for that cycle / TLD / years). Final pricing
/// (coupons, config options, taxes) is hos-billing's job.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CartLine {
    #[sqlx(flatten)]
    pub item: CartItemRow,
    pub product_name: Option<String>,
    pub addon_name: Option<String>,
    /// Matched TLD for domain lines (`com.tr`).
    pub tld: Option<String>,
    pub unit_price: Option<Decimal>,
    pub setup_fee: Option<Decimal>,
}

/// Lines of a cart with names and list prices (one query).
pub async fn cart_lines(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
) -> DbResult<Vec<CartLine>> {
    Ok(sqlx::query_as::<_, CartLine>(
        "select ci.*, p.name as product_name, a.name as addon_name, t.tld,
                case
                    when ci.item_type = 'product' then pp.price
                    when ci.item_type = 'addon' then ap.price
                    when ci.item_type like 'domain\\_%' then tp.price
                end as unit_price,
                case
                    when ci.item_type = 'product' then pp.setup_fee
                    when ci.item_type = 'addon' then ap.setup_fee
                    else 0
                end as setup_fee
         from cart_items ci
         join carts c on c.id = ci.cart_id
         left join products p on p.id = ci.product_id
         left join product_pricing pp
                on pp.product_id = ci.product_id and pp.currency = c.currency
               and pp.billing_cycle = ci.billing_cycle and pp.is_enabled
         left join product_addons a on a.id = ci.addon_id
         left join product_addon_pricing ap
                on ap.addon_id = ci.addon_id and ap.currency = c.currency and ap.billing_cycle = ci.billing_cycle
         left join lateral (
                select tl.id, tl.tld from tlds tl
                where ci.domain_name is not null and tl.tenant_id = ci.tenant_id and tl.is_active
                  and ci.domain_name::text like '%.' || tl.tld
                order by length(tl.tld) desc limit 1
         ) t on true
         left join tld_pricing tp
                on tp.tld_id = t.id and tp.currency = c.currency and tp.years = coalesce(ci.years, 1)
               and tp.action = case ci.item_type when 'domain_register' then 'register'
                                                 when 'domain_transfer' then 'transfer'
                                                 when 'domain_renew' then 'renew' end
         where ci.tenant_id = $1 and ci.cart_id = $2
         order by ci.created_at, ci.id",
    )
    .bind(tenant_id)
    .bind(cart_id)
    .fetch_all(db)
    .await?)
}

/// Marks a cart as converted into `order_id`.
pub async fn mark_converted(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    cart_id: CartId,
    order_id: OrderId,
) -> DbResult<bool> {
    let res =
        sqlx::query("update carts set converted_order_id = $3 where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(cart_id)
            .bind(order_id)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Deletes expired, unconverted carts (all tenants).
pub async fn purge_expired_carts(db: impl PgExecutor<'_>) -> DbResult<u64> {
    let res =
        sqlx::query("delete from carts where expires_at <= now() and converted_order_id is null")
            .execute(db)
            .await?;
    Ok(res.rows_affected())
}
