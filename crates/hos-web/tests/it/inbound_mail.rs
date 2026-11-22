//! E-mail piping into tickets (`/webhooks/mail/mailgun-mime`,
//! `/webhooks/mail/raw`): Mailgun signature (HMAC-SHA256 of timestamp +
//! token), replay window and token reuse, both Mailgun body encodings, the
//! raw endpoint's bearer token, reply-token and subject threading, sender
//! checks with quarantine and staff review, automated / loop mail, floods,
//! duplicates, attachments, HTML bodies, size limits and tenant isolation;
//! sender authentication (trusted `Authentication-Results`, Mailgun's
//! `X-Mailgun-Spf`) before a sender counts as a client, reply tokens
//! required for threading, the durable Mailgun replay set and the e-mail
//! badges on the ticket.

use crate::biz_support::{login, owner, post, staff, sub_user};
use crate::common::{Res, TestApp};
use chrono::{Duration, Utc};
use hos_core::domain::TicketPriority;
use hos_core::ids::{ClientId, TicketId, UserId};
use hos_db::enums::{AuthorKind, MessageSource};
use hos_db::repo::mail_inbound::{self as mrepo, InboundSource, InboundStatus, UnknownSenders};
use hos_db::repo::support::{self, NewTicket, NewTicketMessage, TicketRow};
use http::{Method, StatusCode};
use serde_json::{Value, json};

const KEY: &str = "mailgun-signing-key-for-tests";
const RAW_TOKEN: &str = "raw-endpoint-token-0123456789abcdef0123";
const SUPPORT: &str = "destek@inbound.test";
/// The tenant's receiving MTA (trusted `Authentication-Results` writer).
const AUTHSERV: &str = "mx.inbound.test";

/// A passing DMARC result for `from`'s domain from the trusted MTA.
fn authed(from: &str) -> String {
    let domain = from.rsplit('@').next().unwrap();
    format!("Authentication-Results: {AUTHSERV}; dmarc=pass header.from={domain}\r\n")
}

async fn enable(app: &TestApp, policy: UnknownSenders) {
    mrepo::save_settings(app.pool(), app.tenant_id, true, policy, None)
        .await
        .unwrap();
    let sealed = hos_billing::secrets::seal(
        &app.state.secret_box,
        KEY,
        &hos_billing::secrets::mail_inbound_aad(app.tenant_id),
    )
    .unwrap();
    mrepo::set_mailgun_key(app.pool(), app.tenant_id, Some(&sealed))
        .await
        .unwrap();
    mrepo::set_raw_token_hash(
        app.pool(),
        app.tenant_id,
        Some(&hos_core::crypto::hash_token(RAW_TOKEN)),
    )
    .await
    .unwrap();
    mrepo::save_sender_auth(app.pool(), app.tenant_id, Some(AUTHSERV), true)
        .await
        .unwrap();
    app.setting("mail.reply_to", json!(SUPPORT)).await;
}

fn message(from: &str, to: &str, subject: &str, body: &str, extra_headers: &str) -> String {
    format!(
        "From: Gönderen <{from}>\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{}@mail.test>\r\n\
         Date: Fri, 20 Nov 2026 10:00:00 +0300\r\nMIME-Version: 1.0\r\n{extra_headers}\
         Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n",
        uuid::Uuid::now_v7().simple()
    )
}

fn signature(timestamp: &str, token: &str) -> String {
    hos_core::crypto::hmac_sha256_hex(KEY.as_bytes(), format!("{timestamp}{token}").as_bytes())
}

fn form(fields: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in fields {
        s.append_pair(k, v);
    }
    s.finish()
}

