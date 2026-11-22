//! Turkish e-invoicing (`einvoice` category): e-Fatura and e-Arşiv Fatura
//! through a private integrator (özel entegratör) or a pre-accounting service.
//!
//! ## Legal model (what the types encode)
//!
//! * **e-Fatura vs e-Arşiv** — a buyer registered in GİB's e-Fatura user list
//!   (looked up by VKN / TCKN, [`EInvoiceProvider::lookup_taxpayer`]) must get
//!   an **e-Fatura** ([`DocumentKind::EFatura`], profile `TEMELFATURA` or
//!   `TICARIFATURA`, delivered to one of the buyer's GİB mailbox aliases);
//!   everybody else gets an **e-Arşiv Fatura** ([`DocumentKind::EArsiv`],
//!   profile `EARSIVFATURA`). The decision is made by the caller
//!   (`hos-billing`), never guessed by a provider.
//! * **Buyer identifiers** — [`PartyId::Vkn`] (10 digits) for companies,
//!   [`PartyId::Tckn`] (11 digits) for people / sole proprietors. A domestic
//!   consumer who gave no TCKN is written as [`CONSUMER_TCKN`]
//!   (`11111111111`, e-Arşiv only, never looked up); a buyer without any
//!   Turkish tax id (abroad) as [`FOREIGN_VKN`] (`2222222222`, e-Arşiv only).
//! * **Cancellation** — an e-Arşiv invoice can be cancelled at the
//!   integrator (reported to GİB) while the integrator allows it
//!   ([`DocumentState::cancellable_until`]). An e-Fatura can **not** be
//!   cancelled by the seller: a `TICARIFATURA` may be rejected by the buyer
//!   ([`DocumentStatus::Rejected`]); otherwise the buyer issues a return
//!   invoice or both sides cancel outside the system. Providers therefore
//!   return [`IntegrationError::Unsupported`] for e-Fatura cancellation.
//! * **Return invoices (`IADE`)** — submitted like any other document with
//!   [`InvoiceTypeCode::Iade`] and a [`BillingReference`] to the original
//!   (UBL-TR: `cac:BillingReference/cac:InvoiceDocumentReference`,
//!   `DocumentTypeCode = IADE`). Whether the seller may issue one for a given
//!   refund is a legal decision `hos-billing` leaves to staff.
//! * **Numbers** — the 16-character legal number `AAA` + year + 9-digit
//!   sequence is either assigned by the merchant (UBL-based providers: the
//!   caller fills [`EDocumentRequest::number`]) or by the provider
//!   ([`Capabilities::assigns_numbers`], returned in [`DocumentState::number`]).
//!
//! ## Idempotency
//!
//! [`EDocumentRequest::ettn`] (the document UUID) is generated once by the
//! caller and never changes for a document; providers use it to recognise a
//! repeated submission. Multi-step providers record their progress in the
//! `checkpoint` value passed to [`EInvoiceProvider::submit`], which the
//! caller persists after every call (successful or not) and passes back on
//! retry, so a retried submission never creates a second legal document.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{ConnectionInfo, PostalAddress};
use crate::error::IntegrationError;
use crate::money::Currency;

/// TCKN written for a domestic consumer who did not give one (e-Arşiv only).
pub const CONSUMER_TCKN: &str = "11111111111";
/// VKN written for a buyer without a Turkish tax id (abroad; e-Arşiv only).
pub const FOREIGN_VKN: &str = "2222222222";

crate::string_enum! {
    /// Legal document family.
    pub enum DocumentKind {
        /// e-Fatura (both parties in GİB's e-Fatura system).
        EFatura => "efatura",
        /// e-Arşiv Fatura (buyer outside the e-Fatura system).
        EArsiv => "earsiv",
    }
}

crate::string_enum! {
    /// UBL-TR `cbc:ProfileID` (scenario).
    pub enum Profile {
        /// `TEMELFATURA`: no application response from the buyer.
        TemelFatura => "TEMELFATURA",
        /// `TICARIFATURA`: the buyer may accept or reject within 8 days.
        TicariFatura => "TICARIFATURA",
        /// `EARSIVFATURA`.
        EArsivFatura => "EARSIVFATURA",
    }
}

