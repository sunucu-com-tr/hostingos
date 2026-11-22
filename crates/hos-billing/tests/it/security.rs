//! Regression tests of security findings in the billing engine, against the
//! real database with mock providers:
//!
//! * VM reinstall only with an offered template (no cloning of other disks);
//! * downgrade credit granted only when the plan change is applied, one
//!   pending change per service;
//! * refunds take back the credit their payment created (overpayment, add
//!   funds) and touch the invoice only for the part applied to it;
//! * a synchronous refund is not recorded again by its webhook, and the same
//!   webhook delivered concurrently is applied once;
//! * mock providers need an explicit opt-in;
//! * `email_log` never stores one-time links in plain text.

use std::sync::Arc;
use std::time::Duration;

use crate::common::{Env, GATEWAY_SECRET, d, suffix, try_};
use chrono::Utc;
use hos_billing::lifecycle::LifecycleOutcome;
use hos_billing::notify::{DeliveryOutcome, OutgoingEmail};
use hos_billing::payments::{EventOutcome, RefundInput, StartPayment};
use hos_billing::provisioning::{ProvisionOutcome, RunContext};
use hos_billing::registry::SaveIntegration;
use hos_billing::{Billing, BillingConfig, IntegrationRegistry, ManualClock, NoFraudCheck};
use hos_core::CoreError;
use hos_core::domain::{IntegrationCategory, InvoiceStatus, Locale, ProductType, ServiceStatus};
use hos_core::ids::{ClientId, IntegrationId, InvoiceId, ServiceId};
use hos_core::integrations::gateway::{PaymentEventKind, RefundStatus};
use hos_core::integrations::mailer::Mailbox;
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::carts::NewCartItem;
use hos_db::repo::payments::{self, TransactionRow};
use hos_db::repo::{invoices, services};
use rust_decimal::Decimal;
use serde_json::json;

fn recorded(r: &hos_billing::WebhookResult) -> &EventOutcome {
    &r.events.first().expect("one event").outcome
}

async fn credit(env: &Env, client: ClientId) -> Decimal {
    payments::credit_balance(&env.pool, env.tenant, client, Currency::TRY)
        .await
        .unwrap()
}

async fn invoice(env: &Env, id: InvoiceId) -> invoices::InvoiceRow {
    invoices::get_invoice(&env.pool, env.tenant, id)
        .await
        .unwrap()
        .unwrap()
}

