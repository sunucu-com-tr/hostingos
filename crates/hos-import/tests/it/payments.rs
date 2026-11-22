//! Invoice payments: `amount_paid` of an imported invoice follows its
//! imported transactions (open invoices) or its WHMCS status (paid ones).

use hos_db::repo::import as repo;
use hos_import::entity::Entity;
use hos_import::row::Row;
use rust_decimal::Decimal;

use crate::common::{Env, fixture};

fn invoice(id: i64, total: &str, credit: &str, status: &str) -> Row {
    Row::new()
        .with("id", id)
        .with("userid", 1)
        .with("invoicenum", "")
        .with("date", "2099-02-01")
        .with("duedate", "2099-02-08")
        .with("subtotal", total)
        .with("credit", credit)
        .with("tax", "0")
        .with("tax2", "0")
        .with("total", total)
        .with("taxrate", "0")
        .with("taxrate2", "0")
        .with("status", status)
        .with("paymentmethod", "banktransfer")
}

fn txn(id: i64, invoice: i64, amount_in: &str, amount_out: &str, currency: i64) -> Row {
    Row::new()
        .with("id", id)
        .with("userid", 1)
        .with("currency", currency)
        .with("gateway", "banktransfer")
        .with("date", "2099-02-02 12:00:00")
        .with("description", "Invoice Payment")
        .with("amountin", amount_in)
        .with("fees", "0")
        .with("amountout", amount_out)
        .with("transid", format!("PX-{id}"))
        .with("invoiceid", invoice)
}

fn source() -> hos_import::source::fixture::FixtureSource {
    let mut src = fixture();
    // Client 1 bills in TRY (currency id 2); USD (id 1) payments do not count.
    src.rows_mut(Entity::Invoices).extend([
        invoice(200, "100.00", "0", "Unpaid"),
        // WHMCS `total` is after credit: 100 − 10.
        invoice(201, "100.00", "10.00", "Unpaid").with("total", "90.00"),
        invoice(202, "30.00", "0", "Unpaid"),
        invoice(203, "80.00", "0", "Collections"),
    ]);
    src.rows_mut(Entity::Transactions).extend([
        // 200: partial payment.
        txn(50, 200, "40.00", "0", 0),
        // 201: 70 paid, 20 refunded → 50 (cap 100 − 10 credit = 90).
        txn(51, 201, "70.00", "0", 0),
        txn(52, 201, "0", "20.00", 0).with("refundid", 51),
        // 202: overpaid → clamped to the total.
        txn(53, 202, "50.00", "0", 0),
        // 203: a payment in another currency is not counted.
        txn(54, 203, "25.00", "0", 2),
        txn(55, 203, "99.00", "0", 1),
    ]);
    src
}

async fn paid_and_balance(env: &Env, number_source: &str) -> (Decimal, Decimal) {
    sqlx::query_as(
        "select i.amount_paid, i.balance from invoices i
           join import_id_map m on m.tenant_id = i.tenant_id and m.target_id = i.id
          where i.tenant_id = $1 and m.entity = 'invoices' and m.source_id = $2",
    )
    .bind(env.tenant)
    .bind(number_source)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn open_invoice_payments_follow_the_transactions() {
    let Some(env) = Env::new().await else { return };
    let opts = crate::all_options();
    let run = env.import(source(), opts.clone()).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);

    let d = |v: i64| Decimal::new(v, 0);
    assert_eq!(
        paid_and_balance(&env, "200").await,
        (d(40), d(60)),
        "partial payment"
    );
    assert_eq!(
        paid_and_balance(&env, "201").await,
        (d(50), d(40)),
        "partial refund, credit kept"
    );
    assert_eq!(
        paid_and_balance(&env, "202").await,
        (d(30), d(0)),
        "clamped to the total"
    );
    assert_eq!(
        paid_and_balance(&env, "203").await,
        (d(25), d(55)),
        "other currency ignored"
    );
    assert_eq!(
        paid_and_balance(&env, "101").await,
        (d(0), d(6)),
        "no transactions: unpaid"
    );
    // Paid in WHMCS: the status wins over transactions that do not add up
    // (30 paid − 5 refunded ≠ 30 due), and the difference is reported.
    assert_eq!(paid_and_balance(&env, "100").await, (d(30), d(0)));

    let msgs = repo::list_messages(&env.pool, env.tenant, run.id, None, 5000)
        .await
        .unwrap();
    let has = |code: &str, sid: &str| {
        msgs.iter()
            .any(|m| m.code == code && m.source_id.as_deref() == Some(sid))
    };
    assert!(has("paid_amount_mismatch", "100"), "paid mismatch reported");
    assert!(has("overpaid", "202"), "overpayment reported");
    assert!(has("foreign_currency_payment", "203"));
    assert!(!has("paid_amount_mismatch", "200"));

    // Re-run: the same figures, nothing added twice.
    let rerun = env.import(source(), opts).await;
    assert_eq!(rerun.status, "completed", "error: {:?}", rerun.error);
    assert_eq!(paid_and_balance(&env, "200").await, (d(40), d(60)));
    assert_eq!(paid_and_balance(&env, "201").await, (d(50), d(40)));
    assert_eq!(paid_and_balance(&env, "202").await, (d(30), d(0)));
    assert_eq!(paid_and_balance(&env, "100").await, (d(30), d(0)));

    env.cleanup().await;
}
