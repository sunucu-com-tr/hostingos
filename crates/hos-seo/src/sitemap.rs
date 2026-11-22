//! XML sitemaps (sitemaps.org 0.9) with `xhtml:link` hreflang alternates and
//! the Google image extension, plus sitemap indexes.
//!
//! * [`SitemapWriter`] streams `<url>` entries into any [`std::io::Write`] and
//!   refuses an entry (returning [`SeoError::LimitReached`], nothing written)
//!   once it would exceed 50 000 URLs or 50 MiB uncompressed.
//! * [`build_sitemaps`] splits any number of URLs into as many files as needed;
//!   [`SitemapSet::index_xml`] renders the matching `<sitemapindex>`.
//! * [`SitemapIndexWriter`] streams index entries.
//!
//! `changefreq` / `priority` are supported for completeness but Google ignores
//! them; `lastmod` matters (keep it accurate). Image captions/titles were
//! deprecated by Google in 2022, so only `image:loc` is written.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::error::SeoError;
use crate::escape;
use crate::meta::{self, Alternate};

/// Maximum URLs per sitemap file (and sitemaps per index).
pub const MAX_URLS: usize = 50_000;
/// Maximum uncompressed size of one file: 50 MiB.
pub const MAX_BYTES: usize = 52_428_800;
/// Maximum images per `<url>` (Google).
pub const MAX_IMAGES_PER_URL: usize = 1_000;
/// Maximum URL length accepted by search engines.
pub const MAX_URL_LEN: usize = 2_048;

const NS: &str = "http://www.sitemaps.org/schemas/sitemap/0.9";
const NS_XHTML: &str = "http://www.w3.org/1999/xhtml";
const NS_IMAGE: &str = "http://www.google.com/schemas/sitemap-image/1.1";
const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

/// `changefreq` hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeFreq {
    /// always
    Always,
    /// hourly
    Hourly,
    /// daily
    Daily,
    /// weekly
    Weekly,
    /// monthly
    Monthly,
    /// yearly
    Yearly,
    /// never
    Never,
}

impl ChangeFreq {
    fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
            Self::Never => "never",
        }
    }
}

/// One image of a page (`image:image`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapImage {
    /// Absolute image URL.
    pub loc: String,
}

/// One `<url>` entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SitemapUrl {
    /// Absolute canonical URL.
    pub loc: String,
    /// Last significant modification.
    pub lastmod: Option<DateTime<Utc>>,
    /// Change frequency hint.
    pub changefreq: Option<ChangeFreq>,
    /// Priority 0.0–1.0 (clamped).
    pub priority: Option<f32>,
    /// hreflang alternates, **including the URL itself** (Google requirement).
    pub alternates: Vec<Alternate>,
    /// Images on the page.
    pub images: Vec<SitemapImage>,
}

impl SitemapUrl {
    /// Entry with a location only.
    pub fn new(loc: impl Into<String>) -> Self {
        Self {
            loc: loc.into(),
            ..Self::default()
        }
    }

    /// Sets `lastmod`.
    #[must_use]
    pub fn with_lastmod(mut self, lastmod: DateTime<Utc>) -> Self {
        self.lastmod = Some(lastmod);
        self
    }
}

/// Size limits of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum entries.
    pub max_urls: usize,
    /// Maximum bytes (uncompressed, including header and footer).
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_urls: MAX_URLS,
            max_bytes: MAX_BYTES,
        }
    }
}

fn valid_loc(loc: &str) -> Option<&str> {
    let url = escape::safe_http_url(loc)?;
    (url.len() <= MAX_URL_LEN).then_some(url)
}

