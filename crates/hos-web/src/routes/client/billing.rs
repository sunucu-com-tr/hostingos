//! Client billing: invoices (list, detail, printable copy, payment and the
//! gateway hand-off), account credit (ledger, top-up), transactions, saved
//! payment methods, orders and the billing profile.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::{NaiveDate, Utc};
use hos_billing::StartPayment;
use hos_billing::mass_pay::MassPayment;
use hos_billing::payments::StartedPayment;
use hos_core::domain::IntegrationCategory;
use hos_core::domain::InvoiceStatus;
use hos_core::ids::{IntegrationId, InvoiceId};
use hos_core::integrations::gateway::{CheckoutSession, FormMethod};
use hos_core::money::Currency;
use hos_db::repo::billing_support::{self, PaymentMethodRow};
use hos_db::repo::invoices::{self, InvoiceDetail, InvoiceFilter, InvoiceListRow, InvoiceRow};
use hos_db::repo::mass_pay::{self, MassPaymentItemRow, MassPaymentStatus};
use hos_db::repo::orders::{self, OrderFilter, OrderListRow};
use hos_db::repo::payments::{self, CreditRow, TransactionFilter, TransactionRow};
use hos_db::repo::{clients, integrations};
use http::HeaderValue;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Fmt as _, MemberAccess, audit_client, friendly_error, no_store, outcome, parse_id, setup,
    with_csp,
};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, Htmx, PageQuery, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::FormErrors;
use crate::routes::admin::common::FormData;
use crate::routes::cart::checkout::{DetailsForm, country_options};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, Pager, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/invoices", get(list))
        .route("/client/invoices/mass-pay", post(mass_pay))
        .route("/client/invoices/{id}", get(show))
        .route("/client/invoices/{id}/print", get(print))
        .route("/client/invoices/{id}/pay", post(pay))
        .route("/client/invoices/{id}/status", get(status))
        .route("/client/billing", get(overview))
        .route("/client/billing/funds", post(add_funds))
        .route("/client/billing/transactions", get(transactions))
        .route("/client/billing/methods/{id}/delete", post(delete_method))
        .route("/client/billing/methods/add", post(add_method))
        .route("/client/orders", get(order_list))
        .route(
            "/client/account/billing",
            get(profile_form).post(profile_save),
        )
}

/// Long date for a `NaiveDate` (tenant / user time zone independent).
pub fn day(ctx: &PageCtx, d: &NaiveDate) -> String {
    d.and_hms_opt(12, 0, 0)
        .map(|dt| ctx.date(&dt.and_utc()))
        .unwrap_or_default()
}

/// `day` or `—`.
pub fn day_opt(ctx: &PageCtx, d: &Option<NaiveDate>) -> String {
    d.as_ref().map_or_else(|| "—".to_owned(), |d| day(ctx, d))
}

/// An active payment gateway that accepts a currency.
pub struct Gateway {
    /// Integration id.
    pub id: IntegrationId,
    /// Display name.
    pub name: String,
}

/// The tenant's active gateways that support `currency`.
pub async fn usable_gateways(
    state: &AppState,
    tenant: &Tenant,
    currency: Currency,
) -> AppResult<Vec<Gateway>> {
    let rows =
        integrations::active_integrations(&state.pool, tenant.id, IntegrationCategory::Gateway)
            .await?;
    let mut out = Vec::new();
    for r in rows {
        match state.billing.registry().gateway(tenant.id, r.id).await {
            Ok(g) if g.capabilities().supports_currency(currency) => out.push(Gateway {
                id: r.id,
                name: r.name,
            }),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, integration = %r.id, "gateway unavailable"),
        }
    }
    Ok(out)
}

async fn own_invoice(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    id: &str,
) -> AppResult<InvoiceDetail> {
    let id: InvoiceId = parse_id(id)?;
    invoices::invoice_detail(&state.pool, tenant.id, id)
        .await?
        .filter(|d| {
            d.invoice.client_id == client.client_id && d.invoice.status != InvoiceStatus::Draft
        })
        .ok_or(AppError::NotFound)
}

fn payable(i: &InvoiceRow) -> bool {
    matches!(
        i.status,
        InvoiceStatus::Unpaid | InvoiceStatus::PaymentPending | InvoiceStatus::Collections
    ) && i.balance > Decimal::ZERO
}

