//! Inbound webhooks (idempotency store) and outbound webhook endpoints.

use chrono::{DateTime, Utc};
use hos_core::ids::{IntegrationId, TenantId, WebhookEndpointId, WebhookInId};
use sqlx::{FromRow, PgExecutor};

use super::Json;
use crate::enums::WebhookInStatus;
use crate::error::DbResult;

/// `webhooks_in` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct WebhookInRow {
    pub id: WebhookInId,
    pub tenant_id: TenantId,
    pub integration_id: IntegrationId,
    pub external_event_id: String,
    pub event_type: Option<String>,
    pub payload: Json,
    pub headers: Json,
    pub signature_valid: bool,
    pub status: WebhookInStatus,
    pub attempts: i32,
    pub error: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct WebhookInFlag {
    #[sqlx(flatten)]
    row: WebhookInRow,
    is_new: bool,
}

/// Stores an incoming event; idempotent on `(integration_id, external_event_id)`.
/// Returns `(row, is_new)` — when `is_new` is false the event was seen before
/// and must not be processed again (check `row.status`).
#[allow(clippy::too_many_arguments)]
pub async fn record_incoming(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    external_event_id: &str,
    event_type: Option<&str>,
    payload: &Json,
    headers: &Json,
    signature_valid: bool,
) -> DbResult<(WebhookInRow, bool)> {
    let r = sqlx::query_as::<_, WebhookInFlag>(
        "with ins as (
             insert into webhooks_in (id, tenant_id, integration_id, external_event_id, event_type, payload, headers,
                                      signature_valid)
             values ($1, $2, $3, $4, $5, $6, $7, $8)
             on conflict (integration_id, external_event_id) do nothing
             returning *
         )
         select ins.*, true as is_new from ins
         union all
         select w.*, false as is_new from webhooks_in w
         where not exists (select 1 from ins) and w.tenant_id = $2 and w.integration_id = $3
           and w.external_event_id = $4",
    )
    .bind(WebhookInId::new())
    .bind(tenant_id)
    .bind(integration_id)
    .bind(external_event_id)
    .bind(event_type)
    .bind(payload)
    .bind(headers)
    .bind(signature_valid)
    .fetch_one(db)
    .await?;
    Ok((r.row, r.is_new))
}

/// Stores a request that failed authentication (or carries none) as a row
/// with `signature_valid = false` and keeps only the newest `keep` such rows
/// of the integration: an anonymous sender can neither grow the table nor
/// push out verified events. The caller passes a summary (size, hash),
/// never the body.
#[allow(clippy::too_many_arguments)]
pub async fn record_unverified(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    integration_id: IntegrationId,
    event_type: &str,
    status: WebhookInStatus,
    summary: &Json,
    headers: &Json,
    error: Option<&str>,
    keep: i64,
) -> DbResult<()> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query(
        "insert into webhooks_in (id, tenant_id, integration_id, external_event_id, event_type, payload,
                                  headers, signature_valid, status, error, attempts, processed_at)
         values ($1, $2, $3, $4, $5, $6, $7, false, $8, $9, 1,
                 case when $8 in ('processed', 'ignored') then now() end)",
    )
    .bind(WebhookInId::new())
    .bind(tenant_id)
    .bind(integration_id)
    .bind(format!("req:{}", uuid::Uuid::now_v7()))
    .bind(event_type)
    .bind(summary)
    .bind(headers)
    .bind(status)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "delete from webhooks_in where id in (
             select id from webhooks_in
              where tenant_id = $1 and integration_id = $2 and not signature_valid
                and external_event_id like 'req:%'
              order by created_at desc, id desc offset $3)",
    )
    .bind(tenant_id)
    .bind(integration_id)
    .bind(keep.max(1))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// One page of stored payloads (of one tenant, or all), by id after
