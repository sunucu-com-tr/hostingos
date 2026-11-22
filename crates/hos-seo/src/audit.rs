//! Classic on-page SEO auditor.
//!
//! [`audit_page`] inspects one rendered HTML page; [`audit_site`] audits many
//! pages and adds site-wide checks (duplicate titles / descriptions, hreflang
//! reciprocity, canonicals pointing to `noindex` pages) plus a summary.
//!
//! Findings carry a [`Severity`], an i18n `message_key` / `hint_key` and
//! `params` (default texts in [`crate::i18n`]). Page score = 100 minus
//! deductions (critical 25, error 10, warning 4, notice 1), floored at 0.

use std::collections::{BTreeMap, HashMap, HashSet};

use hos_core::domain::Locale;
use serde::{Deserialize, Serialize};

use crate::i18n;
use crate::jsonld::{self, IssueSeverity};
use crate::meta::{self, CanonicalPolicy};
use crate::page::{self, PageFacts};
use crate::text;

/// Finding severity (most severe first when sorted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Blocks indexing / ranking outright.
    Critical,
    /// Serious problem.
    Error,
    /// Should be fixed.
    Warning,
    /// Minor improvement.
    Notice,
}

impl Severity {
    /// Points deducted from the page score.
    pub const fn deduction(self) -> u32 {
        match self {
            Self::Critical => 25,
            Self::Error => 10,
            Self::Warning => 4,
            Self::Notice => 1,
        }
    }
}

/// One audit finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable id (`title_missing`, …).
    pub id: String,
    /// Severity.
    pub severity: Severity,
    /// i18n key of the message (`audit.{id}`).
    pub message_key: String,
    /// i18n key of the fix hint (`audit.{id}.hint`).
    pub hint_key: Option<String>,
    /// Placeholder values.
    pub params: BTreeMap<String, String>,
    /// Affected URLs (site-wide findings; empty for page findings).
    pub urls: Vec<String>,
}

impl Finding {
    fn new(id: &str, severity: Severity, params: &[(&str, String)]) -> Self {
        Self {
            id: id.to_owned(),
            severity,
            message_key: format!("audit.{id}"),
            hint_key: Some(format!("audit.{id}.hint")),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            urls: Vec::new(),
        }
    }

    /// Default message text.
    pub fn message(&self, locale: Locale) -> String {
        i18n::render(&self.message_key, locale, &self.params)
    }

    /// Default hint text.
    pub fn hint(&self, locale: Locale) -> Option<String> {
        self.hint_key
            .as_deref()
            .map(|k| i18n::render(k, locale, &self.params))
    }
}

/// A page to audit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditPage {
    /// Absolute URL the HTML was served from.
    pub url: String,
    /// Rendered HTML.
    pub html: String,
    /// Focus keywords (the first one is primary).
    pub keywords: Vec<String>,
}

/// Thresholds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditOptions {
    /// Default content language when `<html lang>` is missing.
    pub locale: Locale,
    /// Title length range (graphemes).
    pub title_min: usize,
    /// Maximum title length.
    pub title_max: usize,
    /// Description length range.
    pub description_min: usize,
    /// Maximum description length.
    pub description_max: usize,
    /// Below this word count content is "thin".
    pub min_words: usize,
    /// Canonical comparison policy.
    pub canonical: CanonicalPolicy,
}

impl Default for AuditOptions {
    fn default() -> Self {
        Self {
            locale: Locale::Tr,
            title_min: 15,
            title_max: meta::TITLE_MAX,
            description_min: 70,
            description_max: meta::DESCRIPTION_MAX,
            min_words: 300,
            canonical: CanonicalPolicy::default(),
        }
    }
}

/// Facts extracted for one page (for dashboards).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PageStats {
    /// Title.
    pub title: Option<String>,
    /// Meta description.
    pub description: Option<String>,
    /// First H1.
    pub h1: Option<String>,
    /// Number of H1s.
    pub h1_count: usize,
    /// Visible words in the content area.
    pub word_count: usize,
    /// Internal links.
    pub internal_links: usize,
    /// External links.
    pub external_links: usize,
    /// Images in the content area.
    pub images: usize,
    /// Images without an `alt` attribute.
    pub images_missing_alt: usize,
    /// `src` of images without `alt` (first 20).
    pub images_without_alt: Vec<String>,
    /// Canonical URL.
    pub canonical: Option<String>,
    /// `noindex` present.
    pub noindex: bool,
    /// hreflang alternates `(code, url)`.
    pub hreflangs: Vec<(String, String)>,
    /// JSON-LD types.
    pub json_ld_types: Vec<String>,
    /// Readability score (Ateşman for Turkish, Flesch Reading Ease for English).
    pub readability: Option<f64>,
    /// Primary keyword density in percent.
    pub keyword_density: Option<f64>,
}

/// Result of [`audit_page`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageReport {
    /// Audited URL.
    pub url: String,
    /// 0–100.
    pub score: u8,
    /// Findings, most severe first.
    pub findings: Vec<Finding>,
    /// Extracted facts.
    pub stats: PageStats,
}

