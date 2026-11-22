//! Content blocks for CMS pages: directives inside the Markdown body, so the
//! existing page editor (a Markdown textarea) can build landing pages.
//!
//! ```text
//! :::features Why {{site_name}}?
//! Optional intro paragraph.
//!
//! ### :zap: Automatic setup
//! Text of the card.
//! :::
//! ```
//!
//! A block opens with `:::<kind> [args]` on its own line and closes with a
//! line `:::`. Kinds (README "Content blocks"):
//!
//! | kind | args | body |
//! |---|---|---|
//! | `hero` | title | lead paragraph, a line of links → buttons (home / landing pages) |
//! | `features` | title | intro, then `### [:icon:] Title` + text per card |
//! | `steps` | title | intro, then `### Title` + text per step (numbered) |
//! | `faq` | title | intro, then `### Question?` + answer (accordion + `FAQPage`) |
//! | `compare` | title | intro + a Markdown table; `✓` / `✗` cells become marks |
//! | `spec` | title | a two-column Markdown table (key / value) |
//! | `pricing` | `<group-slug> [title]` | — (the group's plans with the cycle switch) |
//! | `tlds` | title | intro (the domain price table) |
//! | `cta` | title | text, a line of links → buttons |
//! | `note` | `[info\|success\|warning] [title]` | Markdown (call-out box) |
//! | `todo` | title | Markdown; shown to staff with `content.manage` only |
//!
//! Text outside blocks is ordinary Markdown. Bodies may use the tokens
//! `{{site_name}}`, `{{legal_name}}`, `{{email}}`, `{{support_email}}`,
//! `{{phone}}`, `{{address}}`, `{{year}}`, `{{url}}` and `{{base}}` (locale
//! prefix for links: `[Contact]({{base}}/contact)`), filled from the
//! tenant's settings when the page is rendered ([`Tokens`]). Unknown
//! directives are rendered as plain Markdown, so a typo never hides text.

use std::fmt::Write as _;

use rust_decimal::Decimal;

use super::ProductCard;
use super::domains::TldPriceRow;
use crate::markdown;

/// Tenant facts substituted for `{{token}}`s.
#[derive(Debug, Clone, Default)]
pub struct Tokens {
    /// `{{site_name}}`.
    pub site_name: String,
    /// `{{legal_name}}` (falls back to the site name).
    pub legal_name: String,
    /// `{{email}}`.
    pub email: String,
    /// `{{support_email}}` (falls back to `email`).
    pub support_email: String,
    /// `{{phone}}`.
    pub phone: String,
    /// `{{address}}`.
    pub address: String,
    /// `{{year}}`.
    pub year: i32,
    /// `{{url}}` (origin, no trailing slash).
    pub url: String,
    /// `{{base}}`: the locale prefix of site links (`""` or `"/en"`), so
    /// `[Contact]({{base}}/contact)` works in every language.
    pub base: String,
}

/// Escapes Markdown punctuation in a substituted value.
fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '[' | ']' | '<' | '>' | '`' | '#' | '|' | '!'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Tokens {
    /// Replaces the tokens in `body` (values are Markdown-escaped; an empty
    /// value leaves an em dash so sentences stay readable).
    pub fn apply(&self, body: &str) -> String {
        if !body.contains("{{") {
            return body.to_owned();
        }
        let or = |v: &str, fallback: &str| {
            let v = if v.trim().is_empty() { fallback } else { v };
            if v.trim().is_empty() {
                "—".to_owned()
            } else {
                md_escape(v.trim())
            }
        };
        body.replace("{{site_name}}", &or(&self.site_name, ""))
            .replace("{{legal_name}}", &or(&self.legal_name, &self.site_name))
            .replace("{{email}}", &or(&self.email, &self.support_email))
            .replace("{{support_email}}", &or(&self.support_email, &self.email))
            .replace("{{phone}}", &or(&self.phone, ""))
            .replace("{{address}}", &or(&self.address, ""))
            .replace("{{year}}", &self.year.to_string())
            .replace("{{url}}", &self.url)
            .replace("{{base}}", &self.base)
    }
}

