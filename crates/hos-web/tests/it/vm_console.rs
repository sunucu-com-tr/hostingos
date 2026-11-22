//! The VM console relay over a real TCP listener: a client's one-time token,
//! the websocket upgrade (Origin check, token single use and expiry, token
//! bound to its user, per-user session cap), the browser-side RFB handshake
//! ("None" security, the VNC password stays server-side) relayed to the
//! mock provider's simulated VNC server, and the upstream closing. Also the
//! member-area gate (sub-user without `services`), suspended services, and
//! running consoles ending on logout, lost access or suspension.

use std::net::SocketAddr;
use std::time::Duration;

use crate::biz_support::{login, owner, sub_user};
use crate::common::TestApp;
use futures::{SinkExt as _, StreamExt as _};
use hos_core::domain::{IntegrationCategory, ProductType};
use hos_core::ids::{IntegrationId, ServiceId};
use hos_core::integrations::virt::VmStatus;
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::catalog::{self, ProductInput};
use hos_db::repo::integrations;
use hos_db::repo::services::{self, NewService, VmInput};
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

/// An active VPS of `client` on a mock hypervisor that adopts unknown VMs.
async fn vps(app: &TestApp, client: hos_core::ids::ClientId) -> ServiceId {
    let id = IntegrationId::new();
    integrations::create_integration(
        app.pool(),
        app.tenant_id,
        &integrations::NewIntegration {
            id,
            category: IntegrationCategory::Virt,
            kind: hos_virt::MOCK.to_owned(),
            name: format!("Mock virt {}", uuid::Uuid::now_v7().simple()),
            config: json!({"adopt_unknown_vms": true}),
            secrets_enc: None,
            is_active: true,
            sort_order: 0,
        },
    )
    .await
    .expect("integration");
    let mut input = ProductInput::new(
        &format!("vps-{}", uuid::Uuid::now_v7().simple()),
        "VPS",
        ProductType::Vps,
        None,
    );
    input.integration_id = Some(id);
    let product = catalog::create_product(app.pool(), app.tenant_id, &input)
        .await
        .expect("product");
    let today = chrono::Utc::now().date_naive();
    let svc = services::create_service(
        app.pool(),
        app.tenant_id,
        &NewService {
            client_id: client,
            product_id: product.id,
            order_id: None,
            billing_cycle: BillingCycle::Monthly,
            first_payment_amount: Decimal::ZERO,
            recurring_amount: Decimal::ZERO,
            currency: Currency::TRY,
            quantity: 1,
            registration_date: today,
            next_due_date: Some(today),
            next_invoice_date: Some(today),
            integration_id: Some(id),
            domain: Some("console-test.example.com".into()),
            username: None,
            config_selections: json!({}),
            custom_fields: json!({}),
        },
    )
    .await
    .expect("service");
    sqlx::query("update services set status = 'active' where tenant_id = $1 and id = $2")
        .bind(app.tenant_id)
        .bind(svc.id)
        .execute(app.pool())
        .await
        .expect("activate");
    services::upsert_vm(
        app.pool(),
        app.tenant_id,
        &VmInput {
            service_id: Some(svc.id),
            integration_id: id,
            node: None,
            external_id: format!("vm-{}", uuid::Uuid::now_v7().simple()),
            name: "console-test".into(),
            hostname: Some("console-test.example.com".into()),
            os_template: None,
            cpu_cores: Some(1),
            memory_mb: Some(1024),
            disk_gb: Some(20),
            status: VmStatus::Running,
            primary_ipv4: None,
            primary_ipv6: None,
            metadata: json!({}),
        },
    )
    .await
    .expect("vm");
    svc.id
}

/// Serves the app on 127.0.0.1 (websockets need a real connection).
async fn serve(app: &TestApp) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let router = app.router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    addr
}

