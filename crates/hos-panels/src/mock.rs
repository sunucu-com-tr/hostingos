//! In-memory panel for demos and end-to-end tests.
//!
//! Behaves like a real panel: usernames follow cPanel's rules (generated from
//! the domain when blank), duplicate usernames/domains are `Conflict`, unknown
//! accounts/packages are `NotFound`, suspended accounts cannot open SSO
//! sessions, and usage figures are deterministic per account.
//!
//! State lives in a process-wide store keyed by `config.namespace`, so
//! rebuilding the integration (e.g. after an admin edits it) keeps accounts.
//! `config.fail_operations` makes chosen operations fail, to exercise error
//! handling in billing/job code.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::panel::{
    AccountCreated, AccountInfo, AccountRef, AccountSpec, AccountUsage, PanelLimits, PanelPackage,
    SsoLogin, SsoService, SsoTarget,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, HostingPanel, IntegrationDescriptor,
};
use hos_core::secret::SecretString;
use serde_json::{Value, json};

use crate::settings::parse_list;
use crate::username::UsernameRules;

/// `integrations.kind` of the mock panel.
pub const KIND: &str = "mock";

const LOGIN_URL: &str = "https://panel.mock.invalid:2083/";

/// Admin form schema.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(KIND, "Demo panel (simulated)", IntegrationCategory::Panel)
        .description(
            "In-memory hosting panel for demos and tests. Nothing is provisioned on a real server.",
        )
        .config_field(
            FieldSpec::text("namespace", "Namespace")
                .default_value("default")
                .help("Integrations with the same namespace share accounts"),
        )
        .config_field(
            FieldSpec::text_area("packages", "Packages (JSON)")
                .help("Optional JSON array of packages: [{\"id\":\"starter\",\"name\":\"Starter\",\"limits\":{\"disk_mb\":1024}}]"),
        )
        .config_field(
            FieldSpec::number("latency_ms", "Simulated latency (ms)").default_value(0),
        )
        .config_field(
            FieldSpec::text("fail_operations", "Failing operations")
                .placeholder("suspend, usage")
                .help("Comma-separated method names that return a provider error"),
        )
        .config_field(FieldSpec::text("default_ip", "IP address").default_value("192.0.2.10"))
        .config_field(
            FieldSpec::text("nameservers", "Nameservers")
                .default_value("ns1.mock.invalid, ns2.mock.invalid"),
        )
}

#[derive(Debug, Clone)]
struct MockAccount {
    username: String,
    domain: String,
    package: String,
    email: String,
    ip: Option<IpAddr>,
    password: SecretString,
    reseller: bool,
    suspended: bool,
    suspend_reason: Option<String>,
    limits: PanelLimits,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct MockState {
    packages: Vec<PanelPackage>,
    accounts: BTreeMap<String, MockAccount>,
}

type Store = Arc<Mutex<MockState>>;

fn registry() -> &'static Mutex<HashMap<String, Store>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Store>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn default_packages() -> Vec<PanelPackage> {
    let pkg = |id: &str, name: &str, reseller: bool, limits: PanelLimits| PanelPackage {
        id: id.into(),
        name: name.into(),
        limits,
        reseller,
        extra: Value::Null,
    };
    vec![
        pkg(
            "starter",
            "Starter",
            false,
            PanelLimits {
                disk_mb: Some(5 * 1024),
                bandwidth_mb: Some(50 * 1024),
                domains: Some(1),
                subdomains: Some(10),
                alias_domains: Some(2),
                email_accounts: Some(10),
                databases: Some(2),
                ftp_accounts: Some(2),
                inodes: Some(150_000),
                reseller_accounts: None,
            },
        ),
        pkg(
            "business",
            "Business",
            false,
            PanelLimits {
                disk_mb: Some(25 * 1024),
                bandwidth_mb: None,
                domains: Some(10),
                subdomains: None,
                alias_domains: None,
                email_accounts: None,
                databases: Some(25),
                ftp_accounts: None,
                inodes: Some(500_000),
                reseller_accounts: None,
            },
        ),
        pkg(
            "reseller",
            "Reseller",
            true,
            PanelLimits {
                disk_mb: Some(100 * 1024),
                bandwidth_mb: Some(1024 * 1024),
                reseller_accounts: Some(30),
                ..PanelLimits::default()
            },
        ),
    ]
}

