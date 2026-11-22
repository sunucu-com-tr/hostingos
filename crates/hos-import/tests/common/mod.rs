//! Harness of the import integration tests: the workspace `.env` database
//! (skipped without one), a throw-away tenant per test (deleted at the end)
//! and a WHMCS fixture.

#![allow(dead_code)]

use std::path::Path;

use hos_core::crypto::SecretBox;
use hos_core::ids::TenantId;
use hos_core::net::OutboundPolicy;
use hos_db::repo::import::{self as repo, ImportRunRow, NewImportRun};
use hos_db::repo::platform;
use hos_db::{DbConfig, PgPool};
use hos_import::config::{
    DbSourceConfig, ImportOptions, Mappings, SourceConfig, SourceSecrets, TlsMode,
};
use hos_import::crypto::seal_secrets;
use hos_import::entity::Entity;
use hos_import::row::Row;
use hos_import::runner::Runner;
use hos_import::source::fixture::FixtureSource;
use hos_import::source::{AttachmentOwner, Flavor, Source};
use tokio::sync::OnceCell;
use uuid::Uuid;

static MIGRATED: OnceCell<()> = OnceCell::const_new();

/// WHMCS `cc_encryption_hash` of the fixture.
pub const CC_HASH: &str = "SOmECRAZYLONGHASHROFLCOPTERBBQKTHX";
/// `hunter2` encrypted with [`CC_HASH`].
pub const ENC_HUNTER2: &str = "auhMlPfMu6L7sJbH/wWHtwWi+4Rpd21wYHQ1";
/// Password of the fixture's bcrypt login.
pub const USER_PASSWORD: &str = "correct horse battery";
/// Stored XSS attempt carried by the fixture.
pub const XSS: &str = "<script>alert('x')</script><img src=x onerror=alert(1)>";

fn load_env() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let _ = dotenvy::from_path(root);
}

fn env_url(vars: &[&str]) -> Option<String> {
    load_env();
    vars.iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|u| !u.trim().is_empty())
}

/// Connects (and migrates once); `None` without a database.
pub async fn pool() -> Option<PgPool> {
    let Some(url) = env_url(&["DATABASE_URL", "DATABASE_URL_DIRECT"]) else {
        eprintln!("DATABASE_URL not set — skipping database test");
        return None;
    };
    MIGRATED
        .get_or_init(|| async {
            let direct = env_url(&["DATABASE_URL_DIRECT", "DATABASE_URL"]).expect("url");
            let pool = hos_db::connect(&DbConfig::new(&direct).with_max_connections(1))
                .await
                .expect("connect for migrations");
            hos_db::migrate(&pool).await.expect("migrate");
            pool.close().await;
        })
        .await;
    let cfg = DbConfig::new(&url)
        .with_max_connections(8)
        .with_application_name("hos-import-tests");
    Some(hos_db::connect(&cfg).await.expect("connect"))
}

/// A throw-away tenant.
pub struct Env {
    pub pool: PgPool,
    pub tenant: TenantId,
    pub secret_box: SecretBox,
}

impl Env {
    pub async fn new() -> Option<Self> {
        let pool = pool().await?;
        let slug = format!("imp-{}", &Uuid::now_v7().simple().to_string()[20..]);
        let mut new = platform::NewTenant::new(&slug, "Import Test");
        new.status = hos_core::domain::TenantStatus::Active;
        let row = platform::create_tenant(&pool, &new).await.expect("tenant");
        let mut conn = pool.acquire().await.expect("acquire");
        hos_db::seed::seed_tenant_defaults(&mut conn, row.id)
            .await
            .expect("seed tenant");
        drop(conn);
        Some(Self {
            pool,
            tenant: row.id,
            secret_box: SecretBox::new(&[7u8; 32]),
        })
    }

    pub async fn cleanup(self) {
        platform::delete_tenant(&self.pool, self.tenant)
            .await
            .expect("delete tenant");
    }

    pub fn runner(&self) -> Runner {
        Runner::new(
            self.pool.clone(),
            self.secret_box.clone(),
            OutboundPolicy::Unrestricted,
            "test-worker",
        )
    }

