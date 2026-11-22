//! Public demo mode (`HOS_DEMO_MODE`, docs/DEMO.md): every block, the
//! protected demo logins, the banner / noindex / login hints, the mailbox
//! and the scheduled reset. Each test runs on its own throw-away demo
//! tenant ([`TestApp::demo`]); the shared database is never switched into
//! demo mode and its `demo` tenant is not touched.

use hos_core::domain::IntegrationCategory;
use hos_core::money::Currency;
use hos_db::repo::clients::{self, ClientInput};
use hos_db::repo::{identity, mail, platform};
use http::{Method, StatusCode};
use serde_json::json;

use crate::common::{Res, TestApp};

const STAFF: (&str, &str) = ("admin@demo.test", "demo-admin");
const CLIENT: (&str, &str) = ("musteri@demo.test", "demo-client");

async fn login(app: &mut TestApp, (email, password): (&str, &str)) -> Res {
    let page = app.get("/auth/login").await;
    let token = page.csrf();
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

async fn login_ok(app: &mut TestApp, account: (&str, &str)) {
    let r = login(app, account).await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "login {}: {}",
        account.0,
        r.body
    );
}

fn is_refused(r: &Res) -> bool {
    r.status == StatusCode::FORBIDDEN
        && (r.body.contains("Demoda kullanılamaz")
            || r.body.contains("Not available in the demo")
            || r.body.contains("demo_disabled"))
}

/// A form post the demo refuses, sent without a `Referer` (so the answer is
/// the 403 page, not a redirect back) and without a CSRF token: the demo
/// guard runs before the CSRF check.
async fn post_bare(app: &mut TestApp, path: &str) -> Res {
    app.post(path, &[("x", "1")]).await
}

#[tokio::test]
async fn demo_reset_restores_the_seed_and_keeps_logins_working() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    let tenant = app.tenant_id;
    // The seeded demo: logins, mocks, a demo order, sign-up off.
    login_ok(&mut app, STAFF).await;
    let dash = app.get("/admin").await;
    assert_eq!(dash.status, StatusCode::OK, "{}", dash.body);
    let orders: i64 = sqlx::query_scalar("select count(*) from orders where tenant_id = $1")
        .bind(tenant)
        .fetch_one(app.pool())
        .await
        .unwrap();
    assert_eq!(orders, 1, "the demo order");
    let mocks: Vec<String> = sqlx::query_scalar(
        "select kind from integrations where tenant_id = $1 and is_active order by kind",
    )
    .bind(tenant)
    .fetch_all(app.pool())
    .await
    .unwrap();
    assert!(mocks.iter().all(|k| k == "mock"), "{mocks:?}");
    assert_eq!(mocks.len(), 4);
    assert_eq!(
        platform::get_setting(app.pool(), tenant, "auth.allow_registration")
            .await
            .unwrap(),
        Some(json!(false))
    );

    // A visitor changes things.
    platform::set_setting(app.pool(), tenant, "seo.title_suffix", &json!(" | defaced"))
        .await
        .unwrap();
    let stranger = clients::create_client(
        app.pool(),
        tenant,
        &ClientInput::individual("Kalıcı", "Değil", "kalici@example.org", Currency::TRY),
    )
    .await
    .unwrap();
    let staff = identity::user_by_email(app.pool(), tenant, STAFF.0)
        .await
        .unwrap()
        .unwrap();
    identity::set_password_hash(app.pool(), tenant, staff.id, Some("not-a-hash"))
        .await
        .unwrap();

    // Reset: same tenant id, visitor data gone, seed back, logins work.
    let report = hos_web::demo::reset(&app.state).await.expect("reset");
    assert_eq!(report.tenant_id, tenant, "the reset keeps the tenant id");
    assert!(
        clients::get_client(app.pool(), tenant, stranger.id)
            .await
            .unwrap()
            .is_none(),
        "visitor data is wiped"
    );
    assert_ne!(
        platform::get_setting(app.pool(), tenant, "seo.title_suffix")
            .await
            .unwrap(),
        Some(json!(" | defaced"))
    );
    // The old session is gone: back to the login page, which works again.
    let r = app.get("/admin").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(r.location().contains("/auth/login"), "{}", r.location());
    login_ok(&mut app, STAFF).await;
    app.cookies.clear();
    login_ok(&mut app, CLIENT).await;
    let orders: i64 = sqlx::query_scalar("select count(*) from orders where tenant_id = $1")
        .bind(tenant)
        .fetch_one(app.pool())
        .await
        .unwrap();
    assert_eq!(orders, 1, "exactly the seeded order again");
    app.cleanup().await;
}

