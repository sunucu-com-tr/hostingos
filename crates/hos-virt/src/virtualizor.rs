//! Virtualizor (`kind = "virtualizor"`) — admin API on the master panel,
//! `https://<master>:4085/index.php?act=<act>&api=json&adminapikey=..&adminapipass=..`.
//!
//! * GET carries every parameter in the query string; POST sends the action
//!   data as a form body (auth stays in the URL). The credentials therefore
//!   live in URLs: this module never logs URLs/queries and strips them from
//!   transport errors.
//! * Virtualizor answers **HTTP 200 even on failure**; the `error` key
//!   (object/array of messages) is checked on every response.
//! * Nodes are hypervisor servers (`serid`, master = 0); templates are OS
//!   templates (`osid`); plans are `plid`. [`VmRef::node`] = `serid`,
//!   [`VmRef::id`] = `vpsid`.
//! * `addvs` and `rebuild` queue a background task and return its `actid`,
//!   exposed as a [`TaskRef`] that `task_status` polls through `act=tasks`.
//! * No snapshot API (Virtualizor only has backups) → `Unsupported`.
//!
//! Extended operations (admin API reference
//! <https://www.virtualizor.com/docs/admin-api/>, pages `vnc`, `vps_stats`,
//! `vsbandwidth`, `managevps`):
//!
//! * Console relay: `act=vnc&novnc={vpsid}` → `info.vnc_hostname`, `port`,
//!   `password`; the relay opens the raw VNC (RFB over TCP) port itself after
//!   the outbound-policy check and authenticates with the password.
//! * Metrics history: `act=vps_stats&svs={vpsid}&show=YYYYMM` (`time`, `cpu`
//!   %, `ram` MB; network columns have no documented unit and are not used).
//! * Bandwidth: `act=vsbandwidth&vm_ids={vpsid}` (`used` / `limit`, MB).
//! * Not implemented: root password reset and ISO mount — the only admin API
//!   path is `managevps` POST (`rootpass`, `iso`), which per its own
//!   documentation rebuilds several unrelated feature-dependent settings
//!   from form state on a partial update; backups (`vpsrestore`), rescue
//!   and rDNS records have no safe per-VPS admin API we could verify.
//!
//! `VmSpec.extra` keys: `uid` (owner user id), `user_email` / `user_pass` /
//! `fname` / `lname` (inline user creation when no uid), `virt`, `swap_mb`,
//! `cpu_units` (`cpu` weight), `ip_pool_id`, `recipe` (post-install recipe id),
//! `send_email` (bool, default false → `noemail=1`).

use std::fmt;

use async_trait::async_trait;
use chrono::Utc;
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::virt::{
    BandwidthUsage, ConsoleAccess, ConsoleKind, ConsoleTransport, ConsoleUpstream, IpConfig,
    MetricSample, MetricsHistory, MetricsRange, NodeInfo, OsFamily, PowerAction, SnapshotCreated,
    SnapshotInfo, SnapshotRequest, TaskRef, TaskState, TemplateInfo, VirtCapabilities,
    VirtFeatures, VmInfo, VmMetrics, VmProvisioned, VmRef, VmReinstall, VmResize, VmSpec, VmStatus,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, VirtProvider, kinds,
};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use serde_json::{Value, json};

use crate::http::{
    ClientOptions, HttpClient, RetryPolicy, build_client, execute, map_reqwest_error,
    normalize_base, redact, redact_error,
};
use crate::util::{
    JsonExt, extra_bool, extra_str, extra_u64, generate_password, guess_os_family, split_ips,
    validate_spec, validate_ssh_key, value_u64,
};

const PROVIDER: &str = "virtualizor";
const PAGE_SIZE: u64 = 50;
const MAX_PAGES: u64 = 200;

/// Admin-form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::VIRTUALIZOR, "Virtualizor", IntegrationCategory::Virt)
        .description(
            "Virtualizor master panel (admin API). Template = OS template id (osid), node = \
             server id (serid, master = 0), plan = plan id (plid). Create an API key pair under \
             Configuration → API Credentials and whitelist HostingOS's IP.",
        )
        .docs_url("https://www.virtualizor.com/docs/admin/api/authentication")
        .config_field(
            FieldSpec::url("api_url", "Master panel URL")
                .required()
                .placeholder("https://virt.example.com:4085")
                .help("Admin panel base URL (HTTPS port 4085); /index.php is appended."),
        )
        .config_field(
            FieldSpec::boolean("skip_tls_verify", "Skip TLS certificate verification")
                .default_value(false)
                .help("Virtualizor ships a self-signed certificate; prefer installing a real one."),
        )
        .config_field(
            FieldSpec::text("virt", "Virtualization type")
                .default_value("kvm")
                .help("Value passed as `virt` to addvs (e.g. kvm, lxc, proxk, proxl, vzk)."),
        )
        .config_field(FieldSpec::number(
            "default_server_id",
            "Default server id (serid)",
        ))
        .config_field(
            FieldSpec::number("default_user_id", "Default owner user id (uid)")
                .help("Owner of new VPSs unless the order supplies extra.uid or extra.user_email."),
        )
        .config_field(FieldSpec::number(
            "default_plan_id",
            "Default plan id (plid)",
        ))
        .config_field(FieldSpec::number("ip_pool_id", "IP pool id (ippoolid)"))
        .config_field(FieldSpec::number("timeout_secs", "HTTP timeout (s)").default_value(60))
        .secret_field(FieldSpec::password("api_key", "API key").required())
        .secret_field(FieldSpec::password("api_pass", "API password").required())
}

