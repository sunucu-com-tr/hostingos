//! REST API: authentication, client scoping, permissions, idempotent order
//! creation, pagination / sorting, rate-limit headers and the OpenAPI
//! document.

use crate::common::TestApp;
use axum::body::Body;
use hos_core::domain::{Locale, ProductType, UserKind};
use hos_core::ids::{ClientId, ProductId, UserId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::catalog::{self, ProductInput};
use hos_db::repo::clients::{self, ClientInput};
use hos_db::repo::identity::{self, NewUser};
use http::{HeaderMap, Request, StatusCode};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

async fn call(
    app: &TestApp,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", &app.host);
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app
        .router
        .clone()
        .oneshot(b.body(body).expect("request"))
        .await
        .expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Reply {
        status,
        headers,
        body,
    }
}

async fn key_for(app: &TestApp, user: UserId, scopes: &[&str]) -> String {
    let g = hos_auth::tokens::generate_api_key();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
    identity::create_api_key(
        app.pool(),
        app.tenant_id,
        Some(user),
        "test key",
        &g.lookup,
        &g.key_hash,
        &scopes,
        None,
    )
    .await
    .expect("api key");
    g.key.expose_str().to_owned()
}

/// Owner staff user → API key (optionally scoped).
async fn staff_key(app: &TestApp, scopes: &[&str]) -> String {
    let email = format!("staff-{}@example.com", uuid::Uuid::now_v7().simple());
    assert!(
        hos_web::cli::create_staff(
            app.pool(),
            app.tenant_id,
            &hos_web::cli::StaffSpec {
                email: &email,
                password: "Sufficiently-long-pass-42",
                role: "owner",
                first_name: "Api",
                last_name: "Staff",
            },
        )
        .await
        .expect("staff")
    );
    let user = identity::user_by_email(app.pool(), app.tenant_id, &email)
        .await
        .expect("query")
        .expect("user");
    key_for(app, user.id, scopes).await
}

/// A client with a login user → (client id, API key).
async fn client_key(app: &TestApp, name: &str) -> (ClientId, String) {
    let email = format!("{name}-{}@example.com", uuid::Uuid::now_v7().simple());
    let user = identity::create_user(
        app.pool(),
        app.tenant_id,
        &NewUser {
            email: email.clone(),
            password_hash: None,
            kind: UserKind::Client,
            first_name: name.to_owned(),
            last_name: "Client".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    let mut conn = app.pool().acquire().await.expect("conn");
    let client = clients::create_client_with_owner(
        &mut conn,
        app.tenant_id,
        &ClientInput::individual(name, "Client", &email, Currency::TRY),
        user.id,
    )
    .await
    .expect("client");
    drop(conn);
    (client.id, key_for(app, user.id, &[]).await)
}

async fn product(app: &TestApp) -> ProductId {
    let slug = format!("api-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
    let p = catalog::create_product(
        app.pool(),
        app.tenant_id,
        &ProductInput::new(&slug, "API Test Product", ProductType::Other, None),
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

#[tokio::test]
async fn missing_or_bad_credentials_are_401_problems() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let r = call(&app, "GET", "/api/v1/invoices", None, None, &[]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/problem+json")
    );
    assert_eq!(r.body["status"], 401);
    let r = call(
        &app,
        "GET",
        "/api/v1/me",
        Some("hos_live_bogus_key"),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Public endpoints work without a key and report the rate limit.
    let r = call(&app, "GET", "/api/v1/catalog", None, None, &[]).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.body["currency"], "TRY");
    assert!(r.headers.contains_key("ratelimit-limit"));
    assert!(r.headers.contains_key("ratelimit-remaining"));
    app.cleanup().await;
}

#[tokio::test]
async fn client_keys_only_see_their_own_records() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (alice, alice_key) = client_key(&app, "alice").await;
    let (bob, _) = client_key(&app, "bob").await;
    let billing = &app.state.billing;
    let own = billing
        .order_add_funds(app.tenant_id, alice, Decimal::from(50))
        .await
        .expect("own invoice");
    let foreign = billing
        .order_add_funds(app.tenant_id, bob, Decimal::from(70))
        .await
        .expect("foreign invoice");

    let r = call(
        &app,
        "GET",
        &format!("/api/v1/invoices/{}", foreign.id),
        Some(&alice_key),
        None,
        &[],
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "another client's invoice is invisible"
    );
    let r = call(
        &app,
        "GET",
        &format!("/api/v1/invoices/{}", own.id),
        Some(&alice_key),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["total"], "50.00");
    assert_eq!(r.body["currency"], "TRY");

    // Lists are scoped even when another client_id is requested.
    let r = call(
        &app,
        "GET",
        &format!("/api/v1/invoices?client_id={bob}"),
        Some(&alice_key),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let ids: Vec<&str> = r.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [own.id.to_string()]);

    let r = call(&app, "GET", "/api/v1/me", Some(&alice_key), None, &[]).await;
    assert_eq!(r.body["kind"], "client");
    assert_eq!(r.body["client_id"], alice.to_string());
    // Staff-only endpoints are 403 for client keys.
    let r = call(
        &app,
        "GET",
        "/api/v1/integrations",
        Some(&alice_key),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    app.cleanup().await;
}

#[tokio::test]
async fn staff_key_without_permission_is_403() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let support_only = staff_key(&app, &["support.view"]).await;
    let r = call(
        &app,
        "GET",
        "/api/v1/invoices",
        Some(&support_only),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.body);
    assert_eq!(r.body["code"], "forbidden");
    let r = call(
        &app,
        "GET",
        "/api/v1/tickets",
        Some(&support_only),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let r = call(&app, "GET", "/api/v1/me", Some(&support_only), None, &[]).await;
    assert_eq!(r.body["permissions"], json!(["support.view"]));
    app.cleanup().await;
}

#[tokio::test]
async fn order_creation_is_idempotent() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let staff = staff_key(&app, &[]).await;
    let (client, _) = client_key(&app, "buyer").await;
    let pid = product(&app).await;
    let order = json!({
        "client_id": client,
        "items": [{ "product_id": pid, "billing_cycle": "monthly" }],
    });
    let key = uuid::Uuid::now_v7().to_string();
    let first = call(
        &app,
        "POST",
        "/api/v1/orders",
        Some(&staff),
        Some(order.clone()),
        &[("idempotency-key", &key)],
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
    assert_eq!(first.body["quote"]["subtotal"], "49.90", "{:?}", first.body);
    assert_eq!(first.body["currency"], "TRY");
    assert_eq!(first.body["service_ids"].as_array().unwrap().len(), 1);
    let second = call(
        &app,
        "POST",
        "/api/v1/orders",
        Some(&staff),
        Some(order.clone()),
        &[("idempotency-key", &key)],
    )
    .await;
    assert_eq!(second.status, StatusCode::CREATED);
    assert_eq!(second.body, first.body, "the stored response is replayed");
    assert_eq!(second.headers["idempotent-replayed"], "true");
    let r = call(
        &app,
        "GET",
        &format!("/api/v1/orders?client_id={client}"),
        Some(&staff),
        None,
        &[],
    )
    .await;
    assert_eq!(
        r.body["pagination"]["total"], 1,
        "exactly one order: {:?}",
        r.body
    );

    // Same key, different body → 422.
    let other = json!({ "client_id": client, "items": [{ "product_id": pid, "billing_cycle": "monthly", "quantity": 2 }] });
    let r = call(
        &app,
        "POST",
        "/api/v1/orders",
        Some(&staff),
        Some(other),
        &[("idempotency-key", &key)],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["code"], "idempotency_key_reused");

    // Field-level validation.
    let r = call(
        &app,
        "POST",
        "/api/v1/orders",
        Some(&staff),
        Some(json!({ "items": [{ "product_id": pid }] })),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let fields: Vec<&str> = r.body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["items[0].billing_cycle"]);

    // The client's view of the new order and invoice.
    let order_id = first.body["order_id"].as_str().unwrap();
    let r = call(
        &app,
        "GET",
        &format!("/api/v1/orders/{order_id}"),
        Some(&staff),
        None,
        &[],
    )
    .await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 1);
    app.cleanup().await;
}

#[tokio::test]
async fn lists_paginate_sort_and_validate() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let staff = staff_key(&app, &[]).await;
    for name in ["carol", "alice", "bob"] {
        clients::create_client(
            app.pool(),
            app.tenant_id,
            &ClientInput::individual(
                name,
                "List",
                &format!("{name}-{}@example.com", app.slug),
                Currency::TRY,
            ),
        )
        .await
        .expect("client");
    }
    let r = call(
        &app,
        "GET",
        "/api/v1/clients?limit=2&sort=email",
        Some(&staff),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let emails: Vec<&str> = r.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["email"].as_str().unwrap())
        .collect();
    assert_eq!(emails.len(), 2);
    assert!(
        emails[0].starts_with("alice") && emails[1].starts_with("bob"),
        "{emails:?}"
    );
    assert_eq!(r.body["pagination"]["total"], 3);
    assert_eq!(r.body["pagination"]["has_more"], true);
    assert_eq!(r.body["pagination"]["next_offset"], 2);
    let r = call(
        &app,
        "GET",
        "/api/v1/clients?limit=2&offset=2&sort=email",
        Some(&staff),
        None,
        &[],
    )
    .await;
    let page2 = r.body["data"].as_array().unwrap();
    assert_eq!(page2.len(), 1);
    assert!(page2[0]["email"].as_str().unwrap().starts_with("carol"));
    assert_eq!(r.body["pagination"]["has_more"], false);
    let r = call(
        &app,
        "GET",
        "/api/v1/clients?q=bob",
        Some(&staff),
        None,
        &[],
    )
    .await;
    assert_eq!(r.body["pagination"]["total"], 1);

    let r = call(
        &app,
        "GET",
        "/api/v1/clients?limit=1000&sort=password",
        Some(&staff),
        None,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let fields: Vec<&str> = r.body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["limit", "sort"]);

    // Create + patch through the API.
    let r = call(
        &app,
        "POST",
        "/api/v1/clients",
        Some(&staff),
        Some(json!({ "first_name": "Dave", "last_name": "Api", "email": "not-an-email" })),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["errors"][0]["field"], "email");
    let r = call(&app, "POST", "/api/v1/clients", Some(&staff), Some(json!({ "first_name": "Dave", "last_name": "Api", "email": format!("dave-{}@example.com", app.slug) })), &[]).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let id = r.body["id"].as_str().unwrap().to_owned();
    let r = call(
        &app,
        "PATCH",
        &format!("/api/v1/clients/{id}"),
        Some(&staff),
        Some(json!({ "city": "İzmir" })),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["city"], "İzmir");
    assert_eq!(r.body["first_name"], "Dave");
    app.cleanup().await;
}

/// Every documented operation enforces the authentication it declares:
/// `Public` ones answer an anonymous caller (anything but 401), all others
/// refuse it with 401.
#[tokio::test]
async fn openapi_auth_matches_what_is_enforced() {
    use hos_web::routes::api::openapi::{Access, OPERATIONS};
    let Some(app) = TestApp::new().await else {
        return;
    };
    // An anonymous browser: its `hos_anon` cookie and CSRF token, so unsafe
    // methods reach the API gate instead of stopping at the CSRF check.
    let page = call(&app, "GET", "/auth/login", None, None, &[]).await;
    let anon = page
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|c| c.starts_with("hos_anon="))
        .and_then(|c| c.split(';').next())
        .expect("anonymous cookie")
        .to_owned();
    let html = page.body.as_str().unwrap_or_default().to_owned();
    let (_, client) = client_key(&app, "openapi").await;
    let mut keys: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let csrf = html
        .split("name=\"csrf-token\" content=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .expect("csrf meta")
        .to_owned();
    let mut wrong = Vec::new();
    for o in OPERATIONS {
        let path = o
            .path
            .split('/')
            .map(|seg| {
                if seg.starts_with('{') {
                    uuid::Uuid::now_v7().to_string()
                } else {
                    seg.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let unsafe_method = matches!(o.method, "POST" | "PATCH" | "PUT" | "DELETE");
        let body = unsafe_method.then(|| json!({}));
        let headers: Vec<(&str, &str)> = if unsafe_method {
            vec![("cookie", anon.as_str()), ("x-csrf-token", csrf.as_str())]
        } else {
            Vec::new()
        };
        let r = call(&app, o.method, &path, None, body.clone(), &headers).await;
        let anonymous_ok = r.status != StatusCode::UNAUTHORIZED;
        if anonymous_ok != matches!(o.access, Access::Public) {
            wrong.push(format!(
                "{} {} declared {:?}, anonymous call answered {}",
                o.method, o.path, o.access, r.status
            ));
        }
        // A client account owner's key: refused on staff-only operations,
        // never with 401 / a staff-only 403 elsewhere.
        let r = call(&app, o.method, &path, Some(&client), body.clone(), &[]).await;
        let refused = r.status == StatusCode::UNAUTHORIZED
            || (r.status == StatusCode::FORBIDDEN
                && r.body.to_string().to_ascii_lowercase().contains("staff"));
        let staff_only = matches!(o.access, Access::Staff(_));
        if staff_only && !(r.status == StatusCode::FORBIDDEN || r.status == StatusCode::NOT_FOUND) {
            wrong.push(format!(
                "{} {} declared {:?}, a client key got {}",
                o.method, o.path, o.access, r.status
            ));
        }
        if !staff_only && refused {
            wrong.push(format!(
                "{} {} declared {:?}, a client key was refused: {} {}",
                o.method, o.path, o.access, r.status, r.body
            ));
        }
        // A staff key holding exactly the declared permission is enough.
        if let Access::Scoped(p) | Access::Staff(p) = o.access {
            if !keys.contains_key(p) {
                keys.insert(p, staff_key(&app, &[p]).await);
            }
            let r = call(&app, o.method, &path, Some(&keys[p]), body, &[]).await;
            // A second permission the summary names (quote acceptance also
            // needs `billing.manage`) is declared too.
            let detail = r.body["detail"].as_str().unwrap_or_default();
            let extra = detail
                .strip_prefix("Missing permission `")
                .and_then(|d| d.split('`').next())
                .is_some_and(|q| o.summary.contains(q));
            if r.status == StatusCode::UNAUTHORIZED
                || (r.status == StatusCode::FORBIDDEN
                    && detail.contains("Missing permission")
                    && !extra)
            {
                wrong.push(format!(
                    "{} {} declared {:?}, a key with it got {} {}",
                    o.method, o.path, o.access, r.status, r.body
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    app.cleanup().await;
}

#[tokio::test]
async fn openapi_document_and_reference_page() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let r = call(&app, "GET", "/api/v1/openapi.json", None, None, &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["openapi"], "3.1.0");
    let paths = r.body["paths"].as_object().unwrap();
    for (method, path) in hos_web::routes::api::route_table() {
        assert!(
            paths
                .get(path)
                .and_then(|p| p.get(method.to_ascii_lowercase()))
                .is_some(),
            "{method} {path} undocumented"
        );
    }
    let docs = app
        .router
        .clone()
        .oneshot(Request::get("/api/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(docs.status(), StatusCode::OK);
    let html = axum::body::to_bytes(docs.into_body(), 4 << 20)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&html);
    assert!(html.contains("/api/v1/orders"));
    assert!(
        !html.contains("<script"),
        "self-contained, script-free page"
    );
    app.cleanup().await;
}
