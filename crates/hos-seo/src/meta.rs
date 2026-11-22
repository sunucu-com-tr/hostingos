//! Page `<head>` metadata: title / description / canonical / robots /
//! hreflang / Open Graph / Twitter / article tags.
//!
//! [`PageMeta::render_head`] returns an HTML fragment where every value is
//! escaped and every URL is checked to be absolute `http(s)` (anything else —
//! `javascript:`, relative paths — is silently dropped), so templates can
//! insert it with Askama's `|safe`.
//!
//! Helpers: [`TitleTemplate`] (`"{page} | {site}"`), grapheme-aware
//! [`truncate`], [`description_from_text`] and [`canonical_url`]
//! (tracking-parameter stripping, trailing-slash policy, lower-case host).

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::error::SeoError;
use crate::escape;
use crate::text;

/// Recommended maximum title length in graphemes (Google truncates ~600 px).
pub const TITLE_MAX: usize = 60;
/// Recommended maximum meta description length in graphemes.
pub const DESCRIPTION_MAX: usize = 160;

/// Everything that goes into a storefront page's `<head>` (besides JSON-LD,
/// see [`crate::jsonld`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PageMeta {
    /// Full `<title>` text (use [`TitleTemplate`] to build it).
    pub title: String,
    /// Meta description (≤ [`DESCRIPTION_MAX`] graphemes recommended).
    pub description: Option<String>,
    /// Absolute canonical URL (normalise with [`canonical_url`]).
    pub canonical: Option<String>,
    /// `<meta name="robots">` directives.
    pub robots: RobotsMeta,
    /// BCP 47 language of the page (`"tr"`, `"en-US"`); drives `og:locale`.
    pub locale: Option<String>,
    /// Language alternates (`<link rel="alternate" hreflang>`), including the
    /// page itself (Google requires self-reference).
    pub alternates: Vec<Alternate>,
    /// URL for `hreflang="x-default"` (usually the language selector / default locale).
    pub x_default: Option<String>,
    /// Open Graph.
    pub og: OpenGraph,
    /// Twitter / X card.
    pub twitter: TwitterCard,
    /// Article timestamps / authorship (blog posts, KB articles).
    pub article: Option<ArticleMeta>,
}

/// One `hreflang` alternate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alternate {
    /// Language (`"tr"`) or language-region (`"en-GB"`) code.
    pub hreflang: String,
    /// Absolute URL of the alternate.
    pub url: String,
}

impl Alternate {
    /// Creates an alternate.
    pub fn new(hreflang: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            hreflang: hreflang.into(),
            url: url.into(),
        }
    }
}

/// `max-image-preview` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagePreview {
    /// No image preview.
    None,
    /// Default size preview.
    Standard,
    /// Large preview (recommended for Discover / AI overviews).
    Large,
}

impl ImagePreview {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Standard => "standard",
            Self::Large => "large",
        }
    }
}

/// `<meta name="robots">` directives. The default is
/// `index, follow, max-snippet:-1, max-image-preview:large, max-video-preview:-1`
/// — maximally quotable for search and AI answer engines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RobotsMeta {
    /// `index` / `noindex`.
    pub index: bool,
    /// `follow` / `nofollow`.
    pub follow: bool,
    /// `max-snippet:N` (`-1` = unlimited, `0` = no snippet).
    pub max_snippet: Option<i32>,
    /// `max-image-preview:…`.
    pub max_image_preview: Option<ImagePreview>,
    /// `max-video-preview:N` (`-1` = unlimited).
    pub max_video_preview: Option<i32>,
    /// `noarchive`.
    pub noarchive: bool,
    /// `notranslate`.
    pub notranslate: bool,
    /// `noimageindex`.
    pub noimageindex: bool,
    /// `unavailable_after: <RFC 3339>`.
    pub unavailable_after: Option<DateTime<Utc>>,
}

impl Default for RobotsMeta {
    fn default() -> Self {
        Self {
            index: true,
            follow: true,
            max_snippet: Some(-1),
            max_image_preview: Some(ImagePreview::Large),
            max_video_preview: Some(-1),
            noarchive: false,
            notranslate: false,
            noimageindex: false,
            unavailable_after: None,
        }
    }
}

impl RobotsMeta {
    /// `noindex, follow` (thin / utility pages that should pass link equity).
    pub fn noindex() -> Self {
        Self {
            index: false,
            ..Self::default()
        }
    }

