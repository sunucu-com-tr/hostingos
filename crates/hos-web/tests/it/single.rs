//! Single-company mode (`HOS_MODE=single`, the default; `hos_web::mode`):
//! one company on every host, no `/platform`, the owner's server pages in
//! the admin panel, no plan limits. Each test's company is its own
//! throw-away tenant ([`TestApp::single`]); the shared database itself holds
//! many, which is why the tests name the company instead of letting the
//! server find it.

use hos_core::domain::{Locale, UserKind};
use hos_db::repo::identity;
use http::{Method, StatusCode};

use crate::common::TestApp;

const PASSWORD: &str = "Single-company-test-1";

async fn staff_login(app: &mut TestApp, email: &str, role: &str) {
    let user = identity::create_user(
        app.pool(),
        app.tenant_id,
        &identity::NewUser {
            email: email.to_owned(),
            password_hash: Some(hos_auth::password::hash_password(PASSWORD).unwrap()),
            kind: UserKind::Staff,
            first_name: "Test".into(),
            last_name: role.into(),
            phone: None,
            locale: Locale::En,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    let role = identity::role_by_slug(app.pool(), app.tenant_id, role)
        .await
        .unwrap()
        .expect("role");
    identity::assign_role(app.pool(), app.tenant_id, user.id, role.id)
        .await
        .unwrap();
    app.cookies.clear();
    let page = app.get("/auth/login").await;
    let csrf = page.csrf();
    let r = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", &csrf),
                ("email", email),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
}

#[tokio::test]
async fn one_company_on_every_host_and_no_console() {
    let Some(mut app) = TestApp::single().await else {
        return;
    };
    for host in ["anything.example", "203.0.113.5:8080", "localhost"] {
        let r = app.send_to(host, Method::GET, "/", None, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{host}: {}", r.body);
        assert!(!r.body.contains("Site not found"), "{host}");
    }
    for path in [
        "/platform",
        "/platform/login",
        "/platform/signup",
        "/platform/tenants",
        "/en/platform/login",
    ] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}: {}", r.body);
    }
    // Plans and their limits belong to multi-company operation.
    for limit in [
        hos_web::routes::platform::limits::Limit::Clients,
        hos_web::routes::platform::limits::Limit::Staff,
    ] {
        let status =
            hos_web::routes::platform::limits::plan_limit_status(&app.state, app.tenant_id, limit)
                .await
                .unwrap();
        assert_eq!(status.max, None, "{limit:?} is not limited");
    }
    app.cleanup().await;
}

#[tokio::test]
async fn owner_reaches_server_pages_with_reauthentication_others_do_not() {
    let Some(mut app) = TestApp::single().await else {
        return;
    };
    // Not signed in: the login page, never the operator console's.
    let r = app.get("/admin/server/updates").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("/auth/login"), "{}", r.location());

    // An administrator who is not the owner: no server pages, no menu entry.
    let admin_email = format!("admin-{}@example.org", app.slug);
    staff_login(&mut app, &admin_email, "admin").await;
    let dash = app.get("/admin").await;
    assert!(!dash.body.contains("admin/server\""), "menu entry hidden");
    for path in ["/admin/server", "/admin/server/updates"] {
        assert_eq!(app.get(path).await.status, StatusCode::FORBIDDEN, "{path}");
    }

    // The owner.
    let owner_email = format!("owner-{}@example.org", app.slug);
    staff_login(&mut app, &owner_email, "owner").await;
    let dash = app.get("/admin").await;
    assert!(
        dash.body.contains("admin/server\""),
        "menu entry for the owner"
    );
    for path in ["/admin/server", "/admin/server/updates"] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::OK, "GET {path}: {}", r.body);
        assert!(
            !r.body.contains("=\"/platform") && !r.body.contains("=\"/en/platform"),
            "links stay in the admin panel"
        );
        assert!(
            !r.body.contains("%2Fplatform"),
            "the language switch returns to the same page"
        );
    }
    for path in [
        "/admin/server/tenants",
        "/admin/server/plans",
        "/admin/server/jobs",
    ] {
        assert_eq!(app.get(path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    // Applying an update needs the owner's own password again.
    let page = app.get("/admin/server/updates").await;
    let csrf = page.csrf();
    let r = app
        .post(
            "/admin/server/updates/apply",
            &[
                ("csrf_token", &csrf),
                ("version", "999.0.0"),
                ("password", "not-the-password"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.location(), "/admin/server/updates");
    let logged: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where tenant_id = $1
           and action = 'server.update.reauth_failed' and actor_kind = 'user' and changes -> 'by' ->> 'email' = $2",
    )
    .bind(app.tenant_id)
    .bind(&owner_email)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(
        logged, 1,
        "the failed attempt is audited with the owner's name"
    );

    // During an update's maintenance mode visitors get 503, the owner keeps
    // the server pages and the login.
    app.state.updates.force_maintenance(Some(true));
    assert_eq!(app.get("/").await.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        app.get("/admin/clients").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    for path in ["/admin/server", "/admin/server/updates"] {
        assert_eq!(
            app.get(path).await.status,
            StatusCode::OK,
            "{path} during maintenance"
        );
    }
    for path in ["/en/admin/server/updates", "/auth/login"] {
        let r = app.get(path).await;
        assert_ne!(
            r.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} during maintenance"
        );
    }
    app.state.updates.force_maintenance(None);
    app.cleanup().await;
}

#[tokio::test]
async fn multi_company_mode_is_unchanged() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let r = app
        .send_to("unknown-host.example", Method::GET, "/", None, &[])
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "unknown hosts stay unknown"
    );
    let r = app.get("/admin/server").await;
    assert_ne!(
        r.status,
        StatusCode::OK,
        "no server area in multi-company mode"
    );
    app.cleanup().await;
}
