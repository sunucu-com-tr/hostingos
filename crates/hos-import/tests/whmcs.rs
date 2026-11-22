//! End-to-end import tests against the workspace database with a WHMCS
//! fixture (no MySQL needed) and a mocked WHMCS API (wiremock).

mod common;
mod it;

use common::{Env, USER_PASSWORD, XSS, fixture};
use hos_core::ids::{ServiceId, TenantId};
use hos_db::repo::import as repo;
use hos_import::config::{ApiSourceConfig, ImportOptions, Mappings, SourceConfig, SourceSecrets};
use hos_import::engine::Progress;
use hos_import::entity::Entity;
use hos_import::source::api::ApiSource;
use hos_import::source::fixture::FixtureSource;
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

fn all_options() -> ImportOptions {
    ImportOptions {
        entities: Entity::ORDER.iter().copied().collect(),
        ..ImportOptions::default()
    }
}

async fn counts(env: &Env) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for t in [
        "clients",
        "users",
        "contacts",
        "client_notes",
        "credits",
        "products",
        "product_groups",
        "product_addons",
        "coupons",
        "tlds",
        "invoices",
        "invoice_items",
        "orders",
        "services",
        "service_addons",
        "domains",
        "transactions",
        "refunds",
        "tickets",
        "ticket_messages",
        "ticket_attachments",
        "canned_responses",
        "kb_categories",
        "kb_articles",
        "announcements",
        "integrations",
        "currencies",
        "tax_rules",
        "client_groups",
        "config_options",
        "quotes",
        "quote_items",
        "affiliates",
        "affiliate_referrals",
        "affiliate_commissions",
        "affiliate_payouts",
        "staff_tasks",
        "fraud_bans",
        "download_categories",
        "downloads",
        "status_incidents",
        "status_incident_updates",
    ] {
        out.push((t.to_owned(), env.count(t).await));
    }
    out
}

