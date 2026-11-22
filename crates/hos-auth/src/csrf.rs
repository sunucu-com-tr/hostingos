//! CSRF protection: session-bound, HMAC-signed tokens.
//!
//! Token = `base64url(nonce[16] || HMAC-SHA256(k, "hf-csrf/v1" || nonce || binding))`
//! where `k` is derived from `HOS_SECRET_KEY` (never the raw master key) and
//! `binding` is a per-browser secret value — use the session's `token_hash`
//! for logged-in users, or the hash of an anonymous pre-session cookie for
//! login/registration forms. A fresh nonce per token makes every rendered
//! token different (BREACH-style compression attacks see no stable secret),
//! and a token stolen from one session is useless in another.
//!
//! `hos-web` checks every non-safe method ([`is_safe_method`]): token from the
//! `X-CSRF-Token` header (htmx) or the `csrf_token` form field
//! ([`extract_token`]), then [`CsrfProtector::verify`]. [`is_same_origin`] is
//! an additional, cheap `Origin`/`Referer` check.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hos_core::crypto::{hmac_sha256, random_bytes, verify_hmac_sha256};
use url::Url;

/// Form field name carrying the token.
pub const CSRF_FORM_FIELD: &str = "csrf_token";
/// Request header carrying the token (htmx: `hx-headers`).
pub const CSRF_HEADER: &str = "X-CSRF-Token";

const NONCE_LEN: usize = 16;
const MAC_LEN: usize = 32;
// Pre-rename "hf" label, kept on purpose: HMAC domain of issued CSRF tokens.
const DOMAIN: &[u8] = b"hf-csrf/v1";

/// Issues and verifies CSRF tokens. Cheap to clone; `Debug` hides the key.
#[derive(Clone)]
pub struct CsrfProtector {
    key: [u8; 32],
}

impl fmt::Debug for CsrfProtector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfProtector([REDACTED])")
    }
}

impl CsrfProtector {
    /// Derives the CSRF key from the 32-byte master key (`HOS_SECRET_KEY`).
    pub fn new(master_key: &[u8; 32]) -> Self {
        Self {
            // Pre-rename "hf" label, kept on purpose: KDF label of the CSRF key.
            key: hmac_sha256(master_key, b"hf-auth key derivation: csrf v1"),
        }
    }

    fn message(nonce: &[u8], binding: &str) -> Vec<u8> {
        let mut msg = Vec::with_capacity(DOMAIN.len() + nonce.len() + binding.len());
        msg.extend_from_slice(DOMAIN);
        msg.extend_from_slice(nonce);
        msg.extend_from_slice(binding.as_bytes());
        msg
    }

    /// New token bound to `binding` (e.g. the session `token_hash`).
    pub fn generate(&self, binding: &str) -> String {
        let nonce: [u8; NONCE_LEN] = random_bytes();
        let mac = hmac_sha256(&self.key, &Self::message(&nonce, binding));
        let mut raw = Vec::with_capacity(NONCE_LEN + MAC_LEN);
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(&mac);
        URL_SAFE_NO_PAD.encode(raw)
    }

    /// Constant-time verification of `token` for `binding`. An empty binding
    /// never verifies.
    pub fn verify(&self, binding: &str, token: &str) -> bool {
        if binding.is_empty() || token.len() > 128 {
            return false;
        }
        let Ok(raw) = URL_SAFE_NO_PAD.decode(token.trim()) else {
            return false;
        };
        if raw.len() != NONCE_LEN + MAC_LEN {
            return false;
        }
        let (nonce, mac) = raw.split_at(NONCE_LEN);
        verify_hmac_sha256(&self.key, &Self::message(nonce, binding), mac)
    }
}

/// Whether `method` is safe (no state change; no CSRF check needed).
pub fn is_safe_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS" | "TRACE"
    )
}

/// Picks the presented token: the header wins over the form field; blank
/// values are ignored.
pub fn extract_token<'a>(header: Option<&'a str>, form_field: Option<&'a str>) -> Option<&'a str> {
    header
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| form_field.map(str::trim).filter(|s| !s.is_empty()))
}

/// Whether an `Origin` (or `Referer`) header value has the same origin
/// (scheme, host, port) as `expected` — the request's own base URL. `"null"`
/// and unparsable values are rejected.
pub fn is_same_origin(origin_or_referer: &str, expected: &Url) -> bool {
    Url::parse(origin_or_referer.trim())
        .map(|u| u.origin() == expected.origin())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protector() -> CsrfProtector {
        CsrfProtector::new(&[42u8; 32])
    }

    #[test]
    fn roundtrip_and_binding() {
        let p = protector();
        let t = p.generate("session-hash-a");
        assert_eq!(t.len(), 64);
        assert!(p.verify("session-hash-a", &t));
        assert!(!p.verify("session-hash-b", &t));
        assert!(!p.verify("", &t));
        // Fresh nonce each time.
        let t2 = p.generate("session-hash-a");
        assert_ne!(t, t2);
        assert!(p.verify("session-hash-a", &t2));
        // Other key.
        assert!(!CsrfProtector::new(&[1u8; 32]).verify("session-hash-a", &t));
    }

    #[test]
    fn rejects_garbage_and_tampering() {
        let p = protector();
        let t = p.generate("s");
        assert!(!p.verify("s", ""));
        assert!(!p.verify("s", "not base64!"));
        assert!(!p.verify("s", &t[..60]));
        let mut raw = URL_SAFE_NO_PAD.decode(&t).unwrap();
        raw[0] ^= 1;
        assert!(!p.verify("s", &URL_SAFE_NO_PAD.encode(&raw)));
        assert!(!p.verify("s", &"A".repeat(500)));
        assert_eq!(format!("{p:?}"), "CsrfProtector([REDACTED])");
    }

    #[test]
    fn helpers() {
        assert!(is_safe_method("get"));
        assert!(is_safe_method("HEAD"));
        assert!(!is_safe_method("POST"));
        assert!(!is_safe_method("DELETE"));
        assert_eq!(extract_token(Some("h"), Some("f")), Some("h"));
        assert_eq!(extract_token(Some("  "), Some("f")), Some("f"));
        assert_eq!(extract_token(None, None), None);
        let base = Url::parse("https://acme.example.com/admin").unwrap();
        assert!(is_same_origin("https://acme.example.com", &base));
        assert!(is_same_origin(
            "https://acme.example.com/client/x?y=1",
            &base
        ));
        assert!(!is_same_origin("https://evil.example.com", &base));
        assert!(!is_same_origin("http://acme.example.com", &base));
        assert!(!is_same_origin("https://acme.example.com:8443", &base));
        assert!(!is_same_origin("null", &base));
    }
}
