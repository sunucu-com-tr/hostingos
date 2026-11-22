//! Nilvera (private integrator, özel entegratör) — e-Fatura and e-Arşiv.
//!
//! * **Docs** (read 2026-11-18): <https://developer.nilvera.com> —
//!   "Genel Bakış" (environments, auth), "API Anahtarı Üretimi", "Hata
//!   Kodları", General API `GET /GlobalCompany/GetGlobalCustomerInfo/{TaxNumber}`
//!   and `GET /Company`, e-Arşiv API `POST /Send/Xml`,
//!   `GET /Invoices/{UUID}/Status|pdf|xml`, `PUT /Invoices/{UUID}/Cancel`,
//!   e-Fatura API `POST /Send/Xml?Alias=…`, `GET /Sale/{UUID}/Status|pdf|xml`
//!   (each page embeds its OpenAPI 3.0.1 fragment, API version `v1`).
//! * **Auth**: persistent API key created in the Nilvera portal, sent as
//!   `Authorization: Bearer {key}`.
//! * **Hosts**: live `https://api.nilvera.com/{api}`, test
//!   `https://apitest.nilvera.com/{api}` (`api` = `general`, `einvoice`,
//!   `earchive`); keys and companies are separate per environment.
//!
//! Documents are sent as UBL-TR XML built by [`crate::ubl`] (multipart
//! `file` field). HostingOS assigns the legal number (series + year +
//! sequence) and the ETTN; a repeated ETTN is refused by Nilvera with HTTP 409
//! / code 1001, which HostingOS treats as "already submitted" and answers
//! with the document's current status — so retries never create a second
//! document.
//!
//! Internet-sale information for e-Arşiv is written into standard UBL fields
//! (`cbc:WebsiteURI`, `cac:PaymentMeans`); Nilvera's documentation does not
//! describe a separate field for it in the XML upload.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::Utc;
use hos_core::IntegrationError;
use hos_core::integrations::einvoice::{
    Capabilities, DocumentKind, DocumentState, DocumentStatus, EDocumentRef, EDocumentRequest,
    EInvoiceProvider, TaxpayerInfo,
};
use hos_core::integrations::{ConnectionInfo, FieldReader};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::http::{HttpClient, MAX_API_BODY, MAX_DOCUMENT_BODY, RawResponse, describe_body};
use crate::ubl;

/// `integrations.kind`.
pub const KIND: &str = hos_core::integrations::kinds::NILVERA;
/// Live host.
pub const LIVE_URL: &str = "https://api.nilvera.com/";
/// Test host.
pub const TEST_URL: &str = "https://apitest.nilvera.com/";

/// Settings of a Nilvera integration.
#[derive(Debug, Clone)]
pub struct NilveraConfig {
    /// Host (`https://api.nilvera.com/` or the test host).
    pub base_url: Url,
    /// Whether this is the test environment.
    pub test: bool,
    /// API key.
    pub api_key: SecretString,
    /// Request timeout.
    pub timeout: Duration,
    /// Outbound policy.
    pub outbound: OutboundPolicy,
}

impl NilveraConfig {
    /// From an `integrations` row.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let test = c.string_or("environment", "test")? != "live";
        let base_url = match c.opt_url("base_url")? {
            Some(u) => u,
            None => Url::parse(if test { TEST_URL } else { LIVE_URL })
                .map_err(|e| IntegrationError::Config(e.to_string()))?,
        };
        Ok(Self {
            base_url: ensure_slash(base_url),
            test,
            api_key: s.secret("api_key")?,
            timeout: Duration::from_secs(c.u64_or("timeout_secs", 30)?),
            outbound: OutboundPolicy::global(),
        })
    }
}

fn ensure_slash(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    url
}

/// Nilvera client.
pub struct Nilvera {
    cfg: NilveraConfig,
    http: HttpClient,
}

impl std::fmt::Debug for Nilvera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nilvera")
            .field("test", &self.cfg.test)
            .finish_non_exhaustive()
    }
}