#[tokio::test]
async fn demo_refuses_outbound_traffic_raw_html_and_lockouts() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    login_ok(&mut app, STAFF).await;
    for path in [
        "/admin/integrations",
        "/admin/developer/webhooks/new",
        "/admin/settings/security",
        "/admin/settings/registration",
        "/admin/settings/einvoice",
        "/admin/settings/email/templates/invoice_created/tr",
        "/admin/ai/generate",
        "/admin/seo/indexnow/submit",
        "/admin/tools/import/new",
        "/auth/2fa/setup",
    ] {
        let r = post_bare(&mut app, path).await;
        assert!(is_refused(&r), "POST {path}: {} {}", r.status, r.body);
    }
    for path in [
        "/admin/tools/import/new",
        "/auth/register",
        "/auth/forgot",
        "/auth/oauth/google",
        "/whois?domain=example.com",
    ] {
        let r = app.get(path).await;
        assert!(is_refused(&r), "GET {path}: {} {}", r.status, r.body);
    }
    // Custom domains belong to multi-company operation: not there at all.
    let r = app.get("/admin/settings/domains").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
    // The REST API is read-only: problem+json.
    let r = post_bare(&mut app, "/api/v1/clients").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(r.body.contains("demo_disabled"), "{}", r.body);
    let r = app.get("/api/v1/me").await;
    assert_eq!(r.status, StatusCode::OK, "reads stay allowed: {}", r.body);
    // Inbound mail webhooks (tenantless): refused before any work.
    let host = app.host.clone();
    let r = app
        .send_to(
            &host,
            Method::POST,
            "/webhooks/mail/raw",
            Some(&[("x", "1")]),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);

    // A refused form post with a same-host Referer goes back with a flash.
    let referer = format!("http://{host}/admin/settings");
    let r = app
        .send_to(
            &host,
            Method::POST,
            "/admin/settings/security",
            Some(&[("password_min_length", "64")]),
            &[("referer", &referer)],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/admin/settings");
    assert!(
        r.headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .any(|c| c.to_str().unwrap_or_default().starts_with("hos_flash=")),
        "the refusal is explained"
    );
    // … but never to another host.
    let r = app
        .send_to(
            &host,
            Method::POST,
            "/admin/settings/security",
            Some(&[("password_min_length", "64")]),
            &[("referer", "https://evil.example/phish")],
        )
        .await;
    assert!(is_refused(&r), "{} {}", r.status, r.body);

    // The demo admin cannot be deactivated, demoted or reset.
    let admin = identity::user_by_email(app.pool(), app.tenant_id, STAFF.0)
        .await
        .unwrap()
        .unwrap();
    for tail in ["deactivate", "roles", "reset-2fa"] {
        let path = format!("/admin/staff/{}/{tail}", admin.id);
        let r = post_bare(&mut app, &path).await;
        assert!(is_refused(&r), "POST {path}: {} {}", r.status, r.body);
    }
    app.cleanup().await;
}

