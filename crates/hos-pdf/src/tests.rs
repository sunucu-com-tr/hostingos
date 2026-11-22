use super::*;

fn sample(lines: usize) -> InvoicePdf {
    let l = labels::for_locale(hos_core::domain::Locale::Tr);
    InvoicePdf {
        lang: "tr".into(),
        heading: l.invoice.into(),
        number: "HOS2026000000042".into(),
        brand: "Örnek Barındırma".into(),
        logo: None,
        meta: vec![
            (l.issued.into(), "16 Kasım 2026".into()),
            (l.due.into(), "23 Kasım 2026".into()),
        ],
        seller_heading: l.from.into(),
        seller: vec![
            "Örnek Barındırma Hizmetleri A.Ş.".into(),
            "Çankaya Vergi Dairesi 1234567890".into(),
        ],
        buyer_heading: l.bill_to.into(),
        buyer: vec![
            "Şükrü Öğüt".into(),
            "İstiklal Cad. No: 5, Beyoğlu, İstanbul".into(),
        ],
        legal: vec!["e-Arşiv Fatura HOS2026000000042".into()],
        columns: [
            l.item.into(),
            l.quantity.into(),
            l.unit_price.into(),
            l.amount.into(),
        ],
        lines: (0..lines)
            .map(|i| PdfLine {
                description: format!("Paylaşımlı hosting — satır {i} (ığüşöç İĞÜŞÖÇ)"),
                detail: "01.11.2026 – 30.11.2026".into(),
                quantity: "1".into(),
                unit_price: "100,00 ₺".into(),
                amount: "100,00 ₺".into(),
            })
            .collect(),
        totals: vec![
            PdfTotal {
                label: l.subtotal.into(),
                value: "100,00 ₺".into(),
                emphasis: false,
            },
            PdfTotal {
                label: "KDV (%20)".into(),
                value: "20,00 ₺".into(),
                emphasis: false,
            },
            PdfTotal {
                label: l.total.into(),
                value: "120,00 ₺".into(),
                emphasis: true,
            },
        ],
        stamp: Some(Stamp {
            text: l.stamp_paid.into(),
            tone: StampTone::Success,
        }),
        notes_heading: l.notes.into(),
        notes: vec!["Ödemeniz için teşekkür ederiz.".into()],
        bank_heading: l.bank.into(),
        bank: vec!["TR00 0000 0000 0000 0000 0000 00".into()],
        disclaimer: l.print_note.into(),
        footer: "Örnek Barındırma Hizmetleri A.Ş.".into(),
        page_label: l.page.into(),
        continued_label: l.continued.into(),
        title: "Fatura HOS2026000000042".into(),
    }
}

fn text_of(pdf: &[u8]) -> String {
    pdf_extract::extract_text_from_mem(pdf).expect("extract text")
}

