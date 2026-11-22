//! Invoice PDFs (`hos-pdf`): the same content as the printable HTML invoice
//! (seller, buyer snapshot, lines, tax breakdown, totals, payment stamp),
//! plus the legal e-document reference when one was issued, notes and the
//! tenant's bank details. Amounts and dates use the HTML formatting
//! (`hos_pdf::format`, kept identical to `hos-web`'s `i18n::format_money`).
//!
//! Every string that comes from clients or staff is passed through
//! [`hos_pdf::clean`] / [`hos_pdf::paragraphs`] (control and bidi characters
//! removed, length capped); the renderer draws text as glyphs only, so it
//! cannot alter the PDF structure.

use std::sync::Arc;

use hos_core::CoreError;
use hos_core::domain::{InvoiceStatus, Locale};
use hos_core::ids::{InvoiceId, TenantId};
use hos_core::integrations::mailer::{Attachment, EmailMessage};
use hos_db::repo::{admin_settings, clients, einvoice as edocs, invoices, platform};
use hos_pdf::format::{date, money, percent, quantity};
use hos_pdf::labels::for_locale;
use hos_pdf::{
    InvoicePdf, Logo, LogoFormat, MAX_FIELD_CHARS, MAX_NOTE_CHARS, PdfLine, PdfTotal, Stamp,
    StampTone, clean, paragraphs,
};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::einvoice::EInvoiceSettings;

use crate::{Billing, BillingResult};
/// The PDF's money formatting (the web tests compare it with the HTML one).
pub use hos_pdf::format::money as pdf_money;

/// Largest logo embedded in a PDF.
const MAX_LOGO_BYTES: usize = 2 * 1024 * 1024;

/// A rendered PDF.
#[derive(Debug, Clone)]
pub struct RenderedPdf {
    /// File name (`fatura-INV2026000042.pdf`), ASCII only.
    pub filename: String,
    /// PDF bytes.
    pub data: Vec<u8>,
}

/// Safe ASCII file name for an invoice number.
pub fn pdf_filename(locale: Locale, number: Option<&str>, id: InvoiceId) -> String {
    let stem: String = number
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    let stem = if stem.is_empty() {
        id.to_string()
    } else {
        stem
    };
    let word = match locale {
        Locale::Tr => "fatura",
        Locale::En => "invoice",
    };
    format!("{word}-{stem}.pdf")
}

fn snapshot_lines(v: &Value) -> Vec<String> {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    [
        s("company_name"),
        s("name"),
        [s("tax_office"), s("tax_number")]
            .join(" ")
            .trim()
            .to_owned(),
        s("address_line1"),
        s("address_line2"),
        [s("postal_code"), s("city"), s("state")]
            .join(" ")
            .trim()
            .to_owned(),
        s("country_code"),
        s("email"),
    ]
    .into_iter()
    .filter(|l| !l.is_empty())
    .map(|l| clean(&l, MAX_FIELD_CHARS))
    .collect()
}