/// Visitors may enter content, including what the public storefront shows;
/// it goes through the normal sanitising and the next reset deletes it.
#[tokio::test]
async fn demo_visitors_can_publish_test_content_until_the_reset() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    login_ok(&mut app, STAFF).await;
    // Allowed (a missing CSRF token is the only objection).
    for path in [
        "/admin/catalog/products",
        "/admin/catalog/tlds",
        "/admin/content/pages",
        "/admin/support/announcements",
        "/admin/support/kb",
        "/admin/downloads",
        "/admin/status/incidents",
        "/admin/seo/redirects",
        "/admin/fraud/bans",
        "/admin/staff/invite",
        "/admin/developer/keys",
        "/admin/settings/general",
        "/admin/settings/orders",
        "/admin/settings/email",
    ] {
        let r = post_bare(&mut app, path).await;
        assert!(!is_refused(&r), "POST {path}: {} {}", r.status, r.body);
    }

    let slug = format!("ziyaretci-{}", &app.slug[app.slug.len() - 6..]);
    let page = app.get("/admin/content/pages/new").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let token = page.csrf();
    let r = app
        .post(
            "/admin/content/pages",
            &[
                ("csrf_token", &token),
                ("tr_title", "Ziyaretçi sayfası"),
                ("tr_slug", &slug),
                ("tr_body", "Merhaba **demo**. <script>alert(1)</script>"),
                ("tr_status", "published"),
            ],
        )
        .await;
    assert!(
        r.status == StatusCode::SEE_OTHER && !is_refused(&r),
        "{} {}",
        r.status,
        r.body
    );
    app.cookies.clear();
    let public = app.get(&format!("/{slug}")).await;
    assert_eq!(public.status, StatusCode::OK, "{}", public.body);
    assert!(public.body.contains("Ziyaretçi sayfası"));
    assert!(public.body.contains("<strong>demo</strong>"));
    assert!(
        !public.body.contains("<script>alert(1)"),
        "raw HTML stays text"
    );
    assert!(
        public.body.contains("demo-banner"),
        "marked as demo content"
    );

    hos_web::demo::reset(&app.state).await.expect("reset");
    let gone = app.get(&format!("/{slug}")).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "deleted by the reset");
    app.cleanup().await;
}

/// Someone in the middle of a form when the reset runs gets a notice and a
/// fresh login, not a CSRF error, a 500 or a broken session.
#[tokio::test]
async fn demo_reset_mid_form_explains_itself() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    let host = app.host.clone();
    login_ok(&mut app, STAFF).await;
    let form = app.get("/admin/content/pages/new").await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(
        app.cookies.contains_key(hos_web::demo::GEN_COOKIE),
        "{:?}",
        app.cookies
    );
    let token = form.csrf();

    hos_web::demo::reset(&app.state).await.expect("reset");
    let referer = format!("http://{host}/admin/content/pages/new");
    let r = app
        .send_to(
            &host,
            Method::POST,
            "/admin/content/pages",
            Some(&[("csrf_token", &token), ("tr_title", "Yarım kalan")]),
            &[("referer", &referer)],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/admin/content/pages/new");
    let pages: i64 = sqlx::query_scalar(
        "select count(*) from pages where tenant_id = $1 and title = 'Yarım kalan'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(pages, 0, "the stale form is not processed");
    // Back on the form: the session is gone, so the login page explains why.
    let r = app.get("/admin/content/pages/new").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(r.location().contains("/auth/login"), "{}", r.location());
    let login_page = app.get(r.location()).await;
    assert_eq!(login_page.status, StatusCode::OK);
    assert!(
        login_page.body.contains("Demo az önce sıfırlandı"),
        "{}",
        login_page.body
    );
    login_ok(&mut app, STAFF).await;
    assert_eq!(app.get("/admin").await.status, StatusCode::OK);

    // htmx requests are sent to the page instead of swapping in an error.
    hos_web::demo::reset(&app.state).await.expect("reset");
    let r = app
        .send_to(
            &host,
            Method::POST,
            "/admin/notifications/read-all",
            Some(&[("csrf_token", &token)]),
            &[("hx-request", "true"), ("referer", &referer)],
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    assert_eq!(
        r.headers.get("hx-redirect").and_then(|v| v.to_str().ok()),
        Some("/admin/content/pages/new")
    );
    app.cleanup().await;
}

/// `hostingos demo-reset` runs in another process: the server notices the
/// new generation and treats its visitors as after its own reset.
#[tokio::test]
async fn demo_server_follows_a_reset_run_elsewhere() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    assert!(
        !hos_web::demo::follow_external_reset(&app.state)
            .await
            .unwrap(),
        "nothing to follow after its own reset"
    );
    app.get("/").await;
    let before = app.state.demo.generation();
    platform::set_setting(
        app.pool(),
        app.tenant_id,
        "demo.generation",
        &json!(before + 1),
    )
    .await
    .unwrap();
    assert!(
        hos_web::demo::follow_external_reset(&app.state)
            .await
            .unwrap()
    );
    assert_eq!(app.state.demo.generation(), before + 1);
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(
        home.body.contains("Demo az önce sıfırlandı"),
        "notice on the next page"
    );
    app.cleanup().await;
}

/// Any host that reaches the app shows the demo, never "site not found".
#[tokio::test]
async fn demo_answers_on_unknown_hosts() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    let stray = format!("www.{}-elsewhere.test", app.slug);
    let r = app.send_to(&stray, Method::GET, "/", None, &[]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("demo-banner"));
    app.cleanup().await;
}

