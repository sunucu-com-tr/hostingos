//! Panel-compliant username generation.
//!
//! Every supported panel accepts lowercase ASCII letters and digits starting
//! with a letter; lengths and reserved names differ. Generation derives a
//! readable base from the domain (`"Şirket-Örnek.com.tr"` → `"sirketor"`) and,
//! when that is taken or reserved, tries numbered then random suffixes while
//! staying within the panel's limits.
//!
//! ```
//! use hos_panels::username::UsernameRules;
//! let rules = UsernameRules::CPANEL;
//! let taken = ["example"];
//! let name = rules.generate("example.com", |u| taken.contains(&u)).unwrap();
//! assert_eq!(name, "example1");
//! assert!(rules.validate(&name).is_ok());
//! ```

use hos_core::IntegrationError;
use hos_core::integrations::kinds;
use rand::RngExt as _;

/// Username constraints of one panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsernameRules {
    /// Panel kind these rules belong to.
    pub kind: &'static str,
    /// Minimum length.
    pub min_len: usize,
    /// Maximum length accepted by the panel.
    pub max_len: usize,
    /// Length generated names aim for (≤ `max_len`). cPanel prefixes database
    /// names with the first 8 characters, so generated names stay within 8.
    pub preferred_len: usize,
    /// Names the panel refuses (system users, mail aliases).
    pub reserved: &'static [&'static str],
    /// Forbidden prefixes (cPanel: `test`).
    pub forbidden_prefixes: &'static [&'static str],
    /// Forbidden suffixes (cPanel: `assword`).
    pub forbidden_suffixes: &'static [&'static str],
}

/// System accounts and mail aliases no panel lets a customer take.
const COMMON_RESERVED: &[&str] = &[
    "abuse",
    "adm",
    "admin",
    "administrator",
    "apache",
    "backup",
    "bin",
    "daemon",
    "dovecot",
    "ftp",
    "games",
    "hostmaster",
    "httpd",
    "info",
    "lp",
    "mail",
    "mailer",
    "mailman",
    "mailnull",
    "man",
    "marketing",
    "mysql",
    "named",
    "news",
    "nginx",
    "nobody",
    "noc",
    "operator",
    "postfix",
    "postgres",
    "postmaster",
    "proxy",
    "root",
    "sales",
    "security",
    "shutdown",
    "sshd",
    "support",
    "sync",
    "sys",
    "system",
    "toor",
    "uucp",
    "webmaster",
    "wheel",
    "www",
    "wwwdata",
];

/// cPanel: default `/etc/aliases` table from the "Reserved, invalid and
/// misconfigured usernames" document plus well-known cPanel service users.
const CPANEL_RESERVED: &[&str] = &[
    "abuse",
    "adm",
    "admin",
    "all",
    "apache",
    "bin",
    "cpanel",
    "cpanelcabcache",
    "cpanellogin",
    "cpaneleximfilter",
    "cpaneleximscanner",
    "cphulkd",
    "cpses",
    "daemon",
    "dovecot",
    "dovenull",
    "ftp",
    "ftpadm",
    "ftpadmin",
    "games",
    "halt",
    "hostmaster",
    "info",
    "lp",
    "mail",
    "mailer",
    "mailman",
    "mailnull",
    "marketing",
    "mysql",
    "named",
    "news",
    "newsadm",
    "newsadmin",
    "nobody",
    "operator",
    "postfix",
    "postgres",
    "postmaster",
    "root",
    "sales",
    "security",
    "shadow",
    "shutdown",
    "sshd",
    "sudo",
    "support",
    "sync",
    "system",
    "tomcat",
    "toor",
    "usenet",
    "virtfs",
    "webmail",
    "webmaster",
    "wheel",
    "whm",
    "www",
];

/// DirectAdmin system users in addition to [`COMMON_RESERVED`].
const DIRECTADMIN_RESERVED: &[&str] = &[
    "abuse",
    "adm",
    "admin",
    "apache",
    "bin",
    "daemon",
    "diradmin",
    "directadmin",
    "dovecot",
    "exim",
    "ftp",
    "hostmaster",
    "info",
    "mail",
    "majordomo",
    "mysql",
    "named",
    "nobody",
    "postmaster",
    "root",
    "security",
    "sshd",
    "support",
    "system",
    "webapps",
    "webmaster",
    "www",
];

