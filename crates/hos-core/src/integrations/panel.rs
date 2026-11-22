//! Hosting control panels: cPanel/WHM (WHM API 1), Plesk (REST/XML-RPC),
//! DirectAdmin, CyberPanel.
//!
//! An account is addressed by [`AccountRef`]: `username` is the panel login
//! (WHM `user`, DirectAdmin user, Plesk customer/system user); `id` holds the
//! panel's internal id where operations need it (Plesk subscription/customer
//! id). Store both on the service.
//!
//! Resource values use `None` for "unlimited or not reported".

use std::net::IpAddr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ConnectionInfo;
use crate::domain::Locale;
use crate::error::IntegrationError;
use crate::secret::SecretString;

/// Reference to a hosting account at the panel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountRef {
    /// Panel username.
    pub username: String,
    /// Primary domain of the account.
    pub domain: Option<String>,
    /// Panel-internal id (Plesk subscription id), when needed for API calls.
    pub id: Option<String>,
}

impl AccountRef {
    /// Reference by username only.
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            domain: None,
            id: None,
        }
    }
}

/// Resource limits of a package / account. `None` = unlimited or unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelLimits {
    /// Disk quota (MiB).
    pub disk_mb: Option<u64>,
    /// Monthly bandwidth (MiB).
    pub bandwidth_mb: Option<u64>,
    /// Domains (addon + primary).
    pub domains: Option<u64>,
    /// Subdomains.
    pub subdomains: Option<u64>,
    /// Parked / alias domains.
    pub alias_domains: Option<u64>,
    /// Mailboxes.
    pub email_accounts: Option<u64>,
    /// Databases.
    pub databases: Option<u64>,
    /// FTP accounts.
    pub ftp_accounts: Option<u64>,
    /// Inodes / files.
    pub inodes: Option<u64>,
    /// Sub-accounts a reseller may create.
    pub reseller_accounts: Option<u64>,
}

/// A hosting package / service plan defined on the panel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelPackage {
    /// Value to pass as [`AccountSpec::package`] (WHM package name, Plesk plan name/id).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Limits.
    pub limits: PanelLimits,
    /// Whether this is a reseller plan.
    pub reseller: bool,
    /// Provider-specific data.
    pub extra: Value,
}

/// Owner / contact of a new account (Plesk creates a customer from it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountOwner {
    /// Full name.
    pub name: Option<String>,
    /// Company.
    pub company: Option<String>,
    /// Contact email (required by every panel).
    pub email: String,
}

/// Request to create a hosting account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountSpec {
    /// Desired username (implementations may reject invalid ones with `InvalidInput`).
    pub username: String,
    /// Primary domain.
    pub domain: String,
    /// Initial password.
    pub password: SecretString,
    /// Package / plan ([`PanelPackage::id`]).
    pub package: String,
    /// Owner contact.
    pub owner: AccountOwner,
    /// Specific IP to assign (dedicated IP); `None` = shared/default.
    pub ip: Option<IpAddr>,
    /// Create as a reseller account.
    pub reseller: bool,
    /// Grant shell access, where configurable.
    pub shell_access: bool,
    /// Panel UI language.
    pub locale: Option<Locale>,
    /// Per-account overrides of package limits, where the panel allows it.
    pub limit_overrides: Option<PanelLimits>,
    /// Provider-specific options (cPanel `featurelist`, Plesk `owner-login`...).
    pub extra: Value,
}

/// Result of [`HostingPanel::create_account`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCreated {
    /// The new account.
    pub account: AccountRef,
    /// IP assigned to the account.
    pub ip: Option<IpAddr>,
    /// Nameservers the client should use.
    #[serde(default)]
    pub nameservers: Vec<String>,
    /// Panel login URL for the welcome email.
    pub login_url: Option<String>,
    /// Provider-specific data.
    #[serde(default)]
    pub extra: Value,
}

/// Account details as reported by the panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    /// The account.
    pub account: AccountRef,
    /// Package / plan.
    pub package: Option<String>,
    /// Contact email.
    pub email: Option<String>,
    /// Assigned IP.
    pub ip: Option<IpAddr>,
    /// Whether the account is suspended.
    pub suspended: bool,
    /// Suspension reason.
    pub suspend_reason: Option<String>,
    /// Owning reseller, if any.
    pub owner: Option<String>,
    /// Creation time.
    pub created_at: Option<DateTime<Utc>>,
    /// Provider-specific data.
    #[serde(default)]
    pub extra: Value,
}

