//! Minimal XML writer with strict escaping, and a hardened reader.
//!
//! **Writing** — every text node and attribute value goes through
//! [`escape`]: the five XML special characters are escaped and characters
//! that are not allowed in XML 1.0 (C0 controls except tab / LF / CR,
//! U+FFFE, U+FFFF) are dropped, so client-controlled strings (names,
//! addresses, notes) can never close an element or inject markup.
//!
//! **Reading** — [`parse`] builds a small element tree with `quick-xml`.
//! A document type declaration is rejected outright (no internal or external
//! entities, no entity expansion), only the five predefined entities and
//! numeric character references are understood, and depth / element count /
//! input size are capped.

use std::fmt::Write as _;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Escapes text for element content or a double-quoted attribute value.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            '\u{FFFE}' | '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Streaming writer that keeps track of open elements.
pub struct XmlWriter {
    out: String,
    stack: Vec<&'static str>,
}

impl Default for XmlWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl XmlWriter {
    /// Starts a document with the XML declaration.
    pub fn new() -> Self {
        Self {
            out: String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"),
            stack: Vec::new(),
        }
    }

    fn start_tag(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            let _ = write!(self.out, " {k}=\"{}\"", escape(v));
        }
    }

    /// Opens `<name attrs…>`.
    pub fn open_with(&mut self, name: &'static str, attrs: &[(&str, &str)]) -> &mut Self {
        self.start_tag(name, attrs);
        self.out.push('>');
        self.stack.push(name);
        self
    }

    /// Opens `<name>`.
    pub fn open(&mut self, name: &'static str) -> &mut Self {
        self.open_with(name, &[])
    }

    /// Closes the innermost open element.
    pub fn close(&mut self) -> &mut Self {
        if let Some(name) = self.stack.pop() {
            let _ = write!(self.out, "</{name}>");
        }
        self
    }

    /// `<name attrs…>text</name>`.
    pub fn leaf_with(&mut self, name: &str, attrs: &[(&str, &str)], text: &str) -> &mut Self {
        self.start_tag(name, attrs);
        let _ = write!(self.out, ">{}</{name}>", escape(text));
        self
    }

    /// `<name>text</name>`.
    pub fn leaf(&mut self, name: &str, text: &str) -> &mut Self {
        self.leaf_with(name, &[], text)
    }

    /// `<name>text</name>` when `text` is non-empty after trimming.
    pub fn opt_leaf(&mut self, name: &str, text: Option<&str>) -> &mut Self {
        if let Some(t) = text.map(str::trim).filter(|t| !t.is_empty()) {
            self.leaf(name, t);
        }
        self
    }

    /// Closes everything still open and returns the document.
    pub fn finish(mut self) -> String {
        while !self.stack.is_empty() {
            self.close();
        }
        self.out
    }
}

/// Parse failure (never quotes the input).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum XmlError {
    /// Malformed XML.
    #[error("malformed XML: {0}")]
    Malformed(String),
    /// A DOCTYPE was present (entities are never processed).
    #[error("XML with a document type declaration is not accepted")]
    Doctype,
    /// Too deep, too many elements or too large.
    #[error("XML document exceeds the {0} limit")]
    Limit(&'static str),
}

/// Largest document [`parse`] accepts.
pub const MAX_XML_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_ELEMENTS: usize = 200_000;

/// A parsed element: local name (prefix removed), attributes (local names),
/// concatenated text and children.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    /// Local name.
    pub name: String,
    /// Attributes by local name.
    pub attrs: Vec<(String, String)>,
    /// Text content directly inside this element.
    pub text: String,
    /// Child elements.
    pub children: Vec<Element>,
}

