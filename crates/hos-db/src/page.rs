//! Pagination for list queries.

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{Acquire, FromRow, Postgres, QueryBuilder};

use crate::error::DbResult;

/// `LIMIT`/`OFFSET` window. Construct with [`Page::new`] or [`Page::number`]
/// (both clamp to `1..=MAX_LIMIT` and `offset >= 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    /// Rows per page.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

impl Page {
    /// Default page size.
    pub const DEFAULT_LIMIT: i64 = 25;
    /// Largest accepted page size.
    pub const MAX_LIMIT: i64 = 200;

    /// Clamped window.
    pub fn new(limit: i64, offset: i64) -> Self {
        Self {
            limit: limit.clamp(1, Self::MAX_LIMIT),
            offset: offset.max(0),
        }
    }

    /// 1-based page number × page size (`page = 0` is treated as 1).
    pub fn number(page: u32, per_page: u32) -> Self {
        let limit = i64::from(per_page).clamp(1, Self::MAX_LIMIT);
        let page = i64::from(page.max(1));
        Self::new(limit, (page - 1).saturating_mul(limit))
    }

    /// First page with `limit` rows.
    pub fn first(limit: i64) -> Self {
        Self::new(limit, 0)
    }

    /// 1-based number of this page.
    pub fn page_number(&self) -> i64 {
        self.offset / self.limit.max(1) + 1
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new(Self::DEFAULT_LIMIT, 0)
    }
}

/// One page of rows plus the total number of matching rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Paged<T> {
    /// Rows of this page.
    pub items: Vec<T>,
    /// Total matching rows (all pages).
    pub total: i64,
    /// Page size used.
    pub limit: i64,
    /// Offset used.
    pub offset: i64,
}

impl<T> Paged<T> {
    /// Number of pages (at least 1).
    pub fn total_pages(&self) -> i64 {
        ((self.total + self.limit - 1) / self.limit.max(1)).max(1)
    }

    /// 1-based current page.
    pub fn page_number(&self) -> i64 {
        self.offset / self.limit.max(1) + 1
    }

    /// Whether another page follows.
    pub fn has_next(&self) -> bool {
        self.offset + (self.items.len() as i64) < self.total
    }

    /// Maps the rows, keeping the counts.
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Paged<U> {
        Paged {
            items: self.items.into_iter().map(f).collect(),
            total: self.total,
            limit: self.limit,
            offset: self.offset,
        }
    }
}

/// Runs `select count(*) <from_where>` and `<select> <from_where> order by
/// <order_by> limit/offset` on one connection. `push_from_where` must push the
/// same `from ... where ...` clause (and binds) into both builders.
pub(crate) async fn fetch_paged<'a, T, A, F>(
    db: A,
    select: &str,
    push_from_where: F,
    order_by: &str,
    page: Page,
) -> DbResult<Paged<T>>
where
    A: Acquire<'a, Database = Postgres>,
    F: Fn(&mut QueryBuilder<'static, Postgres>),
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    let mut conn = db.acquire().await?;

    let mut count_qb: QueryBuilder<'static, Postgres> = QueryBuilder::new("select count(*) ");
    push_from_where(&mut count_qb);
    let total: i64 = count_qb
        .build_query_scalar::<i64>()
        .fetch_one(&mut *conn)
        .await?;

    let mut qb: QueryBuilder<'static, Postgres> = QueryBuilder::new(select);
    qb.push(' ');
    push_from_where(&mut qb);
    qb.push(" order by ");
    qb.push(order_by);
    qb.push(" limit ");
    qb.push_bind(page.limit);
    qb.push(" offset ");
    qb.push_bind(page.offset);
    let items = qb.build_query_as::<T>().fetch_all(&mut *conn).await?;

    Ok(Paged {
        items,
        total,
        limit: page.limit,
        offset: page.offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_clamping() {
        assert_eq!(
            Page::new(0, -5),
            Page {
                limit: 1,
                offset: 0
            }
        );
        assert_eq!(Page::new(1000, 10).limit, Page::MAX_LIMIT);
        assert_eq!(
            Page::number(3, 20),
            Page {
                limit: 20,
                offset: 40
            }
        );
        assert_eq!(Page::number(0, 20).offset, 0);
        assert_eq!(Page::number(3, 20).page_number(), 3);
    }

    #[test]
    fn paged_math() {
        let p = Paged {
            items: vec![1, 2],
            total: 5,
            limit: 2,
            offset: 2,
        };
        assert_eq!(p.total_pages(), 3);
        assert_eq!(p.page_number(), 2);
        assert!(p.has_next());
        let empty: Paged<i32> = Paged {
            items: vec![],
            total: 0,
            limit: 10,
            offset: 0,
        };
        assert_eq!(empty.total_pages(), 1);
        assert!(!empty.has_next());
        assert_eq!(p.map(|x| x * 10).items, vec![10, 20]);
    }
}
