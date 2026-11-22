//! cPanel & WHM via WHM API 1.
//!
//! * Endpoint: `POST https://host:2087/json-api/<function>?api.version=1`, form
//!   encoded parameters (keeps passwords out of URLs and access logs).
//! * Auth: `Authorization: whm <user>:<API token>`.
//! * Errors: WHM answers HTTP 200 with `metadata.result == 0` and the message
//!   in `metadata.reason`; a rejected token yields HTTP 401/403 (with an API 2
//!   style `cpanelresult.error` body on some versions).
//!
//! Reference: <https://api.docs.cpanel.net/whm/introduction/>

use std::net::IpAddr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::panel::{
    AccountCreated, AccountInfo, AccountRef, AccountSpec, AccountUsage, PanelLimits, PanelPackage,
    SsoLogin, SsoService, SsoTarget,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, HostingPanel, IntegrationDescriptor, kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::Method;
use serde_json::{Value, json};

use crate::http::PanelHttp;
use crate::settings::{CommonSettings, connection_fields, provisioning_fields};
use crate::units::{bytes_to_mib, flag, limit, size_mib, text};
use crate::username::UsernameRules;

const DEFAULT_PORT: u16 = 2087;
/// cPanel (end-user) ports for login URLs.
const CPANEL_HTTPS_PORT: u16 = 2083;
const CPANEL_HTTP_PORT: u16 = 2082;
/// How many generated usernames are checked with `verify_new_username`.
const MAX_USERNAME_PROBES: usize = 12;

/// WHM functions an API token must be allowed to call for provisioning.
const REQUIRED_FUNCTIONS: &[&str] = &[
    "createacct",
    "suspendacct",
    "unsuspendacct",
    "removeacct",
    "changepackage",
    "passwd",
    "listpkgs",
    "accountsummary",
    "create_user_session",
];

/// `createacct` parameters set by this adapter; `AccountSpec::extra` may not
/// override them.
const MANAGED_PARAMS: &[&str] = &[
    "username",
    "user",
    "domain",
    "password",
    "pass",
    "plan",
    "contactemail",
    "reseller",
    "hasshell",
    "language",
    "customip",
    "ip",
    "ownerself",
    "quota",
    "bwlimit",
    "maxftp",
    "maxsql",
    "maxpop",
    "maxsub",
    "maxpark",
    "maxaddon",
    "acllist",
];

/// Admin form schema.
pub fn descriptor() -> IntegrationDescriptor {
    let d = IntegrationDescriptor::new(kinds::CPANEL, "cPanel & WHM", IntegrationCategory::Panel)
        .description(
            "Provisions cPanel accounts through WHM API 1 with an API token \
             (WHM » Development » Manage API Tokens).",
        )
        .docs_url("https://api.docs.cpanel.net/whm/introduction/");
    let d = connection_fields(d, DEFAULT_PORT, "server.example.com");
    let d = d.config_field(
        FieldSpec::text("username", "WHM username")
            .default_value("root")
            .help("Owner of the API token: root or a reseller"),
    );
    let d = provisioning_fields(d, true)
        .config_field(
            FieldSpec::text("reseller_acl_list", "Reseller ACL list")
                .help("Saved ACL list (WHM » Edit Reseller Nameservers and Privileges) applied to new reseller accounts"),
        )
        .config_field(
            FieldSpec::boolean("reseller_owns_self", "Resellers own their own account")
                .default_value(true),
        )
        .config_field(
            FieldSpec::boolean("keep_dns_on_terminate", "Keep DNS zone on termination")
                .default_value(false),
        );
    d.secret_field(FieldSpec::password("api_token", "WHM API token").required())
}

/// cPanel/WHM client.
#[derive(Debug)]
pub struct CpanelPanel {
    http: PanelHttp,
    settings: CommonSettings,
    whm_user: String,
    authorization: SecretString,
    reseller_acl_list: Option<String>,
    reseller_owns_self: bool,
    keep_dns: bool,
}

impl CpanelPanel {
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
        let whm_user = cfg.string_or("username", "root")?;
        if whm_user.contains([':', ' ']) {
            return Err(IntegrationError::Config(
                "config.username must not contain ':' or spaces".into(),
            ));
        }
        let token = sec.secret("api_token")?;
        let authorization = SecretString::from(format!("whm {whm_user}:{}", token.expose_str()));
        let http = PanelHttp::new(kinds::CPANEL, &settings.endpoint, vec![token])?;
        Ok(Self {
            http,
            whm_user,
            authorization,
            reseller_acl_list: cfg.opt_str("reseller_acl_list")?.map(str::to_owned),
            reseller_owns_self: cfg.bool_or("reseller_owns_self", true)?,
            keep_dns: cfg.bool_or("keep_dns_on_terminate", false)?,
            settings,
        })
    }

    /// Calls a WHM API 1 function. `secrets` are request values to scrub from
    /// error messages (passwords).
    async fn call(
        &self,
        function: &'static str,
        params: &[(&str, String)],
        secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        let req = self
            .http
            .request(Method::POST, &format!("json-api/{function}"))?
            .query(&[("api.version", "1")])
            .header(
                reqwest::header::AUTHORIZATION,
                self.authorization.expose_str(),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .form(params);
        let reply = self.http.send(function, req).await?;
        let parsed: Option<Value> = serde_json::from_str(&reply.body).ok();

        if !reply.status.is_success() {
            let message = parsed.as_ref().and_then(whm_error_text);
            return Err(self.http.status_error(&reply, message.as_deref(), secrets));
        }
        let Some(body) = parsed else {
            return Err(self.http.parse_error(function, &reply, "not JSON"));
        };
        if let Some(meta) = body.get("metadata") {
            if flag(&meta["result"]) {
                return Ok(body);
            }
            let reason = text(&meta["reason"]).unwrap_or_default();
            return Err(self
                .http
                .business_error(&format!("{function}: {reason}"), None, secrets));
        }
        // API 2 / legacy envelope (e.g. "Token denied" on some versions).
        if let Some(message) = whm_error_text(&body) {
            return Err(self.http.business_error(&message, None, secrets));
        }
        Err(self
            .http
            .parse_error(function, &reply, "no metadata object"))
    }

    /// First `data.acct[]` entry of `accountsummary`.
    async fn account_summary(&self, user: &str) -> Result<Value, IntegrationError> {
        let body = self
            .call("accountsummary", &[("user", user.to_owned())], &[])
            .await?;
        body["data"]["acct"]
            .as_array()
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| IntegrationError::NotFound(format!("cPanel account `{user}`")))
    }

    /// Picks a free username for `domain` with `verify_new_username`.
    async fn free_username(&self, domain: &str) -> Result<String, IntegrationError> {
        for candidate in UsernameRules::CPANEL
            .candidates(domain)
            .into_iter()
            .take(MAX_USERNAME_PROBES)
        {
            match self
                .call("verify_new_username", &[("user", candidate.clone())], &[])
                .await
            {
                Ok(_) => return Ok(candidate),
                Err(
                    e @ (IntegrationError::Auth(_)
                    | IntegrationError::Transport(_)
                    | IntegrationError::Timeout
                    | IntegrationError::RateLimited { .. }),
                ) => return Err(e),
                Err(IntegrationError::Remote {
                    status: Some(s), ..
                }) if s >= 500 => {
                    return Err(IntegrationError::remote(
                        Some(s),
                        "cPanel: verify_new_username failed",
                    ));
                }
                // WHM without this function ("Unknown app ..." / HTTP 404):
                // let `createacct` itself report a clash.
                Err(IntegrationError::Remote { message, .. })
                    if message.to_lowercase().contains("unknown app") =>
                {
                    return Ok(candidate);
                }
                Err(IntegrationError::NotFound(message)) if message.contains("HTTP 404") => {
                    return Ok(candidate);
                }
                Err(_) => continue,
            }
        }
        Err(IntegrationError::Conflict(format!(
            "cPanel: no free username found for `{domain}`"
        )))
    }

    fn acct_to_info(&self, acct: &Value) -> AccountInfo {
        let username = text(&acct["user"]).unwrap_or_default();
        let suspended = flag(&acct["suspended"]);
        AccountInfo {
            account: AccountRef {
                username,
                domain: text(&acct["domain"]),
                id: None,
            },
            package: text(&acct["plan"]),
            email: text(&acct["email"]).filter(|e| e != "*unknown*"),
            ip: text(&acct["ip"]).and_then(|ip| ip.parse().ok()),
            suspended,
            suspend_reason: if suspended {
                text(&acct["suspendreason"]).filter(|r| r != "not suspended")
            } else {
                None
            },
            owner: text(&acct["owner"]),
            created_at: acct["unix_startdate"]
                .as_i64()
                .or_else(|| text(&acct["unix_startdate"]).and_then(|s| s.parse().ok()))
                .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0)),
            extra: acct.clone(),
        }
    }
}

