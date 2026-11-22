<img src="assets/icon.svg" width="64" height="64" alt="">

# HostingOS

[English](README.md) · **Türkçe**

HostingOS, bir hosting firmasını yürüten açık kaynaklı bir yazılımdır:
firmanın mağazası (vitrini), müşteri paneli ve yönetim paneli; faturalama,
hosting hesaplarının ve sanal sunucuların otomatik kurulumu, alan adı
yönetimi ve destek talepleriyle birlikte tek bir uygulamada. Kendi
kurulumunu işletmek isteyen hosting firmaları için WHMCS'e alternatif
olarak yazıldı. Tek bir Rust binary'si (`hostingos`) ve PostgreSQL'den
oluşur. Adındaki "OS"a rağmen bir işletim sistemi değildir.

> **Erken sürüm.** HostingOS 0.1.0 sürümünde ve henüz etiketlenmiş bir
> sürümü yok. Üçüncü taraf entegrasyonların tamamı şimdiye kadar yalnızca
> sağlayıcıların belgelerine ve sahte (mock) sunuculara karşı test edildi,
> gerçek hesaplarla denenmedi. Gerçek müşteriler ve gerçek ödemelerle
> kullanmadan önce [Bilinen sınırlamalar](#bilinen-sinirlamalar) bölümünü
> okuyun.

![HostingOS mağazası, "Klasik" şablon](docs/screenshots/storefront-home-classic.jpg)

Ekran görüntüleri Türkçe arayüzden alındı; her sayfanın İngilizcesi de
var. Diğer görüntüler: [docs/SCREENSHOTS.md](docs/SCREENSHOTS.md).

## Neler yapabiliyor?

**Mağaza.** Sunucu tarafında oluşturulan Türkçe ve İngilizce sayfalar; altı
şablon (Klasik, Bulut, Kurumsal, Editoryal, Stüdyo, Terminal), açık ve
koyu tema, ürün grupları ve yapılandırılabilir seçenekler, paketler
(bundle), para birimi seçici, alan adı sorgulama, bilgi bankası, blog,
duyurular, dosya indirme merkezi, ağ durumu sayfası, iletişim formu ve
açılış sayfaları. SEO tarafında canonical adresler, hreflang, JSON-LD,
sitemap, `robots.txt`, `llms.txt`, sayfaların Markdown sürümleri,
IndexNow, yönlendirmeler ve site denetimi.

**Sipariş ve faturalama.** Ürün, eklenti, paket ve alan adını aynı sepette
toplayan sipariş akışı; kuponlar, ülke ya da bölgeye göre vergi kuralları,
kurlarıyla birlikte birden çok para birimi, müşteri bakiyesi. Sipariş
faturaya dönüşür, ödeme gelince hizmet kendiliğinden kurulur. Yinelenen
faturalar, hatırlatmalar, gecikme bedeli, iadeler, kısmi ödemeler, birden
çok faturayı tek seferde ödeme, bakiye yükleme, fatura PDF'leri ve
müşterinin çevrim içi onaylayabildiği teklifler.

**Otomatik kurulum.** cPanel/WHM, Plesk, DirectAdmin ve CyberPanel üzerinde
hosting hesapları (açma, askıya alma, askıdan çıkarma, silme, paket
değiştirme, parola sıfırlama, tek tıkla panele giriş). Proxmox VE, VMware,
Virtualizor ve Hetzner Cloud üzerinde sanal sunucular: açma/kapama,
yeniden kurulum, snapshot, yedek, tarayıcıdan konsol, kurtarma modu, ISO,
güvenlik duvarı, ters DNS, grafikler, SSH anahtarları ve IP havuzları;
her işlem, platformun desteklediği ölçüde.

**Alan adları.** ResellerClub, Namecheap ya da OpenProvider üzerinden
kayıt, transfer, yenileme, name server, kilit, gizlilik, EPP kodu, kişi
bilgileri ve DNS; RDAP ile müsaitlik sorgusu; uzantı fiyat listeleri.

**Müşteri paneli.** Hizmetler, alan adları, faturalar, ödeme yöntemleri ve
bakiye, teklifler, destek talepleri, dosyalar, ortaklık (affiliate)
programı, iki adımlı doğrulama, oturumlar, API anahtarları ve yalnızca
izin verilen bölümleri gören alt kullanıcılar.

**Destek.** Departmanlar, ekli talepler, hazır yanıtlar, iç notlar,
öncelikler ve toplu işlemler. Talepler e-postayla da açılıp yanıtlanabilir
(Mailgun inbound route ya da ham ileti uç noktası); tanınmayan
gönderenlerin e-postaları karantinaya alınır.

**Yönetim paneli.** Müşteriler, siparişler, faturalar, hizmetler, alan
adları, ürün kataloğu, entegrasyonlar, e-posta şablonları, ayrıntılı
yetkilere sahip personel rolleri, denetim kaydı, müşteri hesabına geçiş
(impersonation), genel arama, yapılacaklar listesi, yasak listeleriyle
sipariş dolandırıcılık kontrolü ve raporlar (gelir, MRR ve kayıp oranı,
vergi, vadesi geçmiş alacaklar, en çok satanlar, alan adı yenilemeleri,
CSV dışa aktarma).

**Türkiye'ye özel.** Nilvera ya da Paraşüt üzerinden e-Fatura ve e-Arşiv
(UBL-TR), Türkiye'de yaygın ödeme altyapıları (iyzico, PayTR, Nestpay,
Papara, Paynet, havale/EFT), VKN / TCKN kontrolü, Türkçe arayüz ve TL.

