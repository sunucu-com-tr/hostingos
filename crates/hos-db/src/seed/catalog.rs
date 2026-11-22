//! Demo catalog: product groups, products with TRY / USD / EUR prices,
//! configurable VPS options, addons and 15 TLDs with prices.

use hos_core::domain::ProductType;
use hos_core::ids::{
    ConfigOptionGroupId, ConfigOptionId, ProductAddonId, ProductGroupId, ProductId, TenantId, TldId,
};
use hos_core::money::{BillingCycle, Currency, Money};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::SeedReport;
use crate::error::DbResult;

/// Discount factor per cycle relative to `months × monthly`.
fn cycle_factor(cycle: BillingCycle) -> Decimal {
    match cycle {
        BillingCycle::Monthly | BillingCycle::OneTime => Decimal::ONE,
        BillingCycle::Quarterly => Decimal::new(97, 2),
        BillingCycle::Semiannual => Decimal::new(94, 2),
        BillingCycle::Annual => Decimal::new(85, 2),
        BillingCycle::Biennial => Decimal::new(78, 2),
        BillingCycle::Triennial => Decimal::new(72, 2),
    }
}

/// Price of `cycle` from a monthly price, rounded to the currency's minor unit.
pub(crate) fn cycle_price(monthly: Decimal, cycle: BillingCycle, currency: Currency) -> Decimal {
    let months = Decimal::from(cycle.months().unwrap_or(1));
    Money::new(monthly * months * cycle_factor(cycle), currency)
        .round()
        .amount
}

const ALL_RECURRING: &[BillingCycle] = &[
    BillingCycle::Monthly,
    BillingCycle::Quarterly,
    BillingCycle::Semiannual,
    BillingCycle::Annual,
    BillingCycle::Biennial,
    BillingCycle::Triennial,
];
const SERVER_CYCLES: &[BillingCycle] = &[
    BillingCycle::Monthly,
    BillingCycle::Quarterly,
    BillingCycle::Semiannual,
    BillingCycle::Annual,
];
const DEDICATED_CYCLES: &[BillingCycle] = &[
    BillingCycle::Monthly,
    BillingCycle::Quarterly,
    BillingCycle::Annual,
];
const SAAS_CYCLES: &[BillingCycle] = &[BillingCycle::Monthly, BillingCycle::Annual];
const ONE_TIME: &[BillingCycle] = &[BillingCycle::OneTime];

struct GroupSeed {
    slug: &'static str,
    name: &'static str,
    name_en: &'static str,
    tagline: &'static str,
    tagline_en: &'static str,
    icon: &'static str,
}

const GROUPS: &[GroupSeed] = &[
    GroupSeed {
        slug: "web-hosting",
        name: "Web Hosting",
        name_en: "Web Hosting",
        tagline: "NVMe diskler, LiteSpeed ve ücretsiz SSL ile hızlı paylaşımlı hosting",
        tagline_en: "Fast shared hosting with NVMe, LiteSpeed and free SSL",
        icon: "globe",
    },
    GroupSeed {
        slug: "vps",
        name: "VPS Sunucular",
        name_en: "VPS Servers",
        tagline: "Tam root erişimli, dakikalar içinde hazır KVM sanal sunucular",
        tagline_en: "KVM virtual servers with full root access, ready in minutes",
        icon: "server",
    },
    GroupSeed {
        slug: "dedicated",
        name: "Dedicated Sunucular",
        name_en: "Dedicated Servers",
        tagline: "Size özel donanım, IPMI erişimi ve sınırsız trafik",
        tagline_en: "Dedicated hardware with IPMI access and unmetered traffic",
        icon: "cpu",
    },
    GroupSeed {
        slug: "cloud-platform",
        name: "Bulut Platformu",
        name_en: "Cloud Platform",
        tagline: "Yönetilen veritabanı, nesne depolama, container ve yapay zekâ servisleri",
        tagline_en: "Managed databases, object storage, containers and AI services",
        icon: "cloud",
    },
];

struct ProductSeed {
    slug: &'static str,
    group: &'static str,
    product_type: ProductType,
    name: &'static str,
    name_en: &'static str,
    tagline: &'static str,
    tagline_en: &'static str,
    features: &'static [&'static str],
    features_en: &'static [&'static str],
    /// Monthly (or one-time) price: TRY, USD, EUR.
    price: (i64, i64, i64),
    /// Setup fee: TRY, USD, EUR (in cents).
    setup: (i64, i64, i64),
    cycles: &'static [BillingCycle],
    pay_type: &'static str,
    auto_setup: &'static str,
    requires_domain: &'static str,
    featured: bool,
    module_config: fn() -> Value,
}

/// Prices in minor units: TRY, USD, EUR.
type Cents = (i64, i64, i64);

/// Cents → Decimal (`8990` → `89.90`).
fn c(cents: i64) -> Decimal {
    Decimal::new(cents, 2)
}

