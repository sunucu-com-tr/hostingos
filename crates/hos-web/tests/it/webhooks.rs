//! `/webhooks/*`: signed mock-gateway webhooks pay invoices exactly once, bad
//! signatures change nothing, every request is recorded, registrar
//! notifications enqueue a status sync.

use crate::common::TestApp;
use axum::body::Body;
use chrono::Utc;
use hos_billing::{SaveIntegration, StartPayment};
use hos_core::domain::{IntegrationCategory, InvoiceStatus};
use hos_core::ids::IntegrationId;
use hos_core::integrations::gateway::{PaymentEventKind, WebhookRequest};
use hos_core::money::Currency;
use hos_db::repo::clients::{self, ClientInput};
use hos_db::repo::invoices::{self, InvoiceRow};
use hos_db::repo::payments;
use hos_payments::mock::{self, MockEvent};
use http::{Request, StatusCode};
use rust_decimal::Decimal;
use serde_json::json;
use tower::ServiceExt;

const SECRET: &str = "whsec_web_test_0123456789abcdef";

async fn integration(
    app: &TestApp,
    category: IntegrationCategory,
    kind: &str,
    secrets: serde_json::Value,
) -> IntegrationId {
    app.state
        .billing
        .registry()
        .save_integration(
            app.tenant_id,
            &SaveIntegration {
                id: None,
                category,
                kind: kind.to_owned(),
                name: format!("{category} test"),
                config: json!({}),
                secrets: Some(secrets),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("save integration")
        .id
}

/// Gateway + an unpaid 100 TRY invoice + a started payment (reference).
async fn unpaid_invoice(app: &TestApp) -> (IntegrationId, InvoiceRow, String) {
    let gw = integration(
        app,
        IntegrationCategory::Gateway,
        mock::KIND,
        json!({ "webhook_secret": SECRET }),
    )
    .await;
    let client = clients::create_client(
        app.pool(),
        app.tenant_id,
        &ClientInput::individual(
            "Web",
            "Hook",
            &format!("hook-{}@example.com", app.slug),
            Currency::TRY,
        ),
    )
    .await
    .expect("client");
    let inv = app
        .state
        .billing
        .order_add_funds(app.tenant_id, client.id, Decimal::from(100))
        .await
        .expect("invoice");
    let started = app
        .state
        .billing
        .start_payment(
            app.tenant_id,
            inv.id,
            &StartPayment {
                integration_id: Some(gw),
                ..StartPayment::default()
            },
        )
        .await
        .expect("start payment");
    (gw, inv, started.reference)
}

fn paid_event(inv: &InvoiceRow, reference: &str) -> MockEvent {
    MockEvent::new(
        PaymentEventKind::Paid,
        format!("mock_{reference}"),
        inv.balance_money(),
    )
    .with_reference(reference)
}

async fn deliver(app: &TestApp, path: &str, req: &WebhookRequest) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "webhooks.invalid");
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    let res = app
        .router
        .clone()
        .oneshot(builder.body(Body::from(req.body.clone())).expect("request"))
        .await
        .expect("infallible");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn invoice(app: &TestApp, inv: &InvoiceRow) -> InvoiceRow {
    invoices::get_invoice(app.pool(), app.tenant_id, inv.id)
        .await
        .expect("query")
        .expect("invoice")
}

async fn recorded(app: &TestApp, integration: IntegrationId) -> Vec<(String, Option<bool>)> {
    sqlx::query_as::<_, (String, Option<bool>)>(
        "select status::text, signature_valid from webhooks_in
          where tenant_id = $1 and integration_id = $2 order by created_at",
    )
    .bind(app.tenant_id)
    .bind(integration)
    .fetch_all(app.pool())
    .await
    .expect("webhooks_in")
}

#[tokio::test]
async fn signed_webhook_pays_invoice_exactly_once() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, inv, reference) = unpaid_invoice(&app).await;
    assert_eq!(inv.status, InvoiceStatus::Unpaid);
    let req = mock::webhook_request(
        SECRET.as_bytes(),
        &paid_event(&inv, &reference),
        Utc::now().timestamp(),
    );
    let path = format!("/webhooks/gateway/{gw}");

    let (status, _) = deliver(&app, &path, &req).await;
    assert_eq!(status, StatusCode::OK);
    let after = invoice(&app, &inv).await;
    assert_eq!(after.status, InvoiceStatus::Paid);
    assert_eq!(after.balance, Decimal::ZERO);

    // The provider retries the same delivery: still one payment.
    let (status, _) = deliver(&app, &path, &req).await;
    assert_eq!(status, StatusCode::OK);
    let txns = payments::invoice_transactions(app.pool(), app.tenant_id, inv.id)
        .await
        .expect("transactions");
    assert_eq!(txns.len(), 1, "duplicate delivery must not double-pay");
    assert_eq!(invoice(&app, &inv).await.amount_paid, inv.total);
    assert!(
        !recorded(&app, gw).await.is_empty(),
        "deliveries are recorded"
    );
    app.cleanup().await;
}

