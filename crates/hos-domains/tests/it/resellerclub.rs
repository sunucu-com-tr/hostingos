//! ResellerClub against a wiremock server (no real network).

use crate::support::{form, paths, query, registration, value, values};
use chrono::{TimeZone, Utc};
use hos_core::IntegrationError;
use hos_core::domain::DomainStatus;
use hos_core::integrations::registrar::{
    DnsRecord, DnsRecordType, DomainOperationStatus, DomainRegistrar, DomainRenewal, DomainTransfer,
};
use hos_core::secret::SecretString;
use hos_domains::OutboundPolicy;
use hos_domains::resellerclub::{ResellerClub, ResellerClubConfig};
use serde_json::json;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "RC-SECRET-KEY-0123456789";

fn client(server: &MockServer) -> ResellerClub {
    let base = Url::parse(&format!("{}/api/", server.uri())).unwrap();
    let mut cfg = ResellerClubConfig::new(base, "424242", SecretString::from(KEY));
    cfg.currency = Some(hos_core::money::Currency::USD);
    cfg.outbound = OutboundPolicy::Unrestricted;
    ResellerClub::new(cfg).unwrap()
}

fn error_body(msg: &str) -> ResponseTemplate {
    ResponseTemplate::new(500).set_body_json(json!({"status": "ERROR", "message": msg}))
}

#[tokio::test]
async fn availability_batches_and_maps_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/domains/available.json"))
        .and(query_param("auth-userid", "424242"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ornek.com": {"classkey": "domcno", "status": "available"},
            "ornek.net": {"classkey": "dotnet", "status": "regthroughothers"},
            "lux.com": {"classkey": "domcno", "status": "available",
                        "costHash": {"create": "1200.00", "renew": "12.00", "sellingCurrencySymbol": "USD"}}
        })))
        .expect(2)
        .mount(&server)
        .await;
    let rc = client(&server);
    let res = rc
        .check_availability(&[
            "ornek.com".into(),
            "Ornek.NET".into(),
            "bad_name.com".into(),
            "lux.com".into(),
        ])
        .await
        .unwrap();
    assert_eq!(res.len(), 4);
    assert!(res[0].available);
    assert_eq!(res[1].domain, "ornek.net");
    assert!(!res[1].available);
    assert!(!res[2].available && res[2].reason.is_some());
    assert!(res[3].available && res[3].premium);
    assert_eq!(
        res[3].register_price.unwrap().to_decimal_string(),
        "1200.00"
    );
    let reqs = server.received_requests().await.unwrap();
    // "ornek" has {com, net}; "lux" has {com} → two calls.
    let q = query(&reqs[0]);
    assert_eq!(value(&q, "api-key").as_deref(), Some(KEY));
    let all_tlds: Vec<Vec<String>> = reqs.iter().map(|r| values(&query(r), "tlds")).collect();
    assert!(all_tlds.contains(&vec!["com".to_owned(), "net".to_owned()]));
    assert!(all_tlds.contains(&vec!["com".to_owned()]));
}

