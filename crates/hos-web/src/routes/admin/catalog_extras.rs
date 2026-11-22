//! Catalog extras: configurable option groups (`/admin/catalog/options`),
//! addons (`/admin/catalog/addons`) and TLDs + prices
//! (`/admin/catalog/tlds`: bulk price grid, registrar sync, margin rule).

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_billing::settings::PriceMargin;
use hos_core::domain::IntegrationCategory;
use hos_core::ids::{
    ConfigOptionGroupId, ConfigOptionId, IntegrationId, ProductAddonId, ProductId, TldId,
};
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::{ConfigOptionType, PayType, TldAction};
use hos_db::ids::ConfigOptionValueId;
use hos_db::repo::admin::{self as arepo, ConfigGroupRow, OptionValueRow, PriceRow};
use hos_db::repo::catalog::{self, CurrencyRow, ProductAddonRow};
use hos_db::repo::domains::{self, TldInput, TldPriceInput, TldRow};
use hos_db::repo::integrations::{self, IntegrationRow};
use hos_db::repo::platform;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::catalog::{product_choices, storefront_changed};
use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/catalog/options", get(groups).post(group_create))
        .route("/admin/catalog/options/{gid}", get(group_show))
        .route("/admin/catalog/options/{gid}/delete", post(group_delete))
        .route("/admin/catalog/options/{gid}/options", post(option_create))
        .route(
            "/admin/catalog/options/{gid}/options/{oid}/delete",
            post(option_delete),
        )
        .route(
            "/admin/catalog/options/{gid}/options/{oid}/values",
            post(value_create),
        )
        .route(
            "/admin/catalog/options/{gid}/values/{vid}/prices",
            post(value_prices),
        )
        .route(
            "/admin/catalog/options/{gid}/values/{vid}/delete",
            post(value_delete),
        )
        .route("/admin/catalog/addons", get(addons).post(addon_create))
        .route(
            "/admin/catalog/addons/{id}",
            get(addon_edit).post(addon_update),
        )
        .route("/admin/catalog/addons/{id}/pricing", post(addon_pricing))
        .route("/admin/catalog/addons/{id}/delete", post(addon_delete))
        .route("/admin/catalog/tlds", get(tlds).post(tld_create))
        .route("/admin/catalog/tlds/prices", post(tld_prices))
        .route("/admin/catalog/tlds/sync", post(tld_sync))
        .route("/admin/catalog/tlds/margin", post(tld_margin))
        .route("/admin/catalog/tlds/bulk", post(tld_bulk))
}

const RECURRING: [BillingCycle; 6] = [
    BillingCycle::Monthly,
    BillingCycle::Quarterly,
    BillingCycle::Semiannual,
    BillingCycle::Annual,
    BillingCycle::Biennial,
    BillingCycle::Triennial,
];

fn int_field(f: &FormData, key: &str) -> i32 {
    f.int(key)
        .ok()
        .flatten()
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Configurable option groups
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/catalog/options.html")]
struct GroupsPage {
    ctx: PageCtx,
    groups: Vec<ConfigGroupRow>,
}
crate::page!(GroupsPage);

async fn groups(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let groups = arepo::config_groups(&state.pool, tenant.id).await?;
    let title = ctx.t("admin.options.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.catalog.title", "/admin/catalog")],
    );
    render(&GroupsPage { ctx, groups })
}

async fn group_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let name = f.text("name", 120);
    if name.is_empty() {
        return Ok(failed(
            &ctx,
            "/admin/catalog/options",
            ctx.t("admin.common.required"),
        ));
    }
    let id = catalog::create_config_option_group(
        &state.pool,
        tenant.id,
        &name,
        f.opt("description", 500).as_deref(),
    )
    .await?;
    au.log(
        &state,
        "config_group.create",
        Some(("config_group", id.into())),
        json!({"name": name}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/catalog/options/{id}"),
        "admin.common.created",
    ))
}

/// A value with its per-cycle prices (default currency).
pub struct ValueView {
    pub id: ConfigOptionValueId,
    pub label: String,
    pub value: String,
    pub prices: Vec<(String, String, String)>,
}

