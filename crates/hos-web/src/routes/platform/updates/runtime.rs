//! The self-update system inside the `hostingos` process (docs/UPDATES.md):
//!
//! * [`UpdateRuntime`] (`AppState::updates`): build info, the install type
//!   detected at start-up, the release source, the compiled-in keys, the
//!   cached maintenance flag;
//! * [`check_now`]: one update check, persisted (releases, problems,
//!   endorsed keys) with a notification when a newer version appears;
//! * [`spawn_background`]: the daily, jittered check (claimed through the
//!   database so one node checks per day) and the `current`-link watch that
//!   restarts a process whose release was replaced;
//! * [`start_apply`] / [`request_deploy`]: the in-place update of a
//!   bare-binary install ([`hos_update::apply`] with database hooks) and the
//!   deploy webhook of container installs;
//! * [`early_startup`] / [`late_startup`]: the watchdog handshake that
//!   confirms or rolls back a freshly installed release.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use hos_core::integrations::mailer::{EmailMessage, Mailbox};
use hos_db::repo::platform::PlatformAdminRow;
use hos_db::repo::updates::{self as repo, NewRun, ReleaseInput};
use hos_update::apply::{self, ApplyRequest, Hooks, Step};
use hos_update::backup::{self, BackupPlan};
use hos_update::check::{CheckReport, CheckRequest, Trust};
use hos_update::fetch::{Fetcher, NetConfig};
use hos_update::install::{InstallKind, Probe};
use hos_update::keys::{KeyOrigin, KeyRing, TrustedKey};
use hos_update::source::Source;
use hos_update::watchdog::{self, Startup};
use hos_update::webhook::{DeployWebhook, WebhookMethod, WebhookPayload};
use hos_update::{BuildInfo, Channel, UpdateError, UpdateResult};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::state::AppState;
use crate::worker::ShutdownSignal;

/// AAD of the encrypted deploy-webhook secret.
pub const WEBHOOK_SECRET_AAD: &[u8] = b"hostingos/update-webhook-secret/v1";

/// How long the new binary gets to become healthy (`HOS_UPDATE_HEALTH_TIMEOUT`, seconds).
const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(300);

/// How long running jobs may take to finish before an update gives up.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

/// How long `hostingos migrate` of the new release may run.
const MIGRATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Maintenance flag cache lifetime.
const MAINTENANCE_TTL_MS: u64 = 2000;

fn db_err(e: impl std::fmt::Display) -> UpdateError {
    UpdateError::Preflight(format!("database: {e}"))
}

/// The cluster-wide maintenance flag, cached for two seconds per process.
#[derive(Debug, Default)]
pub struct MaintenanceCache {
    on: AtomicBool,
    /// Milliseconds since `UpdateRuntime::started` of the last refresh + 1
    /// (0 = never).
    refreshed: AtomicU64,
    forced: RwLock<Option<bool>>,
}

/// Self-update runtime (one per process, `AppState::updates`).
#[derive(Debug)]
pub struct UpdateRuntime {
    /// This binary.
    pub build: BuildInfo,
    /// Host name (recorded with update runs).
    pub node: String,
    install: RwLock<InstallKind>,
    source: RwLock<Result<Option<Source>, String>>,
    builtin_keys: RwLock<Result<KeyRing, String>>,
    net_override: RwLock<Option<NetConfig>>,
    maintenance: MaintenanceCache,
    applying: AtomicBool,
    health_timeout: Duration,
    started: Instant,
}

fn node_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().chars().take(100).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