    /// The `content` attribute value.
    pub fn content(&self) -> String {
        let mut parts: Vec<String> = vec![
            if self.index { "index" } else { "noindex" }.into(),
            if self.follow { "follow" } else { "nofollow" }.into(),
        ];
        if self.index {
            if let Some(n) = self.max_snippet {
                parts.push(format!("max-snippet:{n}"));
            }
            if let Some(p) = self.max_image_preview {
                parts.push(format!("max-image-preview:{}", p.as_str()));
            }
            if let Some(n) = self.max_video_preview {
                parts.push(format!("max-video-preview:{n}"));
            }
            if self.noarchive {
                parts.push("noarchive".into());
            }
            if self.notranslate {
                parts.push("notranslate".into());
            }
            if self.noimageindex {
                parts.push("noimageindex".into());
            }
            if let Some(t) = self.unavailable_after {
                parts.push(format!(
                    "unavailable_after: {}",
                    t.to_rfc3339_opts(SecondsFormat::Secs, true)
                ));
            }
        }
        parts.join(", ")
    }
}

/// `og:type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OgType {
    /// `website` (home, listing and static pages).
    #[default]
    Website,
    /// `article` (blog posts, KB articles, announcements).
    Article,
    /// `product` (hosting packages).
    Product,
    /// `profile`.
    Profile,
}

impl OgType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Website => "website",
            Self::Article => "article",
            Self::Product => "product",
            Self::Profile => "profile",
        }
    }
}

/// Open Graph image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OgImage {
    /// Absolute image URL (1200×630 recommended).
    pub url: String,
    /// Width in pixels.
    pub width: Option<u32>,
    /// Height in pixels.
    pub height: Option<u32>,
    /// Alt text.
    pub alt: Option<String>,
    /// MIME type (`image/png`).
    pub mime: Option<String>,
}

/// Open Graph tags. `title`/`description`/`url` fall back to the page values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenGraph {
    /// `og:type`.
    pub kind: OgType,
    /// `og:title` override (e.g. without the ` | Site` suffix).
    pub title: Option<String>,
    /// `og:description` override.
    pub description: Option<String>,
    /// `og:url` override (defaults to the canonical URL).
    pub url: Option<String>,
    /// `og:site_name`.
    pub site_name: Option<String>,
    /// `og:image`.
    pub image: Option<OgImage>,
}

/// `twitter:card` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TwitterCardKind {
    /// `summary`.
    Summary,
    /// `summary_large_image`.
    SummaryLargeImage,
}

/// Twitter / X card. Title, description and image come from Open Graph
/// (X falls back to `og:*`), so only card-specific tags are emitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TwitterCard {
    /// Card type; `None` = `summary_large_image` when an OG image exists, else `summary`.
    pub card: Option<TwitterCardKind>,
    /// `twitter:site` (`@handle` of the company).
    pub site: Option<String>,
    /// `twitter:creator` (`@handle` of the author).
    pub creator: Option<String>,
}

/// `article:*` tags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArticleMeta {
    /// First publication.
    pub published_time: Option<DateTime<Utc>>,
    /// Last substantive update (freshness signal for search and AI engines).
    pub modified_time: Option<DateTime<Utc>>,
    /// Author profile URLs or names.
    pub authors: Vec<String>,
    /// Section / category.
    pub section: Option<String>,
    /// Tags.
    pub tags: Vec<String>,
}

