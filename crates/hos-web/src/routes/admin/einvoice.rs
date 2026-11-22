//! e-Fatura / e-Arşiv in the admin panel and the invoice PDF download.
//!
//! | Route | Permission | |
//! |---|---|---|
//! | `GET /admin/einvoice` | `billing.view` | documents, filter `problems` (default) / `all` / a status |
//! | `GET /admin/invoices/{id}/pdf` | `billing.view` | the invoice PDF (`hos_billing::invoice_pdf`) |
//! | `POST /admin/invoices/{id}/einvoice` | `billing.manage` | issue now (creates / requeues the document) |
//! | `POST /admin/einvoice/{doc}/retry` | `billing.manage` | retry a failed document |
//! | `POST /admin/einvoice/{doc}/cancel` | `billing.manage` | cancel an issued e-Arşiv (queued) |
//! | `POST /admin/einvoice/{doc}/refresh` | `billing.manage` | re-read the status at the integrator (queued) |
//! | `POST /admin/einvoice/{doc}/acknowledge` | `billing.manage` | clear the staff flag |
//! | `GET /admin/einvoice/{doc}/pdf`, `…/ubl` | `billing.view` | legal PDF / UBL from the integrator |
//!
//! Every query is tenant-scoped (`hos_db::repo::einvoice` takes the tenant
//! id; a document of another tenant is a `404`), every POST passes the CSRF
//! middleware and writes `audit_log` (`einvoice.*`). The invoice page shows
//! the [`Panel`] (`templates/admin/einvoice/_panel.html`).

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_core::domain::InvoiceStatus;
use hos_core::ids::InvoiceId;
use hos_db::repo::einvoice::{
    self as edocs, DocumentListRow, EInvoiceDocumentId, EInvoiceDocumentRow,
};
use hos_db::repo::invoices::{self, InvoiceRow};
use hos_db::{Page, Paged};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, Ui as _, done, failed};
use crate::einvoice::file_response;
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/einvoice", get(list))
        .route("/admin/invoices/{id}/pdf", get(invoice_pdf))
        .route("/admin/invoices/{id}/einvoice", post(issue))
        .route("/admin/einvoice/{id}/retry", post(retry))
        .route("/admin/einvoice/{id}/cancel", post(cancel))
        .route("/admin/einvoice/{id}/refresh", post(refresh))
        .route("/admin/einvoice/{id}/acknowledge", post(acknowledge))
        .route("/admin/einvoice/{id}/pdf", get(legal_pdf))
        .route("/admin/einvoice/{id}/ubl", get(legal_ubl))
}

fn tone(status: &str) -> &'static str {
    match status {
        "issued" => "success",
        "pending" | "processing" => "warning",
        "failed" | "rejected" => "danger",
        _ => "",
    }
}

