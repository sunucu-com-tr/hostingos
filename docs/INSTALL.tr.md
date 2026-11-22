# HostingOS kurulumu

[English](INSTALL.md) · **Türkçe**

Bu rehber, HostingOS'u bir hosting firması için (firmanın mağazası,
müşteri paneli ve yönetim paneli) iki yoldan biriyle kurar:

- **A. Docker Compose:** Tek bir Linux sunucuda PostgreSQL, HostingOS ve
  TLS'i üstlenip sertifikaları kendisi alan Caddy.
- **B. systemd altında tek binary:** Bir reverse proxy'nin arkasında,
  HostingOS'un güncellemeleri yerinde kurabildiği dizin düzeniyle.

İki yol da aynı `hostingos` programını çalıştırır: web sunucusu, arka plan
iş kuyruğu, zamanlayıcı ve yönetim komutları tek bir binary'dedir.
Şablonlar ve veritabanı migration'ları programın içine derlenmiştir;
`static/` ve `locales/` dizinleri binary'nin yanındaki diskten okunur.
Ölçekleme ve ayrı worker süreci [DEPLOYMENT.md](DEPLOYMENT.md) dosyasında
(İngilizce) anlatılır; isteğe bağlı, ileri düzey bir kullanım olan çoklu
firma modu da oradadır.

