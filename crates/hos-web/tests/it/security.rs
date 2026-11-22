//! Regression tests of the security review fixes: e-mail verification and
//! account pre-hijacking (H1), client sub-user permissions outside `/client`
//! (H2), open redirects through the locale prefix (M1), `Host` confusion
//! (F3), impersonated 2FA changes (L1), API access while 2FA setup is
//! pending (L2), the platform host / dev tenant fallback (L3), the trusted
//! `X-Forwarded-For` hop (L4), CSRF for cookie-authenticated API calls (L6)
//! and internal-note attachments (F4). Each test uses its own throw-away
//! tenant (see `common`).

use crate::common::{self, Res, TestApp};
use axum::body::Body;
use chrono::Utc;
use hos_core::config::OAuthAppConfig;
use hos_core::domain::{Locale, TicketPriority, UserKind};
use hos_core::ids::{ClientId, UserId};
use hos_core::money::Currency;
use hos_core::secret::SecretString;
use hos_db::enums::{AuthorKind, ImpersonatorKind, MessageSource, OAuthProvider};
use hos_db::repo::clients::{self, ClientInput};
use hos_db::repo::identity::{self, NewUser, SessionOwner};
use hos_db::repo::support;
use hos_web::config::OAuthEndpoints;
use http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "Gv7#rQ2!mWz9pLx4";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

async fn csrf(app: &mut TestApp, page: &str) -> String {
    app.get(page).await.csrf()
}

async fn login(app: &mut TestApp, email: &str, password: &str) -> Res {
    let token = csrf(app, "/auth/login").await;
    app.post(
        "/auth/login",
        &[
            ("csrf_token", &token),
            ("email", email),
            ("password", password),
        ],
    )
    .await
}

async fn register(app: &mut TestApp, email: &str) -> Res {
    let token = csrf(app, "/auth/register").await;
    app.post(
        "/auth/register",
        &[
            ("csrf_token", &token),
            ("first_name", "Deneme"),
            ("last_name", "Müşteri"),
            ("email", email),
            ("password", PASSWORD),
            ("terms", "1"),
            ("website", ""),
        ],
    )
    .await
}

/// A client user (with `PASSWORD`).
async fn user(app: &TestApp, email: &str, verified: bool) -> UserId {
    let hash = hos_web::routes::auth::hash_password(PASSWORD)
        .await
        .expect("hash");
    identity::create_user(
        app.pool(),
        app.tenant_id,
        &NewUser {
            email: email.to_owned(),
            password_hash: Some(hash),
            kind: UserKind::Client,
            first_name: "Test".into(),
            last_name: "Kullanıcı".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: verified,
        },
    )
    .await
    .expect("user")
    .id
}

/// A client account owned by a new user.
async fn owner(app: &TestApp, email: &str, verified: bool) -> (UserId, ClientId) {
    let id = user(app, email, verified).await;
    let mut conn = app.pool().acquire().await.expect("conn");
    let client = clients::create_client_with_owner(
        &mut conn,
        app.tenant_id,
        &ClientInput::individual("Test", "Kullanıcı", email, Currency::TRY),
        id,
    )
    .await
    .expect("client");
    (id, client.id)
}

/// A sub-user of `client` with the given areas.
async fn sub_user(app: &TestApp, client: ClientId, email: &str, areas: &[&str]) -> UserId {
    let id = user(app, email, true).await;
    let areas: Vec<String> = areas.iter().map(|a| (*a).to_owned()).collect();
    clients::link_user(app.pool(), app.tenant_id, client, id, false, &areas)
        .await
        .expect("member");
    id
}

async fn key_for(app: &TestApp, user: UserId) -> String {
    let g = hos_auth::tokens::generate_api_key();
    identity::create_api_key(
        app.pool(),
        app.tenant_id,
        Some(user),
        "test key",
        &g.lookup,
        &g.key_hash,
        &[],
        None,
    )
    .await
    .expect("api key");
    g.key.expose_str().to_owned()
}

