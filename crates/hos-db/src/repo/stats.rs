//! Admin dashboard aggregates. Amounts in other currencies are converted to the
//! tenant's default currency with `currencies.exchange_rate`
//! (`amount_default = amount / rate`; missing rate → 1).

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::ids::TenantId;
use hos_core::money::Currency;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::{Acquire, FromRow, PgExecutor, Postgres};

use crate::error::{DbResult, not_found};

/// An amount in one currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, FromRow)]
pub struct CurrencyAmount {
    pub currency: Currency,
    pub amount: Decimal,
}

/// Headline numbers of the admin dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DashboardStats {
    /// Tenant default currency (all converted amounts are in it).
    pub currency: Currency,
    /// Monthly recurring revenue of active services + addons, converted.
    pub mrr: Decimal,
    /// MRR per billing currency (unconverted).
    pub mrr_by_currency: Vec<CurrencyAmount>,
    pub active_services: i64,
    pub active_clients: i64,
    pub new_clients_30d: i64,
    pub unpaid_invoices: i64,
    /// Open balance of unpaid / payment_pending / collections invoices, converted.
    pub unpaid_total: Decimal,
    pub overdue_invoices: i64,
    pub overdue_total: Decimal,
    /// Tickets not closed.
    pub open_tickets: i64,
    /// Tickets waiting for staff (`open`, `customer_reply`, `in_progress`).
    pub tickets_awaiting_staff: i64,
    pub pending_orders: i64,
    pub domains_expiring_30d: i64,
    /// Payments − refunds since the first day of the current month (tenant time zone), converted.
    pub revenue_this_month: Decimal,
}

#[derive(FromRow)]
struct Counters {
    active_services: i64,
    active_clients: i64,
    new_clients_30d: i64,
    unpaid_invoices: i64,
    unpaid_total: Decimal,
    overdue_invoices: i64,
    overdue_total: Decimal,
    open_tickets: i64,
    tickets_awaiting_staff: i64,
    pending_orders: i64,
    domains_expiring_30d: i64,
    revenue_this_month: Decimal,
}

/// SQL expression: months in a billing cycle column `c`.
const CYCLE_MONTHS: &str =
    "case {c} when 'monthly' then 1 when 'quarterly' then 3 when 'semiannual' then 6
     when 'annual' then 12 when 'biennial' then 24 when 'triennial' then 36 end";

fn cycle_months(col: &str) -> String {
    CYCLE_MONTHS.replace("{c}", col)
}

/// MRR per currency: active recurring services and service addons, normalised
/// to one month.
pub async fn mrr_by_currency(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<CurrencyAmount>> {
    let sql = format!(
        "select currency, round(sum(amount), 6) as amount from (
             select s.currency, s.recurring_amount / {} as amount from services s
              where s.tenant_id = $1 and s.status = 'active' and s.billing_cycle <> 'one_time'
             union all
             select a.currency, a.recurring_amount / {} from service_addons a
              where a.tenant_id = $1 and a.status = 'active' and a.billing_cycle <> 'one_time'
         ) x group by currency order by currency",
        cycle_months("s.billing_cycle"),
        cycle_months("a.billing_cycle"),
    );
    Ok(sqlx::query_as::<_, CurrencyAmount>(&sql)
        .bind(tenant_id)
        .fetch_all(db)
        .await?)
}

