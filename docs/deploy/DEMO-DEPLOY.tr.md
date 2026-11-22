# Herkese açık demonun kurulumu

[English](DEMO-DEPLOY.md)

Tek bir alan adında herkese açık bir HostingOS demosu çalıştırma (demo
modu: [../DEMO.md](../DEMO.md)). İki biçim:

* **A — Coolify, Dockerfile ile derleme, ayrı PostgreSQL** (önerilen;
  herkese açık demonun kullandığı biçim).
* **B — `docker-compose.demo.yml`**: uygulama ve kendi PostgreSQL'i tek
  yığında; Coolify'da ya da herhangi bir Docker sunucusunda.

Metin boyunca `demo.example.invalid` sizin demo alan adınızın yerine geçer.

## Ne elde edersiniz

`https://demo.example.invalid` üzerinde (tek alan adı, joker DNS yok):

| Yol | İçerik |
|---|---|
| `/` | Demo hosting firması "Demo Hosting A.Ş."nin vitrini (Türkçe; İngilizcesi `/en` altında), demo bandıyla |
| `/auth/login` | Tek tıkla demo hesaplarıyla giriş: `admin@demo.test` / `demo-admin` (yönetim paneli), `musteri@demo.test` / `demo-client` (müşteri paneli) |
| `/admin`, `/client` | Yönetim paneli ve müşteri paneli |
| `/admin/server` | Firma sahibinin sunucu sayfaları (sürüm, sağlık, güncellemeler); demoda salt okunur |
| `/demo/mailbox` | Demonun "gönderdiği" e-postalar (sunucudan hiçbir şey çıkmaz) |

Ziyaretçiler vitrinde görünenler dahil içerik ekleyip değiştirebilir; her
şey bir sonraki sıfırlamada silinir (varsayılan: 30 dakikada bir). Her
sayfadaki bant ve giriş sayfaları bunu Türkçe ve İngilizce olarak söyler.

## Ortam değişkenleri

