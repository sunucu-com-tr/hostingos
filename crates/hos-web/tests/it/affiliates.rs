//! Affiliate programme end to end: staff settings (permission, CSRF),
//! client activation (owner only), the referral link and cookie, sign-up
//! attribution, the self-referral flag, accrual on a paid invoice (once),
//! clearance, a credit payout (once, other tenant refused) and the automatic
//! reversal after a refund.

use crate::biz_support::{
    PASSWORD, csrf_of, login, owner, paid_order, post, product, staff, sub_user,
};
use crate::common::TestApp;
use hos_billing::payments::RefundInput;
use hos_core::ids::ClientId;
use hos_core::money::Currency;
use hos_db::repo::affiliates::{self as arepo, AffiliateRow};
use hos_db::repo::{clients, identity, invoices, payments as prepo};
use http::StatusCode;
use rust_decimal::Decimal;

async fn register(app: &mut TestApp, email: &str) -> ClientId {
    let token = app.get("/auth/register").await.csrf();
    let r = app
        .post(
            "/auth/register",
            &[
                ("csrf_token", &token),
                ("first_name", "Yönlendirilen"),
                ("last_name", "Müşteri"),
                ("email", email),
                ("password", PASSWORD),
                ("terms", "1"),
                ("website", ""),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "register: {}", r.body);
    let u = identity::user_by_email(app.pool(), app.tenant_id, email)
        .await
        .unwrap()
        .expect("user");
    clients::clients_of_user(app.pool(), app.tenant_id, u.id)
        .await
        .unwrap()
        .first()
        .expect("client")
        .id
}

async fn affiliate_of(app: &TestApp, client: ClientId) -> Option<AffiliateRow> {
    arepo::affiliate_for_client(app.pool(), app.tenant_id, client)
        .await
        .unwrap()
}

#[tokio::test]
async fn affiliate_programme_end_to_end() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("auth.require_email_verification", serde_json::json!(false))
        .await;
    let product = product(&app).await;

    // ---- settings: support may not see the programme; CSRF required
    staff(&mut app, "support").await;
    assert_eq!(
        app.get("/admin/affiliates").await.status,
        StatusCode::FORBIDDEN
    );
    let csrf = staff(&mut app, "owner").await;
    let settings = [
        ("enabled", "1"),
        ("self_signup", "1"),
        ("cookie_days", "30"),
        ("attribution", "first"),
        ("commission_type", "percent"),
        ("commission_value", "10"),
        ("clearance_days", "0"),
        ("min_payout", "1"),
        ("payout_credit", "1"),
        ("payout_manual", "1"),
    ];
    let r = app.post("/admin/affiliates/settings", &settings).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    let bad = post(
        &mut app,
        "/admin/affiliates/settings",
        &csrf,
        &[
            ("commission_type", "percent"),
            ("commission_value", "150"),
            ("cookie_days", "30"),
            ("clearance_days", "0"),
            ("min_payout", "1"),
        ],
    )
    .await;
    assert_eq!(
        bad.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "150 % refused"
    );
    let r = post(&mut app, "/admin/affiliates/settings", &csrf, &settings).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(app.get("/admin/affiliates").await.status, StatusCode::OK);

    // ---- client A activates; a sub-user without the owner area cannot
    let (_a_user, a_client, a_email) = owner(&app, "ortak").await;
    let sub = sub_user(&app, a_client, &["services", "invoices"]).await;
    login(&mut app, &sub).await;
    assert_eq!(
        app.get("/client/affiliate").await.status,
        StatusCode::FORBIDDEN
    );
    login(&mut app, &a_email).await;
    let page = app.get("/client/affiliate").await;
    assert_eq!(page.status, StatusCode::OK);
    let token = page.csrf();
    let r = app.post("/client/affiliate/activate", &[]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    assert!(affiliate_of(&app, a_client).await.is_none());
    let r = post(&mut app, "/client/affiliate/activate", &token, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let aff = affiliate_of(&app, a_client).await.expect("affiliate");
    let page = app.get("/client/affiliate").await;
    assert!(
        page.body.contains(&format!("/r/{}", aff.code)),
        "referral link shown"
    );

    // ---- referral link: cookie + click; open redirects refused
    app.cookies.clear();
    let r = app.get(&format!("/r/{}?to=/cart", aff.code)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), "/cart");
    assert!(app.cookies.contains_key("hos_ref"), "referral cookie set");
    let r = app
        .get(&format!("/r/{}?to=https://evil.example/", aff.code))
        .await;
    assert_eq!(r.location(), "/", "no open redirect");
    let stats = arepo::affiliate_stats(app.pool(), app.tenant_id, aff.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.clicks, 1, "one click per visitor and day");

    // ---- sign-up through the link is attributed; a same-domain sign-up is flagged
    let b_client = register(
        &mut app,
        &format!("b-{}@referred.example", uuid::Uuid::now_v7().simple()),
    )
    .await;
    let b_ref = arepo::referral_for_client(app.pool(), app.tenant_id, b_client)
        .await
        .unwrap()
        .expect("referral");
    assert_eq!(b_ref.affiliate_id, aff.id);
    assert!(b_ref.flag.is_none(), "{:?}", b_ref.flag);
    app.cookies.clear();
    app.get(&format!("/r/{}", aff.code)).await;
    let domain = a_email.split_once('@').unwrap().1.to_owned();
    let c_client = register(
        &mut app,
        &format!("c-{}@{domain}", uuid::Uuid::now_v7().simple()),
    )
    .await;
    let c_ref = arepo::referral_for_client(app.pool(), app.tenant_id, c_client)
        .await
        .unwrap()
        .expect("referral");
    assert_eq!(c_ref.flag.as_deref(), Some("same_email_domain"));
    // Without the cookie nothing is attributed; the affiliate cannot refer itself.
    app.cookies.clear();
    let d_client = register(
        &mut app,
        &format!("d-{}@other.example", uuid::Uuid::now_v7().simple()),
    )
    .await;
    assert!(
        arepo::referral_for_client(app.pool(), app.tenant_id, d_client)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        app.state
            .billing
            .attribute_referral(app.tenant_id, a_client, &aff.code, None)
            .await
            .is_ok()
    );
    assert!(
        arepo::referral_for_client(app.pool(), app.tenant_id, a_client)
            .await
            .unwrap()
            .is_none(),
        "no self-referral"
    );

    // ---- a paid invoice accrues 10 % once; the flagged referral is held
    let (b_invoice, b_txn) = paid_order(&app, b_client, product).await;
    let rows = arepo::commissions_for_invoice(app.pool(), app.tenant_id, b_invoice)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].amount, Decimal::new(499, 2));
    assert_eq!(rows[0].status.as_str(), "pending");
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, b_invoice)
        .await
        .unwrap()
        .unwrap();
    let mut conn = app.pool().acquire().await.unwrap();
    let again = app
        .state
        .billing
        .accrue_commissions(&mut conn, app.tenant_id, &inv)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(again, 0, "accrual is idempotent");
    let (c_invoice, _) = paid_order(&app, c_client, product).await;
    let held = arepo::commissions_for_invoice(app.pool(), app.tenant_id, c_invoice)
        .await
        .unwrap();
    assert_eq!(held[0].status.as_str(), "held");
    let (_, cleared) = app
        .state
        .billing
        .run_affiliate_maintenance(app.tenant_id)
        .await
        .unwrap();
    assert_eq!(cleared, 1, "only the unflagged commission clears");

    // ---- payout to credit: once, amount from the server
    login(&mut app, &a_email).await;
    let token = csrf_of(&mut app, "/client/affiliate").await;
    let r = post(
        &mut app,
        "/client/affiliate/payout",
        &token,
        &[("method", "credit"), ("amount", "1000")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post(
        &mut app,
        "/client/affiliate/payout",
        &token,
        &[("method", "credit")],
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "replay answers with an error flash"
    );
    let payouts = arepo::payouts_of(app.pool(), app.tenant_id, aff.id, 10)
        .await
        .unwrap();
    assert_eq!(payouts.len(), 1, "one open request");
    assert_eq!(
        payouts[0].amount,
        Decimal::new(499, 2),
        "tampered amount ignored"
    );

    // Another tenant's staff cannot touch it.
    let Some(mut other) = TestApp::new().await else {
        return;
    };
    let other_csrf = staff(&mut other, "owner").await;
    let r = post(
        &mut other,
        &format!("/admin/affiliates/payouts/{}/approve", payouts[0].id),
        &other_csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    other.cleanup().await;

    let csrf = staff(&mut app, "owner").await;
    let path = format!("/admin/affiliates/payouts/{}/approve", payouts[0].id);
    let r = post(&mut app, &path, &csrf, &[("note", "ok")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post(&mut app, &path, &csrf, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let credit = prepo::credit_balance(app.pool(), app.tenant_id, a_client, Currency::TRY)
        .await
        .unwrap();
    assert_eq!(credit, Decimal::new(499, 2), "credited exactly once");
    assert_eq!(
        app.get("/admin/affiliates/payouts").await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.get(&format!("/admin/affiliates/{}", aff.id))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.get("/admin/affiliates/review").await.status,
        StatusCode::OK
    );

    // ---- a refund takes the paid-out commission back
    app.state
        .billing
        .refund_payment(
            app.tenant_id,
            &RefundInput {
                transaction_id: b_txn.id,
                amount: b_txn.amount,
                reason: Some("test".into()),
                to_credit: true,
                staff_user_id: None,
                ip: None,
            },
        )
        .await
        .expect("refund");
    let (reversed, _) = app
        .state
        .billing
        .run_affiliate_maintenance(app.tenant_id)
        .await
        .unwrap();
    assert_eq!(reversed, 1);
    let all = arepo::commissions_of(app.pool(), app.tenant_id, aff.id, 50)
        .await
        .unwrap();
    assert!(
        all.iter()
            .any(|c| c.kind == "adjustment" && c.amount == Decimal::new(-499, 2)),
        "negative adjustment"
    );
    let (again, _) = app
        .state
        .billing
        .run_affiliate_maintenance(app.tenant_id)
        .await
        .unwrap();
    assert_eq!(again, 0, "reversal is idempotent");

    // ---- staff rejects the held (flagged) commission
    let r = post(
        &mut app,
        &format!("/admin/affiliates/referrals/{}/release", c_ref.id),
        &csrf,
        &[("decision", "reject")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let held = arepo::commissions_for_invoice(app.pool(), app.tenant_id, c_invoice)
        .await
        .unwrap();
    assert_eq!(held[0].status.as_str(), "reversed");
    app.cleanup().await;
}
