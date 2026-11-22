//! Affiliate programme: `affiliates`, `affiliate_product_rates`,
//! `affiliate_clicks`, `affiliate_referrals`, `affiliate_commissions`,
//! `affiliate_payouts`.
//!
//! Money rules (hos-billing `affiliates` drives them):
//!
//! * a commission is accrued once per paid invoice line
//!   (`affiliate_commissions_item_idx`), `pending` until `clears_at`, then
//!   `available`; `held` when the referral is flagged for review;
//! * the balance is the sum of `available` rows (negative `adjustment`
//!   rows take back commission that was already paid out);
//! * a payout request moves every `available` row into the request
//!   (`withdrawn`) in one transaction under the affiliate's row lock; staff
//!   approval is a single `requested → paid` transition, so a replay never
//!   credits twice; a rejection puts the rows back.

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::CoreError;
use hos_core::ids::{ClientId, InvoiceId, ProductId, ServiceId, TenantId, UserId};
use hos_core::money::Currency;
use hos_core::string_enum;
use rust_decimal::Decimal;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use crate::error::{DbResult, conflict_on_unique, not_found};
use crate::page::{Page, Paged, fetch_paged};

hos_core::define_id! {
    /// `affiliates`.
    pub struct AffiliateId;
}
hos_core::define_id! {
    /// `affiliate_referrals`.
    pub struct ReferralId;
}
hos_core::define_id! {
    /// `affiliate_commissions`.
    pub struct CommissionId;
}
hos_core::define_id! {
    /// `affiliate_payouts`.
    pub struct PayoutId;
}

string_enum! {
    /// `affiliates.status`.
    pub enum AffiliateStatus {
        Active => "active",
        Suspended => "suspended",
    }
}

string_enum! {
    /// `affiliate_commissions.status`.
    pub enum CommissionStatus {
        /// Waiting for the clearance delay.
        Pending => "pending",
        /// Held for staff review (anti-fraud flag).
        Held => "held",
        /// Cleared: part of the balance.
        Available => "available",
        /// Included in a payout request.
        Withdrawn => "withdrawn",
        /// Taken back (refund, chargeback, cancellation, staff).
        Reversed => "reversed",
    }
}

string_enum! {
    /// `affiliate_payouts.status`.
    pub enum PayoutStatus {
        Requested => "requested",
        Paid => "paid",
        Rejected => "rejected",
    }
}

string_enum! {
    /// `affiliate_payouts.method`.
    pub enum PayoutMethod {
        /// Added to the affiliate's account credit.
        Credit => "credit",
        /// Paid by staff outside HostingOS (bank transfer …).
        Manual => "manual",
    }
}

