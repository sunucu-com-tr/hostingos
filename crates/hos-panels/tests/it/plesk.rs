//! Plesk adapter against a wiremock REST API v2 / CLI gateway / XML API.

use crate::common::{SPEC_PASSWORD, panel, spec};
use hos_core::IntegrationError;
use hos_core::domain::Locale;
use hos_core::integrations::panel::{AccountRef, SsoService, SsoTarget};
use hos_core::integrations::{HostingPanel, kinds};
use hos_core::secret::SecretString;
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{
    basic_auth, body_json, body_string_contains, header, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup() -> (MockServer, Arc<dyn HostingPanel>) {
    let server = MockServer::start().await;
    let p = panel(
        kinds::PLESK,
        &server,
        json!({"default_ip": "203.0.113.5", "nameservers": "ns1.plesk.test ns2.plesk.test"}),
        json!({"api_key": "APIKEY-1"}),
    );
    (server, p)
}

fn rest(m: &str, p: &str) -> wiremock::MockBuilder {
    Mock::given(method(m))
        .and(path(format!("/api/v2/{p}")))
        .and(header("x-api-key", "APIKEY-1"))
}

fn json_reply(status: u16, body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

fn by_id() -> AccountRef {
    AccountRef {
        username: "alice".into(),
        domain: Some("example.com".into()),
        id: Some("7".into()),
    }
}

#[tokio::test]
async fn test_connection_with_api_key() {
    let (server, p) = setup().await;
    rest("GET", "server")
        .respond_with(json_reply(
            200,
            json!({
                "platform": "Unix", "hostname": "plesk.test", "panel_version": "18.0.60"
            }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let info = p.test_connection().await.unwrap();
    assert_eq!(info.server_version.as_deref(), Some("Plesk 18.0.60"));
    assert_eq!(info.details.as_deref(), Some("plesk.test (Unix)"));
}

#[tokio::test]
async fn basic_auth_and_401() {
    let server = MockServer::start().await;
    let p = panel(
        kinds::PLESK,
        &server,
        json!({"username": "admin"}),
        json!({"password": "AdminPass!1"}),
    );
    Mock::given(path("/api/v2/server"))
        .and(basic_auth("admin", "AdminPass!1"))
        .respond_with(json_reply(
            401,
            json!({"code": 1001, "message": "Authentication failed"}),
        ))
        .mount(&server)
        .await;
    let err = p.test_connection().await.unwrap_err();
    assert!(matches!(err, IntegrationError::Auth(_)), "{err}");

    // Neither key nor password → configuration error.
    assert!(matches!(
        hos_panels::build(kinds::PLESK, &json!({"host": "p.test"}), &json!({})),
        Err(IntegrationError::Config(_))
    ));
}

#[tokio::test]
async fn create_account_creates_customer_then_subscription() {
    let (server, p) = setup().await;
    rest("POST", "clients")
        .and(body_json(json!({
            "name": "Ayşe Yılmaz",
            "company": "Örnek Ltd",
            "login": "alice",
            "email": "ayse@example.com",
            "password": SPEC_PASSWORD,
            "type": "customer",
            "locale": "tr-TR",
            "external_id": "svc-42",
            "description": "Created by HostingOS"
        })))
        .respond_with(json_reply(201, json!({"id": 15, "guid": "c-guid"})))
        .expect(1)
        .mount(&server)
        .await;
    rest("POST", "domains")
        .and(body_json(json!({
            "name": "example.com",
            "hosting_type": "virtual",
            "hosting_settings": {"ftp_login": "alice", "ftp_password": SPEC_PASSWORD},
            "owner_client": {"id": 15},
            "ip_addresses": ["203.0.113.5"],
            "plan": {"name": "Unlimited"}
        })))
        .respond_with(json_reply(201, json!({"id": 7, "guid": "d-guid"})))
        .expect(1)
        .mount(&server)
        .await;
    let mut s = spec("alice", "example.com", "Unlimited");
    s.locale = Some(Locale::Tr);
    s.extra = json!({"external_id": "svc-42"});
    let created = p.create_account(&s).await.unwrap();
    assert_eq!(created.account.id.as_deref(), Some("7"));
    assert_eq!(created.account.username, "alice");
    assert_eq!(created.extra["customer_id"], 15);
    assert_eq!(
        created.nameservers,
        vec!["ns1.plesk.test", "ns2.plesk.test"]
    );
}

#[tokio::test]
async fn failed_subscription_rolls_back_customer() {
    let (server, p) = setup().await;
    rest("POST", "clients")
        .respond_with(json_reply(201, json!({"id": 15, "guid": "g"})))
        .mount(&server)
        .await;
    rest("POST", "domains")
        .respond_with(json_reply(
            400,
            json!({"code": 1007, "message": "Domain example.com already exists"}),
        ))
        .mount(&server)
        .await;
    rest("DELETE", "clients/15")
        .respond_with(json_reply(200, json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let err = p
        .create_account(&spec("alice", "example.com", "Unlimited"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Conflict(_)), "{err}");
}

#[tokio::test]
async fn generated_login_avoids_existing_clients() {
    let (server, p) = setup().await;
    rest("GET", "clients")
        .respond_with(json_reply(
            200,
            json!([{"id": 1, "login": "admin"}, {"id": 2, "login": "shop"}]),
        ))
        .mount(&server)
        .await;
    rest("POST", "clients")
        .and(body_string_contains("\"login\":\"shop1\""))
        .respond_with(json_reply(201, json!({"id": 3})))
        .expect(1)
        .mount(&server)
        .await;
    rest("POST", "domains")
        .respond_with(json_reply(201, json!({"id": 9})))
        .mount(&server)
        .await;
    let created = p
        .create_account(&spec("", "shop.com.tr", "Basic"))
        .await
        .unwrap();
    assert_eq!(created.account.username, "shop1");
}

#[tokio::test]
async fn rest_errors_are_mapped_and_scrubbed() {
    let (server, p) = setup().await;
    rest("POST", "clients")
        .respond_with(json_reply(
            500,
            json!({
                "code": 1019,
                "message": format!("Password {SPEC_PASSWORD} does not meet the security policy")
            }),
        ))
        .mount(&server)
        .await;
    let err = p
        .create_account(&spec("alice", "example.com", "P"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::InvalidInput(_)), "{err}");
    assert!(!err.to_string().contains(SPEC_PASSWORD), "{err}");

    rest("GET", "domains")
        .and(query_param("name", "missing.test"))
        .respond_with(json_reply(200, json!([])))
        .mount(&server)
        .await;
    let missing = AccountRef {
        username: "x".into(),
        domain: Some("missing.test".into()),
        id: None,
    };
    let err = p.suspend(&missing, None).await.unwrap_err();
    assert!(matches!(err, IntegrationError::NotFound(_)), "{err}");

    rest("PUT", "domains/7/status")
        .respond_with(json_reply(
            503,
            json!({"code": 9999, "message": "maintenance"}),
        ))
        .mount(&server)
        .await;
    let err = p.suspend(&by_id(), None).await.unwrap_err();
    assert!(err.is_retryable(), "{err}");
}

#[tokio::test]
async fn suspend_and_unsuspend_subscription_and_customer() {
    let (server, p) = setup().await;
    rest("GET", "domains")
        .and(query_param("name", "example.com"))
        .respond_with(json_reply(200, json!([{"id": 7, "name": "example.com"}])))
        .mount(&server)
        .await;
    rest("PUT", "domains/7/status")
        .and(body_json(json!({"status": "suspended"})))
        .respond_with(json_reply(200, json!({"status": "suspended"})))
        .expect(1)
        .mount(&server)
        .await;
    rest("PUT", "domains/7/status")
        .and(body_json(json!({"status": "active"})))
        .respond_with(json_reply(200, json!({"status": "active"})))
        .expect(1)
        .mount(&server)
        .await;
    rest("GET", "domains/7/client")
        .respond_with(json_reply(200, json!({"id": 15, "login": "alice"})))
        .mount(&server)
        .await;
    rest("PUT", "clients/15/suspend")
        .respond_with(json_reply(200, json!({"status": 16})))
        .expect(1)
        .mount(&server)
        .await;
    rest("PUT", "clients/15/activate")
        .respond_with(json_reply(200, json!({"status": 0})))
        .expect(1)
        .mount(&server)
        .await;
    // Look-up by domain when the id is unknown.
    let by_domain = AccountRef {
        username: "alice".into(),
        domain: Some("example.com".into()),
        id: None,
    };
    p.suspend(&by_domain, Some("unpaid")).await.unwrap();
    p.unsuspend(&by_id()).await.unwrap();
}

#[tokio::test]
async fn terminate_deletes_subscription_and_empty_customer() {
    let (server, p) = setup().await;
    rest("GET", "domains/7/client")
        .respond_with(json_reply(200, json!({"id": 15})))
        .mount(&server)
        .await;
    rest("DELETE", "domains/7")
        .respond_with(json_reply(200, json!({})))
        .expect(1)
        .mount(&server)
        .await;
    rest("GET", "clients/15/domains")
        .respond_with(json_reply(200, json!([])))
        .mount(&server)
        .await;
    rest("DELETE", "clients/15")
        .respond_with(json_reply(200, json!({})))
        .expect(1)
        .mount(&server)
        .await;
    p.terminate(&by_id()).await.unwrap();
}

#[tokio::test]
async fn change_package_uses_cli_gateway() {
    let (server, p) = setup().await;
    rest("POST", "cli/subscription/call")
        .and(body_json(json!({
            "params": ["--switch-subscription", "example.com", "-service-plan", "Pro Plan"]
        })))
        .respond_with(json_reply(
            200,
            json!({"code": 0, "stdout": "SUCCESS", "stderr": ""}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    p.change_package(&by_id(), "Pro Plan").await.unwrap();
}

#[tokio::test]
async fn cli_nonzero_exit_is_an_error() {
    let (server, p) = setup().await;
    rest("POST", "cli/subscription/call")
        .respond_with(json_reply(
            200,
            json!({
                "code": 1, "stdout": "", "stderr": "Service plan \"Nope\" does not exist"
            }),
        ))
        .mount(&server)
        .await;
    let err = p.change_package(&by_id(), "Nope").await.unwrap_err();
    assert!(matches!(err, IntegrationError::NotFound(_)), "{err}");
}

#[tokio::test]
async fn change_password_updates_customer_and_ftp_user() {
    let (server, p) = setup().await;
    rest("GET", "domains/7/client")
        .respond_with(json_reply(200, json!({"id": 15})))
        .mount(&server)
        .await;
    rest("PUT", "clients/15")
        .and(body_json(json!({"password": "N3w!Passw0rd-1"})))
        .respond_with(json_reply(200, json!({"id": 15})))
        .expect(1)
        .mount(&server)
        .await;
    rest("PUT", "domains/7")
        .and(body_json(
            json!({"hosting_settings": {"ftp_login": "alice", "ftp_password": "N3w!Passw0rd-1"}}),
        ))
        .respond_with(json_reply(200, json!({"id": 7})))
        .expect(1)
        .mount(&server)
        .await;
    p.change_password(&by_id(), &SecretString::from("N3w!Passw0rd-1"))
        .await
        .unwrap();
}

#[tokio::test]
async fn usage_from_client_statistics() {
    let (server, p) = setup().await;
    rest("GET", "domains/7/client")
        .respond_with(json_reply(200, json!({"id": 15})))
        .mount(&server)
        .await;
    rest("GET", "clients/15/statistics")
        .respond_with(json_reply(
            200,
            json!({
                "active_domains": 2, "subdomains": 1, "disk_space": 104_857_600u64,
                "email_postboxes": 4, "databases": 1, "traffic": 2_097_152u64, "traffic_prevday": 0
            }),
        ))
        .mount(&server)
        .await;
    let u = p.usage(&by_id()).await.unwrap();
    assert_eq!(u.disk_used_mb, Some(100));
    assert_eq!(u.bandwidth_used_mb, Some(2));
    assert_eq!(u.domains, Some(2));
    assert_eq!(u.email_accounts, Some(4));
}

#[tokio::test]
async fn sso_link_from_admin_cli() {
    let (server, p) = setup().await;
    rest("POST", "cli/admin/call")
        .and(body_json(json!({"params": ["--get-login-link", "-user", "alice"]})))
        .respond_with(json_reply(200, json!({
            "code": 0,
            "stdout": "https://plesk.test:8443/login?secret=ONE\nhttps://203.0.113.5:8443/login?secret=ONE\n",
            "stderr": ""
        })))
        .expect(1)
        .mount(&server)
        .await;
    let login = p.sso_url(&by_id(), &SsoTarget::default()).await.unwrap();
    assert_eq!(
        login.url.expose_str(),
        "https://plesk.test:8443/login?secret=ONE"
    );
    assert!(login.expires_at.is_some());
    let err = p
        .sso_url(
            &by_id(),
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
async fn list_packages_via_xml_api() {
    let (server, p) = setup().await;
    Mock::given(method("POST"))
        .and(path("/enterprise/control/agent.php"))
        .and(header("key", "APIKEY-1"))
        .and(body_string_contains(
            "<service-plan><get><filter/></get></service-plan>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<?xml version=\"1.0\"?><packet version=\"1.6.9.1\"><service-plan><get>\
             <result><status>ok</status><id>1</id><name>Basic</name><guid>g1</guid>\
             <limits><overuse>normal</overuse>\
             <limit><name>disk_space</name><value>1073741824</value></limit>\
             <limit><name>max_site</name><value>1</value></limit>\
             <limit><name>max_box</name><value>-1</value></limit></limits></result>\
             </get></service-plan></packet>",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/enterprise/control/agent.php"))
        .and(body_string_contains("<reseller-plan>"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<packet><reseller-plan><get><result><status>ok</status><id>3</id><name>R1</name>\
             <limits><limit><name>max_cl</name><value>25</value></limit></limits>\
             </result></get></reseller-plan></packet>",
        ))
        .mount(&server)
        .await;
    let pkgs = p.list_packages().await.unwrap();
    assert_eq!(pkgs.len(), 2);
    assert_eq!(pkgs[0].id, "Basic");
    assert_eq!(pkgs[0].limits.disk_mb, Some(1024));
    assert_eq!(pkgs[0].limits.domains, Some(1));
    assert_eq!(pkgs[0].limits.email_accounts, None);
    assert!(pkgs[1].reseller);
    assert_eq!(pkgs[1].limits.reseller_accounts, Some(25));
}

#[tokio::test]
async fn xml_system_error_maps_to_auth() {
    let (server, p) = setup().await;
    Mock::given(path("/enterprise/control/agent.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<packet><system><status>error</status><errcode>1001</errcode>\
             <errtext>Authentication failed</errtext></system></packet>",
        ))
        .mount(&server)
        .await;
    let err = p.list_packages().await.unwrap_err();
    assert!(matches!(err, IntegrationError::Auth(_)), "{err}");
}

#[tokio::test]
async fn reseller_accounts_are_unsupported() {
    let (_server, p) = setup().await;
    let mut s = spec("res", "res.test", "R1");
    s.reseller = true;
    assert!(matches!(
        p.create_account(&s).await,
        Err(IntegrationError::Unsupported(_))
    ));
}
