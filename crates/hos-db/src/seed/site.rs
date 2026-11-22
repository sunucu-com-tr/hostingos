//! Default storefront copy every tenant starts with (tr + en CMS pages).
//!
//! Pages are ordinary `pages` rows, edited in the admin content editor; the
//! bodies use the block syntax and tokens of `hos-web`'s
//! `routes::storefront::blocks` (`:::features …:::`, `{{site_name}}`,
//! `{{base}}` = the locale prefix of links). Nothing here states a fact that
//! only the tenant can know (uptime, customer counts, locations, awards):
//! such spots are `:::todo` blocks, visible to content editors only.
//!
//! * `home`, `domains`, `contact` — copy merged into those routes;
//! * category landings (`template = 'landing'`) — merged into the product
//!   group with the same slug; published for the demo, drafts elsewhere
//!   (a tenant publishes the ones it sells);
//! * company pages (about, infrastructure, hosting types, status) and the
//!   legal templates (`template = 'legal'`: terms, privacy / KVKK, refund,
//!   SLA), each opening with a `:::todo` review reminder.
//!
//! Inserts are keyed on `(tenant, locale, slug)` and never overwrite. For
//! the demo, [`refresh_untouched`] replaces seeded rows that nobody edited
//! (`updated_at = created_at`) with the current defaults.

use hos_core::ids::TenantId;
use sqlx::PgConnection;
use uuid::Uuid;

use super::SeedReport;
use crate::error::DbResult;

/// One language version.
pub struct SiteDoc {
    /// Slug in that language.
    pub slug: &'static str,
    /// Title.
    pub title: &'static str,
    /// Markdown body with blocks.
    pub body: &'static str,
}

/// A page in both languages.
pub struct SitePage {
    /// Turkish.
    pub tr: SiteDoc,
    /// English.
    pub en: SiteDoc,
    /// `default`, `landing` or `legal`.
    pub template: &'static str,
    /// Shown in the main navigation.
    pub nav: bool,
    /// Listed in the footer.
    pub footer: bool,
}

impl SitePage {
    /// A category landing (draft outside the demo).
    pub fn is_landing(&self) -> bool {
        self.template == "landing"
    }
}

