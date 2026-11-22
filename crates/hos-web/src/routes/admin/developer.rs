//! Developer tools (`/admin/developer*`, `settings.manage`).
//!
//! * **API keys** owned by staff: the creator picks the scopes from their own
//!   permissions (never more), the secret is shown once (`no-store`), keys can
//!   be revoked. `routes/api` evaluates a key as `scopes ∩ owner's current
//!   permissions`, so a later role change narrows the key too.
//! * **Outgoing webhooks**: `https://` only and no loopback / private /
//!   link-local targets (checked here after DNS resolution and again by the
//!   delivery job, which pins the checked addresses). Event selection,
//!   signing secret shown once / regenerate, on/off, delivery log (the
//!   `deliver_webhook` jobs) with resend, and a test event.

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{Duration, Utc};
use hos_auth::rbac::{Permission, PermissionSet};
use hos_billing::jobs::{self, DeliverWebhookPayload, JobKind, JobRequest};
use hos_billing::notify::{WebhookTargetError, resolve_webhook_target};
use hos_core::ids::{ApiKeyId, WebhookEndpointId};
use hos_db::repo::admin_settings as srepo;
use hos_db::repo::identity::{self, ApiKeyRow};
use hos_db::repo::jobs::JobRow;
use hos_db::repo::webhooks::WebhookEndpointRow;
use http::{HeaderValue, header};
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use super::staff::roles::permission_label;
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::forms::FormErrors;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render, render_status};

const BACK: &str = "/admin/developer";
const HOOKS: &str = "/admin/developer/webhooks";

/// Events a webhook endpoint can subscribe to (what `hos-billing` emits).
pub const EVENTS: &[&str] = &[
    "order.created",
    "order.accepted",
    "invoice.created",
    "invoice.paid",
    "payment.refunded",
    "payment.chargeback",
    "service.activated",
    "service.provisioning_failed",
    "service.suspended",
    "service.unsuspended",
    "service.terminated",
    "service.plan_changed",
    "domain.registered",
];

/// Event of the "send test" button (delivered regardless of the selection).
pub const TEST_EVENT: &str = "webhook.test";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(BACK, get(overview))
        .route("/admin/developer/keys", get(keys_index).post(create_key))
        .route("/admin/developer/keys/{id}/revoke", post(revoke_key))
        .route(HOOKS, get(hooks).post(create_hook))
        .route("/admin/developer/webhooks/new", get(new_hook))
        .route(
            "/admin/developer/webhooks/{id}",
            get(show_hook).post(update_hook),
        )
        .route("/admin/developer/webhooks/{id}/secret", post(rotate_secret))
        .route("/admin/developer/webhooks/{id}/test", post(test_hook))
        .route("/admin/developer/webhooks/{id}/delete", post(delete_hook))
        .route(
            "/admin/developer/webhooks/{id}/deliveries/{job}/resend",
            post(resend),
        )
}

fn tabs(ctx: &PageCtx) -> Vec<NavItem> {
    common::tabs(
        ctx,
        &[
            ("admin_settings.developer.tab_keys", BACK.into()),
            ("admin_settings.developer.tab_webhooks", HOOKS.into()),
        ],
    )
}

fn no_store(mut res: Response) -> Response {
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/// A scope checkbox (one catalog permission the creator holds).
#[derive(Debug, Clone)]
pub struct ScopeChoice {
    pub value: String,
    pub label: String,
    pub checked: bool,
}

/// One key row.
#[derive(Debug, Clone)]
pub struct KeyView {
    pub key: ApiKeyRow,
    pub owner: String,
    pub scopes: Vec<String>,
    pub mine: bool,
}

#[derive(Template)]
#[template(path = "admin/developer/keys.html")]
struct KeysPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    keys: Vec<KeyView>,
    scopes: Vec<ScopeChoice>,
    key_name: String,
    expires_days: String,
    errors: FormErrors,
    /// The new key, shown once.
    secret: String,
}
crate::page!(KeysPage);

impl KeysPage {
    fn status(&self, k: &ApiKeyRow) -> &'static str {
        if k.revoked_at.is_some() {
            "revoked"
        } else if k.expires_at.is_some_and(|e| e <= Utc::now()) {
            "expired"
        } else {
            "active"
        }
    }
}