// ---------------------------------------------------------------------------
// Invoice list
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct StatusFilter {
    #[serde(default)]
    status: String,
}

#[derive(Template)]
#[template(path = "client/invoices.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<InvoiceListRow>,
    pager: Pager,
    status: String,
    statuses: Vec<(String, String)>,
    today: NaiveDate,
    /// Invoices that may join a mass payment (unpaid, no credit lines).
    selectable: Vec<InvoiceId>,
}
crate::page!(ListPage);

impl ListPage {
    fn overdue(&self, i: &InvoiceRow) -> bool {
        i.is_overdue(self.today)
    }
    fn selectable(&self, i: &InvoiceRow) -> bool {
        self.selectable.contains(&i.id)
    }
    /// Offer "pay selected" when at least two invoices can be combined.
    fn mass_pay(&self) -> bool {
        self.selectable.len() >= 2
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    pq: PageQuery,
    Query(f): Query<StatusFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<InvoiceStatus> = f.status.parse().ok();
    let paged = invoices::list_invoices(
        &state.pool,
        tenant.id,
        &InvoiceFilter {
            client_id: Some(client.client_id),
            status,
            overdue_as_of: None,
            search: pq.q.clone().filter(|q| !q.trim().is_empty()),
        },
        pq.db_page(),
    )
    .await?;
    let mut paged = paged;
    paged
        .items
        .retain(|r| r.invoice.status != InvoiceStatus::Draft);
    let status_s = status.map(|s| s.as_str().to_owned()).unwrap_or_default();
    let q = pq.q.clone().unwrap_or_default();
    let pager = Pager::new(
        &paged,
        &ctx.url("/client/invoices"),
        &[("status", status_s.as_str()), ("q", q.as_str())],
    );
    let statuses = std::iter::once((String::new(), ctx.t("client.invoices.all")))
        .chain(
            [
                InvoiceStatus::Unpaid,
                InvoiceStatus::PaymentPending,
                InvoiceStatus::Paid,
                InvoiceStatus::Cancelled,
                InvoiceStatus::Refunded,
            ]
            .iter()
            .map(|s| {
                (
                    s.as_str().to_owned(),
                    ctx.t(&format!("common.status.{}", s.as_str())),
                )
            }),
        )
        .collect();
    let unpaid: Vec<InvoiceId> = paged
        .items
        .iter()
        .filter(|r| r.invoice.status == InvoiceStatus::Unpaid && r.invoice.balance > Decimal::ZERO)
        .map(|r| r.invoice.id)
        .collect();
    let excluded = mass_pay::with_credit_lines(&state.pool, tenant.id, &unpaid).await?;
    let selectable = unpaid
        .into_iter()
        .filter(|i| !excluded.contains(i))
        .collect();
    let title = ctx.t("client.invoices.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    render(&ListPage {
        ctx,
        rows: paged.items,
        pager,
        status: status_s,
        statuses,
        today: Utc::now().date_naive(),
        selectable,
    })
}