/// Count of findings by severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    /// Critical.
    pub critical: usize,
    /// Error.
    pub error: usize,
    /// Warning.
    pub warning: usize,
    /// Notice.
    pub notice: usize,
}

impl SeverityCounts {
    fn add(&mut self, s: Severity) {
        match s {
            Severity::Critical => self.critical += 1,
            Severity::Error => self.error += 1,
            Severity::Warning => self.warning += 1,
            Severity::Notice => self.notice += 1,
        }
    }
}

/// How often an issue occurs across the site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueCount {
    /// Finding id.
    pub id: String,
    /// Severity.
    pub severity: Severity,
    /// Number of pages (or site findings) affected.
    pub count: usize,
}

/// Site-wide summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteSummary {
    /// Pages audited.
    pub page_count: usize,
    /// Mean page score.
    pub average_page_score: u8,
    /// Site score (mean page score minus site-wide deductions).
    pub score: u8,
    /// Findings by severity (page + site findings).
    pub counts: SeverityCounts,
    /// Most frequent issues (max 10), most severe / frequent first.
    pub top_issues: Vec<IssueCount>,
}

/// Result of [`audit_site`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteReport {
    /// Per-page reports.
    pub pages: Vec<PageReport>,
    /// Site-wide findings (with `urls`).
    pub site_findings: Vec<Finding>,
    /// Summary.
    pub summary: SiteSummary,
}

fn score_of(findings: &[Finding]) -> u8 {
    let total: u32 = findings.iter().map(|f| f.severity.deduction()).sum();
    u8::try_from(100u32.saturating_sub(total)).unwrap_or(0)
}

fn host_key(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
}

fn canonicalize(url: &str, policy: &CanonicalPolicy) -> String {
    meta::canonical_url(url, policy).unwrap_or_else(|_| url.trim().to_owned())
}

/// Audits one page.
pub fn audit_page(page: &AuditPage, opts: &AuditOptions) -> PageReport {
    let f = page::analyze(&page.html);
    let mut out: Vec<Finding> = Vec::new();
    let mut stats = PageStats::default();

    check_title(&f, opts, &mut out, &mut stats);
    check_description(&f, opts, &mut out, &mut stats);
    check_headings(&f, &mut out, &mut stats);
    check_images(&f, &mut out, &mut stats);
    check_canonical_and_robots(page, &f, opts, &mut out, &mut stats);
    check_hreflang(page, &f, opts, &mut out, &mut stats);
    check_links(page, &f, &mut out, &mut stats);
    check_content(&f, opts, &mut out, &mut stats);
    check_json_ld(&f, &mut out, &mut stats);
    check_social(&f, &mut out);
    check_keywords(page, &f, &mut out, &mut stats);
    check_readability(&f, opts, &mut out, &mut stats);
    if f.lang.is_none() {
        out.push(Finding::new("lang_missing", Severity::Notice, &[]));
    }
    if !f.has_viewport {
        out.push(Finding::new("viewport_missing", Severity::Warning, &[]));
    }

    out.sort_by(|a, b| a.severity.cmp(&b.severity).then_with(|| a.id.cmp(&b.id)));
    PageReport {
        url: page.url.clone(),
        score: score_of(&out),
        findings: out,
        stats,
    }
}

fn check_title(f: &PageFacts, opts: &AuditOptions, out: &mut Vec<Finding>, stats: &mut PageStats) {
    let titles: Vec<&String> = f.titles.iter().filter(|t| !t.is_empty()).collect();
    match titles.first() {
        None => out.push(Finding::new("title_missing", Severity::Critical, &[])),
        Some(t) => {
            stats.title = Some((*t).clone());
            let len = text::grapheme_len(t);
            if len < opts.title_min {
                out.push(Finding::new(
                    "title_too_short",
                    Severity::Notice,
                    &[("length", len.to_string())],
                ));
            } else if len > opts.title_max {
                out.push(Finding::new(
                    "title_too_long",
                    Severity::Warning,
                    &[
                        ("length", len.to_string()),
                        ("max", opts.title_max.to_string()),
                    ],
                ));
            }
        }
    }
    if titles.len() > 1 {
        out.push(Finding::new(
            "title_multiple",
            Severity::Warning,
            &[("count", titles.len().to_string())],
        ));
    }
}

fn check_description(
    f: &PageFacts,
    opts: &AuditOptions,
    out: &mut Vec<Finding>,
    stats: &mut PageStats,
) {
    let descs: Vec<&String> = f.descriptions.iter().filter(|d| !d.is_empty()).collect();
    match descs.first() {
        None => out.push(Finding::new("description_missing", Severity::Error, &[])),
        Some(d) => {
            stats.description = Some((*d).clone());
            let len = text::grapheme_len(d);
            if len < opts.description_min {
                out.push(Finding::new(
                    "description_too_short",
                    Severity::Notice,
                    &[("length", len.to_string())],
                ));
            } else if len > opts.description_max {
                out.push(Finding::new(
                    "description_too_long",
                    Severity::Warning,
                    &[
                        ("length", len.to_string()),
                        ("max", opts.description_max.to_string()),
                    ],
                ));
            }
        }
    }
    if descs.len() > 1 {
        out.push(Finding::new(
            "description_multiple",
            Severity::Warning,
            &[("count", descs.len().to_string())],
        ));
    }
}

