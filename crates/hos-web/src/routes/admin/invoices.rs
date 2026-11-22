//! `/admin/invoices` — list + filters, draft creation / line editing,
//! publish, cancel, manual payments (bank transfer approval), refunds,
//! reminders, printable view; `/admin/transactions` — the transaction list.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Days;
use hos_billing::payments::{ManualPayment, RefundInput};
use hos_billing::pricing::{TaxContext, tax_rates};
use hos_core::domain::{IntegrationCategory, InvoiceStatus};
use hos_core::ids::{ClientId, InvoiceId, InvoiceItemId, TransactionId};
use hos_db::Page;
use hos_db::enums::{InvoiceItemType, TransactionKind};
use hos_db::repo::clients::{self, ClientRow};
use hos_db::repo::integrations::{self, IntegrationRow};
use hos_db::repo::invoices::{
    self, InvoiceDetail, InvoiceFilter, InvoiceListRow, NewInvoice, NewInvoiceItem, TaxLineInput,
};
use hos_db::repo::payments::{self, RefundRow, TransactionFilter, TransactionRow};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed, today};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, PageQuery, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/invoices", get(list).post(create))
        .route("/admin/invoices/new", get(new_form))
        .route("/admin/invoices/bulk", post(bulk))
        .route("/admin/invoices/{id}", get(show))
        .route("/admin/invoices/{id}/print", get(print))
        .route("/admin/invoices/{id}/items", post(add_item))
        .route(
            "/admin/invoices/{id}/items/{item}/delete",
            post(remove_item),
        )
        .route("/admin/invoices/{id}/meta", post(update_meta))
        .route("/admin/invoices/{id}/publish", post(publish))
        .route("/admin/invoices/{id}/cancel", post(cancel))
        .route("/admin/invoices/{id}/payment", post(add_payment))
        .route("/admin/invoices/{id}/refund", post(refund))
        .route("/admin/invoices/{id}/remind", post(remind))
        .route("/admin/transactions", get(transactions))
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    status: Option<String>,
    client: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/invoices/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<InvoiceListRow>,
    pager: Pager,
    q: String,
    status: String,
    statuses: Vec<(String, String)>,
    today: chrono::NaiveDate,
    /// This list with its filters (where a bulk action returns to).
    back: String,
}
crate::page!(ListPage);

