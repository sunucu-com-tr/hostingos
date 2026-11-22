//! Generative Engine Optimization (GEO) analyser: how likely AI answer
//! engines (ChatGPT search, Claude, Perplexity, Google AI Overviews) are to
//! understand, trust and **quote** a page.
//!
//! [`analyze`] runs weighted heuristic checks over the rendered HTML and
//! returns a [`GeoReport`] with a 0–100 score. Every [`Check`] carries an i18n
//! `message_key` / `hint_key` (+ `params`) with default tr/en texts in
//! [`crate::i18n`].
//!
//! | id | weight | what |
//! |---|---|---|
//! | `direct_answer` | 15 | Opening paragraph that answers the topic in ≤ ~90 words |
//! | `question_headings` | 10 | H2–H4 phrased as questions |
//! | `faq` | 10 | Visible FAQ block (+ `FAQPage` schema whose questions are on the page) |
//! | `definitions` | 8 | Concise "X is a …" / "X, … bir …dir." sentences |
//! | `concrete_data` | 10 | Numbers, prices, units, tables |
//! | `entity_clarity` | 10 | Brand / product / location named clearly; Organization schema |
//! | `freshness` | 8 | Dated within the last year (meta, JSON-LD or a `<time>` in the content) |
//! | `authorship` | 8 | Accountable author / publisher + source / trust signals (E-E-A-T) |
//! | `structured_data` | 10 | Valid JSON-LD coverage |
//! | `chunkability` | 8 | Short paragraphs, frequent headings |
//! | `ssr` | 3 | Content present without JavaScript (HostingOS renders server-side) |

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::domain::Locale;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::i18n;
use crate::jsonld::{self, IssueSeverity};
use crate::page::{self, PageFacts};
use crate::text;

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Fully satisfied.
    Pass,
    /// Partially satisfied (half credit).
    Warn,
    /// Not satisfied.
    Fail,
    /// Not applicable (excluded from the score).
    Skipped,
}

impl CheckStatus {
    fn credit(self) -> Option<f64> {
        match self {
            Self::Pass => Some(1.0),
            Self::Warn => Some(0.5),
            Self::Fail => Some(0.0),
            Self::Skipped => None,
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail | Self::Skipped => "fail",
        }
    }
}

/// One GEO check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Stable id (`direct_answer`, …).
    pub id: String,
    /// Outcome.
    pub status: CheckStatus,
    /// Weight in the score.
    pub weight: u8,
    /// i18n key of the result message (`geo.{id}.{pass|warn|fail}`).
    pub message_key: String,
    /// i18n key of the improvement hint (`None` on pass).
    pub hint_key: Option<String>,
    /// Placeholder values for the texts.
    pub params: BTreeMap<String, String>,
}

impl Check {
    fn new(id: &str, weight: u8, status: CheckStatus, params: &[(&str, String)]) -> Self {
        Self {
            id: id.to_owned(),
            status,
            weight,
            message_key: format!("geo.{id}.{}", status.suffix()),
            hint_key: (status != CheckStatus::Pass).then(|| format!("geo.{id}.hint")),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
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

/// GEO analysis result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeoReport {
    /// 0–100.
    pub score: u8,
    /// Individual checks.
    pub checks: Vec<Check>,
}

impl GeoReport {
    /// Checks that did not pass, heaviest first (what to fix first).
    pub fn improvements(&self) -> Vec<&Check> {
        let mut v: Vec<&Check> = self
            .checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::Warn | CheckStatus::Fail))
            .collect();
        v.sort_by_key(|c| {
            (
                std::cmp::Reverse(
                    u16::from(c.weight) * if c.status == CheckStatus::Fail { 2 } else { 1 },
                ),
                c.id.clone(),
            )
        });
        v
    }
}

/// Input of [`analyze`].
#[derive(Debug, Clone)]
pub struct GeoInput<'a> {
    /// Rendered page HTML.
    pub html: &'a str,
    /// Content language.
    pub locale: Locale,
    /// Brand (tenant display name).
    pub brand: Option<&'a str>,
    /// Other entities that must be named (product name, city / country served).
    pub entities: &'a [String],
    /// "Now" for freshness (inject for deterministic tests).
    pub now: DateTime<Utc>,
    /// Whether the page is server-rendered (always true for HostingOS pages).
    pub server_rendered: bool,
}