/// Dashboard numbers (three queries on one connection).
pub async fn dashboard<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
) -> DbResult<DashboardStats> {
    let mut conn = db.acquire().await?;
    let currency: Currency =
        sqlx::query_scalar("select default_currency from tenants where id = $1")
            .bind(tenant_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| not_found("tenant", tenant_id))?;

    let mrr_by_currency = mrr_by_currency(&mut *conn, tenant_id).await?;

    let c = sqlx::query_as::<_, Counters>(
        "with rates as (select code, exchange_rate from currencies where tenant_id = $1),
              t as (select timezone from tenants where id = $1)
         select
            (select count(*) from services where tenant_id = $1 and status = 'active') as active_services,
            (select count(*) from clients where tenant_id = $1 and status = 'active') as active_clients,
            (select count(*) from clients where tenant_id = $1 and created_at >= now() - interval '30 days')
                as new_clients_30d,
            (select count(*) from invoices where tenant_id = $1
                and status in ('unpaid', 'payment_pending', 'collections')) as unpaid_invoices,
            (select coalesce(round(sum(i.balance / coalesce(r.exchange_rate, 1)), 2), 0)
               from invoices i left join rates r on r.code = i.currency
              where i.tenant_id = $1 and i.status in ('unpaid', 'payment_pending', 'collections')) as unpaid_total,
            (select count(*) from invoices where tenant_id = $1
                and status in ('unpaid', 'payment_pending', 'collections') and due_date < current_date)
                as overdue_invoices,
            (select coalesce(round(sum(i.balance / coalesce(r.exchange_rate, 1)), 2), 0)
               from invoices i left join rates r on r.code = i.currency
              where i.tenant_id = $1 and i.status in ('unpaid', 'payment_pending', 'collections')
                and i.due_date < current_date) as overdue_total,
            (select count(*) from tickets where tenant_id = $1 and status <> 'closed') as open_tickets,
            (select count(*) from tickets where tenant_id = $1
                and status in ('open', 'customer_reply', 'in_progress')) as tickets_awaiting_staff,
            (select count(*) from orders where tenant_id = $1 and status = 'pending') as pending_orders,
            (select count(*) from domains where tenant_id = $1 and status in ('active', 'grace')
                and expires_at between now() and now() + interval '30 days') as domains_expiring_30d,
            (select coalesce(round(sum(case x.kind when 'payment' then x.amount else -x.amount end
                                       / coalesce(r.exchange_rate, 1)), 2), 0)
               from transactions x left join rates r on r.code = x.currency
              where x.tenant_id = $1 and x.status = 'succeeded'
                and x.occurred_at >= (date_trunc('month', now() at time zone (select timezone from t))
                                      at time zone (select timezone from t))) as revenue_this_month",
    )
    .bind(tenant_id)
    .fetch_one(&mut *conn)
    .await?;

    let rates: Vec<(Currency, Decimal)> =
        sqlx::query_as("select code, exchange_rate from currencies where tenant_id = $1")
            .bind(tenant_id)
            .fetch_all(&mut *conn)
            .await?;
    let mrr = mrr_by_currency
        .iter()
        .map(|m| {
            let rate = rates
                .iter()
                .find(|(c, _)| *c == m.currency)
                .map(|(_, r)| *r)
                .filter(|r| !r.is_zero())
                .unwrap_or(Decimal::ONE);
            m.amount / rate
        })
        .sum::<Decimal>()
        .round_dp(2);

    Ok(DashboardStats {
        currency,
        mrr,
        mrr_by_currency,
        active_services: c.active_services,
        active_clients: c.active_clients,
        new_clients_30d: c.new_clients_30d,
        unpaid_invoices: c.unpaid_invoices,
        unpaid_total: c.unpaid_total,
        overdue_invoices: c.overdue_invoices,
        overdue_total: c.overdue_total,
        open_tickets: c.open_tickets,
        tickets_awaiting_staff: c.tickets_awaiting_staff,
        pending_orders: c.pending_orders,
        domains_expiring_30d: c.domains_expiring_30d,
        revenue_this_month: c.revenue_this_month,
    })
}

/// Revenue of one month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, FromRow)]
pub struct MonthlyRevenue {
    /// First day of the month (tenant time zone).
    pub month: NaiveDate,
    /// Succeeded payments − refunds/chargebacks, converted to the default currency.
    pub amount: Decimal,
}

