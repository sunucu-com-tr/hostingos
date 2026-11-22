//! Background runtime of the `hostingos` binary.
//!
//! * [`spawn`] starts the `hos-jobs` [`Worker`] (job queue: provisioning,
//!   e-mail delivery, webhooks out, …), the cron [`Scheduler`] and the
//!   platform [`maintenance`] loop (custom-domain DNS verification retries,
//!   expired `Idempotency-Key` cleanup) and the update check loop. The
//!   worker pauses while an update holds maintenance mode. `hostingos serve` runs them next to
//!   the HTTP server unless `--no-worker`; `hostingos worker` runs only them.
//! * [`Shutdown`] is the shared graceful-shutdown switch (SIGTERM / SIGINT):
//!   the server stops accepting, the worker stops claiming jobs and hands
//!   unfinished ones back within `shutdown_grace`.
//! * [`run_once`] (`hostingos jobs run-once`) ticks the scheduler once and
//!   drains every due job.
//!
//! The worker `LISTEN`s on `DATABASE_URL_DIRECT` (a transaction pooler
//! cannot carry `LISTEN`); polling remains the fallback.

use std::time::Duration;

use hos_jobs::{RunStats, Scheduler, SchedulerConfig, Worker, WorkerConfig};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::state::AppState;

/// Sender side of the shutdown switch.
#[derive(Debug)]
pub struct Shutdown(watch::Sender<bool>);

/// Receiver side: resolves once shutdown was requested.
#[derive(Debug, Clone)]
pub struct ShutdownSignal(watch::Receiver<bool>);

impl Shutdown {
    /// A new switch and its first listener.
    pub fn new() -> (Self, ShutdownSignal) {
        let (tx, rx) = watch::channel(false);
        (Self(tx), ShutdownSignal(rx))
    }

    /// Requests shutdown (idempotent).
    pub fn trigger(&self) {
        let _ = self.0.send(true);
    }

    /// Another listener.
    pub fn signal(&self) -> ShutdownSignal {
        ShutdownSignal(self.0.subscribe())
    }
}

impl ShutdownSignal {
    /// Waits for the shutdown request.
    pub async fn wait(mut self) {
        let _ = self.0.wait_for(|stop| *stop).await;
    }

    /// Whether shutdown was requested.
    pub fn is_triggered(&self) -> bool {
        *self.0.borrow()
    }
}

/// What [`spawn`] starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundOptions {
    /// The job worker.
    pub worker: bool,
    /// The cron scheduler.
    pub scheduler: bool,
    /// Custom-domain checks and cleanup.
    pub maintenance: bool,
}

impl Default for BackgroundOptions {
    fn default() -> Self {
        Self {
            worker: true,
            scheduler: true,
            maintenance: true,
        }
    }
}

/// Worker settings: `LISTEN` on the direct database URL, concurrency from
/// `HOS_WORKER_CONCURRENCY` (default 8).
pub fn worker_config(state: &AppState) -> WorkerConfig {
    let direct = hos_core::config::strip_channel_binding(state.config.database_url_direct.expose());
    let concurrency = std::env::var("HOS_WORKER_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| (1..=256).contains(n))
        .unwrap_or(8);
    WorkerConfig {
        listen_url: Some(direct),
        concurrency,
        ..WorkerConfig::default()
    }
}

/// Starts the background tasks; each ends when `signal` fires.
pub fn spawn(
    state: &AppState,
    options: BackgroundOptions,
    signal: &ShutdownSignal,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    if options.worker {
        // Paused while an update holds maintenance mode (docs/UPDATES.md).
        handles.push(tokio::spawn(
            crate::routes::platform::updates::maintenance::run_worker(
                state.clone(),
                signal.clone(),
            ),
        ));
        // WHMCS imports (queued from /admin/tools/import or the CLI).
        let imports = hos_import::runner::Runner::new(
            state.pool.clone(),
            state.secret_box.clone(),
            hos_core::net::OutboundPolicy::global(),
            format!("import-{}", uuid::Uuid::now_v7()),
        );
        handles.push(tokio::spawn(imports.run_loop(signal.clone().wait())));
    }
    if options.scheduler {
        let scheduler = Scheduler::new(state.pool.clone(), SchedulerConfig::default());
        let stop = signal.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = scheduler.run(stop.wait()).await {
                tracing::error!(error = %e, "scheduler stopped with an error");
            }
        }));
    }
    if options.maintenance {
        handles.push(tokio::spawn(maintenance(state.clone(), signal.clone())));
        // Daily update check + `current`-link watch (docs/UPDATES.md).
        handles.push(crate::routes::platform::updates::runtime::spawn_background(
            state, signal,
        ));
    }
    tracing::info!(
        worker = options.worker,
        scheduler = options.scheduler,
        maintenance = options.maintenance,
        "background runtime started"
    );
    handles
}

/// Waits for background tasks after shutdown was triggered (bounded).
pub async fn join(handles: Vec<JoinHandle<()>>, limit: Duration) {
    let all = futures::future::join_all(handles);
    if tokio::time::timeout(limit, all).await.is_err() {
        tracing::warn!("background tasks did not stop in time");
    }
}

/// Interval of the maintenance loop.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(300);

/// Platform maintenance: re-checks unverified custom domains (each at most
/// every 15 minutes), purges expired idempotency keys and ticket reply tokens.
pub async fn maintenance(state: AppState, signal: ShutdownSignal) {
    let mut tick = tokio::time::interval(MAINTENANCE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = signal.clone().wait() => break,
            _ = tick.tick() => {}
        }
        maintenance_pass(&state).await;
    }
    tracing::info!("maintenance loop stopped");
}

async fn maintenance_pass(state: &AppState) {
    match crate::routes::platform::domains::check_due_domains(state, 50).await {
        Ok(n) if n > 0 => tracing::info!(checked = n, "custom domains checked"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "custom domain checks failed"),
    }
    match hos_db::repo::idempotency::purge_expired(&state.pool).await {
        Ok(n) if n > 0 => tracing::debug!(purged = n, "expired idempotency keys purged"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "purging idempotency keys failed"),
    }
    match hos_db::repo::mail_inbound::prune_reply_tokens(&state.pool, chrono::Utc::now()).await {
        Ok(n) if n > 0 => tracing::debug!(pruned = n, "expired ticket reply tokens pruned"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "pruning ticket reply tokens failed"),
    }
}

/// One pass: default scheduled tasks ensured, due tasks enqueued, every due
/// job run (up to `max_jobs`), custom domains checked.
pub async fn run_once(state: &AppState, max_jobs: usize) -> anyhow::Result<RunStats> {
    let scheduler = Scheduler::new(state.pool.clone(), SchedulerConfig::default());
    scheduler
        .ensure_default_tasks()
        .await
        .map_err(|e| anyhow::anyhow!("ensuring scheduled tasks: {e}"))?;
    let tick = scheduler
        .tick()
        .await
        .map_err(|e| anyhow::anyhow!("scheduler tick: {e}"))?;
    tracing::info!(enqueued = tick.enqueued, "scheduler ticked");
    let worker = Worker::new(
        state.pool.clone(),
        state.billing.clone(),
        WorkerConfig {
            listen_url: None,
            ..worker_config(state)
        },
    );
    let stats = worker
        .run_until_idle(max_jobs)
        .await
        .map_err(|e| anyhow::anyhow!("running jobs: {e}"))?;
    maintenance_pass(state).await;
    Ok(stats)
}
