//! Seed content: system e-mail templates (tr + en, keys shared with
//! `hos-mail::defaults::keys`), knowledge-base articles and FAQs, an
//! announcement and legal / about pages.

use hos_core::ids::TenantId;
use sqlx::PgConnection;
use uuid::Uuid;

use super::SeedReport;
use crate::error::DbResult;

// ---------------------------------------------------------------------------
// E-mail templates
// ---------------------------------------------------------------------------

/// A template written as plain paragraphs (may contain `{{var}}` and
/// `{{#if}}` blocks) plus an optional call-to-action button; rendered to both
/// `body_html` (fragment, the mail layout wraps it) and `body_text`.
struct MailSeed {
    key: &'static str,
    locale: &'static str,
    category: &'static str,
    name: &'static str,
    subject: &'static str,
    before: &'static [&'static str],
    /// `(label, url variable path)`.
    button: Option<(&'static str, &'static str)>,
    after: &'static [&'static str],
}

impl MailSeed {
    fn html(&self) -> String {
        let mut out = Vec::new();
        out.extend(self.before.iter().map(|p| format!("<p>{p}</p>")));
        if let Some((label, url)) = self.button {
            out.push(format!(
                r#"<p><a class="hos-button" href="{{{{{url}}}}}">{label}</a></p>"#
            ));
        }
        out.extend(self.after.iter().map(|p| format!("<p>{p}</p>")));
        out.join("\n")
    }

    fn text(&self) -> String {
        let mut out: Vec<String> = self.before.iter().map(|p| (*p).to_owned()).collect();
        if let Some((label, url)) = self.button {
            out.push(format!("{label}: {{{{{url}}}}}"));
        }
        out.extend(self.after.iter().map(|p| (*p).to_owned()));
        out.join("\n\n")
    }
}