/// `affiliates` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct AffiliateRow {
    pub id: AffiliateId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub code: String,
    pub status: AffiliateStatus,
    pub commission_type: Option<String>,
    pub commission_value: Option<Decimal>,
    pub commission_recurring: Option<bool>,
    pub currency: Currency,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `affiliate_referrals` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ReferralRow {
    pub id: ReferralId,
    pub tenant_id: TenantId,
    pub affiliate_id: AffiliateId,
    pub client_id: ClientId,
    pub source: String,
    pub clicked_at: Option<DateTime<Utc>>,
    pub flag: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `affiliate_commissions` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct CommissionRow {
    pub id: CommissionId,
    pub tenant_id: TenantId,
    pub affiliate_id: AffiliateId,
    pub referral_id: ReferralId,
    pub client_id: ClientId,
    pub invoice_id: Option<InvoiceId>,
    pub invoice_item_id: Option<Uuid>,
    pub service_id: Option<ServiceId>,
    pub kind: String,
    pub base_amount: Decimal,
    pub amount: Decimal,
    pub currency: Currency,
    pub status: CommissionStatus,
    pub clears_at: DateTime<Utc>,
    pub reversed_at: Option<DateTime<Utc>>,
    pub reversal_reason: Option<String>,
    pub payout_id: Option<PayoutId>,
    pub adjusts_id: Option<CommissionId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `affiliate_payouts` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PayoutRow {
    pub id: PayoutId,
    pub tenant_id: TenantId,
    pub affiliate_id: AffiliateId,
    pub amount: Decimal,
    pub currency: Currency,
    pub method: PayoutMethod,
    pub status: PayoutStatus,
    pub details: Option<String>,
    pub staff_note: Option<String>,
    pub processed_by: Option<UserId>,
    pub processed_at: Option<DateTime<Utc>>,
    pub credit_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `affiliate_product_rates` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ProductRateRow {
    pub product_id: ProductId,
    pub product_name: String,
    pub commission_type: String,
    pub commission_value: Decimal,
    pub recurring: bool,
}

// ---------------------------------------------------------------------------
// affiliates
// ---------------------------------------------------------------------------

/// Creates the affiliate account of a client (`Conflict` when the client
/// already has one or the code is taken).
pub async fn create_affiliate(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    code: &str,
    currency: Currency,
) -> DbResult<AffiliateRow> {
    sqlx::query_as::<_, AffiliateRow>(
        "insert into affiliates (id, tenant_id, client_id, code, currency)
         values ($1, $2, $3, $4, $5) returning *",
    )
    .bind(AffiliateId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(code)
    .bind(currency)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("affiliate"))
}

/// One affiliate.
pub async fn get_affiliate(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AffiliateId,
) -> DbResult<Option<AffiliateRow>> {
    Ok(sqlx::query_as::<_, AffiliateRow>(
        "select * from affiliates where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// The affiliate account of a client.
pub async fn affiliate_for_client(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Option<AffiliateRow>> {
    Ok(sqlx::query_as::<_, AffiliateRow>(
        "select * from affiliates where tenant_id = $1 and client_id = $2",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_optional(db)
    .await?)
}

/// The active affiliate with referral `code` (case-insensitive).
pub async fn active_affiliate_by_code(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    code: &str,
) -> DbResult<Option<AffiliateRow>> {
    Ok(sqlx::query_as::<_, AffiliateRow>(
        "select * from affiliates where tenant_id = $1 and code = $2::citext and status = 'active'",
    )
    .bind(tenant_id)
    .bind(code)
    .fetch_optional(db)
    .await?)
}

/// Sets status and commission overrides (`None` type clears the override).
pub async fn update_affiliate(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AffiliateId,
    status: AffiliateStatus,
    commission: Option<(String, Decimal)>,
    recurring: Option<bool>,
) -> DbResult<AffiliateRow> {
    let (kind, value) = commission.map_or((None, None), |(k, v)| (Some(k), Some(v)));
    sqlx::query_as::<_, AffiliateRow>(
        "update affiliates set status = $3, commission_type = $4, commission_value = $5,
                commission_recurring = $6
          where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(kind)
    .bind(value)
    .bind(recurring)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("affiliate"))?
    .ok_or_else(|| not_found("affiliate", id))
}

/// Admin list row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct AffiliateListRow {
    #[sqlx(flatten)]
    pub affiliate: AffiliateRow,
    pub client_name: String,
    pub client_email: String,
    pub referrals: i64,
    pub clicks: i64,
    pub pending: Decimal,
    pub balance: Decimal,
    pub paid_out: Decimal,
}

const STATS_SELECT: &str = "select a.*,
        coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name,
        c.email::text as client_email,
        (select count(*) from affiliate_referrals r where r.tenant_id = a.tenant_id and r.affiliate_id = a.id) as referrals,
        (select count(*) from affiliate_clicks k where k.tenant_id = a.tenant_id and k.affiliate_id = a.id) as clicks,
        (select coalesce(sum(m.amount), 0) from affiliate_commissions m
          where m.tenant_id = a.tenant_id and m.affiliate_id = a.id and m.status in ('pending', 'held')) as pending,
        (select coalesce(sum(m.amount), 0) from affiliate_commissions m
          where m.tenant_id = a.tenant_id and m.affiliate_id = a.id and m.status = 'available') as balance,
        (select coalesce(sum(p.amount), 0) from affiliate_payouts p
          where p.tenant_id = a.tenant_id and p.affiliate_id = a.id and p.status = 'paid') as paid_out";

/// Paged affiliates with their figures.
pub async fn list_affiliates(
    db: impl sqlx::Acquire<'_, Database = sqlx::Postgres>,
    tenant_id: TenantId,
    search: Option<&str>,
    page: Page,
) -> DbResult<Paged<AffiliateListRow>> {
    let search = search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(crate::error::like_contains);
    fetch_paged(
        db,
        STATS_SELECT,
        |qb| {
            qb.push(
                "from affiliates a join clients c on c.tenant_id = a.tenant_id and c.id = a.client_id
                 where a.tenant_id = ",
            )
            .push_bind(tenant_id);
            if let Some(s) = &search {
                qb.push(" and (c.email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or a.code::text ilike ")
                    .push_bind(s.clone())
                    .push(" or c.company_name ilike ")
                    .push_bind(s.clone())
                    .push(" or (c.first_name || ' ' || c.last_name) ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        "a.created_at desc",
        page,
    )
    .await
}

/// One affiliate with its figures.
pub async fn affiliate_stats(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: AffiliateId,
) -> DbResult<Option<AffiliateListRow>> {
    Ok(sqlx::query_as::<_, AffiliateListRow>(&format!(
        "{STATS_SELECT} from affiliates a join clients c on c.tenant_id = a.tenant_id and c.id = a.client_id
          where a.tenant_id = $1 and a.id = $2"
    ))
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------------------
// product rates
// ---------------------------------------------------------------------------

/// Per-product overrides with the product names.
pub async fn product_rates(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<ProductRateRow>> {
    Ok(sqlx::query_as::<_, ProductRateRow>(
        "select r.product_id, p.name as product_name, r.commission_type, r.commission_value, r.recurring
           from affiliate_product_rates r
           join products p on p.tenant_id = r.tenant_id and p.id = r.product_id
          where r.tenant_id = $1 order by p.name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// The override of one product.
pub async fn product_rate(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
) -> DbResult<Option<(String, Decimal, bool)>> {
    Ok(sqlx::query_as::<_, (String, Decimal, bool)>(
        "select commission_type, commission_value, recurring from affiliate_product_rates
          where tenant_id = $1 and product_id = $2",
    )
    .bind(tenant_id)
    .bind(product_id)
    .fetch_optional(db)
    .await?)
}

/// Creates or replaces a product override.
pub async fn set_product_rate(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
    commission_type: &str,
    value: Decimal,
    recurring: bool,
) -> DbResult<()> {
    sqlx::query(
        "insert into affiliate_product_rates (id, tenant_id, product_id, commission_type, commission_value, recurring)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (tenant_id, product_id) do update
            set commission_type = excluded.commission_type,
                commission_value = excluded.commission_value,
                recurring = excluded.recurring",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(product_id)
    .bind(commission_type)
    .bind(value)
    .bind(recurring)
    .execute(db)
    .await
    .map_err(conflict_on_unique("product rate"))?;
    Ok(())
}

/// Removes a product override.
pub async fn delete_product_rate(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    product_id: ProductId,
) -> DbResult<bool> {
    let res =
        sqlx::query("delete from affiliate_product_rates where tenant_id = $1 and product_id = $2")
            .bind(tenant_id)
            .bind(product_id)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// clicks and referrals
// ---------------------------------------------------------------------------

/// Records a referral link visit (once per visitor and day).
pub async fn record_click(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
    visitor_hash: &str,
    day: NaiveDate,
    landing_path: Option<&str>,
    referrer_host: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "insert into affiliate_clicks (id, tenant_id, affiliate_id, visitor_hash, day, landing_path, referrer_host)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (tenant_id, affiliate_id, visitor_hash, day) do nothing",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(visitor_hash)
    .bind(day)
    .bind(landing_path)
    .bind(referrer_host)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Links a client to the affiliate that referred it; `None` when the client
/// already has a referrer (first attribution wins).
pub async fn create_referral(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
    client_id: ClientId,
    source: &str,
    clicked_at: Option<DateTime<Utc>>,
    flag: Option<&str>,
) -> DbResult<Option<ReferralRow>> {
    Ok(sqlx::query_as::<_, ReferralRow>(
        "insert into affiliate_referrals (id, tenant_id, affiliate_id, client_id, source, clicked_at, flag)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (tenant_id, client_id) do nothing returning *",
    )
    .bind(ReferralId::new())
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(client_id)
    .bind(source)
    .bind(clicked_at)
    .bind(flag)
    .fetch_optional(db)
    .await?)
}

/// The referral of a client.
pub async fn referral_for_client(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Option<ReferralRow>> {
    Ok(sqlx::query_as::<_, ReferralRow>(
        "select * from affiliate_referrals where tenant_id = $1 and client_id = $2",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_optional(db)
    .await?)
}

/// Flags (or clears) a referral for review.
pub async fn set_referral_flag(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ReferralId,
    flag: Option<&str>,
) -> DbResult<()> {
    sqlx::query("update affiliate_referrals set flag = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(flag)
        .execute(db)
        .await?;
    Ok(())
}

/// A referral row as shown to the affiliate (no personal data of the
/// referred client beyond a masked name).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ReferralView {
    pub id: ReferralId,
    pub client_id: ClientId,
    pub first_name: String,
    pub last_initial: String,
    pub flag: Option<String>,
    pub created_at: DateTime<Utc>,
    pub earned: Decimal,
}

/// Referrals of an affiliate, newest first.
pub async fn referrals_of(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
    limit: i64,
) -> DbResult<Vec<ReferralView>> {
    Ok(sqlx::query_as::<_, ReferralView>(
        "select r.id, r.client_id, c.first_name, left(c.last_name, 1) as last_initial,
                r.flag, r.created_at,
                (select coalesce(sum(m.amount), 0) from affiliate_commissions m
                  where m.tenant_id = r.tenant_id and m.referral_id = r.id and m.status <> 'reversed') as earned
           from affiliate_referrals r
           join clients c on c.tenant_id = r.tenant_id and c.id = r.client_id
          where r.tenant_id = $1 and r.affiliate_id = $2
          order by r.created_at desc limit $3",
    )
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Visits per day for the last `days` days (oldest first, missing days omitted).
pub async fn clicks_by_day(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
    since: NaiveDate,
) -> DbResult<Vec<(NaiveDate, i64)>> {
    Ok(sqlx::query_as::<_, (NaiveDate, i64)>(
        "select day, count(*) from affiliate_clicks
          where tenant_id = $1 and affiliate_id = $2 and day >= $3
          group by day order by day",
    )
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(since)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// commissions
// ---------------------------------------------------------------------------

/// A new commission.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCommission {
    pub affiliate_id: AffiliateId,
    pub referral_id: ReferralId,
    pub client_id: ClientId,
    pub invoice_id: InvoiceId,
    pub invoice_item_id: Uuid,
    pub service_id: Option<ServiceId>,
    pub kind: &'static str,
    pub base_amount: Decimal,
    pub amount: Decimal,
    pub currency: Currency,
    pub status: CommissionStatus,
    pub clears_at: DateTime<Utc>,
}

/// Inserts a commission; `None` when the invoice line already has one
/// (idempotent accrual).
pub async fn insert_commission(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    c: &NewCommission,
) -> DbResult<Option<CommissionRow>> {
    Ok(sqlx::query_as::<_, CommissionRow>(
        "insert into affiliate_commissions (id, tenant_id, affiliate_id, referral_id, client_id, invoice_id,
                invoice_item_id, service_id, kind, base_amount, amount, currency, status, clears_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         on conflict (tenant_id, invoice_item_id) where invoice_item_id is not null and kind <> 'adjustment'
         do nothing returning *",
    )
    .bind(CommissionId::new())
    .bind(tenant_id)
    .bind(c.affiliate_id)
    .bind(c.referral_id)
    .bind(c.client_id)
    .bind(c.invoice_id)
    .bind(c.invoice_item_id)
    .bind(c.service_id)
    .bind(c.kind)
    .bind(c.base_amount)
    .bind(c.amount)
    .bind(c.currency)
    .bind(c.status)
    .bind(c.clears_at)
    .fetch_optional(db)
    .await?)
}

/// Whether a commission was ever accrued for `service_id`.
pub async fn service_has_commission(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    service_id: ServiceId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from affiliate_commissions
                        where tenant_id = $1 and service_id = $2 and kind <> 'adjustment')",
    )
    .bind(tenant_id)
    .bind(service_id)
    .fetch_one(db)
    .await?)
}

/// Commissions (not yet reversed) whose invoice was refunded, cancelled, sent
/// to collections or charged back (also through the mass payment that paid
/// it), or — still pending — whose service was cancelled / terminated:
/// `(commission, reason)`.
pub async fn commissions_to_reverse(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<(CommissionRow, String)>> {
    #[derive(FromRow)]
    struct Row {
        #[sqlx(flatten)]
        c: CommissionRow,
        reason: String,
    }
    let rows = sqlx::query_as::<_, Row>(
        "select m.*,
                case
                  when exists(select 1 from transactions t where t.tenant_id = m.tenant_id
                                 and t.invoice_id = m.invoice_id and t.kind = 'chargeback')
                       then 'chargeback'
                  when exists(select 1 from mass_payment_items mi
                                join mass_payments mp on mp.tenant_id = mi.tenant_id and mp.id = mi.mass_payment_id
                                join transactions t on t.tenant_id = mp.tenant_id and t.invoice_id = mp.invoice_id
                                                   and t.kind = 'chargeback'
                               where mi.tenant_id = m.tenant_id and mi.invoice_id = m.invoice_id
                                 and mi.reversed > 0)
                       then 'chargeback'
                  when i.status in ('refunded', 'partially_refunded') then 'refund'
                  when i.status in ('cancelled', 'collections') then 'invoice_' || i.status
                  else 'service_cancelled'
                end as reason
           from affiliate_commissions m
           left join invoices i on i.tenant_id = m.tenant_id and i.id = m.invoice_id
           left join services s on s.tenant_id = m.tenant_id and s.id = m.service_id
          where m.tenant_id = $1 and m.kind <> 'adjustment' and m.reversed_at is null
            and m.status <> 'reversed'
            and (
                  i.status in ('refunded', 'partially_refunded', 'cancelled', 'collections')
               or exists(select 1 from transactions t where t.tenant_id = m.tenant_id
                            and t.invoice_id = m.invoice_id and t.kind = 'chargeback')
               -- paid by a mass payment whose money was charged back
               or exists(select 1 from mass_payment_items mi
                           join mass_payments mp on mp.tenant_id = mi.tenant_id and mp.id = mi.mass_payment_id
                           join transactions t on t.tenant_id = mp.tenant_id and t.invoice_id = mp.invoice_id
                                              and t.kind = 'chargeback'
                          where mi.tenant_id = m.tenant_id and mi.invoice_id = m.invoice_id
                            and mi.reversed > 0)
               or (m.status in ('pending', 'held') and s.status in ('cancelled', 'terminated'))
            )
          order by m.created_at limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.c, r.reason)).collect())
}

/// Reverses a commission. Not yet paid out (`pending`, `held`,
/// `available`): it becomes `reversed`. Already in a payout (`withdrawn`): it
/// is marked and a negative `adjustment` takes the amount back from the
/// balance. Returns whether anything changed (idempotent).
pub async fn reverse_commission(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: CommissionId,
    reason: &str,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let Some(c) = sqlx::query_as::<_, CommissionRow>(
        "select * from affiliate_commissions where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Err(not_found("commission", id));
    };
    if c.reversed_at.is_some() || c.status == CommissionStatus::Reversed || c.kind == "adjustment" {
        tx.commit().await?;
        return Ok(false);
    }
    match c.status {
        CommissionStatus::Withdrawn => {
            sqlx::query(
                "update affiliate_commissions set reversed_at = $3, reversal_reason = $4
                  where tenant_id = $1 and id = $2",
            )
            .bind(tenant_id)
            .bind(id)
            .bind(now)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "insert into affiliate_commissions (id, tenant_id, affiliate_id, referral_id, client_id,
                        invoice_id, service_id, kind, base_amount, amount, currency, status, clears_at,
                        reversal_reason, adjusts_id)
                 values ($1, $2, $3, $4, $5, $6, $7, 'adjustment', 0, $8, $9, 'available', $10, $11, $12)
                 on conflict do nothing",
            )
            .bind(CommissionId::new())
            .bind(tenant_id)
            .bind(c.affiliate_id)
            .bind(c.referral_id)
            .bind(c.client_id)
            .bind(c.invoice_id)
            .bind(c.service_id)
            .bind(-c.amount)
            .bind(c.currency)
            .bind(now)
            .bind(reason)
            .bind(c.id)
            .execute(&mut *tx)
            .await?;
        }
        _ => {
            sqlx::query(
                "update affiliate_commissions set status = 'reversed', reversed_at = $3, reversal_reason = $4
                  where tenant_id = $1 and id = $2",
            )
            .bind(tenant_id)
            .bind(id)
            .bind(now)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(true)
}

/// `pending` commissions whose clearance delay has passed become `available`.
pub async fn clear_due(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    now: DateTime<Utc>,
) -> DbResult<u64> {
    let res = sqlx::query(
        "update affiliate_commissions set status = 'available'
          where tenant_id = $1 and status = 'pending' and clears_at <= $2 and reversed_at is null",
    )
    .bind(tenant_id)
    .bind(now)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Releases (`approve = true`: to `pending`, clearing as usual) or reverses
/// the held commissions of a referral; clears the referral's flag on release.
pub async fn release_held(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    referral_id: ReferralId,
    approve: bool,
    now: DateTime<Utc>,
) -> DbResult<u64> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let res = if approve {
        sqlx::query(
            "update affiliate_commissions set status = 'pending'
              where tenant_id = $1 and referral_id = $2 and status = 'held'",
        )
        .bind(tenant_id)
        .bind(referral_id)
        .execute(&mut *tx)
        .await?
    } else {
        sqlx::query(
            "update affiliate_commissions set status = 'reversed', reversed_at = $3,
                    reversal_reason = 'rejected_by_staff'
              where tenant_id = $1 and referral_id = $2 and status = 'held'",
        )
        .bind(tenant_id)
        .bind(referral_id)
        .bind(now)
        .execute(&mut *tx)
        .await?
    };
    if approve {
        set_referral_flag(&mut *tx, tenant_id, referral_id, None).await?;
    }
    tx.commit().await?;
    Ok(res.rows_affected())
}

/// Commissions of an affiliate, newest first.
pub async fn commissions_of(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
    limit: i64,
) -> DbResult<Vec<CommissionRow>> {
    Ok(sqlx::query_as::<_, CommissionRow>(
        "select * from affiliate_commissions where tenant_id = $1 and affiliate_id = $2
          order by created_at desc limit $3",
    )
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Commissions of an invoice.
pub async fn commissions_for_invoice(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
) -> DbResult<Vec<CommissionRow>> {
    Ok(sqlx::query_as::<_, CommissionRow>(
        "select * from affiliate_commissions where tenant_id = $1 and invoice_id = $2 order by created_at",
    )
    .bind(tenant_id)
    .bind(invoice_id)
    .fetch_all(db)
    .await?)
}

/// The cleared balance of an affiliate (may be negative after adjustments).
pub async fn balance(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
) -> DbResult<Decimal> {
    Ok(sqlx::query_scalar::<_, Decimal>(
        "select coalesce(sum(amount), 0) from affiliate_commissions
          where tenant_id = $1 and affiliate_id = $2 and status = 'available'",
    )
    .bind(tenant_id)
    .bind(affiliate_id)
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// payouts
// ---------------------------------------------------------------------------

/// Requests a payout of the whole cleared balance. Fails with `Validation`
/// below `minimum` and `Conflict` while another request is open.
pub async fn request_payout(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
    method: PayoutMethod,
    details: Option<&str>,
    minimum: Decimal,
) -> DbResult<PayoutRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let aff = sqlx::query_as::<_, AffiliateRow>(
        "select * from affiliates where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(affiliate_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| not_found("affiliate", affiliate_id))?;
    if aff.status != AffiliateStatus::Active {
        return Err(CoreError::Forbidden(
            "the affiliate account is suspended".into(),
        ));
    }
    let amount = balance(&mut *tx, tenant_id, affiliate_id).await?;
    if amount <= Decimal::ZERO || amount < minimum {
        return Err(CoreError::validation(
            "the balance is below the minimum payout amount",
        ));
    }
    let payout = sqlx::query_as::<_, PayoutRow>(
        "insert into affiliate_payouts (id, tenant_id, affiliate_id, amount, currency, method, details)
         values ($1, $2, $3, $4, $5, $6, $7) returning *",
    )
    .bind(PayoutId::new())
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(amount)
    .bind(aff.currency)
    .bind(method)
    .bind(details)
    .fetch_one(&mut *tx)
    .await
    .map_err(conflict_on_unique("open payout request"))?;
    let moved = sqlx::query_scalar::<_, Decimal>(
        "with m as (
             update affiliate_commissions set status = 'withdrawn', payout_id = $3
              where tenant_id = $1 and affiliate_id = $2 and status = 'available'
             returning amount)
         select coalesce(sum(amount), 0) from m",
    )
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(payout.id)
    .fetch_one(&mut *tx)
    .await?;
    if moved != amount {
        return Err(CoreError::Conflict("the balance changed; try again".into()));
    }
    tx.commit().await?;
    Ok(payout)
}

/// One payout.
pub async fn get_payout(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: PayoutId,
) -> DbResult<Option<PayoutRow>> {
    Ok(sqlx::query_as::<_, PayoutRow>(
        "select * from affiliate_payouts where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// `requested → paid` (exactly once); `None` when it was not requested.
pub async fn mark_payout_paid(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: PayoutId,
    staff: Option<UserId>,
    note: Option<&str>,
    now: DateTime<Utc>,
) -> DbResult<Option<PayoutRow>> {
    Ok(sqlx::query_as::<_, PayoutRow>(
        "update affiliate_payouts set status = 'paid', processed_by = $3, staff_note = $4, processed_at = $5
          where tenant_id = $1 and id = $2 and status = 'requested' returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(staff)
    .bind(note)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

/// Links the credit entry a credit payout created.
pub async fn set_payout_credit(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: PayoutId,
    credit_id: Uuid,
) -> DbResult<()> {
    sqlx::query("update affiliate_payouts set credit_id = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(credit_id)
        .execute(db)
        .await?;
    Ok(())
}

/// `requested → rejected`; the commissions return to the balance.
pub async fn reject_payout(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: PayoutId,
    staff: Option<UserId>,
    note: Option<&str>,
    now: DateTime<Utc>,
) -> DbResult<Option<PayoutRow>> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let row = sqlx::query_as::<_, PayoutRow>(
        "update affiliate_payouts set status = 'rejected', processed_by = $3, staff_note = $4, processed_at = $5
          where tenant_id = $1 and id = $2 and status = 'requested' returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(staff)
    .bind(note)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    if row.is_some() {
        sqlx::query(
            "update affiliate_commissions set status = 'available', payout_id = null
              where tenant_id = $1 and payout_id = $2 and status = 'withdrawn'",
        )
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// Payouts of an affiliate, newest first.
pub async fn payouts_of(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    affiliate_id: AffiliateId,
    limit: i64,
) -> DbResult<Vec<PayoutRow>> {
    Ok(sqlx::query_as::<_, PayoutRow>(
        "select * from affiliate_payouts where tenant_id = $1 and affiliate_id = $2
          order by created_at desc limit $3",
    )
    .bind(tenant_id)
    .bind(affiliate_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Admin payout queue row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct PayoutListRow {
    #[sqlx(flatten)]
    pub payout: PayoutRow,
    pub client_id: ClientId,
    pub client_name: String,
    pub code: String,
}

/// Payouts in `status` (all when `None`), oldest request first for the queue.
pub async fn list_payouts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    status: Option<PayoutStatus>,
    limit: i64,
) -> DbResult<Vec<PayoutListRow>> {
    Ok(sqlx::query_as::<_, PayoutListRow>(
        "select p.*, a.client_id,
                coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name,
                a.code::text as code
           from affiliate_payouts p
           join affiliates a on a.tenant_id = p.tenant_id and a.id = p.affiliate_id
           join clients c on c.tenant_id = a.tenant_id and c.id = a.client_id
          where p.tenant_id = $1 and ($2::text is null or p.status = $2)
          order by case when p.status = 'requested' then 0 else 1 end, p.created_at desc
          limit $3",
    )
    .bind(tenant_id)
    .bind(status)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Referrals flagged for review with their held amount.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct FlaggedReferral {
    pub id: ReferralId,
    pub affiliate_id: AffiliateId,
    pub affiliate_code: String,
    pub client_id: ClientId,
    pub client_name: String,
    pub flag: String,
    pub held: Decimal,
    pub currency: Option<Currency>,
    pub created_at: DateTime<Utc>,
}

/// Flagged referrals (review queue).
pub async fn flagged_referrals(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    limit: i64,
) -> DbResult<Vec<FlaggedReferral>> {
    Ok(sqlx::query_as::<_, FlaggedReferral>(
        "select r.id, r.affiliate_id, a.code::text as affiliate_code, r.client_id,
                coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name,
                r.flag,
                (select coalesce(sum(m.amount), 0) from affiliate_commissions m
                  where m.tenant_id = r.tenant_id and m.referral_id = r.id and m.status = 'held') as held,
                a.currency, r.created_at
           from affiliate_referrals r
           join affiliates a on a.tenant_id = r.tenant_id and a.id = r.affiliate_id
           join clients c on c.tenant_id = r.tenant_id and c.id = r.client_id
          where r.tenant_id = $1 and r.flag is not null
          order by r.created_at desc limit $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Programme totals for the admin overview: `(affiliates, referrals)`.
pub async fn programme_totals(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<(i64, i64)> {
    Ok(sqlx::query_as::<_, (i64, i64)>(
        "select (select count(*) from affiliates where tenant_id = $1),
                (select count(*) from affiliate_referrals where tenant_id = $1)",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await?)
}

/// Programme money per currency: `(currency, pending + held, available,
/// paid out)`. Amounts are never summed across currencies.
pub async fn programme_money(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(Currency, Decimal, Decimal, Decimal)>> {
    Ok(sqlx::query_as::<_, (Currency, Decimal, Decimal, Decimal)>(
        "with c as (
            select currency,
                   coalesce(sum(amount) filter (where status in ('pending', 'held')), 0) as pending,
                   coalesce(sum(amount) filter (where status = 'available'), 0) as available
              from affiliate_commissions where tenant_id = $1 group by currency),
         p as (
            select currency, coalesce(sum(amount), 0) as paid
              from affiliate_payouts where tenant_id = $1 and status = 'paid' group by currency)
         select coalesce(c.currency, p.currency), coalesce(c.pending, 0),
                coalesce(c.available, 0), coalesce(p.paid, 0)
           from c full join p on p.currency = c.currency
          order by 1",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Signals used by the self-referral rules: whether the two clients share a
/// stored payment method, and their sign-up IPs / e-mails.
pub async fn shares_payment_method(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    a: ClientId,
    b: ClientId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists(
             select 1 from payment_methods x
               join payment_methods y on y.tenant_id = x.tenant_id and y.token_hash = x.token_hash
              where x.tenant_id = $1 and x.client_id = $2 and y.client_id = $3)",
    )
    .bind(tenant_id)
    .bind(a)
    .bind(b)
    .fetch_one(db)
    .await?)
}
