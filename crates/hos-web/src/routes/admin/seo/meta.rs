//! `/admin/seo/meta*` — hand-written `seo_meta` of the storefront home page,
//! product groups and products (tr / en side by side, live SERP preview),
//! without touching the catalog screens. Content pages edit their metadata
//! in the content editor.

use std::collections::BTreeSet;

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use hos_core::domain::Locale;
use hos_core::ids::{ProductGroupId, ProductId};
use hos_db::Page;
use hos_db::enums::SeoEntityType;
use hos_db::repo::catalog::{self, ProductFilter, translation};
use hos_db::repo::seo::{self as seo_repo, SeoTarget};
use serde_json::json;
use uuid::Uuid;

use super::super::common::{self, Audit, FormData};
use super::super::content::{MetaFields, locales, save_meta, title_suffix};
use super::nav;
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::forms::FormErrors;
use crate::locale::localize_path;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/seo/meta", get(list))
        .route("/admin/seo/meta/home", get(edit_home).post(save_home))
        .route("/admin/seo/meta/{kind}/{id}", get(edit).post(save))
}

/// A row of the metadata list.
pub struct MetaRow {
    pub kind: &'static str,
    pub name: String,
    pub sub: String,
    pub href: String,
    pub tr: bool,
    pub en: bool,
    pub hidden: bool,
}

/// `(entity id, locale)` pairs with a hand-written description.
async fn described(
    state: &AppState,
    tenant: &Tenant,
    t: SeoEntityType,
) -> AppResult<BTreeSet<(Option<Uuid>, Locale)>> {
    Ok(seo_repo::list_seo_meta(&state.pool, tenant.id, Some(t))
        .await?
        .into_iter()
        .filter(|m| {
            m.description
                .as_deref()
                .is_some_and(|d| !d.trim().is_empty())
                || m.title.as_deref().is_some_and(|d| !d.trim().is_empty())
        })
        .map(|m| (m.entity_id, m.locale))
        .collect())
}

/// Visible products / groups without metadata in the default language.
pub async fn missing_count(state: &AppState, tenant: &Tenant) -> AppResult<i64> {
    let l = tenant.default_locale;
    let groups = described(state, tenant, SeoEntityType::ProductGroup).await?;
    let products = described(state, tenant, SeoEntityType::Product).await?;
    let mut n = 0i64;
    for g in catalog::list_product_groups(&state.pool, tenant.id, false).await? {
        if !groups.contains(&(Some(g.id.into()), l)) {
            n += 1;
        }
    }
    for p in catalog::list_products(
        &state.pool,
        tenant.id,
        &ProductFilter::default(),
        Page::first(100),
    )
    .await?
    .items
    {
        if !p.is_hidden && !products.contains(&(Some(p.id.into()), l)) {
            n += 1;
        }
    }
    Ok(n)
}

#[derive(Template)]
#[template(path = "admin/seo/meta_list.html")]
struct ListPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<MetaRow>,
}
crate::page!(ListPage);

impl ListPage {
    fn kind_label(&self, kind: &str) -> String {
        self.t(&format!("admin_content.seo.meta.kinds.{kind}"))
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_content.seo.meta.title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/meta");
    let home = described(&state, &tenant, SeoEntityType::Home).await?;
    let groups_meta = described(&state, &tenant, SeoEntityType::ProductGroup).await?;
    let products_meta = described(&state, &tenant, SeoEntityType::Product).await?;
    let mut rows = vec![MetaRow {
        kind: "home",
        name: ctx.t("admin_content.seo.meta.home"),
        sub: "/".into(),
        href: "/admin/seo/meta/home".into(),
        tr: home.contains(&(None, Locale::Tr)),
        en: home.contains(&(None, Locale::En)),
        hidden: false,
    }];
    let groups = catalog::list_product_groups(&state.pool, tenant.id, true).await?;
    for g in &groups {
        let id: Uuid = g.id.into();
        rows.push(MetaRow {
            kind: "product_group",
            name: g.name.clone(),
            sub: format!("/{}", g.slug),
            href: format!("/admin/seo/meta/product_group/{id}"),
            tr: groups_meta.contains(&(Some(id), Locale::Tr)),
            en: groups_meta.contains(&(Some(id), Locale::En)),
            hidden: g.is_hidden,
        });
    }
    for p in catalog::list_products(
        &state.pool,
        tenant.id,
        &ProductFilter::default(),
        Page::first(100),
    )
    .await?
    .items
    {
        let id: Uuid = p.id.into();
        let group = groups.iter().find(|g| Some(g.id) == p.group_id);
        rows.push(MetaRow {
            kind: "product",
            name: p.name.clone(),
            sub: group.map_or_else(String::new, |g| format!("/{}/{}", g.slug, p.slug)),
            href: format!("/admin/seo/meta/product/{id}"),
            tr: products_meta.contains(&(Some(id), Locale::Tr)),
            en: products_meta.contains(&(Some(id), Locale::En)),
            hidden: p.is_hidden || group.is_none_or(|g| g.is_hidden),
        });
    }
    render(&ListPage {
        tabs,
        active,
        rows,
        ctx,
    })
}

/// One language column of the editor.
#[derive(Debug, Clone, Default)]
pub struct MetaVersion {
    pub locale: String,
    pub label: String,
    pub meta: MetaFields,
    pub default_title: String,
    pub default_desc: String,
    pub url: String,
}

#[derive(Template)]
#[template(path = "admin/seo/meta_edit.html")]
struct EditPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    heading: String,
    kind_label: String,
    action: String,
    versions: Vec<MetaVersion>,
    errors: FormErrors,
    suffix: String,
    ai_entity: String,
    ai_ready: bool,
}
crate::page!(EditPage);

