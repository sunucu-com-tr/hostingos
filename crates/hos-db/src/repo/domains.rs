//! Domain names: sellable TLDs and prices, registered domains, their contacts,
//! DNS zones and records.

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::domain::DomainStatus;
use hos_core::ids::{
    ClientId, DnsRecordId, DnsZoneId, DomainId, IntegrationId, OrderId, TenantId, TldId,
};
use hos_core::integrations::registrar::DnsRecordType;
use hos_core::money::Currency;
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgExecutor, Postgres};

use super::Json;
use crate::enums::{ContactRole, DomainRegistrationType, TldAction};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::ids::{DomainContactId, TldPricingId};
use crate::page::{Page, Paged, fetch_paged};

// ---------------------------------------------------------------------------
// TLDs & pricing
// ---------------------------------------------------------------------------

/// `tlds` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TldRow {
    pub id: TldId,
    pub tenant_id: TenantId,
    pub tld: String,
    pub registrar_integration_id: Option<IntegrationId>,
    pub requires_epp: bool,
    pub supports_privacy: bool,
    pub supports_dns_management: bool,
    pub requirements: Json,
    pub min_years: i16,
    pub max_years: i16,
    pub grace_period_days: i32,
    pub redemption_period_days: i32,
    pub label: Option<String>,
    pub is_featured: bool,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// TLD settings for [`upsert_tld`].
#[derive(Debug, Clone)]
pub struct TldInput {
    /// Without leading dot (`com.tr`).
    pub tld: String,
    pub registrar_integration_id: Option<IntegrationId>,
    pub requires_epp: bool,
    pub supports_privacy: bool,
    pub min_years: i16,
    pub max_years: i16,
    pub grace_period_days: i32,
    pub redemption_period_days: i32,
    pub label: Option<String>,
    pub is_featured: bool,
    pub sort_order: i32,
}

impl TldInput {
    /// Defaults: EPP required, privacy supported, 1–10 years.
    pub fn new(tld: &str) -> Self {
        Self {
            tld: tld.trim().trim_start_matches('.').to_ascii_lowercase(),
            registrar_integration_id: None,
            requires_epp: true,
            supports_privacy: true,
            min_years: 1,
            max_years: 10,
            grace_period_days: 30,
            redemption_period_days: 30,
            label: None,
            is_featured: false,
            sort_order: 0,
        }
    }
}

/// Inserts or updates a TLD.
pub async fn upsert_tld(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    t: &TldInput,
) -> DbResult<TldRow> {
    Ok(sqlx::query_as::<_, TldRow>(
        "insert into tlds (id, tenant_id, tld, registrar_integration_id, requires_epp, supports_privacy, min_years,
                           max_years, grace_period_days, redemption_period_days, label, is_featured, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         on conflict (tenant_id, tld) do update
            set registrar_integration_id = coalesce(excluded.registrar_integration_id, tlds.registrar_integration_id),
                requires_epp = excluded.requires_epp, supports_privacy = excluded.supports_privacy,
                min_years = excluded.min_years, max_years = excluded.max_years,
                grace_period_days = excluded.grace_period_days,
                redemption_period_days = excluded.redemption_period_days, label = excluded.label,
                is_featured = excluded.is_featured, sort_order = excluded.sort_order
         returning *",
    )
    .bind(TldId::new())
    .bind(tenant_id)
    .bind(&t.tld)
    .bind(t.registrar_integration_id)
    .bind(t.requires_epp)
    .bind(t.supports_privacy)
    .bind(t.min_years)
    .bind(t.max_years)
    .bind(t.grace_period_days)
    .bind(t.redemption_period_days)
    .bind(&t.label)
    .bind(t.is_featured)
    .bind(t.sort_order)
    .fetch_one(db)
    .await?)
}

/// One TLD price point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TldPriceInput {
    pub currency: Currency,
    pub action: TldAction,
    pub years: i16,
    pub price: Decimal,
}

