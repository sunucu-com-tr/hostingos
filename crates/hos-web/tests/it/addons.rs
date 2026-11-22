//! Addon for an existing service: offered on the service page, ordered by
//! the owner (order → invoice with a prorated first charge → activation on
//! payment), with the negative cases: sub-user without the orders area,
//! missing CSRF, another client's service, an addon not offered for the
//! product, a forged id and a duplicate order.

use crate::biz_support::{active_service, csrf_of, login, owner, post, sub_user};
use crate::common::TestApp;
use hos_billing::payments::ManualPayment;
use hos_core::domain::{InvoiceStatus, ServiceStatus};
use hos_core::ids::{InvoiceId, ProductAddonId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::{InvoiceItemType, PayType};
use hos_db::repo::{catalog, invoices, services};
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;

async fn addon(app: &TestApp, name: &str, pay: PayType) -> ProductAddonId {
    let slug = format!("ek-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
    catalog::create_addon(
        app.pool(),
        app.tenant_id,
        &slug,
        name,
        None,
        &json!({}),
        pay,
    )
    .await
    .expect("addon")
    .id
}

#[tokio::test]
async fn addon_for_existing_service_with_negative_cases() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_a_user, a_client, a_email) = owner(&app, "ekhizmet").await;
    let (_b_user, b_client, b_email) = owner(&app, "baskasi").await;
    let svc = active_service(&app, a_client).await;
    let product_id = services::get_service(app.pool(), app.tenant_id, svc)
        .await
        .unwrap()
        .unwrap()
        .product_id;
    let backup = addon(&app, "Günlük Yedek", PayType::Recurring).await;
    catalog::attach_addon(app.pool(), app.tenant_id, backup, product_id)
        .await
        .unwrap();
    catalog::set_addon_price(
        app.pool(),
        app.tenant_id,
        backup,
        Currency::TRY,
        BillingCycle::Monthly,
        Decimal::new(30, 0),
        Decimal::ZERO,
    )
    .await
    .unwrap();
    // Exists but is not offered for this product.
    let stray = addon(&app, "Başka Ürün Eki", PayType::Recurring).await;
    let _b_svc = active_service(&app, b_client).await;

    // ---- sub-user without the orders area: no offer, POST refused
    let sub = sub_user(&app, a_client, &["services"]).await;
    login(&mut app, &sub).await;
    let page = app.get(&format!("/client/services/{svc}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("addon-offers"));
    let csrf = page.csrf();
    let r = post(
        &mut app,
        &format!("/client/services/{svc}/addons"),
        &csrf,
        &[("addon", &backup.to_string())],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // ---- another client cannot order for A's service
    login(&mut app, &b_email).await;
    let csrf = csrf_of(&mut app, "/client/services").await;
    let r = post(
        &mut app,
        &format!("/client/services/{svc}/addons"),
        &csrf,
        &[("addon", &backup.to_string())],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // ---- the owner
    login(&mut app, &a_email).await;
    let page = app.get(&format!("/client/services/{svc}")).await;
    assert!(page.body.contains("addon-offers"), "offer shown");
    assert!(page.body.contains("Günlük Yedek"));
    assert!(!page.body.contains("Başka Ürün Eki"));
    let csrf = page.csrf();
    let path = format!("/client/services/{svc}/addons");
    let r = app.post(&path, &[("addon", &backup.to_string())]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    let r = post(&mut app, &path, &csrf, &[("addon", &stray.to_string())]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().ends_with(&format!("/client/services/{svc}")));
    let r = post(&mut app, &path, &csrf, &[("addon", "not-an-id")]).await;
    assert!(r.status.is_client_error(), "{}", r.status);
    assert_eq!(
        services::service_addons(app.pool(), app.tenant_id, svc)
            .await
            .unwrap()
            .len(),
        0
    );

    let r = post(
        &mut app,
        &path,
        &csrf,
        &[("addon", &backup.to_string()), ("price", "0")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let loc = r.location().to_owned();
    assert!(loc.contains("/client/invoices/"), "{loc}");
    let invoice_id: InvoiceId = loc
        .trim_end_matches("#pay")
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let items = invoices::invoice_items(app.pool(), app.tenant_id, invoice_id)
        .await
        .unwrap();
    let line = items
        .iter()
        .find(|i| i.item_type == InvoiceItemType::Addon)
        .expect("addon line");
    assert!(
        line.amount > Decimal::ZERO && line.amount < Decimal::new(30, 0),
        "prorated first charge, got {}",
        line.amount
    );
    let sa = services::service_addons(app.pool(), app.tenant_id, svc)
        .await
        .unwrap();
    assert_eq!(sa.len(), 1);
    assert_eq!(sa[0].status, ServiceStatus::Pending);
    assert_eq!(sa[0].recurring_amount, Decimal::new(30, 0));

    // ---- a second order of the same addon is refused while unpaid
    let r = post(&mut app, &path, &csrf, &[("addon", &backup.to_string())]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().ends_with(&format!("/client/services/{svc}")));
    assert_eq!(
        services::service_addons(app.pool(), app.tenant_id, svc)
            .await
            .unwrap()
            .len(),
        1
    );

    // ---- paying the invoice activates the addon
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Unpaid);
    app.state
        .billing
        .record_manual_payment(
            app.tenant_id,
            &ManualPayment {
                invoice_id,
                amount: inv.balance,
                integration_id: None,
                method: Some("bank_transfer".into()),
                reference: Some(format!("ADDON-{}", uuid::Uuid::now_v7().simple())),
                occurred_at: None,
                note: None,
                staff_user_id: None,
            },
        )
        .await
        .expect("payment");
    let sa = services::service_addons(app.pool(), app.tenant_id, svc)
        .await
        .unwrap();
    assert_eq!(sa[0].status, ServiceStatus::Active, "activated on payment");
    let page = app.get(&format!("/client/services/{svc}")).await;
    assert!(!page.body.contains("addon-offers"), "nothing left to offer");
    app.cleanup().await;
}

/// A service paid several periods ahead pays the addon for each of them up
/// to its due date (the first charge used to cover them all for the price
/// of one period); a service due within the period pays the running part.
#[tokio::test]
async fn addon_on_a_service_paid_ahead_is_charged_per_period() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (_user, client, _email) = owner(&app, "pesin").await;
    let svc = active_service(&app, client).await;
    let product_id = services::get_service(app.pool(), app.tenant_id, svc)
        .await
        .unwrap()
        .unwrap()
        .product_id;
    let backup = addon(&app, "Peşin Yedek", PayType::Recurring).await;
    catalog::attach_addon(app.pool(), app.tenant_id, backup, product_id)
        .await
        .unwrap();
    catalog::set_addon_price(
        app.pool(),
        app.tenant_id,
        backup,
        Currency::TRY,
        BillingCycle::Monthly,
        Decimal::new(30, 0),
        Decimal::ZERO,
    )
    .await
    .unwrap();
    let today = app.state.billing.clock().today();
    let charge = |months: u32| {
        let pool = app.pool().clone();
        let (tenant, billing) = (app.tenant_id, app.state.billing.clone());
        async move {
            let end = today
                .checked_add_months(chrono::Months::new(months))
                .unwrap();
            sqlx::query("update services set next_due_date = $3 where tenant_id = $1 and id = $2")
                .bind(tenant)
                .bind(svc)
                .bind(end)
                .execute(&pool)
                .await
                .unwrap();
            let order = billing
                .order_service_addon(tenant, svc, Some(client), backup, None, None)
                .await
                .expect("addon order");
            let line = invoices::invoice_items(&pool, tenant, order.invoice_id)
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.item_type == InvoiceItemType::Addon)
                .expect("addon line");
            // Free the addon again for the next case.
            sqlx::query(
                "update invoices set status = 'cancelled' where tenant_id = $1 and id = $2",
            )
            .bind(tenant)
            .bind(order.invoice_id)
            .execute(&pool)
            .await
            .unwrap();
            (line.amount, line.period_end)
        }
    };
    let (amount, end) = charge(3).await;
    assert_eq!(amount, Decimal::new(90, 0), "three whole months");
    assert_eq!(
        end,
        today
            .checked_add_months(chrono::Months::new(3))
            .and_then(|d| d.pred_opt())
    );
    let (amount, _) = charge(1).await;
    assert_eq!(amount, Decimal::new(30, 0), "exactly one period");
    app.cleanup().await;
}
