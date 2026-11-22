//! Web helpers shared by the e-invoice / invoice-PDF pages
//! (`routes/admin/einvoice.rs`, `routes/admin/settings/einvoice.rs`,
//! `routes/client/invoice_files.rs`, `routes/api/documents.rs`) and the
//! client forms that validate Turkish tax ids.
//!
//! * [`tax_ids_enforced`] / [`check_tax_ids`]: VKN / TCKN checksum rules,
//!   applied only while the tenant has e-invoicing on and the client is in
//!   Türkiye (client profile, checkout, admin client forms).
//! * [`file_response`]: a download with a safe `Content-Disposition`
//!   (ASCII-only file name), `nosniff`, `no-store` and a sandbox CSP.
//!
//! Translations: `locales/{tr,en}/einvoice.json`.

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use hos_billing::einvoice::{keys, taxid};
use hos_core::ids::TenantId;
use hos_db::enums::ClientKind;
use hos_db::repo::clients::ClientInput;
use hos_db::repo::platform;
use http::HeaderValue;
use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use serde_json::Value;

use crate::error::AppResult;
use crate::forms::FormErrors;
use crate::state::AppState;
use crate::view::PageCtx;

/// Whether the tenant has e-invoicing switched on.
pub async fn enabled(state: &AppState, tenant_id: TenantId) -> AppResult<bool> {
    let map = platform::get_settings(&state.pool, tenant_id, keys::ENABLED).await?;
    Ok(match map.get(keys::ENABLED) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "true" | "1" | "on"),
        _ => false,
    })
}

/// Whether the VKN / TCKN rules apply to `input`: e-invoicing on and the
/// client in Türkiye.
pub async fn tax_ids_enforced(
    state: &AppState,
    tenant_id: TenantId,
    input: &ClientInput,
) -> AppResult<bool> {
    if !input.country_code.trim().eq_ignore_ascii_case("TR") {
        return Ok(false);
    }
    enabled(state, tenant_id).await
}

/// The VKN / TCKN rules (checksums, tax office) when they apply; errors are
/// added on the `tax_number`, `tax_office` and `national_id` fields.
///
/// * corporate: a checksum-valid VKN (10) or a sole proprietor's TCKN (11)
///   plus the tax office;
/// * T.C. identity number: checksum-valid when given. Individuals may leave
///   it empty — the e-Arşiv then carries the consumer placeholder
///   `11111111111`, which is never accepted as input.
pub async fn check_tax_ids(
    state: &AppState,
    tenant_id: TenantId,
    ctx: &PageCtx,
    input: &ClientInput,
    errors: &mut FormErrors,
) -> AppResult<()> {
    if !tax_ids_enforced(state, tenant_id, input).await? {
        return Ok(());
    }
    for (field, message) in tax_id_problems(input) {
        if errors.get(field).is_empty() {
            errors.add(field, ctx.t(message));
        }
    }
    Ok(())
}

/// `(field, message key)` of every rule `input` breaks (the rules of
/// [`check_tax_ids`], without the "does it apply" part).
pub fn tax_id_problems(input: &ClientInput) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    let present = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    if input.kind == ClientKind::Corporate {
        let ok = present(&input.tax_number)
            .and_then(|n| taxid::normalize(&n))
            .is_some_and(|n| taxid::is_valid_tax_number(&n));
        if !ok {
            out.push(("tax_number", "einvoice.validation.tax_number"));
        }
        if present(&input.tax_office).is_none() {
            out.push(("tax_office", "einvoice.validation.tax_office"));
        }
    }
    if let Some(id) = present(&input.national_id) {
        let ok = taxid::normalize(&id).is_some_and(|n| taxid::is_valid_tckn(&n));
        if !ok {
            out.push(("national_id", "einvoice.validation.national_id"));
        }
    }
    out
}

/// ASCII-only file name (letters, digits, `-`, `_`, `.`; at most 80 chars).
pub fn safe_filename(name: &str, fallback: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(80)
        .collect();
    let s = s.trim_start_matches('.').to_owned();
    if s.is_empty() { fallback.to_owned() } else { s }
}

