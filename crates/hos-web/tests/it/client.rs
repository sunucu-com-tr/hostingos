//! Cart → checkout → payment → client area, end to end (TestApp: a fresh
//! tenant per test, mock gateway / registrar / panel / virt integrations).
//! Skipped when no database is configured.

use std::str::FromStr;

use crate::common::{self, TestApp};
use axum::body::Body;
use chrono::Utc;
use hos_core::domain::{IntegrationCategory, InvoiceStatus, ProductType, ServiceStatus};
use hos_core::ids::{ClientId, IntegrationId, InvoiceId};
use hos_core::integrations::gateway::WebhookRequest;
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::{AutoSetup, CreditKind, DiscountType, DomainRequirement, PayType, TldAction};
use hos_db::repo::{
    catalog, clients, domains, identity, integrations, invoices, payments, services,
};
use http::{Method, Request, StatusCode};
use rust_decimal::Decimal;
use serde_json::json;
use tower::ServiceExt;

const PASSWORD: &str = "Gizli-Parola-2026!xq";

struct Fixture {
    gateway: IntegrationId,
    currency: Currency,
}

async fn integration(
    app: &TestApp,
    category: IntegrationCategory,
    kind: &str,
    secrets: serde_json::Value,
) -> IntegrationId {
    let id = IntegrationId::new();
    let sealed = app
        .state
        .secret_box
        .encrypt_json(
            &secrets,
            &hos_core::crypto::integration_aad(app.tenant_id, id),
        )
        .expect("seal");
    integrations::create_integration(
        app.pool(),
        app.tenant_id,
        &integrations::NewIntegration {
            id,
            category,
            kind: kind.to_owned(),
            name: format!("Mock {kind}"),
            config: json!({}),
            secrets_enc: Some(sealed),
            is_active: true,
            sort_order: 0,
        },
    )
    .await
    .expect("integration");
    id
}

async fn product(
    app: &TestApp,
    slug: &str,
    kind: ProductType,
    integration: IntegrationId,
    module_config: serde_json::Value,
    currency: Currency,
    price: i64,
) {
    let p = catalog::create_product(
        app.pool(),
        app.tenant_id,
        &catalog::ProductInput {
            group_id: None,
            slug: slug.into(),
            product_type: kind,
            name: format!("Test {slug}"),
            tagline: None,
            description: None,
            features: json!([]),
            translations: json!({}),
            integration_id: Some(integration),
            module_config,
            pay_type: PayType::Recurring,
            auto_setup: AutoSetup::OnPayment,
            requires_domain: if kind == ProductType::SharedHosting {
                DomainRequirement::Required
            } else {
                DomainRequirement::None
            },
            welcome_email_template: None,
            is_taxable: true,
            is_featured: false,
            is_hidden: false,
            sort_order: 0,
        },
    )
    .await
    .expect("product");
    for (cycle, factor) in [(BillingCycle::Monthly, 1), (BillingCycle::Annual, 10)] {
        catalog::set_product_price(
            app.pool(),
            app.tenant_id,
            p.id,
            currency,
            cycle,
            Decimal::from(price * factor),
            Decimal::ZERO,
        )
        .await
        .expect("price");
    }
}