fn w3c(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Renders one `<url>` element (without trailing newline handling by caller).
/// Invalid alternates / images are skipped; an invalid `loc` is an error.
pub fn render_url(url: &SitemapUrl) -> Result<String, SeoError> {
    let loc = valid_loc(&url.loc).ok_or_else(|| SeoError::invalid_url(&url.loc))?;
    let mut s = String::with_capacity(128 + url.alternates.len() * 96);
    s.push_str("<url><loc>");
    s.push_str(&escape::xml(loc));
    s.push_str("</loc>");
    if let Some(t) = &url.lastmod {
        s.push_str("<lastmod>");
        s.push_str(&w3c(t));
        s.push_str("</lastmod>");
    }
    if let Some(f) = url.changefreq {
        s.push_str("<changefreq>");
        s.push_str(f.as_str());
        s.push_str("</changefreq>");
    }
    if let Some(p) = url.priority.filter(|p| p.is_finite()) {
        s.push_str(&format!("<priority>{:.1}</priority>", p.clamp(0.0, 1.0)));
    }
    for alt in &url.alternates {
        let lang = alt.hreflang.trim();
        if !meta::is_valid_hreflang(lang) {
            continue;
        }
        if let Some(href) = valid_loc(&alt.url) {
            s.push_str("<xhtml:link rel=\"alternate\" hreflang=\"");
            s.push_str(&escape::xml(lang));
            s.push_str("\" href=\"");
            s.push_str(&escape::xml(href));
            s.push_str("\"/>");
        }
    }
    for img in url.images.iter().take(MAX_IMAGES_PER_URL) {
        if let Some(src) = valid_loc(&img.loc) {
            s.push_str("<image:image><image:loc>");
            s.push_str(&escape::xml(src));
            s.push_str("</image:loc></image:image>");
        }
    }
    s.push_str("</url>\n");
    Ok(s)
}

const URLSET_FOOTER: &str = "</urlset>\n";

fn urlset_header() -> String {
    format!(
        "{XML_DECL}<urlset xmlns=\"{NS}\" xmlns:xhtml=\"{NS_XHTML}\" xmlns:image=\"{NS_IMAGE}\">\n"
    )
}

/// Streaming `<urlset>` writer. See the [module docs](self).
pub struct SitemapWriter<W: Write> {
    out: W,
    limits: Limits,
    urls: usize,
    bytes: usize,
    latest: Option<DateTime<Utc>>,
}

impl<W: Write> SitemapWriter<W> {
    /// Starts a sitemap with the default limits (writes the XML header).
    pub fn new(out: W) -> Result<Self, SeoError> {
        Self::with_limits(out, Limits::default())
    }

    /// Starts a sitemap with custom limits.
    pub fn with_limits(mut out: W, limits: Limits) -> Result<Self, SeoError> {
        let header = urlset_header();
        out.write_all(header.as_bytes())?;
        Ok(Self {
            out,
            limits,
            urls: 0,
            bytes: header.len(),
            latest: None,
        })
    }

    /// Entries written so far.
    pub fn len(&self) -> usize {
        self.urls
    }

    /// Whether nothing was written yet.
    pub fn is_empty(&self) -> bool {
        self.urls == 0
    }

    /// Bytes written so far (without the footer).
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Newest `lastmod` written (for the index entry).
    pub fn latest_lastmod(&self) -> Option<DateTime<Utc>> {
        self.latest
    }

    fn fits(&self, entry_len: usize) -> bool {
        self.urls < self.limits.max_urls
            && self.bytes + entry_len + URLSET_FOOTER.len() <= self.limits.max_bytes
    }

    /// Writes one entry. Returns [`SeoError::LimitReached`] (writing nothing)
    /// when the file is full and [`SeoError::InvalidUrl`] for a bad `loc`.
    pub fn push(&mut self, url: &SitemapUrl) -> Result<(), SeoError> {
        let entry = render_url(url)?;
        self.push_rendered(&entry, url.lastmod)
    }

    fn push_rendered(
        &mut self,
        entry: &str,
        lastmod: Option<DateTime<Utc>>,
    ) -> Result<(), SeoError> {
        if !self.fits(entry.len()) {
            return Err(SeoError::LimitReached(
                if self.urls >= self.limits.max_urls {
                    "max URLs per sitemap"
                } else {
                    "max bytes per sitemap"
                },
            ));
        }
        self.out.write_all(entry.as_bytes())?;
        self.urls += 1;
        self.bytes += entry.len();
        if let Some(t) = lastmod {
            self.latest = Some(self.latest.map_or(t, |l| l.max(t)));
        }
        Ok(())
    }

    /// Writes the footer and returns the sink.
    pub fn finish(mut self) -> Result<W, SeoError> {
        self.out.write_all(URLSET_FOOTER.as_bytes())?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// One generated sitemap file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitemapFile {
    /// 1-based file number (use it in the file name: `sitemap-{n}.xml`).
    pub number: usize,
    /// XML document.
    pub xml: String,
    /// Number of URLs.
    pub url_count: usize,
    /// Newest `lastmod` in the file.
    pub lastmod: Option<DateTime<Utc>>,
}

/// Result of [`build_sitemaps`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SitemapSet {
    /// Files in order (at least one, possibly empty, when built).
    pub files: Vec<SitemapFile>,
    /// `loc` values that were rejected (not absolute http(s) / too long).
    pub rejected: Vec<String>,
}

impl SitemapSet {
    /// Whether more than one file was produced (serve an index at `/sitemap.xml`).
    pub fn needs_index(&self) -> bool {
        self.files.len() > 1
    }

    /// Renders the `<sitemapindex>`; `file_url(n)` gives the absolute URL of file `n`.
    pub fn index_xml(&self, file_url: impl Fn(usize) -> String) -> Result<String, SeoError> {
        let mut w = SitemapIndexWriter::new(Vec::new())?;
        for f in &self.files {
            w.push(&file_url(f.number), f.lastmod)?;
        }
        let bytes = w.finish()?;
        String::from_utf8(bytes).map_err(|e| SeoError::Io(e.to_string()))
    }
}

/// Splits URLs into sitemap files respecting `limits` (50 000 URLs / 50 MiB
/// by default). Invalid URLs are collected in [`SitemapSet::rejected`].
pub fn build_sitemaps(urls: impl IntoIterator<Item = SitemapUrl>, limits: Limits) -> SitemapSet {
    let mut set = SitemapSet::default();
    let mut current = new_buffer(limits);
    for url in urls {
        let entry = match render_url(&url) {
            Ok(e) => e,
            Err(_) => {
                set.rejected.push(url.loc);
                continue;
            }
        };
        if let Err(SeoError::LimitReached(_)) = current.push_rendered(&entry, url.lastmod) {
            let full = std::mem::replace(&mut current, new_buffer(limits));
            set.files.push(finish_buffer(full, set.files.len() + 1));
            if current.push_rendered(&entry, url.lastmod).is_err() {
                // A single entry larger than a whole file: cannot be stored.
                set.rejected.push(url.loc);
            }
        }
    }
    if !current.is_empty() || set.files.is_empty() {
        set.files.push(finish_buffer(current, set.files.len() + 1));
    }
    set
}

fn new_buffer(limits: Limits) -> SitemapWriter<Vec<u8>> {
    let header = urlset_header();
    SitemapWriter {
        bytes: header.len(),
        out: header.into_bytes(),
        limits,
        urls: 0,
        latest: None,
    }
}

fn finish_buffer(w: SitemapWriter<Vec<u8>>, number: usize) -> SitemapFile {
    let url_count = w.urls;
    let lastmod = w.latest;
    let mut bytes = w.out;
    bytes.extend_from_slice(URLSET_FOOTER.as_bytes());
    SitemapFile {
        number,
        // Only valid UTF-8 was ever written.
        xml: String::from_utf8_lossy(&bytes).into_owned(),
        url_count,
        lastmod,
    }
}

const INDEX_FOOTER: &str = "</sitemapindex>\n";

/// Streaming `<sitemapindex>` writer (same limits as sitemaps).
pub struct SitemapIndexWriter<W: Write> {
    out: W,
    count: usize,
    bytes: usize,
}

impl<W: Write> SitemapIndexWriter<W> {
    /// Starts an index.
    pub fn new(mut out: W) -> Result<Self, SeoError> {
        let header = format!("{XML_DECL}<sitemapindex xmlns=\"{NS}\">\n");
        out.write_all(header.as_bytes())?;
        Ok(Self {
            out,
            count: 0,
            bytes: header.len(),
        })
    }

    /// Adds a sitemap reference.
    pub fn push(&mut self, loc: &str, lastmod: Option<DateTime<Utc>>) -> Result<(), SeoError> {
        let loc = valid_loc(loc).ok_or_else(|| SeoError::invalid_url(loc))?;
        let mut entry = format!("<sitemap><loc>{}</loc>", escape::xml(loc));
        if let Some(t) = lastmod {
            entry.push_str(&format!("<lastmod>{}</lastmod>", w3c(&t)));
        }
        entry.push_str("</sitemap>\n");
        if self.count >= MAX_URLS || self.bytes + entry.len() + INDEX_FOOTER.len() > MAX_BYTES {
            return Err(SeoError::LimitReached("max sitemaps per index"));
        }
        self.out.write_all(entry.as_bytes())?;
        self.count += 1;
        self.bytes += entry.len();
        Ok(())
    }

    /// Writes the footer and returns the sink.
    pub fn finish(mut self) -> Result<W, SeoError> {
        self.out.write_all(INDEX_FOOTER.as_bytes())?;
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use quick_xml::Reader;
    use quick_xml::events::Event;

    /// Parses the document fully; returns (number of <url> or <sitemap> elements, root name).
    fn parse(xml: &str) -> (usize, String) {
        let mut reader = Reader::from_str(xml);
        let mut count = 0;
        let mut root = String::new();
        let mut depth = 0usize;
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let name = e.name().0.to_owned();
                    if depth == 0 {
                        root = name.clone();
                    }
                    if depth == 1 && (name == "url" || name == "sitemap") {
                        count += 1;
                    }
                    depth += 1;
                }
                Ok(Event::End(_)) => depth -= 1,
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("invalid XML: {e}"),
            }
        }
        assert_eq!(depth, 0, "unbalanced document");
        (count, root)
    }

    fn url(i: usize) -> SitemapUrl {
        SitemapUrl {
            loc: format!("https://acme.example/p/{i}?a=1&b=2"),
            lastmod: Some(
                Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap()
                    + chrono::Duration::days(i as i64),
            ),
            changefreq: Some(ChangeFreq::Weekly),
            priority: Some(0.8),
            alternates: vec![
                Alternate::new("tr", format!("https://acme.example/p/{i}")),
                Alternate::new("en", format!("https://acme.example/en/p/{i}")),
                Alternate::new("bad code", "https://acme.example/x"),
            ],
            images: vec![SitemapImage {
                loc: format!("https://acme.example/img/{i}.png"),
            }],
        }
    }

    #[test]
    fn single_file_is_valid_xml() {
        let mut w = SitemapWriter::new(Vec::new()).unwrap();
        w.push(&url(1)).unwrap();
        w.push(&url(2)).unwrap();
        assert!(matches!(
            w.push(&SitemapUrl::new("javascript:alert(1)")),
            Err(SeoError::InvalidUrl(_))
        ));
        assert_eq!(w.len(), 2);
        let xml = String::from_utf8(w.finish().unwrap()).unwrap();
        let (count, root) = parse(&xml);
        assert_eq!((count, root.as_str()), (2, "urlset"));
        assert!(xml.contains("<loc>https://acme.example/p/1?a=1&amp;b=2</loc>"));
        assert!(xml.contains("<lastmod>2025-01-02T00:00:00Z</lastmod>"));
        assert!(xml.contains(
            r#"<xhtml:link rel="alternate" hreflang="en" href="https://acme.example/en/p/1"/>"#
        ));
        assert!(xml.contains(
            "<image:image><image:loc>https://acme.example/img/1.png</image:loc></image:image>"
        ));
        assert!(xml.contains("<priority>0.8</priority>"));
        assert!(!xml.contains("bad code"));
    }

    #[test]
    fn splits_by_url_count_and_builds_index() {
        let limits = Limits {
            max_urls: 3,
            max_bytes: MAX_BYTES,
        };
        let mut urls: Vec<SitemapUrl> = (0..8).map(url).collect();
        urls.push(SitemapUrl::new("/relative"));
        let set = build_sitemaps(urls, limits);
        assert_eq!(set.files.len(), 3);
        assert_eq!(
            set.files.iter().map(|f| f.url_count).collect::<Vec<_>>(),
            vec![3, 3, 2]
        );
        assert_eq!(set.rejected, vec!["/relative".to_owned()]);
        for f in &set.files {
            assert_eq!(parse(&f.xml).0, f.url_count);
        }
        assert!(set.needs_index());
        assert_eq!(
            set.files[2].lastmod,
            Some(Utc.with_ymd_and_hms(2025, 1, 8, 0, 0, 0).unwrap())
        );
        let index = set
            .index_xml(|n| format!("https://acme.example/sitemaps/sitemap-{n}.xml"))
            .unwrap();
        let (count, root) = parse(&index);
        assert_eq!((count, root.as_str()), (3, "sitemapindex"));
        assert!(index.contains("<loc>https://acme.example/sitemaps/sitemap-3.xml</loc><lastmod>2025-01-08T00:00:00Z</lastmod>"));
    }

    #[test]
    fn splits_by_byte_size() {
        let entry_len = render_url(&url(1)).unwrap().len();
        let limits = Limits {
            max_urls: MAX_URLS,
            max_bytes: urlset_header().len() + URLSET_FOOTER.len() + entry_len * 2 + 5,
        };
        let set = build_sitemaps((1..=5).map(url), limits);
        assert_eq!(set.files.len(), 3);
        for f in &set.files {
            assert!(f.xml.len() <= limits.max_bytes);
            parse(&f.xml);
        }
    }

    #[test]
    fn default_limits_hold_fifty_thousand() {
        let set = build_sitemaps(
            (0..50_001).map(|i| SitemapUrl::new(format!("https://a.example/{i}"))),
            Limits::default(),
        );
        assert_eq!(set.files.len(), 2);
        assert_eq!(set.files[0].url_count, MAX_URLS);
        assert_eq!(set.files[1].url_count, 1);
    }

    #[test]
    fn empty_set_still_yields_valid_file() {
        let set = build_sitemaps(Vec::new(), Limits::default());
        assert_eq!(set.files.len(), 1);
        assert_eq!(parse(&set.files[0].xml), (0, "urlset".into()));
        assert!(!set.needs_index());
    }
}