/// A console token for `service` with the current session (`Ok(ws path,
/// token)`, or the status and body of a refusal).
async fn token(
    app: &mut TestApp,
    service: ServiceId,
) -> Result<(String, String), (StatusCode, String)> {
    let page = app
        .get(&format!("/client/services/{service}/server/console/view"))
        .await;
    if page.status != StatusCode::OK {
        return Err((page.status, page.body));
    }
    let csrf = page.csrf();
    let r = app
        .post(
            &format!("/client/services/{service}/server/console/token"),
            &[("csrf_token", &csrf)],
        )
        .await;
    if r.status != StatusCode::OK {
        return Err((r.status, r.body));
    }
    let v: serde_json::Value = serde_json::from_str(&r.body).expect("json");
    assert!(v.get("password").is_none() && v.get("ticket").is_none());
    Ok((
        v["ws"].as_str().expect("ws").to_owned(),
        v["token"].as_str().expect("token").to_owned(),
    ))
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// Opens the websocket as the browser would (`origin` overridable).
async fn open(
    app: &TestApp,
    addr: SocketAddr,
    path: &str,
    token: &str,
    origin: Option<&str>,
) -> Result<Ws, StatusCode> {
    let scheme = if app.state.web.secure {
        "https"
    } else {
        "http"
    };
    let mut req = format!("ws://{addr}{path}?token={token}")
        .into_client_request()
        .expect("request");
    let h = req.headers_mut();
    h.insert("host", app.host.parse().unwrap());
    let origin = origin.map_or_else(|| format!("{scheme}://{}", app.host), str::to_owned);
    h.insert("origin", origin.parse().unwrap());
    let cookie = app
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    h.insert("cookie", cookie.parse().unwrap());
    let stream = tokio::net::TcpStream::connect(addr).await.expect("tcp");
    match tokio_tungstenite::client_async(req, stream).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(res)) => Err(res.status()),
        Err(e) => panic!("websocket: {e}"),
    }
}

/// Collects at least `n` bytes from the relay.
async fn read(ws: &mut Ws, buf: &mut Vec<u8>, n: usize) -> Vec<u8> {
    while buf.len() < n {
        match tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("relay timed out")
        {
            Some(Ok(Message::Binary(b))) => buf.extend_from_slice(&b),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    buf.drain(..n).collect()
}

/// Browser side of RFB 3.8 against the relay up to `ServerInit`; returns
/// the desktop name.
async fn rfb(ws: &mut Ws) -> String {
    let mut buf = Vec::new();
    assert_eq!(read(ws, &mut buf, 12).await, b"RFB 003.008\n");
    ws.send(Message::Binary(b"RFB 003.008\n".to_vec().into()))
        .await
        .unwrap();
    // The relay offers exactly "None": no VNC password reaches the browser.
    assert_eq!(read(ws, &mut buf, 2).await, [1, 1]);
    ws.send(Message::Binary(vec![1].into())).await.unwrap();
    assert_eq!(read(ws, &mut buf, 4).await, [0, 0, 0, 0]);
    ws.send(Message::Binary(vec![1].into())).await.unwrap(); // ClientInit
    let init = read(ws, &mut buf, 24).await;
    let len = u32::from_be_bytes(init[20..24].try_into().unwrap()) as usize;
    String::from_utf8(read(ws, &mut buf, len).await).unwrap()
}

async fn close_reason(app: &TestApp, service: ServiceId) -> Vec<Option<String>> {
    sqlx::query_scalar::<_, Option<String>>(
        "select close_reason from vm_console_sessions where tenant_id = $1 and service_id = $2
          order by issued_at",
    )
    .bind(app.tenant_id)
    .bind(service)
    .fetch_all(app.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn relay_connects_once_and_reports_upstream_close() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, client, email) = owner(&app, "console").await;
    let svc = vps(&app, client).await;
    login(&mut app, &email).await;
    let addr = serve(&app).await;

    let (path, tok) = token(&mut app, svc).await.expect("token");
    let mut ws = open(&app, addr, &path, &tok, None).await.expect("upgrade");
    assert_eq!(rfb(&mut ws).await, "HostingOS demo");

    // Single use: the same token is refused while (and after) it is used.
    assert_eq!(
        open(&app, addr, &path, &tok, None).await.err(),
        Some(StatusCode::FORBIDDEN)
    );

    // An invalid RFB message makes the simulated VNC server hang up; the
    // relay closes the browser side and records why.
    ws.send(Message::Binary(vec![0xFF, 0, 0, 0].into()))
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => break frame.map(|f| u16::from(f.code)),
                Some(Ok(_)) => {}
                _ => break None,
            }
        }
    })
    .await
    .expect("close");
    assert_eq!(closed, Some(4001));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reasons = close_reason(&app, svc).await;
    assert_eq!(
        reasons.first().cloned().flatten().as_deref(),
        Some("upstream_closed")
    );
    let traffic: (i64, i64) = sqlx::query_as(
        "select bytes_in, bytes_out from vm_console_sessions where tenant_id = $1 and service_id = $2
          order by issued_at limit 1",
    )
    .bind(app.tenant_id)
    .bind(svc)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert!(traffic.0 > 0 && traffic.1 > 0, "{traffic:?}");
    app.cleanup().await;
}