impl Billing {
    /// Renders the PDF of an invoice in `locale` (default: the client's).
    pub async fn invoice_pdf(
        &self,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
        locale: Option<Locale>,
    ) -> BillingResult<RenderedPdf> {
        let mut conn = self.pool().acquire().await?;
        let invoice = invoices::get_invoice(&mut *conn, tenant_id, invoice_id)
            .await?
            .ok_or_else(|| CoreError::not_found("invoice", invoice_id))?;
        let items = invoices::invoice_items(&mut *conn, tenant_id, invoice_id).await?;
        let taxes = invoices::invoice_tax_lines(&mut *conn, tenant_id, invoice_id).await?;
        let tenant = platform::get_tenant(&mut *conn, tenant_id)
            .await?
            .ok_or_else(|| CoreError::not_found("tenant", tenant_id))?;
        let client = clients::get_client(&mut *conn, tenant_id, invoice.client_id).await?;
        let locale = locale
            .or_else(|| client.as_ref().map(|c| c.locale))
            .unwrap_or(tenant.default_locale);
        let settings = EInvoiceSettings::load(&mut conn, tenant_id).await?;
        let edoc = edocs::for_invoice(
            &mut conn,
            tenant_id,
            invoice_id,
            crate::einvoice::PURPOSE_SALE,
        )
        .await?;
        let logo = admin_settings::get_tenant_asset(&mut *conn, tenant_id, "logo")
            .await?
            .filter(|a| a.data.len() <= MAX_LOGO_BYTES)
            .and_then(|a| {
                LogoFormat::from_content_type(&a.content_type).map(|format| Logo {
                    data: Arc::new(a.data),
                    format,
                })
            });
        drop(conn);

        let l = for_locale(locale);
        let cur = invoice.currency;
        let m = |d: &Decimal| money(*d, cur, locale);
        let number = invoice
            .number
            .clone()
            .unwrap_or_else(|| l.stamp_draft.to_owned());
        let today = self.clock().today();

        let mut meta = vec![
            (l.issued.to_owned(), date(invoice.issue_date, locale)),
            (l.due.to_owned(), date(invoice.due_date, locale)),
        ];
        if let Some(p) = invoice.paid_at {
            meta.push((l.paid_at.to_owned(), date(p.date_naive(), locale)));
        }

        let addr = |k: &str| {
            tenant
                .address
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        let legal_name = tenant
            .legal_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| tenant.name.clone());
        let seller: Vec<String> = [
            legal_name.clone(),
            [
                addr("line1"),
                addr("city"),
                addr("state"),
                addr("postal_code"),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
            match (tenant.tax_office.as_deref(), tenant.tax_number.as_deref()) {
                (Some(o), Some(n)) if !n.trim().is_empty() => {
                    format!("{} {o} · {} {n}", l.tax_office, l.tax_number)
                }
                (None, Some(n)) if !n.trim().is_empty() => format!("{} {n}", l.tax_number),
                _ => String::new(),
            },
            tenant.contact_email.clone().unwrap_or_default(),
            tenant.contact_phone.clone().unwrap_or_default(),
        ]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| clean(&s, MAX_FIELD_CHARS))
        .collect();

        let mut legal = Vec::new();
        if let Some(d) = edoc
            .as_ref()
            .filter(|d| matches!(d.status.as_str(), "issued" | "cancelled"))
        {
            let kind = match d.kind.as_deref() {
                Some("efatura") => "e-Fatura",
                _ => "e-Arşiv Fatura",
            };
            if let Some(n) = &d.number {
                legal.push(format!("{}: {kind} {n}", l.e_document));
            }
            legal.push(format!(
                "{}: {}",
                l.ettn,
                d.ettn.to_string().to_ascii_uppercase()
            ));
        }

        let lines: Vec<PdfLine> = items
            .iter()
            .map(|i| PdfLine {
                description: clean(&i.description, MAX_FIELD_CHARS),
                detail: match (i.period_start, i.period_end) {
                    (Some(a), Some(b)) => format!("{} – {}", date(a, locale), date(b, locale)),
                    _ => String::new(),
                },
                quantity: quantity(i.quantity, locale),
                unit_price: m(&i.unit_price),
                amount: m(&i.amount),
            })
            .collect();

        let mut totals = vec![PdfTotal {
            label: l.subtotal.to_owned(),
            value: m(&invoice.subtotal),
            emphasis: false,
        }];
        if !invoice.discount_total.is_zero() {
            totals.push(PdfTotal {
                label: l.discount.to_owned(),
                value: format!("−{}", m(&invoice.discount_total)),
                emphasis: false,
            });
        }
        for t in &taxes {
            totals.push(PdfTotal {
                label: clean(&format!("{} ({})", t.name, percent(t.rate)), 80),
                value: m(&t.tax_amount),
                emphasis: false,
            });
        }
        totals.push(PdfTotal {
            label: l.total.to_owned(),
            value: m(&invoice.total),
            emphasis: true,
        });
        let paid = invoice.amount_paid + invoice.credit_applied;
        if !paid.is_zero() {
            totals.push(PdfTotal {
                label: l.paid.to_owned(),
                value: m(&paid),
                emphasis: false,
            });
        }
        totals.push(PdfTotal {
            label: l.balance.to_owned(),
            value: m(&invoice.balance),
            emphasis: false,
        });

        let stamp = match invoice.status {
            InvoiceStatus::Paid => Some((l.stamp_paid, StampTone::Success)),
            InvoiceStatus::Refunded => Some((l.stamp_refunded, StampTone::Warning)),
            InvoiceStatus::PartiallyRefunded => {
                Some((l.stamp_partially_refunded, StampTone::Warning))
            }
            InvoiceStatus::Cancelled => Some((l.stamp_cancelled, StampTone::Neutral)),
            InvoiceStatus::Draft => Some((l.stamp_draft, StampTone::Neutral)),
            _ if invoice.is_overdue(today) => Some((l.stamp_overdue, StampTone::Danger)),
            _ => Some((l.stamp_unpaid, StampTone::Danger)),
        }
        .map(|(text, tone)| Stamp {
            text: text.to_owned(),
            tone,
        });

        let doc = InvoicePdf {
            lang: locale.as_str().to_owned(),
            heading: l.invoice.to_owned(),
            number: clean(&number, 64),
            brand: clean(&tenant.name, 120),
            logo,
            meta,
            seller_heading: l.from.to_owned(),
            seller,
            buyer_heading: l.bill_to.to_owned(),
            buyer: snapshot_lines(&invoice.billing_snapshot),
            legal,
            columns: [
                l.item.to_owned(),
                l.quantity.to_owned(),
                l.unit_price.to_owned(),
                l.amount.to_owned(),
            ],
            lines,
            totals,
            stamp,
            notes_heading: l.notes.to_owned(),
            notes: invoice
                .notes
                .as_deref()
                .map(|n| paragraphs(n, MAX_NOTE_CHARS))
                .unwrap_or_default(),
            bank_heading: l.bank.to_owned(),
            bank: paragraphs(&settings.bank_details, MAX_NOTE_CHARS),
            disclaimer: if settings.enabled {
                l.print_note.to_owned()
            } else {
                String::new()
            },
            footer: clean(&legal_name, 120),
            page_label: l.page.to_owned(),
            continued_label: l.continued.to_owned(),
            title: clean(&format!("{} {number}", l.invoice), 120),
        };
        let data = tokio::task::spawn_blocking(move || hos_pdf::render_invoice(&doc))
            .await
            .map_err(|e| CoreError::Internal(format!("PDF task: {e}")))?
            .map_err(|e| CoreError::Internal(format!("PDF rendering failed: {e}")))?;
        Ok(RenderedPdf {
            filename: pdf_filename(locale, invoice.number.as_deref(), invoice.id),
            data,
        })
    }

