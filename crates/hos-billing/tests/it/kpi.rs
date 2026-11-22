//! Admin dashboard KPI trends (`hos_db::repo::stats::kpi_trends`) against the
//! real database: stocks at period ends, flows per period, MRR movement.
//! `kpi_trends_on_volume` (ignored) fills a test tenant with generated rows
//! and prints `EXPLAIN ANALYZE` of the heaviest query.

use crate::common::Env;
use hos_core::domain::ProductType;
use hos_db::repo::stats::{self, KpiRange};
use serde_json::json;

async fn services(env: &Env, n: i64, every_tenth_terminated: bool) {
    let product = env
        .product("KPI", ProductType::Other, None, json!({}), "10", "0")
        .await;
    let client = env.client("Kpi").await;
    sqlx::query(
        "insert into services (tenant_id, client_id, product_id, billing_cycle, recurring_amount,
                               currency, registration_date, status, created_at, terminated_at)
         select $1, $2, $3, 'monthly', 10, 'TRY', current_date,
                case when $5 and g % 10 = 0 then 'terminated' else 'active' end,
                now() - ((g % 400) || ' days')::interval - interval '1 hour',
                case when $5 and g % 10 = 0 then now() - ((g % 25) || ' days')::interval end
           from generate_series(1, $4) g",
    )
    .bind(env.tenant)
    .bind(client)
    .bind(product)
    .bind(n)
    .bind(every_tenth_terminated)
    .execute(&env.pool)
    .await
    .expect("services");
}

#[tokio::test]
async fn kpi_trends_count_stocks_and_flows() {
    let Some(env) = Env::new().await else { return };
    let product = env
        .product("KPI", ProductType::Other, None, json!({}), "10", "0")
        .await;
    let client = env.client("Kpi").await;
    // created 40 d ago (active), 10 d ago (active), 50 d ago (terminated 5 d ago)
    for (created, terminated) in [(40, None), (10, None), (50, Some(5))] {
        sqlx::query(
            "insert into services (tenant_id, client_id, product_id, billing_cycle, recurring_amount,
                                   currency, registration_date, status, created_at, terminated_at)
             values ($1, $2, $3, 'monthly', 10, 'TRY', current_date, $4,
                     now() - make_interval(days => $5), now() - make_interval(days => $6))",
        )
        .bind(env.tenant)
        .bind(client)
        .bind(product)
        .bind(if terminated.is_some() { "terminated" } else { "active" })
        .bind(created)
        .bind(terminated)
        .execute(&env.pool)
        .await
        .expect("service");
    }
    let k = stats::kpi_trends(&env.pool, env.tenant, KpiRange::Month)
        .await
        .unwrap();
    assert_eq!(k.active_services.points.len(), 30);
    assert_eq!(k.active_services.current, 2.0);
    // 30 days ago: the 40-day and the 50-day one.
    assert_eq!(k.active_services.previous, 2.0);
    assert_eq!(
        (k.cancellations.current, k.cancellations.previous),
        (1.0, 0.0)
    );
    assert!((k.mrr.current - 20.0).abs() < 1e-6, "{:?}", k.mrr);
    // +10 (new) −10 (terminated) this period.
    assert!(k.mrr_net.current.abs() < 1e-6, "{:?}", k.mrr_net);
    // The client created by the fixture is a new client of this period.
    assert!(k.new_clients.current >= 1.0);
    env.cleanup().await;
}

#[tokio::test]
#[ignore = "fills a test tenant with 50k services; run by hand for EXPLAIN"]
async fn kpi_trends_on_volume() {
    let Some(env) = Env::new().await else { return };
    services(&env, 50_000, true).await;
    let started = std::time::Instant::now();
    let k = stats::kpi_trends(&env.pool, env.tenant, KpiRange::Year)
        .await
        .unwrap();
    eprintln!(
        "kpi_trends(12m) on 50k services: {:?}, active {}",
        started.elapsed(),
        k.active_services.current
    );
    let plan: Vec<String> = sqlx::query_scalar(
        "explain (analyze, buffers)
         with ev as (select s.created_at as st, coalesce(s.terminated_at, s.cancelled_at) as en, 1::float8 as w
                       from services s where s.tenant_id = $1 and s.status not in ('pending', 'failed')),
              e as (select st, en, w from ev where en is null or en > st)
         select -1 as b, coalesce(sum(w) filter (where st < now() - interval '728 days'), 0) from e
         union all
         select floor(extract(epoch from (st - (now() - interval '728 days'))) / 604800)::int, sum(w)
           from e where st >= now() - interval '728 days' group by 1",
    )
    .bind(env.tenant)
    .fetch_all(&env.pool)
    .await
    .unwrap();
    for l in plan {
        eprintln!("{l}");
    }
    env.cleanup().await;
}
