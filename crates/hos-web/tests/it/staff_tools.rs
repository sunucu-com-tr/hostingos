//! Staff tools: order screening rules and ban list (held orders), the to-do
//! list (permissions, CSRF, other tenant, assignee checks) and the client
//! credit top-up limits.

use crate::biz_support::{login, owner, owner_with_email, paid_order, post, product, staff, user};
use crate::common::TestApp;
use hos_core::domain::UserKind;
use hos_db::repo::fraud as frepo;
use hos_db::repo::tasks::{self as trepo, TaskFilter, TaskStatus};
use hos_db::repo::{identity, invoices};
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;

async fn order_status(
    app: &TestApp,
    invoice: hos_core::ids::InvoiceId,
) -> (String, serde_json::Value) {
    sqlx::query_as::<_, (String, serde_json::Value)>(
        "select status, fraud_details from orders where tenant_id = $1 and invoice_id = $2",
    )
    .bind(app.tenant_id)
    .bind(invoice)
    .fetch_one(app.pool())
    .await
    .expect("order")
}

#[tokio::test]
async fn fraud_rules_hold_banned_and_disposable_orders() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("billing.auto_accept_orders", json!(true)).await;
    let product = product(&app).await;

    // support has no fraud.manage
    staff(&mut app, "support").await;
    assert_eq!(app.get("/admin/fraud").await.status, StatusCode::FORBIDDEN);
    let csrf = staff(&mut app, "billing").await;
    assert_eq!(app.get("/admin/fraud").await.status, StatusCode::OK);
    let rules = [
        ("enabled", "1"),
        ("block_disposable", "1"),
        ("hold_score", "50"),
        ("max_orders_per_ip", "3"),
        ("max_clients_per_ip", "1"),
        ("max_orders_per_client", "5"),
    ];
    assert_eq!(
        app.post("/admin/fraud", &rules).await.status,
        StatusCode::FORBIDDEN,
        "missing CSRF"
    );
    assert_eq!(
        post(&mut app, "/admin/fraud", &csrf, &rules).await.status,
        StatusCode::SEE_OTHER
    );

    // Ban list: invalid values are refused; a domain ban covers subdomains.
    post(
        &mut app,
        "/admin/fraud/bans",
        &csrf,
        &[("kind", "ip"), ("value", "not-an-ip")],
    )
    .await;
    post(
        &mut app,
        "/admin/fraud/bans",
        &csrf,
        &[("kind", "domain"), ("value", "bad_domain")],
    )
    .await;
    assert!(
        frepo::list_bans(app.pool(), app.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
    let r = post(
        &mut app,
        "/admin/fraud/bans",
        &csrf,
        &[
            ("kind", "domain"),
            ("value", "@Spam.Example"),
            ("reason", "<b>chargebacks</b>"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    post(
        &mut app,
        "/admin/fraud/bans",
        &csrf,
        &[("kind", "ip"), ("value", "203.0.113.0/24")],
    )
    .await;
    let bans = frepo::list_bans(app.pool(), app.tenant_id).await.unwrap();
    assert_eq!(bans.len(), 2);
    let page = app.get("/admin/fraud").await;
    assert!(
        page.body.contains("spam.example") && !page.body.contains("<b>chargebacks</b>"),
        "escaped"
    );
    let hits = frepo::matching_bans(
        app.pool(),
        app.tenant_id,
        "x@mail.spam.example",
        Some("203.0.113.9".parse().unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(hits.len(), 2, "subdomain + network");

    // Another tenant cannot remove a ban.
    let Some(mut other) = TestApp::new().await else {
        return;
    };
    let other_csrf = staff(&mut other, "owner").await;
    let r = post(
        &mut other,
        &format!("/admin/fraud/bans/{}/delete", bans[0].id),
        &other_csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    other.cleanup().await;

    // Orders: banned domain and disposable mailbox are held; a clean one is accepted.
    let (_, banned, _) = owner_with_email(
        &app,
        "yasakli",
        &format!("b-{}@mail.spam.example", uuid::Uuid::now_v7().simple()),
    )
    .await;
    let (_, throwaway, _) = owner_with_email(
        &app,
        "gecici",
        &format!("t-{}@mailinator.com", uuid::Uuid::now_v7().simple()),
    )
    .await;
    let (_, clean, _) = owner(&app, "temiz").await;
    let (inv, _) = paid_order(&app, banned, product).await;
    let (status, details) = order_status(&app, inv).await;
    assert_eq!(status, "fraud");
    assert!(
        details["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["rule"] == "banned"),
        "{details}"
    );
    let (inv, _) = paid_order(&app, throwaway, product).await;
    assert_eq!(order_status(&app, inv).await.0, "fraud");
    let (inv, _) = paid_order(&app, clean, product).await;
    let (status, details) = order_status(&app, inv).await;
    assert_eq!(status, "accepted", "{details}");
    app.cleanup().await;
}

#[tokio::test]
async fn tasks_are_tenant_scoped_and_checked() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // A custom role without tasks.manage.
    identity::create_role(
        app.pool(),
        app.tenant_id,
        "readonly",
        "Salt okunur",
        None,
        false,
    )
    .await
    .unwrap();
    staff(&mut app, "readonly").await;
    assert_eq!(app.get("/admin/tasks").await.status, StatusCode::FORBIDDEN);

    let csrf = staff(&mut app, "support").await;
    let colleague = user(
        &app,
        &format!("c-{}@staff.test", uuid::Uuid::now_v7().simple()),
        UserKind::Staff,
        Some("sales"),
    )
    .await;
    let (client_user, client, _) = owner(&app, "gorev").await;
    let colleague_s = colleague.to_string();
    let client_s = client.to_string();
    let fields = [
        ("title", "Yedekleri kontrol et"),
        ("body", "<script>x</script>"),
        ("due_date", "2020-01-01"),
        ("priority", "high"),
        ("assigned_to", colleague_s.as_str()),
        ("client_id", client_s.as_str()),
    ];
    assert_eq!(
        app.post("/admin/tasks", &fields).await.status,
        StatusCode::FORBIDDEN,
        "missing CSRF"
    );
    assert_eq!(
        post(&mut app, "/admin/tasks", &csrf, &fields).await.status,
        StatusCode::SEE_OTHER
    );
    // A client user is not a valid assignee.
    let client_user_s = client_user.to_string();
    let r = post(
        &mut app,
        "/admin/tasks",
        &csrf,
        &[("title", "x"), ("assigned_to", client_user_s.as_str())],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let tasks = trepo::list_tasks(app.pool(), app.tenant_id, &TaskFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    let t = &tasks[0];
    assert_eq!(t.task.assigned_to, Some(colleague));
    let page = app.get("/admin/tasks").await;
    assert!(
        page.body.contains("Yedekleri kontrol et") && page.body.contains("badge--danger"),
        "overdue shown"
    );
    assert!(
        app.get(&format!("/admin/tasks?client={client}"))
            .await
            .body
            .contains("Yedekleri")
    );
    assert!(
        !app.get(&format!("/admin/tasks/{}", t.task.id))
            .await
            .body
            .contains("<script>x"),
        "escaped"
    );

    // Another tenant cannot touch it.
    let Some(mut other) = TestApp::new().await else {
        return;
    };
    let other_csrf = staff(&mut other, "owner").await;
    let r = post(
        &mut other,
        &format!("/admin/tasks/{}/status", t.task.id),
        &other_csrf,
        &[("status", "done")],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        other
            .get(&format!("/admin/tasks/{}", t.task.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    other.cleanup().await;

    let r = post(
        &mut app,
        &format!("/admin/tasks/{}/status", t.task.id),
        &csrf,
        &[("status", "done"), ("back", "https://evil.example/")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("/admin/tasks"), "{}", r.location());
    let done = trepo::get_task(app.pool(), app.tenant_id, t.task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, TaskStatus::Done);
    assert!(done.completed_at.is_some());
    assert_eq!(
        post(
            &mut app,
            &format!("/admin/tasks/{}/delete", t.task.id),
            &csrf,
            &[]
        )
        .await
        .status,
        StatusCode::SEE_OTHER
    );
    assert!(
        trepo::get_task(app.pool(), app.tenant_id, t.task.id)
            .await
            .unwrap()
            .is_none()
    );
    app.cleanup().await;
}

#[tokio::test]
async fn credit_top_up_limits_apply() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("billing.add_funds_min", json!("50")).await;
    app.setting("billing.add_funds_max", json!("200")).await;
    app.setting("billing.credit_balance_max", json!("300"))
        .await;
    let (_, client, email) = owner(&app, "bakiye").await;
    let billing = &app.state.billing;
    assert!(
        billing
            .order_add_funds(app.tenant_id, client, Decimal::from(49))
            .await
            .is_err()
    );
    assert!(
        billing
            .order_add_funds(app.tenant_id, client, Decimal::from(201))
            .await
            .is_err()
    );
    billing
        .order_add_funds(app.tenant_id, client, Decimal::from(200))
        .await
        .expect("within limits");
    // The unpaid top-up counts against the balance cap.
    assert!(
        billing
            .order_add_funds(app.tenant_id, client, Decimal::from(150))
            .await
            .is_err()
    );
    billing
        .order_add_funds(app.tenant_id, client, Decimal::from(100))
        .await
        .expect("up to the cap");
    let open = invoices::list_invoices(
        app.pool(),
        app.tenant_id,
        &invoices::InvoiceFilter {
            client_id: Some(client),
            ..invoices::InvoiceFilter::default()
        },
        hos_db::Page::first(10),
    )
    .await
    .unwrap();
    assert_eq!(open.total, 2);
    login(&mut app, &email).await;
    let page = app.get("/client/billing").await;
    assert_eq!(page.status, StatusCode::OK);
    app.cleanup().await;
}