/// A card / step / question of a block.
#[derive(Debug, Clone, Default)]
pub struct Item {
    /// Title (H3 text without the icon).
    pub title: String,
    /// Icon id from `static/img/icons.svg` (`:zap:` prefix), or empty.
    pub icon: String,
    /// Body as safe HTML.
    pub html: String,
    /// Body as plain text (JSON-LD).
    pub text: String,
}

/// A button / link of a `hero` or `cta` block.
#[derive(Debug, Clone)]
pub struct Link {
    /// Label.
    pub label: String,
    /// URL (unsafe schemes are replaced by `#`).
    pub href: String,
}

/// One parsed segment of a page body.
#[derive(Debug, Clone, Default)]
pub struct Block {
    /// `markdown`, `hero`, `features`, `steps`, `faq`, `compare`, `spec`,
    /// `pricing`, `tlds`, `cta`, `note`, `todo`.
    pub kind: &'static str,
    /// `pricing`: group slug; `note`: tone.
    pub arg: String,
    /// Heading (H2) of the block; may be empty.
    pub title: String,
    /// Anchor id of the heading.
    pub id: String,
    /// Intro (before the first item) as safe HTML.
    pub intro_html: String,
    /// Body as safe HTML (`markdown`, tables, `note`, `todo`, hero lead).
    pub html: String,
    /// Items (`features`, `steps`, `faq`).
    pub items: Vec<Item>,
    /// Buttons (`hero`, `cta`).
    pub links: Vec<Link>,
    /// The raw Markdown body (Markdown twin / llms-full.txt).
    pub source: String,
    /// `pricing`: resolved plans (filled by the handler).
    pub cards: Vec<ProductCard>,
    /// `pricing`: annual saving for the cycle switch.
    pub saving: Option<i64>,
    /// `pricing`: localized URL of the group.
    pub href: String,
    /// `tlds`: price rows (filled by the handler).
    pub tlds: Vec<TldPriceRow>,
}

const KINDS: &[&str] = &[
    "hero", "features", "steps", "faq", "compare", "spec", "pricing", "tlds", "cta", "note", "todo",
];

/// Parses a page body (tokens already applied) into blocks.
pub fn parse(body: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let opener = trimmed
            .strip_prefix(":::")
            .map(str::trim)
            .filter(|rest| !rest.is_empty());
        let Some(rest) = opener else {
            plain.push_str(line);
            plain.push('\n');
            continue;
        };
        let (kind, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let Some(kind) = KINDS.iter().copied().find(|k| k.eq_ignore_ascii_case(kind)) else {
            plain.push_str(line);
            plain.push('\n');
            continue;
        };
        let mut inner = String::new();
        let mut closed = false;
        for l in lines.by_ref() {
            if l.trim() == ":::" {
                closed = true;
                break;
            }
            inner.push_str(l);
            inner.push('\n');
        }
        if !closed {
            // An unterminated block is ordinary text.
            plain.push_str(line);
            plain.push('\n');
            plain.push_str(&inner);
            continue;
        }
        flush_markdown(&mut plain, &mut out);
        out.push(build(kind, args.trim(), &inner));
    }
    flush_markdown(&mut plain, &mut out);
    out
}

fn flush_markdown(plain: &mut String, out: &mut Vec<Block>) {
    if !plain.trim().is_empty() {
        out.push(Block {
            kind: "markdown",
            html: markdown::render(plain),
            source: plain.trim().to_owned(),
            ..Block::default()
        });
    }
    plain.clear();
}

fn heading_id(title: &str, kind: &str) -> String {
    let id = markdown::slugify(title);
    if id.is_empty() { kind.to_owned() } else { id }
}

