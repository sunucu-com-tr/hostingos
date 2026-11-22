//! Infrastructure: integrations by category (`/admin/integrations`) with a
//! form generated from `IntegrationRegistry::descriptors()` — secrets are
//! write-only (blank keeps the stored value, never echoed), htmx connection
//! test, default / active switches, webhook + OAuth callback URLs;
//! virtualization inventory (live nodes / templates / VMs, link a VM to a
//! service) and IP pools (`/admin/ip-pools`).

use std::net::IpAddr;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use hos_billing::registry::{IntegrationRegistry, SaveIntegration};
use hos_core::domain::IntegrationCategory;
use hos_core::ids::{IntegrationId, IpAddressId, IpPoolId, ServiceId};
use hos_core::integrations::virt::{NodeInfo, TemplateInfo, VmInfo, VmStatus};
use hos_core::integrations::{FieldKind, IntegrationDescriptor};
use hos_core::{CoreError, IntegrationError};
use hos_db::enums::IpStatus;
use hos_db::repo::admin as arepo;
use hos_db::repo::integrations::{self, IntegrationRow};
use hos_db::repo::services::{self, IpAddressRow, IpPoolUsage, VmInput, VmRow};
use ipnet::IpNet;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/integrations", get(list).post(create))
        .route("/admin/integrations/new", get(new_form))
        .route("/admin/integrations/{id}", get(edit).post(update))
        .route("/admin/integrations/{id}/test", post(test))
        .route("/admin/integrations/{id}/default", post(make_default))
        .route("/admin/integrations/{id}/delete", post(delete))
        .route("/admin/integrations/{id}/inventory", get(inventory))
        .route("/admin/integrations/{id}/link-vm", post(link_vm))
        .route("/admin/ip-pools", get(pools).post(pool_create))
        .route("/admin/ip-pools/{id}", get(pool_show))
        .route("/admin/ip-pools/{id}/delete", post(pool_delete))
        .route("/admin/ip-pools/{id}/addresses/{aid}", post(address_status))
}

const CATEGORIES: [IntegrationCategory; 8] = [
    IntegrationCategory::Panel,
    IntegrationCategory::Virt,
    IntegrationCategory::Registrar,
    IntegrationCategory::Gateway,
    IntegrationCategory::Einvoice,
    IntegrationCategory::Mail,
    IntegrationCategory::Ai,
    IntegrationCategory::Oauth,
];

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// One category section of the list.
pub struct CategoryView {
    pub key: String,
    pub label: String,
    pub hint: String,
    pub rows: Vec<IntegrationRow>,
    pub kinds: Vec<(String, String)>,
}

#[derive(Template)]
#[template(path = "admin/integrations/list.html")]
struct ListPage {
    ctx: PageCtx,
    sections: Vec<CategoryView>,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let all = integrations::list_integrations(&state.pool, tenant.id, None).await?;
    let descriptors = IntegrationRegistry::descriptors();
    // The `mock` providers are offered only where they can be saved.
    let mocks_allowed = state.billing.registry().mock_integrations_allowed();
    let sections = CATEGORIES
        .iter()
        .map(|c| CategoryView {
            key: c.as_str().to_owned(),
            label: ctx.t(&format!("admin.integrations.categories.{}", c.as_str())),
            hint: ctx.t(&format!("admin.integrations.hints.{}", c.as_str())),
            rows: all.iter().filter(|r| r.category == *c).cloned().collect(),
            kinds: descriptors
                .iter()
                .filter(|d| d.category == *c && (d.kind != "mock" || mocks_allowed))
                .map(|d| (d.kind.to_owned(), d.display_name.to_owned()))
                .collect(),
        })
        .collect();
    let title = ctx.t("admin.integrations.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&ListPage { ctx, sections })
}

// ---------------------------------------------------------------------------
// Form
// ---------------------------------------------------------------------------

/// One generated form field.
pub struct FormField {
    pub name: String,
    pub label: String,
    pub kind: &'static str,
    pub required: bool,
    pub help: String,
    pub placeholder: String,
    pub value: String,
    pub checked: bool,
    pub options: Vec<(String, String)>,
    /// Secret field that already has a stored value.
    pub stored: bool,
}

fn field_kind(k: &FieldKind) -> &'static str {
    match k {
        FieldKind::Text => "text",
        FieldKind::TextArea => "textarea",
        FieldKind::Password => "password",
        FieldKind::Number => "number",
        FieldKind::Bool => "bool",
        FieldKind::Select(_) => "select",
        FieldKind::Url => "url",
    }
}

