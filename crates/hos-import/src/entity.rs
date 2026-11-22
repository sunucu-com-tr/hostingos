//! What is imported, in which order, from which WHMCS table into which
//! HostingOS table — and what is reported as "skipped: unsupported".

use serde::{Deserialize, Serialize};

/// An imported entity. The order of [`Entity::ORDER`] is the import order
/// (parents before children); rollback deletes in reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Entity {
    Currencies,
    TaxRules,
    ClientGroups,
    Servers,
    Staff,
    Clients,
    Users,
    UserLinks,
    Contacts,
    ClientNotes,
    Credits,
    ProductGroups,
    Products,
    ConfigGroups,
    ConfigLinks,
    ConfigOptions,
    ConfigValues,
    Addons,
    Promotions,
    Tlds,
    Pricing,
    Invoices,
    Orders,
    Services,
    ServiceOptions,
    CustomFieldValues,
    ServiceAddons,
    CancelRequests,
    Domains,
    InvoiceItems,
    Transactions,
    Departments,
    Tickets,
    TicketReplies,
    TicketNotes,
    CannedResponses,
    KbCategories,
    KbArticles,
    Announcements,
    Quotes,
    QuoteItems,
    Affiliates,
    AffiliateAccounts,
    AffiliateHistory,
    AffiliateWithdrawals,
    Todos,
    BannedIps,
    BannedEmails,
    DownloadCategories,
    Downloads,
    NetworkIssues,
    ActivityLog,
}

/// Static facts about an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityInfo {
    /// Stable key (`clients`), also the id-map entity name.
    pub key: &'static str,
    /// WHMCS table (database connector).
    pub table: &'static str,
    /// HostingOS table the id map points into (`""`: merged into a parent,
    /// no own rows).
    pub target: &'static str,
    /// Wizard / report group.
    pub group: &'static str,
    /// Imported unless switched off (`false`: opt-in, e.g. the activity log).
    pub default_on: bool,
}

