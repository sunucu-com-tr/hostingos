//! Executes queued runs: the background worker loop (`hostingos serve`
//! worker) and inline execution (CLI, tests).
//!
//! A run is claimed with `FOR UPDATE SKIP LOCKED` and kept alive by a
//! background heartbeat every [`HEARTBEAT`] (and with every progress
//! snapshot); a run whose worker died is picked up again after
//! [`STALE_SECS`] and resumes at its cursor. Every batch transaction locks
//! the run row and checks the lease first (`repo::hold_lease`), so a run is
//! never taken over in the middle of a batch, and a worker that lost the
//! lease writes nothing more and stops; only the lease holder finishes a run. The
//! source credentials are opened only here, never logged, and wiped when
//! the run ends (they survive only a finished dry run, for the import that
//! follows it, and are wiped after [`SECRET_TTL_SECS`] in any case).

use std::time::Duration;

use hos_core::crypto::SecretBox;
use hos_core::ids::TenantId;
use hos_core::net::OutboundPolicy;
use hos_db::repo::import::{self as repo, ImportRunRow, RunEnd};
use serde_json::{Value as Json, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::{SourceConfig, SourceSecrets};
use crate::crypto::open_secrets;
use crate::engine::{Engine, EngineParams, today};
use crate::error::{ImportError, ImportResult};
use crate::rollback;
use crate::source::Source;
use crate::source::api::{ApiClient, ApiSource};
use crate::source::mysql::MysqlSource;

/// A run whose heartbeat is older than this is taken over.
pub const STALE_SECS: i64 = 300;
/// Interval of the background heartbeat of a working run.
pub const HEARTBEAT: Duration = Duration::from_secs(30);
/// Source credentials of an unfinished draft are wiped after this.
pub const SECRET_TTL_SECS: i64 = 24 * 3600;
const POLL: Duration = Duration::from_secs(5);

/// Connector for a run's source.
pub fn build_source(
    config: &SourceConfig,
    secrets: &SourceSecrets,
    policy: OutboundPolicy,
) -> ImportResult<Box<dyn Source>> {
    Ok(match config {
        SourceConfig::WhmcsDb(c) => {
            let attachments = match &c.attachments_api {
                Some(api)
                    if secrets
                        .api_secret
                        .as_deref()
                        .is_some_and(|s| !s.trim().is_empty()) =>
                {
                    Some(ApiClient::new(api, secrets, policy)?)
                }
                _ => None,
            };
            Box::new(MysqlSource::new(c.clone(), secrets, policy, attachments))
        }
        SourceConfig::WhmcsApi(c) => Box::new(ApiSource::new(c, secrets, policy)?),
    })
}

/// The executor.
#[derive(Clone)]
pub struct Runner {
    pool: PgPool,
    secret_box: SecretBox,
    policy: OutboundPolicy,
    worker_id: String,
}

impl Runner {
    /// Runner identified as `worker_id`.
    pub fn new(
        pool: PgPool,
        secret_box: SecretBox,
        policy: OutboundPolicy,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            secret_box,
            policy,
            worker_id: worker_id.into(),
        }
    }

    /// Claims and executes one run; `false` when nothing was queued.
    pub async fn run_once(&self) -> ImportResult<bool> {
        let Some(run) = repo::claim_next_run(&self.pool, &self.worker_id, STALE_SECS).await? else {
            return Ok(false);
        };
        self.execute(run, None).await?;
        Ok(true)
    }

    /// Worker loop until `shutdown` resolves.
    pub async fn run_loop(self, shutdown: impl std::future::Future<Output = ()>) {
        tokio::pin!(shutdown);
        let mut last_wipe = std::time::Instant::now() - Duration::from_secs(3600);
        loop {
            if last_wipe.elapsed() > Duration::from_secs(600) {
                if let Err(e) = repo::wipe_stale_secrets(&self.pool, SECRET_TTL_SECS).await {
                    tracing::warn!(error = %e, "import: wiping stale source credentials failed");
                }
                last_wipe = std::time::Instant::now();
            }
            let worked = match self.run_once().await {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!(error = %e, "import runner error");
                    false
                }
            };
            if worked {
                continue;
            }
            tokio::select! {
                () = &mut shutdown => break,
                () = tokio::time::sleep(POLL) => {}
            }
        }
    }

    /// Executes a queued run of `tenant` right here (CLI / tests);
    /// `source` overrides the configured connector (fixtures).
    pub async fn run_inline(
        &self,
        tenant: TenantId,
        run_id: Uuid,
        source: Option<Box<dyn Source>>,
    ) -> ImportResult<ImportRunRow> {
        if !repo::start_inline(&self.pool, tenant, run_id, &self.worker_id).await? {
            return Err(ImportError::Config("the run is not queued".into()));
        }
        let run = repo::get_run(&self.pool, tenant, run_id)
            .await?
            .ok_or_else(|| ImportError::Config("run not found".into()))?;
        self.execute(run, source).await?;
        repo::get_run(&self.pool, tenant, run_id)
            .await?
            .ok_or_else(|| ImportError::Config("run not found".into()))
    }

    /// Queues `phase` of a draft run and claims it for this worker in one
    /// transaction, then executes it here (CLI, tests). No other worker ever
    /// sees the run queued, so none can pick it up first (a `serve` worker
    /// on the same database would run it with its own secret key and fail
    /// it). `Ok(None)`: the run is not a draft with credentials, or another
    /// import of the tenant is active.
    pub async fn queue_and_run(
        &self,
        tenant: TenantId,
        run_id: Uuid,
        phase: &str,
        started_by: Option<Uuid>,
        source: Option<Box<dyn Source>>,
    ) -> ImportResult<Option<ImportRunRow>> {
        let mut tx = self.pool.begin().await?;
        if !repo::queue_run(&mut *tx, tenant, run_id, phase, started_by).await?
            || !repo::start_inline(&mut *tx, tenant, run_id, &self.worker_id).await?
        {
            return Ok(None);
        }
        tx.commit().await?;
        self.execute_claimed(tenant, run_id, source).await.map(Some)
    }

    /// Queues the rollback of a finished import and claims it in one
    /// transaction, then executes it here (see [`Runner::queue_and_run`]).
    pub async fn rollback_now(
        &self,
        tenant: TenantId,
        run_id: Uuid,
    ) -> ImportResult<Option<ImportRunRow>> {
        let mut tx = self.pool.begin().await?;
        if !repo::queue_rollback(&mut *tx, tenant, run_id).await?
            || !repo::start_inline(&mut *tx, tenant, run_id, &self.worker_id).await?
        {
            return Ok(None);
        }
        tx.commit().await?;
        self.execute_claimed(tenant, run_id, None).await.map(Some)
    }

    async fn execute_claimed(
        &self,
        tenant: TenantId,
        run_id: Uuid,
        source: Option<Box<dyn Source>>,
    ) -> ImportResult<ImportRunRow> {
        let run = repo::get_run(&self.pool, tenant, run_id)
            .await?
            .ok_or_else(|| ImportError::Config("run not found".into()))?;
        self.execute(run, source).await?;
        repo::get_run(&self.pool, tenant, run_id)
            .await?
            .ok_or_else(|| ImportError::Config("run not found".into()))
    }

    async fn finish(
        &self,
        run: &ImportRunRow,
        status: &str,
        progress: &Json,
        result: &Json,
        error: Option<&str>,
    ) -> ImportResult<()> {
        if !repo::finish_run(
            &self.pool,
            run.tenant_id,
            run.id,
            &self.worker_id,
            &RunEnd {
                status,
                progress,
                result,
                error,
            },
        )
        .await?
        {
            tracing::warn!(run = %run.id, "import run no longer held by this worker; not finished here");
        }
        Ok(())
    }

    /// Background heartbeat of `run` while this worker holds it; stops by
    /// itself once the lease is gone, and with the returned guard.
    fn heartbeat(&self, run: &ImportRunRow) -> Beat {
        let pool = self.pool.clone();
        let (tenant, id, worker) = (run.tenant_id, run.id, self.worker_id.clone());
        Beat(tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT);
            tick.tick().await;
            loop {
                tick.tick().await;
                match repo::heartbeat(&pool, tenant, id, &worker).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => tracing::warn!(run = %id, error = %e, "import heartbeat failed"),
                }
            }
        }))
    }

    async fn execute(
        &self,
        run: ImportRunRow,
        source: Option<Box<dyn Source>>,
    ) -> ImportResult<()> {
        let _beat = self.heartbeat(&run);
        if run.status == "rolling_back" {
            return self.execute_rollback(&run).await;
        }
        let dry = run.phase == "dry_run";
        let secrets = match repo::run_secrets(&self.pool, run.tenant_id, run.id).await? {
            Some(sealed) => open_secrets(&self.secret_box, run.tenant_id, run.id, &sealed)?,
            None if source.is_some() => SourceSecrets::default(),
            None => {
                let msg = ImportError::SecretsGone.to_string();
                return self
                    .finish(
                        &run,
                        if dry { "draft" } else { "failed" },
                        &run.progress,
                        &json!({}),
                        Some(&msg),
                    )
                    .await;
            }
        };
        let mut source = match source {
            Some(s) => s,
            None => {
                let config: SourceConfig = match serde_json::from_value(run.source_config.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        let msg = format!("invalid source configuration: {e}");
                        return self
                            .finish(
                                &run,
                                if dry { "draft" } else { "failed" },
                                &run.progress,
                                &json!({}),
                                Some(&msg),
                            )
                            .await;
                    }
                };
                match build_source(&config, &secrets, self.policy) {
                    Ok(s) => s,
                    Err(e) => {
                        let msg = e.to_string();
                        return self
                            .finish(
                                &run,
                                if dry { "draft" } else { "failed" },
                                &run.progress,
                                &json!({}),
                                Some(&msg),
                            )
                            .await;
                    }
                }
            }
        };
        let mut engine = Engine::new(EngineParams {
            pool: &self.pool,
            secret_box: &self.secret_box,
            run: &run,
            secrets: &secrets,
            worker: self.worker_id.clone(),
            today: today(),
        })?;
        let outcome = engine.run(source.as_mut()).await;
        let progress = serde_json::to_value(engine.progress()).unwrap_or_default();
        drop(secrets);
        match outcome {
            Ok(result) => {
                let status = if dry { "draft" } else { "completed" };
                let mut result = serde_json::to_value(&result).unwrap_or_default();
                if let Some(o) = result.as_object_mut() {
                    o.insert("final_status".into(), json!(status));
                }
                tracing::info!(run = %run.id, phase = %run.phase, "import run finished");
                self.finish(&run, status, &progress, &result, None).await
            }
            Err(ImportError::LeaseLost) => {
                tracing::warn!(run = %run.id, "import run taken over by another worker");
                Ok(())
            }
            Err(ImportError::Cancelled) => {
                let status = if dry { "draft" } else { "cancelled" };
                self.finish(
                    &run,
                    status,
                    &progress,
                    &json!({ "final_status": status }),
                    Some("cancelled"),
                )
                .await
            }
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "import run failed");
                let status = if dry { "draft" } else { "failed" };
                let msg = e.to_string();
                self.finish(
                    &run,
                    status,
                    &progress,
                    &json!({ "final_status": status }),
                    Some(&msg),
                )
                .await
            }
        }
    }

    async fn execute_rollback(&self, run: &ImportRunRow) -> ImportResult<()> {
        let previous = run
            .summary
            .get("final_status")
            .and_then(Json::as_str)
            .unwrap_or("completed")
            .to_owned();
        match rollback::rollback(&self.pool, run.tenant_id, run.id, run.automation_paused).await {
            Ok(outcome) => match outcome.blocked_message() {
                Some(msg) => {
                    let mut summary = run.summary.clone();
                    if let Some(o) = summary.as_object_mut() {
                        o.insert(
                            "rollback_blocked".into(),
                            serde_json::to_value(&outcome.blocked).unwrap_or_default(),
                        );
                    }
                    self.finish(run, &previous, &run.progress, &summary, Some(&msg))
                        .await
                }
                None => {
                    let mut summary = run.summary.clone();
                    if let Some(o) = summary.as_object_mut() {
                        o.insert(
                            "rolled_back".into(),
                            serde_json::to_value(&outcome.deleted).unwrap_or_default(),
                        );
                    }
                    self.finish(run, "rolled_back", &run.progress, &summary, None)
                        .await
                }
            },
            Err(e) => {
                let msg = format!("rollback failed: {e}");
                self.finish(run, &previous, &run.progress, &run.summary, Some(&msg))
                    .await
            }
        }
    }
}

/// Aborts the heartbeat task when the run ends (any way).
struct Beat(tokio::task::JoinHandle<()>);

impl Drop for Beat {
    fn drop(&mut self) {
        self.0.abort();
    }
}