const PRODUCTS: &[ProductSeed] = &[
    ProductSeed {
        slug: "hosting-baslangic",
        group: "web-hosting",
        product_type: ProductType::SharedHosting,
        name: "Başlangıç",
        name_en: "Starter",
        tagline: "Kişisel siteler ve bloglar için",
        tagline_en: "For personal sites and blogs",
        features: &[
            "10 GB NVMe disk",
            "1 web sitesi",
            "10 e-posta hesabı",
            "Ücretsiz SSL",
            "Haftalık yedekleme",
        ],
        features_en: &[
            "10 GB NVMe storage",
            "1 website",
            "10 email accounts",
            "Free SSL",
            "Weekly backups",
        ],
        price: (8990, 249, 229),
        setup: (0, 0, 0),
        cycles: ALL_RECURRING,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "required",
        featured: false,
        module_config: || json!({"package": "starter"}),
    },
    ProductSeed {
        slug: "hosting-pro",
        group: "web-hosting",
        product_type: ProductType::SharedHosting,
        name: "Pro",
        name_en: "Pro",
        tagline: "KOBİ'ler ve e-ticaret siteleri için en popüler paket",
        tagline_en: "Most popular plan for small businesses and online stores",
        features: &[
            "30 GB NVMe disk",
            "10 web sitesi",
            "Sınırsız e-posta hesabı",
            "Ücretsiz SSL",
            "Günlük yedekleme",
            "LiteSpeed Cache",
        ],
        features_en: &[
            "30 GB NVMe storage",
            "10 websites",
            "Unlimited email accounts",
            "Free SSL",
            "Daily backups",
            "LiteSpeed Cache",
        ],
        price: (16990, 449, 419),
        setup: (0, 0, 0),
        cycles: ALL_RECURRING,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "required",
        featured: true,
        module_config: || json!({"package": "pro"}),
    },
    ProductSeed {
        slug: "hosting-business",
        group: "web-hosting",
        product_type: ProductType::SharedHosting,
        name: "Business",
        name_en: "Business",
        tagline: "Yoğun trafikli siteler için ayrılmış kaynaklar",
        tagline_en: "Dedicated resources for high-traffic sites",
        features: &[
            "100 GB NVMe disk",
            "Sınırsız web sitesi",
            "2 vCPU / 4 GB RAM ayrılmış kaynak",
            "Günlük yedekleme, 30 gün saklama",
            "Ücretsiz SSL ve CDN",
            "Öncelikli destek",
        ],
        features_en: &[
            "100 GB NVMe storage",
            "Unlimited websites",
            "2 vCPU / 4 GB RAM dedicated resources",
            "Daily backups, 30-day retention",
            "Free SSL and CDN",
            "Priority support",
        ],
        price: (29990, 799, 749),
        setup: (0, 0, 0),
        cycles: ALL_RECURRING,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "required",
        featured: false,
        module_config: || json!({"package": "business"}),
    },
    ProductSeed {
        slug: "vps-s",
        group: "vps",
        product_type: ProductType::Vps,
        name: "VPS S",
        name_en: "VPS S",
        tagline: "Geliştirme ve küçük projeler için",
        tagline_en: "For development and small projects",
        features: &[
            "2 vCPU",
            "4 GB RAM",
            "80 GB NVMe",
            "4 TB trafik",
            "1 IPv4 + /64 IPv6",
        ],
        features_en: &[
            "2 vCPU",
            "4 GB RAM",
            "80 GB NVMe",
            "4 TB traffic",
            "1 IPv4 + /64 IPv6",
        ],
        price: (44900, 999, 929),
        setup: (0, 0, 0),
        cycles: SERVER_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: false,
        module_config: || json!({"cpu_cores": 2, "memory_mb": 4096, "disk_gb": 80, "bandwidth_gb": 4000}),
    },
    ProductSeed {
        slug: "vps-m",
        group: "vps",
        product_type: ProductType::Vps,
        name: "VPS M",
        name_en: "VPS M",
        tagline: "Üretim uygulamaları için dengeli kaynak",
        tagline_en: "Balanced resources for production workloads",
        features: &[
            "4 vCPU",
            "8 GB RAM",
            "160 GB NVMe",
            "8 TB trafik",
            "1 IPv4 + /64 IPv6",
        ],
        features_en: &[
            "4 vCPU",
            "8 GB RAM",
            "160 GB NVMe",
            "8 TB traffic",
            "1 IPv4 + /64 IPv6",
        ],
        price: (84900, 1899, 1749),
        setup: (0, 0, 0),
        cycles: SERVER_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: true,
        module_config: || json!({"cpu_cores": 4, "memory_mb": 8192, "disk_gb": 160, "bandwidth_gb": 8000}),
    },
    ProductSeed {
        slug: "vps-l",
        group: "vps",
        product_type: ProductType::Vps,
        name: "VPS L",
        name_en: "VPS L",
        tagline: "Veritabanları ve yoğun uygulamalar için",
        tagline_en: "For databases and demanding applications",
        features: &[
            "8 vCPU",
            "16 GB RAM",
            "320 GB NVMe",
            "16 TB trafik",
            "1 IPv4 + /64 IPv6",
        ],
        features_en: &[
            "8 vCPU",
            "16 GB RAM",
            "320 GB NVMe",
            "16 TB traffic",
            "1 IPv4 + /64 IPv6",
        ],
        price: (159900, 3499, 3249),
        setup: (0, 0, 0),
        cycles: SERVER_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: false,
        module_config: || json!({"cpu_cores": 8, "memory_mb": 16384, "disk_gb": 320, "bandwidth_gb": 16000}),
    },
    ProductSeed {
        slug: "vps-xl",
        group: "vps",
        product_type: ProductType::Vps,
        name: "VPS XL",
        name_en: "VPS XL",
        tagline: "Maksimum performans gerektiren iş yükleri",
        tagline_en: "Maximum performance workloads",
        features: &[
            "16 vCPU",
            "32 GB RAM",
            "640 GB NVMe",
            "32 TB trafik",
            "1 IPv4 + /64 IPv6",
        ],
        features_en: &[
            "16 vCPU",
            "32 GB RAM",
            "640 GB NVMe",
            "32 TB traffic",
            "1 IPv4 + /64 IPv6",
        ],
        price: (299900, 6499, 5999),
        setup: (0, 0, 0),
        cycles: SERVER_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: false,
        module_config: || json!({"cpu_cores": 16, "memory_mb": 32768, "disk_gb": 640, "bandwidth_gb": 32000}),
    },
    ProductSeed {
        slug: "dedicated-ryzen-7950x",
        group: "dedicated",
        product_type: ProductType::Dedicated,
        name: "Ryzen 9 7950X",
        name_en: "Ryzen 9 7950X",
        tagline: "16 çekirdek, 128 GB DDR5 ECC, NVMe RAID 1",
        tagline_en: "16 cores, 128 GB DDR5 ECC, NVMe RAID 1",
        features: &[
            "AMD Ryzen 9 7950X (16C/32T)",
            "128 GB DDR5 ECC RAM",
            "2 × 1,92 TB NVMe (RAID 1)",
            "1 Gbit/s sınırsız trafik",
            "IPMI / KVM uzaktan erişim",
            "/29 IPv4 blok",
        ],
        features_en: &[
            "AMD Ryzen 9 7950X (16C/32T)",
            "128 GB DDR5 ECC RAM",
            "2 × 1.92 TB NVMe (RAID 1)",
            "1 Gbit/s unmetered traffic",
            "IPMI / KVM remote access",
            "/29 IPv4 block",
        ],
        price: (799900, 16900, 15500),
        setup: (150000, 3500, 3200),
        cycles: DEDICATED_CYCLES,
        pay_type: "recurring",
        auto_setup: "manual",
        requires_domain: "none",
        featured: false,
        module_config: || json!({"hardware": "ryzen-7950x-128g", "provisioning": "manual"}),
    },
    ProductSeed {
        slug: "managed-postgresql",
        group: "cloud-platform",
        product_type: ProductType::ManagedDb,
        name: "Yönetilen PostgreSQL",
        name_en: "Managed PostgreSQL",
        tagline: "Yedekleme, güncelleme ve izleme bizden",
        tagline_en: "We handle backups, upgrades and monitoring",
        features: &[
            "2 vCPU / 4 GB RAM",
            "50 GB NVMe depolama",
            "Günlük yedek + 7 gün PITR",
            "PgBouncer bağlantı havuzu",
            "PostgreSQL 17 / 18",
        ],
        features_en: &[
            "2 vCPU / 4 GB RAM",
            "50 GB NVMe storage",
            "Daily backups + 7-day PITR",
            "PgBouncer connection pooling",
            "PostgreSQL 17 / 18",
        ],
        price: (69900, 1499, 1399),
        setup: (0, 0, 0),
        cycles: SAAS_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: true,
        module_config: || json!({"engine": "postgresql", "version": "18", "vcpu": 2, "memory_mb": 4096, "storage_gb": 50}),
    },
    ProductSeed {
        slug: "object-storage-1tb",
        group: "cloud-platform",
        product_type: ProductType::ObjectStorage,
        name: "Nesne Depolama 1 TB",
        name_en: "Object Storage 1 TB",
        tagline: "S3 uyumlu, ölçeklenebilir depolama",
        tagline_en: "S3-compatible, scalable storage",
        features: &[
            "1 TB depolama",
            "2 TB çıkış trafiği",
            "Sınırsız bucket",
            "Versiyonlama ve yaşam döngüsü kuralları",
        ],
        features_en: &[
            "1 TB storage",
            "2 TB egress",
            "Unlimited buckets",
            "Versioning and lifecycle rules",
        ],
        price: (19900, 449, 419),
        setup: (0, 0, 0),
        cycles: SAAS_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: false,
        module_config: || json!({"quota_gb": 1024, "egress_gb": 2048}),
    },
    ProductSeed {
        slug: "container-app",
        group: "cloud-platform",
        product_type: ProductType::ContainerApp,
        name: "Container App",
        name_en: "Container App",
        tagline: "Git push ile dağıtım, otomatik HTTPS",
        tagline_en: "Deploy with git push, automatic HTTPS",
        features: &[
            "1 vCPU / 2 GB RAM",
            "Docker imajı veya Git ile dağıtım",
            "Otomatik HTTPS",
            "Ortam değişkenleri ve gizli anahtarlar",
        ],
        features_en: &[
            "1 vCPU / 2 GB RAM",
            "Deploy from Docker image or Git",
            "Automatic HTTPS",
            "Environment variables and secrets",
        ],
        price: (24900, 549, 499),
        setup: (0, 0, 0),
        cycles: SAAS_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "optional",
        featured: false,
        module_config: || json!({"vcpu": 1, "memory_mb": 2048, "max_instances": 3}),
    },
    ProductSeed {
        slug: "ai-asistan",
        group: "cloud-platform",
        product_type: ProductType::AiService,
        name: "AI Asistan",
        name_en: "AI Assistant",
        tagline: "Web siteniz için Claude destekli müşteri asistanı",
        tagline_en: "Claude-powered customer assistant for your website",
        features: &[
            "Aylık 2.000 sohbet",
            "Bilgi bankası ve SSS entegrasyonu",
            "Türkçe ve İngilizce",
            "Tek satır kodla siteye ekleme",
        ],
        features_en: &[
            "2,000 conversations per month",
            "Knowledge base and FAQ integration",
            "Turkish and English",
            "One-line website embed",
        ],
        price: (49900, 1099, 999),
        setup: (0, 0, 0),
        cycles: SAAS_CYCLES,
        pay_type: "recurring",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: true,
        module_config: || json!({"monthly_conversations": 2000, "model": "claude-opus-5-5"}),
    },
    ProductSeed {
        slug: "ai-api-kredisi",
        group: "cloud-platform",
        product_type: ProductType::AiService,
        name: "AI API Kredisi (10M token)",
        name_en: "AI API Credits (10M tokens)",
        tagline: "Kendi uygulamalarınız için kullandıkça öde yapay zekâ API'si",
        tagline_en: "Pay-as-you-go AI API for your own applications",
        features: &["10 milyon token", "OpenAI uyumlu uç nokta", "12 ay geçerli"],
        features_en: &[
            "10 million tokens",
            "OpenAI-compatible endpoint",
            "Valid for 12 months",
        ],
        price: (89900, 1999, 1849),
        setup: (0, 0, 0),
        cycles: ONE_TIME,
        pay_type: "one_time",
        auto_setup: "on_payment",
        requires_domain: "none",
        featured: false,
        module_config: || json!({"tokens": 10_000_000, "validity_days": 365}),
    },
];