impl UpdateRuntime {
    /// Reads `HOS_UPDATE_*`, detects the install type, parses the
    /// compiled-in keys.
    pub fn from_env() -> Self {
        let source = Source::from_env().map_err(|e| e.to_string());
        if let Err(e) = &source {
            tracing::warn!(error = %e, "update source misconfigured; update checks are off");
        }
        let health_timeout = std::env::var("HOS_UPDATE_HEALTH_TIMEOUT")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| (30..=3600).contains(s))
            .map_or(DEFAULT_HEALTH_TIMEOUT, Duration::from_secs);
        Self {
            build: BuildInfo::current(),
            node: node_name(),
            install: RwLock::new(hos_update::install::detect(&Probe::from_system())),
            source: RwLock::new(source),
            builtin_keys: RwLock::new(KeyRing::built_in().map_err(|e| e.to_string())),
            net_override: RwLock::new(None),
            maintenance: MaintenanceCache::default(),
            applying: AtomicBool::new(false),
            health_timeout,
            started: Instant::now(),
        }
    }

    /// Demo mode (docs/DEMO.md): no release source, so the background loop
    /// never checks for, downloads or announces updates.
    pub fn disable_source(&self) {
        if let Ok(mut s) = self.source.write() {
            *s = Ok(None);
        }
    }

    /// Tests: replaces the source, the network settings (wiremock) and the
    /// install type.
    #[doc(hidden)]
    pub fn set_test_overrides(
        &self,
        source: Option<Source>,
        net: Option<NetConfig>,
        install: Option<InstallKind>,
    ) {
        if let Ok(mut s) = self.source.write() {
            *s = Ok(source);
        }
        if let Ok(mut n) = self.net_override.write() {
            *n = net;
        }
        if let (Some(kind), Ok(mut i)) = (install, self.install.write()) {
            *i = kind;
        }
    }

    /// Tests: replaces the compiled-in keys (keys generated by the test).
    #[doc(hidden)]
    pub fn set_test_keys(&self, ring: KeyRing) {
        if let Ok(mut k) = self.builtin_keys.write() {
            *k = Ok(ring);
        }
    }

    /// Tests: pins the maintenance flag of this process (`None` = database).
    #[doc(hidden)]
    pub fn force_maintenance(&self, on: Option<bool>) {
        if let Ok(mut f) = self.maintenance.forced.write() {
            *f = on;
        }
    }

    /// The install type detected at start-up.
    pub fn install(&self) -> InstallKind {
        self.install
            .read()
            .map(|i| i.clone())
            .unwrap_or_else(|_| InstallKind::Unknown {
                reason: "lock poisoned".into(),
            })
    }

    /// The release source (`Ok(None)`: not configured).
    pub fn source(&self) -> Result<Option<Source>, String> {
        self.source
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|_| Err("lock poisoned".into()))
    }

    /// The compiled-in keys (an error when `keys/trusted.pub` is malformed,
    /// which the build already rules out).
    pub fn builtin_keys(&self) -> Result<KeyRing, String> {
        self.builtin_keys
            .read()
            .map_err(|_| "lock poisoned".to_owned())
            .and_then(|k| k.clone())
    }

    /// Whether an update is being applied by this process.
    pub fn applying(&self) -> bool {
        self.applying.load(Ordering::SeqCst)
    }

    fn net_config(&self, hosts: Vec<String>) -> NetConfig {
        match self.net_override.read().ok().and_then(|n| n.clone()) {
            Some(mut cfg) => {
                cfg.allow(hosts);
                cfg
            }
            None => NetConfig::new(hosts),
        }
    }

    /// Download client for `source`.
    pub fn fetcher(&self, source: &Source) -> UpdateResult<Fetcher> {
        Fetcher::new(self.net_config(source.allowed_hosts()))
    }

    /// Client for deploy webhooks (no host allow-list, HTTPS only).
    pub fn webhook_fetcher(&self) -> UpdateResult<Fetcher> {
        Fetcher::new(self.net_config(Vec::new()))
    }

    /// Cached maintenance flag (database, refreshed every two seconds).
    pub async fn maintenance_on(&self, pool: &hos_db::PgPool) -> bool {
        if let Some(f) = self.maintenance.forced.read().ok().and_then(|f| *f) {
            return f;
        }
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX) + 1;
        let last = self.maintenance.refreshed.load(Ordering::Relaxed);
        let stale = last == 0 || now.saturating_sub(last) > MAINTENANCE_TTL_MS;
        if stale
            && self
                .maintenance
                .refreshed
                .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            match tokio::time::timeout(Duration::from_secs(2), repo::maintenance(pool)).await {
                Ok(Ok(on)) => self.maintenance.on.store(on, Ordering::Relaxed),
                Ok(Err(e)) => tracing::debug!(error = %e, "maintenance flag unavailable"),
                Err(_) => tracing::debug!("maintenance flag lookup timed out"),
            }
        }
        self.maintenance.on.load(Ordering::Relaxed)
    }

    /// Updates the cached flag right after this process changed it.
    pub fn set_maintenance_cache(&self, on: bool) {
        self.maintenance.on.store(on, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

/// Compiled-in keys plus the ones endorsed by verified manifests.
pub async fn trusted_ring(state: &AppState) -> UpdateResult<KeyRing> {
    let mut ring = state.updates.builtin_keys().map_err(UpdateError::Config)?;
    if ring.is_empty() {
        // Endorsements need a compiled-in root of trust.
        return Ok(ring);
    }
    for k in repo::endorsed_keys(&state.pool).await.map_err(db_err)? {
        let origin = KeyOrigin::Endorsed {
            by: k.endorsed_by.clone(),
            version: k.endorsed_in.clone(),
        };
        match TrustedKey::parse(&k.public_key, "endorsed", origin) {
            Ok(key) => {
                ring.add(key);
            }
            Err(e) => tracing::warn!(key = %k.key_id, error = %e, "ignoring a stored key"),
        }
    }
    Ok(ring)
}

// ---------------------------------------------------------------------------
// checking
// ---------------------------------------------------------------------------

/// Outcome of [`check_now`].
#[derive(Debug, Clone, Default)]
pub struct CheckSummary {
    /// Newer versions recorded.
    pub recorded: usize,
    /// Newest recorded version.
    pub newest: Option<String>,
    /// Releases whose manifest was rejected or unavailable.
    pub problems: usize,
}

/// Runs one update check and persists the result.
pub async fn check_now(state: &AppState) -> UpdateResult<CheckSummary> {
    let rt = &state.updates;
    let source = rt
        .source()
        .map_err(UpdateError::Config)?
        .ok_or(UpdateError::NotConfigured)?;
    let mut conn = state.pool.acquire().await.map_err(db_err)?;
    let st = repo::state(&mut conn).await.map_err(db_err)?;
    drop(conn);
    let channel = Channel::parse(&st.channel).unwrap_or_default();
    let ring = trusted_ring(state).await?;
    let seen: HashMap<String, chrono::DateTime<Utc>> = repo::seen_published(&state.pool)
        .await
        .map_err(db_err)?
        .into_iter()
        .collect();
    let fetcher = rt.fetcher(&source)?;
    let request = CheckRequest {
        source,
        channel,
        current: rt.build.semver(),
        ring,
        seen,
        now: Utc::now(),
    };
    match hos_update::check::run_check(&fetcher, &request).await {
        Ok(report) => {
            persist_report(
                state,
                &report,
                st.notified_version.as_deref(),
                st.notify_email.as_deref(),
            )
            .await
        }
        Err(e) => {
            if let Err(db) = repo::record_check(&state.pool, Some(&e.to_string()), &json!([])).await
            {
                tracing::warn!(error = %db, "recording the failed update check");
            }
            Err(e)
        }
    }
}

fn newer(a: &str, than: Option<&str>) -> bool {
    match (semver::Version::parse(a), than.map(semver::Version::parse)) {
        (Ok(a), Some(Ok(b))) => a > b,
        (Ok(_), _) => true,
        _ => false,
    }
}

async fn persist_report(
    state: &AppState,
    report: &CheckReport,
    notified: Option<&str>,
    notify_email: Option<&str>,
) -> UpdateResult<CheckSummary> {
    let mut keep = Vec::new();
    let mut problems = Vec::new();
    let mut any_unavailable = false;
    for f in &report.found {
        match (&f.trust, &f.manifest, &f.manifest_raw) {
            (Trust::Verified { .. } | Trust::Unverified, Some(m), Some(raw)) => {
                let detail = match &f.trust {
                    Trust::Verified { key_id } => Some(key_id.clone()),
                    _ => Some("no release signing key is built into this binary".to_owned()),
                };
                repo::upsert_release(
                    &state.pool,
                    &ReleaseInput {
                        version: m.version.clone(),
                        channel: m.channel.as_str().to_owned(),
                        trust: f.trust.as_str().to_owned(),
                        trust_detail: detail,
                        severity: m.severity.as_str().to_owned(),
                        published_at: m.published_at,
                        min_upgradable_from: m.min_upgradable_from.clone(),
                        migrations: m.migrations,
                        rollback_safe: m.rollback_safe,
                        blocked: f.blocked.clone(),
                        notes_md: m.notes_md.clone(),
                        container_image: m.container_image.clone(),
                        release_url: f.release_url.clone(),
                        manifest: raw.clone(),
                        signature: f.signature.clone(),
                    },
                )
                .await
                .map_err(db_err)?;
                keep.push(m.version.clone());
            }
            (trust, _, _) => {
                any_unavailable |= matches!(trust, Trust::Unavailable { .. });
                let reason = match trust {
                    Trust::Rejected { reason } | Trust::Unavailable { reason } => reason.clone(),
                    _ => String::new(),
                };
                problems.push(json!({
                    "version": f.version,
                    "trust": trust.as_str(),
                    "reason": reason,
                }));
            }
        }
    }
    if !any_unavailable {
        repo::retain_releases(&state.pool, &keep)
            .await
            .map_err(db_err)?;
    }
    for k in &report.endorsed {
        if let KeyOrigin::Endorsed { by, version } = &k.origin {
            repo::add_endorsed_key(&state.pool, &k.key_id, &k.public_key, by, version)
                .await
                .map_err(db_err)?;
            tracing::warn!(key = %k.key_id, by = %by, version = %version, "release signing key endorsed");
        }
    }
    for id in &report.revoked {
        repo::revoke_key(&state.pool, id).await.map_err(db_err)?;
        tracing::warn!(key = %id, "release signing key revoked");
    }
    repo::record_check(&state.pool, None, &json!(problems))
        .await
        .map_err(db_err)?;

    let newest = report
        .found
        .iter()
        .find(|f| f.manifest.is_some() && f.blocked.is_none());
    if let Some(n) = newest
        && let Some(m) = &n.manifest
        && newer(&m.version, notified)
    {
        let data = json!({
            "version": m.version,
            "severity": m.severity.as_str(),
            "trust": n.trust.as_str(),
        });
        let dedupe = format!("update.available:{}", m.version);
        repo::notify_platform(
            &state.pool,
            "update.available",
            &data,
            Some("/platform/updates"),
            Some(&dedupe),
        )
        .await
        .map_err(db_err)?;
        repo::set_notified_version(&state.pool, &m.version)
            .await
            .map_err(db_err)?;
        // Only a signed release is announced by e-mail: in unsigned mode
        // (no key built in) the manifest is untrusted text from the release
        // source and stays on the console page.
        if let Some(to) = notify_email.filter(|e| !e.trim().is_empty())
            && matches!(n.trust, Trust::Verified { .. })
        {
            send_available_mail(state, to, &m.version, m.severity.as_str()).await;
        }
    }
    Ok(CheckSummary {
        recorded: keep.len(),
        newest: newest.map(|f| f.version.clone()),
        problems: problems.len(),
    })
}

async fn send_available_mail(state: &AppState, to: &str, version: &str, severity: &str) {
    let link = format!(
        "{}{}",
        state.config.base_url.as_str().trim_end_matches('/'),
        crate::mode::console_link(state.web.single(), "/platform/updates")
    );
    let security = severity == "security";
    let subject = if security {
        format!("HostingOS {version}: security update available")
    } else {
        format!("HostingOS {version} is available")
    };
    let text = format!(
        "HostingOS {version} is available{}.\nReview the release notes and install it from {link}\n\n\
         HostingOS {version} sürümü yayımlandı{}.\nSürüm notlarını inceleyip güncellemeyi buradan kurabilirsiniz: {link}\n",
        if security { " (security fix)" } else { "" },
        if security {
            " (güvenlik düzeltmesi)"
        } else {
            ""
        },
    );
    let esc = crate::view::escape_html;
    let html = text
        .lines()
        .map(|l| format!("<p>{}</p>", esc(l)))
        .collect::<String>();
    let message = EmailMessage {
        to: vec![Mailbox::new(to)],
        subject,
        text_body: Some(text),
        html_body: Some(html),
        tags: vec!["platform_update".into()],
        ..EmailMessage::default()
    };
    if let Err(e) = state.mailer.send(&message).await {
        tracing::warn!(error = %e, "sending the update notification e-mail failed");
    }
}

// ---------------------------------------------------------------------------
// background loop
// ---------------------------------------------------------------------------

fn jitter_secs(max: u64) -> u64 {
    let b = hos_core::crypto::random_bytes::<8>();
    u64::from_le_bytes(b) % max.max(1)
}

/// Starts the update loop: the daily jittered check (one node per day,
/// claimed in `update_state.next_check_at`) and, on binary installs, the
/// `current`-link watch.
pub fn spawn_background(state: &AppState, signal: &ShutdownSignal) -> JoinHandle<()> {
    let state = state.clone();
    let signal = signal.clone();
    tokio::spawn(async move {
        let first = Duration::from_secs(30 + jitter_secs(120));
        tokio::select! {
            () = signal.clone().wait() => return,
            () = tokio::time::sleep(first) => {}
        }
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = signal.clone().wait() => break,
                _ = tick.tick() => {}
            }
            scheduled_check(&state).await;
            watch_current_link(&state);
        }
        tracing::debug!("update loop stopped");
    })
}

