//! Entity writes of the data import (`hos-import`). Every function is
//! tenant-scoped and idempotent for a given target id:
//!
//! * `upsert_*` take the id to write (`Uuid::now_v7()` for a new row, the
//!   id-map target on a re-run) and return whether the row was inserted
//!   (`insert … on conflict (id) do update … returning (xmax = 0)`). A target
//!   that was deleted since the last run is simply inserted again with the
//!   same id. The conflict update is guarded by `tenant_id`, so an id of
//!   another tenant never matches.
//! * derived rows without their own source id (pricing, links, tax lines,
//!   memberships) are written with their natural unique keys.
//! * [`find_id`] / [`unique_slug`] look up natural keys for adopting rows
//!   that already exist in the tenant and for collision-free slugs.
//!
//! Nothing here queues jobs, e-mails or webhooks: an import is silent.

use std::net::IpAddr;

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::CoreError;
use hos_core::ids::TenantId;
use rust_decimal::Decimal;
use sqlx::PgConnection;
use uuid::Uuid;

use super::Json;
use crate::error::DbResult;

/// Builds `insert into <table> (id, tenant_id, cols…) values (…) on conflict
/// (id) do update set … where <table>.tenant_id = excluded.tenant_id
/// returning (xmax = 0)`. `keep` columns are written on insert only.
fn upsert_sql(table: &str, cols: &[&str], keep: &[&str]) -> String {
    let all: Vec<&str> = ["id", "tenant_id"].iter().chain(cols).copied().collect();
    let params: Vec<String> = (1..=all.len()).map(|i| format!("${i}")).collect();
    let sets: Vec<String> = cols
        .iter()
        .filter(|c| !keep.contains(c))
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    let set = if sets.is_empty() {
        "tenant_id = excluded.tenant_id".to_owned()
    } else {
        sets.join(", ")
    };
    format!(
        "insert into {table} ({}) values ({}) on conflict (id) do update set {set}
         where {table}.tenant_id = excluded.tenant_id returning (xmax = 0)",
        all.join(", "),
        params.join(", ")
    )
}

fn not_written(table: &str) -> CoreError {
    CoreError::Conflict(format!("{table}: target row belongs to another tenant"))
}

async fn run_upsert(
    q: sqlx::query::QueryScalar<'_, sqlx::Postgres, bool, sqlx::postgres::PgArguments>,
    conn: &mut PgConnection,
    table: &str,
) -> DbResult<bool> {
    q.fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| not_written(table))
}

// ---------------------------------------------------------------------------
// natural keys
// ---------------------------------------------------------------------------

/// Natural keys used to adopt existing rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NaturalKey {
    CurrencyCode,
    ClientGroupName,
    UserEmail,
    ConfigGroupName,
    CouponCode,
    Tld,
    DepartmentSlug,
    KbCategorySlug,
    IntegrationName,
    RoleSlug,
    LiveDomainName,
    OrderNumber,
    InvoiceNumber,
    TicketNumber,
}

impl NaturalKey {
    fn sql(self) -> &'static str {
        match self {
            Self::CurrencyCode => "select id from currencies where tenant_id = $1 and code = $2",
            Self::ClientGroupName => {
                "select id from client_groups where tenant_id = $1 and name = $2"
            }
            Self::UserEmail => "select id from users where tenant_id = $1 and email = $2::citext",
            Self::ConfigGroupName => {
                "select id from config_option_groups where tenant_id = $1 and name = $2"
            }
            Self::CouponCode => "select id from coupons where tenant_id = $1 and code = $2::citext",
            Self::Tld => "select id from tlds where tenant_id = $1 and tld = $2",
            Self::DepartmentSlug => {
                "select id from ticket_departments where tenant_id = $1 and slug = $2"
            }
            Self::KbCategorySlug => {
                "select id from kb_categories where tenant_id = $1 and slug = $2"
            }
            Self::IntegrationName => {
                "select id from integrations where tenant_id = $1 and name = $2"
            }
            Self::RoleSlug => "select id from roles where tenant_id = $1 and slug = $2",
            Self::LiveDomainName => {
                "select id from domains where tenant_id = $1 and name = $2::citext
                   and status not in ('expired', 'transferred_away', 'cancelled', 'fraud')"
            }
            Self::OrderNumber => "select id from orders where tenant_id = $1 and order_number = $2",
            Self::InvoiceNumber => "select id from invoices where tenant_id = $1 and number = $2",
            Self::TicketNumber => "select id from tickets where tenant_id = $1 and number = $2",
        }
    }
}

/// Id of the row with natural key `value`.
pub async fn find_id(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    key: NaturalKey,
    value: &str,
) -> DbResult<Option<Uuid>> {
    Ok(sqlx::query_scalar::<_, Uuid>(key.sql())
        .bind(tenant_id)
        .bind(value)
        .fetch_optional(&mut *conn)
        .await?)
}

/// Tables with a per-tenant (and per-locale) `slug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlugTable {
    ProductGroups,
    Products,
    ProductAddons,
    TicketDepartments,
    KbCategories,
    KbArticles,
    Announcements,
}

impl SlugTable {
    fn table(self) -> &'static str {
        match self {
            Self::ProductGroups => "product_groups",
            Self::Products => "products",
            Self::ProductAddons => "product_addons",
            Self::TicketDepartments => "ticket_departments",
            Self::KbCategories => "kb_categories",
            Self::KbArticles => "kb_articles",
            Self::Announcements => "announcements",
        }
    }

    fn per_locale(self) -> bool {
        matches!(self, Self::KbArticles | Self::Announcements)
    }
}

/// `base`, or `base-2`, `base-3` … — the first slug no other row (than
/// `own`) of the tenant (and locale) uses.
pub async fn unique_slug(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    table: SlugTable,
    base: &str,
    locale: Option<&str>,
    own: Uuid,
) -> DbResult<String> {
    let sql = if table.per_locale() {
        format!(
            "select exists(select 1 from {} where tenant_id = $1 and slug = $2 and id <> $3 and locale = $4)",
            table.table()
        )
    } else {
        format!(
            "select exists(select 1 from {} where tenant_id = $1 and slug = $2 and id <> $3)",
            table.table()
        )
    };
    let base: String = base.chars().take(80).collect();
    let base = if base.is_empty() {
        "item".to_owned()
    } else {
        base
    };
    for n in 1..1000 {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let mut q = sqlx::query_scalar::<_, bool>(&sql)
            .bind(tenant_id)
            .bind(&candidate)
            .bind(own);
        if table.per_locale() {
            q = q.bind(locale.unwrap_or("tr"));
        }
        if !q.fetch_one(&mut *conn).await? {
            return Ok(candidate);
        }
    }
    Ok(format!("{base}-{}", &own.simple().to_string()[..8]))
}

/// The slug currently stored for `id` (keeps a slug stable across re-runs).
pub async fn current_slug(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    table: SlugTable,
    id: Uuid,
) -> DbResult<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(&format!(
        "select slug from {} where tenant_id = $1 and id = $2",
        table.table()
    ))
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

// ---------------------------------------------------------------------------
// reference data
// ---------------------------------------------------------------------------

/// `currencies`.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrencyInput {
    pub code: String,
    pub name: String,
    pub prefix: String,
    pub suffix: String,
    pub exchange_rate: Decimal,
    pub sort_order: i32,
}

/// Upserts a currency (never touches `is_default`).
pub async fn upsert_currency(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &CurrencyInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "currencies",
        &[
            "code",
            "name",
            "symbol",
            "prefix",
            "suffix",
            "exchange_rate",
            "sort_order",
        ],
        &[],
    );
    let symbol = if c.prefix.trim().is_empty() {
        c.suffix.trim()
    } else {
        c.prefix.trim()
    };
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&c.code)
        .bind(&c.name)
        .bind(symbol)
        .bind(&c.prefix)
        .bind(&c.suffix)
        .bind(c.exchange_rate)
        .bind(c.sort_order);
    run_upsert(q, conn, "currencies").await
}

/// `tax_rules`.
#[derive(Debug, Clone, PartialEq)]
pub struct TaxRuleInput {
    pub name: String,
    pub level: i16,
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub rate: Decimal,
    pub is_compound: bool,
}

/// Upserts a tax rule.
pub async fn upsert_tax_rule(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    r: &TaxRuleInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "tax_rules",
        &[
            "name",
            "level",
            "country_code",
            "region",
            "rate",
            "is_compound",
            "is_active",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&r.name)
        .bind(r.level)
        .bind(&r.country_code)
        .bind(&r.region)
        .bind(r.rate)
        .bind(r.is_compound)
        .bind(true);
    run_upsert(q, conn, "tax_rules").await
}

/// `client_groups`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientGroupInput {
    pub name: String,
    pub color: Option<String>,
    pub discount_percent: Decimal,
}

/// Upserts a client group.
pub async fn upsert_client_group(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    g: &ClientGroupInput,
) -> DbResult<bool> {
    let sql = upsert_sql("client_groups", &["name", "color", "discount_percent"], &[]);
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&g.name)
        .bind(&g.color)
        .bind(g.discount_percent);
    run_upsert(q, conn, "client_groups").await
}

/// A new (inactive) integration created from a source server.
#[derive(Debug, Clone, PartialEq)]
pub struct IntegrationInput {
    pub category: String,
    pub kind: String,
    pub name: String,
    pub config: Json,
    /// SecretBox(JSON) sealed with `integration_aad(tenant, id)`.
    pub secrets_enc: Option<Vec<u8>>,
}