async fn upsert_group(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    g: &GroupSeed,
    sort: i32,
) -> DbResult<(ProductGroupId, bool)> {
    Ok(sqlx::query_as::<_, (ProductGroupId, bool)>(
        "with ins as (
             insert into product_groups (id, tenant_id, slug, name, tagline, translations, icon, sort_order)
             values ($1, $2, $3, $4, $5, $6, $7, $8)
             on conflict (tenant_id, slug) do nothing returning id
         )
         select id, true from ins union all
         select id, false from product_groups where tenant_id = $2 and slug = $3 limit 1",
    )
    .bind(ProductGroupId::new())
    .bind(tenant_id)
    .bind(g.slug)
    .bind(g.name)
    .bind(g.tagline)
    .bind(json!({"en": {"name": g.name_en, "tagline": g.tagline_en}}))
    .bind(g.icon)
    .bind(sort)
    .fetch_one(&mut *conn)
    .await?)
}

async fn upsert_product(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    group_id: ProductGroupId,
    p: &ProductSeed,
    sort: i32,
) -> DbResult<(ProductId, bool)> {
    Ok(sqlx::query_as::<_, (ProductId, bool)>(
        "with ins as (
             insert into products (id, tenant_id, group_id, slug, product_type, name, tagline, features, translations,
                                   module_config, pay_type, auto_setup, requires_domain, is_featured, sort_order,
                                   welcome_email_template)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 'service_activated')
             on conflict (tenant_id, slug) do nothing returning id
         )
         select id, true from ins union all
         select id, false from products where tenant_id = $2 and slug = $4 limit 1",
    )
    .bind(ProductId::new())
    .bind(tenant_id)
    .bind(group_id)
    .bind(p.slug)
    .bind(p.product_type)
    .bind(p.name)
    .bind(p.tagline)
    .bind(json!(p.features))
    .bind(json!({"en": {"name": p.name_en, "tagline": p.tagline_en, "features": p.features_en}}))
    .bind((p.module_config)())
    .bind(p.pay_type)
    .bind(p.auto_setup)
    .bind(p.requires_domain)
    .bind(p.featured)
    .bind(sort)
    .fetch_one(&mut *conn)
    .await?)
}

