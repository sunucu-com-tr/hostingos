//! `llms.txt` / `llms-full.txt` (<https://llmstxt.org>) and per-page Markdown.
//!
//! * [`LlmsTxt::render`] — `# Site` H1, `> summary` blockquote, optional
//!   detail paragraphs (no headings), then `## Section` link lists
//!   (`- [Title](url): description`), with the spec's special `## Optional`
//!   section last.
//! * [`render_llms_full`] — the full-text Markdown dump: products with price
//!   tables, FAQ, knowledge-base articles, policies and other pages.
//! * [`page_markdown`] — the `.md` rendition of one content page (serve it at
//!   `{page}.md` and link it with `<link rel="alternate" type="text/markdown">`).

use chrono::{DateTime, Utc};
use hos_core::domain::Locale;
use hos_core::money::{BillingCycle, Money};
use serde::{Deserialize, Serialize};

use crate::escape;
use crate::jsonld::FaqEntry;
use crate::markdown::{self, MarkdownOptions, escape_md, url_for_md};
use crate::text;

/// One link line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmsLink {
    /// Link text.
    pub title: String,
    /// Absolute URL (prefer the `.md` rendition when one exists).
    pub url: String,
    /// Optional one-line description.
    #[serde(default)]
    pub description: Option<String>,
}

impl LlmsLink {
    /// Creates a link.
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            url: url.into(),
            description: None,
        }
    }

    /// Adds a description.
    #[must_use]
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    fn render(&self) -> Option<String> {
        let url = escape::safe_http_url(&self.url)?;
        let title = text::normalize_ws(&self.title);
        if title.is_empty() {
            return None;
        }
        let mut line = format!("- [{}]({})", escape_md(&title), url_for_md(url));
        if let Some(d) = self
            .description
            .as_deref()
            .map(text::normalize_ws)
            .filter(|d| !d.is_empty())
        {
            line.push_str(": ");
            line.push_str(&escape_md(&d));
        }
        Some(line)
    }
}

/// A `## Section` with links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmsSection {
    /// Section title.
    pub title: String,
    /// Links.
    pub links: Vec<LlmsLink>,
}

/// `llms.txt` content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmsTxt {
    /// Site / company name (H1).
    pub site_name: String,
    /// One-paragraph summary (blockquote).
    pub summary: String,
    /// Extra Markdown paragraphs / lists (headings are neutralised).
    pub details: Option<String>,
    /// Link sections.
    pub sections: Vec<LlmsSection>,
    /// Secondary links (`## Optional` — may be skipped by short-context readers).
    pub optional: Vec<LlmsLink>,
}

impl LlmsTxt {
    /// Renders `llms.txt`.
    pub fn render(&self) -> String {
        let mut out = format!("# {}\n", escape_md(&text::normalize_ws(&self.site_name)));
        let summary = text::normalize_ws(&self.summary);
        if !summary.is_empty() {
            out.push_str(&format!("\n> {}\n", escape_md(&summary)));
        }
        if let Some(d) = self
            .details
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            out.push('\n');
            out.push_str(&neutralize_headings(d));
            out.push('\n');
        }
        for s in &self.sections {
            let lines: Vec<String> = s.links.iter().filter_map(LlmsLink::render).collect();
            let title = text::normalize_ws(&s.title);
            if lines.is_empty() || title.is_empty() || title.eq_ignore_ascii_case("optional") {
                continue;
            }
            out.push_str(&format!(
                "\n## {}\n\n{}\n",
                escape_md(&title),
                lines.join("\n")
            ));
        }
        let optional: Vec<String> = self.optional.iter().filter_map(LlmsLink::render).collect();
        if !optional.is_empty() {
            out.push_str(&format!("\n## Optional\n\n{}\n", optional.join("\n")));
        }
        out
    }
}

