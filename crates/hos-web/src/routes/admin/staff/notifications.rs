//! Staff notifications (`notifications` table): the admin top-bar bell
//! (htmx-refreshed unread count), the list page, mark read / mark all read.
//!
//! Every staff member only ever sees their own rows (tenant + user scoped);
//! no permission beyond being staff is needed.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use hos_core::ids::NotificationId;
use hos_db::repo::notifications::{self as nrepo, NotificationRow};
use http::header;

use super::super::common::{self, fragment};
use crate::error::{AppError, AppResult};
use crate::extract::CurrentStaff;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

const BACK: &str = "/admin/notifications";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(BACK, get(list))
        .route("/admin/notifications/bell", get(bell))
        .route("/admin/update-indicator", get(update_indicator))
        .route("/admin/notifications/read-all", post(read_all))
        .route("/admin/notifications/{id}/read", post(read_one))
}

/// Only same-site absolute paths are followed (no `//host`, no schemes).
fn safe_link(link: Option<&str>) -> Option<&str> {
    link.filter(|l| l.starts_with('/') && !l.starts_with("//") && !l.contains('\\'))
}

#[derive(Template)]
#[template(path = "admin/staff/bell.html")]
struct Bell {
    ctx: PageCtx,
    unread: i64,
}
crate::page!(Bell);

/// `GET /admin/notifications/bell` (htmx): the bell button with its count.
async fn bell(
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    ctx: PageCtx,
) -> AppResult<Response> {
    let unread = nrepo::unread_count(&state.pool, tenant.id, staff.user_id).await?;
    let html = Bell { ctx, unread }.render().map_err(AppError::internal)?;
    let mut res = fragment(html);
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(res)
}

#[derive(Template)]
#[template(path = "admin/staff/update_indicator.html")]
struct UpdateIndicator {
    ctx: PageCtx,
    /// An update is available.
    show: bool,
    /// It fixes a security issue.
    security: bool,
    /// Tooltip / accessible name.
    label: String,
    /// `/platform/updates` on the console host (`/admin/server/updates` in
    /// single-company mode).
    href: String,
}
crate::page!(UpdateIndicator);

/// `GET /admin/update-indicator` (htmx, loaded once per page): a discreet
/// top-bar link to `/platform/updates` when a newer release is available —
/// only when the same browser also holds an active platform console session
/// (the tenant admin who also runs the installation). Matching e-mail
/// addresses proves nothing (a tenant can give a staff member any address),
/// so it is never used. In single-company mode the owner (`system.manage`)
/// sees it and it leads to `/admin/server/updates`. Everyone else gets an
/// empty fragment, which removes the placeholder.
async fn update_indicator(
    State(state): State<AppState>,
    _staff: CurrentStaff,
    headers: header::HeaderMap,
    ctx: PageCtx,
) -> AppResult<Response> {
    let single = state.web.single();
    let operator = if single {
        ctx.allows(hos_auth::rbac::Permission::SYSTEM_MANAGE)
    } else {
        crate::session::platform_admin_of(&state, &headers)
            .await?
            .is_some_and(|a| a.status == hos_db::enums::AccountStatus::Active)
    };
    let available = if operator {
        crate::routes::platform::updates::available_release(&state).await
    } else {
        None
    };
    let href = if single {
        crate::mode::console_link(true, "/platform/updates")
    } else {
        state
            .base_url()
            .join("/platform/updates")
            .map_or_else(|_| "/platform/updates".to_owned(), |u| u.to_string())
    };
    let (show, security, label) = match available {
        Some((v, security)) => (
            true,
            security,
            ctx.tv(
                if security {
                    "admin.shell.update_security"
                } else {
                    "admin.shell.update_available"
                },
                "version",
                v,
            ),
        ),
        None => (false, false, String::new()),
    };
    let html = UpdateIndicator {
        ctx,
        show,
        security,
        label,
        href,
    }
    .render()
    .map_err(AppError::internal)?;
    let mut res = fragment(html);
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(res)
}

#[derive(Template)]
#[template(path = "admin/staff/notifications.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<NotificationRow>,
    unread: i64,
}
crate::page!(ListPage);

impl ListPage {
    fn link(&self, n: &NotificationRow) -> bool {
        safe_link(n.link_url.as_deref()).is_some()
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = nrepo::list_notifications(&state.pool, tenant.id, staff.user_id, false, 100).await?;
    let unread = rows.iter().filter(|n| n.read_at.is_none()).count() as i64;
    let title = ctx.t("admin_settings.notifications.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&ListPage { ctx, rows, unread })
}

/// Marks one notification read and follows its link (or back to the list).
async fn read_one(
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id: NotificationId = id.parse().map_err(|_| AppError::NotFound)?;
    let row = hos_db::repo::admin_settings::notification(&state.pool, tenant.id, staff.user_id, id)
        .await?
        .ok_or(AppError::NotFound)?;
    nrepo::mark_read(&state.pool, tenant.id, staff.user_id, id).await?;
    let to = safe_link(row.link_url.as_deref())
        .map(str::to_owned)
        .unwrap_or_else(|| ctx.url(BACK));
    Ok(Redirect::to(&to).into_response())
}

async fn read_all(
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    ctx: PageCtx,
) -> AppResult<Response> {
    nrepo::mark_all_read(&state.pool, tenant.id, staff.user_id).await?;
    Ok(common::done(
        &ctx,
        BACK,
        "admin_settings.notifications.all_read",
    ))
}

#[cfg(test)]
mod tests {
    use super::safe_link;

    #[test]
    fn only_local_links() {
        assert_eq!(
            safe_link(Some("/admin/tickets/1")),
            Some("/admin/tickets/1")
        );
        assert_eq!(safe_link(Some("//evil.example")), None);
        assert_eq!(safe_link(Some("https://evil.example")), None);
        assert_eq!(safe_link(Some("/\\evil.example")), None);
        assert_eq!(safe_link(None), None);
    }
}
