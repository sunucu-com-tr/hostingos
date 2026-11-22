//! OpenProvider against a wiremock server (no real network).

use crate::support::{paths, registration};
use chrono::{TimeZone, Utc};
use hos_core::IntegrationError;
use hos_core::domain::DomainStatus;
use hos_core::integrations::registrar::{
    DnsRecord, DnsRecordType, DomainOperationStatus, DomainRegistrar, DomainRenewal,
};
use hos_core::secret::SecretString;
use hos_domains::OutboundPolicy;
use hos_domains::openprovider::{OpenProvider, OpenProviderConfig};
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{bearer_token, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "OP-SECRET-PASSWORD-42";

fn client(server: &MockServer) -> OpenProvider {
    let base = Url::parse(&format!("{}/v1beta/", server.uri())).unwrap();
    let mut cfg = OpenProviderConfig::new(base, "apiuser", SecretString::from(PASSWORD));
    cfg.outbound = OutboundPolicy::Unrestricted;
    OpenProvider::new(cfg).unwrap()
}

fn ok(data: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"code": 0, "desc": "", "data": data}))
}

fn op_err(http: u16, code: i64, desc: &str) -> ResponseTemplate {
    ResponseTemplate::new(http).set_body_json(json!({"code": code, "desc": desc, "data": null}))
}

async fn mount_login(server: &MockServer, token: &str) {
    Mock::given(method("POST"))
        .and(path("/v1beta/auth/login"))
        .respond_with(ok(json!({"token": token, "reseller_id": 7})))
        .mount(server)
        .await;
}

fn body(req: &wiremock::Request) -> Value {
    req.body_json().unwrap()
}

