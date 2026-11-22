//! Trusted release signing keys and signature checks.
//!
//! Releases are signed with [minisign](https://jedisct1.github.io/minisign/)
//! (Ed25519, pre-hashed mode). The verifier is the dependency-free
//! `minisign-verify` crate; nothing here implements cryptography.
//!
//! The key ring is
//!
//! 1. the keys compiled in from `crates/hos-update/keys/trusted.pub`
//!    ([`KeyOrigin::BuiltIn`]), plus
//! 2. keys **endorsed** by a manifest that verified against the ring
//!    (`next_keys`, [`KeyOrigin::Endorsed`]) — key rotation: release N, signed
//!    with the old key, announces the new key; release N+1 may be signed with
//!    the new key and installations built before the rotation still accept
//!    it. Endorsements are persisted by the caller (table `update_trusted_keys`)
//!    and fed back with [`KeyRing::add`].
//!
//! A signature names its key (8-byte key id); a signature by a key that is
//! not in the ring is refused. Legacy (non pre-hashed) signatures are refused.

use std::io::Read as _;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::error::{UpdateError, UpdateResult};

/// The compiled-in key file (may contain no key).
pub const BUILT_IN_KEYS: &str = include_str!("../keys/trusted.pub");

/// Largest accepted signature file.
pub const MAX_SIGNATURE_BYTES: usize = 4096;

/// Where a trusted key comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyOrigin {
    /// `keys/trusted.pub`, compiled into this binary.
    BuiltIn,
    /// Announced in `next_keys` of a verified manifest.
    Endorsed {
        /// Key id that signed the announcing manifest.
        by: String,
        /// Version of the announcing manifest.
        version: String,
    },
}

/// A minisign public key the updater trusts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedKey {
    /// Key id as minisign prints it (16 upper-case hex digits).
    pub key_id: String,
    /// The base64 public key (56 characters, `RW…`).
    pub public_key: String,
    /// Free-form label.
    pub label: String,
    /// Where the key comes from.
    pub origin: KeyOrigin,
}

impl TrustedKey {
    /// Parses a base64 minisign public key.
    pub fn parse(public_key: &str, label: &str, origin: KeyOrigin) -> UpdateResult<Self> {
        let public_key = public_key.trim().to_owned();
        let key_id = key_id_of(&public_key)?;
        minisign_verify::PublicKey::from_base64(&public_key)
            .map_err(|e| UpdateError::Signature(format!("invalid public key: {e}")))?;
        Ok(Self {
            key_id,
            public_key,
            label: label.trim().to_owned(),
            origin,
        })
    }
}

/// Key id of a base64 minisign public key (`Ed` + 8-byte id + 32-byte key).
pub fn key_id_of(public_key: &str) -> UpdateResult<String> {
    let raw = STANDARD
        .decode(public_key.trim())
        .map_err(|_| UpdateError::Signature("public key is not base64".into()))?;
    if raw.len() != 42 || &raw[..2] != b"Ed" {
        return Err(UpdateError::Signature(
            "not an Ed25519 minisign public key".into(),
        ));
    }
    Ok(format_key_id(&raw[2..10]))
}

fn format_key_id(bytes: &[u8]) -> String {
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[..8]);
    format!("{:016X}", u64::from_le_bytes(id))
}

/// Key id and mode of a minisign signature file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    /// Id of the key that made the signature.
    pub key_id: String,
    /// Pre-hashed (BLAKE2b) mode — the only mode accepted.
    pub prehashed: bool,
}

/// Reads the key id out of a signature without verifying it.
pub fn signature_info(signature: &str) -> UpdateResult<SignatureInfo> {
    if signature.len() > MAX_SIGNATURE_BYTES {
        return Err(UpdateError::Signature("signature file too large".into()));
    }
    let mut lines = signature.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or_default();
    if !first.starts_with("untrusted comment:") {
        return Err(UpdateError::Signature("not a minisign signature".into()));
    }
    let raw = STANDARD
        .decode(lines.next().unwrap_or_default())
        .map_err(|_| UpdateError::Signature("malformed signature".into()))?;
    if raw.len() != 74 {
        return Err(UpdateError::Signature("malformed signature".into()));
    }
    let prehashed = match &raw[..2] {
        b"ED" => true,
        b"Ed" => false,
        _ => {
            return Err(UpdateError::Signature(
                "unsupported signature algorithm".into(),
            ));
        }
    };
    Ok(SignatureInfo {
        key_id: format_key_id(&raw[2..10]),
        prehashed,
    })
}

/// A successful verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// Key that made the signature.
    pub key_id: String,
    /// The (authenticated) trusted comment.
    pub trusted_comment: String,
}

