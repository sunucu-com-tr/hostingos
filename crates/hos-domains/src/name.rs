//! Domain name parsing, validation, IDNA and search suggestions.
//!
//! * Names are normalised with UTS #46 (IDNA 2008 + compatibility mapping) to
//!   lower-case ASCII / punycode — the form every registrar API and
//!   [`hos_core::integrations::registrar`] expect (`örnek.com.tr` →
//!   `xn--rnek-4qa.com.tr`).
//! * Turkish input is handled explicitly: `İ` (U+0130) maps to `i` instead of
//!   UTS #46's `i̇` (i + combining dot), and [`fold_turkish`] produces the ASCII
//!   variant (`şirket` → `sirket`) that most TLDs require.
//! * The SLD/TLD split uses the **embedded** Public Suffix List (`psl` crate, no
//!   network access). Only ICANN suffixes count as TLDs: `foo.blogspot.com`
//!   (a private PSL entry) is split as `blogspot` + `com`.

use std::fmt;
use std::str::FromStr;

use hos_core::IntegrationError;
use idna::uts46::{AsciiDenyList, DnsLength, Hyphens, Uts46};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::tld_rules;

/// Maximum length of a domain name in presentation form, without the root dot.
pub const MAX_DOMAIN_LEN: usize = 253;
/// Maximum length of a single label.
pub const MAX_LABEL_LEN: usize = 63;

/// Why a domain name was rejected. Messages are safe to show to users.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainNameError {
    /// Nothing left after trimming.
    #[error("domain name is empty")]
    Empty,
    /// Longer than [`MAX_DOMAIN_LEN`] after IDNA conversion.
    #[error("domain name is longer than {MAX_DOMAIN_LEN} characters")]
    TooLong,
    /// Two consecutive dots or a leading dot.
    #[error("domain name contains an empty label")]
    EmptyLabel,
    /// A label is longer than [`MAX_LABEL_LEN`] after IDNA conversion.
    #[error("label {0:?} is longer than {MAX_LABEL_LEN} characters")]
    LabelTooLong(String),
    /// Leading or trailing hyphen.
    #[error("label {0:?} must not start or end with a hyphen")]
    HyphenAtEdge(String),
    /// `--` in positions 3–4 of a label that is not punycode (`xn--`).
    #[error("label {0:?} must not contain \"--\" in the third and fourth positions")]
    ReservedHyphens(String),
    /// Characters outside letters, digits and hyphen (or invalid IDN code points).
    #[error("{0:?} contains characters that are not allowed in domain names")]
    InvalidCharacters(String),
    /// The top-level part is not a known (ICANN) public suffix.
    #[error("{0:?} is not a known top-level domain")]
    UnknownTld(String),
    /// The whole input is a public suffix (`com.tr`).
    #[error("{0:?} is a top-level domain, not a registrable domain name")]
    IsPublicSuffix(String),
    /// The input has labels left of the registrable name (`www.example.com`).
    #[error("{input:?} is a subdomain; the registrable domain is {registrable:?}")]
    Subdomain {
        /// Normalised input.
        input: String,
        /// The registrable domain it belongs to.
        registrable: String,
    },
}

impl From<DomainNameError> for IntegrationError {
    fn from(err: DomainNameError) -> Self {
        IntegrationError::InvalidInput(err.to_string())
    }
}

/// A validated, registrable domain name: exactly one label (the SLD) left of an
/// ICANN public suffix (the TLD, possibly multi-label like `com.tr`).
///
/// Stored as lower-case ASCII (punycode); [`DomainName::to_unicode`] gives the
/// display form. Serializes as the ASCII string.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DomainName {
    ascii: String,
    /// Byte offset where the TLD starts.
    tld_start: usize,
}

impl DomainName {
    /// Parses a registrable domain name (`Örnek.COM.tr.` → `xn--rnek-4qa.com.tr`).
    ///
    /// Rejects subdomains ([`DomainNameError::Subdomain`]), bare public suffixes
    /// and unknown TLDs. Use [`DomainName::from_user_input`] for free-form input.
    pub fn parse(input: &str) -> Result<Self, DomainNameError> {
        let ascii = to_ascii(input)?;
        Self::from_ascii(ascii, false)
    }