fn opt(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Invoice page panel
// ---------------------------------------------------------------------------

/// One e-document on the invoice page.
#[derive(Debug, Clone)]
pub struct DocView {
    pub id: String,
    pub purpose: String,
    pub kind: String,
    pub number: String,
    pub ettn: String,
    pub status: String,
    pub status_label: String,
    pub tone: &'static str,
    pub is_test: bool,
    pub attempts: i32,
    pub last_error: String,
    pub message: String,
    pub attention: String,
    pub issued_at: String,
    pub cancellable_until: String,
    pub can_retry: bool,
    pub can_cancel: bool,
    pub can_refresh: bool,
    pub can_download: bool,
}

/// The e-document card of the admin invoice page.
#[derive(Debug, Clone)]
pub struct Panel {
    pub enabled: bool,
    pub docs: Vec<DocView>,
    pub can_issue: bool,
}

fn doc_view(ctx: &PageCtx, d: &EInvoiceDocumentRow) -> DocView {
    let now = Utc::now();
    let final_doc = matches!(d.status.as_str(), "issued" | "cancelled" | "rejected");
    DocView {
        id: d.id.to_string(),
        purpose: ctx.t(&format!("einvoice.purpose.{}", d.purpose)),
        kind: ctx.t(&format!(
            "einvoice.kind.{}",
            d.kind.as_deref().unwrap_or("pending")
        )),
        number: opt(&d.number),
        ettn: d.ettn.to_string(),
        status: d.status.clone(),
        status_label: ctx.t(&format!("einvoice.status.{}", d.status)),
        tone: tone(&d.status),
        is_test: d.is_test,
        attempts: d.attempts,
        last_error: opt(&d.last_error),
        message: opt(&d.message),
        attention: opt(&d.attention),
        issued_at: d.issued_at.map(|t| ctx.datetime(&t)).unwrap_or_default(),
        cancellable_until: d
            .cancellable_until
            .map(|t| ctx.datetime(&t))
            .unwrap_or_default(),
        can_retry: d.status == "failed",
        can_cancel: d.status == "issued"
            && d.kind.as_deref() == Some("earsiv")
            && d.cancellable_until.is_none_or(|t| t > now),
        can_refresh: final_doc || d.status == "processing",
        can_download: final_doc,
    }
}

/// The panel of `inv` (`None`: e-invoicing off and no document).
pub async fn panel(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    inv: &InvoiceRow,
) -> AppResult<Option<Panel>> {
    let enabled = crate::einvoice::enabled(state, tenant.id).await?;
    let mut conn = state.pool.acquire().await?;
    let rows = edocs::list_for_invoice(&mut conn, tenant.id, inv.id).await?;
    drop(conn);
    if !enabled && rows.is_empty() {
        return Ok(None);
    }
    let has_sale = rows.iter().any(|r| r.purpose == "sale");
    let can_issue = enabled
        && !has_sale
        && inv.number.is_some()
        && !matches!(inv.status, InvoiceStatus::Draft | InvoiceStatus::Cancelled);
    Ok(Some(Panel {
        enabled,
        docs: rows.iter().map(|d| doc_view(ctx, d)).collect(),
        can_issue,
    }))
}

// ---------------------------------------------------------------------------
// Document list
// ---------------------------------------------------------------------------

const FILTERS: &[&str] = &[
    "problems",
    "all",
    "pending",
    "processing",
    "issued",
    "failed",
    "rejected",
    "cancelled",
    "skipped",
];

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    status: Option<String>,
}

/// One row of the list.
pub struct ListRow {
    pub row: DocumentListRow,
    pub kind: String,
    pub status_label: String,
    pub tone: &'static str,
    pub problem: String,
    pub updated: String,
}

#[derive(Template)]
#[template(path = "admin/einvoice/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ListRow>,
    pager: Pager,
    status: String,
    filters: Vec<(String, String)>,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status = f
        .status
        .filter(|s| FILTERS.contains(&s.as_str()))
        .unwrap_or_else(|| "problems".to_owned());
    let page: Page = q.db_page();
    let mut conn = state.pool.acquire().await?;
    let total = edocs::count_documents(&mut conn, tenant.id, &status).await?;
    let items =
        edocs::list_documents(&mut conn, tenant.id, &status, page.limit, page.offset).await?;
    drop(conn);
    let rows = items
        .into_iter()
        .map(|row| ListRow {
            kind: ctx.t(&format!(
                "einvoice.kind.{}",
                row.kind.as_deref().unwrap_or("pending")
            )),
            status_label: ctx.t(&format!("einvoice.status.{}", row.status)),
            tone: tone(&row.status),
            problem: row
                .attention
                .clone()
                .or_else(|| row.last_error.clone())
                .or_else(|| row.message.clone())
                .unwrap_or_default(),
            updated: ctx.datetime(&row.updated_at),
            row,
        })
        .collect::<Vec<_>>();
    let paged = Paged {
        items: Vec::<()>::new(),
        total,
        limit: page.limit,
        offset: page.offset,
    };
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/einvoice"),
        &[("status", status.as_str())],
    );
    let filters = FILTERS
        .iter()
        .map(|s| {
            let label = match *s {
                "problems" => ctx.t("einvoice.list.filter_problems"),
                "all" => ctx.t("einvoice.list.filter_all"),
                other => ctx.t(&format!("einvoice.status.{other}")),
            };
            ((*s).to_owned(), label)
        })
        .collect();
    let title = ctx.t("einvoice.list.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&ListPage {
        ctx,
        rows,
        pager,
        status,
        filters,
    })
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async fn document(state: &AppState, tenant: &Tenant, raw: Uuid) -> AppResult<EInvoiceDocumentRow> {
    let mut conn = state.pool.acquire().await?;
    edocs::get_document(&mut conn, tenant.id, EInvoiceDocumentId::from(raw))
        .await?
        .ok_or(AppError::NotFound)
}

