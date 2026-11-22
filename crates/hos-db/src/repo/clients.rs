//! Customers: billing profiles (`clients`), linked login users, contacts,
//! staff notes, client groups and the credit balance.

use chrono::{DateTime, Utc};
use hos_core::domain::Locale;
use hos_core::ids::{ClientGroupId, ClientId, ContactId, IntegrationId, TenantId, UserId};
use hos_core::money::Currency;
use rust_decimal::Decimal;
use sqlx::{Acquire, FromRow, PgConnection, PgExecutor, Postgres};

use super::Json;
use crate::enums::{ClientKind, ClientStatus};
use crate::error::{DbResult, conflict_on_unique, like_contains};
use crate::ids::{ClientNoteId, ClientUserId};
use crate::page::{Page, Paged, fetch_paged};

/// `clients` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientRow {
    pub id: ClientId,
    pub tenant_id: TenantId,
    pub kind: ClientKind,
    pub first_name: String,
    pub last_name: String,
    pub company_name: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country_code: String,
    pub tax_office: Option<String>,
    pub tax_number: Option<String>,
    pub national_id: Option<String>,
    pub currency: Currency,
    pub locale: Locale,
    pub group_id: Option<ClientGroupId>,
    pub status: ClientStatus,
    pub tax_exempt: bool,
    pub late_fee_exempt: bool,
    pub overdue_notices: bool,
    pub separate_invoices: bool,
    pub marketing_opt_in: bool,
    pub default_gateway_id: Option<IntegrationId>,
    pub custom_fields: Json,
    pub signup_ip: Option<std::net::IpAddr>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ClientRow {
    /// Company name for corporate clients, else "First Last".
    pub fn display_name(&self) -> String {
        match (&self.kind, &self.company_name) {
            (ClientKind::Corporate, Some(c)) if !c.trim().is_empty() => c.clone(),
            _ => format!("{} {}", self.first_name, self.last_name)
                .trim()
                .to_owned(),
        }
    }
}

/// Editable client fields (create and update).
#[derive(Debug, Clone)]
pub struct ClientInput {
    pub kind: ClientKind,
    pub first_name: String,
    pub last_name: String,
    pub company_name: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    /// ISO 3166-1 alpha-2, upper-case.
    pub country_code: String,
    pub tax_office: Option<String>,
    pub tax_number: Option<String>,
    pub national_id: Option<String>,
    pub currency: Currency,
    pub locale: Locale,
    pub group_id: Option<ClientGroupId>,
    pub tax_exempt: bool,
    pub marketing_opt_in: bool,
}

impl ClientInput {
    /// Minimal individual client in Turkey.
    pub fn individual(first_name: &str, last_name: &str, email: &str, currency: Currency) -> Self {
        Self {
            kind: ClientKind::Individual,
            first_name: first_name.to_owned(),
            last_name: last_name.to_owned(),
            company_name: None,
            email: email.to_owned(),
            phone: None,
            address_line1: None,
            address_line2: None,
            city: None,
            state: None,
            postal_code: None,
            country_code: "TR".to_owned(),
            tax_office: None,
            tax_number: None,
            national_id: None,
            currency,
            locale: Locale::Tr,
            group_id: None,
            tax_exempt: false,
            marketing_opt_in: false,
        }
    }
}

/// Creates a client.
pub async fn create_client(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    input: &ClientInput,
) -> DbResult<ClientRow> {
    sqlx::query_as::<_, ClientRow>(
        "insert into clients (id, tenant_id, kind, first_name, last_name, company_name, email, phone,
                              address_line1, address_line2, city, state, postal_code, country_code,
                              tax_office, tax_number, national_id, currency, locale, group_id,
                              tax_exempt, marketing_opt_in)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, upper($14), $15, $16, $17, $18,
                 $19, $20, $21, $22)
         returning *",
    )
    .bind(ClientId::new())
    .bind(tenant_id)
    .bind(input.kind)
    .bind(&input.first_name)
    .bind(&input.last_name)
    .bind(&input.company_name)
    .bind(input.email.trim())
    .bind(&input.phone)
    .bind(&input.address_line1)
    .bind(&input.address_line2)
    .bind(&input.city)
    .bind(&input.state)
    .bind(&input.postal_code)
    .bind(&input.country_code)
    .bind(&input.tax_office)
    .bind(&input.tax_number)
    .bind(&input.national_id)
    .bind(input.currency)
    .bind(input.locale)
    .bind(input.group_id)
    .bind(input.tax_exempt)
    .bind(input.marketing_opt_in)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("client"))
}

