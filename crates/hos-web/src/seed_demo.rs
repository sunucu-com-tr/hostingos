//! `hostingos seed-demo`: idempotent demo setup on top of `hos_db::seed::run`.
//!
//! * runs migrations + the hos-db seed (demo tenant, catalog, TLDs, content);
//! * accounts: staff owner `admin@demo.test`, client `musteri@demo.test`
//!   (with a client account) and platform operator `platform@demo.test` —
//!   random passwords, printed **only to stdout** when an account is created
//!   (or with `--reset-passwords`);
//! * `mock` integrations (virt, panel, registrar, gateway) with encrypted
//!   secrets, linked to the products / TLDs that need them; the mock panel
//!   gets the packages the seeded hosting products name (`starter`, `pro`,
//!   `business`); the mock registrar quotes in USD (its price
//!   list's currency) and the demo's nightly TLD price sync is turned off so
//!   the curated TLD prices stay (an older demo whose registrar quoted in TRY
//!   gets its seeded TLD prices restored);
//! * one **unpaid** VPS order + invoice for the demo client — no ready
//!   "active" service: the in-memory mock hypervisor only knows VMs created
//!   by the running process, so the demo walks the real path (pay the
//!   invoice → provisioning job → VM on the mock);
//! * sample blog posts in Turkish and English.
//!
//! Re-running changes nothing that already exists.

use anyhow::Context as _;
use chrono::{Duration, Utc};
use hos_core::config::AppConfig;
use hos_core::crypto::integration_aad;
use hos_core::domain::IntegrationCategory;
use hos_core::domain::{Locale, ProductType, UserKind};
use hos_core::ids::{IntegrationId, TenantId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::PgPool;
use hos_db::enums::ContentStatus;
use hos_db::repo::{catalog, clients, content, domains, identity, integrations, platform};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::cli::{StaffSpec, create_platform_admin, create_staff, random_password};

/// Demo staff owner.
pub const STAFF_EMAIL: &str = "admin@demo.test";
/// Demo client user.
pub const CLIENT_EMAIL: &str = "musteri@demo.test";
/// Demo platform operator.
pub const PLATFORM_EMAIL: &str = "platform@demo.test";

/// Runs the demo seed (see module docs).
pub async fn run(pool: &PgPool, config: &AppConfig, reset_passwords: bool) -> anyhow::Result<()> {
    hos_db::migrate(pool).await.context("running migrations")?;
    let report = hos_db::seed::run(pool).await.context("hos-db seed")?;
    let tenant_id = report
        .tenant_id
        .context("seed did not return the demo tenant")?;
    println!(
        "hos-db seed: {} new rows (tenant {})",
        report.total(),
        hos_db::seed::DEMO_TENANT_SLUG
    );

    let mut credentials: Vec<(String, &str, String)> = Vec::new();

    // ---- accounts -------------------------------------------------------
    let pw = random_password();
    if create_staff(
        pool,
        tenant_id,
        &StaffSpec {
            email: STAFF_EMAIL,
            password: &pw,
            role: "owner",
            first_name: "Demo",
            last_name: "Yönetici",
        },
    )
    .await?
    {
        credentials.push(("staff (owner)".into(), STAFF_EMAIL, pw));
    } else if reset_passwords {
        let pw = random_password();
        reset_user_password(pool, tenant_id, STAFF_EMAIL, &pw).await?;
        credentials.push(("staff (owner)".into(), STAFF_EMAIL, pw));
    }

    let pw = random_password();
    if create_demo_client(pool, tenant_id, &pw).await? {
        credentials.push(("client".into(), CLIENT_EMAIL, pw));
    } else if reset_passwords {
        let pw = random_password();
        reset_user_password(pool, tenant_id, CLIENT_EMAIL, &pw).await?;
        credentials.push(("client".into(), CLIENT_EMAIL, pw));
    }

    let pw = random_password();
    if create_platform_admin(pool, PLATFORM_EMAIL, "Platform Operatörü", &pw).await? {
        credentials.push(("platform admin".into(), PLATFORM_EMAIL, pw));
    } else if reset_passwords {
        let pw = random_password();
        let hash = hash(&pw).await?;
        sqlx::query("update platform_admins set password_hash = $2 where email = $1::citext")
            .bind(PLATFORM_EMAIL)
            .bind(hash)
            .execute(pool)
            .await?;
        credentials.push(("platform admin".into(), PLATFORM_EMAIL, pw));
    }

    // ---- integrations -----------------------------------------------------
    let created = seed_integrations(pool, config, tenant_id).await?;
    println!("mock integrations: {created} created");

    // ---- demo order (unpaid VPS invoice) ------------------------------------
    if let Some(number) = seed_demo_order(pool, config, tenant_id, CLIENT_EMAIL).await? {
        println!("demo order: {number} (unpaid VPS S invoice for {CLIENT_EMAIL})");
    }

    // ---- blog -------------------------------------------------------------
    let posts = seed_blog(&mut *pool.acquire().await?, tenant_id).await?;
    println!("blog posts: {posts} created");

    // ---- summary (stdout only) ----------------------------------------------
    let demo = platform::tenant_context_by_slug(pool, hos_db::seed::DEMO_TENANT_SLUG)
        .await?
        .context("demo tenant missing")?;
    println!();
    println!("demo tenant: {} ({})", demo.display_name(), demo.slug);
    println!(
        "storefront:  {} (or http://localhost:<port> with HOS_DEV_TENANT=demo)",
        config.base_url
    );
    if credentials.is_empty() {
        println!(
            "accounts already exist; run `hostingos seed-demo --reset-passwords` for new passwords"
        );
    } else {
        println!("accounts (passwords are shown only now):");
        for (role, email, pw) in &credentials {
            println!("  {role:<16} {email:<24} {pw}");
        }
    }
    Ok(())
}

async fn hash(password: &str) -> anyhow::Result<String> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hos_auth::password::hash_password(&password))
        .await
        .context("hashing task")?
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

