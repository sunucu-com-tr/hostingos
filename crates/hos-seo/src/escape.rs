//! Escaping helpers. Every string that ends up in HTML, XML or an inline
//! `<script>` produced by this crate goes through one of these.

use std::borrow::Cow;

/// Escapes text for an HTML text node (`&`, `<`, `>`, `"`, `'`).
///
/// Quotes are escaped too so the same function is safe inside attributes.
pub fn html(text: &str) -> Cow<'_, str> {
    escape_with(text, |c| match c {
        '&' => Some("&amp;"),
        '<' => Some("&lt;"),
        '>' => Some("&gt;"),
        '"' => Some("&quot;"),
        '\'' => Some("&#39;"),
        _ => None,
    })
}

/// Escapes a double-quoted HTML attribute value. Also drops ASCII control
/// characters (other than tab / newline, which become spaces).
pub fn attr(value: &str) -> Cow<'_, str> {
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '&' | '<' | '>' | '"' | '\''))
    {
        let mut out = String::with_capacity(value.len() + 8);
        for c in value.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                '\t' | '\n' | '\r' => out.push(' '),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(value)
    }
}

/// Escapes text for XML 1.0 element content or attribute values and removes
/// characters that are not allowed in XML 1.0 at all.
pub fn xml(text: &str) -> Cow<'_, str> {
    let needs = text
        .chars()
        .any(|c| matches!(c, '&' | '<' | '>' | '"' | '\'') || !is_xml_char(c));
    if !needs {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if is_xml_char(c) => out.push(c),
            _ => {}
        }
    }
    Cow::Owned(out)
}

fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

/// Serializes a JSON value so it can be embedded verbatim inside
/// `<script type="application/ld+json">`: `<`, `>`, `&`, U+2028 and U+2029 are
/// written as `\uXXXX` escapes. The result is still valid JSON that parses to
/// the same value, and it can never contain `</script>` or `<!--`.
pub fn json_for_script(value: &serde_json::Value) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + 16);
    for c in raw.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

fn escape_with(text: &str, map: impl Fn(char) -> Option<&'static str>) -> Cow<'_, str> {
    let Some(first) = text.char_indices().find(|(_, c)| map(*c).is_some()) else {
        return Cow::Borrowed(text);
    };
    let mut out = String::with_capacity(text.len() + 16);
    out.push_str(&text[..first.0]);
    for c in text[first.0..].chars() {
        match map(c) {
            Some(rep) => out.push_str(rep),
            None => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Returns the URL only if it is an absolute `http`/`https` URL (blocks
/// `javascript:`, `data:` and relative values in `href`/`content`).
pub(crate) fn safe_http_url(url: &str) -> Option<&str> {
    let trimmed = url.trim();
    let parsed = url::Url::parse(trimmed).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn html_escapes_markup_and_quotes() {
        assert_eq!(
            html(r#"<b>"x" & 'y'</b>"#),
            "&lt;b&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/b&gt;"
        );
        assert!(matches!(html("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn attr_drops_controls() {
        assert_eq!(attr("a\"b\nc\u{0}"), "a&quot;b c");
    }

    #[test]
    fn xml_removes_invalid_chars() {
        assert_eq!(xml("a&b<\u{1}>'"), "a&amp;b&lt;&gt;&apos;");
    }

    #[test]
    fn json_script_escape_is_safe_and_roundtrips() {
        let v = json!({"name": "</script><script>alert(1)</script>", "x": "a&b\u{2028}<!--"});
        let s = json_for_script(&v);
        assert!(!s.contains("</script"));
        assert!(!s.contains("<!--"));
        assert!(!s.contains('<'));
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn safe_url_rejects_scripts() {
        assert!(safe_http_url("javascript:alert(1)").is_none());
        assert!(safe_http_url("/relative").is_none());
        assert!(safe_http_url("data:text/html,x").is_none());
        assert_eq!(
            safe_http_url(" https://example.com/a "),
            Some("https://example.com/a")
        );
    }
}