**Entegrasyon ve API.** API anahtarlı REST API (`/api/v1`): sayfalama,
idempotency anahtarları, hız sınırları ve OpenAPI 3.1 tanımı. İmzalı giden
webhook'lar; ödeme altyapılarından ve registrar'lardan gelen imzalı
webhook'lar ve her birinin yalnızca bir kez işlenmesi. WHMCS'ten aktarım:
müşteriler, ürünler, hizmetler, alan adları, faturalar, destek talepleri
ve daha fazlası, WHMCS veritabanından ya da API'sinden; deneme çalıştırması
ve geri alma ile.

**İşletim.** Web sunucusu, arka plan iş kuyruğu ve zamanlayıcı tek binary'de
(ya da ayrı bir worker sürecinde); programa gömülü veritabanı
migration'ları; otomatik TLS için Caddy'li Docker imajı ve Compose dosyası;
systemd birimi; imzalı sürümlere karşı güncelleme denetimi, binary
kurulumlarında tek tıkla kurulum ve sorun çıkarsa otomatik geri alma;
güncellemeler, bakım modu ve sistem durumu, yönetim panelinde firma
sahibine açık Sunucu bölümündedir (`/admin/server`). İçerik
ve SEO için isteğe bağlı yapay zekâ taslakları (Anthropic API anahtarı
gerekir).

