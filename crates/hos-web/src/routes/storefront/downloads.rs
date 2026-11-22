//! Public downloads (`/downloads`, `/downloads/{id}/file`): only downloads
//! marked `public`; fetches are rate limited per IP and served as
//! attachments (see [`crate::routes::client::downloads::file_response`]).

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;
use hos_db::repo::downloads::{self as drepo, DownloadId, DownloadRow};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::extract::ClientIp;
use crate::routes::client::downloads::{DownloadGroup, file_response, group, size_text};
use crate::seo::{self, SeoPage};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

#[derive(Template)]
#[template(path = "storefront/downloads.html")]
struct DownloadsPage {
    ctx: PageCtx,
    title: String,
    groups: Vec<DownloadGroup>,
}
crate::page!(DownloadsPage);

impl DownloadsPage {
    fn size(&self, d: &DownloadRow) -> String {
        size_text(&self.ctx, d.size_bytes)
    }
}

/// `GET /downloads`.
pub(super) async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let rows = drepo::visible(&state.pool, tenant.id, None).await?;
    let categories = drepo::categories(&state.pool, tenant.id).await?;
    let groups = group(&categories, rows, ctx.t("downloads.other"));
    let title = ctx.t("downloads.title");
    let page = SeoPage::new(title.clone(), "/downloads")
        .description(ctx.t("downloads.public_lead"))
        .crumb(title.clone(), "/downloads");
    seo::apply(&mut ctx, page).await?;
    render(&DownloadsPage { ctx, title, groups })
}

/// `GET /downloads/{id}/file` — public downloads only.
pub(super) async fn file(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    check_rate(&state.limits.downloads, &ip.key("download"))?;
    let (row, bytes) = drepo::fetch_for(&state.pool, tenant.id, DownloadId::from(id), None)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(file_response(&row, bytes))
}