#[derive(Debug, Clone)]
struct Settings {
    virt: String,
    default_server_id: Option<u64>,
    default_user_id: Option<u64>,
    default_plan_id: Option<u64>,
    ip_pool_id: Option<u64>,
}

/// Virtualizor provider. Build with [`VirtualizorProvider::from_config`] or [`crate::build`].
pub struct VirtualizorProvider {
    http: HttpClient,
    endpoint: url::Url,
    api_key: SecretString,
    api_pass: SecretString,
    retry: RetryPolicy,
    settings: Settings,
}

impl fmt::Debug for VirtualizorProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualizorProvider")
            .field("endpoint", &self.endpoint.as_str())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Request body: `None` → GET, `Some` → POST form.
type Form = Option<Vec<(String, String)>>;

impl VirtualizorProvider {
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
        let base = cfg.url("api_url")?;
        let endpoint = if base.path().ends_with("index.php") {
            normalize_base(base, &[])?
        } else {
            normalize_base(base, &["index.php"])?
        };
        Ok(Self {
            http: build_client(ClientOptions::from_config(&cfg, 60, policy)?, &endpoint)?,
            endpoint,
            api_key: sec.secret("api_key")?,
            api_pass: sec.secret("api_pass")?,
            retry: RetryPolicy::standard(),
            settings: Settings {
                virt: cfg.string_or("virt", "kvm")?,
                default_server_id: cfg.opt_u64("default_server_id")?,
                default_user_id: cfg.opt_u64("default_user_id")?,
                default_plan_id: cfg.opt_u64("default_plan_id")?,
                ip_pool_id: cfg.opt_u64("ip_pool_id")?,
            },
        })
    }

    fn secrets(&self) -> [&str; 2] {
        [self.api_key.expose_str(), self.api_pass.expose_str()]
    }

    /// Calls `act`. `idempotent` must be false for state-changing GETs
    /// (start/stop/delete are GETs in this API).
    async fn call(
        &self,
        act: &str,
        query: &[(&str, String)],
        form: Form,
        idempotent: bool,
    ) -> Result<Value, IntegrationError> {
        let mut url = self.endpoint.clone();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("act", act);
            for (k, v) in query {
                q.append_pair(k, v);
            }
            q.append_pair("api", "json");
            q.append_pair("adminapikey", self.api_key.expose_str());
            q.append_pair("adminapipass", self.api_pass.expose_str());
        }
        let builder = match &form {
            Some(form) => self.http.post(url).form(form),
            None => self.http.get(url),
        };
        let request = builder.build().map_err(map_reqwest_error)?;
        let raw = execute(
            &self.http,
            request,
            idempotent && form.is_none(),
            &self.retry,
            PROVIDER,
        )
        .await
        .map_err(|e| redact_error(e, &self.secrets()))?;
        if !raw.is_success() {
            let message = redact(&raw.status_text(), &self.secrets());
            return Err(raw.status_error(message, None));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|_| {
            IntegrationError::Transport(
                "Virtualizor returned a non-JSON response; check the panel URL (admin port 4085), \
                 the API key pair and that HostingOS's IP is allowed"
                    .into(),
            )
        })?;
        if let Some(message) = error_text(&value) {
            return Err(classify(redact(&message, &self.secrets())));
        }
        Ok(value)
    }

    async fn read(&self, act: &str, query: &[(&str, String)]) -> Result<Value, IntegrationError> {
        self.call(act, query, None, true).await
    }

    /// State-changing GET (never retried).
    async fn trigger(
        &self,
        act: &str,
        query: &[(&str, String)],
    ) -> Result<Value, IntegrationError> {
        self.call(act, query, None, false).await
    }

    async fn submit(
        &self,
        act: &str,
        query: &[(&str, String)],
        form: Vec<(String, String)>,
    ) -> Result<Value, IntegrationError> {
        self.call(act, query, Some(form), false).await
    }

    async fn vps(&self, vm: &VmRef) -> Result<Value, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self.read("managevps", &[("vpsid", id.to_string())]).await?;
        v.get("vps")
            .filter(|vps| vps.is_object() && vps.get("vpsid").is_some())
            .cloned()
            .ok_or_else(|| IntegrationError::NotFound(format!("Virtualizor VPS {id}")))
    }

    /// Power state per vpsid (`1` = running).
    async fn power_states(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, bool>, IntegrationError> {
        let mut out = std::collections::HashMap::new();
        for chunk in ids.chunks(100) {
            let v = self.read("vs", &[("vs_status", chunk.join(","))]).await?;
            for (id, st) in v
                .get("status")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                out.insert(id.clone(), value_u64(st) == Some(1));
            }
        }
        Ok(out)
    }

    fn power_query(
        vm: &VmRef,
        action: &str,
    ) -> Result<Vec<(&'static str, String)>, IntegrationError> {
        let mut q = vec![("action", action.to_owned())];
        if let Some(serid) = vm.node.as_deref().filter(|s| !s.is_empty()) {
            q.push(("serid", serid.to_owned()));
        }
        q.push(("vpsid", vpsid(vm)?.to_string()));
        Ok(q)
    }

    async fn power_action(&self, vm: &VmRef, action: &str) -> Result<(), IntegrationError> {
        let q = Self::power_query(vm, action)?;
        let v = self.trigger("vs", &q).await?;
        ensure_done(&v, action)
    }
}

