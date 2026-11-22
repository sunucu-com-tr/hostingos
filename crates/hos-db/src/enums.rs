//! `text` + `CHECK` column types that have no counterpart in `hos_core::domain`.
//!
//! Defined with [`hos_core::string_enum!`], so they decode/bind with sqlx as
//! `text` exactly like the hos-core enums. Each enum's `as_str()` values are the
//! `CHECK` list of the named column(s) — keep both in sync.
//!
//! Reused from hos-core: `TenantStatus`, `UserKind`, `Locale`, `ProductType`,
//! `IntegrationCategory`, `ServiceStatus`, `OrderStatus`, `InvoiceStatus`,
//! `TicketStatus`, `TicketPriority`, `DomainStatus`, `BillingCycle`,
//! `integrations::virt::VmStatus` (`vms.status`),
//! `integrations::registrar::DnsRecordType` (`dns_records.record_type`) and
//! `integrations::gateway::RefundStatus` (`refunds.status`).

use hos_core::string_enum;

string_enum! {
    /// `users.status`, `platform_admins.status`.
    #[derive(Default)]
    pub enum AccountStatus {
        /// Can log in.
        #[default]
        Active => "active",
        /// Temporarily locked (too many failed logins).
        Locked => "locked",
        /// Disabled by staff.
        Disabled => "disabled",
    }
}

string_enum! {
    /// `user_identities.provider`.
    pub enum OAuthProvider {
        /// Google (OpenID Connect).
        Google => "google",
        /// GitHub.
        Github => "github",
    }
}

string_enum! {
    /// `sessions.impersonator_kind`.
    pub enum ImpersonatorKind {
        /// A tenant staff user.
        Staff => "staff",
        /// A platform operator.
        PlatformAdmin => "platform_admin",
    }
}

string_enum! {
    /// `audit_log.actor_kind`, `service_events.actor_kind`.
    pub enum ActorKind {
        /// A tenant user (client or staff).
        User => "user",
        /// A platform operator.
        PlatformAdmin => "platform_admin",
        /// Background job / cron.
        System => "system",
        /// REST API key.
        ApiKey => "api_key",
    }
}

string_enum! {
    /// `tenant_domains.kind`.
    pub enum TenantDomainKind {
        /// `<slug>.<HOS_BASE_DOMAIN>`.
        Subdomain => "subdomain",
        /// A domain owned by the tenant.
        Custom => "custom",
    }
}

string_enum! {
    /// `tenant_domains.tls_status`.
    pub enum TlsStatus {
        /// Certificate not issued yet.
        Pending => "pending",
        /// Certificate active.
        Active => "active",
        /// Issuance failed.
        Failed => "failed",
        /// No certificate needed (localhost / behind a proxy).
        NotRequired => "not_required",
    }
}

string_enum! {
    /// `tenant_subscriptions.status`.
    pub enum SubscriptionStatus {
        /// In trial.
        Trialing => "trialing",
        /// Paid and current.
        Active => "active",
        /// Payment overdue.
        PastDue => "past_due",
        /// Cancelled.
        Cancelled => "cancelled",
        /// Ended.
        Expired => "expired",
    }
}

string_enum! {
    /// `clients.kind`.
    #[derive(Default)]
    pub enum ClientKind {
        /// Private person.
        #[default]
        Individual => "individual",
        /// Company.
        Corporate => "corporate",
    }
}

string_enum! {
    /// `clients.status`.
    #[derive(Default)]
    pub enum ClientStatus {
        /// Active customer.
        #[default]
        Active => "active",
        /// No active services.
        Inactive => "inactive",
        /// Closed account.
        Closed => "closed",
    }
}

string_enum! {
    /// `products.pay_type`, `product_addons.pay_type`.
    #[derive(Default)]
    pub enum PayType {
        /// Free.
        Free => "free",
        /// Paid once.
        OneTime => "one_time",
        /// Recurring.
        #[default]
        Recurring => "recurring",
    }
}

string_enum! {
    /// `products.auto_setup` (WHMCS "module settings → automatic setup").
    #[derive(Default)]
    pub enum AutoSetup {
        /// As soon as the order is placed.
        OnOrder => "on_order",
        /// When the first payment is received.
        #[default]
        OnPayment => "on_payment",
        /// When staff accepts the order.
        OnAccept => "on_accept",
        /// Never automatically.
        Manual => "manual",
    }
}

string_enum! {
    /// `products.requires_domain`.
    #[derive(Default)]
    pub enum DomainRequirement {
        /// No domain asked.
        #[default]
        None => "none",
        /// Domain optional.
        Optional => "optional",
        /// Domain required.
        Required => "required",
    }
}

string_enum! {
    /// `config_options.option_type`.
    pub enum ConfigOptionType {
        /// Select box.
        Dropdown => "dropdown",
        /// Radio buttons.
        Radio => "radio",
        /// Checkbox.
        YesNo => "yes_no",
        /// Numeric quantity.
        Quantity => "quantity",
    }
}

