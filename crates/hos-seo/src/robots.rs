//! `robots.txt` generation (RFC 9309) with fine-grained AI crawler control.
//!
//! AI crawlers are grouped by purpose ([`AiPurpose`]):
//!
//! * **Training** — collects content to train models (GPTBot, ClaudeBot,
//!   Google-Extended, Applebot-Extended, CCBot, …).
//! * **Search** — builds the index behind AI search / answer engines
//!   (OAI-SearchBot, Claude-SearchBot, PerplexityBot, …). Blocking these
//!   removes the site from AI answers and citations — the core of GEO.
//! * **UserFetch** — fetches a page live because a user asked the assistant
//!   about it (ChatGPT-User, Claude-User, Perplexity-User, …).
//!
//! The tenant policy ([`AiPolicy`]) allows or blocks each class, with per-bot
//! overrides (also for bots not in the catalogue). The default policy allows
//! search and user fetches and blocks training (opt-in).
//!
//! Panel / transactional paths ([`ALWAYS_DISALLOWED`]) are always disallowed
//! for every crawler. A crawler obeys only the most specific matching group,
//! so the allowed-AI group repeats those rules.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// What an AI crawler does with fetched content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiPurpose {
    /// Model training data collection.
    Training,
    /// Indexing for AI search / answer engines.
    Search,
    /// Live fetch on behalf of a user prompt.
    UserFetch,
}

impl AiPurpose {
    /// Machine name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Training => "training",
            Self::Search => "search",
            Self::UserFetch => "user_fetch",
        }
    }
}

/// A known AI crawler / robots.txt product token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AiBot {
    /// `User-agent` token to use in robots.txt.
    pub token: &'static str,
    /// Operator.
    pub operator: &'static str,
    /// Purpose class.
    pub purpose: AiPurpose,
    /// Robots-only control token (no crawler of its own, e.g. `Google-Extended`).
    pub control_token_only: bool,
    /// Superseded token kept for older crawlers.
    pub legacy: bool,
}

const fn bot(token: &'static str, operator: &'static str, purpose: AiPurpose) -> AiBot {
    AiBot {
        token,
        operator,
        purpose,
        control_token_only: false,
        legacy: false,
    }
}

const fn token_only(token: &'static str, operator: &'static str, purpose: AiPurpose) -> AiBot {
    AiBot {
        token,
        operator,
        purpose,
        control_token_only: true,
        legacy: false,
    }
}

const fn legacy(token: &'static str, operator: &'static str, purpose: AiPurpose) -> AiBot {
    AiBot {
        token,
        operator,
        purpose,
        control_token_only: false,
        legacy: true,
    }
}

use AiPurpose::{Search, Training, UserFetch};

/// Catalogue of AI crawlers (as published by their operators; review
/// periodically — new agents appear often).
pub const AI_BOTS: &[AiBot] = &[
    // OpenAI
    bot("GPTBot", "OpenAI", Training),
    bot("OAI-SearchBot", "OpenAI", Search),
    bot("ChatGPT-User", "OpenAI", UserFetch),
    // Anthropic
    bot("ClaudeBot", "Anthropic", Training),
    bot("Claude-SearchBot", "Anthropic", Search),
    bot("Claude-User", "Anthropic", UserFetch),
    legacy("anthropic-ai", "Anthropic", Training),
    legacy("Claude-Web", "Anthropic", UserFetch),
    // Perplexity
    bot("PerplexityBot", "Perplexity", Search),
    bot("Perplexity-User", "Perplexity", UserFetch),
    // Google
    token_only("Google-Extended", "Google", Training),
    bot("Google-CloudVertexBot", "Google", Search),
    // Apple
    token_only("Applebot-Extended", "Apple", Training),
    // Amazon
    bot("Amazonbot", "Amazon", Training),
    bot("Amzn-SearchBot", "Amazon", Search),
    bot("Amzn-User", "Amazon", UserFetch),
    // Meta
    bot("meta-externalagent", "Meta", Training),
    bot("meta-externalfetcher", "Meta", UserFetch),
    bot("FacebookBot", "Meta", Training),
    // Others
    bot("CCBot", "Common Crawl", Training),
    bot("Bytespider", "ByteDance", Training),
    bot("MistralAI-User", "Mistral AI", UserFetch),
    bot("DuckAssistBot", "DuckDuckGo", Search),
    bot("YouBot", "You.com", Search),
    bot("cohere-ai", "Cohere", UserFetch),
    bot("cohere-training-data-crawler", "Cohere", Training),
    bot("AI2Bot", "Allen Institute for AI", Training),
    bot("Diffbot", "Diffbot", Training),
    bot("PanguBot", "Huawei", Training),
    bot("Timpibot", "Timpi", Training),
    bot("omgili", "Webz.io", Training),
    bot("ImagesiftBot", "ImageSift", Training),
];