/// Escapes lines that would start a heading (details must not contain headings).
fn neutralize_headings(md: &str) -> String {
    md.lines()
        .map(|l| {
            if l.trim_start().starts_with('#') {
                format!("\\{}", l.trim_start())
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Content body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", content = "content", rename_all = "snake_case")]
pub enum Body {
    /// HTML (converted with [`markdown::html_to_markdown`]).
    Html(String),
    /// Markdown (used as is).
    Markdown(String),
    /// Plain text (escaped).
    Text(String),
}

impl Body {
    /// The body as Markdown.
    pub fn to_markdown(&self, base_url: Option<&str>) -> String {
        match self {
            Self::Html(h) => {
                let opts = MarkdownOptions {
                    base_url: base_url.map(str::to_owned),
                    ..MarkdownOptions::default()
                };
                markdown::html_to_markdown(h, &opts)
            }
            Self::Markdown(m) => m.trim().to_owned(),
            Self::Text(t) => t
                .split("\n\n")
                .map(|p| escape_md(&text::normalize_ws(p)))
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
        }
    }
}

/// A content document (KB article, policy, page).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    /// Title.
    pub title: String,
    /// Canonical URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Last update.
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    /// Content.
    pub body: Body,
}

/// One price row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLine {
    /// Billing cycle.
    pub cycle: BillingCycle,
    /// Price per cycle.
    pub price: Money,
    /// Setup fee.
    #[serde(default)]
    pub setup_fee: Option<Money>,
}

/// A product for `llms-full.txt`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProductEntry {
    /// Name.
    pub name: String,
    /// Product group (`"WordPress Hosting"`).
    pub group: Option<String>,
    /// Product page URL.
    pub url: Option<String>,
    /// Description.
    pub description: Option<Body>,
    /// Prices.
    pub prices: Vec<PriceLine>,
    /// Feature list (`"Disk"` → `"20 GB NVMe"`).
    pub features: Vec<(String, String)>,
}

/// `llms-full.txt` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmsFull {
    /// Site / company name.
    pub site_name: String,
    /// Summary.
    pub summary: String,
    /// Home URL (base for relative links in HTML bodies).
    pub base_url: Option<String>,
    /// Language of section headings / labels.
    pub locale: Locale,
    /// Products and prices.
    pub products: Vec<ProductEntry>,
    /// FAQ.
    pub faqs: Vec<FaqEntry>,
    /// Knowledge-base articles.
    pub kb_articles: Vec<Document>,
    /// Policies (terms, privacy, refund, SLA …) — full text.
    pub policies: Vec<Document>,
    /// Other pages (about, contact …).
    pub pages: Vec<Document>,
}

struct Labels {
    products: &'static str,
    faq: &'static str,
    kb: &'static str,
    policies: &'static str,
    pages: &'static str,
    cycle: &'static str,
    price: &'static str,
    setup: &'static str,
    updated: &'static str,
    source: &'static str,
}

fn labels(locale: Locale) -> Labels {
    match locale {
        Locale::Tr => Labels {
            products: "Ürünler ve fiyatlar",
            faq: "Sıkça sorulan sorular",
            kb: "Bilgi bankası",
            policies: "Politikalar",
            pages: "Sayfalar",
            cycle: "Fatura dönemi",
            price: "Fiyat",
            setup: "Kurulum ücreti",
            updated: "Son güncelleme",
            source: "Kaynak",
        },
        Locale::En => Labels {
            products: "Products and pricing",
            faq: "Frequently asked questions",
            kb: "Knowledge base",
            policies: "Policies",
            pages: "Pages",
            cycle: "Billing cycle",
            price: "Price",
            setup: "Setup fee",
            updated: "Last updated",
            source: "Source",
        },
    }
}