/// An unpaid 120 TRY invoice (100 + KDV %20) of a new client.
async fn open_invoice(env: &Env, name: &str, gw: IntegrationId) -> (ClientId, InvoiceId) {
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
    let client = env.client(name).await;
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

/// Pays `amount` for `invoice` through the mock gateway; returns the payment.
async fn pay(env: &Env, gw: IntegrationId, invoice: InvoiceId, amount: &str) -> TransactionRow {
    let reference = env.start_payment(invoice, gw).await;
    let res = env
        .webhook(gw, PaymentEventKind::Paid, &reference, try_(amount))
        .await;
    assert!(
        matches!(recorded(&res), EventOutcome::PaymentRecorded { .. }),
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

fn staff_refund(txn: &TransactionRow, amount: &str) -> RefundInput {
    RefundInput {
        transaction_id: txn.id,
        amount: d(amount),
        reason: Some("test".into()),
        to_credit: false,
        staff_user_id: None,
        ip: None,
    }
}

/// Delivers a signed mock `Refunded` webhook for `txn`.
async fn refund_webhook(
    env: &Env,
    gw: IntegrationId,
    txn: &TransactionRow,
    amount: &str,
    refund_id: Option<&str>,
) -> hos_billing::WebhookResult {
    let mut event = hos_payments::mock::MockEvent::new(
        PaymentEventKind::Refunded,
        txn.gateway_txn_id.clone().expect("gateway id"),
        try_(amount),
    );
    if let Some(id) = refund_id {
        event = event.with_refund_id(id);
    }
    let req = hos_payments::mock::webhook_request(
        GATEWAY_SECRET.as_bytes(),
        &event,
        Utc::now().timestamp(),
    );
    env.billing
        .handle_webhook(env.tenant, gw, &req)
        .await
        .expect("webhook")
}

// ---------------------------------------------------------------------------
// F1: VM reinstall templates
// ---------------------------------------------------------------------------

/// An active VPS of a new client on `virt` (synchronous mock hypervisor).
async fn active_vps(
    env: &Env,
    gw: IntegrationId,
    virt: IntegrationId,
    name: &str,
) -> (ClientId, ServiceId) {
    let product = env
        .product(
            "VPS 1",
            ProductType::Vps,
            Some(virt),
            json!({"template": "ubuntu-24.04", "cpu_cores": 1, "memory_mb": 1024, "disk_gb": 40}),
            "100",
            "0",
        )
        .await;
    let client = env.client(name).await;
    let mut item = NewCartItem::product(product, BillingCycle::Monthly);
    item.hostname = Some(format!("vps-{}.example.com", suffix()));
    let cart = env.cart(client, &[item]).await;
    let out = env.checkout(cart, client, Some(gw)).await;
    env.pay(out.invoice_id, gw).await;
    let svc = out.services[0];
    let mut outcome = env
        .billing
        .provision_service(env.tenant, svc, RunContext::default())
        .await
        .unwrap();
    for _ in 0..5 {
        if outcome == ProvisionOutcome::Activated {
            break;
        }
        let payloads = env.pending_payloads("poll_vm_task").await;
        let p: hos_billing::jobs::PollVmTaskPayload =
            serde_json::from_value(payloads.last().expect("poll job").clone()).unwrap();
        env.complete_jobs("poll_vm_task").await;
        if let Some(o) = env
            .billing
            .poll_vm_task(env.tenant, &p, RunContext::default())
            .await
            .unwrap()
        {
            outcome = o;
        }
    }
    assert_eq!(outcome, ProvisionOutcome::Activated);
    env.complete_jobs("provision_service").await;
    (client, svc)
}

/// The REST API passed any template id to the hypervisor: another customer's
/// VM id cloned their disk. Every caller is now checked against the offered
/// templates, the staff path included.
#[tokio::test]
async fn vm_reinstall_only_accepts_offered_templates() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(false).await;
    env.ip_pool(virt).await;
    let (_victim, victim_svc) = active_vps(&env, gw, virt, "Victim").await;
    let (client, svc) = active_vps(&env, gw, virt, "Attacker").await;
    let victim_vm = services::vm_for_service(&env.pool, t, victim_svc)
        .await
        .unwrap()
        .expect("victim vm");
    let own_vm = services::vm_for_service(&env.pool, t, svc)
        .await
        .unwrap()
        .expect("own vm");

    for (who, caller) in [("client", Some(client)), ("staff", None)] {
        for template in [victim_vm.external_id.as_str(), "", "../../etc/passwd"] {
            let err = env
                .billing
                .vm_reinstall(t, svc, caller, template, Vec::new())
                .await
                .expect_err("not an offered template");
            assert!(
                matches!(err, CoreError::Validation(_)),
                "{who} {template:?}: {err}"
            );
        }
    }
    // Nothing reached the hypervisor.
    let after = services::vm_for_service(&env.pool, t, svc)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, own_vm.status);

    // Offered templates (the hypervisor's list, the product's own) work.
    let offered = env
        .billing
        .vm_templates(t, svc, Some(client))
        .await
        .unwrap();
    assert!(offered.iter().any(|tpl| tpl.id == "debian-12"));
    env.billing
        .vm_reinstall(t, svc, Some(client), "debian-12", Vec::new())
        .await
        .unwrap();
    env.cleanup().await;
}

/// The OS template of a new VM came from the service's configurable
/// options unchecked: a quantity option mapped to a template key let the
/// client type any number (a VM id to clone), and stored options were never
/// compared with what the product offers. Both order and provisioning now
/// check the product's offered templates.
#[tokio::test]
async fn vm_create_only_uses_templates_the_product_offers() {
    use hos_db::enums::ConfigOptionType;
    use hos_db::repo::catalog;
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(false).await;
    env.ip_pool(virt).await;
    let product = env
        .product(
            "VPS options",
            ProductType::Vps,
            Some(virt),
            json!({"template": "ubuntu-24.04", "cpu_cores": 1, "memory_mb": 1024, "disk_gb": 40}),
            "100",
            "0",
        )
        .await;
    let group = catalog::create_config_option_group(&env.pool, t, "OS", None)
        .await
        .unwrap();
    catalog::attach_config_group(&env.pool, t, group, product)
        .await
        .unwrap();
    let os = catalog::create_config_option(
        &env.pool,
        t,
        group,
        "OS",
        ConfigOptionType::Dropdown,
        Some("template"),
        &json!({}),
        0,
    )
    .await
    .unwrap();
    let debian = catalog::create_config_option_value(
        &env.pool,
        t,
        os,
        "Debian 12",
        "debian-12",
        &json!({}),
        0,
    )
    .await
    .unwrap();
    let image = catalog::create_config_option(
        &env.pool,
        t,
        group,
        "Image",
        ConfigOptionType::Quantity,
        Some("os_template"),
        &json!({}),
        1,
    )
    .await
    .unwrap();
    let image_value =
        catalog::create_config_option_value(&env.pool, t, image, "Image", "", &json!({}), 0)
            .await
            .unwrap();
    let client = env.client("Options").await;
    let item = |selections: serde_json::Value| {
        let mut it = NewCartItem::product(product, BillingCycle::Monthly);
        it.hostname = Some(format!("vps-{}.example.com", suffix()));
        it.config_selections = selections;
        it
    };

    // A quantity option cannot pick the template (here: VM id 200).
    let cart = env
        .cart(
            client,
            &[item(json!({
                os.to_string(): {"value_id": debian.to_string()},
                image.to_string(): {"value_id": image_value.to_string(), "quantity": 200},
            }))],
        )
        .await;
    let err = env
        .billing
        .checkout(
            t,
            &hos_billing::CheckoutRequest {
                cart_id: cart,
                client_id: client,
                user_id: None,
                payment_integration_id: Some(gw),
                ip: None,
                user_agent: None,
                notes: None,
            },
        )
        .await
        .expect_err("quantity option as template");
    assert!(
        matches!(&err, CoreError::Validation(m) if m.contains("not offered")),
        "{err}"
    );

    // An offered value orders fine …
    let cart = env
        .cart(
            client,
            &[item(
                json!({os.to_string(): {"value_id": debian.to_string()}}),
            )],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    let svc = out.services[0];
    let stored = services::get_service(&env.pool, t, svc)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.module_data["options"]["template"], "debian-12");
    // … but options that are not offered (any more) never reach the hypervisor.
    sqlx::query(
        "update services set module_data = jsonb_set(module_data, '{options,template}', '\"4242\"')
         where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(svc)
    .execute(&env.pool)
    .await
    .unwrap();
    env.pay(out.invoice_id, gw).await;
    let outcome = env
        .billing
        .provision_service(
            t,
            svc,
            RunContext {
                final_attempt: true,
                ..RunContext::default()
            },
        )
        .await;
    assert!(
        matches!(&outcome, Ok(ProvisionOutcome::Failed { reason }) if reason.contains("not offered")),
        "{outcome:?}"
    );
    assert!(
        services::vm_for_service(&env.pool, t, svc)
            .await
            .unwrap()
            .is_none(),
        "no VM was created"
    );
    env.cleanup().await;
}

// ---------------------------------------------------------------------------
// F2: plan changes
// ---------------------------------------------------------------------------

async fn active_hosting(env: &Env) -> (IntegrationId, IntegrationId, ServiceId) {
    let gw = env.gateway().await;
    let panel = env.panel().await;
    let product = env
        .product(
            "Hosting",
            ProductType::SharedHosting,
            Some(panel),
            json!({"package": "starter"}),
            "100",
            "0",
        )
        .await;
    let client = env.client("Plan").await;
    let cart = env
        .cart(
            client,
            &[Env::hosting_item(
                product,
                &format!("plan-{}.com", suffix()),
            )],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    env.pay(out.invoice_id, gw).await;
    let svc = out.services[0];
    assert_eq!(
        env.billing
            .provision_service(env.tenant, svc, RunContext::default())
            .await
            .unwrap(),
        ProvisionOutcome::Activated
    );
    env.complete_jobs("provision_service").await;
    (gw, panel, svc)
}

async fn upgrade_status(env: &Env, id: uuid::Uuid) -> String {
    sqlx::query_scalar("select status from service_upgrades where id = $1")
        .bind(id)
        .fetch_one(&env.pool)
        .await
        .unwrap()
}

/// The downgrade credit was granted when the change was ordered, before the
/// `change_package` job ran: ordering again while one was pending, or a
/// failing job, left free credit behind.
#[tokio::test]
async fn downgrade_credit_is_granted_only_when_the_change_is_applied() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let (_gw, panel, svc_id) = active_hosting(&env).await;
    let svc = services::get_service(&env.pool, t, svc_id)
        .await
        .unwrap()
        .unwrap();
    let client = svc.client_id;
    // "Small" has no panel package: applying it fails permanently.
    let small = env
        .product(
            "Small Hosting",
            ProductType::SharedHosting,
            Some(panel),
            json!({}),
            "50",
            "0",
        )
        .await;
    let tiny = env
        .product(
            "Tiny Hosting",
            ProductType::SharedHosting,
            Some(panel),
            json!({"package": "business"}),
            "40",
            "0",
        )
        .await;
    sqlx::query("update products set upgrade_product_ids = array[$2, $3] where id = $1")
        .bind(svc.product_id)
        .bind(small)
        .bind(tiny)
        .execute(&env.pool)
        .await
        .unwrap();

    let order = env
        .billing
        .order_plan_change(t, svc_id, Some(client), small)
        .await
        .unwrap();
    assert!(order.invoice_id.is_none());
    assert!(order.credited > Decimal::ZERO);
    assert_eq!(credit(&env, client).await, Decimal::ZERO, "nothing yet");
    // One pending change per service.
    let err = env
        .billing
        .order_plan_change(t, svc_id, Some(client), tiny)
        .await
        .expect_err("a change is pending");
    assert!(matches!(err, CoreError::Conflict(_)), "{err}");
    assert_eq!(credit(&env, client).await, Decimal::ZERO);

    // The job fails for good: the change is marked failed, no credit.
    let out = env
        .billing
        .apply_plan_change(t, order.upgrade_id, RunContext::default())
        .await
        .unwrap();
    assert!(matches!(out, LifecycleOutcome::Failed { .. }), "{out:?}");
    assert_eq!(upgrade_status(&env, order.upgrade_id).await, "failed");
    assert_eq!(credit(&env, client).await, Decimal::ZERO);
    env.complete_jobs("change_package").await;

    // A working downgrade is credited once, when applied.
    let order = env
        .billing
        .order_plan_change(t, svc_id, Some(client), tiny)
        .await
        .unwrap();
    assert_eq!(credit(&env, client).await, Decimal::ZERO);
    let out = env
        .billing
        .apply_plan_change(t, order.upgrade_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(out, LifecycleOutcome::Done);
    assert_eq!(upgrade_status(&env, order.upgrade_id).await, "applied");
    assert_eq!(credit(&env, client).await, order.credited);
    let again = env
        .billing
        .apply_plan_change(t, order.upgrade_id, RunContext::default())
        .await
        .unwrap();
    assert!(matches!(again, LifecycleOutcome::Skipped { .. }));
    assert_eq!(credit(&env, client).await, order.credited, "credited once");
    let svc = services::get_service(&env.pool, t, svc_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((svc.product_id, svc.status), (tiny, ServiceStatus::Active));
    env.cleanup().await;
}

// ---------------------------------------------------------------------------
// F3: refunds and the credit a payment created
// ---------------------------------------------------------------------------

/// Refunding an overpaying payment left its overpayment credit behind and
/// lowered the invoice by the whole refund.
#[tokio::test]
async fn refund_takes_back_overpayment_credit_before_touching_the_invoice() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let (client, inv) = open_invoice(&env, "Over", gw).await;
    let txn = pay(&env, gw, inv, "150").await;
    assert_eq!(credit(&env, client).await, d("30"));

    // 20 comes out of the 30 overpayment credit; the invoice stays paid.
    let r = env
        .billing
        .refund_payment(t, &staff_refund(&txn, "20"))
        .await
        .unwrap();
    assert_eq!(r.status, RefundStatus::Succeeded);
    assert_eq!(credit(&env, client).await, d("10"));
    let i = invoice(&env, inv).await;
    assert_eq!((i.status, i.amount_paid), (InvoiceStatus::Paid, d("120")));

    // The rest: 10 of credit, 120 of invoice.
    env.billing
        .refund_payment(t, &staff_refund(&txn, "130"))
        .await
        .unwrap();
    assert_eq!(credit(&env, client).await, Decimal::ZERO);
    let i = invoice(&env, inv).await;
    assert_eq!(
        (i.status, i.amount_paid),
        (InvoiceStatus::Refunded, Decimal::ZERO)
    );
    // Nothing is left to refund.
    assert!(
        env.billing
            .refund_payment(t, &staff_refund(&txn, "1"))
            .await
            .is_err()
    );
    env.cleanup().await;
}

/// Overpayment credit spent on another invoice: staff cannot refund the
/// payment to the card (clear error, gateway not called); a refund the
/// gateway reports anyway is recorded, the balance goes negative and staff
/// are notified.
#[tokio::test]
async fn refund_of_spent_credit_is_refused_by_staff_but_recorded_from_the_gateway() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    env.staff().await;
    let gw = env.gateway().await;
    let (client, inv) = open_invoice(&env, "Spent", gw).await;
    let txn = pay(&env, gw, inv, "150").await;
    // Spend the 30 credit on a second invoice.
    let product = env
        .product("Extra", ProductType::Other, None, json!({}), "25", "0")
        .await;
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(product, BillingCycle::Monthly)],
        )
        .await;
    // (checkout applies credit automatically; otherwise pay with it)
    let second = env.checkout(cart, client, None).await;
    env.billing
        .pay_with_credit(t, second.invoice_id, Some(client), None, None)
        .await
        .unwrap();
    assert_eq!(
        invoice(&env, second.invoice_id).await.status,
        InvoiceStatus::Paid
    );
    assert_eq!(credit(&env, client).await, Decimal::ZERO);

    let err = env
        .billing
        .refund_payment(t, &staff_refund(&txn, "150"))
        .await
        .expect_err("credit already used");
    assert!(
        matches!(&err, CoreError::Validation(m) if m.contains("credit")),
        "{err}"
    );
    assert!(
        payments::transaction_refunds(&env.pool, t, txn.id)
            .await
            .unwrap()
            .is_empty(),
        "no refund opened, gateway not called"
    );

    // Refunded in the gateway dashboard anyway.
    let res = refund_webhook(&env, gw, &txn, "150", Some("re_dashboard_1")).await;
    assert!(
        matches!(recorded(&res), EventOutcome::Refunded { .. }),
        "{res:?}"
    );
    assert_eq!(credit(&env, client).await, d("-30"), "the client owes 30");
    let i = invoice(&env, inv).await;
    assert_eq!(
        (i.status, i.amount_paid),
        (InvoiceStatus::Refunded, Decimal::ZERO)
    );
    assert_eq!(
        env.notifications("payment.refund_credit_shortfall").await,
        1
    );
    env.cleanup().await;
}

