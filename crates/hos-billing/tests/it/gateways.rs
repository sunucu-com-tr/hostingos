//! End-to-end payment flows of real gateway implementations through the
//! billing engine, against the database and (where the gateway calls an API)
//! a local wiremock server:
//!
//! * NestPay / Payten 3D Pay Hosting: form → signed bank response on the
//!   server callback (`Approved` reply) and the browser return (replay),
//!   tampering, the exact-attempt check and a second integration;
//! * Paynet (Paynet.js embed): signed form action → server-side charge →
//!   invoice paid, replay, refund request completed by the signed
//!   `succeedUrl` notification.
//!
//! The mock-server tests need the `Unrestricted` outbound policy (loopback);
//! `common::pool` selects it for the whole test binary before anything reads
//! the policy.

use crate::common::{Env, d, suffix};
use hos_billing::payments::{EventOutcome, RefundInput, StartPayment, StartedPayment};
use hos_billing::registry::SaveIntegration;
use hos_core::CoreError;
use hos_core::domain::{IntegrationCategory, InvoiceStatus, ProductType};
use hos_core::ids::{ClientId, IntegrationId, InvoiceId};
use hos_core::integrations::gateway::{
    CheckoutSession, RefundStatus, WebhookReply, WebhookRequest,
};
use hos_core::money::BillingCycle;
use hos_core::net::OutboundPolicy;
use hos_db::repo::carts::NewCartItem;
use hos_db::repo::{invoices, payments};
use http::HeaderMap;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const STORE_KEY: &str = "NESTPAYTESTKEY1";
const PAYNET_SECRET: &str = "sck_test_billing";

fn unrestricted() -> bool {
    match OutboundPolicy::set_global(OutboundPolicy::Unrestricted) {
        Ok(()) => true,
        Err(active) => active == OutboundPolicy::Unrestricted,
    }
}

async fn gateway(env: &Env, kind: &str, config: Value, secrets: Value) -> IntegrationId {
    env.billing
        .registry()
        .save_integration(
            env.tenant,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Gateway,
                kind: kind.to_owned(),
                name: format!("{kind} {}", suffix()),
                config,
                secrets: Some(secrets),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("save gateway")
        .id
}

/// An unpaid 120 TRY invoice (100 + KDV %20) of a new client.
async fn open_invoice(env: &Env, gw: IntegrationId) -> (ClientId, InvoiceId) {
    let product = env
        .product(
            "Consulting",
            ProductType::Other,
            None,
            json!({}),
            "100",
            "0",
        )
        .await;
    let client = env.client("Payer").await;
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(product, BillingCycle::Monthly)],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    assert_eq!(out.quote.total, d("120"));
    (client, out.invoice_id)
}

async fn start(env: &Env, invoice: InvoiceId, gw: IntegrationId) -> StartedPayment {
    env.billing
        .start_payment(
            env.tenant,
            invoice,
            &StartPayment {
                integration_id: Some(gw),
                ip: Some("203.0.113.9".parse().expect("ip")),
                ..StartPayment::default()
            },
        )
        .await
        .expect("start payment")
}

async fn status(env: &Env, invoice: InvoiceId) -> InvoiceStatus {
    invoices::get_invoice(&env.pool, env.tenant, invoice)
        .await
        .expect("invoice")
        .expect("exists")
        .status
}

fn form_request(fields: &[(String, String)], query: Option<&str>) -> WebhookRequest {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    WebhookRequest {
        headers: HeaderMap::new(),
        body: body.into_bytes(),
        query: query.map(str::to_owned),
        remote_ip: None,
    }
}

fn outcome(result: &hos_billing::WebhookResult) -> &EventOutcome {
    &result.events.first().expect("one event").outcome
}