/// Price rows `(currency, cycle, price, setup_fee)` for a monthly price triple.
fn price_rows(
    cycles: &[BillingCycle],
    monthly: (i64, i64, i64),
    setup: (i64, i64, i64),
) -> Vec<(Currency, BillingCycle, Decimal, Decimal)> {
    let mut rows = Vec::new();
    for (cur, m, s) in [
        (Currency::TRY, monthly.0, setup.0),
        (Currency::USD, monthly.1, setup.1),
        (Currency::EUR, monthly.2, setup.2),
    ] {
        for cycle in cycles {
            rows.push((cur, *cycle, cycle_price(c(m), *cycle, cur), c(s)));
        }
    }
    rows
}

async fn insert_prices(
    conn: &mut PgConnection,
    table: &'static str,
    owner_col: &'static str,
    tenant_id: TenantId,
    owner_id: Uuid,
    rows: &[(Currency, BillingCycle, Decimal, Decimal)],
) -> DbResult<u64> {
    let ids: Vec<Uuid> = rows.iter().map(|_| Uuid::now_v7()).collect();
    let currencies: Vec<Currency> = rows.iter().map(|r| r.0).collect();
    let cycles: Vec<BillingCycle> = rows.iter().map(|r| r.1).collect();
    let prices: Vec<Decimal> = rows.iter().map(|r| r.2).collect();
    let setups: Vec<Decimal> = rows.iter().map(|r| r.3).collect();
    let sql = format!(
        "insert into {table} (id, tenant_id, {owner_col}, currency, billing_cycle, price, setup_fee)
         select u.id, $1, $2, u.currency, u.cycle, u.price, u.setup
         from unnest($3::uuid[], $4::text[], $5::text[], $6::numeric[], $7::numeric[])
              as u(id, currency, cycle, price, setup)
         on conflict ({owner_col}, currency, billing_cycle) do nothing"
    );
    let res = sqlx::query(&sql)
        .bind(tenant_id)
        .bind(owner_id)
        .bind(&ids)
        .bind(&currencies)
        .bind(&cycles)
        .bind(&prices)
        .bind(&setups)
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected())
}