fn value_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(v @ (Value::Array(_) | Value::Object(_))) => {
            serde_json::to_string_pretty(v).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn build_fields(
    d: &IntegrationDescriptor,
    config: &Value,
    stored_secrets: &[String],
) -> Vec<FormField> {
    let mut out = Vec::new();
    for (secret, spec) in d
        .config_fields
        .iter()
        .map(|f| (false, f))
        .chain(d.secret_fields.iter().map(|f| (true, f)))
    {
        let current = if secret {
            None
        } else {
            config.get(spec.key).or(spec.default.as_ref())
        };
        let kind = if secret {
            "password"
        } else {
            field_kind(&spec.kind)
        };
        out.push(FormField {
            name: format!("{}{}", if secret { "s_" } else { "c_" }, spec.key),
            label: spec.label.to_owned(),
            kind,
            required: spec.required,
            help: spec.help.unwrap_or_default().to_owned(),
            placeholder: spec.placeholder.unwrap_or_default().to_owned(),
            value: value_text(current),
            checked: current.and_then(Value::as_bool).unwrap_or(false),
            options: match &spec.kind {
                FieldKind::Select(opts) => opts
                    .iter()
                    .map(|o| (o.value.to_owned(), o.label.to_owned()))
                    .collect(),
                _ => Vec::new(),
            },
            stored: secret && stored_secrets.iter().any(|k| k == spec.key),
        });
    }
    out
}

/// Translated label / help of the form fields: `integrations.<kind>.<key>.
/// label|help` from `locales/*/integrations.json` when present, otherwise
/// the provider descriptor's (English) text.
fn localize_fields(ctx: &PageCtx, kind: &str, fields: &mut [FormField]) {
    for f in fields {
        let key = f
            .name
            .strip_prefix("c_")
            .or_else(|| f.name.strip_prefix("s_"))
            .unwrap_or(&f.name);
        let base = format!("integrations.{kind}.{key}");
        let label = format!("{base}.label");
        if ctx.cat.has(&label) {
            f.label = ctx.t(&label);
        }
        let help = format!("{base}.help");
        if ctx.cat.has(&help) {
            f.help = ctx.t(&help);
        }
    }
}

/// Reads the generated fields back into `config` / `secrets` objects.
fn read_fields(d: &IntegrationDescriptor, f: &FormData) -> Result<(Value, Value), String> {
    let mut config = Map::new();
    for spec in &d.config_fields {
        let name = format!("c_{}", spec.key);
        let v = match &spec.kind {
            FieldKind::Bool => Value::Bool(f.flag(&name)),
            FieldKind::Number => {
                let raw = f.text(&name, 40);
                if raw.is_empty() {
                    continue;
                }
                if let Ok(i) = raw.parse::<i64>() {
                    json!(i)
                } else if let Ok(x) = raw.replace(',', ".").parse::<f64>() {
                    json!(x)
                } else {
                    return Err(spec.label.to_owned());
                }
            }
            FieldKind::TextArea => Value::String(f.body(&name, 20_000)),
            _ => Value::String(f.text(&name, 2000)),
        };
        config.insert(spec.key.to_owned(), v);
    }
    let mut secrets = Map::new();
    for spec in &d.secret_fields {
        let raw = f.raw(&format!("s_{}", spec.key));
        secrets.insert(
            spec.key.to_owned(),
            Value::String(raw.chars().take(20_000).collect()),
        );
    }
    Ok((Value::Object(config), Value::Object(secrets)))
}

/// Keys of the stored (encrypted) secrets, never their values.
fn stored_secret_keys(state: &AppState, row: &IntegrationRow) -> Vec<String> {
    state
        .billing
        .registry()
        .decrypt_secrets(row)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .map(|m| {
            m.into_iter()
                .filter(|(_, v)| v.as_str().is_none_or(|s| !s.is_empty()))
                .map(|(k, _)| k)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "admin/integrations/form.html")]
struct FormPage {
    ctx: PageCtx,
    row: Option<IntegrationRow>,
    category: String,
    category_label: String,
    kind: String,
    display_name: String,
    description: String,
    docs_url: String,
    name_value: String,
    is_active: bool,
    sort_order: i32,
    fields: Vec<FormField>,
    webhook_url: String,
    callback_url: String,
    error: String,
    linkable: bool,
}
crate::page!(FormPage);

/// `?category=&kind=` of the "add" picker. Both are optional so that a
/// picker submitted without a provider (e.g. an empty select) lands back on
/// the list instead of a 400.
#[derive(Debug, Deserialize)]
struct NewQuery {
    category: Option<String>,
    kind: Option<String>,
}

fn descriptor(category: &str, kind: &str) -> AppResult<IntegrationDescriptor> {
    let cat: IntegrationCategory = category.parse().map_err(|_| AppError::NotFound)?;
    IntegrationRegistry::descriptor(cat, kind).ok_or(AppError::NotFound)
}

#[allow(clippy::too_many_arguments)]
async fn form_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    mut ctx: PageCtx,
    d: &IntegrationDescriptor,
    row: Option<IntegrationRow>,
    fields: Vec<FormField>,
    name: String,
    is_active: bool,
    error: String,
) -> AppResult<Response> {
    let mut fields = fields;
    localize_fields(&ctx, d.kind, &mut fields);
    let title = match &row {
        Some(r) => r.name.clone(),
        None => ctx.tv("admin.integrations.new_title", "name", d.display_name),
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin.integrations.title", "/admin/integrations")],
    );
    let base = state
        .billing
        .tenant_url(tenant.id)
        .await
        .unwrap_or_else(|_| ctx.origin.clone());
    let webhook_url = match (&row, d.category) {
        (Some(r), IntegrationCategory::Gateway) => format!("{base}/webhooks/gateway/{}", r.id),
        (Some(r), IntegrationCategory::Registrar) => format!("{base}/webhooks/registrar/{}", r.id),
        _ => String::new(),
    };
    let callback_url = if d.category == IntegrationCategory::Oauth {
        format!("{}/auth/oauth/{}/callback", ctx.origin, d.kind)
    } else {
        String::new()
    };
    let page = FormPage {
        category: d.category.as_str().to_owned(),
        category_label: ctx.t(&format!(
            "admin.integrations.categories.{}",
            d.category.as_str()
        )),
        kind: d.kind.to_owned(),
        display_name: d.display_name.to_owned(),
        description: d.description.unwrap_or_default().to_owned(),
        docs_url: d.docs_url.unwrap_or_default().to_owned(),
        sort_order: row.as_ref().map_or(0, |r| r.sort_order),
        linkable: d.category == IntegrationCategory::Virt && row.is_some(),
        row,
        name_value: name,
        is_active,
        fields,
        webhook_url,
        callback_url,
        error: error.clone(),
        ctx,
    };
    if error.is_empty() {
        render(&page)
    } else {
        crate::view::render_status(axum::http::StatusCode::UNPROCESSABLE_ENTITY, &page)
    }
}

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    Query(nq): Query<NewQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let category = nq
        .category
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .and_then(|c| c.parse::<IntegrationCategory>().ok());
    let kind = nq.kind.as_deref().map(str::trim).filter(|k| !k.is_empty());
    let (Some(category), Some(kind)) = (category, kind) else {
        // No provider chosen: back to the list (at the category's picker).
        let anchor = category.map(|c| format!("#cat-{}", c.as_str()));
        return Ok(axum::response::Redirect::to(&format!(
            "{}{}",
            ctx.url("/admin/integrations"),
            anchor.unwrap_or_default()
        ))
        .into_response());
    };
    let d = descriptor(category.as_str(), kind)?;
    let fields = build_fields(&d, &json!({}), &[]);
    let name = d.display_name.to_owned();
    form_page(
        &state,
        &tenant,
        &staff,
        ctx,
        &d,
        None,
        fields,
        name,
        true,
        String::new(),
    )
    .await
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let d = descriptor(&f.text("category", 20), &f.text("kind", 40))?;
    save(&state, &tenant, &staff, &au, ctx, &d, None, &f).await
}

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<IntegrationRow> {
    integrations::get_integration(&state.pool, tenant.id, IntegrationId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let d = descriptor(row.category.as_str(), &row.kind)?;
    let stored = stored_secret_keys(&state, &row);
    let fields = build_fields(&d, &row.config, &stored);
    let (name, active) = (row.name.clone(), row.is_active);
    form_page(
        &state,
        &tenant,
        &staff,
        ctx,
        &d,
        Some(row),
        fields,
        name,
        active,
        String::new(),
    )
    .await
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let d = descriptor(row.category.as_str(), &row.kind)?;
    save(&state, &tenant, &staff, &au, ctx, &d, Some(row), &f).await
}

#[allow(clippy::too_many_arguments)]
async fn save(
    state: &AppState,
    tenant: &Tenant,
    staff: &crate::extract::CurrentStaff,
    au: &Audit,
    ctx: PageCtx,
    d: &IntegrationDescriptor,
    row: Option<IntegrationRow>,
    f: &FormData,
) -> AppResult<Response> {
    let name = f.text("name", 120);
    let is_active = f.flag("is_active");
    let stored = row
        .as_ref()
        .map(|r| stored_secret_keys(state, r))
        .unwrap_or_default();
    let rebuild = |cfg: &Value| {
        let mut fields = build_fields(d, cfg, &stored);
        // Keep what was typed (never secrets).
        for fl in &mut fields {
            if fl.kind != "password" {
                fl.value = f.text(&fl.name, 20_000);
                fl.checked = f.flag(&fl.name);
            }
        }
        fields
    };
    if name.is_empty() {
        let fields = rebuild(&json!({}));
        let err = ctx.t("admin.common.required");
        return form_page(
            state, tenant, staff, ctx, d, row, fields, name, is_active, err,
        )
        .await;
    }
    let (config, secrets) = match read_fields(d, f) {
        Ok(v) => v,
        Err(label) => {
            let fields = rebuild(&json!({}));
            let err = ctx.tv("admin.integrations.invalid_field", "field", label);
            return form_page(
                state, tenant, staff, ctx, d, row, fields, name, is_active, err,
            )
            .await;
        }
    };
    // Plan limit (`Integrations`), checked right before a new one is created.
    if row.is_none()
        && !super::settings::has_room(
            state,
            tenant.id,
            crate::routes::platform::limits::Limit::Integrations,
        )
        .await?
    {
        let fields = rebuild(&json!({}));
        let err = ctx.t("admin_settings.limits.integrations");
        return form_page(
            state, tenant, staff, ctx, d, row, fields, name, is_active, err,
        )
        .await;
    }
    let input = SaveIntegration {
        id: row.as_ref().map(|r| r.id),
        category: d.category,
        kind: d.kind.to_owned(),
        name: name.clone(),
        config: config.clone(),
        secrets: Some(secrets),
        is_active,
        sort_order: f
            .int("sort_order")
            .ok()
            .flatten()
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0),
    };
    match state
        .billing
        .registry()
        .save_integration(tenant.id, &input)
        .await
    {
        Ok(saved) => {
            // Never log secret values; only which secret keys were replaced.
            let replaced: Vec<&str> = d
                .secret_fields
                .iter()
                .filter(|s| !f.raw(&format!("s_{}", s.key)).trim().is_empty())
                .map(|s| s.key)
                .collect();
            au.log(
                state,
                if row.is_some() {
                    "integration.update"
                } else {
                    "integration.create"
                },
                Some(("integration", saved.id.into())),
                json!({"kind": saved.kind, "name": saved.name, "active": saved.is_active, "config": config, "secrets_replaced": replaced}),
            )
            .await?;
            Ok(done(
                &ctx,
                &format!("/admin/integrations/{}", saved.id),
                "admin.common.saved",
            ))
        }
        Err(e) if e.http_status() < 500 => {
            let fields = rebuild(&config);
            let err = ctx.tv("admin.common.failed", "reason", &e);
            form_page(
                state, tenant, staff, ctx, d, row, fields, name, is_active, err,
            )
            .await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "admin/integrations/_test.html")]
struct TestFragment {
    ctx: PageCtx,
    ok: bool,
    message: String,
}
crate::page!(TestFragment);

/// Classified, admin-facing summary of a failed integration call
/// ([`IntegrationError::summary`], shared with hos-billing, which stores the
/// same text as the connection-test result).
fn classify_integration_error(err: &IntegrationError) -> String {
    err.summary()
}

/// [`classify_integration_error`] for errors that went through hos-billing.
fn classify_core_error(err: &CoreError) -> String {
    if !err.is_user_facing() && !matches!(err, CoreError::Integration(_)) {
        tracing::warn!(error = %err, "integration call failed");
    }
    err.integration_summary()
}

/// htmx: runs the provider's connection test. hos-billing records the
/// classified result on the row; the same text is shown here.
async fn test(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let (ok, message) = if row.category == IntegrationCategory::Oauth {
        (false, ctx.t("admin.integrations.oauth_no_test"))
    } else {
        match state
            .billing
            .registry()
            .test_connection(tenant.id, row.id)
            .await
        {
            Ok(Ok(info)) => (true, info.summary()),
            Ok(Err(e)) => (false, classify_integration_error(&e)),
            Err(e) => (false, classify_core_error(&e)),
        }
    };
    au.log(
        &state,
        "integration.test",
        Some(("integration", row.id.into())),
        json!({"ok": ok}),
    )
    .await?;
    render(&TestFragment { ctx, ok, message })
}

async fn make_default(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let mut conn = state.pool.acquire().await?;
    integrations::set_default_integration(&mut conn, tenant.id, row.id).await?;
    drop(conn);
    state.billing.registry().invalidate_tenant(tenant.id);
    au.log(
        &state,
        "integration.default",
        Some(("integration", row.id.into())),
        json!({"category": row.category.as_str()}),
    )
    .await?;
    Ok(done(
        &ctx,
        "/admin/integrations",
        "admin.integrations.default_set",
    ))
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    match integrations::delete_integration(&state.pool, tenant.id, row.id).await {
        Ok(_) => {}
        Err(e) if e.http_status() == 409 || e.http_status() == 422 => {
            return Ok(failed(
                &ctx,
                &format!("/admin/integrations/{}", row.id),
                ctx.t("admin.integrations.in_use"),
            ));
        }
        Err(e) => return Err(e.into()),
    }
    state.billing.registry().invalidate(tenant.id, row.id);
    au.log(
        &state,
        "integration.delete",
        Some(("integration", row.id.into())),
        json!({"kind": row.kind, "name": row.name}),
    )
    .await?;
    Ok(done(&ctx, "/admin/integrations", "admin.common.deleted"))
}

// ---------------------------------------------------------------------------
// Virtualization inventory
// ---------------------------------------------------------------------------

/// A live VM with the local link (if any).
pub struct VmView {
    pub info: VmInfo,
    pub service: Option<ServiceId>,
}

#[derive(Template)]
#[template(path = "admin/integrations/inventory.html")]
struct InventoryPage {
    ctx: PageCtx,
    row: IntegrationRow,
    nodes: Vec<NodeInfo>,
    templates: Vec<TemplateInfo>,
    vms: Vec<VmView>,
    local: Vec<VmRow>,
    error: String,
}
crate::page!(InventoryPage);

impl InventoryPage {
    fn vm_status(&self, s: &VmStatus) -> String {
        s.as_str().to_owned()
    }
}

async fn inventory(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    if row.category != IntegrationCategory::Virt {
        return Err(AppError::NotFound);
    }
    let local = services::list_vms(&state.pool, tenant.id, Some(row.id)).await?;
    let (mut nodes, mut templates, mut vms, mut error) =
        (Vec::new(), Vec::new(), Vec::new(), String::new());
    match state.billing.registry().virt(tenant.id, row.id).await {
        Ok(virt) => {
            match virt.list_nodes().await {
                Ok(n) => nodes = n,
                Err(e) => error = classify_integration_error(&e),
            }
            templates = virt.list_templates(None).await.unwrap_or_default();
            match virt.list_vms(None).await {
                Ok(list) => {
                    vms = list
                        .into_iter()
                        .map(|info| VmView {
                            service: local
                                .iter()
                                .find(|l| l.external_id == info.vm.id)
                                .and_then(|l| l.service_id),
                            info,
                        })
                        .collect();
                }
                Err(e) => error = classify_integration_error(&e),
            }
        }
        Err(e) => error = classify_core_error(&e),
    }
    let title = ctx.tv("admin.integrations.inventory_title", "name", &row.name);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.integrations.title", "/admin/integrations")],
    );
    render(&InventoryPage {
        ctx,
        row,
        nodes,
        templates,
        vms,
        local,
        error,
    })
}