async fn setup(app: &TestApp) -> Fixture {
    app.setting("auth.require_email_verification", json!(false))
        .await;
    let currency = catalog::list_currencies(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.is_default)
        .map_or(Currency::TRY, |c| c.code);
    let gateway = integration(
        app,
        IntegrationCategory::Gateway,
        hos_payments::MOCK,
        json!({"webhook_secret": "test-webhook-secret"}),
    )
    .await;
    let panel = integration(app, IntegrationCategory::Panel, hos_panels::MOCK, json!({})).await;
    let virt = integration(app, IntegrationCategory::Virt, hos_virt::MOCK, json!({})).await;
    let registrar = integration(
        app,
        IntegrationCategory::Registrar,
        hos_domains::mock::KIND,
        json!({}),
    )
    .await;
    product(
        app,
        "web-test",
        ProductType::SharedHosting,
        panel,
        json!({"package": "starter"}),
        currency,
        100,
    )
    .await;
    product(
        app,
        "vps-test",
        ProductType::Vps,
        virt,
        json!({"cpu_cores": 2, "memory_mb": 4096, "disk_gb": 80, "bandwidth_gb": 4000, "template": "ubuntu-24.04"}),
        currency,
        300,
    )
    .await;
    let tld = domains::upsert_tld(
        app.pool(),
        app.tenant_id,
        &domains::TldInput {
            tld: "com".into(),
            registrar_integration_id: Some(registrar),
            requires_epp: true,
            supports_privacy: true,
            min_years: 1,
            max_years: 10,
            grace_period_days: 30,
            redemption_period_days: 30,
            label: None,
            is_featured: true,
            sort_order: 0,
        },
    )
    .await
    .expect("tld");
    let prices: Vec<domains::TldPriceInput> =
        [TldAction::Register, TldAction::Transfer, TldAction::Renew]
            .into_iter()
            .map(|action| domains::TldPriceInput {
                currency,
                action,
                years: 1,
                price: Decimal::from(400),
            })
            .collect();
    domains::set_tld_prices(app.pool(), app.tenant_id, tld.id, &prices)
        .await
        .expect("tld prices");
    Fixture { gateway, currency }
}

/// `value` of `<input … name="name" value="…">`.
fn input_value(html: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{name}\" value=\"");
    let start = html.find(&needle)? + needle.len();
    let end = html[start..].find('"')? + start;
    Some(unescape(&html[start..end]))
}

/// Decodes the HTML entities Askama emits (numeric and the common named ones).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let decoded = match entity {
            "quot" => Some('"'),
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Anonymous CSRF token (bound to the `hos_anon` cookie) or the session's.
async fn csrf(app: &mut TestApp, page: &str) -> String {
    app.get(page).await.csrf()
}