#[tokio::test]
async fn bad_signature_is_rejected_and_changes_nothing() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, inv, reference) = unpaid_invoice(&app).await;
    let forged = mock::webhook_request(
        b"not-the-secret",
        &paid_event(&inv, &reference),
        Utc::now().timestamp(),
    );
    let (status, body) = deliver(&app, &format!("/webhooks/gateway/{gw}"), &forged).await;
    assert!(status.is_client_error(), "got {status}: {body}");
    assert!(body.contains("\"status\""), "problem+json body: {body}");
    let after = invoice(&app, &inv).await;
    assert_eq!(after.status, InvoiceStatus::Unpaid);
    assert!(
        payments::invoice_transactions(app.pool(), app.tenant_id, inv.id)
            .await
            .expect("transactions")
            .is_empty()
    );
    let rows = recorded(&app, gw).await;
    assert!(
        rows.iter().any(|(_, valid)| *valid == Some(false)),
        "the rejected request is recorded: {rows:?}"
    );
    app.cleanup().await;
}

#[tokio::test]
async fn browser_form_post_back_is_accepted() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, inv, reference) = unpaid_invoice(&app).await;
    let fields = mock::form_fields(
        SECRET.as_bytes(),
        &paid_event(&inv, &reference),
        Utc::now().timestamp(),
    );
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish();
    let res = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/webhooks/gateway/{gw}"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert!(
        matches!(res.status(), StatusCode::OK | StatusCode::SEE_OTHER),
        "got {}",
        res.status()
    );
    assert_eq!(invoice(&app, &inv).await.status, InvoiceStatus::Paid);
    app.cleanup().await;
}

