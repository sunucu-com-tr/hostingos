//! The apply state machine against a temporary directory with fake
//! binaries: success, failed health check → rollback, crash mid-swap →
//! recovery, migration failure, concurrent applies, unsafe archives.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use hos_update::apply::{ApplyRequest, Downloader, Hooks, Step, apply, rollback_to_previous};
use hos_update::layout::{ArchiveLimits, FileLock, Layout};
use hos_update::manifest::{Artifact, Manifest};
use hos_update::watchdog::{self, Phase, Startup};
use hos_update::{UpdateError, UpdateResult};
use tokio::sync::{Mutex, Notify};

use crate::support::{
    TempDir, TestKey, evil_tarball, fake_binary, layout_with, manifest_json, release_tarball,
    sha256_hex, target, write,
};

/// Serves a local tarball + signature.
struct LocalDownloader {
    tarball: Vec<u8>,
    signature: String,
}

#[async_trait]
impl Downloader for LocalDownloader {
    async fn download(&self, artifact: &Artifact, dest: &Path) -> UpdateResult<()> {
        if sha256_hex(&self.tarball) != artifact.sha256 {
            return Err(UpdateError::Checksum("fake".into()));
        }
        std::fs::write(dest, &self.tarball)?;
        Ok(())
    }

    async fn signature(&self, _artifact: &Artifact) -> UpdateResult<String> {
        Ok(self.signature.clone())
    }
}