const MAILS: &[MailSeed] = &[
    // ---------------------------------------------------------------- welcome
    MailSeed {
        key: "welcome",
        locale: "tr",
        category: "client",
        name: "Hoş geldiniz",
        subject: "{{company.name}} ailesine hoş geldiniz",
        before: &[
            "Merhaba {{user.name}},",
            "{{company.name}} hesabınız {{user.email}} adresiyle oluşturuldu. Müşteri panelinizden hizmetlerinizi, faturalarınızı ve destek taleplerinizi tek yerden yönetebilirsiniz.",
        ],
        button: Some(("Müşteri paneline git", "action_url")),
        after: &["Herhangi bir sorunuz olursa bize her zaman ulaşabilirsiniz."],
    },
    MailSeed {
        key: "welcome",
        locale: "en",
        category: "client",
        name: "Welcome",
        subject: "Welcome to {{company.name}}",
        before: &[
            "Hi {{user.name}},",
            "Your {{company.name}} account has been created for {{user.email}}. From your client area you can manage your services, invoices and support tickets in one place.",
        ],
        button: Some(("Go to your client area", "action_url")),
        after: &["If you have any questions, we are always happy to help."],
    },
    // ----------------------------------------------------- email_verification
    MailSeed {
        key: "email_verification",
        locale: "tr",
        category: "auth",
        name: "E-posta doğrulama",
        subject: "E-posta adresinizi doğrulayın",
        before: &[
            "Merhaba {{user.name}},",
            "{{company.name}} hesabınızı etkinleştirmek için lütfen e-posta adresinizi doğrulayın.",
        ],
        button: Some(("E-posta adresimi doğrula", "action_url")),
        after: &[
            "Bu bağlantı {{expires_in_hours}} saat boyunca geçerlidir. Bu hesabı siz oluşturmadıysanız bu e-postayı dikkate almayabilirsiniz.",
        ],
    },
    MailSeed {
        key: "email_verification",
        locale: "en",
        category: "auth",
        name: "Email verification",
        subject: "Verify your email address",
        before: &[
            "Hi {{user.name}},",
            "Please verify your email address to activate your {{company.name}} account.",
        ],
        button: Some(("Verify my email address", "action_url")),
        after: &[
            "This link is valid for {{expires_in_hours}} hours. If you did not create this account, you can safely ignore this email.",
        ],
    },
    // --------------------------------------------------------- password_reset
    MailSeed {
        key: "password_reset",
        locale: "tr",
        category: "auth",
        name: "Parola sıfırlama",
        subject: "Parola sıfırlama talebiniz",
        before: &[
            "Merhaba {{user.name}},",
            "{{company.name}} hesabınız için bir parola sıfırlama talebi aldık. Yeni bir parola belirlemek için aşağıdaki bağlantıyı kullanın.",
        ],
        button: Some(("Yeni parola belirle", "action_url")),
        after: &[
            "Bu bağlantı {{expires_in_minutes}} dakika boyunca geçerlidir ve yalnızca bir kez kullanılabilir.{{#if request.ip}} Talep {{request.ip}} IP adresinden gönderildi.{{/if}}",
            "Bu talebi siz yapmadıysanız bu e-postayı dikkate almayın; parolanız değişmeyecektir.",
        ],
    },
    MailSeed {
        key: "password_reset",
        locale: "en",
        category: "auth",
        name: "Password reset",
        subject: "Your password reset request",
        before: &[
            "Hi {{user.name}},",
            "We received a request to reset the password of your {{company.name}} account. Use the link below to choose a new password.",
        ],
        button: Some(("Choose a new password", "action_url")),
        after: &[
            "This link is valid for {{expires_in_minutes}} minutes and can be used only once.{{#if request.ip}} The request was sent from IP address {{request.ip}}.{{/if}}",
            "If you did not request this, ignore this email; your password will not change.",
        ],
    },
    // -------------------------------------------------------- invoice_created
    MailSeed {
        key: "invoice_created",
        locale: "tr",
        category: "invoice",
        name: "Fatura oluşturuldu",
        subject: "{{invoice.number}} numaralı faturanız oluşturuldu",
        before: &[
            "Merhaba {{user.name}},",
            "{{invoice.number}} numaralı, {{invoice.total}} tutarındaki faturanız oluşturuldu. Son ödeme tarihi: {{invoice.due_date}}.",
            "{{#if invoice.notes}}{{invoice.notes}}{{/if}}",
        ],
        button: Some(("Faturayı görüntüle ve öde", "invoice.url")),
        after: &[
            "Ödemenizi zamanında yaparak hizmetlerinizin kesintisiz devam etmesini sağlayabilirsiniz.",
        ],
    },
    MailSeed {
        key: "invoice_created",
        locale: "en",
        category: "invoice",
        name: "Invoice created",
        subject: "Invoice {{invoice.number}} has been issued",
        before: &[
            "Hi {{user.name}},",
            "Invoice {{invoice.number}} for {{invoice.total}} has been issued. Due date: {{invoice.due_date}}.",
            "{{#if invoice.notes}}{{invoice.notes}}{{/if}}",
        ],
        button: Some(("View and pay invoice", "invoice.url")),
        after: &["Paying on time keeps your services running without interruption."],
    },
    // ----------------------------------------------------------- invoice_paid
    MailSeed {
        key: "invoice_paid",
        locale: "tr",
        category: "invoice",
        name: "Ödeme alındı",
        subject: "Ödemeniz alındı — {{invoice.number}}",
        before: &[
            "Merhaba {{user.name}},",
            "{{invoice.number}} numaralı faturanız için {{invoice.total}} tutarındaki ödemeniz {{invoice.paid_at}} tarihinde alındı{{#if payment.method}} ({{payment.method}}){{/if}}. Teşekkür ederiz!",
        ],
        button: Some(("Faturayı görüntüle", "invoice.url")),
        after: &[],
    },
    MailSeed {
        key: "invoice_paid",
        locale: "en",
        category: "invoice",
        name: "Payment received",
        subject: "Payment received — {{invoice.number}}",
        before: &[
            "Hi {{user.name}},",
            "We received your payment of {{invoice.total}} for invoice {{invoice.number}} on {{invoice.paid_at}}{{#if payment.method}} ({{payment.method}}){{/if}}. Thank you!",
        ],
        button: Some(("View invoice", "invoice.url")),
        after: &[],
    },
    // -------------------------------------------------------- invoice_overdue
    MailSeed {
        key: "invoice_overdue",
        locale: "tr",
        category: "invoice",
        name: "Gecikmiş fatura",
        subject: "Ödenmemiş fatura hatırlatması — {{invoice.number}}",
        before: &[
            "Merhaba {{user.name}},",
            "{{invoice.due_date}} vadeli {{invoice.number}} numaralı, {{invoice.total}} tutarındaki faturanız {{invoice.days_overdue}} gündür ödenmemiş görünüyor.",
            "{{#if invoice.suspension_date}}Ödeme yapılmazsa ilgili hizmetler {{invoice.suspension_date}} tarihinde askıya alınacaktır.{{/if}}",
        ],
        button: Some(("Şimdi öde", "invoice.url")),
        after: &["Ödemenizi yeni yaptıysanız bu e-postayı dikkate almayınız."],
    },
    MailSeed {
        key: "invoice_overdue",
        locale: "en",
        category: "invoice",
        name: "Invoice overdue",
        subject: "Overdue invoice reminder — {{invoice.number}}",
        before: &[
            "Hi {{user.name}},",
            "Invoice {{invoice.number}} for {{invoice.total}}, due on {{invoice.due_date}}, is {{invoice.days_overdue}} days overdue.",
            "{{#if invoice.suspension_date}}If it remains unpaid, the related services will be suspended on {{invoice.suspension_date}}.{{/if}}",
        ],
        button: Some(("Pay now", "invoice.url")),
        after: &["If you have just paid, please disregard this email."],
    },
    // ------------------------------------------------------ service_activated
    MailSeed {
        key: "service_activated",
        locale: "tr",
        category: "product",
        name: "Hizmet aktif",
        subject: "{{service.name}} hizmetiniz aktif",
        before: &[
            "Merhaba {{user.name}},",
            "{{service.name}}{{#if service.domain}} ({{service.domain}}){{/if}} hizmetiniz kuruldu ve kullanıma hazır.",
            "{{#if service.username}}Kullanıcı adınız: {{service.username}}. Giriş bilgilerinizi müşteri panelinizden görüntüleyebilirsiniz.{{/if}}",
        ],
        button: Some(("Hizmeti yönet", "action_url")),
        after: &["Kurulumla ilgili yardıma ihtiyacınız olursa destek ekibimiz yanınızda."],
    },
    MailSeed {
        key: "service_activated",
        locale: "en",
        category: "product",
        name: "Service active",
        subject: "Your {{service.name}} service is active",
        before: &[
            "Hi {{user.name}},",
            "Your {{service.name}}{{#if service.domain}} ({{service.domain}}){{/if}} service has been provisioned and is ready to use.",
            "{{#if service.username}}Your username is {{service.username}}. Login details are available in your client area.{{/if}}",
        ],
        button: Some(("Manage service", "action_url")),
        after: &["If you need help getting started, our support team is here for you."],
    },
    // ------------------------------------------------------ service_suspended
    MailSeed {
        key: "service_suspended",
        locale: "tr",
        category: "product",
        name: "Hizmet askıya alındı",
        subject: "{{service.name}} hizmetiniz askıya alındı",
        before: &[
            "Merhaba {{user.name}},",
            "{{service.name}}{{#if service.domain}} ({{service.domain}}){{/if}} hizmetiniz askıya alınmıştır.{{#if service.reason}} Sebep: {{service.reason}}.{{/if}}",
            "Ödenmemiş faturanız varsa ödeme yapıldığında hizmetiniz otomatik olarak yeniden etkinleştirilir.",
        ],
        button: Some(("Faturalarımı görüntüle", "action_url")),
        after: &[],
    },
    MailSeed {
        key: "service_suspended",
        locale: "en",
        category: "product",
        name: "Service suspended",
        subject: "Your {{service.name}} service has been suspended",
        before: &[
            "Hi {{user.name}},",
            "Your {{service.name}}{{#if service.domain}} ({{service.domain}}){{/if}} service has been suspended.{{#if service.reason}} Reason: {{service.reason}}.{{/if}}",
            "If an invoice is outstanding, the service is reactivated automatically once it is paid.",
        ],
        button: Some(("View my invoices", "action_url")),
        after: &[],
    },
    // ----------------------------------------------------------- ticket_reply
    MailSeed {
        key: "ticket_reply",
        locale: "tr",
        category: "support",
        name: "Destek talebi yanıtı",
        subject: "[#{{ticket.number}}] {{ticket.subject}}",
        before: &[
            "Merhaba {{user.name}},",
            "#{{ticket.number}} numaralı destek talebiniz {{ticket.staff_name}} tarafından yanıtlandı:",
            "{{ticket.reply_excerpt}}",
        ],
        button: Some(("Talebi görüntüle ve yanıtla", "ticket.url")),
        after: &["Bu e-postayı yanıtlayarak da talebinize mesaj ekleyebilirsiniz."],
    },
    MailSeed {
        key: "ticket_reply",
        locale: "en",
        category: "support",
        name: "Ticket reply",
        subject: "[#{{ticket.number}}] {{ticket.subject}}",
        before: &[
            "Hi {{user.name}},",
            "{{ticket.staff_name}} replied to your ticket #{{ticket.number}}:",
            "{{ticket.reply_excerpt}}",
        ],
        button: Some(("View and reply", "ticket.url")),
        after: &["You can also reply to this email to add a message to your ticket."],
    },
    // -------------------------------------------------------- domain_expiring
    MailSeed {
        key: "domain_expiring",
        locale: "tr",
        category: "domain",
        name: "Alan adı süresi doluyor",
        subject: "{{domain.name}} alan adınızın süresi {{domain.days_left}} gün içinde doluyor",
        before: &[
            "Merhaba {{user.name}},",
            "{{domain.name}} alan adınızın kayıt süresi {{domain.expires_on}} tarihinde sona eriyor.",
            "{{#if domain.auto_renew}}Otomatik yenileme açık; yenileme faturası vadesinden önce oluşturulacaktır.{{else}}Alan adınızı kaybetmemek için süresi dolmadan yenilemenizi öneririz.{{/if}}",
        ],
        button: Some(("Alan adını yenile", "domain.renew_url")),
        after: &[],
    },
    MailSeed {
        key: "domain_expiring",
        locale: "en",
        category: "domain",
        name: "Domain expiring",
        subject: "{{domain.name}} expires in {{domain.days_left}} days",
        before: &[
            "Hi {{user.name}},",
            "The registration of {{domain.name}} expires on {{domain.expires_on}}.",
            "{{#if domain.auto_renew}}Auto-renew is on; a renewal invoice will be issued before the expiry date.{{else}}Renew it before it expires so you do not lose the domain.{{/if}}",
        ],
        button: Some(("Renew domain", "domain.renew_url")),
        after: &[],
    },
];

