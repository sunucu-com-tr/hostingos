//! Runs the `hos-seo` page audit and GEO analysis on a saved HTML page.
//!
//! ```text
//! curl -s http://localhost:8080/ > /tmp/home.html
//! cargo run -p hos-web --example seo_audit -- /tmp/home.html http://localhost:8080/ tr "Demo Hosting" "hosting"
//! ```
//! Arguments: `<html file> <url> [tr|en] [brand] [primary keyword]`.

use hos_core::domain::Locale;
use hos_seo::audit::{AuditOptions, AuditPage, audit_page};
use hos_seo::geo::{GeoInput, analyze};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [file, url, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: seo_audit <html file> <url> [tr|en] [brand] [keyword]");
    };
    let locale = match rest.first().map(String::as_str) {
        Some("en") => Locale::En,
        _ => Locale::Tr,
    };
    let brand = rest.get(1).map(String::as_str);
    let html = std::fs::read_to_string(file)?;

    let page = AuditPage {
        url: url.clone(),
        html: html.clone(),
        keywords: rest.get(2).cloned().into_iter().collect(),
    };
    let report = audit_page(
        &page,
        &AuditOptions {
            locale,
            ..AuditOptions::default()
        },
    );
    println!(
        "SEO score {} ({} words, ld {:?})",
        report.score, report.stats.word_count, report.stats.json_ld_types
    );
    for f in &report.findings {
        println!(
            "  [{:?}] {} — {}",
            f.severity,
            f.message(locale),
            f.hint(locale).unwrap_or_default()
        );
    }

    let mut input = GeoInput::new(&html, locale);
    input.brand = brand;
    let geo = analyze(&input);
    println!("GEO score {}", geo.score);
    for c in geo.improvements() {
        println!(
            "  [{:?} w{}] {} — {}",
            c.status,
            c.weight,
            c.message(locale),
            c.hint(locale).unwrap_or_default()
        );
    }
    Ok(())
}
