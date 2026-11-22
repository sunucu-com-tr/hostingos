//! Idempotent seed data (`hos-migrate --seed`).
//!
//! * [`run`] — everything below for the `demo` tenant, in one transaction.
//! * [`seed_platform_plans`] — Starter / Growth / Scale SaaS plans.
//! * [`seed_tenant_defaults`] — what every new tenant needs: currencies (TRY,
//!   USD, EUR), the TR KDV %20 tax rule, invoice numbering, default roles and
//!   permissions, ticket departments, base settings and the tr+en e-mail
//!   templates, and the default storefront pages ([`site`]: home / domains /
//!   contact copy, about, infrastructure, hosting types, status, legal
//!   templates and category landings as drafts). The platform's "create
//!   tenant" flow calls it too.
//! * demo-only data: product catalog (shared hosting, VPS, dedicated and
//!   next-gen products) with prices, configurable options and addons, 15 TLDs
//!   with prices, knowledge base articles + FAQ, an announcement and CMS pages.
//!
//! Every insert is keyed on a natural unique key and uses `ON CONFLICT DO
//! NOTHING` (or `WHERE NOT EXISTS`), so re-running changes nothing and never
//! overwrites edits made through the admin UI. Users are **not** seeded
//! (password hashing lives in hos-auth).

mod catalog;
mod content;
pub mod site;

use hos_core::domain::{Locale, TenantStatus};
use hos_core::ids::{PlatformPlanId, RoleId, TenantDomainId, TenantId};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

use crate::error::DbResult;

/// Slug of the demo tenant.
pub const DEMO_TENANT_SLUG: &str = "demo";

/// Permission catalogue used by the default roles (`area.action`; `*` = all).
pub const PERMISSIONS: &[&str] = &[
    "clients.view",
    "clients.manage",
    "billing.view",
    "billing.manage",
    "products.manage",
    "services.view",
    "services.manage",
    "domains.manage",
    "support.view",
    "support.manage",
    "content.manage",
    "seo.manage",
    "integrations.manage",
    "settings.manage",
    "staff.manage",
    "reports.view",
    "quotes.view",
    "quotes.manage",
    "affiliates.view",
    "affiliates.manage",
    "status.manage",
    "tasks.manage",
    "fraud.manage",
];

/// Default roles: `(slug, tr name, description, permissions)`.
pub const DEFAULT_ROLES: &[(&str, &str, &str, &[&str])] = &[
    ("owner", "Sahip", "Şirket sahibi — tüm yetkiler", &["*"]),
    (
        "admin",
        "Yönetici",
        "Personel yönetimi dahil tüm yönetim işlemleri",
        PERMISSIONS,
    ),
    (
        "billing",
        "Muhasebe",
        "Faturalar, ödemeler ve iadeler",
        &[
            "clients.view",
            "billing.view",
            "billing.manage",
            "services.view",
            "reports.view",
            "quotes.view",
            "affiliates.view",
            "affiliates.manage",
            "fraud.manage",
            "tasks.manage",
        ],
    ),
    (
        "support",
        "Destek",
        "Destek talepleri ve bilgi bankası",
        &[
            "clients.view",
            "services.view",
            "domains.manage",
            "support.view",
            "support.manage",
            "content.manage",
            "status.manage",
            "tasks.manage",
        ],
    ),
    (
        "sales",
        "Satış",
        "Müşteri kazanımı ve siparişler",
        &[
            "clients.view",
            "clients.manage",
            "billing.view",
            "services.view",
            "support.view",
            "reports.view",
            "quotes.view",
            "quotes.manage",
            "affiliates.view",
            "tasks.manage",
        ],
    ),
];

/// What [`run`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub tenant_id: Option<TenantId>,
    /// Newly inserted rows per table (0 everywhere on a re-run).
    pub inserted: Vec<(&'static str, u64)>,
}

impl SeedReport {
    pub(crate) fn add(&mut self, table: &'static str, n: u64) {
        if let Some(e) = self.inserted.iter_mut().find(|(t, _)| *t == table) {
            e.1 += n;
        } else {
            self.inserted.push((table, n));
        }
    }

    /// Total inserted rows.
    pub fn total(&self) -> u64 {
        self.inserted.iter().map(|(_, n)| n).sum()
    }
}

/// Verified hosts of the `seed-demo` tenant: `(host, kind, is_primary)`.
pub const DEMO_TENANT_HOSTS: &[(&str, &str, bool)] = &[
    ("demo.localhost", "subdomain", true),
    ("localhost", "custom", false),
];

