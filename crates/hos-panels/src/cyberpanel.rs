//! CyberPanel via its `/api/*` JSON endpoints.
//!
//! * Every call is `POST /api/<endpoint>` with a JSON body carrying
//!   `adminUser`/`adminPass` (the admin needs "API access" enabled and no
//!   2FA). Failures usually come back as HTTP 200 with the endpoint's status
//!   key set to `0` and `error_message`; `verifyConn` uses real HTTP statuses.
//! * A HostingOS "account" is a website (`domainName`) owned by a CyberPanel
//!   user (`websiteOwner` = [`AccountRef::username`]). Suspension, deletion and
//!   package changes act on the website, so [`AccountRef::domain`] is required.
//! * CyberPanel's security middleware rejects any `/api/` value containing
//!   `;`, `&&`, `||`, `|`, `...`, `` ` ``, `$`, `../` or newlines; such
//!   passwords are refused locally with `InvalidInput`.
//! * There is no token SSO (`/api/loginAPI` needs the user's password posted
//!   from the browser) → `sso_url` is `Unsupported`. Usage comes from
//!   `/cloudAPI/` (`fetchWebsiteData`), which only the user named `admin` may
//!   call with the token returned by `verifyConn`.
//!
//! Reference: `api/views.py`, `cloudAPI/cloudManager.py` in
//! <https://github.com/usmannasir/cyberpanel> (stable branch).

