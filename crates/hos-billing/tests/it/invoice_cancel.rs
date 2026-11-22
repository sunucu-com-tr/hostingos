//! Cancelling an invoice never strands money: applied credit comes back,
//! money paid on it needs an explicit move to credit (or a refund), and a
//! payment in flight at a gateway blocks the cancellation.

use crate::common::{Env, d};
use hos_billing::payments::ManualPayment;
use hos_core::CoreError;
use hos_core::domain::{InvoiceStatus, OrderStatus, ProductType};
use hos_core::ids::{ClientId, IntegrationId, InvoiceId, ProductId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::CreditKind;
use hos_db::repo::carts::NewCartItem;
use hos_db::repo::invoices::{self, InvoiceRow};
use hos_db::repo::orders;
use hos_db::repo::payments::{self, CancelBlock, CancelOutcome};
use serde_json::json;

async fn invoice(env: &Env, id: InvoiceId) -> InvoiceRow {
    invoices::get_invoice(&env.pool, env.tenant, id)
        .await
        .unwrap()
        .unwrap()
}

async fn credit(env: &Env, client: ClientId) -> rust_decimal::Decimal {
    payments::credit_balance(&env.pool, env.tenant, client, Currency::TRY)
        .await
        .unwrap()
}

async fn setup(env: &Env) -> (IntegrationId, ProductId, ClientId) {
    let gw = env.gateway().await;
    let product = env
        .product("Support", ProductType::Other, None, json!({}), "100", "0")
        .await;
    let client = env.client("Cancel").await;
    (gw, product, client)
}

/// One checkout → `(order, unpaid invoice of 120 TRY)`.
async fn unpaid(
    env: &Env,
    product: ProductId,
    client: ClientId,
    gw: IntegrationId,
) -> (hos_core::ids::OrderId, InvoiceId) {
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(product, BillingCycle::Monthly)],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    (out.order_id, out.invoice_id)
}

async fn give_credit(env: &Env, client: ClientId, amount: &str) {
    let mut conn = env.pool.acquire().await.unwrap();
    payments::adjust_credit(
        &mut conn,
        env.tenant,
        client,
        d(amount),
        Currency::TRY,
        CreditKind::Manual,
        "test",
        None,
    )
    .await
    .unwrap();
}

async fn apply_credit(env: &Env, inv: InvoiceId, amount: &str) {
    let mut conn = env.pool.acquire().await.unwrap();
    payments::apply_credit_to_invoice(&mut conn, env.tenant, inv, Some(d(amount)), None)
        .await
        .unwrap();
}

async fn pay(env: &Env, inv: InvoiceId, amount: &str) {
    env.billing
        .record_manual_payment(
            env.tenant,
            &ManualPayment {
                invoice_id: inv,
                amount: d(amount),
                integration_id: None,
                method: Some("bank_transfer".into()),
                reference: None,
                occurred_at: None,
                note: None,
                staff_user_id: None,
            },
        )
        .await
        .unwrap();
}

async fn cancel(env: &Env, inv: InvoiceId, paid_to_credit: bool) -> CancelOutcome {
    let mut conn = env.pool.acquire().await.unwrap();
    payments::cancel_invoice_with_credit(&mut conn, env.tenant, inv, paid_to_credit, None)
        .await
        .unwrap()
}

