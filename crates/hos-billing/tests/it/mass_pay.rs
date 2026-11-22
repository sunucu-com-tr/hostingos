//! Mass payment: one combined invoice pays several invoices through the
//! normal (mock) gateway path; each invoice is applied exactly once,
//! amounts are computed server-side, replays change nothing and whatever an
//! invoice no longer needs stays as credit.

use crate::common::{Env, d, try_};
use chrono::Duration;
use hos_billing::payments::EventOutcome;
use hos_core::CoreError;
use hos_core::domain::{InvoiceStatus, ProductType};
use hos_core::ids::{ClientId, IntegrationId, InvoiceId, ProductId};
use hos_core::integrations::gateway::PaymentEventKind;
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::InvoiceItemType;
use hos_db::repo::carts::NewCartItem;
use hos_db::repo::invoices::{self, InvoiceRow};
use hos_db::repo::mass_pay;
use hos_db::repo::payments;
use serde_json::json;

async fn invoice(env: &Env, id: InvoiceId) -> InvoiceRow {
    invoices::get_invoice(&env.pool, env.tenant, id)
        .await
        .unwrap()
        .unwrap()
}

/// One checkout → one unpaid invoice of 120 TRY (100 + 20 % VAT).
async fn unpaid(env: &Env, product: ProductId, client: ClientId, gw: IntegrationId) -> InvoiceId {
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(product, BillingCycle::Monthly)],
        )
        .await;
    env.checkout(cart, client, Some(gw)).await.invoice_id
}

async fn credit(env: &Env, client: ClientId) -> rust_decimal::Decimal {
    payments::credit_balance(&env.pool, env.tenant, client, Currency::TRY)
        .await
        .unwrap()
}

async fn setup(env: &Env) -> (IntegrationId, ProductId, ClientId) {
    let gw = env.gateway().await;
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
    let client = env.client("Mass").await;
    (gw, product, client)
}

#[tokio::test]
async fn mass_payment_pays_each_invoice_exactly_once() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;
    let c = unpaid(&env, product, client, gw).await;
    // b is partly paid already: only its balance (70) is combined.
    let r = env.start_payment(b, gw).await;
    env.webhook(gw, PaymentEventKind::Paid, &r, try_("50"))
        .await;
    assert_eq!(invoice(&env, b).await.balance, d("70"));

    let combined = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b, c, a])
        .await
        .expect("mass payment");
    assert_eq!(
        combined.total,
        d("310"),
        "120 + 70 + 120, no tax on credit lines"
    );
    let lines = invoices::invoice_items(&env.pool, env.tenant, combined.id)
        .await
        .unwrap();
    assert_eq!(lines.len(), 3);
    assert!(
        lines
            .iter()
            .all(|l| l.item_type == InvoiceItemType::Credit && !l.taxable)
    );

    // Paid through the normal attempt / webhook path.
    let reference = env.start_payment(combined.id, gw).await;
    let res = env
        .webhook(gw, PaymentEventKind::Paid, &reference, try_("310"))
        .await;
    assert!(matches!(
        res.events[0].outcome,
        EventOutcome::PaymentRecorded {
            invoice_paid: true,
            ..
        }
    ));
    for id in [a, b, c] {
        let inv = invoice(&env, id).await;
        assert_eq!(inv.status, InvoiceStatus::Paid, "{id} paid");
        assert_eq!(inv.balance, d("0"));
    }
    assert_eq!(credit(&env, client).await, d("0"));
    // Each order provisioned once (paid hook of every invoice ran once).
    assert_eq!(env.pending_payloads("provision_service").await.len(), 3);
    let mp = mass_pay::for_invoice(&env.pool, env.tenant, combined.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mp.status, mass_pay::MassPaymentStatus::Applied);
    let items = mass_pay::items(&env.pool, env.tenant, mp.id).await.unwrap();
    assert!(items.iter().all(|i| i.applied_at.is_some()));
    let applied: rust_decimal::Decimal = items.iter().filter_map(|i| i.applied).sum();
    assert_eq!(applied, d("310"));

    // Replay: duplicate, nothing applied twice.
    let again = env
        .webhook(gw, PaymentEventKind::Paid, &reference, try_("310"))
        .await;
    assert_eq!(again.events[0].outcome, EventOutcome::Duplicate);
    assert_eq!(credit(&env, client).await, d("0"));
    assert_eq!(env.pending_payloads("provision_service").await.len(), 3);
    assert_eq!(
        env.count("select count(*) from credits where tenant_id = $1 and kind = 'invoice_payment'")
            .await,
        3
    );
    env.cleanup().await;
}