/// `{"Message": …, "Errors": [{"Code", "Description", "Detail"}]}`.
fn errors(body: &[u8]) -> (Vec<i64>, String) {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return (vec![], describe_body(body));
    };
    let list = v["Errors"].as_array().cloned().unwrap_or_default();
    let codes = list.iter().filter_map(|e| e["Code"].as_i64()).collect();
    let mut parts: Vec<String> = list
        .iter()
        .map(|e| {
            let code = e["Code"]
                .as_i64()
                .map(|c| c.to_string())
                .unwrap_or_default();
            let text = [e["Description"].as_str(), e["Detail"].as_str()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            format!("{code} {text}").trim().to_owned()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty()
        && let Some(m) = v["Message"].as_str()
    {
        parts.push(m.to_owned());
    }
    (codes, parts.join("; "))
}

/// The endpoint returns either the raw file or a base64 string (plain or
/// JSON-quoted); accept both and verify the content type by magic bytes.
fn decode_document(body: &[u8], expect_pdf: bool) -> Option<Vec<u8>> {
    let looks_right = |b: &[u8]| {
        if expect_pdf {
            b.starts_with(b"%PDF")
        } else {
            let t = b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b);
            t.iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'<')
        }
    };
    if looks_right(body) {
        return Some(body.to_vec());
    }
    let text = std::str::from_utf8(body).ok()?.trim();
    let text = serde_json::from_str::<String>(text).unwrap_or_else(|_| text.to_owned());
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(text.trim().as_bytes())
        .ok()?;
    looks_right(&decoded).then_some(decoded)
}

impl Nilvera {
    /// New client.
    pub fn new(cfg: NilveraConfig) -> Result<Self, IntegrationError> {
        let http = HttpClient::new("nilvera", cfg.timeout, &[&cfg.api_key], cfg.outbound)?;
        http.check_endpoint(&cfg.base_url)?;
        Ok(Self { cfg, http })
    }

    fn url(&self, path: &str) -> Result<Url, IntegrationError> {
        self.cfg
            .base_url
            .join(path)
            .map_err(|e| IntegrationError::Config(format!("nilvera: bad URL: {e}")))
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        operation: &'static str,
        limit: usize,
    ) -> Result<RawResponse, IntegrationError> {
        self.http
            .execute(
                req.bearer_auth(self.cfg.api_key.expose_str()),
                operation,
                limit,
            )
            .await
    }

    fn error(&self, resp: &RawResponse) -> IntegrationError {
        let (_, msg) = errors(&resp.body);
        self.http.status_error(resp, &msg)
    }

    async fn get_json(
        &self,
        path: &str,
        operation: &'static str,
    ) -> Result<Value, IntegrationError> {
        let resp = self
            .send(
                self.http.client().get(self.url(path)?),
                operation,
                MAX_API_BODY,
            )
            .await?;
        if !resp.ok() {
            return Err(self.error(&resp));
        }
        serde_json::from_slice(&resp.body).map_err(|_| {
            IntegrationError::remote(
                Some(resp.status),
                format!(
                    "nilvera: {operation} returned {}",
                    describe_body(&resp.body)
                ),
            )
        })
    }

    fn doc_path(kind: DocumentKind, ettn: Uuid, what: &str) -> String {
        match kind {
            DocumentKind::EArsiv => format!("earchive/Invoices/{ettn}/{what}"),
            DocumentKind::EFatura => format!("einvoice/Sale/{ettn}/{what}"),
        }
    }