/// Updates a client's editable fields.
pub async fn update_client(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ClientId,
    input: &ClientInput,
) -> DbResult<Option<ClientRow>> {
    sqlx::query_as::<_, ClientRow>(
        "update clients set kind = $3, first_name = $4, last_name = $5, company_name = $6, email = $7,
                phone = $8, address_line1 = $9, address_line2 = $10, city = $11, state = $12,
                postal_code = $13, country_code = upper($14), tax_office = $15, tax_number = $16,
                national_id = $17, currency = $18, locale = $19, group_id = $20, tax_exempt = $21,
                marketing_opt_in = $22
         where tenant_id = $1 and id = $2
         returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(input.kind)
    .bind(&input.first_name)
    .bind(&input.last_name)
    .bind(&input.company_name)
    .bind(input.email.trim())
    .bind(&input.phone)
    .bind(&input.address_line1)
    .bind(&input.address_line2)
    .bind(&input.city)
    .bind(&input.state)
    .bind(&input.postal_code)
    .bind(&input.country_code)
    .bind(&input.tax_office)
    .bind(&input.tax_number)
    .bind(&input.national_id)
    .bind(input.currency)
    .bind(input.locale)
    .bind(input.group_id)
    .bind(input.tax_exempt)
    .bind(input.marketing_opt_in)
    .fetch_optional(db)
    .await
    .map_err(conflict_on_unique("client"))
}

/// Client by id.
pub async fn get_client(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ClientId,
) -> DbResult<Option<ClientRow>> {
    Ok(
        sqlx::query_as::<_, ClientRow>("select * from clients where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Changes the client status (`closed` stamps `closed_at`).
pub async fn set_client_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ClientId,
    status: ClientStatus,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update clients set status = $3,
                closed_at = case when $3 = 'closed' then coalesce(closed_at, now()) else null end
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Filters of [`list_clients`].
#[derive(Debug, Clone, Default)]
pub struct ClientFilter {
    /// Matches names, company, e-mail, phone and tax number.
    pub search: Option<String>,
    pub status: Option<ClientStatus>,
    pub group_id: Option<ClientGroupId>,
}

/// Client list row with aggregates for the admin table.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientListRow {
    #[sqlx(flatten)]
    pub client: ClientRow,
    pub active_services: i64,
    pub unpaid_total: Decimal,
}

/// Clients of a tenant, newest first, with active service count and unpaid total.
pub async fn list_clients<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ClientFilter,
    page: Page,
) -> DbResult<Paged<ClientListRow>> {
    let f = filter.clone();
    let search = f.search.as_deref().map(like_contains);
    fetch_paged(
        db,
        "select c.*,
                (select count(*) from services s
                  where s.tenant_id = c.tenant_id and s.client_id = c.id and s.status = 'active') as active_services,
                (select coalesce(sum(i.balance), 0) from invoices i
                  where i.tenant_id = c.tenant_id and i.client_id = c.id
                    and i.status in ('unpaid', 'payment_pending', 'collections')) as unpaid_total",
        |qb| {
            qb.push("from clients c where c.tenant_id = ").push_bind(tenant_id);
            if let Some(s) = &search {
                qb.push(" and (c.email::text ilike ")
                    .push_bind(s.clone())
                    .push(" or (c.first_name || ' ' || c.last_name) ilike ")
                    .push_bind(s.clone())
                    .push(" or c.company_name ilike ")
                    .push_bind(s.clone())
                    .push(" or c.phone ilike ")
                    .push_bind(s.clone())
                    .push(" or c.tax_number ilike ")
                    .push_bind(s.clone())
                    .push(")");
            }
            if let Some(st) = f.status {
                qb.push(" and c.status = ").push_bind(st);
            }
            if let Some(g) = f.group_id {
                qb.push(" and c.group_id = ").push_bind(g);
            }
        },
        "c.created_at desc, c.id desc",
        page,
    )
    .await
}

// ---------------------------------------------------------------------------
// client_users
// ---------------------------------------------------------------------------

/// `client_users` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientUserRow {
    pub id: ClientUserId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub user_id: UserId,
    pub is_owner: bool,
    pub permissions: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Links a login user to a client account.
pub async fn link_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    user_id: UserId,
    is_owner: bool,
    permissions: &[String],
) -> DbResult<ClientUserRow> {
    sqlx::query_as::<_, ClientUserRow>(
        "insert into client_users (id, tenant_id, client_id, user_id, is_owner, permissions)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (client_id, user_id) do update set is_owner = excluded.is_owner, permissions = excluded.permissions
         returning *",
    )
    .bind(ClientUserId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(user_id)
    .bind(is_owner)
    .bind(permissions)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("client owner"))
}

/// Links a sub-user that the client user `invited_by` invited from the
/// client area (recorded in `client_users.invited_by`, see
/// [`remove_members_invited_by`]). Never an owner.
pub async fn link_invited_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    user_id: UserId,
    permissions: &[String],
    invited_by: UserId,
) -> DbResult<ClientUserRow> {
    sqlx::query_as::<_, ClientUserRow>(
        "insert into client_users (id, tenant_id, client_id, user_id, is_owner, permissions, invited_by)
         values ($1, $2, $3, $4, false, $5, $6)
         on conflict (client_id, user_id) do update
            set permissions = excluded.permissions,
                invited_by = coalesce(client_users.invited_by, excluded.invited_by)
            where not client_users.is_owner
         returning *",
    )
    .bind(ClientUserId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(user_id)
    .bind(permissions)
    .bind(invited_by)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| hos_core::CoreError::Conflict("the user owns this client account".into()))
}