#[tokio::test]
async fn unknown_or_wrong_integrations_are_404() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let req = WebhookRequest {
        headers: http::HeaderMap::new(),
        body: b"{}".to_vec(),
        query: None,
        remote_ip: None,
    };
    let (status, _) = deliver(
        &app,
        &format!("/webhooks/gateway/{}", IntegrationId::new()),
        &req,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = deliver(&app, "/webhooks/gateway/not-a-uuid", &req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A registrar id on the gateway endpoint is not a gateway.
    let reg = integration(&app, IntegrationCategory::Registrar, "mock", json!({})).await;
    let (status, _) = deliver(&app, &format!("/webhooks/gateway/{reg}"), &req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    app.cleanup().await;
}

#[tokio::test]
async fn registrar_notification_enqueues_status_sync() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let reg = integration(&app, IntegrationCategory::Registrar, "mock", json!({})).await;
    let req = WebhookRequest {
        headers: http::HeaderMap::new(),
        body: br#"{"event":"domain.updated","domain":"example.com"}"#.to_vec(),
        query: Some("source=test".into()),
        remote_ip: None,
    };
    let (status, body) = deliver(
        &app,
        &format!("/webhooks/registrar/{reg}?source=test"),
        &req,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let jobs: Vec<(String,)> = sqlx::query_as(
        "select kind from jobs where tenant_id = $1 and kind = 'sync_domain_status'",
    )
    .bind(app.tenant_id)
    .fetch_all(app.pool())
    .await
    .expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(recorded(&app, reg).await.len(), 1);
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// Rate limits after authentication; unverified requests stored as hashes
// ---------------------------------------------------------------------------

/// [`deliver`] from client address `ip` (the connection's peer address).
async fn deliver_from(
    app: &TestApp,
    path: &str,
    req: &WebhookRequest,
    ip: &str,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "webhooks.invalid");
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    let mut request = builder.body(Body::from(req.body.clone())).expect("request");
    let addr: std::net::SocketAddr = format!("{ip}:40000").parse().expect("ip");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(addr));
    let res = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("infallible");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// A forged callback carrying a body an attacker controls (with a card
/// field and a marker that must never be stored).
fn forged(inv: &InvoiceRow, reference: &str) -> WebhookRequest {
    let mut req = mock::webhook_request(
        b"not-the-secret",
        &paid_event(inv, reference),
        Utc::now().timestamp(),
    );
    req.body = format!(
        "{{\"MaskedPan\":\"435508***4358\",\"cavv\":\"AAAB\",\"filler\":\"FORGED-MARKER-{}\"}}",
        "x".repeat(4000)
    )
    .into_bytes();
    req
}

/// Requests left in `limiter` for `key` (consumes one to find out).
fn remaining(limiter: &hos_auth::ratelimit::RateLimiter, key: &str) -> u32 {
    match limiter.check(key) {
        hos_auth::ratelimit::RateDecision::Allowed { remaining } => remaining,
        hos_auth::ratelimit::RateDecision::Limited { .. } => 0,
    }
}

/// Forged callbacks used to take from the integration's single bucket
/// before the signature was checked: enough of them (300/min) and genuine
/// gateway callbacks and browser returns got 429.
#[tokio::test]
async fn forged_callbacks_do_not_use_up_the_integration_limit() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, inv, reference) = unpaid_invoice(&app).await;
    let path = format!("/webhooks/gateway/{gw}");
    for i in 0..5 {
        let (status, _) = deliver_from(
            &app,
            &path,
            &forged(&inv, &reference),
            &format!("198.51.100.{i}"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let burst = app.state.limits.webhook.limit().burst;
    assert_eq!(
        remaining(&app.state.limits.webhook, &format!("webhook:{gw}")),
        burst - 1,
        "forged calls took from the verified bucket"
    );
    // The genuine callback goes through.
    let genuine = mock::webhook_request(
        SECRET.as_bytes(),
        &paid_event(&inv, &reference),
        Utc::now().timestamp(),
    );
    let (status, _) = deliver_from(&app, &path, &genuine, "192.0.2.10").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(invoice(&app, &inv).await.status, InvoiceStatus::Paid);
    app.cleanup().await;
}

/// An IP that keeps failing is turned away (429) without any work or row;
/// other senders are unaffected; failed requests keep no body.
#[tokio::test]
async fn failing_ip_is_limited_and_failed_requests_keep_only_a_hash() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, inv, reference) = unpaid_invoice(&app).await;
    let path = format!("/webhooks/gateway/{gw}");
    let budget = app.state.limits.webhook_failed.limit().burst;
    let mut statuses = Vec::new();
    for _ in 0..budget + 3 {
        statuses.push(
            deliver_from(&app, &path, &forged(&inv, &reference), "203.0.113.77")
                .await
                .0,
        );
    }
    assert!(
        statuses[..budget as usize]
            .iter()
            .all(|s| *s == StatusCode::UNAUTHORIZED)
    );
    assert!(
        statuses[budget as usize..]
            .iter()
            .all(|s| *s == StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );
    let rows: Vec<(serde_json::Value, bool)> = sqlx::query_as(
        "select payload, signature_valid from webhooks_in where tenant_id = $1 and integration_id = $2",
    )
    .bind(app.tenant_id)
    .bind(gw)
    .fetch_all(app.pool())
    .await
    .expect("rows");
    assert_eq!(rows.len(), budget as usize, "limited calls store nothing");
    for (payload, valid) in &rows {
        assert!(!valid);
        let text = payload.to_string();
        assert!(
            !text.contains("FORGED-MARKER") && !text.contains("435508") && !text.contains("AAAB"),
            "{text}"
        );
        assert!(
            payload["body_sha256"]
                .as_str()
                .is_some_and(|h| h.len() == 64),
            "{text}"
        );
        assert!(payload["body_bytes"].as_u64().is_some_and(|n| n > 4000));
    }
    // A genuine callback from another address still pays the invoice.
    let genuine = mock::webhook_request(
        SECRET.as_bytes(),
        &paid_event(&inv, &reference),
        Utc::now().timestamp(),
    );
    let (status, _) = deliver_from(&app, &path, &genuine, "192.0.2.11").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(invoice(&app, &inv).await.status, InvoiceStatus::Paid);
    // Unknown integrations count against the IP too.
    for _ in 0..budget {
        let (status, _) = deliver_from(
            &app,
            &format!("/webhooks/gateway/{}", IntegrationId::new()),
            &genuine,
            "203.0.113.78",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _) = deliver_from(&app, &path, &genuine, "203.0.113.78").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    app.cleanup().await;
}

/// The failed-request rows of an integration are capped (newest kept).
#[tokio::test]
async fn failed_request_rows_are_capped() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, _, _) = unpaid_invoice(&app).await;
    let mut conn = app.pool().acquire().await.expect("conn");
    for i in 0..5 {
        hos_db::repo::webhooks::record_unverified(
            &mut conn,
            app.tenant_id,
            gw,
            &format!("request.unauthorized.{i}"),
            hos_db::enums::WebhookInStatus::Failed,
            &json!({"body_bytes": i}),
            &json!({}),
            None,
            3,
        )
        .await
        .expect("record");
    }
    let kept: Vec<(String,)> = sqlx::query_as(
        "select event_type from webhooks_in where tenant_id = $1 and integration_id = $2 order by created_at",
    )
    .bind(app.tenant_id)
    .bind(gw)
    .fetch_all(app.pool())
    .await
    .expect("rows");
    let kept: Vec<&str> = kept.iter().map(|(e,)| e.as_str()).collect();
    assert_eq!(
        kept,
        [
            "request.unauthorized.2",
            "request.unauthorized.3",
            "request.unauthorized.4"
        ]
    );
    drop(conn);
    app.cleanup().await;
}

/// A gateway that verifies through its provider's API (PayPal webhooks;
/// the mock simulates it with a header) keeps a per-integration limit in
/// front of that call; locally verified requests do not touch it.
#[tokio::test]
async fn remotely_verified_webhooks_are_limited_before_verification() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, inv, reference) = unpaid_invoice(&app).await;
    let path = format!("/webhooks/gateway/{gw}");
    let key = format!("webhook-remote:{gw}");
    let burst = app.state.limits.webhook_remote.limit().burst;
    let (status, _) = deliver_from(&app, &path, &forged(&inv, &reference), "198.51.100.20").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Local verification: the remote bucket is untouched (one taken below).
    assert_eq!(remaining(&app.state.limits.webhook_remote, &key), burst - 1);
    let mut remote = forged(&inv, &reference);
    remote.headers.insert(
        mock::REMOTE_VERIFY_HEADER,
        http::HeaderValue::from_static("1"),
    );
    let (status, _) = deliver_from(&app, &path, &remote, "198.51.100.21").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(remaining(&app.state.limits.webhook_remote, &key), burst - 3);
    app.cleanup().await;
}