/// Error text of an API 2 / cpsrvd error body.
fn whm_error_text(body: &Value) -> Option<String> {
    text(&body["metadata"]["reason"])
        .or_else(|| text(&body["cpanelresult"]["error"]))
        .or_else(|| text(&body["error"]))
        .or_else(|| text(&body["errors"][0]))
}

/// `*unknown*` in account limits means "none allowed".
fn acct_limit(v: &Value) -> Option<u64> {
    if v.as_str() == Some("*unknown*") {
        Some(0)
    } else {
        limit(v)
    }
}

/// Package / account disk & bandwidth: `"unlimited"`, `0` (unlimited), MB
/// numbers or suffixed sizes (`"500M"`).
fn mb_limit(v: &Value) -> Option<u64> {
    size_mib(v, 1.0).filter(|mb| *mb > 0)
}

fn package_from(pkg: &Value) -> PanelPackage {
    let name = text(&pkg["name"]).unwrap_or_default();
    PanelPackage {
        id: name.clone(),
        name,
        limits: PanelLimits {
            disk_mb: mb_limit(&pkg["QUOTA"]),
            bandwidth_mb: mb_limit(&pkg["BWLIMIT"]),
            // MAXADDON counts addon domains; the primary domain comes on top.
            domains: limit(&pkg["MAXADDON"]).map(|n| n + 1),
            subdomains: limit(&pkg["MAXSUB"]),
            alias_domains: limit(&pkg["MAXPARK"]),
            email_accounts: limit(&pkg["MAXPOP"]),
            databases: limit(&pkg["MAXSQL"]),
            ftp_accounts: limit(&pkg["MAXFTP"]),
            inodes: None,
            reseller_accounts: None,
        },
        reseller: false,
        extra: pkg.clone(),
    }
}

