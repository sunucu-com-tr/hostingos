//! Listing queries for the public REST API (`/api/v1`).
//!
//! The panel listings in the other repo modules have a fixed order; the API
//! lets callers pick a sort key from a per-resource whitelist. Every function
//! here takes an [`ApiSort`] whose column is a `&'static str` chosen by the
//! caller from such a whitelist — it is pasted into SQL, so it must never be
//! built from user input.

use hos_core::ids::{ClientId, TenantId, WebhookEndpointId};
use sqlx::{Acquire, PgExecutor, Postgres, QueryBuilder};

use super::catalog::ProductRow;
use super::clients::ClientRow;
use super::domains::DomainRow;
use super::invoices::InvoiceRow;
use super::orders::OrderRow;
use super::payments::TransactionRow;
use super::services::ServiceListRow;
use super::support::TicketRow;
use crate::error::{DbResult, like_contains};
use crate::page::{Page, Paged, fetch_paged};

/// Sort order: a whitelisted SQL expression plus direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiSort {
    /// SQL expression (static, from a whitelist).
    pub column: &'static str,
    /// Descending?
    pub desc: bool,
}

impl ApiSort {
    /// Newest first.
    pub const NEWEST: Self = Self {
        column: "created_at",
        desc: true,
    };

    fn order_by(self, alias: &str) -> String {
        let dir = if self.desc { "desc" } else { "asc" };
        format!("{alias}.{} {dir} nulls last, {alias}.id {dir}", self.column)
    }
}

/// Common API list filters (all optional).
#[derive(Debug, Clone, Default)]
pub struct ApiFilter {
    /// Only rows of this client (client keys always set this).
    pub client_id: Option<ClientId>,
    /// `status::text` equals.
    pub status: Option<String>,
    /// Case-insensitive substring search on the resource's text columns.
    pub search: Option<String>,
    /// `created_at >= from`.
    pub created_from: Option<chrono::DateTime<chrono::Utc>>,
    /// `created_at < to`.
    pub created_to: Option<chrono::DateTime<chrono::Utc>>,
}

fn push_common(
    qb: &mut QueryBuilder<'static, Postgres>,
    alias: &str,
    f: &ApiFilter,
    client_col: Option<&str>,
    search_cols: &[&str],
) {
    if let (Some(c), Some(col)) = (f.client_id, client_col) {
        qb.push(format!(" and {alias}.{col} = ")).push_bind(c);
    }
    if let Some(s) = &f.status {
        qb.push(format!(" and {alias}.status::text = "))
            .push_bind(s.clone());
    }
    if let Some(d) = f.created_from {
        qb.push(format!(" and {alias}.created_at >= ")).push_bind(d);
    }
    if let Some(d) = f.created_to {
        qb.push(format!(" and {alias}.created_at < ")).push_bind(d);
    }
    if let Some(term) = f.search.as_deref().filter(|s| !s.trim().is_empty())
        && !search_cols.is_empty()
    {
        let like = like_contains(term);
        qb.push(" and (");
        for (i, col) in search_cols.iter().enumerate() {
            if i > 0 {
                qb.push(" or ");
            }
            qb.push(format!("{col} ilike ")).push_bind(like.clone());
        }
        qb.push(")");
    }
}

/// Clients.
pub async fn list_clients<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<ClientRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select c.*",
        |qb| {
            qb.push("from clients c where c.tenant_id = ")
                .push_bind(tenant_id);
            push_common(
                qb,
                "c",
                &f,
                Some("id"),
                &[
                    "c.email::text",
                    "(c.first_name || ' ' || c.last_name)",
                    "c.company_name",
                ],
            );
        },
        &sort.order_by("c"),
        page,
    )
    .await
}

/// Products (staff view: hidden products included, retired only on request).
pub async fn list_products<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    include_retired: bool,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<ProductRow>> {
    let f = ApiFilter {
        status: None,
        client_id: None,
        ..filter.clone()
    };
    fetch_paged(
        db,
        "select p.*",
        |qb| {
            qb.push("from products p where p.tenant_id = ")
                .push_bind(tenant_id);
            if !include_retired {
                qb.push(" and not p.is_retired");
            }
            push_common(qb, "p", &f, None, &["p.name", "p.slug"]);
        },
        &sort.order_by("p"),
        page,
    )
    .await
}

/// Orders.
pub async fn list_orders<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<OrderRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select o.*",
        |qb| {
            qb.push("from orders o where o.tenant_id = ")
                .push_bind(tenant_id);
            push_common(qb, "o", &f, Some("client_id"), &["o.order_number"]);
        },
        &sort.order_by("o"),
        page,
    )
    .await
}

/// Invoices.
pub async fn list_invoices<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<InvoiceRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select i.*",
        |qb| {
            qb.push("from invoices i where i.tenant_id = ")
                .push_bind(tenant_id);
            push_common(qb, "i", &f, Some("client_id"), &["i.number"]);
        },
        &sort.order_by("i"),
        page,
    )
    .await
}

/// Transactions.
pub async fn list_transactions<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<TransactionRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select x.*",
        |qb| {
            qb.push("from transactions x where x.tenant_id = ")
                .push_bind(tenant_id);
            push_common(
                qb,
                "x",
                &f,
                Some("client_id"),
                &["x.gateway_txn_id", "x.gateway"],
            );
        },
        &sort.order_by("x"),
        page,
    )
    .await
}

/// Services (with product and client names).
pub async fn list_services<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<ServiceListRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select s.*, p.name as product_name, p.product_type,
                coalesce(nullif(c.company_name, ''), trim(c.first_name || ' ' || c.last_name)) as client_name",
        |qb| {
            qb.push(
                "from services s join products p on p.id = s.product_id
                 join clients c on c.id = s.client_id where s.tenant_id = ",
            )
            .push_bind(tenant_id);
            push_common(
                qb,
                "s",
                &f,
                Some("client_id"),
                &["s.domain", "s.username", "p.name"],
            );
        },
        &sort.order_by("s"),
        page,
    )
    .await
}

/// Domains.
pub async fn list_domains<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<DomainRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select d.*",
        |qb| {
            qb.push("from domains d where d.tenant_id = ")
                .push_bind(tenant_id);
            push_common(qb, "d", &f, Some("client_id"), &["d.name::text"]);
        },
        &sort.order_by("d"),
        page,
    )
    .await
}

/// Tickets.
pub async fn list_tickets<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &ApiFilter,
    sort: ApiSort,
    page: Page,
) -> DbResult<Paged<TicketRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select t.*",
        |qb| {
            qb.push("from tickets t where t.tenant_id = ")
                .push_bind(tenant_id);
            push_common(qb, "t", &f, Some("client_id"), &["t.number", "t.subject"]);
        },
        &sort.order_by("t"),
        page,
    )
    .await
}

/// Deletes a webhook endpoint; `false` when it did not exist.
pub async fn delete_webhook_endpoint(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: WebhookEndpointId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from webhook_endpoints where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_by_is_stable() {
        assert_eq!(
            ApiSort::NEWEST.order_by("c"),
            "c.created_at desc nulls last, c.id desc"
        );
        let s = ApiSort {
            column: "total",
            desc: false,
        };
        assert_eq!(s.order_by("i"), "i.total asc nulls last, i.id asc");
    }
}
