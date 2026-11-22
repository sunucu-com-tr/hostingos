//! Invoice numbering, payments and orders against the real database.

use std::collections::BTreeSet;

use crate::common::{self, TestTenant, d, today};
use hos_core::domain::{InvoiceStatus, OrderStatus, ProductType};
use hos_core::ids::{ClientId, ProductId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::LineItemType;
use hos_db::repo::{catalog, invoices, orders, payments};
use hos_db::{PgPool, Postgres};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::Acquire;

fn invoice(
    client_id: ClientId,
    amount: &str,
    taxes: Vec<invoices::TaxLineInput>,
) -> invoices::NewInvoice {
    invoices::NewInvoice {
        client_id,
        currency: Currency::TRY,
        issue_date: today(),
        due_date: today() + chrono::Days::new(7),
        issue: true,
        payment_integration_id: None,
        notes: None,
        billing_snapshot: json!({"name": "Test client"}),
        items: vec![invoices::NewInvoiceItem::manual(
            "Web Hosting Pro (1 ay)",
            d(amount),
        )],
        taxes,
    }
}

async fn kdv(pool: &PgPool, t: &TestTenant) -> Vec<invoices::TaxLineInput> {
    catalog::applicable_tax_rules(pool, t.id, "TR", None)
        .await
        .unwrap()
        .iter()
        .map(invoices::TaxLineInput::from)
        .collect()
}

#[tokio::test]
async fn invoice_numbers_are_gapless_under_concurrency() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let client = t.client("Numara").await;

    const N: i64 = 24;
    let mut tasks = Vec::new();
    for _ in 0..N {
        let pool = pool.clone();
        let tenant = t.id;
        tasks.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.unwrap();
            invoices::create_invoice(&mut conn, tenant, &invoice(client, "10", vec![]))
                .await
                .unwrap()
        }));
    }
    let mut seqs = BTreeSet::new();
    let mut numbers = BTreeSet::new();
    for task in tasks {
        let inv = task.await.unwrap();
        seqs.insert(inv.seq.unwrap());
        numbers.insert(inv.number.unwrap());
    }
    assert_eq!(
        seqs,
        (1..=N).collect::<BTreeSet<_>>(),
        "sequence has gaps or duplicates"
    );
    assert_eq!(numbers.len() as i64, N);
    // `<PREFIX><year><9 digits>`: "test-…" → prefix "TES".
    let year = chrono::Datelike::year(&today());
    assert!(
        numbers.contains(&format!("TES{year}{:09}", 1)),
        "{numbers:?}"
    );

    // A rolled-back allocation does not burn a number.
    {
        let mut conn = pool.acquire().await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        let (_, seq) = invoices::allocate_invoice_number(&mut tx, t.id, today())
            .await
            .unwrap();
        assert_eq!(seq, N + 1);
        tx.rollback().await.unwrap();
    }
    let mut conn = pool.acquire().await.unwrap();
    let next = invoices::create_invoice(&mut conn, t.id, &invoice(client, "10", vec![]))
        .await
        .unwrap();
    assert_eq!(next.seq, Some(N + 1));

    // A failed invoice insert (unknown client) rolls its number back too.
    assert!(
        invoices::create_invoice(&mut conn, t.id, &invoice(ClientId::new(), "10", vec![]))
            .await
            .is_err()
    );
    let after = invoices::create_invoice(&mut conn, t.id, &invoice(client, "10", vec![]))
        .await
        .unwrap();
    assert_eq!(after.seq, Some(N + 2));

    // Drafts get no number until issued.
    let mut draft = invoice(client, "10", vec![]);
    draft.issue = false;
    let draft = invoices::create_invoice(&mut conn, t.id, &draft)
        .await
        .unwrap();
    assert_eq!(
        (draft.status, draft.number.as_deref()),
        (InvoiceStatus::Draft, None)
    );
    let issued = invoices::issue_invoice(&mut conn, t.id, draft.id)
        .await
        .unwrap();
    assert_eq!(issued.seq, Some(N + 3));
    drop(conn);
    t.cleanup().await;
}