/// Plesk service accounts in addition to [`COMMON_RESERVED`].
const PLESK_RESERVED: &[&str] = &[
    "admin",
    "apache",
    "drweb",
    "ftp",
    "mail",
    "mailman",
    "mysql",
    "named",
    "nginx",
    "nobody",
    "popuser",
    "postfix",
    "psaadm",
    "psacln",
    "psaftp",
    "psaserv",
    "root",
    "swkey",
    "webmaster",
    "www",
];

/// CyberPanel service accounts.
const CYBERPANEL_RESERVED: &[&str] = &[
    "admin",
    "cyberpanel",
    "docker",
    "ftp",
    "lscpd",
    "lsadm",
    "mail",
    "mysql",
    "nobody",
    "postfix",
    "root",
    "vmail",
    "www",
];

impl UsernameRules {
    /// cPanel/WHM: `[a-z][a-z0-9]{0,15}`, not starting with `test`, not ending
    /// with `assword`, not reserved.
    pub const CPANEL: Self = Self {
        kind: kinds::CPANEL,
        min_len: 1,
        max_len: 16,
        preferred_len: 8,
        reserved: CPANEL_RESERVED,
        forbidden_prefixes: &["test"],
        forbidden_suffixes: &["assword"],
    };

    /// DirectAdmin: `[a-z][a-z0-9]{2,15}` (`max_username_length` defaults to
    /// 16; some servers lower it to 10, so generated names stay within 8).
    pub const DIRECTADMIN: Self = Self {
        kind: kinds::DIRECTADMIN,
        min_len: 3,
        max_len: 16,
        preferred_len: 8,
        reserved: DIRECTADMIN_RESERVED,
        forbidden_prefixes: &[],
        forbidden_suffixes: &[],
    };

    /// Plesk: generated names double as customer login and subscription
    /// system (FTP) user, so the conservative system-user form is used.
    pub const PLESK: Self = Self {
        kind: kinds::PLESK,
        min_len: 3,
        max_len: 16,
        preferred_len: 10,
        reserved: PLESK_RESERVED,
        forbidden_prefixes: &[],
        forbidden_suffixes: &[],
    };

    /// CyberPanel: website owner (CyberPanel user) names are stored lowercase.
    pub const CYBERPANEL: Self = Self {
        kind: kinds::CYBERPANEL,
        min_len: 3,
        max_len: 32,
        preferred_len: 10,
        reserved: CYBERPANEL_RESERVED,
        forbidden_prefixes: &[],
        forbidden_suffixes: &[],
    };

    /// Rules for a panel kind (`mock` and unknown kinds use cPanel's, the
    /// strictest common denominator).
    pub fn for_kind(kind: &str) -> Self {
        match kind {
            kinds::DIRECTADMIN => Self::DIRECTADMIN,
            kinds::PLESK => Self::PLESK,
            kinds::CYBERPANEL => Self::CYBERPANEL,
            _ => Self::CPANEL,
        }
    }