impl<'a> GeoInput<'a> {
    /// Input with defaults (now, server-rendered, no brand / entities).
    pub fn new(html: &'a str, locale: Locale) -> Self {
        Self {
            html,
            locale,
            brand: None,
            entities: &[],
            now: Utc::now(),
            server_rendered: true,
        }
    }
}

fn re(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

/// Runs every check.
pub fn analyze(input: &GeoInput<'_>) -> GeoReport {
    let f = page::analyze(input.html);
    let checks = vec![
        check_direct_answer(&f),
        check_question_headings(&f),
        check_faq(&f),
        check_definitions(&f),
        check_concrete_data(&f),
        check_entities(&f, input),
        check_freshness(&f, input.now),
        check_authorship(&f),
        check_structured_data(&f),
        check_chunkability(&f),
        check_ssr(&f, input.server_rendered),
    ];
    let (mut got, mut max) = (0.0, 0.0);
    for c in &checks {
        if let Some(credit) = c.status.credit() {
            got += credit * f64::from(c.weight);
            max += f64::from(c.weight);
        }
    }
    let score = if max > 0.0 {
        (got / max * 100.0).round().clamp(0.0, 100.0) as u8
    } else {
        0
    };
    GeoReport { score, checks }
}

fn check_direct_answer(f: &PageFacts) -> Check {
    let Some(lead) = f.lead_paragraph.as_deref() else {
        return Check::new(
            "direct_answer",
            15,
            CheckStatus::Fail,
            &[("words", "0".into())],
        );
    };
    let words = text::word_count(lead);
    let first_sentence = text::sentences(lead)
        .first()
        .map(|s| text::word_count(s))
        .unwrap_or(words);
    let status = if (15..=90).contains(&words) && first_sentence <= 35 {
        CheckStatus::Pass
    } else if words >= 5 {
        CheckStatus::Warn
    } else {
        CheckStatus::Fail
    };
    Check::new("direct_answer", 15, status, &[("words", words.to_string())])
}

const QUESTION_WORDS: &[&str] = &[
    // tr
    "nedir", "nasil", "neden", "nicin", "ne", "hangi", "kac", "kim", "nerede", "ne zaman", "mi",
    "mu", "mudur", "midir", // en
    "what", "how", "why", "when", "which", "who", "where", "can", "does", "do", "is", "are",
    "should", "will",
];

pub(crate) fn is_question(heading: &str) -> bool {
    let h = heading.trim();
    if h.ends_with('?') || h.ends_with('？') {
        return true;
    }
    let folded = text::fold(h);
    let words: Vec<&str> = text::words(&folded).collect();
    let Some(first) = words.first() else {
        return false;
    };
    let en_start = ["what", "how", "why", "when", "which", "who", "where"];
    if en_start.contains(first) {
        return true;
    }
    words
        .iter()
        .any(|w| QUESTION_WORDS[..14].contains(w) && !matches!(*w, "ne" | "kim"))
}

fn check_question_headings(f: &PageFacts) -> Check {
    let q = f
        .headings
        .iter()
        .filter(|(lvl, t)| (2..=4).contains(lvl) && is_question(t))
        .count();
    let status = match q {
        0 => CheckStatus::Fail,
        1 => CheckStatus::Warn,
        _ => CheckStatus::Pass,
    };
    Check::new("question_headings", 10, status, &[("count", q.to_string())])
}

/// Questions of every `FAQPage` node (`mainEntity[].name`).
fn faq_schema_questions(f: &PageFacts) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            serde_json::Value::Object(m) => {
                if m.get("@type").and_then(|t| t.as_str()) == Some("FAQPage") {
                    let entities = m.get("mainEntity");
                    let list = match entities {
                        Some(serde_json::Value::Array(a)) => a.iter().collect::<Vec<_>>(),
                        Some(one) => vec![one],
                        None => Vec::new(),
                    };
                    out.extend(
                        list.into_iter()
                            .filter_map(|q| q.get("name").and_then(|n| n.as_str()))
                            .map(str::to_owned),
                    );
                }
                m.values().for_each(|x| walk(x, out));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for v in f.json_ld.iter().flatten() {
        walk(v, &mut out);
    }
    out
}

/// FAQ: full credit needs both a visible Q&A block and `FAQPage` markup
/// whose questions appear in the page text. Markup for questions a reader
/// cannot see is not rewarded (search engines treat it as spam and answer
/// engines quote what is on the page).
fn check_faq(f: &PageFacts) -> Check {
    let questions = faq_schema_questions(f);
    let schema = f.has_json_ld_type(&["FAQPage"]);
    let visible_questions = questions
        .iter()
        .filter(|q| text::contains_phrase(&f.main_text, q))
        .count();
    let schema_visible =
        schema && !questions.is_empty() && visible_questions * 2 >= questions.len();
    let heading = f.headings.iter().any(|(_, t)| {
        let t = text::fold(t);
        t.contains("sss")
            || t.contains("sikca sorulan")
            || t.contains("faq")
            || t.contains("frequently asked")
            || t.contains("sorular")
            || t.contains("questions")
    });
    let content = heading
        || f.details >= 2
        || f.headings
            .iter()
            .filter(|(lvl, t)| *lvl >= 2 && is_question(t))
            .count()
            >= 3;
    let status = match (schema_visible, content) {
        (true, true) => CheckStatus::Pass,
        (true, false) | (false, true) => CheckStatus::Warn,
        (false, false) if schema => CheckStatus::Warn,
        (false, false) => CheckStatus::Fail,
    };
    let mut c = Check::new(
        "faq",
        10,
        status,
        &[
            ("questions", questions.len().to_string()),
            ("visible", visible_questions.to_string()),
        ],
    );
    if schema && !schema_visible {
        c.message_key = "geo.faq.hidden".into();
    }
    c
}

static DEF_EN: OnceLock<Option<Regex>> = OnceLock::new();
static DEF_TR: OnceLock<Option<Regex>> = OnceLock::new();

pub(crate) fn is_definition(sentence: &str) -> bool {
    let words = text::word_count(sentence);
    if !(4..=40).contains(&words) {
        return false;
    }
    let en = re(
        &DEF_EN,
        r"(?i)^(?:an?\s+|the\s+)?[\p{L}\p{N}][\p{L}\p{N}\s\-'’().&/]{0,60}?\s+(?:is|are|refers to|means|stands for)\s+(?:an?\s|the\s|one of\s|a type of\s|the process\s)",
    );
    if en.is_some_and(|r| r.is_match(sentence)) {
        return true;
    }
    let tr = re(
        &DEF_TR,
        r"(?i)(?:\p{L}+(?:dır|dir|dur|dür|tır|tir|tur|tür)|anlamına gelir|olarak tanımlanır|demektir|denir|denilir)\s*[.!]?\s*$",
    );
    let lower = text::fold(sentence);
    tr.is_some_and(|r| r.is_match(sentence))
        && (lower.contains(" bir ") || sentence.contains(','))
        && sentence.find(',').is_none_or(|i| i < 80)
}

fn check_definitions(f: &PageFacts) -> Check {
    let mut count = 0;
    for p in f.paragraphs.iter().take(40) {
        count += text::sentences(p)
            .into_iter()
            .filter(|s| is_definition(s))
            .count();
    }
    let status = match count {
        0 => CheckStatus::Fail,
        1 => CheckStatus::Warn,
        _ => CheckStatus::Pass,
    };
    Check::new("definitions", 8, status, &[("count", count.to_string())])
}

static FACTS: OnceLock<Option<Regex>> = OnceLock::new();

pub(crate) fn count_concrete_facts(s: &str) -> usize {
    re(
        &FACTS,
        r"(?i)(?:[$€£₺%]\s?\d[\d.,]*|\d[\d.,]*\s?(?:%|‰|tl\b|try\b|usd\b|eur\b|gbp\b|\$|€|₺|£|gb\b|tb\b|mb\b|kb\b|gbit|mbit|gbps|mbps|ms\b|vcpu|cpu\b|core|çekirdek|ghz|mhz|gün|ay\b|aylık|yıl|yıllık|saat|dakika|saniye|days?\b|months?\b|years?\b|hours?\b|minutes?\b|seconds?\b|sites?\b|hesap|accounts?\b|domains?\b|alan adı|e-posta|emails?\b|users?\b|kullanıcı|x\b))",
    )
    .map_or(0, |r| r.find_iter(s).count())
}

fn check_concrete_data(f: &PageFacts) -> Check {
    let count = count_concrete_facts(&f.main_text);
    let status = if count >= 3 || (f.tables >= 1 && count >= 1) {
        CheckStatus::Pass
    } else if count >= 1 || f.tables >= 1 {
        CheckStatus::Warn
    } else {
        CheckStatus::Fail
    };
    Check::new(
        "concrete_data",
        10,
        status,
        &[
            ("count", count.to_string()),
            ("tables", f.tables.to_string()),
        ],
    )
}

fn check_entities(f: &PageFacts, input: &GeoInput<'_>) -> Check {
    let title = f.titles.first().map(String::as_str).unwrap_or("");
    let h1 = f
        .headings
        .iter()
        .filter(|(l, _)| *l == 1)
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let lead = f.lead_paragraph.as_deref().unwrap_or("");
    let org_schema = f.has_json_ld_type(&[
        "Organization",
        "OnlineBusiness",
        "Corporation",
        "LocalBusiness",
    ]) || f.json_ld_find("brand").is_some()
        || f.json_ld_find("publisher").is_some()
        || f.json_ld_find("provider").is_some();

    let mut got = 0;
    let mut max = 0;
    let mut missing: Vec<String> = Vec::new();
    if let Some(brand) = input.brand.map(str::trim).filter(|b| !b.is_empty()) {
        max += 2;
        if text::contains_phrase(title, brand) || text::contains_phrase(&h1, brand) {
            got += 1;
        } else {
            missing.push(format!("{brand} (title/H1)"));
        }
        if text::contains_phrase(lead, brand) || text::contains_phrase(&f.main_text, brand) {
            got += 1;
        } else {
            missing.push(format!("{brand} (text)"));
        }
    }
    for e in input
        .entities
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
    {
        max += 1;
        if text::contains_phrase(&f.main_text, e) || text::contains_phrase(title, e) {
            got += 1;
        } else {
            missing.push(e.to_owned());
        }
    }
    max += 1;
    if org_schema {
        got += 1;
    } else {
        missing.push("Organization JSON-LD".into());
    }
    let status = if got == max {
        CheckStatus::Pass
    } else if got > 0 {
        CheckStatus::Warn
    } else {
        CheckStatus::Fail
    };
    Check::new(
        "entity_clarity",
        10,
        status,
        &[("missing", missing.join(", "))],
    )
}

static DATE_ISO: OnceLock<Option<Regex>> = OnceLock::new();
static DATE_DMY: OnceLock<Option<Regex>> = OnceLock::new();

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.date_naive());
    }
    s.get(..10)
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
}

