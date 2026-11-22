//! What this binary is: semantic version (the workspace version), git
//! commit, build date and target triple, compiled in by `build.rs`.
//!
//! Shown to platform admins only (`/platform/system`, `/platform/updates`,
//! `hostingos version`); public endpoints (`/healthz`, the API) do not
//! reveal the version.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// The workspace version (`[workspace.package] version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Git commit (`unknown` when built without git and without `HOS_BUILD_COMMIT`).
pub const COMMIT: &str = env!("HOS_BUILD_COMMIT");
/// Target triple, e.g. `x86_64-unknown-linux-musl`.
pub const TARGET: &str = env!("HOS_BUILD_TARGET");
/// Cargo profile (`release`, `debug`).
pub const PROFILE: &str = env!("HOS_BUILD_PROFILE");
const EPOCH: &str = env!("HOS_BUILD_EPOCH");

/// Build metadata of the running binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    /// Semantic version.
    pub version: String,
    /// Full git commit hash or `unknown`.
    pub commit: String,
    /// Build date (`SOURCE_DATE_EPOCH` or commit time); `None` when unknown.
    pub build_date: Option<DateTime<Utc>>,
    /// Target triple.
    pub target: String,
    /// Cargo profile.
    pub profile: String,
}

impl BuildInfo {
    /// The running binary.
    pub fn current() -> Self {
        Self {
            version: VERSION.to_owned(),
            commit: COMMIT.to_owned(),
            build_date: EPOCH
                .parse::<i64>()
                .ok()
                .and_then(|s| DateTime::from_timestamp(s, 0)),
            target: TARGET.to_owned(),
            profile: PROFILE.to_owned(),
        }
    }

    /// The version as [`semver::Version`] (`0.0.0` if it does not parse,
    /// which `Cargo.toml` already rules out).
    pub fn semver(&self) -> semver::Version {
        semver::Version::parse(&self.version).unwrap_or_else(|_| semver::Version::new(0, 0, 0))
    }

    /// First 12 characters of the commit.
    pub fn short_commit(&self) -> &str {
        self.commit.get(..12).unwrap_or(&self.commit)
    }

    /// One line for logs and `hostingos --version`-like output.
    pub fn describe(&self) -> String {
        let date = self
            .build_date
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown date".to_owned());
        format!(
            "hostingos {} ({} {}, {})",
            self.version,
            self.short_commit(),
            date,
            self.target
        )
    }
}

/// The `User-Agent` of update checks: product and version, nothing else.
pub fn user_agent() -> String {
    format!("HostingOS-Updater/{VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_build_info_is_consistent() {
        let b = BuildInfo::current();
        assert_eq!(b.version, env!("CARGO_PKG_VERSION"));
        assert!(semver::Version::parse(&b.version).is_ok());
        assert!(!b.target.is_empty());
        assert!(b.commit == "unknown" || b.commit.bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(b.describe().starts_with("hostingos "));
        assert!(user_agent().starts_with("HostingOS-Updater/"));
    }
}
