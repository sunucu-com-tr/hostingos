//! Password hashing (argon2id) and password policy.
//!
//! * [`hash_password`] — argon2id, OWASP Password Storage Cheat Sheet
//!   parameters: m = 19 MiB, t = 2, p = 1, 16-byte random salt, 32-byte output,
//!   PHC string format (`$argon2id$v=19$m=19456,t=2,p=1$…`).
//! * [`verify_password`] — uses the parameters embedded in the stored hash, so
//!   old hashes keep working after a policy change; follow up with
//!   [`needs_rehash`] after a successful login and store the new hash.
//! * [`verify_user_password`] / [`dummy_verify`] — when the user does not exist
//!   (or has no password, e.g. OAuth-only), burn the same CPU time on a dummy
//!   hash so response timing does not reveal which emails are registered.
//! * [`PasswordPolicy`] — length limits, embedded common-password list,
//!   trivial patterns and personal information (NIST SP 800-63B style: no
//!   composition rules).
//!
//! Hashing/verification are deliberately slow (tens of ms): call them from
//! `tokio::task::spawn_blocking`.

use std::sync::OnceLock;

use argon2::password_hash::Error as PhError;
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use hos_core::domain::Locale;
use serde::Serialize;

use crate::error::AuthError;

/// Argon2 memory cost in KiB (19 MiB, OWASP minimum recommendation).
pub const M_COST_KIB: u32 = 19 * 1024;
/// Argon2 iterations.
pub const T_COST: u32 = 2;
/// Argon2 parallelism.
pub const P_COST: u32 = 1;
/// Hash output length in bytes.
pub const OUTPUT_LEN: usize = 32;
/// Hard upper bound on password input in bytes, independent of the policy,
/// so a megabyte "password" cannot be used to burn CPU.
pub const MAX_PASSWORD_BYTES: usize = 1024;

fn hasher() -> Result<Argon2<'static>, AuthError> {
    let params = Params::new(M_COST_KIB, T_COST, P_COST, Some(OUTPUT_LEN))
        .map_err(|e| AuthError::PasswordHash(e.to_string()))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hashes `password` with argon2id and a fresh random salt; returns the PHC string.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AuthError::PasswordHash(
            "password exceeds maximum length".into(),
        ));
    }
    hasher()?
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| AuthError::PasswordHash(e.to_string()))
}

/// Verifies `password` against a stored PHC hash (constant-time comparison).
///
/// `Ok(false)` for a wrong password, `Err(MalformedHash)` when the stored value
/// is not a valid argon2 PHC string.
pub fn verify_password(password: &str, stored_hash: &str) -> Result<bool, AuthError> {
    let parsed = PasswordHash::new(stored_hash).map_err(|_| AuthError::MalformedHash)?;
    if !parsed.algorithm.as_str().starts_with("argon2") {
        return Err(AuthError::MalformedHash);
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Ok(false);
    }
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(PhError::PasswordInvalid) => Ok(false),
        Err(PhError::Algorithm | PhError::Version | PhError::ParamsInvalid) => {
            Err(AuthError::MalformedHash)
        }
        Err(e) => Err(AuthError::PasswordHash(e.to_string())),
    }
}

/// Whether a stored hash should be replaced (after a successful verification)
/// because it was produced with another algorithm, version or parameters than
/// the current policy. Unparsable hashes also return `true`.
pub fn needs_rehash(stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return true;
    };
    if parsed.algorithm.as_str() != "argon2id" || parsed.version != Some(0x13) {
        return true;
    }
    let salt_ok = parsed.salt.as_ref().is_some_and(|s| s.len() >= 16);
    let output_ok = parsed.hash.as_ref().is_some_and(|h| h.len() == OUTPUT_LEN);
    match Params::try_from(&parsed) {
        Ok(p) => {
            !(salt_ok
                && output_ok
                && p.m_cost() == M_COST_KIB
                && p.t_cost() == T_COST
                && p.p_cost() == P_COST)
        }
        Err(_) => true,
    }
}

static DUMMY_HASH: OnceLock<Option<String>> = OnceLock::new();

/// Performs a full argon2id verification against a dummy hash and returns
/// `false`. Call it when the account does not exist (or has no password) so the
/// "unknown user" path costs the same time as the "wrong password" path.
pub fn dummy_verify(password: &str) -> bool {
    let dummy = DUMMY_HASH.get_or_init(|| hash_password(&hos_core::crypto::generate_token()).ok());
    if let Some(hash) = dummy {
        // The dummy password is random and never returned, so this cannot succeed.
        let _ = verify_password(password, hash);
    }
    false
}

