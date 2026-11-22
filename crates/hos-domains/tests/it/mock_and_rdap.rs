//! Mock registrar end-to-end flow and RDAP lookups (wiremock).

use std::time::Duration;

use crate::support::{contact, registration};
use chrono::{TimeZone, Utc};
use hos_core::IntegrationError;
use hos_core::domain::DomainStatus;
use hos_core::integrations::registrar::{
    DnsRecord, DnsRecordType, DomainOperationStatus, DomainRegistrar, DomainRenewal, DomainTransfer,
};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use hos_domains::rdap::{RdapClient, RdapResult};
use hos_domains::{MockRegistrar, OutboundPolicy, build, build_with_policy};
use rust_decimal_macros::dec;
use serde_json::json;
use url::Url;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn mock_registrar_full_lifecycle() {
    let now = Utc.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap();
    let r = MockRegistrar::new(Currency::USD).with_now(now);
    let avail = r
        .check_availability(&[
            "ornek.com".into(),
            "taken-site.com".into(),
            "premium-x.com".into(),
            "reserved-x.com".into(),
            "ornek.museum".into(),
        ])
        .await
        .unwrap();
    assert!(avail[0].available);
    assert!(!avail[1].available);
    assert!(avail[2].available && avail[2].premium);
    assert!(!avail[3].available);
    assert!(!avail[4].available);

    let op = r.register(&registration("ornek.com", 2)).await.unwrap();
    assert_eq!(op.status, DomainOperationStatus::Completed);
    assert_eq!(op.cost, Some(Money::new(dec!(18.40), Currency::USD)));
    assert!(matches!(
        r.register(&registration("ornek.com", 1)).await,
        Err(IntegrationError::Conflict(_))
    ));
    // Premium names need an accepted price.
    assert!(r.register(&registration("premium-x.com", 1)).await.is_err());
    let mut prem = registration("premium-x.com", 1);
    prem.premium_price = Some(Money::new(dec!(92), Currency::USD));
    assert!(r.register(&prem).await.is_ok());

    let info = r.info("ornek.com").await.unwrap();
    assert_eq!(info.status, DomainStatus::Active);
    assert_eq!(
        info.expires_at,
        Some(Utc.with_ymd_and_hms(2028, 11, 16, 12, 0, 0).unwrap())
    );

    // Renewal checks the expiry HostingOS believes in.
    let wrong = DomainRenewal {
        domain: "ornek.com".into(),
        years: 1,
        current_expiry: Some(now),
        restore_if_needed: false,
    };
    assert!(matches!(
        r.renew(&wrong).await,
        Err(IntegrationError::Conflict(_))
    ));
    let renewed = r
        .renew(&DomainRenewal {
            current_expiry: info.expires_at,
            ..wrong
        })
        .await
        .unwrap();
    assert_eq!(
        renewed.expires_at,
        Some(Utc.with_ymd_and_hms(2029, 11, 16, 12, 0, 0).unwrap())
    );

    r.set_nameservers(
        "ornek.com",
        &["ns1.a.example".into(), "ns2.a.example".into()],
    )
    .await
    .unwrap();
    assert_eq!(
        r.get_nameservers("ornek.com").await.unwrap(),
        vec!["ns1.a.example", "ns2.a.example"]
    );
    r.set_lock("ornek.com", false).await.unwrap();
    assert_eq!(r.info("ornek.com").await.unwrap().locked, Some(false));
    let code = r.get_auth_code("ornek.com").await.unwrap();
    assert_eq!(code, r.get_auth_code("ornek.com").await.unwrap());

    let rec = r
        .upsert_dns_record(
            "ornek.com",
            &DnsRecord {
                id: None,
                name: "www".into(),
                record_type: DnsRecordType::A,
                value: "192.0.2.1".into(),
                ttl: Some(300),
                priority: None,
            },
        )
        .await
        .unwrap();
    assert!(rec.id.as_deref().unwrap().starts_with("mock-"));
    assert_eq!(r.list_dns_records("ornek.com").await.unwrap().len(), 1);
    r.delete_dns_record("ornek.com", &rec).await.unwrap();
    assert!(r.list_dns_records("ornek.com").await.unwrap().is_empty());

    // Pending registration: first info pending, then active.
    r.register(&registration("pending-x.com", 1)).await.unwrap();
    assert_eq!(
        r.info("pending-x.com").await.unwrap().status,
        DomainStatus::PendingRegistration
    );
    assert_eq!(
        r.info("pending-x.com").await.unwrap().status,
        DomainStatus::Active
    );

    // Transfers.
    let transfer = DomainTransfer {
        domain: "taken-site.com".into(),
        auth_code: SecretString::from("EPP-1"),
        years: 1,
        contacts: registration("x.com", 1).contacts,
        ..DomainTransfer::default()
    };
    let t = r.transfer(&transfer).await.unwrap();
    assert_eq!(t.status, DomainOperationStatus::Pending);
    assert_eq!(
        r.info("taken-site.com").await.unwrap().status,
        DomainStatus::PendingTransfer
    );
    let bad = DomainTransfer {
        domain: "taken-other.com".into(),
        auth_code: SecretString::from("invalid"),
        ..transfer.clone()
    };
    assert!(r.transfer(&bad).await.is_err());
    assert!(r.register(&registration("fail-x.com", 1)).await.is_err());
    let mut incomplete = registration("ornek.net", 1);
    incomplete.contacts.registrant = contact();
    incomplete.contacts.registrant.email.clear();
    assert!(matches!(
        r.register(&incomplete).await,
        Err(IntegrationError::InvalidInput(_))
    ));
    assert!(!r.tld_pricing(&[]).await.unwrap().is_empty());
    assert_eq!(
        r.tld_pricing(&["com.tr".into()]).await.unwrap()[0].max_years,
        5
    );
}