    /// A draft run with sealed secrets.
    pub async fn draft(
        &self,
        config: &SourceConfig,
        options: &ImportOptions,
        mappings: &Mappings,
        secrets: &SourceSecrets,
    ) -> Uuid {
        let id = Uuid::now_v7();
        repo::create_run(
            &self.pool,
            self.tenant,
            &NewImportRun {
                id,
                source_kind: config.kind().into(),
                source_key: config.key(),
                source_label: config.label(),
                source_config: serde_json::to_value(config).unwrap(),
                options: serde_json::to_value(options).unwrap(),
                started_by: None,
            },
        )
        .await
        .expect("create run");
        let sealed = seal_secrets(&self.secret_box, self.tenant, id, secrets).unwrap();
        assert!(
            repo::set_run_secrets(&self.pool, self.tenant, id, &sealed)
                .await
                .unwrap()
        );
        repo::update_draft(
            &self.pool,
            self.tenant,
            id,
            None,
            Some(&serde_json::to_value(mappings).unwrap()),
            None,
        )
        .await
        .unwrap();
        id
    }

    /// Queues `phase` and runs it inline against `source` (queued and
    /// claimed in one transaction: a worker of a server on the same database
    /// never sees the run queued).
    pub async fn run(&self, id: Uuid, phase: &str, source: Box<dyn Source>) -> ImportRunRow {
        self.runner()
            .queue_and_run(self.tenant, id, phase, None, Some(source))
            .await
            .expect("run")
            .expect("queued")
    }

    /// Dry run + import of the fixture in one go.
    pub async fn import(&self, source: FixtureSource, options: ImportOptions) -> ImportRunRow {
        let id = self
            .draft(&db_config(), &options, &Mappings::default(), &secrets())
            .await;
        self.run(id, "import", Box::new(source)).await
    }

