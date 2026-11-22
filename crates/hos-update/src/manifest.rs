//! The release manifest (`hostingos-manifest.json`), published with every
//! release next to its minisign signature (`hostingos-manifest.json.minisig`).
//! The release workflow writes it (docs/RELEASING.md); the updater trusts
//! nothing in it before the signature verified ([`crate::verify`]).
//!
//! ```json
//! {
//!   "schema": 1,
//!   "product": "hostingos",
//!   "version": "0.2.0",
//!   "channel": "stable",
//!   "published_at": "2026-12-01T10:00:00Z",
//!   "severity": "regular",
//!   "min_upgradable_from": "0.1.0",
//!   "migrations": true,
//!   "rollback_safe": true,
//!   "notes_md": "### Added\n- …",
//!   "container_image": "ghcr.io/OWNER/REPO:0.2.0",
//!   "artifacts": [{
//!     "target": "x86_64-unknown-linux-musl",
//!     "name": "hostingos-0.2.0-x86_64-unknown-linux-musl.tar.gz",
//!     "url": "https://github.com/OWNER/REPO/releases/download/v0.2.0/hostingos-0.2.0-x86_64-unknown-linux-musl.tar.gz",
//!     "size": 12345678,
//!     "sha256": "…64 hex…"
//!   }],
//!   "next_keys": [],
//!   "revoked_keys": []
//! }
//! ```
//!
//! Unknown fields are ignored (newer manifests may add some); a higher
//! `schema` than [`SCHEMA`] is refused.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{UpdateError, UpdateResult};

/// Manifest schema this binary understands.
pub const SCHEMA: u32 = 1;

/// Largest accepted manifest.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Largest accepted release-notes text.
pub const MAX_NOTES_BYTES: usize = 64 * 1024;

/// Hard cap of an artefact, whatever the manifest says.
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Regular releases.
    #[default]
    Stable,
    /// Pre-releases (opt-in).
    Beta,
}

impl Channel {
    /// `stable` / `beta`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }

    /// Parses `stable` / `beta`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stable" => Some(Self::Stable),
            "beta" | "prerelease" => Some(Self::Beta),
            _ => None,
        }
    }

    /// Whether a release of `other` may be offered on this channel.
    pub const fn accepts(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Beta, _) | (Self::Stable, Self::Stable)
        )
    }
}

/// How urgent a release is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Features and fixes.
    #[default]
    Regular,
    /// Fixes a security vulnerability.
    Security,
}

impl Severity {
    /// `regular` / `security`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Security => "security",
        }
    }
}

/// A downloadable build of the release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Rust target triple (`x86_64-unknown-linux-musl`).
    pub target: String,
    /// File name (`hostingos-<version>-<target>.tar.gz`).
    pub name: String,
    /// HTTPS download URL.
    pub url: String,
    /// Exact size in bytes.
    pub size: u64,
    /// SHA-256 of the file, lower-case hex.
    pub sha256: String,
    /// Detached minisign signature of the file; default `<url>.minisig`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_url: Option<String>,
}

impl Artifact {
    /// URL of the detached signature.
    pub fn signature_url(&self) -> String {
        self.signature_url
            .clone()
            .unwrap_or_else(|| format!("{}{}", self.url, crate::SIGNATURE_SUFFIX))
    }
}

/// A release manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version ([`SCHEMA`]).
    pub schema: u32,
    /// Always [`crate::PRODUCT`].
    pub product: String,
    /// Semantic version of the release.
    pub version: String,
    /// Release channel.
    #[serde(default)]
    pub channel: Channel,
    /// When the manifest was produced (monotonic per release line).
    pub published_at: DateTime<Utc>,
    /// Security fix or regular release.
    #[serde(default)]
    pub severity: Severity,
    /// Oldest version that may update to this release directly.
    #[serde(default)]
    pub min_upgradable_from: Option<String>,
    /// Whether the release adds database migrations.
    #[serde(default)]
    pub migrations: bool,
    /// Whether the previous release still runs against the migrated schema
    /// (expand / contract), so a binary rollback needs no database restore.
    /// Absent means `false`: assume a restore is needed.
    #[serde(default)]
    pub rollback_safe: bool,
    /// Release notes (Markdown; rendered sanitised).
    #[serde(default)]
    pub notes_md: String,
    /// Container image of the release (`ghcr.io/owner/repo:<version>`).
    #[serde(default)]
    pub container_image: Option<String>,
    /// Binary builds.
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    /// Signing keys announced for later releases (key rotation).
    #[serde(default)]
    pub next_keys: Vec<String>,
    /// Endorsed key ids no longer trusted.
    #[serde(default)]
    pub revoked_keys: Vec<String>,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn safe_file_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'))
}