/// An option with its values.
pub struct OptionView {
    pub id: ConfigOptionId,
    pub name: String,
    pub option_type: String,
    pub module_key: String,
    pub values: Vec<ValueView>,
}

#[derive(Template)]
#[template(path = "admin/catalog/option_group.html")]
struct GroupPage {
    ctx: PageCtx,
    group: ConfigGroupRow,
    options: Vec<OptionView>,
    currency: Currency,
    cycles: Vec<(String, String)>,
    types: Vec<(String, String)>,
}
crate::page!(GroupPage);

async fn load_group(state: &AppState, tenant: &Tenant, gid: Uuid) -> AppResult<ConfigGroupRow> {
    let gid = ConfigOptionGroupId::from(gid);
    arepo::config_groups(&state.pool, tenant.id)
        .await?
        .into_iter()
        .find(|g| g.id == gid)
        .ok_or(AppError::NotFound)
}

async fn group_values(
    state: &AppState,
    tenant: &Tenant,
    gid: ConfigOptionGroupId,
) -> AppResult<Vec<OptionValueRow>> {
    Ok(arepo::group_option_values(&state.pool, tenant.id, gid, tenant.default_currency).await?)
}

async fn group_show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    Path(gid): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let group = load_group(&state, &tenant, gid).await?;
    let currency = tenant.default_currency;
    let flat = group_values(&state, &tenant, group.id).await?;
    let prices = arepo::group_value_prices(&state.pool, tenant.id, group.id, currency).await?;
    let mut options: Vec<OptionView> = Vec::new();
    for r in flat {
        if options.last().is_none_or(|o| o.id != r.option_id) {
            options.push(OptionView {
                id: r.option_id,
                name: r.option_name.clone(),
                option_type: r.option_type.as_str().into(),
                module_key: r.module_key.clone().unwrap_or_default(),
                values: Vec::new(),
            });
        }
        if let (Some(vid), Some(o)) = (r.value_id, options.last_mut()) {
            o.values.push(ValueView {
                id: vid,
                label: r.label.clone().unwrap_or_default(),
                value: r.value.clone().unwrap_or_default(),
                prices: RECURRING
                    .iter()
                    .map(|c| {
                        let p = prices
                            .iter()
                            .find(|p| p.value_id == vid && p.billing_cycle == *c);
                        (
                            c.as_str().to_owned(),
                            p.map(|p| p.price.normalize().to_string())
                                .unwrap_or_default(),
                            p.map(|p| p.setup_fee.normalize().to_string())
                                .unwrap_or_default(),
                        )
                    })
                    .collect(),
            });
        }
    }
    let title = group.name.clone();
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[
            ("admin.catalog.title", "/admin/catalog"),
            ("admin.options.title", "/admin/catalog/options"),
        ],
    );
    let cycles = RECURRING
        .iter()
        .map(|c| (c.as_str().to_owned(), ctx.cycle(c)))
        .collect();
    let types = common::enum_options(
        &ctx,
        ConfigOptionType::ALL,
        |t| t.as_str(),
        "admin.options.types",
    );
    render(&GroupPage {
        ctx,
        group,
        options,
        currency,
        cycles,
        types,
    })
}

async fn group_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(gid): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let group = load_group(&state, &tenant, gid).await?;
    arepo::delete_config_group(&state.pool, tenant.id, group.id).await?;
    au.log(
        &state,
        "config_group.delete",
        Some(("config_group", group.id.into())),
        json!({"name": group.name}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, "/admin/catalog/options", "admin.common.deleted"))
}

