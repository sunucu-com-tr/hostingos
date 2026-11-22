//! Every imported string is untrusted.
//!
//! * WHMCS stores most user input HTML-escaped (`&amp;`, `&#039;`); plain
//!   fields are entity-decoded, stripped of control characters and capped
//!   ([`clean_text`], [`clean_line`]). HostingOS escapes them on output like
//!   any native value, so a decoded `<script>` is shown as text.
//! * Rich text (KB articles, announcements, product descriptions, HTML
//!   ticket bodies) becomes Markdown ([`html_to_markdown`]): structure is
//!   kept (paragraphs, headings, lists, emphasis, links, code), every other
//!   tag is dropped, `script` / `style` / `iframe` / `object` content is
//!   removed entirely and links keep only `http(s)`, `mailto` and relative
//!   targets. The result goes through the same Markdown renderer as native
//!   content (raw HTML rendered as text, `javascript:` links neutralised).

use scraper::{ElementRef, Html, Node};

/// Decodes the HTML entities WHMCS writes (named basics and numeric).
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse::<u32>().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Multi-line text: entities decoded, `\r\n` → `\n`, control characters
/// (other than newline / tab) removed, trimmed, at most `max` characters.
pub fn clean_text(s: &str, max: usize) -> String {
    decode_entities(s)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .trim()
        .chars()
        .take(max)
        .collect()
}