async fn scheduled_check(state: &AppState) {
    if !matches!(state.updates.source(), Ok(Some(_))) {
        return;
    }
    let now = Utc::now();
    let next = now
        + chrono::Duration::hours(24)
        + chrono::Duration::seconds(i64::try_from(jitter_secs(2 * 3600)).unwrap_or(0));
    match repo::claim_scheduled_check(&state.pool, now, next).await {
        Ok(true) => match check_now(state).await {
            Ok(s) => {
                tracing::info!(recorded = s.recorded, newest = ?s.newest, problems = s.problems, "update check done")
            }
            Err(e) => tracing::warn!(error = %e, "update check failed"),
        },
        Ok(false) => {}
        Err(e) => tracing::debug!(error = %e, "update check not claimed"),
    }
}

/// Restarts this process when another process installed a new release
/// into the `current` link (multi-process hosts, CLI installs, rollbacks).
fn watch_current_link(state: &AppState) {
    let InstallKind::Binary(b) = state.updates.install() else {
        return;
    };
    if state.updates.applying() || hos_update::restart::requested().is_some() {
        return;
    }
    let layout = b.layout();
    let Ok(current) = layout.current() else {
        return;
    };
    if current == b.release {
        return;
    }
    let settled = match watchdog::read(&layout) {
        Ok(None) => true,
        Ok(Some(m)) => matches!(
            m.phase,
            watchdog::Phase::Restarting | watchdog::Phase::RolledBack
        ),
        Err(_) => false,
    };
    if settled {
        hos_update::restart::request(format!(
            "the active release changed to {}",
            current.display()
        ));
    }
}

