//! Configured provider modules (`integrations`, docs/ARCHITECTURE.md §4).
//!
//! `secrets_enc` is opaque here: callers encrypt with
//! `SecretBox::encrypt_json(&secrets, &integration_aad(tenant_id, id))`, which
//! is why [`NewIntegration`] carries a caller-generated id.

use chrono::{DateTime, Utc};
use hos_core::domain::IntegrationCategory;
use hos_core::ids::{IntegrationId, TenantId};
use sqlx::{FromRow, PgConnection, PgExecutor};

use super::Json;
use crate::error::{DbResult, conflict_on_unique};

/// `integrations` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct IntegrationRow {
    pub id: IntegrationId,
    pub tenant_id: TenantId,
    pub category: IntegrationCategory,
    pub kind: String,
    pub name: String,
    pub config: Json,
    pub secrets_enc: Option<Vec<u8>>,
    pub is_active: bool,
    pub is_default: bool,
    pub sort_order: i32,
    pub last_test_at: Option<DateTime<Utc>>,
    pub last_test_ok: Option<bool>,
    pub last_test_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input of [`create_integration`].
#[derive(Debug, Clone)]
pub struct NewIntegration {
    /// Generate first (`IntegrationId::new()`) so the secrets AAD can include it.
    pub id: IntegrationId,
    pub category: IntegrationCategory,
    /// `hos_core::integrations::kinds::*`.
    pub kind: String,
    pub name: String,
    pub config: Json,
    pub secrets_enc: Option<Vec<u8>>,
    pub is_active: bool,
    pub sort_order: i32,
}

/// Creates an integration.
pub async fn create_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    new: &NewIntegration,
) -> DbResult<IntegrationRow> {
    sqlx::query_as::<_, IntegrationRow>(
        "insert into integrations (id, tenant_id, category, kind, name, config, secrets_enc, is_active, sort_order)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning *",
    )
    .bind(new.id)
    .bind(tenant_id)
    .bind(new.category)
    .bind(&new.kind)
    .bind(&new.name)
    .bind(&new.config)
    .bind(&new.secrets_enc)
    .bind(new.is_active)
    .bind(new.sort_order)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("integration name"))
}

/// Integration by id.
pub async fn get_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IntegrationId,
) -> DbResult<Option<IntegrationRow>> {
    Ok(sqlx::query_as::<_, IntegrationRow>(
        "select * from integrations where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Integrations of a tenant, optionally of one category.
pub async fn list_integrations(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    category: Option<IntegrationCategory>,
) -> DbResult<Vec<IntegrationRow>> {
    Ok(sqlx::query_as::<_, IntegrationRow>(
        "select * from integrations where tenant_id = $1 and ($2::text is null or category = $2)
         order by category, sort_order, name",
    )
    .bind(tenant_id)
    .bind(category)
    .fetch_all(db)
    .await?)
}

/// Active integrations of a category (e.g. the gateways offered at checkout).
pub async fn active_integrations(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    category: IntegrationCategory,
) -> DbResult<Vec<IntegrationRow>> {
    Ok(sqlx::query_as::<_, IntegrationRow>(
        "select * from integrations where tenant_id = $1 and category = $2 and is_active
         order by is_default desc, sort_order, name",
    )
    .bind(tenant_id)
    .bind(category)
    .fetch_all(db)
    .await?)
}

/// The active default of a category, else the first active one.
pub async fn default_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    category: IntegrationCategory,
) -> DbResult<Option<IntegrationRow>> {
    Ok(sqlx::query_as::<_, IntegrationRow>(
        "select * from integrations where tenant_id = $1 and category = $2 and is_active
         order by is_default desc, sort_order, created_at limit 1",
    )
    .bind(tenant_id)
    .bind(category)
    .fetch_optional(db)
    .await?)
}

/// The active integration of a category and kind (e.g. `oauth` + `google`:
/// the tenant's own OAuth app).
pub async fn active_by_kind(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    category: IntegrationCategory,
    kind: &str,
) -> DbResult<Option<IntegrationRow>> {
    Ok(sqlx::query_as::<_, IntegrationRow>(
        "select * from integrations where tenant_id = $1 and category = $2 and kind = $3 and is_active
         order by is_default desc, sort_order, created_at limit 1",
    )
    .bind(tenant_id)
    .bind(category)
    .bind(kind)
    .fetch_optional(db)
    .await?)
}

/// Updates name, config and active flag.
pub async fn update_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IntegrationId,
    name: &str,
    config: &Json,
    is_active: bool,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update integrations set name = $3, config = $4, is_active = $5 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(name)
    .bind(config)
    .bind(is_active)
    .execute(db)
    .await
    .map_err(conflict_on_unique("integration name"))?;
    Ok(res.rows_affected() == 1)
}

/// Replaces the encrypted secrets.
pub async fn set_integration_secrets(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IntegrationId,
    secrets_enc: Option<&[u8]>,
) -> DbResult<bool> {
    let res =
        sqlx::query("update integrations set secrets_enc = $3 where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .bind(secrets_enc)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Stores the outcome of `test_connection`.
pub async fn record_test_result(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IntegrationId,
    ok: bool,
    message: Option<&str>,
) -> DbResult<()> {
    sqlx::query(
        "update integrations set last_test_at = now(), last_test_ok = $3, last_test_message = $4
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(ok)
    .bind(message)
    .execute(db)
    .await?;
    Ok(())
}

/// Makes `id` the default of its category (transactional).
pub async fn set_default_integration(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: IntegrationId,
) -> DbResult<bool> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let category: Option<IntegrationCategory> = sqlx::query_scalar(
        "select category from integrations where tenant_id = $1 and id = $2 for update",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(category) = category else {
        return Ok(false);
    };
    sqlx::query("update integrations set is_default = false where tenant_id = $1 and category = $2 and is_default")
        .bind(tenant_id)
        .bind(category)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update integrations set is_default = true where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Deletes an integration (references are set null).
pub async fn delete_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IntegrationId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from integrations where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}
