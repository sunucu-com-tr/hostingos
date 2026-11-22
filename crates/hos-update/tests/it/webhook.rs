//! Deploy webhooks (container installs).

use hos_update::UpdateError;
use hos_update::fetch::{Fetcher, NetConfig};
use hos_update::webhook::{DeployWebhook, WebhookMethod, WebhookPayload, call, validate_url};
use url::Url;
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fetcher() -> Fetcher {
    Fetcher::new(NetConfig::insecure_for_tests(Vec::<String>::new())).unwrap()
}

#[tokio::test]
async fn get_webhook_with_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/deploy"))
        .and(query_param("uuid", "app-1"))
        .and(header("authorization", "Bearer s3cret"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let hook = DeployWebhook {
        url: Url::parse(&format!("{}/api/v1/deploy?uuid=app-1", server.uri())).unwrap(),
        method: WebhookMethod::Get,
        secret: Some("s3cret".into()),
    };
    assert!(
        !format!("{hook:?}").contains("s3cret"),
        "secret not in Debug"
    );
    let status = call(
        &fetcher(),
        &hook,
        &WebhookPayload::new("0.2.0", None, "stable"),
    )
    .await
    .expect("webhook");
    assert_eq!(status, 200);
}

#[tokio::test]
async fn post_webhook_is_signed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("x-hostingos-event", "hostingos.update"))
        .and(header_exists("x-hostingos-signature"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let hook = DeployWebhook {
        url: Url::parse(&format!("{}/hook", server.uri())).unwrap(),
        method: WebhookMethod::Post,
        secret: Some("k".into()),
    };
    let payload = WebhookPayload::new("0.2.0", Some("ghcr.io/example/hostingos:0.2.0"), "stable");
    assert_eq!(call(&fetcher(), &hook, &payload).await.unwrap(), 204);
    let received = server.received_requests().await.unwrap();
    let req = &received[0];
    let body = req.body.clone();
    let sig = req
        .headers
        .get("x-hostingos-signature")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        sig,
        format!("sha256={}", hos_core::crypto::hmac_sha256_hex(b"k", &body))
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["version"], "0.2.0");
    assert_eq!(json["image"], "ghcr.io/example/hostingos:0.2.0");
}

#[tokio::test]
async fn failures_and_validation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("location", "https://elsewhere.example/"),
        )
        .mount(&server)
        .await;
    let hook = DeployWebhook {
        url: Url::parse(&format!("{}/x", server.uri())).unwrap(),
        method: WebhookMethod::Get,
        secret: None,
    };
    let err = call(
        &fetcher(),
        &hook,
        &WebhookPayload::new("0.2.0", None, "stable"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, UpdateError::Refused(_)),
        "redirects are not followed: {err}"
    );

    let prod = Fetcher::new(NetConfig::new(Vec::<String>::new())).unwrap();
    assert!(
        validate_url(&prod, "http://deploy.example/hook").is_err(),
        "HTTPS only"
    );
    assert!(validate_url(&prod, "https://u:p@deploy.example/hook").is_err());
    assert!(
        validate_url(&prod, "https://169.254.169.254/latest").is_err(),
        "metadata address"
    );
    assert!(validate_url(&prod, "https://deploy.example/hook").is_ok());
    assert_eq!(WebhookMethod::parse("post"), Some(WebhookMethod::Post));
}
