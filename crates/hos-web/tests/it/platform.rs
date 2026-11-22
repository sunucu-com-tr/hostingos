//! Platform console: TOTP sign-in, tenant creation from the panel (storefront
//! answers on `<slug>.<base>`), suspension, tenant isolation of API keys,
//! self-service sign-up and the TLS allow-list.

use crate::common::{self, TestApp};
use axum::body::Body;
use hos_auth::totp::{TotpSecret, code_at};
use hos_core::domain::TenantStatus;
use hos_db::repo::{identity, platform, saas};
use hos_web::routes::platform::{login, signup};
use http::{Method, Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

/// Host of the platform console (`HOS_BASE_URL`).
fn console_host(app: &TestApp) -> String {
    let u = &app.state.config.base_url;
    match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or("localhost")),
        None => u.host_str().unwrap_or("localhost").to_owned(),
    }
}

async fn get(app: &mut TestApp, host: &str, path: &str) -> common::Res {
    app.send_to(host, Method::GET, path, None, &[]).await
}

async fn post(app: &mut TestApp, host: &str, path: &str, form: &[(&str, &str)]) -> common::Res {
    app.send_to(host, Method::POST, path, Some(form), &[]).await
}

/// A platform admin with TOTP enabled; returns (email, password, secret).
async fn admin_with_totp(app: &TestApp) -> (String, String, TotpSecret) {
    let email = format!("ops-{}@example.com", uuid::Uuid::now_v7().simple());
    let password = "Platform-Test-Password-91".to_owned();
    assert!(
        hos_web::cli::create_platform_admin(app.pool(), &email, "Ops Test", &password)
            .await
            .expect("admin")
    );
    let admin = platform::platform_admin_by_email(app.pool(), &email)
        .await
        .expect("query")
        .expect("admin");
    let secret = TotpSecret::generate();
    let enc = app
        .state
        .secret_box
        .encrypt(secret.as_bytes(), &login::totp_aad(admin.id))
        .expect("encrypt");
    saas::set_platform_admin_totp(app.pool(), admin.id, Some(&enc), true, &[])
        .await
        .expect("totp");
    (email, password, secret)
}

async fn drop_admin(app: &TestApp, email: &str) {
    sqlx::query("delete from platform_admins where email = $1")
        .bind(email)
        .execute(app.pool())
        .await
        .expect("delete admin");
}

