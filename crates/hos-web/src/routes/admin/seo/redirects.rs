//! `/admin/seo/redirects*` — 301 / 302 redirects (`redirects` table, applied
//! by the storefront on 404s to the unprefixed request path).
//!
//! Source paths are validated (absolute, no query / fragment, no panel or
//! asset areas, no language prefix — the storefront matches the path after
//! the `/en` prefix is removed). Loops are rejected (`A → B → A`); a target
//! that is itself redirected, or a source that other redirects point to, is
//! saved with a chain warning. Hits are counted by the storefront.

use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::domain::Locale;
use hos_db::ids::RedirectId;
use hos_db::repo::content::{self as content_repo, RedirectRow};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::super::common::{self, Audit, FormData, Ui as _};
use super::nav;
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::flash::{FlashKind, SetFlash};
use crate::forms::FormErrors;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/seo/redirects", get(list).post(create))
        .route("/admin/seo/redirects/{id}", get(edit).post(update))
        .route("/admin/seo/redirects/{id}/delete", post(delete))
}

/// Paths a redirect may not take over.
const RESERVED: &[&str] = &[
    "/admin",
    "/client",
    "/platform",
    "/api",
    "/auth",
    "/static",
    "/assets",
    "/webhooks",
    "/cart",
    "/checkout",
    "/lang",
];

/// Redirect form values.
#[derive(Debug, Clone, Default)]
pub struct RedirectForm {
    pub from_path: String,
    pub to_url: String,
    pub status_code: String,
    pub note: String,
    pub is_active: bool,
}

impl RedirectForm {
    fn from_row(r: &RedirectRow) -> Self {
        Self {
            from_path: r.from_path.clone(),
            to_url: r.to_url.clone(),
            status_code: r.status_code.to_string(),
            note: r.note.clone().unwrap_or_default(),
            is_active: r.is_active,
        }
    }

    fn read(f: &FormData) -> Self {
        let to = f.text("to_url", 1000);
        Self {
            from_path: f.text("from_path", 500),
            to_url: if to.is_empty() {
                f.text("to_path", 1000)
            } else {
                to
            },
            status_code: f.text("status_code", 3),
            note: f.text("note", 300),
            is_active: !f.has("active_field") || f.flag("is_active"),
        }
    }
}

/// Normalized source path, or the error key.
pub fn check_source(path: &str) -> Result<String, &'static str> {
    let p = path.trim();
    if !p.starts_with('/') || p.starts_with("//") {
        return Err("admin_content.seo.redirects.from_absolute");
    }
    if p.chars().any(|c| c.is_whitespace() || c.is_control()) || p.contains(['?', '#']) {
        return Err("admin_content.seo.redirects.from_chars");
    }
    let p = if p.len() > 1 {
        p.trim_end_matches('/')
    } else {
        p
    };
    if p == "/" {
        return Err("admin_content.seo.redirects.from_home");
    }
    let under = |prefix: &str| p == prefix || p.starts_with(&format!("{prefix}/"));
    if RESERVED.iter().any(|r| under(r)) {
        return Err("admin_content.seo.redirects.from_reserved");
    }
    if Locale::ALL
        .iter()
        .any(|l| under(&format!("/{}", l.as_str())))
    {
        return Err("admin_content.seo.redirects.from_locale");
    }
    Ok(p.to_owned())
}

/// Normalized target (same-site absolute URLs become paths), or the error key.
pub fn check_target(to: &str, origin: &str) -> Result<String, &'static str> {
    let t = to.trim();
    if t.is_empty() {
        return Err("admin.common.required");
    }
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("admin_content.seo.redirects.to_invalid");
    }
    if t.starts_with('/') && !t.starts_with("//") {
        return Ok(t.to_owned());
    }
    match url::Url::parse(t) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => {
            Ok(super::local_path(origin, t).unwrap_or_else(|| t.to_owned()))
        }
        _ => Err("admin_content.seo.redirects.to_invalid"),
    }
}

/// Path part of a target (for graph checks; external URLs stay as they are).
fn target_key(to: &str) -> String {
    let path = to.split(['?', '#']).next().unwrap_or(to);
    if path.len() > 1 {
        path.trim_end_matches('/').to_owned()
    } else {
        path.to_owned()
    }
}

/// Result of the loop / chain analysis.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GraphCheck {
    pub looped: bool,
    pub chain: Vec<String>,
    pub incoming: Vec<String>,
}

/// Follows `to` through the active redirects (other than `own`) and checks
/// whether it comes back to `from`.
pub fn check_graph(
    existing: &[RedirectRow],
    own: Option<RedirectId>,
    from: &str,
    to: &str,
) -> GraphCheck {
    let map: BTreeMap<String, String> = existing
        .iter()
        .filter(|r| r.is_active && Some(r.id) != own)
        .map(|r| (r.from_path.clone(), target_key(&r.to_url)))
        .collect();
    let mut out = GraphCheck::default();
    let mut cur = target_key(to);
    let mut seen = BTreeSet::new();
    loop {
        if cur == from {
            out.looped = true;
            break;
        }
        if !seen.insert(cur.clone()) {
            // A loop that does not involve `from`; still a broken chain.
            out.looped = true;
            break;
        }
        match map.get(&cur) {
            Some(next) => {
                out.chain.push(cur.clone());
                cur = next.clone();
            }
            None => break,
        }
        if out.chain.len() > 20 {
            break;
        }
    }
    out.incoming = map
        .iter()
        .filter(|(_, t)| *t == from)
        .map(|(f, _)| f.clone())
        .collect();
    out
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    q: String,
}

