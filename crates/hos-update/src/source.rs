//! Where releases come from.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `HOS_UPDATE_REPO` | `owner/name` of a GitHub repository: its Releases are listed through the GitHub API; each release carries `hostingos-manifest.json` + `.minisig` |
//! | `HOS_UPDATE_MANIFEST_URL` | full HTTPS URL of a manifest (a mirror or self-hosted channel file); the signature is `<url>.minisig`. Wins over `HOS_UPDATE_REPO` |
//!
//! Neither set (the default — no repository exists yet) means update
//! checking is off ([`UpdateError::NotConfigured`]).

use serde::Deserialize;
use url::Url;

use crate::error::{UpdateError, UpdateResult};
use crate::fetch::{Fetcher, GITHUB_HOSTS};
use crate::manifest::Channel;

/// Default GitHub API base.
pub const GITHUB_API: &str = "https://api.github.com";

/// Largest accepted GitHub release listing.
pub const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;

/// A release source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// GitHub Releases of `repo` (`owner/name`).
    GitHub {
        /// `owner/name`.
        repo: String,
        /// API base (`https://api.github.com`; tests point it at wiremock).
        api_base: Url,
    },
    /// One manifest URL (signature at `<url>.minisig`).
    Manifest {
        /// The manifest.
        url: Url,
    },
}

fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let owner_ok = (1..=39).contains(&owner.len())
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !owner.starts_with('-');
    let name_ok = (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && name != "."
        && name != "..";
    owner_ok && name_ok
}

impl Source {
    /// From `HOS_UPDATE_MANIFEST_URL` / `HOS_UPDATE_REPO`; `Ok(None)` when
    /// neither is set.
    pub fn from_env() -> UpdateResult<Option<Self>> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self::from_settings(
            var("HOS_UPDATE_REPO").as_deref(),
            var("HOS_UPDATE_MANIFEST_URL").as_deref(),
        )
    }

    /// From explicit values (manifest URL wins).
    pub fn from_settings(
        repo: Option<&str>,
        manifest_url: Option<&str>,
    ) -> UpdateResult<Option<Self>> {
        if let Some(raw) = manifest_url.map(str::trim).filter(|s| !s.is_empty()) {
            let url = Url::parse(raw).map_err(|_| {
                UpdateError::Config(format!("HOS_UPDATE_MANIFEST_URL {raw:?} is not a URL"))
            })?;
            if url.scheme() != "https" {
                return Err(UpdateError::Config(
                    "HOS_UPDATE_MANIFEST_URL must be an https:// URL".into(),
                ));
            }
            if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
                return Err(UpdateError::Config(
                    "HOS_UPDATE_MANIFEST_URL needs a host and no credentials".into(),
                ));
            }
            return Ok(Some(Self::Manifest { url }));
        }
        match repo.map(str::trim).filter(|s| !s.is_empty()) {
            Some(r) => Self::github(r).map(Some),
            None => Ok(None),
        }
    }

    /// GitHub Releases of `owner/name`.
    pub fn github(repo: &str) -> UpdateResult<Self> {
        let repo = repo
            .trim()
            .trim_start_matches("https://github.com/")
            .trim_end_matches('/');
        if !valid_repo(repo) {
            return Err(UpdateError::Config(format!(
                "HOS_UPDATE_REPO {repo:?} is not of the form owner/name"
            )));
        }
        let api_base = Url::parse(GITHUB_API).map_err(|e| UpdateError::Config(e.to_string()))?;
        Ok(Self::GitHub {
            repo: repo.to_owned(),
            api_base,
        })
    }

    /// Replaces the GitHub API base (tests).
    #[must_use]
    pub fn with_api_base(self, base: Url) -> Self {
        match self {
            Self::GitHub { repo, .. } => Self::GitHub {
                repo,
                api_base: base,
            },
            other => other,
        }
    }

    /// Hosts the updater may contact for this source (redirect allow-list).
    pub fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = GITHUB_HOSTS.iter().map(|h| (*h).to_owned()).collect();
        let own = match self {
            Self::GitHub { api_base, .. } => api_base.host_str(),
            Self::Manifest { url } => url.host_str(),
        };
        if let Some(h) = own {
            hosts.push(h.to_ascii_lowercase());
        }
        let extra = std::env::var("HOS_UPDATE_ALLOWED_HOSTS").unwrap_or_default();
        hosts.extend(
            extra
                .split(',')
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty()),
        );
        hosts
    }

    /// Human description (`GitHub releases of owner/name` / the URL).
    pub fn describe(&self) -> String {
        match self {
            Self::GitHub { repo, .. } => format!("GitHub releases of {repo}"),
            Self::Manifest { url } => crate::fetch::redact(url),
        }
    }

    /// The repository, for GitHub sources.
    pub fn repo(&self) -> Option<&str> {
        match self {
            Self::GitHub { repo, .. } => Some(repo),
            Self::Manifest { .. } => None,
        }
    }
}

