//! Admin panel (`/admin/*`) for tenant staff.
//!
//! Every route sits behind [`guards::require_staff`]; each handler also takes
//! the `RequirePermission<perm::X>` it needs (the menu only hides entries).
//! State changes write `audit_log` through [`common::Audit`]. Business rules
//! live in `hos-billing` (`state.billing`) and `hos-seo`; data access in
//! `hos_db::repo::*` (admin-only queries: `hos_db::repo::admin`).
//!
//! Templates: `templates/admin/` (pages extend `admin/layout.html`),
//! translations: `locales/{tr,en}/admin.json`, styles: `static/css/admin.css`,
//! behaviour: `static/js/admin.js`.

pub mod affiliates;
pub mod ai_studio;
pub mod billing;
pub mod bundles;
pub mod catalog;
pub mod catalog_extras;
pub mod chart;
pub mod clients;
pub mod common;
pub mod content;
pub mod dashboard;
pub mod developer;
pub mod domains;
pub mod downloads;
pub mod einvoice;
pub mod fraud;
pub mod impersonate;
pub mod import;
pub mod integrations;
pub mod invoices;
pub mod kb;
pub mod mail_inbound;
pub mod mailer;
pub mod menu;
pub mod orders;
pub mod quotes;
pub mod reports;
pub mod search;
pub mod seo;
pub mod services;
pub mod settings;
pub mod staff;
pub mod status;
mod styleguide;
pub mod support;
pub mod system;
pub mod tasks;
pub mod vm_manage;

use axum::Router;
use axum::middleware::from_fn;
use axum::routing::{get, post};

use crate::routes::guards;
use crate::state::AppState;

/// Routes of the admin panel.
pub fn router(_state: &AppState) -> Router<AppState> {
    let mut r = Router::new()
        .route("/admin", get(dashboard::show))
        .route("/admin/dashboard/kpis", get(dashboard::kpis))
        .route("/admin/search", get(search::search))
        .route("/admin/clients/{id}/impersonate", post(impersonate::start))
        .merge(clients::routes())
        .merge(orders::routes())
        .merge(quotes::routes())
        .merge(affiliates::routes())
        .merge(status::routes())
        .merge(fraud::routes())
        .merge(tasks::routes())
        .merge(invoices::routes())
        .merge(einvoice::routes())
        .merge(billing::routes())
        .merge(catalog::routes())
        .merge(catalog_extras::routes())
        .merge(bundles::routes())
        .merge(downloads::routes())
        .merge(mail_inbound::routes())
        .merge(services::routes())
        .merge(vm_manage::routes())
        .merge(domains::routes())
        .merge(integrations::routes())
        .merge(system::routes())
        .merge(import::routes())
        .merge(settings::routes())
        .merge(staff::routes())
        .merge(developer::routes())
        .merge(reports::routes())
        .merge(support::routes())
        .merge(kb::routes())
        .merge(content::routes())
        .merge(ai_studio::routes())
        .merge(seo::routes())
        .route("/admin/preview/markdown", post(common::markdown_preview));
    if cfg!(debug_assertions) {
        r = r.route("/admin/_styleguide", get(styleguide::show));
    }
    r.route_layer(from_fn(guards::require_staff)).merge(
        // Reached from the impersonated (client) session, so outside the staff guard.
        Router::new().route(
            "/admin/impersonation/end",
            get(impersonate::end_page).post(impersonate::end),
        ),
    )
    // Public, tenant-scoped brand images (logo / favicon uploads).
    .merge(settings::general::public_routes())
}