async fn option_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(gid): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let group = load_group(&state, &tenant, gid).await?;
    let back = format!("/admin/catalog/options/{}", group.id);
    let name = f.text("name", 120);
    let Some(kind) = f.parse::<ConfigOptionType>("option_type") else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_value")));
    };
    if name.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.required")));
    }
    let module_key = f.opt("module_key", 60);
    let id = catalog::create_config_option(
        &state.pool,
        tenant.id,
        group.id,
        &name,
        kind,
        module_key.as_deref(),
        &json!({}),
        int_field(&f, "sort_order"),
    )
    .await?;
    au.log(
        &state,
        "config_option.create",
        Some(("config_group", group.id.into())),
        json!({"option": id, "name": name, "type": kind.as_str()}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.common.created"))
}

async fn option_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path((gid, oid)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let group = load_group(&state, &tenant, gid).await?;
    let oid = ConfigOptionId::from(oid);
    if arepo::config_option_group(&state.pool, tenant.id, oid).await? != Some(group.id) {
        return Err(AppError::NotFound);
    }
    arepo::delete_config_option(&state.pool, tenant.id, oid).await?;
    au.log(
        &state,
        "config_option.delete",
        Some(("config_group", group.id.into())),
        json!({"option": oid}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(
        &ctx,
        &format!("/admin/catalog/options/{}", group.id),
        "admin.common.deleted",
    ))
}

/// Price + setup fee per recurring cycle (`None`: not offered).
type CyclePrices = Vec<(BillingCycle, Option<(Decimal, Decimal)>)>;

/// Reads `price_<cycle>` / `setup_<cycle>`; `Err` on an invalid amount.
#[allow(clippy::result_unit_err)]
fn read_cycle_prices(f: &FormData) -> Result<CyclePrices, ()> {
    let mut out = Vec::new();
    for c in RECURRING {
        let price = f.decimal(&format!("price_{}", c.as_str()))?;
        let setup = f.decimal(&format!("setup_{}", c.as_str()))?;
        match price {
            Some(p) if p >= Decimal::ZERO => {
                let s = setup.unwrap_or_default();
                if s < Decimal::ZERO {
                    return Err(());
                }
                out.push((c, Some((p, s))));
            }
            Some(_) => return Err(()),
            None => out.push((c, None)),
        }
    }
    Ok(out)
}

async fn store_value_prices(
    state: &AppState,
    tenant: &Tenant,
    vid: ConfigOptionValueId,
    prices: &CyclePrices,
) -> AppResult<()> {
    let mut tx = state.pool.begin().await?;
    for (cycle, p) in prices {
        match p {
            Some((price, setup)) => {
                catalog::set_config_option_price(
                    &mut *tx,
                    tenant.id,
                    vid,
                    tenant.default_currency,
                    *cycle,
                    *price,
                    *setup,
                )
                .await?;
            }
            None => {
                arepo::delete_config_value_price(
                    &mut *tx,
                    tenant.id,
                    vid,
                    tenant.default_currency,
                    *cycle,
                )
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn value_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path((gid, oid)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let group = load_group(&state, &tenant, gid).await?;
    let back = format!("/admin/catalog/options/{}", group.id);
    let oid = ConfigOptionId::from(oid);
    if arepo::config_option_group(&state.pool, tenant.id, oid).await? != Some(group.id) {
        return Err(AppError::NotFound);
    }
    let label = f.text("label", 120);
    if label.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.required")));
    }
    let value = f.opt("value", 200).unwrap_or_else(|| label.clone());
    let Ok(prices) = read_cycle_prices(&f) else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
    };
    let vid = catalog::create_config_option_value(
        &state.pool,
        tenant.id,
        oid,
        &label,
        &value,
        &json!({}),
        int_field(&f, "sort_order"),
    )
    .await?;
    store_value_prices(&state, &tenant, vid, &prices).await?;
    au.log(
        &state,
        "config_value.create",
        Some(("config_group", group.id.into())),
        json!({"option": oid, "value": vid, "label": label}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.common.created"))
}

async fn owned_value(
    state: &AppState,
    tenant: &Tenant,
    group: ConfigOptionGroupId,
    vid: Uuid,
) -> AppResult<ConfigOptionValueId> {
    let vid = ConfigOptionValueId::from(vid);
    let found = group_values(state, tenant, group)
        .await?
        .iter()
        .any(|r| r.value_id == Some(vid));
    if found {
        Ok(vid)
    } else {
        Err(AppError::NotFound)
    }
}

async fn value_prices(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path((gid, vid)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let group = load_group(&state, &tenant, gid).await?;
    let back = format!("/admin/catalog/options/{}", group.id);
    let vid = owned_value(&state, &tenant, group.id, vid).await?;
    let Ok(prices) = read_cycle_prices(&f) else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
    };
    store_value_prices(&state, &tenant, vid, &prices).await?;
    au.log(
        &state,
        "config_value.pricing",
        Some(("config_group", group.id.into())),
        json!({"value": vid}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.common.saved"))
}

async fn value_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path((gid, vid)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let group = load_group(&state, &tenant, gid).await?;
    let vid = owned_value(&state, &tenant, group.id, vid).await?;
    arepo::delete_config_value(&state.pool, tenant.id, vid).await?;
    au.log(
        &state,
        "config_value.delete",
        Some(("config_group", group.id.into())),
        json!({"value": vid}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(
        &ctx,
        &format!("/admin/catalog/options/{}", group.id),
        "admin.common.deleted",
    ))
}

// ---------------------------------------------------------------------------
// Addons
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/catalog/addons.html")]
struct AddonsPage {
    ctx: PageCtx,
    rows: Vec<ProductAddonRow>,
    pay_types: Vec<(String, String)>,
}
crate::page!(AddonsPage);

async fn addons(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = arepo::list_addons(&state.pool, tenant.id).await?;
    let title = ctx.t("admin.addons.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.catalog.title", "/admin/catalog")],
    );
    let pay_types = common::enum_options(
        &ctx,
        PayType::ALL,
        |t| t.as_str(),
        "admin.catalog.pay_types",
    );
    render(&AddonsPage {
        ctx,
        rows,
        pay_types,
    })
}

async fn addon_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/catalog/addons";
    let name = f.text("name", 120);
    let pay_type = f.parse::<PayType>("pay_type").unwrap_or(PayType::Recurring);
    if name.is_empty() {
        return Ok(failed(&ctx, back, ctx.t("admin.common.required")));
    }
    let slug = crate::markdown::slugify(&name);
    match catalog::create_addon(
        &state.pool,
        tenant.id,
        &slug,
        &name,
        f.opt("description", 2000).as_deref(),
        &json!({}),
        pay_type,
    )
    .await
    {
        Ok(a) => {
            au.log(
                &state,
                "addon.create",
                Some(("addon", a.id.into())),
                json!({"name": a.name}),
            )
            .await?;
            Ok(done(
                &ctx,
                &format!("/admin/catalog/addons/{}", a.id),
                "admin.common.created",
            ))
        }
        Err(e) if e.http_status() == 409 => {
            Ok(failed(&ctx, back, ctx.t("admin.catalog.slug_taken")))
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "admin/catalog/addon.html")]
struct AddonPage {
    ctx: PageCtx,
    a: ProductAddonRow,
    products: Vec<(ProductId, String)>,
    attached: Vec<ProductId>,
    currencies: Vec<CurrencyRow>,
    prices: Vec<PriceRow>,
    cycles: Vec<(String, String)>,
    templates: Vec<(String, String)>,
}
crate::page!(AddonPage);

impl AddonPage {
    fn has_product(&self, id: &ProductId) -> bool {
        self.attached.contains(id)
    }
    fn price(&self, cur: &Currency, cycle: &str) -> String {
        self.prices
            .iter()
            .find(|p| p.currency == *cur && p.billing_cycle.as_str() == cycle)
            .map(|p| p.price.normalize().to_string())
            .unwrap_or_default()
    }
    fn setup(&self, cur: &Currency, cycle: &str) -> String {
        self.prices
            .iter()
            .find(|p| p.currency == *cur && p.billing_cycle.as_str() == cycle)
            .map(|p| p.setup_fee.normalize().to_string())
            .unwrap_or_default()
    }
}

async fn load_addon(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<ProductAddonRow> {
    catalog::get_addon(&state.pool, tenant.id, ProductAddonId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

async fn addon_edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let a = load_addon(&state, &tenant, id).await?;
    let products = product_choices(&state, &tenant).await?;
    let attached = arepo::addon_product_ids(&state.pool, tenant.id, a.id).await?;
    let currencies = catalog::list_currencies(&state.pool, tenant.id, true).await?;
    let prices = arepo::addon_pricing(&state.pool, tenant.id, a.id).await?;
    let mut templates: Vec<(String, String)> =
        hos_db::repo::mail::list_templates(&state.pool, tenant.id)
            .await?
            .into_iter()
            .map(|t| (t.key.clone(), format!("{} ({})", t.name, t.key)))
            .collect();
    templates.sort();
    templates.dedup_by(|x, y| x.0 == y.0);
    let title = a.name.clone();
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[
            ("admin.catalog.title", "/admin/catalog"),
            ("admin.addons.title", "/admin/catalog/addons"),
        ],
    );
    let cycles = if a.pay_type == PayType::Recurring {
        RECURRING
            .iter()
            .map(|c| (c.as_str().to_owned(), ctx.cycle(c)))
            .collect()
    } else {
        vec![(
            BillingCycle::OneTime.as_str().to_owned(),
            ctx.cycle(&BillingCycle::OneTime),
        )]
    };
    render(&AddonPage {
        ctx,
        a,
        products,
        attached,
        currencies,
        prices,
        cycles,
        templates,
    })
}

async fn addon_update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let a = load_addon(&state, &tenant, id).await?;
    let back = format!("/admin/catalog/addons/{}", a.id);
    let name = f.text("name", 120);
    if name.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.required")));
    }
    arepo::update_addon(
        &state.pool,
        tenant.id,
        a.id,
        &name,
        f.opt("description", 2000).as_deref(),
        f.flag("show_on_order"),
        f.flag("auto_activate"),
        f.flag("is_hidden"),
        f.opt("welcome_email_template", 100).as_deref(),
    )
    .await?;
    let known: Vec<ProductId> = product_choices(&state, &tenant)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let products: Vec<ProductId> = f
        .ids::<ProductId>("product_ids")
        .into_iter()
        .filter(|p| known.contains(p))
        .collect();
    arepo::set_addon_products(&state.pool, tenant.id, a.id, &products).await?;
    au.log(
        &state,
        "addon.update",
        Some(("addon", a.id.into())),
        json!({"name": name, "products": products.len()}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.common.saved"))
}

async fn addon_pricing(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let a = load_addon(&state, &tenant, id).await?;
    let back = format!("/admin/catalog/addons/{}#pricing", a.id);
    let currencies = catalog::list_currencies(&state.pool, tenant.id, true).await?;
    let mut set = Vec::new();
    let mut clear = Vec::new();
    for c in &currencies {
        for cycle in BillingCycle::ALL {
            let pk = format!("price_{}_{}", c.code, cycle.as_str());
            if !f.has(&pk) {
                continue;
            }
            let sk = format!("setup_{}_{}", c.code, cycle.as_str());
            let (Ok(price), Ok(setup)) = (f.decimal(&pk), f.decimal(&sk)) else {
                return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
            };
            match price {
                Some(p) if p >= Decimal::ZERO && setup.unwrap_or_default() >= Decimal::ZERO => {
                    set.push((c.code, *cycle, p, setup.unwrap_or_default()));
                }
                Some(_) => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount"))),
                None => clear.push((c.code, *cycle)),
            }
        }
    }
    let mut tx = state.pool.begin().await?;
    for (cur, cycle, p, s) in &set {
        catalog::set_addon_price(&mut *tx, tenant.id, a.id, *cur, *cycle, *p, *s).await?;
    }
    for (cur, cycle) in &clear {
        arepo::delete_addon_price(&mut *tx, tenant.id, a.id, *cur, *cycle).await?;
    }
    tx.commit().await?;
    au.log(
        &state,
        "addon.pricing",
        Some(("addon", a.id.into())),
        json!({"prices": set.len()}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, &back, "admin.catalog.pricing_saved"))
}

async fn addon_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let a = load_addon(&state, &tenant, id).await?;
    if !arepo::delete_addon(&state.pool, tenant.id, a.id).await? {
        return Ok(failed(
            &ctx,
            &format!("/admin/catalog/addons/{}", a.id),
            ctx.t("admin.catalog.in_use"),
        ));
    }
    au.log(
        &state,
        "addon.delete",
        Some(("addon", a.id.into())),
        json!({"name": a.name}),
    )
    .await?;
    storefront_changed(&state);
    Ok(done(&ctx, "/admin/catalog/addons", "admin.common.deleted"))
}

// ---------------------------------------------------------------------------
// TLDs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct TldQuery {
    currency: Option<String>,
}

/// One TLD row of the price grid.
pub struct TldView {
    pub tld: TldRow,
    pub registrar: String,
    pub register: String,
    pub transfer: String,
    pub renew: String,
}

#[derive(Template)]
#[template(path = "admin/catalog/tlds.html")]
struct TldsPage {
    ctx: PageCtx,
    rows: Vec<TldView>,
    currency: Currency,
    currencies: Vec<CurrencyRow>,
    registrars: Vec<IntegrationRow>,
    margin_percent: String,
    margin_fixed: String,
}
crate::page!(TldsPage);

async fn tlds(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ProductsManage>,
    Query(tq): Query<TldQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let currencies = catalog::list_currencies(&state.pool, tenant.id, true).await?;
    let currency = tq
        .currency
        .as_deref()
        .and_then(|c| c.parse::<Currency>().ok())
        .filter(|c| currencies.iter().any(|x| x.code == *c))
        .unwrap_or(tenant.default_currency);
    let list = domains::list_tlds(&state.pool, tenant.id, false).await?;
    let cells = arepo::tld_one_year_prices(&state.pool, tenant.id, currency).await?;
    let registrars = integrations::list_integrations(
        &state.pool,
        tenant.id,
        Some(IntegrationCategory::Registrar),
    )
    .await?;
    let price = |id: TldId, action: TldAction| {
        cells
            .iter()
            .find(|c| c.tld_id == id && c.action == action && c.years == 1)
            .map(|c| c.price.normalize().to_string())
            .unwrap_or_default()
    };
    let rows = list
        .into_iter()
        .map(|t| TldView {
            registrar: t
                .registrar_integration_id
                .and_then(|r| registrars.iter().find(|i| i.id == r))
                .map(|i| i.name.clone())
                .unwrap_or_default(),
            register: price(t.id, TldAction::Register),
            transfer: price(t.id, TldAction::Transfer),
            renew: price(t.id, TldAction::Renew),
            tld: t,
        })
        .collect();
    let settings = hos_billing::BillingSettings::load(&state.pool, tenant.id).await?;
    let title = ctx.t("admin.tlds.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.catalog.title", "/admin/catalog")],
    );
    render(&TldsPage {
        ctx,
        rows,
        currency,
        currencies,
        registrars,
        margin_percent: settings.price_margin.percent.normalize().to_string(),
        margin_fixed: settings.price_margin.fixed.normalize().to_string(),
    })
}

async fn tld_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/catalog/tlds";
    let raw = f.text("tld", 63).to_lowercase();
    let tld = format!(".{}", raw.trim_start_matches('.'));
    let valid = tld.len() > 1
        && tld[1..]
            .split('.')
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    if !valid {
        return Ok(failed(&ctx, back, ctx.t("admin.tlds.invalid")));
    }
    let registrar = f.id::<IntegrationId>("registrar_integration_id");
    if let Some(r) = registrar
        && integrations::get_integration(&state.pool, tenant.id, r)
            .await?
            .is_none_or(|i| i.category != IntegrationCategory::Registrar)
    {
        return Err(AppError::NotFound);
    }
    let mut input = TldInput::new(&tld);
    input.registrar_integration_id = registrar;
    input.requires_epp = f.flag("requires_epp");
    input.supports_privacy = f.flag("supports_privacy");
    if let Ok(Some(n)) = f.int("max_years")
        && (1..=10).contains(&n)
    {
        input.max_years = i16::try_from(n).unwrap_or(10);
    }
    let row = domains::upsert_tld(&state.pool, tenant.id, &input).await?;
    au.log(
        &state,
        "tld.save",
        Some(("tld", row.id.into())),
        json!({"tld": row.tld}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.common.saved"))
}

async fn tld_prices(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let Some(currency) = f.parse::<Currency>("currency") else {
        return Err(AppError::bad_request("currency"));
    };
    let back = format!("/admin/catalog/tlds?currency={currency}");
    let list = domains::list_tlds(&state.pool, tenant.id, false).await?;
    let mut updates: Vec<(TldId, Vec<TldPriceInput>)> = Vec::new();
    for t in &list {
        let mut prices = Vec::new();
        for action in [TldAction::Register, TldAction::Transfer, TldAction::Renew] {
            let key = format!("{}_{}", action.as_str(), t.id);
            match f.decimal(&key) {
                Ok(Some(p)) if p >= Decimal::ZERO => prices.push(TldPriceInput {
                    currency,
                    action,
                    years: 1,
                    price: p,
                }),
                Ok(None) => {}
                _ => return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount"))),
            }
        }
        if !prices.is_empty() {
            updates.push((t.id, prices));
        }
    }
    let mut tx = state.pool.begin().await?;
    for (id, prices) in &updates {
        domains::set_tld_prices(&mut *tx, tenant.id, *id, prices).await?;
    }
    tx.commit().await?;
    au.log(
        &state,
        "tld.prices",
        None,
        json!({"currency": currency, "tlds": updates.len()}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.catalog.pricing_saved"))
}

async fn tld_bulk(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/catalog/tlds";
    let ids: Vec<TldId> = f.ids("ids");
    let active = match f.text("action", 20).as_str() {
        "activate" => true,
        "deactivate" => false,
        _ => return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_value"))),
    };
    if ids.is_empty() {
        return Ok(failed(&ctx, back, ctx.t("admin.common.nothing_selected")));
    }
    let mut n = 0;
    for id in &ids {
        if arepo::set_tld_active(&state.pool, tenant.id, *id, active).await? {
            n += 1;
        }
    }
    au.log(
        &state,
        "tld.bulk",
        None,
        json!({"active": active, "count": n}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.common.saved"))
}

async fn tld_sync(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
) -> AppResult<Response> {
    let back = "/admin/catalog/tlds";
    match state.billing.sync_tld_pricing(tenant.id).await {
        Ok(r) => {
            au.log(
                &state,
                "tld.sync",
                None,
                json!({"checked": r.checked, "updated": r.updated, "errors": r.errors}),
            )
            .await?;
            let text = ctx
                .t("admin.tlds.synced")
                .replace("{checked}", &r.checked.to_string())
                .replace("{updated}", &r.updated.to_string())
                .replace("{errors}", &r.errors.to_string());
            Ok(crate::flash::redirect(
                &ctx.url(back),
                if r.errors > 0 {
                    crate::flash::SetFlash::error(text)
                } else {
                    crate::flash::SetFlash::success(text)
                },
            ))
        }
        Err(e) => Ok(failed(&ctx, back, common::failure_text(&ctx, &e)?)),
    }
}

async fn tld_margin(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ProductsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/catalog/tlds";
    let (Ok(Some(percent)), Ok(fixed)) = (f.decimal("percent"), f.decimal("fixed")) else {
        return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_amount")));
    };
    let fixed = fixed.unwrap_or_default();
    if percent < Decimal::ZERO || percent > Decimal::from(1000) || fixed < Decimal::ZERO {
        return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_amount")));
    }
    let margin = PriceMargin { percent, fixed };
    let value = json!({"percent": margin.percent, "fixed": margin.fixed});
    platform::set_setting(&state.pool, tenant.id, "domains.price_margin", &value).await?;
    au.log(
        &state,
        "settings.update",
        None,
        json!({"domains.price_margin": value}),
    )
    .await?;
    Ok(done(&ctx, back, "admin.common.saved"))
}