pub(super) async fn seed_catalog(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    let mut group_ids = Vec::new();
    for (i, g) in GROUPS.iter().enumerate() {
        let (id, created) =
            upsert_group(conn, tenant_id, g, i32::try_from(i).unwrap_or(0) * 10).await?;
        report.add("product_groups", u64::from(created));
        group_ids.push((g.slug, id));
    }

    let mut product_ids = Vec::new();
    for (i, p) in PRODUCTS.iter().enumerate() {
        let Some((_, group_id)) = group_ids.iter().find(|(slug, _)| *slug == p.group) else {
            continue;
        };
        let (id, created) = upsert_product(
            conn,
            tenant_id,
            *group_id,
            p,
            i32::try_from(i).unwrap_or(0) * 10,
        )
        .await?;
        report.add("products", u64::from(created));
        let rows = price_rows(p.cycles, p.price, p.setup);
        let n = insert_prices(
            conn,
            "product_pricing",
            "product_id",
            tenant_id,
            id.into_uuid(),
            &rows,
        )
        .await?;
        report.add("product_pricing", n);
        product_ids.push((p.slug, id));
    }

    let vps: Vec<ProductId> = product_ids
        .iter()
        .filter(|(slug, _)| slug.starts_with("vps-"))
        .map(|(_, id)| *id)
        .collect();
    let dedicated: Vec<ProductId> = product_ids
        .iter()
        .filter(|(slug, _)| slug.starts_with("dedicated-"))
        .map(|(_, id)| *id)
        .collect();
    seed_vps_options(conn, tenant_id, &vps, report).await?;
    seed_addons(conn, tenant_id, &vps, &dedicated, report).await?;
    Ok(())
}

