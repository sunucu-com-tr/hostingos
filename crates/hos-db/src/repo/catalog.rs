//! Product catalog: currencies, tax rules, product groups, products and their
//! pricing, configurable options, addons and coupons.

use chrono::{DateTime, Utc};
use hos_core::domain::ProductType;
use hos_core::ids::{
    ConfigOptionGroupId, ConfigOptionId, CouponId, IntegrationId, ProductAddonId, ProductGroupId,
    ProductId, TaxRuleId, TenantId,
};
use hos_core::money::{BillingCycle, Currency};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, FromRow, PgExecutor, Postgres};

use super::{Json, empty_object};
use crate::enums::{AutoSetup, ConfigOptionType, DiscountType, DomainRequirement, PayType};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::ids::{
    ConfigOptionPricingId, ConfigOptionValueId, CurrencyId, ProductAddonPricingId, ProductPricingId,
};
use crate::page::{Page, Paged, fetch_paged};

// ---------------------------------------------------------------------------
// currencies
// ---------------------------------------------------------------------------

/// `currencies` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CurrencyRow {
    pub id: CurrencyId,
    pub tenant_id: TenantId,
    pub code: Currency,
    pub name: String,
    pub symbol: String,
    pub prefix: String,
    pub suffix: String,
    pub decimals: i16,
    pub exchange_rate: Decimal,
    pub is_default: bool,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Currency definition for [`upsert_currency`].
#[derive(Debug, Clone)]
pub struct CurrencyInput {
    pub code: Currency,
    pub name: String,
    pub symbol: String,
    pub prefix: String,
    pub suffix: String,
    pub decimals: i16,
    pub exchange_rate: Decimal,
    pub is_default: bool,
    pub sort_order: i32,
}

/// Inserts or updates a currency by code. Making it default is the caller's
/// job via [`set_default_currency`] (the partial unique index allows one).
pub async fn upsert_currency(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    c: &CurrencyInput,
) -> DbResult<CurrencyRow> {
    sqlx::query_as::<_, CurrencyRow>(
        "insert into currencies (id, tenant_id, code, name, symbol, prefix, suffix, decimals, exchange_rate,
                                 is_default, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         on conflict (tenant_id, code) do update
            set name = excluded.name, symbol = excluded.symbol, prefix = excluded.prefix, suffix = excluded.suffix,
                decimals = excluded.decimals, exchange_rate = excluded.exchange_rate, sort_order = excluded.sort_order
         returning *",
    )
    .bind(CurrencyId::new())
    .bind(tenant_id)
    .bind(c.code)
    .bind(&c.name)
    .bind(&c.symbol)
    .bind(&c.prefix)
    .bind(&c.suffix)
    .bind(c.decimals)
    .bind(c.exchange_rate)
    .bind(c.is_default)
    .bind(c.sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("default currency"))
}

/// Makes `code` the tenant's only default currency (one statement).
pub async fn set_default_currency(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    code: Currency,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update currencies set is_default = (code = $2)
         where tenant_id = $1 and (is_default or code = $2)",
    )
    .bind(tenant_id)
    .bind(code)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Currencies of a tenant (default first).
pub async fn list_currencies(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    active_only: bool,
) -> DbResult<Vec<CurrencyRow>> {
    Ok(sqlx::query_as::<_, CurrencyRow>(
        "select * from currencies where tenant_id = $1 and (not $2 or is_active)
         order by is_default desc, sort_order, code",
    )
    .bind(tenant_id)
    .bind(active_only)
    .fetch_all(db)
    .await?)
}