/// What the Payten gate posts back: every request field plus the result,
/// signed (hash ver3) with `key`.
fn bank_response(
    form: &[(String, String)],
    key: &str,
    amount: Option<&str>,
) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = form.to_vec();
    if let Some(a) = amount {
        for (k, v) in &mut fields {
            if k == "amount" {
                *v = a.to_owned();
            }
        }
    }
    let oid = fields
        .iter()
        .find(|(k, _)| k == "oid")
        .map(|(_, v)| v.clone())
        .expect("oid");
    for (k, v) in [
        ("Response", "Approved"),
        ("ProcReturnCode", "00"),
        ("mdStatus", "1"),
        ("TransId", "26322LmHA12345"),
        ("AuthCode", "123456"),
        ("MaskedPan", "454360***4360"),
    ] {
        fields.push((k.to_owned(), v.to_owned()));
    }
    fields.push(("ReturnOid".to_owned(), oid));
    let hash = hos_payments::nestpay::ver3_hash(&fields, key);
    fields.push(("HASH".to_owned(), hash));
    fields
}

#[tokio::test]
async fn nestpay_3d_pay_hosting_end_to_end() {
    let Some(env) = Env::new().await else { return };
    env.staff().await;
    let nestpay_config = json!({"bank": "ziraat", "mode": "test", "client_id": "100200127"});
    let gw = gateway(
        &env,
        "nestpay",
        nestpay_config.clone(),
        json!({"store_key": STORE_KEY}),
    )
    .await;
    let (_, invoice) = open_invoice(&env, gw).await;
    let started = start(&env, invoice, gw).await;
    let CheckoutSession::Form { action, fields, .. } = &started.session else {
        panic!(
            "NestPay must hand the customer a form: {:?}",
            started.session
        );
    };
    assert!(action.ends_with("/fim/est3Dgate"));
    let get = |k: &str| {
        fields
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .expect(k)
    };
    assert_eq!(get("amount"), "120.00");
    assert_eq!(get("currency"), "949");
    assert_eq!(get("oid"), started.reference);
    assert!(get("callbackUrl").contains(&format!("/webhooks/gateway/{gw}?hos_src=callback")));

    // 1. A response altered after signing is refused before anything is applied.
    let mut forged = bank_response(fields, STORE_KEY, None);
    for (k, v) in &mut forged {
        if k == "amount" {
            *v = "1.00".into();
        }
    }
    let err = env
        .billing
        .handle_webhook(
            env.tenant,
            gw,
            &form_request(&forged, Some("hos_src=callback")),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Unauthorized), "{err:?}");
    assert_eq!(status(&env, invoice).await, InvoiceStatus::Unpaid);

    // 2. Server callback: paid, answered with the literal `Approved`.
    let response = bank_response(fields, STORE_KEY, None);
    let res = env
        .billing
        .handle_webhook(
            env.tenant,
            gw,
            &form_request(&response, Some("hos_src=callback")),
        )
        .await
        .expect("callback");
    assert_eq!(res.reply, WebhookReply::Text("Approved".into()));
    assert!(
        matches!(
            outcome(&res),
            EventOutcome::PaymentRecorded {
                invoice_paid: true,
                ..
            }
        ),
        "{res:?}"
    );
    assert_eq!(status(&env, invoice).await, InvoiceStatus::Paid);

    // 3. Browser return with the same signed result: a replay, and a 303 back.
    let ok_url = url::Url::parse(&get("okUrl")).expect("okUrl");
    let res = env
        .billing
        .handle_webhook(env.tenant, gw, &form_request(&response, ok_url.query()))
        .await
        .expect("browser return");
    assert!(matches!(outcome(&res), EventOutcome::Duplicate), "{res:?}");
    let WebhookReply::Redirect(to) = &res.reply else {
        panic!("redirect expected: {:?}", res.reply)
    };
    assert!(to.contains(&format!("/client/invoices/{invoice}")));
    let payments_recorded = env
        .count("select count(*) from transactions where tenant_id = $1 and kind = 'payment' and status = 'succeeded'")
        .await;
    assert_eq!(payments_recorded, 1);
    // Card data never reaches the stored payloads.
    let leaked = env
        .count("select count(*) from webhooks_in where tenant_id = $1 and payload::text like '%454360%'")
        .await;
    assert_eq!(leaked, 0);

    // 4. A correctly signed result whose amount differs from the attempt is
    //    not applied as a partial payment: rejected + staff notified.
    let (_, invoice2) = open_invoice(&env, gw).await;
    let started2 = start(&env, invoice2, gw).await;
    let CheckoutSession::Form {
        fields: fields2, ..
    } = &started2.session
    else {
        panic!("form expected")
    };
    let cheap = bank_response(fields2, STORE_KEY, Some("12.00"));
    let res = env
        .billing
        .handle_webhook(
            env.tenant,
            gw,
            &form_request(&cheap, Some("hos_src=callback")),
        )
        .await
        .expect("mismatch callback");
    assert!(
        matches!(outcome(&res), EventOutcome::Rejected { .. }),
        "{res:?}"
    );
    assert_eq!(res.reply, WebhookReply::Text("Approved".into()));
    assert_eq!(status(&env, invoice2).await, InvoiceStatus::Unpaid);
    assert_eq!(env.notifications("payment.attempt_mismatch").await, 1);

    // 5. Integration B: another store key refuses A's result; the same key
    //    (duplicated settings) still cannot pay A's attempt through B.
    let other_key = gateway(
        &env,
        "nestpay",
        nestpay_config.clone(),
        json!({"store_key": "ANOTHERSTOREKEY"}),
    )
    .await;
    let response2 = bank_response(fields2, STORE_KEY, None);
    let err = env
        .billing
        .handle_webhook(
            env.tenant,
            other_key,
            &form_request(&response2, Some("hos_src=callback")),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Unauthorized));
    let same_key = gateway(
        &env,
        "nestpay",
        nestpay_config,
        json!({"store_key": STORE_KEY}),
    )
    .await;
    let res = env
        .billing
        .handle_webhook(
            env.tenant,
            same_key,
            &form_request(&response2, Some("hos_src=callback")),
        )
        .await
        .expect("cross-integration callback");
    assert!(
        matches!(outcome(&res), EventOutcome::Rejected { .. }),
        "{res:?}"
    );
    assert_eq!(status(&env, invoice2).await, InvoiceStatus::Unpaid);
    env.cleanup().await;
}

