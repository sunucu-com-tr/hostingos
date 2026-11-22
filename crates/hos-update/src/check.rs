//! One update check: discover candidate releases, download each manifest
//! and its signature, verify them (in version order, so a key announced by
//! an older release can verify a newer one) and report what was found.
//!
//! The caller (hos-web) persists the report: available versions, endorsed /
//! revoked keys, the replay high-water marks, notifications.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::error::{UpdateError, UpdateResult};
use crate::fetch::Fetcher;
use crate::keys::{KeyOrigin, KeyRing, MAX_SIGNATURE_BYTES, TrustedKey};
use crate::manifest::{Channel, MAX_MANIFEST_BYTES, Manifest};
use crate::source::{Candidate, Source, discover};
use crate::verify::{VerifyPolicy, check_offer, verify_signed};

/// Releases looked at per check.
pub const MAX_CANDIDATES: usize = 5;

/// Input of [`run_check`].
#[derive(Debug, Clone)]
pub struct CheckRequest {
    /// Release source.
    pub source: Source,
    /// Channel.
    pub channel: Channel,
    /// Running version.
    pub current: semver::Version,
    /// Built-in + previously endorsed keys.
    pub ring: KeyRing,
    /// `published_at` already accepted per version (replay protection).
    pub seen: HashMap<String, DateTime<Utc>>,
    /// Now.
    pub now: DateTime<Utc>,
}

/// Trust status of a found release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Signature verified by `key_id`.
    Verified {
        /// Signing key.
        key_id: String,
    },
    /// No trusted key is configured: announced only, never installable.
    Unverified,
    /// Signature / replay check failed: shown as a warning, contents ignored.
    Rejected {
        /// Why.
        reason: String,
    },
    /// The manifest could not be downloaded.
    Unavailable {
        /// Why.
        reason: String,
    },
}

impl Trust {
    /// `verified` / `unverified` / `rejected` / `unavailable`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified { .. } => "verified",
            Self::Unverified => "unverified",
            Self::Rejected { .. } => "rejected",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

/// A release newer than the running binary.
#[derive(Debug, Clone)]
pub struct Found {
    /// Version (from the manifest, else the tag).
    pub version: String,
    /// Trust status.
    pub trust: Trust,
    /// The manifest (absent when rejected / unavailable).
    pub manifest: Option<Manifest>,
    /// Exact manifest text (absent when rejected / unavailable).
    pub manifest_raw: Option<String>,
    /// Signature text, when downloaded.
    pub signature: Option<String>,
    /// Why it cannot be installed directly (e.g. `min_upgradable_from`).
    pub blocked: Option<String>,
    /// Release page.
    pub release_url: Option<String>,
}

/// Result of [`run_check`].
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    /// Newer releases, newest first.
    pub found: Vec<Found>,
    /// Keys endorsed by verified manifests in this check.
    pub endorsed: Vec<TrustedKey>,
    /// Endorsed key ids revoked by verified manifests.
    pub revoked: Vec<String>,
    /// Whether the ring was empty (announce-only mode).
    pub unsigned_mode: bool,
}

impl CheckReport {
    /// The newest release that verified and is installable.
    pub fn best(&self) -> Option<&Found> {
        self.found
            .iter()
            .find(|f| matches!(f.trust, Trust::Verified { .. }) && f.blocked.is_none())
    }
}

async fn download(fetcher: &Fetcher, c: &Candidate) -> UpdateResult<(Vec<u8>, String)> {
    let raw = fetcher
        .get_bytes(
            &c.manifest_url,
            "application/octet-stream",
            MAX_MANIFEST_BYTES,
        )
        .await?;
    let sig = fetcher
        .get_text(
            &c.signature_url,
            "application/octet-stream",
            MAX_SIGNATURE_BYTES,
        )
        .await?;
    Ok((raw, sig))
}

