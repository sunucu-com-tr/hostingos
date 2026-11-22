//! UBL-TR 2.1 (`CustomizationID` `TR1.2`) invoice builder shared by the
//! XML-based providers, plus [`check`], an offline subset of GİB's
//! schematron rules that runs before anything leaves HostingOS.
//!
//! Sources (read 2026-11-18): GİB e-Fatura package
//! (`UBL-TR_Main_Schematron.xml`, `UBL-TR_Common_Schematron.xml`,
//! `UBL-TR_Codelist.xml`, sample documents) from
//! <https://ebelge.gib.gov.tr/dosyalar/kilavuzlar/e-FaturaPaketi.zip>, and the
//! OASIS UBL 2.1 `Invoice` schema for element order.
//!
//! The document is **unsigned**: the integrator adds `ext:UBLExtensions`
//! (XAdES signature) and `cac:Signature`, which the schematron allows to be
//! absent in what the merchant sends. Every text value is escaped by
//! [`crate::xml::XmlWriter`].

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use hos_core::integrations::einvoice::{
    DocumentKind, EDocumentRequest, InvoiceTypeCode, Party, PartyId, PaymentMethod, Profile,
    TaxExemption,
};
use hos_core::money::Currency;
use rust_decimal::{Decimal, RoundingStrategy};

use crate::codes::{ExemptionClass, exemption_class, is_legal_number, is_valid_withholding};
use crate::taxid;
use crate::xml::XmlWriter;

const NS_INVOICE: &str = "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2";
const NS_CAC: &str = "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2";
const NS_CBC: &str = "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2";

/// UBL `TaxTypeCode` of KDV.
pub const KDV_CODE: &str = "0015";

/// Validation failures (one line each, safe to show to staff: they never
/// contain tax ids).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("UBL-TR validation failed: {}", .0.join("; "))]
pub struct UblError(pub Vec<String>);

