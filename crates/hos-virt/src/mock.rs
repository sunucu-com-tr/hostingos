//! In-memory "Demo / Test" provider (`kind = "mock"`).
//!
//! A deterministic state machine behind a mutex that implements every
//! [`VirtProvider`] method the way a real hypervisor would: VMs move through
//! `provisioning → running ⇄ stopped`, power actions are validated against the
//! current state, disks only grow, snapshots capture and restore the VM's
//! configuration, suspension blocks power-on, and every mutating call can be
//! made asynchronous (returning a [`TaskRef`] that completes after a number of
//! `task_status` polls) to exercise the job queue.
//!
//! Config (all optional): `async_tasks` (bool), `task_polls` (polls until a
//! task completes, default 2), `latency_ms` (artificial delay per call),
//! `fail_operations` (comma list of trait method names that fail with a
//! `Remote` error, e.g. `create_vm,console`), `nodes` (number of nodes, 1–8,
//! default 2), `adopt_unknown_vms` (bool: an unknown VM id is re-created as a
//! running VM on first use, so a demo survives a restart).
//!
//! Every extended operation is simulated too: the console relay upstream is
//! the in-process VNC server of [`crate::console::sim`], metrics history is a
//! deterministic synthetic curve, two daily backups exist per VM, three ISOs
//! are offered, and rescue / PTR / firewall settings are kept per VM.
//!
//! Templates: `ubuntu-24.04`, `debian-12`, `almalinux-9`, `windows-2022`.
//! Addresses come from the documentation ranges 203.0.113.0/24 and
//! 2001:db8::/32. Nothing leaves the process.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hos_core::IntegrationError;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::virt::{
    BackupInfo, BandwidthUsage, ConsoleAccess, ConsoleKind, ConsoleTransport, ConsoleUpstream,
    FirewallState, IpConfig, IsoImage, MetricSample, MetricsHistory, MetricsRange, NodeInfo,
    OsFamily, PasswordReset, PasswordResetRequest, PowerAction, RescueResult, ReverseDnsEntry,
    SnapshotCreated, SnapshotInfo, SnapshotRequest, TaskRef, TaskState, TemplateInfo,
    VirtCapabilities, VirtFeatures, VmExtras, VmInfo, VmMetrics, VmProvisioned, VmRef, VmReinstall,
    VmResize, VmSpec, VmStatus,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, VirtProvider,
};
use serde_json::{Value, json};

use crate::MOCK;
use crate::util::{generate_password, snapshot_name, split_list, validate_spec};

/// Admin-form descriptor ("Demo / Test").
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(MOCK, "Demo / Test", IntegrationCategory::Virt)
        .description(
            "Simulated hypervisor kept in memory: no real VMs are created. For demos, staging \
             and end-to-end tests. State is lost when the application restarts.",
        )
        .config_field(
            FieldSpec::boolean("async_tasks", "Simulate asynchronous tasks")
                .default_value(false)
                .help("Mutating calls return task ids that complete after a few polls."),
        )
        .config_field(
            FieldSpec::number("task_polls", "Polls until a task completes").default_value(2),
        )
        .config_field(FieldSpec::number("latency_ms", "Artificial latency (ms)").default_value(0))
        .config_field(FieldSpec::number("nodes", "Number of nodes").default_value(2))
        .config_field(
            FieldSpec::text("fail_operations", "Operations that fail")
                .placeholder("create_vm,console")
                .help(
                    "Comma-separated method names that return a provider error (failure drills).",
                ),
        )
        .config_field(
            FieldSpec::boolean("adopt_unknown_vms", "Re-create unknown VMs")
                .default_value(false)
                .help(
                    "A VM id this process has not seen (e.g. after a restart) is re-created as a \
                     running VM on first use. Demo installations only.",
                ),
        )
}

#[derive(Debug, Clone)]
struct MockTemplate {
    id: &'static str,
    name: &'static str,
    family: OsFamily,
    min_disk_gb: u64,
}

const TEMPLATES: &[MockTemplate] = &[
    MockTemplate {
        id: "ubuntu-24.04",
        name: "Ubuntu 24.04 LTS",
        family: OsFamily::Linux,
        min_disk_gb: 10,
    },
    MockTemplate {
        id: "debian-12",
        name: "Debian 12",
        family: OsFamily::Linux,
        min_disk_gb: 8,
    },
    MockTemplate {
        id: "almalinux-9",
        name: "AlmaLinux 9",
        family: OsFamily::Linux,
        min_disk_gb: 10,
    },
    MockTemplate {
        id: "windows-2022",
        name: "Windows Server 2022",
        family: OsFamily::Windows,
        min_disk_gb: 40,
    },
];

#[derive(Debug, Clone, PartialEq)]
struct Shape {
    cpu: u32,
    memory_mb: u64,
    disk_gb: u64,
    template: String,
}

#[derive(Debug, Clone)]
struct MockSnapshot {
    id: String,
    name: String,
    description: Option<String>,
    created_at: DateTime<Utc>,
    includes_memory: bool,
    parent: Option<String>,
    shape: Shape,
    was_running: bool,
}

