//! Shared helpers for the hos-web integration tests.
//!
//! The tests drive the real router with `tower::ServiceExt::oneshot` against
//! the database configured in the workspace `.env`. Without a configuration
//! they print a notice and pass. Every test creates its own tenant (random
//! slug, reached through `Host: <slug>.<HOS_BASE_DOMAIN>`) and deletes it at the
//! end; the `Drop` impl cleans up after a panicking test too.

#![allow(dead_code)]

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use hos_core::config::AppConfig;
use hos_core::domain::TenantStatus;
use hos_core::ids::TenantId;
use hos_core::net::OutboundPolicy;
use hos_db::repo::platform;
use hos_db::{DbConfig, PgPool};
use hos_web::AppState;
use hos_web::config::WebConfig;
use http::{HeaderMap, Method, Request, StatusCode};
use tokio::sync::OnceCell;
use tower::ServiceExt;

static MIGRATED: OnceCell<()> = OnceCell::const_new();

/// A response with its body read.
pub struct Res {
    /// Status.
    pub status: StatusCode,
    /// Headers.
    pub headers: HeaderMap,
    /// Body as text.
    pub body: String,
}

impl Res {
    /// `Location` header.
    pub fn location(&self) -> &str {
        self.headers
            .get(http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }

    /// `Content-Type` header.
    pub fn content_type(&self) -> &str {
        self.headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }

    /// The CSRF token rendered into the page (hidden field or meta tag).
    pub fn csrf(&self) -> String {
        for marker in [
            "name=\"csrf_token\" value=\"",
            "name=\"csrf-token\" content=\"",
        ] {
            if let Some(start) = self.body.find(marker) {
                let rest = &self.body[start + marker.len()..];
                if let Some(end) = rest.find('"') {
                    return rest[..end].to_owned();
                }
            }
        }
        String::new()
    }

    /// Every `<script type="application/ld+json">` block, parsed.
    pub fn json_ld(&self) -> Vec<serde_json::Value> {
        let marker = "<script type=\"application/ld+json\">";
        let mut out = Vec::new();
        let mut rest = self.body.as_str();
        while let Some(start) = rest.find(marker) {
            rest = &rest[start + marker.len()..];
            let end = rest.find("</script>").expect("closed json-ld script");
            out.push(serde_json::from_str(&rest[..end]).expect("valid json-ld"));
            rest = &rest[end..];
        }
        out
    }
}

/// A throw-away tenant plus the application router.
pub struct TestApp {
    pub state: AppState,
    pub router: Router,
    pub tenant_id: TenantId,
    pub slug: String,
    pub host: String,
    pool: PgPool,
    cleaned: bool,
    /// Cookie jar (`name → value`).
    pub cookies: BTreeMap<String, String>,
    /// Demo-mode apps: the throw-away platform operator to remove.
    demo_platform_email: Option<String>,
}

/// Process-wide test setup that must happen before anything builds an
/// integration: the tests run providers against wiremock servers on
/// 127.0.0.1, which the default outbound policy (`private`) refuses.
pub fn outbound_policy_for_tests() {
    if let Err(active) = OutboundPolicy::set_global(OutboundPolicy::Unrestricted) {
        assert_eq!(
            active,
            OutboundPolicy::Unrestricted,
            "the outbound policy was read before the test harness set it"
        );
    }
    // A `hostingos serve` on the same database must not claim (and fail)
    // the jobs of the test tenants: they live in the `tests.` queue namespace.
    hos_db::repo::jobs::isolate_queue("tests");
}

/// [`AppState::new`] plus the explicit opt-in of the tests to the `mock`
/// integrations (hos-billing refuses them otherwise).
///
/// The tests share one database with many companies, so they run in
/// multi-company mode unless a single company is named (`web.company`,
/// [`TestApp::single`], [`TestApp::demo`]).
pub fn new_state(pool: PgPool, config: AppConfig, mut web: WebConfig) -> AppState {
    if web.company.is_none() {
        web.mode = hos_web::config::AppMode::Multi;
    }
    let state = AppState::new(pool, config, web).expect("state");
    state.billing.registry().set_mock_integrations_allowed(true);
    state
}

fn config() -> Option<AppConfig> {
    outbound_policy_for_tests();
    hos_web::config::load_dotenv();
    match AppConfig::from_env() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("configuration not available ({e}) — skipping web integration test");
            None
        }
    }
}

