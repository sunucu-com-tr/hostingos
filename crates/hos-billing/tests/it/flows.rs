//! End-to-end billing flows against the real database with mock providers:
//! cart → order → invoice → gateway webhook → paid → provisioning, plus
//! webhook idempotency, partial payments, coupon limits under concurrency and
//! the daily automation driven by a manual clock.

use crate::common::{Env, d, suffix, try_};
use chrono::Days;
use hos_billing::jobs::PollVmTaskPayload;
use hos_billing::payments::EventOutcome;
use hos_billing::provisioning::{DomainOutcome, ManualProvisioning, ProvisionOutcome, RunContext};
use hos_core::domain::{DomainStatus, InvoiceStatus, OrderStatus, ProductType, ServiceStatus};
use hos_core::ids::{CouponId, ServiceId};
use hos_core::integrations::gateway::PaymentEventKind;
use hos_core::integrations::panel::AccountRef;
use hos_core::money::{Currency, Money};
use hos_db::enums::{DiscountType, LineItemType};
use hos_db::repo::carts::{self, NewCartItem};
use hos_db::repo::catalog::{self, CouponInput};
use hos_db::repo::{domains, invoices, orders, services};
use serde_json::json;

fn paid(r: &hos_billing::WebhookResult) -> bool {
    matches!(
        r.events.first().map(|e| &e.outcome),
        Some(EventOutcome::PaymentRecorded {
            invoice_paid: true,
            ..
        })
    )
}

async fn service(env: &Env, id: ServiceId) -> services::ServiceRow {
    services::get_service(&env.pool, env.tenant, id)
        .await
        .unwrap()
        .unwrap()
}

async fn coupon(env: &Env, value: &str, max_uses: Option<i32>) -> (CouponId, String) {
    let code = format!("C{}", suffix()).to_uppercase();
    let c = catalog::create_coupon(
        &env.pool,
        env.tenant,
        &CouponInput {
            code: code.clone(),
            description: None,
            discount_type: DiscountType::Percent,
            value: d(value),
            currency: None,
            product_ids: Vec::new(),
            recurring: false,
            new_clients_only: false,
            once_per_client: false,
            max_uses,
            starts_at: None,
            expires_at: None,
        },
    )
    .await
    .unwrap();
    (c.id, code)
}