impl PageMeta {
    /// Creates metadata with a title and default robots directives.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// Renders the `<head>` fragment (one tag per line, all values escaped).
    pub fn render_head(&self) -> String {
        let mut h = String::with_capacity(2048);
        let title = text::normalize_ws(&self.title);
        let _ = writeln!(h, "<title>{}</title>", escape::html(&title));
        let description = self
            .description
            .as_deref()
            .map(text::normalize_ws)
            .filter(|d| !d.is_empty());
        if let Some(d) = &description {
            meta_name(&mut h, "description", d);
        }
        let canonical = self.canonical.as_deref().and_then(escape::safe_http_url);
        if let Some(c) = canonical {
            let _ = writeln!(h, r#"<link rel="canonical" href="{}">"#, escape::attr(c));
        }
        meta_name(&mut h, "robots", &self.robots.content());

        for alt in &self.alternates {
            let lang = alt.hreflang.trim();
            if !is_valid_hreflang(lang) || lang.eq_ignore_ascii_case("x-default") {
                continue;
            }
            if let Some(url) = escape::safe_http_url(&alt.url) {
                let _ = writeln!(
                    h,
                    r#"<link rel="alternate" hreflang="{}" href="{}">"#,
                    escape::attr(lang),
                    escape::attr(url)
                );
            }
        }
        if let Some(url) = self.x_default.as_deref().and_then(escape::safe_http_url) {
            let _ = writeln!(
                h,
                r#"<link rel="alternate" hreflang="x-default" href="{}">"#,
                escape::attr(url)
            );
        }

        // Open Graph
        let og = &self.og;
        meta_prop(&mut h, "og:type", og.kind.as_str());
        let og_title = og
            .title
            .as_deref()
            .map(text::normalize_ws)
            .unwrap_or_else(|| title.clone());
        if !og_title.is_empty() {
            meta_prop(&mut h, "og:title", &og_title);
        }
        if let Some(d) = og
            .description
            .as_deref()
            .map(text::normalize_ws)
            .or_else(|| description.clone())
        {
            meta_prop(&mut h, "og:description", &d);
        }
        if let Some(url) = og
            .url
            .as_deref()
            .and_then(escape::safe_http_url)
            .or(canonical)
        {
            meta_prop(&mut h, "og:url", url);
        }
        if let Some(site) = og.site_name.as_deref().map(text::normalize_ws) {
            meta_prop(&mut h, "og:site_name", &site);
        }
        let own_locale = self.locale.as_deref().and_then(og_locale);
        if let Some(loc) = &own_locale {
            meta_prop(&mut h, "og:locale", loc);
            let mut seen = vec![loc.clone()];
            for alt in &self.alternates {
                if let Some(other) = og_locale(&alt.hreflang)
                    && !seen.contains(&other)
                {
                    meta_prop(&mut h, "og:locale:alternate", &other);
                    seen.push(other);
                }
            }
        }
        let image = og
            .image
            .as_ref()
            .and_then(|img| escape::safe_http_url(&img.url).map(|u| (img, u)));
        if let Some((img, url)) = image {
            meta_prop(&mut h, "og:image", url);
            if url.starts_with("https://") {
                meta_prop(&mut h, "og:image:secure_url", url);
            }
            if let Some(m) = img.mime.as_deref() {
                meta_prop(&mut h, "og:image:type", m);
            }
            if let Some(w) = img.width {
                meta_prop(&mut h, "og:image:width", &w.to_string());
            }
            if let Some(hh) = img.height {
                meta_prop(&mut h, "og:image:height", &hh.to_string());
            }
            if let Some(alt) = img.alt.as_deref() {
                meta_prop(&mut h, "og:image:alt", &text::normalize_ws(alt));
            }
        }

        // Twitter
        let card = self.twitter.card.unwrap_or(if image.is_some() {
            TwitterCardKind::SummaryLargeImage
        } else {
            TwitterCardKind::Summary
        });
        meta_name(
            &mut h,
            "twitter:card",
            match card {
                TwitterCardKind::Summary => "summary",
                TwitterCardKind::SummaryLargeImage => "summary_large_image",
            },
        );
        if let Some(site) = self.twitter.site.as_deref().map(twitter_handle) {
            meta_name(&mut h, "twitter:site", &site);
        }
        if let Some(c) = self.twitter.creator.as_deref().map(twitter_handle) {
            meta_name(&mut h, "twitter:creator", &c);
        }
        if let Some((img, _)) = image
            && let Some(alt) = img.alt.as_deref()
        {
            meta_name(&mut h, "twitter:image:alt", &text::normalize_ws(alt));
        }

        // Article
        if let Some(a) = &self.article {
            if let Some(t) = a.published_time {
                meta_prop(
                    &mut h,
                    "article:published_time",
                    &t.to_rfc3339_opts(SecondsFormat::Secs, true),
                );
            }
            if let Some(t) = a.modified_time {
                let ts = t.to_rfc3339_opts(SecondsFormat::Secs, true);
                meta_prop(&mut h, "article:modified_time", &ts);
                meta_prop(&mut h, "og:updated_time", &ts);
            }
            for author in &a.authors {
                meta_prop(&mut h, "article:author", &text::normalize_ws(author));
            }
            if let Some(s) = a.section.as_deref() {
                meta_prop(&mut h, "article:section", &text::normalize_ws(s));
            }
            for tag in &a.tags {
                meta_prop(&mut h, "article:tag", &text::normalize_ws(tag));
            }
        }
        h
    }
}

fn meta_name(out: &mut String, name: &str, content: &str) {
    let _ = writeln!(
        out,
        r#"<meta name="{}" content="{}">"#,
        escape::attr(name),
        escape::attr(content)
    );
}

fn meta_prop(out: &mut String, property: &str, content: &str) {
    let _ = writeln!(
        out,
        r#"<meta property="{}" content="{}">"#,
        escape::attr(property),
        escape::attr(content)
    );
}

fn twitter_handle(h: &str) -> String {
    let h = h.trim().trim_start_matches('@');
    format!("@{h}")
}

/// `"tr"` → `tr_TR`, `"en"` → `en_US`, `"en-GB"` → `en_GB`; `None` for
/// `x-default` or malformed input.
pub fn og_locale(bcp47: &str) -> Option<String> {
    let tag = bcp47.trim();
    if !is_valid_hreflang(tag) || tag.eq_ignore_ascii_case("x-default") {
        return None;
    }
    let mut parts = tag.split(['-', '_']);
    let lang = parts.next()?.to_ascii_lowercase();
    let region = parts
        .rfind(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()))
        .map(str::to_ascii_uppercase);
    let region = region.or_else(|| {
        match lang.as_str() {
            "tr" => Some("TR"),
            "en" => Some("US"),
            "de" => Some("DE"),
            "fr" => Some("FR"),
            "es" => Some("ES"),
            "it" => Some("IT"),
            "nl" => Some("NL"),
            "ru" => Some("RU"),
            "ar" => Some("SA"),
            "az" => Some("AZ"),
            _ => None,
        }
        .map(str::to_owned)
    })?;
    Some(format!("{lang}_{region}"))
}

