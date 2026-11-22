//! e-Fatura / e-Arşiv and invoice PDFs in the web layer: permissions and
//! ownership of every download (admin, client area, REST API, other
//! tenants), the e-invoice settings form, and the VKN / TCKN rules of the
//! client forms. Uses the opt-in `mock` e-invoice provider.
//!
//! Like the other web tests they need the database from `.env`; without it
//! they print a notice and pass.

use crate::common::{Res, TestApp};
use chrono::{Days, Utc};
use hos_billing::einvoice::EInvoiceAction;
use hos_billing::payments::ManualPayment;
use hos_billing::registry::SaveIntegration;
use hos_core::domain::{IntegrationCategory, Locale, UserKind};
use hos_core::ids::{InvoiceId, UserId};
use hos_core::money::Currency;
use hos_db::repo::clients::{self, ClientInput, ClientRow};
use hos_db::repo::einvoice::{self as edocs, EInvoiceDocumentRow};
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::invoices::{self, NewInvoice, NewInvoiceItem, TaxLineInput};
use http::{Method, StatusCode};
use rust_decimal::Decimal;
use serde_json::json;
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-Battery-9";

async fn user(app: &TestApp, email: &str, kind: UserKind, role: Option<&str>) -> UserId {
    let u = identity::create_user(
        app.pool(),
        app.tenant_id,
        &NewUser {
            email: email.into(),
            password_hash: Some(hos_auth::password::hash_password(PASSWORD).unwrap()),
            kind,
            first_name: "Test".into(),
            last_name: "Kullanıcı".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    if let Some(slug) = role {
        let r = identity::role_by_slug(app.pool(), app.tenant_id, slug)
            .await
            .unwrap()
            .expect("role");
        identity::assign_role(app.pool(), app.tenant_id, u.id, r.id)
            .await
            .unwrap();
    }
    u.id
}

async fn login(app: &mut TestApp, email: &str) {
    app.cookies.clear();
    let page = app.get("/auth/login").await;
    let token = page.csrf();
    let r = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", &token),
                ("email", email),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "login: {}", r.body);
}

/// Logs in a fresh staff member with `role`; returns the CSRF token.
async fn staff(app: &mut TestApp, role: &str) -> String {
    let email = format!("{role}-{}@staff.test", uuid::Uuid::now_v7().simple());
    user(app, &email, UserKind::Staff, Some(role)).await;
    login(app, &email).await;
    let dash = app.get("/admin").await;
    assert_eq!(dash.status, StatusCode::OK, "{}", dash.body);
    dash.csrf()
}

async fn post(app: &mut TestApp, path: &str, csrf: &str, fields: &[(&str, &str)]) -> Res {
    let mut all = vec![("csrf_token", csrf)];
    all.extend_from_slice(fields);
    app.post(path, &all).await
}

/// A client account with a login user (individual, İstanbul).
async fn client_user(app: &TestApp, first: &str) -> (ClientRow, UserId, String) {
    let email = format!("{}@client.test", uuid::Uuid::now_v7().simple());
    let uid = user(app, &email, UserKind::Client, None).await;
    let mut input = ClientInput::individual(first, "Yılmaz", &email, Currency::TRY);
    input.city = Some("İstanbul".into());
    input.address_line1 = Some("İstiklal Cd. 1".into());
    let mut conn = app.pool().acquire().await.unwrap();
    let c = clients::create_client_with_owner(&mut conn, app.tenant_id, &input, uid)
        .await
        .expect("client");
    (c, uid, email)
}

/// Company data, settings and a mock e-invoice integration.
async fn enable(app: &TestApp) {
    sqlx::query(
        "update tenants set legal_name = 'Test Barındırma A.Ş.', tax_number = '1234567890',
                tax_office = 'Çankaya',
                address = '{\"line1\": \"Atatürk Bulvarı 1\", \"city\": \"Ankara\"}'::jsonb
          where id = $1",
    )
    .bind(app.tenant_id)
    .execute(app.pool())
    .await
    .unwrap();
    let integration = app
        .state
        .billing
        .registry()
        .save_integration(
            app.tenant_id,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Einvoice,
                kind: "mock".into(),
                name: "Mock e-Fatura".into(),
                config: json!({}),
                secrets: Some(json!({})),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("integration")
        .id;
    app.setting("einvoice.enabled", json!(true)).await;
    app.setting("einvoice.integration_id", json!(integration.to_string()))
        .await;
    app.setting("einvoice.series_earsiv", json!("TST")).await;
    app.setting("einvoice.series_efatura", json!("TEF")).await;
}

/// An issued, fully paid invoice of `client` (100 TRY + KDV %20).
async fn paid_invoice(app: &TestApp, client: &ClientRow) -> InvoiceId {
    let today = Utc::now().date_naive();
    let mut conn = app.pool().acquire().await.unwrap();
    let inv = invoices::create_invoice(
        &mut conn,
        app.tenant_id,
        &NewInvoice {
            client_id: client.id,
            currency: Currency::TRY,
            issue_date: today,
            due_date: today + Days::new(7),
            issue: true,
            payment_integration_id: None,
            notes: Some("Teşekkürler <script>alert(1)</script>".into()),
            billing_snapshot: hos_billing::checkout::billing_snapshot(client),
            items: vec![NewInvoiceItem::manual(
                "Web hosting — ğüşıöç İ",
                Decimal::from(100),
            )],
            taxes: vec![TaxLineInput {
                tax_rule_id: None,
                name: "KDV".into(),
                rate: Decimal::from(20),
                is_compound: false,
            }],
        },
    )
    .await
    .expect("invoice");
    drop(conn);
    app.state
        .billing
        .record_manual_payment(
            app.tenant_id,
            &ManualPayment {
                invoice_id: inv.id,
                amount: inv.balance,
                integration_id: None,
                method: Some("bank_transfer".into()),
                reference: Some(format!("REF-{}", uuid::Uuid::now_v7().simple())),
                occurred_at: None,
                note: None,
                staff_user_id: None,
            },
        )
        .await
        .expect("payment");
    inv.id
}

async fn document(app: &TestApp, invoice: InvoiceId) -> EInvoiceDocumentRow {
    let mut conn = app.pool().acquire().await.unwrap();
    edocs::for_invoice(&mut conn, app.tenant_id, invoice, "sale")
        .await
        .unwrap()
        .expect("e-document row")
}

/// Runs the queued submission of `invoice`'s document.
async fn issue(app: &TestApp, invoice: InvoiceId) -> EInvoiceDocumentRow {
    let doc = document(app, invoice).await;
    app.state
        .billing
        .einvoice_process(app.tenant_id, doc.id, EInvoiceAction::Submit, false)
        .await
        .expect("process");
    let doc = document(app, invoice).await;
    assert_eq!(doc.status, "issued", "{:?}", doc.last_error);
    doc
}

fn is_pdf(r: &Res) -> bool {
    r.status == StatusCode::OK
        && r.content_type() == "application/pdf"
        && r.body.starts_with("%PDF")
        && r.headers[http::header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"")
}

async fn api(app: &TestApp, path: &str, key: &str) -> Res {
    let req = http::Request::builder()
        .method(Method::GET)
        .uri(path)
        .header("host", &app.host)
        .header("authorization", format!("Bearer {key}"))
        .body(axum::body::Body::empty())
        .expect("request");
    let res = app.router.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 16 << 20)
        .await
        .expect("body");
    Res {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn api_key(app: &TestApp, user: UserId) -> String {
    let g = hos_auth::tokens::generate_api_key();
    identity::create_api_key(
        app.pool(),
        app.tenant_id,
        Some(user),
        "test key",
        &g.lookup,
        &g.key_hash,
        &[],
        None,
    )
    .await
    .expect("api key");
    g.key.expose_str().to_owned()
}

#[tokio::test]
async fn downloads_check_permission_ownership_and_tenant() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    enable(&app).await;
    let (alice, alice_user, alice_email) = client_user(&app, "Ayşe").await;
    let (bob, _, _) = client_user(&app, "Burak").await;
    let inv_a = paid_invoice(&app, &alice).await;
    let inv_b = paid_invoice(&app, &bob).await;
    let doc = issue(&app, inv_a).await;
    let number = doc.number.clone().expect("number");

    // Anonymous: nothing.
    for p in [
        format!("/client/invoices/{inv_a}/pdf"),
        format!("/admin/invoices/{inv_a}/pdf"),
        format!("/admin/einvoice/{}/pdf", doc.id),
    ] {
        let r = app.get(&p).await;
        assert_ne!(r.status, StatusCode::OK, "{p}");
        assert!(!r.body.starts_with("%PDF"), "{p}");
    }

    // Sales staff: billing.view → downloads yes, actions no.
    let csrf = staff(&mut app, "sales").await;
    let r = app.get(&format!("/admin/invoices/{inv_a}/pdf")).await;
    assert!(is_pdf(&r), "{} {}", r.status, r.content_type());
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    let page = app.get(&format!("/admin/invoices/{inv_a}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(&number), "panel shows the number");
    assert!(
        page.body
            .contains(&format!("/admin/einvoice/{}/pdf", doc.id))
    );
    assert!(
        !page
            .body
            .contains(&format!("/admin/einvoice/{}/refresh", doc.id)),
        "no manage actions"
    );
    assert!(is_pdf(
        &app.get(&format!("/admin/einvoice/{}/pdf", doc.id)).await
    ));
    let ubl = app.get(&format!("/admin/einvoice/{}/ubl", doc.id)).await;
    assert_eq!(ubl.status, StatusCode::OK);
    assert_eq!(ubl.content_type(), "application/xml");
    assert!(ubl.body.contains("<Invoice"), "{}", ubl.body);
    let list = app.get("/admin/einvoice?status=all").await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.contains(&number));
    assert!(
        !app.get("/admin/einvoice").await.body.contains(&number),
        "no problems"
    );
    for action in ["retry", "cancel", "refresh", "acknowledge"] {
        let r = post(
            &mut app,
            &format!("/admin/einvoice/{}/{action}", doc.id),
            &csrf,
            &[],
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{action}");
    }
    let r = post(
        &mut app,
        &format!("/admin/invoices/{inv_b}/einvoice"),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let missing = uuid::Uuid::now_v7();
    assert_eq!(
        app.get(&format!("/admin/einvoice/{missing}/pdf"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get(&format!("/admin/invoices/{missing}/pdf"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Billing staff: actions with CSRF only, audited.
    let csrf = staff(&mut app, "billing").await;
    let r = app
        .post(&format!("/admin/einvoice/{}/refresh", doc.id), &[])
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "CSRF is enforced");
    let r = post(
        &mut app,
        &format!("/admin/einvoice/{}/refresh", doc.id),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where tenant_id = $1 and action = 'einvoice.refresh'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // The client: own invoice only; the legal PDF only for a live document.
    login(&mut app, &alice_email).await;
    let r = app.get(&format!("/client/invoices/{inv_a}/pdf")).await;
    assert!(
        is_pdf(&r),
        "{} {}",
        r.status,
        r.body.chars().take(200).collect::<String>()
    );
    assert_eq!(
        app.get(&format!("/client/invoices/{inv_b}/pdf"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get(&format!("/client/invoices/{inv_a}/e-invoice"))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a test-environment document is not handed to clients"
    );
    sqlx::query("update einvoice_documents set is_test = false where id = $1")
        .bind(doc.id)
        .execute(app.pool())
        .await
        .unwrap();
    assert!(is_pdf(
        &app.get(&format!("/client/invoices/{inv_a}/e-invoice"))
            .await
    ));
    assert_eq!(
        app.get(&format!("/client/invoices/{inv_b}/e-invoice"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let r = app.get(&format!("/admin/einvoice/{}/pdf", doc.id)).await;
    assert!(
        !r.body.starts_with("%PDF"),
        "client reached the admin download"
    );

    // REST API: client keys see their own invoices only.
    let key = api_key(&app, alice_user).await;
    let r = api(&app, &format!("/api/v1/invoices/{inv_a}/pdf"), &key).await;
    assert!(is_pdf(&r), "{} {}", r.status, r.body);
    let r = api(&app, &format!("/api/v1/invoices/{inv_b}/pdf"), &key).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = api(
        &app,
        &format!("/api/v1/invoices/{inv_a}/pdf?locale=xx"),
        &key,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Another tenant's invoice / document: 404 for this tenant's staff.
    let Some(other) = TestApp::new().await else {
        return;
    };
    enable(&other).await;
    let (carol, _, _) = client_user(&other, "Cem").await;
    let inv_c = paid_invoice(&other, &carol).await;
    let doc_c = issue(&other, inv_c).await;
    staff(&mut app, "admin").await;
    for p in [
        format!("/admin/invoices/{inv_c}/pdf"),
        format!("/admin/einvoice/{}/pdf", doc_c.id),
        format!("/admin/einvoice/{}/ubl", doc_c.id),
    ] {
        assert_eq!(app.get(&p).await.status, StatusCode::NOT_FOUND, "{p}");
    }
    other.cleanup().await;
    app.cleanup().await;
}

#[tokio::test]
async fn settings_form_permissions_and_validation() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let path = "/admin/settings/einvoice";
    // Billing staff may not change settings.
    let csrf = staff(&mut app, "billing").await;
    assert_eq!(app.get(path).await.status, StatusCode::FORBIDDEN);
    let r = post(&mut app, path, &csrf, &[("einvoice.enabled", "1")]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let csrf = staff(&mut app, "admin").await;
    let page = app.get(path).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("name=\"einvoice.series_earsiv\""));
    assert!(
        page.body.contains("Henüz e-Fatura entegrasyonu yok"),
        "warns: no integration"
    );
    assert!(
        page.body.contains("Firma bilgileri eksik"),
        "warns: company data"
    );

    let base = [
        ("einvoice.enabled", "1"),
        ("einvoice.integration_id", ""),
        ("einvoice.issue_on", "payment"),
        ("einvoice.start_date", ""),
        ("einvoice.exemption_code", "351"),
        ("einvoice.exemption_reason", "Diğer"),
        ("invoice_pdf.bank_details", "TR00 0000 <b>IBAN</b>"),
    ];
    let bad: [&[(&str, &str)]; 5] = [
        &[
            ("einvoice.series_earsiv", "AB"),
            ("einvoice.series_efatura", "DEF"),
        ],
        &[
            ("einvoice.series_earsiv", "ABC"),
            ("einvoice.series_efatura", "abc"),
        ],
        &[
            ("einvoice.series_earsiv", ""),
            ("einvoice.series_efatura", "DEF"),
        ],
        &[
            ("einvoice.series_earsiv", "ABC"),
            ("einvoice.series_efatura", "DEF"),
            ("einvoice.exemption_code", "801"),
        ],
        &[
            ("einvoice.series_earsiv", "ABC"),
            ("einvoice.series_efatura", "DEF"),
            ("einvoice.issue_on", "always"),
        ],
    ];
    for extra in bad {
        let mut fields: Vec<(&str, &str)> = base
            .iter()
            .filter(|(k, _)| !extra.iter().any(|(e, _)| e == k))
            .copied()
            .collect();
        fields.extend_from_slice(extra);
        let r = post(&mut app, path, &csrf, &fields).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{extra:?}");
    }
    let stored = hos_db::repo::platform::get_settings(app.pool(), app.tenant_id, "einvoice.")
        .await
        .unwrap();
    assert!(stored.is_empty(), "nothing saved: {stored:?}");
    // Without the CSRF token.
    let r = app.post(path, &base).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let mut fields = base.to_vec();
    fields.extend_from_slice(&[
        ("einvoice.series_earsiv", "abc"),
        ("einvoice.series_efatura", "DEF"),
    ]);
    let r = post(&mut app, path, &csrf, &fields).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let stored = hos_db::repo::platform::get_settings(app.pool(), app.tenant_id, "")
        .await
        .unwrap();
    assert_eq!(stored["einvoice.enabled"], json!(true));
    assert_eq!(stored["einvoice.series_earsiv"], json!("ABC"));
    assert_eq!(stored["einvoice.exemption_code"], json!("351"));
    let start = stored["einvoice.start_date"].as_str().unwrap();
    assert!(
        chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d").is_ok(),
        "{start}"
    );
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where tenant_id = $1 and action = 'settings.einvoice'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert_eq!(audited, 1);
    // The bank details are escaped when the form is shown again.
    let page = app.get(path).await;
    assert!(page.body.contains("TR00 0000 &#60;b&#62;IBAN&#60;/b&#62;"));
    assert!(!page.body.contains("<b>IBAN"));
    app.cleanup().await;
}

#[tokio::test]
async fn tax_ids_are_checked_only_while_einvoicing_is_on() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "admin").await;
    let company = |email: &'static str, country: &'static str, vkn: &'static str| {
        vec![
            ("kind", "corporate"),
            ("first_name", "Ali"),
            ("last_name", "Veli"),
            ("company_name", "Örnek Ltd. Şti."),
            ("email", email),
            ("country_code", country),
            ("currency", "TRY"),
            ("locale", "tr"),
            ("city", "İzmir"),
            ("tax_number", vkn),
            ("tax_office", ""),
        ]
    };
    // Off: the old, lenient rules.
    let r = post(
        &mut app,
        "/admin/clients",
        &csrf,
        &company("a1@client.test", "TR", "1234567891"),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);

    app.setting("einvoice.enabled", json!(true)).await;
    let r = post(
        &mut app,
        "/admin/clients",
        &csrf,
        &company("a2@client.test", "TR", "1234567891"),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("Geçerli bir VKN"), "tax number error shown");
    assert!(
        r.body.contains("vergi dairesi zorunludur"),
        "tax office error shown"
    );
    let mut ok = company("a3@client.test", "TR", "1234567890");
    ok.retain(|(k, _)| *k != "tax_office");
    ok.push(("tax_office", "Konak"));
    let r = post(&mut app, "/admin/clients", &csrf, &ok).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // Outside Türkiye the Turkish rules do not apply.
    let r = post(
        &mut app,
        "/admin/clients",
        &csrf,
        &company("a4@client.test", "DE", "DE123456789"),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);

    // Client billing profile: a TCKN with a wrong check digit.
    let (_, _, email) = client_user(&app, "Deniz").await;
    login(&mut app, &email).await;
    let form = app.get("/client/account/billing").await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    let token = form.csrf();
    let fields = |tckn: &'static str| {
        vec![
            ("kind", "individual"),
            ("first_name", "Deniz"),
            ("last_name", "Yılmaz"),
            ("country_code", "TR"),
            ("city", "İstanbul"),
            ("national_id", tckn),
        ]
    };
    let r = post(
        &mut app,
        "/client/account/billing",
        &token,
        &fields("10000000147"),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("T.C. kimlik numarası geçersiz"));
    let r = post(
        &mut app,
        "/client/account/billing",
        &token,
        &fields("10000000146"),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    app.cleanup().await;
}
