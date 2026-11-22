//! Downloads area (`download_categories`, `downloads`, `download_files`,
//! `download_products`; migration 0043). File bytes live in
//! `download_files` and are only read by [`file`].

use chrono::{DateTime, Utc};
use hos_core::ids::{ClientId, ProductId, TenantId, UserId};
use hos_core::string_enum;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use crate::error::{DbResult, not_found};

hos_core::define_id! {
    /// `download_categories`.
    pub struct DownloadCategoryId;
}

hos_core::define_id! {
    /// `downloads`.
    pub struct DownloadId;
}

string_enum! {
    /// Who may fetch a download.
    pub enum DownloadAccess {
        /// Anyone (storefront `/downloads`).
        Public => "public",
        /// Signed-in clients.
        Clients => "clients",
        /// Clients with an active service of a linked product.
        Product => "product",
    }
}

/// `download_categories` row.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct DownloadCategoryRow {
    pub id: DownloadCategoryId,
    pub tenant_id: TenantId,
    pub name: String,
    pub description: Option<String>,
    pub is_hidden: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `downloads` row (+ linked product ids, + whether bytes are stored).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct DownloadRow {
    pub id: DownloadId,
    pub tenant_id: TenantId,
    pub category_id: Option<DownloadCategoryId>,
    pub title: String,
    pub description: Option<String>,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: Option<String>,
    pub access: DownloadAccess,
    pub is_hidden: bool,
    pub download_count: i64,
    pub sort_order: i32,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub product_ids: Vec<Uuid>,
    pub has_file: bool,
}

const ROW: &str = "select d.*,
        coalesce((select array_agg(p.product_id order by p.product_id) from download_products p
                   where p.tenant_id = d.tenant_id and p.download_id = d.id), '{}') as product_ids,
        exists(select 1 from download_files f where f.tenant_id = d.tenant_id and f.download_id = d.id) as has_file
   from downloads d";

/// Input of [`create_category`] / [`update_category`].
#[derive(Debug, Clone)]
pub struct CategoryInput {
    pub name: String,
    pub description: Option<String>,
    pub is_hidden: bool,
    pub sort_order: i32,
}

/// Creates a category.
pub async fn create_category(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    c: &CategoryInput,
) -> DbResult<DownloadCategoryRow> {
    Ok(sqlx::query_as::<_, DownloadCategoryRow>(
        "insert into download_categories (id, tenant_id, name, description, is_hidden, sort_order)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(DownloadCategoryId::new())
    .bind(tenant_id)
    .bind(&c.name)
    .bind(&c.description)
    .bind(c.is_hidden)
    .bind(c.sort_order)
    .fetch_one(db)
    .await?)
}

/// Updates a category.
pub async fn update_category(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DownloadCategoryId,
    c: &CategoryInput,
) -> DbResult<DownloadCategoryRow> {
    sqlx::query_as::<_, DownloadCategoryRow>(
        "update download_categories set name = $3, description = $4, is_hidden = $5, sort_order = $6
          where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&c.name)
    .bind(&c.description)
    .bind(c.is_hidden)
    .bind(c.sort_order)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| not_found("download category", id))
}

