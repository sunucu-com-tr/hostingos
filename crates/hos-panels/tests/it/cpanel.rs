//! cPanel/WHM adapter against a wiremock WHM API 1.

use crate::common::{SPEC_PASSWORD, form, form_exact, panel, spec};
use hos_core::IntegrationError;
use hos_core::domain::Locale;
use hos_core::integrations::panel::{AccountRef, SsoService, SsoTarget};
use hos_core::integrations::{HostingPanel, kinds};
use hos_core::secret::SecretString;
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const AUTH: &str = "whm root:TOKEN123";

async fn setup() -> (MockServer, Arc<dyn HostingPanel>) {
    let server = MockServer::start().await;
    let p = panel(
        kinds::CPANEL,
        &server,
        json!({"default_ip": "192.0.2.10", "nameservers": "ns1.host.test, ns2.host.test"}),
        json!({"api_token": "TOKEN123"}),
    );
    (server, p)
}

fn ok(data: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "metadata": {"result": 1, "reason": "OK", "version": 1, "command": "x"},
        "data": data
    }))
}

fn fail(reason: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "metadata": {"result": 0, "reason": reason, "version": 1, "command": "x"}
    }))
}

fn whm(function: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path(format!("/json-api/{function}")))
        .and(query_param("api.version", "1"))
        .and(header("authorization", AUTH))
}

fn acct(username: &str) -> AccountRef {
    AccountRef::new(username)
}

#[tokio::test]
async fn test_connection_sends_whm_token_and_checks_acls() {
    let (server, p) = setup().await;
    whm("version")
        .respond_with(ok(json!({"version": "11.120.0.5"})))
        .expect(2)
        .mount(&server)
        .await;
    let all = [
        "createacct",
        "suspendacct",
        "unsuspendacct",
        "removeacct",
        "changepackage",
        "passwd",
        "listpkgs",
        "accountsummary",
        "create_user_session",
        "version",
    ];
    whm("applist")
        .respond_with(ok(json!({"app": all})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let info = p.test_connection().await.unwrap();
    assert_eq!(info.server_version.as_deref(), Some("WHM 11.120.0.5"));

    // A token without `createacct` is reported as an auth problem.
    whm("applist")
        .respond_with(ok(json!({"app": ["version", "listpkgs"]})))
        .mount(&server)
        .await;
    let err = p.test_connection().await.unwrap_err();
    assert!(
        matches!(&err, IntegrationError::Auth(m) if m.contains("createacct")),
        "{err}"
    );
}

#[tokio::test]
async fn rejected_token_maps_to_auth() {
    let (server, p) = setup().await;
    Mock::given(path("/json-api/version"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "cpanelresult": {"error": "Token denied", "data": {"result": "0"}}
        })))
        .mount(&server)
        .await;
    let err = p.test_connection().await.unwrap_err();
    assert!(
        matches!(&err, IntegrationError::Auth(m) if m.contains("Token denied")),
        "{err}"
    );
    assert!(!err.to_string().contains("TOKEN123"));
}

