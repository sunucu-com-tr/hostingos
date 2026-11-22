use chrono::{NaiveDate, NaiveTime};
use hos_core::integrations::PostalAddress;
use hos_core::integrations::einvoice::{
    BillingReference, CONSUMER_TCKN, DocumentKind, EDocumentRequest, InternetSale, InvoiceTypeCode,
    Line, Party, PartyId, PaymentMethod, Profile, TaxExemption, Totals, VatSubtotal, Withholding,
};
use hos_core::money::Currency;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use uuid::Uuid;

use super::*;
use crate::compose::{SourceInvoice, SourceLine, SourceTax, compose};
use crate::xml::parse;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 18).unwrap()
}

fn address() -> PostalAddress {
    PostalAddress {
        line1: "Atatürk Cad. No: 1".into(),
        line2: None,
        city: "Çankaya".into(),
        state: Some("Ankara".into()),
        postal_code: Some("06100".into()),
        country_code: "TR".into(),
    }
}

fn seller() -> Party {
    Party {
        id: PartyId::Vkn("1234567890".into()),
        name: "Örnek Barındırma A.Ş.".into(),
        first_name: None,
        family_name: None,
        tax_office: Some("Çankaya".into()),
        address: address(),
        country_name: "Türkiye".into(),
        email: Some("fatura@example.test".into()),
        phone: None,
    }
}

fn person(tckn: &str) -> Party {
    Party {
        id: PartyId::Tckn(tckn.into()),
        name: "Şule Öztürk".into(),
        first_name: Some("Şule".into()),
        family_name: Some("Öztürk".into()),
        tax_office: None,
        address: address(),
        country_name: "Türkiye".into(),
        email: Some("sule@example.test".into()),
        phone: Some("+905551112233".into()),
    }
}

fn request(
    lines: Vec<SourceLine>,
    rate: Decimal,
    exemption: Option<TaxExemption>,
) -> EDocumentRequest {
    let cur = Currency::TRY;
    let subtotal: Decimal = lines.iter().map(|l| l.amount).sum();
    let base: Decimal = lines.iter().filter(|l| l.taxable).map(|l| l.amount).sum();
    let tax =
        (base * rate / dec!(100)).round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    let src = SourceInvoice {
        currency: cur,
        lines,
        taxes: if rate.is_zero() {
            vec![]
        } else {
            vec![SourceTax {
                rate,
                compound: false,
                taxable_amount: base,
                amount: tax,
            }]
        },
        subtotal,
        tax_total: tax,
        total: subtotal + tax,
    };
    let c = compose(&src, exemption.as_ref()).unwrap();
    EDocumentRequest {
        ettn: Uuid::parse_str("3c5108e0-7962-4c02-b050-1022834d683f").unwrap(),
        number: Some("HOS2026000000042".into()),
        issue_date: today(),
        issue_time: NaiveTime::from_hms_opt(10, 30, 0),
        kind: DocumentKind::EArsiv,
        profile: Profile::EArsivFatura,
        type_code: c.type_code,
        currency: cur,
        exchange_rate: None,
        supplier: seller(),
        customer: person(CONSUMER_TCKN),
        recipient_alias: None,
        lines: c.lines,
        vat: c.vat,
        totals: c.totals,
        notes: vec!["Ödeme kredi kartı ile alınmıştır.".into()],
        internet_sale: Some(InternetSale {
            website: "https://www.example.test".into(),
            payment_method: PaymentMethod::Card,
            payment_platform: Some("iyzico".into()),
            payment_date: today(),
        }),
        billing_reference: None,
        order_reference: Some("INV-2026-0042".into()),
        delivery_email: None,
    }
}

fn line(desc: &str, amount: Decimal, taxable: bool) -> SourceLine {
    SourceLine {
        description: desc.into(),
        quantity: Decimal::ONE,
        unit_price: amount,
        amount,
        taxable,
    }
}

fn text<'a>(root: &'a crate::xml::Element, path: &[&str]) -> &'a str {
    root.path(path)
        .unwrap_or_else(|| panic!("missing {path:?}"))
        .text()
}