/// Inserts the system e-mail templates the tenant does not have yet (never
/// overwrites a customised template).
pub(super) async fn seed_email_templates(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    let ids: Vec<Uuid> = MAILS.iter().map(|_| Uuid::now_v7()).collect();
    let keys: Vec<&str> = MAILS.iter().map(|m| m.key).collect();
    let locales: Vec<&str> = MAILS.iter().map(|m| m.locale).collect();
    let categories: Vec<&str> = MAILS.iter().map(|m| m.category).collect();
    let names: Vec<&str> = MAILS.iter().map(|m| m.name).collect();
    let subjects: Vec<&str> = MAILS.iter().map(|m| m.subject).collect();
    let html: Vec<String> = MAILS.iter().map(MailSeed::html).collect();
    let text: Vec<String> = MAILS.iter().map(MailSeed::text).collect();
    let res = sqlx::query(
        "insert into email_templates (id, tenant_id, key, locale, category, name, subject, body_html, body_text, is_system)
         select u.id, $1, u.key, u.locale, u.category, u.name, u.subject, u.html, u.text, true
         from unnest($2::uuid[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[])
              as u(id, key, locale, category, name, subject, html, text)
         on conflict (tenant_id, key, locale) do nothing",
    )
    .bind(tenant_id)
    .bind(&ids)
    .bind(&keys)
    .bind(&locales)
    .bind(&categories)
    .bind(&names)
    .bind(&subjects)
    .bind(&html)
    .bind(&text)
    .execute(&mut *conn)
    .await?;
    report.add("email_templates", res.rows_affected());
    Ok(())
}

// ---------------------------------------------------------------------------
// Knowledge base, announcements, pages
// ---------------------------------------------------------------------------

/// One localized document.
struct Doc {
    slug: &'static str,
    title: &'static str,
    excerpt: &'static str,
    body: &'static str,
}

/// A tr / en pair sharing one `translation_group_id`.
struct Pair {
    tr: Doc,
    en: Doc,
}

struct KbCategorySeed {
    slug: &'static str,
    name: &'static str,
    name_en: &'static str,
    description: &'static str,
    description_en: &'static str,
    icon: &'static str,
}

const KB_CATEGORIES: &[KbCategorySeed] = &[
    KbCategorySeed {
        slug: "baslangic",
        name: "Başlarken",
        name_en: "Getting Started",
        description: "Hesabınızı ve müşteri panelini kullanmaya başlayın",
        description_en: "Get started with your account and the client area",
        icon: "rocket",
    },
    KbCategorySeed {
        slug: "alan-adlari",
        name: "Alan Adları",
        name_en: "Domains",
        description: "Kayıt, transfer, DNS ve nameserver işlemleri",
        description_en: "Registration, transfers, DNS and nameservers",
        icon: "globe",
    },
    KbCategorySeed {
        slug: "hosting",
        name: "Web Hosting",
        name_en: "Web Hosting",
        description: "Kontrol paneli, e-posta ve web sitesi kurulumları",
        description_en: "Control panel, email and website setup",
        icon: "layout",
    },
    KbCategorySeed {
        slug: "vps",
        name: "VPS ve Sunucular",
        name_en: "VPS & Servers",
        description: "Sanal ve dedicated sunucu yönetimi",
        description_en: "Managing virtual and dedicated servers",
        icon: "server",
    },
    KbCategorySeed {
        slug: "faturalama",
        name: "Faturalama",
        name_en: "Billing",
        description: "Faturalar, ödeme yöntemleri ve iadeler",
        description_en: "Invoices, payment methods and refunds",
        icon: "credit-card",
    },
    KbCategorySeed {
        slug: "sss",
        name: "Sıkça Sorulan Sorular",
        name_en: "FAQ",
        description: "En çok merak edilen sorular",
        description_en: "Frequently asked questions",
        icon: "help-circle",
    },
];

/// `(category slug, pair)`.
const KB_ARTICLES: &[(&str, Pair)] = &[
    (
        "baslangic",
        Pair {
            tr: Doc {
                slug: "musteri-paneline-giris",
                title: "Müşteri paneline nasıl giriş yaparım?",
                excerpt: "E-posta ve parola, Google / GitHub hesabı veya iki adımlı doğrulama ile giriş.",
                body: "## Giriş yöntemleri\n\nMüşteri paneline sitemizin sağ üst köşesindeki **Giriş** bağlantısından ulaşabilirsiniz.\n\n- **E-posta ve parola:** Kayıt olurken kullandığınız e-posta adresi ve parolanızla giriş yapın.\n- **Google veya GitHub:** Sosyal giriş düğmeleriyle tek tıkla oturum açın.\n\n## İki adımlı doğrulama\n\nHesabınızı korumak için *Hesabım → Güvenlik* bölümünden iki adımlı doğrulamayı (TOTP) etkinleştirmenizi öneririz.\n\n## Parolamı unuttum\n\nGiriş sayfasındaki **Parolamı unuttum** bağlantısını kullanın; sıfırlama bağlantısı e-posta adresinize gönderilir ve 60 dakika geçerlidir.",
            },
            en: Doc {
                slug: "logging-in-to-client-area",
                title: "How do I log in to the client area?",
                excerpt: "Sign in with email and password, a Google / GitHub account, or two-factor authentication.",
                body: "## Sign-in options\n\nOpen the client area from the **Log in** link in the top right corner of our website.\n\n- **Email and password:** use the address and password you registered with.\n- **Google or GitHub:** sign in with one click using the social login buttons.\n\n## Two-factor authentication\n\nWe recommend enabling two-factor authentication (TOTP) under *My account → Security*.\n\n## Forgot your password?\n\nUse the **Forgot password** link on the login page; a reset link valid for 60 minutes is sent to your email address.",
            },
        },
    ),
    (
        "alan-adlari",
        Pair {
            tr: Doc {
                slug: "nameserver-degistirme",
                title: "Alan adımın nameserver (DNS) adreslerini nasıl değiştiririm?",
                excerpt: "Nameserver değişikliği müşteri panelinden birkaç adımda yapılır.",
                body: "1. Müşteri panelinde **Alan Adlarım** sayfasını açın.\n2. İlgili alan adının yanındaki **Yönet** düğmesine tıklayın.\n3. **Nameserver'lar** sekmesinde yeni adresleri girin ve kaydedin.\n\nVarsayılan nameserver adreslerimiz: `ns1.demo.localhost` ve `ns2.demo.localhost`.\n\n> Değişikliğin tüm dünyada geçerli olması 24-48 saat sürebilir. `.tr` uzantılı alan adlarında nameserver'ların önceden yapılandırılmış olması gerekir.",
            },
            en: Doc {
                slug: "changing-nameservers",
                title: "How do I change my domain's nameservers?",
                excerpt: "Nameservers can be changed from the client area in a few steps.",
                body: "1. Open **My Domains** in the client area.\n2. Click **Manage** next to the domain.\n3. Enter the new addresses in the **Nameservers** tab and save.\n\nOur default nameservers are `ns1.demo.localhost` and `ns2.demo.localhost`.\n\n> Propagation can take 24-48 hours. For `.tr` domains the nameservers must already be configured for the zone.",
            },
        },
    ),
    (
        "hosting",
        Pair {
            tr: Doc {
                slug: "wordpress-kurulumu",
                title: "WordPress'i tek tıkla nasıl kurarım?",
                excerpt: "Kontrol panelindeki uygulama yükleyici ile WordPress birkaç dakikada kurulur.",
                body: "## Kurulum\n\n1. Müşteri panelinden hosting hizmetinizi açın ve **Kontrol Paneline Git** düğmesine tıklayın.\n2. **WordPress** (veya *Softaculous / Uygulama Yükleyici*) bölümünü açın.\n3. Alan adınızı seçin, yönetici kullanıcı adı ve güçlü bir parola belirleyin.\n4. **Kur** düğmesine tıklayın.\n\n## Sonrası\n\n- Ücretsiz SSL sertifikanız otomatik olarak kurulur; sitenizi `https://` ile açın.\n- Pro ve Business paketlerde LiteSpeed Cache eklentisini etkinleştirerek hızınızı artırabilirsiniz.",
            },
            en: Doc {
                slug: "installing-wordpress",
                title: "How do I install WordPress with one click?",
                excerpt: "The control panel's app installer sets up WordPress in minutes.",
                body: "## Installation\n\n1. Open your hosting service in the client area and click **Go to control panel**.\n2. Open **WordPress** (or *Softaculous / App Installer*).\n3. Choose your domain and set an admin username and a strong password.\n4. Click **Install**.\n\n## Afterwards\n\n- Your free SSL certificate is installed automatically; open your site with `https://`.\n- On Pro and Business plans enable the LiteSpeed Cache plugin for extra speed.",
            },
        },
    ),
    (
        "vps",
        Pair {
            tr: Doc {
                slug: "vps-ssh-baglantisi",
                title: "VPS sunucuma SSH ile nasıl bağlanırım?",
                excerpt: "Sunucu IP adresi ve root parolası ya da SSH anahtarınızla bağlanın.",
                body: "Sunucunuzun IP adresi ve ilk giriş bilgileri **Hizmetlerim → VPS** sayfasında yer alır.\n\n```bash\nssh root@SUNUCU_IP_ADRESI\n```\n\n## Güvenlik önerileri\n\n- Parola yerine SSH anahtarı kullanın ve `PasswordAuthentication no` ayarlayın.\n- Sistem paketlerini düzenli olarak güncelleyin.\n- Yalnızca ihtiyaç duyduğunuz portları güvenlik duvarında açın.\n\nSunucuya erişiminizi kaybederseniz panelden **Konsol** ile tarayıcı üzerinden bağlanabilirsiniz.",
            },
            en: Doc {
                slug: "connecting-to-vps-via-ssh",
                title: "How do I connect to my VPS via SSH?",
                excerpt: "Connect with the server IP and root password or your SSH key.",
                body: "Your server's IP address and initial login details are shown under **My Services → VPS**.\n\n```bash\nssh root@SERVER_IP_ADDRESS\n```\n\n## Security tips\n\n- Use SSH keys instead of passwords and set `PasswordAuthentication no`.\n- Keep system packages up to date.\n- Open only the ports you need in the firewall.\n\nIf you lose access, use the **Console** in the client area to connect from your browser.",
            },
        },
    ),
    (
        "faturalama",
        Pair {
            tr: Doc {
                slug: "havale-eft-ile-odeme",
                title: "Havale / EFT ile nasıl ödeme yaparım?",
                excerpt: "Banka bilgilerimiz fatura sayfasında; açıklamaya fatura numarasını yazmayı unutmayın.",
                body: "1. Müşteri panelinden ödenecek faturayı açın ve ödeme yöntemi olarak **Havale / EFT**'yi seçin.\n2. Görüntülenen IBAN'a fatura tutarını gönderin.\n3. Açıklama alanına **fatura numaranızı** yazın.\n\nÖdemeniz banka hesabımıza ulaştıktan sonra en geç 1 iş günü içinde onaylanır ve hizmetiniz otomatik olarak etkinleştirilir. Hızlı onay için kredi kartı ile ödeme yapabilirsiniz.",
            },
            en: Doc {
                slug: "paying-by-bank-transfer",
                title: "How do I pay by bank transfer?",
                excerpt: "Our bank details are on the invoice page; include the invoice number in the reference.",
                body: "1. Open the invoice in the client area and choose **Bank transfer** as the payment method.\n2. Transfer the invoice amount to the IBAN shown.\n3. Put your **invoice number** in the payment reference.\n\nPayments are confirmed within 1 business day of reaching our account and your service is activated automatically. Pay by card for instant confirmation.",
            },
        },
    ),
];

const FAQS: &[Pair] = &[
    Pair {
        tr: Doc {
            slug: "ucretsiz-ssl-var-mi",
            title: "Hosting paketlerinde SSL sertifikası ücretsiz mi?",
            excerpt: "",
            body: "Evet. Tüm web hosting paketlerinde Let's Encrypt SSL sertifikası ücretsizdir ve otomatik olarak yenilenir.",
        },
        en: Doc {
            slug: "is-ssl-free",
            title: "Is an SSL certificate included with hosting?",
            excerpt: "",
            body: "Yes. Every web hosting plan includes a free Let's Encrypt certificate that renews automatically.",
        },
    },
    Pair {
        tr: Doc {
            slug: "iade-politikasi",
            title: "Para iade garantiniz var mı?",
            excerpt: "",
            body: "Web hosting paketlerinde ilk 30 gün içinde koşulsuz iade garantisi sunuyoruz. Alan adı kayıtları ve kurulum ücretleri iade kapsamında değildir.",
        },
        en: Doc {
            slug: "refund-policy",
            title: "Do you offer a money-back guarantee?",
            excerpt: "",
            body: "Web hosting plans come with a 30-day money-back guarantee. Domain registrations and setup fees are non-refundable.",
        },
    },
    Pair {
        tr: Doc {
            slug: "alan-adi-transferi-ne-kadar-surer",
            title: "Alan adı transferi ne kadar sürer?",
            excerpt: "",
            body: "Uluslararası uzantılarda (.com, .net vb.) transfer genellikle 5-7 gün sürer. Transfer için alan adının kilidinin açık olması ve geçerli bir EPP (transfer) kodu gerekir.",
        },
        en: Doc {
            slug: "how-long-does-domain-transfer-take",
            title: "How long does a domain transfer take?",
            excerpt: "",
            body: "Transfers of generic domains (.com, .net, ...) usually complete within 5-7 days. The domain must be unlocked and you need a valid EPP (auth) code.",
        },
    },
    Pair {
        tr: Doc {
            slug: "paket-yukseltme",
            title: "Paketimi daha sonra yükseltebilir miyim?",
            excerpt: "",
            body: "Evet. Müşteri panelinden istediğiniz zaman üst pakete geçebilirsiniz; yalnızca kalan süreye ait fiyat farkını ödersiniz.",
        },
        en: Doc {
            slug: "upgrading-plan",
            title: "Can I upgrade my plan later?",
            excerpt: "",
            body: "Yes. Upgrade any time from the client area; you only pay the prorated difference for the remaining period.",
        },
    },
    Pair {
        tr: Doc {
            slug: "yedekleme-sikligi",
            title: "Verilerim ne sıklıkla yedekleniyor?",
            excerpt: "",
            body: "Başlangıç paketinde haftalık, Pro ve Business paketlerinde günlük yedek alınır. VPS sunucular için Günlük Yedekleme eklentisini satın alabilirsiniz.",
        },
        en: Doc {
            slug: "backup-frequency",
            title: "How often is my data backed up?",
            excerpt: "",
            body: "Starter plans are backed up weekly, Pro and Business plans daily. For VPS servers you can order the Daily Backups addon.",
        },
    },
];

const ANNOUNCEMENT: Pair = Pair {
    tr: Doc {
        slug: "yeni-nvme-altyapisi",
        title: "Tüm hosting sunucularımız NVMe altyapısına taşındı",
        excerpt: "Web hosting müşterilerimiz ek ücret ödemeden 3 kata kadar daha hızlı disk performansına sahip.",
        body: "Tüm paylaşımlı hosting sunucularımızı NVMe disklere ve LiteSpeed web sunucusuna taşıdık. Taşıma kesintisiz tamamlandı; sizin herhangi bir işlem yapmanıza gerek yok.\n\nYeni altyapı ile sayfa yükleme süreleri ortalama %40 kısaldı.",
    },
    en: Doc {
        slug: "new-nvme-infrastructure",
        title: "All hosting servers have moved to NVMe",
        excerpt: "Web hosting customers get up to 3× faster storage at no extra cost.",
        body: "We have moved all shared hosting servers to NVMe storage and the LiteSpeed web server. The migration finished without downtime; no action is needed on your side.\n\nPage load times dropped by 40% on average.",
    },
};

/// `(pair, show_in_nav)`.
const PAGES: &[(Pair, bool)] = &[
    (
        Pair {
            tr: Doc {
                slug: "hakkimizda",
                title: "Hakkımızda",
                excerpt: "",
                body: "## Biz kimiz?\n\nDemo Hosting, 2015'ten bu yana bireysel kullanıcılara ve işletmelere güvenilir web hosting, VPS ve alan adı hizmetleri sunuyor.\n\n- Türkiye ve Avrupa'da veri merkezleri\n- 7/24 Türkçe destek\n- %99,9 çalışma süresi garantisi",
            },
            en: Doc {
                slug: "about-us",
                title: "About us",
                excerpt: "",
                body: "## Who we are\n\nSince 2015 Demo Hosting has provided reliable web hosting, VPS and domain services to individuals and businesses.\n\n- Data centers in Turkey and Europe\n- 24/7 support\n- 99.9% uptime guarantee",
            },
        },
        true,
    ),
    (
        Pair {
            tr: Doc {
                slug: "gizlilik-politikasi",
                title: "Gizlilik Politikası ve KVKK Aydınlatma Metni",
                excerpt: "",
                body: "Bu metin, 6698 sayılı Kişisel Verilerin Korunması Kanunu kapsamında kişisel verilerinizin hangi amaçlarla işlendiğini açıklar.\n\n## İşlenen veriler\n\nKimlik, iletişim, fatura ve işlem güvenliği verileri.\n\n## Amaçlar\n\nHizmetlerin sunulması, faturalandırma, yasal yükümlülüklerin yerine getirilmesi ve destek taleplerinin yanıtlanması.\n\n## Haklarınız\n\nKVKK'nın 11. maddesi kapsamındaki taleplerinizi destek@demo.localhost adresine iletebilirsiniz.",
            },
            en: Doc {
                slug: "privacy-policy",
                title: "Privacy Policy",
                excerpt: "",
                body: "This policy explains how we process your personal data in line with Turkish data protection law (KVKK) and the GDPR.\n\n## Data we process\n\nIdentity, contact, billing and security data.\n\n## Purposes\n\nProviding services, billing, meeting legal obligations and answering support requests.\n\n## Your rights\n\nSend requests regarding your data to destek@demo.localhost.",
            },
        },
        false,
    ),
    (
        Pair {
            tr: Doc {
                slug: "kullanim-kosullari",
                title: "Hizmet Sözleşmesi ve Kullanım Koşulları",
                excerpt: "",
                body: "Hizmetlerimizi kullanarak aşağıdaki koşulları kabul etmiş olursunuz.\n\n1. Hizmetler, ödemesi yapılan dönem boyunca sunulur ve dönem sonunda otomatik olarak yenilenir.\n2. Vadesi geçen faturalar nedeniyle hizmetler askıya alınabilir.\n3. Spam, kimlik avı ve yasa dışı içerik barındırmak yasaktır.\n4. Uyuşmazlıklarda İstanbul mahkemeleri yetkilidir.",
            },
            en: Doc {
                slug: "terms-of-service",
                title: "Terms of Service",
                excerpt: "",
                body: "By using our services you agree to the following terms.\n\n1. Services are provided for the paid period and renew automatically at the end of it.\n2. Services may be suspended for overdue invoices.\n3. Spam, phishing and illegal content are prohibited.\n4. Istanbul courts have jurisdiction over disputes.",
            },
        },
        false,
    ),
];

/// `translation_group_id` of the Turkish row with `slug_tr` (or a new id).
async fn translation_group(
    conn: &mut PgConnection,
    table: &'static str,
    tenant_id: TenantId,
    slug_tr: &str,
) -> DbResult<Uuid> {
    let sql = format!(
        "select translation_group_id from {table} where tenant_id = $1 and locale = 'tr' and slug = $2"
    );
    let existing: Option<Uuid> = sqlx::query_scalar(&sql)
        .bind(tenant_id)
        .bind(slug_tr)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(existing.unwrap_or_else(Uuid::now_v7))
}

fn none_if_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

pub(super) async fn seed_content(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    report: &mut SeedReport,
) -> DbResult<()> {
    // KB categories.
    let mut categories: Vec<(&str, Uuid)> = Vec::new();
    for (i, c) in KB_CATEGORIES.iter().enumerate() {
        let (id, created): (Uuid, bool) = sqlx::query_as(
            "with ins as (
                 insert into kb_categories (id, tenant_id, slug, name, description, translations, icon, sort_order)
                 values ($1, $2, $3, $4, $5, $6, $7, $8)
                 on conflict (tenant_id, slug) do nothing returning id
             )
             select id, true from ins union all
             select id, false from kb_categories where tenant_id = $2 and slug = $3 limit 1",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(c.slug)
        .bind(c.name)
        .bind(c.description)
        .bind(serde_json::json!({"en": {"name": c.name_en, "description": c.description_en}}))
        .bind(c.icon)
        .bind(i32::try_from(i).unwrap_or(0) * 10)
        .fetch_one(&mut *conn)
        .await?;
        report.add("kb_categories", u64::from(created));
        categories.push((c.slug, id));
    }
    let category = |slug: &str| {
        categories
            .iter()
            .find(|(s, _)| *s == slug)
            .map(|(_, id)| *id)
    };

    // KB articles and FAQs.
    let articles = KB_ARTICLES
        .iter()
        .map(|(cat, pair)| (*cat, "article", pair))
        .chain(FAQS.iter().map(|pair| ("sss", "faq", pair)));
    for (sort, (cat, kind, pair)) in articles.enumerate() {
        let group = translation_group(conn, "kb_articles", tenant_id, pair.tr.slug).await?;
        for (locale, doc) in [("tr", &pair.tr), ("en", &pair.en)] {
            let res = sqlx::query(
                "insert into kb_articles (id, tenant_id, category_id, kind, slug, locale, translation_group_id, title,
                                          excerpt, body, status, published_at, is_featured, sort_order)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'published', now(), $11, $12)
                 on conflict (tenant_id, locale, slug) do nothing",
            )
            .bind(Uuid::now_v7())
            .bind(tenant_id)
            .bind(category(cat))
            .bind(kind)
            .bind(doc.slug)
            .bind(locale)
            .bind(group)
            .bind(doc.title)
            .bind(none_if_empty(doc.excerpt))
            .bind(doc.body)
            .bind(kind == "article" && sort < 2)
            .bind(i32::try_from(sort).unwrap_or(0) * 10)
            .execute(&mut *conn)
            .await?;
            report.add("kb_articles", res.rows_affected());
        }
    }

    // Announcement.
    let group = translation_group(conn, "announcements", tenant_id, ANNOUNCEMENT.tr.slug).await?;
    for (locale, doc) in [("tr", &ANNOUNCEMENT.tr), ("en", &ANNOUNCEMENT.en)] {
        let res = sqlx::query(
            "insert into announcements (id, tenant_id, slug, locale, translation_group_id, title, excerpt, body,
                                        status, published_at, is_pinned)
             values ($1, $2, $3, $4, $5, $6, $7, $8, 'published', now(), true)
             on conflict (tenant_id, locale, slug) do nothing",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(doc.slug)
        .bind(locale)
        .bind(group)
        .bind(doc.title)
        .bind(none_if_empty(doc.excerpt))
        .bind(doc.body)
        .execute(&mut *conn)
        .await?;
        report.add("announcements", res.rows_affected());
    }

    // Pages.
    for (order, (pair, in_nav)) in PAGES.iter().enumerate() {
        let group = translation_group(conn, "pages", tenant_id, pair.tr.slug).await?;
        for (locale, doc) in [("tr", &pair.tr), ("en", &pair.en)] {
            let res = sqlx::query(
                "insert into pages (id, tenant_id, slug, locale, translation_group_id, title, body, template, status,
                                    published_at, show_in_nav, show_in_footer, nav_order)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, 'published', now(), $9, true, $10)
                 on conflict (tenant_id, locale, slug) do nothing",
            )
            .bind(Uuid::now_v7())
            .bind(tenant_id)
            .bind(doc.slug)
            .bind(locale)
            .bind(group)
            .bind(doc.title)
            .bind(doc.body)
            .bind(if *in_nav { "default" } else { "legal" })
            .bind(*in_nav)
            .bind(i32::try_from(order).unwrap_or(0) * 10)
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

    #[test]
    fn templates_cover_every_key_in_both_locales() {
        let keys = [
            "welcome",
            "email_verification",
            "password_reset",
            "invoice_created",
            "invoice_paid",
            "invoice_overdue",
            "service_activated",
            "service_suspended",
            "ticket_reply",
            "domain_expiring",
        ];
        for key in keys {
            for locale in ["tr", "en"] {
                assert_eq!(
                    MAILS
                        .iter()
                        .filter(|m| m.key == key && m.locale == locale)
                        .count(),
                    1,
                    "{key}/{locale}"
                );
            }
        }
        assert_eq!(MAILS.len(), keys.len() * 2);
    }

    #[test]
    fn rendering() {
        let m = &MAILS[0];
        let html = m.html();
        assert!(html.contains(r#"href="{{action_url}}""#), "{html}");
        assert!(m.text().contains(": {{action_url}}"));
        // Every `{{#if}}` is closed.
        for m in MAILS {
            let t = m.text();
            assert_eq!(
                t.matches("{{#if").count(),
                t.matches("{{/if}}").count(),
                "{}",
                m.key
            );
        }
    }

    #[test]
    fn slugs_are_valid() {
        let ok = |s: &str| {
            !s.is_empty()
                && s.split('-').all(|p| {
                    !p.is_empty()
                        && p.chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                })
        };
        let docs = KB_ARTICLES
            .iter()
            .map(|(_, p)| p)
            .chain(FAQS)
            .chain([&ANNOUNCEMENT])
            .chain(PAGES.iter().map(|(p, _)| p));
        for p in docs {
            assert!(
                ok(p.tr.slug) && ok(p.en.slug),
                "{} / {}",
                p.tr.slug,
                p.en.slug
            );
        }
        assert!(KB_CATEGORIES.iter().all(|c| ok(c.slug)));
    }
}