/// Looks a bot up by token (case-insensitive).
pub fn find_bot(token: &str) -> Option<&'static AiBot> {
    AI_BOTS.iter().find(|b| b.token.eq_ignore_ascii_case(token))
}

/// Per-bot override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRule {
    /// Allow (panel paths stay blocked).
    Allow,
    /// Disallow the whole site.
    Disallow,
}

/// Tenant AI crawler policy (stored in tenant SEO settings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiPolicy {
    /// Allow AI search / answer-engine indexing (recommended: visibility in AI answers).
    pub allow_search: bool,
    /// Allow live fetches on behalf of users (recommended: citations in chats).
    pub allow_user_fetch: bool,
    /// Allow model-training crawlers (opt-in).
    pub allow_training: bool,
    /// Per-token overrides (keys are User-agent tokens, case-insensitive).
    /// Unknown tokens are emitted as additional groups.
    pub overrides: BTreeMap<String, BotRule>,
}

impl Default for AiPolicy {
    fn default() -> Self {
        Self {
            allow_search: true,
            allow_user_fetch: true,
            allow_training: false,
            overrides: BTreeMap::new(),
        }
    }
}

/// Where an [`EffectiveRule`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    /// The purpose-class switch.
    Class,
    /// A per-bot override.
    Override,
}

/// Resolved decision for one bot (for the admin UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveRule {
    /// Bot token.
    pub token: String,
    /// Catalogue entry, `None` for custom override tokens.
    pub bot: Option<AiBot>,
    /// Allowed?
    pub allowed: bool,
    /// Why.
    pub source: RuleSource,
}

impl AiPolicy {
    fn class_allows(&self, purpose: AiPurpose) -> bool {
        match purpose {
            AiPurpose::Training => self.allow_training,
            AiPurpose::Search => self.allow_search,
            AiPurpose::UserFetch => self.allow_user_fetch,
        }
    }

    fn override_for(&self, token: &str) -> Option<BotRule> {
        self.overrides
            .iter()
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(token))
            .map(|(_, r)| *r)
    }

    /// Whether `token` may crawl the public site under this policy. Tokens
    /// that are neither catalogued nor overridden are allowed (they fall
    /// under `User-agent: *`).
    pub fn allows(&self, token: &str) -> bool {
        match self.override_for(token) {
            Some(rule) => rule == BotRule::Allow,
            None => find_bot(token).is_none_or(|b| self.class_allows(b.purpose)),
        }
    }

    /// Decision for every catalogued bot plus every custom override.
    pub fn effective_rules(&self) -> Vec<EffectiveRule> {
        let mut rules: Vec<EffectiveRule> = AI_BOTS
            .iter()
            .map(|b| match self.override_for(b.token) {
                Some(rule) => EffectiveRule {
                    token: b.token.into(),
                    bot: Some(*b),
                    allowed: rule == BotRule::Allow,
                    source: RuleSource::Override,
                },
                None => EffectiveRule {
                    token: b.token.into(),
                    bot: Some(*b),
                    allowed: self.class_allows(b.purpose),
                    source: RuleSource::Class,
                },
            })
            .collect();
        for (token, rule) in &self.overrides {
            let Some(token) = sanitize_token(token) else {
                continue;
            };
            if find_bot(&token).is_none() {
                rules.push(EffectiveRule {
                    token,
                    bot: None,
                    allowed: *rule == BotRule::Allow,
                    source: RuleSource::Override,
                });
            }
        }
        rules
    }
}

/// Paths that are disallowed for every crawler regardless of policy
/// (client area, admin, platform, API, auth, cart / checkout, and the
/// display-currency and referral redirects, which only set a cookie).
pub const ALWAYS_DISALLOWED: &[&str] = &[
    "/client",
    "/admin",
    "/platform",
    "/api",
    "/auth",
    "/cart",
    "/checkout",
    "/currency/",
    "/r/",
];

/// robots.txt input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RobotsConfig {
    /// AI crawler policy.
    pub ai: AiPolicy,
    /// Absolute sitemap URLs (`Sitemap:` lines).
    pub sitemaps: Vec<String>,
    /// Extra tenant paths to disallow for everyone (`/promo-draft`, `/*?sort=`).
    pub disallow: Vec<String>,
    /// Extra explicit `Allow:` rules.
    pub allow: Vec<String>,
    /// Block every crawler from the whole site (staging / maintenance).
    pub block_all: bool,
}

