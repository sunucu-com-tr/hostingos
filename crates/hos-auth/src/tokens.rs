//! One-time tokens and API keys.
//!
//! ## One-time tokens (password reset, email verification, invite)
//!
//! [`issue_token`] returns the plain token (goes into the emailed link) and a
//! purpose-bound SHA-256 hash (goes into `password_resets` /
//! `email_verifications` / invites). On use: `hash_one_time_token(purpose,
//! presented)` → look the row up by hash → [`check_one_time_token`] (expiry,
//! already used) → mark the row consumed **in the same transaction** as the
//! action. Binding the purpose into the hash means a token of one kind can
//! never be redeemed as another.
//!
//! ## API keys
//!
//! Format: `hos_live_<lookup>_<secret>` — `lookup` is 10 lower-case Crockford
//! base32 chars stored in clear (indexed, unique) and shown in the UI; `secret`
//! is 40 base62 chars (~238 bits). Only [`hash_api_key`] of the full key is
//! stored. Auth: [`bearer_token`] → [`parse_api_key`] → load row by `lookup` →
//! [`verify_api_key`].

use chrono::{DateTime, Duration, Utc};
use hos_core::crypto::{constant_time_eq, generate_token, hash_token, random_bytes, sha256_hex};
use hos_core::secret::SecretString;

use crate::error::AuthError;

hos_core::string_enum! {
    /// What a one-time token is for.
    pub enum TokenPurpose {
        /// "Forgot password" link.
        PasswordReset => "password_reset",
        /// Email address verification link.
        EmailVerification => "email_verification",
        /// Staff / client-user invitation.
        Invite => "invite",
    }
}

impl TokenPurpose {
    /// Default lifetime: reset 1 h, verification 48 h, invite 7 days.
    pub fn default_ttl(&self) -> Duration {
        match self {
            Self::PasswordReset => Duration::hours(1),
            Self::EmailVerification => Duration::hours(48),
            Self::Invite => Duration::days(7),
        }
    }
}

/// A freshly issued one-time token.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// Purpose.
    pub purpose: TokenPurpose,
    /// Plain token for the link. Never store or log.
    pub token: SecretString,
    /// Hash to store (`token_hash` column).
    pub token_hash: String,
    /// Expiry to store.
    pub expires_at: DateTime<Utc>,
}

/// Issues a token with the purpose's default TTL.
pub fn issue_token(purpose: TokenPurpose, now: DateTime<Utc>) -> IssuedToken {
    issue_token_with_ttl(purpose, now, purpose.default_ttl())
}

/// Issues a token with an explicit TTL.
pub fn issue_token_with_ttl(
    purpose: TokenPurpose,
    now: DateTime<Utc>,
    ttl: Duration,
) -> IssuedToken {
    let token = generate_token();
    IssuedToken {
        purpose,
        token_hash: hash_one_time_token(purpose, &token),
        token: SecretString::from(token),
        expires_at: now + ttl,
    }
}

/// Purpose-bound hash used for storage and lookup.
pub fn hash_one_time_token(purpose: TokenPurpose, token: &str) -> String {
    // Pre-rename "hf" label, kept on purpose: stored token hashes are looked up with it.
    sha256_hex(format!("hf-ott:{}:{}", purpose.as_str(), token.trim()).as_bytes())
}

