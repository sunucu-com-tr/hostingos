//! DirectAdmin via the classic `CMD_API_*` commands.
//!
//! * Auth: HTTP basic auth with the admin/reseller username and a login key
//!   (or password). "Login as" another user is `admin|user` as the username.
//! * Responses are URL-encoded (`error=1&text=...&details=...`, lists as
//!   `list[]=a&list[]=b`); every value may be percent-encoded. Errors come with
//!   HTTP 200, 401 (`Not logged in`), 403 (`Access Denied`) or 500.
//! * One-time login links use the newer JSON endpoint
//!   `POST /api/login-keys/urls`, falling back to the legacy
//!   `CMD_API_LOGIN_KEYS type=one_time_url` on servers without it.
//!
//! Reference: <https://docs.directadmin.com/developer/api/legacy-api.html>

use std::collections::HashSet;
use std::net::IpAddr;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::panel::{
    AccountCreated, AccountInfo, AccountRef, AccountSpec, AccountUsage, PanelLimits, PanelPackage,
    SsoLogin, SsoService, SsoTarget,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, HostingPanel, IntegrationDescriptor, SelectOption,
    kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::Method;
use serde_json::{Value, json};

use crate::http::{PanelHttp, Reply};
use crate::settings::{CommonSettings, connection_fields, provisioning_fields};
use crate::units::{limit_str, size_mib_str};
use crate::username::UsernameRules;

const DEFAULT_PORT: u16 = 2222;
/// Lifetime requested for one-time login links.
const SSO_MINUTES: i64 = 15;

/// Suspension reasons DirectAdmin accepts (1.40+; free text was dropped in 1.714).
const SUSPEND_REASONS: &[&str] = &[
    "user_bandwidth",
    "user_quota",
    "domain_bandwidth",
    "domain_quota",
    "reseller_bandwidth",
    "reseller_quota",
    "billing",
    "abuse",
    "spam",
    "other",
];

/// Admin form schema.
pub fn descriptor() -> IntegrationDescriptor {
    let d = IntegrationDescriptor::new(
        kinds::DIRECTADMIN,
        "DirectAdmin",
        IntegrationCategory::Panel,
    )
    .description(
        "Provisions DirectAdmin users and resellers through the CMD_API interface. \
             Use a login key with the CMD_API_* commands, CMD_LOGIN_KEYS and the JSON API allowed.",
    )
    .docs_url("https://docs.directadmin.com/developer/api/legacy-api.html");
    let d = connection_fields(d, DEFAULT_PORT, "server.example.com");
    let d = d.config_field(
        FieldSpec::text("username", "Admin / reseller username")
            .required()
            .default_value("admin"),
    );
    let d = provisioning_fields(d, true)
        .config_field(
            FieldSpec::select(
                "reseller_ip_mode",
                "IP for new resellers",
                vec![
                    SelectOption::new("shared", "Server shared IP"),
                    SelectOption::new("sharedreseller", "Shared reseller IP"),
                    SelectOption::new("assign", "Assign a free dedicated IP"),
                ],
            )
            .default_value("shared"),
        )
        .config_field(
            FieldSpec::boolean("notify_user", "Let DirectAdmin email the new user")
                .default_value(false),
        );
    d.secret_field(
        FieldSpec::password("login_key", "Login key or password")
            .required()
            .help(
                "Login key from Account Manager » Login Keys (recommended) or the account password",
            ),
    )
}

/// DirectAdmin client.
#[derive(Debug)]
pub struct DirectAdminPanel {
    http: PanelHttp,
    settings: CommonSettings,
    admin_user: String,
    login_key: SecretString,
    reseller_ip_mode: String,
    notify_user: bool,
}

/// A parsed URL-encoded response.
#[derive(Debug, Default)]
struct DaResponse {
    pairs: Vec<(String, String)>,
}

impl DaResponse {
    fn parse(body: &str) -> Self {
        Self {
            pairs: url::form_urlencoded::parse(body.trim().as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    }

    fn list(&self) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == "list[]" || k == "list")
            .map(|(_, v)| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .collect()
    }

    fn is_error(&self) -> bool {
        self.get("error").is_some_and(|e| e != "0")
    }

    /// `text: details` of an error response.
    fn message(&self) -> String {
        match (self.get("text"), self.get("details")) {
            (Some(t), Some(d)) if d != "none" => format!("{t}: {d}"),
            (Some(t), _) => t.to_owned(),
            (None, Some(d)) => d.to_owned(),
            (None, None) => "DirectAdmin reported an error".to_owned(),
        }
    }

    fn limit(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(limit_str)
    }

    fn mb(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| size_mib_str(v, 1.0))
    }
}

fn map_reason(reason: Option<&str>) -> &'static str {
    let reason = reason.unwrap_or_default().trim().to_lowercase();
    if let Some(r) = SUSPEND_REASONS.iter().find(|r| **r == reason) {
        return r;
    }
    if reason.contains("abuse") {
        "abuse"
    } else if reason.contains("spam") {
        "spam"
    } else if [
        "bill", "pay", "invoice", "overdue", "unpaid", "ödeme", "fatura",
    ]
    .iter()
    .any(|k| reason.contains(k))
    {
        "billing"
    } else {
        "other"
    }
}

