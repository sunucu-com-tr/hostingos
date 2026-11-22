//! Admin sidebar. Each entry names the permission that reveals it; the
//! routes themselves must still check permissions (`RequirePermission`).

use hos_auth::rbac::Permission;

use crate::extract::CurrentStaff;
use crate::view::{Crumb, MenuDef, MenuSectionDef, PageCtx, PanelChrome};

const fn item(
    key: &'static str,
    href: &'static str,
    icon: &'static str,
    p: Option<Permission>,
) -> MenuDef {
    MenuDef {
        key,
        href,
        icon,
        permission: p,
        also: &[],
    }
}

/// Sidebar definition (only pages that exist; handlers enforce permissions).
pub const MENU: &[MenuSectionDef] = &[
    MenuSectionDef {
        key: "",
        items: &[item("admin.menu.dashboard", "/admin", "gauge", None)],
    },
    MenuSectionDef {
        key: "admin.menu.section_customers",
        items: &[
            item(
                "admin.menu.clients",
                "/admin/clients",
                "users",
                Some(Permission::CLIENTS_VIEW),
            ),
            item(
                "admin.menu.orders",
                "/admin/orders",
                "cart",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "admin.menu.provisioning",
                "/admin/orders/provisioning",
                "settings",
                Some(Permission::SERVICES_VIEW),
            ),
            item(
                "admin.menu.services",
                "/admin/services",
                "server",
                Some(Permission::SERVICES_VIEW),
            ),
            item(
                "admin.menu.domains",
                "/admin/domains",
                "globe",
                Some(Permission::DOMAINS_MANAGE),
            ),
            item(
                "tasks.menu",
                "/admin/tasks",
                "check-circle",
                Some(Permission::TASKS_MANAGE),
            ),
            item(
                "fraud.menu",
                "/admin/fraud",
                "shield",
                Some(Permission::FRAUD_MANAGE),
            ),
        ],
    },
    MenuSectionDef {
        key: "admin.menu.section_billing",
        items: &[
            item(
                "admin.menu.invoices",
                "/admin/invoices",
                "receipt",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "quotes.menu",
                "/admin/quotes",
                "file-text",
                Some(Permission::QUOTES_VIEW),
            ),
            item(
                "admin.menu.transactions",
                "/admin/transactions",
                "credit-card",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "einvoice.menu",
                "/admin/einvoice",
                "file-text",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "affiliates.menu",
                "/admin/affiliates",
                "percent",
                Some(Permission::AFFILIATES_VIEW),
            ),
            item(
                "admin.menu.coupons",
                "/admin/billing/coupons",
                "tag",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "admin.menu.taxes",
                "/admin/billing/taxes",
                "percent",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "admin.menu.currencies",
                "/admin/billing/currencies",
                "layers",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "admin.menu.gateway_log",
                "/admin/billing/gateway-log",
                "activity",
                Some(Permission::BILLING_VIEW),
            ),
            item(
                "admin.menu.reports",
                "/admin/reports",
                "bar-chart",
                Some(Permission::REPORTS_VIEW),
            ),
        ],
    },
    MenuSectionDef {
        key: "admin.menu.section_catalog",
        items: &[
            item(
                "admin.menu.products",
                "/admin/catalog",
                "box",
                Some(Permission::PRODUCTS_MANAGE),
            ),
            item(
                "admin.menu.options",
                "/admin/catalog/options",
                "list",
                Some(Permission::PRODUCTS_MANAGE),
            ),
            item(
                "admin.menu.addons",
                "/admin/catalog/addons",
                "grid",
                Some(Permission::PRODUCTS_MANAGE),
            ),
            item(
                "admin.menu.tlds",
                "/admin/catalog/tlds",
                "globe",
                Some(Permission::PRODUCTS_MANAGE),
            ),
            item(
                "bundles.menu",
                "/admin/catalog/bundles",
                "layers",
                Some(Permission::PRODUCTS_MANAGE),
            ),
        ],
    },
    MenuSectionDef {
        key: "admin.menu.section_support",
        items: &[
            MenuDef {
                // Canned replies and departments are part of the ticket desk.
                also: &["/admin/support/canned", "/admin/support/departments"],
                ..item(
                    "admin.menu.tickets",
                    "/admin/support/tickets",
                    "ticket",
                    Some(Permission::SUPPORT_VIEW),
                )
            },
            item(
                "inbound.admin.menu",
                "/admin/support/inbound",
                "inbox",
                Some(Permission::SUPPORT_MANAGE),
            ),
            item(
                "admin.menu.kb",
                "/admin/support/kb",
                "book",
                Some(Permission::SUPPORT_MANAGE),
            ),
            item(
                "admin.menu.announcements",
                "/admin/support/announcements",
                "megaphone",
                Some(Permission::SUPPORT_MANAGE),
            ),
            item(
                "downloads.admin.menu",
                "/admin/downloads",
                "download",
                Some(Permission::CONTENT_MANAGE),
            ),
            item(
                "netstatus.menu",
                "/admin/status",
                "activity",
                Some(Permission::STATUS_MANAGE),
            ),
        ],
    },
    MenuSectionDef {
        key: "admin.menu.section_content",
        items: &[
            item(
                "admin.menu.pages",
                "/admin/content/pages",
                "file-text",
                Some(Permission::CONTENT_MANAGE),
            ),
            item(
                "admin.menu.blog",
                "/admin/content/posts",
                "newspaper",
                Some(Permission::CONTENT_MANAGE),
            ),
            item(
                "admin.menu.seo",
                "/admin/seo",
                "search",
                Some(Permission::SEO_MANAGE),
            ),
            item(
                "admin.menu.ai_studio",
                "/admin/ai",
                "sparkles",
                Some(Permission::SEO_MANAGE),
            ),
        ],
    },
    MenuSectionDef {
        key: "admin.menu.section_system",
        items: &[
            item(
                "admin.menu.integrations",
                "/admin/integrations",
                "plug",
                Some(Permission::INTEGRATIONS_MANAGE),
            ),
            item(
                "admin.menu.ip_pools",
                "/admin/ip-pools",
                "hard-drive",
                Some(Permission::INTEGRATIONS_MANAGE),
            ),
            item(
                "admin.menu.audit",
                "/admin/system/audit",
                "list",
                Some(Permission::SETTINGS_MANAGE),
            ),
            item(
                "admin.menu.jobs",
                "/admin/system/jobs",
                "clock",
                Some(Permission::SETTINGS_MANAGE),
            ),
            item(
                "import.menu",
                "/admin/tools/import",
                "database",
                Some(Permission::IMPORT_MANAGE),
            ),
            item(
                "admin.menu.settings",
                "/admin/settings",
                "settings",
                Some(Permission::SETTINGS_MANAGE),
            ),
            item(
                "admin.menu.staff",
                "/admin/staff",
                "users",
                Some(Permission::STAFF_MANAGE),
            ),
            // Single-company mode only (`crate::mode`): updates,
            // maintenance, server health, job queues.
            item(
                "admin.menu.server",
                crate::mode::SERVER_PREFIX,
                "server",
                Some(Permission::SYSTEM_MANAGE),
            ),
            item(
                "admin.menu.developer",
                "/admin/developer",
                "code",
                Some(Permission::SETTINGS_MANAGE),
            ),
        ],
    },
];

