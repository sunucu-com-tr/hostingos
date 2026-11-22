//! Job queue: `FOR UPDATE SKIP LOCKED` claiming, retries with backoff, stuck
//! job recovery and scheduled tasks.

use std::collections::HashSet;
use std::time::Duration;

use crate::common::{self, TestTenant};
use chrono::Utc;
use hos_core::ids::JobId;
use hos_db::enums::{JobStatus, TaskRunStatus};
use hos_db::repo::jobs::{self, Enqueued, NewJob, Retry};
use serde_json::json;
use sqlx::Acquire;

/// A queue name only this test uses (claims are per queue).
fn queue(t: &TestTenant) -> String {
    format!("test.{}", t.slug)
}

#[tokio::test]
async fn concurrent_workers_never_claim_the_same_job() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let q = queue(&t);

    const JOBS: usize = 60;
    for i in 0..JOBS {
        let job = NewJob::new("test.noop", json!({ "i": i }))
            .tenant(t.id)
            .queue(&q)
            .priority(i16::try_from(i % 3).unwrap());
        assert!(matches!(
            jobs::enqueue(&pool, &job).await.unwrap(),
            Enqueued::Created(_)
        ));
    }

    let mut workers = Vec::new();
    for w in 0..6 {
        let pool = pool.clone();
        let queues = vec![q.clone()];
        workers.push(tokio::spawn(async move {
            let worker = format!("worker-{w}");
            let mut mine = Vec::new();
            loop {
                let batch = jobs::claim_batch(&pool, &worker, &queues, 4).await.unwrap();
                if batch.is_empty() {
                    break;
                }
                for job in batch {
                    assert_eq!(job.status, JobStatus::Running);
                    assert_eq!(job.locked_by.as_deref(), Some(worker.as_str()));
                    assert_eq!(job.attempts, 1);
                    assert!(
                        jobs::complete_job(&pool, job.id, &worker, None)
                            .await
                            .unwrap()
                    );
                    mine.push(job.id);
                }
            }
            mine
        }));
    }
    let mut seen: HashSet<JobId> = HashSet::new();
    let mut total = 0;
    for w in workers {
        for id in w.await.unwrap() {
            total += 1;
            assert!(seen.insert(id), "job {id} claimed twice");
        }
    }
    assert_eq!(total, JOBS);
    let pending: i64 =
        sqlx::query_scalar("select count(*) from jobs where queue = $1 and status <> 'completed'")
            .bind(&q)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending, 0);
    t.cleanup().await;
}

