//! WHMCS import in the web app: `import.manage` permission and CSRF on
//! `/admin/tools/import`, the wizard against a mocked WHMCS API (secrets
//! never rendered, dry run writes nothing, credentials can be wiped), tenant
//! isolation of runs, imported content rendered escaped, and logging in with
//! imported password hashes (bcrypt → re-hashed to argon2id, weak MD5 →
//! reset link instead of a session).
//!
//! Like the other web tests they need the database from `.env`; without it
//! they print a notice and pass.

use std::time::Duration;

use crate::common::{Res, TestApp};
use hos_core::crypto::SecretBox;
use hos_core::domain::UserKind;
use hos_core::net::OutboundPolicy;
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::import::{self as repo, NewImportRun};
use hos_import::config::{
    DbSourceConfig, ImportOptions, Mappings, SourceConfig, SourceSecrets, TlsMode,
};
use hos_import::entity::Entity;
use hos_import::row::Row;
use hos_import::runner::Runner;
use hos_import::source::Flavor;
use hos_import::source::fixture::FixtureSource;
use http::StatusCode;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "Correct-Horse-Battery-9";
/// The example of PHP's `password_hash()` / `password_verify()` manual page.
const PHP_BCRYPT: &str = "$2y$10$.vGA1O9wmRjrwAVXD98HNOgsNpDczlqm3Jq7KnEd1rVAGv3Fykk1a";
const PHP_BCRYPT_PASSWORD: &str = "rasmuslerdorf";
/// `md5("password")`: an unsalted legacy WHMCS hash.
const MD5_PASSWORD_HASH: &str = "5f4dcc3b5aa765d61d8327deb882cf99";
const XSS: &str = "<script>alert('x')</script><img src=x onerror=alert(1)>";
const API_SECRET: &str = "very-secret-api-value-7731";

async fn user(app: &TestApp, email: &str, kind: UserKind, role: Option<&str>) {
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
}