/// A release that may be newer than the running binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Version from the tag (`None` for a plain manifest URL).
    pub version: Option<semver::Version>,
    /// Release tag (`v0.2.0`), if any.
    pub tag: Option<String>,
    /// Manifest URL.
    pub manifest_url: Url,
    /// Signature URL.
    pub signature_url: Url,
    /// Release page, if any.
    pub html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Version of a tag: `v1.2.3` / `1.2.3`.
pub fn tag_version(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.trim().trim_start_matches('v')).ok()
}

/// Lists releases newer than `current` on `channel`, newest first, at most
/// `max`. A manifest-URL source always yields its one manifest.
pub async fn discover(
    fetcher: &Fetcher,
    source: &Source,
    channel: Channel,
    current: &semver::Version,
    max: usize,
) -> UpdateResult<Vec<Candidate>> {
    match source {
        Source::Manifest { url } => {
            let sig = Url::parse(&format!("{url}{}", crate::SIGNATURE_SUFFIX))
                .map_err(|e| UpdateError::Config(e.to_string()))?;
            Ok(vec![Candidate {
                version: None,
                tag: None,
                manifest_url: url.clone(),
                signature_url: sig,
                html_url: None,
            }])
        }
        Source::GitHub { repo, api_base } => {
            let listing = api_base
                .join(&format!("repos/{repo}/releases?per_page=30"))
                .map_err(|e| UpdateError::Config(e.to_string()))?;
            let body = fetcher
                .get_bytes(&listing, "application/vnd.github+json", MAX_LISTING_BYTES)
                .await?;
            let releases: Vec<GhRelease> = serde_json::from_slice(&body)
                .map_err(|e| UpdateError::Network(format!("unexpected GitHub API answer: {e}")))?;
            let mut out = Vec::new();
            for r in releases {
                if r.draft || (r.prerelease && channel == Channel::Stable) {
                    continue;
                }
                let Some(version) = tag_version(&r.tag_name) else {
                    continue;
                };
                if !version.pre.is_empty() && channel == Channel::Stable {
                    continue;
                }
                if version <= *current {
                    continue;
                }
                let asset = |name: &str| {
                    r.assets
                        .iter()
                        .find(|a| a.name == name)
                        .and_then(|a| Url::parse(&a.browser_download_url).ok())
                };
                let sig_name = format!("{}{}", crate::MANIFEST_ASSET, crate::SIGNATURE_SUFFIX);
                let (Some(manifest_url), Some(signature_url)) =
                    (asset(crate::MANIFEST_ASSET), asset(&sig_name))
                else {
                    tracing::debug!(tag = %r.tag_name, "release without a signed manifest skipped");
                    continue;
                };
                out.push(Candidate {
                    version: Some(version),
                    tag: Some(r.tag_name),
                    manifest_url,
                    signature_url,
                    html_url: r.html_url,
                });
            }
            out.sort_by(|a, b| b.version.cmp(&a.version));
            out.truncate(max);
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(Source::from_settings(None, None).unwrap(), None);
        assert_eq!(Source::from_settings(Some(" "), Some("")).unwrap(), None);
        let gh = Source::from_settings(Some("acme-co/hostingos"), None)
            .unwrap()
            .unwrap();
        assert_eq!(gh.repo(), Some("acme-co/hostingos"));
        assert!(Source::from_settings(Some("nope"), None).is_err());
        assert!(Source::from_settings(Some("a/b/c"), None).is_err());
        assert!(Source::from_settings(Some("../x"), None).is_err());
        let m = Source::from_settings(Some("a/b"), Some("https://updates.example.com/stable.json"))
            .unwrap()
            .unwrap();
        assert!(matches!(m, Source::Manifest { .. }));
        assert!(
            m.allowed_hosts()
                .contains(&"updates.example.com".to_owned())
        );
        assert!(Source::from_settings(None, Some("http://updates.example.com/x.json")).is_err());
        assert!(Source::from_settings(None, Some("https://u:p@updates.example.com/x")).is_err());
    }

    #[test]
    fn tags() {
        assert_eq!(tag_version("v1.2.3"), Some(semver::Version::new(1, 2, 3)));
        assert!(tag_version("release-1").is_none());
    }
}
