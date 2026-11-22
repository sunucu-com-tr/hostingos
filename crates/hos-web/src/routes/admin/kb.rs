//! `/admin/support/kb*` and `/admin/support/announcements*` — knowledge-base
//! categories (tr / en names), KB articles / FAQ entries and announcements.
//! Articles and announcements use the translation-group editor of
//! [`super::content`] (tr / en side by side, Markdown + preview, publishing
//! state, `seo_meta`).

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_db::ids::KbCategoryId;
use hos_db::repo::content::{self, KbCategoryInput, KbCategoryRow, KbCategoryWithCount};
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData};
use super::content::{Kind, valid_slug};
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::forms::FormErrors;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/support/kb/categories",
            get(categories).post(create_category),
        )
        .route(
            "/admin/support/kb/categories/{id}",
            get(edit_category).post(update_category),
        )
        .route(
            "/admin/support/kb/categories/{id}/delete",
            post(delete_category),
        )
        .merge(super::content::kind_router(Kind::Kb))
        .merge(super::content::kind_router(Kind::News))
}

/// Category form values.
#[derive(Debug, Clone, Default)]
pub struct CategoryForm {
    pub name: String,
    pub name_en: String,
    pub slug: String,
    pub description: String,
    pub description_en: String,
    pub icon: String,
    pub sort_order: String,
    pub is_hidden: bool,
}

impl CategoryForm {
    fn from_row(c: &KbCategoryRow) -> Self {
        let tr = |field: &str| {
            c.translations
                .get("en")
                .and_then(|e| e.get(field))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            name: c.name.clone(),
            name_en: tr("name"),
            slug: c.slug.clone(),
            description: c.description.clone().unwrap_or_default(),
            description_en: tr("description"),
            icon: c.icon.clone().unwrap_or_default(),
            sort_order: c.sort_order.to_string(),
            is_hidden: c.is_hidden,
        }
    }

    fn read(f: &FormData) -> Self {
        Self {
            name: f.text("name", 120),
            name_en: f.text("name_en", 120),
            slug: f.text("slug", 80).to_lowercase(),
            description: f.text("description", 500),
            description_en: f.text("description_en", 500),
            icon: f.text("icon", 40),
            sort_order: f.text("sort_order", 10),
            is_hidden: f.flag("is_hidden"),
        }
    }

    fn validate(&mut self, ctx: &PageCtx, errors: &mut FormErrors) -> KbCategoryInput {
        if self.name.is_empty() {
            errors.add("name", ctx.t("admin.common.required"));
        }
        if self.slug.is_empty() {
            self.slug = crate::markdown::slugify(&self.name);
        }
        if !valid_slug(&self.slug) {
            errors.add("slug", ctx.t("admin_content.editor.slug_invalid"));
        }
        let sort_order = match self.sort_order.trim() {
            "" => 0,
            s => s.parse().unwrap_or_else(|_| {
                errors.add("sort_order", ctx.t("admin.common.invalid_value"));
                0
            }),
        };
        let mut en = serde_json::Map::new();
        if !self.name_en.is_empty() {
            en.insert("name".into(), json!(self.name_en));
        }
        if !self.description_en.is_empty() {
            en.insert("description".into(), json!(self.description_en));
        }
        let translations = if en.is_empty() {
            json!({})
        } else {
            json!({ "en": en })
        };
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        KbCategoryInput {
            slug: self.slug.clone(),
            name: self.name.clone(),
            description: opt(&self.description),
            translations,
            icon: opt(&self.icon),
            is_hidden: self.is_hidden,
            sort_order,
        }
    }
}

#[derive(Template)]
#[template(path = "admin/kb/categories.html")]
struct CategoriesPage {
    ctx: PageCtx,
    rows: Vec<KbCategoryWithCount>,
    form: CategoryForm,
    errors: FormErrors,
}
crate::page!(CategoriesPage);

async fn categories_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    form: CategoryForm,
    errors: FormErrors,
) -> AppResult<CategoriesPage> {
    let title = ctx.t("admin_support.kb.categories_title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_content.kinds.kb.title", Kind::Kb.base())],
    );
    let rows = content::list_kb_categories_admin(&state.pool, tenant.id).await?;
    Ok(CategoriesPage {
        ctx,
        rows,
        form,
        errors,
    })
}

async fn categories(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = CategoryForm {
        sort_order: "0".into(),
        ..CategoryForm::default()
    };
    let page = categories_page(&state, &tenant, &staff, ctx, form, FormErrors::default()).await?;
    render(&page)
}

async fn create_category(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut form = CategoryForm::read(&f);
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &mut errors);
    if !errors.any()
        && content::kb_category_by_slug(&state.pool, tenant.id, &input.slug)
            .await?
            .is_some()
    {
        errors.add("slug", ctx.t("admin_content.editor.slug_taken"));
    }
    if errors.any() {
        let page = categories_page(&state, &tenant, &staff, ctx, form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let row = content::create_kb_category_full(&state.pool, tenant.id, &input).await?;
    au.log(
        &state,
        "kb_category.create",
        Some(("kb_category", row.id.into())),
        json!({"slug": row.slug, "name": row.name}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/kb/categories",
        "admin.common.created",
    ))
}

#[derive(Template)]
#[template(path = "admin/kb/category.html")]
struct CategoryPage {
    ctx: PageCtx,
    id: KbCategoryId,
    form: CategoryForm,
    errors: FormErrors,
}
crate::page!(CategoryPage);

async fn load_category(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<KbCategoryRow> {
    content::get_kb_category(&state.pool, tenant.id, KbCategoryId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

fn category_page(
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    id: KbCategoryId,
    form: CategoryForm,
    errors: FormErrors,
) -> CategoryPage {
    let title = if form.name.is_empty() {
        ctx.t("admin_support.kb.categories_title")
    } else {
        form.name.clone()
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[
            ("admin_content.kinds.kb.title", Kind::Kb.base()),
            (
                "admin_support.kb.categories_title",
                "/admin/support/kb/categories",
            ),
        ],
    );
    CategoryPage {
        ctx,
        id,
        form,
        errors,
    }
}

async fn edit_category(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = load_category(&state, &tenant, id).await?;
    render(&category_page(
        &staff,
        ctx,
        c.id,
        CategoryForm::from_row(&c),
        FormErrors::default(),
    ))
}

async fn update_category(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let before = load_category(&state, &tenant, id).await?;
    let mut form = CategoryForm::read(&f);
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &mut errors);
    if !errors.any()
        && content::kb_category_by_slug(&state.pool, tenant.id, &input.slug)
            .await?
            .is_some_and(|c| c.id != before.id)
    {
        errors.add("slug", ctx.t("admin_content.editor.slug_taken"));
    }
    if errors.any() {
        let page = category_page(&staff, ctx, before.id, form, errors);
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    content::update_kb_category(&state.pool, tenant.id, before.id, &input)
        .await?
        .ok_or(AppError::NotFound)?;
    au.log(
        &state,
        "kb_category.update",
        Some(("kb_category", before.id.into())),
        json!({"before": {"slug": before.slug, "name": before.name}, "after": {"slug": input.slug, "name": input.name}}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/kb/categories",
        "admin.common.saved",
    ))
}

async fn delete_category(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = load_category(&state, &tenant, id).await?;
    content::delete_kb_category(&state.pool, tenant.id, c.id).await?;
    au.log(
        &state,
        "kb_category.delete",
        Some(("kb_category", c.id.into())),
        json!({"slug": c.slug}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/kb/categories",
        "admin.common.deleted",
    ))
}
