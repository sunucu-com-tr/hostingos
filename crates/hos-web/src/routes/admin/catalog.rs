//! Catalog: product groups (`/admin/catalog/groups`) and products
//! (`/admin/catalog`, `/admin/catalog/products/*`): details with a Markdown
//! description + second-language copy, the currency × cycle price matrix,
//! the provisioning module (integration + settings, with packages /
//! templates fetched live from the server), option groups and stock.

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::domain::{IntegrationCategory, Locale, ProductType};
use hos_core::ids::{ConfigOptionGroupId, IntegrationId, ProductGroupId, ProductId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::Page;
use hos_db::enums::{AutoSetup, DomainRequirement, PayType};
use hos_db::repo::admin::{self as arepo, ConfigGroupRow};
use hos_db::repo::catalog::{
    self, CurrencyRow, ProductFilter, ProductGroupInput, ProductGroupRow, ProductInput, ProductRow,
};
use hos_db::repo::integrations::{self, IntegrationRow};
use hos_db::repo::mail;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, PageQuery, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/catalog", get(list))
        .route("/admin/catalog/groups", get(groups).post(group_create))
        .route("/admin/catalog/groups/{id}", post(group_update))
        .route("/admin/catalog/groups/{id}/delete", post(group_delete))
        .route("/admin/catalog/products", post(create))
        .route("/admin/catalog/products/new", get(new_form))
        .route("/admin/catalog/products/{id}", get(edit).post(update))
        .route("/admin/catalog/products/{id}/pricing", post(save_pricing))
        .route("/admin/catalog/products/{id}/module", post(save_module))
        .route("/admin/catalog/products/{id}/remote", get(remote_items))
        .route("/admin/catalog/products/{id}/extras", post(save_extras))
        .route("/admin/catalog/products/{id}/delete", post(delete))
}

/// Tells the storefront to rebuild its cached navigation.
pub(super) fn storefront_changed(state: &AppState) {
    state.store_chrome.invalidate_all();
}

fn other_locale(default: Locale) -> Locale {
    if default == Locale::Tr {
        Locale::En
    } else {
        Locale::Tr
    }
}