    /// Lenient parsing for search boxes and the WHOIS page: strips a URL scheme,
    /// credentials, port, path/query, a leading `www.` and any other subdomain
    /// (`https://www.Örnek.com.tr/iletisim` → `xn--rnek-4qa.com.tr`).
    pub fn from_user_input(input: &str) -> Result<Self, DomainNameError> {
        let host = extract_host(input);
        let ascii = to_ascii(host)?;
        Self::from_ascii(ascii, true)
    }

    /// Builds from an SLD and a TLD (`("örnek", ".com.tr")`).
    pub fn from_parts(sld: &str, tld: &str) -> Result<Self, DomainNameError> {
        let tld = tld.trim().trim_start_matches('.');
        Self::parse(&format!("{}.{tld}", sld.trim()))
    }

    fn from_ascii(ascii: String, reduce: bool) -> Result<Self, DomainNameError> {
        let suffix_len = icann_suffix_len(&ascii)
            .ok_or_else(|| DomainNameError::UnknownTld(top_label(&ascii).to_owned()))?;
        if suffix_len >= ascii.len() {
            return Err(DomainNameError::IsPublicSuffix(ascii));
        }
        let tld_start = ascii.len() - suffix_len;
        // `ascii[..tld_start - 1]` is everything left of the suffix's dot.
        let left = &ascii[..tld_start - 1];
        match left.rfind('.') {
            None => Ok(Self { ascii, tld_start }),
            Some(dot) => {
                let registrable = ascii[dot + 1..].to_owned();
                if reduce {
                    Ok(Self {
                        tld_start: tld_start - (dot + 1),
                        ascii: registrable,
                    })
                } else {
                    Err(DomainNameError::Subdomain {
                        input: ascii,
                        registrable,
                    })
                }
            }
        }
    }

    /// The ASCII (punycode) name, e.g. `xn--rnek-4qa.com.tr`.
    pub fn as_str(&self) -> &str {
        &self.ascii
    }

    /// Consumes into the ASCII string.
    pub fn into_string(self) -> String {
        self.ascii
    }

    /// The second-level label in ASCII (`xn--rnek-4qa`).
    pub fn sld(&self) -> &str {
        &self.ascii[..self.tld_start - 1]
    }

    /// The public suffix in ASCII without a leading dot (`com.tr`).
    pub fn tld(&self) -> &str {
        &self.ascii[self.tld_start..]
    }

    /// The last label (`tr` for `example.com.tr`) — the IANA TLD.
    pub fn top_level(&self) -> &str {
        top_label(&self.ascii)
    }

    /// Whether any label is punycode.
    pub fn is_idn(&self) -> bool {
        self.ascii.split('.').any(|l| l.starts_with("xn--"))
    }

    /// Unicode display form (`örnek.com.tr`).
    pub fn to_unicode(&self) -> String {
        to_unicode(&self.ascii)
    }

    /// Unicode SLD (`örnek`).
    pub fn sld_unicode(&self) -> String {
        to_unicode(self.sld())
    }
}

impl fmt::Display for DomainName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.ascii)
    }
}

impl fmt::Debug for DomainName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DomainName({})", self.ascii)
    }
}

impl FromStr for DomainName {
    type Err = DomainNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl AsRef<str> for DomainName {
    fn as_ref(&self) -> &str {
        &self.ascii
    }
}

impl Serialize for DomainName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.ascii)
    }
}

impl<'de> Deserialize<'de> for DomainName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Parses a registrable domain and returns `(sld, tld)` in ASCII
/// (`"örnek.com.tr"` → `("xn--rnek-4qa", "com.tr")`).
pub fn split(domain: &str) -> Result<(String, String), DomainNameError> {
    let d = DomainName::parse(domain)?;
    Ok((d.sld().to_owned(), d.tld().to_owned()))
}

/// Normalises a TLD (`".COM.TR"` → `"com.tr"`, `".türkiye"` → punycode). Errors
/// unless it is a known ICANN public suffix.
pub fn normalize_tld(tld: &str) -> Result<String, DomainNameError> {
    let trimmed = tld.trim().trim_start_matches('.');
    let ascii = to_ascii(trimmed)?;
    match icann_suffix_len(&ascii) {
        Some(len) if len == ascii.len() => Ok(ascii),
        _ => Err(DomainNameError::UnknownTld(ascii)),
    }
}

