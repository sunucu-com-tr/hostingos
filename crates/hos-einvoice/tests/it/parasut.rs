//! Paraşüt client against a wiremock server shaped after the API v4
//! reference (read 2026-11-18).

use crate::common;

use hos_core::IntegrationError;
use hos_core::integrations::einvoice::{
    DocumentKind, DocumentStatus, EDocumentRef, EInvoiceProvider,
};
use hos_einvoice::{OutboundPolicy, build_with_policy};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "client-secret-value";
const PASSWORD: &str = "user-password-value";

async fn client(server: &MockServer) -> std::sync::Arc<dyn EInvoiceProvider> {
    build_with_policy(
        "parasut",
        &json!({"company_id": "115", "client_id": "cid", "username": "api@example.test", "base_url": format!("{}/", server.uri())}),
        &json!({"client_secret": SECRET, "password": PASSWORD}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

async fn token(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=password"))
        .and(body_string_contains("redirect_uri=urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "AT1", "token_type": "bearer", "expires_in": 7200, "refresh_token": "RT1"})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn oauth_password_grant_and_bad_credentials() {
    let server = MockServer::start().await;
    token(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/me"))
        .and(header("authorization", "Bearer AT1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "1", "type": "users", "attributes": {"name": "API Kullanıcısı"}}})))
        .expect(1)
        .mount(&server)
        .await;
    let info = client(&server).await.test_connection().await.unwrap();
    assert_eq!(info.details.as_deref(), Some("user API Kullanıcısı"));

    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_grant", "error_description": format!("wrong password {PASSWORD}")})))
        .mount(&bad)
        .await;
    let err = client(&bad).await.test_connection().await.unwrap_err();
    assert!(matches!(err, IntegrationError::Auth(_)), "{err:?}");
    assert!(!err.to_string().contains(PASSWORD));
}

#[tokio::test]
async fn expired_token_is_renewed_once() {
    let server = MockServer::start().await;
    token(&server).await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "AT2", "expires_in": 7200, "refresh_token": "RT2"}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/e_invoice_inboxes"))
        .and(header("authorization", "Bearer AT1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/e_invoice_inboxes"))
        .and(query_param("filter[vkn]", "1234567890"))
        .and(header("authorization", "Bearer AT2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "9", "type": "e_invoice_inboxes", "attributes": {"vkn": "1234567890", "e_invoice_address": "urn:mail:defaultpk@alici.test", "name": "Alıcı A.Ş."}}], "meta": {"total_count": 1}})))
        .mount(&server)
        .await;
    let info = client(&server)
        .await
        .lookup_taxpayer("1234567890")
        .await
        .unwrap();
    assert!(info.registered);
    assert_eq!(
        info.aliases,
        vec!["urn:mail:defaultpk@alici.test".to_owned()]
    );
    assert_eq!(info.title.as_deref(), Some("Alıcı A.Ş."));
}

#[tokio::test]
async fn earsiv_flow_with_trackable_job_and_pdf() {
    let server = MockServer::start().await;
    token(&server).await;
    let req = common::earsiv_request();
    Mock::given(method("GET"))
        .and(path("/v4/115/contacts"))
        .and(query_param("filter[tax_number]", "10000000146"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/115/contacts"))
        .and(body_string_contains("\"contact_type\":\"person\""))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"data": {"id": "501", "type": "contacts"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/sales_invoices"))
        .and(query_param("filter[contact_id]", "501"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/115/sales_invoices"))
        .and(body_string_contains("\"currency\":\"TRL\""))
        .and(body_string_contains("\"vat_rate\":\"20\""))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"id": "7001", "type": "sales_invoices", "attributes": {"net_total": "120.0"}}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/sales_invoices/7001"))
        .and(query_param("include", "active_e_document"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"id": "7001", "relationships": {"active_e_document": {"data": null}}}}),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/115/e_archives"))
        .and(body_string_contains("\"payment_type\":\"KREDIKARTI/BANKAKARTI\""))
        .and(body_string_contains("\"id\":\"7001\""))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"id": "job-1", "type": "trackable_jobs", "attributes": {"status": "running"}}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/trackable_jobs/job-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"id": "job-1", "attributes": {"status": "running", "errors": []}}}),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let c = client(&server).await;
    let mut cp = Value::Null;
    let st = c.submit(&req, &mut cp).await.unwrap();
    assert_eq!(st.status, DocumentStatus::Processing);
    assert_eq!(cp["contact_id"], json!("501"));
    assert_eq!(cp["sales_invoice_id"], json!("7001"));
    assert_eq!(cp["job_id"], json!("job-1"));

    // The job finishes; the retry polls instead of posting again.
    Mock::given(method("GET"))
        .and(path("/v4/115/trackable_jobs/job-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"id": "job-1", "attributes": {"status": "done", "errors": []}}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/sales_invoices/7001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "7001", "relationships": {"active_e_document": {"data": {"id": "88", "type": "e_archives"}}}}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/e_archives/88"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "88", "type": "e_archives", "attributes": {
            "uuid": "3c5108e0-7962-4c02-b050-1022834d683f", "invoice_number": "PRS2026000000123",
            "status": "sent", "cancellable_until": "2026-11-25T23:59:59+03:00"
        }}})))
        .mount(&server)
        .await;
    let st = c.submit(&req, &mut cp).await.unwrap();
    assert_eq!(st.status, DocumentStatus::Issued);
    assert_eq!(st.number.as_deref(), Some("PRS2026000000123"));
    assert_eq!(st.provider_id.as_deref(), Some("e_archives/88"));
    assert_eq!(
        st.ettn.unwrap().to_string(),
        "3c5108e0-7962-4c02-b050-1022834d683f"
    );
    assert!(st.cancellable_until.is_some());

    // PDF: a short-lived link, downloaded through the guarded client.
    Mock::given(method("GET"))
        .and(path("/v4/115/e_archives/88/pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "1", "type": "e_document_pdfs", "attributes": {"url": format!("{}/files/88.pdf", server.uri()), "expires_at": "2026-11-18T12:00:00Z"}}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/88.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.4 e-arsiv".to_vec()))
        .mount(&server)
        .await;
    let r = EDocumentRef {
        kind: DocumentKind::EArsiv,
        ettn: req.ettn,
        number: None,
        provider_id: st.provider_id.clone(),
        checkpoint: cp.clone(),
    };
    assert_eq!(c.pdf(&r).await.unwrap(), b"%PDF-1.4 e-arsiv");
    assert_eq!(c.status(&r).await.unwrap().status, DocumentStatus::Issued);
    assert!(matches!(
        c.cancel(&r, "x").await.unwrap_err(),
        IntegrationError::Unsupported(_)
    ));
}

#[tokio::test]
async fn total_mismatch_deletes_the_draft() {
    let server = MockServer::start().await;
    token(&server).await;
    let req = common::earsiv_request();
    Mock::given(method("GET"))
        .and(path("/v4/115/contacts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "501", "type": "contacts"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/115/sales_invoices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v4/115/sales_invoices"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                json!({"data": {"id": "7002", "attributes": {"net_total": 120.01}}}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v4/115/sales_invoices/7002"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let mut cp = Value::Null;
    let err = client(&server)
        .await
        .submit(&req, &mut cp)
        .await
        .unwrap_err();
    assert!(
        matches!(err, IntegrationError::InvalidInput(ref m) if m.contains("120.01")),
        "{err:?}"
    );
    assert!(cp["sales_invoice_id"].is_null());
    assert_eq!(cp["contact_id"], json!("501"));
}

#[tokio::test]
async fn failed_job_is_reported_and_cleared() {
    let server = MockServer::start().await;
    token(&server).await;
    let req = common::earsiv_request();
    Mock::given(method("GET"))
        .and(path("/v4/115/trackable_jobs/job-9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "job-9", "attributes": {"status": "error", "errors": ["Alıcı adresi eksik"]}}})))
        .mount(&server)
        .await;
    let mut cp = json!({"contact_id": "1", "sales_invoice_id": "2", "job_id": "job-9"});
    let st = client(&server).await.submit(&req, &mut cp).await.unwrap();
    assert_eq!(st.status, DocumentStatus::Failed);
    assert_eq!(st.message.as_deref(), Some("Alıcı adresi eksik"));
    assert!(
        cp["job_id"].is_null(),
        "a later retry may post a new e-document"
    );

    let limited = MockServer::start().await;
    token(&limited).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "10"))
        .mount(&limited)
        .await;
    let err = client(&limited)
        .await
        .lookup_taxpayer("1234567890")
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::RateLimited { .. }));
    assert!(err.is_retryable());
}
