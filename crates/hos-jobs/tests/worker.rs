//! Worker and scheduler against the real database with mock providers.
//!
//! Each test drains only its own tenant's jobs (`run_tenant_until_idle`) or
//! uses a private queue, so tests can share the database (and run in parallel).

#[path = "../../hos-billing/tests/it/common.rs"]
mod common;

use std::time::Duration;

use common::{Env, suffix};
use hos_billing::jobs::{JobKind, NOTIFY_CHANNEL};
use hos_billing::provisioning::ProvisionOutcome;
use hos_core::domain::{ProductType, ServiceStatus};
use hos_core::ids::ServiceId;
use hos_core::money::BillingCycle;
use hos_db::repo::billing_support;
use hos_db::repo::carts::NewCartItem;
use hos_db::repo::jobs::{self as jobs_repo, NewJob};
use hos_db::repo::services;
use hos_jobs::{MissedRunPolicy, Scheduler, SchedulerConfig, Worker, WorkerConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn worker(env: &Env) -> Worker {
    Worker::new(
        env.pool.clone(),
        env.billing.clone(),
        WorkerConfig {
            concurrency: 4,
            // The webhook tests deliver to 127.0.0.1.
            allow_private_webhook_targets: true,
            ..WorkerConfig::default()
        },
    )
}

async fn status(env: &Env, id: ServiceId) -> ServiceStatus {
    services::get_service(&env.pool, env.tenant, id)
        .await
        .unwrap()
        .unwrap()
        .status
}

/// `(status, attempts, last_error, run_at - now() in seconds)` of a job.
async fn job_state(env: &Env, kind: &str) -> (String, i32, Option<String>, f64) {
    sqlx::query_as::<_, (String, i32, Option<String>, f64)>(
        "select status::text, attempts, last_error, extract(epoch from run_at - now())::float8
         from jobs where tenant_id = $1 and kind = $2 order by created_at desc limit 1",
    )
    .bind(env.tenant)
    .bind(kind)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

/// Scenario 1 through the worker: payment webhook → provisioning job and
/// e-mail jobs run by the worker → mock panel account → service active.
#[tokio::test]
async fn worker_runs_hosting_flow_end_to_end() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let panel = env.panel().await;
    let product = env
        .product(
            "Hosting",
            ProductType::SharedHosting,
            Some(panel),
            json!({"package": "starter"}),
            "100",
            "0",
        )
        .await;
    let client = env.client("Worker").await;
    let cart = env
        .cart(
            client,
            &[Env::hosting_item(product, &format!("w-{}.com", suffix()))],
        )
        .await;
    let out = env.checkout(cart, client, Some(gw)).await;
    env.pay(out.invoice_id, gw).await;
    let svc = out.services[0];
    assert_eq!(status(&env, svc).await, ServiceStatus::Pending);

    let w = worker(&env);
    let stats = w.run_tenant_until_idle(t, 100).await.unwrap();
    assert_eq!((stats.retried, stats.dead), (0, 0), "{stats:?}");
    assert!(stats.completed >= 4, "{stats:?}"); // provisioning + e-mails
    assert_eq!(status(&env, svc).await, ServiceStatus::Active);
    let p = env.billing.registry().panel(t, panel).await.unwrap();
    assert_eq!(p.list_accounts().await.unwrap().len(), 1);

    let jobs = env.jobs().await;
    assert!(jobs.iter().all(|(_, s)| s == "completed"), "{jobs:?}");
    assert!(jobs.iter().any(|(k, _)| k == "provision_service"));
    let result: Value = sqlx::query_scalar(
        "select result from jobs where tenant_id = $1 and kind = 'provision_service'",
    )
    .bind(t)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        result,
        serde_json::to_value(ProvisionOutcome::Activated).unwrap()
    );
    assert_eq!(
        env.count("select count(*) from email_log where tenant_id = $1 and status <> 'sent'")
            .await,
        0
    );
    assert!(
        env.count("select count(*) from email_log where tenant_id = $1")
            .await
            >= 3
    );
    // nothing left
    assert_eq!(w.run_tenant_until_idle(t, 100).await.unwrap().completed, 0);
    env.cleanup().await;
}