/// The default pages.
pub const SITE_PAGES: &[SitePage] = &[
    // ------------------------------------------------------------------ home
    SitePage {
        template: "default",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "home",
            title: "Ana sayfa",
            body: r##":::hero Web siteniz, sunucunuz ve alan adınız tek yerde
{{site_name}}; web hosting, sunucu ve alan adı hizmetlerini tek panelden, şeffaf fiyatlarla sunar.
:::

:::features Neden {{site_name}}?
Siparişten faturaya kadar her adım tek hesapta. İhtiyacınız büyüdükçe paketinizi değiştirmeniz yeterli.

### :zap: Ödemeden sonra otomatik kurulum
Ödemeniz onaylandığında hizmetiniz otomatik olarak hazırlanır, giriş bilgileri e-postanıza ve müşteri panelinize gelir.

### :shield: Hesap güvenliği
İki aşamalı doğrulama, şifreli bağlantılar ve oturum kontrolleri tüm hesaplarda kullanılabilir.

### :receipt: Şeffaf faturalandırma
Fiyatlar KDV durumu belirtilerek gösterilir. Aylık ya da yıllık ödeyin, her ödeme için fatura alın.

### :globe: Alan adı yönetimi
Alan adı kaydı, transferi, yenilemesi ve DNS ayarları hostinginizle aynı panelde.

### :life-buoy: Destek talepleri
Teknik ve idari sorularınızı müşteri panelinden talep açarak iletin; yazışmalarınız tek yerde saklanır.

### :terminal: Tek panel
Hizmetler, faturalar, alan adları ve destek talepleri aynı müşteri panelinde.
:::

:::steps Nasıl başlarım?
Hesap açmak ücretsizdir; yalnızca seçtiğiniz hizmet için ödeme yaparsınız.

### Paketinizi seçin
Web hosting, sunucu ya da alan adı; ihtiyacınıza uyan paketi ve ödeme dönemini belirleyin.

### Ödemeyi tamamlayın
Sepetinizi onaylayın, desteklenen ödeme yöntemlerinden biriyle ödeyin; faturanız otomatik oluşur.

### Hizmetiniz hazır
Kurulum tamamlanınca bilgileriniz panelinize ve e-postanıza gelir; sitenizi hemen yayına alabilirsiniz.
:::

:::faq Sıkça sorulan sorular
### Hangi paketi seçmeliyim?
Tek bir web sitesi veya blog için web hosting yeterlidir. Kendi yazılımınızı kurmak, root erişimi veya ayrılmış kaynak istiyorsanız VPS, en yüksek performans ve tam donanım kontrolü için dedicated sunucu seçin. Karar veremezseniz [hosting türleri karşılaştırmasına]({{base}}/hosting-turleri) bakın.

### Paketimi daha sonra değiştirebilir miyim?
Evet. Müşteri panelinden daha üst bir pakete geçiş talep edebilirsiniz; fiyat farkı kalan süreye göre hesaplanır.

### Hangi ödeme yöntemlerini kabul ediyorsunuz?
Kullanılabilir ödeme yöntemleri sepet ve ödeme adımında listelenir. Her ödeme için müşteri panelinizde fatura bulunur.

### Destek ekibine nasıl ulaşırım?
Müşteri panelinden destek talebi açabilir ya da [iletişim sayfasındaki]({{base}}/contact) kanalları kullanabilirsiniz.
:::

:::cta Hemen başlayın
Hesabınızı birkaç dakikada oluşturun, hizmetinizi seçin ve yayına alın.

[Ücretsiz hesap oluşturun]({{base}}/auth/register) [Bize ulaşın]({{base}}/contact)
:::
"##,
        },
        en: SiteDoc {
            slug: "home",
            title: "Home",
            body: r##":::hero Your website, server and domain in one place
{{site_name}} provides web hosting, servers and domain names from a single panel, with transparent prices.
:::

:::features Why {{site_name}}?
Every step from order to invoice lives in one account. When you need more, switch plans.

### :zap: Automatic setup after payment
Once your payment is confirmed the service is provisioned automatically and the login details arrive by e-mail and in your client area.

### :shield: Account security
Two-factor authentication, encrypted connections and session controls are available on every account.

### :receipt: Transparent billing
Prices show whether VAT is included. Pay monthly or yearly and get an invoice for every payment.

### :globe: Domain management
Register, transfer and renew domains and edit DNS next to your hosting.

### :life-buoy: Support tickets
Send technical and billing questions as tickets from your client area; every conversation stays in one place.

### :terminal: One panel
Services, invoices, domains and tickets in the same client area.
:::

:::steps How do I get started?
Creating an account is free; you only pay for the service you choose.

### Pick a plan
Web hosting, a server or a domain: choose the plan and the billing period that fit.

### Complete the payment
Confirm your cart and pay with one of the supported methods; the invoice is created automatically.

### Your service is ready
When setup completes, the details appear in your client area and inbox, ready to go live.
:::

:::faq Frequently asked questions
### Which plan should I choose?
Web hosting is enough for a single website or blog. Choose a VPS when you need your own software, root access or dedicated resources, and a dedicated server for maximum performance and full hardware control. Still unsure? Read the [hosting types comparison]({{base}}/hosting-types-compared).

### Can I change my plan later?
Yes. Request an upgrade from your client area; the price difference is calculated for the remaining term.

### Which payment methods do you accept?
The available methods are listed in the cart and at checkout. Every payment has an invoice in your client area.

### How do I reach support?
Open a ticket from your client area or use the channels on the [contact page]({{base}}/contact).
:::

:::cta Get started today
Create your account in minutes, pick a service and go live.

[Create a free account]({{base}}/auth/register) [Contact us]({{base}}/contact)
:::
"##,
        },
    },
    // --------------------------------------------------------------- domains
    SitePage {
        template: "default",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "domains",
            title: "Alan adı sorgulama ve kayıt",
            body: r##"Alan adı, web sitenizin ve e-posta adreslerinizin internetteki adresidir (ör. ornek.com.tr). {{site_name}} ile alan adınızı sorgulayın, kaydedin, başka bir firmadan transfer edin ve DNS kayıtlarını müşteri panelinden yönetin.

:::steps Alan adı transferi nasıl yapılır?
Transfer sırasında alan adınızın süresi kaybolmaz; transfer ücreti genellikle bir yıllık yenilemeyi de kapsar.

### Kilidi kaldırın
Mevcut firmanızın panelinde alan adının transfer kilidini kapatın.

### Transfer kodunu alın
EPP / yetki kodunu (auth code) mevcut firmanızdan isteyin.

### Transfer siparişi verin
Alan adını bu sayfada aratın, "transfer" seçeneğiyle sepete ekleyin ve kodu girin.

### Onaylayın
Kayıt şirketinin gönderdiği onay e-postasını kabul edin; işlem tamamlanınca alan adı panelinizde görünür.
:::

:::faq Alan adları hakkında sık sorulanlar
### Alan adı transferi ne kadar sürer?
Uzantıya göre değişir; çoğu uluslararası uzantıda transfer, onaydan sonra birkaç gün içinde tamamlanır. Süreç boyunca siteniz yayında kalır.

### .com.tr alan adı için hangi belgeler gerekir?
.tr uzantılarının kuralları TRABİS tarafından belirlenir; bazı alt uzantılar belge ister. Sipariş adımında gereken bilgiler gösterilir.

### Alan adımın süresi dolarsa ne olur?
Süre dolmadan önce e-posta ile hatırlatma gönderilir. Otomatik yenilemeyi açarak alan adınızın boşa düşmesini önleyebilirsiniz.

### DNS ayarlarımı kendim değiştirebilir miyim?
Evet. Ad sunucularını ve DNS kayıtlarını (A, CNAME, MX, TXT) müşteri panelinden düzenleyebilirsiniz.
:::
"##,
        },
        en: SiteDoc {
            slug: "domains",
            title: "Domain search and registration",
            body: r##"A domain name is the address of your website and e-mail on the internet (e.g. example.com). With {{site_name}} you can search, register and transfer domains, and manage their DNS records from your client area.

:::steps How do I transfer a domain?
A transfer does not shorten your registration; the transfer fee usually includes a one-year renewal.

### Unlock the domain
Turn off the transfer lock in your current provider's panel.

### Get the transfer code
Ask your current provider for the EPP / authorisation code.

### Order the transfer
Search the domain on this page, add it to the cart as a transfer and enter the code.

### Approve
Accept the confirmation e-mail sent by the registry; once complete, the domain appears in your client area.
:::

:::faq Domain questions
### How long does a domain transfer take?
It depends on the extension; for most generic extensions the transfer completes within a few days of approval. Your website stays online throughout.

### What happens if my domain expires?
You receive e-mail reminders before the expiry date. Turn on auto-renewal to make sure the domain is never lost.

### Can I edit my DNS settings myself?
Yes. Nameservers and DNS records (A, CNAME, MX, TXT) can be edited from your client area.
:::
"##,
        },
    },
    // --------------------------------------------------------------- contact
    SitePage {
        template: "default",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "contact",
            title: "İletişim",
            body: r##"Satış, faturalandırma ve teknik sorularınız için {{site_name}} ekibine bu formdan yazabilirsiniz. Mevcut müşterilerimiz için en hızlı yol, müşteri panelinden destek talebi açmaktır.

:::faq İletişim hakkında
### Destek talebi mi açmalıyım, form mu doldurmalıyım?
Bir hizmetinizle ilgili sorularda müşteri panelinden talep açın; talep ilgili hizmete bağlanır ve yazışmalar kayıt altında tutulur. Satış öncesi sorular için bu form yeterlidir.

### Faturamla ilgili soruyu nereye iletmeliyim?
Müşteri panelinde faturalandırma departmanına talep açın ya da formda konu olarak faturanızı belirtin.
:::

:::todo Çalışma saatleri ve yanıt süreleri
Destek saatlerinizi ve hedef yanıt sürelerinizi buraya ekleyin. Gerçekte uyguladığınız süreleri yazın.
:::
"##,
        },
        en: SiteDoc {
            slug: "contact",
            title: "Contact",
            body: r##"Write to the {{site_name}} team with this form for sales, billing or technical questions. For existing customers the fastest route is a support ticket from the client area.

:::faq About contacting us
### Should I open a ticket or use this form?
For questions about one of your services, open a ticket from the client area: it is linked to the service and the conversation is kept on record. Pre-sales questions are fine here.

### Where do I send a question about an invoice?
Open a ticket for the billing department in your client area, or mention the invoice in the subject of this form.
:::

:::todo Opening hours and response times
Add your support hours and the response times you actually work to.
:::
"##,
        },
    },
    // ----------------------------------------------------------------- terms
    SitePage {
        template: "legal",
        nav: false,
        footer: true,
        tr: SiteDoc {
            slug: "kullanim-kosullari",
            title: "Hizmet sözleşmesi ve kullanım koşulları",
            body: r##":::todo Hukuki inceleme gerekli
Bu metin bir şablondur. Yayında tutmadan önce şirketinize ve hizmetlerinize göre bir hukuk danışmanıyla gözden geçirin.
:::

Bu koşullar, {{legal_name}} ("{{site_name}}") ile hizmet satın alan müşteri arasındaki ilişkiyi düzenler. Sipariş vererek bu koşulları kabul etmiş sayılırsınız.

## 1. Hizmetler

{{site_name}}; web hosting, sunucu, alan adı ve ilgili ek hizmetleri, sipariş sırasında belirtilen özellik ve fiyatlarla sunar. Hizmet, ödemenin onaylanmasının ardından etkinleştirilir.

## 2. Ödeme ve yenileme

Hizmetler seçilen dönem (aylık, yıllık vb.) için peşin ücretlendirilir. Yenileme faturaları vade tarihinden önce düzenlenir; vadesinde ödenmeyen hizmetler askıya alınabilir ve süre sonunda kapatılabilir.

## 3. Kullanım kuralları

Hizmetler yürürlükteki mevzuata aykırı içerik, istenmeyen toplu e-posta (spam), kötü amaçlı yazılım veya başkalarının sistemlerine zarar verecek faaliyetler için kullanılamaz. Bu kurallara aykırılık halinde hizmet bildirimle veya acil durumlarda bildirimsiz durdurulabilir.

## 4. Yedekleme ve veri sorumluluğu

Müşteri, kendi verilerinin güncel yedeğini tutmakla yükümlüdür. Sağlayıcının aldığı yedekler ek güvence niteliğindedir.

## 5. Sorumluluğun sınırı

{{site_name}}, hizmet kesintilerinden doğan dolaylı zararlardan sorumlu tutulamaz. Hizmet seviyesi taahhütleri [hizmet seviyesi sözleşmesinde]({{base}}/hizmet-seviyesi-sozlesmesi) yer alır.

## 6. İletişim

Bu koşullarla ilgili sorularınız için {{email}} adresine yazabilirsiniz.
"##,
        },
        en: SiteDoc {
            slug: "terms-of-service",
            title: "Terms of service",
            body: r##":::todo Legal review required
This text is a template. Have it reviewed by a lawyer for your company and services before relying on it.
:::

These terms govern the relationship between {{legal_name}} ("{{site_name}}") and the customer purchasing services. By placing an order you accept these terms.

## 1. Services

{{site_name}} provides web hosting, servers, domain names and related add-ons with the features and prices stated at the time of order. A service is activated once the payment is confirmed.

## 2. Payment and renewal

Services are billed in advance for the selected period (monthly, yearly, etc.). Renewal invoices are issued before the due date; services left unpaid may be suspended and later terminated.

## 3. Acceptable use

Services may not be used for unlawful content, unsolicited bulk e-mail (spam), malware or activities that harm other systems. Violations may lead to suspension after notice, or without notice in an emergency.

## 4. Backups and data

Customers are responsible for keeping current backups of their own data. Backups taken by the provider are an additional safeguard.

## 5. Limitation of liability

{{site_name}} is not liable for indirect damages arising from service interruptions. Service level commitments are set out in the [service level agreement]({{base}}/service-level-agreement).

## 6. Contact

Questions about these terms: {{email}}.
"##,
        },
    },
    // --------------------------------------------------------------- privacy
    SitePage {
        template: "legal",
        nav: false,
        footer: true,
        tr: SiteDoc {
            slug: "gizlilik-politikasi",
            title: "Gizlilik politikası ve KVKK aydınlatma metni",
            body: r##":::todo Hukuki inceleme gerekli
Bu metin 6698 sayılı KVKK kapsamındaki aydınlatma yükümlülüğü için bir şablondur. Veri sorumlusu bilgilerini, işleme amaçlarını ve aktarım yapılan tarafları gerçeğe uygun hale getirin ve bir hukuk danışmanına onaylatın.
:::

Bu metin, veri sorumlusu sıfatıyla {{legal_name}} tarafından kişisel verilerinizin hangi amaçlarla ve nasıl işlendiğini 6698 sayılı Kişisel Verilerin Korunması Kanunu ("KVKK") uyarınca açıklar.

## Hangi verileri işliyoruz?

- **Kimlik ve iletişim:** ad, soyad, e-posta, telefon, adres.
- **Fatura bilgileri:** vergi numarası / T.C. kimlik numarası (fatura için gerekli olduğunda), ödeme kayıtları.
- **Hizmet kayıtları:** sipariş, hizmet, alan adı ve destek talebi kayıtları.
- **Güvenlik kayıtları:** oturum açma zamanları, IP adresleri.

## Verileriniz hangi amaçlarla işlenir?

Sözleşmenin kurulması ve ifası, faturalandırma ve yasal yükümlülükler, hizmet güvenliği ve destek taleplerinin yanıtlanması.

## Veriler kimlere aktarılır?

Alan adı kayıt kuruluşlarına (alan adı tescili için zorunlu bilgiler), ödeme hizmeti sağlayıcılarına ve yasal olarak yetkili kurumlara, yalnızca ilgili amaçla sınırlı olarak.

## Haklarınız nelerdir?

KVKK'nın 11. maddesi uyarınca verilerinizin işlenip işlenmediğini öğrenme, düzeltilmesini veya silinmesini isteme ve itiraz etme haklarına sahipsiniz. Başvurularınızı {{email}} adresine iletebilirsiniz.

## Çerezler

Sitemiz yalnızca oturum, güvenlik ve dil/tema tercihi için gerekli çerezleri kullanır.
"##,
        },
        en: SiteDoc {
            slug: "privacy-policy",
            title: "Privacy policy",
            body: r##":::todo Legal review required
This text is a template. Make the controller details, purposes and recipients accurate for your company and have it approved by a lawyer.
:::

This policy explains how {{legal_name}}, as the data controller, processes your personal data and for which purposes.

## What data do we process?

- **Identity and contact:** name, e-mail, phone, address.
- **Billing:** tax or ID number (where required for invoicing), payment records.
- **Service records:** orders, services, domains and support tickets.
- **Security logs:** sign-in times and IP addresses.

## Why do we process it?

To conclude and perform the contract, for billing and legal obligations, for service security and to answer support requests.

## Who receives it?

Domain registries (data required for registration), payment service providers and legally authorised authorities — limited to the purpose concerned.

## What are your rights?

You may ask whether your data is processed, request correction or deletion, and object to processing. Send requests to {{email}}.

## Cookies

This site only uses cookies needed for sessions, security and your language / theme preference.
"##,
        },
    },
    // ---------------------------------------------------------------- refund
    SitePage {
        template: "legal",
        nav: false,
        footer: true,
        tr: SiteDoc {
            slug: "iade-politikasi",
            title: "İade politikası",
            body: r##":::todo İade koşullarınızı belirleyin
Bu metin bir şablondur. İade süresini, iade edilmeyen hizmetleri ve iade yöntemini kendi uygulamanıza göre düzenleyin; Mesafeli Sözleşmeler Yönetmeliği'ne uygunluğunu kontrol ettirin.
:::

Bu politika, {{site_name}} hizmetlerinde iade taleplerinin nasıl değerlendirildiğini açıklar.

## Hangi hizmetler iade edilebilir?

İade koşulları hizmet türüne göre değişir ve sipariş sırasında gösterilir. Alan adı kayıt, transfer ve yenileme ücretleri, kayıt kuruluşuna aktarıldıkları için iade edilemez.

## İade talebimi nasıl iletirim?

Müşteri panelinden faturalandırma departmanına destek talebi açın; hizmet adını ve talebinizin gerekçesini yazın.

## İade nasıl yapılır?

Onaylanan iadeler, ödemenin yapıldığı yönteme veya hesabınıza kredi olarak yapılır. Süreç, ödeme kuruluşunun işlem sürelerine bağlıdır.
"##,
        },
        en: SiteDoc {
            slug: "refund-policy",
            title: "Refund policy",
            body: r##":::todo Define your refund terms
This text is a template. Set the refund window, the non-refundable services and the refund method you actually apply, and have it checked against consumer law.
:::

This policy explains how {{site_name}} handles refund requests.

## Which services can be refunded?

Refund conditions depend on the service type and are shown at the time of order. Domain registration, transfer and renewal fees are non-refundable because they are passed on to the registry.

## How do I request a refund?

Open a ticket for the billing department in your client area with the service name and the reason for your request.

## How is a refund paid?

Approved refunds go back to the original payment method or to your account as credit. Timing depends on the payment provider.
"##,
        },
    },
    // ------------------------------------------------------------------- SLA
    SitePage {
        template: "legal",
        nav: false,
        footer: true,
        tr: SiteDoc {
            slug: "hizmet-seviyesi-sozlesmesi",
            title: "Hizmet seviyesi sözleşmesi (SLA)",
            body: r##":::todo Taahhütlerinizi girin
Bu metin bir şablondur. Aylık erişilebilirlik hedefini, ölçüm yöntemini, telafi oranlarını ve başvuru süresini yalnızca gerçekten sağlayabileceğiniz değerlerle, bir `:::spec Taahhüt özeti` bloğu olarak ekleyin.
:::

Hizmet seviyesi sözleşmesi (SLA), {{site_name}} hizmetlerinin erişilebilirlik hedeflerini ve bu hedefler karşılanmadığında uygulanacak telafiyi tanımlar.

## Kapsam dışı durumlar nelerdir?

Önceden duyurulan planlı bakımlar, müşterinin yaptığı yapılandırma hataları, kötüye kullanım nedeniyle yapılan askıya almalar ve sağlayıcının kontrolü dışındaki olaylar.

## Telafi talebini nasıl iletirim?

Kesintiyi izleyen dönem içinde müşteri panelinden faturalandırma departmanına talep açın; kesinti zamanını ve etkilenen hizmeti belirtin.
"##,
        },
        en: SiteDoc {
            slug: "service-level-agreement",
            title: "Service level agreement (SLA)",
            body: r##":::todo Enter your commitments
This text is a template. Add the monthly availability target, how it is measured, the service credits and the claim window — only values you can actually deliver — as a `:::spec Commitments at a glance` block.
:::

The service level agreement (SLA) defines the availability targets of {{site_name}} services and the credit applied when a target is missed.

## What is excluded?

Planned maintenance announced in advance, configuration errors made by the customer, suspensions for abuse and events outside the provider's control.

## How do I claim a credit?

Within the following billing period, open a ticket for the billing department in your client area with the time of the outage and the affected service.
"##,
        },
    },
    // ----------------------------------------------------------------- about
    SitePage {
        template: "default",
        nav: true,
        footer: false,
        tr: SiteDoc {
            slug: "hakkimizda",
            title: "Hakkımızda",
            body: r##"{{legal_name}}, {{site_name}} markasıyla web hosting, sunucu ve alan adı hizmetleri sunan bir hosting sağlayıcısıdır. Müşterilerimiz siparişlerini, faturalarını, alan adlarını ve destek taleplerini tek bir müşteri panelinden yönetir.

:::todo Şirket hikâyesi
Kuruluş yılınızı, ekibinizi ve sizi farklı kılan noktaları buraya yazın. Yalnızca doğrulanabilir bilgiler kullanın.
:::

## Ne sunuyoruz?

- **Web hosting:** Web siteleri, bloglar ve e-ticaret için paylaşımlı barındırma.
- **Sunucular:** Root erişimli sanal sunucular (VPS) ve dedicated sunucular.
- **Alan adları:** Kayıt, transfer, yenileme ve DNS yönetimi.
- **Destek:** Müşteri panelinden açılan taleplerle teknik ve idari destek.

## Nasıl çalışıyoruz?

Fiyatlarımızı KDV durumu ile birlikte açıkça yayınlıyoruz. Ödemesi tamamlanan hizmetler otomatik olarak kurulur; her ödeme için fatura düzenlenir. Politikalarımız [kullanım koşulları]({{base}}/kullanim-kosullari), [gizlilik politikası]({{base}}/gizlilik-politikasi) ve [iade politikası]({{base}}/iade-politikasi) sayfalarında yer alır.

:::spec Şirket bilgileri
| Bilgi | Değer |
|---|---|
| Ticari unvan | {{legal_name}} |
| Adres | {{address}} |
| E-posta | {{email}} |
| Telefon | {{phone}} |
:::

:::cta Sorunuz mu var?
Ekibimize iletişim formundan ya da müşteri panelinden ulaşabilirsiniz.

[İletişime geçin]({{base}}/contact)
:::
"##,
        },
        en: SiteDoc {
            slug: "about-us",
            title: "About us",
            body: r##"{{legal_name}} is a hosting provider offering web hosting, servers and domain names under the {{site_name}} brand. Customers manage orders, invoices, domains and support tickets from a single client area.

:::todo Company story
Add your founding year, your team and what sets you apart. Use verifiable facts only.
:::

## What do we offer?

- **Web hosting:** shared hosting for websites, blogs and online stores.
- **Servers:** virtual private servers (VPS) with root access and dedicated servers.
- **Domains:** registration, transfer, renewal and DNS management.
- **Support:** technical and billing help through tickets in the client area.

## How do we work?

We publish our prices openly, including their VAT status. Paid services are provisioned automatically and every payment gets an invoice. Our policies are in the [terms of service]({{base}}/terms-of-service), [privacy policy]({{base}}/privacy-policy) and [refund policy]({{base}}/refund-policy).

:::spec Company details
| Item | Details |
|---|---|
| Legal name | {{legal_name}} |
| Address | {{address}} |
| E-mail | {{email}} |
| Phone | {{phone}} |
:::

:::cta Have a question?
Reach our team through the contact form or your client area.

[Contact us]({{base}}/contact)
:::
"##,
        },
    },
    // -------------------------------------------------------- infrastructure
    SitePage {
        template: "default",
        nav: false,
        footer: true,
        tr: SiteDoc {
            slug: "altyapi",
            title: "Veri merkezi ve ağ altyapısı",
            body: r##"Bu sayfa, {{site_name}} hizmetlerinin çalıştığı veri merkezi, ağ ve donanım altyapısını özetler. Altyapı bilgileri, bir hosting sağlayıcısını değerlendirirken performans, güvenilirlik ve veri konumu açısından en önemli ölçütlerdir.

:::todo Altyapı bilgilerini doldurun
Veri merkezinin bulunduğu şehir/ülke, sertifikalar (ör. ISO 27001), enerji ve soğutma yedekliliği, ağ bağlantı kapasitesi, disk teknolojisi ve yedekleme bilgilerini bir `:::spec Altyapı özeti` bloğu olarak bu notun yerine ekleyin (iki sütunlu tablo: Özellik | Bilgi). Yalnızca doğrulayabildiğiniz bilgileri yazın.
:::

## Verilerim nerede saklanır?

Hizmetlerinizin verileri yukarıda belirtilen veri merkezinde tutulur. Kişisel verilerin işlenmesine ilişkin ayrıntılar [gizlilik politikasında]({{base}}/gizlilik-politikasi) yer alır.

## Bakım çalışmalarından nasıl haberdar olurum?

Planlı bakım ve kesintiler [duyurular]({{base}}/announcements) sayfasında ve [durum]({{base}}/durum) sayfasında yayınlanır.
"##,
        },
        en: SiteDoc {
            slug: "infrastructure",
            title: "Data centre and network",
            body: r##"This page summarises the data centre, network and hardware behind {{site_name}} services. Infrastructure details are among the most important criteria when evaluating a hosting provider: they determine performance, reliability and where your data lives.

:::todo Fill in your infrastructure
Replace this note with a `:::spec Infrastructure at a glance` block (a two-column table: Item | Details) listing the data centre city/country, certifications (e.g. ISO 27001), power and cooling redundancy, network capacity, storage technology and backups. Only state what you can verify.
:::

## Where is my data stored?

Your services' data is kept in the data centre listed above. How personal data is processed is described in the [privacy policy]({{base}}/privacy-policy).

## How will I hear about maintenance?

Planned maintenance and incidents are published under [announcements]({{base}}/announcements) and on the [status page]({{base}}/status).
"##,
        },
    },
    // ------------------------------------------------------- hosting types
    SitePage {
        template: "default",
        nav: false,
        footer: true,
        tr: SiteDoc {
            slug: "hosting-turleri",
            title: "Hosting türleri karşılaştırması",
            body: r##"Hosting türleri, bir web sitesinin veya uygulamanın sunucu kaynaklarını nasıl paylaştığına göre ayrılır: web hosting kaynakları paylaşır, VPS ayrılmış sanal kaynak sunar, dedicated sunucu ise fiziksel bir makinenin tamamını size ayırır. Bu rehber, {{site_name}} hizmetleri arasında doğru seçimi yapmanıza yardımcı olur.

:::compare Hosting türleri bir bakışta
| | Web hosting | WordPress hosting | VPS | Dedicated sunucu |
|---|---|---|---|---|
| Kimler için? | Tanıtım siteleri, bloglar | WordPress siteleri | Yazılımcılar, büyüyen projeler | Yüksek trafik, özel donanım ihtiyacı |
| Kaynaklar | Paylaşımlı | Paylaşımlı, WordPress'e göre ayarlı | Ayrılmış sanal kaynak | Fiziksel sunucunun tamamı |
| Root erişimi | ✗ | ✗ | ✓ | ✓ |
| Yönetim paneli | ✓ | ✓ | İsteğe bağlı | İsteğe bağlı |
| Teknik bilgi | Gerekmez | Gerekmez | Gerekir | Gerekir |
| Ölçekleme | Paket yükseltme | Paket yükseltme | Kaynak ekleme | Donanım değişikliği |
:::

## Web hosting nedir?

Web hosting, birçok sitenin aynı sunucunun kaynaklarını paylaştığı, bakımı sağlayıcı tarafından yapılan en ekonomik barındırma türüdür. Kurumsal tanıtım siteleri ve bloglar için idealdir.

## VPS nedir?

VPS (sanal özel sunucu), fiziksel bir sunucunun sanallaştırma ile bölünmüş, size ayrılmış işlemci, bellek ve disk kaynağı sunan bölümüdür. Root erişimiyle istediğiniz yazılımı kurabilirsiniz.

## Dedicated sunucu nedir?

Dedicated sunucu, fiziksel bir sunucunun tüm kaynaklarının yalnızca bir müşteriye ayrıldığı barındırma türüdür. En yüksek performans ve tam donanım kontrolü sağlar.

:::faq Karar verirken
### Hangi durumda web hosting'den VPS'e geçmeliyim?
Siteniz paylaşımlı kaynak sınırlarına sık takılıyorsa, özel bir yazılım kurmanız gerekiyorsa ya da root erişimi istiyorsanız VPS'e geçmenin zamanı gelmiştir.

### Taşıma sırasında sitem kapanır mı?
Taşıma planlı yapıldığında kesinti DNS değişikliğinin yayılmasıyla sınırlı kalır. Ayrıntılar için destek ekibine talep açın.
:::
"##,
        },
        en: SiteDoc {
            slug: "hosting-types-compared",
            title: "Hosting types compared",
            body: r##"Hosting types differ in how a website or application shares server resources: web hosting shares them, a VPS gives you dedicated virtual resources and a dedicated server reserves an entire physical machine for you. This guide helps you choose between {{site_name}} services.

:::compare Hosting types at a glance
| | Web hosting | WordPress hosting | VPS | Dedicated server |
|---|---|---|---|---|
| Best for | Company sites, blogs | WordPress sites | Developers, growing projects | High traffic, custom hardware |
| Resources | Shared | Shared, tuned for WordPress | Dedicated virtual | The whole physical server |
| Root access | ✗ | ✗ | ✓ | ✓ |
| Control panel | ✓ | ✓ | Optional | Optional |
| Technical skills | Not required | Not required | Required | Required |
| Scaling | Upgrade the plan | Upgrade the plan | Add resources | Change hardware |
:::

## What is web hosting?

Web hosting is the most economical type of hosting, where many sites share the resources of one server maintained by the provider. It is ideal for company websites and blogs.

## What is a VPS?

A VPS (virtual private server) is a virtualised slice of a physical server with processor, memory and disk resources reserved for you. Root access lets you install any software.

## What is a dedicated server?

A dedicated server is a type of hosting in which all resources of a physical server belong to one customer. It offers the highest performance and full control of the hardware.

:::faq Choosing
### When should I move from web hosting to a VPS?
When your site keeps hitting shared resource limits, needs custom software or you want root access, it is time for a VPS.

### Will my site go offline during a migration?
With a planned migration the interruption is limited to DNS propagation. Open a ticket and our team will help you plan it.
:::
"##,
        },
    },
    // ---------------------------------------------------------------- status
    SitePage {
        template: "default",
        nav: false,
        footer: true,
        tr: SiteDoc {
            slug: "durum",
            title: "Hizmet durumu",
            body: r##"Hizmet durumu sayfası, {{site_name}} altyapısındaki planlı bakım çalışmalarını ve olası kesintileri duyurduğumuz yerdir. Güncel bildirimler [duyurular]({{base}}/announcements) sayfasında yayınlanır; RSS ile de takip edebilirsiniz.

:::todo Durum sayfası bağlantısı
Harici bir durum izleme sayfanız varsa bağlantısını buraya ekleyin.
:::

## Bir sorun yaşıyorsam ne yapmalıyım?

Duyurularda bilinen bir kesinti yoksa müşteri panelinizden destek talebi açın; hizmet adını ve gördüğünüz hata mesajını yazın.
"##,
        },
        en: SiteDoc {
            slug: "status",
            title: "Service status",
            body: r##"The service status page is where {{site_name}} announces planned maintenance and incidents. Current notices are published under [announcements]({{base}}/announcements), which you can also follow via RSS.

:::todo Status page link
If you run an external status monitor, add its link here.
:::

## What should I do if something is not working?

If no incident is announced, open a ticket from your client area with the service name and the error you see.
"##,
        },
    },
    // -------------------------------------------------------------- landings
    SitePage {
        template: "landing",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "web-hosting",
            title: "Web hosting",
            body: r##"Web hosting, bir web sitesinin dosyalarını, veritabanlarını ve e-posta hesaplarını internete sürekli bağlı bir sunucuda barındıran hizmettir. {{site_name}} web hosting paketleri yönetim paneli, SSL sertifikası ve e-posta ile birlikte gelir; aylık veya yıllık ödenebilir.

:::features Her pakette neler var?
### :lock: Ücretsiz SSL
Siteniz HTTPS ile açılır; tarayıcılar güvenli bağlantı gösterir.

### :mail: Alan adınızla e-posta
ad@siteniz.com.tr biçiminde kurumsal e-posta adresleri oluşturun.

### :layers: Kolay kurulum
WordPress ve popüler uygulamaları yönetim panelinden birkaç tıklamayla kurun.

### :refresh: Yedekleme
Paket kapsamındaki yedeklerden dosyalarınızı geri yükleyebilirsiniz.
:::

:::steps Siteniz nasıl yayına çıkar?
### Paket ve alan adı seçin
Paketinizi seçin, mevcut alan adınızı kullanın ya da yenisini kaydedin.

### Kurulum otomatik tamamlanır
Ödemeden sonra hosting hesabınız açılır, giriş bilgileri panelinize gelir.

### Sitenizi yükleyin
Dosyalarınızı yükleyin veya WordPress'i kurun; siteniz yayında.
:::

:::faq Web hosting hakkında sık sorulanlar
### Web hosting nedir?
Web hosting, web sitenizin internetten erişilebilmesi için dosyalarını ve veritabanlarını sunucuda barındıran hizmettir. Sunucu bakımı ve güvenliği sağlayıcı tarafından yapılır.

### Kaç web sitesi barındırabilirim?
Her paketin site sayısı yukarıdaki karşılaştırma tablosunda yazar. Daha fazla siteye ihtiyacınız olursa paketinizi yükseltebilirsiniz.

### Mevcut sitemi taşıyabilir miyim?
Evet. Dosyalarınızı ve veritabanınızı yeni hesabınıza aktarabilirsiniz; yardıma ihtiyacınız olursa destek talebi açın.

### Web hosting mi, VPS mi?
Tek bir site veya blog için web hosting yeterlidir. Root erişimi ve ayrılmış kaynak gerekiyorsa [hosting türleri karşılaştırmasına]({{base}}/hosting-turleri) bakın.
:::
"##,
        },
        en: SiteDoc {
            slug: "web-hosting",
            title: "Web hosting",
            body: r##"Web hosting is a service that stores a website's files, databases and e-mail accounts on a server that is permanently connected to the internet. {{site_name}} web hosting plans come with a control panel, an SSL certificate and e-mail, billed monthly or yearly.

:::features What is in every plan?
### :lock: Free SSL
Your site loads over HTTPS and browsers show a secure connection.

### :mail: E-mail on your domain
Create addresses like name@yoursite.com.

### :layers: One-click apps
Install WordPress and other popular apps from the control panel.

### :refresh: Backups
Restore files from the backups included with your plan.
:::

:::steps How does my site go live?
### Choose a plan and a domain
Pick a plan and use your existing domain or register a new one.

### Setup completes automatically
After payment your hosting account is created and the login details appear in your client area.

### Upload your site
Upload your files or install WordPress, and you are live.
:::

:::faq Web hosting questions
### What is web hosting?
Web hosting keeps your website's files and databases on a server so the site is reachable on the internet. The provider maintains and secures the server.

### How many websites can I host?
The number of sites per plan is listed in the comparison table above. Upgrade when you need more.

### Can I move my existing site?
Yes. Transfer your files and database to the new account; open a ticket if you need help.

### Web hosting or a VPS?
Web hosting is enough for one site or blog. If you need root access and dedicated resources, read the [hosting types comparison]({{base}}/hosting-types-compared).
:::
"##,
        },
    },
    SitePage {
        template: "landing",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "wordpress-hosting",
            title: "WordPress hosting",
            body: r##"WordPress hosting, sunucu ayarları WordPress için hazırlanmış, WordPress'in hazır kurulu geldiği web hosting türüdür. {{site_name}} WordPress paketleriyle temayı ve eklentileri seçip doğrudan içeriğe odaklanabilirsiniz.

:::pricing wordpress-hosting WordPress paketleri
:::

:::features WordPress için hazır
### :zap: Hazır kurulum
WordPress hesabınızla birlikte kurulur; yönetici bilgileri panelinize gelir.

### :refresh: Güncellemeler
WordPress çekirdeğini, temaları ve eklentileri yönetim panelinden güncelleyin.

### :lock: SSL ve güvenlik
Ücretsiz SSL ve hesap güvenliği ayarları her pakette.

### :hard-drive: Yedekler
Paket kapsamındaki yedeklerden sitenizi geri yükleyin.
:::

:::faq WordPress hosting hakkında
### WordPress hosting ile web hosting arasındaki fark nedir?
İkisi de paylaşımlı barındırmadır; WordPress hosting'de sunucu ve yazılım ayarları WordPress'e göre hazırlanır ve WordPress kurulu gelir.

### Mevcut WordPress sitemi taşıyabilir miyim?
Evet. Bir taşıma eklentisiyle ya da dosya ve veritabanını aktararak sitenizi taşıyabilirsiniz.

### Eklenti kurmakta sınır var mı?
Hayır; ancak çok sayıda ağır eklenti performansı etkileyebilir. Yalnızca kullandığınız eklentileri tutun.
:::
"##,
        },
        en: SiteDoc {
            slug: "wordpress-hosting",
            title: "WordPress hosting",
            body: r##"WordPress hosting is web hosting with the server tuned for WordPress and WordPress installed for you. With {{site_name}} WordPress plans you pick a theme and plugins and focus on your content.

:::pricing wordpress-hosting WordPress plans
:::

:::features Ready for WordPress
### :zap: Pre-installed
WordPress is installed with your account; the admin login appears in your client area.

### :refresh: Updates
Update WordPress core, themes and plugins from the control panel.

### :lock: SSL and security
Free SSL and account security settings on every plan.

### :hard-drive: Backups
Restore your site from the backups included with the plan.
:::

:::faq WordPress hosting questions
### How is WordPress hosting different from web hosting?
Both are shared hosting; WordPress hosting has the server and software settings prepared for WordPress, which comes pre-installed.

### Can I move my existing WordPress site?
Yes, with a migration plugin or by transferring the files and database.

### Is there a limit on plugins?
No, but many heavy plugins can slow your site down. Keep only the plugins you use.
:::
"##,
        },
    },
    SitePage {
        template: "landing",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "vps",
            title: "VPS sunucu",
            body: r##"VPS (sanal özel sunucu), fiziksel bir sunucunun sanallaştırmayla bölünmüş ve işlemci, bellek ve disk kaynakları size ayrılmış bölümüdür. {{site_name}} VPS sunucularında root erişimiyle işletim sistemini ve yazılımı siz seçersiniz.

:::features VPS ile neler yapabilirsiniz?
### :terminal: Tam root erişimi
İstediğiniz yazılımı kurun, sunucuyu kendi ihtiyacınıza göre yapılandırın.

### :cpu: Ayrılmış kaynak
İşlemci, bellek ve disk kaynakları planınızda belirtildiği gibi size ayrılır.

### :refresh: Yeniden kurulum
İşletim sistemini müşteri panelinden yeniden kurun, sunucuyu açıp kapatın.

### :globe: Ayrı IP adresi
Her sunucu kendi IP adresiyle gelir.
:::

:::faq VPS hakkında sık sorulanlar
### VPS nedir?
VPS, paylaşımlı hosting ile dedicated sunucu arasındaki barındırma türüdür: kaynaklar size ayrılır, root erişimine sahip olursunuz ama fiziksel sunucu diğer sanal sunucularla paylaşılır.

### VPS kurmak için teknik bilgi gerekir mi?
Evet, temel Linux ve SSH bilgisi gerekir. Yönetim paneli istiyorsanız ek hizmet olarak seçebilirsiniz.

### Kaynakları sonradan artırabilir miyim?
Evet. Daha büyük bir plana geçerek işlemci, bellek ve disk kaynağını artırabilirsiniz.
:::
"##,
        },
        en: SiteDoc {
            slug: "vps",
            title: "VPS servers",
            body: r##"A VPS (virtual private server) is a virtualised slice of a physical server with processor, memory and disk resources reserved for you. On {{site_name}} VPS servers you choose the operating system and software, with full root access.

:::features What can you do with a VPS?
### :terminal: Full root access
Install any software and configure the server as you need.

### :cpu: Dedicated resources
CPU, memory and disk are reserved for you as stated in your plan.

### :refresh: Reinstall any time
Reinstall the operating system and power the server on or off from your client area.

### :globe: Its own IP address
Every server comes with its own IP address.
:::

:::faq VPS questions
### What is a VPS?
A VPS sits between shared hosting and a dedicated server: the resources are yours and you have root access, but the physical machine is shared with other virtual servers.

### Do I need technical skills?
Yes, basic Linux and SSH knowledge. If you want a control panel, add it as an option.

### Can I add resources later?
Yes. Move to a larger plan to get more CPU, memory and disk.
:::
"##,
        },
    },
    SitePage {
        template: "landing",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "dedicated",
            title: "Dedicated sunucu",
            body: r##"Dedicated sunucu, fiziksel bir sunucunun işlemci, bellek, disk ve ağ kaynaklarının tamamının tek bir müşteriye ayrıldığı barındırma türüdür. {{site_name}} dedicated sunucuları yüksek trafikli siteler, veritabanları ve yoğun iş yükleri için tasarlanmıştır.

:::features Neden dedicated?
### :cpu: Paylaşılmayan donanım
Tüm kaynaklar yalnızca size ait; başka müşterilerin iş yükü performansınızı etkilemez.

### :terminal: Tam kontrol
İşletim sistemini, RAID yapısını ve yazılımı ihtiyacınıza göre seçin.

### :shield: İzolasyon
Fiziksel olarak ayrılmış sunucu, sıkı güvenlik ve uyumluluk gereksinimleri için uygundur.
:::

:::faq Dedicated sunucu hakkında
### Dedicated sunucu ne zaman gerekir?
VPS kaynakları yetmediğinde, donanım düzeyinde izolasyon istediğinizde ya da özel donanım yapılandırmasına ihtiyaç duyduğunuzda.

### Kurulum ne kadar sürer?
Donanım hazırlığına bağlıdır; tahmini süre sipariş sırasında ve müşteri panelinde bildirilir.
:::
"##,
        },
        en: SiteDoc {
            slug: "dedicated",
            title: "Dedicated servers",
            body: r##"A dedicated server is a type of hosting in which all CPU, memory, disk and network resources of a physical server belong to one customer. {{site_name}} dedicated servers are built for high-traffic sites, databases and heavy workloads.

:::features Why dedicated?
### :cpu: Unshared hardware
Every resource is yours alone; other customers' workloads never affect your performance.

### :terminal: Full control
Choose the operating system, the RAID layout and the software you need.

### :shield: Isolation
A physically separate server suits strict security and compliance requirements.
:::

:::faq Dedicated server questions
### When do I need a dedicated server?
When a VPS is no longer enough, when you need hardware-level isolation or a custom hardware configuration.

### How long does setup take?
It depends on hardware preparation; the estimate is shown at order time and in your client area.
:::
"##,
        },
    },
    SitePage {
        template: "landing",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "ssl",
            title: "SSL sertifikası",
            body: r##"SSL sertifikası, web sitesi ile ziyaretçi arasındaki bağlantıyı şifreleyen ve sitenin kimliğini doğrulayan dijital bir sertifikadır. {{site_name}} üzerinden alan adı doğrulamalı (DV) ve kurumsal doğrulamalı sertifikaları sipariş edebilirsiniz.

:::pricing ssl SSL sertifikaları
:::

:::compare Sertifika türleri
| | Alan adı doğrulamalı (DV) | Kurumsal doğrulamalı (OV) |
|---|---|---|
| Doğrulanan | Alan adı sahipliği | Alan adı ve şirket bilgileri |
| Düzenlenme | Dakikalar içinde | Belge kontrolünden sonra |
| Uygun olduğu yer | Bloglar, tanıtım siteleri | Kurumsal siteler, e-ticaret |
:::

:::faq SSL hakkında
### SSL sertifikası neden gerekli?
Tarayıcılar SSL olmayan siteleri "güvenli değil" olarak işaretler. SSL, form ve ödeme bilgilerinin şifreli iletilmesini sağlar.

### Hosting paketimde SSL var mı?
Web hosting paketlerinde ücretsiz SSL bulunur. Kurumsal doğrulama veya garanti gerekiyorsa ücretli sertifika seçin.
:::
"##,
        },
        en: SiteDoc {
            slug: "ssl",
            title: "SSL certificates",
            body: r##"An SSL certificate is a digital certificate that encrypts the connection between a website and its visitors and proves the site's identity. Through {{site_name}} you can order domain-validated (DV) and organisation-validated certificates.

:::pricing ssl SSL certificates
:::

:::compare Certificate types
| | Domain validated (DV) | Organisation validated (OV) |
|---|---|---|
| Verifies | Domain ownership | Domain and company details |
| Issued | Within minutes | After document checks |
| Best for | Blogs, brochure sites | Company sites, online stores |
:::

:::faq SSL questions
### Why do I need an SSL certificate?
Browsers mark sites without SSL as "not secure". SSL encrypts form and payment data in transit.

### Does my hosting plan include SSL?
Web hosting plans include free SSL. Choose a paid certificate when you need organisation validation or a warranty.
:::
"##,
        },
    },
    SitePage {
        template: "landing",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "email-hosting",
            title: "Kurumsal e-posta",
            body: r##"Kurumsal e-posta, şirketinizin alan adıyla (ad@firmaniz.com.tr) çalışan, web sitenizden bağımsız barındırılabilen e-posta hizmetidir. {{site_name}} e-posta paketleri webmail, mobil uyum ve istenmeyen posta filtresiyle gelir.

:::pricing email-hosting E-posta paketleri
:::

:::features Profesyonel iletişim
### :mail: Alan adınızla adres
Müşterilerinize kendi alan adınızdan yazın.

### :smartphone: Her cihazda
Webmail, IMAP ve SMTP ile telefon ve bilgisayardan erişin.

### :shield: Spam koruması
İstenmeyen ve zararlı e-postalar filtrelenir.
:::

:::faq E-posta hakkında
### Mevcut e-postalarımı taşıyabilir miyim?
Evet. IMAP ile eski sunucudaki e-postalarınızı yeni hesabınıza aktarabilirsiniz.

### Hosting paketimdeki e-postadan farkı nedir?
E-posta paketleri yalnızca e-posta için ayrılmış kaynak ve depolama sunar; web sitenizin yükü e-postayı etkilemez.
:::
"##,
        },
        en: SiteDoc {
            slug: "email-hosting",
            title: "Business e-mail",
            body: r##"Business e-mail is an e-mail service on your company's own domain (name@yourcompany.com) that can be hosted separately from your website. {{site_name}} e-mail plans include webmail, mobile access and spam filtering.

:::pricing email-hosting E-mail plans
:::

:::features Professional communication
### :mail: Your domain
Write to customers from your own domain.

### :smartphone: On every device
Use webmail, IMAP and SMTP on your phone and computer.

### :shield: Spam protection
Unwanted and harmful messages are filtered.
:::

:::faq E-mail questions
### Can I move my existing mailboxes?
Yes. Import your messages from the old server over IMAP.

### How is it different from e-mail in my hosting plan?
E-mail plans have resources and storage reserved for e-mail only; your website's load never affects your mail.
:::
"##,
        },
    },
    SitePage {
        template: "landing",
        nav: false,
        footer: false,
        tr: SiteDoc {
            slug: "reseller-hosting",
            title: "Bayi hosting",
            body: r##"Bayi (reseller) hosting, hosting kaynaklarını kendi markanızla müşterilerinize ayrı hesaplar halinde satabildiğiniz barındırma türüdür. {{site_name}} bayi paketleri web ajansları ve serbest çalışanlar için uygundur.

:::pricing reseller-hosting Bayi paketleri
:::

:::features Ajanslar için
### :users: Müşteri hesapları
Her müşteriniz için ayrı hosting hesabı açın, kaynakları paylaştırın.

### :layout: Kendi markanız
Müşterileriniz hizmeti sizin markanızla kullanır.

### :gauge: Kaynak kontrolü
Disk ve trafik kullanımını tek panelden izleyin.
:::

:::faq Bayi hosting hakkında
### Bayi hosting kimler için uygundur?
Birden fazla müşterinin sitesini yöneten ajanslar, tasarımcılar ve kendi hosting hizmetini sunmak isteyenler için.

### Müşterilerim sizinle iletişime geçer mi?
Hayır; müşterileriniz sizinle çalışır. Siz de teknik konularda bizim destek ekibimize ulaşırsınız.
:::
"##,
        },
        en: SiteDoc {
            slug: "reseller-hosting",
            title: "Reseller hosting",
            body: r##"Reseller hosting is a type of hosting that lets you sell hosting resources to your own customers, as separate accounts under your brand. {{site_name}} reseller plans suit web agencies and freelancers.

:::pricing reseller-hosting Reseller plans
:::

:::features For agencies
### :users: Customer accounts
Create a separate hosting account for each client and share out the resources.

### :layout: Your brand
Your clients use the service under your brand.

### :gauge: Resource control
Track disk and traffic usage from one panel.
:::

:::faq Reseller hosting questions
### Who is reseller hosting for?
Agencies and designers who manage several clients' sites, and anyone who wants to offer hosting under their own name.

### Will my clients contact you?
No; your clients work with you, and you reach our support team for technical matters.
:::
"##,
        },
    },
];

