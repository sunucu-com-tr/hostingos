//! Reports (`/admin/reports*`, `reports.view`).
//!
//! | Tab | Content |
//! |---|---|
//! | `revenue` | payments / refunds / net per day·week·month·year; by product, product group, gateway |
//! | `growth` | MRR / ARR per currency, churn, new / ended services and new clients per period |
//! | `tax` | taxable base + tax per rate (cash or accrual basis) |
//! | `aging` | open receivables: not due, 1–30, 31–60, 61–90, 90+ days |
//! | `products` | best sellers |
//! | `domains` | TLD distribution, upcoming renewals |
//!
//! Every page takes a date range and a currency. **Amounts are never summed
//! across currencies**: tables keep one row per currency, charts (inline SVG
//! from [`super::chart`]) show the selected currency only. Each table has a
//! CSV export (`?format=csv&table=…`, formula-escaped by `common::csv_cell`).

use askama::Template;
use axum::Router;
use axum::extract::{Query, State};
use axum::response::{Redirect, Response};
use axum::routing::get;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use hos_core::domain::Locale;
use hos_core::money::Currency;
use hos_db::repo::admin_settings::{
    self as srepo, FlowRow, GroupRevenueRow, RenewalRow, TldShareRow,
};
use hos_db::repo::catalog;
use hos_db::repo::reports::{
    AgingRow, GatewayRevenueRow, Granularity, ProductRevenueRow, RevenueRow, TaxReportRow,
    TopProductRow,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::chart::{self, Point};
use super::common::{self, Ui as _, csv_response};
use crate::error::AppResult;
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render};

const BASE: &str = "/admin/reports";
/// Longest accepted range.
const MAX_DAYS: i64 = 3 * 366;
/// Aging bucket labels, in [`AgingRow`] column order.
const BUCKETS: [&str; 5] = [
    "admin_settings.reports.not_due",
    "admin_settings.reports.days_1_30",
    "admin_settings.reports.days_31_60",
    "admin_settings.reports.days_61_90",
    "admin_settings.reports.days_90",
];

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            BASE,
            get(|| async { Redirect::to("/admin/reports/revenue") }),
        )
        .route("/admin/reports/revenue", get(revenue))
        .route("/admin/reports/growth", get(growth))
        .route("/admin/reports/tax", get(tax))
        .route("/admin/reports/aging", get(aging))
        .route("/admin/reports/products", get(products))
        .route("/admin/reports/domains", get(domains))
}

// ---------------------------------------------------------------------------
// filters
// ---------------------------------------------------------------------------

/// Query string of every report.
#[derive(Debug, Default, Deserialize)]
pub struct ReportQuery {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    basis: Option<String>,
    #[serde(default)]
    days: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    table: Option<String>,
}

/// Parsed filters (also rendered back into the filter form).
#[derive(Debug, Clone)]
pub struct Filters {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub currency: Currency,
    pub unit: &'static str,
    /// Currencies of the select.
    pub currencies: Vec<(String, String)>,
    /// `true` when the given range had to be corrected.
    pub adjusted: bool,
    pub csv: bool,
    pub table: String,
    /// Query string without `format` / `table` (export links).
    pub qs: String,
}

impl Filters {
    /// `[from 00:00, to + 1 day 00:00)` in the tenant's time zone, as UTC.
    pub fn range(&self, tz: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        let zone = crate::i18n::parse_tz(tz);
        let at = |d: NaiveDate| {
            zone.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap_or_default())
                .earliest()
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or_else(|| {
                    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap_or_default())
                })
        };
        (at(self.from), at(self.to + Duration::days(1)))
    }

    fn granularity(&self) -> Granularity {
        match self.unit {
            "day" => Granularity::Day,
            "week" => Granularity::Week,
            "year" => Granularity::Year,
            _ => Granularity::Month,
        }
    }

    /// Export link for `table`.
    pub fn export(&self, path: &str, table: &str) -> String {
        format!("{path}?{}&format=csv&table={table}", self.qs)
    }
}