struct Guard(Arc<AtomicBool>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct FakeHooks {
    steps: Mutex<Vec<String>>,
    /// Shared between "nodes": the advisory lock.
    lock: Arc<AtomicBool>,
    maintenance: AtomicBool,
    restart: AtomicBool,
    fail_migrate: bool,
    fail_backup: bool,
    /// When set, `drain_jobs` waits for it (to hold an apply mid-way).
    hold: Option<Arc<Notify>>,
    held: Option<Arc<Notify>>,
}

#[async_trait]
impl Hooks for FakeHooks {
    async fn step(&self, step: Step, _detail: &str) {
        self.steps.lock().await.push(step.as_str().to_owned());
    }
    async fn lock(&self) -> UpdateResult<Box<dyn Send>> {
        if self.lock.swap(true, Ordering::SeqCst) {
            return Err(UpdateError::Busy);
        }
        Ok(Box::new(Guard(self.lock.clone())))
    }
    async fn check_database(&self) -> Result<(), String> {
        Ok(())
    }
    async fn enter_maintenance(&self) -> Result<(), String> {
        self.maintenance.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn drain_jobs(&self) -> Result<(), String> {
        if let (Some(hold), Some(held)) = (&self.hold, &self.held) {
            held.notify_one();
            hold.notified().await;
        }
        Ok(())
    }
    async fn leave_maintenance(&self) {
        self.maintenance.store(false, Ordering::SeqCst);
    }
    async fn backup(&self) -> Result<String, String> {
        if self.fail_backup {
            Err("disk full".into())
        } else {
            Ok("test backup".into())
        }
    }
    async fn migrate(&self, binary: &Path) -> Result<(), String> {
        assert!(binary.ends_with("current/hostingos"));
        if self.fail_migrate {
            Err("relation already exists".into())
        } else {
            Ok(())
        }
    }
    async fn request_restart(&self) {
        self.restart.store(true, Ordering::SeqCst);
    }
}

struct Fixture {
    dir: TempDir,
    layout: Layout,
    old: std::path::PathBuf,
    key: TestKey,
    tarball: Vec<u8>,
}

impl Fixture {
    fn new(tag: &str, binary: &str) -> Self {
        let dir = TempDir::new(tag);
        let (layout, old) = layout_with(dir.path(), "0.1.0");
        Self {
            key: TestKey::generate(),
            tarball: release_tarball("0.2.0", binary),
            dir,
            layout,
            old,
        }
    }

    fn manifest(&self) -> Manifest {
        let json = manifest_json(
            "0.2.0",
            "https://github.com/acme/hostingos/releases/download/v0.2.0/a.tar.gz",
            self.tarball.len() as u64,
            &sha256_hex(&self.tarball),
        );
        Manifest::parse(&serde_json::to_vec(&json).unwrap()).unwrap()
    }

    fn request(&self) -> ApplyRequest {
        let mut r = ApplyRequest::new(
            "run-1",
            self.layout.clone(),
            self.manifest(),
            self.key.ring(),
        );
        r.current_version = "0.1.0".into();
        r.skip_disk_check = true;
        r.health_timeout = Duration::from_secs(300);
        r
    }

    fn downloader(&self) -> LocalDownloader {
        LocalDownloader {
            signature: self.key.sign(&self.tarball),
            tarball: self.tarball.clone(),
        }
    }
}

fn good_binary() -> String {
    fake_binary("0.2.0", &target())
}

#[tokio::test]
async fn successful_update_then_healthy_start() {
    let fx = Fixture::new("ok", &good_binary());
    let hooks = FakeHooks::default();
    let done = apply(&fx.request(), &fx.downloader(), &hooks)
        .await
        .expect("apply");
    let new = std::fs::canonicalize(fx.dir.path().join("releases/0.2.0")).unwrap();
    assert_eq!(done.release, new);
    assert_eq!(fx.layout.current().unwrap(), new);
    assert_eq!(fx.layout.previous().unwrap(), fx.old);
    assert!(hooks.restart.load(Ordering::SeqCst), "restart requested");
    assert!(
        hooks.maintenance.load(Ordering::SeqCst),
        "maintenance stays on until the new binary is healthy"
    );
    assert!(!hooks.lock.load(Ordering::SeqCst), "lock released");
    assert!(!fx.layout.lock_path().exists());
    assert_eq!(
        *hooks.steps.lock().await,
        [
            "preflight",
            "maintenance",
            "download",
            "verify",
            "unpack",
            "backup",
            "swap",
            "migrate",
            "restart"
        ]
    );
    let marker = watchdog::read(&fx.layout).unwrap().expect("marker");
    assert_eq!(marker.phase, Phase::Restarting);
    assert!(
        fx.layout
            .downloads_dir()
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );

    // The new binary starts: trial, health OK, marker gone.
    let now = chrono::Utc::now().timestamp();
    assert!(matches!(
        watchdog::on_startup(&fx.layout, "0.2.0", now).unwrap(),
        Startup::Trial(_)
    ));
    let confirmed = watchdog::confirm_healthy(&fx.layout)
        .unwrap()
        .expect("marker");
    assert_eq!(confirmed.to_version, "0.2.0");
    assert!(watchdog::read(&fx.layout).unwrap().is_none());
    assert!(matches!(
        watchdog::on_startup(&fx.layout, "0.2.0", now).unwrap(),
        Startup::Clean
    ));

    // Manual rollback afterwards puts 0.1.0 back.
    let (from, to) = rollback_to_previous(&fx.layout).unwrap();
    assert_eq!((from, to.clone()), (new, fx.old.clone()));
    assert_eq!(fx.layout.current().unwrap(), fx.old);
    assert!(
        rollback_to_previous(&fx.layout).is_err(),
        "no previous any more"
    );
}

#[tokio::test]
async fn failed_health_check_rolls_back() {
    let fx = Fixture::new("health", &good_binary());
    apply(&fx.request(), &fx.downloader(), &FakeHooks::default())
        .await
        .expect("apply");
    let now = chrono::Utc::now().timestamp();
    assert!(matches!(
        watchdog::on_startup(&fx.layout, "0.2.0", now).unwrap(),
        Startup::Trial(_)
    ));
    let m = watchdog::fail_trial(&fx.layout, "readiness check failed")
        .unwrap()
        .expect("marker");
    assert_eq!(m.phase, Phase::RolledBack);
    assert_eq!(
        fx.layout.current().unwrap(),
        fx.old,
        "old release is current again"
    );
    assert!(
        fx.layout.previous().is_none(),
        "previous restored to what it was"
    );
    // The old binary starts and records the rollback.
    match watchdog::on_startup(&fx.layout, "0.1.0", now).unwrap() {
        Startup::RolledBack(m) => assert_eq!(m.message.as_deref(), Some("readiness check failed")),
        other => panic!("expected RolledBack, got {other:?}"),
    }
    watchdog::clear(&fx.layout).unwrap();
}

#[tokio::test]
async fn deadline_and_crash_loops_roll_back() {
    let fx = Fixture::new("deadline", &good_binary());
    apply(&fx.request(), &fx.downloader(), &FakeHooks::default())
        .await
        .expect("apply");
    let mut m = watchdog::read(&fx.layout).unwrap().unwrap();
    let now = chrono::Utc::now().timestamp();
    // A crashing binary: every start counts.
    for _ in 0..m.max_attempts {
        assert!(matches!(
            watchdog::on_startup(&fx.layout, "0.2.0", now).unwrap(),
            Startup::Trial(_)
        ));
    }
    assert!(matches!(
        watchdog::on_startup(&fx.layout, "0.2.0", now).unwrap(),
        Startup::RestartNow(_)
    ));
    assert_eq!(fx.layout.current().unwrap(), fx.old);

    // Deadline: restore the trial state, then start after the deadline.
    fx.layout.point_current(&m.new_release).unwrap();
    m.phase = Phase::Restarting;
    m.attempts = 0;
    m.deadline = now - 1;
    watchdog::write(&fx.layout, &m).unwrap();
    assert!(matches!(
        watchdog::on_startup(&fx.layout, "0.2.0", now).unwrap(),
        Startup::RestartNow(_)
    ));
    assert_eq!(fx.layout.current().unwrap(), fx.old);
}

#[tokio::test]
async fn crash_mid_swap_is_recoverable() {
    let fx = Fixture::new("crash", &good_binary());
    let new = crate::support::install_release(fx.dir.path(), "0.2.0");
    let marker = watchdog::Marker {
        run_id: "run-1".into(),
        from_version: "0.1.0".into(),
        to_version: "0.2.0".into(),
        old_release: fx.old.clone(),
        new_release: new.clone(),
        prior_previous: None,
        phase: Phase::Swapping,
        deadline: 0,
        attempts: 0,
        max_attempts: 3,
        rollback_safe: true,
        message: None,
    };
    let now = chrono::Utc::now().timestamp();

    // Crash after `previous` moved but before `current` did: the supervisor
    // starts the old binary, which cleans up.
    watchdog::write(&fx.layout, &marker).unwrap();
    fx.layout.point_previous(Some(&fx.old)).unwrap();
    match watchdog::on_startup(&fx.layout, "0.1.0", now).unwrap() {
        Startup::RolledBack(m) => assert!(m.message.unwrap().contains("interrupted")),
        other => panic!("expected RolledBack, got {other:?}"),
    }
    assert_eq!(fx.layout.current().unwrap(), fx.old);
    assert!(fx.layout.previous().is_none());
    watchdog::clear(&fx.layout).unwrap();

    // Crash after `current` moved, during migrations: the new binary starts,
    // finds the marker, puts the old release back and asks to restart.
    let mut m = marker.clone();
    m.phase = Phase::Migrating;
    watchdog::write(&fx.layout, &m).unwrap();
    fx.layout.activate(&new).unwrap();
    assert_eq!(fx.layout.current().unwrap(), new);
    assert!(matches!(
        watchdog::on_startup(&fx.layout, "0.2.0", now).unwrap(),
        Startup::RestartNow(_)
    ));
    assert_eq!(fx.layout.current().unwrap(), fx.old);
    assert!(matches!(
        watchdog::on_startup(&fx.layout, "0.1.0", now).unwrap(),
        Startup::RolledBack(_)
    ));

    // A leftover marker blocks the next apply until it is dealt with.
    let err = apply(&fx.request(), &fx.downloader(), &FakeHooks::default())
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Preflight(_)), "{err}");
    watchdog::clear(&fx.layout).unwrap();
}