/// The simulated panel.
#[derive(Debug, Clone)]
pub struct MockPanel {
    store: Store,
    latency: Duration,
    fail: HashSet<String>,
    default_ip: Option<IpAddr>,
    nameservers: Vec<String>,
}

impl MockPanel {
    /// A panel with its own private store and the default packages.
    pub fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(MockState {
                packages: default_packages(),
                accounts: BTreeMap::new(),
            })),
            latency: Duration::ZERO,
            fail: HashSet::new(),
            default_ip: "192.0.2.10".parse().ok(),
            nameservers: vec!["ns1.mock.invalid".into(), "ns2.mock.invalid".into()],
        }
    }

    /// Builds from `integrations.config` (no secrets are used).
    pub fn from_config(config: &Value) -> Result<Self, IntegrationError> {
        let cfg = FieldReader::config(config);
        let namespace = cfg.string_or("namespace", "default")?;
        let packages: Option<Vec<PanelPackage>> = match cfg.opt_str("packages") {
            Ok(Some(raw)) => Some(serde_json::from_str(raw).map_err(|_| {
                IntegrationError::Config("config.packages must be a JSON array of packages".into())
            })?),
            Ok(None) => None,
            // Allow a JSON array value as well as a string.
            Err(_) => cfg.opt_json("packages")?,
        };
        let store = {
            let mut reg = registry().lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(reg.entry(namespace).or_insert_with(|| {
                Arc::new(Mutex::new(MockState {
                    packages: default_packages(),
                    accounts: BTreeMap::new(),
                }))
            }))
        };
        if let Some(packages) = packages {
            lock(&store).packages = packages;
        }
        let default_ip = cfg
            .string_or("default_ip", "192.0.2.10")?
            .parse()
            .map_err(|_| {
                IntegrationError::Config("config.default_ip must be an IP address".into())
            })?;
        Ok(Self {
            store,
            latency: Duration::from_millis(cfg.u64_or("latency_ms", 0)?.min(60_000)),
            fail: parse_list(cfg.opt_str("fail_operations")?.unwrap_or_default())
                .into_iter()
                .collect(),
            default_ip: Some(default_ip),
            nameservers: parse_list(
                &cfg.string_or("nameservers", "ns1.mock.invalid, ns2.mock.invalid")?,
            ),
        })
    }

    /// Makes `operation` (a trait method name) fail with a provider error.
    #[must_use]
    pub fn failing(mut self, operation: &str) -> Self {
        self.fail.insert(operation.to_owned());
        self
    }

    /// Whether `password` is the account's current password (for tests).
    pub fn verify_password(&self, username: &str, password: &str) -> bool {
        lock(&self.store)
            .accounts
            .get(username)
            .is_some_and(|a| a.password == SecretString::from(password))
    }

    async fn enter(&self, op: &'static str) -> Result<(), IntegrationError> {
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        if self.fail.contains(op) {
            return Err(IntegrationError::remote_with_code(
                Some(500),
                "MOCK_FAILURE",
                format!("mock: simulated failure of {op}"),
            ));
        }
        Ok(())
    }

    fn state(&self) -> MutexGuard<'_, MockState> {
        lock(&self.store)
    }
}

impl Default for MockPanel {
    fn default() -> Self {
        Self::new()
    }
}

fn lock(store: &Store) -> MutexGuard<'_, MockState> {
    // A panic while holding the lock cannot leave the maps half-updated
    // (every mutation is a single insert/remove/field write).
    store.lock().unwrap_or_else(PoisonError::into_inner)
}

fn not_found(username: &str) -> IntegrationError {
    IntegrationError::NotFound(format!("mock: account `{username}` does not exist"))
}

fn info(a: &MockAccount) -> AccountInfo {
    AccountInfo {
        account: AccountRef {
            username: a.username.clone(),
            domain: Some(a.domain.clone()),
            id: None,
        },
        package: Some(a.package.clone()),
        email: Some(a.email.clone()),
        ip: a.ip,
        suspended: a.suspended,
        suspend_reason: a.suspend_reason.clone(),
        owner: Some("root".into()),
        created_at: Some(a.created_at),
        extra: json!({ "reseller": a.reseller }),
    }
}