fn check_headings(f: &PageFacts, out: &mut Vec<Finding>, stats: &mut PageStats) {
    let h1s: Vec<&String> = f
        .headings
        .iter()
        .filter(|(l, _)| *l == 1)
        .map(|(_, t)| t)
        .collect();
    stats.h1_count = h1s.len();
    stats.h1 = h1s.iter().find(|t| !t.is_empty()).map(|t| (*t).clone());
    match h1s.len() {
        0 => out.push(Finding::new("h1_missing", Severity::Error, &[])),
        1 => {}
        n => out.push(Finding::new(
            "h1_multiple",
            Severity::Warning,
            &[("count", n.to_string())],
        )),
    }
    let mut prev: Option<u8> = None;
    for (level, _) in &f.headings {
        if let Some(p) = prev
            && *level > p + 1
        {
            out.push(Finding::new(
                "heading_skip",
                Severity::Notice,
                &[("from", p.to_string()), ("to", level.to_string())],
            ));
            break;
        }
        prev = Some(*level);
    }
}

fn check_images(f: &PageFacts, out: &mut Vec<Finding>, stats: &mut PageStats) {
    stats.images = f.images.len();
    stats.images_missing_alt = f.images.iter().filter(|i| i.alt.is_none()).count();
    stats.images_without_alt = f
        .images
        .iter()
        .filter(|i| i.alt.is_none())
        .map(|i| i.src.clone())
        .take(20)
        .collect();
    if stats.images_missing_alt > 0 {
        out.push(Finding::new(
            "img_alt_missing",
            Severity::Warning,
            &[("count", stats.images_missing_alt.to_string())],
        ));
    }
}

fn robots_flags(f: &PageFacts) -> (bool, bool, bool) {
    let mut noindex = false;
    let mut index = false;
    let mut nofollow = false;
    for r in &f.robots {
        for d in r.split(',').map(str::trim) {
            match d {
                "noindex" | "none" => noindex = true,
                "index" | "all" => index = true,
                _ => {}
            }
            if d == "nofollow" || d == "none" {
                nofollow = true;
            }
        }
    }
    (noindex, index, nofollow)
}

fn check_canonical_and_robots(
    page: &AuditPage,
    f: &PageFacts,
    opts: &AuditOptions,
    out: &mut Vec<Finding>,
    stats: &mut PageStats,
) {
    let (noindex, index, nofollow) = robots_flags(f);
    stats.noindex = noindex;
    let canon: Vec<&String> = f.canonicals.iter().filter(|c| !c.is_empty()).collect();
    let mut points_elsewhere = false;
    match canon.as_slice() {
        [] => {
            if !noindex {
                out.push(Finding::new("canonical_missing", Severity::Warning, &[]));
            }
        }
        [c, rest @ ..] => {
            if !rest.is_empty() {
                out.push(Finding::new(
                    "canonical_multiple",
                    Severity::Error,
                    &[("count", canon.len().to_string())],
                ));
            }
            stats.canonical = Some((*c).clone());
            if url::Url::parse(c).is_err() {
                if c.starts_with('/') {
                    out.push(Finding::new(
                        "canonical_relative",
                        Severity::Notice,
                        &[("url", (*c).clone())],
                    ));
                } else {
                    out.push(Finding::new(
                        "canonical_invalid",
                        Severity::Error,
                        &[("url", (*c).clone())],
                    ));
                }
            } else if meta::canonical_url(c, &opts.canonical).is_err() {
                out.push(Finding::new(
                    "canonical_invalid",
                    Severity::Error,
                    &[("url", (*c).clone())],
                ));
            }
            let resolved = meta::absolute_url(&page.url, c).unwrap_or_else(|_| (*c).clone());
            if !page.url.is_empty()
                && canonicalize(&resolved, &opts.canonical)
                    != canonicalize(&page.url, &opts.canonical)
            {
                points_elsewhere = true;
                out.push(Finding::new(
                    "canonical_other",
                    Severity::Notice,
                    &[("url", resolved)],
                ));
            }
        }
    }
    if noindex && index {
        out.push(Finding::new("robots_conflict", Severity::Error, &[]));
    }
    if noindex {
        out.push(Finding::new("noindex", Severity::Warning, &[]));
        if points_elsewhere {
            out.push(Finding::new(
                "noindex_canonical_conflict",
                Severity::Warning,
                &[],
            ));
        }
    }
    if nofollow {
        out.push(Finding::new("nofollow", Severity::Notice, &[]));
    }
}