#[tokio::test]
async fn built_mock_is_usable_through_trait_object() {
    let r = build("mock", &json!({"currency": "TRY"}), &json!({})).unwrap();
    let p = r.tld_pricing(&["com".into()]).await.unwrap();
    assert_eq!(p[0].register.unwrap().currency, Currency::TRY);
    assert!(r.test_connection().await.is_ok());
}

async fn rdap_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/bootstrap/dns.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "version": "1.0",
            "services": [[["com", "net"], [format!("{}/rdap/", server.uri())]]]
        })))
        // One fetch per client instance (the cache is per instance).
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path("/rdap/domain/example.com"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "objectClassName": "domain",
            "ldhName": "EXAMPLE.COM",
            "events": [{"eventAction": "expiration", "eventDate": "2027-08-13T04:00:00Z"}],
            "nameservers": [{"ldhName": "A.IANA-SERVERS.NET"}],
            "entities": [{"roles": ["registrar"], "vcardArray": ["vcard", [["fn", {}, "text", "Example Registrar"]]]}]
        })))
        .mount(&server)
        .await;
    Mock::given(path("/rdap/domain/free-name.com"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(path("/rdap/domain/busy.net"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn rdap_lookups_with_cached_bootstrap() {
    let server = rdap_server().await;
    let client = RdapClient::with_options_and_policy(
        Some(Url::parse(&format!("{}/bootstrap/dns.json", server.uri())).unwrap()),
        Duration::from_secs(5),
        OutboundPolicy::Unrestricted,
    )
    .unwrap();
    match client.lookup("Example.com").await.unwrap() {
        RdapResult::Registered(r) => {
            assert_eq!(r.registrar.as_deref(), Some("Example Registrar"));
            assert_eq!(r.nameservers, vec!["a.iana-servers.net"]);
            assert_eq!(
                r.expires_at.unwrap().to_rfc3339(),
                "2027-08-13T04:00:00+00:00"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        client.lookup("free-name.com").await.unwrap(),
        RdapResult::Available
    );
    assert!(matches!(
        client.lookup("busy.net").await.unwrap(),
        RdapResult::Unknown { .. }
    ));
    assert!(matches!(
        client.lookup("ornek.com.tr").await.unwrap(),
        RdapResult::Unknown { .. }
    ));
    let many = client
        .check_availability(&[
            "free-name.com".into(),
            "example.com".into(),
            "bad name".into(),
        ])
        .await;
    assert!(many[0].available);
    assert!(!many[1].available);
    assert!(!many[2].available && many[2].reason.is_some());
    let bootstrap_fetches = |reqs: &[wiremock::Request]| {
        reqs.iter()
            .filter(|r| r.url.path() == "/bootstrap/dns.json")
            .count()
    };
    assert_eq!(
        bootstrap_fetches(&server.received_requests().await.unwrap()),
        1
    );

    let reg = build_with_policy(
        "rdap",
        &json!({"bootstrap_url": format!("{}/bootstrap/dns.json", server.uri())}),
        &json!({}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap();
    let info = reg.info("example.com").await.unwrap();
    assert_eq!(info.extra["registrar"], "Example Registrar");
    assert!(matches!(
        reg.info("free-name.com").await,
        Err(IntegrationError::NotFound(_))
    ));
    assert!(matches!(
        reg.register(&registration("x.com", 1)).await,
        Err(IntegrationError::Unsupported("register"))
    ));
}

#[tokio::test]
async fn rdap_server_override_skips_bootstrap() {
    let server = MockServer::start().await;
    Mock::given(path("/tr-rdap/domain/ornek.com.tr"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let client = RdapClient::with_options_and_policy(
        Some(Url::parse("http://127.0.0.1:9/never").unwrap()),
        Duration::from_secs(2),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
    .server_override(
        "tr",
        Url::parse(&format!("{}/tr-rdap", server.uri())).unwrap(),
    );
    assert_eq!(
        client.lookup("ornek.com.tr").await.unwrap(),
        RdapResult::Available
    );
}

/// RDAP servers come from a bootstrap file (tenant-configurable) or
/// overrides: every one of them is checked against the outbound policy.
#[tokio::test]
async fn rdap_servers_are_checked_against_the_outbound_policy() {
    let client =
        RdapClient::with_options_and_policy(None, Duration::from_secs(2), OutboundPolicy::Private)
            .unwrap()
            .server_override("tr", Url::parse("http://169.254.169.254/latest/").unwrap());
    match client.lookup("ornek.com.tr").await {
        Err(IntegrationError::Config(m)) => assert!(m.contains("HOS_OUTBOUND_POLICY"), "{m}"),
        other => panic!("expected Config, got {other:?}"),
    }
    // A loopback bootstrap URL is refused when the registrar is built.
    assert!(matches!(
        build_with_policy(
            "rdap",
            &json!({"bootstrap_url": "http://127.0.0.1:8080/dns.json"}),
            &json!({}),
            OutboundPolicy::Private,
        ),
        Err(IntegrationError::Config(_))
    ));
}
