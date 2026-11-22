//! `/admin/downloads` — the downloads area: categories and files offered
//! publicly, to signed-in clients or to clients with an active service of
//! a linked product. Needs `content.manage`; uploads are type-checked by
//! their bytes, hashed (SHA-256) and audited; staff can fetch any file.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::ids::ProductId;
use hos_db::repo::downloads::{
    self as drepo, CategoryInput, DownloadAccess, DownloadCategoryId, DownloadCategoryRow,
    DownloadFile, DownloadId, DownloadInput, DownloadRow,
};
use serde_json::json;
use uuid::Uuid;

use super::catalog::{product_choices, storefront_changed};
use super::common::{self, Audit, FormData, Multipart, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::routes::client::downloads::{download_type, file_response, size_text};
use crate::routes::client::support::clean_filename;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/downloads", get(list).post(create))
        .route("/admin/downloads/{id}", get(edit).post(update))
        .route("/admin/downloads/{id}/delete", post(delete))
        .route("/admin/downloads/{id}/file", get(file))
        .route("/admin/downloads/categories", post(create_category))
        .route("/admin/downloads/categories/{id}", post(update_category))
        .route(
            "/admin/downloads/categories/{id}/delete",
            post(delete_category),
        )
}

const BACK: &str = "/admin/downloads";

#[derive(Template)]
#[template(path = "admin/downloads.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<DownloadRow>,
    categories: Vec<DownloadCategoryRow>,
    products: Vec<(ProductId, String)>,
    editing: Option<DownloadRow>,
    max_kb: usize,
}
crate::page!(ListPage);

impl ListPage {
    fn size(&self, d: &DownloadRow) -> String {
        size_text(&self.ctx, d.size_bytes)
    }
    fn category_name(&self, d: &DownloadRow) -> String {
        d.category_id
            .and_then(|c| self.categories.iter().find(|x| x.id == c))
            .map_or_else(|| "—".to_owned(), |c| c.name.clone())
    }
    fn access_label(&self, a: &DownloadAccess) -> String {
        self.t(&format!("downloads.access.{}", a.as_str()))
    }
    fn access_options(&self) -> Vec<(String, String)> {
        common::enum_options(
            self,
            DownloadAccess::ALL,
            |a| a.as_str(),
            "downloads.access",
        )
    }
    fn form_title(&self) -> String {
        self.t(if self.editing.is_some() {
            "downloads.admin.edit"
        } else {
            "downloads.admin.new"
        })
    }
    fn form_action(&self) -> String {
        match &self.editing {
            Some(d) => self.url(format!("/admin/downloads/{}", d.id)),
            None => self.url(BACK),
        }
    }
    fn edit_value(&self, f: &str) -> String {
        let Some(d) = &self.editing else {
            return match f {
                "access" => "clients".into(),
                "sort" => "0".into(),
                _ => String::new(),
            };
        };
        match f {
            "title" => d.title.clone(),
            "description" => d.description.clone().unwrap_or_default(),
            "access" => d.access.as_str().into(),
            "sort" => d.sort_order.to_string(),
            _ => String::new(),
        }
    }
    fn cat_sel(&self, c: &DownloadCategoryRow) -> &'static str {
        if self
            .editing
            .as_ref()
            .is_some_and(|d| d.category_id == Some(c.id))
        {
            " selected"
        } else {
            ""
        }
    }
    fn edit_hidden(&self) -> bool {
        self.editing.as_ref().is_some_and(|d| d.is_hidden)
    }
    fn product_checked(&self, p: &ProductId) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|d| d.product_ids.contains(&p.into_uuid()))
    }
    fn is_editing(&self, d: &DownloadRow) -> bool {
        self.editing.as_ref().is_some_and(|e| e.id == d.id)
    }
}