#[tokio::test]
async fn demo_logins_cannot_be_changed_or_locked() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    // Wrong passwords never lock the shared login (5 would, normally).
    for _ in 0..7 {
        let r = login(&mut app, (STAFF.0, "wrong-password")).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    }
    login_ok(&mut app, CLIENT).await;
    for path in [
        "/client/account/password",
        "/client/account/sessions/revoke-others",
        "/client/account/privacy/delete",
        "/auth/2fa/setup",
        "/auth/2fa/disable",
    ] {
        let r = post_bare(&mut app, path).await;
        assert!(is_refused(&r), "POST {path}: {} {}", r.status, r.body);
    }
    for path in ["/client/account/api-keys", "/client/account/users"] {
        let r = post_bare(&mut app, path).await;
        assert!(!is_refused(&r), "POST {path}: {} {}", r.status, r.body);
    }
    // Profile details stay editable (no e-mail change there).
    let page = app.get("/client/account").await;
    assert_eq!(page.status, StatusCode::OK);
    app.cookies.clear();
    login_ok(&mut app, STAFF).await;

    // The demo client's account status is protected; other clients' is not.
    let demo_user = identity::user_by_email(app.pool(), app.tenant_id, CLIENT.0)
        .await
        .unwrap()
        .unwrap();
    let demo_client = clients::clients_of_user(app.pool(), app.tenant_id, demo_user.id)
        .await
        .unwrap()
        .remove(0);
    let other = clients::create_client(
        app.pool(),
        app.tenant_id,
        &ClientInput::individual("Başka", "Müşteri", "baska@example.org", Currency::TRY),
    )
    .await
    .unwrap();
    let page = app.get(&format!("/admin/clients/{}", other.id)).await;
    let token = page.csrf();
    let r = app
        .post(
            &format!("/admin/clients/{}/status", demo_client.id),
            &[("csrf_token", &token), ("status", "closed")],
        )
        .await;
    assert!(is_refused(&r), "{} {}", r.status, r.body);
    let r = app
        .post(
            &format!("/admin/clients/{}/status", other.id),
            &[("csrf_token", &token), ("status", "inactive")],
        )
        .await;
    assert!(!is_refused(&r), "{} {}", r.status, r.body);

    // Stored addresses are masked (sessions, login history, audit).
    let ip: Option<String> = sqlx::query_scalar(
        "select host(ip) from sessions where tenant_id = $1 order by created_at desc limit 1",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(ip.as_deref(), Some("127.0.0.0"));
    app.cleanup().await;
}

/// The owner's server pages (updates, maintenance, server health) are
/// visible in the demo but nothing there can be changed; there is no
/// operator console.
#[tokio::test]
async fn demo_server_pages_are_read_only() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    for path in [
        "/platform",
        "/platform/login",
        "/platform/tenants",
        "/en/platform/login",
    ] {
        assert_eq!(app.get(path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    let page = app.get("/auth/login").await;
    assert!(
        !page.body.contains("platform@demo.test"),
        "no third demo account"
    );
    login_ok(&mut app, STAFF).await;
    for path in ["/admin/server", "/admin/server/updates"] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::OK, "GET {path}: {}", r.body);
        assert!(r.body.contains("demo-banner"), "banner on {path}");
    }
    for path in [
        "/admin/server/updates/check",
        "/admin/server/updates/apply",
        "/admin/server/updates/settings",
        "/admin/server/updates/maintenance/off",
    ] {
        let r = post_bare(&mut app, path).await;
        assert!(is_refused(&r), "POST {path}: {} {}", r.status, r.body);
    }
    app.cleanup().await;
}