/// The set of trusted keys.
#[derive(Debug, Clone, Default)]
pub struct KeyRing {
    keys: Vec<TrustedKey>,
}

impl KeyRing {
    /// No keys.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The keys compiled into this binary.
    pub fn built_in() -> UpdateResult<Self> {
        Self::parse(BUILT_IN_KEYS, &KeyOrigin::BuiltIn)
    }

    /// Parses a key file (`<base64> [label]` per line, `#` comments).
    pub fn parse(text: &str, origin: &KeyOrigin) -> UpdateResult<Self> {
        let mut ring = Self::empty();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, label) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            ring.add(TrustedKey::parse(key, label, origin.clone())?);
        }
        Ok(ring)
    }

    /// Adds a key (ignored when its id is already present). Returns whether
    /// it was new.
    pub fn add(&mut self, key: TrustedKey) -> bool {
        if self.contains(&key.key_id) {
            return false;
        }
        self.keys.push(key);
        true
    }

    /// Removes a key by id (revocation). Built-in keys cannot be removed:
    /// revoking one needs a new build.
    pub fn revoke(&mut self, key_id: &str) -> bool {
        let before = self.keys.len();
        self.keys
            .retain(|k| k.key_id != key_id || k.origin == KeyOrigin::BuiltIn);
        before != self.keys.len()
    }

    /// Whether the ring holds no key at all.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Number of keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether `key_id` is trusted.
    pub fn contains(&self, key_id: &str) -> bool {
        self.keys.iter().any(|k| k.key_id == key_id)
    }

    /// The keys.
    pub fn keys(&self) -> &[TrustedKey] {
        &self.keys
    }

    fn key_for(&self, signature: &str) -> UpdateResult<(&TrustedKey, minisign_verify::Signature)> {
        if self.is_empty() {
            return Err(UpdateError::NoTrustedKeys);
        }
        let info = signature_info(signature)?;
        if !info.prehashed {
            return Err(UpdateError::Signature(
                "legacy (non pre-hashed) signatures are not accepted".into(),
            ));
        }
        let key = self
            .keys
            .iter()
            .find(|k| k.key_id == info.key_id)
            .ok_or_else(|| {
                UpdateError::Signature(format!("signed by unknown key {}", info.key_id))
            })?;
        let sig = minisign_verify::Signature::decode(signature)
            .map_err(|e| UpdateError::Signature(format!("malformed signature: {e}")))?;
        Ok((key, sig))
    }

    /// Verifies `signature` over `data` (signature and trusted comment).
    pub fn verify(&self, data: &[u8], signature: &str) -> UpdateResult<Verified> {
        let (key, sig) = self.key_for(signature)?;
        let pk = minisign_verify::PublicKey::from_base64(&key.public_key)
            .map_err(|e| UpdateError::Signature(format!("invalid public key: {e}")))?;
        pk.verify(data, &sig, false)
            .map_err(|e| UpdateError::Signature(e.to_string()))?;
        Ok(Verified {
            key_id: key.key_id.clone(),
            trusted_comment: sig.trusted_comment().to_owned(),
        })
    }

    /// Verifies `signature` over a file, streaming.
    pub fn verify_file(&self, path: &Path, signature: &str) -> UpdateResult<Verified> {
        let (key, sig) = self.key_for(signature)?;
        let pk = minisign_verify::PublicKey::from_base64(&key.public_key)
            .map_err(|e| UpdateError::Signature(format!("invalid public key: {e}")))?;
        let mut verifier = pk
            .verify_stream(&sig)
            .map_err(|e| UpdateError::Signature(e.to_string()))?;
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            verifier.update(&buf[..n]);
        }
        verifier
            .finalize()
            .map_err(|e| UpdateError::Signature(e.to_string()))?;
        Ok(Verified {
            key_id: key.key_id.clone(),
            trusted_comment: sig.trusted_comment().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_key_file_parses() {
        // The shipped file may be empty, but it must always parse.
        let ring = KeyRing::built_in().expect("keys/trusted.pub parses");
        for k in ring.keys() {
            assert_eq!(k.key_id.len(), 16);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(key_id_of("not base64!").is_err());
        assert!(key_id_of("AAAA").is_err());
        assert!(signature_info("hello").is_err());
        assert!(KeyRing::parse("RWbogus", &KeyOrigin::BuiltIn).is_err());
        assert!(matches!(
            KeyRing::empty().verify(b"x", "untrusted comment: x\nAAAA"),
            Err(UpdateError::NoTrustedKeys)
        ));
    }
}