/// Runs one check (see the module docs). Network errors of the release
/// listing fail the whole check; a single unreachable manifest is reported
/// as [`Trust::Unavailable`].
pub async fn run_check(fetcher: &Fetcher, req: &CheckRequest) -> UpdateResult<CheckReport> {
    let mut candidates = discover(
        fetcher,
        &req.source,
        req.channel,
        &req.current,
        MAX_CANDIDATES,
    )
    .await?;
    // Oldest first: endorsements of older releases verify newer ones.
    candidates.reverse();
    let mut ring = req.ring.clone();
    let mut report = CheckReport {
        unsigned_mode: ring.is_empty(),
        ..CheckReport::default()
    };
    for c in &candidates {
        let tag_version = c.version.as_ref().map(ToString::to_string);
        let fallback = tag_version.clone().unwrap_or_else(|| "?".into());
        let (raw, sig) = match download(fetcher, c).await {
            Ok(v) => v,
            Err(e) => {
                report.found.push(Found {
                    version: fallback,
                    trust: Trust::Unavailable {
                        reason: e.to_string(),
                    },
                    manifest: None,
                    manifest_raw: None,
                    signature: None,
                    blocked: None,
                    release_url: c.html_url.clone(),
                });
                continue;
            }
        };
        let rejected = |reason: String| Found {
            version: fallback.clone(),
            trust: Trust::Rejected { reason },
            manifest: None,
            manifest_raw: None,
            signature: Some(sig.clone()),
            blocked: None,
            release_url: c.html_url.clone(),
        };
        let (manifest, trust) = if report.unsigned_mode {
            match Manifest::parse(&raw) {
                Ok(m) => (m, Trust::Unverified),
                Err(e) => {
                    report.found.push(rejected(e.to_string()));
                    continue;
                }
            }
        } else {
            match verify_signed(&raw, &sig, &ring) {
                Ok(v) => (v.manifest, Trust::Verified { key_id: v.key_id }),
                Err(e) => {
                    report.found.push(rejected(e.to_string()));
                    continue;
                }
            }
        };
        if let Some(tag) = &tag_version
            && *tag != manifest.version
        {
            report.found.push(rejected(format!(
                "release tag {tag} carries a manifest for {}",
                manifest.version
            )));
            continue;
        }
        let policy = VerifyPolicy {
            current: req.current.clone(),
            channel: req.channel,
            now: req.now,
            seen_published_at: req.seen.get(&manifest.version).copied(),
        };
        let blocked = match check_offer(&manifest, &policy) {
            Ok(()) => None,
            Err(UpdateError::NotNewer { .. } | UpdateError::Manifest(_)) => continue,
            Err(e @ UpdateError::NotUpgradable { .. }) => Some(e.to_string()),
            Err(e) => {
                report.found.push(rejected(e.to_string()));
                continue;
            }
        };
        if let Trust::Verified { key_id } = &trust {
            for k in &manifest.next_keys {
                let origin = KeyOrigin::Endorsed {
                    by: key_id.clone(),
                    version: manifest.version.clone(),
                };
                if let Ok(key) = TrustedKey::parse(k, "announced", origin)
                    && ring.add(key.clone())
                {
                    report.endorsed.push(key);
                }
            }
            for revoked in &manifest.revoked_keys {
                if revoked != key_id && ring.revoke(revoked) {
                    report.revoked.push(revoked.clone());
                }
            }
        }
        report.found.push(Found {
            version: manifest.version.clone(),
            trust,
            manifest_raw: Some(String::from_utf8_lossy(&raw).into_owned()),
            signature: Some(sig.clone()),
            blocked,
            release_url: c.html_url.clone(),
            manifest: Some(manifest),
        });
    }
    report.found.sort_by(|a, b| {
        let va = semver::Version::parse(&a.version).ok();
        let vb = semver::Version::parse(&b.version).ok();
        vb.cmp(&va)
    });
    Ok(report)
}