fn back(doc: &EInvoiceDocumentRow) -> String {
    format!("/admin/invoices/{}", doc.invoice_id)
}

async fn issue(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let inv = invoices::get_invoice(&state.pool, tenant.id, InvoiceId::from(id))
        .await?
        .ok_or(AppError::NotFound)?;
    let back = format!("/admin/invoices/{}", inv.id);
    match state.billing.einvoice_issue_now(tenant.id, inv.id).await {
        Ok(doc) => {
            au.log(
                &state,
                "einvoice.issue",
                Some(("invoice", inv.id.into())),
                json!({"document": doc.id.to_string()}),
            )
            .await?;
            Ok(done(&ctx, &back, "einvoice.flash.queued"))
        }
        Err(e) => Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
}

/// A queued staff action on a document: audit + flash.
async fn act(
    state: &AppState,
    au: &Audit,
    ctx: &PageCtx,
    doc: &EInvoiceDocumentRow,
    action: &str,
    result: Result<(), hos_core::CoreError>,
    ok_key: &str,
) -> AppResult<Response> {
    let back = back(doc);
    if result.is_ok() {
        au.log(
            state,
            &format!("einvoice.{action}"),
            Some(("invoice", doc.invoice_id.into())),
            json!({"document": doc.id.to_string(), "status": doc.status}),
        )
        .await?;
    }
    common::outcome(ctx, &back, result, ok_key)
}

async fn retry(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let doc = document(&state, &tenant, id).await?;
    let r = state.billing.einvoice_retry(tenant.id, doc.id).await;
    act(
        &state,
        &au,
        &ctx,
        &doc,
        "retry",
        r,
        "einvoice.flash.retry_queued",
    )
    .await
}

async fn cancel(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let doc = document(&state, &tenant, id).await?;
    let r = state
        .billing
        .einvoice_request_cancel(tenant.id, doc.id)
        .await;
    act(
        &state,
        &au,
        &ctx,
        &doc,
        "cancel",
        r,
        "einvoice.flash.cancel_queued",
    )
    .await
}

async fn refresh(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let doc = document(&state, &tenant, id).await?;
    let r = state
        .billing
        .einvoice_request_refresh(tenant.id, doc.id)
        .await;
    act(
        &state,
        &au,
        &ctx,
        &doc,
        "refresh",
        r,
        "einvoice.flash.refresh_queued",
    )
    .await
}

async fn acknowledge(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let doc = document(&state, &tenant, id).await?;
    let r = state.billing.einvoice_acknowledge(tenant.id, doc.id).await;
    act(
        &state,
        &au,
        &ctx,
        &doc,
        "acknowledge",
        r,
        "einvoice.flash.acknowledged",
    )
    .await
}

// ---------------------------------------------------------------------------
// Downloads
// ---------------------------------------------------------------------------

async fn legal_file(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    id: Uuid,
    pdf: bool,
) -> AppResult<Response> {
    let doc = document(state, tenant, id).await?;
    match state.billing.einvoice_file(tenant.id, doc.id, pdf).await {
        Ok((data, name)) => Ok(file_response(
            data,
            &name,
            if pdf {
                "application/pdf"
            } else {
                "application/xml"
            },
        )),
        Err(e) => Ok(failed(ctx, &back(&doc), common::failure_text(ctx, &e)?)),
    }
}

async fn legal_pdf(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingView>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    legal_file(&state, &tenant, &ctx, id, true).await
}

async fn legal_ubl(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingView>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    legal_file(&state, &tenant, &ctx, id, false).await
}

/// `GET /admin/invoices/{id}/pdf` — drafts included (staff preview).
async fn invoice_pdf(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingView>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let pdf = state
        .billing
        .invoice_pdf(tenant.id, InvoiceId::from(id), Some(ctx.locale))
        .await?;
    Ok(file_response(pdf.data, &pdf.filename, "application/pdf"))
}