impl TestApp {
    /// Connects, migrates once, creates a tenant. `None` without a database.
    pub async fn new() -> Option<Self> {
        Self::with_mode(hos_web::config::AppMode::Multi).await
    }

    /// A single-company app (`HOS_MODE=single`) whose one company is this
    /// test's throw-away tenant: every host shows it, `/platform` is 404,
    /// the owner has `/admin/server`.
    pub async fn single() -> Option<Self> {
        Self::with_mode(hos_web::config::AppMode::Single).await
    }

    async fn with_mode(mode: hos_web::config::AppMode) -> Option<Self> {
        let config = config()?;
        MIGRATED
            .get_or_init(|| async {
                let pool = hos_db::connect(&DbConfig::for_migrations(&config))
                    .await
                    .expect("connect for migrations");
                hos_db::migrate(&pool).await.expect("migrate");
                pool.close().await;
            })
            .await;
        let pool = hos_db::connect(
            &DbConfig::from_app_config(&config)
                .with_max_connections(4)
                .with_application_name("hos-web-tests"),
        )
        .await
        .expect("connect");
        let slug = format!(
            "webtest-{}",
            &uuid::Uuid::now_v7().simple().to_string()[20..]
        );
        let mut new = platform::NewTenant::new(&slug, "Web Test Hosting");
        new.status = TenantStatus::Active;
        let row = platform::create_tenant(&pool, &new)
            .await
            .expect("create tenant");
        let mut conn = pool.acquire().await.expect("acquire");
        hos_db::seed::seed_tenant_defaults(&mut conn, row.id)
            .await
            .expect("seed tenant defaults");
        drop(conn);
        let host = format!("{slug}.{}", config.base_domain);
        let mut web = WebConfig::from_env(&config);
        web.mode = mode;
        web.company = (mode == hos_web::config::AppMode::Single).then(|| slug.clone());
        let state = new_state(pool.clone(), config, web);
        let router = hos_web::app(state.clone());
        let operator = (mode == hos_web::config::AppMode::Single)
            .then(|| hos_web::mode::operator_email(&slug));
        Some(Self {
            state,
            router,
            tenant_id: row.id,
            slug,
            host,
            pool,
            cleaned: false,
            cookies: BTreeMap::new(),
            demo_platform_email: operator,
        })
    }

    /// An app in public demo mode (`HOS_DEMO_MODE`) on a throw-away demo
    /// tenant: the shared database is never put into demo mode, and the
    /// `demo` tenant and `platform@demo.test` of `seed-demo` are never
    /// touched. The tenant is created by the demo reset itself.
    pub async fn demo() -> Option<Self> {
        Self::demo_with(false).await
    }

    /// [`demo`](Self::demo) deployed the way the public demo is: one public
    /// host (`HOS_BASE_URL=https://<slug>-public.test`, no wildcard DNS) that
    /// serves the demo tenant and the platform console. `host` is that host.
    pub async fn demo_single_host() -> Option<Self> {
        Self::demo_with(true).await
    }

    async fn demo_with(single_host: bool) -> Option<Self> {
        let mut config = config()?;
        MIGRATED
            .get_or_init(|| async {
                let pool = hos_db::connect(&DbConfig::for_migrations(&config))
                    .await
                    .expect("connect for migrations");
                hos_db::migrate(&pool).await.expect("migrate");
                pool.close().await;
            })
            .await;
        let pool = hos_db::connect(
            &DbConfig::from_app_config(&config)
                .with_max_connections(4)
                .with_application_name("hos-web-tests"),
        )
        .await
        .expect("connect");
        let slug = format!(
            "demotest-{}",
            &uuid::Uuid::now_v7().simple().to_string()[20..]
        );
        let mut demo = hos_web::demo::DemoConfig::standard();
        demo.tenant_slug = slug.clone();
        demo.platform.email = format!("platform-{slug}@demo.test");
        let platform_email = demo.platform.email.clone();
        let host = if single_host {
            let host = format!("{slug}-public.test");
            config.base_url = format!("https://{host}").parse().expect("base url");
            host
        } else {
            format!("{slug}.{}", config.base_domain)
        };
        let mut web = WebConfig::from_env(&config);
        // Demo mode never runs together with debug mode (`demo::start`).
        web.debug = false;
        web.demo = Some(demo);
        web.mode = hos_web::config::AppMode::Single;
        web.company = Some(slug.clone());
        web.mock_integrations = hos_web::config::MockIntegrations::AllowedByDemo;
        let state = new_state(pool.clone(), config, web);
        let report = hos_web::demo::reset(&state).await.expect("demo reset");
        let router = hos_web::app(state.clone());
        Some(Self {
            state,
            router,
            tenant_id: report.tenant_id,
            slug,
            host,
            pool,
            cleaned: false,
            cookies: BTreeMap::new(),
            demo_platform_email: Some(platform_email),
        })
    }