impl Entity {
    /// Import order.
    pub const ORDER: &'static [Entity] = &[
        Self::Currencies,
        Self::TaxRules,
        Self::ClientGroups,
        Self::Servers,
        Self::Staff,
        Self::Clients,
        Self::Users,
        Self::UserLinks,
        Self::Contacts,
        Self::ClientNotes,
        Self::ProductGroups,
        Self::Products,
        Self::ConfigGroups,
        Self::ConfigLinks,
        Self::ConfigOptions,
        Self::ConfigValues,
        Self::Addons,
        Self::Promotions,
        Self::Tlds,
        Self::Pricing,
        Self::Invoices,
        Self::Credits,
        Self::Orders,
        Self::Services,
        Self::ServiceOptions,
        Self::CustomFieldValues,
        Self::ServiceAddons,
        Self::CancelRequests,
        Self::Domains,
        Self::InvoiceItems,
        Self::Transactions,
        Self::Departments,
        Self::Tickets,
        Self::TicketReplies,
        Self::TicketNotes,
        Self::CannedResponses,
        Self::KbCategories,
        Self::KbArticles,
        Self::Announcements,
        Self::Quotes,
        Self::QuoteItems,
        Self::Affiliates,
        Self::AffiliateAccounts,
        Self::AffiliateHistory,
        Self::AffiliateWithdrawals,
        Self::Todos,
        Self::BannedIps,
        Self::BannedEmails,
        Self::DownloadCategories,
        Self::Downloads,
        Self::NetworkIssues,
        Self::ActivityLog,
    ];

    /// Static facts.
    pub const fn info(self) -> EntityInfo {
        const fn i(
            key: &'static str,
            table: &'static str,
            target: &'static str,
            group: &'static str,
        ) -> EntityInfo {
            EntityInfo {
                key,
                table,
                target,
                group,
                default_on: true,
            }
        }
        match self {
            Self::Currencies => i("currencies", "tblcurrencies", "currencies", "setup"),
            Self::TaxRules => i("tax_rules", "tbltax", "tax_rules", "setup"),
            Self::ClientGroups => i("client_groups", "tblclientgroups", "client_groups", "setup"),
            Self::Servers => i("servers", "tblservers", "integrations", "setup"),
            Self::Staff => i("staff", "tbladmins", "users", "setup"),
            Self::Clients => i("clients", "tblclients", "clients", "clients"),
            Self::Users => i("users", "tblusers", "users", "clients"),
            Self::UserLinks => i("user_links", "tblusers_clients", "", "clients"),
            Self::Contacts => i("contacts", "tblcontacts", "contacts", "clients"),
            Self::ClientNotes => i("client_notes", "tblnotes", "client_notes", "clients"),
            Self::Credits => i("credits", "tblcredit", "credits", "clients"),
            Self::ProductGroups => i(
                "product_groups",
                "tblproductgroups",
                "product_groups",
                "catalog",
            ),
            Self::Products => i("products", "tblproducts", "products", "catalog"),
            Self::ConfigGroups => i(
                "config_groups",
                "tblproductconfiggroups",
                "config_option_groups",
                "catalog",
            ),
            Self::ConfigLinks => i("config_links", "tblproductconfiglinks", "", "catalog"),
            Self::ConfigOptions => i(
                "config_options",
                "tblproductconfigoptions",
                "config_options",
                "catalog",
            ),
            Self::ConfigValues => i(
                "config_values",
                "tblproductconfigoptionssub",
                "config_option_values",
                "catalog",
            ),
            Self::Addons => i("addons", "tbladdons", "product_addons", "catalog"),
            Self::Promotions => i("promotions", "tblpromotions", "coupons", "catalog"),
            Self::Tlds => i("tlds", "tbldomainpricing", "tlds", "catalog"),
            Self::Pricing => i("pricing", "tblpricing", "", "catalog"),
            Self::Invoices => i("invoices", "tblinvoices", "invoices", "billing"),
            Self::Orders => i("orders", "tblorders", "orders", "billing"),
            Self::Services => i("services", "tblhosting", "services", "services"),
            Self::ServiceOptions => i("service_options", "tblhostingconfigoptions", "", "services"),
            Self::CustomFieldValues => i(
                "custom_field_values",
                "tblcustomfieldsvalues",
                "",
                "clients",
            ),
            Self::ServiceAddons => i(
                "service_addons",
                "tblhostingaddons",
                "service_addons",
                "services",
            ),
            Self::CancelRequests => i("cancel_requests", "tblcancelrequests", "", "services"),
            Self::Domains => i("domains", "tbldomains", "domains", "services"),
            Self::InvoiceItems => i(
                "invoice_items",
                "tblinvoiceitems",
                "invoice_items",
                "billing",
            ),
            Self::Transactions => i("transactions", "tblaccounts", "transactions", "billing"),
            Self::Departments => i(
                "departments",
                "tblticketdepartments",
                "ticket_departments",
                "support",
            ),
            Self::Tickets => i("tickets", "tbltickets", "tickets", "support"),
            Self::TicketReplies => i(
                "ticket_replies",
                "tblticketreplies",
                "ticket_messages",
                "support",
            ),
            Self::TicketNotes => i(
                "ticket_notes",
                "tblticketnotes",
                "ticket_messages",
                "support",
            ),
            Self::CannedResponses => i(
                "canned_responses",
                "tblticketpredefinedreplies",
                "canned_responses",
                "support",
            ),
            Self::KbCategories => i(
                "kb_categories",
                "tblknowledgebasecats",
                "kb_categories",
                "content",
            ),
            Self::KbArticles => i("kb_articles", "tblknowledgebase", "kb_articles", "content"),
            Self::Announcements => i(
                "announcements",
                "tblannouncements",
                "announcements",
                "content",
            ),
            Self::Quotes => i("quotes", "tblquotes", "quotes", "billing"),
            Self::QuoteItems => i("quote_items", "tblquoteitems", "quote_items", "billing"),
            Self::Affiliates => i("affiliates", "tblaffiliates", "affiliates", "clients"),
            Self::AffiliateAccounts => i(
                "affiliate_accounts",
                "tblaffiliatesaccounts",
                "affiliate_referrals",
                "clients",
            ),
            Self::AffiliateHistory => i(
                "affiliate_history",
                "tblaffiliateshistory",
                "affiliate_commissions",
                "clients",
            ),
            Self::AffiliateWithdrawals => i(
                "affiliate_withdrawals",
                "tblaffiliateswithdrawals",
                "affiliate_payouts",
                "clients",
            ),
            Self::Todos => i("todos", "tbltodolist", "staff_tasks", "system"),
            Self::BannedIps => i("banned_ips", "tblbannedips", "fraud_bans", "setup"),
            Self::BannedEmails => i("banned_emails", "tblbannedemails", "fraud_bans", "setup"),
            Self::DownloadCategories => i(
                "download_categories",
                "tbldownloadcats",
                "download_categories",
                "content",
            ),
            Self::Downloads => i("downloads", "tbldownloads", "downloads", "content"),
            Self::NetworkIssues => i(
                "network_issues",
                "tblnetworkissues",
                "status_incidents",
                "content",
            ),
            Self::ActivityLog => EntityInfo {
                default_on: false,
                ..i("activity_log", "tblactivitylog", "audit_log", "system")
            },
        }
    }

    /// Stable key.
    pub const fn key(self) -> &'static str {
        self.info().key
    }

    /// Entity by key.
    pub fn parse(key: &str) -> Option<Self> {
        Self::ORDER.iter().copied().find(|e| e.key() == key.trim())
    }
}

