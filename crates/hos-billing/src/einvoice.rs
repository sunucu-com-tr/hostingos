//! Turkish e-invoicing (e-Fatura / e-Arşiv) on top of `hos-einvoice`.
//!
//! ## Flow
//!
//! * **Trigger** — with `einvoice.issue_on = payment` (default) the
//!   [`after_invoice_paid`](Billing::after_invoice_paid) hook creates the
//!   invoice's `einvoice_documents` row and queues `einvoice_process`, inside
//!   a savepoint: an e-invoice problem never rolls back or blocks a payment.
//!   With `issue_on = creation` the 10-minute sweep picks up newly issued
//!   invoices instead. The sweep also catches anything a hook missed.
//! * **One legal document per invoice** — the row is unique per invoice and
//!   purpose; the worker leases it (`locked_until`) before calling the
//!   provider; the ETTN and (for UBL providers) the legal number are fixed on
//!   the first attempt and the submitted request is frozen in the row, so a
//!   retried or duplicated job resends exactly the same document, which the
//!   provider recognises (ETTN, Paraşüt checkpoint).
//! * **e-Fatura or e-Arşiv** — a buyer with a real VKN / TCKN is looked up in
//!   GİB's e-Fatura user list through the provider; registered users get an
//!   e-Fatura (`TEMELFATURA`) to their first active alias, everybody else an
//!   e-Arşiv (`EARSIVFATURA`). Consumers without TCKN get `11111111111`,
//!   buyers abroad `2222222222`; neither is ever looked up.
//! * **Refunds / cancellations / chargebacks** (sweep) — a fully refunded or
//!   cancelled invoice whose e-Arşiv is still cancellable at the provider is
//!   cancelled there. e-Fatura documents cannot be cancelled by the seller,
//!   and partial refunds need a return invoice or a credit note that depends
//!   on the buyer: those, chargebacks and failed cancellations set
//!   `attention` and notify staff.
//! * **Reconciliation** — processing documents are polled by the sweep;
//!   issued documents are re-checked once a day for 45 days (buyer rejection
//!   of a TICARIFATURA, cancellation made in the provider's panel).
//!
//! ## Settings (`tenant_settings`)
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `einvoice.enabled` | false | issue e-documents |
//! | `einvoice.integration_id` | default `einvoice` integration | provider |
//! | `einvoice.issue_on` | `payment` | `payment` or `creation` |
//! | `einvoice.series_efatura` / `einvoice.series_earsiv` | — | 3-character series (UBL providers number the documents in HostingOS) |
//! | `einvoice.exemption_code` / `einvoice.exemption_reason` | — | KDV exemption for 0 % lines (e.g. `351`) |
//! | `einvoice.test_mode` | false | only issue through a test-environment integration |
//! | `einvoice.start_date` | date it was enabled | invoices issued before are never e-invoiced |
//! | `invoice_pdf.attach_to_emails` | false | attach the invoice PDF to invoice e-mails |
//! | `invoice_pdf.bank_details` | — | bank details printed on the PDF |

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use hos_core::CoreError;
use hos_core::domain::{IntegrationCategory, InvoiceStatus, Locale};
use hos_core::ids::{IntegrationId, InvoiceId, TenantId};
use hos_core::integrations::PostalAddress;
use hos_core::integrations::einvoice::{
    BillingReference, CONSUMER_TCKN, DocumentKind, DocumentState, DocumentStatus, EDocumentRef,
    EDocumentRequest, EInvoiceProvider, FOREIGN_VKN, InternetSale, Party, PartyId, PaymentMethod,
    Profile, TaxExemption,
};
use hos_core::money::Currency;
use hos_db::enums::{InvoiceItemType, TransactionKind, TransactionStatus};
use hos_db::repo::einvoice::{
    self as repo, DocumentUpdate, EInvoiceDocumentId, EInvoiceDocumentRow,
};
use hos_db::repo::{billing_support, catalog, invoices, payments, platform};
/// Exemption / withholding codes and series (re-exported for the web forms).
pub use hos_einvoice::codes;
use hos_einvoice::codes::{Series, legal_number};
use hos_einvoice::compose::{SourceInvoice, SourceLine, SourceTax, compose};
/// VKN / TCKN validation (re-exported for the web forms).
pub use hos_einvoice::taxid;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::jobs::{self, JobKind, JobRequest};
use crate::{Billing, BillingResult};

/// Purpose of a sale document.
pub const PURPOSE_SALE: &str = "sale";
/// Lease taken while talking to a provider.
const LEASE_SECS: i32 = 180;
/// Türkiye is UTC+3 all year.
const TR_OFFSET_HOURS: i64 = 3;

/// Setting keys.
pub mod keys {
    /// Master switch.
    pub const ENABLED: &str = "einvoice.enabled";
    /// Integration id.
    pub const INTEGRATION_ID: &str = "einvoice.integration_id";
    /// `payment` / `creation`.
    pub const ISSUE_ON: &str = "einvoice.issue_on";
    /// e-Fatura series.
    pub const SERIES_EFATURA: &str = "einvoice.series_efatura";
    /// e-Arşiv series.
    pub const SERIES_EARSIV: &str = "einvoice.series_earsiv";
    /// Default exemption code.
    pub const EXEMPTION_CODE: &str = "einvoice.exemption_code";
    /// Default exemption reason.
    pub const EXEMPTION_REASON: &str = "einvoice.exemption_reason";
    /// Test mode.
    pub const TEST_MODE: &str = "einvoice.test_mode";
    /// First issue date that is e-invoiced.
    pub const START_DATE: &str = "einvoice.start_date";
    /// Attach the PDF to invoice e-mails.
    pub const ATTACH_PDF: &str = "invoice_pdf.attach_to_emails";
    /// Bank details on the PDF.
    pub const BANK_DETAILS: &str = "invoice_pdf.bank_details";
}

/// When documents are issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueOn {
    /// When the invoice is fully paid.
    Payment,
    /// As soon as the invoice is issued (numbered).
    Creation,
}

/// Parsed e-invoice settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EInvoiceSettings {
    pub enabled: bool,
    pub integration_id: Option<IntegrationId>,
    pub issue_on: IssueOn,
    pub series_efatura: Option<Series>,
    pub series_earsiv: Option<Series>,
    pub exemption: Option<TaxExemption>,
    pub test_mode: bool,
    pub start_date: Option<NaiveDate>,
    pub attach_pdf: bool,
    pub bank_details: String,
}

