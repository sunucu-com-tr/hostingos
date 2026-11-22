//! Hetzner Cloud (`kind = "hetzner"`) — `https://api.hetzner.cloud/v1`, for
//! reselling cloud servers.
//!
//! * Auth: `Authorization: Bearer <project API token>` (read & write).
//! * Nodes are **locations** (`fsn1`, `nbg1`, `hel1`, `ash`, ...), templates are
//!   system/app **images**, plans are **server types** (`cx22`, `cpx31`, ...).
//! * Every mutating call returns an **action** id ([`TaskRef`]); `task_status`
//!   polls `GET /actions/{id}`.
//! * SSH keys must exist as project resources: they are created (or found)
//!   under a deterministic name `hostingos-<sha256 prefix>` and referenced by id.
//!   The root password and caller user data go through cloud-init.
//! * Snapshots are images (`create_image` with `type=snapshot`); rollback is a
//!   rebuild from the snapshot image. Images are checked to belong to the
//!   server before rollback/delete.
//! * Create and reinstall (`rebuild`) only accept OS images (`system` /
//!   `app`, as listed by `list_templates`); snapshots and backups — possibly
//!   of another customer's server in the same project — are refused.
//! * Errors use `{"error": {"code", "message", "details"}}` and are mapped by
//!   `code`; `429` honours `RateLimit-Reset`.
//!
//!
//! Extended operations (checked against the OpenAPI document
//! <https://docs.hetzner.cloud/cloud.spec.json>, reference at
//! <https://docs.hetzner.cloud/reference/cloud>):
//!
//! * Console relay: `POST /servers/{id}/actions/request_console` → `wss_url`
//!   (token valid for one minute) and the VNC `password`; the relay connects
//!   server-side (host must be the API host or `*.hetzner.cloud`).
//! * Password reset: `POST /servers/{id}/actions/reset_password` → generated
//!   `root_password` (Linux with the QEMU guest agent, server running).
//! * Metrics history: `GET /servers/{id}/metrics?type=cpu,disk,network&start&end&step`
//!   (CPU is per vCPU and is divided by the core count).
//! * Bandwidth: server `outgoing_traffic`, `ingoing_traffic`,
//!   `included_traffic` (bytes, billing period; Hetzner bills outbound).
//! * Backups: `GET /images?type=backup&bound_to={id}`; restore = `rebuild`
//!   with a backup image whose `bound_to` is this server.
//! * Reverse DNS: `POST /servers/{id}/actions/change_dns_ptr` (`ip`,
//!   `dns_ptr`, `null` resets) for the primary IPv4 / IPv6 /64 only.
//! * ISO: `GET /isos`, `attach_iso` / `detach_iso` (an attached ISO boots first).
//! * Rescue: `enable_rescue` (`linux64`, returns the rescue root password) /
//!   `disable_rescue`; effective at the next reboot.
//! * Firewall: one project firewall per server labelled
//!   `managed-by=hostingos,hostingos-server=<id>`: `POST /firewalls`,
//!   `actions/set_rules`, `apply_to_resources`, `remove_from_resources`.
//!   Applied = inbound denied unless a rule allows it; any outbound rule turns
//!   outbound into an allow-list (Hetzner semantics).
//!
//! `VmSpec.extra` keys: `firewall_ids` (array of ids), `network_ids` (array),
//! `placement_group_id`, `volume_ids` (array), `automount` (bool),
//! `primary_ipv4_id` / `primary_ipv6_id` (existing Primary IPs, required for
//! `IpConfig::Static`).

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::virt::{
    BackupInfo, BandwidthUsage, ConsoleAccess, ConsoleKind, ConsoleTransport, ConsoleUpstream,
    FirewallDirection, FirewallProtocol, FirewallRule, FirewallState, IpConfig, IsoImage,
    MetricSample, MetricsHistory, MetricsRange, NodeInfo, OsFamily, PasswordReset,
    PasswordResetRequest, PowerAction, RescueResult, ReverseDnsEntry, SnapshotCreated,
    SnapshotInfo, SnapshotRequest, TaskRef, TaskState, TemplateInfo, VirtCapabilities,
    VirtFeatures, VmExtras, VmInfo, VmMetrics, VmProvisioned, VmRef, VmReinstall, VmResize, VmSpec,
    VmStatus,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, VirtProvider, kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::Method;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::cloudinit::CloudConfig;
use crate::http::{
    ClientOptions, HttpClient, RawResponse, RetryPolicy, build_client, execute, join_path,
    map_reqwest_error, normalize_base, redact, redact_error,
};
use crate::util::{
    JsonExt, bytes_to_gib_f64, dns_name, extra_bool, extra_u64, poll_until, split_list,
    ssh_key_identity, validate_spec, validate_ssh_key, value_u64,
};

const PROVIDER: &str = "hetzner";
const DEFAULT_URL: &str = "https://api.hetzner.cloud/v1";
const USER_DATA_LIMIT: usize = 32 * 1024;
const MAX_PAGES: u64 = 200;

/// Admin-form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::HETZNER, "Hetzner Cloud", IntegrationCategory::Virt)
        .description(
            "Resell Hetzner Cloud servers from one Hetzner project. Products use plan = server \
             type (cx22, cpx31, ...), template = image name (ubuntu-24.04) and location = \
             fsn1/nbg1/hel1/ash/hil/sin.",
        )
        .docs_url("https://docs.hetzner.cloud/")
        .config_field(
            FieldSpec::url("api_url", "API URL")
                .default_value(DEFAULT_URL)
                .help("Leave empty for the public endpoint."),
        )
        .config_field(FieldSpec::text("default_location", "Default location").placeholder("fsn1"))
        .config_field(
            FieldSpec::text("default_server_type", "Default server type").placeholder("cx22"),
        )
        .config_field(
            FieldSpec::text("firewall_ids", "Firewall IDs")
                .help("Comma-separated firewall ids applied to every new server."),
        )
        .config_field(FieldSpec::number("timeout_secs", "HTTP timeout (s)").default_value(30))
        .config_field(
            FieldSpec::number("task_timeout_secs", "Action wait limit (s)")
                .default_value(600)
                .help("Upper bound for actions awaited inside one operation (resize)."),
        )
        .config_field(
            FieldSpec::number("task_poll_ms", "Action poll interval (ms)").default_value(2000),
        )
        .secret_field(
            FieldSpec::password("api_token", "API token")
                .required()
                .help("Project API token with Read & Write permission."),
        )
}

#[derive(Debug, Clone)]
struct Settings {
    default_location: Option<String>,
    default_server_type: Option<String>,
    firewall_ids: Vec<u64>,
    task_timeout: Duration,
    task_poll: Duration,
}

/// Hetzner Cloud provider. Build with [`HetznerProvider::from_config`] or [`crate::build`].
pub struct HetznerProvider {
    http: HttpClient,
    base: url::Url,
    auth: HeaderValue,
    token: SecretString,
    retry: RetryPolicy,
    settings: Settings,
}