/// Verified callbacks without events keep their body for diagnosis, minus
/// card and 3-D Secure fields; the stored-payload scrub removes such fields
/// from rows kept before.
#[tokio::test]
async fn stored_callbacks_never_keep_card_fields() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, _, _) = unpaid_invoice(&app).await;
    // An old row with a NestPay-style form body and a JSON one.
    for (key, payload) in [
        (
            "old:1",
            json!({"body": "oid=INV1&MaskedPan=435508***4358&md=MDX&cavv=AAAB&eci=05", "query": null}),
        ),
        (
            "old:2",
            json!({"body": {"oid": "INV2", "xid": "XID", "nested": {"cvv": "123"}}, "query": null}),
        ),
        ("old:3", json!({"body": {"oid": "INV3"}, "query": null})),
    ] {
        hos_db::repo::webhooks::record_incoming(
            app.pool(),
            app.tenant_id,
            gw,
            key,
            Some("request.failed"),
            &payload,
            &json!({}),
            false,
        )
        .await
        .expect("seed row");
    }
    let changed = hos_web::routes::webhooks::scrub_stored_payloads(app.pool(), Some(app.tenant_id))
        .await
        .expect("scrub");
    assert!(changed >= 2, "{changed}");
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "select external_event_id, payload from webhooks_in where tenant_id = $1 order by external_event_id",
    )
    .bind(app.tenant_id)
    .fetch_all(app.pool())
    .await
    .expect("rows");
    for (key, payload) in &rows {
        let text = payload.to_string();
        for leak in ["435508", "MDX", "AAAB", "XID", "\"cvv\""] {
            assert!(!text.contains(leak), "{key}: {text}");
        }
    }
    assert_eq!(rows[1].1["body"]["oid"], "INV2", "other fields stay");
    assert_eq!(
        rows[2].1,
        json!({"body": {"oid": "INV3"}, "query": null}),
        "untouched"
    );
    // Running it again changes nothing.
    let again = hos_web::routes::webhooks::scrub_stored_payloads(app.pool(), Some(app.tenant_id))
        .await
        .expect("scrub");
    let _ = again;
    let still: serde_json::Value = sqlx::query_scalar(
        "select payload from webhooks_in where tenant_id = $1 and external_event_id = 'old:3'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .expect("row");
    assert_eq!(still, json!({"body": {"oid": "INV3"}, "query": null}));
    app.cleanup().await;
}

