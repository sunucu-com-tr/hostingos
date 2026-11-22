//! DirectAdmin adapter against a wiremock CMD_API server.

use crate::common::{self, SPEC_PASSWORD, form, form_exact, panel, spec};
use hos_core::IntegrationError;
use hos_core::integrations::panel::{AccountRef, PanelLimits, SsoService, SsoTarget};
use hos_core::integrations::{HostingPanel, kinds};
use hos_core::secret::SecretString;
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{basic_auth, body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup() -> (MockServer, Arc<dyn HostingPanel>) {
    let server = MockServer::start().await;
    let p = panel(
        kinds::DIRECTADMIN,
        &server,
        json!({"username": "admin", "default_ip": "192.0.2.20", "nameservers": "ns1.da.test,ns2.da.test"}),
        json!({"login_key": "LOGINKEY99"}),
    );
    (server, p)
}

fn text(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/plain")
        .set_body_string(body)
}

fn da(m: &str, cmd: &str) -> wiremock::MockBuilder {
    Mock::given(method(m))
        .and(path(format!("/{cmd}")))
        .and(basic_auth("admin", "LOGINKEY99"))
}

#[tokio::test]
async fn test_connection_uses_basic_auth() {
    let (server, p) = setup().await;
    da("GET", "CMD_API_PACKAGES_USER")
        .respond_with(text("list[]=starter&list[]=pro"))
        .expect(1)
        .mount(&server)
        .await;
    da("GET", "api/version")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"version": "1.680", "os": "linux"})),
        )
        .mount(&server)
        .await;
    let info = p.test_connection().await.unwrap();
    assert_eq!(info.server_version.as_deref(), Some("DirectAdmin 1.680"));
    assert!(info.details.unwrap().contains("2 user packages"));
}

#[tokio::test]
async fn not_logged_in_maps_to_auth() {
    let (server, p) = setup().await;
    Mock::given(path("/CMD_API_PACKAGES_USER"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("content-type", "text/plain")
                .set_body_string("details=Unauthorized&error=1&text=Not+logged+in"),
        )
        .mount(&server)
        .await;
    let err = p.test_connection().await.unwrap_err();
    assert!(
        matches!(&err, IntegrationError::Auth(m) if m.contains("Not logged in")),
        "{err}"
    );
}

#[tokio::test]
async fn create_user_sends_exact_parameters() {
    let (server, p) = setup().await;
    da("POST", "CMD_API_ACCOUNT_USER")
        .and(form_exact(&[
            ("action", "create"),
            ("add", "Submit"),
            ("username", "alice"),
            ("email", "ayse@example.com"),
            ("passwd", SPEC_PASSWORD),
            ("passwd2", SPEC_PASSWORD),
            ("domain", "example.com"),
            ("package", "starter"),
            ("ip", "192.0.2.20"),
            ("notify", "no"),
        ]))
        .respond_with(text("error=0&text=User%20created&details=none"))
        .expect(1)
        .mount(&server)
        .await;
    let created = p
        .create_account(&spec("alice", "example.com", "starter"))
        .await
        .unwrap();
    assert_eq!(created.account.username, "alice");
    assert_eq!(created.ip, Some("192.0.2.20".parse().unwrap()));
    assert_eq!(created.nameservers, vec!["ns1.da.test", "ns2.da.test"]);
}

#[tokio::test]
async fn create_reseller_and_custom_limits() {
    let (server, p) = setup().await;
    da("POST", "CMD_API_ACCOUNT_RESELLER")
        .and(form(&[
            ("username", "resell"),
            ("ip", "shared"),
            ("package", "rpack"),
        ]))
        .respond_with(text("error=0&text=Reseller%20created"))
        .expect(1)
        .mount(&server)
        .await;
    let mut s = spec("resell", "reseller.test", "rpack");
    s.reseller = true;
    let created = p.create_account(&s).await.unwrap();
    assert_eq!(created.ip, None);

    da("POST", "CMD_API_ACCOUNT_USER")
        .and(form(&[
            ("username", "custom"),
            ("quota", "2048"),
            ("ubandwidth", "ON"),
            ("php", "ON"),
            ("ip", "192.0.2.20"),
        ]))
        .and(common::BodyLacks("package="))
        .respond_with(text("error=0&text=ok"))
        .expect(1)
        .mount(&server)
        .await;
    let mut s = spec("custom", "custom.test", "");
    s.limit_overrides = Some(PanelLimits {
        disk_mb: Some(2048),
        ..PanelLimits::default()
    });
    p.create_account(&s).await.unwrap();
}

#[tokio::test]
async fn generated_username_avoids_existing_users() {
    let (server, p) = setup().await;
    da("GET", "CMD_API_SHOW_ALL_USERS")
        .respond_with(text("list[]=example&list[]=example1"))
        .mount(&server)
        .await;
    da("GET", "CMD_API_SHOW_RESELLERS")
        .respond_with(text("list[]=example2"))
        .mount(&server)
        .await;
    da("POST", "CMD_API_ACCOUNT_USER")
        .and(form(&[("username", "example3")]))
        .respond_with(text("error=0&text=ok"))
        .expect(1)
        .mount(&server)
        .await;
    let created = p
        .create_account(&spec("", "example.com", "starter"))
        .await
        .unwrap();
    assert_eq!(created.account.username, "example3");
}

#[tokio::test]
async fn errors_in_body_are_mapped_and_scrubbed() {
    let (server, p) = setup().await;
    da("POST", "CMD_API_ACCOUNT_USER")
        .respond_with(text(&format!(
            "error=1&text=Cannot%20Execute%20Command&details=That%20username%20already%20exists%20on%20the%20system%20{}",
            url::form_urlencoded::byte_serialize(SPEC_PASSWORD.as_bytes()).collect::<String>()
        )))
        .mount(&server)
        .await;
    let err = p
        .create_account(&spec("alice", "example.com", "starter"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Conflict(_)), "{err}");
    assert!(!err.to_string().contains(SPEC_PASSWORD), "{err}");

    // HTTP 500 carrying a URL-encoded error is a business error, not a retry.
    da("POST", "CMD_API_SELECT_USERS")
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_string("error=1&text=Error&details=User%20ghost%20does%20not%20exist"),
        )
        .mount(&server)
        .await;
    let err = p
        .suspend(&AccountRef::new("ghost"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::NotFound(_)), "{err}");

    // An HTML login page means the credentials were not accepted.
    da("POST", "CMD_API_USER_PASSWD")
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<html><head><title>DirectAdmin Login</title></head></html>"),
        )
        .mount(&server)
        .await;
    let err = p
        .change_password(
            &AccountRef::new("bob"),
            &SecretString::from("Abcdefgh12345!"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Auth(_)), "{err}");
}

