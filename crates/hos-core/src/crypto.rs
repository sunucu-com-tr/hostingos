//! Cryptographic helpers.
//!
//! * [`SecretBox`] — AES-256-GCM encryption of integration secrets
//!   (`integrations.secrets_enc`), keyed by `HOS_SECRET_KEY`.
//! * Tokens — [`generate_token`] + [`hash_token`] for sessions, API keys, password
//!   reset / email verification links (store only the hash).
//! * [`constant_time_eq`], [`sha256`], HMAC-SHA256 sign/verify (webhook signatures:
//!   Stripe hex, PayTR/iyzico base64).
//!
//! ## Ciphertext format
//!
//! `[version: 1 byte = 0x01][nonce: 12 random bytes][AES-256-GCM ciphertext || 16-byte tag]`
//!
//! The version byte allows key rotation / algorithm changes later without
//! guessing. Associated data (AAD) is authenticated but not stored; the same
//! AAD must be supplied to decrypt. For `integrations.secrets_enc` use
//! [`integration_aad`] so a ciphertext copied to another row/tenant fails to
//! decrypt.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::ids::{IntegrationId, TenantId};

/// Current ciphertext format version (first byte of every [`SecretBox`] output).
pub const SECRET_BOX_VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Errors from [`SecretBox`] and encoding helpers. Never contain key material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// Key has the wrong length / encoding.
    #[error("invalid key: {0}")]
    InvalidKey(String),
    /// Encryption failed (should not happen with valid inputs).
    #[error("encryption failed")]
    Encrypt,
    /// Authentication failed: wrong key, wrong AAD, or tampered/truncated data.
    #[error("decryption failed (wrong key, wrong associated data, or corrupted data)")]
    Decrypt,
    /// Ciphertext has an unknown version byte or is too short.
    #[error("unsupported or malformed ciphertext")]
    Malformed,
    /// JSON (de)serialization of the plaintext failed.
    #[error("serialization failed: {0}")]
    Serialization(String),
}

/// AES-256-GCM sealing box. Cheap to clone; `Debug` never prints the key.
#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBox([REDACTED])")
    }
}

impl SecretBox {
    /// Creates a box from a 32-byte key.
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(&(*key).into()),
        }
    }

    /// Creates a box from a base64-encoded 32-byte key (the `HOS_SECRET_KEY` format).
    pub fn from_base64(encoded: &str) -> Result<Self, CryptoError> {
        let key = crate::config::decode_secret_key(encoded).map_err(CryptoError::InvalidKey)?;
        Ok(Self::new(&key))
    }

    /// Encrypts `plaintext`, binding it to `aad` (may be empty).
    pub fn encrypt(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce_bytes: [u8; NONCE_LEN] = random_bytes();
        let nonce = Nonce::from(nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::Encrypt)?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
        out.push(SECRET_BOX_VERSION);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypts data produced by [`SecretBox::encrypt`] with the same `aad`.
    pub fn decrypt(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (&version, rest) = sealed.split_first().ok_or(CryptoError::Malformed)?;
        if version != SECRET_BOX_VERSION || rest.len() < NONCE_LEN + TAG_LEN {
            return Err(CryptoError::Malformed);
        }
        let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);
        let nonce_arr: [u8; NONCE_LEN] =
            nonce_bytes.try_into().map_err(|_| CryptoError::Malformed)?;
        self.cipher
            .decrypt(
                &Nonce::from(nonce_arr),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt)
    }

    /// Serializes `value` to JSON and encrypts it.
    pub fn encrypt_json<T: Serialize + ?Sized>(
        &self,
        value: &T,
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let json =
            serde_json::to_vec(value).map_err(|e| CryptoError::Serialization(e.to_string()))?;
        self.encrypt(&json, aad)
    }

    /// Decrypts and deserializes JSON produced by [`SecretBox::encrypt_json`].
    pub fn decrypt_json<T: DeserializeOwned>(
        &self,
        sealed: &[u8],
        aad: &[u8],
    ) -> Result<T, CryptoError> {
        let plain = self.decrypt(sealed, aad)?;
        serde_json::from_slice(&plain).map_err(|e| CryptoError::Serialization(e.to_string()))
    }
}

/// Canonical AAD for `integrations.secrets_enc`: `tenant_id || integration_id`
/// (32 bytes). Binds the ciphertext to its row.
pub fn integration_aad(tenant_id: TenantId, integration_id: IntegrationId) -> [u8; 32] {
    let mut aad = [0u8; 32];
    aad[..16].copy_from_slice(tenant_id.as_uuid().as_bytes());
    aad[16..].copy_from_slice(integration_id.as_uuid().as_bytes());
    aad
}

/// Generates a fresh random 32-byte key, base64 encoded (suitable for `HOS_SECRET_KEY`).
pub fn generate_secret_key_base64() -> String {
    STANDARD.encode(random_bytes::<32>())
}

/// `N` cryptographically secure random bytes (thread-local CSPRNG seeded from the OS).
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::fill(&mut buf[..]);
    buf
}

/// Constant-time byte comparison (lengths are compared, not hidden).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Default token size in random bytes (256 bits).
pub const TOKEN_BYTES: usize = 32;

/// A URL-safe random token (base64url, no padding) with [`TOKEN_BYTES`] of entropy
/// (43 characters). Give it to the user; store only [`hash_token`].
pub fn generate_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<TOKEN_BYTES>())
}

/// Like [`generate_token`] with a readable prefix, e.g. `hos_live_…` for API keys.
pub fn generate_prefixed_token(prefix: &str) -> String {
    format!("{prefix}_{}", generate_token())
}

