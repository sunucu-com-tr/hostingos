//! Business-domain enums.
//!
//! All are [`string_enum!`](crate::string_enum)s: stored as `text` with a `CHECK`
//! constraint (docs/ARCHITECTURE.md §5), serialized as their `as_str()` value.
//! When writing a migration, the allowed values are exactly `Type::ALL`
//! mapped through `as_str()`.
//!
//! State machines ([`ServiceStatus`], [`InvoiceStatus`], [`OrderStatus`],
//! [`TicketStatus`], [`DomainStatus`]) expose `can_transition_to` and a
//! `transition` helper that returns [`CoreError::InvalidTransition`].
//! Self-transitions are always rejected (callers treat "already in that state"
//! as a no-op before asking).

use chrono::NaiveDate;

use crate::error::CoreError;

crate::string_enum! {
    /// UI / content language. Only Turkish and English are supported (§1 i18n).
    /// Defaults to Turkish.
    #[derive(Default)]
    pub enum Locale {
        /// Turkish.
        #[default]
        Tr => "tr",
        /// English.
        En => "en",
    }
}

impl Locale {
    /// Picks the best supported locale from an `Accept-Language` header value
    /// (`"en-US,en;q=0.9,tr;q=0.8"` → `En`). `None` when nothing matches.
    pub fn negotiate(accept_language: &str) -> Option<Locale> {
        let mut best: Option<(Locale, f32)> = None;
        for part in accept_language.split(',') {
            let mut pieces = part.trim().split(';');
            let tag = pieces.next().unwrap_or_default().trim();
            let q = pieces
                .find_map(|p| p.trim().strip_prefix("q="))
                .and_then(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            let primary = tag.split(['-', '_']).next().unwrap_or_default();
            let Ok(locale) = primary.to_ascii_lowercase().parse::<Locale>() else {
                continue;
            };
            if q > 0.0 && best.is_none_or(|(_, bq)| q > bq) {
                best = Some((locale, q));
            }
        }
        best.map(|(l, _)| l)
    }
}

crate::string_enum! {
    /// Lifecycle of a tenant (a hosting company) on the platform.
    pub enum TenantStatus {
        /// Trial period.
        Trial => "trial",
        /// Paying / active.
        Active => "active",
        /// Suspended by the platform (unpaid, abuse); storefront offline.
        Suspended => "suspended",
        /// Cancelled; data retained until purge.
        Cancelled => "cancelled",
    }
}

impl TenantStatus {
    /// Whether the tenant's storefront/panels should serve requests.
    pub const fn is_operational(&self) -> bool {
        matches!(self, Self::Trial | Self::Active)
    }
}

crate::string_enum! {
    /// `users.kind`.
    pub enum UserKind {
        /// A customer of the hosting company (client area).
        Client => "client",
        /// Hosting company staff (admin area, RBAC).
        Staff => "staff",
    }
}

crate::string_enum! {
    /// `products.product_type` (§4). Drives which integration category provisions it.
    pub enum ProductType {
        /// Shared web hosting (cPanel/Plesk/DirectAdmin account).
        SharedHosting => "shared_hosting",
        /// Reseller hosting account.
        ResellerHosting => "reseller_hosting",
        /// Virtual private server.
        Vps => "vps",
        /// Dedicated server.
        Dedicated => "dedicated",
        /// Cloud instance (hourly/elastic).
        Cloud => "cloud",
        /// Domain name.
        Domain => "domain",
        /// SSL certificate.
        Ssl => "ssl",
        /// Software license.
        License => "license",
        /// Hosted email.
        Email => "email",
        /// AI service (API credits, assistants).
        AiService => "ai_service",
        /// Managed database.
        ManagedDb => "managed_db",
        /// Object storage (S3-compatible).
        ObjectStorage => "object_storage",
        /// Container application hosting.
        ContainerApp => "container_app",
        /// Anything else (manual fulfilment).
        Other => "other",
    }
}

impl ProductType {
    /// "Next generation" product types (§4).
    pub const fn is_next_gen(&self) -> bool {
        matches!(
            self,
            Self::Cloud
                | Self::AiService
                | Self::ManagedDb
                | Self::ObjectStorage
                | Self::ContainerApp
        )
    }