/// Page count as printed in the footer counter of page 1 ("Sayfa 1 / N").
fn page_count(text: &str) -> usize {
    let rest = &text[text.find("Sayfa 1 / ").expect("page counter") + "Sayfa 1 / ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap()
}

#[test]
fn renders_turkish_text_selectably() {
    let pdf = render_invoice(&sample(3)).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    let text = text_of(&pdf);
    for needle in [
        "Fatura",
        "HOS2026000000042",
        "Şükrü Öğüt",
        "İstiklal",
        "ığüşöç İĞÜŞÖÇ",
        "120,00 ₺",
        "ÖDENDİ",
        "Ödemeniz için teşekkür ederiz.",
        "Sayfa 1 / 1",
        "SATICI",
        "FATURA ADRESİ",
        "BİRİM FİYAT",
        "BANKA BİLGİLERİ",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
}

#[test]
fn long_invoice_spans_pages_with_counters() {
    let pdf = render_invoice(&sample(200)).unwrap();
    let text = text_of(&pdf);
    let pages = page_count(&text);
    assert!(
        pages >= 5,
        "200 lines should need several pages, got {pages}"
    );
    for n in 1..=pages {
        assert!(text.contains(&format!("Sayfa {n} / {pages}")), "page {n}");
    }
    assert!(text.contains(&format!("Sayfa {pages} / {pages}")));
    assert!(text.contains("satır 0 "));
    assert!(text.contains("satır 199 "));
    // The table heading is repeated on every continuation page that still
    // has lines (the last page may hold only the totals).
    assert!(text.matches("AÇIKLAMA (DEVAMI)").count() >= pages - 2);
    assert_eq!(crate::layout::upper("tr", "devamı iş"), "DEVAMI İŞ");
    assert_eq!(crate::layout::upper("en", "invoice"), "INVOICE");
}

#[test]
fn hostile_strings_are_inert() {
    let mut doc = sample(1);
    doc.buyer = vec![
        "Evil\u{202E}gnp.exe".into(),
        ") Tj ET BT /F1 99 Tf (pwned".into(),
    ];
    doc.notes = paragraphs("line1\r\nline2\u{0007}\n\n<</JS (app.alert(1))>>", 50);
    // Callers clean; make sure clean removes the override.
    let cleaned: Vec<String> = doc.buyer.iter().map(|s| clean(s, 100)).collect();
    assert_eq!(cleaned[0], "Evil gnp.exe");
    doc.buyer = cleaned;
    let pdf = render_invoice(&doc).unwrap();
    let raw = String::from_utf8_lossy(&pdf);
    assert!(!raw.contains("/JS"), "no JavaScript action may appear");
    assert!(
        !raw.contains("pwned"),
        "text is drawn as glyphs, never as syntax"
    );
    let text = text_of(&pdf);
    assert!(text.contains("Tj ET BT"));
    assert!(text.contains("app.alert(1)"));
}

#[test]
fn clean_and_paragraphs() {
    assert_eq!(clean("  a\tb\u{0000}c  ", 10), "a b c");
    assert_eq!(clean("abcdef", 3), "abc…");
    assert_eq!(
        paragraphs("x\n\n y \r\nz", 10),
        vec!["x".to_owned(), "y".to_owned(), "z".to_owned()]
    );
}

#[test]
fn logo_formats() {
    assert_eq!(
        LogoFormat::from_content_type("image/PNG"),
        Some(LogoFormat::Png)
    );
    assert_eq!(LogoFormat::from_content_type("image/svg+xml"), None);
}

/// A valid 8-bit greyscale PNG of `w` × `h` (stored deflate blocks).
fn grey_png(w: u32, h: u32) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], body: &[u8]) {
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
        let mut crc = kind.to_vec();
        crc.extend_from_slice(body);
        out.extend_from_slice(&crc);
        out.extend_from_slice(&crc32(&crc).to_be_bytes());
    }
    let mut raw = Vec::new();
    for _ in 0..h {
        raw.push(0);
        raw.extend(std::iter::repeat_n(0x80u8, w as usize));
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = u16::try_from(b.len()).unwrap();
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
fn oversized_logo_is_never_decoded() {
    let logo = |w, h| Logo {
        data: std::sync::Arc::new(grey_png(w, h)),
        format: LogoFormat::Png,
    };
    assert!(
        crate::layout::image(&logo(64, 32)).is_some(),
        "a normal logo is embedded"
    );
    // Wider than the limit: left out before krilla decodes it; the brand
    // name is printed instead.
    assert!(crate::layout::image(&logo(5000, 1)).is_none());
    let mut doc = sample(1);
    doc.logo = Some(logo(5000, 1));
    let text = text_of(&render_invoice(&doc).unwrap());
    assert!(text.contains("Örnek Barındırma"));
    // A WebP header claiming 16383 × 16383 (1 GiB of pixels).
    let bits: u32 = 16382 | (16382 << 14);
    let mut bomb = b"RIFF\x1a\0\0\0WEBPVP8L\x0d\0\0\0\x2f".to_vec();
    bomb.extend_from_slice(&bits.to_le_bytes());
    bomb.extend_from_slice(&[0; 8]);
    let webp = Logo {
        data: std::sync::Arc::new(bomb),
        format: LogoFormat::Webp,
    };
    assert!(crate::layout::image(&webp).is_none());
}