/// Converts any host name to lower-case ASCII (IDNA/punycode) and validates
/// label syntax (letters, digits, hyphen; 1–63 chars; ≤ 253 total). Does **not**
/// check the Public Suffix List. A single trailing dot is accepted and removed.
pub fn to_ascii(input: &str) -> Result<String, DomainNameError> {
    let trimmed = input.trim();
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err(DomainNameError::Empty);
    }
    let pre = normalize_turkish(trimmed);
    let ascii = Uts46::new()
        .to_ascii(
            pre.as_bytes(),
            AsciiDenyList::STD3,
            Hyphens::Allow,
            DnsLength::Ignore,
        )
        .map_err(|_| {
            if pre.split('.').any(str::is_empty) {
                DomainNameError::EmptyLabel
            } else {
                DomainNameError::InvalidCharacters(trimmed.to_owned())
            }
        })?
        .into_owned();
    validate_ascii_labels(&ascii)?;
    if ascii.len() > MAX_DOMAIN_LEN {
        return Err(DomainNameError::TooLong);
    }
    Ok(ascii)
}

/// Unicode display form of an ASCII/punycode name. Invalid punycode is returned
/// unchanged.
pub fn to_unicode(ascii: &str) -> String {
    let (unicode, result) =
        Uts46::new().to_unicode(ascii.as_bytes(), AsciiDenyList::EMPTY, Hyphens::Allow);
    if result.is_ok() {
        unicode.into_owned()
    } else {
        ascii.to_owned()
    }
}

/// Validates a host name such as a nameserver (`ns1.example.com`): ASCII/IDNA
/// conversion, label rules, at least two labels. Returns the normalised form.
pub fn normalize_hostname(host: &str) -> Result<String, DomainNameError> {
    let ascii = to_ascii(host)?;
    if !ascii.contains('.') {
        return Err(DomainNameError::InvalidCharacters(ascii));
    }
    Ok(ascii)
}

/// Pre-IDNA Turkish handling: `İ` → `i` and `i` + U+0307 → `i` (UTS #46 would
/// otherwise keep the combining dot).
pub fn normalize_turkish(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{130}' => out.push('i'),
            'i' | 'I' if chars.peek() == Some(&'\u{307}') => {
                chars.next();
                out.push('i');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Folds Turkish letters to ASCII (`ç ğ ı İ ö ş ü` → `c g i i o s u`) after
/// lower-casing. Other characters are kept.
pub fn fold_turkish(input: &str) -> String {
    normalize_turkish(input)
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| *c != '\u{307}')
        .map(|c| match c {
            'ç' => 'c',
            'ğ' => 'g',
            'ı' => 'i',
            'ö' => 'o',
            'ş' => 's',
            'ü' => 'u',
            'â' => 'a',
            'î' => 'i',
            'û' => 'u',
            other => other,
        })
        .collect()
}

/// Whether the text contains Turkish-specific letters.
pub fn has_turkish_chars(input: &str) -> bool {
    input.chars().any(|c| "çğıöşüÇĞİÖŞÜ".contains(c))
}

/// Builds candidate domains for a storefront search.
///
/// * `query` may be a keyword phrase (`"Benim Şirketim"`), a domain
///   (`"ornek.com.tr"`, which comes first in the result) or a URL.
/// * Words are joined (`benimsirketim`) and, for multi-word input, also
///   hyphenated (`benim-sirketim`).
/// * Turkish letters are folded to ASCII for every TLD; the IDN variant
///   (`benimşirketim`) is added only for TLDs whose [`tld_rules`] entry allows
///   Turkish IDNs.
/// * `tlds` accepts `"com"`, `".com.tr"`…; unknown TLDs are skipped.
///
/// The result is de-duplicated and ordered: exact domain, then per keyword
/// variant in `tlds` order.
pub fn suggest<S: AsRef<str>>(query: &str, tlds: &[S]) -> Vec<DomainName> {
    let mut out: Vec<DomainName> = Vec::new();
    let push = |d: DomainName, out: &mut Vec<DomainName>| {
        if !out.contains(&d) {
            out.push(d);
        }
    };
    let host = extract_host(query);
    let mut keyword_source = query.trim().to_owned();
    if host.contains('.') && !host.contains(char::is_whitespace) {
        if let Ok(exact) = DomainName::from_user_input(host) {
            keyword_source = exact.sld_unicode();
            push(exact, &mut out);
        } else if let Some((first, _)) = host.split_once('.') {
            keyword_source = first.to_owned();
        }
    }
    let tlds: Vec<String> = tlds
        .iter()
        .filter_map(|t| normalize_tld(t.as_ref()).ok())
        .collect();
    let (unicode_variants, ascii_variants) = keyword_variants(&keyword_source);
    for kw in &ascii_variants {
        for tld in &tlds {
            if let Ok(d) = DomainName::from_parts(kw, tld) {
                push(d, &mut out);
            }
        }
    }
    for kw in &unicode_variants {
        for tld in &tlds {
            let idn_ok = tld_rules::builtin()
                .get(tld)
                .is_some_and(|rule| rule.turkish_idn);
            if idn_ok && let Ok(d) = DomainName::from_parts(kw, tld) {
                push(d, &mut out);
            }
        }
    }
    out
}

/// Returns `(unicode_variants, ascii_variants)` for a keyword phrase.
fn keyword_variants(phrase: &str) -> (Vec<String>, Vec<String>) {
    let lowered: String = normalize_turkish(phrase)
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| *c != '\u{307}')
        .collect();
    let words: Vec<String> = lowered
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .map(|w| w.trim_matches('-').to_owned())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut unicode = vec![words.concat()];
    if words.len() > 1 {
        unicode.push(words.join("-"));
    }
    let ascii: Vec<String> = unicode.iter().map(|w| fold_turkish(w)).collect();
    let unicode = unicode
        .into_iter()
        .filter(|w| !w.is_ascii() && has_turkish_chars(w))
        .collect();
    (unicode, ascii)
}