#[tokio::test]
async fn login_then_check_availability() {
    let server = MockServer::start().await;
    mount_login(&server, "tok-1").await;
    Mock::given(method("POST"))
        .and(path("/v1beta/domains/check"))
        .and(bearer_token("tok-1"))
        .respond_with(ok(json!({"results": [
            {"domain": "ornek.com", "status": "free", "is_premium": false,
             "price": {"product": {"price": 9.5, "currency": "USD"}, "reseller": {"price": 8.1, "currency": "EUR"}}},
            {"domain": "google.com", "status": "active", "reason": "Domain exists"},
            {"domain": "lux.io", "status": "free", "is_premium": true,
             "premium": {"currency": "USD", "price": {"create": 1999.0, "renew": 49.0}}}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let op = client(&server);
    let res = op
        .check_availability(&["ornek.com".into(), "google.com".into(), "lux.io".into()])
        .await
        .unwrap();
    assert!(res[0].available);
    assert_eq!(res[0].register_price.unwrap().to_decimal_string(), "8.10");
    assert!(!res[1].available);
    assert_eq!(res[1].reason.as_deref(), Some("active: Domain exists"));
    assert!(res[2].premium);
    assert_eq!(
        res[2].register_price.unwrap().to_decimal_string(),
        "1999.00"
    );
    let reqs = server.received_requests().await.unwrap();
    let login = body(&reqs[0]);
    assert_eq!(login["username"], "apiuser");
    let check = body(&reqs[1]);
    assert_eq!(check["with_price"], true);
    assert_eq!(
        check["domains"][0],
        json!({"name": "ornek", "extension": "com"})
    );
}

#[tokio::test]
async fn registration_creates_handle_once_and_registers() {
    let server = MockServer::start().await;
    mount_login(&server, "tok").await;
    Mock::given(method("GET"))
        .and(path("/v1beta/customers"))
        .and(query_param("email_pattern", "ada@example.com"))
        .respond_with(ok(json!({"results": [], "total": 0})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1beta/customers"))
        .respond_with(ok(json!({"handle": "AY000001-NL"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1beta/domains"))
        .respond_with(ok(json!({
            "id": 4711, "status": "ACT", "activation_date": "2026-11-16 10:00:00",
            "expiration_date": "2028-11-16 10:00:00", "renewal_date": "2028-11-16 10:00:00"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let op = client(&server);
    let res = op.register(&registration("ornek.com", 2)).await.unwrap();
    assert_eq!(res.status, DomainOperationStatus::Completed);
    assert_eq!(res.registrar_ref.as_deref(), Some("4711"));
    assert_eq!(
        res.expires_at,
        Some(Utc.with_ymd_and_hms(2028, 11, 16, 10, 0, 0).unwrap())
    );
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        paths(&reqs),
        vec![
            "/v1beta/auth/login",
            "/v1beta/customers",
            "/v1beta/customers",
            "/v1beta/domains"
        ]
    );
    let customer = body(&reqs[2]);
    assert_eq!(customer["name"]["first_name"], "Ada");
    assert_eq!(customer["address"]["street"], "Atatürk Cad.");
    assert_eq!(customer["address"]["number"], "12");
    assert_eq!(customer["phone"]["country_code"], "+90");
    let domain = body(&reqs[3]);
    assert_eq!(
        domain["domain"],
        json!({"name": "ornek", "extension": "com"})
    );
    assert_eq!(domain["period"], 2);
    for h in [
        "owner_handle",
        "admin_handle",
        "tech_handle",
        "billing_handle",
    ] {
        assert_eq!(domain[h], "AY000001-NL", "{h}");
    }
    assert_eq!(domain["name_servers"][1]["name"], "ns2.host.example");
    assert_eq!(domain["autorenew"], "off");
    assert_eq!(domain["is_private_whois_enabled"], true);
}

#[tokio::test]
async fn existing_handle_is_reused() {
    let server = MockServer::start().await;
    mount_login(&server, "tok").await;
    Mock::given(method("GET"))
        .and(path("/v1beta/customers"))
        .respond_with(ok(json!({"total": 1, "results": [{
            "handle": "AY000009-NL",
            "name": {"first_name": "Ada", "last_name": "Yılmaz"},
            "address": {"street": "Atatürk Cad.", "number": "12", "zipcode": "34000", "city": "İstanbul", "state": "İstanbul", "country": "TR"},
            "phone": {"country_code": "+90", "area_code": "555", "subscriber_number": "1234567"},
            "email": "ada@example.com"
        }]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1beta/domains"))
        .respond_with(ok(json!({"id": 1, "status": "REQ"})))
        .mount(&server)
        .await;
    let op = client(&server);
    let res = op.register(&registration("ornek.com", 1)).await.unwrap();
    assert_eq!(res.status, DomainOperationStatus::Pending);
    let reqs = server.received_requests().await.unwrap();
    assert!(
        !reqs
            .iter()
            .any(|r| r.method.as_str() == "POST" && r.url.path() == "/v1beta/customers")
    );
    assert_eq!(body(reqs.last().unwrap())["owner_handle"], "AY000009-NL");
}

async fn mount_domain_lookup(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1beta/domains"))
        .and(query_param("full_name", "ornek.com"))
        .respond_with(ok(json!({"total": 1, "results": [{"id": 42, "domain": {"name": "ornek", "extension": "com"}}]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/domains/42"))
        .respond_with(ok(json!({
            "id": 42, "domain": {"name": "ornek", "extension": "com"}, "status": "ACT", "type": "NEW",
            "expiration_date": "2027-03-01 10:00:00", "creation_date": "2020-03-01 10:00:00",
            "name_servers": [{"name": "ns1.x.example", "seq_nr": 1}, {"name": "ns2.x.example", "seq_nr": 2}],
            "is_locked": true, "is_private_whois_enabled": false, "autorenew": "off",
            "auth_code": "Never-Copied-1"
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn renew_info_and_nameservers() {
    let server = MockServer::start().await;
    mount_login(&server, "tok").await;
    mount_domain_lookup(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1beta/domains/42/renew"))
        .respond_with(ok(json!({"status": "ACT"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1beta/domains/42"))
        .respond_with(ok(json!({"id": 42, "status": "ACT"})))
        .expect(1)
        .mount(&server)
        .await;
    let op = client(&server);
    let res = op
        .renew(&DomainRenewal {
            domain: "ornek.com".into(),
            years: 1,
            current_expiry: None,
            restore_if_needed: false,
        })
        .await
        .unwrap();
    assert_eq!(
        res.expires_at,
        Some(Utc.with_ymd_and_hms(2028, 3, 1, 10, 0, 0).unwrap())
    );
    let info = op.info("ornek.com").await.unwrap();
    assert_eq!(info.status, DomainStatus::Active);
    assert_eq!(info.locked, Some(true));
    assert_eq!(info.auto_renew, Some(false));
    assert_eq!(info.nameservers, vec!["ns1.x.example", "ns2.x.example"]);
    assert!(!info.extra.to_string().contains("Never-Copied"));
    op.set_nameservers(
        "ornek.com",
        &["ns1.new.example".into(), "ns2.new.example".into()],
    )
    .await
    .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let renew = reqs
        .iter()
        .find(|r| r.url.path().ends_with("/renew"))
        .unwrap();
    assert_eq!(body(renew)["period"], 1);
    let put = reqs.iter().find(|r| r.method.as_str() == "PUT").unwrap();
    assert_eq!(
        body(put)["name_servers"],
        json!([{"name": "ns1.new.example", "seq_nr": 1}, {"name": "ns2.new.example", "seq_nr": 2}])
    );
    // The id lookup is cached: only one GET /domains?full_name=.
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/v1beta/domains" && r.method.as_str() == "GET")
            .count(),
        1
    );
}

#[tokio::test]
async fn expired_token_triggers_one_relogin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/auth/login"))
        .respond_with(ok(json!({"token": "tok"})))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/domains"))
        .respond_with(op_err(500, 196, "Authentication/Authorization Failed"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/domains"))
        .respond_with(ok(json!({"results": [{"id": 9}]})))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/domains/9/authcode"))
        .respond_with(ok(
            json!({"auth_code": "Ep9!code", "type": "external", "success": true}),
        ))
        .mount(&server)
        .await;
    let op = client(&server);
    let code = op.get_auth_code("ornek.com").await.unwrap();
    assert_eq!(code.expose_str(), "Ep9!code");
}

#[tokio::test]
async fn errors_are_mapped_without_leaking() {
    let server = MockServer::start().await;
    mount_login(&server, "tok").await;
    Mock::given(method("GET"))
        .and(path("/v1beta/domains"))
        .respond_with(ok(json!({"results": [], "total": 0})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/tlds/com"))
        .respond_with(op_err(
            400,
            10005,
            &format!("Access denied for password {PASSWORD}"),
        ))
        .mount(&server)
        .await;
    let op = client(&server);
    assert!(matches!(
        op.info("ornek.com").await,
        Err(IntegrationError::NotFound(_))
    ));
    let err = op.tld_pricing(&["com".into()]).await.unwrap_err();
    assert_eq!(err.kind(), "auth");
    assert!(!format!("{err} {err:?}").contains(PASSWORD), "{err}");

    // Bad credentials at login.
    let server2 = MockServer::start().await;
    Mock::given(path("/v1beta/auth/login"))
        .respond_with(op_err(401, 196, "Authentication/Authorization Failed"))
        .mount(&server2)
        .await;
    let err = client(&server2).test_connection().await.unwrap_err();
    assert_eq!(err.kind(), "auth");
}

#[tokio::test]
async fn tld_prices_and_dns() {
    let server = MockServer::start().await;
    mount_login(&server, "tok").await;
    Mock::given(method("GET"))
        .and(path("/v1beta/tlds/com"))
        .and(query_param("with_price", "true"))
        .respond_with(ok(json!({
            "name": "com", "min_period": 1, "max_period": 10,
            "prices": {
                "create_price": {"product": {"price": 9.99, "currency": "USD"}, "reseller": {"price": 7.5, "currency": "EUR"}},
                "renew_price": {"reseller": {"price": 7.6, "currency": "EUR"}},
                "transfer_price": {"reseller": {"price": 7.5, "currency": "EUR"}},
                "restore_price": {"reseller": {"price": 70, "currency": "EUR"}}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/dns/zones/ornek.com"))
        .respond_with(ok(json!({"name": "ornek.com", "records": [
            {"name": "ornek.com", "type": "SOA", "value": "ns1 hostmaster 1 2 3 4 5", "ttl": 86400},
            {"name": "www.ornek.com", "type": "A", "value": "192.0.2.5", "ttl": 3600, "prio": 0},
            {"name": "ornek.com", "type": "MX", "value": "mx.ornek.com", "ttl": 3600, "prio": 10}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1beta/dns/zones/ornek.com"))
        .respond_with(ok(json!({"success": true})))
        .expect(2)
        .mount(&server)
        .await;
    let op = client(&server);
    let prices = op.tld_pricing(&["com".into()]).await.unwrap();
    assert_eq!(prices[0].register.unwrap().to_decimal_string(), "7.50");
    assert_eq!(prices[0].restore.unwrap().to_decimal_string(), "70.00");
    assert_eq!(prices[0].max_years, 10);

    let records = op.list_dns_records("ornek.com").await.unwrap();
    assert_eq!(records.len(), 2, "SOA is not a managed record type");
    let www = records.iter().find(|r| r.name == "www").unwrap();
    let updated = op
        .upsert_dns_record(
            "ornek.com",
            &DnsRecord {
                value: "192.0.2.6".into(),
                ttl: Some(300),
                ..www.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.ttl, Some(900));
    op.upsert_dns_record(
        "ornek.com",
        &DnsRecord {
            id: None,
            name: "@".into(),
            record_type: DnsRecordType::Txt,
            value: "v=spf1 -all".into(),
            ttl: None,
            priority: None,
        },
    )
    .await
    .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let puts: Vec<Value> = reqs
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(body)
        .collect();
    assert_eq!(
        puts[0]["records"]["update"][0]["original_record"],
        json!({"name": "www", "type": "A", "value": "192.0.2.5", "ttl": 3600})
    );
    assert_eq!(puts[0]["records"]["update"][0]["record"]["ttl"], 900);
    assert_eq!(puts[1]["records"]["add"][0]["name"], "");
    assert_eq!(puts[1]["records"]["add"][0]["type"], "TXT");
}