// ----------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------

fn vpsid(vm: &VmRef) -> Result<u64, IntegrationError> {
    vm.id.trim().parse().map_err(|_| {
        IntegrationError::InvalidInput(format!("invalid Virtualizor vpsid {:?}", vm.id))
    })
}

/// Error messages from `error` (object, array or string); `None` when empty.
fn error_text(v: &Value) -> Option<String> {
    let msgs: Vec<String> = match v.get("error")? {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|m| m.as_str().map(str::to_owned))
            .collect(),
        Value::Object(o) => o
            .values()
            .filter_map(|m| m.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    let msgs: Vec<String> = msgs
        .into_iter()
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .collect();
    (!msgs.is_empty()).then(|| msgs.join("; "))
}

fn classify(message: String) -> IntegrationError {
    let lower = message.to_ascii_lowercase();
    if [
        "api key",
        "apikey",
        "api pass",
        "authenticat",
        "unauthori",
        "permission",
        "not allowed",
        "access denied",
        "login",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        IntegrationError::Auth(message)
    } else if [
        "does not exist",
        "not found",
        "not exist",
        "invalid vps",
        "no such",
        "no vps",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        IntegrationError::NotFound(message)
    } else if ["already", "in progress", "locked", "is suspended"]
        .iter()
        .any(|k| lower.contains(k))
    {
        IntegrationError::Conflict(message)
    } else {
        IntegrationError::remote(None, message)
    }
}

/// Successful write calls carry a truthy `done`.
fn ensure_done(v: &Value, what: &str) -> Result<(), IntegrationError> {
    let done = match v.get("done") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64() != Some(0),
        Some(Value::String(s)) => !s.is_empty() && s != "0",
        Some(Value::Object(_) | Value::Array(_)) => true,
        _ => false,
    };
    if done {
        Ok(())
    } else {
        Err(IntegrationError::remote(
            None,
            format!("Virtualizor did not confirm {what}"),
        ))
    }
}

/// Addresses in `ips` (array, `{id: ip}` object or a single string).
fn ip_strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        Some(Value::Object(o)) => o
            .values()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn vps_info(v: &Value, running: Option<bool>) -> Option<VmInfo> {
    let id = v.u64_at("vpsid")?.to_string();
    let serid = v.u64_at("serid").unwrap_or(0).to_string();
    let mut addrs = ip_strings(v.get("ips"));
    addrs.extend(ip_strings(v.get("ip")));
    addrs.extend(ip_strings(v.get("ips6")));
    let (ipv4, ipv6) = split_ips(addrs.iter().map(String::as_str));
    let suspended = v.bool_at("suspended").unwrap_or(false);
    let status = if suspended {
        VmStatus::Suspended
    } else {
        match running {
            Some(true) => VmStatus::Running,
            Some(false) => VmStatus::Stopped,
            None => VmStatus::Unknown,
        }
    };
    Some(VmInfo {
        vm: VmRef::on_node(serid.clone(), id.clone()),
        name: v
            .str_at("vps_name")
            .or(v.str_at("hostname"))
            .unwrap_or(&id)
            .to_owned(),
        status,
        hostname: v.str_at("hostname").map(str::to_owned),
        cpu_cores: v.u64_at("cores").and_then(|c| u32::try_from(c).ok()),
        memory_mb: v.u64_at("ram"),
        disk_gb: v.u64_at("space"),
        ipv4,
        ipv6,
        template: v.u64_at("osid").map(|o| o.to_string()),
        plan: v.u64_at("plid").filter(|p| *p > 0).map(|p| p.to_string()),
        tags: Vec::new(),
        created_at: None,
        uptime_secs: None,
        extra: json!({
            "virt": v.str_at("virt"),
            "serid": serid,
            "uid": v.u64_at("uid"),
            "bandwidth_gb": v.u64_at("bandwidth"),
            "os_name": v.str_at("os_name"),
        }),
    })
}

fn push(form: &mut Vec<(String, String)>, k: &str, v: impl ToString) {
    form.push((k.to_owned(), v.to_string()));
}

/// Mbit/s → KB/s (Virtualizor `network_speed`).
fn kbytes_per_s(mbps: u32) -> u64 {
    u64::from(mbps) * 125
}