fn check_hreflang(
    page: &AuditPage,
    f: &PageFacts,
    opts: &AuditOptions,
    out: &mut Vec<Finding>,
    stats: &mut PageStats,
) {
    stats.hreflangs = f
        .hreflangs
        .iter()
        .map(|(code, href)| {
            (
                code.clone(),
                meta::absolute_url(&page.url, href).unwrap_or_else(|_| href.clone()),
            )
        })
        .collect();
    if stats.hreflangs.is_empty() {
        return;
    }
    let invalid: Vec<&str> = stats
        .hreflangs
        .iter()
        .map(|(c, _)| c.as_str())
        .filter(|c| !meta::is_valid_hreflang(c))
        .collect();
    if !invalid.is_empty() {
        out.push(Finding::new(
            "hreflang_invalid",
            Severity::Error,
            &[("codes", invalid.join(", "))],
        ));
    }
    let me = canonicalize(&page.url, &opts.canonical);
    let self_listed = stats.hreflangs.iter().any(|(c, u)| {
        !c.eq_ignore_ascii_case("x-default") && canonicalize(u, &opts.canonical) == me
    });
    if !page.url.is_empty() && !self_listed {
        out.push(Finding::new("hreflang_no_self", Severity::Warning, &[]));
    }
    if !stats
        .hreflangs
        .iter()
        .any(|(c, _)| c.eq_ignore_ascii_case("x-default"))
    {
        out.push(Finding::new("hreflang_no_x_default", Severity::Notice, &[]));
    }
}

fn check_links(page: &AuditPage, f: &PageFacts, out: &mut Vec<Finding>, stats: &mut PageStats) {
    let own = host_key(&page.url);
    let mut empty = 0;
    let mut js = 0;
    for l in &f.links {
        let href = l.href.trim();
        let lower = href.to_ascii_lowercase();
        if lower.starts_with("javascript:") || href == "#" || href.is_empty() {
            js += 1;
            continue;
        }
        if lower.starts_with("mailto:") || lower.starts_with("tel:") || href.starts_with('#') {
            continue;
        }
        if l.text.is_empty() {
            empty += 1;
        }
        match url::Url::parse(href) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => {
                let h = u
                    .host_str()
                    .map(|h| h.trim_start_matches("www.").to_ascii_lowercase());
                if own.is_some() && h == own {
                    stats.internal_links += 1;
                } else {
                    stats.external_links += 1;
                }
            }
            Ok(_) => {}
            Err(_) => stats.internal_links += 1,
        }
    }
    if stats.internal_links == 0 {
        out.push(Finding::new("links_internal_none", Severity::Warning, &[]));
    }
    if empty > 0 {
        out.push(Finding::new(
            "link_empty_anchor",
            Severity::Notice,
            &[("count", empty.to_string())],
        ));
    }
    if js > 0 {
        out.push(Finding::new(
            "link_javascript",
            Severity::Notice,
            &[("count", js.to_string())],
        ));
    }
}

fn check_content(
    f: &PageFacts,
    opts: &AuditOptions,
    out: &mut Vec<Finding>,
    stats: &mut PageStats,
) {
    stats.word_count = f.word_count;
    if f.word_count < 100 {
        out.push(Finding::new(
            "thin_content_severe",
            Severity::Error,
            &[("words", f.word_count.to_string())],
        ));
    } else if f.word_count < opts.min_words {
        out.push(Finding::new(
            "thin_content",
            Severity::Warning,
            &[
                ("words", f.word_count.to_string()),
                ("min", opts.min_words.to_string()),
            ],
        ));
    }
}

fn check_json_ld(f: &PageFacts, out: &mut Vec<Finding>, stats: &mut PageStats) {
    let mut types = f.json_ld_types();
    types.sort();
    types.dedup();
    stats.json_ld_types = types;
    if f.json_ld.is_empty() {
        out.push(Finding::new("jsonld_missing", Severity::Notice, &[]));
        return;
    }
    let invalid = f.json_ld.iter().filter(|r| r.is_err()).count();
    if invalid > 0 {
        out.push(Finding::new(
            "jsonld_invalid",
            Severity::Error,
            &[("count", invalid.to_string())],
        ));
    }
    let issues: Vec<jsonld::Issue> = f
        .json_ld
        .iter()
        .flatten()
        .flat_map(jsonld::validate_document)
        .collect();
    let summarize = |sev: IssueSeverity| -> (usize, String) {
        let list: Vec<String> = issues
            .iter()
            .filter(|i| i.severity == sev)
            .map(|i| i.path.clone())
            .collect();
        let shown: Vec<String> = list.iter().take(5).cloned().collect();
        let more = if list.len() > 5 { ", …" } else { "" };
        (list.len(), format!("{}{more}", shown.join(", ")))
    };
    let (errors, details) = summarize(IssueSeverity::Error);
    if errors > 0 {
        out.push(Finding::new(
            "jsonld_errors",
            Severity::Error,
            &[("count", errors.to_string()), ("details", details)],
        ));
    }
    let (warnings, details) = summarize(IssueSeverity::Warning);
    if warnings > 0 {
        out.push(Finding::new(
            "jsonld_warnings",
            Severity::Notice,
            &[("count", warnings.to_string()), ("details", details)],
        ));
    }
}