/// Timing-safe login check: verifies against `stored_hash` when present,
/// otherwise runs [`dummy_verify`]. Malformed hashes are logged and treated as
/// a failed login (after a dummy verification).
pub fn verify_user_password(password: &str, stored_hash: Option<&str>) -> bool {
    match stored_hash {
        Some(hash) => match verify_password(password, hash) {
            Ok(ok) => ok,
            Err(e) => {
                tracing::error!(error = %e, "stored password hash could not be verified");
                dummy_verify(password)
            }
        },
        None => dummy_verify(password),
    }
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Password rules applied on registration / password change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordPolicy {
    /// Minimum length in characters (Unicode scalar values).
    pub min_chars: usize,
    /// Maximum length in characters.
    pub max_chars: usize,
    /// Reject passwords from the embedded common-password list and trivial
    /// patterns (`aaaaaaaaaa`, `1234567890`, `qwertyuiop`).
    pub reject_common: bool,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_chars: 10,
            max_chars: 128,
            reject_common: true,
        }
    }
}

/// Why a password was rejected. `code()` is a stable i18n key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum PolicyViolation {
    /// Shorter than `min` characters.
    TooShort {
        /// Required minimum.
        min: usize,
    },
    /// Longer than `max` characters.
    TooLong {
        /// Allowed maximum.
        max: usize,
    },
    /// A commonly used / breached password or a trivial pattern.
    Common,
    /// Contains the user's email, name or company.
    ContainsPersonalInfo,
}

impl PolicyViolation {
    /// Stable machine-readable code (`password_too_short`, ...).
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooShort { .. } => "password_too_short",
            Self::TooLong { .. } => "password_too_long",
            Self::Common => "password_common",
            Self::ContainsPersonalInfo => "password_personal_info",
        }
    }

    /// Human readable message in `locale`.
    pub fn message(&self, locale: Locale) -> String {
        match (self, locale) {
            (Self::TooShort { min }, Locale::Tr) => format!("Parola en az {min} karakter olmalı."),
            (Self::TooShort { min }, Locale::En) => {
                format!("Password must be at least {min} characters long.")
            }
            (Self::TooLong { max }, Locale::Tr) => {
                format!("Parola en fazla {max} karakter olabilir.")
            }
            (Self::TooLong { max }, Locale::En) => {
                format!("Password must be at most {max} characters long.")
            }
            (Self::Common, Locale::Tr) => {
                "Bu parola çok yaygın veya kolay tahmin edilebilir; başka bir parola seçin.".into()
            }
            (Self::Common, Locale::En) => {
                "This password is too common or easy to guess; choose another one.".into()
            }
            (Self::ContainsPersonalInfo, Locale::Tr) => {
                "Parola e-posta adresinizi, adınızı veya şirket adınızı içermemeli.".into()
            }
            (Self::ContainsPersonalInfo, Locale::En) => {
                "Password must not contain your email address, name or company name.".into()
            }
        }
    }
}

