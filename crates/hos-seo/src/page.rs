//! Internal: one-pass extraction of everything the GEO analyser and the SEO
//! auditor look at in a rendered HTML page.

use std::collections::BTreeMap;

use scraper::{ElementRef, Html, Selector};
use serde_json::Value;

use crate::markdown::{SKIP_ROLES, SKIP_TAGS, is_hidden};
use crate::text;

#[derive(Debug, Clone, Default)]
pub(crate) struct Link {
    pub href: String,
    pub text: String,
    pub rel: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Image {
    pub src: String,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PageFacts {
    pub titles: Vec<String>,
    pub descriptions: Vec<String>,
    pub canonicals: Vec<String>,
    /// Lower-cased contents of `<meta name="robots|googlebot">`.
    pub robots: Vec<String>,
    pub lang: Option<String>,
    pub has_viewport: bool,
    /// (hreflang, href)
    pub hreflangs: Vec<(String, String)>,
    /// `<meta property|name=…>` → content (first occurrence), keys lower-cased.
    pub meta: BTreeMap<String, String>,
    /// (level, text) in document order, content area only.
    pub headings: Vec<(u8, String)>,
    /// Paragraph texts in the content area.
    pub paragraphs: Vec<String>,
    /// First paragraph after the H1 (or first paragraph).
    pub lead_paragraph: Option<String>,
    /// Visible text of the content area.
    pub main_text: String,
    pub word_count: usize,
    pub links: Vec<Link>,
    pub images: Vec<Image>,
    /// Parsed JSON-LD blocks (`Err` = invalid JSON, with the parser message).
    pub json_ld: Vec<Result<Value, String>>,
    pub tables: usize,
    pub lists: usize,
    pub details: usize,
    pub time_datetimes: Vec<String>,
    pub rel_author: usize,
    pub has_byline: bool,
}

impl PageFacts {
    pub(crate) fn meta(&self, key: &str) -> Option<&str> {
        self.meta.get(key).map(String::as_str)
    }

    /// All JSON-LD `@type`s (recursing into `@graph` and nested nodes).
    pub(crate) fn json_ld_types(&self) -> Vec<String> {
        fn walk(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                Value::Object(m) => {
                    match m.get("@type") {
                        Some(Value::String(s)) => out.push(s.clone()),
                        Some(Value::Array(a)) => {
                            out.extend(a.iter().filter_map(Value::as_str).map(str::to_owned));
                        }
                        _ => {}
                    }
                    m.values().for_each(|x| walk(x, out));
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for v in self.json_ld.iter().flatten() {
            walk(v, &mut out);
        }
        out
    }

    pub(crate) fn has_json_ld_type(&self, types: &[&str]) -> bool {
        self.json_ld_types()
            .iter()
            .any(|t| types.contains(&t.as_str()))
    }

    /// Finds the first value for `key` anywhere in the JSON-LD blocks.
    pub(crate) fn json_ld_find(&self, key: &str) -> Option<Value> {
        fn walk(v: &Value, key: &str) -> Option<Value> {
            match v {
                Value::Array(a) => a.iter().find_map(|x| walk(x, key)),
                Value::Object(m) => m
                    .get(key)
                    .cloned()
                    .or_else(|| m.values().find_map(|x| walk(x, key))),
                _ => None,
            }
        }
        self.json_ld.iter().flatten().find_map(|v| walk(v, key))
    }
}

fn sel(s: &str) -> Option<Selector> {
    Selector::parse(s).ok()
}

fn is_noise(el: &ElementRef<'_>) -> bool {
    SKIP_TAGS.contains(&el.value().name())
        || el
            .value()
            .attr("role")
            .is_some_and(|r| SKIP_ROLES.contains(&r))
        || is_hidden(el)
}

/// Whether any ancestor (up to but excluding `root`) is noise.
fn inside_noise(el: &ElementRef<'_>, root: &ElementRef<'_>) -> bool {
    if is_noise(el) {
        return true;
    }
    for a in el.ancestors().filter_map(ElementRef::wrap) {
        if a.id() == root.id() {
            return false;
        }
        if is_noise(&a) || (a.value().name() == "header" && root.value().name() == "body") {
            return true;
        }
    }
    false
}

fn visible_text(el: ElementRef<'_>, out: &mut String) {
    for child in el.children() {
        if let Some(t) = child.value().as_text() {
            out.push_str(t);
        } else if let Some(c) = ElementRef::wrap(child) {
            if is_noise(&c) {
                continue;
            }
            let block = !matches!(
                c.value().name(),
                "a" | "span"
                    | "strong"
                    | "b"
                    | "em"
                    | "i"
                    | "code"
                    | "small"
                    | "abbr"
                    | "mark"
                    | "sup"
                    | "sub"
                    | "time"
            );
            if block {
                out.push(' ');
            }
            visible_text(c, out);
            if block {
                out.push(' ');
            }
        }
    }
}

fn el_text(el: ElementRef<'_>) -> String {
    let mut s = String::new();
    visible_text(el, &mut s);
    text::normalize_ws(&s)
}

pub(crate) fn analyze(html: &str) -> PageFacts {
    let doc = Html::parse_document(html);
    let mut f = PageFacts::default();
    let all = |s: &str| -> Vec<ElementRef<'_>> {
        sel(s).map(|s| doc.select(&s).collect()).unwrap_or_default()
    };

    f.titles = all("head title, title")
        .into_iter()
        .map(|t| text::normalize_ws(&t.text().collect::<String>()))
        .collect();
    f.titles.dedup();
    f.lang = all("html")
        .first()
        .and_then(|h| h.value().attr("lang"))
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty());

    for m in all("meta") {
        let v = m.value();
        let content = v.attr("content").unwrap_or("").trim().to_owned();
        let key = v
            .attr("name")
            .or_else(|| v.attr("property"))
            .or_else(|| v.attr("itemprop"))
            .map(|k| k.trim().to_ascii_lowercase());
        let Some(key) = key else { continue };
        match key.as_str() {
            "description" => f.descriptions.push(text::normalize_ws(&content)),
            "robots" | "googlebot" => f.robots.push(content.to_ascii_lowercase()),
            "viewport" => f.has_viewport = true,
            _ => {}
        }
        f.meta.entry(key).or_insert(content);
    }
    for l in all("link[rel]") {
        let v = l.value();
        let rels: Vec<String> = v
            .attr("rel")
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let href = v.attr("href").unwrap_or("").trim().to_owned();
        if rels.iter().any(|r| r == "canonical") {
            f.canonicals.push(href.clone());
        }
        if rels.iter().any(|r| r == "alternate")
            && let Some(lang) = v.attr("hreflang")
        {
            f.hreflangs.push((lang.trim().to_owned(), href.clone()));
        }
        if rels.iter().any(|r| r == "author") {
            f.rel_author += 1;
        }
    }
    for s in all("script") {
        let ty = s
            .value()
            .attr("type")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if ty == "application/ld+json" {
            let raw: String = s.text().collect();
            f.json_ld
                .push(serde_json::from_str::<Value>(raw.trim()).map_err(|e| e.to_string()));
        }
    }

    // Content root.
    let root = ["main", "[role=main]", "article", "body"]
        .iter()
        .find_map(|s| all(s).into_iter().next())
        .unwrap_or_else(|| doc.root_element());

    let within = |s: &str| -> Vec<ElementRef<'_>> {
        sel(s)
            .map(|sel| {
                root.select(&sel)
                    .filter(|e| !inside_noise(e, &root))
                    .collect()
            })
            .unwrap_or_default()
    };

    f.headings = within("h1, h2, h3, h4, h5, h6")
        .into_iter()
        .map(|h| (h.value().name().as_bytes()[1] - b'0', el_text(h)))
        .collect();
    // H1s outside the content root (e.g. in a page header) still count.
    if root.value().name() != "body" {
        let outside_h1 = all("h1")
            .into_iter()
            .filter(|h| {
                !h.ancestors()
                    .filter_map(ElementRef::wrap)
                    .any(|a| a.id() == root.id())
            })
            .count();
        for _ in 0..outside_h1 {
            f.headings.insert(0, (1, String::new()));
        }
    }
    f.paragraphs = within("p")
        .into_iter()
        .map(el_text)
        .filter(|p| !p.is_empty())
        .collect();

    // Lead paragraph: first <p> after the first <h1> in document order.
    let mut seen_h1 = false;
    let mut first_p = None;
    let mut lead = None;
    if let Some(order_sel) = sel("h1, p") {
        for e in root.select(&order_sel) {
            if inside_noise(&e, &root) {
                continue;
            }
            match e.value().name() {
                "h1" => seen_h1 = true,
                _ => {
                    let t = el_text(e);
                    if t.is_empty() {
                        continue;
                    }
                    if first_p.is_none() {
                        first_p = Some(t.clone());
                    }
                    if seen_h1 {
                        lead = Some(t);
                        break;
                    }
                }
            }
        }
    }
    f.lead_paragraph = lead.or(first_p);

    f.main_text = el_text(root);
    f.word_count = text::word_count(&f.main_text);

    // Every link counts (navigation included) except hidden ones.
    f.links = all("a[href]")
        .into_iter()
        .filter(|a| {
            !is_hidden(a)
                && !a
                    .ancestors()
                    .filter_map(ElementRef::wrap)
                    .any(|x| is_hidden(&x) || matches!(x.value().name(), "template" | "noscript"))
        })
        .map(|a| Link {
            href: a.value().attr("href").unwrap_or("").trim().to_owned(),
            text: text::normalize_ws(&a.text().collect::<String>()),
            rel: a.value().attr("rel").unwrap_or("").to_ascii_lowercase(),
        })
        .collect();
    f.images = within("img")
        .into_iter()
        .map(|i| Image {
            src: i.value().attr("src").unwrap_or("").trim().to_owned(),
            alt: i.value().attr("alt").map(|a| a.trim().to_owned()),
        })
        .collect();
    f.tables = within("table").len();
    f.lists = within("ul, ol").len();
    f.details = within("details").len();
    // Only dates in the content area: a date in navigation or the footer
    // says nothing about the page.
    f.time_datetimes = within("time[datetime]")
        .into_iter()
        .filter_map(|t| t.value().attr("datetime").map(|d| d.trim().to_owned()))
        .collect();
    f.has_byline = all("[class], [itemprop=author], [rel=author]")
        .into_iter()
        .any(|e| {
            e.value().attr("itemprop") == Some("author")
                || e.value().classes().any(|c| {
                    let c = c.to_ascii_lowercase();
                    c.contains("author") || c.contains("byline") || c.contains("yazar")
                })
        });
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_core_facts() {
        let html = r#"<html lang="tr"><head><title> Başlık </title>
<meta name="description" content="Açıklama">
<meta name="robots" content="NOINDEX">
<meta property="og:title" content="OG">
<link rel="canonical" href="https://a.example/x">
<link rel="alternate" hreflang="en" href="https://a.example/en/x">
<script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"FAQPage"},{"@type":"Organization","name":"A"}]}</script>
<script type="application/ld+json">{broken</script>
</head><body><header><h1>Logo</h1><nav><a href="/n">Nav</a></nav></header>
<main><h2>Giriş</h2><p>İlk paragraf.</p><p>İkinci <a href="https://b.example">dış</a>.</p>
<img src="a.png"><img src="b.png" alt=""><table><tr><td>1</td></tr></table>
<p class="byline">Yazar: Ayşe</p><time datetime="2025-01-01">1 Ocak</time></main>
<footer><p>Alt bilgi</p></footer></body></html>"#;
        let f = analyze(html);
        assert_eq!(f.titles, vec!["Başlık"]);
        assert_eq!(f.descriptions, vec!["Açıklama"]);
        assert_eq!(f.robots, vec!["noindex"]);
        assert_eq!(f.lang.as_deref(), Some("tr"));
        assert_eq!(f.canonicals, vec!["https://a.example/x"]);
        assert_eq!(f.hreflangs.len(), 1);
        assert_eq!(f.meta("og:title"), Some("OG"));
        assert_eq!(f.json_ld.len(), 2);
        assert!(f.json_ld[1].is_err());
        assert!(f.has_json_ld_type(&["FAQPage"]));
        assert_eq!(f.headings.iter().filter(|h| h.0 == 1).count(), 1);
        assert_eq!(f.paragraphs.len(), 3);
        assert_eq!(f.lead_paragraph.as_deref(), Some("İlk paragraf."));
        assert!(!f.main_text.contains("Alt bilgi"));
        assert_eq!(f.images.len(), 2);
        assert_eq!(f.images[0].alt, None);
        assert_eq!(f.tables, 1);
        assert!(f.has_byline);
        assert_eq!(f.time_datetimes, vec!["2025-01-01"]);
        assert!(f.links.iter().any(|l| l.href == "/n"));
        assert!(f.links.iter().any(|l| l.href == "https://b.example"));
    }
}