crate::string_enum! {
    /// UBL-TR `cbc:InvoiceTypeCode` values HostingOS issues.
    pub enum InvoiceTypeCode {
        /// Sale.
        Satis => "SATIS",
        /// Return invoice.
        Iade => "IADE",
        /// VAT exempt sale (exemption code from the `istisna` list, e.g. 301).
        Istisna => "ISTISNA",
        /// Sale with VAT withholding.
        Tevkifat => "TEVKIFAT",
    }
}

/// Tax identifier of a party.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scheme", content = "value", rename_all = "UPPERCASE")]
pub enum PartyId {
    /// Vergi Kimlik Numarası (10 digits).
    Vkn(String),
    /// T.C. Kimlik Numarası (11 digits).
    Tckn(String),
}

impl PartyId {
    /// The digits.
    pub fn value(&self) -> &str {
        match self {
            Self::Vkn(v) | Self::Tckn(v) => v,
        }
    }

    /// `VKN` / `TCKN` (UBL `schemeID`).
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Vkn(_) => "VKN",
            Self::Tckn(_) => "TCKN",
        }
    }

    /// Whether this is one of the placeholder ids ([`CONSUMER_TCKN`],
    /// [`FOREIGN_VKN`]) that must never be looked up or used for e-Fatura.
    pub fn is_placeholder(&self) -> bool {
        matches!(self.value(), CONSUMER_TCKN | FOREIGN_VKN)
    }
}

/// Seller or buyer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    /// Tax identifier.
    pub id: PartyId,
    /// Company title (VKN) or full name (TCKN).
    pub name: String,
    /// Given name(s) — required by UBL-TR for a TCKN party.
    pub first_name: Option<String>,
    /// Family name — required by UBL-TR for a TCKN party.
    pub family_name: Option<String>,
    /// Tax office (vergi dairesi), companies only.
    pub tax_office: Option<String>,
    /// `city` = district (ilçe) when `state` holds the province (il);
    /// otherwise `city` is used for both.
    pub address: PostalAddress,
    /// Country name as printed (`Türkiye`).
    pub country_name: String,
    /// Contact e-mail.
    pub email: Option<String>,
    /// Contact phone.
    pub phone: Option<String>,
}

/// A VAT exemption (`cbc:TaxExemptionReasonCode` + `cbc:TaxExemptionReason`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxExemption {
    /// GİB exemption code (e.g. `301`, `351`).
    pub code: String,
    /// Human-readable reason printed on the document.
    pub reason: String,
}

/// VAT withholding (tevkifat) of a line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Withholding {
    /// Withholding code (601…627).
    pub code: String,
    /// Percent of the VAT withheld (e.g. 50 for 5/10).
    pub rate: Decimal,
    /// Withheld VAT amount.
    pub amount: Decimal,
}

/// One invoice line. All amounts are rounded to the currency's minor unit and
/// consistent: `line_extension = round(quantity × unit_price) − allowance`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// Item description.
    pub name: String,
    /// Quantity.
    pub quantity: Decimal,
    /// UN/ECE rec. 20 unit code (`C62` = piece / unit).
    pub unit_code: String,
    /// Net unit price (VAT excluded).
    pub unit_price: Decimal,
    /// Discount on the line (≥ 0).
    pub allowance: Decimal,
    /// Net line amount after the discount.
    pub line_extension: Decimal,
    /// VAT percent (0 with an exemption).
    pub vat_rate: Decimal,
    /// VAT of the line.
    pub vat_amount: Decimal,
    /// Exemption when `vat_rate` is 0.
    pub exemption: Option<TaxExemption>,
    /// VAT withholding.
    pub withholding: Option<Withholding>,
}

/// Document-level VAT subtotal of one rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VatSubtotal {
    /// VAT percent.
    pub rate: Decimal,
    /// Taxable base.
    pub taxable: Decimal,
    /// VAT amount.
    pub amount: Decimal,
    /// Exemption when `rate` is 0.
    pub exemption: Option<TaxExemption>,
}