**Güvenlik.** argon2id parola özetleri, CSRF koruması, sıkı Content
Security Policy, TOTP ile iki adımlı doğrulama, OAuth ile giriş (Google,
GitHub), hız sınırları, şifreli saklanan entegrasyon bilgileri ve sunucunun
entegrasyonlara açtığı bağlantılar için koruma
([docs/INSTALL.tr.md](docs/INSTALL.tr.md#dis-baglantilar)).

| Entegrasyon | Sağlayıcılar |
|---|---|
| Ödeme altyapıları | Stripe, iyzico, PayTR, PayPal, Nestpay (Payten), Papara, Paynet, havale/EFT |
| Hosting panelleri | cPanel/WHM, Plesk, DirectAdmin, CyberPanel |
| Sanallaştırma | Proxmox VE, VMware vSphere (vCenter üzerinden), Virtualizor, Hetzner Cloud |
| Alan adı registrar'ları | ResellerClub, Namecheap, OpenProvider; RDAP sorgusu |
| E-posta | SMTP, Resend, Postmark, Mailgun |
| e-Fatura / e-Arşiv | Nilvera, Paraşüt |

Geliştirme ve demo için ödeme, panel, sanallaştırma, registrar ve e-Fatura
kategorilerinde birer `mock` sağlayıcı, e-postada da yalnızca loga yazan
bir gönderici var; canlı kurulumda mock sağlayıcılar, açıkça izin
verilmedikçe reddedilir. Ayrıntılı belgeler (İngilizce): özellikler
[docs/FEATURES.md](docs/FEATURES.md), ödemeler
[docs/PAYMENTS.md](docs/PAYMENTS.md), e-Fatura
[docs/EINVOICE.md](docs/EINVOICE.md), API [docs/API.md](docs/API.md),
WHMCS aktarımı [docs/IMPORT-WHMCS.md](docs/IMPORT-WHMCS.md).

## Canlı demo

**Demo:** https://hostingos.sunucu.com.tr

Aşağıdaki herkese açık hesaplardan biriyle giriş yapın (giriş sayfası bu
hesapları, formu tek tıkla dolduran bir düğmeyle birlikte gösterir):

| Rol | Giriş sayfası | E-posta | Parola |
|---|---|---|---|
| Hosting firması sahibi (yönetim paneli) | `/auth/login`, ardından `/admin` | `admin@demo.test` | `demo-admin` |
| Müşteri (ödenmemiş bir VPS siparişiyle) | `/auth/login`, ardından `/client` | `musteri@demo.test` | `demo-client` |

- **Her şey 30 dakikada bir sıfırlanır.** Ziyaretçilerin girdiği bütün
  veriler silinir, demo aynı örnek verilerle yeniden kurulur. Her sayfadaki
  bant, bir sonraki sıfırlamaya kalan süreyi gösterir.
- **Bu hesapları herkes ortak kullanır.** Yazdıklarınız bir sonraki
  sıfırlamaya kadar diğer ziyaretçilere görünür. Gerçek kişisel bilgi
  girmeyin.
- **Neredeyse her şeyi deneyebilirsiniz.** Sipariş verip mock ödeme
  altyapısıyla ödeyin ve VPS'in mock sanallaştırma platformunda
  kurulmasını izleyin; müşterileri, faturaları ve destek taleplerini
  yönetin, ürünleri, fiyatları ve sayfaları düzenleyin, mağazanın
  görünümünü değiştirin.
- **Sunucudan dışarı hiçbir şey çıkmaz.** Yalnızca mock sağlayıcılar
  çalışır. Demonun "gönderdiği" e-postalar demo posta kutusunda
  (`/demo/mailbox`) görünür.
- Yalnızca başka sunuculara ulaşan, diğer ziyaretçileri dışarıda
  bırakabilecek ya da kod çalıştıran işlemler kapalıdır: kayıt olma ve
  parola sıfırlama, ortak hesaplarda değişiklik, gerçek entegrasyonlar ve
  webhook'lar, WHMCS aktarımı ve REST API üzerinden yazma. Yalnızca görsel
  dosyalar yüklenebilir.

Demo modunun nasıl çalıştığı: [docs/DEMO.md](docs/DEMO.md) (İngilizce);
kendi demonuzu kurmak için:
[docs/deploy/DEMO-DEPLOY.tr.md](docs/deploy/DEMO-DEPLOY.tr.md).

## Hızlı başlangıç (Docker Compose)

Bu adımlar tek bir Linux sunucuda canlı kullanıma uygun bir kurulum
yapar: PostgreSQL 16, HostingOS ve TLS sertifikalarını kendisi alan Caddy.
systemd altında binary kurulumu da dahil eksiksiz rehber:
[docs/INSTALL.tr.md](docs/INSTALL.tr.md).

Gerekenler: Docker Engine ve Compose eklentisi kurulu, 80 ve 443 portları
açık bir sunucu ve DNS kaydı bu sunucuyu gösteren bir alan adı (örnekte
`www.example.com`).

```sh
git clone https://github.com/sunucu-com-tr/hostingos.git hostingos
cd hostingos
cp docs/deploy/env.example .env.production

# Üç gizli anahtarı üretin:
sed -i \
  -e "s|^POSTGRES_PASSWORD=.*|POSTGRES_PASSWORD=$(openssl rand -hex 32)|" \
  -e "s|^HOS_SECRET_KEY=.*|HOS_SECRET_KEY=$(openssl rand -base64 32)|" \
  -e "s|^HOS_TLS_ASK_SECRET=.*|HOS_TLS_ASK_SECRET=$(openssl rand -hex 32)|" \
  .env.production
```

`.env.production` dosyasında `HOS_BASE_URL` değerini sitenizin herkese
açık adresi (`https://www.example.com`), `ACME_EMAIL` değerini de Let's
Encrypt için kullanılacak e-posta adresi yapın. Şablondaki
`HOS_MODE=single` satırını olduğu gibi bırakın: kurulum tek firmanızı
çalıştırır ve sunucuya ulaşan her adres bu firmayı gösterir.
`HOS_SECRET_KEY`'in bir kopyasını güvenli bir yerde saklayın:
entegrasyon bilgilerini ve iki adımlı doğrulama anahtarlarını şifreler; o
olmadan bir veritabanı yedeğinin değeri sınırlıdır.

```sh
# İmajı derleyip PostgreSQL'i, migration'ları, HostingOS'u ve Caddy'yi başlatın:
docker compose --env-file .env.production up -d --build

# Firmanızı ve sahip hesabını oluşturun (parola bir kez ekrana yazılır):
docker compose --env-file .env.production run --rm app setup \
  --email siz@example.com --company "Örnek Hosting"
```

İlk derleme HostingOS'u kaynak koddan derlediği için biraz sürer.
Veritabanı migration'ları uygulama başlamadan önce kendiliğinden çalışır
(tek seferlik `migrate` servisi). `setup` çalıştırılana kadar her sayfa
`503 Service Unavailable` yanıtı verir. `setup` ayrıca `--locale tr|en`,
`--currency` (varsayılanlar `tr` ve `TRY`) ve `--password` seçeneklerini
alır; parola verilmezse kendisi üretir.

Ardından `https://www.example.com/auth/login` adresinden giriş yapın;
yönetim paneli `/admin` altındadır. Hesabınız için iki adımlı doğrulamayı
açın, Entegrasyonlar altında ödeme altyapılarınızı, sunucularınızı,
registrar'ınızı ve e-posta sağlayıcınızı, ardından ürünlerinizi ekleyin.
E-posta entegrasyonu kurulana kadar e-postalar yalnızca loga yazılır.

DNS ve TLS ile uğraşmadan önce bakmak isterseniz kendi bilgisayarınızda
demo yığınını başlatın (`docker-compose.demo.yml`, bkz.
[docs/deploy/DEMO-DEPLOY.tr.md](docs/deploy/DEMO-DEPLOY.tr.md)). Demo
modunda çalışır: herkese açık hesaplar, mock entegrasyonlar, düzenli
sıfırlama.

## Yapılandırma

HostingOS ortam değişkenleriyle yapılandırılır. Önemli olanlar:

| Değişken | Zorunlu mu? | Anlamı |
|---|---|---|
| `DATABASE_URL` | evet | PostgreSQL bağlantı adresi (havuzlu olabilir) |
| `DATABASE_URL_DIRECT` | önerilir | Migration'lar ve iş kuyruğu için doğrudan bağlantı; verilmezse `DATABASE_URL` |
| `HOS_SECRET_KEY` | evet | 32 rastgele bayt, base64 (`openssl rand -base64 32`); saklanan gizli bilgileri şifreler. Yedekleyin |
| `HOS_BASE_URL` | canlıda | Sitenizin herkese açık adresi; `https` ise çerezler Secure olur |
| `HOS_MODE` | hayır | `single` (varsayılan): tek hosting firması; sunucuya ulaşan her adres bu firmayı gösterir |
| `HOS_BIND` | hayır | Dinlenen adres, varsayılan `127.0.0.1:8080` (imajda `0.0.0.0:8080`) |
| `HOS_TRUST_PROXY` | proxy arkasında | İstemci adresini reverse proxy'nin `X-Forwarded-For` başlığından oku |
| `HOS_TRUSTED_PROXY_HOPS` | hayır | Uygulamanın önündeki proxy sayısı, varsayılan `1` |
| `HOS_TRUSTED_PROXIES` | proxy arkasında önerilir | Proxy'lerin adresleri / CIDR aralıkları; iletilen başlıklar yalnızca onlardan gelince dikkate alınır (boşsa herkesten, uyarıyla) |
| `HOS_MIGRATE_ON_START` | hayır | `1`: bekleyen migration'ları hizmete başlamadan önce uygula (`hostingos serve --migrate` ile aynı) |
| `HOS_OUTBOUND_POLICY` | hayır | `private` (varsayılan: entegrasyonlar yerel ağınızdaki sunuculara ulaşabilir) ya da `public` (yalnızca genel adresler) |
| `HOS_TLS_ASK_SECRET` | Caddy ile | Caddy'nin sertifika kontrolü için paylaşılan anahtar |
| `HOS_UPDATE_REPO` | hayır | İmzalı sürümlerin aranacağı GitHub deposu (`sahip/ad`); verilmedikçe güncelleme denetimi kapalıdır |

Compose dosyası ayrıca `POSTGRES_PASSWORD` ve `ACME_EMAIL` değerlerini
okur. Bütün değişkenler:
[docs/INSTALL.tr.md](docs/INSTALL.tr.md#ortam-degiskenleri).

## Sürüm yükseltme

Önce veritabanının yedeğini alın (bkz.
[docs/INSTALL.tr.md](docs/INSTALL.tr.md#yedekleme)). Migration'lar yalnızca
ileri gider; yeni sürüm bunları başlamadan önce uygular.

- **Kaynaktan derlenen Docker Compose:** `git pull`, ardından
  `docker compose --env-file .env.production up -d --build`.
- **Yayımlanmış imajla Docker Compose:** ortam dosyasına `HOS_IMAGE` ve
  `HOS_TAG` yazın, sonra `docker compose --env-file .env.production pull`
  ve `up -d`.
- **systemd altında binary:** yeni sürümü yönetim panelinde Sunucu →
  Güncellemeler sayfasından (`/admin/server/updates`) kurun (yedek, bakım
  modu, migration'lar, sağlık kontrolü ve gerekirse otomatik geri alma
  kendiliğinden yapılır) ya da `hostingos update apply` komutunu
  kullanın.

Ayrıntılar: [docs/UPDATES.md](docs/UPDATES.md) (İngilizce). Yayımlanmış
imajlar ve imzalı sürümler henüz yok; ilk sürüme kadar kaynak koddan
derleyin.

## Güvenlik

Güvenlik açıklarını herkese açık issue'larda değil,
[SECURITY.md](SECURITY.md) dosyasında anlatıldığı gibi gizli olarak
bildirin.

<a id="bilinen-sinirlamalar"></a>

## Bilinen sınırlamalar

- **Erken sürüm.** Sürüm 0.1.0, henüz etiketlenmiş bir sürüm yok. Docker
  imajı, sürüm yayımlama hattı ve kendi kendini güncelleme yolu yeni ve
  henüz gerçek bir sürüm yayımında denenmedi. Sürüm imzalama anahtarı da
  henüz yok; bu yüzden ilk imzalı sürüme kadar tek tıkla güncelleme hiçbir
  şey kuramaz.
- **Entegrasyonlar gerçek hesaplarla denenmedi.** Ödeme altyapıları,
  sanallaştırma platformları, hosting panelleri, registrar'lar, e-Fatura
  entegratörleri ve Mailgun inbound route yalnızca sağlayıcıların
  belgelerine ve mock sunuculara karşı test edildi. Canlıya geçmeden önce
  her sağlayıcının test ortamında deneme işlemleri yapın.
- **WHMCS aktarımı** henüz canlı bir WHMCS veritabanı üzerinde
  çalıştırılmadı. Güvenmeden önce verilerinizin bir kopyası üzerinde
  deneme çalıştırması yapıp raporu okuyun
  ([docs/IMPORT-WHMCS.md](docs/IMPORT-WHMCS.md)).
- **Tek web süreci ya da yapışkan yönlendirme.** Hız sınırları her sürecin
  belleğinde tutulur; birden çok web süreci sınırları katlar
  ([docs/DEPLOYMENT.md](docs/DEPLOYMENT.md#scaling)).
- **Ödemeler:** kayıtlı kart her ödeme altyapısında yok; bkz.
  [docs/PAYMENTS.md](docs/PAYMENTS.md) içindeki "Known gaps".
- **e-Fatura:** iade faturaları, kısmi iadeler ve ters ibrazlar personele
  bırakılır ([docs/EINVOICE.md](docs/EINVOICE.md)).
- **Diller:** arayüz Türkçe ve İngilizcedir.

## Yol haritası

Sıradaki adımlar, belirli bir sıra olmadan: imzalı ilk sürüm ve yayımlanmış
imajlar; her entegrasyonun sağlayıcının test ya da canlı ortamında
denenmesi; gerçek veriyle deneme amaçlı bir WHMCS aktarımı; web süreçleri
arasında paylaşılan bir hız sınırlayıcı; katkıcı lisans sözleşmesi (CLA).

## Katkıda bulunma

Hata bildirimleri, sağlayıcıların test ortamlarında yaptığınız denemelerin
sonuçları ve pull request'ler memnuniyetle karşılanır.
[CONTRIBUTING.md](CONTRIBUTING.md) (derleme, testler, kurallar ve
sign-off kuralı) ile [davranış kurallarını](CODE_OF_CONDUCT.md) okuyun.
Mimari [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), web katmanı
[crates/hos-web/README.md](crates/hos-web/README.md) dosyasında anlatılır.
Değişiklikler [CHANGELOG.md](CHANGELOG.md) dosyasındadır.

## Lisans ve ticari seçenekler

HostingOS, GNU Affero Genel Kamu Lisansı sürüm 3 (yalnızca bu sürüm) ile
lisanslanmıştır ([LICENSE](LICENSE)). Ticari kullanım dahil her amaçla
kullanabilirsiniz. Değiştirilmiş bir sürümü ağ üzerinden hizmet olarak
sunarsanız, kaynak kodunu o hizmetin kullanıcılarına sunmanız gerekir.
[COMMERCIAL.tr.md](COMMERCIAL.tr.md) bunu sade bir dille açıklar ve ücretli
seçenekleri anlatır: AGPL'yi kabul edemeyen kurumlar için ticari lisans,
ücretli destek ve WHMCS'ten geçiş desteği. Üçüncü taraf bileşenler ve
lisansları: [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Projeyi destekleyin

HostingOS'u kullanmak ücretsizdir. İşinize yarıyorsa gelişimine şu
yollarla destek olabilirsiniz:

- bağış ya da sponsorluk seçenekleri hazır olduğunda bağış yaparak (burada
  ve deponun *Sponsor* düğmesinde yer alacaklar);
- destek ya da WHMCS'ten geçiş yardımı satın alarak
  ([COMMERCIAL.tr.md](COMMERCIAL.tr.md));
- hata bildirerek, entegrasyonları kendi test hesaplarınızla deneyerek,
  belgeleri ve çevirileri geliştirerek.

Bağış isteğe bağlıdır ve farklı koşullar satın almaz: herkes aynı yazılımı
aynı lisansla kullanır.
