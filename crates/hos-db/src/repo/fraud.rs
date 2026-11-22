//! Order screening data: the ban list (`fraud_bans`) and the velocity
//! signals the built-in rules read (orders per IP, clients sharing a stored
//! payment method, recent orders of a client).

use chrono::{DateTime, Utc};
use hos_core::ids::{ClientId, OrderId, TenantId, UserId};
use hos_core::string_enum;
use sqlx::{FromRow, PgExecutor};
use std::net::IpAddr;

use crate::error::{DbResult, conflict_on_unique};

hos_core::define_id! {
    /// `fraud_bans`.
    pub struct BanId;
}

string_enum! {
    /// `fraud_bans.kind`.
    pub enum BanKind {
        Email => "email",
        Domain => "domain",
        Ip => "ip",
    }
}

/// `fraud_bans` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct BanRow {
    pub id: BanId,
    pub tenant_id: TenantId,
    pub kind: BanKind,
    pub value: String,
    pub reason: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Adds a ban (`value` already normalised by the caller: lower-case e-mail /
/// domain, or an address / CIDR network).
pub async fn add_ban(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    kind: BanKind,
    value: &str,
    reason: Option<&str>,
    created_by: Option<UserId>,
) -> DbResult<BanRow> {
    sqlx::query_as::<_, BanRow>(
        "insert into fraud_bans (id, tenant_id, kind, value, reason, created_by)
         values ($1, $2, $3, $4, $5, $6)
         returning *",
    )
    .bind(BanId::new())
    .bind(tenant_id)
    .bind(kind)
    .bind(value)
    .bind(reason)
    .bind(created_by)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("ban"))
}

/// Removes a ban.
pub async fn delete_ban(db: impl PgExecutor<'_>, tenant_id: TenantId, id: BanId) -> DbResult<bool> {
    let res = sqlx::query("delete from fraud_bans where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// The ban list, by kind and value.
pub async fn list_bans(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<Vec<BanRow>> {
    Ok(sqlx::query_as::<_, BanRow>(
        "select * from fraud_bans where tenant_id = $1 order by kind, value",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Bans matching an e-mail address, its domain or an IP (inside a banned
/// network).
pub async fn matching_bans(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    email: &str,
    ip: Option<IpAddr>,
) -> DbResult<Vec<BanRow>> {
    let email = email.trim().to_lowercase();
    let domain = email
        .rsplit_once('@')
        .map(|(_, d)| d.to_owned())
        .unwrap_or_default();
    Ok(sqlx::query_as::<_, BanRow>(
        "select * from fraud_bans
          where tenant_id = $1
            and ((kind = 'email' and value = $2)
              or (kind = 'domain' and ($3 = value or $3 like '%.' || value))
              or (kind = 'ip' and $4::inet is not null and $4::inet <<= value::inet))
          order by kind",
    )
    .bind(tenant_id)
    .bind(&email)
    .bind(&domain)
    .bind(ip)
    .fetch_all(db)
    .await?)
}

/// Orders placed from `ip` since `since`, other than `except`.
pub async fn orders_from_ip(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ip: IpAddr,
    since: DateTime<Utc>,
    except: OrderId,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from orders where tenant_id = $1 and ip = $2 and created_at >= $3 and id <> $4",
    )
    .bind(tenant_id)
    .bind(ip)
    .bind(since)
    .bind(except)
    .fetch_one(db)
    .await?)
}

/// Distinct client accounts from `ip` in orders since `since` (other than
/// `client`).
pub async fn clients_from_ip(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ip: IpAddr,
    since: DateTime<Utc>,
    client: ClientId,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(distinct client_id) from orders
          where tenant_id = $1 and ip = $2 and created_at >= $3 and client_id <> $4",
    )
    .bind(tenant_id)
    .bind(ip)
    .bind(since)
    .bind(client)
    .fetch_one(db)
    .await?)
}

/// Orders of `client` since `since`, other than `except`.
pub async fn orders_of_client(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client: ClientId,
    since: DateTime<Utc>,
    except: OrderId,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from orders where tenant_id = $1 and client_id = $2 and created_at >= $3 and id <> $4",
    )
    .bind(tenant_id)
    .bind(client)
    .bind(since)
    .bind(except)
    .fetch_one(db)
    .await?)
}

/// Other client accounts holding one of `client`'s stored payment methods
/// (same gateway token fingerprint).
pub async fn clients_sharing_card(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client: ClientId,
) -> DbResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(distinct y.client_id) from payment_methods x
           join payment_methods y on y.tenant_id = x.tenant_id and y.token_hash = x.token_hash
          where x.tenant_id = $1 and x.client_id = $2 and y.client_id <> $2",
    )
    .bind(tenant_id)
    .bind(client)
    .fetch_one(db)
    .await?)
}
