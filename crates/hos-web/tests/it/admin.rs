//! Admin panel integration tests: permissions, catalog → storefront,
//! integrations (secrets), billing, service lifecycle with the mock panel,
//! impersonation, tenant isolation and CSV escaping.
//!
//! Like the other web tests they need the database from `.env`; without it
//! they print a notice and pass.

use crate::common::{Res, TestApp};
use hos_billing::registry::SaveIntegration;
use hos_core::domain::{IntegrationCategory, InvoiceStatus, ServiceStatus, UserKind};
use hos_core::ids::UserId;
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::clients::{self, ClientInput, ClientRow};
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::{catalog, invoices, services};
use hos_jobs::{Worker, WorkerConfig};
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;

const PASSWORD: &str = "Correct-Horse-Battery-9";

/// Creates a user of `kind` (staff: with role `role`).
async fn user(app: &TestApp, email: &str, kind: UserKind, role: Option<&str>) -> UserId {
    let u = identity::create_user(
        app.pool(),
        app.tenant_id,
        &NewUser {
            email: email.into(),
            password_hash: Some(hos_auth::password::hash_password(PASSWORD).unwrap()),
            kind,
            first_name: "Test".into(),
            last_name: "User".into(),
            phone: None,
            locale: hos_core::domain::Locale::Tr,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    if let Some(slug) = role {
        let r = identity::role_by_slug(app.pool(), app.tenant_id, slug)
            .await
            .unwrap()
            .expect("role");
        identity::assign_role(app.pool(), app.tenant_id, u.id, r.id)
            .await
            .unwrap();
    }
    u.id
}

async fn login(app: &mut TestApp, email: &str) {
    let page = app.get("/auth/login").await;
    let token = page.csrf();
    let r = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", &token),
                ("email", email),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "login: {}", r.body);
}

/// Logs in a fresh staff member with `role`; returns the CSRF token.
async fn staff(app: &mut TestApp, role: &str) -> String {
    let email = format!("{role}-{}@staff.test", uuid::Uuid::now_v7().simple());
    user(app, &email, UserKind::Staff, Some(role)).await;
    login(app, &email).await;
    let dash = app.get("/admin").await;
    assert_eq!(dash.status, StatusCode::OK, "{}", dash.body);
    dash.csrf()
}

async fn post(app: &mut TestApp, path: &str, csrf: &str, fields: &[(&str, &str)]) -> Res {
    let mut all = vec![("csrf_token", csrf)];
    all.extend_from_slice(fields);
    app.post(path, &all).await
}

async fn client(app: &TestApp, first: &str) -> ClientRow {
    let email = format!("{}@client.test", uuid::Uuid::now_v7().simple());
    clients::create_client(
        app.pool(),
        app.tenant_id,
        &ClientInput::individual(first, "Müşteri", &email, Currency::TRY),
    )
    .await
    .expect("client")
}

fn worker(app: &TestApp) -> Worker {
    Worker::new(
        app.pool().clone(),
        app.state.billing.clone(),
        WorkerConfig::default(),
    )
}

#[tokio::test]
async fn permission_matrix() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // Anonymous → login.
    let r = app.get("/admin").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("/auth/login"), "{}", r.location());

    // A client user → 403.
    let email = format!("{}@client.test", uuid::Uuid::now_v7().simple());
    user(&app, &email, UserKind::Client, None).await;
    login(&mut app, &email).await;
    for path in ["/admin", "/admin/clients", "/admin/invoices"] {
        let r = app.get(path).await;
        let refused = r.status == StatusCode::FORBIDDEN
            || (r.status == StatusCode::SEE_OTHER && !r.location().starts_with("/admin"));
        assert!(
            refused,
            "client reached {path}: {} {}",
            r.status,
            r.location()
        );
    }
    app.cookies.clear();

    // Support role: may view clients, may not manage billing / catalog.
    let csrf = staff(&mut app, "support").await;
    assert_eq!(app.get("/admin/clients").await.status, StatusCode::OK);
    assert_eq!(
        app.get("/admin/billing/coupons/new").await.status,
        StatusCode::FORBIDDEN
    );
    let r = post(
        &mut app,
        "/admin/billing/coupons",
        &csrf,
        &[
            ("code", "HACK"),
            ("discount_type", "percent"),
            ("value", "50"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        catalog::list_coupons(app.pool(), app.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        app.get("/admin/catalog").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.get("/admin/integrations").await.status,
        StatusCode::FORBIDDEN
    );
    // The menu hides what the role cannot open.
    let dash = app.get("/admin").await;
    assert!(!dash.body.contains("/admin/integrations\""));
    assert!(dash.body.contains("/admin/clients"));
    app.cleanup().await;
}

#[tokio::test]
async fn product_created_in_admin_shows_on_storefront() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;
    let r = post(
        &mut app,
        "/admin/catalog/groups",
        &csrf,
        &[("name", "Test Grubu"), ("slug", "test-grubu")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let group = catalog::product_group_by_slug(app.pool(), app.tenant_id, "test-grubu")
        .await
        .unwrap()
        .expect("group");
    let gid = group.id.to_string();
    let r = post(
        &mut app,
        "/admin/catalog/products",
        &csrf,
        &[
            ("name", "Süper Paket"),
            ("slug", "super-paket"),
            ("product_type", "shared_hosting"),
            ("group_id", &gid),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let product = catalog::product_by_slug(app.pool(), app.tenant_id, "super-paket")
        .await
        .unwrap()
        .expect("product");
    assert!(product.is_hidden, "new products start hidden");
    let base = format!("/admin/catalog/products/{}", product.id);
    let r = post(
        &mut app,
        &format!("{base}/pricing"),
        &csrf,
        &[("price_TRY_monthly", "99,90"), ("setup_TRY_monthly", "0")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post(
        &mut app,
        &base,
        &csrf,
        &[
            ("name", "Süper Paket"),
            ("slug", "super-paket"),
            ("product_type", "shared_hosting"),
            ("group_id", &gid),
            ("pay_type", "recurring"),
            ("auto_setup", "on_payment"),
            ("requires_domain", "optional"),
            ("description", "**Hızlı** hosting"),
            ("features", "10 GB disk\nSınırsız trafik"),
            ("is_taxable", "1"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let price = catalog::product_price(
        app.pool(),
        app.tenant_id,
        product.id,
        Currency::TRY,
        BillingCycle::Monthly,
    )
    .await
    .unwrap()
    .expect("price");
    assert_eq!(price.price, Decimal::new(9990, 2));
    let page = app.get("/test-grubu/super-paket").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Süper Paket"));
    assert!(page.body.contains("Sınırsız trafik"));
    // Audit trail.
    let n: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where tenant_id = $1 and action like 'product.%'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert!(n >= 3);
    app.cleanup().await;
}

#[tokio::test]
async fn integration_secret_is_encrypted_and_never_echoed() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;
    let secret = "whsec-super-secret-value-123";
    let r = post(
        &mut app,
        "/admin/integrations",
        &csrf,
        &[
            ("category", "gateway"),
            ("kind", "mock"),
            ("name", "Test ödeme"),
            ("c_default_outcome", "success"),
            ("c_webhook_tolerance_secs", "300"),
            ("s_webhook_secret", secret),
            ("is_active", "1"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let row = hos_db::repo::integrations::list_integrations(
        app.pool(),
        app.tenant_id,
        Some(IntegrationCategory::Gateway),
    )
    .await
    .unwrap()
    .into_iter()
    .find(|i| i.kind == "mock")
    .expect("saved");
    let sealed = row.secrets_enc.clone().expect("secrets stored");
    assert!(!String::from_utf8_lossy(&sealed).contains(secret));
    assert!(!row.config.to_string().contains(secret));
    let edit = app.get(&format!("/admin/integrations/{}", row.id)).await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(!edit.body.contains(secret), "secret echoed");
    assert!(edit.body.contains(&format!("/webhooks/gateway/{}", row.id)));
    // Blank secret on update keeps the stored one.
    let r = post(
        &mut app,
        &format!("/admin/integrations/{}", row.id),
        &csrf,
        &[
            ("name", "Test ödeme 2"),
            ("c_default_outcome", "success"),
            ("c_webhook_tolerance_secs", "300"),
            ("s_webhook_secret", ""),
            ("is_active", "1"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let row2 = hos_db::repo::integrations::get_integration(app.pool(), app.tenant_id, row.id)
        .await
        .unwrap()
        .unwrap();
    let secrets = app.state.billing.registry().decrypt_secrets(&row2).unwrap();
    assert_eq!(secrets["webhook_secret"], json!(secret));
    // Connection test (htmx fragment) against the mock provider.
    let host = app.host.clone();
    let t = app
        .send_to(
            &host,
            http::Method::POST,
            &format!("/admin/integrations/{}/test", row.id),
            Some(&[("csrf_token", csrf.as_str())][..]),
            &[("hx-request", "true")],
        )
        .await;
    assert_eq!(t.status, StatusCode::OK);
    assert!(t.body.contains("check-pass"), "{}", t.body);
    app.cleanup().await;
}

/// hos-billing refuses the `mock` providers (a mock gateway lets anyone pay
/// any invoice) unless the process opted in (`HOS_ALLOW_MOCK_INTEGRATIONS`,
/// debug mode, the test harness). Without the opt-in the admin form shows
/// the refusal, nothing is saved and the picker does not offer them.
#[tokio::test]
async fn mock_gateway_cannot_be_saved_without_the_opt_in() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // A production process (no opt-in).
    app.state
        .billing
        .registry()
        .set_mock_integrations_allowed(false);
    let csrf = staff(&mut app, "owner").await;
    let list = app.get("/admin/integrations").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(!list.body.contains("value=\"mock\""), "mock offered");
    assert!(list.body.contains("value=\"stripe\""));

    let r = post(
        &mut app,
        "/admin/integrations",
        &csrf,
        &[
            ("category", "gateway"),
            ("kind", "mock"),
            ("name", "Bedava ödeme"),
            ("c_default_outcome", "success"),
            ("c_webhook_tolerance_secs", "300"),
            ("s_webhook_secret", "whsec-x"),
            ("is_active", "1"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert!(r.body.contains("HOS_ALLOW_MOCK_INTEGRATIONS"), "{}", r.body);
    let saved = hos_db::repo::integrations::list_integrations(
        app.pool(),
        app.tenant_id,
        Some(IntegrationCategory::Gateway),
    )
    .await
    .unwrap();
    assert!(saved.iter().all(|i| i.kind != "mock"), "mock gateway saved");
    app.cleanup().await;
}

/// `href`s of the elements marked `aria-current="page"` (sidebar entry, tabs).
fn current_links(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("aria-current=\"page\"") {
        let tag = &rest[rest[..at].rfind('<').unwrap_or(0)..at];
        if let Some(h) = tag.find("href=\"") {
            let href = &tag[h + 6..];
            out.push(href[..href.find('"').unwrap_or(href.len())].to_owned());
        }
        rest = &rest[at + 1..];
    }
    out
}

/// Route fixes from a UI pass: GET on the API-key form's action, the
/// integration picker without a provider, the active sidebar entry of pages
/// without their own entry, and distinct ticket filter chips.
#[tokio::test]
async fn admin_route_fixes() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    staff(&mut app, "owner").await;
    let r = app.get("/admin/developer/keys").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/admin/developer");

    for (path, to) in [
        ("/admin/integrations/new", "/admin/integrations"),
        ("/admin/integrations/new?kind=", "/admin/integrations"),
        (
            "/admin/integrations/new?category=gateway",
            "/admin/integrations#cat-gateway",
        ),
    ] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{path}: {}", r.body);
        assert_eq!(r.location(), to, "{path}");
    }
    let r = app
        .get("/admin/integrations/new?category=gateway&kind=stripe")
        .await;
    assert_eq!(r.status, StatusCode::OK);

    // Pages without a sidebar entry no longer highlight the dashboard.
    let canned = app.get("/admin/support/canned").await;
    assert_eq!(canned.status, StatusCode::OK);
    let active = current_links(&canned.body);
    assert!(
        active.iter().any(|h| h == "/admin/support/tickets"),
        "{active:?}"
    );
    assert!(!active.iter().any(|h| h == "/admin"), "{active:?}");
    let notes = app.get("/admin/notifications").await;
    assert_eq!(notes.status, StatusCode::OK);
    let active = current_links(&notes.body);
    assert!(!active.iter().any(|h| h == "/admin"), "{active:?}");
    let dash = app.get("/admin").await;
    assert!(current_links(&dash.body).iter().any(|h| h == "/admin"));

    // "All open" and the `open` status are different filters.
    let tickets = app.get("/admin/support/tickets").await;
    assert_eq!(tickets.status, StatusCode::OK);
    assert_eq!(tickets.body.matches("?status=open\"").count(), 1);
    assert!(tickets.body.contains("?status=active\""));
    app.cleanup().await;
}

#[tokio::test]
async fn manual_payment_marks_invoice_paid() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "billing").await;
    let c = client(&app, "Ödeyen").await;
    let cid = c.id.to_string();
    let r = post(
        &mut app,
        "/admin/invoices",
        &csrf,
        &[
            ("client_id", &cid),
            ("issue_date", "2026-11-01"),
            ("due_date", "2026-11-08"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let path = r.location().to_owned();
    assert!(path.starts_with("/admin/invoices/"), "{path}");
    let r = post(
        &mut app,
        &format!("{path}/items"),
        &csrf,
        &[
            ("description", "Danışmanlık"),
            ("quantity", "1"),
            ("unit_price", "100"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post(&mut app, &format!("{path}/publish"), &csrf, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let id: uuid::Uuid = path.rsplit('/').next().unwrap().parse().unwrap();
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, id.into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Unpaid);
    let total = inv.balance.to_string();
    let r = post(
        &mut app,
        &format!("{path}/payment"),
        &csrf,
        &[
            ("amount", &total),
            ("method", "bank_transfer"),
            ("reference", "DEKONT-1"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, id.into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Paid);
    let page = app.get(&path).await;
    assert!(page.body.contains("DEKONT-1"));
    app.cleanup().await;
}

#[tokio::test]
async fn service_suspend_and_unsuspend_with_mock_panel() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "admin").await;
    let panel = app
        .state
        .billing
        .registry()
        .save_integration(
            app.tenant_id,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Panel,
                kind: "mock".into(),
                name: "Demo panel".into(),
                config: json!({"namespace": format!("t-{}", app.slug)}),
                secrets: Some(json!({})),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("panel");
    let mut input = catalog::ProductInput::new(
        "panel-paket",
        "Panel paketi",
        hos_core::domain::ProductType::SharedHosting,
        None,
    );
    input.integration_id = Some(panel.id);
    input.module_config = json!({"package": "starter"});
    let product = catalog::create_product(app.pool(), app.tenant_id, &input)
        .await
        .unwrap();
    let c = client(&app, "Hizmetli").await;
    let svc = services::create_service(
        app.pool(),
        app.tenant_id,
        &services::NewService {
            client_id: c.id,
            product_id: product.id,
            order_id: None,
            billing_cycle: BillingCycle::Monthly,
            first_payment_amount: Decimal::TEN,
            recurring_amount: Decimal::TEN,
            currency: Currency::TRY,
            quantity: 1,
            registration_date: chrono::Utc::now().date_naive(),
            next_due_date: None,
            next_invoice_date: None,
            integration_id: Some(panel.id),
            domain: Some("ornek-test.com".into()),
            username: None,
            config_selections: json!({}),
            custom_fields: json!({}),
        },
    )
    .await
    .unwrap();
    let base = format!("/admin/services/{}", svc.id);
    let w = worker(&app);
    let status = |app: &TestApp| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        async move {
            services::get_service(&pool, tenant, svc.id)
                .await
                .unwrap()
                .unwrap()
                .status
        }
    };
    let r = post(
        &mut app,
        &format!("{base}/command"),
        &csrf,
        &[("action", "create")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    w.run_tenant_until_idle(app.tenant_id, 20).await.unwrap();
    assert_eq!(status(&app).await, ServiceStatus::Active);
    let r = post(
        &mut app,
        &format!("{base}/command"),
        &csrf,
        &[("action", "suspend"), ("reason", "Test")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    w.run_tenant_until_idle(app.tenant_id, 20).await.unwrap();
    assert_eq!(status(&app).await, ServiceStatus::Suspended);
    let r = post(
        &mut app,
        &format!("{base}/command"),
        &csrf,
        &[("action", "unsuspend")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    w.run_tenant_until_idle(app.tenant_id, 20).await.unwrap();
    assert_eq!(status(&app).await, ServiceStatus::Active);
    let page = app.get(&base).await;
    assert_eq!(page.status, StatusCode::OK);
    app.cleanup().await;
}

#[tokio::test]
async fn impersonation_start_and_end_are_audited() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;
    let email = format!("{}@client.test", uuid::Uuid::now_v7().simple());
    let owner = user(&app, &email, UserKind::Client, None).await;
    let mut conn = app.pool().acquire().await.unwrap();
    let c = clients::create_client_with_owner(
        &mut conn,
        app.tenant_id,
        &ClientInput::individual("Taklit", "Edilen", &email, Currency::TRY),
        owner,
    )
    .await
    .unwrap();
    drop(conn);
    let owner_s = owner.to_string();
    let r = post(
        &mut app,
        &format!("/admin/clients/{}/impersonate", c.id),
        &csrf,
        &[("user_id", &owner_s)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(r.location().starts_with("/client"), "{}", r.location());
    let imp: Option<uuid::Uuid> = sqlx::query_scalar(
        "select impersonator_id from sessions where tenant_id = $1 and user_id = $2",
    )
    .bind(app.tenant_id)
    .bind(owner)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert!(imp.is_some(), "session records the impersonator");
    // An admin page opened while impersonating offers to end it (no bare 403);
    // the admin area itself stays closed to the client session.
    let back = app.get("/admin/clients").await;
    assert_eq!(back.status, StatusCode::SEE_OTHER, "{}", back.body);
    assert_eq!(back.location(), "/admin/impersonation/end");
    // End from the client session (CSRF of that session).
    let page = app.get("/admin/impersonation/end").await;
    assert_eq!(page.status, StatusCode::OK);
    let token = page.csrf();
    let r = app
        .post("/admin/impersonation/end", &[("csrf_token", &token)])
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("/admin"), "{}", r.location());
    assert_eq!(
        app.get("/admin").await.status,
        StatusCode::OK,
        "staff session restored"
    );
    let actions: Vec<String> = sqlx::query_scalar(
        "select action from audit_log where tenant_id = $1 and action like 'client.impersonate%' order by created_at",
    )
    .bind(app.tenant_id)
    .fetch_all(app.pool())
    .await
    .unwrap();
    assert_eq!(
        actions,
        ["client.impersonate_start", "client.impersonate_end"]
    );
    app.cleanup().await;
}

#[tokio::test]
async fn staff_cannot_open_another_tenants_records() {
    let Some(mut a) = TestApp::new().await else {
        return;
    };
    let Some(b) = TestApp::new().await else {
        return;
    };
    let foreign = client(&b, "Yabancı").await;
    let product = catalog::create_product(
        b.pool(),
        b.tenant_id,
        &catalog::ProductInput::new(
            "yabanci",
            "Yabancı",
            hos_core::domain::ProductType::Other,
            None,
        ),
    )
    .await
    .unwrap();
    let _csrf = staff(&mut a, "owner").await;
    for path in [
        format!("/admin/clients/{}", foreign.id),
        format!("/admin/clients/{}/edit", foreign.id),
        format!("/admin/catalog/products/{}", product.id),
        format!("/admin/invoices/new?client={}", foreign.id),
    ] {
        let r = a.get(&path).await;
        assert!(
            r.status == StatusCode::NOT_FOUND || !r.body.contains("Yabancı"),
            "{path} leaked: {}",
            r.status
        );
    }
    assert_eq!(
        a.get(&format!("/admin/clients/{}", foreign.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    a.cleanup().await;
    b.cleanup().await;
}

#[tokio::test]
async fn csv_export_escapes_formulas() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let _csrf = staff(&mut app, "owner").await;
    client(&app, "=HYPERLINK(\"http://evil\")").await;
    let r = app.get("/admin/clients/export.csv").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.content_type().starts_with("text/csv"));
    assert!(r.body.contains("'=HYPERLINK"), "{}", r.body);
    assert!(!r.body.contains(",=HYPERLINK"));
    app.cleanup().await;
}

/// A pending order of `client` with one domain line (no invoice).
async fn pending_order(app: &TestApp, client: &ClientRow) -> hos_core::ids::OrderId {
    use hos_db::enums::LineItemType;
    use hos_db::repo::orders::{self, NewOrder, NewOrderItem};
    let mut conn = app.pool().acquire().await.unwrap();
    let domain = format!("bulk-{}.com", uuid::Uuid::now_v7().simple());
    let (order, _) = orders::create_order(
        &mut conn,
        app.tenant_id,
        &NewOrder {
            client_id: client.id,
            user_id: None,
            currency: Currency::TRY,
            coupon_id: None,
            coupon_code: None,
            payment_integration_id: None,
            tax_total: Decimal::ZERO,
            ip: None,
            user_agent: None,
            notes: None,
            cart_id: None,
            items: vec![NewOrderItem::domain(
                LineItemType::DomainRegister,
                &domain,
                1,
                "Domain",
                Decimal::new(10, 0),
            )],
        },
    )
    .await
    .expect("order");
    order.id
}

/// A draft invoice of `client` with one line, created through the admin.
async fn draft_invoice(app: &mut TestApp, csrf: &str, client: &ClientRow) -> uuid::Uuid {
    let cid = client.id.to_string();
    let r = post(
        app,
        "/admin/invoices",
        csrf,
        &[
            ("client_id", &cid),
            ("issue_date", "2026-11-01"),
            ("due_date", "2026-11-08"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let path = r.location().to_owned();
    let r = post(
        app,
        &format!("{path}/items"),
        csrf,
        &[
            ("description", "Danışmanlık"),
            ("quantity", "1"),
            ("unit_price", "100"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    path.rsplit('/').next().unwrap().parse().unwrap()
}

#[tokio::test]
async fn invoice_and_order_bulk_actions() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let mut other = TestApp::new().await.unwrap();
    let foreign_client = client(&other, "Yabancı").await;
    let foreign_order = pending_order(&other, &foreign_client).await;
    let ocsrf = staff(&mut other, "billing").await;
    let foreign_invoice = draft_invoice(&mut other, &ocsrf, &foreign_client).await;

    let csrf = staff(&mut app, "billing").await;
    let c = client(&app, "Toplu").await;
    let a = draft_invoice(&mut app, &csrf, &c).await;
    let b = draft_invoice(&mut app, &csrf, &c).await;
    let (a_s, b_s, f_s) = (a.to_string(), b.to_string(), foreign_invoice.to_string());
    let status = |app: &TestApp, id: uuid::Uuid| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        async move {
            invoices::get_invoice(&pool, tenant, id.into())
                .await
                .unwrap()
                .unwrap()
        }
    };

    // The list offers the selection to billing staff.
    let list = app.get("/admin/invoices").await;
    assert!(list.body.contains("/admin/invoices/bulk"), "bulk form");
    assert!(list.body.contains(&format!("name=\"ids\" value=\"{a}\"")));

    // Publish both drafts; another tenant's id is skipped, not touched.
    let r = post(
        &mut app,
        "/admin/invoices/bulk",
        &csrf,
        &[
            ("ids", &a_s),
            ("ids", &b_s),
            ("ids", &f_s),
            ("action", "publish"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(status(&app, a).await.status, InvoiceStatus::Unpaid);
    assert_eq!(status(&app, b).await.status, InvoiceStatus::Unpaid);
    assert_eq!(
        status(&other, foreign_invoice).await.status,
        InvoiceStatus::Draft
    );

    // Remind both, then cancel one; a second cancel is skipped.
    let r = post(
        &mut app,
        "/admin/invoices/bulk",
        &csrf,
        &[("ids", &a_s), ("ids", &b_s), ("action", "remind")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(status(&app, a).await.reminder_count, 1);
    assert_eq!(status(&app, b).await.reminder_count, 1);
    for _ in 0..2 {
        let r = post(
            &mut app,
            "/admin/invoices/bulk",
            &csrf,
            &[("ids", &a_s), ("action", "cancel")],
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    }
    assert_eq!(status(&app, a).await.status, InvoiceStatus::Cancelled);
    assert_eq!(status(&app, b).await.status, InvoiceStatus::Unpaid);
    // Unknown action / empty selection: nothing changes.
    let r = post(
        &mut app,
        "/admin/invoices/bulk",
        &csrf,
        &[("ids", &b_s), ("action", "delete")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(status(&app, b).await.status, InvoiceStatus::Unpaid);

    // Orders: cancel a pending order; other tenants' and already
    // cancelled orders are skipped, accepting a cancelled one does nothing.
    let o1 = pending_order(&app, &c).await;
    let o2 = pending_order(&app, &c).await;
    let (o1_s, o2_s, fo_s) = (o1.to_string(), o2.to_string(), foreign_order.to_string());
    let list = app.get("/admin/orders").await;
    assert!(list.body.contains("/admin/orders/bulk"), "bulk form");
    let r = post(
        &mut app,
        "/admin/orders/bulk",
        &csrf,
        &[("ids", &o1_s), ("ids", &fo_s), ("action", "cancel")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let order_status = |app: &TestApp, id| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        async move {
            hos_db::repo::orders::get_order(&pool, tenant, id)
                .await
                .unwrap()
                .unwrap()
                .status
        }
    };
    use hos_core::domain::OrderStatus;
    assert_eq!(order_status(&app, o1).await, OrderStatus::Cancelled);
    assert_eq!(order_status(&app, o2).await, OrderStatus::Pending);
    assert_eq!(
        order_status(&other, foreign_order).await,
        OrderStatus::Pending
    );
    let r = post(
        &mut app,
        "/admin/orders/bulk",
        &csrf,
        &[("ids", &o1_s), ("action", "accept")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(order_status(&app, o1).await, OrderStatus::Cancelled);

    // Plan limit, checked for every selected order: over the plan nothing
    // is accepted; back within it the order is.
    let (pool, tenant) = (app.pool().clone(), app.tenant_id);
    let limit = |max: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "update tenants set metadata = coalesce(metadata, '{}'::jsonb)
                   || jsonb_build_object('limits', coalesce(metadata->'limits', '{}'::jsonb)
                        || jsonb_build_object('max_services', $2::bigint))
                 where id = $1",
            )
            .bind(tenant)
            .bind(max)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let used: i64 = sqlx::query_scalar(
        "select count(*) from services where tenant_id = $1
            and status in ('pending', 'provisioning', 'active', 'suspended')",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    limit(used - 1).await;
    let r = post(
        &mut app,
        "/admin/orders/bulk",
        &csrf,
        &[("ids", &o2_s), ("action", "accept")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        order_status(&app, o2).await,
        OrderStatus::Pending,
        "over the plan"
    );
    limit(used).await;
    let r = post(
        &mut app,
        "/admin/orders/bulk",
        &csrf,
        &[("ids", &o2_s), ("action", "accept")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_ne!(
        order_status(&app, o2).await,
        OrderStatus::Pending,
        "within the plan"
    );

    // Without billing.manage both are refused.
    app.cookies.clear();
    let csrf = staff(&mut app, "support").await;
    for (path, action) in [
        ("/admin/invoices/bulk", "cancel"),
        ("/admin/orders/bulk", "cancel"),
    ] {
        let r = post(
            &mut app,
            path,
            &csrf,
            &[("ids", &b_s), ("ids", &o2_s), ("action", action)],
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(status(&app, b).await.status, InvoiceStatus::Unpaid);
    assert_ne!(order_status(&app, o2).await, OrderStatus::Cancelled);
    app.cleanup().await;
    other.cleanup().await;
}

#[tokio::test]
async fn task_and_inbound_mail_bulk_actions() {
    use hos_db::repo::mail_inbound::{self as mrepo, InboundSource, InboundStatus, NewInbound};
    use hos_db::repo::tasks::{self as trepo, TaskInput, TaskPriority, TaskStatus};
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let other = TestApp::new().await.unwrap();
    let task = |title: &str| TaskInput {
        title: title.into(),
        body: None,
        priority: TaskPriority::Normal,
        due_date: None,
        assigned_to: None,
        client_id: None,
    };
    let t1 = trepo::create_task(app.pool(), app.tenant_id, &task("Bir"), None)
        .await
        .unwrap();
    let t2 = trepo::create_task(app.pool(), app.tenant_id, &task("İki"), None)
        .await
        .unwrap();
    let tf = trepo::create_task(other.pool(), other.tenant_id, &task("Yabancı"), None)
        .await
        .unwrap();
    let task_status = |app: &TestApp, id| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        async move {
            trepo::get_task(&pool, tenant, id)
                .await
                .unwrap()
                .unwrap()
                .status
        }
    };

    let csrf = staff(&mut app, "support").await;
    let page = app.get("/admin/tasks").await;
    assert!(page.body.contains("/admin/tasks/bulk"), "bulk form");
    let (s1, s2, sf) = (t1.id.to_string(), t2.id.to_string(), tf.id.to_string());
    let r = post(
        &mut app,
        "/admin/tasks/bulk",
        &csrf,
        &[
            ("ids", &s1),
            ("ids", &s2),
            ("ids", &sf),
            ("action", "complete"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(task_status(&app, t1.id).await, TaskStatus::Done);
    assert_eq!(task_status(&app, t2.id).await, TaskStatus::Done);
    assert_eq!(task_status(&other, tf.id).await, TaskStatus::Open);
    let r = post(
        &mut app,
        "/admin/tasks/bulk",
        &csrf,
        &[
            ("ids", &s2),
            ("action", "reopen"),
            ("back", "/admin/tasks?show=done"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/admin/tasks?show=done");
    assert_eq!(task_status(&app, t2.id).await, TaskStatus::Open);
    // The per-row button still posts to the status route inside the form.
    let r = post(
        &mut app,
        &format!("/admin/tasks/{s1}/status"),
        &csrf,
        &[("status", "open"), ("ids", &s2), ("action", "complete")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(task_status(&app, t1.id).await, TaskStatus::Open);
    assert_eq!(task_status(&app, t2.id).await, TaskStatus::Open);

    // Quarantined e-mail: discard the selection; a reviewed message and
    // another tenant's message stay as they are.
    let msg = |subject: &str| NewInbound {
        source: InboundSource::Raw,
        message_id: Some(format!("<{}@bulk.test>", uuid::Uuid::now_v7().simple())),
        sender: Some("someone@bulk.test".into()),
        recipient: Some("support@bulk.test".into()),
        subject: Some(subject.into()),
        status: InboundStatus::Quarantined,
        reason: Some("unknown_sender".into()),
        ticket_id: None,
        body_text: Some("gizli içerik".into()),
        attachment_names: vec![],
        sender_verified: None,
        size_bytes: 10,
    };
    let m1 = mrepo::record(app.pool(), app.tenant_id, &msg("Bir"))
        .await
        .unwrap()
        .unwrap();
    let m2 = mrepo::record(app.pool(), app.tenant_id, &msg("İki"))
        .await
        .unwrap()
        .unwrap();
    let mf = mrepo::record(other.pool(), other.tenant_id, &msg("Yabancı"))
        .await
        .unwrap()
        .unwrap();
    let page = app.get("/admin/support/inbound").await;
    assert!(
        page.body.contains("/admin/support/inbound/bulk"),
        "bulk form"
    );
    let (ms1, msf) = (m1.to_string(), mf.to_string());
    let r = post(
        &mut app,
        "/admin/support/inbound/bulk",
        &csrf,
        &[("ids", &ms1), ("ids", &msf), ("action", "discard")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let state_of = |app: &TestApp, id| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        async move {
            sqlx::query_as::<_, (String, Option<String>)>(
                "select status::text, body_text from mail_inbound_messages where tenant_id = $1 and id = $2",
            )
            .bind(tenant)
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(state_of(&app, m1).await, ("discarded".into(), None));
    assert_eq!(state_of(&app, m2).await.0, "quarantined");
    assert_eq!(state_of(&other, mf).await.0, "quarantined");

    // Staff of a role without support.manage / tasks.manage cannot use either.
    identity::create_role(
        app.pool(),
        app.tenant_id,
        "readonly",
        "Salt okunur",
        None,
        false,
    )
    .await
    .unwrap();
    app.cookies.clear();
    let csrf = staff(&mut app, "readonly").await;
    for path in ["/admin/tasks/bulk", "/admin/support/inbound/bulk"] {
        let r = post(
            &mut app,
            path,
            &csrf,
            &[
                ("ids", &s2),
                ("ids", &m2.to_string()),
                ("action", "discard"),
            ],
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
    }
    app.cleanup().await;
    other.cleanup().await;
}

/// Audit entries of a deleted record show as plain text, not as a link to
/// a page that no longer exists.
#[tokio::test]
async fn audit_entries_of_deleted_records_do_not_link() {
    use hos_db::enums::ActorKind;
    use hos_db::repo::audit::{self, AuditEntry};
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    staff(&mut app, "owner").await;
    let kept = client(&app, "Kalan").await;
    let gone = client(&app, "Silinen").await;
    for c in [&kept, &gone] {
        audit::write(
            app.pool(),
            &AuditEntry::new(
                Some(app.tenant_id),
                ActorKind::System,
                None,
                "client.update",
            )
            .entity("client", c.id),
        )
        .await
        .unwrap();
    }
    sqlx::query("delete from clients where tenant_id = $1 and id = $2")
        .bind(app.tenant_id)
        .bind(gone.id)
        .execute(app.pool())
        .await
        .unwrap();
    for path in ["/admin/system/audit", "/admin"] {
        let page = app.get(path).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        assert!(
            page.body.contains(&format!("/admin/clients/{}\"", kept.id)),
            "{path}: link to the existing record"
        );
        assert!(
            !page.body.contains(&format!("/admin/clients/{}", gone.id)),
            "{path}: no link to the deleted record"
        );
    }
    let missing = app.get(&format!("/admin/clients/{}", gone.id)).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    app.cleanup().await;
}