/// Which tenant [`seed_demo_tenant`] creates or completes.
#[derive(Debug, Clone, Copy)]
pub struct DemoTenant<'a> {
    /// Tenant slug ([`DEMO_TENANT_SLUG`] for `seed-demo`; the public demo's
    /// tests use throw-away slugs).
    pub slug: &'a str,
    /// Id of a newly inserted tenant row (`None` → a new id). The demo-mode
    /// reset passes the previous id so cached references stay valid.
    pub id: Option<TenantId>,
    /// Verified hosts: `(host, kind, is_primary)`; a host that already
    /// belongs to any tenant is left alone.
    pub hosts: &'a [(&'a str, &'a str, bool)],
}

/// Seeds platform plans, the demo tenant with its defaults, catalog and
/// content — in a single transaction.
pub async fn run(pool: &PgPool) -> DbResult<SeedReport> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let report = seed_demo_tenant(
        &mut tx,
        &DemoTenant {
            slug: DEMO_TENANT_SLUG,
            id: None,
            hosts: DEMO_TENANT_HOSTS,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(report)
}

/// [`run`] for any slug, inside the caller's transaction: platform plans,
/// the tenant with its defaults, catalog and content. Idempotent like
/// everything here.
pub async fn seed_demo_tenant(
    conn: &mut PgConnection,
    target: &DemoTenant<'_>,
) -> DbResult<SeedReport> {
    let mut report = SeedReport::default();
    seed_platform_plans(conn, &mut report).await?;
    let tenant_id = ensure_demo_tenant(conn, target, &mut report).await?;
    // The demo publishes its category landings and keeps untouched default
    // pages current (site.rs); other tenants get drafts.
    site::seed_site_pages(conn, tenant_id, true, &mut report).await?;
    site::refresh_untouched(conn, tenant_id, &mut report).await?;
    seed_tenant_defaults_into(conn, tenant_id, &mut report).await?;
    seed_demo_settings(conn, tenant_id, &mut report).await?;
    catalog::seed_catalog(conn, tenant_id, &mut report).await?;
    catalog::seed_tlds(conn, tenant_id, &mut report).await?;
    content::seed_content(conn, tenant_id, &mut report).await?;
    report.tenant_id = Some(tenant_id);
    Ok(report)
}

/// Puts the seeded TLD price list back for `tenant_id`, in one transaction:
/// every price point of the seeded TLDs is replaced (other TLDs are left
/// alone). `seed-demo` uses it to repair demo prices that a TLD price sync
/// derived from a misconfigured mock registrar. Returns the price points
/// written.
pub async fn reset_tld_prices(pool: &PgPool, tenant_id: TenantId) -> DbResult<u64> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let written = catalog::reset_tld_prices(&mut tx, tenant_id).await?;
    tx.commit().await?;
    Ok(written)
}

/// Defaults every tenant needs (see module docs). Idempotent.
pub async fn seed_tenant_defaults(
    conn: &mut PgConnection,
    tenant_id: TenantId,
) -> DbResult<SeedReport> {
    let mut tx = conn.begin().await?;
    let mut report = SeedReport::default();
    seed_tenant_defaults_into(&mut tx, tenant_id, &mut report).await?;
    tx.commit().await?;
    report.tenant_id = Some(tenant_id);
    Ok(report)
}

