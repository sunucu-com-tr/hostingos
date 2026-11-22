//! # hos-core
//!
//! Foundation crate of HostingOS. Every other crate depends on it; it depends on
//! no other workspace crate and never touches the database (the optional `sqlx`
//! feature only adds Postgres type mappings).
//!
//! | Module | Contents |
//! |---|---|
//! | [`ids`] | UUID v7 newtype ids (`TenantId`, `ServiceId`, ...) |
//! | [`money`] | [`Money`](money::Money), [`Currency`](money::Currency), [`BillingCycle`](money::BillingCycle) |
//! | [`error`] | [`CoreError`], [`IntegrationError`] |
//! | [`config`] | [`AppConfig`](config::AppConfig) loaded from the environment |
//! | [`crypto`] | AES-256-GCM [`SecretBox`](crypto::SecretBox), tokens, hashes, HMAC |
//! | [`secret`] | [`Secret<T>`](secret::Secret): Debug-redacting wrapper for credentials |
//! | [`tenant`] | [`TenantContext`](tenant::TenantContext) and branding |
//! | [`domain`] | Business enums stored as `text` (statuses, product types, ...) |
//! | [`integrations`] | Every integration trait (virt, panel, registrar, gateway, mail, ai) |
//! | [`net`] | Outbound policy for tenant-configured endpoints (`HOS_OUTBOUND_POLICY`, SSRF guard) |
//! | [`prelude`] | Convenient glob import |
//!
//! ## Cargo features
//!
//! * `sqlx` (default): Postgres `Type`/`Encode`/`Decode` impls for ids, string
//!   enums and [`Currency`](money::Currency). The workspace dependency entry turns
//!   default features off; DB-facing crates enable it with `features = ["sqlx"]`.
//! * `net` (default): the `reqwest`/`tokio` half of [`net`] (policy-checking
//!   DNS resolver, async host validation, bounded body reads). Integration
//!   crates enable it with `features = ["net"]`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod macros;

pub mod config;
pub mod crypto;
pub mod domain;
pub mod error;
pub mod ids;
pub mod image;
pub mod integrations;
pub mod money;
pub mod net;
pub mod prelude;
pub mod secret;
pub mod tenant;

pub use async_trait::async_trait;
pub use error::{CoreError, CoreResult, IntegrationError, IntegrationResult};

/// Re-exports used by the exported macros. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use chrono;
    pub use serde;
    #[cfg(feature = "sqlx")]
    pub use sqlx;
    pub use uuid;
}

/// Compile-time checks that ids, string enums and `Currency` bind/decode with sqlx.
#[cfg(all(test, feature = "sqlx"))]
mod sqlx_compat {
    use crate::domain::ServiceStatus;
    use crate::ids::TenantId;
    use crate::money::Currency;
    use sqlx::Row as _;

    #[allow(dead_code)]
    fn binds() {
        let _q = sqlx::query("select $1, $2, $3, $4, $5")
            .bind(TenantId::new())
            .bind(ServiceStatus::Active)
            .bind(Currency::TRY)
            .bind(vec![TenantId::new()])
            .bind(vec![ServiceStatus::Pending]);
    }

    type Decoded = (
        TenantId,
        ServiceStatus,
        Currency,
        Vec<TenantId>,
        Option<TenantId>,
    );

    #[allow(dead_code)]
    fn decodes(row: &sqlx::postgres::PgRow) -> Result<Decoded, sqlx::Error> {
        Ok((
            row.try_get(0)?,
            row.try_get(1)?,
            row.try_get(2)?,
            row.try_get(3)?,
            row.try_get(4)?,
        ))
    }

    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct ServiceRow {
        tenant_id: TenantId,
        status: ServiceStatus,
        currency: Currency,
    }

    #[test]
    fn type_info_matches_postgres() {
        use sqlx::{Type, TypeInfo as _, postgres::Postgres};
        assert_eq!(<TenantId as Type<Postgres>>::type_info().name(), "UUID");
        assert_eq!(
            <ServiceStatus as Type<Postgres>>::type_info().name(),
            "TEXT"
        );
        assert_eq!(<Currency as Type<Postgres>>::type_info().name(), "TEXT");
    }
}