/// "Add funds" credit was not linked to its payment, so refunding the
/// payment left the bought credit behind.
#[tokio::test]
async fn add_funds_credit_is_linked_to_its_payment_and_refunded_with_it() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let client = env.client("Funds").await;
    let inv = env
        .billing
        .order_add_funds(t, client, d("100"))
        .await
        .unwrap();
    let txn = pay(&env, gw, inv.id, "100").await;
    assert_eq!(credit(&env, client).await, d("100"));
    let linked: Decimal = sqlx::query_scalar(
        "select coalesce(sum(amount), 0) from credits where tenant_id = $1 and transaction_id = $2",
    )
    .bind(t)
    .bind(txn.id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(linked, d("100"));

    env.billing
        .refund_payment(t, &staff_refund(&txn, "100"))
        .await
        .unwrap();
    assert_eq!(credit(&env, client).await, Decimal::ZERO);
    assert_eq!(invoice(&env, inv.id).await.status, InvoiceStatus::Refunded);
    env.cleanup().await;
}

/// Delivers a signed mock `Chargeback` webhook for `txn` (`id` makes the
/// gateway event unique).
async fn chargeback_webhook(
    env: &Env,
    gw: IntegrationId,
    txn: &TransactionRow,
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
        GATEWAY_SECRET.as_bytes(),
        &event,
        Utc::now().timestamp(),
    );
    env.billing
        .handle_webhook(env.tenant, gw, &req)
        .await
        .expect("webhook")
}