/// Splits a body into the intro and `### ` sections.
fn sections(body: &str) -> (String, Vec<(String, String)>) {
    let mut intro = String::new();
    let mut items: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        if let Some(h) = line.trim_start().strip_prefix("### ") {
            items.push((h.trim().to_owned(), String::new()));
        } else if let Some((_, text)) = items.last_mut() {
            text.push_str(line);
            text.push('\n');
        } else {
            intro.push_str(line);
            intro.push('\n');
        }
    }
    (intro, items)
}

/// `:icon: Title` → (`icon`, `Title`).
fn split_icon(title: &str) -> (String, String) {
    if let Some(rest) = title.strip_prefix(':')
        && let Some((icon, after)) = rest.split_once(':')
        && !icon.is_empty()
        && icon
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return (icon.to_owned(), after.trim().to_owned());
    }
    (String::new(), title.to_owned())
}

fn safe_href(href: &str) -> String {
    let lower = href.trim().to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("vbscript:")
    {
        "#".into()
    } else {
        href.trim().to_owned()
    }
}

/// A line made only of Markdown links → buttons.
fn link_line(line: &str) -> Option<Vec<Link>> {
    let mut rest = line.trim();
    if !rest.starts_with('[') {
        return None;
    }
    let mut links = Vec::new();
    while !rest.is_empty() {
        let r = rest.strip_prefix('[')?;
        let (label, after) = r.split_once("](")?;
        // The closing parenthesis that balances the opening one, so a URL
        // with parentheses (or a `javascript:f(1)` attempt) parses whole.
        let mut depth = 0usize;
        let end = after.char_indices().find_map(|(i, c)| match c {
            '(' => {
                depth += 1;
                None
            }
            ')' if depth == 0 => Some(i),
            ')' => {
                depth -= 1;
                None
            }
            _ => None,
        })?;
        let (href, tail) = (&after[..end], &after[end + 1..]);
        links.push(Link {
            label: label.trim().to_owned(),
            href: safe_href(href),
        });
        rest = tail.trim_start_matches([' ', '·', '|']).trim();
    }
    (!links.is_empty()).then_some(links)
}