    /// Integration category that normally provisions this product type, if any.
    pub const fn default_integration_category(&self) -> Option<IntegrationCategory> {
        match self {
            Self::SharedHosting | Self::ResellerHosting | Self::Email => {
                Some(IntegrationCategory::Panel)
            }
            Self::Vps | Self::Dedicated | Self::Cloud => Some(IntegrationCategory::Virt),
            Self::Domain => Some(IntegrationCategory::Registrar),
            Self::AiService => Some(IntegrationCategory::Ai),
            Self::Ssl
            | Self::License
            | Self::ManagedDb
            | Self::ObjectStorage
            | Self::ContainerApp
            | Self::Other => None,
        }
    }
}

crate::string_enum! {
    /// `integrations.category` (§4).
    pub enum IntegrationCategory {
        /// Virtualization ([`VirtProvider`](crate::integrations::virt::VirtProvider)).
        Virt => "virt",
        /// Hosting control panel ([`HostingPanel`](crate::integrations::panel::HostingPanel)).
        Panel => "panel",
        /// Domain registrar ([`DomainRegistrar`](crate::integrations::registrar::DomainRegistrar)).
        Registrar => "registrar",
        /// Payment gateway ([`PaymentGateway`](crate::integrations::gateway::PaymentGateway)).
        Gateway => "gateway",
        /// Outgoing email ([`Mailer`](crate::integrations::mailer::Mailer)).
        Mail => "mail",
        /// AI text generation ([`AiProvider`](crate::integrations::ai::AiProvider)).
        Ai => "ai",
        /// A tenant's own OAuth application for social login (`kind` is
        /// [`kinds::GOOGLE`](crate::integrations::kinds::GOOGLE) or
        /// [`kinds::GITHUB`](crate::integrations::kinds::GITHUB)); used instead of
        /// the platform default `GOOGLE_CLIENT_*` / `GITHUB_CLIENT_*` apps.
        Oauth => "oauth",
        /// Turkish e-invoicing (e-Fatura / e-Arşiv,
        /// [`EInvoiceProvider`](crate::integrations::einvoice::EInvoiceProvider)).
        Einvoice => "einvoice",
    }
}

/// Returns `Ok(next)` when `allowed`, otherwise [`CoreError::InvalidTransition`].
fn checked_transition<T: Copy>(
    entity: &'static str,
    from: &'static str,
    to_str: &'static str,
    to: T,
    allowed: bool,
) -> Result<T, CoreError> {
    if allowed {
        Ok(to)
    } else {
        Err(CoreError::InvalidTransition {
            entity,
            from,
            to: to_str,
        })
    }
}

crate::string_enum! {
    /// `services.status` (§4): `pending → provisioning → active → suspended →
    /// terminated | cancelled | failed`.
    pub enum ServiceStatus {
        /// Ordered; waiting for payment / acceptance.
        Pending => "pending",
        /// A provisioning job is running.
        Provisioning => "provisioning",
        /// Live and billable.
        Active => "active",
        /// Temporarily disabled (overdue, abuse); still billable.
        Suspended => "suspended",
        /// Removed from the provider. Terminal.
        Terminated => "terminated",
        /// Cancelled (by client or admin) without/after provisioning. Terminal.
        Cancelled => "cancelled",
        /// Provisioning failed; needs a retry or manual action.
        Failed => "failed",
    }
}

impl ServiceStatus {
    /// Allowed lifecycle transitions:
    ///
    /// | from | to |
    /// |---|---|
    /// | pending | provisioning, active (manual products), cancelled, failed |
    /// | provisioning | active, failed, cancelled |
    /// | active | suspended, terminated, cancelled, provisioning (reinstall/upgrade re-run) |
    /// | suspended | active, terminated, cancelled |
    /// | failed | provisioning (retry), active (fixed manually), cancelled, terminated |
    /// | terminated, cancelled | — (terminal) |
    pub const fn can_transition_to(&self, next: ServiceStatus) -> bool {
        use ServiceStatus::*;
        matches!(
            (self, next),
            (Pending, Provisioning | Active | Cancelled | Failed)
                | (Provisioning, Active | Failed | Cancelled)
                | (Active, Suspended | Terminated | Cancelled | Provisioning)
                | (Suspended, Active | Terminated | Cancelled)
                | (Failed, Provisioning | Active | Cancelled | Terminated)
        )
    }

    /// `Ok(next)` if allowed, else [`CoreError::InvalidTransition`].
    pub fn transition(self, next: ServiceStatus) -> Result<ServiceStatus, CoreError> {
        checked_transition(
            "service",
            self.as_str(),
            next.as_str(),
            next,
            self.can_transition_to(next),
        )
    }

    /// `terminated` or `cancelled`.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminated | Self::Cancelled)
    }

    /// Whether renewal invoices are generated (`active` or `suspended`).
    pub const fn is_billable(&self) -> bool {
        matches!(self, Self::Active | Self::Suspended)
    }
}

