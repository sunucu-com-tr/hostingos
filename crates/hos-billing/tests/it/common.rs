//! Shared fixtures of the billing / jobs integration tests (also used by
//! `hos-jobs` through `#[path]`).
//!
//! Tests run against the database of the workspace `.env`; without one they
//! print a notice and pass. Every test creates its own random tenant (with
//! the seeded defaults: currencies, KDV %20, settings, templates) and deletes
//! it at the end — also when the test panics.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use hos_billing::payments::{StartPayment, WebhookResult};
use hos_billing::registry::SaveIntegration;
use hos_billing::{Billing, BillingConfig, IntegrationRegistry, ManualClock, NoFraudCheck};
use hos_core::crypto::SecretBox;
use hos_core::domain::{IntegrationCategory, ProductType};
use hos_core::ids::{CartId, ClientId, IntegrationId, InvoiceId, ProductId, TenantId};
use hos_core::integrations::gateway::PaymentEventKind;
use hos_core::money::{BillingCycle, Currency, Money};
use hos_core::net::OutboundPolicy;
use hos_db::enums::{AutoSetup, DomainRequirement, TldAction};
use hos_db::repo::carts::{self, NewCartItem};
use hos_db::repo::catalog::{self, ProductInput};
use hos_db::repo::clients::{self, ClientInput};
use hos_db::repo::domains::{self, TldInput, TldPriceInput};
use hos_db::repo::{platform, services};
use hos_db::{DbConfig, PgPool};
use rust_decimal::Decimal;
use serde_json::{Value, json};
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

/// Pooled URL (like the application).
pub fn test_url() -> Option<String> {
    load_env();
    env_url(&["DATABASE_URL", "DATABASE_URL_DIRECT"])
}

/// Direct URL (migrations, `LISTEN`).
pub fn direct_url() -> Option<String> {
    load_env();
    env_url(&["DATABASE_URL_DIRECT", "DATABASE_URL"])
}

/// Process-wide test setup: the wiremock gateway tests (`gateways`) reach
/// 127.0.0.1, which the default outbound policy (`private`) refuses. All test
/// modules share one process, and the policy is fixed by whatever reads it
/// first, so every database test selects it here before building anything.
pub fn outbound_policy_for_tests() {
    let _ = OutboundPolicy::set_global(OutboundPolicy::Unrestricted);
    // A `hostingos serve` on the same database must not claim (and fail)
    // the jobs of the test tenants: they live in the `tests.` queue namespace.
    hos_db::repo::jobs::isolate_queue("tests");
}

/// Connects and migrates once per test binary; `None` without a database.
pub async fn pool() -> Option<PgPool> {
    outbound_policy_for_tests();
    let Some(url) = test_url() else {
        eprintln!("DATABASE_URL / DATABASE_URL_DIRECT not set — skipping database test");
        return None;
    };
    MIGRATED
        .get_or_init(|| async {
            let direct = direct_url().expect("url");
            let pool = hos_db::connect(&DbConfig::new(&direct).with_max_connections(1))
                .await
                .expect("connect for migrations");
            hos_db::migrate(&pool).await.expect("migrate");
            pool.close().await;
        })
        .await;
    let cfg = DbConfig::new(&url)
        .with_max_connections(16)
        .with_application_name("hos-billing-tests");
    Some(hos_db::connect(&cfg).await.expect("connect"))
}

/// Random lower-case suffix.
pub fn suffix() -> String {
    uuid::Uuid::now_v7().simple().to_string()[20..].to_owned()
}

/// `Decimal` from a literal.
pub fn d(s: &str) -> Decimal {
    s.parse().expect("decimal")
}

/// Mock gateway webhook secret.
pub const GATEWAY_SECRET: &str = "whsec_test_secret_value";

/// A throw-away tenant plus a billing engine with a manual clock.
pub struct Env {
    pub pool: PgPool,
    pub tenant: TenantId,
    pub clock: Arc<ManualClock>,
    pub billing: Billing,
    cleaned: bool,
}

