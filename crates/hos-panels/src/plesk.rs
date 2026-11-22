//! Plesk Obsidian via REST API v2, its CLI gateway and (for plan listing) the
//! XML API.
//!
//! * Auth: `X-API-Key` (preferred; keys come from `POST /api/v2/auth/keys` or
//!   `plesk bin secret_key -c` and are bound to the caller's IP) or HTTP basic
//!   auth with the admin login. The XML API takes the same key in `KEY`, or
//!   `HTTP_AUTH_LOGIN`/`HTTP_AUTH_PASSWD`.
//! * Model: one HostingOS account = one Plesk **customer** (login =
//!   [`AccountRef::username`]) owning one **subscription** (domain,
//!   [`AccountRef::id`] = the subscription's domain id) whose system/FTP user
//!   has the same login and password.
//! * REST covers customers, subscriptions, status and statistics. Plan
//!   switches and login links use `POST /api/v2/cli/<utility>/call`
//!   (`subscription --switch-subscription`, `admin --get-login-link -user`).
//!   REST has no service-plan endpoint, so plans come from the XML API
//!   (`<service-plan><get><filter/></get></service-plan>`).
//! * REST errors are `{"code": <int>, "message": "..."}` (XML API error codes);
//!   CLI calls answer HTTP 200 with `{"code": <exit status>, "stdout", "stderr"}`.
//!
//! Reference: <https://docs.plesk.com/en-US/obsidian/api-rpc/about-rest-api.79359/>

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use chrono::Utc;
use hos_core::IntegrationError;
use hos_core::domain::{IntegrationCategory, Locale};
use hos_core::integrations::panel::{
    AccountCreated, AccountInfo, AccountRef, AccountSpec, AccountUsage, PanelLimits, PanelPackage,
    SsoLogin, SsoService, SsoTarget,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, HostingPanel, IntegrationDescriptor, kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::{Method, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::http::PanelHttp;
use crate::settings::{CommonSettings, connection_fields, provisioning_fields};
use crate::units::{bytes_to_mib, limit, limit_str, size_mib_str, text};
use crate::username::UsernameRules;

const DEFAULT_PORT: u16 = 8443;
const PLAIN_HTTP_PORT: u16 = 8880;
const BYTE_IN_MIB: f64 = 1.0 / (1024.0 * 1024.0);
/// Plesk login links are single-use and expire after 24 hours.
const LOGIN_LINK_HOURS: i64 = 24;

/// Admin form schema.
pub fn descriptor() -> IntegrationDescriptor {
    let d = IntegrationDescriptor::new(kinds::PLESK, "Plesk Obsidian", IntegrationCategory::Panel)
        .description(
            "Creates a Plesk customer and subscription per account through the REST API v2. \
             An API key (plesk bin secret_key -c -ip-address <HostingOS IP>) is recommended; \
             the admin password works too.",
        )
        .docs_url("https://docs.plesk.com/en-US/obsidian/api-rpc/about-rest-api.79359/");
    let d = connection_fields(d, DEFAULT_PORT, "plesk.example.com");
    let d = d.config_field(
        FieldSpec::text("username", "Admin login")
            .default_value("admin")
            .help("Used with the password when no API key is set"),
    );
    let d = provisioning_fields(d, true)
        .config_field(
            FieldSpec::boolean("suspend_customer", "Also suspend the customer account")
                .default_value(true)
                .help("Blocks panel login while the subscription is suspended"),
        )
        .config_field(
            FieldSpec::boolean(
                "delete_customer_on_terminate",
                "Delete the customer on termination",
            )
            .default_value(true)
            .help("Only when the customer has no other subscriptions"),
        );
    d.secret_field(FieldSpec::password("api_key", "API key").help("Preferred over the password"))
        .secret_field(FieldSpec::password("password", "Admin password"))
}

#[derive(Debug, Clone)]
enum Auth {
    ApiKey(SecretString),
    Basic {
        login: String,
        password: SecretString,
    },
}

/// Plesk client.
#[derive(Debug)]
pub struct PleskPanel {
    http: PanelHttp,
    settings: CommonSettings,
    auth: Auth,
    suspend_customer: bool,
    delete_customer: bool,
}

// ---- XML API (service plans) ------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct XmlPacket {
    #[serde(rename = "service-plan", default)]
    service_plan: Option<XmlOperator>,
    #[serde(rename = "reseller-plan", default)]
    reseller_plan: Option<XmlOperator>,
    #[serde(default)]
    system: Option<XmlResult>,
}

#[derive(Debug, Default, Deserialize)]
struct XmlOperator {
    #[serde(default)]
    get: Option<XmlGet>,
}

#[derive(Debug, Default, Deserialize)]
struct XmlGet {
    #[serde(default)]
    result: Vec<XmlResult>,
}

#[derive(Debug, Default, Deserialize)]
struct XmlResult {
    #[serde(default)]
    status: String,
    #[serde(default)]
    errcode: Option<String>,
    #[serde(default)]
    errtext: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    guid: Option<String>,
    #[serde(default)]
    limits: Option<XmlLimits>,
}

#[derive(Debug, Default, Deserialize)]
struct XmlLimits {
    #[serde(default)]
    limit: Vec<XmlLimit>,
}

#[derive(Debug, Default, Deserialize)]
struct XmlLimit {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

/// Maps a Plesk API error code (shared by the XML and REST APIs).
fn error_for_code(code: i64, message: String) -> Option<IntegrationError> {
    Some(match code {
        1001 | 1006 => IntegrationError::Auth(message),
        1002 | 1007 => IntegrationError::Conflict(message),
        1013 | 1015 => IntegrationError::NotFound(message),
        1014 | 1019 | 1023 => IntegrationError::InvalidInput(message),
        _ => return None,
    })
}

fn plan_to_package(plan: XmlResult, reseller: bool) -> PanelPackage {
    let limits: BTreeMap<String, String> = plan
        .limits
        .map(|l| l.limit.into_iter().map(|x| (x.name, x.value)).collect())
        .unwrap_or_default();
    let count = |k: &str| limits.get(k).and_then(|v| limit_str(v));
    let mib = |k: &str| limits.get(k).and_then(|v| size_mib_str(v, BYTE_IN_MIB));
    let name = plan.name.unwrap_or_default();
    PanelPackage {
        id: name.clone(),
        name,
        limits: PanelLimits {
            disk_mb: mib("disk_space"),
            bandwidth_mb: mib("max_traffic"),
            domains: count("max_site"),
            subdomains: count("max_subdom"),
            alias_domains: count("max_dom_aliases"),
            email_accounts: count("max_box"),
            databases: count("max_db"),
            ftp_accounts: count("max_subftp_users"),
            inodes: None,
            reseller_accounts: count("max_cl"),
        },
        reseller,
        extra: json!({ "id": plan.id, "guid": plan.guid, "limits": limits }),
    }
}

fn plesk_locale(locale: Locale) -> &'static str {
    match locale {
        Locale::Tr => "tr-TR",
        Locale::En => "en-US",
    }
}

impl PleskPanel {
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
        let (auth, credentials) = match (sec.opt_secret("api_key")?, sec.opt_secret("password")?) {
            (Some(key), _) => (Auth::ApiKey(key.clone()), vec![key]),
            (None, Some(password)) => (
                Auth::Basic {
                    login: cfg.string_or("username", "admin")?,
                    password: password.clone(),
                },
                vec![password],
            ),
            (None, None) => {
                return Err(IntegrationError::Config(
                    "secrets.api_key or secrets.password is required".into(),
                ));
            }
        };
        let http = PanelHttp::new(kinds::PLESK, &settings.endpoint, credentials)?;
        Ok(Self {
            http,
            auth,
            suspend_customer: cfg.bool_or("suspend_customer", true)?,
            delete_customer: cfg.bool_or("delete_customer_on_terminate", true)?,
            settings,
        })
    }

    fn with_auth(&self, req: RequestBuilder) -> RequestBuilder {
        match &self.auth {
            Auth::ApiKey(key) => req.header("X-API-Key", key.expose_str()),
            Auth::Basic { login, password } => req.basic_auth(login, Some(password.expose_str())),
        }
    }

    /// REST call; returns the parsed JSON body (`Null` for an empty body).
    async fn rest(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        secrets: &[&str],
    ) -> Result<Value, IntegrationError> {
        let mut req = self
            .with_auth(self.http.request(method, &format!("api/v2/{path}"))?)
            .header(reqwest::header::ACCEPT, "application/json");
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let reply = self.http.send(op, req).await?;
        let parsed: Option<Value> = serde_json::from_str(&reply.body).ok();
        if reply.status.is_success() {
            return match parsed {
                Some(v) => Ok(v),
                None if reply.body.trim().is_empty() => Ok(Value::Null),
                None => Err(self.http.parse_error(op, &reply, "not JSON")),
            };
        }
        let code = parsed.as_ref().and_then(|v| v["code"].as_i64());
        let message = parsed
            .as_ref()
            .and_then(|v| text(&v["message"]))
            .unwrap_or_default();
        if let Some(code) = code {
            let full = self
                .http
                .scrub(&format!("plesk: {op}: {message} (code {code})"), secrets);
            if let Some(e) = error_for_code(code, full.clone()) {
                return Err(e);
            }
            if reply.status.is_server_error() {
                return Err(IntegrationError::Remote {
                    status: Some(reply.status.as_u16()),
                    code: Some(code.to_string()),
                    message: full,
                });
            }
        }
        let message = (!message.is_empty()).then(|| format!("{op}: {message}"));
        Err(self.http.status_error(&reply, message.as_deref(), secrets))
    }

    /// Runs a Plesk CLI utility through `POST /api/v2/cli/<utility>/call` and
    /// returns its stdout. A non-zero exit status is an error.
    async fn cli(
        &self,
        utility: &'static str,
        params: &[&str],
        secrets: &[&str],
    ) -> Result<String, IntegrationError> {
        let body = json!({ "params": params });
        let v = self
            .rest(
                utility,
                Method::POST,
                &format!("cli/{utility}/call"),
                &[],
                Some(&body),
                secrets,
            )
            .await?;
        let Some(code) = v["code"].as_i64() else {
            return Err(IntegrationError::Transport(format!(
                "plesk: cli {utility}: unexpected response"
            )));
        };
        if v.get("stdout").is_none() && v.get("message").is_some() {
            // e.g. {"code":0,"message":"Command '...' not found"}
            let message = text(&v["message"]).unwrap_or_default();
            return Err(IntegrationError::remote(
                None,
                format!("plesk: cli {utility}: {message}"),
            ));
        }
        let stdout = v["stdout"].as_str().unwrap_or_default().to_owned();
        if code != 0 {
            let stderr = text(&v["stderr"]).unwrap_or_else(|| stdout.clone());
            return Err(self.http.business_error(
                &format!("plesk: {utility}: {stderr}"),
                Some(format!("exit {code}")),
                secrets,
            ));
        }
        Ok(stdout)
    }

    /// Posts an XML API packet and parses the reply.
    async fn xml(
        &self,
        op: &'static str,
        packet: &'static str,
    ) -> Result<XmlPacket, IntegrationError> {
        let req = self
            .http
            .request(Method::POST, "enterprise/control/agent.php")?
            .header(reqwest::header::CONTENT_TYPE, "text/xml; charset=UTF-8")
            .body(packet);
        let req = match &self.auth {
            Auth::ApiKey(key) => req.header("KEY", key.expose_str()),
            Auth::Basic { login, password } => req
                .header("HTTP_AUTH_LOGIN", login.as_str())
                .header("HTTP_AUTH_PASSWD", password.expose_str()),
        };
        let reply = self.http.send(op, req).await?;
        if !reply.status.is_success() {
            return Err(self.http.status_error(&reply, None, &[]));
        }
        let packet: XmlPacket = quick_xml::de::from_str(&reply.body)
            .map_err(|e| self.http.parse_error(op, &reply, e))?;
        if let Some(system) = &packet.system
            && system.status == "error"
        {
            return Err(self.xml_error(op, system));
        }
        Ok(packet)
    }

    fn xml_error(&self, op: &str, r: &XmlResult) -> IntegrationError {
        let code = r
            .errcode
            .as_deref()
            .and_then(|c| c.trim().parse::<i64>().ok());
        let message = self.http.scrub(
            &format!("plesk: {op}: {}", r.errtext.as_deref().unwrap_or("error")),
            &[],
        );
        code.and_then(|c| error_for_code(c, message.clone()))
            .unwrap_or_else(|| IntegrationError::Remote {
                status: None,
                code: r.errcode.clone(),
                message,
            })
    }

    async fn plans(&self, reseller: bool) -> Result<Vec<PanelPackage>, IntegrationError> {
        let packet = if reseller {
            self.xml(
                "reseller-plan.get",
                "<packet><reseller-plan><get><filter><all/></filter><limits/></get></reseller-plan></packet>",
            )
            .await?
        } else {
            self.xml(
                "service-plan.get",
                "<packet><service-plan><get><filter/></get></service-plan></packet>",
            )
            .await?
        };
        let op = if reseller {
            packet.reseller_plan
        } else {
            packet.service_plan
        };
        let results = op.and_then(|o| o.get).map(|g| g.result).unwrap_or_default();
        let mut out = Vec::with_capacity(results.len());
        for r in results {
            if r.status == "error" {
                return Err(self.xml_error("plans", &r));
            }
            out.push(plan_to_package(r, reseller));
        }
        Ok(out)
    }

    async fn all_logins(&self) -> Result<HashSet<String>, IntegrationError> {
        let clients = self
            .rest("clients.list", Method::GET, "clients", &[], None, &[])
            .await?;
        Ok(clients
            .as_array()
            .map(|a| a.iter().filter_map(|c| text(&c["login"])).collect())
            .unwrap_or_default())
    }

    /// Subscription (domain) id: `AccountRef::id`, or looked up by domain.
    async fn subscription_id(&self, account: &AccountRef) -> Result<String, IntegrationError> {
        if let Some(id) = account
            .id
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty())
        {
            return Ok(id.to_owned());
        }
        let domain = account
            .domain
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| {
                IntegrationError::InvalidInput(
                    "plesk: AccountRef.id or AccountRef.domain is required".into(),
                )
            })?;
        let found = self
            .rest(
                "domains.find",
                Method::GET,
                "domains",
                &[("name", domain)],
                None,
                &[],
            )
            .await?;
        found
            .as_array()
            .and_then(|a| a.first())
            .and_then(|d| text(&d["id"]))
            .ok_or_else(|| IntegrationError::NotFound(format!("plesk: subscription `{domain}`")))
    }

    async fn subscription_name(
        &self,
        account: &AccountRef,
        sid: &str,
    ) -> Result<String, IntegrationError> {
        if let Some(d) = account
            .domain
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            return Ok(d.to_owned());
        }
        let d = self
            .rest(
                "domains.get",
                Method::GET,
                &format!("domains/{sid}"),
                &[],
                None,
                &[],
            )
            .await?;
        text(&d["name"]).ok_or_else(|| IntegrationError::NotFound(format!("plesk: domain {sid}")))
    }

    async fn owner(&self, sid: &str) -> Result<Value, IntegrationError> {
        self.rest(
            "domains.client",
            Method::GET,
            &format!("domains/{sid}/client"),
            &[],
            None,
            &[],
        )
        .await
    }

    async fn owner_id(&self, sid: &str) -> Result<String, IntegrationError> {
        text(&self.owner(sid).await?["id"]).ok_or_else(|| {
            IntegrationError::NotFound(format!("plesk: owner of subscription {sid}"))
        })
    }

    async fn set_status(&self, sid: &str, status: &str) -> Result<(), IntegrationError> {
        self.rest(
            "domains.status",
            Method::PUT,
            &format!("domains/{sid}/status"),
            &[],
            Some(&json!({ "status": status })),
            &[],
        )
        .await
        .map(drop)
    }
}