/// Removes every (non-owner) membership that `inviter` created through the
/// client area; returns how many were removed. Used when an account whose
/// e-mail was never verified is reclaimed by the mailbox owner.
pub async fn remove_members_invited_by(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    inviter: UserId,
) -> DbResult<u64> {
    let res = sqlx::query(
        "delete from client_users where tenant_id = $1 and invited_by = $2 and not is_owner",
    )
    .bind(tenant_id)
    .bind(inviter)
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Removes a user from a client account.
pub async fn unlink_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    user_id: UserId,
) -> DbResult<bool> {
    let res = sqlx::query(
        "delete from client_users where tenant_id = $1 and client_id = $2 and user_id = $3",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(user_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Client accounts a user can act for (owner first).
pub async fn clients_of_user(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    user_id: UserId,
) -> DbResult<Vec<ClientRow>> {
    Ok(sqlx::query_as::<_, ClientRow>(
        "select c.* from client_users cu join clients c on c.id = cu.client_id
         where cu.tenant_id = $1 and cu.user_id = $2
         order by cu.is_owner desc, c.created_at",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// The link between a user and a client (for permission checks), if any.
pub async fn client_membership(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    user_id: UserId,
) -> DbResult<Option<ClientUserRow>> {
    Ok(sqlx::query_as::<_, ClientUserRow>(
        "select * from client_users where tenant_id = $1 and client_id = $2 and user_id = $3",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?)
}

/// A login user of a client account (client area "users" page).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientMemberRow {
    #[sqlx(flatten)]
    pub link: ClientUserRow,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    pub last_login_at: Option<DateTime<Utc>>,
    /// `users.password_hash is not null` (an invited user who never set a
    /// password has none).
    pub has_password: bool,
}

/// Users linked to a client account (owner first).
pub async fn client_members(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<ClientMemberRow>> {
    Ok(sqlx::query_as::<_, ClientMemberRow>(
        "select cu.*, u.email::text as email, u.first_name, u.last_name, u.last_login_at,
                (u.password_hash is not null) as has_password
         from client_users cu join users u on u.id = cu.user_id and u.tenant_id = cu.tenant_id
         where cu.tenant_id = $1 and cu.client_id = $2
         order by cu.is_owner desc, cu.created_at",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// Replaces the permissions of a (non-owner) member.
pub async fn set_member_permissions(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    user_id: UserId,
    permissions: &[String],
) -> DbResult<bool> {
    let res = sqlx::query(
        "update client_users set permissions = $4
         where tenant_id = $1 and client_id = $2 and user_id = $3 and not is_owner",
    )
    .bind(tenant_id)
    .bind(client_id)
    .bind(user_id)
    .bind(permissions)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Creates a client and links `owner` as its owner in one transaction (signup).
pub async fn create_client_with_owner(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    input: &ClientInput,
    owner: UserId,
) -> DbResult<ClientRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let client = create_client(&mut *tx, tenant_id, input).await?;
    link_user(&mut *tx, tenant_id, client.id, owner, true, &[]).await?;
    tx.commit().await?;
    Ok(client)
}

// ---------------------------------------------------------------------------
// contacts
// ---------------------------------------------------------------------------

/// `contacts` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ContactRow {
    pub id: ContactId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub first_name: String,
    pub last_name: String,
    pub company_name: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country_code: String,
    pub is_billing: bool,
    pub is_domain_contact: bool,
    pub email_preferences: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Editable contact fields.
#[derive(Debug, Clone)]
pub struct ContactInput {
    pub first_name: String,
    pub last_name: String,
    pub company_name: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country_code: String,
    pub is_billing: bool,
    pub is_domain_contact: bool,
    pub email_preferences: Vec<String>,
}

/// Adds a contact.
pub async fn create_contact(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    c: &ContactInput,
) -> DbResult<ContactRow> {
    Ok(sqlx::query_as::<_, ContactRow>(
        "insert into contacts (id, tenant_id, client_id, first_name, last_name, company_name, email, phone,
                               address_line1, address_line2, city, state, postal_code, country_code,
                               is_billing, is_domain_contact, email_preferences)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, upper($14), $15, $16, $17)
         returning *",
    )
    .bind(ContactId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(&c.first_name)
    .bind(&c.last_name)
    .bind(&c.company_name)
    .bind(c.email.trim())
    .bind(&c.phone)
    .bind(&c.address_line1)
    .bind(&c.address_line2)
    .bind(&c.city)
    .bind(&c.state)
    .bind(&c.postal_code)
    .bind(&c.country_code)
    .bind(c.is_billing)
    .bind(c.is_domain_contact)
    .bind(&c.email_preferences)
    .fetch_one(db)
    .await?)
}

/// Contacts of a client.
pub async fn list_contacts(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<ContactRow>> {
    Ok(sqlx::query_as::<_, ContactRow>(
        "select * from contacts where tenant_id = $1 and client_id = $2 order by first_name, last_name",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// Deletes a contact.
pub async fn delete_contact(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: ContactId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from contacts where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// client_notes / client_groups
// ---------------------------------------------------------------------------

/// `client_notes` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientNoteRow {
    pub id: ClientNoteId,
    pub tenant_id: TenantId,
    pub client_id: ClientId,
    pub author_user_id: Option<UserId>,
    pub body: String,
    pub is_sticky: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Adds a staff note.
pub async fn add_note(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
    author: Option<UserId>,
    body: &str,
    is_sticky: bool,
) -> DbResult<ClientNoteRow> {
    Ok(sqlx::query_as::<_, ClientNoteRow>(
        "insert into client_notes (id, tenant_id, client_id, author_user_id, body, is_sticky)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(ClientNoteId::new())
    .bind(tenant_id)
    .bind(client_id)
    .bind(author)
    .bind(body)
    .bind(is_sticky)
    .fetch_one(db)
    .await?)
}

/// Notes of a client (sticky first, newest first).
pub async fn list_notes(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<ClientNoteRow>> {
    Ok(sqlx::query_as::<_, ClientNoteRow>(
        "select * from client_notes where tenant_id = $1 and client_id = $2
         order by is_sticky desc, created_at desc",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?)
}

/// `client_groups` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ClientGroupRow {
    pub id: ClientGroupId,
    pub tenant_id: TenantId,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub discount_percent: Decimal,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creates a client group.
pub async fn create_group(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    name: &str,
    discount_percent: Decimal,
    color: Option<&str>,
) -> DbResult<ClientGroupRow> {
    sqlx::query_as::<_, ClientGroupRow>(
        "insert into client_groups (id, tenant_id, name, discount_percent, color)
         values ($1, $2, $3, $4, $5) returning *",
    )
    .bind(ClientGroupId::new())
    .bind(tenant_id)
    .bind(name)
    .bind(discount_percent)
    .bind(color)
    .fetch_one(db)
    .await
    .map_err(conflict_on_unique("client group"))
}

/// Client groups of a tenant.
pub async fn list_groups(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<ClientGroupRow>> {
    Ok(sqlx::query_as::<_, ClientGroupRow>(
        "select * from client_groups where tenant_id = $1 order by name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}