fn package_limits(r: &DaResponse) -> PanelLimits {
    PanelLimits {
        disk_mb: r.mb("quota"),
        bandwidth_mb: r.mb("bandwidth"),
        domains: r.limit("vdomains"),
        subdomains: r.limit("nsubdomains"),
        alias_domains: r.limit("domainptr"),
        email_accounts: r.limit("nemails"),
        databases: r.limit("mysql"),
        ftp_accounts: r.limit("ftp"),
        inodes: r.limit("inode"),
        reseller_accounts: r.limit("nusers"),
    }
}

/// `date_created` such as `Mon Oct 24 11:14:11 2022` (server local time,
/// treated as UTC).
fn parse_da_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    NaiveDateTime::parse_from_str(&s, "%a %b %d %H:%M:%S %Y")
        .ok()
        .map(|n| n.and_utc())
}

/// Custom-limit form fields (`uX=ON` means unlimited) plus the default
/// feature switches; DirectAdmin treats omitted checkboxes as OFF.
fn custom_limit_params(limits: &PanelLimits, shell: bool) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut put = |key: &'static str, unlimited_key: &'static str, v: Option<u64>| match v {
        Some(n) => out.push((key, n.to_string())),
        None => out.push((unlimited_key, "ON".to_owned())),
    };
    put("bandwidth", "ubandwidth", limits.bandwidth_mb);
    put("quota", "uquota", limits.disk_mb);
    put("inode", "uinode", limits.inodes);
    put("vdomains", "uvdomains", limits.domains);
    put("nsubdomains", "unsubdomains", limits.subdomains);
    put("domainptr", "udomainptr", limits.alias_domains);
    put("nemails", "unemails", limits.email_accounts);
    put("mysql", "umysql", limits.databases);
    put("ftp", "uftp", limits.ftp_accounts);
    for unlimited in ["unemailf", "unemailml", "unemailr"] {
        out.push((unlimited, "ON".to_owned()));
    }
    for feature in [
        "aftp",
        "cgi",
        "php",
        "spam",
        "cron",
        "catchall",
        "ssl",
        "sysinfo",
        "login_keys",
        "dnscontrol",
    ] {
        out.push((feature, "ON".to_owned()));
    }
    if shell {
        out.push(("ssh", "ON".to_owned()));
    }
    out
}