/// `cac:LegalMonetaryTotal` and tax totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// Sum of line net amounts.
    pub line_extension: Decimal,
    /// Sum of line discounts (informational; already inside `line_extension`).
    pub allowance_total: Decimal,
    /// Taxable amount (= `line_extension`, no document-level allowances).
    pub tax_exclusive: Decimal,
    /// Total VAT.
    pub vat_total: Decimal,
    /// Total withheld VAT.
    pub withholding_total: Decimal,
    /// `tax_exclusive + vat_total`.
    pub tax_inclusive: Decimal,
    /// `tax_inclusive − withholding_total`.
    pub payable: Decimal,
}

/// How an online sale was paid (e-Arşiv internet sale block).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethod {
    /// Credit / debit card.
    Card,
    /// EFT / wire transfer.
    BankTransfer,
    /// Payment intermediary (wallet, marketplace, PayPal…).
    Intermediary,
    /// Anything else (client credit, cash).
    Other,
}

/// e-Arşiv internet-sale information (required for online sales).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternetSale {
    /// Web address the sale was made on.
    pub website: String,
    /// How it was paid.
    pub payment_method: PaymentMethod,
    /// Payment platform / intermediary (`iyzico`, `PayTR`…).
    pub payment_platform: Option<String>,
    /// Payment date.
    pub payment_date: NaiveDate,
}

/// Reference to the invoice a return invoice returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingReference {
    /// 16-character legal number of the original document.
    pub number: String,
    /// Issue date of the original document.
    pub issue_date: NaiveDate,
    /// ETTN of the original document, when known.
    pub ettn: Option<Uuid>,
}

/// A document to issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EDocumentRequest {
    /// ETTN (`cbc:UUID`), fixed for the life of the document.
    pub ettn: Uuid,
    /// Legal number when the merchant assigns it (else the provider does).
    pub number: Option<String>,
    /// Issue date (never in the future).
    pub issue_date: NaiveDate,
    /// Issue time.
    pub issue_time: Option<NaiveTime>,
    /// e-Fatura or e-Arşiv.
    pub kind: DocumentKind,
    /// Scenario.
    pub profile: Profile,
    /// Invoice type.
    pub type_code: InvoiceTypeCode,
    /// Document currency.
    pub currency: Currency,
    /// TRY per one unit of `currency` (required when `currency` is not TRY).
    pub exchange_rate: Option<Decimal>,
    /// Seller (the tenant).
    pub supplier: Party,
    /// Buyer.
    pub customer: Party,
    /// Buyer's GİB mailbox alias (`urn:mail:…`, e-Fatura only).
    pub recipient_alias: Option<String>,
    /// Lines.
    pub lines: Vec<Line>,
    /// VAT subtotals per rate.
    pub vat: Vec<VatSubtotal>,
    /// Document totals.
    pub totals: Totals,
    /// Free-text notes (`cbc:Note`).
    pub notes: Vec<String>,
    /// Internet-sale block (e-Arşiv online sales).
    pub internet_sale: Option<InternetSale>,
    /// Original document of a return invoice.
    pub billing_reference: Option<BillingReference>,
    /// The HostingOS invoice number (order reference).
    pub order_reference: Option<String>,
    /// Where an e-Arşiv invoice is delivered electronically.
    pub delivery_email: Option<String>,
}

/// Where a document stands at the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentStatus {
    /// Accepted by the provider, not final yet (signing, GİB, buyer).
    Processing,
    /// A legal document exists (signed / delivered / reported).
    Issued,
    /// Rejected by the buyer (TICARIFATURA) — legally void.
    Rejected,
    /// Refused by the provider or GİB; nothing legal was created.
    Failed,
    /// Cancelled (e-Arşiv).
    Cancelled,
}

/// Result of submit / status / cancel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentState {
    /// Status.
    pub status: DocumentStatus,
    /// Legal number (when known).
    pub number: Option<String>,
    /// Provider's own id of the document.
    pub provider_id: Option<String>,
    /// Provider / GİB message (short, already cleaned of secrets).
    pub message: Option<String>,
    /// ETTN the provider assigned, when it does not use the caller's
    /// [`EDocumentRequest::ettn`].
    pub ettn: Option<Uuid>,
    /// Last moment the provider accepts a cancellation (e-Arşiv).
    pub cancellable_until: Option<DateTime<Utc>>,
}