#[tokio::test]
async fn invoice_totals_include_kdv() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let client = t.client("Vergi").await;
    let taxes = kdv(&pool, &t).await;
    assert_eq!(taxes.len(), 1);
    let mut conn = pool.acquire().await.unwrap();
    let inv = invoices::create_invoice(&mut conn, t.id, &invoice(client, "149.90", taxes))
        .await
        .unwrap();
    assert_eq!(inv.subtotal, d("149.90"));
    assert_eq!(inv.tax_total, d("29.98"));
    assert_eq!(inv.total, d("179.88"));
    assert_eq!(inv.balance, d("179.88"));
    let detail = invoices::invoice_items(&pool, t.id, inv.id).await.unwrap();
    assert_eq!(detail.len(), 1);
    drop(conn);
    t.cleanup().await;
}

#[tokio::test]
async fn payments_are_idempotent() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let client = t.client("Odeme").await;
    let gateway = t.gateway().await;
    let taxes = kdv(&pool, &t).await;
    let mut conn = pool.acquire().await.unwrap();
    let inv = invoices::create_invoice(&mut conn, t.id, &invoice(client, "100", taxes.clone()))
        .await
        .unwrap();
    assert_eq!(inv.total, d("120"));

    let pay = |txn: &str, amount: &str, invoice_id| {
        payments::NewPayment::gateway(
            client,
            invoice_id,
            gateway,
            "stripe",
            txn,
            d(amount),
            Currency::TRY,
        )
    };

    // Partial payment, then the webhook is replayed.
    let first = payments::record_payment(&mut conn, t.id, &pay("pi_1", "50", inv.id))
        .await
        .unwrap();
    assert!(!first.duplicate && !first.invoice_paid);
    assert_eq!(first.invoice.as_ref().unwrap().balance, d("70"));
    let replay = payments::record_payment(&mut conn, t.id, &pay("pi_1", "50", inv.id))
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.transaction.id, first.transaction.id);
    assert_eq!(replay.invoice.as_ref().unwrap().amount_paid, d("50"));

    // The rest pays the invoice.
    let second = payments::record_payment(&mut conn, t.id, &pay("pi_2", "70", inv.id))
        .await
        .unwrap();
    assert!(second.invoice_paid);
    let paid = second.invoice.unwrap();
    assert_eq!(
        (paid.status, paid.balance),
        (InvoiceStatus::Paid, Decimal::ZERO)
    );
    assert!(paid.paid_at.is_some());
    assert_eq!(
        payments::invoice_transactions(&pool, t.id, inv.id)
            .await
            .unwrap()
            .len(),
        2
    );

    // Concurrent deliveries of the same event: exactly one is applied.
    let inv2 = invoices::create_invoice(&mut conn, t.id, &invoice(client, "100", taxes))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let p = pay("pi_concurrent", "120", inv2.id);
        let tenant = t.id;
        tasks.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.unwrap();
            payments::record_payment(&mut conn, tenant, &p)
                .await
                .unwrap()
        }));
    }
    let mut applied = 0;
    for task in tasks {
        if !task.await.unwrap().duplicate {
            applied += 1;
        }
    }
    assert_eq!(applied, 1);
    let inv2 = invoices::get_invoice(&pool, t.id, inv2.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (inv2.status, inv2.amount_paid),
        (InvoiceStatus::Paid, d("120"))
    );
    assert_eq!(
        payments::invoice_transactions(&pool, t.id, inv2.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // Overpayment of an already paid invoice goes to client credit.
    let over = payments::record_payment(&mut conn, t.id, &pay("pi_3", "25", inv2.id))
        .await
        .unwrap();
    assert_eq!(over.credited, d("25"));
    assert_eq!(
        payments::credit_balance(&pool, t.id, client, Currency::TRY)
            .await
            .unwrap(),
        d("25")
    );

    // Mismatched currency is rejected and leaves no transaction behind.
    let mut wrong = pay("pi_4", "10", inv.id);
    wrong.currency = Currency::USD;
    assert!(
        payments::record_payment(&mut conn, t.id, &wrong)
            .await
            .is_err()
    );
    assert!(
        payments::transaction_by_gateway_id(&pool, t.id, Some(gateway), "pi_4")
            .await
            .unwrap()
            .is_none()
    );
    drop(conn);
    t.cleanup().await;
}

/// A refund first takes back the overpayment credit its payment created and
/// lowers the invoice only by the part applied to it; spent credit leaves a
/// reported shortfall (negative balance). Gateway refund ids are unique per
/// tenant + integration.
#[tokio::test]
async fn refunds_reverse_payment_credit_and_gateway_ids_are_unique() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let client = t.client("Iade").await;
    let gateway = t.gateway().await;
    let taxes = kdv(&pool, &t).await;
    let mut conn = pool.acquire().await.unwrap();
    let inv = invoices::create_invoice(&mut conn, t.id, &invoice(client, "100", taxes.clone()))
        .await
        .unwrap();
    let paid = payments::record_payment(
        &mut conn,
        t.id,
        &payments::NewPayment::gateway(
            client,
            inv.id,
            gateway,
            "stripe",
            "pi_over",
            d("150"),
            Currency::TRY,
        ),
    )
    .await
    .unwrap();
    assert_eq!(paid.credited, d("30"));

    // Staff refund of 40: 30 from the credit, 10 from the invoice.
    let r = payments::create_refund(
        &mut conn,
        t.id,
        paid.transaction.id,
        d("40"),
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let s = payments::settle_refund(&mut conn, t.id, r.id, true, Some("re_a"), None)
        .await
        .unwrap();
    assert_eq!(
        (s.split.from_credit, s.split.from_invoice, s.shortfall),
        (d("30"), d("10"), Decimal::ZERO)
    );
    assert_eq!(
        payments::credit_balance(&pool, t.id, client, Currency::TRY)
            .await
            .unwrap(),
        Decimal::ZERO
    );
    let after = invoices::get_invoice(&pool, t.id, inv.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (after.amount_paid, after.status),
        (d("110"), InvoiceStatus::PartiallyRefunded)
    );

    // The same gateway refund id cannot be recorded twice.
    let dup = payments::create_gateway_refund(
        &mut conn,
        t.id,
        paid.transaction.id,
        d("5"),
        None,
        Some("re_a"),
    )
    .await;
    assert!(
        matches!(dup, Err(hos_core::CoreError::Conflict(_))),
        "{dup:?}"
    );
    assert_eq!(
        payments::refund_by_gateway_id(&pool, t.id, gateway, "re_a")
            .await
            .unwrap()
            .map(|r| r.id),
        Some(r.id)
    );

    // Second payment with 30 overpayment, credit spent: staff refunds are
    // refused, a gateway refund is recorded with a shortfall.
    let inv2 = invoices::create_invoice(&mut conn, t.id, &invoice(client, "100", taxes.clone()))
        .await
        .unwrap();
    let paid2 = payments::record_payment(
        &mut conn,
        t.id,
        &payments::NewPayment::gateway(
            client,
            inv2.id,
            gateway,
            "stripe",
            "pi_over2",
            d("150"),
            Currency::TRY,
        ),
    )
    .await
    .unwrap();
    let inv3 = invoices::create_invoice(&mut conn, t.id, &invoice(client, "25", vec![]))
        .await
        .unwrap();
    let (inv3, used) = payments::apply_credit_to_invoice(&mut conn, t.id, inv3.id, None, None)
        .await
        .unwrap();
    assert_eq!(used, d("25"));
    // The ledger line is written in the client's language (default `tr`).
    let line: String = sqlx::query_scalar(
        "select description from credits where tenant_id = $1 and invoice_id = $2 and amount < 0",
    )
    .bind(t.id)
    .bind(inv3.id)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert!(line.ends_with("numaralı faturaya uygulandı"), "{line}");
    assert!(matches!(
        payments::create_refund(
            &mut conn,
            t.id,
            paid2.transaction.id,
            d("150"),
            None,
            false,
            None
        )
        .await,
        Err(hos_core::CoreError::Validation(_))
    ));
    let gw_refund = payments::create_gateway_refund(
        &mut conn,
        t.id,
        paid2.transaction.id,
        d("150"),
        Some("dashboard"),
        Some("re_b"),
    )
    .await
    .unwrap();
    let s = payments::settle_refund(&mut conn, t.id, gw_refund.id, true, Some("re_b"), None)
        .await
        .unwrap();
    assert_eq!((s.credit_reversed, s.shortfall), (d("30"), d("25")));
    assert_eq!(
        payments::credit_balance(&pool, t.id, client, Currency::TRY)
            .await
            .unwrap(),
        d("-25")
    );
    let after = invoices::get_invoice(&pool, t.id, inv2.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (after.amount_paid, after.status),
        (Decimal::ZERO, InvoiceStatus::Refunded)
    );
    drop(conn);
    t.cleanup().await;
}

async fn product(pool: &PgPool, t: &TestTenant) -> ProductId {
    let input = catalog::ProductInput::new(
        "hosting-pro",
        "Hosting Pro",
        ProductType::SharedHosting,
        None,
    );
    let p = catalog::create_product(pool, t.id, &input).await.unwrap();
    catalog::set_product_price(
        pool,
        t.id,
        p.id,
        Currency::TRY,
        BillingCycle::Annual,
        d("1699"),
        Decimal::ZERO,
    )
    .await
    .unwrap();
    p.id
}

#[tokio::test]
async fn orders_are_created_atomically() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let client = t.client("Siparis").await;
    let product_id = product(&pool, &t).await;

    let mut domain = orders::NewOrderItem::domain(
        LineItemType::DomainRegister,
        "ornek-firma.com.tr",
        1,
        "ornek-firma.com.tr (1 yıl)",
        d("249"),
    );
    domain.parent_index = Some(0);
    let new = orders::NewOrder {
        client_id: client,
        user_id: None,
        currency: Currency::TRY,
        coupon_id: None,
        coupon_code: None,
        payment_integration_id: None,
        tax_total: d("389.60"),
        ip: Some("203.0.113.7".parse().unwrap()),
        user_agent: Some("tests".into()),
        notes: None,
        cart_id: None,
        items: vec![
            orders::NewOrderItem::product(
                product_id,
                BillingCycle::Annual,
                "Hosting Pro (yıllık)",
                d("1699"),
            ),
            domain,
        ],
    };
    let mut conn = pool.acquire().await.unwrap();
    let (order, items) = orders::create_order(&mut conn, t.id, &new).await.unwrap();
    assert_eq!(order.status, OrderStatus::Pending);
    assert_eq!(order.subtotal, d("1948"));
    assert_eq!(order.total, d("2337.60"));
    assert_eq!(order.order_number.len(), 10);
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].parent_item_id, Some(items[0].id));
    let stored = orders::order_items(&pool, t.id, order.id).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(
        orders::order_by_number(&pool, t.id, &order.order_number)
            .await
            .unwrap()
            .map(|o| o.id),
        Some(order.id)
    );

    // A failing line (unknown product) rolls the whole order back.
    let mut bad = new.clone();
    bad.items[1] =
        orders::NewOrderItem::product(ProductId::new(), BillingCycle::Annual, "ghost", d("1"));
    bad.items[1].parent_index = None;
    assert!(orders::create_order(&mut conn, t.id, &bad).await.is_err());
    let count = |pool: PgPool, tenant| async move {
        let n: i64 =
            sqlx::query_scalar::<Postgres, i64>("select count(*) from orders where tenant_id = $1")
                .bind(tenant)
                .fetch_one(&pool)
                .await
                .unwrap();
        n
    };
    assert_eq!(count(pool.clone(), t.id).await, 1);

    // Invalid parent reference is rejected before touching the database.
    let mut bad = new.clone();
    bad.items[0].parent_index = Some(1);
    assert!(matches!(
        orders::create_order(&mut conn, t.id, &bad).await,
        Err(hos_core::CoreError::Validation(_))
    ));

    // Inside an outer transaction the order is part of the caller's unit of work.
    {
        let mut tx = conn.begin().await.unwrap();
        orders::create_order(&mut tx, t.id, &new).await.unwrap();
        tx.rollback().await.unwrap();
    }
    assert_eq!(count(pool.clone(), t.id).await, 1);

    // Status transitions are guarded by the expected current status.
    assert!(
        orders::transition_order(
            &pool,
            t.id,
            order.id,
            OrderStatus::Pending,
            OrderStatus::Accepted
        )
        .await
        .unwrap()
    );
    assert!(
        !orders::transition_order(
            &pool,
            t.id,
            order.id,
            OrderStatus::Pending,
            OrderStatus::Cancelled
        )
        .await
        .unwrap()
    );
    drop(conn);
    t.cleanup().await;
}
