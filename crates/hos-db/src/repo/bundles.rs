//! Product bundles (`product_bundles`, `product_bundle_items`; migration
//! 0043). A bundle's discount is its own coupon with
//! `requires_all_products`, kept in step with the bundle's products and
//! billing cycles. Pricing reads the items ([`coupon_items`]) and discounts
//! one unit per item; the code only works through the bundle's order button
//! ([`is_bundle_coupon`] refuses it when typed in).

use chrono::{DateTime, Utc};
use hos_core::ids::{CouponId, ProductId, TenantId};
use hos_core::money::BillingCycle;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use crate::enums::DiscountType;
use crate::error::{DbResult, conflict_on_unique, not_found};

hos_core::define_id! {
    /// `product_bundles`.
    pub struct BundleId;
}

/// `product_bundles` row with its coupon terms.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct BundleRow {
    pub id: BundleId,
    pub tenant_id: TenantId,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub coupon_id: CouponId,
    pub is_active: bool,
    pub show_on_storefront: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub coupon_code: String,
    pub discount_type: DiscountType,
    pub discount_value: rust_decimal::Decimal,
    pub recurring: bool,
}

/// `product_bundle_items` row (+ product name and slugs for links).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct BundleItemRow {
    pub id: Uuid,
    pub bundle_id: BundleId,
    pub product_id: ProductId,
    pub billing_cycle: BillingCycle,
    pub sort_order: i32,
    pub product_name: String,
    pub product_slug: String,
    pub group_slug: Option<String>,
}

const ROW: &str = "select b.*, c.code as coupon_code, c.discount_type, c.value as discount_value,
        c.recurring
   from product_bundles b
   join coupons c on c.tenant_id = b.tenant_id and c.id = b.coupon_id";

/// Input of [`create`] / [`update`].
#[derive(Debug, Clone)]
pub struct BundleInput {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub is_active: bool,
    pub show_on_storefront: bool,
    pub sort_order: i32,
    pub discount_type: DiscountType,
    pub discount_value: rust_decimal::Decimal,
    /// Discount renewals too (else the first period only).
    pub recurring: bool,
    /// `(product, cycle)`, at least two, each product once.
    pub items: Vec<(ProductId, BillingCycle)>,
}