/// Localised billing-cycle name.
pub fn cycle_label(cycle: BillingCycle, locale: Locale) -> &'static str {
    match (locale, cycle) {
        (Locale::Tr, BillingCycle::Monthly) => "Aylık",
        (Locale::Tr, BillingCycle::Quarterly) => "3 aylık",
        (Locale::Tr, BillingCycle::Semiannual) => "6 aylık",
        (Locale::Tr, BillingCycle::Annual) => "Yıllık",
        (Locale::Tr, BillingCycle::Biennial) => "2 yıllık",
        (Locale::Tr, BillingCycle::Triennial) => "3 yıllık",
        (Locale::Tr, BillingCycle::OneTime) => "Tek seferlik",
        (Locale::En, BillingCycle::Monthly) => "Monthly",
        (Locale::En, BillingCycle::Quarterly) => "Quarterly",
        (Locale::En, BillingCycle::Semiannual) => "Semi-annual",
        (Locale::En, BillingCycle::Annual) => "Annual",
        (Locale::En, BillingCycle::Biennial) => "Biennial",
        (Locale::En, BillingCycle::Triennial) => "Triennial",
        (Locale::En, BillingCycle::OneTime) => "One-time",
    }
}

/// Formats money for humans: `1.234,50 TRY` (tr) / `1,234.50 TRY` (en).
pub fn format_money(m: &Money, locale: Locale) -> String {
    let s = m.to_decimal_string();
    let (neg, s) = s
        .strip_prefix('-')
        .map_or((false, s.as_str()), |r| (true, r));
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let (group_sep, dec_sep) = match locale {
        Locale::Tr => ('.', ','),
        Locale::En => (',', '.'),
    };
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(group_sep);
        }
        grouped.push(c);
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&grouped);
    if !frac.is_empty() {
        out.push(dec_sep);
        out.push_str(frac);
    }
    format!("{out} {}", m.currency.as_str())
}