impl DocumentState {
    /// A state with only a status.
    pub fn new(status: DocumentStatus) -> Self {
        Self {
            status,
            number: None,
            provider_id: None,
            message: None,
            ettn: None,
            cancellable_until: None,
        }
    }
}

/// An issued (or in-flight) document as stored by HostingOS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EDocumentRef {
    /// e-Fatura or e-Arşiv.
    pub kind: DocumentKind,
    /// ETTN.
    pub ettn: Uuid,
    /// Legal number (when known).
    pub number: Option<String>,
    /// Provider's own id.
    pub provider_id: Option<String>,
    /// Provider checkpoint (see [`EInvoiceProvider::submit`]).
    pub checkpoint: Value,
}

/// Result of [`EInvoiceProvider::lookup_taxpayer`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxpayerInfo {
    /// Registered e-Fatura user (must receive an e-Fatura).
    pub registered: bool,
    /// Registered title.
    pub title: Option<String>,
    /// Active GİB mailbox aliases (`urn:mail:…`), oldest first.
    pub aliases: Vec<String>,
}

/// What a provider supports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// e-Arşiv cancellation through the API.
    pub cancel_earsiv: bool,
    /// Return invoices (`IADE`) through the API.
    pub return_invoices: bool,
    /// The UBL XML can be downloaded.
    pub ubl_download: bool,
    /// The provider assigns legal numbers (else the merchant does).
    pub assigns_numbers: bool,
    /// The provider accepts non-TRY documents with an exchange rate.
    pub foreign_currency: bool,
    /// The integration talks to a test environment (documents have no legal
    /// effect).
    pub test_environment: bool,
}

/// An e-invoice provider (Paraşüt, Nilvera, mock…).
#[async_trait]
pub trait EInvoiceProvider: Send + Sync {
    /// `integrations.kind`.
    fn kind(&self) -> &'static str;

    /// Supported operations.
    fn capabilities(&self) -> Capabilities;

    /// Checks credentials / endpoint.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError>;

    /// Whether `tax_id` (VKN or TCKN, digits only) is a registered e-Fatura
    /// user, with its mailbox aliases.
    async fn lookup_taxpayer(&self, tax_id: &str) -> Result<TaxpayerInfo, IntegrationError>;

    /// Submits a document. `checkpoint` is the provider's progress from an
    /// earlier attempt (`null` the first time); the provider updates it as it
    /// goes and the caller stores it even when this returns an error.
    async fn submit(
        &self,
        request: &EDocumentRequest,
        checkpoint: &mut Value,
    ) -> Result<DocumentState, IntegrationError>;

    /// Current state of a submitted document.
    async fn status(&self, document: &EDocumentRef) -> Result<DocumentState, IntegrationError>;

    /// The legal PDF (the integrator's rendering of the signed document).
    async fn pdf(&self, document: &EDocumentRef) -> Result<Vec<u8>, IntegrationError>;

    /// The signed UBL-TR XML.
    async fn ubl(&self, _document: &EDocumentRef) -> Result<Vec<u8>, IntegrationError> {
        Err(IntegrationError::Unsupported("ubl"))
    }

    /// Cancels an e-Arşiv document. e-Fatura documents cannot be cancelled by
    /// the seller and return [`IntegrationError::Unsupported`].
    async fn cancel(
        &self,
        _document: &EDocumentRef,
        _reason: &str,
    ) -> Result<DocumentState, IntegrationError> {
        Err(IntegrationError::Unsupported("cancel"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_and_schemes() {
        assert!(PartyId::Tckn(CONSUMER_TCKN.into()).is_placeholder());
        assert!(PartyId::Vkn(FOREIGN_VKN.into()).is_placeholder());
        assert!(!PartyId::Vkn("1234567890".into()).is_placeholder());
        assert_eq!(PartyId::Tckn("1".into()).scheme(), "TCKN");
        assert_eq!(Profile::EArsivFatura.as_str(), "EARSIVFATURA");
        assert_eq!(InvoiceTypeCode::Iade.as_str(), "IADE");
        assert_eq!(
            "earsiv".parse::<DocumentKind>().unwrap(),
            DocumentKind::EArsiv
        );
        let v = serde_json::to_value(PartyId::Vkn("1234567890".into())).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"scheme": "VKN", "value": "1234567890"})
        );
    }
}