#[tokio::test]
async fn demo_serves_storefront_and_admin_on_the_base_url_host() {
    let Some(mut app) = TestApp::demo_single_host().await else {
        return;
    };
    let host = app.host.clone();
    let domains: Vec<(String, bool, bool)> = sqlx::query_as(
        "select host, is_primary, verified_at is not null from tenant_domains \
         where tenant_id = $1 order by host",
    )
    .bind(app.tenant_id)
    .fetch_all(app.pool())
    .await
    .unwrap();
    assert_eq!(
        domains,
        vec![(host.clone(), true, true)],
        "only the public host"
    );

    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK, "{}", home.body);
    assert!(home.body.contains("demo-banner"));
    assert!(
        home.body.contains(&format!("href=\"https://{host}/")),
        "absolute links use the public https origin"
    );
    // A browser on https behind a proxy that reports its own plain-HTTP hop.
    let origin = format!("https://{host}");
    let page = app.get("/auth/login").await;
    let token = page.csrf();
    let r = app
        .send_to(
            &host,
            Method::POST,
            "/auth/login",
            Some(&[
                ("csrf_token", &token),
                ("email", STAFF.0),
                ("password", STAFF.1),
            ]),
            &[("origin", &origin), ("x-forwarded-proto", "http")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let cookie = r
        .headers
        .get_all(http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|c| c.starts_with("__Host-hos_session="))
        .expect("secure session cookie");
    assert!(cookie.contains("Secure") && !cookie.to_ascii_lowercase().contains("domain="));
    let admin = app.get("/admin").await;
    assert_eq!(admin.status, StatusCode::OK, "{}", admin.body);

    // One company: no operator console; the owner's server pages are in
    // the admin panel on the same host.
    assert_eq!(
        app.get("/platform/login").await.status,
        StatusCode::NOT_FOUND
    );
    let server = app.get("/admin/server").await;
    assert_eq!(server.status, StatusCode::OK, "{}", server.body);
    assert!(
        !server.body.contains("\"/platform"),
        "links point at /admin/server"
    );
    let token = server.csrf();
    // Another origin is still refused.
    let r = app
        .send_to(
            &host,
            Method::POST,
            "/auth/logout",
            Some(&[("csrf_token", &token)]),
            &[("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    app.cleanup().await;
}

#[tokio::test]
async fn demo_pages_carry_banner_noindex_and_login_hints() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK, "{}", home.body);
    assert!(home.body.contains("demo-banner"));
    assert!(
        home.body.contains("en fazla 30 dakika saklanır"),
        "Turkish banner"
    );
    assert!(
        home.body.contains("data-demo-reset-at=\""),
        "live countdown"
    );
    assert_eq!(
        home.headers
            .get("x-robots-tag")
            .and_then(|v| v.to_str().ok()),
        Some("noindex, nofollow, noarchive")
    );
    let en = app.get("/en").await;
    assert!(
        en.body.contains("kept for at most 30 minutes"),
        "English banner"
    );
    let robots = app.get("/robots.txt").await;
    assert!(
        robots.body.contains("User-agent: *\nDisallow: /"),
        "{}",
        robots.body
    );
    assert!(!robots.body.contains("Sitemap:"));
    let page = app.get("/auth/login").await;
    for needle in [
        STAFF.0,
        STAFF.1,
        CLIENT.0,
        CLIENT.1,
        "data-demo-fill=\"login-form\"",
    ] {
        assert!(page.body.contains(needle), "{needle}");
    }
    assert!(
        !page.body.contains("/auth/register"),
        "no sign-up link in the demo"
    );
    app.cleanup().await;
}

#[tokio::test]
async fn demo_mailbox_shows_captured_mail_with_masked_addresses() {
    let Some(mut app) = TestApp::demo().await else {
        return;
    };
    let tenant = app.tenant_id;
    let demo_user = identity::user_by_email(app.pool(), tenant, CLIENT.0)
        .await
        .unwrap()
        .unwrap();
    let demo_client = clients::clients_of_user(app.pool(), tenant, demo_user.id)
        .await
        .unwrap()
        .remove(0);
    for (to, client, subject) in [
        ("visitor.name@example.org", None, "Mail to a visitor"),
        (CLIENT.0, Some(demo_client.id), "Mail to the demo client"),
    ] {
        mail::log_email(
            app.pool(),
            tenant,
            &mail::NewEmailLog {
                client_id: client,
                to_address: to.into(),
                subject: subject.into(),
                body_text: Some(format!("Body of: {subject}")),
                ..mail::NewEmailLog::default()
            },
        )
        .await
        .unwrap();
    }
    // Anonymous visitors are sent to the login page.
    let r = app.get("/demo/mailbox").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    // Staff see every message, other visitors' addresses masked.
    login_ok(&mut app, STAFF).await;
    let r = app.get("/demo/mailbox").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Mail to a visitor") && r.body.contains("Mail to the demo client"));
    assert!(r.body.contains("v***@e***.org"), "{}", r.body);
    assert!(!r.body.contains("visitor.name@example.org"));
    // The demo client sees only its own mail.
    app.cookies.clear();
    login_ok(&mut app, CLIENT).await;
    let r = app.get("/demo/mailbox").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Mail to the demo client"));
    assert!(!r.body.contains("Mail to a visitor"));
    assert!(
        r.body.contains("demo-banner__link"),
        "mailbox link in the banner"
    );
    app.cleanup().await;
}

#[tokio::test]
async fn demo_registry_uses_only_mocks_and_the_log_mailer() {
    let Some(app) = TestApp::demo().await else {
        return;
    };
    let registry = app.state.billing.registry();
    assert!(registry.mock_only());
    let err = registry
        .save_integration(
            app.tenant_id,
            &hos_billing::registry::SaveIntegration {
                id: None,
                category: IntegrationCategory::Gateway,
                kind: "stripe".into(),
                name: "Real gateway".into(),
                config: json!({}),
                secrets: Some(json!({"secret_key": "sk_test_x"})),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect_err("real providers are refused");
    assert!(err.to_string().contains("disabled"), "{err}");
    assert_eq!(registry.mailer(app.tenant_id).await.kind(), "log");
    assert!(registry.ai(app.tenant_id).await.unwrap().is_none());
    // The demo's update runtime has no release source.
    assert!(matches!(app.state.updates.source(), Ok(None)));
    app.cleanup().await;
}

#[tokio::test]
async fn demo_mode_refuses_a_database_with_other_data() {
    let Some(app) = TestApp::demo().await else {
        return;
    };
    let mut cfg = app.state.web.demo.clone().unwrap();
    // The shared test database always holds other tenants (at least the
    // other tests'), so the check must refuse it …
    let other = TestApp::new().await.expect("second tenant");
    let err = hos_web::demo::check_database(app.pool(), &cfg)
        .await
        .expect_err("foreign data is refused");
    assert!(
        err.to_string().contains("HOS_DEMO_ALLOW_EXISTING_DATA"),
        "{err}"
    );
    // … unless the operator overrides it explicitly.
    cfg.allow_existing_data = true;
    hos_web::demo::check_database(app.pool(), &cfg)
        .await
        .expect("override");
    other.cleanup().await;
    app.cleanup().await;
}

/// A visitor who sends the headers of a form post and then trickles (here:
/// stalls) the body does not hold the reset back: the body is read in full
/// before the request enters the reset gate (`hos_web::server::limits`
/// runs outside `hos_web::demo::layer`). Before, the gate was taken first
/// and the reset waited 30 s and was then postponed.
#[tokio::test]
async fn demo_reset_is_not_held_up_by_a_slow_request_body() {
    use std::time::Duration;

    use futures::StreamExt as _;
    use tower::ServiceExt as _;

    let Some(app) = TestApp::demo().await else {
        return;
    };
    let (keep_open, rx) =
        tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::convert::Infallible>>(1);
    let first = futures::stream::once(async {
        Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b"csrf_token=x&name="))
    });
    let rest = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let req = http::Request::post("/admin/products/new")
        .header("host", &app.host)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from_stream(first.chain(rest)))
        .unwrap();
    let pending = tokio::spawn(app.router.clone().oneshot(req));
    // Let the request get as far into the stack as it can.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pending.is_finished(), "the request waits for its body");
    assert!(
        !app.state.demo.handling_requests(),
        "a request without its body holds the reset gate"
    );
    // So the reset does not wait for it (it would otherwise wait up to 30 s).
    // No time limit here: the reset's own duration depends on the shared
    // test database's load; the assertion above is the deterministic part.
    hos_web::demo::reset(&app.state).await.expect("demo reset");
    drop(keep_open);
    pending.abort();
    app.cleanup().await;
}
