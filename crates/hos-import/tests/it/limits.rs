//! Oversized source values are cut before they are mapped and listed in
//! the report.

use hos_db::repo::import as repo;
use hos_import::entity::Entity;
use hos_import::row::MAX_FIELD_CHARS;

use crate::common::{Env, fixture};

#[tokio::test]
async fn oversized_values_are_cut_and_reported() {
    let Some(env) = Env::new().await else { return };
    let mut src = fixture();
    for r in src.rows_mut(Entity::Clients) {
        if r.id() == 1 {
            *r = r.clone().with("notes", "x".repeat(MAX_FIELD_CHARS + 50));
        }
    }
    let opts = hos_import::config::ImportOptions {
        entities: [Entity::Currencies, Entity::ClientGroups, Entity::Clients]
            .into_iter()
            .collect(),
        pause_automation: false,
        ..hos_import::config::ImportOptions::default()
    };
    let run = env.import(src, opts).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);
    let msgs = repo::list_messages(&env.pool, env.tenant, run.id, None, 1000)
        .await
        .unwrap();
    assert!(
        msgs.iter().any(|m| m.code == "truncated"
            && m.source_id.as_deref() == Some("1")
            && m.message.contains("notes")),
        "{msgs:?}"
    );
    assert_eq!(env.count("clients").await, 2);
    env.cleanup().await;
}