#[test]
fn earsiv_consumer_document() {
    let req = request(
        vec![line("Paylaşımlı hosting", dec!(33.33), true); 3],
        dec!(20),
        None,
    );
    let xml = to_xml(&req, today()).unwrap();
    let root = parse(&xml).unwrap();
    assert_eq!(root.name, "Invoice");
    assert_eq!(text(&root, &["UBLVersionID"]), "2.1");
    assert_eq!(text(&root, &["CustomizationID"]), "TR1.2");
    assert_eq!(text(&root, &["ProfileID"]), "EARSIVFATURA");
    assert_eq!(text(&root, &["ID"]), "HOS2026000000042");
    assert_eq!(
        text(&root, &["UUID"]),
        "3C5108E0-7962-4C02-B050-1022834D683F"
    );
    assert_eq!(text(&root, &["InvoiceTypeCode"]), "SATIS");
    assert_eq!(text(&root, &["LineCountNumeric"]), "3");
    // Consumer: placeholder TCKN with a Person block.
    let customer = root.path(&["AccountingCustomerParty", "Party"]).unwrap();
    let id = customer.path(&["PartyIdentification", "ID"]).unwrap();
    assert_eq!(
        (id.attr("schemeID"), id.text()),
        (Some("TCKN"), CONSUMER_TCKN)
    );
    assert_eq!(text(customer, &["Person", "FirstName"]), "Şule");
    assert_eq!(text(customer, &["PostalAddress", "CityName"]), "Ankara");
    assert_eq!(
        text(customer, &["PostalAddress", "CitySubdivisionName"]),
        "Çankaya"
    );
    // Totals reconcile: VAT 20.00, not 3 × 6.67.
    assert_eq!(text(&root, &["TaxTotal", "TaxAmount"]), "20.00");
    assert_eq!(
        root.path(&["TaxTotal", "TaxAmount"])
            .unwrap()
            .attr("currencyID"),
        Some("TRY")
    );
    assert_eq!(
        text(&root, &["LegalMonetaryTotal", "LineExtensionAmount"]),
        "99.99"
    );
    assert_eq!(
        text(&root, &["LegalMonetaryTotal", "PayableAmount"]),
        "119.99"
    );
    let line_vat: Vec<&str> = root
        .children_named("InvoiceLine")
        .map(|l| l.path(&["TaxTotal", "TaxAmount"]).unwrap().text())
        .collect();
    assert_eq!(line_vat, vec!["6.67", "6.67", "6.66"]);
    // Internet sale.
    assert_eq!(
        text(&root, &["AccountingSupplierParty", "Party", "WebsiteURI"]),
        "https://www.example.test"
    );
    assert_eq!(text(&root, &["PaymentMeans", "PaymentMeansCode"]), "48");
    assert_eq!(
        text(
            &root,
            &[
                "TaxTotal",
                "TaxSubtotal",
                "TaxCategory",
                "TaxScheme",
                "TaxTypeCode"
            ]
        ),
        "0015"
    );
}

#[test]
fn hostile_strings_are_escaped() {
    let mut req = request(
        vec![line(
            "x</cbc:Name><cbc:Injected>1</cbc:Injected>",
            dec!(10),
            true,
        )],
        dec!(20),
        None,
    );
    req.customer.name = "A & B <Ltd> \"q\" 'a'\u{0}\u{7}".into();
    req.customer.first_name = Some("<script>".into());
    req.notes = vec!["]]><!DOCTYPE x [<!ENTITY e SYSTEM \"file:///etc/passwd\">]>&e;".into()];
    let xml = to_xml(&req, today()).unwrap();
    assert!(!xml.contains("<cbc:Injected>"));
    assert!(!xml.contains("<!DOCTYPE"));
    assert!(!xml.contains('\u{0}') && !xml.contains('\u{7}'));
    let root = parse(&xml).unwrap();
    assert_eq!(
        text(&root, &["InvoiceLine", "Item", "Name"]),
        "x</cbc:Name><cbc:Injected>1</cbc:Injected>"
    );
    assert_eq!(
        text(
            &root,
            &["AccountingCustomerParty", "Party", "Person", "FirstName"]
        ),
        "<script>"
    );
    assert!(text(&root, &["Note"]).contains("&e;"));
}