fn slug_ok(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 100
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Integration category that provisions a product type.
pub(super) fn module_category(t: ProductType) -> Option<IntegrationCategory> {
    match t {
        ProductType::SharedHosting | ProductType::ResellerHosting | ProductType::Email => {
            Some(IntegrationCategory::Panel)
        }
        ProductType::Vps | ProductType::Dedicated | ProductType::Cloud => {
            Some(IntegrationCategory::Virt)
        }
        ProductType::Domain => Some(IntegrationCategory::Registrar),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Product list
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    group: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    retired: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/catalog/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ProductRow>,
    pager: Pager,
    q: String,
    group: String,
    kind: String,
    retired: bool,
    groups: Vec<ProductGroupRow>,
    types: Vec<(String, String)>,
}
crate::page!(ListPage);

impl ListPage {
    fn group_name(&self, id: &Option<ProductGroupId>) -> String {
        id.and_then(|g| self.groups.iter().find(|x| x.id == g))
            .map(|g| g.name.clone())
            .unwrap_or_else(|| "—".into())
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let group_id = f
        .group
        .as_deref()
        .and_then(|g| Uuid::parse_str(g).ok())
        .map(ProductGroupId::from);
    let product_type = f.kind.as_deref().and_then(|k| k.parse().ok());
    let retired = f.retired.as_deref() == Some("1");
    let paged = catalog::list_products(
        &state.pool,
        tenant.id,
        &ProductFilter {
            group_id,
            product_type,
            search: q.q.clone(),
            include_retired: retired,
        },
        q.db_page(),
    )
    .await?;
    let groups = catalog::list_product_groups(&state.pool, tenant.id, true).await?;
    let title = ctx.t("admin.catalog.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let (qv, gv, kv) = (
        q.q.clone().unwrap_or_default(),
        f.group.clone().unwrap_or_default(),
        f.kind.clone().unwrap_or_default(),
    );
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/catalog"),
        &[
            ("q", qv.as_str()),
            ("group", gv.as_str()),
            ("type", kv.as_str()),
            ("retired", if retired { "1" } else { "" }),
        ],
    );
    let types = common::enum_options(
        &ctx,
        ProductType::ALL,
        |t| t.as_str(),
        "admin.catalog.types",
    );
    render(&ListPage {
        rows: paged.items,
        pager,
        q: qv,
        group: gv,
        kind: kv,
        retired,
        groups,
        types,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/catalog/groups.html")]
struct GroupsPage {
    ctx: PageCtx,
    groups: Vec<ProductGroupRow>,
    other: String,
}
crate::page!(GroupsPage);

impl GroupsPage {
    fn tr_field(&self, g: &ProductGroupRow, field: &str) -> String {
        catalog::translation(&g.translations, &self.other, field)
            .unwrap_or_default()
            .to_owned()
    }
}

async fn groups(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let groups = catalog::list_product_groups(&state.pool, tenant.id, true).await?;
    let title = ctx.t("admin.catalog.groups_title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.catalog.title", "/admin/catalog")],
    );
    render(&GroupsPage {
        ctx,
        groups,
        other: other_locale(tenant.default_locale).as_str().into(),
    })
}

fn read_group(f: &FormData, other: Locale) -> Result<ProductGroupInput, &'static str> {
    let name = f.text("name", 120);
    if name.is_empty() {
        return Err("admin.common.required");
    }
    let mut slug = f.text("slug", 100).to_lowercase();
    if slug.is_empty() {
        slug = crate::markdown::slugify(&name);
    }
    if !slug_ok(&slug) {
        return Err("admin.catalog.invalid_slug");
    }
    let mut tr = Map::new();
    for field in ["name", "tagline", "description"] {
        if let Some(v) = f.opt(&format!("{field}_tr"), 2000) {
            tr.insert(field.into(), Value::String(v));
        }
    }
    let translations = if tr.is_empty() {
        json!({})
    } else {
        json!({ other.as_str(): tr })
    };
    Ok(ProductGroupInput {
        slug,
        name,
        tagline: f.opt("tagline", 300),
        description: f.opt("description", 4000),
        translations,
        icon: f.opt("icon", 40),
        is_hidden: f.flag("is_hidden"),
        sort_order: f
            .int("sort_order")
            .ok()
            .flatten()
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0),
    })
}

async fn group_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/catalog/groups";
    let input = match read_group(&f, other_locale(tenant.default_locale)) {
        Ok(i) => i,
        Err(k) => return Ok(failed(&ctx, back, ctx.t(k))),
    };
    match catalog::create_product_group(&state.pool, tenant.id, &input).await {
        Ok(g) => {
            au.log(
                &state,
                "product_group.create",
                Some(("product_group", g.id.into())),
                json!({"name": g.name}),
            )
            .await?;
            storefront_changed(&state);
            Ok(done(&ctx, back, "admin.common.created"))
        }
        Err(e) if e.http_status() == 409 => {
            Ok(failed(&ctx, back, ctx.t("admin.catalog.slug_taken")))
        }
        Err(e) => Err(e.into()),
    }
}

async fn group_update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/catalog/groups";
    let input = match read_group(&f, other_locale(tenant.default_locale)) {
        Ok(i) => i,
        Err(k) => return Ok(failed(&ctx, back, ctx.t(k))),
    };
    let id = ProductGroupId::from(id);
    match arepo::update_product_group(&state.pool, tenant.id, id, &input).await {
        Ok(true) => {}
        Ok(false) => return Err(AppError::NotFound),
        Err(e) if e.http_status() == 409 => {
            return Ok(failed(&ctx, back, ctx.t("admin.catalog.slug_taken")));
        }
        Err(e) => return Err(e.into()),
    }
    au.log(
        &state,
        "product_group.update",
        Some(("product_group", id.into())),
        json!({"name": input.name, "hidden": input.is_hidden}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, back, "admin.common.saved"))
}

