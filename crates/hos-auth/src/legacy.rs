//! Password hashes imported from another billing system (WHMCS, see
//! `hos-import`), verified so migrated clients keep their password.
//!
//! **Read-only, no downgrade.** This module never hashes a password: it only
//! checks a password against a hash that was carried over, and it re-encodes
//! imported values ([`from_whmcs`]). After a successful login the caller
//! replaces the stored value with a fresh argon2id hash
//! ([`password::needs_rehash`](crate::password::needs_rehash) is `true` for
//! every legacy value), so a legacy hash is used at most once per account.
//!
//! | Stored value | Origin | Login |
//! |---|---|---|
//! | `$2y$` / `$2a$` / `$2b$` bcrypt, cost 4–[`MAX_BCRYPT_COST`] | WHMCS ≥ 6.3 (`password_hash`) | accepted, then re-hashed to argon2id |
//! | `$hos-legacy$whmcs-md5$<hex salt>$<md5 hex>` | WHMCS 4.2–6.2: `md5(salt . password) . ':' . salt` (5-char salt) | **never accepted**: a matching password only proves who the user is and forces a password reset ([`Verification::ResetRequired`]) |
//! | `$hos-legacy$md5$<md5 hex>` | unsalted `md5(password)` (very old admin rows) | as above |
//!
//! Salted / plain MD5 are fast, unsalted-or-short-salted digests: anyone who
//! obtained the WHMCS database can brute-force them, so knowing the password
//! is not enough to open a session. The login page sends a reset link to the
//! account's e-mail address instead (the attacker does not control that
//! mailbox) — see `hos-web` `routes/auth/login.rs`.
//!
//! Comparisons are constant-time (`subtle` in the bcrypt crate, and
//! [`hos_core::crypto::constant_time_eq`] for the MD5 digests).
//!
//! **Constant cost.** [`verify_any`] does the same work for every account:
//! one argon2id and one bcrypt verification, real or dummy. An unknown
//! e-mail, a native argon2id account and an imported MD5 account also run a
//! bcrypt check at [`DUMMY_BCRYPT_COST`] (PHP's default, what WHMCS writes);
//! an imported bcrypt account also runs a dummy argon2id check. So timing
//! tells neither whether an address exists nor how its password is stored.

use std::sync::OnceLock;

use md5::{Digest as _, Md5};

use crate::password::{self, MAX_PASSWORD_BYTES};

/// Prefix of the re-encoded weak legacy formats.
pub const LEGACY_PREFIX: &str = "$hos-legacy$";

/// Highest bcrypt cost accepted from an import (PHP's default is 10; cost 15
/// already takes seconds and a crafted value could be used to burn CPU).
pub const MAX_BCRYPT_COST: u32 = 14;

/// bcrypt cost of the dummy check in [`verify_any`].
pub const DUMMY_BCRYPT_COST: u32 = 10;

static DUMMY_BCRYPT: OnceLock<Option<String>> = OnceLock::new();

// Counts the verifications of this thread (`[argon2id, bcrypt]`, tests).
#[cfg(test)]
thread_local! {
    static WORK: std::cell::Cell<[u32; 2]> = const { std::cell::Cell::new([0, 0]) };
}

#[cfg_attr(not(test), allow(unused_variables))]
fn spent(argon2: u32, bcrypt: u32) {
    #[cfg(test)]
    WORK.with(|w| {
        let [a, b] = w.get();
        w.set([a + argon2, b + bcrypt]);
    });
}

/// A full bcrypt verification against a random hash (never matches). Like
/// every check, an over-long password costs nothing.
fn dummy_bcrypt(password: &str) {
    if password.len() > MAX_PASSWORD_BYTES {
        return;
    }
    let dummy = DUMMY_BCRYPT
        .get_or_init(|| bcrypt::hash(hos_core::crypto::generate_token(), DUMMY_BCRYPT_COST).ok());
    if let Some(hash) = dummy {
        let _ = bcrypt::verify(password.as_bytes(), hash);
    }
    spent(0, 1);
}

/// [`password::dummy_verify`], counted.
fn dummy_argon2(password: &str) {
    let _ = password::dummy_verify(password);
    spent(1, 0);
}

/// A legacy hash scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LegacyScheme {
    /// bcrypt (`$2y$`, `$2a$`, `$2b$`).
    Bcrypt,
    /// WHMCS ≤ 6.2 client hash: `md5(salt . password)` with a short salt.
    WhmcsSaltedMd5,
    /// Unsalted `md5(password)`.
    Md5,
}