    /// Checks a username; the error explains the violated rule.
    pub fn validate(&self, username: &str) -> Result<(), String> {
        let len = username.chars().count();
        if len < self.min_len || len > self.max_len {
            return Err(format!(
                "username must be {}-{} characters long",
                self.min_len, self.max_len
            ));
        }
        if !username
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return Err("username may contain only lowercase letters a-z and digits".into());
        }
        if !username.starts_with(|c: char| c.is_ascii_lowercase()) {
            return Err("username must start with a letter".into());
        }
        if let Some(p) = self
            .forbidden_prefixes
            .iter()
            .find(|p| username.starts_with(**p))
        {
            return Err(format!("username must not start with `{p}`"));
        }
        if let Some(s) = self
            .forbidden_suffixes
            .iter()
            .find(|s| username.ends_with(**s))
        {
            return Err(format!("username must not end with `{s}`"));
        }
        if self.reserved.contains(&username) || COMMON_RESERVED.contains(&username) {
            return Err(format!("username `{username}` is reserved"));
        }
        Ok(())
    }

    /// [`validate`](Self::validate) as an [`IntegrationError::InvalidInput`].
    pub fn check(&self, username: &str) -> Result<(), IntegrationError> {
        self.validate(username)
            .map_err(|e| IntegrationError::InvalidInput(format!("{}: {e}", self.kind)))
    }

    /// Readable base derived from a domain (not checked against reserved
    /// names; see [`candidates`](Self::candidates)).
    pub fn base_from_domain(&self, domain: &str) -> String {
        let domain = domain.trim().trim_end_matches('.').to_lowercase();
        let domain = domain.strip_prefix("www.").unwrap_or(&domain);
        let mut labels = domain.split('.').filter(|l| !l.is_empty()).map(clean_label);
        let mut base = labels.next().unwrap_or_default();
        // Too short ("a.com"): borrow from the following labels.
        for next in labels {
            if base.len() >= self.min_len.max(3) {
                break;
            }
            base.push_str(&next);
        }
        if base.is_empty() {
            base.push_str("user");
        }
        if base.starts_with(|c: char| c.is_ascii_digit())
            || self.forbidden_prefixes.iter().any(|p| base.starts_with(p))
        {
            base.insert(0, 'u');
        }
        while base.len() < self.min_len {
            base.push_str("web");
        }
        base.truncate(self.preferred_len.max(self.min_len).min(self.max_len));
        base
    }

    /// Candidate usernames in preference order: the base, numbered variants
    /// (`base1`…`base99`, truncating the base to fit), then random 4-character
    /// suffixes. Every candidate passes [`validate`](Self::validate).
    pub fn candidates(&self, domain: &str) -> Vec<String> {
        let base = self.base_from_domain(domain);
        let target = self.preferred_len.max(self.min_len).min(self.max_len);
        let mut out: Vec<String> = Vec::with_capacity(128);
        let push = |name: String, out: &mut Vec<String>| {
            if self.validate(&name).is_ok() && !out.contains(&name) {
                out.push(name);
            }
        };
        push(base.clone(), &mut out);
        for n in 1..=99u32 {
            let suffix = n.to_string();
            push(with_suffix(&base, &suffix, target), &mut out);
        }
        let mut rng = rand::rng();
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        for _ in 0..24 {
            let suffix: String = (0..4)
                .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
                .collect();
            push(with_suffix(&base, &suffix, target), &mut out);
        }
        out
    }

    /// First candidate for `domain` that `is_taken` rejects; `Conflict` when
    /// all are taken.
    pub fn generate(
        &self,
        domain: &str,
        mut is_taken: impl FnMut(&str) -> bool,
    ) -> Result<String, IntegrationError> {
        self.candidates(domain)
            .into_iter()
            .find(|c| !is_taken(c))
            .ok_or_else(|| {
                IntegrationError::Conflict(format!(
                    "{}: no free username could be derived from `{domain}`",
                    self.kind
                ))
            })
    }
}

/// `stem + suffix` limited to `target` characters, shortening the stem (never
/// below one character, so the name still starts with a letter).
fn with_suffix(base: &str, suffix: &str, target: usize) -> String {
    let keep = target.saturating_sub(suffix.len()).max(1).min(base.len());
    format!("{}{suffix}", &base[..keep])
}

/// Lowercase ASCII letters/digits of a domain label, transliterating common
/// Turkish and Latin-1 letters and stripping the IDNA `xn--` prefix.
fn clean_label(label: &str) -> String {
    let label = label.strip_prefix("xn--").unwrap_or(label);
    let mut out = String::with_capacity(label.len());
    for c in label.chars() {
        match c {
            'a'..='z' | '0'..='9' => out.push(c),
            'A'..='Z' => out.push(c.to_ascii_lowercase()),
            _ => out.push_str(transliterate(c)),
        }
    }
    out
}

