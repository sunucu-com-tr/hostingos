//! Quotes end to end: staff editor (server-side totals, permissions, CSRF),
//! sending with a secret link, the public link (wrong token, other tenant,
//! replay), client area ownership and sub-user areas, conversion into an
//! order + invoice exactly once, prospect quotes, expiry and the PDF.

use crate::biz_support::{api, api_key, csrf_of, login, owner, post, staff, sub_user};
use crate::common::TestApp;
use hos_db::repo::quotes::{self as qrepo, QuoteFilter, QuoteStatus};
use hos_db::repo::{invoices, orders};
use http::StatusCode;
use rust_decimal::Decimal;

async fn quote_count(app: &TestApp) -> i64 {
    qrepo::list_quotes(
        app.pool(),
        app.tenant_id,
        &QuoteFilter::default(),
        "q.created_at desc",
        hos_db::Page::first(50),
    )
    .await
    .expect("list")
    .total
}

async fn order_count(app: &TestApp, client: hos_core::ids::ClientId) -> i64 {
    orders::list_orders(
        app.pool(),
        app.tenant_id,
        &orders::OrderFilter {
            status: None,
            client_id: Some(client),
            search: None,
        },
        hos_db::Page::first(50),
    )
    .await
    .expect("orders")
    .total
}

fn lines<'a>(extra: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut v = vec![
        ("line_product", ""),
        ("line_cycle", ""),
        ("line_description", "Kurulum ve taşıma"),
        ("line_quantity", "2"),
        ("line_price", "100"),
        ("line_discount", ""),
        ("line_taxable", "1"),
        ("line_product", ""),
        ("line_cycle", ""),
        ("line_description", "Danışmanlık"),
        ("line_quantity", "1"),
        ("line_price", "50"),
        ("line_discount", "10"),
        ("line_taxable", "1"),
        // An unused blank row is ignored.
        ("line_product", ""),
        ("line_cycle", ""),
        ("line_description", ""),
        ("line_quantity", "1"),
        ("line_price", ""),
        ("line_discount", ""),
        ("line_taxable", "1"),
    ];
    v.extend_from_slice(extra);
    v
}