/// Renders `robots.txt`.
pub fn render(cfg: &RobotsConfig) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("# robots.txt generated by HostingOS\n");
    if cfg.block_all {
        out.push_str("\nUser-agent: *\nDisallow: /\n");
        write_sitemaps(&mut out, &cfg.sitemaps);
        return out;
    }
    let _ = writeln!(
        out,
        "# AI crawlers: search={}, user-fetch={}, training={}",
        allow_word(cfg.ai.allow_search),
        allow_word(cfg.ai.allow_user_fetch),
        allow_word(cfg.ai.allow_training)
    );

    let mut base_rules = Vec::new();
    for p in ALWAYS_DISALLOWED {
        for rule in prefix_rules(p) {
            base_rules.push(format!("Disallow: {rule}"));
        }
    }
    for p in cfg.disallow.iter().filter_map(|p| sanitize_path(p)) {
        base_rules.push(format!("Disallow: {p}"));
    }
    for p in cfg.allow.iter().filter_map(|p| sanitize_path(p)) {
        base_rules.push(format!("Allow: {p}"));
    }
    base_rules.push("Allow: /".into());

    out.push_str("\nUser-agent: *\n");
    for r in &base_rules {
        out.push_str(r);
        out.push('\n');
    }

    let rules = cfg.ai.effective_rules();
    let mut allowed: Vec<&str> = Vec::new();
    let mut blocked: Vec<&str> = Vec::new();
    for r in &rules {
        if r.allowed {
            allowed.push(&r.token);
        } else {
            blocked.push(&r.token);
        }
    }
    if !allowed.is_empty() {
        out.push_str("\n# AI crawlers allowed by policy (panel paths stay blocked)\n");
        for t in &allowed {
            let _ = writeln!(out, "User-agent: {t}");
        }
        for r in &base_rules {
            out.push_str(r);
            out.push('\n');
        }
    }
    if !blocked.is_empty() {
        out.push_str("\n# AI crawlers blocked by policy\n");
        for t in &blocked {
            let _ = writeln!(out, "User-agent: {t}");
        }
        out.push_str("Disallow: /\n");
    }
    write_sitemaps(&mut out, &cfg.sitemaps);
    out
}

fn allow_word(b: bool) -> &'static str {
    if b { "allow" } else { "disallow" }
}

fn write_sitemaps(out: &mut String, sitemaps: &[String]) {
    let valid: Vec<&str> = sitemaps
        .iter()
        .filter_map(|s| crate::escape::safe_http_url(s))
        .filter(|s| !s.chars().any(|c| c.is_whitespace() || c.is_control()))
        .collect();
    if !valid.is_empty() {
        out.push('\n');
        for s in valid {
            let _ = writeln!(out, "Sitemap: {s}");
        }
    }
}

/// `/client` → `/client/`, `/client$`, `/client?` (blocks the section without
/// also blocking e.g. `/clients-say`).
fn prefix_rules(p: &str) -> [String; 3] {
    [format!("{p}/"), format!("{p}$"), format!("{p}?")]
}

/// A robots path pattern: must start with `/` or `*`; whitespace, control
/// characters and `#` are not allowed (prevents rule / line injection).
fn sanitize_path(p: &str) -> Option<String> {
    let p = p.trim();
    if p.is_empty()
        || p.len() > 512
        || !(p.starts_with('/') || p.starts_with('*'))
        || p.chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '#')
    {
        return None;
    }
    Some(p.to_owned())
}

