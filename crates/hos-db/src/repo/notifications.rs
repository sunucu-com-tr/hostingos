//! In-app notifications for users (bell icon).

use chrono::{DateTime, Utc};
use hos_core::ids::{NotificationId, TenantId, UserId};
use sqlx::{FromRow, PgExecutor};

use super::Json;
use crate::error::DbResult;

/// `notifications` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct NotificationRow {
    pub id: NotificationId,
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub kind: String,
    pub title: String,
    pub body: Option<String>,
    pub link_url: Option<String>,
    pub data: Json,
    pub read_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Creates a notification.
#[allow(clippy::too_many_arguments)]
pub async fn notify(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    kind: &str,
    title: &str,
    body: Option<&str>,
    link_url: Option<&str>,
    data: &Json,
) -> DbResult<NotificationRow> {
    Ok(sqlx::query_as::<_, NotificationRow>(
        "insert into notifications (id, tenant_id, user_id, kind, title, body, link_url, data)
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning *",
    )
    .bind(NotificationId::new())
    .bind(tenant_id)
    .bind(user_id)
    .bind(kind)
    .bind(title)
    .bind(body)
    .bind(link_url)
    .bind(data)
    .fetch_one(db)
    .await?)
}

/// Latest notifications of a user.
pub async fn list_notifications(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    unread_only: bool,
    limit: i64,
) -> DbResult<Vec<NotificationRow>> {
    Ok(sqlx::query_as::<_, NotificationRow>(
        "select * from notifications where tenant_id = $1 and user_id = $2 and (not $3 or read_at is null)
         order by created_at desc limit $4",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(unread_only)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Unread count.
pub async fn unread_count(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from notifications where tenant_id = $1 and user_id = $2 and read_at is null",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_one(db)
    .await?)
}

/// Marks one notification read.
pub async fn mark_read(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    id: NotificationId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update notifications set read_at = coalesce(read_at, now()) where tenant_id = $1 and user_id = $2 and id = $3",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Marks all of a user's notifications read.
pub async fn mark_all_read(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<u64> {
    let res = sqlx::query(
        "update notifications set read_at = now() where tenant_id = $1 and user_id = $2 and read_at is null",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}
