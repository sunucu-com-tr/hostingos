//! Services. Staff: `services.view` (read), `services.manage` (actions).
//! Clients: their own services, VM actions included (suspend / unsuspend /
//! terminate are staff-only). Lifecycle work goes through hos-billing, which
//! queues provisioning jobs (`202`) or calls the virtualisation module.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use hos_auth::rbac::Permission;
use hos_core::ids::ServiceId;
use hos_core::integrations::virt::PowerAction;
use hos_core::{CoreError, IntegrationError};
use hos_db::repo::{api as api_repo, services as services_repo};
use serde::Deserialize;
use serde_json::json;

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, Scope, Sorts, dto, list_query, paged,
    parse_body, parse_id,
};
use crate::state::AppState;

const SORTS: Sorts = &[
    ("created_at", "created_at"),
    ("next_due_date", "next_due_date"),
    ("recurring_amount", "recurring_amount"),
];
const STATUSES: &[&str] = &[
    "pending",
    "provisioning",
    "active",
    "suspended",
    "terminated",
    "cancelled",
    "failed",
];

/// `GET /api/v1/services`.
pub async fn list(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let q = list_query(&raw, SORTS, STATUSES, scope)?;
    let page = api_repo::list_services(&state.pool, tenant.id, &q.filter, q.sort, q.page).await?;
    Ok(paged(&page, dto::service))
}

/// Loads a service the caller may see (`404` otherwise).
async fn load(
    state: &AppState,
    tenant: &ApiTenant,
    scope: Scope,
    raw: &str,
) -> ApiResult<hos_db::repo::services::ServiceRow> {
    let id: ServiceId = parse_id(raw)?;
    let s = services_repo::get_service(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    scope.owns(Some(s.client_id))?;
    Ok(s)
}

/// `GET /api/v1/services/{id}`.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    Ok(ApiReply::ok(dto::service_row(&s)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonBody {
    #[serde(default)]
    reason: Option<String>,
}

fn queued(id: ServiceId, action: &str) -> ApiReply {
    ApiReply::accepted(json!({ "service_id": id, "action": action, "queued": true }))
}

/// `POST /api/v1/services/{id}/suspend` (staff, `services.manage`).
pub async fn suspend(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let user = auth.staff(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, Scope::Staff(user), &id).await?;
    let input: ReasonBody = parse_body(&body)?;
    let reason = input
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("Suspended via API");
    state
        .billing
        .request_suspend(tenant.id, s.id, reason, Some(user))
        .await?;
    Ok(queued(s.id, "suspend"))
}

/// `POST /api/v1/services/{id}/unsuspend` (staff).
pub async fn unsuspend(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let user = auth.staff(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, Scope::Staff(user), &id).await?;
    state
        .billing
        .request_unsuspend(tenant.id, s.id, Some(user))
        .await?;
    Ok(queued(s.id, "unsuspend"))
}