crate::string_enum! {
    /// `orders.status`.
    pub enum OrderStatus {
        /// Awaiting payment and/or staff review.
        Pending => "pending",
        /// Accepted: services/domains created and queued for provisioning.
        Accepted => "accepted",
        /// Cancelled. Terminal.
        Cancelled => "cancelled",
        /// Flagged as fraud. Terminal (unless staff re-opens to pending).
        Fraud => "fraud",
    }
}

impl OrderStatus {
    /// pending → accepted | cancelled | fraud; accepted → cancelled; fraud → pending | cancelled.
    pub const fn can_transition_to(&self, next: OrderStatus) -> bool {
        use OrderStatus::*;
        matches!(
            (self, next),
            (Pending, Accepted | Cancelled | Fraud)
                | (Accepted, Cancelled)
                | (Fraud, Pending | Cancelled)
        )
    }

    /// `Ok(next)` if allowed, else [`CoreError::InvalidTransition`].
    pub fn transition(self, next: OrderStatus) -> Result<OrderStatus, CoreError> {
        checked_transition(
            "order",
            self.as_str(),
            next.as_str(),
            next,
            self.can_transition_to(next),
        )
    }
}

crate::string_enum! {
    /// `invoices.status`. "Overdue" is not a status: use [`InvoiceStatus::is_overdue`].
    pub enum InvoiceStatus {
        /// Being prepared; invisible to the client.
        Draft => "draft",
        /// Issued and awaiting payment.
        Unpaid => "unpaid",
        /// Payment initiated but not yet confirmed (bank transfer, async gateway).
        PaymentPending => "payment_pending",
        /// Fully paid.
        Paid => "paid",
        /// Partially refunded after payment.
        PartiallyRefunded => "partially_refunded",
        /// Fully refunded.
        Refunded => "refunded",
        /// Voided. Terminal.
        Cancelled => "cancelled",
        /// Handed over to debt collection.
        Collections => "collections",
    }
}

impl InvoiceStatus {
    /// Allowed transitions:
    ///
    /// | from | to |
    /// |---|---|
    /// | draft | unpaid, cancelled |
    /// | unpaid | payment_pending, paid, cancelled, collections |
    /// | payment_pending | paid, unpaid (payment failed), cancelled |
    /// | paid | partially_refunded, refunded, unpaid (chargeback) |
    /// | partially_refunded | refunded, unpaid (chargeback) |
    /// | collections | paid, cancelled |
    /// | refunded, cancelled | — |
    pub const fn can_transition_to(&self, next: InvoiceStatus) -> bool {
        use InvoiceStatus::*;
        matches!(
            (self, next),
            (Draft, Unpaid | Cancelled)
                | (Unpaid, PaymentPending | Paid | Cancelled | Collections)
                | (PaymentPending, Paid | Unpaid | Cancelled)
                | (Paid, PartiallyRefunded | Refunded | Unpaid)
                | (PartiallyRefunded, Refunded | Unpaid)
                | (Collections, Paid | Cancelled)
        )
    }

    /// `Ok(next)` if allowed, else [`CoreError::InvalidTransition`].
    pub fn transition(self, next: InvoiceStatus) -> Result<InvoiceStatus, CoreError> {
        checked_transition(
            "invoice",
            self.as_str(),
            next.as_str(),
            next,
            self.can_transition_to(next),
        )
    }

    /// Still expecting money (`unpaid`, `payment_pending`, `collections`).
    pub const fn is_open(&self) -> bool {
        matches!(
            self,
            Self::Unpaid | Self::PaymentPending | Self::Collections
        )
    }

    /// Open and past `due_date` as of `today`.
    pub fn is_overdue(&self, due_date: NaiveDate, today: NaiveDate) -> bool {
        self.is_open() && today > due_date
    }
}

crate::string_enum! {
    /// `tickets.status`.
    pub enum TicketStatus {
        /// New, waiting for staff.
        Open => "open",
        /// Staff replied; waiting for the client.
        Answered => "answered",
        /// Client replied; waiting for staff.
        CustomerReply => "customer_reply",
        /// Staff is working on it.
        InProgress => "in_progress",
        /// Parked (waiting on a third party).
        OnHold => "on_hold",
        /// Closed. A new client reply re-opens it (`customer_reply`).
        Closed => "closed",
    }
}

impl TicketStatus {
    /// Any status may move to any other except: `closed` can only be re-opened
    /// to `open` or `customer_reply`.
    pub const fn can_transition_to(&self, next: TicketStatus) -> bool {
        use TicketStatus::*;
        match (self, next) {
            (a, b) if *a as u8 == b as u8 => false,
            (Closed, Open | CustomerReply) => true,
            (Closed, _) => false,
            _ => true,
        }
    }