#[tokio::test]
async fn quotes_end_to_end_with_negative_cases() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("auth.require_email_verification", serde_json::json!(false))
        .await;
    let (_a_user, a_client, a_email) = owner(&app, "alfa").await;
    let (_b_user, _b_client, b_email) = owner(&app, "beta").await;

    // ---- permissions: support role has neither quotes.view nor quotes.manage
    let csrf = staff(&mut app, "support").await;
    assert_eq!(app.get("/admin/quotes").await.status, StatusCode::FORBIDDEN);
    let r = post(&mut app, "/admin/quotes", &csrf, &[("subject", "x")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // ---- owner creates a quote; totals are computed server-side
    let csrf = staff(&mut app, "owner").await;
    let valid = (chrono::Utc::now().date_naive() + chrono::Days::new(14)).to_string();
    let mut fields = vec![
        ("client_email", a_email.as_str()),
        ("subject", "Kurumsal taşıma"),
        ("currency", "TRY"),
        ("valid_until", valid.as_str()),
        ("notes", "<script>alert(1)</script> **önemli**"),
        // Tampered totals are not form fields and are ignored.
        ("total", "1"),
        ("subtotal", "1"),
    ];
    let l = lines(&[]);
    fields.extend(l.iter().copied());
    // Missing CSRF → 403, nothing stored.
    let r = app.post("/admin/quotes", &fields).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(quote_count(&app).await, 0);
    let r = post(&mut app, "/admin/quotes", &csrf, &fields).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let list = qrepo::list_quotes(
        app.pool(),
        app.tenant_id,
        &QuoteFilter::default(),
        "q.created_at desc",
        hos_db::Page::first(5),
    )
    .await
    .unwrap();
    let q = list.items[0].quote.clone();
    assert_eq!(q.subtotal, Decimal::from(245), "2×100 + 50−10%");
    assert_eq!(q.discount_total, Decimal::from(5));
    assert!(q.total >= q.subtotal);
    assert_eq!(q.status, QuoteStatus::Draft);
    let items = qrepo::quote_items(app.pool(), app.tenant_id, q.id)
        .await
        .unwrap();
    assert_eq!(items.len(), 2, "blank row ignored");

    // Detail page escapes the notes (Markdown sanitiser).
    let page = app.get(&format!("/admin/quotes/{}", q.id)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("<script>alert(1)"));

    // PDF.
    let pdf = app.get(&format!("/admin/quotes/{}/pdf", q.id)).await;
    assert_eq!(pdf.status, StatusCode::OK);
    assert_eq!(pdf.content_type(), "application/pdf");
    assert!(pdf.body.starts_with("%PDF"));
    let cd = pdf
        .headers
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cd.starts_with("attachment; filename=\"") && cd.is_ascii());

    // Drafts are invisible to the client.
    login(&mut app, &a_email).await;
    assert_eq!(
        app.get(&format!("/client/quotes/{}", q.id)).await.status,
        StatusCode::NOT_FOUND
    );

    // ---- send (through the engine, which returns the secret link)
    let (sent, link) = app
        .state
        .billing
        .send_quote(app.tenant_id, q.id)
        .await
        .expect("send");
    assert_eq!(sent.status, QuoteStatus::Sent);
    let path = link
        .split_once("://")
        .map(|(_, r)| &r[r.find('/').unwrap()..])
        .unwrap()
        .to_owned();
    let token = path.split_once("token=").unwrap().1.to_owned();
    assert_eq!(token.len(), 43);
    assert_ne!(
        sent.token_hash.as_deref(),
        Some(token.as_str()),
        "only the hash is stored"
    );
    // The logged e-mail never contains the token.
    let logged: Vec<String> = sqlx::query_scalar(
        "select coalesce(body_html, '') || coalesce(body_text, '') from email_log
          where tenant_id = $1 and template_key = 'quote'",
    )
    .bind(app.tenant_id)
    .fetch_all(app.pool())
    .await
    .unwrap();
    assert_eq!(logged.len(), 1);
    assert!(!logged[0].contains(&token), "token redacted in email_log");

    // ---- public link
    app.cookies.clear();
    let ok = app.get(&path).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert!(ok.body.contains(&sent.number()));
    assert_eq!(
        ok.headers.get("cache-control").unwrap(),
        "private, no-store"
    );
    assert_eq!(ok.headers.get("referrer-policy").unwrap(), "no-referrer");
    let wrong = format!("/quote/{}?token={}", q.id, "A".repeat(43));
    assert_eq!(app.get(&wrong).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.get(&format!("/quote/{}", q.id)).await.status,
        StatusCode::NOT_FOUND
    );

    // Another tenant with the same id and token → 404.
    let Some(mut other) = TestApp::new().await else {
        return;
    };
    assert_eq!(other.get(&path).await.status, StatusCode::NOT_FOUND);
    other.cleanup().await;

    // ---- another client cannot see or accept it
    login(&mut app, &b_email).await;
    assert_eq!(
        app.get(&format!("/client/quotes/{}", q.id)).await.status,
        StatusCode::NOT_FOUND
    );
    let c = csrf_of(&mut app, "/client/quotes").await;
    let r = post(
        &mut app,
        &format!("/client/quotes/{}/accept", q.id),
        &c,
        &[("agree", "1")],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // ---- a sub-user without the `orders` area is refused
    let sub = sub_user(&app, a_client, &["tickets"]).await;
    login(&mut app, &sub).await;
    assert_eq!(
        app.get("/client/quotes").await.status,
        StatusCode::FORBIDDEN
    );

    // ---- the client accepts: order + invoice, exactly once
    login(&mut app, &a_email).await;
    let listp = app.get("/client/quotes").await;
    assert!(listp.body.contains(&sent.number()));
    let c = csrf_of(&mut app, &format!("/client/quotes/{}", q.id)).await;
    let r = app
        .post(
            &format!("/client/quotes/{}/accept", q.id),
            &[("agree", "1")],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    let r = post(
        &mut app,
        &format!("/client/quotes/{}/accept", q.id),
        &c,
        &[],
    )
    .await;
    assert!(r.status.is_redirection());
    assert_eq!(
        qrepo::get_quote(app.pool(), app.tenant_id, q.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        QuoteStatus::Sent,
        "agree required"
    );
    let r = post(
        &mut app,
        &format!("/client/quotes/{}/accept", q.id),
        &c,
        &[("agree", "1")],
    )
    .await;
    assert!(
        r.location().contains("/client/invoices/"),
        "{}",
        r.location()
    );
    let accepted = qrepo::get_quote(app.pool(), app.tenant_id, q.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(accepted.status, QuoteStatus::Accepted);
    assert!(accepted.token_hash.is_none());
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, accepted.invoice_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.client_id, a_client);
    assert_eq!(inv.total, accepted.total, "invoice total = quote total");
    assert_eq!(order_count(&app, a_client).await, 1);
    // Replays: client area and secret link create nothing.
    let r = post(
        &mut app,
        &format!("/client/quotes/{}/accept", q.id),
        &c,
        &[("agree", "1")],
    )
    .await;
    assert!(r.status.is_redirection());
    app.cookies.clear();
    let anon = app.get("/auth/login").await.csrf();
    let r = app
        .post(
            &format!("/quote/{}/accept", q.id),
            &[("csrf_token", &anon), ("token", &token), ("agree", "1")],
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "token died with the acceptance"
    );
    assert_eq!(order_count(&app, a_client).await, 1);

    // ---- prospect quote: link needs an account to accept; decline by link
    let csrf = staff(&mut app, "owner").await;
    let mut fields = vec![
        ("prospect_name", "Ayşe Potansiyel"),
        ("prospect_email", "ayse.prospect@example.com"),
        ("subject", "Teklif"),
        ("currency", "TRY"),
        ("valid_until", valid.as_str()),
    ];
    fields.extend(l.iter().copied());
    let r = post(&mut app, "/admin/quotes", &csrf, &fields).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let p = qrepo::list_quotes(
        app.pool(),
        app.tenant_id,
        &QuoteFilter {
            status: Some(QuoteStatus::Draft),
            ..QuoteFilter::default()
        },
        "q.created_at desc",
        hos_db::Page::first(5),
    )
    .await
    .unwrap()
    .items[0]
        .quote
        .clone();
    assert!(p.client_id.is_none());
    let (_, plink) = app
        .state
        .billing
        .send_quote(app.tenant_id, p.id)
        .await
        .unwrap();
    let ptoken = plink.split_once("token=").unwrap().1.to_owned();
    app.cookies.clear();
    let page = app.get(&format!("/quote/{}?token={ptoken}", p.id)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("/auth/login?next="), "asks to sign in");
    let anon = page.csrf();
    let r = app
        .post(
            &format!("/quote/{}/accept", p.id),
            &[("csrf_token", &anon), ("token", &ptoken), ("agree", "1")],
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "anonymous prospect is sent to sign in"
    );
    assert!(
        r.location().contains("/auth/login?next="),
        "{}",
        r.location()
    );
    let still = qrepo::get_quote(app.pool(), app.tenant_id, p.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        still.status,
        QuoteStatus::Sent,
        "nothing accepted without an account"
    );
    let r = app
        .post(
            &format!("/quote/{}/decline", p.id),
            &[
                ("csrf_token", &anon),
                ("token", &ptoken),
                ("reason", "pahalı"),
            ],
        )
        .await;
    assert!(r.status.is_redirection(), "{}", r.body);
    let declined = qrepo::get_quote(app.pool(), app.tenant_id, p.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(declined.status, QuoteStatus::Declined);
    assert_eq!(declined.decline_reason.as_deref(), Some("pahalı"));

    // ---- expiry automation
    let csrf = staff(&mut app, "owner").await;
    let r = post(&mut app, "/admin/quotes", &csrf, &fields).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let e = qrepo::list_quotes(
        app.pool(),
        app.tenant_id,
        &QuoteFilter {
            status: Some(QuoteStatus::Draft),
            ..QuoteFilter::default()
        },
        "q.created_at desc",
        hos_db::Page::first(5),
    )
    .await
    .unwrap()
    .items[0]
        .quote
        .clone();
    app.state
        .billing
        .send_quote(app.tenant_id, e.id)
        .await
        .unwrap();
    let later = e.valid_until + chrono::Days::new(1);
    assert_eq!(
        app.state
            .billing
            .expire_quotes(app.tenant_id, later)
            .await
            .unwrap(),
        1
    );
    let expired = qrepo::get_quote(app.pool(), app.tenant_id, e.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(expired.status, QuoteStatus::Expired);
    assert!(expired.token_hash.is_none());

    app.cleanup().await;
}

/// Sends a free-line quote to `client`.
async fn sent_quote(app: &TestApp, client: hos_core::ids::ClientId) -> qrepo::QuoteId {
    let q = app
        .state
        .billing
        .save_quote(
            app.tenant_id,
            None,
            &hos_billing::quotes::QuoteDraft {
                client_id: Some(client),
                prospect_name: None,
                prospect_email: None,
                prospect_company: None,
                subject: "API teklifi".into(),
                currency: hos_core::money::Currency::TRY,
                valid_until: chrono::Utc::now().date_naive() + chrono::Days::new(7),
                notes: None,
                admin_notes: Some("iç not".into()),
                lines: vec![hos_billing::quotes::QuoteLine {
                    product_id: None,
                    billing_cycle: None,
                    description: "Kurulum".into(),
                    quantity: Decimal::ONE,
                    unit_price: Decimal::from(300),
                    discount_percent: Decimal::ZERO,
                    taxable: true,
                }],
            },
            None,
        )
        .await
        .expect("quote");
    app.state
        .billing
        .send_quote(app.tenant_id, q.id)
        .await
        .expect("send");
    q.id
}

#[tokio::test]
async fn quotes_api_is_scoped_to_the_client() {
    let Some(app) = TestApp::new().await else {
        return;
    };
    let (a_user, a_client, _) = owner(&app, "apia").await;
    let (b_user, b_client, _) = owner(&app, "apib").await;
    let qa = sent_quote(&app, a_client).await;
    let qb = sent_quote(&app, b_client).await;
    let ka = api_key(&app, a_user).await;
    let kb = api_key(&app, b_user).await;

    let (st, body) = api(&app, "GET", "/api/v1/quotes", &ka, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1);
    assert_eq!(body["data"][0]["id"], qa.to_string());
    assert!(
        body.to_string().find("iç not").is_none(),
        "staff notes never leave"
    );
    let (st, _) = api(&app, "GET", &format!("/api/v1/quotes/{qb}"), &ka, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "other client's quote");
    let (st, body) = api(&app, "GET", &format!("/api/v1/quotes/{qa}"), &ka, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"][0]["unit_price"], "300.00");

    // A sub-user without the orders area is refused.
    let sub_email = sub_user(&app, a_client, &["tickets"]).await;
    let sub = hos_db::repo::identity::user_by_email(app.pool(), app.tenant_id, &sub_email)
        .await
        .unwrap()
        .unwrap();
    let ks = api_key(&app, sub.id).await;
    let (st, _) = api(&app, "GET", "/api/v1/quotes", &ks, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = api(
        &app,
        "POST",
        &format!("/api/v1/quotes/{qa}/accept"),
        &ks,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // B cannot accept A's quote; A accepts once.
    let (st, _) = api(
        &app,
        "POST",
        &format!("/api/v1/quotes/{qa}/accept"),
        &kb,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, body) = api(
        &app,
        "POST",
        &format!("/api/v1/quotes/{qa}/accept"),
        &ka,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert!(body["invoice_id"].is_string());
    let (st, _) = api(
        &app,
        "POST",
        &format!("/api/v1/quotes/{qa}/accept"),
        &ka,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "accepted once");
    assert_eq!(order_count(&app, a_client).await, 1);
    let (st, body) = api(
        &app,
        "POST",
        &format!("/api/v1/quotes/{qb}/decline"),
        &kb,
        Some(serde_json::json!({"reason": "bütçe"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "declined");
    app.cleanup().await;
}