/// Whether `code` is a syntactically valid `hreflang` value: ISO 639 language
/// (2–3 letters), optional script (4 letters) and optional region (2 letters
/// or 3 digits), or `x-default`.
pub fn is_valid_hreflang(code: &str) -> bool {
    if code.eq_ignore_ascii_case("x-default") {
        return true;
    }
    let mut parts = code.split('-');
    let Some(lang) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&lang.len()) || !lang.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    let is_script = |s: &str| s.len() == 4 && s.chars().all(|c| c.is_ascii_alphabetic());
    let is_region = |s: &str| {
        (s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()))
            || (s.len() == 3 && s.chars().all(|c| c.is_ascii_digit()))
    };
    match rest.as_slice() {
        [] => true,
        [a] => is_script(a) || is_region(a),
        [a, b] => is_script(a) && is_region(b),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Titles / descriptions
// ---------------------------------------------------------------------------

/// Grapheme-aware truncation to `max` user-perceived characters (including a
/// trailing `…` when shortened), preferring a word boundary.
pub fn truncate(text: &str, max: usize) -> String {
    text::truncate_graphemes(&text::normalize_ws(text), max)
}

/// A meta description from arbitrary text: whitespace-normalised and
/// truncated to `max` graphemes ([`DESCRIPTION_MAX`] is the usual value).
pub fn description_from_text(text: &str, max: usize) -> String {
    truncate(text, max)
}

/// Title pattern with `{page}` (alias `{sayfa}`) and `{site}` placeholders,
/// e.g. `"{page} | {site}"` or `"{site} — {page}"`.
///
/// When the result would exceed `max_len` graphemes the page part is
/// shortened first; if the fixed part leaves fewer than 15 graphemes for the
/// page, the site part is dropped instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleTemplate {
    /// Pattern.
    pub pattern: String,
    /// Maximum length in graphemes.
    pub max_len: usize,
}

impl Default for TitleTemplate {
    fn default() -> Self {
        Self {
            pattern: "{page} | {site}".into(),
            max_len: TITLE_MAX,
        }
    }
}

