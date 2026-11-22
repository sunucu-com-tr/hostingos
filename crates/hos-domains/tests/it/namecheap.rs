//! Namecheap against a wiremock server (no real network).

use crate::support::{form, registration, value};
use hos_core::IntegrationError;
use hos_core::integrations::registrar::{
    DnsRecord, DnsRecordType, DomainOperationStatus, DomainRegistrar, DomainRenewal,
};
use hos_core::secret::SecretString;
use hos_domains::OutboundPolicy;
use hos_domains::namecheap::{Namecheap, NamecheapConfig};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "NC-SECRET-KEY-abcdef0123";

fn client(server: &MockServer) -> Namecheap {
    let endpoint = Url::parse(&format!("{}/xml.response", server.uri())).unwrap();
    let mut cfg = NamecheapConfig::new(endpoint, "apiuser", SecretString::from(KEY), "203.0.113.7");
    cfg.outbound = OutboundPolicy::Unrestricted;
    Namecheap::new(cfg).unwrap()
}

fn ok(command: &str, inner: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<ApiResponse Status="OK" xmlns="http://api.namecheap.com/xml.response">
  <Errors />
  <Warnings />
  <RequestedCommand>{command}</RequestedCommand>
  <CommandResponse Type="{command}">{inner}</CommandResponse>
  <Server>WEB1</Server><GMTTimeDifference>--5:00</GMTTimeDifference><ExecutionTime>0.1</ExecutionTime>
</ApiResponse>"#
    ))
}

fn err(number: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<ApiResponse Status="ERROR" xmlns="http://api.namecheap.com/xml.response">
  <Errors><Error Number="{number}">{message}</Error></Errors>
  <Warnings />
  <CommandResponse />
</ApiResponse>"#
    ))
}