fn opt_num(v: Option<u64>) -> String {
    // WHM: "unlimited" is accepted by every createacct limit parameter.
    v.map_or_else(|| "unlimited".to_owned(), |n| n.to_string())
}

fn scalar_param(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_owned()),
        _ => None,
    }
}

#[async_trait]
impl HostingPanel for CpanelPanel {
    fn kind(&self) -> &'static str {
        kinds::CPANEL
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let version = self.call("version", &[], &[]).await?;
        let server_version = text(&version["data"]["version"]);
        let apps = self.call("applist", &[], &[]).await?;
        let allowed: Vec<String> = apps["data"]["app"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| text(v).or_else(|| text(&v["name"])))
                    .collect()
            })
            .unwrap_or_default();
        let missing: Vec<&str> = REQUIRED_FUNCTIONS
            .iter()
            .copied()
            .filter(|f| !allowed.iter().any(|a| a == f))
            .collect();
        if !allowed.is_empty() && !missing.is_empty() {
            return Err(IntegrationError::Auth(format!(
                "cPanel: the API token of `{}` may not call: {}",
                self.whm_user,
                missing.join(", ")
            )));
        }
        Ok(ConnectionInfo {
            server_version: server_version.map(|v| format!("WHM {v}")),
            details: Some(format!(
                "authenticated as {}; {} API functions allowed",
                self.whm_user,
                allowed.len()
            )),
        })
    }

    async fn list_packages(&self) -> Result<Vec<PanelPackage>, IntegrationError> {
        let body = self.call("listpkgs", &[], &[]).await?;
        Ok(body["data"]["pkg"]
            .as_array()
            .map(|pkgs| pkgs.iter().map(package_from).collect())
            .unwrap_or_default())
    }

    async fn create_account(&self, spec: &AccountSpec) -> Result<AccountCreated, IntegrationError> {
        let domain = spec.domain.trim().to_lowercase();
        if domain.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cPanel: domain is required".into(),
            ));
        }
        if spec.owner.email.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cPanel: owner email is required".into(),
            ));
        }
        if spec.password.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cPanel: password is required (see hos_panels::password)".into(),
            ));
        }
        let username = if spec.username.trim().is_empty() {
            self.free_username(&domain).await?
        } else {
            let u = spec.username.trim().to_owned();
            UsernameRules::CPANEL.check(&u)?;
            u
        };
        let password = spec.password.expose_str();

        let mut params: Vec<(&str, String)> = vec![
            ("username", username.clone()),
            ("domain", domain.clone()),
            ("password", password.to_owned()),
            ("contactemail", spec.owner.email.trim().to_owned()),
            ("hasshell", if spec.shell_access { "1" } else { "0" }.into()),
        ];
        let package = spec.package.trim();
        if !package.is_empty() {
            params.push(("plan", package.to_owned()));
        }
        if let Some(limits) = &spec.limit_overrides {
            if !package.is_empty() {
                // WHM ignores/forbids quota parameters together with `plan`.
                return Err(IntegrationError::InvalidInput(
                    "cPanel: limit_overrides cannot be combined with a package; \
                     create a dedicated package instead"
                        .into(),
                ));
            }
            params.extend([
                (
                    "quota",
                    limits
                        .disk_mb
                        .map_or_else(|| "0".to_owned(), |n| n.to_string()),
                ),
                ("bwlimit", opt_num(limits.bandwidth_mb)),
                ("maxftp", opt_num(limits.ftp_accounts)),
                ("maxsql", opt_num(limits.databases)),
                ("maxpop", opt_num(limits.email_accounts)),
                ("maxsub", opt_num(limits.subdomains)),
                ("maxpark", opt_num(limits.alias_domains)),
                (
                    "maxaddon",
                    opt_num(limits.domains.map(|d| d.saturating_sub(1))),
                ),
            ]);
        }
        if let Some(locale) = spec.locale {
            params.push(("language", locale.as_str().to_owned()));
        }
        match spec.ip.or(self.settings.default_ip) {
            Some(IpAddr::V4(ip)) => params.push(("customip", ip.to_string())),
            Some(IpAddr::V6(_)) => {
                return Err(IntegrationError::InvalidInput(
                    "cPanel: createacct only accepts an IPv4 address in customip".into(),
                ));
            }
            None => {}
        }
        if spec.reseller {
            params.push(("reseller", "1".into()));
            if self.reseller_owns_self {
                params.push(("ownerself", "1".into()));
            }
        }
        if let Some(extra) = spec.extra.as_object() {
            for (key, value) in extra {
                if MANAGED_PARAMS.contains(&key.as_str()) {
                    continue;
                }
                if let Some(v) = scalar_param(value) {
                    // Keys come from admin-defined product config.
                    params.push((key.as_str(), v));
                }
            }
        }

        let body = self.call("createacct", &params, &[password]).await?;
        let data = &body["data"];

        let mut warnings: Vec<String> = Vec::new();
        if spec.reseller {
            let acl = spec.extra["acllist"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| self.reseller_acl_list.clone());
            if let Some(acl) = acl
                && let Err(e) = self
                    .call(
                        "setacls",
                        &[("reseller", username.clone()), ("acllist", acl)],
                        &[],
                    )
                    .await
            {
                tracing::warn!(panel = "cpanel", user = %username, error = %e, "setacls failed after createacct");
                warnings.push(format!("setacls failed: {e}"));
            }
        }

        let mut nameservers: Vec<String> =
            ["nameserver", "nameserver2", "nameserver3", "nameserver4"]
                .iter()
                .filter_map(|k| text(&data[*k]))
                .collect();
        if nameservers.is_empty() {
            nameservers.clone_from(&self.settings.nameservers);
        }
        Ok(AccountCreated {
            account: AccountRef {
                username,
                domain: Some(domain),
                id: None,
            },
            ip: text(&data["ip"]).and_then(|ip| ip.parse().ok()),
            nameservers,
            login_url: Some(
                self.settings
                    .login_url_or(CPANEL_HTTPS_PORT, CPANEL_HTTP_PORT),
            ),
            extra: json!({
                "package": data["package"],
                "warnings": warnings,
            }),
        })
    }

    async fn suspend(
        &self,
        account: &AccountRef,
        reason: Option<&str>,
    ) -> Result<(), IntegrationError> {
        let mut params = vec![("user", account.username.clone())];
        if let Some(reason) = reason.map(str::trim).filter(|r| !r.is_empty()) {
            params.push(("reason", reason.to_owned()));
        }
        match self.call("suspendacct", &params, &[]).await {
            // Idempotent: a retried job must not fail on an already suspended account.
            Err(e) if e.to_string().to_lowercase().contains("already suspended") => Ok(()),
            other => other.map(drop),
        }
    }

    async fn unsuspend(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        match self
            .call("unsuspendacct", &[("user", account.username.clone())], &[])
            .await
        {
            Err(e) if e.to_string().to_lowercase().contains("not suspended") => Ok(()),
            other => other.map(drop),
        }
    }

    async fn terminate(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        self.call(
            "removeacct",
            &[
                ("username", account.username.clone()),
                ("keepdns", if self.keep_dns { "1" } else { "0" }.into()),
            ],
            &[],
        )
        .await
        .map(drop)
    }

    async fn change_package(
        &self,
        account: &AccountRef,
        package: &str,
    ) -> Result<(), IntegrationError> {
        if package.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "cPanel: package is required".into(),
            ));
        }
        self.call(
            "changepackage",
            &[
                ("user", account.username.clone()),
                ("pkg", package.trim().to_owned()),
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
                "cPanel: password is required".into(),
            ));
        }
        let pw = password.expose_str();
        self.call(
            "passwd",
            &[
                ("user", account.username.clone()),
                ("password", pw.to_owned()),
            ],
            &[pw],
        )
        .await
        .map(drop)
    }

    async fn usage(&self, account: &AccountRef) -> Result<AccountUsage, IntegrationError> {
        let acct = self.account_summary(&account.username).await?;
        let bw = self
            .call(
                "showbw",
                &[
                    ("searchtype", "user".to_owned()),
                    ("search", account.username.clone()),
                ],
                &[],
            )
            .await?;
        let bw_acct = bw["data"]["acct"]
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|x| x["user"].as_str() == Some(account.username.as_str()))
            })
            .cloned()
            .unwrap_or(Value::Null);
        let bytes = |v: &Value| v.as_u64().or_else(|| text(v).and_then(|s| s.parse().ok()));
        Ok(AccountUsage {
            collected_at: Utc::now(),
            disk_used_mb: size_mib(&acct["diskused"], 1.0),
            bandwidth_used_mb: bytes(&bw_acct["totalbytes"]).map(bytes_to_mib),
            domains: None,
            email_accounts: None,
            databases: None,
            ftp_accounts: None,
            inodes: limit(&acct["inodesused"]),
            limits: PanelLimits {
                disk_mb: mb_limit(&acct["disklimit"]),
                bandwidth_mb: bytes(&bw_acct["limit"])
                    .filter(|b| *b > 0)
                    .map(bytes_to_mib),
                domains: acct_limit(&acct["maxaddons"]).map(|n| n + 1),
                subdomains: acct_limit(&acct["maxsub"]),
                alias_domains: acct_limit(&acct["maxparked"]),
                email_accounts: acct_limit(&acct["maxpop"]),
                databases: acct_limit(&acct["maxsql"]),
                ftp_accounts: acct_limit(&acct["maxftp"]),
                inodes: limit(&acct["inodeslimit"]).filter(|n| *n > 0),
                reseller_accounts: None,
            },
        })
    }

    async fn sso_url(
        &self,
        account: &AccountRef,
        target: &SsoTarget,
    ) -> Result<SsoLogin, IntegrationError> {
        let service = match target.service {
            SsoService::ControlPanel => "cpaneld",
            SsoService::Webmail => "webmaild",
            SsoService::Reseller => "whostmgrd",
        };
        let mut params = vec![
            ("user", account.username.clone()),
            ("service", service.to_owned()),
        ];
        if let (SsoService::ControlPanel, Some(app)) = (target.service, &target.app) {
            params.push(("app", app.clone()));
        }
        if let Some(locale) = target.locale {
            params.push(("locale", locale.as_str().to_owned()));
        }
        if let Some(domain) = &account.domain
            && target.service == SsoService::Webmail
        {
            params.push(("preferred_domain", domain.clone()));
        }
        let body = self.call("create_user_session", &params, &[]).await?;
        let url = text(&body["data"]["url"]).ok_or_else(|| {
            IntegrationError::Transport("cPanel: create_user_session returned no URL".into())
        })?;
        let expires_at = body["data"]["expires"]
            .as_i64()
            .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0));
        Ok(SsoLogin {
            url: SecretString::from(url),
            expires_at,
        })
    }

    async fn get_account(&self, account: &AccountRef) -> Result<AccountInfo, IntegrationError> {
        let acct = self.account_summary(&account.username).await?;
        Ok(self.acct_to_info(&acct))
    }

    async fn list_accounts(&self) -> Result<Vec<AccountInfo>, IntegrationError> {
        let body = self.call("listaccts", &[], &[]).await?;
        Ok(body["data"]["acct"]
            .as_array()
            .map(|a| a.iter().map(|acct| self.acct_to_info(acct)).collect())
            .unwrap_or_default())
    }
}