/// Stable pseudo-random number for an account (usage simulation).
fn seed(username: &str, salt: &str) -> u64 {
    let mut h = DefaultHasher::new();
    username.hash(&mut h);
    salt.hash(&mut h);
    h.finish()
}

/// `pct` (0-90 %) of a limit, or a plausible absolute value when unlimited.
fn simulated(username: &str, what: &str, limit: Option<u64>, unlimited_max: u64) -> u64 {
    let s = seed(username, what);
    match limit {
        Some(0) => 0,
        Some(l) => l * (s % 91) / 100,
        None => s % unlimited_max.max(1),
    }
}

#[async_trait]
impl HostingPanel for MockPanel {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        self.enter("test_connection").await?;
        let n = self.state().accounts.len();
        Ok(ConnectionInfo {
            server_version: Some("HostingOS Mock Panel 1.0".into()),
            details: Some(format!("{n} simulated accounts")),
        })
    }

    async fn list_packages(&self) -> Result<Vec<PanelPackage>, IntegrationError> {
        self.enter("list_packages").await?;
        Ok(self.state().packages.clone())
    }

    async fn create_account(&self, spec: &AccountSpec) -> Result<AccountCreated, IntegrationError> {
        self.enter("create_account").await?;
        let domain = spec.domain.trim().trim_end_matches('.').to_lowercase();
        if domain.is_empty() || !domain.contains('.') || domain.contains(char::is_whitespace) {
            return Err(IntegrationError::InvalidInput(format!(
                "mock: `{domain}` is not a valid domain"
            )));
        }
        if !spec.owner.email.contains('@') {
            return Err(IntegrationError::InvalidInput(
                "mock: owner email is invalid".into(),
            ));
        }
        if spec.password.expose_str().chars().count() < 8 {
            return Err(IntegrationError::InvalidInput(
                "mock: password must be at least 8 characters".into(),
            ));
        }
        let mut state = self.state();
        let package = state
            .packages
            .iter()
            .find(|p| p.id == spec.package)
            .cloned()
            .ok_or_else(|| {
                IntegrationError::NotFound(format!(
                    "mock: package `{}` does not exist",
                    spec.package
                ))
            })?;
        if state.accounts.values().any(|a| a.domain == domain) {
            return Err(IntegrationError::Conflict(format!(
                "mock: domain `{domain}` already exists"
            )));
        }
        let rules = UsernameRules::CPANEL;
        let username = if spec.username.trim().is_empty() {
            rules.generate(&domain, |u| state.accounts.contains_key(u))?
        } else {
            let u = spec.username.trim().to_owned();
            rules.check(&u)?;
            if state.accounts.contains_key(&u) {
                return Err(IntegrationError::Conflict(format!(
                    "mock: username `{u}` is already taken"
                )));
            }
            u
        };
        let ip = spec.ip.or(self.default_ip);
        let limits = spec.limit_overrides.clone().unwrap_or(package.limits);
        state.accounts.insert(
            username.clone(),
            MockAccount {
                username: username.clone(),
                domain: domain.clone(),
                package: package.id,
                email: spec.owner.email.trim().to_owned(),
                ip,
                password: spec.password.clone(),
                reseller: spec.reseller,
                suspended: false,
                suspend_reason: None,
                limits,
                created_at: Utc::now(),
            },
        );
        Ok(AccountCreated {
            account: AccountRef {
                username,
                domain: Some(domain),
                id: None,
            },
            ip,
            nameservers: self.nameservers.clone(),
            login_url: Some(LOGIN_URL.into()),
            extra: json!({ "simulated": true }),
        })
    }

    async fn suspend(
        &self,
        account: &AccountRef,
        reason: Option<&str>,
    ) -> Result<(), IntegrationError> {
        self.enter("suspend").await?;
        let mut state = self.state();
        let a = state
            .accounts
            .get_mut(&account.username)
            .ok_or_else(|| not_found(&account.username))?;
        a.suspended = true;
        a.suspend_reason = reason.map(str::to_owned);
        Ok(())
    }

    async fn unsuspend(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        self.enter("unsuspend").await?;
        let mut state = self.state();
        let a = state
            .accounts
            .get_mut(&account.username)
            .ok_or_else(|| not_found(&account.username))?;
        a.suspended = false;
        a.suspend_reason = None;
        Ok(())
    }

    async fn terminate(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        self.enter("terminate").await?;
        self.state()
            .accounts
            .remove(&account.username)
            .map(drop)
            .ok_or_else(|| not_found(&account.username))
    }

    async fn change_package(
        &self,
        account: &AccountRef,
        package: &str,
    ) -> Result<(), IntegrationError> {
        self.enter("change_package").await?;
        let mut state = self.state();
        let pkg = state
            .packages
            .iter()
            .find(|p| p.id == package)
            .cloned()
            .ok_or_else(|| {
                IntegrationError::NotFound(format!("mock: package `{package}` does not exist"))
            })?;
        let a = state
            .accounts
            .get_mut(&account.username)
            .ok_or_else(|| not_found(&account.username))?;
        a.package = pkg.id;
        a.limits = pkg.limits;
        Ok(())
    }

    async fn change_password(
        &self,
        account: &AccountRef,
        password: &SecretString,
    ) -> Result<(), IntegrationError> {
        self.enter("change_password").await?;
        if password.expose_str().chars().count() < 8 {
            return Err(IntegrationError::InvalidInput(
                "mock: password must be at least 8 characters".into(),
            ));
        }
        let mut state = self.state();
        let a = state
            .accounts
            .get_mut(&account.username)
            .ok_or_else(|| not_found(&account.username))?;
        a.password = password.clone();
        Ok(())
    }

    async fn usage(&self, account: &AccountRef) -> Result<AccountUsage, IntegrationError> {
        self.enter("usage").await?;
        let state = self.state();
        let a = state
            .accounts
            .get(&account.username)
            .ok_or_else(|| not_found(&account.username))?;
        let u = a.username.as_str();
        let l = &a.limits;
        Ok(AccountUsage {
            collected_at: Utc::now(),
            disk_used_mb: Some(simulated(u, "disk", l.disk_mb, 20 * 1024)),
            bandwidth_used_mb: Some(simulated(u, "bw", l.bandwidth_mb, 100 * 1024)),
            domains: Some(simulated(u, "dom", l.domains, 5).max(1)),
            email_accounts: Some(simulated(u, "mail", l.email_accounts, 25)),
            databases: Some(simulated(u, "db", l.databases, 10)),
            ftp_accounts: Some(simulated(u, "ftp", l.ftp_accounts, 3)),
            inodes: Some(simulated(u, "inode", l.inodes, 200_000)),
            limits: l.clone(),
        })
    }

    async fn sso_url(
        &self,
        account: &AccountRef,
        target: &SsoTarget,
    ) -> Result<SsoLogin, IntegrationError> {
        self.enter("sso_url").await?;
        let state = self.state();
        let a = state
            .accounts
            .get(&account.username)
            .ok_or_else(|| not_found(&account.username))?;
        if a.suspended {
            return Err(IntegrationError::Conflict(format!(
                "mock: account `{}` is suspended",
                a.username
            )));
        }
        if target.service == SsoService::Reseller && !a.reseller {
            return Err(IntegrationError::InvalidInput(format!(
                "mock: `{}` is not a reseller",
                a.username
            )));
        }
        let token = hos_core::crypto::generate_token();
        let mut url = format!(
            "{LOGIN_URL}login/?session={}&service={}&token={token}",
            a.username,
            target.service.as_str()
        );
        if let Some(app) = &target.app {
            url.push_str("&app=");
            url.push_str(&url::form_urlencoded::byte_serialize(app.as_bytes()).collect::<String>());
        }
        Ok(SsoLogin {
            url: SecretString::from(url),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(15)),
        })
    }

    async fn get_account(&self, account: &AccountRef) -> Result<AccountInfo, IntegrationError> {
        self.enter("get_account").await?;
        self.state()
            .accounts
            .get(&account.username)
            .map(info)
            .ok_or_else(|| not_found(&account.username))
    }

    async fn list_accounts(&self) -> Result<Vec<AccountInfo>, IntegrationError> {
        self.enter("list_accounts").await?;
        Ok(self.state().accounts.values().map(info).collect())
    }
}
