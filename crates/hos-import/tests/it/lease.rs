//! The run lease: a worker that lost its run (another worker took it over)
//! writes no further batch and does not finish the run; the lease holder's
//! heartbeat keeps the run.

use std::collections::BTreeMap;

use async_trait::async_trait;
use hos_db::repo::import as repo;
use hos_import::entity::Entity;
use hos_import::error::ImportResult;
use hos_import::source::fixture::FixtureSource;
use hos_import::source::{Attachment, AttachmentOwner, Batch, Flavor, Probe, Source, Support};
use sqlx::PgPool;

use crate::common::{Env, db_config, fixture, secrets};

/// The fixture, but another worker takes the run over right before the
/// clients are read (as after a long stall of this worker).
struct Stolen {
    inner: FixtureSource,
    pool: PgPool,
    tenant: hos_core::ids::TenantId,
}

#[async_trait]
impl Source for Stolen {
    fn flavor(&self) -> Flavor {
        self.inner.flavor()
    }
    fn supports(&self, entity: Entity) -> Support {
        self.inner.supports(entity)
    }
    async fn probe(&mut self) -> ImportResult<Probe> {
        self.inner.probe().await
    }
    async fn count(&mut self, entity: Entity) -> ImportResult<Option<u64>> {
        self.inner.count(entity).await
    }
    async fn count_table(&mut self, table: &str) -> ImportResult<Option<u64>> {
        self.inner.count_table(table).await
    }
    async fn fetch(&mut self, entity: Entity, cursor: i64, limit: u32) -> ImportResult<Batch> {
        if entity == Entity::Clients {
            sqlx::query("update import_runs set locked_by = 'other-worker' where tenant_id = $1 and status = 'running'")
                .bind(self.tenant)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        self.inner.fetch(entity, cursor, limit).await
    }
    async fn settings(&mut self) -> ImportResult<BTreeMap<String, String>> {
        self.inner.settings().await
    }
    async fn attachment(
        &mut self,
        owner: AttachmentOwner,
        owner_id: i64,
        index: usize,
        stored_name: &str,
        max_bytes: u64,
    ) -> ImportResult<Attachment> {
        self.inner
            .attachment(owner, owner_id, index, stored_name, max_bytes)
            .await
    }
}

#[tokio::test]
async fn a_worker_that_lost_the_run_writes_nothing_more() {
    let Some(env) = Env::new().await else { return };
    let opts = hos_import::config::ImportOptions {
        entities: [Entity::Currencies, Entity::ClientGroups, Entity::Clients]
            .into_iter()
            .collect(),
        pause_automation: false,
        ..hos_import::config::ImportOptions::default()
    };
    let id = env
        .draft(&db_config(), &opts, &Default::default(), &secrets())
        .await;
    let source = Stolen {
        inner: fixture(),
        pool: env.pool.clone(),
        tenant: env.tenant,
    };
    let row = env
        .runner()
        .queue_and_run(env.tenant, id, "import", None, Some(Box::new(source)))
        .await
        .expect("run")
        .expect("queued");
    assert_eq!(env.count("clients").await, 0, "no batch after the takeover");
    assert_eq!(row.status, "running", "not finished by the old worker");
    assert_eq!(row.locked_by.as_deref(), Some("other-worker"));

    // The holder's heartbeat / finish work, a stranger's do not.
    assert!(
        repo::heartbeat(&env.pool, env.tenant, id, "other-worker")
            .await
            .unwrap()
    );
    assert!(
        !repo::heartbeat(&env.pool, env.tenant, id, "test-worker")
            .await
            .unwrap()
    );
    let end = repo::RunEnd {
        status: "failed",
        progress: &serde_json::json!({}),
        result: &serde_json::json!({}),
        error: Some("x"),
    };
    assert!(
        !repo::finish_run(&env.pool, env.tenant, id, "test-worker", &end)
            .await
            .unwrap()
    );
    assert!(
        repo::finish_run(&env.pool, env.tenant, id, "other-worker", &end)
            .await
            .unwrap()
    );
    env.cleanup().await;
}