// ---------------------------------------------------------------------------
// applying (bare binary)
// ---------------------------------------------------------------------------

/// Where an apply runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// From the console: the serving process exits for its supervisor.
    Service,
    /// `hostingos update apply`: the service notices the new `current` link
    /// and restarts itself.
    Cli,
}

/// Who asked.
#[derive(Debug, Clone)]
pub struct Requester {
    /// Operator id.
    pub id: Option<hos_core::ids::PlatformAdminId>,
    /// Operator e-mail (or `cli`).
    pub email: String,
}

impl Requester {
    /// From an operator row.
    pub fn admin(a: &PlatformAdminRow) -> Self {
        Self {
            id: Some(a.id),
            email: a.email.clone(),
        }
    }

    /// Whoever acts on the update page: the operator, or in single-company
    /// mode the owner (shown by e-mail, filed under the internal operator
    /// record).
    pub fn of(a: &crate::extract::PlatformAdmin) -> Self {
        let mut who = Self::admin(&a.admin);
        if let Some(staff) = &a.staff {
            who.email = staff.email.clone();
        }
        who
    }
}

/// A checked apply, ready to start.
pub struct Prepared {
    request: ApplyRequest,
    plan: BackupPlan,
}

/// Validates an install of `version` (install type, keys, re-verified
/// manifest, target, backup).
pub async fn prepare_apply(
    state: &AppState,
    version: &str,
    backup_confirmed: bool,
    mode: ApplyMode,
) -> UpdateResult<Prepared> {
    let rt = &state.updates;
    let binary = match rt.install() {
        InstallKind::Binary(b) => b,
        InstallKind::Container(kind) => {
            return Err(UpdateError::Unsupported(format!(
                "this is a {} install: the image is immutable, deploy the new image instead",
                kind.as_str()
            )));
        }
        InstallKind::Unknown { reason } => {
            return Err(UpdateError::Unsupported(format!(
                "in-place updates need the release layout of docs/deploy/install.sh ({reason})"
            )));
        }
    };
    if mode == ApplyMode::Service && !binary.supervised {
        return Err(UpdateError::Unsupported(
            "no supervisor detected that restarts hostingos when it exits (systemd sets INVOCATION_ID; otherwise set HOS_SUPERVISED=1)".into(),
        ));
    }
    let ring = trusted_ring(state).await?;
    if ring.is_empty() {
        return Err(UpdateError::NoTrustedKeys);
    }
    let release = repo::get_release(&state.pool, version)
        .await
        .map_err(db_err)?
        .ok_or_else(|| {
            UpdateError::Config(format!("version {version} is not known; run a check first"))
        })?;
    let signature = release
        .signature
        .as_deref()
        .ok_or_else(|| UpdateError::Signature("no signature recorded".into()))?;
    // The table is not trusted: verify the exact signed bytes again.
    let verified =
        hos_update::verify::verify_signed(release.manifest.as_bytes(), signature, &ring)?;
    let mut conn = state.pool.acquire().await.map_err(db_err)?;
    let st = repo::state(&mut conn).await.map_err(db_err)?;
    drop(conn);
    let policy = hos_update::verify::VerifyPolicy::new(
        rt.build.semver(),
        Channel::parse(&st.channel).unwrap_or_default(),
    );
    hos_update::verify::check_offer(&verified.manifest, &policy)?;
    verified.manifest.artifact_for(&rt.build.target)?;
    let plan = BackupPlan::from_env(&binary.layout().backups_dir());
    if plan == BackupPlan::Manual && !backup_confirmed {
        return Err(UpdateError::Preflight(
            "no automatic database backup is configured (HOS_UPDATE_BACKUP_COMMAND or pg_dump); confirm that you have a current backup".into(),
        ));
    }
    let mut request = ApplyRequest::new("", binary.layout(), verified.manifest, ring);
    request.health_timeout = rt.health_timeout;
    Ok(Prepared { request, plan })
}