#[test]
fn efatura_company_with_exemption_and_discount() {
    let mut req = request(
        vec![
            line("Sunucu", dec!(100), true),
            line("Alan adı", dec!(50), false),
            line("İndirim", dec!(-10), true),
        ],
        dec!(20),
        Some(TaxExemption {
            code: "351".into(),
            reason: "KDV İstisna Olmayan Diğer".into(),
        }),
    );
    req.kind = DocumentKind::EFatura;
    req.profile = Profile::TemelFatura;
    req.internet_sale = None;
    req.customer = Party {
        id: PartyId::Vkn("1234567890".into()),
        name: "Alıcı Ltd. Şti.".into(),
        first_name: None,
        family_name: None,
        tax_office: Some("Kadıköy".into()),
        ..seller()
    };
    req.recipient_alias = Some("urn:mail:defaultpk@alici.test".into());
    let xml = to_xml(&req, today()).unwrap();
    let root = parse(&xml).unwrap();
    assert_eq!(text(&root, &["ProfileID"]), "TEMELFATURA");
    assert_eq!(text(&root, &["InvoiceTypeCode"]), "SATIS");
    let subs: Vec<_> = root
        .path(&["TaxTotal"])
        .unwrap()
        .children_named("TaxSubtotal")
        .collect();
    assert_eq!(subs.len(), 2);
    assert_eq!(subs[0].child("TaxableAmount").unwrap().text(), "90.00");
    assert_eq!(subs[0].child("TaxAmount").unwrap().text(), "18.00");
    assert_eq!(
        text(subs[1], &["TaxCategory", "TaxExemptionReasonCode"]),
        "351"
    );
    let first = root.children_named("InvoiceLine").next().unwrap();
    assert_eq!(text(first, &["AllowanceCharge", "Amount"]), "10.00");
    assert_eq!(text(first, &["AllowanceCharge", "BaseAmount"]), "100.00");
    assert_eq!(text(first, &["LineExtensionAmount"]), "90.00");
    assert_eq!(
        text(&root, &["LegalMonetaryTotal", "PayableAmount"]),
        "158.00"
    );
    assert_eq!(
        text(
            &root,
            &["AccountingCustomerParty", "Party", "PartyName", "Name"]
        ),
        "Alıcı Ltd. Şti."
    );
    assert_eq!(
        text(
            &root,
            &[
                "AccountingCustomerParty",
                "Party",
                "PartyTaxScheme",
                "TaxScheme",
                "Name"
            ]
        ),
        "Kadıköy"
    );
}

#[test]
fn exemption_makes_istisna() {
    let req = request(
        vec![line("Yurt dışına hizmet", dec!(100), false)],
        Decimal::ZERO,
        Some(TaxExemption {
            code: "301".into(),
            reason: "11/1-a Mal ihracatı".into(),
        }),
    );
    let xml = to_xml(&req, today()).unwrap();
    let root = parse(&xml).unwrap();
    assert_eq!(text(&root, &["InvoiceTypeCode"]), "ISTISNA");
    assert_eq!(text(&root, &["TaxTotal", "TaxAmount"]), "0.00");
    assert_eq!(
        text(
            &root,
            &[
                "TaxTotal",
                "TaxSubtotal",
                "TaxCategory",
                "TaxExemptionReason"
            ]
        ),
        "11/1-a Mal ihracatı"
    );
    // The same code on a SATIS invoice is refused.
    let mut bad = req.clone();
    bad.type_code = InvoiceTypeCode::Satis;
    let err = check(&bad, today()).unwrap_err();
    assert!(err.0.iter().any(|e| e.contains("ISTISNA")), "{err}");
}

#[test]
fn foreign_currency_needs_rate() {
    let mut req = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    req.currency = Currency::USD;
    assert!(
        check(&req, today())
            .unwrap_err()
            .0
            .iter()
            .any(|e| e.contains("exchange rate"))
    );
    req.exchange_rate = Some(dec!(34.1234567));
    assert!(check(&req, today()).is_err(), "more than 6 decimals");
    req.exchange_rate = Some(dec!(34.123456));
    let xml = to_xml(&req, today()).unwrap();
    let root = parse(&xml).unwrap();
    assert_eq!(text(&root, &["DocumentCurrencyCode"]), "USD");
    assert_eq!(
        text(&root, &["PricingExchangeRate", "CalculationRate"]),
        "34.123456"
    );
    assert_eq!(
        text(&root, &["PricingExchangeRate", "TargetCurrencyCode"]),
        "TRY"
    );
}

