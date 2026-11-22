//! Clean HTML → Markdown conversion (CommonMark + GFM tables) for
//! `llms-full.txt`, per-page `.md` files and AI prompts.
//!
//! Noise is dropped: scripts, styles, forms, navigation, asides, footers,
//! hidden elements (`hidden`, `aria-hidden="true"`, `display:none`,
//! `sr-only`), and — when converting a whole document — everything outside
//! `<main>` / `<article>` if present. Relative links are resolved against
//! [`MarkdownOptions::base_url`]; `javascript:` links are reduced to text.

use scraper::{ElementRef, Html, Selector};
use url::Url;

use crate::text;

/// Conversion options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownOptions {
    /// Base URL to resolve relative `href` / `src` values.
    pub base_url: Option<String>,
    /// Keep `![alt](src)` images (otherwise images are dropped).
    pub keep_images: bool,
    /// For full documents, convert only `<main>` / `<article>` / `[role=main]`
    /// when present (falls back to `<body>`).
    pub main_content_only: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            keep_images: true,
            main_content_only: true,
        }
    }
}

impl MarkdownOptions {
    /// Options with a base URL.
    pub fn with_base(base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..Self::default()
        }
    }
}

/// Converts an HTML document or fragment to Markdown.
pub fn html_to_markdown(html: &str, opts: &MarkdownOptions) -> String {
    let lower_head: String = html
        .chars()
        .take(512)
        .collect::<String>()
        .to_ascii_lowercase();
    let is_document = lower_head.contains("<html")
        || lower_head.contains("<!doctype")
        || lower_head.contains("<body")
        || lower_head.contains("<head");
    let doc = if is_document {
        Html::parse_document(html)
    } else {
        Html::parse_fragment(html)
    };
    let root = pick_root(&doc, is_document && opts.main_content_only);
    let mut conv = Converter {
        base: opts.base_url.as_deref().and_then(|b| Url::parse(b).ok()),
        keep_images: opts.keep_images,
        skip_header: is_document && root.value().name() == "body",
    };
    let mut blocks = Vec::new();
    conv.blocks(root, &mut blocks);
    finish(&blocks.join("\n\n"))
}

fn pick_root(doc: &Html, main_only: bool) -> ElementRef<'_> {
    let first = |sel: &str| {
        Selector::parse(sel)
            .ok()
            .and_then(|s| doc.select(&s).next())
    };
    if main_only
        && let Some(el) = first("main")
            .or_else(|| first("[role=main]"))
            .or_else(|| first("article"))
    {
        return el;
    }
    first("body").unwrap_or_else(|| doc.root_element())
}

fn finish(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

pub(crate) const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "svg", "canvas", "form", "input",
    "button", "select", "textarea", "nav", "aside", "footer", "object", "embed", "video", "audio",
    "map", "dialog", "head", "title", "meta", "link",
];

pub(crate) const SKIP_ROLES: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "search",
    "complementary",
    "dialog",
    "alert",
];

pub(crate) fn is_hidden(el: &ElementRef<'_>) -> bool {
    let v = el.value();
    if v.attr("hidden").is_some() || v.attr("aria-hidden") == Some("true") {
        return true;
    }
    if let Some(style) = v.attr("style") {
        let s: String = style
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        if s.contains("display:none") || s.contains("visibility:hidden") {
            return true;
        }
    }
    v.classes()
        .any(|c| matches!(c, "sr-only" | "visually-hidden" | "screen-reader-text"))
}

const BLOCK_TAGS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "hr",
    "table",
    "dl",
    "div",
    "section",
    "article",
    "main",
    "header",
    "figure",
    "figcaption",
    "details",
    "summary",
    "address",
    "body",
    "html",
];

fn is_block(name: &str) -> bool {
    BLOCK_TAGS.contains(&name)
}

struct Converter {
    base: Option<Url>,
    keep_images: bool,
    skip_header: bool,
}