    /// Attaches the invoice PDF to an invoice e-mail when the tenant enabled
    /// `invoice_pdf.attach_to_emails`. Failures are logged; the e-mail is
    /// sent without the attachment.
    pub(crate) async fn attach_invoice_pdf(
        &self,
        tenant_id: TenantId,
        row: &hos_db::repo::mail::EmailLogRow,
        message: &mut EmailMessage,
    ) {
        if row.related_type.as_deref() != Some("invoice") {
            return;
        }
        let Some(invoice_id) = row.related_id.map(InvoiceId::from_uuid) else {
            return;
        };
        let enabled = match self.pool().acquire().await {
            Ok(mut c) => EInvoiceSettings::load(&mut c, tenant_id)
                .await
                .map(|s| s.attach_pdf)
                .unwrap_or(false),
            Err(_) => false,
        };
        if !enabled {
            return;
        }
        match self.invoice_pdf(tenant_id, invoice_id, None).await {
            Ok(pdf) => message.attachments.push(Attachment {
                filename: pdf.filename,
                content_type: "application/pdf".into(),
                data: pdf.data,
            }),
            Err(e) => {
                tracing::warn!(%tenant_id, %invoice_id, error = %e, "invoice PDF attachment failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_ascii() {
        let id = InvoiceId::new();
        assert_eq!(
            pdf_filename(Locale::Tr, Some("INV-2026/0042\"x"), id),
            "fatura-INV-20260042x.pdf"
        );
        assert_eq!(
            pdf_filename(Locale::En, Some("ÇĞ"), id),
            format!("invoice-{id}.pdf")
        );
    }
}