impl fmt::Debug for HetznerProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HetznerProvider")
            .field("base", &self.base.as_str())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl HetznerProvider {
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
        let url = match cfg.opt_url("api_url")? {
            Some(u) => u,
            None => url::Url::parse(DEFAULT_URL)
                .map_err(|e| IntegrationError::Config(format!("default API URL: {e}")))?,
        };
        let token = sec.secret("api_token")?;
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {}", token.expose_str())).map_err(|_| {
                IntegrationError::Config("API token contains invalid characters".into())
            })?;
        auth.set_sensitive(true);
        let firewall_ids = cfg
            .opt_str("firewall_ids")?
            .map(split_list)
            .unwrap_or_default()
            .iter()
            .map(|s| {
                s.parse::<u64>().map_err(|_| {
                    IntegrationError::Config("config.firewall_ids must be numeric ids".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let base = normalize_base(url, &[])?;
        Ok(Self {
            http: build_client(ClientOptions::from_config(&cfg, 30, policy)?, &base)?,
            base,
            auth,
            token,
            retry: RetryPolicy::standard(),
            settings: Settings {
                default_location: cfg.opt_str("default_location")?.map(str::to_owned),
                default_server_type: cfg.opt_str("default_server_type")?.map(str::to_owned),
                firewall_ids,
                task_timeout: Duration::from_secs(cfg.u64_or("task_timeout_secs", 600)?.max(5)),
                task_poll: Duration::from_millis(cfg.u64_or("task_poll_ms", 2000)?.max(5)),
            },
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
        body: Option<&Value>,
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
        if let Some(body) = body {
            builder = builder.json(body);
        }
        let request = builder.build().map_err(map_reqwest_error)?;
        let raw = execute(&self.http, request, idempotent, &self.retry, PROVIDER).await?;
        if !raw.is_success() {
            return Err(self.error_from(&raw));
        }
        if raw.body.is_empty() {
            return Ok(Value::Null);
        }
        raw.json()
    }

    async fn get(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<Value, IntegrationError> {
        self.call(Method::GET, segments, query, None).await
    }

    async fn post(&self, segments: &[&str], body: &Value) -> Result<Value, IntegrationError> {
        self.call(Method::POST, segments, &[], Some(body)).await
    }

    fn error_from(&self, raw: &RawResponse) -> IntegrationError {
        let body: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
        let err = body.get("error").cloned().unwrap_or(Value::Null);
        let code = err.str_at("code").map(str::to_owned);
        let mut message = err
            .str_at("message")
            .map(str::to_owned)
            .unwrap_or_else(|| raw.status_text());
        let fields: Vec<String> = err
            .get("details")
            .and_then(|d| d.get("fields"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| {
                let name = f.str_at("name")?;
                let msgs: Vec<&str> = f
                    .get("messages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                Some(format!("{name}: {}", msgs.join(", ")))
            })
            .collect();
        if !fields.is_empty() && !message.contains(&fields[0]) {
            message = format!("{message} ({})", fields.join("; "));
        }
        let message = redact(&message, &[self.token.expose_str()]);
        match code.as_deref() {
            Some("unauthorized" | "forbidden") => IntegrationError::Auth(message),
            Some("not_found") => IntegrationError::NotFound(message),
            Some("uniqueness_error" | "conflict" | "locked" | "protected") => {
                IntegrationError::Conflict(message)
            }
            Some("invalid_input" | "json_error") => IntegrationError::InvalidInput(message),
            Some("rate_limit_exceeded") => IntegrationError::RateLimited {
                retry_after: raw.retry_after(),
            },
            Some(
                c @ ("resource_limit_exceeded"
                | "maintenance"
                | "resource_unavailable"
                | "unsupported_error"
                | "service_error"
                | "placement_error"),
            ) => IntegrationError::Remote {
                status: Some(raw.status.as_u16()),
                code: Some(c.to_owned()),
                message,
            },
            _ => raw.status_error(message, code),
        }
    }

    /// Follows `meta.pagination.next_page` and concatenates `key` arrays.
    async fn list_all(
        &self,
        segments: &[&str],
        key: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<Value>, IntegrationError> {
        let mut out = Vec::new();
        let mut page = 1u64;
        loop {
            let mut q: Vec<(&str, String)> = query.to_vec();
            q.push(("page", page.to_string()));
            q.push(("per_page", "50".into()));
            let v = self.get(segments, &q).await?;
            out.extend(
                v.get(key)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            let next = v
                .get("meta")
                .and_then(|m| m.get("pagination"))
                .and_then(|p| p.get("next_page"))
                .and_then(value_u64);
            match next {
                Some(n) if n > page && n <= MAX_PAGES => page = n,
                _ => break,
            }
        }
        Ok(out)
    }

    async fn server(&self, vm: &VmRef) -> Result<Value, IntegrationError> {
        let id = server_id(vm)?;
        let v = self.get(&["servers", &id.to_string()], &[]).await?;
        v.get("server")
            .cloned()
            .ok_or_else(|| IntegrationError::Transport("response without server object".into()))
    }

    async fn action(
        &self,
        vm: &VmRef,
        action: &str,
        body: &Value,
    ) -> Result<Value, IntegrationError> {
        let id = server_id(vm)?;
        self.post(&["servers", &id.to_string(), "actions", action], body)
            .await
    }

    async fn action_state(&self, id: &str) -> Result<TaskState, IntegrationError> {
        let v = self.get(&["actions", id], &[]).await?;
        let a = v.get("action").cloned().unwrap_or(Value::Null);
        Ok(match a.str_at("status") {
            Some("success") => TaskState::Succeeded,
            Some("error") => TaskState::Failed {
                message: a
                    .get("error")
                    .and_then(|e| e.str_at("message"))
                    .unwrap_or("action failed")
                    .to_owned(),
            },
            _ => TaskState::Running {
                progress_percent: a
                    .u64_at("progress")
                    .and_then(|p| u8::try_from(p.min(100)).ok()),
            },
        })
    }

    async fn wait_action(&self, task: &TaskRef) -> Result<(), IntegrationError> {
        let id = task.id.as_str();
        let state = poll_until(
            self.settings.task_timeout,
            self.settings.task_poll,
            move || async move {
                Ok(match self.action_state(id).await? {
                    TaskState::Running { .. } => None,
                    done => Some(done),
                })
            },
        )
        .await?;
        match state {
            TaskState::Failed { message } => Err(IntegrationError::remote(
                None,
                format!("Hetzner action {id} failed: {message}"),
            )),
            _ => Ok(()),
        }
    }

    /// Finds or creates project SSH keys; returns their ids.
    async fn ensure_ssh_keys(&self, keys: &[String]) -> Result<Vec<u64>, IntegrationError> {
        let mut ids = Vec::new();
        let mut all: Option<Vec<Value>> = None;
        for key in keys {
            validate_ssh_key(key)?;
            let identity = ssh_key_identity(key);
            let digest = hex::encode(Sha256::digest(identity.as_bytes()));
            let name = format!("hostingos-{}", &digest[..16]);
            let found = self.get(&["ssh_keys"], &[("name", name.clone())]).await?;
            if let Some(id) = found
                .get("ssh_keys")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|k| k.u64_at("id"))
            {
                ids.push(id);
                continue;
            }
            let body = json!({
                "name": name,
                "public_key": key.trim(),
                "labels": {"managed-by": "hostingos"},
            });
            match self.post(&["ssh_keys"], &body).await {
                Ok(v) => ids.push(
                    v.get("ssh_key")
                        .and_then(|k| k.u64_at("id"))
                        .ok_or_else(|| IntegrationError::Transport("ssh_key without id".into()))?,
                ),
                Err(IntegrationError::Conflict(_)) => {
                    // Same key already uploaded under another name.
                    if all.is_none() {
                        all = Some(self.list_all(&["ssh_keys"], "ssh_keys", &[]).await?);
                    }
                    let id = all
                        .iter()
                        .flatten()
                        .find(|k| {
                            k.str_at("public_key").map(ssh_key_identity).as_deref()
                                == Some(identity.as_str())
                        })
                        .and_then(|k| k.u64_at("id"))
                        .ok_or_else(|| {
                            IntegrationError::Conflict(
                                "SSH key name/fingerprint clash in the Hetzner project".into(),
                            )
                        })?;
                    ids.push(id);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(ids)
    }

    /// A create / rebuild image must be an OS image (`system` or `app`) — the same
    /// rule as [`VirtProvider::list_templates`]. Snapshots and backups are
    /// disk images of servers (possibly another customer's) and are refused,
    /// as are unknown ids/names, with the same message.
    async fn verify_os_image(&self, template: &str) -> Result<(), IntegrationError> {
        let refused =
            || IntegrationError::InvalidInput(format!("{template:?} is not an available OS image"));
        if template.is_empty() {
            return Err(refused());
        }
        let images: Vec<Value> = if let Ok(id) = template.parse::<u64>() {
            match self.get(&["images", &id.to_string()], &[]).await {
                Ok(v) => v.get("image").cloned().into_iter().collect(),
                Err(IntegrationError::NotFound(_) | IntegrationError::InvalidInput(_)) => {
                    Vec::new()
                }
                Err(e) => return Err(e),
            }
        } else {
            self.get(&["images"], &[("name", template.to_owned())])
                .await?
                .get("images")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        if !images.is_empty() && images.iter().all(is_os_image) {
            Ok(())
        } else {
            Err(refused())
        }
    }

    /// Image `id` must be a snapshot created from server `vm`.
    async fn own_snapshot(&self, vm: &VmRef, snapshot_id: &str) -> Result<Value, IntegrationError> {
        let sid = server_id(vm)?;
        let image_id: u64 = snapshot_id.trim().parse().map_err(|_| {
            IntegrationError::InvalidInput(format!("invalid snapshot id {snapshot_id:?}"))
        })?;
        let v = self.get(&["images", &image_id.to_string()], &[]).await?;
        let image = v.get("image").cloned().unwrap_or(Value::Null);
        let from = image.get("created_from").and_then(|c| c.u64_at("id"));
        if image.str_at("type") != Some("snapshot") || from != Some(sid) {
            return Err(IntegrationError::NotFound(format!(
                "snapshot {image_id} of server {sid}"
            )));
        }
        Ok(image)
    }
}

// ----------------------------------------------------------------------
// Mapping helpers
// ----------------------------------------------------------------------

/// `system` / `app` images (Hetzner's OS images), not `snapshot` / `backup`.
fn is_os_image(image: &Value) -> bool {
    matches!(image.str_at("type"), Some("system" | "app"))
}

fn server_id(vm: &VmRef) -> Result<u64, IntegrationError> {
    vm.id.trim().parse().map_err(|_| {
        IntegrationError::InvalidInput(format!("invalid Hetzner server id {:?}", vm.id))
    })
}

fn action_task(v: &Value) -> Option<TaskRef> {
    v.get("action")
        .and_then(|a| a.u64_at("id"))
        .map(|id| TaskRef {
            id: id.to_string(),
            node: None,
        })
}

fn map_status(s: Option<&str>) -> VmStatus {
    match s {
        Some("running") => VmStatus::Running,
        Some("off") => VmStatus::Stopped,
        Some("initializing" | "rebuilding") => VmStatus::Provisioning,
        Some("starting") => VmStatus::Starting,
        Some("stopping" | "deleting") => VmStatus::Stopping,
        Some("migrating") => VmStatus::Migrating,
        _ => VmStatus::Unknown,
    }
}

fn location_of(server: &Value) -> Option<String> {
    server
        .get("location")
        .and_then(|l| l.str_at("name"))
        .or_else(|| {
            server
                .get("datacenter")
                .and_then(|d| d.get("location"))
                .and_then(|l| l.str_at("name"))
        })
        .map(str::to_owned)
}

/// The server's address inside its routed IPv6 network (`<prefix>::1`).
fn ipv6_host(network: &str) -> Option<IpAddr> {
    let (addr, _) = network.split_once('/').unwrap_or((network, ""));
    let base: Ipv6Addr = addr.parse().ok()?;
    if base.segments()[4..].iter().all(|s| *s == 0) {
        Some(IpAddr::V6(Ipv6Addr::from(u128::from(base) | 1)))
    } else {
        Some(IpAddr::V6(base))
    }
}

fn server_info(s: &Value) -> Option<VmInfo> {
    let id = s.u64_at("id")?.to_string();
    let location = location_of(s);
    let st = s.get("server_type").cloned().unwrap_or(Value::Null);
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    if let Some(ip) = s
        .get("public_net")
        .and_then(|p| p.get("ipv4"))
        .and_then(|v| v.str_at("ip"))
        .and_then(|ip| ip.parse().ok())
    {
        ipv4.push(ip);
    }
    if let Some(ip) = s
        .get("public_net")
        .and_then(|p| p.get("ipv6"))
        .and_then(|v| v.str_at("ip"))
        .and_then(ipv6_host)
    {
        ipv6.push(ip);
    }
    for private in s
        .get("private_net")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(ip) = private
            .str_at("ip")
            .and_then(|ip| ip.parse::<IpAddr>().ok())
        {
            if ip.is_ipv4() {
                ipv4.push(ip)
            } else {
                ipv6.push(ip)
            }
        }
    }
    let labels = s.get("labels").and_then(Value::as_object);
    let tags = labels
        .into_iter()
        .flatten()
        .map(|(k, v)| match v.as_str() {
            Some(val) if !val.is_empty() => format!("{k}={val}"),
            _ => k.clone(),
        })
        .collect();
    let image = s.get("image").cloned().unwrap_or(Value::Null);
    Some(VmInfo {
        vm: match &location {
            Some(loc) => VmRef::on_node(loc.clone(), id),
            None => VmRef::new(id),
        },
        name: s.str_at("name").unwrap_or_default().to_owned(),
        status: map_status(s.str_at("status")),
        hostname: s.str_at("name").map(str::to_owned),
        cpu_cores: st.u64_at("cores").and_then(|c| u32::try_from(c).ok()),
        memory_mb: st.f64_at("memory").map(|gb| (gb * 1024.0).round() as u64),
        disk_gb: s.u64_at("primary_disk_size").or_else(|| st.u64_at("disk")),
        ipv4,
        ipv6,
        template: image
            .str_at("name")
            .map(str::to_owned)
            .or_else(|| image.u64_at("id").map(|i| i.to_string())),
        plan: st.str_at("name").map(str::to_owned),
        tags,
        created_at: s
            .str_at("created")
            .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
            .map(|d| d.with_timezone(&Utc)),
        uptime_secs: None,
        extra: json!({
            "location": location,
            "locked": s.bool_at("locked"),
            "rescue_enabled": s.bool_at("rescue_enabled"),
            "outgoing_traffic": s.u64_at("outgoing_traffic"),
            "ingoing_traffic": s.u64_at("ingoing_traffic"),
            "included_traffic": s.u64_at("included_traffic"),
            "server_type_id": st.u64_at("id"),
        }),
    })
}

/// HostingOS tags → Hetzner labels (`key=value` or bare `key`).
fn labels(tags: &[String]) -> Map<String, Value> {
    let clean = |s: &str| -> String {
        let s: String = s
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .take(63)
            .collect();
        s.trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_owned()
    };
    let mut out = Map::new();
    out.insert("managed-by".into(), json!("hostingos"));
    for tag in tags {
        let (k, v) = tag.split_once('=').unwrap_or((tag.as_str(), ""));
        let key = clean(k);
        if !key.is_empty() {
            out.insert(key, json!(clean(v)));
        }
    }
    out
}

fn id_array(extra: &Value, key: &str) -> Result<Vec<u64>, IntegrationError> {
    match extra.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                value_u64(v).ok_or_else(|| {
                    IntegrationError::InvalidInput(format!("extra.{key} must contain numeric ids"))
                })
            })
            .collect(),
        Some(Value::String(s)) => split_list(s)
            .iter()
            .map(|s| {
                s.parse().map_err(|_| {
                    IntegrationError::InvalidInput(format!("extra.{key} must contain numeric ids"))
                })
            })
            .collect(),
        Some(_) => Err(IntegrationError::InvalidInput(format!(
            "extra.{key} must be an array"
        ))),
    }
}

fn os_family(flavor: Option<&str>) -> Option<OsFamily> {
    match flavor? {
        "unknown" => None,
        "windows" => Some(OsFamily::Windows),
        _ => Some(OsFamily::Linux),
    }
}

fn check_user_data(data: &str) -> Result<(), IntegrationError> {
    if data.len() > USER_DATA_LIMIT {
        return Err(IntegrationError::InvalidInput(format!(
            "user data is {} bytes; Hetzner accepts at most 32 KiB",
            data.len()
        )));
    }
    Ok(())
}

/// Minimal user data that replaces whatever the server was created with, so a
/// rebuild does not re-apply an old password.
const EMPTY_CLOUD_CONFIG: &str = "#cloud-config\n{}\n";

#[async_trait]
impl VirtProvider for HetznerProvider {
    fn kind(&self) -> &'static str {
        kinds::HETZNER
    }

    fn capabilities(&self) -> VirtCapabilities {
        VirtCapabilities {
            snapshots: true,
            memory_snapshots: false,
            live_resize: false,
            reinstall: true,
            console_kinds: vec![ConsoleKind::NoVnc],
            metrics: true,
            multi_node: true,
            plan_based: true,
            async_tasks: true,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let locations = self.get(&["locations"], &[]).await?;
        let servers = self.get(&["servers"], &[("per_page", "1".into())]).await?;
        let n_locations = locations
            .get("locations")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let n_servers = servers
            .get("meta")
            .and_then(|m| m.get("pagination"))
            .and_then(|p| p.u64_at("total_entries"));
        Ok(ConnectionInfo {
            server_version: Some("Hetzner Cloud API v1".into()),
            details: Some(match n_servers {
                Some(n) => format!("{n_locations} location(s), {n} server(s) in project"),
                None => format!("{n_locations} location(s)"),
            }),
        })
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, IntegrationError> {
        let v = self.get(&["locations"], &[]).await?;
        Ok(v.get("locations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let name = l.str_at("name")?.to_owned();
                Some(NodeInfo {
                    id: name.clone(),
                    name: l.str_at("description").unwrap_or(&name).to_owned(),
                    online: true,
                    location: Some(format!(
                        "{}, {}",
                        l.str_at("city").unwrap_or_default(),
                        l.str_at("country").unwrap_or_default()
                    )),
                    extra: json!({"id": l.u64_at("id"), "network_zone": l.str_at("network_zone")}),
                    ..NodeInfo::default()
                })
            })
            .collect())
    }

    async fn list_templates(
        &self,
        _node: Option<&str>,
    ) -> Result<Vec<TemplateInfo>, IntegrationError> {
        let images = self
            .list_all(
                &["images"],
                "images",
                &[
                    ("type", "system".into()),
                    ("type", "app".into()),
                    ("status", "available".into()),
                ],
            )
            .await?;
        Ok(images
            .iter()
            .filter(|i| is_os_image(i))
            .filter(|i| i.get("deprecated").is_none_or(Value::is_null))
            .filter_map(|i| {
                let id = i
                    .str_at("name")
                    .map(str::to_owned)
                    .or_else(|| i.u64_at("id").map(|n| n.to_string()))?;
                Some(TemplateInfo {
                    name: i.str_at("description").unwrap_or(&id).to_owned(),
                    id,
                    node: None,
                    os_family: os_family(i.str_at("os_flavor")),
                    min_disk_gb: i.u64_at("disk_size"),
                    cloud_init: true,
                    extra: json!({
                        "image_id": i.u64_at("id"),
                        "type": i.str_at("type"),
                        "architecture": i.str_at("architecture"),
                        "os_version": i.str_at("os_version"),
                        "rapid_deploy": i.bool_at("rapid_deploy"),
                    }),
                })
            })
            .collect())
    }

    async fn list_vms(&self, node: Option<&str>) -> Result<Vec<VmInfo>, IntegrationError> {
        let servers = self.list_all(&["servers"], "servers", &[]).await?;
        Ok(servers
            .iter()
            .filter_map(server_info)
            .filter(|i| node.is_none_or(|n| i.vm.node.as_deref() == Some(n)))
            .collect())
    }

    async fn get_vm(&self, vm: &VmRef) -> Result<VmInfo, IntegrationError> {
        let s = self.server(vm).await?;
        server_info(&s).ok_or_else(|| IntegrationError::Transport("malformed server object".into()))
    }

    async fn create_vm(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        validate_spec(spec)?;
        let server_type = spec
            .plan
            .clone()
            .or_else(|| self.settings.default_server_type.clone())
            .ok_or_else(|| {
                IntegrationError::InvalidInput(
                    "Hetzner Cloud is plan-based: set plan (server type) or default_server_type"
                        .into(),
                )
            })?;
        let name = dns_name(if spec.name.trim().is_empty() {
            &spec.hostname
        } else {
            &spec.name
        });
        if name.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "server name is empty after sanitising".into(),
            ));
        }
        let extra = &spec.extra;
        let mut public_net = Map::new();
        for (family, cfg, extra_key) in [
            ("ipv4", &spec.network.ipv4, "primary_ipv4_id"),
            ("ipv6", &spec.network.ipv6, "primary_ipv6_id"),
        ] {
            public_net.insert(format!("enable_{family}"), json!(cfg.is_some()));
            let primary = extra_u64(extra, extra_key)?;
            match (cfg, primary) {
                (Some(_), Some(id)) => {
                    public_net.insert(family.into(), json!(id));
                }
                (Some(IpConfig::Static(_)), None) => {
                    return Err(IntegrationError::InvalidInput(format!(
                        "Hetzner assigns public IPs itself; for a fixed {family} pass extra.{extra_key} \
                         (an existing Primary IP in the same location)"
                    )));
                }
                _ => {}
            }
        }
        let hostname = Some(spec.hostname.trim()).filter(|h| !h.is_empty());
        let user_data = CloudConfig {
            hostname,
            root_password: spec.root_password.as_ref(),
            ssh_keys: &[],
            user_data: spec.cloud_init_user_data.as_deref(),
        }
        .render();
        if let Some(data) = &user_data {
            check_user_data(data)?;
        }
        // Same rule as reinstall: only OS images, never a snapshot / backup
        // (a disk image of a server, possibly another customer's). Checked
        // before anything is created (SSH keys included).
        self.verify_os_image(spec.template.trim()).await?;
        let ssh_keys = self.ensure_ssh_keys(&spec.ssh_keys).await?;
        let mut firewalls = self.settings.firewall_ids.clone();
        firewalls.extend(id_array(extra, "firewall_ids")?);
        firewalls.sort_unstable();
        firewalls.dedup();

        let mut body = json!({
            "name": name,
            "server_type": server_type,
            "image": spec.template.trim(),
            "start_after_create": spec.start_after_create,
            "labels": labels(&spec.tags),
            "ssh_keys": ssh_keys,
            "public_net": public_net,
        });
        let location = spec
            .location
            .clone()
            .or_else(|| spec.node.clone())
            .or_else(|| self.settings.default_location.clone());
        if let Some(location) = location {
            body["location"] = json!(location);
        }
        if let Some(data) = user_data {
            body["user_data"] = json!(data);
        }
        if !firewalls.is_empty() {
            body["firewalls"] = json!(
                firewalls
                    .iter()
                    .map(|id| json!({"firewall": id}))
                    .collect::<Vec<_>>()
            );
        }
        let networks = id_array(extra, "network_ids")?;
        if !networks.is_empty() {
            body["networks"] = json!(networks);
        }
        let volumes = id_array(extra, "volume_ids")?;
        if !volumes.is_empty() {
            body["volumes"] = json!(volumes);
            body["automount"] = json!(extra_bool(extra, "automount").unwrap_or(false));
        }
        if let Some(pg) = extra_u64(extra, "placement_group_id")? {
            body["placement_group"] = json!(pg);
        }
        let secrets: Vec<&str> = spec
            .root_password
            .iter()
            .map(SecretString::expose_str)
            .collect();
        let v = self
            .post(&["servers"], &body)
            .await
            .map_err(|e| redact_error(e, &secrets))?;
        let server = v.get("server").cloned().unwrap_or(Value::Null);
        let info = server_info(&server)
            .ok_or_else(|| IntegrationError::Transport("create response without server".into()))?;
        let task = v
            .get("next_actions")
            .and_then(Value::as_array)
            .and_then(|a| a.last())
            .and_then(|a| a.u64_at("id"))
            .map(|id| TaskRef {
                id: id.to_string(),
                node: None,
            })
            .or_else(|| action_task(&v));
        let root_password = if spec.root_password.is_some() {
            None
        } else {
            v.str_at("root_password").map(SecretString::from)
        };
        Ok(VmProvisioned {
            vm: info.vm,
            task,
            root_password,
            ipv4: info.ipv4,
            ipv6: info.ipv6,
            extra: json!({"location": location_of(&server), "server_type": server_type}),
        })
    }

    async fn power(
        &self,
        vm: &VmRef,
        action: PowerAction,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let endpoint = match action {
            PowerAction::Start => "poweron",
            PowerAction::Stop => "poweroff",
            PowerAction::Shutdown => "shutdown",
            PowerAction::Reboot => "reboot",
            PowerAction::Reset => "reset",
        };
        let v = self.action(vm, endpoint, &json!({})).await?;
        Ok(action_task(&v))
    }

    async fn resize(
        &self,
        vm: &VmRef,
        resize: &VmResize,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let Some(plan) = resize.plan.clone() else {
            return Err(IntegrationError::InvalidInput(
                "Hetzner Cloud is plan-based: set plan (target server type)".into(),
            ));
        };
        let server = self.server(vm).await?;
        let was_running = server.str_at("status") != Some("off");
        if was_running {
            if !resize.allow_restart {
                return Err(IntegrationError::Conflict(
                    "the server must be powered off to change its type; set allow_restart".into(),
                ));
            }
            // Graceful shutdown first, hard power-off if the guest ignores ACPI.
            if let Some(task) = action_task(&self.action(vm, "shutdown", &json!({})).await?) {
                self.wait_action(&task).await?;
            }
            let half = self.settings.task_timeout / 2;
            let off = poll_until(half, self.settings.task_poll, move || async move {
                let s = self.server(vm).await?;
                Ok((s.str_at("status") == Some("off")).then_some(()))
            })
            .await;
            if off.is_err()
                && let Some(task) = action_task(&self.action(vm, "poweroff", &json!({})).await?)
            {
                self.wait_action(&task).await?;
            }
        }
        let body = json!({"server_type": plan, "upgrade_disk": resize.disk_gb.is_some()});
        let changed = self.action(vm, "change_type", &body).await?;
        let Some(task) = action_task(&changed) else {
            return Ok(None);
        };
        if !was_running {
            return Ok(Some(task));
        }
        self.wait_action(&task).await?;
        Ok(action_task(&self.action(vm, "poweron", &json!({})).await?))
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
            validate_ssh_key(key)?;
        }
        self.verify_os_image(request.template.trim()).await?;
        let user_data = CloudConfig {
            hostname: request.hostname.as_deref(),
            root_password: request.root_password.as_ref(),
            ssh_keys: &request.ssh_keys,
            user_data: request.cloud_init_user_data.as_deref(),
        }
        .render()
        .unwrap_or_else(|| EMPTY_CLOUD_CONFIG.to_owned());
        check_user_data(&user_data)?;
        let secrets: Vec<&str> = request
            .root_password
            .iter()
            .map(SecretString::expose_str)
            .collect();
        let v = self
            .action(
                vm,
                "rebuild",
                &json!({"image": request.template.trim(), "user_data": user_data}),
            )
            .await
            .map_err(|e| redact_error(e, &secrets))?;
        Ok(VmProvisioned {
            vm: vm.clone(),
            task: action_task(&v),
            root_password: if request.root_password.is_some() {
                None
            } else {
                v.str_at("root_password").map(SecretString::from)
            },
            ipv4: Vec::new(),
            ipv6: Vec::new(),
            extra: Value::Null,
        })
    }

    async fn delete_vm(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let id = server_id(vm)?;
        let v = self
            .call(Method::DELETE, &["servers", &id.to_string()], &[], None)
            .await?;
        Ok(action_task(&v))
    }

    async fn list_snapshots(&self, vm: &VmRef) -> Result<Vec<SnapshotInfo>, IntegrationError> {
        let sid = server_id(vm)?;
        let images = self
            .list_all(&["images"], "images", &[("type", "snapshot".into())])
            .await?;
        Ok(images
            .iter()
            .filter(|i| i.get("created_from").and_then(|c| c.u64_at("id")) == Some(sid))
            .filter_map(|i| {
                let id = i.u64_at("id")?.to_string();
                Some(SnapshotInfo {
                    name: i.str_at("description").unwrap_or(&id).to_owned(),
                    id,
                    description: None,
                    created_at: i
                        .str_at("created")
                        .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
                        .map(|d| d.with_timezone(&Utc)),
                    size_mb: i
                        .f64_at("image_size")
                        .map(|gb| (gb * 1024.0).round() as u64),
                    includes_memory: false,
                    parent: None,
                })
            })
            .collect())
    }

    async fn create_snapshot(
        &self,
        vm: &VmRef,
        request: &SnapshotRequest,
    ) -> Result<SnapshotCreated, IntegrationError> {
        if request.include_memory {
            return Err(IntegrationError::Unsupported(
                "memory snapshots (Hetzner snapshots are disk images)",
            ));
        }
        let description = match request
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            Some(d) => format!("{}: {}", request.name.trim(), d.trim()),
            None => request.name.trim().to_owned(),
        };
        let v = self
            .action(
                vm,
                "create_image",
                &json!({"type": "snapshot", "description": description, "labels": {"managed-by": "hostingos"}}),
            )
            .await?;
        let image_id = v.get("image").and_then(|i| i.u64_at("id")).ok_or_else(|| {
            IntegrationError::Transport("create_image response without image id".into())
        })?;
        Ok(SnapshotCreated {
            snapshot_id: image_id.to_string(),
            task: action_task(&v),
        })
    }

    async fn rollback_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let image = self.own_snapshot(vm, snapshot_id).await?;
        let image_id = image.u64_at("id").unwrap_or_default();
        let v = self
            .action(
                vm,
                "rebuild",
                &json!({"image": image_id.to_string(), "user_data": EMPTY_CLOUD_CONFIG}),
            )
            .await?;
        Ok(action_task(&v))
    }

    async fn delete_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let image = self.own_snapshot(vm, snapshot_id).await?;
        let image_id = image.u64_at("id").unwrap_or_default();
        self.call(
            Method::DELETE,
            &["images", &image_id.to_string()],
            &[],
            None,
        )
        .await?;
        Ok(None)
    }

    async fn console(&self, vm: &VmRef) -> Result<ConsoleAccess, IntegrationError> {
        let v = self.action(vm, "request_console", &json!({})).await?;
        let url = v.str_at("wss_url").ok_or_else(|| {
            IntegrationError::Transport("request_console returned no wss_url".into())
        })?;
        Ok(ConsoleAccess {
            kind: ConsoleKind::NoVnc,
            url: url.to_owned(),
            ticket: None,
            password: v.str_at("password").map(SecretString::from),
            port: None,
            expires_at: None,
            extra: json!({
                "action_id": v.get("action").and_then(|a| a.u64_at("id")),
                "note": "wss_url carries a short-lived token; use the password as the VNC password (noVNC).",
            }),
        })
    }

    async fn metrics(&self, vm: &VmRef) -> Result<VmMetrics, IntegrationError> {
        let server = self.server(vm).await?;
        let id = server_id(vm)?;
        let end = Utc::now();
        let start = end - chrono::Duration::minutes(10);
        let v = self
            .get(
                &["servers", &id.to_string(), "metrics"],
                &[
                    ("type", "cpu,disk,network".into()),
                    (
                        "start",
                        start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    ),
                    (
                        "end",
                        end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    ),
                    ("step", "60".into()),
                ],
            )
            .await?;
        let series = v
            .get("metrics")
            .and_then(|m| m.get("time_series"))
            .cloned()
            .unwrap_or(Value::Null);
        let last = |name: &str| -> Option<f64> {
            series
                .get(name)?
                .get("values")?
                .as_array()?
                .iter()
                .rev()
                .find_map(|pair| {
                    pair.get(1)?
                        .as_str()?
                        .parse::<f64>()
                        .ok()
                        .filter(|f| f.is_finite())
                })
        };
        let cores = server
            .get("server_type")
            .and_then(|t| t.f64_at("cores"))
            .filter(|c| *c > 0.0)
            .unwrap_or(1.0);
        let info = server_info(&server);
        Ok(VmMetrics {
            collected_at: end,
            status: map_status(server.str_at("status")),
            // Hetzner reports 100 per fully used vCPU.
            cpu_usage_percent: last("cpu").map(|c| (c / cores).clamp(0.0, 100.0)),
            memory_used_mb: None,
            memory_total_mb: info.as_ref().and_then(|i| i.memory_mb),
            disk_used_gb: None,
            disk_total_gb: info.as_ref().and_then(|i| i.disk_gb).map(|d| d as f64),
            disk_read_bps: last("disk.0.bandwidth.read"),
            disk_write_bps: last("disk.0.bandwidth.write"),
            net_in_bps: last("network.0.bandwidth.in"),
            net_out_bps: last("network.0.bandwidth.out"),
            bandwidth_used_gb: server
                .u64_at("outgoing_traffic")
                .map(|b| bytes_to_gib_f64(b as f64)),
            uptime_secs: None,
        })
    }

    async fn task_status(&self, task: &TaskRef) -> Result<TaskState, IntegrationError> {
        if task.id.trim().parse::<u64>().is_err() {
            return Err(IntegrationError::InvalidInput(format!(
                "invalid Hetzner action id {:?}",
                task.id
            )));
        }
        self.action_state(task.id.trim()).await
    }

    fn features(&self) -> VirtFeatures {
        VirtFeatures {
            console_relay: true,
            password_reset: true,
            backups: true,
            backup_restore: true,
            reverse_dns: true,
            iso: true,
            rescue: true,
            firewall: true,
            metrics_history: true,
            bandwidth: true,
        }
    }

    /// `POST /servers/{id}/actions/request_console` → `wss_url` (valid for
    /// one minute) + VNC `password`; the relay connects and authenticates.
    async fn console_upstream(&self, vm: &VmRef) -> Result<ConsoleUpstream, IntegrationError> {
        let v = self.action(vm, "request_console", &json!({})).await?;
        let raw = v.str_at("wss_url").ok_or_else(|| {
            IntegrationError::Transport("request_console returned no wss_url".into())
        })?;
        let url = url::Url::parse(raw)
            .map_err(|_| IntegrationError::Transport("request_console: invalid wss_url".into()))?;
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let same_host = self
            .base
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&host));
        let hetzner = host == "hetzner.cloud" || host.ends_with(".hetzner.cloud");
        let scheme_ok =
            url.scheme() == "wss" || (url.scheme() == "ws" && self.base.scheme() == "http");
        if !(scheme_ok && (same_host || hetzner)) {
            return Err(IntegrationError::Transport(
                "request_console returned an unexpected console host".into(),
            ));
        }
        let password = v
            .str_at("password")
            .map(SecretString::from)
            .ok_or_else(|| {
                IntegrationError::Transport("request_console returned no password".into())
            })?;
        Ok(ConsoleUpstream {
            transport: ConsoleTransport::WebSocket,
            url: url.to_string(),
            headers: Vec::new(),
            subprotocols: Vec::new(),
            accept_invalid_certs: false,
            vnc_password: Some(password),
            expires_at: Some(Utc::now() + chrono::Duration::seconds(60)),
        })
    }

    /// `POST /servers/{id}/actions/reset_password` (Linux with the QEMU guest
    /// agent, server running); Hetzner generates the password.
    async fn reset_password(
        &self,
        vm: &VmRef,
        _request: &PasswordResetRequest,
    ) -> Result<PasswordReset, IntegrationError> {
        let v = self.action(vm, "reset_password", &json!({})).await?;
        let password = v
            .str_at("root_password")
            .map(SecretString::from)
            .ok_or_else(|| {
                IntegrationError::Transport("reset_password returned no root_password".into())
            })?;
        Ok(PasswordReset {
            username: "root".into(),
            password,
            task: action_task(&v),
        })
    }

    /// `GET /servers/{id}/metrics?type=cpu,disk,network&start&end&step`.
    async fn metrics_history(
        &self,
        vm: &VmRef,
        range: MetricsRange,
    ) -> Result<MetricsHistory, IntegrationError> {
        let server = self.server(vm).await?;
        let id = server_id(vm)?;
        let end = Utc::now();
        let start = end - range.duration();
        let fmt = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let v = self
            .get(
                &["servers", &id.to_string(), "metrics"],
                &[
                    ("type", "cpu,disk,network".into()),
                    ("start", fmt(start)),
                    ("end", fmt(end)),
                    ("step", range.step_secs().to_string()),
                ],
            )
            .await?;
        let metrics = v.get("metrics").cloned().unwrap_or(Value::Null);
        let step = metrics
            .f64_at("step")
            .filter(|s| *s >= 1.0)
            .map_or(range.step_secs(), |s| s as u32);
        let cores = server
            .get("server_type")
            .and_then(|t| t.f64_at("cores"))
            .filter(|c| *c > 0.0)
            .unwrap_or(1.0);
        let series = metrics.get("time_series").cloned().unwrap_or(Value::Null);
        let mut by_time: std::collections::BTreeMap<i64, MetricSample> = Default::default();
        let mut put = |name: &str, set: &dyn Fn(&mut MetricSample, f64)| {
            for pair in series
                .get(name)
                .and_then(|s| s.get("values"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(ts) = pair.get(0).and_then(Value::as_f64) else {
                    continue;
                };
                let val = pair
                    .get(1)
                    .and_then(|v| {
                        v.as_str()
                            .and_then(|s| s.parse::<f64>().ok())
                            .or(v.as_f64())
                    })
                    .filter(|f| f.is_finite());
                let Some(val) = val else { continue };
                let key = ts.round() as i64;
                let entry = by_time.entry(key).or_insert_with(|| MetricSample {
                    at: DateTime::from_timestamp(key, 0),
                    ..MetricSample::default()
                });
                set(entry, val);
            }
        };
        put("cpu", &|s, v| {
            s.cpu_percent = Some((v / cores).clamp(0.0, 100.0))
        });
        put("disk.0.bandwidth.read", &|s, v| s.disk_read_bps = Some(v));
        put("disk.0.bandwidth.write", &|s, v| s.disk_write_bps = Some(v));
        put("network.0.bandwidth.in", &|s, v| s.net_in_bps = Some(v));
        put("network.0.bandwidth.out", &|s, v| s.net_out_bps = Some(v));
        Ok(MetricsHistory {
            range,
            step_secs: step,
            samples: by_time.into_values().collect(),
        })
    }

    /// Server `outgoing_traffic` / `ingoing_traffic` / `included_traffic`
    /// (bytes, current billing period); Hetzner counts outbound traffic.
    async fn bandwidth_usage(&self, vm: &VmRef) -> Result<BandwidthUsage, IntegrationError> {
        let server = self.server(vm).await?;
        let gib = |k: &str| server.u64_at(k).map(|b| bytes_to_gib_f64(b as f64));
        let out = gib("outgoing_traffic");
        Ok(BandwidthUsage {
            outgoing_gb: out,
            incoming_gb: gib("ingoing_traffic"),
            counted_gb: out.unwrap_or(0.0),
            provider_limit_gb: gib("included_traffic"),
            estimated: false,
            period_start: None,
        })
    }

    /// `GET /images?type=backup&bound_to={server}`.
    async fn list_backups(&self, vm: &VmRef) -> Result<Vec<BackupInfo>, IntegrationError> {
        let sid = server_id(vm)?;
        let images = self
            .list_all(
                &["images"],
                "images",
                &[("type", "backup".into()), ("bound_to", sid.to_string())],
            )
            .await?;
        let mut out: Vec<BackupInfo> = images
            .iter()
            .filter(|i| i.str_at("type") == Some("backup") && i.u64_at("bound_to") == Some(sid))
            .filter_map(|i| {
                let id = i.u64_at("id")?.to_string();
                Some(BackupInfo {
                    name: i.str_at("description").unwrap_or(&id).to_owned(),
                    id,
                    created_at: i
                        .str_at("created")
                        .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
                        .map(|d| d.with_timezone(&Utc)),
                    size_mb: i
                        .f64_at("image_size")
                        .map(|gb| (gb * 1024.0).round() as u64),
                    notes: None,
                })
            })
            .collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(out)
    }

    /// `rebuild` from a backup image bound to this server.
    async fn restore_backup(
        &self,
        vm: &VmRef,
        backup_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let sid = server_id(vm)?;
        let image_id: u64 = backup_id.trim().parse().map_err(|_| {
            IntegrationError::NotFound(format!("backup {backup_id:?} of server {sid}"))
        })?;
        let v = self.get(&["images", &image_id.to_string()], &[]).await?;
        let image = v.get("image").cloned().unwrap_or(Value::Null);
        if image.str_at("type") != Some("backup") || image.u64_at("bound_to") != Some(sid) {
            return Err(IntegrationError::NotFound(format!(
                "backup {image_id} of server {sid}"
            )));
        }
        let v = self
            .action(
                vm,
                "rebuild",
                &json!({"image": image_id.to_string(), "user_data": EMPTY_CLOUD_CONFIG}),
            )
            .await?;
        Ok(action_task(&v))
    }

    async fn vm_extras(&self, vm: &VmRef) -> Result<VmExtras, IntegrationError> {
        let server = self.server(vm).await?;
        let net = server.get("public_net").cloned().unwrap_or(Value::Null);
        let mut ptr = Vec::new();
        if let Some(v4) = net.get("ipv4").filter(|v| !v.is_null())
            && let Some(ip) = v4.str_at("ip").and_then(|s| s.parse::<IpAddr>().ok())
        {
            ptr.push(ReverseDnsEntry {
                ip,
                ptr: v4.str_at("dns_ptr").map(str::to_owned),
            });
        }
        for e in net
            .get("ipv6")
            .and_then(|v| v.get("dns_ptr"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(ip) = e.str_at("ip").and_then(|s| s.parse::<IpAddr>().ok()) {
                ptr.push(ReverseDnsEntry {
                    ip,
                    ptr: e.str_at("dns_ptr").map(str::to_owned),
                });
            }
        }
        let iso = server.get("iso").filter(|i| !i.is_null());
        Ok(VmExtras {
            rescue_enabled: server.bool_at("rescue_enabled"),
            mounted_iso: iso.and_then(|i| {
                i.str_at("name")
                    .map(str::to_owned)
                    .or_else(|| i.u64_at("id").map(|id| id.to_string()))
            }),
            boot_from_iso: Some(iso.is_some()),
            reverse_dns: ptr,
        })
    }

    /// `POST /servers/{id}/actions/change_dns_ptr` for the server's primary
    /// IPv4 or an address of its primary IPv6 /64.
    async fn set_reverse_dns(
        &self,
        vm: &VmRef,
        ip: IpAddr,
        ptr: Option<&str>,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let server = self.server(vm).await?;
        let net = server.get("public_net").cloned().unwrap_or(Value::Null);
        let owns = match ip {
            IpAddr::V4(_) => {
                net.get("ipv4")
                    .and_then(|v| v.str_at("ip"))
                    .and_then(|s| s.parse::<IpAddr>().ok())
                    == Some(ip)
            }
            IpAddr::V6(v6) => net
                .get("ipv6")
                .and_then(|v| v.str_at("ip"))
                .is_some_and(|n| in_v6_network(n, v6)),
        };
        if !owns {
            return Err(IntegrationError::InvalidInput(format!(
                "{ip} is not a primary address of this server"
            )));
        }
        let v = self
            .action(
                vm,
                "change_dns_ptr",
                &json!({"ip": ip.to_string(), "dns_ptr": ptr}),
            )
            .await?;
        Ok(action_task(&v))
    }

    /// `GET /isos` (deprecated ISOs left out).
    async fn list_isos(&self, _vm: &VmRef) -> Result<Vec<IsoImage>, IntegrationError> {
        let isos = self.list_all(&["isos"], "isos", &[]).await?;
        Ok(isos
            .iter()
            .filter(|i| i.get("deprecation").is_none_or(Value::is_null))
            .filter_map(|i| {
                let id = i
                    .str_at("name")
                    .map(str::to_owned)
                    .or_else(|| i.u64_at("id").map(|n| n.to_string()))?;
                Some(IsoImage {
                    name: i.str_at("description").unwrap_or(&id).to_owned(),
                    id,
                    description: i.str_at("type").map(str::to_owned),
                    size_mb: None,
                })
            })
            .collect())
    }

    /// `attach_iso` (Hetzner always boots an attached ISO first).
    async fn mount_iso(
        &self,
        vm: &VmRef,
        iso_id: &str,
        _boot_first: bool,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let v = self
            .action(vm, "attach_iso", &json!({"iso": iso_id}))
            .await?;
        Ok(action_task(&v))
    }

    async fn unmount_iso(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let v = self.action(vm, "detach_iso", &json!({})).await?;
        Ok(action_task(&v))
    }

    /// `enable_rescue` (`linux64`, returns the rescue root password) /
    /// `disable_rescue`. Takes effect at the next reboot.
    async fn set_rescue(
        &self,
        vm: &VmRef,
        enable: bool,
        ssh_keys: &[String],
    ) -> Result<RescueResult, IntegrationError> {
        if !enable {
            let v = self.action(vm, "disable_rescue", &json!({})).await?;
            return Ok(RescueResult {
                password: None,
                task: action_task(&v),
            });
        }
        let key_ids = self.ensure_ssh_keys(ssh_keys).await?;
        let v = self
            .action(
                vm,
                "enable_rescue",
                &json!({"type": "linux64", "ssh_keys": key_ids}),
            )
            .await?;
        Ok(RescueResult {
            password: v.str_at("root_password").map(SecretString::from),
            task: action_task(&v),
        })
    }

    /// The project firewall labelled `hostingos-server=<id>` (created on the
    /// first save); "enabled" = applied to the server.
    async fn firewall(&self, vm: &VmRef) -> Result<FirewallState, IntegrationError> {
        let sid = server_id(vm)?;
        Ok(match self.own_firewall(sid).await? {
            None => FirewallState::default(),
            Some(fw) => FirewallState {
                enabled: firewall_applied(&fw, sid),
                rules: fw
                    .get("rules")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(hetzner_rule)
                    .collect(),
            },
        })
    }

    /// `POST /firewalls` (first time) or `set_rules`, then
    /// `apply_to_resources` / `remove_from_resources`.
    async fn set_firewall(
        &self,
        vm: &VmRef,
        state: &FirewallState,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let sid = server_id(vm)?;
        let rules: Vec<Value> = state.rules.iter().map(rule_json).collect();
        let fw = match self.own_firewall(sid).await? {
            Some(fw) => {
                let id = fw.u64_at("id").unwrap_or_default().to_string();
                self.post(
                    &["firewalls", &id, "actions", "set_rules"],
                    &json!({"rules": rules}),
                )
                .await?;
                fw
            }
            None => {
                let v = self
                    .post(
                        &["firewalls"],
                        &json!({
                            "name": format!("hostingos-server-{sid}"),
                            "labels": {"managed-by": "hostingos", "hostingos-server": sid.to_string()},
                            "rules": rules,
                        }),
                    )
                    .await?;
                v.get("firewall").cloned().ok_or_else(|| {
                    IntegrationError::Transport("create firewall: no firewall object".into())
                })?
            }
        };
        let id = fw.u64_at("id").unwrap_or_default().to_string();
        let applied = firewall_applied(&fw, sid);
        let target = json!([{"type": "server", "server": {"id": sid}}]);
        if state.enabled && !applied {
            self.post(
                &["firewalls", &id, "actions", "apply_to_resources"],
                &json!({"apply_to": target}),
            )
            .await?;
        } else if !state.enabled && applied {
            self.post(
                &["firewalls", &id, "actions", "remove_from_resources"],
                &json!({"remove_from": target}),
            )
            .await?;
        }
        Ok(None)
    }
}

impl HetznerProvider {
    async fn own_firewall(&self, sid: u64) -> Result<Option<Value>, IntegrationError> {
        let v = self
            .get(
                &["firewalls"],
                &[(
                    "label_selector",
                    format!("managed-by=hostingos,hostingos-server={sid}"),
                )],
            )
            .await?;
        Ok(v.get("firewalls")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .cloned())
    }
}

fn firewall_applied(fw: &Value, sid: u64) -> bool {
    fw.get("applied_to")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|a| {
            a.str_at("type") == Some("server")
                && a.get("server").and_then(|s| s.u64_at("id")) == Some(sid)
        })
}

fn hetzner_rule(r: &Value) -> Option<FirewallRule> {
    let direction = match r.str_at("direction")? {
        "in" => FirewallDirection::In,
        "out" => FirewallDirection::Out,
        _ => return None,
    };
    let protocol = match r.str_at("protocol")? {
        "tcp" => FirewallProtocol::Tcp,
        "udp" => FirewallProtocol::Udp,
        "icmp" => FirewallProtocol::Icmp,
        _ => return None,
    };
    let key = match direction {
        FirewallDirection::In => "source_ips",
        FirewallDirection::Out => "destination_ips",
    };
    let mut cidrs: Vec<String> = r
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_owned))
        .collect();
    if cidrs.iter().any(|c| c == "0.0.0.0/0") && cidrs.iter().any(|c| c == "::/0") {
        cidrs.clear();
    }
    Some(FirewallRule {
        direction,
        protocol,
        port: r.str_at("port").map(str::to_owned),
        cidrs,
        comment: r.str_at("description").map(str::to_owned),
    })
}