impl PasswordPolicy {
    /// Checks `password`. `user_inputs` are personal strings (email, first/last
    /// name, company) that must not appear in the password; values shorter than
    /// 4 characters are ignored. Returns every violation found.
    pub fn validate(
        &self,
        password: &str,
        user_inputs: &[&str],
    ) -> Result<(), Vec<PolicyViolation>> {
        let mut violations = Vec::new();
        let len = password.chars().count();
        if len < self.min_chars {
            violations.push(PolicyViolation::TooShort {
                min: self.min_chars,
            });
        }
        if len > self.max_chars || password.len() > MAX_PASSWORD_BYTES {
            violations.push(PolicyViolation::TooLong {
                max: self.max_chars,
            });
        }
        let lower = password.to_lowercase();
        if self.reject_common && is_common_or_trivial(&lower) {
            violations.push(PolicyViolation::Common);
        }
        if contains_personal_info(&lower, user_inputs) {
            violations.push(PolicyViolation::ContainsPersonalInfo);
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// Lower-case passwords seen at the top of breach corpora (global + Turkish).
const COMMON_PASSWORDS: &[&str] = &[
    "123456",
    "123456789",
    "12345678",
    "1234567890",
    "12345",
    "1234567",
    "123123",
    "111111",
    "000000",
    "654321",
    "666666",
    "121212",
    "112233",
    "123321",
    "987654321",
    "0987654321",
    "1q2w3e4r",
    "1q2w3e4r5t",
    "1qaz2wsx",
    "1qaz2wsx3edc",
    "qwerty",
    "qwerty123",
    "qwertyuiop",
    "qwerty1234",
    "asdfgh",
    "asdfghjkl",
    "zxcvbnm",
    "azerty",
    "password",
    "password1",
    "password123",
    "passw0rd",
    "p@ssw0rd",
    "p@ssword",
    "pass1234",
    "passwort",
    "letmein",
    "welcome",
    "welcome1",
    "welcome123",
    "admin",
    "admin123",
    "administrator",
    "root",
    "toor",
    "changeme",
    "default",
    "guest",
    "login",
    "master",
    "secret",
    "access",
    "shadow",
    "superman",
    "batman",
    "iloveyou",
    "iloveu",
    "monkey",
    "dragon",
    "football",
    "baseball",
    "soccer",
    "princess",
    "sunshine",
    "starwars",
    "trustno1",
    "whatever",
    "freedom",
    "charlie",
    "michael",
    "jennifer",
    "jordan23",
    "hello",
    "hello123",
    "loveme",
    "abc123",
    "abcd1234",
    "abcdef",
    "aa123456",
    "a123456",
    "a12345678",
    "qazwsx",
    "zaq12wsx",
    "test",
    "test123",
    "test1234",
    "testtest",
    "demo",
    "user",
    "user1234",
    "hosting",
    "server",
    "cpanel",
    "webmaster",
    "hostingos",
    "google",
    "facebook",
    "instagram",
    "linkedin",
    "microsoft",
    "windows",
    "computer",
    "internet",
    "samsung",
    "apple",
    "sifre",
    "şifre",
    "sifre123",
    "şifre123",
    "parola",
    "parola123",
    "sifrem",
    "şifrem",
    "galatasaray",
    "fenerbahce",
    "fenerbahçe",
    "besiktas",
    "beşiktaş",
    "trabzonspor",
    "cimbom",
    "istanbul",
    "ankara",
    "izmir",
    "turkiye",
    "türkiye",
    "ataturk",
    "atatürk",
    "1905",
    "1907",
    "1903",
    "1967",
    "askim",
    "aşkım",
    "seviyorum",
    "canim",
    "canım",
    "bebegim",
    "bebeğim",
    "mustafa",
    "mehmet",
    "ahmet",
    "fatma",
    "ayse",
    "ayşe",
    "emine",
    "kral",
    "kraliçe",
    "allah",
    "deneme",
    "deneme123",
    "merhaba",
    "yasemin",
    "zeynep",
];

/// Keyboard rows and alphabets used for "sequential" detection.
const SEQUENCES: &[&str] = &[
    "01234567890",
    "09876543210",
    "abcdefghijklmnopqrstuvwxyz",
    "zyxwvutsrqponmlkjihgfedcba",
    "qwertyuiop",
    "asdfghjkl",
    "zxcvbnm",
    "poiuytrewq",
];

fn is_common_or_trivial(lower: &str) -> bool {
    if COMMON_PASSWORDS.contains(&lower) {
        return true;
    }
    // "password2024!", "galatasaray1905" → strip trailing digits/symbols.
    let base = lower.trim_end_matches(|c: char| !c.is_alphabetic());
    if base.chars().count() >= 3 && COMMON_PASSWORDS.contains(&base) {
        return true;
    }
    // Same for leading digits/symbols ("123password").
    let base = lower.trim_start_matches(|c: char| !c.is_alphabetic());
    if base.chars().count() >= 3 && COMMON_PASSWORDS.contains(&base) {
        return true;
    }
    let chars: Vec<char> = lower.chars().collect();
    // Few distinct characters ("aaaaaaaaaa", "abababab12").
    let mut distinct = chars.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() <= 3 {
        return true;
    }
    // A short pattern repeated ("abc123abc123abc123").
    for period in 1..=chars.len() / 3 {
        if chars.len().is_multiple_of(period) && chars.chunks(period).all(|c| c == &chars[..period])
        {
            return true;
        }
    }
    // Entirely a run of a known sequence ("1234567890", "qwertyuiop").
    SEQUENCES.iter().any(|seq| seq.contains(lower))
}

fn contains_personal_info(lower: &str, user_inputs: &[&str]) -> bool {
    user_inputs.iter().any(|input| {
        let input = input.trim().to_lowercase();
        let mut candidates = vec![input.clone()];
        if let Some((local, _domain)) = input.split_once('@') {
            candidates.push(local.to_owned());
        }
        candidates.extend(
            input
                .split(|c: char| !c.is_alphanumeric())
                .map(str::to_owned),
        );
        candidates
            .iter()
            .any(|c| c.chars().count() >= 4 && lower.contains(c.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify_password("correct horse battery staple", &hash).unwrap());
        assert!(!verify_password("Correct horse battery staple", &hash).unwrap());
        assert!(!needs_rehash(&hash));
        // Different salt every time.
        assert_ne!(hash, hash_password("correct horse battery staple").unwrap());
        assert!(verify_user_password(
            "correct horse battery staple",
            Some(&hash)
        ));
        assert!(!verify_user_password("nope", Some(&hash)));
    }

    #[test]
    fn malformed_and_foreign_hashes() {
        assert_eq!(
            verify_password("x", "not a hash"),
            Err(AuthError::MalformedHash)
        );
        assert_eq!(
            verify_password(
                "x",
                "$2b$12$abcdefghijklmnopqrstuuK5b0e6Yb1N3bQ9wWm8Rr0fW8cQ6Y1Z2"
            ),
            Err(AuthError::MalformedHash)
        );
        assert!(!verify_user_password("x", Some("garbage")));
        assert!(needs_rehash("garbage"));
    }

    #[test]
    fn rehash_detects_old_params() {
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8 * 1024, 1, 1, None).unwrap(),
        )
        .hash_password(b"secret-password")
        .unwrap()
        .to_string();
        assert!(needs_rehash(&weak));
        // Old hashes still verify (parameters come from the PHC string).
        assert!(verify_password("secret-password", &weak).unwrap());

        let argon2i = Argon2::new(Algorithm::Argon2i, Version::V0x13, Params::default())
            .hash_password(b"secret-password")
            .unwrap()
            .to_string();
        assert!(needs_rehash(&argon2i));
        assert!(verify_password("secret-password", &argon2i).unwrap());
    }

    #[test]
    fn dummy_verify_is_always_false() {
        assert!(!dummy_verify("anything"));
        assert!(!verify_user_password("anything", None));
    }

    #[test]
    fn overlong_passwords() {
        let long = "x".repeat(MAX_PASSWORD_BYTES + 1);
        assert!(hash_password(&long).is_err());
        let hash = hash_password("short-but-fine").unwrap();
        assert!(!verify_password(&long, &hash).unwrap());
    }

    #[test]
    fn policy() {
        let p = PasswordPolicy::default();
        assert!(p.validate("tr0ub4dor&3-horse", &[]).is_ok());
        assert_eq!(
            p.validate("short1", &[]).unwrap_err(),
            vec![PolicyViolation::TooShort { min: 10 }]
        );
        for common in [
            "password123",
            "Password2024!",
            "galatasaray1905",
            "Fenerbahçe1907",
            "1234567890",
            "qwertyuiop",
            "aaaaaaaaaaaa",
            "abcabcabcabc",
            "123password",
        ] {
            assert!(
                p.validate(common, &[])
                    .unwrap_err()
                    .contains(&PolicyViolation::Common),
                "{common} should be rejected"
            );
        }
        let err = p
            .validate(
                "ahmet.yilmaz-2026",
                &["ahmet.yilmaz@example.com", "Ahmet Yılmaz"],
            )
            .unwrap_err();
        assert_eq!(err, vec![PolicyViolation::ContainsPersonalInfo]);
        assert!(p.validate("unrelated-phrase-77", &["ali@x.io"]).is_ok());
        let too_long = "ab1-".repeat(40);
        assert!(
            p.validate(&too_long, &[])
                .unwrap_err()
                .contains(&PolicyViolation::TooLong { max: 128 })
        );
        // Unicode counts characters, not bytes.
        assert!(p.validate("çğıöşüÇĞİÖ", &[]).is_ok());
    }

    #[test]
    fn violation_messages() {
        let v = PolicyViolation::TooShort { min: 10 };
        assert_eq!(v.code(), "password_too_short");
        assert!(v.message(Locale::Tr).contains("10"));
        assert!(v.message(Locale::En).contains("10"));
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["code"], "too_short");
        assert_eq!(json["min"], 10);
    }
}