    pub async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(&format!(
            "select count(*) from {table} where tenant_id = $1"
        ))
        .bind(self.tenant)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    pub async fn scalar<T>(&self, sql: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
    {
        sqlx::query_scalar::<_, T>(sql)
            .bind(self.tenant)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

pub fn db_config() -> SourceConfig {
    SourceConfig::WhmcsDb(DbSourceConfig {
        host: "whmcs-db.example.net".into(),
        port: 3306,
        database: "whmcs".into(),
        user: "reader".into(),
        tls: TlsMode::Disabled,
        attachments_api: None,
    })
}

pub fn secrets() -> SourceSecrets {
    SourceSecrets {
        db_password: Some("db-pass".into()),
        cc_encryption_hash: Some(CC_HASH.into()),
        ..SourceSecrets::default()
    }
}

/// A PHP-style (`$2y$`) bcrypt hash of [`USER_PASSWORD`].
pub fn bcrypt_hash() -> String {
    let h = bcrypt::hash(USER_PASSWORD, 4).unwrap();
    format!("$2y${}", &h[4..])
}

fn r() -> Row {
    Row::new()
}

/// A small but complete WHMCS 8 installation.
pub fn fixture() -> FixtureSource {
    FixtureSource::new(Flavor::Database)
        .with_setting("Version", "8.10.1-release.1")
        .with_setting("DefaultCountry", "TR")
        .with_setting("Language", "turkish")
        .with_table("tblbillableitems", 2)
        .with_table("tblservergroups", 0)
        .with(
            Entity::Currencies,
            vec![
                r().with("id", 1)
                    .with("code", "USD")
                    .with("prefix", "$")
                    .with("suffix", " USD")
                    .with("rate", "1.00000")
                    .with("default", 1),
                r().with("id", 2)
                    .with("code", "TRY")
                    .with("prefix", "")
                    .with("suffix", " TL")
                    .with("rate", "32.50000")
                    .with("default", 0),
                r().with("id", 3)
                    .with("code", "GBP")
                    .with("prefix", "&pound;")
                    .with("suffix", "")
                    .with("rate", "0.79")
                    .with("default", 0),
            ],
        )
        .with(
            Entity::TaxRules,
            vec![
                r().with("id", 1)
                    .with("level", 1)
                    .with("name", "VAT")
                    .with("country", "GB")
                    .with("state", "")
                    .with("taxrate", "20.00"),
            ],
        )
        .with(
            Entity::ClientGroups,
            vec![
                r().with("id", 1)
                    .with("groupname", "Resellers")
                    .with("groupcolour", "#ff0000")
                    .with("discountpercent", "10.00"),
            ],
        )
        .with(
            Entity::Servers,
            vec![
                r().with("id", 1)
                    .with("name", "web1")
                    .with("hostname", "web1.example.net")
                    .with("ipaddress", "192.0.2.10")
                    .with("type", "cpanel")
                    .with("username", "root")
                    .with("password", ENC_HUNTER2)
                    .with("accesshash", "")
                    .with("secure", "on"),
                r().with("id", 2)
                    .with("name", "legacy")
                    .with("hostname", "old.example.net")
                    .with("type", "autorelease"),
            ],
        )
        .with(
            Entity::Staff,
            vec![
                r().with("id", 1)
                    .with("roleid", 1)
                    .with("role_name", "Full Administrator")
                    .with("username", "admin")
                    .with("firstname", "Ada")
                    .with("lastname", "Admin")
                    .with("email", "ada.staff@example.net")
                    .with("disabled", 0),
            ],
        )
        .with(
            Entity::Clients,
            vec![
                r().with("id", 1)
                    .with("firstname", "Ayşe")
                    .with("lastname", "Yılmaz")
                    .with("companyname", "")
                    .with("email", "ayse@example.net")
                    .with("address1", "Street 1")
                    .with("city", "İstanbul")
                    .with("country", "TR")
                    .with("phonenumber", "+90.5551112233")
                    .with("currency", 2)
                    .with("groupid", 1)
                    .with("status", "Active")
                    .with("credit", "25.00")
                    .with("defaultgateway", "banktransfer")
                    .with("datecreated", "2020-01-05")
                    .with("language", "turkish")
                    .with("notes", "VIP customer")
                    .with("taxexempt", "")
                    .with("email_verified", 1),
                r().with("id", 2)
                    .with("firstname", XSS)
                    .with("lastname", "Doe")
                    .with("companyname", "Evil &amp; Co")
                    .with("email", "john@example.net")
                    .with("country", "GB")
                    .with("currency", 1)
                    .with("status", "Active")
                    .with("credit", "0.00")
                    .with("datecreated", "2021-03-01"),
                r().with("id", 3)
                    .with("firstname", "Bad")
                    .with("lastname", "Mail")
                    .with("email", "not-an-email")
                    .with("country", "TR")
                    .with("currency", 1)
                    .with("status", "Active"),
            ],
        )
        .with(
            Entity::Users,
            vec![
                r().with("id", 10)
                    .with("first_name", "Ayşe")
                    .with("last_name", "Yılmaz")
                    .with("email", "ayse@example.net")
                    .with("password", bcrypt_hash())
                    .with("language", "turkish")
                    .with("created_at", "2020-01-05 10:00:00"),
                r().with("id", 11)
                    .with("first_name", "John")
                    .with("last_name", "Doe")
                    .with("email", "john@example.net")
                    .with("password", "5f4dcc3b5aa765d61d8327deb882cf99:abcde")
                    .with("created_at", "2021-03-01 10:00:00"),
            ],
        )
        .with(
            Entity::UserLinks,
            vec![
                r().with("id", 1)
                    .with("auth_user_id", 10)
                    .with("client_id", 1)
                    .with("owner", 1)
                    .with("permissions", ""),
                r().with("id", 2)
                    .with("auth_user_id", 11)
                    .with("client_id", 2)
                    .with("owner", 1)
                    .with("permissions", ""),
                r().with("id", 3)
                    .with("auth_user_id", 11)
                    .with("client_id", 1)
                    .with("owner", 0)
                    .with("permissions", "profile,invoices,tickets"),
            ],
        )
        .with(
            Entity::Contacts,
            vec![
                r().with("id", 1)
                    .with("userid", 1)
                    .with("firstname", "Mehmet")
                    .with("lastname", "Yılmaz")
                    .with("email", "mehmet@example.net")
                    .with("country", "TR")
                    .with("invoiceemails", 1)
                    .with("supportemails", 1),
            ],
        )
        .with(
            Entity::ClientNotes,
            vec![
                r().with("id", 1)
                    .with("userid", 1)
                    .with("adminid", 1)
                    .with("note", "Called about renewal")
                    .with("sticky", 0)
                    .with("created", "2023-01-01 09:00:00"),
            ],
        )
        .with(
            Entity::Credits,
            vec![
                r().with("id", 1)
                    .with("clientid", 1)
                    .with("date", "2023-02-01")
                    .with("description", "Manual credit")
                    .with("amount", "50.00")
                    .with("relid", 0),
                r().with("id", 2)
                    .with("clientid", 1)
                    .with("date", "2023-03-01")
                    .with("description", "Credit Applied to Invoice #100")
                    .with("amount", "-20.00")
                    .with("relid", 100),
            ],
        )
        .with(
            Entity::ProductGroups,
            vec![
                r().with("id", 1)
                    .with("name", "Web Hosting")
                    .with("headline", "Fast")
                    .with("hidden", 0)
                    .with("order", 1),
            ],
        )
        .with(
            Entity::Products,
            vec![
                r().with("id", 1)
                    .with("type", "hostingaccount")
                    .with("gid", 1)
                    .with("name", "Starter")
                    .with(
                        "description",
                        "<p>Great <b>plan</b></p><script>x()</script>",
                    )
                    .with("paytype", "recurring")
                    .with("autosetup", "payment")
                    .with("servertype", "cpanel")
                    .with("servergroup", 1)
                    .with("group_server_id", 1)
                    .with("configoption1", "starter_pkg")
                    .with("showdomainoptions", "on")
                    .with("tax", 1),
            ],
        )
        .with(
            Entity::ConfigGroups,
            vec![
                r().with("id", 1)
                    .with("name", "Extras")
                    .with("description", ""),
            ],
        )
        .with(
            Entity::ConfigLinks,
            vec![r().with("id", 1).with("gid", 1).with("pid", 1)],
        )
        .with(
            Entity::ConfigOptions,
            vec![
                r().with("id", 1)
                    .with("gid", 1)
                    .with("optionname", "disk|Extra disk")
                    .with("optiontype", 4)
                    .with("qtyminimum", 0)
                    .with("qtymaximum", 10),
            ],
        )
        .with(
            Entity::ConfigValues,
            vec![
                r().with("id", 1)
                    .with("configid", 1)
                    .with("optionname", "gb|GB")
                    .with("sortorder", 0),
            ],
        )
        .with(
            Entity::Addons,
            vec![
                r().with("id", 1)
                    .with("packages", "1")
                    .with("name", "Backups")
                    .with("billingcycle", "Monthly")
                    .with("showorder", "on")
                    .with("tax", "on"),
            ],
        )
        .with(
            Entity::Promotions,
            vec![
                r().with("id", 1)
                    .with("code", "WELCOME10")
                    .with("type", "Percentage")
                    .with("value", "10")
                    .with("cycles", "Monthly,Annually")
                    .with("appliesto", "1,A1,D.com")
                    .with("recurring", 0)
                    .with("maxuses", 0)
                    .with("uses", 3)
                    .with("expirationdate", "0000-00-00"),
            ],
        )
        .with(
            Entity::Tlds,
            vec![
                r().with("id", 1)
                    .with("extension", ".com")
                    .with("dnsmanagement", "on")
                    .with("idprotection", "on")
                    .with("eppcode", "on")
                    .with("autoreg", "enom")
                    .with("order", 1),
            ],
        )
        .with(
            Entity::Pricing,
            vec![
                r().with("id", 1)
                    .with("type", "product")
                    .with("currency", 1)
                    .with("relid", 1)
                    .with("msetupfee", "0")
                    .with("monthly", "5.00")
                    .with("qsetupfee", "0")
                    .with("quarterly", "-1.00")
                    .with("ssetupfee", "0")
                    .with("semiannually", "-1")
                    .with("asetupfee", "0")
                    .with("annually", "50.00")
                    .with("bsetupfee", "0")
                    .with("biennially", "-1")
                    .with("tsetupfee", "0")
                    .with("triennially", "-1"),
                r().with("id", 2)
                    .with("type", "domainregister")
                    .with("currency", 1)
                    .with("relid", 1)
                    .with("tld", "com")
                    .with("msetupfee", "9.99")
                    .with("qsetupfee", "19.98")
                    .with("ssetupfee", "-1")
                    .with("asetupfee", "-1")
                    .with("bsetupfee", "-1")
                    .with("monthly", "-1")
                    .with("quarterly", "-1")
                    .with("semiannually", "-1")
                    .with("annually", "-1")
                    .with("biennially", "-1"),
                r().with("id", 3)
                    .with("type", "addon")
                    .with("currency", 1)
                    .with("relid", 1)
                    .with("msetupfee", "0")
                    .with("monthly", "2.00")
                    .with("quarterly", "-1")
                    .with("semiannually", "-1")
                    .with("annually", "-1")
                    .with("biennially", "-1")
                    .with("triennially", "-1"),
                r().with("id", 4)
                    .with("type", "configoptions")
                    .with("currency", 1)
                    .with("relid", 1)
                    .with("msetupfee", "0")
                    .with("monthly", "1.00")
                    .with("quarterly", "-1")
                    .with("semiannually", "-1")
                    .with("annually", "-1")
                    .with("biennially", "-1")
                    .with("triennially", "-1"),
            ],
        )
        .with(
            Entity::Invoices,
            vec![
                r().with("id", 100)
                    .with("userid", 1)
                    .with("invoicenum", "")
                    .with("date", "2024-01-01")
                    .with("duedate", "2024-01-08")
                    .with("datepaid", "2024-01-03 12:00:00")
                    .with("subtotal", "50.00")
                    .with("credit", "20.00")
                    .with("tax", "0")
                    .with("tax2", "0")
                    .with("total", "30.00")
                    .with("taxrate", "0")
                    .with("taxrate2", "0")
                    .with("status", "Paid")
                    .with("paymentmethod", "banktransfer"),
                r().with("id", 101)
                    .with("userid", 1)
                    .with("invoicenum", "")
                    .with("date", "2099-01-01")
                    .with("duedate", "2099-01-08")
                    .with("subtotal", "5.00")
                    .with("credit", "0")
                    .with("tax", "1.00")
                    .with("tax2", "0")
                    .with("total", "6.00")
                    .with("taxrate", "20.00")
                    .with("taxrate2", "0")
                    .with("status", "Unpaid")
                    .with("paymentmethod", "banktransfer"),
                r().with("id", 102)
                    .with("userid", 99)
                    .with("date", "2024-01-01")
                    .with("duedate", "2024-01-08")
                    .with("total", "1")
                    .with("status", "Unpaid"),
            ],
        )
        .with(
            Entity::Orders,
            vec![
                r().with("id", 1)
                    .with("ordernum", "1234567890")
                    .with("userid", 1)
                    .with("date", "2024-01-01 10:00:00")
                    .with("amount", "50.00")
                    .with("invoiceid", 100)
                    .with("status", "Active")
                    .with("ipaddress", "198.51.100.7")
                    .with("paymentmethod", "banktransfer"),
            ],
        )
        .with(
            Entity::Services,
            vec![
                r().with("id", 1)
                    .with("userid", 1)
                    .with("orderid", 1)
                    .with("packageid", 1)
                    .with("server", 1)
                    .with("regdate", "2024-01-01")
                    .with("domain", "ayse.example")
                    .with("paymentmethod", "banktransfer")
                    .with("firstpaymentamount", "5.00")
                    .with("amount", "5.00")
                    .with("billingcycle", "Monthly")
                    .with("nextduedate", "2099-01-08")
                    .with("nextinvoicedate", "2099-01-01")
                    .with("domainstatus", "Active")
                    .with("username", "ayse")
                    .with("password", ENC_HUNTER2)
                    .with("dedicatedip", "")
                    .with("assignedips", "192.0.2.50\n192.0.2.51")
                    .with("notes", "")
                    .with("server_hostname", "web1.example.net"),
                r().with("id", 2)
                    .with("userid", 1)
                    .with("packageid", 77)
                    .with("billingcycle", "Monthly")
                    .with("domainstatus", "Active"),
            ],
        )
        .with(
            Entity::ServiceOptions,
            vec![
                r().with("id", 1)
                    .with("relid", 1)
                    .with("configid", 1)
                    .with("optionid", 1)
                    .with("qty", 5)
                    .with("opt_type", 4),
            ],
        )
        .with(
            Entity::CustomFieldValues,
            vec![
                r().with("id", 1)
                    .with("fieldid", 1)
                    .with("relid", 1)
                    .with("value", "12345678901")
                    .with("cf_type", "client")
                    .with("cf_name", "tc_no|TC Kimlik No"),
                r().with("id", 2)
                    .with("fieldid", 2)
                    .with("relid", 1)
                    .with("value", "wordpress")
                    .with("cf_type", "product")
                    .with("cf_name", "CMS"),
            ],
        )
        .with(
            Entity::ServiceAddons,
            vec![
                r().with("id", 1)
                    .with("hostingid", 1)
                    .with("addonid", 1)
                    .with("userid", 1)
                    .with("name", "")
                    .with("recurring", "2.00")
                    .with("billingcycle", "Monthly")
                    .with("status", "Active")
                    .with("regdate", "2024-01-01")
                    .with("nextduedate", "2099-01-08"),
            ],
        )
        .with(Entity::CancelRequests, vec![])
        .with(
            Entity::Domains,
            vec![
                r().with("id", 1)
                    .with("userid", 1)
                    .with("orderid", 1)
                    .with("type", "Register")
                    .with("registrationdate", "2023-05-01")
                    .with("domain", "ayse-example.com")
                    .with("firstpaymentamount", "9.99")
                    .with("recurringamount", "9.99")
                    .with("registrar", "enom")
                    .with("registrationperiod", 1)
                    .with("expirydate", "2099-05-01")
                    .with("status", "Active")
                    .with("nextduedate", "2099-05-01")
                    .with("dnsmanagement", "on")
                    .with("idprotection", "")
                    .with("donotrenew", 0)
                    .with("order_nameservers", "ns1.example.net,ns2.example.net"),
            ],
        )
        .with(
            Entity::InvoiceItems,
            vec![
                r().with("id", 1)
                    .with("invoiceid", 100)
                    .with("userid", 1)
                    .with("type", "Hosting")
                    .with("relid", 1)
                    .with("description", "Starter (01/01/2024 - 31/01/2024)")
                    .with("amount", "50.00")
                    .with("taxed", 0)
                    .with("duedate", "2024-01-01")
                    .with("service_cycle", "Monthly"),
                r().with("id", 2)
                    .with("invoiceid", 101)
                    .with("userid", 1)
                    .with("type", "Hosting")
                    .with("relid", 1)
                    .with("description", "Starter renewal")
                    .with("amount", "5.00")
                    .with("taxed", 1)
                    .with("duedate", "2099-01-08")
                    .with("service_cycle", "Monthly"),
            ],
        )
        .with(
            Entity::Transactions,
            vec![
                r().with("id", 1)
                    .with("userid", 1)
                    .with("currency", 0)
                    .with("gateway", "banktransfer")
                    .with("date", "2024-01-03 12:00:00")
                    .with("description", "Invoice Payment")
                    .with("amountin", "30.00")
                    .with("fees", "0")
                    .with("amountout", "0")
                    .with("transid", "TX-1")
                    .with("invoiceid", 100),
                r().with("id", 2)
                    .with("userid", 1)
                    .with("currency", 0)
                    .with("gateway", "banktransfer")
                    .with("date", "2024-01-04 12:00:00")
                    .with("description", "Refund")
                    .with("amountin", "0")
                    .with("fees", "0")
                    .with("amountout", "5.00")
                    .with("transid", "TX-1")
                    .with("invoiceid", 100)
                    .with("refundid", 1),
            ],
        )
        .with(
            Entity::Departments,
            vec![
                r().with("id", 1)
                    .with("name", "Billing Desk")
                    .with("email", "billing@example.net")
                    .with("hidden", ""),
            ],
        )
        .with(
            Entity::Tickets,
            vec![
                r().with("id", 1)
                    .with("tid", "ABC-123456")
                    .with("did", 1)
                    .with("userid", 1)
                    .with("name", "")
                    .with("email", "ayse@example.net")
                    .with("date", "2024-02-01 10:00:00")
                    .with("title", "Help <b>me</b>")
                    .with("message", format!("<p>Hello</p>{XSS}"))
                    .with("status", "Answered")
                    .with("urgency", "High")
                    .with("admin", "")
                    .with("attachment", "111111_shot.png|222222_evil.html")
                    .with("lastreply", "2024-02-01 11:00:00")
                    .with("flag", 1)
                    .with("service", "S1"),
            ],
        )
        .with_attachment(
            AttachmentOwner::Ticket,
            1,
            0,
            "shot.png",
            b"\x89PNG\r\n\x1a\nfakepng",
        )
        .with_attachment(
            AttachmentOwner::Ticket,
            1,
            1,
            "evil.html",
            b"<script>alert(1)</script>",
        )
        .with(
            Entity::TicketReplies,
            vec![
                r().with("id", 1)
                    .with("tid", 1)
                    .with("userid", 0)
                    .with("name", "Ada Admin")
                    .with("date", "2024-02-01 11:00:00")
                    .with("message", "We are on it.")
                    .with("admin", "Ada Admin"),
            ],
        )
        .with(
            Entity::TicketNotes,
            vec![
                r().with("id", 1)
                    .with("ticketid", 1)
                    .with("admin", "Ada Admin")
                    .with("date", "2024-02-01 11:05:00")
                    .with("message", "internal only"),
            ],
        )
        .with(
            Entity::CannedResponses,
            vec![
                r().with("id", 1)
                    .with("name", "Thanks")
                    .with("reply", "Thank you!")
                    .with("category_name", "General"),
            ],
        )
        .with(
            Entity::KbCategories,
            vec![
                r().with("id", 1)
                    .with("parentid", 0)
                    .with("name", "Getting started")
                    .with("description", "")
                    .with("hidden", ""),
            ],
        )
        .with(
            Entity::KbArticles,
            vec![
                r().with("id", 1)
                    .with("title", "How to log in")
                    .with(
                        "article",
                        "<h2>Steps</h2><p>Go to <a href=\"javascript:alert(1)\">here</a></p>",
                    )
                    .with("views", 5)
                    .with("useful", 2)
                    .with("private", "")
                    .with("categoryid", 1),
            ],
        )
        .with(
            Entity::Announcements,
            vec![
                r().with("id", 1)
                    .with("date", "2024-03-01 09:00:00")
                    .with("title", "We moved")
                    .with("announcement", "New panel!")
                    .with("published", "on"),
            ],
        )
        .with(
            Entity::ActivityLog,
            vec![
                r().with("id", 1)
                    .with("date", "2024-03-01 09:00:00")
                    .with("description", "Client logged in")
                    .with("user", "ayse")
                    .with("ipaddr", "198.51.100.7"),
            ],
        )
        .with(
            Entity::Quotes,
            vec![
                r().with("id", 1)
                    .with("subject", "Migration offer")
                    .with("stage", "Delivered")
                    .with("validuntil", "2099-01-31")
                    .with("userid", 1)
                    .with("currency", 2)
                    .with("subtotal", "114.63")
                    .with("tax1", "20.00")
                    .with("tax2", "0.00")
                    .with("total", "134.63")
                    .with("proposal", "Our offer")
                    .with("customernotes", "Thanks!")
                    .with("adminnotes", "internal")
                    .with("datecreated", "2024-05-01")
                    .with("datesent", "2024-05-02"),
                r().with("id", 2)
                    .with("subject", "")
                    .with("stage", "Accepted")
                    .with("validuntil", "2024-06-01")
                    .with("userid", 0)
                    .with("firstname", "Pat")
                    .with("lastname", "Prospect")
                    .with("companyname", "Acme")
                    .with("email", "Prospect@Example.org")
                    .with("currency", 1)
                    .with("subtotal", "10.00")
                    .with("total", "10.00")
                    .with("datecreated", "2024-05-10")
                    .with("dateaccepted", "2024-05-20"),
                r().with("id", 3)
                    .with("subject", "Nobody")
                    .with("stage", "Draft")
                    .with("userid", 0)
                    .with("email", ""),
            ],
        )
        .with(
            Entity::QuoteItems,
            vec![
                r().with("id", 1)
                    .with("quoteid", 1)
                    .with("description", "Site migration")
                    .with("quantity", "1")
                    .with("unitprice", "100.00")
                    .with("discount", "0.00")
                    .with("taxable", 1),
                r().with("id", 2)
                    .with("quoteid", 1)
                    .with("description", "Mailbox move")
                    .with("quantity", "3")
                    .with("unitprice", "5.00")
                    .with("discount", "2.50")
                    .with("taxable", 0),
                r().with("id", 3)
                    .with("quoteid", 2)
                    .with("description", "Setup")
                    .with("quantity", "1")
                    .with("unitprice", "10.00")
                    .with("discount", "0"),
                r().with("id", 4)
                    .with("quoteid", 99)
                    .with("description", "Orphan")
                    .with("unitprice", "1.00"),
            ],
        )
        .with(
            Entity::Affiliates,
            vec![
                r().with("id", 1)
                    .with("date", "2023-01-01")
                    .with("clientid", 2)
                    .with("visitors", 5)
                    .with("paytype", "percentage")
                    .with("payamount", "15.00")
                    .with("onetime", 1)
                    .with("balance", "30.00")
                    .with("withdrawn", "10.00"),
                r().with("id", 2)
                    .with("date", "2023-02-01")
                    .with("clientid", 1)
                    .with("paytype", "")
                    .with("payamount", "0.00")
                    .with("onetime", 0)
                    .with("balance", "12.50")
                    .with("withdrawn", "0.00"),
            ],
        )
        .with(
            Entity::AffiliateAccounts,
            vec![
                r().with("id", 1)
                    .with("affiliateid", 1)
                    .with("relid", 1)
                    .with("service_userid", 1),
                r().with("id", 2)
                    .with("affiliateid", 1)
                    .with("relid", 2)
                    .with("service_userid", 1),
            ],
        )
        .with(
            Entity::AffiliateHistory,
            vec![
                r().with("id", 1)
                    .with("affiliateid", 1)
                    .with("date", "2024-02-01")
                    .with("affaccid", 1)
                    .with("account_relid", 1)
                    .with("description", "Commission")
                    .with("amount", "20.00"),
                r().with("id", 2)
                    .with("affiliateid", 1)
                    .with("date", "2024-03-01")
                    .with("affaccid", 1)
                    .with("account_relid", 1)
                    .with("amount", "15.00"),
                r().with("id", 3)
                    .with("affiliateid", 1)
                    .with("date", "2024-03-05")
                    .with("affaccid", 1)
                    .with("amount", "-5.00"),
                r().with("id", 4)
                    .with("affiliateid", 1)
                    .with("date", "2024-03-06")
                    .with("affaccid", 0)
                    .with("amount", "7.00"),
            ],
        )
        .with(
            Entity::AffiliateWithdrawals,
            vec![
                r().with("id", 1)
                    .with("affiliateid", 1)
                    .with("date", "2024-01-15")
                    .with("amount", "10.00"),
            ],
        )
        .with(
            Entity::Todos,
            vec![
                r().with("id", 1)
                    .with("date", "2024-04-01")
                    .with("title", "Renew SSL")
                    .with("description", "for web1")
                    .with("admin", 1)
                    .with("status", "In Progress")
                    .with("duedate", "2099-05-01"),
                r().with("id", 2)
                    .with("date", "2024-04-02")
                    .with("title", "Old chore")
                    .with("admin", 0)
                    .with("status", "Completed"),
                r().with("id", 3).with("title", "").with("status", "New"),
            ],
        )
        .with(
            Entity::BannedIps,
            vec![
                r().with("id", 1)
                    .with("ip", "203.0.113.9")
                    .with("reason", "abuse")
                    .with("expires", "2099-01-01 00:00:00"),
                r().with("id", 2)
                    .with("ip", "198.51.100.*")
                    .with("reason", "")
                    .with("expires", ""),
                r().with("id", 3)
                    .with("ip", "203.0.113.10")
                    .with("expires", "2020-01-01 00:00:00"),
                r().with("id", 4).with("ip", "nonsense"),
            ],
        )
        .with(
            Entity::BannedEmails,
            vec![
                r().with("id", 1)
                    .with("domain", "spam.example")
                    .with("count", 3),
                r().with("id", 2).with("domain", "@Junk.Example"),
                r().with("id", 3).with("domain", "x"),
            ],
        )
        .with(
            Entity::DownloadCategories,
            vec![
                r().with("id", 1)
                    .with("parentid", 0)
                    .with("name", "Guides")
                    .with("description", "PDF guides")
                    .with("hidden", 0),
            ],
        )
        .with(
            Entity::Downloads,
            vec![
                r().with("id", 1)
                    .with("category", 1)
                    .with("type", "pdf")
                    .with("title", "Setup guide")
                    .with("description", "How to start")
                    .with("downloads", 12)
                    .with("location", "setup-guide.pdf")
                    .with("clientsonly", 1)
                    .with("hidden", 0)
                    .with("productdownload", 0),
                r().with("id", 2)
                    .with("category", 1)
                    .with("title", "Public tool")
                    .with("location", "../../tool.zip")
                    .with("clientsonly", 0),
            ],
        )
        .with(
            Entity::NetworkIssues,
            vec![
                r().with("id", 1)
                    .with("title", "Web1 outage")
                    .with("description", "Disk failure")
                    .with("type", "Server")
                    .with("server", 1)
                    .with("server_name", "web1")
                    .with("priority", "High")
                    .with("status", "Resolved")
                    .with("startdate", "2024-02-01 10:00:00")
                    .with("enddate", "2024-02-01 12:00:00")
                    .with("lastupdate", "2024-02-01 12:00:00"),
                r().with("id", 2)
                    .with("title", "Planned upgrade")
                    .with("type", "Other")
                    .with("affecting", "All shared hosting")
                    .with("priority", "Low")
                    .with("status", "Scheduled")
                    .with("startdate", "2099-01-01 02:00:00")
                    .with("enddate", "2099-01-01 04:00:00"),
            ],
        )
}