fn split_links(body: &str) -> (String, Vec<Link>) {
    let mut text = String::new();
    let mut links = Vec::new();
    for line in body.lines() {
        match link_line(line) {
            Some(mut l) => links.append(&mut l),
            None => {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    (text, links)
}

/// Marks `✓` / `✗` cells of a comparison table.
fn mark_cells(html: &str) -> String {
    let mut out = html.to_owned();
    for yes in ["✓", "✔", "evet", "yes", "var"] {
        out = out.replace(
            &format!("<td>{yes}</td>"),
            "<td class=\"cell-yes\"><span aria-hidden=\"true\">✓</span><span class=\"visually-hidden\">✓</span></td>",
        );
    }
    for no in ["✗", "✕", "×", "hayır", "no", "yok", "-", "—"] {
        out = out.replace(
            &format!("<td>{no}</td>"),
            "<td class=\"cell-no\"><span aria-hidden=\"true\">—</span><span class=\"visually-hidden\">✗</span></td>",
        );
    }
    out
}

fn build(kind: &'static str, args: &str, inner: &str) -> Block {
    let mut b = Block {
        kind,
        source: inner.trim().to_owned(),
        ..Block::default()
    };
    match kind {
        "pricing" | "note" => {
            let (arg, title) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
            let tone = matches!(arg, "info" | "success" | "warning");
            if kind == "note" && !tone {
                b.arg = "info".into();
                b.title = args.to_owned();
            } else {
                b.arg = arg.to_owned();
                b.title = title.trim().to_owned();
            }
        }
        _ => b.title = args.to_owned(),
    }
    b.id = heading_id(&b.title, kind);
    match kind {
        "features" | "steps" | "faq" => {
            let (intro, items) = sections(inner);
            b.intro_html = markdown::render(&intro);
            b.items = items
                .into_iter()
                .map(|(title, body)| {
                    let (icon, title) = if kind == "features" {
                        split_icon(&title)
                    } else {
                        (String::new(), title)
                    };
                    Item {
                        title,
                        icon,
                        html: markdown::render(&body),
                        text: markdown::plain_text(&body),
                    }
                })
                .filter(|i| !i.title.is_empty())
                .collect();
        }
        "compare" | "spec" => {
            let html = markdown::render(inner);
            b.html = if kind == "compare" {
                mark_cells(&html)
            } else {
                html.replace("class=\"table\"", "class=\"table table--spec\"")
            };
        }
        "hero" | "cta" => {
            let (text, links) = split_links(inner);
            b.html = markdown::render(&text);
            b.links = links;
        }
        "tlds" => b.intro_html = markdown::render(inner),
        _ => b.html = markdown::render(inner),
    }
    b
}

/// Questions and plain-text answers of every `faq` block (FAQPage JSON-LD).
pub fn faq_entries(blocks: &[Block]) -> Vec<(String, String)> {
    blocks
        .iter()
        .filter(|b| b.kind == "faq")
        .flat_map(|b| b.items.iter())
        .filter(|i| !i.text.is_empty())
        .map(|i| (i.title.clone(), i.text.clone()))
        .collect()
}

/// Group slugs referenced by `pricing` blocks.
pub fn pricing_groups(blocks: &[Block]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| b.kind == "pricing" && !b.arg.is_empty())
        .map(|b| b.arg.clone())
        .collect()
}

/// Plain text of the visible blocks (descriptions, word counts).
pub fn plain_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks.iter().filter(|b| b.kind != "todo") {
        out.push_str(&b.title);
        out.push(' ');
        out.push_str(&markdown::plain_text(&b.source));
        out.push(' ');
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First paragraph of the first Markdown or hero segment (page lead).
pub fn lead_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .find(|b| matches!(b.kind, "markdown" | "hero"))
        .and_then(|b| {
            b.source
                .split("\n\n")
                .map(str::trim)
                .find(|p| !p.is_empty() && !p.starts_with('#') && !p.starts_with('['))
                .map(markdown::plain_text)
        })
        .unwrap_or_default()
}

fn price_text(amount: &Decimal) -> String {
    amount.round_dp(2).to_string()
}

/// The blocks as Markdown (the `.md` twin, llms-full.txt): directives are
/// replaced by headings, lists and tables; `todo` blocks are dropped and
/// `pricing` / `tlds` blocks become tables (amounts with `currency`).
pub fn to_markdown(blocks: &[Block], currency: &str) -> String {
    let mut out = String::new();
    let heading = |out: &mut String, title: &str| {
        if !title.is_empty() {
            let _ = write!(out, "## {title}\n\n");
        }
    };
    for b in blocks {
        match b.kind {
            "todo" => continue,
            "markdown" | "note" | "compare" | "spec" | "hero" | "cta" => {
                heading(&mut out, &b.title);
                out.push_str(b.source.trim());
                out.push_str("\n\n");
            }
            "features" | "steps" | "faq" => {
                heading(&mut out, &b.title);
                let (intro, _) = sections(&b.source);
                if !intro.trim().is_empty() {
                    out.push_str(intro.trim());
                    out.push_str("\n\n");
                }
                for (n, i) in b.items.iter().enumerate() {
                    if b.kind == "steps" {
                        let _ = writeln!(out, "{}. **{}** — {}", n + 1, i.title, i.text);
                    } else {
                        let _ = write!(out, "### {}\n\n{}\n\n", i.title, i.text);
                    }
                }
                out.push('\n');
            }
            "pricing" => {
                heading(&mut out, &b.title);
                if !b.cards.is_empty() {
                    out.push_str("| Plan | Price | Features |\n|---|---|---|\n");
                    for c in &b.cards {
                        let price = c
                            .prices
                            .first()
                            .map(|p| {
                                format!(
                                    "{} {currency} ({})",
                                    price_text(&p.total),
                                    p.cycle.as_str()
                                )
                            })
                            .unwrap_or_else(|| "—".into());
                        let _ = writeln!(
                            out,
                            "| [{}]({}) | {price} | {} |",
                            c.name,
                            c.href,
                            c.features.join(", ").replace('|', "/")
                        );
                    }
                    out.push('\n');
                }
            }
            "tlds" => {
                heading(&mut out, &b.title);
                if !b.tlds.is_empty() {
                    out.push_str("| TLD | Register | Transfer | Renew |\n|---|---|---|---|\n");
                    let cell = |v: &Option<Decimal>| {
                        v.map(|d| format!("{} {currency}", price_text(&d)))
                            .unwrap_or_else(|| "—".into())
                    };
                    for t in &b.tlds {
                        let _ = writeln!(
                            out,
                            "| .{} | {} | {} | {} |",
                            t.tld,
                            cell(&t.register),
                            cell(&t.transfer),
                            cell(&t.renew)
                        );
                    }
                    out.push('\n');
                }
            }
            _ => {}
        }
    }
    out.trim_end().to_owned() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "Intro **text**.\n\n:::features Why us?\nLead.\n\n### :zap: Fast setup\nRight after payment.\n\n### Plain\nNo icon.\n:::\n\n:::faq Questions\n### Is SSL free?\nYes, on every plan.\n:::\n\n:::pricing web-hosting Plans\n:::\n\n:::cta Start now\nPick a plan.\n\n[Plans](/web-hosting) [Contact](/contact)\n:::\n\n:::bogus x\nstays text\n:::\n\n:::todo Fill in\nOnly staff.\n:::\n\n:::compare\n| | A | B |\n|---|---|---|\n| SSL | ✓ | ✗ |\n:::\n";

    #[test]
    fn parses_blocks() {
        let b = parse(BODY);
        let kinds: Vec<&str> = b.iter().map(|x| x.kind).collect();
        assert_eq!(
            kinds,
            [
                "markdown", "features", "faq", "pricing", "cta", "markdown", "todo", "compare"
            ]
        );
        assert_eq!(b[1].title, "Why us?");
        assert_eq!(b[1].id, "why-us");
        assert_eq!(b[1].items.len(), 2);
        assert_eq!(b[1].items[0].icon, "zap");
        assert_eq!(b[1].items[0].title, "Fast setup");
        assert!(b[1].intro_html.contains("Lead."));
        assert_eq!(
            faq_entries(&b),
            vec![("Is SSL free?".into(), "Yes, on every plan.".into())]
        );
        assert_eq!(pricing_groups(&b), vec!["web-hosting".to_owned()]);
        assert_eq!(b[3].title, "Plans");
        assert_eq!(b[4].links.len(), 2);
        assert_eq!(b[4].links[1].href, "/contact");
        assert!(
            b[5].html.contains(":::bogus x"),
            "unknown directive stays text"
        );
        assert!(b[7].html.contains("cell-yes") && b[7].html.contains("cell-no"));
        let md = to_markdown(&b, "TRY");
        assert!(md.contains("## Why us?") && md.contains("### Fast setup"));
        assert!(!md.contains("Only staff"));
        assert!(!plain_text(&b).contains("Only staff"));
        assert_eq!(lead_text(&b), "Intro text.");
    }

    #[test]
    fn unterminated_block_is_text_and_links_are_safe() {
        let b = parse(":::faq Open\n### Q?\nA");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, "markdown");
        let b = parse(":::cta Go\n[x](javascript:alert(1))\n:::");
        assert_eq!(b[0].links[0].href, "#");
    }

    #[test]
    fn tokens_are_escaped() {
        let t = Tokens {
            site_name: "Acme *Host*".into(),
            year: 2026,
            ..Tokens::default()
        };
        assert_eq!(
            t.apply("{{site_name}} © {{year}} {{phone}} {{legal_name}}"),
            "Acme \\*Host\\* © 2026 — Acme \\*Host\\*"
        );
    }
}