/// Pays several invoices with one payment: creates the combined invoice
/// (amounts computed by hos-billing) and sends the client to pay it.
async fn mass_pay(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    form: FormData,
) -> AppResult<Response> {
    let back = ctx.url("/client/invoices");
    let ids: Vec<InvoiceId> = form.ids("invoice");
    if ids.len() < 2 {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.invoices.mass_pick_two")),
        ));
    }
    let res = state
        .billing
        .create_mass_payment(tenant.id, client.client_id, &ids)
        .await;
    match res {
        Ok(inv) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.invoice.mass_pay",
                Some(("invoice", inv.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(
                &ctx.url(format!("/client/invoices/{}#pay", inv.id)),
                SetFlash::success(ctx.t("client.invoices.mass_created")),
            ))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

// ---------------------------------------------------------------------------
// Invoice detail + payment
// ---------------------------------------------------------------------------

/// The invoice page's "pay" box.
pub struct PayBox {
    /// `(value, label)`: gateways and `credit`.
    pub choices: Vec<(String, String)>,
    /// `(gateway id, installment counts)` for gateways offering a choice.
    pub installments: Vec<(String, Vec<u32>)>,
    /// Preselected.
    pub selected: String,
    /// Account credit in the invoice currency.
    pub credit: String,
    /// Credit can cover part of the balance.
    pub has_credit: bool,
}

#[derive(Template)]
#[template(path = "client/invoice.html")]
struct InvoicePage {
    ctx: PageCtx,
    d: InvoiceDetail,
    txns: Vec<TransactionRow>,
    pay: Option<PayBox>,
    /// `return` | `cancel` | "".
    payment: String,
    snapshot: Vec<String>,
    overdue: bool,
    /// Set when this is a mass payment's combined invoice.
    mass: Option<MassPayment>,
}
crate::page!(InvoicePage);

impl InvoicePage {
    fn cur(&self, v: &Decimal) -> String {
        self.ctx.money_in(v, &self.d.invoice.currency)
    }
    fn number(&self) -> String {
        invoice_number(&self.d.invoice)
    }
    fn waiting(&self) -> bool {
        self.payment == "return" && self.d.invoice.status != InvoiceStatus::Paid
    }
    /// Installment counts offered by gateway `id` (empty: none).
    fn installments_of(&self, id: &str) -> Vec<u32> {
        self.pay
            .as_ref()
            .and_then(|p| p.installments.iter().find(|(g, _)| g == id))
            .map(|(_, c)| c.clone())
            .unwrap_or_default()
    }
    fn mass_state(&self, item: &MassPaymentItemRow) -> String {
        match (&self.mass, item.applied_at) {
            (_, Some(_)) => self.ctx.t("client.invoices.mass_applied"),
            (Some(m), None) if m.row.status == MassPaymentStatus::Cancelled => {
                self.ctx.t("client.invoices.mass_cancelled")
            }
            _ => self.ctx.t("client.invoices.mass_waiting"),
        }
    }
}

/// `INV-2026-0001`, or a short id for proforma invoices.
pub fn invoice_number(i: &InvoiceRow) -> String {
    i.number.clone().unwrap_or_else(|| {
        let s = i.id.to_string();
        format!("#{}", &s[s.len().saturating_sub(8)..])
    })
}

/// Billing address lines from an invoice snapshot.
pub fn snapshot_lines(v: &Value) -> Vec<String> {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    let mut out = Vec::new();
    for line in [
        s("company_name"),
        s("name"),
        [s("tax_office"), s("tax_number")]
            .join(" ")
            .trim()
            .to_owned(),
        s("address_line1"),
        s("address_line2"),
        [s("postal_code"), s("city"), s("state")]
            .join(" ")
            .trim()
            .to_owned(),
        s("country_code"),
        s("email"),
    ] {
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

#[derive(Debug, Default, Deserialize)]
struct ReturnQuery {
    #[serde(default)]
    payment: String,
}

async fn pay_box(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    client: &CurrentClient,
    inv: &InvoiceRow,
) -> AppResult<Option<PayBox>> {
    if !payable(inv) {
        return Ok(None);
    }
    let gateways = usable_gateways(state, tenant, inv.currency).await?;
    let mut installments = Vec::new();
    for g in &gateways {
        if let Ok(gw) = state.billing.registry().gateway(tenant.id, g.id).await {
            let counts = gw.installment_choices(&inv.balance_money().round());
            if !counts.is_empty() {
                installments.push((g.id.to_string(), counts));
            }
        }
    }
    let mut choices: Vec<(String, String)> = gateways
        .into_iter()
        .map(|g| (g.id.to_string(), g.name))
        .collect();
    let credit =
        payments::credit_balance(&state.pool, tenant.id, client.client_id, inv.currency).await?;
    let has_credit = credit > Decimal::ZERO;
    if has_credit {
        choices.push((
            "credit".into(),
            if credit >= inv.balance {
                ctx.t("client.invoices.pay_credit_full")
            } else {
                ctx.tv(
                    "client.invoices.pay_credit_part",
                    "amount",
                    ctx.money_in(&credit, &inv.currency),
                )
            },
        ));
    }
    let selected = inv
        .payment_integration_id
        .map(|i| i.to_string())
        .filter(|s| choices.iter().any(|c| c.0 == *s))
        .or_else(|| choices.first().map(|c| c.0.clone()))
        .unwrap_or_default();
    Ok(Some(PayBox {
        choices,
        installments,
        selected,
        credit: ctx.money_in(&credit, &inv.currency),
        has_credit,
    }))
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    Query(rq): Query<ReturnQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let d = own_invoice(&state, &tenant, &client, &id).await?;
    let txns = payments::invoice_transactions(&state.pool, tenant.id, d.invoice.id).await?;
    let pay = pay_box(&state, &tenant, &ctx, &client, &d.invoice).await?;
    let number = invoice_number(&d.invoice);
    let title = ctx.tv("client.invoices.detail_title", "number", &number);
    let crumbs = vec![
        Crumb::link(ctx.t("client.invoices.title"), ctx.url("/client/invoices")),
        Crumb::current(number),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    let payment = match rq.payment.as_str() {
        "return" | "cancel" => rq.payment.clone(),
        _ => String::new(),
    };
    if payment == "cancel" && payable(&d.invoice) {
        ctx.flashes.push(crate::flash::FlashMessage {
            kind: crate::flash::FlashKind::Warning,
            text: ctx.t("client.invoices.payment_cancelled"),
        });
    }
    let overdue = d.invoice.is_overdue(Utc::now().date_naive());
    let snapshot = snapshot_lines(&d.invoice.billing_snapshot);
    let mass = state
        .billing
        .mass_payment_for_invoice(tenant.id, d.invoice.id)
        .await?;
    let res = render(&InvoicePage {
        ctx,
        d,
        txns,
        pay,
        payment,
        snapshot,
        overdue,
        mass,
    })?;
    Ok(no_store(with_csp(
        &state,
        res,
        &super::payment_form_csp(&state),
    )))
}

#[derive(Template)]
#[template(path = "client/invoice_print.html")]
struct PrintPage {
    ctx: PageCtx,
    d: InvoiceDetail,
    snapshot: Vec<String>,
    seller: crate::view::CompanyInfo,
}
crate::page!(PrintPage);

impl PrintPage {
    fn status_label(&self) -> String {
        self.t(&format!("common.status.{}", self.d.invoice.status.as_str()))
    }
    fn cur(&self, v: &Decimal) -> String {
        self.ctx.money_in(v, &self.d.invoice.currency)
    }
    fn number(&self) -> String {
        invoice_number(&self.d.invoice)
    }
}

async fn print(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let d = own_invoice(&state, &tenant, &client, &id).await?;
    ctx.load_store().await?;
    let seller = ctx.store().company.clone();
    let title = ctx.tv(
        "client.invoices.detail_title",
        "number",
        invoice_number(&d.invoice),
    );
    ctx.set_title(title);
    let snapshot = snapshot_lines(&d.invoice.billing_snapshot);
    render(&PrintPage {
        ctx,
        d,
        snapshot,
        seller,
    })
}

async fn pay(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    Path(id): Path<String>,
    ctx: PageCtx,
    form: FormData,
) -> AppResult<Response> {
    let method = form.text("method", 64);
    let save = form.flag("save");
    let d = own_invoice(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/invoices/{}", d.invoice.id));
    if !payable(&d.invoice) {
        return Ok(flash::redirect(
            &back,
            SetFlash::info(ctx.t("client.invoices.nothing_due")),
        ));
    }
    if method == "credit" {
        let res = state
            .billing
            .pay_with_credit(
                tenant.id,
                d.invoice.id,
                Some(client.client_id),
                None,
                Some(client.user_id),
            )
            .await;
        return outcome(&ctx, &back, res, |(inv, applied)| {
            if inv.status == InvoiceStatus::Paid {
                ctx.t("client.invoices.paid_with_credit")
            } else {
                ctx.tv(
                    "client.invoices.credit_applied",
                    "amount",
                    ctx.money_in(&applied, &inv.currency),
                )
            }
        });
    }
    let gateway: IntegrationId = match method.parse() {
        Ok(g) => g,
        Err(_) => {
            return Ok(flash::redirect(
                &back,
                SetFlash::error(ctx.t("client.invoices.choose_method")),
            ));
        }
    };
    if !usable_gateways(&state, &tenant, d.invoice.currency)
        .await?
        .iter()
        .any(|g| g.id == gateway)
    {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.invoices.choose_method")),
        ));
    }
    let started = state
        .billing
        .start_payment(
            tenant.id,
            d.invoice.id,
            &StartPayment {
                integration_id: Some(gateway),
                client_id: Some(client.client_id),
                return_url: None,
                cancel_url: None,
                ip: Some(ip.0),
                save_payment_method: save,
                installments: match form.text(&format!("installments_{gateway}"), 4).as_str() {
                    "" | "1" => None,
                    n => match n.parse::<u32>() {
                        Ok(n) => Some(n),
                        Err(_) => {
                            return Ok(flash::redirect(
                                &back,
                                SetFlash::error(ctx.t("client.invoices.installments_invalid")),
                            ));
                        }
                    },
                },
            },
        )
        .await;
    match started {
        Ok(s) => payment_response(&state, &tenant, ctx, &d.invoice, s).await,
        Err(e) => match friendly_error(&ctx, &e) {
            Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

#[derive(Template)]
#[template(path = "client/pay.html")]
struct PayPage {
    ctx: PageCtx,
    /// `form` | `iframe` | `embed` | `offline`.
    mode: &'static str,
    invoice_id: String,
    number: String,
    amount: String,
    reference: String,
    action: String,
    method: &'static str,
    fields: Vec<(String, String)>,
    url: String,
    html: String,
    nonce: String,
}
crate::page!(PayPage);

fn origin_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "https" | "http"))
        .map(|u| u.origin().ascii_serialization())
        .filter(|o| o != "null")
}

/// Adds `nonce` to every `<script` tag of gateway-provided HTML.
fn nonce_scripts(html: &str, nonce: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let lower = html.to_ascii_lowercase();
    let mut last = 0;
    let mut from = 0;
    while let Some(pos) = lower[from..].find("<script") {
        let at = from + pos + "<script".len();
        out.push_str(&html[last..at]);
        out.push_str(&format!(" nonce=\"{nonce}\""));
        last = at;
        from = at;
    }
    out.push_str(&html[last..]);
    out
}

/// Hands the customer to the gateway: a redirect, an auto-submitted form,
/// an iframe, the gateway's embedded HTML, or offline instructions. Each
/// page gets a CSP widened only as far as that gateway needs.
pub async fn payment_response(
    state: &AppState,
    _tenant: &Tenant,
    mut ctx: PageCtx,
    invoice: &InvoiceRow,
    started: StartedPayment,
) -> AppResult<Response> {
    let number = invoice_number(invoice);
    let amount = ctx.money_in(&started.amount.amount, &started.amount.currency);
    let nonce = hos_core::crypto::generate_token();
    let mode;
    let (mut action, mut method, mut fields, mut url, mut html) = (
        String::new(),
        "post",
        Vec::new(),
        String::new(),
        String::new(),
    );
    let mut csp: Vec<(&str, String)> = Vec::new();
    match started.session {
        CheckoutSession::Redirect { url, .. } => {
            if origin_of(&url).is_none() {
                return Err(AppError::internal(
                    "gateway returned an invalid redirect URL",
                ));
            }
            return Ok(no_store(Redirect::to(&url).into_response()));
        }
        CheckoutSession::Form {
            action: a,
            method: m,
            fields: f,
            ..
        } => {
            let origin = origin_of(&a)
                .ok_or_else(|| AppError::internal("gateway returned an invalid form action"))?;
            mode = "form";
            action = a;
            method = match m {
                FormMethod::Get => "get",
                FormMethod::Post => "post",
            };
            fields = f;
            csp.push(("form-action", format!("'self' {origin} https:")));
            csp.push(("script-src", format!("'self' 'nonce-{nonce}'")));
        }
        CheckoutSession::Iframe { url: u, .. } => {
            let origin = origin_of(&u)
                .ok_or_else(|| AppError::internal("gateway returned an invalid iframe URL"))?;
            mode = "iframe";
            url = u;
            csp.push(("frame-src", origin.clone()));
            csp.push(("form-action", format!("'self' {origin} https:")));
        }
        CheckoutSession::Embed { html: h, .. } => {
            mode = "embed";
            html = nonce_scripts(&h, &nonce);
            csp.push((
                "script-src",
                format!("'self' 'nonce-{nonce}' 'strict-dynamic' https:"),
            ));
            csp.push(("style-src", "'self' 'unsafe-inline' https:".into()));
            csp.push(("frame-src", "https:".into()));
            csp.push(("connect-src", "'self' https:".into()));
            csp.push(("form-action", "'self' https:".into()));
        }
        CheckoutSession::Offline { instructions } => {
            mode = "offline";
            html = crate::markdown::render(&instructions);
        }
    }
    let title = ctx.tv("client.pay.title", "number", &number);
    ctx.set_title(title);
    let res = render(&PayPage {
        ctx,
        mode,
        invoice_id: invoice.id.to_string(),
        number,
        amount,
        reference: started.reference.clone(),
        action,
        method,
        fields,
        url,
        html,
        nonce,
    })?;
    Ok(no_store(with_csp(state, res, &csp)))
}

#[derive(Template)]
#[template(path = "client/_invoice_status.html")]
struct StatusFragment {
    ctx: PageCtx,
    paid: bool,
    pending: bool,
}
crate::page!(StatusFragment);

/// `GET /client/invoices/{id}/status` (htmx polling after the gateway
/// returned): reloads the page once the webhook marked the invoice paid.
async fn status(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let d = own_invoice(&state, &tenant, &client, &id).await?;
    let paid = d.invoice.status == InvoiceStatus::Paid;
    let mut res = render(&StatusFragment {
        paid,
        pending: d.invoice.status == InvoiceStatus::PaymentPending,
        ctx,
    })?;
    if paid {
        res.headers_mut()
            .insert("HX-Refresh", HeaderValue::from_static("true"));
    }
    Ok(no_store(res))
}

// ---------------------------------------------------------------------------
// Credit, transactions, payment methods
// ---------------------------------------------------------------------------

/// A saved payment method row.
pub struct MethodView {
    /// Id.
    pub id: String,
    /// Label.
    pub label: String,
    /// Gateway name.
    pub gateway: String,
    /// Default.
    pub is_default: bool,
    /// The last automatic charge with it failed.
    pub failed: bool,
    /// Last use.
    pub last_used: String,
}

#[derive(Template)]
#[template(path = "client/billing.html")]
struct OverviewPage {
    ctx: PageCtx,
    balance: String,
    currency: String,
    credits: Vec<CreditRow>,
    txns: Vec<TransactionRow>,
    methods: Vec<MethodView>,
    can_top_up: bool,
    /// "Min … – max …" of a top-up (hos_billing::funds).
    funds_hint: String,
    /// `(gateway id, name)` that can save a card without a payment.
    setup_gateways: Vec<(String, String)>,
}
crate::page!(OverviewPage);

async fn overview(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Query(rq): Query<CardQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let c = clients::get_client(&state.pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let balance = payments::credit_balance(&state.pool, tenant.id, c.id, c.currency).await?;
    let mut credits = payments::list_credits(&state.pool, tenant.id, c.id).await?;
    credits.truncate(50);
    let txns = payments::list_transactions(
        &state.pool,
        tenant.id,
        &TransactionFilter {
            client_id: Some(c.id),
            ..TransactionFilter::default()
        },
        hos_db::Page::first(5),
    )
    .await?
    .items;
    let methods = method_views(&state, &tenant, &ctx, c.id).await?;
    let gateways = usable_gateways(&state, &tenant, c.currency).await?;
    let can_top_up = !gateways.is_empty();
    let mut setup_gateways = Vec::new();
    for g in gateways {
        if let Ok(gw) = state.billing.registry().gateway(tenant.id, g.id).await
            && gw.supports_card_setup()
            && gw.capabilities().stored_payment_methods
        {
            setup_gateways.push((g.id.to_string(), g.name));
        }
    }
    let title = ctx.t("client.billing.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    let limits = hos_billing::funds::FundsLimits::load(&state.pool, tenant.id).await?;
    let funds_hint = ctx.tv2(
        "client.billing.funds_limits",
        "min",
        ctx.money_in(&limits.min, &c.currency),
        "max",
        ctx.money_in(&limits.max, &c.currency),
    );
    match rq.card.as_str() {
        "saved" => ctx.flashes.push(crate::flash::FlashMessage {
            kind: crate::flash::FlashKind::Info,
            text: ctx.t("client.billing.card_pending"),
        }),
        "cancelled" => ctx.flashes.push(crate::flash::FlashMessage {
            kind: crate::flash::FlashKind::Warning,
            text: ctx.t("client.billing.card_cancelled"),
        }),
        _ => {}
    }
    let res = render(&OverviewPage {
        balance: ctx.money_in(&balance, &c.currency),
        currency: c.currency.as_str().to_owned(),
        funds_hint,
        ctx,
        credits,
        txns,
        methods,
        can_top_up,
        setup_gateways,
    })?;
    Ok(no_store(res))
}

#[derive(Debug, Default, Deserialize)]
struct CardQuery {
    #[serde(default)]
    card: String,
}

#[derive(Debug, Deserialize)]
struct AddMethodForm {
    #[serde(default)]
    gateway: String,
}

#[derive(Template)]
#[template(path = "client/card_setup.html")]
struct CardSetupPage {
    ctx: PageCtx,
    action: String,
    method: &'static str,
    fields: Vec<(String, String)>,
    nonce: String,
}
crate::page!(CardSetupPage);

/// "Add a card" without paying: hands the client to the gateway's card
/// form; the card is stored only from the gateway's signed completion.
async fn add_method(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    mut ctx: PageCtx,
    Form(f): Form<AddMethodForm>,
) -> AppResult<Response> {
    let back = ctx.url("/client/billing");
    let c = clients::get_client(&state.pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let Ok(gateway) = f.gateway.parse::<IntegrationId>() else {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.invoices.choose_method")),
        ));
    };
    if !usable_gateways(&state, &tenant, c.currency)
        .await?
        .iter()
        .any(|g| g.id == gateway)
    {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.invoices.choose_method")),
        ));
    }
    let started = match state
        .billing
        .start_card_setup(
            tenant.id,
            client.client_id,
            gateway,
            Some(client.user_id),
            Some(ip.0),
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            return match friendly_error(&ctx, &e) {
                Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
                None => Err(e.into()),
            };
        }
    };
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.payment_method.setup",
        None,
    )
    .await;
    match started.session {
        CheckoutSession::Redirect { url, .. } if origin_of(&url).is_some() => {
            Ok(no_store(Redirect::to(&url).into_response()))
        }
        CheckoutSession::Form {
            action,
            method,
            fields,
            ..
        } => {
            let origin = origin_of(&action)
                .ok_or_else(|| AppError::internal("gateway returned an invalid form action"))?;
            let nonce = hos_core::crypto::generate_token();
            let csp = vec![
                ("form-action", format!("'self' {origin} https:")),
                ("script-src", format!("'self' 'nonce-{nonce}'")),
            ];
            let title = ctx.t("client.billing.card_title");
            ctx.set_title(title);
            let res = render(&CardSetupPage {
                ctx,
                action,
                method: match method {
                    FormMethod::Get => "get",
                    FormMethod::Post => "post",
                },
                fields,
                nonce,
            })?;
            Ok(no_store(with_csp(&state, res, &csp)))
        }
        _ => Err(AppError::internal(
            "gateway returned an unsupported card setup hand-off",
        )),
    }
}

async fn method_views(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    client_id: hos_core::ids::ClientId,
) -> AppResult<Vec<MethodView>> {
    let rows: Vec<PaymentMethodRow> =
        billing_support::list_payment_methods(&state.pool, tenant.id, client_id).await?;
    let gateways =
        integrations::active_integrations(&state.pool, tenant.id, IntegrationCategory::Gateway)
            .await?;
    Ok(rows
        .into_iter()
        .map(|m| MethodView {
            id: m.id.to_string(),
            label: m
                .label
                .clone()
                .unwrap_or_else(|| ctx.t("client.billing.method_unnamed")),
            gateway: gateways
                .iter()
                .find(|g| g.id == m.integration_id)
                .map(|g| g.name.clone())
                .unwrap_or_default(),
            is_default: m.is_default,
            failed: m.last_error.is_some(),
            last_used: ctx.date_opt(&m.last_used_at),
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct FundsForm {
    amount: String,
}

async fn add_funds(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<FundsForm>,
) -> AppResult<Response> {
    let back = ctx.url("/client/billing");
    let raw = f.amount.trim().replace(' ', "");
    // Accept "1.234,56" (tr) and "1,234.56" (en) as well as plain numbers.
    let normalized = match (raw.rfind(','), raw.rfind('.')) {
        (Some(c), Some(d)) if c > d => raw.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => raw.replace(',', ""),
        (Some(_), None) => raw.replace(',', "."),
        _ => raw,
    };
    let Ok(amount) = normalized.parse::<Decimal>() else {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.billing.amount_invalid")),
        ));
    };
    let res = state
        .billing
        .order_add_funds(tenant.id, client.client_id, amount)
        .await;
    match res {
        Ok(inv) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.credit.top_up",
                Some(("invoice", inv.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(
                &ctx.url(format!("/client/invoices/{}", inv.id)),
                SetFlash::success(ctx.t("client.billing.top_up_created")),
            ))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

#[derive(Template)]
#[template(path = "client/transactions.html")]
struct TxnPage {
    ctx: PageCtx,
    rows: Vec<TransactionRow>,
    pager: Pager,
}
crate::page!(TxnPage);

async fn transactions(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    pq: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let paged = payments::list_transactions(
        &state.pool,
        tenant.id,
        &TransactionFilter {
            client_id: Some(client.client_id),
            ..TransactionFilter::default()
        },
        pq.db_page(),
    )
    .await?;
    let pager = Pager::new(&paged, &ctx.url("/client/billing/transactions"), &[]);
    let title = ctx.t("client.billing.transactions");
    let crumbs = vec![
        Crumb::link(ctx.t("client.billing.title"), ctx.url("/client/billing")),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    render(&TxnPage {
        ctx,
        rows: paged.items,
        pager,
    })
}

async fn delete_method(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id: uuid::Uuid = parse_id(&id)?;
    if !billing_support::delete_payment_method(&state.pool, tenant.id, client.client_id, id).await?
    {
        return Err(AppError::NotFound);
    }
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.payment_method.deleted",
        None,
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/billing"),
        SetFlash::success(ctx.t("client.billing.method_deleted")),
    ))
}

// ---------------------------------------------------------------------------
// Orders
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/orders.html")]
struct OrdersPage {
    ctx: PageCtx,
    rows: Vec<OrderListRow>,
    pager: Pager,
}
crate::page!(OrdersPage);

async fn order_list(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    pq: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let paged = orders::list_orders(
        &state.pool,
        tenant.id,
        &OrderFilter {
            client_id: Some(client.client_id),
            ..OrderFilter::default()
        },
        pq.db_page(),
    )
    .await?;
    let pager = Pager::new(&paged, &ctx.url("/client/orders"), &[]);
    let title = ctx.t("client.orders.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    render(&OrdersPage {
        ctx,
        rows: paged.items,
        pager,
    })
}

// ---------------------------------------------------------------------------
// Billing profile
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/billing_profile.html")]
struct ProfilePage {
    ctx: PageCtx,
    form: DetailsForm,
    errors: FormErrors,
    countries: Vec<(String, String)>,
    email: String,
}
crate::page!(ProfilePage);

async fn profile_form(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let c = clients::get_client(&state.pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let form = DetailsForm::from_client(&c);
    let countries = country_options(&ctx, &form.country_code);
    profile_crumbs(&mut ctx, &client, &access);
    render(&ProfilePage {
        ctx,
        form,
        errors: FormErrors::default(),
        countries,
        email: c.email,
    })
}

fn profile_crumbs(ctx: &mut PageCtx, client: &CurrentClient, access: &MemberAccess) {
    let title = ctx.t("client.billing.profile_title");
    let crumbs = vec![
        Crumb::link(ctx.t("client.account.title"), ctx.url("/client/account")),
        Crumb::current(title.clone()),
    ];
    setup(ctx, client, access, title, crumbs);
}

#[allow(clippy::too_many_arguments)]
async fn profile_save(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    _htmx: Htmx,
    mut ctx: PageCtx,
    Form(form): Form<DetailsForm>,
) -> AppResult<Response> {
    let c = clients::get_client(&state.pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &c, false, &mut errors);
    crate::einvoice::check_tax_ids(&state, tenant.id, &ctx, &input, &mut errors).await?;
    if errors.any() {
        let countries = country_options(&ctx, &form.country_code);
        profile_crumbs(&mut ctx, &client, &access);
        return render_status(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            &ProfilePage {
                ctx,
                form,
                errors,
                countries,
                email: c.email,
            },
        );
    }
    clients::update_client(&state.pool, tenant.id, client.client_id, &input).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.billing_profile.updated",
        Some(("client", client.client_id.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/billing"),
        SetFlash::success(ctx.t("client.billing.profile_saved")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_added_to_scripts() {
        let html = r#"<div></div><script src="https://x/a.js"></script><SCRIPT>go()</SCRIPT>"#;
        let out = nonce_scripts(html, "abc");
        assert_eq!(out.matches("nonce=\"abc\"").count(), 2);
        assert!(out.starts_with("<div></div><script nonce=\"abc\" src="));
    }

    #[test]
    fn origins() {
        assert_eq!(
            origin_of("https://pay.example.com/a?b=1").as_deref(),
            Some("https://pay.example.com")
        );
        assert_eq!(origin_of("javascript:alert(1)"), None);
    }
}