impl Converter {
    fn skip(&self, el: &ElementRef<'_>) -> bool {
        let name = el.value().name();
        SKIP_TAGS.contains(&name)
            || (self.skip_header && name == "header")
            || el
                .value()
                .attr("role")
                .is_some_and(|r| SKIP_ROLES.contains(&r))
            || is_hidden(el)
    }

    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        let lower = href.to_ascii_lowercase();
        if lower.starts_with("javascript:")
            || lower.starts_with("data:")
            || lower.starts_with("vbscript:")
        {
            return None;
        }
        if lower.starts_with("mailto:") || lower.starts_with("tel:") {
            return Some(href.to_owned());
        }
        match Url::parse(href) {
            Ok(u) => matches!(u.scheme(), "http" | "https").then(|| u.to_string()),
            Err(_) => match &self.base {
                Some(base) => base.join(href).ok().map(|u| u.to_string()),
                None => Some(href.to_owned()),
            },
        }
    }

    /// Converts the children of `el` into Markdown blocks.
    fn blocks(&mut self, el: ElementRef<'_>, out: &mut Vec<String>) {
        let mut para = String::new();
        for child in el.children() {
            if let Some(t) = child.value().as_text() {
                para.push_str(&inline_text(t));
                continue;
            }
            let Some(c) = ElementRef::wrap(child) else {
                continue;
            };
            if self.skip(&c) {
                continue;
            }
            let name = c.value().name();
            if is_block(name) {
                flush(&mut para, out);
                self.block(c, out);
            } else {
                para.push_str(&self.inline(c));
            }
        }
        flush(&mut para, out);
    }

    fn block(&mut self, el: ElementRef<'_>, out: &mut Vec<String>) {
        let name = el.value().name();
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(name.as_bytes()[1] - b'0');
                let t = text::normalize_ws(&self.inline_children(el));
                if !t.is_empty() {
                    out.push(format!("{} {}", "#".repeat(level), t));
                }
            }
            "p" | "summary" | "address" => {
                let t = clean_paragraph(&self.inline_children(el));
                if !t.is_empty() {
                    out.push(if name == "summary" {
                        format!("**{t}**")
                    } else {
                        t
                    });
                }
            }
            "figcaption" => {
                let t = clean_paragraph(&self.inline_children(el));
                if !t.is_empty() {
                    out.push(format!("*{t}*"));
                }
            }
            "hr" => out.push("---".into()),
            "pre" => out.push(code_block(el)),
            "ul" | "ol" => {
                let list = self.list(el, name == "ol");
                if !list.is_empty() {
                    out.push(list);
                }
            }
            "blockquote" => {
                let mut inner = Vec::new();
                self.blocks(el, &mut inner);
                let body = inner.join("\n\n");
                if !body.trim().is_empty() {
                    out.push(
                        body.lines()
                            .map(|l| {
                                if l.is_empty() {
                                    ">".to_owned()
                                } else {
                                    format!("> {l}")
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }
            "table" => {
                if let Some(t) = self.table(el) {
                    out.push(t);
                }
            }
            "dl" => {
                let mut items = Vec::new();
                let mut term = String::new();
                for c in el.child_elements() {
                    match c.value().name() {
                        "dt" => term = text::normalize_ws(&self.inline_children(c)),
                        "dd" => {
                            let def = text::normalize_ws(&self.inline_children(c));
                            if term.is_empty() {
                                items.push(format!("- {def}"));
                            } else {
                                items.push(format!("- **{term}**: {def}"));
                            }
                        }
                        _ => {}
                    }
                }
                if !items.is_empty() {
                    out.push(items.join("\n"));
                }
            }
            "li" => {
                let list = self.list_item(el, "- ");
                if !list.is_empty() {
                    out.push(list);
                }
            }
            _ => self.blocks(el, out),
        }
    }

    fn list(&mut self, el: ElementRef<'_>, ordered: bool) -> String {
        let start: usize = el
            .value()
            .attr("start")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);
        let mut lines = Vec::new();
        let mut n = start;
        for li in el.child_elements().filter(|c| c.value().name() == "li") {
            if self.skip(&li) {
                continue;
            }
            let marker = if ordered {
                format!("{n}. ")
            } else {
                "- ".to_owned()
            };
            let item = self.list_item(li, &marker);
            if !item.is_empty() {
                lines.push(item);
                n += 1;
            }
        }
        lines.join("\n")
    }

    fn list_item(&mut self, li: ElementRef<'_>, marker: &str) -> String {
        let mut blocks = Vec::new();
        self.blocks(li, &mut blocks);
        let body = blocks.join("\n");
        if body.trim().is_empty() {
            return String::new();
        }
        let indent = " ".repeat(marker.len());
        let mut out = String::new();
        for (i, line) in body.lines().enumerate() {
            if i == 0 {
                out.push_str(marker);
                out.push_str(line);
            } else {
                out.push('\n');
                if !line.is_empty() {
                    out.push_str(&indent);
                    out.push_str(line);
                }
            }
        }
        out
    }

    fn table(&mut self, el: ElementRef<'_>) -> Option<String> {
        let Ok(row_sel) = Selector::parse("tr") else {
            return None;
        };
        let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
        for tr in el.select(&row_sel) {
            // Skip rows of nested tables.
            if tr
                .ancestors()
                .filter_map(ElementRef::wrap)
                .find(|a| a.value().name() == "table")
                .is_some_and(|t| t.id() != el.id())
            {
                continue;
            }
            let mut header = false;
            let cells: Vec<String> = tr
                .child_elements()
                .filter(|c| matches!(c.value().name(), "td" | "th"))
                .map(|c| {
                    header |= c.value().name() == "th";
                    text::normalize_ws(&self.inline_children(c)).replace('|', "\\|")
                })
                .collect();
            if !cells.is_empty() {
                let in_thead = tr
                    .parent()
                    .and_then(ElementRef::wrap)
                    .is_some_and(|p| p.value().name() == "thead");
                rows.push((header || in_thead, cells));
            }
        }
        if rows.is_empty() {
            return None;
        }
        let cols = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
        let pad = |mut cells: Vec<String>| {
            cells.resize(cols, String::new());
            format!("| {} |", cells.join(" | "))
        };
        let mut iter = rows.into_iter();
        let (_, head) = iter.next()?;
        let mut out = vec![pad(head), format!("|{}|", vec![" --- "; cols].join("|"))];
        out.extend(iter.map(|(_, cells)| pad(cells)));
        Some(out.join("\n"))
    }

    fn inline_children(&mut self, el: ElementRef<'_>) -> String {
        let mut s = String::new();
        for child in el.children() {
            if let Some(t) = child.value().as_text() {
                s.push_str(&inline_text(t));
            } else if let Some(c) = ElementRef::wrap(child) {
                if self.skip(&c) {
                    continue;
                }
                if is_block(c.value().name()) {
                    s.push(' ');
                    s.push_str(&self.inline_children(c));
                    s.push(' ');
                } else {
                    s.push_str(&self.inline(c));
                }
            }
        }
        s
    }

    fn inline(&mut self, el: ElementRef<'_>) -> String {
        match el.value().name() {
            "strong" | "b" => wrap_inline(&self.inline_children(el), "**"),
            "em" | "i" | "cite" => wrap_inline(&self.inline_children(el), "*"),
            "del" | "s" | "strike" => wrap_inline(&self.inline_children(el), "~~"),
            "code" | "kbd" | "samp" => inline_code(&el.text().collect::<String>()),
            "br" => "\n".into(),
            "img" => {
                if !self.keep_images {
                    return String::new();
                }
                let alt = text::normalize_ws(el.value().attr("alt").unwrap_or(""));
                match el.value().attr("src").and_then(|s| self.resolve(s)) {
                    Some(src) => format!("![{}]({})", escape_md(&alt), url_for_md(&src)),
                    None => String::new(),
                }
            }
            "a" => {
                let label = self.inline_children(el);
                let trimmed = label.trim();
                match el.value().attr("href").and_then(|h| self.resolve(h)) {
                    Some(href) if !trimmed.is_empty() => {
                        let lead = if label.starts_with(char::is_whitespace) {
                            " "
                        } else {
                            ""
                        };
                        let trail = if label.ends_with(char::is_whitespace) {
                            " "
                        } else {
                            ""
                        };
                        format!(
                            "{lead}[{}]({}){trail}",
                            text::normalize_ws(trimmed),
                            url_for_md(&href)
                        )
                    }
                    _ => label,
                }
            }
            _ => self.inline_children(el),
        }
    }
}

fn flush(para: &mut String, out: &mut Vec<String>) {
    let t = clean_paragraph(para);
    if !t.is_empty() {
        out.push(t);
    }
    para.clear();
}

/// Collapses spaces on each line, trims, drops empty lines and escapes
/// characters that would start a block construct at line start.
fn clean_paragraph(s: &str) -> String {
    s.split('\n')
        .map(text::normalize_ws)
        .filter(|l| !l.is_empty())
        .map(|l| escape_line_start(&l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_line_start(line: &str) -> String {
    let needs = line.starts_with('#')
        || line.starts_with("> ")
        || line.starts_with("- ")
        || line.starts_with("+ ")
        || line.starts_with("=")
        || {
            let digits = line.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && line[digits..].starts_with(". ")
        };
    if !needs {
        return line.to_owned();
    }
    if line.starts_with(|c: char| c.is_ascii_digit()) {
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        format!("{}\\{}", &line[..digits], &line[digits..])
    } else {
        format!("\\{line}")
    }
}

/// Text node → Markdown-escaped text with whitespace collapsed (keeps one
/// leading / trailing space so adjacent inline nodes stay separated).
fn inline_text(t: &str) -> String {
    if t.trim().is_empty() {
        return if t.is_empty() {
            String::new()
        } else {
            " ".into()
        };
    }
    let mut s = String::new();
    if t.starts_with(char::is_whitespace) {
        s.push(' ');
    }
    s.push_str(&escape_md(&text::normalize_ws(t)));
    if t.ends_with(char::is_whitespace) {
        s.push(' ');
    }
    s
}

fn wrap_inline(inner: &str, mark: &str) -> String {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return inner.to_owned();
    }
    let lead = if inner.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let trail = if inner.ends_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    format!("{lead}{mark}{}{mark}{trail}", text::normalize_ws(trimmed))
}

fn inline_code(code: &str) -> String {
    let code = text::normalize_ws(code);
    if code.is_empty() {
        return String::new();
    }
    let longest = longest_run(&code, '`');
    let fence = "`".repeat(longest + 1);
    if code.starts_with('`') || code.ends_with('`') {
        format!("{fence} {code} {fence}")
    } else {
        format!("{fence}{code}{fence}")
    }
}

fn code_block(pre: ElementRef<'_>) -> String {
    let code_el = pre.child_elements().find(|c| c.value().name() == "code");
    let lang = code_el
        .into_iter()
        .chain(std::iter::once(pre))
        .flat_map(|e| e.value().classes())
        .find_map(|c| {
            c.strip_prefix("language-")
                .or_else(|| c.strip_prefix("lang-"))
        })
        .filter(|l| {
            l.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '_'))
        })
        .unwrap_or("");
    let body: String = pre.text().collect();
    let body = body.trim_matches('\n').trim_end();
    let fence = "`".repeat(longest_run(body, '`').max(2) + 1);
    format!("{fence}{lang}\n{body}\n{fence}")
}

fn longest_run(s: &str, ch: char) -> usize {
    let mut best = 0;
    let mut cur = 0;
    for c in s.chars() {
        if c == ch {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// Escapes Markdown punctuation in running text without making ordinary
/// prose noisy (`snake_case` and `a < b` stay readable).
pub(crate) fn escape_md(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        match c {
            '\\' | '*' | '`' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            '_' => {
                let intraword = prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric);
                if !intraword {
                    out.push('\\');
                }
                out.push('_');
            }
            '<' if next
                .is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?')) =>
            {
                out.push_str("\\<");
            }
            _ => out.push(c),
        }
    }
    out
}

/// Makes a URL safe inside `(...)` of a Markdown link.
pub(crate) fn url_for_md(url: &str) -> String {
    url.replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>T</title><style>.x{}</style></head>
<body>
<header><a href="/">Logo</a></header>
<nav><a href="/a">Menü</a></nav>
<main>
  <h1>WordPress Hosting nedir?</h1>
  <p>WordPress hosting, <strong>WordPress</strong> siteleri için optimize edilmiş bir
     <a href="/hosting">hosting</a> türüdür. Fiyatlar <em>aylık 49,90 TL</em>'den başlar.</p>
  <script>alert(1)</script>
  <div class="cookie" hidden>Çerez</div>
  <h2>Özellikler</h2>
  <ul>
    <li>20 GB <code>NVMe</code> disk</li>
    <li>Ücretsiz SSL
      <ul><li>Let's Encrypt</li></ul>
    </li>
  </ul>
  <ol start="3"><li>Üç</li><li>Dört</li></ol>
  <table>
    <thead><tr><th>Paket</th><th>Fiyat</th></tr></thead>
    <tbody><tr><td>Başlangıç</td><td>49,90 TL | ay</td></tr><tr><td>Pro</td><td>99 TL</td></tr></tbody>
  </table>
  <pre><code class="language-bash">wp plugin install `x`
echo ok</code></pre>
  <blockquote><p>Harika hizmet.</p></blockquote>
  <p><img src="/img/a.png" alt="Ekran"> <a href="javascript:alert(1)">tıkla</a> * yıldız [köşeli] snake_case _vurgu_ 1 &lt; 2</p>
  <form><input name="q"><button>Ara</button></form>
</main>
<footer>© Acme</footer>
</body></html>"#;

    #[test]
    fn converts_main_content_cleanly() {
        let md = html_to_markdown(
            PAGE,
            &MarkdownOptions::with_base("https://acme.example/blog/"),
        );
        let expected = "# WordPress Hosting nedir?

WordPress hosting, **WordPress** siteleri için optimize edilmiş bir [hosting](https://acme.example/hosting) türüdür. Fiyatlar *aylık 49,90 TL*'den başlar.

## Özellikler

- 20 GB `NVMe` disk
- Ücretsiz SSL
  - Let's Encrypt

3. Üç
4. Dört

| Paket | Fiyat |
| --- | --- |
| Başlangıç | 49,90 TL \\| ay |
| Pro | 99 TL |

```bash
wp plugin install `x`
echo ok
```

> Harika hizmet.

![Ekran](https://acme.example/img/a.png) tıkla \\* yıldız \\[köşeli\\] snake_case \\_vurgu\\_ 1 < 2
";
        assert_eq!(md, expected);
        for noise in ["Logo", "Menü", "alert", "Çerez", "Acme", "Ara"] {
            assert!(!md.contains(noise), "{noise}");
        }
    }

    #[test]
    fn fragments_and_line_start_escapes() {
        let md = html_to_markdown(
            "<p># not a heading</p><p>1. not a list<br>- nor this</p><h3> Başlık </h3><dl><dt>RAM</dt><dd>4 GB</dd></dl><hr><p><b> kalın </b>metin</p>",
            &MarkdownOptions::default(),
        );
        assert_eq!(
            md,
            "\\# not a heading\n\n1\\. not a list\n\\- nor this\n\n### Başlık\n\n- **RAM**: 4 GB\n\n---\n\n**kalın** metin\n"
        );
    }

    #[test]
    fn without_main_uses_body_minus_header() {
        let md = html_to_markdown(
            "<html><body><header>Site</header><div><p>İçerik</p></div><aside>Yan</aside></body></html>",
            &MarkdownOptions {
                keep_images: false,
                ..MarkdownOptions::default()
            },
        );
        assert_eq!(md, "İçerik\n");
    }
}
