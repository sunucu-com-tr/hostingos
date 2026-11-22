//! Single-company mode (`HOS_MODE=single`, the default; see
//! [`AppMode`](crate::config::AppMode)).
//!
//! The schema and the code stay multi-company; this module makes that
//! invisible when one installation runs one hosting company:
//!
//! * [`resolve_company`] — at startup: exactly one company in the database
//!   (none → "run `hostingos setup`", several → refused). Every host that
//!   reaches the app then shows it (`crate::tenant`).
//! * [`setup`] — `hostingos setup`: migrations, the company and its owner.
//! * [`layer`] — the server area `/admin/server` for the owner (permission
//!   `system.manage`, held only by the owner role): the operator pages that
//!   used to live under `/platform` — software updates with their
//!   re-authentication, maintenance mode and server health (job queues and
//!   the audit log are admin pages already). `/platform` itself answers 404.
//!
//! The server-area pages are the operator pages of multi-company mode,
//! served under the admin path: this layer maps `/admin/server/…` to
//! `/platform/…` before routing and the links in the answer back. Their
//! actions are recorded under one internal operator record per company
//! ([`operator_email`]; it cannot sign in) together with the staff member
//! who acted.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use hos_auth::rbac::Permission;
use hos_db::PgPool;
use hos_db::repo::identity::UserRow;
use hos_db::repo::platform::PlatformAdminRow;
use http::HeaderValue;

use crate::config::{AppMode, WebConfig};
use crate::error::AppError;
use crate::session::SessionInfo;
use crate::state::AppState;

/// Where the owner finds the server pages.
pub const SERVER_PREFIX: &str = "/admin/server";

/// Operator pages reachable through [`SERVER_PREFIX`] (first path segment
/// below `/platform`); everything else there is about several companies.
/// (Job queues and the audit log are in the admin panel already:
/// `/admin/system/jobs`, `/admin/system/audit`.)
const SERVER_PAGES: &[&str] = &["system", "updates"];

/// The company of a single-company database, from its companies' slugs.
pub fn pick_company(slugs: &[String]) -> Result<String, String> {
    match slugs {
        [one] => Ok(one.clone()),
        [] => Err(
            "no hosting company has been set up in this database yet: run \
             `hostingos setup --email <owner e-mail>` once (docs/INSTALL.md)"
                .into(),
        ),
        many => Err(format!(
            "this database holds {} hosting companies, but the server runs in single-company \
             mode ({}=single, the default). Point DATABASE_URL at the database of one company, \
             or set {}=multi to run them all with the operator console",
            if many.len() > 2 {
                "more than two".to_owned()
            } else {
                many.len().to_string()
            },
            AppMode::ENV,
            AppMode::ENV
        )),
    }
}

/// Up to three company slugs (enough to tell none / one / several); `None`
/// when the database has no tables yet.
async fn company_slugs(pool: &PgPool) -> anyhow::Result<Option<Vec<String>>> {
    let missing: bool = sqlx::query_scalar("select to_regclass('public.tenants') is null")
        .fetch_one(pool)
        .await
        .context("reaching the database")?;
    if missing {
        return Ok(None);
    }
    Ok(Some(
        sqlx::query_scalar("select slug from tenants order by created_at, slug limit 3")
            .fetch_all(pool)
            .await?,
    ))
}

