//! Admin support / content / SEO / AI studio integration tests: ticket
//! replies vs. internal notes, permissions, publishing → storefront and
//! sitemap, seo_meta overrides, crawler policy → robots.txt, redirects, site
//! audits, the AI studio against a fake OpenAI-compatible provider and tenant
//! isolation.
//!
//! Like the other web tests they need the database from `.env`; without it
//! they print a notice and pass.

use std::time::Duration;

use crate::common::{Res, TestApp};
use axum::body::Body;
use hos_billing::registry::SaveIntegration;
use hos_core::domain::{IntegrationCategory, Locale, TicketPriority, TicketStatus, UserKind};
use hos_core::ids::{TicketId, UserId};
use hos_core::money::Currency;
use hos_db::enums::{AiGenerationStatus, AuthorKind, ContentStatus, MessageSource, RunStatus};
use hos_db::ids::{AiGenerationId, SeoAuditId};
use hos_db::repo::clients::{self, ClientInput, ClientRow};
use hos_db::repo::content::{self, BlogPostInput, BlogPostRow};
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::support::{self, NewTicket, NewTicketMessage, TicketRow};
use hos_db::repo::{billing_support, domains, seo};
use hos_jobs::{Worker, WorkerConfig};
use http::{Method, Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "Correct-Horse-Battery-9";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Creates a user of `kind` (staff: with role `role`).
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
    app.cookies.clear();
    let email = format!("{role}-{}@staff.test", Uuid::now_v7().simple());
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

/// A client with a login (`email`, [`PASSWORD`]).
async fn client_with_login(app: &TestApp) -> (ClientRow, String) {
    let email = format!("{}@client.test", Uuid::now_v7().simple());
    let uid = user(app, &email, UserKind::Client, None).await;
    let mut conn = app.pool().acquire().await.unwrap();
    let c = clients::create_client_with_owner(
        &mut conn,
        app.tenant_id,
        &ClientInput::individual("Ayşe", "Müşteri", &email, Currency::TRY),
        uid,
    )
    .await
    .expect("client");
    (c, email)
}

/// Sends a multipart form (text fields only) with the cookie jar.
async fn multipart(app: &mut TestApp, path: &str, fields: &[(&str, &str)]) -> Res {
    let boundary = "----hosAdminContentBoundary";
    let mut body = String::new();
    for (k, v) in fields {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    let cookie = app
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", app.host.clone())
        .header("cookie", cookie)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    Res {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A ticket of `client` (opened directly in the database).
async fn ticket(app: &TestApp, client: Option<&ClientRow>, subject: &str) -> TicketRow {
    let dept = support::list_departments(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("seeded department");
    let mut conn = app.pool().acquire().await.unwrap();
    let (t, _) = support::open_ticket(
        &mut conn,
        app.tenant_id,
        &NewTicket {
            department_id: dept.id,
            client_id: client.map(|c| c.id),
            user_id: None,
            guest_name: client.is_none().then(|| "Misafir".to_owned()),
            guest_email: client
                .is_none()
                .then(|| format!("{}@guest.test", Uuid::now_v7().simple())),
            subject: subject.into(),
            priority: TicketPriority::Medium,
            service_id: None,
            domain_id: None,
            first_message: NewTicketMessage {
                author_kind: AuthorKind::Client,
                user_id: None,
                author_name: "Ayşe Müşteri".into(),
                author_email: None,
                body: "Sitem açılmıyor, yardım eder misiniz?".into(),
                is_internal: false,
                source: MessageSource::Web,
                message_id: None,
                ip: None,
            },
        },
    )
    .await
    .expect("ticket");
    t
}

/// A blog post written directly to the database.
async fn blog_post(app: &TestApp, slug: &str, body: &str, status: ContentStatus) -> BlogPostRow {
    content::create_blog_post(
        app.pool(),
        app.tenant_id,
        &BlogPostInput {
            slug: slug.into(),
            locale: Locale::Tr,
            translation_group_id: None,
            title: "Alan adı nasıl seçilir".into(),
            excerpt: None,
            body: body.into(),
            cover_image_url: None,
            category: None,
            tags: Vec::new(),
            status,
            published_at: (status == ContentStatus::Published).then(chrono::Utc::now),
            reading_minutes: None,
            author_id: None,
            author_name: None,
        },
    )
    .await
    .expect("blog post")
}

/// `(body, status)` of a blog post row.
async fn post_state(app: &TestApp, id: Uuid) -> (String, ContentStatus) {
    sqlx::query_as::<_, (String, ContentStatus)>(
        "select body, status from blog_posts where tenant_id = $1 and id = $2",
    )
    .bind(app.tenant_id)
    .bind(id)
    .fetch_one(app.pool())
    .await
    .expect("blog post row")
}

/// Every sitemap document body (follows a sitemap index).
async fn sitemap_text(app: &mut TestApp) -> String {
    let root = app.get("/sitemap.xml").await;
    assert_eq!(root.status, StatusCode::OK);
    if !root.body.contains("<sitemapindex") {
        return root.body;
    }
    let mut all = String::new();
    let locs: Vec<String> = root
        .body
        .split("<loc>")
        .skip(1)
        .filter_map(|s| s.split("</loc>").next())
        .map(str::to_owned)
        .collect();
    for loc in locs {
        let path = loc
            .find("/sitemaps/")
            .map(|i| loc[i..].to_owned())
            .unwrap_or(loc);
        all.push_str(&app.get(&path).await.body);
    }
    all
}

/// Rules of the robots.txt group naming `ua`.
fn robots_group(txt: &str, ua: &str) -> Vec<String> {
    let mut groups: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut in_agents = false;
    for line in txt
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        if let Some(agent) = line.strip_prefix("User-agent: ") {
            if !in_agents {
                groups.push((Vec::new(), Vec::new()));
            }
            in_agents = true;
            groups.last_mut().unwrap().0.push(agent.to_owned());
        } else {
            in_agents = false;
            if line.starts_with("Sitemap:") {
                continue;
            }
            if let Some(g) = groups.last_mut() {
                g.1.push(line.to_owned());
            }
        }
    }
    groups
        .into_iter()
        .find(|(agents, _)| agents.iter().any(|a| a == ua))
        .map(|(_, rules)| rules)
        .unwrap_or_default()
}

/// The id at the end of a `Location` like `/admin/x/<uuid>`.
fn trailing_id(location: &str) -> Uuid {
    let last = location
        .split('?')
        .next()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap();
    Uuid::parse_str(last).unwrap_or_else(|_| panic!("no id in {location}"))
}

// ---------------------------------------------------------------------------
// Support
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ticket_reply_reaches_client_but_internal_note_does_not() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (client, client_email) = client_with_login(&app).await;
    let csrf = staff(&mut app, "support").await;

    // Staff opens a ticket on behalf of the client.
    let form = app.get("/admin/support/tickets/new").await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    let dept = support::list_departments(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .remove(0);
    let client_id = client.id.to_string();
    let dept_id = dept.id.to_string();
    let r = post(
        &mut app,
        "/admin/support/tickets/new",
        &csrf,
        &[
            ("client_id", &client_id),
            ("department", &dept_id),
            ("priority", "high"),
            ("subject", "Alan adı yönlendirmesi"),
            ("body", "Merhaba, talebinizi sizin adınıza açtık."),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let ticket_id = trailing_id(r.location());
    let url = format!("/admin/support/tickets/{ticket_id}");

    // The client's tickets tab links to the ticket page (it pointed to a
    // non-existent `/admin/tickets/…` before).
    let tab = app
        .get(&format!("/admin/clients/{client_id}/tickets"))
        .await;
    assert_eq!(tab.status, StatusCode::OK, "{}", tab.body);
    assert!(tab.body.contains(&format!("href=\"{url}\"")), "ticket link");

    // Queue / filters / detail render.
    let list = app
        .get("/admin/support/tickets?status=answered&priority=high&q=yönlendirme")
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert!(list.body.contains("Alan adı yönlendirmesi"));
    let detail = app.get(&url).await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    assert!(detail.body.contains("sizin adınıza"));

    // Public reply and internal note.
    let r = multipart(
        &mut app,
        &format!("{url}/reply"),
        &[
            ("csrf_token", &csrf),
            ("mode", "reply"),
            ("body", "Yönlendirmeyi **düzelttik**, kontrol eder misiniz?"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let r = multipart(
        &mut app,
        &format!("{url}/reply"),
        &[
            ("csrf_token", &csrf),
            ("mode", "note"),
            ("body", "GIZLI-NOT: DNS paneli şifresi müşteride."),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let detail = app.get(&url).await;
    assert!(detail.body.contains("GIZLI-NOT"), "staff sees the note");
    assert!(detail.body.contains("<strong>düzelttik</strong>"));

    // ticket_reply e-mail queued for the client.
    let (mails,): (i64,) = sqlx::query_as(
        "select count(*) from email_log where tenant_id = $1 and template_key = 'ticket_reply'",
    )
    .bind(app.tenant_id)
    .fetch_one(app.pool())
    .await
    .unwrap();
    assert!(mails >= 1, "ticket_reply e-mail queued");

    // Status change through the transition rules.
    let r = post(
        &mut app,
        &format!("{url}/update"),
        &csrf,
        &[("status", "on_hold")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let t = support::get_ticket(app.pool(), app.tenant_id, TicketId::from(ticket_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.status, TicketStatus::OnHold);

    // The client sees the reply, never the note.
    app.cookies.clear();
    login(&mut app, &client_email).await;
    let mine = app.get(&format!("/client/tickets/{ticket_id}")).await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.body);
    assert!(mine.body.contains("düzelttik"), "client sees the reply");
    assert!(!mine.body.contains("GIZLI-NOT"), "internal note hidden");
    assert!(!mine.body.contains("DNS paneli"), "internal note hidden");

    // Bulk close as staff.
    let csrf = staff(&mut app, "support").await;
    let id = ticket_id.to_string();
    let r = post(
        &mut app,
        "/admin/support/tickets/bulk",
        &csrf,
        &[("ids", &id), ("action", "close")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let t = support::get_ticket(app.pool(), app.tenant_id, TicketId::from(ticket_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.status, TicketStatus::Closed);
    app.cleanup().await;
}

#[tokio::test]
async fn support_role_cannot_reach_seo() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "support").await;
    let dash = app.get("/admin").await;
    assert!(dash.body.contains("href=\"/admin/support/tickets\""));
    assert!(
        !dash.body.contains("href=\"/admin/seo\""),
        "SEO hidden from the menu"
    );
    for path in [
        "/admin/seo",
        "/admin/seo/redirects",
        "/admin/seo/crawlers",
        "/admin/ai",
    ] {
        assert_eq!(
            app.get(path).await.status,
            StatusCode::FORBIDDEN,
            "support reached {path}"
        );
    }
    let r = post(
        &mut app,
        "/admin/seo/redirects",
        &csrf,
        &[
            ("from_path", "/x"),
            ("to_url", "/y"),
            ("status_code", "301"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // …while support pages work.
    for path in [
        "/admin/support/tickets",
        "/admin/support/kb",
        "/admin/support/canned",
        "/admin/support/departments",
        "/admin/content/posts",
    ] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.body);
    }
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publishing_reaches_storefront_and_sitemap_and_meta_overrides_title() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "admin").await;
    assert_eq!(
        app.get("/admin/content/posts/new").await.status,
        StatusCode::OK
    );
    let r = post(
        &mut app,
        "/admin/content/posts",
        &csrf,
        &[
            ("tr_title", "Alan adı seçim rehberi"),
            ("tr_slug", "alan-adi-secim-rehberi"),
            ("tr_status", "published"),
            ("tr_body", "Kısa ve akılda kalıcı bir **alan adı** seçin."),
            ("tr_meta_title", "Özel SEO Başlığı Deneme"),
            (
                "tr_meta_description",
                "Alan adı seçerken dikkat edilecekler.",
            ),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let editor = app.get(r.location()).await;
    assert_eq!(editor.status, StatusCode::OK, "{}", editor.body);
    assert!(editor.body.contains("alan-adi-secim-rehberi"));

    let public = app.get("/blog/alan-adi-secim-rehberi").await;
    assert_eq!(public.status, StatusCode::OK, "{}", public.body);
    let title = public
        .body
        .split("<title>")
        .nth(1)
        .and_then(|s| s.split("</title>").next())
        .unwrap_or_default();
    assert!(
        title.contains("Özel SEO Başlığı Deneme"),
        "seo_meta title overrides <title>: {title}"
    );
    assert!(public.body.contains("<strong>alan adı</strong>"));

    // A draft is not public and not in the sitemap.
    let r = post(
        &mut app,
        "/admin/content/posts",
        &csrf,
        &[
            ("tr_title", "Taslak yazı"),
            ("tr_slug", "taslak-yazi-deneme"),
            ("tr_status", "draft"),
            ("tr_body", "Henüz hazır değil."),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(
        app.get("/blog/taslak-yazi-deneme").await.status,
        StatusCode::NOT_FOUND
    );

    // Slug conflict is reported, not saved.
    let r = post(
        &mut app,
        "/admin/content/posts",
        &csrf,
        &[
            ("tr_title", "Kopya"),
            ("tr_slug", "alan-adi-secim-rehberi"),
            ("tr_status", "draft"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    let sitemap = sitemap_text(&mut app).await;
    assert!(
        sitemap.contains("/blog/alan-adi-secim-rehberi"),
        "published post in sitemap"
    );
    assert!(
        !sitemap.contains("taslak-yazi-deneme"),
        "draft not in sitemap"
    );
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// SEO
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crawler_policy_changes_robots_txt() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "admin").await;
    let page = app.get("/admin/seo/crawlers").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);

    // Training allowed → GPTBot is not blocked.
    let r = post(
        &mut app,
        "/admin/seo/crawlers",
        &csrf,
        &[
            ("allow_search", "1"),
            ("allow_user_fetch", "1"),
            ("allow_training", "1"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let robots = app.get("/robots.txt").await.body;
    assert!(
        !robots_group(&robots, "GPTBot").contains(&"Disallow: /".to_owned()),
        "{robots}"
    );

    // Training off (+ an extra disallowed path) → GPTBot disallowed.
    let r = post(
        &mut app,
        "/admin/seo/crawlers",
        &csrf,
        &[
            ("allow_search", "1"),
            ("allow_user_fetch", "1"),
            ("disallow", "/gizli-kampanya"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let robots = app.get("/robots.txt").await.body;
    assert_eq!(robots_group(&robots, "GPTBot"), vec!["Disallow: /"]);
    assert!(robots.contains("Disallow: /gizli-kampanya"), "{robots}");
    let stored = hos_db::repo::platform::get_setting(app.pool(), app.tenant_id, "seo.ai_crawlers")
        .await
        .unwrap()
        .expect("seo.ai_crawlers saved");
    assert_eq!(stored["allow_training"], json!(false));

    // The live preview renders the same file.
    let preview = post(
        &mut app,
        "/admin/seo/crawlers/preview",
        &csrf,
        &[("allow_search", "1")],
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert!(preview.body.contains("GPTBot"));
    app.cleanup().await;
}

#[tokio::test]
async fn redirect_loops_are_rejected_and_301_is_served() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "admin").await;
    let r = post(
        &mut app,
        "/admin/seo/redirects",
        &csrf,
        &[
            ("from_path", "/eski-kampanya"),
            ("to_url", "/yeni-kampanya"),
            ("status_code", "301"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);

    // B → A would loop.
    let r = post(
        &mut app,
        "/admin/seo/redirects",
        &csrf,
        &[
            ("from_path", "/yeni-kampanya"),
            ("to_url", "/eski-kampanya"),
            ("status_code", "301"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "loop rejected");
    // Invalid source paths are rejected too.
    let r = post(
        &mut app,
        "/admin/seo/redirects",
        &csrf,
        &[
            ("from_path", "eski sayfa"),
            ("to_url", "/x"),
            ("status_code", "301"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let all = content::list_redirects(app.pool(), app.tenant_id)
        .await
        .unwrap();
    assert_eq!(all.len(), 1);

    // The working redirect is served as a 301 and counted.
    let res = app.get("/eski-kampanya").await;
    assert_eq!(res.status, StatusCode::MOVED_PERMANENTLY);
    assert!(
        res.location().ends_with("/yeni-kampanya"),
        "{}",
        res.location()
    );
    let all = content::list_redirects(app.pool(), app.tenant_id)
        .await
        .unwrap();
    assert_eq!(all[0].hits, 1);
    app.cleanup().await;
}

#[tokio::test]
async fn seo_audit_runs_and_saves_its_report() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    blog_post(
        &app,
        "denetim-yazisi",
        "Denetim için kısa bir yazı.",
        ContentStatus::Published,
    )
    .await;
    let csrf = staff(&mut app, "admin").await;
    let r = post(&mut app, "/admin/seo/audits", &csrf, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = trailing_id(r.location());

    // The audit runs in a background task; wait for it.
    let mut row = None;
    for _ in 0..600 {
        let a = seo::get_seo_audit(app.pool(), app.tenant_id, SeoAuditId::from(id))
            .await
            .unwrap()
            .expect("audit row");
        if matches!(a.status, RunStatus::Completed | RunStatus::Failed) {
            row = Some(a);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let row = row.expect("audit finished in time");
    assert_eq!(row.status, RunStatus::Completed, "{:?}", row.error);
    assert!(row.score.is_some());
    assert!(row.pages_done > 0 && row.pages_done == row.pages_total);
    assert!(row.issues.get("site").is_some(), "site report stored");

    let report = app.get(&format!("/admin/seo/audits/{id}")).await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);
    assert!(report.body.contains("denetim-yazisi"), "page listed");
    let progress = app.get(&format!("/admin/seo/audits/{id}/progress")).await;
    assert_eq!(progress.status, StatusCode::OK);
    for path in ["/admin/seo", "/admin/seo/geo", "/admin/seo/sitemap"] {
        let r = app.get(path).await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.body);
    }
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// AI studio
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ai_studio_end_to_end_with_a_fake_provider() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let server = MockServer::start().await;
    let rewritten = "## Alan adı nasıl seçilir?\n\nKısa, akılda kalıcı ve markanızla uyumlu bir alan adı seçin. Uzantı seçimi de önemlidir.";
    let output = json!({
        "content_markdown": rewritten,
        "changes": [{"change": "Soru başlığı eklendi", "reason": "Yanıt motorları soru-cevap yapısını alıntılar"}]
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test", "model": "test-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": output.to_string()}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 120, "completion_tokens": 40}
        })))
        .mount(&server)
        .await;
    app.state
        .billing
        .registry()
        .save_integration(
            app.tenant_id,
            &SaveIntegration {
                id: None,
                category: IntegrationCategory::Ai,
                kind: "openai_compatible".into(),
                name: "Sahte AI".into(),
                config: json!({"base_url": format!("{}/v1", server.uri()), "model": "test-model", "max_retries": 0}),
                secrets: Some(json!({})),
                is_active: true,
                sort_order: 0,
            },
        )
        .await
        .expect("ai integration");
    let original =
        "Kısa, akılda kalıcı ve markanızla uyumlu bir alan adı seçin. Uzantı seçimi de önemlidir.";
    let post_row = blog_post(&app, "alan-adi-secimi", original, ContentStatus::Draft).await;

    let csrf = staff(&mut app, "admin").await;
    let studio = app.get("/admin/ai").await;
    assert_eq!(studio.status, StatusCode::OK, "{}", studio.body);
    assert!(!studio.body.contains("AI sağlayıcısı yapılandırılmamış"));
    let form = app.get("/admin/ai/new?task=geo_rewrite").await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    let entity = format!("blog_post:{}", post_row.id);
    assert!(form.body.contains(&entity), "post offered as target");

    let r = post(
        &mut app,
        "/admin/ai/generate",
        &csrf,
        &[("task", "geo_rewrite"), ("entity", &entity)],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let gid = trailing_id(r.location());
    let pending = app
        .get(&format!("/admin/ai/generations/{gid}/status"))
        .await;
    assert_eq!(pending.status, StatusCode::OK);

    Worker::new(
        app.pool().clone(),
        app.state.billing.clone(),
        WorkerConfig::default(),
    )
    .run_tenant_until_idle(app.tenant_id, 20)
    .await
    .unwrap();
    let g =
        billing_support::get_ai_generation(app.pool(), app.tenant_id, AiGenerationId::from(gid))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(g.status, AiGenerationStatus::Completed, "{:?}", g.error);
    // The model only got real content from the database.
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    let sent = String::from_utf8_lossy(&requests[0].body).into_owned();
    assert!(sent.contains("markanızla uyumlu"), "post body sent");

    let result = app.get(&format!("/admin/ai/generations/{gid}")).await;
    assert_eq!(result.status, StatusCode::OK, "{}", result.body);
    assert!(result.body.contains("Soru başlığı eklendi"));
    assert_eq!(post_state(&app, post_row.id.into()).await.0, original);

    let r = post(
        &mut app,
        &format!("/admin/ai/generations/{gid}/apply"),
        &result.csrf(),
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let (body, status) = post_state(&app, post_row.id.into()).await;
    assert_eq!(body, rewritten, "draft updated");
    assert_eq!(status, ContentStatus::Draft);
    let g =
        billing_support::get_ai_generation(app.pool(), app.tenant_id, AiGenerationId::from(gid))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(g.status, AiGenerationStatus::Applied);

    let history = app.get("/admin/ai/history").await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.body);
    app.cleanup().await;
}

#[tokio::test]
async fn ai_studio_without_provider_shows_empty_state() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // An environment-wide ANTHROPIC_API_KEY counts as a provider.
    if app
        .state
        .billing
        .registry()
        .ai(app.tenant_id)
        .await
        .unwrap()
        .is_some()
    {
        eprintln!("an AI provider is configured in the environment — skipping");
        app.cleanup().await;
        return;
    }
    let csrf = staff(&mut app, "admin").await;
    let studio = app.get("/admin/ai").await;
    assert_eq!(studio.status, StatusCode::OK, "{}", studio.body);
    assert!(studio.body.contains("AI sağlayıcısı yapılandırılmamış"));
    assert!(studio.body.contains("Entegrasyonlara git"));
    let r = post(
        &mut app,
        "/admin/ai/generate",
        &csrf,
        &[("task", "faq"), ("topic", "Yedekleme")],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let (n,): (i64,) = sqlx::query_as("select count(*) from ai_generations where tenant_id = $1")
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing queued");
    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn other_tenants_tickets_and_posts_are_404() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let Some(other) = TestApp::new().await else {
        return;
    };
    let foreign_ticket = ticket(&other, None, "Başka kiracı").await;
    let foreign_post = blog_post(
        &other,
        "baska-kiraci",
        "Başka kiracının yazısı.",
        ContentStatus::Published,
    )
    .await;
    let csrf = staff(&mut app, "admin").await;
    let t = format!("/admin/support/tickets/{}", foreign_ticket.id);
    assert_eq!(app.get(&t).await.status, StatusCode::NOT_FOUND);
    let r = multipart(
        &mut app,
        &format!("{t}/reply"),
        &[("csrf_token", &csrf), ("body", "x")],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let p = format!("/admin/content/posts/{}", foreign_post.translation_group_id);
    assert_eq!(app.get(&p).await.status, StatusCode::NOT_FOUND);
    let r = post(&mut app, &p, &csrf, &[("tr_title", "Ele geçir")]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let entity = format!("blog_post:{}", foreign_post.id);
    let r = post(
        &mut app,
        "/admin/ai/generate",
        &csrf,
        &[("task", "geo_rewrite"), ("entity", &entity)],
    )
    .await;
    assert_ne!(r.status, StatusCode::SEE_OTHER, "foreign target refused");
    let still = support::get_ticket(other.pool(), other.tenant_id, foreign_ticket.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.status, TicketStatus::Open);
    app.cleanup().await;
    other.cleanup().await;
}

async fn tld_active(app: &TestApp, id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("select is_active from tlds where tenant_id = $1 and id = $2")
        .bind(app.tenant_id)
        .bind(id)
        .fetch_one(app.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn tld_bulk_action_changes_only_the_tenants_selection() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let other = TestApp::new().await.unwrap();
    let mine = domains::upsert_tld(app.pool(), app.tenant_id, &domains::TldInput::new("com"))
        .await
        .expect("tld");
    let kept = domains::upsert_tld(app.pool(), app.tenant_id, &domains::TldInput::new("net"))
        .await
        .expect("tld");
    let foreign = domains::upsert_tld(
        other.pool(),
        other.tenant_id,
        &domains::TldInput::new("org"),
    )
    .await
    .expect("tld");
    let (mine_id, kept_id, foreign_id) = (
        *mine.id.as_uuid(),
        *kept.id.as_uuid(),
        *foreign.id.as_uuid(),
    );
    let (mine_s, foreign_s) = (mine_id.to_string(), foreign_id.to_string());

    // Without products.manage the action is refused.
    let csrf = staff(&mut app, "support").await;
    let r = post(
        &mut app,
        "/admin/catalog/tlds/bulk",
        &csrf,
        &[("ids", &mine_s), ("action", "deactivate")],
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(tld_active(&app, mine_id).await);

    // An unknown action and an empty selection change nothing.
    let csrf = staff(&mut app, "admin").await;
    for fields in [
        vec![("ids", mine_s.as_str()), ("action", "drop")],
        vec![("action", "deactivate")],
    ] {
        let r = post(&mut app, "/admin/catalog/tlds/bulk", &csrf, &fields).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        assert!(tld_active(&app, mine_id).await);
    }

    // The selection of this tenant is deactivated; another tenant's id in
    // the same request is ignored, unselected rows stay as they were.
    let r = post(
        &mut app,
        "/admin/catalog/tlds/bulk",
        &csrf,
        &[
            ("ids", &mine_s),
            ("ids", &foreign_s),
            ("action", "deactivate"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(!tld_active(&app, mine_id).await);
    assert!(tld_active(&app, kept_id).await);
    assert!(
        tld_active(&other, foreign_id).await,
        "other tenant untouched"
    );

    let r = post(
        &mut app,
        "/admin/catalog/tlds/bulk",
        &csrf,
        &[("ids", &mine_s), ("action", "activate")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert!(tld_active(&app, mine_id).await);
    app.cleanup().await;
    other.cleanup().await;
}
