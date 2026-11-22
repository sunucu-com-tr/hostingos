//! Invoice downloads in the client area (`invoices` member area):
//!
//! * `GET /client/invoices/{id}/pdf` — the invoice PDF in the viewer's locale;
//! * `GET /client/invoices/{id}/e-invoice` — the legal e-Fatura / e-Arşiv PDF
//!   from the integrator, once issued (never a test-environment document).
//!
//! Ownership: the invoice must belong to the signed-in client account
//! (`client.client_id`) and not be a draft; anything else — another
//! client's invoice, another tenant's id, no issued document — is a `404`.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use hos_core::domain::InvoiceStatus;
use hos_core::ids::InvoiceId;
use hos_db::repo::einvoice as edocs;
use hos_db::repo::invoices::{self, InvoiceRow};

use super::parse_id;
use crate::einvoice::file_response;
use crate::error::{AppError, AppResult};
use crate::extract::CurrentClient;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::PageCtx;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/invoices/{id}/pdf", get(pdf))
        .route("/client/invoices/{id}/e-invoice", get(legal_pdf))
}

/// The client's own, non-draft invoice.
async fn own(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    raw: &str,
) -> AppResult<InvoiceRow> {
    let id: InvoiceId = parse_id(raw)?;
    invoices::get_invoice(&state.pool, tenant.id, id)
        .await?
        .filter(|i| i.client_id == client.client_id && i.status != InvoiceStatus::Draft)
        .ok_or(AppError::NotFound)
}

async fn pdf(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let inv = own(&state, &tenant, &client, &id).await?;
    let pdf = state
        .billing
        .invoice_pdf(tenant.id, inv.id, Some(ctx.locale))
        .await?;
    Ok(file_response(pdf.data, &pdf.filename, "application/pdf"))
}

async fn legal_pdf(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let inv = own(&state, &tenant, &client, &id).await?;
    let mut conn = state.pool.acquire().await?;
    let doc = edocs::for_invoice(&mut conn, tenant.id, inv.id, "sale")
        .await?
        .filter(|d| d.status == "issued" && !d.is_test)
        .ok_or(AppError::NotFound)?;
    drop(conn);
    let (data, name) = state.billing.einvoice_file(tenant.id, doc.id, true).await?;
    Ok(file_response(data, &name, "application/pdf"))
}
