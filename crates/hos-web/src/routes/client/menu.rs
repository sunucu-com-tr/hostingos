//! Client-area sidebar (label keys live in `locales/*/client.json`). Items a
//! sub-user has no permission for are hidden ([`MemberAccess`]).

use super::{MemberAccess, area_of};
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
        items: &[item("client.menu.dashboard", "/client", "gauge")],
    },
    MenuSectionDef {
        key: "client.menu.section_products",
        items: &[
            item("client.menu.services", "/client/services", "server"),
            item("client.menu.domains", "/client/domains", "globe"),
            item("client.menu.order", "/cart", "cart"),
        ],
    },
    MenuSectionDef {
        key: "client.menu.section_billing",
        items: &[
            item("client.menu.invoices", "/client/invoices", "receipt"),
            item("client.menu.billing", "/client/billing", "credit-card"),
            item("client.menu.orders", "/client/orders", "box"),
            item("quotes.client.menu", "/client/quotes", "file-text"),
        ],
    },
    MenuSectionDef {
        key: "client.menu.section_support",
        items: &[
            item("client.menu.tickets", "/client/tickets", "life-buoy"),
            item("client.menu.kb", "/kb", "book"),
            item("downloads.menu", "/client/downloads", "download"),
        ],
    },
    MenuSectionDef {
        key: "client.menu.section_account",
        items: &[
            item("client.menu.account", "/client/account", "user"),
            item("client.menu.security", "/client/account/security", "shield"),
            item("client.menu.ssh_keys", "/client/account/ssh-keys", "key"),
            item("client.menu.users", "/client/account/users", "users"),
            item("affiliates.client.menu", "/client/affiliate", "percent"),
        ],
    },
];

/// The panel shell for a client page. `crumbs` are the trail below the area
/// root (empty on the dashboard itself).
pub fn chrome(ctx: &PageCtx, access: &MemberAccess, crumbs: Vec<Crumb>) -> PanelChrome {
    let root = ctx.t("client.area");
    let trail = if crumbs.is_empty() {
        vec![Crumb::current(root)]
    } else {
        std::iter::once(Crumb::link(root, ctx.url("/client")))
            .chain(crumbs)
            .collect()
    };
    let mut panel =
        PanelChrome::build(ctx, "client", "client.area", MENU, |_| true).with_crumbs(trail);
    // Hide what this (sub-)user may not open; sections and items are built
    // in MENU order.
    for (section, def) in panel.sections.iter_mut().zip(MENU) {
        let mut defs = def.items.iter();
        section.items.retain(|_| {
            defs.next()
                .and_then(|d| area_of(d.href))
                .is_none_or(|area| access.allows(area))
        });
    }
    panel.sections.retain(|s| !s.items.is_empty());
    panel
}