fn scope_choices(ctx: &PageCtx, staff: &CurrentStaff, picked: &[String]) -> Vec<ScopeChoice> {
    staff
        .permissions
        .effective()
        .into_iter()
        .map(|p| ScopeChoice {
            value: p.as_str().to_owned(),
            label: permission_label(ctx, p),
            checked: picked.iter().any(|s| s == p.as_str()),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn keys_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    picked: &[String],
    name: String,
    expires_days: String,
    errors: FormErrors,
    secret: String,
) -> AppResult<Response> {
    let owners: BTreeMap<_, _> = identity::list_staff(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|u| (u.id, u.display_name()))
        .collect();
    let keys = srepo::staff_api_keys(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|k| KeyView {
            owner: k
                .user_id
                .and_then(|u| owners.get(&u).cloned())
                .unwrap_or_default(),
            scopes: if k.scopes.is_empty() {
                vec![ctx.t("admin_settings.developer.scope_all")]
            } else {
                PermissionSet::from_grants(k.scopes.iter().cloned())
                    .effective()
                    .into_iter()
                    .map(|p| permission_label(&ctx, p))
                    .collect()
            },
            mine: k.user_id == Some(staff.user_id),
            key: k,
        })
        .collect();
    let scopes = scope_choices(&ctx, staff, picked);
    let title = ctx.t("admin_settings.developer.title");
    common::page(&mut ctx, staff, &title, &[]);
    let page = KeysPage {
        tabs: tabs(&ctx),
        active: ctx.url(BACK),
        keys,
        scopes,
        key_name: name,
        expires_days,
        errors,
        secret,
        ctx,
    };
    let res = if page.errors.any() {
        render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)?
    } else {
        render(&page)?
    };
    Ok(no_store(res))
}

/// `GET /admin/developer/keys` (the form's action, e.g. after a reload or a
/// bookmarked URL): the keys list is the developer overview.
async fn keys_index(
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> Response {
    axum::response::IntoResponse::into_response(axum::response::Redirect::to(&ctx.url(BACK)))
}

async fn overview(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    keys_page(
        &state,
        &tenant,
        &staff,
        ctx,
        &[],
        String::new(),
        String::new(),
        FormErrors::default(),
        String::new(),
    )
    .await
}

async fn create_key(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let name = f.text("name", 80);
    let expires_days = f.text("expires_days", 5);
    let picked: Vec<String> = f.all("scopes");
    let mut errors = FormErrors::default();
    if name.is_empty() {
        errors.add("name", ctx.t("admin.common.required"));
    }
    let mut scopes = Vec::new();
    for s in &picked {
        match Permission::parse(s) {
            // Only what the creator holds right now.
            Some(p) if staff.permissions.allows(p) => scopes.push(p.as_str().to_owned()),
            _ => errors.add_general(ctx.t("admin_settings.staff.errors.escalation")),
        }
    }
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() && !errors.any() {
        errors.add_general(ctx.t("admin_settings.developer.scopes_required"));
    }
    let expires_at = match expires_days.parse::<i64>() {
        _ if expires_days.is_empty() => None,
        Ok(d) if (1..=3650).contains(&d) => Some(Utc::now() + Duration::days(d)),
        _ => {
            errors.add(
                "expires_days",
                ctx.tv2("admin_settings.errors.range", "min", 1, "max", 3650),
            );
            None
        }
    };
    if errors.any() {
        return keys_page(
            &state,
            &tenant,
            &staff,
            ctx,
            &picked,
            name,
            expires_days,
            errors,
            String::new(),
        )
        .await;
    }
    let generated = hos_auth::tokens::generate_api_key();
    let key = identity::create_api_key(
        &state.pool,
        tenant.id,
        Some(staff.user_id),
        &name,
        &generated.hint,
        &generated.key_hash,
        &scopes,
        expires_at,
    )
    .await?;
    au.log(
        &state,
        "api_key.create",
        Some(("api_key", key.id.into())),
        json!({"name": name, "scopes": scopes, "expires_at": expires_at}),
    )
    .await?;
    let secret = generated.key.expose_str().to_owned();
    keys_page(
        &state,
        &tenant,
        &staff,
        ctx,
        &[],
        String::new(),
        String::new(),
        FormErrors::default(),
        secret,
    )
    .await
}

async fn revoke_key(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id: ApiKeyId = id.parse().map_err(|_| AppError::NotFound)?;
    let key = srepo::staff_api_key(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    identity::revoke_api_key(&state.pool, tenant.id, key.id).await?;
    au.log(
        &state,
        "api_key.revoke",
        Some(("api_key", key.id.into())),
        json!({"name": key.name, "prefix": key.prefix}),
    )
    .await?;
    Ok(done(&ctx, BACK, "admin_settings.developer.key_revoked"))
}

// ---------------------------------------------------------------------------
// webhooks
// ---------------------------------------------------------------------------

/// Endpoint form values.
#[derive(Debug, Clone, Default)]
pub struct HookForm {
    pub url: String,
    pub description: String,
    pub events: Vec<String>,
    pub active: bool,
}

impl HookForm {
    fn read(f: &FormData) -> Self {
        Self {
            url: f.text("url", 2000),
            description: f.text("description", 200),
            events: f
                .all("events")
                .into_iter()
                .filter(|e| EVENTS.contains(&e.as_str()))
                .collect(),
            active: f.flag("is_active"),
        }
    }

    fn of(row: &WebhookEndpointRow) -> Self {
        Self {
            url: row.url.clone(),
            description: row.description.clone().unwrap_or_default(),
            events: row.events.clone(),
            active: row.is_active,
        }
    }

    /// Validates (including DNS) — the error goes to the `url` field.
    async fn validate(&self, ctx: &PageCtx, errors: &mut FormErrors) {
        let key = match resolve_webhook_target(&self.url).await {
            Ok(_) => return,
            Err(WebhookTargetError::NotHttps) => "admin_settings.developer.url_https",
            Err(WebhookTargetError::NonPublic) => "admin_settings.developer.url_private",
            Err(WebhookTargetError::Unresolvable) => "admin_settings.developer.url_unresolvable",
            Err(WebhookTargetError::Invalid) => "admin_settings.developer.url_invalid",
            Err(WebhookTargetError::Offline) => "demo.blocked.text",
        };
        errors.add("url", ctx.t(key));
    }
}

/// One endpoint row.
#[derive(Debug, Clone)]
pub struct HookView {
    pub row: WebhookEndpointRow,
    pub events: String,
}

#[derive(Template)]
#[template(path = "admin/developer/webhooks.html")]
struct HooksPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<HookView>,
}
crate::page!(HooksPage);

fn events_text(ctx: &PageCtx, events: &[String]) -> String {
    if events.is_empty() {
        ctx.t("admin_settings.developer.all_events")
    } else {
        events.join(", ")
    }
}

/// The webhook list is the `webhooks` tab of the overview.
async fn hooks_list(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = hos_db::repo::webhooks::list_endpoints(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|row| HookView {
            events: events_text(&ctx, &row.events),
            row,
        })
        .collect();
    let title = ctx.t("admin_settings.developer.webhooks");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_settings.developer.title", BACK)],
    );
    render(&HooksPage {
        tabs: tabs(&ctx),
        active: ctx.url(HOOKS),
        rows,
        ctx,
    })
}

#[derive(Template)]
#[template(path = "admin/developer/webhook.html")]
struct HookPage {
    ctx: PageCtx,
    endpoint: Option<WebhookEndpointRow>,
    form: HookForm,
    events: Vec<(String, bool)>,
    deliveries: Vec<JobRow>,
    errors: FormErrors,
    /// Signing secret, shown once after creation / rotation.
    secret: String,
}
crate::page!(HookPage);

impl HookPage {
    fn action_url(&self) -> String {
        match &self.endpoint {
            Some(e) => format!("{HOOKS}/{}", e.id),
            None => HOOKS.to_owned(),
        }
    }

    fn event_of(&self, j: &JobRow) -> String {
        j.payload
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    }

    fn outcome(&self, j: &JobRow) -> String {
        j.result
            .as_ref()
            .and_then(|r| r.get("status"))
            .map(|s| format!("HTTP {s}"))
            .or_else(|| j.last_error.clone())
            .unwrap_or_default()
    }
}

#[allow(clippy::too_many_arguments)]
async fn hook_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    endpoint: Option<WebhookEndpointRow>,
    form: HookForm,
    errors: FormErrors,
    secret: String,
) -> AppResult<Response> {
    let deliveries = match &endpoint {
        Some(e) => srepo::endpoint_deliveries(&state.pool, tenant.id, e.id, 50).await?,
        None => Vec::new(),
    };
    let events = EVENTS
        .iter()
        .map(|e| ((*e).to_owned(), form.events.iter().any(|x| x == e)))
        .collect();
    let title = match &endpoint {
        Some(e) => e.url.clone(),
        None => ctx.t("admin_settings.developer.new_webhook"),
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[
            ("admin_settings.developer.title", BACK),
            ("admin_settings.developer.webhooks", HOOKS),
        ],
    );
    let page = HookPage {
        ctx,
        endpoint,
        form,
        events,
        deliveries,
        errors,
        secret,
    };
    let res = if page.errors.any() {
        render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)?
    } else {
        render(&page)?
    };
    Ok(no_store(res))
}