#[derive(Debug, Clone)]
struct MockVm {
    id: String,
    node: String,
    name: String,
    hostname: String,
    status: VmStatus,
    suspended: bool,
    shape: Shape,
    plan: Option<String>,
    ipv4: Vec<IpAddr>,
    ipv6: Vec<IpAddr>,
    tags: Vec<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    snapshots: Vec<MockSnapshot>,
    current_snapshot: Option<String>,
    extras: MockExtras,
}

/// State of the extended operations (rescue, ISO, PTR, firewall).
#[derive(Debug, Clone, Default)]
struct MockExtras {
    rescue: bool,
    iso: Option<String>,
    boot_iso: bool,
    ptr: BTreeMap<IpAddr, String>,
    firewall: FirewallState,
}

/// ISO images the mock offers.
const ISOS: &[(&str, &str)] = &[
    ("debian-12.8-netinst.iso", "Debian 12 netinst"),
    ("ubuntu-24.04-live-server.iso", "Ubuntu 24.04 live server"),
    ("systemrescue-11.03.iso", "SystemRescue 11"),
];

/// What happens when a task completes.
#[derive(Debug, Clone)]
enum Completion {
    SetStatus { vm: String, status: VmStatus },
    Delete { vm: String },
    Nothing,
}

#[derive(Debug, Clone)]
struct MockTask {
    polls_left: u32,
    completion: Completion,
    failed: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    vms: BTreeMap<String, MockVm>,
    tasks: BTreeMap<String, MockTask>,
    next_vm: u64,
    next_task: u64,
    next_ip: u32,
}

#[derive(Debug, Clone)]
struct Options {
    async_tasks: bool,
    task_polls: u32,
    latency: Duration,
    fail: HashSet<String>,
    nodes: Vec<String>,
    adopt_unknown: bool,
}

/// In-memory provider. See the [module docs](self).
#[derive(Debug)]
pub struct MockProvider {
    opts: Options,
    state: Mutex<State>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// The counter VM ids start from. The mock keeps its VMs in memory, so a
/// fixed start (`1001`, `1002` …) reissues ids after every restart and a new
/// VM would take over the stored row of an older service (VMs are keyed by
/// integration + external id). Starting from the clock keeps ids distinct
/// across restarts (room for 1000 VMs per second of uptime).
fn first_vm_id() -> u64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    1000 + (secs % 100_000_000) * 1000
}

impl MockProvider {
    /// A synchronous mock with two nodes and no failures.
    pub fn new() -> Self {
        Self {
            opts: Options {
                async_tasks: false,
                task_polls: 2,
                latency: Duration::ZERO,
                fail: HashSet::new(),
                nodes: vec!["mock-node-1".into(), "mock-node-2".into()],
                adopt_unknown: false,
            },
            state: Mutex::new(State {
                next_vm: first_vm_id(),
                ..State::default()
            }),
        }
    }

    /// Creates the provider from `integrations.config` (secrets are ignored).
    pub fn from_config(config: &Value, _secrets: &Value) -> Result<Self, IntegrationError> {
        let cfg = FieldReader::config(config);
        let mut p = Self::new();
        p.opts.async_tasks = cfg.bool_or("async_tasks", false)?;
        p.opts.task_polls = u32::try_from(cfg.u64_or("task_polls", 2)?.clamp(1, 100)).unwrap_or(2);
        p.opts.latency = Duration::from_millis(cfg.u64_or("latency_ms", 0)?.min(10_000));
        p.opts.fail = cfg
            .opt_str("fail_operations")?
            .map(split_list)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let nodes = cfg.u64_or("nodes", 2)?.clamp(1, 8);
        p.opts.nodes = (1..=nodes).map(|i| format!("mock-node-{i}")).collect();
        p.opts.adopt_unknown = cfg.bool_or("adopt_unknown_vms", false)?;
        Ok(p)
    }

    /// Re-creates unknown VMs on lookup (builder style): a demo process keeps
    /// working after a restart even though the simulator state was lost.
    #[must_use]
    pub fn adopting_unknown_vms(mut self) -> Self {
        self.opts.adopt_unknown = true;
        self
    }

    /// `adopt_unknown_vms`: registers a running VM for an id this process
    /// has not seen.
    fn adopt(&self, vm: &VmRef) {
        if !self.opts.adopt_unknown {
            return;
        }
        let mut state = self.lock();
        if state.vms.contains_key(&vm.id) {
            return;
        }
        let (ipv4, ipv6) = alloc_ips(&mut state, &Some(IpConfig::Auto), &Some(IpConfig::Auto));
        let node = vm
            .node
            .clone()
            .filter(|n| self.opts.nodes.contains(n))
            .unwrap_or_else(|| self.opts.nodes[0].clone());
        let now = Utc::now();
        state.vms.insert(
            vm.id.clone(),
            MockVm {
                id: vm.id.clone(),
                node,
                name: format!("vm-{}", vm.id),
                hostname: format!("vm-{}.demo.invalid", vm.id),
                status: VmStatus::Running,
                suspended: false,
                shape: Shape {
                    cpu: 2,
                    memory_mb: 4096,
                    disk_gb: 80,
                    template: "ubuntu-24.04".into(),
                },
                plan: None,
                ipv4,
                ipv6,
                tags: vec!["adopted".into()],
                created_at: now - chrono::Duration::days(12),
                started_at: Some(now - chrono::Duration::hours(30)),
                snapshots: Vec::new(),
                current_snapshot: None,
                extras: MockExtras::default(),
            },
        );
    }