#[tokio::test]
async fn paynet_embed_charge_refund_end_to_end() {
    if !unrestricted() {
        eprintln!("outbound policy already fixed — skipping wiremock gateway test");
        return;
    }
    let Some(env) = Env::new().await else { return };
    let server = MockServer::start().await;
    let gw = gateway(
        &env,
        "paynet",
        json!({"mode": "test", "publishable_key": "pbk_test_1", "api_base_override": server.uri()}),
        json!({"secret_key": PAYNET_SECRET}),
    )
    .await;
    let (_, invoice) = open_invoice(&env, gw).await;
    let started = start(&env, invoice, gw).await;
    let CheckoutSession::Embed { html, .. } = &started.session else {
        panic!("Paynet must embed Paynet.js: {:?}", started.session);
    };
    assert!(html.contains("data-amount=\"12000\""));
    assert!(html.contains("pts-pj.paynet.com.tr"));
    let start_at = html.find("action=\"").expect("action") + 8;
    let action =
        html[start_at..start_at + html[start_at..].find('"').expect("quote")].replace("&amp;", "&");
    let action = url::Url::parse(&action).expect("action url");
    assert_eq!(action.path(), format!("/webhooks/gateway/{gw}"));

    Mock::given(method("POST"))
        .and(path("/v1/transaction/charge"))
        .and(body_partial_json(json!({
            "session_id": "js_session_1", "token_id": "tok_1", "amount": 12000,
            "reference_no": started.reference
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 2526145, "xact_id": "xk_EAAAAFZ59jeB", "is_succeed": true,
            "amount": 120.0, "net_amount": 116.4, "comission": 3.6, "currency": "TRY",
            "card_no_masked": "435508******4358", "agent_reference_no": started.reference,
            "code": 0, "message": "Başarılı İşlem"
        })))
        .expect(2)
        .mount(&server)
        .await;
    let post = WebhookRequest {
        headers: HeaderMap::new(),
        body: b"session_id=js_session_1&token_id=tok_1".to_vec(),
        query: action.query().map(str::to_owned),
        remote_ip: None,
    };
    let res = env
        .billing
        .handle_webhook(env.tenant, gw, &post)
        .await
        .expect("paynet post");
    assert!(
        matches!(
            outcome(&res),
            EventOutcome::PaymentRecorded {
                invoice_paid: true,
                ..
            }
        ),
        "{res:?}"
    );
    assert!(matches!(&res.reply, WebhookReply::Redirect(u) if u.contains("payment=return")));
    assert_eq!(status(&env, invoice).await, InvoiceStatus::Paid);
    // Browser refresh: charged again at Paynet's side → same event, no double payment.
    let res = env
        .billing
        .handle_webhook(env.tenant, gw, &post)
        .await
        .expect("replay");
    assert!(matches!(outcome(&res), EventOutcome::Duplicate), "{res:?}");
    // A tampered amount in the action never reaches Paynet.
    let mut tampered = post.clone();
    tampered.query = post
        .query
        .as_ref()
        .map(|q| q.replace("hos_amt=12000", "hos_amt=100"));
    assert!(matches!(
        env.billing.handle_webhook(env.tenant, gw, &tampered).await,
        Err(CoreError::Unauthorized)
    ));

    // Refund: Paynet accepts the request (pending), then confirms on succeedUrl.
    Mock::given(method("POST"))
        .and(path("/v1/transaction/reversed_request"))
        .and(body_partial_json(
            json!({"xact_id": "xk_EAAAAFZ59jeB", "amount": "5000"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object_name": "transaction_reversed_request", "message": "Başarılı İşlem", "code": 0
        })))
        .expect(1)
        .mount(&server)
        .await;
    let txn =
        payments::transaction_by_gateway_id(&env.pool, env.tenant, Some(gw), "xk_EAAAAFZ59jeB")
            .await
            .expect("txn")
            .expect("payment row");
    assert_eq!(txn.fee, d("3.6"));
    let refund = env
        .billing
        .refund_payment(
            env.tenant,
            &RefundInput {
                transaction_id: txn.id,
                amount: d("50"),
                reason: Some("test".into()),
                to_credit: false,
                staff_user_id: None,
                ip: None,
            },
        )
        .await
        .expect("refund");
    assert_eq!(refund.status, RefundStatus::Pending);
    let sent = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .find(|r| r.url.path() == "/v1/transaction/reversed_request")
        .expect("refund request");
    let sent: Value = serde_json::from_slice(&sent.body).expect("json");
    let succeed = url::Url::parse(sent["succeedUrl"].as_str().expect("succeedUrl")).expect("url");
    assert_eq!(succeed.path(), format!("/webhooks/gateway/{gw}"));
    let notify = WebhookRequest {
        headers: HeaderMap::new(),
        body: json!({"xact_id": "xk_REFUND1", "code": "0", "status": "3", "message": "ok"})
            .to_string()
            .into_bytes(),
        query: succeed.query().map(str::to_owned),
        remote_ip: None,
    };
    let res = env
        .billing
        .handle_webhook(env.tenant, gw, &notify)
        .await
        .expect("refund notification");
    assert!(
        matches!(outcome(&res), EventOutcome::Refunded { refund_id } if *refund_id == refund.id),
        "{res:?}"
    );
    let done = payments::get_refund(&env.pool, env.tenant, refund.id)
        .await
        .expect("refund row")
        .expect("exists");
    assert_eq!(done.status, RefundStatus::Succeeded);
    // The same notification again changes nothing.
    let res = env
        .billing
        .handle_webhook(env.tenant, gw, &notify)
        .await
        .expect("refund replay");
    assert!(matches!(outcome(&res), EventOutcome::Duplicate), "{res:?}");
    env.cleanup().await;
}