/// Inserts an inactive integration; an existing row with `id` is left as
/// the admin configured it (returns `false`).
pub async fn insert_integration(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    i: &IntegrationInput,
) -> DbResult<bool> {
    let r = sqlx::query(
        "insert into integrations (id, tenant_id, category, kind, name, config, secrets_enc, is_active)
         values ($1, $2, $3, $4, $5, $6, $7, false)
         on conflict (id) do nothing",
    )
    .bind(id)
    .bind(t)
    .bind(&i.category)
    .bind(&i.kind)
    .bind(&i.name)
    .bind(&i.config)
    .bind(&i.secrets_enc)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// people
// ---------------------------------------------------------------------------

/// `users` (client logins and disabled staff).
#[derive(Clone, PartialEq)]
pub struct UserInput {
    pub email: String,
    /// Imported hash (bcrypt / legacy marker) or `None`.
    pub password_hash: Option<String>,
    /// `client` | `staff`.
    pub kind: String,
    pub first_name: String,
    pub last_name: String,
    pub phone: Option<String>,
    pub locale: String,
    pub email_verified_at: Option<DateTime<Utc>>,
    /// `active` | `disabled`.
    pub status: String,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for UserInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserInput")
            .field("email", &self.email)
            .field("kind", &self.kind)
            .field("has_password", &self.password_hash.is_some())
            .finish_non_exhaustive()
    }
}

/// Upserts a user. On update the password is replaced only while the
/// stored one is still an imported legacy value (or none): a user who
/// logged in since then has an argon2id hash that a re-sync must keep.
/// Staff rows keep the status and e-mail address they have (an admin's
/// activation must not be undone, nor the address of an activated account
/// changed by a re-run).
pub async fn upsert_user(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    u: &UserInput,
) -> DbResult<bool> {
    let inserted = sqlx::query_scalar::<_, bool>(
        "insert into users (id, tenant_id, email, password_hash, kind, first_name, last_name, phone, locale,
                            email_verified_at, status, last_login_at, created_at, password_changed_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, coalesce($13, now()), null)
         on conflict (id) do update
            set email = case when users.kind = 'staff' then users.email else excluded.email end,
                first_name = excluded.first_name, last_name = excluded.last_name,
                phone = excluded.phone,
                email_verified_at = coalesce(users.email_verified_at, excluded.email_verified_at),
                password_hash = case
                    when users.password_hash is null
                      or users.password_hash like '$2_$%'
                      or users.password_hash like '$hos-legacy$%'
                    then excluded.password_hash else users.password_hash end,
                status = case when users.kind = 'staff' then users.status else excluded.status end
          where users.tenant_id = excluded.tenant_id and users.kind = excluded.kind
         returning (xmax = 0)",
    )
    .bind(id)
    .bind(t)
    .bind(&u.email)
    .bind(&u.password_hash)
    .bind(&u.kind)
    .bind(&u.first_name)
    .bind(&u.last_name)
    .bind(&u.phone)
    .bind(&u.locale)
    .bind(u.email_verified_at)
    .bind(&u.status)
    .bind(u.last_login_at)
    .bind(u.created_at)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| not_written("users"))?;
    Ok(inserted)
}