impl TitleTemplate {
    /// Creates a template with the default maximum length.
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            max_len: TITLE_MAX,
        }
    }

    /// Sets the maximum length.
    #[must_use]
    pub fn with_max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self
    }

    /// Renders the title for a page.
    pub fn render(&self, page: &str, site: &str) -> String {
        let page = text::normalize_ws(page);
        let site = text::normalize_ws(site);
        let pattern = self.pattern.replace("{sayfa}", "{page}");
        if !pattern.contains("{page}") {
            return text::truncate_graphemes(&pattern.replace("{site}", &site), self.max_len);
        }
        if site.is_empty() || page.is_empty() || text::fold(&page) == text::fold(&site) {
            let only = if site.is_empty() { &page } else { &site };
            return text::truncate_graphemes(only, self.max_len);
        }
        let full = pattern.replace("{site}", &site).replace("{page}", &page);
        if text::grapheme_len(&full) <= self.max_len {
            return full;
        }
        let fixed = text::grapheme_len(&pattern.replace("{site}", &site).replace("{page}", ""));
        let available = self.max_len.saturating_sub(fixed);
        if available >= 15 {
            let short = text::truncate_graphemes(&page, available);
            pattern.replace("{site}", &site).replace("{page}", &short)
        } else {
            text::truncate_graphemes(&page, self.max_len)
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical URLs
// ---------------------------------------------------------------------------

/// Trailing-slash policy for canonical paths (the root `/` is never changed,
/// nor are paths whose last segment looks like a file, e.g. `/sitemap.xml`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrailingSlash {
    /// `/hosting/` → `/hosting`.
    #[default]
    Strip,
    /// `/hosting` → `/hosting/`.
    Add,
    /// Leave as is.
    Preserve,
}

/// Options for [`canonical_url`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CanonicalPolicy {
    /// Trailing slash handling.
    pub trailing_slash: TrailingSlash,
    /// Rewrite `http://` to `https://`.
    pub force_https: bool,
    /// Sort the remaining query parameters by name (stable canonical form).
    pub sort_query: bool,
    /// Additional parameter names (case-insensitive) to strip.
    pub strip_params: Vec<String>,
    /// When set, only these parameters survive (e.g. `["page"]` for pagination).
    pub allowed_params: Option<Vec<String>>,
}

impl Default for CanonicalPolicy {
    fn default() -> Self {
        Self {
            trailing_slash: TrailingSlash::Strip,
            force_https: false,
            sort_query: true,
            strip_params: Vec::new(),
            allowed_params: None,
        }
    }
}

const TRACKING_PARAMS: &[&str] = &[
    "gclid",
    "gclsrc",
    "dclid",
    "gbraid",
    "wbraid",
    "fbclid",
    "msclkid",
    "yclid",
    "twclid",
    "ttclid",
    "li_fat_id",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
    "_hsenc",
    "_hsmi",
    "mkt_tok",
    "srsltid",
    "oly_anon_id",
    "oly_enc_id",
    "vero_id",
    "rb_clickid",
    "s_cid",
    "sc_cid",
    "epik",
    "irclickid",
    "wickedid",
    "ref_src",
    "spm",
];

/// Whether `name` is a known click / campaign tracking parameter
/// (`utm_*`, `hsa_*`, `gclid`, `fbclid`, `msclkid`, …).
pub fn is_tracking_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("utm_") || n.starts_with("hsa_") || TRACKING_PARAMS.contains(&n.as_str())
}