/// Startup: finds the one company (single mode). Demo mode knows its
/// company already (it creates it, `crate::demo`).
pub async fn resolve_company(pool: &PgPool, web: &mut WebConfig) -> anyhow::Result<()> {
    if !web.single() {
        web.company = None;
        return Ok(());
    }
    if let Some(demo) = &web.demo {
        web.company = Some(demo.tenant_slug.clone());
        return Ok(());
    }
    let slugs = company_slugs(pool).await?.unwrap_or_default();
    if slugs.is_empty() {
        // Not set up yet. The server still starts, so a container platform
        // keeps it running and `hostingos migrate` / `hostingos setup` can
        // be run inside it; every page answers 503 until then.
        tracing::warn!(
            "no hosting company has been set up in this database yet: run \
             `hostingos setup --email <owner e-mail>` (docs/DEPLOYMENT.md). Until then \
             every page answers 503"
        );
        web.company = Some(COMPANY_SLUG.to_owned());
        return Ok(());
    }
    web.company = Some(pick_company(&slugs).map_err(anyhow::Error::msg)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// hostingos setup
// ---------------------------------------------------------------------------

/// `hostingos setup` input.
#[derive(Debug, Clone)]
pub struct SetupRequest {
    /// Owner's login e-mail.
    pub email: String,
    /// Owner's password; `None` → generated and printed.
    pub password: Option<String>,
    /// Company name.
    pub company: String,
    /// Default language.
    pub locale: hos_core::domain::Locale,
    /// Default currency.
    pub currency: hos_core::money::Currency,
}

/// Internal slug of the company `hostingos setup` creates.
pub const COMPANY_SLUG: &str = "main";

/// First installation: applies migrations, then creates the company and its
/// owner account. Does nothing (and says so) when the company exists.
pub async fn setup(req: SetupRequest) -> anyhow::Result<()> {
    let config = hos_core::config::AppConfig::from_env().context("loading configuration")?;
    let migrations = hos_db::connect(&hos_db::DbConfig::for_migrations(&config))
        .await
        .context("connecting to the database")?;
    hos_db::migrate(&migrations)
        .await
        .context("running migrations")?;
    let slugs = company_slugs(&migrations).await?.unwrap_or_default();
    migrations.close().await;
    match slugs.as_slice() {
        [] => {}
        [_] => {
            println!(
                "already set up: the company exists. Sign in at {}/admin",
                base(&config)
            );
            return Ok(());
        }
        _ => bail!("{}", pick_company(&slugs).unwrap_err()),
    }
    let (password, generated) = match req.password.filter(|p| !p.is_empty()) {
        Some(p) => (p, false),
        None => (crate::cli::random_password(), true),
    };
    hos_auth::password::PasswordPolicy::default()
        .validate(&password, &[&req.email, &req.company])
        .map_err(|v| {
            let reasons: Vec<String> = v
                .iter()
                .map(|x| x.message(hos_core::domain::Locale::En))
                .collect();
            anyhow::anyhow!("owner password rejected: {}", reasons.join("; "))
        })?;
    let state = AppState::from_env_mode(Some(AppMode::Multi)).await?;
    let mut new = crate::routes::platform::provision::NewTenantRequest::new(
        COMPANY_SLUG,
        &req.company,
        &req.email,
    );
    new.owner_password = Some(password.clone());
    new.trial_days = Some(0);
    // No plan: plans belong to multi-company operation (empty = none).
    new.plan_code = Some(String::new());
    new.locale = req.locale;
    new.currency = req.currency;
    crate::routes::platform::provision::provision(&state, &new)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    state.pool.close().await;
    println!("company created: {}", req.company);
    println!("admin panel: {}/admin", base(&config));
    println!("owner login: {}", req.email);
    if generated {
        println!("password:    {password}   (shown once; change it after signing in)");
    }
    Ok(())
}

fn base(config: &hos_core::config::AppConfig) -> String {
    config.base_url.as_str().trim_end_matches('/').to_owned()
}

// ---------------------------------------------------------------------------
// The server area (/admin/server)
// ---------------------------------------------------------------------------

/// Request extension of a server-area request: who acts, and the internal
/// operator record their actions are filed under.
#[derive(Debug, Clone)]
pub struct ServerArea {
    /// The internal operator record ([`operator_email`]).
    pub operator: PlatformAdminRow,
    /// The staff member (owner) who acts.
    pub staff: UserRow,
    /// The path the browser asked for (`/admin/server/…`).
    pub path: String,
}

/// E-mail of the internal operator record of company `slug`. The
/// `.invalid` domain never receives mail; the record has no password and
/// cannot sign in.
pub fn operator_email(slug: &str) -> String {
    format!("server-owner+{slug}@hostingos.invalid")
}

/// Whether `email` is such an internal record.
pub fn is_operator_email(email: &str) -> bool {
    let e = email.trim().to_ascii_lowercase();
    e.starts_with("server-owner+") && e.ends_with("@hostingos.invalid")
}

/// The operator record of company `slug` (created on first use).
pub async fn ensure_operator(pool: &PgPool, slug: &str) -> Result<PlatformAdminRow, sqlx::Error> {
    let email = operator_email(slug);
    sqlx::query(
        "insert into platform_admins (email, name) values ($1, 'Owner (admin panel)')
         on conflict (email) do nothing",
    )
    .bind(&email)
    .execute(pool)
    .await?;
    sqlx::query_as::<_, PlatformAdminRow>("select * from platform_admins where email = $1")
        .bind(&email)
        .fetch_one(pool)
        .await
}

/// `/admin/server[/…]` → the `/platform/…` page it shows, if any.
pub fn map_in(path: &str) -> Option<String> {
    let rest = path.strip_prefix(SERVER_PREFIX)?;
    if rest.is_empty() || rest == "/" {
        return Some("/platform/system".into());
    }
    let rest = rest.strip_prefix('/')?;
    let first = rest.split('/').next().unwrap_or_default();
    SERVER_PAGES
        .contains(&first)
        .then(|| format!("/platform/{rest}"))
}

/// `/platform…` in a link of a server-area answer → `/admin/server…`
/// (also behind a `/en` or `/tr` prefix). `None` for anything else.
fn map_out(path: &str) -> Option<String> {
    let (prefix, rest) = ["/en", "/tr"]
        .iter()
        .find_map(|l| {
            path.strip_prefix(l)
                .filter(|r| r.starts_with("/platform"))
                .map(|r| (*l, r))
        })
        .unwrap_or(("", path));
    let tail = rest.strip_prefix("/platform")?;
    if !(tail.is_empty() || tail.starts_with(['/', '?', '#'])) {
        return None;
    }
    Some(format!("{prefix}{SERVER_PREFIX}{tail}"))
}

/// A console path as people reach it: `/admin/server…` in single-company
/// mode, unchanged otherwise (e-mail links, the update indicator).
pub fn console_link(single: bool, path: &str) -> String {
    if single {
        map_out(path).unwrap_or_else(|| path.to_owned())
    } else {
        path.to_owned()
    }
}

/// Rewrites attribute values (`href`, `action`, `hx-*` …) that point at
/// `/platform` in an HTML page of the server area: only values right after
/// `="` or `='`, never text.
pub fn rewrite_links(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 256);
    let mut rest = html;
    loop {
        let next = [("=\"", '"'), ("='", '\'')]
            .iter()
            .filter_map(|(pat, q)| rest.find(pat).map(|i| (i, *q)))
            .min_by_key(|(i, _)| *i);
        let Some((i, quote)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i + 2]);
        let after = &rest[i + 2..];
        let end = after.find(quote).unwrap_or(after.len());
        let value = &after[..end];
        match map_out(value) {
            Some(mapped) => out.push_str(&mapped),
            None => out.push_str(value),
        }
        rest = &after[end..];
    }
}