impl EInvoiceSettings {
    /// From `tenant_settings` (all keys).
    pub fn from_map(map: &BTreeMap<String, Value>) -> Self {
        let s = |k: &str| {
            map.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
        };
        let b = |k: &str| match map.get(k) {
            Some(Value::Bool(v)) => *v,
            Some(Value::String(v)) => matches!(v.as_str(), "true" | "1" | "on"),
            _ => false,
        };
        let code = s(keys::EXEMPTION_CODE);
        Self {
            enabled: b(keys::ENABLED),
            integration_id: s(keys::INTEGRATION_ID).parse().ok(),
            issue_on: if s(keys::ISSUE_ON) == "creation" {
                IssueOn::Creation
            } else {
                IssueOn::Payment
            },
            series_efatura: Series::parse(s(keys::SERIES_EFATURA)),
            series_earsiv: Series::parse(s(keys::SERIES_EARSIV)),
            exemption: (!code.is_empty()).then(|| TaxExemption {
                code: code.to_owned(),
                reason: s(keys::EXEMPTION_REASON).to_owned(),
            }),
            test_mode: b(keys::TEST_MODE),
            start_date: s(keys::START_DATE).parse().ok(),
            attach_pdf: b(keys::ATTACH_PDF),
            bank_details: s(keys::BANK_DETAILS).to_owned(),
        }
    }

    /// Loads the settings of a tenant.
    pub async fn load(conn: &mut PgConnection, tenant_id: TenantId) -> BillingResult<Self> {
        let mut map = platform::get_settings(&mut *conn, tenant_id, "einvoice.").await?;
        map.extend(platform::get_settings(&mut *conn, tenant_id, "invoice_pdf.").await?);
        Ok(Self::from_map(&map))
    }
}

/// What a worker run should do with a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EInvoiceAction {
    /// Build (first time) and submit / continue.
    Submit,
    /// Re-read the status of a final document.
    Refresh,
    /// Cancel an issued e-Arşiv.
    Cancel,
}

/// Payload of [`JobKind::EinvoiceProcess`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EInvoicePayload {
    pub document_id: EInvoiceDocumentId,
    pub action: EInvoiceAction,
}

fn status_str(s: DocumentStatus) -> &'static str {
    match s {
        DocumentStatus::Processing => "processing",
        DocumentStatus::Issued => "issued",
        DocumentStatus::Rejected => "rejected",
        DocumentStatus::Failed => "failed",
        DocumentStatus::Cancelled => "cancelled",
    }
}

fn tr_now(now: DateTime<Utc>) -> chrono::NaiveDateTime {
    (now + ChronoDuration::hours(TR_OFFSET_HOURS)).naive_utc()
}

fn short(text: &str, max: usize) -> String {
    let t: String = text.chars().filter(|c| !c.is_control()).collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max {
        t
    } else {
        let mut s: String = t.chars().take(max).collect();
        s.push('…');
        s
    }
}

/// Error a document build can end in (shown to staff, never contains tax ids).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildError {
    /// Nothing to issue (zero total, add-funds invoice).
    Skip(String),
    /// Staff must fix data / settings.
    Fail(String),
}

fn text(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned()
}

/// The buyer from the invoice's billing snapshot.
fn buyer(snapshot: &Value) -> Result<Party, BuildError> {
    let digits = |s: &str| taxid::normalize(s).unwrap_or_default();
    let country = {
        let c = text(snapshot, "country_code").to_ascii_uppercase();
        if c.len() == 2 { c } else { "TR".to_owned() }
    };
    let company = text(snapshot, "company_name");
    let person = text(snapshot, "name");
    let corporate = text(snapshot, "kind") == "corporate";
    let tax_number = digits(&text(snapshot, "tax_number"));
    let national_id = digits(&text(snapshot, "national_id"));
    let (first, family) = match person.rsplit_once(' ') {
        Some((f, l)) => (f.trim().to_owned(), l.trim().to_owned()),
        None => (person.clone(), String::new()),
    };
    let id = if country != "TR" {
        PartyId::Vkn(FOREIGN_VKN.to_owned())
    } else if corporate && !tax_number.is_empty() {
        match tax_number.len() {
            10 if taxid::is_valid_vkn(&tax_number) => PartyId::Vkn(tax_number),
            11 if taxid::is_valid_tckn(&tax_number) => PartyId::Tckn(tax_number),
            _ => {
                return Err(BuildError::Fail(
                    "the client's tax number (VKN / TCKN) is not valid".into(),
                ));
            }
        }
    } else if !national_id.is_empty() {
        if !taxid::is_valid_tckn(&national_id) {
            return Err(BuildError::Fail(
                "the client's T.C. identity number is not valid".into(),
            ));
        }
        PartyId::Tckn(national_id)
    } else if corporate {
        return Err(BuildError::Fail(
            "a corporate client needs a tax number (VKN)".into(),
        ));
    } else {
        PartyId::Tckn(CONSUMER_TCKN.to_owned())
    };
    let title = match &id {
        PartyId::Vkn(_) if !company.is_empty() => company.clone(),
        _ if !person.is_empty() => person.clone(),
        _ => company.clone(),
    };
    if title.is_empty() {
        return Err(BuildError::Fail("the client has no name".into()));
    }
    let is_person = matches!(id, PartyId::Tckn(_));
    if is_person && (first.is_empty() || family.is_empty()) {
        return Err(BuildError::Fail(
            "a person needs first and last name for an e-document".into(),
        ));
    }
    let city = text(snapshot, "city");
    let state = text(snapshot, "state");
    if city.is_empty() && state.is_empty() {
        return Err(BuildError::Fail(
            "the client's billing address has no city".into(),
        ));
    }
    Ok(Party {
        id,
        name: short(&title, 200),
        first_name: is_person.then(|| short(&first, 100)),
        family_name: is_person.then(|| short(&family, 100)),
        tax_office: Some(text(snapshot, "tax_office")).filter(|s| !s.is_empty() && corporate),
        address: PostalAddress {
            line1: short(&text(snapshot, "address_line1"), 200),
            line2: Some(short(&text(snapshot, "address_line2"), 200)).filter(|s| !s.is_empty()),
            city: if city.is_empty() {
                state.clone()
            } else {
                short(&city, 100)
            },
            state: Some(short(&state, 100)).filter(|s| !s.is_empty()),
            postal_code: Some(text(snapshot, "postal_code")).filter(|s| !s.is_empty()),
            country_code: country.clone(),
        },
        country_name: country_name(&country),
        email: Some(text(snapshot, "email")).filter(|s| s.contains('@')),
        phone: Some(text(snapshot, "phone")).filter(|s| !s.is_empty()),
    })
}