impl LegacyScheme {
    /// Stable name (`bcrypt`, `whmcs_md5`, `md5`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bcrypt => "bcrypt",
            Self::WhmcsSaltedMd5 => "whmcs_md5",
            Self::Md5 => "md5",
        }
    }

    /// Weak schemes are never accepted for a login (reset required).
    pub const fn is_weak(self) -> bool {
        !matches!(self, Self::Bcrypt)
    }
}

/// Outcome of checking a password against a stored hash of any supported kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// The password is correct; open the session (and re-hash when
    /// [`needs_rehash`](crate::password::needs_rehash) says so).
    Valid,
    /// Wrong password, no hash, or an unusable hash.
    Invalid,
    /// The password matches a weak legacy hash: do not log in, require a
    /// password reset (e-mailed link).
    ResetRequired,
}

/// An imported hash ready to be stored in `users.password_hash`.
#[derive(Clone, PartialEq, Eq)]
pub struct ImportedHash {
    /// Value for `users.password_hash`.
    pub stored: String,
    /// Its scheme.
    pub scheme: LegacyScheme,
}

impl std::fmt::Debug for ImportedHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hashes are credentials too: never print them.
        f.debug_struct("ImportedHash")
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Cost of a well-formed bcrypt string, `None` otherwise.
fn bcrypt_cost(value: &str) -> Option<u32> {
    let b = value.as_bytes();
    if b.len() != 60 || b[0] != b'$' || b[3] != b'$' || b[6] != b'$' {
        return None;
    }
    if !matches!(&value[1..3], "2y" | "2a" | "2b") {
        return None;
    }
    let cost: u32 = value[4..6].parse().ok()?;
    let tail_ok = value[7..]
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'/');
    (tail_ok && (4..=31).contains(&cost)).then_some(cost)
}

/// Converts a WHMCS `password` column value (`tblusers`, `tblclients`,
/// `tblcontacts`) into the value to store, or `None` when the value is empty
/// or not a recognised hash (the account then has no password and must use
/// "forgot password"). bcrypt values above [`MAX_BCRYPT_COST`] are refused.
pub fn from_whmcs(raw: &str) -> Option<ImportedHash> {
    let v = raw.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(cost) = bcrypt_cost(v) {
        return (cost <= MAX_BCRYPT_COST).then(|| ImportedHash {
            stored: v.to_owned(),
            scheme: LegacyScheme::Bcrypt,
        });
    }
    // `md5hex:salt` — the salt is printable ASCII (WHMCS uses 5 characters);
    // it is hex-encoded so any byte survives the `$` separators.
    if let Some((digest, salt)) = v.split_once(':')
        && is_hex(digest, 32)
        && !salt.is_empty()
        && salt.len() <= 64
    {
        return Some(ImportedHash {
            stored: format!(
                "{LEGACY_PREFIX}whmcs-md5${}${}",
                hex::encode(salt.as_bytes()),
                digest.to_ascii_lowercase()
            ),
            scheme: LegacyScheme::WhmcsSaltedMd5,
        });
    }
    if is_hex(v, 32) {
        return Some(ImportedHash {
            stored: format!("{LEGACY_PREFIX}md5${}", v.to_ascii_lowercase()),
            scheme: LegacyScheme::Md5,
        });
    }
    None
}

/// The legacy scheme of a stored value, `None` for argon2 / unknown values.
pub fn scheme_of(stored: &str) -> Option<LegacyScheme> {
    if bcrypt_cost(stored).is_some() {
        return Some(LegacyScheme::Bcrypt);
    }
    let rest = stored.strip_prefix(LEGACY_PREFIX)?;
    if rest.starts_with("whmcs-md5$") {
        Some(LegacyScheme::WhmcsSaltedMd5)
    } else if rest.starts_with("md5$") {
        Some(LegacyScheme::Md5)
    } else {
        None
    }
}

/// Whether `stored` is a legacy value (always `needs_rehash`).
pub fn is_legacy(stored: &str) -> bool {
    scheme_of(stored).is_some()
}

fn md5_hex(parts: &[&[u8]]) -> String {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    hex::encode(h.finalize().as_slice())
}