/// Signs in through password + TOTP; leaves the session in the cookie jar.
async fn sign_in(app: &mut TestApp, host: &str, email: &str, password: &str, secret: &TotpSecret) {
    let page = get(app, host, "/platform/login").await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = page.csrf();
    let r = post(
        app,
        host,
        "/platform/login",
        &[
            ("csrf_token", &csrf),
            ("email", email),
            ("password", password),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/platform/login/2fa");

    // Without the second factor the console stays closed.
    let r = get(app, host, "/platform").await;
    assert!(r.status.is_redirection(), "console needs the second factor");

    let page = get(app, host, "/platform/login/2fa").await;
    let csrf = page.csrf();
    let wrong = post(
        app,
        host,
        "/platform/login/2fa",
        &[("csrf_token", &csrf), ("code", "000000")],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);

    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap();
    let code = code_at(secret, now).expect("code");
    let page = get(app, host, "/platform/login/2fa").await;
    let csrf = page.csrf();
    let r = post(
        app,
        host,
        "/platform/login/2fa",
        &[("csrf_token", &csrf), ("code", &code)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/platform");
    let r = get(app, host, "/platform").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

#[tokio::test]
async fn totp_login_tenant_wizard_suspension_and_isolation() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let host = console_host(&app);
    let (email, password, secret) = admin_with_totp(&app).await;
    sign_in(&mut app, &host, &email, &password, &secret).await;

    // Create a tenant through the wizard.
    let slug = format!("pt-{}", &uuid::Uuid::now_v7().simple().to_string()[22..]);
    let page = get(&mut app, &host, "/platform/tenants/new").await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = page.csrf();
    let owner = format!("owner-{slug}@example.com");
    let r = post(
        &mut app,
        &host,
        "/platform/tenants",
        &[
            ("csrf_token", &csrf),
            ("name", "Panel Test Hosting"),
            ("slug", &slug),
            ("plan", ""),
            ("billing_cycle", "monthly"),
            ("trial_days", "14"),
            ("owner_email", &owner),
            ("owner_first_name", "Owner"),
            ("owner_last_name", "Test"),
            ("custom_domain", ""),
            ("locale", "tr"),
            ("currency", "TRY"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let tenant = platform::get_tenant_by_slug(app.pool(), &slug)
        .await
        .expect("query")
        .expect("tenant provisioned");
    assert_eq!(tenant.status, TenantStatus::Trial);
    let owner_user = identity::user_by_email(app.pool(), tenant.id, &owner)
        .await
        .expect("query");
    assert!(owner_user.is_some(), "owner account created");

    // The storefront answers on <slug>.<base domain>.
    let tenant_host = format!("{slug}.{}", app.state.config.base_domain);
    let store = get(&mut app, &tenant_host, "/").await;
    assert_eq!(store.status, StatusCode::OK);

    // Tenant isolation: a key of the test tenant is unknown on the new host.
    let g = hos_auth::tokens::generate_api_key();
    let staff = identity::create_user(
        app.pool(),
        app.tenant_id,
        &identity::NewUser {
            email: format!("iso-{slug}@example.com"),
            password_hash: None,
            kind: hos_core::domain::UserKind::Staff,
            first_name: "Iso".into(),
            last_name: "Lation".into(),
            phone: None,
            locale: hos_core::domain::Locale::Tr,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    identity::create_api_key(
        app.pool(),
        app.tenant_id,
        Some(staff.id),
        "iso",
        &g.lookup,
        &g.key_hash,
        &[],
        None,
    )
    .await
    .expect("key");
    let bearer = format!("Bearer {}", g.key.expose_str());
    let own = app
        .send_to(
            &app.host.clone(),
            Method::GET,
            "/api/v1/me",
            None,
            &[("authorization", &bearer)],
        )
        .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    let foreign = app
        .send_to(
            &tenant_host,
            Method::GET,
            "/api/v1/me",
            None,
            &[("authorization", &bearer)],
        )
        .await;
    assert_eq!(foreign.status, StatusCode::UNAUTHORIZED);

    // Suspend → info page.
    let detail = get(&mut app, &host, &format!("/platform/tenants/{}", tenant.id)).await;
    assert_eq!(detail.status, StatusCode::OK);
    let csrf = detail.csrf();
    let r = post(
        &mut app,
        &host,
        &format!("/platform/tenants/{}/status", tenant.id),
        &[
            ("csrf_token", &csrf),
            ("action", "suspend"),
            ("reason", "test"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let store = get(&mut app, &tenant_host, "/").await;
    assert_eq!(store.status, StatusCode::SERVICE_UNAVAILABLE);

    // TLS allow-list: only known hosts.
    let ok = get(
        &mut app,
        "127.0.0.1",
        &format!("/internal/tls-allow?domain={tenant_host}"),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
    let unknown = get(
        &mut app,
        "127.0.0.1",
        "/internal/tls-allow?domain=unknown.example.org",
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    // Delete (suspended first, slug typed).
    let detail = get(&mut app, &host, &format!("/platform/tenants/{}", tenant.id)).await;
    let csrf = detail.csrf();
    let r = post(
        &mut app,
        &host,
        &format!("/platform/tenants/{}/delete", tenant.id),
        &[("csrf_token", &csrf), ("confirm_slug", &slug)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        platform::get_tenant(app.pool(), tenant.id)
            .await
            .expect("query")
            .is_none()
    );

    drop_admin(&app, &email).await;
    app.cleanup().await;
}

#[tokio::test]
async fn self_service_signup_provisions_after_email_verification() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let host = console_host(&app);
    let previous = saas::get_platform_setting(app.pool(), "signup.enabled")
        .await
        .expect("setting");
    saas::set_platform_setting(app.pool(), "signup.enabled", &json!(true))
        .await
        .expect("enable signup");

    let slug = format!("su-{}", &uuid::Uuid::now_v7().simple().to_string()[22..]);
    let email = format!("founder-{slug}@example.com");

    // Slug availability (JSON) and reserved names.
    let r = get(
        &mut app,
        &host,
        &format!("/platform/signup/slug?slug={slug}"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("\"available\":true"), "{}", r.body);
    let r = get(&mut app, &host, "/platform/signup/slug?slug=admin").await;
    assert!(r.body.contains("\"available\":false"), "{}", r.body);

    // Honeypot: looks accepted, stores nothing.
    let page = get(&mut app, &host, "/platform/signup").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let csrf = page.csrf();
    let bot_slug = format!("{slug}x");
    let r = post(
        &mut app,
        &host,
        "/platform/signup",
        &[
            ("csrf_token", &csrf),
            ("company", "Bot Inc"),
            ("slug", &bot_slug),
            ("plan", ""),
            ("name", "Bot"),
            ("email", "bot@example.com"),
            ("password", "Bot-password-123456"),
            ("terms", "1"),
            ("website", "http://spam.example"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let bots: (i64,) = sqlx::query_as("select count(*) from platform_signups where slug = $1")
        .bind(&bot_slug)
        .fetch_one(app.pool())
        .await
        .expect("count");
    assert_eq!(bots.0, 0);

    let r = post(
        &mut app,
        &host,
        "/platform/signup",
        &[
            ("csrf_token", &csrf),
            ("company", "Founder Hosting"),
            ("slug", &slug),
            ("plan", ""),
            ("name", "Ada Founder"),
            ("email", &email),
            ("password", "Kx9-violet-Harbor-77"),
            ("terms", "1"),
            ("website", ""),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/platform/signup/sent");
    // Nothing is provisioned before the e-mail is confirmed.
    assert!(
        platform::get_tenant_by_slug(app.pool(), &slug)
            .await
            .expect("query")
            .is_none()
    );

    // The link went to the mailer; plant a known token instead.
    let token = "test-signup-token-0123456789abcdef";
    sqlx::query("update platform_signups set token_hash = $1 where email = $2 and slug = $3")
        .bind(signup::token_hash(token))
        .bind(&email)
        .bind(&slug)
        .execute(app.pool())
        .await
        .expect("plant token");
    let r = get(
        &mut app,
        &host,
        &format!("/platform/signup/verify?token={token}"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let tenant = platform::get_tenant_by_slug(app.pool(), &slug)
        .await
        .expect("query")
        .expect("tenant created");
    let owner = identity::user_by_email(app.pool(), tenant.id, &email)
        .await
        .expect("query")
        .expect("owner");
    assert!(owner.password_hash.is_some(), "the chosen password is kept");
    // A second click does not provision twice.
    let again = get(
        &mut app,
        &host,
        &format!("/platform/signup/verify?token={token}"),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);

    let tenant_host = format!("{slug}.{}", app.state.config.base_domain);
    let store = get(&mut app, &tenant_host, "/").await;
    assert_eq!(store.status, StatusCode::OK);

    platform::delete_tenant(app.pool(), tenant.id)
        .await
        .expect("delete tenant");
    sqlx::query("delete from platform_signups where email = $1")
        .bind(&email)
        .execute(app.pool())
        .await
        .expect("cleanup signup");
    saas::set_platform_setting(
        app.pool(),
        "signup.enabled",
        &previous.unwrap_or(json!(false)),
    )
    .await
    .expect("restore");
    app.cleanup().await;
}

#[tokio::test]
async fn console_requires_login_and_tls_ask_rejects_remote_callers() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let res = app
        .router
        .clone()
        .oneshot(
            Request::get("/platform/tenants")
                .header("host", console_host(&app))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_redirection());
    // A proxied (non-loopback) caller without the shared secret is refused.
    let res = app
        .router
        .clone()
        .oneshot(
            Request::get(format!("/internal/tls-allow?domain={}", app.host))
                .header("x-forwarded-for", "203.0.113.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    app.cleanup().await;
}