#[tokio::test]
async fn full_import_rerun_and_rollback() {
    let Some(env) = Env::new().await else { return };
    let before_currencies = env.count("currencies").await;
    let run = env.import(fixture(), all_options()).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);
    assert!(
        run.secrets_wiped_at.is_some(),
        "source credentials wiped after the run"
    );
    assert!(!run.has_secrets);

    // Clients: invalid e-mail skipped, XSS stored as data (escaped at render).
    assert_eq!(env.count("clients").await, 2);
    let first: String = env
        .scalar(
            "select first_name from clients where tenant_id = $1 and email = 'john@example.net'",
        )
        .await;
    assert_eq!(first, XSS.replace("&amp;", "&"));
    let company: String = env
        .scalar(
            "select company_name from clients where tenant_id = $1 and email = 'john@example.net'",
        )
        .await;
    assert_eq!(company, "Evil & Co", "entities decoded");
    let ayse_currency: String = env
        .scalar("select currency from clients where tenant_id = $1 and email = 'ayse@example.net'")
        .await;
    assert_eq!(ayse_currency, "TRY");
    let custom: serde_json::Value = env
        .scalar(
            "select custom_fields from clients where tenant_id = $1 and email = 'ayse@example.net'",
        )
        .await;
    assert_eq!(custom["TC Kimlik No"], json!("12345678901"));

    // Currencies: USD / TRY adopted, GBP created.
    assert_eq!(env.count("currencies").await, before_currencies + 1);

    // Logins: bcrypt kept verbatim, MD5 re-encoded as a reset-only legacy value.
    let ayse_hash: String = env
        .scalar(
            "select password_hash from users where tenant_id = $1 and email = 'ayse@example.net'",
        )
        .await;
    assert!(ayse_hash.starts_with("$2y$"));
    assert_eq!(
        hos_auth::legacy::verify_any(USER_PASSWORD, Some(&ayse_hash)),
        hos_auth::legacy::Verification::Valid
    );
    let john_hash: String = env
        .scalar(
            "select password_hash from users where tenant_id = $1 and email = 'john@example.net'",
        )
        .await;
    assert!(john_hash.starts_with("$hos-legacy$whmcs-md5$"));
    let staff_pw: Option<String> = env.scalar("select password_hash from users where tenant_id = $1 and email = 'ada.staff@example.net'").await;
    assert_eq!(staff_pw, None, "staff passwords are never imported");
    let memberships: i64 = env
        .scalar("select count(*) from client_users where tenant_id = $1")
        .await;
    assert_eq!(memberships, 3);
    let perms: Vec<String> = env
        .scalar("select cu.permissions from client_users cu join users u on u.id = cu.user_id where cu.tenant_id = $1 and u.email = 'john@example.net' and not cu.is_owner")
        .await;
    assert_eq!(perms, vec!["profile", "invoices", "tickets"]);

    // Credit balance: WHMCS balance 25 = 50 − 20 + opening adjustment −5.
    let balance: Decimal = env
        .scalar("select coalesce(sum(amount), 0) from credits where tenant_id = $1")
        .await;
    assert_eq!(balance, Decimal::new(25, 0));

    // Service: decrypted password re-sealed for HostingOS, grace period set.
    let (sid, sealed, override_until): (Uuid, Option<Vec<u8>>, Option<chrono::NaiveDate>) = sqlx::query_as(
        "select id, password_enc, override_suspend_until from services where tenant_id = $1 and username = 'ayse'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    let opened = hos_billing::secrets::open(
        &env.secret_box,
        &sealed.expect("password imported"),
        &hos_billing::secrets::service_password_aad(env.tenant, ServiceId::from_uuid(sid)),
    )
    .unwrap();
    assert_eq!(opened.expose_str(), "hunter2");
    assert!(override_until.is_some_and(|d| d >= chrono::Utc::now().date_naive()));
    assert_eq!(
        env.count("services").await,
        1,
        "service with unknown product skipped"
    );
    let sel: serde_json::Value = env
        .scalar("select config_selections from services where tenant_id = $1")
        .await;
    let (_, v) = sel
        .as_object()
        .unwrap()
        .iter()
        .next()
        .expect("config option selected");
    assert_eq!(v["quantity"], json!(5));

    // Invoices: numbers kept, sequence continues, unpaid invoice protected.
    let numbers: Vec<String> = env
        .scalar("select array_agg(number order by number) from invoices where tenant_id = $1")
        .await;
    assert_eq!(numbers, vec!["100", "101"]);
    let next: i64 = env
        .scalar("select next_value from invoice_sequences where tenant_id = $1")
        .await;
    assert!(next > 101);
    let late_fee_guard: i64 = env.scalar("select count(*) from invoices where tenant_id = $1 and status = 'unpaid' and late_fee_applied_at is not null").await;
    assert_eq!(late_fee_guard, 1);
    let period: Option<chrono::NaiveDate> = env.scalar("select period_start from invoice_items where tenant_id = $1 and description = 'Starter renewal'").await;
    assert_eq!(period, chrono::NaiveDate::from_ymd_opt(2099, 1, 8));
    assert_eq!(env.count("refunds").await, 1);

    // Tickets: HTML converted, script dropped, only the allowed attachment kept.
    let body: String = env.scalar("select body from ticket_messages where tenant_id = $1 and author_kind = 'client' limit 1").await;
    assert!(!body.contains("<script"), "{body}");
    assert!(!body.to_ascii_lowercase().contains("onerror"), "{body}");
    assert!(body.contains("Hello"));
    assert_eq!(env.count("ticket_attachments").await, 1);
    let internal: i64 = env
        .scalar("select count(*) from ticket_messages where tenant_id = $1 and is_internal")
        .await;
    assert_eq!(internal, 1);
    let kb: String = env
        .scalar("select body from kb_articles where tenant_id = $1")
        .await;
    assert!(!kb.contains("javascript:"), "{kb}");

    // Silent: no jobs, mails or webhooks; automation paused; audit written.
    assert_eq!(env.count("jobs").await, 0);
    assert_eq!(env.count("email_log").await, 0);
    assert!(
        !repo::automation_active(&env.pool, env.tenant)
            .await
            .unwrap()
    );
    let audit: i64 = env
        .scalar(
            "select count(*) from audit_log where tenant_id = $1 and action = 'import.completed'",
        )
        .await;
    assert_eq!(audit, 1);
    let integration_active: bool = env
        .scalar("select is_active from integrations where tenant_id = $1 and kind = 'cpanel'")
        .await;
    assert!(!integration_active, "imported integrations start inactive");

    // Report: skipped rows and the unsupported tables are listed.
    let msgs = repo::list_messages(&env.pool, env.tenant, run.id, None, 1000)
        .await
        .unwrap();
    assert!(msgs.iter().any(|m| m.code == "invalid_email"));
    assert!(msgs.iter().any(|m| m.code == "product_missing"));
    assert!(msgs.iter().any(|m| m.code == "attachment_type"));
    assert!(
        msgs.iter()
            .any(|m| m.code == "unsupported" && m.entity == "billable_items")
    );
    assert!(
        !msgs
            .iter()
            .any(|m| m.code == "unsupported" && m.entity == "quotes"),
        "quotes are imported now"
    );
    for code in [
        "no_recipient",
        "not_a_commission",
        "no_referral",
        "balance_not_imported",
        "expired",
        "invalid",
        "download_files_missing",
    ] {
        assert!(msgs.iter().any(|m| m.code == code), "message {code}");
    }

    // Quotes: a client's and a prospect's; numbers, statuses and lines.
    assert_eq!(env.count("quotes").await, 2);
    let (status, total, currency, notes): (String, Decimal, String, Option<String>) = sqlx::query_as(
        "select status, total, currency, notes from quotes where tenant_id = $1 and client_id is not null",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(status, "sent");
    assert_eq!(total, Decimal::new(13463, 2));
    assert_eq!(currency, "TRY", "the client's currency");
    assert_eq!(notes.as_deref(), Some("Our offer\n\nThanks!"));
    let (pstatus, pemail): (String, String) = sqlx::query_as(
        "select status, prospect_email::text from quotes where tenant_id = $1 and client_id is null",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        (pstatus.as_str(), pemail.as_str()),
        ("accepted", "prospect@example.org")
    );
    let seqs: Vec<i64> =
        sqlx::query_scalar("select seq from quotes where tenant_id = $1 order by seq")
            .bind(env.tenant)
            .fetch_all(&env.pool)
            .await
            .unwrap();
    assert_eq!(seqs, vec![1, 2]);
    assert_eq!(env.count("quote_items").await, 3);
    let line: Decimal = env
        .scalar(
            "select amount from quote_items where tenant_id = $1 and description = 'Mailbox move'",
        )
        .await;
    assert_eq!(
        line,
        Decimal::new(1463, 2),
        "3 × 5.00 − 2.5 %, rounded half up"
    );

    // Affiliates: overrides, one referral for two services, balance = WHMCS.
    assert_eq!(env.count("affiliates").await, 2);
    let (ctype, cvalue, recurring): (Option<String>, Option<Decimal>, Option<bool>) = sqlx::query_as(
        "select a.commission_type, a.commission_value, a.commission_recurring from affiliates a
           join clients c on c.id = a.client_id where a.tenant_id = $1 and c.email = 'john@example.net'",
    )
    .bind(env.tenant)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(ctype.as_deref(), Some("percent"));
    assert_eq!(cvalue, Some(Decimal::new(15, 0)));
    assert_eq!(recurring, Some(false));
    assert_eq!(env.count("affiliate_referrals").await, 1);
    let available: Decimal = env
        .scalar(
            "select coalesce(sum(m.amount), 0) from affiliate_commissions m join affiliates a on a.id = m.affiliate_id
               join clients c on c.id = a.client_id
              where m.tenant_id = $1 and c.email = 'john@example.net' and m.status = 'available'",
        )
        .await;
    assert_eq!(available, Decimal::new(30, 0), "the WHMCS balance");
    assert_eq!(
        env.count("affiliate_commissions").await,
        3,
        "two history lines + opening balance"
    );
    let paid: Decimal = env
        .scalar("select coalesce(sum(amount), 0) from affiliate_payouts where tenant_id = $1 and status = 'paid'")
        .await;
    assert_eq!(paid, Decimal::new(10, 0));

    // To-dos, bans, downloads, network issues.
    assert_eq!(env.count("staff_tasks").await, 2);
    let assigned: i64 = env
        .scalar("select count(*) from staff_tasks where tenant_id = $1 and assigned_to is not null and status = 'open'")
        .await;
    assert_eq!(assigned, 1);
    let bans: Vec<String> = sqlx::query_scalar(
        "select kind || ':' || value from fraud_bans where tenant_id = $1 order by 1",
    )
    .bind(env.tenant)
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        bans,
        vec![
            "domain:junk.example",
            "domain:spam.example",
            "ip:198.51.100.0/24",
            "ip:203.0.113.9"
        ]
    );
    let downloads: Vec<(String, String, String)> = sqlx::query_as(
        "select filename, access, content_type from downloads where tenant_id = $1 order by title",
    )
    .bind(env.tenant)
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        downloads,
        vec![
            ("tool.zip".into(), "public".into(), "application/zip".into()),
            (
                "setup-guide.pdf".into(),
                "clients".into(),
                "application/pdf".into()
            ),
        ]
    );
    assert_eq!(
        env.count("download_files").await,
        0,
        "files are uploaded again"
    );
    let incidents: Vec<(String, String, String)> = sqlx::query_as(
        "select kind, status, impact from status_incidents where tenant_id = $1 order by started_at",
    )
    .bind(env.tenant)
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        incidents,
        vec![
            ("incident".into(), "resolved".into(), "major".into()),
            ("maintenance".into(), "scheduled".into(), "none".into()),
        ]
    );
    let body: String = env
        .scalar("select body from status_incident_updates u join status_incidents i on i.id = u.incident_id where u.tenant_id = $1 and i.title = 'Web1 outage'")
        .await;
    assert!(
        body.contains("Disk failure") && body.contains("Server: web1"),
        "{body}"
    );
    assert!(msgs.iter().any(|m| m.code == "weak_password_hash"));
    assert!(
        !msgs
            .iter()
            .any(|m| m.message.contains("hunter2") || m.message.contains("$2y$")),
        "no secrets in the report"
    );
    let csv = hos_import::report::csv_report(&env.pool, env.tenant, &run)
        .await
        .unwrap();
    assert!(csv.contains("invalid_email"));

    // Idempotent re-run: nothing new.
    let before = counts(&env).await;
    let rerun = env.import(fixture(), all_options()).await;
    assert_eq!(rerun.status, "completed", "error: {:?}", rerun.error);
    assert_eq!(counts(&env).await, before);
    let p = Progress::from_json(&rerun.progress);
    assert_eq!(p.entities["clients"].created, 0);
    assert_eq!(p.entities["clients"].updated, 2);
    let balance: Decimal = env
        .scalar("select coalesce(sum(amount), 0) from credits where tenant_id = $1")
        .await;
    assert_eq!(balance, Decimal::new(25, 0), "re-run keeps the balance");

    // Rollback of the first run is blocked by a row created outside it…
    let client: Uuid = env
        .scalar("select id from clients where tenant_id = $1 and email = 'ayse@example.net'")
        .await;
    sqlx::query("insert into client_notes (id, tenant_id, client_id, body) values ($1, $2, $3, 'added by staff')")
        .bind(Uuid::now_v7())
        .bind(env.tenant)
        .bind(client)
        .execute(&env.pool)
        .await
        .unwrap();
    let check = hos_import::rollback::check(&env.pool, env.tenant, run.id)
        .await
        .unwrap();
    assert!(check.blocked.iter().any(|b| b.table == "client_notes"));
    let after = env
        .runner()
        .rollback_now(env.tenant, run.id)
        .await
        .unwrap()
        .expect("rollback queued");
    assert_eq!(after.status, "completed");
    assert!(after.error.unwrap_or_default().contains("rollback blocked"));
    assert_eq!(env.count("clients").await, 2);

    // …and runs once it is gone.
    sqlx::query("delete from client_notes where tenant_id = $1 and body = 'added by staff'")
        .bind(env.tenant)
        .execute(&env.pool)
        .await
        .unwrap();
    let after = env
        .runner()
        .rollback_now(env.tenant, run.id)
        .await
        .unwrap()
        .expect("rollback queued");
    assert_eq!(after.status, "rolled_back", "error: {:?}", after.error);
    for t in [
        "clients",
        "users_client",
        "invoices",
        "services",
        "tickets",
        "domains",
        "products",
        "credits",
        "quotes",
        "affiliates",
        "affiliate_commissions",
        "staff_tasks",
        "fraud_bans",
        "downloads",
        "status_incidents",
    ] {
        if t == "users_client" {
            let n: i64 = env
                .scalar("select count(*) from users where tenant_id = $1 and kind = 'client'")
                .await;
            assert_eq!(n, 0);
            continue;
        }
        assert_eq!(env.count(t).await, 0, "{t} rolled back");
    }
    assert_eq!(
        env.count("currencies").await,
        before_currencies,
        "adopted currencies stay"
    );
    assert!(
        repo::automation_active(&env.pool, env.tenant)
            .await
            .unwrap(),
        "automation restored"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn dry_run_writes_nothing_and_reports_conflicts() {
    let Some(env) = Env::new().await else { return };
    let before = counts(&env).await;
    let id = env
        .draft(
            &common::db_config(),
            &all_options(),
            &Mappings::default(),
            &common::secrets(),
        )
        .await;
    let run = env.run(id, "dry_run", Box::new(fixture())).await;
    assert_eq!(run.status, "draft", "error: {:?}", run.error);
    assert!(
        run.has_secrets,
        "credentials kept for the import that follows"
    );
    assert!(run.dry_run_at.is_some());
    assert_eq!(counts(&env).await, before, "a dry run writes nothing");
    assert_eq!(env.count("import_id_map").await, 0);
    assert!(
        repo::automation_active(&env.pool, env.tenant)
            .await
            .unwrap()
    );
    let a = &run.preview["analysis"];
    assert!(a["gateways"]["banktransfer"]["count"].as_u64().unwrap() >= 3);
    assert_eq!(a["registrars"]["enom"]["count"], json!(2));
    assert!(
        a["servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["module"] == "autorelease" && s["kind"].is_null())
    );
    assert_eq!(a["roles"]["1"], json!("Full Administrator"));
    let codes: Vec<&str> = a["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["code"].as_str())
        .collect();
    assert!(codes.contains(&"gateway_unmapped"), "{codes:?}");
    assert!(codes.contains(&"server_type_unknown"), "{codes:?}");
    let p = Progress::from_json(&run.progress);
    assert_eq!(p.entities["clients"].read, 3);
    assert_eq!(
        p.entities["clients"].skipped + p.entities["clients"].errors,
        1,
        "the invalid client would be skipped"
    );
    env.cleanup().await;
}

#[tokio::test]
async fn tenants_are_isolated() {
    let Some(a) = Env::new().await else { return };
    let Some(b) = Env::new().await else { return };
    let ra = a.import(fixture(), all_options()).await;
    assert_eq!(ra.status, "completed", "{:?}", ra.error);
    assert_eq!(b.count("clients").await, 0);
    // Same source into another tenant: its own rows, tenant A untouched.
    let rb = b.import(fixture(), all_options()).await;
    assert_eq!(rb.status, "completed", "{:?}", rb.error);
    assert_eq!(a.count("clients").await, 2);
    assert_eq!(b.count("clients").await, 2);
    let shared: i64 = sqlx::query_scalar(
        "select count(*) from clients x join clients y on x.id = y.id where x.tenant_id = $1 and y.tenant_id = $2",
    )
    .bind(a.tenant)
    .bind(b.tenant)
    .fetch_one(&a.pool)
    .await
    .unwrap();
    assert_eq!(shared, 0);
    let foreign: Option<_> = repo::get_run(&a.pool, b.tenant, ra.id).await.unwrap();
    assert!(foreign.is_none(), "runs are tenant scoped");
    a.cleanup().await;
    b.cleanup().await;
}

#[tokio::test]
async fn cancelled_run_stops_and_resumes_nothing() {
    let Some(env) = Env::new().await else { return };
    let id = env
        .draft(
            &common::db_config(),
            &all_options(),
            &Mappings::default(),
            &common::secrets(),
        )
        .await;
    // Queued and cancelled in one transaction: a worker of a server on the
    // same database never sees the run queued.
    let mut tx = env.pool.begin().await.unwrap();
    assert!(
        repo::queue_run(&mut *tx, env.tenant, id, "import", None)
            .await
            .unwrap()
    );
    assert!(
        repo::request_cancel(&mut *tx, env.tenant, id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let run = repo::get_run(&env.pool, env.tenant, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "cancelled");
    assert!(!run.has_secrets);
    assert_eq!(env.count("clients").await, 0);
    env.cleanup().await;
}

#[tokio::test]
async fn source_failure_marks_the_run_failed_and_wipes_secrets() {
    let Some(env) = Env::new().await else { return };
    let mut src: FixtureSource = fixture();
    src.fail_on = Some(Entity::Invoices);
    let run = env.import(src, all_options()).await;
    assert_eq!(run.status, "failed");
    assert!(run.error.unwrap_or_default().contains("fixture failure"));
    assert!(!run.has_secrets);
    // Rows before the failure stay (resumable, idempotent); later ones are absent.
    assert_eq!(env.count("clients").await, 2);
    assert_eq!(env.count("invoices").await, 0);
    env.cleanup().await;
}

async fn mock_api(server: &wiremock::MockServer) {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, ResponseTemplate};
    let ok = |v: serde_json::Value| ResponseTemplate::new(200).set_body_json(v);
    let routes: Vec<(&str, serde_json::Value)> = vec![
        (
            "GetCurrencies",
            json!({"result": "success", "totalresults": 1, "currencies": {"currency": [
            {"id": 1, "code": "USD", "prefix": "$", "suffix": " USD", "format": 1, "rate": "1.00000"}]}}),
        ),
        (
            "GetClients",
            json!({"result": "success", "totalresults": 1, "numreturned": 1, "clients": {"client": [
            {"id": 7, "firstname": "Api", "lastname": "Client", "email": "api.client@example.net", "datecreated": "2022-01-01", "groupid": 0, "status": "Active"}]}}),
        ),
        (
            "GetClientsDetails",
            json!({"result": "success", "client": {"id": 7, "firstname": "Api", "lastname": "Client",
            "email": "api.client@example.net", "countrycode": "DE", "currency": 1, "status": "Active", "credit": "0.00",
            "customfields": [{"id": 4, "value": "abc"}]}}),
        ),
        (
            "GetInvoices",
            json!({"result": "success", "totalresults": 1, "numreturned": 1, "invoices": {"invoice": [
            {"id": 500, "userid": 7, "invoicenum": "", "date": "2024-05-01", "duedate": "2024-05-10", "datepaid": "0000-00-00 00:00:00",
             "subtotal": "10.00", "credit": "0.00", "tax": "0.00", "tax2": "0.00", "total": "10.00", "status": "Unpaid",
             "paymentmethod": "paypal", "currencycode": "USD"}]}}),
        ),
        (
            "GetSupportDepartments",
            json!({"result": "success", "departments": {"department": [{"id": 3, "name": "Api Support"}]}}),
        ),
        (
            "GetTickets",
            json!({"result": "success", "totalresults": 1, "numreturned": 1, "tickets": {"ticket": [
            {"id": 9, "tid": "API-1", "deptid": 3, "userid": 7, "name": "Api Client", "email": "api.client@example.net",
             "date": "2024-05-02 10:00:00", "subject": "x", "title": "From the API", "status": "Open", "priority": "Low", "lastreply": "2024-05-02 10:00:00"}]}}),
        ),
        (
            "GetTicket",
            json!({"result": "success", "ticketid": 9, "replies": {"reply": [
            {"replyid": 0, "name": "Api Client", "date": "2024-05-02 10:00:00", "message": "<b>Opening</b> message", "admin": "", "attachment": ""}]},
            "notes": ""}),
        ),
    ];
    for (action, body) in routes {
        Mock::given(method("POST"))
            .and(body_string_contains(format!("action={action}&")))
            .respond_with(ok(body))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn api_connector_imports_through_mocked_whmcs() {
    let Some(env) = Env::new().await else { return };
    let server = wiremock::MockServer::start().await;
    mock_api(&server).await;
    let config = SourceConfig::WhmcsApi(ApiSourceConfig {
        url: server.uri(),
        identifier: "ident".into(),
    });
    let secrets = SourceSecrets {
        api_secret: Some("api-secret".into()),
        ..SourceSecrets::default()
    };
    let options = ImportOptions {
        entities: [
            Entity::Currencies,
            Entity::Clients,
            Entity::Invoices,
            Entity::Departments,
            Entity::Tickets,
        ]
        .into_iter()
        .collect(),
        ..ImportOptions::default()
    };
    let id = env
        .draft(&config, &options, &Mappings::default(), &secrets)
        .await;
    let source = ApiSource::new(
        match &config {
            SourceConfig::WhmcsApi(c) => c,
            SourceConfig::WhmcsDb(_) => unreachable!(),
        },
        &secrets,
        hos_core::net::OutboundPolicy::Unrestricted,
    )
    .unwrap();
    let run = env.run(id, "import", Box::new(source)).await;
    assert_eq!(run.status, "completed", "error: {:?}", run.error);
    let country: String = env.scalar("select country_code from clients where tenant_id = $1 and email = 'api.client@example.net'").await;
    assert_eq!(country, "DE");
    let pw: Option<String> = env.scalar("select password_hash from users where tenant_id = $1 and email = 'api.client@example.net'").await;
    assert_eq!(pw, None, "the API exposes no password hashes");
    assert_eq!(env.count("invoices").await, 1);
    let body: String = env
        .scalar("select body from ticket_messages where tenant_id = $1")
        .await;
    assert!(body.contains("**Opening**"), "{body}");
    // Every request carried the credentials in the POST body, never the URL.
    for req in server.received_requests().await.unwrap() {
        assert!(!req.url.as_str().contains("secret"));
        assert!(String::from_utf8_lossy(&req.body).contains("secret=api-secret"));
    }
    let msgs = repo::list_messages(&env.pool, env.tenant, run.id, None, 1000)
        .await
        .unwrap();
    assert!(
        msgs.iter()
            .any(|m| m.code == "partial" && m.entity == "clients")
    );
    env.cleanup().await;
}

#[test]
fn tenant_ids_are_distinct_types() {
    // Compile-time guard used by the harness: TenantId is not a bare Uuid.
    let t = TenantId::new();
    assert_ne!(t.into_uuid(), Uuid::nil());
}
