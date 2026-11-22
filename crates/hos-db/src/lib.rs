//! # hos-db
//!
//! PostgreSQL schema (`migrations/`) and the tenant-scoped repository layer of
//! HostingOS (docs/ARCHITECTURE.md §3, §5).
//!
//! * [`connect`] / [`migrate`] — pool construction (Neon pooler / PgBouncer
//!   friendly) and the embedded [`MIGRATOR`].
//! * [`repo`] — one module per table group. Every function of a tenant-owned
//!   table takes the [`TenantId`](hos_core::ids::TenantId) right after the
//!   executor and filters on `tenant_id`; only [`repo::platform`] has
//!   tenant-less functions.
//! * [`seed`] — idempotent demo / default data (`hos-migrate --seed`).
//!
//! ## Conventions
//!
//! * Runtime queries only (`sqlx::query`, `query_as::<_, Row>`); the `query!`
//!   macros are banned so compiling never needs a database.
//! * Row structs derive [`sqlx::FromRow`] and use hos-core id newtypes and
//!   `string_enum!` types **directly** as field types: those types implement
//!   sqlx `Type/Encode/Decode` as `text` (hos-core `sqlx` feature), so a status
//!   column decodes straight into e.g. [`ServiceStatus`](hos_core::domain::ServiceStatus)
//!   and binds with `.bind(status)`. Enums that only exist in the schema
//!   (job status, client kind, ...) are defined the same way in [`enums`].
//!   The `CHECK` constraints in the migrations list exactly the `as_str()`
//!   values, so an unknown string can only come from a manual edit and surfaces
//!   as a decode error, never as a silently wrong variant.
//! * Executors: single-statement functions take `impl PgExecutor<'_>` (pass
//!   `&PgPool`, `&mut *tx` or `&mut *conn`); functions that need several
//!   statements atomically take `&mut PgConnection` and open their own
//!   transaction (a savepoint when the caller is already inside one); paged
//!   lists take `impl Acquire` (pool, connection or transaction).
//! * Errors are [`CoreError`](hos_core::CoreError) (`sqlx::Error` converts via
//!   `?`; `RowNotFound` → `NotFound`). Expected unique violations are mapped to
//!   `CoreError::Conflict`; see [`error`].
//! * Money columns are `numeric(20,6)` ↔ [`rust_decimal::Decimal`]; currency
//!   codes ↔ [`Currency`](hos_core::money::Currency).
//! * Case-insensitive columns (e-mails, coupon codes, domain names) are
//!   `citext`. A bound Rust string is sent as `text`, and `citext = text`
//!   resolves to the case-sensitive text operator — so lookups cast the
//!   parameter: `where email = $2::citext`.
//! * Tenant integrity is enforced by the schema too: every reference between
//!   tenant-owned tables is a composite FK `(tenant_id, x_id) → x (tenant_id, id)`
//!   (migration 0012), so a row can never point into another tenant even if a
//!   caller passes a foreign id. `delete from tenants` cascades everything.

#![forbid(unsafe_code)]

pub mod enums;
pub mod error;
pub mod ids;
pub mod page;
pub mod pool;
pub mod repo;
pub mod seed;

pub use error::DbResult;
pub use page::{Page, Paged};
pub use pool::{
    DbConfig, MIGRATION_LOCK_KEY, MIGRATOR, MigrationOutcome, SchemaStatus, applied_versions,
    connect, embedded_versions, migrate, migrate_if_behind, schema_status,
};
pub use sqlx::{PgConnection, PgPool, Postgres};
