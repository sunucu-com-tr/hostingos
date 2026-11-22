//! WHMCS field decryption and the sealing of a run's source credentials.
//!
//! ## WHMCS `decrypt()`
//!
//! WHMCS stores hosting account passwords (`tblhosting.password`), server
//! passwords (`tblservers.password`) and registrar / gateway settings
//! encrypted with its PHP `encrypt()` function, keyed by
//! `$cc_encryption_hash` from `configuration.php`. The algorithm is not
//! documented by WHMCS; it is the one published in community decoders (e.g.
//! <https://gist.github.com/Wruczek/3becc94ce602df70467813dbb8e907ef>,
//! <https://3v4l.org/PGNn4>):
//!
//! ```text
//! key      = md5(md5(hash)) . md5(hash)            (hex strings, 64 chars)
//! hash_key = sha1_raw(key)                          (20 bytes)
//! data     = base64_decode(value)
//! iv       = data[0..20] XOR hash_key
//! k        = iv
//! for c in 0..len(data[20..]):
//!     if c != 0 and c % 20 == 0: k = sha1_raw(k . plain[c-20 .. c])
//!     plain[c] = k[c % 20] XOR data[20 + c]
//! ```
//!
//! **Verification.** The tests check it against the sample of that gist
//! (ciphertext `JATa2iUqVdzCkBP5RiyitlQlUiACl8UrpJOeGUJO` with hash
//! `SOmECRAZYLONGHASHROFLCOPTERBBQKTHX` decrypts to `MyPassword` — a
//! readable plaintext, which a wrong algorithm would not produce) and
//! against vectors produced by an independent Python port of the PHP
//! reference (multi-block and empty values). It has **not** been checked
//! against a live WHMCS 8.x database: the output is therefore validated
//! ([`looks_like_secret`]) and a value that does not decrypt to printable
//! UTF-8 is reported and left out instead of being stored as garbage.
//! Card data (`tblcreditcards` / `tblpaymethods`, AES with a per-client key)
//! is never decrypted or imported.

use hos_core::CoreError;
use hos_core::crypto::SecretBox;
use hos_core::ids::TenantId;
use md5::{Digest as _, Md5};
use sha1::Sha1;
use uuid::Uuid;

use crate::config::SourceSecrets;

fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data).as_slice())
}

fn sha1_raw(data: &[u8]) -> [u8; 20] {
    let d = Sha1::digest(data);
    let mut out = [0u8; 20];
    out.copy_from_slice(d.as_slice());
    out
}

/// Why a value did not decrypt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecryptError {
    #[error("value is not base64")]
    NotBase64,
    #[error("value is shorter than the IV")]
    TooShort,
    #[error(
        "decrypted value is not printable text (wrong cc_encryption_hash or unsupported format)"
    )]
    Garbage,
}

/// Raw WHMCS `decrypt()` (no output validation).
pub fn whmcs_decrypt_raw(value: &str, cc_encryption_hash: &str) -> Result<Vec<u8>, DecryptError> {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|_| DecryptError::NotBase64)?;
    let h = md5_hex(cc_encryption_hash.as_bytes());
    let key = format!("{}{h}", md5_hex(h.as_bytes()));
    let hash_key = sha1_raw(key.as_bytes());
    let hl = hash_key.len();
    if data.len() < hl {
        return Err(DecryptError::TooShort);
    }
    let (iv_enc, body) = data.split_at(hl);
    let mut k: Vec<u8> = iv_enc
        .iter()
        .zip(hash_key.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    let mut out: Vec<u8> = Vec::with_capacity(body.len());
    for (c, byte) in body.iter().enumerate() {
        if c != 0 && c % hl == 0 {
            let mut chained = k.clone();
            chained.extend_from_slice(&out[c - hl..c]);
            k = sha1_raw(&chained).to_vec();
        }
        out.push(k[c % hl] ^ byte);
    }
    Ok(out)
}

/// Whether decrypted bytes look like a credential: UTF-8 without control
/// characters (tabs / newlines allowed for multi-line keys).
pub fn looks_like_secret(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|s| {
        s.chars()
            .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
    })
}

/// Decrypts a WHMCS-encrypted field. Empty input → `Ok(None)`.
pub fn whmcs_decrypt(
    value: &str,
    cc_encryption_hash: &str,
) -> Result<Option<String>, DecryptError> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let raw = whmcs_decrypt_raw(value, cc_encryption_hash)?;
    if !looks_like_secret(&raw) {
        return Err(DecryptError::Garbage);
    }
    let s = String::from_utf8(raw).map_err(|_| DecryptError::Garbage)?;
    Ok((!s.is_empty()).then_some(s))
}

