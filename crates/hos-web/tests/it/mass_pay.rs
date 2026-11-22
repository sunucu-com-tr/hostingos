//! Client mass payment: the invoice list offers "pay selected"; the POST is
//! CSRF-protected, limited to the invoices area, refuses other clients'
//! and tampered ids, and leads to the combined invoice.

use crate::biz_support::{csrf_of, login, owner, post, product, sub_user, unpaid_order};
use crate::common::TestApp;
use hos_core::ids::InvoiceId;
use hos_db::repo::{invoices, mass_pay};
use http::StatusCode;
use rust_decimal::Decimal;

async fn mass_count(app: &TestApp) -> i64 {
    sqlx::query_scalar::<_, i64>("select count(*) from mass_payments where tenant_id = $1")
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn client_mass_pay_with_negative_cases() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let product = product(&app).await;
    let (_a_user, a_client, a_email) = owner(&app, "toplu").await;
    let (_b_user, b_client, _b_email) = owner(&app, "diger").await;
    let a1 = unpaid_order(&app, a_client, product).await;
    let a2 = unpaid_order(&app, a_client, product).await;
    let b1 = unpaid_order(&app, b_client, product).await;

    // ---- a sub-user without the invoices area gets 403
    let sub = sub_user(&app, a_client, &["tickets"]).await;
    login(&mut app, &sub).await;
    let csrf = csrf_of(&mut app, "/client/tickets").await;
    let r = post(
        &mut app,
        "/client/invoices/mass-pay",
        &csrf,
        &[("invoice", &a1.to_string()), ("invoice", &a2.to_string())],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(mass_count(&app).await, 0);

    // ---- the owner sees the selection
    login(&mut app, &a_email).await;
    let list = app.get("/client/invoices").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.contains("id=\"mass-pay\""));
    assert!(list.body.contains(&format!("value=\"{a1}\"")));
    let csrf = list.csrf();

    // ---- missing CSRF → 403
    let r = app
        .post(
            "/client/invoices/mass-pay",
            &[("invoice", &a1.to_string()), ("invoice", &a2.to_string())],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // ---- another client's invoice → refused, nothing created
    let r = post(
        &mut app,
        "/client/invoices/mass-pay",
        &csrf,
        &[("invoice", &a1.to_string()), ("invoice", &b1.to_string())],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(
        r.location().ends_with("/client/invoices"),
        "{}",
        r.location()
    );
    // ---- a tampered id leaves one invoice → refused
    let r = post(
        &mut app,
        "/client/invoices/mass-pay",
        &csrf,
        &[("invoice", &a1.to_string()), ("invoice", "1 or 1=1")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().ends_with("/client/invoices"));
    assert_eq!(mass_count(&app).await, 0);

    // ---- two own invoices → combined invoice with server-side amounts
    let r = post(
        &mut app,
        "/client/invoices/mass-pay",
        &csrf,
        &[
            ("invoice", &a1.to_string()),
            ("invoice", &a2.to_string()),
            ("amount", "0.01"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let loc = r.location().to_owned();
    assert!(
        loc.contains("/client/invoices/") && loc.ends_with("#pay"),
        "{loc}"
    );
    let id: InvoiceId = loc
        .trim_end_matches("#pay")
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let combined = invoices::get_invoice(app.pool(), app.tenant_id, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(combined.client_id, a_client);
    assert_eq!(combined.total, Decimal::new(9980, 2), "2 × 49.90");
    let mp = mass_pay::for_invoice(app.pool(), app.tenant_id, id)
        .await
        .unwrap()
        .expect("mass payment");
    assert_eq!(mp.client_id, a_client);
    let page = app.get(&format!("/client/invoices/{id}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("mass-pay-list"));
    assert!(page.body.contains(&format!("/client/invoices/{a1}")));

    // ---- the other client cannot open it
    let (_c_user, _c_client, c_email) = owner(&app, "ucuncu").await;
    login(&mut app, &c_email).await;
    assert_eq!(
        app.get(&format!("/client/invoices/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    app.cleanup().await;
}
