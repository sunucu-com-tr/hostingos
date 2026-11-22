//! TOTP two-factor authentication (RFC 6238) and recovery codes.
//!
//! * SHA-1, 6 digits, 30 s step — the only combination every authenticator
//!   app supports. Secrets are 160 random bits (RFC 4226 recommendation).
//! * [`verify_code`] accepts the previous/current/next step (±30 s clock skew)
//!   and returns the **matched step**. Store it (`totp_secrets.last_used_step`)
//!   and pass it back next time: codes for that step or earlier are refused,
//!   so an observed code cannot be replayed (RFC 6238 §5.2).
//! * The secret must be stored encrypted (`SecretBox`, AAD = user id) — it is a
//!   shared key, not a hash.
//! * Recovery codes: 10 × 80-bit codes `xxxx-xxxx-xxxx-xxxx` (Crockford
//!   base32, lower-case). Only SHA-256 hashes are stored (high entropy makes a
//!   slow hash unnecessary); each code works once ([`verify_recovery_code`]
//!   returns the index to delete).

use hos_core::crypto::{constant_time_eq, random_bytes, sha256_hex};
use hos_core::secret::{Secret, SecretString};
use totp_rs::{Algorithm, Builder, Secret as RawSecret, Totp};

use crate::error::AuthError;

/// Step length in seconds.
pub const STEP_SECS: u64 = 30;
/// Code length.
pub const DIGITS: u8 = 6;
/// Accepted steps before/after the current one.
pub const SKEW_STEPS: u16 = 1;
/// Secret length in bytes.
pub const SECRET_BYTES: usize = 20;
/// Number of recovery codes issued at once.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// A TOTP shared secret. `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct TotpSecret(Secret<Vec<u8>>);

impl std::fmt::Debug for TotpSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TotpSecret([REDACTED])")
    }
}

impl TotpSecret {
    /// Generates a new random 160-bit secret.
    pub fn generate() -> Self {
        Self(Secret::new(random_bytes::<SECRET_BYTES>().to_vec()))
    }

    /// Wraps raw secret bytes (e.g. after decrypting the DB column). At least
    /// 16 bytes (128 bits) are required.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, AuthError> {
        if bytes.len() < 16 {
            return Err(AuthError::Totp("secret must be at least 128 bits".into()));
        }
        Ok(Self(Secret::new(bytes)))
    }

    /// Parses an RFC 4648 base32 secret (case-insensitive, padding optional).
    pub fn from_base32(encoded: &str) -> Result<Self, AuthError> {
        let cleaned: String = encoded
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '=')
            .collect::<String>()
            .to_ascii_uppercase();
        let raw = RawSecret::try_from_base32(&cleaned)
            .map_err(|_| AuthError::Totp("secret is not valid base32".into()))?;
        Self::from_bytes(raw.as_ref().to_vec())
    }

    /// Raw bytes (encrypt these for storage).
    pub fn as_bytes(&self) -> &[u8] {
        self.0.expose()
    }

    /// Base32 (no padding) — shown to the user for manual entry.
    pub fn to_base32(&self) -> SecretString {
        SecretString::from(
            RawSecret::from(self.as_bytes())
                .to_base32()
                .trim_end_matches('=')
                .to_owned(),
        )
    }

    fn totp(&self, issuer: Option<&str>, account: &str) -> Result<Totp, AuthError> {
        Builder::new()
            .with_algorithm(Algorithm::SHA1)
            .with_digits(DIGITS)
            .with_skew(SKEW_STEPS)
            .with_step_duration(STEP_SECS)
            .with_secret(self.as_bytes())
            .with_issuer(issuer.map(sanitize_label))
            .with_account_name(sanitize_label(account))
            .build()
            .map_err(|e| AuthError::Totp(e.to_string()))
    }
}

/// `:` separates issuer and account in otpauth labels, so it is replaced.
fn sanitize_label(s: &str) -> String {
    s.trim().replace(':', " ")
}