#[tokio::test]
async fn registration_creates_customer_and_contact_then_registers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/customers/details.json"))
        .respond_with(error_body("Customer ada@example.com not found"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/customers/signup.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("12345"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/contacts/search.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"recsonpage": "0", "recsindb": "0", "result": []})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/contacts/add.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("777"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/domains/register.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "description": "ornek.com", "entityid": "9001", "actiontype": "AddNewDomain",
            "actionstatus": "Success", "actionstatusdesc": "Domain registration completed Successfully"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let rc = client(&server);
    let op = rc.register(&registration("ornek.com", 2)).await.unwrap();
    assert_eq!(op.status, DomainOperationStatus::Completed);
    assert_eq!(op.registrar_ref.as_deref(), Some("9001"));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        paths(&reqs),
        vec![
            "/api/customers/details.json",
            "/api/customers/signup.json",
            "/api/contacts/search.json",
            "/api/contacts/add.json",
            "/api/domains/register.json",
        ]
    );
    // Mutations are POST with credentials in the body, never in the URL.
    for r in reqs.iter().filter(|r| r.method.as_str() == "POST") {
        assert!(r.url.query().is_none(), "POST with query string: {}", r.url);
        assert_eq!(value(&form(r), "api-key").as_deref(), Some(KEY));
    }
    let signup = form(&reqs[1]);
    assert_eq!(
        value(&signup, "username").as_deref(),
        Some("ada@example.com")
    );
    assert_eq!(value(&signup, "phone-cc").as_deref(), Some("90"));
    assert_eq!(value(&signup, "passwd").map(|p| p.len()), Some(15));
    let add = form(&reqs[3]);
    assert_eq!(value(&add, "customer-id").as_deref(), Some("12345"));
    assert_eq!(value(&add, "type").as_deref(), Some("Contact"));
    assert_eq!(value(&add, "company").as_deref(), Some("N/A"));
    let reg = form(&reqs[4]);
    assert_eq!(
        values(&reg, "ns"),
        vec!["ns1.host.example", "ns2.host.example"]
    );
    for k in [
        "reg-contact-id",
        "admin-contact-id",
        "tech-contact-id",
        "billing-contact-id",
    ] {
        assert_eq!(value(&reg, k).as_deref(), Some("777"), "{k}");
    }
    assert_eq!(value(&reg, "customer-id").as_deref(), Some("12345"));
    assert_eq!(value(&reg, "invoice-option").as_deref(), Some("NoInvoice"));
    assert_eq!(value(&reg, "years").as_deref(), Some("2"));
    assert_eq!(value(&reg, "purchase-privacy").as_deref(), Some("true"));
    assert_eq!(value(&reg, "auto-renew").as_deref(), Some("false"));
}

#[tokio::test]
async fn registration_reuses_existing_customer_and_contact() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/customers/details.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"customerid": "555", "username": "ada@example.com"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/contacts/search.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "recsonpage": "1", "recsindb": "1",
            "result": [{
                "entity.entityid": "31337", "contact.name": "Ada  Yılmaz", "contact.company": "N/A",
                "contact.emailaddr": "ADA@example.com", "contact.address1": "Atatürk Cad. 12",
                "contact.city": "İstanbul", "contact.country": "TR", "contact.zip": "34000",
                "contact.telnocc": "90", "contact.telno": "5551234567",
                "entity.currentstatus": "Active", "contact.type": "Contact"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/domains/register.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entityid": "9002", "actionstatus": "PendingExecution", "actionstatusdesc": "queued"
        })))
        .mount(&server)
        .await;
    let rc = client(&server);
    let op = rc.register(&registration("ornek.com", 1)).await.unwrap();
    assert_eq!(op.status, DomainOperationStatus::Pending);
    let reqs = server.received_requests().await.unwrap();
    assert!(
        !paths(&reqs)
            .iter()
            .any(|p| p.ends_with("contacts/add.json") || p.ends_with("signup.json"))
    );
    let reg = form(reqs.last().unwrap());
    assert_eq!(value(&reg, "reg-contact-id").as_deref(), Some("31337"));
    assert_eq!(value(&reg, "customer-id").as_deref(), Some("555"));
}

#[tokio::test]
async fn uk_registration_sends_minus_one_for_unused_roles() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/customers/details.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"customerid": "555"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/contacts/search.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/contacts/add.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("88"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/domains/register.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"entityid": "1", "actionstatus": "Success"})),
        )
        .mount(&server)
        .await;
    let rc = client(&server);
    rc.register(&registration("ornek.co.uk", 1)).await.unwrap();
    let reqs = server.received_requests().await.unwrap();
    let add = form(
        reqs.iter()
            .find(|r| r.url.path().ends_with("contacts/add.json"))
            .unwrap(),
    );
    assert_eq!(value(&add, "type").as_deref(), Some("UkContact"));
    let reg = form(reqs.last().unwrap());
    assert_eq!(value(&reg, "reg-contact-id").as_deref(), Some("88"));
    for k in ["admin-contact-id", "tech-contact-id", "billing-contact-id"] {
        assert_eq!(value(&reg, k).as_deref(), Some("-1"), "{k}");
    }
    // .uk has no privacy service at ResellerClub.
    assert_eq!(value(&reg, "purchase-privacy"), None);
}