/// The panel shell for an admin page (entries filtered by permission).
pub fn chrome(ctx: &PageCtx, staff: &CurrentStaff, crumbs: Vec<Crumb>) -> PanelChrome {
    let root = ctx.t("admin.area");
    let trail = if crumbs.is_empty() {
        vec![Crumb::current(root)]
    } else {
        std::iter::once(Crumb::link(root, ctx.url("/admin")))
            .chain(crumbs)
            .collect()
    };
    let single = ctx.single_company();
    PanelChrome::build(ctx, "admin", "admin.area", MENU, |p| {
        p.is_none_or(|p| staff.can(p) && (single || p != Permission::SYSTEM_MANAGE))
    })
    .with_crumbs(trail)
}

/// The panel shell of the owner's server pages (single-company mode): the
/// admin menu, with "Server" as the trail root.
pub fn server_chrome(ctx: &PageCtx, crumbs: Vec<Crumb>) -> PanelChrome {
    let trail = std::iter::once(Crumb::link(ctx.t("admin.area"), ctx.url("/admin")))
        .chain(std::iter::once(Crumb::link(
            ctx.t("admin.menu.server"),
            ctx.url(crate::mode::SERVER_PREFIX),
        )))
        .chain(crumbs)
        .collect();
    PanelChrome::build(ctx, "admin", "admin.area", MENU, |p| {
        p.is_none_or(|p| ctx.allows(p))
    })
    .with_crumbs(trail)
}