/// Scenario 2 through the worker: VM creation is an asynchronous hypervisor
/// task; the poll job answers "again" until the task finished.
#[tokio::test]
async fn worker_polls_vm_task_until_active() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let gw = env.gateway().await;
    let virt = env.virt(true).await;
    env.ip_pool(virt).await;
    let product = env
        .product(
            "VPS",
            ProductType::Vps,
            Some(virt),
            json!({"template": "ubuntu-24.04", "cpu_cores": 1, "memory_mb": 1024, "disk_gb": 20}),
            "150",
            "0",
        )
        .await;
    let client = env.client("Vm").await;
    let mut item = NewCartItem::product(product, BillingCycle::Monthly);
    item.hostname = Some(format!("vm-{}.example.com", suffix()));
    let cart = env.cart(client, &[item]).await;
    let out = env.checkout(cart, client, Some(gw)).await;
    env.pay(out.invoice_id, gw).await;
    let svc = out.services[0];

    let w = worker(&env);
    for _ in 0..40 {
        w.run_tenant_until_idle(t, 50).await.unwrap();
        if status(&env, svc).await == ServiceStatus::Active {
            break;
        }
        // a poll job may be a moment in the future (client vs. database clock)
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(status(&env, svc).await, ServiceStatus::Active);
    let (st, attempts, _, _) = job_state(&env, "poll_vm_task").await;
    assert_eq!(st, "completed");
    assert!(
        attempts >= 2,
        "the task needed more than one poll ({attempts})"
    );
    let vm = services::vm_for_service(&env.pool, t, svc)
        .await
        .unwrap()
        .unwrap();
    assert!(vm.primary_ipv4.is_some());
    env.cleanup().await;
}

/// Poison jobs (bad payload, unknown kind) go straight to `failed`.
#[tokio::test]
async fn poison_jobs_are_dead_immediately() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    for (kind, payload) in [
        ("provision_service", json!({"nope": true})),
        ("no_such_kind", json!({})),
    ] {
        jobs_repo::enqueue(
            &env.pool,
            &NewJob::new(kind, payload).tenant(t).max_attempts(5),
        )
        .await
        .unwrap();
    }
    let stats = worker(&env).run_tenant_until_idle(t, 10).await.unwrap();
    assert_eq!((stats.completed, stats.retried, stats.dead), (0, 0, 2));
    let (st, attempts, err, _) = job_state(&env, "provision_service").await;
    assert_eq!((st.as_str(), attempts), ("failed", 1));
    assert!(err.unwrap().contains("invalid provision_service payload"));
    let (st, _, err, _) = job_state(&env, "no_such_kind").await;
    assert_eq!(st, "failed");
    assert!(err.unwrap().contains("unknown job kind"));
    env.cleanup().await;
}

/// Emits `invoice.paid` to the tenant's endpoints (one `deliver_webhook` job each).
async fn emit(env: &Env) {
    let mut conn = env.pool.acquire().await.unwrap();
    let n = env
        .billing
        .emit_event(
            &mut conn,
            env.tenant,
            "invoice.paid",
            json!({"invoice_id": "x", "total": "120.00"}),
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// Outgoing webhook to an unreachable endpoint: retried with exponential
/// backoff (30 s ± 10 %), dead once the attempts are used up.
#[tokio::test]
async fn webhook_delivery_backs_off_then_dies() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let (endpoint, _secret) = env
        .billing
        .create_webhook_endpoint(t, "http://127.0.0.1:9/hook", None, &["invoice.paid".into()])
        .await
        .unwrap();
    emit(&env).await;
    sqlx::query(
        "update jobs set max_attempts = 2 where tenant_id = $1 and kind = 'deliver_webhook'",
    )
    .bind(t)
    .execute(&env.pool)
    .await
    .unwrap();
    let w = worker(&env);
    let stats = w.run_tenant_until_idle(t, 10).await.unwrap();
    assert_eq!((stats.retried, stats.dead), (1, 0), "{stats:?}");
    let (st, attempts, err, delay) = job_state(&env, "deliver_webhook").await;
    assert_eq!((st.as_str(), attempts), ("pending", 1));
    assert!(err.unwrap().contains("delivery failed"));
    assert!((25.0..=35.0).contains(&delay), "backoff {delay}s");
    // not due yet
    assert_eq!(
        w.run_tenant_until_idle(t, 10).await.unwrap(),
        hos_jobs::RunStats::default()
    );

    sqlx::query("update jobs set run_at = now() where tenant_id = $1 and kind = 'deliver_webhook'")
        .bind(t)
        .execute(&env.pool)
        .await
        .unwrap();
    let stats = w.run_tenant_until_idle(t, 10).await.unwrap();
    assert_eq!(stats.dead, 1, "{stats:?}");
    let (st, attempts, _, _) = job_state(&env, "deliver_webhook").await;
    assert_eq!((st.as_str(), attempts), ("failed", 2));
    let ep = env
        .billing
        .webhook_endpoint(t, endpoint.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ep.failure_count, ep.last_status_code), (2, None));
    env.cleanup().await;
}