fn rule_json(rule: &FirewallRule) -> Value {
    let cidrs: Vec<String> = if rule.cidrs.is_empty() {
        vec!["0.0.0.0/0".into(), "::/0".into()]
    } else {
        rule.cidrs.clone()
    };
    let mut v = json!({
        "direction": rule.direction.as_str(),
        "protocol": rule.protocol.as_str(),
        "description": rule.comment,
    });
    match rule.direction {
        FirewallDirection::In => v["source_ips"] = json!(cidrs),
        FirewallDirection::Out => v["destination_ips"] = json!(cidrs),
    }
    if rule.protocol != FirewallProtocol::Icmp
        && let Some(p) = &rule.port
    {
        v["port"] = json!(p);
    }
    v
}

/// Whether `ip` lies in the IPv6 network `net` (`2001:db8:1:2::/64`).
fn in_v6_network(net: &str, ip: Ipv6Addr) -> bool {
    let Some((addr, len)) = net.split_once('/') else {
        return addr_eq(net, ip);
    };
    let (Ok(base), Ok(len)) = (addr.parse::<Ipv6Addr>(), len.parse::<u32>()) else {
        return false;
    };
    if len > 128 {
        return false;
    }
    let mask = if len == 0 {
        0
    } else {
        u128::MAX << (128 - len)
    };
    (u128::from(base) & mask) == (u128::from(ip) & mask)
}