struct DbHooks {
    state: AppState,
    run_id: Uuid,
    plan: BackupPlan,
    label: String,
    mode: ApplyMode,
}

struct LockGuard(#[allow(dead_code)] hos_db::PgConnection);

#[async_trait]
impl Hooks for DbHooks {
    async fn step(&self, step: Step, detail: &str) {
        tracing::info!(run = %self.run_id, step = step.as_str(), detail, "update step");
        if let Err(e) = repo::log_step(&self.state.pool, self.run_id, step.as_str(), detail).await {
            tracing::warn!(error = %e, "logging an update step");
        }
    }

    async fn lock(&self) -> UpdateResult<Box<dyn Send>> {
        let mut conn = self.state.pool.acquire().await.map_err(db_err)?.detach();
        if !repo::try_update_lock(&mut conn).await.map_err(db_err)? {
            return Err(UpdateError::Busy);
        }
        Ok(Box::new(LockGuard(conn)))
    }

    async fn check_database(&self) -> Result<(), String> {
        repo::ping(&self.state.pool)
            .await
            .map_err(|e| e.to_string())
    }

    async fn enter_maintenance(&self) -> Result<(), String> {
        let reason = format!("update to {}", self.label);
        repo::set_maintenance(&self.state.pool, true, Some(&reason))
            .await
            .map_err(|e| e.to_string())?;
        self.state.updates.set_maintenance_cache(true);
        Ok(())
    }

    async fn drain_jobs(&self) -> Result<(), String> {
        let until = Instant::now() + DRAIN_TIMEOUT;
        loop {
            let running = repo::running_jobs(&self.state.pool)
                .await
                .map_err(|e| e.to_string())?;
            if running == 0 {
                return Ok(());
            }
            if Instant::now() > until {
                return Err(format!(
                    "{running} job(s) still running after {} s",
                    DRAIN_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn leave_maintenance(&self) {
        if let Err(e) = repo::set_maintenance(&self.state.pool, false, None).await {
            tracing::error!(error = %e, "turning maintenance mode off failed");
        }
        self.state.updates.set_maintenance_cache(false);
    }

    async fn backup(&self) -> Result<String, String> {
        let url = self.state.config.database_url_direct.expose().to_owned();
        let url = hos_core::config::strip_channel_binding(&url);
        let label = format!(
            "{}-to-{}-{}",
            self.state.updates.build.version,
            self.label,
            Utc::now().format("%Y%m%dT%H%M%SZ")
        );
        let done = backup::run(&self.plan, &url, &label)
            .await
            .map_err(|e| e.to_string())?;
        if let Err(e) = repo::set_run_backup(&self.state.pool, self.run_id, &done).await {
            tracing::warn!(error = %e, "recording the backup");
        }
        Ok(done)
    }

    async fn migrate(&self, binary: &Path) -> Result<(), String> {
        let mut cmd = tokio::process::Command::new(binary);
        cmd.arg("migrate").kill_on_drop(true);
        let out = tokio::time::timeout(MIGRATE_TIMEOUT, cmd.output())
            .await
            .map_err(|_| "hostingos migrate timed out".to_owned())?
            .map_err(|e| format!("starting hostingos migrate: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        Err(format!(
            "hostingos migrate exited with {}: {}",
            out.status,
            tail[tail.len().saturating_sub(3)..].join(" | ")
        ))
    }

    async fn request_restart(&self) {
        if let Err(e) =
            repo::set_run_status(&self.state.pool, self.run_id, "restarting", None).await
        {
            tracing::warn!(error = %e, "recording the restart");
        }
        match self.mode {
            ApplyMode::Service => hos_update::restart::request(format!("update to {}", self.label)),
            ApplyMode::Cli => tracing::info!(
                "release switched; the running service restarts itself within a minute (or restart it now)"
            ),
        }
    }
}

/// Records the run and starts the apply. `Service`: in the background,
/// returns at once; `Cli`: waits for the result.
pub async fn start_apply(
    state: &AppState,
    prepared: Prepared,
    who: &Requester,
    mode: ApplyMode,
) -> UpdateResult<Uuid> {
    let rt = &state.updates;
    let to = prepared.request.manifest.version.clone();
    let run = repo::create_run(
        &state.pool,
        &NewRun {
            kind: "binary",
            from_version: &rt.build.version,
            to_version: &to,
            status: "running",
            requested_by: who.id,
            requested_by_email: Some(&who.email),
            node: &rt.node,
        },
    )
    .await
    .map_err(|e| match e {
        hos_core::CoreError::Conflict(_) => UpdateError::Busy,
        other => db_err(other),
    })?;
    let mut request = prepared.request;
    request.run_id = run.id.to_string();
    let hooks = DbHooks {
        state: state.clone(),
        run_id: run.id,
        plan: prepared.plan,
        label: to.clone(),
        mode,
    };
    rt.applying.store(true, Ordering::SeqCst);
    let state2 = state.clone();
    let task = async move {
        let fetcher = match source_fetcher(&state2) {
            Ok(f) => f,
            Err(e) => return finish_failed(&state2, run.id, &e).await,
        };
        match apply::apply(&request, &fetcher, &hooks).await {
            Ok(done) => {
                tracing::info!(release = %done.release.display(), backup = %done.backup, "update installed; restarting")
            }
            Err(e) => finish_failed(&state2, run.id, &e).await,
        }
    };
    match mode {
        ApplyMode::Service => {
            tokio::spawn(task);
        }
        ApplyMode::Cli => task.await,
    }
    Ok(run.id)
}

fn source_fetcher(state: &AppState) -> UpdateResult<Fetcher> {
    let source = state
        .updates
        .source()
        .map_err(UpdateError::Config)?
        .ok_or(UpdateError::NotConfigured)?;
    state.updates.fetcher(&source)
}

async fn finish_failed(state: &AppState, run: Uuid, e: &UpdateError) {
    state.updates.applying.store(false, Ordering::SeqCst);
    tracing::error!(run = %run, error = %e, kind = e.kind(), "update failed");
    let msg = format!("{}: {e}", e.kind());
    if let Err(db) = repo::set_run_status(&state.pool, run, "failed", Some(&msg)).await {
        tracing::error!(error = %db, "recording the failed update");
    }
    notify_run(state, "update.failed", run).await;
}

async fn notify_run(state: &AppState, kind: &str, run: Uuid) {
    let Ok(Some(r)) = repo::get_run(&state.pool, run).await else {
        return;
    };
    let data = json!({
        "version": r.to_version,
        "from": r.from_version,
        "message": r.message,
    });
    let dedupe = format!("{kind}:{run}");
    let link = format!("/platform/updates/runs/{run}");
    if let Err(e) =
        repo::notify_platform(&state.pool, kind, &data, Some(&link), Some(&dedupe)).await
    {
        tracing::warn!(error = %e, "update notification");
    }
}

// ---------------------------------------------------------------------------
// container installs: deploy webhook
// ---------------------------------------------------------------------------

/// Calls the configured deploy webhook for `version` and records a run.
pub async fn request_deploy(
    state: &AppState,
    version: &str,
    who: &Requester,
) -> UpdateResult<Uuid> {
    let rt = &state.updates;
    let mut conn = state.pool.acquire().await.map_err(db_err)?;
    let st = repo::state(&mut conn).await.map_err(db_err)?;
    drop(conn);
    let url = st
        .webhook_url
        .clone()
        .ok_or_else(|| UpdateError::Config("no deploy webhook is configured".into()))?;
    let release = repo::get_release(&state.pool, version)
        .await
        .map_err(db_err)?
        .ok_or_else(|| {
            UpdateError::Config(format!("version {version} is not known; run a check first"))
        })?;
    if !newer(&release.version, Some(&rt.build.version)) {
        return Err(UpdateError::NotNewer {
            offered: release.version,
            current: rt.build.version.clone(),
        });
    }
    if release.trust != "verified" {
        return Err(UpdateError::Signature(
            "the release manifest is not signature-verified".into(),
        ));
    }
    if let Some(reason) = release.blocked {
        return Err(UpdateError::Refused(reason));
    }
    let secret =
        match &st.webhook_secret_enc {
            Some(enc) => Some(
                String::from_utf8(state.secret_box.decrypt(enc, WEBHOOK_SECRET_AAD).map_err(
                    |_| {
                        UpdateError::Config(
                            "the webhook secret cannot be decrypted (HOS_SECRET_KEY changed?)"
                                .into(),
                        )
                    },
                )?)
                .map_err(|_| UpdateError::Config("the webhook secret is not text".into()))?,
            ),
            None => None,
        };
    let fetcher = rt.webhook_fetcher()?;
    let hook = DeployWebhook {
        url: hos_update::webhook::validate_url(&fetcher, &url)?,
        method: WebhookMethod::parse(&st.webhook_method).unwrap_or_default(),
        secret,
    };
    let payload = WebhookPayload::new(
        &release.version,
        release.container_image.as_deref(),
        &release.channel,
    );
    let result = hos_update::webhook::call(&fetcher, &hook, &payload).await;
    let (status, message) = match &result {
        Ok(code) => ("requested", format!("deploy webhook answered HTTP {code}")),
        Err(e) => ("failed", format!("{}: {e}", e.kind())),
    };
    let run = repo::create_run(
        &state.pool,
        &NewRun {
            kind: "webhook",
            from_version: &rt.build.version,
            to_version: &release.version,
            status,
            requested_by: who.id,
            requested_by_email: Some(&who.email),
            node: &rt.node,
        },
    )
    .await
    .map_err(db_err)?;
    let _ = repo::set_run_status(&state.pool, run.id, status, Some(&message)).await;
    let _ = repo::log_step(&state.pool, run.id, "webhook", &message).await;
    result.map(|_| run.id)
}

// ---------------------------------------------------------------------------
// start-up handshake
// ---------------------------------------------------------------------------

/// What [`early_startup`] found.
#[derive(Debug)]
pub enum Early {
    /// Go on (with a watchdog result for [`late_startup`]).
    Continue(Option<Startup>),
    /// A rollback was just performed: exit with the restart code at once.
    Exit(String),
}

/// Before anything that can fail (configuration, database): the watchdog
/// marker of a bare-binary install. Counts this start when it is the trial
/// of a new release and rolls back after too many starts / the deadline.
pub fn early_startup() -> Early {
    let InstallKind::Binary(b) = hos_update::install::detect(&Probe::from_system()) else {
        return Early::Continue(None);
    };
    let layout = b.layout();
    match watchdog::on_startup(
        &layout,
        hos_update::build_info::VERSION,
        Utc::now().timestamp(),
    ) {
        Ok(Startup::RestartNow(m)) => Early::Exit(
            m.message
                .unwrap_or_else(|| "the previous release was restored".into()),
        ),
        Ok(Startup::Clean) => Early::Continue(None),
        Ok(other) => {
            tracing::warn!(?other, "update in flight");
            Early::Continue(Some(other))
        }
        Err(e) => {
            tracing::error!(error = %e, "reading the update marker failed");
            Early::Continue(None)
        }
    }
}

/// After the state exists: cleans up interrupted runs and, for a trial,
/// checks health (database, migrations, `/readyz` on `http`) and confirms
/// or rolls back.
pub fn late_startup(
    state: &AppState,
    early: Option<Startup>,
    http: Option<SocketAddr>,
) -> JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        let rt = &state.updates;
        let stale = Utc::now() - chrono::Duration::hours(3);
        if let Err(e) = repo::fail_interrupted_runs(&state.pool, &rt.node, stale).await {
            tracing::debug!(error = %e, "update runs cleanup");
        }
        let _ = repo::complete_requested_runs(&state.pool, &rt.build.version).await;
        match early {
            Some(Startup::Trial(m)) => trial(&state, m, http).await,
            Some(Startup::RolledBack(m)) => {
                record_rollback(&state, &m).await;
                if let InstallKind::Binary(b) = rt.install()
                    && let Err(e) = watchdog::clear(&b.layout())
                {
                    tracing::error!(error = %e, "removing the update marker");
                }
            }
            _ => release_stuck_maintenance(&state).await,
        }
    })
}

async fn release_stuck_maintenance(state: &AppState) {
    if matches!(repo::maintenance(&state.pool).await, Ok(true))
        && matches!(repo::active_run(&state.pool).await, Ok(None))
    {
        tracing::warn!("maintenance mode was on without an active update; turning it off");
        let _ = repo::set_maintenance(&state.pool, false, None).await;
        state.updates.set_maintenance_cache(false);
    }
}

async fn record_rollback(state: &AppState, m: &watchdog::Marker) {
    let message = m.message.clone().unwrap_or_else(|| "rolled back".into());
    let message = if m.rollback_safe {
        message
    } else {
        format!(
            "{message}. This release was not marked rollback-safe: if its migrations ran, restore the database backup taken before the update."
        )
    };
    tracing::error!(from = %m.from_version, to = %m.to_version, %message, "update rolled back");
    if let Ok(run) = m.run_id.parse::<Uuid>() {
        let _ = repo::set_run_status(&state.pool, run, "rolled_back", Some(&message)).await;
        let _ = repo::log_step(&state.pool, run, "rollback", &message).await;
        notify_run(state, "update.rolled_back", run).await;
    }
    let _ = repo::set_maintenance(&state.pool, false, None).await;
    state.updates.set_maintenance_cache(false);
}

/// Health of a freshly started release.
pub async fn health(state: &AppState, http: Option<SocketAddr>) -> Result<(), String> {
    repo::ping(&state.pool)
        .await
        .map_err(|e| format!("database: {e}"))?;
    let applied = repo::applied_migrations(&state.pool)
        .await
        .map_err(|e| format!("migrations: {e}"))?;
    let missing: Vec<i64> = hos_db::MIGRATOR
        .iter()
        .map(|m| m.version)
        .filter(|v| !applied.contains(v))
        .collect();
    if !missing.is_empty() {
        return Err(format!("migrations not applied: {missing:?}"));
    }
    if let Some(addr) = http {
        readyz(addr).await?;
    }
    Ok(())
}

async fn readyz(addr: SocketAddr) -> Result<(), String> {
    let target = match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, addr.port()))
        }
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, addr.port()))
        }
        _ => addr,
    };
    let io = async {
        let mut s = tokio::net::TcpStream::connect(target).await?;
        s.write_all(b"GET /readyz HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await?;
        let mut buf = vec![0u8; 64];
        let n = s.read(&mut buf).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf[..n]).into_owned())
    };
    let head = tokio::time::timeout(Duration::from_secs(5), io)
        .await
        .map_err(|_| "readyz timed out".to_owned())?
        .map_err(|e| format!("readyz: {e}"))?;
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(format!(
            "readyz answered {}",
            head.lines().next().unwrap_or_default()
        ))
    }
}