async fn group_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = ProductGroupId::from(id);
    if !arepo::delete_product_group(&state.pool, tenant.id, id).await? {
        return Ok(failed(
            &ctx,
            "/admin/catalog/groups",
            ctx.t("admin.catalog.group_not_empty"),
        ));
    }
    au.log(
        &state,
        "product_group.delete",
        Some(("product_group", id.into())),
        json!({}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, "/admin/catalog/groups", "admin.common.deleted"))
}

// ---------------------------------------------------------------------------
// Product create
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/catalog/new.html")]
struct NewPage {
    ctx: PageCtx,
    groups: Vec<ProductGroupRow>,
    types: Vec<(String, String)>,
    name_value: String,
    slug: String,
    kind: String,
    group: String,
    error: String,
}
crate::page!(NewPage);

async fn new_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    f: Option<&FormData>,
    error: &str,
) -> AppResult<Response> {
    let groups = catalog::list_product_groups(&state.pool, tenant.id, true).await?;
    let title = ctx.t("admin.catalog.new_title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin.catalog.title", "/admin/catalog")],
    );
    let types = common::enum_options(
        &ctx,
        ProductType::ALL,
        |t| t.as_str(),
        "admin.catalog.types",
    );
    let get = |k: &str| f.map(|f| f.text(k, 200)).unwrap_or_default();
    let page = NewPage {
        groups,
        types,
        name_value: get("name"),
        slug: get("slug"),
        kind: f.map_or_else(|| "shared_hosting".into(), |f| f.text("product_type", 40)),
        group: get("group_id"),
        error: if error.is_empty() {
            String::new()
        } else {
            ctx.t(error)
        },
        ctx,
    };
    if error.is_empty() {
        render(&page)
    } else {
        render_status(StatusCode::UNPROCESSABLE_ENTITY, &page)
    }
}

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    new_page(&state, &tenant, &staff, ctx, None, "").await
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let name = f.text("name", 200);
    let mut slug = f.text("slug", 100).to_lowercase();
    if slug.is_empty() {
        slug = crate::markdown::slugify(&name);
    }
    let Some(product_type) = f.parse::<ProductType>("product_type") else {
        return new_page(
            &state,
            &tenant,
            &staff,
            ctx,
            Some(&f),
            "admin.common.invalid_value",
        )
        .await;
    };
    if name.is_empty() {
        return new_page(
            &state,
            &tenant,
            &staff,
            ctx,
            Some(&f),
            "admin.common.required",
        )
        .await;
    }
    if !slug_ok(&slug) {
        return new_page(
            &state,
            &tenant,
            &staff,
            ctx,
            Some(&f),
            "admin.catalog.invalid_slug",
        )
        .await;
    }
    let group_id = f.id::<ProductGroupId>("group_id");
    if let Some(g) = group_id
        && !catalog::list_product_groups(&state.pool, tenant.id, true)
            .await?
            .iter()
            .any(|x| x.id == g)
    {
        return Err(AppError::NotFound);
    }
    let mut input = ProductInput::new(&slug, &name, product_type, group_id);
    // New products start hidden until priced.
    input.is_hidden = true;
    if module_category(product_type).is_none() {
        input.auto_setup = AutoSetup::Manual;
    }
    let product = match catalog::create_product(&state.pool, tenant.id, &input).await {
        Ok(p) => p,
        Err(e) if e.http_status() == 409 => {
            return new_page(
                &state,
                &tenant,
                &staff,
                ctx,
                Some(&f),
                "admin.catalog.slug_taken",
            )
            .await;
        }
        Err(e) => return Err(e.into()),
    };
    au.log(
        &state,
        "product.create",
        Some(("product", product.id.into())),
        json!({"name": product.name, "type": product.product_type.as_str()}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(
        &ctx,
        &format!("/admin/catalog/products/{}", product.id),
        "admin.catalog.created_hidden",
    ))
}

// ---------------------------------------------------------------------------
// Product edit page
// ---------------------------------------------------------------------------