/// Id-map entity names of rows written on behalf of another entity
/// (the source id is the parent's).
pub mod derived {
    /// The first message of a ticket (`tbltickets.message`).
    pub const TICKET_FIRST_MESSAGE: &str = "ticket_first_message";
    /// A login created from `tblclients` (WHMCS < 8 has no `tblusers`).
    pub const CLIENT_OWNER_USER: &str = "client_owner_user";
    /// A login created from a WHMCS < 8 contact sub-account.
    pub const CONTACT_USER: &str = "contact_user";
    /// The opening credit balance difference of a client.
    pub const CREDIT_ADJUSTMENT: &str = "credit_adjustment";
    /// Refund rows created from a refund transaction.
    pub const REFUND: &str = "refund";
    /// Hidden addon created for WHMCS custom (addonid = 0) service addons.
    pub const CUSTOM_ADDON: &str = "custom_addon";
    /// Ticket attachments (`<kind>:<id>:<index>`).
    pub const ATTACHMENT: &str = "ticket_attachment";
    /// The admin notes field of a client (`tblclients.notes`), kept as a sticky note.
    pub const CLIENT_ADMIN_NOTE: &str = "client_admin_note";
    /// The part of an affiliate's WHMCS balance its imported history does
    /// not explain (one `available` commission per affiliate).
    pub const AFFILIATE_OPENING_BALANCE: &str = "affiliate_opening_balance";
    /// The timeline update of an imported network issue.
    pub const NETWORK_ISSUE_UPDATE: &str = "network_issue_update";
}

/// WHMCS data with no HostingOS destination: counted and reported as
/// "skipped: unsupported", never silently dropped.
pub const UNSUPPORTED: &[(&str, &str, &str)] = &[
    (
        "email_templates",
        "tblemailtemplates",
        "WHMCS templates use Smarty merge fields that HostingOS templates do not understand; review the HostingOS defaults instead",
    ),
    (
        "pay_methods",
        "tblpaymethods",
        "stored cards / bank accounts are gateway tokens bound to the WHMCS merchant account; clients re-add a payment method",
    ),
    (
        "server_groups",
        "tblservergroups",
        "HostingOS products point at one integration; the group's first server is used",
    ),
    (
        "billable_items",
        "tblbillableitems",
        "HostingOS has no billable items; uninvoiced items must be re-entered",
    ),
    (
        "projects",
        "mod_project",
        "the project management addon has no counterpart",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_is_complete_and_keys_unique() {
        let mut keys: Vec<&str> = Entity::ORDER.iter().map(|e| e.key()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n);
        for e in Entity::ORDER {
            assert_eq!(Entity::parse(e.key()), Some(*e));
            assert!(e.info().table.starts_with("tbl"));
        }
        let pos = |e: Entity| Entity::ORDER.iter().position(|x| *x == e).unwrap();
        // Parents first.
        assert!(pos(Entity::Clients) < pos(Entity::Services));
        assert!(pos(Entity::Invoices) < pos(Entity::Orders));
        assert!(pos(Entity::Orders) < pos(Entity::Services));
        assert!(pos(Entity::Services) < pos(Entity::InvoiceItems));
        assert!(pos(Entity::Domains) < pos(Entity::InvoiceItems));
        assert!(pos(Entity::Products) < pos(Entity::Pricing));
        assert!(pos(Entity::ConfigValues) < pos(Entity::Pricing));
        assert!(pos(Entity::Invoices) < pos(Entity::Transactions));
        assert!(pos(Entity::Tickets) < pos(Entity::TicketReplies));
        assert!(pos(Entity::Quotes) < pos(Entity::QuoteItems));
        assert!(pos(Entity::Services) < pos(Entity::AffiliateAccounts));
        assert!(pos(Entity::Affiliates) < pos(Entity::AffiliateAccounts));
        assert!(pos(Entity::AffiliateAccounts) < pos(Entity::AffiliateHistory));
        assert!(pos(Entity::AffiliateHistory) < pos(Entity::AffiliateWithdrawals));
        assert!(pos(Entity::Staff) < pos(Entity::Todos));
        assert!(pos(Entity::DownloadCategories) < pos(Entity::Downloads));
        assert!(!Entity::ActivityLog.info().default_on);
        // Newly supported tables are no longer reported as unsupported.
        for e in Entity::ORDER {
            assert!(
                !UNSUPPORTED.iter().any(|(_, t, _)| *t == e.info().table),
                "{}",
                e.key()
            );
        }
    }
}