fn check_social(f: &PageFacts, out: &mut Vec<Finding>) {
    let missing: Vec<&str> = ["og:title", "og:description", "og:image", "og:url"]
        .into_iter()
        .filter(|k| f.meta(k).is_none_or(str::is_empty))
        .collect();
    if !missing.is_empty() {
        out.push(Finding::new(
            "og_missing",
            Severity::Notice,
            &[("tags", missing.join(", "))],
        ));
    }
    if f.meta("twitter:card").is_none() {
        out.push(Finding::new("twitter_missing", Severity::Notice, &[]));
    }
}

fn check_keywords(page: &AuditPage, f: &PageFacts, out: &mut Vec<Finding>, stats: &mut PageStats) {
    let Some(kw) = page
        .keywords
        .iter()
        .map(|k| k.trim())
        .find(|k| !k.is_empty())
    else {
        return;
    };
    let p = [("keyword", kw.to_owned())];
    let title = f.titles.first().map(String::as_str).unwrap_or("");
    if !text::contains_phrase(title, kw) {
        out.push(Finding::new("keyword_title", Severity::Warning, &p));
    }
    let h1: String = f
        .headings
        .iter()
        .filter(|(l, _)| *l == 1)
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if !text::contains_phrase(&h1, kw) {
        out.push(Finding::new("keyword_h1", Severity::Warning, &p));
    }
    let intro = f.lead_paragraph.clone().unwrap_or_else(|| {
        text::words(&f.main_text)
            .take(100)
            .collect::<Vec<_>>()
            .join(" ")
    });
    if !text::contains_phrase(&intro, kw) {
        out.push(Finding::new("keyword_intro", Severity::Notice, &p));
    }
    let desc = f.descriptions.first().map(String::as_str).unwrap_or("");
    if !desc.is_empty() && !text::contains_phrase(desc, kw) {
        out.push(Finding::new("keyword_description", Severity::Notice, &p));
    }
    let path = url::Url::parse(&page.url)
        .map(|u| u.path().to_owned())
        .unwrap_or_default();
    let slug = text::slugify(kw);
    if !path.is_empty() && !slug.is_empty() && !text::slugify(&path).contains(&slug) {
        out.push(Finding::new("keyword_url", Severity::Notice, &p));
    }
    if f.word_count > 0 {
        let kw_words = text::word_count(kw).max(1);
        let hits = text::count_phrase(&f.main_text, kw);
        let density = (hits * kw_words) as f64 * 100.0 / f.word_count as f64;
        let rounded = (density * 10.0).round() / 10.0;
        stats.keyword_density = Some(rounded);
        if density > 3.0 && f.word_count >= 100 {
            out.push(Finding::new(
                "keyword_stuffing",
                Severity::Warning,
                &[("density", format!("{rounded:.1}"))],
            ));
        }
    }
}

/// Readability: Ateşman (Turkish) or Flesch Reading Ease (English); `None`
/// below 100 words.
pub fn readability(text_content: &str, locale: Locale) -> Option<f64> {
    let words: Vec<&str> = text::words(text_content)
        .filter(|w| w.chars().any(char::is_alphabetic))
        .collect();
    if words.len() < 100 {
        return None;
    }
    let sentences = text::sentences(text_content).len().max(1) as f64;
    let n = words.len() as f64;
    let score = match locale {
        Locale::Tr => {
            let syl: usize = words.iter().map(|w| text::syllables_tr(w)).sum();
            198.825 - 40.175 * (syl as f64 / n) - 2.610 * (n / sentences)
        }
        Locale::En => {
            let syl: usize = words.iter().map(|w| text::syllables_en(w)).sum();
            206.835 - 1.015 * (n / sentences) - 84.6 * (syl as f64 / n)
        }
    };
    Some((score.clamp(0.0, 100.0) * 10.0).round() / 10.0)
}

fn check_readability(
    f: &PageFacts,
    opts: &AuditOptions,
    out: &mut Vec<Finding>,
    stats: &mut PageStats,
) {
    let locale = f
        .lang
        .as_deref()
        .and_then(|l| l.split(['-', '_']).next())
        .and_then(|l| l.to_ascii_lowercase().parse::<Locale>().ok())
        .unwrap_or(opts.locale);
    let body = if f.paragraphs.is_empty() {
        f.main_text.clone()
    } else {
        f.paragraphs.join(" ")
    };
    let Some(score) = readability(&body, locale) else {
        return;
    };
    stats.readability = Some(score);
    let p = [("score", format!("{score:.0}"))];
    if score < 30.0 {
        out.push(Finding::new("readability_very_low", Severity::Warning, &p));
    } else if score < 50.0 {
        out.push(Finding::new("readability_low", Severity::Notice, &p));
    }
}