/// A chargeback of an overpaying payment left its overpayment credit behind
/// and took the whole amount off the invoice (as refunds did).
#[tokio::test]
async fn chargeback_takes_back_overpayment_credit_before_touching_the_invoice() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let (client, inv) = open_invoice(&env, "Dispute", gw).await;
    let txn = pay(&env, gw, inv, "150").await;
    assert_eq!(credit(&env, client).await, d("30"));

    // 20 comes out of the 30 overpayment credit; the invoice stays paid.
    let res = chargeback_webhook(&env, gw, &txn, "20", "d1").await;
    assert!(
        matches!(
            recorded(&res),
            EventOutcome::Chargeback { invoice_id: None }
        ),
        "{res:?}"
    );
    assert_eq!(credit(&env, client).await, d("10"));
    let i = invoice(&env, inv).await;
    assert_eq!((i.status, i.amount_paid), (InvoiceStatus::Paid, d("120")));

    // The rest: 10 of credit, 120 of invoice (reopened).
    let res = chargeback_webhook(&env, gw, &txn, "130", "d2").await;
    assert!(
        matches!(recorded(&res), EventOutcome::Chargeback { invoice_id: Some(id) } if *id == inv),
        "{res:?}"
    );
    assert_eq!(credit(&env, client).await, Decimal::ZERO);
    let i = invoice(&env, inv).await;
    assert_eq!(
        (i.status, i.amount_paid),
        (InvoiceStatus::Unpaid, Decimal::ZERO)
    );
    // Nothing of the payment is left to charge back.
    let res = chargeback_webhook(&env, gw, &txn, "10", "d3").await;
    assert_eq!(recorded(&res), &EventOutcome::Duplicate);
    let charged: Decimal = sqlx::query_scalar(
        "select coalesce(sum(amount), 0) from transactions
         where tenant_id = $1 and kind = 'chargeback'",
    )
    .bind(t)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(charged, d("150"));
    assert_eq!(
        env.notifications("payment.chargeback_credit_shortfall")
            .await,
        0
    );
    env.cleanup().await;
}