    async fn state(
        &self,
        kind: DocumentKind,
        ettn: Uuid,
    ) -> Result<DocumentState, IntegrationError> {
        let v = self
            .get_json(&Self::doc_path(kind, ettn, "Status"), "status")
            .await?;
        let (code, detail, cancelled, answer) = match kind {
            DocumentKind::EArsiv => (
                v["StatusCode"].as_str(),
                v["StatusDetail"].as_str(),
                v["CancelStatus"].as_bool() == Some(true),
                None,
            ),
            DocumentKind::EFatura => (
                v["InvoiceStatus"]["Code"].as_str(),
                v["InvoiceStatus"]["DetailDescription"]
                    .as_str()
                    .or(v["InvoiceStatus"]["Description"].as_str()),
                false,
                v["Answer"]["AnswerCode"].as_str(),
            ),
        };
        let status = if cancelled {
            DocumentStatus::Cancelled
        } else if answer == Some("rejected") {
            DocumentStatus::Rejected
        } else {
            match code {
                Some("succeed") => DocumentStatus::Issued,
                Some("error") => DocumentStatus::Failed,
                _ => DocumentStatus::Processing,
            }
        };
        let mut st = DocumentState::new(status);
        st.provider_id = Some(ettn.to_string());
        if matches!(status, DocumentStatus::Failed | DocumentStatus::Rejected) {
            let note = if status == DocumentStatus::Rejected {
                v["Answer"]["AnswerNote"].as_str().or(detail)
            } else {
                detail
            };
            st.message = note.map(|m| self.http.scrub(m));
        }
        Ok(st)
    }

    async fn document(
        &self,
        document: &EDocumentRef,
        what: &str,
        pdf: bool,
    ) -> Result<Vec<u8>, IntegrationError> {
        let resp = self
            .send(
                self.http.client().get(self.url(&Self::doc_path(
                    document.kind,
                    document.ettn,
                    what,
                ))?),
                if pdf { "pdf" } else { "xml" },
                MAX_DOCUMENT_BODY,
            )
            .await?;
        if !resp.ok() {
            return Err(self.error(&resp));
        }
        decode_document(&resp.body, pdf).ok_or_else(|| {
            IntegrationError::remote(
                Some(resp.status),
                format!("nilvera: {what} returned {}", describe_body(&resp.body)),
            )
        })
    }
}