/// A JSON API call with an optional Bearer key and the cookie jar.
async fn api(
    app: &TestApp,
    verb: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(verb)
        .uri(uri)
        .header("host", &app.host);
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    if !app.cookies.is_empty() {
        let cookie = app
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        b = b.header("cookie", cookie);
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
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
}

/// Rebuilds the app with GitHub sign-in configured (`base`: a mock server,
/// `None`: the real endpoints, never contacted by the tests that use it).
fn with_github(app: &mut TestApp, base: Option<&str>) {
    let mut config = app.state.config.clone();
    config.github_oauth = Some(OAuthAppConfig {
        client_id: "test-client".into(),
        client_secret: SecretString::from("test-secret"),
    });
    let mut web = app.state.web.clone();
    if let Some(base) = base {
        web.oauth_endpoints = Some(OAuthEndpoints {
            google: None,
            github: Some(hos_auth::oauth::GitHubEndpoints {
                authorize: url::Url::parse(&format!("{base}/login/oauth/authorize")).unwrap(),
                token: url::Url::parse(&format!("{base}/login/oauth/access_token")).unwrap(),
                api_base: url::Url::parse(&format!("{base}/api")).unwrap(),
            }),
        });
    }
    let state = common::new_state(app.pool().clone(), config, web);
    app.router = hos_web::app(state.clone());
    app.state = state;
}

async fn github_mocks(server: &MockServer, id: u64, email: &str) {
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gho_test", "token_type": "bearer", "scope": "read:user,user:email"
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "login": "octo", "id": id, "name": "Victim Owner", "email": null, "avatar_url": null
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user/emails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"email": email, "primary": true, "verified": true}
        ])))
        .mount(server)
        .await;
}

fn github_profile(id: &str) -> identity::OAuthProfile {
    identity::OAuthProfile {
        provider: OAuthProvider::Github,
        provider_user_id: id.into(),
        email: Some(format!("{id}@attacker.test")),
        display_name: None,
        avatar_url: None,
        raw: json!({}),
    }
}

/// What a pre-registration of `victim` could have planted: a GitHub link
/// (`1111`), an API key, a confirmed 2FA secret and a sub-user it invited
/// into its client account. Returns (planted key, invited sub-user).
async fn plant(app: &TestApp, victim: UserId, client: ClientId) -> (String, UserId) {
    identity::link_identity(app.pool(), app.tenant_id, victim, &github_profile("1111"))
        .await
        .expect("planted identity");
    let key = key_for(app, victim).await;
    identity::upsert_totp_secret(app.pool(), app.tenant_id, victim, b"opaque", &[])
        .await
        .expect("totp");
    identity::confirm_totp(app.pool(), app.tenant_id, victim)
        .await
        .expect("confirm totp");
    let accomplice = user(app, &format!("accomplice-{}@attacker.test", app.slug), true).await;
    clients::link_invited_user(
        app.pool(),
        app.tenant_id,
        client,
        accomplice,
        &["services".to_owned()],
        victim,
    )
    .await
    .expect("invited member");
    (key, accomplice)
}

/// Asserts that everything [`plant`] left behind is gone.
async fn assert_reclaimed(
    app: &TestApp,
    victim: UserId,
    client: ClientId,
    key: &str,
    accomplice: UserId,
) {
    let identities = identity::list_identities(app.pool(), app.tenant_id, victim)
        .await
        .unwrap();
    assert!(
        identities.iter().all(|i| i.provider_user_id != "1111"),
        "planted GitHub link removed: {identities:?}"
    );
    let keys = identity::list_api_keys(app.pool(), app.tenant_id, Some(victim))
        .await
        .unwrap();
    assert!(!keys.is_empty() && keys.iter().all(|k| k.revoked_at.is_some()));
    let (status, _) = api(app, "GET", "/api/v1/me", Some(key), None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "planted key is dead");
    assert!(
        clients::client_membership(app.pool(), app.tenant_id, client, accomplice)
            .await
            .unwrap()
            .is_none(),
        "invited sub-user removed"
    );
    assert!(
        clients::client_membership(app.pool(), app.tenant_id, client, victim)
            .await
            .unwrap()
            .is_some_and(|m| m.is_owner),
        "the owner stays"
    );
    let u = identity::get_user(app.pool(), app.tenant_id, victim)
        .await
        .unwrap()
        .unwrap();
    assert!(!u.totp_enabled, "planted 2FA removed");
    assert!(u.email_verified_at.is_some());
}

/// Puts a `Set-Cookie` value of `start_session` into the jar.
fn use_cookie(app: &mut TestApp, set_cookie: &str) {
    let pair = set_cookie.split(';').next().unwrap();
    let (name, value) = pair.split_once('=').unwrap();
    app.cookies.insert(name.to_owned(), value.to_owned());
}

// ---------------------------------------------------------------------------
// H1 — unverified e-mail / account pre-hijacking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unverified_email_blocks_password_login() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // Default tenant setting: verification required.
    let email = format!("unverified-{}@example.test", app.slug);
    let r = register(&mut app, &email).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/auth/verify");
    let session_cookie = app.state.sessions.cookie_name().to_owned();

    // The password alone opens no session; the page says why.
    let r = login(&mut app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(r.body.contains("doğrulamanız gerekiyor"), "{}", r.body);
    assert!(!app.cookies.contains_key(&session_cookie));
    assert_eq!(app.get("/client").await.status, StatusCode::SEE_OTHER);
    // A wrong password still gets the generic answer (no hint).
    let r = login(&mut app, &email, "not-the-password").await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!r.body.contains("doğrulamanız gerekiyor"));

    // Once verified, the same login works.
    let u = identity::user_by_email(app.pool(), app.tenant_id, &email)
        .await
        .unwrap()
        .unwrap();
    assert!(u.last_login_at.is_none());
    identity::mark_email_verified(app.pool(), app.tenant_id, u.id)
        .await
        .unwrap();
    let r = login(&mut app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/client");
    assert_eq!(app.get("/client").await.status, StatusCode::OK);
    app.cleanup().await;
}

