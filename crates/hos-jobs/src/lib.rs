//! HostingOS background jobs: a Postgres-backed queue worker
//! (`FOR UPDATE SKIP LOCKED`, `LISTEN/NOTIFY`, backoff, stuck-job recovery,
//! graceful shutdown) and a cron scheduler over `scheduled_tasks`.
//!
//! Job kinds, payloads and `enqueue_*` helpers live in `hos_billing::jobs`
//! (the billing engine enqueues them); the handlers are here.
//!
//! ```ignore
//! let billing = Billing::new(pool.clone(), secret_box, BillingConfig::new(base_url));
//! let worker = Worker::new(pool.clone(), billing, WorkerConfig {
//!     listen_url: Some(direct_url), ..WorkerConfig::default()
//! });
//! let scheduler = Scheduler::new(pool.clone(), SchedulerConfig::default());
//! tokio::try_join!(worker.run(shutdown.clone()), scheduler.run(shutdown))?;
//! ```

pub mod ai;
pub mod handlers;
pub mod scheduler;
pub mod worker;

pub use handlers::{HandlerContext, HandlerError, Outcome};
pub use scheduler::{MissedRunPolicy, Scheduler, SchedulerConfig};
pub use worker::{RunStats, Worker, WorkerConfig};