#[tokio::test]
async fn renew_uses_known_expiry_and_order_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/domains/orderid.json"))
        .and(query_param("domain-name", "ornek.com"))
        .respond_with(ResponseTemplate::new(200).set_body_string("9001"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/domains/renew.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "entityid": "9001", "actionstatus": "Success", "actionstatusdesc": "Renewed",
            "sellingamount": "-18.40", "sellingcurrencysymbol": "USD"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let rc = client(&server);
    let expiry = Utc.with_ymd_and_hms(2027, 3, 1, 10, 0, 0).unwrap();
    let op = rc
        .renew(&DomainRenewal {
            domain: "ornek.com".into(),
            years: 2,
            current_expiry: Some(expiry),
            restore_if_needed: false,
        })
        .await
        .unwrap();
    assert_eq!(op.status, DomainOperationStatus::Completed);
    assert_eq!(
        op.expires_at,
        Some(Utc.with_ymd_and_hms(2029, 3, 1, 10, 0, 0).unwrap())
    );
    assert_eq!(op.cost.unwrap().to_decimal_string(), "18.40");
    let reqs = server.received_requests().await.unwrap();
    let body = form(&reqs[1]);
    assert_eq!(value(&body, "order-id").as_deref(), Some("9001"));
    assert_eq!(
        value(&body, "exp-date"),
        Some(expiry.timestamp().to_string())
    );
    assert_eq!(value(&body, "years").as_deref(), Some("2"));
}

#[tokio::test]
async fn renew_without_expiry_reads_details() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/domains/details-by-name.json"))
        .and(query_param("options", "OrderDetails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "orderid": "9001", "currentstatus": "Active", "endtime": "1803902400", "recurring": "false"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/domains/renew.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"entityid": "9001", "actionstatus": "Success"})),
        )
        .mount(&server)
        .await;
    let rc = client(&server);
    rc.renew(&DomainRenewal {
        domain: "ornek.com".into(),
        years: 1,
        current_expiry: None,
        restore_if_needed: false,
    })
    .await
    .unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        value(&form(&reqs[1]), "exp-date").as_deref(),
        Some("1803902400")
    );
}

#[tokio::test]
async fn nameserver_update_and_info() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/domains/orderid.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("9001"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/domains/modify-ns.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"entityid": "9001", "actionstatus": "Success"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/domains/details-by-name.json"))
        .and(query_param("options", "All"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "orderid": "9001", "domainname": "ornek.com", "currentstatus": "Active",
            "orderstatus": ["transferlock"], "domainstatus": [],
            "creationtime": "1700000000", "endtime": "4102444800",
            "ns1": "ns1.new.example", "ns2": "ns2.new.example", "noOfNameServers": "2",
            "isprivacyprotected": "true", "recurring": "false", "domsecret": "S3cr3t!",
            "registrantcontact": {"contactid": "777", "name": "Ada Yılmaz", "company": "N/A",
                "emailaddr": "ada@example.com", "address1": "Atatürk Cad. 12", "city": "İstanbul",
                "country": "TR", "zip": "34000", "telnocc": "90", "telno": "5551234567"}
        })))
        .mount(&server)
        .await;
    let rc = client(&server);
    rc.set_nameservers(
        "ornek.com",
        &["NS1.New.Example".into(), "ns2.new.example".into()],
    )
    .await
    .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let ns_body = form(
        reqs.iter()
            .find(|r| r.url.path().ends_with("modify-ns.json"))
            .unwrap(),
    );
    assert_eq!(
        values(&ns_body, "ns"),
        vec!["ns1.new.example", "ns2.new.example"]
    );
    assert_eq!(value(&ns_body, "order-id").as_deref(), Some("9001"));

    let info = rc.info("ornek.com").await.unwrap();
    assert_eq!(info.status, DomainStatus::Active);
    assert_eq!(info.locked, Some(true));
    assert_eq!(info.privacy, Some(true));
    assert_eq!(info.nameservers, vec!["ns1.new.example", "ns2.new.example"]);
    assert_eq!(info.contacts.unwrap().registrant.last_name, "Yılmaz");
    // The auth code is never copied into `extra`.
    assert!(!info.extra.to_string().contains("S3cr3t"));

    assert!(rc.set_nameservers("ornek.com", &[]).await.is_err());
}

