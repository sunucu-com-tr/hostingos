//! Settings / staff / developer / reports integration tests: role guards,
//! invitations, deactivation, API keys, webhook targets, billing settings,
//! e-mail previews, branding, plan limits, social linking, reports and
//! tenant isolation.
//!
//! Like the other web tests they need the database from `.env`; without it
//! they print a notice and pass. No worker runs here.

use std::collections::BTreeMap;

use crate::common::{self, Res, TestApp};
use chrono::NaiveDate;
use hos_core::config::OAuthAppConfig;
use hos_core::domain::{InvoiceStatus, UserKind};
use hos_core::ids::{RoleId, UserId};
use hos_core::money::Currency;
use hos_core::secret::SecretString;
use hos_db::enums::{AccountStatus, OAuthProvider};
use hos_db::repo::clients::{self, ClientInput};
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::invoices;
use hos_web::config::OAuthEndpoints;
use http::{Method, StatusCode};
use rust_decimal::Decimal;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "Correct-Horse-Battery-9";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}@staff.test",
        &uuid::Uuid::now_v7().simple().to_string()[16..]
    )
}

async fn user_named(
    app: &TestApp,
    email: &str,
    kind: UserKind,
    role: Option<&str>,
    first: &str,
) -> UserId {
    let u = identity::create_user(
        app.pool(),
        app.tenant_id,
        &NewUser {
            email: email.into(),
            password_hash: Some(hos_auth::password::hash_password(PASSWORD).unwrap()),
            kind,
            first_name: first.into(),
            last_name: "Personel".into(),
            phone: None,
            locale: hos_core::domain::Locale::Tr,
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
    let token = app.get("/auth/login").await.csrf();
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

/// Logs in a fresh staff member with `role`; returns `(user id, csrf)`.
async fn staff(app: &mut TestApp, role: &str) -> (UserId, String) {
    staff_named(app, role, "Test").await
}

async fn staff_named(app: &mut TestApp, role: &str, first: &str) -> (UserId, String) {
    let email = unique(role);
    let id = user_named(app, &email, UserKind::Staff, Some(role), first).await;
    login(app, &email).await;
    let dash = app.get("/admin").await;
    assert_eq!(dash.status, StatusCode::OK, "{}", dash.body);
    (id, dash.csrf())
}

async fn post(app: &mut TestApp, path: &str, csrf: &str, fields: &[(&str, &str)]) -> Res {
    let mut all = vec![("csrf_token", csrf)];
    all.extend_from_slice(fields);
    app.post(path, &all).await
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .expect(sql)
}

async fn user_status(app: &TestApp, id: UserId) -> AccountStatus {
    identity::get_user(app.pool(), app.tenant_id, id)
        .await
        .unwrap()
        .expect("user")
        .status
}

/// Text between `start` and the next `end`.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn attr(tag: &str, name: &str) -> Option<String> {
    between(tag, &format!(" {name}=\""), "\"").map(unescape)
}

/// `value` of the input named `name`.
fn input_value(body: &str, name: &str) -> Option<String> {
    let at = body.find(&format!("name=\"{name}\""))?;
    let start = body[..at].rfind('<')?;
    let end = at + body[at..].find('>')?;
    attr(&body[start..end], "value")
}

/// The fields a browser would submit for the form posting to `action`.
fn form_fields(body: &str, action: &str) -> Vec<(String, String)> {
    let marker = format!("action=\"{action}\"");
    let start = body.find(&marker).expect("form");
    let form = &body[start..start + body[start..].find("</form>").expect("form end")];
    let mut out = Vec::new();
    let mut rest = form;
    while let Some(i) = rest.find('<') {
        rest = &rest[i..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        if tag.starts_with("<input") {
            if let Some(name) = attr(tag, "name") {
                let kind = attr(tag, "type").unwrap_or_default();
                let value = attr(tag, "value").unwrap_or_default();
                if kind == "checkbox" || kind == "radio" {
                    if tag.contains(" checked") {
                        out.push((name, if value.is_empty() { "1".into() } else { value }));
                    }
                } else if kind != "submit" && kind != "file" {
                    out.push((name, value));
                }
            }
        } else if tag.starts_with("<select") {
            let name = attr(tag, "name").unwrap_or_default();
            let close = rest.find("</select>").unwrap_or(rest.len());
            let inner = &rest[..close];
            let mut first = None;
            let mut chosen = None;
            for opt in inner.split("<option").skip(1) {
                let v = attr(opt, "value").unwrap_or_default();
                first.get_or_insert(v.clone());
                if opt[..opt.find('>').unwrap_or(0)].contains(" selected") {
                    chosen = Some(v);
                }
            }
            if let Some(v) = chosen.or(first) {
                out.push((name, v));
            }
        } else if tag.starts_with("<textarea") {
            let name = attr(tag, "name").unwrap_or_default();
            let text = between(rest, ">", "</textarea>").unwrap_or_default();
            out.push((name, unescape(text)));
        }
        rest = &rest[1..];
    }
    out
}

/// Data rows of a CSV export (BOM and header dropped).
fn csv_rows(body: &str) -> Vec<Vec<String>> {
    body.trim_start_matches('\u{feff}')
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.split(',')
                .map(|c| c.trim_matches('"').to_owned())
                .collect()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// C. staff & roles
// ---------------------------------------------------------------------------

#[tokio::test]
async fn role_guards_block_escalation() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // Support: no staff.manage → 403 on every staff route, hidden in the menu.
    let (_, csrf) = staff(&mut app, "support").await;
    for p in [
        "/admin/staff",
        "/admin/staff/roles",
        "/admin/staff/roles/new",
    ] {
        assert_eq!(app.get(p).await.status, StatusCode::FORBIDDEN, "{p}");
    }
    let r = post(
        &mut app,
        "/admin/staff/invite",
        &csrf,
        &[("email", "x@y.test")],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(!app.get("/admin").await.body.contains("/admin/staff\""));
    app.cookies.clear();

    // The owner creates a narrow custom role.
    let (owner_id, csrf) = staff(&mut app, "owner").await;
    let r = post(
        &mut app,
        "/admin/staff/roles/new",
        &csrf,
        &[
            ("name", "Ekip lideri"),
            ("perm", "staff.manage"),
            ("perm", "clients.view"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let lead_role: uuid::Uuid =
        sqlx::query_scalar("select id from roles where tenant_id = $1 and name = 'Ekip lideri'")
            .bind(app.tenant_id)
            .fetch_one(app.pool())
            .await
            .unwrap();
    let owner_jar = std::mem::take(&mut app.cookies);

    // A team lead (staff.manage + clients.view) cannot hand out more.
    let email = unique("lead");
    let lead_id = user_named(&app, &email, UserKind::Staff, None, "Lider").await;
    identity::assign_role(
        app.pool(),
        app.tenant_id,
        lead_id,
        RoleId::from_uuid(lead_role),
    )
    .await
    .unwrap();
    login(&mut app, &email).await;
    let page = app.get("/admin/staff").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let csrf = page.csrf();
    // Role with a permission the lead does not hold → refused.
    post(
        &mut app,
        "/admin/staff/roles/new",
        &csrf,
        &[("name", "Kaçak"), ("perm", "billing.manage")],
    )
    .await;
    assert_eq!(
        count(
            &app,
            "select count(*) from roles where tenant_id = $1 and name = 'Kaçak'"
        )
        .await,
        0
    );
    // Inviting someone as administrator → refused; with the own role → fine.
    let admin_role = identity::role_by_slug(app.pool(), app.tenant_id, "admin")
        .await
        .unwrap()
        .unwrap();
    let admin_role_id = admin_role.id.to_string();
    post(
        &mut app,
        "/admin/staff/invite",
        &csrf,
        &[
            ("email", "yeni-admin@staff.test"),
            ("first_name", "Yeni"),
            ("role_id", &admin_role_id),
        ],
    )
    .await;
    let invites = "select count(*) from staff_invites where tenant_id = $1";
    assert_eq!(count(&app, invites).await, 0);
    let own_role = lead_role.to_string();
    let r = post(
        &mut app,
        "/admin/staff/invite",
        &csrf,
        &[
            ("email", "yeni-uye@staff.test"),
            ("first_name", "Yeni"),
            ("role_id", &own_role),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(count(&app, invites).await, 1);
    // The owner outranks the lead; nobody deactivates themselves.
    post(
        &mut app,
        &format!("/admin/staff/{owner_id}/deactivate"),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(user_status(&app, owner_id).await, AccountStatus::Active);
    let support = identity::role_by_slug(app.pool(), app.tenant_id, "support")
        .await
        .unwrap()
        .unwrap()
        .id
        .to_string();
    post(
        &mut app,
        &format!("/admin/staff/{owner_id}/roles"),
        &csrf,
        &[("role_ids", &support)],
    )
    .await;
    assert_eq!(has_role(&app, owner_id, "owner").await, 1);
    post(
        &mut app,
        &format!("/admin/staff/{lead_id}/deactivate"),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(user_status(&app, lead_id).await, AccountStatus::Active);
    // Nobody raises their own role.
    post(
        &mut app,
        &format!("/admin/staff/{lead_id}/roles"),
        &csrf,
        &[("role_ids", &admin_role_id)],
    )
    .await;
    assert_eq!(has_role(&app, lead_id, "admin").await, 0);

    // Owners: a second owner can be demoted; the last one can be neither
    // demoted nor deactivated.
    app.cookies = owner_jar;
    let csrf = app.get("/admin/staff").await.csrf();
    let second = user_named(
        &app,
        &unique("owner2"),
        UserKind::Staff,
        Some("owner"),
        "İkinci",
    )
    .await;
    let r = post(
        &mut app,
        &format!("/admin/staff/{second}/roles"),
        &csrf,
        &[("role_ids", &support)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(has_role(&app, second, "owner").await, 0);
    post(
        &mut app,
        &format!("/admin/staff/{owner_id}/roles"),
        &csrf,
        &[("role_ids", &support)],
    )
    .await;
    post(
        &mut app,
        &format!("/admin/staff/{owner_id}/deactivate"),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(has_role(&app, owner_id, "owner").await, 1);
    assert_eq!(user_status(&app, owner_id).await, AccountStatus::Active);
    app.cleanup().await;
}

async fn has_role(app: &TestApp, user: UserId, slug: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "select count(*) from user_roles ur join roles r on r.id = ur.role_id
          where ur.user_id = $1 and r.slug = $2",
    )
    .bind(user)
    .bind(slug)
    .fetch_one(app.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn invite_accept_then_login() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "owner").await;
    let role = identity::role_by_slug(app.pool(), app.tenant_id, "support")
        .await
        .unwrap()
        .unwrap()
        .id
        .to_string();
    let email = unique("davet");
    let r = post(
        &mut app,
        "/admin/staff/invite",
        &csrf,
        &[
            ("email", &email),
            ("first_name", "Ayşe"),
            ("last_name", "Yılmaz"),
            ("locale", "tr"),
            ("role_id", &role),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // The link only exists in the queued e-mail: `email_log` keeps a
    // redacted copy, the message the mailer gets carries the link.
    let log_id: hos_db::ids::EmailLogId = sqlx::query_scalar(
        "select id from email_log where tenant_id = $1 and to_address = $2
          order by created_at desc limit 1",
    )
    .bind(app.tenant_id)
    .bind(&email)
    .fetch_one(app.pool())
    .await
    .expect("invite e-mail");
    let message = app
        .state
        .billing
        .logged_email_message(app.tenant_id, log_id)
        .await
        .expect("logged e-mail")
        .expect("invite e-mail message");
    let body = message.text_body.expect("text body");
    let token = between(&body, "token=", "\n")
        .map(|t| t.split_whitespace().next().unwrap_or_default().to_owned())
        .expect("token in e-mail");
    app.cookies.clear();

    // A wrong token shows no form.
    assert_eq!(
        app.get("/auth/invite?token=wrong").await.status,
        StatusCode::NOT_FOUND
    );
    let page = app.get(&format!("/auth/invite?token={token}")).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let r = app
        .post(
            "/auth/invite",
            &[
                ("csrf_token", &page.csrf()),
                ("token", &token),
                ("first_name", "Ayşe"),
                ("last_name", "Yılmaz"),
                ("password", PASSWORD),
                ("password_confirm", PASSWORD),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        app.get("/admin/support/tickets").await.status,
        StatusCode::OK
    );
    // Used tokens cannot be replayed.
    app.cookies.clear();
    let again = app.get(&format!("/auth/invite?token={token}")).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    // The new member signs in with the chosen password and has the role.
    login(&mut app, &email).await;
    assert_eq!(app.get("/admin").await.status, StatusCode::OK);
    assert_eq!(app.get("/admin/staff").await.status, StatusCode::FORBIDDEN);
    app.cleanup().await;
}

#[tokio::test]
async fn deactivated_member_loses_session() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (member, _) = staff(&mut app, "support").await;
    let member_jar = std::mem::take(&mut app.cookies);
    let (_, csrf) = staff(&mut app, "owner").await;
    let r = post(
        &mut app,
        &format!("/admin/staff/{member}/deactivate"),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(user_status(&app, member).await, AccountStatus::Disabled);
    let sessions = sqlx::query_scalar::<_, i64>("select count(*) from sessions where user_id = $1")
        .bind(member)
        .fetch_one(app.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    // The member's browser is signed out.
    app.cookies = member_jar;
    let r = app.get("/admin").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("/auth/login"), "{}", r.location());
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// D. developer
// ---------------------------------------------------------------------------

async fn me(app: &mut TestApp, host: &str, key: &str) -> StatusCode {
    let bearer = format!("Bearer {key}");
    app.send_to(
        host,
        Method::GET,
        "/api/v1/me",
        None,
        &[("authorization", &bearer), ("accept", "application/json")],
    )
    .await
    .status
}

/// Creates an API key through the UI; returns the secret.
async fn create_key(app: &mut TestApp, csrf: &str, name: &str) -> String {
    let r = post(
        app,
        "/admin/developer/keys",
        csrf,
        &[("name", name), ("scopes", "clients.view")],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let secret = between(&r.body, "<code id=\"api-key-secret\">", "</code>")
        .expect("secret shown once")
        .to_owned();
    assert!(secret.starts_with("hos_live_"), "{secret}");
    assert!(
        r.headers
            .get(http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("no-store"))
    );
    secret
}

#[tokio::test]
async fn api_key_create_use_revoke() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "owner").await;
    let key = create_key(&mut app, &csrf, "CI").await;
    // Only the hash is stored; the list never shows the secret again.
    let list = app.get("/admin/developer").await;
    assert!(!list.body.contains(&key));
    let host = app.host.clone();
    assert_eq!(me(&mut app, &host, &key).await, StatusCode::OK);
    // A scope the creator does not hold is refused.
    app.cookies.clear();
    let (_, csrf_b) = staff(&mut app, "admin").await;
    post(
        &mut app,
        "/admin/developer/keys",
        &csrf_b,
        &[
            ("name", "Fazla"),
            ("scopes", "staff.manage"),
            ("scopes", "platform.manage"),
        ],
    )
    .await;
    assert_eq!(
        count(
            &app,
            "select count(*) from api_keys where tenant_id = $1 and name = 'Fazla'"
        )
        .await,
        0
    );
    let id: uuid::Uuid =
        sqlx::query_scalar("select id from api_keys where tenant_id = $1 and name = 'CI'")
            .bind(app.tenant_id)
            .fetch_one(app.pool())
            .await
            .unwrap();
    let r = post(
        &mut app,
        &format!("/admin/developer/keys/{id}/revoke"),
        &csrf_b,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(me(&mut app, &host, &key).await, StatusCode::UNAUTHORIZED);
    app.cleanup().await;
}

#[tokio::test]
async fn webhook_targets_must_be_public() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "owner").await;
    let hooks = "select count(*) from webhook_endpoints where tenant_id = $1";
    for url in [
        "http://127.0.0.1/hook",
        "http://169.254.169.254/latest/meta-data/",
        "https://127.0.0.1/hook",
        "https://[::1]/hook",
        "https://10.0.0.5/hook",
        "https://localhost/hook",
        "ftp://93.184.215.14/hook",
    ] {
        let r = post(
            &mut app,
            "/admin/developer/webhooks",
            &csrf,
            &[("url", url), ("events", "invoice.paid"), ("is_active", "1")],
        )
        .await;
        assert_ne!(r.status, StatusCode::SEE_OTHER, "{url} accepted");
        assert!(
            r.body.contains("field--error") || r.body.contains("alert"),
            "{url}"
        );
        assert_eq!(count(&app, hooks).await, 0, "{url}");
    }
    // A public literal address passes (no DNS needed).
    let r = post(
        &mut app,
        "/admin/developer/webhooks",
        &csrf,
        &[
            ("url", "https://93.184.215.14/hook"),
            ("events", "invoice.paid"),
            ("is_active", "1"),
        ],
    )
    .await;
    assert!(
        r.status == StatusCode::SEE_OTHER || r.status == StatusCode::OK,
        "{}",
        r.body
    );
    assert_eq!(count(&app, hooks).await, 1);
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// B. settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn billing_setting_changes_new_invoice_due_date() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "owner").await;
    let page = app.get("/admin/settings/billing").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let mut fields: BTreeMap<String, String> = form_fields(&page.body, "/admin/settings/billing")
        .into_iter()
        .collect();
    fields.insert("billing.payment_terms_days".into(), "21".into());
    fields.insert("csrf_token".into(), csrf.clone());
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let r = app.post("/admin/settings/billing", &pairs).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    // New manual invoices now default to issue date + 21 days.
    let form = app.get("/admin/invoices/new").await;
    let issue = input_value(&form.body, "issue_date").expect("issue");
    let due = input_value(&form.body, "due_date").expect("due");
    let issue = NaiveDate::parse_from_str(&issue, "%Y-%m-%d").unwrap();
    let due = NaiveDate::parse_from_str(&due, "%Y-%m-%d").unwrap();
    assert_eq!((due - issue).num_days(), 21);
    // Out of range values are refused and nothing changes.
    let mut bad = fields.clone();
    bad.insert("billing.payment_terms_days".into(), "999".into());
    let pairs: Vec<(&str, &str)> = bad.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let r = app.post("/admin/settings/billing", &pairs).await;
    assert_ne!(r.status, StatusCode::SEE_OTHER);
    let v = hos_db::repo::platform::get_setting(
        app.pool(),
        app.tenant_id,
        "billing.payment_terms_days",
    )
    .await
    .unwrap();
    assert_eq!(v, Some(json!(21)));
    app.cleanup().await;
}

/// `payments.return_url_hosts` on the billing settings page: exact host
/// names only (one per line, lower-cased), anything else is refused and
/// nothing is stored.
#[tokio::test]
async fn return_url_hosts_are_edited_and_validated() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "owner").await;
    let page = app.get("/admin/settings/billing").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("name=\"payments.return_url_hosts\""));
    let mut fields: BTreeMap<String, String> = form_fields(&page.body, "/admin/settings/billing")
        .into_iter()
        .collect();
    fields.insert("csrf_token".into(), csrf.clone());
    let stored = |app: &TestApp| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        async move {
            hos_db::repo::platform::get_setting(&pool, tenant, "payments.return_url_hosts")
                .await
                .unwrap()
        }
    };
    let mut ok = fields.clone();
    ok.insert(
        "payments.return_url_hosts".into(),
        "Shop.Partner.example\npay.partner.example.\n".into(),
    );
    let pairs: Vec<(&str, &str)> = ok.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let r = app.post("/admin/settings/billing", &pairs).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        stored(&app).await,
        Some(json!(["shop.partner.example", "pay.partner.example"]))
    );
    let page = app.get("/admin/settings/billing").await;
    assert!(
        page.body
            .contains("shop.partner.example\npay.partner.example")
    );
    for bad in [
        "https://evil.example/",
        "*.evil.example",
        "evil.example/path",
        "user@evil.example",
        "localhost",
    ] {
        let mut f = fields.clone();
        f.insert("payments.return_url_hosts".into(), bad.into());
        let pairs: Vec<(&str, &str)> = f.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let r = app.post("/admin/settings/billing", &pairs).await;
        assert_ne!(r.status, StatusCode::SEE_OTHER, "{bad}");
        assert_eq!(
            stored(&app).await,
            Some(json!(["shop.partner.example", "pay.partner.example"])),
            "{bad}: nothing stored"
        );
    }
    app.cleanup().await;
}

#[tokio::test]
async fn email_preview_escapes_variables() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff_named(&mut app, "owner", "<script>alert(1)</script>").await;
    let r = post(
        &mut app,
        "/admin/settings/email/templates/welcome/tr/preview",
        &csrf,
        &[
            ("subject", "Merhaba {{user.name}}"),
            ("body_html", "<p>Merhaba {{user.name}}</p>"),
            ("body_text", "Merhaba {{user.name}}"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("<script>alert(1)"), "{}", r.body);
    assert!(r.body.contains("&lt;script&gt;"), "{}", r.body);
    let csp = r
        .headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        csp.contains("script-src 'none'") || csp.contains("default-src 'none'"),
        "{csp}"
    );
    app.cleanup().await;
}

#[tokio::test]
async fn brand_color_reaches_storefront() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "owner").await;
    let r = post(
        &mut app,
        "/admin/settings/general/brand",
        &csrf,
        &[("primary_color", "#0f5d75"), ("accent_color", "#b45309")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    app.cookies.clear();
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("/brand.css"));
    let css = app.get("/brand.css").await;
    assert!(css.body.contains("--brand: #0f5d75;"), "{}", css.body);
    assert!(css.body.contains("--accent: #b45309;"), "{}", css.body);
    // Invalid colours never reach the stylesheet.
    app.cookies.clear();
    let (_, csrf) = staff(&mut app, "owner").await;
    post(
        &mut app,
        "/admin/settings/general/brand",
        &csrf,
        &[
            ("primary_color", "red;}body{x"),
            ("accent_color", "#b45309"),
        ],
    )
    .await;
    let css = app.get("/brand.css").await;
    assert!(!css.body.contains("body{x"), "{}", css.body);
    app.cleanup().await;
}

#[tokio::test]
async fn client_creation_respects_plan_limit() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    sqlx::query(
        "update tenants set metadata = jsonb_set(coalesce(metadata, '{}'::jsonb), '{limits}',
                '{\"max_clients\": 2}'::jsonb) where id = $1",
    )
    .bind(app.tenant_id)
    .execute(app.pool())
    .await
    .unwrap();
    clients::create_client(
        app.pool(),
        app.tenant_id,
        &ClientInput::individual("Mevcut", "Müşteri", "mevcut@client.test", Currency::TRY),
    )
    .await
    .unwrap();
    let (_, csrf) = staff(&mut app, "owner").await;
    let form = |email: &'static str| {
        vec![
            ("kind", "individual"),
            ("first_name", "Yeni"),
            ("last_name", "Müşteri"),
            ("email", email),
            ("country_code", "TR"),
            ("currency", "TRY"),
            ("locale", "tr"),
        ]
    };
    let live = "select count(*) from clients where tenant_id = $1";
    let r = post(
        &mut app,
        "/admin/clients",
        &csrf,
        &form("ikinci@client.test"),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(count(&app, live).await, 2);
    // At the limit: refused with a message, nothing created.
    let r = post(
        &mut app,
        "/admin/clients",
        &csrf,
        &form("ucuncu@client.test"),
    )
    .await;
    assert_ne!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(count(&app, live).await, 2);
    // Self sign-up is refused as well.
    app.cookies.clear();
    let token = app.get("/auth/register").await.csrf();
    let r = app
        .post(
            "/auth/register",
            &[
                ("csrf_token", &token),
                ("first_name", "Kayıt"),
                ("last_name", "Olan"),
                ("email", "kayit@client.test"),
                ("password", PASSWORD),
                ("terms", "1"),
                ("website", ""),
            ],
        )
        .await;
    assert_ne!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(count(&app, live).await, 2);
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// E. social linking (wiremock GitHub)
// ---------------------------------------------------------------------------

async fn register(app: &mut TestApp, email: &str) {
    let token = app.get("/auth/register").await.csrf();
    let r = app
        .post(
            "/auth/register",
            &[
                ("csrf_token", &token),
                ("first_name", "Bağlantı"),
                ("last_name", "Deneme"),
                ("email", email),
                ("password", PASSWORD),
                ("terms", "1"),
                ("website", ""),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "register: {}", r.body);
}

async fn github_mocks(server: &MockServer, id: u64, email: &str) {
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gho_test", "token_type": "bearer", "scope": "read:user,user:email"
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "login": "octo", "id": id, "name": "Octo", "email": null, "avatar_url": null
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user/emails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"email": email, "primary": true, "verified": true}
        ])))
        .mount(server)
        .await;
}

/// Starts a link from the security page and completes the callback.
async fn link_github(app: &mut TestApp) -> Res {
    let page = app.get("/client/account/security").await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "{} {}",
        page.location(),
        page.body
    );
    assert!(page.body.contains("/auth/oauth/github/link"), "link button");
    let r = post(
        app,
        "/auth/oauth/github/link",
        &page.csrf(),
        &[("next", "/client/account/security")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let auth = url::Url::parse(r.location()).expect("provider url");
    let state = auth
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state");
    app.get(&format!(
        "/auth/oauth/github/callback?code=gh-code&state={state}"
    ))
    .await
}

#[tokio::test]
async fn social_link_rejects_identity_of_another_user() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let server = MockServer::start().await;
    let base = server.uri();
    let mut config = app.state.config.clone();
    config.github_oauth = Some(OAuthAppConfig {
        client_id: "test-client".into(),
        client_secret: SecretString::from("test-secret"),
    });
    let mut web = app.state.web.clone();
    web.oauth_endpoints = Some(OAuthEndpoints {
        google: None,
        github: Some(hos_auth::oauth::GitHubEndpoints {
            authorize: url::Url::parse(&format!("{base}/login/oauth/authorize")).unwrap(),
            token: url::Url::parse(&format!("{base}/login/oauth/access_token")).unwrap(),
            api_base: url::Url::parse(&format!("{base}/api")).unwrap(),
        }),
    });
    let state = common::new_state(app.pool().clone(), config, web);
    app.router = hos_web::app(state.clone());
    app.state = state;

    app.setting("auth.require_email_verification", json!(false))
        .await;
    // B already owns GitHub account 1001.
    register(&mut app, "b-link@client.test").await;
    app.cookies.clear();
    let b = identity::user_by_email(app.pool(), app.tenant_id, "b-link@client.test")
        .await
        .unwrap()
        .unwrap();
    identity::link_identity(
        app.pool(),
        app.tenant_id,
        b.id,
        &identity::OAuthProfile {
            provider: OAuthProvider::Github,
            provider_user_id: "1001".into(),
            email: Some("octo@example.test".into()),
            display_name: None,
            avatar_url: None,
            raw: json!({}),
        },
    )
    .await
    .unwrap();

    // A tries to link the same GitHub account → refused, nothing changes,
    // A stays A.
    register(&mut app, "a-link@client.test").await;
    let a = identity::user_by_email(app.pool(), app.tenant_id, "a-link@client.test")
        .await
        .unwrap()
        .unwrap();
    // Linking needs a verified account e-mail (the `security` module covers the
    // refusal for an unverified one).
    identity::mark_email_verified(app.pool(), app.tenant_id, a.id)
        .await
        .unwrap();
    github_mocks(&server, 1001, "octo@example.test").await;
    let r = link_github(&mut app).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(
        r.location().contains("/client/account/security"),
        "{}",
        r.location()
    );
    let owner =
        identity::user_by_identity(app.pool(), app.tenant_id, OAuthProvider::Github, "1001")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(owner.id, b.id);
    assert!(
        identity::list_identities(app.pool(), app.tenant_id, a.id)
            .await
            .unwrap()
            .is_empty()
    );
    let me = app.get("/client/account/security").await;
    assert!(
        me.body.contains("a-link@client.test"),
        "still signed in as A"
    );

    // A free GitHub account links to A.
    github_mocks(&server, 2002, "a-github@example.test").await;
    let r = link_github(&mut app).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let owner =
        identity::user_by_identity(app.pool(), app.tenant_id, OAuthProvider::Github, "2002")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(owner.id, a.id);
    // Anonymous visitors cannot start a link.
    app.cookies.clear();
    let token = app.get("/auth/login").await.csrf();
    let r = app
        .post(
            "/auth/oauth/github/link",
            &[("csrf_token", &token), ("next", "/client")],
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().contains("/auth/login"), "{}", r.location());
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// A. reports
// ---------------------------------------------------------------------------

/// A paid invoice of `amount` for a new client in `currency`; returns the
/// collected total.
async fn paid_invoice(app: &mut TestApp, csrf: &str, currency: Currency, amount: &str) -> Decimal {
    let email = format!("{}@client.test", uuid::Uuid::now_v7().simple());
    let c = clients::create_client(
        app.pool(),
        app.tenant_id,
        &ClientInput::individual("Rapor", "Müşteri", &email, currency),
    )
    .await
    .unwrap();
    let today = chrono::Utc::now().date_naive().to_string();
    let r = post(
        app,
        "/admin/invoices",
        csrf,
        &[
            ("client_id", &c.id.to_string()),
            ("issue_date", &today),
            ("due_date", &today),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let path = r.location().to_owned();
    let r = post(
        app,
        &format!("{path}/items"),
        csrf,
        &[
            ("description", "Hizmet"),
            ("quantity", "1"),
            ("unit_price", amount),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    post(app, &format!("{path}/publish"), csrf, &[]).await;
    let id: uuid::Uuid = path.rsplit('/').next().unwrap().parse().unwrap();
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, id.into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.currency, currency);
    let total = inv.balance;
    let r = post(
        app,
        &format!("{path}/payment"),
        csrf,
        &[
            ("amount", &total.to_string()),
            ("method", "bank_transfer"),
            ("reference", &format!("RAPOR-{}", currency.as_str())),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let inv = invoices::get_invoice(app.pool(), app.tenant_id, id.into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inv.status, InvoiceStatus::Paid);
    total
}

fn net_sum(rows: &[Vec<String>], currency: &str) -> Decimal {
    rows.iter()
        .filter(|r| r.get(1).map(String::as_str) == Some(currency))
        .map(|r| r[6].parse::<Decimal>().unwrap())
        .sum()
}

#[tokio::test]
async fn reports_keep_currencies_apart() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "owner").await;
    let try_total = paid_invoice(&mut app, &csrf, Currency::TRY, "100").await;
    let usd_total = paid_invoice(&mut app, &csrf, Currency::USD, "40").await;
    app.cookies.clear();
    // Billing staff (reports.view) reads the reports.
    staff(&mut app, "billing").await;
    let today = chrono::Utc::now().date_naive();
    let range = format!(
        "from={}&to={}",
        today - chrono::Duration::days(7),
        today + chrono::Duration::days(1)
    );
    let page = app
        .get(&format!(
            "/admin/reports/revenue?{range}&currency=TRY&unit=day"
        ))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("<svg class=\"chart"), "chart rendered");
    // USD is mentioned as "also has data", never added to TRY.
    assert!(page.body.contains("report-note"), "other currency hinted");
    for (cur, own, other) in [("TRY", try_total, usd_total), ("USD", usd_total, try_total)] {
        let csv = app
            .get(&format!(
                "/admin/reports/revenue?{range}&currency={cur}&unit=day&format=csv&table=period"
            ))
            .await;
        assert_eq!(csv.status, StatusCode::OK);
        assert!(csv.content_type().starts_with("text/csv"));
        let rows = csv_rows(&csv.body);
        assert!(!rows.is_empty(), "{}", csv.body);
        assert!(rows.iter().all(|r| r[1] == cur), "{}", csv.body);
        assert_eq!(net_sum(&rows, cur), own);
        let mixed = (own + other).round_dp(2).to_string();
        assert!(
            !csv.body.contains(&mixed),
            "currencies summed: {}",
            csv.body
        );
    }
    // Every other report renders and exports.
    for tab in ["growth", "tax", "aging", "products", "domains"] {
        let r = app.get(&format!("/admin/reports/{tab}?{range}")).await;
        assert_eq!(r.status, StatusCode::OK, "{tab}: {}", r.body);
        let r = app
            .get(&format!("/admin/reports/{tab}?{range}&format=csv"))
            .await;
        assert!(r.content_type().starts_with("text/csv"), "{tab}");
    }
    // Support has no reports.view.
    app.cookies.clear();
    staff(&mut app, "support").await;
    assert_eq!(
        app.get("/admin/reports/revenue").await.status,
        StatusCode::FORBIDDEN
    );
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// permissions & isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn billing_role_cannot_open_settings() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (_, csrf) = staff(&mut app, "billing").await;
    for p in [
        "/admin/settings",
        "/admin/settings/general",
        "/admin/settings/billing",
        "/admin/settings/orders",
        "/admin/settings/security",
        "/admin/settings/registration",
        "/admin/settings/localization",
        "/admin/settings/domains",
        "/admin/settings/automation",
        "/admin/settings/email",
        "/admin/developer",
        "/admin/developer/webhooks",
        "/admin/staff",
    ] {
        assert_eq!(app.get(p).await.status, StatusCode::FORBIDDEN, "{p}");
    }
    for p in [
        "/admin/settings/billing",
        "/admin/settings/general/brand",
        "/admin/developer/keys",
        "/admin/developer/webhooks",
    ] {
        let r = post(&mut app, p, &csrf, &[("billing.payment_terms_days", "1")]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "POST {p}");
    }
    let dash = app.get("/admin").await;
    assert!(dash.body.contains("/admin/reports"));
    assert!(!dash.body.contains("/admin/developer\""));
    assert!(!dash.body.contains("/admin/settings\""));
    app.cleanup().await;
}

#[tokio::test]
async fn settings_objects_are_tenant_scoped() {
    let Some(mut a) = TestApp::new().await else {
        return;
    };
    let Some(mut b) = TestApp::new().await else {
        return;
    };
    // Tenant A: a role, a staff member, an API key and a webhook.
    let (a_staff, csrf) = staff(&mut a, "owner").await;
    post(
        &mut a,
        "/admin/staff/roles/new",
        &csrf,
        &[("name", "A rolü"), ("perm", "clients.view")],
    )
    .await;
    let role: uuid::Uuid =
        sqlx::query_scalar("select id from roles where tenant_id = $1 and name = 'A rolü'")
            .bind(a.tenant_id)
            .fetch_one(a.pool())
            .await
            .unwrap();
    let key = create_key(&mut a, &csrf, "A anahtarı").await;
    let key_id: uuid::Uuid =
        sqlx::query_scalar("select id from api_keys where tenant_id = $1 and name = 'A anahtarı'")
            .bind(a.tenant_id)
            .fetch_one(a.pool())
            .await
            .unwrap();
    post(
        &mut a,
        "/admin/developer/webhooks",
        &csrf,
        &[
            ("url", "https://93.184.215.14/a"),
            ("events", "invoice.paid"),
        ],
    )
    .await;
    let hook: uuid::Uuid =
        sqlx::query_scalar("select id from webhook_endpoints where tenant_id = $1")
            .bind(a.tenant_id)
            .fetch_one(a.pool())
            .await
            .unwrap();

    // Tenant B's owner cannot reach any of them by id.
    let (_, csrf_b) = staff(&mut b, "owner").await;
    for p in [
        format!("/admin/staff/roles/{role}"),
        format!("/admin/staff/{a_staff}"),
        format!("/admin/developer/webhooks/{hook}"),
    ] {
        assert_eq!(b.get(&p).await.status, StatusCode::NOT_FOUND, "{p}");
    }
    for p in [
        format!("/admin/staff/roles/{role}/delete"),
        format!("/admin/staff/{a_staff}/deactivate"),
        format!("/admin/developer/keys/{key_id}/revoke"),
        format!("/admin/developer/webhooks/{hook}/delete"),
    ] {
        let r = post(&mut b, &p, &csrf_b, &[]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "POST {p}");
    }
    assert_eq!(user_status(&a, a_staff).await, AccountStatus::Active);
    assert_eq!(
        count(
            &a,
            "select count(*) from webhook_endpoints where tenant_id = $1"
        )
        .await,
        1
    );
    // A's key does not authenticate on B's host.
    let host_b = b.host.clone();
    assert_eq!(me(&mut b, &host_b, &key).await, StatusCode::UNAUTHORIZED);
    let host_a = a.host.clone();
    assert_eq!(me(&mut a, &host_a, &key).await, StatusCode::OK);
    a.cleanup().await;
    b.cleanup().await;
}