#[tokio::test]
async fn mass_payment_rejects_foreign_paid_and_combined_invoices() {
    let Some(env) = Env::new().await else { return };
    let Some(other_env) = Env::new().await else {
        return;
    };
    let (gw, product, client) = setup(&env).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;

    // Another client's invoice.
    let stranger = env.client("Other").await;
    let foreign = unpaid(&env, product, stranger, gw).await;
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, foreign])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound { .. }), "{err:?}");

    // Another tenant's invoice (same id space, other tenant).
    let (ogw, oproduct, oclient) = setup(&other_env).await;
    let other_tenant_invoice = unpaid(&other_env, oproduct, oclient, ogw).await;
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, other_tenant_invoice])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound { .. }), "{err:?}");

    // One invoice only / unknown id.
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, a])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, InvoiceId::new()])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound { .. }), "{err:?}");

    // A paid invoice.
    env.pay(b, gw).await;
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");

    // A combined invoice cannot itself be combined; an add-funds one neither.
    let c = unpaid(&env, product, client, gw).await;
    let combined = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, c])
        .await
        .unwrap();
    let d2 = unpaid(&env, product, client, gw).await;
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[combined.id, d2])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");
    let top_up = env
        .billing
        .order_add_funds(env.tenant, client, d("100"))
        .await
        .unwrap();
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[top_up.id, d2])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");
    assert_eq!(
        env.count("select count(*) from mass_payments where tenant_id = $1")
            .await,
        1
    );
    other_env.cleanup().await;
    env.cleanup().await;
}

#[tokio::test]
async fn newer_selection_replaces_unpaid_one_and_leftover_stays_credit() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;
    let c = unpaid(&env, product, client, gw).await;

    let first = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap();
    let second = env
        .billing
        .create_mass_payment(env.tenant, client, &[b, c])
        .await
        .unwrap();
    assert_eq!(
        invoice(&env, first.id).await.status,
        InvoiceStatus::Cancelled
    );
    let first_mp = mass_pay::for_invoice(&env.pool, env.tenant, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_mp.status, mass_pay::MassPaymentStatus::Cancelled);

    // An underpayment of the combined invoice applies nothing.
    let r = env.start_payment(second.id, gw).await;
    env.webhook(gw, PaymentEventKind::Paid, &r, try_("100"))
        .await;
    assert_eq!(invoice(&env, b).await.status, InvoiceStatus::Unpaid);
    assert_eq!(invoice(&env, c).await.status, InvoiceStatus::Unpaid);
    // A combined invoice with money on it cannot be replaced.
    let err = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err:?}");

    // c gets paid on its own meanwhile; the rest of the combined invoice
    // then pays b and leaves c's share as credit.
    env.pay(c, gw).await;
    let r = env.start_payment(second.id, gw).await;
    env.webhook(gw, PaymentEventKind::Paid, &r, try_("140"))
        .await;
    assert_eq!(invoice(&env, second.id).await.status, InvoiceStatus::Paid);
    assert_eq!(invoice(&env, b).await.status, InvoiceStatus::Paid);
    assert_eq!(credit(&env, client).await, d("120"));
    assert_eq!(invoice(&env, a).await.status, InvoiceStatus::Unpaid);
    env.cleanup().await;
}

#[tokio::test]
async fn stale_combined_invoices_are_cancelled() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;
    let combined = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap();
    assert_eq!(
        env.billing
            .cancel_stale_mass_payments(env.tenant)
            .await
            .unwrap(),
        0
    );
    env.clock.advance(Duration::days(4));
    assert_eq!(
        env.billing
            .cancel_stale_mass_payments(env.tenant)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        invoice(&env, combined.id).await.status,
        InvoiceStatus::Cancelled
    );
    assert_eq!(invoice(&env, a).await.status, InvoiceStatus::Unpaid);
    env.cleanup().await;
}

// ---------------------------------------------------------------------------
// Refunds and chargebacks reach the invoices a mass payment paid (item 3);
// combined invoices cannot buy credit past the add-funds limits (item 7).
// ---------------------------------------------------------------------------

use chrono::Utc;
use hos_billing::payments::RefundInput;
use hos_db::repo::affiliates as arepo;
use rust_decimal::Decimal;

fn first(res: &hos_billing::WebhookResult) -> &EventOutcome {
    &res.events.first().expect("one event").outcome
}