async fn translation_group(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    page: &SitePage,
) -> DbResult<Uuid> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "select translation_group_id from pages
          where tenant_id = $1 and ((locale = 'tr' and slug = $2) or (locale = 'en' and slug = $3))
          order by locale desc limit 1",
    )
    .bind(tenant_id)
    .bind(page.tr.slug)
    .bind(page.en.slug)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(existing.unwrap_or_else(Uuid::now_v7))
}

/// Inserts the default pages that do not exist yet. `publish_landings`:
/// category landings go live (demo) instead of staying drafts.
pub(super) async fn seed_site_pages(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    publish_landings: bool,
    report: &mut SeedReport,
) -> DbResult<()> {
    for (order, page) in SITE_PAGES.iter().enumerate() {
        let group = translation_group(conn, tenant_id, page).await?;
        let status = if page.is_landing() && !publish_landings {
            "draft"
        } else {
            "published"
        };
        for (locale, doc) in [("tr", &page.tr), ("en", &page.en)] {
            let res = sqlx::query(
                "insert into pages (id, tenant_id, slug, locale, translation_group_id, title, body, template, status,
                                    published_at, show_in_nav, show_in_footer, nav_order)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                         case when $9 = 'published' then now() end, $10, $11, $12)
                 on conflict (tenant_id, locale, slug) do nothing",
            )
            .bind(Uuid::now_v7())
            .bind(tenant_id)
            .bind(doc.slug)
            .bind(locale)
            .bind(group)
            .bind(doc.title)
            .bind(doc.body)
            .bind(page.template)
            .bind(status)
            .bind(page.nav)
            .bind(page.footer)
            .bind(i32::try_from(order).unwrap_or(0) * 10)
            .execute(&mut *conn)
            .await?;
            report.add("pages", res.rows_affected());
        }
    }
    Ok(())
}