pub(crate) fn find_dates(f: &PageFacts) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    for key in [
        "article:modified_time",
        "og:updated_time",
        "article:published_time",
        "datemodified",
        "datepublished",
        "last-modified",
    ] {
        if let Some(d) = f.meta(key).and_then(parse_date) {
            dates.push(d);
        }
    }
    for key in ["dateModified", "datePublished"] {
        if let Some(d) = f
            .json_ld_find(key)
            .as_ref()
            .and_then(|v| v.as_str())
            .and_then(parse_date)
        {
            dates.push(d);
        }
    }
    dates.extend(f.time_datetimes.iter().filter_map(|d| parse_date(d)));
    if let Some(r) = re(&DATE_ISO, r"\b(20\d{2})-(\d{2})-(\d{2})\b") {
        for c in r.captures_iter(&f.main_text) {
            let (y, m, d) = (c[1].parse().ok(), c[2].parse().ok(), c[3].parse().ok());
            if let (Some(y), Some(m), Some(d)) = (y, m, d)
                && let Some(date) = NaiveDate::from_ymd_opt(y, m, d)
            {
                dates.push(date);
            }
        }
    }
    if let Some(r) = re(&DATE_DMY, r"\b(\d{1,2})[./](\d{1,2})[./](20\d{2})\b") {
        for c in r.captures_iter(&f.main_text) {
            let (d, m, y) = (c[1].parse().ok(), c[2].parse().ok(), c[3].parse().ok());
            if let (Some(y), Some(m), Some(d)) = (y, m, d)
                && let Some(date) = NaiveDate::from_ymd_opt(y, m, d)
            {
                dates.push(date);
            }
        }
    }
    dates
}