#[tokio::test]
async fn create_account_sends_exact_parameters() {
    let (server, p) = setup().await;
    whm("createacct")
        .and(form_exact(&[
            ("username", "alice"),
            ("domain", "example.com"),
            ("password", SPEC_PASSWORD),
            ("contactemail", "ayse@example.com"),
            ("hasshell", "0"),
            ("plan", "starter"),
            ("language", "tr"),
            ("customip", "192.0.2.10"),
            ("featurelist", "gold"),
        ]))
        .respond_with(ok(json!({
            "ip": "192.0.2.10", "nameserver": "ns1.server.test", "nameserver2": "ns2.server.test",
            "nameserver3": null, "package": "starter"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut s = spec("alice", "Example.com", "starter");
    s.locale = Some(Locale::Tr);
    // `password` in extra must not override the managed parameter.
    s.extra = json!({"featurelist": "gold", "password": "evil"});
    let created = p.create_account(&s).await.unwrap();
    assert_eq!(created.account.username, "alice");
    assert_eq!(created.account.domain.as_deref(), Some("example.com"));
    assert_eq!(created.ip, Some("192.0.2.10".parse().unwrap()));
    assert_eq!(
        created.nameservers,
        vec!["ns1.server.test", "ns2.server.test"]
    );
    assert!(created.login_url.unwrap().ends_with(":2082/"));
}

#[tokio::test]
async fn create_reseller_sets_acl_list() {
    let (server, p) = setup().await;
    whm("createacct")
        .and(form(&[
            ("reseller", "1"),
            ("ownerself", "1"),
            ("username", "resell"),
        ]))
        .respond_with(ok(json!({"ip": "192.0.2.10", "package": "rs"})))
        .expect(1)
        .mount(&server)
        .await;
    whm("setacls")
        .and(form_exact(&[
            ("reseller", "resell"),
            ("acllist", "standard"),
        ]))
        .respond_with(ok(json!({"acl": []})))
        .expect(1)
        .mount(&server)
        .await;
    let mut s = spec("resell", "reseller.test", "rs");
    s.reseller = true;
    s.extra = json!({"acllist": "standard"});
    let created = p.create_account(&s).await.unwrap();
    // Configured nameservers are the fallback when WHM reports none.
    assert_eq!(created.nameservers, vec!["ns1.host.test", "ns2.host.test"]);
}

#[tokio::test]
async fn blank_username_is_generated_and_verified() {
    let (server, p) = setup().await;
    whm("verify_new_username")
        .and(form_exact(&[("user", "example")]))
        .respond_with(fail("The username “example” already exists."))
        .expect(1)
        .mount(&server)
        .await;
    whm("verify_new_username")
        .and(form_exact(&[("user", "example1")]))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    whm("createacct")
        .and(form(&[("username", "example1"), ("domain", "example.com")]))
        .respond_with(ok(json!({"ip": "192.0.2.10"})))
        .expect(1)
        .mount(&server)
        .await;
    let created = p
        .create_account(&spec("", "example.com", "starter"))
        .await
        .unwrap();
    assert_eq!(created.account.username, "example1");
}

#[tokio::test]
async fn whm_errors_inside_http_200_are_mapped_and_scrubbed() {
    let (server, p) = setup().await;
    whm("createacct")
        .respond_with(fail(&format!(
            "This system already has an account named “alice”. (password {SPEC_PASSWORD})"
        )))
        .mount(&server)
        .await;
    let err = p
        .create_account(&spec("alice", "example.com", "starter"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Conflict(_)), "{err}");
    assert!(!err.to_string().contains(SPEC_PASSWORD), "{err}");

    whm("suspendacct")
        .respond_with(fail(
            "You do not have a user named “ghost”; called for a user that does not exist.",
        ))
        .mount(&server)
        .await;
    let err = p.suspend(&acct("ghost"), None).await.unwrap_err();
    assert!(matches!(err, IntegrationError::NotFound(_)), "{err}");

    whm("removeacct")
        .respond_with(fail(
            "Permission denied: You do not have the required privileges",
        ))
        .mount(&server)
        .await;
    let err = p.terminate(&acct("bob")).await.unwrap_err();
    assert!(matches!(err, IntegrationError::Auth(_)), "{err}");

    // Invalid usernames are rejected before any request.
    let err = p
        .create_account(&spec("Test-User", "x.com", "p"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::InvalidInput(_)), "{err}");
}

#[tokio::test]
async fn suspend_unsuspend_terminate() {
    let (server, p) = setup().await;
    whm("suspendacct")
        .and(form_exact(&[
            ("user", "bob"),
            ("reason", "Overdue invoice"),
        ]))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    whm("unsuspendacct")
        .and(form_exact(&[("user", "bob")]))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    whm("removeacct")
        .and(form_exact(&[("username", "bob"), ("keepdns", "0")]))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    p.suspend(&acct("bob"), Some("Overdue invoice"))
        .await
        .unwrap();
    p.unsuspend(&acct("bob")).await.unwrap();
    p.terminate(&acct("bob")).await.unwrap();
}

#[tokio::test]
async fn suspend_is_idempotent() {
    let (server, p) = setup().await;
    whm("suspendacct")
        .respond_with(fail("The account “bob” is already suspended."))
        .mount(&server)
        .await;
    p.suspend(&acct("bob"), None).await.unwrap();
}

#[tokio::test]
async fn change_package_and_password() {
    let (server, p) = setup().await;
    whm("changepackage")
        .and(form_exact(&[("user", "bob"), ("pkg", "business")]))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    whm("passwd")
        .and(form_exact(&[
            ("user", "bob"),
            ("password", "N3w!Passw0rd-abc"),
        ]))
        .respond_with(fail("The password N3w!Passw0rd-abc is too weak"))
        .expect(1)
        .mount(&server)
        .await;
    p.change_package(&acct("bob"), "business").await.unwrap();
    let err = p
        .change_password(&acct("bob"), &SecretString::from("N3w!Passw0rd-abc"))
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::InvalidInput(_)), "{err}");
    assert!(!err.to_string().contains("N3w!Passw0rd-abc"), "{err}");
}

#[tokio::test]
async fn usage_combines_accountsummary_and_showbw() {
    let (server, p) = setup().await;
    whm("accountsummary")
        .and(form_exact(&[("user", "bob")]))
        .respond_with(ok(json!({"acct": [{
            "user": "bob", "diskused": "250M", "disklimit": "1000M", "inodesused": 1234,
            "inodeslimit": "unlimited", "maxaddons": "*unknown*", "maxsub": "unlimited",
            "maxparked": 2, "maxpop": "10", "maxsql": 5, "maxftp": "unlimited"
        }]})))
        .mount(&server)
        .await;
    whm("showbw")
        .and(form_exact(&[("searchtype", "user"), ("search", "bob")]))
        .respond_with(ok(json!({"acct": [
            {"user": "bobby", "totalbytes": 1, "limit": 0},
            {"user": "bob", "totalbytes": 52_428_800u64, "limit": 10_737_418_240u64}
        ]})))
        .mount(&server)
        .await;
    let u = p.usage(&acct("bob")).await.unwrap();
    assert_eq!(u.disk_used_mb, Some(250));
    assert_eq!(u.bandwidth_used_mb, Some(50));
    assert_eq!(u.inodes, Some(1234));
    assert_eq!(u.limits.disk_mb, Some(1000));
    assert_eq!(u.limits.bandwidth_mb, Some(10240));
    assert_eq!(u.limits.domains, Some(1));
    assert_eq!(u.limits.subdomains, None);
    assert_eq!(u.limits.email_accounts, Some(10));
}

#[tokio::test]
async fn sso_creates_user_session() {
    let (server, p) = setup().await;
    whm("create_user_session")
        .and(form_exact(&[
            ("user", "bob"),
            ("service", "cpaneld"),
            ("app", "Email_Accounts"),
            ("locale", "en"),
        ]))
        .respond_with(ok(json!({
            "url": "https://server.test:2083/cpsess123/login/?session=bob%3atok",
            "expires": 1_900_000_000, "session": "bob:tok", "service": "cpaneld"
        })))
        .expect(1)
        .mount(&server)
        .await;
    whm("create_user_session")
        .and(form(&[
            ("service", "webmaild"),
            ("preferred_domain", "bob.test"),
        ]))
        .respond_with(ok(
            json!({"url": "https://server.test:2096/cpsess9/login", "expires": 1}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let target = SsoTarget {
        service: SsoService::ControlPanel,
        app: Some("Email_Accounts".into()),
        client_ip: None,
        locale: Some(Locale::En),
    };
    let login = p.sso_url(&acct("bob"), &target).await.unwrap();
    assert!(login.url.expose_str().contains("cpsess123"));
    assert_eq!(login.expires_at.unwrap().timestamp(), 1_900_000_000);
    assert!(!format!("{login:?}").contains("cpsess123"));

    let mut account = acct("bob");
    account.domain = Some("bob.test".into());
    let webmail = SsoTarget {
        service: SsoService::Webmail,
        ..SsoTarget::default()
    };
    p.sso_url(&account, &webmail).await.unwrap();
}

#[tokio::test]
async fn list_packages_maps_limits() {
    let (server, p) = setup().await;
    whm("listpkgs")
        .respond_with(ok(json!({"pkg": [
            {"name": "starter", "QUOTA": 1024, "BWLIMIT": "unlimited", "MAXADDON": 0,
             "MAXSUB": "unlimited", "MAXPARK": 2, "MAXPOP": 10, "MAXSQL": "5", "MAXFTP": 1},
            {"name": "unl", "QUOTA": "unlimited", "BWLIMIT": 0, "MAXADDON": "unlimited"}
        ]})))
        .mount(&server)
        .await;
    let pkgs = p.list_packages().await.unwrap();
    assert_eq!(pkgs.len(), 2);
    assert_eq!(pkgs[0].id, "starter");
    assert_eq!(pkgs[0].limits.disk_mb, Some(1024));
    assert_eq!(pkgs[0].limits.bandwidth_mb, None);
    assert_eq!(pkgs[0].limits.domains, Some(1));
    assert_eq!(pkgs[0].limits.databases, Some(5));
    assert_eq!(pkgs[1].limits.disk_mb, None);
    assert_eq!(pkgs[1].limits.bandwidth_mb, None);
    assert_eq!(pkgs[1].limits.domains, None);
}

#[tokio::test]
async fn account_info_and_listing() {
    let (server, p) = setup().await;
    let entry = json!({
        "user": "bob", "domain": "bob.test", "plan": "starter", "email": "b@bob.test",
        "ip": "192.0.2.10", "suspended": 1, "suspendreason": "Unpaid", "owner": "root",
        "unix_startdate": 1_700_000_000
    });
    whm("accountsummary")
        .respond_with(ok(json!({"acct": [entry.clone()]})))
        .mount(&server)
        .await;
    whm("listaccts")
        .respond_with(ok(json!({"acct": [entry]})))
        .mount(&server)
        .await;
    let info = p.get_account(&acct("bob")).await.unwrap();
    assert!(info.suspended);
    assert_eq!(info.suspend_reason.as_deref(), Some("Unpaid"));
    assert_eq!(info.package.as_deref(), Some("starter"));
    assert_eq!(info.created_at.unwrap().timestamp(), 1_700_000_000);
    assert_eq!(p.list_accounts().await.unwrap().len(), 1);
}

#[tokio::test]
async fn slow_server_times_out() {
    let server = MockServer::start().await;
    let p = panel(
        kinds::CPANEL,
        &server,
        json!({"timeout_secs": 1}),
        json!({"api_token": "TOKEN123"}),
    );
    whm("version")
        .respond_with(ok(json!({"version": "1"})).set_delay(std::time::Duration::from_secs(3)))
        .mount(&server)
        .await;
    let err = p.test_connection().await.unwrap_err();
    assert_eq!(err, IntegrationError::Timeout);
}

#[tokio::test]
async fn unreachable_server_is_transport_error() {
    // Bind and immediately release a port so nothing listens there.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let p = hos_panels::build_with_policy(
        kinds::CPANEL,
        &json!({"host": "127.0.0.1", "port": port, "use_ssl": false}),
        &json!({"api_token": "TOKEN123"}),
        hos_panels::OutboundPolicy::Unrestricted,
    )
    .unwrap();
    let err = p.test_connection().await.unwrap_err();
    assert!(
        matches!(&err, IntegrationError::Transport(m) if m.contains("cannot connect")),
        "{err}"
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn old_whm_without_verify_new_username_still_creates() {
    let (server, p) = setup().await;
    whm("verify_new_username")
        .respond_with(fail(
            "Unknown app (verify_new_username) requested for this version (1) of the API.",
        ))
        .expect(1)
        .mount(&server)
        .await;
    whm("createacct")
        .and(form(&[("username", "shop")]))
        .respond_with(ok(json!({"ip": "192.0.2.10"})))
        .expect(1)
        .mount(&server)
        .await;
    let created = p
        .create_account(&spec("", "shop.example", "starter"))
        .await
        .unwrap();
    assert_eq!(created.account.username, "shop");
}