/// One cell of the price matrix.
pub struct PriceCell {
    pub currency: String,
    pub price: String,
    pub setup: String,
}

/// One row (billing cycle) of the price matrix.
pub struct PriceRowView {
    pub cycle: String,
    pub label: String,
    pub cells: Vec<PriceCell>,
}

/// A module setting input.
pub struct ModuleField {
    pub key: &'static str,
    pub label: String,
    pub value: String,
    pub numeric: bool,
    /// Filled by the "fetch from server" button.
    pub remote: bool,
}

#[derive(Template)]
#[template(path = "admin/catalog/edit.html")]
struct EditPage {
    ctx: PageCtx,
    p: ProductRow,
    groups: Vec<ProductGroupRow>,
    types: Vec<(String, String)>,
    pay_types: Vec<(String, String)>,
    setups: Vec<(String, String)>,
    domain_reqs: Vec<(String, String)>,
    templates: Vec<(String, String)>,
    other_label: String,
    features: String,
    tr_name: String,
    tr_tagline: String,
    tr_description: String,
    tr_features: String,
    currencies: Vec<CurrencyRow>,
    matrix: Vec<PriceRowView>,
    module_integrations: Vec<IntegrationRow>,
    module_category: String,
    module_fields: Vec<ModuleField>,
    config_groups: Vec<ConfigGroupRow>,
    attached: Vec<ConfigOptionGroupId>,
    in_use: bool,
    storefront_url: String,
}
crate::page!(EditPage);

impl EditPage {
    fn attached_group(&self, id: &ConfigOptionGroupId) -> bool {
        self.attached.contains(id)
    }
    fn group_sel(&self, id: &ProductGroupId) -> &'static str {
        if self.p.group_id == Some(*id) {
            " selected"
        } else {
            ""
        }
    }
    fn integration_sel(&self, id: &IntegrationId) -> &'static str {
        if self.p.integration_id == Some(*id) {
            " selected"
        } else {
            ""
        }
    }
}

fn lines(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn to_lines(s: &str) -> Value {
    Value::Array(
        s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(40)
            .map(|l| Value::String(l.chars().take(200).collect()))
            .collect(),
    )
}

fn setting_text(cfg: &Value, key: &str) -> String {
    match cfg.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Settings `hos-billing` reads from `products.module_config` for a category.
fn module_keys(cat: Option<IntegrationCategory>) -> &'static [(&'static str, bool, bool)] {
    // (key, numeric, remote)
    match cat {
        Some(IntegrationCategory::Panel) => &[("package", false, true)],
        Some(IntegrationCategory::Virt) => &[
            ("template", false, true),
            ("node", false, true),
            ("location", false, false),
            ("plan", false, false),
            ("cpu_cores", true, false),
            ("memory_mb", true, false),
            ("disk_gb", true, false),
            ("storage", false, false),
            ("bandwidth_gb", true, false),
            ("bridge", false, false),
        ],
        _ => &[],
    }
}