async fn seed_tenant_defaults_into(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    seed_currencies(conn, tenant_id, report).await?;
    seed_tax_rules(conn, tenant_id, report).await?;
    seed_invoice_sequence(conn, tenant_id, report).await?;
    seed_roles(conn, tenant_id, report).await?;
    seed_departments(conn, tenant_id, report).await?;
    seed_base_settings(conn, tenant_id, report).await?;
    content::seed_email_templates(conn, tenant_id, report).await?;
    site::seed_site_pages(conn, tenant_id, false, report).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// platform
// ---------------------------------------------------------------------------

/// Starter / Growth / Scale.
pub async fn seed_platform_plans(conn: &mut PgConnection, report: &mut SeedReport) -> DbResult<()> {
    let plans = [
        (
            "starter",
            "Starter",
            "Yeni başlayan hosting firmaları için: vitrin, müşteri paneli, faturalandırma ve temel otomasyon.",
            Decimal::new(29, 0),
            Decimal::new(290, 0),
            json!({"max_clients": 250, "max_services": 500, "max_staff": 2, "custom_domains": 1, "integrations": 3}),
            json!([
                "storefront",
                "client_area",
                "billing",
                "support_desk",
                "knowledge_base",
                "seo_basics"
            ]),
            10,
        ),
        (
            "growth",
            "Growth",
            "Büyüyen firmalar için: sınırsız entegrasyon, REST API, AI içerik üretimi ve gelişmiş SEO.",
            Decimal::new(79, 0),
            Decimal::new(790, 0),
            json!({"max_clients": 2000, "max_services": 5000, "max_staff": 10, "custom_domains": 5, "integrations": null}),
            json!([
                "storefront",
                "client_area",
                "billing",
                "support_desk",
                "knowledge_base",
                "seo_basics",
                "api",
                "ai_content",
                "seo_audits",
                "white_label"
            ]),
            20,
        ),
        (
            "scale",
            "Scale",
            "Yüksek hacimli sağlayıcılar için: sınırsız müşteri, öncelikli destek, SLA ve özel entegrasyonlar.",
            Decimal::new(199, 0),
            Decimal::new(1990, 0),
            json!({"max_clients": null, "max_services": null, "max_staff": 50, "custom_domains": null, "integrations": null}),
            json!([
                "storefront",
                "client_area",
                "billing",
                "support_desk",
                "knowledge_base",
                "seo_basics",
                "api",
                "ai_content",
                "seo_audits",
                "white_label",
                "priority_support",
                "sla",
                "sso"
            ]),
            30,
        ),
    ];
    for (code, name, description, monthly, annual, limits, features, sort) in plans {
        let res = sqlx::query(
            "insert into platform_plans (id, code, name, description, currency, price_monthly, price_annual,
                                         trial_days, limits, features, sort_order)
             values ($1, $2, $3, $4, 'USD', $5, $6, 14, $7, $8, $9)
             on conflict (code) do nothing",
        )
        .bind(PlatformPlanId::new())
        .bind(code)
        .bind(name)
        .bind(description)
        .bind(monthly)
        .bind(annual)
        .bind(limits)
        .bind(features)
        .bind(sort)
        .execute(&mut *conn)
        .await?;
        report.add("platform_plans", res.rows_affected());
    }
    Ok(())
}

async fn ensure_demo_tenant(
    conn: &mut PgConnection,
    target: &DemoTenant<'_>,
    report: &mut SeedReport,
) -> DbResult<TenantId> {
    let branding = json!({
        "company_name": "Demo Hosting A.Ş.",
        "primary_color": "#2563eb",
        "accent_color": "#f59e0b",
        "support_email": "destek@demo.localhost",
        "email_from_name": "Demo Hosting",
        "email_from_address": "noreply@demo.localhost"
    });
    let address = json!({
        "line1": "Maslak Mah. Büyükdere Cad. No: 255",
        "city": "Sarıyer",
        "state": "İstanbul",
        "postal_code": "34485",
        "country_code": "TR"
    });
    let res = sqlx::query(
        "insert into tenants (id, slug, name, status, default_locale, default_currency, timezone, branding,
                              legal_name, tax_office, tax_number, address, contact_email, contact_phone)
         values ($1, $2, 'Demo Hosting', $3, $4, 'TRY', 'Europe/Istanbul', $5,
                 'Demo Hosting Bilişim A.Ş.', 'Maslak', '1234567890', $6, 'info@demo.localhost', '+90 212 000 00 00')
         on conflict (slug) do nothing",
    )
    .bind(target.id.unwrap_or_else(TenantId::new))
    .bind(target.slug)
    .bind(TenantStatus::Active)
    .bind(Locale::Tr)
    .bind(branding)
    .bind(address)
    .execute(&mut *conn)
    .await?;
    report.add("tenants", res.rows_affected());
    let tenant_id: TenantId = sqlx::query_scalar("select id from tenants where slug = $1")
        .bind(target.slug)
        .fetch_one(&mut *conn)
        .await?;

    for &(host, kind, primary) in target.hosts {
        let res = sqlx::query(
            "insert into tenant_domains (id, tenant_id, host, kind, is_primary, verified_at, tls_status)
             values ($1, $2, $3, $4, $5, now(), 'not_required')
             on conflict (host) do nothing",
        )
        .bind(TenantDomainId::new())
        .bind(tenant_id)
        .bind(host)
        .bind(kind)
        .bind(primary)
        .execute(&mut *conn)
        .await?;
        report.add("tenant_domains", res.rows_affected());
    }

    let res = sqlx::query(
        "insert into tenant_subscriptions (id, tenant_id, plan_id, status, billing_cycle, price, currency,
                                           current_period_start, current_period_end)
         select $1, $2, p.id, 'active', 'monthly', p.price_monthly, p.currency, now(), now() + interval '1 month'
         from platform_plans p
         where p.code = 'growth'
           and not exists (select 1 from tenant_subscriptions s where s.tenant_id = $2)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .execute(&mut *conn)
    .await?;
    report.add("tenant_subscriptions", res.rows_affected());
    Ok(tenant_id)
}