fn transliterate(c: char) -> &'static str {
    match c {
        'ç' | 'Ç' | 'ć' | 'č' => "c",
        'ğ' | 'Ğ' => "g",
        'ı' | 'İ' | 'í' | 'ì' | 'î' | 'ï' | 'Î' => "i",
        'ö' | 'Ö' | 'ó' | 'ò' | 'ô' | 'õ' | 'ø' | 'Ô' => "o",
        'ş' | 'Ş' | 'ś' | 'š' => "s",
        'ü' | 'Ü' | 'ú' | 'ù' | 'û' | 'Û' => "u",
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Â' | 'Ä' => "a",
        'é' | 'è' | 'ê' | 'ë' => "e",
        'ñ' | 'ń' => "n",
        'ž' | 'ź' | 'ż' => "z",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng as _;
    use rand::rngs::StdRng;

    const ALL: [UsernameRules; 4] = [
        UsernameRules::CPANEL,
        UsernameRules::DIRECTADMIN,
        UsernameRules::PLESK,
        UsernameRules::CYBERPANEL,
    ];

    #[test]
    fn readable_bases() {
        let r = UsernameRules::CPANEL;
        assert_eq!(r.base_from_domain("example.com"), "example");
        assert_eq!(r.base_from_domain("WWW.My-Shop.com.tr."), "myshop");
        assert_eq!(r.base_from_domain("şirketörnek.com.tr"), "sirketor");
        assert_eq!(r.base_from_domain("123host.net"), "u123host");
        assert_eq!(r.base_from_domain("testsite.org"), "utestsit");
        assert_eq!(r.base_from_domain("a.io"), "aio");
        assert_eq!(r.base_from_domain("xn--mnchen-3ya.de"), "mnchen3y");
        assert_eq!(r.base_from_domain(""), "user");
        assert_eq!(UsernameRules::DIRECTADMIN.base_from_domain("a.b"), "abweb");
    }

    #[test]
    fn validation_rules() {
        let c = UsernameRules::CPANEL;
        assert!(c.validate("alice").is_ok());
        assert!(c.validate("Alice").is_err());
        assert!(c.validate("1alice").is_err());
        assert!(c.validate("testuser").is_err());
        assert!(c.validate("mypassword").is_err());
        assert!(c.validate("root").is_err());
        assert!(c.validate("abcdefghijklmnopq").is_err());
        assert!(c.validate("ali-ce").is_err());
        assert!(UsernameRules::DIRECTADMIN.validate("ab").is_err());
        assert!(UsernameRules::DIRECTADMIN.validate("diradmin").is_err());
        assert!(matches!(
            c.check("Bad!"),
            Err(IntegrationError::InvalidInput(_))
        ));
    }

    #[test]
    fn reserved_and_suffix_handling() {
        let c = UsernameRules::CPANEL;
        // "root.com" → base "root" is reserved → first numbered candidate.
        assert_eq!(c.generate("root.com", |_| false).unwrap(), "root1");
        // "password.net" → "password" ends with "assword" → numbered.
        assert_eq!(c.generate("password.net", |_| false).unwrap(), "passwor1");
        // Long base gets truncated to make room for the suffix.
        let taken = ["longdoma", "longdom1"];
        assert_eq!(
            c.generate("longdomainname.com", |u| taken.contains(&u))
                .unwrap(),
            "longdom2"
        );
        assert!(matches!(
            c.generate("example.com", |_| true),
            Err(IntegrationError::Conflict(_))
        ));
    }

    /// Property-style check over many random (including hostile) domains:
    /// every generated name is valid for its panel, within the preferred
    /// length, and avoids every taken name.
    #[test]
    fn generated_names_always_valid_and_unique() {
        let mut rng = StdRng::seed_from_u64(0x5eed_cafe);
        let pool: Vec<char> = "abcxyz019-._ŞşİıĞğÜüÖöÇç漢字🙂 TEST".chars().collect();
        for round in 0..400 {
            let len = rng.random_range(0..30);
            let mut domain: String = (0..len)
                .map(|_| pool[rng.random_range(0..pool.len())])
                .collect();
            if round % 3 == 0 {
                domain.push_str(".com");
            }
            for rules in ALL {
                let mut taken: Vec<String> = Vec::new();
                for _ in 0..12 {
                    let name = rules
                        .generate(&domain, |u| taken.iter().any(|t| t == u))
                        .unwrap();
                    assert!(
                        rules.validate(&name).is_ok(),
                        "{}: invalid `{name}` from {domain:?}",
                        rules.kind
                    );
                    assert!(name.len() <= rules.preferred_len.max(rules.min_len));
                    assert!(!taken.contains(&name), "duplicate `{name}`");
                    taken.push(name);
                }
            }
        }
    }

    #[test]
    fn candidates_are_distinct() {
        for rules in ALL {
            let c = rules.candidates("example.com");
            let mut sorted = c.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), c.len());
            assert!(c.len() > 100);
        }
    }
}
