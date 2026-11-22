//! Cron scheduler over `scheduled_tasks`: due tasks are locked with
//! `FOR UPDATE SKIP LOCKED` inside a transaction, their job is enqueued with
//! the dedupe key `sched:{task}:{due unix time}` and the next fire time is
//! stored — so any number of scheduler instances can run side by side and a
//! fire time produces exactly one job.
//!
//! Expressions use the `cron` crate syntax with seconds
//! (`sec min hour day month weekday`); classic 5-field expressions are
//! accepted too (seconds = 0). Times are evaluated in the task's IANA
//! timezone.
//!
//! **Missed runs** (scheduler down): [`MissedRunPolicy::RunOnce`] enqueues a
//! single catch-up run and continues from now; [`MissedRunPolicy::Skip`]
//! drops runs that are older than the grace period.

use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use hos_billing::jobs::{self, JobKind, JobRequest};
use hos_core::CoreError;
use hos_core::ids::TenantId;
use hos_db::PgPool;
use hos_db::enums::TaskRunStatus;
use hos_db::repo::jobs::{self as repo, ScheduledTaskInput};
use hos_db::repo::platform;
use serde_json::json;

/// What to do with fire times missed while no scheduler was running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedRunPolicy {
    /// Run once to catch up, then continue with the next future fire time.
    RunOnce,
    /// Skip runs more than `grace` late.
    Skip { grace: Duration },
}

/// Scheduler settings.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// How often due tasks are checked.
    pub tick: Duration,
    /// Tasks handled per tick.
    pub batch: i64,
    pub missed_runs: MissedRunPolicy,
    /// How often default tasks are (re)created for new tenants.
    pub ensure_interval: Duration,
    /// Only this tenant's tasks are ticked (`None`: every task, platform
    /// tasks included). For tests on a database other tenants share: an
    /// unscoped tick enqueues — or, with [`MissedRunPolicy::Skip`], skips —
    /// every other tenant's due tasks too.
    pub only_tenant: Option<TenantId>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(30),
            batch: 100,
            missed_runs: MissedRunPolicy::RunOnce,
            ensure_interval: Duration::from_secs(3600),
            only_tenant: None,
        }
    }
}

/// A default task definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultTask {
    pub name: &'static str,
    pub kind: JobKind,
    pub cron: &'static str,
}

/// Tasks every operational tenant gets (in the tenant's timezone).
pub const TENANT_TASKS: &[DefaultTask] = &[
    DefaultTask {
        name: "daily_automation",
        kind: JobKind::DailyAutomation,
        cron: "0 0 6 * * *",
    },
    DefaultTask {
        name: "sync_domain_status",
        kind: JobKind::SyncDomainStatus,
        cron: "0 20 */6 * * *",
    },
    DefaultTask {
        name: "sync_tld_pricing",
        kind: JobKind::SyncTldPricing,
        cron: "0 30 4 * * *",
    },
    DefaultTask {
        name: "sync_usage",
        kind: JobKind::SyncUsage,
        cron: "0 15 * * * *",
    },
    DefaultTask {
        name: "einvoice_sweep",
        kind: JobKind::EinvoiceSweep,
        cron: "0 */10 * * * *",
    },
];

/// Platform-wide tasks (UTC).
pub const PLATFORM_TASKS: &[DefaultTask] = &[DefaultTask {
    name: "cleanup",
    kind: JobKind::Cleanup,
    cron: "0 30 3 * * *",
}];

/// Accepts 5-field expressions by adding a leading seconds field.
pub fn normalize_cron(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {}", expr.trim())
    } else {
        expr.trim().to_owned()
    }
}

