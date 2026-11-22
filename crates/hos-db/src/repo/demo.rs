//! The public demo (`HOS_DEMO_MODE`, docs/DEMO.md): the queries of the
//! scheduled reset and of the startup check. Everything tenant-scoped is
//! removed by deleting the tenant row (every tenant table cascades); the rest
//! here is the platform-level remainder of the demo accounts.

use hos_core::domain::ProductType;
use hos_core::ids::{PlatformAdminId, ProductId, TenantId};
use sqlx::{PgConnection, PgExecutor};

use crate::error::DbResult;

/// Serialises demo resets across processes for the rest of the transaction
/// (`pg_advisory_xact_lock`).
pub async fn lock_reset(conn: &mut PgConnection) -> DbResult<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('hostingos:demo-reset', 0))")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Deletes the tenant `slug` with all its data (sessions, users, clients,
/// orders, jobs, `email_log` … cascade) and returns its id.
pub async fn delete_tenant_by_slug(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> DbResult<Option<TenantId>> {
    Ok(
        sqlx::query_scalar("delete from tenants where slug = $1 returning id")
            .bind(slug)
            .fetch_optional(db)
            .await?,
    )
}

/// Slugs of the tenants other than `slug` (at most `limit`), for the
/// startup check that keeps demo mode away from a real database.
pub async fn other_tenant_slugs(
    db: impl PgExecutor<'_>,
    slug: &str,
    limit: i64,
) -> DbResult<Vec<String>> {
    Ok(
        sqlx::query_scalar(
            "select slug from tenants where slug <> $1 order by created_at limit $2",
        )
        .bind(slug)
        .bind(limit)
        .fetch_all(db)
        .await?,
    )
}

/// Number of platform operators other than `email`.
pub async fn count_other_platform_admins(db: impl PgExecutor<'_>, email: &str) -> DbResult<i64> {
    Ok(
        // `server-owner+…@hostingos.invalid`: the internal operator record of
        // a single-company installation (hos-web `mode::operator_email`).
        sqlx::query_scalar(
            "select count(*) from platform_admins where email <> $1::citext
               and email::text not like 'server-owner+%@hostingos.invalid'",
        )
        .bind(email)
        .fetch_one(db)
        .await?,
    )
}

/// Creates or restores the demo platform operator: the given password
/// hash, active, no 2FA, no recovery codes. Its sessions and its
/// platform-level audit entries are removed. Returns its id.
pub async fn reset_platform_admin(
    conn: &mut PgConnection,
    email: &str,
    name: &str,
    password_hash: &str,
) -> DbResult<PlatformAdminId> {
    let id: PlatformAdminId = sqlx::query_scalar(
        "insert into platform_admins (id, email, name, password_hash)
         values ($1, $2, $3, $4)
         on conflict (email) do update
            set name = excluded.name,
                password_hash = excluded.password_hash,
                totp_secret_enc = null,
                totp_enabled = false,
                totp_last_step = null,
                totp_recovery_hashes = '{}',
                status = 'active',
                last_login_at = null,
                last_login_ip = null,
                updated_at = now()
         returning id",
    )
    .bind(PlatformAdminId::new())
    .bind(email)
    .bind(name)
    .bind(password_hash)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query("delete from sessions where platform_admin_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "delete from audit_log
         where tenant_id is null and actor_kind = 'platform_admin' and actor_id = $1",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Removes the demo platform operator (tests).
pub async fn delete_platform_admin(db: impl PgExecutor<'_>, email: &str) -> DbResult<u64> {
    Ok(
        sqlx::query("delete from platform_admins where email = $1::citext")
            .bind(email)
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// Whether `email` is a login of client `client_id` (the demo client is
/// protected from status changes that would lock its visitors out).
pub async fn client_has_login(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: uuid::Uuid,
    email: &str,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar(
        "select exists(
             select 1 from client_users cu join users u on u.id = cu.user_id
             where cu.tenant_id = $1 and cu.client_id = $2 and u.email = $3::citext)",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(email)
    .fetch_one(db)
    .await?)
}

/// Products of `tenant_id` without a provisioning module.
pub async fn products_without_integration(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<(ProductId, ProductType)>> {
    Ok(sqlx::query_as(
        "select id, product_type from products
         where tenant_id = $1 and integration_id is null
         order by sort_order, id",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}