#[async_trait]
impl EInvoiceProvider for Nilvera {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            cancel_earsiv: true,
            return_invoices: true,
            ubl_download: true,
            assigns_numbers: false,
            foreign_currency: true,
            test_environment: self.cfg.test,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let v = self.get_json("general/Company", "company").await?;
        Ok(ConnectionInfo {
            server_version: Some(
                if self.cfg.test {
                    "test environment"
                } else {
                    "live"
                }
                .into(),
            ),
            details: v["Name"].as_str().map(|n| self.http.scrub(n)),
        })
    }

    async fn lookup_taxpayer(&self, tax_id: &str) -> Result<TaxpayerInfo, IntegrationError> {
        let mut url = self.url(&format!(
            "general/GlobalCompany/GetGlobalCustomerInfo/{tax_id}"
        ))?;
        url.query_pairs_mut()
            .append_pair("globalUserType", "Invoice");
        let resp = self
            .send(self.http.client().get(url), "taxpayer", MAX_API_BODY)
            .await?;
        if resp.status == 404 {
            return Ok(TaxpayerInfo::default());
        }
        if !resp.ok() {
            return Err(self.error(&resp));
        }
        let v: Value = serde_json::from_slice(&resp.body).map_err(|_| {
            IntegrationError::remote(
                Some(resp.status),
                "nilvera: taxpayer lookup returned a non-JSON body",
            )
        })?;
        let aliases: Vec<String> = v["Aliases"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter(|a| a["DeletionTime"].is_null())
                    .filter_map(|a| a["Name"].as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(TaxpayerInfo {
            registered: !aliases.is_empty(),
            title: v["Title"].as_str().map(str::to_owned),
            aliases,
        })
    }

    async fn submit(
        &self,
        request: &EDocumentRequest,
        checkpoint: &mut Value,
    ) -> Result<DocumentState, IntegrationError> {
        if checkpoint["sent"] == json!(true) {
            return self.state(request.kind, request.ettn).await;
        }
        let today = (Utc::now() + chrono::Duration::hours(3)).date_naive();
        let xml = ubl::to_xml(request, today)
            .map_err(|e| IntegrationError::InvalidInput(e.to_string()))?;
        let mut url = self.url(match request.kind {
            DocumentKind::EArsiv => "earchive/Send/Xml",
            DocumentKind::EFatura => "einvoice/Send/Xml",
        })?;
        if request.kind == DocumentKind::EFatura {
            let alias = request.recipient_alias.as_deref().ok_or_else(|| {
                IntegrationError::InvalidInput("nilvera: recipient alias missing".into())
            })?;
            url.query_pairs_mut().append_pair("Alias", alias);
        }
        let boundary = format!("hos-{}", Uuid::now_v7().simple());
        let mut body = Vec::with_capacity(xml.len() + 256);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}.xml\"\r\nContent-Type: application/xml\r\n\r\n",
                request.ettn
            )
            .as_bytes(),
        );
        body.extend_from_slice(xml.as_bytes());
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = self
            .http
            .client()
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body);
        let resp = self.send(req, "send", MAX_API_BODY).await?;
        if resp.status == 409 {
            let (codes, _) = errors(&resp.body);
            if codes.contains(&1001) {
                // Same ETTN already at Nilvera: an earlier attempt got through.
                checkpoint["sent"] = json!(true);
                return self.state(request.kind, request.ettn).await;
            }
        }
        if !resp.ok() {
            return Err(self.error(&resp));
        }
        checkpoint["sent"] = json!(true);
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        let mut st = DocumentState::new(DocumentStatus::Processing);
        st.provider_id = Some(request.ettn.to_string());
        st.number = v["InvoiceNumber"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| request.number.clone());
        if let Some(u) = v["UUID"].as_str().and_then(|u| Uuid::parse_str(u).ok())
            && u != request.ettn
        {
            st.ettn = Some(u);
        }
        Ok(st)
    }

    async fn status(&self, document: &EDocumentRef) -> Result<DocumentState, IntegrationError> {
        self.state(document.kind, document.ettn).await
    }

    async fn pdf(&self, document: &EDocumentRef) -> Result<Vec<u8>, IntegrationError> {
        self.document(document, "pdf", true).await
    }

    async fn ubl(&self, document: &EDocumentRef) -> Result<Vec<u8>, IntegrationError> {
        self.document(document, "xml", false).await
    }

    async fn cancel(
        &self,
        document: &EDocumentRef,
        _reason: &str,
    ) -> Result<DocumentState, IntegrationError> {
        if document.kind != DocumentKind::EArsiv {
            return Err(IntegrationError::Unsupported("e-Fatura cancellation"));
        }
        let url = self.url(&Self::doc_path(document.kind, document.ettn, "Cancel"))?;
        let resp = self
            .send(self.http.client().put(url), "cancel", MAX_API_BODY)
            .await?;
        if !resp.ok() {
            return Err(self.error(&resp));
        }
        let messages: Vec<String> =
            serde_json::from_slice::<Vec<String>>(&resp.body).unwrap_or_default();
        let mut st = self.state(document.kind, document.ettn).await?;
        if st.status != DocumentStatus::Cancelled && st.message.is_none() && !messages.is_empty() {
            st.message = Some(self.http.scrub(&messages.join("; ")));
        }
        Ok(st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_raw_or_base64() {
        assert_eq!(decode_document(b"%PDF-1.7 x", true).unwrap(), b"%PDF-1.7 x");
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 y");
        assert_eq!(
            decode_document(b64.as_bytes(), true).unwrap(),
            b"%PDF-1.4 y"
        );
        let quoted = format!("\"{b64}\"");
        assert_eq!(
            decode_document(quoted.as_bytes(), true).unwrap(),
            b"%PDF-1.4 y"
        );
        assert!(decode_document(b"<html>", true).is_none());
        assert!(decode_document(b"\xEF\xBB\xBF <Invoice/>", false).is_some());
    }

    #[test]
    fn error_bodies() {
        let (codes, msg) = errors(br#"{"Message":"HATALI ISTEK","Errors":[{"Code":1001,"Description":"ETTN Numarasi Sistemde Kayitlidir.","Detail":"x"}]}"#);
        assert_eq!(codes, vec![1001]);
        assert!(msg.starts_with("1001 ETTN"));
        assert_eq!(errors(b"<html>").1, "an HTML page");
    }
}