/// `POST /api/v1/services/{id}/terminate` (staff).
pub async fn terminate(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let user = auth.staff(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, Scope::Staff(user), &id).await?;
    state
        .billing
        .request_terminate(tenant.id, s.id, Some(user))
        .await?;
    Ok(queued(s.id, "terminate"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PowerBody {
    action: String,
}

/// `POST /api/v1/services/{id}/power` — `start|stop|shutdown|reboot|reset`.
pub async fn power(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: PowerBody = parse_body(&body)?;
    let action: PowerAction =
        input.action.trim().parse().map_err(|_| {
            ApiError::field("action", "one of start, stop, shutdown, reboot, reset")
        })?;
    state
        .billing
        .vm_power(tenant.id, s.id, scope.client(), action)
        .await?;
    Ok(ApiReply::ok(
        json!({ "service_id": s.id, "action": action.as_str(), "ok": true }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReinstallBody {
    template: String,
    #[serde(default)]
    ssh_keys: Vec<String>,
}

/// `POST /api/v1/services/{id}/reinstall`.
pub async fn reinstall(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: ReinstallBody = parse_body(&body)?;
    let template = input.template.trim();
    if template.is_empty() {
        return Err(ApiError::field("template", "required"));
    }
    if input.ssh_keys.len() > 20 || input.ssh_keys.iter().any(|k| k.len() > 16 * 1024) {
        return Err(ApiError::field("ssh_keys", "at most 20 keys of 16 KiB"));
    }
    state
        .billing
        .vm_reinstall(tenant.id, s.id, scope.client(), template, input.ssh_keys)
        .await
        .map_err(reinstall_error)?;
    Ok(ApiReply::accepted(
        json!({ "service_id": s.id, "action": "reinstall", "template": template }),
    ))
}

/// Maps a `vm_reinstall` failure. A refused template — hos-billing's
/// allow-list (`Validation`) or the virtualisation driver's own template
/// check (`InvalidInput`) — is a client error: `422`, never a `5xx`. The
/// allow-list itself lives in hos-billing, not here.
fn reinstall_error(err: CoreError) -> ApiError {
    match err {
        CoreError::Validation(msg)
        | CoreError::Integration(IntegrationError::InvalidInput(msg)) => ApiError::new(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
            msg,
        ),
        other => other.into(),
    }
}

/// `GET /api/v1/services/{id}/snapshots`.
pub async fn snapshots(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let list = state
        .billing
        .vm_snapshots(tenant.id, s.id, scope.client())
        .await?;
    Ok(super::collection(
        list.iter()
            .map(|sn| {
                json!({
                    "id": sn.id,
                    "name": sn.name,
                    "description": sn.description,
                    "created_at": sn.created_at,
                    "size_mb": sn.size_mb,
                    "includes_memory": sn.includes_memory,
                })
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotBody {
    name: String,
}

/// `POST /api/v1/services/{id}/snapshots`.
pub async fn create_snapshot(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: SnapshotBody = parse_body(&body)?;
    let name = input.name.trim();
    if name.is_empty()
        || name.len() > 40
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(ApiError::field(
            "name",
            "1–40 characters: letters, digits, - and _",
        ));
    }
    let snap = state
        .billing
        .vm_create_snapshot(tenant.id, s.id, scope.client(), name)
        .await?;
    Ok(ApiReply::created(
        json!({ "service_id": s.id, "snapshot_id": snap, "name": name }),
    ))
}

/// `POST /api/v1/services/{id}/snapshots/{snapshot}/rollback`.
pub async fn rollback_snapshot(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path((id, snapshot)): Path<(String, String)>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    state
        .billing
        .vm_rollback_snapshot(tenant.id, s.id, scope.client(), &snapshot)
        .await?;
    Ok(ApiReply::ok(
        json!({ "service_id": s.id, "snapshot_id": snapshot, "rolled_back": true }),
    ))
}

/// `DELETE /api/v1/services/{id}/snapshots/{snapshot}`.
pub async fn delete_snapshot(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path((id, snapshot)): Path<(String, String)>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    state
        .billing
        .vm_delete_snapshot(tenant.id, s.id, scope.client(), &snapshot)
        .await?;
    Ok(ApiReply::ok(
        json!({ "service_id": s.id, "snapshot_id": snapshot, "deleted": true }),
    ))
}

/// `POST /api/v1/services/{id}/console` — short-lived console access.
pub async fn console(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let c = match state
        .billing
        .vm_console(tenant.id, s.id, scope.client())
        .await
    {
        Ok(c) => c,
        // Relay drivers: hypervisor tickets never leave HostingOS; the
        // console is the signed-in page (one-time token + websocket relay).
        Err(CoreError::Integration(IntegrationError::Unsupported(_)))
            if state
                .billing
                .vm_features(tenant.id, s.id, scope.client())
                .await
                .is_ok_and(|f| f.console_relay) =>
        {
            let path = match scope.client() {
                Some(_) => format!("/client/services/{}/server/console/view", s.id),
                None => format!("/admin/services/{}/vm/console/view", s.id),
            };
            let url = state
                .base_url()
                .join(&path)
                .map(|u| u.to_string())
                .unwrap_or(path);
            return Ok(ApiReply::ok(json!({
                "service_id": s.id,
                "kind": "embedded",
                "url": url,
                "ticket": null,
                "password": null,
                "port": null,
                "expires_at": null,
            })));
        }
        Err(e) => return Err(e.into()),
    };
    Ok(ApiReply::ok(json!({
        "service_id": s.id,
        "kind": c.kind.as_str(),
        "url": c.url,
        "ticket": c.ticket.as_ref().map(|t| t.expose_str().to_owned()),
        "password": c.password.as_ref().map(|t| t.expose_str().to_owned()),
        "port": c.port,
        "expires_at": c.expires_at,
    })))
}

// ---------------------------------------------------------------------------
// VPS management (hos_billing::vm_ops): every id from the request is looked
// up on the service's own VM / client in hos-billing.
// ---------------------------------------------------------------------------

fn rate(limiter: &hos_auth::ratelimit::RateLimiter, key: &str) -> ApiResult<()> {
    crate::state::check_rate(limiter, key).map_err(ApiError::from)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordResetBody {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    confirm: bool,
}

/// `POST /api/v1/services/{id}/password-reset` — new root password, returned
/// once (not stored, not e-mailed). Needs `"confirm": true`.
pub async fn password_reset(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: PasswordResetBody = parse_body(&body)?;
    if !input.confirm {
        return Err(ApiError::field("confirm", "must be true"));
    }
    rate(&state.limits.vm_password, &format!("vmpw:{}", s.id))?;
    let r = state
        .billing
        .vm_reset_password(tenant.id, s.id, scope.client(), input.username.as_deref())
        .await?;
    Ok(ApiReply::ok(json!({
        "service_id": s.id,
        "username": r.username,
        "password": r.password.expose_str(),
    })))
}

/// `GET /api/v1/services/{id}/backups`.
pub async fn backups(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let list = state
        .billing
        .vm_backups(tenant.id, s.id, scope.client())
        .await?;
    Ok(super::collection(
        list.into_iter()
            .map(|b| json!({"id": b.id, "name": b.name, "created_at": b.created_at, "size_mb": b.size_mb}))
            .collect(),
    ))
}

/// `POST /api/v1/services/{id}/backups/{backup}/restore`.
pub async fn restore_backup(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path((id, backup)): Path<(String, String)>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    rate(
        &state.limits.vm_action,
        &format!("vm:{}:{}", scope.user(), s.id),
    )?;
    state
        .billing
        .vm_restore_backup(tenant.id, s.id, scope.client(), &backup)
        .await?;
    Ok(ApiReply::accepted(
        json!({"service_id": s.id, "action": "backup_restore", "queued": true}),
    ))
}

/// `GET /api/v1/services/{id}/reverse-dns`.
pub async fn reverse_dns(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let list = state
        .billing
        .vm_reverse_dns(tenant.id, s.id, scope.client())
        .await?;
    Ok(super::collection(
        list.into_iter()
            .map(|p| json!({"ip": p.ip, "ptr": p.ptr, "editable": p.editable}))
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PtrBody {
    ip: String,
    #[serde(default)]
    ptr: Option<String>,
}

/// `PUT /api/v1/services/{id}/reverse-dns` — `ptr: null` resets.
pub async fn set_reverse_dns(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: PtrBody = parse_body(&body)?;
    let ip = input
        .ip
        .trim()
        .parse()
        .map_err(|_| ApiError::field("ip", "an IPv4 or IPv6 address"))?;
    rate(
        &state.limits.vm_action,
        &format!("vm:{}:{}", scope.user(), s.id),
    )?;
    state
        .billing
        .vm_set_reverse_dns(tenant.id, s.id, scope.client(), ip, input.ptr.as_deref())
        .await?;
    Ok(ApiReply::ok(json!({"service_id": s.id, "ok": true})))
}

/// `GET /api/v1/services/{id}/isos` — mountable (approved) images.
pub async fn isos(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let list = state
        .billing
        .vm_isos(tenant.id, s.id, scope.client())
        .await?;
    Ok(super::collection(
        list.into_iter()
            .map(|i| json!({"id": i.iso_ref, "name": i.name}))
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IsoBody {
    iso: String,
    #[serde(default)]
    boot: bool,
}

/// `POST /api/v1/services/{id}/iso`.
pub async fn mount_iso(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: IsoBody = parse_body(&body)?;
    rate(
        &state.limits.vm_action,
        &format!("vm:{}:{}", scope.user(), s.id),
    )?;
    state
        .billing
        .vm_mount_iso(tenant.id, s.id, scope.client(), &input.iso, input.boot)
        .await?;
    Ok(ApiReply::ok(json!({"service_id": s.id, "ok": true})))
}

/// `DELETE /api/v1/services/{id}/iso`.
pub async fn unmount_iso(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    rate(
        &state.limits.vm_action,
        &format!("vm:{}:{}", scope.user(), s.id),
    )?;
    state
        .billing
        .vm_unmount_iso(tenant.id, s.id, scope.client())
        .await?;
    Ok(ApiReply::ok(json!({"service_id": s.id, "ok": true})))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RescueBody {
    enabled: bool,
    #[serde(default)]
    ssh_key_ids: Vec<uuid::Uuid>,
}

/// `POST /api/v1/services/{id}/rescue` — the rescue password is returned once.
pub async fn rescue(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: RescueBody = parse_body(&body)?;
    rate(
        &state.limits.vm_action,
        &format!("vm:{}:{}", scope.user(), s.id),
    )?;
    let r = state
        .billing
        .vm_set_rescue(
            tenant.id,
            s.id,
            scope.client(),
            input.enabled,
            &input.ssh_key_ids,
        )
        .await?;
    Ok(ApiReply::ok(json!({
        "service_id": s.id,
        "enabled": input.enabled,
        "password": r.password.as_ref().map(|p| p.expose_str().to_owned()),
    })))
}

fn firewall_json(f: &hos_core::integrations::virt::FirewallState) -> serde_json::Value {
    serde_json::to_value(f).unwrap_or_default()
}

/// `GET /api/v1/services/{id}/firewall`.
pub async fn firewall(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let f = state
        .billing
        .vm_firewall(tenant.id, s.id, scope.client())
        .await?;
    Ok(ApiReply::ok(firewall_json(&f)))
}

/// `PUT /api/v1/services/{id}/firewall` — replaces the managed rules.
pub async fn set_firewall(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let input: hos_core::integrations::virt::FirewallState = parse_body(&body)?;
    rate(
        &state.limits.vm_action,
        &format!("vm:{}:{}", scope.user(), s.id),
    )?;
    let applied = state
        .billing
        .vm_set_firewall(tenant.id, s.id, scope.client(), &input)
        .await?;
    Ok(ApiReply::ok(firewall_json(&applied)))
}

/// `GET /api/v1/services/{id}/metrics?range=hour|day|week|month`.
pub async fn metrics(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let range = crate::vm_charts::parse_range(raw.get("range").map(String::as_str));
    let h = state
        .billing
        .vm_metrics_history(tenant.id, s.id, scope.client(), range)
        .await?;
    Ok(ApiReply::ok(serde_json::to_value(&h).unwrap_or_default()))
}

/// `GET /api/v1/services/{id}/bandwidth`.
pub async fn bandwidth(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let v = state
        .billing
        .vm_bandwidth(tenant.id, s.id, scope.client())
        .await?;
    Ok(ApiReply::ok(json!({
        "service_id": s.id,
        "counted_gb": v.usage.counted_gb,
        "incoming_gb": v.usage.incoming_gb,
        "outgoing_gb": v.usage.outgoing_gb,
        "estimated": v.usage.estimated,
        "period_start": v.usage.period_start,
        "quota_gb": v.quota_gb,
        "used_percent": v.used_percent,
        "overage_gb": v.overage_gb,
    })))
}

/// `GET /api/v1/services/{id}/operations` — VM actions with progress.
pub async fn operations(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let s = load(&state, &tenant, scope, &id).await?;
    let ops = state
        .billing
        .vm_operations(tenant.id, s.id, scope.client(), 50)
        .await?;
    let staff = scope.client().is_none();
    Ok(super::collection(
        ops.into_iter()
            .map(|o| {
                json!({
                    "id": o.id,
                    "kind": o.kind,
                    "status": o.status,
                    "progress": o.progress,
                    "detail": o.detail,
                    "error": if staff { o.error } else { o.error.map(|_| "provider error".to_owned()) },
                    "created_at": o.created_at,
                    "finished_at": o.finished_at,
                })
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// SSH keys (client profile; staff pass `client_id`)
// ---------------------------------------------------------------------------

fn key_owner(scope: Scope, explicit: Option<&str>) -> ApiResult<hos_core::ids::ClientId> {
    match scope.client() {
        Some(c) => Ok(c),
        None => explicit
            .map(parse_id)
            .transpose()?
            .ok_or_else(|| ApiError::field("client_id", "required for staff")),
    }
}

fn key_json(k: &hos_db::repo::vm_ops::SshKeyRow) -> serde_json::Value {
    json!({
        "id": k.id,
        "client_id": k.client_id,
        "name": k.name,
        "public_key": k.public_key,
        "fingerprint": k.fingerprint,
        "created_at": k.created_at,
    })
}

/// `GET /api/v1/ssh-keys`.
pub async fn ssh_keys(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_VIEW)?;
    let client = key_owner(scope, raw.get("client_id").map(String::as_str))?;
    let keys = state.billing.client_ssh_keys(tenant.id, client).await?;
    Ok(super::collection(keys.iter().map(key_json).collect()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyBody {
    name: String,
    public_key: String,
    #[serde(default)]
    client_id: Option<String>,
}

/// `POST /api/v1/ssh-keys`.
pub async fn add_ssh_key(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let input: KeyBody = parse_body(&body)?;
    let client = key_owner(scope, input.client_id.as_deref())?;
    if scope.client().is_none() {
        // Staff: the client must exist in this tenant.
        hos_db::repo::clients::get_client(&state.pool, tenant.id, client)
            .await?
            .ok_or_else(ApiError::not_found)?;
    }
    rate(&state.limits.vm_action, &format!("sshkey:{}", scope.user()))?;
    let k = state
        .billing
        .add_client_ssh_key(
            tenant.id,
            client,
            &input.name,
            &input.public_key,
            Some(scope.user()),
        )
        .await?;
    Ok(ApiReply::created(key_json(&k)))
}

/// `DELETE /api/v1/ssh-keys/{id}`.
pub async fn delete_ssh_key(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SERVICES_MANAGE)?;
    let client = key_owner(scope, raw.get("client_id").map(String::as_str))?;
    let key: uuid::Uuid = id.parse().map_err(|_| ApiError::not_found())?;
    state
        .billing
        .delete_client_ssh_key(tenant.id, client, key)
        .await?;
    Ok(ApiReply::ok(json!({"id": key, "deleted": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_reinstall_templates_are_client_errors() {
        let e = reinstall_error(CoreError::validation(
            "template 102 is not offered for this service",
        ));
        assert_eq!(e.status, http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(e.code, "validation_failed");
        assert!(e.detail.contains("not offered"));
        let e = reinstall_error(CoreError::Integration(IntegrationError::InvalidInput(
            "102 is not an available QEMU template".into(),
        )));
        assert_eq!(e.status, http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(e.detail.contains("not an available"));
        let e = reinstall_error(CoreError::Forbidden("the service is suspended".into()));
        assert_eq!(e.status, http::StatusCode::FORBIDDEN);
        // Genuine upstream failures keep their mapping.
        let e = reinstall_error(CoreError::Integration(IntegrationError::Timeout));
        assert!(e.status.is_server_error());
    }
}