async fn seed_vps_options(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    vps: &[ProductId],
    report: &mut SeedReport,
) -> DbResult<()> {
    let (group_id, created): (ConfigOptionGroupId, bool) = sqlx::query_as(
        "with ins as (
             insert into config_option_groups (id, tenant_id, name, description)
             values ($1, $2, 'VPS Seçenekleri', 'İşletim sistemi ve ek IP seçimi')
             on conflict (tenant_id, name) do nothing returning id
         )
         select id, true from ins union all
         select id, false from config_option_groups where tenant_id = $2 and name = 'VPS Seçenekleri' limit 1",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .fetch_one(&mut *conn)
    .await?;
    report.add("config_option_groups", u64::from(created));

    for product_id in vps {
        let res = sqlx::query(
            "insert into config_option_group_products (id, tenant_id, group_id, product_id) values ($1, $2, $3, $4)
             on conflict (group_id, product_id) do nothing",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(group_id)
        .bind(product_id)
        .execute(&mut *conn)
        .await?;
        report.add("config_option_group_products", res.rows_affected());
    }

    // (name, name_en, type, module_key, sort, values: (label, value, monthly price TRY/USD/EUR cents))
    type ValueSeed = (&'static str, &'static str, Cents);
    type OptionSeed = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        i32,
        &'static [ValueSeed],
    );
    let options: [OptionSeed; 2] = [
        (
            "İşletim Sistemi",
            "Operating System",
            "dropdown",
            "os_template",
            0,
            &[
                ("Ubuntu 24.04 LTS", "ubuntu-24.04", (0, 0, 0)),
                ("Debian 12", "debian-12", (0, 0, 0)),
                ("AlmaLinux 9", "almalinux-9", (0, 0, 0)),
                ("Rocky Linux 9", "rockylinux-9", (0, 0, 0)),
                ("Windows Server 2022", "windows-2022", (45000, 1000, 900)),
            ],
        ),
        (
            "Ek IPv4 Adresi",
            "Additional IPv4",
            "quantity",
            "extra_ipv4",
            1,
            &[("IPv4 adresi", "ipv4", (6000, 150, 140))],
        ),
    ];
    for (name, name_en, kind, module_key, sort, values) in options {
        let (option_id, created): (ConfigOptionId, bool) = sqlx::query_as(
            "with ins as (
                 insert into config_options (id, tenant_id, group_id, name, translations, option_type, module_key,
                                             max_quantity, sort_order)
                 select $1, $2, $3, $4, $5, $6, $7, case when $6 = 'quantity' then 8 else 0 end, $8
                 where not exists (select 1 from config_options where tenant_id = $2 and group_id = $3 and name = $4)
                 returning id
             )
             select id, true from ins union all
             select id, false from config_options where tenant_id = $2 and group_id = $3 and name = $4 limit 1",
        )
        .bind(ConfigOptionId::new())
        .bind(tenant_id)
        .bind(group_id)
        .bind(name)
        .bind(json!({"en": {"name": name_en}}))
        .bind(kind)
        .bind(module_key)
        .bind(sort)
        .fetch_one(&mut *conn)
        .await?;
        report.add("config_options", u64::from(created));

        for (i, (label, value, monthly)) in values.iter().enumerate() {
            let (value_id, created): (Uuid, bool) = sqlx::query_as(
                "with ins as (
                     insert into config_option_values (id, tenant_id, option_id, label, value, sort_order)
                     select $1, $2, $3, $4, $5, $6
                     where not exists (select 1 from config_option_values where option_id = $3 and value = $5)
                     returning id
                 )
                 select id, true from ins union all
                 select id, false from config_option_values where option_id = $3 and value = $5 limit 1",
            )
            .bind(Uuid::now_v7())
            .bind(tenant_id)
            .bind(option_id)
            .bind(label)
            .bind(value)
            .bind(i32::try_from(i).unwrap_or(0))
            .fetch_one(&mut *conn)
            .await?;
            report.add("config_option_values", u64::from(created));
            let rows = price_rows(SERVER_CYCLES, *monthly, (0, 0, 0));
            let n = insert_prices(
                conn,
                "config_option_pricing",
                "value_id",
                tenant_id,
                value_id,
                &rows,
            )
            .await?;
            report.add("config_option_pricing", n);
        }
    }
    Ok(())
}

