//! Nilvera client against a wiremock server shaped after the documented
//! endpoints (developer.nilvera.com, read 2026-11-18).

use crate::common;

use base64::Engine as _;
use hos_core::IntegrationError;
use hos_core::integrations::einvoice::{
    DocumentKind, DocumentStatus, EDocumentRef, EInvoiceProvider,
};
use hos_einvoice::{OutboundPolicy, build_with_policy};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "9EE05B6564525810C86A32646DB46A26";

async fn client(server: &MockServer) -> std::sync::Arc<dyn EInvoiceProvider> {
    build_with_policy(
        "nilvera",
        &json!({"environment": "test", "base_url": format!("{}/", server.uri())}),
        &json!({"api_key": KEY}),
        OutboundPolicy::Unrestricted,
    )
    .unwrap()
}

fn doc_ref(kind: DocumentKind, ettn: uuid::Uuid) -> EDocumentRef {
    EDocumentRef {
        kind,
        ettn,
        number: None,
        provider_id: None,
        checkpoint: Value::Null,
    }
}

#[tokio::test]
async fn auth_and_connection() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/general/Company"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"Name": "Örnek A.Ş.", "TaxNumber": "1234567890"})),
        )
        .mount(&server)
        .await;
    let c = client(&server).await;
    let info = c.test_connection().await.unwrap();
    assert_eq!(info.details.as_deref(), Some("Örnek A.Ş."));
    assert!(c.capabilities().test_environment);

    let bad = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string(format!("invalid key {KEY}")))
        .mount(&bad)
        .await;
    let err = client(&bad).await.test_connection().await.unwrap_err();
    assert!(matches!(err, IntegrationError::Auth(_)), "{err:?}");
    assert!(
        !err.to_string().contains(KEY),
        "secrets are scrubbed: {err}"
    );
}

#[tokio::test]
async fn registered_user_lookup() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/general/GlobalCompany/GetGlobalCustomerInfo/1234567890"))
        .and(query_param("globalUserType", "Invoice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "TaxNumber": "1234567890", "Title": "Alıcı A.Ş.",
            "Aliases": [
                {"Name": "urn:mail:eski@alici.test", "CreationTime": "2020-01-01T00:00:00", "DeletionTime": "2021-01-01T00:00:00"},
                {"Name": "urn:mail:defaultpk@alici.test", "CreationTime": "2021-01-01T00:00:00", "DeletionTime": null}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/general/GlobalCompany/GetGlobalCustomerInfo/10000000146"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"Message": "HATALI ISTEK", "Errors": [{"Code": 3003, "Description": "Bulunamadı"}]})))
        .mount(&server)
        .await;
    let c = client(&server).await;
    let info = c.lookup_taxpayer("1234567890").await.unwrap();
    assert!(info.registered);
    assert_eq!(
        info.aliases,
        vec!["urn:mail:defaultpk@alici.test".to_owned()]
    );
    assert!(!c.lookup_taxpayer("10000000146").await.unwrap().registered);
}

#[tokio::test]
async fn submit_status_pdf_cancel() {
    let server = MockServer::start().await;
    let req = common::earsiv_request();
    let ettn = req.ettn;
    Mock::given(method("POST"))
        .and(path("/earchive/Send/Xml"))
        .and(header_regex(
            "content-type",
            "^multipart/form-data; boundary=",
        ))
        .and(body_string_contains(
            "<cbc:ProfileID>EARSIVFATURA</cbc:ProfileID>",
        ))
        .and(body_string_contains(ettn.to_string().to_ascii_uppercase()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"UUID": ettn, "InvoiceNumber": req.number})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/earchive/Invoices/{ettn}/Status")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"StatusCode": "succeed", "StatusDetail": "Başarılı", "ReportStatus": "NotReported", "CancelStatus": false})))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    let pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7 legal");
    Mock::given(method("GET"))
        .and(path(format!("/earchive/Invoices/{ettn}/pdf")))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("\"{pdf}\"")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/earchive/Invoices/{ettn}/xml")))
        .respond_with(ResponseTemplate::new(200).set_body_string("<Invoice/>"))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/earchive/Invoices/{ettn}/Cancel")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(["İptal edildi"])))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server).await;
    let mut cp = Value::Null;
    let st = c.submit(&req, &mut cp).await.unwrap();
    assert_eq!(st.status, DocumentStatus::Processing);
    assert_eq!(st.number, req.number);
    assert_eq!(cp["sent"], json!(true));
    // A retry with the checkpoint only asks for the status (no second upload).
    let st = c.submit(&req, &mut cp).await.unwrap();
    assert_eq!(st.status, DocumentStatus::Issued);

    let r = doc_ref(DocumentKind::EArsiv, ettn);
    assert_eq!(c.status(&r).await.unwrap().status, DocumentStatus::Issued);
    assert_eq!(c.pdf(&r).await.unwrap(), b"%PDF-1.7 legal");
    assert_eq!(c.ubl(&r).await.unwrap(), b"<Invoice/>");

    Mock::given(method("GET"))
        .and(path(format!("/earchive/Invoices/{ettn}/Status")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"StatusCode": "succeed", "CancelStatus": true})),
        )
        .mount(&server)
        .await;
    assert_eq!(
        c.cancel(&r, "iade").await.unwrap().status,
        DocumentStatus::Cancelled
    );
    let err = c
        .cancel(&doc_ref(DocumentKind::EFatura, ettn), "x")
        .await
        .unwrap_err();
    assert!(matches!(err, IntegrationError::Unsupported(_)));
}

