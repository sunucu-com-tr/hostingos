//! Support tickets. Staff: `support.view` (read) / `support.manage`
//! (create, reply, internal notes). Clients: their own tickets, internal
//! notes never shown.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use hos_auth::rbac::Permission;
use hos_core::domain::TicketPriority;
use hos_core::ids::{ClientId, DomainId, ServiceId, TicketDepartmentId, TicketId};
use hos_db::enums::{AuthorKind, MessageSource};
use hos_db::repo::support::{self, NewTicket, NewTicketMessage};
use hos_db::repo::{api as api_repo, domains, identity, services};
use serde::Deserialize;
use serde_json::json;

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, Scope, Sorts, dto, list_query, paged,
    parse_body, parse_id,
};
use crate::extract::ClientIp;
use crate::state::AppState;

const SORTS: Sorts = &[
    ("last_reply_at", "last_reply_at"),
    ("created_at", "created_at"),
    ("priority", "priority"),
];
const STATUSES: &[&str] = &[
    "open",
    "answered",
    "customer_reply",
    "in_progress",
    "on_hold",
    "closed",
];
const MAX_BODY_CHARS: usize = 20_000;

/// `GET /api/v1/departments` — departments a caller may open tickets in.
pub async fn departments(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SUPPORT_VIEW)?;
    let rows = support::list_departments(&state.pool, tenant.id, scope.client().is_none()).await?;
    Ok(super::collection(
        rows.iter().map(dto::department).collect(),
    ))
}

/// `GET /api/v1/tickets`.
pub async fn list(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SUPPORT_VIEW)?;
    let q = list_query(&raw, SORTS, STATUSES, scope)?;
    let page = api_repo::list_tickets(&state.pool, tenant.id, &q.filter, q.sort, q.page).await?;
    Ok(paged(&page, dto::ticket))
}

async fn load(
    state: &AppState,
    tenant: &ApiTenant,
    scope: Scope,
    raw: &str,
) -> ApiResult<hos_db::repo::support::TicketRow> {
    let id: TicketId = parse_id(raw)?;
    let t = support::get_ticket(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    scope.owns(t.client_id)?;
    Ok(t)
}

/// `GET /api/v1/tickets/{id}` — with its messages.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SUPPORT_VIEW)?;
    let t = load(&state, &tenant, scope, &id).await?;
    let msgs =
        support::ticket_messages(&state.pool, tenant.id, t.id, scope.client().is_none()).await?;
    let mut v = dto::ticket(&t);
    v["messages"] = msgs.iter().map(dto::ticket_message).collect();
    Ok(ApiReply::ok(v))
}

/// `GET /api/v1/tickets/{id}/messages`.
pub async fn messages(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SUPPORT_VIEW)?;
    let t = load(&state, &tenant, scope, &id).await?;
    let msgs =
        support::ticket_messages(&state.pool, tenant.id, t.id, scope.client().is_none()).await?;
    Ok(super::collection(
        msgs.iter().map(dto::ticket_message).collect(),
    ))
}

/// Author of a message written by the caller.
async fn author(
    state: &AppState,
    tenant: &ApiTenant,
    scope: Scope,
) -> ApiResult<(AuthorKind, String, Option<String>)> {
    let user = identity::get_user(&state.pool, tenant.id, scope.user())
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let kind = match scope {
        Scope::Staff(_) => AuthorKind::Staff,
        Scope::Client { .. } => AuthorKind::Client,
    };
    Ok((kind, user.display_name(), Some(user.email.clone())))
}