/// Matches the form field `Command=<name>`.
fn cmd(name: &'static str) -> impl wiremock::Match + 'static {
    move |req: &wiremock::Request| form(req).iter().any(|(k, v)| k == "Command" && v == name)
}

#[tokio::test]
async fn availability() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/xml.response"))
        .and(cmd("namecheap.domains.check"))
        .respond_with(ok(
            "namecheap.domains.check",
            r#"<DomainCheckResult Domain="ornek.com" Available="true" ErrorNo="0" Description="" IsPremiumName="false" PremiumRegistrationPrice="0" PremiumRenewalPrice="0" PremiumRestorePrice="0" PremiumTransferPrice="0" IcannFee="0" EapFee="0.0" />
<DomainCheckResult Domain="google.com" Available="false" ErrorNo="0" Description="" IsPremiumName="false" PremiumRegistrationPrice="0" PremiumRenewalPrice="0" PremiumRestorePrice="0" PremiumTransferPrice="0" IcannFee="0" EapFee="0.0" />
<domaincheckresult domain="lux.io" available="True" errorno="0" description="" ispremiumname="true" premiumregistrationprice="2500.00" premiumrenewalprice="60.00" />"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let nc = client(&server);
    let res = nc
        .check_availability(&["ornek.com".into(), "google.com".into(), "lux.io".into()])
        .await
        .unwrap();
    assert!(res[0].available);
    assert!(!res[1].available);
    assert!(res[2].available && res[2].premium);
    assert_eq!(
        res[2].register_price.unwrap().to_decimal_string(),
        "2500.00"
    );
    let reqs = server.received_requests().await.unwrap();
    assert!(
        reqs[0].url.query().is_none(),
        "credentials must not be in the URL"
    );
    let f = form(&reqs[0]);
    assert_eq!(
        value(&f, "DomainList").as_deref(),
        Some("ornek.com,google.com,lux.io")
    );
    assert_eq!(value(&f, "ApiKey").as_deref(), Some(KEY));
    assert_eq!(value(&f, "ClientIp").as_deref(), Some("203.0.113.7"));
    assert_eq!(value(&f, "UserName").as_deref(), Some("apiuser"));
}

#[tokio::test]
async fn register_sends_all_contacts() {
    let server = MockServer::start().await;
    Mock::given(cmd("namecheap.domains.create"))
        .respond_with(ok(
            "namecheap.domains.create",
            r#"<DomainCreateResult Domain="ornek.com" Registered="true" ChargedAmount="20.3600" DomainID="9007" OrderID="196074" TransactionID="380716" WhoisguardEnable="true" NonRealTimeDomain="false" />"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let nc = client(&server);
    let op = nc.register(&registration("ornek.com", 2)).await.unwrap();
    assert_eq!(op.status, DomainOperationStatus::Completed);
    assert_eq!(op.registrar_ref.as_deref(), Some("9007"));
    assert_eq!(op.cost.unwrap().to_decimal_string(), "20.36");
    let reqs = server.received_requests().await.unwrap();
    let f = form(&reqs[0]);
    for prefix in ["Registrant", "Tech", "Admin", "AuxBilling"] {
        assert_eq!(
            value(&f, &format!("{prefix}FirstName")).as_deref(),
            Some("Ada")
        );
        assert_eq!(
            value(&f, &format!("{prefix}Phone")).as_deref(),
            Some("+90.5551234567")
        );
        assert_eq!(
            value(&f, &format!("{prefix}Country")).as_deref(),
            Some("TR")
        );
        assert_eq!(
            value(&f, &format!("{prefix}EmailAddress")).as_deref(),
            Some("ada@example.com")
        );
    }
    assert_eq!(value(&f, "Years").as_deref(), Some("2"));
    assert_eq!(
        value(&f, "Nameservers").as_deref(),
        Some("ns1.host.example,ns2.host.example")
    );
    assert_eq!(value(&f, "AddFreeWhoisguard").as_deref(), Some("yes"));
    assert_eq!(value(&f, "WGEnabled").as_deref(), Some("yes"));
}

#[tokio::test]
async fn us_registration_maps_nexus_attributes() {
    let server = MockServer::start().await;
    Mock::given(cmd("namecheap.domains.create"))
        .respond_with(ok(
            "namecheap.domains.create",
            r#"<DomainCreateResult Domain="ornek.us" Registered="true" ChargedAmount="8" DomainID="1" OrderID="2" TransactionID="3" NonRealTimeDomain="false" />"#,
        ))
        .mount(&server)
        .await;
    let nc = client(&server);
    let mut req = registration("ornek.us", 1);
    req.extra = serde_json::json!({"us_nexus_category": "C31", "us_purpose": "p1", "us_nexus_country": "tr"});
    nc.register(&req).await.unwrap();
    let f = form(&server.received_requests().await.unwrap()[0]);
    assert_eq!(value(&f, "RegistrantNexus").as_deref(), Some("C31"));
    assert_eq!(value(&f, "RegistrantPurpose").as_deref(), Some("P1"));
    assert_eq!(value(&f, "RegistrantNexusCountry").as_deref(), Some("TR"));
    // Privacy is prohibited for .us.
    assert_eq!(value(&f, "WGEnabled").as_deref(), Some("no"));
}

#[tokio::test]
async fn renew_parses_new_expiry() {
    let server = MockServer::start().await;
    Mock::given(cmd("namecheap.domains.renew"))
        .respond_with(ok(
            "namecheap.domains.renew",
            "<DomainRenewResult DomainName=\"ornek.com\" DomainID=\"151378\" Renew=\"true\" OrderID=\"1\" TransactionID=\"2\" ChargedAmount=\"10.8700\"><DomainDetails><ExpiredDate>4/30/2028 11:31:13\u{202f}AM</ExpiredDate><NumYears>0</NumYears></DomainDetails></DomainRenewResult>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let nc = client(&server);
    let op = nc
        .renew(&DomainRenewal {
            domain: "ornek.com".into(),
            years: 1,
            current_expiry: None,
            restore_if_needed: false,
        })
        .await
        .unwrap();
    assert_eq!(
        op.expires_at.unwrap().to_rfc3339(),
        "2028-04-30T00:00:00+00:00"
    );
    assert_eq!(op.cost.unwrap().to_decimal_string(), "10.87");
}

#[tokio::test]
async fn expired_domain_is_reactivated_when_allowed() {
    let server = MockServer::start().await;
    Mock::given(cmd("namecheap.domains.renew"))
        .respond_with(err(
            "2020166",
            "Domain has expired. Please reactivate your domain.",
        ))
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.reactivate"))
        .respond_with(ok(
            "namecheap.domains.reactivate",
            r#"<DomainReactivateResult Domain="ornek.com" IsSuccess="true" ChargedAmount="90.00" OrderID="5" TransactionID="6" />"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let nc = client(&server);
    let mut req = DomainRenewal {
        domain: "ornek.com".into(),
        years: 1,
        current_expiry: None,
        restore_if_needed: false,
    };
    assert!(matches!(
        nc.renew(&req).await,
        Err(IntegrationError::Conflict(_))
    ));
    req.restore_if_needed = true;
    let op = nc.renew(&req).await.unwrap();
    assert_eq!(op.cost.unwrap().to_decimal_string(), "90.00");
}

#[tokio::test]
async fn nameservers_lock_and_info() {
    let server = MockServer::start().await;
    Mock::given(cmd("namecheap.domains.dns.setCustom"))
        .respond_with(ok(
            "namecheap.domains.dns.setCustom",
            r#"<DomainDNSSetCustomResult Domain="ornek.com" Updated="true" />"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.setRegistrarLock"))
        .respond_with(ok(
            "namecheap.domains.setRegistrarLock",
            r#"<DomainSetRegistrarLockResult Domain="ornek.com" IsSuccess="true" />"#,
        ))
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.getInfo"))
        .respond_with(ok(
            "namecheap.domains.getInfo",
            r#"<DomainGetInfoResult Status="Ok" ID="57582" DomainName="ornek.com" OwnerName="apiuser" IsOwner="true" IsPremium="false">
  <DomainDetails><CreatedDate>02/15/2016</CreatedDate><ExpiredDate>02/15/2099</ExpiredDate><NumYears>0</NumYears></DomainDetails>
  <LockDetails />
  <Whoisguard Enabled="True"><ID>53536</ID><ExpiredDate>02/11/2099</ExpiredDate><EmailDetails WhoisGuardEmail="x@whoisguard.com" ForwardedTo="ada@example.com" /></Whoisguard>
  <DnsDetails ProviderType="CUSTOM" IsUsingOurDNS="false" HostCount="5" EmailType="FWD"><Nameserver>ns1.new.example</Nameserver><Nameserver>NS2.new.example</Nameserver></DnsDetails>
</DomainGetInfoResult>"#,
        ))
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.getRegistrarLock"))
        .respond_with(ok(
            "namecheap.domains.getRegistrarLock",
            r#"<DomainGetRegistrarLockResult Domain="ornek.com" RegistrarLockStatus="true" />"#,
        ))
        .mount(&server)
        .await;
    let nc = client(&server);
    nc.set_nameservers(
        "ornek.com",
        &["ns1.new.example".into(), "ns2.new.example".into()],
    )
    .await
    .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let f = form(&reqs[0]);
    assert_eq!(value(&f, "SLD").as_deref(), Some("ornek"));
    assert_eq!(value(&f, "TLD").as_deref(), Some("com"));
    assert_eq!(
        value(&f, "Nameservers").as_deref(),
        Some("ns1.new.example,ns2.new.example")
    );
    nc.set_lock("ornek.com", false).await.unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        value(&form(&reqs[1]), "LockAction").as_deref(),
        Some("UNLOCK")
    );

    let info = nc.info("ornek.com").await.unwrap();
    assert_eq!(info.locked, Some(true));
    assert_eq!(info.privacy, Some(true));
    assert_eq!(info.nameservers, vec!["ns1.new.example", "ns2.new.example"]);
    assert_eq!(
        info.expires_at.unwrap().to_rfc3339(),
        "2099-02-15T00:00:00+00:00"
    );
    assert_eq!(info.registrar_ref.as_deref(), Some("57582"));

    assert_eq!(
        nc.get_auth_code("ornek.com").await,
        Err(IntegrationError::Unsupported("get_auth_code"))
    );
}

#[tokio::test]
async fn dns_upsert_is_read_modify_write() {
    let server = MockServer::start().await;
    let before = r#"<DomainDNSGetHostsResult Domain="ornek.com" EmailType="MX" IsUsingOurDNS="true">
  <host HostId="1" Name="@" Type="A" Address="192.0.2.1" MXPref="10" TTL="1800" />
  <host HostId="2" Name="@" Type="MX" Address="mx.ornek.com." MXPref="10" TTL="1800" />
</DomainDNSGetHostsResult>"#;
    let after = r#"<DomainDNSGetHostsResult Domain="ornek.com" EmailType="MX" IsUsingOurDNS="true">
  <host HostId="11" Name="@" Type="A" Address="192.0.2.1" MXPref="10" TTL="1800" />
  <host HostId="12" Name="@" Type="MX" Address="mx.ornek.com." MXPref="10" TTL="1800" />
  <host HostId="13" Name="www" Type="CNAME" Address="ornek.com." MXPref="10" TTL="300" />
</DomainDNSGetHostsResult>"#;
    Mock::given(cmd("namecheap.domains.dns.getHosts"))
        .respond_with(ok("namecheap.domains.dns.getHosts", before))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.dns.getHosts"))
        .respond_with(ok("namecheap.domains.dns.getHosts", after))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.dns.setHosts"))
        .respond_with(ok(
            "namecheap.domains.dns.setHosts",
            r#"<DomainDNSSetHostsResult Domain="ornek.com" IsSuccess="true" />"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let nc = client(&server);
    let stored = nc
        .upsert_dns_record(
            "ornek.com",
            &DnsRecord {
                id: None,
                name: "www".into(),
                record_type: DnsRecordType::Cname,
                value: "ornek.com.".into(),
                ttl: Some(300),
                priority: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(stored.id.as_deref(), Some("13"));
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 3);
    let set = form(&reqs[1]);
    // All existing records are written back together with the new one.
    assert_eq!(value(&set, "HostName1").as_deref(), Some("@"));
    assert_eq!(value(&set, "RecordType2").as_deref(), Some("MX"));
    assert_eq!(value(&set, "MXPref2").as_deref(), Some("10"));
    assert_eq!(value(&set, "HostName3").as_deref(), Some("www"));
    assert_eq!(value(&set, "TTL3").as_deref(), Some("300"));
    assert_eq!(value(&set, "EmailType").as_deref(), Some("MX"));
    assert_eq!(value(&set, "HostName4"), None);
}

#[tokio::test]
async fn errors_map_and_do_not_leak() {
    let server = MockServer::start().await;
    Mock::given(cmd("namecheap.users.getBalances"))
        .respond_with(err("1011150", "Invalid request IP: 203.0.113.7"))
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.getInfo"))
        .respond_with(err("2019166", &format!("Domain not found (ApiKey={KEY})")))
        .mount(&server)
        .await;
    Mock::given(cmd("namecheap.domains.renew"))
        .respond_with(ResponseTemplate::new(405).set_body_string("Too many requests"))
        .mount(&server)
        .await;
    let nc = client(&server);
    let auth = nc.test_connection().await.unwrap_err();
    assert_eq!(auth.kind(), "auth");
    assert!(auth.to_string().contains("whitelisted"));
    let nf = nc.info("ornek.com").await.unwrap_err();
    assert_eq!(nf.kind(), "not_found");
    assert!(!format!("{nf} {nf:?}").contains(KEY), "{nf}");
    let rl = nc
        .renew(&DomainRenewal {
            domain: "ornek.com".into(),
            years: 1,
            ..DomainRenewal::default()
        })
        .await
        .unwrap_err();
    assert_eq!(rl.kind(), "rate_limited");
}