/// Middleware in the tenant stack, after the session (single mode only):
/// admits the owner to the server area and maps its paths and links.
pub async fn layer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    if !state.web.single() {
        return next.run(req).await;
    }
    let path = req.uri().path().to_owned();
    let Some(target) = map_in(&path) else {
        return next.run(req).await;
    };
    let session = req
        .extensions()
        .get::<Arc<SessionInfo>>()
        .cloned()
        .filter(|s| s.is_authenticated());
    let Some((session, user)) = session.and_then(|s| s.user.clone().map(|u| (s, u))) else {
        let next_url: String = url::form_urlencoded::byte_serialize(path.as_bytes()).collect();
        return Redirect::to(&format!("/auth/login?next={next_url}")).into_response();
    };
    if !session.principal.can(Permission::SYSTEM_MANAGE) {
        return AppError::Forbidden("server operations are for the owner".into()).into_response();
    }
    let Some(company) = state.web.company.as_deref() else {
        return AppError::NotFound.into_response();
    };
    let operator = match ensure_operator(&state.pool, company).await {
        Ok(row) => row,
        Err(e) => return AppError::internal(e).into_response(),
    };
    req.extensions_mut().insert(ServerArea {
        operator,
        staff: user,
        path: path.clone(),
    });
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    match format!("{target}{query}").parse() {
        Ok(uri) => *req.uri_mut() = uri,
        Err(_) => return AppError::NotFound.into_response(),
    }
    rewrite_response(next.run(req).await).await
}

async fn rewrite_response(res: Response) -> Response {
    let (mut parts, body) = res.into_parts();
    for name in ["location", "hx-redirect", "hx-location"] {
        if let Some(mapped) = parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(map_out)
            .and_then(|v| HeaderValue::from_str(&v).ok())
        {
            parts.headers.insert(name, mapped);
        }
    }
    let html = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if !html {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, 16 * 1024 * 1024).await else {
        return AppError::internal("reading a server-area page").into_response();
    };
    let page = rewrite_links(&String::from_utf8_lossy(&bytes));
    parts.headers.remove(http::header::CONTENT_LENGTH);
    parts.headers.remove(http::header::ETAG);
    Response::from_parts(parts, axum::body::Body::from(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_company() {
        assert_eq!(pick_company(&["acme".into()]).unwrap(), "acme");
        assert!(pick_company(&[]).unwrap_err().contains("hostingos setup"));
        assert!(
            pick_company(&["a".into(), "b".into()])
                .unwrap_err()
                .contains("HOS_MODE=multi")
        );
    }

    #[test]
    fn server_paths() {
        assert_eq!(map_in("/admin/server").as_deref(), Some("/platform/system"));
        assert_eq!(
            map_in("/admin/server/updates/runs/1").as_deref(),
            Some("/platform/updates/runs/1")
        );
        for p in [
            "/admin/server/jobs",
            "/admin/server/audit",
            "/admin/server/tenants",
            "/admin/server/plans/new",
            "/admin/serverx",
            "/admin/system/jobs",
        ] {
            assert_eq!(map_in(p), None, "{p}");
        }
        assert_eq!(map_out("/platform").as_deref(), Some("/admin/server"));
        assert_eq!(
            map_out("/en/platform/updates?x=1").as_deref(),
            Some("/en/admin/server/updates?x=1")
        );
        assert_eq!(map_out("/platformer"), None);
        assert_eq!(map_out("/admin"), None);
        assert_eq!(
            console_link(true, "/platform/updates"),
            "/admin/server/updates"
        );
        assert_eq!(
            console_link(false, "/platform/updates"),
            "/platform/updates"
        );
    }

    #[test]
    fn links_in_pages_are_rewritten() {
        let html = r#"<a href="/platform/updates">U</a><form action='/platform/updates/apply'><a href="/en/platform">x</a> /platform/text "quoted /platform"</form>"#;
        assert_eq!(
            rewrite_links(html),
            r#"<a href="/admin/server/updates">U</a><form action='/admin/server/updates/apply'><a href="/en/admin/server">x</a> /platform/text "quoted /platform"</form>"#
        );
    }
}
