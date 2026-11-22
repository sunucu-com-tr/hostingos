#![allow(dead_code)]

use chrono::{NaiveTime, Utc};
use hos_core::integrations::PostalAddress;
use hos_core::integrations::einvoice::{
    DocumentKind, EDocumentRequest, InternetSale, Party, PartyId, PaymentMethod, Profile,
};
use hos_core::money::Currency;
use hos_einvoice::compose::{SourceInvoice, SourceLine, SourceTax, compose};
use rust_decimal_macros::dec;
use uuid::Uuid;

pub fn address() -> PostalAddress {
    PostalAddress {
        line1: "Atatürk Cad. No: 1".into(),
        line2: None,
        city: "Çankaya".into(),
        state: Some("Ankara".into()),
        postal_code: Some("06100".into()),
        country_code: "TR".into(),
    }
}

/// e-Arşiv request for a consumer: 100.00 + 20 % KDV.
pub fn earsiv_request() -> EDocumentRequest {
    let src = SourceInvoice {
        currency: Currency::TRY,
        lines: vec![SourceLine {
            description: "Paylaşımlı hosting".into(),
            quantity: dec!(1),
            unit_price: dec!(100),
            amount: dec!(100.00),
            taxable: true,
        }],
        taxes: vec![SourceTax {
            rate: dec!(20),
            compound: false,
            taxable_amount: dec!(100.00),
            amount: dec!(20.00),
        }],
        subtotal: dec!(100.00),
        tax_total: dec!(20.00),
        total: dec!(120.00),
    };
    let c = compose(&src, None).unwrap();
    let today = (Utc::now() + chrono::Duration::hours(3)).date_naive();
    EDocumentRequest {
        ettn: Uuid::now_v7(),
        number: Some(format!("HOS{}000000007", chrono::Datelike::year(&today))),
        issue_date: today,
        issue_time: NaiveTime::from_hms_opt(9, 0, 0),
        kind: DocumentKind::EArsiv,
        profile: Profile::EArsivFatura,
        type_code: c.type_code,
        currency: Currency::TRY,
        exchange_rate: None,
        supplier: Party {
            id: PartyId::Vkn("1234567890".into()),
            name: "Örnek Barındırma A.Ş.".into(),
            first_name: None,
            family_name: None,
            tax_office: Some("Çankaya".into()),
            address: address(),
            country_name: "Türkiye".into(),
            email: None,
            phone: None,
        },
        customer: Party {
            id: PartyId::Tckn("10000000146".into()),
            name: "Şule Öztürk".into(),
            first_name: Some("Şule".into()),
            family_name: Some("Öztürk".into()),
            tax_office: None,
            address: address(),
            country_name: "Türkiye".into(),
            email: Some("sule@example.test".into()),
            phone: None,
        },
        recipient_alias: None,
        lines: c.lines,
        vat: c.vat,
        totals: c.totals,
        notes: vec![],
        internet_sale: Some(InternetSale {
            website: "https://www.example.test".into(),
            payment_method: PaymentMethod::Card,
            payment_platform: Some("iyzico".into()),
            payment_date: today,
        }),
        billing_reference: None,
        order_reference: Some("INV-0007".into()),
        delivery_email: None,
    }
}