/// Audits many pages and adds site-wide checks.
pub fn audit_site(pages: &[AuditPage], opts: &AuditOptions) -> SiteReport {
    let reports: Vec<PageReport> = pages.iter().map(|p| audit_page(p, opts)).collect();
    let mut site: Vec<Finding> = Vec::new();

    // Duplicate titles / descriptions.
    let mut dup = |field: &str, id: &str, get: &dyn Fn(&PageStats) -> Option<&String>| {
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut shown: HashMap<String, String> = HashMap::new();
        for r in &reports {
            if let Some(v) = get(&r.stats).filter(|v| !v.trim().is_empty()) {
                let key = text::fold(&text::normalize_ws(v));
                shown.entry(key.clone()).or_insert_with(|| v.clone());
                groups.entry(key).or_default().push(r.url.clone());
            }
        }
        for (key, urls) in groups.into_iter().filter(|(_, u)| u.len() > 1) {
            let mut fnd = Finding::new(
                id,
                Severity::Warning,
                &[
                    ("count", urls.len().to_string()),
                    (field, shown.get(&key).cloned().unwrap_or_default()),
                ],
            );
            fnd.urls = urls;
            site.push(fnd);
        }
    };
    dup("title", "duplicate_title", &|s| s.title.as_ref());
    dup("description", "duplicate_description", &|s| {
        s.description.as_ref()
    });

    // hreflang reciprocity and canonical → noindex.
    let key = |u: &str| canonicalize(u, &opts.canonical);
    let by_url: HashMap<String, &PageReport> = reports.iter().map(|r| (key(&r.url), r)).collect();
    let mut reported: HashSet<(String, String)> = HashSet::new();
    for r in &reports {
        let me = key(&r.url);
        for (code, target) in &r.stats.hreflangs {
            if code.eq_ignore_ascii_case("x-default") {
                continue;
            }
            let t = key(target);
            if t == me {
                continue;
            }
            if let Some(other) = by_url.get(&t) {
                let back = other.stats.hreflangs.iter().any(|(_, u)| key(u) == me);
                if !back && reported.insert((me.clone(), t.clone())) {
                    let mut fnd = Finding::new(
                        "hreflang_not_reciprocal",
                        Severity::Error,
                        &[("from", r.url.clone()), ("to", other.url.clone())],
                    );
                    fnd.urls = vec![r.url.clone(), other.url.clone()];
                    site.push(fnd);
                }
            }
        }
        if let Some(c) = &r.stats.canonical {
            let resolved = meta::absolute_url(&r.url, c).unwrap_or_else(|_| c.clone());
            let t = key(&resolved);
            if t != me
                && let Some(target) = by_url.get(&t)
                && target.stats.noindex
            {
                let mut fnd = Finding::new(
                    "canonical_to_noindex",
                    Severity::Error,
                    &[("from", r.url.clone()), ("to", target.url.clone())],
                );
                fnd.urls = vec![r.url.clone(), target.url.clone()];
                site.push(fnd);
            }
        }
    }
    site.sort_by(|a, b| a.severity.cmp(&b.severity).then_with(|| a.id.cmp(&b.id)));

    // Summary.
    let mut counts = SeverityCounts::default();
    let mut freq: BTreeMap<(String, Severity), usize> = BTreeMap::new();
    for f in reports.iter().flat_map(|r| &r.findings).chain(&site) {
        counts.add(f.severity);
        *freq.entry((f.id.clone(), f.severity)).or_default() += 1;
    }
    let mut top: Vec<IssueCount> = freq
        .into_iter()
        .map(|((id, severity), count)| IssueCount {
            id,
            severity,
            count,
        })
        .collect();
    top.sort_by(|a, b| {
        (u32::try_from(b.count).unwrap_or(u32::MAX) * b.severity.deduction())
            .cmp(&(u32::try_from(a.count).unwrap_or(u32::MAX) * a.severity.deduction()))
            .then_with(|| a.id.cmp(&b.id))
    });
    top.truncate(10);
    let average = if reports.is_empty() {
        0
    } else {
        let sum: u32 = reports.iter().map(|r| u32::from(r.score)).sum();
        u8::try_from(sum / u32::try_from(reports.len()).unwrap_or(1)).unwrap_or(0)
    };
    let site_penalty: u32 = site
        .iter()
        .map(|f| match f.severity {
            Severity::Critical => 10,
            Severity::Error => 5,
            Severity::Warning => 2,
            Severity::Notice => 1,
        })
        .sum::<u32>()
        .min(30);
    SiteReport {
        summary: SiteSummary {
            page_count: reports.len(),
            average_page_score: average,
            score: u8::try_from(u32::from(average).saturating_sub(site_penalty)).unwrap_or(0),
            counts,
            top_issues: top,
        },
        pages: reports,
        site_findings: site,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"<!doctype html><html lang="tr"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>WordPress Hosting Paketleri | Acme Hosting</title>
<meta name="description" content="Acme Hosting WordPress hosting paketleri: ücretsiz SSL, günlük yedek, NVMe disk ve 7/24 Türkçe destek. Aylık 49,90 TL'den başlayan fiyatlar.">
<link rel="canonical" href="https://acme.example/wordpress-hosting">
<link rel="alternate" hreflang="tr" href="https://acme.example/wordpress-hosting">
<link rel="alternate" hreflang="en" href="https://acme.example/en/wordpress-hosting">
<link rel="alternate" hreflang="x-default" href="https://acme.example/wordpress-hosting">
<meta property="og:title" content="WordPress Hosting"><meta property="og:description" content="x">
<meta property="og:image" content="https://acme.example/og.png"><meta property="og:url" content="https://acme.example/wordpress-hosting">
<meta name="twitter:card" content="summary_large_image">
<script type="application/ld+json">{"@context":"https://schema.org","@type":"Product","name":"WordPress Hosting","image":"https://acme.example/p.png","description":"d","offers":{"@type":"Offer","price":"49.90","priceCurrency":"TRY","availability":"https://schema.org/InStock"}}</script>
</head><body><header><a href="/">Acme</a></header><main>
<h1>WordPress Hosting</h1>
<p>WordPress hosting paketlerimiz küçük bloglardan yoğun trafikli e-ticaret sitelerine kadar her ölçekte hızlı ve güvenli bir altyapı sunar. Tüm paketlerde ücretsiz SSL, günlük yedekleme ve LiteSpeed önbellek bulunur.</p>
<h2>Paketler</h2>
<p>Başlangıç paketi tek site için idealdir. Pro paket beş siteye kadar destek verir. İşletme paketi ise sınırsız site ve öncelikli destek içerir. Her paket kolayca yükseltilebilir ve verileriniz taşınırken hiçbir kesinti yaşanmaz.</p>
<img src="/a.png" alt="Kontrol paneli"><img src="/b.png" alt="">
<h3>Özellikler</h3>
<p>Sunucularımız İstanbul veri merkezinde çalışır. Ağ altyapımız yedekli bağlantılarla korunur. Disklerimiz NVMe teknolojisi kullanır. Yedekler farklı bir lokasyonda saklanır. Destek ekibimiz yılın her günü yanınızdadır. Kurulum sihirbazı ile WordPress tek tıkla kurulur. Eklenti ve tema güncellemeleri otomatik yapılır. Güvenlik duvarı zararlı istekleri engeller. Kötü amaçlı yazılım taraması her gün çalışır. Site hızınızı ölçmek için ücretsiz araçlar sunarız.</p>
<h2>Neden biz?</h2>
<p>Yıllardır binlerce müşteriye hizmet veriyoruz. Şeffaf fiyatlandırma ve gizli ücret olmaması en büyük farkımızdır. Taşıma işlemlerini ekibimiz ücretsiz yapar. Memnun kalmazsanız ilk otuz gün içinde iade alırsınız. Sorularınız için <a href="/iletisim">iletişim</a> sayfamızı ve <a href="https://wordpress.org/">WordPress.org</a> belgelerini inceleyebilirsiniz.</p>
<p>Paketlerimizi karşılaştırmak için <a href="/wordpress-hosting/karsilastir">karşılaştırma tablosuna</a> bakın.</p>
</main></body></html>"#;

    const BAD: &str = r#"<html><head><title>Ana</title><title>İkinci</title>
<meta name="robots" content="noindex, nofollow"><meta name="robots" content="index">
<link rel="canonical" href="https://acme.example/baska"><link rel="canonical" href="/x">
<link rel="alternate" hreflang="english" href="https://acme.example/en/">
<script type="application/ld+json">{oops}</script>
<script type="application/ld+json">{"@context":"https://schema.org","@type":"Product","name":"X"}</script>
</head><body><h1>Bir</h1><h1>İki</h1><h4>Dört</h4><img src="a.png">
<a href="javascript:void(0)">x</a><a href="https://other.example/"><img src="i.png" alt=""></a>
<p>Kısa metin.</p></body></html>"#;

    fn ids(r: &PageReport) -> Vec<&str> {
        r.findings.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn good_page_is_clean() {
        let page = AuditPage {
            url: "https://acme.example/wordpress-hosting".into(),
            html: GOOD.into(),
            keywords: vec!["WordPress hosting".into()],
        };
        let r = audit_page(
            &page,
            &AuditOptions {
                min_words: 150,
                ..AuditOptions::default()
            },
        );
        let blocking: Vec<&Finding> = r
            .findings
            .iter()
            .filter(|f| f.severity <= Severity::Warning)
            .collect();
        assert!(blocking.is_empty(), "{blocking:#?}");
        assert!(r.score >= 95, "score {} {:?}", r.score, ids(&r));
        assert_eq!(r.stats.h1_count, 1);
        assert_eq!(r.stats.images_missing_alt, 0);
        assert!(r.stats.internal_links >= 3);
        assert_eq!(r.stats.external_links, 1);
        assert!(r.stats.readability.is_some());
        assert!(r.stats.json_ld_types.contains(&"Product".to_owned()));
    }

    #[test]
    fn bad_page_has_many_findings() {
        let page = AuditPage {
            url: "https://acme.example/kampanya".into(),
            html: BAD.into(),
            keywords: vec!["vps sunucu".into()],
        };
        let r = audit_page(&page, &AuditOptions::default());
        let got = ids(&r);
        for id in [
            "title_multiple",
            "title_too_short",
            "description_missing",
            "h1_multiple",
            "heading_skip",
            "img_alt_missing",
            "canonical_multiple",
            "canonical_other",
            "robots_conflict",
            "noindex",
            "noindex_canonical_conflict",
            "nofollow",
            "hreflang_invalid",
            "hreflang_no_self",
            "link_javascript",
            "link_empty_anchor",
            "thin_content_severe",
            "jsonld_invalid",
            "jsonld_errors",
            "og_missing",
            "twitter_missing",
            "keyword_title",
            "keyword_h1",
            "keyword_url",
            "lang_missing",
            "viewport_missing",
        ] {
            assert!(got.contains(&id), "missing {id} in {got:?}");
        }
        assert!(r.score < 20);
        assert_eq!(r.findings[0].severity, Severity::Error);
        let msg = r
            .findings
            .iter()
            .find(|f| f.id == "keyword_title")
            .unwrap()
            .message(Locale::Tr);
        assert_eq!(msg, "Odak anahtar kelime \"vps sunucu\" başlıkta geçmiyor.");
    }

    #[test]
    fn site_wide_checks() {
        let page = |url: &str, title: &str, alts: &[(&str, &str)], extra: &str| {
            let links: String = alts
                .iter()
                .map(|(l, h)| format!(r#"<link rel="alternate" hreflang="{l}" href="{h}">"#))
                .collect();
            AuditPage {
                url: url.into(),
                html: format!(
                    r#"<html lang="tr"><head><title>{title}</title><meta name="description" content="Aynı açıklama metni burada yer alıyor ve yeterince uzun olmalı ki kısa sayılmasın.">{links}{extra}</head><body><h1>{title}</h1><p>Metin <a href="/">ana</a></p></body></html>"#
                ),
                keywords: vec![],
            }
        };
        let pages = vec![
            page(
                "https://a.example/tr/vps",
                "VPS Sunucu",
                &[
                    ("tr", "https://a.example/tr/vps"),
                    ("en", "https://a.example/en/vps"),
                ],
                "",
            ),
            page(
                "https://a.example/en/vps",
                "VPS Server",
                &[("en", "https://a.example/en/vps")],
                "",
            ),
            page(
                "https://a.example/eski",
                "VPS Sunucu",
                &[],
                r#"<link rel="canonical" href="https://a.example/gizli">"#,
            ),
            page(
                "https://a.example/gizli",
                "Gizli",
                &[],
                r#"<meta name="robots" content="noindex">"#,
            ),
        ];
        let r = audit_site(&pages, &AuditOptions::default());
        let site_ids: Vec<&str> = r.site_findings.iter().map(|f| f.id.as_str()).collect();
        assert!(
            site_ids.contains(&"hreflang_not_reciprocal"),
            "{site_ids:?}"
        );
        assert!(site_ids.contains(&"canonical_to_noindex"));
        assert!(site_ids.contains(&"duplicate_title"));
        assert!(site_ids.contains(&"duplicate_description"));
        let dup = r
            .site_findings
            .iter()
            .find(|f| f.id == "duplicate_title")
            .unwrap();
        assert_eq!(dup.urls.len(), 2);
        assert_eq!(dup.params["title"], "VPS Sunucu");
        let rec = r
            .site_findings
            .iter()
            .find(|f| f.id == "hreflang_not_reciprocal")
            .unwrap();
        assert_eq!(rec.params["from"], "https://a.example/tr/vps");
        assert_eq!(rec.params["to"], "https://a.example/en/vps");
        assert_eq!(r.summary.page_count, 4);
        assert!(r.summary.score <= r.summary.average_page_score);
        assert!(!r.summary.top_issues.is_empty());
        assert!(r.summary.counts.error >= 2);
    }

    #[test]
    fn readability_scores() {
        let easy_tr = "Bu bir test. Kısa cümle var. ".repeat(30);
        let s = readability(&easy_tr, Locale::Tr).unwrap();
        assert!(s > 70.0, "{s}");
        let hard_en = "Notwithstanding considerable organizational heterogeneity, infrastructural virtualization methodologies substantially facilitate operational scalability considerations. ".repeat(12);
        let s = readability(&hard_en, Locale::En).unwrap();
        assert!(s < 30.0, "{s}");
        assert!(readability("çok kısa", Locale::Tr).is_none());
    }
}