/// Scenario 1 + 6: hosting order paid by webhook → provisioning job →
/// account on the mock panel → active; re-running never opens a second account.
#[tokio::test]
async fn hosting_order_is_paid_and_provisioned_once() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let panel = env.panel().await;
    let product = env
        .product(
            "Starter Hosting",
            ProductType::SharedHosting,
            Some(panel),
            json!({"package": "starter"}),
            "100",
            "0",
        )
        .await;
    let client = env.client("Ayse").await;
    let domain = format!("site-{}.com", suffix());
    let cart = env
        .cart(client, &[Env::hosting_item(product, &domain)])
        .await;

    let out = env.checkout(cart, client, Some(gw)).await;
    assert!(!out.paid);
    assert_eq!(out.quote.total, d("120")); // 100 + KDV %20
    let inv = invoices::get_invoice(&env.pool, t, out.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Unpaid);
    assert_eq!(inv.total, d("120"));
    assert!(
        env.pending_payloads("provision_service").await.is_empty(),
        "on_payment: nothing before payment"
    );

    let res = env.pay(out.invoice_id, gw).await;
    assert!(paid(&res), "{res:?}");
    let inv = invoices::get_invoice(&env.pool, t, out.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Paid);
    let order = orders::get_order(&env.pool, t, out.order_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(order.status, OrderStatus::Accepted);
    let jobs = env.pending_payloads("provision_service").await;
    assert_eq!(jobs.len(), 1);
    let svc_id = out.services[0];
    assert_eq!(jobs[0]["service_id"], json!(svc_id));

    // the job's work
    let o = env
        .billing
        .provision_service(t, svc_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(o, ProvisionOutcome::Activated);
    let svc = service(&env, svc_id).await;
    assert_eq!(svc.status, ServiceStatus::Active);
    assert!(svc.external_id.is_some());
    let username = svc.username.clone().expect("username");
    let password = env
        .billing
        .service_password(t, svc_id, Some(client))
        .await
        .unwrap()
        .unwrap();
    assert!(password.expose_str().len() >= 8);

    // running the job again is a no-op
    let again = env
        .billing
        .provision_service(t, svc_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(again, ProvisionOutcome::AlreadyActive);
    let mails = env
        .count("select count(*) from email_log where tenant_id = $1 and template_key in ('order_confirmation', 'invoice_paid', 'service_activated')")
        .await;
    assert_eq!(mails, 3, "one email each, the re-run sends nothing");
    // a crash after the panel call (service still provisioning) adopts the account
    sqlx::query("update services set status = 'provisioning', external_id = null where id = $1")
        .bind(svc_id)
        .execute(&env.pool)
        .await
        .unwrap();
    let adopted = env
        .billing
        .provision_service(t, svc_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(adopted, ProvisionOutcome::Activated);
    let p = env.billing.registry().panel(t, panel).await.unwrap();
    let accounts = p.list_accounts().await.unwrap();
    assert_eq!(accounts.len(), 1, "exactly one account on the panel");
    let info = p.get_account(&AccountRef::new(username)).await.unwrap();
    assert_eq!(info.account.domain.as_deref(), Some(domain.as_str()));

    let events: Vec<String> = services::service_events(&env.pool, t, svc_id, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event)
        .collect();
    assert!(events.contains(&"provisioning_started".to_owned()));
    assert!(events.contains(&"account_created".to_owned()));
    env.cleanup().await;
}

/// Scenario 2 (VPS): IP from the pool, asynchronous creation, task polling → active.
#[tokio::test]
async fn vps_order_gets_pool_ip_and_activates_after_polling() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(true).await;
    env.ip_pool(virt).await;
    let product = env
        .product(
            "VPS 2",
            ProductType::Vps,
            Some(virt),
            json!({"template": "ubuntu-24.04", "cpu_cores": 2, "memory_mb": 2048, "disk_gb": 40}),
            "250",
            "50",
        )
        .await;
    let client = env.client("Mehmet").await;
    let mut item = NewCartItem::product(product, hos_core::money::BillingCycle::Monthly);
    item.hostname = Some(format!("vps-{}.example.com", suffix()));
    let cart = env.cart(client, &[item]).await;
    let out = env.checkout(cart, client, Some(gw)).await;
    assert_eq!(out.quote.total, d("360")); // (250 + 50 setup) × 1.2
    assert!(paid(&env.pay(out.invoice_id, gw).await));
    let svc_id = out.services[0];

    let o = env
        .billing
        .provision_service(t, svc_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(o, ProvisionOutcome::InProgress);
    let svc = service(&env, svc_id).await;
    assert_eq!(svc.status, ServiceStatus::Provisioning);
    assert_eq!(svc.dedicated_ip, Some("198.51.100.2".parse().unwrap()));
    let vm = services::vm_for_service(&env.pool, t, svc_id)
        .await
        .unwrap()
        .expect("vm row");
    assert_eq!(vm.primary_ipv4, svc.dedicated_ip);

    let mut done = None;
    for _ in 0..5 {
        let payloads = env.pending_payloads("poll_vm_task").await;
        let p: PollVmTaskPayload =
            serde_json::from_value(payloads.last().expect("poll job").clone()).unwrap();
        if let Some(o) = env
            .billing
            .poll_vm_task(t, &p, RunContext::default())
            .await
            .unwrap()
        {
            done = Some(o);
            break;
        }
    }
    assert_eq!(done, Some(ProvisionOutcome::Activated));
    assert_eq!(service(&env, svc_id).await.status, ServiceStatus::Active);
    let vm = services::vm_for_service(&env.pool, t, svc_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(vm.status, hos_core::integrations::virt::VmStatus::Running);
    let assigned = env
        .count("select count(*) from ip_addresses where tenant_id = $1 and status = 'assigned'")
        .await;
    assert_eq!(assigned, 1);
    // client-area wrapper: power action on an active VM
    env.billing
        .vm_power(
            t,
            svc_id,
            Some(client),
            hos_core::integrations::virt::PowerAction::Reboot,
        )
        .await
        .unwrap();
    env.cleanup().await;
}

/// Scenario 2 (domain): registration through the mock registrar, expiry
/// notice, renewal invoice and renewal at the registrar.
#[tokio::test]
async fn domain_order_registers_and_renews() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let registrar = env.registrar().await;
    env.tld_com(Some(registrar)).await;
    let client = env.client("Zeynep").await;
    let name = format!("brand-{}.com", suffix());
    let cart = env
        .cart(
            client,
            &[NewCartItem::domain(LineItemType::DomainRegister, &name, 1)],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    assert_eq!(out.quote.total, d("360"));
    assert!(paid(&env.pay(out.invoice_id, gw).await));
    let domain_id = out.domains[0];
    assert_eq!(env.pending_payloads("register_domain").await.len(), 1);

    let o = env
        .billing
        .register_domain(t, domain_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(o, DomainOutcome::Completed);
    assert_eq!(
        env.billing
            .register_domain(t, domain_id, RunContext::default())
            .await
            .unwrap(),
        DomainOutcome::AlreadyDone
    );
    let dom = domains::get_domain(&env.pool, t, domain_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dom.status, DomainStatus::Active);
    let expires = dom.expires_at.expect("expiry");
    assert!(dom.external_id.is_some());
    assert_eq!(dom.next_due_date, Some(expires.date_naive()));

    // 30 days before expiry: notice; 14 days before: renewal invoice
    env.clock.set_date(expires.date_naive() - Days::new(30));
    let r = env.billing.run_daily(t, None).await.unwrap();
    assert_eq!(r.domain_notices_sent, 1);
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .domain_notices_sent,
        0,
        "once"
    );
    env.clock.set_date(expires.date_naive() - Days::new(14));
    let r = env.billing.run_daily(t, None).await.unwrap();
    assert_eq!(r.invoices_created, 1);
    let renewal: hos_core::ids::InvoiceId = sqlx::query_scalar(
        "select i.id from invoices i join invoice_items ii on ii.invoice_id = i.id
         where i.tenant_id = $1 and ii.domain_id = $2 and i.status = 'unpaid'",
    )
    .bind(t)
    .bind(domain_id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(paid(&env.pay(renewal, gw).await));
    let jobs = env.pending_payloads("renew_domain").await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        env.billing
            .renew_domain(t, domain_id, Some(1), RunContext::default())
            .await
            .unwrap(),
        DomainOutcome::Completed
    );
    let dom = domains::get_domain(&env.pool, t, domain_id)
        .await
        .unwrap()
        .unwrap();
    let new_exp = dom.expires_at.unwrap();
    assert!((new_exp - expires).num_days() >= 365);
    assert_eq!(dom.next_due_date, Some(new_exp.date_naive()));
    // client-area wrapper: nameservers through the registrar
    env.billing
        .domain_set_nameservers(
            t,
            domain_id,
            Some(client),
            &["ns1.example.net".into(), "ns2.example.net".into()],
        )
        .await
        .unwrap();
    env.cleanup().await;
}

/// Scenario 3: a replayed webhook records one payment; an underpayment does
/// not mark the invoice paid; a wrong currency goes to credit.
#[tokio::test]
async fn webhook_replay_partial_payment_and_currency_mismatch() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
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
    let client = env.client("Ali").await;
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(
                product,
                hos_core::money::BillingCycle::Monthly,
            )],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    assert_eq!(out.quote.total, d("120"));

    // underpayment
    let r1 = env.start_payment(out.invoice_id, gw).await;
    let res = env
        .webhook(gw, PaymentEventKind::Paid, &r1, try_("50"))
        .await;
    assert!(matches!(
        res.events[0].outcome,
        EventOutcome::PaymentRecorded {
            invoice_paid: false,
            ..
        }
    ));
    let inv = invoices::get_invoice(&env.pool, t, out.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Unpaid);
    assert_eq!(inv.balance, d("70"));
    assert!(env.pending_payloads("provision_service").await.is_empty());
    // replay of the same webhook
    let dup = env
        .webhook(gw, PaymentEventKind::Paid, &r1, try_("50"))
        .await;
    assert_eq!(dup.events[0].outcome, EventOutcome::Duplicate);
    // the rest through a new attempt
    let r2 = env.start_payment(out.invoice_id, gw).await;
    assert!(paid(
        &env.webhook(gw, PaymentEventKind::Paid, &r2, try_("70"))
            .await
    ));
    let again = env
        .webhook(gw, PaymentEventKind::Paid, &r2, try_("70"))
        .await;
    assert_eq!(again.events[0].outcome, EventOutcome::Duplicate);
    let payments = env
        .count("select count(*) from transactions where tenant_id = $1 and kind = 'payment' and status = 'succeeded'")
        .await;
    assert_eq!(payments, 2);
    let inv = invoices::get_invoice(&env.pool, t, out.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (inv.status, inv.amount_paid),
        (InvoiceStatus::Paid, d("120"))
    );
    assert_eq!(env.pending_payloads("provision_service").await.len(), 1);

    // currency mismatch: credited, invoice untouched
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(
                product,
                hos_core::money::BillingCycle::Monthly,
            )],
        )
        .await;
    let out2 = env.checkout(cart, client, Some(gw)).await;
    let r3 = env.start_payment(out2.invoice_id, gw).await;
    let res = env
        .webhook(
            gw,
            PaymentEventKind::Paid,
            &r3,
            Money::new(d("120"), Currency::USD),
        )
        .await;
    assert_eq!(
        res.events[0].outcome,
        EventOutcome::CurrencyMismatch { credited: d("120") }
    );
    let inv2 = invoices::get_invoice(&env.pool, t, out2.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv2.status, InvoiceStatus::Unpaid);
    assert_eq!(
        hos_db::repo::payments::credit_balance(&env.pool, t, client, Currency::USD)
            .await
            .unwrap(),
        d("120")
    );
    // a pending event moves the invoice to payment_pending, a failure back
    let r4 = env.start_payment(out2.invoice_id, gw).await;
    env.webhook(gw, PaymentEventKind::Pending, &r4, try_("120"))
        .await;
    let inv2 = invoices::get_invoice(&env.pool, t, out2.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv2.status, InvoiceStatus::PaymentPending);
    env.webhook(gw, PaymentEventKind::Failed, &r4, try_("120"))
        .await;
    let inv2 = invoices::get_invoice(&env.pool, t, out2.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv2.status, InvoiceStatus::Unpaid);
    env.cleanup().await;
}