/// Parses the query: defaults to the last 12 months in the tenant currency;
/// swaps a reversed range, caps it at three years and coarsens a unit that
/// would produce too many buckets.
async fn filters(state: &AppState, tenant: &Tenant, q: &ReportQuery) -> AppResult<Filters> {
    let today = common::today(&tenant.timezone);
    let parse = |s: &Option<String>| {
        s.as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok())
    };
    let mut adjusted = false;
    let to = parse(&q.to).unwrap_or(today);
    // Default: the last twelve calendar months, current month included.
    let from = parse(&q.from).unwrap_or_else(|| {
        to.with_day(1)
            .and_then(|d| d.checked_sub_months(chrono::Months::new(11)))
            .unwrap_or(to - Duration::days(365))
    });
    let (mut from, mut to) = (from, to);
    if from > to {
        std::mem::swap(&mut from, &mut to);
        adjusted = true;
    }
    if (to - from).num_days() > MAX_DAYS {
        from = to - Duration::days(MAX_DAYS);
        adjusted = true;
    }
    let span = (to - from).num_days();
    let mut unit: &'static str = match q.unit.as_deref() {
        Some("day") => "day",
        Some("week") => "week",
        Some("year") => "year",
        _ => "month",
    };
    if unit == "day" && span > 92 {
        unit = "week";
        adjusted = true;
    }
    if unit == "week" && span > 730 {
        unit = "month";
        adjusted = true;
    }
    let rows = catalog::list_currencies(&state.pool, tenant.id, false).await?;
    let mut currencies: Vec<(String, String)> = rows
        .iter()
        .map(|c| {
            (
                c.code.as_str().to_owned(),
                format!("{} · {}", c.code.as_str(), c.name),
            )
        })
        .collect();
    if currencies.is_empty() {
        let c = tenant.default_currency.as_str().to_owned();
        currencies.push((c.clone(), c));
    }
    let currency = q
        .currency
        .as_deref()
        .and_then(|c| c.parse::<Currency>().ok())
        .filter(|c| currencies.iter().any(|(v, _)| v == c.as_str()))
        .unwrap_or(tenant.default_currency);
    let mut qs = url::form_urlencoded::Serializer::new(String::new());
    qs.append_pair("from", &from.to_string())
        .append_pair("to", &to.to_string())
        .append_pair("currency", currency.as_str())
        .append_pair("unit", unit);
    if let Some(b) = q.basis.as_deref().filter(|b| *b == "accrual") {
        qs.append_pair("basis", b);
    }
    if let Some(d) = q.days.as_deref().filter(|d| !d.is_empty()) {
        qs.append_pair("days", d);
    }
    Ok(Filters {
        from,
        to,
        currency,
        unit,
        currencies,
        adjusted,
        csv: q.format.as_deref() == Some("csv"),
        table: q.table.clone().unwrap_or_default(),
        qs: qs.finish(),
    })
}

fn tabs(ctx: &PageCtx, f: &Filters) -> Vec<NavItem> {
    let q = |p: &str| format!("{p}?{}", f.qs);
    common::tabs(
        ctx,
        &[
            (
                "admin_settings.reports.tab_revenue",
                q("/admin/reports/revenue"),
            ),
            (
                "admin_settings.reports.tab_growth",
                q("/admin/reports/growth"),
            ),
            ("admin_settings.reports.tab_tax", q("/admin/reports/tax")),
            (
                "admin_settings.reports.tab_aging",
                q("/admin/reports/aging"),
            ),
            (
                "admin_settings.reports.tab_products",
                q("/admin/reports/products"),
            ),
            (
                "admin_settings.reports.tab_domains",
                q("/admin/reports/domains"),
            ),
        ],
    )
}

