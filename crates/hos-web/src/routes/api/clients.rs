//! `/api/v1/clients` — staff (`clients.view` / `clients.manage`); a client
//! key sees only its own record and cannot write.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use hos_auth::rbac::Permission;
use hos_core::domain::Locale;
use hos_core::ids::ClientId;
use hos_core::money::Currency;
use hos_db::enums::ClientKind;
use hos_db::repo::api as api_repo;
use hos_db::repo::clients::{self, ClientInput, ClientRow};
use serde::Deserialize;

use super::{
    ApiAuth, ApiError, ApiReply, ApiResult, ApiTenant, FieldError, Sorts, dto, list_query, paged,
    parse_body, parse_id,
};
use crate::forms;
use crate::routes::platform::limits::{Limit, check_plan_limit};
use crate::state::AppState;

const SORTS: Sorts = &[
    ("created_at", "created_at"),
    ("email", "email"),
    ("last_name", "last_name"),
];
const STATUSES: &[&str] = &["active", "inactive", "closed"];

/// `GET /api/v1/clients`.
pub async fn list(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Query(raw): Query<HashMap<String, String>>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::CLIENTS_VIEW)?;
    let q = list_query(&raw, SORTS, STATUSES, scope)?;
    let page = api_repo::list_clients(&state.pool, tenant.id, &q.filter, q.sort, q.page).await?;
    Ok(paged(&page, dto::client))
}

/// `GET /api/v1/clients/{id}`.
pub async fn show(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
) -> ApiResult<ApiReply> {
    let scope = auth.scope(Permission::CLIENTS_VIEW)?;
    let id: ClientId = parse_id(&id)?;
    scope.owns(Some(id))?;
    let c = clients::get_client(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(ApiReply::ok(dto::client(&c)))
}

/// Create / patch body. On `PATCH` every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientBody {
    kind: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    company_name: Option<String>,
    email: Option<String>,
    phone: Option<String>,
    address_line1: Option<String>,
    address_line2: Option<String>,
    city: Option<String>,
    state: Option<String>,
    postal_code: Option<String>,
    country_code: Option<String>,
    tax_office: Option<String>,
    tax_number: Option<String>,
    currency: Option<String>,
    locale: Option<String>,
    tax_exempt: Option<bool>,
    marketing_opt_in: Option<bool>,
}

