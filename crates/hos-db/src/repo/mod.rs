//! Repository functions, one module per table group (docs/ARCHITECTURE.md §5).
//!
//! | Module | Tables |
//! |---|---|
//! | [`platform`] | tenants, tenant_domains, tenant_settings, platform_admins, platform_plans, tenant_subscriptions |
//! | [`identity`] | users, user_identities, sessions, roles, role_permissions, user_roles, totp_secrets, password_resets, email_verifications, api_keys |
//! | [`audit`] | audit_log |
//! | [`clients`] | clients, client_users, contacts, client_notes, client_groups |
//! | [`catalog`] | currencies, tax_rules, product_groups, products, product_pricing, config options, addons, coupons |
//! | [`integrations`] | integrations |
//! | [`carts`] | carts, cart_items |
//! | [`orders`] | orders, order_items |
//! | [`invoices`] | invoices, invoice_items, invoice_tax_lines, invoice_sequences |
//! | [`payments`] | transactions, credits, refunds |
//! | [`services`] | services, service_addons, service_events, vms, ip_pools, ip_addresses |
//! | [`domains`] | tlds, tld_pricing, domains, domain_contacts, dns_zones, dns_records |
//! | [`support`] | ticket_departments, tickets, ticket_messages, ticket_attachments, canned_responses |
//! | [`content`] | kb_categories, kb_articles, announcements, pages, blog_posts, redirects |
//! | [`demo`] | the public demo's reset and startup check (docs/DEMO.md) |
//! | [`seo`] | seo_meta, seo_audits, ai_generations |
//! | [`jobs`] | jobs, scheduled_tasks |
//! | [`mail`] | email_templates, email_log |
//! | [`webhooks`] | webhooks_in, webhook_endpoints |
//! | [`notifications`] | notifications |
//! | [`stats`] | admin dashboard aggregates |
//! | [`billing_support`] | payment_attempts, payment_methods, service_upgrades + billing-engine lookups |
//! | [`reports`] | revenue, churn, tax, aging, best sellers |
//! | [`saas`] | platform console: tenant provisioning, platform settings, plans / subscriptions admin, usage + plan limits, sign-ups, hand-offs, custom-domain checks |
//! | [`idempotency`] | api_idempotency_keys (REST API `Idempotency-Key`) |
//! | [`api`] | REST API listings with whitelisted sort keys, webhook endpoint delete |
//! | [`admin_settings`] | staff_invites, tenant_assets + admin settings / staff / developer / report queries |
//! | [`import`] | import_runs, import_id_map, import_run_messages (hos-import), rollback, cut-over helpers |
//! | [`import_write`] | entity upserts of the data import (silent: no jobs, mails, webhooks) |
//! | [`einvoice`] | einvoice_documents, einvoice_sequences (Turkish e-Fatura / e-Arşiv) |
//! | [`quotes`] | quotes, quote_items |
//! | [`affiliates`] | affiliates, affiliate_product_rates, affiliate_clicks, affiliate_referrals, affiliate_commissions, affiliate_payouts |
//! | [`status`] | status_components, status_incidents, status_incident_components, status_incident_updates |
//! | [`tasks`] | staff_tasks |
//! | [`fraud`] | fraud_bans (+ order velocity signals) |
//! | [`vm_ops`] | vm_console_sessions, vm_operations, client_ssh_keys, vm_iso_images (+ IP PTR updates) |
//! | [`mass_pay`] | mass_payments, mass_payment_items (one invoice paying several) |
//! | [`downloads`] | download_categories, downloads, download_files, download_products |
//! | [`bundles`] | product_bundles, product_bundle_items (+ their coupons) |
//! | [`mail_inbound`] | mail_inbound_settings, ticket_reply_tokens, mail_inbound_messages (e-mail piping) |
//! | [`updates`] | update_state, update_releases, update_runs, update_trusted_keys, platform_notifications (self-update, maintenance flag, update advisory lock) |

pub mod admin;
pub mod admin_settings;
pub mod affiliates;
pub mod api;
pub mod audit;
pub mod billing_support;
pub mod bundles;
pub mod carts;
pub mod catalog;
pub mod clients;
pub mod content;
pub mod demo;
pub mod domains;
pub mod downloads;
pub mod einvoice;
pub mod fraud;
pub mod idempotency;
pub mod identity;
pub mod import;
pub mod import_write;
pub mod integrations;
pub mod invoices;
pub mod jobs;
pub mod mail;
pub mod mail_inbound;
pub mod mass_pay;
pub mod notifications;
pub mod orders;
pub mod payments;
pub mod platform;
pub mod quotes;
pub mod reports;
pub mod saas;
pub mod seo;
pub mod services;
pub mod stats;
pub mod status;
pub mod support;
pub mod tasks;
pub mod updates;
pub mod vm_ops;
pub mod webhooks;

/// `serde_json::Value` alias used for `jsonb` columns.
pub type Json = serde_json::Value;

/// `'{}'::jsonb`.
pub(crate) fn empty_object() -> Json {
    Json::Object(serde_json::Map::new())
}