fn check_freshness(f: &PageFacts, now: DateTime<Utc>) -> Check {
    let today = now.date_naive();
    let latest = find_dates(f).into_iter().filter(|d| *d <= today).max();
    match latest {
        None => Check::new("freshness", 8, CheckStatus::Fail, &[]),
        Some(d) => {
            let days = (today - d).num_days();
            let status = if days <= 365 {
                CheckStatus::Pass
            } else {
                CheckStatus::Warn
            };
            Check::new(
                "freshness",
                8,
                status,
                &[
                    ("date", d.format("%Y-%m-%d").to_string()),
                    ("days", days.to_string()),
                ],
            )
        }
    }
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
}

/// A commercial page (product, category, service) has no personal author;
/// the accountable party is the business. It counts as the author signal
/// when the page's structured data names an organisation **with a way to
/// reach it** (e-mail, telephone, postal address or a contact point) — a
/// bare name does not, so a page cannot earn this with boilerplate alone.
fn accountable_publisher(f: &PageFacts) -> bool {
    fn walk(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Array(a) => a.iter().any(walk),
            serde_json::Value::Object(m) => {
                let org = matches!(
                    m.get("@type").and_then(|t| t.as_str()),
                    Some("Organization" | "OnlineBusiness" | "Corporation" | "LocalBusiness")
                );
                let reachable = ["email", "telephone", "address", "contactPoint"]
                    .iter()
                    .any(|k| m.get(*k).is_some_and(|x| !x.is_null()));
                (org && reachable) || m.values().any(walk)
            }
            _ => false,
        }
    }
    f.json_ld.iter().flatten().any(walk)
}