#[tokio::test]
async fn errors_are_mapped() {
    let server = MockServer::start().await;
    Mock::given(path("/api/domains/details-by-name.json"))
        .respond_with(error_body("Website doesn't exist for nope.com"))
        .mount(&server)
        .await;
    Mock::given(path("/api/domains/orderid.json"))
        .respond_with(error_body(
            "Access Denied. You are not allowed to perform this action",
        ))
        .mount(&server)
        .await;
    Mock::given(path("/api/resellers/details.json"))
        .respond_with(ResponseTemplate::new(503).set_body_string("<html>maintenance</html>"))
        .mount(&server)
        .await;
    let rc = client(&server);
    assert!(matches!(
        rc.info("nope.com").await,
        Err(IntegrationError::NotFound(_))
    ));
    let auth = rc.set_lock("ornek.com", true).await.unwrap_err();
    assert_eq!(auth.kind(), "auth");
    assert!(auth.to_string().contains("whitelisted"));
    let down = rc.test_connection().await.unwrap_err();
    assert!(down.is_retryable(), "{down:?}");
    // Business errors (HTTP 500 + status ERROR) are not retryable.
    assert!(!rc.info("nope.com").await.unwrap_err().is_retryable());
}

#[tokio::test]
async fn secrets_never_leak_into_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/api/domains/details-by-name.json"))
        .respond_with(error_body(&format!(
            "Invalid api-key={KEY} for user 424242 (key {KEY})"
        )))
        .mount(&server)
        .await;
    Mock::given(path("/api/domains/transfer.json"))
        .respond_with(error_body("Invalid auth-code AUTH-CODE-XYZ supplied"))
        .mount(&server)
        .await;
    Mock::given(path("/api/customers/details.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"customerid": "1"})))
        .mount(&server)
        .await;
    Mock::given(path("/api/contacts/search.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&server)
        .await;
    Mock::given(path("/api/contacts/add.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("2"))
        .mount(&server)
        .await;
    let rc = client(&server);
    let err = rc.info("ornek.com").await.unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains(KEY), "{text}");
    let transfer = DomainTransfer {
        domain: "ornek.com".into(),
        auth_code: SecretString::from("AUTH-CODE-XYZ"),
        years: 1,
        contacts: registration("ornek.com", 1).contacts,
        ..DomainTransfer::default()
    };
    let err = rc.transfer(&transfer).await.unwrap_err();
    assert!(!err.to_string().contains("AUTH-CODE-XYZ"), "{err}");

    // Transport errors must not contain the URL (which carries the key for GETs).
    let mut dead_cfg = ResellerClubConfig::new(
        Url::parse("http://127.0.0.1:9/api/").unwrap(),
        "1",
        SecretString::from(KEY),
    );
    dead_cfg.outbound = OutboundPolicy::Unrestricted;
    let dead = ResellerClub::new(dead_cfg).unwrap();
    let err = dead.info("ornek.com").await.unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(err.is_retryable());
    assert!(!text.contains(KEY) && !text.contains("api-key"), "{text}");
}

#[tokio::test]
async fn dns_records_add_list_delete() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/dns/manage/add-ipv4-record.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "Success"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/dns/manage/search-records.json"))
        .and(query_param("type", "A"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "recsonpage": "1", "recsindb": "1",
            "1": {"host": "www.ornek.com", "value": "192.0.2.10", "type": "A", "timetolive": "14400", "status": "Active"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/dns/manage/search-records.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"recsonpage": "0", "recsindb": "0"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/dns/manage/delete-ipv4-record.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "Success"})))
        .expect(1)
        .mount(&server)
        .await;
    let rc = client(&server);
    let rec = DnsRecord {
        id: None,
        name: "www".into(),
        record_type: DnsRecordType::A,
        value: "192.0.2.10".into(),
        ttl: Some(7200),
        priority: None,
    };
    let stored = rc.upsert_dns_record("ornek.com", &rec).await.unwrap();
    assert!(stored.id.is_some());
    let list = rc.list_dns_records("ornek.com").await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "www");
    rc.delete_dns_record("ornek.com", &list[0]).await.unwrap();
    let caa = DnsRecord {
        record_type: DnsRecordType::Caa,
        ..rec
    };
    assert!(matches!(
        rc.upsert_dns_record("ornek.com", &caa).await,
        Err(IntegrationError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn tld_pricing_maps_product_keys() {
    let server = MockServer::start().await;
    Mock::given(path("/api/products/category-keys-mapping.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "domorder": [{"domcno": ["com"]}, {"dotnet": ["net"]}, {"thirdleveldotuk": ["co.uk", "org.uk"]}],
            "hosting": [{"x": ["y"]}]
        })))
        .mount(&server)
        .await;
    Mock::given(path("/api/products/reseller-cost-price.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "domcno": {
                "addnewdomain": {"1": "9.25", "2": "9.25", "10": "9.25"},
                "renewdomain": {"1": "9.50"},
                "addtransferdomain": {"1": "9.25"},
                "restoredomain": {"1": "80"}
            },
            "thirdleveldotuk": {"addnewdomain": {"1": "5.00", "2": "5.00"}, "renewdomain": {"1": "5.00"}}
        })))
        .mount(&server)
        .await;
    let rc = client(&server);
    let all = rc.tld_pricing(&[]).await.unwrap();
    let com = all.iter().find(|p| p.tld == "com").unwrap();
    assert_eq!(com.register.unwrap().to_decimal_string(), "9.25");
    assert_eq!(com.renew.unwrap().to_decimal_string(), "9.50");
    assert_eq!(com.restore.unwrap().to_decimal_string(), "80.00");
    assert_eq!((com.min_years, com.max_years), (1, 10));
    assert!(all.iter().any(|p| p.tld == "org.uk"));
    assert!(!all.iter().any(|p| p.tld == "net"));
    let only = rc.tld_pricing(&[".CO.UK".into()]).await.unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].tld, "co.uk");
}