/// Revenue of the last `months` months including the current one, oldest
/// first, with zero for months without payments (chart series).
pub async fn monthly_revenue(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    months: i32,
) -> DbResult<Vec<MonthlyRevenue>> {
    let months = months.clamp(1, 120);
    Ok(sqlx::query_as::<_, MonthlyRevenue>(
        "with t as (select timezone from tenants where id = $1),
              m as (
                  select generate_series(
                      date_trunc('month', now() at time zone (select timezone from t)) - make_interval(months => $2 - 1),
                      date_trunc('month', now() at time zone (select timezone from t)),
                      interval '1 month')::date as month
              ),
              x as (
                  select date_trunc('month', tr.occurred_at at time zone (select timezone from t))::date as month,
                         sum(case tr.kind when 'payment' then tr.amount else -tr.amount end
                             / coalesce(r.exchange_rate, 1)) as amount
                  from transactions tr
                  left join currencies r on r.tenant_id = tr.tenant_id and r.code = tr.currency
                  where tr.tenant_id = $1 and tr.status = 'succeeded'
                    and tr.occurred_at >= ((select min(month) from m)::timestamp at time zone (select timezone from t))
                  group by 1
              )
         select m.month, coalesce(round(x.amount, 2), 0) as amount
         from m left join x on x.month = m.month
         order by m.month",
    )
    .bind(tenant_id)
    .bind(months)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// client area / platform console (added for hos-web)
// ---------------------------------------------------------------------------

/// Numbers for the client-area dashboard of one client account.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
pub struct ClientSummary {
    /// The client's billing currency.
    pub currency: Currency,
    /// Services in `active` state.
    pub active_services: i64,
    /// Services not yet active or suspended (pending / suspended).
    pub attention_services: i64,
    /// Domains in `active` / `grace` state.
    pub active_domains: i64,
    /// Of those, expiring within 30 days.
    pub domains_expiring_30d: i64,
    /// Unpaid invoices.
    pub unpaid_invoices: i64,
    /// Their outstanding balance (client currency).
    pub unpaid_total: Decimal,
    /// Tickets that are not closed.
    pub open_tickets: i64,
}

/// Dashboard numbers of `client_id` (one query).
pub async fn client_summary(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: hos_core::ids::ClientId,
) -> DbResult<ClientSummary> {
    sqlx::query_as::<_, ClientSummary>(
        "select c.currency,
            (select count(*) from services s where s.tenant_id = $1 and s.client_id = $2
                and s.status = 'active') as active_services,
            (select count(*) from services s where s.tenant_id = $1 and s.client_id = $2
                and s.status in ('pending', 'suspended')) as attention_services,
            (select count(*) from domains d where d.tenant_id = $1 and d.client_id = $2
                and d.status in ('active', 'grace')) as active_domains,
            (select count(*) from domains d where d.tenant_id = $1 and d.client_id = $2
                and d.status in ('active', 'grace')
                and d.expires_at between now() and now() + interval '30 days') as domains_expiring_30d,
            (select count(*) from invoices i where i.tenant_id = $1 and i.client_id = $2
                and i.status in ('unpaid', 'payment_pending', 'collections')) as unpaid_invoices,
            (select coalesce(sum(i.balance), 0) from invoices i where i.tenant_id = $1 and i.client_id = $2
                and i.status in ('unpaid', 'payment_pending', 'collections')) as unpaid_total,
            (select count(*) from tickets t where t.tenant_id = $1 and t.client_id = $2
                and t.status <> 'closed') as open_tickets
         from clients c where c.tenant_id = $1 and c.id = $2",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| not_found("client", client_id))
}

/// Cross-tenant numbers for the platform console.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct PlatformOverview {
    /// All tenants.
    pub tenants: i64,
    /// Tenants in `active` state.
    pub active_tenants: i64,
    /// Tenants in `trial`.
    pub trial_tenants: i64,
    /// Suspended tenants.
    pub suspended_tenants: i64,
    /// Tenants created in the last 30 days.
    pub new_tenants_30d: i64,
    /// Active client accounts across tenants.
    pub clients: i64,
    /// Active services across tenants.
    pub active_services: i64,
    /// Open tickets across tenants.
    pub open_tickets: i64,
}

/// Platform-wide counters (one query; no tenant filter by design).
pub async fn platform_overview(db: impl PgExecutor<'_>) -> DbResult<PlatformOverview> {
    Ok(sqlx::query_as::<_, PlatformOverview>(
        "select
            (select count(*) from tenants) as tenants,
            (select count(*) from tenants where status = 'active') as active_tenants,
            (select count(*) from tenants where status = 'trial') as trial_tenants,
            (select count(*) from tenants where status = 'suspended') as suspended_tenants,
            (select count(*) from tenants where created_at >= now() - interval '30 days') as new_tenants_30d,
            (select count(*) from clients where status = 'active') as clients,
            (select count(*) from services where status = 'active') as active_services,
            (select count(*) from tickets where status <> 'closed') as open_tickets",
    )
    .fetch_one(db)
    .await?)
}

// ---------------------------------------------------------------------------
// KPI trends (admin dashboard: sparklines, period comparison)
// ---------------------------------------------------------------------------

/// Window of the dashboard KPI trends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KpiRange {
    /// 7 daily buckets.
    Week,
    /// 30 daily buckets (default).
    Month,
    /// 13 weekly buckets.
    Quarter,
    /// 52 weekly buckets.
    Year,
}

impl KpiRange {
    /// From a query value (`7d|30d|90d|12m`; anything else: 30 days).
    pub fn parse(raw: &str) -> Self {
        match raw {
            "7d" => Self::Week,
            "90d" => Self::Quarter,
            "12m" => Self::Year,
            _ => Self::Month,
        }
    }

    /// Query value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Quarter => "90d",
            Self::Year => "12m",
        }
    }

    /// `(buckets, bucket length in days)`.
    pub fn buckets(self) -> (usize, i64) {
        match self {
            Self::Week => (7, 1),
            Self::Month => (30, 1),
            Self::Quarter => (13, 7),
            Self::Year => (52, 7),
        }
    }
}

