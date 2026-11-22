//! Installing a verified release on a bare-binary install.
//!
//! ```text
//! lock (cluster: DB advisory lock via Hooks; host: state/update.lock)
//!  → pre-flight: layout, write permission, free disk, database, no unfinished update
//!  → maintenance mode on, running jobs drained
//!  → download (HTTPS, size + SHA-256 from the signed manifest)
//!  → verify the artefact's own minisign signature
//!  → unpack to a staging dir, validate, smoke-test `hostingos version --json`
//!  → database backup (command / pg_dump / operator confirmation)
//!  → marker "swapping" → previous := old, current := new → marker "migrating"
//!  → `<new>/hostingos migrate`            (failure: old release back, maintenance off)
//!  → marker "restarting" (+ deadline) → ask the process to exit (75)
//! ```
//!
//! After the restart the new binary finds the marker ([`crate::watchdog`]),
//! checks its health and either confirms (maintenance off, marker removed)
//! or rolls back. Everything that needs the database or the process is a
//! [`Hooks`] method, so the state machine runs against a temporary
//! directory with fake binaries in tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{UpdateError, UpdateResult};
use crate::fetch::Fetcher;
use crate::keys::{KeyRing, MAX_SIGNATURE_BYTES};
use crate::layout::{ArchiveLimits, BINARY_NAME, FileLock, Layout, free_bytes};
use crate::manifest::{Artifact, Manifest};
use crate::watchdog::{self, DEFAULT_MAX_ATTEMPTS, Marker, Phase};

/// A local update lock older than this belongs to a crashed updater.
pub const STALE_LOCK: Duration = Duration::from_secs(3 * 60 * 60);

/// Reserve kept free on top of three times the artefact size.
pub const DISK_RESERVE: u64 = 200 * 1024 * 1024;

/// Steps reported through [`Hooks::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Checks before anything changes.
    Preflight,
    /// Maintenance mode, job drain.
    Maintenance,
    /// Artefact download.
    Download,
    /// Signature / checksum.
    Verify,
    /// Unpack + smoke test.
    Unpack,
    /// Database backup.
    Backup,
    /// `current` switch.
    Swap,
    /// `hostingos migrate`.
    Migrate,
    /// Exit for the supervisor.
    Restart,
    /// Putting the old release back.
    Rollback,
}

impl Step {
    /// Snake-case name (stored in `update_runs.step`, translated in the UI).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Maintenance => "maintenance",
            Self::Download => "download",
            Self::Verify => "verify",
            Self::Unpack => "unpack",
            Self::Backup => "backup",
            Self::Swap => "swap",
            Self::Migrate => "migrate",
            Self::Restart => "restart",
            Self::Rollback => "rollback",
        }
    }
}

/// Everything outside the filesystem: database, process, progress.
#[async_trait]
pub trait Hooks: Send + Sync {
    /// Progress (the caller logs it into `update_runs`).
    async fn step(&self, step: Step, detail: &str);
    /// Cluster-wide lock (a PostgreSQL advisory lock). The returned guard
    /// is held until the apply ends; `Err(Busy)` when another node holds it.
    async fn lock(&self) -> UpdateResult<Box<dyn Send>>;
    /// The database is reachable.
    async fn check_database(&self) -> Result<(), String>;
    /// Maintenance mode on (pauses the job workers on every node).
    async fn enter_maintenance(&self) -> Result<(), String>;
    /// Waits until no job is running.
    async fn drain_jobs(&self) -> Result<(), String>;
    /// Maintenance mode off (after a failure before the restart).
    async fn leave_maintenance(&self);
    /// Backs the database up; returns a description.
    async fn backup(&self) -> Result<String, String>;
    /// Runs `<binary> migrate`.
    async fn migrate(&self, binary: &Path) -> Result<(), String>;
    /// Asks the process to exit for the supervisor.
    async fn request_restart(&self);
}

/// Gets an artefact and its detached signature.
#[async_trait]
pub trait Downloader: Send + Sync {
    /// Downloads to `dest`, checking size and SHA-256.
    async fn download(&self, artifact: &Artifact, dest: &Path) -> UpdateResult<()>;
    /// The detached minisign signature.
    async fn signature(&self, artifact: &Artifact) -> UpdateResult<String>;
}

#[async_trait]
impl Downloader for Fetcher {
    async fn download(&self, artifact: &Artifact, dest: &Path) -> UpdateResult<()> {
        Fetcher::download(self, artifact, dest).await
    }

    async fn signature(&self, artifact: &Artifact) -> UpdateResult<String> {
        let url = url::Url::parse(&artifact.signature_url()).map_err(|_| {
            UpdateError::Manifest(format!("invalid signature URL for {}", artifact.name))
        })?;
        self.get_text(&url, "application/octet-stream", MAX_SIGNATURE_BYTES)
            .await
    }
}