/// One-line text (names, subjects): like [`clean_text`] with whitespace runs collapsed.
pub fn clean_line(s: &str, max: usize) -> String {
    clean_text(s, max.saturating_mul(2))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// [`clean_line`] → `None` when empty.
pub fn opt_line(s: Option<String>, max: usize) -> Option<String> {
    s.map(|v| clean_line(&v, max)).filter(|v| !v.is_empty())
}

/// Whether `s` contains HTML markup worth converting.
pub fn looks_like_html(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    [
        "<p", "<br", "<div", "<ul", "<ol", "<li", "<a ", "<strong", "<b>", "<em", "<h1", "<h2",
        "<h3", "<table", "<span", "<img", "<script", "<iframe",
    ]
    .iter()
    .any(|t| lower.contains(t))
}

fn safe_href(href: &str) -> Option<String> {
    let h = decode_entities(href.trim());
    let lower = h.to_ascii_lowercase();
    let ok = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || (lower.starts_with('/') && !lower.starts_with("//"))
        || lower.starts_with('#');
    (ok && !h.contains(['(', ')', ' ', '<', '>', '"'])).then_some(h)
}

/// Escapes characters that would change the Markdown structure of text.
fn md_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

struct Md {
    out: String,
    list_stack: Vec<Option<usize>>,
}

impl Md {
    fn blank_line(&mut self) {
        let trimmed = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(trimmed);
        if self.out.is_empty() {
            return;
        }
        while !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn newline(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn inline_text(&mut self, el: ElementRef<'_>) -> String {
        let mut sub = Md {
            out: String::new(),
            list_stack: Vec::new(),
        };
        for child in el.children() {
            sub.node(child);
        }
        sub.out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn node(&mut self, node: ego_tree_ref::NodeRef<'_>) {
        match node.value() {
            Node::Text(t) => {
                let text: &str = t;
                let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if collapsed.is_empty() {
                    if !text.is_empty() && !self.out.ends_with([' ', '\n']) && !self.out.is_empty()
                    {
                        self.out.push(' ');
                    }
                    return;
                }
                if text.starts_with(char::is_whitespace)
                    && !self.out.ends_with([' ', '\n'])
                    && !self.out.is_empty()
                {
                    self.out.push(' ');
                }
                self.out.push_str(&md_escape(&collapsed));
                if text.ends_with(char::is_whitespace) {
                    self.out.push(' ');
                }
            }
            Node::Element(_) => {
                if let Some(el) = ElementRef::wrap(node) {
                    self.element(el);
                }
            }
            _ => {
                for child in node.children() {
                    self.node(child);
                }
            }
        }
    }

    fn children(&mut self, el: ElementRef<'_>) {
        for child in el.children() {
            self.node(child);
        }
    }

    fn element(&mut self, el: ElementRef<'_>) {
        let name = el.value().name().to_ascii_lowercase();
        match name.as_str() {
            "script" | "style" | "iframe" | "object" | "embed" | "noscript" | "template"
            | "svg" | "math" | "form" | "input" | "button" | "select" | "textarea" | "head"
            | "title" | "meta" | "link" => {}
            "br" => {
                self.out.push_str("  \n");
            }
            "hr" => {
                self.blank_line();
                self.out.push_str("---");
                self.blank_line();
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "table" | "tr" => {
                self.blank_line();
                self.children(el);
                self.blank_line();
            }
            "td" | "th" => {
                self.children(el);
                self.out.push(' ');
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name[1..].parse::<usize>().unwrap_or(2).clamp(2, 4);
                self.blank_line();
                let text = self.inline_text(el);
                if !text.is_empty() {
                    self.out.push_str(&"#".repeat(level));
                    self.out.push(' ');
                    self.out.push_str(&text);
                }
                self.blank_line();
            }
            "ul" | "ol" => {
                self.blank_line();
                self.list_stack.push((name == "ol").then_some(0));
                self.children(el);
                self.list_stack.pop();
                self.blank_line();
            }
            "li" => {
                self.newline();
                let depth = self.list_stack.len().saturating_sub(1);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{n}.")
                    }
                    _ => "-".to_owned(),
                };
                self.out.push_str(&"  ".repeat(depth));
                self.out.push_str(&marker);
                self.out.push(' ');
                let text = self.inline_text(el);
                self.out.push_str(&text);
                self.out.push('\n');
            }
            "strong" | "b" => {
                let text = self.inline_text(el);
                if !text.is_empty() {
                    self.out.push_str(&format!("**{text}**"));
                }
            }
            "em" | "i" => {
                let text = self.inline_text(el);
                if !text.is_empty() {
                    self.out.push_str(&format!("*{text}*"));
                }
            }
            "code" => {
                let text = el.text().collect::<String>().replace('`', "'");
                if !text.trim().is_empty() {
                    self.out.push_str(&format!("`{}`", text.trim()));
                }
            }
            "pre" => {
                self.blank_line();
                let text = el.text().collect::<String>().replace("```", "'''");
                self.out.push_str("```\n");
                self.out.push_str(text.trim_end());
                self.out.push_str("\n```");
                self.blank_line();
            }
            "blockquote" => {
                self.blank_line();
                let text = self.inline_text(el);
                if !text.is_empty() {
                    self.out.push_str("> ");
                    self.out.push_str(&text);
                }
                self.blank_line();
            }
            "a" => {
                let text = self.inline_text(el);
                match el.value().attr("href").and_then(safe_href) {
                    Some(href) if !text.is_empty() => {
                        self.out.push_str(&format!("[{text}]({href})"))
                    }
                    _ => self.out.push_str(&text),
                }
            }
            "img" => {
                if let Some(alt) = el
                    .value()
                    .attr("alt")
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                {
                    self.out.push_str(&md_escape(alt));
                }
            }
            _ => self.children(el),
        }
    }
}

/// `scraper` re-exports its tree node type through `ego_tree`.
mod ego_tree_ref {
    pub type NodeRef<'a> = ego_tree::NodeRef<'a, scraper::Node>;
}

/// Converts (untrusted) HTML to Markdown, at most `max` characters.
pub fn html_to_markdown(html: &str, max: usize) -> String {
    let doc = Html::parse_fragment(html);
    let mut md = Md {
        out: String::new(),
        list_stack: Vec::new(),
    };
    for child in doc.root_element().children() {
        md.node(child);
    }
    let text: String = md
        .out
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    // Collapse 3+ newlines.
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    for c in text.trim().chars() {
        if c == '\n' {
            newlines += 1;
            if newlines > 2 {
                continue;
            }
        } else {
            newlines = 0;
        }
        out.push(c);
    }
    out.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(max)
        .collect()
}

/// Rich text of unknown kind (WHMCS stores HTML, Markdown or plain text):
/// HTML is converted, anything else is entity-decoded and cleaned.
pub fn rich_text(s: &str, max: usize) -> String {
    if looks_like_html(s) {
        html_to_markdown(s, max)
    } else {
        clean_text(s, max)
    }
}

/// URL-safe ASCII slug (`Web Hosting – Pro` → `web-hosting-pro`), Turkish letters folded.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in decode_entities(s).to_lowercase().chars() {
        let mapped = match c {
            'ç' => "c",
            'ğ' => "g",
            'ı' | 'i' | 'î' | 'ï' | 'í' | 'ì' => "i",
            'ö' | 'ô' | 'ó' | 'ò' => "o",
            'ş' => "s",
            'ü' | 'û' | 'ú' | 'ù' => "u",
            'â' | 'à' | 'á' | 'ä' | 'å' => "a",
            'é' | 'è' | 'ê' | 'ë' => "e",
            'ñ' => "n",
            _ => "",
        };
        if !mapped.is_empty() {
            out.push_str(mapped);
            dash = false;
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let s: String = out.trim_end_matches('-').chars().take(70).collect();
    s.trim_end_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities() {
        assert_eq!(
            decode_entities("Tom &amp; Jerry&#039;s &lt;b&gt;"),
            "Tom & Jerry's <b>"
        );
        assert_eq!(decode_entities("&#x41;&#66;&nbsp;x"), "AB\u{a0}x");
        assert_eq!(
            decode_entities("AT&T & co &unknown; &#xZZ;"),
            "AT&T & co &unknown; &#xZZ;"
        );
        assert_eq!(decode_entities("&#0;"), "\u{0}");
    }

    #[test]
    fn text_cleaning() {
        assert_eq!(clean_text(" a\r\nb\u{0}c&amp; ", 100), "a\nbc&");
        assert_eq!(clean_line("  Ada \n\t Lovelace ", 100), "Ada Lovelace");
        assert_eq!(clean_line("abcdef", 3), "abc");
        assert_eq!(opt_line(Some("   ".into()), 10), None);
        // Decoded markup stays text; the template engine escapes it on output.
        assert_eq!(
            clean_line("&lt;script&gt;alert(1)&lt;/script&gt;", 100),
            "<script>alert(1)</script>"
        );
    }

    #[test]
    fn html_conversion_keeps_structure() {
        let md = html_to_markdown(
            "<h2>Plans</h2><p>Fast <strong>NVMe</strong> &amp; <em>free</em> SSL.<br>New line</p>\
             <ul><li>10 GB</li><li>Unlimited <b>mail</b></li></ul><ol><li>one</li><li>two</li></ol>\
             <p><a href=\"https://example.com/x\">docs</a> <a href=\"/kb\">kb</a></p>",
            10_000,
        );
        assert!(md.contains("## Plans"), "{md}");
        assert!(md.contains("Fast **NVMe** & *free* SSL."), "{md}");
        assert!(md.contains("- 10 GB"), "{md}");
        assert!(md.contains("- Unlimited **mail**"), "{md}");
        assert!(md.contains("1. one") && md.contains("2. two"), "{md}");
        assert!(
            md.contains("[docs](https://example.com/x)") && md.contains("[kb](/kb)"),
            "{md}"
        );
    }

    #[test]
    fn html_conversion_drops_active_content() {
        let md = html_to_markdown(
            "<p onclick=\"steal()\">Hi<script>alert('x')</script><style>p{}</style>\
             <iframe src=\"https://evil\"></iframe><img src=x onerror=alert(1) alt=\"pic\">\
             <a href=\"javascript:alert(1)\">click</a> <a href=\"data:text/html,x\">d</a>\
             <a href=\"//evil.example\">p</a></p><svg><script>x()</script></svg>",
            10_000,
        );
        for bad in [
            "script",
            "alert",
            "steal",
            "iframe",
            "onerror",
            "javascript",
            "data:",
            "evil",
            "<",
            "style",
        ] {
            assert!(!md.contains(bad), "{bad} in {md}");
        }
        assert!(
            md.contains("Hi") && md.contains("pic") && md.contains("click"),
            "{md}"
        );
    }

    #[test]
    fn text_that_looks_like_markdown_or_html_is_escaped() {
        let md = html_to_markdown(
            "<p>&lt;script&gt;x&lt;/script&gt; [a](javascript:x) *b*</p>",
            1000,
        );
        assert!(md.contains("\\<script\\>"), "{md}");
        assert!(md.contains("\\[a\\]"), "{md}");
        assert!(md.contains("\\*b\\*"), "{md}");
        assert_eq!(rich_text("plain &amp; simple", 100), "plain & simple");
        assert!(looks_like_html("<p>x</p>") && !looks_like_html("a < b"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Web Hosting – Pro!"), "web-hosting-pro");
        assert_eq!(
            slugify("Kurumsal Şirket Paketi Ğüç"),
            "kurumsal-sirket-paketi-guc"
        );
        assert_eq!(slugify("&amp;&amp;"), "");
        assert_eq!(slugify("---a---"), "a");
    }
}