/// One KPI over the window: per-bucket values plus the current and previous
/// period figure. Stocks (active services, open tickets, unpaid …) are the
/// value at each bucket's end and at the end of each period; flows (new
/// clients, cancellations, MRR movement) are per-bucket sums and period sums.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct KpiSeries {
    /// Values of the current period, oldest first.
    pub points: Vec<f64>,
    /// Current period figure.
    pub current: f64,
    /// Previous period figure (same length, right before).
    pub previous: f64,
}

/// All dashboard KPI trends.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KpiTrends {
    /// Window.
    #[serde(skip)]
    pub range: KpiRange,
    /// First bucket start (UTC).
    pub from: DateTime<Utc>,
    /// End of the window (now).
    pub to: DateTime<Utc>,
    /// Active services (stock).
    pub active_services: KpiSeries,
    /// New clients (flow).
    pub new_clients: KpiSeries,
    /// Unpaid invoice total, default currency (stock).
    pub unpaid_total: KpiSeries,
    /// Overdue invoice total, default currency (stock).
    pub overdue_total: KpiSeries,
    /// Open tickets (stock).
    pub open_tickets: KpiSeries,
    /// Services cancelled / terminated (flow).
    pub cancellations: KpiSeries,
    /// MRR, default currency (stock).
    pub mrr: KpiSeries,
    /// Net MRR movement: new − churned MRR (flow).
    pub mrr_net: KpiSeries,
}

/// Per-bucket ins / outs of an event table (`b = -1`: stock before the
/// window, at the previous period's start).
#[derive(FromRow)]
struct BucketRow {
    b: i32,
    v_in: f64,
    v_out: f64,
}