/// What to install.
#[derive(Debug, Clone)]
pub struct ApplyRequest {
    /// `update_runs.id`.
    pub run_id: String,
    /// The install root.
    pub layout: Layout,
    /// The verified manifest.
    pub manifest: Manifest,
    /// Keys for the artefact signature.
    pub ring: KeyRing,
    /// This binary's target triple.
    pub target: String,
    /// Running version.
    pub current_version: String,
    /// Time the new binary gets to become healthy after the restart.
    pub health_timeout: Duration,
    /// Starts of the new binary before it is given up.
    pub max_attempts: u32,
    /// Archive limits.
    pub limits: ArchiveLimits,
    /// Skip the free-space check (tests).
    pub skip_disk_check: bool,
}

impl ApplyRequest {
    /// Defaults: 5 minutes to become healthy, three starts.
    pub fn new(run_id: &str, layout: Layout, manifest: Manifest, ring: KeyRing) -> Self {
        let build = crate::BuildInfo::current();
        Self {
            run_id: run_id.to_owned(),
            layout,
            manifest,
            ring,
            target: build.target,
            current_version: build.version,
            health_timeout: Duration::from_secs(300),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            limits: ArchiveLimits::default(),
            skip_disk_check: false,
        }
    }
}

/// A finished apply (the process is about to restart).
#[derive(Debug, Clone)]
pub struct Applied {
    /// The new release directory.
    pub release: PathBuf,
    /// The release that was current.
    pub old_release: PathBuf,
    /// Backup description.
    pub backup: String,
    /// The marker as written.
    pub marker: Marker,
}

#[derive(Debug, Deserialize)]
struct SelfReport {
    version: String,
    target: String,
}

/// Runs `<binary> version --json` and checks version and target.
pub async fn smoke_test(binary: &Path, version: &str, target: &str) -> UpdateResult<()> {
    let dir = binary.parent().unwrap_or_else(|| Path::new("/"));
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("version")
        .arg("--json")
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| UpdateError::SmokeTest("timed out".into()))?
        .map_err(|e| UpdateError::SmokeTest(format!("cannot run it: {e}")))?;
    if !out.status.success() {
        return Err(UpdateError::SmokeTest(format!(
            "exit status {}",
            out.status
        )));
    }
    let report: SelfReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| UpdateError::SmokeTest(format!("unexpected output: {e}")))?;
    if report.version != version {
        return Err(UpdateError::SmokeTest(format!(
            "reports version {}, expected {version}",
            report.version
        )));
    }
    if report.target != target {
        return Err(UpdateError::SmokeTest(format!(
            "built for {}, this host runs {target}",
            report.target
        )));
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> UpdateResult<T> + Send + 'static,
) -> UpdateResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| UpdateError::Io(std::io::Error::other(e.to_string())))?
}

/// Installs the release (see the module docs). On `Ok` the old process
/// must exit; on `Err` nothing is left changed except a failed download.
pub async fn apply(
    req: &ApplyRequest,
    dl: &dyn Downloader,
    hooks: &dyn Hooks,
) -> UpdateResult<Applied> {
    if req.ring.is_empty() {
        return Err(UpdateError::NoTrustedKeys);
    }
    let artifact = req.manifest.artifact_for(&req.target)?.clone();
    let _cluster_lock = hooks.lock().await?;
    req.layout.ensure_state_dirs()?;
    let _host_lock = FileLock::acquire(&req.layout.lock_path(), STALE_LOCK)?;

    hooks.step(Step::Preflight, "").await;
    if let Some(m) = watchdog::read(&req.layout)? {
        return Err(UpdateError::Preflight(format!(
            "an earlier update ({} → {}, {}) has not finished; see {}",
            m.from_version,
            m.to_version,
            m.phase.as_str(),
            req.layout.marker_path().display()
        )));
    }
    let old_release = req
        .layout
        .current()
        .map_err(|e| UpdateError::Preflight(format!("current release: {e}")))?;
    req.layout.check_writable()?;
    if !req.skip_disk_check {
        let need = artifact.size.saturating_mul(3).saturating_add(DISK_RESERVE);
        match free_bytes(req.layout.root()) {
            Some(free) if free < need => {
                return Err(UpdateError::Preflight(format!(
                    "{free} bytes free on {}, {need} needed",
                    req.layout.root().display()
                )));
            }
            Some(_) => {}
            None => tracing::warn!("free disk space unknown; continuing"),
        }
    }
    hooks
        .check_database()
        .await
        .map_err(|e| UpdateError::Preflight(format!("database: {e}")))?;

    hooks.step(Step::Maintenance, "").await;
    hooks
        .enter_maintenance()
        .await
        .map_err(|e| UpdateError::Preflight(format!("maintenance mode: {e}")))?;
    match install(req, &artifact, &old_release, dl, hooks).await {
        Ok(done) => Ok(done),
        Err(e) => {
            hooks.leave_maintenance().await;
            Err(e)
        }
    }
}

