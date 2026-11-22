//! The job worker: claims due jobs (`FOR UPDATE SKIP LOCKED`), runs them with
//! bounded concurrency, a per-job timeout and a heartbeat, and records the
//! outcome (completed / retry with exponential backoff + jitter / dead).
//!
//! Wake-ups come from `LISTEN hf_jobs` (enqueue does `pg_notify` on commit)
//! when a direct database URL is configured — `LISTEN` does not work through
//! a transaction-mode pooler — and from polling otherwise (and as a safety
//! net). Jobs whose worker died are returned to the queue by the stuck-job
//! recovery. On shutdown the worker stops claiming, waits up to
//! `shutdown_grace` for running jobs and hands unfinished ones back.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use hos_billing::Billing;
use hos_billing::jobs::{NOTIFY_CHANNEL, queues};
use hos_core::CoreError;
use hos_core::ids::TenantId;
use hos_db::PgPool;
use hos_db::repo::billing_support;
use hos_db::repo::jobs::{self as repo, JobRow, Retry};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::handlers::{self, HandlerContext, HandlerError, Outcome};

/// Worker settings.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Identifies this worker in `jobs.locked_by`.
    pub worker_id: String,
    /// Queues to serve (default: all).
    pub queues: Vec<String>,
    /// Jobs run at the same time.
    pub concurrency: usize,
    /// Polling interval when idle (also the fallback without `LISTEN`).
    pub poll_interval: Duration,
    /// Upper bound of the per-job time limit (each kind has its own,
    /// see [`suggested_timeout`]).
    pub job_timeout: Duration,
    /// Lock refresh interval of a running job.
    pub heartbeat_interval: Duration,
    /// Jobs locked longer than this (no heartbeat) are taken back.
    pub lock_timeout: Duration,
    /// How often stuck-job recovery runs.
    pub recovery_interval: Duration,
    /// Backoff of retryable failures.
    pub retry: Retry,
    /// Time running jobs get to finish on shutdown.
    pub shutdown_grace: Duration,
    /// Direct (non-pooled) database URL for `LISTEN`; `None` = polling only.
    pub listen_url: Option<String>,
    /// Retention of finished jobs (cleanup task default).
    pub job_retention: Duration,
    /// Deliver outgoing webhooks to loopback / private addresses too.
    /// **Tests only**; production keeps the SSRF guard (`false`).
    pub allow_private_webhook_targets: bool,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: default_worker_id(),
            queues: queues::ALL.iter().map(|q| (*q).to_owned()).collect(),
            concurrency: 8,
            poll_interval: Duration::from_secs(2),
            job_timeout: Duration::from_secs(1800),
            heartbeat_interval: Duration::from_secs(30),
            lock_timeout: Duration::from_secs(600),
            recovery_interval: Duration::from_secs(60),
            retry: Retry::DEFAULT,
            shutdown_grace: Duration::from_secs(30),
            listen_url: None,
            job_retention: Duration::from_secs(30 * 86_400),
            allow_private_webhook_targets: false,
        }
    }
}

/// `host-pid-random`.
pub fn default_worker_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "worker".into());
    let rnd = uuid::Uuid::now_v7().simple().to_string();
    format!("{host}-{}-{}", std::process::id(), &rnd[24..])
}

/// Counters of [`Worker::run_until_idle`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunStats {
    pub completed: usize,
    pub retried: usize,
    pub dead: usize,
}

/// What happened to one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobResult {
    Completed,
    Retried,
    Dead,
    /// The lock was lost (another worker took the job over).
    Lost,
}

struct Inner {
    pool: PgPool,
    cfg: WorkerConfig,
    ctx: HandlerContext,
    wake: Notify,
}

/// A job worker (cheap to clone).
#[derive(Clone)]
pub struct Worker {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("worker_id", &self.inner.cfg.worker_id)
            .finish_non_exhaustive()
    }
}

impl Worker {
    /// A worker over `pool` running handlers with `billing` (its registry
    /// builds the integrations).
    pub fn new(pool: PgPool, billing: Billing, config: WorkerConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        let ctx = HandlerContext {
            billing,
            http,
            job_retention: config.job_retention,
            allow_private_webhook_targets: config.allow_private_webhook_targets,
        };
        Self {
            inner: Arc::new(Inner {
                pool,
                cfg: config,
                ctx,
                wake: Notify::new(),
            }),
        }
    }

    /// Settings.
    pub fn config(&self) -> &WorkerConfig {
        &self.inner.cfg
    }

