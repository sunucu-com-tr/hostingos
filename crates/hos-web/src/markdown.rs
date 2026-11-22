//! Markdown (CMS / KB / blog bodies) → safe HTML.
//!
//! Raw HTML in the source is rendered as text, `javascript:` / `data:` /
//! `vbscript:` links are neutralised, images get `loading="lazy"` and
//! headings get stable `id`s (anchors / table of contents).
//!
//! Text written by users (tickets) uses [`render_without_images`]: an image
//! becomes a plain link to it, so opening a ticket never loads a remote
//! image (tracking pixels, the reader's IP address).

use std::sync::atomic::{AtomicBool, Ordering};

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};

/// Set once in the public demo, where any page body may be visitor-written.
static UNTRUSTED_LINKS: AtomicBool = AtomicBool::new(false);

/// From now on absolute links get `rel="nofollow ugc noopener"` (public
/// demo: process-wide and one-way, like `hos_core::net::go_offline`).
pub fn treat_links_as_untrusted() {
    UNTRUSTED_LINKS.store(true, Ordering::Relaxed);
}

fn link_rel() -> &'static str {
    if UNTRUSTED_LINKS.load(Ordering::Relaxed) {
        "<a rel=\"nofollow ugc noopener\" href=\"http"
    } else {
        "<a rel=\"noopener\" href=\"http"
    }
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}

fn safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    !(lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("vbscript:"))
}

/// Slug for heading ids (`Giriş yöntemleri` → `giris-yontemleri`).
pub fn slugify(text: &str) -> String {
    let folded = hos_domains::name::fold_turkish(&text.to_lowercase());
    let mut out = String::new();
    let mut dash = false;
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').chars().take(64).collect()
}

/// A heading of the document (table of contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// 2–4.
    pub level: u8,
    /// Text.
    pub text: String,
    /// Anchor id.
    pub id: String,
}

/// Renders Markdown to HTML (see module docs). Returns the HTML and the
/// H2–H3 headings.
pub fn render_with_toc(md: &str) -> (String, Vec<Heading>) {
    render_events(md, true, true)
}

/// `images == false`: images become links to their address (or, with
/// `links == false` too, plain text). `links == false`: links are rendered
/// as their text only (the public demo, where every visitor reads what the
/// others wrote).
fn render_events(md: &str, images: bool, links: bool) -> (String, Vec<Heading>) {
    let events: Vec<Event<'_>> = Parser::new_ext(md, options()).collect();
    let mut out_events = Vec::with_capacity(events.len());
    let mut toc = Vec::new();
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::Heading { level, .. }) => {
                // Collect heading text up to the end tag.
                let level = *level;
                let mut text = String::new();
                let mut j = i + 1;
                while j < events.len() && !matches!(events[j], Event::End(TagEnd::Heading(_))) {
                    if let Event::Text(t) | Event::Code(t) = &events[j] {
                        text.push_str(t);
                    }
                    j += 1;
                }
                let n = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                let mut id = slugify(&text);
                if id.is_empty() {
                    id = format!("h-{i}");
                }
                if toc.iter().any(|h: &Heading| h.id == id) {
                    id = format!("{id}-{i}");
                }
                if (2..=3).contains(&n) {
                    toc.push(Heading {
                        level: n,
                        text: text.clone(),
                        id: id.clone(),
                    });
                }
                out_events.push(Event::Start(Tag::Heading {
                    level,
                    id: Some(CowStr::from(id)),
                    classes: vec![],
                    attrs: vec![],
                }));
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                out_events.push(Event::Text(h.clone()));
            }
            // Text only: no `<a>`, no `<img>`.
            Event::Start(Tag::Link { .. }) | Event::End(TagEnd::Link) if !links => {}
            Event::Start(Tag::Image { dest_url, .. }) if !links => {
                if matches!(events.get(i + 1), Some(Event::End(TagEnd::Image))) {
                    out_events.push(Event::Text(dest_url.clone()));
                }
            }
            Event::End(TagEnd::Image) if !links => {}
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let dest = if safe_url(dest_url) {
                    dest_url.clone()
                } else {
                    CowStr::from("#")
                };
                out_events.push(Event::Start(Tag::Link {
                    link_type: *link_type,
                    dest_url: dest,
                    title: title.clone(),
                    id: id.clone(),
                }));
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) if !images => {
                let dest = if safe_url(dest_url) {
                    dest_url.clone()
                } else {
                    CowStr::from("#")
                };
                out_events.push(Event::Start(Tag::Link {
                    link_type: *link_type,
                    dest_url: dest.clone(),
                    title: title.clone(),
                    id: id.clone(),
                }));
                // No alt text: the address itself is the link text.
                if matches!(events.get(i + 1), Some(Event::End(TagEnd::Image))) {
                    out_events.push(Event::Text(dest));
                }
            }
            Event::End(TagEnd::Image) if !images => out_events.push(Event::End(TagEnd::Link)),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let dest = if safe_url(dest_url) {
                    dest_url.clone()
                } else {
                    CowStr::from("")
                };
                out_events.push(Event::Start(Tag::Image {
                    link_type: *link_type,
                    dest_url: dest,
                    title: title.clone(),
                    id: id.clone(),
                }));
            }
            other => out_events.push(other.clone()),
        }
        i += 1;
    }
    let mut out = String::with_capacity(md.len() * 3 / 2);
    html::push_html(&mut out, out_events.into_iter());
    let out = out
        .replace("<img ", "<img loading=\"lazy\" decoding=\"async\" ")
        .replace("<a href=\"http", link_rel())
        .replace(
            "<table>",
            "<div class=\"table-wrap\"><table class=\"table\">",
        )
        .replace("</table>", "</table></div>");
    (out, toc)
}

