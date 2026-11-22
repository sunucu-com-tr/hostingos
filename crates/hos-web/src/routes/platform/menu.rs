//! Platform console sidebar.

use crate::view::{Crumb, MenuDef, MenuSectionDef, PageCtx, PanelChrome};

const fn item(key: &'static str, href: &'static str, icon: &'static str) -> MenuDef {
    MenuDef {
        key,
        href,
        icon,
        permission: None,
        also: &[],
    }
}

/// Sidebar definition.
pub const MENU: &[MenuSectionDef] = &[
    MenuSectionDef {
        key: "",
        items: &[item("platform.menu.dashboard", "/platform", "gauge")],
    },
    MenuSectionDef {
        key: "platform.menu.section_saas",
        items: &[
            item("platform.menu.tenants", "/platform/tenants", "building"),
            item(
                "platform.menu.subscriptions",
                "/platform/subscriptions",
                "credit-card",
            ),
            item("platform.menu.plans", "/platform/plans", "layers"),
        ],
    },
    MenuSectionDef {
        key: "platform.menu.section_system",
        items: &[
            item("platform.menu.settings", "/platform/settings", "settings"),
            item("platform.menu.admins", "/platform/admins", "shield"),
            item("platform.menu.jobs", "/platform/jobs", "activity"),
            item("platform.menu.audit", "/platform/audit", "file-text"),
            item("platform.menu.system", "/platform/system", "server"),
            item("updates.menu", "/platform/updates", "download"),
            item("platform.menu.account", "/platform/account", "user"),
        ],
    },
];

/// The panel shell for a platform page.
pub fn chrome(ctx: &PageCtx, crumbs: Vec<Crumb>) -> PanelChrome {
    // Single-company mode: these pages are the owner's server area inside
    // the admin panel (`crate::mode`).
    if ctx.single_company() {
        return crate::routes::admin::menu::server_chrome(ctx, crumbs);
    }
    let root = ctx.t("platform.area");
    let trail = if crumbs.is_empty() {
        vec![Crumb::current(root)]
    } else {
        std::iter::once(Crumb::link(root, "/platform"))
            .chain(crumbs)
            .collect()
    };
    PanelChrome::build(ctx, "platform", "platform.area", MENU, |_| true).with_crumbs(trail)
}