async fn register(app: &mut TestApp, email: &str) {
    let token = csrf(app, "/auth/register").await;
    let res = app
        .post(
            "/auth/register",
            &[
                ("csrf_token", &token),
                ("first_name", "Deneme"),
                ("last_name", "Müşteri"),
                ("email", email),
                ("password", PASSWORD),
                ("terms", "1"),
                ("next", "/checkout"),
                ("website", ""),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "register: {}", res.body);
}

async fn client_of(app: &TestApp, email: &str) -> ClientId {
    let user = identity::user_by_email(app.pool(), app.tenant_id, email)
        .await
        .unwrap()
        .expect("user");
    clients::clients_of_user(app.pool(), app.tenant_id, user.id)
        .await
        .unwrap()
        .first()
        .expect("client")
        .id
}

async fn billing_details(app: &mut TestApp) {
    let token = csrf(app, "/checkout").await;
    let res = app
        .post(
            "/checkout/details",
            &[
                ("csrf_token", &token),
                ("kind", "individual"),
                ("first_name", "Deneme"),
                ("last_name", "Müşteri"),
                ("phone", "+90 532 000 00 00"),
                ("address_line1", "Örnek Mah. 1. Sok. No:1"),
                ("city", "İstanbul"),
                ("postal_code", "34000"),
                ("country_code", "TR"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "details: {}", res.body);
    assert!(res.location().ends_with("/checkout/review"));
}

/// Places the order with `payment` and returns the response + cart id.
async fn place(app: &mut TestApp, payment: &str) -> (common::Res, String) {
    let review = app.get("/checkout/review").await;
    assert_eq!(review.status, StatusCode::OK, "review: {}", review.body);
    let cart = input_value(&review.body, "cart").expect("cart id");
    let token = review.csrf();
    let res = app
        .post(
            "/checkout/place",
            &[
                ("csrf_token", &token),
                ("cart", &cart),
                ("payment", payment),
                ("terms", "1"),
            ],
        )
        .await;
    (res, cart)
}

async fn run_worker(app: &TestApp) {
    let worker = hos_jobs::worker::Worker::new(
        app.pool().clone(),
        app.state.billing.clone(),
        hos_jobs::worker::WorkerConfig::default(),
    );
    worker
        .run_tenant_until_idle(app.tenant_id, 200)
        .await
        .expect("worker");
}

async fn add(app: &mut TestApp, fields: &[(&str, &str)]) -> common::Res {
    let token = csrf(app, "/cart").await;
    let mut all = vec![("csrf_token", token.as_str())];
    all.extend_from_slice(fields);
    app.post("/cart/add", &all).await
}

/// Hosting needs a domain, so "add" leads to the configure page; registers
/// `domain` alongside.
async fn order_hosting(app: &mut TestApp, domain: &str) {
    let res = add(app, &[("product", "web-test"), ("cycle", "monthly")]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(res.location().contains("/cart/configure/web-test"));
    let page = app.get("/cart/configure/web-test?cycle=monthly").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let res = app
        .post(
            "/cart/configure/web-test",
            &[
                ("csrf_token", &page.csrf()),
                ("cycle", "monthly"),
                ("domain_mode", "register"),
                ("domain", domain),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
}

#[tokio::test]
async fn guest_cart_checkout_gateway_payment_activates_service() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let fx = setup(&app).await;
    catalog::create_coupon(
        app.pool(),
        app.tenant_id,
        &catalog::CouponInput {
            code: "INDIRIM10".into(),
            description: None,
            discount_type: DiscountType::Percent,
            value: Decimal::from(10),
            currency: None,
            product_ids: Vec::new(),
            recurring: false,
            new_clients_only: false,
            once_per_client: false,
            max_uses: None,
            starts_at: None,
            expires_at: None,
        },
    )
    .await
    .expect("coupon");

    // Guest: hosting (configured with a new domain) + coupon.
    order_hosting(&mut app, "deneme-alani-hos.com").await;
    assert!(
        app.cookies.contains_key("hos_cart"),
        "guest cart cookie set"
    );
    let token = csrf(&mut app, "/cart").await;
    let res = app
        .post(
            "/cart/coupon",
            &[("csrf_token", &token), ("code", "indirim10")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let cart = app.get("/cart").await;
    assert_eq!(cart.status, StatusCode::OK);
    assert!(cart.body.contains("Test web-test"), "product listed");
    assert!(cart.body.contains("deneme-alani-hos.com"), "domain listed");
    assert!(cart.body.contains("INDIRIM10"), "coupon applied");
    assert_eq!(cart.headers.get("cache-control").unwrap(), "no-store");
    // Live summary via htmx: changing the cycle re-quotes.
    let token = cart.csrf();
    let product_row = cart.body.split("/cart/items/").nth(1).unwrap();
    let item_id = &product_row[..36];
    let res = app
        .send_to(
            &app.host.clone(),
            Method::POST,
            &format!("/cart/items/{item_id}"),
            Some(&[("csrf_token", &token), ("cycle", "annual")]),
            &[("hx-request", "true")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("id=\"cart\""));
    assert!(res.headers.get("hx-trigger").is_some(), "toast for htmx");

    // Checkout: inline account step, then register → details → review.
    let page = app.get("/checkout").await;
    assert!(page.body.contains("/auth/register"));
    register(&mut app, "alici@example.test").await;
    let page = app.get("/checkout").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("/checkout/details"), "billing step");
    billing_details(&mut app).await;
    let (res, cart_id) = place(&mut app, &fx.gateway.to_string()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let csp = res
        .headers
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("'nonce-"), "auto-submit script nonce: {csp}");
    let payload = input_value(&res.body, "payload").expect("mock payload");
    let signature = input_value(&res.body, "signature").expect("mock signature");

    // Double submit: the converted cart leads to the existing invoice.
    let token = csrf(&mut app, "/client").await;
    let again = app
        .post(
            "/checkout/place",
            &[
                ("csrf_token", &token),
                ("cart", &cart_id),
                ("payment", &fx.gateway.to_string()),
                ("terms", "1"),
            ],
        )
        .await;
    assert_eq!(again.status, StatusCode::SEE_OTHER);
    assert!(again.location().contains("/client/invoices/"));
    let invoice_id = InvoiceId::from_str(again.location().rsplit('/').next().unwrap()).unwrap();

    // The gateway posts back (the browser form → webhook).
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "application/x-www-form-urlencoded".parse().unwrap(),
    );
    let body = common::serde_urlencoded(&[("payload", &payload), ("signature", &signature)]);
    app.state
        .billing
        .handle_webhook(
            app.tenant_id,
            fx.gateway,
            &WebhookRequest {
                headers,
                body: body.into_bytes(),
                query: None,
                remote_ip: None,
            },
        )
        .await
        .expect("webhook");
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Paid);
    assert!(
        inv.discount_total > Decimal::ZERO,
        "coupon discount on the invoice"
    );

    run_worker(&app).await;
    let client = client_of(&app, "alici@example.test").await;
    let svcs = services::client_services(app.pool(), app.tenant_id, client)
        .await
        .unwrap();
    assert_eq!(svcs.len(), 1);
    let events = services::service_events(app.pool(), app.tenant_id, svcs[0].service.id, 20)
        .await
        .unwrap();
    assert_eq!(
        svcs[0].service.status,
        ServiceStatus::Active,
        "{:?}",
        events.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert_eq!(svcs[0].service.billing_cycle, BillingCycle::Annual);
    let list = app.get("/client/services").await;
    assert!(list.body.contains("Test web-test"));
    let dash = app.get("/client").await;
    assert_eq!(dash.status, StatusCode::OK);
    let detail = app.get(&format!("/client/invoices/{invoice_id}")).await;
    assert!(detail.body.contains("badge"), "invoice page renders");
    let print = app
        .get(&format!("/client/invoices/{invoice_id}/print"))
        .await;
    assert_eq!(print.status, StatusCode::OK);

    // Domain registered → nameserver update.
    let doms = domains::client_domains(app.pool(), app.tenant_id, client)
        .await
        .unwrap();
    assert_eq!(doms.len(), 1, "domain created");
    let dom = &doms[0];
    let page = app.get(&format!("/client/domains/{}", dom.id)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let res = app
        .post(
            &format!("/client/domains/{}/nameservers", dom.id),
            &[
                ("csrf_token", &page.csrf()),
                ("nameservers", "ns1.ornek-dns.net\nns2.ornek-dns.net"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let dom = domains::get_domain(app.pool(), app.tenant_id, dom.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        dom.nameservers,
        vec!["ns1.ornek-dns.net", "ns2.ornek-dns.net"]
    );
    app.cleanup().await;
}

#[tokio::test]
async fn credit_payment_vps_power_and_tickets() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let fx = setup(&app).await;
    register(&mut app, "vps@example.test").await;
    let client = client_of(&app, "vps@example.test").await;
    let mut conn = app.pool().acquire().await.unwrap();
    payments::adjust_credit(
        &mut conn,
        app.tenant_id,
        client,
        Decimal::from(5000),
        fx.currency,
        CreditKind::Manual,
        "test",
        None,
    )
    .await
    .unwrap();
    drop(conn);

    // VPS needs configuration (hostname) before it reaches the cart.
    let res = add(&mut app, &[("product", "vps-test"), ("cycle", "monthly")]).await;
    assert!(res.location().contains("/cart/configure/vps-test"));
    let page = app.get("/cart/configure/vps-test?cycle=monthly").await;
    assert_eq!(page.status, StatusCode::OK);
    let quote = app
        .get("/cart/configure/vps-test/quote?cycle=monthly")
        .await;
    assert!(quote.body.contains("config-summary"));
    let bad = app
        .post(
            "/cart/configure/vps-test",
            &[
                ("csrf_token", &page.csrf()),
                ("cycle", "monthly"),
                ("hostname", "bad"),
            ],
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let ok = app
        .post(
            "/cart/configure/vps-test",
            &[
                ("csrf_token", &page.csrf()),
                ("cycle", "monthly"),
                ("hostname", "vps1.ornek-test.com"),
            ],
        )
        .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER, "{}", ok.body);
    billing_details(&mut app).await;
    let (res, _) = place(&mut app, "credit").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let invoice_id = InvoiceId::from_str(res.location().rsplit('/').next().unwrap()).unwrap();
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Paid, "paid with credit");

    run_worker(&app).await;
    let svc = services::client_services(app.pool(), app.tenant_id, client)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(svc.service.status, ServiceStatus::Active);
    let server = app
        .get(&format!("/client/services/{}/server", svc.service.id))
        .await;
    assert_eq!(server.status, StatusCode::OK, "{}", server.body);
    assert!(server.body.contains("vm-live"));
    let live = app
        .get(&format!(
            "/client/services/{}/server/live?h_cpu=1,2,3",
            svc.service.id
        ))
        .await;
    assert!(live.body.contains("<polyline class=\"vm-chart__line\""));
    let res = app
        .post(
            &format!("/client/services/{}/server/power", svc.service.id),
            &[("csrf_token", &server.csrf()), ("action", "reboot")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let flash_page = app
        .get(&format!("/client/services/{}/server", svc.service.id))
        .await;
    assert!(
        flash_page.body.contains("toast--success"),
        "power action succeeded"
    );

    // Ticket with an attachment, then a reply.
    let new = app.get("/client/tickets/new").await;
    let dept = new.body.split("name=\"department\"").nth(1).unwrap();
    let dept_id = dept.split("value=\"").nth(1).unwrap()[..36].to_owned();
    let res = multipart(
        &mut app,
        "/client/tickets",
        &[
            ("csrf_token", &new.csrf()),
            ("department", &dept_id),
            ("priority", "high"),
            ("subject", "Sunucu yavaş"),
            ("body", "Merhaba **yardım** <script>x</script>"),
        ],
        Some(("rapor.txt", b"log satiri\n")),
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let ticket_url = res.location().to_owned();
    let thread = app.get(&ticket_url).await;
    assert!(thread.body.contains("<strong>yardım</strong>"));
    assert!(!thread.body.contains("<script>x"), "raw HTML escaped");
    let link = thread.body.split("/attachments/").nth(1).unwrap()[..36].to_owned();
    let file = app.get(&format!("{ticket_url}/attachments/{link}")).await;
    assert_eq!(file.status, StatusCode::OK);
    assert!(
        file.headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(file.headers["x-content-type-options"], "nosniff");
    let res = multipart(
        &mut app,
        &format!("{ticket_url}/reply"),
        &[("csrf_token", &thread.csrf()), ("body", "Ek bilgi")],
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let bad_type = multipart(
        &mut app,
        &format!("{ticket_url}/reply"),
        &[("csrf_token", &thread.csrf()), ("body", "x")],
        Some(("x.html", b"<html><script>alert(1)</script>")),
    )
    .await;
    assert_eq!(bad_type.status, StatusCode::UNPROCESSABLE_ENTITY);
    app.cleanup().await;
}

#[tokio::test]
async fn other_clients_objects_are_404_and_csrf_is_enforced() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let fx = setup(&app).await;
    // Client B with a service, a domain, an invoice and a ticket.
    register(&mut app, "b@example.test").await;
    let b = client_of(&app, "b@example.test").await;
    let mut conn = app.pool().acquire().await.unwrap();
    payments::adjust_credit(
        &mut conn,
        app.tenant_id,
        b,
        Decimal::from(5000),
        fx.currency,
        CreditKind::Manual,
        "test",
        None,
    )
    .await
    .unwrap();
    drop(conn);
    order_hosting(&mut app, "b-musterisi-hos.com").await;
    let res = add(&mut app, &[("domain", "b-ikinci-alan-hos.com")]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let res = add(&mut app, &[("domain", "b-ikinci-alan-hos.com")]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "duplicate is a no-op");
    billing_details(&mut app).await;
    let (res, _) = place(&mut app, "credit").await;
    let b_invoice = res.location().rsplit('/').next().unwrap().to_owned();
    run_worker(&app).await;
    let b_service = services::client_services(app.pool(), app.tenant_id, b)
        .await
        .unwrap()
        .remove(0)
        .service
        .id;
    let b_domain = domains::client_domains(app.pool(), app.tenant_id, b)
        .await
        .unwrap()
        .remove(0)
        .id;
    let new = app.get("/client/tickets/new").await;
    let dept = new.body.split("name=\"department\"").nth(1).unwrap();
    let dept_id = dept.split("value=\"").nth(1).unwrap()[..36].to_owned();
    let res = multipart(
        &mut app,
        "/client/tickets",
        &[
            ("csrf_token", &new.csrf()),
            ("department", &dept_id),
            ("subject", "B"),
            ("body", "B"),
        ],
        Some(("b.txt", b"secret")),
    )
    .await;
    let b_ticket = res.location().to_owned();
    let thread = app.get(&b_ticket).await;
    let b_attachment = format!(
        "{b_ticket}/attachments/{}",
        &thread.body.split("/attachments/").nth(1).unwrap()[..36]
    );

    // Client A.
    app.cookies.clear();
    register(&mut app, "a@example.test").await;
    for path in [
        format!("/client/invoices/{b_invoice}"),
        format!("/client/invoices/{b_invoice}/print"),
        format!("/client/services/{b_service}"),
        format!("/client/services/{b_service}/server"),
        format!("/client/domains/{b_domain}"),
        format!("/client/domains/{b_domain}/dns"),
        b_ticket.clone(),
        b_attachment.clone(),
        "/client/invoices/not-a-uuid".to_owned(),
    ] {
        let res = app.get(&path).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
    let token = csrf(&mut app, "/client").await;
    let res = app
        .post(
            &format!("/client/invoices/{b_invoice}/pay"),
            &[("csrf_token", &token), ("method", "credit")],
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app
        .post(
            &format!("/client/domains/{b_domain}/lock"),
            &[("csrf_token", &token), ("enabled", "0")],
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // No CSRF token → 403 (forms and cart).
    let res = app
        .post(
            "/client/account",
            &[("first_name", "X"), ("last_name", "Y")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app.post("/cart/add", &[("product", "web-test")]).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = multipart(
        &mut app,
        "/client/tickets",
        &[("subject", "x"), ("body", "y")],
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // A's own pages render.
    for path in [
        "/client",
        "/client/services",
        "/client/domains",
        "/client/invoices",
        "/client/billing",
        "/client/billing/transactions",
        "/client/orders",
        "/client/tickets",
        "/client/tickets/new",
        "/client/account",
        "/client/account/security",
        "/client/account/billing",
        "/client/account/users",
        "/client/account/contacts",
        "/client/account/api-keys",
        "/client/account/notifications",
        "/client/account/activity",
        "/client/account/privacy",
        "/en/client",
        "/en/cart",
    ] {
        let res = app.get(path).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "{path}: {}",
            res.body.chars().take(400).collect::<String>()
        );
    }
    let export = app.get("/client/account/privacy/export").await;
    assert!(
        export.headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains("attachment")
    );
    let _ = Utc::now();
    app.cleanup().await;
}

/// Sends a multipart form (optionally with one file) with the cookie jar.
async fn multipart(
    app: &mut TestApp,
    path: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &[u8])>,
) -> common::Res {
    let boundary = "----hosTestBoundary7MA4YWxk";
    let mut body: Vec<u8> = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    if let Some((name, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let cookie = app
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", app.host.clone())
        .header("cookie", cookie)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    common::Res {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}
