//! End-to-end tests of the router (tenant resolution, CSRF, auth flows,
//! area guards, storefront SEO output) against the real database.

use crate::common::TestApp;
use http::{Method, StatusCode};

const PASSWORD: &str = "Kj8#vQ2!pLm9zRw4";

fn ld_types(values: &[serde_json::Value]) -> Vec<String> {
    let mut out = Vec::new();
    for v in values {
        let items = v
            .get("@graph")
            .and_then(|g| g.as_array())
            .cloned()
            .unwrap_or_else(|| vec![v.clone()]);
        for item in items {
            if let Some(t) = item.get("@type").and_then(|t| t.as_str()) {
                out.push(t.to_owned());
            }
        }
    }
    out
}

#[tokio::test]
async fn tenant_resolution_by_host() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(
        home.body.contains("Web Test Hosting"),
        "tenant name rendered"
    );

    let unknown = app
        .send_to("no-such-tenant.invalid", Method::GET, "/", None, &[])
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(unknown.body.contains("Site not found"));

    // The platform console needs no tenant.
    let platform = app
        .send_to(
            "no-such-tenant.invalid",
            Method::GET,
            "/platform/login",
            None,
            &[],
        )
        .await;
    assert_eq!(platform.status, StatusCode::OK);

    let health = app.get("/healthz").await;
    assert_eq!(health.status, StatusCode::OK);
    assert!(health.content_type().starts_with("application/json"));
    app.cleanup().await;
}

#[tokio::test]
async fn storefront_meta_json_ld_and_locale_prefix() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("<html lang=\"tr\""));
    assert!(home.body.contains("<link rel=\"canonical\""));
    assert!(home.body.contains("<meta name=\"description\""));
    assert!(home.body.contains("property=\"og:title\""));
    assert!(home.body.contains("hreflang=\"en\""));
    let types = ld_types(&home.json_ld());
    assert!(types.iter().any(|t| t == "Organization"), "{types:?}");
    assert!(types.iter().any(|t| t == "WebSite"), "{types:?}");
    let csp = home
        .headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(csp.contains("script-src 'self'"), "{csp}");

    // Non-default locale: prefixed, English content.
    let en = app.get("/en/").await;
    assert_eq!(en.status, StatusCode::OK);
    assert!(en.body.contains("<html lang=\"en\""));
    let en_login = app.get("/en/auth/login").await;
    assert!(en_login.body.contains("Sign in"));
    // Default locale prefix redirects to the bare path.
    let tr = app.get("/tr/kb?q=x").await;
    assert_eq!(tr.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(tr.location(), "/kb?q=x");

    for path in [
        "/kb",
        "/blog",
        "/domains?q=ornek",
        "/contact",
        "/announcements",
    ] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
    }
    let missing = app.get("/no-such-page").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(missing.body.contains("data-error=\"not_found\""));
    app.cleanup().await;
}

#[tokio::test]
async fn sitemap_robots_and_llms() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let sitemap = app.get("/sitemap.xml").await;
    assert_eq!(sitemap.status, StatusCode::OK);
    assert!(sitemap.content_type().contains("xml"));
    assert!(sitemap.body.contains("<loc>"));
    assert!(sitemap.body.contains("hreflang"));

    let robots = app.get("/robots.txt").await;
    assert_eq!(robots.status, StatusCode::OK);
    assert!(robots.body.contains("Disallow: /admin/"));
    assert!(robots.body.contains("Sitemap: "));

    let llms = app.get("/llms.txt").await;
    assert_eq!(llms.status, StatusCode::OK);
    assert!(llms.body.starts_with("# "));
    let full = app.get("/llms-full.txt").await;
    assert_eq!(full.status, StatusCode::OK);
    app.cleanup().await;
}