impl Element {
    /// First direct child named `name`.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Direct children named `name`.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// First element named `name` in document order (self included).
    pub fn find(&self, name: &str) -> Option<&Element> {
        if self.name == name {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// Follows a path of direct children.
    pub fn path(&self, names: &[&str]) -> Option<&Element> {
        names.iter().try_fold(self, |e, n| e.child(n))
    }

    /// Attribute by local name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Trimmed text.
    pub fn text(&self) -> &str {
        self.text.trim()
    }
}

fn local(name: &str) -> String {
    match name.rsplit_once(':') {
        Some((_, l)) => l.to_owned(),
        None => name.to_owned(),
    }
}

fn element(start: &BytesStart<'_>) -> Result<Element, XmlError> {
    let mut attrs = Vec::new();
    for a in start.attributes() {
        let a = a.map_err(|e| XmlError::Malformed(e.to_string()))?;
        let key = local(a.key.as_ref());
        if key == "xmlns" || a.key.as_ref().starts_with("xmlns:") {
            continue;
        }
        let value = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|e| XmlError::Malformed(e.to_string()))?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(Element {
        name: local(start.local_name().as_ref()),
        attrs,
        ..Element::default()
    })
}

/// Parses `xml` into its root [`Element`] (see the module docs for the
/// hardening rules).
pub fn parse(xml: &str) -> Result<Element, XmlError> {
    if xml.len() > MAX_XML_BYTES {
        return Err(XmlError::Limit("size"));
    }
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let mut count = 0usize;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| XmlError::Malformed(e.to_string()))?;
        match event {
            Event::DocType(_) => return Err(XmlError::Doctype),
            Event::Start(start) => {
                count += 1;
                if count > MAX_ELEMENTS {
                    return Err(XmlError::Limit("element count"));
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(XmlError::Limit("depth"));
                }
                stack.push(element(&start)?);
            }
            Event::Empty(start) => {
                count += 1;
                if count > MAX_ELEMENTS {
                    return Err(XmlError::Limit("element count"));
                }
                let e = element(&start)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(e),
                    None if root.is_none() => root = Some(e),
                    None => return Err(XmlError::Malformed("several root elements".into())),
                }
            }
            Event::End(_) => {
                let done = stack
                    .pop()
                    .ok_or_else(|| XmlError::Malformed("unbalanced end tag".into()))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(done),
                    None if root.is_none() => root = Some(done),
                    None => return Err(XmlError::Malformed("several root elements".into())),
                }
            }
            Event::Text(text) => {
                if let Some(cur) = stack.last_mut() {
                    cur.text
                        .push_str(&text.xml_content(XmlVersion::Implicit1_0));
                }
            }
            Event::GeneralRef(r) => {
                // `&amp;` & co. and numeric references; anything else would
                // need a DTD, which is refused above.
                if let Some(cur) = stack.last_mut() {
                    if let Some(c) = r
                        .resolve_char_ref()
                        .map_err(|e| XmlError::Malformed(e.to_string()))?
                    {
                        cur.text.push(c);
                    } else {
                        let name: &str = r.as_ref();
                        let c = match name {
                            "amp" => '&',
                            "lt" => '<',
                            "gt" => '>',
                            "quot" => '"',
                            "apos" => '\'',
                            _ => {
                                return Err(XmlError::Malformed("undefined entity".into()));
                            }
                        };
                        cur.text.push(c);
                    }
                }
            }
            Event::CData(data) => {
                if let Some(cur) = stack.last_mut() {
                    cur.text.push_str(data.as_ref());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(XmlError::Malformed("unclosed element".into()));
    }
    root.ok_or_else(|| XmlError::Malformed("no root element".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_drops_invalid_chars() {
        assert_eq!(
            escape("<a href=\"x\">&'\u{0}\u{1b}\tok\u{FFFF}"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&apos;\tok"
        );
        let mut w = XmlWriter::new();
        w.open_with("r", &[("k", "\"><evil/>")])
            .leaf("t", "</t><injected>")
            .opt_leaf("empty", Some("  "))
            .opt_leaf("none", None);
        let xml = w.finish();
        let root = parse(&xml).unwrap();
        assert_eq!(root.attr("k"), Some("\"><evil/>"));
        assert_eq!(root.child("t").unwrap().text(), "</t><injected>");
        assert!(root.child("injected").is_none());
        assert!(root.child("empty").is_none());
    }

    #[test]
    fn rejects_doctype_and_entities() {
        let xxe = r#"<?xml version="1.0"?><!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]><r>&x;</r>"#;
        assert_eq!(parse(xxe), Err(XmlError::Doctype));
        let bomb = r#"<!DOCTYPE r [<!ENTITY a "aaaa"><!ENTITY b "&a;&a;">]><r>&b;</r>"#;
        assert_eq!(parse(bomb), Err(XmlError::Doctype));
        assert!(matches!(
            parse("<r>&undefined;</r>"),
            Err(XmlError::Malformed(_))
        ));
        assert_eq!(parse("<r>&amp;&#65;&#x42;</r>").unwrap().text(), "&AB");
    }

    #[test]
    fn namespaces_and_paths() {
        let doc = r#"<s:Envelope xmlns:s="urn:s"><s:Body><x:R xmlns:x="urn:x" x:Id="7"><x:V>1</x:V><x:V>2</x:V></x:R></s:Body></s:Envelope>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "Envelope");
        let r = root.path(&["Body", "R"]).unwrap();
        assert_eq!(r.attr("Id"), Some("7"));
        assert_eq!(r.children_named("V").count(), 2);
        assert_eq!(root.find("V").unwrap().text(), "1");
    }

    #[test]
    fn limits() {
        let deep = "<a>".repeat(100) + &"</a>".repeat(100);
        assert_eq!(parse(&deep), Err(XmlError::Limit("depth")));
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("").is_err());
    }
}