// ---------------------------------------------------------------------------
// Outbound policy, redirects and error-message hygiene.
// ---------------------------------------------------------------------------

#[test]
fn base_url_is_checked_against_the_outbound_policy() {
    for base in [
        "http://127.0.0.1:8080/api/",
        "http://2130706433/api/",
        "http://[::ffff:169.254.169.254]/api/",
        "http://localhost/api/",
    ] {
        let mut cfg =
            ResellerClubConfig::new(Url::parse(base).unwrap(), "424242", SecretString::from(KEY));
        cfg.outbound = OutboundPolicy::Private;
        match ResellerClub::new(cfg) {
            Err(IntegrationError::Config(m)) => assert!(m.contains("HOS_OUTBOUND_POLICY"), "{m}"),
            other => panic!("{base}: expected Config, got {other:?}"),
        }
    }
    // Through the factory (tenant-supplied `base_url`).
    let built = hos_domains::build_with_policy(
        "resellerclub",
        &json!({"reseller_id": "424242", "base_url": "http://10.0.0.9/api/"}),
        &json!({"api_key": KEY}),
        OutboundPolicy::Public,
    );
    assert!(matches!(built, Err(IntegrationError::Config(_))));
}

#[tokio::test]
async fn redirects_are_not_followed_and_bodies_are_not_quoted() {
    let server = MockServer::start().await;
    Mock::given(path("/api/resellers/details.json"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/internal", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(path("/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(0)
        .mount(&server)
        .await;
    assert!(client(&server).test_connection().await.is_err());

    let server = MockServer::start().await;
    Mock::given(path("/api/resellers/details.json"))
        .respond_with(ResponseTemplate::new(502).set_body_string("SECRET-INTERNAL-PAGE contents"))
        .mount(&server)
        .await;
    let err = client(&server).test_connection().await.unwrap_err();
    assert!(!err.to_string().contains("SECRET-INTERNAL-PAGE"), "{err}");
    assert!(err.to_string().contains("502"), "{err}");
}