/// `otpauth://totp/…` URI for authenticator apps (render it as a QR code).
/// `issuer` is the tenant's display name, `account` the user's email.
pub fn otpauth_uri(secret: &TotpSecret, issuer: &str, account: &str) -> Result<String, AuthError> {
    secret
        .totp(Some(issuer), account)?
        .to_url()
        .map_err(|e| AuthError::Totp(e.to_string()))
}

/// The code for `unix_time` (for tests and development tooling).
pub fn code_at(secret: &TotpSecret, unix_time: u64) -> Result<String, AuthError> {
    Ok(secret.totp(None, "")?.generate(unix_time).to_string())
}

/// Verifies a user-entered `code` at `unix_time`.
///
/// Returns the matched time step, or `None` when the code is wrong, malformed
/// or belongs to a step `<= last_used_step` (replay).
pub fn verify_code(
    secret: &TotpSecret,
    code: &str,
    unix_time: u64,
    last_used_step: Option<u64>,
) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != usize::from(DIGITS) || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let totp = secret.totp(None, "").ok()?;
    let step = totp.check(&code, unix_time)?;
    match last_used_step {
        Some(last) if step <= last => None,
        _ => Some(step),
    }
}

// ---------------------------------------------------------------------------
// Recovery codes
// ---------------------------------------------------------------------------

/// Crockford base32 alphabet (no i, l, o, u), lower-case.
const RECOVERY_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const RECOVERY_CHARS: usize = 16;

/// Freshly generated recovery codes.
#[derive(Debug, Clone)]
pub struct RecoveryCodes {
    /// Plain codes — show once, never store.
    pub codes: Vec<SecretString>,
    /// Hashes to store (same order as `codes`).
    pub hashes: Vec<String>,
}

/// Generates [`RECOVERY_CODE_COUNT`] codes and their hashes.
pub fn generate_recovery_codes() -> RecoveryCodes {
    let mut codes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    let mut hashes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let bytes: [u8; RECOVERY_CHARS] = random_bytes();
        let mut code = String::with_capacity(RECOVERY_CHARS + 3);
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 && i % 4 == 0 {
                code.push('-');
            }
            // 256 is a multiple of 32: masking keeps the distribution uniform.
            code.push(char::from(RECOVERY_ALPHABET[usize::from(b & 0x1f)]));
        }
        hashes.push(hash_recovery_code(&code));
        codes.push(SecretString::from(code));
    }
    RecoveryCodes { codes, hashes }
}

/// Normalizes user input: lower-case, strips separators/whitespace and maps
/// Crockford look-alikes (`o`→`0`, `i`/`l`→`1`).
fn normalize_recovery_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_lowercase() {
            'o' => '0',
            'i' | 'l' => '1',
            other => other,
        })
        .collect()
}

/// Hash stored for a recovery code (normalized, domain-separated SHA-256 hex).
pub fn hash_recovery_code(code: &str) -> String {
    // Pre-rename "hf" label, kept on purpose: stored recovery-code hashes depend on it.
    sha256_hex(format!("hf-recovery-code:{}", normalize_recovery_code(code)).as_bytes())
}