/// Scenario 7: a coupon with `max_uses = 2` is redeemed exactly twice by
/// four concurrent checkouts.
#[tokio::test]
async fn coupon_limit_holds_under_concurrent_checkouts() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let product = env
        .product("Widget", ProductType::Other, None, json!({}), "100", "0")
        .await;
    let (coupon_id, code) = coupon(&env, "10", Some(2)).await;
    let mut jobs = Vec::new();
    for i in 0..4 {
        let client = env.client(&format!("C{i}")).await;
        let cart = env
            .cart(
                client,
                &[NewCartItem::product(
                    product,
                    hos_core::money::BillingCycle::Monthly,
                )],
            )
            .await;
        carts::set_cart_coupon(&env.pool, t, cart, Some((coupon_id, &code)))
            .await
            .unwrap();
        let billing = env.billing.clone();
        jobs.push(tokio::spawn(async move {
            billing
                .checkout(
                    t,
                    &hos_billing::CheckoutRequest {
                        cart_id: cart,
                        client_id: client,
                        user_id: None,
                        payment_integration_id: None,
                        ip: None,
                        user_agent: None,
                        notes: None,
                    },
                )
                .await
        }));
    }
    let mut ok = 0;
    for j in jobs {
        match j.await.unwrap() {
            Ok(o) => {
                ok += 1;
                assert_eq!(o.quote.discount_total, d("10"));
            }
            Err(e) => assert!(
                matches!(
                    e,
                    hos_core::CoreError::Conflict(_) | hos_core::CoreError::Validation(_)
                ),
                "{e}"
            ),
        }
    }
    assert_eq!(ok, 2);
    let uses: i32 = sqlx::query_scalar("select uses_count from coupons where id = $1")
        .bind(coupon_id)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(uses, 2);
    assert_eq!(
        env.count("select count(*) from orders where tenant_id = $1")
            .await,
        2
    );
    env.cleanup().await;
}

