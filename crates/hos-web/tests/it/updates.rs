//! Self-update console (`/platform/updates`, docs/UPDATES.md): access
//! control, CSRF, re-authentication, the update check against a wiremock
//! GitHub release API, the deploy webhook and maintenance mode.
//!
//! The update tables hold one cluster-wide row (`update_state`), so the
//! tests that touch them run one at a time ([`LOCK`]), snapshot the row
//! first and restore it afterwards. Maintenance mode is only ever forced on
//! the test's own `AppState`, never in the shared database.

use crate::common::{self, TestApp};
use hos_auth::totp::{TotpSecret, code_at};
use hos_core::domain::UserKind;
use hos_db::repo::{identity, platform, saas};
use hos_update::fetch::NetConfig;
use hos_update::install::{BinaryInstall, ContainerKind, InstallKind};
use hos_update::keys::{KeyOrigin, KeyRing, TrustedKey};
use hos_update::source::Source;
use hos_web::routes::platform::login;
use http::{Method, StatusCode};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const PASSWORD: &str = "Update-Test-Password-73";
/// Versions far ahead of any real release, cleaned up by [`Snapshot::restore`].
const V_GOOD: &str = "900.0.0";
const V_BAD: &str = "900.1.0";

fn console_host(app: &TestApp) -> String {
    let u = &app.state.config.base_url;
    match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or("localhost")),
        None => u.host_str().unwrap_or("localhost").to_owned(),
    }
}

async fn get(app: &mut TestApp, path: &str) -> common::Res {
    let host = console_host(app);
    app.send_to(&host, Method::GET, path, None, &[]).await
}

async fn post(app: &mut TestApp, path: &str, form: &[(&str, &str)]) -> common::Res {
    let host = console_host(app);
    app.send_to(&host, Method::POST, path, Some(form), &[])
        .await
}