/// A file download: `Content-Disposition: attachment` with a sanitised
/// ASCII name (no quotes, no header injection), `nosniff`, `no-store`
/// and a sandbox CSP (a PDF / XML opened in the browser runs nothing).
/// The bodies are bounded (generated PDFs, provider files capped at
/// 12 MiB by `hos_einvoice::http`) and sent with their length.
pub fn file_response(data: Vec<u8>, filename: &str, content_type: &'static str) -> Response {
    let name = safe_filename(filename, "document");
    let len = data.len();
    let mut res = Body::from(data).into_response();
    let h = res.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    res
}

#[cfg(test)]
mod tests {
    use hos_core::domain::Locale;
    use hos_core::money::Currency;
    use rust_decimal::Decimal;

    use super::*;

    fn corporate(tax: Option<&str>, office: Option<&str>) -> ClientInput {
        let mut c = ClientInput::individual("Ali", "Veli", "a@example.test", Currency::TRY);
        c.kind = ClientKind::Corporate;
        c.company_name = Some("Örnek A.Ş.".into());
        c.tax_number = tax.map(str::to_owned);
        c.tax_office = office.map(str::to_owned);
        c.country_code = "TR".into();
        c
    }

    #[test]
    fn tax_id_rules() {
        assert!(tax_id_problems(&corporate(Some("1234567890"), Some("Kadıköy"))).is_empty());
        // Sole proprietorship: TCKN as the tax number.
        assert!(tax_id_problems(&corporate(Some("10000000146"), Some("Kadıköy"))).is_empty());
        assert_eq!(
            tax_id_problems(&corporate(Some("1234567891"), Some("Kadıköy"))),
            vec![("tax_number", "einvoice.validation.tax_number")]
        );
        assert_eq!(
            tax_id_problems(&corporate(Some("2222222222"), None)),
            vec![
                ("tax_number", "einvoice.validation.tax_number"),
                ("tax_office", "einvoice.validation.tax_office")
            ]
        );
        let mut person = ClientInput::individual("Ayşe", "Yılmaz", "a@example.test", Currency::TRY);
        assert!(tax_id_problems(&person).is_empty(), "TCKN is optional");
        person.national_id = Some("10000000146".into());
        assert!(tax_id_problems(&person).is_empty());
        for bad in ["10000000147", "11111111111", "1000000014x"] {
            person.national_id = Some(bad.into());
            assert_eq!(
                tax_id_problems(&person),
                vec![("national_id", "einvoice.validation.national_id")],
                "{bad}"
            );
        }
    }

    #[test]
    fn filenames_are_header_safe() {
        assert_eq!(
            safe_filename("Fatura-ABC2026000000001.pdf", "x"),
            "Fatura-ABC2026000000001.pdf"
        );
        assert_eq!(
            safe_filename("a\"b\r\nSet-Cookie: x=1.pdf", "x"),
            "abSet-Cookiex1.pdf"
        );
        assert_eq!(safe_filename("çğü/..", "doc"), "doc");
        let res = file_response(b"%PDF-1.7".to_vec(), "x\".pdf", "application/pdf");
        assert_eq!(
            res.headers()[CONTENT_DISPOSITION].to_str().unwrap(),
            "attachment; filename=\"x.pdf\""
        );
        assert_eq!(res.headers()["x-content-type-options"], "nosniff");
    }

    /// The PDF formats money exactly like the HTML pages.
    #[test]
    fn pdf_money_matches_html() {
        for locale in [Locale::Tr, Locale::En] {
            for currency in [Currency::TRY, Currency::USD, Currency::EUR] {
                for raw in ["0", "1", "1234.5", "-1234567.891", "0.005", "999999.995"] {
                    let amount: Decimal = raw.parse().unwrap();
                    assert_eq!(
                        hos_billing::invoice_pdf::pdf_money(amount, currency, locale),
                        crate::i18n::format_money(amount, currency, locale),
                        "{raw} {currency:?} {locale:?}"
                    );
                }
            }
        }
    }
}
