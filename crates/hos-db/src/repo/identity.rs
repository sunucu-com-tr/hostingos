//! Identity: users, OAuth identities, sessions, RBAC, TOTP, one-time tokens
//! (password reset, e-mail verification) and API keys.
//!
//! Tokens are never stored in clear: callers pass `hos_core::crypto::hash_token(token)`.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use hos_core::domain::{Locale, UserKind};
use hos_core::ids::{ApiKeyId, PlatformAdminId, RoleId, SessionId, TenantId, UserId};
use ipnet::IpNet;
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};

use super::{Json, empty_object};
use crate::enums::{AccountStatus, ImpersonatorKind, OAuthProvider};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::ids::{
    EmailVerificationId, PasswordResetId, RolePermissionId, TotpSecretId, UserIdentityId,
    UserRoleId,
};
use crate::page::{Page, Paged, fetch_paged};

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

/// `users` row. `password_hash` is `None` for OAuth-only accounts.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct UserRow {
    pub id: UserId,
    pub tenant_id: TenantId,
    pub email: String,
    pub password_hash: Option<String>,
    pub kind: UserKind,
    pub first_name: String,
    pub last_name: String,
    pub phone: Option<String>,
    pub locale: Locale,
    pub timezone: Option<String>,
    pub avatar_url: Option<String>,
    pub email_verified_at: Option<DateTime<Utc>>,
    pub totp_enabled: bool,
    pub status: AccountStatus,
    pub failed_login_count: i32,
    pub locked_until: Option<DateTime<Utc>>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub last_login_ip: Option<IpAddr>,
    pub password_changed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl UserRow {
    /// `"First Last"`, or the e-mail when both names are empty.
    pub fn display_name(&self) -> String {
        let name = format!("{} {}", self.first_name.trim(), self.last_name.trim());
        let name = name.trim();
        if name.is_empty() {
            self.email.clone()
        } else {
            name.to_owned()
        }
    }

    /// Active and not inside a lock window.
    pub fn can_login(&self, now: DateTime<Utc>) -> bool {
        self.status == AccountStatus::Active && self.locked_until.is_none_or(|t| t <= now)
    }
}

/// Input of [`create_user`].
#[derive(Debug, Clone)]
pub struct NewUser {
    pub email: String,
    pub password_hash: Option<String>,
    pub kind: UserKind,
    pub first_name: String,
    pub last_name: String,
    pub phone: Option<String>,
    pub locale: Locale,
    /// Stamp `email_verified_at` now (OAuth-verified e-mail, staff created by an admin).
    pub email_verified: bool,
}

/// Creates a user. Duplicate e-mail within the tenant → `CoreError::Conflict`.
pub async fn create_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    new: &NewUser,
) -> DbResult<UserRow> {
    sqlx::query_as::<_, UserRow>(
        "insert into users (id, tenant_id, email, password_hash, kind, first_name, last_name, phone, locale,
                            email_verified_at, password_changed_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                 case when $10 then now() end, case when $4 is not null then now() end)
         returning *",
    )
    .bind(UserId::new())
    .bind(tenant_id)
    .bind(new.email.trim())
    .bind(&new.password_hash)
    .bind(new.kind)
    .bind(&new.first_name)
    .bind(&new.last_name)
    .bind(&new.phone)
    .bind(new.locale)
    .bind(new.email_verified)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("email"))
}