Önce sunucu, DNS ve TLS olmadan kendi bilgisayarınızda bakmak isterseniz
[README](../README.tr.md#bilgisayarinizda-deneyin) içindeki hızlı
başlangıcı kullanın (`docker-compose.quickstart.yml`).

> **Durum.** HostingOS'un henüz etiketlenmiş bir sürümü yok; bu yüzden
> imzalı sürüm arşivleri ve sürüm numaralı imajlar da yok. Compose (A
> yolu) hazır `ghcr.io/sunucu-com-tr/hostingos:edge` imajını çalıştırır;
> bu imajı CI, bütün testler geçtikten sonra `main` dalından derler.
> `main` dalını izler ve imzalı bir sürüm değildir. İmaj, ilk imaj
> yayımlandıktan sonra kullanılabilir; o zamana kadar onu kaynak koddan
> derleyin (bölüm 5). B yolunda binary, ilk imzalı sürüme kadar kaynak
> koddan derlenir.

## İçindekiler

1. [Gereksinimler](#gereksinimler)
2. [Alan adları ve DNS](#alan-adlari)
3. [Gizli anahtarlar](#gizli-anahtarlar)
4. [Dış bağlantılar: `HOS_OUTBOUND_POLICY`](#dis-baglantilar)
5. [A. Caddy ile Docker Compose](#kurulum-compose)
6. [B. systemd altında binary](#kurulum-systemd)
7. [Reverse proxy ve istemci adresleri](#reverse-proxy)
8. [E-posta](#e-posta)
9. [İlk çalıştırma: firmanız ve sahip hesabı](#ilk-calistirma)
10. [Ortam değişkenleri](#ortam-degiskenleri)
11. [Yedekleme](#yedekleme)
12. [Sürüm yükseltme](#surum-yukseltme)
13. [Sorun giderme](#sorun-giderme)

<a id="gereksinimler"></a>

## 1. Gereksinimler

- Bir Linux sunucu. Sürüm binary'leri x86_64 ve aarch64 için (musl,
  statik) derlenecek; kaynak koddan derleme, Rust ve PostgreSQL'in
  çalıştığı her yerde olur.
- **PostgreSQL 16.** Compose dosyası kendi veritabanını getirir. Binary
  kurulumunda dağıtımınızın paketini ya da yönetilen bir veritabanını
  kullanın (PgBouncer ya da Neon'unki gibi bir bağlantı havuzu varsa
  havuzlu adresi `DATABASE_URL`'e, doğrudan adresi `DATABASE_URL_DIRECT`'e
  yazın).
- DNS'ini yönettiğiniz bir alan adı. Caddy'nin sertifika alabilmesi için
  80 ve 443 portlarına internetten erişilebilmeli.
- Gizli anahtarları üretmek için `openssl`.
- A için: Docker Engine ve Compose eklentisi. Compose yayımlanmış imajı
  indirir. İmajı bunun yerine kendiniz derlemek HostingOS'u derlemek
  demektir; biraz zaman alır ve Rust derleyicisine yetecek kadar bellek
  ister.
- B için: systemd, bir reverse proxy (örneklerde Caddy 2) ve ya
  `minisign` (imzalı bir sürümü `install.sh` ile kurmak için) ya da Rust ve
  bir C derleyicisi (kaynaktan derlemek için). Yerinde güncellemelerden
  önce yedek alınabilmesi için `PATH` üzerinde `pg_dump`.

<a id="alan-adlari"></a>

## 2. Alan adları ve DNS

`HOS_BASE_URL`, sitenizin herkese açık adresidir; örneğin
`https://www.example.com`. HostingOS bu adresi e-postalardaki ve
belgelerdeki bağlantılarda kullanır; `https` ile başlıyorsa çerezler
Secure olarak gönderilir.

Mağaza `/`, müşteri paneli `/client`, yönetim paneli `/admin`, REST API
`/api/v1` altındadır; izleme için `/healthz` (süreç ayakta mı) ve
`/readyz` (veritabanına erişiliyor ve migration'lar uygulanmış mı) yanıt verir. `Host` başlığı bozuk
olan bir istek (harf, rakam, `-` ve `.` dışında karakter içeren, isteğe
bağlı port dışında ek taşıyan ya da köşeli parantezli bir IPv6 adresi
olmayan) `400 Bad request` alır.

HostingOS tek bir firmayı çalıştırır (`HOS_MODE=single`, varsayılan):
sunucuya ulaşan her alan adı bu firmayı gösterir, "site bulunamadı"
sayfası yoktur. Sertifikalar ise ayrı bir konudur: Caddy bir sertifika
istemeden önce HostingOS'a sorar (`/internal/tls-allow`), HostingOS da
yalnızca `HOS_BASE_URL` adresindeki ad için onay verir. Böylece kimse
sunucuya rastgele adlar için sertifika aldıramaz. `HOS_BASE_DOMAIN`
gerekmez.

**DNS.** `HOS_BASE_URL` adresindeki adı bir A/AAAA (ya da CNAME) kaydıyla
sunucuya yönlendirin. Siteye başka bir addan da (örneğin `www` olmadan
`example.com`) ulaşılmasını istiyorsanız o adı DNS sağlayıcınızda ya da
Caddyfile'a ekleyeceğiniz bir site bloğuyla `HOS_BASE_URL` adresine
yönlendirin; örneğin
`example.com { redir https://www.example.com{uri} permanent }`.

<a id="gizli-anahtarlar"></a>

## 3. Gizli anahtarlar

| Anahtar | Üretmek için | Not |
|---|---|---|
| `HOS_SECRET_KEY` | `openssl rand -base64 32` | Çözüldüğünde tam 32 bayt olmalı (standart ya da URL-safe base64). Entegrasyon kimlik bilgilerini, iki adımlı doğrulama anahtarlarını ve saklanan diğer gizli bilgileri şifreler. **Veritabanından ayrı bir yerde yedekleyin.** Kaybolursa ya da değişirse bu bilgiler okunamaz |
| `POSTGRES_PASSWORD` (Compose) | `openssl rand -hex 32` | Hex, bağlantı adresinin içinde sorun çıkarmaz |
| `HOS_TLS_ASK_SECRET` | `openssl rand -hex 32` | Sertifika kontrolü için Caddy ile HostingOS arasında paylaşılan anahtar |

Doldurulmuş bir ortam dosyasını asla depoya eklemeyin. Boş değerler hiç
verilmemiş sayılır.

<a id="dis-baglantilar"></a>

## 4. Dış bağlantılar: `HOS_OUTBOUND_POLICY`

Personeliniz yönetim panelinde, sunucunun bağlanacağı adresleri girer:
sanallaştırma ve panel API adresleri, SMTP sunucuları, registrar ve RDAP
uç noktaları, yapay zekâ uç noktaları. `HOS_OUTBOUND_POLICY` bu adreslerin
nereyi gösterebileceğine karar verir ve açılışta bir kez okunur:

| Değer | Entegrasyonların ulaşabildiği yerler | Ne zaman seçilir |
|---|---|---|
| `private` (varsayılan) | genel adresler **ve** özel ağlar (10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7) | Proxmox, vCenter, hosting panelleri ya da e-posta sunucunuz kendi yerel ağınızdaysa |
| `public` | yalnızca genel adresler | HostingOS'un konuştuğu bütün sistemlere genel bir adresten ulaşılıyorsa; böylece entegrasyon ayarları hiçbir zaman iç ağınızı gösteremez |

İki değer de loopback, link-local ve bulut metadata adreslerini, nasıl
yazılmış olurlarsa olsunlar, her zaman reddeder; her DNS yanıtı bağlantı
kurulurken yeniden denetlenir. Giden webhook'lar, politika ne olursa olsun
yalnızca genel adreslere gidebilir. Başka bir değer verilirse `hostingos`
açılmaz. Ayrıntılar:
[DEPLOYMENT.md](DEPLOYMENT.md#outbound-connections-ssrf-guard).

<a id="kurulum-compose"></a>

## 5. A. Caddy ile Docker Compose

`docker-compose.yml` dört servis çalıştırır: `postgres` (PostgreSQL 16),
`migrate` (bekleyen migration'ları uygular ve çıkar), `app` (HostingOS:
web sunucusu, iş kuyruğu ve zamanlayıcı) ve `caddy` (80 ve 443 portlarında
TLS; ayarları `docs/deploy/Caddyfile`). Konteynerler salt okunur kök dosya
sistemiyle, hiçbir yetki (capability) olmadan ve `no-new-privileges` ile
çalışır; uygulama uid 10001 ile çalışır. `migrate` ve `app` yayımlanmış
imajı çalıştırır (aşağıda "İmaj ve etiketler"); dosya hiçbir zaman imaj
derlemez.

```sh
git clone https://github.com/sunucu-com-tr/hostingos.git hostingos
cd hostingos
cp docs/deploy/env.example .env.production
chmod 600 .env.production

sed -i \
  -e "s|^POSTGRES_PASSWORD=.*|POSTGRES_PASSWORD=$(openssl rand -hex 32)|" \
  -e "s|^HOS_SECRET_KEY=.*|HOS_SECRET_KEY=$(openssl rand -base64 32)|" \
  -e "s|^HOS_TLS_ASK_SECRET=.*|HOS_TLS_ASK_SECRET=$(openssl rand -hex 32)|" \
  .env.production
```

Ardından `.env.production` dosyasını düzenleyin:

| Değişken | Örnek | Anlamı |
|---|---|---|
| `HOS_BASE_URL` | `https://www.example.com` | sitenizin herkese açık adresi |
| `ACME_EMAIL` | `siz@example.com` | Let's Encrypt hesabının e-posta adresi |
| `HOS_OUTBOUND_POLICY` | `private` ya da `public` | bkz. bölüm 4 |
| `HOS_UPDATE_REPO` | `sunucu-com-tr/hostingos` | yeni sürümlerin aranacağı yer (isteğe bağlı; boşken denetim kapalıdır) |
| `HOS_MODE` | `single` | şablonda zaten var: tek hosting firması. Olduğu gibi bırakın; `HOS_BASE_DOMAIN` yorum satırı olarak kalır |

Her şeyi başlatın (ilk açılışta imajlar indirilir):

```sh
docker compose --env-file .env.production up -d
docker compose --env-file .env.production ps
docker compose --env-file .env.production logs -f app
```

`app` başlamadan önce `migrate` çalışır; Caddy de uygulama hazır olduğunu
bildirene kadar bekler (konteynerin sağlık kontrolü
`hostingos healthcheck --path /readyz` komutunu çalıştırır). Caddy bir ad için sertifikayı o ada
ilk TLS bağlantısı geldiğinde ister; önce HostingOS'a
(`/internal/tls-allow`) adın bu kuruluma ait olup olmadığını sorar. Böylece
kimse sunucuya rastgele adlar için sertifika aldıramaz.

[Bölüm 9](#ilk-calistirma) ile devam edin. Bu kurulumda her `hostingos …`
komutu şöyle çalıştırılır:

```sh
docker compose --env-file .env.production run --rm app <komut> [seçenekler]
```

**İmaj ve etiketler.** Hangi imajın çalışacağını ortam dosyasındaki
`HOS_IMAGE` ve `HOS_TAG` belirler; varsayılan değerlerle imaj
`ghcr.io/sunucu-com-tr/hostingos:edge` olur.

| Etiket | Ne olduğu |
|---|---|
| `edge` | `main` dalının en yeni derlemesi. CI, `main`'e yapılan her push'ta, bütün testler ve uçtan uca test geçtikten sonra derleyip yayımlar. `main` ile birlikte ilerler ve **imzalı bir sürüm değildir** |
| `sha-<commit>` | Aynı derleme, commit'inin ilk yedi karakteriyle (örneğin `sha-0123abc`); hiç değişmez. O derlemede kalmak için `HOS_TAG`'e bunu yazın |
| `X.Y.Z`, `X.Y`, `latest` | İmzalı ve numaralı sürümler. **Henüz hiçbiri yayımlanmadı** |

Compose imajı yalnızca sunucuda henüz yoksa indirir; daha yeni bir `edge`
`docker compose --env-file .env.production pull` ile gelir
([Sürüm yükseltme](#surum-yukseltme)). İndirme başarısız olursa yerine
hiçbir şey derlenmez; açılış, imaj deposunun (registry) verdiği hatayla
durur.

**Kaynak koddan derleme.** Kodu değiştirdiyseniz ya da yayımlanmış imajı
kullanmak istemiyorsanız `docker-compose.build.yml` dosyasını ekleyin; bu
dosya imajı kendi kopyanızdan `hostingos:local` adıyla derler ve onu
hiçbir yerden indirmeye çalışmaz. Dosyanın o kurulumdaki her komuta
eklenmesi gerekir; en kolayı bunu kabuk için bir kez ayarlamaktır:

```sh
export COMPOSE_FILE=docker-compose.yml:docker-compose.build.yml
# isteğe bağlı: sunucu sayfasında görünen commit ve derleme tarihi
export HOS_BUILD_COMMIT="$(git rev-parse HEAD)" SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)"
docker compose --env-file .env.production up -d --build
```

**Önde başka bir proxy ya da CDN.** Caddy tek proxy olarak ayarlıdır
(`HOS_TRUST_PROXY=true`, bir atlama; istemci adresini yalnızca Caddy'nin
belirleyebilmesi için `HOS_TRUSTED_PROXIES`'i Compose ağının aralığına
ayarlayın, bölüm 7). Caddy'nin önünde bir CDN varsa
[bölüm 7](#reverse-proxy)'ye bakın. Caddy yerine var olan bir reverse
proxy'yi kullanacaksanız `docker-compose.yml` dosyasını uyarlayın: proxy
özgün `Host` başlığını iletmeli, `X-Forwarded-For` başlığını eklemeli ve
alan adlarınız için sertifika sağlamalıdır (bunun için
`X-HostingOS-Secret: <HOS_TLS_ASK_SECRET>` başlığıyla
`/internal/tls-allow?domain=<ad>` adresine sorabilir).

<a id="kurulum-systemd"></a>

## 6. B. systemd altında binary

Yerinde güncelleme için şu düzen gerekir:

```text
/opt/hostingos/
  releases/<sürüm>/{hostingos, static/, locales/}
  current  -> releases/<sürüm>          (systemd current/hostingos'u çalıştırır)
  previous -> releases/<önceki sürüm>   (bir güncellemeden sonra)
  state/                                (güncelleme işareti, kilit, yedekler)
/etc/hostingos/hostingos.env            (yapılandırma, 0640 root:hostingos)
```

### 6.1 PostgreSQL

Bir rol ve bir veritabanı oluşturun; örneğin yerel PostgreSQL 16'da:

```sh
sudo -u postgres createuser --pwprompt hostingos
sudo -u postgres createdb --owner hostingos hostingos
```

Bağlantı adresi bu durumda
`postgres://hostingos:PAROLA@127.0.0.1:5432/hostingos` olur.

### 6.2 Binary

**İmzalı bir sürümden** (sürümler yayımlandıktan sonra).
[`docs/deploy/install.sh`](deploy/install.sh) mimarinize uygun arşivi
indirir, minisign imzasını verdiğiniz açık anahtarla doğrular, arşivin
içeriğini denetler, `hostingos` sistem kullanıcısını ve yukarıdaki düzeni
oluşturur, systemd birimini kurar ve bir ortam dosyası şablonu yazar.
Servisi başlatmaz, migration çalıştırmaz. Betiği indirin, okuyun, sonra
çalıştırın:

```sh
sudo ./install.sh --version X.Y.Z --repo sunucu-com-tr/hostingos --pubkey hostingos-release.pub
```

`hostingos-release.pub` projenin minisign açık anahtarıdır.
`--archive DOSYA` ile (imza `DOSYA.minisig` olarak yanında) indirmeden,
yerel dosyalardan kurar.

**Kaynak koddan.** Rust ve bir C derleyicisi kurulu bir makinede derleyin,
sonra aynı düzeni elle oluşturun (`0.1.0` yerine
`./target/release/hostingos version` çıktısındaki sürümü yazın):

```sh
cargo build --release --locked -p hos-web --bin hostingos

V=0.1.0
sudo useradd --system --home-dir /opt/hostingos --no-create-home \
  --shell /usr/sbin/nologin hostingos
sudo install -d -m 0755 -o hostingos -g hostingos /opt/hostingos /opt/hostingos/releases
sudo install -d -m 0700 -o hostingos -g hostingos /opt/hostingos/state
sudo install -d -m 0755 /opt/hostingos/releases/$V
sudo install -m 0755 target/release/hostingos /opt/hostingos/releases/$V/hostingos
sudo cp -R static locales /opt/hostingos/releases/$V/
sudo chown -R hostingos:hostingos /opt/hostingos/releases/$V
sudo ln -sfn releases/$V /opt/hostingos/current
sudo chown -h hostingos:hostingos /opt/hostingos/current
```

### 6.3 Yapılandırma

```sh
sudo install -d -m 0750 -o root -g hostingos /etc/hostingos
sudo install -m 0640 -o root -g hostingos /dev/null /etc/hostingos/hostingos.env
sudoedit /etc/hostingos/hostingos.env
```

(`install.sh` ikisini de oluşturur; var olan bir dosyaya dokunmaz.) En
küçük haliyle dosya:

```sh
DATABASE_URL=postgres://hostingos:PAROLA@127.0.0.1:5432/hostingos
DATABASE_URL_DIRECT=postgres://hostingos:PAROLA@127.0.0.1:5432/hostingos
HOS_SECRET_KEY=            # openssl rand -base64 32
HOS_BIND=127.0.0.1:8080
HOS_BASE_URL=https://www.example.com
HOS_TRUST_PROXY=true
HOS_TRUSTED_PROXIES=127.0.0.1,::1   # bu makinedeki reverse proxy
HOS_TLS_ASK_SECRET=        # openssl rand -hex 32
HOS_OUTBOUND_POLICY=private
HOS_UPDATE_REPO=
# HOS_MODE=single          # varsayılan: tek hosting firması
```

`HOS_BIND=127.0.0.1:8080`, uygulamaya yalnızca aynı makinedeki reverse
proxy üzerinden erişilmesini sağlar. `HOS_UPDATE_ROOT` ve
`HOS_LOG_FORMAT=json` değerlerini systemd birimi kendisi verir.

### 6.4 Migration, sahip hesabı, servis

```sh
sudo -u hostingos sh -c 'set -a; . /etc/hostingos/hostingos.env; exec /opt/hostingos/current/hostingos migrate'
sudo -u hostingos sh -c 'set -a; . /etc/hostingos/hostingos.env; exec /opt/hostingos/current/hostingos setup --email siz@example.com --company "Örnek Hosting"'

sudo install -m 0644 docs/deploy/hostingos.service /etc/systemd/system/hostingos.service   # install.sh bunu zaten yaptı
sudo systemctl daemon-reload
sudo systemctl enable --now hostingos
journalctl -u hostingos -f
```

Bu kurulumda diğer `hostingos` komutları da yukarıdaki `migrate` gibi
çalıştırılır. [`docs/deploy/hostingos.service`](deploy/hostingos.service),
`current/hostingos serve` komutunu `hostingos` kullanıcısıyla,
sıkılaştırılmış bir kum havuzunda (sistem salt okunur, yalnızca
`/opt/hostingos` altına yazılabilir) çalıştırır. Süreç **75** koduyla
çıktığında servisi yeniden başlatır; bu kod, bir güncellemeden ya da geri
almadan sonra "beni etkin sürümle yeniden başlat" demektir. Kendi
biriminizi yazarsanız `RestartForceExitStatus=75` satırını koruyun.
`SIGTERM` gelince süreç, çalışan istekleri ve işleri bitirmelerine izin
verir (en çok 45 saniye).

### 6.5 Aynı makinede Caddy

Caddy 2'yi dağıtımınızdan ya da Caddy projesinden kurup şuna benzer bir
Caddyfile kullanın (depodaki `docs/deploy/Caddyfile`, uygulamaya `app:8080`
olarak ulaşan Compose sürümüdür):

```caddyfile
{
	email siz@example.com
	on_demand_tls {
		ask http://127.0.0.1:8080/internal/tls-allow?secret=HOS_TLS_ASK_SECRET_DEGERINIZ
	}
}

https:// {
	tls {
		on_demand
	}
	encode zstd gzip
	reverse_proxy 127.0.0.1:8080 {
		header_up X-Forwarded-Proto {scheme}
	}
}

http:// {
	redir https://{host}{uri} permanent
}
```

<a id="reverse-proxy"></a>

## 7. Reverse proxy ve istemci adresleri

İstemci adresi hız sınırlarında, denetim kaydında, oturumlarda ve
siparişlerde kullanılır. Bir reverse proxy'nin arkasında uygulama, iletilen
başlıklara güvenmesi söylenmedikçe yalnızca proxy'nin adresini görür:

| Değişken | Varsayılan | Anlamı |
|---|---|---|
| `HOS_TRUST_PROXY` | kapalı | `true` (ya da `1`, `yes`, `on`): istemci adresini iletilen başlıklardan oku. `HOS_BASE_URL` `https` ise her istek https sayılır; `http` ise `X-Forwarded-Proto` dikkate alınır |
| `HOS_TRUSTED_PROXY_HOPS` | `1` | Uygulamanın önünde `X-Forwarded-For`'a ekleme yapan proxy sayısı; istemci adresi sağdan bu kadar sıradaki kayıttır. `0` başlığı yok sayar |
| `HOS_CLIENT_IP_HEADER` | boş | `HOS_TRUST_PROXY` açıkken, en dıştaki proxy'nin ziyaretçi adresini yazdığı başlık; örneğin Cloudflare arkasında `CF-Connecting-IP`. Tam olarak bir geçerli adres içeriyorsa `X-Forwarded-For`'dan önce kullanılır |
| `HOS_TRUSTED_PROXIES` | boş | Uygulamaya bağlanan proxy'lerin virgülle ayrılmış adresleri ya da CIDR aralıkları (IPv4 ve IPv6). `HOS_TRUST_PROXY` açıkken yukarıdaki başlıklar ve `X-Forwarded-Proto` yalnızca bu adreslerden gelen bağlantılarda dikkate alınır; diğer herkes kendi adresiyle görünür. Boşsa eskisi gibi her bağlantıya güvenilir ve açılışta uyarı loglanır. Adres ya da aralık olmayan bir değer verilirse `hostingos` açılmaz |

Sık kullanılan ayarlar:

- Yalnızca Caddy (ya da nginx), aynı makinede: `HOS_TRUST_PROXY=true`,
  `HOS_TRUSTED_PROXY_HOPS=1`, `HOS_TRUSTED_PROXIES=127.0.0.1,::1`.
- Compose ağında Caddy: aynısı, `HOS_TRUSTED_PROXIES` ağın aralığına
  ayarlı olarak (`docker network inspect <ağ> --format '{{range
  .IPAM.Config}}{{.Subnet}} {{end}}'`; bu deponun `docker-compose.yml`
  dosyasında ağın adı `hostingos_default`).
- Caddy'nin önünde bir CDN (CDN → Caddy → HostingOS): Caddy (Traefik gibi),
  gönderen kendi `trusted_proxies` listesinde değilse gelen
  `X-Forwarded-For`'u yok sayar. Ya CDN'in adres aralıklarını bu listeye
  ekleyip `HOS_TRUSTED_PROXY_HOPS=2` yapın ya da uygulamanın CDN'in kendi
  başlığını okumasını sağlayın; Cloudflare için
  `HOS_CLIENT_IP_HEADER=CF-Connecting-IP`.

`HOS_TRUST_PROXY`'yi yalnızca istemciler uygulamaya doğrudan
ulaşamıyorsa açın (uygulamayı `127.0.0.1`'e bağlayın ya da Compose ağının
içinde tutun) ya da proxy'leri `HOS_TRUSTED_PROXIES`'te sayın; yoksa porta
ulaşan herkes istediği adresi taşıyormuş gibi davranabilir.
`HOS_CLIENT_IP_HEADER`'ı yalnızca sunucu bağlantıyı sadece o CDN'den
kabul ediyorsa (yalnızca CDN'in adres aralıklarına izin veren bir güvenlik
duvarıyla) kullanın; yoksa ziyaretçiler bu başlığı taklit edebilir
(`HOS_TRUSTED_PROXIES` burada işe yaramaz: bu istekler sizin proxy'niz
üzerinden gelir). Proxy arkasında `HOS_TRUST_PROXY` kapalıysa bütün
ziyaretçiler proxy'nin adresini paylaşır, dolayısıyla tek bir hız sınırına
takılır.

Uygulama yavaş istemcileri de sınırlar: istek başlıkları için 120 saniye,
gövde için 60 saniye (`408`) ve tüm istek için 120 saniye (`503`);
`HOS_HTTP_HEADER_TIMEOUT_SECS`, `HOS_HTTP_BODY_TIMEOUT_SECS` ve
`HOS_HTTP_REQUEST_TIMEOUT_SECS` bunları değiştirir, `0` birini kapatır
([DEPLOYMENT.md](DEPLOYMENT.md#http-timeouts)).

<a id="e-posta"></a>

## 8. E-posta

Sipariş ve fatura e-postaları, parola sıfırlama ve destek yanıtları,
yönetim panelinde Entegrasyonlar altında kurduğunuz e-posta entegrasyonuyla
gönderilir (E-posta kategorisi: SMTP, Resend, Postmark ya da Mailgun).
Gönderen adres ile Türkçe ve İngilizce şablonlar Ayarlar → E-posta
sayfasındadır; oradan test e-postası da gönderilebilir. E-postalar arka
plan işi olarak kuyruğa girer ve hata olursa yeniden denenir. Bir e-posta
entegrasyonu kurulana kadar e-postalar yalnızca loga yazılır (gönderen,
alıcılar, konu ve boyut; içerik yazılmaz).

`HOS_DEBUG_MAIL=1`, e-posta içeriklerini (parola sıfırlama bağlantıları
dahil) loga yazar. Geliştirme içindir; canlı kurulumda asla açmayın.

Destek talepleri için gelen e-posta (Mailgun inbound route ya da ham ileti
uç noktası) [API.md](API.md#incoming-e-mail-ticket-piping) dosyasında
anlatılır.

<a id="ilk-calistirma"></a>

## 9. İlk çalıştırma: firmanız ve sahip hesabı

1. **Migration'lar.** Compose bunları `migrate` servisiyle çalıştırır; B
   yolunda `hostingos migrate` komutunu çalıştırın (bölüm 6.4); `setup`
   da bunları uygular. Yalnızca ileri doğru ilerler, tekrar çalıştırmak
   güvenlidir.
2. **Firma ve sahip hesabı.** `hostingos setup` komutunu bir kez
   çalıştırın (Compose:
   `docker compose --env-file .env.production run --rm app setup …`; B:
   bölüm 6.4'teki gibi):

   ```sh
   hostingos setup --email siz@example.com --company "Örnek Hosting" --locale tr --currency TRY
   ```

   Komut bekleyen migration'ları uygular, firmayı ve sahip hesabını
   oluşturur, giriş adresini yazar. Parola `--password` seçeneğinden ya da
   `HOS_NEW_PASSWORD` değişkeninden alınır; verilmezse rastgele bir parola
   üretilip bir kez ekrana yazılır. Varsayılanlar `--company "My Hosting
   Company"`, `--locale tr` ve `--currency TRY`'dir. `setup` yeniden
   çalıştırılırsa yalnızca firmanın zaten var olduğunu söyler. `setup`'tan
   önce başlatılan bir sunucu çalışmaya devam eder ve her sayfaya `503`
   yanıtı verir; firma, `setup`'tan birkaç saniye sonra yeniden başlatmaya
   gerek kalmadan görünür.
3. **Giriş:** `https://www.example.com/auth/login`; yönetim paneli
   `/admin` altındadır. Kendi hesabınız için iki adımlı doğrulamayı açın;
   Ayarlar → Güvenlik altında bunu bütün personel için zorunlu
   kılabilirsiniz.
4. **Yapılandırma:** Entegrasyonlar altında ödeme altyapılarını,
   sunucuları, registrar'ları ve e-postayı, ardından katalogda ürünleri ve
   fiyatları tanımlayın. Başka personel hesaplarını Personel altında, her
   birine bir rol vererek ekleyin.
5. **Sunucu sayfaları.** Yönetim menüsündeki Sunucu bölümünü yalnızca
   firma sahibi görür: `/admin/server` sürümü, çalışma süresini,
   veritabanını, migration'ları ve sağlık durumunu gösterir;
   `/admin/server/updates` güncelleme denetimini, güncelleme kurmayı, bakım
   modunu ve deploy webhook'unu içerir. Buradaki her işlem denetim kaydına
   sahibin adıyla yazılır.

<a id="ortam-degiskenleri"></a>

## Ortam değişkenleri

Zorunlu olanlar işaretlidir. Boş değerler hiç verilmemiş sayılır. Tam
liste [DEPLOYMENT.md](DEPLOYMENT.md#environment) dosyasındadır.

**Veritabanı ve anahtarlar**

| Değişken | Varsayılan | Anlamı |
|---|---|---|
| `DATABASE_URL` | – (**zorunlu**) | PostgreSQL bağlantısı (`postgres://` ya da `postgresql://`); havuzlu bağlantı olabilir. `channel_binding=…` kendiliğinden çıkarılır |
| `DATABASE_URL_DIRECT` | `DATABASE_URL` | Migration'lar ve worker'ın `LISTEN/NOTIFY` kullanımı için doğrudan bağlantı; `DATABASE_URL` bir havuzdan geçiyorsa verin |
| `HOS_SECRET_KEY` | – (**zorunlu**) | 32 rastgele bayt, base64 (bölüm 3) |

**Site ve HTTP**

| Değişken | Varsayılan | Anlamı |
|---|---|---|
| `HOS_BASE_URL` | `http://localhost:<port>` | Sitenizin herkese açık adresi (bölüm 2); `https` Secure çerezleri açar |
| `HOS_MODE` | `single` | Tek hosting firması; sunucuya ulaşan her alan adı bu firmayı gösterir. Bilinmeyen bir değer verilirse `hostingos` açılmaz |
| `HOS_BIND` | `127.0.0.1:8080` | Dinlenen adres; imaj `0.0.0.0:8080` kullanır |
| `HOS_TRUST_PROXY`, `HOS_TRUSTED_PROXY_HOPS`, `HOS_CLIENT_IP_HEADER`, `HOS_TRUSTED_PROXIES` | kapalı, `1`, –, – | Bölüm 7 |
| `HOS_TLS_ASK_SECRET` | – | Proxy aynı makinede (loopback) değilse `/internal/tls-allow` için anahtar |
| `HOS_MAX_BODY_BYTES` | `2097152` | İstek gövdesi sınırı (bayt) |
| `HOS_HTTP_HEADER_TIMEOUT_SECS`, `HOS_HTTP_BODY_TIMEOUT_SECS`, `HOS_HTTP_REQUEST_TIMEOUT_SECS` | `120`, `60`, `120` | İstek başlıkları, gövde (`408`) ve tüm istek (`503`) için saniye; `0` = sınır yok (bölüm 7) |
| `HOS_MIGRATE_ON_START` | kapalı | `1`: `hostingos serve` portunu açmadan önce bekleyen migration'ları uygular, `serve --migrate` gibi ([DEPLOYMENT.md](DEPLOYMENT.md#migrations)) |
| `HOS_STATIC_DIR`, `HOS_LOCALES_DIR` | binary'nin yanı, yoksa `./static`, `./locales` | Dosya ve çeviri dizinleri |

**Entegrasyonlar ve arka plan işleri**

| Değişken | Varsayılan | Anlamı |
|---|---|---|
| `HOS_OUTBOUND_POLICY` | `private` | `private` ya da `public` (bölüm 4) |
| `HOS_WORKER_CONCURRENCY` | `8` | Worker süreci başına paralel iş sayısı (1–256) |
| `HOS_JOB_QUEUE_NAMESPACE` | – | Yalnızca aynı veritabanını paylaşan ikinci bir kurulum (test ortamı) için (bkz. DEPLOYMENT.md) |
| `ANTHROPIC_API_KEY`, `HOS_AI_MODEL` | –, yerleşik varsayılan | İçerik ve SEO için isteğe bağlı yapay zekâ taslakları; anahtar yoksa kapalı |
| `GOOGLE_CLIENT_ID` + `GOOGLE_CLIENT_SECRET`, `GITHUB_CLIENT_ID` + `GITHUB_CLIENT_SECRET` | – | Google ya da GitHub ile giriş (her çiftin ikisini birden verin ya da hiçbirini) |

**Güncellemeler** ([UPDATES.md](UPDATES.md))

| Değişken | Varsayılan | Anlamı |
|---|---|---|
| `HOS_UPDATE_REPO` | – | İmzalı sürüm bildirimini taşıyan GitHub deposu (`sahip/ad`); bu ya da bir sonraki verilmedikçe güncelleme denetimi kapalıdır |
| `HOS_UPDATE_MANIFEST_URL` | – | GitHub deposu yerine bir bildirim dosyasının HTTPS adresi |
| `HOS_UPDATE_ALLOWED_HOSTS` | GitHub adresleri | İndirme için ek adresler |
| `HOS_UPDATE_HEALTH_TIMEOUT` | `300` | Yeni bir sürümün sağlıklı olduğunu bildirmesi için saniye; süre dolarsa geri alınır |
| `HOS_UPDATE_BACKUP_COMMAND` | `PATH`'te varsa `pg_dump` | Yerinde güncellemeden önce çalışan komut |
| `HOS_INSTALL_TYPE`, `HOS_UPDATE_ROOT`, `HOS_SUPERVISED` | otomatik algılanır | Algılamayı geçersiz kılmak için; bkz. UPDATES.md |

**Loglar**

| Değişken | Varsayılan | Anlamı |
|---|---|---|
| `HOS_LOG_FORMAT` | metin (imajda ve systemd biriminde `json`) | Yapılandırılmış log için `json` |
| `RUST_LOG` | `info` | Log süzgeci, örneğin `info,sqlx=warn` |

**Yalnızca geliştirme, demo ve komut satırı.** İlk üçünü canlı kurulumda
asla açmayın.

| Değişken | Anlamı |
|---|---|
| `HOS_DEBUG` | Geliştirme modu (ayrıntılı hata sayfaları, mock entegrasyonlara izin). Varsayılan olarak yalnızca debug derlemelerinde açıktır |
| `HOS_DEBUG_MAIL` | E-posta içeriklerini loga yazar |
| `HOS_ALLOW_MOCK_INTEGRATIONS` | `mock` ödeme altyapısına, sanallaştırma, panel ve registrar sağlayıcılarına izin verir (mock ödeme altyapısıyla herkes her faturayı ödenmiş gösterebilir) |
| `HOS_DEMO_MODE`, `HOS_DEMO_RESET_MINUTES`, `HOS_DEMO_ALLOW_EXISTING_DATA` | Herkese açık demo modu ([DEMO.md](DEMO.md)) |
| `HOS_NEW_PASSWORD` | Komut satırından oluşturulan bir hesabın parolası; verilmezse rastgele bir parola üretilip ekrana yazılır |
| `HOS_IMPORT_DB_PASSWORD`, `HOS_IMPORT_CC_HASH`, `HOS_IMPORT_API_SECRET`, `HOS_IMPORT_API_ACCESS_KEY` | `hostingos import whmcs` için gizli bilgiler ([IMPORT-WHMCS.md](IMPORT-WHMCS.md)) |

**Yalnızca Compose dosyası:** `POSTGRES_PASSWORD`, `ACME_EMAIL`,
`HOS_IMAGE` (varsayılan `ghcr.io/sunucu-com-tr/hostingos`), `HOS_TAG`
(varsayılan `edge`; bölüm 5).

<a id="yedekleme"></a>

## Yedekleme

- **Veritabanı.** Her gün yedek alın, birkaç kuşağı başka bir makinede
  saklayın ve ara ara geri yüklemeyi deneyin.

  ```sh
  # Compose
  docker compose --env-file .env.production exec -T postgres \
    pg_dump -U hostingos -Fc hostingos > hostingos-$(date +%F).dump

  # Binary kurulumu (pg_dump'ı olan ve veritabanına erişebilen her makine)
  pg_dump --format=custom --no-owner "$DATABASE_URL_DIRECT" > hostingos-$(date +%F).dump
  ```

- **`HOS_SECRET_KEY`.** Yedeklerden ayrı bir yerde, örneğin bir parola
  yöneticisinde saklayın. Anahtar olmadan geri yüklenen bir yedekte
  entegrasyon kimlik bilgileri ve iki adımlı doğrulama anahtarları
  kullanılamaz.
- **Başka kalıcı veri yok.** Destek eklerinin ve yüklenen dosyaların hepsi
  veritabanındadır; Caddy'nin sertifikaları kaybolursa yeniden alınır.
- Binary kurulumunda yerinde güncelleme önce kendi yedeğini alır
  (`HOS_UPDATE_BACKUP_COMMAND`, yoksa `/opt/hostingos/state/backups/`
  altına `pg_dump`).

Geri yüklemek için uygulamayı durdurun, yedeği boş bir veritabanına
`pg_restore --no-owner --dbname "<bağlantı adresi>" <dosya>` ile yükleyin,
HostingOS'u aynı `HOS_SECRET_KEY` ile başlatın ve `/readyz` adresini
kontrol edin.

<a id="surum-yukseltme"></a>

## Sürüm yükseltme

Önce [CHANGELOG.md](../CHANGELOG.md) dosyasını okuyun ve yedek alın.
Migration'lar yalnızca ileri gider ve yeni sürüm istek almadan önce
uygulanır.

- **Yayımlanmış imajla Compose** (varsayılan): bir `sha-…` derlemesine
  sabitlediyseniz `HOS_TAG`'i değiştirin, sonra

  ```sh
  docker compose --env-file .env.production pull
  docker compose --env-file .env.production up -d
  ```

  `up`, imajı değişen konteynerleri yeniden oluşturur: önce tek seferlik
  `migrate` servisi yeni sürümün migration'larını uygular, `app` ancak o
  başarıyla bittikten sonra başlar. Bir migration başarısız olursa yeni
  `app` başlatılmaz; nedenini `docker compose … logs migrate` gösterir.
  Konteyner kendini hiçbir zaman değiştirmez. İmzalı sürümler (çıktıkları
  zaman) güncelleme sayfasında da duyurulacak ve oradan bir deploy
  webhook'u tetiklenebilecek; `edge` derlemeleri orada duyurulmaz.
- **Kaynaktan derlenen Compose:** `git pull`, ardından bölüm 5'teki gibi
  `COMPOSE_FILE` ayarlıyken
  `docker compose --env-file .env.production up -d --build`.
- **Binary:** `HOS_UPDATE_REPO`'yu ayarlayın, sonra yeni sürümü firma
  sahibi olarak yönetim panelinde Sunucu → Güncellemeler sayfasından
  (`/admin/server/updates`) kurun. Sayfa parolanızı (iki adımlı doğrulama
  açıksa kodu da) yeniden sorar; yedek, bakım modu, migration'lar, sağlık
  kontrolü ve gerekirse otomatik geri alma kendiliğinden yapılır. Ya da `hostingos update check` ve
  `hostingos update apply <sürüm>` komutlarını kullanın.
  `hostingos update rollback`, `current` bağlantısını önceki sürüme geri
  çevirir; veritabanına dokunmaz. `install.sh --version … --activate` elle
  geçiş yapar.

Yerinde kurulum, güvenilen bir anahtarla imzalanmamış sürümleri reddeder.
Henüz bir imza anahtarı oluşturulmadı; bu yol ilk imzalı sürümle birlikte
çalışmaya başlayacak. Ayrıntılar: [UPDATES.md](UPDATES.md).

<a id="sorun-giderme"></a>

## Sorun giderme

| Belirti | Neden ve çözüm |
|---|---|
| `missing required environment variable …` / `invalid environment variable HOS_SECRET_KEY` | Değişken boş ya da anahtar çözüldüğünde 32 bayt etmiyor; `openssl rand -base64 32` ile üretin |
| `ghcr.io/sunucu-com-tr/hostingos` indirilirken `denied`, `unauthorized` ya da `not found` hatası | İmaj henüz yayımlanmadı, henüz herkese açık değil ya da `HOS_TAG` var olmayan bir etiketi gösteriyor. Etiketi kontrol edin ya da imajı kaynak koddan derleyin (bölüm 5) |
| `invalid HOS_OUTBOUND_POLICY value` | Yalnızca `private` ve `public` kabul edilir |
| Her sayfa `503` veriyor, logda "no hosting company has been set up" yazıyor | `hostingos setup` komutunu çalıştırın (bölüm 9); yeniden başlatmak gerekmez |
| Sunucu açılmıyor: "this database holds … hosting companies" | Veritabanında birden fazla firma var (örneğin bir denemeden kalma); `DATABASE_URL`'i firmanızın veritabanına yönlendirin |
| `Bad request – Invalid Host header` | Proxy alışılmadık bir `Host` iletiyor (örneğin `_` içeren); özgün adı iletin |
| Oturum açılmıyor ya da hemen kapanıyor | `HOS_BASE_URL` `https` ise çerezler Secure'dur ve yalnızca HTTPS üzerinden gönderilir; siteyi proxy'nin https adresinden açın |
| Bütün ziyaretçiler aynı IP adresiyle görünüyor ya da hız sınırı herkesi birden etkiliyor | Proxy arkasında `HOS_TRUST_PROXY` kapalı ya da proxy ayarları önünüzdeki proxy'lerle uyuşmuyor (bölüm 7) |
| Bir ad için sertifika alınamıyor | DNS sunucuyu göstermeli, 80/443 portları açık olmalı. Caddy yalnızca `/internal/tls-allow`'un onayladığı adlar için sertifika alır (bölüm 2). Caddy logunda `403` görüyorsanız `HOS_TLS_ASK_SECRET` iki tarafta farklıdır. Ayrıntı için `docker compose … logs caddy` |
| Bir entegrasyonun "Bağlantıyı test et" düğmesi yapılandırma hatası veriyor | Adres dış bağlantı politikasına takılıyor (loopback, link-local, metadata ya da `public` altında özel ağ adresi); bkz. bölüm 4 |
| E-posta gelmiyor | Henüz e-posta entegrasyonu kurulmamış, e-postalar yalnızca loga yazılıyor (bölüm 8) |
| `mock` sağlayıcılar kaydedilemiyor | Canlı kurulumda reddedilirler; yalnızca geliştirme ve demo içindir (`HOS_ALLOW_MOCK_INTEGRATIONS`, debug ya da demo modu) |
| `/readyz` hata veriyor | `"database":"error"`: veritabanına `DATABASE_URL` ile ulaşılamıyor; `"schema":"behind"`: bu sürümün migration'ları uygulanmamış, `hostingos migrate` çalıştırın (yeniden başlatma gerekmez) |
| Güncelleme: "Update checks are off", "no supervisor detected", bakım modunda kalma | [UPDATES.md](UPDATES.md#7-troubleshooting) içindeki sorun giderme tablosuna bakın |

Loglar: `docker compose --env-file .env.production logs -f app` ya da
`journalctl -u hostingos -f`. Ölçekleme ve ayrı worker hakkında daha
fazlası: [DEPLOYMENT.md](DEPLOYMENT.md).