/// Shifts Markdown ATX headings down by `by` levels (max H6), outside code fences.
fn demote_headings(md: &str, by: usize) -> String {
    let mut in_fence = false;
    md.lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("```") || t.starts_with("~~~") {
                in_fence = !in_fence;
                return l.to_owned();
            }
            if in_fence {
                return l.to_owned();
            }
            let hashes = t.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
                let level = (hashes + by).min(6);
                format!("{}{}", "#".repeat(level), &t[hashes..])
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn doc_block(doc: &Document, base: Option<&str>, l: &Labels, level: usize) -> String {
    let mut s = format!(
        "{} {}\n",
        "#".repeat(level),
        escape_md(&text::normalize_ws(&doc.title))
    );
    let mut meta = Vec::new();
    if let Some(u) = doc.url.as_deref().and_then(escape::safe_http_url) {
        meta.push(format!("{}: <{}>", l.source, u));
    }
    if let Some(t) = doc.updated {
        meta.push(format!("{}: {}", l.updated, t.format("%Y-%m-%d")));
    }
    if !meta.is_empty() {
        s.push('\n');
        s.push_str(&meta.join("  \n"));
        s.push('\n');
    }
    let body = demote_headings(&doc.body.to_markdown(doc.url.as_deref().or(base)), level);
    if !body.trim().is_empty() {
        s.push('\n');
        s.push_str(body.trim());
        s.push('\n');
    }
    s
}

/// Renders `llms-full.txt`.
pub fn render_llms_full(full: &LlmsFull) -> String {
    let l = labels(full.locale);
    let base = full.base_url.as_deref();
    let mut out = format!("# {}\n", escape_md(&text::normalize_ws(&full.site_name)));
    let summary = text::normalize_ws(&full.summary);
    if !summary.is_empty() {
        out.push_str(&format!("\n> {}\n", escape_md(&summary)));
    }

    if !full.products.is_empty() {
        out.push_str(&format!("\n## {}\n", l.products));
        for p in &full.products {
            out.push_str(&format!(
                "\n### {}\n",
                escape_md(&text::normalize_ws(&p.name))
            ));
            let mut meta = Vec::new();
            if let Some(g) = p.group.as_deref().filter(|g| !g.trim().is_empty()) {
                meta.push(escape_md(&text::normalize_ws(g)));
            }
            if let Some(u) = p.url.as_deref().and_then(escape::safe_http_url) {
                meta.push(format!("<{u}>"));
            }
            if !meta.is_empty() {
                out.push_str(&format!("\n{}\n", meta.join(" · ")));
            }
            if let Some(d) = &p.description {
                let md = demote_headings(&d.to_markdown(p.url.as_deref().or(base)), 3);
                if !md.trim().is_empty() {
                    out.push_str(&format!("\n{}\n", md.trim()));
                }
            }
            if !p.prices.is_empty() {
                out.push_str(&format!(
                    "\n| {} | {} | {} |\n| --- | --- | --- |\n",
                    l.cycle, l.price, l.setup
                ));
                for pr in &p.prices {
                    let setup = pr
                        .setup_fee
                        .as_ref()
                        .filter(|f| f.is_positive())
                        .map_or_else(|| "—".to_owned(), |f| format_money(f, full.locale));
                    out.push_str(&format!(
                        "| {} | {} | {} |\n",
                        cycle_label(pr.cycle, full.locale),
                        format_money(&pr.price, full.locale),
                        setup
                    ));
                }
            }
            if !p.features.is_empty() {
                out.push('\n');
                for (k, v) in &p.features {
                    out.push_str(&format!(
                        "- **{}**: {}\n",
                        escape_md(&text::normalize_ws(k)),
                        escape_md(&text::normalize_ws(v))
                    ));
                }
            }
        }
    }

    if !full.faqs.is_empty() {
        out.push_str(&format!("\n## {}\n", l.faq));
        for f in &full.faqs {
            let answer = if f.answer.contains('<') {
                Body::Html(f.answer.clone()).to_markdown(base)
            } else {
                Body::Text(f.answer.clone()).to_markdown(base)
            };
            out.push_str(&format!(
                "\n### {}\n\n{}\n",
                escape_md(&text::normalize_ws(&f.question)),
                answer.trim()
            ));
        }
    }

    for (title, docs) in [
        (l.kb, &full.kb_articles),
        (l.policies, &full.policies),
        (l.pages, &full.pages),
    ] {
        if docs.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {title}\n"));
        for d in docs {
            out.push('\n');
            out.push_str(&doc_block(d, base, &l, 3));
        }
    }
    out
}

/// The `.md` rendition of one page: `# Title`, source / updated lines, body.
pub fn page_markdown(doc: &Document, locale: Locale) -> String {
    let l = labels(locale);
    let mut s = doc_block(doc, None, &l, 1);
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use hos_core::money::Currency;
    use rust_decimal_macros::dec;

    #[test]
    fn llms_txt_follows_spec_shape() {
        let txt = LlmsTxt {
            site_name: "Acme Hosting".into(),
            summary: "İstanbul merkezli web hosting, VPS ve alan adı sağlayıcısı.".into(),
            details: Some("Tüm fiyatlara KDV dahildir.\n# not a heading".into()),
            sections: vec![
                LlmsSection {
                    title: "Ürünler".into(),
                    links: vec![
                        LlmsLink::new(
                            "WordPress Hosting",
                            "https://acme.example/wordpress-hosting.md",
                        )
                        .describe("Aylık 49,90 TL'den başlayan paketler"),
                        LlmsLink::new("Bad [link]", "javascript:alert(1)"),
                        LlmsLink::new("VPS (KVM)", "https://acme.example/vps (kvm).md"),
                    ],
                },
                LlmsSection {
                    title: "Empty".into(),
                    links: vec![],
                },
            ],
            optional: vec![LlmsLink::new("Blog", "https://acme.example/blog")],
        };
        let out = txt.render();
        assert_eq!(
            out,
            "# Acme Hosting\n\n> İstanbul merkezli web hosting, VPS ve alan adı sağlayıcısı.\n\nTüm fiyatlara KDV dahildir.\n\\# not a heading\n\n## Ürünler\n\n- [WordPress Hosting](https://acme.example/wordpress-hosting.md): Aylık 49,90 TL'den başlayan paketler\n- [VPS (KVM)](https://acme.example/vps%20%28kvm%29.md)\n\n## Optional\n\n- [Blog](https://acme.example/blog)\n"
        );
        // Exactly one H1, and it is the first line.
        assert_eq!(out.lines().filter(|l| l.starts_with("# ")).count(), 1);
        assert!(out.starts_with("# "));
    }

    #[test]
    fn llms_full_dump() {
        let full = LlmsFull {
            site_name: "Acme".into(),
            summary: "Hosting.".into(),
            base_url: Some("https://acme.example/".into()),
            locale: Locale::Tr,
            products: vec![ProductEntry {
                name: "Pro Hosting".into(),
                group: Some("WordPress Hosting".into()),
                url: Some("https://acme.example/pro".into()),
                description: Some(Body::Html(
                    "<h2>Özet</h2><p>Hızlı <a href=\"/ssl\">SSL</a>.</p>".into(),
                )),
                prices: vec![
                    PriceLine {
                        cycle: BillingCycle::Monthly,
                        price: Money::new(dec!(1249.9), Currency::TRY),
                        setup_fee: Some(Money::new(dec!(25), Currency::TRY)),
                    },
                    PriceLine {
                        cycle: BillingCycle::Annual,
                        price: Money::new(dec!(499), Currency::TRY),
                        setup_fee: None,
                    },
                ],
                features: vec![("Disk".into(), "20 GB NVMe".into())],
            }],
            faqs: vec![FaqEntry::new(
                "SSL ücretsiz mi?",
                "<p>Evet, <strong>ücretsiz</strong>.</p>",
            )],
            kb_articles: vec![Document {
                title: "E-posta kurulumu".into(),
                url: Some("https://acme.example/kb/eposta".into()),
                updated: Some(Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()),
                body: Body::Markdown("## Adım 1\n\nGiriş yapın.\n\n```\n# not demoted\n```".into()),
            }],
            policies: vec![Document {
                title: "İade politikası".into(),
                url: None,
                updated: None,
                body: Body::Text("30 gün içinde iade.\n\nKoşullar geçerlidir.".into()),
            }],
            pages: vec![],
        };
        let out = render_llms_full(&full);
        assert!(out.starts_with("# Acme\n\n> Hosting.\n\n## Ürünler ve fiyatlar\n\n### Pro Hosting\n\nWordPress Hosting · <https://acme.example/pro>\n"));
        assert!(out.contains("##### Özet\n\nHızlı [SSL](https://acme.example/ssl)."));
        assert!(out.contains("| Aylık | 1.249,90 TRY | 25,00 TRY |\n| Yıllık | 499,00 TRY | — |"));
        assert!(out.contains("- **Disk**: 20 GB NVMe"));
        assert!(
            out.contains("## Sıkça sorulan sorular\n\n### SSL ücretsiz mi?\n\nEvet, **ücretsiz**.")
        );
        assert!(out.contains("## Bilgi bankası\n\n### E-posta kurulumu\n\nKaynak: <https://acme.example/kb/eposta>  \nSon güncelleme: 2026-10-01\n\n##### Adım 1"));
        assert!(out.contains("```\n# not demoted\n```"));
        assert!(out.contains(
            "## Politikalar\n\n### İade politikası\n\n30 gün içinde iade.\n\nKoşullar geçerlidir."
        ));
        assert!(!out.contains("## Sayfalar"));
    }

    #[test]
    fn page_md_and_money() {
        let doc = Document {
            title: "Hakkımızda".into(),
            url: Some("https://acme.example/hakkimizda".into()),
            updated: None,
            body: Body::Html("<main><h1>Hakkımızda</h1><p>2010'dan beri.</p></main>".into()),
        };
        assert_eq!(
            page_markdown(&doc, Locale::En),
            "# Hakkımızda\n\nSource: <https://acme.example/hakkimizda>\n\n## Hakkımızda\n\n2010'dan beri.\n"
        );
        assert_eq!(
            format_money(&Money::new(dec!(1234567.5), Currency::USD), Locale::En),
            "1,234,567.50 USD"
        );
        assert_eq!(
            format_money(&Money::new(dec!(-5), Currency::TRY), Locale::Tr),
            "-5,00 TRY"
        );
    }
}