/// Period label for the chosen unit.
fn period_label(ctx: &PageCtx, unit: &str, d: NaiveDate) -> String {
    let tr = ctx.locale == Locale::Tr;
    match unit {
        "year" => d.year().to_string(),
        "month" if tr => format!("{:02}.{}", d.month(), d.year()),
        "month" => format!("{}-{:02}", d.year(), d.month()),
        _ => hos_billing::format::date(d, ctx.locale),
    }
}

fn dec(d: &Decimal) -> String {
    d.round_dp(2).to_string()
}

/// Shared page parts.
struct Shell {
    tabs: Vec<NavItem>,
    active: String,
    filters: Filters,
    path: &'static str,
    /// Other currencies that have data in the range (shown as a hint; their
    /// amounts are never added to the selected currency).
    others: Vec<String>,
    /// `(value, label)` of the period unit select.
    units: Vec<(&'static str, String)>,
}

impl Shell {
    fn export(&self, table: &str) -> String {
        self.filters.export(self.path, table)
    }
}

fn shell(
    ctx: &mut PageCtx,
    staff: &CurrentStaff,
    f: Filters,
    path: &'static str,
    title_key: &str,
    others: Vec<String>,
) -> Shell {
    let title = ctx.t(title_key);
    common::page(
        ctx,
        staff,
        &title,
        &[("admin_settings.reports.title", BASE)],
    );
    Shell {
        tabs: tabs(ctx, &f),
        active: ctx.url(format!("{path}?{}", f.qs)),
        filters: f,
        path,
        others,
        units: ["day", "week", "month", "year"]
            .into_iter()
            .map(|u| (u, ctx.t(&format!("admin_settings.reports.unit_{u}"))))
            .collect(),
    }
}

/// Distinct currencies other than `selected`, sorted.
fn other_currencies<'a>(
    selected: Currency,
    found: impl Iterator<Item = &'a Currency>,
) -> Vec<String> {
    let mut v: Vec<String> = found
        .filter(|c| **c != selected)
        .map(|c| c.as_str().to_owned())
        .collect();
    v.sort();
    v.dedup();
    v
}

// ---------------------------------------------------------------------------
// revenue
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/reports/revenue.html")]
struct RevenuePage {
    ctx: PageCtx,
    s: Shell,
    periods: Vec<RevenueRow>,
    products: Vec<ProductRevenueRow>,
    groups: Vec<GroupRevenueRow>,
    gateways: Vec<GatewayRevenueRow>,
    chart_svg: String,
    product_svg: String,
}
crate::page!(RevenuePage);

impl RevenuePage {
    fn period(&self, d: &NaiveDate) -> String {
        period_label(&self.ctx, self.s.filters.unit, *d)
    }

    fn group(&self, r: &GroupRevenueRow) -> String {
        group_label(&self.ctx, r)
    }
}

/// Product group name, or "Domains" / "Ungrouped" for lines without one.
fn group_label(ctx: &PageCtx, r: &GroupRevenueRow) -> String {
    match &r.group_name {
        Some(n) => n.clone(),
        None if r.is_domain => ctx.t("admin_settings.reports.group_domains"),
        None => ctx.t("admin_settings.reports.ungrouped"),
    }
}