async fn reset_user_password(
    pool: &PgPool,
    tenant_id: TenantId,
    email: &str,
    password: &str,
) -> anyhow::Result<()> {
    let user = identity::user_by_email(pool, tenant_id, email)
        .await?
        .with_context(|| format!("user {email} missing"))?;
    let hash = hash(password).await?;
    identity::set_password_hash(pool, tenant_id, user.id, Some(&hash)).await?;
    Ok(())
}

async fn create_demo_client(
    pool: &PgPool,
    tenant_id: TenantId,
    password: &str,
) -> anyhow::Result<bool> {
    if identity::user_by_email(pool, tenant_id, CLIENT_EMAIL)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    let hash = hash(password).await?;
    let mut tx = pool.begin().await?;
    let user = identity::create_user(
        &mut *tx,
        tenant_id,
        &identity::NewUser {
            email: CLIENT_EMAIL.into(),
            password_hash: Some(hash),
            kind: UserKind::Client,
            first_name: "Ayşe".into(),
            last_name: "Yılmaz".into(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await?;
    clients::create_client_with_owner(
        &mut tx,
        tenant_id,
        &clients::ClientInput::individual("Ayşe", "Yılmaz", CLIENT_EMAIL, Currency::TRY),
        user.id,
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

struct MockSpec {
    category: IntegrationCategory,
    kind: &'static str,
    name: &'static str,
    config: Value,
    secrets: Value,
}

async fn seed_integrations(
    pool: &PgPool,
    config: &AppConfig,
    tenant_id: TenantId,
) -> anyhow::Result<u32> {
    let secret_box = config.secret_box();
    let specs = [
        MockSpec {
            category: IntegrationCategory::Virt,
            kind: hos_virt::MOCK,
            name: "Mock sanallaştırma",
            config: json!({"adopt_unknown_vms": true}),
            secrets: json!({}),
        },
        MockSpec {
            category: IntegrationCategory::Panel,
            kind: hos_panels::MOCK,
            name: "Mock hosting paneli",
            config: json!({"packages": demo_panel_packages()}),
            secrets: json!({}),
        },
        MockSpec {
            category: IntegrationCategory::Registrar,
            kind: hos_domains::mock::KIND,
            name: "Mock kayıt firması",
            // The mock's price list is in USD (`.com` 9.20).
            config: json!({"currency": MOCK_REGISTRAR_CURRENCY, "default_nameservers": "ns1.demo.test, ns2.demo.test"}),
            secrets: json!({}),
        },
        MockSpec {
            category: IntegrationCategory::Gateway,
            kind: hos_payments::MOCK,
            name: "Mock ödeme sağlayıcısı",
            config: json!({}),
            secrets: json!({"webhook_secret": hos_core::crypto::generate_token()}),
        },
    ];
    let mut created = 0;
    let mut ids: Vec<(IntegrationCategory, IntegrationId)> = Vec::new();
    // The mock registrar was created or repaired in this run.
    let mut registrar_set_up = false;
    for spec in specs {
        if let Some(existing) =
            integrations::active_by_kind(pool, tenant_id, spec.category, spec.kind).await?
        {
            if spec.category == IntegrationCategory::Registrar
                && repair_mock_registrar(pool, tenant_id, &existing).await?
            {
                registrar_set_up = true;
            }
            // Older demo databases: the mock panel without the products' packages.
            if spec.category == IntegrationCategory::Panel
                && existing.config.get("packages").is_none()
            {
                let mut cfg = existing.config.clone();
                if let Some(obj) = cfg.as_object_mut() {
                    obj.insert("packages".into(), demo_panel_packages());
                }
                integrations::update_integration(
                    pool,
                    tenant_id,
                    existing.id,
                    &existing.name,
                    &cfg,
                    existing.is_active,
                )
                .await?;
            }
            ids.push((spec.category, existing.id));
            continue;
        }
        let id = IntegrationId::new();
        let sealed = secret_box
            .encrypt_json(&spec.secrets, &integration_aad(tenant_id, id))
            .map_err(|e| anyhow::anyhow!("encrypting integration secrets: {e}"))?;
        integrations::create_integration(
            pool,
            tenant_id,
            &integrations::NewIntegration {
                id,
                category: spec.category,
                kind: spec.kind.to_owned(),
                name: spec.name.to_owned(),
                config: spec.config,
                secrets_enc: Some(sealed),
                is_active: true,
                sort_order: 0,
            },
        )
        .await?;
        ids.push((spec.category, id));
        created += 1;
        registrar_set_up |= spec.category == IntegrationCategory::Registrar;
    }

    let id_of = |c: IntegrationCategory| ids.iter().find(|(cat, _)| *cat == c).map(|(_, id)| *id);
    // Link products without a module to the matching mock.
    let products = catalog::catalog(pool, tenant_id, Currency::TRY, true).await?;
    for p in products.iter().flat_map(|g| g.products.iter()) {
        if p.product.integration_id.is_some() {
            continue;
        }
        let target = match p.product.product_type {
            ProductType::SharedHosting | ProductType::ResellerHosting | ProductType::Email => {
                id_of(IntegrationCategory::Panel)
            }
            ProductType::Vps
            | ProductType::Dedicated
            | ProductType::Cloud
            | ProductType::ManagedDb
            | ProductType::ObjectStorage
            | ProductType::ContainerApp => id_of(IntegrationCategory::Virt),
            _ => None,
        };
        if let Some(integration) = target {
            catalog::set_product_integration(pool, tenant_id, p.product.id, Some(integration))
                .await?;
        }
    }
    if let Some(registrar) = id_of(IntegrationCategory::Registrar) {
        domains::assign_default_registrar(pool, tenant_id, registrar).await?;
    }
    if registrar_set_up && keep_demo_tld_prices(pool, tenant_id).await? {
        println!("demo: nightly TLD price sync from the mock registrar disabled");
    }
    Ok(created)
}

/// Currency of the demo's mock registrar: the currency its price list is in.
const MOCK_REGISTRAR_CURRENCY: &str = "USD";

/// Older demo databases configured the mock registrar with `TRY`, so its
/// USD price list (`.com` 9.20) became 9.20 TRY of cost and the nightly TLD
/// price sync priced the demo's `.com` at 0.23 USD. Sets the registrar's
/// real currency and puts the seeded TLD prices back; `true` when it did.
async fn repair_mock_registrar(
    pool: &PgPool,
    tenant_id: TenantId,
    existing: &integrations::IntegrationRow,
) -> anyhow::Result<bool> {
    let currency = existing.config.get("currency").and_then(Value::as_str);
    if currency.is_none_or(|c| c.eq_ignore_ascii_case(MOCK_REGISTRAR_CURRENCY)) {
        return Ok(false);
    }
    let mut cfg = existing.config.clone();
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("currency".into(), json!(MOCK_REGISTRAR_CURRENCY));
    }
    integrations::update_integration(
        pool,
        tenant_id,
        existing.id,
        &existing.name,
        &cfg,
        existing.is_active,
    )
    .await?;
    let restored = hos_db::seed::reset_tld_prices(pool, tenant_id).await?;
    println!(
        "mock registrar: currency {} → {MOCK_REGISTRAR_CURRENCY}; {restored} seeded TLD prices restored",
        currency.unwrap_or_default()
    );
    Ok(true)
}

/// The demo's TLD prices are curated (seeded in TRY, USD and EUR); the mock
/// registrar's price list is made up. Its nightly `sync_tld_pricing` task
/// would replace them with the mock's costs plus margin, so the demo tenant
/// gets the task disabled when its mock registrar is set up (staff can switch
/// it back on under Settings → Automation, or sync once from the TLD page; a
/// later `seed-demo` leaves it alone).
pub(crate) async fn keep_demo_tld_prices(
    pool: &PgPool,
    tenant_id: TenantId,
) -> anyhow::Result<bool> {
    hos_jobs::Scheduler::new(pool.clone(), hos_jobs::SchedulerConfig::default())
        .ensure_tenant_tasks(tenant_id)
        .await
        .map_err(|e| anyhow::anyhow!("scheduled tasks: {e}"))?;
    let task = hos_db::repo::jobs::list_scheduled_tasks(pool, Some(tenant_id))
        .await?
        .into_iter()
        .find(|t| t.name == "sync_tld_pricing");
    if let Some(task) = task.filter(|t| t.is_active) {
        hos_db::repo::jobs::set_task_active(pool, task.id, false).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Mock panel packages named like the seeded hosting products'
/// `module_config.package` (`starter` / `pro` / `business`).
pub(crate) fn demo_panel_packages() -> Value {
    let pkg = |id: &str, name: &str, disk_gb: u64, domains: u32| {
        json!({
            "id": id,
            "name": name,
            "limits": {
                "disk_mb": disk_gb * 1024,
                "bandwidth_mb": disk_gb * 10 * 1024,
                "domains": domains,
                "email_accounts": domains * 10,
                "databases": domains * 2
            }
        })
    };
    json!([
        pkg("starter", "Starter", 5, 1),
        pkg("pro", "Pro", 25, 5),
        pkg("business", "Business", 75, 25),
    ])
}

/// One unpaid VPS S order for the demo client `client_email` (only when the
/// client has no order yet), placed through the real checkout. Returns the
/// order number.
pub(crate) async fn seed_demo_order(
    pool: &PgPool,
    config: &AppConfig,
    tenant_id: TenantId,
    client_email: &str,
) -> anyhow::Result<Option<String>> {
    let Some(user) = identity::user_by_email(pool, tenant_id, client_email).await? else {
        return Ok(None);
    };
    let Some(client) = clients::clients_of_user(pool, tenant_id, user.id)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let orders: i64 =
        sqlx::query_scalar("select count(*) from orders where tenant_id = $1 and client_id = $2")
            .bind(tenant_id)
            .bind(client.id)
            .fetch_one(pool)
            .await?;
    if orders > 0 {
        return Ok(None);
    }
    let Some(product) = catalog::product_by_slug(pool, tenant_id, "vps-s").await? else {
        return Ok(None);
    };
    let cart = hos_db::repo::carts::create_cart(
        pool,
        tenant_id,
        client.currency,
        Some(user.id),
        Some(client.id),
        None,
    )
    .await?;
    let mut item = hos_db::repo::carts::NewCartItem::product(product.id, BillingCycle::Monthly);
    item.hostname = Some("vps1.demo.test".into());
    // The first operating system offered for the product: without one,
    // paying the order would end in a failed provisioning.
    let os: Option<(Uuid, Uuid)> = sqlx::query_as(
        "select o.id, v.id
           from config_options o
           join config_option_group_products gp
             on gp.tenant_id = o.tenant_id and gp.group_id = o.group_id
           join config_option_values v on v.tenant_id = o.tenant_id and v.option_id = o.id
          where o.tenant_id = $1 and gp.product_id = $2 and o.module_key = 'os_template'
          order by o.sort_order, v.sort_order
          limit 1",
    )
    .bind(tenant_id)
    .bind(product.id)
    .fetch_optional(pool)
    .await?;
    if let Some((option_id, value_id)) = os {
        item.config_selections = json!({ option_id.to_string(): { "value_id": value_id } });
    }
    hos_db::repo::carts::add_item(pool, tenant_id, cart.id, &item).await?;
    // The demo runs on mock integrations: opt in for this engine only.
    let mut billing_config = hos_billing::BillingConfig::new(config.base_url.as_str());
    billing_config.allow_mock_integrations = true;
    let billing = hos_billing::Billing::new(pool.clone(), config.secret_box(), billing_config);
    let out = billing
        .checkout(
            tenant_id,
            &hos_billing::checkout::CheckoutRequest {
                cart_id: cart.id,
                client_id: client.id,
                user_id: Some(user.id),
                payment_integration_id: None,
                ip: None,
                user_agent: None,
                notes: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("demo checkout: {e}"))?;
    Ok(Some(out.order_number))
}

struct PostSeed {
    slug_tr: &'static str,
    slug_en: &'static str,
    title_tr: &'static str,
    title_en: &'static str,
    excerpt_tr: &'static str,
    excerpt_en: &'static str,
    body_tr: &'static str,
    body_en: &'static str,
    category: &'static str,
    tags: &'static [&'static str],
    days_ago: i64,
}

const POSTS: &[PostSeed] = &[
    PostSeed {
        slug_tr: "wordpress-sitenizi-hizlandirmanin-7-yolu",
        slug_en: "7-ways-to-speed-up-your-wordpress-site",
        title_tr: "WordPress sitenizi hızlandırmanın 7 yolu",
        title_en: "7 ways to speed up your WordPress site",
        excerpt_tr: "Önbellek, görsel optimizasyonu ve doğru PHP sürümüyle sayfa yükleme süresini kısaltın.",
        excerpt_en: "Shorten page load times with caching, image optimisation and the right PHP version.",
        body_tr: "Hızlı bir site hem ziyaretçileriniz hem de arama motorları için önemlidir. Aşağıdaki adımlar çoğu WordPress sitesinde belirgin bir iyileşme sağlar.\n\n## 1. Güncel PHP sürümü kullanın\n\nPHP 8.x, eski sürümlere göre daha az bellek kullanır ve istekleri daha hızlı işler. Sürümü kontrol panelinizden tek tıkla değiştirebilirsiniz.\n\n## 2. Sayfa önbelleğini açın\n\nLiteSpeed Cache gibi bir eklenti, dinamik sayfaların hazır kopyasını sunarak veritabanı sorgularını azaltır.\n\n## 3. Görselleri sıkıştırın\n\nGörselleri WebP biçimine dönüştürün ve sayfada gösterildikleri boyutta yükleyin.\n\n## 4. Kullanmadığınız eklentileri kaldırın\n\nHer eklenti ek kod ve sorgu demektir. Pasif eklentileri de silin.\n\n## 5. Tembel yüklemeyi (lazy loading) kullanın\n\nEkranın altında kalan görseller ancak kullanıcı kaydırdığında yüklenmelidir.\n\n## 6. Veritabanını düzenli temizleyin\n\nRevizyonlar, çöp kutusundaki yorumlar ve geçici veriler tabloları şişirir.\n\n## 7. Doğru paketi seçin\n\nTrafiğiniz arttıkça NVMe disk ve ayrılmış kaynak sunan bir pakete geçmek en etkili adımdır.",
        body_en: "A fast site matters to your visitors and to search engines alike. The steps below make a noticeable difference on most WordPress sites.\n\n## 1. Use a current PHP version\n\nPHP 8.x uses less memory than older releases and handles requests faster. You can switch versions in your control panel with one click.\n\n## 2. Turn on page caching\n\nA plugin such as LiteSpeed Cache serves ready-made copies of dynamic pages and cuts database queries.\n\n## 3. Compress your images\n\nConvert images to WebP and upload them at the size they are displayed.\n\n## 4. Remove unused plugins\n\nEvery plugin means extra code and queries. Delete inactive plugins too.\n\n## 5. Use lazy loading\n\nImages below the fold should load only when the visitor scrolls to them.\n\n## 6. Clean your database regularly\n\nRevisions, trashed comments and transients bloat your tables.\n\n## 7. Pick the right plan\n\nAs traffic grows, moving to a plan with NVMe storage and dedicated resources is the most effective step.",
        category: "hosting",
        tags: &["wordpress", "performans"],
        days_ago: 3,
    },
    PostSeed {
        slug_tr: "vps-mi-paylasimli-hosting-mi",
        slug_en: "vps-or-shared-hosting",
        title_tr: "VPS mi, paylaşımlı hosting mi? Doğru seçimi yapmak",
        title_en: "VPS or shared hosting? Making the right choice",
        excerpt_tr: "İki hizmetin farklarını, maliyetlerini ve hangi durumda hangisinin uygun olduğunu karşılaştırdık.",
        excerpt_en: "We compare the two services, their costs and when each one is the right fit.",
        body_tr: "Paylaşımlı hosting ve VPS aynı işi farklı yollarla yapar: sitenizi internete açar. Aradaki fark, kaynakların nasıl paylaşıldığı ve ne kadar kontrol sahibi olduğunuzdur.\n\n## Paylaşımlı hosting\n\n- Sunucu yönetimi sağlayıcıya aittir.\n- Kontrol paneli, e-posta ve SSL hazır gelir.\n- Kurumsal siteler, bloglar ve küçük e-ticaret siteleri için idealdir.\n\n## VPS\n\n- Size ayrılmış işlemci ve bellek bulunur.\n- Root erişimiyle istediğiniz yazılımı kurarsınız.\n- Özel uygulamalar, yoğun trafik ve geliştirme ortamları için uygundur.\n\n## Nasıl karar verilir?\n\nSunucu yönetmek istemiyorsanız paylaşımlı hostingle başlayın. Özel yazılım ihtiyacınız veya düzenli kaynak sınırı uyarılarınız varsa VPS'e geçin.",
        body_en: "Shared hosting and a VPS do the same job in different ways: they put your site on the internet. The difference is how resources are shared and how much control you have.\n\n## Shared hosting\n\n- The provider manages the server.\n- Control panel, email and SSL come ready to use.\n- Ideal for company sites, blogs and small online stores.\n\n## VPS\n\n- CPU and memory are reserved for you.\n- Root access lets you install any software.\n- A fit for custom applications, heavy traffic and development environments.\n\n## How to decide\n\nIf you do not want to manage a server, start with shared hosting. Move to a VPS when you need custom software or keep hitting resource limits.",
        category: "rehber",
        tags: &["vps", "hosting"],
        days_ago: 10,
    },
    PostSeed {
        slug_tr: "alan-adi-secerken-dikkat-edilmesi-gerekenler",
        slug_en: "choosing-a-domain-name",
        title_tr: "Alan adı seçerken dikkat edilmesi gerekenler",
        title_en: "What to consider when choosing a domain name",
        excerpt_tr: "Kısa, akılda kalıcı ve markanızla uyumlu bir alan adı için pratik öneriler.",
        excerpt_en: "Practical tips for a short, memorable domain that fits your brand.",
        body_tr: "Alan adınız, markanızın internetteki adresidir. İyi bir seçim yıllarca işinize yarar.\n\n## Kısa ve okunaklı olsun\n\nTelefonda söylendiğinde yanlış yazılmayacak bir ad seçin. Tire ve rakamlardan mümkünse kaçının.\n\n## Doğru uzantıyı seçin\n\nTürkiye'de faaliyet gösteriyorsanız .com.tr veya .tr güven verir; uluslararası projelerde .com hâlâ en bilinen uzantıdır.\n\n## Marka haklarını kontrol edin\n\nBaşkasına ait bir markayı içeren alan adları hukuki sorun yaratabilir.\n\n## Otomatik yenilemeyi açın\n\nSüresi dolan alan adları kısa sürede başkaları tarafından alınabilir. Otomatik yenileme ve güncel iletişim bilgileri bu riski ortadan kaldırır.",
        body_en: "Your domain is your brand's address on the internet. A good choice serves you for years.\n\n## Keep it short and readable\n\nPick a name that will not be misspelled when said over the phone. Avoid hyphens and digits where possible.\n\n## Choose the right extension\n\nFor businesses in Turkey .com.tr or .tr build trust; for international projects .com is still the best-known extension.\n\n## Check trademark rights\n\nDomains containing someone else's trademark can cause legal trouble.\n\n## Turn on auto-renewal\n\nExpired domains can be registered by others quickly. Auto-renewal and up-to-date contact details remove that risk.",
        category: "alan-adlari",
        tags: &["alan adı", "marka"],
        days_ago: 21,
    },
];

/// Sample blog posts (tr + en); existing slugs are skipped.
pub(crate) async fn seed_blog(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
) -> anyhow::Result<u32> {
    let mut created = 0;
    for p in POSTS {
        let group = Uuid::now_v7();
        let published_at = Utc::now() - Duration::days(p.days_ago);
        for (locale, slug, title, excerpt, body) in [
            (Locale::Tr, p.slug_tr, p.title_tr, p.excerpt_tr, p.body_tr),
            (Locale::En, p.slug_en, p.title_en, p.excerpt_en, p.body_en),
        ] {
            let exists: bool = sqlx::query_scalar(
                "select exists(select 1 from blog_posts where tenant_id = $1 and locale = $2 and slug = $3)",
            )
            .bind(tenant_id)
            .bind(locale)
            .bind(slug)
            .fetch_one(&mut *conn)
            .await?;
            if exists {
                continue;
            }
            // Reuse the translation group of an already seeded sibling.
            let sibling: Option<Uuid> = sqlx::query_scalar(
                "select translation_group_id from blog_posts where tenant_id = $1 and slug = any($2) limit 1",
            )
            .bind(tenant_id)
            .bind(vec![p.slug_tr, p.slug_en])
            .fetch_optional(&mut *conn)
            .await?;
            let minutes = i32::try_from(crate::markdown::reading_minutes(body)).unwrap_or(1);
            content::create_blog_post(
                &mut *conn,
                tenant_id,
                &content::BlogPostInput {
                    slug: slug.to_owned(),
                    locale,
                    translation_group_id: Some(sibling.unwrap_or(group)),
                    title: title.to_owned(),
                    excerpt: Some(excerpt.to_owned()),
                    body: body.to_owned(),
                    cover_image_url: None,
                    category: Some(p.category.to_owned()),
                    tags: p.tags.iter().map(|t| (*t).to_owned()).collect(),
                    status: ContentStatus::Published,
                    published_at: Some(published_at),
                    reading_minutes: Some(minutes),
                    author_id: None,
                    author_name: Some(match locale {
                        Locale::Tr => "Demo Hosting Ekibi".to_owned(),
                        Locale::En => "Demo Hosting Team".to_owned(),
                    }),
                },
            )
            .await?;
            created += 1;
        }
    }
    Ok(created)
}
