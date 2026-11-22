//! Imported staff never get a working login by themselves: they start
//! deactivated, unverified and without a password, and a re-run keeps what
//! an admin did since (activation, e-mail address).

use hos_import::config::Mappings;
use hos_import::entity::Entity;

use crate::common::{Env, db_config, fixture, secrets};

async fn staff_row(env: &Env) -> (String, String, bool, i64) {
    sqlx::query_as(
        "select u.email, u.status, u.email_verified_at is not null,
                (select count(*) from user_roles ur join roles r on r.id = ur.role_id
                  where ur.user_id = u.id and r.slug = 'support')
           from users u join import_id_map m on m.tenant_id = u.tenant_id and m.target_id = u.id
          where u.tenant_id = $1 and m.entity = 'staff' and m.source_id = '1'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn imported_staff_start_inactive_and_keep_admin_changes() {
    let Some(env) = Env::new().await else { return };
    let opts = hos_import::config::ImportOptions {
        entities: [Entity::Currencies, Entity::Staff].into_iter().collect(),
        pause_automation: false,
        ..hos_import::config::ImportOptions::default()
    };
    let mut maps = Mappings::default();
    maps.roles.insert("1".into(), "support".into());
    let id = env.draft(&db_config(), &opts, &maps, &secrets()).await;
    let run = env.run(id, "import", Box::new(fixture())).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);

    let (email, status, verified, roles) = staff_row(&env).await;
    assert_eq!(email, "ada.staff@example.net");
    assert_eq!(status, "disabled", "imported staff start deactivated");
    assert!(!verified, "imported staff e-mail is not verified");
    assert_eq!(roles, 1, "the mapped role is granted");

    // An admin activates the member; the source then changes its address.
    sqlx::query("update users set status = 'active' where tenant_id = $1 and email = 'ada.staff@example.net'")
        .bind(env.tenant)
        .execute(&env.pool)
        .await
        .unwrap();
    let mut src = fixture();
    for r in src.rows_mut(Entity::Staff) {
        *r = r.clone().with("email", "attacker@example.org");
    }
    let id = env.draft(&db_config(), &opts, &maps, &secrets()).await;
    let rerun = env.run(id, "import", Box::new(src)).await;
    assert_eq!(rerun.status, "completed", "error: {:?}", rerun.error);
    let (email, status, _, _) = staff_row(&env).await;
    assert_eq!(email, "ada.staff@example.net", "a re-run keeps the address");
    assert_eq!(status, "active", "a re-run keeps the activation");

    env.cleanup().await;
}