/// Pays `invoice` in full through the mock gateway; returns the payment.
async fn pay_txn(env: &Env, gw: IntegrationId, invoice: InvoiceId) -> payments::TransactionRow {
    let amount = invoice_balance(env, invoice).await;
    let reference = env.start_payment(invoice, gw).await;
    let res = env
        .webhook(gw, PaymentEventKind::Paid, &reference, amount)
        .await;
    assert!(
        matches!(first(&res), EventOutcome::PaymentRecorded { .. }),
        "{res:?}"
    );
    payments::transaction_by_gateway_id(
        &env.pool,
        env.tenant,
        Some(gw),
        &format!("mock_{reference}"),
    )
    .await
    .unwrap()
    .expect("payment")
}

async fn invoice_balance(env: &Env, id: InvoiceId) -> hos_core::money::Money {
    invoice(env, id).await.balance_money()
}

/// A signed mock chargeback of `amount` of `txn` (`id`: dispute id).
async fn chargeback(
    env: &Env,
    gw: IntegrationId,
    txn: &payments::TransactionRow,
    amount: &str,
    id: &str,
) -> hos_billing::WebhookResult {
    let mut event = hos_payments::mock::MockEvent::new(
        PaymentEventKind::Chargeback,
        txn.gateway_txn_id.clone().expect("gateway id"),
        try_(amount),
    );
    event.id = format!("mock:dispute:{id}");
    let req = hos_payments::mock::webhook_request(
        crate::common::GATEWAY_SECRET.as_bytes(),
        &event,
        Utc::now().timestamp(),
    );
    env.billing
        .handle_webhook(env.tenant, gw, &req)
        .await
        .expect("webhook")
}

fn refund_input(txn: &payments::TransactionRow, amount: &str) -> RefundInput {
    RefundInput {
        transaction_id: txn.id,
        amount: d(amount),
        reason: Some("test".into()),
        to_credit: false,
        staff_user_id: None,
        ip: None,
    }
}

/// Every service of the tenant counts as provisioned and running.
async fn activate_services(env: &Env) {
    sqlx::query("update services set status = 'active' where tenant_id = $1")
        .bind(env.tenant)
        .execute(&env.pool)
        .await
        .unwrap();
}