/// A User-agent product token: letters, digits, `-`, `_`, `.` only.
fn sanitize_token(t: &str) -> Option<String> {
    let t = t.trim();
    (!t.is_empty()
        && t.len() <= 64
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    .then(|| t.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the rule lines of the group that lists `ua` (exact token).
    fn group_of<'a>(txt: &'a str, ua: &str) -> Vec<&'a str> {
        let mut groups: Vec<(Vec<&str>, Vec<&str>)> = Vec::new();
        let mut in_agents = false;
        for line in txt
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            if let Some(agent) = line.strip_prefix("User-agent: ") {
                if !in_agents {
                    groups.push((Vec::new(), Vec::new()));
                }
                in_agents = true;
                groups.last_mut().unwrap().0.push(agent);
            } else {
                in_agents = false;
                if line.starts_with("Sitemap:") {
                    continue;
                }
                if let Some(g) = groups.last_mut() {
                    g.1.push(line);
                }
            }
        }
        groups
            .into_iter()
            .find(|(agents, _)| agents.contains(&ua))
            .map(|(_, rules)| rules)
            .unwrap_or_default()
    }

    #[test]
    fn default_policy_blocks_training_only() {
        let cfg = RobotsConfig {
            sitemaps: vec!["https://acme.example/sitemap.xml".into()],
            ..RobotsConfig::default()
        };
        let txt = render(&cfg);
        assert_eq!(group_of(&txt, "GPTBot"), vec!["Disallow: /"]);
        assert_eq!(group_of(&txt, "ClaudeBot"), vec!["Disallow: /"]);
        assert_eq!(group_of(&txt, "Google-Extended"), vec!["Disallow: /"]);
        for ua in [
            "OAI-SearchBot",
            "ChatGPT-User",
            "Claude-SearchBot",
            "Claude-User",
            "PerplexityBot",
            "Perplexity-User",
        ] {
            let g = group_of(&txt, ua);
            assert!(g.contains(&"Allow: /"), "{ua}");
            assert!(g.contains(&"Disallow: /client/"), "{ua}");
            assert!(g.contains(&"Disallow: /admin$"), "{ua}");
        }
        let star = group_of(&txt, "*");
        for p in ALWAYS_DISALLOWED {
            assert!(star.contains(&format!("Disallow: {p}/").as_str()));
        }
        assert!(txt.contains("\nSitemap: https://acme.example/sitemap.xml\n"));
        assert!(txt.contains("# AI crawlers: search=allow, user-fetch=allow, training=disallow"));
    }

    #[test]
    fn everything_blocked_and_overrides() {
        let mut overrides = BTreeMap::new();
        overrides.insert("claudebot".to_owned(), BotRule::Allow);
        overrides.insert("SomeNewAIBot".to_owned(), BotRule::Disallow);
        overrides.insert(
            "Evil\nUser-agent: *\nDisallow: /".to_owned(),
            BotRule::Disallow,
        );
        let cfg = RobotsConfig {
            ai: AiPolicy {
                allow_search: false,
                allow_user_fetch: false,
                allow_training: false,
                overrides,
            },
            ..RobotsConfig::default()
        };
        let txt = render(&cfg);
        assert_eq!(group_of(&txt, "OAI-SearchBot"), vec!["Disallow: /"]);
        assert_eq!(group_of(&txt, "ChatGPT-User"), vec!["Disallow: /"]);
        assert_eq!(group_of(&txt, "SomeNewAIBot"), vec!["Disallow: /"]);
        assert!(group_of(&txt, "ClaudeBot").contains(&"Allow: /"));
        assert!(!txt.contains("Evil"));
        assert!(cfg.ai.allows("ClaudeBot"));
        assert!(!cfg.ai.allows("GPTBot"));
        assert!(cfg.ai.allows("Googlebot"));
    }

    #[test]
    fn training_allowed_puts_all_in_one_allowed_group() {
        let cfg = RobotsConfig {
            ai: AiPolicy {
                allow_training: true,
                ..AiPolicy::default()
            },
            disallow: vec![
                "/taslak".into(),
                "bad path".into(),
                "/x\nAllow: /admin".into(),
            ],
            ..RobotsConfig::default()
        };
        let txt = render(&cfg);
        assert!(!txt.contains("# AI crawlers blocked"));
        let g = group_of(&txt, "GPTBot");
        assert!(g.contains(&"Disallow: /taslak"));
        assert!(!txt.contains("bad path"));
        assert!(!txt.contains("Allow: /admin"));
    }

    #[test]
    fn block_all_mode() {
        let cfg = RobotsConfig {
            block_all: true,
            ..RobotsConfig::default()
        };
        assert_eq!(
            render(&cfg),
            "# robots.txt generated by HostingOS\n\nUser-agent: *\nDisallow: /\n"
        );
    }

    #[test]
    fn catalogue_is_consistent() {
        let mut seen = std::collections::HashSet::new();
        for b in AI_BOTS {
            assert!(
                seen.insert(b.token.to_ascii_lowercase()),
                "duplicate {}",
                b.token
            );
            assert!(sanitize_token(b.token).is_some());
        }
        for (token, purpose) in [
            ("GPTBot", AiPurpose::Training),
            ("OAI-SearchBot", AiPurpose::Search),
            ("ChatGPT-User", AiPurpose::UserFetch),
            ("ClaudeBot", AiPurpose::Training),
            ("Claude-User", AiPurpose::UserFetch),
            ("Claude-SearchBot", AiPurpose::Search),
            ("PerplexityBot", AiPurpose::Search),
            ("Perplexity-User", AiPurpose::UserFetch),
            ("Google-Extended", AiPurpose::Training),
            ("Applebot-Extended", AiPurpose::Training),
            ("CCBot", AiPurpose::Training),
            ("Bytespider", AiPurpose::Training),
            ("Amazonbot", AiPurpose::Training),
            ("meta-externalagent", AiPurpose::Training),
        ] {
            assert_eq!(find_bot(token).map(|b| b.purpose), Some(purpose), "{token}");
        }
    }
}