/// Overpayment credit spent on another invoice, then the payment is charged
/// back: the chargeback is recorded anyway, the balance goes negative and
/// staff are notified.
#[tokio::test]
async fn chargeback_of_spent_credit_leaves_a_negative_balance_and_notifies_staff() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    env.staff().await;
    let gw = env.gateway().await;
    let (client, inv) = open_invoice(&env, "Spent dispute", gw).await;
    let txn = pay(&env, gw, inv, "150").await;
    let product = env
        .product("Extra", ProductType::Other, None, json!({}), "25", "0")
        .await;
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(product, BillingCycle::Monthly)],
        )
        .await;
    let second = env.checkout(cart, client, None).await;
    env.billing
        .pay_with_credit(t, second.invoice_id, Some(client), None, None)
        .await
        .unwrap();
    assert_eq!(credit(&env, client).await, Decimal::ZERO);

    let res = chargeback_webhook(&env, gw, &txn, "150", "spent").await;
    assert!(
        matches!(recorded(&res), EventOutcome::Chargeback { .. }),
        "{res:?}"
    );
    assert_eq!(credit(&env, client).await, d("-30"), "the client owes 30");
    let i = invoice(&env, inv).await;
    assert_eq!(
        (i.status, i.amount_paid),
        (InvoiceStatus::Unpaid, Decimal::ZERO)
    );
    assert_eq!(
        env.notifications("payment.chargeback_credit_shortfall")
            .await,
        1
    );
    assert!(env.notifications("payment.chargeback").await >= 1);
    env.cleanup().await;
}