#[tokio::test]
async fn migration_failure_restores_the_old_release() {
    let fx = Fixture::new("migrate", &good_binary());
    let hooks = FakeHooks {
        fail_migrate: true,
        ..FakeHooks::default()
    };
    let err = apply(&fx.request(), &fx.downloader(), &hooks)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Migration(_)), "{err}");
    assert_eq!(fx.layout.current().unwrap(), fx.old);
    assert!(fx.layout.previous().is_none());
    assert!(watchdog::read(&fx.layout).unwrap().is_none());
    assert!(
        !hooks.maintenance.load(Ordering::SeqCst),
        "maintenance off again"
    );
    assert!(!hooks.restart.load(Ordering::SeqCst));
}

#[tokio::test]
async fn bad_artefacts_change_nothing() {
    // Wrong version reported by the new binary.
    let fx = Fixture::new("smoke", &fake_binary("9.9.9", &target()));
    let hooks = FakeHooks::default();
    let err = apply(&fx.request(), &fx.downloader(), &hooks)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::SmokeTest(_)), "{err}");
    assert_eq!(fx.layout.current().unwrap(), fx.old);
    assert!(!fx.dir.path().join("releases/0.2.0").exists());
    assert!(!hooks.maintenance.load(Ordering::SeqCst));

    // Built for another target.
    let fx = Fixture::new(
        "target",
        &fake_binary("0.2.0", "riscv64gc-unknown-linux-musl"),
    );
    let err = apply(&fx.request(), &fx.downloader(), &FakeHooks::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, UpdateError::SmokeTest(m) if m.contains("riscv64")),
        "{err}"
    );

    // Signature by a key that is not trusted.
    let fx = Fixture::new("badsig", &good_binary());
    let mut dl = fx.downloader();
    dl.signature = TestKey::generate().sign(&fx.tarball);
    let err = apply(&fx.request(), &dl, &FakeHooks::default())
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Signature(_)), "{err}");
    assert!(!fx.dir.path().join("releases/0.2.0").exists());

    // Backup failure stops before the swap.
    let fx = Fixture::new("backup", &good_binary());
    let hooks = FakeHooks {
        fail_backup: true,
        ..FakeHooks::default()
    };
    let err = apply(&fx.request(), &fx.downloader(), &hooks)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Backup(_)), "{err}");
    assert_eq!(fx.layout.current().unwrap(), fx.old);

    // No trusted key: nothing is installed.
    let fx = Fixture::new("nokey", &good_binary());
    let mut req = fx.request();
    req.ring = hos_update::KeyRing::empty();
    let err = apply(&req, &fx.downloader(), &FakeHooks::default())
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::NoTrustedKeys), "{err}");
}