fn amount(d: Decimal, cur: Currency) -> String {
    let dp = cur.minor_units();
    let mut x = d.round_dp_with_strategy(dp, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(dp);
    x.to_string()
}

fn plain(d: Decimal) -> String {
    d.normalize().to_string()
}

/// Checks `req` against the rules HostingOS can verify offline. `today`
/// is the current date in Türkiye (issue dates may not be in the future).
pub fn check(req: &EDocumentRequest, today: NaiveDate) -> Result<(), UblError> {
    let mut errors = Vec::new();
    let mut err = |m: &str| errors.push(m.to_owned());
    let cur = req.currency;
    let r = |d: Decimal| {
        d.round_dp_with_strategy(cur.minor_units(), RoundingStrategy::MidpointAwayFromZero)
    };

    match req.number.as_deref() {
        Some(n) if is_legal_number(n) => {
            if n[3..7] != req.issue_date.year().to_string() {
                err("the number's year differs from the issue date");
            }
        }
        Some(_) => err("the number must match AAA + year + 9 digits"),
        None => err("a document number is required"),
    }
    if req.issue_date > today {
        err("the issue date is in the future");
    }
    match (req.kind, req.profile) {
        (DocumentKind::EArsiv, Profile::EArsivFatura)
        | (DocumentKind::EFatura, Profile::TemelFatura | Profile::TicariFatura) => {}
        _ => err("profile does not match the document kind"),
    }
    match req.type_code {
        InvoiceTypeCode::Iade => {
            if req.profile == Profile::TicariFatura {
                err("IADE is not allowed with TICARIFATURA");
            }
            match &req.billing_reference {
                Some(b) if is_legal_number(&b.number) => {}
                _ => err("IADE needs the 16-character number of the original invoice"),
            }
        }
        _ if req.billing_reference.is_some() => {
            err("a billing reference is only allowed on IADE invoices");
        }
        _ => {}
    }
    if req.kind == DocumentKind::EFatura {
        if req.customer.id.is_placeholder() {
            err("an e-Fatura needs the buyer's real VKN / TCKN");
        }
        match req.recipient_alias.as_deref() {
            Some(a) if a.starts_with("urn:mail:") && a.len() > 9 => {}
            _ => err("an e-Fatura needs the buyer's GİB mailbox alias"),
        }
        if req.internet_sale.is_some() {
            err("internet-sale information is only used on e-Arşiv invoices");
        }
    }
    party_errors(&req.supplier, "seller", false, &mut errors);
    party_errors(&req.customer, "buyer", true, &mut errors);
    let mut err = |m: &str| errors.push(m.to_owned());

    if cur != Currency::TRY {
        match req.exchange_rate {
            Some(x) if x > Decimal::ZERO && x.normalize().scale() <= 6 => {}
            _ => err("a non-TRY invoice needs an exchange rate with at most 6 decimals"),
        }
    }
    if req.lines.is_empty() {
        err("an invoice needs at least one line");
    }
    let mut by_rate: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    let mut withholding_total = Decimal::ZERO;
    for (i, l) in req.lines.iter().enumerate() {
        let n = i + 1;
        if l.name.trim().is_empty() {
            errors.push(format!("line {n}: empty description"));
        }
        if l.quantity <= Decimal::ZERO {
            errors.push(format!("line {n}: quantity must be positive"));
        }
        if l.allowance < Decimal::ZERO
            || l.line_extension < Decimal::ZERO
            || l.vat_amount < Decimal::ZERO
        {
            errors.push(format!("line {n}: negative amounts are not allowed"));
        }
        if r(l.quantity * l.unit_price) - l.allowance != l.line_extension {
            errors.push(format!(
                "line {n}: quantity × price − discount ≠ line amount"
            ));
        }
        if l.vat_amount.is_zero() && req.type_code != InvoiceTypeCode::Iade && l.exemption.is_none()
        {
            errors.push(format!("line {n}: zero KDV needs an exemption reason"));
        }
        if !l.vat_rate.is_zero() && l.exemption.is_some() {
            errors.push(format!("line {n}: an exemption is only allowed at 0 %"));
        }
        if let Some(ex) = &l.exemption {
            exemption_errors(ex, req.type_code, &format!("line {n}"), &mut errors);
        }
        if let Some(w) = &l.withholding {
            if !matches!(
                req.type_code,
                InvoiceTypeCode::Tevkifat | InvoiceTypeCode::Iade
            ) {
                errors.push(format!("line {n}: withholding needs invoice type TEVKIFAT"));
            }
            if !is_valid_withholding(&w.code, &plain(w.rate)) {
                errors.push(format!("line {n}: invalid withholding code / rate"));
            }
            if w.amount != r(l.vat_amount * w.rate / Decimal::ONE_HUNDRED) {
                errors.push(format!("line {n}: withholding amount ≠ KDV × rate"));
            }
            withholding_total += w.amount;
        }
        let e = by_rate.entry(plain(l.vat_rate)).or_default();
        e.0 += l.line_extension;
        e.1 += l.vat_amount;
    }
    let mut err = |m: &str| errors.push(m.to_owned());
    if req.type_code == InvoiceTypeCode::Tevkifat && withholding_total.is_zero() {
        err("a TEVKIFAT invoice needs withheld lines");
    }
    let t = &req.totals;
    let lines_net: Decimal = req.lines.iter().map(|l| l.line_extension).sum();
    let lines_vat: Decimal = req.lines.iter().map(|l| l.vat_amount).sum();
    if t.line_extension != lines_net || t.tax_exclusive != lines_net {
        err("line amounts do not add up to the totals");
    }
    if t.vat_total != lines_vat {
        err("line KDV does not add up to the KDV total");
    }
    if t.withholding_total != withholding_total {
        err("withholding does not add up");
    }
    if t.tax_inclusive != t.tax_exclusive + t.vat_total {
        err("tax-inclusive total ≠ net + KDV");
    }
    if t.payable != t.tax_inclusive - t.withholding_total {
        err("payable ≠ tax-inclusive − withholding");
    }
    let vat_sum: Decimal = req.vat.iter().map(|v| v.amount).sum();
    let base_sum: Decimal = req.vat.iter().map(|v| v.taxable).sum();
    if vat_sum != t.vat_total || base_sum != t.line_extension {
        err("KDV subtotals do not add up");
    }
    for v in &req.vat {
        match by_rate.get(&plain(v.rate)) {
            Some((base, vat)) if *base == v.taxable && *vat == v.amount => {}
            _ => err("a KDV subtotal does not match its lines"),
        }
        if v.amount.is_zero() && req.type_code != InvoiceTypeCode::Iade && v.exemption.is_none() {
            err("a zero KDV subtotal needs an exemption reason");
        }
    }
    if by_rate.len() != req.vat.len() {
        err("every KDV rate needs exactly one subtotal");
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(UblError(errors))
    }
}

fn exemption_errors(
    ex: &TaxExemption,
    type_code: InvoiceTypeCode,
    at: &str,
    errors: &mut Vec<String>,
) {
    if ex.reason.trim().is_empty() {
        errors.push(format!("{at}: exemption reason text is empty"));
    }
    match exemption_class(&ex.code) {
        ExemptionClass::Unknown => errors.push(format!("{at}: unknown exemption code")),
        ExemptionClass::Istisna
            if !matches!(type_code, InvoiceTypeCode::Istisna | InvoiceTypeCode::Iade) =>
        {
            errors.push(format!("{at}: exemption code needs invoice type ISTISNA"));
        }
        ExemptionClass::OzelMatrah | ExemptionClass::IhracKayitli => {
            errors.push(format!("{at}: exemption code is not supported"));
        }
        _ => {}
    }
}

fn party_errors(p: &Party, role: &str, buyer: bool, errors: &mut Vec<String>) {
    let digits_ok = |v: &str, len: usize| v.len() == len && v.bytes().all(|b| b.is_ascii_digit());
    match &p.id {
        PartyId::Vkn(v) => {
            if !digits_ok(v, 10) {
                errors.push(format!("{role}: VKN must be 10 digits"));
            } else if !(buyer && p.id.is_placeholder()) && !taxid::is_valid_vkn(v) {
                errors.push(format!("{role}: VKN check digit is wrong"));
            }
            if p.name.trim().is_empty() {
                errors.push(format!("{role}: a VKN party needs a title"));
            }
        }
        PartyId::Tckn(v) => {
            if !digits_ok(v, 11) {
                errors.push(format!("{role}: TCKN must be 11 digits"));
            } else if !(buyer && p.id.is_placeholder()) && !taxid::is_valid_tckn(v) {
                errors.push(format!("{role}: TCKN check digits are wrong"));
            }
            let empty = |s: &Option<String>| s.as_deref().is_none_or(|s| s.trim().is_empty());
            if empty(&p.first_name) || empty(&p.family_name) {
                errors.push(format!("{role}: a TCKN party needs first and family name"));
            }
        }
    }
    if !buyer && p.id.is_placeholder() {
        errors.push(format!("{role}: placeholder tax id"));
    }
    if p.address.city.trim().is_empty() {
        errors.push(format!("{role}: city / district is required"));
    }
    if p.country_name.trim().is_empty() {
        errors.push(format!("{role}: country is required"));
    }
}

fn party(w: &mut XmlWriter, p: &Party, website: Option<&str>) {
    w.open("cac:Party");
    w.opt_leaf("cbc:WebsiteURI", website);
    w.open("cac:PartyIdentification")
        .leaf_with("cbc:ID", &[("schemeID", p.id.scheme())], p.id.value())
        .close();
    if matches!(p.id, PartyId::Vkn(_)) || !p.name.trim().is_empty() {
        w.open("cac:PartyName")
            .leaf("cbc:Name", p.name.trim())
            .close();
    }
    let a = &p.address;
    let street = match a.line2.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(l2) => format!("{} {}", a.line1.trim(), l2),
        None => a.line1.trim().to_owned(),
    };
    let (district, city) = match a.state.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(province) => (a.city.trim(), province),
        None => (a.city.trim(), a.city.trim()),
    };
    w.open("cac:PostalAddress")
        .opt_leaf("cbc:StreetName", Some(&street))
        .leaf("cbc:CitySubdivisionName", district)
        .leaf("cbc:CityName", city)
        .opt_leaf("cbc:PostalZone", a.postal_code.as_deref())
        .open("cac:Country")
        .opt_leaf("cbc:IdentificationCode", Some(&a.country_code))
        .leaf("cbc:Name", p.country_name.trim())
        .close()
        .close();
    if let Some(office) = p
        .tax_office
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        w.open("cac:PartyTaxScheme")
            .open("cac:TaxScheme")
            .leaf("cbc:Name", office)
            .close()
            .close();
    }
    if p.email.is_some() || p.phone.is_some() {
        w.open("cac:Contact")
            .opt_leaf("cbc:Telephone", p.phone.as_deref())
            .opt_leaf("cbc:ElectronicMail", p.email.as_deref())
            .close();
    }
    if matches!(p.id, PartyId::Tckn(_)) {
        w.open("cac:Person")
            .opt_leaf("cbc:FirstName", p.first_name.as_deref())
            .opt_leaf("cbc:FamilyName", p.family_name.as_deref())
            .close();
    }
    w.close();
}