// ---------------------------------------------------------------------------
// F7: refund webhooks
// ---------------------------------------------------------------------------

/// A partial refund completed synchronously was recorded a second time when
/// the gateway's refund webhook arrived.
#[tokio::test]
async fn synchronous_refund_is_not_recorded_again_by_its_webhook() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let (_client, inv) = open_invoice(&env, "Twice", gw).await;
    let txn = pay(&env, gw, inv, "120").await;
    let refund = env
        .billing
        .refund_payment(t, &staff_refund(&txn, "50"))
        .await
        .unwrap();
    assert_eq!(refund.status, RefundStatus::Succeeded);
    let rid = refund.gateway_refund_id.clone().expect("gateway refund id");

    let res = refund_webhook(&env, gw, &txn, "50", Some(&rid)).await;
    assert_eq!(recorded(&res), &EventOutcome::Duplicate);
    let refunds = payments::transaction_refunds(&env.pool, t, txn.id)
        .await
        .unwrap();
    assert_eq!(refunds.len(), 1);
    assert_eq!(invoice(&env, inv).await.amount_paid, d("70"));
    env.cleanup().await;
}

/// The same refund event delivered concurrently was applied by every delivery.
#[tokio::test]
async fn concurrent_deliveries_of_one_refund_event_apply_once() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let (_client, inv) = open_invoice(&env, "Race", gw).await;
    let txn = pay(&env, gw, inv, "120").await;
    let event = hos_payments::mock::MockEvent::new(
        PaymentEventKind::Refunded,
        txn.gateway_txn_id.clone().unwrap(),
        try_("40"),
    );
    let req = hos_payments::mock::webhook_request(
        GATEWAY_SECRET.as_bytes(),
        &event,
        Utc::now().timestamp(),
    );
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let billing = env.billing.clone();
        let req = req.clone();
        tasks.push(tokio::spawn(async move {
            billing.handle_webhook(t, gw, &req).await
        }));
    }
    let mut applied = 0;
    for task in tasks {
        let res = task.await.unwrap().expect("webhook");
        match recorded(&res) {
            EventOutcome::Refunded { .. } => applied += 1,
            EventOutcome::Duplicate => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(
        payments::transaction_refunds(&env.pool, t, txn.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(invoice(&env, inv).await.amount_paid, d("80"));
    env.cleanup().await;
}

// ---------------------------------------------------------------------------
// F5: mock providers
// ---------------------------------------------------------------------------

/// An active mock gateway let anyone pay any invoice for free; the admin
/// picker hid it but saving / resolving accepted it.
#[tokio::test]
async fn mock_integrations_need_an_explicit_opt_in() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await; // saved with the opt-in of the test engine
    let (_client, inv) = open_invoice(&env, "Mock", gw).await;

    let registry = IntegrationRegistry::new(env.pool.clone(), env.billing.secret_box().clone())
        .with_ttl(Duration::from_secs(3600))
        .with_mock_integrations(false);
    let mut config = BillingConfig::new("https://billing.test");
    config.allow_mock_integrations = false;
    let strict = Billing::with_parts(
        env.pool.clone(),
        registry.clone(),
        Arc::new(ManualClock::new(Utc::now())),
        config,
        Arc::new(NoFraudCheck),
    );
    assert!(!strict.registry().mock_integrations_allowed());

    let err = registry.get(t, gw).await.expect_err("mock refused");
    assert!(
        matches!(&err, CoreError::Validation(m) if m.contains("HOS_ALLOW_MOCK_INTEGRATIONS")),
        "{err}"
    );
    let err = strict
        .start_payment(
            t,
            inv,
            &StartPayment {
                integration_id: Some(gw),
                ..StartPayment::default()
            },
        )
        .await
        .expect_err("no free payments");
    assert!(matches!(err, CoreError::Validation(_)), "{err}");

    for category in [
        IntegrationCategory::Gateway,
        IntegrationCategory::Virt,
        IntegrationCategory::Panel,
        IntegrationCategory::Registrar,
    ] {
        let err = registry
            .save_integration(
                t,
                &SaveIntegration {
                    id: None,
                    category,
                    kind: "mock".into(),
                    name: format!("mock {category}"),
                    config: json!({}),
                    secrets: Some(json!({"webhook_secret": "s"})),
                    is_active: true,
                    sort_order: 0,
                },
            )
            .await
            .expect_err("mock refused");
        assert!(matches!(err, CoreError::Validation(_)), "{category}: {err}");
    }
    // The `log` mailer is not a mock.
    registry
        .save_integration(
            t,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Mail,
                kind: "log".into(),
                name: "log mailer".into(),
                config: json!({}),
                secrets: None,
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("log mailer");

    // Explicit opt-in (demo seed, tests).
    registry.set_mock_integrations_allowed(true);
    registry.get(t, gw).await.expect("allowed");
    env.cleanup().await;
}

// ---------------------------------------------------------------------------
// L5: one-time links in email_log
// ---------------------------------------------------------------------------

/// Password-reset / invitation links were stored in `email_log` in plain text.
#[tokio::test]
async fn email_log_redacts_one_time_links_but_the_sent_message_keeps_them() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let token = format!("SeCrEt{}", suffix());
    let link = format!("https://billing.test/auth/reset?token={token}");
    let mut conn = env.pool.acquire().await.unwrap();
    let id = env
        .billing
        .queue_email(
            &mut conn,
            t,
            &OutgoingEmail {
                template_key: hos_mail::defaults::keys::PASSWORD_RESET.into(),
                to: Mailbox::new(format!("reset-{}@example.com", suffix())),
                locale: Locale::En,
                context: json!({"action_url": link, "expires_in_minutes": 60}),
                client_id: None,
                user_id: None,
                related: None,
                reply_to: None,
            },
        )
        .await
        .unwrap()
        .expect("queued");
    drop(conn);
    let (subject, html, text): (String, Option<String>, Option<String>) = sqlx::query_as(
        "select subject, body_html, body_text from email_log where tenant_id = $1 and id = $2",
    )
    .bind(t)
    .bind(id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    let html = html.unwrap_or_default();
    let text = text.unwrap_or_default();
    for stored in [&subject, &html, &text] {
        assert!(!stored.contains(&token), "token stored: {stored}");
    }
    assert!(text.contains("token=[redacted]"), "{text}");

    // What the send job delivers still has the working link.
    let message = env
        .billing
        .logged_email_message(t, id)
        .await
        .unwrap()
        .expect("message");
    assert!(
        message.text_body.unwrap_or_default().contains(&link),
        "link missing from the sent text"
    );
    assert!(message.html_body.unwrap_or_default().contains(&token));
    assert_eq!(
        env.billing.send_logged_email(t, id, true).await.unwrap(),
        DeliveryOutcome::Sent
    );
    let sealed: Option<Vec<u8>> =
        sqlx::query_scalar("select sealed_body from email_log where id = $1")
            .bind(id)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert!(sealed.is_none(), "the sealed copy is dropped once sent");

    // Mails without one-time links are stored as rendered.
    let mut conn = env.pool.acquire().await.unwrap();
    let plain = env
        .billing
        .queue_email(
            &mut conn,
            t,
            &OutgoingEmail {
                template_key: hos_mail::defaults::keys::WELCOME.into(),
                to: Mailbox::new(format!("w-{}@example.com", suffix())),
                locale: Locale::En,
                context: json!({"action_url": "https://billing.test/client"}),
                client_id: None,
                user_id: None,
                related: None,
                reply_to: None,
            },
        )
        .await
        .unwrap()
        .expect("queued");
    drop(conn);
    let text: Option<String> = sqlx::query_scalar("select body_text from email_log where id = $1")
        .bind(plain)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert!(
        text.unwrap_or_default()
            .contains("https://billing.test/client")
    );
    env.cleanup().await;
}

// ---------------------------------------------------------------------------
// Integration secrets follow their endpoint
// ---------------------------------------------------------------------------

/// Pointing an integration at another API host drops its stored secrets
/// (they must be entered again), so a changed URL cannot carry the API key
/// to a server of someone's choosing; other edits keep them.
#[tokio::test]
async fn stored_secrets_do_not_follow_a_changed_endpoint() {
    let Some(env) = Env::new().await else { return };
    let registry = env.billing.registry();
    let save = |id, base: &str, key: Option<&str>| SaveIntegration {
        id,
        category: IntegrationCategory::Einvoice,
        kind: "nilvera".into(),
        name: "Nilvera".into(),
        config: json!({"environment": "test", "base_url": base}),
        secrets: key.map(|k| json!({ "api_key": k })),
        is_active: false,
        sort_order: 0,
    };
    let row = registry
        .save_integration(
            env.tenant,
            &save(None, "https://apitest.nilvera.com/", Some("real-key")),
        )
        .await
        .expect("created");
    let key = |row: &hos_db::repo::integrations::IntegrationRow| {
        registry.decrypt_secrets(row).unwrap()["api_key"].clone()
    };
    // Same host (path only): the key stays without re-entering it.
    let same = registry
        .save_integration(
            env.tenant,
            &save(Some(row.id), "https://apitest.nilvera.com/v1/", None),
        )
        .await
        .expect("same host");
    assert_eq!(key(&same), json!("real-key"));
    // Another host without the key: refused, nothing changes.
    let err = registry
        .save_integration(
            env.tenant,
            &save(Some(row.id), "https://collector.example/", None),
        )
        .await
        .expect_err("the key must be entered again");
    assert!(err.to_string().contains("api_key"), "{err}");
    let kept = hos_db::repo::integrations::get_integration(&env.pool, env.tenant, row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        kept.config["base_url"],
        json!("https://apitest.nilvera.com/v1/")
    );
    assert_eq!(key(&kept), json!("real-key"));
    // With a key entered for the new host it is saved.
    let moved = registry
        .save_integration(
            env.tenant,
            &save(Some(row.id), "https://collector.example/", Some("new-key")),
        )
        .await
        .expect("new host with its key");
    assert_eq!(key(&moved), json!("new-key"));
    env.cleanup().await;
}
