//! Proxmox VE (`kind = "proxmox"`) — `https://<host>:8006/api2/json`.
//!
//! * Auth: API token, `Authorization: PVEAPIToken=USER@REALM!TOKENID=SECRET`
//!   (no CSRF token needed for tokens).
//! * Clusters: every VM call first resolves the VM's current node and guest
//!   type through `GET /cluster/resources?type=vm`, so [`VmRef::node`] may be
//!   stale (after a migration) or absent.
//! * QEMU VMs are created by **full-cloning a template** (`template` = template
//!   VMID) and configured through cloud-init (`ciuser`, `cipassword`,
//!   `sshkeys`, `ipconfig0`, `nameserver`, `searchdomain`). LXC containers are
//!   created from an **OS template volume** (`template` = `storage:vztmpl/...`)
//!   or when `extra.guest_type = "lxc"`.
//! * Create and reinstall only accept what `list_templates` offers: a QEMU
//!   guest flagged `template`, or a `vztmpl` volume listed on the target
//!   node. Regular guests (other customers' VMs), backups
//!   (`storage:backup/vzdump-…`) and other volumes are refused (on reinstall
//!   before the old guest is destroyed).
//! * Long operations return a UPID ([`TaskRef`]); `task_status` polls
//!   `GET /nodes/{node}/tasks/{upid}/status`. Multi-step flows (clone →
//!   configure → resize) wait for intermediate tasks internally (bounded by
//!   `task_timeout_secs`) and return the final start task.
//! * Error messages: newer PVE versions put them into the JSON body
//!   (`message`, `errors`), older ones only into the HTTP reason phrase; both
//!   are read. PVE answers 500 for "does not exist" / "locked" conditions —
//!   those are mapped to `NotFound` / `Conflict`.
//!
//!
//! Extended operations (checked against the API viewer schema,
//! <https://pve.proxmox.com/pve-docs/api-viewer/> — `apidoc.js`, PVE 9):
//!
//! * Console relay: `POST /nodes/{node}/{qemu|lxc}/{vmid}/vncproxy`
//!   (`websocket=1`; returns `port`, `ticket`, optional `password`) then
//!   `GET …/vncwebsocket?port=&vncticket=` upgraded server-side with the API
//!   token. The VNC password (or the ticket) is used by the relay for RFB VNC
//!   authentication and never leaves HostingOS. Needs `VM.Console`.
//! * Password reset: `POST /nodes/{node}/qemu/{vmid}/agent/set-user-password`
//!   (`username`, `password`). Prerequisite: `agent: 1` in the VM config and
//!   qemu-guest-agent running in the guest; PVE 9 requires
//!   `VM.GuestAgent.Unrestricted`. LXC: unsupported (no API).
//! * Metrics history / bandwidth: `GET …/rrddata?timeframe=&cf=AVERAGE`.
//!   Bandwidth is an **estimate** (average rates × RRD step, current UTC
//!   month); Proxmox has no traffic counter.
//! * Backups: `GET /nodes/{node}/storage?content=backup`, then
//!   `GET /nodes/{node}/storage/{storage}/content?content=backup&vmid=`;
//!   restore: `POST /nodes/{node}/qemu` (`archive`, `vmid`, `force=1`) or
//!   `POST /nodes/{node}/lxc` (`ostemplate`, `restore=1`, `force=1`,
//!   `storage`), after stopping the guest. Only volids listed for the guest
//!   are accepted.
//! * ISO: `content=iso` storage content of the node; mount with
//!   `POST …/qemu/{vmid}/config` `ide2=<volid>,media=cdrom` and
//!   `boot=order=ide2;<disk>`; unmount with `ide2=none,media=cdrom`.
//! * Firewall: `GET/PUT …/firewall/options` (`enable`, `policy_in=DROP`) and
//!   `GET/POST/DELETE …/firewall/rules[/{pos}]`. Only rules whose comment
//!   starts with `hostingos` are managed; filtering needs the datacenter
//!   firewall on and `firewall=1` on the NIC.
//! * No reverse DNS or rescue mode in the PVE API.
//!
//! `VmSpec.extra` / `VmReinstall.extra` keys: `guest_type` (`qemu`|`lxc`),
//! `vmid`, `pool`, `cpu_type`, `sockets`, `nic_model` (default `virtio`),
//! `firewall` (bool), `qemu_agent` (bool), `onboot` (bool, default true),
//! `ciuser`, `cicustom` (needed for raw cloud-init user data: Proxmox cannot
//! receive user data through the API, only a snippet reference), `ciupgrade`
//! (bool), `swap_mb` (LXC, default 512), `unprivileged` (LXC, default true),
//! `features` (LXC, e.g. `nesting=1`), `storage` (reinstall target storage).

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use futures::future::join_all;
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::virt::{
    BackupInfo, BandwidthUsage, ConsoleAccess, ConsoleKind, ConsoleTransport, ConsoleUpstream,
    FirewallDirection, FirewallProtocol, FirewallRule, FirewallState, IpConfig, IsoImage,
    MetricSample, MetricsHistory, MetricsRange, NetworkSpec, NodeInfo, OsFamily, PasswordReset,
    PasswordResetRequest, PowerAction, SnapshotCreated, SnapshotInfo, SnapshotRequest, TaskRef,
    TaskState, TemplateInfo, VirtCapabilities, VirtFeatures, VmExtras, VmInfo, VmMetrics,
    VmProvisioned, VmRef, VmReinstall, VmResize, VmSpec, VmStatus,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, SelectOption, VirtProvider,
    kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::{Value, json};

use crate::http::{
    ClientOptions, HttpClient, RawResponse, RetryPolicy, build_client, execute, join_path,
    map_reqwest_error, normalize_base, redact, redact_error,
};
use crate::util::{
    GIB, JsonExt, bytes_to_gib, bytes_to_gib_f64, bytes_to_mib, dns_name, extra_bool, extra_str,
    extra_u64, generate_password, parse_size, percent_encode_strict, poll_until, snapshot_name,
    split_fqdn, split_ips, static_ips, validate_spec,
};

const PROVIDER: &str = "proxmox";

/// 500 is deliberately absent: PVE uses it for application errors
/// ("VM is locked", "does not exist"); 595/596 are pveproxy ↔ node
/// connection failures inside a cluster.
const RETRY_STATUSES: &[u16] = &[429, 502, 503, 504, 595, 596];

/// Admin-form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::PROXMOX, "Proxmox VE", IntegrationCategory::Virt)
        .description(
            "Proxmox VE 7/8/9 standalone or cluster. QEMU VMs are full clones of a cloud-init \
             template (template = template VMID); LXC containers use an OS template volume \
             (storage:vztmpl/...). Create an API token with privilege separation and grant it \
             VM.*, Datastore.AllocateSpace, Datastore.Audit, Sys.Audit, Pool.Allocate (if pools) \
             and SDN.Use on the bridges.",
        )
        .docs_url("https://pve.proxmox.com/pve-docs/api-viewer/")
        .config_field(
            FieldSpec::url("api_url", "API URL")
                .required()
                .placeholder("https://pve1.example.com:8006")
                .help("Any cluster node; /api2/json is appended automatically."),
        )
        .config_field(
            FieldSpec::text("token_id", "API token ID")
                .required()
                .placeholder("hostingos@pve!provisioning")
                .help("USER@REALM!TOKENID"),
        )
        .config_field(
            FieldSpec::boolean("skip_tls_verify", "Skip TLS certificate verification")
                .default_value(false)
                .help("Only for self-signed certificates on a trusted network."),
        )
        .config_field(
            FieldSpec::text("default_node", "Default node")
                .help("Used when the product does not pin a node; otherwise the template's node."),
        )
        .config_field(
            FieldSpec::text("default_storage", "Default storage")
                .placeholder("local-lvm")
                .help("Target storage for full clones and LXC root disks."),
        )
        .config_field(FieldSpec::text("default_bridge", "Default bridge").default_value("vmbr0"))
        .config_field(
            FieldSpec::select(
                "default_guest_type",
                "Default guest type",
                vec![
                    SelectOption::new("qemu", "QEMU/KVM virtual machine"),
                    SelectOption::new("lxc", "LXC container"),
                ],
            )
            .default_value("qemu"),
        )
        .config_field(
            FieldSpec::text("pool", "Resource pool").help("Optional pool for new guests."),
        )
        .config_field(
            FieldSpec::text("cpu_type", "CPU type")
                .placeholder("x86-64-v2-AES")
                .help("QEMU `cpu` option; empty keeps the template's setting."),
        )
        .config_field(
            FieldSpec::text("ciuser", "cloud-init user")
                .default_value("root")
                .help("User that receives the password and SSH keys."),
        )
        .config_field(FieldSpec::number("timeout_secs", "HTTP timeout (s)").default_value(30))
        .config_field(
            FieldSpec::number("task_timeout_secs", "Task wait limit (s)")
                .default_value(900)
                .help("Upper bound for clone/stop/delete tasks awaited inside one operation."),
        )
        .config_field(
            FieldSpec::number("task_poll_ms", "Task poll interval (ms)").default_value(2000),
        )
        .secret_field(FieldSpec::password("token_secret", "API token secret").required())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestKind {
    Qemu,
    Lxc,
}

impl GuestKind {
    fn path(self) -> &'static str {
        match self {
            Self::Qemu => "qemu",
            Self::Lxc => "lxc",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "qemu" | "kvm" | "vm" => Some(Self::Qemu),
            "lxc" | "ct" | "container" => Some(Self::Lxc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Settings {
    default_node: Option<String>,
    default_storage: Option<String>,
    default_bridge: String,
    default_guest: GuestKind,
    pool: Option<String>,
    cpu_type: Option<String>,
    ciuser: Option<String>,
    task_timeout: Duration,
    task_poll: Duration,
    /// `skip_tls_verify` (the console relay applies the same rule).
    accept_invalid_certs: bool,
}

/// A guest as listed by `/cluster/resources`.
#[derive(Debug, Clone)]
struct Guest {
    node: String,
    vmid: u64,
    kind: GuestKind,
    status: String,
    name: Option<String>,
    template: bool,
    lock: Option<String>,
    raw: Value,
}

impl Guest {
    fn from_resource(v: &Value) -> Option<Self> {
        let kind = match v.str_at("type")? {
            "qemu" => GuestKind::Qemu,
            "lxc" => GuestKind::Lxc,
            _ => return None,
        };
        Some(Self {
            node: v.str_at("node")?.to_owned(),
            vmid: v.u64_at("vmid")?,
            kind,
            status: v.str_at("status").unwrap_or("unknown").to_owned(),
            name: v.str_at("name").map(str::to_owned),
            template: v.bool_at("template").unwrap_or(false),
            lock: v.str_at("lock").map(str::to_owned),
            raw: v.clone(),
        })
    }

    fn vmid_s(&self) -> String {
        self.vmid.to_string()
    }

    fn vm_ref(&self) -> VmRef {
        VmRef::on_node(self.node.clone(), self.vmid_s())
    }

    fn task(&self, upid: String) -> TaskRef {
        TaskRef {
            id: upid,
            node: Some(self.node.clone()),
        }
    }
}

/// Proxmox VE provider. Build with [`ProxmoxProvider::from_config`] or [`crate::build`].
pub struct ProxmoxProvider {
    http: HttpClient,
    base: url::Url,
    auth: HeaderValue,
    token_secret: SecretString,
    retry: RetryPolicy,
    settings: Settings,
}

impl fmt::Debug for ProxmoxProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxmoxProvider")
            .field("base", &self.base.as_str())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl ProxmoxProvider {
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
        let base = normalize_base(cfg.url("api_url")?, &["api2", "json"])?;
        let token_id = cfg.str("token_id")?;
        if !token_id.contains('@') || !token_id.contains('!') {
            return Err(IntegrationError::Config(
                "config.token_id must look like USER@REALM!TOKENID".into(),
            ));
        }
        let token_secret = sec.secret("token_secret")?;
        let mut auth = HeaderValue::from_str(&format!(
            "PVEAPIToken={token_id}={}",
            token_secret.expose_str()
        ))
        .map_err(|_| IntegrationError::Config("API token contains invalid characters".into()))?;
        auth.set_sensitive(true);
        let default_guest = match cfg.opt_str("default_guest_type")? {
            Some(s) => GuestKind::parse(s).ok_or_else(|| {
                IntegrationError::Config("config.default_guest_type must be qemu or lxc".into())
            })?,
            None => GuestKind::Qemu,
        };
        let settings = Settings {
            default_node: cfg.opt_str("default_node")?.map(str::to_owned),
            default_storage: cfg.opt_str("default_storage")?.map(str::to_owned),
            default_bridge: cfg.string_or("default_bridge", "vmbr0")?,
            default_guest,
            pool: cfg.opt_str("pool")?.map(str::to_owned),
            cpu_type: cfg.opt_str("cpu_type")?.map(str::to_owned),
            ciuser: Some(cfg.string_or("ciuser", "root")?).filter(|s| !s.is_empty()),
            task_timeout: Duration::from_secs(cfg.u64_or("task_timeout_secs", 900)?.max(5)),
            task_poll: Duration::from_millis(cfg.u64_or("task_poll_ms", 2000)?.max(5)),
            accept_invalid_certs: cfg.bool_or("skip_tls_verify", false)?,
        };
        Ok(Self {
            http: build_client(ClientOptions::from_config(&cfg, 30, policy)?, &base)?,
            base,
            auth,
            token_secret,
            retry: RetryPolicy::standard().with_statuses(RETRY_STATUSES),
            settings,
        })
    }

    // ------------------------------------------------------------------
    // HTTP
    // ------------------------------------------------------------------

    async fn call(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, String)],
        form: Option<&[(String, String)]>,
    ) -> Result<Value, IntegrationError> {
        let mut url = join_path(&self.base, segments)?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let idempotent = method == Method::GET;
        let mut builder = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, self.auth.clone());
        if let Some(form) = form {
            builder = builder.form(form);
        }
        let request = builder.build().map_err(map_reqwest_error)?;
        let raw = execute(&self.http, request, idempotent, &self.retry, PROVIDER).await?;
        if !raw.is_success() {
            return Err(self.error_from(&raw));
        }
        let body: Value = raw.json()?;
        Ok(body.get("data").cloned().unwrap_or(Value::Null))
    }

    async fn get(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<Value, IntegrationError> {
        self.call(Method::GET, segments, query, None).await
    }

    async fn post(
        &self,
        segments: &[&str],
        form: &[(String, String)],
    ) -> Result<Value, IntegrationError> {
        self.call(Method::POST, segments, &[], Some(form)).await
    }

    async fn put(
        &self,
        segments: &[&str],
        form: &[(String, String)],
    ) -> Result<Value, IntegrationError> {
        self.call(Method::PUT, segments, &[], Some(form)).await
    }

    async fn delete(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<Value, IntegrationError> {
        self.call(Method::DELETE, segments, query, None).await
    }

    fn error_from(&self, raw: &RawResponse) -> IntegrationError {
        let body: Option<Value> = serde_json::from_slice(&raw.body).ok();
        let mut message = body
            .as_ref()
            .and_then(|b| b.str_at("message"))
            .map(str::to_owned)
            .unwrap_or_else(|| raw.status_text());
        if let Some(errors) = body
            .as_ref()
            .and_then(|b| b.get("errors"))
            .and_then(Value::as_object)
        {
            let details: Vec<String> = errors
                .iter()
                .map(|(k, v)| {
                    let text = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                    format!("{k}: {}", text.trim())
                })
                .collect();
            if !details.is_empty() {
                message = format!("{} ({})", message.trim(), details.join("; "));
            }
        }
        let message = redact(message.trim(), &[self.token_secret.expose_str()]);
        classify(raw, message)
    }

    // ------------------------------------------------------------------
    // Lookups
    // ------------------------------------------------------------------

    async fn resources(&self, kind: &str) -> Result<Vec<Value>, IntegrationError> {
        let data = self
            .get(&["cluster", "resources"], &[("type", kind.to_owned())])
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    async fn guests(&self) -> Result<Vec<Guest>, IntegrationError> {
        Ok(self
            .resources("vm")
            .await?
            .iter()
            .filter_map(Guest::from_resource)
            .collect())
    }

    async fn locate(&self, vm: &VmRef) -> Result<Guest, IntegrationError> {
        let vmid = parse_vmid(&vm.id)?;
        self.guests()
            .await?
            .into_iter()
            .find(|g| g.vmid == vmid)
            .ok_or_else(|| IntegrationError::NotFound(format!("Proxmox guest {vmid}")))
    }

    async fn next_vmid(&self) -> Result<u64, IntegrationError> {
        let data = self.get(&["cluster", "nextid"], &[]).await?;
        crate::util::value_u64(&data).ok_or_else(|| {
            IntegrationError::Transport("unexpected /cluster/nextid response".into())
        })
    }

    async fn guest_config(&self, g: &Guest) -> Result<Value, IntegrationError> {
        self.get(
            &["nodes", &g.node, g.kind.path(), &g.vmid_s(), "config"],
            &[],
        )
        .await
    }

    /// Online node with the most free memory.
    async fn pick_node(&self) -> Result<String, IntegrationError> {
        let nodes = self.get(&["nodes"], &[]).await?;
        nodes
            .as_array()
            .into_iter()
            .flatten()
            .filter(|n| n.str_at("status") == Some("online"))
            .max_by_key(|n| {
                n.u64_at("maxmem")
                    .unwrap_or(0)
                    .saturating_sub(n.u64_at("mem").unwrap_or(0))
            })
            .and_then(|n| n.str_at("node"))
            .map(str::to_owned)
            .ok_or_else(|| IntegrationError::Remote {
                status: None,
                code: None,
                message: "no online Proxmox node available".into(),
            })
    }

    // ------------------------------------------------------------------
    // Tasks
    // ------------------------------------------------------------------

    async fn task_state_on(&self, node: &str, upid: &str) -> Result<TaskState, IntegrationError> {
        let data = self
            .get(&["nodes", node, "tasks", upid, "status"], &[])
            .await?;
        Ok(match data.str_at("status") {
            Some("running") => TaskState::Running {
                progress_percent: None,
            },
            _ => {
                let exit = data.str_at("exitstatus").unwrap_or("unknown");
                if exit == "OK" || exit.starts_with("WARNINGS") {
                    TaskState::Succeeded
                } else {
                    TaskState::Failed {
                        message: exit.to_owned(),
                    }
                }
            }
        })
    }

    /// Waits for a UPID to finish; `Failed` becomes an error.
    async fn wait_task(&self, upid: &str) -> Result<(), IntegrationError> {
        let node = upid_node(upid)?;
        let node = node.as_str();
        let state = poll_until(
            self.settings.task_timeout,
            self.settings.task_poll,
            move || async move {
                Ok(match self.task_state_on(node, upid).await? {
                    TaskState::Running { .. } => None,
                    done => Some(done),
                })
            },
        )
        .await
        .map_err(|err| match err {
            IntegrationError::Timeout => IntegrationError::Other(format!(
                "timed out after {}s waiting for Proxmox task {upid}",
                self.settings.task_timeout.as_secs()
            )),
            other => other,
        })?;
        match state {
            TaskState::Failed { message } => Err(task_failure(upid, &message)),
            _ => Ok(()),
        }
    }

    /// Waits when `value` is a UPID string (async PVE endpoints may return null).
    async fn wait_if_task(&self, value: &Value) -> Result<(), IntegrationError> {
        match value.as_str().filter(|s| s.starts_with("UPID:")) {
            Some(upid) => self.wait_task(upid).await,
            None => Ok(()),
        }
    }

    async fn status_action(&self, g: &Guest, action: &str) -> Result<String, IntegrationError> {
        let v = self
            .post(
                &[
                    "nodes",
                    &g.node,
                    g.kind.path(),
                    &g.vmid_s(),
                    "status",
                    action,
                ],
                &[],
            )
            .await?;
        upid_of(&v)
    }

    async fn stop_and_wait(&self, g: &Guest) -> Result<(), IntegrationError> {
        if g.status == "stopped" {
            return Ok(());
        }
        let upid = self.status_action(g, "stop").await?;
        self.wait_task(&upid).await
    }

    /// Best-effort removal of a half-created guest.
    async fn destroy_quietly(&self, node: &str, kind: GuestKind, vmid: u64) {
        let g = Guest {
            node: node.to_owned(),
            vmid,
            kind,
            status: "unknown".into(),
            name: None,
            template: false,
            lock: None,
            raw: Value::Null,
        };
        if let Ok(upid) = self.status_action(&g, "stop").await {
            let _ = self.wait_task(&upid).await;
        }
        let result = self
            .delete(
                &["nodes", node, kind.path(), &vmid.to_string()],
                &[
                    ("purge", "1".into()),
                    ("destroy-unreferenced-disks", "1".into()),
                ],
            )
            .await;
        match result {
            Ok(v) => {
                let _ = self.wait_if_task(&v).await;
            }
            Err(err) => tracing::warn!(
                provider = PROVIDER,
                vmid,
                error = %err,
                "cleanup of partially created guest failed"
            ),
        }
    }

    // ------------------------------------------------------------------
    // Create
    // ------------------------------------------------------------------

    fn guest_kind_for(&self, template: &str, extra: &Value) -> Result<GuestKind, IntegrationError> {
        if let Some(kind) = extra_str(extra, "guest_type") {
            return GuestKind::parse(kind).ok_or_else(|| {
                IntegrationError::InvalidInput("extra.guest_type must be qemu or lxc".into())
            });
        }
        if template.contains(":vztmpl/") {
            return Ok(GuestKind::Lxc);
        }
        if template.trim().parse::<u64>().is_ok() {
            return Ok(GuestKind::Qemu);
        }
        Ok(self.settings.default_guest)
    }

    fn ciuser<'a>(&'a self, extra: &'a Value) -> Option<&'a str> {
        extra_str(extra, "ciuser").or(self.settings.ciuser.as_deref())
    }

    /// cloud-init + hardware options for a QEMU guest.
    fn qemu_config_form(&self, spec: &VmSpec) -> Result<Vec<(String, String)>, IntegrationError> {
        let extra = &spec.extra;
        let mut form = Vec::new();
        let mut push = |k: &str, v: String| form.push((k.to_owned(), v));
        let label = if spec.hostname.trim().is_empty() {
            &spec.name
        } else {
            &spec.hostname
        };
        push("name", guest_name(label)?);
        push("cores", spec.cpu_cores.to_string());
        push(
            "sockets",
            extra_u64(extra, "sockets")?.unwrap_or(1).max(1).to_string(),
        );
        push("memory", spec.memory_mb.to_string());
        if let Some(cpu) = extra_str(extra, "cpu_type").or(self.settings.cpu_type.as_deref()) {
            push("cpu", cpu.to_owned());
        }
        if let Some(user) = self.ciuser(extra) {
            push("ciuser", user.to_owned());
        }
        if let Some(pw) = &spec.root_password {
            push("cipassword", pw.expose_str().to_owned());
        }
        if !spec.ssh_keys.is_empty() {
            push("sshkeys", encode_sshkeys(&spec.ssh_keys));
        }
        if let Some(ipconfig) = ipconfig(&spec.network)? {
            push("ipconfig0", ipconfig);
        }
        if !spec.network.dns_servers.is_empty() {
            push("nameserver", join_ips(&spec.network.dns_servers));
        }
        if let Some(domain) = split_fqdn(spec.hostname.trim()).1 {
            push("searchdomain", domain.to_owned());
        }
        push(
            "net0",
            qemu_net(
                &spec.network,
                &self.settings.default_bridge,
                extra_str(extra, "nic_model").unwrap_or("virtio"),
                extra_bool(extra, "firewall"),
            ),
        );
        if !spec.tags.is_empty() {
            push("tags", pve_tags(&spec.tags));
        }
        push(
            "description",
            format!("Managed by HostingOS ({})", spec.name),
        );
        push(
            "onboot",
            bool01(extra_bool(extra, "onboot").unwrap_or(true)),
        );
        if let Some(agent) = extra_bool(extra, "qemu_agent") {
            push("agent", bool01(agent));
        }
        if let Some(up) = extra_bool(extra, "ciupgrade") {
            push("ciupgrade", bool01(up));
        }
        match (extra_str(extra, "cicustom"), &spec.cloud_init_user_data) {
            (Some(custom), _) => push("cicustom", custom.to_owned()),
            (None, Some(data)) if !data.trim().is_empty() => {
                return Err(IntegrationError::InvalidInput(
                    "Proxmox cannot receive raw cloud-init user data through its API; upload \
                     it as a snippet and pass extra.cicustom (e.g. user=local:snippets/x.yaml)"
                        .into(),
                ));
            }
            _ => {}
        }
        Ok(form)
    }

    async fn create_qemu(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        let template_id = parse_vmid(&spec.template)?;
        let guests = self.guests().await?;
        // Same rule as reinstall: only guests flagged as templates may be
        // cloned. Any other guest is somebody's VM and a full clone would copy
        // its disks. Unknown ids and regular guests get the same answer (no
        // guest-id probing).
        let template = guests
            .iter()
            .find(|g| g.vmid == template_id && g.kind == GuestKind::Qemu && g.template)
            .ok_or_else(|| {
                IntegrationError::InvalidInput(format!(
                    "{template_id} is not an available QEMU template"
                ))
            })?;
        let config_form = self.qemu_config_form(spec)?;
        let node = spec
            .node
            .clone()
            .or_else(|| self.settings.default_node.clone())
            .unwrap_or_else(|| template.node.clone());
        let vmid = match extra_u64(&spec.extra, "vmid")? {
            Some(id) => id,
            None => self.next_vmid().await?,
        };
        let mut clone = vec![
            ("newid".to_owned(), vmid.to_string()),
            (
                "name".to_owned(),
                guest_name(if spec.hostname.trim().is_empty() {
                    &spec.name
                } else {
                    &spec.hostname
                })?,
            ),
            ("full".to_owned(), "1".to_owned()),
        ];
        if node != template.node {
            clone.push(("target".into(), node.clone()));
        }
        if let Some(storage) = spec
            .storage
            .as_ref()
            .or(self.settings.default_storage.as_ref())
        {
            clone.push(("storage".into(), storage.clone()));
        }
        if let Some(pool) = extra_str(&spec.extra, "pool").or(self.settings.pool.as_deref()) {
            clone.push(("pool".into(), pool.to_owned()));
        }
        let upid = upid_of(
            &self
                .post(
                    &[
                        "nodes",
                        &template.node,
                        "qemu",
                        &template_id.to_string(),
                        "clone",
                    ],
                    &clone,
                )
                .await?,
        )?;
        self.wait_task(&upid).await?;
        let guest = Guest {
            node: node.clone(),
            vmid,
            kind: GuestKind::Qemu,
            status: "stopped".into(),
            name: None,
            template: false,
            lock: None,
            raw: Value::Null,
        };
        let configured = async {
            let v = self
                .post(
                    &["nodes", &node, "qemu", &vmid.to_string(), "config"],
                    &config_form,
                )
                .await?;
            self.wait_if_task(&v).await?;
            self.grow_disk(&guest, spec.disk_gb).await?;
            if spec.start_after_create {
                Ok::<_, IntegrationError>(Some(
                    guest.task(self.status_action(&guest, "start").await?),
                ))
            } else {
                Ok(None)
            }
        }
        .await;
        let task = match configured {
            Ok(task) => task,
            Err(err) => {
                self.destroy_quietly(&node, GuestKind::Qemu, vmid).await;
                return Err(err);
            }
        };
        let (ipv4, ipv6) = static_ips(spec);
        Ok(VmProvisioned {
            vm: guest.vm_ref(),
            task,
            root_password: None,
            ipv4,
            ipv6,
            extra: json!({"guest_type": "qemu", "vmid": vmid, "node": node}),
        })
    }

    async fn create_lxc(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        if spec
            .cloud_init_user_data
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
        {
            return Err(IntegrationError::InvalidInput(
                "LXC containers do not run cloud-init; remove cloud_init_user_data".into(),
            ));
        }
        if parse_vztmpl_volid(&spec.template).is_none() {
            return Err(IntegrationError::InvalidInput(
                "LXC template must be an OS template volume such as local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst"
                    .into(),
            ));
        }
        let storage = spec
            .storage
            .clone()
            .or_else(|| self.settings.default_storage.clone())
            .ok_or_else(|| {
                IntegrationError::InvalidInput(
                    "a storage is required for the LXC root disk (spec.storage or default_storage)"
                        .into(),
                )
            })?;
        let node = match spec
            .node
            .clone()
            .or_else(|| self.settings.default_node.clone())
        {
            Some(n) => n,
            None => self.pick_node().await?,
        };
        // Same rule as reinstall: only an OS template volume listed on the
        // node (not a backup, ISO or any other volume) may seed the root disk.
        self.ensure_os_template(&node, &spec.template).await?;
        let vmid = match extra_u64(&spec.extra, "vmid")? {
            Some(id) => id,
            None => self.next_vmid().await?,
        };
        let (generated, password) = match &spec.root_password {
            Some(pw) => (None, pw.clone()),
            None if spec.ssh_keys.is_empty() => {
                let pw = generate_password(20);
                (Some(pw.clone()), pw)
            }
            None => (None, SecretString::from(String::new())),
        };
        let extra = &spec.extra;
        let hostname = dns_name(if spec.hostname.trim().is_empty() {
            &spec.name
        } else {
            &spec.hostname
        });
        let mut form: Vec<(String, String)> = vec![
            ("vmid".into(), vmid.to_string()),
            ("ostemplate".into(), spec.template.trim().to_owned()),
            ("hostname".into(), hostname),
            ("cores".into(), spec.cpu_cores.to_string()),
            ("memory".into(), spec.memory_mb.to_string()),
            (
                "swap".into(),
                extra_u64(extra, "swap_mb")?.unwrap_or(512).to_string(),
            ),
            (
                "rootfs".into(),
                format!("{storage}:{}", spec.disk_gb.max(1)),
            ),
            (
                "net0".into(),
                lxc_net(
                    &spec.network,
                    &self.settings.default_bridge,
                    extra_bool(extra, "firewall"),
                )?,
            ),
            (
                "unprivileged".into(),
                bool01(extra_bool(extra, "unprivileged").unwrap_or(true)),
            ),
            ("start".into(), bool01(spec.start_after_create)),
            (
                "onboot".into(),
                bool01(extra_bool(extra, "onboot").unwrap_or(true)),
            ),
            (
                "description".into(),
                format!("Managed by HostingOS ({})", spec.name),
            ),
        ];
        if !password.is_empty() {
            form.push(("password".into(), password.expose_str().to_owned()));
        }
        if !spec.ssh_keys.is_empty() {
            form.push(("ssh-public-keys".into(), spec.ssh_keys.join("\n")));
        }
        if !spec.network.dns_servers.is_empty() {
            form.push(("nameserver".into(), join_ips(&spec.network.dns_servers)));
        }
        if let Some(domain) = split_fqdn(spec.hostname.trim()).1 {
            form.push(("searchdomain".into(), domain.to_owned()));
        }
        if !spec.tags.is_empty() {
            form.push(("tags".into(), pve_tags(&spec.tags)));
        }
        if let Some(pool) = extra_str(extra, "pool").or(self.settings.pool.as_deref()) {
            form.push(("pool".into(), pool.to_owned()));
        }
        if let Some(features) = extra_str(extra, "features") {
            form.push(("features".into(), features.to_owned()));
        }
        let upid = upid_of(&self.post(&["nodes", &node, "lxc"], &form).await?)?;
        let (ipv4, ipv6) = static_ips(spec);
        Ok(VmProvisioned {
            vm: VmRef::on_node(node.clone(), vmid.to_string()),
            task: Some(TaskRef {
                id: upid,
                node: Some(node.clone()),
            }),
            root_password: generated,
            ipv4,
            ipv6,
            extra: json!({"guest_type": "lxc", "vmid": vmid, "node": node}),
        })
    }

    /// Grows the primary disk to `disk_gb` GiB. Returns without doing anything
    /// when the disk is already that size; shrinking is `InvalidInput`.
    async fn grow_disk(&self, g: &Guest, disk_gb: u64) -> Result<(), IntegrationError> {
        let config = self.guest_config(g).await?;
        let (key, current) = match g.kind {
            GuestKind::Qemu => {
                qemu_primary_disk(&config).ok_or_else(|| IntegrationError::Remote {
                    status: None,
                    code: None,
                    message: format!("cannot find the primary disk of VM {}", g.vmid),
                })?
            }
            GuestKind::Lxc => (
                "rootfs".to_owned(),
                config.str_at("rootfs").and_then(disk_size).unwrap_or(0),
            ),
        };
        let wanted = disk_gb.saturating_mul(GIB);
        if wanted < current {
            return Err(IntegrationError::InvalidInput(format!(
                "disks can only grow (current {} GiB, requested {disk_gb} GiB)",
                bytes_to_gib(current)
            )));
        }
        if wanted == current {
            return Ok(());
        }
        let v = self
            .put(
                &["nodes", &g.node, g.kind.path(), &g.vmid_s(), "resize"],
                &[("disk".into(), key), ("size".into(), format!("{disk_gb}G"))],
            )
            .await?;
        self.wait_if_task(&v).await
    }

    /// Checks that `volid` is an LXC OS template volume (`storage:vztmpl/…`)
    /// listed with `vztmpl` content on `node`'s storage.
    async fn ensure_os_template(&self, node: &str, volid: &str) -> Result<(), IntegrationError> {
        let volid = volid.trim();
        let refused = || {
            IntegrationError::InvalidInput(format!(
                "{volid:?} is not an available LXC OS template (storage:vztmpl/…)"
            ))
        };
        let (storage, _) = parse_vztmpl_volid(volid).ok_or_else(refused)?;
        let content = match self
            .get(
                &["nodes", node, "storage", storage, "content"],
                &[("content", "vztmpl".into())],
            )
            .await
        {
            Ok(v) => v,
            Err(IntegrationError::NotFound(_) | IntegrationError::InvalidInput(_)) => {
                return Err(refused());
            }
            Err(e) => return Err(e),
        };
        let listed = content
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| item.str_at("volid") == Some(volid) && is_os_template_item(item));
        if listed { Ok(()) } else { Err(refused()) }
    }

    // ------------------------------------------------------------------
    // Reinstall
    // ------------------------------------------------------------------

    async fn reinstall_qemu(
        &self,
        g: Guest,
        req: &VmReinstall,
    ) -> Result<VmProvisioned, IntegrationError> {
        let template_id = parse_vmid(&req.template)?;
        let guests = self.guests().await?;
        // Only guests flagged as templates may be cloned: any other guest is
        // somebody's VM, and a full clone would copy its disks. Unknown ids
        // and regular guests get the same answer (no guest-id probing).
        let template = guests
            .iter()
            .find(|t| t.vmid == template_id && t.kind == GuestKind::Qemu && t.template)
            .ok_or_else(|| {
                IntegrationError::InvalidInput(format!(
                    "{template_id} is not an available QEMU template"
                ))
            })?
            .clone();
        if template_id == g.vmid {
            return Err(IntegrationError::InvalidInput(
                "a VM cannot be reinstalled from itself".into(),
            ));
        }
        if req
            .cloud_init_user_data
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
            && extra_str(&req.extra, "cicustom").is_none()
        {
            return Err(IntegrationError::InvalidInput(
                "Proxmox cannot receive raw cloud-init user data; pass extra.cicustom".into(),
            ));
        }
        let old = self.guest_config(&g).await?;
        let old_disk = qemu_primary_disk(&old).map(|(_, size)| size).unwrap_or(0);
        let was_running = g.status == "running";
        self.stop_and_wait(&g).await?;
        let v = self
            .delete(
                &["nodes", &g.node, "qemu", &g.vmid_s()],
                &[("destroy-unreferenced-disks", "1".into())],
            )
            .await?;
        self.wait_if_task(&v).await?;

        let mut clone = vec![
            ("newid".to_owned(), g.vmid_s()),
            ("full".to_owned(), "1".to_owned()),
        ];
        if let Some(name) = old.str_at("name") {
            clone.push(("name".into(), name.to_owned()));
        }
        if g.node != template.node {
            clone.push(("target".into(), g.node.clone()));
        }
        if let Some(storage) =
            extra_str(&req.extra, "storage").or(self.settings.default_storage.as_deref())
        {
            clone.push(("storage".into(), storage.to_owned()));
        }
        if let Some(pool) = extra_str(&req.extra, "pool").or(self.settings.pool.as_deref()) {
            clone.push(("pool".into(), pool.to_owned()));
        }
        let upid = upid_of(
            &self
                .post(
                    &[
                        "nodes",
                        &template.node,
                        "qemu",
                        &template_id.to_string(),
                        "clone",
                    ],
                    &clone,
                )
                .await?,
        )?;
        self.wait_task(&upid).await?;

        let mut form = preserved_qemu_config(&old, req.ssh_keys.is_empty());
        let mut set = |k: &str, v: String| {
            form.retain(|(key, _)| key != k);
            form.push((k.to_owned(), v));
        };
        if let Some(host) = req.hostname.as_deref().filter(|h| !h.trim().is_empty()) {
            set("name", guest_name(host)?);
            if let Some(domain) = split_fqdn(host.trim()).1 {
                set("searchdomain", domain.to_owned());
            }
        }
        if let Some(user) = self.ciuser(&req.extra) {
            set("ciuser", user.to_owned());
        }
        if let Some(pw) = &req.root_password {
            set("cipassword", pw.expose_str().to_owned());
        }
        if !req.ssh_keys.is_empty() {
            set("sshkeys", encode_sshkeys(&req.ssh_keys));
        }
        if let Some(custom) = extra_str(&req.extra, "cicustom") {
            set("cicustom", custom.to_owned());
        }
        let fresh = Guest {
            status: "stopped".into(),
            ..g.clone()
        };
        let v = self
            .post(&["nodes", &g.node, "qemu", &g.vmid_s(), "config"], &form)
            .await?;
        self.wait_if_task(&v).await?;
        if old_disk > 0 {
            self.grow_disk(&fresh, old_disk.div_ceil(GIB)).await?;
        }
        let start = was_running || extra_bool(&req.extra, "start").unwrap_or(true);
        let task = if start {
            Some(fresh.task(self.status_action(&fresh, "start").await?))
        } else {
            None
        };
        Ok(VmProvisioned {
            vm: fresh.vm_ref(),
            task,
            root_password: None,
            ipv4: Vec::new(),
            ipv6: Vec::new(),
            extra: json!({"guest_type": "qemu", "vmid": g.vmid, "node": g.node}),
        })
    }

    async fn reinstall_lxc(
        &self,
        g: Guest,
        req: &VmReinstall,
    ) -> Result<VmProvisioned, IntegrationError> {
        // Before anything is destroyed: the volume must be an OS template
        // (`vztmpl` content) on the container's node — never a backup
        // (`storage:backup/vzdump-…`), another guest's volume or a path.
        self.ensure_os_template(&g.node, &req.template).await?;
        let old = self.guest_config(&g).await?;
        let rootfs = old.str_at("rootfs").unwrap_or_default();
        let storage = extra_str(&req.extra, "storage")
            .map(str::to_owned)
            .or_else(|| {
                rootfs
                    .split(':')
                    .next()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .or_else(|| self.settings.default_storage.clone())
            .ok_or_else(|| {
                IntegrationError::InvalidInput("cannot determine the root storage".into())
            })?;
        let size_gb = disk_size(rootfs).map_or(8, |b| b.div_ceil(GIB)).max(1);
        self.stop_and_wait(&g).await?;
        let v = self
            .delete(
                &["nodes", &g.node, "lxc", &g.vmid_s()],
                &[
                    ("force", "1".into()),
                    ("destroy-unreferenced-disks", "1".into()),
                ],
            )
            .await?;
        self.wait_if_task(&v).await?;

        let (generated, password) = match &req.root_password {
            Some(pw) => (None, pw.clone()),
            None if req.ssh_keys.is_empty() => {
                let pw = generate_password(20);
                (Some(pw.clone()), pw)
            }
            None => (None, SecretString::from(String::new())),
        };
        let hostname = req
            .hostname
            .as_deref()
            .filter(|h| !h.trim().is_empty())
            .map(dns_name)
            .or_else(|| old.str_at("hostname").map(str::to_owned))
            .unwrap_or_else(|| format!("ct{}", g.vmid));
        let mut form: Vec<(String, String)> = vec![
            ("vmid".into(), g.vmid_s()),
            ("ostemplate".into(), req.template.trim().to_owned()),
            ("hostname".into(), hostname),
            ("rootfs".into(), format!("{storage}:{size_gb}")),
            ("start".into(), "1".into()),
        ];
        for (key, value) in old.as_object().into_iter().flatten() {
            let keep = matches!(
                key.as_str(),
                "cores"
                    | "memory"
                    | "swap"
                    | "nameserver"
                    | "searchdomain"
                    | "tags"
                    | "onboot"
                    | "unprivileged"
                    | "features"
                    | "description"
            ) || is_indexed(key, "net");
            if keep {
                let text = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned);
                form.push((key.clone(), text));
            }
        }
        if !password.is_empty() {
            form.push(("password".into(), password.expose_str().to_owned()));
        }
        if !req.ssh_keys.is_empty() {
            form.push(("ssh-public-keys".into(), req.ssh_keys.join("\n")));
        }
        let upid = upid_of(&self.post(&["nodes", &g.node, "lxc"], &form).await?)?;
        Ok(VmProvisioned {
            vm: g.vm_ref(),
            task: Some(g.task(upid)),
            root_password: generated,
            ipv4: Vec::new(),
            ipv6: Vec::new(),
            extra: json!({"guest_type": "lxc", "vmid": g.vmid, "node": g.node}),
        })
    }

    // ------------------------------------------------------------------
    // Info helpers
    // ------------------------------------------------------------------

    fn info_from_guest(g: &Guest) -> VmInfo {
        let r = &g.raw;
        VmInfo {
            vm: g.vm_ref(),
            name: g.name.clone().unwrap_or_else(|| g.vmid_s()),
            status: map_status(&g.status, g.lock.as_deref(), None),
            hostname: g.name.clone(),
            cpu_cores: r.u64_at("maxcpu").and_then(|c| u32::try_from(c).ok()),
            memory_mb: r.u64_at("maxmem").map(bytes_to_mib),
            disk_gb: r.u64_at("maxdisk").map(bytes_to_gib),
            ipv4: Vec::new(),
            ipv6: Vec::new(),
            template: None,
            plan: None,
            tags: r.str_at("tags").map(split_tags).unwrap_or_default(),
            created_at: None,
            uptime_secs: r.u64_at("uptime").filter(|u| *u > 0),
            extra: json!({
                "guest_type": g.kind.path(),
                "vmid": g.vmid,
                "lock": g.lock,
                "hastate": r.str_at("hastate"),
                "pool": r.str_at("pool"),
            }),
        }
    }

    async fn guest_ips(
        &self,
        g: &Guest,
        config: &Value,
    ) -> (Vec<std::net::IpAddr>, Vec<std::net::IpAddr>) {
        let vmid = g.vmid_s();
        let mut addrs: Vec<String> = Vec::new();
        match g.kind {
            GuestKind::Qemu if g.status == "running" => {
                let agent_on = config
                    .str_at("agent")
                    .is_some_and(|a| a.starts_with('1') || a.contains("enabled=1"));
                if agent_on
                    && let Ok(v) = self
                        .get(
                            &[
                                "nodes",
                                &g.node,
                                "qemu",
                                &vmid,
                                "agent",
                                "network-get-interfaces",
                            ],
                            &[],
                        )
                        .await
                {
                    for iface in v
                        .get("result")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        for ip in iface
                            .get("ip-addresses")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            if let Some(a) = ip.str_at("ip-address") {
                                addrs.push(a.to_owned());
                            }
                        }
                    }
                }
            }
            GuestKind::Lxc if g.status == "running" => {
                if let Ok(v) = self
                    .get(&["nodes", &g.node, "lxc", &vmid, "interfaces"], &[])
                    .await
                {
                    for iface in v.as_array().into_iter().flatten() {
                        for key in ["inet", "inet6"] {
                            if let Some(a) = iface.str_at(key) {
                                addrs.push(a.to_owned());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        if addrs.is_empty() {
            // Fall back to statically configured addresses.
            for (key, value) in config.as_object().into_iter().flatten() {
                if !(is_indexed(key, "ipconfig") || is_indexed(key, "net")) {
                    continue;
                }
                for part in value.as_str().unwrap_or_default().split(',') {
                    if let Some(ip) = part
                        .strip_prefix("ip=")
                        .or_else(|| part.strip_prefix("ip6="))
                    {
                        addrs.push(ip.to_owned());
                    }
                }
            }
        }
        split_ips(addrs.iter().map(String::as_str))
    }
}

// ----------------------------------------------------------------------
// Pure helpers
// ----------------------------------------------------------------------

fn classify(raw: &RawResponse, message: String) -> IntegrationError {
    let status = raw.status.as_u16();
    let lower = message.to_ascii_lowercase();
    if !matches!(status, 401 | 403 | 429) {
        if lower.contains("does not exist")
            || lower.contains("no such")
            || lower.contains("not found")
            || lower.contains("unable to find")
        {
            return IntegrationError::NotFound(message);
        }
        if lower.contains("is locked")
            || lower.contains("can't lock file")
            || lower.contains("already running")
            || lower.contains("already exists")
            || lower.contains("not running")
        {
            return IntegrationError::Conflict(message);
        }
    }
    raw.status_error(message, None)
}

fn task_failure(upid: &str, message: &str) -> IntegrationError {
    let kind = upid.split(':').nth(5).unwrap_or("task");
    let lower = message.to_ascii_lowercase();
    if lower.contains("already exists") || lower.contains("locked") {
        IntegrationError::Conflict(format!("Proxmox {kind} failed: {message}"))
    } else {
        IntegrationError::remote_with_code(
            None,
            kind.to_owned(),
            format!("Proxmox {kind} task failed: {message}"),
        )
    }
}

/// Splits an LXC OS template volume id `storage:vztmpl/file` into
/// `(storage, file)`. Backups (`storage:backup/…`), guest disks
/// (`storage:vm-101-disk-0`), ISO images and absolute paths are refused.
fn parse_vztmpl_volid(volid: &str) -> Option<(&str, &str)> {
    let (storage, rest) = volid.trim().split_once(':')?;
    let file = rest.strip_prefix("vztmpl/")?;
    let storage_ok = storage
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && storage
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let file_ok = !file.is_empty()
        && !file.contains("..")
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '~'));
    (storage_ok && file_ok).then_some((storage, file))
}

/// A storage content item that is an OS template (`content = vztmpl`).
fn is_os_template_item(item: &Value) -> bool {
    item.str_at("volid").and_then(parse_vztmpl_volid).is_some()
        && item.str_at("content").is_none_or(|c| c == "vztmpl")
}

fn parse_vmid(id: &str) -> Result<u64, IntegrationError> {
    id.trim()
        .parse::<u64>()
        .ok()
        .filter(|v| *v >= 100)
        .ok_or_else(|| IntegrationError::InvalidInput(format!("invalid Proxmox VMID: {id:?}")))
}

/// Node name encoded in a UPID (`UPID:<node>:<pid>:...`).
fn upid_node(upid: &str) -> Result<String, IntegrationError> {
    let mut parts = upid.split(':');
    match (parts.next(), parts.next()) {
        (Some("UPID"), Some(node)) if !node.is_empty() => Ok(node.to_owned()),
        _ => Err(IntegrationError::InvalidInput(format!(
            "not a Proxmox UPID: {upid:?}"
        ))),
    }
}

fn upid_of(v: &Value) -> Result<String, IntegrationError> {
    v.as_str()
        .filter(|s| s.starts_with("UPID:"))
        .map(str::to_owned)
        .ok_or_else(|| {
            IntegrationError::Transport("Proxmox did not return a task id (UPID)".into())
        })
}

fn bool01(b: bool) -> String {
    if b { "1" } else { "0" }.to_owned()
}

fn guest_name(raw: &str) -> Result<String, IntegrationError> {
    let name = dns_name(raw);
    if name.is_empty() {
        Err(IntegrationError::InvalidInput(format!(
            "cannot derive a guest name from {raw:?}"
        )))
    } else {
        Ok(name)
    }
}

/// `sshkeys` must be URL-encoded **before** form encoding: PVE decodes it with
/// `uri_unescape` (where `+` is not a space) and validates
/// `^[-%a-zA-Z0-9_.!~*'()]*$`. Strict percent-encoding satisfies both.
fn encode_sshkeys(keys: &[String]) -> String {
    let joined = keys
        .iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    percent_encode_strict(&joined)
}

fn join_ips(ips: &[std::net::IpAddr]) -> String {
    ips.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Mbit/s → PVE `rate` (MB/s, float).
fn rate_mb_per_s(mbps: u32) -> String {
    let mb = f64::from(mbps) / 8.0;
    let s = format!("{mb:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn qemu_net(
    net: &NetworkSpec,
    default_bridge: &str,
    model: &str,
    firewall: Option<bool>,
) -> String {
    let mut parts = vec![match &net.mac_address {
        Some(mac) => format!("{model}={mac}"),
        None => model.to_owned(),
    }];
    parts.push(format!(
        "bridge={}",
        net.bridge.as_deref().unwrap_or(default_bridge)
    ));
    if let Some(tag) = net.vlan {
        parts.push(format!("tag={tag}"));
    }
    if let Some(rate) = net.rate_limit_mbps {
        parts.push(format!("rate={}", rate_mb_per_s(rate)));
    }
    if let Some(fw) = firewall {
        parts.push(format!("firewall={}", bool01(fw)));
    }
    parts.join(",")
}

fn check_family(cfg: &IpConfig, v6: bool) -> Result<(), IntegrationError> {
    if let IpConfig::Static(s) = cfg
        && (s.address.is_ipv6() != v6 || s.gateway.is_some_and(|g| g.is_ipv6() != v6))
    {
        return Err(IntegrationError::InvalidInput(format!(
            "static {} configuration has an address of the wrong family",
            if v6 { "IPv6" } else { "IPv4" }
        )));
    }
    Ok(())
}

/// QEMU `ipconfig0`. `Auto` (provider pool) has no PVE equivalent and falls
/// back to DHCP (IPv4) / SLAAC (IPv6).
fn ipconfig(net: &NetworkSpec) -> Result<Option<String>, IntegrationError> {
    let mut parts = Vec::new();
    if let Some(v4) = &net.ipv4 {
        check_family(v4, false)?;
        match v4 {
            IpConfig::Static(s) => {
                parts.push(format!("ip={}/{}", s.address, s.prefix_len));
                if let Some(gw) = s.gateway {
                    parts.push(format!("gw={gw}"));
                }
            }
            IpConfig::Dhcp | IpConfig::Auto => parts.push("ip=dhcp".to_owned()),
        }
    }
    if let Some(v6) = &net.ipv6 {
        check_family(v6, true)?;
        match v6 {
            IpConfig::Static(s) => {
                parts.push(format!("ip6={}/{}", s.address, s.prefix_len));
                if let Some(gw) = s.gateway {
                    parts.push(format!("gw6={gw}"));
                }
            }
            IpConfig::Dhcp => parts.push("ip6=dhcp".to_owned()),
            IpConfig::Auto => parts.push("ip6=auto".to_owned()),
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join(",")))
}

fn lxc_net(
    net: &NetworkSpec,
    default_bridge: &str,
    firewall: Option<bool>,
) -> Result<String, IntegrationError> {
    let mut parts = vec![
        "name=eth0".to_owned(),
        format!("bridge={}", net.bridge.as_deref().unwrap_or(default_bridge)),
    ];
    if let Some(mac) = &net.mac_address {
        parts.push(format!("hwaddr={mac}"));
    }
    if let Some(v4) = &net.ipv4 {
        check_family(v4, false)?;
        match v4 {
            IpConfig::Static(s) => {
                parts.push(format!("ip={}/{}", s.address, s.prefix_len));
                if let Some(gw) = s.gateway {
                    parts.push(format!("gw={gw}"));
                }
            }
            IpConfig::Dhcp | IpConfig::Auto => parts.push("ip=dhcp".to_owned()),
        }
    }
    if let Some(v6) = &net.ipv6 {
        check_family(v6, true)?;
        match v6 {
            IpConfig::Static(s) => {
                parts.push(format!("ip6={}/{}", s.address, s.prefix_len));
                if let Some(gw) = s.gateway {
                    parts.push(format!("gw6={gw}"));
                }
            }
            IpConfig::Dhcp => parts.push("ip6=dhcp".to_owned()),
            IpConfig::Auto => parts.push("ip6=auto".to_owned()),
        }
    }
    if let Some(tag) = net.vlan {
        parts.push(format!("tag={tag}"));
    }
    if let Some(rate) = net.rate_limit_mbps {
        parts.push(format!("rate={}", rate_mb_per_s(rate)));
    }
    if let Some(fw) = firewall {
        parts.push(format!("firewall={}", bool01(fw)));
    }
    Ok(parts.join(","))
}

/// PVE tags: `[a-z0-9_][a-z0-9_+.-]*`, `;`-separated.
fn pve_tags(tags: &[String]) -> String {
    tags.iter()
        .map(|t| {
            let mut s: String = t
                .trim()
                .to_ascii_lowercase()
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            if s.starts_with(['-', '+', '.']) {
                s.insert(0, '_');
            }
            s
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(";")
}

fn split_tags(s: &str) -> Vec<String> {
    s.split([';', ',', ' '])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

fn is_indexed(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

fn disk_size(value: &str) -> Option<u64> {
    value
        .split(',')
        .find_map(|p| p.trim().strip_prefix("size="))
        .and_then(parse_size)
}

/// Primary (boot) disk of a QEMU config: `(key, size in bytes)`.
fn qemu_primary_disk(config: &Value) -> Option<(String, u64)> {
    let is_bus = |k: &str| {
        ["scsi", "virtio", "sata", "ide"]
            .iter()
            .any(|p| is_indexed(k, p))
    };
    let disk = |k: &str| -> Option<u64> {
        if !is_bus(k) {
            return None;
        }
        let v = config.str_at(k)?;
        if v.contains("media=cdrom") || v.contains("cloudinit") || v == "none" {
            return None;
        }
        Some(disk_size(v).unwrap_or(0))
    };
    if let Some(boot) = config.str_at("boot") {
        let order = boot
            .split(',')
            .find_map(|p| p.trim().strip_prefix("order="));
        for dev in order.into_iter().flat_map(|o| o.split(';')) {
            if let Some(size) = disk(dev.trim()) {
                return Some((dev.trim().to_owned(), size));
            }
        }
    }
    if let Some(bootdisk) = config.str_at("bootdisk")
        && let Some(size) = disk(bootdisk)
    {
        return Some((bootdisk.to_owned(), size));
    }
    for prefix in ["scsi", "virtio", "sata", "ide"] {
        for i in 0..31 {
            let key = format!("{prefix}{i}");
            if let Some(size) = disk(&key) {
                return Some((key, size));
            }
        }
    }
    None
}

/// Config keys carried over when a QEMU VM is re-cloned on reinstall.
fn preserved_qemu_config(old: &Value, keep_sshkeys: bool) -> Vec<(String, String)> {
    const KEYS: &[&str] = &[
        "name",
        "description",
        "tags",
        "onboot",
        "cores",
        "sockets",
        "memory",
        "balloon",
        "cpu",
        "numa",
        "agent",
        "ciuser",
        "nameserver",
        "searchdomain",
        "cicustom",
        "ciupgrade",
    ];
    old.as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| {
            KEYS.contains(&k.as_str())
                || is_indexed(k, "net")
                || is_indexed(k, "ipconfig")
                || (keep_sshkeys && k.as_str() == "sshkeys")
        })
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str().map_or_else(|| v.to_string(), str::to_owned),
            )
        })
        .collect()
}

fn map_status(status: &str, lock: Option<&str>, qmpstatus: Option<&str>) -> VmStatus {
    match lock {
        Some("clone" | "create" | "restore" | "rollback") => return VmStatus::Provisioning,
        Some("migrate") => return VmStatus::Migrating,
        _ => {}
    }
    match qmpstatus.unwrap_or(status) {
        "running" => VmStatus::Running,
        "stopped" => VmStatus::Stopped,
        "paused" | "suspended" | "prelaunch" | "inmigrate" | "postmigrate" => VmStatus::Paused,
        "io-error" | "internal-error" | "guest-panicked" => VmStatus::Error,
        _ => VmStatus::Unknown,
    }
}

fn os_family_of(ostype: Option<&str>) -> Option<OsFamily> {
    let o = ostype?;
    Some(if o.starts_with('l') {
        OsFamily::Linux
    } else if o.starts_with('w') {
        OsFamily::Windows
    } else {
        OsFamily::Other
    })
}

fn creation_time(config: &Value) -> Option<DateTime<Utc>> {
    let meta = config.str_at("meta")?;
    let ctime = meta
        .split(',')
        .find_map(|p| p.trim().strip_prefix("ctime="))?
        .parse::<i64>()
        .ok()?;
    Utc.timestamp_opt(ctime, 0).single()
}

// ----------------------------------------------------------------------
// Trait
// ----------------------------------------------------------------------

#[async_trait]
impl VirtProvider for ProxmoxProvider {
    fn kind(&self) -> &'static str {
        kinds::PROXMOX
    }

    fn capabilities(&self) -> VirtCapabilities {
        VirtCapabilities {
            snapshots: true,
            memory_snapshots: true,
            live_resize: true,
            reinstall: true,
            console_kinds: vec![ConsoleKind::NoVnc],
            metrics: true,
            multi_node: true,
            plan_based: false,
            async_tasks: true,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let version = self.get(&["version"], &[]).await?;
        let nodes = self.get(&["nodes"], &[]).await?;
        let nodes = nodes.as_array().cloned().unwrap_or_default();
        let online = nodes
            .iter()
            .filter(|n| n.str_at("status") == Some("online"))
            .count();
        Ok(ConnectionInfo {
            server_version: version
                .str_at("version")
                .map(|v| format!("pve-manager/{v}")),
            details: Some(format!("{online}/{} node(s) online", nodes.len())),
        })
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, IntegrationError> {
        let nodes = self.get(&["nodes"], &[]).await?;
        let guests = self.guests().await?;
        Ok(nodes
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| {
                let name = n.str_at("node")?.to_owned();
                let vm_count = guests
                    .iter()
                    .filter(|g| g.node == name && !g.template)
                    .count();
                Some(NodeInfo {
                    id: name.clone(),
                    name,
                    online: n.str_at("status") == Some("online"),
                    location: None,
                    cpu_cores: n.u64_at("maxcpu").and_then(|c| u32::try_from(c).ok()),
                    cpu_usage_percent: n.f64_at("cpu").map(|c| c * 100.0),
                    memory_total_mb: n.u64_at("maxmem").map(bytes_to_mib),
                    memory_used_mb: n.u64_at("mem").map(bytes_to_mib),
                    disk_total_gb: n.u64_at("maxdisk").map(bytes_to_gib),
                    disk_used_gb: n.u64_at("disk").map(bytes_to_gib),
                    vm_count: u32::try_from(vm_count).ok(),
                    extra: json!({
                        "uptime": n.u64_at("uptime"),
                        "support_level": n.str_at("level"),
                        "ssl_fingerprint": n.str_at("ssl_fingerprint"),
                    }),
                })
            })
            .collect())
    }

    async fn list_templates(
        &self,
        node: Option<&str>,
    ) -> Result<Vec<TemplateInfo>, IntegrationError> {
        let guests = self.guests().await?;
        let qemu: Vec<&Guest> = guests
            .iter()
            .filter(|g| g.template && g.kind == GuestKind::Qemu)
            .filter(|g| node.is_none_or(|n| g.node == n))
            .collect();
        let configs = join_all(qemu.iter().map(|g| self.guest_config(g))).await;
        let mut out: Vec<TemplateInfo> = qemu
            .iter()
            .zip(configs)
            .map(|(g, config)| {
                let config = config.unwrap_or(Value::Null);
                let cloud_init = config.as_object().is_some_and(|o| {
                    o.values()
                        .any(|v| v.as_str().is_some_and(|s| s.contains("cloudinit")))
                });
                let name = g.name.clone().unwrap_or_else(|| g.vmid_s());
                TemplateInfo {
                    id: g.vmid_s(),
                    os_family: os_family_of(config.str_at("ostype"))
                        .or_else(|| crate::util::guess_os_family(&name)),
                    name,
                    node: Some(g.node.clone()),
                    min_disk_gb: qemu_primary_disk(&config).map(|(_, s)| bytes_to_gib(s)),
                    cloud_init,
                    extra: json!({"guest_type": "qemu", "vmid": g.vmid}),
                }
            })
            .collect();

        // LXC OS templates on every (or the given) online node.
        let node_names: Vec<String> = match node {
            Some(n) => vec![n.to_owned()],
            None => self
                .get(&["nodes"], &[])
                .await?
                .as_array()
                .into_iter()
                .flatten()
                .filter(|n| n.str_at("status") == Some("online"))
                .filter_map(|n| n.str_at("node").map(str::to_owned))
                .collect(),
        };
        let mut seen = std::collections::HashSet::new();
        for node_name in node_names {
            let storages = self
                .get(
                    &["nodes", &node_name, "storage"],
                    &[("content", "vztmpl".into()), ("enabled", "1".into())],
                )
                .await
                .unwrap_or(Value::Null);
            let storages: Vec<(String, bool)> = storages
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| {
                    Some((
                        s.str_at("storage")?.to_owned(),
                        s.bool_at("shared").unwrap_or(false),
                    ))
                })
                .collect();
            let contents = join_all(storages.iter().map(|(storage, _)| {
                let node_name = node_name.as_str();
                let storage = storage.as_str();
                async move {
                    let path = ["nodes", node_name, "storage", storage, "content"];
                    let query = [("content", "vztmpl".to_owned())];
                    self.get(&path, &query).await
                }
            }))
            .await;
            for ((_, shared), content) in storages.iter().zip(contents) {
                for item in content
                    .unwrap_or(Value::Null)
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let Some(volid) = item.str_at("volid") else {
                        continue;
                    };
                    if !is_os_template_item(item) || !seen.insert(volid.to_owned()) {
                        continue;
                    }
                    let file = volid.rsplit('/').next().unwrap_or(volid);
                    out.push(TemplateInfo {
                        id: volid.to_owned(),
                        name: file.to_owned(),
                        node: (!shared).then(|| node_name.clone()),
                        os_family: crate::util::guess_os_family(file).or(Some(OsFamily::Linux)),
                        min_disk_gb: None,
                        cloud_init: false,
                        extra: json!({"guest_type": "lxc", "size": item.u64_at("size")}),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn list_vms(&self, node: Option<&str>) -> Result<Vec<VmInfo>, IntegrationError> {
        Ok(self
            .guests()
            .await?
            .iter()
            .filter(|g| !g.template)
            .filter(|g| node.is_none_or(|n| g.node == n))
            .map(Self::info_from_guest)
            .collect())
    }

    async fn get_vm(&self, vm: &VmRef) -> Result<VmInfo, IntegrationError> {
        let g = self.locate(vm).await?;
        let vmid = g.vmid_s();
        let path = [
            "nodes",
            g.node.as_str(),
            g.kind.path(),
            vmid.as_str(),
            "status",
            "current",
        ];
        let (current, config) = futures::join!(self.get(&path, &[]), self.guest_config(&g));
        let current = current?;
        let config = config.unwrap_or(Value::Null);
        let mut info = Self::info_from_guest(&g);
        info.status = map_status(
            current.str_at("status").unwrap_or(&g.status),
            current.str_at("lock").or(g.lock.as_deref()),
            current.str_at("qmpstatus"),
        );
        info.cpu_cores = current
            .u64_at("cpus")
            .and_then(|c| u32::try_from(c).ok())
            .or(info.cpu_cores);
        info.memory_mb = current
            .u64_at("maxmem")
            .map(bytes_to_mib)
            .or(info.memory_mb);
        info.uptime_secs = current.u64_at("uptime").filter(|u| *u > 0);
        if let Some((_, size)) = qemu_primary_disk(&config).filter(|_| g.kind == GuestKind::Qemu) {
            info.disk_gb = Some(bytes_to_gib(size));
        }
        if g.kind == GuestKind::Lxc {
            info.hostname = config
                .str_at("hostname")
                .map(str::to_owned)
                .or(info.hostname);
        }
        info.created_at = creation_time(&config);
        let (ipv4, ipv6) = self.guest_ips(&g, &config).await;
        info.ipv4 = ipv4;
        info.ipv6 = ipv6;
        Ok(info)
    }

    async fn create_vm(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        validate_spec(spec)?;
        let secrets: Vec<&str> = spec
            .root_password
            .iter()
            .map(SecretString::expose_str)
            .collect();
        let result = match self.guest_kind_for(&spec.template, &spec.extra)? {
            GuestKind::Qemu => self.create_qemu(spec).await,
            GuestKind::Lxc => self.create_lxc(spec).await,
        };
        result.map_err(|e| redact_error(e, &secrets))
    }

    async fn power(
        &self,
        vm: &VmRef,
        action: PowerAction,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        let endpoint = match (action, g.kind) {
            (PowerAction::Start, _) => "start",
            (PowerAction::Stop, _) => "stop",
            (PowerAction::Shutdown, _) => "shutdown",
            (PowerAction::Reboot, _) => "reboot",
            (PowerAction::Reset, GuestKind::Qemu) => "reset",
            (PowerAction::Reset, GuestKind::Lxc) => {
                return Err(IntegrationError::Unsupported(
                    "power reset (LXC containers have no hardware reset; use reboot or stop+start)",
                ));
            }
        };
        let upid = self.status_action(&g, endpoint).await?;
        Ok(Some(g.task(upid)))
    }

    async fn resize(
        &self,
        vm: &VmRef,
        resize: &VmResize,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        if resize.cpu_cores.is_none() && resize.memory_mb.is_none() && resize.disk_gb.is_none() {
            return Err(IntegrationError::InvalidInput(
                "Proxmox has no plans; set cpu_cores, memory_mb and/or disk_gb".into(),
            ));
        }
        let g = self.locate(vm).await?;
        let vmid = g.vmid_s();
        let mut form = Vec::new();
        if let Some(c) = resize.cpu_cores {
            if c == 0 {
                return Err(IntegrationError::InvalidInput(
                    "cpu_cores must be at least 1".into(),
                ));
            }
            form.push(("cores".to_owned(), c.to_string()));
        }
        if let Some(m) = resize.memory_mb {
            form.push(("memory".to_owned(), m.to_string()));
        }
        if !form.is_empty() {
            match g.kind {
                GuestKind::Qemu => {
                    let v = self
                        .post(&["nodes", &g.node, "qemu", &vmid, "config"], &form)
                        .await?;
                    self.wait_if_task(&v).await?;
                }
                GuestKind::Lxc => {
                    self.put(&["nodes", &g.node, "lxc", &vmid, "config"], &form)
                        .await?;
                }
            }
        }
        if let Some(gb) = resize.disk_gb {
            self.grow_disk(&g, gb).await?;
        }
        if g.kind == GuestKind::Qemu && g.status == "running" && !form.is_empty() {
            let pending = self
                .get(&["nodes", &g.node, "qemu", &vmid, "pending"], &[])
                .await?;
            let needs_restart = pending.as_array().into_iter().flatten().any(|p| {
                p.get("pending").is_some()
                    && matches!(p.str_at("key"), Some("cores" | "memory" | "sockets"))
            });
            if needs_restart {
                if resize.allow_restart {
                    // `qm reboot` applies pending changes.
                    return Ok(Some(g.task(self.status_action(&g, "reboot").await?)));
                }
                tracing::info!(
                    provider = PROVIDER,
                    vmid = g.vmid,
                    "CPU/memory change pending until the next restart (hotplug not enabled)"
                );
            }
        }
        Ok(None)
    }

    async fn reinstall(
        &self,
        vm: &VmRef,
        request: &VmReinstall,
    ) -> Result<VmProvisioned, IntegrationError> {
        if request.template.trim().is_empty() {
            return Err(IntegrationError::InvalidInput(
                "template is required".into(),
            ));
        }
        for key in &request.ssh_keys {
            crate::util::validate_ssh_key(key)?;
        }
        let g = self.locate(vm).await?;
        let secrets: Vec<&str> = request
            .root_password
            .iter()
            .map(SecretString::expose_str)
            .collect();
        let result = match g.kind {
            GuestKind::Qemu => self.reinstall_qemu(g, request).await,
            GuestKind::Lxc => self.reinstall_lxc(g, request).await,
        };
        result.map_err(|e| redact_error(e, &secrets))
    }

    async fn delete_vm(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        let vmid = g.vmid_s();
        let v = match g.kind {
            GuestKind::Qemu => {
                self.stop_and_wait(&g).await?;
                self.delete(
                    &["nodes", &g.node, "qemu", &vmid],
                    &[
                        ("purge", "1".into()),
                        ("destroy-unreferenced-disks", "1".into()),
                    ],
                )
                .await?
            }
            GuestKind::Lxc => {
                self.delete(
                    &["nodes", &g.node, "lxc", &vmid],
                    &[
                        ("purge", "1".into()),
                        ("force", "1".into()),
                        ("destroy-unreferenced-disks", "1".into()),
                    ],
                )
                .await?
            }
        };
        Ok(upid_of(&v).ok().map(|upid| g.task(upid)))
    }

    async fn list_snapshots(&self, vm: &VmRef) -> Result<Vec<SnapshotInfo>, IntegrationError> {
        let g = self.locate(vm).await?;
        let data = self
            .get(
                &["nodes", &g.node, g.kind.path(), &g.vmid_s(), "snapshot"],
                &[],
            )
            .await?;
        Ok(data
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s.str_at("name") != Some("current"))
            .filter_map(|s| {
                let name = s.str_at("name")?.to_owned();
                Some(SnapshotInfo {
                    id: name.clone(),
                    name,
                    description: s.str_at("description").map(str::to_owned),
                    created_at: s
                        .u64_at("snaptime")
                        .and_then(|t| i64::try_from(t).ok())
                        .and_then(|t| Utc.timestamp_opt(t, 0).single()),
                    size_mb: None,
                    includes_memory: s.bool_at("vmstate").unwrap_or(false),
                    parent: s.str_at("parent").map(str::to_owned),
                })
            })
            .collect())
    }

    async fn create_snapshot(
        &self,
        vm: &VmRef,
        request: &SnapshotRequest,
    ) -> Result<SnapshotCreated, IntegrationError> {
        let g = self.locate(vm).await?;
        let name = snapshot_name(&request.name);
        let mut form = vec![("snapname".to_owned(), name.clone())];
        if let Some(desc) = request
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            form.push(("description".into(), desc.to_owned()));
        }
        if request.include_memory {
            if g.kind == GuestKind::Lxc {
                return Err(IntegrationError::Unsupported(
                    "memory snapshots of LXC containers",
                ));
            }
            form.push(("vmstate".into(), "1".into()));
        }
        let v = self
            .post(
                &["nodes", &g.node, g.kind.path(), &g.vmid_s(), "snapshot"],
                &form,
            )
            .await?;
        Ok(SnapshotCreated {
            snapshot_id: name,
            task: upid_of(&v).ok().map(|u| g.task(u)),
        })
    }

    async fn rollback_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        let v = self
            .post(
                &[
                    "nodes",
                    &g.node,
                    g.kind.path(),
                    &g.vmid_s(),
                    "snapshot",
                    snapshot_id,
                    "rollback",
                ],
                &[],
            )
            .await?;
        Ok(upid_of(&v).ok().map(|u| g.task(u)))
    }

    async fn delete_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        let v = self
            .delete(
                &[
                    "nodes",
                    &g.node,
                    g.kind.path(),
                    &g.vmid_s(),
                    "snapshot",
                    snapshot_id,
                ],
                &[],
            )
            .await?;
        Ok(upid_of(&v).ok().map(|u| g.task(u)))
    }

    async fn console(&self, vm: &VmRef) -> Result<ConsoleAccess, IntegrationError> {
        let g = self.locate(vm).await?;
        let vmid = g.vmid_s();
        let data = self
            .post(
                &["nodes", &g.node, g.kind.path(), &vmid, "vncproxy"],
                &[("websocket".into(), "1".into())],
            )
            .await?;
        let port = data
            .u64_at("port")
            .and_then(|p| u16::try_from(p).ok())
            .ok_or_else(|| IntegrationError::Transport("vncproxy returned no port".into()))?;
        let ticket = data
            .str_at("ticket")
            .ok_or_else(|| IntegrationError::Transport("vncproxy returned no ticket".into()))?
            .to_owned();
        let mut ws = join_path(
            &self.base,
            &["nodes", &g.node, g.kind.path(), &vmid, "vncwebsocket"],
        )?;
        let scheme = if self.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        ws.set_scheme(scheme)
            .map_err(|()| IntegrationError::Other("cannot build websocket URL".into()))?;
        ws.query_pairs_mut()
            .append_pair("port", &port.to_string())
            .append_pair("vncticket", &ticket);
        let password = data
            .str_at("password")
            .map_or_else(|| SecretString::from(ticket.clone()), SecretString::from);
        Ok(ConsoleAccess {
            kind: ConsoleKind::NoVnc,
            url: ws.to_string(),
            ticket: Some(SecretString::from(ticket)),
            password: Some(password),
            port: Some(port),
            expires_at: None,
            extra: json!({
                "node": g.node,
                "vmid": g.vmid,
                "guest_type": g.kind.path(),
                "upid": data.str_at("upid"),
                "requires_api_auth": true,
                "note": "The vncwebsocket upgrade must carry the API token (or a PVEAuthCookie); \
                         proxy it server-side and connect within ~10 s of this call.",
            }),
        })
    }

    async fn metrics(&self, vm: &VmRef) -> Result<VmMetrics, IntegrationError> {
        let g = self.locate(vm).await?;
        let vmid = g.vmid_s();
        let current_path = [
            "nodes",
            g.node.as_str(),
            g.kind.path(),
            vmid.as_str(),
            "status",
            "current",
        ];
        let rrd_path = [
            "nodes",
            g.node.as_str(),
            g.kind.path(),
            vmid.as_str(),
            "rrddata",
        ];
        let rrd_query = [
            ("timeframe", "hour".to_owned()),
            ("cf", "AVERAGE".to_owned()),
        ];
        let (current, rrd) = futures::join!(
            self.get(&current_path, &[]),
            self.get(&rrd_path, &rrd_query)
        );
        let current = current?;
        let rrd = rrd.unwrap_or(Value::Null);
        let last = rrd
            .as_array()
            .and_then(|a| {
                a.iter()
                    .rev()
                    .find(|e| e.get("netin").is_some_and(|v| !v.is_null()))
            })
            .cloned()
            .unwrap_or(Value::Null);
        let disk_used = (g.kind == GuestKind::Lxc)
            .then(|| current.u64_at("disk"))
            .flatten();
        Ok(VmMetrics {
            collected_at: Utc::now(),
            status: map_status(
                current.str_at("status").unwrap_or(&g.status),
                current.str_at("lock"),
                current.str_at("qmpstatus"),
            ),
            cpu_usage_percent: current.f64_at("cpu").map(|c| (c * 100.0).clamp(0.0, 100.0)),
            memory_used_mb: current.u64_at("mem").map(bytes_to_mib),
            memory_total_mb: current.u64_at("maxmem").map(bytes_to_mib),
            disk_used_gb: disk_used.map(|d| bytes_to_gib_f64(d as f64)),
            disk_total_gb: current
                .u64_at("maxdisk")
                .map(|d| bytes_to_gib_f64(d as f64)),
            disk_read_bps: last.f64_at("diskread"),
            disk_write_bps: last.f64_at("diskwrite"),
            net_in_bps: last.f64_at("netin"),
            net_out_bps: last.f64_at("netout"),
            bandwidth_used_gb: None,
            uptime_secs: current.u64_at("uptime"),
        })
    }

    async fn task_status(&self, task: &TaskRef) -> Result<TaskState, IntegrationError> {
        let node = match &task.node {
            Some(n) => n.clone(),
            None => upid_node(&task.id)?,
        };
        self.task_state_on(&node, &task.id).await
    }

    fn features(&self) -> VirtFeatures {
        VirtFeatures {
            console_relay: true,
            password_reset: true,
            backups: true,
            backup_restore: true,
            reverse_dns: false,
            iso: true,
            rescue: false,
            firewall: true,
            metrics_history: true,
            bandwidth: true,
        }
    }

    /// `POST …/vncproxy` (websocket=1) → `GET …/vncwebsocket?port&vncticket`
    /// upgraded with the API token; the VNC password is the returned
    /// `password` (PVE 8+) or the ticket.
    async fn console_upstream(&self, vm: &VmRef) -> Result<ConsoleUpstream, IntegrationError> {
        let g = self.locate(vm).await?;
        let vmid = g.vmid_s();
        let data = self
            .post(
                &["nodes", &g.node, g.kind.path(), &vmid, "vncproxy"],
                &[("websocket".into(), "1".into())],
            )
            .await?;
        let port = data
            .u64_at("port")
            .and_then(|p| u16::try_from(p).ok())
            .ok_or_else(|| IntegrationError::Transport("vncproxy returned no port".into()))?;
        let ticket = data
            .str_at("ticket")
            .ok_or_else(|| IntegrationError::Transport("vncproxy returned no ticket".into()))?
            .to_owned();
        let mut ws = join_path(
            &self.base,
            &["nodes", &g.node, g.kind.path(), &vmid, "vncwebsocket"],
        )?;
        let scheme = if self.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        ws.set_scheme(scheme)
            .map_err(|()| IntegrationError::Other("cannot build websocket URL".into()))?;
        ws.query_pairs_mut()
            .append_pair("port", &port.to_string())
            .append_pair("vncticket", &ticket);
        let password = data
            .str_at("password")
            .map_or_else(|| SecretString::from(ticket.clone()), SecretString::from);
        let auth = self
            .auth
            .to_str()
            .map_err(|_| IntegrationError::Config("API token is not printable".into()))?;
        Ok(ConsoleUpstream {
            transport: ConsoleTransport::WebSocket,
            url: ws.to_string(),
            headers: vec![("Authorization".into(), SecretString::from(auth))],
            subprotocols: vec!["binary".into()],
            accept_invalid_certs: self.settings.accept_invalid_certs,
            vnc_password: Some(password),
            // pveproxy waits ~10 s for the websocket after vncproxy.
            expires_at: Some(Utc::now() + chrono::Duration::seconds(10)),
        })
    }

    /// QEMU guest agent `set-user-password` (needs `agent: 1` and
    /// qemu-guest-agent running in the guest; PVE 9: `VM.GuestAgent.Unrestricted`).
    async fn reset_password(
        &self,
        vm: &VmRef,
        request: &PasswordResetRequest,
    ) -> Result<PasswordReset, IntegrationError> {
        let g = self.locate(vm).await?;
        if g.kind == GuestKind::Lxc {
            return Err(IntegrationError::Unsupported(
                "password reset of LXC containers (no guest agent API)",
            ));
        }
        if g.status != "running" {
            return Err(IntegrationError::Conflict(
                "the server must be running to change its password".into(),
            ));
        }
        let username = request
            .username
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| "root".into());
        let password = request
            .password
            .clone()
            .unwrap_or_else(|| generate_password(20));
        let form = vec![
            ("username".to_owned(), username.clone()),
            ("password".to_owned(), password.expose_str().to_owned()),
        ];
        let result = self
            .post(
                &[
                    "nodes",
                    &g.node,
                    "qemu",
                    &g.vmid_s(),
                    "agent",
                    "set-user-password",
                ],
                &form,
            )
            .await;
        match result {
            Ok(_) => Ok(PasswordReset {
                username,
                password,
                task: None,
            }),
            Err(e) => {
                let e = redact_error(e, &[password.expose_str()]);
                let text = e.to_string().to_ascii_lowercase();
                if text.contains("guest agent") || text.contains("qga") {
                    Err(IntegrationError::Unsupported(
                        "password reset (the QEMU guest agent is not running in this server)",
                    ))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// `GET …/rrddata?timeframe=hour|day|week|month&cf=AVERAGE`.
    async fn metrics_history(
        &self,
        vm: &VmRef,
        range: MetricsRange,
    ) -> Result<MetricsHistory, IntegrationError> {
        let g = self.locate(vm).await?;
        let rows = self.rrd(&g, range.as_str()).await?;
        let mut samples: Vec<MetricSample> = rows
            .iter()
            .filter_map(|r| {
                let t = r.u64_at("time").and_then(|t| i64::try_from(t).ok())?;
                let at = Utc.timestamp_opt(t, 0).single()?;
                Some(MetricSample {
                    at: Some(at),
                    cpu_percent: r.f64_at("cpu").map(|c| (c * 100.0).clamp(0.0, 100.0)),
                    memory_used_mb: r.f64_at("mem").map(|b| b / 1_048_576.0),
                    memory_total_mb: r.f64_at("maxmem").map(|b| b / 1_048_576.0),
                    disk_read_bps: r.f64_at("diskread"),
                    disk_write_bps: r.f64_at("diskwrite"),
                    net_in_bps: r.f64_at("netin"),
                    net_out_bps: r.f64_at("netout"),
                })
            })
            .filter(|s| s.at.is_some_and(|a| a >= Utc::now() - range.duration()))
            .collect();
        samples.sort_by_key(|s| s.at);
        let step = samples
            .windows(2)
            .filter_map(|w| Some((w[1].at? - w[0].at?).num_seconds()))
            .find(|s| *s > 0)
            .and_then(|s| u32::try_from(s).ok())
            .unwrap_or_else(|| range.step_secs());
        Ok(MetricsHistory {
            range,
            step_secs: step,
            samples,
        })
    }

    /// Estimated from the month RRD (average in + out rates × step) for the
    /// current calendar month (UTC): Proxmox keeps no traffic counter.
    async fn bandwidth_usage(&self, vm: &VmRef) -> Result<BandwidthUsage, IntegrationError> {
        let g = self.locate(vm).await?;
        let rows = self.rrd(&g, "month").await?;
        let now = Utc::now();
        let period_start = Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single();
        let mut points: Vec<(i64, f64, f64)> = rows
            .iter()
            .filter_map(|r| {
                let t = r.u64_at("time").and_then(|t| i64::try_from(t).ok())?;
                Some((
                    t,
                    r.f64_at("netin").unwrap_or(0.0),
                    r.f64_at("netout").unwrap_or(0.0),
                ))
            })
            .collect();
        points.sort_by_key(|p| p.0);
        let step = points
            .windows(2)
            .map(|w| w[1].0 - w[0].0)
            .find(|s| *s > 0)
            .unwrap_or(3600) as f64;
        let since = period_start.map_or(0, |p| p.timestamp());
        let (mut inb, mut outb) = (0.0f64, 0.0f64);
        for (t, i, o) in points.iter().filter(|p| p.0 >= since) {
            let _ = t;
            inb += i.max(0.0) * step;
            outb += o.max(0.0) * step;
        }
        let gib = |b: f64| b / 1_073_741_824.0;
        Ok(BandwidthUsage {
            outgoing_gb: Some(gib(outb)),
            incoming_gb: Some(gib(inb)),
            counted_gb: gib(inb + outb),
            provider_limit_gb: None,
            estimated: true,
            period_start,
        })
    }

    /// vzdump archives of this guest on the node's backup storages
    /// (`GET /nodes/{node}/storage/{storage}/content?content=backup&vmid=`).
    async fn list_backups(&self, vm: &VmRef) -> Result<Vec<BackupInfo>, IntegrationError> {
        let g = self.locate(vm).await?;
        self.backups_of(&g).await
    }

    /// Restores one of this guest's backups over it (`POST /nodes/{node}/qemu`
    /// with `archive` + `force=1`; LXC: `/lxc` with `restore=1`, `force=1`
    /// and the current root-disk storage). The guest is stopped first.
    async fn restore_backup(
        &self,
        vm: &VmRef,
        backup_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        let backups = self.backups_of(&g).await?;
        let Some(backup) = backups.iter().find(|b| b.id == backup_id) else {
            return Err(IntegrationError::NotFound(format!(
                "backup {backup_id:?} of guest {}",
                g.vmid
            )));
        };
        if g.lock.is_some() {
            return Err(IntegrationError::Conflict(format!(
                "guest {} is locked ({})",
                g.vmid,
                g.lock.as_deref().unwrap_or_default()
            )));
        }
        let config = self.guest_config(&g).await?;
        self.stop_and_wait(&g).await?;
        let vmid = g.vmid_s();
        let mut form = vec![
            ("vmid".to_owned(), vmid),
            ("force".to_owned(), "1".to_owned()),
        ];
        let v = match g.kind {
            GuestKind::Qemu => {
                form.push(("archive".into(), backup.id.clone()));
                self.post(&["nodes", &g.node, "qemu"], &form).await?
            }
            GuestKind::Lxc => {
                form.push(("ostemplate".into(), backup.id.clone()));
                form.push(("restore".into(), "1".into()));
                if let Some(storage) = config
                    .str_at("rootfs")
                    .and_then(|r| r.split(',').next())
                    .and_then(|v| v.split_once(':'))
                    .map(|(s, _)| s.to_owned())
                {
                    form.push(("storage".into(), storage));
                }
                self.post(&["nodes", &g.node, "lxc"], &form).await?
            }
        };
        Ok(upid_of(&v).ok().map(|u| g.task(u)))
    }

    async fn vm_extras(&self, vm: &VmRef) -> Result<VmExtras, IntegrationError> {
        let g = self.locate(vm).await?;
        if g.kind == GuestKind::Lxc {
            return Ok(VmExtras::default());
        }
        let config = self.guest_config(&g).await?;
        let (mounted, boot_first) = cdrom_state(&config);
        Ok(VmExtras {
            rescue_enabled: None,
            mounted_iso: mounted,
            boot_from_iso: Some(boot_first),
            reverse_dns: Vec::new(),
        })
    }

    /// ISO volumes on the guest's node (`content=iso` storages).
    async fn list_isos(&self, vm: &VmRef) -> Result<Vec<IsoImage>, IntegrationError> {
        let g = self.locate(vm).await?;
        if g.kind == GuestKind::Lxc {
            return Err(IntegrationError::Unsupported(
                "ISO images for LXC containers",
            ));
        }
        self.isos_on(&g.node).await
    }

    /// `ide2 = <volid>,media=cdrom`; `boot_first` → `boot = order=ide2;<disk>`.
    async fn mount_iso(
        &self,
        vm: &VmRef,
        iso_id: &str,
        boot_first: bool,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        if g.kind == GuestKind::Lxc {
            return Err(IntegrationError::Unsupported(
                "ISO images for LXC containers",
            ));
        }
        let isos = self.isos_on(&g.node).await?;
        let Some(iso) = isos.iter().find(|i| i.id == iso_id) else {
            return Err(IntegrationError::NotFound(format!("ISO {iso_id:?}")));
        };
        let config = self.guest_config(&g).await?;
        let disk = qemu_primary_disk(&config).map(|(k, _)| k);
        let mut form = vec![("ide2".to_owned(), format!("{},media=cdrom", iso.id))];
        form.push(("boot".to_owned(), boot_order(boot_first, disk.as_deref())));
        let v = self
            .post(&["nodes", &g.node, "qemu", &g.vmid_s(), "config"], &form)
            .await?;
        self.wait_if_task(&v).await?;
        Ok(None)
    }

    async fn unmount_iso(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        if g.kind == GuestKind::Lxc {
            return Err(IntegrationError::Unsupported(
                "ISO images for LXC containers",
            ));
        }
        let config = self.guest_config(&g).await?;
        let disk = qemu_primary_disk(&config).map(|(k, _)| k);
        let form = vec![
            ("ide2".to_owned(), "none,media=cdrom".to_owned()),
            ("boot".to_owned(), boot_order(false, disk.as_deref())),
        ];
        let v = self
            .post(&["nodes", &g.node, "qemu", &g.vmid_s(), "config"], &form)
            .await?;
        self.wait_if_task(&v).await?;
        Ok(None)
    }

    /// VM-level firewall: options `enable` / `policy_in` and the rules whose
    /// comment starts with `hostingos` (rules staff added by hand are left
    /// alone). Takes effect only when the datacenter firewall is enabled and
    /// the NIC has `firewall=1`.
    async fn firewall(&self, vm: &VmRef) -> Result<FirewallState, IntegrationError> {
        let g = self.locate(vm).await?;
        let base = ["nodes", g.node.as_str(), g.kind.path()];
        let vmid = g.vmid_s();
        let opts_path = [
            base[0],
            base[1],
            base[2],
            vmid.as_str(),
            "firewall",
            "options",
        ];
        let rules_path = [
            base[0],
            base[1],
            base[2],
            vmid.as_str(),
            "firewall",
            "rules",
        ];
        let (opts, rules) = futures::join!(self.get(&opts_path, &[]), self.get(&rules_path, &[]));
        let opts = opts?;
        let rules = rules?;
        Ok(FirewallState {
            enabled: opts.bool_at("enable").unwrap_or(false) || opts.u64_at("enable") == Some(1),
            rules: merge_pve_rules(rules.as_array().map(Vec::as_slice).unwrap_or_default()),
        })
    }

    async fn set_firewall(
        &self,
        vm: &VmRef,
        state: &FirewallState,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let g = self.locate(vm).await?;
        let vmid = g.vmid_s();
        let rules_path = [
            "nodes",
            g.node.as_str(),
            g.kind.path(),
            vmid.as_str(),
            "firewall",
            "rules",
        ];
        let existing = self.get(&rules_path, &[]).await?;
        let mut ours: Vec<u64> = existing
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| is_managed_rule(r))
            .filter_map(|r| r.u64_at("pos"))
            .collect();
        // Highest position first: deleting shifts the ones below.
        ours.sort_unstable_by(|a, b| b.cmp(a));
        for pos in ours {
            let p = pos.to_string();
            self.delete(
                &[
                    "nodes",
                    &g.node,
                    g.kind.path(),
                    &vmid,
                    "firewall",
                    "rules",
                    &p,
                ],
                &[],
            )
            .await?;
        }
        // New rules go to the top; create in reverse to keep the given order.
        for form in pve_rule_forms(&state.rules)?.into_iter().rev() {
            self.post(&rules_path, &form).await?;
        }
        let opts = vec![
            ("enable".to_owned(), bool01(state.enabled)),
            ("policy_in".to_owned(), "DROP".to_owned()),
            ("policy_out".to_owned(), "ACCEPT".to_owned()),
        ];
        self.put(
            &[
                "nodes",
                &g.node,
                g.kind.path(),
                &vmid,
                "firewall",
                "options",
            ],
            &opts,
        )
        .await?;
        Ok(None)
    }
}

impl ProxmoxProvider {
    async fn rrd(&self, g: &Guest, timeframe: &str) -> Result<Vec<Value>, IntegrationError> {
        let vmid = g.vmid_s();
        let data = self
            .get(
                &["nodes", &g.node, g.kind.path(), &vmid, "rrddata"],
                &[
                    ("timeframe", timeframe.to_owned()),
                    ("cf", "AVERAGE".to_owned()),
                ],
            )
            .await?;
        Ok(data.as_array().cloned().unwrap_or_default())
    }

    async fn storages_with(
        &self,
        node: &str,
        content: &str,
    ) -> Result<Vec<String>, IntegrationError> {
        let v = self
            .get(
                &["nodes", node, "storage"],
                &[("content", content.to_owned()), ("enabled", "1".into())],
            )
            .await?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.str_at("storage").map(str::to_owned))
            .collect())
    }

    async fn backups_of(&self, g: &Guest) -> Result<Vec<BackupInfo>, IntegrationError> {
        let storages = self.storages_with(&g.node, "backup").await?;
        let vmid = g.vmid_s();
        let lists = join_all(storages.iter().map(|storage| {
            let path = [
                "nodes",
                g.node.as_str(),
                "storage",
                storage.as_str(),
                "content",
            ];
            let query = [("content", "backup".to_owned()), ("vmid", vmid.clone())];
            async move { self.get(&path, &query).await }
        }))
        .await;
        let mut out = Vec::new();
        for list in lists {
            for item in list?.as_array().into_iter().flatten() {
                let Some(volid) = item.str_at("volid") else {
                    continue;
                };
                // Defence in depth: the storage filter must have honoured vmid.
                if item.u64_at("vmid") != Some(g.vmid) || !volid.contains(":backup/") {
                    continue;
                }
                out.push(BackupInfo {
                    id: volid.to_owned(),
                    name: volid.rsplit('/').next().unwrap_or(volid).to_owned(),
                    created_at: item
                        .u64_at("ctime")
                        .and_then(|t| i64::try_from(t).ok())
                        .and_then(|t| Utc.timestamp_opt(t, 0).single()),
                    size_mb: item.u64_at("size").map(bytes_to_mib),
                    notes: item.str_at("notes").map(str::to_owned),
                });
            }
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(out)
    }

    async fn isos_on(&self, node: &str) -> Result<Vec<IsoImage>, IntegrationError> {
        let storages = self.storages_with(node, "iso").await?;
        let lists = join_all(storages.iter().map(|storage| {
            let path = ["nodes", node, "storage", storage.as_str(), "content"];
            let query = [("content", "iso".to_owned())];
            async move { self.get(&path, &query).await }
        }))
        .await;
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for list in lists {
            for item in list?.as_array().into_iter().flatten() {
                let Some(volid) = item.str_at("volid") else {
                    continue;
                };
                if !volid.contains(":iso/") || !seen.insert(volid.to_owned()) {
                    continue;
                }
                out.push(IsoImage {
                    id: volid.to_owned(),
                    name: volid.rsplit('/').next().unwrap_or(volid).to_owned(),
                    description: None,
                    size_mb: item.u64_at("size").map(bytes_to_mib),
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// `(mounted ISO volid, CD-ROM first in the boot order)` of a QEMU config.
fn cdrom_state(config: &Value) -> (Option<String>, bool) {
    let mounted = config
        .str_at("ide2")
        .filter(|v| v.contains("media=cdrom"))
        .and_then(|v| v.split(',').next())
        .filter(|v| *v != "none" && *v != "cdrom")
        .map(str::to_owned);
    let first = config
        .str_at("boot")
        .and_then(|b| b.split(',').find_map(|p| p.trim().strip_prefix("order=")))
        .and_then(|o| o.split(';').next())
        .is_some_and(|d| d.trim() == "ide2");
    (mounted, first)
}

fn boot_order(cdrom_first: bool, disk: Option<&str>) -> String {
    match (cdrom_first, disk) {
        (true, Some(d)) => format!("order=ide2;{d}"),
        (true, None) => "order=ide2".into(),
        (false, Some(d)) => format!("order={d};ide2"),
        (false, None) => "order=ide2".into(),
    }
}

const MANAGED_COMMENT: &str = "hostingos";

fn is_managed_rule(r: &Value) -> bool {
    r.str_at("comment")
        .is_some_and(|c| c.starts_with(MANAGED_COMMENT))
}

/// Our PVE rules → [`FirewallRule`]s; the per-family twins created for one
/// rule (IPv4 / IPv6 sources, icmp / ipv6-icmp) are merged back by comment.
fn merge_pve_rules(rules: &[Value]) -> Vec<FirewallRule> {
    let mut out: Vec<FirewallRule> = Vec::new();
    let mut sorted: Vec<&Value> = rules.iter().filter(|r| is_managed_rule(r)).collect();
    sorted.sort_by_key(|r| r.u64_at("pos").unwrap_or(0));
    for r in sorted {
        let direction = match r.str_at("type") {
            Some("out") => FirewallDirection::Out,
            _ => FirewallDirection::In,
        };
        let protocol = match r.str_at("proto") {
            Some("udp") => FirewallProtocol::Udp,
            Some("icmp" | "ipv6-icmp" | "icmpv6") => FirewallProtocol::Icmp,
            _ => FirewallProtocol::Tcp,
        };
        let addr = match direction {
            FirewallDirection::In => r.str_at("source"),
            FirewallDirection::Out => r.str_at("dest"),
        };
        let cidrs: Vec<String> = addr
            .map(|a| {
                a.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let comment = r
            .str_at("comment")
            .and_then(|c| c.strip_prefix(MANAGED_COMMENT))
            .map(|c| c.trim_start_matches(':').trim().to_owned())
            .filter(|c| !c.is_empty());
        let port = r.str_at("dport").map(|p| p.replace(':', "-"));
        let rule = FirewallRule {
            direction,
            protocol,
            port,
            cidrs,
            comment,
        };
        if let Some(prev) = out.last_mut()
            && prev.direction == rule.direction
            && prev.protocol == rule.protocol
            && prev.port == rule.port
            && prev.comment == rule.comment
        {
            if prev.cidrs.is_empty() || rule.cidrs.is_empty() {
                prev.cidrs.clear();
            } else {
                prev.cidrs.extend(rule.cidrs);
            }
            continue;
        }
        out.push(rule);
    }
    out
}

/// One [`FirewallRule`] → PVE rule forms (one per address family when the
/// source list mixes families; ICMP needs `icmp` and `ipv6-icmp`).
fn pve_rule_forms(rules: &[FirewallRule]) -> Result<Vec<Vec<(String, String)>>, IntegrationError> {
    let mut out = Vec::new();
    for rule in rules {
        let any =
            rule.cidrs.is_empty() || rule.cidrs.iter().any(|c| c == "0.0.0.0/0" || c == "::/0");
        let (v4, v6): (Vec<&String>, Vec<&String>) =
            rule.cidrs.iter().partition(|c| !c.contains(':'));
        let families: Vec<(Option<String>, bool)> = if any {
            vec![(None, false), (None, true)]
        } else {
            let mut f = Vec::new();
            if !v4.is_empty() {
                f.push((
                    Some(v4.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",")),
                    false,
                ));
            }
            if !v6.is_empty() {
                f.push((
                    Some(v6.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",")),
                    true,
                ));
            }
            f
        };
        let comment = match &rule.comment {
            Some(c) => format!("{MANAGED_COMMENT}: {c}"),
            None => MANAGED_COMMENT.to_owned(),
        };
        for (addrs, v6) in families {
            let proto = match (rule.protocol, v6) {
                (FirewallProtocol::Tcp, _) => "tcp",
                (FirewallProtocol::Udp, _) => "udp",
                (FirewallProtocol::Icmp, false) => "icmp",
                (FirewallProtocol::Icmp, true) => "ipv6-icmp",
            };
            // TCP/UDP rules without addresses cover both families at once.
            if any && v6 && rule.protocol != FirewallProtocol::Icmp {
                continue;
            }
            let mut form = vec![
                ("type".to_owned(), rule.direction.as_str().to_owned()),
                ("action".to_owned(), "ACCEPT".to_owned()),
                ("proto".to_owned(), proto.to_owned()),
                ("enable".to_owned(), "1".to_owned()),
                ("comment".to_owned(), comment.clone()),
            ];
            if let Some(port) = rule
                .port
                .as_deref()
                .filter(|_| rule.protocol != FirewallProtocol::Icmp)
            {
                form.push(("dport".to_owned(), port.replace('-', ":")));
            }
            if let Some(a) = addrs {
                let key = match rule.direction {
                    FirewallDirection::In => "source",
                    FirewallDirection::Out => "dest",
                };
                form.push((key.to_owned(), a));
            }
            out.push(form);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::integrations::virt::StaticIp;

    #[test]
    fn sshkeys_are_double_encoded_correctly() {
        let keys = vec![
            "ssh-ed25519 AAAAC3Nza+/x= alice@host".to_owned(),
            "ssh-rsa AAAAB3 bob".to_owned(),
        ];
        let enc = encode_sshkeys(&keys);
        assert_eq!(
            enc,
            "ssh-ed25519%20AAAAC3Nza%2B%2Fx%3D%20alice%40host%0Assh-rsa%20AAAAB3%20bob"
        );
        // PVE's validation regex for `urlencoded`.
        assert!(
            enc.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-%_.!~*'()".contains(c))
        );
    }

    #[test]
    fn network_strings() {
        let net = NetworkSpec {
            bridge: Some("vmbr1".into()),
            vlan: Some(42),
            ipv4: Some(IpConfig::Static(StaticIp {
                address: "203.0.113.10".parse().unwrap(),
                prefix_len: 24,
                gateway: Some("203.0.113.1".parse().unwrap()),
            })),
            ipv6: Some(IpConfig::Auto),
            dns_servers: vec![],
            mac_address: Some("BC:24:11:00:00:01".into()),
            rate_limit_mbps: Some(100),
        };
        assert_eq!(
            qemu_net(&net, "vmbr0", "virtio", Some(true)),
            "virtio=BC:24:11:00:00:01,bridge=vmbr1,tag=42,rate=12.5,firewall=1"
        );
        assert_eq!(
            ipconfig(&net).unwrap().unwrap(),
            "ip=203.0.113.10/24,gw=203.0.113.1,ip6=auto"
        );
        assert_eq!(
            lxc_net(&net, "vmbr0", None).unwrap(),
            "name=eth0,bridge=vmbr1,hwaddr=BC:24:11:00:00:01,ip=203.0.113.10/24,gw=203.0.113.1,ip6=auto,tag=42,rate=12.5"
        );
        let bad = NetworkSpec {
            ipv4: Some(IpConfig::Static(StaticIp {
                address: "2001:db8::1".parse().unwrap(),
                prefix_len: 64,
                gateway: None,
            })),
            ..Default::default()
        };
        assert!(ipconfig(&bad).is_err());
        assert_eq!(ipconfig(&NetworkSpec::default()).unwrap(), None);
        assert_eq!(rate_mb_per_s(1000), "125");
        assert_eq!(rate_mb_per_s(10), "1.25");
    }

    #[test]
    fn primary_disk_detection() {
        let cfg = json!({
            "boot": "order=ide2;scsi0;net0",
            "ide2": "local:iso/x.iso,media=cdrom",
            "scsi0": "local-lvm:vm-100-disk-0,iothread=1,size=32G",
            "ide0": "local-lvm:vm-100-cloudinit,media=cdrom",
        });
        assert_eq!(qemu_primary_disk(&cfg), Some(("scsi0".into(), 32 * GIB)));
        let legacy = json!({"bootdisk": "virtio0", "virtio0": "local:100/vm.qcow2,size=10G"});
        assert_eq!(
            qemu_primary_disk(&legacy),
            Some(("virtio0".into(), 10 * GIB))
        );
        let fallback = json!({"sata0": "local:x,size=2048M"});
        assert_eq!(
            qemu_primary_disk(&fallback),
            Some(("sata0".into(), 2 * GIB))
        );
        assert_eq!(qemu_primary_disk(&json!({})), None);
    }

    #[test]
    fn upids_and_names() {
        let upid = "UPID:pve2:0000C350:01A2B3C4:6553F00D:qmclone:9000:hostingos@pve!prov:";
        assert_eq!(upid_node(upid).unwrap(), "pve2");
        assert!(upid_node("nope").is_err());
        assert_eq!(
            task_failure(upid, "clone failed: storage full").kind(),
            "remote"
        );
        assert!(parse_vmid("99").is_err());
        assert_eq!(parse_vmid(" 101 ").unwrap(), 101);
        assert_eq!(
            pve_tags(&["HostingOS".into(), "plan:VPS 1".into(), "-x".into()]),
            "hostingos;plan-vps-1;_-x"
        );
    }

    #[test]
    fn status_mapping() {
        assert_eq!(
            map_status("running", None, Some("paused")),
            VmStatus::Paused
        );
        assert_eq!(
            map_status("stopped", Some("clone"), None),
            VmStatus::Provisioning
        );
        assert_eq!(
            map_status("running", Some("migrate"), None),
            VmStatus::Migrating
        );
        assert_eq!(map_status("running", None, None), VmStatus::Running);
        assert_eq!(map_status("weird", None, None), VmStatus::Unknown);
    }

    #[test]
    fn reinstall_preserves_network_and_size_keys() {
        let old = json!({
            "name": "vps1", "cores": 2, "memory": "2048", "net0": "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr0",
            "ipconfig0": "ip=203.0.113.5/24,gw=203.0.113.1", "sshkeys": "ssh-rsa%20AAA",
            "scsi0": "local-lvm:vm-1-disk-0,size=20G", "digest": "abc", "smbios1": "uuid=x"
        });
        let kept = preserved_qemu_config(&old, false);
        let keys: Vec<&str> = kept.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"net0") && keys.contains(&"ipconfig0") && keys.contains(&"cores"));
        assert!(
            !keys.contains(&"sshkeys") && !keys.contains(&"scsi0") && !keys.contains(&"digest")
        );
        assert!(kept.contains(&("cores".to_owned(), "2".to_owned())));
        assert!(
            preserved_qemu_config(&old, true)
                .iter()
                .any(|(k, _)| k == "sshkeys")
        );
    }

    #[test]
    fn creation_time_from_meta() {
        let cfg = json!({"meta": "creation-qemu=9.0.2,ctime=1700000000"});
        assert_eq!(creation_time(&cfg).unwrap().timestamp(), 1_700_000_000);
    }
}