impl DirectAdminPanel {
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
        let admin_user = cfg.string_or("username", "admin")?;
        if admin_user.contains(['|', ':', ' ']) {
            return Err(IntegrationError::Config(
                "config.username must not contain '|', ':' or spaces".into(),
            ));
        }
        let login_key = sec.secret("login_key")?;
        let http = PanelHttp::new(
            kinds::DIRECTADMIN,
            &settings.endpoint,
            vec![login_key.clone()],
        )?;
        Ok(Self {
            http,
            admin_user,
            login_key,
            reseller_ip_mode: cfg.string_or("reseller_ip_mode", "shared")?,
            notify_user: cfg.bool_or("notify_user", false)?,
            settings,
        })
    }

    fn auth_user(&self, as_user: Option<&str>) -> String {
        match as_user {
            Some(u) => format!("{}|{u}", self.admin_user),
            None => self.admin_user.clone(),
        }
    }

    /// Calls a `CMD_API_*` command. GET sends `params` as the query string
    /// (never use it with secrets); POST sends them form-encoded.
    async fn cmd(
        &self,
        method: Method,
        command: &'static str,
        params: &[(&str, String)],
        secrets: &[&str],
    ) -> Result<DaResponse, IntegrationError> {
        let req = self
            .http
            .request(method.clone(), command)?
            .basic_auth(self.auth_user(None), Some(self.login_key.expose_str()));
        let req = if method == Method::GET {
            req.query(params)
        } else {
            req.form(params)
        };
        let reply = self.http.send(command, req).await?;
        self.check(command, &reply, secrets)
    }

    fn check(
        &self,
        command: &str,
        reply: &Reply,
        secrets: &[&str],
    ) -> Result<DaResponse, IntegrationError> {
        let status = reply.status.as_u16();
        if reply.status.is_redirection() {
            // DirectAdmin redirects unknown commands to `/`.
            return Err(IntegrationError::remote(
                Some(status),
                format!("directadmin: {command} is not available on this server"),
            ));
        }
        if reply.is_html() {
            return Err(IntegrationError::Auth(format!(
                "directadmin: {command} returned an HTML page instead of an API response \
                 (wrong credentials, or the login key may not use this command)"
            )));
        }
        let parsed = DaResponse::parse(&reply.body);
        match status {
            401 | 403 => {
                let msg = if parsed.is_error() {
                    parsed.message()
                } else {
                    "authentication failed".to_owned()
                };
                Err(IntegrationError::Auth(
                    self.http
                        .scrub(&format!("directadmin: {command}: {msg}"), secrets),
                ))
            }
            _ if parsed.is_error() => Err(self.http.business_error(
                &format!("directadmin: {command}: {}", parsed.message()),
                None,
                secrets,
            )),
            _ if !reply.status.is_success() => Err(self.http.status_error(reply, None, secrets)),
            _ => Ok(parsed),
        }
    }

    async fn user_config(&self, user: &str) -> Result<DaResponse, IntegrationError> {
        self.cmd(
            Method::GET,
            "CMD_API_SHOW_USER_CONFIG",
            &[("user", user.to_owned())],
            &[],
        )
        .await
    }

    async fn existing_users(&self) -> Result<HashSet<String>, IntegrationError> {
        let all = match self
            .cmd(Method::GET, "CMD_API_SHOW_ALL_USERS", &[], &[])
            .await
        {
            Ok(r) => r,
            // Resellers may not list every user; list their own.
            Err(IntegrationError::Auth(_)) => {
                self.cmd(Method::GET, "CMD_API_SHOW_USERS", &[], &[])
                    .await?
            }
            Err(e) => return Err(e),
        };
        let mut users: HashSet<String> = all.list().into_iter().collect();
        if let Ok(resellers) = self
            .cmd(Method::GET, "CMD_API_SHOW_RESELLERS", &[], &[])
            .await
        {
            users.extend(resellers.list());
        }
        Ok(users)
    }

    async fn pick_ip(&self, spec: &AccountSpec) -> Result<IpAddr, IntegrationError> {
        if let Some(ip) = spec.ip.or(self.settings.default_ip) {
            return Ok(ip);
        }
        let ips = self
            .cmd(Method::GET, "CMD_API_SHOW_RESELLER_IPS", &[], &[])
            .await?;
        ips.list()
            .iter()
            .find_map(|ip| ip.parse().ok())
            .ok_or_else(|| {
                IntegrationError::Config(
                    "directadmin: no IP available; set config.default_ip".into(),
                )
            })
    }

    async fn package_details(
        &self,
        command: &'static str,
        name: &str,
        reseller: bool,
    ) -> Result<PanelPackage, IntegrationError> {
        let r = self
            .cmd(Method::GET, command, &[("package", name.to_owned())], &[])
            .await?;
        let extra: serde_json::Map<String, Value> = r
            .pairs
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        Ok(PanelPackage {
            id: name.to_owned(),
            name: name.to_owned(),
            limits: package_limits(&r),
            reseller,
            extra: Value::Object(extra),
        })
    }

    fn info_from_config(&self, username: &str, c: &DaResponse) -> AccountInfo {
        let extra: serde_json::Map<String, Value> = c
            .pairs
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        AccountInfo {
            account: AccountRef {
                username: username.to_owned(),
                domain: c.get("domain").map(str::to_owned),
                id: None,
            },
            package: c.get("package").map(str::to_owned),
            email: c.get("email").map(str::to_owned),
            ip: c.get("ip").and_then(|ip| ip.parse().ok()),
            suspended: c.get("suspended") == Some("yes"),
            suspend_reason: c
                .get("suspended_reason")
                .or_else(|| c.get("reason"))
                .map(str::to_owned),
            owner: c.get("creator").map(str::to_owned),
            created_at: c.get("date_created").and_then(parse_da_date),
            extra: Value::Object(extra),
        }
    }

    async fn select_users(
        &self,
        action: &[(&str, String)],
        username: &str,
    ) -> Result<(), IntegrationError> {
        let mut params: Vec<(&str, String)> = vec![
            ("location", "CMD_SELECT_USERS".to_owned()),
            ("select0", username.to_owned()),
        ];
        params.extend(action.iter().cloned());
        self.cmd(Method::POST, "CMD_API_SELECT_USERS", &params, &[])
            .await
            .map(drop)
    }

    /// Legacy one-time login URL (`CMD_API_LOGIN_KEYS`, DirectAdmin 1.59+).
    /// Needs the acting admin's real password as `passwd`, so it only works
    /// when the configured secret is the password rather than a login key.
    async fn legacy_login_url(
        &self,
        account: &AccountRef,
        redirect: Option<&str>,
    ) -> Result<SsoLogin, IntegrationError> {
        let key = crate::password::PasswordPolicy {
            length: 32,
            symbols: "",
        }
        .generate();
        let mut params: Vec<(&str, String)> = vec![
            ("action", "create".into()),
            ("type", "one_time_url".into()),
            ("key", key.expose_str().to_owned()),
            ("key2", key.expose_str().to_owned()),
            ("passwd", self.login_key.expose_str().to_owned()),
            ("expiry", format!("{SSO_MINUTES}m")),
        ];
        if let Some(r) = redirect {
            params.push(("redirect-url", r.to_owned()));
        }
        let req = self
            .http
            .request(Method::POST, "CMD_API_LOGIN_KEYS")?
            .basic_auth(
                self.auth_user(Some(&account.username)),
                Some(self.login_key.expose_str()),
            )
            .form(&params);
        let reply = self.http.send("CMD_API_LOGIN_KEYS", req).await?;
        let r = self.check("CMD_API_LOGIN_KEYS", &reply, &[key.expose_str()])?;
        let url = r
            .get("details")
            .filter(|u| u.starts_with("http"))
            .ok_or_else(|| {
                IntegrationError::Transport(
                    "directadmin: CMD_API_LOGIN_KEYS returned no URL".into(),
                )
            })?;
        Ok(SsoLogin {
            url: SecretString::from(url.to_owned()),
            expires_at: Some(Utc::now() + chrono::Duration::minutes(SSO_MINUTES)),
        })
    }
}