/// `client` was referred by an affiliate (10 % commission).
async fn referred(env: &Env, client: ClientId) {
    env.setting("affiliates.enabled", json!(true)).await;
    env.setting("affiliates.commission_type", json!("percent"))
        .await;
    env.setting("affiliates.commission_value", json!("10"))
        .await;
    let partner = env.client("Partner").await;
    let code = format!("AFF{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
    let affiliate = arepo::create_affiliate(&env.pool, env.tenant, partner, &code, Currency::TRY)
        .await
        .unwrap();
    arepo::create_referral(
        &env.pool,
        env.tenant,
        affiliate.id,
        client,
        "staff",
        None,
        None,
    )
    .await
    .unwrap()
    .expect("referral");
}

async fn commissions(env: &Env, invoice: InvoiceId) -> Vec<arepo::CommissionRow> {
    arepo::commissions_for_invoice(&env.pool, env.tenant, invoice)
        .await
        .unwrap()
}

/// A chargeback of a mass payment reopened only the combined invoice (and
/// took the credit back on top: the client owed twice); the invoices the
/// money paid stayed paid, their services ran on and their commissions
/// were kept.
#[tokio::test]
async fn chargeback_of_a_mass_payment_reopens_the_invoices_it_paid() {
    let Some(env) = Env::new().await else { return };
    env.staff().await;
    let (gw, product, client) = setup(&env).await;
    referred(&env, client).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;
    activate_services(&env).await;
    let combined = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap();
    let txn = pay_txn(&env, gw, combined.id).await;
    for id in [a, b] {
        assert_eq!(invoice(&env, id).await.status, InvoiceStatus::Paid);
        assert_eq!(commissions(&env, id).await.len(), 1, "commission accrued");
    }
    assert_eq!(credit(&env, client).await, d("0"));

    let res = chargeback(&env, gw, &txn, "240", "cb1").await;
    assert!(
        matches!(first(&res), EventOutcome::Chargeback { .. }),
        "{res:?}"
    );
    for id in [a, b] {
        let inv = invoice(&env, id).await;
        assert_eq!(
            (inv.status, inv.balance),
            (InvoiceStatus::Unpaid, d("120")),
            "{id} reopened"
        );
    }
    let c = invoice(&env, combined.id).await;
    assert_eq!(
        c.status,
        InvoiceStatus::Refunded,
        "the combined invoice is not owed again"
    );
    assert_eq!(
        credit(&env, client).await,
        d("0"),
        "owed once, on the invoices"
    );
    assert_eq!(
        env.pending_payloads("suspend_service").await.len(),
        2,
        "both services suspended by the chargeback policy"
    );
    let (reversed, _) = env
        .billing
        .run_affiliate_maintenance(env.tenant)
        .await
        .unwrap();
    assert_eq!(reversed, 2);
    for id in [a, b] {
        let c = &commissions(&env, id).await[0];
        assert_eq!(c.reversal_reason.as_deref(), Some("chargeback"));
    }
    let (again, _) = env
        .billing
        .run_affiliate_maintenance(env.tenant)
        .await
        .unwrap();
    assert_eq!(again, 0, "reversed once");

    // Replays and further disputes change nothing.
    let res = chargeback(&env, gw, &txn, "240", "cb1").await;
    assert_eq!(first(&res), &EventOutcome::Duplicate);
    let res = chargeback(&env, gw, &txn, "10", "cb2").await;
    assert_eq!(first(&res), &EventOutcome::Duplicate);
    assert_eq!(invoice(&env, a).await.balance, d("120"));
    assert_eq!(credit(&env, client).await, d("0"));
    env.cleanup().await;
}

/// A refund of a mass payment was refused ("the credit was already used")
/// although the credit had paid invoices of the same payment; partial
/// refunds now give it back from the newest invoice first.
#[tokio::test]
async fn partial_refunds_of_a_mass_payment_reach_its_invoices() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;
    let combined = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap();
    let txn = pay_txn(&env, gw, combined.id).await;
    let mp = mass_pay::for_invoice(&env.pool, env.tenant, combined.id)
        .await
        .unwrap()
        .unwrap();
    let items = mass_pay::items(&env.pool, env.tenant, mp.id).await.unwrap();
    // Undone newest first: the item applied last.
    let mut order: Vec<_> = items
        .iter()
        .map(|i| (i.applied_at, i.id, i.invoice_id))
        .collect();
    order.sort();
    let (newest, oldest) = (order[1].2, order[0].2);

    let refund = env
        .billing
        .refund_payment(env.tenant, &refund_input(&txn, "100"))
        .await
        .expect("refund of a mass payment");
    assert_eq!(
        refund.status,
        hos_core::integrations::gateway::RefundStatus::Succeeded
    );
    let n = invoice(&env, newest).await;
    assert_eq!(
        (n.status, n.credit_applied),
        (InvoiceStatus::PartiallyRefunded, d("20"))
    );
    assert_eq!(invoice(&env, oldest).await.status, InvoiceStatus::Paid);
    assert_eq!(
        invoice(&env, combined.id).await.status,
        InvoiceStatus::PartiallyRefunded
    );
    assert_eq!(credit(&env, client).await, d("0"), "nothing left negative");

    env.billing
        .refund_payment(env.tenant, &refund_input(&txn, "140"))
        .await
        .expect("rest");
    for id in [a, b] {
        let inv = invoice(&env, id).await;
        assert_eq!(
            (inv.status, inv.credit_applied),
            (InvoiceStatus::Refunded, d("0")),
            "{id}"
        );
    }
    assert_eq!(
        invoice(&env, combined.id).await.status,
        InvoiceStatus::Refunded
    );
    assert_eq!(credit(&env, client).await, d("0"));
    let reversed: Decimal = mass_pay::items(&env.pool, env.tenant, mp.id)
        .await
        .unwrap()
        .iter()
        .map(|i| i.reversed)
        .sum();
    assert_eq!(reversed, d("240"));
    let err = env
        .billing
        .refund_payment(env.tenant, &refund_input(&txn, "1"))
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "{err:?}");
    env.cleanup().await;
}

