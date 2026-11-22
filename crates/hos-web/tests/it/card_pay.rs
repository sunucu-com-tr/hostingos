//! Pay page installment picker (NestPay) and "add a card without paying"
//! (mock gateway) through the client area, with the negative cases:
//! missing CSRF, sub-user without the invoices area, a tampered
//! installment count and a gateway of no use for the client.

use crate::biz_support::{csrf_of, login, owner, post, product, sub_user, unpaid_order};
use crate::common::TestApp;
use hos_billing::registry::SaveIntegration;
use hos_core::domain::IntegrationCategory;
use hos_core::ids::IntegrationId;
use hos_payments::mock;
use http::StatusCode;
use serde_json::json;

async fn gateway(
    app: &TestApp,
    kind: &str,
    config: serde_json::Value,
    secrets: serde_json::Value,
) -> IntegrationId {
    app.state
        .billing
        .registry()
        .save_integration(
            app.tenant_id,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Gateway,
                kind: kind.to_owned(),
                name: format!("{kind} kasa"),
                config,
                secrets: Some(secrets),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("gateway")
        .id
}

async fn methods(app: &TestApp) -> i64 {
    sqlx::query_scalar::<_, i64>("select count(*) from payment_methods where tenant_id = $1")
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn installments_and_card_setup_in_the_client_area() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let nestpay = gateway(
        &app,
        "nestpay",
        json!({"bank": "ziraat", "mode": "test", "client_id": "100200127", "max_installment": 6}),
        json!({"store_key": "TRPS0200"}),
    )
    .await;
    let mockgw = gateway(
        &app,
        mock::KIND,
        json!({}),
        json!({"webhook_secret": "whsec_card_pay_0123456789"}),
    )
    .await;
    let product = product(&app).await;
    let (_u, client, email) = owner(&app, "taksit").await;
    let invoice = unpaid_order(&app, client, product).await;

    // ---- pay page shows the picker for NestPay only
    login(&mut app, &email).await;
    let page = app.get(&format!("/client/invoices/{invoice}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains(&format!("name=\"installments_{nestpay}\""))
    );
    assert!(
        !page
            .body
            .contains(&format!("name=\"installments_{mockgw}\""))
    );
    let csrf = page.csrf();
    let pay = format!("/client/invoices/{invoice}/pay");
    let field = format!("installments_{nestpay}");
    let r = app
        .post(&pay, &[("method", &nestpay.to_string()), (&field, "3")])
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    // Tampered counts are refused (13 > max, "x" not a number).
    for bad in ["13", "x"] {
        let r = post(
            &mut app,
            &pay,
            &csrf,
            &[("method", &nestpay.to_string()), (&field, bad)],
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{bad}");
        assert!(
            r.location()
                .ends_with(&format!("/client/invoices/{invoice}")),
            "{bad}"
        );
    }
    let r = post(
        &mut app,
        &pay,
        &csrf,
        &[("method", &nestpay.to_string()), (&field, "3")],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "gate form page");
    assert!(
        r.body.contains("name=\"taksit\" value=\"3\""),
        "taksit sent"
    );

    // ---- add a card without paying (mock gateway)
    let billing = app.get("/client/billing").await;
    assert!(billing.body.contains("/client/billing/methods/add"));
    let csrf = billing.csrf();
    let r = app
        .post(
            "/client/billing/methods/add",
            &[("gateway", &mockgw.to_string())],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    // NestPay cannot save cards; an unknown id neither.
    for g in [
        nestpay.to_string(),
        IntegrationId::new().to_string(),
        "nope".into(),
    ] {
        let r = post(
            &mut app,
            "/client/billing/methods/add",
            &csrf,
            &[("gateway", &g)],
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{g}");
        assert!(r.location().ends_with("/client/billing"));
    }
    let r = post(
        &mut app,
        "/client/billing/methods/add",
        &csrf,
        &[("gateway", &mockgw.to_string())],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("gateway-form"));
    assert_eq!(
        methods(&app).await,
        0,
        "stored only after the signed completion"
    );

    // ---- a sub-user without the invoices area cannot start one
    let sub = sub_user(&app, client, &["tickets"]).await;
    login(&mut app, &sub).await;
    let csrf = csrf_of(&mut app, "/client/tickets").await;
    let r = post(
        &mut app,
        "/client/billing/methods/add",
        &csrf,
        &[("gateway", &mockgw.to_string())],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    app.cleanup().await;
}
