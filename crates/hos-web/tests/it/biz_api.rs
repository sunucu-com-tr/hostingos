//! REST API of the business features: the public network status, the
//! caller's own affiliate account (account owner only) and the staff to-do
//! list (`tasks.manage`). Negative cases: other tenant, other client,
//! sub-user without the owner area, missing permission, staff vs client
//! keys, session calls without the CSRF token, unknown ids.

use crate::biz_support::{api, api_key, owner, staff, sub_user, user};
use crate::common::TestApp;
use chrono::Utc;
use hos_core::domain::UserKind;
use hos_core::money::Currency;
use hos_db::repo::affiliates as arepo;
use hos_db::repo::identity;
use hos_db::repo::status::{
    self as srepo, ComponentInput, ComponentStatus, IncidentImpact, IncidentInput, IncidentKind,
    IncidentStatus,
};
use http::StatusCode;
use serde_json::json;

/// A JSON API call without any `Authorization` header.
async fn anon(app: &TestApp, path: &str) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let res = app
        .router
        .clone()
        .oneshot(
            http::Request::builder()
                .uri(path)
                .header("host", &app.host)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn staff_key(app: &TestApp, role: &str) -> (hos_core::ids::UserId, String) {
    let email = format!("api-{role}-{}@staff.test", uuid::Uuid::now_v7().simple());
    let id = user(app, &email, UserKind::Staff, Some(role)).await;
    (id, api_key(app, id).await)
}

#[tokio::test]
async fn status_api_is_public_and_shows_only_public_data() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let save = |name: &str, public: bool, status: ComponentStatus| ComponentInput {
        name: name.into(),
        group_name: "Hosting".into(),
        description: None,
        status,
        integration_id: None,
        is_public: public,
        sort_order: 0,
    };
    let public = srepo::save_component(
        app.pool(),
        app.tenant_id,
        None,
        &save("Web İstanbul", true, ComponentStatus::Degraded),
    )
    .await
    .unwrap();
    let hidden = srepo::save_component(
        app.pool(),
        app.tenant_id,
        None,
        &save("İç ağ", false, ComponentStatus::MajorOutage),
    )
    .await
    .unwrap();
    let mut conn = app.pool().acquire().await.unwrap();
    let open = srepo::create_incident(
        &mut conn,
        app.tenant_id,
        &IncidentInput {
            kind: IncidentKind::Incident,
            title: "Yavaşlık".into(),
            impact: IncidentImpact::Minor,
            status: IncidentStatus::Investigating,
            scheduled_start: None,
            scheduled_end: None,
            is_public: true,
            components: vec![public.id, hidden.id],
        },
        "İnceleniyor",
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    let private = srepo::create_incident(
        &mut conn,
        app.tenant_id,
        &IncidentInput {
            kind: IncidentKind::Incident,
            title: "Gizli olay".into(),
            impact: IncidentImpact::Major,
            status: IncidentStatus::Investigating,
            scheduled_start: None,
            scheduled_end: None,
            is_public: false,
            components: vec![hidden.id],
        },
        "gizli",
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    drop(conn);

    // Anonymous (no key at all) is enough.
    let (s, v) = anon(&app, "/api/v1/status").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["level"], "minor");
    let comps = v["components"].as_array().unwrap();
    assert_eq!(comps.len(), 1, "{v}");
    assert_eq!(comps[0]["name"], "Web İstanbul");
    assert_eq!(comps[0]["status"], "degraded");
    let incidents = v["incidents"].as_array().unwrap();
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0]["title"], "Yavaşlık");
    assert_eq!(
        incidents[0]["component_ids"],
        json!([public.id.to_string()]),
        "private component ids are left out"
    );
    assert!(!v.to_string().contains("Gizli"));
    assert!(!v.to_string().contains("İç ağ"));

    let (s, v) = anon(&app, "/api/v1/status/incidents").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["total"], 1);
    let (s, v) = anon(&app, &format!("/api/v1/status/incidents/{}", open.id)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["updates"][0]["body"], "İnceleniyor");
    // A private incident, a malformed id and another tenant's incident: 404.
    for path in [
        format!("/api/v1/status/incidents/{}", private.id),
        "/api/v1/status/incidents/x%27%20or%201=1".to_owned(),
    ] {
        let (s, _) = anon(&app, &path).await;
        assert!(
            s == StatusCode::NOT_FOUND || s == StatusCode::BAD_REQUEST,
            "{path}: {s}"
        );
    }
    let other = TestApp::new().await.unwrap();
    let (s, _) = anon(&other, &format!("/api/v1/status/incidents/{}", open.id)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "other tenant");
    let (s, v) = anon(&other, "/api/v1/status").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["components"], json!([]));
    other.cleanup().await;
    app.cleanup().await;
}

