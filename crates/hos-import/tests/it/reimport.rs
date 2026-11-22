//! Re-importing after go-live: the shop's own activity since the last
//! import is detected (the web wizard and the CLI then ask for an explicit
//! confirmation), and credit the client used in HostingOS is never added
//! back by the re-run.

use hos_db::repo::import as repo;
use hos_import::config::ImportOptions;
use hos_import::entity::Entity;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::common::{Env, fixture};

fn options() -> ImportOptions {
    ImportOptions {
        entities: [
            Entity::Currencies,
            Entity::ClientGroups,
            Entity::Clients,
            Entity::Credits,
        ]
        .into_iter()
        .collect(),
        pause_automation: false,
        ..ImportOptions::default()
    }
}

async fn balance(env: &Env, client: Uuid) -> Decimal {
    sqlx::query_scalar(
        "select coalesce(sum(amount), 0) from credits where tenant_id = $1 and client_id = $2",
    )
    .bind(env.tenant)
    .bind(client)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn reimport_after_go_live_keeps_spent_credit_spent() {
    let Some(env) = Env::new().await else { return };
    let run = env.import(fixture(), options()).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);
    assert_eq!(
        repo::live_activity(&env.pool, env.tenant).await.unwrap(),
        None,
        "nothing happened since the import"
    );
    let client: Uuid = sqlx::query_scalar(
        "select id from clients where tenant_id = $1 and email = 'ayse@example.net'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        balance(&env, client).await,
        Decimal::new(25, 0),
        "WHMCS credit"
    );
    let currency: String = sqlx::query_scalar(
        "select currency from credits where tenant_id = $1 and client_id = $2 limit 1",
    )
    .bind(env.tenant)
    .bind(client)
    .fetch_one(&env.pool)
    .await
    .unwrap();

    // Go-live: the client pays 10 with credit in HostingOS.
    sqlx::query(
        "insert into credits (tenant_id, client_id, amount, currency, kind, description)
         values ($1, $2, -10, $3, 'manual', 'Used in HostingOS')",
    )
    .bind(env.tenant)
    .bind(client)
    .bind(&currency)
    .execute(&env.pool)
    .await
    .unwrap();
    let live = repo::live_activity(&env.pool, env.tenant)
        .await
        .unwrap()
        .expect("live activity");
    assert_eq!(live.records, 1);

    let rerun = env.import(fixture(), options()).await;
    assert_eq!(rerun.status, "completed", "error: {:?}", rerun.error);
    assert_eq!(
        balance(&env, client).await,
        Decimal::new(15, 0),
        "the credit used in HostingOS stays used"
    );
    env.cleanup().await;
}