/// Checks `input` against the stored hashes; returns the index of the match
/// (delete or mark that row as used). All hashes are compared.
pub fn verify_recovery_code(input: &str, stored_hashes: &[String]) -> Option<usize> {
    if normalize_recovery_code(input).len() != RECOVERY_CHARS {
        return None;
    }
    let candidate = hash_recovery_code(input);
    let mut found = None;
    for (i, stored) in stored_hashes.iter().enumerate() {
        let matches = constant_time_eq(
            candidate.as_bytes(),
            stored.trim().to_ascii_lowercase().as_bytes(),
        );
        if matches && found.is_none() {
            found = Some(i);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 Appendix B secret for SHA-1.
    fn rfc_secret() -> TotpSecret {
        TotpSecret::from_bytes(b"12345678901234567890".to_vec()).unwrap()
    }

    #[test]
    fn rfc6238_vectors_6_digits() {
        // 8-digit vectors from RFC 6238 truncated to the last 6 digits.
        let s = rfc_secret();
        for (t, expected) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
        ] {
            assert_eq!(code_at(&s, t).unwrap(), expected, "t={t}");
        }
    }

    #[test]
    fn verify_window_and_replay() {
        let s = TotpSecret::generate();
        let now = 1_700_000_000;
        let code = code_at(&s, now).unwrap();
        let step = verify_code(&s, &code, now, None).unwrap();
        assert_eq!(step, now / STEP_SECS);
        // Replay of the same step is rejected.
        assert_eq!(verify_code(&s, &code, now, Some(step)), None);
        // Previous step's code still accepted (skew), next step too.
        let prev = code_at(&s, now - STEP_SECS).unwrap();
        assert_eq!(verify_code(&s, &prev, now, None), Some(step - 1));
        let next = code_at(&s, now + STEP_SECS).unwrap();
        assert_eq!(verify_code(&s, &next, now, Some(step)), Some(step + 1));
        // Two steps away is rejected.
        let old = code_at(&s, now - 2 * STEP_SECS).unwrap();
        if old != code && old != prev && old != next {
            assert_eq!(verify_code(&s, &old, now, None), None);
        }
        // Formatting tolerated, garbage rejected.
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert_eq!(verify_code(&s, &spaced, now, None), Some(step));
        assert_eq!(verify_code(&s, "12345", now, None), None);
        assert_eq!(verify_code(&s, "abcdef", now, None), None);
        assert_eq!(verify_code(&s, "1234567", now, None), None);
    }

    #[test]
    fn secret_encoding_and_uri() {
        let s = TotpSecret::generate();
        assert_eq!(s.as_bytes().len(), SECRET_BYTES);
        let b32 = s.to_base32();
        assert_eq!(b32.expose_str().len(), 32);
        let back = TotpSecret::from_base32(&b32.expose_str().to_lowercase()).unwrap();
        assert_eq!(back, s);
        assert!(TotpSecret::from_base32("not*base32").is_err());
        assert!(TotpSecret::from_bytes(vec![1; 10]).is_err());
        assert_eq!(format!("{s:?}"), "TotpSecret([REDACTED])");

        let uri = otpauth_uri(&s, "Acme: Hosting", "ali@example.com").unwrap();
        assert!(uri.starts_with("otpauth://totp/"), "{uri}");
        assert!(
            uri.contains(&format!("secret={}", b32.expose_str())),
            "{uri}"
        );
        assert!(uri.contains("issuer=Acme"), "{uri}");
        assert!(
            uri.contains("ali%40example.com") || uri.contains("ali@example.com"),
            "{uri}"
        );
    }

    #[test]
    fn recovery_codes() {
        let rc = generate_recovery_codes();
        assert_eq!(rc.codes.len(), RECOVERY_CODE_COUNT);
        assert_eq!(rc.hashes.len(), RECOVERY_CODE_COUNT);
        let first = rc.codes[0].expose_str();
        assert_eq!(first.len(), 19);
        assert_eq!(first.matches('-').count(), 3);
        assert!(!rc.hashes.contains(&first.to_owned()));
        let mut unique = rc.hashes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), RECOVERY_CODE_COUNT);

        let third = rc.codes[2].expose_str();
        assert_eq!(verify_recovery_code(third, &rc.hashes), Some(2));
        // Case/separator-insensitive.
        let sloppy = third.replace('-', " ").to_uppercase();
        assert_eq!(verify_recovery_code(&sloppy, &rc.hashes), Some(2));
        assert_eq!(
            verify_recovery_code("0000-0000-0000-0000", &rc.hashes),
            None
        );
        assert_eq!(verify_recovery_code("short", &rc.hashes), None);
        // Look-alikes map to Crockford digits.
        assert_eq!(
            hash_recovery_code("oooo-iiii-llll-0000"),
            hash_recovery_code("0000111111110000")
        );
    }
}