async fn seed_addons(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    vps: &[ProductId],
    dedicated: &[ProductId],
    report: &mut SeedReport,
) -> DbResult<()> {
    let servers: Vec<ProductId> = vps.iter().chain(dedicated).copied().collect();
    // (slug, name, name_en, description, monthly price, products)
    type AddonSeed<'a> = (&'a str, &'a str, &'a str, &'a str, Cents, &'a [ProductId]);
    let addons: [AddonSeed<'_>; 2] = [
        (
            "gunluk-yedekleme",
            "Günlük Yedekleme",
            "Daily Backups",
            "Sunucunuzun her gün otomatik anlık görüntüsü, 7 gün saklama",
            (8900, 199, 189),
            vps,
        ),
        (
            "ddos-koruma-pro",
            "Gelişmiş DDoS Koruması",
            "Advanced DDoS Protection",
            "L3/L4 ve L7 saldırılara karşı 1 Tbps'e kadar filtreleme",
            (24900, 549, 499),
            &servers,
        ),
    ];
    for (slug, name, name_en, description, monthly, products) in addons {
        let (addon_id, created): (ProductAddonId, bool) = sqlx::query_as(
            "with ins as (
                 insert into product_addons (id, tenant_id, slug, name, description, translations)
                 values ($1, $2, $3, $4, $5, $6)
                 on conflict (tenant_id, slug) do nothing returning id
             )
             select id, true from ins union all
             select id, false from product_addons where tenant_id = $2 and slug = $3 limit 1",
        )
        .bind(ProductAddonId::new())
        .bind(tenant_id)
        .bind(slug)
        .bind(name)
        .bind(description)
        .bind(json!({"en": {"name": name_en}}))
        .fetch_one(&mut *conn)
        .await?;
        report.add("product_addons", u64::from(created));
        let rows = price_rows(SERVER_CYCLES, monthly, (0, 0, 0));
        let n = insert_prices(
            conn,
            "product_addon_pricing",
            "addon_id",
            tenant_id,
            addon_id.into_uuid(),
            &rows,
        )
        .await?;
        report.add("product_addon_pricing", n);
        for product_id in products {
            let res = sqlx::query(
                "insert into product_addon_products (id, tenant_id, addon_id, product_id) values ($1, $2, $3, $4)
                 on conflict (addon_id, product_id) do nothing",
            )
            .bind(Uuid::now_v7())
            .bind(tenant_id)
            .bind(addon_id)
            .bind(product_id)
            .execute(&mut *conn)
            .await?;
            report.add("product_addon_products", res.rows_affected());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TLDs
// ---------------------------------------------------------------------------

struct TldSeed {
    tld: &'static str,
    /// 1-year register / transfer / renew prices in cents: TRY, USD, EUR.
    register: (i64, i64, i64),
    transfer: (i64, i64, i64),
    renew: (i64, i64, i64),
    min_years: i16,
    label: Option<&'static str>,
    featured: bool,
}

const TLDS: &[TldSeed] = &[
    TldSeed {
        tld: "com",
        register: (54900, 1299, 1199),
        transfer: (52900, 1249, 1149),
        renew: (57900, 1399, 1299),
        min_years: 1,
        label: Some("popular"),
        featured: true,
    },
    TldSeed {
        tld: "com.tr",
        register: (24900, 699, 649),
        transfer: (24900, 699, 649),
        renew: (24900, 699, 649),
        min_years: 1,
        label: Some("popular"),
        featured: true,
    },
    TldSeed {
        tld: "net",
        register: (64900, 1499, 1399),
        transfer: (62900, 1449, 1349),
        renew: (67900, 1599, 1499),
        min_years: 1,
        label: None,
        featured: true,
    },
    TldSeed {
        tld: "org",
        register: (57900, 1349, 1249),
        transfer: (55900, 1299, 1199),
        renew: (60900, 1449, 1349),
        min_years: 1,
        label: None,
        featured: false,
    },
    TldSeed {
        tld: "tr",
        register: (34900, 899, 829),
        transfer: (34900, 899, 829),
        renew: (34900, 899, 829),
        min_years: 1,
        label: Some("new"),
        featured: true,
    },
    TldSeed {
        tld: "net.tr",
        register: (24900, 699, 649),
        transfer: (24900, 699, 649),
        renew: (24900, 699, 649),
        min_years: 1,
        label: None,
        featured: false,
    },
    TldSeed {
        tld: "info",
        register: (19900, 499, 449),
        transfer: (64900, 1499, 1399),
        renew: (69900, 1649, 1549),
        min_years: 1,
        label: Some("sale"),
        featured: false,
    },
    TldSeed {
        tld: "io",
        register: (189900, 4299, 3999),
        transfer: (189900, 4299, 3999),
        renew: (199900, 4499, 4199),
        min_years: 1,
        label: None,
        featured: true,
    },
    TldSeed {
        tld: "co",
        register: (119900, 2799, 2599),
        transfer: (114900, 2699, 2499),
        renew: (124900, 2899, 2699),
        min_years: 1,
        label: None,
        featured: false,
    },
    TldSeed {
        tld: "dev",
        register: (69900, 1599, 1499),
        transfer: (67900, 1549, 1449),
        renew: (72900, 1699, 1599),
        min_years: 1,
        label: None,
        featured: false,
    },
    TldSeed {
        tld: "app",
        register: (79900, 1799, 1699),
        transfer: (77900, 1749, 1649),
        renew: (82900, 1899, 1799),
        min_years: 1,
        label: None,
        featured: false,
    },
    TldSeed {
        tld: "ai",
        register: (399900, 8999, 8299),
        transfer: (399900, 8999, 8299),
        renew: (399900, 8999, 8299),
        min_years: 2,
        label: Some("new"),
        featured: true,
    },
    TldSeed {
        tld: "xyz",
        register: (7900, 199, 189),
        transfer: (54900, 1299, 1199),
        renew: (59900, 1399, 1299),
        min_years: 1,
        label: Some("sale"),
        featured: false,
    },
    TldSeed {
        tld: "online",
        register: (9900, 249, 229),
        transfer: (119900, 2799, 2599),
        renew: (129900, 2999, 2799),
        min_years: 1,
        label: Some("sale"),
        featured: false,
    },
    TldSeed {
        tld: "store",
        register: (12900, 299, 279),
        transfer: (179900, 3999, 3699),
        renew: (189900, 4299, 3999),
        min_years: 1,
        label: Some("sale"),
        featured: false,
    },
];

/// Restore (redemption) fee: TRY, USD, EUR cents.
const RESTORE_FEE: (i64, i64, i64) = (399900, 8900, 8200);

/// Price rows `(currency, action, years, price)` of one TLD.
fn tld_price_rows(t: &TldSeed) -> Vec<(Currency, &'static str, i16, Decimal)> {
    let mut rows = Vec::new();
    for (cur, reg, tra, ren, restore) in [
        (
            Currency::TRY,
            t.register.0,
            t.transfer.0,
            t.renew.0,
            RESTORE_FEE.0,
        ),
        (
            Currency::USD,
            t.register.1,
            t.transfer.1,
            t.renew.1,
            RESTORE_FEE.1,
        ),
        (
            Currency::EUR,
            t.register.2,
            t.transfer.2,
            t.renew.2,
            RESTORE_FEE.2,
        ),
    ] {
        let max_years = t.min_years.max(3);
        for years in t.min_years..=max_years {
            let extra = Decimal::from(years - 1) * c(ren);
            // First year at the (promotional) register price, further years at renew price.
            rows.push((cur, "register", years, c(reg) + extra));
            rows.push((cur, "renew", years, c(ren) * Decimal::from(years)));
        }
        rows.push((cur, "transfer", 1, c(tra)));
        if !t.tld.ends_with("tr") {
            rows.push((cur, "restore", 1, c(restore)));
        }
    }
    rows
}

pub(super) async fn seed_tlds(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    for (i, t) in TLDS.iter().enumerate() {
        let (tld_id, created): (TldId, bool) = sqlx::query_as(
            "with ins as (
                 insert into tlds (id, tenant_id, tld, min_years, max_years, label, is_featured, sort_order,
                                   supports_privacy, grace_period_days, redemption_period_days)
                 values ($1, $2, $3, $4, 10, $5, $6, $7, $8, $9, $10)
                 on conflict (tenant_id, tld) do nothing returning id
             )
             select id, true from ins union all
             select id, false from tlds where tenant_id = $2 and tld = $3 limit 1",
        )
        .bind(TldId::new())
        .bind(tenant_id)
        .bind(t.tld)
        .bind(t.min_years)
        .bind(t.label)
        .bind(t.featured)
        .bind(i32::try_from(i).unwrap_or(0) * 10)
        // .tr registry (TRABIS) has no WHOIS privacy service for individuals' data — it is hidden by default.
        .bind(!t.tld.ends_with("tr"))
        .bind(if t.tld.ends_with("tr") { 0 } else { 30 })
        .bind(if t.tld.ends_with("tr") { 0 } else { 30 })
        .fetch_one(&mut *conn)
        .await?;
        report.add("tlds", u64::from(created));
        let added = insert_tld_prices(conn, tenant_id, tld_id, t).await?;
        report.add("tld_pricing", added);
    }
    Ok(())
}