#[tokio::test]
async fn unsafe_archives_are_refused() {
    let dir = TempDir::new("archive");
    let layout = Layout::new(dir.path());
    for (name, bytes) in [
        (
            "traversal",
            evil_tarball("../escape", tar::EntryType::Regular),
        ),
        (
            "absolute",
            evil_tarball("/tmp/hos-escape", tar::EntryType::Regular),
        ),
        ("symlink", evil_tarball("top/link", tar::EntryType::Symlink)),
    ] {
        let path = dir.path().join(format!("{name}.tar.gz"));
        write(&path, &bytes);
        let err = layout
            .unpack(&path, "0.2.0", ArchiveLimits::default())
            .unwrap_err();
        assert!(matches!(err, UpdateError::Archive(_)), "{name}: {err}");
    }
    assert!(!Path::new("/tmp/hos-escape").exists());
    assert!(!dir.path().join("escape").exists());
    // Missing binary.
    let path = dir.path().join("nobin.tar.gz");
    write(
        &path,
        &evil_tarball("top/static/x.css", tar::EntryType::Regular),
    );
    assert!(
        layout
            .unpack(&path, "0.2.0", ArchiveLimits::default())
            .is_err()
    );
    // Size limit.
    let path = dir.path().join("big.tar.gz");
    write(&path, &release_tarball("0.2.0", &good_binary()));
    let tiny = ArchiveLimits {
        max_bytes: 10,
        max_entries: 100,
    };
    assert!(layout.unpack(&path, "0.2.0", tiny).is_err());
    let leftovers: Vec<_> = std::fs::read_dir(layout.releases_dir())
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging directories removed: {leftovers:?}"
    );
}

#[tokio::test]
async fn concurrent_applies_are_blocked() {
    let fx = Fixture::new("concurrent", &good_binary());
    let lock = Arc::new(AtomicBool::new(false));
    let hold = Arc::new(Notify::new());
    let held = Arc::new(Notify::new());
    let first_hooks = Arc::new(FakeHooks {
        lock: lock.clone(),
        hold: Some(hold.clone()),
        held: Some(held.clone()),
        ..FakeHooks::default()
    });
    let req = fx.request();
    let dl = fx.downloader();
    let hooks = first_hooks.clone();
    let first = tokio::spawn(async move { apply(&req, &dl, hooks.as_ref()).await.map(|_| ()) });
    held.notified().await;

    // Another node (same advisory lock) is refused.
    let other_node = FakeHooks {
        lock: lock.clone(),
        ..FakeHooks::default()
    };
    let err = apply(&fx.request(), &fx.downloader(), &other_node)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Busy), "{err}");
    // A second process on this host (own DB connection) hits the file lock.
    let err = FileLock::acquire(&fx.layout.lock_path(), Duration::from_secs(3600)).unwrap_err();
    assert!(matches!(err, UpdateError::Busy), "{err}");

    hold.notify_one();
    first.await.unwrap().expect("first apply finishes");
    assert!(!lock.load(Ordering::SeqCst));
}