#[async_trait]
impl VirtProvider for VirtualizorProvider {
    fn kind(&self) -> &'static str {
        kinds::VIRTUALIZOR
    }

    fn capabilities(&self) -> VirtCapabilities {
        VirtCapabilities {
            snapshots: false,
            memory_snapshots: false,
            live_resize: false,
            reinstall: true,
            console_kinds: vec![ConsoleKind::NoVnc],
            metrics: true,
            multi_node: true,
            plan_based: false,
            async_tasks: true,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let v = self.read("servers", &[]).await?;
        let servers = v
            .get("servs")
            .and_then(Value::as_object)
            .map_or(0, |o| o.len());
        Ok(ConnectionInfo {
            server_version: v.str_at("version").map(|s| format!("Virtualizor {s}")),
            details: Some(format!("{servers} server(s)")),
        })
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, IntegrationError> {
        let v = self.read("servers", &[]).await?;
        Ok(v.get("servs")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(key, s)| {
                let id = s
                    .u64_at("serid")
                    .map_or_else(|| key.clone(), |i| i.to_string());
                NodeInfo {
                    name: s.str_at("server_name").unwrap_or(&id).to_owned(),
                    online: s.u64_at("status") == Some(1),
                    location: s.str_at("region").map(str::to_owned),
                    memory_total_mb: s.u64_at("total_ram"),
                    disk_total_gb: s.u64_at("total_space"),
                    vm_count: s.u64_at("numvps").and_then(|n| u32::try_from(n).ok()),
                    extra: json!({"ip": s.str_at("ip"), "virt": s.str_at("virt")}),
                    id,
                    ..NodeInfo::default()
                }
            })
            .collect())
    }

    async fn list_templates(
        &self,
        _node: Option<&str>,
    ) -> Result<Vec<TemplateInfo>, IntegrationError> {
        let v = self.read("ostemplates", &[]).await?;
        let all: Vec<&Value> = v
            .get("ostemplates")
            .and_then(Value::as_object)
            .map(|o| o.values().collect())
            .unwrap_or_default();
        let virt = self.settings.virt.as_str();
        let any_match = all.iter().any(|t| t.str_at("type") == Some(virt));
        Ok(all
            .iter()
            .filter(|t| !any_match || t.str_at("type") == Some(virt))
            .filter_map(|t| {
                let id = t.u64_at("osid")?.to_string();
                let name = t.str_at("name").unwrap_or(&id).to_owned();
                let distro = t.str_at("distro").unwrap_or_default();
                Some(TemplateInfo {
                    os_family: guess_os_family(distro)
                        .or_else(|| guess_os_family(&name))
                        .or(Some(OsFamily::Other)),
                    id,
                    name,
                    node: None,
                    min_disk_gb: None,
                    cloud_init: false,
                    extra: json!({"type": t.str_at("type"), "distro": distro, "filename": t.str_at("filename")}),
                })
            })
            .collect())
    }

    async fn list_vms(&self, node: Option<&str>) -> Result<Vec<VmInfo>, IntegrationError> {
        let mut rows: Vec<Value> = Vec::new();
        for page in 1..=MAX_PAGES {
            let v = self
                .read(
                    "vs",
                    &[
                        ("page", page.to_string()),
                        ("reslen", PAGE_SIZE.to_string()),
                    ],
                )
                .await?;
            let batch: Vec<Value> = v
                .get("vs")
                .and_then(Value::as_object)
                .map(|o| o.values().cloned().collect())
                .unwrap_or_default();
            let n = batch.len();
            rows.extend(batch);
            if (n as u64) < PAGE_SIZE {
                break;
            }
        }
        rows.retain(|r| {
            node.is_none_or(|n| r.u64_at("serid").map(|s| s.to_string()).as_deref() == Some(n))
        });
        let ids: Vec<String> = rows
            .iter()
            .filter_map(|r| r.u64_at("vpsid"))
            .map(|i| i.to_string())
            .collect();
        let states = self.power_states(&ids).await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let id = r.u64_at("vpsid")?.to_string();
                vps_info(r, states.get(&id).copied())
            })
            .collect())
    }

    async fn get_vm(&self, vm: &VmRef) -> Result<VmInfo, IntegrationError> {
        let vps = self.vps(vm).await?;
        let id = vpsid(vm)?.to_string();
        let states = self.power_states(std::slice::from_ref(&id)).await?;
        vps_info(&vps, states.get(&id).copied())
            .ok_or_else(|| IntegrationError::Transport("malformed VPS record".into()))
    }

    async fn create_vm(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        validate_spec(spec)?;
        let extra = &spec.extra;
        if spec
            .cloud_init_user_data
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
        {
            return Err(IntegrationError::InvalidInput(
                "Virtualizor does not accept raw cloud-init user data; use a recipe (extra.recipe)"
                    .into(),
            ));
        }
        let osid: u64 = spec.template.trim().parse().map_err(|_| {
            IntegrationError::InvalidInput("template must be a Virtualizor osid".into())
        })?;
        let generated = spec.root_password.is_none().then(|| generate_password(20));
        let password = spec
            .root_password
            .clone()
            .or_else(|| generated.clone())
            .unwrap_or_default();
        let mut form = Vec::new();
        push(&mut form, "addvps", 1);
        push(
            &mut form,
            "virt",
            extra_str(extra, "virt").unwrap_or(&self.settings.virt),
        );
        push(&mut form, "osid", osid);
        push(
            &mut form,
            "hostname",
            if spec.hostname.trim().is_empty() {
                spec.name.trim()
            } else {
                spec.hostname.trim()
            },
        );
        push(&mut form, "rootpass", password.expose_str());
        push(&mut form, "ram", spec.memory_mb);
        push(&mut form, "space", spec.disk_gb);
        push(&mut form, "cores", spec.cpu_cores);
        push(&mut form, "bandwidth", spec.bandwidth_gb.unwrap_or(0));
        push(&mut form, "vnc", 1);
        let send_email = extra_bool(extra, "send_email").unwrap_or(false);
        push(&mut form, "noemail", u8::from(!send_email));
        let serid = match spec.node.as_deref() {
            Some(n) => Some(n.trim().parse::<u64>().map_err(|_| {
                IntegrationError::InvalidInput(
                    "node must be a Virtualizor server id (serid)".into(),
                )
            })?),
            None => self.settings.default_server_id,
        };
        if let Some(serid) = serid {
            push(&mut form, "serid", serid);
        }
        if let Some(uid) = extra_u64(extra, "uid")?.or(self.settings.default_user_id) {
            push(&mut form, "uid", uid);
        } else if let Some(email) = extra_str(extra, "user_email") {
            push(&mut form, "user_email", email);
            let user_pass = extra_str(extra, "user_pass")
                .map(SecretString::from)
                .unwrap_or_else(|| generate_password(16));
            push(&mut form, "user_pass", user_pass.expose_str());
            for key in ["fname", "lname"] {
                if let Some(v) = extra_str(extra, key) {
                    push(&mut form, key, v);
                }
            }
        } else {
            return Err(IntegrationError::InvalidInput(
                "Virtualizor needs an owner: extra.uid, extra.user_email or default_user_id".into(),
            ));
        }
        let plan = match spec.plan.as_deref() {
            Some(p) => Some(p.trim().parse::<u64>().map_err(|_| {
                IntegrationError::InvalidInput("plan must be a Virtualizor plan id (plid)".into())
            })?),
            None => self.settings.default_plan_id,
        };
        if let Some(plid) = plan {
            push(&mut form, "plid", plid);
        }
        if let Some(swap) = extra_u64(extra, "swap_mb")? {
            push(&mut form, "swapram", swap);
        }
        if let Some(cpu) = extra_u64(extra, "cpu_units")? {
            push(&mut form, "cpu", cpu);
        }
        match &spec.network.ipv4 {
            Some(IpConfig::Static(s)) => push(&mut form, "ips[]", s.address),
            Some(IpConfig::Auto | IpConfig::Dhcp) => push(&mut form, "num_ips", 1),
            None => push(&mut form, "num_ips", 0),
        }
        match &spec.network.ipv6 {
            Some(IpConfig::Static(s)) => push(&mut form, "ipv6[]", s.address),
            Some(IpConfig::Auto | IpConfig::Dhcp) => push(&mut form, "num_ips6", 1),
            None => {}
        }
        if let Some(pool) = extra_u64(extra, "ip_pool_id")?.or(self.settings.ip_pool_id) {
            push(&mut form, "ippoolid", pool);
        }
        for dns in &spec.network.dns_servers {
            push(&mut form, "dns[]", dns);
        }
        if let Some(mac) = &spec.network.mac_address {
            push(&mut form, "mac", mac);
        }
        if let Some(rate) = spec.network.rate_limit_mbps {
            push(&mut form, "network_speed", kbytes_per_s(rate));
            push(&mut form, "upload_speed", kbytes_per_s(rate));
        }
        if !spec.ssh_keys.is_empty() {
            push(&mut form, "eu_sshkeys", spec.ssh_keys.join("\n"));
        }
        if let Some(recipe) = extra_u64(extra, "recipe")? {
            push(&mut form, "recipe", recipe);
        }
        if !spec.start_after_create {
            tracing::info!(
                provider = PROVIDER,
                "Virtualizor boots new VPSs; start_after_create=false is ignored"
            );
        }
        let secrets = [password.expose_str()];
        let v = self
            .submit("addvs", &[], form)
            .await
            .map_err(|e| redact_error(e, &secrets))?;
        let info = v
            .get("vs_info")
            .or_else(|| v.get("newvs"))
            .cloned()
            .unwrap_or(Value::Null);
        let id = info
            .u64_at("vpsid")
            .or_else(|| v.get("done").and_then(|d| d.u64_at("vpsid")))
            .ok_or_else(|| {
                IntegrationError::Transport("addvs succeeded but returned no vpsid".into())
            })?;
        let actid = v
            .u64_at("actid")
            .or_else(|| v.get("done").and_then(|d| d.u64_at("actid")))
            .or_else(|| info.u64_at("actid"));
        let node = info.u64_at("serid").or(serid).unwrap_or(0).to_string();
        let mut addrs = ip_strings(info.get("ips"));
        addrs.extend(ip_strings(info.get("ips6")));
        let (ipv4, ipv6) = split_ips(addrs.iter().map(String::as_str));
        Ok(VmProvisioned {
            vm: VmRef::on_node(node, id.to_string()),
            task: actid.map(|a| TaskRef {
                id: a.to_string(),
                node: None,
            }),
            root_password: generated,
            ipv4,
            ipv6,
            extra: json!({"vpsid": id, "vps_name": info.str_at("vps_name")}),
        })
    }

    async fn power(
        &self,
        vm: &VmRef,
        action: PowerAction,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        match action {
            PowerAction::Start => self.power_action(vm, "start").await?,
            PowerAction::Stop => self.power_action(vm, "poweroff").await?,
            PowerAction::Shutdown => self.power_action(vm, "stop").await?,
            PowerAction::Reboot => self.power_action(vm, "restart").await?,
            PowerAction::Reset => {
                // No hardware reset in the API: hard power-off, then start.
                self.power_action(vm, "poweroff").await?;
                self.power_action(vm, "start").await?;
            }
        }
        Ok(None)
    }

    async fn resize(
        &self,
        vm: &VmRef,
        resize: &VmResize,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let id = vpsid(vm)?;
        let current = self.vps(vm).await?;
        if let (Some(new), Some(old)) = (resize.disk_gb, current.u64_at("space"))
            && new < old
        {
            return Err(IntegrationError::InvalidInput(format!(
                "disks can only grow (current {old} GiB)"
            )));
        }
        if let Some(plan) = &resize.plan {
            let plid: u64 = plan.trim().parse().map_err(|_| {
                IntegrationError::InvalidInput("plan must be a Virtualizor plid".into())
            })?;
            let mut form = Vec::new();
            push(&mut form, "editvps", 1);
            push(&mut form, "plid", plid);
            let v = self
                .submit("managevps", &[("vpsid", id.to_string())], form)
                .await?;
            ensure_done(&v, "plan change")?;
        }
        if resize.cpu_cores.is_some() || resize.memory_mb.is_some() || resize.disk_gb.is_some() {
            let mut form = Vec::new();
            push(&mut form, "vpsid", id);
            push(&mut form, "editvps", 1);
            if let Some(c) = resize.cpu_cores {
                push(&mut form, "cores", c);
            }
            if let Some(m) = resize.memory_mb {
                push(&mut form, "ram", m);
            }
            if let Some(d) = resize.disk_gb {
                push(&mut form, "space", d);
            }
            let v = self
                .submit("editvs", &[("vpsid", id.to_string())], form)
                .await?;
            ensure_done(&v, "resource change")?;
        }
        if resize.allow_restart
            && (resize.cpu_cores.is_some() || resize.memory_mb.is_some() || resize.plan.is_some())
        {
            let running = self.power_states(&[id.to_string()]).await?;
            if running.get(&id.to_string()).copied().unwrap_or(false) {
                self.power_action(vm, "restart").await?;
            }
        }
        Ok(None)
    }

    async fn reinstall(
        &self,
        vm: &VmRef,
        request: &VmReinstall,
    ) -> Result<VmProvisioned, IntegrationError> {
        let id = vpsid(vm)?;
        let osid: u64 = request.template.trim().parse().map_err(|_| {
            IntegrationError::InvalidInput("template must be a Virtualizor osid".into())
        })?;
        for key in &request.ssh_keys {
            validate_ssh_key(key)?;
        }
        // `osid` must be one of the OS templates `list_templates` offers
        // (admin-managed `ostemplates` of this panel's virtualisation type).
        let offered = self.list_templates(None).await?;
        if !offered.iter().any(|t| t.id == osid.to_string()) {
            return Err(IntegrationError::InvalidInput(format!(
                "{osid} is not an available OS template"
            )));
        }
        let generated = request
            .root_password
            .is_none()
            .then(|| generate_password(20));
        let password = request
            .root_password
            .clone()
            .or_else(|| generated.clone())
            .unwrap_or_default();
        let mut form = Vec::new();
        push(&mut form, "reos", 1);
        push(&mut form, "vpsid", id);
        push(&mut form, "osid", osid);
        push(&mut form, "newpass", password.expose_str());
        push(&mut form, "conf", password.expose_str());
        if !request.ssh_keys.is_empty() {
            push(&mut form, "sshkey", request.ssh_keys.join("\n"));
        }
        if let Some(recipe) = extra_u64(&request.extra, "recipe")? {
            push(&mut form, "recipe", recipe);
        }
        let secrets = [password.expose_str()];
        let v = self
            .submit("rebuild", &[], form)
            .await
            .map_err(|e| redact_error(e, &secrets))?;
        ensure_done(&v, "rebuild")?;
        let mut warnings = Vec::new();
        if request.hostname.is_some() {
            warnings.push("hostname is not changed by Virtualizor rebuild");
        }
        if request.cloud_init_user_data.is_some() {
            warnings.push("cloud-init user data is not supported by Virtualizor");
        }
        Ok(VmProvisioned {
            vm: vm.clone(),
            task: v.u64_at("actid").map(|a| TaskRef {
                id: a.to_string(),
                node: None,
            }),
            root_password: generated,
            ipv4: Vec::new(),
            ipv6: Vec::new(),
            extra: json!({"warnings": warnings}),
        })
    }

    async fn delete_vm(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self.trigger("vs", &[("delete", id.to_string())]).await?;
        ensure_done(&v, "delete")?;
        Ok(None)
    }

    async fn list_snapshots(&self, _vm: &VmRef) -> Result<Vec<SnapshotInfo>, IntegrationError> {
        Err(IntegrationError::Unsupported(
            "snapshots (Virtualizor admin API offers backups only)",
        ))
    }

    async fn create_snapshot(
        &self,
        _vm: &VmRef,
        _r: &SnapshotRequest,
    ) -> Result<SnapshotCreated, IntegrationError> {
        Err(IntegrationError::Unsupported(
            "snapshots (Virtualizor admin API offers backups only)",
        ))
    }

    async fn rollback_snapshot(
        &self,
        _vm: &VmRef,
        _id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        Err(IntegrationError::Unsupported(
            "snapshots (Virtualizor admin API offers backups only)",
        ))
    }

    async fn delete_snapshot(
        &self,
        _vm: &VmRef,
        _id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        Err(IntegrationError::Unsupported(
            "snapshots (Virtualizor admin API offers backups only)",
        ))
    }

    async fn console(&self, vm: &VmRef) -> Result<ConsoleAccess, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self.read("vnc", &[("novnc", id.to_string())]).await?;
        let info =
            v.get("info")
                .filter(|i| i.is_object())
                .ok_or_else(|| IntegrationError::Remote {
                    status: None,
                    code: None,
                    message: "VNC is not available for this VPS (is VNC enabled?)".into(),
                })?;
        let host = info
            .str_at("vnc_hostname")
            .or(info.str_at("ip"))
            .ok_or_else(|| IntegrationError::Transport("VNC info without host".into()))?;
        let port = info.u64_at("port").and_then(|p| u16::try_from(p).ok());
        Ok(ConsoleAccess {
            kind: ConsoleKind::NoVnc,
            url: match port {
                Some(p) => format!("vnc://{host}:{p}"),
                None => format!("vnc://{host}"),
            },
            ticket: None,
            password: info.str_at("password").map(SecretString::from),
            port,
            expires_at: None,
            extra: json!({
                "protocol": "rfb",
                "websocket": false,
                "virt": info.str_at("virt"),
                "note": "Raw VNC endpoint on the hypervisor; browsers need a websocket proxy \
                         (e.g. websockify) in front of it.",
            }),
        })
    }

    async fn metrics(&self, vm: &VmRef) -> Result<VmMetrics, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self
            .read(
                "vps_stats",
                &[("vpsid", id.to_string()), ("live", "1".into())],
            )
            .await?;
        let row = v
            .get("vps_data")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(_, r)| r)
            .find(|r| r.u64_at("vpsid") == Some(id))
            .cloned()
            .ok_or_else(|| IntegrationError::NotFound(format!("statistics of VPS {id}")))?;
        let status = match row.u64_at("status") {
            Some(1) => VmStatus::Running,
            Some(0) => VmStatus::Stopped,
            _ => VmStatus::Unknown,
        };
        Ok(VmMetrics {
            collected_at: Utc::now(),
            status,
            cpu_usage_percent: row.f64_at("used_cpu").map(|c| c.clamp(0.0, 100.0)),
            memory_used_mb: row.u64_at("used_ram"),
            memory_total_mb: row.u64_at("ram"),
            disk_used_gb: row.f64_at("used_disk"),
            disk_total_gb: row.f64_at("disk"),
            disk_read_bps: row.f64_at("io_read"),
            disk_write_bps: row.f64_at("io_write"),
            net_in_bps: row.f64_at("net_in"),
            net_out_bps: row.f64_at("net_out"),
            bandwidth_used_gb: row.f64_at("used_bandwidth"),
            uptime_secs: None,
        })
    }

    async fn task_status(&self, task: &TaskRef) -> Result<TaskState, IntegrationError> {
        let actid: u64 = task.id.trim().parse().map_err(|_| {
            IntegrationError::InvalidInput(format!("invalid Virtualizor task id {:?}", task.id))
        })?;
        let v = self.read("tasks", &[("actid", actid.to_string())]).await?;
        let row = v
            .get("tasks")
            .and_then(Value::as_object)
            .and_then(|o| o.get(&actid.to_string()).or_else(|| o.values().next()))
            .cloned()
            .ok_or_else(|| IntegrationError::NotFound(format!("Virtualizor task {actid}")))?;
        let text = row.str_at("status_txt").unwrap_or_default().to_owned();
        let lower = text.to_ascii_lowercase();
        let progress = row.u64_at("progress").map(|p| p.min(100));
        let errored = row.get("status").and_then(Value::as_str) == Some("-1")
            || row.get("status").and_then(Value::as_i64) == Some(-1)
            || lower.contains("error")
            || lower.contains("fail");
        Ok(if errored {
            TaskState::Failed {
                message: if text.is_empty() {
                    "task failed".into()
                } else {
                    text
                },
            }
        } else if progress == Some(100) || lower.contains("complet") {
            TaskState::Succeeded
        } else {
            TaskState::Running {
                progress_percent: progress.and_then(|p| u8::try_from(p).ok()),
            }
        })
    }

    async fn suspend(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self.trigger("vs", &[("suspend", id.to_string())]).await?;
        ensure_done(&v, "suspend")?;
        Ok(None)
    }

    async fn unsuspend(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self.trigger("vs", &[("unsuspend", id.to_string())]).await?;
        ensure_done(&v, "unsuspend")?;
        Ok(None)
    }

    fn features(&self) -> VirtFeatures {
        VirtFeatures {
            console_relay: true,
            metrics_history: true,
            bandwidth: true,
            ..VirtFeatures::default()
        }
    }

    /// `act=vnc&novnc={vpsid}` → `info.vnc_hostname` (or `ip`), `port`,
    /// `password`: raw VNC over TCP on the hypervisor, relayed server-side.
    async fn console_upstream(&self, vm: &VmRef) -> Result<ConsoleUpstream, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self.read("vnc", &[("novnc", id.to_string())]).await?;
        let info = v
            .get("info")
            .filter(|i| i.is_object())
            .ok_or(IntegrationError::Unsupported(
                "VNC console (VNC disabled for this VPS)",
            ))?;
        let host = info
            .str_at("vnc_hostname")
            .or(info.str_at("ip"))
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .ok_or_else(|| IntegrationError::Transport("VNC info without host".into()))?;
        let port = info
            .u64_at("port")
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| IntegrationError::Transport("VNC info without port".into()))?;
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        Ok(ConsoleUpstream {
            transport: ConsoleTransport::Tcp,
            url: format!("{host}:{port}"),
            headers: Vec::new(),
            subprotocols: Vec::new(),
            accept_invalid_certs: false,
            vnc_password: info.str_at("password").map(SecretString::from),
            expires_at: None,
        })
    }

    /// `act=vps_stats&svs={vpsid}&show=YYYYMM` (monthly series: `time`,
    /// `cpu` %, `ram` MB; the current and, for long ranges, previous month).
    async fn metrics_history(
        &self,
        vm: &VmRef,
        range: MetricsRange,
    ) -> Result<MetricsHistory, IntegrationError> {
        use chrono::Datelike as _;
        let id = vpsid(vm)?;
        let now = Utc::now();
        let since = now - range.duration();
        let mut months = vec![format!("{:04}{:02}", now.year(), now.month())];
        if since.month() != now.month() || since.year() != now.year() {
            months.insert(0, format!("{:04}{:02}", since.year(), since.month()));
        }
        let mut samples = Vec::new();
        for show in months {
            let v = self
                .read("vps_stats", &[("svs", id.to_string()), ("show", show)])
                .await?;
            for row in v
                .get("vps_stats")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(at) = row
                    .u64_at("time")
                    .and_then(|t| i64::try_from(t).ok())
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                else {
                    continue;
                };
                if at < since {
                    continue;
                }
                samples.push(MetricSample {
                    at: Some(at),
                    cpu_percent: row.f64_at("cpu").map(|c| c.clamp(0.0, 100.0)),
                    memory_used_mb: row.f64_at("ram"),
                    ..MetricSample::default()
                });
            }
        }
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

    /// `act=vsbandwidth&vm_ids={vpsid}` → `vpsusage.{id}.used` / `limit` (MB).
    async fn bandwidth_usage(&self, vm: &VmRef) -> Result<BandwidthUsage, IntegrationError> {
        let id = vpsid(vm)?;
        let v = self
            .read("vsbandwidth", &[("vm_ids", id.to_string())])
            .await?;
        let row = v
            .get("vpsusage")
            .and_then(|u| u.get(id.to_string()))
            .cloned()
            .ok_or_else(|| IntegrationError::NotFound(format!("bandwidth of VPS {id}")))?;
        let used_mb = row.f64_at("used").unwrap_or(0.0);
        let limit_mb = row.f64_at("limit").filter(|l| *l > 0.0);
        Ok(BandwidthUsage {
            outgoing_gb: None,
            incoming_gb: None,
            counted_gb: used_mb / 1024.0,
            provider_limit_gb: limit_mb.map(|l| l / 1024.0),
            estimated: false,
            period_start: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_shapes() {
        assert_eq!(
            error_text(&json!({"error": {"1": "Invalid VPS"}})).as_deref(),
            Some("Invalid VPS")
        );
        assert_eq!(
            error_text(&json!({"error": ["a", "b"]})).as_deref(),
            Some("a; b")
        );
        assert_eq!(error_text(&json!({"error": []})), None);
        assert_eq!(error_text(&json!({"error": {}})), None);
        assert_eq!(error_text(&json!({"done": 1})), None);
    }

    #[test]
    fn classification() {
        assert_eq!(classify("Invalid API Key".into()).kind(), "auth");
        assert_eq!(
            classify("The VPS does not exist".into()).kind(),
            "not_found"
        );
        assert_eq!(classify("VPS is already running".into()).kind(), "conflict");
        assert_eq!(classify("Disk full".into()).kind(), "remote");
        assert!(!classify("Disk full".into()).is_retryable());
    }

    #[test]
    fn done_detection() {
        assert!(ensure_done(&json!({"done": 1}), "x").is_ok());
        assert!(ensure_done(&json!({"done": {"msg": "ok"}}), "x").is_ok());
        assert!(ensure_done(&json!({"done": 0}), "x").is_err());
        assert!(ensure_done(&json!({}), "x").is_err());
    }

    #[test]
    fn vps_mapping() {
        let v = json!({"vpsid": "7", "hostname": "web01.example.com", "ips": {"3": "203.0.113.10"},
                       "ram": "1024", "cores": "2", "space": "20", "serid": "1", "virt": "kvm"});
        let info = vps_info(&v, Some(true)).unwrap();
        assert_eq!(info.vm, VmRef::on_node("1", "7"));
        assert_eq!(info.status, VmStatus::Running);
        assert_eq!(info.memory_mb, Some(1024));
        assert_eq!(
            info.ipv4,
            vec!["203.0.113.10".parse::<std::net::IpAddr>().unwrap()]
        );
        assert_eq!(kbytes_per_s(100), 12_500);
    }
}