/// Rows of `(start, end, weight)` events → bucket aggregates over
/// `[prev_start, now)`. One pass over the tenant's rows; the `events` CTE
/// gets `$1` tenant, `$2` previous-period start, `$3` bucket seconds,
/// `$4` end of the window.
async fn bucket_events(
    conn: &mut sqlx::PgConnection,
    events: &str,
    tenant_id: TenantId,
    prev_start: DateTime<Utc>,
    step_secs: i64,
    end: DateTime<Utc>,
) -> DbResult<Vec<BucketRow>> {
    let sql = format!(
        "with rates as (select code, exchange_rate from currencies where tenant_id = $1),
              t as (select timezone from tenants where id = $1),
              ev as ({events}),
              e as (select st, en, w from ev where en is null or en > st)
         select -1 as b, coalesce(sum(w) filter (where st < $2 and (en is null or en >= $2)), 0)::float8 as v_in,
                0::float8 as v_out
           from e
         union all
         select floor(extract(epoch from (st - $2)) / $3)::int, sum(w)::float8, 0::float8
           from e where st >= $2 and st < $4 group by 1
         union all
         select floor(extract(epoch from (en - $2)) / $3)::int, 0::float8, sum(w)::float8
           from e where en >= $2 and en < $4 group by 1"
    );
    Ok(sqlx::query_as::<_, BucketRow>(&sql)
        .bind(tenant_id)
        .bind(prev_start)
        .bind(step_secs as f64)
        .bind(end)
        .fetch_all(&mut *conn)
        .await?)
}

/// Builds the stock and the in / out flow series of `2n` buckets (the
/// first `n` are the previous period).
fn series_of(rows: &[BucketRow], n: usize) -> (KpiSeries, KpiSeries, KpiSeries, Vec<f64>) {
    let mut ins = vec![0.0; 2 * n];
    let mut outs = vec![0.0; 2 * n];
    let mut base = 0.0;
    for r in rows {
        if r.b < 0 {
            base += r.v_in;
        } else if let Some(i) = usize::try_from(r.b).ok().filter(|i| *i < 2 * n) {
            ins[i] += r.v_in;
            outs[i] += r.v_out;
        }
    }
    let mut stock = Vec::with_capacity(2 * n);
    let mut v = base;
    for i in 0..2 * n {
        v += ins[i] - outs[i];
        stock.push(v);
    }
    let cur = |xs: &[f64]| xs[n..].to_vec();
    let sum = |xs: &[f64]| xs.iter().sum::<f64>();
    let stock_s = KpiSeries {
        points: cur(&stock),
        current: stock[2 * n - 1],
        previous: stock[n - 1],
    };
    let in_s = KpiSeries {
        points: cur(&ins),
        current: sum(&ins[n..]),
        previous: sum(&ins[..n]),
    };
    let out_s = KpiSeries {
        points: cur(&outs),
        current: sum(&outs[n..]),
        previous: sum(&outs[..n]),
    };
    let net: Vec<f64> = (0..2 * n).map(|i| ins[i] - outs[i]).collect();
    (stock_s, in_s, out_s, net)
}

