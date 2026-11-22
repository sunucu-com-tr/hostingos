//! `api_idempotency_keys`: replay protection of REST API `POST`s carrying an
//! `Idempotency-Key` header (tenant scoped, per caller).
//!
//! 1. [`begin`] reserves the key (or reports a stored response / a request
//!    still in flight / a key reused for another request);
//! 2. the handler runs;
//! 3. [`complete`] stores the response (2xx–4xx) — or [`release`] drops the
//!    reservation after a server error so the client can retry.

use chrono::{DateTime, Utc};
use hos_core::ids::TenantId;
use sqlx::{FromRow, PgExecutor};
use uuid::Uuid;

use super::Json;
use crate::error::DbResult;

/// Outcome of [`begin`].
#[derive(Debug, Clone, PartialEq)]
pub enum IdempotencyBegin {
    /// Reserved: run the request, then [`complete`] / [`release`] this id.
    New(Uuid),
    /// Already completed: answer with the stored response.
    Replay { status: i32, body: Json },
    /// The first request with this key is still running.
    InProgress,
    /// The key was used for a different request (method, path or body).
    Mismatch,
}

#[derive(FromRow)]
struct Existing {
    id: Uuid,
    request_hash: String,
    status: String,
    response_status: Option<i32>,
    response_body: Option<Json>,
    expires_at: DateTime<Utc>,
}

/// Reserves `key` for `principal` (see module docs). Expired entries are
/// replaced.
pub async fn begin(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    principal: &str,
    key: &str,
    method: &str,
    path: &str,
    request_hash: &str,
) -> DbResult<IdempotencyBegin> {
    for _ in 0..2 {
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "insert into api_idempotency_keys (id, tenant_id, principal, idempotency_key, method, path, request_hash)
             values ($1, $2, $3, $4, $5, $6, $7)
             on conflict (tenant_id, principal, idempotency_key) do nothing
             returning id",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(principal)
        .bind(key)
        .bind(method)
        .bind(path)
        .bind(request_hash)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = inserted {
            return Ok(IdempotencyBegin::New(id));
        }
        let existing = sqlx::query_as::<_, Existing>(
            "select id, request_hash, status, response_status, response_body, expires_at
               from api_idempotency_keys
              where tenant_id = $1 and principal = $2 and idempotency_key = $3",
        )
        .bind(tenant_id)
        .bind(principal)
        .bind(key)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(e) = existing else {
            continue; // released in between: try to insert again
        };
        if e.expires_at <= Utc::now() {
            sqlx::query("delete from api_idempotency_keys where id = $1")
                .bind(e.id)
                .execute(&mut *conn)
                .await?;
            continue;
        }
        if e.request_hash != request_hash {
            return Ok(IdempotencyBegin::Mismatch);
        }
        return Ok(match (e.status.as_str(), e.response_status) {
            ("completed", Some(status)) => IdempotencyBegin::Replay {
                status,
                body: e.response_body.unwrap_or(Json::Null),
            },
            _ => IdempotencyBegin::InProgress,
        });
    }
    Ok(IdempotencyBegin::InProgress)
}

/// Stores the response of a reserved key.
pub async fn complete(db: impl PgExecutor<'_>, id: Uuid, status: i32, body: &Json) -> DbResult<()> {
    sqlx::query(
        "update api_idempotency_keys set status = 'completed', response_status = $2, response_body = $3
          where id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(body)
    .execute(db)
    .await?;
    Ok(())
}

/// Drops a reservation (the request failed with a server error).
pub async fn release(db: impl PgExecutor<'_>, id: Uuid) -> DbResult<()> {
    sqlx::query("delete from api_idempotency_keys where id = $1 and status = 'processing'")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Deletes expired entries; returns how many.
pub async fn purge_expired(db: impl PgExecutor<'_>) -> DbResult<u64> {
    Ok(
        sqlx::query("delete from api_idempotency_keys where expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}
