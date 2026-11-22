//! On-disk layout of a bare-binary install and the file operations of an
//! update.
//!
//! ```text
//! <root>/                          e.g. /opt/hostingos
//!   releases/<version>/            one unpacked release tarball each:
//!     hostingos                    the binary
//!     static/  locales/            assets read at runtime (next to the binary)
//!   current  -> releases/0.2.0     what the supervisor starts (atomic symlink)
//!   previous -> releases/0.1.0     rollback target (atomic symlink)
//!   state/
//!     update-marker.json           watchdog marker (crate::watchdog)
//!     update.lock                  local lock while an update runs
//!     downloads/                   partial downloads
//!     backups/                     pg_dump output
//! ```
//!
//! Symlinks are replaced atomically (`symlink` to a temporary name, then
//! `rename` over the old link), so a crash leaves either the old or the new
//! link, never none. A release directory is unpacked under a temporary name
//! and renamed into place only after it was validated.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::error::{UpdateError, UpdateResult};

/// File name of the binary inside a release directory.
pub const BINARY_NAME: &str = "hostingos";

/// Limits of an unpacked release archive.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    /// Total unpacked bytes.
    pub max_bytes: u64,
    /// Number of entries.
    pub max_entries: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024 * 1024,
            max_entries: 50_000,
        }
    }
}

fn valid_version_dir(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && !version.starts_with('.')
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

fn random_suffix() -> String {
    hex::encode(hos_core::crypto::random_bytes::<6>())
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "in-place updates need a Unix system",
    ))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    true
}

/// Replaces (or creates) `link` so it points to `target`, atomically.
pub fn atomic_symlink(target: &Path, link: &Path) -> io::Result<()> {
    let name = link.file_name().and_then(|n| n.to_str()).unwrap_or("link");
    let tmp = link.with_file_name(format!(".{name}.tmp-{}", random_suffix()));
    make_symlink(target, &tmp)?;
    if let Err(e) = fs::rename(&tmp, link) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(link.parent());
    Ok(())
}

fn sync_dir(dir: Option<&Path>) {
    if let Some(d) = dir
        && let Ok(f) = fs::File::open(d)
    {
        let _ = f.sync_all();
    }
}

/// Writes `bytes` to `path` atomically (temporary file, fsync, rename).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", random_suffix()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(path.parent());
    Ok(())
}