/// First fire time strictly after `after`, evaluated in `timezone`
/// (unknown zones fall back to UTC).
pub fn next_fire(
    cron_expr: &str,
    timezone: &str,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, String> {
    let schedule = cron::Schedule::from_str(&normalize_cron(cron_expr))
        .map_err(|e| format!("invalid cron expression `{cron_expr}`: {e}"))?;
    let tz: Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
    Ok(schedule
        .after(&after.with_timezone(&tz))
        .next()
        .map(|t| t.with_timezone(&Utc)))
}

/// Counters of one [`Scheduler::tick`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickStats {
    pub enqueued: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// The cron scheduler.
#[derive(Debug, Clone)]
pub struct Scheduler {
    pool: PgPool,
    cfg: SchedulerConfig,
}

impl Scheduler {
    pub fn new(pool: PgPool, config: SchedulerConfig) -> Self {
        Self { pool, cfg: config }
    }

    /// Creates the platform tasks and the default tasks of every operational
    /// tenant (existing tasks keep their state and `is_active`). Returns the
    /// number of tasks upserted.
    pub async fn ensure_default_tasks(&self) -> Result<usize, CoreError> {
        let now = Utc::now();
        let mut n = 0;
        for t in PLATFORM_TASKS {
            self.upsert(None, t, "UTC", now).await?;
            n += 1;
        }
        for tenant_id in platform::operational_tenant_ids(&self.pool).await? {
            n += self.ensure_tenant_tasks(tenant_id).await?;
        }
        Ok(n)
    }

    /// Default tasks of one tenant, in its timezone.
    pub async fn ensure_tenant_tasks(&self, tenant_id: TenantId) -> Result<usize, CoreError> {
        let tz = platform::get_tenant(&self.pool, tenant_id)
            .await?
            .map_or_else(|| "UTC".to_owned(), |t| t.timezone);
        let now = Utc::now();
        for t in TENANT_TASKS {
            self.upsert(Some(tenant_id), t, &tz, now).await?;
        }
        Ok(TENANT_TASKS.len())
    }

    async fn upsert(
        &self,
        tenant_id: Option<TenantId>,
        t: &DefaultTask,
        tz: &str,
        now: DateTime<Utc>,
    ) -> Result<(), CoreError> {
        let next = next_fire(t.cron, tz, now).map_err(CoreError::Internal)?;
        repo::upsert_scheduled_task(
            &self.pool,
            &ScheduledTaskInput {
                tenant_id,
                name: t.name.to_owned(),
                kind: t.kind.as_str().to_owned(),
                queue: t.kind.queue().to_owned(),
                payload: json!({}),
                cron: t.cron.to_owned(),
                timezone: tz.to_owned(),
                next_run_at: next,
            },
        )
        .await?;
        Ok(())
    }

    /// Enqueues the jobs of due tasks once.
    pub async fn tick(&self) -> Result<TickStats, CoreError> {
        let mut stats = TickStats::default();
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let due = match self.cfg.only_tenant {
            Some(tenant_id) => {
                repo::lock_due_tenant_tasks(&mut tx, tenant_id, self.cfg.batch).await?
            }
            None => repo::lock_due_tasks(&mut tx, self.cfg.batch).await?,
        };
        for task in due {
            let Some(due) = task.next_run_at else {
                continue;
            };
            let next = match next_fire(&task.cron, &task.timezone, now) {
                Ok(n) => n,
                Err(e) => {
                    repo::mark_task_run(&mut *tx, task.id, None, TaskRunStatus::Error, Some(&e))
                        .await?;
                    stats.errors += 1;
                    continue;
                }
            };
            let late = (now - due).to_std().unwrap_or_default();
            if let MissedRunPolicy::Skip { grace } = self.cfg.missed_runs
                && late > grace
            {
                repo::mark_task_run(
                    &mut *tx,
                    task.id,
                    next,
                    TaskRunStatus::Ok,
                    Some("missed run skipped"),
                )
                .await?;
                stats.skipped += 1;
                continue;
            }
            let Ok(kind) = task.kind.parse::<JobKind>() else {
                let msg = format!("unknown job kind {}", task.kind);
                repo::mark_task_run(&mut *tx, task.id, next, TaskRunStatus::Error, Some(&msg))
                    .await?;
                stats.errors += 1;
                continue;
            };
            let req = JobRequest::new(kind, task.tenant_id, &task.payload).dedupe(format!(
                "sched:{}:{}",
                task.id,
                due.timestamp()
            ));
            jobs::enqueue(&mut tx, req).await?;
            repo::mark_task_run(&mut *tx, task.id, next, TaskRunStatus::Ok, None).await?;
            stats.enqueued += 1;
        }
        tx.commit().await?;
        Ok(stats)
    }

    /// Ticks until `shutdown` resolves; re-ensures default tasks periodically.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send) -> Result<(), CoreError> {
        tracing::info!("scheduler started");
        tokio::pin!(shutdown);
        let mut last_ensure: Option<tokio::time::Instant> = None;
        loop {
            if last_ensure.is_none_or(|t| t.elapsed() >= self.cfg.ensure_interval) {
                match self.ensure_default_tasks().await {
                    Ok(n) => tracing::debug!(tasks = n, "default scheduled tasks ensured"),
                    Err(e) => tracing::error!(error = %e, "ensuring scheduled tasks failed"),
                }
                last_ensure = Some(tokio::time::Instant::now());
            }
            match self.tick().await {
                Ok(s) if s.enqueued + s.skipped + s.errors > 0 => {
                    tracing::info!(
                        enqueued = s.enqueued,
                        skipped = s.skipped,
                        errors = s.errors,
                        "scheduler tick"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "scheduler tick failed"),
            }
            tokio::select! {
                () = &mut shutdown => break,
                () = tokio::time::sleep(self.cfg.tick) => {}
            }
        }
        tracing::info!("scheduler stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn cron_in_timezone() {
        let after = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
        // 06:00 Istanbul (UTC+3) = 03:00 UTC next day
        let n = next_fire("0 0 6 * * *", "Europe/Istanbul", after)
            .unwrap()
            .unwrap();
        assert_eq!(n, Utc.with_ymd_and_hms(2026, 3, 2, 3, 0, 0).unwrap());
        // five fields accepted
        let n = next_fire("30 4 * * *", "UTC", after).unwrap().unwrap();
        assert_eq!(n, Utc.with_ymd_and_hms(2026, 3, 2, 4, 30, 0).unwrap());
        assert!(next_fire("nope", "UTC", after).is_err());
        // unknown zone → UTC
        let n = next_fire("0 0 6 * * *", "Mars/Base", after)
            .unwrap()
            .unwrap();
        assert_eq!(n, Utc.with_ymd_and_hms(2026, 3, 2, 6, 0, 0).unwrap());
    }

    #[test]
    fn defaults_are_valid() {
        let now = Utc::now();
        for t in TENANT_TASKS.iter().chain(PLATFORM_TASKS) {
            assert!(
                next_fire(t.cron, "Europe/Istanbul", now).unwrap().is_some(),
                "{}",
                t.name
            );
        }
    }
}