/// Upserts many price points of one TLD in a single statement.
pub async fn set_tld_prices(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    tld_id: TldId,
    prices: &[TldPriceInput],
) -> DbResult<u64> {
    if prices.is_empty() {
        return Ok(0);
    }
    let ids: Vec<TldPricingId> = prices.iter().map(|_| TldPricingId::new()).collect();
    let currencies: Vec<Currency> = prices.iter().map(|p| p.currency).collect();
    let actions: Vec<TldAction> = prices.iter().map(|p| p.action).collect();
    let years: Vec<i16> = prices.iter().map(|p| p.years).collect();
    let amounts: Vec<Decimal> = prices.iter().map(|p| p.price).collect();
    let res = sqlx::query(
        "insert into tld_pricing (id, tenant_id, tld_id, currency, action, years, price)
         select u.id, $1, t.id, u.currency, u.action, u.years, u.price
         from unnest($3::uuid[], $4::text[], $5::text[], $6::int2[], $7::numeric[])
              as u(id, currency, action, years, price)
         join tlds t on t.tenant_id = $1 and t.id = $2
         on conflict (tld_id, currency, action, years) do update set price = excluded.price",
    )
    .bind(tenant_id)
    .bind(tld_id)
    .bind(&ids)
    .bind(&currencies)
    .bind(&actions)
    .bind(&years)
    .bind(&amounts)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// TLDs of a tenant in display order.
pub async fn list_tlds(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    active_only: bool,
) -> DbResult<Vec<TldRow>> {
    Ok(sqlx::query_as::<_, TldRow>(
        "select * from tlds where tenant_id = $1 and (not $2 or is_active) order by sort_order, tld",
    )
    .bind(tenant_id)
    .bind(active_only)
    .fetch_all(db)
    .await?)
}

/// The longest active TLD that `domain_name` ends with (`example.com.tr` → `com.tr`).
pub async fn match_tld(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    domain_name: &str,
) -> DbResult<Option<TldRow>> {
    Ok(sqlx::query_as::<_, TldRow>(
        "select * from tlds
         where tenant_id = $1 and is_active and lower($2) like '%.' || tld
         order by length(tld) desc limit 1",
    )
    .bind(tenant_id)
    .bind(domain_name.trim())
    .fetch_optional(db)
    .await?)
}

/// Price of one action for `years` years.
pub async fn tld_price(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    tld_id: TldId,
    currency: Currency,
    action: TldAction,
    years: i16,
) -> DbResult<Option<Decimal>> {
    Ok(sqlx::query_scalar::<_, Decimal>(
        "select price from tld_pricing
         where tenant_id = $1 and tld_id = $2 and currency = $3 and action = $4 and years = $5",
    )
    .bind(tenant_id)
    .bind(tld_id)
    .bind(currency)
    .bind(action)
    .bind(years)
    .fetch_optional(db)
    .await?)
}

/// One row of the public domain price table (1-year prices).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TldPriceTableRow {
    pub tld_id: TldId,
    pub tld: String,
    pub label: Option<String>,
    pub is_featured: bool,
    pub register: Option<Decimal>,
    pub transfer: Option<Decimal>,
    pub renew: Option<Decimal>,
}

