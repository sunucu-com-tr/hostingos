//! HostingOS billing automation core (a WHMCS-style engine):
//!
//! | module | what |
//! |---|---|
//! | [`registry`] | builds integrations from `integrations` rows (decrypted secrets, TTL cache) |
//! | [`pricing`] | cart → [`pricing::Quote`] (cycles, config options, addons, domains, coupons, taxes) |
//! | [`checkout`] | cart → order + services/domains + invoice in one transaction; order acceptance |
//! | [`payments`] | hosted checkout, gateway webhooks, manual payments, credit, refunds, chargebacks |
//! | [`provisioning`] | panel / virt / registrar provisioning jobs (idempotent) |
//! | [`lifecycle`] | suspend / unsuspend / terminate / plan change / cancellation / domain & VM actions |
//! | [`automation`] | daily cron: renewal invoices, reminders, late fees, suspension, termination, syncs |
//! | [`notify`] | e-mail (templates + layout + `email_log`), in-app notifications, outgoing webhooks |
//! | [`reports`] | revenue, MRR/ARR/churn, tax, aging, top sellers |
//! | [`jobs`] | typed job kinds, payloads and `enqueue_*` helpers (handlers live in `hos-jobs`) |
//! | [`einvoice`] | Turkish e-Fatura / e-Arşiv documents (`hos-einvoice` providers) |
//! | [`invoice_pdf`] | invoice PDFs (`hos-pdf`) and e-mail attachments |
//! | [`quotes`] | quotes: pricing, accept link, conversion into order + invoice, PDF, expiry |
//! | [`affiliates`] | affiliate programme: attribution, commissions, clearance / reversal, payouts |
//! | [`network`] | network status notices to the clients of affected services |
//! | [`funds`] | limits of client credit top-ups |
//! | [`fraud`] | built-in order screening rules (ban list, disposable e-mail, velocity) |
//! | [`vm_ops`] | VPS management: console sessions, password reset, backups, rDNS, ISO, rescue, firewall, metrics, bandwidth, SSH keys, operation log |
//!
//! Entry point: [`Billing`] (cheap to clone). Everything time-dependent goes
//! through its [`Clock`], so tests can drive the calendar with [`ManualClock`].

pub mod affiliates;
pub mod automation;
pub mod checkout;
pub mod clock;
pub mod context;
pub mod einvoice;
pub mod format;
pub mod fraud;
pub mod funds;
pub mod invoice_pdf;
pub mod jobs;
pub mod lifecycle;
pub mod mass_pay;
pub mod network;
pub mod notify;
pub mod payments;
pub mod pricing;
pub mod provisioning;
pub mod quotes;
pub mod registry;
pub mod reports;
pub mod secrets;
pub mod settings;
pub mod vm_ops;

pub use checkout::{CheckoutOutcome, CheckoutRequest};
pub use clock::{Clock, ManualClock, SystemClock};
pub use context::{Billing, BillingConfig, FraudCheck, FraudVerdict, NoFraudCheck};
pub use payments::{StartPayment, StartedPayment, VerifiedWebhook, WebhookResult};
pub use pricing::{Quote, QuoteItem, QuoteRequest};
pub use registry::{IntegrationRegistry, LoadedIntegration, SaveIntegration};
pub use settings::BillingSettings;

/// Result type of the billing engine.
pub type BillingResult<T> = Result<T, hos_core::CoreError>;