/// KPI trends of the admin dashboard for `range` ending now. Five
/// aggregate queries (services, MRR, clients, invoices ×2, tickets) on one
/// connection, each a single pass over the tenant's rows of one table.
pub async fn kpi_trends<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    range: KpiRange,
) -> DbResult<KpiTrends> {
    let mut conn = db.acquire().await?;
    let (n, days) = range.buckets();
    let step = chrono::Duration::days(days);
    let end = Utc::now();
    let start = end - step * n as i32;
    let prev_start = start - step * n as i32;
    let secs = step.num_seconds();
    let months = cycle_months("s.billing_cycle");

    let services = bucket_events(
        &mut conn,
        "select s.created_at as st, coalesce(s.terminated_at, s.cancelled_at) as en, 1::float8 as w
           from services s where s.tenant_id = $1 and s.status not in ('pending', 'failed')",
        tenant_id,
        prev_start,
        secs,
        end,
    )
    .await?;
    let (active_services, _, cancellations, _) = series_of(&services, n);

    let mrr_events = format!(
        "select s.created_at as st, coalesce(s.terminated_at, s.cancelled_at) as en,
                (s.recurring_amount / {months} / coalesce(r.exchange_rate, 1))::float8 as w
           from services s left join rates r on r.code = s.currency
          where s.tenant_id = $1 and s.status not in ('pending', 'failed')
            and s.billing_cycle <> 'one_time'"
    );
    let mrr_rows = bucket_events(&mut conn, &mrr_events, tenant_id, prev_start, secs, end).await?;
    let (mrr, _, _, net) = series_of(&mrr_rows, n);
    let mrr_net = KpiSeries {
        points: net[n..].to_vec(),
        current: net[n..].iter().sum(),
        previous: net[..n].iter().sum(),
    };

    let clients = bucket_events(
        &mut conn,
        "select c.created_at as st, c.closed_at as en, 1::float8 as w
           from clients c where c.tenant_id = $1",
        tenant_id,
        prev_start,
        secs,
        end,
    )
    .await?;
    let (_, new_clients, _, _) = series_of(&clients, n);

    let unpaid = bucket_events(
        &mut conn,
        "select (i.issue_date::timestamp at time zone (select timezone from t)) as st,
                coalesce(i.paid_at, i.cancelled_at, i.refunded_at) as en,
                (i.total / coalesce(r.exchange_rate, 1))::float8 as w
           from invoices i left join rates r on r.code = i.currency
          where i.tenant_id = $1 and i.status <> 'draft'",
        tenant_id,
        prev_start,
        secs,
        end,
    )
    .await?;
    let (unpaid_total, _, _, _) = series_of(&unpaid, n);
    let overdue = bucket_events(
        &mut conn,
        "select ((i.due_date + 1)::timestamp at time zone (select timezone from t)) as st,
                coalesce(i.paid_at, i.cancelled_at, i.refunded_at) as en,
                (i.total / coalesce(r.exchange_rate, 1))::float8 as w
           from invoices i left join rates r on r.code = i.currency
          where i.tenant_id = $1 and i.status <> 'draft'",
        tenant_id,
        prev_start,
        secs,
        end,
    )
    .await?;
    let (overdue_total, _, _, _) = series_of(&overdue, n);

    let tickets = bucket_events(
        &mut conn,
        "select k.created_at as st, k.closed_at as en, 1::float8 as w
           from tickets k where k.tenant_id = $1",
        tenant_id,
        prev_start,
        secs,
        end,
    )
    .await?;
    let (open_tickets, _, _, _) = series_of(&tickets, n);

    Ok(KpiTrends {
        range,
        from: start,
        to: end,
        active_services,
        new_clients,
        unpaid_total,
        overdue_total,
        open_tickets,
        cancellations,
        mrr,
        mrr_net,
    })
}

/// The tenant's default currency (amounts of [`kpi_trends`] are in it).
pub async fn dashboard_currency(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Currency> {
    sqlx::query_scalar("select default_currency from tenants where id = $1")
        .bind(tenant_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| not_found("tenant", tenant_id))
}

#[cfg(test)]
mod kpi_tests {
    use super::*;

    #[test]
    fn stocks_and_flows_from_buckets() {
        // 2 buckets per period: 3 rows active before the window, one added
        // in previous bucket 1, one added + one removed in current bucket 2.
        let rows = vec![
            BucketRow {
                b: -1,
                v_in: 3.0,
                v_out: 0.0,
            },
            BucketRow {
                b: 1,
                v_in: 1.0,
                v_out: 0.0,
            },
            BucketRow {
                b: 2,
                v_in: 1.0,
                v_out: 0.0,
            },
            BucketRow {
                b: 2,
                v_in: 0.0,
                v_out: 1.0,
            },
        ];
        let (stock, ins, outs, net) = series_of(&rows, 2);
        assert_eq!(stock.points, vec![4.0, 4.0]);
        assert_eq!((stock.current, stock.previous), (4.0, 4.0));
        assert_eq!((ins.current, ins.previous), (1.0, 1.0));
        assert_eq!((outs.current, outs.previous), (1.0, 0.0));
        assert_eq!(net, vec![0.0, 1.0, 0.0, 0.0]);
        assert_eq!(KpiRange::parse("90d").buckets(), (13, 7));
        assert_eq!(KpiRange::parse("nope"), KpiRange::Month);
    }
}