#[async_trait]
impl HostingPanel for PleskPanel {
    fn kind(&self) -> &'static str {
        kinds::PLESK
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let s = self
            .rest("server", Method::GET, "server", &[], None, &[])
            .await?;
        Ok(ConnectionInfo {
            server_version: text(&s["panel_version"]).map(|v| format!("Plesk {v}")),
            details: match (text(&s["hostname"]), text(&s["platform"])) {
                (Some(h), Some(p)) => Some(format!("{h} ({p})")),
                (Some(h), None) => Some(h),
                _ => None,
            },
        })
    }

    async fn list_packages(&self) -> Result<Vec<PanelPackage>, IntegrationError> {
        let mut out = self.plans(false).await?;
        match self.plans(true).await {
            Ok(reseller) => out.extend(reseller),
            // Non-admin keys may not read reseller plans.
            Err(IntegrationError::Auth(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(out)
    }

    async fn create_account(&self, spec: &AccountSpec) -> Result<AccountCreated, IntegrationError> {
        if spec.reseller {
            return Err(IntegrationError::Unsupported(
                "create_account (Plesk reseller accounts)",
            ));
        }
        let domain = spec.domain.trim().to_lowercase();
        if domain.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "plesk: domain is required".into(),
            ));
        }
        let email = spec.owner.email.trim();
        if email.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "plesk: owner email is required".into(),
            ));
        }
        let plan = spec.package.trim();
        if plan.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "plesk: service plan is required".into(),
            ));
        }
        if spec.limit_overrides.is_some() {
            return Err(IntegrationError::InvalidInput(
                "plesk: limit_overrides are not supported; use a dedicated service plan".into(),
            ));
        }
        let password = spec.password.expose_str();
        if password.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "plesk: password is required (see hos_panels::password)".into(),
            ));
        }
        let ip = spec.ip.or(self.settings.default_ip).ok_or_else(|| {
            IntegrationError::Config(
                "plesk: set config.default_ip (subscriptions need an IP)".into(),
            )
        })?;
        let rules = UsernameRules::PLESK;
        let username = if spec.username.trim().is_empty() {
            let taken = self.all_logins().await?;
            rules.generate(&domain, |u| taken.contains(u))?
        } else {
            let u = spec.username.trim().to_owned();
            rules.check(&u)?;
            u
        };

        let display_name = spec
            .owner
            .name
            .as_deref()
            .or(spec.owner.company.as_deref())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(&username)
            .to_owned();
        let mut client = json!({
            "name": display_name,
            "login": username,
            "email": email,
            "password": password,
            "type": "customer",
            "description": "Created by HostingOS",
        });
        if let Some(company) = spec
            .owner
            .company
            .as_deref()
            .filter(|c| !c.trim().is_empty())
        {
            client["company"] = json!(company.trim());
        }
        if let Some(locale) = spec.locale {
            client["locale"] = json!(plesk_locale(locale));
        }
        for key in ["owner_login", "external_id"] {
            if let Some(v) = spec.extra[key].as_str() {
                client[key] = json!(v);
            }
        }
        let created_client = self
            .rest(
                "clients.create",
                Method::POST,
                "clients",
                &[],
                Some(&client),
                &[password],
            )
            .await?;
        let customer_id = created_client["id"].clone();

        let subscription = json!({
            "name": domain,
            "hosting_type": "virtual",
            "hosting_settings": { "ftp_login": username, "ftp_password": password },
            "owner_client": { "id": customer_id },
            "ip_addresses": [ip.to_string()],
            "plan": { "name": plan },
        });
        let created_domain = match self
            .rest(
                "domains.create",
                Method::POST,
                "domains",
                &[],
                Some(&subscription),
                &[password],
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                // Roll back the customer so a retry starts clean.
                if let Some(cid) = text(&customer_id)
                    && let Err(rb) = self
                        .rest(
                            "clients.delete",
                            Method::DELETE,
                            &format!("clients/{cid}"),
                            &[],
                            None,
                            &[],
                        )
                        .await
                {
                    tracing::warn!(panel = "plesk", customer = %cid, error = %rb, "rollback of customer failed");
                }
                return Err(e);
            }
        };

        Ok(AccountCreated {
            account: AccountRef {
                username,
                domain: Some(domain),
                id: text(&created_domain["id"]),
            },
            ip: Some(ip),
            nameservers: self.settings.nameservers.clone(),
            login_url: Some(self.settings.login_url_or(DEFAULT_PORT, PLAIN_HTTP_PORT)),
            extra: json!({
                "customer_id": customer_id,
                "customer_guid": created_client["guid"],
                "subscription_guid": created_domain["guid"],
            }),
        })
    }

    async fn suspend(
        &self,
        account: &AccountRef,
        _reason: Option<&str>,
    ) -> Result<(), IntegrationError> {
        let sid = self.subscription_id(account).await?;
        self.set_status(&sid, "suspended").await?;
        if self.suspend_customer {
            let cid = self.owner_id(&sid).await?;
            self.rest(
                "clients.suspend",
                Method::PUT,
                &format!("clients/{cid}/suspend"),
                &[],
                None,
                &[],
            )
            .await?;
        }
        Ok(())
    }

    async fn unsuspend(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        let sid = self.subscription_id(account).await?;
        if self.suspend_customer {
            let cid = self.owner_id(&sid).await?;
            self.rest(
                "clients.activate",
                Method::PUT,
                &format!("clients/{cid}/activate"),
                &[],
                None,
                &[],
            )
            .await?;
        }
        self.set_status(&sid, "active").await
    }

    async fn terminate(&self, account: &AccountRef) -> Result<(), IntegrationError> {
        let sid = self.subscription_id(account).await?;
        let cid = if self.delete_customer {
            text(&self.owner(&sid).await?["id"])
        } else {
            None
        };
        self.rest(
            "domains.delete",
            Method::DELETE,
            &format!("domains/{sid}"),
            &[],
            None,
            &[],
        )
        .await?;
        if let Some(cid) = cid {
            let remaining = self
                .rest(
                    "clients.domains",
                    Method::GET,
                    &format!("clients/{cid}/domains"),
                    &[],
                    None,
                    &[],
                )
                .await;
            match remaining {
                Ok(v) if v.as_array().is_some_and(Vec::is_empty) => {
                    if let Err(e) = self
                        .rest(
                            "clients.delete",
                            Method::DELETE,
                            &format!("clients/{cid}"),
                            &[],
                            None,
                            &[],
                        )
                        .await
                    {
                        tracing::warn!(panel = "plesk", customer = %cid, error = %e, "customer not deleted");
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(panel = "plesk", customer = %cid, error = %e, "could not list remaining subscriptions");
                }
            }
        }
        Ok(())
    }

    async fn change_package(
        &self,
        account: &AccountRef,
        package: &str,
    ) -> Result<(), IntegrationError> {
        let plan = package.trim();
        if plan.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "plesk: service plan is required".into(),
            ));
        }
        let sid = self.subscription_id(account).await?;
        let name = self.subscription_name(account, &sid).await?;
        self.cli(
            "subscription",
            &["--switch-subscription", &name, "-service-plan", plan],
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
        let pw = password.expose_str();
        if pw.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "plesk: password is required".into(),
            ));
        }
        let sid = self.subscription_id(account).await?;
        let cid = self.owner_id(&sid).await?;
        self.rest(
            "clients.update",
            Method::PUT,
            &format!("clients/{cid}"),
            &[],
            Some(&json!({ "password": pw })),
            &[pw],
        )
        .await?;
        // Keep the subscription's system (FTP) user in sync.
        let ftp =
            json!({ "hosting_settings": { "ftp_login": account.username, "ftp_password": pw } });
        if let Err(e) = self
            .rest(
                "domains.update",
                Method::PUT,
                &format!("domains/{sid}"),
                &[],
                Some(&ftp),
                &[pw],
            )
            .await
        {
            tracing::warn!(panel = "plesk", subscription = %sid, error = %e, "FTP password not updated");
        }
        Ok(())
    }

    async fn usage(&self, account: &AccountRef) -> Result<AccountUsage, IntegrationError> {
        let sid = self.subscription_id(account).await?;
        let cid = self.owner_id(&sid).await?;
        let s = self
            .rest(
                "clients.statistics",
                Method::GET,
                &format!("clients/{cid}/statistics"),
                &[],
                None,
                &[],
            )
            .await?;
        let bytes = |v: &Value| v.as_u64().map(bytes_to_mib);
        Ok(AccountUsage {
            collected_at: Utc::now(),
            disk_used_mb: bytes(&s["disk_space"]),
            bandwidth_used_mb: bytes(&s["traffic"]),
            domains: limit(&s["active_domains"]),
            email_accounts: limit(&s["email_postboxes"]),
            databases: limit(&s["databases"]),
            ftp_accounts: None,
            inodes: None,
            limits: PanelLimits::default(),
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
        if account.username.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "plesk: AccountRef.username is required".into(),
            ));
        }
        let stdout = self
            .cli(
                "admin",
                &["--get-login-link", "-user", account.username.trim()],
                &[],
            )
            .await?;
        // One link per line (hostname and IP variants); prefer the first.
        let url = stdout
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("https://") || l.starts_with("http://"))
            .ok_or_else(|| {
                IntegrationError::Transport("plesk: login link missing in CLI output".into())
            })?;
        Ok(SsoLogin {
            url: SecretString::from(url.to_owned()),
            expires_at: Some(Utc::now() + chrono::Duration::hours(LOGIN_LINK_HOURS)),
        })
    }

    async fn get_account(&self, account: &AccountRef) -> Result<AccountInfo, IntegrationError> {
        let sid = self.subscription_id(account).await?;
        let domain = self
            .rest(
                "domains.get",
                Method::GET,
                &format!("domains/{sid}"),
                &[],
                None,
                &[],
            )
            .await?;
        let status = self
            .rest(
                "domains.status",
                Method::GET,
                &format!("domains/{sid}/status"),
                &[],
                None,
                &[],
            )
            .await?;
        let owner = self.owner(&sid).await?;
        let state = text(&status["status"]).unwrap_or_default();
        Ok(AccountInfo {
            account: AccountRef {
                username: text(&owner["login"]).unwrap_or_else(|| account.username.clone()),
                domain: text(&domain["name"]),
                id: Some(sid),
            },
            package: None,
            email: text(&owner["email"]),
            ip: None,
            suspended: !state.is_empty() && state != "active",
            suspend_reason: None,
            owner: text(&owner["owner_login"]),
            created_at: None,
            extra: json!({ "domain": domain, "status": state, "customer": owner }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_service_plan_packet() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<packet version="1.6.9.1">
  <service-plan>
    <get>
      <result>
        <status>ok</status>
        <id>2</id>
        <name>Starter &amp; Co</name>
        <guid>abc</guid>
        <external-id/>
        <mail><nonexistent-user><bounce>x</bounce></nonexistent-user></mail>
        <limits>
          <overuse>notify</overuse>
          <limit><name>disk_space</name><value>10737418240</value></limit>
          <limit><name>max_traffic</name><value>-1</value></limit>
          <limit><name>max_site</name><value>3</value></limit>
          <limit><name>max_db</name><value>10</value></limit>
          <limit><name>max_box</name><value>-1</value></limit>
        </limits>
      </result>
      <result><status>error</status><errcode>1013</errcode><errtext>gone</errtext></result>
    </get>
  </service-plan>
</packet>"#;
        let p: XmlPacket = quick_xml::de::from_str(xml).unwrap();
        let mut results = p.service_plan.unwrap().get.unwrap().result;
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].errcode.as_deref(), Some("1013"));
        let pkg = plan_to_package(results.remove(0), false);
        assert_eq!(pkg.id, "Starter & Co");
        assert_eq!(pkg.limits.disk_mb, Some(10240));
        assert_eq!(pkg.limits.bandwidth_mb, None);
        assert_eq!(pkg.limits.domains, Some(3));
        assert_eq!(pkg.limits.databases, Some(10));
        assert_eq!(pkg.limits.email_accounts, None);
    }

    #[test]
    fn parses_system_error_packet() {
        let xml = "<packet><system><status>error</status><errcode>1001</errcode>\
                   <errtext>Authentication failed</errtext></system></packet>";
        let p: XmlPacket = quick_xml::de::from_str(xml).unwrap();
        let s = p.system.unwrap();
        assert_eq!(s.status, "error");
        assert!(matches!(
            error_for_code(1001, "x".into()),
            Some(IntegrationError::Auth(_))
        ));
    }
}