#[async_trait]
impl HostingPanel for DirectAdminPanel {
    fn kind(&self) -> &'static str {
        kinds::DIRECTADMIN
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        // Valid for admins and resellers; proves the credentials work.
        let pkgs = self
            .cmd(Method::GET, "CMD_API_PACKAGES_USER", &[], &[])
            .await?;
        // Version is admin-only and best-effort.
        let req = self
            .http
            .request(Method::GET, "api/version")?
            .basic_auth(self.auth_user(None), Some(self.login_key.expose_str()));
        let version = match self.http.send("api/version", req).await {
            Ok(reply) if reply.status.is_success() => serde_json::from_str::<Value>(&reply.body)
                .ok()
                .and_then(|v| v["version"].as_str().map(|s| format!("DirectAdmin {s}"))),
            _ => None,
        };
        Ok(ConnectionInfo {
            server_version: version,
            details: Some(format!(
                "authenticated as {}; {} user packages",
                self.admin_user,
                pkgs.list().len()
            )),
        })
    }

    async fn list_packages(&self) -> Result<Vec<PanelPackage>, IntegrationError> {
        let mut out = Vec::new();
        let users = self
            .cmd(Method::GET, "CMD_API_PACKAGES_USER", &[], &[])
            .await?;
        for name in users.list() {
            out.push(
                self.package_details("CMD_API_PACKAGES_USER", &name, false)
                    .await?,
            );
        }
        // Reseller packages exist only for admins.
        match self
            .cmd(Method::GET, "CMD_API_PACKAGES_RESELLER", &[], &[])
            .await
        {
            Ok(resellers) => {
                for name in resellers.list() {
                    out.push(
                        self.package_details("CMD_API_PACKAGES_RESELLER", &name, true)
                            .await?,
                    );
                }
            }
            Err(IntegrationError::Auth(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(out)
    }

    async fn create_account(&self, spec: &AccountSpec) -> Result<AccountCreated, IntegrationError> {
        let domain = spec.domain.trim().to_lowercase();
        if domain.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "directadmin: domain is required".into(),
            ));
        }
        if spec.owner.email.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "directadmin: owner email is required".into(),
            ));
        }
        if spec.password.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "directadmin: password is required (see hos_panels::password)".into(),
            ));
        }
        let rules = UsernameRules::DIRECTADMIN;
        let username = if spec.username.trim().is_empty() {
            let taken = self.existing_users().await?;
            rules.generate(&domain, |u| taken.contains(u))?
        } else {
            let u = spec.username.trim().to_owned();
            rules.check(&u)?;
            u
        };
        let password = spec.password.expose_str();
        let package = spec.package.trim();

        let mut params: Vec<(&str, String)> = vec![
            ("action", "create".into()),
            ("add", "Submit".into()),
            ("username", username.clone()),
            ("email", spec.owner.email.trim().to_owned()),
            ("passwd", password.to_owned()),
            ("passwd2", password.to_owned()),
            ("domain", domain.clone()),
            ("notify", if self.notify_user { "yes" } else { "no" }.into()),
        ];
        match (&spec.limit_overrides, package.is_empty()) {
            (Some(_), false) => {
                return Err(IntegrationError::InvalidInput(
                    "directadmin: limit_overrides cannot be combined with a package; \
                     create a dedicated package instead"
                        .into(),
                ));
            }
            (Some(limits), true) => {
                params.extend(custom_limit_params(limits, spec.shell_access));
                if let Some(locale) = spec.locale {
                    params.push(("language", locale.as_str().to_owned()));
                }
            }
            (None, false) => params.push(("package", package.to_owned())),
            (None, true) => {
                return Err(IntegrationError::InvalidInput(
                    "directadmin: a package or limit_overrides is required".into(),
                ));
            }
        }

        let (command, ip) = if spec.reseller {
            let mode = if spec.ip.is_some() {
                "assign".to_owned()
            } else {
                self.reseller_ip_mode.clone()
            };
            params.push(("ip", mode));
            ("CMD_API_ACCOUNT_RESELLER", None)
        } else {
            let ip = self.pick_ip(spec).await?;
            params.push(("ip", ip.to_string()));
            ("CMD_API_ACCOUNT_USER", Some(ip))
        };
        let created = self
            .cmd(Method::POST, command, &params, &[password])
            .await?;

        Ok(AccountCreated {
            account: AccountRef {
                username,
                domain: Some(domain),
                id: None,
            },
            ip,
            nameservers: self.settings.nameservers.clone(),
            login_url: Some(self.settings.login_url_or(DEFAULT_PORT, DEFAULT_PORT)),
            extra: json!({ "message": created.get("text") }),
        })
    }

    async fn suspend(
        &self,
        account: &AccountRef,
        reason: Option<&str>,
    ) -> Result<(), IntegrationError> {
        self.select_users(
            &[
                ("dosuspend", "yes".into()),
                ("reason", map_reason(reason).into()),
            ],
            &account.username,
        )
        .await
    }

    async fn unsuspend(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        self.select_users(&[("dounsuspend", "yes".into())], &account.username)
            .await
    }

    async fn terminate(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        self.select_users(
            &[("confirmed", "Confirm".into()), ("delete", "yes".into())],
            &account.username,
        )
        .await
    }

    async fn change_package(
        &self,
        account: &AccountRef,
        package: &str,
    ) -> Result<(), IntegrationError> {
        if package.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "directadmin: package is required".into(),
            ));
        }
        let config = self.user_config(&account.username).await?;
        let command = if config.get("usertype") == Some("reseller") {
            "CMD_API_MODIFY_RESELLER"
        } else {
            "CMD_API_MODIFY_USER"
        };
        self.cmd(
            Method::POST,
            command,
            &[
                ("action", "package".into()),
                ("user", account.username.clone()),
                ("package", package.trim().to_owned()),
            ],
            &[],
        )
        .await
        .map(drop)
    }

    async fn change_password(
        &self,
        account: &AccountRef,
        password: &SecretString,
    ) -> Result<(), IntegrationError> {
        if password.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "directadmin: password is required".into(),
            ));
        }
        let pw = password.expose_str();
        self.cmd(
            Method::POST,
            "CMD_API_USER_PASSWD",
            &[
                ("username", account.username.clone()),
                ("passwd", pw.to_owned()),
                ("passwd2", pw.to_owned()),
            ],
            &[pw],
        )
        .await
        .map(drop)
    }

    async fn usage(&self, account: &AccountRef) -> Result<AccountUsage, IntegrationError> {
        let usage = self
            .cmd(
                Method::GET,
                "CMD_API_SHOW_USER_USAGE",
                &[("user", account.username.clone())],
                &[],
            )
            .await?;
        let config = self.user_config(&account.username).await?;
        Ok(AccountUsage {
            collected_at: Utc::now(),
            disk_used_mb: usage.mb("quota"),
            bandwidth_used_mb: usage.mb("bandwidth"),
            domains: usage.limit("vdomains"),
            email_accounts: usage.limit("nemails"),
            databases: usage.limit("mysql"),
            ftp_accounts: usage.limit("ftp"),
            inodes: usage.limit("inode"),
            limits: package_limits(&config),
        })
    }

    async fn sso_url(
        &self,
        account: &AccountRef,
        target: &SsoTarget,
    ) -> Result<SsoLogin, IntegrationError> {
        if target.service == SsoService::Webmail {
            return Err(IntegrationError::Unsupported("sso_url(webmail)"));
        }
        let redirect = target
            .app
            .as_deref()
            .filter(|a| a.starts_with('/') && !a.starts_with("//"));
        let expires = Utc::now() + chrono::Duration::minutes(SSO_MINUTES);
        let mut body =
            json!({ "expires": expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) });
        if let Some(r) = redirect {
            body["redirectURL"] = Value::String(r.to_owned());
        }
        let req = self
            .http
            .request(Method::POST, "api/login-keys/urls")?
            .basic_auth(
                self.auth_user(Some(&account.username)),
                Some(self.login_key.expose_str()),
            )
            .json(&body);
        let reply = self.http.send("api/login-keys/urls", req).await?;
        match reply.status.as_u16() {
            200..=299 => {
                let v: Value = serde_json::from_str(&reply.body)
                    .map_err(|e| self.http.parse_error("api/login-keys/urls", &reply, e))?;
                let url = v["url"].as_str().filter(|u| !u.is_empty()).ok_or_else(|| {
                    IntegrationError::Transport("directadmin: login URL response has no url".into())
                })?;
                let expires_at = v["expires"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc))
                    .or(Some(expires));
                Ok(SsoLogin {
                    url: SecretString::from(url.to_owned()),
                    expires_at,
                })
            }
            // Older servers: no JSON API (404) or redirect to `/`.
            301..=308 | 404 | 405 => self.legacy_login_url(account, redirect).await,
            _ => {
                let v: Option<Value> = serde_json::from_str(&reply.body).ok();
                let message = v.as_ref().and_then(|v| {
                    v["message"]
                        .as_str()
                        .or_else(|| v["error"].as_str())
                        .or_else(|| v["type"].as_str())
                        .map(str::to_owned)
                });
                Err(self.http.status_error(&reply, message.as_deref(), &[]))
            }
        }
    }

    async fn get_account(&self, account: &AccountRef) -> Result<AccountInfo, IntegrationError> {
        let config = self.user_config(&account.username).await?;
        Ok(self.info_from_config(&account.username, &config))
    }

    async fn list_accounts(&self) -> Result<Vec<AccountInfo>, IntegrationError> {
        let mut users: Vec<String> = self.existing_users().await?.into_iter().collect();
        users.sort();
        let mut out = Vec::with_capacity(users.len());
        for user in users {
            let config = self.user_config(&user).await?;
            out.push(self.info_from_config(&user, &config));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_encoded_responses() {
        let r = DaResponse::parse("error=1&text=Cannot%20Execute%20Command&details=User+exists");
        assert!(r.is_error());
        assert_eq!(r.message(), "Cannot Execute Command: User exists");
        let l = DaResponse::parse("list[]=alice&list[]=bob&list%5B%5D=carol");
        assert_eq!(l.list(), vec!["alice", "bob", "carol"]);
        let u = DaResponse::parse("bandwidth=%31%30%30%30&quota=50.2&vdomains=unlimited");
        assert_eq!(u.mb("bandwidth"), Some(1000));
        assert_eq!(u.mb("quota"), Some(50));
        assert_eq!(u.limit("vdomains"), None);
    }

    #[test]
    fn reasons_and_dates() {
        assert_eq!(map_reason(Some("billing")), "billing");
        assert_eq!(map_reason(Some("Overdue invoice #42")), "billing");
        assert_eq!(map_reason(Some("Spam complaints")), "spam");
        assert_eq!(map_reason(None), "other");
        assert_eq!(
            parse_da_date("Mon Oct 24 11:14:11 2022").map(|d| d.to_rfc3339()),
            Some("2022-10-24T11:14:11+00:00".into())
        );
    }

    #[test]
    fn custom_limits_use_unlimited_switches() {
        let p = custom_limit_params(
            &PanelLimits {
                disk_mb: Some(1024),
                ..PanelLimits::default()
            },
            false,
        );
        assert!(p.contains(&("quota", "1024".into())));
        assert!(p.contains(&("ubandwidth", "ON".into())));
        assert!(p.contains(&("php", "ON".into())));
        assert!(!p.iter().any(|(k, _)| *k == "ssh"));
    }
}