impl Env {
    /// `None` when no database is configured.
    pub async fn new() -> Option<Self> {
        let pool = pool().await?;
        let slug = format!("test-{}", suffix());
        let mut new = platform::NewTenant::new(&slug, "Billing Test");
        new.status = hos_core::domain::TenantStatus::Active;
        let row = platform::create_tenant(&pool, &new).await.expect("tenant");
        let mut conn = pool.acquire().await.expect("acquire");
        hos_db::seed::seed_tenant_defaults(&mut conn, row.id)
            .await
            .expect("seed tenant");
        drop(conn);
        let clock = Arc::new(ManualClock::new(Utc::now()));
        let registry = IntegrationRegistry::new(
            pool.clone(),
            SecretBox::new(&hos_core::crypto::random_bytes()),
        )
        .with_ttl(Duration::from_secs(3600));
        let mut config = BillingConfig::new("https://billing.test");
        config.vm_poll_interval = Duration::ZERO;
        // Every flow runs on the mock providers (refused without opt-in).
        config.allow_mock_integrations = true;
        let billing = Billing::with_parts(
            pool.clone(),
            registry,
            clock.clone(),
            config,
            Arc::new(NoFraudCheck),
        );
        Some(Self {
            pool,
            tenant: row.id,
            clock,
            billing,
            cleaned: false,
        })
    }

    /// Deletes the tenant (cascade).
    pub async fn cleanup(mut self) {
        platform::delete_tenant(&self.pool, self.tenant)
            .await
            .expect("delete tenant");
        self.cleaned = true;
    }

    /// Sets a tenant setting.
    pub async fn setting(&self, key: &str, value: Value) {
        platform::set_setting(&self.pool, self.tenant, key, &value)
            .await
            .expect("setting");
    }

    /// An active TR client with a full address and phone (domain contacts).
    pub async fn client(&self, name: &str) -> ClientId {
        let mut input = ClientInput::individual(
            name,
            "Tester",
            &format!("{}-{}@example.com", name.to_lowercase(), suffix()),
            Currency::TRY,
        );
        input.phone = Some("+90 555 123 45 67".into());
        input.address_line1 = Some("Atatürk Cad. 1".into());
        input.city = Some("İstanbul".into());
        input.postal_code = Some("34000".into());
        clients::create_client(&self.pool, self.tenant, &input)
            .await
            .expect("client")
            .id
    }

    pub async fn integration(
        &self,
        category: IntegrationCategory,
        kind: &str,
        config: Value,
        secrets: Value,
    ) -> IntegrationId {
        self.billing
            .registry()
            .save_integration(
                self.tenant,
                &SaveIntegration {
                    id: None,
                    category,
                    kind: kind.to_owned(),
                    name: format!("{category} {}", suffix()),
                    config,
                    secrets: Some(secrets),
                    is_active: true,
                    sort_order: 0,
                },
            )
            .await
            .expect("save integration")
            .id
    }

    /// An active staff member (receives `notify_staff` notifications).
    pub async fn staff(&self) -> hos_core::ids::UserId {
        hos_db::repo::identity::create_user(
            &self.pool,
            self.tenant,
            &hos_db::repo::identity::NewUser {
                email: format!("staff-{}@example.com", suffix()),
                password_hash: None,
                kind: hos_core::domain::UserKind::Staff,
                first_name: "Staff".into(),
                last_name: "Member".into(),
                phone: None,
                locale: hos_core::domain::Locale::En,
                email_verified: true,
            },
        )
        .await
        .expect("staff user")
        .id
    }