fn country_name(code: &str) -> String {
    match code {
        "TR" => "Türkiye".to_owned(),
        other => other.to_owned(),
    }
}

/// The seller (tenant company).
fn seller(t: &platform::TenantRow) -> Result<Party, BuildError> {
    let number = taxid::normalize(t.tax_number.as_deref().unwrap_or("")).unwrap_or_default();
    let name = t
        .legal_name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| t.name.clone());
    let id = match number.len() {
        10 if taxid::is_valid_vkn(&number) => PartyId::Vkn(number),
        11 if taxid::is_valid_tckn(&number) => PartyId::Tckn(number),
        _ => {
            return Err(BuildError::Fail(
                "set a valid company tax number (VKN / TCKN) in Settings → Company".into(),
            ));
        }
    };
    let office = t.tax_office.clone().unwrap_or_default();
    if office.trim().is_empty() {
        return Err(BuildError::Fail(
            "set the company's tax office in Settings → Company".into(),
        ));
    }
    let a = |k: &str| text(&t.address, k);
    let (first, family) = match name.trim().rsplit_once(' ') {
        Some((f, l)) => (Some(f.to_owned()), Some(l.to_owned())),
        None => (None, None),
    };
    let is_person = matches!(id, PartyId::Tckn(_));
    let city = a("city");
    if city.is_empty() {
        return Err(BuildError::Fail(
            "set the company's city in Settings → Company".into(),
        ));
    }
    Ok(Party {
        id,
        name: short(&name, 200),
        first_name: if is_person { first } else { None },
        family_name: if is_person { family } else { None },
        tax_office: Some(office),
        address: PostalAddress {
            line1: a("line1"),
            line2: None,
            city,
            state: Some(a("state")).filter(|s| !s.is_empty()),
            postal_code: Some(a("postal_code")).filter(|s| !s.is_empty()),
            country_code: "TR".into(),
        },
        country_name: "Türkiye".into(),
        email: t.contact_email.clone(),
        phone: t.contact_phone.clone(),
    })
}

fn payment_method_of(gateway: &str) -> PaymentMethod {
    let g = gateway.to_ascii_lowercase();
    if g.contains("bank") || g.contains("transfer") || g.contains("havale") || g == "manual" {
        PaymentMethod::BankTransfer
    } else if g == "credit" {
        PaymentMethod::Other
    } else if g.contains("paypal") || g.contains("papara") {
        PaymentMethod::Intermediary
    } else {
        PaymentMethod::Card
    }
}

impl Billing {
    // -----------------------------------------------------------------------
    // triggers
    // -----------------------------------------------------------------------

    /// `after_invoice_paid` hook: queues the e-document of a paid invoice.
    /// Runs in a savepoint of the caller's transaction and never fails.
    pub(crate) async fn einvoice_on_paid(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
    ) {
        if let Err(e) = sqlx::query("savepoint hos_einvoice_hook")
            .execute(&mut *conn)
            .await
        {
            tracing::warn!(%tenant_id, error = %e, "e-invoice hook: savepoint failed");
            return;
        }
        let res = self
            .einvoice_on_paid_inner(conn, tenant_id, invoice_id)
            .await;
        let sql = if res.is_ok() {
            "release savepoint hos_einvoice_hook"
        } else {
            "rollback to savepoint hos_einvoice_hook"
        };
        if let Err(e) = res {
            tracing::warn!(%tenant_id, %invoice_id, error = %e, "e-invoice hook failed; the sweep will retry");
        }
        if let Err(e) = sqlx::query(sql).execute(&mut *conn).await {
            tracing::warn!(%tenant_id, error = %e, "e-invoice hook: savepoint cleanup failed");
        }
    }

    async fn einvoice_on_paid_inner(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
    ) -> BillingResult<()> {
        let settings = EInvoiceSettings::load(conn, tenant_id).await?;
        if !settings.enabled || settings.issue_on != IssueOn::Payment {
            return Ok(());
        }
        let Some(invoice) = invoices::get_invoice(&mut *conn, tenant_id, invoice_id).await? else {
            return Ok(());
        };
        if settings.start_date.is_some_and(|d| invoice.issue_date < d) {
            return Ok(());
        }
        let (doc, _) = repo::ensure_document(
            conn,
            tenant_id,
            invoice_id,
            PURPOSE_SALE,
            settings.integration_id,
        )
        .await?;
        if doc.status == "pending" {
            self.einvoice_enqueue(conn, tenant_id, doc.id, EInvoiceAction::Submit)
                .await?;
        }
        Ok(())
    }