fn check_authorship(f: &PageFacts) -> Check {
    let author = f.meta("author").is_some_and(|a| !a.is_empty())
        || f.meta("article:author").is_some()
        || f.rel_author > 0
        || f.has_byline
        || f.json_ld_find("author").is_some_and(|a| {
            a.get("name").is_some() || a.as_array().is_some_and(|x| !x.is_empty()) || a.is_string()
        })
        || accountable_publisher(f);
    let own_host = f
        .canonicals
        .first()
        .and_then(|c| host_of(c))
        .or_else(|| f.meta("og:url").and_then(host_of));
    let external_citation = f.links.iter().any(|l| {
        host_of(&l.href).is_some_and(|h| own_host.as_deref() != Some(h.as_str()))
            && !l.rel.contains("sponsored")
    });
    let trust_pages = f.links.iter().any(|l| {
        let h = text::fold(&l.href);
        [
            "hakkimizda",
            "about",
            "iletisim",
            "contact",
            "kurumsal",
            "ekibimiz",
            "team",
        ]
        .iter()
        .any(|k| h.contains(k))
    });
    let source_heading = f.headings.iter().any(|(_, t)| {
        let t = text::fold(t);
        t.contains("kaynak") || t.contains("source") || t.contains("reference")
    });
    let source = external_citation || trust_pages || source_heading;
    let status = match (author, source) {
        (true, true) => CheckStatus::Pass,
        (true, false) | (false, true) => CheckStatus::Warn,
        (false, false) => CheckStatus::Fail,
    };
    Check::new("authorship", 8, status, &[])
}

const RICH_TYPES: &[&str] = &[
    "Organization",
    "OnlineBusiness",
    "Corporation",
    "LocalBusiness",
    "WebSite",
    "WebPage",
    "BreadcrumbList",
    "Product",
    "Offer",
    "Service",
    "FAQPage",
    "Article",
    "BlogPosting",
    "NewsArticle",
    "TechArticle",
    "HowTo",
    "SoftwareApplication",
    "ItemList",
    "AggregateRating",
];