/// Validates a looked-up token row: `presented` must hash to `stored_hash`
/// (constant time), not be expired and not be consumed.
pub fn check_one_time_token(
    purpose: TokenPurpose,
    presented: &str,
    stored_hash: &str,
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<(), AuthError> {
    if !crate::session::is_well_formed_token(presented.trim()) {
        return Err(AuthError::MalformedToken);
    }
    let candidate = hash_one_time_token(purpose, presented);
    if !constant_time_eq(
        candidate.as_bytes(),
        stored_hash.trim().to_ascii_lowercase().as_bytes(),
    ) {
        return Err(AuthError::InvalidToken);
    }
    if consumed_at.is_some() {
        return Err(AuthError::AlreadyUsed);
    }
    if now >= expires_at {
        return Err(AuthError::Expired);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/// Prefix of every live API key.
pub const API_KEY_PREFIX: &str = "hos_live_";
/// Length of the lookup part.
pub const API_KEY_LOOKUP_LEN: usize = 10;
/// Length of the secret part.
pub const API_KEY_SECRET_LEN: usize = 40;

const LOOKUP_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const SECRET_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A newly generated API key.
#[derive(Debug, Clone)]
pub struct GeneratedApiKey {
    /// Full key — show once, never store.
    pub key: SecretString,
    /// Lookup prefix to store (unique index) and display.
    pub lookup: String,
    /// [`hash_api_key`] of the full key — the stored hash.
    pub key_hash: String,
    /// Display hint, e.g. `hos_live_ab12cd34ef_…Xy9z`.
    pub hint: String,
}

/// A syntactically valid API key, split into its parts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ParsedApiKey<'a> {
    full: &'a str,
    /// Lookup prefix (search `api_keys.lookup` with it).
    pub lookup: &'a str,
}

impl std::fmt::Debug for ParsedApiKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedApiKey")
            .field("lookup", &self.lookup)
            .finish_non_exhaustive()
    }
}

fn random_string(alphabet: &[u8], len: usize) -> String {
    let mut out = String::with_capacity(len);
    // Rejection sampling keeps the distribution uniform for any alphabet size.
    let limit = 256 - (256 % alphabet.len());
    while out.len() < len {
        for b in random_bytes::<64>() {
            if usize::from(b) < limit {
                out.push(char::from(alphabet[usize::from(b) % alphabet.len()]));
                if out.len() == len {
                    break;
                }
            }
        }
    }
    out
}

/// Generates a new API key.
pub fn generate_api_key() -> GeneratedApiKey {
    let lookup = random_string(LOOKUP_ALPHABET, API_KEY_LOOKUP_LEN);
    let secret = random_string(SECRET_ALPHABET, API_KEY_SECRET_LEN);
    let key = format!("{API_KEY_PREFIX}{lookup}_{secret}");
    GeneratedApiKey {
        key_hash: hash_api_key(&key),
        hint: format!(
            "{API_KEY_PREFIX}{lookup}_…{}",
            &secret[API_KEY_SECRET_LEN - 4..]
        ),
        key: SecretString::from(key),
        lookup,
    }
}

/// Parses `hos_live_<lookup>_<secret>`; rejects anything else.
pub fn parse_api_key(key: &str) -> Result<ParsedApiKey<'_>, AuthError> {
    let key = key.trim();
    let rest = key
        .strip_prefix(API_KEY_PREFIX)
        .ok_or(AuthError::MalformedToken)?;
    let (lookup, secret) = rest.split_once('_').ok_or(AuthError::MalformedToken)?;
    let lookup_ok =
        lookup.len() == API_KEY_LOOKUP_LEN && lookup.bytes().all(|b| LOOKUP_ALPHABET.contains(&b));
    let secret_ok =
        secret.len() == API_KEY_SECRET_LEN && secret.bytes().all(|b| b.is_ascii_alphanumeric());
    if !(lookup_ok && secret_ok) {
        return Err(AuthError::MalformedToken);
    }
    Ok(ParsedApiKey { full: key, lookup })
}

/// Hash stored for an API key (SHA-256 hex of the full key).
pub fn hash_api_key(key: &str) -> String {
    hash_token(key.trim())
}

/// Constant-time check of a parsed key against the stored hash.
pub fn verify_api_key(parsed: &ParsedApiKey<'_>, stored_hash: &str) -> bool {
    hos_core::crypto::verify_token_hash(parsed.full, stored_hash)
}

