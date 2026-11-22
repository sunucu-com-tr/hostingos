//! Invoice documents: `GET /api/v1/invoices/{id}/pdf` — the invoice PDF
//! (`application/pdf`, `?locale=tr|en`, default: the client's language).
//! Staff need `billing.view` (drafts included); client keys only get their
//! own non-draft invoices (others answer `404`).

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::response::Response;
use hos_auth::rbac::Permission;
use hos_core::domain::{InvoiceStatus, Locale};
use hos_core::ids::InvoiceId;
use hos_db::repo::invoices;

use super::{ApiAuth, ApiError, ApiResult, ApiTenant, parse_id};
use crate::einvoice::file_response;
use crate::state::AppState;

/// `GET /api/v1/invoices/{id}/pdf`.
pub async fn invoice_pdf(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let scope = auth.scope(Permission::BILLING_VIEW)?;
    let id: InvoiceId = parse_id(&id)?;
    let inv = invoices::get_invoice(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if scope.client().is_some() && inv.status == InvoiceStatus::Draft {
        return Err(ApiError::not_found());
    }
    scope.owns(Some(inv.client_id))?;
    let locale = match raw.get("locale").map(String::as_str) {
        None | Some("") => None,
        Some("tr") => Some(Locale::Tr),
        Some("en") => Some(Locale::En),
        Some(_) => return Err(ApiError::field("locale", "must be tr or en")),
    };
    let pdf = state.billing.invoice_pdf(tenant.id, inv.id, locale).await?;
    Ok(file_response(pdf.data, &pdf.filename, "application/pdf"))
}
