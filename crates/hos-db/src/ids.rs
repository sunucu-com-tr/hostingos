//! Id newtypes for tables that have no id type in `hos_core::ids`.
//!
//! Generated with [`hos_core::define_id!`], so they behave exactly like the
//! hos-core ids (UUID v7, serde-transparent, sqlx `uuid`).

macro_rules! db_ids {
    ($($(#[$meta:meta])* $name:ident;)+) => {
        $(hos_core::define_id! { $(#[$meta])* pub struct $name; })+
    };
}

db_ids! {
    /// `tenant_settings`.
    TenantSettingId;
    /// `user_identities`.
    UserIdentityId;
    /// `role_permissions`.
    RolePermissionId;
    /// `user_roles`.
    UserRoleId;
    /// `totp_secrets`.
    TotpSecretId;
    /// `password_resets`.
    PasswordResetId;
    /// `email_verifications`.
    EmailVerificationId;
    /// `client_users`.
    ClientUserId;
    /// `client_notes`.
    ClientNoteId;
    /// `currencies`.
    CurrencyId;
    /// `product_pricing`.
    ProductPricingId;
    /// `config_option_values`.
    ConfigOptionValueId;
    /// `config_option_pricing`.
    ConfigOptionPricingId;
    /// `product_addon_pricing`.
    ProductAddonPricingId;
    /// `cart_items`.
    CartItemId;
    /// `invoice_sequences`.
    InvoiceSequenceId;
    /// `invoice_tax_lines`.
    InvoiceTaxLineId;
    /// `service_events`.
    ServiceEventId;
    /// `tld_pricing`.
    TldPricingId;
    /// `domain_contacts`.
    DomainContactId;
    /// `ticket_attachments`.
    TicketAttachmentId;
    /// `canned_responses`.
    CannedResponseId;
    /// `kb_categories`.
    KbCategoryId;
    /// `seo_meta`.
    SeoMetaId;
    /// `redirects`.
    RedirectId;
    /// `seo_audits`.
    SeoAuditId;
    /// `ai_generations`.
    AiGenerationId;
    /// `scheduled_tasks`.
    ScheduledTaskId;
    /// `email_log`.
    EmailLogId;
}