/// SHA-256 of a token as lower-case hex — what goes into the database.
pub fn hash_token(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// Checks `token` against a stored [`hash_token`] value in constant time.
pub fn verify_token_hash(token: &str, stored_hex: &str) -> bool {
    constant_time_eq(
        hash_token(token).as_bytes(),
        stored_hex.trim().to_ascii_lowercase().as_bytes(),
    )
}

/// SHA-256 digest.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// SHA-256 digest as lower-case hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8]) -> HmacSha256 {
    // Provably infallible: HMAC pads/hashes keys of any length (RFC 2104 §2).
    #[allow(clippy::expect_used)]
    <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts keys of any length")
}

/// HMAC-SHA256 of `message` under `key`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = hmac(key);
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// HMAC-SHA256 as lower-case hex (Stripe-style signatures).
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    hex::encode(hmac_sha256(key, message))
}

/// HMAC-SHA256 as standard base64 (PayTR / iyzico style signatures).
pub fn hmac_sha256_base64(key: &[u8], message: &[u8]) -> String {
    STANDARD.encode(hmac_sha256(key, message))
}

/// Verifies a raw HMAC-SHA256 tag in constant time.
pub fn verify_hmac_sha256(key: &[u8], message: &[u8], tag: &[u8]) -> bool {
    let mut mac = hmac(key);
    mac.update(message);
    mac.verify_slice(tag).is_ok()
}

/// Verifies a hex HMAC-SHA256 signature (case-insensitive) in constant time.
pub fn verify_hmac_sha256_hex(key: &[u8], message: &[u8], signature_hex: &str) -> bool {
    hex::decode(signature_hex.trim())
        .map(|tag| verify_hmac_sha256(key, message, &tag))
        .unwrap_or(false)
}

/// Verifies a standard-base64 HMAC-SHA256 signature in constant time.
pub fn verify_hmac_sha256_base64(key: &[u8], message: &[u8], signature_b64: &str) -> bool {
    STANDARD
        .decode(signature_b64.trim())
        .map(|tag| verify_hmac_sha256(key, message, &tag))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap();
        }
        k
    }

    #[test]
    fn secret_box_roundtrip() {
        let sb = SecretBox::new(&key());
        let sealed = sb.encrypt(b"hello", b"aad").unwrap();
        assert_eq!(sealed[0], SECRET_BOX_VERSION);
        assert_eq!(sealed.len(), 1 + 12 + 5 + 16);
        assert_eq!(sb.decrypt(&sealed, b"aad").unwrap(), b"hello");
        // Random nonce: same plaintext encrypts differently.
        assert_ne!(sealed, sb.encrypt(b"hello", b"aad").unwrap());
        // Empty plaintext works.
        let empty = sb.encrypt(b"", &[]).unwrap();
        assert_eq!(sb.decrypt(&empty, &[]).unwrap(), b"");
    }

    #[test]
    fn secret_box_json_roundtrip() {
        let sb = SecretBox::from_base64(&STANDARD.encode(key())).unwrap();
        let aad = integration_aad(TenantId::new(), IntegrationId::new());
        let value = json!({"api_token": "s3cr3t", "port": 8006});
        let sealed = sb.encrypt_json(&value, &aad).unwrap();
        let back: serde_json::Value = sb.decrypt_json(&sealed, &aad).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn secret_box_rejects_tampering() {
        let sb = SecretBox::new(&key());
        let mut sealed = sb.encrypt(b"payload", b"a").unwrap();
        assert_eq!(sb.decrypt(&sealed, b"b"), Err(CryptoError::Decrypt));
        let other = SecretBox::new(&[7u8; 32]);
        assert_eq!(other.decrypt(&sealed, b"a"), Err(CryptoError::Decrypt));
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert_eq!(sb.decrypt(&sealed, b"a"), Err(CryptoError::Decrypt));
        assert_eq!(sb.decrypt(&[], b"a"), Err(CryptoError::Malformed));
        assert_eq!(sb.decrypt(&[1, 2, 3], b"a"), Err(CryptoError::Malformed));
        let mut wrong_version = sb.encrypt(b"x", b"").unwrap();
        wrong_version[0] = 9;
        assert_eq!(sb.decrypt(&wrong_version, b""), Err(CryptoError::Malformed));
        assert!(SecretBox::from_base64("c2hvcnQ=").is_err());
        assert_eq!(format!("{sb:?}"), "SecretBox([REDACTED])");
    }

    #[test]
    fn generated_key_is_valid() {
        let k = generate_secret_key_base64();
        assert!(SecretBox::from_base64(&k).is_ok());
    }

    #[test]
    fn tokens() {
        let t = generate_token();
        assert_eq!(t.len(), 43);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(t, generate_token());
        let h = hash_token(&t);
        assert_eq!(h.len(), 64);
        assert!(verify_token_hash(&t, &h));
        assert!(verify_token_hash(&t, &h.to_ascii_uppercase()));
        assert!(!verify_token_hash("other", &h));
        assert!(generate_prefixed_token("hos_live").starts_with("hos_live_"));
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_rfc4231_case2() {
        let key = b"Jefe";
        let msg = b"what do ya want for nothing?";
        let expected = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        assert_eq!(hmac_sha256_hex(key, msg), expected);
        assert!(verify_hmac_sha256_hex(key, msg, expected));
        assert!(verify_hmac_sha256_hex(
            key,
            msg,
            &expected.to_ascii_uppercase()
        ));
        assert!(!verify_hmac_sha256_hex(key, b"tampered", expected));
        assert!(!verify_hmac_sha256_hex(key, msg, "zz"));
        let b64 = hmac_sha256_base64(key, msg);
        assert!(verify_hmac_sha256_base64(key, msg, &b64));
        assert!(!verify_hmac_sha256_base64(b"other", msg, &b64));
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