/// A 100 % coupon gives a zero invoice: paid at checkout, provisioning queued;
/// products without a module land in the manual queue.
#[tokio::test]
async fn zero_total_order_is_paid_and_manual_setup_completes() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let product = env
        .product(
            "Setup Service",
            ProductType::Other,
            None,
            json!({}),
            "100",
            "0",
        )
        .await;
    let (coupon_id, code) = coupon(&env, "100", None).await;
    let client = env.client("Free").await;
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(
                product,
                hos_core::money::BillingCycle::Monthly,
            )],
        )
        .await;
    carts::set_cart_coupon(&env.pool, t, cart, Some((coupon_id, &code)))
        .await
        .unwrap();
    let out = env.checkout(cart, client, None).await;
    assert!(out.paid);
    assert_eq!(out.quote.total, d("0"));
    assert_eq!(env.pending_payloads("provision_service").await.len(), 1);
    let svc = out.services[0];
    assert_eq!(
        env.billing
            .provision_service(t, svc, RunContext::default())
            .await
            .unwrap(),
        ProvisionOutcome::Manual
    );
    assert_eq!(service(&env, svc).await.status, ServiceStatus::Provisioning);
    env.billing
        .complete_manual_provisioning(
            t,
            svc,
            &ManualProvisioning {
                username: Some("manual".into()),
                note: Some("done by hand".into()),
                ..ManualProvisioning::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(service(&env, svc).await.status, ServiceStatus::Active);
    env.cleanup().await;
}

async fn active_hosting(
    env: &Env,
) -> (
    hos_core::ids::IntegrationId,
    hos_core::ids::IntegrationId,
    ServiceId,
) {
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
    let client = env.client("Renew").await;
    let cart = env
        .cart(
            client,
            &[Env::hosting_item(
                product,
                &format!("renew-{}.com", suffix()),
            )],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    assert!(paid(&env.pay(out.invoice_id, gw).await));
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

async fn open_invoice_of(env: &Env, svc: ServiceId) -> invoices::InvoiceRow {
    hos_db::repo::billing_support::open_invoices_for_service(&env.pool, env.tenant, svc)
        .await
        .unwrap()
        .pop()
        .expect("open invoice")
}

/// Scenario 4: renewal invoice → reminders → late fee → suspension →
/// termination, with the clock moved day by day.
#[tokio::test]
async fn automation_renews_suspends_and_terminates() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    env.setting(
        "billing.late_fee",
        json!({"type": "fixed", "amount": "25", "after_days": 2}),
    )
    .await;
    let (_gw, panel, svc_id) = active_hosting(&env).await;
    let due = service(&env, svc_id).await.next_due_date.expect("due date");

    env.clock.set_date(due - Days::new(8));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .invoices_created,
        0
    );
    env.clock.set_date(due - Days::new(7));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .invoices_created,
        1
    );
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .invoices_created,
        0,
        "idempotent"
    );
    let renewal = open_invoice_of(&env, svc_id).await;
    assert_eq!(renewal.due_date, due);
    assert_eq!(renewal.total, d("120"));

    env.clock.set_date(due - Days::new(3));
    assert_eq!(
        env.billing.run_daily(t, None).await.unwrap().reminders_sent,
        1
    );
    assert_eq!(
        env.billing.run_daily(t, None).await.unwrap().reminders_sent,
        0
    );
    env.clock.set_date(due + Days::new(1));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .overdue_notices_sent,
        1
    );
    env.clock.set_date(due + Days::new(3));
    let r = env.billing.run_daily(t, None).await.unwrap();
    assert_eq!((r.overdue_notices_sent, r.late_fees_added), (1, 1));
    assert_eq!(open_invoice_of(&env, svc_id).await.total, d("145"));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .late_fees_added,
        0
    );

    env.clock.set_date(due + Days::new(5));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .suspensions_queued,
        0
    );
    env.clock.set_date(due + Days::new(6));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .suspensions_queued,
        1
    );
    let jobs = env.pending_payloads("suspend_service").await;
    assert_eq!(jobs[0]["reason"], json!("overdue"));
    env.billing
        .suspend_service(t, svc_id, Some("overdue"), RunContext::default())
        .await
        .unwrap();
    env.complete_jobs("suspend_service").await;
    let svc = service(&env, svc_id).await;
    assert_eq!(svc.status, ServiceStatus::Suspended);
    let p = env.billing.registry().panel(t, panel).await.unwrap();
    let info = p
        .get_account(&AccountRef::new(svc.username.clone().unwrap()))
        .await
        .unwrap();
    assert!(info.suspended);

    env.clock.set_date(due + Days::new(30));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .terminations_queued,
        0
    );
    env.clock.set_date(due + Days::new(31));
    assert_eq!(
        env.billing
            .run_daily(t, None)
            .await
            .unwrap()
            .terminations_queued,
        1
    );
    env.billing
        .terminate_service(t, svc_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(
        service(&env, svc_id).await.status,
        ServiceStatus::Terminated
    );
    assert!(p.list_accounts().await.unwrap().is_empty());
    let cancelled = invoices::get_invoice(&env.pool, t, renewal.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, InvoiceStatus::Cancelled);
    env.cleanup().await;
}