fn validate_ascii_labels(ascii: &str) -> Result<(), DomainNameError> {
    for label in ascii.split('.') {
        if label.is_empty() {
            return Err(DomainNameError::EmptyLabel);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(DomainNameError::LabelTooLong(label.to_owned()));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(DomainNameError::InvalidCharacters(label.to_owned()));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(DomainNameError::HyphenAtEdge(label.to_owned()));
        }
        if label.len() >= 4 && &label[2..4] == "--" && !label.starts_with("xn--") {
            return Err(DomainNameError::ReservedHyphens(label.to_owned()));
        }
    }
    Ok(())
}

/// Length in bytes of the ICANN public suffix of `ascii`, or `None` for an
/// unknown TLD. Private PSL entries are skipped.
fn icann_suffix_len(ascii: &str) -> Option<usize> {
    let mut candidate = ascii;
    loop {
        let suffix = psl::suffix(candidate.as_bytes())?;
        let len = suffix.as_bytes().len();
        match suffix.typ() {
            Some(psl::Type::Icann) => return Some(len),
            Some(psl::Type::Private) => {
                let private = &candidate[candidate.len() - len..];
                candidate = private.split_once('.')?.1;
            }
            None => return None,
        }
    }
}

fn top_label(ascii: &str) -> &str {
    ascii.rsplit('.').next().unwrap_or(ascii)
}