async fn login_with(app: &mut TestApp, email: &str, password: &str) -> Res {
    let token = app.get("/auth/login").await.csrf();
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

/// Logs in a fresh staff member with `role`; returns the CSRF token.
async fn staff(app: &mut TestApp, role: &str) -> String {
    app.cookies.clear();
    let email = format!("{role}-{}@staff.test", Uuid::now_v7().simple());
    user(app, &email, UserKind::Staff, Some(role)).await;
    let r = login_with(app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "login: {}", r.body);
    let dash = app.get("/admin").await;
    assert_eq!(dash.status, StatusCode::OK, "{}", dash.body);
    dash.csrf()
}

async fn post(app: &mut TestApp, path: &str, csrf: &str, fields: &[(&str, &str)]) -> Res {
    let mut all = vec![("csrf_token", csrf)];
    all.extend_from_slice(fields);
    app.post(path, &all).await
}

async fn runs(app: &TestApp) -> i64 {
    sqlx::query_scalar("select count(*) from import_runs where tenant_id = $1")
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn import_needs_the_permission_and_csrf() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // The built-in `admin` and `support` roles do not include import.manage.
    for role in ["admin", "support"] {
        let csrf = staff(&mut app, role).await;
        let dash = app.get("/admin").await;
        assert!(
            !dash.body.contains("/admin/tools/import"),
            "{role} sees the menu item"
        );
        assert_eq!(
            app.get("/admin/tools/import").await.status,
            StatusCode::FORBIDDEN,
            "{role}"
        );
        assert_eq!(
            app.get("/admin/tools/import/new").await.status,
            StatusCode::FORBIDDEN,
            "{role}"
        );
        let r = post(
            &mut app,
            "/admin/tools/import/new",
            &csrf,
            &[("kind", "api")],
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{role} POST");
    }
    assert_eq!(runs(&app).await, 0);

    // The owner may.
    let csrf = staff(&mut app, "owner").await;
    let dash = app.get("/admin").await;
    assert!(
        dash.body.contains("/admin/tools/import"),
        "owner menu item missing"
    );
    let index = app.get("/admin/tools/import").await;
    assert_eq!(index.status, StatusCode::OK, "{}", index.body);
    assert_eq!(
        app.get("/admin/tools/import/new?kind=api").await.status,
        StatusCode::OK
    );

    // No CSRF token → refused before the handler.
    let r = app
        .post(
            "/admin/tools/import/new",
            &[
                ("kind", "api"),
                ("url", "https://x.test/"),
                ("identifier", "a"),
                ("api_secret", "b"),
            ],
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "missing CSRF: {}",
        r.status
    );
    // Missing fields → the form again, nothing stored.
    let r = post(
        &mut app,
        "/admin/tools/import/new",
        &csrf,
        &[("kind", "api")],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(runs(&app).await, 0);
    app.cleanup().await;
}

/// A WHMCS API that answers every action with an empty success.
async fn whmcs_api() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/includes/api.php"))
        .and(body_string_contains("action=WhmcsDetails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "result": "success",
            "whmcs": { "version": "8.10.1", "canonicalversion": "8.10.1-release.1" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/includes/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "result": "success",
            "totalresults": 0,
            "startnumber": 0,
            "numreturned": 0
        })))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn wizard_connects_previews_and_never_shows_secrets() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let server = whmcs_api().await;
    let csrf = staff(&mut app, "owner").await;
    let url = format!("{}/", server.uri());
    let r = post(
        &mut app,
        "/admin/tools/import/new",
        &csrf,
        &[
            ("kind", "api"),
            ("url", &url),
            ("identifier", "import-reader"),
            ("api_secret", API_SECRET),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let run_path = r.location().to_owned();
    assert!(run_path.contains("/admin/tools/import/"), "{run_path}");
    let id: Uuid = run_path
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .expect("run id");

    // Sealed, not stored in clear; never rendered back.
    let sealed: Vec<u8> =
        sqlx::query_scalar("select secrets_enc from import_runs where tenant_id = $1 and id = $2")
            .bind(app.tenant_id)
            .bind(id)
            .fetch_one(app.pool())
            .await
            .unwrap();
    assert!(
        !sealed
            .windows(API_SECRET.len())
            .any(|w| w == API_SECRET.as_bytes())
    );
    let page = app.get(&run_path).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("8.10.1"), "probe version missing");
    assert!(!page.body.contains(API_SECRET));
    let audit: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where tenant_id = $1 and action = 'import.create'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(audit, 1);

    // Preview (dry run) through the wizard: runs in the web process, writes nothing.
    let r = post(
        &mut app,
        &format!("/admin/tools/import/{id}/options"),
        &csrf,
        &[
            ("entity", "clients"),
            ("entity", "invoices"),
            ("grace_days", "10"),
            ("action", "preview"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let mut row = None;
    for _ in 0..150 {
        let r = repo::get_run(app.pool(), app.tenant_id, id)
            .await
            .unwrap()
            .unwrap();
        if !repo::ACTIVE_STATUSES.contains(&r.status.as_str()) {
            row = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let row = row.expect("preview finished");
    assert_eq!(row.status, "draft", "error: {:?}", row.error);
    assert!(row.dry_run_at.is_some());
    assert!(
        row.has_secrets,
        "a finished preview keeps the credentials for the import"
    );
    let clients: i64 = sqlx::query_scalar("select count(*) from clients where tenant_id = $1")
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .unwrap();
    assert_eq!(clients, 0);
    let page = app.get(&run_path).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(
        page.body.contains("name=\"confirm_cutover\""),
        "start form missing"
    );
    assert!(!page.body.contains(API_SECRET));
    let progress = app.get(&format!("{run_path}/progress")).await;
    assert_eq!(progress.status, StatusCode::OK);
    let csv = app.get(&format!("{run_path}/report.csv")).await;
    assert_eq!(csv.status, StatusCode::OK);
    assert!(csv.content_type().starts_with("text/csv"));

    // Starting needs the explicit cut-over confirmation.
    let r = post(
        &mut app,
        &format!("/admin/tools/import/{id}/options"),
        &csrf,
        &[("entity", "clients"), ("action", "start")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let still = repo::get_run(app.pool(), app.tenant_id, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.status, "draft");

    // Wiping the credentials.
    let r = post(
        &mut app,
        &format!("/admin/tools/import/{id}/forget"),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let wiped = repo::get_run(app.pool(), app.tenant_id, id)
        .await
        .unwrap()
        .unwrap();
    assert!(!wiped.has_secrets);
    app.cleanup().await;
}

#[tokio::test]
async fn runs_of_another_tenant_are_invisible() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let Some(other) = TestApp::new().await else {
        return;
    };
    let foreign = repo::create_run(
        other.pool(),
        other.tenant_id,
        &NewImportRun {
            id: Uuid::now_v7(),
            source_kind: "whmcs_api".into(),
            source_key: "whmcs-api:other.test".into(),
            source_label: "other.test".into(),
            source_config: serde_json::json!({}),
            options: serde_json::json!({}),
            started_by: None,
        },
    )
    .await
    .unwrap();
    let csrf = staff(&mut app, "owner").await;
    let base = format!("/admin/tools/import/{}", foreign.id);
    assert_eq!(app.get(&base).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.get(&format!("{base}/report.json")).await.status,
        StatusCode::NOT_FOUND
    );
    for action in ["cancel", "rollback", "forget"] {
        let r = post(
            &mut app,
            &format!("{base}/{action}"),
            &csrf,
            &[("confirm", "1")],
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{action}");
    }
    let index = app.get("/admin/tools/import").await;
    assert!(!index.body.contains("other.test"));
    other.cleanup().await;
    app.cleanup().await;
}

fn r() -> Row {
    Row::new()
}

/// Imports a small WHMCS fixture into the test tenant (database flavour).
async fn import_fixture(app: &TestApp) -> Uuid {
    let source = FixtureSource::new(Flavor::Database)
        .with_setting("Version", "8.10.1-release.1")
        .with(
            Entity::Currencies,
            vec![
                r().with("id", 1)
                    .with("code", "TRY")
                    .with("prefix", "")
                    .with("suffix", " TL")
                    .with("rate", "1.00000")
                    .with("default", 1),
            ],
        )
        .with(
            Entity::Clients,
            vec![
                r().with("id", 1)
                    .with("firstname", "Ayşe")
                    .with("lastname", "Yılmaz")
                    .with("companyname", "")
                    .with("email", "ayse@import.test")
                    .with("country", "TR")
                    .with("currency", 1)
                    .with("status", "Active")
                    .with("credit", "0.00")
                    .with("datecreated", "2020-01-05"),
                r().with("id", 2)
                    .with("firstname", XSS)
                    .with("lastname", "Doe")
                    .with("companyname", XSS)
                    .with("email", "john@import.test")
                    .with("country", "GB")
                    .with("currency", 1)
                    .with("status", "Active")
                    .with("credit", "0.00")
                    .with("datecreated", "2021-03-01")
                    .with("notes", XSS),
            ],
        )
        .with(
            Entity::Users,
            vec![
                r().with("id", 10)
                    .with("first_name", "Ayşe")
                    .with("last_name", "Yılmaz")
                    .with("email", "ayse@import.test")
                    .with("password", PHP_BCRYPT)
                    .with("created_at", "2020-01-05 10:00:00"),
                r().with("id", 11)
                    .with("first_name", "John")
                    .with("last_name", "Doe")
                    .with("email", "john@import.test")
                    .with("password", MD5_PASSWORD_HASH)
                    .with("created_at", "2021-03-01 10:00:00"),
            ],
        )
        .with(
            Entity::UserLinks,
            vec![
                r().with("id", 1)
                    .with("auth_user_id", 10)
                    .with("client_id", 1)
                    .with("owner", 1)
                    .with("permissions", ""),
                r().with("id", 2)
                    .with("auth_user_id", 11)
                    .with("client_id", 2)
                    .with("owner", 1)
                    .with("permissions", ""),
            ],
        );
    let config = SourceConfig::WhmcsDb(DbSourceConfig {
        host: "whmcs-db.example.net".into(),
        port: 3306,
        database: "whmcs".into(),
        user: "reader".into(),
        tls: TlsMode::Disabled,
        attachments_api: None,
    });
    let options = ImportOptions {
        entities: [
            Entity::Currencies,
            Entity::Clients,
            Entity::Users,
            Entity::UserLinks,
        ]
        .into_iter()
        .collect(),
        pause_automation: false,
        ..ImportOptions::default()
    };
    let secret_box: SecretBox = app.state.secret_box.clone();
    let id = Uuid::now_v7();
    repo::create_run(
        app.pool(),
        app.tenant_id,
        &NewImportRun {
            id,
            source_kind: config.kind().into(),
            source_key: config.key(),
            source_label: config.label(),
            source_config: serde_json::to_value(&config).unwrap(),
            options: serde_json::to_value(&options).unwrap(),
            started_by: None,
        },
    )
    .await
    .unwrap();
    let sealed =
        hos_import::crypto::seal_secrets(&secret_box, app.tenant_id, id, &SourceSecrets::default())
            .unwrap();
    repo::set_run_secrets(app.pool(), app.tenant_id, id, &sealed)
        .await
        .unwrap();
    repo::update_draft(
        app.pool(),
        app.tenant_id,
        id,
        None,
        Some(&serde_json::to_value(Mappings::default()).unwrap()),
        None,
    )
    .await
    .unwrap();
    let runner = Runner::new(
        app.pool().clone(),
        secret_box,
        OutboundPolicy::Unrestricted,
        "web-test",
    );
    let row = runner
        .queue_and_run(app.tenant_id, id, "import", None, Some(Box::new(source)))
        .await
        .expect("import")
        .expect("queued");
    assert_eq!(row.status, "completed", "{:?}", row.error);
    id
}

async fn password_hash(app: &TestApp, email: &str) -> String {
    sqlx::query_scalar("select password_hash from users where tenant_id = $1 and email = $2")
        .bind(app.tenant_id)
        .bind(email)
        .fetch_one(app.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn imported_accounts_log_in_and_content_is_escaped() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let run = import_fixture(&app).await;

    // Imported bcrypt hash: the PHP password works and is re-hashed.
    assert_eq!(password_hash(&app, "ayse@import.test").await, PHP_BCRYPT);
    let r = login_with(&mut app, "ayse@import.test", "wrong-password").await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = login_with(&mut app, "ayse@import.test", PHP_BCRYPT_PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        password_hash(&app, "ayse@import.test")
            .await
            .starts_with("$argon2id$")
    );
    app.cookies.clear();
    let r = login_with(&mut app, "ayse@import.test", PHP_BCRYPT_PASSWORD).await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "argon2 login after re-hash"
    );
    app.cookies.clear();

    // Weak MD5: the right password opens no session but mails a reset link.
    let r = login_with(&mut app, "john@import.test", "password").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(
        !app.cookies.keys().any(|k| k.contains("session")),
        "{:?}",
        app.cookies
    );
    let resets: i64 = sqlx::query_scalar(
        "select count(*) from password_resets p join users u on u.id = p.user_id
          where p.tenant_id = $1 and u.email = 'john@import.test'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(resets, 1);
    assert!(
        !password_hash(&app, "john@import.test")
            .await
            .starts_with("$argon2")
    );
    let r = login_with(&mut app, "john@import.test", "not-the-password").await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Imported markup is shown as text in the admin panel.
    staff(&mut app, "owner").await;
    let client_id: Uuid = sqlx::query_scalar(
        "select id from clients where tenant_id = $1 and email = 'john@import.test'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    for page in [
        "/admin/clients".to_owned(),
        format!("/admin/clients/{client_id}"),
    ] {
        let r = app.get(&page).await;
        assert_eq!(r.status, StatusCode::OK, "{page}");
        assert!(
            !r.body.contains("<script>alert"),
            "{page} renders imported script"
        );
        assert!(
            !r.body.contains("<img src=x"),
            "{page} renders imported markup"
        );
    }
    let index = app.get("/admin/tools/import").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("whmcs-db.example.net"));
    let page = app.get(&format!("/admin/tools/import/{run}")).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(
        page.body
            .contains(&format!("/admin/tools/import/{run}/rollback")),
        "rollback form missing"
    );
    assert!(page.body.contains("report.csv"));
    app.cleanup().await;
}

/// Logs in a fresh staff member holding a custom role with `grants`.
async fn staff_with(app: &mut TestApp, slug: &str, grants: &[&str]) -> String {
    let role = identity::create_role(app.pool(), app.tenant_id, slug, slug, None, false)
        .await
        .unwrap();
    let mut conn = app.pool().acquire().await.unwrap();
    let grants: Vec<String> = grants.iter().map(|g| (*g).to_owned()).collect();
    identity::set_role_permissions(&mut conn, app.tenant_id, role.id, &grants)
        .await
        .unwrap();
    drop(conn);
    app.cookies.clear();
    let email = format!("{slug}-{}@staff.test", Uuid::now_v7().simple());
    user(app, &email, UserKind::Staff, Some(slug)).await;
    let r = login_with(app, &email, PASSWORD).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "login: {}", r.body);
    app.get("/admin").await.csrf()
}

async fn draft_run(app: &TestApp) -> Uuid {
    repo::create_run(
        app.pool(),
        app.tenant_id,
        &NewImportRun {
            id: Uuid::now_v7(),
            source_kind: "whmcs_api".into(),
            source_key: format!("whmcs-api:{}.test", Uuid::now_v7().simple()),
            source_label: "roles.test".into(),
            source_config: serde_json::json!({}),
            options: serde_json::to_value(ImportOptions::default()).unwrap(),
            started_by: None,
        },
    )
    .await
    .unwrap()
    .id
}

async fn mapped_role(app: &TestApp, run: Uuid) -> Option<String> {
    let m: serde_json::Value =
        sqlx::query_scalar("select mappings from import_runs where tenant_id = $1 and id = $2")
            .bind(app.tenant_id)
            .bind(run)
            .fetch_one(app.pool())
            .await
            .unwrap();
    m["roles"]["1"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn role_mappings_follow_the_staff_escalation_rule() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let save = |role: &'static str| {
        vec![
            ("action", "save"),
            ("map_role:1", role),
            ("entity", "staff"),
        ]
    };
    // A role that every staff manager below may hand out.
    let helper = identity::create_role(app.pool(), app.tenant_id, "helper", "Helper", None, false)
        .await
        .unwrap();
    let mut conn = app.pool().acquire().await.unwrap();
    identity::set_role_permissions(
        &mut conn,
        app.tenant_id,
        helper.id,
        &["support.view".to_owned()],
    )
    .await
    .unwrap();
    drop(conn);

    // import.manage alone: no role mapping at all.
    let csrf = staff_with(&mut app, "importer", &["import.manage", "support.view"]).await;
    let run = draft_run(&app).await;
    let path = format!("/admin/tools/import/{run}/options");
    let r = post(&mut app, &path, &csrf, &save("helper")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(mapped_role(&app, run).await, None, "needs staff.manage");

    // staff.manage, but not every permission of the mapped role.
    let csrf = staff_with(
        &mut app,
        "lead",
        &["import.manage", "staff.manage", "support.view"],
    )
    .await;
    let r = post(&mut app, &path, &csrf, &save("admin")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(mapped_role(&app, run).await, None, "no escalation");
    let r = post(&mut app, &path, &csrf, &save("helper")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(
        mapped_role(&app, run).await.as_deref(),
        Some("helper"),
        "a covered role is fine"
    );

    // Even the owner cannot map the owner role.
    let csrf = staff(&mut app, "owner").await;
    let r = post(&mut app, &path, &csrf, &save("owner")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(mapped_role(&app, run).await.as_deref(), Some("helper"));
    let page = app.get(&format!("/admin/tools/import/{run}")).await;
    assert_eq!(page.status, StatusCode::OK);

    app.cleanup().await;
}

/// A re-import into a shop that has been live since the last import needs
/// the extra `confirm_live` tick; without live activity it does not.
#[tokio::test]
async fn reimport_into_a_live_shop_needs_confirmation() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    import_fixture(&app).await;
    let csrf = staff(&mut app, "owner").await;
    let run = draft_run(&app).await;
    sqlx::query("update import_runs set dry_run_at = now() where tenant_id = $1 and id = $2")
        .bind(app.tenant_id)
        .bind(run)
        .execute(app.pool())
        .await
        .unwrap();
    let page = format!("/admin/tools/import/{run}");
    let path = format!("{page}/options");
    let start = |live: bool| {
        let mut f = vec![("action", "start"), ("confirm_cutover", "1")];
        if live {
            f.push(("confirm_live", "1"));
        }
        f
    };
    let locale = hos_core::domain::Locale::Tr;
    let required = app.state.i18n.t(locale, "import.run.confirm_live_required");
    let gone = app.state.i18n.t(locale, "import.run.secrets_gone");

    // Nothing happened since the import: no extra question (the draft has
    // no credentials, so it stops right after the gate).
    assert!(!app.get(&page).await.body.contains("confirm_live"));
    let r = post(&mut app, &path, &csrf, &start(false)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let body = app.get(&page).await.body;
    assert!(body.contains(&gone) && !body.contains(&required));

    // The shop goes live: staff add credit to a client in HostingOS.
    let client: Uuid = sqlx::query_scalar(
        "select id from clients where tenant_id = $1 and email = 'ayse@import.test'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    sqlx::query(
        "insert into credits (tenant_id, client_id, amount, currency, kind)
         values ($1, $2, 5, 'TRY', 'manual')",
    )
    .bind(app.tenant_id)
    .bind(client)
    .execute(app.pool())
    .await
    .unwrap();
    assert!(app.get(&page).await.body.contains("name=\"confirm_live\""));
    let r = post(&mut app, &path, &csrf, &start(false)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let body = app.get(&page).await.body;
    assert!(body.contains(&required), "refused without the tick");
    let r = post(&mut app, &path, &csrf, &start(true)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let body = app.get(&page).await.body;
    assert!(
        body.contains(&gone) && !body.contains(&required),
        "past the gate"
    );
    app.cleanup().await;
}
