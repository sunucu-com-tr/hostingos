//! Deterministic in-memory provider for demos and tests (kind `mock`; only
//! usable when mock integrations are allowed, see `hos-billing`'s registry).
//!
//! It runs the full UBL-TR builder and offline checks, stores documents in a
//! process-wide map keyed by ETTN (so a repeated submission is recognised
//! like a real integrator would) and never talks to the network.
//!
//! Config: `registered` (comma-separated tax ids that count as e-Fatura
//! users), `fail` (`""`, `"transient"` or `"permanent"`: every submit fails
//! that way), `async` (bool: documents stay `processing` until the first
//! status query).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use hos_core::IntegrationError;
use hos_core::integrations::ConnectionInfo;
use hos_core::integrations::FieldReader;
use hos_core::integrations::einvoice::{
    Capabilities, DocumentKind, DocumentState, DocumentStatus, EDocumentRef, EDocumentRequest,
    EInvoiceProvider, TaxpayerInfo,
};
use serde_json::Value;
use uuid::Uuid;

use crate::ubl;

/// `integrations.kind`.
pub const KIND: &str = "mock";

#[derive(Debug, Clone)]
struct Stored {
    kind: DocumentKind,
    number: Option<String>,
    status: DocumentStatus,
    xml: String,
    submissions: usize,
}

fn store() -> &'static Mutex<HashMap<Uuid, Stored>> {
    static STORE: OnceLock<Mutex<HashMap<Uuid, Stored>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How many times a document with `ettn` was submitted (tests).
pub fn submissions(ettn: Uuid) -> usize {
    store()
        .lock()
        .map(|s| s.get(&ettn).map_or(0, |d| d.submissions))
        .unwrap_or(0)
}

/// The stored UBL of `ettn` (tests).
pub fn stored_xml(ettn: Uuid) -> Option<String> {
    store().lock().ok()?.get(&ettn).map(|d| d.xml.clone())
}

/// The mock provider.
#[derive(Debug, Clone, Default)]
pub struct MockProvider {
    registered: Vec<String>,
    fail: String,
    asynchronous: bool,
}

impl MockProvider {
    /// From an `integrations` row.
    pub fn from_config(config: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        Ok(Self {
            registered: c
                .opt_str("registered")?
                .unwrap_or_default()
                .split([',', ' ', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            fail: c.string_or("fail", "")?,
            asynchronous: c.bool_or("async", false)?,
        })
    }

    fn state(d: &Stored, ettn: Uuid) -> DocumentState {
        DocumentState {
            status: d.status,
            number: d.number.clone(),
            provider_id: Some(format!("mock-{ettn}")),
            message: None,
            ettn: None,
            cancellable_until: (d.kind == DocumentKind::EArsiv)
                .then(|| Utc::now() + ChronoDuration::days(7)),
        }
    }
}

const MOCK_PDF: &[u8] = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 595 842]>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n";

#[async_trait]
impl EInvoiceProvider for MockProvider {
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
            test_environment: true,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        Ok(ConnectionInfo {
            server_version: Some("mock".into()),
            details: Some("in-memory e-invoice provider".into()),
        })
    }

    async fn lookup_taxpayer(&self, tax_id: &str) -> Result<TaxpayerInfo, IntegrationError> {
        let registered = self.registered.iter().any(|r| r == tax_id);
        Ok(TaxpayerInfo {
            registered,
            title: registered.then(|| "Mock Taxpayer".to_owned()),
            aliases: if registered {
                vec!["urn:mail:defaultpk@mock.invalid".into()]
            } else {
                vec![]
            },
        })
    }

    async fn submit(
        &self,
        request: &EDocumentRequest,
        checkpoint: &mut Value,
    ) -> Result<DocumentState, IntegrationError> {
        match self.fail.as_str() {
            "transient" => {
                return Err(IntegrationError::Transport("mock: connection reset".into()));
            }
            "permanent" => {
                return Err(IntegrationError::InvalidInput(
                    "mock: document refused".into(),
                ));
            }
            _ => {}
        }
        let today = (Utc::now() + ChronoDuration::hours(3)).date_naive();
        let xml = ubl::to_xml(request, today)
            .map_err(|e| IntegrationError::InvalidInput(e.to_string()))?;
        let mut s = store()
            .lock()
            .map_err(|_| IntegrationError::Other("mock store poisoned".into()))?;
        let doc = s.entry(request.ettn).or_insert_with(|| Stored {
            kind: request.kind,
            number: request.number.clone(),
            status: DocumentStatus::Processing,
            xml,
            submissions: 0,
        });
        doc.submissions += 1;
        if !self.asynchronous && doc.status == DocumentStatus::Processing {
            doc.status = DocumentStatus::Issued;
        }
        *checkpoint = serde_json::json!({ "submitted": true });
        Ok(Self::state(doc, request.ettn))
    }

    async fn status(&self, document: &EDocumentRef) -> Result<DocumentState, IntegrationError> {
        let mut s = store()
            .lock()
            .map_err(|_| IntegrationError::Other("mock store poisoned".into()))?;
        let doc = s
            .get_mut(&document.ettn)
            .ok_or_else(|| IntegrationError::NotFound("mock: unknown document".into()))?;
        if doc.status == DocumentStatus::Processing {
            doc.status = DocumentStatus::Issued;
        }
        Ok(Self::state(doc, document.ettn))
    }

    async fn pdf(&self, document: &EDocumentRef) -> Result<Vec<u8>, IntegrationError> {
        let s = store()
            .lock()
            .map_err(|_| IntegrationError::Other("mock store poisoned".into()))?;
        s.get(&document.ettn)
            .map(|_| MOCK_PDF.to_vec())
            .ok_or_else(|| IntegrationError::NotFound("mock: unknown document".into()))
    }

    async fn ubl(&self, document: &EDocumentRef) -> Result<Vec<u8>, IntegrationError> {
        stored_xml(document.ettn)
            .map(String::into_bytes)
            .ok_or_else(|| IntegrationError::NotFound("mock: unknown document".into()))
    }

    async fn cancel(
        &self,
        document: &EDocumentRef,
        _reason: &str,
    ) -> Result<DocumentState, IntegrationError> {
        if document.kind != DocumentKind::EArsiv {
            return Err(IntegrationError::Unsupported("e-Fatura cancellation"));
        }
        let mut s = store()
            .lock()
            .map_err(|_| IntegrationError::Other("mock store poisoned".into()))?;
        let doc = s
            .get_mut(&document.ettn)
            .ok_or_else(|| IntegrationError::NotFound("mock: unknown document".into()))?;
        doc.status = DocumentStatus::Cancelled;
        Ok(Self::state(doc, document.ettn))
    }
}