fn tax_category(w: &mut XmlWriter, exemption: Option<&TaxExemption>) {
    w.open("cac:TaxCategory");
    if let Some(ex) = exemption {
        w.leaf("cbc:TaxExemptionReasonCode", &ex.code)
            .leaf("cbc:TaxExemptionReason", ex.reason.trim());
    }
    w.open("cac:TaxScheme")
        .leaf("cbc:Name", "KDV")
        .leaf("cbc:TaxTypeCode", KDV_CODE)
        .close()
        .close();
}

/// UN/ECE 4461 payment means code for the internet-sale block.
fn payment_means_code(m: PaymentMethod) -> &'static str {
    match m {
        PaymentMethod::Card => "48",
        PaymentMethod::BankTransfer => "42",
        PaymentMethod::Intermediary | PaymentMethod::Other => "ZZZ",
    }
}

/// Builds the UBL-TR XML of `req` after [`check`]ing it.
pub fn to_xml(req: &EDocumentRequest, today: NaiveDate) -> Result<String, UblError> {
    check(req, today)?;
    let cur = req.currency;
    let code = cur.as_str();
    let cur_attr = [("currencyID", code)];
    let mut w = XmlWriter::new();
    w.open_with(
        "Invoice",
        &[
            ("xmlns", NS_INVOICE),
            ("xmlns:cac", NS_CAC),
            ("xmlns:cbc", NS_CBC),
        ],
    );
    w.leaf("cbc:UBLVersionID", "2.1")
        .leaf("cbc:CustomizationID", "TR1.2")
        .leaf("cbc:ProfileID", req.profile.as_str())
        .leaf("cbc:ID", req.number.as_deref().unwrap_or_default())
        .leaf("cbc:CopyIndicator", "false")
        .leaf("cbc:UUID", &req.ettn.to_string().to_ascii_uppercase())
        .leaf(
            "cbc:IssueDate",
            &req.issue_date.format("%Y-%m-%d").to_string(),
        );
    if let Some(t) = req.issue_time {
        w.leaf("cbc:IssueTime", &t.format("%H:%M:%S").to_string());
    }
    w.leaf("cbc:InvoiceTypeCode", req.type_code.as_str());
    for note in &req.notes {
        w.opt_leaf("cbc:Note", Some(note));
    }
    w.leaf("cbc:DocumentCurrencyCode", code)
        .leaf("cbc:LineCountNumeric", &req.lines.len().to_string());
    if let Some(order) = req
        .order_reference
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        w.open("cac:OrderReference")
            .leaf("cbc:ID", order.trim())
            .leaf(
                "cbc:IssueDate",
                &req.issue_date.format("%Y-%m-%d").to_string(),
            )
            .close();
    }
    if let Some(b) = &req.billing_reference {
        w.open("cac:BillingReference")
            .open("cac:InvoiceDocumentReference");
        w.leaf("cbc:ID", &b.number);
        if let Some(u) = b.ettn {
            w.leaf("cbc:UUID", &u.to_string().to_ascii_uppercase());
        }
        w.leaf(
            "cbc:IssueDate",
            &b.issue_date.format("%Y-%m-%d").to_string(),
        )
        .leaf("cbc:DocumentTypeCode", "IADE")
        .close()
        .close();
    }
    let website = req.internet_sale.as_ref().map(|s| s.website.as_str());
    w.open("cac:AccountingSupplierParty");
    party(&mut w, &req.supplier, website);
    w.close();
    w.open("cac:AccountingCustomerParty");
    party(&mut w, &req.customer, None);
    w.close();
    if let Some(sale) = &req.internet_sale {
        w.open("cac:PaymentMeans")
            .leaf(
                "cbc:PaymentMeansCode",
                payment_means_code(sale.payment_method),
            )
            .leaf(
                "cbc:PaymentDueDate",
                &sale.payment_date.format("%Y-%m-%d").to_string(),
            )
            .opt_leaf("cbc:InstructionNote", sale.payment_platform.as_deref())
            .close();
    }
    if cur != Currency::TRY
        && let Some(rate) = req.exchange_rate
    {
        w.open("cac:PricingExchangeRate")
            .leaf("cbc:SourceCurrencyCode", code)
            .leaf("cbc:TargetCurrencyCode", "TRY")
            .leaf("cbc:CalculationRate", &plain(rate))
            .leaf("cbc:Date", &req.issue_date.format("%Y-%m-%d").to_string())
            .close();
    }
    // Document VAT.
    w.open("cac:TaxTotal").leaf_with(
        "cbc:TaxAmount",
        &cur_attr,
        &amount(req.totals.vat_total, cur),
    );
    for v in &req.vat {
        w.open("cac:TaxSubtotal")
            .leaf_with("cbc:TaxableAmount", &cur_attr, &amount(v.taxable, cur))
            .leaf_with("cbc:TaxAmount", &cur_attr, &amount(v.amount, cur))
            .leaf("cbc:Percent", &plain(v.rate));
        tax_category(&mut w, v.exemption.as_ref());
        w.close();
    }
    w.close();
    // Document withholding, grouped by code and rate.
    let mut withheld: BTreeMap<(String, String), (Decimal, Decimal)> = BTreeMap::new();
    for l in &req.lines {
        if let Some(h) = &l.withholding {
            let e = withheld.entry((h.code.clone(), plain(h.rate))).or_default();
            e.0 += l.vat_amount;
            e.1 += h.amount;
        }
    }
    if !withheld.is_empty() {
        w.open("cac:WithholdingTaxTotal").leaf_with(
            "cbc:TaxAmount",
            &cur_attr,
            &amount(req.totals.withholding_total, cur),
        );
        for ((code, rate), (base, tax)) in &withheld {
            withholding_subtotal(&mut w, cur, code, rate, *base, *tax);
        }
        w.close();
    }
    w.open("cac:LegalMonetaryTotal")
        .leaf_with(
            "cbc:LineExtensionAmount",
            &cur_attr,
            &amount(req.totals.line_extension, cur),
        )
        .leaf_with(
            "cbc:TaxExclusiveAmount",
            &cur_attr,
            &amount(req.totals.tax_exclusive, cur),
        )
        .leaf_with(
            "cbc:TaxInclusiveAmount",
            &cur_attr,
            &amount(req.totals.tax_inclusive, cur),
        )
        .leaf_with(
            "cbc:PayableAmount",
            &cur_attr,
            &amount(req.totals.payable, cur),
        )
        .close();
    for (i, l) in req.lines.iter().enumerate() {
        w.open("cac:InvoiceLine")
            .leaf("cbc:ID", &(i + 1).to_string())
            .leaf_with(
                "cbc:InvoicedQuantity",
                &[("unitCode", &l.unit_code)],
                &plain(l.quantity),
            )
            .leaf_with(
                "cbc:LineExtensionAmount",
                &cur_attr,
                &amount(l.line_extension, cur),
            );
        if !l.allowance.is_zero() {
            w.open("cac:AllowanceCharge")
                .leaf("cbc:ChargeIndicator", "false")
                .leaf_with("cbc:Amount", &cur_attr, &amount(l.allowance, cur))
                .leaf_with(
                    "cbc:BaseAmount",
                    &cur_attr,
                    &amount(l.line_extension + l.allowance, cur),
                )
                .close();
        }
        w.open("cac:TaxTotal")
            .leaf_with("cbc:TaxAmount", &cur_attr, &amount(l.vat_amount, cur))
            .open("cac:TaxSubtotal")
            .leaf_with(
                "cbc:TaxableAmount",
                &cur_attr,
                &amount(l.line_extension, cur),
            )
            .leaf_with("cbc:TaxAmount", &cur_attr, &amount(l.vat_amount, cur))
            .leaf("cbc:Percent", &plain(l.vat_rate));
        tax_category(&mut w, l.exemption.as_ref());
        w.close().close();
        if let Some(h) = &l.withholding {
            w.open("cac:WithholdingTaxTotal").leaf_with(
                "cbc:TaxAmount",
                &cur_attr,
                &amount(h.amount, cur),
            );
            withholding_subtotal(&mut w, cur, &h.code, &plain(h.rate), l.vat_amount, h.amount);
            w.close();
        }
        w.open("cac:Item").leaf("cbc:Name", l.name.trim()).close();
        w.open("cac:Price")
            .leaf_with("cbc:PriceAmount", &cur_attr, &plain(l.unit_price))
            .close();
        w.close();
    }
    Ok(w.finish())
}

fn withholding_subtotal(
    w: &mut XmlWriter,
    cur: Currency,
    code: &str,
    rate: &str,
    base: Decimal,
    tax: Decimal,
) {
    let cur_attr = [("currencyID", cur.as_str())];
    w.open("cac:TaxSubtotal")
        .leaf_with("cbc:TaxableAmount", &cur_attr, &amount(base, cur))
        .leaf_with("cbc:TaxAmount", &cur_attr, &amount(tax, cur))
        .leaf("cbc:Percent", rate)
        .open("cac:TaxCategory")
        .open("cac:TaxScheme")
        .leaf("cbc:Name", "KDV TEVKİFAT")
        .leaf("cbc:TaxTypeCode", code)
        .close()
        .close()
        .close();
}

#[cfg(test)]
mod tests;