    /// Queues a worker run for a document (deduplicated while queued).
    pub async fn einvoice_enqueue(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        document_id: EInvoiceDocumentId,
        action: EInvoiceAction,
    ) -> BillingResult<()> {
        let key = format!(
            "einvoice:{document_id}:{}",
            serde_json::to_value(action)
                .unwrap_or_default()
                .as_str()
                .unwrap_or("x")
        );
        jobs::enqueue(
            conn,
            JobRequest::new(
                JobKind::EinvoiceProcess,
                Some(tenant_id),
                &EInvoicePayload {
                    document_id,
                    action,
                },
            )
            .dedupe(key),
        )
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // staff actions
    // -----------------------------------------------------------------------

    /// "Issue now": creates the document if needed and queues it; a failed
    /// document is reset for another attempt.
    pub async fn einvoice_issue_now(
        &self,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
    ) -> BillingResult<EInvoiceDocumentRow> {
        let mut tx = self.pool().begin().await?;
        let settings = EInvoiceSettings::load(&mut tx, tenant_id).await?;
        if !settings.enabled {
            return Err(CoreError::validation("e-invoicing is not enabled"));
        }
        let invoice = invoices::get_invoice(&mut *tx, tenant_id, invoice_id)
            .await?
            .ok_or_else(|| CoreError::not_found("invoice", invoice_id))?;
        if invoice.number.is_none()
            || matches!(
                invoice.status,
                InvoiceStatus::Draft | InvoiceStatus::Cancelled
            )
        {
            return Err(CoreError::validation(
                "only issued invoices get an e-document",
            ));
        }
        let (doc, _) = repo::ensure_document(
            &mut tx,
            tenant_id,
            invoice_id,
            PURPOSE_SALE,
            settings.integration_id,
        )
        .await?;
        if doc.status == "failed" {
            repo::reset_for_retry(&mut tx, tenant_id, doc.id).await?;
        }
        self.einvoice_enqueue(&mut tx, tenant_id, doc.id, EInvoiceAction::Submit)
            .await?;
        tx.commit().await?;
        let mut conn = self.pool().acquire().await?;
        Ok(repo::get_document(&mut conn, tenant_id, doc.id)
            .await?
            .unwrap_or(doc))
    }

    /// Staff "retry" of a failed document.
    pub async fn einvoice_retry(
        &self,
        tenant_id: TenantId,
        document_id: EInvoiceDocumentId,
    ) -> BillingResult<()> {
        let mut tx = self.pool().begin().await?;
        if !repo::reset_for_retry(&mut tx, tenant_id, document_id).await? {
            return Err(CoreError::validation(
                "only failed or pending documents that are not being sent can be retried",
            ));
        }
        self.einvoice_enqueue(&mut tx, tenant_id, document_id, EInvoiceAction::Submit)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Staff "cancel" of an issued e-Arşiv (queued).
    pub async fn einvoice_request_cancel(
        &self,
        tenant_id: TenantId,
        document_id: EInvoiceDocumentId,
    ) -> BillingResult<()> {
        let mut tx = self.pool().begin().await?;
        let doc = repo::get_document(&mut tx, tenant_id, document_id)
            .await?
            .ok_or_else(|| CoreError::not_found("e-document", document_id))?;
        if doc.status != "issued" || doc.kind.as_deref() != Some("earsiv") {
            return Err(CoreError::validation(
                "only issued e-Arşiv documents can be cancelled; an e-Fatura is answered or returned by the buyer",
            ));
        }
        self.einvoice_enqueue(&mut tx, tenant_id, document_id, EInvoiceAction::Cancel)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Staff "refresh status" (queued).
    pub async fn einvoice_request_refresh(
        &self,
        tenant_id: TenantId,
        document_id: EInvoiceDocumentId,
    ) -> BillingResult<()> {
        let mut tx = self.pool().begin().await?;
        repo::get_document(&mut tx, tenant_id, document_id)
            .await?
            .ok_or_else(|| CoreError::not_found("e-document", document_id))?;
        self.einvoice_enqueue(&mut tx, tenant_id, document_id, EInvoiceAction::Refresh)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Clears the staff flag of a document.
    pub async fn einvoice_acknowledge(
        &self,
        tenant_id: TenantId,
        document_id: EInvoiceDocumentId,
    ) -> BillingResult<()> {
        let mut conn = self.pool().acquire().await?;
        repo::set_attention(&mut conn, tenant_id, document_id, None).await?;
        Ok(())
    }

    /// The legal PDF (`pdf = true`) or the signed UBL of an issued document.
    pub async fn einvoice_file(
        &self,
        tenant_id: TenantId,
        document_id: EInvoiceDocumentId,
        pdf: bool,
    ) -> BillingResult<(Vec<u8>, String)> {
        let mut conn = self.pool().acquire().await?;
        let doc = repo::get_document(&mut conn, tenant_id, document_id)
            .await?
            .ok_or_else(|| CoreError::not_found("e-document", document_id))?;
        drop(conn);
        if !matches!(doc.status.as_str(), "issued" | "cancelled" | "rejected") {
            return Err(CoreError::validation("the e-document is not issued yet"));
        }
        let provider = self
            .einvoice_provider(tenant_id, doc.integration_id)
            .await?
            .1;
        let r = doc_ref(&doc)?;
        let data = if pdf {
            provider.pdf(&r).await?
        } else {
            provider.ubl(&r).await?
        };
        let name = doc.number.clone().unwrap_or_else(|| doc.ettn.to_string());
        let name: String = name.chars().filter(char::is_ascii_alphanumeric).collect();
        Ok((data, format!("{name}.{}", if pdf { "pdf" } else { "xml" })))
    }

    async fn einvoice_provider(
        &self,
        tenant_id: TenantId,
        integration_id: Option<IntegrationId>,
    ) -> BillingResult<(IntegrationId, Arc<dyn EInvoiceProvider>)> {
        let id = match integration_id {
            Some(id) => id,
            None => self
                .registry()
                .default_id(tenant_id, IntegrationCategory::Einvoice)
                .await?
                .ok_or_else(|| CoreError::validation("no e-invoice integration is configured"))?,
        };
        Ok((id, self.registry().einvoice(tenant_id, id).await?))
    }

    // -----------------------------------------------------------------------
    // worker
    // -----------------------------------------------------------------------

    /// One worker step for a document (`einvoice_process` job). Retryable
    /// provider errors are returned (the job retries); everything else is
    /// recorded on the document.
    pub async fn einvoice_process(
        &self,
        tenant_id: TenantId,
        document_id: EInvoiceDocumentId,
        action: EInvoiceAction,
        final_attempt: bool,
    ) -> BillingResult<Value> {
        let mut conn = self.pool().acquire().await?;
        let claimed = match action {
            EInvoiceAction::Submit => {
                repo::claim(&mut conn, tenant_id, document_id, LEASE_SECS).await?
            }
            _ => repo::claim_any(&mut conn, tenant_id, document_id, LEASE_SECS).await?,
        };
        let Some(doc) = claimed else {
            return Ok(json!({"skipped": "document busy or already final"}));
        };
        drop(conn);
        let res = match action {
            EInvoiceAction::Submit => {
                self.einvoice_submit(tenant_id, doc.clone(), final_attempt)
                    .await
            }
            EInvoiceAction::Refresh => self.einvoice_refresh(tenant_id, doc.clone()).await,
            EInvoiceAction::Cancel => self.einvoice_cancel(tenant_id, doc.clone()).await,
        };
        if res.is_err() {
            let mut conn = self.pool().acquire().await?;
            repo::release(&mut conn, tenant_id, doc.id).await?;
        }
        res
    }

    async fn einvoice_submit(
        &self,
        tenant_id: TenantId,
        doc: EInvoiceDocumentRow,
        final_attempt: bool,
    ) -> BillingResult<Value> {
        let mut conn = self.pool().acquire().await?;
        let settings = EInvoiceSettings::load(&mut conn, tenant_id).await?;
        let mut update = DocumentUpdate {
            status: doc.status.clone(),
            checkpoint: doc.checkpoint.clone(),
            is_test: doc.is_test,
            ..DocumentUpdate::default()
        };
        if !settings.enabled {
            repo::release(&mut conn, tenant_id, doc.id).await?;
            return Ok(json!({"skipped": "e-invoicing disabled"}));
        }
        let (integration_id, provider) = match self
            .einvoice_provider(tenant_id, doc.integration_id.or(settings.integration_id))
            .await
        {
            Ok(p) => p,
            Err(e) => {
                return self
                    .einvoice_fail(&mut conn, tenant_id, &doc, update, &e.to_string())
                    .await;
            }
        };
        update.integration_id = Some(integration_id);
        let caps = provider.capabilities();
        if settings.test_mode && !caps.test_environment {
            return self
                .einvoice_fail(
                    &mut conn,
                    tenant_id,
                    &doc,
                    update,
                    "test mode is on but the e-invoice integration is a live account",
                )
                .await;
        }
        update.is_test = caps.test_environment;

        // Build (first attempt) or reuse the frozen request.
        let (mut request, to_number): (EDocumentRequest, Option<(Series, i32)>) = match &doc.request
        {
            Some(v) => (
                serde_json::from_value(v.clone())
                    .map_err(|e| CoreError::Internal(format!("stored e-document request: {e}")))?,
                None,
            ),
            None => {
                let built = self
                    .einvoice_build(&mut conn, tenant_id, &doc, &settings, provider.as_ref())
                    .await;
                match built {
                    Ok(r) => r,
                    Err(BuildError::Skip(why)) => {
                        update.status = "skipped".into();
                        update.message = Some(why);
                        repo::save(&mut conn, tenant_id, doc.id, &update).await?;
                        return Ok(json!({"status": "skipped"}));
                    }
                    Err(BuildError::Fail(why)) => {
                        return self
                            .einvoice_fail(&mut conn, tenant_id, &doc, update, &why)
                            .await;
                    }
                }
            }
        };
        update.kind = Some(request.kind.as_str().to_owned());
        update.profile = Some(request.profile.as_str().to_owned());
        update.type_code = Some(request.type_code.as_str().to_owned());
        // Persist the frozen request before anything leaves HostingOS, in one
        // transaction with the legal number: a number is consumed only
        // together with the request that carries it (no gap when the build
        // or this step fails), and the lease is kept throughout (no second
        // worker can submit in between).
        if doc.request.is_none() {
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            if let Some((series, year)) = to_number {
                let seq = repo::allocate_number(&mut tx, tenant_id, series.as_str(), year).await?;
                let Some(n) = legal_number(&series, year, seq) else {
                    drop(tx);
                    return self
                        .einvoice_fail(
                            &mut conn,
                            tenant_id,
                            &doc,
                            update,
                            "the document sequence is exhausted for this year",
                        )
                        .await;
                };
                repo::set_number(&mut tx, tenant_id, doc.id, series.as_str(), &n).await?;
                request.number = Some(n);
            }
            update.request = Some(
                serde_json::to_value(&request).map_err(|e| CoreError::Internal(e.to_string()))?,
            );
            let mut first = update.clone();
            first.status = "pending".into();
            repo::save(&mut tx, tenant_id, doc.id, &first).await?;
            if repo::claim(&mut tx, tenant_id, doc.id, LEASE_SECS)
                .await?
                .is_none()
            {
                return Err(CoreError::Internal(
                    "the e-document lease was lost while freezing the request".into(),
                ));
            }
            tx.commit().await?;
        } else {
            update.request = Some(
                serde_json::to_value(&request).map_err(|e| CoreError::Internal(e.to_string()))?,
            );
        }
        let mut checkpoint = if doc.checkpoint.is_object() {
            doc.checkpoint.clone()
        } else {
            json!({})
        };
        let result = provider.submit(&request, &mut checkpoint).await;
        update.checkpoint = checkpoint;
        update.attempted = true;
        match result {
            Ok(state) => {
                self.einvoice_apply(&mut conn, tenant_id, &doc, update, state)
                    .await
            }
            Err(e) if e.is_retryable() => {
                update.status = "pending".into();
                update.last_error = Some(short(&e.summary(), 500));
                repo::save(&mut conn, tenant_id, doc.id, &update).await?;
                if final_attempt {
                    self.einvoice_alert(&mut conn, tenant_id, &doc, &e.summary())
                        .await;
                }
                Err(e.into())
            }
            Err(e) => {
                self.einvoice_fail(&mut conn, tenant_id, &doc, update, &e.summary())
                    .await
            }
        }
    }

    async fn einvoice_apply(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        doc: &EInvoiceDocumentRow,
        mut update: DocumentUpdate,
        state: DocumentState,
    ) -> BillingResult<Value> {
        update.status = status_str(state.status).to_owned();
        update.provider_id = state.provider_id.clone();
        update.cancellable_until = state.cancellable_until;
        update.message = state.message.as_deref().map(|m| short(m, 500));
        if let Some(n) = state
            .number
            .clone()
            .filter(|n| hos_einvoice::codes::is_legal_number(n))
        {
            update.number = Some(n);
        }
        update.ettn = state.ettn;
        update.last_error = None;
        let row = repo::save(conn, tenant_id, doc.id, &update).await?;
        match state.status {
            DocumentStatus::Issued => {
                // A test-environment document is not a legal reference.
                if let Some(n) = row.number.as_ref().filter(|_| !row.is_test) {
                    repo::set_invoice_external_ref(conn, tenant_id, row.invoice_id, n).await?;
                }
            }
            DocumentStatus::Failed => {
                let msg = row
                    .message
                    .clone()
                    .unwrap_or_else(|| "the provider refused the document".into());
                self.einvoice_alert(conn, tenant_id, &row, &msg).await;
            }
            DocumentStatus::Rejected => {
                let msg =
                    "the buyer rejected the e-Fatura (TICARIFATURA); issue a corrected invoice";
                repo::set_attention(conn, tenant_id, row.id, Some(msg)).await?;
                self.einvoice_alert(conn, tenant_id, &row, msg).await;
            }
            _ => {}
        }
        Ok(json!({"status": row.status, "number": row.number}))
    }

    async fn einvoice_fail(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        doc: &EInvoiceDocumentRow,
        mut update: DocumentUpdate,
        why: &str,
    ) -> BillingResult<Value> {
        update.status = if doc.status == "processing" {
            "processing".into()
        } else {
            "failed".into()
        };
        update.last_error = Some(short(why, 500));
        repo::save(conn, tenant_id, doc.id, &update).await?;
        self.einvoice_alert(conn, tenant_id, doc, why).await;
        Ok(json!({"status": update.status, "error": update.last_error}))
    }

    /// Notifies staff once per document and problem.
    async fn einvoice_alert(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        doc: &EInvoiceDocumentRow,
        why: &str,
    ) {
        let key = format!("einvoice_alert:{}:{}", doc.id, short(why, 60));
        let res: BillingResult<()> = async {
            if !billing_support::mark_once(&mut *conn, tenant_id, &key).await? {
                return Ok(());
            }
            let locale = platform::get_tenant(&mut *conn, tenant_id)
                .await?
                .map_or(Locale::Tr, |t| t.default_locale);
            let title = match locale {
                Locale::Tr => "e-Fatura / e-Arşiv belgesi işlem bekliyor",
                Locale::En => "An e-invoice document needs attention",
            };
            self.notify_staff(
                conn,
                tenant_id,
                "einvoice",
                title,
                Some(&short(why, 300)),
                Some(&format!("/admin/invoices/{}", doc.invoice_id)),
                &json!({"document_id": doc.id, "invoice_id": doc.invoice_id}),
            )
            .await?;
            Ok(())
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(%tenant_id, error = %e, "e-invoice staff notification failed");
        }
    }

    async fn einvoice_refresh(
        &self,
        tenant_id: TenantId,
        doc: EInvoiceDocumentRow,
    ) -> BillingResult<Value> {
        let mut conn = self.pool().acquire().await?;
        if doc.request.is_none() || doc.kind.is_none() {
            repo::release(&mut conn, tenant_id, doc.id).await?;
            return Ok(json!({"skipped": "never submitted"}));
        }
        let provider = self
            .einvoice_provider(tenant_id, doc.integration_id)
            .await?
            .1;
        let state = provider.status(&doc_ref(&doc)?).await?;
        let update = DocumentUpdate {
            status: doc.status.clone(),
            checkpoint: doc.checkpoint.clone(),
            is_test: doc.is_test,
            ..DocumentUpdate::default()
        };
        if state.status == DocumentStatus::Failed && doc.status == "issued" {
            // A final document never goes back to failed on a status read.
            repo::release(&mut conn, tenant_id, doc.id).await?;
            return Ok(json!({"status": doc.status}));
        }
        self.einvoice_apply(&mut conn, tenant_id, &doc, update, state)
            .await
    }

    async fn einvoice_cancel(
        &self,
        tenant_id: TenantId,
        doc: EInvoiceDocumentRow,
    ) -> BillingResult<Value> {
        let mut conn = self.pool().acquire().await?;
        if doc.status != "issued" {
            repo::release(&mut conn, tenant_id, doc.id).await?;
            return Ok(json!({"skipped": "not issued"}));
        }
        let provider = self
            .einvoice_provider(tenant_id, doc.integration_id)
            .await?
            .1;
        let too_late = doc
            .cancellable_until
            .is_some_and(|t| t < self.clock().now());
        let outcome = if doc.kind.as_deref() != Some("earsiv") {
            Err("e-Fatura documents cannot be cancelled by the seller; the buyer must reject or return them".to_owned())
        } else if !provider.capabilities().cancel_earsiv {
            Err("this provider does not cancel e-Arşiv documents through its API; cancel it in the provider panel".to_owned())
        } else if too_late {
            Err(
                "the provider's cancellation window has passed; a return invoice or note is needed"
                    .to_owned(),
            )
        } else {
            match provider.cancel(&doc_ref(&doc)?, "refund").await {
                Ok(s) if s.status == DocumentStatus::Cancelled => Ok(s),
                Ok(_) => Err("the provider did not confirm the cancellation".to_owned()),
                Err(e) if e.is_retryable() => {
                    repo::release(&mut conn, tenant_id, doc.id).await?;
                    return Err(e.into());
                }
                Err(e) => Err(e.summary()),
            }
        };
        match outcome {
            Ok(state) => {
                let update = DocumentUpdate {
                    status: doc.status.clone(),
                    checkpoint: doc.checkpoint.clone(),
                    is_test: doc.is_test,
                    ..DocumentUpdate::default()
                };
                let res = self
                    .einvoice_apply(&mut conn, tenant_id, &doc, update, state)
                    .await?;
                repo::set_attention(&mut conn, tenant_id, doc.id, None).await?;
                Ok(res)
            }
            Err(why) => {
                repo::release(&mut conn, tenant_id, doc.id).await?;
                repo::set_attention(&mut conn, tenant_id, doc.id, Some(&why)).await?;
                self.einvoice_alert(&mut conn, tenant_id, &doc, &why).await;
                Ok(json!({"status": doc.status, "attention": why}))
            }
        }
    }

    // -----------------------------------------------------------------------
    // building the document
    // -----------------------------------------------------------------------

    async fn einvoice_build(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        doc: &EInvoiceDocumentRow,
        settings: &EInvoiceSettings,
        provider: &dyn EInvoiceProvider,
    ) -> Result<(EDocumentRequest, Option<(Series, i32)>), BuildError> {
        let internal =
            |e: CoreError| BuildError::Fail(format!("internal error: {}", e.integration_summary()));
        let invoice = invoices::get_invoice(&mut *conn, tenant_id, doc.invoice_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| BuildError::Fail("the invoice no longer exists".into()))?;
        if matches!(
            invoice.status,
            InvoiceStatus::Draft | InvoiceStatus::Cancelled
        ) || invoice.number.is_none()
        {
            return Err(BuildError::Skip(
                "the invoice is a draft or cancelled".into(),
            ));
        }
        let items = invoices::invoice_items(&mut *conn, tenant_id, invoice.id)
            .await
            .map_err(internal)?;
        if items.iter().all(|i| i.item_type == InvoiceItemType::Credit) {
            return Err(BuildError::Skip(
                "add-funds (credit) invoices are not e-invoiced".into(),
            ));
        }
        if items.iter().any(|i| {
            i.item_type == InvoiceItemType::Credit && i.amount > rust_decimal::Decimal::ZERO
        }) {
            return Err(BuildError::Fail(
                "the invoice mixes add-funds credit with sales; issue the e-document manually"
                    .into(),
            ));
        }
        if invoice.total <= rust_decimal::Decimal::ZERO {
            return Err(BuildError::Skip(
                "zero-amount invoices are not e-invoiced".into(),
            ));
        }
        let taxes = invoices::invoice_tax_lines(&mut *conn, tenant_id, invoice.id)
            .await
            .map_err(internal)?;
        let src = SourceInvoice {
            currency: invoice.currency,
            lines: items
                .iter()
                .map(|i| SourceLine {
                    description: short(&i.description, 300),
                    quantity: i.quantity,
                    unit_price: i.unit_price,
                    amount: i.amount,
                    taxable: i.taxable,
                })
                .collect(),
            taxes: taxes
                .iter()
                .map(|t| SourceTax {
                    rate: t.rate,
                    compound: t.is_compound,
                    taxable_amount: t.taxable_amount,
                    amount: t.tax_amount,
                })
                .collect(),
            subtotal: invoice.subtotal,
            tax_total: invoice.tax_total,
            total: invoice.total,
        };
        let composed = compose(&src, settings.exemption.as_ref())
            .map_err(|e| BuildError::Fail(e.to_string()))?;
        let tenant = platform::get_tenant(&mut *conn, tenant_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| BuildError::Fail("tenant not found".into()))?;
        let supplier = seller(&tenant)?;
        let customer = buyer(&invoice.billing_snapshot)?;

        // e-Fatura or e-Arşiv.
        let caps = provider.capabilities();
        let (kind, profile, alias) = if customer.id.is_placeholder() {
            (DocumentKind::EArsiv, Profile::EArsivFatura, None)
        } else {
            match provider.lookup_taxpayer(customer.id.value()).await {
                Ok(info) if info.registered => match info.aliases.first() {
                    Some(a) => (DocumentKind::EFatura, Profile::TemelFatura, Some(a.clone())),
                    None => {
                        return Err(BuildError::Fail(
                            "the buyer is an e-Fatura user without an active alias".into(),
                        ));
                    }
                },
                Ok(_) => (DocumentKind::EArsiv, Profile::EArsivFatura, None),
                Err(e) => {
                    return Err(BuildError::Fail(format!(
                        "e-Fatura user lookup failed: {}",
                        e.summary()
                    )));
                }
            }
        };
        if invoice.currency != Currency::TRY && !caps.foreign_currency {
            return Err(BuildError::Fail(
                "the provider only accepts TRY documents".into(),
            ));
        }
        let exchange_rate = if invoice.currency == Currency::TRY {
            None
        } else {
            let rates = catalog::list_currencies(&mut *conn, tenant_id, false)
                .await
                .map_err(internal)?;
            let rate = |c: Currency| rates.iter().find(|r| r.code == c).map(|r| r.exchange_rate);
            match (rate(Currency::TRY), rate(invoice.currency)) {
                (Some(t), Some(x)) if x > rust_decimal::Decimal::ZERO => Some((t / x).round_dp(6)),
                _ => {
                    return Err(BuildError::Fail(
                        "no TRY exchange rate is configured for the invoice currency".into(),
                    ));
                }
            }
        };
        let now = self.clock().now();
        let local = tr_now(now);
        let issue_date = local.date();

        // Legal number (UBL providers): a number already on the document is
        // kept; otherwise the series / year to allocate from when the request
        // is frozen (`einvoice_submit`, same transaction).
        let (number, to_number) = if caps.assigns_numbers {
            (None, None)
        } else if let Some(n) = doc.number.clone() {
            (Some(n), None)
        } else {
            let series = match kind {
                DocumentKind::EFatura => settings.series_efatura.clone(),
                DocumentKind::EArsiv => settings.series_earsiv.clone(),
            }
            .ok_or_else(|| {
                BuildError::Fail(
                    "set the e-Fatura / e-Arşiv series in the e-invoice settings".into(),
                )
            })?;
            (None, Some((series, chrono::Datelike::year(&issue_date))))
        };

        // Internet sale (e-Arşiv, paid online).
        let internet_sale = if kind == DocumentKind::EArsiv {
            let txns = payments::invoice_transactions(&mut *conn, tenant_id, invoice.id)
                .await
                .map_err(internal)?;
            let paid = txns
                .iter()
                .filter(|t| {
                    t.kind == TransactionKind::Payment && t.status == TransactionStatus::Succeeded
                })
                .max_by_key(|t| t.occurred_at);
            match paid {
                Some(t) => Some(InternetSale {
                    website: self.tenant_url(tenant_id).await.map_err(internal)?,
                    payment_method: payment_method_of(&t.gateway),
                    payment_platform: Some(short(&t.gateway, 60)).filter(|g| !g.is_empty()),
                    payment_date: tr_now(t.occurred_at).date(),
                }),
                None => None,
            }
        } else {
            None
        };
        let mut notes = Vec::new();
        if let Some(n) = invoice
            .notes
            .as_deref()
            .map(|n| short(n, 500))
            .filter(|n| !n.is_empty())
        {
            notes.push(n);
        }
        let delivery_email = customer.email.clone();
        let request = EDocumentRequest {
            ettn: doc.ettn,
            number,
            issue_date,
            issue_time: Some(local.time()),
            kind,
            profile,
            type_code: composed.type_code,
            currency: invoice.currency,
            exchange_rate,
            supplier,
            customer,
            recipient_alias: alias,
            lines: composed.lines,
            vat: composed.vat,
            totals: composed.totals,
            notes,
            internet_sale,
            billing_reference: None::<BillingReference>,
            order_reference: invoice.number.clone(),
            delivery_email,
        };
        Ok((request, to_number))
    }

    // -----------------------------------------------------------------------
    // sweep
    // -----------------------------------------------------------------------

    /// The `einvoice_sweep` task (every 10 minutes): queues stuck / pending
    /// documents, creates missing ones, handles refunds and cancellations and
    /// re-checks issued documents once a day.
    pub async fn einvoice_sweep(&self, tenant_id: TenantId) -> BillingResult<Value> {
        let mut tx = self.pool().begin().await?;
        let settings = EInvoiceSettings::load(&mut tx, tenant_id).await?;
        if !settings.enabled {
            return Ok(json!({"enabled": false}));
        }
        let mut queued = 0;
        for id in repo::due_for_sweep(&mut tx, tenant_id, 120, 200).await? {
            self.einvoice_enqueue(&mut tx, tenant_id, id, EInvoiceAction::Submit)
                .await?;
            queued += 1;
        }
        let since = settings.start_date.unwrap_or_else(|| self.clock().today());
        let statuses: &[&str] = match settings.issue_on {
            IssueOn::Payment => &["paid"],
            IssueOn::Creation => &["unpaid", "payment_pending", "paid", "collections"],
        };
        let mut created = 0;
        for invoice_id in
            repo::invoices_without_document(&mut tx, tenant_id, statuses, since, 100).await?
        {
            let (doc, _) = repo::ensure_document(
                &mut tx,
                tenant_id,
                invoice_id,
                PURPOSE_SALE,
                settings.integration_id,
            )
            .await?;
            self.einvoice_enqueue(&mut tx, tenant_id, doc.id, EInvoiceAction::Submit)
                .await?;
            created += 1;
        }
        let mut followups = 0;
        for (doc_id, status, charged_back) in
            repo::issued_with_reversed_invoice(&mut tx, tenant_id, 100).await?
        {
            let marker = format!("einvoice_followup:{doc_id}:{status}:{charged_back}");
            if !billing_support::mark_once(&mut *tx, tenant_id, &marker).await? {
                continue;
            }
            followups += 1;
            let Some(doc) = repo::get_document(&mut tx, tenant_id, doc_id).await? else {
                continue;
            };
            let attention = if charged_back {
                Some(
                    "the invoice was charged back after the e-document was issued; review whether a return invoice is needed",
                )
            } else if status == "partially_refunded" {
                Some(
                    "partial refund: the e-document stays valid; issue a return invoice / note for the refunded part if required",
                )
            } else if doc.kind.as_deref() == Some("efatura") {
                Some(
                    "refunded or cancelled: an e-Fatura cannot be cancelled by the seller; agree a return invoice (iade) with the buyer",
                )
            } else {
                None
            };
            match attention {
                Some(text) => {
                    repo::set_attention(&mut tx, tenant_id, doc_id, Some(text)).await?;
                    self.einvoice_alert(&mut tx, tenant_id, &doc, text).await;
                }
                None => {
                    self.einvoice_enqueue(&mut tx, tenant_id, doc_id, EInvoiceAction::Cancel)
                        .await?
                }
            }
        }
        let mut refreshed = 0;
        for id in repo::due_for_reconcile(&mut tx, tenant_id, 100).await? {
            self.einvoice_enqueue(&mut tx, tenant_id, id, EInvoiceAction::Refresh)
                .await?;
            refreshed += 1;
        }
        tx.commit().await?;
        Ok(
            json!({"queued": queued, "created": created, "followups": followups, "refreshed": refreshed}),
        )
    }
}

fn doc_ref(doc: &EInvoiceDocumentRow) -> BillingResult<EDocumentRef> {
    let kind = match doc.kind.as_deref() {
        Some("efatura") => DocumentKind::EFatura,
        Some("earsiv") => DocumentKind::EArsiv,
        _ => return Err(CoreError::validation("the e-document was never submitted")),
    };
    Ok(EDocumentRef {
        kind,
        ettn: doc.ettn,
        number: doc.number.clone(),
        provider_id: doc.provider_id.clone(),
        checkpoint: doc.checkpoint.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse() {
        let mut m = BTreeMap::new();
        m.insert(keys::ENABLED.into(), json!(true));
        m.insert(keys::ISSUE_ON.into(), json!("creation"));
        m.insert(keys::SERIES_EARSIV.into(), json!("hea"));
        m.insert(keys::SERIES_EFATURA.into(), json!("TOO-LONG"));
        m.insert(keys::EXEMPTION_CODE.into(), json!("351"));
        m.insert(
            keys::EXEMPTION_REASON.into(),
            json!("KDV İstisna Olmayan Diğer"),
        );
        m.insert(keys::START_DATE.into(), json!("2026-11-01"));
        let s = EInvoiceSettings::from_map(&m);
        assert!(s.enabled);
        assert_eq!(s.issue_on, IssueOn::Creation);
        assert_eq!(s.series_earsiv.unwrap().as_str(), "HEA");
        assert!(s.series_efatura.is_none());
        assert_eq!(s.exemption.unwrap().code, "351");
        assert_eq!(s.start_date, NaiveDate::from_ymd_opt(2026, 11, 1));
        assert!(!EInvoiceSettings::from_map(&BTreeMap::new()).enabled);
    }

    #[test]
    fn buyer_identification() {
        let consumer = json!({"kind": "individual", "name": "Ayşe Yılmaz", "city": "Kadıköy", "state": "İstanbul", "country_code": "TR"});
        let p = buyer(&consumer).unwrap();
        assert_eq!(p.id, PartyId::Tckn(CONSUMER_TCKN.into()));
        assert_eq!(p.first_name.as_deref(), Some("Ayşe"));
        assert_eq!(p.family_name.as_deref(), Some("Yılmaz"));
        let with_id = json!({"kind": "individual", "name": "Ayşe Yılmaz", "national_id": "100 000 001 46", "city": "Kadıköy", "country_code": "TR"});
        assert_eq!(
            buyer(&with_id).unwrap().id,
            PartyId::Tckn("10000000146".into())
        );
        let bad = json!({"kind": "individual", "name": "Ayşe Yılmaz", "national_id": "10000000145", "city": "Kadıköy", "country_code": "TR"});
        assert!(matches!(buyer(&bad), Err(BuildError::Fail(_))));
        let company = json!({"kind": "corporate", "name": "Ali Veli", "company_name": "Örnek Ltd.", "tax_number": "1234567890", "tax_office": "Kadıköy", "city": "Kadıköy", "country_code": "TR"});
        let c = buyer(&company).unwrap();
        assert_eq!(c.id, PartyId::Vkn("1234567890".into()));
        assert_eq!(c.name, "Örnek Ltd.");
        assert_eq!(c.tax_office.as_deref(), Some("Kadıköy"));
        let abroad = json!({"kind": "corporate", "company_name": "Acme GmbH", "tax_number": "DE123", "city": "Berlin", "country_code": "DE"});
        assert_eq!(buyer(&abroad).unwrap().id, PartyId::Vkn(FOREIGN_VKN.into()));
        let no_vkn =
            json!({"kind": "corporate", "company_name": "X", "city": "Y", "country_code": "TR"});
        assert!(buyer(&no_vkn).is_err());
        let no_city = json!({"kind": "individual", "name": "A B", "country_code": "TR"});
        assert!(buyer(&no_city).is_err());
    }

    #[test]
    fn gateway_mapping() {
        assert_eq!(payment_method_of("iyzico"), PaymentMethod::Card);
        assert_eq!(
            payment_method_of("bank_transfer"),
            PaymentMethod::BankTransfer
        );
        assert_eq!(payment_method_of("paypal"), PaymentMethod::Intermediary);
        assert_eq!(payment_method_of("credit"), PaymentMethod::Other);
    }
}