fn message_body(raw: &str) -> ApiResult<String> {
    let body = raw.trim();
    if body.is_empty() {
        return Err(ApiError::field("message", "required"));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(ApiError::field("message", "at most 20 000 characters"));
    }
    Ok(body.to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    subject: String,
    message: String,
    #[serde(default)]
    department_id: Option<TicketDepartmentId>,
    #[serde(default)]
    priority: Option<String>,
    /// Staff only (required for staff).
    #[serde(default)]
    client_id: Option<ClientId>,
    #[serde(default)]
    service_id: Option<ServiceId>,
    #[serde(default)]
    domain_id: Option<DomainId>,
}

/// `POST /api/v1/tickets`.
pub async fn create(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    ip: ClientIp,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SUPPORT_MANAGE)?;
    let input: CreateBody = parse_body(&body)?;
    let subject = crate::forms::clean(&input.subject, 200);
    if subject.is_empty() {
        return Err(ApiError::field("subject", "required"));
    }
    let message = message_body(&input.message)?;
    let client_id = match scope {
        Scope::Client { client_id, .. } => client_id,
        Scope::Staff(_) => input
            .client_id
            .ok_or_else(|| ApiError::field("client_id", "required"))?,
    };
    if hos_db::repo::clients::get_client(&state.pool, tenant.id, client_id)
        .await?
        .is_none()
    {
        return Err(ApiError::field("client_id", "unknown client"));
    }
    let priority = match input.priority.as_deref() {
        None => TicketPriority::Medium,
        Some(p) => p
            .parse()
            .map_err(|_| ApiError::field("priority", "low, medium, high or urgent"))?,
    };
    let depts = support::list_departments(&state.pool, tenant.id, scope.client().is_none()).await?;
    let department = match input.department_id {
        Some(d) => depts
            .iter()
            .find(|x| x.id == d)
            .ok_or_else(|| ApiError::field("department_id", "unknown department"))?,
        None => depts
            .first()
            .ok_or_else(|| ApiError::field("department_id", "no support department configured"))?,
    };
    if let Some(sid) = input.service_id {
        let s = services::get_service(&state.pool, tenant.id, sid).await?;
        if s.is_none_or(|s| s.client_id != client_id) {
            return Err(ApiError::field("service_id", "unknown service"));
        }
    }
    if let Some(did) = input.domain_id {
        let d = domains::get_domain(&state.pool, tenant.id, did).await?;
        if d.is_none_or(|d| d.client_id != client_id) {
            return Err(ApiError::field("domain_id", "unknown domain"));
        }
    }
    let (kind, name, email) = author(&state, &tenant, scope).await?;
    let mut conn = state.pool.acquire().await?;
    let (ticket, msg) = support::open_ticket(
        &mut conn,
        tenant.id,
        &NewTicket {
            department_id: department.id,
            client_id: Some(client_id),
            user_id: Some(scope.user()),
            guest_name: None,
            guest_email: None,
            subject,
            priority,
            service_id: input.service_id,
            domain_id: input.domain_id,
            first_message: NewTicketMessage {
                author_kind: kind,
                user_id: Some(scope.user()),
                author_name: name,
                author_email: email,
                body: message,
                is_internal: false,
                source: MessageSource::Api,
                message_id: None,
                ip: Some(ip.0),
            },
        },
    )
    .await?;
    let mut v = dto::ticket(&ticket);
    v["messages"] = json!([dto::ticket_message(&msg)]);
    Ok(ApiReply::created(v))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyBody {
    message: String,
    /// Staff only: an internal note.
    #[serde(default)]
    internal: bool,
}

/// `POST /api/v1/tickets/{id}/messages`.
pub async fn reply(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    ip: ClientIp,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::SUPPORT_MANAGE)?;
    let t = load(&state, &tenant, scope, &id).await?;
    let input: ReplyBody = parse_body(&body)?;
    if input.internal && scope.client().is_some() {
        return Err(ApiError::field(
            "internal",
            "clients cannot write internal notes",
        ));
    }
    let message = message_body(&input.message)?;
    let (kind, name, email) = author(&state, &tenant, scope).await?;
    let mut conn = state.pool.acquire().await?;
    let (msg, ticket) = support::reply_to_ticket(
        &mut conn,
        tenant.id,
        t.id,
        &NewTicketMessage {
            author_kind: kind,
            user_id: Some(scope.user()),
            author_name: name,
            author_email: email,
            body: message,
            is_internal: input.internal,
            source: MessageSource::Api,
            message_id: None,
            ip: Some(ip.0),
        },
    )
    .await?;
    Ok(ApiReply::created(json!({
        "ticket": dto::ticket(&ticket),
        "message": dto::ticket_message(&msg),
    })))
}
