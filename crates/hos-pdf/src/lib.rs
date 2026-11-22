//! Invoice / credit-note PDFs in pure Rust (no browser, no external binary).
//!
//! * Writer: [`krilla`] (maintained by the typst authors) — embedded fonts are
//!   subset and carry a `ToUnicode` map, so text is selectable / searchable
//!   and Turkish characters (ç ğ ı İ ö ş ü) and `₺` extract correctly.
//! * Font: Geist (SIL OFL 1.1, `static/fonts/Geist-Variable.woff2`, the web
//!   UI's own face) with Inter's latin-ext subset (SIL OFL 1.1) as fallback
//!   for the few characters Geist lacks (`₺`), decoded from WOFF2 with
//!   [`wuff`] once per process and instanced at weights 400 / 600; only the
//!   glyphs used are embedded.
//! * Input is a fully formatted [`InvoicePdf`] (strings, not domain types):
//!   callers format money / dates with [`format`] (identical to the HTML
//!   invoice). Every string is drawn as glyphs — never interpreted as PDF
//!   syntax — after [`clean`] removed control and bidi-override characters,
//!   so client-controlled text (names, notes) cannot inject content or
//!   reorder what is displayed.
//! * Layout: A4, header with logo or brand name, seller / buyer blocks, an
//!   item table that continues over as many pages as needed (header row
//!   repeated), totals with the tax breakdown, a payment-status stamp, notes,
//!   bank details and `Page n / m` on every page.

pub mod format;
pub mod labels;

mod font;
mod layout;

use std::sync::Arc;

pub use labels::Labels;

/// Errors of [`render_invoice`].
#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    /// The bundled font could not be decoded.
    #[error("font: {0}")]
    Font(String),
    /// krilla refused the document.
    #[error("pdf: {0}")]
    Write(String),
}

/// Raster logo formats krilla embeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl LogoFormat {
    /// From a MIME type (`image/png` …); `None` for anything else (SVG, ICO).
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        match content_type.trim().to_ascii_lowercase().as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

/// A tenant logo.
#[derive(Debug, Clone)]
pub struct Logo {
    pub data: Arc<Vec<u8>>,
    pub format: LogoFormat,
}

/// Colour of the status stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampTone {
    /// Paid (green).
    Success,
    /// Unpaid / overdue (red).
    Danger,
    /// Refunded / partially refunded (amber).
    Warning,
    /// Cancelled / draft (grey).
    Neutral,
}

/// The payment-status stamp.
#[derive(Debug, Clone)]
pub struct Stamp {
    pub text: String,
    pub tone: StampTone,
}

/// One item row.
#[derive(Debug, Clone, Default)]
pub struct PdfLine {
    pub description: String,
    /// Secondary line (service period …), may be empty.
    pub detail: String,
    pub quantity: String,
    pub unit_price: String,
    pub amount: String,
}

/// One row of the totals block.
#[derive(Debug, Clone)]
pub struct PdfTotal {
    pub label: String,
    pub value: String,
    /// Grand total: bold with a rule above.
    pub emphasis: bool,
}

/// A fully formatted invoice-like document.
#[derive(Debug, Clone, Default)]
pub struct InvoicePdf {
    /// `tr` / `en` (document language tag).
    pub lang: String,
    /// Big heading (`Fatura`, `Invoice`, `Credit note`).
    pub heading: String,
    /// Document number shown under the heading.
    pub number: String,
    /// Brand name (used when there is no logo).
    pub brand: String,
    pub logo: Option<Logo>,
    /// `(label, value)` rows next to the heading (dates, status).
    pub meta: Vec<(String, String)>,
    pub seller_heading: String,
    pub seller: Vec<String>,
    pub buyer_heading: String,
    pub buyer: Vec<String>,
    /// Legal e-document reference block (e-Arşiv / e-Fatura no, ETTN).
    pub legal: Vec<String>,
    /// Column headings: description, quantity, unit price, amount.
    pub columns: [String; 4],
    pub lines: Vec<PdfLine>,
    pub totals: Vec<PdfTotal>,
    pub stamp: Option<Stamp>,
    pub notes_heading: String,
    pub notes: Vec<String>,
    pub bank_heading: String,
    pub bank: Vec<String>,
    /// Small print at the bottom of the last page.
    pub disclaimer: String,
    /// Left part of every page footer.
    pub footer: String,
    /// `Page` word of `Page 1 / 2`.
    pub page_label: String,
    /// Suffix of a repeated table heading on continuation pages.
    pub continued_label: String,
    /// Document title metadata.
    pub title: String,
}

/// Longest string kept per field (anything longer is cut with `…`).
pub const MAX_FIELD_CHARS: usize = 600;
/// Longest note paragraph.
pub const MAX_NOTE_CHARS: usize = 4_000;

/// Text safe to draw: all control characters (including newlines — use
/// [`paragraphs`] to keep line breaks), bidi embedding / override / isolate
/// controls and other format characters become spaces; whitespace runs collapse; cut to `max`
/// characters.
pub fn clean(text: &str, max: usize) -> String {
    let mapped: String = text
        .chars()
        .map(|c| {
            let bidi = matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}');
            if c.is_control() || bidi || matches!(c, '\u{2028}' | '\u{2029}' | '\u{FEFF}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let flat = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &flat[..idx]),
        None => flat,
    }
}

/// Splits user text into cleaned paragraphs (on line breaks), dropping empty
/// ones.
pub fn paragraphs(text: &str, max: usize) -> Vec<String> {
    text.split(['\n', '\r'])
        .map(|p| clean(p, max))
        .filter(|p| !p.is_empty())
        .collect()
}

/// Renders the document. Deterministic for the same input.
pub fn render_invoice(doc: &InvoicePdf) -> Result<Vec<u8>, PdfError> {
    let fonts = font::Fonts::load()?;
    let pages = layout::lay_out(doc, &fonts);
    layout::write(doc, &fonts, &pages)
}

#[cfg(test)]
mod tests;