/// One currency by code.
pub async fn get_currency(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    code: Currency,
) -> DbResult<Option<CurrencyRow>> {
    Ok(sqlx::query_as::<_, CurrencyRow>(
        "select * from currencies where tenant_id = $1 and code = $2",
    )
    .bind(tenant_id)
    .bind(code)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// tax rules
// ---------------------------------------------------------------------------

/// `tax_rules` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TaxRuleRow {
    pub id: TaxRuleId,
    pub tenant_id: TenantId,
    pub name: String,
    pub level: i16,
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub rate: Decimal,
    pub is_compound: bool,
    pub is_active: bool,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a tax rule.
#[allow(clippy::too_many_arguments)]
pub async fn create_tax_rule(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    name: &str,
    level: i16,
    country_code: Option<&str>,
    region: Option<&str>,
    rate: Decimal,
    is_compound: bool,
) -> DbResult<TaxRuleRow> {
    Ok(sqlx::query_as::<_, TaxRuleRow>(
        "insert into tax_rules (id, tenant_id, name, level, country_code, region, rate, is_compound)
         values ($1, $2, $3, $4, upper($5), $6, $7, $8) returning *",
    )
    .bind(TaxRuleId::new())
    .bind(tenant_id)
    .bind(name)
    .bind(level)
    .bind(country_code)
    .bind(region)
    .bind(rate)
    .bind(is_compound)
    .fetch_one(db)
    .await?)
}

/// All tax rules of a tenant.
pub async fn list_tax_rules(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<TaxRuleRow>> {
    Ok(sqlx::query_as::<_, TaxRuleRow>(
        "select * from tax_rules where tenant_id = $1 order by level, country_code nulls last, priority desc",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// The rules applying to a billing address: at most one per level, the most
/// specific match winning (country + region > country > everywhere), ordered
/// by level. Feed them to `invoices::set_tax_lines`.
pub async fn applicable_tax_rules(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    country_code: &str,
    region: Option<&str>,
) -> DbResult<Vec<TaxRuleRow>> {
    Ok(sqlx::query_as::<_, TaxRuleRow>(
        "select distinct on (level) * from tax_rules
         where tenant_id = $1 and is_active
           and (country_code is null or country_code = upper($2))
           and (region is null or lower(region) = lower($3))
         order by level, (region is not null) desc, (country_code is not null) desc, priority desc",
    )
    .bind(tenant_id)
    .bind(country_code)
    .bind(region)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// product groups
// ---------------------------------------------------------------------------

/// `product_groups` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ProductGroupRow {
    pub id: ProductGroupId,
    pub tenant_id: TenantId,
    pub slug: String,
    pub name: String,
    pub tagline: Option<String>,
    pub description: Option<String>,
    /// `{"en": {"name": .., "tagline": .., "description": ..}}`
    pub translations: Json,
    pub icon: Option<String>,
    pub order_template: String,
    pub is_hidden: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_product_group`].
#[derive(Debug, Clone)]
pub struct ProductGroupInput {
    pub slug: String,
    pub name: String,
    pub tagline: Option<String>,
    pub description: Option<String>,
    pub translations: Json,
    pub icon: Option<String>,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Creates a product group.
pub async fn create_product_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    g: &ProductGroupInput,
) -> DbResult<ProductGroupRow> {
    sqlx::query_as::<_, ProductGroupRow>(
        "insert into product_groups (id, tenant_id, slug, name, tagline, description, translations, icon,
                                     is_hidden, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning *",
    )
    .bind(ProductGroupId::new())
    .bind(tenant_id)
    .bind(&g.slug)
    .bind(&g.name)
    .bind(&g.tagline)
    .bind(&g.description)
    .bind(&g.translations)
    .bind(&g.icon)
    .bind(g.is_hidden)
    .bind(g.sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("product group slug"))
}

/// Product groups in display order.
pub async fn list_product_groups(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    include_hidden: bool,
) -> DbResult<Vec<ProductGroupRow>> {
    Ok(sqlx::query_as::<_, ProductGroupRow>(
        "select * from product_groups where tenant_id = $1 and ($2 or not is_hidden) order by sort_order, name",
    )
    .bind(tenant_id)
    .bind(include_hidden)
    .fetch_all(db)
    .await?)
}

/// Product group by slug.
pub async fn product_group_by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
) -> DbResult<Option<ProductGroupRow>> {
    Ok(sqlx::query_as::<_, ProductGroupRow>(
        "select * from product_groups where tenant_id = $1 and slug = $2",
    )
    .bind(tenant_id)
    .bind(slug)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// products & pricing
// ---------------------------------------------------------------------------

/// `products` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ProductRow {
    pub id: ProductId,
    pub tenant_id: TenantId,
    pub group_id: Option<ProductGroupId>,
    pub slug: String,
    pub product_type: ProductType,
    pub name: String,
    pub tagline: Option<String>,
    pub description: Option<String>,
    /// `["10 GB NVMe", ...]`
    pub features: Json,
    pub translations: Json,
    pub integration_id: Option<IntegrationId>,
    pub module_config: Json,
    pub pay_type: PayType,
    pub auto_setup: AutoSetup,
    pub requires_domain: DomainRequirement,
    pub welcome_email_template: Option<String>,
    pub stock_control: bool,
    pub stock_quantity: i32,
    pub allow_quantity: bool,
    pub is_taxable: bool,
    pub is_featured: bool,
    pub is_hidden: bool,
    pub is_retired: bool,
    pub upgrade_product_ids: Vec<ProductId>,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ProductRow {
    /// Orderable on the storefront (visible, not retired, in stock).
    pub fn is_orderable(&self) -> bool {
        !self.is_hidden && !self.is_retired && (!self.stock_control || self.stock_quantity > 0)
    }
}

/// Input of [`create_product`] / [`update_product`].
#[derive(Debug, Clone)]
pub struct ProductInput {
    pub group_id: Option<ProductGroupId>,
    pub slug: String,
    pub product_type: ProductType,
    pub name: String,
    pub tagline: Option<String>,
    pub description: Option<String>,
    pub features: Json,
    pub translations: Json,
    pub integration_id: Option<IntegrationId>,
    pub module_config: Json,
    pub pay_type: PayType,
    pub auto_setup: AutoSetup,
    pub requires_domain: DomainRequirement,
    pub welcome_email_template: Option<String>,
    pub is_taxable: bool,
    pub is_featured: bool,
    pub is_hidden: bool,
    pub sort_order: i32,
}

impl ProductInput {
    /// Recurring, taxable, visible product with empty JSON fields.
    pub fn new(
        slug: &str,
        name: &str,
        product_type: ProductType,
        group_id: Option<ProductGroupId>,
    ) -> Self {
        Self {
            group_id,
            slug: slug.to_owned(),
            product_type,
            name: name.to_owned(),
            tagline: None,
            description: None,
            features: Json::Array(Vec::new()),
            translations: empty_object(),
            integration_id: None,
            module_config: empty_object(),
            pay_type: PayType::Recurring,
            auto_setup: AutoSetup::OnPayment,
            requires_domain: DomainRequirement::None,
            welcome_email_template: None,
            is_taxable: true,
            is_featured: false,
            is_hidden: false,
            sort_order: 0,
        }
    }
}

/// Creates a product.
pub async fn create_product(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    p: &ProductInput,
) -> DbResult<ProductRow> {
    sqlx::query_as::<_, ProductRow>(
        "insert into products (id, tenant_id, group_id, slug, product_type, name, tagline, description, features,
                               translations, integration_id, module_config, pay_type, auto_setup, requires_domain,
                               welcome_email_template, is_taxable, is_featured, is_hidden, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
         returning *",
    )
    .bind(ProductId::new())
    .bind(tenant_id)
    .bind(p.group_id)
    .bind(&p.slug)
    .bind(p.product_type)
    .bind(&p.name)
    .bind(&p.tagline)
    .bind(&p.description)
    .bind(&p.features)
    .bind(&p.translations)
    .bind(p.integration_id)
    .bind(&p.module_config)
    .bind(p.pay_type)
    .bind(p.auto_setup)
    .bind(p.requires_domain)
    .bind(&p.welcome_email_template)
    .bind(p.is_taxable)
    .bind(p.is_featured)
    .bind(p.is_hidden)
    .bind(p.sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("product slug"))
}

/// Updates a product.
pub async fn update_product(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductId,
    p: &ProductInput,
) -> DbResult<Option<ProductRow>> {
    sqlx::query_as::<_, ProductRow>(
        "update products set group_id = $3, slug = $4, product_type = $5, name = $6, tagline = $7,
                description = $8, features = $9, translations = $10, integration_id = $11, module_config = $12,
                pay_type = $13, auto_setup = $14, requires_domain = $15, welcome_email_template = $16,
                is_taxable = $17, is_featured = $18, is_hidden = $19, sort_order = $20
         where tenant_id = $1 and id = $2
         returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(p.group_id)
    .bind(&p.slug)
    .bind(p.product_type)
    .bind(&p.name)
    .bind(&p.tagline)
    .bind(&p.description)
    .bind(&p.features)
    .bind(&p.translations)
    .bind(p.integration_id)
    .bind(&p.module_config)
    .bind(p.pay_type)
    .bind(p.auto_setup)
    .bind(p.requires_domain)
    .bind(&p.welcome_email_template)
    .bind(p.is_taxable)
    .bind(p.is_featured)
    .bind(p.is_hidden)
    .bind(p.sort_order)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("product slug"))
}

/// Product by id.
pub async fn get_product(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductId,
) -> DbResult<Option<ProductRow>> {
    Ok(
        sqlx::query_as::<_, ProductRow>("select * from products where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Product by slug.
pub async fn product_by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
) -> DbResult<Option<ProductRow>> {
    Ok(
        sqlx::query_as::<_, ProductRow>(
            "select * from products where tenant_id = $1 and slug = $2",
        )
        .bind(tenant_id)
        .bind(slug)
        .fetch_optional(db)
        .await?,
    )
}

/// Filters of [`list_products`].
#[derive(Debug, Clone, Default)]
pub struct ProductFilter {
    pub group_id: Option<ProductGroupId>,
    pub product_type: Option<ProductType>,
    pub search: Option<String>,
    pub include_retired: bool,
}

/// Admin product list.
pub async fn list_products<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ProductFilter,
    page: Page,
) -> DbResult<Paged<ProductRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        "select p.*",
        |qb| {
            qb.push("from products p where p.tenant_id = ")
                .push_bind(tenant_id);
            if let Some(g) = f.group_id {
                qb.push(" and p.group_id = ").push_bind(g);
            }
            if let Some(t) = f.product_type {
                qb.push(" and p.product_type = ").push_bind(t);
            }
            if !f.include_retired {
                qb.push(" and not p.is_retired");
            }
            if let Some(s) = &search {
                qb.push(" and (p.name ilike ")
                    .push_bind(s.clone())
                    .push(" or p.slug ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        "p.sort_order, p.name, p.id",
        page,
    )
    .await
}

/// `product_pricing` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ProductPricingRow {
    pub id: ProductPricingId,
    pub tenant_id: TenantId,
    pub product_id: ProductId,
    pub currency: Currency,
    pub billing_cycle: BillingCycle,
    pub price: Decimal,
    pub setup_fee: Decimal,
    pub is_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Sets (inserts or replaces) one price point.
pub async fn set_product_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    currency: Currency,
    cycle: BillingCycle,
    price: Decimal,
    setup_fee: Decimal,
) -> DbResult<ProductPricingRow> {
    sqlx::query_as::<_, ProductPricingRow>(
        "insert into product_pricing (id, tenant_id, product_id, currency, billing_cycle, price, setup_fee)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (product_id, currency, billing_cycle) do update
            set price = excluded.price, setup_fee = excluded.setup_fee, is_enabled = true
         returning *",
    )
    .bind(ProductPricingId::new())
    .bind(tenant_id)
    .bind(product_id)
    .bind(currency)
    .bind(cycle)
    .bind(price)
    .bind(setup_fee)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("price"))
}

/// Disables one price point (the cycle disappears from the order form).
pub async fn disable_product_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    currency: Currency,
    cycle: BillingCycle,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update product_pricing set is_enabled = false
         where tenant_id = $1 and product_id = $2 and currency = $3 and billing_cycle = $4",
    )
    .bind(tenant_id)
    .bind(product_id)
    .bind(currency)
    .bind(cycle)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// All price points of a product.
pub async fn product_pricing(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
) -> DbResult<Vec<ProductPricingRow>> {
    Ok(sqlx::query_as::<_, ProductPricingRow>(
        "select * from product_pricing where tenant_id = $1 and product_id = $2
         order by currency, array_position(array['monthly','quarterly','semiannual','annual','biennial','triennial','one_time'], billing_cycle)",
    )
    .bind(tenant_id)
    .bind(product_id)
    .fetch_all(db)
    .await?)
}

/// The enabled price for one currency and cycle.
pub async fn product_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    currency: Currency,
    cycle: BillingCycle,
) -> DbResult<Option<ProductPricingRow>> {
    Ok(sqlx::query_as::<_, ProductPricingRow>(
        "select * from product_pricing
         where tenant_id = $1 and product_id = $2 and currency = $3 and billing_cycle = $4 and is_enabled",
    )
    .bind(tenant_id)
    .bind(product_id)
    .bind(currency)
    .bind(cycle)
    .fetch_optional(db)
    .await?)
}

/// One price point in a catalog tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceTag {
    pub billing_cycle: BillingCycle,
    pub price: Decimal,
    pub setup_fee: Decimal,
}

/// A product with its enabled prices in one currency (cycle order).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CatalogProduct {
    #[sqlx(flatten)]
    pub product: ProductRow,
    #[sqlx(json)]
    pub prices: Vec<PriceTag>,
}

impl CatalogProduct {
    /// The cheapest per-month equivalent price ("from ₺89.90/mo").
    pub fn starting_monthly(&self) -> Option<Decimal> {
        self.prices
            .iter()
            .filter_map(|p| p.billing_cycle.months().map(|m| p.price / Decimal::from(m)))
            .min()
    }
}

/// A product group with its products.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogGroup {
    pub group: ProductGroupRow,
    pub products: Vec<CatalogProduct>,
}

const CATALOG_PRODUCT_SELECT: &str = "select p.*,
        coalesce((select json_agg(json_build_object(
                     'billing_cycle', x.billing_cycle, 'price', x.price::text, 'setup_fee', x.setup_fee::text)
                   order by array_position(array['monthly','quarterly','semiannual','annual','biennial',
                                                 'triennial','one_time'], x.billing_cycle))
                  from product_pricing x
                  where x.product_id = p.id and x.currency = $2 and x.is_enabled), '[]'::json) as prices
     from products p";

/// The storefront catalog in one call: visible groups → orderable products →
/// enabled prices in `currency` (two queries on one connection). Groups
/// without visible products are omitted unless `include_hidden`.
pub async fn catalog<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    currency: Currency,
    include_hidden: bool,
) -> DbResult<Vec<CatalogGroup>> {
    let mut conn = db.acquire().await?;
    let groups = list_product_groups(&mut *conn, tenant_id, include_hidden).await?;
    let sql = format!(
        "{CATALOG_PRODUCT_SELECT}
         where p.tenant_id = $1 and p.group_id is not null
           and ($3 or (not p.is_hidden and not p.is_retired))
         order by p.sort_order, p.name"
    );
    let products = sqlx::query_as::<_, CatalogProduct>(&sql)
        .bind(tenant_id)
        .bind(currency)
        .bind(include_hidden)
        .fetch_all(&mut *conn)
        .await?;
    let mut out: Vec<CatalogGroup> = groups
        .into_iter()
        .map(|group| CatalogGroup {
            group,
            products: Vec::new(),
        })
        .collect();
    for p in products {
        if let Some(g) = out
            .iter_mut()
            .find(|g| Some(g.group.id) == p.product.group_id)
        {
            g.products.push(p);
        }
    }
    if !include_hidden {
        out.retain(|g| !g.products.is_empty());
    }
    Ok(out)
}

/// One product (by slug) with its prices in `currency`.
pub async fn catalog_product_by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
    currency: Currency,
) -> DbResult<Option<CatalogProduct>> {
    let sql = format!("{CATALOG_PRODUCT_SELECT} where p.tenant_id = $1 and p.slug = $3");
    Ok(sqlx::query_as::<_, CatalogProduct>(&sql)
        .bind(tenant_id)
        .bind(currency)
        .bind(slug)
        .fetch_optional(db)
        .await?)
}

/// Featured, orderable products (home page), with prices in `currency`.
pub async fn featured_products(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    currency: Currency,
    limit: i64,
) -> DbResult<Vec<CatalogProduct>> {
    let sql = format!(
        "{CATALOG_PRODUCT_SELECT}
         where p.tenant_id = $1 and p.is_featured and not p.is_hidden and not p.is_retired
         order by p.sort_order, p.name limit $3"
    );
    Ok(sqlx::query_as::<_, CatalogProduct>(&sql)
        .bind(tenant_id)
        .bind(currency)
        .bind(limit)
        .fetch_all(db)
        .await?)
}

/// Decrements stock for stock-controlled products; `false` if out of stock.
pub async fn take_stock(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    qty: i32,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update products set stock_quantity = stock_quantity - $3
         where tenant_id = $1 and id = $2 and (not stock_control or stock_quantity >= $3)",
    )
    .bind(tenant_id)
    .bind(product_id)
    .bind(qty)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// configurable options
// ---------------------------------------------------------------------------

/// `config_options` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ConfigOptionRow {
    pub id: ConfigOptionId,
    pub tenant_id: TenantId,
    pub group_id: ConfigOptionGroupId,
    pub name: String,
    pub translations: Json,
    pub option_type: ConfigOptionType,
    pub module_key: Option<String>,
    pub unit: Option<String>,
    pub min_quantity: i32,
    pub max_quantity: i32,
    pub is_hidden: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A selectable value with its prices (all cycles) in one currency.
///
/// Option values are priced per currency without conversion. A cycle with
/// no price row in any currency is free; a cycle that another currency
/// prices (non-zero price or setup fee) but this one does not is listed in
/// `unpriced`: the value cannot be ordered for that cycle in this currency
/// (it must never fall back to free).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValueTag {
    pub id: ConfigOptionValueId,
    pub label: String,
    pub value: String,
    pub translations: Json,
    pub prices: Vec<PriceTag>,
    /// Cycles priced in another currency but not in this one.
    #[serde(default)]
    pub unpriced: Vec<BillingCycle>,
}

impl ConfigValueTag {
    /// The price for `cycle` in this currency: `Ok(None)` when no currency
    /// prices the cycle (free), `Err(())` when only another currency does.
    #[allow(clippy::result_unit_err)]
    pub fn price_for(&self, cycle: BillingCycle) -> Result<Option<&PriceTag>, ()> {
        match self.prices.iter().find(|p| p.billing_cycle == cycle) {
            Some(p) => Ok(Some(p)),
            None if self.unpriced.contains(&cycle) => Err(()),
            None => Ok(None),
        }
    }

    /// Whether the value can be offered in this currency: false when it has
    /// no price here at all but is priced in another currency.
    pub fn offered(&self) -> bool {
        !(self.prices.is_empty() && !self.unpriced.is_empty())
    }
}

/// A configurable option of a product with its values.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ProductConfigOption {
    #[sqlx(flatten)]
    pub option: ConfigOptionRow,
    #[sqlx(json)]
    pub values: Vec<ConfigValueTag>,
}

/// Creates a config option group.
pub async fn create_config_option_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    name: &str,
    description: Option<&str>,
) -> DbResult<ConfigOptionGroupId> {
    let id = ConfigOptionGroupId::new();
    sqlx::query("insert into config_option_groups (id, tenant_id, name, description) values ($1, $2, $3, $4)")
        .bind(id)
        .bind(tenant_id)
        .bind(name)
        .bind(description)
        .execute(db)
        .await
        .map_err(conflict_on_unique("config option group"))?;
    Ok(id)
}

/// Attaches a config option group to a product (idempotent).
pub async fn attach_config_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    group_id: ConfigOptionGroupId,
    product_id: ProductId,
) -> DbResult<()> {
    sqlx::query(
        "insert into config_option_group_products (id, tenant_id, group_id, product_id) values ($1, $2, $3, $4)
         on conflict (group_id, product_id) do nothing",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant_id)
    .bind(group_id)
    .bind(product_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Creates a config option.
#[allow(clippy::too_many_arguments)]
pub async fn create_config_option(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    group_id: ConfigOptionGroupId,
    name: &str,
    option_type: ConfigOptionType,
    module_key: Option<&str>,
    translations: &Json,
    sort_order: i32,
) -> DbResult<ConfigOptionId> {
    let id = ConfigOptionId::new();
    sqlx::query(
        "insert into config_options (id, tenant_id, group_id, name, option_type, module_key, translations, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(group_id)
    .bind(name)
    .bind(option_type)
    .bind(module_key)
    .bind(translations)
    .bind(sort_order)
    .execute(db)
    .await?;
    Ok(id)
}

/// Adds a value to a config option.
pub async fn create_config_option_value(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    option_id: ConfigOptionId,
    label: &str,
    value: &str,
    translations: &Json,
    sort_order: i32,
) -> DbResult<ConfigOptionValueId> {
    let id = ConfigOptionValueId::new();
    sqlx::query(
        "insert into config_option_values (id, tenant_id, option_id, label, value, translations, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(option_id)
    .bind(label)
    .bind(value)
    .bind(translations)
    .bind(sort_order)
    .execute(db)
    .await?;
    Ok(id)
}

/// Sets the price of a config option value for one currency / cycle.
pub async fn set_config_option_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    value_id: ConfigOptionValueId,
    currency: Currency,
    cycle: BillingCycle,
    price: Decimal,
    setup_fee: Decimal,
) -> DbResult<()> {
    sqlx::query(
        "insert into config_option_pricing (id, tenant_id, value_id, currency, billing_cycle, price, setup_fee)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (value_id, currency, billing_cycle) do update
            set price = excluded.price, setup_fee = excluded.setup_fee",
    )
    .bind(ConfigOptionPricingId::new())
    .bind(tenant_id)
    .bind(value_id)
    .bind(currency)
    .bind(cycle)
    .bind(price)
    .bind(setup_fee)
    .execute(db)
    .await?;
    Ok(())
}

/// Values (trimmed, non-empty, hidden ones included) of a product's
/// choice-type config options (`dropdown`, `radio`, `yes_no`) whose
/// `module_key` is one of `module_keys` (compared case-insensitively).
/// Quantity options are left out: their module value is derived from a
/// client-chosen quantity.
pub async fn product_option_values_for_keys(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    module_keys: &[&str],
) -> DbResult<Vec<String>> {
    let keys: Vec<String> = module_keys
        .iter()
        .map(|k| k.trim().to_ascii_lowercase())
        .collect();
    Ok(sqlx::query_scalar::<_, String>(
        "select distinct btrim(v.value)
         from config_option_group_products gp
         join config_options o on o.tenant_id = gp.tenant_id and o.group_id = gp.group_id
         join config_option_values v on v.tenant_id = o.tenant_id and v.option_id = o.id
         where gp.tenant_id = $1 and gp.product_id = $2
           and o.option_type <> 'quantity'
           and lower(btrim(o.module_key)) = any($3)
           and btrim(v.value) <> ''",
    )
    .bind(tenant_id)
    .bind(product_id)
    .bind(&keys)
    .fetch_all(db)
    .await?)
}

/// Visible config options of a product with their values and prices in
/// `currency` (and, per value, the cycles only other currencies price: see
/// [`ConfigValueTag`]).
pub async fn product_config_options(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    currency: Currency,
) -> DbResult<Vec<ProductConfigOption>> {
    Ok(sqlx::query_as::<_, ProductConfigOption>(
        "select o.*,
                coalesce((select json_agg(json_build_object(
                             'id', v.id, 'label', v.label, 'value', v.value, 'translations', v.translations,
                             'prices', coalesce((select json_agg(json_build_object(
                                          'billing_cycle', cp.billing_cycle, 'price', cp.price::text,
                                          'setup_fee', cp.setup_fee::text))
                                        from config_option_pricing cp
                                        where cp.value_id = v.id and cp.currency = $3), '[]'::json),
                             'unpriced', coalesce((select json_agg(distinct op.billing_cycle)
                                        from config_option_pricing op
                                        where op.value_id = v.id and op.currency <> $3
                                          and (op.price <> 0 or op.setup_fee <> 0)
                                          and not exists (
                                              select 1 from config_option_pricing hp
                                              where hp.value_id = v.id and hp.currency = $3
                                                and hp.billing_cycle = op.billing_cycle)), '[]'::json))
                           order by v.sort_order, v.label)
                          from config_option_values v
                          where v.option_id = o.id and not v.is_hidden), '[]'::json) as values
         from config_option_group_products gp
         join config_options o on o.group_id = gp.group_id
         where gp.tenant_id = $1 and gp.product_id = $2 and not o.is_hidden
         order by o.sort_order, o.name",
    )
    .bind(tenant_id)
    .bind(product_id)
    .bind(currency)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// addons
// ---------------------------------------------------------------------------

/// `product_addons` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ProductAddonRow {
    pub id: ProductAddonId,
    pub tenant_id: TenantId,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub translations: Json,
    pub integration_id: Option<IntegrationId>,
    pub module_config: Json,
    pub pay_type: PayType,
    pub auto_activate: bool,
    pub show_on_order: bool,
    pub welcome_email_template: Option<String>,
    pub is_taxable: bool,
    pub is_hidden: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An addon with its prices in one currency.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CatalogAddon {
    #[sqlx(flatten)]
    pub addon: ProductAddonRow,
    #[sqlx(json)]
    pub prices: Vec<PriceTag>,
}

/// Creates an addon.
pub async fn create_addon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
    name: &str,
    description: Option<&str>,
    translations: &Json,
    pay_type: PayType,
) -> DbResult<ProductAddonRow> {
    sqlx::query_as::<_, ProductAddonRow>(
        "insert into product_addons (id, tenant_id, slug, name, description, translations, pay_type)
         values ($1, $2, $3, $4, $5, $6, $7) returning *",
    )
    .bind(ProductAddonId::new())
    .bind(tenant_id)
    .bind(slug)
    .bind(name)
    .bind(description)
    .bind(translations)
    .bind(pay_type)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("addon slug"))
}

/// Makes an addon available for a product (idempotent).
pub async fn attach_addon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    addon_id: ProductAddonId,
    product_id: ProductId,
) -> DbResult<()> {
    sqlx::query(
        "insert into product_addon_products (id, tenant_id, addon_id, product_id) values ($1, $2, $3, $4)
         on conflict (addon_id, product_id) do nothing",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(tenant_id)
    .bind(addon_id)
    .bind(product_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Sets an addon price point.
pub async fn set_addon_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    addon_id: ProductAddonId,
    currency: Currency,
    cycle: BillingCycle,
    price: Decimal,
    setup_fee: Decimal,
) -> DbResult<()> {
    sqlx::query(
        "insert into product_addon_pricing (id, tenant_id, addon_id, currency, billing_cycle, price, setup_fee)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (addon_id, currency, billing_cycle) do update
            set price = excluded.price, setup_fee = excluded.setup_fee",
    )
    .bind(ProductAddonPricingId::new())
    .bind(tenant_id)
    .bind(addon_id)
    .bind(currency)
    .bind(cycle)
    .bind(price)
    .bind(setup_fee)
    .execute(db)
    .await?;
    Ok(())
}

/// Orderable addons of a product with prices in `currency`.
pub async fn addons_for_product(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    currency: Currency,
) -> DbResult<Vec<CatalogAddon>> {
    Ok(sqlx::query_as::<_, CatalogAddon>(
        "select a.*,
                coalesce((select json_agg(json_build_object(
                             'billing_cycle', x.billing_cycle, 'price', x.price::text, 'setup_fee', x.setup_fee::text))
                          from product_addon_pricing x where x.addon_id = a.id and x.currency = $3), '[]'::json) as prices
         from product_addon_products ap
         join product_addons a on a.id = ap.addon_id
         where ap.tenant_id = $1 and ap.product_id = $2 and not a.is_hidden and a.show_on_order
         order by a.sort_order, a.name",
    )
    .bind(tenant_id)
    .bind(product_id)
    .bind(currency)
    .fetch_all(db)
    .await?)
}

/// Addon by id.
pub async fn get_addon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ProductAddonId,
) -> DbResult<Option<ProductAddonRow>> {
    Ok(sqlx::query_as::<_, ProductAddonRow>(
        "select * from product_addons where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Addon price for one currency / cycle: `(price, setup_fee)`.
pub async fn addon_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    addon_id: ProductAddonId,
    currency: Currency,
    cycle: BillingCycle,
) -> DbResult<Option<(Decimal, Decimal)>> {
    Ok(sqlx::query_as::<_, (Decimal, Decimal)>(
        "select price, setup_fee from product_addon_pricing
         where tenant_id = $1 and addon_id = $2 and currency = $3 and billing_cycle = $4",
    )
    .bind(tenant_id)
    .bind(addon_id)
    .bind(currency)
    .bind(cycle)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// coupons
// ---------------------------------------------------------------------------

/// `coupons` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CouponRow {
    pub id: CouponId,
    pub tenant_id: TenantId,
    pub code: String,
    pub description: Option<String>,
    pub discount_type: DiscountType,
    pub value: Decimal,
    pub currency: Option<Currency>,
    pub product_ids: Vec<ProductId>,
    pub addon_ids: Vec<ProductAddonId>,
    pub tlds: Vec<String>,
    pub billing_cycles: Vec<String>,
    pub applies_to_setup: bool,
    pub recurring: bool,
    pub recurring_limit: i32,
    pub new_clients_only: bool,
    pub once_per_client: bool,
    pub min_order_amount: Option<Decimal>,
    pub max_uses: Option<i32>,
    pub uses_count: i32,
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Applies only when every product of `product_ids` is ordered
    /// (product bundles, migration 0043).
    #[sqlx(default)]
    pub requires_all_products: bool,
}

impl CouponRow {
    /// Whether the coupon restricts products and `product_id` is not allowed.
    pub fn applies_to_product(&self, product_id: ProductId) -> bool {
        self.product_ids.is_empty() || self.product_ids.contains(&product_id)
    }
}

/// Input of [`create_coupon`].
#[derive(Debug, Clone)]
pub struct CouponInput {
    pub code: String,
    pub description: Option<String>,
    pub discount_type: DiscountType,
    pub value: Decimal,
    pub currency: Option<Currency>,
    pub product_ids: Vec<ProductId>,
    pub recurring: bool,
    pub new_clients_only: bool,
    pub once_per_client: bool,
    pub max_uses: Option<i32>,
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Creates a coupon.
pub async fn create_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    c: &CouponInput,
) -> DbResult<CouponRow> {
    sqlx::query_as::<_, CouponRow>(
        "insert into coupons (id, tenant_id, code, description, discount_type, value, currency, product_ids,
                              recurring, new_clients_only, once_per_client, max_uses, starts_at, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) returning *",
    )
    .bind(CouponId::new())
    .bind(tenant_id)
    .bind(c.code.trim())
    .bind(&c.description)
    .bind(c.discount_type)
    .bind(c.value)
    .bind(c.currency)
    .bind(&c.product_ids)
    .bind(c.recurring)
    .bind(c.new_clients_only)
    .bind(c.once_per_client)
    .bind(c.max_uses)
    .bind(c.starts_at)
    .bind(c.expires_at)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("coupon code"))
}

/// A currently redeemable coupon by code (active, within its dates, uses left).
pub async fn redeemable_coupon(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    code: &str,
) -> DbResult<Option<CouponRow>> {
    Ok(sqlx::query_as::<_, CouponRow>(
        "select * from coupons
         where tenant_id = $1 and code = $2::citext and is_active
           and (starts_at is null or starts_at <= now())
           and (expires_at is null or expires_at > now())
           and (max_uses is null or uses_count < max_uses)",
    )
    .bind(tenant_id)
    .bind(code.trim())
    .fetch_optional(db)
    .await?)
}

/// Atomically counts one use; `false` when the coupon is exhausted.
pub async fn increment_coupon_use(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: CouponId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update coupons set uses_count = uses_count + 1
         where tenant_id = $1 and id = $2 and (max_uses is null or uses_count < max_uses)",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Coupons of a tenant (newest first).
pub async fn list_coupons(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<CouponRow>> {
    Ok(sqlx::query_as::<_, CouponRow>(
        "select * from coupons where tenant_id = $1 order by created_at desc",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Convenience: the raw `translations` value for a locale key, if present.
pub fn translation<'a>(translations: &'a Json, locale: &str, field: &str) -> Option<&'a str> {
    translations.get(locale)?.get(field)?.as_str()
}

/// Points a product at the integration (module) that provisions it.
/// Returns `false` when the product does not exist in the tenant.
pub async fn set_product_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    integration_id: Option<IntegrationId>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update products set integration_id = $3, updated_at = now() where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(product_id)
    .bind(integration_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}