/// Normalises a URL into its canonical form: absolute `http(s)` only,
/// lower-case host without trailing dot, default port and fragment removed,
/// tracking parameters stripped, duplicate slashes collapsed, query sorted and
/// the trailing-slash policy applied.
///
/// ```
/// use hos_seo::meta::{canonical_url, CanonicalPolicy};
/// let c = canonical_url(
///     "HTTPS://Example.COM:443/Hosting//WordPress/?utm_source=x&b=2&a=1#top",
///     &CanonicalPolicy::default(),
/// ).unwrap();
/// assert_eq!(c, "https://example.com/Hosting/WordPress?a=1&b=2");
/// ```
pub fn canonical_url(input: &str, policy: &CanonicalPolicy) -> Result<String, SeoError> {
    let mut url = url::Url::parse(input.trim()).map_err(|_| SeoError::invalid_url(input))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(SeoError::invalid_url(input));
    }
    if policy.force_https && url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|()| SeoError::invalid_url(input))?;
        if url.port() == Some(80) {
            let _ = url.set_port(None);
        }
    }
    if let Some(host) = url.host_str().map(str::to_owned)
        && let Some(stripped) = host.strip_suffix('.')
    {
        url.set_host(Some(stripped))
            .map_err(|_| SeoError::invalid_url(input))?;
    }
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);

    // Query
    let extra: Vec<String> = policy
        .strip_params
        .iter()
        .map(|p| p.to_ascii_lowercase())
        .collect();
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !k.is_empty())
        .filter(|(k, _)| !is_tracking_param(k) && !extra.contains(&k.to_ascii_lowercase()))
        .filter(|(k, _)| {
            policy
                .allowed_params
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|a| a.eq_ignore_ascii_case(k)))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if policy.sort_query {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
    }
    if pairs.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }

    // Path
    let mut path = String::with_capacity(url.path().len());
    let mut prev_slash = false;
    for c in url.path().chars() {
        if c == '/' {
            if prev_slash {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        path.push(c);
    }
    if path.is_empty() {
        path.push('/');
    }
    if path != "/" {
        let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let looks_like_file = last.contains('.');
        match policy.trailing_slash {
            TrailingSlash::Strip => {
                while path.len() > 1 && path.ends_with('/') {
                    path.pop();
                }
            }
            TrailingSlash::Add if !looks_like_file && !path.ends_with('/') => path.push('/'),
            TrailingSlash::Add | TrailingSlash::Preserve => {}
        }
    }
    url.set_path(&path);
    Ok(url.to_string())
}

/// Resolves `path` (absolute path, relative path or full URL) against `base`.
pub fn absolute_url(base: &str, path: &str) -> Result<String, SeoError> {
    let base = url::Url::parse(base.trim()).map_err(|_| SeoError::invalid_url(base))?;
    let joined = base
        .join(path.trim())
        .map_err(|_| SeoError::invalid_url(path))?;
    if !matches!(joined.scheme(), "http" | "https") {
        return Err(SeoError::invalid_url(path));
    }
    Ok(joined.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> PageMeta {
        PageMeta {
            title: "WordPress Hosting | Acme".into(),
            description: Some("Hızlı ve güvenli WordPress hosting.".into()),
            canonical: Some("https://acme.example/wordpress-hosting".into()),
            locale: Some("tr".into()),
            alternates: vec![
                Alternate::new("tr", "https://acme.example/wordpress-hosting"),
                Alternate::new("en", "https://acme.example/en/wordpress-hosting"),
            ],
            x_default: Some("https://acme.example/wordpress-hosting".into()),
            og: OpenGraph {
                kind: OgType::Product,
                site_name: Some("Acme".into()),
                image: Some(OgImage {
                    url: "https://acme.example/og.png".into(),
                    width: Some(1200),
                    height: Some(630),
                    alt: Some("Acme logo".into()),
                    mime: Some("image/png".into()),
                }),
                ..OpenGraph::default()
            },
            twitter: TwitterCard {
                site: Some("acmehost".into()),
                ..TwitterCard::default()
            },
            article: Some(ArticleMeta {
                published_time: Some(Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 5).unwrap()),
                tags: vec!["wordpress".into()],
                ..ArticleMeta::default()
            }),
            ..PageMeta::default()
        }
    }

    #[test]
    fn renders_full_head() {
        let h = sample().render_head();
        assert!(h.contains("<title>WordPress Hosting | Acme</title>"));
        assert!(
            h.contains(r#"<link rel="canonical" href="https://acme.example/wordpress-hosting">"#)
        );
        assert!(h.contains(r#"<meta name="robots" content="index, follow, max-snippet:-1, max-image-preview:large, max-video-preview:-1">"#));
        assert!(h.contains(r#"hreflang="en" href="https://acme.example/en/wordpress-hosting""#));
        assert!(h.contains(r#"hreflang="x-default""#));
        assert!(h.contains(r#"<meta property="og:locale" content="tr_TR">"#));
        assert!(h.contains(r#"<meta property="og:locale:alternate" content="en_US">"#));
        assert!(h.contains(r#"<meta property="og:type" content="product">"#));
        assert!(h.contains(r#"<meta name="twitter:card" content="summary_large_image">"#));
        assert!(h.contains(r#"<meta name="twitter:site" content="@acmehost">"#));
        assert!(h.contains(
            r#"<meta property="article:published_time" content="2025-01-02T03:04:05Z">"#
        ));
        assert!(h.contains(
            r#"<meta property="og:url" content="https://acme.example/wordpress-hosting">"#
        ));
    }

    #[test]
    fn head_escapes_xss_vectors() {
        let meta = PageMeta {
            title: "</title><script>alert(1)</script>".into(),
            description: Some(r#"" onload="alert(1)"#.into()),
            canonical: Some("javascript:alert(1)".into()),
            og: OpenGraph {
                image: Some(OgImage {
                    url: "https://x.example/a.png\"><script>".into(),
                    ..OgImage::default()
                }),
                site_name: Some("<b>Acme</b>".into()),
                ..OpenGraph::default()
            },
            alternates: vec![Alternate::new("en\"><script>", "https://x.example/en")],
            ..PageMeta::default()
        };
        let h = meta.render_head();
        assert!(!h.contains("<script"));
        assert!(!h.contains("</title><"));
        assert!(h.contains("&lt;/title&gt;&lt;script&gt;"));
        assert!(h.contains(r#"content="&quot; onload=&quot;alert(1)""#));
        assert!(!h.contains("javascript:"));
        assert!(!h.contains("rel=\"canonical\""));
        assert!(h.contains("&lt;b&gt;Acme&lt;/b&gt;"));
    }

    #[test]
    fn noindex_drops_snippet_directives() {
        assert_eq!(RobotsMeta::noindex().content(), "noindex, follow");
    }

    #[test]
    fn title_templates() {
        let t = TitleTemplate::default();
        assert_eq!(t.render("VPS", "Acme"), "VPS | Acme");
        assert_eq!(t.render("", "Acme"), "Acme");
        assert_eq!(t.render("acme", "Acme"), "Acme");
        let long = "Kurumsal e-posta, SSL sertifikası ve sınırsız trafik içeren WordPress hosting paketleri";
        let out = t.render(long, "Acme Hosting");
        assert!(text::grapheme_len(&out) <= TITLE_MAX, "{out}");
        assert!(out.ends_with(" | Acme Hosting"));
        let tr = TitleTemplate::new("{site} — {sayfa}").with_max_len(30);
        assert_eq!(tr.render("Alan adı", "Acme"), "Acme — Alan adı");
        let tiny = TitleTemplate::new("{page} | {site}").with_max_len(20);
        let out = tiny.render("Çok uzun bir sayfa başlığı burada", "Çok Uzun Site Adı");
        assert!(text::grapheme_len(&out) <= 20);
        assert!(!out.contains('|'));
    }

    #[test]
    fn canonical_normalisation() {
        let p = CanonicalPolicy::default();
        assert_eq!(
            canonical_url("https://EXAMPLE.com./a/?gclid=1&fbclid=2&utm_medium=x", &p).unwrap(),
            "https://example.com/a"
        );
        assert_eq!(
            canonical_url("https://example.com", &p).unwrap(),
            "https://example.com/"
        );
        let add = CanonicalPolicy {
            trailing_slash: TrailingSlash::Add,
            force_https: true,
            ..CanonicalPolicy::default()
        };
        assert_eq!(
            canonical_url("http://example.com:80/blog/post", &add).unwrap(),
            "https://example.com/blog/post/"
        );
        assert_eq!(
            canonical_url("https://example.com/sitemap.xml", &add).unwrap(),
            "https://example.com/sitemap.xml"
        );
        let allow = CanonicalPolicy {
            allowed_params: Some(vec!["page".into()]),
            ..CanonicalPolicy::default()
        };
        assert_eq!(
            canonical_url("https://example.com/blog?sort=x&page=2", &allow).unwrap(),
            "https://example.com/blog?page=2"
        );
        assert!(canonical_url("ftp://example.com/", &p).is_err());
        assert!(canonical_url("/relative", &p).is_err());
        assert_eq!(
            absolute_url("https://example.com/a/b", "../c").unwrap(),
            "https://example.com/c"
        );
    }

    #[test]
    fn hreflang_validation() {
        for ok in ["tr", "en-GB", "zh-Hant-TW", "es-419", "x-default"] {
            assert!(is_valid_hreflang(ok), "{ok}");
        }
        for bad in ["english", "en_GB", "e", "en-GBR1", ""] {
            assert!(!is_valid_hreflang(bad), "{bad}");
        }
        assert_eq!(og_locale("en-GB").as_deref(), Some("en_GB"));
        assert_eq!(og_locale("x-default"), None);
    }
}
