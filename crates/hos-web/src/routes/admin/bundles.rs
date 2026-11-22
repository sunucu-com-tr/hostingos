//! `/admin/catalog/bundles` — product bundles: two or more products (each
//! with a billing cycle) sold together at a percentage discount. The
//! discount is the bundle's own coupon (unguessable code, created and kept
//! in step here) that only applies while every product of the bundle is in
//! the cart. Needs `products.manage`; changes are audited.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::ids::ProductId;
use hos_core::money::BillingCycle;
use hos_db::enums::DiscountType;
use hos_db::repo::bundles::{self as brepo, BundleId, BundleInput, BundleItemRow, BundleRow};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

use super::catalog::{product_choices, storefront_changed};
use super::common::{self, Audit, FormData, Ui as _, done, failed};
use super::content::valid_slug;
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/catalog/bundles", get(list).post(create))
        .route("/admin/catalog/bundles/{id}", get(edit).post(update))
        .route("/admin/catalog/bundles/{id}/delete", post(delete))
}

const BACK: &str = "/admin/catalog/bundles";
/// Product rows on the form.
const ROWS: usize = 4;
/// Most products in one bundle.
const MAX_ITEMS: usize = 10;

/// A new bundle coupon code: `BND-` + 64 random bits (hex), not guessable
/// from the bundle.
pub fn bundle_code() -> String {
    let bytes = hos_core::crypto::random_bytes::<8>();
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!("BND-{hex}")
}

#[derive(Template)]
#[template(path = "admin/bundles.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<(BundleRow, Vec<BundleItemRow>)>,
    products: Vec<(ProductId, String)>,
    editing: Option<(BundleRow, Vec<BundleItemRow>)>,
}
crate::page!(ListPage);

impl ListPage {
    fn form_title(&self) -> String {
        self.t(if self.editing.is_some() {
            "bundles.admin.edit"
        } else {
            "bundles.admin.new"
        })
    }
    fn form_action(&self) -> String {
        match &self.editing {
            Some((b, _)) => self.url(format!("{BACK}/{}", b.id)),
            None => self.url(BACK),
        }
    }
    fn edit_value(&self, f: &str) -> String {
        let Some((b, _)) = &self.editing else {
            return match f {
                "sort" => "0".into(),
                _ => String::new(),
            };
        };
        match f {
            "name" => b.name.clone(),
            "slug" => b.slug.clone(),
            "description" => b.description.clone().unwrap_or_default(),
            "value" => b.discount_value.normalize().to_string(),
            "sort" => b.sort_order.to_string(),
            _ => String::new(),
        }
    }
    fn edit_flag(&self, f: &str) -> bool {
        match &self.editing {
            Some((b, _)) => match f {
                "active" => b.is_active,
                "storefront" => b.show_on_storefront,
                "recurring" => b.recurring,
                _ => false,
            },
            None => matches!(f, "active" | "storefront"),
        }
    }
    /// Form rows: the current items, then blanks up to [`ROWS`] (+1 spare).
    fn item_rows(&self) -> Vec<(usize, String, String)> {
        let current: Vec<(String, String)> = self
            .editing
            .as_ref()
            .map(|(_, items)| {
                items
                    .iter()
                    .map(|i| {
                        (
                            i.product_id.to_string(),
                            i.billing_cycle.as_str().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let n = (current.len() + 1).clamp(ROWS, MAX_ITEMS);
        (0..n)
            .map(|i| {
                let (p, c) = current
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), "monthly".into()));
                (i + 1, p, c)
            })
            .collect()
    }
    fn product_options(&self) -> Vec<(String, String)> {
        self.products
            .iter()
            .map(|(id, name)| (id.to_string(), name.clone()))
            .collect()
    }
    fn cycle_options(&self) -> Vec<(String, String)> {
        BillingCycle::ALL
            .iter()
            .map(|c| (c.as_str().to_owned(), self.cycle(c)))
            .collect()
    }
    fn item_names(&self, items: &[BundleItemRow]) -> String {
        items
            .iter()
            .map(|i| format!("{} ({})", i.product_name, self.cycle(&i.billing_cycle)))
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn percent(&self, b: &BundleRow) -> String {
        format!("%{}", b.discount_value.normalize())
    }
}

async fn with_items(
    state: &AppState,
    tenant: &Tenant,
    b: BundleRow,
) -> AppResult<(BundleRow, Vec<BundleItemRow>)> {
    let items = brepo::items(&state.pool, tenant.id, b.id).await?;
    Ok((b, items))
}

async fn render_list(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    editing: Option<BundleRow>,
) -> AppResult<Response> {
    let mut rows = Vec::new();
    for b in brepo::list(&state.pool, tenant.id).await? {
        rows.push(with_items(state, tenant, b).await?);
    }
    let editing = match editing {
        Some(b) => Some(with_items(state, tenant, b).await?),
        None => None,
    };
    let title = ctx.t("bundles.admin.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin.catalog.title", "/admin/catalog")],
    );
    render(&ListPage {
        products: product_choices(state, tenant).await?,
        rows,
        editing,
        ctx,
    })
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    render_list(&state, &tenant, &staff, ctx, None).await
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = brepo::get(&state.pool, tenant.id, BundleId::from(id))
        .await?
        .ok_or(AppError::NotFound)?;
    render_list(&state, &tenant, &staff, ctx, Some(row)).await
}

/// Validated input: a slug, a 0–100 % discount, two or more distinct
/// products of this tenant (an unknown product id is a 404).
async fn input(
    state: &AppState,
    tenant: &Tenant,
    f: &FormData,
) -> AppResult<Result<BundleInput, &'static str>> {
    let name = f.text("name", 120);
    if name.is_empty() {
        return Ok(Err("bundles.admin.name_required"));
    }
    let slug = f.text("slug", 120).to_ascii_lowercase();
    if !valid_slug(&slug) {
        return Ok(Err("bundles.admin.bad_slug"));
    }
    let value = match f.decimal("discount_value") {
        Ok(Some(v)) if v > Decimal::ZERO && v <= Decimal::ONE_HUNDRED => v,
        _ => return Ok(Err("bundles.admin.bad_discount")),
    };
    let Ok(sort) = f.int("sort_order") else {
        return Ok(Err("bundles.admin.invalid"));
    };
    let mut items: Vec<(ProductId, BillingCycle)> = Vec::new();
    for n in 1..=MAX_ITEMS {
        let raw = f.text(&format!("item_product_{n}"), 40);
        if raw.is_empty() {
            continue;
        }
        let Ok(id) = Uuid::parse_str(&raw).map(ProductId::from) else {
            return Ok(Err("bundles.admin.invalid"));
        };
        hos_db::repo::catalog::get_product(&state.pool, tenant.id, id)
            .await?
            .ok_or(AppError::NotFound)?;
        let Some(cycle) = f.parse::<BillingCycle>(&format!("item_cycle_{n}")) else {
            return Ok(Err("bundles.admin.invalid"));
        };
        if items.iter().any(|(p, _)| *p == id) {
            return Ok(Err("bundles.admin.duplicate_product"));
        }
        items.push((id, cycle));
    }
    if items.len() < 2 {
        return Ok(Err("bundles.admin.two_products"));
    }
    Ok(Ok(BundleInput {
        slug,
        name,
        description: f.opt("description", 1000),
        is_active: f.flag("is_active"),
        show_on_storefront: f.flag("show_on_storefront"),
        sort_order: i32::try_from(sort.unwrap_or(0)).unwrap_or(0),
        discount_type: DiscountType::Percent,
        discount_value: value,
        recurring: f.flag("recurring"),
        items,
    }))
}