async fn link_vm(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let back = format!("/admin/integrations/{}/inventory", row.id);
    let external_id = f.text("external_id", 200);
    // The service is entered by id (copied from its page).
    let service_id = Uuid::parse_str(f.text("service_id", 64).trim())
        .ok()
        .map(ServiceId::from);
    let Some(service_id) = service_id else {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin.integrations.service_not_found"),
        ));
    };
    let Some(svc) = services::get_service(&state.pool, tenant.id, service_id).await? else {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin.integrations.service_not_found"),
        ));
    };
    let virt = match state.billing.registry().virt(tenant.id, row.id).await {
        Ok(v) => v,
        Err(e) => return Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    };
    let node = f.opt("node", 100);
    let info = match virt
        .get_vm(&hos_core::integrations::virt::VmRef {
            node: node.clone(),
            id: external_id.clone(),
        })
        .await
    {
        Ok(i) => i,
        Err(e) => {
            return Ok(failed(
                &ctx,
                &back,
                ctx.tv("admin.common.failed", "reason", e),
            ));
        }
    };
    let to_i32 = |v: Option<u64>| v.and_then(|x| i32::try_from(x).ok());
    let vm = services::upsert_vm(
        &state.pool,
        tenant.id,
        &VmInput {
            service_id: Some(svc.id),
            integration_id: row.id,
            node: info.vm.node.clone().or(node),
            external_id: info.vm.id.clone(),
            name: info.name.clone(),
            hostname: info.hostname.clone(),
            os_template: info.template.clone(),
            cpu_cores: info.cpu_cores.and_then(|c| i32::try_from(c).ok()),
            memory_mb: to_i32(info.memory_mb),
            disk_gb: to_i32(info.disk_gb),
            status: info.status,
            primary_ipv4: info.ipv4.first().copied(),
            primary_ipv6: info.ipv6.first().copied(),
            metadata: json!({"linked_by_staff": true}),
        },
    )
    .await?;
    services::update_provisioning(
        &state.pool,
        tenant.id,
        svc.id,
        &services::ProvisioningUpdate {
            integration_id: Some(row.id),
            external_id: Some(info.vm.id.clone()),
            ..services::ProvisioningUpdate::default()
        },
    )
    .await?;
    au.log(
        &state,
        "vm.link",
        Some(("service", svc.id.into())),
        json!({"integration": row.id, "external_id": vm.external_id}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.integrations.vm_linked"))
}

// ---------------------------------------------------------------------------
// IP pools
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/integrations/pools.html")]
struct PoolsPage {
    ctx: PageCtx,
    pools: Vec<IpPoolUsage>,
    virts: Vec<IntegrationRow>,
}
crate::page!(PoolsPage);

async fn pools(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let pools = services::ip_pool_usage(&state.pool, tenant.id).await?;
    let virts =
        integrations::list_integrations(&state.pool, tenant.id, Some(IntegrationCategory::Virt))
            .await?;
    let title = ctx.t("admin.ip.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&PoolsPage { ctx, pools, virts })
}

/// Usable host addresses of a network (IPv4: without network / broadcast
/// for prefixes < 31), minus the gateway; at most `max`.
fn host_addresses(net: &IpNet, gateway: Option<IpAddr>, max: usize) -> Vec<IpAddr> {
    // `hosts()` skips the network / broadcast addresses of IPv4 nets < /31.
    net.hosts()
        .filter(|a| Some(*a) != gateway)
        .take(max)
        .collect()
}

async fn pool_create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/ip-pools";
    let name = f.text("name", 100);
    let Ok(network) = f.text("network", 64).parse::<IpNet>() else {
        return Ok(failed(&ctx, back, ctx.t("admin.ip.invalid_network")));
    };
    let network = network.trunc();
    let gateway = match f.opt("gateway", 64) {
        Some(g) => match g.parse::<IpAddr>() {
            Ok(ip) if network.contains(&ip) => Some(ip),
            _ => return Ok(failed(&ctx, back, ctx.t("admin.ip.invalid_gateway"))),
        },
        None => None,
    };
    let mut nameservers = Vec::new();
    for ns in f
        .text("nameservers", 300)
        .split([',', ' '])
        .filter(|s| !s.is_empty())
    {
        match ns.parse::<IpAddr>() {
            Ok(ip) => nameservers.push(ip),
            Err(_) => return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_ip"))),
        }
    }
    if name.is_empty() {
        return Ok(failed(&ctx, back, ctx.t("admin.common.required")));
    }
    let integration = f.id::<IntegrationId>("integration_id");
    if let Some(i) = integration
        && integrations::get_integration(&state.pool, tenant.id, i)
            .await?
            .is_none()
    {
        return Err(AppError::NotFound);
    }
    let addresses = if f.flag("generate") {
        host_addresses(&network, gateway, 4096)
    } else {
        Vec::new()
    };
    let mut tx = state.pool.begin().await?;
    let pool = services::create_ip_pool(
        &mut *tx,
        tenant.id,
        &name,
        network,
        gateway,
        &nameservers,
        integration,
    )
    .await?;
    let added = if addresses.is_empty() {
        0
    } else {
        services::add_ip_addresses(&mut *tx, tenant.id, pool.id, &addresses).await?
    };
    tx.commit().await?;
    au.log(
        &state,
        "ip_pool.create",
        Some(("ip_pool", pool.id.into())),
        json!({"name": name, "network": network.to_string(), "addresses": added}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/ip-pools/{}", pool.id),
        "admin.common.created",
    ))
}

#[derive(Template)]
#[template(path = "admin/integrations/pool.html")]
struct PoolPage {
    ctx: PageCtx,
    usage: IpPoolUsage,
    addresses: Vec<IpAddressRow>,
}
crate::page!(PoolPage);

async fn load_pool(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<IpPoolUsage> {
    let id = IpPoolId::from(id);
    services::ip_pool_usage(&state.pool, tenant.id)
        .await?
        .into_iter()
        .find(|p| p.pool.id == id)
        .ok_or(AppError::NotFound)
}

async fn pool_show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::IntegrationsManage>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let usage = load_pool(&state, &tenant, id).await?;
    let addresses = arepo::pool_addresses(&state.pool, tenant.id, usage.pool.id).await?;
    let title = usage.pool.name.clone();
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.ip.title", "/admin/ip-pools")],
    );
    render(&PoolPage {
        ctx,
        usage,
        addresses,
    })
}

