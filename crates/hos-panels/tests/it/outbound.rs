//! Outbound policy, redirects, body cap and error-message hygiene of the
//! shared panel HTTP layer (exercised through the cPanel adapter).

use crate::common::panel;
use hos_core::IntegrationError;
use hos_core::integrations::kinds;
use hos_panels::OutboundPolicy;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn panel_host_is_checked_against_the_outbound_policy() {
    let secrets = json!({"api_token": "TOKEN123"});
    for host in [
        "127.0.0.1",
        "2130706433",
        "0x7f.1",
        "::1",
        "::ffff:127.0.0.1",
        "169.254.169.254",
        "http://[fe80::1]:2087",
        "localhost",
    ] {
        for policy in [OutboundPolicy::Private, OutboundPolicy::Public] {
            let built = hos_panels::build_with_policy(
                kinds::CPANEL,
                &json!({"host": host}),
                &secrets,
                policy,
            );
            match built {
                Err(IntegrationError::Config(m)) => {
                    assert!(m.contains("HOS_OUTBOUND_POLICY"), "{host}: {m}")
                }
                Err(other) => panic!("{host}: expected Config, got {other:?}"),
                Ok(_) => panic!("{host}: built under {policy}"),
            }
        }
    }
    // Private ranges: allowed for a self-hosted company, not under `public`.
    let lan = json!({"host": "10.0.0.20"});
    assert!(
        hos_panels::build_with_policy(kinds::CPANEL, &lan, &secrets, OutboundPolicy::Private)
            .is_ok()
    );
    assert!(
        hos_panels::build_with_policy(kinds::CPANEL, &lan, &secrets, OutboundPolicy::Public)
            .is_err()
    );
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/json-api/version"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/landing", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/landing"))
        .respond_with(ResponseTemplate::new(200).set_body_string("internal page"))
        .expect(0)
        .mount(&server)
        .await;
    let p = panel(
        kinds::CPANEL,
        &server,
        json!({}),
        json!({"api_token": "TOKEN123"}),
    );
    let err = p.test_connection().await.unwrap_err();
    assert!(!err.to_string().contains("internal page"), "{err}");
}

#[tokio::test]
async fn error_messages_do_not_quote_response_bodies() {
    let server = MockServer::start().await;
    let internal = "root:x:0:0:root:/root:/bin/bash SECRET-INTERNAL-DATA";
    Mock::given(method("POST"))
        .and(path("/json-api/version"))
        .respond_with(ResponseTemplate::new(500).set_body_string(internal))
        .mount(&server)
        .await;
    let p = panel(
        kinds::CPANEL,
        &server,
        json!({}),
        json!({"api_token": "TOKEN123"}),
    );
    let err = p.test_connection().await.unwrap_err();
    let text = err.to_string();
    assert!(!text.contains("SECRET-INTERNAL-DATA"), "{text}");
    assert!(text.contains("HTTP 500"), "{text}");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/json-api/version"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<html><head><title>Intranet: payroll</title></head></html>"),
        )
        .mount(&server)
        .await;
    let p = panel(
        kinds::CPANEL,
        &server,
        json!({}),
        json!({"api_token": "TOKEN123"}),
    );
    let text = p.test_connection().await.unwrap_err().to_string();
    assert!(!text.contains("payroll"), "{text}");
}

#[tokio::test]
async fn oversized_responses_are_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/json-api/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(17 * 1024 * 1024)))
        .mount(&server)
        .await;
    let p = panel(
        kinds::CPANEL,
        &server,
        json!({}),
        json!({"api_token": "TOKEN123"}),
    );
    match p.test_connection().await {
        Err(IntegrationError::Other(m)) => assert!(m.contains("exceeds"), "{m}"),
        other => panic!("expected the body cap, got {other:?}"),
    }
}
