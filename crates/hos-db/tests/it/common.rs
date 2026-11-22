//! Shared helpers for the database integration tests.
//!
//! Tests run against the real database configured in the workspace `.env`
//! (`DATABASE_URL` — the pooled URL the application uses — or
//! `DATABASE_URL_DIRECT`). When neither is set the tests print a notice and
//! pass without doing anything.
//!
//! Every test creates its own tenant with a random slug and deletes it at the
//! end; `delete from tenants` cascades to every tenant-scoped row. If a test
//! panics, [`TestTenant`]'s `Drop` still removes the tenant.

#![allow(dead_code)]

use std::path::Path;

use chrono::Utc;
use hos_core::domain::IntegrationCategory;
use hos_core::ids::{ClientId, IntegrationId, TenantId};
use hos_core::money::Currency;
use hos_db::repo::{clients, integrations, platform};
use hos_db::{DbConfig, PgPool};
use rust_decimal::Decimal;
use tokio::sync::OnceCell;

static MIGRATED: OnceCell<()> = OnceCell::const_new();

fn load_env() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let _ = dotenvy::from_path(root);
}

fn env_url(vars: &[&str]) -> Option<String> {
    vars.iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|u| !u.trim().is_empty())
}

/// URL used by the tests (pooled first, like the application).
pub fn test_url() -> Option<String> {
    load_env();
    env_url(&["DATABASE_URL", "DATABASE_URL_DIRECT"])
}

/// Connects (and applies pending migrations once per test binary). `None`
/// when no database is configured — the caller returns early.
pub async fn pool() -> Option<PgPool> {
    let Some(url) = test_url() else {
        eprintln!("DATABASE_URL / DATABASE_URL_DIRECT not set — skipping database test");
        return None;
    };
    MIGRATED
        .get_or_init(|| async {
            let direct = env_url(&["DATABASE_URL_DIRECT", "DATABASE_URL"]).expect("url");
            let pool = hos_db::connect(&DbConfig::new(&direct).with_max_connections(1))
                .await
                .expect("connect for migrations");
            hos_db::migrate(&pool).await.expect("migrate");
            pool.close().await;
        })
        .await;
    let cfg = DbConfig::new(&url)
        .with_max_connections(12)
        .with_application_name("hos-db-tests");
    Some(hos_db::connect(&cfg).await.expect("connect"))
}

/// Random lower-case suffix.
pub fn random_suffix() -> String {
    // The tail of a v7 UUID is random (the head is the timestamp).
    uuid::Uuid::now_v7().simple().to_string()[20..].to_owned()
}

/// A throw-away tenant (with the default roles, currencies, KDV, invoice
/// sequence, departments, settings and e-mail templates seeded).
pub struct TestTenant {
    pub id: TenantId,
    pub slug: String,
    pool: PgPool,
    cleaned: bool,
}

impl TestTenant {
    pub async fn create(pool: &PgPool) -> Self {
        let slug = format!("test-{}", random_suffix());
        let mut new = platform::NewTenant::new(&slug, "Test Tenant");
        new.status = hos_core::domain::TenantStatus::Active;
        let row = platform::create_tenant(pool, &new)
            .await
            .expect("create tenant");
        let mut conn = pool.acquire().await.expect("acquire");
        hos_db::seed::seed_tenant_defaults(&mut conn, row.id)
            .await
            .expect("seed tenant defaults");
        Self {
            id: row.id,
            slug,
            pool: pool.clone(),
            cleaned: false,
        }
    }

    /// Deletes the tenant (cascade) and asserts nothing tenant-scoped is left.
    pub async fn cleanup(mut self) {
        assert!(
            platform::delete_tenant(&self.pool, self.id)
                .await
                .expect("delete tenant")
        );
        self.cleaned = true;
        let left: i64 = sqlx::query_scalar(
            "select (select count(*) from clients where tenant_id = $1)
                  + (select count(*) from invoices where tenant_id = $1)
                  + (select count(*) from jobs where tenant_id = $1)
                  + (select count(*) from roles where tenant_id = $1)",
        )
        .bind(self.id)
        .fetch_one(&self.pool)
        .await
        .expect("count leftovers");
        assert_eq!(left, 0, "tenant rows left after delete");
    }

    /// An active individual client billed in TRY.
    pub async fn client(&self, name: &str) -> ClientId {
        let email = format!("{}@example.com", name.to_lowercase());
        let input = clients::ClientInput::individual(name, "Test", &email, Currency::TRY);
        clients::create_client(&self.pool, self.id, &input)
            .await
            .expect("create client")
            .id
    }

    /// A gateway integration (`stripe`).
    pub async fn gateway(&self) -> IntegrationId {
        let new = integrations::NewIntegration {
            id: IntegrationId::new(),
            category: IntegrationCategory::Gateway,
            kind: hos_core::integrations::kinds::STRIPE.to_owned(),
            name: format!("Stripe {}", random_suffix()),
            config: serde_json::json!({}),
            secrets_enc: None,
            is_active: true,
            sort_order: 0,
        };
        integrations::create_integration(&self.pool, self.id, &new)
            .await
            .expect("create integration")
            .id
    }
}

impl Drop for TestTenant {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        // The test failed before `cleanup`: delete from a fresh runtime on
        // another thread (we may be inside the test's runtime here).
        let Some(url) = test_url() else { return };
        let id = self.id;
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                if let Ok(pool) =
                    hos_db::connect(&DbConfig::new(&url).with_max_connections(1)).await
                {
                    let _ = platform::delete_tenant(&pool, id).await;
                    pool.close().await;
                }
            });
        })
        .join();
    }
}

/// `Decimal` from a string literal.
pub fn d(s: &str) -> Decimal {
    s.parse().expect("decimal")
}

pub fn today() -> chrono::NaiveDate {
    Utc::now().date_naive()
}