async fn install(
    req: &ApplyRequest,
    artifact: &Artifact,
    old_release: &Path,
    dl: &dyn Downloader,
    hooks: &dyn Hooks,
) -> UpdateResult<Applied> {
    hooks
        .drain_jobs()
        .await
        .map_err(|e| UpdateError::Preflight(format!("jobs: {e}")))?;

    hooks.step(Step::Download, &artifact.name).await;
    let archive = req.layout.downloads_dir().join(&artifact.name);
    dl.download(artifact, &archive).await?;
    let result = unpack_and_switch(req, artifact, &archive, old_release, dl, hooks).await;
    let _ = std::fs::remove_file(&archive);
    result
}

async fn unpack_and_switch(
    req: &ApplyRequest,
    artifact: &Artifact,
    archive: &Path,
    old_release: &Path,
    dl: &dyn Downloader,
    hooks: &dyn Hooks,
) -> UpdateResult<Applied> {
    let version = req.manifest.version.clone();
    hooks.step(Step::Verify, &artifact.name).await;
    let signature = dl.signature(artifact).await?;
    let ring = req.ring.clone();
    let path = archive.to_path_buf();
    let verified = blocking(move || ring.verify_file(&path, &signature)).await?;
    tracing::info!(key = %verified.key_id, artefact = %artifact.name, "release artefact signature verified");

    hooks.step(Step::Unpack, &version).await;
    let layout = req.layout.clone();
    let (path, v, limits) = (archive.to_path_buf(), version.clone(), req.limits);
    let staging = blocking(move || layout.unpack(&path, &v, limits)).await?;
    if let Err(e) = smoke_test(&staging.join(BINARY_NAME), &version, &artifact.target).await {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let release = req.layout.commit_release(&staging, &version)?;

    hooks.step(Step::Backup, "").await;
    let backup = hooks.backup().await.map_err(UpdateError::Backup)?;

    hooks.step(Step::Swap, &version).await;
    let mut marker = Marker {
        run_id: req.run_id.clone(),
        from_version: req.current_version.clone(),
        to_version: version.clone(),
        old_release: old_release.to_path_buf(),
        new_release: release.clone(),
        prior_previous: req.layout.previous(),
        phase: Phase::Swapping,
        deadline: 0,
        attempts: 0,
        max_attempts: req.max_attempts,
        rollback_safe: req.manifest.rollback_safe,
        message: None,
    };
    watchdog::write(&req.layout, &marker)?;
    if let Err(e) = req.layout.activate(&release) {
        let _ = watchdog::roll_back(&req.layout, &mut marker, "switching releases failed");
        let _ = watchdog::clear(&req.layout);
        return Err(e);
    }
    marker.phase = Phase::Migrating;
    watchdog::write(&req.layout, &marker)?;

    hooks.step(Step::Migrate, &version).await;
    let new_binary = req.layout.current_link().join(BINARY_NAME);
    if let Err(e) = hooks.migrate(&new_binary).await {
        hooks.step(Step::Rollback, &e).await;
        watchdog::roll_back(&req.layout, &mut marker, "migrations failed")?;
        watchdog::clear(&req.layout)?;
        let hint = if req.manifest.rollback_safe {
            "the previous release is current again"
        } else {
            "the previous release is current again; this release is not rollback-safe, so if some migrations were applied, restore the backup before relying on the old version"
        };
        return Err(UpdateError::Migration(format!("{e} ({hint})")));
    }

    marker.phase = Phase::Restarting;
    marker.deadline =
        chrono::Utc::now().timestamp() + i64::try_from(req.health_timeout.as_secs()).unwrap_or(300);
    watchdog::write(&req.layout, &marker)?;
    hooks.step(Step::Restart, &version).await;
    hooks.request_restart().await;
    Ok(Applied {
        release,
        old_release: old_release.to_path_buf(),
        backup,
        marker,
    })
}

/// Manual rollback (`hostingos update rollback`): `current` := `previous`,
/// `previous` := removed. Returns (from, to) release directories. The
/// caller restarts the service.
pub fn rollback_to_previous(layout: &Layout) -> UpdateResult<(PathBuf, PathBuf)> {
    let _lock = FileLock::acquire(&layout.lock_path(), STALE_LOCK)?;
    let current = layout.current()?;
    let previous = layout.previous().ok_or_else(|| {
        UpdateError::Preflight("there is no previous release to go back to".into())
    })?;
    if previous == current {
        return Err(UpdateError::Preflight(
            "previous and current are the same release".into(),
        ));
    }
    layout.point_current(&previous)?;
    layout.point_previous(None)?;
    watchdog::clear(layout)?;
    Ok((current, previous))
}