/// Free bytes on the filesystem holding `path` (`df -Pk`); `None` when it
/// cannot be determined.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// A bare-binary install root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// The layout under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/releases`.
    pub fn releases_dir(&self) -> PathBuf {
        self.root.join("releases")
    }

    /// `<root>/current`.
    pub fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }

    /// `<root>/previous`.
    pub fn previous_link(&self) -> PathBuf {
        self.root.join("previous")
    }

    /// `<root>/state`.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// `<root>/state/update-marker.json`.
    pub fn marker_path(&self) -> PathBuf {
        self.state_dir().join("update-marker.json")
    }

    /// `<root>/state/update.lock`.
    pub fn lock_path(&self) -> PathBuf {
        self.state_dir().join("update.lock")
    }

    /// `<root>/state/downloads`.
    pub fn downloads_dir(&self) -> PathBuf {
        self.state_dir().join("downloads")
    }

    /// `<root>/state/backups`.
    pub fn backups_dir(&self) -> PathBuf {
        self.state_dir().join("backups")
    }

    /// `<root>/releases/<version>`.
    pub fn release_dir(&self, version: &str) -> UpdateResult<PathBuf> {
        if !valid_version_dir(version) {
            return Err(UpdateError::Archive(format!("invalid version {version:?}")));
        }
        Ok(self.releases_dir().join(version))
    }

    /// Creates `state/`, `state/downloads/` and `state/backups/` (0700).
    pub fn ensure_state_dirs(&self) -> UpdateResult<()> {
        for d in [self.state_dir(), self.downloads_dir(), self.backups_dir()] {
            fs::create_dir_all(&d)?;
            set_mode(&d, 0o700)?;
        }
        Ok(())
    }

    fn resolve(&self, link: &Path) -> UpdateResult<PathBuf> {
        fs::symlink_metadata(link)?;
        Ok(fs::canonicalize(link)?)
    }

    /// Canonical directory `current` points to.
    pub fn current(&self) -> UpdateResult<PathBuf> {
        self.resolve(&self.current_link())
    }

    /// Canonical directory `previous` points to, if any.
    pub fn previous(&self) -> Option<PathBuf> {
        self.resolve(&self.previous_link()).ok()
    }

    fn link_target(&self, release: &Path) -> PathBuf {
        // Relative targets keep the tree relocatable.
        match release
            .canonicalize()
            .ok()
            .zip(self.releases_dir().canonicalize().ok())
        {
            Some((rel, base)) => match rel.strip_prefix(&base) {
                Ok(name) => Path::new("releases").join(name),
                Err(_) => release.to_path_buf(),
            },
            None => release.to_path_buf(),
        }
    }

    /// Points `current` at `release`.
    pub fn point_current(&self, release: &Path) -> UpdateResult<()> {
        atomic_symlink(&self.link_target(release), &self.current_link())?;
        Ok(())
    }

    /// Points `previous` at `release`, or removes it.
    pub fn point_previous(&self, release: Option<&Path>) -> UpdateResult<()> {
        match release {
            Some(r) => atomic_symlink(&self.link_target(r), &self.previous_link())?,
            None => match fs::remove_file(self.previous_link()) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }

    /// Makes `release` current: `previous` := the old current, then
    /// `current` := `release`. Returns the old current.
    pub fn activate(&self, release: &Path) -> UpdateResult<PathBuf> {
        let old = self.current()?;
        self.point_previous(Some(&old))?;
        self.point_current(release)?;
        Ok(old)
    }

    /// Checks that the root and `releases/` are writable.
    pub fn check_writable(&self) -> UpdateResult<()> {
        for dir in [self.root.clone(), self.releases_dir()] {
            let probe = dir.join(format!(".write-test-{}", random_suffix()));
            fs::write(&probe, b"ok").map_err(|e| {
                UpdateError::Preflight(format!("{} is not writable: {e}", dir.display()))
            })?;
            let _ = fs::remove_file(&probe);
        }
        Ok(())
    }

    /// Unpacks a `.tar.gz` release into a staging directory under
    /// `releases/` and validates it. The archive must hold exactly one
    /// top-level directory with `hostingos` (executable), `static/` and
    /// `locales/`. Only regular files and directories are accepted (no
    /// links, devices, absolute paths or `..`). Returns the staging path;
    /// [`commit_release`](Self::commit_release) moves it into place.
    pub fn unpack(
        &self,
        archive: &Path,
        version: &str,
        limits: ArchiveLimits,
    ) -> UpdateResult<PathBuf> {
        self.release_dir(version)?;
        fs::create_dir_all(self.releases_dir())?;
        let staging = self
            .releases_dir()
            .join(format!(".staging-{version}-{}", random_suffix()));
        fs::create_dir(&staging)?;
        match unpack_into(archive, &staging, limits).and_then(|()| validate_release(&staging)) {
            Ok(()) => Ok(staging),
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                Err(e)
            }
        }
    }

    /// Moves a validated staging directory to `releases/<version>`. An
    /// existing directory of that version (an earlier, unfinished attempt)
    /// is replaced unless it is `current` or `previous`.
    pub fn commit_release(&self, staging: &Path, version: &str) -> UpdateResult<PathBuf> {
        let dest = self.release_dir(version)?;
        if dest.exists() {
            let canon = fs::canonicalize(&dest)?;
            if self.current().ok().as_ref() == Some(&canon)
                || self.previous().as_ref() == Some(&canon)
            {
                return Err(UpdateError::Preflight(format!(
                    "{} is in use (current or previous release)",
                    dest.display()
                )));
            }
            let trash = self
                .releases_dir()
                .join(format!(".old-{version}-{}", random_suffix()));
            fs::rename(&dest, &trash)?;
            let _ = fs::remove_dir_all(&trash);
        }
        fs::rename(staging, &dest)?;
        sync_dir(Some(&self.releases_dir()));
        Ok(fs::canonicalize(&dest)?)
    }

    /// Removes staging leftovers and releases other than `current`,
    /// `previous` and the `keep` newest others (by modification time).
    pub fn prune(&self, keep: usize) -> UpdateResult<Vec<PathBuf>> {
        let current = self.current().ok();
        let previous = self.previous();
        let mut others = Vec::new();
        let mut removed = Vec::new();
        for entry in fs::read_dir(self.releases_dir())? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".staging-") || name.starts_with(".old-") {
                if fs::remove_dir_all(&path).is_ok() {
                    removed.push(path);
                }
                continue;
            }
            let Ok(canon) = fs::canonicalize(&path) else {
                continue;
            };
            if Some(&canon) == current.as_ref() || Some(&canon) == previous.as_ref() {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            others.push((modified, path));
        }
        others.sort_by_key(|o| std::cmp::Reverse(o.0));
        for (_, path) in others.into_iter().skip(keep) {
            if fs::remove_dir_all(&path).is_ok() {
                removed.push(path);
            }
        }
        Ok(removed)
    }
}

fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn unpack_into(archive: &Path, dest: &Path, limits: ArchiveLimits) -> UpdateResult<()> {
    let file = fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(io::BufReader::new(file)));
    let mut top: Option<PathBuf> = None;
    let mut total: u64 = 0;
    let mut count = 0usize;
    let entries = tar
        .entries()
        .map_err(|e| UpdateError::Archive(format!("unreadable archive: {e}")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| UpdateError::Archive(format!("unreadable archive: {e}")))?;
        count += 1;
        if count > limits.max_entries {
            return Err(UpdateError::Archive("too many entries".into()));
        }
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            tar::EntryType::XGlobalHeader
                | tar::EntryType::XHeader
                | tar::EntryType::GNULongName
                | tar::EntryType::GNULongLink
        ) {
            continue;
        }
        let raw_path = entry
            .path()
            .map_err(|e| UpdateError::Archive(format!("invalid path: {e}")))?
            .into_owned();
        let rel = safe_relative(&raw_path)
            .ok_or_else(|| UpdateError::Archive(format!("unsafe path {}", raw_path.display())))?;
        let mut parts = rel.components();
        let first = PathBuf::from(parts.next().map(|c| c.as_os_str()).unwrap_or_default());
        match &top {
            None => top = Some(first.clone()),
            Some(t) if *t == first => {}
            Some(_) => {
                return Err(UpdateError::Archive(
                    "the archive must contain a single top-level directory".into(),
                ));
            }
        }
        let inner: PathBuf = parts.collect();
        let target = dest.join(&inner);
        match kind {
            tar::EntryType::Directory => {
                if !inner.as_os_str().is_empty() {
                    fs::create_dir_all(&target)?;
                }
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if inner.as_os_str().is_empty() {
                    return Err(UpdateError::Archive("file at the top level".into()));
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let size = entry.header().size().unwrap_or(0);
                total = total.saturating_add(size);
                if total > limits.max_bytes {
                    return Err(UpdateError::Archive(
                        "archive too large when unpacked".into(),
                    ));
                }
                let mut out = fs::File::create(&target)?;
                let copied = io::copy(&mut (&mut entry).take(size), &mut out)?;
                if copied != size {
                    return Err(UpdateError::Archive(format!(
                        "truncated entry {}",
                        inner.display()
                    )));
                }
                let mode = entry.header().mode().unwrap_or(0o644);
                set_mode(&target, if mode & 0o111 != 0 { 0o755 } else { 0o644 })?;
            }
            other => {
                return Err(UpdateError::Archive(format!(
                    "unsupported entry type {other:?} at {}",
                    raw_path.display()
                )));
            }
        }
    }
    if top.is_none() {
        return Err(UpdateError::Archive("empty archive".into()));
    }
    Ok(())
}

fn validate_release(dir: &Path) -> UpdateResult<()> {
    let bin = dir.join(BINARY_NAME);
    let meta = fs::symlink_metadata(&bin)
        .map_err(|_| UpdateError::Archive(format!("{BINARY_NAME} is missing")))?;
    if !meta.is_file() || !is_executable(&meta) {
        return Err(UpdateError::Archive(format!(
            "{BINARY_NAME} is not an executable file"
        )));
    }
    for d in ["static", "locales"] {
        if !dir.join(d).is_dir() {
            return Err(UpdateError::Archive(format!("{d}/ is missing")));
        }
    }
    Ok(())
}

/// A lock file held while an update runs on this host. Removed on drop; a
/// lock older than `stale_after` (a crashed updater) is taken over.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Takes the lock or fails with [`UpdateError::Busy`].
    pub fn acquire(path: &Path, stale_after: Duration) -> UpdateResult<Self> {
        for attempt in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    let _ = f.sync_all();
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > stale_after);
                    if attempt == 0 && stale {
                        tracing::warn!(path = %path.display(), "taking over a stale update lock");
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    return Err(UpdateError::Busy);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(UpdateError::Busy)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert_eq!(safe_relative(Path::new("a/b")), Some(PathBuf::from("a/b")));
        assert_eq!(safe_relative(Path::new("./a")), Some(PathBuf::from("a")));
        assert_eq!(safe_relative(Path::new("../a")), None);
        assert_eq!(safe_relative(Path::new("/etc/passwd")), None);
        assert_eq!(safe_relative(Path::new("a/../../b")), None);
        assert!(valid_version_dir("0.2.0-beta.1+build.5"));
        assert!(!valid_version_dir("../x"));
        assert!(!valid_version_dir(".hidden"));
    }
}