fn addr_eq(s: &str, ip: Ipv6Addr) -> bool {
    s.parse::<Ipv6Addr>().is_ok_and(|a| a == ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_host_address() {
        assert_eq!(
            ipv6_host("2001:db8:1:2::/64"),
            Some("2001:db8:1:2::1".parse().unwrap())
        );
        assert_eq!(
            ipv6_host("2001:db8::5"),
            Some("2001:db8::5".parse().unwrap())
        );
        assert_eq!(ipv6_host("nope"), None);
    }

    #[test]
    fn tags_become_labels() {
        let l = labels(&["env=prod".into(), "customer 42".into(), "=x".into()]);
        assert_eq!(l["managed-by"], "hostingos");
        assert_eq!(l["env"], "prod");
        assert_eq!(l["customer-42"], "");
        assert_eq!(l.len(), 3);
    }

    #[test]
    fn status_mapping() {
        assert_eq!(map_status(Some("off")), VmStatus::Stopped);
        assert_eq!(map_status(Some("initializing")), VmStatus::Provisioning);
        assert_eq!(map_status(None), VmStatus::Unknown);
    }

    #[test]
    fn id_arrays() {
        assert_eq!(id_array(&json!({"a": [1, "2"]}), "a").unwrap(), vec![1, 2]);
        assert_eq!(id_array(&json!({"a": "3, 4"}), "a").unwrap(), vec![3, 4]);
        assert!(id_array(&json!({"a": [true]}), "a").is_err());
        assert!(id_array(&json!({}), "a").unwrap().is_empty());
    }
}