/// Checks `password` against a legacy value. Unknown / malformed values are
/// [`Verification::Invalid`].
pub fn verify_legacy(password: &str, stored: &str) -> Verification {
    if password.len() > MAX_PASSWORD_BYTES {
        return Verification::Invalid;
    }
    match scheme_of(stored) {
        Some(LegacyScheme::Bcrypt) => {
            let cost_ok = bcrypt_cost(stored).is_some_and(|c| c <= MAX_BCRYPT_COST);
            if !cost_ok {
                dummy_bcrypt(password);
                return Verification::Invalid;
            }
            spent(0, 1);
            // PHP's bcrypt truncates at 72 bytes; `bcrypt::verify` does the same.
            match bcrypt::verify(password.as_bytes(), stored) {
                Ok(true) => Verification::Valid,
                _ => Verification::Invalid,
            }
        }
        Some(LegacyScheme::WhmcsSaltedMd5) => {
            let ok = (|| {
                let rest = stored
                    .strip_prefix(LEGACY_PREFIX)?
                    .strip_prefix("whmcs-md5$")?;
                let (salt_hex, digest) = rest.split_once('$')?;
                let salt = hex::decode(salt_hex).ok()?;
                if !is_hex(digest, 32) {
                    return None;
                }
                let computed = md5_hex(&[&salt, password.as_bytes()]);
                Some(hos_core::crypto::constant_time_eq(
                    computed.as_bytes(),
                    digest.as_bytes(),
                ))
            })()
            .unwrap_or(false);
            dummy_argon2(password);
            if ok {
                Verification::ResetRequired
            } else {
                Verification::Invalid
            }
        }
        Some(LegacyScheme::Md5) => {
            let ok = stored
                .strip_prefix(LEGACY_PREFIX)
                .and_then(|r| r.strip_prefix("md5$"))
                .filter(|d| is_hex(d, 32))
                .is_some_and(|digest| {
                    hos_core::crypto::constant_time_eq(
                        md5_hex(&[password.as_bytes()]).as_bytes(),
                        digest.as_bytes(),
                    )
                });
            dummy_argon2(password);
            if ok {
                Verification::ResetRequired
            } else {
                Verification::Invalid
            }
        }
        None => Verification::Invalid,
    }
}