/// Public price table (register / transfer / renew for 1 year) in `currency`.
pub async fn tld_price_table(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    currency: Currency,
) -> DbResult<Vec<TldPriceTableRow>> {
    Ok(sqlx::query_as::<_, TldPriceTableRow>(
        "select t.id as tld_id, t.tld, t.label, t.is_featured,
                min(p.price) filter (where p.action = 'register') as register,
                min(p.price) filter (where p.action = 'transfer') as transfer,
                min(p.price) filter (where p.action = 'renew') as renew
         from tlds t
         left join tld_pricing p on p.tld_id = t.id and p.currency = $2 and p.years = 1
         where t.tenant_id = $1 and t.is_active
         group by t.id
         order by t.sort_order, t.tld",
    )
    .bind(tenant_id)
    .bind(currency)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// domains
// ---------------------------------------------------------------------------

/// `domains` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct DomainRow {
    pub id: DomainId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub order_id: Option<OrderId>,
    pub name: String,
    pub tld: String,
    pub registrar_integration_id: Option<IntegrationId>,
    pub external_id: Option<String>,
    pub status: DomainStatus,
    pub registration_type: DomainRegistrationType,
    pub registration_period: i16,
    pub registered_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub next_due_date: Option<NaiveDate>,
    pub next_invoice_date: Option<NaiveDate>,
    pub first_payment_amount: Decimal,
    pub recurring_amount: Decimal,
    pub currency: Currency,
    pub auto_renew: bool,
    pub privacy: bool,
    pub lock: bool,
    pub dns_management: bool,
    pub email_forwarding: bool,
    pub nameservers: Vec<String>,
    pub epp_code_enc: Option<Vec<u8>>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub sync_error: Option<String>,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_domain`] (status `pending`).
#[derive(Debug, Clone)]
pub struct NewDomain {
    pub client_id: ClientId,
    pub order_id: Option<OrderId>,
    /// FQDN; lower-cased on insert.
    pub name: String,
    pub tld: String,
    pub registrar_integration_id: Option<IntegrationId>,
    pub registration_type: DomainRegistrationType,
    pub registration_period: i16,
    pub first_payment_amount: Decimal,
    pub recurring_amount: Decimal,
    pub currency: Currency,
    pub privacy: bool,
    pub nameservers: Vec<String>,
    pub epp_code_enc: Option<Vec<u8>>,
}

/// Creates a domain. A live duplicate within the tenant → `Conflict`.
pub async fn create_domain(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    d: &NewDomain,
) -> DbResult<DomainRow> {
    sqlx::query_as::<_, DomainRow>(
        "insert into domains (id, tenant_id, client_id, order_id, name, tld, registrar_integration_id,
                              registration_type, registration_period, first_payment_amount, recurring_amount,
                              currency, privacy, nameservers, epp_code_enc)
         values ($1, $2, $3, $4, lower($5), lower($6), $7, $8, $9, $10, $11, $12, $13, $14, $15)
         returning *",
    )
    .bind(DomainId::new())
    .bind(tenant_id)
    .bind(d.client_id)
    .bind(d.order_id)
    .bind(d.name.trim())
    .bind(d.tld.trim().trim_start_matches('.'))
    .bind(d.registrar_integration_id)
    .bind(d.registration_type)
    .bind(d.registration_period)
    .bind(d.first_payment_amount)
    .bind(d.recurring_amount)
    .bind(d.currency)
    .bind(d.privacy)
    .bind(&d.nameservers)
    .bind(&d.epp_code_enc)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("domain"))
}

/// Domain by id.
pub async fn get_domain(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
) -> DbResult<Option<DomainRow>> {
    Ok(
        sqlx::query_as::<_, DomainRow>("select * from domains where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// The live domain with this name (any client).
pub async fn domain_by_name(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    name: &str,
) -> DbResult<Option<DomainRow>> {
    Ok(sqlx::query_as::<_, DomainRow>(
        "select * from domains where tenant_id = $1 and name = lower($2)::citext
           and status not in ('expired', 'transferred_away', 'cancelled', 'fraud')",
    )
    .bind(tenant_id)
    .bind(name.trim())
    .fetch_optional(db)
    .await?)
}

/// Filters of [`list_domains`].
#[derive(Debug, Clone, Default)]
pub struct DomainFilter {
    pub client_id: Option<ClientId>,
    pub status: Option<DomainStatus>,
    pub search: Option<String>,
}

/// Domains of a tenant (by name).
pub async fn list_domains<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &DomainFilter,
    page: Page,
) -> DbResult<Paged<DomainRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from domains where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(c) = f.client_id {
                qb.push(" and client_id = ").push_bind(c);
            }
            if let Some(s) = f.status {
                qb.push(" and status = ").push_bind(s);
            }
            if let Some(s) = &search {
                qb.push(" and name::text ilike ").push_bind(s.clone());
            }
        },
        "name, id",
        page,
    )
    .await
}

/// Domains of a client (client area).
pub async fn client_domains(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<DomainRow>> {
    Ok(sqlx::query_as::<_, DomainRow>(
        "select * from domains where tenant_id = $1 and client_id = $2 order by expires_at nulls last, name",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// Moves a domain `from` → `to` if unchanged; transitions `DomainStatus::transition`
/// forbids are rejected with `CoreError::InvalidTransition`.
pub async fn transition_domain(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    from: DomainStatus,
    to: DomainStatus,
) -> DbResult<bool> {
    from.transition(to)?;
    let res = sqlx::query(
        "update domains set status = $4,
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

/// Stores registrar data after register / transfer / renew / sync.
#[allow(clippy::too_many_arguments)]
pub async fn update_registration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    external_id: Option<&str>,
    registered_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    next_due_date: Option<NaiveDate>,
    nameservers: Option<&[String]>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update domains set external_id = coalesce($3, external_id), registered_at = coalesce($4, registered_at),
                expires_at = coalesce($5, expires_at), next_due_date = coalesce($6, next_due_date),
                nameservers = coalesce($7, nameservers), last_synced_at = now(), sync_error = null
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(external_id)
    .bind(registered_at)
    .bind(expires_at)
    .bind(next_due_date)
    .bind(nameservers)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Records a failed registrar sync.
pub async fn record_sync_error(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    error: &str,
) -> DbResult<()> {
    sqlx::query("update domains set last_synced_at = now(), sync_error = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(error)
        .execute(db)
        .await?;
    Ok(())
}

/// Updates auto-renew / privacy / lock flags.
pub async fn set_domain_flags(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    auto_renew: bool,
    privacy: bool,
    lock: bool,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update domains set auto_renew = $3, privacy = $4, lock = $5 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(auto_renew)
    .bind(privacy)
    .bind(lock)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Stores the encrypted EPP code.
pub async fn set_epp_code(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DomainId,
    epp_code_enc: Option<&[u8]>,
) -> DbResult<bool> {
    let res = sqlx::query("update domains set epp_code_enc = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(epp_code_enc)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Live domains expiring within `[now, now + within_days]` (expiry notices,
/// renewal). `auto_renew_only` restricts to domains set to auto-renew.
pub async fn domains_expiring(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    within_days: i32,
    auto_renew_only: bool,
) -> DbResult<Vec<DomainRow>> {
    Ok(sqlx::query_as::<_, DomainRow>(
        "select * from domains
         where tenant_id = $1 and status in ('active', 'grace')
           and expires_at is not null and expires_at <= now() + make_interval(days => $2)
           and expires_at >= now() - interval '45 days'
           and (not $3 or auto_renew)
         order by expires_at, id",
    )
    .bind(tenant_id)
    .bind(within_days)
    .bind(auto_renew_only)
    .fetch_all(db)
    .await?)
}

/// Auto-renewing live domains whose renewal invoice is due
/// (`coalesce(next_invoice_date, next_due_date - days_before) <= as_of`, no
/// non-cancelled invoice line for that due date yet).
pub async fn domains_due_for_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    as_of: NaiveDate,
    days_before: i32,
    limit: i64,
) -> DbResult<Vec<DomainRow>> {
    Ok(sqlx::query_as::<_, DomainRow>(
        "select d.* from domains d
         where d.tenant_id = $1 and d.status in ('active', 'grace') and d.auto_renew
           and d.next_due_date is not null
           and coalesce(d.next_invoice_date, d.next_due_date - $3) <= $2
           and not exists (
               select 1 from invoice_items ii join invoices i on i.id = ii.invoice_id
               where ii.domain_id = d.id and ii.period_start = d.next_due_date and i.status <> 'cancelled')
         order by d.next_due_date, d.id limit $4",
    )
    .bind(tenant_id)
    .bind(as_of)
    .bind(days_before)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// contacts
// ---------------------------------------------------------------------------

/// `domain_contacts` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct DomainContactRow {
    pub id: DomainContactId,
    pub tenant_id: TenantId,
    pub domain_id: DomainId,
    pub role: ContactRole,
    pub first_name: String,
    pub last_name: String,
    pub organization: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country_code: String,
    pub external_contact_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Contact data for [`upsert_contact`].
#[derive(Debug, Clone)]
pub struct DomainContactInput {
    pub first_name: String,
    pub last_name: String,
    pub organization: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country_code: String,
    pub external_contact_id: Option<String>,
}

/// Inserts or replaces the contact of one role.
pub async fn upsert_contact(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    domain_id: DomainId,
    role: ContactRole,
    c: &DomainContactInput,
) -> DbResult<DomainContactRow> {
    Ok(sqlx::query_as::<_, DomainContactRow>(
        "insert into domain_contacts (id, tenant_id, domain_id, role, first_name, last_name, organization, email,
                                      phone, address_line1, address_line2, city, state, postal_code, country_code,
                                      external_contact_id)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, upper($15), $16)
         on conflict (domain_id, role) do update
            set first_name = excluded.first_name, last_name = excluded.last_name,
                organization = excluded.organization, email = excluded.email, phone = excluded.phone,
                address_line1 = excluded.address_line1, address_line2 = excluded.address_line2,
                city = excluded.city, state = excluded.state, postal_code = excluded.postal_code,
                country_code = excluded.country_code, external_contact_id = excluded.external_contact_id
         returning *",
    )
    .bind(DomainContactId::new())
    .bind(tenant_id)
    .bind(domain_id)
    .bind(role)
    .bind(&c.first_name)
    .bind(&c.last_name)
    .bind(&c.organization)
    .bind(c.email.trim())
    .bind(&c.phone)
    .bind(&c.address_line1)
    .bind(&c.address_line2)
    .bind(&c.city)
    .bind(&c.state)
    .bind(&c.postal_code)
    .bind(&c.country_code)
    .bind(&c.external_contact_id)
    .fetch_one(db)
    .await?)
}

/// Contacts of a domain.
pub async fn domain_contacts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    domain_id: DomainId,
) -> DbResult<Vec<DomainContactRow>> {
    Ok(sqlx::query_as::<_, DomainContactRow>(
        "select * from domain_contacts where tenant_id = $1 and domain_id = $2 order by role",
    )
    .bind(tenant_id)
    .bind(domain_id)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// DNS
// ---------------------------------------------------------------------------

/// `dns_zones` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct DnsZoneRow {
    pub id: DnsZoneId,
    pub tenant_id: TenantId,
    pub domain_id: Option<DomainId>,
    pub client_id: Option<ClientId>,
    pub name: String,
    pub integration_id: Option<IntegrationId>,
    pub external_id: Option<String>,
    pub serial: i64,
    pub default_ttl: i32,
    pub is_active: bool,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `dns_records` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct DnsRecordRow {
    pub id: DnsRecordId,
    pub tenant_id: TenantId,
    pub zone_id: DnsZoneId,
    pub name: String,
    pub record_type: DnsRecordType,
    pub content: String,
    pub ttl: i32,
    pub priority: Option<i32>,
    pub external_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a zone.
pub async fn create_zone(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    name: &str,
    domain_id: Option<DomainId>,
    client_id: Option<ClientId>,
    integration_id: Option<IntegrationId>,
) -> DbResult<DnsZoneRow> {
    sqlx::query_as::<_, DnsZoneRow>(
        "insert into dns_zones (id, tenant_id, name, domain_id, client_id, integration_id)
         values ($1, $2, lower($3), $4, $5, $6) returning *",
    )
    .bind(DnsZoneId::new())
    .bind(tenant_id)
    .bind(name.trim().trim_end_matches('.'))
    .bind(domain_id)
    .bind(client_id)
    .bind(integration_id)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("dns zone"))
}

/// Zone by apex name.
pub async fn zone_by_name(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    name: &str,
) -> DbResult<Option<DnsZoneRow>> {
    Ok(sqlx::query_as::<_, DnsZoneRow>(
        "select * from dns_zones where tenant_id = $1 and name = lower($2)::citext",
    )
    .bind(tenant_id)
    .bind(name.trim().trim_end_matches('.'))
    .fetch_optional(db)
    .await?)
}

/// Records of a zone.
pub async fn zone_records(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    zone_id: DnsZoneId,
) -> DbResult<Vec<DnsRecordRow>> {
    Ok(sqlx::query_as::<_, DnsRecordRow>(
        "select * from dns_records where tenant_id = $1 and zone_id = $2 order by name, record_type, priority nulls first",
    )
    .bind(tenant_id)
    .bind(zone_id)
    .fetch_all(db)
    .await?)
}

/// Adds a record and bumps the zone serial (one statement).
#[allow(clippy::too_many_arguments)]
pub async fn add_record(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    zone_id: DnsZoneId,
    name: &str,
    record_type: DnsRecordType,
    content: &str,
    ttl: i32,
    priority: Option<i32>,
) -> DbResult<DnsRecordRow> {
    Ok(sqlx::query_as::<_, DnsRecordRow>(
        "with z as (update dns_zones set serial = serial + 1 where tenant_id = $2 and id = $3 returning id)
         insert into dns_records (id, tenant_id, zone_id, name, record_type, content, ttl, priority)
         select $1, $2, z.id, $4, $5, $6, $7, $8 from z
         returning *",
    )
    .bind(DnsRecordId::new())
    .bind(tenant_id)
    .bind(zone_id)
    .bind(name)
    .bind(record_type)
    .bind(content)
    .bind(ttl)
    .bind(priority)
    .fetch_one(db)
    .await?)
}

/// Deletes a record and bumps the zone serial.
pub async fn delete_record(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    zone_id: DnsZoneId,
    id: DnsRecordId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "with d as (delete from dns_records where tenant_id = $1 and zone_id = $2 and id = $3 returning zone_id)
         update dns_zones set serial = serial + 1 where id in (select zone_id from d)",
    )
    .bind(tenant_id)
    .bind(zone_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Assigns `integration_id` as registrar of every TLD of the tenant that has
/// none yet (demo setup / first registrar). Returns the number of TLDs updated.
pub async fn assign_default_registrar(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: IntegrationId,
) -> DbResult<u64> {
    let res = sqlx::query(
        "update tlds set registrar_integration_id = $2, updated_at = now()
          where tenant_id = $1 and registrar_integration_id is null",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}