/// A one-shot HTTP server: returns its URL and a handle yielding the request
/// headers (lower-case names) and body.
async fn one_request_server() -> (
    String,
    tokio::task::JoinHandle<(Vec<(String, String)>, String)>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head_end, len) = loop {
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before the headers");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                let len = head
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, v)| v.trim().parse::<usize>().unwrap());
                break (pos + 4, len);
            }
        };
        while buf.len() < head_end + len {
            let n = sock.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
        sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .await
            .unwrap();
        let _ = sock.shutdown().await;
        (headers, body)
    });
    (format!("http://{addr}/hook"), handle)
}

/// Outgoing webhook delivered with a verifiable HMAC signature.
#[tokio::test]
async fn webhook_delivery_is_signed() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let (url, server) = one_request_server().await;
    let (endpoint, secret) = env
        .billing
        .create_webhook_endpoint(t, &url, Some("test"), &[])
        .await
        .unwrap();
    emit(&env).await;
    let stats = worker(&env).run_tenant_until_idle(t, 10).await.unwrap();
    assert_eq!(stats.completed, 1, "{stats:?}");
    let (headers, body) = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(header("x-hostingos-event"), "invoice.paid");
    assert!(!header("x-hostingos-event-id").is_empty());
    assert!(hos_billing::notify::verify_webhook_signature(
        secret.expose_str(),
        &header("x-hostingos-signature"),
        &body
    ));
    assert!(!hos_billing::notify::verify_webhook_signature(
        "other",
        &header("x-hostingos-signature"),
        &body
    ));
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["event"], "invoice.paid");
    assert_eq!(v["data"]["total"], "120.00");
    let ep = env
        .billing
        .webhook_endpoint(t, endpoint.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ep.failure_count, ep.last_status_code), (0, Some(200)));
    env.cleanup().await;
}