/// User by id.
pub async fn get_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
) -> DbResult<Option<UserRow>> {
    Ok(
        sqlx::query_as::<_, UserRow>("select * from users where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// User by e-mail (case-insensitive, `citext`).
pub async fn user_by_email(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    email: &str,
) -> DbResult<Option<UserRow>> {
    Ok(sqlx::query_as::<_, UserRow>(
        "select * from users where tenant_id = $1 and email = $2::citext",
    )
    .bind(tenant_id)
    .bind(email.trim())
    .fetch_optional(db)
    .await?)
}

/// Filters of [`list_users`].
#[derive(Debug, Clone, Default)]
pub struct UserFilter {
    pub kind: Option<UserKind>,
    pub status: Option<AccountStatus>,
    /// Matches e-mail and names.
    pub search: Option<String>,
}

/// Users of a tenant, newest first.
pub async fn list_users<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &UserFilter,
    page: Page,
) -> DbResult<Paged<UserRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from users where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(k) = f.kind {
                qb.push(" and kind = ").push_bind(k);
            }
            if let Some(s) = f.status {
                qb.push(" and status = ").push_bind(s);
            }
            if let Some(s) = &search {
                qb.push(" and (email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or (first_name || ' ' || last_name) ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}

/// Staff users (admin area), by name.
pub async fn list_staff(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<Vec<UserRow>> {
    Ok(sqlx::query_as::<_, UserRow>(
        "select * from users where tenant_id = $1 and kind = 'staff' order by first_name, last_name, email",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Updates profile fields.
pub async fn update_user_profile(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
    first_name: &str,
    last_name: &str,
    phone: Option<&str>,
    locale: Locale,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update users set first_name = $3, last_name = $4, phone = $5, locale = $6
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(first_name)
    .bind(last_name)
    .bind(phone)
    .bind(locale)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Sets the user's preferred time zone (IANA name; `None`: tenant default).
pub async fn set_user_timezone(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
    timezone: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update users set timezone = $3, updated_at = now() where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(timezone)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Changes the login e-mail (clears `email_verified_at`).
pub async fn update_user_email(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
    email: &str,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update users set email = $3, email_verified_at = case when email = $3::citext then email_verified_at end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(email.trim())
    .execute(db)
    .await
    .map_err(conflict_on_unique("email"))?;
    Ok(res.rows_affected() == 1)
}

/// Sets (or clears, with `None`) the password hash.
pub async fn set_password_hash(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
    password_hash: Option<&str>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update users set password_hash = $3, password_changed_at = now(), failed_login_count = 0, locked_until = null
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(password_hash)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Stamps `email_verified_at`.
pub async fn mark_email_verified(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update users set email_verified_at = coalesce(email_verified_at, now()) where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Changes the account status.
pub async fn set_user_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
    status: AccountStatus,
) -> DbResult<bool> {
    let res = sqlx::query("update users set status = $3 where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(status)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Successful login: resets the failure counter and stamps the login.
pub async fn record_login_success(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
    ip: Option<IpAddr>,
) -> DbResult<()> {
    sqlx::query(
        "update users set last_login_at = now(), last_login_ip = $3, failed_login_count = 0, locked_until = null
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(ip)
    .execute(db)
    .await?;
    Ok(())
}

/// Failed login: increments the counter and, once it reaches `lock_after`,
/// locks the account for `lock_seconds`. Returns `(failed_count, locked_until)`.
pub async fn record_login_failure(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
    lock_after: i32,
    lock_seconds: i64,
) -> DbResult<Option<(i32, Option<DateTime<Utc>>)>> {
    Ok(sqlx::query_as::<_, (i32, Option<DateTime<Utc>>)>(
        "update users set failed_login_count = failed_login_count + 1,
                locked_until = case when failed_login_count + 1 >= $3
                                    then now() + make_interval(secs => $4) else locked_until end
         where tenant_id = $1 and id = $2
         returning failed_login_count, locked_until",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(lock_after)
    .bind(lock_seconds as f64)
    .fetch_optional(db)
    .await?)
}

/// Deletes a user (cascades sessions, identities, roles...).
pub async fn delete_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: UserId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from users where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// user_identities (OAuth)
// ---------------------------------------------------------------------------

/// `user_identities` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct UserIdentityRow {
    pub id: UserIdentityId,
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub provider: OAuthProvider,
    pub provider_user_id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub raw_profile: Json,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Profile returned by an OAuth provider.
#[derive(Debug, Clone)]
pub struct OAuthProfile {
    pub provider: OAuthProvider,
    pub provider_user_id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub raw: Json,
}

/// The user linked to a provider account.
pub async fn user_by_identity(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    provider: OAuthProvider,
    provider_user_id: &str,
) -> DbResult<Option<UserRow>> {
    Ok(sqlx::query_as::<_, UserRow>(
        "select u.* from user_identities i join users u on u.id = i.user_id
         where i.tenant_id = $1 and i.provider = $2 and i.provider_user_id = $3",
    )
    .bind(tenant_id)
    .bind(provider)
    .bind(provider_user_id)
    .fetch_optional(db)
    .await?)
}

/// Links (or refreshes) a provider account for `user_id`. If the provider
/// account is already linked to a *different* user → `CoreError::Conflict`.
pub async fn link_identity(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    profile: &OAuthProfile,
) -> DbResult<UserIdentityRow> {
    let row = sqlx::query_as::<_, UserIdentityRow>(
        "insert into user_identities (id, tenant_id, user_id, provider, provider_user_id, email,
                                      display_name, avatar_url, raw_profile, last_used_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
         on conflict (tenant_id, provider, provider_user_id) do update
            set email = excluded.email, display_name = excluded.display_name,
                avatar_url = excluded.avatar_url, raw_profile = excluded.raw_profile, last_used_at = now()
            where user_identities.user_id = excluded.user_id
         returning *",
    )
    .bind(UserIdentityId::new())
    .bind(tenant_id)
    .bind(user_id)
    .bind(profile.provider)
    .bind(&profile.provider_user_id)
    .bind(&profile.email)
    .bind(&profile.display_name)
    .bind(&profile.avatar_url)
    .bind(&profile.raw)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("identity"))?;
    row.ok_or_else(|| hos_core::CoreError::Conflict("identity is linked to another user".into()))
}

/// Identities of a user.
pub async fn list_identities(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<Vec<UserIdentityRow>> {
    Ok(sqlx::query_as::<_, UserIdentityRow>(
        "select * from user_identities where tenant_id = $1 and user_id = $2 order by provider",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// Deletes every provider account linked to a user except `keep`
/// (`(provider, provider_user_id)` of the identity being signed in with).
/// Returns the number of removed links.
pub async fn delete_user_identities(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    keep: Option<(OAuthProvider, &str)>,
) -> DbResult<u64> {
    let (keep_provider, keep_id) = match keep {
        Some((p, id)) => (Some(p), Some(id)),
        None => (None, None),
    };
    let res = sqlx::query(
        "delete from user_identities
         where tenant_id = $1 and user_id = $2
           and not (coalesce(provider = $3, false) and coalesce(provider_user_id = $4, false))",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(keep_provider)
    .bind(keep_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Unlinks a provider from a user.
pub async fn unlink_identity(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    provider: OAuthProvider,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from user_identities where tenant_id = $1 and user_id = $2 and provider = $3",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(provider)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

/// `sessions` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct SessionRow {
    pub id: SessionId,
    pub tenant_id: Option<TenantId>,
    pub token_hash: String,
    pub user_id: Option<UserId>,
    pub platform_admin_id: Option<PlatformAdminId>,
    pub impersonator_id: Option<uuid::Uuid>,
    pub impersonator_kind: Option<ImpersonatorKind>,
    pub csrf_token: String,
    pub mfa_verified_at: Option<DateTime<Utc>>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub data: Json,
    pub expires_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Who a new session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOwner {
    /// A tenant user.
    User {
        tenant_id: TenantId,
        user_id: UserId,
    },
    /// A platform operator.
    PlatformAdmin(PlatformAdminId),
}

/// Input of [`create_session`].
#[derive(Debug, Clone)]
pub struct NewSession {
    pub owner: SessionOwner,
    /// `hos_core::crypto::hash_token(cookie_token)`.
    pub token_hash: String,
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    /// Second factor already passed (or not required).
    pub mfa_verified: bool,
    /// `(impersonator id, kind)` when staff / platform admin logs in as someone.
    pub impersonator: Option<(uuid::Uuid, ImpersonatorKind)>,
}

/// Creates a session.
pub async fn create_session(db: impl PgExecutor<'_>, new: &NewSession) -> DbResult<SessionRow> {
    let (tenant_id, user_id, admin_id) = match new.owner {
        SessionOwner::User { tenant_id, user_id } => (Some(tenant_id), Some(user_id), None),
        SessionOwner::PlatformAdmin(id) => (None, None, Some(id)),
    };
    Ok(sqlx::query_as::<_, SessionRow>(
        "insert into sessions (id, tenant_id, token_hash, user_id, platform_admin_id, impersonator_id,
                               impersonator_kind, csrf_token, mfa_verified_at, ip, user_agent, data, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, case when $9 then now() end, $10, $11, $12, $13)
         returning *",
    )
    .bind(SessionId::new())
    .bind(tenant_id)
    .bind(&new.token_hash)
    .bind(user_id)
    .bind(admin_id)
    .bind(new.impersonator.map(|(id, _)| id))
    .bind(new.impersonator.map(|(_, k)| k))
    .bind(&new.csrf_token)
    .bind(new.mfa_verified)
    .bind(new.ip)
    .bind(&new.user_agent)
    .bind(empty_object())
    .bind(new.expires_at)
    .fetch_one(db)
    .await?)
}

/// Looks up a live session by token hash, scoped to the tenant serving the
/// request (`None` = platform admin sessions only), and refreshes
/// `last_seen_at` at most once a minute. Expired / foreign sessions → `None`.
pub async fn validate_session(
    db: impl PgExecutor<'_>,
    tenant_id: Option<TenantId>,
    token_hash: &str,
) -> DbResult<Option<SessionRow>> {
    Ok(sqlx::query_as::<_, SessionRow>(
        "with s as (
             select * from sessions
             where token_hash = $1 and expires_at > now() and tenant_id is not distinct from $2
         ), touch as (
             update sessions set last_seen_at = now()
             where id = (select id from s) and last_seen_at < now() - interval '60 seconds'
         )
         select * from s",
    )
    .bind(token_hash)
    .bind(tenant_id)
    .fetch_optional(db)
    .await?)
}

/// Whether session `id` is still live and signed in as `user_id` (an active
/// user): `false` after logout, revocation, expiry, the end of an
/// impersonation or the user being locked / disabled. Long-lived connections
/// (the VM console relay) re-check this while they run.
pub async fn session_held_by(
    db: impl PgExecutor<'_>,
    id: SessionId,
    user_id: UserId,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar(
        "select exists (select 1 from sessions s join users u on u.id = s.user_id
                         where s.id = $1 and s.user_id = $2 and s.expires_at > now()
                           and u.status = 'active')",
    )
    .bind(id)
    .bind(user_id)
    .fetch_one(db)
    .await?)
}

/// Marks the second factor as passed.
pub async fn set_session_mfa_verified(db: impl PgExecutor<'_>, id: SessionId) -> DbResult<bool> {
    let res = sqlx::query("update sessions set mfa_verified_at = now() where id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Moves the expiry (sliding sessions / "remember me").
pub async fn extend_session(
    db: impl PgExecutor<'_>,
    id: SessionId,
    expires_at: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query("update sessions set expires_at = $2 where id = $1")
        .bind(id)
        .bind(expires_at)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Replaces the session's free-form data (flash messages, OAuth state, ...).
pub async fn set_session_data(
    db: impl PgExecutor<'_>,
    id: SessionId,
    data: &Json,
) -> DbResult<bool> {
    let res = sqlx::query("update sessions set data = $2 where id = $1")
        .bind(id)
        .bind(data)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Logout: deletes the session with this token hash.
pub async fn delete_session(db: impl PgExecutor<'_>, token_hash: &str) -> DbResult<bool> {
    let res = sqlx::query("delete from sessions where token_hash = $1")
        .bind(token_hash)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Deletes every session of a user, optionally keeping one (password change).
pub async fn delete_user_sessions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    keep: Option<SessionId>,
) -> DbResult<u64> {
    let res = sqlx::query(
        "delete from sessions where tenant_id = $1 and user_id = $2 and ($3::uuid is null or id <> $3)",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(keep)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Live sessions of a user (newest first).
pub async fn list_user_sessions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<Vec<SessionRow>> {
    Ok(sqlx::query_as::<_, SessionRow>(
        "select * from sessions where tenant_id = $1 and user_id = $2 and expires_at > now()
         order by last_seen_at desc",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// Deletes expired sessions (all tenants). Returns the number removed.
pub async fn purge_expired_sessions(db: impl PgExecutor<'_>) -> DbResult<u64> {
    let res = sqlx::query("delete from sessions where expires_at <= now()")
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// RBAC
// ---------------------------------------------------------------------------

/// `roles` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct RoleRow {
    pub id: RoleId,
    pub tenant_id: TenantId,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a role.
pub async fn create_role(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
    name: &str,
    description: Option<&str>,
    is_system: bool,
) -> DbResult<RoleRow> {
    sqlx::query_as::<_, RoleRow>(
        "insert into roles (id, tenant_id, slug, name, description, is_system)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(RoleId::new())
    .bind(tenant_id)
    .bind(slug)
    .bind(name)
    .bind(description)
    .bind(is_system)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("role"))
}

/// Roles of a tenant.
pub async fn list_roles(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<Vec<RoleRow>> {
    Ok(sqlx::query_as::<_, RoleRow>(
        "select * from roles where tenant_id = $1 order by is_system desc, name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Role by slug.
pub async fn role_by_slug(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    slug: &str,
) -> DbResult<Option<RoleRow>> {
    Ok(
        sqlx::query_as::<_, RoleRow>("select * from roles where tenant_id = $1 and slug = $2")
            .bind(tenant_id)
            .bind(slug)
            .fetch_optional(db)
            .await?,
    )
}

/// Deletes a non-system role.
pub async fn delete_role(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: RoleId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from roles where tenant_id = $1 and id = $2 and not is_system")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Replaces the permission set of a role (transactional).
pub async fn set_role_permissions(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    role_id: RoleId,
    permissions: &[String],
) -> DbResult<()> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query("delete from role_permissions where tenant_id = $1 and role_id = $2")
        .bind(tenant_id)
        .bind(role_id)
        .execute(&mut *tx)
        .await?;
    let ids: Vec<RolePermissionId> = permissions
        .iter()
        .map(|_| RolePermissionId::new())
        .collect();
    sqlx::query(
        "insert into role_permissions (id, tenant_id, role_id, permission)
         select id, $1, $2, perm from unnest($3::uuid[], $4::text[]) as p(id, perm)
         on conflict (role_id, permission) do nothing",
    )
    .bind(tenant_id)
    .bind(role_id)
    .bind(&ids)
    .bind(permissions)
    .execute(&mut *tx)
    .await
    .map_err(conflict_on_unique("permission"))?;
    tx.commit().await?;
    Ok(())
}

/// Permissions of a role, sorted.
pub async fn role_permissions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    role_id: RoleId,
) -> DbResult<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "select permission from role_permissions where tenant_id = $1 and role_id = $2 order by permission",
    )
    .bind(tenant_id)
    .bind(role_id)
    .fetch_all(db)
    .await?)
}

/// Gives a role to a user (no-op if already assigned).
pub async fn assign_role(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    role_id: RoleId,
) -> DbResult<()> {
    sqlx::query(
        "insert into user_roles (id, tenant_id, user_id, role_id) values ($1, $2, $3, $4)
         on conflict (user_id, role_id) do nothing",
    )
    .bind(UserRoleId::new())
    .bind(tenant_id)
    .bind(user_id)
    .bind(role_id)
    .execute(db)
    .await
    .map_err(conflict_on_unique("user role"))?;
    Ok(())
}

/// Removes a role from a user.
pub async fn revoke_role(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    role_id: RoleId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from user_roles where tenant_id = $1 and user_id = $2 and role_id = $3",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(role_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Roles of a user.
pub async fn user_roles(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<Vec<RoleRow>> {
    Ok(sqlx::query_as::<_, RoleRow>(
        "select r.* from user_roles ur join roles r on r.id = ur.role_id
         where ur.tenant_id = $1 and ur.user_id = $2 order by r.name",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// Effective permission strings of a user (union over roles, sorted, may
/// contain `*` / `area.*` wildcards — matching is hos-auth's job).
pub async fn user_permissions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "select distinct rp.permission
         from user_roles ur
         join role_permissions rp on rp.role_id = ur.role_id
         where ur.tenant_id = $1 and ur.user_id = $2
         order by rp.permission",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// TOTP
// ---------------------------------------------------------------------------

/// `totp_secrets` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TotpSecretRow {
    pub id: TotpSecretId,
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub secret_enc: Vec<u8>,
    pub recovery_code_hashes: Vec<String>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub last_used_step: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Stores a new (unconfirmed) secret, replacing any previous one.
pub async fn upsert_totp_secret(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    secret_enc: &[u8],
    recovery_code_hashes: &[String],
) -> DbResult<TotpSecretRow> {
    Ok(sqlx::query_as::<_, TotpSecretRow>(
        "insert into totp_secrets (id, tenant_id, user_id, secret_enc, recovery_code_hashes)
         values ($1, $2, $3, $4, $5)
         on conflict (user_id) do update
            set secret_enc = excluded.secret_enc, recovery_code_hashes = excluded.recovery_code_hashes,
                confirmed_at = null, last_used_step = null
         returning *",
    )
    .bind(TotpSecretId::new())
    .bind(tenant_id)
    .bind(user_id)
    .bind(secret_enc)
    .bind(recovery_code_hashes)
    .fetch_one(db)
    .await?)
}

/// The user's TOTP secret.
pub async fn get_totp_secret(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<Option<TotpSecretRow>> {
    Ok(sqlx::query_as::<_, TotpSecretRow>(
        "select * from totp_secrets where tenant_id = $1 and user_id = $2",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?)
}

/// Confirms the secret and turns `users.totp_enabled` on (one statement).
pub async fn confirm_totp(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "with t as (
             update totp_secrets set confirmed_at = now()
             where tenant_id = $1 and user_id = $2 returning user_id
         )
         update users set totp_enabled = true where id in (select user_id from t)",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Removes the secret and turns `users.totp_enabled` off.
pub async fn disable_totp(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "with t as (delete from totp_secrets where tenant_id = $1 and user_id = $2)
         update users set totp_enabled = false where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Replay protection: records `step` only if it is newer than the last used
/// one. `false` means the code was already used.
pub async fn use_totp_step(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    step: i64,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update totp_secrets set last_used_step = $3
         where tenant_id = $1 and user_id = $2 and (last_used_step is null or last_used_step < $3)",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(step)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Consumes a recovery code (by hash). `true` if it was valid and unused.
pub async fn use_recovery_code(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    code_hash: &str,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update totp_secrets set recovery_code_hashes = array_remove(recovery_code_hashes, $3)
         where tenant_id = $1 and user_id = $2 and $3 = any(recovery_code_hashes)",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(code_hash)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// password resets / e-mail verification
// ---------------------------------------------------------------------------

/// Stores a password reset token hash.
pub async fn create_password_reset(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    token_hash: &str,
    expires_at: DateTime<Utc>,
    ip: Option<IpAddr>,
) -> DbResult<PasswordResetId> {
    let id = PasswordResetId::new();
    sqlx::query(
        "insert into password_resets (id, tenant_id, user_id, token_hash, expires_at, ip)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(token_hash)
    .bind(expires_at)
    .bind(ip)
    .execute(db)
    .await?;
    Ok(id)
}

/// Marks a valid (unused, unexpired) reset token as used and returns its user.
pub async fn consume_password_reset(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    token_hash: &str,
) -> DbResult<Option<UserId>> {
    Ok(sqlx::query_scalar::<_, UserId>(
        "update password_resets set used_at = now()
         where tenant_id = $1 and token_hash = $2 and used_at is null and expires_at > now()
         returning user_id",
    )
    .bind(tenant_id)
    .bind(token_hash)
    .fetch_optional(db)
    .await?)
}

/// Stores an e-mail verification token hash for `email`.
pub async fn create_email_verification(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
    email: &str,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> DbResult<EmailVerificationId> {
    let id = EmailVerificationId::new();
    sqlx::query(
        "insert into email_verifications (id, tenant_id, user_id, email, token_hash, expires_at)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(email.trim())
    .bind(token_hash)
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok(id)
}

/// Consumes a verification token: sets the user's e-mail to the verified
/// address and stamps `email_verified_at`. Returns the user id.
pub async fn consume_email_verification(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    token_hash: &str,
) -> DbResult<Option<UserId>> {
    sqlx::query_scalar::<_, UserId>(
        "with v as (
             update email_verifications set verified_at = now()
             where tenant_id = $1 and token_hash = $2 and verified_at is null and expires_at > now()
             returning user_id, email
         )
         update users u set email = v.email, email_verified_at = now()
         from v where u.id = v.user_id and u.tenant_id = $1
         returning u.id",
    )
    .bind(tenant_id)
    .bind(token_hash)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("email"))
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/// `api_keys` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ApiKeyRow {
    pub id: ApiKeyId,
    pub tenant_id: TenantId,
    pub user_id: Option<UserId>,
    pub name: String,
    pub prefix: String,
    pub key_hash: String,
    pub scopes: Vec<String>,
    pub allowed_ips: Vec<IpNet>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub last_used_ip: Option<IpAddr>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates an API key record (`key_hash = hash_token(full_key)`).
#[allow(clippy::too_many_arguments)]
pub async fn create_api_key(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: Option<UserId>,
    name: &str,
    prefix: &str,
    key_hash: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
) -> DbResult<ApiKeyRow> {
    Ok(sqlx::query_as::<_, ApiKeyRow>(
        "insert into api_keys (id, tenant_id, user_id, name, prefix, key_hash, scopes, expires_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning *",
    )
    .bind(ApiKeyId::new())
    .bind(tenant_id)
    .bind(user_id)
    .bind(name)
    .bind(prefix)
    .bind(key_hash)
    .bind(scopes)
    .bind(expires_at)
    .fetch_one(db)
    .await?)
}

/// A usable (not revoked, not expired) key by hash, within the tenant.
pub async fn api_key_by_hash(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    key_hash: &str,
) -> DbResult<Option<ApiKeyRow>> {
    Ok(sqlx::query_as::<_, ApiKeyRow>(
        "select * from api_keys
         where tenant_id = $1 and key_hash = $2 and revoked_at is null
           and (expires_at is null or expires_at > now())",
    )
    .bind(tenant_id)
    .bind(key_hash)
    .fetch_optional(db)
    .await?)
}

/// Stamps last use.
pub async fn touch_api_key(
    db: impl PgExecutor<'_>,
    id: ApiKeyId,
    ip: Option<IpAddr>,
) -> DbResult<()> {
    sqlx::query("update api_keys set last_used_at = now(), last_used_ip = $2 where id = $1")
        .bind(id)
        .bind(ip)
        .execute(db)
        .await?;
    Ok(())
}

/// Revokes a key.
pub async fn revoke_api_key(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ApiKeyId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update api_keys set revoked_at = coalesce(revoked_at, now()) where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Revokes every still-active key of a user; returns how many were revoked.
pub async fn revoke_user_api_keys(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<u64> {
    let res = sqlx::query(
        "update api_keys set revoked_at = now(), updated_at = now()
         where tenant_id = $1 and user_id = $2 and revoked_at is null",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Keys of a tenant (optionally of one user), newest first.
pub async fn list_api_keys(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: Option<UserId>,
) -> DbResult<Vec<ApiKeyRow>> {
    Ok(sqlx::query_as::<_, ApiKeyRow>(
        "select * from api_keys where tenant_id = $1 and ($2::uuid is null or user_id = $2)
         order by created_at desc",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}