#[tokio::test]
async fn unverified_account_cannot_link_oauth_or_create_api_keys() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    with_github(&mut app, None);
    app.setting("auth.require_email_verification", json!(false))
        .await;
    let email = format!("link-{}@example.test", app.slug);
    let r = register(&mut app, &email).await;
    assert_eq!(r.location(), "/client", "logged in, e-mail unverified");
    let u = identity::user_by_email(app.pool(), app.tenant_id, &email)
        .await
        .unwrap()
        .unwrap();
    assert!(u.email_verified_at.is_none());

    let token = csrf(&mut app, "/client/account/security").await;
    let r = app
        .post(
            "/auth/oauth/github/link",
            &[("csrf_token", &token), ("next", "/client/account/security")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(
        !r.location().starts_with("http"),
        "no hand-off to the provider: {}",
        r.location()
    );
    assert!(r.location().contains("/client/account/security"));

    let page = app.get("/client/account/api-keys").await;
    assert_eq!(page.status, StatusCode::OK);
    let r = app
        .post(
            "/client/account/api-keys",
            &[("csrf_token", &page.csrf()), ("name", "script")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(!r.body.contains("hos_live_"));
    assert!(
        identity::list_api_keys(app.pool(), app.tenant_id, Some(u.id))
            .await
            .unwrap()
            .is_empty()
    );

    // Verified: both work.
    identity::mark_email_verified(app.pool(), app.tenant_id, u.id)
        .await
        .unwrap();
    let r = app
        .post(
            "/auth/oauth/github/link",
            &[("csrf_token", &token), ("next", "/client/account/security")],
        )
        .await;
    assert!(
        r.location()
            .starts_with("https://github.com/login/oauth/authorize"),
        "{}",
        r.location()
    );
    let r = app
        .post(
            "/client/account/api-keys",
            &[("csrf_token", &page.csrf()), ("name", "script")],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("hos_live_"));
    app.cleanup().await;
}

#[tokio::test]
async fn oauth_takeover_of_unverified_account_revokes_planted_access() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let server = MockServer::start().await;
    with_github(&mut app, Some(&server.uri()));
    // Someone pre-registered the victim's address (never verified) and
    // planted their own ways back in.
    let email = format!("victim-{}@example.test", app.slug);
    let (victim, client) = owner(&app, &email, false).await;
    let (key, accomplice) = plant(&app, victim, client).await;
    let (status, _) = api(&app, "GET", "/api/v1/me", Some(&key), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "planted key works before");

    // The real owner signs in with GitHub (verified e-mail, another account).
    github_mocks(&server, 2222, &email).await;
    let r = app.get("/auth/oauth/github").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let state = url::Url::parse(r.location())
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state");
    let r = app
        .get(&format!(
            "/auth/oauth/github/callback?code=gh-code&state={state}"
        ))
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/client");

    assert_reclaimed(&app, victim, client, &key, accomplice).await;
    let identities = identity::list_identities(app.pool(), app.tenant_id, victim)
        .await
        .unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(
        identities[0].provider_user_id, "2222",
        "the sign-in stays linked"
    );
    let u = identity::get_user(app.pool(), app.tenant_id, victim)
        .await
        .unwrap()
        .unwrap();
    assert!(u.password_hash.is_none(), "planted password removed");
    assert_eq!(
        identity::list_user_sessions(app.pool(), app.tenant_id, victim)
            .await
            .unwrap()
            .len(),
        1,
        "only the owner's new session"
    );
    app.cleanup().await;
}

#[tokio::test]
async fn password_reset_of_unverified_account_revokes_planted_access() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let email = format!("reset-{}@example.test", app.slug);
    let (victim, client) = owner(&app, &email, false).await;
    let (key, accomplice) = plant(&app, victim, client).await;
    // A verified account keeps its sign-in methods on a reset.
    let (verified, _) = owner(&app, &format!("kept-{}@example.test", app.slug), true).await;
    identity::link_identity(app.pool(), app.tenant_id, verified, &github_profile("3333"))
        .await
        .unwrap();

    for user_id in [victim, verified] {
        let issued = hos_auth::tokens::issue_token(
            hos_auth::tokens::TokenPurpose::PasswordReset,
            Utc::now(),
        );
        identity::create_password_reset(
            app.pool(),
            app.tenant_id,
            user_id,
            &issued.token_hash,
            issued.expires_at,
            None,
        )
        .await
        .unwrap();
        let token = issued.token.expose_str().to_owned();
        let page = app.get(&format!("/auth/reset?token={token}")).await;
        let r = app
            .post(
                "/auth/reset",
                &[
                    ("csrf_token", &page.csrf()),
                    ("token", &token),
                    ("password", "Yeni-Parola-2026!zx"),
                    ("password_confirm", "Yeni-Parola-2026!zx"),
                ],
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    }
    assert_reclaimed(&app, victim, client, &key, accomplice).await;
    assert_eq!(
        identity::list_identities(app.pool(), app.tenant_id, verified)
            .await
            .unwrap()
            .len(),
        1,
        "verified account untouched"
    );
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// H2 — client sub-user permissions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_sub_user_areas_apply_to_api_keys_and_sessions() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (owner_id, client) = owner(&app, &format!("own-{}@example.test", app.slug), true).await;
    let sub_email = format!("sub-{}@example.test", app.slug);
    let sub = sub_user(&app, client, &sub_email, &["tickets"]).await;
    let owner_key = key_for(&app, owner_id).await;
    let sub_key = key_for(&app, sub).await;

    for path in [
        "/api/v1/services",
        "/api/v1/invoices",
        "/api/v1/transactions",
        "/api/v1/orders",
        "/api/v1/domains",
        "/api/v1/clients",
    ] {
        let (status, body) = api(&app, "GET", path, Some(&sub_key), None, &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        let (status, body) = api(&app, "GET", path, Some(&owner_key), None, &[]).await;
        assert_eq!(status, StatusCode::OK, "owner {path}: {body}");
    }
    let (status, _) = api(
        &app,
        "POST",
        "/api/v1/orders",
        Some(&sub_key),
        Some(json!({"items": []})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for path in ["/api/v1/tickets", "/api/v1/departments", "/api/v1/me"] {
        let (status, body) = api(&app, "GET", path, Some(&sub_key), None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }

    // Evaluated per request: granting / revoking takes effect immediately.
    clients::set_member_permissions(
        app.pool(),
        app.tenant_id,
        client,
        sub,
        &["services".to_owned(), "tickets".to_owned()],
    )
    .await
    .unwrap();
    let (status, _) = api(&app, "GET", "/api/v1/services", Some(&sub_key), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    clients::set_member_permissions(
        app.pool(),
        app.tenant_id,
        client,
        sub,
        &["tickets".to_owned()],
    )
    .await
    .unwrap();

    // The session cookie path follows the same rules.
    let r = login(&mut app, &sub_email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let (status, _) = api(&app, "GET", "/api/v1/services", None, None, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = api(&app, "GET", "/api/v1/tickets", None, None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    app.cookies.clear();

    // Removed from the account: the key stops working.
    clients::unlink_user(app.pool(), app.tenant_id, client, sub)
        .await
        .unwrap();
    let (status, _) = api(&app, "GET", "/api/v1/tickets", Some(&sub_key), None, &[]).await;
    assert!(status.is_client_error(), "{status}");
    app.cleanup().await;
}

#[tokio::test]
async fn api_keys_page_is_owner_only_and_checkout_needs_orders() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let owner_email = format!("own-{}@example.test", app.slug);
    let (_, client) = owner(&app, &owner_email, true).await;
    let sub_email = format!("sub-{}@example.test", app.slug);
    let sub = sub_user(&app, client, &sub_email, &["tickets"]).await;

    let r = login(&mut app, &sub_email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let token = csrf(&mut app, "/client").await;
    assert_eq!(
        app.get("/client/account/api-keys").await.status,
        StatusCode::FORBIDDEN
    );
    let r = app
        .post(
            "/client/account/api-keys",
            &[("csrf_token", &token), ("name", "x")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(app.get("/checkout").await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.get("/checkout/review").await.status,
        StatusCode::FORBIDDEN
    );
    let r = app
        .post(
            "/checkout/place",
            &[
                ("csrf_token", &token),
                ("payment", "credit"),
                ("terms", "1"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // The cart itself stays usable.
    assert_eq!(app.get("/cart").await.status, StatusCode::OK);

    clients::set_member_permissions(
        app.pool(),
        app.tenant_id,
        client,
        sub,
        &["orders".to_owned(), "tickets".to_owned()],
    )
    .await
    .unwrap();
    assert_ne!(app.get("/checkout").await.status, StatusCode::FORBIDDEN);

    app.cookies.clear();
    let r = login(&mut app, &owner_email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.get("/client/account/api-keys").await.status,
        StatusCode::OK
    );
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// M1 — open redirect through the locale prefix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn locale_prefix_cannot_produce_open_redirects() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    for path in ["/tr//evil.com", "/en//evil.com", "/tr//evil.com/x?y=1"] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path} → {}", r.location());
        assert!(!r.location().contains("evil.com"));
    }
    // Ordinary prefix handling is unchanged.
    let r = app.get("/tr/kb?q=x").await;
    assert_eq!(r.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(r.location(), "/kb?q=x");

    let r = app.get("/lang/tr?next=/en//evil.com").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/");
    let r = app.get("/lang/en?next=/tr//evil.com").await;
    assert_eq!(r.location(), "/en/");
    let r = app.get("/lang/en?next=/kb").await;
    assert_eq!(r.location(), "/en/kb");

    let r = app.get("/auth/login?next=/en//evil.com").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("evil.com"), "next dropped");
    let r = app.get("/auth/login?next=/client/services").await;
    assert!(r.body.contains("name=\"next\""), "a safe next is kept");
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// F3 — Host header confusion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_host_header_is_rejected() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    for host in [
        format!("{}:@evil.com", app.host),
        format!("{}:80@evil.com", app.host),
        format!("{}@evil.com", app.host),
        format!("{} evil.com", app.host),
    ] {
        for path in ["/", "/sitemap.xml", "/auth/forgot", "/platform/login"] {
            let r = app.send_to(&host, Method::GET, path, None, &[]).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{host}{path}");
            assert!(!r.body.contains("evil.com"));
        }
    }
    // A valid host with a port is the origin of absolute links.
    let host = format!("{}:8080", app.host);
    let r = app.send_to(&host, Method::GET, "/", None, &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains(&format!("rel=\"canonical\" href=\"http://{host}/")),
        "canonical uses the validated host"
    );
    let r = app
        .send_to(&host, Method::GET, "/sitemap.xml", None, &[])
        .await;
    assert!(r.body.contains(&format!("<loc>http://{host}/")));
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// L1 — 2FA setup while impersonating
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_factor_setup_refuses_impersonated_sessions() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let email = format!("imp-{}@example.test", app.slug);
    let (client_user, _) = owner(&app, &email, true).await;
    let staff_email = format!("staff-{}@example.test", app.slug);
    assert!(
        hos_web::cli::create_staff(
            app.pool(),
            app.tenant_id,
            &hos_web::cli::StaffSpec {
                email: &staff_email,
                password: "Sufficiently-long-pass-42",
                role: "owner",
                first_name: "Destek",
                last_name: "Personel",
            },
        )
        .await
        .unwrap()
    );
    let staff = identity::user_by_email(app.pool(), app.tenant_id, &staff_email)
        .await
        .unwrap()
        .unwrap();
    // "Log in as client" session.
    let (_, cookie) = hos_web::session::start_session(
        &app.state,
        hos_web::session::StartSession {
            owner: SessionOwner::User {
                tenant_id: app.tenant_id,
                user_id: client_user,
            },
            remember: false,
            mfa_verified: true,
            ip: None,
            user_agent: None,
            impersonator: Some((staff.id.into(), ImpersonatorKind::Staff)),
            data: json!({}),
        },
    )
    .await
    .unwrap();
    use_cookie(&mut app, &cookie);
    let dash = app.get("/client").await;
    assert_eq!(dash.status, StatusCode::OK);
    assert_eq!(
        app.get("/auth/2fa/setup").await.status,
        StatusCode::FORBIDDEN
    );
    let r = app
        .post(
            "/auth/2fa/setup",
            &[("csrf_token", &dash.csrf()), ("code", "123456")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app
        .post(
            "/auth/2fa/disable",
            &[("csrf_token", &dash.csrf()), ("code", "123456")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        identity::get_totp_secret(app.pool(), app.tenant_id, client_user)
            .await
            .unwrap()
            .is_none(),
        "no secret written for the impersonator"
    );

    // The account holder's own session may.
    app.cookies.clear();
    let r = login(&mut app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(app.get("/auth/2fa/setup").await.status, StatusCode::OK);
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// L2 — API while staff 2FA setup is pending
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_refuses_staff_session_pending_2fa_setup() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("security.staff_2fa_required", json!(true))
        .await;
    let email = format!("staff-{}@example.test", app.slug);
    assert!(
        hos_web::cli::create_staff(
            app.pool(),
            app.tenant_id,
            &hos_web::cli::StaffSpec {
                email: &email,
                password: PASSWORD,
                role: "owner",
                first_name: "Api",
                last_name: "Staff",
            },
        )
        .await
        .unwrap()
    );
    let r = login(&mut app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let admin = app.get("/admin").await;
    assert!(admin.location().contains("/auth/2fa/setup"), "admin gated");
    for path in ["/api/v1/me", "/api/v1/clients", "/api/v1/invoices"] {
        let (status, body) = api(&app, "GET", path, None, None, &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        assert_eq!(body["code"], "mfa_setup_required");
    }
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// L3 — platform host label, dev tenant fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn platform_host_label_and_dev_fallback_are_not_tenants() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    assert_eq!(app.get("/").await.status, StatusCode::OK);

    // The console lives on `<slug>.<base domain>`: that label is no tenant.
    let mut config = app.state.config.clone();
    config.base_url = url::Url::parse(&format!("http://{}:8080", app.host)).unwrap();
    let state = common::new_state(app.pool().clone(), config, app.state.web.clone());
    assert_eq!(
        hos_web::routes::platform::provision::check_slug(&state, &app.slug)
            .await
            .unwrap(),
        Err(hos_web::routes::platform::provision::SlugProblem::Reserved)
    );
    let res = hos_web::app(state)
        .oneshot(
            Request::get("/")
                .header("host", &app.host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // `localhost` → HOS_DEV_TENANT only in debug mode (here: this test's own
    // tenant as the dev tenant, never `demo`).
    for debug in [true, false] {
        let mut config = app.state.config.clone();
        config.dev_tenant = app.slug.clone();
        let mut web = app.state.web.clone();
        web.debug = debug;
        let state = common::new_state(app.pool().clone(), config, web);
        let res = hos_web::app(state)
            .oneshot(
                Request::get("/")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let expected = if debug {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        };
        assert_eq!(res.status(), expected, "debug = {debug}");
    }
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// L4 — X-Forwarded-For
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_ip_uses_the_trusted_proxy_hop() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let parts = |xff: &str| {
        Request::get("/")
            .header("x-forwarded-for", xff)
            .body(())
            .unwrap()
            .into_parts()
            .0
    };
    let mut web = app.state.web.clone();
    assert_eq!(web.trusted_proxy_hops, 1, "default: one proxy");
    web.trust_proxy = true;
    web.client_ip_header = None;
    web.trusted_proxies = Default::default();
    let ip = |web: &hos_web::config::WebConfig, p: &http::request::Parts| {
        hos_web::extract::ClientIp::from_config(p, web).0
    };
    // The client can prepend anything; the proxy's own entry is the last one.
    let spoofed = parts("6.6.6.6, 203.0.113.7");
    assert_eq!(ip(&web, &spoofed).to_string(), "203.0.113.7");
    web.trusted_proxy_hops = 2;
    assert_eq!(ip(&web, &spoofed).to_string(), "6.6.6.6");
    web.trusted_proxy_hops = 0;
    assert!(
        ip(&web, &spoofed).is_loopback(),
        "header ignored: socket address"
    );
    web.trusted_proxy_hops = 1;
    web.trust_proxy = false;
    assert!(ip(&web, &spoofed).is_loopback(), "HOS_TRUST_PROXY off");
    app.cleanup().await;
}

/// A request as the listener hands it over: with the TCP peer.
fn from_peer(mut req: Request<Body>, peer: &str) -> Request<Body> {
    req.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>().unwrap(),
    ));
    req
}

/// `HOS_TRUSTED_PROXIES`: a client that reaches the app directly (not
/// through the listed proxy) cannot pick its rate-limit bucket with a forged
/// `CF-Connecting-IP`; the proxy's forwarded visitors keep one bucket each.
#[tokio::test]
async fn forged_client_ip_header_from_an_unlisted_peer_is_ignored() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let mut web = app.state.web.clone();
    web.trust_proxy = true;
    web.trusted_proxy_hops = 1;
    web.client_ip_header = Some(http::HeaderName::from_static("cf-connecting-ip"));
    web.trusted_proxies = hos_web::config::TrustedProxies::parse("10.0.1.0/24");
    let config = app.state.config.clone();
    app.state = common::new_state(app.pool().clone(), config, web);
    app.router = hos_web::app(app.state.clone());

    let token = csrf(&mut app, "/auth/login").await;
    let cookie = app
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    // A new e-mail each time: only the per-address limit counts, not the
    // per-account lockout.
    let attempt = |peer: &'static str, forged: String| {
        let email = format!("nobody-{forged}@example.test");
        let body = common::serde_urlencoded(&[
            ("csrf_token", &token),
            ("email", &email),
            ("password", "wrong password"),
        ]);
        let req = Request::post("/auth/login")
            .header("host", &app.host)
            .header("cookie", &cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cf-connecting-ip", forged)
            .body(Body::from(body))
            .unwrap();
        app.router.clone().oneshot(from_peer(req, peer))
    };
    // The login limit is 10 per 5 minutes per address. Through the proxy,
    // twelve visitors are twelve buckets.
    for i in 0..12 {
        let res = attempt("10.0.1.5:40000", format!("198.51.100.{i}"))
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::TOO_MANY_REQUESTS, "visitor {i}");
    }
    // Directly, a new forged address per attempt does not help.
    let mut limited = false;
    for i in 0..12 {
        let res = attempt("203.0.113.9:5555", format!("192.0.2.{i}"))
            .await
            .unwrap();
        limited |= res.status() == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(limited, "the direct client is limited by its own address");
    app.cleanup().await;
}

/// Cloudflare → Traefik (default: forwarded headers not trusted) → app:
/// `X-Forwarded-For` holds only Cloudflare's edge address, so the visitor
/// comes from `HOS_CLIENT_IP_HEADER=CF-Connecting-IP`; rate limits key on it.
#[tokio::test]
async fn client_ip_header_names_the_visitor_behind_a_cdn() {
    use hos_web::extract::ClientIp;
    let Some(app) = TestApp::new().await else {
        return;
    };
    let edge = "172.68.1.10";
    let parts = |cf: Option<&str>| {
        let mut b = Request::get("/").header("x-forwarded-for", edge);
        if let Some(cf) = cf {
            b = b.header("cf-connecting-ip", cf);
        }
        b.body(()).unwrap().into_parts().0
    };
    let mut web = app.state.web.clone();
    web.trust_proxy = true;
    web.trusted_proxy_hops = 1;
    web.client_ip_header = None;
    web.trusted_proxies = Default::default();
    // Without the header setting every visitor shares the edge address.
    let ip = ClientIp::from_config(&parts(Some("198.51.100.4")), &web);
    assert_eq!(ip.0.to_string(), edge);

    web.client_ip_header = Some(http::HeaderName::from_static("cf-connecting-ip"));
    let a = ClientIp::from_config(&parts(Some("198.51.100.4")), &web);
    let b = ClientIp::from_config(&parts(Some("198.51.100.5")), &web);
    assert_eq!(a.0.to_string(), "198.51.100.4");
    assert_ne!(a.key("login"), b.key("login"), "one bucket per visitor");
    // Missing or invalid header: falls back to X-Forwarded-For.
    assert_eq!(
        ClientIp::from_config(&parts(None), &web).0.to_string(),
        edge
    );
    let bad = ClientIp::from_config(&parts(Some("1.2.3.4, 5.6.7.8")), &web);
    assert_eq!(bad.0.to_string(), edge);
    // Ignored entirely unless the proxy is trusted.
    web.trust_proxy = false;
    let ip = ClientIp::from_config(&parts(Some("198.51.100.4")), &web);
    assert!(ip.0.is_loopback(), "HOS_TRUST_PROXY off");
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// L6 — CSRF for cookie-authenticated API calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cookie_authenticated_api_posts_need_csrf() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let email = format!("csrf-{}@example.test", app.slug);
    let (_, client) = owner(&app, &email, true).await;
    let r = login(&mut app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let ticket = json!({"subject": "CSRF", "message": "cookie only"});
    // A non-Bearer Authorization header does not make the cookie call exempt.
    for auth in ["Basic Zm9vOmJhcg==", "Token abc"] {
        let (status, body) = api(
            &app,
            "POST",
            "/api/v1/tickets",
            None,
            Some(ticket.clone()),
            &[("authorization", auth)],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{auth}: {body}");
    }
    let open = support::list_tickets(
        app.pool(),
        app.tenant_id,
        &support::TicketFilter {
            client_id: Some(client),
            ..support::TicketFilter::default()
        },
        hos_db::Page::first(10),
    )
    .await
    .unwrap();
    assert_eq!(open.total, 0, "nothing created without a token");
    // With the page's token the cookie call goes through.
    let token = csrf(&mut app, "/client").await;
    let (status, body) = api(
        &app,
        "POST",
        "/api/v1/tickets",
        None,
        Some(ticket),
        &[("x-csrf-token", &token)],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // Bearer keys stay exempt (and are checked as keys).
    let (status, _) = api(
        &app,
        "POST",
        "/api/v1/tickets",
        Some("hos_live_bogus_key"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// F4 — attachments of internal notes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn internal_note_attachments_are_not_downloadable_by_clients() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let email = format!("att-{}@example.test", app.slug);
    let (user_id, client) = owner(&app, &email, true).await;
    let dept = support::list_departments(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("seeded department");
    let message = |kind: AuthorKind, internal: bool, body: &str| support::NewTicketMessage {
        author_kind: kind,
        user_id: None,
        author_name: "Test".into(),
        author_email: None,
        body: body.into(),
        is_internal: internal,
        source: MessageSource::Web,
        message_id: None,
        ip: None,
    };
    let mut conn = app.pool().acquire().await.unwrap();
    let (ticket, first) = support::open_ticket(
        &mut conn,
        app.tenant_id,
        &support::NewTicket {
            department_id: dept.id,
            client_id: Some(client),
            user_id: Some(user_id),
            guest_name: None,
            guest_email: None,
            subject: "Ek".into(),
            priority: TicketPriority::Medium,
            service_id: None,
            domain_id: None,
            first_message: message(AuthorKind::Client, false, "Merhaba"),
        },
    )
    .await
    .unwrap();
    let (note, _) = support::reply_to_ticket(
        &mut conn,
        app.tenant_id,
        ticket.id,
        &message(AuthorKind::Staff, true, "Dahili not"),
    )
    .await
    .unwrap();
    drop(conn);
    let attach = |msg, name: &'static str| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        let ticket = ticket.id;
        async move {
            support::add_attachment_content(
                &pool,
                tenant,
                ticket,
                Some(msg),
                name,
                "text/plain",
                b"icerik",
                &hos_core::crypto::sha256_hex(b"icerik"),
            )
            .await
            .unwrap()
            .id
        }
    };
    let public = attach(first.id, "public.txt").await;
    let internal = attach(note.id, "internal.txt").await;

    let r = login(&mut app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let base = format!("/client/tickets/{}/attachments", ticket.id);
    assert_eq!(
        app.get(&format!("{base}/{public}")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.get(&format!("{base}/{internal}")).await.status,
        StatusCode::NOT_FOUND,
        "internal note files stay internal"
    );
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// ETag / CSRF binding (a `304` must never hand a page of another session)
// ---------------------------------------------------------------------------

/// The page body with its CSRF token removed (what the ETag hashes).
fn without_token(res: &Res) -> String {
    res.body.replace(&res.csrf(), "")
}

fn etag_of(res: &Res) -> String {
    res.headers
        .get(http::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("etag")
        .to_owned()
}

async fn get_if_none_match(app: &mut TestApp, path: &str, etag: &str) -> Res {
    let host = app.host.clone();
    app.send_to(&host, Method::GET, path, None, &[("if-none-match", etag)])
        .await
}

/// Regression: after a new login the page bytes (token aside) were the same,
/// the browser revalidated with the old ETag, got `304` and kept a form whose
/// CSRF token belonged to the previous session, so every POST failed (403).
#[tokio::test]
async fn etag_is_bound_to_the_session_of_the_csrf_token() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let email = format!("etag-{}@staff.test", uuid::Uuid::now_v7().simple());
    crate::biz_support::user(&app, &email, UserKind::Staff, Some("owner")).await;
    let page = "/admin/settings/general";

    // ---- session 1
    crate::biz_support::login(&mut app, &email).await;
    let first = app.get(page).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let first_etag = etag_of(&first);
    // The same session revalidates.
    let again = get_if_none_match(&mut app, page, &first_etag).await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());

    // ---- session 2 (same user, same page bytes apart from the token)
    crate::biz_support::login(&mut app, &email).await;
    let second = get_if_none_match(&mut app, page, &first_etag).await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "a new session must not get 304 for the old session's ETag"
    );
    assert_eq!(
        without_token(&first),
        without_token(&second),
        "precondition: only the token differs, so the ETag alone separates them"
    );
    assert_ne!(etag_of(&second), first_etag);
    assert_ne!(second.csrf(), first.csrf());
    // Its own ETag still revalidates …
    let second_etag = etag_of(&second);
    assert_eq!(
        get_if_none_match(&mut app, page, &second_etag).await.status,
        StatusCode::NOT_MODIFIED
    );
    // … and the token of the page it got works for a POST.
    let out = app
        .post("/auth/logout", &[("csrf_token", &second.csrf())])
        .await;
    assert_eq!(out.status, StatusCode::SEE_OTHER, "{}", out.body);

    // ---- anonymous visitors: bound to the `hos_anon` cookie
    app.cookies.clear();
    let a = app.get("/contact").await;
    assert_eq!(a.status, StatusCode::OK);
    let a_etag = etag_of(&a);
    assert_eq!(
        get_if_none_match(&mut app, "/contact", &a_etag)
            .await
            .status,
        StatusCode::NOT_MODIFIED
    );
    app.cookies.clear();
    let first_visit = get_if_none_match(&mut app, "/contact", &a_etag).await;
    assert_eq!(first_visit.status, StatusCode::OK);
    let b = get_if_none_match(&mut app, "/contact", &a_etag).await;
    assert_eq!(
        b.status,
        StatusCode::OK,
        "another visitor must not get 304 for the first visitor's ETag"
    );
    assert_eq!(without_token(&a), without_token(&b));
    app.cleanup().await;
}