/// Paying an overdue renewal advances the due date (anchor day kept) and
/// queues reactivation of an overdue-suspended service.
#[tokio::test]
async fn paid_renewal_advances_due_date_and_unsuspends() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let (gw, _panel, svc_id) = active_hosting(&env).await;
    let svc = service(&env, svc_id).await;
    let due = svc.next_due_date.unwrap();
    env.clock.set_date(due - Days::new(7));
    env.billing.run_daily(t, None).await.unwrap();
    env.clock.set_date(due + Days::new(6));
    env.billing.run_daily(t, None).await.unwrap();
    env.billing
        .suspend_service(t, svc_id, Some("overdue"), RunContext::default())
        .await
        .unwrap();
    let renewal = open_invoice_of(&env, svc_id).await;
    assert!(paid(&env.pay(renewal.id, gw).await));
    let svc = service(&env, svc_id).await;
    let expected = svc
        .billing_cycle
        .next_due_anchored(due, u32::try_from(svc.anchor_day.unwrap()).unwrap())
        .unwrap();
    assert_eq!(svc.next_due_date, Some(expected));
    assert_eq!(env.pending_payloads("unsuspend_service").await.len(), 1);
    env.billing
        .unsuspend_service(t, svc_id, RunContext::default())
        .await
        .unwrap();
    assert_eq!(service(&env, svc_id).await.status, ServiceStatus::Active);
    env.cleanup().await;
}