/// Inserts the seeded price points of `t` that are missing.
async fn insert_tld_prices(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    tld_id: TldId,
    t: &TldSeed,
) -> DbResult<u64> {
    let rows = tld_price_rows(t);
    let ids: Vec<Uuid> = rows.iter().map(|_| Uuid::now_v7()).collect();
    let currencies: Vec<Currency> = rows.iter().map(|r| r.0).collect();
    let actions: Vec<&str> = rows.iter().map(|r| r.1).collect();
    let years: Vec<i16> = rows.iter().map(|r| r.2).collect();
    let prices: Vec<Decimal> = rows.iter().map(|r| r.3).collect();
    let res = sqlx::query(
        "insert into tld_pricing (id, tenant_id, tld_id, currency, action, years, price)
         select u.id, $1, $2, u.currency, u.action, u.years, u.price
         from unnest($3::uuid[], $4::text[], $5::text[], $6::int2[], $7::numeric[])
              as u(id, currency, action, years, price)
         on conflict (tld_id, currency, action, years) do nothing",
    )
    .bind(tenant_id)
    .bind(tld_id)
    .bind(&ids)
    .bind(&currencies)
    .bind(&actions)
    .bind(&years)
    .bind(&prices)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected())
}

/// Replaces every price point of the seeded TLDs (that the tenant has) with
/// the seeded price list. Returns the number of price points written.
pub(super) async fn reset_tld_prices(
    conn: &mut PgConnection,
    tenant_id: TenantId,
) -> DbResult<u64> {
    let mut written = 0;
    for t in TLDS {
        let Some(tld_id) =
            sqlx::query_scalar::<_, TldId>("select id from tlds where tenant_id = $1 and tld = $2")
                .bind(tenant_id)
                .bind(t.tld)
                .fetch_optional(&mut *conn)
                .await?
        else {
            continue;
        };
        sqlx::query("delete from tld_pricing where tenant_id = $1 and tld_id = $2")
            .bind(tenant_id)
            .bind(tld_id)
            .execute(&mut *conn)
            .await?;
        written += insert_tld_prices(conn, tenant_id, tld_id, t).await?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn cycle_prices() {
        assert_eq!(
            cycle_price(dec!(89.90), BillingCycle::Monthly, Currency::TRY),
            dec!(89.90)
        );
        // 89.90 × 12 × 0.85 = 916.98
        assert_eq!(
            cycle_price(dec!(89.90), BillingCycle::Annual, Currency::TRY),
            dec!(916.98)
        );
        assert_eq!(
            cycle_price(dec!(19.99), BillingCycle::OneTime, Currency::USD),
            dec!(19.99)
        );
    }

    #[test]
    fn catalog_shape() {
        assert_eq!(TLDS.len(), 15);
        assert_eq!(
            PRODUCTS.iter().filter(|p| p.group == "web-hosting").count(),
            3
        );
        assert_eq!(
            PRODUCTS
                .iter()
                .filter(|p| p.product_type == ProductType::Vps)
                .count(),
            4
        );
        assert!(
            PRODUCTS
                .iter()
                .any(|p| p.product_type == ProductType::Dedicated)
        );
        assert!(
            PRODUCTS
                .iter()
                .any(|p| p.product_type == ProductType::ManagedDb)
        );
        assert!(
            PRODUCTS
                .iter()
                .any(|p| p.product_type == ProductType::ObjectStorage)
        );
        assert!(
            PRODUCTS
                .iter()
                .any(|p| p.product_type == ProductType::ContainerApp)
        );
        assert!(
            PRODUCTS
                .iter()
                .any(|p| p.product_type == ProductType::AiService)
        );
        for p in PRODUCTS {
            assert!(GROUPS.iter().any(|g| g.slug == p.group), "{}", p.slug);
        }
        let ai = TLDS.iter().find(|t| t.tld == "ai").unwrap();
        let rows = tld_price_rows(ai);
        assert!(rows.iter().all(|r| r.1 != "register" || r.2 >= 2));
    }
}