/// A platform admin (optionally with TOTP), signed in; returns its e-mail,
/// TOTP secret and a CSRF token of the console.
async fn admin(app: &mut TestApp, totp: bool) -> (String, Option<TotpSecret>, String) {
    let email = format!("upd-{}@example.com", uuid::Uuid::now_v7().simple());
    assert!(
        hos_web::cli::create_platform_admin(app.pool(), &email, "Update Test", PASSWORD)
            .await
            .expect("admin")
    );
    let row = platform::platform_admin_by_email(app.pool(), &email)
        .await
        .expect("query")
        .expect("admin");
    let secret = if totp {
        let secret = TotpSecret::generate();
        let enc = app
            .state
            .secret_box
            .encrypt(secret.as_bytes(), &login::totp_aad(row.id))
            .expect("encrypt");
        saas::set_platform_admin_totp(app.pool(), row.id, Some(&enc), true, &[])
            .await
            .expect("totp");
        Some(secret)
    } else {
        None
    };
    let page = get(app, "/platform/login").await;
    let csrf = page.csrf();
    let r = post(
        app,
        "/platform/login",
        &[
            ("csrf_token", &csrf),
            ("email", &email),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    if let Some(secret) = &secret {
        assert_eq!(r.location(), "/platform/login/2fa");
        let page = get(app, "/platform/login/2fa").await;
        let csrf = page.csrf();
        let r = post(
            app,
            "/platform/login/2fa",
            &[("csrf_token", &csrf), ("code", &totp_now(secret))],
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    }
    let page = get(app, "/platform/updates").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let csrf = page.csrf();
    (email, secret, csrf)
}

fn totp_now(secret: &TotpSecret) -> String {
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap();
    code_at(secret, now).expect("code")
}

async fn drop_admin(app: &TestApp, email: &str) {
    sqlx::query("delete from platform_admins where email = $1")
        .bind(email)
        .execute(app.pool())
        .await
        .expect("delete admin");
}

/// Creates a tenant staff member (role `owner`) with `email` on `app`'s
/// tenant and signs it in (a fresh cookie jar).
async fn tenant_owner_login(app: &mut TestApp, email: &str) {
    let user = identity::create_user(
        app.pool(),
        app.tenant_id,
        &identity::NewUser {
            email: email.to_owned(),
            password_hash: Some(hos_auth::password::hash_password(PASSWORD).unwrap()),
            kind: UserKind::Staff,
            first_name: "Tenant".into(),
            last_name: "Owner".into(),
            phone: None,
            locale: hos_core::domain::Locale::En,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    let role = identity::role_by_slug(app.pool(), app.tenant_id, "owner")
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

/// The cluster-wide update state before a test.
/// The cluster-wide update state before a test; restored by
/// [`Snapshot::restore`], or on drop when the test panicked.
struct Snapshot {
    row: serde_json::Value,
    restored: bool,
}

/// Puts `row` back and removes everything the tests created (`900.*`
/// releases / runs / notices, `upd-*` operators).
async fn restore_rows(pool: &hos_db::PgPool, row: &serde_json::Value) {
    sqlx::query(
        "update update_state u set channel = r.channel, auto_check = r.auto_check, \
         notify_email = r.notify_email, webhook_url = r.webhook_url, \
         webhook_method = r.webhook_method, webhook_secret_enc = r.webhook_secret_enc, \
         last_check_at = r.last_check_at, last_check_error = r.last_check_error, \
         last_check_problems = r.last_check_problems, next_check_at = r.next_check_at, \
         notified_version = r.notified_version \
         from jsonb_populate_record(null::update_state, $1) r where u.id",
    )
    .bind(row)
    .execute(pool)
    .await
    .expect("restore state");
    for sql in [
        "delete from update_runs where to_version like '900.%'",
        "delete from update_releases where version like '900.%'",
        "delete from platform_notifications where data->>'version' like '900.%' \
         or dedupe_key like 'update.%:900.%'",
        "delete from platform_admins where email like 'upd-%@example.com'",
    ] {
        sqlx::query(sql).execute(pool).await.expect(sql);
    }
}

impl Snapshot {
    async fn take(app: &TestApp) -> Self {
        // Creates the row when missing (the migration seeds it).
        let mut conn = app.pool().acquire().await.expect("conn");
        hos_db::repo::updates::state(&mut conn)
            .await
            .expect("state");
        drop(conn);
        let row: serde_json::Value =
            sqlx::query_scalar("select to_jsonb(s) from update_state s where id")
                .fetch_one(app.pool())
                .await
                .expect("snapshot");
        Self {
            row,
            restored: false,
        }
    }

    async fn restore(mut self, app: &TestApp) {
        restore_rows(app.pool(), &self.row).await;
        self.restored = true;
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        // A failed assertion: restore on a fresh runtime (the test's own is
        // unwinding), like `TestApp`'s tenant cleanup.
        let row = self.row.clone();
        let _ = std::thread::spawn(move || {
            hos_web::config::load_dotenv();
            let Ok(config) = hos_core::config::AppConfig::from_env() else {
                return;
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let cfg = hos_db::DbConfig::from_app_config(&config).with_max_connections(1);
                if let Ok(pool) = hos_db::connect(&cfg).await {
                    restore_rows(&pool, &row).await;
                    pool.close().await;
                }
            });
        })
        .join();
    }
}

/// A throw-away minisign key.
struct TestKey {
    pk: minisign::PublicKey,
    sk: minisign::SecretKey,
}

impl TestKey {
    fn generate() -> Self {
        let minisign::KeyPair { pk, sk } =
            minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        Self { pk, sk }
    }

    fn sign(&self, data: &[u8]) -> String {
        minisign::sign(
            Some(&self.pk),
            &self.sk,
            std::io::Cursor::new(data),
            Some("hostingos test signature"),
            None,
        )
        .expect("sign")
        .to_string()
    }

    fn ring(&self) -> KeyRing {
        let mut ring = KeyRing::empty();
        ring.add(TrustedKey::parse(&self.pk.to_base64(), "test", KeyOrigin::BuiltIn).expect("key"));
        ring
    }
}

fn manifest(version: &str) -> Vec<u8> {
    let target = hos_update::BuildInfo::current().target;
    let published = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "product": "hostingos",
        "version": version,
        "channel": "stable",
        "published_at": published,
        "severity": "security",
        "min_upgradable_from": "0.0.1",
        "migrations": true,
        "rollback_safe": false,
        "notes_md": format!("### Fixed\n- Release {version} fix\n\n<script>alert(1)</script>"),
        "container_image": format!("ghcr.io/example/hostingos:{version}"),
        "artifacts": [{
            "target": target,
            "name": format!("hostingos-{version}-{target}.tar.gz"),
            "url": "https://github.com/acme/hostingos/releases/download/x/a.tar.gz",
            "size": 10,
            "sha256": "cd".repeat(32),
        }]
    }))
    .unwrap()
}

/// A wiremock GitHub API: `V_GOOD` signed by `key`, `V_BAD` with a
/// signature over different bytes.
async fn release_api(key: &TestKey) -> MockServer {
    let server = MockServer::start().await;
    let list: Vec<serde_json::Value> = [V_GOOD, V_BAD]
        .iter()
        .map(|v| {
            serde_json::json!({
                "tag_name": format!("v{v}"),
                "draft": false,
                "prerelease": false,
                "html_url": format!("https://github.com/acme/hostingos/releases/tag/v{v}"),
                "assets": [
                    {"name": "hostingos-manifest.json",
                     "browser_download_url": format!("{}/dl/{v}/hostingos-manifest.json", server.uri())},
                    {"name": "hostingos-manifest.json.minisig",
                     "browser_download_url": format!("{}/dl/{v}/hostingos-manifest.json.minisig", server.uri())},
                ]
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/acme/hostingos/releases"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list))
        .mount(&server)
        .await;
    // Each manifest is serialised once: the signed bytes are the served bytes.
    let good = manifest(V_GOOD);
    let bad = manifest(V_BAD);
    for (v, body, signature) in [
        (V_GOOD, good.clone(), key.sign(&good)),
        (V_BAD, bad, key.sign(b"something else")),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/dl/{v}/hostingos-manifest.json")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/dl/{v}/hostingos-manifest.json.minisig")))
            .respond_with(ResponseTemplate::new(200).set_body_string(signature))
            .mount(&server)
            .await;
    }
    server
}

fn point_at(app: &TestApp, server: &MockServer, key: &TestKey, install: InstallKind) {
    let source = Source::github("acme/hostingos")
        .unwrap()
        .with_api_base(url::Url::parse(&server.uri()).unwrap());
    app.state.updates.set_test_overrides(
        Some(source),
        Some(NetConfig::insecure_for_tests(["127.0.0.1".to_owned()])),
        Some(install),
    );
    app.state.updates.set_test_keys(key.ring());
}

#[tokio::test]
async fn console_only_for_platform_admins_with_csrf() {
    let _guard = LOCK.lock().await;
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // Anonymous: sent to the console login, also for the status fragment.
    for p in [
        "/platform/updates",
        "/platform/updates/runs/00000000-0000-0000-0000-000000000000",
    ] {
        let r = get(&mut app, p).await;
        assert!(r.status.is_redirection(), "{p}: {}", r.status);
        assert!(
            r.location().starts_with("/platform/login"),
            "{p}: {}",
            r.location()
        );
    }
    // An anonymous POST changes nothing (CSRF / login).
    let r = post(&mut app, "/platform/updates/check", &[]).await;
    assert!(
        !(r.status == StatusCode::SEE_OTHER && r.location() == "/platform/updates"),
        "anonymous check ran: {}",
        r.status
    );

    // A tenant staff session does not open the console.
    let email = format!("owner-{}@staff.test", uuid::Uuid::now_v7().simple());
    tenant_owner_login(&mut app, &email).await;
    let host = app.host.clone();
    let r = app
        .send_to(&host, Method::GET, "/platform/updates", None, &[])
        .await;
    assert!(r.status.is_redirection(), "tenant staff: {}", r.status);
    assert!(
        r.location().starts_with("/platform/login"),
        "{}",
        r.location()
    );
    let admin_page = app.get("/admin").await;
    assert_eq!(admin_page.status, StatusCode::OK);
    assert!(
        !admin_page.body.contains("/platform/updates"),
        "tenant admin sees no update link"
    );

    // A platform admin: the page renders, without a configured source it
    // says so, and the version is shown here (not on /healthz).
    app.cookies.clear();
    let snapshot = Snapshot::take(&app).await;
    app.state.updates.set_test_overrides(
        None,
        None,
        Some(InstallKind::Unknown {
            reason: "test".into(),
        }),
    );
    let (email, _, csrf) = admin(&mut app, false).await;
    let page = get(&mut app, "/platform/updates").await;
    assert!(page.body.contains("HOS_UPDATE_REPO"), "not-configured hint");
    assert!(page.body.contains(hos_update::build_info::VERSION));
    let health = get(&mut app, "/healthz").await;
    assert!(!health.body.contains("version"), "{}", health.body);

    // Every state change needs the CSRF token.
    for p in [
        "/platform/updates/check",
        "/platform/updates/settings",
        "/platform/updates/apply",
        "/platform/updates/deploy",
        "/platform/updates/maintenance/off",
    ] {
        let r = post(&mut app, p, &[("version", V_GOOD), ("password", PASSWORD)]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{p} without CSRF");
    }
    // With it: checking without a source is a clear error, not a crash.
    let r = post(
        &mut app,
        "/platform/updates/check",
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/platform/updates");

    // The actions are audited.
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'platform.update.checked' \
         and created_at > now() - interval '5 minutes'",
    )
    .fetch_one(app.pool())
    .await
    .expect("audit");
    assert!(audited >= 1);

    drop_admin(&app, &email).await;
    snapshot.restore(&app).await;
    app.cleanup().await;
}

#[tokio::test]
async fn check_records_verified_releases_and_apply_needs_reauth() {
    let _guard = LOCK.lock().await;
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let snapshot = Snapshot::take(&app).await;
    let key = TestKey::generate();
    let server = release_api(&key).await;
    point_at(
        &app,
        &server,
        &key,
        InstallKind::Unknown {
            reason: "test".into(),
        },
    );
    let (email, secret, csrf) = admin(&mut app, true).await;
    let secret = secret.expect("totp");

    let r = post(
        &mut app,
        "/platform/updates/check",
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);

    let good = hos_db::repo::updates::get_release(app.pool(), V_GOOD)
        .await
        .unwrap()
        .expect("verified release recorded");
    assert_eq!(good.trust, "verified");
    assert_eq!(good.severity, "security");
    assert!(
        hos_db::repo::updates::get_release(app.pool(), V_BAD)
            .await
            .unwrap()
            .is_none(),
        "a bad signature is never recorded as a release"
    );
    let problems: serde_json::Value =
        sqlx::query_scalar("select last_check_problems from update_state where id")
            .fetch_one(app.pool())
            .await
            .unwrap();
    assert!(problems.to_string().contains(V_BAD), "{problems}");
    let notified: i64 =
        sqlx::query_scalar("select count(*) from platform_notifications where dedupe_key = $1")
            .bind(format!("update.available:{V_GOOD}"))
            .fetch_one(app.pool())
            .await
            .unwrap();
    assert_eq!(notified, 1);

    // The page lists it with sanitised notes; the dashboard shows the banner.
    let page = get(&mut app, "/platform/updates").await;
    assert!(page.body.contains(&format!("release-{V_GOOD}")));
    assert!(page.body.contains("Release 900.0.0 fix"));
    assert!(
        !page.body.contains("<script>alert(1)"),
        "notes are sanitised"
    );
    let dash = get(&mut app, "/platform").await;
    assert!(
        dash.body
            .contains(&format!("/platform/updates#release-{V_GOOD}"))
    );

    // A tenant admin whose browser also holds this operator's console
    // session sees a discreet indicator in the admin top bar (security
    // release: highlighted). A staff member with the operator's e-mail
    // address but no console session gets an empty fragment: the address
    // alone proves nothing.
    if let Some(mut shop) = TestApp::new().await {
        let console: Vec<(String, String)> = app
            .cookies
            .iter()
            .filter(|(k, _)| k.contains("psession"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert!(!console.is_empty(), "the operator's console session");
        let other = format!("owner-{}@staff.test", uuid::Uuid::now_v7().simple());
        for (mail, same_browser) in [(email.as_str(), false), (other.as_str(), true)] {
            tenant_owner_login(&mut shop, mail).await;
            if same_browser {
                shop.cookies.extend(console.iter().cloned());
            }
            let r = shop.get("/admin/update-indicator").await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.body);
            assert_eq!(
                r.body.contains("/platform/updates"),
                same_browser,
                "{mail}: {}",
                r.body
            );
            assert_eq!(r.body.contains("ax-update--security"), same_browser);
        }
        shop.cookies.clear();
        let anon = shop.get("/admin/update-indicator").await;
        assert!(!anon.body.contains("/platform/updates"), "anonymous");
        shop.cleanup().await;
    }

    // Apply: the wrong password or a missing TOTP code is refused and audited.
    for (pw, code) in [
        ("wrong-password", totp_now(&secret)),
        (PASSWORD, String::new()),
    ] {
        let r = post(
            &mut app,
            "/platform/updates/apply",
            &[
                ("csrf_token", &csrf),
                ("version", V_GOOD),
                ("password", pw),
                ("code", &code),
                ("backup_confirmed", "1"),
            ],
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        assert_eq!(r.location(), "/platform/updates");
    }
    let failed: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'platform.update.reauth_failed' \
         and created_at > now() - interval '5 minutes'",
    )
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert!(failed >= 2);
    // Correct re-authentication, but this is no bare-binary install: refused
    // before anything changes.
    let r = post(
        &mut app,
        "/platform/updates/apply",
        &[
            ("csrf_token", &csrf),
            ("version", V_GOOD),
            ("password", PASSWORD),
            ("code", &totp_now(&secret)),
            ("backup_confirmed", "1"),
        ],
    )
    .await;
    assert_eq!(r.location(), "/platform/updates");
    // A binary install without a supervisor is refused too.
    app.state.updates.set_test_overrides(
        Some(
            Source::github("acme/hostingos")
                .unwrap()
                .with_api_base(url::Url::parse(&server.uri()).unwrap()),
        ),
        Some(NetConfig::insecure_for_tests(["127.0.0.1".to_owned()])),
        Some(InstallKind::Binary(BinaryInstall {
            root: std::env::temp_dir().join("hostingos-update-test-nonexistent"),
            release: std::env::temp_dir().join("hostingos-update-test-nonexistent/r"),
            supervised: false,
        })),
    );
    let r = post(
        &mut app,
        "/platform/updates/apply",
        &[
            ("csrf_token", &csrf),
            ("version", V_GOOD),
            ("password", PASSWORD),
            ("code", &totp_now(&secret)),
            ("backup_confirmed", "1"),
        ],
    )
    .await;
    assert_eq!(r.location(), "/platform/updates");
    let runs: i64 = sqlx::query_scalar("select count(*) from update_runs where to_version = $1")
        .bind(V_GOOD)
        .fetch_one(app.pool())
        .await
        .unwrap();
    assert_eq!(runs, 0, "refused installs leave no run behind");
    assert!(
        !hos_db::repo::updates::maintenance(app.pool())
            .await
            .unwrap(),
        "maintenance mode untouched"
    );
    // A supervised binary install offers the install form (re-auth fields,
    // CSRF); nothing is posted to it here.
    app.state.updates.set_test_overrides(
        None,
        None,
        Some(InstallKind::Binary(BinaryInstall {
            root: std::env::temp_dir().join("hostingos-update-test-nonexistent"),
            release: std::env::temp_dir().join("hostingos-update-test-nonexistent/r"),
            supervised: true,
        })),
    );
    let page = get(&mut app, "/platform/updates").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(r#"action="/platform/updates/apply""#));
    assert!(page.body.contains(r#"name="code""#), "TOTP code asked");
    assert!(page.body.contains(r#"autocomplete="current-password""#));

    drop_admin(&app, &email).await;
    snapshot.restore(&app).await;
    app.cleanup().await;
}

#[tokio::test]
async fn deploy_webhook_for_container_installs() {
    let _guard = LOCK.lock().await;
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let snapshot = Snapshot::take(&app).await;
    let key = TestKey::generate();
    let server = release_api(&key).await;
    point_at(
        &app,
        &server,
        &key,
        InstallKind::Container(ContainerKind::Docker),
    );
    Mock::given(method("POST"))
        .and(path("/deploy"))
        .and(header("authorization", "Bearer hook-s3cret"))
        .and(header("x-hostingos-event", "hostingos.update"))
        .and(header_exists("x-hostingos-signature"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    let (email, _, csrf) = admin(&mut app, false).await;
    let r = post(
        &mut app,
        "/platform/updates/check",
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);

    // A webhook that is not a URL (or uses a foreign scheme) is refused.
    let r = post(
        &mut app,
        "/platform/updates/settings",
        &[
            ("csrf_token", &csrf),
            ("channel", "stable"),
            ("auto_check", "1"),
            ("webhook_url", "ftp://example.com/x"),
            ("webhook_method", "POST"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let hook = format!("{}/deploy", server.uri());
    let r = post(
        &mut app,
        "/platform/updates/settings",
        &[
            ("csrf_token", &csrf),
            ("channel", "stable"),
            ("auto_check", "1"),
            ("notify_email", "ops@example.com"),
            ("webhook_url", &hook),
            ("webhook_method", "POST"),
            ("webhook_secret", "hook-s3cret"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let unchanged: Option<String> =
        sqlx::query_scalar("select webhook_url from update_state where id")
            .fetch_one(app.pool())
            .await
            .unwrap();
    assert_ne!(
        unchanged.as_deref(),
        Some(hook.as_str()),
        "a webhook change without the password is refused"
    );
    let r = post(
        &mut app,
        "/platform/updates/settings",
        &[
            ("csrf_token", &csrf),
            ("channel", "stable"),
            ("auto_check", "1"),
            ("notify_email", "ops@example.com"),
            ("webhook_url", &hook),
            ("webhook_method", "POST"),
            ("webhook_secret", "hook-s3cret"),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let (stored_url, enc): (Option<String>, Option<Vec<u8>>) =
        sqlx::query_as("select webhook_url, webhook_secret_enc from update_state where id")
            .fetch_one(app.pool())
            .await
            .unwrap();
    assert_eq!(stored_url.as_deref(), Some(hook.as_str()));
    let enc = enc.expect("secret stored");
    assert!(
        !enc.windows(11).any(|w| w == b"hook-s3cret"),
        "the secret is stored encrypted"
    );

    // The page shows the image and the compose commands.
    let page = get(&mut app, "/platform/updates").await;
    assert!(
        page.body
            .contains(&format!("ghcr.io/example/hostingos:{V_GOOD}"))
    );
    assert!(page.body.contains("docker compose pull"));
    assert!(!page.body.contains("hook-s3cret"));

    // Wrong password: no call.
    let r = post(
        &mut app,
        "/platform/updates/deploy",
        &[
            ("csrf_token", &csrf),
            ("version", V_GOOD),
            ("password", "nope"),
        ],
    )
    .await;
    assert_eq!(r.location(), "/platform/updates");
    // Correct: one signed POST with the bearer token, recorded as a run.
    let r = post(
        &mut app,
        "/platform/updates/deploy",
        &[
            ("csrf_token", &csrf),
            ("version", V_GOOD),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        r.location().starts_with("/platform/updates/runs/"),
        "{}",
        r.location()
    );
    let run_page = get(&mut app, r.location()).await;
    assert_eq!(run_page.status, StatusCode::OK);
    assert!(run_page.body.contains("HTTP 202"), "{}", run_page.body);
    let status: String = sqlx::query_scalar(
        "select status from update_runs where to_version = $1 and kind = 'webhook'",
    )
    .bind(V_GOOD)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(status, "requested");
    // In-place installs are refused for containers.
    let r = post(
        &mut app,
        "/platform/updates/apply",
        &[
            ("csrf_token", &csrf),
            ("version", V_GOOD),
            ("password", PASSWORD),
            ("backup_confirmed", "1"),
        ],
    )
    .await;
    assert_eq!(r.location(), "/platform/updates");
    server.verify().await;

    // Moving the webhook to another host drops the stored secret.
    let r = post(
        &mut app,
        "/platform/updates/settings",
        &[
            ("csrf_token", &csrf),
            ("channel", "stable"),
            ("auto_check", "1"),
            ("webhook_url", "https://deploy.example.net/hook"),
            ("webhook_method", "POST"),
            ("password", PASSWORD),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let (stored_url, enc): (Option<String>, Option<Vec<u8>>) =
        sqlx::query_as("select webhook_url, webhook_secret_enc from update_state where id")
            .fetch_one(app.pool())
            .await
            .unwrap();
    assert_eq!(
        stored_url.as_deref(),
        Some("https://deploy.example.net/hook")
    );
    assert!(
        enc.is_none(),
        "the secret never follows the URL to a new host"
    );

    drop_admin(&app, &email).await;
    snapshot.restore(&app).await;
    app.cleanup().await;
}

#[tokio::test]
async fn maintenance_mode_answers_503_but_keeps_console_and_health() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // Only this test's own state: other tests share the database.
    app.state.updates.force_maintenance(Some(true));
    let r = app.get("/").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("120")
    );
    assert!(r.body.contains("Sistemi güncelliyoruz"));
    let host = app.host.clone();
    let r = app
        .send_to(&host, Method::GET, "/api/v1/ping", None, &[])
        .await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.content_type().starts_with("application/problem+json"));
    for p in ["/healthz", "/platform/login"] {
        let r = get(&mut app, p).await;
        assert_eq!(r.status, StatusCode::OK, "{p} stays reachable");
    }
    app.state.updates.force_maintenance(Some(false));
    let r = app.get("/").await;
    assert_ne!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    app.state.updates.force_maintenance(None);
    app.cleanup().await;
}

/// Log output captured in memory (what the development mailer writes).
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// `POST /platform/updates/check` with this thread's log captured in `logs`
/// (the test runtime is single-threaded: the check runs on this thread).
async fn check_capturing(app: &mut TestApp, csrf: &str, logs: &Captured) -> common::Res {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(logs.clone())
        .finish();
    let _sub = tracing::subscriber::set_default(subscriber);
    post(app, "/platform/updates/check", &[("csrf_token", csrf)]).await
}

/// Unsigned mode (no release key built in): releases are listed as
/// unverified with their notes and link as plain text, and no e-mail
/// announces them. Once a release verifies, the e-mail goes out.
#[tokio::test]
async fn unsigned_releases_are_plain_text_and_not_emailed() {
    let _guard = LOCK.lock().await;
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let snapshot = Snapshot::take(&app).await;
    let key = TestKey::generate();
    let server = release_api(&key).await;
    point_at(
        &app,
        &server,
        &key,
        InstallKind::Unknown {
            reason: "test".into(),
        },
    );
    app.state.updates.set_test_keys(KeyRing::empty());
    sqlx::query(
        "update update_state set notify_email = 'ops@example.com', notified_version = null where id",
    )
    .execute(app.pool())
    .await
    .unwrap();
    let (email, _, csrf) = admin(&mut app, false).await;
    let logs = Captured::default();
    let r = check_capturing(&mut app, &csrf, &logs).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let release = hos_db::repo::updates::get_release(app.pool(), V_BAD)
        .await
        .unwrap()
        .expect("unsigned release listed");
    assert_eq!(release.trust, "unverified");
    assert!(
        !logs.text().contains("email logged"),
        "no e-mail for an unsigned release: {}",
        logs.text()
    );
    let page = get(&mut app, "/platform/updates").await;
    assert!(page.body.contains(&format!("release-{V_BAD}")));
    assert!(page.body.contains("### Fixed"), "notes as plain text");
    assert!(!page.body.contains(">Fixed</h3>"), "notes not rendered");
    assert!(page.body.contains("alert(1)") && !page.body.contains("<script>alert(1)"));
    let release_page = format!("https://github.com/acme/hostingos/releases/tag/v{V_BAD}");
    assert!(page.body.contains(&release_page), "link shown as text");
    assert!(
        !page.body.contains(&format!("href=\"{release_page}\"")),
        "unsigned release link is not clickable"
    );

    // With the key: the verified release is announced by e-mail.
    app.state.updates.set_test_keys(key.ring());
    sqlx::query("update update_state set notified_version = null where id")
        .execute(app.pool())
        .await
        .unwrap();
    let r = check_capturing(&mut app, &csrf, &logs).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        logs.text().contains("email logged"),
        "verified release e-mailed: {}",
        logs.text()
    );

    drop_admin(&app, &email).await;
    snapshot.restore(&app).await;
    app.cleanup().await;
}