/// Renders Markdown to safe HTML.
pub fn render(md: &str) -> String {
    render_with_toc(md).0
}

/// Like [`render`], but every image is a link to its address instead of an
/// `<img>` (user-written text; see module docs).
pub fn render_without_images(md: &str) -> String {
    render_events(md, false, true).0
}

/// Like [`render_without_images`], but links and images are shown as plain
/// text (user-written text in the public demo).
pub fn render_without_links(md: &str) -> String {
    render_events(md, false, false).0
}

/// Plain text of a Markdown document (for descriptions / excerpts).
pub fn plain_text(md: &str) -> String {
    let mut out = String::new();
    for ev in Parser::new_ext(md, options()) {
        match ev {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item) => out.push(' '),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text of the items of the first ordered list (HowTo steps).
pub fn ordered_steps(md: &str) -> Vec<String> {
    let mut steps = Vec::new();
    let mut depth = 0usize;
    let mut in_ordered = false;
    let mut done = false;
    let mut current: Option<String> = None;
    for ev in Parser::new_ext(md, options()) {
        if done {
            break;
        }
        match ev {
            Event::Start(Tag::List(first)) => {
                depth += 1;
                if depth == 1 && first.is_some() {
                    in_ordered = true;
                }
            }
            Event::End(TagEnd::List(_)) => {
                if depth == 1 && in_ordered {
                    done = true;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Start(Tag::Item) if in_ordered && depth == 1 => current = Some(String::new()),
            Event::End(TagEnd::Item) if in_ordered && depth == 1 => {
                if let Some(s) = current.take() {
                    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !s.is_empty() {
                        steps.push(s);
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(c) = current.as_mut() {
                    c.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(c) = current.as_mut() {
                    c.push(' ');
                }
            }
            _ => {}
        }
    }
    steps
}

/// Rough reading time in minutes (200 wpm, at least 1).
pub fn reading_minutes(md: &str) -> u32 {
    let words = plain_text(md).split_whitespace().count();
    u32::try_from(words.div_ceil(200))
        .unwrap_or(u32::MAX)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_safely() {
        let html = render(
            "## Giriş yöntemleri\n\n<script>alert(1)</script>\n\n[x](javascript:alert(1)) ![i](https://x/y.png)",
        );
        assert!(html.contains("<h2 id=\"giris-yontemleri\">"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("href=\"#\""));
        assert!(html.contains("loading=\"lazy\""));
    }

    #[test]
    fn user_text_links_images_instead_of_loading_them() {
        let html = render_without_images(
            "![pixel](https://tracker.example/p.png) and ![](https://x.example/a.png) ![x](javascript:alert(1))",
        );
        assert!(!html.contains("<img"), "{html}");
        assert!(
            html.contains("href=\"https://tracker.example/p.png\">pixel</a>"),
            "{html}"
        );
        assert!(html.contains(">https://x.example/a.png</a>"), "{html}");
        assert!(html.contains("href=\"#\""), "{html}");
        // CMS / KB content keeps its images.
        assert!(render("![i](https://x/y.png)").contains("<img"));
    }

    #[test]
    fn demo_user_text_has_no_links() {
        let html = render_without_links(
            "[click me](https://phish.example/) <https://auto.example/x> ![](https://x.example/a.png) ![alt](https://y.example/b.png) **bold**",
        );
        assert!(!html.contains("<a"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("click me"), "{html}");
        assert!(html.contains("https://auto.example/x"), "{html}");
        assert!(html.contains("https://x.example/a.png"), "{html}");
        assert!(html.contains("alt"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
    }

    #[test]
    fn steps_and_text() {
        let md = "Intro\n\n1. Open **My Domains**.\n2. Click Manage.\n\n- not\n- this";
        assert_eq!(ordered_steps(md), vec!["Open My Domains.", "Click Manage."]);
        assert_eq!(plain_text("## A\n\nB *c*"), "A B c");
        assert_eq!(slugify("Nameserver'lar & DNS"), "nameserver-lar-dns");
        assert_eq!(reading_minutes("a b c"), 1);
    }
}