async fn trial(state: &AppState, m: watchdog::Marker, http: Option<SocketAddr>) {
    let InstallKind::Binary(b) = state.updates.install() else {
        return;
    };
    let layout = b.layout();
    let now = Utc::now().timestamp();
    let budget = u64::try_from((m.deadline - now).max(30)).unwrap_or(30);
    let until = Instant::now() + Duration::from_secs(budget);
    let mut last = String::new();
    while Instant::now() < until {
        match health(state, http).await {
            Ok(()) => {
                match watchdog::confirm_healthy(&layout) {
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "removing the update marker"),
                }
                if let Ok(run) = m.run_id.parse::<Uuid>() {
                    let msg = format!("{} is running and healthy", m.to_version);
                    let _ = repo::log_step(&state.pool, run, "healthy", &msg).await;
                    let _ = repo::set_run_status(&state.pool, run, "succeeded", Some(&msg)).await;
                    notify_run(state, "update.succeeded", run).await;
                }
                let _ = repo::set_maintenance(&state.pool, false, None).await;
                state.updates.set_maintenance_cache(false);
                match layout.prune(2) {
                    Ok(removed) if !removed.is_empty() => {
                        tracing::info!(count = removed.len(), "old releases removed");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "pruning old releases"),
                }
                tracing::info!(version = %m.to_version, "update confirmed healthy");
                return;
            }
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let reason = format!("the new release did not become healthy in time ({last}); rolled back");
    tracing::error!(%reason, "update trial failed");
    match watchdog::fail_trial(&layout, &reason) {
        Ok(_) => {
            if let Ok(run) = m.run_id.parse::<Uuid>() {
                let _ = repo::log_step(&state.pool, run, "rollback", &reason).await;
            }
            hos_update::restart::request("rolling back a failed update");
        }
        Err(e) => tracing::error!(error = %e, "rolling back failed"),
    }
}