async fn send(
    app: &mut TestApp,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let mut b = http::Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", &app.host)
        .header("content-type", content_type);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = app
        .router
        .clone()
        .oneshot(b.body(axum::body::Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A Mailgun `…mime` route post (urlencoded) with a valid signature.
async fn mailgun(app: &mut TestApp, raw: &str, recipient: &str) -> (StatusCode, Value) {
    let ts = Utc::now().timestamp().to_string();
    let token = uuid::Uuid::now_v7().simple().to_string();
    let body = form(&[
        ("timestamp", &ts),
        ("token", &token),
        ("signature", &signature(&ts, &token)),
        ("recipient", recipient),
        ("body-mime", raw),
    ]);
    send(
        app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        body.into_bytes(),
        &[],
    )
    .await
}

/// The raw endpoint with the right bearer token.
async fn raw(app: &mut TestApp, raw: &str, recipient: &str) -> (StatusCode, Value) {
    let path = format!(
        "/webhooks/mail/raw?recipient={}",
        url::form_urlencoded::byte_serialize(recipient.as_bytes()).collect::<String>()
    );
    send(
        app,
        &path,
        "message/rfc822",
        raw.as_bytes().to_vec(),
        &[("authorization", &format!("Bearer {RAW_TOKEN}"))],
    )
    .await
}

async fn ticket_count(app: &TestApp) -> i64 {
    sqlx::query_scalar("select count(*) from tickets where tenant_id = $1")
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .unwrap()
}

async fn ticket(app: &TestApp, number: &str) -> TicketRow {
    support::ticket_by_number(app.pool(), app.tenant_id, number)
        .await
        .unwrap()
        .expect("ticket")
}

async fn open_ticket(app: &TestApp, client: ClientId, user: UserId, email: &str) -> TicketRow {
    let dept = support::list_departments(app.pool(), app.tenant_id, true)
        .await
        .unwrap()[0]
        .id;
    let mut conn = app.pool().acquire().await.unwrap();
    support::open_ticket(
        &mut conn,
        app.tenant_id,
        &NewTicket {
            department_id: dept,
            client_id: Some(client),
            user_id: Some(user),
            guest_name: None,
            guest_email: None,
            subject: "Sunucu yavaş".into(),
            priority: TicketPriority::Medium,
            service_id: None,
            domain_id: None,
            first_message: NewTicketMessage {
                author_kind: AuthorKind::Client,
                user_id: Some(user),
                author_name: "Müşteri".into(),
                author_email: Some(email.into()),
                body: "Merhaba".into(),
                is_internal: false,
                source: MessageSource::Web,
                message_id: None,
                ip: None,
            },
        },
    )
    .await
    .unwrap()
    .0
}

/// A reply token for `ticket` sent to `recipient`; returns the address.
async fn reply_address(app: &TestApp, ticket: TicketId, recipient: &str) -> String {
    let token = hos_web::inbound::new_reply_token();
    mrepo::create_reply_token(
        app.pool(),
        app.tenant_id,
        ticket,
        &hos_core::crypto::hash_token(&token),
        recipient,
        Utc::now() + Duration::days(30),
    )
    .await
    .unwrap();
    hos_web::inbound::reply_address(SUPPORT, &token).unwrap()
}

async fn messages(app: &TestApp, ticket: TicketId) -> Vec<support::TicketMessageRow> {
    support::ticket_messages(app.pool(), app.tenant_id, ticket, true)
        .await
        .unwrap()
}

#[tokio::test]
async fn mailgun_signature_replay_and_encodings() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, _a_client, a_email) = owner(&app, "posta").await;
    let raw_a = message(
        &a_email,
        SUPPORT,
        "Yeni talep",
        "Merhaba, yardım lütfen.",
        "X-Mailgun-Spf: Pass\r\n",
    );

    // ---- not configured: 406 (Mailgun does not retry), raw endpoint 404
    let (s, _) = mailgun(&mut app, &raw_a, SUPPORT).await;
    assert_eq!(s, StatusCode::NOT_ACCEPTABLE);
    let (s, _) = raw(&mut app, &raw_a, SUPPORT).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    enable(&app, UnknownSenders::Quarantine).await;

    // ---- bad signature, stale timestamp, wrong content type
    let ts = Utc::now().timestamp().to_string();
    let token = uuid::Uuid::now_v7().simple().to_string();
    let tampered = form(&[
        ("timestamp", &ts),
        ("token", &token),
        ("signature", &signature(&ts, "another-token-value-xyz")),
        ("body-mime", &raw_a),
    ]);
    let (s, _) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        tampered.into_bytes(),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let old = (Utc::now().timestamp() - 3600).to_string();
    let stale = form(&[
        ("timestamp", &old),
        ("token", &token),
        ("signature", &signature(&old, &token)),
        ("body-mime", &raw_a),
    ]);
    let (s, _) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        stale.into_bytes(),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "outside the replay window");
    let (s, _) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        "application/json",
        b"{}".to_vec(),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_ACCEPTABLE);
    assert_eq!(ticket_count(&app).await, 0);

    // ---- a valid post with Mailgun's SPF pass for the envelope sender of
    //      the From domain opens a ticket for the client (no CSRF needed)
    let ok = form(&[
        ("timestamp", &ts),
        ("token", &token),
        ("signature", &signature(&ts, &token)),
        ("recipient", SUPPORT),
        ("sender", &a_email),
        ("body-mime", &raw_a),
    ]);
    let (s, v) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        ok.clone().into_bytes(),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "ticket_created", "{v}");
    let t = ticket(&app, v["ticket"].as_str().unwrap()).await;
    assert!(t.client_id.is_some());
    assert_eq!(t.subject, "Yeni talep");

    // ---- replays: the same token, then the same Message-ID with a new token
    let (s, v) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        ok.into_bytes(),
        &[],
    )
    .await;
    assert_eq!(
        (s, v["status"].as_str()),
        (StatusCode::OK, Some("duplicate"))
    );
    let (_, v) = mailgun(&mut app, &raw_a, SUPPORT).await;
    assert_eq!(v["status"], "duplicate");
    assert_eq!(ticket_count(&app).await, 1);

    // ---- multipart/form-data posts are accepted too
    let raw_b = message(
        &a_email,
        SUPPORT,
        "İkinci talep",
        "Bir sorum daha var.",
        "X-Mailgun-Spf: Pass\r\n",
    );
    let ts = Utc::now().timestamp().to_string();
    let token = uuid::Uuid::now_v7().simple().to_string();
    let boundary = "mgBoundary9x";
    let mut body = String::new();
    for (k, v) in [
        ("timestamp", ts.as_str()),
        ("token", token.as_str()),
        ("signature", signature(&ts, &token).as_str()),
        ("recipient", SUPPORT),
        ("sender", a_email.as_str()),
        ("body-mime", raw_b.as_str()),
    ] {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    let (s, v) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        &format!("multipart/form-data; boundary={boundary}"),
        body.into_bytes(),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "ticket_created", "{v}");

    // ---- another tenant: not configured there, and its own key differs
    let mut other = TestApp::new().await.unwrap();
    let raw_c = message(&a_email, SUPPORT, "Başka kiracı", "x", "");
    let (s, _) = mailgun(&mut other, &raw_c, SUPPORT).await;
    assert_eq!(s, StatusCode::NOT_ACCEPTABLE);
    mrepo::save_settings(
        other.pool(),
        other.tenant_id,
        true,
        UnknownSenders::Guest,
        None,
    )
    .await
    .unwrap();
    let sealed = hos_billing::secrets::seal(
        &other.state.secret_box,
        "a-different-signing-key",
        &hos_billing::secrets::mail_inbound_aad(other.tenant_id),
    )
    .unwrap();
    mrepo::set_mailgun_key(other.pool(), other.tenant_id, Some(&sealed))
        .await
        .unwrap();
    let (s, _) = mailgun(&mut other, &raw_c, SUPPORT).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = raw(&mut other, &raw_c, SUPPORT).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no raw token there");
    other.cleanup().await;
    app.cleanup().await;
}