/// Demo upkeep: rows of the default pages that were never edited
/// (`updated_at = created_at`) get the current default title, body, template
/// and flags. Edited rows are left alone.
pub(super) async fn refresh_untouched(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    for page in SITE_PAGES {
        for (locale, doc) in [("tr", &page.tr), ("en", &page.en)] {
            let res = sqlx::query(
                "update pages set title = $4, body = $5, template = $6, show_in_nav = $7, show_in_footer = $8,
                                  status = 'published', published_at = coalesce(published_at, now()),
                                  created_at = now(), updated_at = now()
                  where tenant_id = $1 and locale = $2 and slug = $3
                    and updated_at = created_at and (body <> $5 or title <> $4 or template <> $6)",
            )
            .bind(tenant_id)
            .bind(locale)
            .bind(doc.slug)
            .bind(doc.title)
            .bind(doc.body)
            .bind(page.template)
            .bind(page.nav)
            .bind(page.footer)
            .execute(&mut *conn)
            .await?;
            report.add("pages", res.rows_affected());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_slug(s: &str) -> bool {
        !s.is_empty()
            && s.split('-').all(|p| {
                !p.is_empty()
                    && p.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
    }

    #[test]
    fn pages_are_complete_and_well_formed() {
        let mut slugs = std::collections::BTreeSet::new();
        for p in SITE_PAGES {
            for doc in [&p.tr, &p.en] {
                assert!(valid_slug(doc.slug), "{}", doc.slug);
                assert!(!doc.title.is_empty() && !doc.body.trim().is_empty());
                // Every block is closed.
                let opens = doc
                    .body
                    .lines()
                    .filter(|l| l.starts_with(":::") && l.trim().len() > 3)
                    .count();
                let closes = doc.body.lines().filter(|l| l.trim() == ":::").count();
                assert_eq!(opens, closes, "{}", doc.slug);
                // Links stay inside the tenant's site and its language.
                assert!(!doc.body.contains("](/"), "{}: use {{{{base}}}}", doc.slug);
                assert!(
                    !doc.body.contains("http"),
                    "{}: no external links",
                    doc.slug
                );
            }
            assert!(["default", "landing", "legal"].contains(&p.template));
            if p.template == "legal" {
                assert!(p.tr.body.starts_with(":::todo") && p.en.body.starts_with(":::todo"));
            }
            assert!(slugs.insert(("tr", p.tr.slug)) && slugs.insert(("en", p.en.slug)));
        }
        // No invented figures: no percentages or "99.9"-style claims.
        for p in SITE_PAGES {
            for doc in [&p.tr, &p.en] {
                assert!(!doc.body.contains('%'), "{}", doc.slug);
                assert!(!doc.body.contains("99."), "{}", doc.slug);
            }
        }
    }
}
