//! e-Fatura / e-Arşiv automation against the real database with the mock
//! e-invoice provider: exactly one legal document per paid invoice even when
//! jobs are delivered twice, retries and staff alerts on failures, and the
//! refund → cancellation / staff-flag path.

use crate::common::{Env, d, suffix};
use hos_billing::einvoice::{EInvoiceAction, keys};
use hos_billing::payments::RefundInput;
use hos_billing::registry::SaveIntegration;
use hos_core::domain::{IntegrationCategory, InvoiceStatus, ProductType};
use hos_core::ids::{ClientId, IntegrationId, InvoiceId};
use hos_core::money::BillingCycle;
use hos_db::repo::carts::NewCartItem;
use hos_db::repo::einvoice::{self as edocs, EInvoiceDocumentRow};
use hos_db::repo::{invoices, payments};
use serde_json::{Value, json};

/// VKN with a valid check digit (`123456789` + `0`).
const SELLER_VKN: &str = "1234567890";

async fn mock_einvoice(env: &Env, config: Value) -> IntegrationId {
    env.billing
        .registry()
        .save_integration(
            env.tenant,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Einvoice,
                kind: "mock".into(),
                name: format!("e-invoice {}", suffix()),
                config,
                secrets: Some(json!({})),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("save integration")
        .id
}

/// Company data, settings and a mock provider.
async fn enable(env: &Env, provider: IntegrationId) {
    sqlx::query(
        "update tenants set legal_name = 'Test Barındırma A.Ş.', tax_number = $2, tax_office = 'Çankaya',
                address = '{\"line1\": \"Atatürk Bulvarı 1\", \"city\": \"Çankaya\", \"state\": \"Ankara\"}'::jsonb
          where id = $1",
    )
    .bind(env.tenant)
    .bind(SELLER_VKN)
    .execute(&env.pool)
    .await
    .unwrap();
    env.setting(keys::ENABLED, json!(true)).await;
    env.setting(keys::INTEGRATION_ID, json!(provider.to_string()))
        .await;
    env.setting(keys::SERIES_EARSIV, json!("TST")).await;
    env.setting(keys::SERIES_EFATURA, json!("TEF")).await;
}

async fn paid_invoice(env: &Env, client: ClientId) -> InvoiceId {
    let gw = env.gateway().await;
    let product = env
        .product("Lisans", ProductType::License, None, json!({}), "100", "0")
        .await;
    let cart = env
        .cart(
            client,
            &[NewCartItem::product(product, BillingCycle::Monthly)],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    env.pay(out.invoice_id, gw).await;
    let inv = invoices::get_invoice(&env.pool, env.tenant, out.invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        inv.status,
        InvoiceStatus::Paid,
        "the payment itself must never be blocked"
    );
    out.invoice_id
}

async fn document(env: &Env, invoice: InvoiceId) -> EInvoiceDocumentRow {
    let mut conn = env.pool.acquire().await.unwrap();
    edocs::for_invoice(&mut conn, env.tenant, invoice, "sale")
        .await
        .unwrap()
        .expect("document row")
}

async fn process(
    env: &Env,
    doc: &EInvoiceDocumentRow,
    action: EInvoiceAction,
    final_attempt: bool,
) -> Result<Value, hos_core::CoreError> {
    env.billing
        .einvoice_process(env.tenant, doc.id, action, final_attempt)
        .await
}

#[tokio::test]
async fn paid_invoice_gets_exactly_one_document() {
    let Some(env) = Env::new().await else { return };
    let provider = mock_einvoice(&env, json!({})).await;
    enable(&env, provider).await;
    let client = env.client("Ayse").await;
    let invoice = paid_invoice(&env, client).await;

    // The payment hook created the row and queued one job.
    let doc = document(&env, invoice).await;
    assert_eq!(doc.status, "pending");
    assert_eq!(env.pending_payloads("einvoice_process").await.len(), 1);

    // Job delivered twice (and the hook / sweep running again).
    process(&env, &doc, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    let again = process(&env, &doc, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    assert!(again.get("skipped").is_some(), "{again}");
    env.billing.einvoice_sweep(env.tenant).await.unwrap();
    let doc = document(&env, invoice).await;
    assert_eq!(doc.status, "issued");
    assert_eq!(doc.kind.as_deref(), Some("earsiv"));
    let year = chrono::Datelike::year(&chrono::Utc::now());
    assert_eq!(
        doc.number.as_deref(),
        Some(format!("TST{year}000000001").as_str())
    );
    assert_eq!(hos_einvoice::mock::submissions(doc.ettn), 1);
    let count = env
        .count("select count(*) from einvoice_documents where tenant_id = $1")
        .await;
    assert_eq!(count, 1);
    // The mock is a test environment: no legal reference on the invoice.
    assert!(doc.is_test);
    let inv = invoices::get_invoice(&env.pool, env.tenant, invoice)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.external_ref, None);

    // The submitted UBL carries the invoice's own totals.
    let xml = hos_einvoice::mock::stored_xml(doc.ettn).unwrap();
    assert!(
        xml.contains("<cbc:PayableAmount currencyID=\"TRY\">120.00</cbc:PayableAmount>"),
        "{xml}"
    );
    assert!(
        xml.contains("schemeID=\"TCKN\">11111111111<"),
        "consumer without TCKN"
    );

    // Legal PDF / UBL downloads go through the provider.
    let (pdf, name) = env
        .billing
        .einvoice_file(env.tenant, doc.id, true)
        .await
        .unwrap();
    assert!(pdf.starts_with(b"%PDF"));
    assert!(name.ends_with(".pdf"));

    // Our own invoice PDF renders with the legal reference.
    let rendered = env
        .billing
        .invoice_pdf(env.tenant, invoice, None)
        .await
        .unwrap();
    assert!(rendered.data.starts_with(b"%PDF"));
    env.cleanup().await;
}

#[tokio::test]
async fn failures_are_retried_flagged_and_recoverable() {
    let Some(env) = Env::new().await else { return };
    env.staff().await;
    let flaky = mock_einvoice(&env, json!({"fail": "transient"})).await;
    enable(&env, flaky).await;
    let client = env.client("Mehmet").await;
    let invoice = paid_invoice(&env, client).await;
    let doc = document(&env, invoice).await;

    // Transient: the job retries (error returned), the row stays pending.
    let err = process(&env, &doc, EInvoiceAction::Submit, false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, hos_core::CoreError::Integration(ref e) if e.is_retryable()),
        "{err:?}"
    );
    let row = document(&env, invoice).await;
    assert_eq!(row.status, "pending");
    assert_eq!(row.attempts, 1);
    assert!(row.last_error.is_some());
    assert!(row.number.is_some(), "number kept for the retry");
    assert_eq!(env.notifications("einvoice").await, 0);
    // Last attempt: staff hear about it.
    process(&env, &doc, EInvoiceAction::Submit, true)
        .await
        .unwrap_err();
    assert!(env.notifications("einvoice").await >= 1);

    // Permanent refusal → failed (no retry loop), staff notified.
    let refusing = mock_einvoice(&env, json!({"fail": "permanent"})).await;
    sqlx::query("update einvoice_documents set integration_id = $2 where id = $1")
        .bind(doc.id)
        .bind(refusing)
        .execute(&env.pool)
        .await
        .unwrap();
    process(&env, &doc, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    let row = document(&env, invoice).await;
    assert_eq!(row.status, "failed");

    // Staff fix the provider and retry: same ETTN and number.
    let good = mock_einvoice(&env, json!({})).await;
    sqlx::query("update einvoice_documents set integration_id = $2 where id = $1")
        .bind(doc.id)
        .bind(good)
        .execute(&env.pool)
        .await
        .unwrap();
    env.billing
        .einvoice_retry(env.tenant, doc.id)
        .await
        .unwrap();
    process(&env, &doc, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    let fixed = document(&env, invoice).await;
    assert_eq!(fixed.status, "issued");
    assert_eq!(fixed.ettn, row.ettn);
    assert_eq!(fixed.number, row.number);
    env.cleanup().await;
}

/// A staff "retry" never breaks the lease of a worker that is submitting
/// the document (a second worker could then submit it again); the legal
/// number is taken together with the frozen request, so the request, the
/// document and the series counter always agree (no gap).
#[tokio::test]
async fn retries_never_double_submit_and_numbers_have_no_gaps() {
    let Some(env) = Env::new().await else { return };
    let provider = mock_einvoice(&env, json!({})).await;
    enable(&env, provider).await;
    let client = env.client("Numara").await;
    let invoice = paid_invoice(&env, client).await;
    let doc = document(&env, invoice).await;
    assert!(doc.number.is_none() && doc.request.is_none());

    // A worker holds the document (its lease runs).
    sqlx::query(
        "update einvoice_documents set locked_until = now() + interval '10 minutes' where id = $1",
    )
    .bind(doc.id)
    .execute(&env.pool)
    .await
    .unwrap();
    assert!(
        env.billing
            .einvoice_retry(env.tenant, doc.id)
            .await
            .is_err(),
        "no retry while a worker holds it"
    );
    let row = document(&env, invoice).await;
    assert!(
        row.locked_until.is_some_and(|t| t > chrono::Utc::now()),
        "the lease stays"
    );
    assert!(
        process(&env, &doc, EInvoiceAction::Submit, false)
            .await
            .unwrap()
            .get("skipped")
            .is_some(),
        "a second worker gets nothing"
    );

    // The lease runs out: one submission, one number.
    sqlx::query("update einvoice_documents set locked_until = null where id = $1")
        .bind(doc.id)
        .execute(&env.pool)
        .await
        .unwrap();
    process(&env, &doc, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    let row = document(&env, invoice).await;
    assert_eq!(row.status, "issued");
    let number = row.number.clone().expect("legal number");
    let frozen = row.request.clone().expect("frozen request");
    assert_eq!(frozen["number"].as_str(), Some(number.as_str()));
    let year = frozen["issue_date"].as_str().unwrap_or_default()[..4].to_owned();
    assert_eq!(&number[3..7], year, "the number's year is the issue year");
    let last: i64 = sqlx::query_scalar(
        "select last_value from einvoice_sequences where tenant_id = $1 and series = 'TST'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(last, 1, "one number used");
    assert!(number.ends_with("000000001"));
    env.cleanup().await;
}

#[tokio::test]
async fn refunds_cancel_earsiv_and_flag_efatura() {
    let Some(env) = Env::new().await else { return };
    env.staff().await;
    // The corporate client's VKN is a registered e-Fatura user at the mock.
    let corporate_vkn = "1234567890";
    let provider = mock_einvoice(&env, json!({"registered": corporate_vkn})).await;
    enable(&env, provider).await;

    // e-Arşiv consumer → full refund → cancelled at the provider.
    let consumer = env.client("Zeynep").await;
    let inv_a = paid_invoice(&env, consumer).await;
    let doc_a = document(&env, inv_a).await;
    process(&env, &doc_a, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    refund_all(&env, inv_a).await;

    // e-Fatura company → refund → staff flag (seller cannot cancel).
    let company = env.client("Firma").await;
    sqlx::query("update clients set kind = 'corporate', company_name = 'Alıcı Ltd. Şti.', tax_number = $2, tax_office = 'Kadıköy' where id = $1")
        .bind(company)
        .bind(corporate_vkn)
        .execute(&env.pool)
        .await
        .unwrap();
    let inv_b = paid_invoice(&env, company).await;
    let doc_b = document(&env, inv_b).await;
    process(&env, &doc_b, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    let doc_b = document(&env, inv_b).await;
    assert_eq!(doc_b.kind.as_deref(), Some("efatura"));
    assert_eq!(doc_b.profile.as_deref(), Some("TEMELFATURA"));
    assert!(doc_b.number.as_deref().unwrap().starts_with("TEF"));
    refund_all(&env, inv_b).await;

    env.billing.einvoice_sweep(env.tenant).await.unwrap();
    // The sweep queued the cancellation of the e-Arşiv; run it.
    let doc_a = document(&env, inv_a).await;
    process(&env, &doc_a, EInvoiceAction::Cancel, false)
        .await
        .unwrap();
    let doc_a = document(&env, inv_a).await;
    assert_eq!(doc_a.status, "cancelled");
    assert!(doc_a.cancelled_at.is_some());
    let doc_b = document(&env, inv_b).await;
    assert_eq!(doc_b.status, "issued", "an e-Fatura stays issued");
    assert!(doc_b.attention.as_deref().unwrap().contains("e-Fatura"));
    // A second sweep does not flag or cancel again.
    env.billing.einvoice_sweep(env.tenant).await.unwrap();
    let flagged = env
        .count("select count(*) from einvoice_documents where tenant_id = $1 and attention is not null")
        .await;
    assert_eq!(flagged, 1);
    env.cleanup().await;
}

async fn refund_all(env: &Env, invoice: InvoiceId) {
    let txns = payments::invoice_transactions(&env.pool, env.tenant, invoice)
        .await
        .unwrap();
    let txn = txns
        .iter()
        .find(|t| t.kind == hos_db::enums::TransactionKind::Payment)
        .expect("payment");
    env.billing
        .refund_payment(
            env.tenant,
            &RefundInput {
                transaction_id: txn.id,
                amount: txn.amount,
                reason: Some("test".into()),
                to_credit: true,
                staff_user_id: None,
                ip: None,
            },
        )
        .await
        .unwrap();
    let inv = invoices::get_invoice(&env.pool, env.tenant, invoice)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Refunded);
    assert_eq!(inv.total, d("120"));
}

#[tokio::test]
async fn disabled_or_unconfigured_never_blocks_payment() {
    let Some(env) = Env::new().await else { return };
    // Enabled without any e-invoice integration.
    env.setting(keys::ENABLED, json!(true)).await;
    let client = env.client("Can").await;
    let invoice = paid_invoice(&env, client).await;
    let doc = document(&env, invoice).await;
    process(&env, &doc, EInvoiceAction::Submit, false)
        .await
        .unwrap();
    let doc = document(&env, invoice).await;
    assert_eq!(doc.status, "failed");
    assert!(doc.last_error.as_deref().unwrap().contains("integration"));

    // Disabled: nothing is created.
    env.setting(keys::ENABLED, json!(false)).await;
    let other = paid_invoice(&env, client).await;
    let mut conn = env.pool.acquire().await.unwrap();
    assert!(
        edocs::for_invoice(&mut conn, env.tenant, other, "sale")
            .await
            .unwrap()
            .is_none()
    );
    drop(conn);
    env.cleanup().await;
}