    /// Makes calls asynchronous (builder style, for tests).
    #[must_use]
    pub fn with_async_tasks(mut self, polls: u32) -> Self {
        self.opts.async_tasks = true;
        self.opts.task_polls = polls.max(1);
        self
    }

    /// Makes `operation` fail (builder style, for tests).
    #[must_use]
    pub fn failing(mut self, operation: &str) -> Self {
        self.opts.fail.insert(operation.to_owned());
        self
    }

    async fn enter(&self, op: &'static str) -> Result<(), IntegrationError> {
        if !self.opts.latency.is_zero() {
            tokio::time::sleep(self.opts.latency).await;
        }
        if self.opts.fail.contains(op) {
            return Err(IntegrationError::remote_with_code(
                Some(500),
                "simulated_failure",
                format!("simulated failure of {op}"),
            ));
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock cannot leave the state half-updated
        // in a way that matters for a simulator; keep serving.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Completes immediately (`None`) or registers a task, applying
    /// `completion` now or when the task finishes.
    fn finish(&self, state: &mut State, completion: Completion) -> Option<TaskRef> {
        if !self.opts.async_tasks {
            apply(state, &completion);
            return None;
        }
        state.next_task += 1;
        let id = format!("mock-task-{}", state.next_task);
        state.tasks.insert(
            id.clone(),
            MockTask {
                polls_left: self.opts.task_polls,
                completion,
                failed: None,
            },
        );
        Some(TaskRef { id, node: None })
    }

    fn pick_node(&self, state: &State, wanted: Option<&str>) -> Result<String, IntegrationError> {
        match wanted {
            Some(n) if self.opts.nodes.iter().any(|x| x == n) => Ok(n.to_owned()),
            Some(n) => Err(IntegrationError::NotFound(format!("node {n}"))),
            None => Ok(self
                .opts
                .nodes
                .iter()
                .min_by_key(|n| state.vms.values().filter(|v| &v.node == *n).count())
                .cloned()
                .unwrap_or_else(|| "mock-node-1".into())),
        }
    }
}

fn apply(state: &mut State, completion: &Completion) {
    match completion {
        Completion::SetStatus { vm, status } => {
            if let Some(v) = state.vms.get_mut(vm) {
                v.status = *status;
                v.started_at = (*status == VmStatus::Running).then(Utc::now);
            }
        }
        Completion::Delete { vm } => {
            state.vms.remove(vm);
        }
        Completion::Nothing => {}
    }
}

fn template(id: &str) -> Result<&'static MockTemplate, IntegrationError> {
    TEMPLATES
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| IntegrationError::NotFound(format!("template {id}")))
}

fn vm_mut<'a>(state: &'a mut State, vm: &VmRef) -> Result<&'a mut MockVm, IntegrationError> {
    state
        .vms
        .get_mut(&vm.id)
        .ok_or_else(|| IntegrationError::NotFound(format!("VM {}", vm.id)))
}

fn busy(vm: &MockVm) -> Result<(), IntegrationError> {
    if matches!(
        vm.status,
        VmStatus::Provisioning | VmStatus::Starting | VmStatus::Stopping
    ) {
        return Err(IntegrationError::Conflict(format!(
            "VM {} is busy ({})",
            vm.id, vm.status
        )));
    }
    Ok(())
}

fn alloc_ips(
    state: &mut State,
    spec_v4: &Option<IpConfig>,
    spec_v6: &Option<IpConfig>,
) -> (Vec<IpAddr>, Vec<IpAddr>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    match spec_v4 {
        Some(IpConfig::Static(s)) => v4.push(s.address),
        Some(IpConfig::Auto | IpConfig::Dhcp) => {
            state.next_ip += 1;
            let host = u8::try_from(10 + state.next_ip % 240).unwrap_or(10);
            v4.push(IpAddr::V4(Ipv4Addr::new(203, 0, 113, host)));
        }
        None => {}
    }
    match spec_v6 {
        Some(IpConfig::Static(s)) => v6.push(s.address),
        Some(IpConfig::Auto | IpConfig::Dhcp) => {
            state.next_ip += 1;
            let seg = u16::try_from(state.next_ip % 0xffff).unwrap_or(1);
            v6.push(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, seg, 0, 0, 0, 1)));
        }
        None => {}
    }
    (v4, v6)
}

fn info(vm: &MockVm) -> VmInfo {
    VmInfo {
        vm: VmRef::on_node(vm.node.clone(), vm.id.clone()),
        name: vm.name.clone(),
        status: if vm.suspended {
            VmStatus::Suspended
        } else {
            vm.status
        },
        hostname: Some(vm.hostname.clone()),
        cpu_cores: Some(vm.shape.cpu),
        memory_mb: Some(vm.shape.memory_mb),
        disk_gb: Some(vm.shape.disk_gb),
        ipv4: vm.ipv4.clone(),
        ipv6: vm.ipv6.clone(),
        template: Some(vm.shape.template.clone()),
        plan: vm.plan.clone(),
        tags: vm.tags.clone(),
        created_at: Some(vm.created_at),
        uptime_secs: uptime(vm),
        extra: json!({"simulated": true, "snapshots": vm.snapshots.len()}),
    }
}