/// Add-funds credit spent on an unrelated invoice, then the top-up is
/// charged back: the top-up invoice used to be reopened on top of the
/// negative balance (owed twice). Now it is closed; the debt is the
/// negative balance, which cannot be spent and is flagged for staff.
#[tokio::test]
async fn charged_back_top_up_spent_elsewhere_leaves_one_negative_balance() {
    let Some(env) = Env::new().await else { return };
    env.staff().await;
    let (gw, product, client) = setup(&env).await;
    let top_up = env
        .billing
        .order_add_funds(env.tenant, client, d("120"))
        .await
        .unwrap();
    let txn = pay_txn(&env, gw, top_up.id).await;
    let other = unpaid(&env, product, client, gw).await;
    env.billing
        .pay_with_credit(env.tenant, other, Some(client), None, None)
        .await
        .unwrap();
    assert_eq!(invoice(&env, other).await.status, InvoiceStatus::Paid);

    let res = chargeback(&env, gw, &txn, "120", "topup").await;
    assert!(
        matches!(first(&res), EventOutcome::Chargeback { .. }),
        "{res:?}"
    );
    assert_eq!(
        invoice(&env, top_up.id).await.status,
        InvoiceStatus::Refunded
    );
    assert_eq!(
        credit(&env, client).await,
        d("-120"),
        "the client owes it once"
    );
    assert_eq!(
        env.notifications("payment.chargeback_credit_shortfall")
            .await,
        1
    );
    // A negative balance pays nothing.
    let next = unpaid(&env, product, client, gw).await;
    let (inv, applied) = env
        .billing
        .pay_with_credit(env.tenant, next, Some(client), None, None)
        .await
        .unwrap();
    assert_eq!((inv.status, applied), (InvoiceStatus::Unpaid, d("0")));
    env.cleanup().await;
}

/// Combined invoice → its invoices paid one by one → the combined invoice
/// paid as well: the whole amount became credit past the add-funds limits.
#[tokio::test]
async fn invoices_paid_separately_void_their_combined_invoice() {
    let Some(env) = Env::new().await else { return };
    let (gw, product, client) = setup(&env).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;
    let combined = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap();
    env.pay(a, gw).await;
    assert_eq!(
        invoice(&env, combined.id).await.status,
        InvoiceStatus::Cancelled
    );
    let mp = mass_pay::for_invoice(&env.pool, env.tenant, combined.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mp.status, mass_pay::MassPaymentStatus::Cancelled);
    env.pay(b, gw).await;
    let err = env
        .billing
        .start_payment(
            env.tenant,
            combined.id,
            &hos_billing::StartPayment {
                integration_id: Some(gw),
                ..hos_billing::StartPayment::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err:?}");
    assert_eq!(credit(&env, client).await, d("0"));

    // A member partly paid meanwhile (not settled): paying the combined
    // invoice is refused and it is cancelled.
    let c = unpaid(&env, product, client, gw).await;
    let e = unpaid(&env, product, client, gw).await;
    let second = env
        .billing
        .create_mass_payment(env.tenant, client, &[c, e])
        .await
        .unwrap();
    let r = env.start_payment(c, gw).await;
    env.webhook(gw, PaymentEventKind::Paid, &r, try_("50"))
        .await;
    let err = env
        .billing
        .start_payment(
            env.tenant,
            second.id,
            &hos_billing::StartPayment {
                integration_id: Some(gw),
                ..hos_billing::StartPayment::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_)), "{err:?}");
    assert_eq!(
        invoice(&env, second.id).await.status,
        InvoiceStatus::Cancelled
    );
    env.cleanup().await;
}

/// A combined invoice already partly paid when one of its invoices is paid
/// elsewhere: the rest applies only what is still owed; credit left over
/// past the add-funds limits is flagged for a refund.
#[tokio::test]
async fn leftover_credit_past_the_limits_is_flagged() {
    let Some(env) = Env::new().await else { return };
    env.staff().await;
    env.setting("billing.credit_balance_max", json!("50")).await;
    let (gw, product, client) = setup(&env).await;
    let a = unpaid(&env, product, client, gw).await;
    let b = unpaid(&env, product, client, gw).await;
    let combined = env
        .billing
        .create_mass_payment(env.tenant, client, &[a, b])
        .await
        .unwrap();
    let r = env.start_payment(combined.id, gw).await;
    env.webhook(gw, PaymentEventKind::Paid, &r, try_("10"))
        .await;
    env.pay(b, gw).await;
    assert_eq!(
        invoice(&env, combined.id).await.status,
        InvoiceStatus::Unpaid,
        "money on it: kept"
    );
    let r = env.start_payment(combined.id, gw).await;
    env.webhook(gw, PaymentEventKind::Paid, &r, try_("230"))
        .await;
    assert_eq!(invoice(&env, a).await.status, InvoiceStatus::Paid);
    assert_eq!(
        credit(&env, client).await,
        d("120"),
        "b's share is left over"
    );
    assert_eq!(env.notifications("payment.mass_pay_excess_credit").await, 1);
    env.cleanup().await;
}