// ---------------------------------------------------------------------------
// tenant defaults
// ---------------------------------------------------------------------------

async fn seed_currencies(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    // exchange_rate = value of 1 TRY in that currency.
    let rows = [
        ("TRY", "Türk Lirası", "₺", "", " ₺", Decimal::ONE, true, 0),
        (
            "USD",
            "US Dollar",
            "$",
            "$",
            "",
            Decimal::new(21, 3),
            false,
            1,
        ),
        ("EUR", "Euro", "€", "€", "", Decimal::new(18, 3), false, 2),
    ];
    for (code, name, symbol, prefix, suffix, rate, is_default, sort) in rows {
        let res = sqlx::query(
            "insert into currencies (id, tenant_id, code, name, symbol, prefix, suffix, decimals, exchange_rate,
                                     is_default, sort_order)
             select $1, $2, $3, $4, $5, $6, $7, 2, $8,
                    $9 and not exists (select 1 from currencies where tenant_id = $2 and is_default), $10
             on conflict (tenant_id, code) do nothing",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(code)
        .bind(name)
        .bind(symbol)
        .bind(prefix)
        .bind(suffix)
        .bind(rate)
        .bind(is_default)
        .bind(sort)
        .execute(&mut *conn)
        .await?;
        report.add("currencies", res.rows_affected());
    }
    Ok(())
}