#[tokio::test]
async fn duplicate_ettn_is_answered_with_status() {
    let server = MockServer::start().await;
    let req = common::earsiv_request();
    let ettn = req.ettn;
    Mock::given(method("POST"))
        .and(path("/earchive/Send/Xml"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"Message": "HATALI ISTEK", "Errors": [{"Code": 1001, "Description": "ETTN Numarası Sistemde Kayıtlıdır.", "Detail": format!("{ettn} ETTN Numarası Sistemde Kayıtlıdır.")}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/earchive/Invoices/{ettn}/Status")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"StatusCode": "waiting", "CancelStatus": false})),
        )
        .mount(&server)
        .await;
    let mut cp = Value::Null;
    let st = client(&server).await.submit(&req, &mut cp).await.unwrap();
    assert_eq!(st.status, DocumentStatus::Processing);
    assert_eq!(cp["sent"], json!(true));
}

#[tokio::test]
async fn efatura_needs_alias_and_maps_rejection_and_errors() {
    let server = MockServer::start().await;
    let mut req = common::earsiv_request();
    req.kind = DocumentKind::EFatura;
    req.profile = hos_core::integrations::einvoice::Profile::TemelFatura;
    req.internet_sale = None;
    req.recipient_alias = Some("urn:mail:defaultpk@alici.test".into());
    let ettn = req.ettn;
    Mock::given(method("POST"))
        .and(path("/einvoice/Send/Xml"))
        .and(query_param("Alias", "urn:mail:defaultpk@alici.test"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({"Message": "HATALI ISTEK", "Errors": [{"Code": 2000, "Description": "Yeterli Kontörünüz Bulunmamaktadır."}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/einvoice/Sale/{ettn}/Status")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "InvoiceProfile": "TICARIFATURA",
            "InvoiceStatus": {"Code": "succeed", "Description": "Başarılı"},
            "Answer": {"AnswerCode": "rejected", "AnswerNote": "Hatalı tutar"}
        })))
        .mount(&server)
        .await;
    let c = client(&server).await;
    let mut cp = Value::Null;
    let err = c.submit(&req, &mut cp).await.unwrap_err();
    assert!(
        matches!(err, IntegrationError::InvalidInput(ref m) if m.contains("2000")),
        "{err:?}"
    );
    assert!(cp.is_null(), "nothing was sent");
    let st = c
        .status(&doc_ref(DocumentKind::EFatura, ettn))
        .await
        .unwrap();
    assert_eq!(st.status, DocumentStatus::Rejected);
    assert_eq!(st.message.as_deref(), Some("Hatalı tutar"));

    Mock::given(method("GET"))
        .and(path(format!("/einvoice/Sale/{ettn}/pdf")))
        .respond_with(ResponseTemplate::new(503).set_body_string("<html>down</html>"))
        .mount(&server)
        .await;
    let err = c
        .pdf(&doc_ref(DocumentKind::EFatura, ettn))
        .await
        .unwrap_err();
    assert!(err.is_retryable(), "{err:?}");

    // Offline validation refuses an invalid document before any request.
    let mut bad = req.clone();
    bad.recipient_alias = None;
    let err = c.submit(&bad, &mut Value::Null).await.unwrap_err();
    assert!(matches!(err, IntegrationError::InvalidInput(_)));
}