async fn wait_for(env: &Env, sql: &str, notify_queue: Option<&str>) -> bool {
    for i in 0..150 {
        if env.count(sql).await > 0 {
            return true;
        }
        // repeat the notification in case the listener was still connecting
        if let Some(q) = notify_queue
            && i % 10 == 9
        {
            billing_support::notify_channel(&env.pool, NOTIFY_CHANNEL, q)
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// `Worker::run`: stuck-job recovery at start, `LISTEN/NOTIFY` wake-up (the
/// poll interval is far too long to explain the pickup) and graceful shutdown.
#[tokio::test]
async fn run_loop_recovers_stuck_jobs_listens_and_shuts_down() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    let queue = format!("test-{}", suffix());
    let job = |key: &str| {
        NewJob::new(JobKind::DailyAutomation.as_str(), json!({}))
            .tenant(t)
            .queue(&queue)
            .dedupe(format!("{queue}:{key}"))
    };
    // a job whose worker died an hour ago
    let stuck = jobs_repo::enqueue(&env.pool, &job("stuck"))
        .await
        .unwrap()
        .id();
    sqlx::query(
        "update jobs set status = 'running', attempts = 1, locked_by = 'dead-worker',
                locked_at = now() - interval '1 hour' where id = $1",
    )
    .bind(stuck)
    .execute(&env.pool)
    .await
    .unwrap();

    let w = Worker::new(
        env.pool.clone(),
        env.billing.clone(),
        WorkerConfig {
            queues: vec![queue.clone()],
            poll_interval: Duration::from_secs(600),
            recovery_interval: Duration::from_secs(600),
            listen_url: common::direct_url(),
            shutdown_grace: Duration::from_secs(10),
            ..WorkerConfig::default()
        },
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(w.run(async move {
        let _ = stopped.await;
    }));

    let done = "select count(*) from jobs where tenant_id = $1 and status = 'completed' and dedupe_key like '%:stuck'";
    assert!(
        wait_for(&env, done, None).await,
        "stuck job recovered and run"
    );
    let (_, attempts, _, _) = job_state(&env, "daily_automation").await;
    assert_eq!(attempts, 2, "taken back from the dead worker and run again");

    // a new job is picked up through NOTIFY
    jobs_repo::enqueue(&env.pool, &job("fresh")).await.unwrap();
    billing_support::notify_channel(&env.pool, NOTIFY_CHANNEL, &queue)
        .await
        .unwrap();
    let fresh = "select count(*) from jobs where tenant_id = $1 and status = 'completed' and dedupe_key like '%:fresh'";
    assert!(wait_for(&env, fresh, Some(&queue)).await, "woken by NOTIFY");

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), handle)
        .await
        .expect("worker stops")
        .unwrap()
        .unwrap();
    env.cleanup().await;
}

/// Scheduler: default tasks per tenant, one job per fire time even with two
/// schedulers ticking at once, next fire time stored, missed-run skip policy;
/// the scheduled job then runs through the worker.
#[tokio::test]
async fn scheduler_enqueues_each_fire_time_once() {
    let Some(env) = Env::new().await else { return };
    let t = env.tenant;
    // Scoped to the test tenant: an unscoped tick would enqueue (or skip)
    // the due tasks of every other tenant on the shared database (`demo`).
    let scoped = || SchedulerConfig {
        only_tenant: Some(t),
        ..SchedulerConfig::default()
    };
    let a = Scheduler::new(env.pool.clone(), scoped());
    let b = Scheduler::new(env.pool.clone(), scoped());
    assert_eq!(a.ensure_tenant_tasks(t).await.unwrap(), 5);
    let tasks = jobs_repo::list_scheduled_tasks(&env.pool, Some(t))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 5);
    let now = chrono::Utc::now();
    assert!(tasks.iter().all(|x| x.next_run_at.is_some_and(|n| n > now)));
    // ensuring again keeps the schedule
    a.ensure_tenant_tasks(t).await.unwrap();
    let again = jobs_repo::list_scheduled_tasks(&env.pool, Some(t))
        .await
        .unwrap();
    assert_eq!(
        tasks.iter().map(|x| x.next_run_at).collect::<Vec<_>>(),
        again.iter().map(|x| x.next_run_at).collect::<Vec<_>>()
    );

    sqlx::query(
        "update scheduled_tasks set next_run_at = now() - interval '5 minutes'
         where tenant_id = $1 and name = 'daily_automation'",
    )
    .bind(t)
    .execute(&env.pool)
    .await
    .unwrap();
    let (ra, rb) = tokio::join!(a.tick(), b.tick());
    ra.unwrap();
    rb.unwrap();
    let daily = "select count(*) from jobs where tenant_id = $1 and kind = 'daily_automation' and dedupe_key like 'sched:%'";
    assert_eq!(env.count(daily).await, 1);
    a.tick().await.unwrap();
    assert_eq!(env.count(daily).await, 1, "next fire time is in the future");
    let task = jobs_repo::list_scheduled_tasks(&env.pool, Some(t))
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.name == "daily_automation")
        .unwrap();
    assert!(task.next_run_at.unwrap() > chrono::Utc::now());
    assert_eq!(task.last_status, Some(hos_db::enums::TaskRunStatus::Ok));

    // missed by an hour with a one-minute grace: skipped
    let skip = Scheduler::new(
        env.pool.clone(),
        SchedulerConfig {
            missed_runs: MissedRunPolicy::Skip {
                grace: Duration::from_secs(60),
            },
            ..scoped()
        },
    );
    sqlx::query(
        "update scheduled_tasks set next_run_at = now() - interval '1 hour'
         where tenant_id = $1 and name = 'sync_usage'",
    )
    .bind(t)
    .execute(&env.pool)
    .await
    .unwrap();
    assert!(skip.tick().await.unwrap().skipped >= 1);
    assert_eq!(
        env.count("select count(*) from jobs where tenant_id = $1 and kind = 'sync_usage'")
            .await,
        0
    );
    let usage = jobs_repo::list_scheduled_tasks(&env.pool, Some(t))
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.name == "sync_usage")
        .unwrap();
    assert_eq!(usage.last_error.as_deref(), Some("missed run skipped"));
    assert!(usage.next_run_at.unwrap() > chrono::Utc::now());

    // the scheduled daily automation runs through the worker
    let stats = worker(&env).run_tenant_until_idle(t, 10).await.unwrap();
    assert_eq!(stats.completed, 1, "{stats:?}");
    let result: Value = sqlx::query_scalar(
        "select result from jobs where tenant_id = $1 and kind = 'daily_automation'",
    )
    .bind(t)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(result["invoices_created"], json!(0));
    env.cleanup().await;
}