async fn address_status(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    Path((id, aid)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let usage = load_pool(&state, &tenant, id).await?;
    let back = format!("/admin/ip-pools/{}", usage.pool.id);
    let aid = IpAddressId::from(aid);
    let addr = arepo::pool_addresses(&state.pool, tenant.id, usage.pool.id)
        .await?
        .into_iter()
        .find(|a| a.id == aid)
        .ok_or(AppError::NotFound)?;
    let Some(status) = f.parse::<IpStatus>("status") else {
        return Err(AppError::bad_request("status"));
    };
    if addr.status == IpStatus::Assigned || status == IpStatus::Assigned {
        return Ok(failed(&ctx, &back, ctx.t("admin.ip.assigned_locked")));
    }
    arepo::set_ip_status(&state.pool, tenant.id, aid, status).await?;
    au.log(
        &state,
        "ip.status",
        Some(("ip_pool", usage.pool.id.into())),
        json!({"address": addr.address.to_string(), "status": status.as_str()}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.common.saved"))
}

async fn pool_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::IntegrationsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let usage = load_pool(&state, &tenant, id).await?;
    if !arepo::delete_ip_pool(&state.pool, tenant.id, usage.pool.id).await? {
        return Ok(failed(
            &ctx,
            &format!("/admin/ip-pools/{}", usage.pool.id),
            ctx.t("admin.ip.in_use"),
        ));
    }
    au.log(
        &state,
        "ip_pool.delete",
        Some(("ip_pool", usage.pool.id.into())),
        json!({"name": usage.pool.name}),
    )
    .await?;
    Ok(done(&ctx, "/admin/ip-pools", "admin.common.deleted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_errors_are_classified_and_short() {
        let e = IntegrationError::remote_with_code(Some(502), "bad_gateway", "upstream down");
        assert_eq!(
            classify_integration_error(&e),
            "provider error (HTTP 502) [bad_gateway]: upstream down"
        );
        assert_eq!(
            classify_integration_error(&IntegrationError::Timeout),
            "timed out"
        );
        let long = format!("line1\r\n<html>{}</html>", "x".repeat(5000));
        let text = classify_integration_error(&IntegrationError::Transport(long));
        assert!(
            text.starts_with("connection failed: line1 <html>"),
            "{text}"
        );
        assert!(!text.contains('\n') && !text.contains('\r'));
        assert!(
            text.chars().count()
                <= "connection failed: ".len() + hos_core::error::SUMMARY_DETAIL_CHARS + 1
        );
        let blocked = IntegrationError::Config(
            "endpoint 127.0.0.1 is not allowed: it is or resolves to a loopback address \
             (HOS_OUTBOUND_POLICY=private)"
                .into(),
        );
        assert!(classify_integration_error(&blocked).starts_with("configuration error: endpoint"));
        let core = CoreError::Integration(IntegrationError::Auth("token rejected".into()));
        assert_eq!(
            classify_core_error(&core),
            "authentication failed: token rejected"
        );
        let internal = CoreError::Internal("db password=hunter2".into());
        assert_eq!(
            classify_core_error(&internal),
            "internal error (details in the server log)"
        );
    }

    #[test]
    fn hosts_skip_gateway_and_edges() {
        let net: IpNet = "192.0.2.0/29".parse().unwrap();
        let gw: IpAddr = "192.0.2.1".parse().unwrap();
        let hosts = host_addresses(&net, Some(gw), 100);
        assert_eq!(hosts.len(), 5);
        assert!(!hosts.contains(&gw));
        assert_eq!(hosts.last().unwrap().to_string(), "192.0.2.6");
        assert_eq!(host_addresses(&net, None, 2).len(), 2);
    }
}