async fn load_hook(state: &AppState, tenant: &Tenant, id: &str) -> AppResult<WebhookEndpointRow> {
    let id: WebhookEndpointId = id.parse().map_err(|_| AppError::NotFound)?;
    srepo::get_endpoint(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn new_hook(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = HookForm {
        active: true,
        ..HookForm::default()
    };
    hook_page(
        &state,
        &tenant,
        &staff,
        ctx,
        None,
        form,
        FormErrors::default(),
        String::new(),
    )
    .await
}

/// `POST /admin/developer/webhooks`.
async fn create_hook(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = HookForm::read(&f);
    let mut errors = FormErrors::default();
    form.validate(&ctx, &mut errors).await;
    if errors.any() {
        return hook_page(
            &state,
            &tenant,
            &staff,
            ctx,
            None,
            form,
            errors,
            String::new(),
        )
        .await;
    }
    let (row, secret) = state
        .billing
        .create_webhook_endpoint(
            tenant.id,
            &form.url,
            (!form.description.is_empty()).then_some(form.description.as_str()),
            &form.events,
        )
        .await?;
    let row = if form.active {
        row
    } else {
        srepo::update_endpoint(
            &state.pool,
            tenant.id,
            row.id,
            &row.url,
            row.description.as_deref(),
            &row.events,
            false,
        )
        .await?;
        srepo::get_endpoint(&state.pool, tenant.id, row.id)
            .await?
            .ok_or(AppError::NotFound)?
    };
    au.log(
        &state,
        "webhook.create",
        Some(("webhook_endpoint", row.id.into())),
        json!({"url": row.url, "events": row.events, "active": row.is_active}),
    )
    .await?;
    let form = HookForm::of(&row);
    hook_page(
        &state,
        &tenant,
        &staff,
        ctx,
        Some(row),
        form,
        FormErrors::default(),
        secret.expose_str().to_owned(),
    )
    .await
}

async fn show_hook(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load_hook(&state, &tenant, &id).await?;
    let form = HookForm::of(&row);
    hook_page(
        &state,
        &tenant,
        &staff,
        ctx,
        Some(row),
        form,
        FormErrors::default(),
        String::new(),
    )
    .await
}

async fn update_hook(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let row = load_hook(&state, &tenant, &id).await?;
    let form = HookForm::read(&f);
    let mut errors = FormErrors::default();
    form.validate(&ctx, &mut errors).await;
    if errors.any() {
        return hook_page(
            &state,
            &tenant,
            &staff,
            ctx,
            Some(row),
            form,
            errors,
            String::new(),
        )
        .await;
    }
    let url = url::Url::parse(form.url.trim())
        .map(|u| u.to_string())
        .map_err(|_| AppError::BadRequest("url".into()))?;
    srepo::update_endpoint(
        &state.pool,
        tenant.id,
        row.id,
        &url,
        (!form.description.is_empty()).then_some(form.description.as_str()),
        &form.events,
        form.active,
    )
    .await?;
    au.log(
        &state,
        "webhook.update",
        Some(("webhook_endpoint", row.id.into())),
        json!({
            "url": {"from": row.url, "to": url},
            "events": {"from": row.events, "to": form.events},
            "active": {"from": row.is_active, "to": form.active},
        }),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("{HOOKS}/{}", row.id),
        "admin.common.saved",
    ))
}

async fn rotate_secret(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load_hook(&state, &tenant, &id).await?;
    let secret = hos_core::crypto::generate_prefixed_token("whsec");
    let sealed = hos_billing::secrets::seal(
        state.billing.secret_box(),
        &secret,
        &hos_billing::secrets::webhook_endpoint_aad(tenant.id),
    )?;
    srepo::set_endpoint_secret(&state.pool, tenant.id, row.id, &sealed).await?;
    au.log(
        &state,
        "webhook.secret_rotate",
        Some(("webhook_endpoint", row.id.into())),
        json!({"url": row.url}),
    )
    .await?;
    let form = HookForm::of(&row);
    hook_page(
        &state,
        &tenant,
        &staff,
        ctx,
        Some(row),
        form,
        FormErrors::default(),
        secret,
    )
    .await
}

async fn enqueue_delivery(
    state: &AppState,
    tenant: &Tenant,
    payload: &DeliverWebhookPayload,
) -> AppResult<()> {
    let mut conn = state.pool.acquire().await?;
    jobs::enqueue(
        &mut conn,
        JobRequest::new(JobKind::DeliverWebhook, Some(tenant.id), payload),
    )
    .await?;
    Ok(())
}

async fn test_hook(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load_hook(&state, &tenant, &id).await?;
    let back = format!("{HOOKS}/{}", row.id);
    if !row.is_active {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin_settings.developer.inactive"),
        ));
    }
    let payload = DeliverWebhookPayload {
        endpoint_id: row.id,
        event: TEST_EVENT.to_owned(),
        event_id: Uuid::now_v7(),
        occurred_at: Utc::now(),
        data: json!({"message": "HostingOS test event", "sent_by": staff.user.email}),
    };
    enqueue_delivery(&state, &tenant, &payload).await?;
    au.log(
        &state,
        "webhook.test",
        Some(("webhook_endpoint", row.id.into())),
        json!({"event_id": payload.event_id}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin_settings.developer.test_queued"))
}

async fn resend(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path((id, job)): Path<(String, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load_hook(&state, &tenant, &id).await?;
    let back = format!("{HOOKS}/{}", row.id);
    let job_id = Uuid::parse_str(&job).map_err(|_| AppError::NotFound)?;
    let job = srepo::endpoint_delivery(&state.pool, tenant.id, row.id, job_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let payload: DeliverWebhookPayload = serde_json::from_value(job.payload.clone())
        .map_err(|_| AppError::BadRequest("payload".into()))?;
    // Same event id: receivers can de-duplicate.
    enqueue_delivery(&state, &tenant, &payload).await?;
    au.log(
        &state,
        "webhook.resend",
        Some(("webhook_endpoint", row.id.into())),
        json!({"event": payload.event, "event_id": payload.event_id}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin_settings.developer.resent"))
}

async fn delete_hook(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load_hook(&state, &tenant, &id).await?;
    hos_db::repo::api::delete_webhook_endpoint(&state.pool, tenant.id, row.id).await?;
    au.log(
        &state,
        "webhook.delete",
        Some(("webhook_endpoint", row.id.into())),
        json!({"url": row.url}),
    )
    .await?;
    Ok(done(&ctx, HOOKS, "admin.common.deleted"))
}

/// `GET /admin/developer/webhooks`.
async fn hooks(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    hooks_list(&state, &tenant, &staff, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_unique_and_dotted() {
        let mut seen = std::collections::BTreeSet::new();
        for e in EVENTS {
            assert!(e.contains('.'), "{e}");
            assert!(seen.insert(*e), "{e}");
        }
        assert!(!EVENTS.contains(&TEST_EVENT));
    }
}