#[tokio::test]
async fn csrf_is_required_for_posts() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let r = app
        .post(
            "/auth/login",
            &[("email", "a@example.com"), ("password", "x")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // A token from the page works; a forged one does not.
    let page = app.get("/auth/login").await;
    let token = page.csrf();
    assert!(!token.is_empty());
    let forged = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", "forged"),
                ("email", "a@example.com"),
                ("password", "x"),
            ],
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let host = app.host.clone();
    let fields = [
        ("csrf_token", token.as_str()),
        ("email", "a@example.com"),
        ("password", "x"),
    ];
    let cross_site = app
        .send_to(
            &host,
            Method::POST,
            "/auth/login",
            Some(&fields[..]),
            &[("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(cross_site.status, StatusCode::FORBIDDEN);
    let ok = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", &token),
                ("email", "a@example.com"),
                ("password", "wrong"),
            ],
        )
        .await;
    assert_eq!(
        ok.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad credentials re-render the form"
    );
    app.cleanup().await;
}

#[tokio::test]
async fn register_login_logout_and_area_guards() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("auth.require_email_verification", serde_json::json!(false))
        .await;

    // Anonymous: /admin and /client redirect to the login page.
    let admin = app.get("/admin").await;
    assert_eq!(admin.status, StatusCode::SEE_OTHER);
    assert!(
        admin.location().starts_with("/auth/login"),
        "{}",
        admin.location()
    );
    let client = app.get("/client").await;
    assert_eq!(client.status, StatusCode::SEE_OTHER);

    // Register (logs in directly when verification is off).
    let email = format!("{}@example.com", app.slug);
    let form = app.get("/auth/register").await;
    assert_eq!(form.status, StatusCode::OK);
    let token = form.csrf();
    let reg = app
        .post(
            "/auth/register",
            &[
                ("csrf_token", &token),
                ("first_name", "Ayşe"),
                ("last_name", "Test"),
                ("email", &email),
                ("password", PASSWORD),
                ("terms", "1"),
            ],
        )
        .await;
    assert_eq!(reg.status, StatusCode::SEE_OTHER, "{}", reg.body);
    assert_eq!(reg.location(), "/client");
    let dash = app.get("/client").await;
    assert_eq!(dash.status, StatusCode::OK);
    assert!(dash.body.contains("Ayşe"));

    // A client cannot open the admin panel.
    let forbidden = app.get("/admin").await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);

    // Logout ends the session.
    let token = dash.csrf();
    let out = app.post("/auth/logout", &[("csrf_token", &token)]).await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    let after = app.get("/client").await;
    assert_eq!(after.status, StatusCode::SEE_OTHER);

    // Log back in with the password.
    let login = app.get("/auth/login").await;
    let token = login.csrf();
    let wrong = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", &token),
                ("email", &email),
                ("password", "not-the-password"),
            ],
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    let ok = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", &token),
                ("email", &email),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER, "{}", ok.body);
    assert_eq!(ok.location(), "/client");
    assert_eq!(app.get("/client").await.status, StatusCode::OK);

    // API: session principal works, anonymous gets problem+json.
    let me = app.get("/api/v1/me").await;
    assert_eq!(me.status, StatusCode::OK);
    app.cookies.clear();
    let anon = app.get("/api/v1/me").await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    assert!(anon.content_type().starts_with("application/problem+json"));
    app.cleanup().await;
}

#[tokio::test]
async fn registration_does_not_reveal_existing_accounts() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // Default: verification required → same answer for new and taken e-mails.
    let email = format!("dup-{}@example.com", app.slug);
    let mut locations = Vec::new();
    for _ in 0..2 {
        let form = app.get("/auth/register").await;
        let token = form.csrf();
        let r = app
            .post(
                "/auth/register",
                &[
                    ("csrf_token", &token),
                    ("first_name", "Mehmet"),
                    ("last_name", "Test"),
                    ("email", &email),
                    ("password", PASSWORD),
                    ("terms", "1"),
                ],
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        locations.push(r.location().to_owned());
    }
    assert_eq!(locations[0], "/auth/verify");
    assert_eq!(locations[0], locations[1]);
    app.cleanup().await;
}