impl EditPage {
    fn err(&self, v: &MetaVersion, field: &str) -> &str {
        self.errors.get(&format!("{}_{field}", v.locale))
    }
}

/// What is being edited.
struct Target {
    seo: SeoTarget,
    entity: Option<(&'static str, Uuid)>,
    name: String,
    kind_key: &'static str,
    /// `(locale) → (default title, default description, unprefixed path)`.
    defaults: Vec<(Locale, String, String, String)>,
    action: String,
}

async fn target(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    kind: &str,
    id: Option<Uuid>,
) -> AppResult<Target> {
    let default = tenant.default_locale;
    let tr_or = |json: &serde_json::Value, l: Locale, field: &str, base: &str| -> String {
        if l == default {
            base.to_owned()
        } else {
            translation(json, l.as_str(), field)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(base)
                .to_owned()
        }
    };
    match (kind, id) {
        ("home", _) => Ok(Target {
            seo: SeoTarget::Home,
            entity: None,
            name: ctx.t("admin_content.seo.meta.home"),
            kind_key: "home",
            defaults: locales(default)
                .iter()
                .map(|l| {
                    (
                        *l,
                        tenant.display_name().to_owned(),
                        String::new(),
                        "/".to_owned(),
                    )
                })
                .collect(),
            action: "/admin/seo/meta/home".into(),
        }),
        ("product_group", Some(id)) => {
            let g = catalog::list_product_groups(&state.pool, tenant.id, true)
                .await?
                .into_iter()
                .find(|g| g.id == ProductGroupId::from(id))
                .ok_or(AppError::NotFound)?;
            Ok(Target {
                seo: SeoTarget::Entity(SeoEntityType::ProductGroup, id),
                entity: Some(("product_group", id)),
                name: g.name.clone(),
                kind_key: "product_group",
                defaults: locales(default)
                    .iter()
                    .map(|l| {
                        (
                            *l,
                            tr_or(&g.translations, *l, "name", &g.name),
                            tr_or(
                                &g.translations,
                                *l,
                                "tagline",
                                g.tagline.as_deref().unwrap_or(""),
                            ),
                            format!("/{}", g.slug),
                        )
                    })
                    .collect(),
                action: format!("/admin/seo/meta/product_group/{id}"),
            })
        }
        ("product", Some(id)) => {
            let p = catalog::get_product(&state.pool, tenant.id, ProductId::from(id))
                .await?
                .ok_or(AppError::NotFound)?;
            let group = match p.group_id {
                Some(gid) => catalog::list_product_groups(&state.pool, tenant.id, true)
                    .await?
                    .into_iter()
                    .find(|g| g.id == gid),
                None => None,
            };
            Ok(Target {
                seo: SeoTarget::Entity(SeoEntityType::Product, id),
                entity: Some(("product", id)),
                name: p.name.clone(),
                kind_key: "product",
                defaults: locales(default)
                    .iter()
                    .map(|l| {
                        let name = tr_or(&p.translations, *l, "name", &p.name);
                        let title = match &group {
                            Some(g) => {
                                format!("{name} — {}", tr_or(&g.translations, *l, "name", &g.name))
                            }
                            None => name,
                        };
                        (
                            *l,
                            title,
                            tr_or(
                                &p.translations,
                                *l,
                                "tagline",
                                p.tagline.as_deref().unwrap_or(""),
                            ),
                            group
                                .as_ref()
                                .map_or_else(String::new, |g| format!("/{}/{}", g.slug, p.slug)),
                        )
                    })
                    .collect(),
                action: format!("/admin/seo/meta/product/{id}"),
            })
        }
        _ => Err(AppError::NotFound),
    }
}

async fn editor(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    t: &Target,
    metas: Option<Vec<(Locale, MetaFields)>>,
    errors: FormErrors,
) -> AppResult<EditPage> {
    let kind_label = ctx.t(&format!("admin_content.seo.meta.kinds.{}", t.kind_key));
    common::page(
        &mut ctx,
        staff,
        &t.name,
        &[
            ("admin_content.seo.title", "/admin/seo"),
            ("admin_content.seo.meta.title", "/admin/seo/meta"),
        ],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/meta");
    let mut versions = Vec::new();
    for (l, title, desc, path) in &t.defaults {
        let meta = match &metas {
            Some(m) => m
                .iter()
                .find(|(x, _)| x == l)
                .map(|(_, f)| f.clone())
                .unwrap_or_default(),
            None => MetaFields::from_row(
                seo_repo::get_seo_meta(&state.pool, tenant.id, &t.seo, *l)
                    .await?
                    .as_ref(),
            ),
        };
        versions.push(MetaVersion {
            locale: l.as_str().into(),
            label: ctx.t(&format!("admin.common.lang_{}", l.as_str())),
            meta,
            default_title: title.clone(),
            default_desc: desc.clone(),
            url: if path.is_empty() {
                String::new()
            } else {
                format!(
                    "{}{}",
                    ctx.origin,
                    localize_path(path, *l, tenant.default_locale)
                )
            },
        });
    }
    let ai_ready = super::super::ai_studio::provider_ready(state, tenant).await;
    Ok(EditPage {
        tabs,
        active,
        heading: t.name.clone(),
        kind_label,
        action: t.action.clone(),
        versions,
        errors,
        suffix: title_suffix(state, tenant).await?,
        ai_entity: t
            .entity
            .map(|(k, id)| format!("{k}:{id}"))
            .unwrap_or_default(),
        ai_ready,
        ctx,
    })
}

async fn show(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    ctx: PageCtx,
    kind: &str,
    id: Option<Uuid>,
) -> AppResult<Response> {
    let t = target(state, tenant, &ctx, kind, id).await?;
    let page = editor(state, tenant, staff, ctx, &t, None, FormErrors::default()).await?;
    render(&page)
}

#[allow(clippy::too_many_arguments)]
async fn store(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    au: Audit,
    ctx: PageCtx,
    kind: &str,
    id: Option<Uuid>,
    f: &FormData,
) -> AppResult<Response> {
    let t = target(state, tenant, &ctx, kind, id).await?;
    let mut errors = FormErrors::default();
    let mut metas = Vec::new();
    for l in locales(tenant.default_locale) {
        let prefix = format!("{}_", l.as_str());
        let m = MetaFields::read(f, &prefix);
        m.validate(&ctx, &mut errors, &prefix);
        metas.push((l, m));
    }
    if errors.any() {
        let page = editor(state, tenant, staff, ctx, &t, Some(metas), errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    for (l, m) in &metas {
        save_meta(state, tenant, &t.seo, *l, m).await?;
    }
    au.log(
        state,
        "seo.meta",
        t.entity,
        json!({"kind": t.kind_key, "titles": metas.iter().map(|(l, m)| (l.as_str(), m.title.clone())).collect::<Vec<_>>()}),
    )
    .await?;
    Ok(common::done(&ctx, &t.action, "admin.common.saved"))
}

async fn edit_home(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    show(&state, &tenant, &staff, ctx, "home", None).await
}

async fn save_home(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    store(&state, &tenant, &staff, au, ctx, "home", None, &f).await
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    Path((kind, id)): Path<(String, Uuid)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if kind == "home" {
        return Err(AppError::NotFound);
    }
    show(&state, &tenant, &staff, ctx, &kind, Some(id)).await
}

#[allow(clippy::too_many_arguments)]
async fn save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    Path((kind, id)): Path<(String, Uuid)>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    if kind == "home" {
        return Err(AppError::NotFound);
    }
    store(&state, &tenant, &staff, au, ctx, &kind, Some(id), &f).await
}