/// Deletes a category (its downloads stay, uncategorised).
pub async fn delete_category(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DownloadCategoryId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from download_categories where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Categories, in order.
pub async fn categories(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
) -> DbResult<Vec<DownloadCategoryRow>> {
    Ok(sqlx::query_as::<_, DownloadCategoryRow>(
        "select * from download_categories where tenant_id = $1 order by sort_order, name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Metadata of a download (input of [`create`] / [`update`]).
#[derive(Debug, Clone)]
pub struct DownloadInput {
    pub category_id: Option<DownloadCategoryId>,
    pub title: String,
    pub description: Option<String>,
    pub access: DownloadAccess,
    pub is_hidden: bool,
    pub sort_order: i32,
    pub product_ids: Vec<ProductId>,
}

/// An uploaded file.
#[derive(Debug, Clone)]
pub struct DownloadFile {
    pub filename: String,
    pub content_type: String,
    pub sha256: String,
    pub content: Vec<u8>,
}

async fn set_products(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: DownloadId,
    products: &[ProductId],
) -> DbResult<()> {
    sqlx::query("delete from download_products where tenant_id = $1 and download_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for p in products {
        sqlx::query(
            "insert into download_products (tenant_id, download_id, product_id) values ($1, $2, $3)
             on conflict do nothing",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(p)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn set_file(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: DownloadId,
    f: &DownloadFile,
) -> DbResult<()> {
    sqlx::query(
        "update downloads set filename = $3, content_type = $4, size_bytes = $5, sha256 = $6
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&f.filename)
    .bind(&f.content_type)
    .bind(i64::try_from(f.content.len()).unwrap_or(i64::MAX))
    .bind(&f.sha256)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "insert into download_files (download_id, tenant_id, content) values ($1, $2, $3)
         on conflict (download_id) do update set content = excluded.content",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&f.content)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Creates a download (with its file when given; imported metadata may
/// come without one and is then not served).
pub async fn create(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    d: &DownloadInput,
    filename: &str,
    file: Option<&DownloadFile>,
    created_by: Option<UserId>,
) -> DbResult<DownloadId> {
    let id = DownloadId::new();
    sqlx::query(
        "insert into downloads (id, tenant_id, category_id, title, description, filename, access,
                                is_hidden, sort_order, created_by)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(d.category_id)
    .bind(&d.title)
    .bind(&d.description)
    .bind(filename)
    .bind(d.access)
    .bind(d.is_hidden)
    .bind(d.sort_order)
    .bind(created_by)
    .execute(&mut *conn)
    .await?;
    set_products(conn, tenant_id, id, &d.product_ids).await?;
    if let Some(f) = file {
        set_file(conn, tenant_id, id, f).await?;
    }
    Ok(id)
}

/// Updates a download (and replaces its file when given).
pub async fn update(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: DownloadId,
    d: &DownloadInput,
    file: Option<&DownloadFile>,
) -> DbResult<()> {
    let res = sqlx::query(
        "update downloads set category_id = $3, title = $4, description = $5, access = $6,
                is_hidden = $7, sort_order = $8
          where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(d.category_id)
    .bind(&d.title)
    .bind(&d.description)
    .bind(d.access)
    .bind(d.is_hidden)
    .bind(d.sort_order)
    .execute(&mut *conn)
    .await?;
    if res.rows_affected() == 0 {
        return Err(not_found("download", id));
    }
    set_products(conn, tenant_id, id, &d.product_ids).await?;
    if let Some(f) = file {
        set_file(conn, tenant_id, id, f).await?;
    }
    Ok(())
}

/// Deletes a download (and its bytes).
pub async fn delete(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DownloadId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from downloads where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// One download (metadata).
pub async fn get(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DownloadId,
) -> DbResult<Option<DownloadRow>> {
    Ok(
        sqlx::query_as::<_, DownloadRow>(&format!("{ROW} where d.tenant_id = $1 and d.id = $2"))
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Every download (staff list).
pub async fn list_all(db: impl PgExecutor<'_>, tenant_id: TenantId) -> DbResult<Vec<DownloadRow>> {
    Ok(sqlx::query_as::<_, DownloadRow>(&format!(
        "{ROW} where d.tenant_id = $1 order by d.sort_order, d.title"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Downloads `client` may fetch (`None`: a guest → public ones only):
/// visible, with a file, in a visible (or no) category.
pub async fn visible(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client: Option<ClientId>,
) -> DbResult<Vec<DownloadRow>> {
    Ok(sqlx::query_as::<_, DownloadRow>(&format!(
        "{ROW}
          where d.tenant_id = $1 and not d.is_hidden
            and exists(select 1 from download_files f where f.tenant_id = d.tenant_id and f.download_id = d.id)
            and (d.category_id is null or exists(select 1 from download_categories c
                  where c.tenant_id = d.tenant_id and c.id = d.category_id and not c.is_hidden))
            and (d.access = 'public'
                 or ($2::uuid is not null and d.access = 'clients')
                 or ($2::uuid is not null and d.access = 'product' and exists(
                       select 1 from download_products p
                         join services s on s.tenant_id = p.tenant_id and s.product_id = p.product_id
                        where p.tenant_id = d.tenant_id and p.download_id = d.id
                          and s.client_id = $2 and s.status = 'active')))
          order by d.sort_order, d.title"
    ))
    .bind(tenant_id)
    .bind(client)
    .fetch_all(db)
    .await?)
}

/// The bytes of a download `client` may fetch (same rules as [`visible`]),
/// counting the download.
pub async fn fetch_for(
    db: &sqlx::PgPool,
    tenant_id: TenantId,
    id: DownloadId,
    client: Option<ClientId>,
) -> DbResult<Option<(DownloadRow, Vec<u8>)>> {
    let Some(row) = visible(db, tenant_id, client)
        .await?
        .into_iter()
        .find(|d| d.id == id)
    else {
        return Ok(None);
    };
    let Some(bytes) = file(db, tenant_id, id).await? else {
        return Ok(None);
    };
    sqlx::query(
        "update downloads set download_count = download_count + 1 where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(Some((row, bytes)))
}

/// Stored bytes of a download.
pub async fn file(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: DownloadId,
) -> DbResult<Option<Vec<u8>>> {
    Ok(sqlx::query_scalar::<_, Vec<u8>>(
        "select content from download_files where tenant_id = $1 and download_id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}
