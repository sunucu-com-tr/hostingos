//! An affiliate account the client opened in HostingOS before the import is
//! linked to the WHMCS affiliate, but keeps its own terms (commission,
//! code, currency) and its own money (no WHMCS commissions, payouts or
//! balance are added to it) on the first run and on every re-run.

use hos_import::config::ImportOptions;
use hos_import::entity::Entity;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::common::{Env, fixture};

fn options(entities: &[Entity]) -> ImportOptions {
    ImportOptions {
        entities: entities.iter().copied().collect(),
        pause_automation: false,
        ..ImportOptions::default()
    }
}

type Terms = (
    String,
    Option<String>,
    Option<Decimal>,
    Option<bool>,
    String,
);

async fn terms(env: &Env, id: Uuid) -> Terms {
    sqlx::query_as(
        "select code::text, commission_type, commission_value, commission_recurring, currency
           from affiliates where tenant_id = $1 and id = $2",
    )
    .bind(env.tenant)
    .bind(id)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_native_affiliate_keeps_its_terms() {
    let Some(env) = Env::new().await else { return };
    let base = [Entity::Currencies, Entity::ClientGroups, Entity::Clients];
    let run = env.import(fixture(), options(&base)).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);
    // WHMCS affiliate 1 belongs to this client (15 %, one-time).
    let client: Uuid = sqlx::query_scalar(
        "select id from clients where tenant_id = $1 and email = 'john@example.net'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    let native = Uuid::now_v7();
    sqlx::query(
        "insert into affiliates (id, tenant_id, client_id, code, commission_type, commission_value,
                                 commission_recurring, currency)
         values ($1, $2, $3, 'NATIVE7CODE', 'fixed', 7, true, 'TRY')",
    )
    .bind(native)
    .bind(env.tenant)
    .bind(client)
    .execute(&env.pool)
    .await
    .unwrap();
    let before = terms(&env, native).await;

    let mut all = base.to_vec();
    all.extend([
        Entity::Products,
        Entity::Services,
        Entity::Affiliates,
        Entity::AffiliateAccounts,
        Entity::AffiliateHistory,
        Entity::AffiliateWithdrawals,
    ]);
    for pass in ["first", "re-run"] {
        let run = env.import(fixture(), options(&all)).await;
        assert_eq!(run.status, "completed", "{pass}: {:?}", run.error);
        assert_eq!(terms(&env, native).await, before, "{pass}: terms kept");
        let linked: Uuid = sqlx::query_scalar(
            "select target_id from import_id_map
              where tenant_id = $1 and entity = 'affiliates' and source_id = '1'",
        )
        .bind(env.tenant)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(linked, native, "{pass}: the WHMCS affiliate is linked");
        let money: (i64, i64) = sqlx::query_as(
            "select (select count(*) from affiliate_commissions where tenant_id = $1 and affiliate_id = $2),
                    (select count(*) from affiliate_payouts where tenant_id = $1 and affiliate_id = $2)",
        )
        .bind(env.tenant)
        .bind(native)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(
            money,
            (0, 0),
            "{pass}: no WHMCS money on the native account"
        );
        let referrals: i64 = sqlx::query_scalar(
            "select count(*) from affiliate_referrals where tenant_id = $1 and affiliate_id = $2",
        )
        .bind(env.tenant)
        .bind(native)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(referrals, 1, "{pass}: the referral is linked");
        let msgs = hos_db::repo::import::list_messages(&env.pool, env.tenant, run.id, None, 1000)
            .await
            .unwrap();
        assert!(
            msgs.iter().any(|m| m.code == "native_affiliate_balance"),
            "{pass}: the WHMCS balance is reported"
        );
    }
    // The other WHMCS affiliate (no native account) is created as usual.
    assert_eq!(env.count("affiliates").await, 2);
    env.cleanup().await;
}