fn check_structured_data(f: &PageFacts) -> Check {
    let invalid = f.json_ld.iter().filter(|r| r.is_err()).count();
    let mut types: Vec<String> = f
        .json_ld_types()
        .into_iter()
        .filter(|t| RICH_TYPES.contains(&t.as_str()))
        .collect();
    types.sort();
    types.dedup();
    let errors = invalid
        + f.json_ld
            .iter()
            .flatten()
            .flat_map(jsonld::validate_value)
            .filter(|i| i.severity == IssueSeverity::Error)
            .count();
    let status = if types.is_empty() {
        CheckStatus::Fail
    } else if types.len() >= 2 && errors == 0 {
        CheckStatus::Pass
    } else {
        CheckStatus::Warn
    };
    let shown = if types.is_empty() {
        "-".to_owned()
    } else {
        types.join(", ")
    };
    Check::new(
        "structured_data",
        10,
        status,
        &[("types", shown), ("errors", errors.to_string())],
    )
}

fn check_chunkability(f: &PageFacts) -> Check {
    let lens: Vec<usize> = f.paragraphs.iter().map(|p| text::word_count(p)).collect();
    let sections = f
        .headings
        .iter()
        .filter(|(l, _)| (2..=4).contains(l))
        .count();
    let per_section = f.word_count / (sections + 1);
    if lens.is_empty() {
        let status = if f.word_count < 50 {
            CheckStatus::Skipped
        } else {
            CheckStatus::Fail
        };
        return Check::new(
            "chunkability",
            8,
            status,
            &[
                ("avg", "0".into()),
                ("max", "0".into()),
                ("per_section", per_section.to_string()),
            ],
        );
    }
    let avg = lens.iter().sum::<usize>() / lens.len();
    let max = lens.iter().copied().max().unwrap_or(0);
    let status = if avg <= 80 && max <= 150 && per_section <= 350 {
        CheckStatus::Pass
    } else if avg > 120 || (f.word_count > 600 && sections == 0) {
        CheckStatus::Fail
    } else {
        CheckStatus::Warn
    };
    Check::new(
        "chunkability",
        8,
        status,
        &[
            ("avg", avg.to_string()),
            ("max", max.to_string()),
            ("per_section", per_section.to_string()),
        ],
    )
}

