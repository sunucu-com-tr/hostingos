//! Saving a card without paying (mock gateway, signed completion stored
//! once, other tenant / tampered payload refused) and the installment
//! choice on hosted payments (validated against the gateway, carried to
//! NestPay as `taksit`, never changing the charged amount).

use crate::common::{Env, d};
use hos_billing::StartPayment;
use hos_billing::payments::EventOutcome;
use hos_billing::registry::SaveIntegration;
use hos_core::CoreError;
use hos_core::domain::{IntegrationCategory, ProductType};
use hos_core::integrations::gateway::{CheckoutSession, WebhookRequest};
use hos_db::repo::carts::NewCartItem;
use serde_json::json;

fn form_request(fields: &[(String, String)]) -> WebhookRequest {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    WebhookRequest {
        body: body.into_bytes(),
        ..WebhookRequest::default()
    }
}

#[tokio::test]
async fn card_setup_stores_the_method_once() {
    let Some(env) = Env::new().await else { return };
    let Some(other) = Env::new().await else {
        return;
    };
    let gw = env.gateway().await;
    let client = env.client("Kart").await;
    let started = env
        .billing
        .start_card_setup(env.tenant, client, gw, None, None)
        .await
        .expect("setup");
    let CheckoutSession::Form { fields, .. } = started.session else {
        panic!("mock hands off with a form");
    };
    let count = || async {
        env.count("select count(*) from payment_methods where tenant_id = $1")
            .await
    };
    assert_eq!(
        count().await,
        0,
        "nothing stored before the gateway confirms"
    );

    // The same signed completion delivered to another tenant's gateway.
    let ogw = other.gateway().await;
    let res = other
        .billing
        .handle_webhook(other.tenant, ogw, &form_request(&fields))
        .await
        .expect("webhook");
    assert!(
        matches!(res.events[0].outcome, EventOutcome::Rejected { .. }),
        "{:?}",
        res.events[0].outcome
    );

    // Tampered payload → signature rejected.
    let mut tampered = fields.clone();
    for (k, v) in &mut tampered {
        if k == "payload" {
            *v = v.replace("mock_pm_", "mock_pm_evil");
        }
    }
    let err = env
        .billing
        .handle_webhook(env.tenant, gw, &form_request(&tampered))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Unauthorized), "{err:?}");
    assert_eq!(count().await, 0);

    // The real completion stores the card once; a replay changes nothing.
    let res = env
        .billing
        .handle_webhook(env.tenant, gw, &form_request(&fields))
        .await
        .expect("webhook");
    assert_eq!(
        res.events[0].outcome,
        EventOutcome::MethodSaved { client_id: client }
    );
    assert_eq!(count().await, 1);
    let again = env
        .billing
        .handle_webhook(env.tenant, gw, &form_request(&fields))
        .await
        .expect("webhook");
    assert_eq!(again.events[0].outcome, EventOutcome::Duplicate);
    assert_eq!(count().await, 1);
    assert_eq!(
        env.count(
            "select count(*) from payment_setups where tenant_id = $1 and status = 'succeeded'"
        )
        .await,
        1
    );
    other.cleanup().await;
    env.cleanup().await;
}

/// A stored card token belongs to its client: the same token saved for
/// another client is neither moved nor does it change either default.
#[tokio::test]
async fn a_stored_token_is_never_moved_to_another_client() {
    use hos_db::repo::billing_support::{
        NewPaymentMethod, list_payment_methods, save_payment_method,
    };
    let Some(env) = Env::new().await else { return };
    let gw = env.gateway().await;
    let (a, b) = (env.client("Kart A").await, env.client("Kart B").await);
    let card = |client, hash: &str| NewPaymentMethod {
        id: uuid::Uuid::now_v7(),
        client_id: client,
        integration_id: gw,
        token_enc: vec![1, 2, 3],
        token_hash: hash.into(),
        label: None,
    };
    let mut conn = env.pool.acquire().await.unwrap();
    let a_card = save_payment_method(&mut conn, env.tenant, &card(a, "tok-a"))
        .await
        .unwrap()
        .expect("stored");
    let b_card = save_payment_method(&mut conn, env.tenant, &card(b, "tok-b"))
        .await
        .unwrap()
        .expect("stored");
    let taken = save_payment_method(&mut conn, env.tenant, &card(b, "tok-a"))
        .await
        .unwrap();
    assert!(taken.is_none(), "another client's token is not moved");
    let of = |c| list_payment_methods(&env.pool, env.tenant, c);
    let a_list = of(a).await.unwrap();
    assert_eq!(a_list.len(), 1);
    assert_eq!((a_list[0].id, a_list[0].is_default), (a_card.id, true));
    let b_list = of(b).await.unwrap();
    assert_eq!(b_list.len(), 1);
    assert_eq!(
        (b_list[0].id, b_list[0].is_default),
        (b_card.id, true),
        "b keeps its default"
    );
    // The same client saving its own token again: re-marked, not doubled.
    let again = save_payment_method(&mut conn, env.tenant, &card(a, "tok-a"))
        .await
        .unwrap()
        .expect("own token");
    assert_eq!(again.id, a_card.id);
    assert_eq!(of(a).await.unwrap().len(), 1);
    drop(conn);
    env.cleanup().await;
}

#[tokio::test]
async fn installments_are_validated_and_reach_nestpay() {
    let Some(env) = Env::new().await else { return };
    let mock = env.gateway().await;
    let nestpay = env
        .billing
        .registry()
        .save_integration(
            env.tenant,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Gateway,
                kind: "nestpay".into(),
                name: "Banka POS".into(),
                config: json!({"bank": "ziraat", "mode": "test", "client_id": "100200127",
                               "max_installment": 6}),
                secrets: Some(json!({"store_key": "TRPS0200"})),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("nestpay")
        .id;
    let product = env
        .product("Taksitli", ProductType::Other, None, json!({}), "1000", "0")
        .await;
    let client = env.client("Taksit").await;
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(
                product,
                hos_core::money::BillingCycle::Monthly,
            )],
        )
        .await;
    let invoice = env.checkout(cart, client, Some(nestpay)).await.invoice_id;
    let start = |gateway, installments| StartPayment {
        integration_id: Some(gateway),
        installments,
        ..StartPayment::default()
    };
    // The mock offers no installments; NestPay at most 6.
    for (gateway, n) in [(mock, 3), (nestpay, 7), (nestpay, 12)] {
        let err = env
            .billing
            .start_payment(env.tenant, invoice, &start(gateway, Some(n)))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)), "{n}: {err:?}");
    }
    let started = env
        .billing
        .start_payment(env.tenant, invoice, &start(nestpay, Some(3)))
        .await
        .expect("3 installments");
    assert_eq!(started.amount.amount, d("1200"), "amount is the balance");
    let CheckoutSession::Form { fields, .. } = started.session else {
        panic!("NestPay posts a form");
    };
    let field = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(field("taksit").as_deref(), Some("3"));
    assert_eq!(field("amount").as_deref(), Some("1200.00"));
    // Single payment: no taksit.
    let single = env
        .billing
        .start_payment(env.tenant, invoice, &start(nestpay, None))
        .await
        .expect("single");
    let CheckoutSession::Form { fields, .. } = single.session else {
        panic!("form");
    };
    assert!(
        fields
            .iter()
            .find(|(n, _)| n == "taksit")
            .is_none_or(|(_, v)| v.is_empty())
    );
    env.cleanup().await;
}