/// AAD binding a run's sealed source secrets to the tenant and run.
pub fn secrets_aad(tenant_id: TenantId, run_id: Uuid) -> Vec<u8> {
    let mut out = b"hos:import-source:".to_vec();
    out.extend_from_slice(tenant_id.as_uuid().as_bytes());
    out.extend_from_slice(run_id.as_bytes());
    out
}

/// Seals source secrets for `import_runs.secrets_enc`.
pub fn seal_secrets(
    secret_box: &SecretBox,
    tenant_id: TenantId,
    run_id: Uuid,
    secrets: &SourceSecrets,
) -> Result<Vec<u8>, CoreError> {
    Ok(secret_box.encrypt_json(secrets, &secrets_aad(tenant_id, run_id))?)
}

/// Opens sealed source secrets.
pub fn open_secrets(
    secret_box: &SecretBox,
    tenant_id: TenantId,
    run_id: Uuid,
    sealed: &[u8],
) -> Result<SourceSecrets, CoreError> {
    Ok(secret_box.decrypt_json(sealed, &secrets_aad(tenant_id, run_id))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "SOmECRAZYLONGHASHROFLCOPTERBBQKTHX";

    #[test]
    fn published_sample_decrypts() {
        assert_eq!(
            whmcs_decrypt("JATa2iUqVdzCkBP5RiyitlQlUiACl8UrpJOeGUJO", HASH).unwrap(),
            Some("MyPassword".to_owned())
        );
    }

    #[test]
    fn independent_port_vectors() {
        // Produced by a Python port of the PHP reference with IV = 01..14.
        assert_eq!(
            whmcs_decrypt("auhMlPfMu6L7sJbH/wWHtwWi+4Rpd21wYHQ1", HASH).unwrap(),
            Some("hunter2".to_owned())
        );
        // 45 bytes: exercises the per-20-byte key chaining.
        assert_eq!(
            whmcs_decrypt(
                "auhMlPfMu6L7sJbH/wWHtwWi+4RSMXFyYHQqWGh5eC16YX10PGZ7dXQnTaV/IbU6jr9kOswjmNwxBiQQPPU4K24=",
                HASH
            )
            .unwrap(),
            Some("S3rver-Pass!word-that-is-longer-than-20-bytes".to_owned())
        );
        assert_eq!(
            whmcs_decrypt("auhMlPfMu6L7sJbH/wWHtwWi+4Q=", HASH).unwrap(),
            None
        );
        assert_eq!(whmcs_decrypt("  ", HASH).unwrap(), None);
    }

    #[test]
    fn wrong_key_and_bad_input_are_rejected() {
        assert_eq!(
            whmcs_decrypt(
                "auhMlPfMu6L7sJbH/wWHtwWi+4RSMXFyYHQqWGh5eC16YX10PGZ7dXQnTaV/IbU6jr9kOswjmNwxBiQQPPU4K24=",
                "another-hash"
            ),
            Err(DecryptError::Garbage)
        );
        assert_eq!(whmcs_decrypt("%%%", HASH), Err(DecryptError::NotBase64));
        assert_eq!(whmcs_decrypt("YWJj", HASH), Err(DecryptError::TooShort));
        assert!(looks_like_secret(b"line1\nline2"));
        assert!(!looks_like_secret(&[0x00, 0x41]));
        assert!(!looks_like_secret(&[0xff, 0xfe]));
    }

    #[test]
    fn sealed_secrets_round_trip_and_bind() {
        let b = SecretBox::new(&[9u8; 32]);
        let t = TenantId::new();
        let run = Uuid::now_v7();
        let s = SourceSecrets {
            db_password: Some("pw".into()),
            ..SourceSecrets::default()
        };
        let sealed = seal_secrets(&b, t, run, &s).unwrap();
        assert_eq!(
            open_secrets(&b, t, run, &sealed)
                .unwrap()
                .db_password
                .as_deref(),
            Some("pw")
        );
        assert!(open_secrets(&b, t, Uuid::now_v7(), &sealed).is_err());
        assert!(open_secrets(&b, TenantId::new(), run, &sealed).is_err());
    }
}