#[tokio::test]
async fn relay_refuses_foreign_origin_expired_and_stolen_tokens() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, client, email) = owner(&app, "console").await;
    let svc = vps(&app, client).await;
    login(&mut app, &email).await;
    let addr = serve(&app).await;

    // Cross-site page: refused before the token is even looked at.
    let (path, tok) = token(&mut app, svc).await.expect("token");
    assert_eq!(
        open(&app, addr, &path, &tok, Some("https://evil.example"))
            .await
            .err(),
        Some(StatusCode::FORBIDDEN)
    );
    // …so the token still works from the page's own origin.
    let mut ws = open(&app, addr, &path, &tok, None)
        .await
        .expect("same origin");
    assert_eq!(rfb(&mut ws).await, "HostingOS demo");
    drop(ws);

    // Expired.
    let (path, tok) = token(&mut app, svc).await.expect("token");
    sqlx::query(
        "update vm_console_sessions set issued_at = now() - interval '10 minutes',
                expires_at = now() - interval '1 second'
          where tenant_id = $1 and connected_at is null",
    )
    .bind(app.tenant_id)
    .execute(app.pool())
    .await
    .unwrap();
    assert_eq!(
        open(&app, addr, &path, &tok, None).await.err(),
        Some(StatusCode::FORBIDDEN)
    );

    // Another user of the same account cannot use the owner's token (and
    // the attempt burns it).
    let (path, tok) = token(&mut app, svc).await.expect("token");
    let member = sub_user(&app, client, &["services"]).await;
    login(&mut app, &member).await;
    assert_eq!(
        open(&app, addr, &path, &tok, None).await.err(),
        Some(StatusCode::FORBIDDEN)
    );
    login(&mut app, &email).await;
    assert_eq!(
        open(&app, addr, &path, &tok, None).await.err(),
        Some(StatusCode::FORBIDDEN)
    );
    assert!(
        close_reason(&app, svc)
            .await
            .iter()
            .any(|r| r.as_deref() == Some("wrong_user"))
    );
    app.cleanup().await;
}

#[tokio::test]
async fn relay_caps_sessions_and_gates_members_and_suspended_services() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, client, email) = owner(&app, "console").await;
    let svc = vps(&app, client).await;
    login(&mut app, &email).await;
    let addr = serve(&app).await;

    let cap = hos_billing::vm_ops::CONSOLE_SESSION_CAP as usize;
    let mut open_sessions = Vec::new();
    for _ in 0..cap {
        let (path, tok) = token(&mut app, svc).await.expect("token");
        let mut ws = open(&app, addr, &path, &tok, None).await.expect("upgrade");
        assert_eq!(rfb(&mut ws).await, "HostingOS demo");
        open_sessions.push(ws);
    }
    let (path, tok) = token(&mut app, svc).await.expect("token");
    assert_eq!(
        open(&app, addr, &path, &tok, None).await.err(),
        Some(StatusCode::CONFLICT)
    );
    assert!(
        close_reason(&app, svc)
            .await
            .iter()
            .any(|r| r.as_deref() == Some("session_cap"))
    );
    for mut ws in open_sessions {
        let _ = ws.close(None).await;
    }

    // A sub-user without the `services` area gets no console at all.
    let member = sub_user(&app, client, &["invoices"]).await;
    login(&mut app, &member).await;
    let refused = token(&mut app, svc).await.expect_err("no services area");
    assert!(
        refused.0 == StatusCode::FORBIDDEN || refused.0.is_redirection(),
        "{refused:?}"
    );

    // Suspended: no token for the client.
    login(&mut app, &email).await;
    sqlx::query("update services set status = 'suspended' where tenant_id = $1 and id = $2")
        .bind(app.tenant_id)
        .bind(svc)
        .execute(app.pool())
        .await
        .unwrap();
    let page = app
        .get(&format!("/client/services/{svc}/server/console/view"))
        .await;
    let csrf = page.csrf();
    let r = app
        .post(
            &format!("/client/services/{svc}/server/console/token"),
            &[("csrf_token", &csrf)],
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert!(!r.body.contains("\"token\""));
    app.cleanup().await;
}