fn audit_data(b: &BundleInput) -> serde_json::Value {
    json!({
        "slug": b.slug,
        "name": b.name,
        "discount_percent": b.discount_value,
        "recurring": b.recurring,
        "active": b.is_active,
        "products": b.items.iter().map(|(p, c)| json!({"product": p, "cycle": c})).collect::<Vec<_>>(),
    })
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let b = match input(&state, &tenant, &f).await? {
        Ok(b) => b,
        Err(key) => return Ok(failed(&ctx, BACK, ctx.t(key))),
    };
    let mut tx = state.pool.begin().await?;
    let id = match brepo::create(&mut tx, tenant.id, &b, &bundle_code()).await {
        Ok(id) => id,
        Err(e) if e.http_status() == 409 => {
            return Ok(failed(&ctx, BACK, ctx.t("bundles.admin.slug_taken")));
        }
        Err(e) => return Err(e.into()),
    };
    tx.commit().await?;
    storefront_changed(&state);
    au.log(
        &state,
        "bundle.create",
        Some(("bundle", id.into_uuid())),
        audit_data(&b),
    )
    .await?;
    Ok(done(&ctx, BACK, "bundles.admin.saved"))
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let id = BundleId::from(id);
    brepo::get(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let back = format!("{BACK}/{id}");
    let b = match input(&state, &tenant, &f).await? {
        Ok(b) => b,
        Err(key) => return Ok(failed(&ctx, &back, ctx.t(key))),
    };
    let mut tx = state.pool.begin().await?;
    match brepo::update(&mut tx, tenant.id, id, &b).await {
        Ok(()) => {}
        Err(e) if e.http_status() == 409 => {
            return Ok(failed(&ctx, &back, ctx.t("bundles.admin.slug_taken")));
        }
        Err(e) => return Err(e.into()),
    }
    tx.commit().await?;
    storefront_changed(&state);
    au.log(
        &state,
        "bundle.update",
        Some(("bundle", id.into_uuid())),
        audit_data(&b),
    )
    .await?;
    Ok(done(&ctx, BACK, "bundles.admin.saved"))
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = BundleId::from(id);
    let mut tx = state.pool.begin().await?;
    if !brepo::delete(&mut tx, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    tx.commit().await?;
    storefront_changed(&state);
    au.log(
        &state,
        "bundle.delete",
        Some(("bundle", id.into_uuid())),
        json!({}),
    )
    .await?;
    Ok(done(&ctx, BACK, "bundles.admin.deleted"))
}

#[cfg(test)]
mod tests {
    use super::bundle_code;

    #[test]
    fn bundle_codes_are_random() {
        let a = bundle_code();
        let b = bundle_code();
        assert_ne!(a, b);
        assert_eq!(a.len(), 4 + 16);
        assert!(a.starts_with("BND-"));
        assert!(a[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