/// `after`: the input of the payload scrub (`hostingos webhooks scrub`).
pub async fn payload_page(
    db: impl PgExecutor<'_>,
    tenant_id: Option<TenantId>,
    after: Option<WebhookInId>,
    limit: i64,
) -> DbResult<Vec<(WebhookInId, TenantId, Json)>> {
    Ok(sqlx::query_as::<_, (WebhookInId, TenantId, Json)>(
        "select id, tenant_id, payload from webhooks_in
          where ($1::uuid is null or tenant_id = $1) and ($2::uuid is null or id > $2)
          order by id limit $3",
    )
    .bind(tenant_id)
    .bind(after)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Replaces a stored payload (scrubbed copy of the same row).
pub async fn replace_payload(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookInId,
    payload: &Json,
) -> DbResult<bool> {
    let res = sqlx::query("update webhooks_in set payload = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(payload)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Locks an incoming event row (`FOR UPDATE`) inside the transaction that
/// applies it, so concurrent deliveries of the same event are applied once:
/// the second waits, then sees `processed` / `ignored`.
pub async fn lock_incoming(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    id: WebhookInId,
) -> DbResult<Option<WebhookInRow>> {
    Ok(sqlx::query_as::<_, WebhookInRow>(
        "select * from webhooks_in where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Sets the processing outcome (`processed`, `failed` with error, `ignored`).
pub async fn set_webhook_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookInId,
    status: WebhookInStatus,
    error: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update webhooks_in set status = $3, error = $4, attempts = attempts + 1,
                processed_at = case when $3 in ('processed', 'ignored') then now() else processed_at end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Received / failed events to (re)process, oldest first.
pub async fn pending_webhooks(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    max_attempts: i32,
    limit: i64,
) -> DbResult<Vec<WebhookInRow>> {
    Ok(sqlx::query_as::<_, WebhookInRow>(
        "select * from webhooks_in
         where tenant_id = $1 and status in ('received', 'failed') and attempts < $2
         order by created_at limit $3",
    )
    .bind(tenant_id)
    .bind(max_attempts)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// `webhook_endpoints` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct WebhookEndpointRow {
    pub id: WebhookEndpointId,
    pub tenant_id: TenantId,
    pub url: String,
    pub description: Option<String>,
    pub secret_enc: Option<Vec<u8>>,
    pub events: Vec<String>,
    pub is_active: bool,
    pub last_delivery_at: Option<DateTime<Utc>>,
    pub last_status_code: Option<i32>,
    pub failure_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates an outbound endpoint.
pub async fn create_endpoint(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    url: &str,
    description: Option<&str>,
    secret_enc: Option<&[u8]>,
    events: &[String],
) -> DbResult<WebhookEndpointRow> {
    Ok(sqlx::query_as::<_, WebhookEndpointRow>(
        "insert into webhook_endpoints (id, tenant_id, url, description, secret_enc, events)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(WebhookEndpointId::new())
    .bind(tenant_id)
    .bind(url)
    .bind(description)
    .bind(secret_enc)
    .bind(events)
    .fetch_one(db)
    .await?)
}

/// Active endpoints subscribed to `event` (empty `events` = all).
pub async fn endpoints_for_event(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    event: &str,
) -> DbResult<Vec<WebhookEndpointRow>> {
    Ok(sqlx::query_as::<_, WebhookEndpointRow>(
        "select * from webhook_endpoints
         where tenant_id = $1 and is_active and (cardinality(events) = 0 or $2 = any(events))",
    )
    .bind(tenant_id)
    .bind(event)
    .fetch_all(db)
    .await?)
}

/// All endpoints of a tenant.
pub async fn list_endpoints(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<WebhookEndpointRow>> {
    Ok(sqlx::query_as::<_, WebhookEndpointRow>(
        "select * from webhook_endpoints where tenant_id = $1 order by created_at",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Records a delivery attempt (success resets the failure counter).
pub async fn record_delivery(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookEndpointId,
    status_code: Option<i32>,
    ok: bool,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update webhook_endpoints set last_delivery_at = now(), last_status_code = $3,
                failure_count = case when $4 then 0 else failure_count + 1 end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status_code)
    .bind(ok)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}
