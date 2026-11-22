//! VMware vSphere (`kind = "vmware"`) through **vCenter Server**.
//!
//! * vSphere Automation REST API (`/api`, vCenter 7.0 U2+): session via
//!   `POST /api/session` (basic auth) → `vmware-api-session-id` header; an
//!   expired session (401) triggers one transparent re-login.
//! * VMs: `GET/DELETE /api/vcenter/vm/{vm}`, power via `/power?action=` (hard)
//!   and `/guest/power?action=` (graceful, needs VMware Tools), CPU/RAM via
//!   `PATCH /hardware/cpu|memory`, port group via `PATCH /hardware/ethernet/{nic}`,
//!   WebMKS console via `POST /console/tickets`.
//! * Create: `template` = `vm-…` MoRef of a VM **marked as template** (needs
//!   `vijson_release` to verify) → `POST /api/vcenter/vm?action=clone`;
//!   anything else = content-library item id of a **VM template** →
//!   `POST /api/vcenter/vm-template/library-items/{id}?action=deploy`. The
//!   source is verified exactly as on reinstall (below). Both are
//!   synchronous in REST (bounded by `deploy_timeout_secs`). Guest OS settings
//!   go through `PUT /api/vcenter/vm/{vm}/guest/customization` (cloud-init,
//!   Linux prep or Windows sysprep) before the first power-on.
//! * **Not in the REST API**: snapshots, virtual-disk growth and performance
//!   counters. When `vijson_release` is configured (vCenter 8.0 U1+), they use
//!   the official VI/JSON wire protocol (`/sdk/vim25/{release}/...`:
//!   `CreateSnapshot_Task`, `RevertToSnapshot_Task`, `RemoveSnapshot_Task`,
//!   `ReconfigVM_Task`, `VirtualMachine/{vm}/summary`); otherwise they return
//!   `Unsupported` / partial metrics.
//! * **Standalone ESXi** (no vCenter) exposes no vSphere Automation REST API at
//!   all; `deployment = esxi` makes every call fail with an explanatory error
//!   instead of guessing endpoints.
//! * Reinstall = delete + redeploy with the same name and size: the VM gets a
//!   **new MoRef**, returned in [`VmProvisioned::vm`]. The source must be a VM
//!   template — a content-library item of type `vm-template` (from
//!   `content_library` when set) or a VM marked as template, which needs
//!   `vijson_release` to verify; anything else (another customer's VM) is
//!   refused before the old VM is touched.
//!
//! `VmSpec.extra` keys: `vm_folder`, `resource_pool`, `cluster`, `windows_product_key`,
//! `windows_time_zone` (index, default 85), `time_zone` (Linux tz name).
//! `VmReinstall.extra` keys: `network` (a `NetworkSpec` object to re-apply).

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::future::join_all;
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::virt::{
    ConsoleAccess, ConsoleKind, IpConfig, NetworkSpec, NodeInfo, PowerAction, SnapshotCreated,
    SnapshotInfo, SnapshotRequest, TaskRef, TemplateInfo, VirtCapabilities, VmInfo, VmMetrics,
    VmProvisioned, VmRef, VmReinstall, VmResize, VmSpec, VmStatus,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, SelectOption, VirtProvider,
    kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::header::HeaderValue;
use reqwest::{Method, StatusCode};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::cloudinit::CloudConfig;
use crate::http::{
    ClientOptions, HttpClient, RawResponse, RetryPolicy, build_client, execute, join_path,
    map_reqwest_error, normalize_base, redact, redact_error,
};
use crate::util::{
    GIB, JsonExt, bytes_to_gib, bytes_to_gib_f64, extra_str, extra_u64, guess_os_family,
    poll_until, split_fqdn, split_ips, static_ips, validate_spec, validate_ssh_key,
};

const PROVIDER: &str = "vmware";
const SESSION_HEADER: &str = "vmware-api-session-id";

/// Upper bound of `deploy_timeout_secs`: clone and content-library deploy are
/// the only synchronous long-running calls of the vSphere REST API.
const MAX_DEPLOY_TIMEOUT_SECS: u64 = 3600;

const ESXI_MESSAGE: &str = "standalone ESXi hosts do not expose the vSphere Automation REST API \
     (/api); HostingOS manages VMware through vCenter Server 7.0 U2 or later — add the host to \
     a vCenter and point the integration at the vCenter";

/// Admin-form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::VMWARE, "VMware vSphere", IntegrationCategory::Virt)
        .description(
            "VMware vCenter Server 7.0 U2+ (vSphere Automation REST API). Templates are \
             content-library VM templates (item id) or VM MoRefs (vm-123) to clone. Snapshots, \
             disk growth and live metrics need vCenter 8.0 U1+ with the VI/JSON release set. \
             Standalone ESXi hosts have no REST API and are not supported.",
        )
        .docs_url("https://developer.broadcom.com/xapis/vsphere-automation-api/latest/")
        .config_field(
            FieldSpec::url("api_url", "vCenter URL")
                .required()
                .placeholder("https://vcenter.example.com"),
        )
        .config_field(
            FieldSpec::select(
                "deployment",
                "Deployment",
                vec![
                    SelectOption::new("vcenter", "vCenter Server"),
                    SelectOption::new("esxi", "Standalone ESXi (no REST API)"),
                ],
            )
            .default_value("vcenter"),
        )
        .config_field(
            FieldSpec::text("username", "Username")
                .required()
                .placeholder("hostingos@vsphere.local"),
        )
        .config_field(
            FieldSpec::boolean("skip_tls_verify", "Skip TLS certificate verification")
                .default_value(false)
                .help("Only for self-signed vCenter certificates on a trusted network."),
        )
        .config_field(FieldSpec::text("cluster", "Cluster MoRef").placeholder("domain-c8"))
        .config_field(
            FieldSpec::text("resource_pool", "Resource pool MoRef").placeholder("resgroup-9"),
        )
        .config_field(
            FieldSpec::text("vm_folder", "VM folder MoRef")
                .placeholder("group-v3")
                .help("Required for content-library deployments."),
        )
        .config_field(FieldSpec::text("datastore", "Datastore MoRef").placeholder("datastore-12"))
        .config_field(
            FieldSpec::text("network", "Default network MoRef")
                .placeholder("network-13 or dvportgroup-21")
                .help("Port group attached to the first NIC (VLANs are port group properties)."),
        )
        .config_field(
            FieldSpec::text("content_library", "Content library id")
                .help("Only list templates from this library (default: all libraries)."),
        )
        .config_field(
            FieldSpec::select(
                "customization",
                "Guest customization",
                vec![
                    SelectOption::new("cloudinit", "cloud-init (Linux, VMware datasource)"),
                    SelectOption::new("linux", "Linux prep (hostname/network only)"),
                    SelectOption::new("windows", "Windows sysprep"),
                    SelectOption::new("none", "None"),
                ],
            )
            .default_value("cloudinit"),
        )
        .config_field(
            FieldSpec::text("vijson_release", "VI/JSON release")
                .placeholder("8.0.2.0")
                .help("vCenter 8.0 U1+: enables snapshots, disk growth and live metrics."),
        )
        .config_field(FieldSpec::number("timeout_secs", "HTTP timeout (s)").default_value(60))
        .config_field(
            FieldSpec::number("deploy_timeout_secs", "Clone/deploy timeout (s)")
                .default_value(1800)
                .help("Clone and library deploy are synchronous REST calls."),
        )
        .config_field(
            FieldSpec::number("task_timeout_secs", "Task wait limit (s)").default_value(900),
        )
        .config_field(
            FieldSpec::number("task_poll_ms", "Task poll interval (ms)").default_value(2000),
        )
        .secret_field(FieldSpec::password("password", "Password").required())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Customization {
    CloudInit,
    Linux,
    Windows,
    None,
}

#[derive(Debug, Clone)]
struct Settings {
    esxi: bool,
    cluster: Option<String>,
    resource_pool: Option<String>,
    vm_folder: Option<String>,
    datastore: Option<String>,
    network: Option<String>,
    content_library: Option<String>,
    customization: Customization,
    vijson_release: Option<String>,
    deploy_timeout: Duration,
    task_timeout: Duration,
    task_poll: Duration,
}

/// VMware vCenter provider. Build with [`VmwareProvider::from_config`] or [`crate::build`].
pub struct VmwareProvider {
    http: HttpClient,
    base: url::Url,
    username: String,
    password: SecretString,
    rest_session: Mutex<Option<SecretString>>,
    vim_session: Mutex<Option<SecretString>>,
    retry: RetryPolicy,
    settings: Settings,
}

impl fmt::Debug for VmwareProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VmwareProvider")
            .field("base", &self.base.as_str())
            .field("username", &self.username)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Which API a call goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Api {
    Rest,
    Vim,
}

impl VmwareProvider {
    /// Creates the provider from `integrations.config` / decrypted secrets,
    /// under the process-wide outbound policy (`HOS_OUTBOUND_POLICY`).
    pub fn from_config(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        Self::from_config_with_policy(config, secrets, OutboundPolicy::global())
    }

    /// [`from_config`](Self::from_config) with an explicit outbound policy
    /// (tests against a local mock server use `OutboundPolicy::Unrestricted`).
    pub fn from_config_with_policy(
        config: &Value,
        secrets: &Value,
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        let cfg = FieldReader::config(config);
        let sec = FieldReader::secrets(secrets);
        let customization = match cfg.string_or("customization", "cloudinit")?.as_str() {
            "cloudinit" => Customization::CloudInit,
            "linux" => Customization::Linux,
            "windows" => Customization::Windows,
            "none" => Customization::None,
            _ => {
                return Err(IntegrationError::Config(
                    "config.customization must be cloudinit, linux, windows or none".into(),
                ));
            }
        };
        let opt = |k: &str| -> Result<Option<String>, IntegrationError> {
            Ok(cfg.opt_str(k)?.map(str::to_owned))
        };
        let settings = Settings {
            esxi: cfg.string_or("deployment", "vcenter")? == "esxi",
            cluster: opt("cluster")?,
            resource_pool: opt("resource_pool")?,
            vm_folder: opt("vm_folder")?,
            datastore: opt("datastore")?,
            network: opt("network")?,
            content_library: opt("content_library")?,
            customization,
            vijson_release: opt("vijson_release")?,
            deploy_timeout: Duration::from_secs(
                cfg.u64_or("deploy_timeout_secs", 1800)?
                    .clamp(30, MAX_DEPLOY_TIMEOUT_SECS),
            ),
            task_timeout: Duration::from_secs(cfg.u64_or("task_timeout_secs", 900)?.max(5)),
            task_poll: Duration::from_millis(cfg.u64_or("task_poll_ms", 2000)?.max(5)),
        };
        if let Some(rel) = &settings.vijson_release
            && !rel.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            return Err(IntegrationError::Config(
                "config.vijson_release must look like 8.0.2.0".into(),
            ));
        }
        let base = normalize_base(cfg.url("api_url")?, &[])?;
        Ok(Self {
            http: build_client(ClientOptions::from_config(&cfg, 60, policy)?, &base)?,
            base,
            username: cfg.str("username")?.to_owned(),
            password: sec.secret("password")?,
            rest_session: Mutex::new(None),
            vim_session: Mutex::new(None),
            retry: RetryPolicy::standard(),
            settings,
        })
    }

    fn esxi_guard(&self, op: &'static str) -> Result<(), IntegrationError> {
        if self.settings.esxi {
            tracing::debug!(provider = PROVIDER, op, "rejected: standalone ESXi");
            Err(IntegrationError::Unsupported(
                "standalone ESXi (no vSphere Automation REST API; use vCenter Server 7.0 U2+)",
            ))
        } else {
            Ok(())
        }
    }

    fn vim_release(&self, feature: &'static str) -> Result<&str, IntegrationError> {
        self.settings
            .vijson_release
            .as_deref()
            .ok_or(IntegrationError::Unsupported(feature))
    }

    // ------------------------------------------------------------------
    // Sessions
    // ------------------------------------------------------------------

    async fn login(&self, api: Api) -> Result<SecretString, IntegrationError> {
        let secrets = [self.password.expose_str()];
        let (request, what) = match api {
            Api::Rest => (
                self.http
                    .post(join_path(&self.base, &["api", "session"])?)
                    .basic_auth(&self.username, Some(self.password.expose_str()))
                    .build()
                    .map_err(map_reqwest_error)?,
                "REST",
            ),
            Api::Vim => {
                let release = self.vim_release("VI/JSON")?;
                let url = join_path(
                    &self.base,
                    &[
                        "sdk",
                        "vim25",
                        release,
                        "SessionManager",
                        "SessionManager",
                        "Login",
                    ],
                )?;
                (
                    self.http
                        .post(url)
                        .json(&json!({"userName": self.username, "password": self.password.expose_str()}))
                        .build()
                        .map_err(map_reqwest_error)?,
                    "VI/JSON",
                )
            }
        };
        let raw = execute(&self.http, request, false, &self.retry, PROVIDER)
            .await
            .map_err(|e| redact_error(e, &secrets))?;
        match raw.status {
            StatusCode::NOT_FOUND if api == Api::Rest => {
                return Err(IntegrationError::Config(format!(
                    "{} has no /api/session endpoint: it is not vCenter 7.0 U2+ ({ESXI_MESSAGE})",
                    self.base.host_str().unwrap_or("the server")
                )));
            }
            StatusCode::NOT_FOUND => {
                return Err(IntegrationError::Config(
                    "VI/JSON endpoint not found: check vijson_release (needs vCenter 8.0 U1+)"
                        .into(),
                ));
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(IntegrationError::Auth(format!(
                    "vCenter rejected the {what} login"
                )));
            }
            s if !s.is_success() => return Err(self.error_from(&raw, api)),
            _ => {}
        }
        let token = match api {
            Api::Rest => raw.json::<String>().ok(),
            Api::Vim => None,
        }
        .or_else(|| {
            raw.headers
                .get(SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            IntegrationError::Transport(format!("{what} login returned no session id"))
        })?;
        Ok(SecretString::from(token))
    }

    async fn session(&self, api: Api) -> Result<SecretString, IntegrationError> {
        let slot = match api {
            Api::Rest => &self.rest_session,
            Api::Vim => &self.vim_session,
        };
        let mut guard = slot.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = self.login(api).await?;
        *guard = Some(s.clone());
        Ok(s)
    }

    async fn drop_session(&self, api: Api, stale: &SecretString) {
        let slot = match api {
            Api::Rest => &self.rest_session,
            Api::Vim => &self.vim_session,
        };
        let mut guard = slot.lock().await;
        if guard.as_ref() == Some(stale) {
            *guard = None;
        }
    }

    // ------------------------------------------------------------------
    // HTTP
    // ------------------------------------------------------------------

    async fn call_api(
        &self,
        api: Api,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<&Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, IntegrationError> {
        let mut url = join_path(&self.base, segments)?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let idempotent = method == Method::GET;
        for attempt in 0..2 {
            let session = self.session(api).await?;
            let mut header = HeaderValue::from_str(session.expose_str())
                .map_err(|_| IntegrationError::Transport("invalid session id".into()))?;
            header.set_sensitive(true);
            let mut builder = self
                .http
                .request(method.clone(), url.clone())
                .header(SESSION_HEADER, header);
            if let Some(body) = body {
                builder = builder.json(body);
            }
            if let Some(t) = timeout {
                builder = builder.timeout(t);
            }
            let request = builder.build().map_err(map_reqwest_error)?;
            let raw = execute(&self.http, request, idempotent, &self.retry, PROVIDER).await?;
            let expired = raw.status == StatusCode::UNAUTHORIZED
                || (api == Api::Vim && vim_fault(&raw).as_deref() == Some("NotAuthenticated"));
            if expired && attempt == 0 {
                self.drop_session(api, &session).await;
                continue;
            }
            if !raw.is_success() {
                return Err(self.error_from(&raw, api));
            }
            if raw.body.is_empty() || raw.status == StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            return raw.json();
        }
        Err(IntegrationError::Auth(
            "vCenter session could not be re-established".into(),
        ))
    }

    async fn rest(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, IntegrationError> {
        let mut path = vec!["api"];
        path.extend_from_slice(segments);
        self.call_api(Api::Rest, method, &path, query, body, None)
            .await
    }

    async fn vim(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<&Value>,
    ) -> Result<Value, IntegrationError> {
        let release = self.vim_release("VI/JSON")?;
        let mut path = vec!["sdk", "vim25", release];
        path.extend_from_slice(segments);
        self.call_api(Api::Vim, method, &path, &[], body, None)
            .await
    }

    fn error_from(&self, raw: &RawResponse, api: Api) -> IntegrationError {
        let body: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
        let secrets = [self.password.expose_str()];
        match api {
            Api::Rest => {
                // 7.0+: {"error_type", "messages"}; legacy: {"type", "value": {"messages"}}.
                let inner = body.get("value").filter(|v| v.is_object()).unwrap_or(&body);
                let error_type = body
                    .str_at("error_type")
                    .or_else(|| inner.str_at("error_type"))
                    .map(str::to_owned)
                    .or_else(|| {
                        body.str_at("type")
                            .and_then(|t| t.rsplit('.').next())
                            .map(str::to_ascii_uppercase)
                    });
                let message = inner
                    .get("messages")
                    .and_then(Value::as_array)
                    .map(|m| {
                        m.iter()
                            .filter_map(|x| x.str_at("default_message"))
                            .collect::<Vec<_>>()
                            .join(" / ")
                    })
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| raw.status_text());
                let message = redact(&message, &secrets);
                let tagged = match &error_type {
                    Some(t) => format!("{t}: {message}"),
                    None => message.clone(),
                };
                match error_type.as_deref() {
                    Some("NOT_FOUND") => IntegrationError::NotFound(tagged),
                    Some("UNAUTHENTICATED" | "UNAUTHORIZED") => IntegrationError::Auth(tagged),
                    Some(
                        "ALREADY_EXISTS"
                        | "ALREADY_IN_DESIRED_STATE"
                        | "RESOURCE_BUSY"
                        | "RESOURCE_IN_USE"
                        | "CONCURRENT_CHANGE"
                        | "NOT_ALLOWED_IN_CURRENT_STATE",
                    ) => IntegrationError::Conflict(tagged),
                    Some("INVALID_ARGUMENT" | "INVALID_REQUEST" | "INVALID_ELEMENT_TYPE") => {
                        IntegrationError::InvalidInput(tagged)
                    }
                    Some("TIMED_OUT") => IntegrationError::Timeout,
                    _ => raw.status_error(message, error_type),
                }
            }
            Api::Vim => {
                let fault = body.str_at("_typeName").map(str::to_owned);
                let message = body
                    .str_at("localizedMessage")
                    .or_else(|| body.str_at("message"))
                    .map(str::to_owned)
                    .or_else(|| {
                        body.get("faultMessage")
                            .and_then(Value::as_array)
                            .and_then(|m| m.first())
                            .and_then(|m| m.str_at("message"))
                            .map(str::to_owned)
                    })
                    .or_else(|| fault.clone())
                    .unwrap_or_else(|| raw.status_text());
                let message = redact(&message, &secrets);
                match fault.as_deref() {
                    Some("ManagedObjectNotFound") => IntegrationError::NotFound(message),
                    Some("NotAuthenticated" | "InvalidLogin" | "NoPermission") => {
                        IntegrationError::Auth(message)
                    }
                    Some(
                        "InvalidPowerState" | "InvalidState" | "TaskInProgress"
                        | "ConcurrentAccess" | "FileLocked",
                    ) => IntegrationError::Conflict(message),
                    Some("InvalidArgument" | "InvalidRequest" | "InvalidType") => {
                        IntegrationError::InvalidInput(message)
                    }
                    _ => raw.status_error(message, fault),
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // VM helpers
    // ------------------------------------------------------------------

    async fn vm_info_raw(&self, vm: &str) -> Result<Value, IntegrationError> {
        self.rest(Method::GET, &["vcenter", "vm", vm], &[], None)
            .await
    }

    async fn power_state(&self, vm: &str) -> Result<String, IntegrationError> {
        let v = self
            .rest(Method::GET, &["vcenter", "vm", vm, "power"], &[], None)
            .await?;
        Ok(v.str_at("state").unwrap_or("UNKNOWN").to_owned())
    }

    /// Runs a power operation; "already in the desired state" is success.
    async fn power_op(&self, vm: &str, guest: bool, action: &str) -> Result<(), IntegrationError> {
        let path: &[&str] = if guest {
            &["vcenter", "vm", vm, "guest", "power"]
        } else {
            &["vcenter", "vm", vm, "power"]
        };
        match self
            .rest(Method::POST, path, &[("action", action.to_owned())], None)
            .await
        {
            Ok(_) => Ok(()),
            Err(IntegrationError::Conflict(m)) if m.starts_with("ALREADY_IN_DESIRED_STATE") => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Graceful shutdown (VMware Tools) with a hard power-off fallback.
    async fn ensure_off(&self, vm: &str) -> Result<(), IntegrationError> {
        if self.power_state(vm).await? == "POWERED_OFF" {
            return Ok(());
        }
        if self.power_op(vm, true, "shutdown").await.is_ok() {
            let half = self.settings.task_timeout / 2;
            let off = poll_until(half, self.settings.task_poll, move || async move {
                Ok((self.power_state(vm).await? == "POWERED_OFF").then_some(()))
            })
            .await;
            if off.is_ok() {
                return Ok(());
            }
        }
        self.power_op(vm, false, "stop").await
    }

    async fn wait_vim_task(&self, task: &str) -> Result<Value, IntegrationError> {
        let info = poll_until(
            self.settings.task_timeout,
            self.settings.task_poll,
            move || async move {
                let info = self.vim(Method::GET, &["Task", task, "info"], None).await?;
                Ok(matches!(info.str_at("state"), Some("success" | "error")).then_some(info))
            },
        )
        .await?;
        if info.str_at("state") == Some("error") {
            let err = info.get("error").cloned().unwrap_or(Value::Null);
            let message = err
                .str_at("localizedMessage")
                .unwrap_or("vSphere task failed")
                .to_owned();
            let fault = err
                .get("fault")
                .and_then(|f| f.str_at("_typeName"))
                .map(str::to_owned);
            return Err(IntegrationError::Remote {
                status: None,
                code: fault,
                message,
            });
        }
        Ok(info.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Grows the first virtual disk via VI/JSON `ReconfigVM_Task`.
    async fn grow_disk(&self, vm: &str, disk_gb: u64) -> Result<(), IntegrationError> {
        self.vim_release("disk resize (set vijson_release; vCenter 8.0 U1+)")?;
        let config = self
            .vim(Method::GET, &["VirtualMachine", vm, "config"], None)
            .await?;
        let mut disks: Vec<Value> = config
            .get("hardware")
            .and_then(|h| h.get("device"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|d| d.str_at("_typeName") == Some("VirtualDisk"))
            .cloned()
            .collect();
        disks.sort_by_key(|d| {
            (
                d.u64_at("controllerKey").unwrap_or(0),
                d.u64_at("unitNumber").unwrap_or(0),
            )
        });
        let mut disk = disks
            .into_iter()
            .next()
            .ok_or_else(|| IntegrationError::Remote {
                status: None,
                code: None,
                message: "VM has no virtual disk".into(),
            })?;
        let current = disk
            .u64_at("capacityInBytes")
            .or_else(|| disk.u64_at("capacityInKB").map(|kb| kb * 1024))
            .unwrap_or(0);
        let wanted = disk_gb.saturating_mul(GIB);
        if wanted < current {
            return Err(IntegrationError::InvalidInput(format!(
                "disks can only grow (current {} GiB)",
                bytes_to_gib(current)
            )));
        }
        if wanted == current {
            return Ok(());
        }
        disk["capacityInBytes"] = json!(wanted);
        disk["capacityInKB"] = json!(wanted / 1024);
        let spec = json!({
            "spec": {
                "_typeName": "VirtualMachineConfigSpec",
                "deviceChange": [{
                    "_typeName": "VirtualDeviceConfigSpec",
                    "operation": "edit",
                    "device": disk,
                }],
            }
        });
        let task = self
            .vim(
                Method::POST,
                &["VirtualMachine", vm, "ReconfigVM_Task"],
                Some(&spec),
            )
            .await?;
        let task_id = moref(&task).ok_or_else(|| {
            IntegrationError::Transport("ReconfigVM_Task returned no task".into())
        })?;
        self.wait_vim_task(&task_id).await.map(|_| ())
    }

    async fn snapshot_tree(&self, vm: &str) -> Result<Vec<SnapshotInfo>, IntegrationError> {
        self.vim_release("snapshots (set vijson_release; vCenter 8.0 U1+)")?;
        let info = self
            .vim(Method::GET, &["VirtualMachine", vm, "snapshot"], None)
            .await?;
        let mut out = Vec::new();
        for root in info
            .get("rootSnapshotList")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            flatten_snapshots(root, None, &mut out);
        }
        Ok(out)
    }

    async fn own_snapshot(&self, vm: &str, snapshot_id: &str) -> Result<(), IntegrationError> {
        if self
            .snapshot_tree(vm)
            .await?
            .iter()
            .any(|s| s.id == snapshot_id)
        {
            Ok(())
        } else {
            Err(IntegrationError::NotFound(format!(
                "snapshot {snapshot_id} of {vm}"
            )))
        }
    }

    /// Placement shared by clone and library deploy.
    fn placement(
        &self,
        spec_node: Option<&str>,
        extra: &Value,
        with_datastore: Option<&str>,
    ) -> Map<String, Value> {
        let mut p = Map::new();
        let mut put = |k: &str, v: Option<&str>| {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                p.insert(k.to_owned(), json!(v));
            }
        };
        put(
            "folder",
            extra_str(extra, "vm_folder").or(self.settings.vm_folder.as_deref()),
        );
        put(
            "resource_pool",
            extra_str(extra, "resource_pool").or(self.settings.resource_pool.as_deref()),
        );
        put("host", spec_node);
        if spec_node.is_none() {
            put(
                "cluster",
                extra_str(extra, "cluster").or(self.settings.cluster.as_deref()),
            );
        }
        put("datastore", with_datastore);
        p
    }

    /// A create / reinstall source must be a VM template: a content-library item of
    /// type `vm-template` (from the configured library, if one is set) or an
    /// inventory VM marked as template (`summary.config.template`, checked
    /// through VI/JSON). Any other VM is a customer's machine and cloning it
    /// would copy its disks, so it is refused — as are unknown ids, with the
    /// same message.
    async fn verify_template(&self, template: &str) -> Result<(), IntegrationError> {
        let refused = || {
            IntegrationError::InvalidInput(format!("{template:?} is not an available VM template"))
        };
        if template.is_empty() {
            return Err(refused());
        }
        if template.starts_with("vm-") {
            if self.settings.vijson_release.is_none() {
                return Err(IntegrationError::InvalidInput(
                    "reinstalling from a VM MoRef needs vijson_release (to verify that the VM is \
                     a template); use a content-library VM template instead"
                        .into(),
                ));
            }
            let summary = match self
                .vim(Method::GET, &["VirtualMachine", template, "summary"], None)
                .await
            {
                Ok(v) => v,
                Err(IntegrationError::NotFound(_) | IntegrationError::InvalidInput(_)) => {
                    return Err(refused());
                }
                Err(e) => return Err(e),
            };
            let is_template = summary
                .get("config")
                .and_then(|c| c.bool_at("template"))
                .unwrap_or(false);
            return if is_template { Ok(()) } else { Err(refused()) };
        }
        let item = match self
            .rest(
                Method::GET,
                &["content", "library", "item", template],
                &[],
                None,
            )
            .await
        {
            Ok(v) => v,
            Err(IntegrationError::NotFound(_) | IntegrationError::InvalidInput(_)) => {
                return Err(refused());
            }
            Err(e) => return Err(e),
        };
        let library_ok = self
            .settings
            .content_library
            .as_deref()
            .is_none_or(|lib| item.str_at("library_id") == Some(lib));
        if item.str_at("type") == Some("vm-template") && library_ok {
            Ok(())
        } else {
            Err(refused())
        }
    }

    /// Creates the VM (powered off) and returns its MoRef.
    async fn deploy(&self, spec: &VmSpec, name: &str) -> Result<(String, bool), IntegrationError> {
        let datastore = spec
            .storage
            .as_deref()
            .or(self.settings.datastore.as_deref());
        let template = spec.template.trim();
        if template.starts_with("vm-") {
            let mut body = json!({"source": template, "name": name, "power_on": false});
            let placement = self.placement(spec.node.as_deref(), &spec.extra, datastore);
            if !placement.is_empty() {
                body["placement"] = Value::Object(placement);
            }
            let id = self
                .call_api(
                    Api::Rest,
                    Method::POST,
                    &["api", "vcenter", "vm"],
                    &[("action", "clone".into())],
                    Some(&body),
                    Some(self.settings.deploy_timeout),
                )
                .await?;
            let id = id
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| IntegrationError::Transport("clone returned no VM id".into()))?;
            return Ok((id, true));
        }
        let mut placement = self.placement(spec.node.as_deref(), &spec.extra, None);
        if !placement.contains_key("folder") {
            return Err(IntegrationError::InvalidInput(
                "content-library deployment needs a VM folder (config vm_folder or extra.vm_folder)".into(),
            ));
        }
        placement.remove("datastore");
        let mut body = json!({
            "name": name,
            "placement": placement,
            "powered_on": false,
            "hardware_customization": {
                "cpu_update": {"num_cpus": spec.cpu_cores},
                "memory_update": {"memory": spec.memory_mb},
            },
        });
        if let Some(ds) = datastore {
            body["disk_storage"] = json!({"datastore": ds});
            body["vm_home_storage"] = json!({"datastore": ds});
        }
        let id = self
            .call_api(
                Api::Rest,
                Method::POST,
                &["api", "vcenter", "vm-template", "library-items", template],
                &[("action", "deploy".into())],
                Some(&body),
                Some(self.settings.deploy_timeout),
            )
            .await?;
        let id = id
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| IntegrationError::Transport("deploy returned no VM id".into()))?;
        Ok((id, false))
    }

    /// Hardware, network, disk and guest customization of a powered-off VM.
    async fn configure(
        &self,
        vm: &str,
        spec: &VmSpec,
        cloned: bool,
        warnings: &mut Vec<String>,
    ) -> Result<(), IntegrationError> {
        if cloned {
            self.rest(
                Method::PATCH,
                &["vcenter", "vm", vm, "hardware", "cpu"],
                &[],
                Some(&json!({"count": spec.cpu_cores})),
            )
            .await?;
            self.rest(
                Method::PATCH,
                &["vcenter", "vm", vm, "hardware", "memory"],
                &[],
                Some(&json!({"size_MiB": spec.memory_mb})),
            )
            .await?;
        }
        let info = self.vm_info_raw(vm).await?;
        let network = spec
            .network
            .bridge
            .as_deref()
            .or(self.settings.network.as_deref());
        let first_nic = info.get("nics").and_then(Value::as_object).and_then(|n| {
            n.keys()
                .min_by_key(|k| k.parse::<u64>().unwrap_or(u64::MAX))
                .cloned()
        });
        if network.is_some() || spec.network.mac_address.is_some() {
            match &first_nic {
                Some(nic) => {
                    let mut update = Map::new();
                    if let Some(net) = network {
                        update.insert(
                            "backing".into(),
                            json!({"type": backing_type(net), "network": net}),
                        );
                    }
                    if let Some(mac) = &spec.network.mac_address {
                        update.insert("mac_type".into(), json!("MANUAL"));
                        update.insert("mac_address".into(), json!(mac));
                    }
                    self.rest(
                        Method::PATCH,
                        &["vcenter", "vm", vm, "hardware", "ethernet", nic],
                        &[],
                        Some(&Value::Object(update)),
                    )
                    .await?;
                }
                None => warnings
                    .push("template has no network adapter; network settings skipped".into()),
            }
        }
        if spec.network.vlan.is_some() {
            warnings.push("vSphere VLANs are port group settings; pick a port group (network) instead of a tag".into());
        }
        if spec.network.rate_limit_mbps.is_some() {
            warnings
                .push("per-VM rate limits are not available through the vSphere REST API".into());
        }
        let current_disk = first_disk_bytes(&info);
        if spec.disk_gb.saturating_mul(GIB) > current_disk {
            if self.settings.vijson_release.is_some() {
                self.grow_disk(vm, spec.disk_gb).await?;
            } else {
                warnings.push(format!(
                    "disk kept at {} GiB: growing disks needs vijson_release (vCenter 8.0 U1+)",
                    bytes_to_gib(current_disk)
                ));
            }
        }
        let custom = customization_spec(
            self.settings.customization,
            &GuestSettings {
                name: &spec.name,
                hostname: if spec.hostname.trim().is_empty() {
                    spec.name.trim()
                } else {
                    spec.hostname.trim()
                },
                password: spec.root_password.as_ref(),
                ssh_keys: &spec.ssh_keys,
                user_data: spec.cloud_init_user_data.as_deref(),
                network: &spec.network,
                extra: &spec.extra,
            },
            warnings,
        )?;
        if let Some(custom) = custom {
            self.rest(
                Method::PUT,
                &["vcenter", "vm", vm, "guest", "customization"],
                &[],
                Some(&json!({"spec": custom})),
            )
            .await?;
        }
        Ok(())
    }

    async fn delete_quietly(&self, vm: &str) {
        let _ = self.power_op(vm, false, "stop").await;
        if let Err(err) = self
            .rest(Method::DELETE, &["vcenter", "vm", vm], &[], None)
            .await
        {
            tracing::warn!(provider = PROVIDER, vm, error = %err, "cleanup of partially created VM failed");
        }
    }

    async fn provision(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        let name = if spec.name.trim().is_empty() {
            spec.hostname.trim()
        } else {
            spec.name.trim()
        };
        if name.chars().count() > 80 {
            return Err(IntegrationError::InvalidInput(
                "vSphere VM names are limited to 80 characters".into(),
            ));
        }
        let (vm, cloned) = self.deploy(spec, name).await?;
        let mut warnings = Vec::new();
        let configured = async {
            self.configure(&vm, spec, cloned, &mut warnings).await?;
            if spec.start_after_create {
                self.power_op(&vm, false, "start").await?;
            }
            Ok::<_, IntegrationError>(())
        }
        .await;
        if let Err(err) = configured {
            self.delete_quietly(&vm).await;
            return Err(err);
        }
        let (ipv4, ipv6) = static_ips(spec);
        Ok(VmProvisioned {
            vm: VmRef::new(vm.clone()),
            task: None,
            root_password: None,
            ipv4,
            ipv6,
            extra: json!({"moref": vm, "warnings": warnings}),
        })
    }

    async fn guest_details(
        &self,
        vm: &str,
    ) -> (Option<String>, Vec<std::net::IpAddr>, Vec<std::net::IpAddr>) {
        let identity_path = ["vcenter", "vm", vm, "guest", "identity"];
        let ifaces_path = ["vcenter", "vm", vm, "guest", "networking", "interfaces"];
        let (identity, ifaces) = futures::join!(
            self.rest(Method::GET, &identity_path, &[], None),
            self.rest(Method::GET, &ifaces_path, &[], None)
        );
        let identity = identity.unwrap_or(Value::Null);
        let mut addrs: Vec<String> = Vec::new();
        for iface in ifaces
            .unwrap_or(Value::Null)
            .as_array()
            .into_iter()
            .flatten()
        {
            for ip in iface
                .get("ip")
                .and_then(|i| i.get("ip_addresses"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(a) = ip.str_at("ip_address") {
                    addrs.push(a.to_owned());
                }
            }
        }
        if addrs.is_empty()
            && let Some(ip) = identity.str_at("ip_address")
        {
            addrs.push(ip.to_owned());
        }
        let (v4, v6) = split_ips(addrs.iter().map(String::as_str));
        (identity.str_at("host_name").map(str::to_owned), v4, v6)
    }
}

// ----------------------------------------------------------------------
// Pure helpers
// ----------------------------------------------------------------------

/// `_typeName` of a VI/JSON fault body.
fn vim_fault(raw: &RawResponse) -> Option<String> {
    if raw.is_success() {
        return None;
    }
    serde_json::from_slice::<Value>(&raw.body)
        .ok()
        .and_then(|v| v.str_at("_typeName").map(str::to_owned))
}

/// `value` of a VI/JSON `ManagedObjectReference`.
fn moref(v: &Value) -> Option<String> {
    v.str_at("value").map(str::to_owned)
}

fn backing_type(network: &str) -> &'static str {
    if network.starts_with("dvportgroup-") {
        "DISTRIBUTED_PORTGROUP"
    } else if network.starts_with("network-") {
        "STANDARD_PORTGROUP"
    } else {
        "OPAQUE_NETWORK"
    }
}

fn first_disk_bytes(info: &Value) -> u64 {
    info.get("disks")
        .and_then(Value::as_object)
        .and_then(|d| {
            d.iter()
                .min_by_key(|(k, _)| k.parse::<u64>().unwrap_or(u64::MAX))
                .and_then(|(_, v)| v.u64_at("capacity"))
        })
        .unwrap_or(0)
}

fn map_power(state: Option<&str>) -> VmStatus {
    match state {
        Some("POWERED_ON") => VmStatus::Running,
        Some("POWERED_OFF") => VmStatus::Stopped,
        Some("SUSPENDED") => VmStatus::Paused,
        _ => VmStatus::Unknown,
    }
}

fn flatten_snapshots(node: &Value, parent: Option<&str>, out: &mut Vec<SnapshotInfo>) {
    let Some(id) = node.get("snapshot").and_then(moref) else {
        return;
    };
    out.push(SnapshotInfo {
        id: id.clone(),
        name: node.str_at("name").unwrap_or(&id).to_owned(),
        description: node.str_at("description").map(str::to_owned),
        created_at: node
            .str_at("createTime")
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc)),
        size_mb: None,
        includes_memory: node.str_at("state") == Some("poweredOn"),
        parent: parent.map(str::to_owned),
    });
    for child in node
        .get("childSnapshotList")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        flatten_snapshots(child, Some(&id), out);
    }
}

struct GuestSettings<'a> {
    name: &'a str,
    hostname: &'a str,
    password: Option<&'a SecretString>,
    ssh_keys: &'a [String],
    user_data: Option<&'a str>,
    network: &'a NetworkSpec,
    extra: &'a Value,
}

fn ipv4_settings(net: &NetworkSpec) -> Value {
    match &net.ipv4 {
        Some(IpConfig::Static(s)) => {
            let mut v = json!({"type": "STATIC", "ip_address": s.address.to_string(), "prefix": s.prefix_len});
            if let Some(gw) = s.gateway {
                v["gateways"] = json!([gw.to_string()]);
            }
            v
        }
        _ => json!({"type": "DHCP"}),
    }
}

fn ipv6_settings(net: &NetworkSpec) -> Option<Value> {
    match &net.ipv6 {
        Some(IpConfig::Static(s)) => {
            let mut v = json!({"type": "STATIC", "ipv6": [{"ip_address": s.address.to_string(), "prefix": s.prefix_len}]});
            if let Some(gw) = s.gateway {
                v["gateways"] = json!([gw.to_string()]);
            }
            Some(v)
        }
        Some(IpConfig::Dhcp | IpConfig::Auto) => Some(json!({"type": "DHCP"})),
        None => None,
    }
}

/// cloud-init `network` (v2) metadata for the VMware datasource.
fn cloudinit_network(net: &NetworkSpec) -> Value {
    let mut eth = Map::new();
    eth.insert("match".into(), json!({"name": "e*"}));
    let mut addresses = Vec::new();
    let mut routes = Vec::new();
    match &net.ipv4 {
        Some(IpConfig::Static(s)) => {
            addresses.push(format!("{}/{}", s.address, s.prefix_len));
            if let Some(gw) = s.gateway {
                routes.push(json!({"to": "0.0.0.0/0", "via": gw.to_string()}));
            }
        }
        Some(_) => {
            eth.insert("dhcp4".into(), json!(true));
        }
        None => {}
    }
    match &net.ipv6 {
        Some(IpConfig::Static(s)) => {
            addresses.push(format!("{}/{}", s.address, s.prefix_len));
            if let Some(gw) = s.gateway {
                routes.push(json!({"to": "::/0", "via": gw.to_string()}));
            }
        }
        Some(IpConfig::Dhcp) => {
            eth.insert("dhcp6".into(), json!(true));
        }
        Some(IpConfig::Auto) => {
            eth.insert("accept-ra".into(), json!(true));
        }
        None => {}
    }
    if !addresses.is_empty() {
        eth.insert("addresses".into(), json!(addresses));
    }
    if !routes.is_empty() {
        eth.insert("routes".into(), json!(routes));
    }
    if !net.dns_servers.is_empty() {
        let dns: Vec<String> = net.dns_servers.iter().map(ToString::to_string).collect();
        eth.insert("nameservers".into(), json!({"addresses": dns}));
    }
    json!({"version": 2, "ethernets": {"nic0": eth}})
}

/// `Guest.CustomizationSpec` for the configured mode (`None` = skip).
fn customization_spec(
    mode: Customization,
    g: &GuestSettings<'_>,
    warnings: &mut Vec<String>,
) -> Result<Option<Value>, IntegrationError> {
    let (host, domain) = split_fqdn(g.hostname);
    let dns: Vec<String> = g
        .network
        .dns_servers
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut adapter = json!({"ipv4": ipv4_settings(g.network)});
    if let Some(v6) = ipv6_settings(g.network) {
        adapter["ipv6"] = v6;
    }
    let global_dns = json!({
        "dns_servers": dns,
        "dns_suffix_list": domain.map(|d| vec![d.to_owned()]).unwrap_or_default(),
    });
    Ok(match mode {
        Customization::None => {
            if g.password.is_some() || !g.ssh_keys.is_empty() || g.user_data.is_some() {
                warnings.push(
                    "guest customization is disabled; credentials and user data were not applied"
                        .into(),
                );
            }
            None
        }
        Customization::CloudInit => {
            let metadata = json!({
                "instance-id": format!("{}-{}", g.name, Utc::now().timestamp()),
                "local-hostname": host,
                "network": cloudinit_network(g.network),
            });
            let mut cloud = json!({"metadata": metadata.to_string()});
            if let Some(userdata) = (CloudConfig {
                hostname: Some(g.hostname),
                root_password: g.password,
                ssh_keys: g.ssh_keys,
                user_data: g.user_data,
            })
            .render()
            {
                cloud["userdata"] = json!(userdata);
            }
            Some(json!({
                "configuration_spec": {"cloud_config": {"type": "CLOUDINIT", "cloudinit": cloud}},
                "global_DNS_settings": {},
                "interfaces": [],
            }))
        }
        Customization::Linux => {
            if g.password.is_some() || !g.ssh_keys.is_empty() {
                warnings.push(
                    "Linux prep cannot set passwords or SSH keys; use cloud-init customization"
                        .into(),
                );
            }
            if g.user_data.is_some() {
                warnings.push("user data ignored: Linux prep has no cloud-init".into());
            }
            let mut linux = json!({
                "hostname": {"type": "FIXED", "fixed_name": host},
                "domain": domain.unwrap_or("localdomain"),
            });
            if let Some(tz) = extra_str(g.extra, "time_zone") {
                linux["time_zone"] = json!(tz);
            }
            Some(json!({
                "configuration_spec": {"linux_config": linux},
                "global_DNS_settings": global_dns,
                "interfaces": [{"adapter": adapter}],
            }))
        }
        Customization::Windows => {
            let computer: String = host
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(15)
                .collect();
            if computer.is_empty() {
                return Err(IntegrationError::InvalidInput(
                    "cannot derive a Windows computer name".into(),
                ));
            }
            let mut unattended = json!({
                "auto_logon": false,
                "auto_logon_count": 1,
                "time_zone": extra_u64(g.extra, "windows_time_zone")?.unwrap_or(85),
            });
            if let Some(pw) = g.password {
                unattended["password"] = json!(pw.expose_str());
            }
            if !g.ssh_keys.is_empty() || g.user_data.is_some() {
                warnings.push(
                    "SSH keys and cloud-init user data are ignored for Windows sysprep".into(),
                );
            }
            Some(json!({
                "configuration_spec": {"windows_config": {
                    "reboot": "REBOOT",
                    "sysprep": {
                        "user_data": {
                            "computer_name": {"type": "FIXED", "fixed_name": computer},
                            "full_name": "Administrator",
                            "organization": "HostingOS",
                            "product_key": extra_str(g.extra, "windows_product_key").unwrap_or(""),
                        },
                        "domain": {"type": "WORKGROUP", "workgroup": "WORKGROUP"},
                        "gui_unattended": unattended,
                    },
                }},
                "global_DNS_settings": global_dns,
                "interfaces": [{"adapter": adapter}],
            }))
        }
    })
}

#[async_trait]
impl VirtProvider for VmwareProvider {
    fn kind(&self) -> &'static str {
        kinds::VMWARE
    }

    fn capabilities(&self) -> VirtCapabilities {
        if self.settings.esxi {
            // Nothing works without vCenter; the UI should hide every action.
            return VirtCapabilities::default();
        }
        let vim = self.settings.vijson_release.is_some();
        VirtCapabilities {
            snapshots: vim,
            memory_snapshots: vim,
            live_resize: false,
            reinstall: true,
            console_kinds: vec![ConsoleKind::WebMks],
            metrics: true,
            multi_node: true,
            plan_based: false,
            async_tasks: false,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        if self.settings.esxi {
            return Err(IntegrationError::Config(ESXI_MESSAGE.into()));
        }
        self.session(Api::Rest).await?;
        let version = self
            .rest(Method::GET, &["appliance", "system", "version"], &[], None)
            .await
            .ok();
        let hosts = self
            .rest(Method::GET, &["vcenter", "host"], &[], None)
            .await?;
        let n_hosts = hosts.as_array().map_or(0, Vec::len);
        let mut details = format!("{n_hosts} host(s)");
        if self.settings.vijson_release.is_some() {
            self.session(Api::Vim).await?;
            details.push_str("; VI/JSON session ok");
        }
        Ok(ConnectionInfo {
            server_version: version.as_ref().and_then(|v| {
                let ver = v.str_at("version")?;
                Some(match v.str_at("build") {
                    Some(b) => format!("vCenter {ver} build {b}"),
                    None => format!("vCenter {ver}"),
                })
            }),
            details: Some(details),
        })
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, IntegrationError> {
        self.esxi_guard("list_nodes")?;
        let query: Vec<(&str, String)> = self
            .settings
            .cluster
            .iter()
            .map(|c| ("clusters", c.clone()))
            .collect();
        let hosts = self
            .rest(Method::GET, &["vcenter", "host"], &query, None)
            .await?;
        Ok(hosts
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| {
                let id = h.str_at("host")?.to_owned();
                Some(NodeInfo {
                    name: h.str_at("name").unwrap_or(&id).to_owned(),
                    online: h.str_at("connection_state") == Some("CONNECTED")
                        && h.str_at("power_state").is_none_or(|p| p == "POWERED_ON"),
                    extra: json!({
                        "connection_state": h.str_at("connection_state"),
                        "power_state": h.str_at("power_state"),
                    }),
                    id,
                    ..NodeInfo::default()
                })
            })
            .collect())
    }

    async fn list_templates(
        &self,
        _node: Option<&str>,
    ) -> Result<Vec<TemplateInfo>, IntegrationError> {
        self.esxi_guard("list_templates")?;
        let libraries: Vec<String> = match &self.settings.content_library {
            Some(id) => vec![id.clone()],
            None => self
                .rest(Method::GET, &["content", "library"], &[], None)
                .await?
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        };
        let mut out = Vec::new();
        for lib in libraries {
            let ids: Vec<String> = self
                .rest(
                    Method::GET,
                    &["content", "library", "item"],
                    &[("library_id", lib.clone())],
                    None,
                )
                .await?
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let items = join_all(ids.iter().map(|id| {
                let id = id.as_str();
                async move {
                    self.rest(Method::GET, &["content", "library", "item", id], &[], None)
                        .await
                }
            }))
            .await;
            for (id, item) in ids.iter().zip(items) {
                let item = item?;
                if item.str_at("type") != Some("vm-template") {
                    continue;
                }
                let name = item.str_at("name").unwrap_or(id).to_owned();
                out.push(TemplateInfo {
                    id: id.clone(),
                    os_family: guess_os_family(&name),
                    name,
                    node: None,
                    min_disk_gb: None,
                    cloud_init: self.settings.customization == Customization::CloudInit,
                    extra: json!({
                        "library_id": lib,
                        "description": item.str_at("description"),
                        "source": "content_library",
                    }),
                });
            }
        }
        Ok(out)
    }

    async fn list_vms(&self, node: Option<&str>) -> Result<Vec<VmInfo>, IntegrationError> {
        self.esxi_guard("list_vms")?;
        let query: Vec<(&str, String)> =
            node.map(|n| ("hosts", n.to_owned())).into_iter().collect();
        let vms = self
            .rest(Method::GET, &["vcenter", "vm"], &query, None)
            .await?;
        Ok(vms
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                let id = v.str_at("vm")?.to_owned();
                Some(VmInfo {
                    vm: VmRef::new(id.clone()),
                    name: v.str_at("name").unwrap_or(&id).to_owned(),
                    status: map_power(v.str_at("power_state")),
                    hostname: None,
                    cpu_cores: v.u64_at("cpu_count").and_then(|c| u32::try_from(c).ok()),
                    memory_mb: v.u64_at("memory_size_MiB"),
                    disk_gb: None,
                    ipv4: Vec::new(),
                    ipv6: Vec::new(),
                    template: None,
                    plan: None,
                    tags: Vec::new(),
                    created_at: None,
                    uptime_secs: None,
                    extra: json!({"moref": id}),
                })
            })
            .collect())
    }

    async fn get_vm(&self, vm: &VmRef) -> Result<VmInfo, IntegrationError> {
        self.esxi_guard("get_vm")?;
        let info = self.vm_info_raw(&vm.id).await?;
        let status = map_power(info.str_at("power_state"));
        let (hostname, ipv4, ipv6) = if status == VmStatus::Running {
            self.guest_details(&vm.id).await
        } else {
            (None, Vec::new(), Vec::new())
        };
        let disk = first_disk_bytes(&info);
        Ok(VmInfo {
            vm: VmRef::new(vm.id.clone()),
            name: info.str_at("name").unwrap_or(&vm.id).to_owned(),
            status,
            hostname,
            cpu_cores: info
                .get("cpu")
                .and_then(|c| c.u64_at("count"))
                .and_then(|c| u32::try_from(c).ok()),
            memory_mb: info.get("memory").and_then(|m| m.u64_at("size_MiB")),
            disk_gb: (disk > 0).then(|| bytes_to_gib(disk)),
            ipv4,
            ipv6,
            template: None,
            plan: None,
            tags: Vec::new(),
            created_at: None,
            uptime_secs: None,
            extra: json!({
                "moref": vm.id,
                "guest_os": info.str_at("guest_OS"),
                "instance_uuid": info.get("identity").and_then(|i| i.str_at("instance_uuid")),
            }),
        })
    }

    async fn create_vm(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        self.esxi_guard("create_vm")?;
        validate_spec(spec)?;
        // Same rule as reinstall: the source must be a VM template, never
        // another (customer's) VM whose disks a clone would copy.
        self.verify_template(spec.template.trim()).await?;
        let secrets: Vec<&str> = spec
            .root_password
            .iter()
            .map(SecretString::expose_str)
            .collect();
        self.provision(spec)
            .await
            .map_err(|e| redact_error(e, &secrets))
    }

    async fn power(
        &self,
        vm: &VmRef,
        action: PowerAction,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.esxi_guard("power")?;
        match action {
            PowerAction::Start => self.power_op(&vm.id, false, "start").await?,
            PowerAction::Stop => self.power_op(&vm.id, false, "stop").await?,
            PowerAction::Reset => self.power_op(&vm.id, false, "reset").await?,
            PowerAction::Shutdown => self.power_op(&vm.id, true, "shutdown").await?,
            PowerAction::Reboot => self.power_op(&vm.id, true, "reboot").await?,
        }
        Ok(None)
    }

    async fn resize(
        &self,
        vm: &VmRef,
        resize: &VmResize,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.esxi_guard("resize")?;
        if resize.cpu_cores.is_none() && resize.memory_mb.is_none() && resize.disk_gb.is_none() {
            return Err(IntegrationError::InvalidInput(
                "vSphere has no plans; set cpu_cores, memory_mb and/or disk_gb".into(),
            ));
        }
        let compute = resize.cpu_cores.is_some() || resize.memory_mb.is_some();
        let was_on = compute && self.power_state(&vm.id).await? == "POWERED_ON";
        if was_on && resize.allow_restart {
            self.ensure_off(&vm.id).await?;
        }
        if let Some(c) = resize.cpu_cores {
            self.rest(
                Method::PATCH,
                &["vcenter", "vm", &vm.id, "hardware", "cpu"],
                &[],
                Some(&json!({"count": c})),
            )
            .await?;
        }
        if let Some(m) = resize.memory_mb {
            self.rest(
                Method::PATCH,
                &["vcenter", "vm", &vm.id, "hardware", "memory"],
                &[],
                Some(&json!({"size_MiB": m})),
            )
            .await?;
        }
        if let Some(gb) = resize.disk_gb {
            self.grow_disk(&vm.id, gb).await?;
        }
        if was_on && resize.allow_restart {
            self.power_op(&vm.id, false, "start").await?;
        }
        Ok(None)
    }

    async fn reinstall(
        &self,
        vm: &VmRef,
        request: &VmReinstall,
    ) -> Result<VmProvisioned, IntegrationError> {
        self.esxi_guard("reinstall")?;
        if request.template.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "template is required".into(),
            ));
        }
        for key in &request.ssh_keys {
            validate_ssh_key(key)?;
        }
        let network: NetworkSpec = match request.extra.get("network") {
            Some(v) if !v.is_null() => serde_json::from_value(v.clone())
                .map_err(|e| IntegrationError::InvalidInput(format!("extra.network: {e}")))?,
            _ => NetworkSpec::default(),
        };
        // Before the VM is deleted: the source must be a template.
        self.verify_template(request.template.trim()).await?;
        let info = self.vm_info_raw(&vm.id).await?;
        let running = info.str_at("power_state") == Some("POWERED_ON");
        let hostname = match request.hostname.clone().filter(|h| !h.trim().is_empty()) {
            Some(h) => h,
            None if running => self.guest_details(&vm.id).await.0.unwrap_or_default(),
            None => String::new(),
        };
        let name = info.str_at("name").unwrap_or(&vm.id).to_owned();
        let spec = VmSpec {
            name: name.clone(),
            hostname: if hostname.is_empty() {
                name.clone()
            } else {
                hostname
            },
            template: request.template.clone(),
            cpu_cores: info
                .get("cpu")
                .and_then(|c| c.u64_at("count"))
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(1),
            memory_mb: info
                .get("memory")
                .and_then(|m| m.u64_at("size_MiB"))
                .unwrap_or(1024),
            disk_gb: first_disk_bytes(&info).div_ceil(GIB),
            network,
            ssh_keys: request.ssh_keys.clone(),
            root_password: request.root_password.clone(),
            cloud_init_user_data: request.cloud_init_user_data.clone(),
            start_after_create: true,
            extra: request.extra.clone(),
            ..VmSpec::default()
        };
        let secrets: Vec<&str> = request
            .root_password
            .iter()
            .map(SecretString::expose_str)
            .collect();
        self.power_op(&vm.id, false, "stop").await?;
        self.rest(Method::DELETE, &["vcenter", "vm", &vm.id], &[], None)
            .await?;
        let mut provisioned = self
            .provision(&spec)
            .await
            .map_err(|e| redact_error(e, &secrets))?;
        provisioned.extra["replaced_moref"] = json!(vm.id);
        Ok(provisioned)
    }

    async fn delete_vm(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        self.esxi_guard("delete_vm")?;
        if self.power_state(&vm.id).await? != "POWERED_OFF" {
            self.power_op(&vm.id, false, "stop").await?;
        }
        self.rest(Method::DELETE, &["vcenter", "vm", &vm.id], &[], None)
            .await?;
        Ok(None)
    }

    async fn list_snapshots(&self, vm: &VmRef) -> Result<Vec<SnapshotInfo>, IntegrationError> {
        self.esxi_guard("list_snapshots")?;
        self.snapshot_tree(&vm.id).await
    }

    async fn create_snapshot(
        &self,
        vm: &VmRef,
        request: &SnapshotRequest,
    ) -> Result<SnapshotCreated, IntegrationError> {
        self.esxi_guard("create_snapshot")?;
        self.vim_release("snapshots (set vijson_release; vCenter 8.0 U1+)")?;
        let mut body = json!({
            "name": request.name.trim(),
            "memory": request.include_memory,
            "quiesce": false,
        });
        if let Some(d) = request.description.as_deref() {
            body["description"] = json!(d);
        }
        let task = self
            .vim(
                Method::POST,
                &["VirtualMachine", &vm.id, "CreateSnapshot_Task"],
                Some(&body),
            )
            .await?;
        let task_id = moref(&task).ok_or_else(|| {
            IntegrationError::Transport("CreateSnapshot_Task returned no task".into())
        })?;
        let result = self.wait_vim_task(&task_id).await?;
        let snapshot_id = moref(&result).ok_or_else(|| {
            IntegrationError::Transport("snapshot task returned no snapshot".into())
        })?;
        Ok(SnapshotCreated {
            snapshot_id,
            task: None,
        })
    }

    async fn rollback_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.esxi_guard("rollback_snapshot")?;
        self.own_snapshot(&vm.id, snapshot_id).await?;
        let task = self
            .vim(
                Method::POST,
                &[
                    "VirtualMachineSnapshot",
                    snapshot_id,
                    "RevertToSnapshot_Task",
                ],
                Some(&json!({})),
            )
            .await?;
        let task_id = moref(&task).ok_or_else(|| {
            IntegrationError::Transport("RevertToSnapshot_Task returned no task".into())
        })?;
        self.wait_vim_task(&task_id).await?;
        Ok(None)
    }

    async fn delete_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.esxi_guard("delete_snapshot")?;
        self.own_snapshot(&vm.id, snapshot_id).await?;
        let task = self
            .vim(
                Method::POST,
                &["VirtualMachineSnapshot", snapshot_id, "RemoveSnapshot_Task"],
                Some(&json!({"removeChildren": false, "consolidate": true})),
            )
            .await?;
        let task_id = moref(&task).ok_or_else(|| {
            IntegrationError::Transport("RemoveSnapshot_Task returned no task".into())
        })?;
        self.wait_vim_task(&task_id).await?;
        Ok(None)
    }

    async fn console(&self, vm: &VmRef) -> Result<ConsoleAccess, IntegrationError> {
        self.esxi_guard("console")?;
        let v = self
            .rest(
                Method::POST,
                &["vcenter", "vm", &vm.id, "console", "tickets"],
                &[],
                Some(&json!({"type": "WEBMKS"})),
            )
            .await?;
        let ticket = v.str_at("ticket").ok_or_else(|| {
            IntegrationError::Transport("console ticket response without ticket".into())
        })?;
        Ok(ConsoleAccess {
            kind: ConsoleKind::WebMks,
            url: ticket.to_owned(),
            ticket: Some(SecretString::from(ticket)),
            password: None,
            port: None,
            expires_at: None,
            extra: json!({"note": "WebMKS ticket URI (one-time, short-lived); open it with the VMware HTML console SDK."}),
        })
    }

    async fn metrics(&self, vm: &VmRef) -> Result<VmMetrics, IntegrationError> {
        self.esxi_guard("metrics")?;
        let info = self.vm_info_raw(&vm.id).await?;
        let disk = first_disk_bytes(&info);
        let mut m = VmMetrics {
            collected_at: Utc::now(),
            status: map_power(info.str_at("power_state")),
            cpu_usage_percent: None,
            memory_used_mb: None,
            memory_total_mb: info.get("memory").and_then(|m| m.u64_at("size_MiB")),
            disk_used_gb: None,
            disk_total_gb: (disk > 0).then(|| bytes_to_gib_f64(disk as f64)),
            disk_read_bps: None,
            disk_write_bps: None,
            net_in_bps: None,
            net_out_bps: None,
            bandwidth_used_gb: None,
            uptime_secs: None,
        };
        if self.settings.vijson_release.is_some() {
            let summary = self
                .vim(Method::GET, &["VirtualMachine", &vm.id, "summary"], None)
                .await?;
            let quick = summary.get("quickStats").cloned().unwrap_or(Value::Null);
            let max_cpu = summary
                .get("runtime")
                .and_then(|r| r.f64_at("maxCpuUsage"))
                .filter(|m| *m > 0.0);
            m.cpu_usage_percent = quick
                .f64_at("overallCpuUsage")
                .zip(max_cpu)
                .map(|(used, max)| (used / max * 100.0).clamp(0.0, 100.0));
            m.memory_used_mb = quick.u64_at("guestMemoryUsage");
            m.uptime_secs = quick.u64_at("uptimeSeconds");
            m.disk_used_gb = summary
                .get("storage")
                .and_then(|s| s.f64_at("committed"))
                .map(bytes_to_gib_f64);
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::integrations::virt::StaticIp;

    fn net() -> NetworkSpec {
        NetworkSpec {
            ipv4: Some(IpConfig::Static(StaticIp {
                address: "203.0.113.20".parse().unwrap(),
                prefix_len: 24,
                gateway: Some("203.0.113.1".parse().unwrap()),
            })),
            ipv6: Some(IpConfig::Auto),
            dns_servers: vec!["1.1.1.1".parse().unwrap()],
            ..NetworkSpec::default()
        }
    }

    #[test]
    fn cloudinit_customization() {
        let pw = SecretString::from("pw123456");
        let n = net();
        let mut warnings = Vec::new();
        let g = GuestSettings {
            name: "svc-1",
            hostname: "vps1.example.com",
            password: Some(&pw),
            ssh_keys: &[],
            user_data: None,
            network: &n,
            extra: &Value::Null,
        };
        let spec = customization_spec(Customization::CloudInit, &g, &mut warnings)
            .unwrap()
            .unwrap();
        let cloud = &spec["configuration_spec"]["cloud_config"];
        assert_eq!(cloud["type"], "CLOUDINIT");
        let meta: Value =
            serde_json::from_str(cloud["cloudinit"]["metadata"].as_str().unwrap()).unwrap();
        assert_eq!(meta["local-hostname"], "vps1");
        assert_eq!(
            meta["network"]["ethernets"]["nic0"]["addresses"][0],
            "203.0.113.20/24"
        );
        assert_eq!(
            meta["network"]["ethernets"]["nic0"]["routes"][0]["via"],
            "203.0.113.1"
        );
        assert!(
            cloud["cloudinit"]["userdata"]
                .as_str()
                .unwrap()
                .contains("pw123456")
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn linux_and_windows_customization() {
        let n = net();
        let pw = SecretString::from("pw");
        let mut warnings = Vec::new();
        let g = GuestSettings {
            name: "svc-1",
            hostname: "web-server-number-one.example.com",
            password: Some(&pw),
            ssh_keys: &[],
            user_data: None,
            network: &n,
            extra: &Value::Null,
        };
        let linux = customization_spec(Customization::Linux, &g, &mut warnings)
            .unwrap()
            .unwrap();
        assert_eq!(
            linux["configuration_spec"]["linux_config"]["domain"],
            "example.com"
        );
        assert_eq!(linux["interfaces"][0]["adapter"]["ipv4"]["type"], "STATIC");
        assert_eq!(linux["interfaces"][0]["adapter"]["ipv6"]["type"], "DHCP");
        assert_eq!(warnings.len(), 1);
        let win = customization_spec(Customization::Windows, &g, &mut warnings)
            .unwrap()
            .unwrap();
        let sysprep = &win["configuration_spec"]["windows_config"]["sysprep"];
        assert_eq!(
            sysprep["user_data"]["computer_name"]["fixed_name"],
            "web-server-numb"
        );
        assert_eq!(sysprep["gui_unattended"]["password"], "pw");
        assert!(
            customization_spec(Customization::None, &g, &mut warnings)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn snapshot_tree_flattening() {
        let tree = json!({
            "snapshot": {"_typeName": "ManagedObjectReference", "type": "VirtualMachineSnapshot", "value": "snapshot-1"},
            "name": "base", "createTime": "2026-01-02T03:04:05Z", "state": "poweredOff",
            "childSnapshotList": [{
                "snapshot": {"_typeName": "ManagedObjectReference", "type": "VirtualMachineSnapshot", "value": "snapshot-2"},
                "name": "child", "state": "poweredOn"
            }]
        });
        let mut out = Vec::new();
        flatten_snapshots(&tree, None, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].parent.as_deref(), Some("snapshot-1"));
        assert!(out[1].includes_memory);
        assert!(!out[0].includes_memory);
    }

    #[test]
    fn helpers() {
        assert_eq!(backing_type("dvportgroup-21"), "DISTRIBUTED_PORTGROUP");
        assert_eq!(backing_type("network-13"), "STANDARD_PORTGROUP");
        assert_eq!(map_power(Some("SUSPENDED")), VmStatus::Paused);
        let info = json!({"disks": {"2001": {"capacity": 10737418240_u64}, "2000": {"capacity": 21474836480_u64}}});
        assert_eq!(first_disk_bytes(&info), 20 * GIB);
    }
}