async fn seed_tax_rules(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    let res = sqlx::query(
        "insert into tax_rules (id, tenant_id, name, level, country_code, rate, is_compound, priority)
         select $1, $2, 'KDV', 1, 'TR', 20, false, 0
         where not exists (select 1 from tax_rules where tenant_id = $2 and name = 'KDV' and country_code = 'TR')",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .execute(&mut *conn)
    .await?;
    report.add("tax_rules", res.rows_affected());
    Ok(())
}

async fn seed_invoice_sequence(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    // GİB e-Arşiv style: 3-letter prefix + year + 9 digits (e.g. HOS2026000000001).
    let res = sqlx::query(
        "insert into invoice_sequences (id, tenant_id, prefix, next_value, padding, reset_yearly)
         select $1, t.id, upper(substr(regexp_replace(t.slug, '[^a-z]', '', 'g') || 'hos', 1, 3)), 1, 9, true
         from tenants t where t.id = $2
         on conflict (tenant_id) do nothing",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .execute(&mut *conn)
    .await?;
    report.add("invoice_sequences", res.rows_affected());
    Ok(())
}

async fn seed_roles(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    for (slug, name, description, permissions) in DEFAULT_ROLES {
        let (role_id, created): (RoleId, bool) = sqlx::query_as(
            "with ins as (
                 insert into roles (id, tenant_id, slug, name, description, is_system)
                 values ($1, $2, $3, $4, $5, true)
                 on conflict (tenant_id, slug) do nothing
                 returning id
             )
             select id, true from ins
             union all select id, false from roles where tenant_id = $2 and slug = $3 limit 1",
        )
        .bind(RoleId::new())
        .bind(tenant_id)
        .bind(slug)
        .bind(name)
        .bind(description)
        .fetch_one(&mut *conn)
        .await?;
        let perms: Vec<String> = permissions.iter().map(|p| (*p).to_owned()).collect();
        let ids: Vec<Uuid> = perms.iter().map(|_| Uuid::now_v7()).collect();
        let res = sqlx::query(
            "insert into role_permissions (id, tenant_id, role_id, permission)
             select u.id, $1, $2, u.perm from unnest($3::uuid[], $4::text[]) as u(id, perm)
             on conflict (role_id, permission) do nothing",
        )
        .bind(tenant_id)
        .bind(role_id)
        .bind(&ids)
        .bind(&perms)
        .execute(&mut *conn)
        .await?;
        report.add("roles", u64::from(created));
        report.add("role_permissions", res.rows_affected());
    }
    Ok(())
}

async fn seed_departments(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    let rows = [
        (
            "genel-destek",
            "Genel Destek",
            "General Support",
            "Genel sorularınız için",
            "General questions",
            false,
            0,
        ),
        (
            "teknik-destek",
            "Teknik Destek",
            "Technical Support",
            "Hosting, VPS ve sunucu sorunları",
            "Hosting, VPS and server issues",
            true,
            1,
        ),
        (
            "satis",
            "Satış",
            "Sales",
            "Ürün ve fiyat soruları",
            "Products and pricing",
            false,
            2,
        ),
        (
            "muhasebe",
            "Muhasebe & Faturalama",
            "Billing",
            "Fatura ve ödeme işlemleri",
            "Invoices and payments",
            true,
            3,
        ),
        (
            "abuse",
            "Kötüye Kullanım (Abuse)",
            "Abuse",
            "Spam, phishing ve kötüye kullanım bildirimleri",
            "Spam, phishing and abuse reports",
            false,
            4,
        ),
    ];
    for (slug, name_tr, name_en, desc_tr, desc_en, clients_only, sort) in rows {
        let res = sqlx::query(
            "insert into ticket_departments (id, tenant_id, slug, name, description, translations, clients_only, sort_order)
             values ($1, $2, $3, $4, $5, $6, $7, $8)
             on conflict (tenant_id, slug) do nothing",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(slug)
        .bind(name_tr)
        .bind(desc_tr)
        .bind(json!({"en": {"name": name_en, "description": desc_en}}))
        .bind(clients_only)
        .bind(sort)
        .execute(&mut *conn)
        .await?;
        report.add("ticket_departments", res.rows_affected());
    }
    Ok(())
}

async fn insert_settings(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    settings: &[(&str, serde_json::Value)],
    report: &mut SeedReport,
) -> DbResult<()> {
    for (key, value) in settings {
        let res = sqlx::query(
            "insert into tenant_settings (id, tenant_id, key, value) values ($1, $2, $3, $4)
             on conflict (tenant_id, key) do nothing",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(key)
        .bind(value)
        .execute(&mut *conn)
        .await?;
        report.add("tenant_settings", res.rows_affected());
    }
    Ok(())
}

async fn seed_base_settings(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    let settings = [
        ("billing.invoice_days_before_due", json!(7)),
        ("billing.payment_terms_days", json!(7)),
        ("billing.reminder_days_before_due", json!([3, 1])),
        ("billing.overdue_notice_days", json!([1, 3, 7])),
        ("billing.suspend_after_days", json!(5)),
        ("billing.terminate_after_days", json!(30)),
        (
            "billing.late_fee",
            json!({"type": "percent", "amount": 0, "min_amount": 0}),
        ),
        ("billing.apply_credit_automatically", json!(true)),
        ("domains.expiry_notice_days", json!([30, 14, 7, 1])),
        (
            "domains.default_nameservers",
            json!(["ns1.demo.localhost", "ns2.demo.localhost"]),
        ),
        ("support.auto_close_hours", json!(72)),
        ("support.allow_guest_tickets", json!(true)),
        ("seo.ai_crawlers", json!("allow")),
        ("seo.indexnow", json!(true)),
        ("auth.allow_registration", json!(true)),
        ("auth.require_email_verification", json!(true)),
        ("auth.session_days", json!(30)),
    ];
    insert_settings(conn, tenant_id, &settings, report).await
}

async fn seed_demo_settings(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    let settings = [
        ("seo.title_suffix", json!(" | Demo Hosting")),
        (
            "seo.default_description",
            json!({
                "tr": "Demo Hosting: NVMe web hosting, VPS, dedicated sunucu, alan adı ve yönetilen bulut hizmetleri.",
                "en": "Demo Hosting: NVMe web hosting, VPS, dedicated servers, domains and managed cloud services."
            }),
        ),
        (
            "storefront.hero",
            json!({
                "tr": {"title": "Hızlı, güvenli ve yerli altyapı", "subtitle": "İstanbul veri merkezinde NVMe hosting ve VPS"},
                "en": {"title": "Fast, secure infrastructure", "subtitle": "NVMe hosting and VPS in Istanbul data centers"}
            }),
        ),
    ];
    insert_settings(conn, tenant_id, &settings, report).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_strings_match_check_constraint() {
        // Same rule as role_permissions.permission CHECK.
        let ok = |p: &str| {
            p == "*"
                || p.split_once('.').is_some_and(|(a, b)| {
                    let ident = |s: &str| {
                        s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                            && s.chars()
                                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    };
                    ident(a) && (b == "*" || ident(b))
                })
        };
        for (_, _, _, perms) in DEFAULT_ROLES {
            for p in *perms {
                assert!(ok(p), "{p}");
            }
        }
    }
}
