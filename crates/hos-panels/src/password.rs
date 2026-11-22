//! Strong password generation within each panel's character restrictions.
//!
//! Passwords use a CSPRNG (`rand::rng()`, ChaCha reseeded from the OS), always
//! contain at least one lowercase letter, uppercase letter, digit and (when
//! the panel allows any) symbol, and skip look-alike characters (`l`, `I`, `O`,
//! `0`, `1`).
//!
//! Symbol sets are deliberately conservative:
//! * CyberPanel's security middleware rejects any `/api/` value containing
//!   `;`, `&&`, `||`, `|`, `...`, `` ` ``, `$`, `../` or newlines.
//! * DirectAdmin and Plesk pass passwords to system tools; quotes, backslashes,
//!   `&`, `$` and spaces are avoided everywhere.

use hos_core::integrations::kinds;
use hos_core::secret::SecretString;
use rand::RngExt as _;
use rand::seq::SliceRandom as _;

const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
const DIGITS: &[u8] = b"23456789";

/// Length/charset policy for generated passwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordPolicy {
    /// Generated length (minimum 12 is enforced).
    pub length: usize,
    /// Allowed symbol characters (ASCII).
    pub symbols: &'static str,
}

impl PasswordPolicy {
    /// cPanel/WHM (password strength meter favours length + classes).
    pub const CPANEL: Self = Self {
        length: 20,
        symbols: "!@#%^*-_=+",
    };
    /// Plesk (the "strong" policy requires all four classes).
    pub const PLESK: Self = Self {
        length: 20,
        symbols: "!@#%^*-_=+",
    };
    /// DirectAdmin.
    pub const DIRECTADMIN: Self = Self {
        length: 20,
        symbols: "!@#^*-_=+",
    };
    /// CyberPanel (see module docs for the forbidden sequences).
    pub const CYBERPANEL: Self = Self {
        length: 20,
        symbols: "@#%^*_+=!",
    };

    /// Policy for a panel kind (`mock`/unknown → cPanel's).
    pub fn for_kind(kind: &str) -> Self {
        match kind {
            kinds::PLESK => Self::PLESK,
            kinds::DIRECTADMIN => Self::DIRECTADMIN,
            kinds::CYBERPANEL => Self::CYBERPANEL,
            _ => Self::CPANEL,
        }
    }

    /// Generates a password.
    pub fn generate(&self) -> SecretString {
        let mut rng = rand::rng();
        let length = self.length.max(12);
        let symbols = self.symbols.as_bytes();
        let mut classes: Vec<&[u8]> = vec![LOWER, UPPER, DIGITS];
        if !symbols.is_empty() {
            classes.push(symbols);
        }
        let all: Vec<u8> = classes.iter().flat_map(|c| c.iter().copied()).collect();
        let mut bytes: Vec<u8> = classes
            .iter()
            .map(|c| c[rng.random_range(0..c.len())])
            .collect();
        while bytes.len() < length {
            bytes.push(all[rng.random_range(0..all.len())]);
        }
        bytes.shuffle(&mut rng);
        // Every byte comes from the ASCII tables above.
        SecretString::from(bytes.into_iter().map(char::from).collect::<String>())
    }

    /// Whether `password` uses only this policy's characters and contains
    /// every class (used by tests and to vet caller-supplied passwords).
    pub fn is_compliant(&self, password: &str) -> bool {
        let allowed = |c: char| c.is_ascii_alphanumeric() || self.symbols.contains(c);
        password.chars().count() >= 12
            && password.chars().all(allowed)
            && password.chars().any(|c| c.is_ascii_lowercase())
            && password.chars().any(|c| c.is_ascii_uppercase())
            && password.chars().any(|c| c.is_ascii_digit())
            && (self.symbols.is_empty() || password.chars().any(|c| self.symbols.contains(c)))
    }
}

/// Generates a password suitable for the given panel kind.
pub fn generate_password(kind: &str) -> SecretString {
    PasswordPolicy::for_kind(kind).generate()
}

/// Characters CyberPanel's `/api/` middleware rejects (see module docs).
pub(crate) fn cyberpanel_rejects(value: &str) -> bool {
    const BAD: &[&str] = &[";", "&&", "||", "|", "...", "`", "$", "../", "\n", "- -"];
    BAD.iter().any(|b| value.contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_comply() {
        for kind in [
            kinds::CPANEL,
            kinds::PLESK,
            kinds::DIRECTADMIN,
            kinds::CYBERPANEL,
            "mock",
        ] {
            let policy = PasswordPolicy::for_kind(kind);
            let mut seen = std::collections::HashSet::new();
            for _ in 0..300 {
                let pw = policy.generate();
                let pw = pw.expose_str();
                assert_eq!(pw.len(), 20);
                assert!(policy.is_compliant(pw), "{kind}: {pw}");
                assert!(!pw.contains(['\'', '"', '\\', '&', '$', ' ', '|', ';', '`']));
                if kind == kinds::CYBERPANEL {
                    assert!(!cyberpanel_rejects(pw));
                }
                assert!(seen.insert(pw.to_owned()), "repeated password");
            }
        }
    }

    #[test]
    fn compliance_check() {
        let p = PasswordPolicy::CPANEL;
        assert!(p.is_compliant("Abcdefgh2345!x"));
        assert!(!p.is_compliant("abcdefgh2345!x"));
        assert!(!p.is_compliant("Abc2!"));
        assert!(!p.is_compliant("Abcdefgh2345 x!"));
        assert!(cyberpanel_rejects("abc;rm"));
        assert!(!cyberpanel_rejects("Abc@123_x"));
    }
}
