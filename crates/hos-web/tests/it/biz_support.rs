//! Helpers of the business-feature tests (quotes, affiliates, network
//! status, mass pay, tasks, fraud, downloads, inbound e-mail).

use crate::common::{Res, TestApp};
use chrono::Days;
use hos_billing::payments::ManualPayment;
use hos_billing::quotes::{QuoteDraft, QuoteLine};
use hos_core::domain::{Locale, ProductType, UserKind};
use hos_core::ids::{ClientId, InvoiceId, ProductId, UserId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::catalog::{self, ProductInput};
use hos_db::repo::clients::{self, ClientInput};
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::invoices;
use hos_db::repo::payments::TransactionRow;
use http::StatusCode;
use rust_decimal::Decimal;

pub const PASSWORD: &str = "Biz-Feature-Pass-2026!";

/// A user of `kind` (staff with role `role`).
pub async fn user(app: &TestApp, email: &str, kind: UserKind, role: Option<&str>) -> UserId {
    let u = identity::create_user(
        app.pool(),
        app.tenant_id,
        &NewUser {
            email: email.into(),
            password_hash: Some(hos_auth::password::hash_password(PASSWORD).expect("hash")),
            kind,
            first_name: "Deneme".into(),
            last_name: "Kullanıcı".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    if let Some(slug) = role {
        let r = identity::role_by_slug(app.pool(), app.tenant_id, slug)
            .await
            .expect("role")
            .expect("role exists");
        identity::assign_role(app.pool(), app.tenant_id, u.id, r.id)
            .await
            .expect("assign");
    }
    u.id
}

/// Signs `email` in (fresh cookie jar).
pub async fn login(app: &mut TestApp, email: &str) {
    app.cookies.clear();
    let token = app.get("/auth/login").await.csrf();
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
    assert_eq!(r.status, StatusCode::SEE_OTHER, "login {email}: {}", r.body);
}

/// A fresh staff member with `role`, signed in; returns the CSRF token.
pub async fn staff(app: &mut TestApp, role: &str) -> String {
    let email = format!("{role}-{}@staff.test", uuid::Uuid::now_v7().simple());
    user(app, &email, UserKind::Staff, Some(role)).await;
    login(app, &email).await;
    let dash = app.get("/admin").await;
    assert_eq!(dash.status, StatusCode::OK, "{}", dash.body);
    dash.csrf()
}

/// A client account owned by a new client user: `(user, client, e-mail)`.
pub async fn owner(app: &TestApp, label: &str) -> (UserId, ClientId, String) {
    let email = format!("{label}-{}@client.test", uuid::Uuid::now_v7().simple());
    owner_with_email(app, label, &email).await
}

/// A client account owned by a new client user with `email`.
pub async fn owner_with_email(
    app: &TestApp,
    label: &str,
    email: &str,
) -> (UserId, ClientId, String) {
    let email = email.to_owned();
    let uid = user(app, &email, UserKind::Client, None).await;
    let mut conn = app.pool().acquire().await.expect("conn");
    let client = clients::create_client_with_owner(
        &mut conn,
        app.tenant_id,
        &ClientInput::individual("Deneme", label, &email, Currency::TRY),
        uid,
    )
    .await
    .expect("client");
    (uid, client.id, email)
}

/// A sub-user of `client` with `areas`; returns its e-mail.
pub async fn sub_user(app: &TestApp, client: ClientId, areas: &[&str]) -> String {
    let email = format!("sub-{}@client.test", uuid::Uuid::now_v7().simple());
    let id = user(app, &email, UserKind::Client, None).await;
    let areas: Vec<String> = areas.iter().map(|a| (*a).to_owned()).collect();
    clients::link_user(app.pool(), app.tenant_id, client, id, false, &areas)
        .await
        .expect("member");
    email
}

/// `POST` with a CSRF token prepended.
pub async fn post(app: &mut TestApp, path: &str, csrf: &str, fields: &[(&str, &str)]) -> Res {
    let mut all = vec![("csrf_token", csrf)];
    all.extend_from_slice(fields);
    app.post(path, &all).await
}

/// CSRF token of a page the current session can open.
pub async fn csrf_of(app: &mut TestApp, path: &str) -> String {
    let r = app.get(path).await;
    assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.body);
    r.csrf()
}

/// A monthly product (49.90 TRY) without a module.
pub async fn product(app: &TestApp) -> ProductId {
    let slug = format!("aff-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
    let p = catalog::create_product(
        app.pool(),
        app.tenant_id,
        &ProductInput::new(&slug, "Ortaklık Test Ürünü", ProductType::Other, None),
    )
    .await
    .expect("product");
    catalog::set_product_price(
        app.pool(),
        app.tenant_id,
        p.id,
        Currency::TRY,
        BillingCycle::Monthly,
        Decimal::new(4990, 2),
        Decimal::ZERO,
    )
    .await
    .expect("price");
    p.id
}

/// Orders `product` for `client` through an accepted quote and pays the
/// invoice by bank transfer; returns `(invoice, payment transaction)`.
pub async fn paid_order(
    app: &TestApp,
    client: ClientId,
    product: ProductId,
) -> (InvoiceId, TransactionRow) {
    let billing = &app.state.billing;
    let inv = unpaid_order(app, client, product).await;
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, inv)
        .await
        .unwrap()
        .unwrap();
    let out = billing
        .record_manual_payment(
            app.tenant_id,
            &ManualPayment {
                invoice_id: inv.id,
                amount: inv.balance,
                integration_id: None,
                method: Some("bank_transfer".into()),
                reference: Some(format!("AFF-{}", uuid::Uuid::now_v7().simple())),
                occurred_at: None,
                note: None,
                staff_user_id: None,
            },
        )
        .await
        .expect("payment");
    assert!(out.invoice_paid);
    (inv.id, out.transaction)
}

/// Orders `product` for `client` through an accepted quote; returns the
/// unpaid invoice (49.90 TRY).
pub async fn unpaid_order(app: &TestApp, client: ClientId, product: ProductId) -> InvoiceId {
    let billing = &app.state.billing;
    let q = billing
        .save_quote(
            app.tenant_id,
            None,
            &QuoteDraft {
                client_id: Some(client),
                prospect_name: None,
                prospect_email: None,
                prospect_company: None,
                subject: "Hosting".into(),
                currency: Currency::TRY,
                valid_until: chrono::Utc::now().date_naive() + Days::new(10),
                notes: None,
                admin_notes: None,
                lines: vec![QuoteLine {
                    product_id: Some(product),
                    billing_cycle: Some(BillingCycle::Monthly),
                    description: "Hosting".into(),
                    quantity: Decimal::ONE,
                    unit_price: Decimal::new(4990, 2),
                    discount_percent: Decimal::ZERO,
                    taxable: false,
                }],
            },
            None,
        )
        .await
        .expect("quote");
    billing.send_quote(app.tenant_id, q.id).await.expect("send");
    let acc = billing
        .accept_quote(app.tenant_id, q.id, client, None, None)
        .await
        .expect("accept");
    acc.invoice_id
}

/// An API key of `user`.
pub async fn api_key(app: &TestApp, user: UserId) -> String {
    let g = hos_auth::tokens::generate_api_key();
    identity::create_api_key(
        app.pool(),
        app.tenant_id,
        Some(user),
        "biz test",
        &g.lookup,
        &g.key_hash,
        &[],
        None,
    )
    .await
    .expect("api key");
    g.key.expose_str().to_owned()
}

/// A JSON API call: `(status, body)`.
pub async fn api(
    app: &TestApp,
    method: &str,
    path: &str,
    key: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let mut b = http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", &app.host)
        .header("authorization", format!("Bearer {key}"));
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            axum::body::Body::from(v.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let res = app
        .router
        .clone()
        .oneshot(b.body(body).expect("request"))
        .await
        .expect("infallible");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// An active service of a new product for `client` (ordered and paid).
pub async fn active_service(app: &TestApp, client: ClientId) -> hos_core::ids::ServiceId {
    let product = product(app).await;
    active_service_of(app, client, product).await
}

/// An active service of `product` for `client` (ordered and paid).
pub async fn active_service_of(
    app: &TestApp,
    client: ClientId,
    product: ProductId,
) -> hos_core::ids::ServiceId {
    let (invoice, _) = paid_order(app, client, product).await;
    let id: hos_core::ids::ServiceId = sqlx::query_scalar(
        "update services s set status = 'active', next_due_date = current_date + 15
          where s.tenant_id = $1 and s.client_id = $2 and s.product_id = $3
            and s.id = (select max(s2.id::text)::uuid from services s2
                         where s2.tenant_id = $1 and s2.client_id = $2 and s2.product_id = $3)
          returning id",
    )
    .bind(app.tenant_id)
    .bind(client)
    .bind(product)
    .fetch_one(app.pool())
    .await
    .unwrap_or_else(|e| panic!("service for invoice {invoice}: {e}"));
    id
}

/// `multipart/form-data` POST (CSRF token as a field when given) with an
/// optional file part `(field, filename, bytes)`.
pub async fn multipart(
    app: &mut TestApp,
    path: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &[u8])>,
) -> Res {
    let boundary = "----hosBizBoundary4kQ9";
    let mut body: Vec<u8> = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    if let Some((field, name, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
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
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri(path)
        .header("host", app.host.clone())
        .header("cookie", cookie)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(axum::body::Body::from(body))
        .expect("request");
    let res = tower::ServiceExt::oneshot(app.router.clone(), req)
        .await
        .expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    Res {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}