#[test]
fn tax_id_rules() {
    let mut req = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    req.customer = person("10000000146");
    assert!(check(&req, today()).is_ok());
    req.customer = person("10000000145");
    assert!(
        check(&req, today())
            .unwrap_err()
            .0
            .iter()
            .any(|e| e.contains("TCKN check"))
    );
    req.customer = person("10000000146");
    req.customer.family_name = None;
    assert!(
        check(&req, today())
            .unwrap_err()
            .0
            .iter()
            .any(|e| e.contains("first and family name"))
    );
    // A placeholder can never receive an e-Fatura.
    let mut e = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    e.kind = DocumentKind::EFatura;
    e.profile = Profile::TemelFatura;
    e.internet_sale = None;
    e.recipient_alias = Some("urn:mail:x@y".into());
    assert!(
        check(&e, today())
            .unwrap_err()
            .0
            .iter()
            .any(|m| m.contains("real VKN"))
    );
    // Seller VKN checksum.
    let mut s = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    s.supplier.id = PartyId::Vkn("1234567891".into());
    assert!(
        check(&s, today())
            .unwrap_err()
            .0
            .iter()
            .any(|m| m.contains("seller: VKN check"))
    );
}

#[test]
fn numbering_dates_and_totals() {
    let mut req = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    req.number = Some("HOS2025000000001".into());
    assert!(
        check(&req, today())
            .unwrap_err()
            .0
            .iter()
            .any(|m| m.contains("year"))
    );
    req.number = Some("HO2026000000001".into());
    assert!(check(&req, today()).is_err());
    req.number = Some("HOS2026000000001".into());
    req.issue_date = today().succ_opt().unwrap();
    req.number = Some("HOS2026000000001".into());
    assert!(
        check(&req, today())
            .unwrap_err()
            .0
            .iter()
            .any(|m| m.contains("future"))
    );
    let mut req = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    req.totals.payable += dec!(0.01);
    assert!(
        check(&req, today())
            .unwrap_err()
            .0
            .iter()
            .any(|m| m.contains("payable"))
    );
    let mut req = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    req.lines[0].vat_amount += dec!(0.01);
    assert!(check(&req, today()).is_err());
}

#[test]
fn return_invoice_and_withholding() {
    let mut req = request(
        vec![line("Hosting iadesi", dec!(100), true)],
        dec!(20),
        None,
    );
    req.type_code = InvoiceTypeCode::Iade;
    assert!(
        check(&req, today())
            .unwrap_err()
            .0
            .iter()
            .any(|m| m.contains("IADE"))
    );
    req.billing_reference = Some(BillingReference {
        number: "HOS2026000000040".into(),
        issue_date: today(),
        ettn: None,
    });
    let xml = to_xml(&req, today()).unwrap();
    let root = parse(&xml).unwrap();
    assert_eq!(
        text(
            &root,
            &[
                "BillingReference",
                "InvoiceDocumentReference",
                "DocumentTypeCode"
            ]
        ),
        "IADE"
    );
    assert_eq!(
        text(
            &root,
            &["BillingReference", "InvoiceDocumentReference", "ID"]
        ),
        "HOS2026000000040"
    );

    // Tevkifat 601 at 40 %: 20.00 VAT, 8.00 withheld, payable 112.00.
    let mut t = request(vec![line("Yapım işi", dec!(100), true)], dec!(20), None);
    t.type_code = InvoiceTypeCode::Tevkifat;
    t.lines[0].withholding = Some(Withholding {
        code: "601".into(),
        rate: dec!(40),
        amount: dec!(8.00),
    });
    t.totals.withholding_total = dec!(8.00);
    t.totals.payable = t.totals.tax_inclusive - dec!(8.00);
    let xml = to_xml(&t, today()).unwrap();
    let root = parse(&xml).unwrap();
    assert_eq!(text(&root, &["WithholdingTaxTotal", "TaxAmount"]), "8.00");
    assert_eq!(
        text(
            &root,
            &[
                "WithholdingTaxTotal",
                "TaxSubtotal",
                "TaxCategory",
                "TaxScheme",
                "TaxTypeCode"
            ]
        ),
        "601"
    );
    assert_eq!(
        text(&root, &["LegalMonetaryTotal", "PayableAmount"]),
        "112.00"
    );
    t.lines[0].withholding = Some(Withholding {
        code: "601".into(),
        rate: dec!(50),
        amount: dec!(10.00),
    });
    assert!(check(&t, today()).is_err(), "601 only exists at 40 %");
}

#[test]
fn line_shapes() {
    let req = request(vec![line("Hosting", dec!(10), true)], dec!(20), None);
    let l: &Line = &req.lines[0];
    assert_eq!(l.unit_code, "C62");
    let v: &VatSubtotal = &req.vat[0];
    assert_eq!(v.rate, dec!(20));
    let t: &Totals = &req.totals;
    assert_eq!(t.tax_inclusive, dec!(12.00));
}
