//! The error type of the update system. Messages are written for operators
//! (shown in `/platform/updates` and printed by `hostingos update …`) and
//! never contain secrets.

/// Result alias.
pub type UpdateResult<T> = Result<T, UpdateError>;

/// Everything that can go wrong while checking for or installing an update.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// Neither `HOS_UPDATE_REPO` nor `HOS_UPDATE_MANIFEST_URL` is set.
    #[error(
        "update checking is not configured: set HOS_UPDATE_REPO (owner/name of the GitHub repository) or HOS_UPDATE_MANIFEST_URL"
    )]
    NotConfigured,
    /// Invalid update settings (repository name, URL, channel …).
    #[error("invalid update configuration: {0}")]
    Config(String),
    /// No signing key is compiled into the binary (or endorsed).
    #[error(
        "no release signing key is built into this binary, so updates cannot be verified and are not installed (see docs/UPDATES.md, \"Signing keys\")"
    )]
    NoTrustedKeys,
    /// A minisign signature is missing, malformed, made by an unknown key or
    /// does not match the data.
    #[error("signature check failed: {0}")]
    Signature(String),
    /// The manifest is malformed or not for this product / schema.
    #[error("release manifest rejected: {0}")]
    Manifest(String),
    /// The offered version is not newer than the running one (downgrade or
    /// re-install).
    #[error("version {offered} is not newer than the running version {current}")]
    NotNewer {
        /// Version in the manifest.
        offered: String,
        /// Running version.
        current: String,
    },
    /// The manifest is older than one already seen for the same version
    /// (replayed / stale manifest) or dated in the future.
    #[error("stale or replayed release manifest: {0}")]
    Replay(String),
    /// The running version is too old to update to this release directly.
    #[error("version {offered} can only be installed from {min} or newer (running {current})")]
    NotUpgradable {
        /// Version in the manifest.
        offered: String,
        /// `min_upgradable_from`.
        min: String,
        /// Running version.
        current: String,
    },
    /// The release has no artefact for this binary's target triple.
    #[error("the release has no artefact for target {0}")]
    WrongTarget(String),
    /// A URL or redirect was refused (not HTTPS, host not allowed, outbound
    /// policy, too many redirects).
    #[error("download refused: {0}")]
    Refused(String),
    /// A response or artefact exceeded its size limit.
    #[error("download exceeds the size limit of {limit} bytes")]
    TooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// Connection, TLS, timeout or HTTP status errors.
    #[error("network error: {0}")]
    Network(String),
    /// The downloaded artefact does not match the signed SHA-256 / size.
    #[error("checksum mismatch: {0}")]
    Checksum(String),
    /// Filesystem errors.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Another update holds the lock (this node or another one).
    #[error("another update is already in progress")]
    Busy,
    /// A pre-flight check failed; nothing was changed.
    #[error("pre-flight check failed: {0}")]
    Preflight(String),
    /// The database backup failed; nothing was changed.
    #[error("database backup failed: {0}")]
    Backup(String),
    /// `hostingos migrate` of the new version failed; the previous binary was
    /// put back.
    #[error("database migrations failed: {0}")]
    Migration(String),
    /// The release archive is malformed or unsafe (path traversal, links,
    /// missing files).
    #[error("invalid release archive: {0}")]
    Archive(String),
    /// The install type cannot be updated in place.
    #[error("{0}")]
    Unsupported(String),
    /// The new binary did not pass its smoke test.
    #[error("the new binary failed its self-test: {0}")]
    SmokeTest(String),
}

impl UpdateError {
    /// Short machine-readable kind (stored with failed runs, used in logs).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Config(_) => "config",
            Self::NoTrustedKeys => "no_trusted_keys",
            Self::Signature(_) => "signature",
            Self::Manifest(_) => "manifest",
            Self::NotNewer { .. } => "not_newer",
            Self::Replay(_) => "replay",
            Self::NotUpgradable { .. } => "not_upgradable",
            Self::WrongTarget(_) => "wrong_target",
            Self::Refused(_) => "refused",
            Self::TooLarge { .. } => "too_large",
            Self::Network(_) => "network",
            Self::Checksum(_) => "checksum",
            Self::Io(_) => "io",
            Self::Busy => "busy",
            Self::Preflight(_) => "preflight",
            Self::Backup(_) => "backup",
            Self::Migration(_) => "migration",
            Self::Archive(_) => "archive",
            Self::Unsupported(_) => "unsupported",
            Self::SmokeTest(_) => "smoke_test",
        }
    }
}
