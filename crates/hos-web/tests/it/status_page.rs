//! Network status: staff management (permission, CSRF, other tenant), the
//! public page / incident page / Atom feed (private entries hidden, Markdown
//! sanitised), affected-service notices in the client area and the one-off
//! client notification (one e-mail per address).

use crate::biz_support::{login, owner, paid_order, post, product, staff};
use crate::common::TestApp;
use hos_billing::registry::SaveIntegration;
use hos_core::domain::IntegrationCategory;
use hos_db::repo::status::{self as srepo, IncidentStatus};
use http::StatusCode;
use serde_json::json;

async fn incident_count(app: &TestApp) -> usize {
    srepo::list_incidents(app.pool(), app.tenant_id, false, false, 100)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn network_status_end_to_end() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("auth.require_email_verification", json!(false))
        .await;

    // A client with a live service on a server integration, and one without.
    let server = app
        .state
        .billing
        .registry()
        .save_integration(
            app.tenant_id,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Panel,
                kind: hos_panels::MOCK.into(),
                name: "Sunucu 1".into(),
                config: json!({}),
                secrets: Some(json!({})),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("integration")
        .id;
    let product = product(&app).await;
    let (_, hit_client, hit_email) = owner(&app, "etkilenen").await;
    let (_, _, calm_email) = owner(&app, "sakin").await;
    // A second account of the same person: same address, other case.
    let twin = hos_db::repo::clients::create_client(
        app.pool(),
        app.tenant_id,
        &hos_db::repo::clients::ClientInput::individual(
            "İkiz",
            "Hesap",
            &hit_email.to_uppercase(),
            hos_core::money::Currency::TRY,
        ),
    )
    .await
    .unwrap()
    .id;
    for client in [hit_client, twin] {
        paid_order(&app, client, product).await;
        sqlx::query("update services set status = 'active', integration_id = $3 where tenant_id = $1 and client_id = $2")
            .bind(app.tenant_id)
            .bind(client)
            .bind(server)
            .execute(app.pool())
            .await
            .unwrap();
    }

    // ---- permissions: sales has no status.manage; support has
    staff(&mut app, "sales").await;
    assert_eq!(app.get("/admin/status").await.status, StatusCode::FORBIDDEN);
    let csrf = staff(&mut app, "support").await;
    assert_eq!(app.get("/admin/status").await.status, StatusCode::OK);

    // ---- components (one private)
    let comp = [
        ("name", "Web hosting İstanbul"),
        ("group_name", "Hosting"),
        ("status", "operational"),
        ("integration_id", ""),
        ("sort_order", "1"),
        ("is_public", "1"),
    ];
    let r = app.post("/admin/status/components", &comp).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    let server_s = server.to_string();
    let mut bound = comp.to_vec();
    bound[3] = ("integration_id", server_s.as_str());
    let r = post(&mut app, "/admin/status/components", &csrf, &bound).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let r = post(
        &mut app,
        "/admin/status/components",
        &csrf,
        &[
            ("name", "Gizli yedek"),
            ("status", "degraded"),
            ("integration_id", ""),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let comps = srepo::list_components(app.pool(), app.tenant_id, false)
        .await
        .unwrap();
    assert_eq!(comps.len(), 2);
    let public = comps
        .iter()
        .find(|c| c.is_public)
        .expect("public component");
    assert_eq!(public.integration_id, Some(server));

    // ---- an incident on the bound component
    let public_id = public.id.to_string();
    let r = post(
        &mut app,
        "/admin/status/incidents",
        &csrf,
        &[
            ("kind", "incident"),
            ("title", "Disk arızası"),
            ("impact", "major"),
            ("status", "investigating"),
            ("is_public", "1"),
            ("components", public_id.as_str()),
            (
                "message",
                "**Ağ** ekibi inceliyor <script>alert(1)</script>",
            ),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let incident = srepo::list_incidents(app.pool(), app.tenant_id, false, false, 5)
        .await
        .unwrap()[0]
        .clone();
    // A maintenance window needs a start; a private incident stays private.
    let r = post(
        &mut app,
        "/admin/status/incidents",
        &csrf,
        &[
            ("kind", "maintenance"),
            ("title", "Bakım"),
            ("impact", "minor"),
            ("status", "scheduled"),
            ("message", "x"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = post(
        &mut app,
        "/admin/status/incidents",
        &csrf,
        &[
            ("kind", "incident"),
            ("title", "İç olay"),
            ("impact", "minor"),
            ("status", "investigating"),
            ("message", "gizli"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(incident_count(&app).await, 2);
    let private = srepo::list_incidents(app.pool(), app.tenant_id, false, false, 5)
        .await
        .unwrap()
        .into_iter()
        .find(|i| !i.is_public)
        .expect("private incident");

    // ---- public page, incident page, feed
    app.cookies.clear();
    let page = app.get("/status").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Disk arızası"));
    assert!(page.body.contains("Web hosting İstanbul"));
    assert!(
        !page.body.contains("Gizli yedek"),
        "private component hidden"
    );
    assert!(!page.body.contains("İç olay"), "private incident hidden");
    assert!(
        !page.body.contains("<script>alert(1)"),
        "Markdown sanitised"
    );
    assert!(page.body.contains("<strong>Ağ</strong>"));
    assert!(page.body.contains("data-level=\"major\""));
    assert!(!page.json_ld().is_empty(), "JSON-LD breadcrumbs");
    assert_eq!(
        app.get(&format!("/status/incidents/{}", incident.id))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.get(&format!("/status/incidents/{}", private.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get("/status/incidents/not-a-uuid").await.status,
        StatusCode::NOT_FOUND
    );
    let feed = app.get("/status/feed.xml").await;
    assert!(feed.content_type().starts_with("application/atom+xml"));
    assert!(feed.body.contains("Disk arızası") && !feed.body.contains("İç olay"));

    // ---- client notices: only the affected client sees it
    login(&mut app, &hit_email).await;
    assert!(app.get("/client").await.body.contains("Disk arızası"));
    login(&mut app, &calm_email).await;
    assert!(!app.get("/client").await.body.contains("Disk arızası"));

    // ---- notification goes out once; another tenant cannot reach it
    let csrf = staff(&mut app, "support").await;
    let path = format!("/admin/status/incidents/{}/notify", incident.id);
    assert_eq!(
        app.post(&path, &[]).await.status,
        StatusCode::FORBIDDEN,
        "missing CSRF"
    );
    assert_eq!(
        post(&mut app, &path, &csrf, &[]).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        post(&mut app, &path, &csrf, &[]).await.status,
        StatusCode::SEE_OTHER
    );
    let sent: i64 = sqlx::query_scalar(
        "select count(*) from notifications where tenant_id = $1 and kind = 'network.notice'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(sent, 1, "one affected user, notified once");
    let mails: i64 = sqlx::query_scalar(
        "select count(*) from email_log where tenant_id = $1 and template_key = 'network_notice'
            and lower(to_address) = lower($2)",
    )
    .bind(app.tenant_id)
    .bind(&hit_email)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(mails, 1, "one e-mail per address, whatever its case");
    let Some(mut other) = TestApp::new().await else {
        return;
    };
    let other_csrf = staff(&mut other, "owner").await;
    assert_eq!(
        other
            .get(&format!("/admin/status/incidents/{}", incident.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let r = post(
        &mut other,
        &format!("/admin/status/incidents/{}/delete", incident.id),
        &other_csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    other.cleanup().await;

    // ---- resolving closes it: the notice disappears, history shows it
    let r = post(
        &mut app,
        &format!("/admin/status/incidents/{}/updates", incident.id),
        &csrf,
        &[("status", "resolved"), ("message", "Disk değiştirildi.")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let row = srepo::get_incident(app.pool(), app.tenant_id, incident.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, IncidentStatus::Resolved);
    assert!(row.resolved_at.is_some());
    // A maintenance status does not fit an incident.
    let r = post(
        &mut app,
        &format!("/admin/status/incidents/{}/updates", incident.id),
        &csrf,
        &[("status", "completed"), ("message", "x")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(
        srepo::incident_updates(app.pool(), app.tenant_id, incident.id)
            .await
            .unwrap()
            .len(),
        2
    );
    login(&mut app, &hit_email).await;
    assert!(!app.get("/client").await.body.contains("Disk arızası"));
    app.cookies.clear();
    let page = app.get("/status").await;
    assert!(
        page.body.contains("data-level=\"ok\""),
        "private entries do not count"
    );
    app.cleanup().await;
}