/// `return_url` / `cancel_url` of `POST /api/v1/invoices/{id}/pay` took any
/// http(s) URL; the gateway return signer signed it and the webhook
/// endpoint answered 303 to it (signed open redirect). Only the shop's own
/// hosts and its allow-list are accepted now.
#[tokio::test]
async fn api_payment_return_urls_stay_on_the_shops_hosts() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (gw, inv, _) = unpaid_invoice(&app).await;
    let staff = crate::biz_support::user(
        &app,
        &format!("api-{}@staff.test", app.slug),
        hos_core::domain::UserKind::Staff,
        Some("owner"),
    )
    .await;
    let key = crate::biz_support::api_key(&app, staff).await;
    let path = format!("/api/v1/invoices/{}/pay", inv.id);
    let pay = |field: &'static str, url: String| json!({"integration_id": gw, field: url});
    for (field, url) in [
        ("return_url", "https://evil.example/landing".to_owned()),
        ("cancel_url", "https://evil.example/".to_owned()),
        ("return_url", format!("https://{}.evil.example/", app.host)),
        ("return_url", format!("https://user@{}/client", app.host)),
        ("return_url", "javascript:alert(1)".to_owned()),
    ] {
        let (status, body) =
            crate::biz_support::api(&app, "POST", &path, &key, Some(pay(field, url.clone()))).await;
        assert!(status.is_client_error(), "{url}: {status} {body}");
        assert!(body.to_string().contains(field), "{url}: {body}");
    }
    let own = format!("https://{}/client/invoices/{}", app.host, inv.id);
    let (status, body) =
        crate::biz_support::api(&app, "POST", &path, &key, Some(pay("return_url", own))).await;
    assert!(status.is_success(), "own host: {status} {body}");
    // A host the tenant allows explicitly.
    app.setting("payments.return_url_hosts", json!(["shop.partner.example"]))
        .await;
    let (status, body) = crate::biz_support::api(
        &app,
        "POST",
        &path,
        &key,
        Some(pay(
            "cancel_url",
            "https://shop.partner.example/cart".to_owned(),
        )),
    )
    .await;
    assert!(status.is_success(), "allow-listed host: {status} {body}");
    app.cleanup().await;
}
