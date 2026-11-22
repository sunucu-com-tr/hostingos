# HostingOS — Mimari ve Sözleşmeler

HostingOS; bir hosting şirketinin otomasyon, faturalandırma, müşteri paneli ve vitrin
(storefront) yazılımıdır. WHMCS'nin yerini almayı hedefler. Bir kurulum tek bir hosting şirketini
çalıştırır (`HOS_MODE=single`, varsayılan); birden fazla şirketi tek kurulumda barındıran çok firmalı
mod (`HOS_MODE=multi`) gelişmiş bir seçenektir (bkz. §3).

Bu belge **bağlayıcıdır**. Buradaki bir kararı değiştirmek gerekiyorsa kodu sessizce saptırma;
değişikliğin açıklamasında gerekçesiyle belirt.

## 1. Teknoloji kararları

| Konu | Karar |
|---|---|
| Dil | Rust (edition 2024), stable toolchain, `~/.cargo/bin` PATH'te olmayabilir → `. "$HOME/.cargo/env"` |
| HTTP | axum 0.8 + tokio + tower-http |
| DB | PostgreSQL, sqlx 0.8 (`runtime-tokio`, `tls-rustls`, `postgres`, `uuid`, `chrono`, `json`, `rust_decimal`, `migrate`) |
| Sorgular | **Yalnızca runtime sorgular** (`sqlx::query`, `query_as::<_, T>` + `FromRow`). `query!` makroları YASAK (derleme DB'ye bağlanmasın) |
| Şablon | Askama (SSR) + htmx (vendored, `static/vendor/`) + elle yazılmış CSS tasarım sistemi. Node/npm build adımı YOK |
| Para | `rust_decimal::Decimal` + ISO 4217 para birimi kodu. Asla f64 |
| ID | UUID v7 (`uuid::Uuid::now_v7()`), DB'de `uuid` |
| Zaman | `chrono::DateTime<Utc>`, DB'de `timestamptz` |
| Hata | kütüphane crate'lerinde `thiserror`; binary'de `anyhow` |
| HTTP istemci | `reqwest` (rustls, json). Her entegrasyon istemcisi timeout'lu |
| Async trait | `async-trait` |
| Log | `tracing` + `tracing-subscriber` |
| Sırlar | Entegrasyon kimlik bilgileri DB'de AES-256-GCM ile şifreli (`hos_core::crypto`), anahtar `HOS_SECRET_KEY` env |
| i18n | `tr` ve `en`; `locales/*.json`, firmanın varsayılan dili + kullanıcı tercihi |

Bağımlılık sürümleri kök `Cargo.toml` içindeki `[workspace.dependencies]` üzerinden yönetilir;
crate'ler `foo.workspace = true` kullanır. Yeni bağımlılık gerekiyorsa kök dosyaya ekle.

## 2. Workspace düzeni

```
Cargo.toml                 # workspace
crates/
  hos-core/                 # domain tipleri, hata, config, crypto, firma bağlamı, TÜM entegrasyon trait'leri
  hos-db/                   # migrations/ + repository fonksiyonları (firma kapsamlı)
  hos-auth/                 # parola (argon2id), oturum, OAuth (Google, GitHub), TOTP 2FA, RBAC
  hos-virt/                 # VirtProvider impl: proxmox, vmware (vSphere/ESXi), virtualizor, hetzner cloud
  hos-panels/               # HostingPanel impl: cpanel/WHM, plesk, directadmin, cyberpanel
  hos-domains/              # DomainRegistrar impl: resellerclub, namecheap, openprovider + RDAP/WHOIS
  hos-payments/             # PaymentGateway impl: stripe, iyzico, paytr, paypal, banka havalesi
  hos-einvoice/             # EInvoiceProvider impl: paraşüt, nilvera (+ UBL-TR 2.1, VKN/TCKN) — docs/EINVOICE.md
  hos-pdf/                  # fatura PDF'i (krilla, gömülü OFL font); DB'ye dokunmaz
  hos-billing/              # sipariş → fatura → ödeme → provizyon iş kuralları, vergi, kupon, kredi
  hos-seo/                  # SEO/GEO/AI: meta, JSON-LD, sitemap, robots, llms.txt, IndexNow, AI içerik (Claude API)
  hos-jobs/                 # Postgres tabanlı iş kuyruğu (SKIP LOCKED) + zamanlayıcı (cron görevleri)
  hos-web/                  # axum uygulaması (bin: hostingos): storefront, müşteri paneli, admin, sunucu sayfaları, REST API, webhook
locales/  static/  docs/
```

Bağımlılık yönü: `hos-core` ← her şey. Entegrasyon crate'leri (`virt`, `panels`, `domains`, `payments`, `seo`)
**yalnızca `hos-core`'a** bağımlıdır; DB'ye dokunmaz. `hos-billing` ve `hos-jobs` → `hos-core` + `hos-db`.
`hos-web` hepsini birleştirir.

## 3. Firma kapsamı ve çalışma modları

- Veri modeli her zaman firma kapsamlıdır (kodda firma = *tenant*): firmaya ait **her tabloda**
  `tenant_id uuid not null references tenants(id)`.
- Tüm repository fonksiyonları ilk argüman olarak `TenantId` alır ve sorguya `tenant_id = $1` koyar. Firma
  kapsamı olmayan sorgu yalnızca `platform_*` modüllerinde.
- **Tek firma modu** (`HOS_MODE=single`, varsayılan; `hos_web::mode`): veritabanında tam olarak bir firma
  vardır (`hostingos setup` oluşturur). Her `Host` o firmaya çözülür; "site bulunamadı" sayfası yoktur.
  Veritabanında birden fazla firma varsa sunucu açıklayıcı bir hatayla açılmaz. `/platform` yoktur (404).
  Sahibin sunucu sayfaları (`system.manage` izni; yalnızca `owner` rolünde) yönetim panelinde
  `/admin/server` altındadır: istek içeride `/platform/system` ve `/platform/updates` işleyicilerine
  yönlendirilir, yanıttaki bağlantılar `/admin/server`'a geri yazılır; yeniden kimlik doğrulama ve denetim
  kaydı sahibin kendi hesabıyla yapılır. Plan limitleri uygulanmaz.
- **Çok firmalı mod** (`HOS_MODE=multi`, gelişmiş): firma, isteğin `Host` başlığından `tenant_domains`
  tablosu ile çözülür (özel alan adı + `*.{HOS_BASE_DOMAIN}` alt alan adı); geliştirmede `localhost` →
  `HOS_DEV_TENANT`. Operatör konsolu `/platform`: firmalar, planlar, abonelikler, kayıt, güncellemeler.
- Aktörler:
  1. **Hosting şirketi personeli**: `/admin` — RBAC rollerine göre; sahip ayrıca `/admin/server`.
  2. **Müşteri** (hosting şirketinin müşterisi): `/` vitrin ve `/client` paneli.
  3. Yalnızca çok firmalı modda **operatör**: `/platform`.
- `users` tablosu firma kapsamlıdır (`unique(tenant_id, email)`); operatör hesapları ayrı `platform_admins`
  tablosunda. Tek firma modunda orada yalnızca parolasız, giriş yapamayan iç kayıt
  `server-owner+<slug>@hostingos.invalid` bulunur (güncelleme çalıştırmaları ve bildirimlerin yabancı
  anahtarı için).
- Firma markası: logo, renkler, alan adı, dil, para birimi, vergi ayarları, e-posta gönderici, SEO ayarları.

## 4. Modül (entegrasyon) modeli — WHMCS "module" karşılığı

- `integrations` tablosu: `(id, tenant_id, category, kind, name, config jsonb, secrets_enc bytea, is_active)`.
  `category` ∈ `virt | panel | registrar | gateway | mail | ai`. `kind` örn. `proxmox`, `cpanel`, `resellerclub`, `stripe`.
- Her entegrasyon crate'i bir fabrika sunar: `pub fn build(kind: &str, config: &serde_json::Value, secrets: &serde_json::Value) -> Result<Arc<dyn Trait>, IntegrationError>`
  ve `pub fn descriptors() -> Vec<IntegrationDescriptor>` (kind, görünen ad, config/secret alan şeması — admin formu bundan üretilir).
- Ürünler (`products`) bir `product_type`'a (`shared_hosting | reseller_hosting | vps | dedicated | cloud | domain | ssl | license | email | ai_service | managed_db | object_storage | container_app | other`) ve
  isteğe bağlı bir `integration_id` + `module_config jsonb`'ye sahiptir. "Yeni nesil ürünler" = `cloud`, `ai_service`, `managed_db`, `object_storage`, `container_app`.
- Hizmet (`services`) yaşam döngüsü: `pending → provisioning → active → suspended → terminated | cancelled | failed`.
  Provizyon işlemleri her zaman `hos-jobs` kuyruğu üzerinden asenkron çalışır ve `service_events` tablosuna loglanır.

### Trait özetleri (kesin imzalar `hos-core/src/integrations/` içinde)

- `VirtProvider`: `test_connection, list_nodes, list_templates, list_vms, get_vm, create_vm(VmSpec), power(VmRef, PowerAction{Start,Stop,Shutdown,Reboot,Reset}), resize, reinstall, delete_vm, list_snapshots, create_snapshot, rollback_snapshot, delete_snapshot, console(VmRef) -> ConsoleAccess, metrics(VmRef)`
- `HostingPanel`: `test_connection, list_packages, create_account(AccountSpec), suspend, unsuspend, terminate, change_package, change_password, usage, sso_url`
- `DomainRegistrar`: `test_connection, check_availability(Vec<String>), tld_pricing, register(DomainRegistration), transfer, renew, info, get_nameservers, set_nameservers, set_lock, get_auth_code, set_privacy, update_contacts, list_dns_records/upsert/delete (varsayılan: Unsupported)`
- `PaymentGateway`: `create_checkout(CheckoutRequest) -> CheckoutSession{Redirect{url}|Form{..}|Offline{instructions}}, parse_webhook(headers, body) -> Vec<PaymentEvent>, refund, capabilities`
- `Mailer`: `send(EmailMessage)`
- `AiProvider`: `complete(AiRequest) -> AiResponse` (varsayılan impl: Anthropic Claude Messages API, model env `HOS_AI_MODEL`, varsayılan `claude-opus-5-5`)

Desteklenmeyen işlemler `IntegrationError::Unsupported` döner; panik yok, `todo!()`/`unimplemented!()` YASAK.
Her HTTP entegrasyonu gerçek sağlayıcı API'sine göre yazılır (uydurma endpoint yok); emin olmadığın yerde resmi dokümana bak.
Birim testleri `wiremock` ile sahte sunucuya karşı yazılır — gerçek sağlayıcıya çağrı yapan test yok.

## 5. Veri modeli (tablo grupları)

- **Firma / operatör**: `tenants, tenant_domains, tenant_settings, platform_admins, platform_plans, tenant_subscriptions`
- **Kimlik**: `users (kind: client|staff), user_identities (oauth: google|github), sessions, roles, role_permissions, user_roles, totp_secrets, password_resets, email_verifications, api_keys, audit_log`
- **Müşteri**: `clients (şirket/fatura profili), client_users, contacts, client_notes, client_groups`
- **Katalog**: `product_groups, products, product_pricing (para birimi × dönem: monthly|quarterly|semiannual|annual|biennial|triennial|one_time + setup_fee), config_option_groups, config_options, config_option_values, product_addons, currencies, tax_rules, coupons`
- **Satış**: `carts, cart_items, orders, order_items, invoices, invoice_items, transactions, credits, refunds`
- **Hizmet**: `integrations, services, service_addons, service_events, vms (hizmet ↔ sanal makine eşlemesi), ip_pools, ip_addresses`
- **Alan adı**: `tlds, tld_pricing, domains, domain_contacts, dns_zones, dns_records`
- **Destek**: `ticket_departments, tickets, ticket_messages, ticket_attachments, canned_responses, kb_categories, kb_articles, announcements`
- **İçerik/SEO**: `pages (CMS), blog_posts, seo_meta (herhangi bir varlığa bağlı), redirects, seo_audits, ai_generations`
- **Sistem**: `jobs, scheduled_tasks, email_templates, email_log, webhooks_in (idempotency), webhook_endpoints (dışa), notifications`

Kurallar: tüm tablolarda `id uuid primary key`, `created_at`, uygun yerde `updated_at`; durum alanları `text` + `check` kısıtı
(Postgres enum tipi yok — migrasyonu zorlaştırıyor); para alanları `numeric(20,6)`; firma kapsamlı her tabloda `tenant_id` ile başlayan index.

## 6. Web katmanı

- Rotalar: `/` vitrin (SSR, SEO öncelikli) · `/auth/*` · `/client/*` · `/admin/*` (sahip: `/admin/server/*`) · `/platform/*` (yalnızca çok firmalı mod) · `/api/v1/*` (JSON, API key + oturum) · `/webhooks/{gateway|registrar}/{integration_id}`
- SEO/GEO/AI: her vitrin sayfasında başlık/açıklama/canonical/hreflang/OpenGraph, schema.org JSON-LD (`Organization`, `Product`+`Offer`, `FAQPage`, `BreadcrumbList`, `Article`),
  `/sitemap.xml`, `/robots.txt` (AI tarayıcı politikası firma ayarı), `/llms.txt` + `/llms-full.txt`, IndexNow ping, admin'de SEO denetimi ve AI ile içerik/meta üretimi.
- Güvenlik: CSRF token (form + htmx header), `HttpOnly; Secure; SameSite=Lax` oturum çerezi, oran sınırlama (auth uçları), argon2id, tüm admin işlemleri `audit_log`'a.
- Erişilebilir, duyarlı (mobil öncelikli), açık/koyu tema; firmanın marka renkleri CSS değişkenleri ile.

## 7. Ortam değişkenleri (`.env`, git'e girmez)

`DATABASE_URL` (uygulama, pooled) · `DATABASE_URL_DIRECT` (migrasyon, pooler'sız) · `HOS_SECRET_KEY` (32 bayt base64) ·
`HOS_BASE_URL` · `HOS_MODE` (`single` varsayılan | `multi`) · `HOS_BASE_DOMAIN` ve `HOS_DEV_TENANT` (yalnızca çok firmalı mod;
ikincisi yalnızca debug modunda) · `HOS_BIND` (varsayılan `127.0.0.1:8080`) ·
`GOOGLE_CLIENT_ID/SECRET` · `GITHUB_CLIENT_ID/SECRET` (varsayılan uygulama; yönetim paneli kendi OAuth uygulamasını `integrations` ile tanımlayabilir) ·
`ANTHROPIC_API_KEY` · `HOS_AI_MODEL`

Güvenlik ile ilgili olanlar:

- `HOS_OUTBOUND_POLICY` = `private` (varsayılan) | `public`: yöneticilerin girdiği entegrasyon uç noktalarının
  (hypervisor/panel/SMTP/registrar/AI URL'leri) bağlanabileceği adresler (`hos_core::net::OutboundPolicy`).
  Loopback, link-local ve bulut metadata adresleri her iki değerde de yasak; `public` özel ağları da kapatır
  (çok firmalı modda birbirinden bağımsız firmalarla `public`). Geçersiz değerde `hostingos` açılmaz. `Unrestricted` yalnızca koddan
  (testler, `OutboundPolicy::set_global`) seçilebilir. Giden webhook'lar her zaman `Public` sınıflandırıcısını kullanır.
- `HOS_ALLOW_MOCK_INTEGRATIONS` (`1`/`true`/`yes`/`on`): `mock` ödeme sağlayıcısı / hypervisor / panel / registrar
  entegrasyonlarına izin verir; aksi hâlde hos-billing bunları kaydetmeyi ve çözmeyi reddeder. Debug modunda
  (`HOS_DEBUG`) otomatik açıktır ve açılışta uyarı loglanır; kod içinden `BillingConfig::allow_mock_integrations` /
  `registry().set_mock_integrations_allowed(true)` (seed-demo, testler). Üretimde asla.
- `HOS_TRUST_PROXY` + `HOS_TRUSTED_PROXY_HOPS` (varsayılan `1`): ters vekil arkasında istemci IP'si
  `X-Forwarded-For`'un sağdan bu kadarıncı girdisi; `0` başlığı yok sayar. Caddy ve Traefik gelen
  `X-Forwarded-For`'a varsayılan olarak güvenmez; CDN → vekil → uygulama için `2` ancak vekil CDN'in
  adres aralıklarına güveniyorsa doğrudur. `HOS_CLIENT_IP_HEADER` (ör. Cloudflare için `CF-Connecting-IP`)
  ayarlıysa ve tek geçerli adres içeriyorsa önce o kullanılır (`extract::ClientIp::from_config`).
  `HOS_TRUSTED_PROXIES` (virgülle ayrılmış IPv4/IPv6 adres ya da CIDR aralıkları): doluysa bu başlıklar
  ve `X-Forwarded-Proto` yalnızca TCP bağlantısı bu adreslerden geliyorsa dikkate alınır
  (`WebConfig::forwarded_headers_trusted`); boşsa eskisi gibi her bağlantıya güvenilir ve açılışta uyarı
  loglanır, geçersiz bir girdide `hostingos` açılmaz. İstemci adresini okuyan tek yer `extract::ClientIp`'dir.
- `HOS_HTTP_HEADER_TIMEOUT_SECS` (120) / `HOS_HTTP_BODY_TIMEOUT_SECS` (60) / `HOS_HTTP_REQUEST_TIMEOUT_SECS`
  (120): istek başlıklarının, gövdenin ve tüm isteğin süre sınırları (`0` kapatır). Başlık sınırı hyper'ın
  kendi zamanlayıcısıyla (`crate::server::serve`), diğer ikisi gövdeyi belleğe alan `server::limits`
  katmanıyla uygulanır; gövde demo sıfırlama kapısından önce okunur.
- `HOS_MIGRATE_ON_START` (`1`/`true`/`yes`/`on`) ya da `hostingos serve --migrate`: bekleyen migration'lar
  dinlemeye başlamadan önce, `hostingos migrate` ile aynı advisory lock altında uygulanır; demo modunda her
  zaman. Veritabanı sürümün gerisindeyse sayfalar 503, `/readyz` hazır değil döner ve migration uygulanınca
  yeniden başlatma gerekmeden açılır (`crate::schema`); ilerideyse yalnızca loglanır.

Not: sqlx bağlantı dizesinde `channel_binding` parametresini desteklemez — URL'den çıkarılmış hali kullanılır.

## 8. Kalite çıtası

- `cargo check -p <crate>` ve `cargo clippy -p <crate> -- -D warnings` temiz; `cargo test -p <crate>` geçer.
- `unwrap()/expect()` yalnızca testlerde ve kanıtlanabilir sabitlerde. `unsafe` yok.
- Yeni bağımlılık yalnızca kök `Cargo.toml`'daki `[workspace.dependencies]` üzerinden eklenir.