    /// Wakes the claim loop (e.g. after an in-process enqueue).
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Runs until `shutdown` resolves.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send) -> Result<(), CoreError> {
        let cfg = &self.inner.cfg;
        tracing::info!(worker = %cfg.worker_id, queues = ?cfg.queues, concurrency = cfg.concurrency, "job worker started");
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let listener = cfg
            .listen_url
            .clone()
            .map(|url| tokio::spawn(listen_loop(self.clone(), url, stop_rx.clone())));
        let permits = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
        let mut running: JoinSet<()> = JoinSet::new();
        let mut last_recovery = tokio::time::Instant::now()
            .checked_sub(cfg.recovery_interval)
            .unwrap_or_else(tokio::time::Instant::now);
        tokio::pin!(shutdown);

        loop {
            while running.try_join_next().is_some() {}
            if last_recovery.elapsed() >= cfg.recovery_interval {
                last_recovery = tokio::time::Instant::now();
                match repo::recover_stuck_jobs(&self.inner.pool, cfg.lock_timeout).await {
                    Ok(0) => {}
                    Ok(n) => tracing::warn!(recovered = n, "returned stuck jobs to the queue"),
                    Err(e) => tracing::error!(error = %e, "stuck job recovery failed"),
                }
            }
            let free = permits.available_permits();
            let mut claimed = 0;
            if free > 0 {
                match repo::claim_batch(
                    &self.inner.pool,
                    &cfg.worker_id,
                    &cfg.queues,
                    i64::try_from(free).unwrap_or(1),
                )
                .await
                {
                    Ok(jobs) => {
                        claimed = jobs.len();
                        for job in jobs {
                            let Ok(permit) = permits.clone().acquire_owned().await else {
                                break;
                            };
                            let me = self.clone();
                            running.spawn(async move {
                                let _permit = permit;
                                me.process(job).await;
                            });
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "claiming jobs failed");
                    }
                }
            }
            if claimed > 0 && claimed == free {
                // Busy: claim again as soon as a slot frees up.
                tokio::select! {
                    () = &mut shutdown => break,
                    _ = running.join_next(), if !running.is_empty() => {}
                }
                continue;
            }
            tokio::select! {
                () = &mut shutdown => break,
                () = self.inner.wake.notified() => {}
                () = tokio::time::sleep(cfg.poll_interval) => {}
                _ = running.join_next(), if !running.is_empty() && free == 0 => {}
            }
        }

        tracing::info!(worker = %cfg.worker_id, running = running.len(), "job worker stopping");
        let _ = stop_tx.send(true);
        let drained = tokio::time::timeout(cfg.shutdown_grace, async {
            while running.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            running.abort_all();
            while running.join_next().await.is_some() {}
            // Unfinished jobs go back to the queue right away.
            let res = sqlx::query(
                "update jobs set status = 'pending', run_at = now(), locked_by = null, locked_at = null,
                        last_error = 'worker shut down'
                 where status = 'running' and locked_by = $1",
            )
            .bind(&cfg.worker_id)
            .execute(&self.inner.pool)
            .await;
            if let Err(e) = res {
                tracing::error!(error = %e, "releasing jobs on shutdown failed");
            }
        }
        if let Some(l) = listener {
            l.abort();
        }
        tracing::info!(worker = %cfg.worker_id, "job worker stopped");
        Ok(())
    }

    /// Processes due jobs of the configured queues until none are left
    /// (CLI). Jobs scheduled in the future are not waited for. Stops after
    /// `max_jobs`.
    pub async fn run_until_idle(&self, max_jobs: usize) -> Result<RunStats, CoreError> {
        self.drain(None, max_jobs).await
    }

    /// Like [`Worker::run_until_idle`] for one tenant's jobs only (any queue).
    pub async fn run_tenant_until_idle(
        &self,
        tenant_id: TenantId,
        max_jobs: usize,
    ) -> Result<RunStats, CoreError> {
        self.drain(Some(tenant_id), max_jobs).await
    }

    async fn drain(
        &self,
        tenant_id: Option<TenantId>,
        max_jobs: usize,
    ) -> Result<RunStats, CoreError> {
        let mut stats = RunStats::default();
        let mut processed = 0;
        let limit = i64::try_from(self.inner.cfg.concurrency.max(1)).unwrap_or(1);
        loop {
            let jobs = match tenant_id {
                Some(t) => {
                    billing_support::claim_tenant_jobs(
                        &self.inner.pool,
                        &self.inner.cfg.worker_id,
                        t,
                        limit,
                    )
                    .await?
                }
                None => {
                    repo::claim_batch(
                        &self.inner.pool,
                        &self.inner.cfg.worker_id,
                        &self.inner.cfg.queues,
                        limit,
                    )
                    .await?
                }
            };
            if jobs.is_empty() {
                return Ok(stats);
            }
            let mut set = JoinSet::new();
            for job in jobs {
                let me = self.clone();
                set.spawn(async move { me.process(job).await });
            }
            while let Some(r) = set.join_next().await {
                processed += 1;
                match r {
                    Ok(JobResult::Completed) => stats.completed += 1,
                    Ok(JobResult::Retried) => stats.retried += 1,
                    Ok(JobResult::Dead) => stats.dead += 1,
                    Ok(JobResult::Lost) | Err(_) => {}
                }
            }
            if processed >= max_jobs {
                return Ok(stats);
            }
        }
    }

    /// Runs one claimed job and records its outcome.
    pub async fn process(&self, job: JobRow) -> JobResult {
        let span = tracing::info_span!(
            "job",
            job_id = %job.id,
            kind = %job.kind,
            tenant_id = ?job.tenant_id,
            attempt = job.attempts,
            max_attempts = job.max_attempts,
        );
        self.process_inner(job).instrument(span).await
    }

    async fn process_inner(&self, job: JobRow) -> JobResult {
        let cfg = &self.inner.cfg;
        let pool = &self.inner.pool;
        let started = std::time::Instant::now();

        // heartbeat
        let hb = {
            let pool = pool.clone();
            let id = job.id;
            let worker = cfg.worker_id.clone();
            let every = cfg.heartbeat_interval;
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(every);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if let Err(e) = repo::heartbeat(&pool, id, &worker).await {
                        tracing::warn!(error = %e, "heartbeat failed");
                    }
                }
            })
        };

        // the handler runs in its own task so a panic is contained
        let ctx = self.inner.ctx.clone();
        let job_for_task = job.clone();
        let handle = tokio::spawn(async move { handlers::dispatch(&ctx, &job_for_task).await });
        let abort = handle.abort_handle();
        let limit = suggested_timeout(&job.kind).min(cfg.job_timeout);
        let result = match tokio::time::timeout(limit, handle).await {
            Ok(Ok(r)) => r,
            Ok(Err(join)) if join.is_panic() => {
                Err(HandlerError::Permanent("handler panicked".into()))
            }
            Ok(Err(_)) => Err(HandlerError::Retry {
                message: "handler cancelled".into(),
                after: None,
            }),
            Err(_) => {
                abort.abort();
                Err(HandlerError::Retry {
                    message: format!("timed out after {}s", limit.as_secs()),
                    after: None,
                })
            }
        };
        hb.abort();
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let recorded = match &result {
            Ok(Outcome::Done(value)) => {
                tracing::info!(elapsed_ms, "job completed");
                repo::complete_job(pool, job.id, &cfg.worker_id, Some(value))
                    .await
                    .map(|ok| {
                        if ok {
                            JobResult::Completed
                        } else {
                            JobResult::Lost
                        }
                    })
            }
            Ok(Outcome::Again { after, note }) => {
                tracing::debug!(elapsed_ms, note = %note, "job not finished yet");
                repo::fail_job(pool, job.id, &cfg.worker_id, note, Retry::After(*after))
                    .await
                    .map(status_result)
            }
            Err(HandlerError::Retry { message, after }) => {
                let retry = after.map_or(cfg.retry, Retry::After);
                let r = repo::fail_job(pool, job.id, &cfg.worker_id, message, retry)
                    .await
                    .map(status_result);
                if matches!(r, Ok(JobResult::Dead)) {
                    tracing::error!(elapsed_ms, error = %message, "job failed permanently (attempts exhausted)");
                } else {
                    tracing::warn!(elapsed_ms, error = %message, "job failed, will retry");
                }
                r
            }
            Err(HandlerError::Permanent(message)) => {
                tracing::error!(elapsed_ms, error = %message, "job failed permanently");
                repo::fail_job(pool, job.id, &cfg.worker_id, message, Retry::Never)
                    .await
                    .map(status_result)
            }
        };
        match recorded {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "recording the job outcome failed");
                JobResult::Lost
            }
        }
    }
}

