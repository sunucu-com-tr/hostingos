//! A login registered before the import is never handed the imported
//! account silently: an unverified one is not adopted (conflict in the
//! report); a verified one is linked, but its password is cleared and its
//! sessions end.

use hos_db::repo::import as repo;
use hos_import::entity::Entity;
use uuid::Uuid;

use crate::common::{Env, fixture};

async fn preregister(env: &Env, email: &str, verified: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into users (id, tenant_id, email, password_hash, kind, first_name, last_name, locale,
                            email_verified_at, status)
         values ($1, $2, $3, 'planted-hash', 'client', 'Pre', 'Registered', 'tr',
                 case when $4 then now() end, 'active')",
    )
    .bind(id)
    .bind(env.tenant)
    .bind(email)
    .bind(verified)
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into sessions (id, tenant_id, user_id, token_hash, expires_at)
         values ($1, $2, $3, $4, now() + interval '1 day')",
    )
    .bind(Uuid::now_v7())
    .bind(env.tenant)
    .bind(id)
    .bind(format!("test-{id}"))
    .execute(&env.pool)
    .await
    .unwrap();
    id
}

async fn owner_of(env: &Env, email: &str) -> Option<Uuid> {
    sqlx::query_scalar(
        "select cu.user_id from client_users cu join clients c on c.id = cu.client_id
          where cu.tenant_id = $1 and c.email = $2 and cu.is_owner",
    )
    .bind(env.tenant)
    .bind(email)
    .fetch_optional(&env.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn preregistered_logins_are_not_adopted_silently() {
    let Some(env) = Env::new().await else { return };
    // An attacker registered the client's address (never verified); the
    // owner of the other address registered and verified it.
    let planted = preregister(&env, "ayse@example.net", false).await;
    let verified = preregister(&env, "john@example.net", true).await;
    let opts = hos_import::config::ImportOptions {
        entities: [
            Entity::Currencies,
            Entity::ClientGroups,
            Entity::Clients,
            Entity::Users,
            Entity::UserLinks,
        ]
        .into_iter()
        .collect(),
        pause_automation: false,
        ..hos_import::config::ImportOptions::default()
    };
    let run = env.import(fixture(), opts).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);

    assert_ne!(
        owner_of(&env, "ayse@example.net").await,
        Some(planted),
        "the unverified login does not own the imported account"
    );
    let still: String =
        sqlx::query_scalar("select password_hash from users where tenant_id = $1 and id = $2")
            .bind(env.tenant)
            .bind(planted)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(still, "planted-hash", "the planted login is left alone");

    assert_eq!(owner_of(&env, "john@example.net").await, Some(verified));
    let (hash, sessions): (Option<String>, i64) = sqlx::query_as(
        "select password_hash, (select count(*) from sessions s where s.user_id = u.id)
           from users u where u.tenant_id = $1 and u.id = $2",
    )
    .bind(env.tenant)
    .bind(verified)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(hash, None, "an adopted login must reset its password");
    assert_eq!(sessions, 0, "its sessions ended");

    let msgs = repo::list_messages(&env.pool, env.tenant, run.id, None, 1000)
        .await
        .unwrap();
    assert!(msgs.iter().any(|m| m.code == "login_unverified"));
    assert!(msgs.iter().any(|m| m.code == "login_adopted"));
    env.cleanup().await;
}