/// Upgrade mid-period: prorated invoice; once paid the plan changes at the panel.
#[tokio::test]
async fn upgrade_is_prorated_and_applied_after_payment() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let (gw, panel, svc_id) = active_hosting(&env).await;
    let bigger = env
        .product(
            "Business Hosting",
            ProductType::SharedHosting,
            Some(panel),
            json!({"package": "business"}),
            "200",
            "0",
        )
        .await;
    let svc = service(&env, svc_id).await;
    sqlx::query(
        "update products set upgrade_product_ids = array[$2] where tenant_id = $1 and id = $3",
    )
    .bind(t)
    .bind(bigger)
    .bind(svc.product_id)
    .execute(&env.pool)
    .await
    .unwrap();
    let q = env
        .billing
        .quote_plan_change(t, svc_id, Some(svc.client_id), bigger)
        .await
        .unwrap();
    assert!(q.proration.amount > d("0") && q.proration.amount <= d("100"));
    let order = env
        .billing
        .order_plan_change(t, svc_id, Some(svc.client_id), bigger)
        .await
        .unwrap();
    let invoice = order.invoice_id.expect("upgrade invoice");
    assert!(paid(&env.pay(invoice, gw).await));
    let jobs = env.pending_payloads("change_package").await;
    assert_eq!(jobs.len(), 1);
    env.billing
        .apply_plan_change(t, order.upgrade_id, RunContext::default())
        .await
        .unwrap();
    let svc = service(&env, svc_id).await;
    assert_eq!(svc.product_id, bigger);
    assert_eq!(svc.recurring_amount, d("200"));
    let p = env.billing.registry().panel(t, panel).await.unwrap();
    let info = p
        .get_account(&AccountRef::new(svc.username.unwrap()))
        .await
        .unwrap();
    assert_eq!(info.package.as_deref(), Some("business"));
    env.cleanup().await;
}
