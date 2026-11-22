//! Tenant-branded email layout and HTML → plain-text conversion.
//!
//! Template bodies (`email_templates.body_html`) contain only the message
//! content; [`wrap_html`] puts it into a responsive, table-based frame with the
//! tenant's logo, primary colour and footer (company name, address, support
//! address, website, unsubscribe link). [`apply`] does the same for a whole
//! [`RenderedEmail`] including the plain-text part and `List-Unsubscribe`.
//!
//! Every brand string is HTML-escaped; URLs are emitted only when they parse as
//! absolute `http(s)` URLs (so `javascript:` & co. are dropped); the colour can
//! only come from a validated [`HexColor`]. The default content style provides a
//! `.hos-button` class (brand-coloured call-to-action link) for templates.

use hos_core::domain::Locale;
use hos_core::tenant::{HexColor, TenantContext};

use crate::template::{RenderedEmail, escape_html};

/// Fallback primary colour when the tenant has none.
pub const DEFAULT_PRIMARY_COLOR: &str = "#2563eb";

/// Branding used by the layout. Build it with [`Brand::from_tenant`] and fill in
/// what the tenant settings provide (address, footer, unsubscribe URL).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Brand {
    /// Company name shown in header (when there is no logo) and footer.
    pub company_name: String,
    /// Logo URL (`http(s)` only; otherwise ignored).
    pub logo_url: Option<String>,
    /// Primary brand colour (top border, links, buttons).
    pub primary_color: Option<HexColor>,
    /// Postal address line(s) for the footer (newlines become `<br>`).
    pub address: Option<String>,
    /// Extra footer text (legal notice, registration number...).
    pub footer_text: Option<String>,
    /// Unsubscribe / notification-preferences URL (`http(s)` only).
    pub unsubscribe_url: Option<String>,
    /// Public support address.
    pub support_email: Option<String>,
    /// Public website URL (`http(s)` only).
    pub website_url: Option<String>,
}

impl Brand {
    /// Brand from the tenant context: display name, logo, primary colour,
    /// support email and `https://{primary_domain}` as website.
    pub fn from_tenant(tenant: &TenantContext) -> Self {
        let b = &tenant.branding;
        Self {
            company_name: tenant.display_name().to_owned(),
            logo_url: b.logo_url.clone(),
            primary_color: b.primary_color.clone(),
            address: None,
            footer_text: None,
            unsubscribe_url: None,
            support_email: b.support_email.clone(),
            website_url: tenant
                .primary_domain
                .as_deref()
                .filter(|d| !d.trim().is_empty())
                .map(|d| format!("https://{}", d.trim())),
        }
    }

    fn color(&self) -> &str {
        self.primary_color
            .as_ref()
            .map_or(DEFAULT_PRIMARY_COLOR, HexColor::as_str)
    }
}

/// Layout options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutOptions {
    /// Hidden preview text shown by mail clients next to the subject.
    pub preheader: Option<String>,
    /// Language of the frame texts (footer labels, `<html lang>`).
    pub locale: Locale,
}

/// Returns the URL (trimmed) when it is an absolute `http(s)` URL with a host.
pub fn safe_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some())
        .then(|| parsed.to_string())
}

/// Plausible email address for a `mailto:` link (no spaces, quotes, brackets).
fn safe_email(email: &str) -> Option<&str> {
    let e = email.trim();
    let (local, domain) = e.split_once('@')?;
    let ok = !local.is_empty()
        && domain.contains('.')
        && !e
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>\"'(),;:\\[]?&#%".contains(c))
        && !domain.contains('@');
    ok.then_some(e)
}

struct Labels {
    support: &'static str,
    unsubscribe: &'static str,
    reason: &'static str,
}

fn labels(locale: Locale) -> Labels {
    match locale {
        Locale::Tr => Labels {
            support: "Destek",
            unsubscribe: "E-posta tercihleri / abonelikten çık",
            reason: "Bu e-postayı {company} hesabınızla ilgili olduğu için aldınız.",
        },
        Locale::En => Labels {
            support: "Support",
            unsubscribe: "Email preferences / unsubscribe",
            reason: "You are receiving this email because you have an account with {company}.",
        },
    }
}