/// Extracts the token from an `Authorization: Bearer <token>` header value.
pub fn bearer_token(authorization: &str) -> Option<&str> {
    let (scheme, token) = authorization.trim().split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()
    }

    #[test]
    fn one_time_token_lifecycle() {
        let t = issue_token(TokenPurpose::PasswordReset, now());
        assert_eq!(t.expires_at, now() + Duration::hours(1));
        let plain = t.token.expose_str();
        assert_ne!(t.token_hash, hash_token(plain));
        let ok = |at| {
            check_one_time_token(
                TokenPurpose::PasswordReset,
                plain,
                &t.token_hash,
                t.expires_at,
                None,
                at,
            )
        };
        assert_eq!(ok(now() + Duration::minutes(59)), Ok(()));
        assert_eq!(ok(now() + Duration::hours(1)), Err(AuthError::Expired));
        assert_eq!(
            check_one_time_token(
                TokenPurpose::PasswordReset,
                plain,
                &t.token_hash,
                t.expires_at,
                Some(now()),
                now()
            ),
            Err(AuthError::AlreadyUsed)
        );
        // Same token, other purpose → hash differs → invalid.
        assert_eq!(
            check_one_time_token(
                TokenPurpose::Invite,
                plain,
                &t.token_hash,
                t.expires_at,
                None,
                now()
            ),
            Err(AuthError::InvalidToken)
        );
        let other = generate_token();
        assert_eq!(
            check_one_time_token(
                TokenPurpose::PasswordReset,
                &other,
                &t.token_hash,
                t.expires_at,
                None,
                now()
            ),
            Err(AuthError::InvalidToken)
        );
        assert_eq!(
            check_one_time_token(
                TokenPurpose::PasswordReset,
                "x",
                &t.token_hash,
                t.expires_at,
                None,
                now()
            ),
            Err(AuthError::MalformedToken)
        );
        assert_eq!(TokenPurpose::Invite.default_ttl(), Duration::days(7));
        assert_eq!(
            TokenPurpose::EmailVerification.as_str(),
            "email_verification"
        );
    }

    #[test]
    fn api_keys() {
        let k = generate_api_key();
        let key = k.key.expose_str();
        assert!(key.starts_with("hos_live_"));
        assert_eq!(
            key.len(),
            API_KEY_PREFIX.len() + API_KEY_LOOKUP_LEN + 1 + API_KEY_SECRET_LEN
        );
        let parsed = parse_api_key(key).unwrap();
        assert_eq!(parsed.lookup, k.lookup);
        assert!(verify_api_key(&parsed, &k.key_hash));
        assert!(!format!("{parsed:?}").contains(&key[20..]));
        assert!(k.hint.starts_with(&format!("hos_live_{}_…", k.lookup)));
        assert!(
            !k.hint
                .contains(&key[API_KEY_PREFIX.len() + API_KEY_LOOKUP_LEN + 1..][..10])
        );

        // Tampered secret fails verification but parses.
        let mut tampered = key.to_owned();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'a' { 'b' } else { 'a' });
        assert!(!verify_api_key(
            &parse_api_key(&tampered).unwrap(),
            &k.key_hash
        ));

        for bad in [
            "",
            "hos_live_",
            "hos_test_abcdefghjk_0123456789012345678901234567890123456789",
            "hos_live_ABCDEFGHJK_0123456789012345678901234567890123456789",
            "hos_live_abcdefghjk_01234567890123456789012345678901234567-9",
            "hos_live_abcdefghjk_short",
            "hos_live_abcdefghjk0123456789012345678901234567890123456789",
        ] {
            assert_eq!(
                parse_api_key(bad).unwrap_err(),
                AuthError::MalformedToken,
                "{bad}"
            );
        }
        assert_ne!(generate_api_key().lookup, k.lookup);
    }

    #[test]
    fn bearer() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer   abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearer"), None);
    }

    #[test]
    fn random_string_alphabet() {
        let s = random_string(SECRET_ALPHABET, 500);
        assert_eq!(s.len(), 500);
        assert!(s.bytes().all(|b| SECRET_ALPHABET.contains(&b)));
    }
}
