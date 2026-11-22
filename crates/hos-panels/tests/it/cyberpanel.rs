//! CyberPanel adapter against a wiremock `/api/*` server.

use crate::common::{SPEC_PASSWORD, panel, spec};
use hos_core::IntegrationError;
use hos_core::integrations::panel::{AccountRef, SsoTarget};
use hos_core::integrations::{HostingPanel, kinds};
use hos_core::secret::SecretString;
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{body_json, body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADMIN_PASS: &str = "Adm1nPass#2024";

async fn setup() -> (MockServer, Arc<dyn HostingPanel>) {
    let server = MockServer::start().await;
    let p = panel(
        kinds::CYBERPANEL,
        &server,
        json!({"username": "admin", "php_version": "PHP 8.2", "nameservers": "ns1.cp.test"}),
        json!({"password": ADMIN_PASS}),
    );
    (server, p)
}

fn api(endpoint: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path(format!("/api/{endpoint}")))
        .and(body_partial_json(
            json!({"adminUser": "admin", "adminPass": ADMIN_PASS}),
        ))
}

fn reply(body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn site(user: &str, domain: &str) -> AccountRef {
    AccountRef {
        username: user.into(),
        domain: Some(domain.into()),
        id: None,
    }
}

#[tokio::test]
async fn test_connection_verifies_credentials() {
    let (server, p) = setup().await;
    api("verifyConn")
        .respond_with(reply(json!({"verifyConn": 1, "apiToken": "Basic cp_tok"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/cyberPanelVersion"))
        .and(body_json(
            json!({"username": "admin", "password": ADMIN_PASS}),
        ))
        .respond_with(reply(
            json!({"getVersion": 1, "currentVersion": "2.4", "build": 3}),
        ))
        .mount(&server)
        .await;
    let info = p.test_connection().await.unwrap();
    assert_eq!(info.server_version.as_deref(), Some("CyberPanel 2.4.3"));
}

#[tokio::test]
async fn auth_failures_map_to_auth() {
    let (server, p) = setup().await;
    Mock::given(path("/api/verifyConn"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "verifyConn": 0, "error_message": "Invalid password."
        })))
        .mount(&server)
        .await;
    let err = p.test_connection().await.unwrap_err();
    assert!(
        matches!(&err, IntegrationError::Auth(m) if m.contains("Invalid password")),
        "{err}"
    );
    assert!(!err.to_string().contains(ADMIN_PASS));

    // Other endpoints report bad credentials inside HTTP 200.
    Mock::given(path("/api/changePackageAPI"))
        .respond_with(reply(
            json!({"changePackage": 0, "error_message": "Could not authorize access to API."}),
        ))
        .mount(&server)
        .await;
    let err = p
        .change_package(&site("bob", "bob.test"), "Default")
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Auth(_)), "{err}");
}

#[tokio::test]
async fn create_account_checks_owner_and_sends_exact_body() {
    let (server, p) = setup().await;
    api("getUserInfo")
        .and(body_partial_json(json!({"username": "alice"})))
        .respond_with(reply(
            json!({"status": 0, "error_message": "User does not exists."}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/createWebsite"))
        .and(body_json(json!({
            "adminUser": "admin",
            "adminPass": ADMIN_PASS,
            "domainName": "example.com",
            "ownerEmail": "ayse@example.com",
            "packageName": "Default",
            "websiteOwner": "alice",
            "ownerPassword": SPEC_PASSWORD,
            "phpSelection": "PHP 8.2",
            "mailDomain": "1"
        })))
        .respond_with(reply(json!({
            "status": 1, "createWebSiteStatus": 1, "error_message": "None",
            "tempStatusPath": "/home/cyberpanel/123", "LinuxUser": "examp1234"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut s = spec("alice", "example.com", "Default");
    s.extra = json!({"mailDomain": "1", "websiteOwner": "evil"});
    let created = p.create_account(&s).await.unwrap();
    assert_eq!(created.account.username, "alice");
    assert_eq!(created.account.domain.as_deref(), Some("example.com"));
    assert_eq!(created.extra["linux_user"], "examp1234");
    assert_eq!(created.nameservers, vec!["ns1.cp.test"]);
}

#[tokio::test]
async fn existing_owner_is_a_conflict_and_generation_skips_it() {
    let (server, p) = setup().await;
    api("getUserInfo")
        .and(body_partial_json(json!({"username": "example"})))
        .respond_with(reply(
            json!({"status": 1, "firstName": "x", "email": "x@x"}),
        ))
        .mount(&server)
        .await;
    api("getUserInfo")
        .and(body_partial_json(json!({"username": "example1"})))
        .respond_with(reply(
            json!({"status": 0, "error_message": "User does not exists."}),
        ))
        .mount(&server)
        .await;
    api("createWebsite")
        .and(body_partial_json(json!({"websiteOwner": "example1"})))
        .respond_with(reply(
            json!({"createWebSiteStatus": 1, "error_message": "None"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let err = p
        .create_account(&spec("example", "example.com", "Default"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Conflict(_)), "{err}");
    let created = p
        .create_account(&spec("", "example.com", "Default"))
        .await
        .unwrap();
    assert_eq!(created.account.username, "example1");
}

#[tokio::test]
async fn create_errors_in_http_200_are_mapped() {
    let (server, p) = setup().await;
    api("getUserInfo")
        .respond_with(reply(
            json!({"status": 0, "error_message": "User does not exists."}),
        ))
        .mount(&server)
        .await;
    api("createWebsite")
        .respond_with(reply(json!({
            "status": 0, "createWebSiteStatus": 0, "error_message": "Invalid domain."
        })))
        .mount(&server)
        .await;
    let err = p
        .create_account(&spec("alice", "bad_domain", "Default"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::InvalidInput(_)), "{err}");

    // Passwords CyberPanel's middleware would reject never leave the process.
    let mut s = spec("alice", "example.com", "Default");
    s.password = SecretString::from("abc$def;ghi");
    assert!(matches!(
        p.create_account(&s).await,
        Err(IntegrationError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn middleware_rejection_is_invalid_input() {
    let (server, p) = setup().await;
    api("changeUserPassAPI")
        .respond_with(reply(json!({
            "error_message": "Data supplied is not accepted.", "errorMessage": "Data supplied is not accepted."
        })))
        .mount(&server)
        .await;
    let err = p
        .change_password(
            &site("bob", "bob.test"),
            &SecretString::from("Fine@Pass1234"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::InvalidInput(_)), "{err}");
}

#[tokio::test]
async fn suspend_unsuspend_terminate() {
    let (server, p) = setup().await;
    api("submitWebsiteStatus")
        .and(body_partial_json(
            json!({"websiteName": "bob.test", "state": "Suspend"}),
        ))
        .respond_with(reply(json!({"websiteStatus": 1, "error_message": "None"})))
        .expect(1)
        .mount(&server)
        .await;
    api("submitWebsiteStatus")
        .and(body_partial_json(
            json!({"websiteName": "bob.test", "state": "Unsuspend"}),
        ))
        .respond_with(reply(json!({"websiteStatus": 1, "error_message": "None"})))
        .expect(1)
        .mount(&server)
        .await;
    api("deleteWebsite")
        .and(body_partial_json(json!({"domainName": "bob.test"})))
        .respond_with(reply(
            json!({"status": 1, "websiteDeleteStatus": 1, "state": "completed"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    api("submitUserDeletion")
        .and(body_partial_json(json!({"accountUsername": "bob"})))
        .respond_with(reply(json!({"status": 1, "deleteStatus": 1})))
        .expect(1)
        .mount(&server)
        .await;
    let bob = site("bob", "bob.test");
    p.suspend(&bob, Some("unpaid")).await.unwrap();
    p.unsuspend(&bob).await.unwrap();
    p.terminate(&bob).await.unwrap();

    // The website is required for website-level operations.
    assert!(matches!(
        p.suspend(&AccountRef::new("bob"), None).await,
        Err(IntegrationError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn delete_failure_is_reported() {
    let (server, p) = setup().await;
    api("deleteWebsite")
        .respond_with(reply(json!({
            "status": 0, "websiteDeleteStatus": 0, "state": "failed",
            "error_message": "Website bob.test does not exist"
        })))
        .mount(&server)
        .await;
    let err = p.terminate(&site("bob", "bob.test")).await.unwrap_err();
    assert!(matches!(err, IntegrationError::NotFound(_)), "{err}");
}

#[tokio::test]
async fn change_package_and_password() {
    let (server, p) = setup().await;
    api("changePackageAPI")
        .and(body_partial_json(
            json!({"websiteName": "bob.test", "packageName": "Pro"}),
        ))
        .respond_with(reply(json!({"changePackage": 1, "error_message": "None"})))
        .expect(1)
        .mount(&server)
        .await;
    api("changeUserPassAPI")
        .and(body_partial_json(
            json!({"websiteOwner": "bob", "ownerPassword": "N3w@Passw0rd1"}),
        ))
        .respond_with(reply(json!({"changeStatus": 1})))
        .expect(1)
        .mount(&server)
        .await;
    let bob = site("bob", "bob.test");
    p.change_package(&bob, "Pro").await.unwrap();
    p.change_password(&bob, &SecretString::from("N3w@Passw0rd1"))
        .await
        .unwrap();
}

#[tokio::test]
async fn list_packages_bare_array() {
    let (server, p) = setup().await;
    api("listPackage")
        .respond_with(reply(json!(["Default", "admin_Pro"])))
        .mount(&server)
        .await;
    let pkgs = p.list_packages().await.unwrap();
    assert_eq!(
        pkgs.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["Default", "admin_Pro"]
    );
}

#[tokio::test]
async fn usage_via_cloud_api_token() {
    let (server, p) = setup().await;
    api("verifyConn")
        .respond_with(reply(json!({"verifyConn": 1, "apiToken": "Basic cp_tok"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/cloudAPI/"))
        .and(header("authorization", "Basic cp_tok"))
        .and(body_json(json!({
            "controller": "fetchWebsiteData", "serverUserName": "admin", "domainName": "bob.test"
        })))
        .respond_with(reply(json!({
            "status": 1, "diskInMB": 120, "diskInMBTotal": 1000, "bwInMB": 55,
            "bwInMBTotal": 0, "dbUsed": 2, "dbAllowed": 5, "ftpUsed": 1, "ftpAllowed": 0
        })))
        .expect(1)
        .mount(&server)
        .await;
    let u = p.usage(&site("bob", "bob.test")).await.unwrap();
    assert_eq!(u.disk_used_mb, Some(120));
    assert_eq!(u.bandwidth_used_mb, Some(55));
    assert_eq!(u.databases, Some(2));
    assert_eq!(u.limits.disk_mb, Some(1000));
    assert_eq!(u.limits.bandwidth_mb, None);
    assert_eq!(u.limits.ftp_accounts, None);
}

#[tokio::test]
async fn sso_is_unsupported() {
    let (_server, p) = setup().await;
    assert!(matches!(
        p.sso_url(&site("bob", "bob.test"), &SsoTarget::default())
            .await,
        Err(IntegrationError::Unsupported(_))
    ));
}