/// Grants a role to a user (no-op when granted).
pub async fn grant_role(
    conn: &mut PgConnection,
    t: TenantId,
    user_id: Uuid,
    role_id: Uuid,
) -> DbResult<()> {
    sqlx::query(
        "insert into user_roles (id, tenant_id, user_id, role_id) values ($1, $2, $3, $4)
         on conflict (user_id, role_id) do nothing",
    )
    .bind(Uuid::now_v7())
    .bind(t)
    .bind(user_id)
    .bind(role_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `clients`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientInput {
    pub kind: String,
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
    pub tax_number: Option<String>,
    pub currency: String,
    pub locale: String,
    pub group_id: Option<Uuid>,
    pub status: String,
    pub tax_exempt: bool,
    pub late_fee_exempt: bool,
    pub overdue_notices: bool,
    pub separate_invoices: bool,
    pub marketing_opt_in: bool,
    pub default_gateway_id: Option<Uuid>,
    pub signup_ip: Option<IpAddr>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a client (`custom_fields` are merged separately).
pub async fn upsert_client(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &ClientInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "clients",
        &[
            "kind",
            "first_name",
            "last_name",
            "company_name",
            "email",
            "phone",
            "address_line1",
            "address_line2",
            "city",
            "state",
            "postal_code",
            "country_code",
            "tax_number",
            "currency",
            "locale",
            "group_id",
            "status",
            "tax_exempt",
            "late_fee_exempt",
            "overdue_notices",
            "separate_invoices",
            "marketing_opt_in",
            "default_gateway_id",
            "signup_ip",
            "closed_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&c.kind)
        .bind(&c.first_name)
        .bind(&c.last_name)
        .bind(&c.company_name)
        .bind(&c.email)
        .bind(&c.phone)
        .bind(&c.address_line1)
        .bind(&c.address_line2)
        .bind(&c.city)
        .bind(&c.state)
        .bind(&c.postal_code)
        .bind(&c.country_code)
        .bind(&c.tax_number)
        .bind(&c.currency)
        .bind(&c.locale)
        .bind(c.group_id)
        .bind(&c.status)
        .bind(c.tax_exempt)
        .bind(c.late_fee_exempt)
        .bind(c.overdue_notices)
        .bind(c.separate_invoices)
        .bind(c.marketing_opt_in)
        .bind(c.default_gateway_id)
        .bind(c.signup_ip)
        .bind(c.closed_at)
        .bind(c.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "clients").await
}

/// Links a user to a client. A new owner takes over ownership from an
/// earlier owner of the same client.
pub async fn link_client_user(
    conn: &mut PgConnection,
    t: TenantId,
    client_id: Uuid,
    user_id: Uuid,
    is_owner: bool,
    permissions: &[String],
) -> DbResult<()> {
    if is_owner {
        sqlx::query(
            "update client_users set is_owner = false
              where tenant_id = $1 and client_id = $2 and user_id <> $3 and is_owner",
        )
        .bind(t)
        .bind(client_id)
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query(
        "insert into client_users (id, tenant_id, client_id, user_id, is_owner, permissions)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (client_id, user_id) do update
            set is_owner = excluded.is_owner, permissions = excluded.permissions",
    )
    .bind(Uuid::now_v7())
    .bind(t)
    .bind(client_id)
    .bind(user_id)
    .bind(is_owner)
    .bind(permissions)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `contacts`.
#[derive(Debug, Clone, PartialEq)]
pub struct ContactInput {
    pub client_id: Uuid,
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
    pub email_preferences: Vec<String>,
}

/// Upserts a contact.
pub async fn upsert_contact(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &ContactInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "contacts",
        &[
            "client_id",
            "first_name",
            "last_name",
            "company_name",
            "email",
            "phone",
            "address_line1",
            "address_line2",
            "city",
            "state",
            "postal_code",
            "country_code",
            "email_preferences",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(c.client_id)
        .bind(&c.first_name)
        .bind(&c.last_name)
        .bind(&c.company_name)
        .bind(&c.email)
        .bind(&c.phone)
        .bind(&c.address_line1)
        .bind(&c.address_line2)
        .bind(&c.city)
        .bind(&c.state)
        .bind(&c.postal_code)
        .bind(&c.country_code)
        .bind(&c.email_preferences);
    run_upsert(q, conn, "contacts").await
}

/// `client_notes`.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteInput {
    pub client_id: Uuid,
    pub author_user_id: Option<Uuid>,
    pub body: String,
    pub is_sticky: bool,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a client note.
pub async fn upsert_note(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    n: &NoteInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "client_notes",
        &[
            "client_id",
            "author_user_id",
            "body",
            "is_sticky",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(n.client_id)
        .bind(n.author_user_id)
        .bind(&n.body)
        .bind(n.is_sticky)
        .bind(n.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "client_notes").await
}

/// Merges `values` into `clients.custom_fields` / `services.custom_fields`.
pub async fn merge_custom_fields(
    conn: &mut PgConnection,
    t: TenantId,
    service: bool,
    id: Uuid,
    values: &Json,
) -> DbResult<bool> {
    let table = if service { "services" } else { "clients" };
    let r = sqlx::query(&format!(
        "update {table} set custom_fields = custom_fields || $3 where tenant_id = $1 and id = $2"
    ))
    .bind(t)
    .bind(id)
    .bind(values)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// `credits` ledger line.
#[derive(Debug, Clone, PartialEq)]
pub struct CreditInput {
    pub client_id: Uuid,
    pub amount: Decimal,
    pub currency: String,
    pub kind: String,
    pub description: String,
    pub invoice_id: Option<Uuid>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a credit ledger line.
pub async fn upsert_credit(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &CreditInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "credits",
        &[
            "client_id",
            "amount",
            "currency",
            "kind",
            "description",
            "invoice_id",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(c.client_id)
        .bind(c.amount)
        .bind(&c.currency)
        .bind(&c.kind)
        .bind(&c.description)
        .bind(c.invoice_id)
        .bind(c.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "credits").await
}

/// Credit balance of a client in `currency` over the imported credit entries
/// only: entries made in HostingOS (credit spent or added after go-live, or
/// before an adopted account was imported) never count, so a re-import never
/// adds back credit the client already used.
pub async fn credit_balance(
    conn: &mut PgConnection,
    t: TenantId,
    client_id: Uuid,
    currency: &str,
) -> DbResult<Decimal> {
    Ok(sqlx::query_scalar::<_, Option<Decimal>>(
        "select sum(amount) from credits c
          where c.tenant_id = $1 and c.client_id = $2 and c.currency = $3
            and exists (select 1 from import_id_map m
                         where m.tenant_id = c.tenant_id and m.target_table = 'credits'
                           and m.target_id = c.id)",
    )
    .bind(t)
    .bind(client_id)
    .bind(currency)
    .fetch_one(&mut *conn)
    .await?
    .unwrap_or(Decimal::ZERO))
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

/// `product_groups`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductGroupInput {
    pub slug: String,
    pub name: String,
    pub tagline: Option<String>,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Upserts a product group.
pub async fn upsert_product_group(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    g: &ProductGroupInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "product_groups",
        &["slug", "name", "tagline", "is_hidden", "sort_order"],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&g.slug)
        .bind(&g.name)
        .bind(&g.tagline)
        .bind(g.is_hidden)
        .bind(g.sort_order);
    run_upsert(q, conn, "product_groups").await
}

/// `products`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductInput {
    pub group_id: Option<Uuid>,
    pub slug: String,
    pub product_type: String,
    pub name: String,
    pub description: Option<String>,
    pub integration_id: Option<Uuid>,
    pub module_config: Json,
    pub pay_type: String,
    pub auto_setup: String,
    pub requires_domain: String,
    pub stock_control: bool,
    pub stock_quantity: i32,
    pub allow_quantity: bool,
    pub is_taxable: bool,
    pub is_hidden: bool,
    pub is_retired: bool,
    pub sort_order: i32,
}

/// Upserts a product.
pub async fn upsert_product(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    p: &ProductInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "products",
        &[
            "group_id",
            "slug",
            "product_type",
            "name",
            "description",
            "integration_id",
            "module_config",
            "pay_type",
            "auto_setup",
            "requires_domain",
            "stock_control",
            "stock_quantity",
            "allow_quantity",
            "is_taxable",
            "is_hidden",
            "is_retired",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(p.group_id)
        .bind(&p.slug)
        .bind(&p.product_type)
        .bind(&p.name)
        .bind(&p.description)
        .bind(p.integration_id)
        .bind(&p.module_config)
        .bind(&p.pay_type)
        .bind(&p.auto_setup)
        .bind(&p.requires_domain)
        .bind(p.stock_control)
        .bind(p.stock_quantity)
        .bind(p.allow_quantity)
        .bind(p.is_taxable)
        .bind(p.is_hidden)
        .bind(p.is_retired)
        .bind(p.sort_order);
    run_upsert(q, conn, "products").await
}

/// Which price table [`upsert_price`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceTarget {
    /// `product_pricing (product_id, …)`.
    Product,
    /// `product_addon_pricing (addon_id, …)`.
    Addon,
    /// `config_option_pricing (value_id, …)`.
    ConfigValue,
}

/// One price per currency × cycle; `None` = cycle disabled.
pub async fn upsert_price(
    conn: &mut PgConnection,
    t: TenantId,
    target: PriceTarget,
    owner_id: Uuid,
    currency: &str,
    cycle: &str,
    price: Option<(Decimal, Decimal)>,
) -> DbResult<()> {
    match target {
        PriceTarget::Product => {
            let (p, s) = price.unwrap_or((Decimal::ZERO, Decimal::ZERO));
            sqlx::query(
                "insert into product_pricing (id, tenant_id, product_id, currency, billing_cycle, price, setup_fee, is_enabled)
                 values ($1, $2, $3, $4, $5, $6, $7, $8)
                 on conflict (product_id, currency, billing_cycle) do update
                    set price = excluded.price, setup_fee = excluded.setup_fee, is_enabled = excluded.is_enabled",
            )
            .bind(Uuid::now_v7())
            .bind(t)
            .bind(owner_id)
            .bind(currency)
            .bind(cycle)
            .bind(p)
            .bind(s)
            .bind(price.is_some())
            .execute(&mut *conn)
            .await?;
        }
        PriceTarget::Addon | PriceTarget::ConfigValue => {
            let (table, col) = if target == PriceTarget::Addon {
                ("product_addon_pricing", "addon_id")
            } else {
                ("config_option_pricing", "value_id")
            };
            match price {
                Some((p, s)) => {
                    sqlx::query(&format!(
                        "insert into {table} (id, tenant_id, {col}, currency, billing_cycle, price, setup_fee)
                         values ($1, $2, $3, $4, $5, $6, $7)
                         on conflict ({col}, currency, billing_cycle) do update
                            set price = excluded.price, setup_fee = excluded.setup_fee"
                    ))
                    .bind(Uuid::now_v7())
                    .bind(t)
                    .bind(owner_id)
                    .bind(currency)
                    .bind(cycle)
                    .bind(p)
                    .bind(s)
                    .execute(&mut *conn)
                    .await?;
                }
                None => {
                    sqlx::query(&format!(
                        "delete from {table} where tenant_id = $1 and {col} = $2 and currency = $3 and billing_cycle = $4"
                    ))
                    .bind(t)
                    .bind(owner_id)
                    .bind(currency)
                    .bind(cycle)
                    .execute(&mut *conn)
                    .await?;
                }
            }
        }
    }
    Ok(())
}

/// `config_option_groups`.
pub async fn upsert_config_group(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    name: &str,
    description: Option<&str>,
) -> DbResult<bool> {
    let sql = upsert_sql("config_option_groups", &["name", "description"], &[]);
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(name)
        .bind(description);
    run_upsert(q, conn, "config_option_groups").await
}

/// Links a config option group or an addon to a product.
pub async fn link_product(
    conn: &mut PgConnection,
    t: TenantId,
    addon: bool,
    owner_id: Uuid,
    product_id: Uuid,
) -> DbResult<()> {
    let (table, col) = if addon {
        ("product_addon_products", "addon_id")
    } else {
        ("config_option_group_products", "group_id")
    };
    sqlx::query(&format!(
        "insert into {table} (id, tenant_id, {col}, product_id) values ($1, $2, $3, $4)
         on conflict ({col}, product_id) do nothing"
    ))
    .bind(Uuid::now_v7())
    .bind(t)
    .bind(owner_id)
    .bind(product_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `config_options`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigOptionInput {
    pub group_id: Uuid,
    pub name: String,
    pub option_type: String,
    pub module_key: Option<String>,
    pub min_quantity: i32,
    pub max_quantity: i32,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Upserts a configurable option.
pub async fn upsert_config_option(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    o: &ConfigOptionInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "config_options",
        &[
            "group_id",
            "name",
            "option_type",
            "module_key",
            "min_quantity",
            "max_quantity",
            "is_hidden",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(o.group_id)
        .bind(&o.name)
        .bind(&o.option_type)
        .bind(&o.module_key)
        .bind(o.min_quantity)
        .bind(o.max_quantity)
        .bind(o.is_hidden)
        .bind(o.sort_order);
    run_upsert(q, conn, "config_options").await
}

/// Upserts a configurable option value.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_config_value(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    option_id: Uuid,
    label: &str,
    value: &str,
    is_hidden: bool,
    sort_order: i32,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "config_option_values",
        &["option_id", "label", "value", "is_hidden", "sort_order"],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(option_id)
        .bind(label)
        .bind(value)
        .bind(is_hidden)
        .bind(sort_order);
    run_upsert(q, conn, "config_option_values").await
}

/// `product_addons`.
#[derive(Debug, Clone, PartialEq)]
pub struct AddonInput {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub pay_type: String,
    pub auto_activate: bool,
    pub show_on_order: bool,
    pub is_taxable: bool,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Upserts an addon.
pub async fn upsert_addon(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    a: &AddonInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "product_addons",
        &[
            "slug",
            "name",
            "description",
            "pay_type",
            "auto_activate",
            "show_on_order",
            "is_taxable",
            "is_hidden",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&a.slug)
        .bind(&a.name)
        .bind(&a.description)
        .bind(&a.pay_type)
        .bind(a.auto_activate)
        .bind(a.show_on_order)
        .bind(a.is_taxable)
        .bind(a.is_hidden)
        .bind(a.sort_order);
    run_upsert(q, conn, "product_addons").await
}

/// `coupons`.
#[derive(Debug, Clone, PartialEq)]
pub struct CouponInput {
    pub code: String,
    pub description: Option<String>,
    pub discount_type: String,
    pub value: Decimal,
    pub currency: Option<String>,
    pub product_ids: Vec<Uuid>,
    pub addon_ids: Vec<Uuid>,
    pub tlds: Vec<String>,
    pub billing_cycles: Vec<String>,
    pub recurring: bool,
    pub recurring_limit: i32,
    pub new_clients_only: bool,
    pub once_per_client: bool,
    pub max_uses: Option<i32>,
    pub uses_count: i32,
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub is_active: bool,
}

/// Upserts a coupon.
pub async fn upsert_coupon(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &CouponInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "coupons",
        &[
            "code",
            "description",
            "discount_type",
            "value",
            "currency",
            "product_ids",
            "addon_ids",
            "tlds",
            "billing_cycles",
            "recurring",
            "recurring_limit",
            "new_clients_only",
            "once_per_client",
            "max_uses",
            "uses_count",
            "starts_at",
            "expires_at",
            "is_active",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&c.code)
        .bind(&c.description)
        .bind(&c.discount_type)
        .bind(c.value)
        .bind(&c.currency)
        .bind(&c.product_ids)
        .bind(&c.addon_ids)
        .bind(&c.tlds)
        .bind(&c.billing_cycles)
        .bind(c.recurring)
        .bind(c.recurring_limit)
        .bind(c.new_clients_only)
        .bind(c.once_per_client)
        .bind(c.max_uses)
        .bind(c.uses_count)
        .bind(c.starts_at)
        .bind(c.expires_at)
        .bind(c.is_active);
    run_upsert(q, conn, "coupons").await
}

/// `tlds`.
#[derive(Debug, Clone, PartialEq)]
pub struct TldInput {
    pub tld: String,
    pub registrar_integration_id: Option<Uuid>,
    pub requires_epp: bool,
    pub supports_privacy: bool,
    pub supports_dns_management: bool,
    pub grace_period_days: i32,
    pub redemption_period_days: i32,
    pub sort_order: i32,
}

/// Upserts a TLD.
pub async fn upsert_tld(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    d: &TldInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "tlds",
        &[
            "tld",
            "registrar_integration_id",
            "requires_epp",
            "supports_privacy",
            "supports_dns_management",
            "grace_period_days",
            "redemption_period_days",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&d.tld)
        .bind(d.registrar_integration_id)
        .bind(d.requires_epp)
        .bind(d.supports_privacy)
        .bind(d.supports_dns_management)
        .bind(d.grace_period_days)
        .bind(d.redemption_period_days)
        .bind(d.sort_order);
    run_upsert(q, conn, "tlds").await
}

/// One TLD price (`None` removes it).
pub async fn upsert_tld_price(
    conn: &mut PgConnection,
    t: TenantId,
    tld_id: Uuid,
    currency: &str,
    action: &str,
    years: i16,
    price: Option<Decimal>,
) -> DbResult<()> {
    match price {
        Some(p) => {
            sqlx::query(
                "insert into tld_pricing (id, tenant_id, tld_id, currency, action, years, price)
                 values ($1, $2, $3, $4, $5, $6, $7)
                 on conflict (tld_id, currency, action, years) do update set price = excluded.price",
            )
            .bind(Uuid::now_v7())
            .bind(t)
            .bind(tld_id)
            .bind(currency)
            .bind(action)
            .bind(years)
            .bind(p)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query(
                "delete from tld_pricing where tenant_id = $1 and tld_id = $2 and currency = $3
                   and action = $4 and years = $5",
            )
            .bind(t)
            .bind(tld_id)
            .bind(currency)
            .bind(action)
            .bind(years)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sales
// ---------------------------------------------------------------------------

/// `orders` (lines are not rebuilt; services / domains point at the order).
#[derive(Debug, Clone, PartialEq)]
pub struct OrderInput {
    pub order_number: String,
    pub client_id: Uuid,
    pub status: String,
    pub currency: String,
    pub total: Decimal,
    pub coupon_code: Option<String>,
    pub payment_integration_id: Option<Uuid>,
    pub invoice_id: Option<Uuid>,
    pub ip: Option<IpAddr>,
    pub fraud_details: Json,
    pub notes: Option<String>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts an order.
pub async fn upsert_order(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    o: &OrderInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "orders",
        &[
            "order_number",
            "client_id",
            "status",
            "currency",
            "subtotal",
            "total",
            "coupon_code",
            "payment_integration_id",
            "invoice_id",
            "ip",
            "fraud_details",
            "admin_notes",
            "accepted_at",
            "cancelled_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&o.order_number)
        .bind(o.client_id)
        .bind(&o.status)
        .bind(&o.currency)
        .bind(o.total)
        .bind(o.total)
        .bind(&o.coupon_code)
        .bind(o.payment_integration_id)
        .bind(o.invoice_id)
        .bind(o.ip)
        .bind(&o.fraud_details)
        .bind(&o.notes)
        .bind(o.accepted_at)
        .bind(o.cancelled_at)
        .bind(o.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "orders").await
}

/// `services` (`config_selections`, `custom_fields` and cancellation
/// requests are written by their own entities).
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceInput {
    pub client_id: Uuid,
    pub product_id: Uuid,
    pub order_id: Option<Uuid>,
    pub status: String,
    pub billing_cycle: String,
    pub first_payment_amount: Decimal,
    pub recurring_amount: Decimal,
    pub currency: String,
    pub quantity: i32,
    pub registration_date: NaiveDate,
    pub next_due_date: Option<NaiveDate>,
    pub next_invoice_date: Option<NaiveDate>,
    pub integration_id: Option<Uuid>,
    pub external_id: Option<String>,
    pub username: Option<String>,
    pub password_enc: Option<Vec<u8>>,
    pub domain: Option<String>,
    pub server_hostname: Option<String>,
    pub dedicated_ip: Option<IpAddr>,
    pub assigned_ips: Vec<IpAddr>,
    pub module_data: Json,
    pub admin_notes: Option<String>,
    pub override_suspend_until: Option<NaiveDate>,
    pub suspended_at: Option<DateTime<Utc>>,
    pub suspended_reason: Option<String>,
    pub terminated_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a service. Never queues provisioning.
pub async fn upsert_service(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    s: &ServiceInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "services",
        &[
            "client_id",
            "product_id",
            "order_id",
            "status",
            "billing_cycle",
            "first_payment_amount",
            "recurring_amount",
            "currency",
            "quantity",
            "registration_date",
            "next_due_date",
            "next_invoice_date",
            "integration_id",
            "external_id",
            "username",
            "password_enc",
            "domain",
            "server_hostname",
            "dedicated_ip",
            "assigned_ips",
            "module_data",
            "admin_notes",
            "override_suspend_until",
            "suspended_at",
            "suspended_reason",
            "terminated_at",
            "cancelled_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(s.client_id)
        .bind(s.product_id)
        .bind(s.order_id)
        .bind(&s.status)
        .bind(&s.billing_cycle)
        .bind(s.first_payment_amount)
        .bind(s.recurring_amount)
        .bind(&s.currency)
        .bind(s.quantity)
        .bind(s.registration_date)
        .bind(s.next_due_date)
        .bind(s.next_invoice_date)
        .bind(s.integration_id)
        .bind(&s.external_id)
        .bind(&s.username)
        .bind(&s.password_enc)
        .bind(&s.domain)
        .bind(&s.server_hostname)
        .bind(s.dedicated_ip)
        .bind(&s.assigned_ips)
        .bind(&s.module_data)
        .bind(&s.admin_notes)
        .bind(s.override_suspend_until)
        .bind(s.suspended_at)
        .bind(&s.suspended_reason)
        .bind(s.terminated_at)
        .bind(s.cancelled_at)
        .bind(s.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "services").await
}

/// Merges `{"<config_option_id>": {"value_id", "quantity"}}` into a service.
pub async fn merge_config_selection(
    conn: &mut PgConnection,
    t: TenantId,
    service_id: Uuid,
    selection: &Json,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update services set config_selections = config_selections || $3 where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(service_id)
    .bind(selection)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Records a cancellation request on a service.
pub async fn set_cancellation(
    conn: &mut PgConnection,
    t: TenantId,
    service_id: Uuid,
    requested_at: DateTime<Utc>,
    cancellation_type: &str,
    reason: Option<&str>,
) -> DbResult<bool> {
    let r = sqlx::query(
        "update services set cancellation_requested_at = $3, cancellation_type = $4, cancellation_reason = $5
          where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(service_id)
    .bind(requested_at)
    .bind(cancellation_type)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// `service_addons`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceAddonInput {
    pub service_id: Uuid,
    pub addon_id: Uuid,
    pub order_id: Option<Uuid>,
    pub status: String,
    pub billing_cycle: String,
    pub setup_fee: Decimal,
    pub recurring_amount: Decimal,
    pub currency: String,
    pub registration_date: NaiveDate,
    pub next_due_date: Option<NaiveDate>,
    pub next_invoice_date: Option<NaiveDate>,
    pub notes: Option<String>,
    pub terminated_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a service addon.
pub async fn upsert_service_addon(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    a: &ServiceAddonInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "service_addons",
        &[
            "service_id",
            "addon_id",
            "order_id",
            "status",
            "billing_cycle",
            "setup_fee",
            "recurring_amount",
            "currency",
            "registration_date",
            "next_due_date",
            "next_invoice_date",
            "notes",
            "terminated_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(a.service_id)
        .bind(a.addon_id)
        .bind(a.order_id)
        .bind(&a.status)
        .bind(&a.billing_cycle)
        .bind(a.setup_fee)
        .bind(a.recurring_amount)
        .bind(&a.currency)
        .bind(a.registration_date)
        .bind(a.next_due_date)
        .bind(a.next_invoice_date)
        .bind(&a.notes)
        .bind(a.terminated_at)
        .bind(a.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "service_addons").await
}

/// `domains`.
#[derive(Debug, Clone, PartialEq)]
pub struct DomainInput {
    pub client_id: Uuid,
    pub order_id: Option<Uuid>,
    pub name: String,
    pub tld: String,
    pub registrar_integration_id: Option<Uuid>,
    pub status: String,
    pub registration_type: String,
    pub registration_period: i16,
    pub registered_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub next_due_date: Option<NaiveDate>,
    pub next_invoice_date: Option<NaiveDate>,
    pub first_payment_amount: Decimal,
    pub recurring_amount: Decimal,
    pub currency: String,
    pub auto_renew: bool,
    pub privacy: bool,
    pub dns_management: bool,
    pub email_forwarding: bool,
    pub nameservers: Vec<String>,
    pub admin_notes: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a domain (nameservers are kept when the source has none).
pub async fn upsert_domain(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    d: &DomainInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "domains",
        &[
            "client_id", "order_id", "name", "tld", "registrar_integration_id", "status", "registration_type",
            "registration_period", "registered_at", "expires_at", "next_due_date", "next_invoice_date",
            "first_payment_amount", "recurring_amount", "currency", "auto_renew", "privacy",
            "dns_management", "email_forwarding", "nameservers", "admin_notes", "created_at",
        ],
        &[],
    )
    .replace(
        "nameservers = excluded.nameservers",
        "nameservers = case when cardinality(excluded.nameservers) > 0 then excluded.nameservers else domains.nameservers end",
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(d.client_id)
        .bind(d.order_id)
        .bind(&d.name)
        .bind(&d.tld)
        .bind(d.registrar_integration_id)
        .bind(&d.status)
        .bind(&d.registration_type)
        .bind(d.registration_period)
        .bind(d.registered_at)
        .bind(d.expires_at)
        .bind(d.next_due_date)
        .bind(d.next_invoice_date)
        .bind(d.first_payment_amount)
        .bind(d.recurring_amount)
        .bind(&d.currency)
        .bind(d.auto_renew)
        .bind(d.privacy)
        .bind(d.dns_management)
        .bind(d.email_forwarding)
        .bind(&d.nameservers)
        .bind(&d.admin_notes)
        .bind(d.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "domains").await
}

/// `invoices` (with its tax breakdown).
#[derive(Debug, Clone, PartialEq)]
pub struct InvoiceInput {
    pub client_id: Uuid,
    pub number: Option<String>,
    pub seq: Option<i64>,
    pub status: String,
    pub currency: String,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    pub paid_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub refunded_at: Option<DateTime<Utc>>,
    pub subtotal: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub amount_paid: Decimal,
    pub credit_applied: Decimal,
    pub payment_integration_id: Option<Uuid>,
    pub billing_snapshot: Json,
    pub notes: Option<String>,
    pub late_fee_applied_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    /// `(name, rate, is_compound, taxable_amount, tax_amount)`.
    pub tax_lines: Vec<(String, Decimal, bool, Decimal, Decimal)>,
}

/// Upserts an invoice and replaces its tax lines. `late_fee_applied_at`
/// is only ever set, never cleared.
pub async fn upsert_invoice(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    i: &InvoiceInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "invoices",
        &[
            "client_id", "number", "seq", "status", "currency", "issue_date", "due_date", "paid_at",
            "cancelled_at", "refunded_at", "subtotal", "tax_total", "total", "amount_paid", "credit_applied",
            "payment_integration_id", "billing_snapshot", "notes", "late_fee_applied_at", "created_at",
        ],
        &[],
    )
    .replace(
        "late_fee_applied_at = excluded.late_fee_applied_at",
        "late_fee_applied_at = coalesce(invoices.late_fee_applied_at, excluded.late_fee_applied_at)",
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(i.client_id)
        .bind(&i.number)
        .bind(i.seq)
        .bind(&i.status)
        .bind(&i.currency)
        .bind(i.issue_date)
        .bind(i.due_date)
        .bind(i.paid_at)
        .bind(i.cancelled_at)
        .bind(i.refunded_at)
        .bind(i.subtotal)
        .bind(i.tax_total)
        .bind(i.total)
        .bind(i.amount_paid)
        .bind(i.credit_applied)
        .bind(i.payment_integration_id)
        .bind(&i.billing_snapshot)
        .bind(&i.notes)
        .bind(i.late_fee_applied_at)
        .bind(i.created_at.unwrap_or_else(Utc::now));
    let inserted = run_upsert(q, conn, "invoices").await?;
    sqlx::query("delete from invoice_tax_lines where tenant_id = $1 and invoice_id = $2")
        .bind(t)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for (n, (name, rate, compound, taxable, amount)) in i.tax_lines.iter().enumerate() {
        sqlx::query(
            "insert into invoice_tax_lines (id, tenant_id, invoice_id, name, rate, is_compound, sort_order,
                                            taxable_amount, tax_amount)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::now_v7())
        .bind(t)
        .bind(id)
        .bind(name)
        .bind(rate)
        .bind(compound)
        .bind(i32::try_from(n).unwrap_or(0))
        .bind(taxable)
        .bind(amount)
        .execute(&mut *conn)
        .await?;
    }
    Ok(inserted)
}

/// `invoice_items`.
#[derive(Debug, Clone, PartialEq)]
pub struct InvoiceItemInput {
    pub invoice_id: Uuid,
    pub item_type: String,
    pub service_id: Option<Uuid>,
    pub service_addon_id: Option<Uuid>,
    pub domain_id: Option<Uuid>,
    pub description: String,
    pub amount: Decimal,
    pub taxable: bool,
    pub period_start: Option<NaiveDate>,
    pub period_end: Option<NaiveDate>,
    pub sort_order: i32,
}

/// Upserts an invoice line.
pub async fn upsert_invoice_item(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    i: &InvoiceItemInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "invoice_items",
        &[
            "invoice_id",
            "item_type",
            "service_id",
            "service_addon_id",
            "domain_id",
            "description",
            "quantity",
            "unit_price",
            "amount",
            "taxable",
            "period_start",
            "period_end",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(i.invoice_id)
        .bind(&i.item_type)
        .bind(i.service_id)
        .bind(i.service_addon_id)
        .bind(i.domain_id)
        .bind(&i.description)
        .bind(Decimal::ONE)
        .bind(i.amount)
        .bind(i.amount)
        .bind(i.taxable)
        .bind(i.period_start)
        .bind(i.period_end)
        .bind(i.sort_order);
    run_upsert(q, conn, "invoice_items").await
}

/// Points an order at its invoice.
pub async fn set_order_invoice(
    conn: &mut PgConnection,
    t: TenantId,
    order_id: Uuid,
    invoice_id: Uuid,
) -> DbResult<()> {
    sqlx::query("update orders set invoice_id = $3 where tenant_id = $1 and id = $2")
        .bind(t)
        .bind(order_id)
        .bind(invoice_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `transactions`.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionInput {
    pub client_id: Uuid,
    pub invoice_id: Option<Uuid>,
    pub integration_id: Option<Uuid>,
    pub gateway: String,
    pub gateway_txn_id: Option<String>,
    pub kind: String,
    pub amount: Decimal,
    pub fee: Decimal,
    pub currency: String,
    pub description: Option<String>,
    pub raw: Json,
    pub occurred_at: DateTime<Utc>,
}

/// Whether `gateway_txn_id` is already used by another row of `integration_id`.
pub async fn gateway_txn_taken(
    conn: &mut PgConnection,
    t: TenantId,
    integration_id: Option<Uuid>,
    gateway_txn_id: &str,
    own: Uuid,
) -> DbResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from transactions where tenant_id = $1 and integration_id is not distinct from $2
                         and gateway_txn_id = $3 and id <> $4)",
    )
    .bind(t)
    .bind(integration_id)
    .bind(gateway_txn_id)
    .bind(own)
    .fetch_one(&mut *conn)
    .await?)
}

/// Upserts a payment / refund transaction (status `succeeded`).
pub async fn upsert_transaction(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    x: &TransactionInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "transactions",
        &[
            "client_id",
            "invoice_id",
            "integration_id",
            "gateway",
            "gateway_txn_id",
            "kind",
            "status",
            "amount",
            "fee",
            "currency",
            "description",
            "raw",
            "occurred_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(x.client_id)
        .bind(x.invoice_id)
        .bind(x.integration_id)
        .bind(&x.gateway)
        .bind(&x.gateway_txn_id)
        .bind(&x.kind)
        .bind("succeeded")
        .bind(x.amount)
        .bind(x.fee)
        .bind(&x.currency)
        .bind(&x.description)
        .bind(&x.raw)
        .bind(x.occurred_at)
        .bind(x.occurred_at);
    run_upsert(q, conn, "transactions").await
}

/// `refunds` row linking a refund transaction to the refunded payment.
#[derive(Debug, Clone, PartialEq)]
pub struct RefundInput {
    pub client_id: Uuid,
    pub transaction_id: Uuid,
    pub invoice_id: Option<Uuid>,
    pub refund_transaction_id: Uuid,
    pub integration_id: Option<Uuid>,
    pub amount: Decimal,
    pub currency: String,
    pub processed_at: DateTime<Utc>,
}

/// Upserts a (succeeded) refund.
pub async fn upsert_refund(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    r: &RefundInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "refunds",
        &[
            "client_id",
            "transaction_id",
            "invoice_id",
            "refund_transaction_id",
            "integration_id",
            "amount",
            "currency",
            "status",
            "processed_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(r.client_id)
        .bind(r.transaction_id)
        .bind(r.invoice_id)
        .bind(r.refund_transaction_id)
        .bind(r.integration_id)
        .bind(r.amount)
        .bind(&r.currency)
        .bind("succeeded")
        .bind(r.processed_at)
        .bind(r.processed_at);
    run_upsert(q, conn, "refunds").await
}

// ---------------------------------------------------------------------------
// support & content
// ---------------------------------------------------------------------------

/// `ticket_departments`.
#[derive(Debug, Clone, PartialEq)]
pub struct DepartmentInput {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub email: Option<String>,
    pub clients_only: bool,
    pub auto_respond: bool,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Upserts a department.
pub async fn upsert_department(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    d: &DepartmentInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "ticket_departments",
        &[
            "slug",
            "name",
            "description",
            "email",
            "clients_only",
            "auto_respond",
            "is_hidden",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&d.slug)
        .bind(&d.name)
        .bind(&d.description)
        .bind(&d.email)
        .bind(d.clients_only)
        .bind(d.auto_respond)
        .bind(d.is_hidden)
        .bind(d.sort_order);
    run_upsert(q, conn, "ticket_departments").await
}

/// `tickets`.
#[derive(Debug, Clone, PartialEq)]
pub struct TicketInput {
    pub number: String,
    pub department_id: Uuid,
    pub client_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub guest_name: Option<String>,
    pub guest_email: Option<String>,
    pub subject: String,
    pub status: String,
    pub priority: String,
    pub service_id: Option<Uuid>,
    pub domain_id: Option<Uuid>,
    pub assigned_to: Option<Uuid>,
    pub last_reply_at: DateTime<Utc>,
    pub last_reply_by: String,
    pub closed_at: Option<DateTime<Utc>>,
    pub cc_emails: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// Upserts a ticket.
pub async fn upsert_ticket(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    k: &TicketInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "tickets",
        &[
            "number",
            "department_id",
            "client_id",
            "user_id",
            "guest_name",
            "guest_email",
            "subject",
            "status",
            "priority",
            "service_id",
            "domain_id",
            "assigned_to",
            "last_reply_at",
            "last_reply_by",
            "closed_at",
            "cc_emails",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&k.number)
        .bind(k.department_id)
        .bind(k.client_id)
        .bind(k.user_id)
        .bind(&k.guest_name)
        .bind(&k.guest_email)
        .bind(&k.subject)
        .bind(&k.status)
        .bind(&k.priority)
        .bind(k.service_id)
        .bind(k.domain_id)
        .bind(k.assigned_to)
        .bind(k.last_reply_at)
        .bind(&k.last_reply_by)
        .bind(k.closed_at)
        .bind(&k.cc_emails)
        .bind(k.created_at);
    run_upsert(q, conn, "tickets").await
}

/// `ticket_messages`.
#[derive(Debug, Clone, PartialEq)]
pub struct TicketMessageInput {
    pub ticket_id: Uuid,
    pub author_kind: String,
    pub user_id: Option<Uuid>,
    pub author_name: String,
    pub author_email: Option<String>,
    pub body: String,
    pub is_internal: bool,
    pub created_at: DateTime<Utc>,
}

/// Upserts a ticket message.
pub async fn upsert_ticket_message(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    m: &TicketMessageInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "ticket_messages",
        &[
            "ticket_id",
            "author_kind",
            "user_id",
            "author_name",
            "author_email",
            "body",
            "is_internal",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(m.ticket_id)
        .bind(&m.author_kind)
        .bind(m.user_id)
        .bind(&m.author_name)
        .bind(&m.author_email)
        .bind(&m.body)
        .bind(m.is_internal)
        .bind(m.created_at);
    run_upsert(q, conn, "ticket_messages").await
}

/// An attachment stored in the database (`storage_key = db:<id>`).
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentInput {
    pub ticket_id: Uuid,
    pub message_id: Option<Uuid>,
    pub filename: String,
    pub content_type: String,
    pub content: Vec<u8>,
    pub sha256: String,
}

/// Upserts an attachment.
pub async fn upsert_attachment(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    a: &AttachmentInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "ticket_attachments",
        &[
            "ticket_id",
            "message_id",
            "filename",
            "content_type",
            "size_bytes",
            "storage_key",
            "sha256",
            "content",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(a.ticket_id)
        .bind(a.message_id)
        .bind(&a.filename)
        .bind(&a.content_type)
        .bind(i64::try_from(a.content.len()).unwrap_or(i64::MAX))
        .bind(format!("db:{id}"))
        .bind(&a.sha256)
        .bind(&a.content);
    run_upsert(q, conn, "ticket_attachments").await
}

/// Upserts a canned response.
pub async fn upsert_canned(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    title: &str,
    body: &str,
    locale: &str,
    category: Option<&str>,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "canned_responses",
        &["title", "body", "locale", "category"],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(title)
        .bind(body)
        .bind(locale)
        .bind(category);
    run_upsert(q, conn, "canned_responses").await
}

/// `kb_categories`.
#[derive(Debug, Clone, PartialEq)]
pub struct KbCategoryInput {
    pub parent_id: Option<Uuid>,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Upserts a KB category.
pub async fn upsert_kb_category(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &KbCategoryInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "kb_categories",
        &[
            "parent_id",
            "slug",
            "name",
            "description",
            "is_hidden",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(c.parent_id)
        .bind(&c.slug)
        .bind(&c.name)
        .bind(&c.description)
        .bind(c.is_hidden)
        .bind(c.sort_order);
    run_upsert(q, conn, "kb_categories").await
}

/// `kb_articles` / `announcements` (Markdown body).
#[derive(Debug, Clone, PartialEq)]
pub struct ArticleInput {
    pub category_id: Option<Uuid>,
    pub slug: String,
    pub locale: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub published_at: Option<DateTime<Utc>>,
    pub sort_order: i32,
    pub views: i64,
    pub helpful_yes: i32,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a KB article.
pub async fn upsert_kb_article(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    a: &ArticleInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "kb_articles",
        &[
            "category_id",
            "slug",
            "locale",
            "title",
            "body",
            "status",
            "published_at",
            "sort_order",
            "views",
            "helpful_yes",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(a.category_id)
        .bind(&a.slug)
        .bind(&a.locale)
        .bind(&a.title)
        .bind(&a.body)
        .bind(&a.status)
        .bind(a.published_at)
        .bind(a.sort_order)
        .bind(a.views)
        .bind(a.helpful_yes)
        .bind(a.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "kb_articles").await
}

/// Upserts an announcement.
pub async fn upsert_announcement(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    a: &ArticleInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "announcements",
        &[
            "slug",
            "locale",
            "title",
            "body",
            "status",
            "published_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&a.slug)
        .bind(&a.locale)
        .bind(&a.title)
        .bind(&a.body)
        .bind(&a.status)
        .bind(a.published_at)
        .bind(a.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "announcements").await
}

// ---------------------------------------------------------------------------
// quotes, affiliates, to-dos, bans, downloads, network issues
// ---------------------------------------------------------------------------

/// `quotes` (a client's or a prospect's).
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteInput {
    pub client_id: Option<Uuid>,
    pub prospect_name: Option<String>,
    pub prospect_email: Option<String>,
    pub prospect_company: Option<String>,
    pub subject: String,
    pub status: String,
    pub currency: String,
    pub valid_until: NaiveDate,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub subtotal: Decimal,
    pub tax_total: Decimal,
    pub total: Decimal,
    pub sent_at: Option<DateTime<Utc>>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub declined_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a quote. A new one gets the next number of the tenant (same
/// per-tenant lock as quotes created in the admin area); a re-sync keeps it.
pub async fn upsert_quote(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    q: &QuoteInput,
) -> DbResult<bool> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('quotes:' || $1::text, 0))")
        .bind(t)
        .execute(&mut *conn)
        .await?;
    let r = sqlx::query_scalar::<_, bool>(
        "insert into quotes (id, tenant_id, seq, client_id, prospect_name, prospect_email,
                prospect_company, subject, status, currency, valid_until, notes, admin_notes,
                subtotal, tax_total, total, sent_at, accepted_at, declined_at, created_at)
         select $1, $2, coalesce(max(seq), 0) + 1, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                $13, $14, $15, $16, $17, $18, $19
           from quotes where tenant_id = $2
         on conflict (id) do update set client_id = excluded.client_id,
                prospect_name = excluded.prospect_name, prospect_email = excluded.prospect_email,
                prospect_company = excluded.prospect_company, subject = excluded.subject,
                status = excluded.status, currency = excluded.currency,
                valid_until = excluded.valid_until, notes = excluded.notes,
                admin_notes = excluded.admin_notes, subtotal = excluded.subtotal,
                tax_total = excluded.tax_total, total = excluded.total,
                sent_at = excluded.sent_at, accepted_at = excluded.accepted_at,
                declined_at = excluded.declined_at, created_at = excluded.created_at
          where quotes.tenant_id = excluded.tenant_id
         returning (xmax = 0)",
    )
    .bind(id)
    .bind(t)
    .bind(q.client_id)
    .bind(&q.prospect_name)
    .bind(&q.prospect_email)
    .bind(&q.prospect_company)
    .bind(&q.subject)
    .bind(&q.status)
    .bind(&q.currency)
    .bind(q.valid_until)
    .bind(&q.notes)
    .bind(&q.admin_notes)
    .bind(q.subtotal)
    .bind(q.tax_total)
    .bind(q.total)
    .bind(q.sent_at)
    .bind(q.accepted_at)
    .bind(q.declined_at)
    .bind(q.created_at.unwrap_or_else(Utc::now))
    .fetch_optional(&mut *conn)
    .await?;
    r.ok_or_else(|| not_written("quotes"))
}

/// `quote_items`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteItemInput {
    pub quote_id: Uuid,
    pub description: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub discount_percent: Decimal,
    pub amount: Decimal,
    pub taxable: bool,
    pub sort_order: i32,
}

/// Upserts a quote line.
pub async fn upsert_quote_item(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    i: &QuoteItemInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "quote_items",
        &[
            "quote_id",
            "description",
            "quantity",
            "unit_price",
            "discount_percent",
            "amount",
            "taxable",
            "sort_order",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(i.quote_id)
        .bind(&i.description)
        .bind(i.quantity)
        .bind(i.unit_price)
        .bind(i.discount_percent)
        .bind(i.amount)
        .bind(i.taxable)
        .bind(i.sort_order);
    run_upsert(q, conn, "quote_items").await
}

/// `affiliates`.
#[derive(Debug, Clone, PartialEq)]
pub struct AffiliateInput {
    pub client_id: Uuid,
    pub code: String,
    pub commission_type: Option<String>,
    pub commission_value: Option<Decimal>,
    pub commission_recurring: Option<bool>,
    pub currency: String,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts an affiliate account (the referral code is kept on a re-sync).
pub async fn upsert_affiliate(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    a: &AffiliateInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "affiliates",
        &[
            "client_id",
            "code",
            "commission_type",
            "commission_value",
            "commission_recurring",
            "currency",
            "created_at",
        ],
        &["code"],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(a.client_id)
        .bind(&a.code)
        .bind(&a.commission_type)
        .bind(a.commission_value)
        .bind(a.commission_recurring)
        .bind(&a.currency)
        .bind(a.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "affiliates").await
}

/// The affiliate referral of a client, if any: `(referral id, affiliate id)`.
pub async fn referral_of_client(
    conn: &mut PgConnection,
    t: TenantId,
    client_id: Uuid,
) -> DbResult<Option<(Uuid, Uuid)>> {
    Ok(sqlx::query_as::<_, (Uuid, Uuid)>(
        "select id, affiliate_id from affiliate_referrals where tenant_id = $1 and client_id = $2",
    )
    .bind(t)
    .bind(client_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Upserts a referral (`source = 'staff'`: assigned, not from a link visit).
pub async fn upsert_affiliate_referral(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    affiliate_id: Uuid,
    client_id: Uuid,
    created_at: Option<DateTime<Utc>>,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "affiliate_referrals",
        &["affiliate_id", "client_id", "source", "created_at"],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(affiliate_id)
        .bind(client_id)
        .bind("staff")
        .bind(created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "affiliate_referrals").await
}

/// An imported commission (history line): kind `one_time`, no invoice line.
#[derive(Debug, Clone, PartialEq)]
pub struct CommissionInput {
    pub affiliate_id: Uuid,
    pub referral_id: Uuid,
    pub client_id: Uuid,
    pub service_id: Option<Uuid>,
    pub amount: Decimal,
    pub currency: String,
    pub created_at: DateTime<Utc>,
}

/// Upserts a history commission. New rows start `withdrawn` (already
/// accounted for in WHMCS); [`reconcile_affiliate_balance`] then makes the
/// newest ones `available` up to the WHMCS balance. The status is kept on a
/// re-sync.
pub async fn upsert_commission(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &CommissionInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "affiliate_commissions",
        &[
            "affiliate_id",
            "referral_id",
            "client_id",
            "service_id",
            "kind",
            "amount",
            "currency",
            "status",
            "clears_at",
            "created_at",
        ],
        &["status"],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(c.affiliate_id)
        .bind(c.referral_id)
        .bind(c.client_id)
        .bind(c.service_id)
        .bind("one_time")
        .bind(c.amount)
        .bind(&c.currency)
        .bind("withdrawn")
        .bind(c.created_at)
        .bind(c.created_at);
    run_upsert(q, conn, "affiliate_commissions").await
}

/// Brings an affiliate's cleared balance to `balance` with its imported
/// commissions (no invoice line, not in a payout): the newest ones whose
/// running total fits become `available`, older ones `withdrawn`. Returns
/// the part of `balance` they could not cover (zero when the history
/// explains the whole balance).
pub async fn reconcile_affiliate_balance(
    conn: &mut PgConnection,
    t: TenantId,
    affiliate_id: Uuid,
    balance: Decimal,
    opening_balance_id: Option<Uuid>,
) -> DbResult<Decimal> {
    let covered: Option<Decimal> = sqlx::query_scalar(
        "with h as (
             select id, sum(amount) over (order by created_at desc, id desc) as run
               from affiliate_commissions
              where tenant_id = $1 and affiliate_id = $2 and invoice_item_id is null
                and kind = 'one_time' and payout_id is null
                and status in ('available', 'withdrawn')
                and ($4::uuid is null or id <> $4)),
         u as (
             update affiliate_commissions c
                set status = case when h.run <= $3 then 'available' else 'withdrawn' end
               from h
              where c.tenant_id = $1 and c.id = h.id
          returning c.status, c.amount)
         select sum(amount) filter (where status = 'available') from u",
    )
    .bind(t)
    .bind(affiliate_id)
    .bind(balance)
    .bind(opening_balance_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok((balance - covered.unwrap_or(Decimal::ZERO)).max(Decimal::ZERO))
}

/// The affiliate account of a client, if any.
pub async fn affiliate_of_client(
    conn: &mut PgConnection,
    t: TenantId,
    client_id: Uuid,
) -> DbResult<Option<Uuid>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "select id from affiliates where tenant_id = $1 and client_id = $2",
    )
    .bind(t)
    .bind(client_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The referred client of a referral.
pub async fn referral_client(
    conn: &mut PgConnection,
    t: TenantId,
    referral_id: Uuid,
) -> DbResult<Option<Uuid>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "select client_id from affiliate_referrals where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(referral_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The currency of an affiliate account.
pub async fn affiliate_currency(
    conn: &mut PgConnection,
    t: TenantId,
    affiliate_id: Uuid,
) -> DbResult<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "select currency from affiliates where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(affiliate_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The first referral of an affiliate (to carry an opening balance).
pub async fn first_referral(
    conn: &mut PgConnection,
    t: TenantId,
    affiliate_id: Uuid,
) -> DbResult<Option<(Uuid, Uuid)>> {
    Ok(sqlx::query_as::<_, (Uuid, Uuid)>(
        "select id, client_id from affiliate_referrals
          where tenant_id = $1 and affiliate_id = $2 order by created_at, id limit 1",
    )
    .bind(t)
    .bind(affiliate_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Upserts the opening-balance commission (`available`) of an affiliate.
pub async fn upsert_opening_balance(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    c: &CommissionInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "affiliate_commissions",
        &[
            "affiliate_id",
            "referral_id",
            "client_id",
            "kind",
            "amount",
            "currency",
            "status",
            "clears_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(c.affiliate_id)
        .bind(c.referral_id)
        .bind(c.client_id)
        .bind("one_time")
        .bind(c.amount)
        .bind(&c.currency)
        .bind("available")
        .bind(c.created_at)
        .bind(c.created_at);
    run_upsert(q, conn, "affiliate_commissions").await
}

/// Upserts a paid manual payout (a WHMCS withdrawal).
pub async fn upsert_payout(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    affiliate_id: Uuid,
    amount: Decimal,
    currency: &str,
    paid_at: DateTime<Utc>,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "affiliate_payouts",
        &[
            "affiliate_id",
            "amount",
            "currency",
            "method",
            "status",
            "staff_note",
            "processed_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(affiliate_id)
        .bind(amount)
        .bind(currency)
        .bind("manual")
        .bind("paid")
        .bind("Imported from WHMCS")
        .bind(paid_at)
        .bind(paid_at);
    run_upsert(q, conn, "affiliate_payouts").await
}

/// `staff_tasks`.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskInput {
    pub title: String,
    pub body: Option<String>,
    pub status: String,
    pub due_date: Option<NaiveDate>,
    pub assigned_to: Option<Uuid>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a staff to-do.
pub async fn upsert_task(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    k: &TaskInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "staff_tasks",
        &[
            "title",
            "body",
            "status",
            "due_date",
            "assigned_to",
            "completed_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&k.title)
        .bind(&k.body)
        .bind(&k.status)
        .bind(k.due_date)
        .bind(k.assigned_to)
        .bind(k.completed_at)
        .bind(k.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "staff_tasks").await
}

/// Adds a ban unless the same `(kind, value)` exists; returns its id and
/// whether it was inserted (an existing ban is adopted, its reason kept).
pub async fn upsert_ban(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    kind: &str,
    value: &str,
    reason: Option<&str>,
) -> DbResult<(Uuid, bool)> {
    Ok(sqlx::query_as::<_, (Uuid, bool)>(
        "insert into fraud_bans (id, tenant_id, kind, value, reason) values ($1, $2, $3, $4, $5)
         on conflict (tenant_id, kind, value)
         do update set reason = coalesce(fraud_bans.reason, excluded.reason)
         returning id, (xmax = 0)",
    )
    .bind(id)
    .bind(t)
    .bind(kind)
    .bind(value)
    .bind(reason)
    .fetch_one(&mut *conn)
    .await?)
}

/// Upserts a download category.
pub async fn upsert_download_category(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    name: &str,
    description: Option<&str>,
    is_hidden: bool,
    sort_order: i32,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "download_categories",
        &["name", "description", "is_hidden", "sort_order"],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(name)
        .bind(description)
        .bind(is_hidden)
        .bind(sort_order);
    run_upsert(q, conn, "download_categories").await
}

/// `downloads` (metadata only: the file itself lives in WHMCS's downloads
/// directory and is uploaded again; without a `download_files` row the
/// download is listed but not served).
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadInput {
    pub category_id: Option<Uuid>,
    pub title: String,
    pub description: Option<String>,
    pub filename: String,
    pub content_type: String,
    pub access: String,
    pub is_hidden: bool,
    pub download_count: i64,
    pub created_at: Option<DateTime<Utc>>,
}

/// Upserts a download.
pub async fn upsert_download(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    d: &DownloadInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "downloads",
        &[
            "category_id",
            "title",
            "description",
            "filename",
            "content_type",
            "access",
            "is_hidden",
            "download_count",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(d.category_id)
        .bind(&d.title)
        .bind(&d.description)
        .bind(&d.filename)
        .bind(&d.content_type)
        .bind(&d.access)
        .bind(d.is_hidden)
        .bind(d.download_count)
        .bind(d.created_at.unwrap_or_else(Utc::now));
    run_upsert(q, conn, "downloads").await
}

/// `status_incidents` (a WHMCS network issue) with its description as the
/// one timeline update.
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentInput {
    pub kind: String,
    pub title: String,
    pub impact: String,
    pub status: String,
    pub scheduled_start: Option<DateTime<Utc>>,
    pub scheduled_end: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub body: String,
}

/// Upserts an incident and its update (`update_id`); returns whether the
/// incident was inserted.
pub async fn upsert_incident(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    update_id: Uuid,
    i: &IncidentInput,
) -> DbResult<bool> {
    let sql = upsert_sql(
        "status_incidents",
        &[
            "kind",
            "title",
            "impact",
            "status",
            "scheduled_start",
            "scheduled_end",
            "started_at",
            "resolved_at",
            "notified_at",
            "created_at",
        ],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(id)
        .bind(t)
        .bind(&i.kind)
        .bind(&i.title)
        .bind(&i.impact)
        .bind(&i.status)
        .bind(i.scheduled_start)
        .bind(i.scheduled_end)
        .bind(i.started_at)
        .bind(i.resolved_at)
        // Never announce an imported incident again.
        .bind(i.started_at)
        .bind(i.started_at);
    let inserted = run_upsert(q, conn, "status_incidents").await?;
    let sql = upsert_sql(
        "status_incident_updates",
        &["incident_id", "status", "body", "created_at"],
        &[],
    );
    let q = sqlx::query_scalar::<_, bool>(&sql)
        .bind(update_id)
        .bind(t)
        .bind(id)
        .bind(&i.status)
        .bind(&i.body)
        .bind(i.resolved_at.unwrap_or(i.started_at));
    run_upsert(q, conn, "status_incident_updates").await?;
    Ok(inserted)
}

/// An imported activity-log line (`audit_log`, actor `system`).
pub async fn upsert_activity(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
    metadata: &Json,
    created_at: DateTime<Utc>,
) -> DbResult<bool> {
    let r = sqlx::query(
        "insert into audit_log (id, tenant_id, actor_kind, action, metadata, created_at)
         values ($1, $2, 'system', 'import.whmcs.activity', $3, $4)
         on conflict (id) do nothing",
    )
    .bind(id)
    .bind(t)
    .bind(metadata)
    .bind(created_at)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// small reads used while importing
// ---------------------------------------------------------------------------

/// `(id, kind)` of the user with `email`.
pub async fn find_user(
    conn: &mut PgConnection,
    t: TenantId,
    email: &str,
) -> DbResult<Option<(Uuid, String)>> {
    Ok(sqlx::query_as::<_, (Uuid, String)>(
        "select id, kind from users where tenant_id = $1 and email = $2::citext",
    )
    .bind(t)
    .bind(email)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Of an existing login: whether its e-mail address is verified and whether
/// it is already a member of `client` (`None`: not checked).
pub async fn login_standing(
    conn: &mut PgConnection,
    t: TenantId,
    user: Uuid,
    client: Option<Uuid>,
) -> DbResult<(bool, bool)> {
    Ok(sqlx::query_as::<_, (bool, bool)>(
        "select u.email_verified_at is not null,
                $3::uuid is not null and exists(select 1 from client_users cu
                     where cu.tenant_id = u.tenant_id and cu.user_id = u.id and cu.client_id = $3)
           from users u where u.tenant_id = $1 and u.id = $2",
    )
    .bind(t)
    .bind(user)
    .bind(client)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or((false, false)))
}

/// An existing login adopted by the import: its password is cleared (it
/// signs in again through "forgot password", proving the mailbox once more)
/// and its sessions end, so the imported account is never reachable
/// through access granted before the import.
pub async fn reset_adopted_login(conn: &mut PgConnection, t: TenantId, user: Uuid) -> DbResult<()> {
    sqlx::query(
        "update users set password_hash = null where tenant_id = $1 and id = $2 and kind = 'client'",
    )
    .bind(t)
    .bind(user)
    .execute(&mut *conn)
    .await?;
    sqlx::query("delete from sessions where tenant_id = $1 and user_id = $2")
        .bind(t)
        .bind(user)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Currency of a client.
pub async fn client_currency(
    conn: &mut PgConnection,
    t: TenantId,
    client_id: Uuid,
) -> DbResult<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "select currency from clients where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(client_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Billing cycle of a service.
pub async fn service_cycle(
    conn: &mut PgConnection,
    t: TenantId,
    service_id: Uuid,
) -> DbResult<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "select billing_cycle from services where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(service_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Amount of one credit row.
pub async fn credit_amount(
    conn: &mut PgConnection,
    t: TenantId,
    id: Uuid,
) -> DbResult<Option<Decimal>> {
    Ok(sqlx::query_scalar::<_, Decimal>(
        "select amount from credits where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Deletes one credit row (an opening-balance adjustment that is no longer needed).
pub async fn delete_credit(conn: &mut PgConnection, t: TenantId, id: Uuid) -> DbResult<()> {
    sqlx::query("delete from credits where tenant_id = $1 and id = $2")
        .bind(t)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// An imported invoice whose recorded payments do not match its amount paid.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct PaymentMismatch {
    pub source_id: String,
    pub status: String,
    /// `total − credit_applied`.
    pub due: Decimal,
    /// Payments − refunds / chargebacks in the invoice currency.
    pub net: Decimal,
    /// Succeeded transactions in another currency (not counted).
    pub foreign_txns: i64,
}

/// Sets `amount_paid` of the open invoices (`draft`, `unpaid`,
/// `payment_pending`, `collections`) imported from `source_key` to their
/// succeeded payments minus refunds and chargebacks in the invoice currency,
/// clamped to `[0, total − credit_applied]`. Paid / refunded / cancelled
/// invoices keep the amount their status gave them. Idempotent: the value is
/// recomputed from `transactions`, never added to. Returns the number of
/// invoices changed and the invoices whose transactions do not add up
/// (overpaid open invoices, paid ones with a different sum, payments in
/// another currency), at most `limit`.
pub async fn reconcile_invoice_payments(
    conn: &mut PgConnection,
    t: TenantId,
    source_key: &str,
    limit: i64,
) -> DbResult<(u64, Vec<PaymentMismatch>)> {
    const NET: &str = "
        select i.id, m.source_id, i.status, greatest(i.total - i.credit_applied, 0) as due,
               coalesce(sum(x.amount) filter (where x.kind = 'payment' and x.currency = i.currency), 0)
             - coalesce(sum(x.amount) filter (where x.kind in ('refund', 'chargeback')
                                               and x.currency = i.currency), 0) as net,
               count(x.id) filter (where x.currency <> i.currency) as foreign_txns,
               i.amount_paid
          from import_id_map m
          join invoices i on i.tenant_id = m.tenant_id and i.id = m.target_id
          left join transactions x on x.tenant_id = i.tenant_id and x.invoice_id = i.id
                                  and x.status = 'succeeded'
         where m.tenant_id = $1 and m.source_key = $2 and m.entity = 'invoices'
         group by i.id, m.source_id, i.status, i.total, i.credit_applied, i.amount_paid";
    let changed = sqlx::query(&format!(
        "with n as ({NET})
         update invoices i set amount_paid = least(greatest(n.net, 0), n.due), updated_at = now()
           from n
          where i.tenant_id = $1 and i.id = n.id
            and i.status in ('draft', 'unpaid', 'payment_pending', 'collections')
            and i.amount_paid <> least(greatest(n.net, 0), n.due)"
    ))
    .bind(t)
    .bind(source_key)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let mismatches = sqlx::query_as::<_, PaymentMismatch>(&format!(
        "with n as ({NET})
         select source_id, status, due, net, foreign_txns
           from n
          where n.foreign_txns > 0
             or (n.status in ('draft', 'unpaid', 'payment_pending', 'collections') and n.net > n.due)
             or (n.status = 'paid' and n.net > 0 and n.net <> n.due)
          order by source_id
          limit $3"
    ))
    .bind(t)
    .bind(source_key)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok((changed, mismatches))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_sql_shape() {
        let sql = upsert_sql("things", &["a", "b"], &["b"]);
        assert!(
            sql.starts_with("insert into things (id, tenant_id, a, b) values ($1, $2, $3, $4)")
        );
        assert!(sql.contains("do update set a = excluded.a"));
        assert!(!sql.contains("b = excluded.b"));
        assert!(sql.contains("where things.tenant_id = excluded.tenant_id"));
        assert!(sql.ends_with("returning (xmax = 0)"));
        assert!(upsert_sql("x", &["a"], &["a"]).contains("set tenant_id = excluded.tenant_id"));
    }
}