#[tokio::test]
async fn affiliate_api_is_the_callers_own_account_only() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (a_user, a_client, _) = owner(&app, "ortak").await;
    let (b_user, b_client, _) = owner(&app, "baska").await;
    let a_key = api_key(&app, a_user).await;
    let b_key = api_key(&app, b_user).await;

    // Not enrolled yet → 404.
    let (s, _) = api(&app, "GET", "/api/v1/affiliate", &a_key, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let a = arepo::create_affiliate(app.pool(), app.tenant_id, a_client, "ORTAK1", Currency::TRY)
        .await
        .unwrap();
    arepo::create_affiliate(app.pool(), app.tenant_id, b_client, "BASKA1", Currency::TRY)
        .await
        .unwrap();
    let (s, v) = api(&app, "GET", "/api/v1/affiliate", &a_key, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["id"], a.id.to_string());
    assert_eq!(v["code"], "ORTAK1");
    assert!(v["link"].as_str().unwrap().ends_with("/r/ORTAK1"), "{v}");
    assert_eq!(v["balance"], "0.00");
    for sub in ["commissions", "referrals", "payouts"] {
        let (s, v) = api(
            &app,
            "GET",
            &format!("/api/v1/affiliate/{sub}"),
            &a_key,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{sub}: {v}");
        assert_eq!(v["total"], 0, "{sub}");
    }
    // B sees its own account, never A's.
    let (_, v) = api(&app, "GET", "/api/v1/affiliate", &b_key, None).await;
    assert_eq!(v["code"], "BASKA1");

    // A sub-user (even with every other area) is not the owner → 403.
    let sub_email = sub_user(
        &app,
        a_client,
        &["services", "invoices", "orders", "tickets"],
    )
    .await;
    let sub = identity::user_by_email(app.pool(), app.tenant_id, &sub_email)
        .await
        .unwrap()
        .unwrap();
    let sub_key = api_key(&app, sub.id).await;
    let (s, _) = api(&app, "GET", "/api/v1/affiliate", &sub_key, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = api(&app, "GET", "/api/v1/affiliate/commissions", &sub_key, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Staff keys and anonymous callers.
    let (_, owner_key) = staff_key(&app, "owner").await;
    let (s, _) = api(&app, "GET", "/api/v1/affiliate", &owner_key, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "client keys only");
    let (s, _) = anon(&app, "/api/v1/affiliate").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // A key of this tenant is unknown on another tenant.
    let other = TestApp::new().await.unwrap();
    let (s, _) = api(&other, "GET", "/api/v1/affiliate", &a_key, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    other.cleanup().await;
    app.cleanup().await;
}

#[tokio::test]
async fn tasks_api_needs_tasks_manage_and_checks_ids() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (owner_id, owner_key) = staff_key(&app, "owner").await;
    let (_, sales_key) = staff_key(&app, "sales").await;
    let (c_user, c_client, _) = owner(&app, "gorev").await;
    let client_key = api_key(&app, c_user).await;

    // Permission / principal checks.
    let (s, _) = api(&app, "GET", "/api/v1/tasks", &client_key, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "client key");
    let (s, _) = anon(&app, "/api/v1/tasks").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // A staff key scoped without `tasks.manage` (scopes ∩ role permissions).
    let (sales_id, _) = staff_key(&app, "sales").await;
    let g = hos_auth::tokens::generate_api_key();
    identity::create_api_key(
        app.pool(),
        app.tenant_id,
        Some(sales_id),
        "scoped",
        &g.lookup,
        &g.key_hash,
        &["clients.view".to_owned()],
        None,
    )
    .await
    .unwrap();
    let scoped = g.key.expose_str().to_owned();
    let (s, _) = api(&app, "GET", "/api/v1/tasks", &scoped, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "key scope lacks tasks.manage");
    let (s, _) = api(
        &app,
        "POST",
        "/api/v1/tasks",
        &scoped,
        Some(json!({"title": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = api(&app, "GET", "/api/v1/tasks", &sales_key, None).await;
    assert_eq!(s, StatusCode::OK, "sales has tasks.manage");

    // Create: validation, foreign ids.
    let (s, v) = api(
        &app,
        "POST",
        "/api/v1/tasks",
        &owner_key,
        Some(json!({"title": " "})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let other = TestApp::new().await.unwrap();
    let (o_user, o_client, _) = owner(&other, "yabanci").await;
    for (field, id) in [
        ("client_id", o_client.to_string()),
        ("assigned_to", o_user.to_string()),
        ("assigned_to", c_user.to_string()),
    ] {
        let (s, v) = api(
            &app,
            "POST",
            "/api/v1/tasks",
            &owner_key,
            Some(json!({"title": "Ara", field: id})),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{field}={id}: {v}");
    }
    let (s, _) = api(
        &app,
        "POST",
        "/api/v1/tasks",
        &owner_key,
        Some(json!({"title": "Ara", "unknown_field": 1})),
    )
    .await;
    assert!(s.is_client_error());

    let (s, v) = api(
        &app,
        "POST",
        "/api/v1/tasks",
        &owner_key,
        Some(json!({
            "title": "Müşteriyi ara",
            "priority": "high",
            "due_date": "2030-01-31",
            "assigned_to": owner_id,
            "client_id": c_client,
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap().to_owned();
    assert_eq!(v["status"], "open");
    assert_eq!(v["priority"], "high");

    // Read, list, update (status done, clear the due date), delete.
    let (s, v) = api(
        &app,
        "GET",
        &format!("/api/v1/tasks/{id}"),
        &owner_key,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["title"], "Müşteriyi ara");
    let (_, v) = api(
        &app,
        "GET",
        &format!("/api/v1/tasks?client_id={c_client}&status=open"),
        &owner_key,
        None,
    )
    .await;
    assert_eq!(v["total"], 1, "{v}");
    let (s, v) = api(
        &app,
        "PATCH",
        &format!("/api/v1/tasks/{id}"),
        &owner_key,
        Some(json!({"status": "done", "due_date": null})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "done");
    assert!(v["completed_at"].is_string());
    assert!(v["due_date"].is_null());
    assert_eq!(v["title"], "Müşteriyi ara", "omitted fields stay");

    // Another tenant's staff cannot see or change it.
    let (o_staff, _) = {
        let email = format!("api-o-{}@staff.test", uuid::Uuid::now_v7().simple());
        let id = user(&other, &email, UserKind::Staff, Some("owner")).await;
        (id, ())
    };
    let o_key = api_key(&other, o_staff).await;
    let (s, _) = api(&other, "GET", &format!("/api/v1/tasks/{id}"), &o_key, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = api(
        &other,
        "DELETE",
        &format!("/api/v1/tasks/{id}"),
        &o_key,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    other.cleanup().await;

    // A session call without the CSRF token is refused.
    staff(&mut app, "owner").await;
    let host = app.host.clone();
    let r = app
        .send_to(
            &host,
            http::Method::DELETE,
            &format!("/api/v1/tasks/{id}"),
            None,
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);

    let (s, v) = api(
        &app,
        "DELETE",
        &format!("/api/v1/tasks/{id}"),
        &owner_key,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = api(
        &app,
        "GET",
        &format!("/api/v1/tasks/{id}"),
        &owner_key,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    app.cleanup().await;
}