#[tokio::test]
async fn suspend_unsuspend_delete() {
    let (server, p) = setup().await;
    da("POST", "CMD_API_SELECT_USERS")
        .and(form_exact(&[
            ("location", "CMD_SELECT_USERS"),
            ("select0", "bob"),
            ("dosuspend", "yes"),
            ("reason", "billing"),
        ]))
        .respond_with(text("error=0&text=Success"))
        .expect(1)
        .mount(&server)
        .await;
    da("POST", "CMD_API_SELECT_USERS")
        .and(form_exact(&[
            ("location", "CMD_SELECT_USERS"),
            ("select0", "bob"),
            ("dounsuspend", "yes"),
        ]))
        .respond_with(text("error=0&text=Success"))
        .expect(1)
        .mount(&server)
        .await;
    da("POST", "CMD_API_SELECT_USERS")
        .and(form_exact(&[
            ("location", "CMD_SELECT_USERS"),
            ("select0", "bob"),
            ("confirmed", "Confirm"),
            ("delete", "yes"),
        ]))
        .respond_with(text("error=0&text=Users%20deleted"))
        .expect(1)
        .mount(&server)
        .await;
    let bob = AccountRef::new("bob");
    p.suspend(&bob, Some("Unpaid invoice #12")).await.unwrap();
    p.unsuspend(&bob).await.unwrap();
    p.terminate(&bob).await.unwrap();
}

#[tokio::test]
async fn change_package_picks_user_or_reseller_command() {
    let (server, p) = setup().await;
    da("GET", "CMD_API_SHOW_USER_CONFIG")
        .and(query_param("user", "bob"))
        .respond_with(text("usertype=user&package=starter"))
        .mount(&server)
        .await;
    da("GET", "CMD_API_SHOW_USER_CONFIG")
        .and(query_param("user", "rs"))
        .respond_with(text("usertype=reseller&package=r1"))
        .mount(&server)
        .await;
    da("POST", "CMD_API_MODIFY_USER")
        .and(form_exact(&[
            ("action", "package"),
            ("user", "bob"),
            ("package", "pro"),
        ]))
        .respond_with(text("error=0&text=ok"))
        .expect(1)
        .mount(&server)
        .await;
    da("POST", "CMD_API_MODIFY_RESELLER")
        .and(form_exact(&[
            ("action", "package"),
            ("user", "rs"),
            ("package", "r2"),
        ]))
        .respond_with(text("error=0&text=ok"))
        .expect(1)
        .mount(&server)
        .await;
    p.change_package(&AccountRef::new("bob"), "pro")
        .await
        .unwrap();
    p.change_package(&AccountRef::new("rs"), "r2")
        .await
        .unwrap();
}