/// Waits for the relay to close `ws`; returns the close code.
async fn closed_with(ws: &mut Ws, within: Duration) -> Option<u16> {
    tokio::time::timeout(within, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => break frame.map(|f| u16::from(f.code)),
                Some(Ok(_)) => {}
                _ => break None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// A running console ends when its holder may no longer use it: logged out,
/// sub-user lost the `services` area, service suspended. A session that is
/// still allowed keeps running past the re-check.
#[tokio::test]
async fn running_consoles_end_when_access_ends() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, client, email) = owner(&app, "console").await;
    let svc = vps(&app, client).await;
    let other_svc = vps(&app, client).await;
    let addr = serve(&app).await;

    // Owner, first browser: logs out later.
    login(&mut app, &email).await;
    let first_browser = app.cookies.clone();
    let (path, tok) = token(&mut app, svc).await.expect("token");
    let mut logged_out = open(&app, addr, &path, &tok, None).await.expect("upgrade");
    assert_eq!(rfb(&mut logged_out).await, "HostingOS demo");

    // Sub-user with `services`: loses it later.
    let member = sub_user(&app, client, &["services"]).await;
    login(&mut app, &member).await;
    let (path, tok) = token(&mut app, svc).await.expect("member token");
    let mut demoted = open(&app, addr, &path, &tok, None).await.expect("upgrade");
    assert_eq!(rfb(&mut demoted).await, "HostingOS demo");

    // Owner, second browser: one console on the service that gets
    // suspended, one that stays allowed.
    login(&mut app, &email).await;
    let (path, tok) = token(&mut app, other_svc).await.expect("token");
    let mut suspended = open(&app, addr, &path, &tok, None).await.expect("upgrade");
    assert_eq!(rfb(&mut suspended).await, "HostingOS demo");
    let (path, tok) = token(&mut app, svc).await.expect("token");
    let mut kept = open(&app, addr, &path, &tok, None).await.expect("upgrade");
    assert_eq!(rfb(&mut kept).await, "HostingOS demo");
    let kept_since = std::time::Instant::now();

    app.cookies = first_browser;
    let csrf = app.get("/client").await.csrf();
    let out = app.post("/auth/logout", &[("csrf_token", &csrf)]).await;
    assert!(out.status.is_redirection(), "logout: {}", out.status);
    sqlx::query(
        "update client_users set permissions = '{invoices}'
          where tenant_id = $1 and client_id = $2
            and user_id = (select id from users where tenant_id = $1 and email = $3)",
    )
    .bind(app.tenant_id)
    .bind(client)
    .bind(&member)
    .execute(app.pool())
    .await
    .unwrap();
    sqlx::query("update services set status = 'suspended' where tenant_id = $1 and id = $2")
        .bind(app.tenant_id)
        .bind(other_svc)
        .execute(app.pool())
        .await
        .unwrap();

    let within = hos_web::console_relay::RECHECK_INTERVAL + Duration::from_secs(10);
    let (a, b, c) = tokio::join!(
        closed_with(&mut logged_out, within),
        closed_with(&mut demoted, within),
        closed_with(&mut suspended, within),
    );
    assert_eq!((a, b, c), (Some(4005), Some(4005), Some(4005)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut reasons: Vec<String> = close_reason(&app, svc)
        .await
        .into_iter()
        .flatten()
        .collect();
    reasons.sort();
    assert_eq!(reasons, ["access_revoked", "signed_out"]);
    assert_eq!(
        close_reason(&app, other_svc).await,
        [Some("service_unavailable".to_owned())]
    );

    // The still-allowed console outlives its own re-check.
    let past_recheck = (hos_web::console_relay::RECHECK_INTERVAL + Duration::from_secs(2))
        .saturating_sub(kept_since.elapsed());
    assert_eq!(closed_with(&mut kept, past_recheck).await, None);
    let open_sessions: i64 = sqlx::query_scalar(
        "select count(*) from vm_console_sessions
          where tenant_id = $1 and service_id = $2 and ended_at is null and connected_at is not null",
    )
    .bind(app.tenant_id)
    .bind(svc)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(open_sessions, 1);
    let _ = kept.close(None).await;
    app.cleanup().await;
}