fn uptime(vm: &MockVm) -> Option<u64> {
    vm.started_at
        .filter(|_| vm.status == VmStatus::Running)
        .map(|s| u64::try_from((Utc::now() - s).num_seconds()).unwrap_or(0))
}

/// Deterministic pseudo-random value in `[0, 1)` from a VM id and a salt.
fn wobble(id: &str, salt: u64) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ salt;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let minute = u64::try_from(Utc::now().timestamp() / 60).unwrap_or(0);
    h ^= minute.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (h % 10_000) as f64 / 10_000.0
}

#[async_trait]
impl VirtProvider for MockProvider {
    fn kind(&self) -> &'static str {
        MOCK
    }

    fn capabilities(&self) -> VirtCapabilities {
        VirtCapabilities {
            snapshots: true,
            memory_snapshots: true,
            live_resize: false,
            reinstall: true,
            console_kinds: vec![ConsoleKind::NoVnc],
            metrics: true,
            multi_node: self.opts.nodes.len() > 1,
            plan_based: false,
            async_tasks: self.opts.async_tasks,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        self.enter("test_connection").await?;
        let vms = self.lock().vms.len();
        Ok(ConnectionInfo {
            server_version: Some(format!("hostingos-mock/{}", env!("CARGO_PKG_VERSION"))),
            details: Some(format!(
                "{} simulated node(s), {vms} VM(s)",
                self.opts.nodes.len()
            )),
        })
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, IntegrationError> {
        self.enter("list_nodes").await?;
        let state = self.lock();
        Ok(self
            .opts
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let vms: Vec<&MockVm> = state.vms.values().filter(|v| &v.node == n).collect();
                let used_mem: u64 = vms.iter().map(|v| v.shape.memory_mb).sum();
                let used_disk: u64 = vms.iter().map(|v| v.shape.disk_gb).sum();
                NodeInfo {
                    id: n.clone(),
                    name: n.clone(),
                    online: true,
                    location: Some(if i % 2 == 0 { "Istanbul" } else { "Frankfurt" }.into()),
                    cpu_cores: Some(64),
                    cpu_usage_percent: Some((vms.len() as f64 * 3.5).min(100.0)),
                    memory_total_mb: Some(256 * 1024),
                    memory_used_mb: Some(used_mem),
                    disk_total_gb: Some(4096),
                    disk_used_gb: Some(used_disk),
                    vm_count: u32::try_from(vms.len()).ok(),
                    extra: json!({"simulated": true}),
                }
            })
            .collect())
    }

    async fn list_templates(
        &self,
        _node: Option<&str>,
    ) -> Result<Vec<TemplateInfo>, IntegrationError> {
        self.enter("list_templates").await?;
        Ok(TEMPLATES
            .iter()
            .map(|t| TemplateInfo {
                id: t.id.into(),
                name: t.name.into(),
                node: None,
                os_family: Some(t.family),
                min_disk_gb: Some(t.min_disk_gb),
                cloud_init: t.family == OsFamily::Linux,
                extra: json!({"simulated": true}),
            })
            .collect())
    }

    async fn list_vms(&self, node: Option<&str>) -> Result<Vec<VmInfo>, IntegrationError> {
        self.enter("list_vms").await?;
        let state = self.lock();
        Ok(state
            .vms
            .values()
            .filter(|v| node.is_none_or(|n| v.node == n))
            .map(info)
            .collect())
    }

    async fn get_vm(&self, vm: &VmRef) -> Result<VmInfo, IntegrationError> {
        self.enter("get_vm").await?;
        self.adopt(vm);
        let state = self.lock();
        state
            .vms
            .get(&vm.id)
            .map(info)
            .ok_or_else(|| IntegrationError::NotFound(format!("VM {}", vm.id)))
    }

    async fn create_vm(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError> {
        self.enter("create_vm").await?;
        validate_spec(spec)?;
        let tpl = template(&spec.template)?;
        if spec.disk_gb < tpl.min_disk_gb {
            return Err(IntegrationError::InvalidInput(format!(
                "template {} needs at least {} GiB",
                tpl.id, tpl.min_disk_gb
            )));
        }
        let mut state = self.lock();
        let name = if spec.name.trim().is_empty() {
            spec.hostname.clone()
        } else {
            spec.name.clone()
        };
        if state.vms.values().any(|v| v.name == name) {
            return Err(IntegrationError::Conflict(format!(
                "a VM named {name} already exists"
            )));
        }
        let node = self.pick_node(&state, spec.node.as_deref())?;
        state.next_vm += 1;
        let id = state.next_vm.to_string();
        let (ipv4, ipv6) = alloc_ips(&mut state, &spec.network.ipv4, &spec.network.ipv6);
        let generated = (spec.root_password.is_none() && spec.ssh_keys.is_empty())
            .then(|| generate_password(16));
        let target = if spec.start_after_create {
            VmStatus::Running
        } else {
            VmStatus::Stopped
        };
        state.vms.insert(
            id.clone(),
            MockVm {
                id: id.clone(),
                node: node.clone(),
                name,
                hostname: if spec.hostname.is_empty() {
                    spec.name.clone()
                } else {
                    spec.hostname.clone()
                },
                status: VmStatus::Provisioning,
                suspended: false,
                shape: Shape {
                    cpu: spec.cpu_cores,
                    memory_mb: spec.memory_mb,
                    disk_gb: spec.disk_gb,
                    template: tpl.id.into(),
                },
                plan: spec.plan.clone(),
                ipv4: ipv4.clone(),
                ipv6: ipv6.clone(),
                tags: spec.tags.clone(),
                created_at: Utc::now(),
                started_at: None,
                snapshots: Vec::new(),
                current_snapshot: None,
                extras: MockExtras::default(),
            },
        );
        let task = self.finish(
            &mut state,
            Completion::SetStatus {
                vm: id.clone(),
                status: target,
            },
        );
        Ok(VmProvisioned {
            vm: VmRef::on_node(node, id),
            task,
            root_password: generated,
            ipv4,
            ipv6,
            extra: json!({"simulated": true}),
        })
    }

    async fn power(
        &self,
        vm: &VmRef,
        action: PowerAction,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("power").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        busy(v)?;
        if v.suspended
            && matches!(
                action,
                PowerAction::Start | PowerAction::Reboot | PowerAction::Reset
            )
        {
            return Err(IntegrationError::Conflict(format!(
                "VM {} is suspended",
                v.id
            )));
        }
        let running = matches!(v.status, VmStatus::Running | VmStatus::Paused);
        let (transient, target) = match action {
            PowerAction::Start if running => {
                return Err(IntegrationError::Conflict(format!(
                    "VM {} is already running",
                    v.id
                )));
            }
            PowerAction::Start => (VmStatus::Starting, VmStatus::Running),
            _ if !running => {
                return Err(IntegrationError::Conflict(format!(
                    "VM {} is not running",
                    v.id
                )));
            }
            PowerAction::Stop | PowerAction::Shutdown => (VmStatus::Stopping, VmStatus::Stopped),
            PowerAction::Reboot | PowerAction::Reset => (VmStatus::Starting, VmStatus::Running),
        };
        if self.opts.async_tasks {
            v.status = transient;
        }
        let id = v.id.clone();
        Ok(self.finish(
            &mut state,
            Completion::SetStatus {
                vm: id,
                status: target,
            },
        ))
    }

    async fn resize(
        &self,
        vm: &VmRef,
        resize: &VmResize,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("resize").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        busy(v)?;
        if let Some(disk) = resize.disk_gb
            && disk < v.shape.disk_gb
        {
            return Err(IntegrationError::InvalidInput(format!(
                "disks can only grow (current {} GiB)",
                v.shape.disk_gb
            )));
        }
        if resize.cpu_cores == Some(0) {
            return Err(IntegrationError::InvalidInput(
                "cpu_cores must be at least 1".into(),
            ));
        }
        let changes_compute = resize.cpu_cores.is_some_and(|c| c != v.shape.cpu)
            || resize.memory_mb.is_some_and(|m| m != v.shape.memory_mb)
            || resize
                .plan
                .as_ref()
                .is_some_and(|p| Some(p) != v.plan.as_ref());
        let running = v.status == VmStatus::Running;
        if changes_compute && running && !resize.allow_restart {
            return Err(IntegrationError::Conflict(
                "CPU/memory changes need a restart; stop the VM or set allow_restart".into(),
            ));
        }
        if let Some(c) = resize.cpu_cores {
            v.shape.cpu = c;
        }
        if let Some(m) = resize.memory_mb {
            v.shape.memory_mb = m;
        }
        if let Some(d) = resize.disk_gb {
            v.shape.disk_gb = d;
        }
        if resize.plan.is_some() {
            v.plan.clone_from(&resize.plan);
        }
        let id = v.id.clone();
        if changes_compute && running {
            if self.opts.async_tasks {
                v.status = VmStatus::Starting;
            }
            return Ok(self.finish(
                &mut state,
                Completion::SetStatus {
                    vm: id,
                    status: VmStatus::Running,
                },
            ));
        }
        Ok(self.finish(&mut state, Completion::Nothing))
    }

    async fn reinstall(
        &self,
        vm: &VmRef,
        request: &VmReinstall,
    ) -> Result<VmProvisioned, IntegrationError> {
        self.enter("reinstall").await?;
        self.adopt(vm);
        let tpl = template(&request.template)?;
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        busy(v)?;
        if v.shape.disk_gb < tpl.min_disk_gb {
            return Err(IntegrationError::InvalidInput(format!(
                "template {} needs at least {} GiB",
                tpl.id, tpl.min_disk_gb
            )));
        }
        v.shape.template = tpl.id.into();
        if let Some(h) = request.hostname.as_ref().filter(|h| !h.trim().is_empty()) {
            v.hostname.clone_from(h);
        }
        v.status = VmStatus::Provisioning;
        v.started_at = None;
        let out_vm = VmRef::on_node(v.node.clone(), v.id.clone());
        let (ipv4, ipv6) = (v.ipv4.clone(), v.ipv6.clone());
        let generated = (request.root_password.is_none() && request.ssh_keys.is_empty())
            .then(|| generate_password(16));
        let task = self.finish(
            &mut state,
            Completion::SetStatus {
                vm: out_vm.id.clone(),
                status: VmStatus::Running,
            },
        );
        Ok(VmProvisioned {
            vm: out_vm,
            task,
            root_password: generated,
            ipv4,
            ipv6,
            extra: json!({"simulated": true}),
        })
    }

    async fn delete_vm(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("delete_vm").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        if v.status == VmStatus::Provisioning {
            return Err(IntegrationError::Conflict(format!(
                "VM {} is still being provisioned",
                v.id
            )));
        }
        if self.opts.async_tasks {
            v.status = VmStatus::Stopping;
        }
        let id = v.id.clone();
        Ok(self.finish(&mut state, Completion::Delete { vm: id }))
    }

    async fn list_snapshots(&self, vm: &VmRef) -> Result<Vec<SnapshotInfo>, IntegrationError> {
        self.enter("list_snapshots").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        Ok(v.snapshots
            .iter()
            .map(|s| SnapshotInfo {
                id: s.id.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                created_at: Some(s.created_at),
                size_mb: Some(s.shape.disk_gb * 64),
                includes_memory: s.includes_memory,
                parent: s.parent.clone(),
            })
            .collect())
    }

    async fn create_snapshot(
        &self,
        vm: &VmRef,
        request: &SnapshotRequest,
    ) -> Result<SnapshotCreated, IntegrationError> {
        self.enter("create_snapshot").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        busy(v)?;
        let id = snapshot_name(&request.name);
        if v.snapshots.iter().any(|s| s.id == id) {
            return Err(IntegrationError::Conflict(format!(
                "snapshot {id} already exists"
            )));
        }
        if v.snapshots.len() >= 10 {
            return Err(IntegrationError::Conflict(
                "snapshot limit (10) reached".into(),
            ));
        }
        let snapshot = MockSnapshot {
            id: id.clone(),
            name: request.name.clone(),
            description: request.description.clone(),
            created_at: Utc::now(),
            includes_memory: request.include_memory && v.status == VmStatus::Running,
            parent: v.current_snapshot.clone(),
            shape: v.shape.clone(),
            was_running: v.status == VmStatus::Running,
        };
        v.snapshots.push(snapshot);
        v.current_snapshot = Some(id.clone());
        let task = self.finish(&mut state, Completion::Nothing);
        Ok(SnapshotCreated {
            snapshot_id: id,
            task,
        })
    }

    async fn rollback_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("rollback_snapshot").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        busy(v)?;
        let snap = v
            .snapshots
            .iter()
            .find(|s| s.id == snapshot_id)
            .cloned()
            .ok_or_else(|| IntegrationError::NotFound(format!("snapshot {snapshot_id}")))?;
        v.shape = snap.shape.clone();
        v.current_snapshot = Some(snap.id.clone());
        // With RAM state the VM resumes running; otherwise it is powered off.
        let target = if snap.includes_memory && snap.was_running {
            VmStatus::Running
        } else {
            VmStatus::Stopped
        };
        let id = v.id.clone();
        Ok(self.finish(
            &mut state,
            Completion::SetStatus {
                vm: id,
                status: target,
            },
        ))
    }

    async fn delete_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("delete_snapshot").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        let pos = v
            .snapshots
            .iter()
            .position(|s| s.id == snapshot_id)
            .ok_or_else(|| IntegrationError::NotFound(format!("snapshot {snapshot_id}")))?;
        let removed = v.snapshots.remove(pos);
        for s in &mut v.snapshots {
            if s.parent.as_deref() == Some(snapshot_id) {
                s.parent.clone_from(&removed.parent);
            }
        }
        if v.current_snapshot.as_deref() == Some(snapshot_id) {
            v.current_snapshot.clone_from(&removed.parent);
        }
        Ok(self.finish(&mut state, Completion::Nothing))
    }

    async fn console(&self, vm: &VmRef) -> Result<ConsoleAccess, IntegrationError> {
        self.enter("console").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        if v.status != VmStatus::Running {
            return Err(IntegrationError::Conflict(format!(
                "VM {} is not running",
                v.id
            )));
        }
        Ok(ConsoleAccess {
            kind: ConsoleKind::NoVnc,
            url: format!("wss://console.mock.invalid/vnc/{}", v.id),
            ticket: Some(generate_password(32)),
            password: Some(generate_password(8)),
            port: Some(443),
            expires_at: Some(Utc::now() + chrono::Duration::seconds(60)),
            extra: json!({"simulated": true}),
        })
    }

    async fn metrics(&self, vm: &VmRef) -> Result<VmMetrics, IntegrationError> {
        self.enter("metrics").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        let running = v.status == VmStatus::Running;
        let w = |salt| wobble(&v.id, salt);
        let disk_used = v.shape.disk_gb as f64 * (0.15 + 0.3 * w(3));
        Ok(VmMetrics {
            collected_at: Utc::now(),
            status: if v.suspended {
                VmStatus::Suspended
            } else {
                v.status
            },
            cpu_usage_percent: Some(if running { 2.0 + 60.0 * w(1) } else { 0.0 }),
            memory_used_mb: Some(if running {
                (v.shape.memory_mb as f64 * (0.2 + 0.6 * w(2))) as u64
            } else {
                0
            }),
            memory_total_mb: Some(v.shape.memory_mb),
            disk_used_gb: Some(disk_used),
            disk_total_gb: Some(v.shape.disk_gb as f64),
            disk_read_bps: Some(if running { 50_000.0 * w(4) } else { 0.0 }),
            disk_write_bps: Some(if running { 80_000.0 * w(5) } else { 0.0 }),
            net_in_bps: Some(if running { 120_000.0 * w(6) } else { 0.0 }),
            net_out_bps: Some(if running { 40_000.0 * w(7) } else { 0.0 }),
            bandwidth_used_gb: Some(25.0 * w(8)),
            uptime_secs: uptime(v),
        })
    }

    async fn task_status(&self, task: &TaskRef) -> Result<TaskState, IntegrationError> {
        self.enter("task_status").await?;
        let mut state = self.lock();
        let t = state
            .tasks
            .get_mut(&task.id)
            .ok_or_else(|| IntegrationError::NotFound(format!("task {}", task.id)))?;
        if let Some(message) = &t.failed {
            return Ok(TaskState::Failed {
                message: message.clone(),
            });
        }
        if t.polls_left > 0 {
            t.polls_left -= 1;
            let total = self.opts.task_polls.max(1);
            let done = total - t.polls_left;
            if t.polls_left > 0 {
                let progress = u8::try_from(done * 100 / total).unwrap_or(99);
                return Ok(TaskState::Running {
                    progress_percent: Some(progress),
                });
            }
            let completion = t.completion.clone();
            t.completion = Completion::Nothing;
            apply(&mut state, &completion);
        }
        Ok(TaskState::Succeeded)
    }

    async fn suspend(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("suspend").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        busy(v)?;
        if v.suspended {
            return Err(IntegrationError::Conflict(format!(
                "VM {} is already suspended",
                v.id
            )));
        }
        v.suspended = true;
        v.status = VmStatus::Stopped;
        v.started_at = None;
        Ok(None)
    }

    async fn unsuspend(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("unsuspend").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        if !v.suspended {
            return Err(IntegrationError::Conflict(format!(
                "VM {} is not suspended",
                v.id
            )));
        }
        v.suspended = false;
        let id = v.id.clone();
        Ok(self.finish(
            &mut state,
            Completion::SetStatus {
                vm: id,
                status: VmStatus::Running,
            },
        ))
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

    /// An in-process simulated VNC server ([`crate::console::sim`]).
    async fn console_upstream(&self, vm: &VmRef) -> Result<ConsoleUpstream, IntegrationError> {
        self.enter("console_upstream").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        if v.status != VmStatus::Running {
            return Err(IntegrationError::Conflict(format!(
                "VM {} is not running",
                v.id
            )));
        }
        Ok(ConsoleUpstream {
            transport: ConsoleTransport::Simulated,
            url: format!("{} ({})", v.name, v.hostname),
            headers: Vec::new(),
            subprotocols: Vec::new(),
            accept_invalid_certs: false,
            vnc_password: Some(generate_password(8)),
            expires_at: Some(Utc::now() + chrono::Duration::seconds(30)),
        })
    }

    async fn reset_password(
        &self,
        vm: &VmRef,
        request: &PasswordResetRequest,
    ) -> Result<PasswordReset, IntegrationError> {
        self.enter("reset_password").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        if v.status != VmStatus::Running {
            return Err(IntegrationError::Conflict(
                "the server must be running to change its password".into(),
            ));
        }
        Ok(PasswordReset {
            username: request.username.clone().unwrap_or_else(|| "root".into()),
            password: request
                .password
                .clone()
                .unwrap_or_else(|| generate_password(20)),
            task: None,
        })
    }

    async fn metrics_history(
        &self,
        vm: &VmRef,
        range: MetricsRange,
    ) -> Result<MetricsHistory, IntegrationError> {
        self.enter("metrics_history").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        let step = range.step_secs();
        let end = Utc::now().timestamp() / i64::from(step) * i64::from(step);
        let count = range.duration().num_seconds() / i64::from(step);
        let mem = v.shape.memory_mb as f64;
        let samples = (0..count)
            .rev()
            .map(|i| {
                let t = end - i * i64::from(step);
                let w = |salt: u64| wave(&v.id, salt, t);
                MetricSample {
                    at: DateTime::from_timestamp(t, 0),
                    cpu_percent: Some(4.0 + 55.0 * w(1)),
                    memory_used_mb: Some(mem * (0.3 + 0.45 * w(2))),
                    memory_total_mb: Some(mem),
                    disk_read_bps: Some(40_000.0 + 400_000.0 * w(3)),
                    disk_write_bps: Some(60_000.0 + 900_000.0 * w(4)),
                    net_in_bps: Some(20_000.0 + 700_000.0 * w(5)),
                    net_out_bps: Some(15_000.0 + 1_600_000.0 * w(6)),
                }
            })
            .collect();
        Ok(MetricsHistory {
            range,
            step_secs: step,
            samples,
        })
    }

    async fn bandwidth_usage(&self, vm: &VmRef) -> Result<BandwidthUsage, IntegrationError> {
        self.enter("bandwidth_usage").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        let days = (Utc::now() - v.created_at).num_days().clamp(1, 31) as f64;
        let out = days * (3.0 + 4.0 * wobble(&v.id, 11));
        let inc = days * (1.0 + 2.0 * wobble(&v.id, 12));
        Ok(BandwidthUsage {
            outgoing_gb: Some(out),
            incoming_gb: Some(inc),
            counted_gb: out + inc,
            provider_limit_gb: None,
            estimated: false,
            period_start: None,
        })
    }

    /// Two daily backups per VM (yesterday, the day before).
    async fn list_backups(&self, vm: &VmRef) -> Result<Vec<BackupInfo>, IntegrationError> {
        self.enter("list_backups").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        Ok(mock_backups(v))
    }

    async fn restore_backup(
        &self,
        vm: &VmRef,
        backup_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("restore_backup").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        busy(v)?;
        if !mock_backups(v).iter().any(|b| b.id == backup_id) {
            return Err(IntegrationError::NotFound(format!(
                "backup {backup_id:?} of VM {}",
                v.id
            )));
        }
        v.status = VmStatus::Starting;
        let id = v.id.clone();
        let task = self.finish(
            &mut state,
            Completion::SetStatus {
                vm: id,
                status: VmStatus::Running,
            },
        );
        Ok(task)
    }

    async fn vm_extras(&self, vm: &VmRef) -> Result<VmExtras, IntegrationError> {
        self.enter("vm_extras").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        Ok(VmExtras {
            rescue_enabled: Some(v.extras.rescue),
            mounted_iso: v.extras.iso.clone(),
            boot_from_iso: Some(v.extras.boot_iso),
            reverse_dns: v
                .ipv4
                .iter()
                .chain(v.ipv6.iter())
                .map(|ip| ReverseDnsEntry {
                    ip: *ip,
                    ptr: v.extras.ptr.get(ip).cloned(),
                })
                .collect(),
        })
    }

    async fn set_reverse_dns(
        &self,
        vm: &VmRef,
        ip: IpAddr,
        ptr: Option<&str>,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("set_reverse_dns").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        if !v.ipv4.contains(&ip) && !v.ipv6.contains(&ip) {
            return Err(IntegrationError::InvalidInput(format!(
                "{ip} is not an address of VM {}",
                v.id
            )));
        }
        match ptr {
            Some(p) => {
                v.extras.ptr.insert(ip, p.to_owned());
            }
            None => {
                v.extras.ptr.remove(&ip);
            }
        }
        Ok(self.finish(&mut state, Completion::Nothing))
    }

    async fn list_isos(&self, vm: &VmRef) -> Result<Vec<IsoImage>, IntegrationError> {
        self.enter("list_isos").await?;
        self.adopt(vm);
        let mut state = self.lock();
        vm_mut(&mut state, vm)?;
        Ok(ISOS
            .iter()
            .map(|(id, name)| IsoImage {
                id: (*id).into(),
                name: (*name).into(),
                description: None,
                size_mb: Some(700),
            })
            .collect())
    }

    async fn mount_iso(
        &self,
        vm: &VmRef,
        iso_id: &str,
        boot_first: bool,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("mount_iso").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        if !ISOS.iter().any(|(id, _)| *id == iso_id) {
            return Err(IntegrationError::NotFound(format!("ISO {iso_id:?}")));
        }
        v.extras.iso = Some(iso_id.to_owned());
        v.extras.boot_iso = boot_first;
        Ok(self.finish(&mut state, Completion::Nothing))
    }

    async fn unmount_iso(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("unmount_iso").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        v.extras.iso = None;
        v.extras.boot_iso = false;
        Ok(self.finish(&mut state, Completion::Nothing))
    }

    async fn set_rescue(
        &self,
        vm: &VmRef,
        enable: bool,
        _ssh_keys: &[String],
    ) -> Result<RescueResult, IntegrationError> {
        self.enter("set_rescue").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        v.extras.rescue = enable;
        Ok(RescueResult {
            password: enable.then(|| generate_password(16)),
            task: self.finish(&mut state, Completion::Nothing),
        })
    }

    async fn firewall(&self, vm: &VmRef) -> Result<FirewallState, IntegrationError> {
        self.enter("firewall").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        Ok(v.extras.firewall.clone())
    }

    async fn set_firewall(
        &self,
        vm: &VmRef,
        fw: &FirewallState,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        self.enter("set_firewall").await?;
        self.adopt(vm);
        let mut state = self.lock();
        let v = vm_mut(&mut state, vm)?;
        v.extras.firewall = fw.clone();
        Ok(self.finish(&mut state, Completion::Nothing))
    }
}