use async_trait::async_trait;
use chrono::Utc;
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::panel::{
    AccountCreated, AccountRef, AccountSpec, AccountUsage, PanelLimits, PanelPackage, SsoLogin,
    SsoTarget,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, HostingPanel, IntegrationDescriptor, kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::http::PanelHttp;
use crate::password::cyberpanel_rejects;
use crate::settings::{CommonSettings, connection_fields, provisioning_fields};
use crate::units::{flag, limit, size_mib, text};
use crate::username::UsernameRules;

const DEFAULT_PORT: u16 = 8090;
const MAX_USERNAME_PROBES: usize = 10;
/// `createWebsite` keys set by this adapter; `AccountSpec::extra` may not override them.
const MANAGED_KEYS: &[&str] = &[
    "adminUser",
    "adminPass",
    "domainName",
    "ownerEmail",
    "packageName",
    "websiteOwner",
    "ownerPassword",
    "acl",
];

/// Admin form schema.
pub fn descriptor() -> IntegrationDescriptor {
    let d = IntegrationDescriptor::new(kinds::CYBERPANEL, "CyberPanel", IntegrationCategory::Panel)
        .description(
            "Creates CyberPanel websites and their owner users through the CyberPanel API. \
             Enable API access for the admin user (Users » API Access); 2FA must be off.",
        )
        .docs_url("https://cyberpanel.net/KnowledgeBase/home/cyberpanel-api/");
    let d = connection_fields(d, DEFAULT_PORT, "server.example.com");
    let d = d.config_field(
        FieldSpec::text("username", "Admin username")
            .required()
            .default_value("admin")
            .help("Usage statistics are only available when this is the user named `admin`"),
    );
    let d = provisioning_fields(d, false)
        .config_field(
            FieldSpec::text("php_version", "PHP version for new websites")
                .placeholder("PHP 8.2")
                .help("Exact CyberPanel label; CyberPanel's default is used when empty"),
        )
        .config_field(
            FieldSpec::boolean(
                "delete_owner_on_terminate",
                "Delete the owner user on termination",
            )
            .default_value(true)
            .help("Only succeeds when the user owns no other websites"),
        );
    d.secret_field(FieldSpec::password("password", "Admin password").required())
}

/// CyberPanel client.
#[derive(Debug)]
pub struct CyberPanel {
    http: PanelHttp,
    settings: CommonSettings,
    admin_user: String,
    admin_pass: SecretString,
    php_version: Option<String>,
    delete_owner: bool,
}

impl CyberPanel {
    /// Builds the client from `integrations.config` / decrypted secrets,
    /// under the process-wide outbound policy (`HOS_OUTBOUND_POLICY`).
    pub fn from_config(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        Self::from_config_with_policy(config, secrets, OutboundPolicy::global())
    }

    /// [`from_config`](Self::from_config) with an explicit outbound policy.
    pub fn from_config_with_policy(
        config: &Value,
        secrets: &Value,
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        let cfg = FieldReader::config(config);
        let sec = FieldReader::secrets(secrets);
        let settings = CommonSettings::read(&cfg, DEFAULT_PORT, policy)?;
        let admin_pass = sec.secret("password")?;
        if cyberpanel_rejects(admin_pass.expose_str()) {
            return Err(IntegrationError::Config(
                "secrets.password contains characters CyberPanel's API rejects (; | ` $ && ...)"
                    .into(),
            ));
        }
        let http = PanelHttp::new(
            kinds::CYBERPANEL,
            &settings.endpoint,
            vec![admin_pass.clone()],
        )?;
        Ok(Self {
            http,
            admin_user: cfg.string_or("username", "admin")?,
            admin_pass,
            php_version: cfg.opt_str("php_version")?.map(str::to_owned),
            delete_owner: cfg.bool_or("delete_owner_on_terminate", true)?,
            settings,
        })
    }

    /// POSTs to `/api/<endpoint>` and checks `status_key == 1`.
    async fn api(
        &self,
        endpoint: &'static str,
        mut body: Map<String, Value>,
        status_key: &str,
        secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        body.insert("adminUser".into(), Value::String(self.admin_user.clone()));
        body.insert(
            "adminPass".into(),
            Value::String(self.admin_pass.expose_str().to_owned()),
        );
        let value = self
            .post(endpoint, &format!("api/{endpoint}"), &body, None, secrets)
            .await?;
        self.status_ok(endpoint, value, status_key, secrets)
    }

    /// POSTs JSON and parses the reply (HTTP errors mapped).
    async fn post(
        &self,
        op: &'static str,
        path: &str,
        body: &Map<String, Value>,
        authorization: Option<&str>,
        secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        let mut req = self
            .http
            .request(Method::POST, path)?
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body);
        if let Some(a) = authorization {
            req = req.header(reqwest::header::AUTHORIZATION, a);
        }
        let reply = self.http.send(op, req).await?;
        let parsed: Option<Value> = serde_json::from_str(&reply.body).ok();
        if !reply.status.is_success() {
            let message = parsed.as_ref().and_then(error_text);
            let message = message.or_else(|| {
                let t = reply.body.trim();
                (!t.is_empty() && t.len() < 200 && !reply.is_html()).then(|| t.to_owned())
            });
            return Err(self.http.status_error(&reply, message.as_deref(), secrets));
        }
        parsed.ok_or_else(|| self.http.parse_error(op, &reply, "not JSON"))
    }

    fn status_ok(
        &self,
        endpoint: &str,
        value: Value,
        status_key: &str,
        secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        if flag(&value[status_key]) {
            return Ok(value);
        }
        let message = error_text(&value).unwrap_or_else(|| "unspecified error".to_owned());
        if value.get(status_key).is_none() && value.get("status").is_none() {
            // Security middleware reply: no status key at all.
            return Err(IntegrationError::InvalidInput(self.http.scrub(
                &format!("cyberpanel: {endpoint}: request rejected: {message}"),
                secrets,
            )));
        }
        if message
            .to_lowercase()
            .contains("could not authorize access to api")
        {
            return Err(IntegrationError::Auth(format!(
                "cyberpanel: {endpoint}: {message} (check the admin password and that API access is enabled)"
            )));
        }
        Err(self
            .http
            .business_error(&format!("cyberpanel: {endpoint}: {message}"), None, secrets))
    }

    fn domain_of<'a>(&self, account: &'a AccountRef) -> Result<&'a str, IntegrationError> {
        account
            .domain
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| {
                IntegrationError::InvalidInput(
                    "cyberpanel: AccountRef.domain (the website) is required".into(),
                )
            })
    }

    /// Whether a CyberPanel user exists (`getUserInfo`).
    async fn user_exists(&self, username: &str) -> Result<bool, IntegrationError> {
        let mut body = Map::new();
        body.insert("username".into(), Value::String(username.to_owned()));
        match self.api("getUserInfo", body, "status", &[]).await {
            Ok(_) => Ok(true),
            Err(IntegrationError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn api_token(&self) -> Result<Option<String>, IntegrationError> {
        let v = self
            .api("verifyConn", Map::new(), "verifyConn", &[])
            .await?;
        Ok(text(&v["apiToken"]))
    }
}

/// `error_message` / `errorMessage` (CyberPanel writes `"None"` on success).
fn error_text(v: &Value) -> Option<String> {
    text(&v["error_message"])
        .or_else(|| text(&v["errorMessage"]))
        .filter(|m| m != "None")
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

#[async_trait]
impl HostingPanel for CyberPanel {
    fn kind(&self) -> &'static str {
        kinds::CYBERPANEL
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let token = self.api_token().await?;
        // Version uses `username`/`password` keys; best-effort.
        let body = obj(json!({
            "username": self.admin_user,
            "password": self.admin_pass.expose_str(),
        }));
        let version = match self
            .post(
                "cyberPanelVersion",
                "api/cyberPanelVersion",
                &body,
                None,
                &[],
            )
            .await
        {
            Ok(v) if flag(&v["getVersion"]) => {
                match (text(&v["currentVersion"]), text(&v["build"])) {
                    (Some(ver), Some(build)) => Some(format!("CyberPanel {ver}.{build}")),
                    (Some(ver), None) => Some(format!("CyberPanel {ver}")),
                    _ => None,
                }
            }
            _ => None,
        };
        Ok(ConnectionInfo {
            server_version: version,
            details: Some(format!(
                "authenticated as {}{}",
                self.admin_user,
                if token.is_some() {
                    "; cloud API token available"
                } else {
                    ""
                }
            )),
        })
    }

    async fn list_packages(&self) -> Result<Vec<PanelPackage>, IntegrationError> {
        let mut body = Map::new();
        body.insert("adminUser".into(), Value::String(self.admin_user.clone()));
        body.insert(
            "adminPass".into(),
            Value::String(self.admin_pass.expose_str().to_owned()),
        );
        let v = self
            .post("listPackage", "api/listPackage", &body, None, &[])
            .await?;
        let names: Vec<String> = match &v {
            Value::Array(items) => items
                .iter()
                .filter_map(|i| text(i).or_else(|| text(&i["packageName"])))
                .collect(),
            other => {
                // Error envelope: {"status":0,"listPackages":[],"error_message":...}
                self.status_ok("listPackage", other.clone(), "status", &[])?;
                other["listPackages"]
                    .as_array()
                    .map(|a| a.iter().filter_map(text).collect())
                    .unwrap_or_default()
            }
        };
        Ok(names
            .into_iter()
            .map(|name| PanelPackage {
                id: name.clone(),
                name,
                limits: PanelLimits::default(),
                reseller: false,
                extra: Value::Null,
            })
            .collect())
    }

    async fn create_account(&self, spec: &AccountSpec) -> Result<AccountCreated, IntegrationError> {
        let domain = spec.domain.trim().to_lowercase();
        if domain.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cyberpanel: domain is required".into(),
            ));
        }
        let email = spec.owner.email.trim();
        if email.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cyberpanel: owner email is required".into(),
            ));
        }
        if spec.package.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cyberpanel: package is required (CyberPanel's built-in one is `Default`)".into(),
            ));
        }
        let password = spec.password.expose_str();
        if password.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cyberpanel: password is required (see hos_panels::password)".into(),
            ));
        }
        if cyberpanel_rejects(password) {
            return Err(IntegrationError::InvalidInput(
                "cyberpanel: password contains characters the API rejects (; | ` $ && ... ../)"
                    .into(),
            ));
        }
        let rules = UsernameRules::CYBERPANEL;
        // createWebsite silently reuses an existing owner (without changing
        // its password), which would hand another customer's user over:
        // refuse existing names.
        let username = if spec.username.trim().is_empty() {
            let mut chosen = None;
            for candidate in rules
                .candidates(&domain)
                .into_iter()
                .take(MAX_USERNAME_PROBES)
            {
                if !self.user_exists(&candidate).await? {
                    chosen = Some(candidate);
                    break;
                }
            }
            chosen.ok_or_else(|| {
                IntegrationError::Conflict(format!(
                    "cyberpanel: no free username found for `{domain}`"
                ))
            })?
        } else {
            let u = spec.username.trim().to_lowercase();
            rules.check(&u)?;
            if self.user_exists(&u).await? {
                return Err(IntegrationError::Conflict(format!(
                    "cyberpanel: user `{u}` already exists"
                )));
            }
            u
        };

        let mut body = Map::new();
        if let Some(extra) = spec.extra.as_object() {
            for (k, v) in extra {
                if !MANAGED_KEYS.contains(&k.as_str()) && (v.is_string() || v.is_number()) {
                    body.insert(k.clone(), v.clone());
                }
            }
        }
        body.insert("domainName".into(), Value::String(domain.clone()));
        body.insert("ownerEmail".into(), Value::String(email.to_owned()));
        body.insert(
            "packageName".into(),
            Value::String(spec.package.trim().to_owned()),
        );
        body.insert("websiteOwner".into(), Value::String(username.clone()));
        body.insert("ownerPassword".into(), Value::String(password.to_owned()));
        if spec.reseller {
            body.insert("acl".into(), Value::String("reseller".into()));
        }
        if let Some(n) = spec.limit_overrides.as_ref().and_then(|l| l.domains) {
            body.insert("websitesLimit".into(), json!(n));
        }
        if !body.contains_key("phpSelection")
            && let Some(php) = &self.php_version
        {
            body.insert("phpSelection".into(), Value::String(php.clone()));
        }
        let created = self
            .api("createWebsite", body, "createWebSiteStatus", &[password])
            .await?;
        Ok(AccountCreated {
            account: AccountRef {
                username,
                domain: Some(domain),
                id: None,
            },
            ip: None,
            nameservers: self.settings.nameservers.clone(),
            login_url: Some(self.settings.login_url_or(DEFAULT_PORT, DEFAULT_PORT)),
            extra: json!({
                // Website creation finishes in the background.
                "status_path": created["tempStatusPath"],
                "linux_user": created["LinuxUser"],
            }),
        })
    }

    async fn suspend(
        &self,
        account: &AccountRef,
        _reason: Option<&str>,
    ) -> Result<(), IntegrationError> {
        let domain = self.domain_of(account)?;
        let body = obj(json!({ "websiteName": domain, "state": "Suspend" }));
        self.api("submitWebsiteStatus", body, "websiteStatus", &[])
            .await
            .map(drop)
    }

    async fn unsuspend(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        let domain = self.domain_of(account)?;
        let body = obj(json!({ "websiteName": domain, "state": "Unsuspend" }));
        self.api("submitWebsiteStatus", body, "websiteStatus", &[])
            .await
            .map(drop)
    }

    async fn terminate(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        let domain = self.domain_of(account)?;
        let body = obj(json!({ "domainName": domain }));
        let mut full = body.clone();
        full.insert("adminUser".into(), Value::String(self.admin_user.clone()));
        full.insert(
            "adminPass".into(),
            Value::String(self.admin_pass.expose_str().to_owned()),
        );
        let v = self
            .post("deleteWebsite", "api/deleteWebsite", &full, None, &[])
            .await?;
        // 1 = deleted, 2 = deletion queued (child domains pending).
        match v["websiteDeleteStatus"].as_i64() {
            Some(1 | 2) => {}
            _ => {
                self.status_ok("deleteWebsite", v, "websiteDeleteStatus", &[])?;
            }
        }
        if self.delete_owner && !account.username.trim().is_empty() {
            let body = obj(json!({ "accountUsername": account.username }));
            if let Err(e) = self
                .api("submitUserDeletion", body, "deleteStatus", &[])
                .await
            {
                // The website is gone; a leftover user is not worth failing the job.
                tracing::warn!(panel = "cyberpanel", user = %account.username, error = %e, "owner user not deleted");
            }
        }
        Ok(())
    }

    async fn change_package(
        &self,
        account: &AccountRef,
        package: &str,
    ) -> Result<(), IntegrationError> {
        let domain = self.domain_of(account)?;
        if package.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cyberpanel: package is required".into(),
            ));
        }
        let body = obj(json!({ "websiteName": domain, "packageName": package.trim() }));
        self.api("changePackageAPI", body, "changePackage", &[])
            .await
            .map(drop)
    }

    async fn change_password(
        &self,
        account: &AccountRef,
        password: &SecretString,
    ) -> Result<(), IntegrationError> {
        let pw = password.expose_str();
        if pw.is_empty() || cyberpanel_rejects(pw) {
            return Err(IntegrationError::InvalidInput(
                "cyberpanel: password is empty or contains characters the API rejects".into(),
            ));
        }
        let body = obj(json!({ "websiteOwner": account.username, "ownerPassword": pw }));
        self.api("changeUserPassAPI", body, "changeStatus", &[pw])
            .await
            .map(drop)
    }

    async fn usage(&self, account: &AccountRef) -> Result<AccountUsage, IntegrationError> {
        let domain = self.domain_of(account)?;
        if self.admin_user != "admin" {
            return Err(IntegrationError::Unsupported(
                "usage (CyberPanel cloud API requires the `admin` user)",
            ));
        }
        let Some(token) = self.api_token().await? else {
            return Err(IntegrationError::Unsupported(
                "usage (this CyberPanel version returns no API token)",
            ));
        };
        let body = obj(json!({
            "controller": "fetchWebsiteData",
            "serverUserName": "admin",
            "domainName": domain,
        }));
        let v = self
            .post(
                "fetchWebsiteData",
                "cloudAPI/",
                &body,
                Some(&token),
                &[&token],
            )
            .await?;
        let v = self.status_ok("fetchWebsiteData", v, "status", &[&token])?;
        // Package limits: 0 means unlimited in CyberPanel packages.
        let positive = |v: &Value| limit(v).filter(|n| *n > 0);
        Ok(AccountUsage {
            collected_at: Utc::now(),
            disk_used_mb: size_mib(&v["diskInMB"], 1.0),
            bandwidth_used_mb: size_mib(&v["bwInMB"], 1.0),
            domains: None,
            email_accounts: None,
            databases: limit(&v["dbUsed"]),
            ftp_accounts: limit(&v["ftpUsed"]),
            inodes: None,
            limits: PanelLimits {
                disk_mb: positive(&v["diskInMBTotal"]),
                bandwidth_mb: positive(&v["bwInMBTotal"]),
                databases: positive(&v["dbAllowed"]),
                ftp_accounts: positive(&v["ftpAllowed"]),
                ..PanelLimits::default()
            },
        })
    }

    async fn sso_url(
        &self,
        _account: &AccountRef,
        _target: &SsoTarget,
    ) -> Result<SsoLogin, IntegrationError> {
        Err(IntegrationError::Unsupported("sso_url"))
    }
}