async fn load_product(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<ProductRow> {
    catalog::get_product(&state.pool, tenant.id, id.into())
        .await?
        .ok_or(AppError::NotFound)
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let p = load_product(&state, &tenant, id).await?;
    let groups = catalog::list_product_groups(&state.pool, tenant.id, true).await?;
    let currencies = catalog::list_currencies(&state.pool, tenant.id, true).await?;
    let pricing = catalog::product_pricing(&state.pool, tenant.id, p.id).await?;
    let matrix = BillingCycle::ALL
        .iter()
        .map(|cycle| PriceRowView {
            cycle: cycle.as_str().into(),
            label: ctx.cycle(cycle),
            cells: currencies
                .iter()
                .map(|c| {
                    let row = pricing.iter().find(|r| {
                        r.currency == c.code && r.billing_cycle == *cycle && r.is_enabled
                    });
                    PriceCell {
                        currency: c.code.to_string(),
                        price: row
                            .map(|r| r.price.normalize().to_string())
                            .unwrap_or_default(),
                        setup: row
                            .map(|r| r.setup_fee.normalize().to_string())
                            .unwrap_or_default(),
                    }
                })
                .collect(),
        })
        .collect();
    let cat = module_category(p.product_type);
    let module_integrations = match cat {
        Some(c) => integrations::list_integrations(&state.pool, tenant.id, Some(c)).await?,
        None => Vec::new(),
    };
    let module_fields = module_keys(cat)
        .iter()
        .map(|(key, numeric, remote)| ModuleField {
            key,
            label: ctx.t(&format!("admin.catalog.module.{key}")),
            value: setting_text(&p.module_config, key),
            numeric: *numeric,
            remote: *remote,
        })
        .collect();
    let config_groups = arepo::config_groups(&state.pool, tenant.id).await?;
    let attached = arepo::product_config_group_ids(&state.pool, tenant.id, p.id).await?;
    let in_use = arepo::product_in_use(&state.pool, tenant.id, p.id).await?;
    let mut templates: Vec<(String, String)> = mail::list_templates(&state.pool, tenant.id)
        .await?
        .into_iter()
        .filter(|t| t.is_active)
        .map(|t| (t.key.clone(), format!("{} ({})", t.name, t.key)))
        .collect();
    templates.sort();
    templates.dedup_by(|a, b| a.0 == b.0);
    let other = other_locale(tenant.default_locale);
    let tr = |f: &str| {
        catalog::translation(&p.translations, other.as_str(), f)
            .unwrap_or_default()
            .to_owned()
    };
    let title = p.name.clone();
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.catalog.title", "/admin/catalog")],
    );
    let types = common::enum_options(
        &ctx,
        ProductType::ALL,
        |t| t.as_str(),
        "admin.catalog.types",
    );
    let pay_types = common::enum_options(
        &ctx,
        PayType::ALL,
        |t| t.as_str(),
        "admin.catalog.pay_types",
    );
    let setups = common::enum_options(
        &ctx,
        AutoSetup::ALL,
        |t| t.as_str(),
        "admin.catalog.auto_setups",
    );
    let domain_reqs = common::enum_options(
        &ctx,
        DomainRequirement::ALL,
        |t| t.as_str(),
        "admin.catalog.domain_reqs",
    );
    let storefront_url = p
        .group_id
        .and_then(|g| groups.iter().find(|x| x.id == g))
        .map(|g| ctx.url(format!("/{}/{}", g.slug, p.slug)))
        .unwrap_or_default();
    render(&EditPage {
        features: lines(&p.features),
        tr_name: tr("name"),
        tr_tagline: tr("tagline"),
        tr_description: tr("description"),
        tr_features: p
            .translations
            .get(other.as_str())
            .and_then(|t| t.get("features"))
            .map(lines)
            .unwrap_or_default(),
        other_label: ctx.t(&format!("admin.common.lang_{}", other.as_str())),
        module_category: cat.map(|c| c.as_str().to_owned()).unwrap_or_default(),
        p,
        groups,
        types,
        pay_types,
        setups,
        domain_reqs,
        templates,
        currencies,
        matrix,
        module_integrations,
        module_fields,
        config_groups,
        attached,
        in_use,
        storefront_url,
        ctx,
    })
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
    let p = load_product(&state, &tenant, id).await?;
    let back = format!("/admin/catalog/products/{}", p.id);
    let name = f.text("name", 200);
    let slug = f.text("slug", 100).to_lowercase();
    if name.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.required")));
    }
    if !slug_ok(&slug) {
        return Ok(failed(&ctx, &back, ctx.t("admin.catalog.invalid_slug")));
    }
    let (Some(product_type), Some(pay_type), Some(auto_setup), Some(requires_domain)) = (
        f.parse::<ProductType>("product_type"),
        f.parse::<PayType>("pay_type"),
        f.parse::<AutoSetup>("auto_setup"),
        f.parse::<DomainRequirement>("requires_domain"),
    ) else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_value")));
    };
    let group_id = f.id::<ProductGroupId>("group_id");
    if let Some(g) = group_id
        && !catalog::list_product_groups(&state.pool, tenant.id, true)
            .await?
            .iter()
            .any(|x| x.id == g)
    {
        return Err(AppError::NotFound);
    }
    let other = other_locale(tenant.default_locale);
    let mut translations = p.translations.as_object().cloned().unwrap_or_default();
    let mut tr = Map::new();
    for field in ["name", "tagline", "description"] {
        if let Some(v) = f.opt(&format!("tr_{field}"), 20_000) {
            tr.insert(field.into(), Value::String(v));
        }
    }
    let tr_features = to_lines(&f.body("tr_features", 8000));
    if tr_features.as_array().is_some_and(|a| !a.is_empty()) {
        tr.insert("features".into(), tr_features);
    }
    translations.insert(other.as_str().into(), Value::Object(tr));
    let input = ProductInput {
        group_id,
        slug,
        product_type,
        name,
        tagline: f.opt("tagline", 300),
        description: Some(f.body("description", 20_000)).filter(|d| !d.trim().is_empty()),
        features: to_lines(&f.body("features", 8000)),
        translations: Value::Object(translations),
        integration_id: p.integration_id,
        module_config: p.module_config.clone(),
        pay_type,
        auto_setup,
        requires_domain,
        welcome_email_template: f.opt("welcome_email_template", 100),
        is_taxable: f.flag("is_taxable"),
        is_featured: f.flag("is_featured"),
        is_hidden: f.flag("is_hidden"),
        sort_order: f
            .int("sort_order")
            .ok()
            .flatten()
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0),
    };
    match catalog::update_product(&state.pool, tenant.id, p.id, &input).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err(AppError::NotFound),
        Err(e) if e.http_status() == 409 => {
            return Ok(failed(&ctx, &back, ctx.t("admin.catalog.slug_taken")));
        }
        Err(e) => return Err(e.into()),
    }
    au.log(
        &state,
        "product.update",
        Some(("product", p.id.into())),
        json!({"name": input.name, "hidden": input.is_hidden, "featured": input.is_featured}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.common.saved"))
}

async fn save_pricing(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let p = load_product(&state, &tenant, id).await?;
    let back = format!("/admin/catalog/products/{}#pricing", p.id);
    let currencies = catalog::list_currencies(&state.pool, tenant.id, true).await?;
    // Validate everything first, then write in one transaction.
    let mut set: Vec<(
        Currency,
        BillingCycle,
        rust_decimal::Decimal,
        rust_decimal::Decimal,
    )> = Vec::new();
    let mut clear: Vec<(Currency, BillingCycle)> = Vec::new();
    for c in &currencies {
        for cycle in BillingCycle::ALL {
            let pk = format!("price_{}_{}", c.code, cycle.as_str());
            let sk = format!("setup_{}_{}", c.code, cycle.as_str());
            let (Ok(price), Ok(setup)) = (f.decimal(&pk), f.decimal(&sk)) else {
                return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
            };
            match price {
                Some(pr) if pr >= rust_decimal::Decimal::ZERO => {
                    let setup = setup.unwrap_or_default();
                    if setup < rust_decimal::Decimal::ZERO {
                        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
                    }
                    set.push((c.code, *cycle, pr, setup));
                }
                Some(_) => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount"))),
                None => clear.push((c.code, *cycle)),
            }
        }
    }
    let mut tx = state.pool.begin().await?;
    for (cur, cycle, price, setup) in &set {
        catalog::set_product_price(&mut *tx, tenant.id, p.id, *cur, *cycle, *price, *setup).await?;
    }
    for (cur, cycle) in &clear {
        catalog::disable_product_price(&mut *tx, tenant.id, p.id, *cur, *cycle).await?;
    }
    tx.commit().await?;
    let summary: BTreeMap<String, String> = set
        .iter()
        .map(|(c, cy, pr, _)| (format!("{c}/{}", cy.as_str()), pr.to_string()))
        .collect();
    au.log(
        &state,
        "product.pricing",
        Some(("product", p.id.into())),
        json!({ "prices": summary }),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.catalog.pricing_saved"))
}

async fn save_module(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let p = load_product(&state, &tenant, id).await?;
    let back = format!("/admin/catalog/products/{}#module", p.id);
    let cat = module_category(p.product_type);
    let integration_id = f.id::<IntegrationId>("integration_id");
    if let Some(iid) = integration_id {
        let row = integrations::get_integration(&state.pool, tenant.id, iid)
            .await?
            .ok_or(AppError::NotFound)?;
        if Some(row.category) != cat {
            return Ok(failed(&ctx, &back, ctx.t("admin.catalog.module_mismatch")));
        }
    }
    let mut cfg = p.module_config.as_object().cloned().unwrap_or_default();
    for (key, numeric, _) in module_keys(cat) {
        let raw = f.text(key, 200);
        if raw.is_empty() {
            cfg.remove(*key);
        } else if *numeric {
            let Ok(n) = raw.parse::<u64>() else {
                return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_value")));
            };
            cfg.insert((*key).into(), json!(n));
        } else {
            cfg.insert((*key).into(), json!(raw));
        }
    }
    let mut input = product_input(&p);
    input.integration_id = integration_id;
    input.module_config = Value::Object(cfg);
    if let Some(a) = f.parse::<AutoSetup>("auto_setup") {
        input.auto_setup = a;
    }
    catalog::update_product(&state.pool, tenant.id, p.id, &input).await?;
    au.log(
        &state,
        "product.module",
        Some(("product", p.id.into())),
        json!({"integration": integration_id, "config": input.module_config, "auto_setup": input.auto_setup.as_str()}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.common.saved"))
}

/// `ProductInput` with the stored values (for partial updates).
fn product_input(p: &ProductRow) -> ProductInput {
    ProductInput {
        group_id: p.group_id,
        slug: p.slug.clone(),
        product_type: p.product_type,
        name: p.name.clone(),
        tagline: p.tagline.clone(),
        description: p.description.clone(),
        features: p.features.clone(),
        translations: p.translations.clone(),
        integration_id: p.integration_id,
        module_config: p.module_config.clone(),
        pay_type: p.pay_type,
        auto_setup: p.auto_setup,
        requires_domain: p.requires_domain,
        welcome_email_template: p.welcome_email_template.clone(),
        is_taxable: p.is_taxable,
        is_featured: p.is_featured,
        is_hidden: p.is_hidden,
        sort_order: p.sort_order,
    }
}

#[derive(Debug, Deserialize)]
struct RemoteQuery {
    integration_id: Option<String>,
}

/// One remote choice (package / template / node).
pub struct RemoteChoice {
    pub value: String,
    pub label: String,
}

#[derive(Template)]
#[template(path = "admin/catalog/_remote.html")]
struct RemoteFragment {
    ctx: PageCtx,
    fields: Vec<(ModuleField, Vec<RemoteChoice>)>,
    error: String,
}
crate::page!(RemoteFragment);

/// htmx: packages (panel) or templates + nodes (virt) from the server.
async fn remote_items(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    Path(id): Path<Uuid>,
    Query(rq): Query<RemoteQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let p = load_product(&state, &tenant, id).await?;
    let iid = rq
        .integration_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .map(IntegrationId::from)
        .or(p.integration_id);
    let field = |key: &'static str| ModuleField {
        key,
        label: ctx.t(&format!("admin.catalog.module.{key}")),
        value: setting_text(&p.module_config, key),
        numeric: false,
        remote: true,
    };
    let mut fields = Vec::new();
    let mut error = String::new();
    match (iid, module_category(p.product_type)) {
        (None, _) => error = ctx.t("admin.catalog.pick_integration"),
        (Some(iid), Some(IntegrationCategory::Panel)) => {
            let reg = state.billing.registry();
            match reg.panel(tenant.id, iid).await {
                Ok(panel) => match panel.list_packages().await {
                    Ok(list) => fields.push((
                        field("package"),
                        list.into_iter()
                            .map(|pk| RemoteChoice {
                                label: if pk.name == pk.id {
                                    pk.name
                                } else {
                                    format!("{} ({})", pk.name, pk.id)
                                },
                                value: pk.id,
                            })
                            .collect(),
                    )),
                    Err(e) => error = ctx.tv("admin.common.failed", "reason", e),
                },
                Err(e) => error = ctx.tv("admin.common.failed", "reason", e),
            }
        }
        (Some(iid), Some(IntegrationCategory::Virt)) => {
            let reg = state.billing.registry();
            match reg.virt(tenant.id, iid).await {
                Ok(virt) => {
                    let node = setting_text(&p.module_config, "node");
                    let node = Some(node.as_str()).filter(|n| !n.is_empty());
                    match virt.list_templates(node).await {
                        Ok(list) => fields.push((
                            field("template"),
                            list.into_iter()
                                .map(|t| RemoteChoice {
                                    label: match &t.node {
                                        Some(n) => format!("{} · {n}", t.name),
                                        None => t.name.clone(),
                                    },
                                    value: t.id,
                                })
                                .collect(),
                        )),
                        Err(e) => error = ctx.tv("admin.common.failed", "reason", e),
                    }
                    if let Ok(nodes) = virt.list_nodes().await {
                        fields.push((
                            field("node"),
                            nodes
                                .into_iter()
                                .map(|n| RemoteChoice {
                                    label: if n.online {
                                        n.name.clone()
                                    } else {
                                        format!("{} (offline)", n.name)
                                    },
                                    value: n.id,
                                })
                                .collect(),
                        ));
                    }
                }
                Err(e) => error = ctx.tv("admin.common.failed", "reason", e),
            }
        }
        _ => error = ctx.t("admin.catalog.no_remote"),
    }
    render(&RemoteFragment { ctx, fields, error })
}

async fn save_extras(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let p = load_product(&state, &tenant, id).await?;
    let back = format!("/admin/catalog/products/{}#extras", p.id);
    let qty = match f.int("stock_quantity") {
        Ok(Some(n)) if n >= 0 => i32::try_from(n).unwrap_or(i32::MAX),
        Ok(None) => 0,
        _ => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_value"))),
    };
    let known: Vec<ConfigOptionGroupId> = arepo::config_groups(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|g| g.id)
        .collect();
    let groups: Vec<ConfigOptionGroupId> = f
        .ids::<ConfigOptionGroupId>("config_groups")
        .into_iter()
        .filter(|g| known.contains(g))
        .collect();
    arepo::set_product_extras(
        &state.pool,
        tenant.id,
        p.id,
        f.flag("stock_control"),
        qty,
        f.flag("allow_quantity"),
        f.flag("is_retired"),
    )
    .await?;
    arepo::set_product_config_groups(&state.pool, tenant.id, p.id, &groups).await?;
    au.log(
        &state,
        "product.extras",
        Some(("product", p.id.into())),
        json!({"stock_control": f.flag("stock_control"), "stock": qty, "retired": f.flag("is_retired"), "config_groups": groups}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.common.saved"))
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let p = load_product(&state, &tenant, id).await?;
    if !arepo::delete_product(&state.pool, tenant.id, p.id).await? {
        return Ok(failed(
            &ctx,
            &format!("/admin/catalog/products/{}", p.id),
            ctx.t("admin.catalog.in_use"),
        ));
    }
    au.log(
        &state,
        "product.delete",
        Some(("product", p.id.into())),
        json!({"name": p.name}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, "/admin/catalog", "admin.common.deleted"))
}

/// Products for pickers (id, name), at most 200.
pub(super) async fn product_choices(
    state: &AppState,
    tenant: &Tenant,
) -> AppResult<Vec<(ProductId, String)>> {
    Ok(catalog::list_products(
        &state.pool,
        tenant.id,
        &ProductFilter::default(),
        Page::first(200),
    )
    .await?
    .items
    .into_iter()
    .map(|p| (p.id, p.name))
    .collect())
}