/// Resource usage of an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUsage {
    /// Sample time.
    pub collected_at: DateTime<Utc>,
    /// Disk used (MiB).
    pub disk_used_mb: Option<u64>,
    /// Bandwidth used this period (MiB).
    pub bandwidth_used_mb: Option<u64>,
    /// Domains in use.
    pub domains: Option<u64>,
    /// Mailboxes in use.
    pub email_accounts: Option<u64>,
    /// Databases in use.
    pub databases: Option<u64>,
    /// FTP accounts in use.
    pub ftp_accounts: Option<u64>,
    /// Inodes in use.
    pub inodes: Option<u64>,
    /// Effective limits.
    #[serde(default)]
    pub limits: PanelLimits,
}

crate::string_enum! {
    /// Which panel UI an SSO link opens.
    #[derive(Default)]
    pub enum SsoService {
        /// The account's control panel (cPanel, Plesk, DirectAdmin user level).
        #[default]
        ControlPanel => "control_panel",
        /// Webmail.
        Webmail => "webmail",
        /// Reseller level (WHM for resellers).
        Reseller => "reseller",
    }
}

/// Where an SSO login should land.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SsoTarget {
    /// Panel UI.
    pub service: SsoService,
    /// Panel-specific app / page to open (cPanel `app=Email_Accounts`).
    pub app: Option<String>,
    /// IP of the browser that will use the link (some panels bind sessions to it).
    pub client_ip: Option<IpAddr>,
    /// UI language.
    pub locale: Option<Locale>,
}

/// A one-time login link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsoLogin {
    /// URL containing a session token — redirect the browser, never log it.
    pub url: SecretString,
    /// Expiry, when known.
    pub expires_at: Option<DateTime<Utc>>,
}

/// A hosting control panel. See the [module docs](self).
#[async_trait]
pub trait HostingPanel: Send + Sync {
    /// The integration kind (e.g. [`kinds::CPANEL`](super::kinds::CPANEL)).
    fn kind(&self) -> &'static str;

    /// Verifies credentials and reachability.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError>;

    /// Lists packages / service plans.
    async fn list_packages(&self) -> Result<Vec<PanelPackage>, IntegrationError>;

    /// Creates an account. Username/domain taken → [`IntegrationError::Conflict`].
    async fn create_account(&self, spec: &AccountSpec) -> Result<AccountCreated, IntegrationError>;

    /// Suspends an account.
    async fn suspend(
        &self,
        account: &AccountRef,
        reason: Option<&str>,
    ) -> Result<(), IntegrationError>;

    /// Unsuspends an account.
    async fn unsuspend(&self, account: &AccountRef) -> Result<(), IntegrationError>;

    /// Deletes an account and its data.
    async fn terminate(&self, account: &AccountRef) -> Result<(), IntegrationError>;

    /// Moves the account to another package.
    async fn change_package(
        &self,
        account: &AccountRef,
        package: &str,
    ) -> Result<(), IntegrationError>;

    /// Sets a new password.
    async fn change_password(
        &self,
        account: &AccountRef,
        password: &SecretString,
    ) -> Result<(), IntegrationError>;

    /// Current resource usage.
    async fn usage(&self, account: &AccountRef) -> Result<AccountUsage, IntegrationError>;

    /// Creates a one-time login URL.
    async fn sso_url(
        &self,
        account: &AccountRef,
        target: &SsoTarget,
    ) -> Result<SsoLogin, IntegrationError>;

    /// Account details. Default: `Unsupported`.
    async fn get_account(&self, account: &AccountRef) -> Result<AccountInfo, IntegrationError> {
        let _ = account;
        Err(IntegrationError::Unsupported("get_account"))
    }

    /// All accounts on the server (import / sync). Default: `Unsupported`.
    async fn list_accounts(&self) -> Result<Vec<AccountInfo>, IntegrationError> {
        Err(IntegrationError::Unsupported("list_accounts"))
    }
}