#[derive(Template)]
#[template(path = "admin/seo/redirects.html")]
struct ListPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<RedirectRow>,
    total: usize,
    q: String,
    form: RedirectForm,
    errors: FormErrors,
}
crate::page!(ListPage);

#[derive(Template)]
#[template(path = "admin/seo/redirect.html")]
struct EditPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    r: RedirectRow,
    form: RedirectForm,
    errors: FormErrors,
}
crate::page!(EditPage);

async fn list_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    q: String,
    form: RedirectForm,
    errors: FormErrors,
) -> AppResult<ListPage> {
    let title = ctx.t("admin_content.seo.redirects.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_content.seo.title", "/admin/seo")],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/redirects");
    let all = content_repo::list_redirects(&state.pool, tenant.id).await?;
    let total = all.len();
    let needle = q.to_lowercase();
    let rows = all
        .into_iter()
        .filter(|r| {
            needle.is_empty()
                || r.from_path.to_lowercase().contains(&needle)
                || r.to_url.to_lowercase().contains(&needle)
        })
        .collect();
    Ok(ListPage {
        tabs,
        active,
        rows,
        total,
        q,
        form,
        errors,
        ctx,
    })
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    Query(lq): Query<ListQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = RedirectForm {
        status_code: "301".into(),
        is_active: true,
        ..RedirectForm::default()
    };
    let page = list_page(
        &state,
        &tenant,
        &staff,
        ctx,
        lq.q.trim().chars().take(100).collect(),
        form,
        FormErrors::default(),
    )
    .await?;
    render(&page)
}

/// Validates the form against the tenant's redirects; `(from, to, code,
/// warning)` when it may be saved.
fn validate(
    ctx: &PageCtx,
    form: &RedirectForm,
    existing: &[RedirectRow],
    own: Option<RedirectId>,
    errors: &mut FormErrors,
) -> Option<(String, String, i16, Option<String>)> {
    let from = match check_source(&form.from_path) {
        Ok(p) => Some(p),
        Err(k) => {
            errors.add("from_path", ctx.t(k));
            None
        }
    };
    let to = match check_target(&form.to_url, &ctx.origin) {
        Ok(t) => Some(t),
        Err(k) => {
            errors.add("to_url", ctx.t(k));
            None
        }
    };
    let code: i16 = match form.status_code.as_str() {
        "301" => 301,
        "302" => 302,
        _ => {
            errors.add("status_code", ctx.t("admin.common.invalid_value"));
            301
        }
    };
    let (from, to) = (from?, to?);
    if existing
        .iter()
        .any(|r| r.from_path == from && Some(r.id) != own)
    {
        errors.add("from_path", ctx.t("admin_content.seo.redirects.from_taken"));
        return None;
    }
    if target_key(&to) == from {
        errors.add("to_url", ctx.t("admin_content.seo.redirects.self_loop"));
        return None;
    }
    let g = check_graph(existing, own, &from, &to);
    if g.looped {
        errors.add(
            "to_url",
            ctx.tv(
                "admin_content.seo.redirects.loop",
                "chain",
                format!("{from} → {} → …", target_key(&to)),
            ),
        );
        return None;
    }
    let warning = if !g.chain.is_empty() {
        Some(ctx.tv(
            "admin_content.seo.redirects.chain_warning",
            "path",
            g.chain.join(" → "),
        ))
    } else if !g.incoming.is_empty() {
        Some(ctx.tv(
            "admin_content.seo.redirects.incoming_warning",
            "paths",
            g.incoming.join(", "),
        ))
    } else {
        None
    };
    (!errors.any()).then_some((from, to, code, warning))
}