/// Login check for any stored value: argon2id (the platform's own hashes),
/// a legacy import, or nothing (dummy verification, see
/// [`password::verify_user_password`]). Every path costs one argon2id and
/// one bcrypt verification (see the module docs). Like the argon2 functions
/// this is CPU-heavy; call it from `spawn_blocking`.
pub fn verify_any(password: &str, stored: Option<&str>) -> Verification {
    match stored {
        Some(hash) if is_legacy(hash) => {
            let verdict = verify_legacy(password, hash);
            match scheme_of(hash) {
                Some(LegacyScheme::Bcrypt) => dummy_argon2(password),
                _ => dummy_bcrypt(password),
            }
            verdict
        }
        other => {
            let ok = password::verify_user_password(password, other);
            spent(1, 0);
            dummy_bcrypt(password);
            if ok {
                Verification::Valid
            } else {
                Verification::Invalid
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example of PHP's `password_verify()` manual page
    /// (<https://www.php.net/manual/en/function.password-verify.php>):
    /// `password_hash("rasmuslerdorf", PASSWORD_DEFAULT)`.
    const PHP_DOC_HASH: &str = "$2y$10$.vGA1O9wmRjrwAVXD98HNOgsNpDczlqm3Jq7KnEd1rVAGv3Fykk1a";

    #[test]
    fn bcrypt_from_php_password_hash() {
        let imported = from_whmcs(PHP_DOC_HASH).expect("bcrypt recognised");
        assert_eq!(imported.scheme, LegacyScheme::Bcrypt);
        assert_eq!(imported.stored, PHP_DOC_HASH);
        assert_eq!(
            verify_any("rasmuslerdorf", Some(&imported.stored)),
            Verification::Valid
        );
        assert_eq!(
            verify_any("rasmuslerdorF", Some(&imported.stored)),
            Verification::Invalid
        );
        assert!(is_legacy(&imported.stored));
        // Legacy values are always replaced after a successful login.
        assert!(password::needs_rehash(&imported.stored));
        // Debug never shows the hash.
        assert!(!format!("{imported:?}").contains("vGA1"));
    }

    #[test]
    fn bcrypt_variants_and_cost_cap() {
        // Same digest with the other PHP / OpenBSD prefixes.
        for prefix in ["$2a$", "$2b$"] {
            let h = PHP_DOC_HASH.replacen("$2y$", prefix, 1);
            assert_eq!(verify_any("rasmuslerdorf", Some(&h)), Verification::Valid);
        }
        let expensive = PHP_DOC_HASH.replacen("$10$", "$15$", 1);
        assert!(from_whmcs(&expensive).is_none());
        assert_eq!(
            verify_legacy("rasmuslerdorf", &expensive),
            Verification::Invalid
        );
        // `$2x$` (crypt_blowfish bug-compat) is not imported.
        assert!(from_whmcs(&PHP_DOC_HASH.replacen("$2y$", "$2x$", 1)).is_none());
    }

    #[test]
    fn whmcs_salted_md5_forces_reset() {
        // WHMCS 4.2–6.2: md5($salt . $password) . ':' . $salt
        let salt = "a:B$c";
        let digest = md5_hex(&[salt.as_bytes(), b"hunter22"]);
        let raw = format!("{digest}:{salt}");
        let imported = from_whmcs(&raw).expect("recognised");
        assert_eq!(imported.scheme, LegacyScheme::WhmcsSaltedMd5);
        assert!(imported.stored.starts_with("$hos-legacy$whmcs-md5$"));
        assert_eq!(
            verify_any("hunter22", Some(&imported.stored)),
            Verification::ResetRequired
        );
        assert_eq!(
            verify_any("hunter23", Some(&imported.stored)),
            Verification::Invalid
        );
        assert!(LegacyScheme::WhmcsSaltedMd5.is_weak());
        assert!(password::needs_rehash(&imported.stored));
    }

    #[test]
    fn known_md5_vector() {
        // RFC 1321 test suite: MD5("abc").
        assert_eq!(md5_hex(&[b"abc"]), "900150983cd24fb0d6963f7d28e17f72");
        let imported = from_whmcs("900150983CD24FB0D6963F7D28E17F72").unwrap();
        assert_eq!(imported.scheme, LegacyScheme::Md5);
        assert_eq!(
            verify_legacy("abc", &imported.stored),
            Verification::ResetRequired
        );
        assert_eq!(
            verify_legacy("abd", &imported.stored),
            Verification::Invalid
        );
    }

    #[test]
    fn unknown_and_hostile_values() {
        for raw in ["", "   ", "plaintext", "$1$abc$def", "zz:salt", "abc:"] {
            assert!(from_whmcs(raw).is_none(), "{raw:?}");
        }
        assert_eq!(
            verify_legacy("x", "$hos-legacy$whmcs-md5$zz$00"),
            Verification::Invalid
        );
        assert_eq!(
            verify_legacy("x", "$hos-legacy$sha1$abc"),
            Verification::Invalid
        );
        assert_eq!(verify_any("x", None), Verification::Invalid);
        let long = "x".repeat(MAX_PASSWORD_BYTES + 1);
        assert_eq!(verify_legacy(&long, PHP_DOC_HASH), Verification::Invalid);
    }

    /// Every login check does one argon2id and one bcrypt verification,
    /// whatever is stored (unknown address, native hash, either import).
    #[test]
    fn every_login_check_costs_the_same() {
        let argon = password::hash_password("doğru parola").unwrap();
        let bcrypt = bcrypt::hash("doğru parola", 4).unwrap();
        let salted = from_whmcs(&format!("{}:abcde", md5_hex(&[b"abcde", b"x"])))
            .unwrap()
            .stored;
        let weird_cost = PHP_DOC_HASH.replace("$10$", "$20$");
        for stored in [
            None,
            Some(argon.as_str()),
            Some(bcrypt.as_str()),
            Some(salted.as_str()),
            Some(weird_cost.as_str()),
        ] {
            for pw in ["doğru parola", "yanlış"] {
                WORK.with(|w| w.set([0, 0]));
                let _ = verify_any(pw, stored);
                assert_eq!(WORK.with(std::cell::Cell::get), [1, 1], "{stored:?}");
            }
        }
        assert_eq!(
            verify_any("doğru parola", Some(&bcrypt)),
            Verification::Valid
        );
    }

    #[test]
    fn argon2_still_verifies_through_verify_any() {
        let h = password::hash_password("correct horse battery").unwrap();
        assert!(!is_legacy(&h));
        assert_eq!(
            verify_any("correct horse battery", Some(&h)),
            Verification::Valid
        );
        assert_eq!(verify_any("nope", Some(&h)), Verification::Invalid);
    }
}