async fn revenue(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ReportsView>,
    Query(q): Query<ReportQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let f = filters(&state, &tenant, &q).await?;
    let (from, to) = f.range(&tenant.timezone);
    let mut periods = state
        .billing
        .report_revenue(tenant.id, from, to, f.granularity())
        .await?;
    let mut products = state
        .billing
        .report_revenue_by_product(tenant.id, from, to)
        .await?;
    let mut groups = srepo::revenue_by_product_group(&state.pool, tenant.id, from, to).await?;
    let mut gateways = state
        .billing
        .report_revenue_by_gateway(tenant.id, from, to)
        .await?;
    let others = other_currencies(
        f.currency,
        periods
            .iter()
            .map(|r| &r.currency)
            .chain(products.iter().map(|r| &r.currency))
            .chain(gateways.iter().map(|r| &r.currency)),
    );
    // One currency per view: amounts in different currencies are never mixed.
    let cur = f.currency;
    periods.retain(|r| r.currency == cur);
    products.retain(|r| r.currency == cur);
    groups.retain(|r| r.currency == cur);
    gateways.retain(|r| r.currency == cur);
    if f.csv {
        return Ok(match f.table.as_str() {
            "product" => csv_response(
                "revenue-by-product.csv",
                &[
                    ctx.t("admin_settings.reports.product"),
                    ctx.t("admin.common.currency"),
                    ctx.t("admin.common.amount"),
                    ctx.t("admin_settings.reports.lines"),
                ],
                &products
                    .iter()
                    .map(|r| {
                        vec![
                            r.product_name.clone(),
                            r.currency.as_str().to_owned(),
                            dec(&r.amount),
                            r.line_count.to_string(),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ),
            "group" => csv_response(
                "revenue-by-group.csv",
                &[
                    ctx.t("admin_settings.reports.group"),
                    ctx.t("admin.common.currency"),
                    ctx.t("admin.common.amount"),
                    ctx.t("admin_settings.reports.lines"),
                ],
                &groups
                    .iter()
                    .map(|r| {
                        vec![
                            group_label(&ctx, r),
                            r.currency.as_str().to_owned(),
                            dec(&r.amount),
                            r.line_count.to_string(),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ),
            "gateway" => csv_response(
                "revenue-by-gateway.csv",
                &[
                    ctx.t("admin_settings.reports.gateway"),
                    ctx.t("admin.common.currency"),
                    ctx.t("admin.common.amount"),
                    ctx.t("admin_settings.reports.fees"),
                    ctx.t("admin_settings.reports.payment_count"),
                ],
                &gateways
                    .iter()
                    .map(|r| {
                        vec![
                            r.gateway.clone(),
                            r.currency.as_str().to_owned(),
                            dec(&r.amount),
                            dec(&r.fees),
                            r.payment_count.to_string(),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => csv_response(
                "revenue-by-period.csv",
                &[
                    ctx.t("admin_settings.reports.period"),
                    ctx.t("admin.common.currency"),
                    ctx.t("admin_settings.reports.payments"),
                    ctx.t("admin_settings.reports.refunds"),
                    ctx.t("admin_settings.reports.chargebacks"),
                    ctx.t("admin_settings.reports.fees"),
                    ctx.t("admin_settings.reports.net"),
                    ctx.t("admin_settings.reports.payment_count"),
                ],
                &periods
                    .iter()
                    .map(|r| {
                        vec![
                            r.period.to_string(),
                            r.currency.as_str().to_owned(),
                            dec(&r.payments),
                            dec(&r.refunds),
                            dec(&r.chargebacks),
                            dec(&r.fees),
                            dec(&r.net),
                            r.payment_count.to_string(),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ),
        });
    }
    let tr = ctx.locale == Locale::Tr;
    let points: Vec<Point> = periods
        .iter()
        .map(|r| Point {
            label: period_label(&ctx, f.unit, r.period),
            value: r.net.max(Decimal::ZERO),
            display: ctx.money_in(&r.net, &r.currency),
        })
        .collect();
    let chart_svg = if points.is_empty() {
        String::new()
    } else {
        chart::columns(
            &points,
            &ctx.tv(
                "admin_settings.reports.net_chart",
                "currency",
                f.currency.as_str(),
            ),
            tr,
        )
    };
    let top: Vec<Point> = products
        .iter()
        .take(10)
        .map(|r| Point {
            label: r.product_name.clone(),
            value: r.amount,
            display: ctx.money_in(&r.amount, &r.currency),
        })
        .collect();
    let product_svg = if top.is_empty() {
        String::new()
    } else {
        chart::hbars(
            &top,
            &ctx.tv(
                "admin_settings.reports.product_chart",
                "currency",
                f.currency.as_str(),
            ),
        )
    };
    let s = shell(
        &mut ctx,
        &staff,
        f,
        "/admin/reports/revenue",
        "admin_settings.reports.tab_revenue",
        others,
    );
    render(&RevenuePage {
        ctx,
        s,
        periods,
        products,
        groups,
        gateways,
        chart_svg,
        product_svg,
    })
}

// ---------------------------------------------------------------------------
// growth
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/reports/growth.html")]
struct GrowthPage {
    ctx: PageCtx,
    s: Shell,
    metrics: hos_billing::reports::SubscriptionMetrics,
    flow: Vec<FlowRow>,
    chart_svg: String,
}
crate::page!(GrowthPage);

impl GrowthPage {
    fn period(&self, d: &NaiveDate) -> String {
        period_label(&self.ctx, self.s.filters.unit, *d)
    }
}

async fn growth(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ReportsView>,
    Query(q): Query<ReportQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let f = filters(&state, &tenant, &q).await?;
    let (from, to) = f.range(&tenant.timezone);
    let metrics = state
        .billing
        .report_subscriptions(tenant.id, from, to)
        .await?;
    let flow = srepo::acquisition_flow(&state.pool, tenant.id, from, to, f.unit).await?;
    if f.csv {
        return Ok(csv_response(
            "growth.csv",
            &[
                ctx.t("admin_settings.reports.period"),
                ctx.t("admin_settings.reports.new_clients"),
                ctx.t("admin_settings.reports.new_services"),
                ctx.t("admin_settings.reports.ended_services"),
            ],
            &flow
                .iter()
                .map(|r| {
                    vec![
                        r.period.to_string(),
                        r.new_clients.to_string(),
                        r.new_services.to_string(),
                        r.ended_services.to_string(),
                    ]
                })
                .collect::<Vec<_>>(),
        ));
    }
    let points: Vec<Point> = flow
        .iter()
        .map(|r| Point {
            label: period_label(&ctx, f.unit, r.period),
            value: Decimal::from(r.new_services),
            display: ctx.num(r.new_services),
        })
        .collect();
    let chart_svg = if points.iter().all(|p| p.value.is_zero()) {
        String::new()
    } else {
        chart::columns(
            &points,
            &ctx.t("admin_settings.reports.new_services"),
            ctx.locale == Locale::Tr,
        )
    };
    let s = shell(
        &mut ctx,
        &staff,
        f,
        "/admin/reports/growth",
        "admin_settings.reports.tab_growth",
        Vec::new(),
    );
    render(&GrowthPage {
        ctx,
        s,
        metrics,
        flow,
        chart_svg,
    })
}

// ---------------------------------------------------------------------------
// tax
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/reports/tax.html")]
struct TaxPage {
    ctx: PageCtx,
    s: Shell,
    rows: Vec<TaxReportRow>,
    accrual: bool,
}
crate::page!(TaxPage);

async fn tax(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ReportsView>,
    Query(q): Query<ReportQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let f = filters(&state, &tenant, &q).await?;
    let accrual = q.basis.as_deref() == Some("accrual");
    let mut rows = state
        .billing
        .report_taxes(tenant.id, f.from, f.to, !accrual)
        .await?;
    let others = other_currencies(f.currency, rows.iter().map(|r| &r.currency));
    let cur = f.currency;
    rows.retain(|r| r.currency == cur);
    if f.csv {
        return Ok(csv_response(
            "tax.csv",
            &[
                ctx.t("admin_settings.reports.tax_name"),
                ctx.t("admin_settings.reports.rate"),
                ctx.t("admin.common.currency"),
                ctx.t("admin_settings.reports.taxable"),
                ctx.t("admin_settings.reports.tax_amount"),
                ctx.t("admin_settings.reports.invoice_count"),
            ],
            &rows
                .iter()
                .map(|r| {
                    vec![
                        r.name.clone(),
                        dec(&r.rate),
                        r.currency.as_str().to_owned(),
                        dec(&r.taxable_amount),
                        dec(&r.tax_amount),
                        r.invoice_count.to_string(),
                    ]
                })
                .collect::<Vec<_>>(),
        ));
    }
    let s = shell(
        &mut ctx,
        &staff,
        f,
        "/admin/reports/tax",
        "admin_settings.reports.tab_tax",
        others,
    );
    render(&TaxPage {
        ctx,
        s,
        rows,
        accrual,
    })
}

// ---------------------------------------------------------------------------
// aging
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/reports/aging.html")]
struct AgingPage {
    ctx: PageCtx,
    s: Shell,
    rows: Vec<AgingRow>,
    chart_svg: String,
    as_of: NaiveDate,
}
crate::page!(AgingPage);

async fn aging(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ReportsView>,
    Query(q): Query<ReportQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let f = filters(&state, &tenant, &q).await?;
    // Receivables are a snapshot: "as of" the end of the range.
    let as_of = f.to;
    let rows = state.billing.report_aging(tenant.id, Some(as_of)).await?;
    let buckets = |r: &AgingRow| {
        BUCKETS
            .iter()
            .zip([
                r.current,
                r.days_1_30,
                r.days_31_60,
                r.days_61_90,
                r.days_over_90,
            ])
            .map(|(k, a)| (*k, a))
            .collect::<Vec<_>>()
    };
    if f.csv {
        let mut head = vec![ctx.t("admin.common.currency")];
        head.extend(BUCKETS.iter().map(|k| ctx.t(k)));
        head.push(ctx.t("admin.common.total"));
        head.push(ctx.t("admin_settings.reports.invoice_count"));
        return Ok(csv_response(
            "aging.csv",
            &head,
            &rows
                .iter()
                .map(|r| {
                    let mut v = vec![r.currency.as_str().to_owned()];
                    v.extend(buckets(r).iter().map(|(_, a)| dec(a)));
                    v.push(dec(&r.total));
                    v.push(r.invoice_count.to_string());
                    v
                })
                .collect::<Vec<_>>(),
        ));
    }
    let chart_svg = rows
        .iter()
        .find(|r| r.currency == f.currency)
        .map(|r| {
            let pts: Vec<Point> = buckets(r)
                .iter()
                .map(|(k, a)| Point {
                    label: ctx.t(k),
                    value: *a,
                    display: ctx.money_in(a, &r.currency),
                })
                .collect();
            chart::hbars(
                &pts,
                &ctx.tv(
                    "admin_settings.reports.aging_chart",
                    "currency",
                    r.currency.as_str(),
                ),
            )
        })
        .unwrap_or_default();
    let s = shell(
        &mut ctx,
        &staff,
        f,
        "/admin/reports/aging",
        "admin_settings.reports.tab_aging",
        Vec::new(),
    );
    render(&AgingPage {
        ctx,
        s,
        rows,
        chart_svg,
        as_of,
    })
}

// ---------------------------------------------------------------------------
// products
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/reports/products.html")]
struct ProductsPage {
    ctx: PageCtx,
    s: Shell,
    rows: Vec<TopProductRow>,
    chart_svg: String,
}
crate::page!(ProductsPage);

async fn products(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ReportsView>,
    Query(q): Query<ReportQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let f = filters(&state, &tenant, &q).await?;
    let (from, to) = f.range(&tenant.timezone);
    let mut rows = state
        .billing
        .report_top_products(tenant.id, from, to, 200)
        .await?;
    let others = other_currencies(f.currency, rows.iter().map(|r| &r.currency));
    let cur = f.currency;
    rows.retain(|r| r.currency == cur);
    rows.truncate(50);
    if f.csv {
        return Ok(csv_response(
            "top-products.csv",
            &[
                ctx.t("admin_settings.reports.product"),
                ctx.t("admin.common.currency"),
                ctx.t("admin_settings.reports.orders"),
                ctx.t("admin_settings.reports.quantity"),
                ctx.t("admin_settings.reports.revenue"),
            ],
            &rows
                .iter()
                .map(|r| {
                    vec![
                        r.product_name.clone(),
                        r.currency.as_str().to_owned(),
                        r.order_count.to_string(),
                        r.quantity.to_string(),
                        dec(&r.revenue),
                    ]
                })
                .collect::<Vec<_>>(),
        ));
    }
    let pts: Vec<Point> = rows
        .iter()
        .take(10)
        .map(|r| Point {
            label: r.product_name.clone(),
            value: r.revenue,
            display: ctx.money_in(&r.revenue, &r.currency),
        })
        .collect();
    let chart_svg = if pts.is_empty() {
        String::new()
    } else {
        chart::hbars(
            &pts,
            &ctx.tv(
                "admin_settings.reports.product_chart",
                "currency",
                f.currency.as_str(),
            ),
        )
    };
    let s = shell(
        &mut ctx,
        &staff,
        f,
        "/admin/reports/products",
        "admin_settings.reports.tab_products",
        others,
    );
    render(&ProductsPage {
        ctx,
        s,
        rows,
        chart_svg,
    })
}

// ---------------------------------------------------------------------------
// domains
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/reports/domains.html")]
struct DomainsPage {
    ctx: PageCtx,
    s: Shell,
    tlds: Vec<TldShareRow>,
    renewals: Vec<RenewalRow>,
    days: i32,
    chart_svg: String,
}
crate::page!(DomainsPage);

async fn domains(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ReportsView>,
    Query(q): Query<ReportQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let f = filters(&state, &tenant, &q).await?;
    let days = q
        .days
        .as_deref()
        .and_then(|d| d.trim().parse::<i32>().ok())
        .filter(|d| (1..=365).contains(d))
        .unwrap_or(60);
    let tlds = srepo::domain_tld_shares(&state.pool, tenant.id).await?;
    let renewals = srepo::upcoming_renewals(&state.pool, tenant.id, days, 200).await?;
    if f.csv {
        return Ok(if f.table == "renewals" {
            csv_response(
                "domain-renewals.csv",
                &[
                    ctx.t("admin_settings.reports.domain"),
                    ctx.t("admin_settings.reports.client"),
                    ctx.t("admin.common.status"),
                    ctx.t("admin_settings.reports.expires"),
                    ctx.t("admin_settings.reports.auto_renew"),
                    ctx.t("admin.common.currency"),
                    ctx.t("admin_settings.reports.renewal_amount"),
                ],
                &renewals
                    .iter()
                    .map(|r| {
                        vec![
                            r.name.clone(),
                            r.client_name.clone(),
                            r.status.clone(),
                            r.expires_at
                                .map(|e| e.date_naive().to_string())
                                .unwrap_or_default(),
                            r.auto_renew.to_string(),
                            r.currency.as_str().to_owned(),
                            dec(&r.recurring_amount),
                        ]
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            csv_response(
                "domain-tlds.csv",
                &[
                    ctx.t("admin_settings.reports.tld"),
                    ctx.t("admin.common.total"),
                    ctx.t("admin.common.active"),
                    ctx.t("admin_settings.reports.auto_renew"),
                ],
                &tlds
                    .iter()
                    .map(|r| {
                        vec![
                            r.tld.clone(),
                            r.total.to_string(),
                            r.active.to_string(),
                            r.auto_renew.to_string(),
                        ]
                    })
                    .collect::<Vec<_>>(),
            )
        });
    }
    let pts: Vec<Point> = tlds
        .iter()
        .take(12)
        .map(|r| Point {
            label: format!(".{}", r.tld),
            value: Decimal::from(r.total),
            display: ctx.num(r.total),
        })
        .collect();
    let chart_svg = if pts.is_empty() {
        String::new()
    } else {
        chart::hbars(&pts, &ctx.t("admin_settings.reports.tld_chart"))
    };
    let s = shell(
        &mut ctx,
        &staff,
        f,
        "/admin/reports/domains",
        "admin_settings.reports.tab_domains",
        Vec::new(),
    );
    render(&DomainsPage {
        ctx,
        s,
        tlds,
        renewals,
        days,
        chart_svg,
    })
}