#[tokio::test]
async fn job_lifecycle_retry_and_recovery() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let t = TestTenant::create(&pool).await;
    let q = queue(&t);
    let queues = vec![q.clone()];

    // Dedupe: the second enqueue returns the existing job.
    let job = NewJob::new("provision.service", json!({"service": 1}))
        .tenant(t.id)
        .queue(&q)
        .max_attempts(3)
        .dedupe(format!("provision:{}", t.slug));
    let Enqueued::Created(id) = jobs::enqueue(&pool, &job).await.unwrap() else {
        panic!("expected a new job")
    };
    assert_eq!(
        jobs::enqueue(&pool, &job).await.unwrap(),
        Enqueued::Duplicate(id)
    );

    // A delayed job is not claimable yet.
    let later = NewJob::new("later", json!({}))
        .tenant(t.id)
        .queue(&q)
        .run_at(Utc::now() + chrono::Duration::hours(1));
    let Enqueued::Created(later_id) = jobs::enqueue(&pool, &later).await.unwrap() else {
        panic!("expected a new job")
    };

    // Attempt 1 fails → pending again with backoff in the future.
    let claimed = jobs::claim_batch(&pool, "w1", &queues, 10).await.unwrap();
    assert_eq!(claimed.iter().map(|j| j.id).collect::<Vec<_>>(), vec![id]);
    // Only the holder may finish it.
    assert!(
        !jobs::complete_job(&pool, id, "intruder", None)
            .await
            .unwrap()
    );
    let status = jobs::fail_job(&pool, id, "w1", "boom", Retry::DEFAULT)
        .await
        .unwrap();
    assert_eq!(status, Some(JobStatus::Pending));
    let row = jobs::get_job(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.last_error.as_deref(), Some("boom"));
    let delay = (row.run_at - Utc::now()).num_seconds();
    assert!((20..=40).contains(&delay), "backoff {delay}s");
    assert!(
        jobs::claim_batch(&pool, "w1", &queues, 10)
            .await
            .unwrap()
            .is_empty()
    );

    // Make it due; attempt 2 is lost by a crashed worker and recovered.
    sqlx::query("update jobs set run_at = now() where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let claimed = jobs::claim_batch(&pool, "w2", &queues, 10).await.unwrap();
    assert_eq!(claimed[0].attempts, 2);
    sqlx::query("update jobs set locked_at = now() - interval '2 hours' where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        jobs::recover_stuck_jobs(&pool, Duration::from_secs(3600))
            .await
            .unwrap()
            >= 1
    );
    let row = jobs::get_job(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        (row.status, row.locked_by.as_deref()),
        (JobStatus::Pending, None)
    );
    // The crashed worker can no longer complete it.
    assert!(!jobs::complete_job(&pool, id, "w2", None).await.unwrap());

    // Attempt 3 (the last) fails → failed for good.
    let claimed = jobs::claim_batch(&pool, "w3", &queues, 10).await.unwrap();
    assert_eq!(claimed[0].attempts, 3);
    let status = jobs::fail_job(&pool, id, "w3", "still broken", Retry::DEFAULT)
        .await
        .unwrap();
    assert_eq!(status, Some(JobStatus::Failed));
    assert!(
        jobs::get_job(&pool, id)
            .await
            .unwrap()
            .unwrap()
            .failed_at
            .is_some()
    );

    // Manual retry resets attempts; success stores the result.
    assert!(jobs::retry_job(&pool, id).await.unwrap());
    let claimed = jobs::claim_batch(&pool, "w4", &queues, 10).await.unwrap();
    assert_eq!((claimed[0].id, claimed[0].attempts), (id, 1));
    assert!(
        jobs::complete_job(&pool, id, "w4", Some(&json!({"ok": true})))
            .await
            .unwrap()
    );
    let done = jobs::get_job(&pool, id).await.unwrap().unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.result, Some(json!({"ok": true})));

    // Retry::Never fails immediately; pending jobs can be cancelled.
    let Enqueued::Created(once) = jobs::enqueue(
        &pool,
        &NewJob::new("once", json!({})).tenant(t.id).queue(&q),
    )
    .await
    .unwrap() else {
        panic!("expected a new job")
    };
    jobs::claim_batch(&pool, "w5", &queues, 10).await.unwrap();
    assert_eq!(
        jobs::fail_job(&pool, once, "w5", "fatal", Retry::Never)
            .await
            .unwrap(),
        Some(JobStatus::Failed)
    );
    assert!(jobs::cancel_job(&pool, later_id).await.unwrap());
    assert!(!jobs::cancel_job(&pool, later_id).await.unwrap());

    // Scheduled tasks: a due task is locked once, then rescheduled.
    let mut task =
        jobs::ScheduledTaskInput::new(&format!("billing-{}", t.slug), "billing.run", "0 * * * *");
    task.tenant_id = Some(t.id);
    task.next_run_at = Some(Utc::now() - chrono::Duration::minutes(1));
    let task = jobs::upsert_scheduled_task(&pool, &task).await.unwrap();
    let mut c1 = pool.acquire().await.unwrap();
    let mut tx1 = c1.begin().await.unwrap();
    let due = jobs::lock_due_tasks(&mut tx1, 100).await.unwrap();
    assert!(due.iter().any(|d| d.id == task.id));
    // A second scheduler instance skips the locked row.
    let mut c2 = pool.acquire().await.unwrap();
    let mut tx2 = c2.begin().await.unwrap();
    assert!(
        !jobs::lock_due_tasks(&mut tx2, 100)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == task.id)
    );
    tx2.rollback().await.unwrap();
    let next = Utc::now() + chrono::Duration::hours(1);
    assert!(
        jobs::mark_task_run(&mut *tx1, task.id, Some(next), TaskRunStatus::Ok, None)
            .await
            .unwrap()
    );
    tx1.commit().await.unwrap();
    let mut tx3 = c2.begin().await.unwrap();
    assert!(
        !jobs::lock_due_tasks(&mut tx3, 100)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == task.id)
    );
    tx3.rollback().await.unwrap();
    drop((c1, c2));

    t.cleanup().await;
}