fn status_result(s: Option<hos_db::enums::JobStatus>) -> JobResult {
    match s {
        Some(hos_db::enums::JobStatus::Pending) => JobResult::Retried,
        Some(_) => JobResult::Dead,
        None => JobResult::Lost,
    }
}

/// `LISTEN hf_jobs` on a dedicated connection; reconnects with backoff.
async fn listen_loop(worker: Worker, url: String, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut delay = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        match sqlx::postgres::PgListener::connect(&url).await {
            Ok(mut l) => {
                if let Err(e) = l.listen(NOTIFY_CHANNEL).await {
                    tracing::warn!(error = %e, "LISTEN failed");
                } else {
                    tracing::debug!("listening for job notifications");
                    delay = Duration::from_secs(1);
                    loop {
                        tokio::select! {
                            n = l.try_recv() => match n {
                                Ok(Some(_)) => worker.wake(),
                                Ok(None) => {
                                    // connection lost; sqlx reconnects on the next call
                                    worker.wake();
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "job notification listener failed");
                                    break;
                                }
                            },
                            _ = stop.changed() => return,
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "connecting the job listener failed"),
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = stop.changed() => return,
        }
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

/// Default job timeout per kind (polls and e-mails are short; provisioning
/// and syncs may take minutes).
pub fn suggested_timeout(kind: &str) -> Duration {
    match kind {
        "send_email" | "deliver_webhook" | "poll_vm_task" | "seo_indexnow_ping" => {
            Duration::from_secs(60)
        }
        "daily_automation" | "sync_domain_status" | "sync_tld_pricing" | "sync_usage"
        | "cleanup" => Duration::from_secs(1800),
        _ => Duration::from_secs(300),
    }
}