async fn render_list(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    editing: Option<DownloadRow>,
) -> AppResult<Response> {
    let title = ctx.t("downloads.admin.title");
    common::page(&mut ctx, staff, &title, &[]);
    render(&ListPage {
        rows: drepo::list_all(&state.pool, tenant.id).await?,
        categories: drepo::categories(&state.pool, tenant.id).await?,
        products: product_choices(state, tenant).await?,
        editing,
        max_kb: state.web.max_body_bytes / 1024,
        ctx,
    })
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ContentManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    render_list(&state, &tenant, &staff, ctx, None).await
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ContentManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = drepo::get(&state.pool, tenant.id, DownloadId::from(id))
        .await?
        .ok_or(AppError::NotFound)?;
    render_list(&state, &tenant, &staff, ctx, Some(row)).await
}

/// Validated metadata: the category and every product must be this
/// tenant's; the product rule needs at least one product.
async fn input(
    state: &AppState,
    tenant: &Tenant,
    f: &FormData,
) -> AppResult<Result<DownloadInput, &'static str>> {
    let title = f.text("title", 200);
    if title.is_empty() {
        return Ok(Err("downloads.admin.title_required"));
    }
    let Some(access) = f.parse::<DownloadAccess>("access") else {
        return Ok(Err("downloads.admin.invalid"));
    };
    let category_id = match f.id::<DownloadCategoryId>("category_id") {
        Some(c) => {
            let cats = drepo::categories(&state.pool, tenant.id).await?;
            if !cats.iter().any(|x| x.id == c) {
                return Err(AppError::NotFound);
            }
            Some(c)
        }
        None => None,
    };
    let mut product_ids: Vec<ProductId> = f.ids("product_ids");
    product_ids.sort_by_key(|p| p.into_uuid());
    product_ids.dedup();
    for p in &product_ids {
        hos_db::repo::catalog::get_product(&state.pool, tenant.id, *p)
            .await?
            .ok_or(AppError::NotFound)?;
    }
    if access == DownloadAccess::Product && product_ids.is_empty() {
        return Ok(Err("downloads.admin.product_required"));
    }
    let Ok(sort) = f.int("sort_order") else {
        return Ok(Err("downloads.admin.invalid"));
    };
    Ok(Ok(DownloadInput {
        category_id,
        title,
        description: f.opt("description", 2000),
        access,
        is_hidden: f.flag("is_hidden"),
        sort_order: i32::try_from(sort.unwrap_or(0)).unwrap_or(0),
        product_ids,
    }))
}