#[tokio::test]
async fn change_password_params() {
    let (server, p) = setup().await;
    da("POST", "CMD_API_USER_PASSWD")
        .and(form_exact(&[
            ("username", "bob"),
            ("passwd", "N3wPassw0rd!x"),
            ("passwd2", "N3wPassw0rd!x"),
        ]))
        .respond_with(text("error=0&text=Password%20Changed"))
        .expect(1)
        .mount(&server)
        .await;
    p.change_password(
        &AccountRef::new("bob"),
        &SecretString::from("N3wPassw0rd!x"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn usage_and_account_info() {
    let (server, p) = setup().await;
    da("GET", "CMD_API_SHOW_USER_USAGE")
        .and(query_param("user", "bob"))
        .respond_with(text(
            "bandwidth=%31%30%30%2E5&quota=50.2&vdomains=2&nemails=7&mysql=1&ftp=1&inode=999",
        ))
        .mount(&server)
        .await;
    da("GET", "CMD_API_SHOW_USER_CONFIG")
        .and(query_param("user", "bob"))
        .respond_with(text(
            "bandwidth=10000&quota=unlimited&vdomains=5&nemails=unlimited&mysql=10&ftp=3&inode=unlimited\
             &suspended=yes&package=starter&email=b%40bob.test&ip=192.0.2.20&creator=admin\
             &date_created=Mon%20Oct%2024%2011%3A14%3A11%202022&domain=bob.test&usertype=user",
        ))
        .mount(&server)
        .await;
    let bob = AccountRef::new("bob");
    let u = p.usage(&bob).await.unwrap();
    assert_eq!(u.bandwidth_used_mb, Some(101));
    assert_eq!(u.disk_used_mb, Some(50));
    assert_eq!(u.domains, Some(2));
    assert_eq!(u.email_accounts, Some(7));
    assert_eq!(u.limits.bandwidth_mb, Some(10000));
    assert_eq!(u.limits.disk_mb, None);
    assert_eq!(u.limits.databases, Some(10));

    let info = p.get_account(&bob).await.unwrap();
    assert!(info.suspended);
    assert_eq!(info.email.as_deref(), Some("b@bob.test"));
    assert_eq!(info.account.domain.as_deref(), Some("bob.test"));
    assert!(info.created_at.is_some());
}

#[tokio::test]
async fn list_packages_reads_details() {
    let (server, p) = setup().await;
    da("GET", "CMD_API_PACKAGES_USER")
        .and(query_param("package", "starter"))
        .respond_with(text("bandwidth=10240&quota=1024&vdomains=1&nsubdomains=unlimited&nemails=10&mysql=2&ftp=1&domainptr=0&inode=unlimited"))
        .mount(&server)
        .await;
    da("GET", "CMD_API_PACKAGES_USER")
        .respond_with(text("list[]=starter"))
        .mount(&server)
        .await;
    da("GET", "CMD_API_PACKAGES_RESELLER")
        .and(query_param("package", "rbig"))
        .respond_with(text("bandwidth=unlimited&quota=51200&nusers=20"))
        .mount(&server)
        .await;
    da("GET", "CMD_API_PACKAGES_RESELLER")
        .respond_with(text("list[]=rbig"))
        .mount(&server)
        .await;
    let pkgs = p.list_packages().await.unwrap();
    assert_eq!(pkgs.len(), 2);
    assert_eq!(pkgs[0].limits.disk_mb, Some(1024));
    assert_eq!(pkgs[0].limits.subdomains, None);
    assert_eq!(pkgs[0].limits.alias_domains, Some(0));
    assert!(pkgs[1].reseller);
    assert_eq!(pkgs[1].limits.reseller_accounts, Some(20));
}

#[tokio::test]
async fn sso_uses_login_url_api_as_the_user() {
    let (server, p) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/login-keys/urls"))
        .and(basic_auth("admin|bob", "LOGINKEY99"))
        .and(body_partial_json(json!({"redirectURL": "/evo/user/email"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "k1", "url": "https://da.test:2222/api/login/url?key=ONE-TIME",
            "expires": "2030-01-01T00:00:00Z"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let target = SsoTarget {
        service: SsoService::ControlPanel,
        app: Some("/evo/user/email".into()),
        ..SsoTarget::default()
    };
    let login = p.sso_url(&AccountRef::new("bob"), &target).await.unwrap();
    assert!(login.url.expose_str().ends_with("key=ONE-TIME"));
    assert_eq!(
        login.expires_at.unwrap().to_rfc3339(),
        "2030-01-01T00:00:00+00:00"
    );

    let err = p
        .sso_url(
            &AccountRef::new("bob"),
            &SsoTarget {
                service: SsoService::Webmail,
                ..SsoTarget::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Unsupported(_)));
}

#[tokio::test]
async fn sso_falls_back_to_legacy_login_keys() {
    let (server, p) = setup().await;
    Mock::given(path("/api/login-keys/urls"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/CMD_API_LOGIN_KEYS"))
        .and(basic_auth("admin|bob", "LOGINKEY99"))
        .and(form(&[
            ("action", "create"),
            ("type", "one_time_url"),
            ("passwd", "LOGINKEY99"),
            ("expiry", "15m"),
        ]))
        .respond_with(text(
            "error=0&text=Login%20URL%20created&details=https%3A%2F%2Fda.test%3A2222%2FCMD_LOGIN%3Fkey%3DABC",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let login = p
        .sso_url(&AccountRef::new("bob"), &SsoTarget::default())
        .await
        .unwrap();
    assert_eq!(
        login.url.expose_str(),
        "https://da.test:2222/CMD_LOGIN?key=ABC"
    );
}