impl Manifest {
    /// Parses and validates the structure (not the signature: see
    /// [`crate::verify::verify_manifest`]).
    pub fn parse(raw: &[u8]) -> UpdateResult<Self> {
        if raw.len() > MAX_MANIFEST_BYTES {
            return Err(UpdateError::TooLarge {
                limit: MAX_MANIFEST_BYTES as u64,
            });
        }
        let m: Self = serde_json::from_slice(raw)
            .map_err(|e| UpdateError::Manifest(format!("not a valid manifest: {e}")))?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> UpdateResult<()> {
        let bad = |msg: String| Err(UpdateError::Manifest(msg));
        if self.schema == 0 || self.schema > SCHEMA {
            return bad(format!(
                "manifest schema {} is not supported (this binary reads schema {SCHEMA}); update manually",
                self.schema
            ));
        }
        if self.product != crate::PRODUCT {
            return bad(format!("manifest is for product {:?}", self.product));
        }
        let version = self.semver()?;
        if self.channel == Channel::Stable && !version.pre.is_empty() {
            return bad(format!("pre-release {version} on the stable channel"));
        }
        if let Some(min) = &self.min_upgradable_from {
            semver::Version::parse(min).map_err(|_| {
                UpdateError::Manifest(format!("invalid min_upgradable_from {min:?}"))
            })?;
        }
        if self.notes_md.len() > MAX_NOTES_BYTES {
            return bad("release notes too long".into());
        }
        for a in &self.artifacts {
            if a.target.is_empty()
                || !a
                    .target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return bad(format!("invalid artefact target {:?}", a.target));
            }
            if !safe_file_name(&a.name) {
                return bad(format!("invalid artefact name {:?}", a.name));
            }
            if !is_hex64(&a.sha256) {
                return bad(format!("invalid sha256 for {}", a.name));
            }
            if a.size == 0 || a.size > MAX_ARTIFACT_BYTES {
                return bad(format!("invalid size for {}", a.name));
            }
            for u in std::iter::once(a.url.clone()).chain(a.signature_url.clone()) {
                match url::Url::parse(&u) {
                    Ok(parsed) if parsed.scheme() == "https" || parsed.scheme() == "http" => {}
                    _ => return bad(format!("invalid artefact URL for {}", a.name)),
                }
            }
        }
        for k in &self.next_keys {
            crate::keys::key_id_of(k)
                .map_err(|_| UpdateError::Manifest("invalid key in next_keys".into()))?;
        }
        Ok(())
    }

    /// The version.
    pub fn semver(&self) -> UpdateResult<semver::Version> {
        semver::Version::parse(&self.version)
            .map_err(|_| UpdateError::Manifest(format!("invalid version {:?}", self.version)))
    }

    /// The artefact for `target`. A glibc build (`*-unknown-linux-gnu`, e.g.
    /// built from source) also takes the static musl build of its
    /// architecture, which runs on any Linux.
    pub fn artifact_for(&self, target: &str) -> UpdateResult<&Artifact> {
        let musl = target
            .strip_suffix("-unknown-linux-gnu")
            .map(|arch| format!("{arch}-unknown-linux-musl"));
        self.artifacts
            .iter()
            .find(|a| a.target == target)
            .or_else(|| {
                musl.as_deref()
                    .and_then(|m| self.artifacts.iter().find(|a| a.target == m))
            })
            .ok_or_else(|| UpdateError::WrongTarget(target.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "product": "hostingos",
            "version": "0.2.0",
            "channel": "stable",
            "published_at": "2026-12-01T10:00:00Z",
            "artifacts": [{
                "target": "x86_64-unknown-linux-musl",
                "name": "hostingos-0.2.0-x86_64-unknown-linux-musl.tar.gz",
                "url": "https://example.com/a.tar.gz",
                "size": 10,
                "sha256": "a".repeat(64)
            }]
        })
    }

    #[test]
    fn parses_and_validates() {
        let m = Manifest::parse(sample().to_string().as_bytes()).expect("valid");
        assert_eq!(m.version, "0.2.0");
        assert!(
            !m.rollback_safe,
            "absent rollback_safe means a restore is needed"
        );
        assert_eq!(m.severity, Severity::Regular);
        assert!(m.artifact_for("x86_64-unknown-linux-musl").is_ok());
        // A glibc build of the same architecture takes the static build.
        assert_eq!(
            m.artifact_for("x86_64-unknown-linux-gnu").unwrap().target,
            "x86_64-unknown-linux-musl"
        );
        assert!(m.artifact_for("x86_64-apple-darwin").is_err());
        assert!(matches!(
            m.artifact_for("aarch64-unknown-linux-musl"),
            Err(UpdateError::WrongTarget(_))
        ));
        assert_eq!(
            m.artifacts[0].signature_url(),
            "https://example.com/a.tar.gz.minisig"
        );
    }

    #[test]
    fn refuses_bad_manifests() {
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("schema", serde_json::json!(2)),
            ("product", serde_json::json!("other")),
            ("version", serde_json::json!("not-a-version")),
            ("version", serde_json::json!("0.3.0-beta.1")),
            ("min_upgradable_from", serde_json::json!("x")),
        ];
        for (field, value) in cases {
            let mut v = sample();
            v[field] = value;
            assert!(
                Manifest::parse(v.to_string().as_bytes()).is_err(),
                "{field} accepted"
            );
        }
        let mut v = sample();
        v["artifacts"][0]["name"] = serde_json::json!("../../etc/passwd");
        assert!(Manifest::parse(v.to_string().as_bytes()).is_err());
        let mut v = sample();
        v["artifacts"][0]["sha256"] = serde_json::json!("abc");
        assert!(Manifest::parse(v.to_string().as_bytes()).is_err());
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert!(matches!(
            Manifest::parse(&big),
            Err(UpdateError::TooLarge { .. })
        ));
    }

    #[test]
    fn channels() {
        assert!(Channel::Beta.accepts(Channel::Stable));
        assert!(Channel::Beta.accepts(Channel::Beta));
        assert!(Channel::Stable.accepts(Channel::Stable));
        assert!(!Channel::Stable.accepts(Channel::Beta));
        assert_eq!(Channel::parse("BETA"), Some(Channel::Beta));
        assert_eq!(Channel::parse("x"), None);
    }
}