fn check_ssr(f: &PageFacts, server_rendered: bool) -> Check {
    let status = if !server_rendered {
        CheckStatus::Fail
    } else if f.word_count >= 50 {
        CheckStatus::Pass
    } else {
        CheckStatus::Warn
    };
    Check::new("ssr", 3, status, &[("words", f.word_count.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    pub(crate) const GOOD: &str = r#"<!doctype html><html lang="tr"><head>
<title>WordPress Hosting nedir? | Acme Hosting</title>
<meta name="description" content="WordPress hosting nedir, nasıl seçilir? Acme Hosting paketleri aylık 49,90 TL'den başlar.">
<link rel="canonical" href="https://acme.example/wordpress-hosting">
<meta name="author" content="Ayşe Yılmaz">
<meta property="article:modified_time" content="2026-10-01T09:00:00Z">
<script type="application/ld+json">{"@context":"https://schema.org","@graph":[
 {"@type":"Organization","name":"Acme Hosting","url":"https://acme.example/","logo":"https://acme.example/logo.png"},
 {"@type":"FAQPage","mainEntity":[{"@type":"Question","name":"WordPress hosting nedir?","acceptedAnswer":{"@type":"Answer","text":"WordPress için optimize edilmiş hosting."}}]},
 {"@type":"BreadcrumbList","itemListElement":[{"@type":"ListItem","position":1,"name":"Ana sayfa","item":"https://acme.example/"},{"@type":"ListItem","position":2,"name":"WordPress Hosting"}]}
]}</script>
</head><body><main>
<h1>WordPress Hosting nedir?</h1>
<p>WordPress hosting, WordPress siteleri için hız, güvenlik ve otomatik güncellemelerle optimize edilmiş bir web barındırma hizmetidir. Acme Hosting'in İstanbul veri merkezindeki paketleri aylık 49,90 TL'den başlar ve 20 GB NVMe disk içerir.</p>
<p class="byline">Yazar: Ayşe Yılmaz · Son güncelleme: <time datetime="2026-10-01">1 Ekim 2026</time></p>
<h2>WordPress hosting nasıl seçilir?</h2>
<p>Doğru paketi seçerken disk alanı, aylık trafik ve site sayısına bakın. Küçük bir blog için 10 GB disk ve 1 site yeterlidir.</p>
<h2>Hangi paket bana uygun?</h2>
<table><tr><th>Paket</th><th>Fiyat</th><th>Disk</th></tr><tr><td>Başlangıç</td><td>49,90 TL</td><td>20 GB</td></tr><tr><td>Pro</td><td>99 TL</td><td>50 GB</td></tr></table>
<p>Pro paket, birden fazla siteyi yöneten ajanslar için tasarlanmış bir pakettir. %99,9 çalışma süresi garantisi sunar.</p>
<h2>Sıkça Sorulan Sorular</h2>
<details><summary>SSL ücretsiz mi?</summary><p>Evet, tüm paketlerde Let's Encrypt SSL ücretsizdir.</p></details>
<details><summary>Taşıma yapıyor musunuz?</summary><p>Evet, ücretsiz site taşıma hizmeti sunuyoruz.</p></details>
<p>Kaynak: <a href="https://wordpress.org/about/requirements/">WordPress gereksinimleri</a> · <a href="/hakkimizda">Hakkımızda</a></p>
</main></body></html>"#;

    pub(crate) const BAD: &str = r#"<html><head><title>Ana Sayfa</title></head><body>
<div id="app"></div><div>Hoş geldiniz. Bizi tercih ettiğiniz için teşekkür ederiz, en iyi hizmeti sunmak için çalışıyoruz ve her zaman yanınızdayız çünkü müşteri memnuniyeti bizim için önemlidir ve kalite anlayışımızla sektörde fark yaratıyoruz.</div>
</body></html>"#;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap()
    }

    fn status(r: &GeoReport, id: &str) -> CheckStatus {
        r.checks.iter().find(|c| c.id == id).unwrap().status
    }

    #[test]
    fn good_page_scores_high() {
        let entities = vec!["İstanbul".to_owned()];
        let input = GeoInput {
            brand: Some("Acme Hosting"),
            entities: &entities,
            now: now(),
            ..GeoInput::new(GOOD, Locale::Tr)
        };
        let r = analyze(&input);
        for c in &r.checks {
            assert_eq!(
                c.status,
                CheckStatus::Pass,
                "{} → {:?} {:?}",
                c.id,
                c.status,
                c.params
            );
        }
        assert_eq!(r.score, 100);
        assert!(r.improvements().is_empty());
        let direct = r.checks.iter().find(|c| c.id == "direct_answer").unwrap();
        assert!(
            direct
                .message(Locale::En)
                .starts_with("The page opens with a concise paragraph")
        );
        assert!(direct.hint_key.is_none());
    }

    #[test]
    fn bad_page_scores_low_with_hints() {
        let input = GeoInput {
            brand: Some("Acme Hosting"),
            now: now(),
            ..GeoInput::new(BAD, Locale::Tr)
        };
        let r = analyze(&input);
        assert!(r.score < 25, "score {}", r.score);
        for id in [
            "direct_answer",
            "question_headings",
            "faq",
            "definitions",
            "concrete_data",
            "entity_clarity",
            "freshness",
            "authorship",
            "structured_data",
        ] {
            assert_eq!(status(&r, id), CheckStatus::Fail, "{id}");
        }
        let worst = r.improvements();
        assert_eq!(worst[0].id, "direct_answer");
        assert!(worst[0].hint(Locale::Tr).unwrap().contains("H1"));
    }

    #[test]
    fn stale_dates_warn_and_csr_fails() {
        let html = r#"<html><body><main><h1>X</h1><p>Updated 2020-01-05. Plain text here.</p></main></body></html>"#;
        let r = analyze(&GeoInput {
            now: now(),
            server_rendered: false,
            ..GeoInput::new(html, Locale::En)
        });
        assert_eq!(status(&r, "freshness"), CheckStatus::Warn);
        assert_eq!(status(&r, "ssr"), CheckStatus::Fail);
    }

    #[test]
    fn faq_markup_must_match_visible_questions() {
        let ld = r#"<script type="application/ld+json">{"@context":"https://schema.org","@type":"FAQPage","mainEntity":[
            {"@type":"Question","name":"Is SSL free?","acceptedAnswer":{"@type":"Answer","text":"Yes."}},
            {"@type":"Question","name":"Do you migrate sites?","acceptedAnswer":{"@type":"Answer","text":"Yes."}}]}</script>"#;
        // Markup only: questions nowhere on the page.
        let hidden = format!(
            "<html><head>{ld}</head><body><main><h1>Plans</h1><p>Our plans.</p></main></body></html>"
        );
        let r = analyze(&GeoInput {
            now: now(),
            ..GeoInput::new(&hidden, Locale::En)
        });
        let faq = r.checks.iter().find(|c| c.id == "faq").unwrap();
        assert_eq!(faq.status, CheckStatus::Warn);
        assert_eq!(faq.message_key, "geo.faq.hidden");
        assert!(faq.message(Locale::En).contains("0/2"));
        // The same markup with the questions visible in an accordion.
        let visible = format!(
            "<html><head>{ld}</head><body><main><h1>Plans</h1><h2>Questions</h2>\
             <details><summary><h3>Is SSL free?</h3></summary><p>Yes.</p></details>\
             <details><summary><h3>Do you migrate sites?</h3></summary><p>Yes.</p></details></main></body></html>"
        );
        let r = analyze(&GeoInput {
            now: now(),
            ..GeoInput::new(&visible, Locale::En)
        });
        assert_eq!(status(&r, "faq"), CheckStatus::Pass);
    }

    #[test]
    fn reachable_organisation_is_the_accountable_author() {
        let page = |org: &str| {
            format!(
                r#"<html><head><link rel="canonical" href="https://acme.example/vps"><script type="application/ld+json">{{"@context":"https://schema.org","@graph":[{org},{{"@type":"Product","name":"VPS M"}}]}}</script></head><body><main><h1>VPS M</h1><p>A plan.</p><p><a href="/about">About</a></p></main></body></html>"#
            )
        };
        let named_only = page(r#"{"@type":"Organization","name":"Acme"}"#);
        let r = analyze(&GeoInput {
            now: now(),
            ..GeoInput::new(&named_only, Locale::En)
        });
        assert_eq!(
            status(&r, "authorship"),
            CheckStatus::Warn,
            "a bare name is not accountability"
        );
        let reachable =
            page(r#"{"@type":"Organization","name":"Acme","email":"help@acme.example"}"#);
        let r = analyze(&GeoInput {
            now: now(),
            ..GeoInput::new(&reachable, Locale::En)
        });
        assert_eq!(status(&r, "authorship"), CheckStatus::Pass);
    }

    #[test]
    fn dates_outside_the_content_do_not_count() {
        let html = r#"<html><body><header><time datetime="2026-11-01">1 Nov</time></header><main><h1>X</h1><p>Text without a date.</p></main><footer><time datetime="2026-11-10">10 Nov</time></footer></body></html>"#;
        let r = analyze(&GeoInput {
            now: now(),
            ..GeoInput::new(html, Locale::En)
        });
        assert_eq!(status(&r, "freshness"), CheckStatus::Fail);
        let html = r#"<html><body><main><h1>X</h1><p>Updated <time datetime="2026-11-01">1 Nov</time>.</p></main></body></html>"#;
        let r = analyze(&GeoInput {
            now: now(),
            ..GeoInput::new(html, Locale::En)
        });
        assert_eq!(status(&r, "freshness"), CheckStatus::Pass);
    }

    #[test]
    fn heuristics() {
        assert!(is_question("What is a VPS?"));
        assert!(is_question("VPS nedir"));
        assert!(is_question("Hangi paketi seçmeliyim"));
        assert!(!is_question("Özellikler"));
        assert!(is_definition(
            "A VPS is a virtual machine sold as a service by a hosting provider."
        ));
        assert!(is_definition(
            "VPS, fiziksel bir sunucunun sanal olarak bölünmesiyle oluşturulan bir sunucudur."
        ));
        assert!(!is_definition("Bize hemen ulaşın."));
        assert_eq!(
            count_concrete_facts("Fiyat 49,90 TL, 20 GB disk, %99.9 uptime, $5/mo"),
            4
        );
    }
}