/// Extracts the host part of free-form input (scheme, userinfo, port, path,
/// query and fragment removed).
fn extract_host(input: &str) -> &str {
    let mut s = input.trim();
    if let Some((scheme, rest)) = s.split_once("://")
        && !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        s = rest;
    }
    let end = s.find(['/', '?', '#']).unwrap_or(s.len());
    s = &s[..end];
    if let Some((_, host)) = s.rsplit_once('@') {
        s = host;
    }
    if let Some((host, port)) = s.rsplit_once(':')
        && port.chars().all(|c| c.is_ascii_digit())
    {
        s = host;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_and_multi_part_tlds() {
        let d = DomainName::parse("Example.COM").unwrap();
        assert_eq!(d.as_str(), "example.com");
        assert_eq!((d.sld(), d.tld()), ("example", "com"));
        let d = DomainName::parse("ornek.com.tr.").unwrap();
        assert_eq!((d.sld(), d.tld(), d.top_level()), ("ornek", "com.tr", "tr"));
        let d = DomainName::parse("shop.co.uk").unwrap();
        assert_eq!(d.tld(), "co.uk");
        assert_eq!(
            split("a-b.org.tr").unwrap(),
            ("a-b".into(), "org.tr".into())
        );
    }

    #[test]
    fn turkish_idn() {
        let d = DomainName::parse("Örnek.com.tr").unwrap();
        assert_eq!(d.as_str(), "xn--rnek-4qa.com.tr");
        assert!(d.is_idn());
        assert_eq!(d.to_unicode(), "örnek.com.tr");
        assert_eq!(d.sld_unicode(), "örnek");
        // Dotted capital I becomes a plain i (not i + U+0307).
        let d = DomainName::parse("İSTANBUL.com").unwrap();
        assert_eq!(d.as_str(), "istanbul.com");
        let d = DomainName::parse("ışık.com").unwrap();
        assert_eq!(d.as_str(), "xn--k-ekaa7p.com");
        assert_eq!(d.sld_unicode(), "ışık");
        assert_eq!(fold_turkish("Işık Çağrı ŞÜÖ"), "isik cagri suo");
        // punycode input round-trips
        assert_eq!(
            DomainName::parse("xn--rnek-4qa.com.tr")
                .unwrap()
                .to_unicode(),
            "örnek.com.tr"
        );
    }

    #[test]
    fn rejects_invalid_names() {
        assert_eq!(DomainName::parse("  "), Err(DomainNameError::Empty));
        assert!(matches!(
            DomainName::parse("-abc.com"),
            Err(DomainNameError::HyphenAtEdge(_))
        ));
        assert!(matches!(
            DomainName::parse("ab--cd.com"),
            Err(DomainNameError::ReservedHyphens(_))
        ));
        assert!(matches!(
            DomainName::parse("a_b.com"),
            Err(DomainNameError::InvalidCharacters(_))
        ));
        assert!(matches!(
            DomainName::parse(&format!("{}.com", "a".repeat(64))),
            Err(DomainNameError::LabelTooLong(_))
        ));
        assert_eq!(
            DomainName::parse("a..com"),
            Err(DomainNameError::EmptyLabel)
        );
        assert!(matches!(
            DomainName::parse("example.notarealtld"),
            Err(DomainNameError::UnknownTld(_))
        ));
        assert!(matches!(
            DomainName::parse("com.tr"),
            Err(DomainNameError::IsPublicSuffix(_))
        ));
        assert_eq!(
            DomainName::parse("www.example.com"),
            Err(DomainNameError::Subdomain {
                input: "www.example.com".into(),
                registrable: "example.com".into()
            })
        );
        let too_long = format!("{}.com", vec!["a".repeat(60); 5].join("."));
        assert!(matches!(to_ascii(&too_long), Err(DomainNameError::TooLong)));
        let e: IntegrationError = DomainNameError::Empty.into();
        assert_eq!(e.kind(), "invalid_input");
    }

    #[test]
    fn lenient_input() {
        let d =
            DomainName::from_user_input("https://user@www.Örnek.com.tr:8443/iletisim?x=1").unwrap();
        assert_eq!(d.as_str(), "xn--rnek-4qa.com.tr");
        let d = DomainName::from_user_input("blog.shop.example.co.uk").unwrap();
        assert_eq!(d.as_str(), "example.co.uk");
        // Private PSL entries are not TLDs for registrars.
        let d = DomainName::from_user_input("foo.blogspot.com").unwrap();
        assert_eq!((d.sld(), d.tld()), ("blogspot", "com"));
    }

    #[test]
    fn tld_normalisation_and_hostnames() {
        assert_eq!(normalize_tld(".COM.TR").unwrap(), "com.tr");
        assert_eq!(normalize_tld("co.uk").unwrap(), "co.uk");
        assert!(normalize_tld("example.com").is_err());
        assert!(normalize_tld("nope").is_err());
        assert_eq!(
            normalize_hostname("NS1.Example.com.").unwrap(),
            "ns1.example.com"
        );
        assert!(normalize_hostname("localhost").is_err());
        assert_eq!(
            DomainName::from_parts("örnek", ".com.tr").unwrap().tld(),
            "com.tr"
        );
    }

    #[test]
    fn suggestions() {
        let s = suggest("Benim Şirketim", &["com", ".com.tr", "bogus"]);
        let names: Vec<String> = s.iter().map(|d| d.to_unicode()).collect();
        assert_eq!(
            names,
            vec![
                "benimsirketim.com",
                "benimsirketim.com.tr",
                "benim-sirketim.com",
                "benim-sirketim.com.tr",
                "benimşirketim.com",
                "benim-şirketim.com",
            ]
        );
        let s = suggest("https://www.ornek.net/", &["com", "net"]);
        let names: Vec<&str> = s.iter().map(DomainName::as_str).collect();
        assert_eq!(names, vec!["ornek.net", "ornek.com"]);
        assert!(suggest("!!!", &["com"]).is_empty());
        let s = suggest("hello", &["com"]);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn serde_roundtrip() {
        let d = DomainName::parse("örnek.com").unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"xn--rnek-4qa.com\"");
        let back: DomainName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        assert!(serde_json::from_str::<DomainName>("\"bad_name.com\"").is_err());
    }
}