    /// Staff notifications of `kind` in this tenant.
    pub async fn notifications(&self, kind: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "select count(*) from notifications where tenant_id = $1 and kind = $2",
        )
        .bind(self.tenant)
        .bind(kind)
        .fetch_one(&self.pool)
        .await
        .expect("notifications")
    }

    /// Mock gateway (secret [`GATEWAY_SECRET`]).
    pub async fn gateway(&self) -> IntegrationId {
        self.integration(
            IntegrationCategory::Gateway,
            hos_payments::mock::KIND,
            json!({"default_outcome": "success"}),
            json!({"webhook_secret": GATEWAY_SECRET}),
        )
        .await
    }

    /// Mock panel in a private namespace.
    pub async fn panel(&self) -> IntegrationId {
        self.integration(
            IntegrationCategory::Panel,
            "mock",
            json!({"namespace": format!("ns-{}", suffix())}),
            json!({}),
        )
        .await
    }

    /// Mock hypervisor (`async_tasks`: creation needs task polling).
    pub async fn virt(&self, async_tasks: bool) -> IntegrationId {
        self.integration(
            IntegrationCategory::Virt,
            "mock",
            json!({"async_tasks": async_tasks, "task_polls": 2}),
            json!({}),
        )
        .await
    }

    /// Mock registrar.
    pub async fn registrar(&self) -> IntegrationId {
        self.integration(IntegrationCategory::Registrar, "mock", json!({}), json!({}))
            .await
    }

    /// A product priced monthly (and annually ×10) in TRY.
    pub async fn product(
        &self,
        name: &str,
        product_type: ProductType,
        integration: Option<IntegrationId>,
        module_config: Value,
        monthly: &str,
        setup: &str,
    ) -> ProductId {
        let mut input = ProductInput::new(&format!("p-{}", suffix()), name, product_type, None);
        input.integration_id = integration;
        input.module_config = module_config;
        input.auto_setup = AutoSetup::OnPayment;
        if matches!(
            product_type,
            ProductType::SharedHosting | ProductType::ResellerHosting
        ) {
            input.requires_domain = DomainRequirement::Required;
        }
        let p = catalog::create_product(&self.pool, self.tenant, &input)
            .await
            .expect("product");
        catalog::set_product_price(
            &self.pool,
            self.tenant,
            p.id,
            Currency::TRY,
            BillingCycle::Monthly,
            d(monthly),
            d(setup),
        )
        .await
        .expect("monthly price");
        catalog::set_product_price(
            &self.pool,
            self.tenant,
            p.id,
            Currency::TRY,
            BillingCycle::Annual,
            d(monthly) * Decimal::from(10),
            d(setup),
        )
        .await
        .expect("annual price");
        p.id
    }

    /// `.com` sold through `registrar` (300 TRY register / 350 renew / 320 transfer).
    pub async fn tld_com(&self, registrar: Option<IntegrationId>) {
        let mut t = TldInput::new("com");
        t.registrar_integration_id = registrar;
        let tld = domains::upsert_tld(&self.pool, self.tenant, &t)
            .await
            .expect("tld");
        let prices: Vec<TldPriceInput> = [
            (TldAction::Register, "300"),
            (TldAction::Renew, "350"),
            (TldAction::Transfer, "320"),
        ]
        .into_iter()
        .map(|(action, p)| TldPriceInput {
            currency: Currency::TRY,
            action,
            years: 1,
            price: d(p),
        })
        .collect();
        domains::set_tld_prices(&self.pool, self.tenant, tld.id, &prices)
            .await
            .expect("tld prices");
    }

    /// A /28 pool for `virt` (gateway .1).
    pub async fn ip_pool(&self, virt: IntegrationId) {
        let pool = services::create_ip_pool(
            &self.pool,
            self.tenant,
            "test-pool",
            "198.51.100.0/28".parse().expect("net"),
            Some("198.51.100.1".parse().expect("gw")),
            &["1.1.1.1".parse().expect("ns")],
            Some(virt),
        )
        .await
        .expect("ip pool");
        let addrs: Vec<std::net::IpAddr> = (2..=6)
            .map(|i| format!("198.51.100.{i}").parse().expect("ip"))
            .collect();
        services::add_ip_addresses(&self.pool, self.tenant, pool.id, &addrs)
            .await
            .expect("ips");
    }

    /// A cart of `client` with `items` (in order; `parent_item_id` is not used).
    pub async fn cart(&self, client: ClientId, items: &[NewCartItem]) -> CartId {
        let cart = carts::create_cart(
            &self.pool,
            self.tenant,
            Currency::TRY,
            None,
            Some(client),
            None,
        )
        .await
        .expect("cart");
        for it in items {
            carts::add_item(&self.pool, self.tenant, cart.id, it)
                .await
                .expect("cart item");
        }
        cart.id
    }

    /// Product cart item with a hosting domain.
    pub fn hosting_item(product: ProductId, domain: &str) -> NewCartItem {
        let mut it = NewCartItem::product(product, BillingCycle::Monthly);
        it.domain_name = Some(domain.to_owned());
        it
    }

    /// Checkout of `cart` for `client` with `gateway`.
    pub async fn checkout(
        &self,
        cart: CartId,
        client: ClientId,
        gateway: Option<IntegrationId>,
    ) -> hos_billing::CheckoutOutcome {
        self.billing
            .checkout(
                self.tenant,
                &hos_billing::CheckoutRequest {
                    cart_id: cart,
                    client_id: client,
                    user_id: None,
                    payment_integration_id: gateway,
                    ip: Some("203.0.113.9".parse().expect("ip")),
                    user_agent: Some("tests".into()),
                    notes: None,
                },
            )
            .await
            .expect("checkout")
    }

    /// Starts a mock payment and returns its merchant reference.
    pub async fn start_payment(&self, invoice: InvoiceId, gateway: IntegrationId) -> String {
        self.billing
            .start_payment(
                self.tenant,
                invoice,
                &StartPayment {
                    integration_id: Some(gateway),
                    ..StartPayment::default()
                },
            )
            .await
            .expect("start payment")
            .reference
    }

    /// Delivers a signed mock gateway webhook.
    pub async fn webhook(
        &self,
        gateway: IntegrationId,
        kind: PaymentEventKind,
        reference: &str,
        amount: Money,
    ) -> WebhookResult {
        let event = hos_payments::mock::MockEvent::new(kind, format!("mock_{reference}"), amount)
            .with_reference(reference);
        let req = hos_payments::mock::webhook_request(
            GATEWAY_SECRET.as_bytes(),
            &event,
            Utc::now().timestamp(),
        );
        self.billing
            .handle_webhook(self.tenant, gateway, &req)
            .await
            .expect("webhook")
    }

    /// Pays an invoice's full balance through the mock gateway.
    pub async fn pay(&self, invoice: InvoiceId, gateway: IntegrationId) -> WebhookResult {
        let inv = hos_db::repo::invoices::get_invoice(&self.pool, self.tenant, invoice)
            .await
            .expect("invoice")
            .expect("invoice exists");
        let reference = self.start_payment(invoice, gateway).await;
        self.webhook(
            gateway,
            PaymentEventKind::Paid,
            &reference,
            inv.balance_money(),
        )
        .await
    }

    /// Jobs of this tenant: `(kind, status)`.
    pub async fn jobs(&self) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "select kind, status::text from jobs where tenant_id = $1 order by created_at, id",
        )
        .bind(self.tenant)
        .fetch_all(&self.pool)
        .await
        .expect("jobs")
    }

    /// Payloads of this tenant's pending jobs of `kind`.
    pub async fn pending_payloads(&self, kind: &str) -> Vec<Value> {
        sqlx::query_scalar::<_, Value>(
            "select payload from jobs where tenant_id = $1 and kind = $2 and status = 'pending' order by created_at",
        )
        .bind(self.tenant)
        .bind(kind)
        .fetch_all(&self.pool)
        .await
        .expect("payloads")
    }

    /// Marks this tenant's pending jobs of `kind` completed (a test ran
    /// their effect directly).
    pub async fn complete_jobs(&self, kind: &str) {
        sqlx::query(
            "update jobs set status = 'completed', completed_at = now() where tenant_id = $1 and kind = $2 and status = 'pending'",
        )
        .bind(self.tenant)
        .bind(kind)
        .execute(&self.pool)
        .await
        .expect("complete jobs");
    }

    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(self.tenant)
            .fetch_one(&self.pool)
            .await
            .expect("count")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let Some(url) = test_url() else { return };
        let id = self.tenant;
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

/// TRY money.
pub fn try_(s: &str) -> Money {
    Money::new(d(s), Currency::TRY)
}