/// The uploaded file, checked: an accepted type by its bytes (images only
/// in the public demo), within the request size limit. `Ok(None)`: none sent.
fn upload(state: &AppState, mp: &Multipart) -> Result<Option<DownloadFile>, &'static str> {
    let Some(part) = mp.file("file") else {
        return Ok(None);
    };
    let filename = clean_filename(part.filename.as_deref().unwrap_or_default());
    let Some(content_type) = download_type(&filename, &part.data)
        .filter(|ct| crate::demo::upload_type_allowed(state, ct))
    else {
        return Err("downloads.admin.bad_type");
    };
    Ok(Some(DownloadFile {
        sha256: hos_core::crypto::sha256_hex(&part.data),
        content_type: content_type.to_owned(),
        filename,
        content: part.data.clone(),
    }))
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ContentManage>,
    au: Audit,
    ctx: PageCtx,
    mp: Multipart,
) -> AppResult<Response> {
    let f = mp.form();
    let d = match input(&state, &tenant, &f).await? {
        Ok(d) => d,
        Err(key) => return Ok(failed(&ctx, BACK, ctx.t(key))),
    };
    let file = match upload(&state, &mp) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(failed(&ctx, BACK, ctx.t("downloads.admin.file_required"))),
        Err(key) => return Ok(failed(&ctx, BACK, ctx.t(key))),
    };
    let mut tx = state.pool.begin().await?;
    let id = drepo::create(
        &mut tx,
        tenant.id,
        &d,
        &file.filename,
        Some(&file),
        Some(staff.user_id),
    )
    .await?;
    tx.commit().await?;
    storefront_changed(&state);
    au.log(
        &state,
        "download.create",
        Some(("download", id.into_uuid())),
        json!({"title": d.title, "access": d.access, "filename": file.filename,
               "size": file.content.len(), "sha256": file.sha256}),
    )
    .await?;
    Ok(done(&ctx, BACK, "downloads.admin.saved"))
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ContentManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    mp: Multipart,
) -> AppResult<Response> {
    let id = DownloadId::from(id);
    let current = drepo::get(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let back = format!("/admin/downloads/{id}");
    let f = mp.form();
    let d = match input(&state, &tenant, &f).await? {
        Ok(d) => d,
        Err(key) => return Ok(failed(&ctx, &back, ctx.t(key))),
    };
    let file = match upload(&state, &mp) {
        Ok(file) => file,
        Err(key) => return Ok(failed(&ctx, &back, ctx.t(key))),
    };
    if file.is_none() && !current.has_file {
        return Ok(failed(&ctx, &back, ctx.t("downloads.admin.file_required")));
    }
    let mut tx = state.pool.begin().await?;
    drepo::update(&mut tx, tenant.id, id, &d, file.as_ref()).await?;
    tx.commit().await?;
    storefront_changed(&state);
    au.log(
        &state,
        "download.update",
        Some(("download", id.into_uuid())),
        json!({"title": d.title, "access": d.access,
               "file": file.as_ref().map(|f| json!({"filename": f.filename, "size": f.content.len(), "sha256": f.sha256}))}),
    )
    .await?;
    Ok(done(&ctx, BACK, "downloads.admin.saved"))
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ContentManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = DownloadId::from(id);
    let row = drepo::get(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    drepo::delete(&state.pool, tenant.id, id).await?;
    storefront_changed(&state);
    au.log(
        &state,
        "download.delete",
        Some(("download", id.into_uuid())),
        json!({"title": row.title, "filename": row.filename}),
    )
    .await?;
    Ok(done(&ctx, BACK, "downloads.admin.deleted"))
}

/// Staff fetch of any download (hidden ones too); not counted.
async fn file(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ContentManage>,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let id = DownloadId::from(id);
    let row = drepo::get(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let bytes = drepo::file(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(file_response(&row, bytes))
}

fn category_input(f: &FormData) -> Result<CategoryInput, &'static str> {
    let name = f.text("name", 120);
    if name.is_empty() {
        return Err("downloads.admin.name_required");
    }
    let Ok(sort) = f.int("sort_order") else {
        return Err("downloads.admin.invalid");
    };
    Ok(CategoryInput {
        name,
        description: f.opt("description", 500),
        is_hidden: f.flag("is_hidden"),
        sort_order: i32::try_from(sort.unwrap_or(0)).unwrap_or(0),
    })
}

async fn create_category(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ContentManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = match category_input(&f) {
        Ok(c) => c,
        Err(key) => return Ok(failed(&ctx, BACK, ctx.t(key))),
    };
    let row = drepo::create_category(&state.pool, tenant.id, &c).await?;
    au.log(
        &state,
        "download_category.create",
        Some(("download_category", row.id.into_uuid())),
        json!({"name": row.name}),
    )
    .await?;
    Ok(done(&ctx, BACK, "downloads.admin.category_saved"))
}

async fn update_category(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ContentManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = DownloadCategoryId::from(id);
    let c = match category_input(&f) {
        Ok(c) => c,
        Err(key) => return Ok(failed(&ctx, BACK, ctx.t(key))),
    };
    let row = drepo::update_category(&state.pool, tenant.id, id, &c).await?;
    storefront_changed(&state);
    au.log(
        &state,
        "download_category.update",
        Some(("download_category", row.id.into_uuid())),
        json!({"name": row.name, "hidden": row.is_hidden}),
    )
    .await?;
    Ok(done(&ctx, BACK, "downloads.admin.category_saved"))
}

async fn delete_category(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ContentManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = DownloadCategoryId::from(id);
    if !drepo::delete_category(&state.pool, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    storefront_changed(&state);
    au.log(
        &state,
        "download_category.delete",
        Some(("download_category", id.into_uuid())),
        json!({}),
    )
    .await?;
    Ok(done(&ctx, BACK, "downloads.admin.category_deleted"))
}