fn opt(v: Option<String>, current: Option<String>) -> Option<String> {
    match v {
        Some(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        None => current,
    }
}

/// Merges `body` over `base` (or defaults) and validates.
fn to_input(
    body: ClientBody,
    base: Option<&ClientRow>,
    default_currency: Currency,
) -> Result<ClientInput, ApiError> {
    let mut errors = Vec::new();
    let mut err = |field: &str, msg: &str| {
        errors.push(FieldError {
            field: field.to_owned(),
            message: msg.to_owned(),
        });
    };
    let kind = match body.kind.as_deref() {
        None => base.map_or(ClientKind::Individual, |b| b.kind),
        Some(k) => k.parse().unwrap_or_else(|_| {
            err("kind", "must be `individual` or `corporate`");
            ClientKind::Individual
        }),
    };
    let first_name = body
        .first_name
        .map(|s| forms::clean(&s, 100))
        .or_else(|| base.map(|b| b.first_name.clone()))
        .unwrap_or_default();
    if first_name.is_empty() {
        err("first_name", "required");
    }
    let last_name = body
        .last_name
        .map(|s| forms::clean(&s, 100))
        .or_else(|| base.map(|b| b.last_name.clone()))
        .unwrap_or_default();
    if last_name.is_empty() {
        err("last_name", "required");
    }
    let email = body
        .email
        .map(|e| forms::normalize_email(&e))
        .or_else(|| base.map(|b| b.email.clone()))
        .unwrap_or_default();
    if !forms::valid_email(&email) {
        err("email", "must be a valid e-mail address");
    }
    let company_name = opt(body.company_name, base.and_then(|b| b.company_name.clone()));
    if kind == ClientKind::Corporate && company_name.is_none() {
        err("company_name", "required for corporate clients");
    }
    let country_code = body
        .country_code
        .map(|c| c.trim().to_ascii_uppercase())
        .or_else(|| base.map(|b| b.country_code.clone()))
        .unwrap_or_else(|| "TR".to_owned());
    if country_code.len() != 2 || !country_code.chars().all(|c| c.is_ascii_uppercase()) {
        err("country_code", "must be an ISO 3166-1 alpha-2 code");
    }
    let currency = match body.currency.as_deref() {
        None => base.map_or(default_currency, |b| b.currency),
        Some(c) => c.trim().to_ascii_uppercase().parse().unwrap_or_else(|_| {
            err("currency", "unknown ISO 4217 code");
            default_currency
        }),
    };
    let locale = match body.locale.as_deref() {
        None => base.map_or(Locale::Tr, |b| b.locale),
        Some(l) => l.trim().parse().unwrap_or_else(|_| {
            err("locale", "must be `tr` or `en`");
            Locale::Tr
        }),
    };
    if !errors.is_empty() {
        return Err(ApiError::validation(errors));
    }
    Ok(ClientInput {
        kind,
        first_name,
        last_name,
        company_name,
        email,
        phone: opt(body.phone, base.and_then(|b| b.phone.clone())),
        address_line1: opt(
            body.address_line1,
            base.and_then(|b| b.address_line1.clone()),
        ),
        address_line2: opt(
            body.address_line2,
            base.and_then(|b| b.address_line2.clone()),
        ),
        city: opt(body.city, base.and_then(|b| b.city.clone())),
        state: opt(body.state, base.and_then(|b| b.state.clone())),
        postal_code: opt(body.postal_code, base.and_then(|b| b.postal_code.clone())),
        country_code,
        tax_office: opt(body.tax_office, base.and_then(|b| b.tax_office.clone())),
        tax_number: opt(body.tax_number, base.and_then(|b| b.tax_number.clone())),
        national_id: base.and_then(|b| b.national_id.clone()),
        currency,
        locale,
        group_id: base.and_then(|b| b.group_id),
        tax_exempt: body
            .tax_exempt
            .unwrap_or_else(|| base.is_some_and(|b| b.tax_exempt)),
        marketing_opt_in: body
            .marketing_opt_in
            .unwrap_or_else(|| base.is_some_and(|b| b.marketing_opt_in)),
    })
}

fn email_conflict(e: hos_core::CoreError) -> ApiError {
    match e {
        hos_core::CoreError::Conflict(_) => {
            ApiError::field("email", "a client with this e-mail exists")
        }
        other => other.into(),
    }
}

/// `POST /api/v1/clients` (`clients.manage`; counts against the plan's
/// `max_clients`).
pub async fn create(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    body: Bytes,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::CLIENTS_MANAGE)?;
    let input = to_input(parse_body(&body)?, None, tenant.default_currency)?;
    check_plan_limit(&state, tenant.id, Limit::Clients).await?;
    let row = clients::create_client(&state.pool, tenant.id, &input)
        .await
        .map_err(email_conflict)?;
    Ok(ApiReply::created(dto::client(&row)))
}

/// `PATCH /api/v1/clients/{id}` (`clients.manage`).
pub async fn update(
    State(state): State<AppState>,
    tenant: ApiTenant,
    auth: ApiAuth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<ApiReply> {
    auth.staff(Permission::CLIENTS_MANAGE)?;
    let id: ClientId = parse_id(&id)?;
    let current = clients::get_client(&state.pool, tenant.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let input = to_input(parse_body(&body)?, Some(&current), tenant.default_currency)?;
    let row = clients::update_client(&state.pool, tenant.id, id, &input)
        .await
        .map_err(email_conflict)?
        .ok_or_else(ApiError::not_found)?;
    Ok(ApiReply::ok(dto::client(&row)))
}