#[tokio::test]
async fn cancel_returns_applied_credit_and_holds_paid_invoices() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;

    // Invoices first: checkout would use credit the client already has.
    let (_, a) = unpaid(&env, product, client, gw).await;
    let (_, b) = unpaid(&env, product, client, gw).await;
    let (_, c) = unpaid(&env, product, client, gw).await;

    // Credit only: cancelling gives the applied credit back.
    give_credit(&env, client, "50").await;
    apply_credit(&env, a, "50").await;
    assert_eq!(credit(&env, client).await, d("0"));
    assert!(
        !invoices::cancel_invoice(&env.pool, env.tenant, a)
            .await
            .unwrap(),
        "the raw cancel refuses an invoice holding credit"
    );
    match cancel(&env, a, false).await {
        CancelOutcome::Cancelled {
            credit_returned, ..
        } => assert_eq!(credit_returned, d("50")),
        other => panic!("{other:?}"),
    }
    let row = invoice(&env, a).await;
    assert_eq!(row.status, InvoiceStatus::Cancelled);
    assert_eq!(row.credit_applied, d("0"));
    assert_eq!(credit(&env, client).await, d("50"), "credit given back");

    // Partly paid: refused until staff moves the payment to credit.
    pay(&env, b, "30").await;
    apply_credit(&env, b, "20").await;
    assert_eq!(credit(&env, client).await, d("30"));
    assert_eq!(
        cancel(&env, b, false).await,
        CancelOutcome::Blocked(CancelBlock::AmountPaid(d("30")))
    );
    assert_eq!(invoice(&env, b).await.status, InvoiceStatus::Unpaid);
    assert_eq!(credit(&env, client).await, d("30"), "nothing moved");
    match cancel(&env, b, true).await {
        CancelOutcome::Cancelled {
            credit_returned,
            paid_to_credit,
            ..
        } => {
            assert_eq!(credit_returned, d("20"));
            assert_eq!(paid_to_credit, d("30"));
        }
        other => panic!("{other:?}"),
    }
    let row = invoice(&env, b).await;
    assert_eq!(row.status, InvoiceStatus::Cancelled);
    assert_eq!((row.amount_paid, row.credit_applied), (d("0"), d("0")));
    assert_eq!(credit(&env, client).await, d("80"), "30 + 20 + 30");
    // The moved payment is linked to its transaction, so a gateway refund
    // of it takes the credit back instead of paying twice.
    let linked = env
        .count(&format!(
            "select count(*) from credits where tenant_id = $1 and invoice_id = '{b}'
               and kind = 'overpayment' and transaction_id is not null and amount = 30"
        ))
        .await;
    assert_eq!(linked, 1);

    // Not open: a paid invoice is never cancelled.
    pay(&env, c, "120").await;
    assert_eq!(
        cancel(&env, c, true).await,
        CancelOutcome::Blocked(CancelBlock::NotOpen)
    );

    env.cleanup().await;
}

#[tokio::test]
async fn cancel_waits_for_a_payment_in_flight() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;

    let (_, a) = unpaid(&env, product, client, gw).await;
    env.start_payment(a, gw).await;
    sqlx::query(
        "update payment_attempts set status = 'pending' where tenant_id = $1 and invoice_id = $2",
    )
    .bind(env.tenant)
    .bind(a)
    .execute(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        cancel(&env, a, true).await,
        CancelOutcome::Blocked(CancelBlock::PaymentInFlight)
    );

    let (_, b) = unpaid(&env, product, client, gw).await;
    sqlx::query("update invoices set status = 'payment_pending' where tenant_id = $1 and id = $2")
        .bind(env.tenant)
        .bind(b)
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        cancel(&env, b, false).await,
        CancelOutcome::Blocked(CancelBlock::PaymentInFlight)
    );
    assert_eq!(invoice(&env, b).await.status, InvoiceStatus::PaymentPending);

    // A started (not pending) attempt does not block: a late payment of a
    // cancelled invoice is kept as credit by `record_payment`.
    let (_, c) = unpaid(&env, product, client, gw).await;
    env.start_payment(c, gw).await;
    assert!(matches!(
        cancel(&env, c, false).await,
        CancelOutcome::Cancelled { .. }
    ));

    env.cleanup().await;
}

#[tokio::test]
async fn order_cancel_keeps_money_and_returns_credit() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;

    // Partly paid order invoice: the order is not cancelled.
    let (order, inv) = unpaid(&env, product, client, gw).await;
    pay(&env, inv, "10").await;
    let err = env
        .billing
        .cancel_order(env.tenant, order)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err:?}");
    let o = orders::get_order(&env.pool, env.tenant, order)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(o.status, OrderStatus::Cancelled);
    assert_eq!(invoice(&env, inv).await.amount_paid, d("10"));

    // Credit only: the order is cancelled and the credit comes back.
    let (order, inv) = unpaid(&env, product, client, gw).await;
    give_credit(&env, client, "40").await;
    apply_credit(&env, inv, "40").await;
    let before = credit(&env, client).await;
    env.billing.cancel_order(env.tenant, order).await.unwrap();
    assert_eq!(invoice(&env, inv).await.status, InvoiceStatus::Cancelled);
    assert_eq!(credit(&env, client).await, before + d("40"));

    env.cleanup().await;
}