string_enum! {
    /// `coupons.discount_type`.
    pub enum DiscountType {
        /// Percentage off.
        Percent => "percent",
        /// Fixed amount off.
        Fixed => "fixed",
        /// Replaces the price.
        PriceOverride => "price_override",
        /// Waives setup fees.
        FreeSetup => "free_setup",
    }
}

string_enum! {
    /// `cart_items.item_type`, `order_items.item_type`.
    pub enum LineItemType {
        /// A product (creates a service).
        Product => "product",
        /// A product addon.
        Addon => "addon",
        /// Domain registration.
        DomainRegister => "domain_register",
        /// Incoming domain transfer.
        DomainTransfer => "domain_transfer",
        /// Domain renewal.
        DomainRenew => "domain_renew",
        /// Anything else.
        Other => "other",
    }
}

impl LineItemType {
    /// Whether the line is a domain action.
    pub const fn is_domain(&self) -> bool {
        matches!(
            self,
            Self::DomainRegister | Self::DomainTransfer | Self::DomainRenew
        )
    }
}

string_enum! {
    /// `invoice_items.item_type`.
    #[derive(Default)]
    pub enum InvoiceItemType {
        /// Service (product) charge.
        Service => "service",
        /// Service addon charge.
        Addon => "addon",
        /// Domain registration / transfer / renewal.
        Domain => "domain",
        /// One-time setup fee.
        SetupFee => "setup_fee",
        /// Late payment fee.
        LateFee => "late_fee",
        /// Credit applied / added.
        Credit => "credit",
        /// Discount (negative amount).
        Discount => "discount",
        /// Upgrade / downgrade difference.
        Upgrade => "upgrade",
        /// Manually added line.
        #[default]
        Manual => "manual",
    }
}

string_enum! {
    /// `transactions.kind`.
    pub enum TransactionKind {
        /// Money in.
        Payment => "payment",
        /// Money returned to the client.
        Refund => "refund",
        /// Payment reversed by the card network / bank.
        Chargeback => "chargeback",
    }
}

string_enum! {
    /// `transactions.status`.
    pub enum TransactionStatus {
        /// Awaiting confirmation.
        Pending => "pending",
        /// Settled.
        Succeeded => "succeeded",
        /// Failed / declined.
        Failed => "failed",
    }
}

string_enum! {
    /// `credits.kind`.
    pub enum CreditKind {
        /// Added / removed by staff.
        Manual => "manual",
        /// Payment larger than the invoice balance.
        Overpayment => "overpayment",
        /// Refund to credit.
        Refund => "refund",
        /// Credit used to pay an invoice (negative).
        InvoicePayment => "invoice_payment",
        /// Correction of an earlier entry.
        Reversal => "reversal",
        /// Promotional credit.
        Promo => "promo",
        /// Affiliate commission paid out to account credit.
        Affiliate => "affiliate",
    }
}

string_enum! {
    /// `service_events.level`.
    #[derive(Default)]
    pub enum EventLevel {
        /// Informational.
        #[default]
        Info => "info",
        /// Something needs attention.
        Warning => "warning",
        /// Failure.
        Error => "error",
    }
}

string_enum! {
    /// `services.cancellation_type`.
    pub enum CancellationType {
        /// Terminate now.
        Immediate => "immediate",
        /// Terminate at the end of the paid period.
        EndOfPeriod => "end_of_period",
    }
}

string_enum! {
    /// `ip_addresses.status`.
    pub enum IpStatus {
        /// Available.
        Free => "free",
        /// Held for an order in progress.
        Reserved => "reserved",
        /// In use by a service / VM.
        Assigned => "assigned",
        /// Never assign (blacklisted, gateway...).
        Blocked => "blocked",
    }
}

string_enum! {
    /// `tld_pricing.action`.
    pub enum TldAction {
        /// New registration.
        Register => "register",
        /// Incoming transfer.
        Transfer => "transfer",
        /// Renewal.
        Renew => "renew",
        /// Redemption restore.
        Restore => "restore",
    }
}

string_enum! {
    /// `domains.registration_type`.
    pub enum DomainRegistrationType {
        /// Registered through us.
        Register => "register",
        /// Transferred in.
        Transfer => "transfer",
    }
}

string_enum! {
    /// `domain_contacts.role`.
    pub enum ContactRole {
        /// Registrant (owner).
        Registrant => "registrant",
        /// Administrative.
        Admin => "admin",
        /// Technical.
        Tech => "tech",
        /// Billing.
        Billing => "billing",
    }
}

string_enum! {
    /// `ticket_messages.author_kind`, `tickets.last_reply_by`.
    pub enum AuthorKind {
        /// Logged-in client user.
        Client => "client",
        /// Staff.
        Staff => "staff",
        /// Automated message.
        System => "system",
        /// Guest (not logged in, e-mail only).
        Guest => "guest",
    }
}