#[tokio::test]
async fn raw_endpoint_threading_quarantine_and_filters() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    enable(&app, UnknownSenders::Quarantine).await;
    let (a_user, a_client, a_email) = owner(&app, "sahip").await;
    let (_, _b_client, b_email) = owner(&app, "diger").await;
    let t = open_ticket(&app, a_client, a_user, &a_email).await;

    // ---- bearer token
    let m = message(&a_email, SUPPORT, "x", "y", "");
    for auth in ["", "Bearer wrong-token", "Basic abc"] {
        let headers: Vec<(&str, &str)> = if auth.is_empty() {
            vec![]
        } else {
            vec![("authorization", auth)]
        };
        let (s, _) = send(
            &mut app,
            "/webhooks/mail/raw",
            "message/rfc822",
            m.clone().into_bytes(),
            &headers,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{auth}");
    }

    // ---- reply token of the ticket's client → reply
    let to = reply_address(&app, t.id, &a_email).await;
    let reply = message(
        &a_email,
        &to,
        "Re: Sunucu yavaş",
        "Hâlâ yavaş.\r\n\r\nOn Mon, X wrote:\r\n> eski",
        "",
    );
    let (s, v) = raw(&mut app, &reply, &to).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "reply_added", "{v}");
    let msgs = messages(&app, t.id).await;
    let last = msgs.last().unwrap();
    assert_eq!(last.source, MessageSource::Email);
    assert_eq!(last.user_id, Some(a_user));
    assert!(last.body.contains("Hâlâ yavaş"));
    assert!(!last.body.contains("eski"), "quoted text cut");

    // ---- the same token from another client's address → quarantine
    let spoof = message(
        &b_email,
        &to,
        "Re: Sunucu yavaş",
        "Bu talebi ben yönetiyorum.",
        "",
    );
    let (_, v) = raw(&mut app, &spoof, &to).await;
    assert_eq!(v["status"], "quarantined");
    assert_eq!(v["reason"], "sender_mismatch");
    assert_eq!(
        messages(&app, t.id).await.len(),
        msgs.len(),
        "nothing added"
    );
    let q = mrepo::list(
        app.pool(),
        app.tenant_id,
        Some(InboundStatus::Quarantined),
        10,
    )
    .await
    .unwrap();
    assert_eq!(q.len(), 1);
    let quarantined = q[0].clone();
    assert_eq!(quarantined.ticket_id, Some(t.id));
    assert!(
        quarantined
            .body_text
            .as_deref()
            .unwrap()
            .contains("yönetiyorum")
    );

    // ---- a ticket number in the subject without the reply token is never
    //      appended (numbers are guessable), even from a verified sender
    let before_number = messages(&app, t.id).await.len();
    for from in [&a_email, &b_email] {
        let by_number = message(
            from,
            SUPPORT,
            &format!("Re: [{}] Sunucu yavaş", t.number),
            "Numara ile yanıt.",
            &authed(from),
        );
        let (_, v) = raw(&mut app, &by_number, SUPPORT).await;
        assert_eq!(
            (v["status"].as_str(), v["reason"].as_str()),
            (Some("quarantined"), Some("no_reply_token")),
            "{v}"
        );
    }
    assert_eq!(messages(&app, t.id).await.len(), before_number);

    // ---- unknown sender → quarantine; a sub-user without tickets too
    let unknown = message("yabanci@nowhere.test", SUPPORT, "Teklif", "Merhaba", "");
    let (_, v) = raw(&mut app, &unknown, SUPPORT).await;
    assert_eq!(
        (v["status"].as_str(), v["reason"].as_str()),
        (Some("quarantined"), Some("unknown_sender"))
    );
    let sub = sub_user(&app, a_client, &["invoices"]).await;
    let (_, v) = raw(
        &mut app,
        &message(&sub, SUPPORT, "Alt kullanıcı", "x", &authed(&sub)),
        SUPPORT,
    )
    .await;
    assert_eq!(v["status"], "quarantined", "no tickets area: {v}");

    // ---- automated mail, loops, bounces and no-reply senders are refused
    for (from, headers, reason) in [
        (
            a_email.as_str(),
            "Auto-Submitted: auto-replied\r\n",
            "auto_submitted",
        ),
        (a_email.as_str(), "Precedence: bulk\r\n", "bulk"),
        (
            a_email.as_str(),
            "List-Id: <liste.test>\r\n",
            "mailing_list",
        ),
        ("mailer-daemon@mx.test", "", "bounce"),
        ("noreply@service.test", "", "no_reply_sender"),
        (SUPPORT, "", "loop"),
    ] {
        let before = ticket_count(&app).await;
        let (s, v) = raw(
            &mut app,
            &message(from, SUPPORT, "Otomatik", "x", headers),
            SUPPORT,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["status"], "rejected", "{reason}: {v}");
        assert_eq!(v["reason"], reason);
        assert_eq!(ticket_count(&app).await, before);
    }

    // ---- attachments: accepted by content, the rest listed for staff;
    //      HTML-only bodies become plain text (no markup kept)
    let mixed = format!(
        "From: <{a_email}>\r\nTo: {SUPPORT}\r\nSubject: Ekler\r\nMessage-ID: <ek-{}@mail.test>\r\n{}\
         MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
         --b1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <p>Merhaba <b>ekip</b></p><script>alert(1)</script>\r\n\
         --b1\r\nContent-Type: text/plain; name=\"log.txt\"\r\nContent-Disposition: attachment; filename=\"log.txt\"\r\n\r\n\
         hata satiri\r\n\
         --b1\r\nContent-Type: application/octet-stream; name=\"kurulum.exe\"\r\nContent-Disposition: attachment; filename=\"kurulum.exe\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
         TVqQAAMAAAAEAAAA\r\n\
         --b1--\r\n",
        uuid::Uuid::now_v7().simple(),
        authed(&a_email),
    );
    let (_, v) = raw(&mut app, &mixed, SUPPORT).await;
    assert_eq!(v["status"], "ticket_created", "{v}");
    let with_files = ticket(&app, v["ticket"].as_str().unwrap()).await;
    let files = support::ticket_attachments(app.pool(), app.tenant_id, with_files.id)
        .await
        .unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].filename, "log.txt");
    let all = messages(&app, with_files.id).await;
    assert!(all[0].body.contains("Merhaba"));
    assert!(!all[0].body.contains("<script") && !all[0].body.contains("<b>"));
    assert!(
        all.iter()
            .any(|m| m.is_internal && m.body.contains("kurulum.exe")),
        "refused attachment noted for staff"
    );

    // ---- floods: the 31st message of a sender within an hour is refused
    for i in 0..30 {
        mrepo::record(
            app.pool(),
            app.tenant_id,
            &mrepo::NewInbound {
                source: InboundSource::Raw,
                message_id: Some(format!("flood-{i}-{}", uuid::Uuid::now_v7())),
                sender: Some("seli@flood.test".into()),
                recipient: None,
                subject: None,
                status: InboundStatus::Rejected,
                reason: Some("unknown_sender".into()),
                ticket_id: None,
                body_text: None,
                attachment_names: Vec::new(),
                size_bytes: 1,
                sender_verified: None,
            },
        )
        .await
        .unwrap();
    }
    let (_, v) = raw(
        &mut app,
        &message("seli@flood.test", SUPPORT, "x", "y", ""),
        SUPPORT,
    )
    .await;
    assert_eq!(v["reason"], "flood", "{v}");

    // ---- garbage and oversized bodies
    let (_, v) = raw(&mut app, "this is not an e-mail", SUPPORT).await;
    assert_eq!(v["status"], "rejected");
    let huge = format!(
        "{}{}",
        message(&a_email, SUPPORT, "büyük", "x", ""),
        "a".repeat(3 * 1024 * 1024)
    );
    let (s, _) = raw(&mut app, &huge, SUPPORT).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);

    // ---- staff review: permission, CSRF, append once
    let path = format!("/admin/support/inbound/{}", quarantined.id);
    staff(&mut app, "sales").await;
    let r = app.get(&path).await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "sales has no support.manage"
    );
    let csrf = staff(&mut app, "support").await;
    let page = app.get(&path).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains(&format!("{path}/append")));
    let r = app.post(&format!("{path}/append"), &[]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    let before = messages(&app, t.id).await.len();
    let r = post(&mut app, &format!("{path}/append"), &csrf, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let after = messages(&app, t.id).await;
    assert_eq!(after.len(), before + 1);
    assert_eq!(
        after.last().unwrap().author_email.as_deref(),
        Some(b_email.as_str())
    );
    let r = post(&mut app, &format!("{path}/append"), &csrf, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(messages(&app, t.id).await.len(), before + 1, "applied once");

    // ---- another tenant's staff cannot open or release it
    let mut other = TestApp::new().await.unwrap();
    let other_csrf = staff(&mut other, "owner").await;
    assert_eq!(other.get(&path).await.status, StatusCode::NOT_FOUND);
    let r: Res = post(&mut other, &format!("{path}/append"), &other_csrf, &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    other.cleanup().await;

    // ---- the client sees the e-mail reply in its ticket
    login(&mut app, &a_email).await;
    let page = app.get(&format!("/client/tickets/{}", t.id)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Hâlâ yavaş"));
    app.cleanup().await;
}

/// [`raw`] to the support address; the JSON answer.
async fn raw_from(app: &mut TestApp, message: &str) -> Value {
    raw(app, message, SUPPORT).await.1
}

/// A Mailgun post with a `sender` (envelope `MAIL FROM`) field.
async fn mailgun_with_sender(app: &mut TestApp, raw: &str, sender: &str) -> Value {
    let ts = Utc::now().timestamp().to_string();
    let token = uuid::Uuid::now_v7().simple().to_string();
    let body = form(&[
        ("timestamp", &ts),
        ("token", &token),
        ("signature", &signature(&ts, &token)),
        ("recipient", SUPPORT),
        ("sender", sender),
        ("body-mime", raw),
    ]);
    send(
        app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        body.into_bytes(),
        &[],
    )
    .await
    .1
}

/// Only `From` decided which client wrote a ticket: anyone could open
/// tickets as any client. A client's address now counts only with a
/// passing sender authentication for the From domain.
#[tokio::test]
async fn a_client_address_in_from_is_not_enough() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    enable(&app, UnknownSenders::Quarantine).await;
    let (_, a_client, a_email) = owner(&app, "kimlik").await;
    let domain = a_email.rsplit('@').next().unwrap().to_owned();
    let spoofed = |headers: &str| message(&a_email, SUPPORT, "Hesabımı kapatın", "x", headers);

    // ---- raw endpoint: no, foreign, failing or misaligned results
    for (headers, why) in [
        (String::new(), "no Authentication-Results"),
        (
            format!("Authentication-Results: evil.test; dmarc=pass header.from={domain}\r\n"),
            "untrusted authserv-id",
        ),
        (
            format!("Authentication-Results: {AUTHSERV}; dmarc=fail header.from={domain}\r\n"),
            "dmarc fail",
        ),
        (
            format!("Authentication-Results: {AUTHSERV}; dmarc=pass header.from=attacker.test\r\n"),
            "another From domain",
        ),
        (
            format!(
                "Authentication-Results: {AUTHSERV}; dkim=pass header.d=attacker.test; spf=pass smtp.mailfrom=x@attacker.test\r\n"
            ),
            "unaligned dkim / spf",
        ),
        (
            format!(
                "Authentication-Results: {AUTHSERV}; spf=fail smtp.mailfrom={a_email}\r\n\
                 Authentication-Results: {AUTHSERV}; dmarc=pass header.from={domain}\r\n"
            ),
            "only the topmost trusted header counts",
        ),
    ] {
        let v = raw_from(&mut app, &spoofed(&headers)).await;
        assert_eq!(
            (v["status"].as_str(), v["reason"].as_str()),
            (Some("quarantined"), Some("sender_not_verified")),
            "{why}: {v}"
        );
    }
    assert_eq!(ticket_count(&app).await, 0);
    let logged = mrepo::list(
        app.pool(),
        app.tenant_id,
        Some(InboundStatus::Quarantined),
        10,
    )
    .await
    .unwrap();
    assert!(logged.iter().all(|m| m.sender_verified == Some(false)));

    // ---- a passing result for the From domain (DMARC, aligned DKIM)
    let v = raw_from(&mut app, &spoofed(&authed(&a_email))).await;
    assert_eq!(v["status"], "ticket_created", "{v}");
    let t = ticket(&app, v["ticket"].as_str().unwrap()).await;
    assert_eq!(t.client_id, Some(a_client));
    assert_eq!(messages(&app, t.id).await[0].sender_verified, Some(true));
    let dkim = format!(
        "Authentication-Results: {AUTHSERV} (comment); dkim=pass (2048-bit) header.d=mail.{domain} header.s=s1\r\n"
    );
    let v = raw_from(&mut app, &message(&a_email, SUPPORT, "DKIM", "x", &dkim)).await;
    assert_eq!(v["status"], "ticket_created", "aligned subdomain: {v}");

    // ---- no trusted authserv-id configured: nothing is trusted
    mrepo::save_sender_auth(app.pool(), app.tenant_id, None, true)
        .await
        .unwrap();
    let v = raw_from(&mut app, &spoofed(&authed(&a_email))).await;
    assert_eq!(v["reason"], "sender_not_verified", "{v}");

    // ---- Mailgun: one X-Mailgun-Spf: Pass for an aligned envelope sender
    let pass = "X-Mailgun-Spf: Pass\r\n";
    let v = mailgun_with_sender(&mut app, &spoofed(""), &a_email).await;
    assert_eq!(v["reason"], "sender_not_verified", "no header: {v}");
    let v = mailgun_with_sender(&mut app, &spoofed(&format!("{pass}{pass}")), &a_email).await;
    assert_eq!(
        v["reason"], "sender_not_verified",
        "sender added its own: {v}"
    );
    let v = mailgun_with_sender(&mut app, &spoofed(pass), "bounce@attacker.test").await;
    assert_eq!(
        v["reason"], "sender_not_verified",
        "unaligned envelope: {v}"
    );
    let v = mailgun_with_sender(&mut app, &spoofed("X-Mailgun-Spf: SoftFail\r\n"), &a_email).await;
    assert_eq!(v["reason"], "sender_not_verified", "softfail: {v}");
    let v = mailgun_with_sender(&mut app, &spoofed(pass), &format!("bounces@{domain}")).await;
    assert_eq!(v["status"], "ticket_created", "{v}");
    // Not trusted unless the tenant says the Mailgun domain marks spam.
    mrepo::save_sender_auth(app.pool(), app.tenant_id, None, false)
        .await
        .unwrap();
    let v = mailgun_with_sender(&mut app, &spoofed(pass), &a_email).await;
    assert_eq!(v["reason"], "sender_not_verified", "{v}");

    // ---- guest policy: an unverified client address opens a guest ticket
    mrepo::save_settings(app.pool(), app.tenant_id, true, UnknownSenders::Guest, None)
        .await
        .unwrap();
    let v = raw_from(&mut app, &spoofed("")).await;
    assert_eq!(v["status"], "ticket_created", "{v}");
    let guest = ticket(&app, v["ticket"].as_str().unwrap()).await;
    assert_eq!(guest.client_id, None, "never the client's ticket");
    assert_eq!(guest.guest_email.as_deref(), Some(a_email.as_str()));
    assert_eq!(
        messages(&app, guest.id).await[0].sender_verified,
        Some(false)
    );
    app.cleanup().await;
}

/// Forged Mailgun posts / bad bearer tokens used up the tenant's limit
/// before they were checked; now only authenticated calls count, and a
/// failing client IP is turned away.
#[tokio::test]
async fn forged_inbound_posts_do_not_use_up_the_tenant_limit() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    enable(&app, UnknownSenders::Quarantine).await;
    let key = format!("inbound-mail:{}", app.tenant_id);
    for _ in 0..3 {
        let (s, _) = send(
            &mut app,
            "/webhooks/mail/raw",
            "message/rfc822",
            message("a@b.test", SUPPORT, "x", "y", "").into_bytes(),
            &[("authorization", "Bearer wrong-token")],
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let burst = app.state.limits.inbound_mail.limit().burst;
    let left = match app.state.limits.inbound_mail.check(&key) {
        hos_auth::ratelimit::RateDecision::Allowed { remaining } => remaining,
        hos_auth::ratelimit::RateDecision::Limited { .. } => 0,
    };
    assert_eq!(left, burst - 1, "forged posts took from the tenant limit");
    // The failing address (all oneshot requests come from 127.0.0.1) is
    // limited once its budget is spent; a valid call from it is refused too.
    let budget = app.state.limits.webhook_failed.limit().burst;
    for _ in 3..budget {
        send(
            &mut app,
            "/webhooks/mail/raw",
            "message/rfc822",
            b"x".to_vec(),
            &[("authorization", "Bearer wrong-token")],
        )
        .await;
    }
    let (s, _) = raw(
        &mut app,
        &message("a@b.test", SUPPORT, "x", "y", ""),
        SUPPORT,
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    app.cleanup().await;
}

/// The Mailgun replay set lived in process memory: a restart (or a second
/// process) accepted a captured post again. It is in the database now.
#[tokio::test]
async fn mailgun_tokens_are_honoured_once_across_processes() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    enable(&app, UnknownSenders::Guest).await;
    let ts = Utc::now().timestamp().to_string();
    let token = uuid::Uuid::now_v7().simple().to_string();
    let post = |subject: &str| {
        form(&[
            ("timestamp", &ts),
            ("token", &token),
            ("signature", &signature(&ts, &token)),
            ("recipient", SUPPORT),
            (
                "body-mime",
                &message("biri@nowhere.test", SUPPORT, subject, "x", ""),
            ),
        ])
    };
    let (s, v) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        post("ilk").into_bytes(),
        &[],
    )
    .await;
    assert_eq!(
        (s, v["status"].as_str()),
        (StatusCode::OK, Some("ticket_created")),
        "{v}"
    );
    // A second process (fresh state, same database) sees the token used,
    // even for a different message under the captured signature.
    let state = crate::common::new_state(
        app.pool().clone(),
        app.state.config.clone(),
        app.state.web.clone(),
    );
    app.router = hos_web::app(state);
    let (s, v) = send(
        &mut app,
        "/webhooks/mail/mailgun-mime",
        "application/x-www-form-urlencoded",
        post("ikinci").into_bytes(),
        &[],
    )
    .await;
    assert_eq!(
        (s, v["status"].as_str()),
        (StatusCode::OK, Some("duplicate")),
        "{v}"
    );
    assert_eq!(ticket_count(&app).await, 1);
    let stored: i64 = sqlx::query_scalar(
        "select count(*) from mail_inbound_replay where tenant_id = $1 and expires_at > now()",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(stored, 1);
    app.cleanup().await;
}

/// Tickets did not show that a message came by e-mail, nor that its
/// sender was not verified.
#[tokio::test]
async fn ticket_views_mark_email_and_unverified_senders() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    enable(&app, UnknownSenders::Quarantine).await;
    let (_, a_client, a_email) = owner(&app, "rozet").await;
    let v = raw_from(
        &mut app,
        &message(&a_email, SUPPORT, "Doğrulanmış", "x", &authed(&a_email)),
    )
    .await;
    assert_eq!(v["status"], "ticket_created", "{v}");
    let t = ticket(&app, v["ticket"].as_str().unwrap()).await;
    assert_eq!(t.client_id, Some(a_client));
    // A reply by token from an unverified sender (the token holder).
    let to = reply_address(&app, t.id, &a_email).await;
    let body = "yanıt ![iz](https://tracker.example/pixel.png)";
    let v = raw_from(
        &mut app,
        &message(&a_email, &to, "Re: Doğrulanmış", body, ""),
    )
    .await;
    assert_eq!(v["status"], "reply_added", "{v}");
    let msgs = messages(&app, t.id).await;
    assert_eq!(
        msgs.iter().map(|m| m.sender_verified).collect::<Vec<_>>(),
        [Some(true), Some(false)]
    );
    let locale = hos_core::domain::Locale::Tr;
    let by_email = app.state.i18n.t(locale, "admin_support.ticket.by_email");
    let unverified = app
        .state
        .i18n
        .t(locale, "admin_support.ticket.sender_unverified");
    staff(&mut app, "support").await;
    let page = app.get(&format!("/admin/support/tickets/{}", t.id)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.body.matches(&by_email).count(),
        2,
        "both came by e-mail"
    );
    assert_eq!(page.body.matches(&unverified).count(), 1, "only the reply");
    // A remote image in a message is a link, never loaded by the viewer.
    assert!(!page.body.contains("<img src=\"https://tracker.example"));
    assert!(
        page.body
            .contains("href=\"https://tracker.example/pixel.png\">iz</a>")
    );
    login(&mut app, &a_email).await;
    let page = app.get(&format!("/client/tickets/{}", t.id)).await;
    assert_eq!(page.status, StatusCode::OK);
    let by_email = app.state.i18n.t(locale, "client.tickets.by_email");
    let unverified = app.state.i18n.t(locale, "client.tickets.sender_unverified");
    assert_eq!(page.body.matches(&by_email).count(), 2);
    assert_eq!(page.body.matches(&unverified).count(), 1);
    app.cleanup().await;
}

/// Waits until a query of another session waits for a lock held by `pid`.
async fn wait_blocked_by(pool: &sqlx::PgPool, pid: i32) {
    for _ in 0..600 {
        let blocked: bool = sqlx::query_scalar(
            "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
        )
        .bind(pid)
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the request never waited for the lock");
}

/// A quarantined message from `sender` (for `ticket`: a reply to it).
async fn quarantined(
    app: &TestApp,
    sender: &str,
    ticket: Option<TicketId>,
) -> mrepo::InboundMessageId {
    mrepo::record(
        app.pool(),
        app.tenant_id,
        &mrepo::NewInbound {
            source: InboundSource::Raw,
            message_id: Some(format!("kilit-{}", uuid::Uuid::now_v7())),
            sender: Some(sender.into()),
            recipient: Some(SUPPORT.into()),
            subject: Some("Kilitli ileti".into()),
            status: InboundStatus::Quarantined,
            reason: Some(
                if ticket.is_some() {
                    "sender_mismatch"
                } else {
                    "unknown_sender"
                }
                .into(),
            ),
            ticket_id: ticket,
            body_text: Some("Merhaba, sunucu yanıt vermiyor.".into()),
            attachment_names: Vec::new(),
            size_bytes: 64,
            sender_verified: Some(false),
        },
    )
    .await
    .unwrap()
    .expect("recorded")
}

/// Releasing or appending a quarantined message is one transaction with the
/// ticket write: when that fails (here the department / ticket disappears
/// while the request waits for the message row), the message stays in
/// quarantine and can be released again (it used to be left "released"
/// with no ticket).
#[tokio::test]
async fn a_failed_release_or_append_leaves_the_message_in_quarantine() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    enable(&app, UnknownSenders::Quarantine).await;
    let pool = app.pool().clone();
    let tenant = app.tenant_id;
    let csrf = staff(&mut app, "support").await;
    let status = |id: mrepo::InboundMessageId| {
        let pool = pool.clone();
        async move {
            mrepo::get(&pool, tenant, id)
                .await
                .unwrap()
                .expect("message")
                .status
        }
    };

    // ---- release: the department is deleted while the request waits
    let sender = format!("yabanci-{}@kilit.test", uuid::Uuid::now_v7().simple());
    let m = quarantined(&app, &sender, None).await;
    let dept = support::create_department(
        &pool,
        tenant,
        &format!("kilit-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]),
        "Kilit",
        None,
        &json!({}),
        None,
        99,
    )
    .await
    .unwrap();
    let before = ticket_count(&app).await;
    let mut lock = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("select 1 from mail_inbound_messages where tenant_id = $1 and id = $2 for update")
        .bind(tenant)
        .bind(m)
        .execute(&mut *lock)
        .await
        .unwrap();
    let path = format!("/admin/support/inbound/{m}/release");
    let dept_id = dept.id.to_string();
    let fields = [("department_id", dept_id.as_str())];
    let (r, ()) = tokio::join!(post(&mut app, &path, &csrf, &fields), async {
        wait_blocked_by(&pool, pid).await;
        sqlx::query("delete from ticket_departments where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(dept.id)
            .execute(&mut *lock)
            .await
            .unwrap();
        lock.commit().await.unwrap();
    });
    assert_ne!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(status(m).await, InboundStatus::Quarantined, "not released");
    assert_eq!(ticket_count(&app).await, before, "no ticket");
    // Released again with a department that exists: one ticket.
    let live = support::list_departments(&pool, tenant, true)
        .await
        .unwrap()[0]
        .id
        .to_string();
    let r = post(&mut app, &path, &csrf, &[("department_id", &live)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(status(m).await, InboundStatus::Released);
    assert_eq!(ticket_count(&app).await, before + 1);
    let r = post(&mut app, &path, &csrf, &[("department_id", &live)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(ticket_count(&app).await, before + 1, "released once");

    // ---- append: the ticket is deleted while the request waits
    let (user, client, email) = owner(&app, "kilit").await;
    let t = open_ticket(&app, client, user, &email).await;
    let m = quarantined(&app, &sender, Some(t.id)).await;
    let mut lock = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("select 1 from mail_inbound_messages where tenant_id = $1 and id = $2 for update")
        .bind(tenant)
        .bind(m)
        .execute(&mut *lock)
        .await
        .unwrap();
    let path = format!("/admin/support/inbound/{m}/append");
    let (r, ()) = tokio::join!(post(&mut app, &path, &csrf, &[]), async {
        wait_blocked_by(&pool, pid).await;
        sqlx::query("delete from tickets where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(t.id)
            .execute(&mut *lock)
            .await
            .unwrap();
        lock.commit().await.unwrap();
    });
    assert_ne!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(status(m).await, InboundStatus::Quarantined, "not released");
    app.cleanup().await;
}
