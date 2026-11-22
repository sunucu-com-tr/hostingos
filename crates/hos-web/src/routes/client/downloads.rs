//! Downloads area, client side (`/client/downloads`): public downloads,
//! downloads for signed-in clients and those tied to a product the client
//! has an active service of. Files are served as attachments with
//! `nosniff` and a sandbox CSP; the access rule is re-checked on every
//! fetch. Shared helpers serve the storefront's public list too.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hos_db::repo::downloads::{self as drepo, DownloadCategoryRow, DownloadId, DownloadRow};
use http::HeaderValue;
use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use uuid::Uuid;

use super::{MemberAccess, setup};
use crate::error::{AppError, AppResult};
use crate::extract::CurrentClient;
use crate::routes::client::support::{clean_filename, sniff};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/downloads", get(list))
        .route("/client/downloads/{id}/file", get(file))
}

/// Content type of an accepted download upload (the ticket upload types
/// plus gzip archives). `None`: rejected.
pub fn download_type(name: &str, bytes: &[u8]) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if bytes.starts_with(&[0x1F, 0x8B]) && (lower.ends_with(".gz") || lower.ends_with(".tgz")) {
        return Some("application/gzip");
    }
    sniff(name, bytes)
}

/// Serves stored bytes as a download: attachment, sanitised name,
/// `nosniff`, sandbox CSP, never cached by shared caches.
pub fn file_response(row: &DownloadRow, bytes: Vec<u8>) -> Response {
    let name = clean_filename(&row.filename);
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
    let disposition = format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        encoded.replace('+', "%20")
    );
    let content_type = download_type(&row.filename, &bytes).unwrap_or("application/octet-stream");
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("attachment")),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    res
}

/// A category with its downloads (list pages).
pub struct DownloadGroup {
    pub name: String,
    pub description: Option<String>,
    pub items: Vec<DownloadRow>,
}

/// Groups visible downloads by visible category (uncategorised last).
pub fn group(
    categories: &[DownloadCategoryRow],
    rows: Vec<DownloadRow>,
    other: String,
) -> Vec<DownloadGroup> {
    let mut out: Vec<DownloadGroup> = Vec::new();
    for c in categories.iter().filter(|c| !c.is_hidden) {
        let items: Vec<DownloadRow> = rows
            .iter()
            .filter(|d| d.category_id == Some(c.id))
            .cloned()
            .collect();
        if !items.is_empty() {
            out.push(DownloadGroup {
                name: c.name.clone(),
                description: c.description.clone(),
                items,
            });
        }
    }
    let rest: Vec<DownloadRow> = rows
        .into_iter()
        .filter(|d| d.category_id.is_none())
        .collect();
    if !rest.is_empty() {
        out.push(DownloadGroup {
            name: other,
            description: None,
            items: rest,
        });
    }
    out
}

/// Human file size (`1,2 MB`, `15,0 KB`).
pub fn size_text(ctx: &PageCtx, bytes: i64) -> String {
    let b = bytes.max(0);
    let (tenths, unit) = if b >= 1024 * 1024 {
        (b * 10 / (1024 * 1024), "MB")
    } else {
        ((b * 10 / 1024).max(1), "KB")
    };
    format!(
        "{} {unit}",
        crate::i18n::format_decimal(rust_decimal::Decimal::new(tenths, 1), 1, ctx.locale)
    )
}

#[derive(Template)]
#[template(path = "client/downloads.html")]
struct ListPage {
    ctx: PageCtx,
    groups: Vec<DownloadGroup>,
}
crate::page!(ListPage);

impl ListPage {
    fn size(&self, d: &DownloadRow) -> String {
        size_text(&self.ctx, d.size_bytes)
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = drepo::visible(&state.pool, tenant.id, Some(client.client_id)).await?;
    let categories = drepo::categories(&state.pool, tenant.id).await?;
    let groups = group(&categories, rows, ctx.t("downloads.other"));
    let title = ctx.t("downloads.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    render(&ListPage { ctx, groups })
}

async fn file(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let (row, bytes) = drepo::fetch_for(
        &state.pool,
        tenant.id,
        DownloadId::from(id),
        Some(client.client_id),
    )
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(file_response(&row, bytes))
}