    /// The pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Demo apps: the throw-away platform operator's e-mail.
    pub fn demo_platform_email(&self) -> &str {
        self.demo_platform_email.as_deref().expect("a demo app")
    }

    /// Sets a tenant setting.
    pub async fn setting(&self, key: &str, value: serde_json::Value) {
        platform::set_setting(&self.pool, self.tenant_id, key, &value)
            .await
            .expect("set setting");
        self.state.tenants.invalidate_all();
    }

    /// Sends a request on `host`, storing `Set-Cookie`s in the jar.
    pub async fn send_to(
        &mut self,
        host: &str,
        method: Method,
        path: &str,
        form: Option<&[(&str, &str)]>,
        headers: &[(&str, &str)],
    ) -> Res {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", host);
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header("cookie", cookie);
        }
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let body = match form {
            Some(fields) => {
                builder = builder.header("content-type", "application/x-www-form-urlencoded");
                Body::from(serde_urlencoded(fields))
            }
            None => Body::empty(),
        };
        let res = self
            .router
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("infallible");
        for value in res.headers().get_all(http::header::SET_COOKIE) {
            let Ok(raw) = value.to_str() else { continue };
            let pair = raw.split(';').next().unwrap_or_default();
            let Some((name, val)) = pair.split_once('=') else {
                continue;
            };
            let expired = raw.to_ascii_lowercase().contains("max-age=0") || val.is_empty();
            if expired {
                self.cookies.remove(name.trim());
            } else {
                self.cookies
                    .insert(name.trim().to_owned(), val.trim().to_owned());
            }
        }
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
            .await
            .expect("body");
        Res {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// `GET path` on the tenant host.
    pub async fn get(&mut self, path: &str) -> Res {
        let host = self.host.clone();
        self.send_to(&host, Method::GET, path, None, &[]).await
    }

    /// `POST path` (form) on the tenant host.
    pub async fn post(&mut self, path: &str, form: &[(&str, &str)]) -> Res {
        let host = self.host.clone();
        self.send_to(&host, Method::POST, path, Some(form), &[])
            .await
    }

    /// Deletes the tenant (cascades to users, clients, sessions …).
    pub async fn cleanup(mut self) {
        assert!(
            platform::delete_tenant(&self.pool, self.tenant_id)
                .await
                .expect("delete tenant")
        );
        for email in self
            .demo_platform_email
            .iter()
            .cloned()
            .chain([hos_web::mode::operator_email(&self.slug)])
        {
            hos_db::repo::demo::delete_platform_admin(&self.pool, &email)
                .await
                .expect("delete platform admin record");
        }
        self.cleaned = true;
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let Some(config) = config() else { return };
        let id = self.tenant_id;
        let platform_email = self.demo_platform_email.clone();
        let operator = hos_web::mode::operator_email(&self.slug);
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let cfg = DbConfig::from_app_config(&config).with_max_connections(1);
                if let Ok(pool) = hos_db::connect(&cfg).await {
                    let _ = platform::delete_tenant(&pool, id).await;
                    for email in platform_email.into_iter().chain([operator]) {
                        let _ = hos_db::repo::demo::delete_platform_admin(&pool, &email).await;
                    }
                    pool.close().await;
                }
            });
        })
        .join();
    }
}

/// `application/x-www-form-urlencoded` body.
pub fn serde_urlencoded(fields: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in fields {
        s.append_pair(k, v);
    }
    s.finish()
}