    /// `Ok(next)` if allowed, else [`CoreError::InvalidTransition`].
    pub fn transition(self, next: TicketStatus) -> Result<TicketStatus, CoreError> {
        checked_transition(
            "ticket",
            self.as_str(),
            next.as_str(),
            next,
            self.can_transition_to(next),
        )
    }

    /// Not closed.
    pub const fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }

    /// The ball is in staff's court (`open`, `customer_reply`, `in_progress`).
    pub const fn awaits_staff(&self) -> bool {
        matches!(self, Self::Open | Self::CustomerReply | Self::InProgress)
    }
}

crate::string_enum! {
    /// `tickets.priority`. Defaults to `medium`.
    #[derive(Default)]
    pub enum TicketPriority {
        /// Low.
        Low => "low",
        /// Medium (default).
        #[default]
        Medium => "medium",
        /// High.
        High => "high",
        /// Urgent (outage).
        Urgent => "urgent",
    }
}

crate::string_enum! {
    /// `domains.status`.
    pub enum DomainStatus {
        /// Ordered, not yet submitted to the registrar.
        Pending => "pending",
        /// Registration submitted, awaiting completion.
        PendingRegistration => "pending_registration",
        /// Incoming transfer in progress.
        PendingTransfer => "pending_transfer",
        /// Registered and active.
        Active => "active",
        /// Expired, still renewable at normal price (registrar grace period).
        Grace => "grace",
        /// In redemption; restore fee applies.
        Redemption => "redemption",
        /// Expired and no longer renewable through us.
        Expired => "expired",
        /// Transferred to another registrar. Terminal.
        TransferredAway => "transferred_away",
        /// Cancelled. Terminal.
        Cancelled => "cancelled",
        /// Flagged as fraud. Terminal.
        Fraud => "fraud",
    }
}

impl DomainStatus {
    /// Allowed transitions (registrar sync may also move between expiry states):
    ///
    /// | from | to |
    /// |---|---|
    /// | pending | pending_registration, pending_transfer, active, cancelled, fraud |
    /// | pending_registration, pending_transfer | active, cancelled, pending |
    /// | active | grace, expired, transferred_away, cancelled |
    /// | grace | active, redemption, expired, cancelled |
    /// | redemption | active, expired, cancelled |
    /// | expired | active (late renewal/restore), cancelled |
    /// | transferred_away, cancelled, fraud | — |
    pub const fn can_transition_to(&self, next: DomainStatus) -> bool {
        use DomainStatus::*;
        matches!(
            (self, next),
            (
                Pending,
                PendingRegistration | PendingTransfer | Active | Cancelled | Fraud
            ) | (
                PendingRegistration | PendingTransfer,
                Active | Cancelled | Pending
            ) | (Active, Grace | Expired | TransferredAway | Cancelled)
                | (Grace, Active | Redemption | Expired | Cancelled)
                | (Redemption, Active | Expired | Cancelled)
                | (Expired, Active | Cancelled)
        )
    }

    /// `Ok(next)` if allowed, else [`CoreError::InvalidTransition`].
    pub fn transition(self, next: DomainStatus) -> Result<DomainStatus, CoreError> {
        checked_transition(
            "domain",
            self.as_str(),
            next.as_str(),
            next,
            self.can_transition_to(next),
        )
    }

