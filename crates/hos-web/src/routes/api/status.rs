//! `/api/v1/status*` — the public network status (no authentication): the
//! public components with their state, the overall level, open public
//! incidents / maintenance and the public history of the last 90 days. The
//! same data as `/status`; nothing that is not public there leaves here.

use axum::extract::{Path, State};
use chrono::{Duration, Utc};
use hos_db::repo::status::{
    self as srepo, ComponentRow, IncidentId, IncidentRow, StatusComponentId,
};
use serde_json::{Value, json};

use super::{ApiError, ApiReply, ApiResult, ApiTenant, collection, parse_id};
use crate::routes::storefront::status::overall_level;
use crate::state::AppState;

fn component_json(c: &ComponentRow) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "group": c.group_name,
        "description": c.description,
        "status": c.status.as_str(),
        "updated_at": c.updated_at,
    })
}

/// An incident; `public` are the public components (others are left out).
async fn incident_json(
    state: &AppState,
    tenant: &ApiTenant,
    i: &IncidentRow,
    public: &[StatusComponentId],
    with_updates: bool,
) -> ApiResult<Value> {
    let components: Vec<Value> = srepo::components_of_incidents(&state.pool, tenant.id, &[i.id])
        .await?
        .into_iter()
        .filter(|(_, c)| public.contains(c))
        .map(|(_, c)| json!(c))
        .collect();
    let mut v = json!({
        "id": i.id,
        "kind": i.kind.as_str(),
        "title": i.title,
        "impact": i.impact.as_str(),
        "status": i.status.as_str(),
        "scheduled_start": i.scheduled_start,
        "scheduled_end": i.scheduled_end,
        "started_at": i.started_at,
        "resolved_at": i.resolved_at,
        "component_ids": components,
        "updated_at": i.updated_at,
    });
    if with_updates {
        v["updates"] = srepo::incident_updates(&state.pool, tenant.id, i.id)
            .await?
            .iter()
            .map(|u| {
                json!({
                    "status": u.status.as_str(),
                    "body": u.body,
                    "created_at": u.created_at,
                })
            })
            .collect();
    }
    Ok(v)
}

async fn public_ids(state: &AppState, tenant: &ApiTenant) -> ApiResult<Vec<StatusComponentId>> {
    Ok(srepo::list_components(&state.pool, tenant.id, true)
        .await?
        .iter()
        .map(|c| c.id)
        .collect())
}

/// `GET /api/v1/status` — components, overall level and open incidents.
pub async fn summary(State(state): State<AppState>, tenant: ApiTenant) -> ApiResult<ApiReply> {
    let components = srepo::list_components(&state.pool, tenant.id, true).await?;
    let public: Vec<StatusComponentId> = components.iter().map(|c| c.id).collect();
    let open = srepo::list_incidents(&state.pool, tenant.id, true, true, 50).await?;
    let mut incidents = Vec::with_capacity(open.len());
    for i in &open {
        incidents.push(incident_json(&state, &tenant, i, &public, false).await?);
    }
    Ok(ApiReply::ok(json!({
        "level": overall_level(&components, &open),
        "components": components.iter().map(component_json).collect::<Vec<_>>(),
        "incidents": incidents,
    })))
}

/// `GET /api/v1/status/incidents` — public incidents of the last 90 days.
pub async fn incidents(State(state): State<AppState>, tenant: ApiTenant) -> ApiResult<ApiReply> {
    let rows = srepo::incidents_since(&state.pool, tenant.id, Utc::now() - Duration::days(90), 100)
        .await?;
    let public = public_ids(&state, &tenant).await?;
    let mut out = Vec::with_capacity(rows.len());
    for i in &rows {
        out.push(incident_json(&state, &tenant, i, &public, false).await?);
    }
    Ok(collection(out))
}

/// `GET /api/v1/status/incidents/{id}` — one public incident with its timeline.
pub async fn incident(
    State(state): State<AppState>,
    tenant: ApiTenant,
    Path(raw): Path<String>,
) -> ApiResult<ApiReply> {
    let id: IncidentId = parse_id(&raw)?;
    let row = srepo::get_incident(&state.pool, tenant.id, id)
        .await?
        .filter(|i| i.is_public)
        .ok_or_else(ApiError::not_found)?;
    let public = public_ids(&state, &tenant).await?;
    Ok(ApiReply::ok(
        incident_json(&state, &tenant, &row, &public, true).await?,
    ))
}