impl ListPage {
    fn overdue(&self, inv: &invoices::InvoiceRow) -> bool {
        inv.is_overdue(self.today)
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let today = today(&ctx.timezone);
    let sv = f.status.clone().unwrap_or_default();
    let (status, overdue) = match sv.as_str() {
        "overdue" => (None, Some(today)),
        s => (s.parse::<InvoiceStatus>().ok(), None),
    };
    let client_id = f
        .client
        .as_deref()
        .and_then(|c| Uuid::parse_str(c).ok())
        .map(ClientId::from);
    let paged = invoices::list_invoices(
        &state.pool,
        tenant.id,
        &InvoiceFilter {
            client_id,
            status,
            overdue_as_of: overdue,
            search: q.q.clone(),
        },
        q.db_page(),
    )
    .await?;
    let title = ctx.t("admin.invoices.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let qv = q.q.clone().unwrap_or_default();
    let cv = f.client.clone().unwrap_or_default();
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/invoices"),
        &[
            ("q", qv.as_str()),
            ("status", sv.as_str()),
            ("client", cv.as_str()),
        ],
    );
    let mut statuses =
        common::enum_options(&ctx, InvoiceStatus::ALL, |s| s.as_str(), "admin.status");
    statuses.insert(0, ("overdue".into(), ctx.t("admin.status.overdue")));
    let back = format!(
        "/admin/invoices?{}",
        common::query_string(&[
            ("q", qv.clone()),
            ("status", sv.clone()),
            ("client", cv.clone()),
            ("page", q.page.to_string()),
        ])
    );
    render(&ListPage {
        rows: paged.items,
        pager,
        q: qv,
        status: sv,
        statuses,
        today,
        back,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// Create draft
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct NewQuery {
    client: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/invoices/new.html")]
struct NewPage {
    ctx: PageCtx,
    client: Option<ClientRow>,
    issue: String,
    due: String,
}
crate::page!(NewPage);

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    Query(nq): Query<NewQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let client = match nq.client.as_deref().and_then(|c| Uuid::parse_str(c).ok()) {
        Some(id) => clients::get_client(&state.pool, tenant.id, id.into()).await?,
        None => None,
    };
    let title = ctx.t("admin.invoices.new_title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.invoices.title", "/admin/invoices")],
    );
    let issue = today(&ctx.timezone);
    let settings = hos_billing::BillingSettings::load(&state.pool, tenant.id).await?;
    let due = issue + Days::new(u64::try_from(settings.payment_terms_days).unwrap_or(7));
    render(&NewPage {
        ctx,
        client,
        issue: issue.to_string(),
        due: due.to_string(),
    })
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/invoices/new";
    let client = match f.id::<ClientId>("client_id") {
        Some(id) => clients::get_client(&state.pool, tenant.id, id).await?,
        None => {
            // Allow an e-mail address instead of an id.
            let email = f.text("client_email", 254).to_lowercase();
            let found = hos_db::repo::admin::list_clients(
                &state.pool,
                tenant.id,
                &clients::ClientFilter {
                    search: Some(email.clone()),
                    ..clients::ClientFilter::default()
                },
                None,
                false,
                Page::first(2),
            )
            .await?
            .items;
            found
                .into_iter()
                .map(|r| r.client)
                .find(|c| !email.is_empty() && c.email.eq_ignore_ascii_case(&email))
        }
    };
    let Some(client) = client else {
        return Ok(failed(&ctx, back, ctx.t("admin.invoices.client_not_found")));
    };
    let (Ok(Some(issue)), Ok(Some(due))) = (f.date("issue_date"), f.date("due_date")) else {
        return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_date")));
    };
    let settings = hos_billing::BillingSettings::load(&state.pool, tenant.id).await?;
    let mut conn = state.pool.acquire().await?;
    let taxes: Vec<TaxLineInput> = tax_rates(
        &mut conn,
        tenant.id,
        &TaxContext::for_client(&client, &settings),
    )
    .await?
    .into_iter()
    .map(|t| TaxLineInput {
        tax_rule_id: t.tax_rule_id,
        name: t.name,
        rate: t.rate,
        is_compound: t.is_compound,
    })
    .collect();
    let result = invoices::create_invoice(
        &mut conn,
        tenant.id,
        &NewInvoice {
            client_id: client.id,
            currency: client.currency,
            issue_date: issue,
            due_date: due,
            issue: false,
            payment_integration_id: None,
            notes: f.opt("notes", 2000),
            billing_snapshot: hos_billing::checkout::billing_snapshot(&client),
            items: Vec::new(),
            taxes,
        },
    )
    .await;
    drop(conn);
    match result {
        Ok(inv) => {
            au.log(
                &state,
                "invoice.create",
                Some(("invoice", inv.id.into())),
                json!({"client": client.id}),
            )
            .await?;
            Ok(done(
                &ctx,
                &format!("/admin/invoices/{}", inv.id),
                "admin.invoices.draft_created",
            ))
        }
        Err(e) => Ok(failed(&ctx, back, common::failure_text(&ctx, &e)?)),
    }
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

/// A transaction with its refunds.
pub struct TxView {
    pub tx: TransactionRow,
    pub refunds: Vec<RefundRow>,
    pub refundable: Decimal,
}

#[derive(Template)]
#[template(path = "admin/invoices/show.html")]
struct ShowPage {
    ctx: PageCtx,
    detail: InvoiceDetail,
    client: ClientRow,
    txs: Vec<TxView>,
    gateways: Vec<IntegrationRow>,
    overdue: bool,
    editable: bool,
    credit: Decimal,
    /// e-Fatura / e-Arşiv card (`super::einvoice`).
    einvoice: Option<super::einvoice::Panel>,
}
crate::page!(ShowPage);

async fn load(state: &AppState, tenant: &Tenant, id: InvoiceId) -> AppResult<InvoiceDetail> {
    invoices::invoice_detail(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let detail = load(&state, &tenant, id.into()).await?;
    let inv = &detail.invoice;
    let client = clients::get_client(&state.pool, tenant.id, inv.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut txs = Vec::new();
    for tx in payments::invoice_transactions(&state.pool, tenant.id, inv.id).await? {
        let refunds = payments::transaction_refunds(&state.pool, tenant.id, tx.id).await?;
        let refunded: Decimal = refunds
            .iter()
            .filter(|r| r.status.as_str() != "failed")
            .map(|r| r.amount)
            .sum();
        let refundable = if tx.kind == TransactionKind::Payment && tx.status.as_str() == "succeeded"
        {
            (tx.amount - refunded).max(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };
        txs.push(TxView {
            tx,
            refunds,
            refundable,
        });
    }
    let gateways =
        integrations::list_integrations(&state.pool, tenant.id, Some(IntegrationCategory::Gateway))
            .await?;
    let overdue = inv.is_overdue(today(&ctx.timezone));
    let editable = inv.status == InvoiceStatus::Draft || inv.status == InvoiceStatus::Unpaid;
    let credit = payments::credit_balance(&state.pool, tenant.id, client.id, inv.currency).await?;
    let einvoice = super::einvoice::panel(&state, &tenant, &ctx, inv).await?;
    let title = match &inv.number {
        Some(n) => ctx.tv("admin.invoices.show_title", "number", n),
        None => ctx.t("admin.invoices.draft_title"),
    };
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.invoices.title", "/admin/invoices")],
    );
    render(&ShowPage {
        ctx,
        client,
        txs,
        gateways,
        overdue,
        editable,
        credit,
        einvoice,
        detail,
    })
}

#[derive(Template)]
#[template(path = "admin/invoices/print.html")]
struct PrintPage {
    ctx: PageCtx,
    detail: InvoiceDetail,
    client: ClientRow,
    company: crate::view::CompanyInfo,
}
crate::page!(PrintPage);

async fn print(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let detail = load(&state, &tenant, id.into()).await?;
    let client = clients::get_client(&state.pool, tenant.id, detail.invoice.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    ctx.load_store().await?;
    let company = ctx.store().company.clone();
    let title = detail
        .invoice
        .number
        .clone()
        .unwrap_or_else(|| ctx.t("admin.invoices.draft_title"));
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.invoices.title", "/admin/invoices")],
    );
    render(&PrintPage {
        ctx,
        detail,
        client,
        company,
    })
}

// ---------------------------------------------------------------------------
// Line editing / meta
// ---------------------------------------------------------------------------

async fn add_item(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    let description = f.text("description", 500);
    let (Ok(Some(price)), Ok(qty)) = (f.decimal("unit_price"), f.decimal("quantity")) else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
    };
    if description.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.required")));
    }
    let quantity = qty.unwrap_or(Decimal::ONE);
    if quantity <= Decimal::ZERO {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
    }
    let item_type = if price < Decimal::ZERO {
        InvoiceItemType::Discount
    } else {
        InvoiceItemType::Manual
    };
    let mut item = NewInvoiceItem::manual(&description, price);
    item.item_type = item_type;
    item.quantity = quantity;
    item.taxable = f.flag("taxable");
    let mut conn = state.pool.acquire().await?;
    let result = invoices::add_invoice_items(&mut conn, tenant.id, inv.id, &[item]).await;
    drop(conn);
    if result.is_ok() {
        au.log(
            &state,
            "invoice.item_add",
            Some(("invoice", inv.id.into())),
            json!({"description": description, "unit_price": price.to_string(), "quantity": quantity.to_string()}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.invoices.item_added")
}

async fn remove_item(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path((id, item)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    let mut conn = state.pool.acquire().await?;
    let result =
        invoices::remove_invoice_item(&mut conn, tenant.id, inv.id, InvoiceItemId::from(item))
            .await;
    drop(conn);
    if result.is_ok() {
        au.log(
            &state,
            "invoice.item_remove",
            Some(("invoice", inv.id.into())),
            json!({"item": item}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.common.deleted")
}

async fn update_meta(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    let gateway = f.id("payment_integration_id");
    if let Some(g) = gateway
        && integrations::get_integration(&state.pool, tenant.id, g)
            .await?
            .is_none()
    {
        return Err(AppError::NotFound);
    }
    let notes = f.opt("notes", 4000);
    let admin_notes = f.opt("admin_notes", 4000);
    invoices::update_invoice_meta(
        &state.pool,
        tenant.id,
        inv.id,
        notes.as_deref(),
        admin_notes.as_deref(),
        gateway,
    )
    .await?;
    au.log(
        &state,
        "invoice.update",
        Some(("invoice", inv.id.into())),
        json!({"notes": notes.is_some(), "admin_notes": admin_notes.is_some()}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.common.saved"))
}

// ---------------------------------------------------------------------------
// Status changes / money
// ---------------------------------------------------------------------------

async fn publish(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    let result = publish_one(&state, &tenant, &au, inv.id).await?;
    common::outcome(&ctx, &back, result, "admin.invoices.published")
}

/// Issues draft `id` and queues the e-mail the automation sends for new
/// invoices. The inner `Err` is the business failure (not a draft …).
async fn publish_one(
    state: &AppState,
    tenant: &Tenant,
    au: &Audit,
    id: InvoiceId,
) -> AppResult<Result<invoices::InvoiceRow, hos_core::CoreError>> {
    let mut conn = state.pool.acquire().await?;
    let result = invoices::issue_invoice(&mut conn, tenant.id, id).await;
    if let Ok(issued) = &result {
        let client = clients::get_client(&mut *conn, tenant.id, issued.client_id).await?;
        if let Some(client) = client {
            let ctx_json = invoice_mail_context(state, tenant, issued, &client).await?;
            state
                .billing
                .queue_client_email(
                    &mut conn,
                    tenant.id,
                    client.id,
                    hos_mail::defaults::keys::INVOICE_CREATED,
                    json!({ "invoice": ctx_json }),
                    Some(("invoice", issued.id.into())),
                )
                .await?;
        }
        au.log(
            state,
            "invoice.publish",
            Some(("invoice", id.into())),
            json!({"number": issued.number}),
        )
        .await?;
    }
    Ok(result)
}

async fn invoice_mail_context(
    state: &AppState,
    tenant: &Tenant,
    inv: &invoices::InvoiceRow,
    client: &ClientRow,
) -> AppResult<serde_json::Value> {
    let url = state.billing.tenant_url(tenant.id).await?;
    let loc = client.locale;
    Ok(json!({
        "number": inv.number.clone().unwrap_or_default(),
        "total": hos_billing::format::money(&inv.balance_money(), loc),
        "due_date": hos_billing::format::date(inv.due_date, loc),
        "url": format!("{url}/client/invoices/{}", inv.id),
        "notes": inv.notes.clone().unwrap_or_default(),
    }))
}

/// Cancels one invoice without stranding money: applied credit goes back
/// to the client; money paid on it needs `paid_to_credit` (an explicit staff
/// choice) or a refund first; a payment in flight blocks it.
async fn cancel_one(
    state: &AppState,
    tenant: &Tenant,
    au: &Audit,
    inv: &invoices::InvoiceRow,
    paid_to_credit: bool,
    by: hos_core::ids::UserId,
    bulk: bool,
) -> AppResult<payments::CancelOutcome> {
    let mut conn = state.pool.acquire().await?;
    let out = payments::cancel_invoice_with_credit(
        &mut conn,
        tenant.id,
        inv.id,
        paid_to_credit,
        Some(by),
    )
    .await?;
    if let payments::CancelOutcome::Cancelled {
        credit_returned,
        paid_to_credit,
        ..
    } = &out
    {
        au.log(
            state,
            "invoice.cancel",
            Some(("invoice", inv.id.into())),
            json!({
                "from": inv.status.as_str(),
                "bulk": bulk,
                "credit_returned": credit_returned.to_string(),
                "paid_to_credit": paid_to_credit.to_string(),
            }),
        )
        .await?;
    }
    Ok(out)
}

async fn cancel(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    let out = cancel_one(
        &state,
        &tenant,
        &au,
        &inv,
        f.flag("paid_to_credit"),
        staff.user_id,
        false,
    )
    .await?;
    let text = match out {
        payments::CancelOutcome::Cancelled { .. } => {
            return Ok(done(&ctx, &back, "admin.invoices.cancelled"));
        }
        payments::CancelOutcome::Blocked(payments::CancelBlock::NotOpen) => {
            ctx.t("admin.invoices.cannot_cancel")
        }
        payments::CancelOutcome::Blocked(payments::CancelBlock::PaymentInFlight) => {
            ctx.t("admin.invoices.cancel_in_flight")
        }
        payments::CancelOutcome::Blocked(payments::CancelBlock::AmountPaid(paid)) => ctx.tv(
            "admin.invoices.cancel_paid",
            "amount",
            ctx.money_in(&paid, &inv.currency),
        ),
    };
    Ok(failed(&ctx, &back, text))
}

async fn add_payment(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    let amount = match f.decimal("amount") {
        Ok(Some(a)) if a > Decimal::ZERO => a,
        _ => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount"))),
    };
    let Ok(date) = f.date("date") else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_date")));
    };
    let integration_id = f.id("integration_id");
    if let Some(g) = integration_id
        && integrations::get_integration(&state.pool, tenant.id, g)
            .await?
            .is_none()
    {
        return Err(AppError::NotFound);
    }
    if f.flag("use_credit") {
        let result = state
            .billing
            .pay_with_credit(tenant.id, inv.id, None, Some(amount), Some(staff.user_id))
            .await;
        if result.is_ok() {
            au.log(
                &state,
                "invoice.credit_applied",
                Some(("invoice", inv.id.into())),
                json!({"amount": amount.to_string()}),
            )
            .await?;
        }
        return common::outcome(&ctx, &back, result, "admin.invoices.payment_recorded");
    }
    let payment = ManualPayment {
        invoice_id: inv.id,
        amount,
        integration_id,
        method: f.opt("method", 40),
        reference: f.opt("reference", 200),
        occurred_at: date
            .and_then(|d| d.and_hms_opt(12, 0, 0))
            .map(|d| d.and_utc()),
        note: f.opt("note", 1000),
        staff_user_id: Some(staff.user_id),
    };
    let result = state
        .billing
        .record_manual_payment(tenant.id, &payment)
        .await;
    if let Ok(outcome) = &result {
        au.log(
            &state,
            "invoice.payment",
            Some(("invoice", inv.id.into())),
            json!({
                "amount": amount.to_string(),
                "method": payment.method,
                "reference": payment.reference,
                "transaction": outcome.transaction.id,
                "paid": outcome.invoice_paid,
            }),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.invoices.payment_recorded")
}

#[allow(clippy::too_many_arguments)]
async fn refund(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    ip: ClientIp,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    let Some(tx_id) = f.id::<TransactionId>("transaction_id") else {
        return Err(AppError::bad_request("transaction"));
    };
    let belongs = payments::invoice_transactions(&state.pool, tenant.id, inv.id)
        .await?
        .iter()
        .any(|t| t.id == tx_id);
    if !belongs {
        return Err(AppError::NotFound);
    }
    let amount = match f.decimal("amount") {
        Ok(Some(a)) if a > Decimal::ZERO => a,
        _ => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount"))),
    };
    let input = RefundInput {
        transaction_id: tx_id,
        amount,
        reason: f.opt("reason", 500),
        to_credit: f.flag("to_credit"),
        staff_user_id: Some(staff.user_id),
        ip: Some(ip.0),
    };
    let result = state.billing.refund_payment(tenant.id, &input).await;
    if let Ok(r) = &result {
        au.log(
            &state,
            "invoice.refund",
            Some(("invoice", inv.id.into())),
            json!({"amount": amount.to_string(), "refund": r.id, "status": r.status.as_str(), "to_credit": input.to_credit}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.invoices.refunded")
}

async fn remind(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let inv = load(&state, &tenant, id.into()).await?.invoice;
    let back = format!("/admin/invoices/{}", inv.id);
    match remind_one(&state, &tenant, &au, &ctx, &inv).await? {
        Reminder::NotUnpaid => Ok(failed(
            &ctx,
            &back,
            ctx.t("admin.invoices.remind_not_unpaid"),
        )),
        Reminder::NoRecipient => Ok(failed(
            &ctx,
            &back,
            ctx.t("admin.invoices.remind_no_recipient"),
        )),
        Reminder::Queued => Ok(done(&ctx, &back, "admin.invoices.reminded")),
    }
}

/// What became of a reminder.
enum Reminder {
    /// Only unpaid invoices are reminded.
    NotUnpaid,
    /// Recorded, but the client has no address to send to.
    NoRecipient,
    Queued,
}

/// Queues the payment reminder for `inv` (the overdue notice once it is
/// past due) and records it.
async fn remind_one(
    state: &AppState,
    tenant: &Tenant,
    au: &Audit,
    ctx: &PageCtx,
    inv: &invoices::InvoiceRow,
) -> AppResult<Reminder> {
    if !matches!(
        inv.status,
        InvoiceStatus::Unpaid | InvoiceStatus::PaymentPending | InvoiceStatus::Collections
    ) {
        return Ok(Reminder::NotUnpaid);
    }
    let client = clients::get_client(&state.pool, tenant.id, inv.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut data = invoice_mail_context(state, tenant, inv, &client).await?;
    let today = today(&ctx.timezone);
    let key = if inv.is_overdue(today) {
        let days = (today - inv.due_date).num_days();
        let settings = hos_billing::BillingSettings::load(&state.pool, tenant.id).await?;
        data["days_overdue"] = json!(days);
        data["suspension_date"] = json!(hos_billing::format::date(
            inv.due_date + Days::new(u64::try_from(settings.suspend_after_days).unwrap_or(0) + 1),
            client.locale
        ));
        hos_mail::defaults::keys::INVOICE_OVERDUE
    } else {
        hos_mail::defaults::keys::INVOICE_CREATED
    };
    let mut conn = state.pool.acquire().await?;
    let queued = state
        .billing
        .queue_client_email(
            &mut conn,
            tenant.id,
            client.id,
            key,
            json!({ "invoice": data }),
            Some(("invoice", inv.id.into())),
        )
        .await?;
    invoices::record_reminder(&mut *conn, tenant.id, inv.id).await?;
    drop(conn);
    au.log(
        state,
        "invoice.remind",
        Some(("invoice", inv.id.into())),
        json!({"template": key, "queued": queued.is_some()}),
    )
    .await?;
    Ok(if queued.is_some() {
        Reminder::Queued
    } else {
        Reminder::NoRecipient
    })
}

/// `POST /admin/invoices/bulk` — publish drafts, send reminders for or
/// cancel the selected invoices. Ids of other tenants and invoices the
/// action does not apply to are skipped and counted.
async fn bulk(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = Some(f.text("back", 600))
        .filter(|b| b.starts_with("/admin/invoices"))
        .unwrap_or_else(|| "/admin/invoices".to_owned());
    let ids: Vec<InvoiceId> = f.ids("ids");
    if ids.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.nothing_selected")));
    }
    let action = f.text("action", 20);
    if !matches!(action.as_str(), "publish" | "remind" | "cancel") {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_value")));
    }
    let (mut done_n, mut skipped) = (0usize, 0usize);
    // Bulk cancel never moves payments to credit: invoices with money paid
    // on them or a payment in flight are skipped and counted.
    let (mut held_paid, mut held_in_flight) = (0usize, 0usize);
    for id in &ids {
        let Some(inv) = invoices::get_invoice(&state.pool, tenant.id, *id).await? else {
            skipped += 1;
            continue;
        };
        let ok = match action.as_str() {
            "publish" => match publish_one(&state, &tenant, &au, inv.id).await? {
                Ok(_) => true,
                Err(e) if e.http_status() < 500 => false,
                Err(e) => return Err(e.into()),
            },
            "remind" => matches!(
                remind_one(&state, &tenant, &au, &ctx, &inv).await?,
                Reminder::Queued
            ),
            _ => match cancel_one(&state, &tenant, &au, &inv, false, staff.user_id, true).await? {
                payments::CancelOutcome::Cancelled { .. } => true,
                payments::CancelOutcome::Blocked(payments::CancelBlock::AmountPaid(_)) => {
                    held_paid += 1;
                    false
                }
                payments::CancelOutcome::Blocked(payments::CancelBlock::PaymentInFlight) => {
                    held_in_flight += 1;
                    false
                }
                payments::CancelOutcome::Blocked(payments::CancelBlock::NotOpen) => false,
            },
        };
        if ok {
            done_n += 1;
        } else {
            skipped += 1;
        }
    }
    let mut text = ctx.tv2("admin.common.bulk_done", "done", done_n, "skipped", skipped);
    if held_paid > 0 {
        text.push(' ');
        text.push_str(&ctx.tv("admin.invoices.bulk_cancel_paid", "count", held_paid));
    }
    if held_in_flight > 0 {
        text.push(' ');
        text.push_str(&ctx.tv(
            "admin.invoices.bulk_cancel_in_flight",
            "count",
            held_in_flight,
        ));
    }
    Ok(crate::flash::redirect(
        &ctx.url(&back),
        crate::flash::SetFlash::success(text),
    ))
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct TxFilter {
    kind: Option<String>,
    gateway: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/invoices/transactions.html")]
struct TxPage {
    ctx: PageCtx,
    rows: Vec<TransactionRow>,
    pager: Pager,
    kind: String,
    gateway: String,
    from: String,
    to: String,
}
crate::page!(TxPage);

async fn transactions(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    q: PageQuery,
    Query(f): Query<TxFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let parse = |s: &Option<String>, end: bool| {
        s.as_deref()
            .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .and_then(|d| if end { d.succ_opt() } else { Some(d) })
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|d| d.and_utc())
    };
    let paged = payments::list_transactions(
        &state.pool,
        tenant.id,
        &TransactionFilter {
            client_id: None,
            kind: f.kind.as_deref().and_then(|k| k.parse().ok()),
            gateway: f.gateway.clone().filter(|g| !g.is_empty()),
            from: parse(&f.from, false),
            to: parse(&f.to, true),
        },
        q.db_page(),
    )
    .await?;
    let title = ctx.t("admin.transactions.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let (kind, gateway, from, to) = (
        f.kind.unwrap_or_default(),
        f.gateway.unwrap_or_default(),
        f.from.unwrap_or_default(),
        f.to.unwrap_or_default(),
    );
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/transactions"),
        &[
            ("kind", kind.as_str()),
            ("gateway", gateway.as_str()),
            ("from", from.as_str()),
            ("to", to.as_str()),
        ],
    );
    render(&TxPage {
        rows: paged.items,
        pager,
        kind,
        gateway,
        from,
        to,
        ctx,
    })
}