| Ad | Değer | Not |
|---|---|---|
| `HOS_DEMO_MODE` | `1` | Zorunlu: demo modunu açar |
| `HOS_DEMO_RESET_MINUTES` | `30` | İsteğe bağlı (5–1440, varsayılan 30) |
| `HOS_MODE` | boş | Boş bırakın (ya da varsayılan `single`): demo tek firmalık bir kurulumdur; `multi` reddedilir |
| `DATABASE_URL` | `postgres://…` | Zorunlu. Demonun **kendine ait, boş** veritabanı (aşağıya bakın) |
| `DATABASE_URL_DIRECT` | `postgres://…` | İsteğe bağlı; yalnızca `DATABASE_URL` PgBouncer üzerinden gidiyorsa |
| `HOS_SECRET_KEY` | `openssl rand -base64 32` | Zorunlu. Sonradan değiştirmek demo için zararsızdır |
| `HOS_BASE_URL` | `https://demo.example.invalid` | Zorunlu. Sonunda `/` yok. Her şeyi bu alan adı sunar |
| `HOS_TRUST_PROXY` | `true` | Önde bir vekil (Coolify'ın Traefik'i) var |
| `HOS_CLIENT_IP_HEADER` | `CF-Connecting-IP` | Cloudflare arkasında; CDN yoksa boş bırakın ([Vekiller](#vekiller-ve-ziyaretçi-ip-adresi)) |
| `HOS_TRUSTED_PROXY_HOPS` | `1` | Yukarıdaki başlık yoksa kullanılan yedek |
| `HOS_TRUSTED_PROXIES` | Docker ağının aralığı, ör. `10.0.1.0/24` | Önerilir: yönlendirme başlıkları yalnızca bu adreslerden (Traefik) gelince dikkate alınır. Aralığı [Vekiller](#vekiller-ve-ziyaretçi-ip-adresi) bölümündeki gibi bulun; boşsa her bağlantıya güvenilir ve logda uyarı çıkar |
| `HOS_MIGRATE_ON_START` | boş | Gerekmez: demo modu bekleyen migration'ları bağlantı kabul etmeden önce her zaman uygular |
| `HOS_OUTBOUND_POLICY` | `public` | Önerilir (demo modu zaten dışarıya bağlantı kurmaz) |
| `HOS_BIND` | `0.0.0.0:8080` | İmajın varsayılanı zaten bu |
| `HOS_DEBUG` | `0` | Sürüm derlemelerinin varsayılanı zaten bu; demo modu `1`'i reddeder |
| `HOS_DEMO_ALLOW_EXISTING_DATA` | boş | Herkese açık demoda asla ayarlamayın |

Başka bir şey gerekmez: joker DNS, `HOS_BASE_DOMAIN`, özel alan adı adımı,
e-posta ya da ödeme bilgisi yok. İlk açılış şemayı oluşturur, demoyu
yükler ve `HOS_BASE_URL` alan adını demo firmanın alan adı yapar.

## Veritabanı yalnızca demonun olmalı

Demo modu demo firmasını her aralıkta silip yeniden oluşturduğundan,
başka bir şey (başka bir firma ya da çok firmalı modun bir operatör hesabı)
içeren bir veritabanında açılmayı reddeder ve logda şunu yazar:

```text
HOS_DEMO_MODE refuses to start: this database holds data that is not the demo's …
```

PostgreSQL kaynağı daha önce normal bir HostingOS kurulumunca kullanıldıysa
(`hostingos setup` çalıştırıldıysa, bir firma oluşturulduysa) demoya
yeni ve boş bir veritabanı verin: Coolify'da yeni bir PostgreSQL kaynağı ya
da aynı sunucuda `create database hostingos_demo;` ve `DATABASE_URL`'de bu
veritabanı. Bunu `HOS_DEMO_ALLOW_EXISTING_DATA=1` ile aşmayın: diğer
veriler herkese açık bir demonun yanında kalır ve demo alan adını kendi
alan adı olarak zaten kullanan bir firma sıfırlamayı başarısız kılar.

## A — Coolify, Dockerfile ile derleme, ayrı PostgreSQL

Aşağıdaki Coolify bilgileri Coolify belgelerinden alınmıştır (Dockerfile
build pack, Health checks, Deployment commands, Basic authentication).

1. **PostgreSQL.** Bir PostgreSQL 16 kaynağı oluşturun (ya da boş bir
   tanesini kullanın) ve iç bağlantı adresini kopyalayın. Yedek gerekmez:
   demoda saklanmaya değer veri yoktur.
2. **Uygulama.** Yeni kaynak → Git deponuz → **Build pack: Dockerfile**,
   **Base Directory** `/`, **Dockerfile Location** `/Dockerfile`.
   **Docker Build Stage Target** boş kalsın (son aşama kaynaktan derler).
3. **Ağ.** **Ports Exposes** `8080`. **Domains**
   `https://demo.example.invalid`.
4. **Ortam değişkenleri.** Yukarıdaki tablodaki gibi (`DATABASE_URL` = 1.
   adımdaki iç adres). Her birinin **Build time** ayarını **Not
   available during build** yapın: imaj derlemesi bunların hiçbirine
   ihtiyaç duymaz, Coolify derleme sırasında kullanılabilen değişkenleri
   Dockerfile'a eklediği `ARG` satırlarına çevirir (bir değeri değiştirmek
   önbellekteki bağımlılık derlemesini boşa çıkarabilir) ve derleme
   argümanları imaj üst verisinde görünebilir. **Advanced → Build
   arguments** altında **Managed manually in Dockerfile** seçin ve derleme
   önbelleğini açık tutun.
5. **Sağlık denetimi.** Coolify'ın **Healthcheck** sayfasını *kapalı*
   bırakın: Coolify o zaman imajdaki `HEALTHCHECK`'i kullanır; bu da
   `/usr/local/bin/hostingos healthcheck` komutunu çalıştırır (kapsayıcının
   içinde 8080 portuna düz HTTP `GET /healthz`, çıkış kodu 0 / 1). Panel
   denetimini tercih ederseniz **Check type: CMD** seçip
   `/usr/local/bin/hostingos healthcheck` girin (veritabanını da denetlemek
   için: `/usr/local/bin/hostingos healthcheck --path /readyz`; bu,
   bekleyen migration varken de başarısız olur); **Start period** en az
   60 saniye olsun. HTTP türünü **seçmeyin**: imajın içinde
   `curl` ya da `wget` ister, bu imajda ikisi de yoktur. Denetimi başarısız
   olan kapsayıcıya Traefik trafik yönlendirmez.
6. **Post-deployment command**: boş bırakın. Demo modu bekleyen
   migration'ları sunucu bağlantı kabul etmeye *başlamadan önce* uygular
   (`hostingos serve --migrate` ile aynı), böylece hiçbir istek yarım
   kalmış bir veritabanına denk gelmez. Buraya daha önce
   `/usr/local/bin/hostingos migrate` yazdıysanız artık gerekmez; zararı da
   yoktur (sunucu migration yaparken bir veritabanı kilidini bekler, sonra
   yapacak bir şey bulamaz).
7. **Deploy.** İlk açılış migration'ları uygular, demoyu yükler ve
   sıfırlar (birkaç saniye); ardından sağlık denetimi "healthy" olur. Bir
   migration başarısız olursa kapsayıcı istek karşılamak yerine hatayı
   loga yazıp durur.
8. **Doğrulama** (kendi bilgisayarınızdan):

   ```sh
   curl -sI https://demo.example.invalid/ | grep -i -e '^HTTP' -e x-robots-tag
   # HTTP/2 200 … x-robots-tag: noindex, nofollow, noarchive
   ```

   Sonra siteyi açın, giriş sayfasında "Hosting firması yöneticisi" ile
   girin ve `/admin/server` sayfasını açın (salt okunur). Coolify'ın
   **Terminal**'inde `/usr/local/bin/hostingos healthcheck` komutu
   `ok: HTTP 200 …` yazar.
9. Demo çalışınca **HTTP basic auth'u kaldırın** (sonraki bölüm).

**Elle sıfırlama.** Uygulamanın Coolify **Terminal**'inde:

```sh
/usr/local/bin/hostingos demo-reset
```

Ziyaretçilerin girdiği her şeyi siler ve örnek verileri hemen geri yükler;
çalışan sunucu bunu 30 saniye içinde fark eder ve yeni bir aralık başlatır.
Aynı komutla bir **Scheduled Task** ve **Execute Now** da olur, **Restart**
da (her açılış sıfırlar).

**Güncelleme.** Yeniden deploy edin. Yeni kapsayıcı dinlemeye başlamadan
önce bekleyen migration'ları uygular ve demoyu sıfırlar. Bu sırada hâlâ
çalışan önceki kapsayıcı hizmet vermeyi sürdürür: kendi sürümünden ileride
olan bir veritabanını reddetmez, bir kez loglar ve hizmet verir
(migration'lar yalnızca ekleme yapar).

## HTTP basic auth'u kaldırma

Demo modu kendisi için `Authorization` başlığına ihtiyaç duymaz:
`Authorization: Basic …` başlığını yok sayar (oturumlar çerezdir; CSRF
denetimi bu istekleri çerezle kimliği doğrulanmış sayar), yani site önünde
basic auth olsa da olmasa da çalışır. Ancak:

* basic auth açıkken halk demoyu göremez;
* REST API anahtarları (`Authorization: Bearer …`) onun arkasından
  kullanılamaz, çünkü bir istek tek bir `Authorization` başlığı taşır (API
  demoda zaten salt okunurdur);
* kapsayıcının sağlık denetimi etkilenmez (Traefik üzerinden değil,
  kapsayıcının içinde çalışır).

Bu yüzden demo hazır olunca kapatın: uygulama → **General** → **Security >
Authentication**, **HTTP Basic Authentication**'dan başka bir seçeneğe
geçin (ya da basic auth etiketlerini elle eklediyseniz silin), kaydedin,
ardından Coolify'ın kapsayıcının vekil etiketlerini yeniden oluşturması
için **Restart** ya da yeniden deploy edin.

## Vekiller ve ziyaretçi IP adresi

Hız sınırları ziyaretçi IP'sine göredir, dolayısıyla uygulamanın bu adresi
görmesi gerekir. Yol: Cloudflare → Coolify'ın Traefik'i → uygulama.

* Coolify, Traefik'i `forwardedHeaders.trustedIPs` olmadan başlatır
  (Coolify'ın vekil yapılandırma kaynağıyla karşılaştırıldı); bu durumda
  Traefik aldığı `X-Forwarded-For`'a güvenmez ve onu bağlanan Cloudflare
  sunucusunun adresiyle değiştirir. Yalnızca `HOS_TRUSTED_PROXY_HOPS=1` ile
  tüm ziyaretçiler birkaç Cloudflare adresini paylaşır ve sınırlar herkese
  birden takılır; başka bir HOPS değeri de işe yaramaz, çünkü ziyaretçinin
  adresi artık başlıkta yoktur.
* Cloudflare ziyaretçinin adresini `CF-Connecting-IP` başlığında da
  gönderir; Traefik bu başlığı olduğu gibi iletir.
  `HOS_CLIENT_IP_HEADER=CF-Connecting-IP` uygulamanın bunu kullanmasını
  sağlar (tam olarak bir geçerli adres içeriyorsa). Yani:

  ```env
  HOS_TRUST_PROXY=true
  HOS_CLIENT_IP_HEADER=CF-Connecting-IP
  HOS_TRUSTED_PROXY_HOPS=1
  ```

  Yedek olarak `HOS_TRUSTED_PROXY_HOPS=1` doğrudur (Traefik'in tek girdisi).
  Traefik'i Cloudflare adres aralıklarına güvenecek şekilde ayarlarsanız
  `X-Forwarded-For` `ziyaretçi, cloudflare` olur ve uygun değer `2` olur;
  başlık ayarı her iki durumda da çalışır.
* `HOS_TRUSTED_PROXIES`, yönlendirme başlıkları (`CF-Connecting-IP`,
  `X-Forwarded-For`, `X-Forwarded-Proto`) dikkate alınacak bağlantı
  adreslerini sayar; başka bir adresten gelen bağlantı kendi adresiyle
  sınırlanır. Traefik uygulamaya, uygulamanın **Destination**'ına ait
  Docker ağı üzerinden ulaşır (başka bir tane oluşturmadıysanız adı
  `coolify`), dolayısıyla o ağın aralığını yazın. Sunucuda:

  ```sh
  docker network inspect coolify --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}'
  # ör. 10.0.1.0/24 yazar → HOS_TRUSTED_PROXIES=10.0.1.0/24
  ```

  Docker Compose uygulamasında bunun yerine `app` kapsayıcısının ağlarını
  listeleyin (`docker inspect <kapsayıcı> --format '{{range $k, $v :=
  .NetworkSettings.Networks}}{{$k}} {{end}}'`) ve her birini aynı şekilde
  inceleyin. Listelenen bir ağdaki her kapsayıcı bu başlıkları
  ayarlayabilir; o ağa başkalarının kapsayıcılarını koymayın. Değişken boş
  bırakılırsa uygulama eskisi gibi her bağlantıya güvenir ve açılışta bir
  uyarı loglar; adres ya da aralık olmayan bir değer açılışı hatayla
  durdurur.
* Sunucuya Cloudflare'i atlayarak doğrudan ulaşan herkes yine Traefik
  üzerinden girer ve kendi `CF-Connecting-IP` değerini gönderebilir;
  yukarıdaki liste bunu engellemez. Sunucu güvenlik duvarında 80 ve
  443 portlarına yalnızca [Cloudflare IP aralıklarından](https://www.cloudflare.com/ips/)
  izin verin (ya da bir Cloudflare Tunnel kullanın).
* https: ayarlanacak bir şey yok. `HOS_BASE_URL` `https://` ile başlıyorsa
  uygulama, son halkanın bildirdiği şemadan bağımsız olarak istekleri https
  sayar. Cloudflare'in SSL/TLS modunu Coolify'ın Cloudflare kılavuzunun
  önerdiği gibi **Full (strict)** tutun.

Cloudflare olmadan (yalnızca Traefik): `HOS_TRUST_PROXY=true`,
`HOS_TRUSTED_PROXY_HOPS=1`, yukarıdaki gibi `HOS_TRUSTED_PROXIES`,
`HOS_CLIENT_IP_HEADER` yok.

**Yavaş istemciler.** Uygulama, istek başlıkları 120 saniyeden uzun süren
bir bağlantıyı kapatır, 60 saniyede tamamlanmayan bir gövdeye 408 ve
120 saniye sonra hâlâ süren bir isteğe 503 döner
(`HOS_HTTP_HEADER_TIMEOUT_SECS`, `HOS_HTTP_BODY_TIMEOUT_SECS`,
`HOS_HTTP_REQUEST_TIMEOUT_SECS`; docs/DEPLOYMENT.md). Önünde Traefik'in
kendi giriş noktası sınırları da geçerlidir (varsayılan olarak bir isteği
okumak için 60 saniye, boşta 180 saniye); demo için varsayılanların
değişmesi gerekmez.

## B — docker-compose.demo.yml

Yığın: `postgres` (PostgreSQL 16, adlandırılmış birim, dışarıya açık
değil) ve `app` (Dockerfile'dan derlenir, demo modu, salt okunur kök dosya
sistemi, yetkisiz, CPU / bellek sınırlı, sağlık denetimi `hostingos
healthcheck --path /readyz`).

**Coolify'da:** Yeni kaynak → Git deposu → **Build pack: Docker Compose**,
**Base Directory** `/`, **Docker Compose Location**
`/docker-compose.demo.yml`. Coolify dosyanın istediği değişkenleri
listeler: `POSTGRES_PASSWORD` (`openssl rand -hex 32`), `HOS_SECRET_KEY`,
`HOS_BASE_URL`; Cloudflare arkasında `HOS_CLIENT_IP_HEADER=CF-Connecting-IP`
de ekleyin. `app` servisine `https://demo.example.invalid:8080` alan adını
verin (Compose servislerinde port alan adının sonuna yazılır). Compose
uygulamalarında basic auth, Compose dosyasındaki etiketlerle yapılır; demo
yayına girince bunları kaldırın. Sıfırlama: `app` kapsayıcısının
**Terminal**'inde `/usr/local/bin/hostingos demo-reset`.

**Kendi ters vekili olan bir Docker sunucusunda:**

```sh
cp .env.demo.example .env.demo      # ZORUNLU (REQUIRED) değerleri doldurun
docker compose -f docker-compose.demo.yml -f docker-compose.demo.ports.yml \
  --env-file .env.demo up -d --build --wait
# uygulama 127.0.0.1:8080'de; demo.example.invalid için ters vekili
# buraya yönlendirin
docker compose -f docker-compose.demo.yml --env-file .env.demo exec app \
  /usr/local/bin/hostingos demo-reset            # elle sıfırlama
docker compose -f docker-compose.demo.yml --env-file .env.demo down --volumes
                                                 # her şeyi kaldırma
```

## İzleme ve loglar

* Dışarıdan `GET /readyz` bir erişilebilirlik izleyicisi için: veritabanına
  ulaşılıyor ve çalışan sürümün tüm migration'ları uygulanmışsa 200,
  bekleyen migration varken `"schema":"behind"` ile 503; canlılık için
  `/healthz`.
* Loglar JSON'dur (`HOS_LOG_FORMAT=json`). Her sıfırlama süresiyle birlikte
  `demo reset` yazar; 30 saniye sonra hâlâ süren istekler artık onu
  bekletmez (`demo reset: requests were still running; resetting anyway`);
  başarısız bir sıfırlama hata loglar ve bir dakika sonra
  yeniden denenir. Reddedilen istekler info düzeyinde
  `demo mode: request refused` yazar.
* Demoda yedeklenecek veri yoktur. Kaldırmak için uygulamayı ve
  veritabanını silin.