async fn write_items(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: BundleId,
    items: &[(ProductId, BillingCycle)],
) -> DbResult<()> {
    sqlx::query("delete from product_bundle_items where tenant_id = $1 and bundle_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for (i, (product, cycle)) in items.iter().enumerate() {
        sqlx::query(
            "insert into product_bundle_items (id, tenant_id, bundle_id, product_id, billing_cycle, sort_order)
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(id)
        .bind(product)
        .bind(cycle)
        .bind(i32::try_from(i).unwrap_or(i32::MAX))
        .execute(&mut *conn)
        .await
        .map_err(conflict_on_unique("bundle product"))?;
    }
    Ok(())
}

/// Distinct cycles of `items`, as stored in `coupons.billing_cycles`.
fn cycles_of(items: &[(ProductId, BillingCycle)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, c) in items {
        let c = c.as_str().to_owned();
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// Creates a bundle and its coupon (`code`: unguessable, from the caller).
pub async fn create(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    b: &BundleInput,
    code: &str,
) -> DbResult<BundleId> {
    let coupon_id = CouponId::new();
    let products: Vec<ProductId> = b.items.iter().map(|(p, _)| *p).collect();
    sqlx::query(
        "insert into coupons (id, tenant_id, code, description, discount_type, value, product_ids,
                              recurring, requires_all_products, is_active, billing_cycles)
         values ($1, $2, $3, $4, $5, $6, $7, $8, true, $9, $10)",
    )
    .bind(coupon_id)
    .bind(tenant_id)
    .bind(code)
    .bind(format!("Bundle: {}", b.name))
    .bind(b.discount_type)
    .bind(b.discount_value)
    .bind(&products)
    .bind(b.recurring)
    .bind(b.is_active)
    .bind(cycles_of(&b.items))
    .execute(&mut *conn)
    .await
    .map_err(conflict_on_unique("coupon code"))?;
    let id = BundleId::new();
    sqlx::query(
        "insert into product_bundles (id, tenant_id, slug, name, description, coupon_id, is_active,
                                      show_on_storefront, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&b.slug)
    .bind(&b.name)
    .bind(&b.description)
    .bind(coupon_id)
    .bind(b.is_active)
    .bind(b.show_on_storefront)
    .bind(b.sort_order)
    .execute(&mut *conn)
    .await
    .map_err(conflict_on_unique("bundle slug"))?;
    write_items(conn, tenant_id, id, &b.items).await?;
    Ok(id)
}

/// Updates a bundle, its items and its coupon terms.
pub async fn update(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: BundleId,
    b: &BundleInput,
) -> DbResult<()> {
    let coupon: CouponId = sqlx::query_scalar(
        "update product_bundles set slug = $3, name = $4, description = $5, is_active = $6,
                show_on_storefront = $7, sort_order = $8
          where tenant_id = $1 and id = $2 returning coupon_id",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&b.slug)
    .bind(&b.name)
    .bind(&b.description)
    .bind(b.is_active)
    .bind(b.show_on_storefront)
    .bind(b.sort_order)
    .fetch_optional(&mut *conn)
    .await
    .map_err(conflict_on_unique("bundle slug"))?
    .ok_or_else(|| not_found("bundle", id))?;
    let products: Vec<ProductId> = b.items.iter().map(|(p, _)| *p).collect();
    sqlx::query(
        "update coupons set description = $3, discount_type = $4, value = $5, product_ids = $6,
                recurring = $7, requires_all_products = true, is_active = $8, billing_cycles = $9
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(coupon)
    .bind(format!("Bundle: {}", b.name))
    .bind(b.discount_type)
    .bind(b.discount_value)
    .bind(&products)
    .bind(b.recurring)
    .bind(b.is_active)
    .bind(cycles_of(&b.items))
    .execute(&mut *conn)
    .await?;
    write_items(conn, tenant_id, id, &b.items).await
}

/// Deletes a bundle and its coupon.
pub async fn delete(conn: &mut PgConnection, tenant_id: TenantId, id: BundleId) -> DbResult<bool> {
    let Some(coupon): Option<CouponId> = sqlx::query_scalar(
        "delete from product_bundles where tenant_id = $1 and id = $2 returning coupon_id",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(false);
    };
    // Orders keep their coupon code; a used coupon is only deactivated.
    sqlx::query(
        "update coupons set is_active = false where tenant_id = $1 and id = $2 and uses_count > 0",
    )
    .bind(tenant_id)
    .bind(coupon)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "delete from coupons c where c.tenant_id = $1 and c.id = $2 and c.uses_count = 0
           and not exists(select 1 from orders o where o.tenant_id = c.tenant_id and o.coupon_id = c.id)",
    )
    .bind(tenant_id)
    .bind(coupon)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// Every bundle (staff).
pub async fn list(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<Vec<BundleRow>> {
    Ok(sqlx::query_as::<_, BundleRow>(&format!(
        "{ROW} where b.tenant_id = $1 order by b.sort_order, b.name"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Active bundles shown on the storefront.
pub async fn storefront(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<Vec<BundleRow>> {
    Ok(sqlx::query_as::<_, BundleRow>(&format!(
        "{ROW} where b.tenant_id = $1 and b.is_active and b.show_on_storefront and c.is_active
          order by b.sort_order, b.name"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// One bundle.
pub async fn get(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: BundleId,
) -> DbResult<Option<BundleRow>> {
    Ok(
        sqlx::query_as::<_, BundleRow>(&format!("{ROW} where b.tenant_id = $1 and b.id = $2"))
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// One bundle by slug.
pub async fn by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
) -> DbResult<Option<BundleRow>> {
    Ok(
        sqlx::query_as::<_, BundleRow>(&format!("{ROW} where b.tenant_id = $1 and b.slug = $2"))
            .bind(tenant_id)
            .bind(slug)
            .fetch_optional(db)
            .await?,
    )
}

/// Items of a bundle, in order.
pub async fn items(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: BundleId,
) -> DbResult<Vec<BundleItemRow>> {
    Ok(sqlx::query_as::<_, BundleItemRow>(
        "select i.id, i.bundle_id, i.product_id, i.billing_cycle, i.sort_order,
                p.name as product_name, p.slug as product_slug, g.slug as group_slug
           from product_bundle_items i
           join products p on p.tenant_id = i.tenant_id and p.id = i.product_id
           left join product_groups g on g.id = p.group_id
          where i.tenant_id = $1 and i.bundle_id = $2
          order by i.sort_order",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// `(product, cycle)` items of the bundle owning `coupon_id`, in order
/// (empty when the coupon is not a bundle's).
pub async fn coupon_items(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    coupon_id: CouponId,
) -> DbResult<Vec<(ProductId, BillingCycle)>> {
    Ok(sqlx::query_as::<_, (ProductId, BillingCycle)>(
        "select i.product_id, i.billing_cycle
           from product_bundle_items i
           join product_bundles b on b.tenant_id = i.tenant_id and b.id = i.bundle_id
          where i.tenant_id = $1 and b.coupon_id = $2
          order by i.sort_order",
    )
    .bind(tenant_id)
    .bind(coupon_id)
    .fetch_all(db)
    .await?)
}

/// Whether `coupon_id` is a bundle's coupon (usable only through the
/// bundle's order button, never typed in).
pub async fn is_bundle_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    coupon_id: CouponId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar(
        "select exists(select 1 from product_bundles where tenant_id = $1 and coupon_id = $2)",
    )
    .bind(tenant_id)
    .bind(coupon_id)
    .fetch_one(db)
    .await?)
}