    /// Whether the domain can still be renewed (`active`, `grace`, `redemption`).
    pub const fn is_renewable(&self) -> bool {
        matches!(self, Self::Active | Self::Grace | Self::Redemption)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_roundtrip_for_all_enums() {
        fn check<T>(all: &[T])
        where
            T: Copy + PartialEq + std::fmt::Debug + std::str::FromStr + std::fmt::Display,
            <T as std::str::FromStr>::Err: std::fmt::Debug,
        {
            for v in all {
                assert_eq!(v.to_string().parse::<T>().unwrap(), *v);
            }
        }
        check(ServiceStatus::ALL);
        check(InvoiceStatus::ALL);
        check(OrderStatus::ALL);
        check(TicketStatus::ALL);
        check(TicketPriority::ALL);
        check(DomainStatus::ALL);
        check(ProductType::ALL);
        check(IntegrationCategory::ALL);
        check(Locale::ALL);
        check(TenantStatus::ALL);
        check(UserKind::ALL);
        assert_eq!(ProductType::ManagedDb.as_str(), "managed_db");
        assert!("Active".parse::<ServiceStatus>().is_err());
        let json = serde_json::to_string(&ServiceStatus::Provisioning).unwrap();
        assert_eq!(json, "\"provisioning\"");
        assert!(serde_json::from_str::<ServiceStatus>("\"bogus\"").is_err());
    }

    #[test]
    fn service_lifecycle() {
        use ServiceStatus::*;
        let path = [Pending, Provisioning, Active, Suspended, Active, Terminated];
        let mut cur = path[0];
        for next in &path[1..] {
            cur = cur.transition(*next).unwrap();
        }
        assert_eq!(cur, Terminated);
        assert!(Provisioning.can_transition_to(Failed));
        assert!(Failed.can_transition_to(Provisioning));
        assert!(Pending.can_transition_to(Cancelled));
        assert!(Suspended.can_transition_to(Cancelled));
    }

    #[test]
    fn service_invalid_transitions() {
        use ServiceStatus::*;
        for terminal in [Terminated, Cancelled] {
            assert!(terminal.is_terminal());
            for next in ServiceStatus::ALL {
                assert!(!terminal.can_transition_to(*next), "{terminal} -> {next}");
            }
        }
        for s in ServiceStatus::ALL {
            assert!(!s.can_transition_to(*s), "self transition {s}");
        }
        assert!(!Pending.can_transition_to(Suspended));
        assert!(!Pending.can_transition_to(Terminated));
        assert!(!Suspended.can_transition_to(Provisioning));
        let err = Pending.transition(Suspended).unwrap_err();
        assert!(matches!(
            err,
            CoreError::InvalidTransition {
                entity: "service",
                from: "pending",
                to: "suspended"
            }
        ));
        assert_eq!(err.http_status(), 409);
    }

    #[test]
    fn invoice_transitions() {
        use InvoiceStatus::*;
        assert!(Draft.can_transition_to(Unpaid));
        assert!(Unpaid.can_transition_to(Paid));
        assert!(Paid.can_transition_to(Refunded));
        assert!(Paid.can_transition_to(Unpaid));
        assert!(!Paid.can_transition_to(Draft));
        assert!(!Cancelled.can_transition_to(Paid));
        assert!(!Refunded.can_transition_to(Paid));
        let due = NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 1, 11).unwrap();
        assert!(Unpaid.is_overdue(due, today));
        assert!(!Unpaid.is_overdue(due, due));
        assert!(!Paid.is_overdue(due, today));
    }

    #[test]
    fn order_ticket_domain_transitions() {
        assert!(OrderStatus::Pending.can_transition_to(OrderStatus::Accepted));
        assert!(!OrderStatus::Cancelled.can_transition_to(OrderStatus::Pending));
        assert!(TicketStatus::Closed.can_transition_to(TicketStatus::CustomerReply));
        assert!(!TicketStatus::Closed.can_transition_to(TicketStatus::Answered));
        assert!(TicketStatus::Open.can_transition_to(TicketStatus::Closed));
        assert!(!TicketStatus::Open.can_transition_to(TicketStatus::Open));
        assert!(DomainStatus::Active.can_transition_to(DomainStatus::Grace));
        assert!(DomainStatus::Grace.can_transition_to(DomainStatus::Redemption));
        assert!(!DomainStatus::TransferredAway.can_transition_to(DomainStatus::Active));
        assert!(DomainStatus::Redemption.is_renewable());
    }

    #[test]
    fn product_type_helpers() {
        assert!(ProductType::Cloud.is_next_gen());
        assert!(!ProductType::Vps.is_next_gen());
        assert_eq!(
            ProductType::Vps.default_integration_category(),
            Some(IntegrationCategory::Virt)
        );
        assert_eq!(
            ProductType::SharedHosting.default_integration_category(),
            Some(IntegrationCategory::Panel)
        );
        assert_eq!(ProductType::Other.default_integration_category(), None);
    }

    #[test]
    fn locale_negotiation() {
        assert_eq!(
            Locale::negotiate("en-US,en;q=0.9,tr;q=0.8"),
            Some(Locale::En)
        );
        assert_eq!(
            Locale::negotiate("de-DE,tr;q=0.5,en;q=0.4"),
            Some(Locale::Tr)
        );
        assert_eq!(Locale::negotiate("TR-tr"), Some(Locale::Tr));
        assert_eq!(Locale::negotiate("de, fr;q=0.8"), None);
        assert_eq!(Locale::negotiate("en;q=0"), None);
        assert_eq!(Locale::negotiate(""), None);
        assert_eq!(Locale::default(), Locale::Tr);
    }
}
