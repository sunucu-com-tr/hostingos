//! `/admin/search` — the command palette / global search. htmx requests get
//! the result list fragment; a plain GET renders a full results page. Hits
//! are filtered by the permissions of the staff member.

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use hos_auth::rbac::Permission;
use hos_db::repo::admin::{self as arepo, SearchHit};

use super::common::{self, Ui as _};
use crate::error::AppResult;
use crate::extract::{CurrentStaff, Htmx, PageQuery};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// One rendered result.
pub struct Hit {
    pub group: String,
    /// First hit of its group (renders the group heading).
    pub first: bool,
    pub title: String,
    pub subtitle: String,
    pub href: String,
    pub icon: &'static str,
}

/// A shortcut to an admin page whose name matches the query.
pub struct Shortcut {
    pub label: String,
    pub href: String,
    pub icon: &'static str,
}

#[derive(Template)]
#[template(path = "admin/_search_results.html")]
struct ResultsFragment {
    ctx: PageCtx,
    q: String,
    hits: Vec<Hit>,
    shortcuts: Vec<Shortcut>,
}
crate::page!(ResultsFragment);

#[derive(Template)]
#[template(path = "admin/search.html")]
struct SearchPage {
    ctx: PageCtx,
    q: String,
    hits: Vec<Hit>,
    shortcuts: Vec<Shortcut>,
}
crate::page!(SearchPage);

fn allowed(staff: &CurrentStaff, kind: &str) -> bool {
    match kind {
        "client" => staff.can(Permission::CLIENTS_VIEW),
        "invoice" | "order" | "transaction" | "transaction_client" => {
            staff.can(Permission::BILLING_VIEW)
        }
        "service" => staff.can(Permission::SERVICES_VIEW),
        "domain" => staff.can(Permission::DOMAINS_MANAGE),
        "ticket" => staff.can(Permission::SUPPORT_VIEW),
        _ => false,
    }
}

fn to_hit(ctx: &PageCtx, h: SearchHit) -> Hit {
    let (base, icon) = match h.kind.as_str() {
        "client" => ("/admin/clients/", "user"),
        "invoice" => ("/admin/invoices/", "receipt"),
        "order" => ("/admin/orders/", "cart"),
        "domain" => ("/admin/domains/", "globe"),
        "service" => ("/admin/services/", "server"),
        "transaction" => ("/admin/invoices/", "credit-card"),
        "transaction_client" => ("/admin/clients/", "credit-card"),
        _ => ("/admin/support/tickets/", "life-buoy"),
    };
    let subtitle = h.subtitle.unwrap_or_default();
    let subtitle = match h.kind.as_str() {
        "order" | "domain" | "ticket" => ctx.lbl("admin.status", &subtitle),
        _ => subtitle,
    };
    Hit {
        group: ctx.t(&format!(
            "admin.search.kind_{}",
            h.kind.strip_suffix("_client").unwrap_or(&h.kind)
        )),
        first: false,
        title: h.title,
        subtitle,
        href: ctx.url(format!("{base}{}", h.id)),
        icon,
    }
}

fn shortcuts(ctx: &PageCtx, staff: &CurrentStaff, q: &str) -> Vec<Shortcut> {
    let needle = q.to_lowercase();
    super::menu::MENU
        .iter()
        .flat_map(|s| s.items.iter())
        .filter(|d| d.permission.is_none_or(|p| staff.can(p)))
        .filter_map(|d| {
            let label = ctx.t(d.key);
            label.to_lowercase().contains(&needle).then(|| Shortcut {
                label,
                href: ctx.url(d.href),
                icon: d.icon,
            })
        })
        .take(5)
        .collect()
}

pub(super) async fn search(
    State(state): State<AppState>,
    tenant: Tenant,
    staff: CurrentStaff,
    htmx: Htmx,
    q: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let term = q.q.clone().unwrap_or_default();
    let (hits, shortcuts) = if term.chars().count() >= 2 {
        let mut hits: Vec<Hit> = arepo::search(&state.pool, tenant.id, &term)
            .await?
            .into_iter()
            .filter(|h| allowed(&staff, &h.kind))
            .map(|h| to_hit(&ctx, h))
            .collect();
        let mut last = String::new();
        for h in &mut hits {
            h.first = h.group != last;
            last.clone_from(&h.group);
        }
        (hits, shortcuts(&ctx, &staff, &term))
    } else {
        (Vec::new(), Vec::new())
    };
    if htmx.wants_fragment() {
        return render(&ResultsFragment {
            ctx,
            q: term,
            hits,
            shortcuts,
        });
    }
    let title = ctx.t("admin.search.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&SearchPage {
        ctx,
        q: term,
        hits,
        shortcuts,
    })
}