fn non_blank(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Wraps template HTML content in the tenant-branded responsive layout.
/// `content_html` is trusted markup (already-rendered template); brand values
/// are escaped here.
pub fn wrap_html(brand: &Brand, content_html: &str, opts: &LayoutOptions) -> String {
    let color = brand.color();
    let company = escape_html(brand.company_name.trim());
    let l = labels(opts.locale);

    let header = match brand.logo_url.as_deref().and_then(safe_url) {
        Some(logo) => format!(
            "<img src=\"{}\" alt=\"{company}\" height=\"40\" style=\"display:block;height:40px;max-height:40px;width:auto;border:0;outline:none;text-decoration:none;\">",
            escape_html(&logo)
        ),
        None => format!(
            "<span style=\"font-size:20px;font-weight:700;color:#111827;\">{company}</span>"
        ),
    };

    let mut footer =
        format!("<p style=\"margin:0 0 8px;font-weight:600;color:#4b5563;\">{company}</p>");
    if let Some(addr) = non_blank(&brand.address) {
        let lines: Vec<String> = addr.lines().map(|l| escape_html(l.trim())).collect();
        footer.push_str(&format!(
            "<p style=\"margin:0 0 8px;\">{}</p>",
            lines.join("<br>")
        ));
    }
    let mut links = Vec::new();
    if let Some(email) = brand.support_email.as_deref().and_then(safe_email) {
        let e = escape_html(email);
        links.push(format!(
            "{}: <a href=\"mailto:{e}\" style=\"color:{color};\">{e}</a>",
            l.support
        ));
    }
    if let Some(site) = brand.website_url.as_deref().and_then(safe_url) {
        let shown = site.trim_end_matches('/');
        links.push(format!(
            "<a href=\"{}\" style=\"color:{color};\">{}</a>",
            escape_html(&site),
            escape_html(shown.split("://").nth(1).unwrap_or(shown))
        ));
    }
    if !links.is_empty() {
        footer.push_str(&format!(
            "<p style=\"margin:0 0 8px;\">{}</p>",
            links.join(" &middot; ")
        ));
    }
    if let Some(text) = non_blank(&brand.footer_text) {
        footer.push_str(&format!(
            "<p style=\"margin:0 0 8px;\">{}</p>",
            escape_html(text)
        ));
    }
    footer.push_str(&format!(
        "<p style=\"margin:0 0 8px;\">{}</p>",
        escape_html(&l.reason.replace("{company}", brand.company_name.trim()))
    ));
    if let Some(unsub) = brand.unsubscribe_url.as_deref().and_then(safe_url) {
        footer.push_str(&format!(
            "<p style=\"margin:0;\"><a href=\"{}\" style=\"color:#6b7280;text-decoration:underline;\">{}</a></p>",
            escape_html(&unsub),
            l.unsubscribe
        ));
    }

    let preheader = non_blank(&opts.preheader)
        .map(|p| {
            format!(
                "<div style=\"display:none;max-height:0;max-width:0;overflow:hidden;opacity:0;mso-hide:all;font-size:1px;line-height:1px;color:#f4f5f7;\">{}</div>",
                escape_html(p)
            )
        })
        .unwrap_or_default();

    let font = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    format!(
        r#"<!DOCTYPE html>
<html lang="{lang}" xmlns="http://www.w3.org/1999/xhtml">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="x-apple-disable-message-reformatting">
<meta name="color-scheme" content="light">
<meta name="supported-color-schemes" content="light">
<title>{company}</title>
<style>
body{{margin:0;padding:0;background:#f4f5f7;}}
a{{color:{color};}}
.hos-content p{{margin:0 0 16px;}}
.hos-content h1,.hos-content h2{{margin:0 0 16px;line-height:1.3;color:#111827;}}
.hos-content table.hos-details{{border-collapse:collapse;margin:0 0 16px;}}
.hos-content table.hos-details td{{padding:4px 16px 4px 0;vertical-align:top;}}
a.hos-button{{display:inline-block;padding:12px 24px;border-radius:6px;background:{color};color:#ffffff !important;font-weight:600;text-decoration:none;}}
@media only screen and (max-width:620px){{
.hos-container{{width:100% !important;}}
.hos-pad{{padding-left:16px !important;padding-right:16px !important;}}
a.hos-button{{display:block;text-align:center;}}
}}
</style>
</head>
<body style="margin:0;padding:0;background:#f4f5f7;">
{preheader}<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="background:#f4f5f7;">
<tr><td align="center" style="padding:24px 12px;">
<table role="presentation" class="hos-container" width="600" cellpadding="0" cellspacing="0" border="0" style="width:600px;max-width:600px;background:#ffffff;border-radius:8px;border-top:4px solid {color};">
<tr><td class="hos-pad" style="padding:24px 32px 0;font-family:{font};">{header}</td></tr>
<tr><td class="hos-content hos-pad" style="padding:24px 32px 32px;font-family:{font};font-size:16px;line-height:1.6;color:#1f2937;">
{content_html}
</td></tr>
</table>
<table role="presentation" class="hos-container" width="600" cellpadding="0" cellspacing="0" border="0" style="width:600px;max-width:600px;">
<tr><td class="hos-pad" style="padding:24px 32px;font-family:{font};font-size:12px;line-height:1.5;color:#6b7280;text-align:center;">
{footer}
</td></tr>
</table>
</td></tr>
</table>
</body>
</html>
"#,
        lang = opts.locale.as_str(),
    )
}

/// Appends the branded footer to a plain-text body.
pub fn wrap_text(brand: &Brand, text: &str, opts: &LayoutOptions) -> String {
    let l = labels(opts.locale);
    let mut out = text.trim_end().to_owned();
    out.push_str("\n\n-- \n");
    out.push_str(brand.company_name.trim());
    if let Some(addr) = non_blank(&brand.address) {
        for line in addr.lines() {
            out.push('\n');
            out.push_str(line.trim());
        }
    }
    if let Some(email) = brand.support_email.as_deref().and_then(safe_email) {
        out.push_str(&format!("\n{}: {email}", l.support));
    }
    if let Some(site) = brand.website_url.as_deref().and_then(safe_url) {
        out.push('\n');
        out.push_str(&site);
    }
    if let Some(text) = non_blank(&brand.footer_text) {
        out.push('\n');
        out.push_str(text);
    }
    out.push('\n');
    out.push_str(&l.reason.replace("{company}", brand.company_name.trim()));
    if let Some(unsub) = brand.unsubscribe_url.as_deref().and_then(safe_url) {
        out.push_str(&format!("\n{}: {unsub}", l.unsubscribe));
    }
    out.push('\n');
    out
}

/// Applies the layout to a rendered email: wraps the HTML, appends the text
/// footer and adds `List-Unsubscribe` when the brand has an unsubscribe URL.
pub fn apply(brand: &Brand, email: RenderedEmail, opts: &LayoutOptions) -> RenderedEmail {
    let mut headers = email.headers;
    if let Some(unsub) = brand.unsubscribe_url.as_deref().and_then(safe_url)
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("List-Unsubscribe"))
    {
        headers.push(("List-Unsubscribe".to_owned(), format!("<{unsub}>")));
    }
    RenderedEmail {
        html: wrap_html(brand, &email.html, opts),
        text: wrap_text(brand, &email.text, opts),
        subject: email.subject,
        headers,
    }
}

// ---------------------------------------------------------------------------
// HTML → text
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TextWriter {
    out: String,
    pending_space: bool,
}

impl TextWriter {
    fn text(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_whitespace() {
                self.pending_space = true;
            } else {
                if self.pending_space && !self.out.is_empty() && !self.out.ends_with(['\n', ' ']) {
                    self.out.push(' ');
                }
                self.pending_space = false;
                self.out.push(c);
            }
        }
    }

    fn raw(&mut self, s: &str) {
        self.out.push_str(s);
        self.pending_space = false;
    }

    fn newline(&mut self, n: usize) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.pending_space = false;
        if self.out.is_empty() {
            return;
        }
        let have = self.out.chars().rev().take_while(|&c| c == '\n').count();
        for _ in have..n {
            self.out.push('\n');
        }
    }
}

/// Decodes the common named entities and numeric character references.
/// Unknown entities are left as-is.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // `rest` starts with the 1-byte `&`; entity names are ASCII and short.
        let decoded = rest[1..]
            .char_indices()
            .take(12)
            .find(|&(_, c)| c == ';')
            .map(|(i, _)| i)
            .and_then(|semi| {
                let name = &rest[1..=semi];
                let c = match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    "middot" => Some('·'),
                    "copy" => Some('©'),
                    "reg" => Some('®'),
                    "trade" => Some('™'),
                    "euro" => Some('€'),
                    "hellip" => Some('…'),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    "lsquo" => Some('‘'),
                    "rsquo" => Some('’'),
                    "ldquo" => Some('“'),
                    "rdquo" => Some('”'),
                    _ => name.strip_prefix('#').and_then(|num| {
                        let code = match num.strip_prefix(['x', 'X']) {
                            Some(hex) => u32::from_str_radix(hex, 16).ok(),
                            None => num.parse::<u32>().ok(),
                        };
                        code.and_then(char::from_u32)
                    }),
                };
                c.map(|c| (c, semi + 2))
            });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
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

fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lower[search..].find(name) {
        let start = search + found;
        search = start + name.len();
        let boundary_ok = start == 0
            || lower[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let after = lower[search..].trim_start();
        if !boundary_ok || !after.starts_with('=') {
            continue;
        }
        let offset = attrs.len() - after.len() + 1;
        let value = attrs[offset..].trim_start();
        let raw = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or_default(),
            _ => value
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or_default(),
        };
        return Some(decode_entities(raw.trim()));
    }
    None
}

fn skipped_element(name: &str) -> Option<&'static str> {
    match name {
        "head" => Some("head"),
        "style" => Some("style"),
        "script" => Some("script"),
        "title" => Some("title"),
        _ => None,
    }
}

/// Derives a readable plain-text version of an HTML email: drops
/// `head`/`style`/`script`, turns block elements into line breaks, list items into
/// `- ` bullets, links into `label (url)` and decodes entities.
pub fn html_to_text(html: &str) -> String {
    let mut w = TextWriter::default();
    let mut skip_until: Option<&'static str> = None;
    let mut links: Vec<(usize, Option<String>)> = Vec::new();
    let mut i = 0;

    while i < html.len() {
        let rest = &html[i..];
        if let Some(after) = rest.strip_prefix("<!--") {
            match after.find("-->") {
                Some(end) => {
                    i += 4 + end + 3;
                    continue;
                }
                None => break,
            }
        }
        let is_tag = rest.starts_with('<')
            && rest[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        if !is_tag {
            let first = rest.chars().next().map_or(1, char::len_utf8);
            let end = rest[first..].find('<').map_or(rest.len(), |p| p + first);
            if skip_until.is_none() {
                w.text(&decode_entities(&rest[..end]));
            }
            i += end;
            continue;
        }
        let Some(end) = rest.find('>') else {
            break;
        };
        let tag = &rest[1..end];
        i += end + 1;
        let closing = tag.starts_with('/');
        let body = tag.trim_start_matches('/');
        let name_len = body
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(body.len());
        let name = body[..name_len].to_ascii_lowercase();
        let attrs = &body[name_len..];

        if let Some(skip) = skip_until {
            if closing && name == skip {
                skip_until = None;
            }
            continue;
        }
        if !closing
            && !attrs.trim_end().ends_with('/')
            && let Some(skip) = skipped_element(&name)
        {
            skip_until = Some(skip);
            continue;
        }
        match name.as_str() {
            "br" => w.newline(1),
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table" | "ul" | "ol"
            | "blockquote" | "hr" | "pre" => w.newline(2),
            "div" | "tr" | "section" | "article" | "header" | "footer" => w.newline(1),
            "li" if !closing => {
                w.newline(1);
                w.raw("- ");
            }
            "td" | "th" if closing => w.text(" "),
            "a" if !closing => {
                links.push((w.out.len(), attr_value(attrs, "href")));
            }
            "a" => {
                if let Some((start, href)) = links.pop() {
                    let href = href.filter(|h| {
                        let lower = h.to_ascii_lowercase();
                        lower.starts_with("http://")
                            || lower.starts_with("https://")
                            || lower.starts_with("mailto:")
                    });
                    if let Some(href) = href {
                        let shown = href.strip_prefix("mailto:").unwrap_or(&href);
                        let label = w.out.get(start..).unwrap_or_default().trim().to_owned();
                        if label.is_empty() {
                            w.text(shown);
                        } else if label != shown && label != href {
                            w.text(&format!(" ({shown})"));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut text = String::with_capacity(w.out.len());
    let mut blank_run = 0;
    for line in w.out.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        text.push_str(line);
        text.push('\n');
    }
    text.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brand() -> Brand {
        Brand {
            company_name: "Acme <Hosting> & Co".into(),
            logo_url: Some("https://cdn.example.com/logo.png?a=1&b=2".into()),
            primary_color: Some(HexColor::new("#FF6600").unwrap()),
            address: Some("Atatürk Cad. 1\nİstanbul".into()),
            footer_text: Some("Mersis: 0123 <b>".into()),
            unsubscribe_url: Some("https://example.com/unsub?t=1".into()),
            support_email: Some("destek@example.com".into()),
            website_url: Some("https://example.com/".into()),
        }
    }

    #[test]
    fn layout_escapes_brand_and_uses_color() {
        let html = wrap_html(
            &brand(),
            "<p>Merhaba</p>",
            &LayoutOptions {
                preheader: Some("Önizleme <x>".into()),
                locale: Locale::Tr,
            },
        );
        assert!(html.contains("<html lang=\"tr\""));
        assert!(html.contains("Acme &lt;Hosting&gt; &amp; Co"));
        assert!(!html.contains("Acme <Hosting>"));
        assert!(html.contains("#ff6600"));
        assert!(html.contains("src=\"https://cdn.example.com/logo.png?a=1&amp;b=2\""));
        assert!(html.contains("Atatürk Cad. 1<br>İstanbul"));
        assert!(html.contains("Mersis: 0123 &lt;b&gt;"));
        assert!(html.contains("Önizleme &lt;x&gt;"));
        assert!(html.contains("mailto:destek@example.com"));
        assert!(html.contains("href=\"https://example.com/unsub?t=1\""));
        assert!(html.contains("<p>Merhaba</p>"));
        assert!(html.contains("abonelikten çık"));
    }

    #[test]
    fn layout_drops_unsafe_urls_and_falls_back() {
        let b = Brand {
            company_name: "X".into(),
            logo_url: Some("javascript:alert(1)".into()),
            unsubscribe_url: Some("javascript:alert(2)".into()),
            website_url: Some("data:text/html,hi".into()),
            support_email: Some("x\"onmouseover=\"y@example.com".into()),
            ..Brand::default()
        };
        let html = wrap_html(&b, "", &LayoutOptions::default());
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("data:text"));
        assert!(!html.contains("onmouseover"));
        assert!(html.contains(DEFAULT_PRIMARY_COLOR));
        assert!(
            html.contains("<span style=\"font-size:20px;font-weight:700;color:#111827;\">X</span>")
        );
        let applied = apply(&b, RenderedEmail::default(), &LayoutOptions::default());
        assert!(applied.headers.is_empty());
    }

    #[test]
    fn apply_wraps_and_adds_list_unsubscribe() {
        let email = RenderedEmail {
            subject: "S".into(),
            html: "<p>Hi</p>".into(),
            text: "Hi".into(),
            headers: Vec::new(),
        };
        let out = apply(
            &brand(),
            email,
            &LayoutOptions {
                preheader: None,
                locale: Locale::En,
            },
        );
        assert_eq!(out.subject, "S");
        assert!(out.html.contains("<p>Hi</p>"));
        assert!(
            out.text
                .starts_with("Hi\n\n-- \nAcme <Hosting> & Co\nAtatürk Cad. 1\nİstanbul")
        );
        assert!(out.text.contains("Support: destek@example.com"));
        assert!(
            out.text
                .contains("unsubscribe: https://example.com/unsub?t=1")
        );
        assert_eq!(
            out.headers,
            vec![(
                "List-Unsubscribe".to_owned(),
                "<https://example.com/unsub?t=1>".to_owned()
            )]
        );
        let msg = out.into_message(vec![hos_core::integrations::mailer::Mailbox::new("a@b.co")]);
        msg.validate().unwrap();
    }

    #[test]
    fn brand_from_tenant() {
        let tenant: TenantContext = serde_json::from_value(serde_json::json!({
            "id": "0190b2c4-0000-7000-8000-000000000000",
            "slug": "acme", "name": "Acme Ltd", "status": "active",
            "default_locale": "tr", "default_currency": "TRY", "timezone": "Europe/Istanbul",
            "primary_domain": "acme.example",
            "branding": {"company_name": "Acme", "primary_color": "#123456", "support_email": "s@acme.example"}
        }))
        .unwrap();
        let b = Brand::from_tenant(&tenant);
        assert_eq!(b.company_name, "Acme");
        assert_eq!(b.website_url.as_deref(), Some("https://acme.example"));
        assert_eq!(b.primary_color.unwrap().as_str(), "#123456");
    }

    #[test]
    fn html_to_text_conversion() {
        let html = r#"<!DOCTYPE html><html><head><title>T</title><style>p{color:red}</style></head>
<body><!-- hidden --><h1>Merhaba&nbsp;Ayşe</h1>
<p>Faturanız   hazır &amp; ödenmeyi
bekliyor.<br>İkinci satır</p>
<ul><li>Bir</li><li>İki &#8364; &#x41;</li></ul>
<p><a href="https://example.com/a?x=1&amp;y=2">Faturayı görüntüle</a>
<a href="https://example.com/same">https://example.com/same</a>
<a href="javascript:alert(1)">tıkla</a> <a href="mailto:x@example.com">x@example.com</a></p>
<script>alert(1)</script><table><tr><td>A</td><td>B</td></tr></table> 3 &lt; 4 &bogus; a < b</body></html>"#;
        let text = html_to_text(html);
        let expected = "Merhaba Ayşe\n\nFaturanız hazır & ödenmeyi bekliyor.\nİkinci satır\n\n- Bir\n- İki € A\n\nFaturayı görüntüle (https://example.com/a?x=1&y=2) https://example.com/same tıkla x@example.com\n\nA B\n\n3 < 4 &bogus; a < b";
        assert_eq!(text, expected);
    }

    #[test]
    fn html_to_text_handles_odd_input_without_panicking() {
        for s in [
            "İ<",
            "<",
            "&",
            "&#",
            "&#x;",
            "a&İ;b",
            "<a href='ş'>ş</a",
            "<!--",
            "<p",
            "ğ&amp;İ",
            "<a>x</a></a>",
            "&#1114112;",
            "<style>never closed",
            "ş<ş>ş",
        ] {
            let _ = html_to_text(s);
        }
        assert_eq!(html_to_text("ğ&amp;İ &#287;"), "ğ&İ ğ");
        assert_eq!(
            decode_entities("&#1114112; &unknown; &lt"),
            "&#1114112; &unknown; &lt"
        );
    }

    #[test]
    fn safe_url_rules() {
        assert!(safe_url("https://a.example/x").is_some());
        assert!(safe_url("http://a.example").is_some());
        assert!(safe_url("javascript:alert(1)").is_none());
        assert!(safe_url("//a.example").is_none());
        assert!(safe_url("mailto:a@b.c").is_none());
    }
}