fn saved(ctx: &PageCtx, back: &str, key: &str, warning: Option<String>) -> Response {
    let mut flash = SetFlash::success(ctx.t(key));
    if let Some(w) = warning {
        flash = flash.and(FlashKind::Warning, w);
    }
    crate::flash::redirect(&ctx.url(back), flash)
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut form = RedirectForm::read(&f);
    form.is_active = true;
    let back = Some(f.text("back", 300))
        .filter(|b| b.starts_with("/admin/") && !b.contains("//"))
        .unwrap_or_else(|| "/admin/seo/redirects".to_owned());
    let existing = content_repo::list_redirects(&state.pool, tenant.id).await?;
    let mut errors = FormErrors::default();
    let Some((from, to, code, warning)) = validate(&ctx, &form, &existing, None, &mut errors)
    else {
        let page = list_page(&state, &tenant, &staff, ctx, String::new(), form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    let note = (!form.note.is_empty()).then_some(form.note.as_str());
    let row = content_repo::create_redirect(&state.pool, tenant.id, &from, &to, code, note).await?;
    au.log(
        &state,
        "redirect.create",
        Some(("redirect", row.id.into())),
        json!({"from": from, "to": to, "status": code}),
    )
    .await?;
    Ok(saved(
        &ctx,
        &back,
        "admin_content.seo.redirects.created",
        warning,
    ))
}

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<RedirectRow> {
    content_repo::get_redirect(&state.pool, tenant.id, RedirectId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

fn edit_page(
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    r: RedirectRow,
    form: RedirectForm,
    errors: FormErrors,
) -> EditPage {
    let title = r.from_path.clone();
    common::page(
        &mut ctx,
        staff,
        &title,
        &[
            ("admin_content.seo.title", "/admin/seo"),
            ("admin_content.seo.redirects.title", "/admin/seo/redirects"),
        ],
    );
    let (tabs, active) = nav(&ctx, "/admin/seo/redirects");
    EditPage {
        tabs,
        active,
        r,
        form,
        errors,
        ctx,
    }
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let r = load(&state, &tenant, id).await?;
    let form = RedirectForm::from_row(&r);
    render(&edit_page(&staff, ctx, r, form, FormErrors::default()))
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let r = load(&state, &tenant, id).await?;
    let form = RedirectForm::read(&f);
    let existing = content_repo::list_redirects(&state.pool, tenant.id).await?;
    let mut errors = FormErrors::default();
    let checked = if form.is_active {
        validate(&ctx, &form, &existing, Some(r.id), &mut errors)
    } else {
        // An inactive redirect cannot loop; still validate the fields.
        validate(&ctx, &form, &[], Some(r.id), &mut errors).and_then(|v| {
            if existing.iter().any(|x| x.from_path == v.0 && x.id != r.id) {
                errors.add("from_path", ctx.t("admin_content.seo.redirects.from_taken"));
                None
            } else {
                Some((v.0, v.1, v.2, None))
            }
        })
    };
    let Some((from, to, code, warning)) = checked else {
        return render_status(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            &edit_page(&staff, ctx, r, form, errors),
        );
    };
    let note = (!form.note.is_empty()).then_some(form.note.as_str());
    content_repo::update_redirect(
        &state.pool,
        tenant.id,
        r.id,
        &from,
        &to,
        code,
        form.is_active,
        note,
    )
    .await?
    .ok_or(AppError::NotFound)?;
    au.log(
        &state,
        "redirect.update",
        Some(("redirect", r.id.into())),
        json!({
            "before": {"from": r.from_path, "to": r.to_url, "status": r.status_code, "active": r.is_active},
            "after": {"from": from, "to": to, "status": code, "active": form.is_active},
        }),
    )
    .await?;
    Ok(saved(
        &ctx,
        "/admin/seo/redirects",
        "admin.common.saved",
        warning,
    ))
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SeoManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let r = load(&state, &tenant, id).await?;
    content_repo::delete_redirect(&state.pool, tenant.id, r.id).await?;
    au.log(
        &state,
        "redirect.delete",
        Some(("redirect", r.id.into())),
        json!({"from": r.from_path, "to": r.to_url}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/seo/redirects",
        "admin.common.deleted",
    ))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use hos_core::ids::TenantId;

    use super::*;

    fn row(from: &str, to: &str) -> RedirectRow {
        RedirectRow {
            id: RedirectId::new(),
            tenant_id: TenantId::new(),
            from_path: from.into(),
            to_url: to.into(),
            status_code: 301,
            is_active: true,
            hits: 0,
            last_hit_at: None,
            note: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn sources() {
        assert_eq!(check_source("/eski-sayfa/").as_deref(), Ok("/eski-sayfa"));
        assert!(check_source("eski").is_err());
        assert!(check_source("//evil.test").is_err());
        assert!(check_source("/a b").is_err());
        assert!(check_source("/a?x=1").is_err());
        assert!(check_source("/admin/x").is_err());
        assert!(check_source("/en/old").is_err());
        assert!(check_source("/english").is_ok());
        assert!(check_source("/").is_err());
    }

    #[test]
    fn targets() {
        assert_eq!(
            check_target("https://site.test/yeni", "https://site.test").as_deref(),
            Ok("/yeni")
        );
        assert_eq!(
            check_target("https://other.test/x", "https://site.test").as_deref(),
            Ok("https://other.test/x")
        );
        assert!(check_target("javascript:alert(1)", "https://site.test").is_err());
        assert!(check_target("//evil.test", "https://site.test").is_err());
    }

    #[test]
    fn loops_and_chains() {
        let existing = vec![row("/a", "/b")];
        assert!(check_graph(&existing, None, "/b", "/a").looped);
        let g = check_graph(&existing, None, "/c", "/a");
        assert!(!g.looped);
        assert_eq!(g.chain, vec!["/a"]);
        let g = check_graph(&existing, None, "/b", "/d");
        assert_eq!(g.incoming, vec!["/a"]);
        let existing = vec![row("/a", "/b"), row("/b", "/c")];
        assert!(check_graph(&existing, None, "/c", "/a").looped);
    }
}
