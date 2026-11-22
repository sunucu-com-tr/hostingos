//! Manifest verification: signature first, then the offer policy.
//!
//! 1. [`verify_signed`]: size limit, minisign signature by a key of the ring
//!    (signature **and** trusted comment), structural validation, product.
//! 2. [`check_offer`]: channel, **no downgrade / re-install** (the version
//!    must be strictly newer than the running one), **no replay** (a
//!    manifest older than one already seen for the same version, or dated
//!    more than a day in the future, is refused) and `min_upgradable_from`.
//!
//! Artefacts are checked at install time against the verified manifest
//! (target triple, size, SHA-256, their own detached signature).

use chrono::{DateTime, Duration, Utc};

use crate::error::{UpdateError, UpdateResult};
use crate::keys::KeyRing;
use crate::manifest::{Channel, MAX_MANIFEST_BYTES, Manifest};

/// Clock skew tolerated for `published_at` in the future.
pub const MAX_FUTURE_SKEW: Duration = Duration::hours(24);

/// What a manifest is checked against.
#[derive(Debug, Clone)]
pub struct VerifyPolicy {
    /// Running version.
    pub current: semver::Version,
    /// Configured channel.
    pub channel: Channel,
    /// Now.
    pub now: DateTime<Utc>,
    /// `published_at` of the manifest already accepted for the same version,
    /// if any (replay protection).
    pub seen_published_at: Option<DateTime<Utc>>,
}

impl VerifyPolicy {
    /// Policy for the running binary on `channel`.
    pub fn new(current: semver::Version, channel: Channel) -> Self {
        Self {
            current,
            channel,
            now: Utc::now(),
            seen_published_at: None,
        }
    }
}

/// A manifest whose signature verified.
#[derive(Debug, Clone)]
pub struct VerifiedManifest {
    /// The parsed manifest.
    pub manifest: Manifest,
    /// The exact signed bytes.
    pub raw: Vec<u8>,
    /// The signature text.
    pub signature: String,
    /// Key that signed it.
    pub key_id: String,
}

/// Signature + structure. `NoTrustedKeys` when the ring is empty.
pub fn verify_signed(
    raw: &[u8],
    signature: &str,
    ring: &KeyRing,
) -> UpdateResult<VerifiedManifest> {
    if raw.len() > MAX_MANIFEST_BYTES {
        return Err(UpdateError::TooLarge {
            limit: MAX_MANIFEST_BYTES as u64,
        });
    }
    let verified = ring.verify(raw, signature)?;
    let manifest = Manifest::parse(raw)?;
    Ok(VerifiedManifest {
        manifest,
        raw: raw.to_vec(),
        signature: signature.to_owned(),
        key_id: verified.key_id,
    })
}

/// The offer policy (see the module docs).
pub fn check_offer(manifest: &Manifest, policy: &VerifyPolicy) -> UpdateResult<()> {
    if !policy.channel.accepts(manifest.channel) {
        return Err(UpdateError::Manifest(format!(
            "{} release on the {} channel",
            manifest.channel.as_str(),
            policy.channel.as_str()
        )));
    }
    let offered = manifest.semver()?;
    if offered <= policy.current {
        return Err(UpdateError::NotNewer {
            offered: offered.to_string(),
            current: policy.current.to_string(),
        });
    }
    if manifest.published_at > policy.now + MAX_FUTURE_SKEW {
        return Err(UpdateError::Replay(format!(
            "manifest of {offered} is dated in the future ({})",
            manifest.published_at.to_rfc3339()
        )));
    }
    if let Some(seen) = policy.seen_published_at
        && manifest.published_at < seen
    {
        return Err(UpdateError::Replay(format!(
            "manifest of {offered} is older ({}) than the one already seen ({})",
            manifest.published_at.to_rfc3339(),
            seen.to_rfc3339()
        )));
    }
    if let Some(min) = &manifest.min_upgradable_from {
        let min = semver::Version::parse(min)
            .map_err(|_| UpdateError::Manifest(format!("invalid min_upgradable_from {min:?}")))?;
        if policy.current < min {
            return Err(UpdateError::NotUpgradable {
                offered: offered.to_string(),
                min: min.to_string(),
                current: policy.current.to_string(),
            });
        }
    }
    Ok(())
}

/// [`verify_signed`] then [`check_offer`].
pub fn verify_manifest(
    raw: &[u8],
    signature: &str,
    ring: &KeyRing,
    policy: &VerifyPolicy,
) -> UpdateResult<VerifiedManifest> {
    let verified = verify_signed(raw, signature, ring)?;
    check_offer(&verified.manifest, policy)?;
    Ok(verified)
}