string_enum! {
    /// `ticket_messages.source`.
    #[derive(Default)]
    pub enum MessageSource {
        /// Web UI.
        #[default]
        Web => "web",
        /// E-mail piping.
        Email => "email",
        /// REST API.
        Api => "api",
    }
}

string_enum! {
    /// `status` of `kb_articles`, `announcements`, `pages`, `blog_posts`.
    #[derive(Default)]
    pub enum ContentStatus {
        /// Not visible.
        #[default]
        Draft => "draft",
        /// Becomes visible at `published_at`.
        Scheduled => "scheduled",
        /// Visible.
        Published => "published",
        /// Hidden but kept.
        Archived => "archived",
    }
}

string_enum! {
    /// `kb_articles.kind`.
    #[derive(Default)]
    pub enum KbArticleKind {
        /// Knowledge base article.
        #[default]
        Article => "article",
        /// Frequently asked question (title = question, body = answer).
        Faq => "faq",
    }
}

string_enum! {
    /// `seo_meta.entity_type`.
    pub enum SeoEntityType {
        /// Storefront home page.
        Home => "home",
        /// Static route (`seo_meta.route`).
        Route => "route",
        /// CMS page.
        Page => "page",
        /// Blog post.
        BlogPost => "blog_post",
        /// Product.
        Product => "product",
        /// Product group.
        ProductGroup => "product_group",
        /// KB article.
        KbArticle => "kb_article",
        /// KB category.
        KbCategory => "kb_category",
        /// Announcement.
        Announcement => "announcement",
        /// TLD landing page.
        Tld => "tld",
    }
}

string_enum! {
    /// `seo_audits.status`.
    pub enum RunStatus {
        /// Queued.
        Pending => "pending",
        /// Running.
        Running => "running",
        /// Done.
        Completed => "completed",
        /// Failed.
        Failed => "failed",
    }
}

string_enum! {
    /// `ai_generations.purpose`.
    pub enum AiPurpose {
        /// Title / description / OG.
        SeoMeta => "seo_meta",
        /// Blog article.
        BlogPost => "blog_post",
        /// Product copy.
        ProductDescription => "product_description",
        /// KB article.
        KbArticle => "kb_article",
        /// FAQ entries.
        Faq => "faq",
        /// tr ↔ en translation.
        Translation => "translation",
        /// Suggested ticket reply.
        TicketReply => "ticket_reply",
        /// Anything else.
        Other => "other",
    }
}

string_enum! {
    /// `ai_generations.status`.
    pub enum AiGenerationStatus {
        /// Request in flight.
        Pending => "pending",
        /// Output available.
        Completed => "completed",
        /// Provider error.
        Failed => "failed",
        /// Output applied to the entity.
        Applied => "applied",
        /// Output rejected by the user.
        Discarded => "discarded",
    }
}

string_enum! {
    /// `jobs.status`.
    pub enum JobStatus {
        /// Waiting for `run_at` / a worker (also used between retries).
        Pending => "pending",
        /// Claimed by a worker.
        Running => "running",
        /// Finished successfully.
        Completed => "completed",
        /// Gave up after `max_attempts` (or a permanent error).
        Failed => "failed",
        /// Cancelled before running.
        Cancelled => "cancelled",
    }
}

string_enum! {
    /// `scheduled_tasks.last_status`.
    pub enum TaskRunStatus {
        /// Last run enqueued / succeeded.
        Ok => "ok",
        /// Last run failed.
        Error => "error",
    }
}

string_enum! {
    /// `email_templates.category`.
    pub enum EmailCategory {
        /// General.
        General => "general",
        /// Client account.
        Client => "client",
        /// Invoices / payments.
        Invoice => "invoice",
        /// Products / services.
        Product => "product",
        /// Domains.
        Domain => "domain",
        /// Support tickets.
        Support => "support",
        /// Authentication (password reset, verification).
        Auth => "auth",
        /// Staff notifications.
        Admin => "admin",
    }
}

string_enum! {
    /// `email_log.status`.
    pub enum EmailStatus {
        /// Waiting to be sent.
        Queued => "queued",
        /// Accepted by the mail server.
        Sent => "sent",
        /// Sending failed.
        Failed => "failed",
        /// Bounced.
        Bounced => "bounced",
    }
}

string_enum! {
    /// `webhooks_in.status`.
    pub enum WebhookInStatus {
        /// Stored, not processed yet.
        Received => "received",
        /// Processed.
        Processed => "processed",
        /// Processing failed (will be retried).
        Failed => "failed",
        /// Irrelevant event.
        Ignored => "ignored",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for s in JobStatus::ALL {
            assert_eq!(s.as_str().parse::<JobStatus>().unwrap(), *s);
        }
        assert_eq!(LineItemType::DomainRegister.as_str(), "domain_register");
        assert!(LineItemType::DomainRenew.is_domain());
        assert!(!LineItemType::Product.is_domain());
        assert_eq!(ContentStatus::default(), ContentStatus::Draft);
    }
}