fn mock_backups(v: &MockVm) -> Vec<BackupInfo> {
    let today = Utc::now().date_naive();
    (1..=2)
        .filter_map(|d| {
            let day = today - chrono::Days::new(d);
            let at = day.and_hms_opt(3, 0, 0)?.and_utc();
            (at > v.created_at).then(|| BackupInfo {
                id: format!("backup-{}-{}", v.id, day.format("%Y%m%d")),
                name: format!("daily {}", day.format("%Y-%m-%d")),
                created_at: Some(at),
                size_mb: Some(v.shape.disk_gb * 1024 / 5),
                notes: Some("automatic".into()),
            })
        })
        .collect()
}

/// Smooth deterministic curve in `[0, 1)` for synthetic history.
fn wave(id: &str, salt: u64, t: i64) -> f64 {
    let seed = (wobble_seed(id, salt) % 1000) as f64 / 1000.0;
    let x = t as f64 / 3600.0;
    let v = 0.5
        + 0.3 * (x * 0.9 + seed * std::f64::consts::TAU).sin()
        + 0.15 * (x * 3.1 + seed * 2.0).sin()
        + 0.05 * (x * 11.0).sin();
    v.clamp(0.0, 0.999)
}

fn wobble_seed(id: &str, salt: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ salt;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}
