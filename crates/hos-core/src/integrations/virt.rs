//! Virtualization providers: Proxmox VE, VMware vSphere/ESXi, Virtualizor, Hetzner Cloud.
//!
//! ## Identifiers
//!
//! A VM is addressed by [`VmRef`]: `node` is the hypervisor node where the
//! provider needs one (Proxmox node name, Virtualizor server id), `id` is the
//! provider's VM id (Proxmox `vmid`, vSphere `vm-123` MoRef, Virtualizor
//! `vpsid`, Hetzner server id). Store both in the `vms` table.
//!
//! ## Long-running operations
//!
//! Mutating calls return `Option<TaskRef>`: `None` means the operation already
//! finished; `Some(task)` means the provider accepted it asynchronously
//! (Proxmox UPID, vSphere task, Hetzner action) and the caller (a `hos-jobs`
//! job) polls [`VirtProvider::task_status`]. Implementations may also wait
//! internally (bounded by their HTTP timeout) and return `None`.

use std::net::IpAddr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ConnectionInfo;
use crate::error::IntegrationError;
use crate::secret::SecretString;

/// Reference to a VM at the provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VmRef {
    /// Node / host the VM lives on, when the provider needs it for addressing.
    pub node: Option<String>,
    /// Provider VM id.
    pub id: String,
}

impl VmRef {
    /// A VM addressed by id only.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            node: None,
            id: id.into(),
        }
    }

    /// A VM on a specific node.
    pub fn on_node(node: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            node: Some(node.into()),
            id: id.into(),
        }
    }
}

impl std::fmt::Display for VmRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.node {
            Some(node) => write!(f, "{node}/{}", self.id),
            None => f.write_str(&self.id),
        }
    }
}

/// Reference to an asynchronous provider task.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskRef {
    /// Provider task id (Proxmox UPID, vSphere task id, Hetzner action id).
    pub id: String,
    /// Node that runs the task, when relevant (Proxmox).
    pub node: Option<String>,
}

/// State of an asynchronous provider task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskState {
    /// Still running.
    Running {
        /// Progress 0–100 when reported.
        progress_percent: Option<u8>,
    },
    /// Finished successfully.
    Succeeded,
    /// Finished with an error.
    Failed {
        /// Provider error message.
        message: String,
    },
}

crate::string_enum! {
    /// Power operation.
    pub enum PowerAction {
        /// Power on.
        Start => "start",
        /// Hard power off.
        Stop => "stop",
        /// Graceful ACPI shutdown.
        Shutdown => "shutdown",
        /// Graceful reboot.
        Reboot => "reboot",
        /// Hard reset.
        Reset => "reset",
    }
}

crate::string_enum! {
    /// Normalised VM power / lifecycle state.
    pub enum VmStatus {
        /// Running.
        Running => "running",
        /// Powered off.
        Stopped => "stopped",
        /// Paused (memory kept).
        Paused => "paused",
        /// Suspended by the provider/admin (Virtualizor suspend).
        Suspended => "suspended",
        /// Booting.
        Starting => "starting",
        /// Shutting down.
        Stopping => "stopping",
        /// Being created / installed.
        Provisioning => "provisioning",
        /// Migrating between nodes.
        Migrating => "migrating",
        /// Error state.
        Error => "error",
        /// Anything the implementation cannot map.
        Unknown => "unknown",
    }
}

crate::string_enum! {
    /// Operating system family of a template.
    pub enum OsFamily {
        /// Linux distributions.
        Linux => "linux",
        /// Microsoft Windows.
        Windows => "windows",
        /// FreeBSD / OpenBSD / ...
        Bsd => "bsd",
        /// Other / unknown.
        Other => "other",
    }
}

crate::string_enum! {
    /// Remote console flavour.
    pub enum ConsoleKind {
        /// noVNC over websocket (Proxmox `vncproxy`, Virtualizor VNC).
        NoVnc => "novnc",
        /// SPICE (`remote-viewer` config in `ConsoleAccess::extra`).
        Spice => "spice",
        /// Serial / xterm.js terminal.
        Xterm => "xterm",
        /// VMware WebMKS ticket.
        WebMks => "webmks",
        /// A provider-hosted console page to open/redirect to (Hetzner `request_console` URL).
        Url => "url",
    }
}

/// A hypervisor node / location.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeInfo {
    /// Provider node id (Proxmox node name, vSphere host MoRef, Hetzner location name).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether the node is online / available.
    pub online: bool,
    /// Data centre / city.
    pub location: Option<String>,
    /// Physical/logical CPU count.
    pub cpu_cores: Option<u32>,
    /// CPU usage 0–100.
    pub cpu_usage_percent: Option<f64>,
    /// Total memory (MiB).
    pub memory_total_mb: Option<u64>,
    /// Used memory (MiB).
    pub memory_used_mb: Option<u64>,
    /// Total storage (GiB).
    pub disk_total_gb: Option<u64>,
    /// Used storage (GiB).
    pub disk_used_gb: Option<u64>,
    /// Number of VMs on the node.
    pub vm_count: Option<u32>,
    /// Provider-specific data.
    pub extra: Value,
}

/// An OS template / image usable for create and reinstall.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateInfo {
    /// Provider template id (Proxmox template vmid or volid, vSphere template MoRef /
    /// content library item, Virtualizor `osid`, Hetzner image name/id).
    pub id: String,
    /// Display name (`"Ubuntu 24.04"`).
    pub name: String,
    /// Node the template is on, when node-local.
    pub node: Option<String>,
    /// OS family.
    pub os_family: Option<OsFamily>,
    /// Minimum disk size (GiB).
    pub min_disk_gb: Option<u64>,
    /// Whether cloud-init user data / SSH keys are honoured.
    pub cloud_init: bool,
    /// Provider-specific data.
    pub extra: Value,
}

/// IP configuration of one address family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum IpConfig {
    /// DHCP / SLAAC inside the guest.
    Dhcp,
    /// Let the provider allocate from its pool (Hetzner public IP, Virtualizor IP pool).
    Auto,
    /// A specific address (from `ip_pools` in HostingOS).
    Static(StaticIp),
}

/// A static IP assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticIp {
    /// Address.
    pub address: IpAddr,
    /// Prefix length (24 for a /24, 64 for a /64).
    pub prefix_len: u8,
    /// Default gateway.
    pub gateway: Option<IpAddr>,
}

/// Network configuration of a new VM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSpec {
    /// Bridge / port group / network name (`vmbr0`, `VM Network`, Hetzner network id).
    pub bridge: Option<String>,
    /// VLAN tag.
    pub vlan: Option<u16>,
    /// IPv4 configuration; `None` = no IPv4.
    pub ipv4: Option<IpConfig>,
    /// IPv6 configuration; `None` = no IPv6.
    pub ipv6: Option<IpConfig>,
    /// DNS resolvers.
    pub dns_servers: Vec<IpAddr>,
    /// Fixed MAC address.
    pub mac_address: Option<String>,
    /// Port speed limit (Mbit/s).
    pub rate_limit_mbps: Option<u32>,
}

/// Everything needed to create a VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VmSpec {
    /// Provider-side VM name (unique-ish label, e.g. `svc-<service id>`).
    pub name: String,
    /// Guest hostname (FQDN).
    pub hostname: String,
    /// Target node; `None` lets the implementation choose (least loaded / only node).
    pub node: Option<String>,
    /// Location / data centre (Hetzner `fsn1`, vSphere datacenter or cluster).
    pub location: Option<String>,
    /// Template / image id ([`TemplateInfo::id`]).
    pub template: String,
    /// Fixed-size plan where the provider sells sizes (Hetzner server type `cx22`,
    /// Virtualizor plan id). When set, `cpu_cores`/`memory_mb`/`disk_gb` are informative.
    pub plan: Option<String>,
    /// vCPU count.
    pub cpu_cores: u32,
    /// Memory (MiB).
    pub memory_mb: u64,
    /// Primary disk (GiB).
    pub disk_gb: u64,
    /// Storage / datastore / pool for the disk.
    pub storage: Option<String>,
    /// Monthly traffic allowance (GiB), where the provider enforces it.
    pub bandwidth_gb: Option<u64>,
    /// Network configuration.
    pub network: NetworkSpec,
    /// Authorized SSH public keys (OpenSSH format).
    pub ssh_keys: Vec<String>,
    /// Root / Administrator password; `None` = provider generates or key-only.
    pub root_password: Option<SecretString>,
    /// Raw cloud-init user data (`#cloud-config ...`).
    pub cloud_init_user_data: Option<String>,
    /// Power on after creation (default `true`).
    pub start_after_create: bool,
    /// Labels/tags to attach at the provider.
    pub tags: Vec<String>,
    /// Provider-specific options (Proxmox `cpu` type, vSphere folder, Hetzner `firewalls`...).
    pub extra: Value,
}

impl Default for VmSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            hostname: String::new(),
            node: None,
            location: None,
            template: String::new(),
            plan: None,
            cpu_cores: 1,
            memory_mb: 1024,
            disk_gb: 20,
            storage: None,
            bandwidth_gb: None,
            network: NetworkSpec::default(),
            ssh_keys: Vec::new(),
            root_password: None,
            cloud_init_user_data: None,
            start_after_create: true,
            tags: Vec::new(),
            extra: Value::Null,
        }
    }
}

/// Result of [`VirtProvider::create_vm`] and [`VirtProvider::reinstall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmProvisioned {
    /// The VM.
    pub vm: VmRef,
    /// Pending task, if the operation is still running.
    pub task: Option<TaskRef>,
    /// Password generated by the provider (Hetzner returns one when no SSH key is given).
    pub root_password: Option<SecretString>,
    /// Assigned IPv4 addresses, if already known.
    #[serde(default)]
    pub ipv4: Vec<IpAddr>,
    /// Assigned IPv6 addresses, if already known.
    #[serde(default)]
    pub ipv6: Vec<IpAddr>,
    /// Provider-specific data.
    #[serde(default)]
    pub extra: Value,
}

/// Current state of a VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmInfo {
    /// The VM.
    pub vm: VmRef,
    /// Provider-side name.
    pub name: String,
    /// Normalised status.
    pub status: VmStatus,
    /// Guest hostname, when known.
    pub hostname: Option<String>,
    /// vCPUs.
    pub cpu_cores: Option<u32>,
    /// Memory (MiB).
    pub memory_mb: Option<u64>,
    /// Primary disk (GiB).
    pub disk_gb: Option<u64>,
    /// IPv4 addresses.
    #[serde(default)]
    pub ipv4: Vec<IpAddr>,
    /// IPv6 addresses.
    #[serde(default)]
    pub ipv6: Vec<IpAddr>,
    /// Template / image it was built from.
    pub template: Option<String>,
    /// Plan / server type.
    pub plan: Option<String>,
    /// Provider tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation time at the provider.
    pub created_at: Option<DateTime<Utc>>,
    /// Uptime in seconds.
    pub uptime_secs: Option<u64>,
    /// Provider-specific data.
    #[serde(default)]
    pub extra: Value,
}

/// Resize request; `None` fields stay unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VmResize {
    /// New vCPU count.
    pub cpu_cores: Option<u32>,
    /// New memory (MiB).
    pub memory_mb: Option<u64>,
    /// New primary disk size (GiB); disks can only grow.
    pub disk_gb: Option<u64>,
    /// New plan / server type (Hetzner `change_type`, Virtualizor plan).
    pub plan: Option<String>,
    /// Allow the implementation to stop/restart the VM if the change needs it.
    pub allow_restart: bool,
}

/// Reinstall (rebuild) request. Destroys guest data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VmReinstall {
    /// Template / image id.
    pub template: String,
    /// New hostname (keep current when `None`).
    pub hostname: Option<String>,
    /// New root password (provider-generated when `None` and no keys).
    pub root_password: Option<SecretString>,
    /// Authorized SSH keys.
    pub ssh_keys: Vec<String>,
    /// cloud-init user data.
    pub cloud_init_user_data: Option<String>,
    /// Provider-specific options.
    pub extra: Value,
}

/// Snapshot creation request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotRequest {
    /// Snapshot name (Proxmox requires `[A-Za-z][A-Za-z0-9_-]*`; implementations sanitise).
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Include RAM state (Proxmox `vmstate`, vSphere `memory`).
    pub include_memory: bool,
}

/// An existing snapshot / backup image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotInfo {
    /// Provider snapshot id (Proxmox snapname, vSphere snapshot MoRef, Hetzner image id).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Creation time.
    pub created_at: Option<DateTime<Utc>>,
    /// Size (MiB), when reported.
    pub size_mb: Option<u64>,
    /// Whether RAM state is included.
    pub includes_memory: bool,
    /// Parent snapshot id (snapshot trees).
    pub parent: Option<String>,
}

/// Result of [`VirtProvider::create_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCreated {
    /// Id of the new snapshot.
    pub snapshot_id: String,
    /// Pending task, if still running.
    pub task: Option<TaskRef>,
}

/// Credentials for an in-browser console session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleAccess {
    /// Console flavour.
    pub kind: ConsoleKind,
    /// URL to open (websocket URL for noVNC/xterm/WebMKS, page URL for `Url`).
    pub url: String,
    /// Session ticket / token (Proxmox `vncticket`, vSphere WebMKS ticket).
    pub ticket: Option<SecretString>,
    /// VNC password, when separate from the ticket.
    pub password: Option<SecretString>,
    /// Port, when not part of `url`.
    pub port: Option<u16>,
    /// When the ticket stops working.
    pub expires_at: Option<DateTime<Utc>>,
    /// Provider-specific data (e.g. full SPICE `.vv` settings).
    #[serde(default)]
    pub extra: Value,
}

/// Point-in-time resource usage of a VM. Rates are averaged over the provider's
/// most recent sample window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmMetrics {
    /// Sample time.
    pub collected_at: DateTime<Utc>,
    /// Status at sample time.
    pub status: VmStatus,
    /// CPU usage 0–100 (of all assigned vCPUs).
    pub cpu_usage_percent: Option<f64>,
    /// Used memory (MiB).
    pub memory_used_mb: Option<u64>,
    /// Assigned memory (MiB).
    pub memory_total_mb: Option<u64>,
    /// Used disk (GiB).
    pub disk_used_gb: Option<f64>,
    /// Disk size (GiB).
    pub disk_total_gb: Option<f64>,
    /// Disk read (bytes/s).
    pub disk_read_bps: Option<f64>,
    /// Disk write (bytes/s).
    pub disk_write_bps: Option<f64>,
    /// Inbound network (bytes/s).
    pub net_in_bps: Option<f64>,
    /// Outbound network (bytes/s).
    pub net_out_bps: Option<f64>,
    /// Traffic used in the current billing period (GiB).
    pub bandwidth_used_gb: Option<f64>,
    /// Uptime in seconds.
    pub uptime_secs: Option<u64>,
}

/// Feature flags of a virtualization provider (drives which buttons the UI shows).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VirtCapabilities {
    /// Snapshots supported.
    pub snapshots: bool,
    /// Snapshots can include RAM state.
    pub memory_snapshots: bool,
    /// CPU/RAM can change without a restart.
    pub live_resize: bool,
    /// Reinstall / rebuild supported.
    pub reinstall: bool,
    /// Supported console kinds (empty = no console).
    pub console_kinds: Vec<ConsoleKind>,
    /// [`VirtProvider::metrics`] supported.
    pub metrics: bool,
    /// The provider has multiple nodes to choose from.
    pub multi_node: bool,
    /// Sizes come from fixed plans ([`VmSpec::plan`]) rather than free cpu/ram/disk.
    pub plan_based: bool,
    /// Operations may return [`TaskRef`]s that need polling.
    pub async_tasks: bool,
}

// ---------------------------------------------------------------------------
// Extended VM management (console relay, password reset, backups, rDNS, ISO,
// rescue, firewall, metrics history, bandwidth). Every method below has a
// default implementation returning `Unsupported`, so a provider only
// implements what its API can do; [`VirtFeatures`] tells the UI up front.
// ---------------------------------------------------------------------------

/// Which extended operations a provider implements (drives the UI: a `false`
/// feature is shown as "not supported by this provider", never as an error).
/// A `true` feature can still be refused for one VM (e.g. no guest agent,
/// LXC container) with [`IntegrationError::Unsupported`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VirtFeatures {
    /// [`VirtProvider::console_upstream`]: a VNC upstream HostingOS can relay
    /// (the browser never receives hypervisor credentials).
    pub console_relay: bool,
    /// [`VirtProvider::reset_password`].
    pub password_reset: bool,
    /// [`VirtProvider::list_backups`].
    pub backups: bool,
    /// [`VirtProvider::restore_backup`].
    pub backup_restore: bool,
    /// [`VirtProvider::set_reverse_dns`] at the provider.
    pub reverse_dns: bool,
    /// [`VirtProvider::list_isos`] / [`VirtProvider::mount_iso`] / [`VirtProvider::unmount_iso`].
    pub iso: bool,
    /// [`VirtProvider::set_rescue`].
    pub rescue: bool,
    /// [`VirtProvider::firewall`] / [`VirtProvider::set_firewall`].
    pub firewall: bool,
    /// [`VirtProvider::metrics_history`].
    pub metrics_history: bool,
    /// [`VirtProvider::bandwidth_usage`].
    pub bandwidth: bool,
}

crate::string_enum! {
    /// How the console relay reaches the VNC server.
    pub enum ConsoleTransport {
        /// RFB over a websocket (`wss://…`; Proxmox `vncwebsocket`, Hetzner `wss_url`).
        WebSocket => "websocket",
        /// RFB over plain TCP (`host:port`; Virtualizor VNC).
        Tcp => "tcp",
        /// In-process simulated VNC server (the `mock` provider only).
        Simulated => "simulated",
    }
}

/// Where and how a console relay connects upstream. Built from the stored
/// integration (and the provider's answer), never from browser input. The
/// secrets in here stay on the server: the relay authenticates to the VNC
/// server itself (RFB VNC authentication with [`Self::vnc_password`]) and
/// offers the browser an already-authenticated stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleUpstream {
    /// Transport.
    pub transport: ConsoleTransport,
    /// `wss://…` / `ws://…` for [`ConsoleTransport::WebSocket`], `host:port`
    /// for [`ConsoleTransport::Tcp`], a label for `Simulated`.
    pub url: String,
    /// Extra request headers of the websocket upgrade (e.g. the API token).
    #[serde(default)]
    pub headers: Vec<(String, SecretString)>,
    /// Websocket subprotocols to offer (`binary` for Proxmox).
    #[serde(default)]
    pub subprotocols: Vec<String>,
    /// Skip TLS verification (mirrors the integration's `skip_tls_verify`).
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// VNC password for RFB "VNC authentication" (`None`: the server must
    /// offer security type "None").
    pub vnc_password: Option<SecretString>,
    /// The upstream must be opened before this instant (Hetzner: 1 minute).
    pub expires_at: Option<DateTime<Utc>>,
}

/// Password reset request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PasswordResetRequest {
    /// Guest user (default `root`; Windows: `Administrator`).
    pub username: Option<String>,
    /// Password generated by HostingOS. Providers that generate their own
    /// (Hetzner) ignore it and return theirs.
    pub password: Option<SecretString>,
}

/// Result of [`VirtProvider::reset_password`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PasswordReset {
    /// Guest user whose password changed.
    pub username: String,
    /// The new password (active once `task`, if any, succeeds).
    pub password: SecretString,
    /// Pending task.
    pub task: Option<TaskRef>,
}

crate::string_enum! {
    /// Time window of [`VirtProvider::metrics_history`].
    pub enum MetricsRange {
        /// Last hour.
        Hour => "hour",
        /// Last 24 hours.
        Day => "day",
        /// Last 7 days.
        Week => "week",
        /// Last 30 days.
        Month => "month",
    }
}

impl MetricsRange {
    /// Length of the window.
    pub fn duration(self) -> chrono::Duration {
        match self {
            Self::Hour => chrono::Duration::hours(1),
            Self::Day => chrono::Duration::days(1),
            Self::Week => chrono::Duration::days(7),
            Self::Month => chrono::Duration::days(30),
        }
    }

    /// Preferred sample spacing (seconds) for roughly 60–200 samples.
    pub fn step_secs(self) -> u32 {
        match self {
            Self::Hour => 60,
            Self::Day => 600,
            Self::Week => 3600,
            Self::Month => 3 * 3600,
        }
    }
}

/// One sample of [`MetricsHistory`]. Rates are averages over the sample step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricSample {
    /// Sample time.
    pub at: Option<DateTime<Utc>>,
    /// CPU 0–100 of all vCPUs.
    pub cpu_percent: Option<f64>,
    /// Used memory (MiB).
    pub memory_used_mb: Option<f64>,
    /// Assigned memory (MiB).
    pub memory_total_mb: Option<f64>,
    /// Disk read (bytes/s).
    pub disk_read_bps: Option<f64>,
    /// Disk write (bytes/s).
    pub disk_write_bps: Option<f64>,
    /// Inbound network (bytes/s).
    pub net_in_bps: Option<f64>,
    /// Outbound network (bytes/s).
    pub net_out_bps: Option<f64>,
}

/// Time series from the provider's metrics endpoint, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsHistory {
    /// Requested window.
    pub range: MetricsRange,
    /// Sample spacing the provider used (seconds).
    pub step_secs: u32,
    /// Samples, oldest first.
    pub samples: Vec<MetricSample>,
}

/// Traffic of the current billing period as reported by the provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BandwidthUsage {
    /// Outbound traffic (GiB).
    pub outgoing_gb: Option<f64>,
    /// Inbound traffic (GiB).
    pub incoming_gb: Option<f64>,
    /// Traffic counted against the allowance (GiB).
    pub counted_gb: f64,
    /// Allowance at the provider (GiB), when it has one.
    pub provider_limit_gb: Option<f64>,
    /// Derived from sampled rates rather than a provider counter.
    pub estimated: bool,
    /// Start of the counting period, when known.
    pub period_start: Option<DateTime<Utc>>,
}

/// A provider-side backup of a VM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupInfo {
    /// Provider backup id (Proxmox volid, Hetzner image id).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Creation time.
    pub created_at: Option<DateTime<Utc>>,
    /// Size (MiB).
    pub size_mb: Option<u64>,
    /// Provider notes / description.
    pub notes: Option<String>,
}

/// An installable / bootable ISO image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IsoImage {
    /// Provider id (Proxmox volid, Hetzner ISO id or name).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Size (MiB).
    pub size_mb: Option<u64>,
}

/// Reverse DNS entry of one of the VM's addresses at the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseDnsEntry {
    /// Address.
    pub ip: IpAddr,
    /// PTR target (`None` = provider default / none).
    pub ptr: Option<String>,
}

/// Provider-side state that does not fit [`VmInfo`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VmExtras {
    /// Rescue mode armed for the next boot.
    pub rescue_enabled: Option<bool>,
    /// Mounted ISO (id).
    pub mounted_iso: Option<String>,
    /// Boot from the ISO first.
    pub boot_from_iso: Option<bool>,
    /// PTR records the provider manages.
    pub reverse_dns: Vec<ReverseDnsEntry>,
}

/// Result of [`VirtProvider::set_rescue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RescueResult {
    /// Rescue system root password (Hetzner).
    pub password: Option<SecretString>,
    /// Pending task.
    pub task: Option<TaskRef>,
}

crate::string_enum! {
    /// Firewall rule direction.
    pub enum FirewallDirection {
        /// Inbound.
        In => "in",
        /// Outbound.
        Out => "out",
    }
}

crate::string_enum! {
    /// Firewall rule protocol.
    pub enum FirewallProtocol {
        /// TCP.
        Tcp => "tcp",
        /// UDP.
        Udp => "udp",
        /// ICMP (v4 and v6).
        Icmp => "icmp",
    }
}

/// One allow rule (traffic not matched by a rule falls to the policy: inbound
/// dropped, outbound allowed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallRule {
    /// Direction.
    pub direction: FirewallDirection,
    /// Protocol.
    pub protocol: FirewallProtocol,
    /// Port or `low-high` range (TCP/UDP only).
    pub port: Option<String>,
    /// Remote networks in CIDR notation (`0.0.0.0/0`, `::/0` = any).
    pub cidrs: Vec<String>,
    /// Description.
    pub comment: Option<String>,
}

/// The VM's firewall as HostingOS manages it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FirewallState {
    /// Filtering on (inbound policy drop).
    pub enabled: bool,
    /// Allow rules.
    pub rules: Vec<FirewallRule>,
}

/// A virtualization platform. See the [module docs](self).
#[async_trait]
pub trait VirtProvider: Send + Sync {
    /// The integration kind (e.g. [`kinds::PROXMOX`](super::kinds::PROXMOX)).
    fn kind(&self) -> &'static str;

    /// Feature flags.
    fn capabilities(&self) -> VirtCapabilities;

    /// Verifies credentials and reachability.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError>;

    /// Lists nodes / locations.
    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, IntegrationError>;

    /// Lists OS templates, optionally only those available on `node`.
    async fn list_templates(
        &self,
        node: Option<&str>,
    ) -> Result<Vec<TemplateInfo>, IntegrationError>;

    /// Lists VMs, optionally only on `node`.
    async fn list_vms(&self, node: Option<&str>) -> Result<Vec<VmInfo>, IntegrationError>;

    /// Fetches one VM. Missing → [`IntegrationError::NotFound`].
    async fn get_vm(&self, vm: &VmRef) -> Result<VmInfo, IntegrationError>;

    /// Creates (and by default starts) a VM.
    async fn create_vm(&self, spec: &VmSpec) -> Result<VmProvisioned, IntegrationError>;

    /// Runs a power action.
    async fn power(
        &self,
        vm: &VmRef,
        action: PowerAction,
    ) -> Result<Option<TaskRef>, IntegrationError>;

    /// Changes resources.
    async fn resize(
        &self,
        vm: &VmRef,
        resize: &VmResize,
    ) -> Result<Option<TaskRef>, IntegrationError>;

    /// Reinstalls the OS (destroys data).
    async fn reinstall(
        &self,
        vm: &VmRef,
        request: &VmReinstall,
    ) -> Result<VmProvisioned, IntegrationError>;

    /// Deletes the VM and its disks (stopping it first if needed).
    async fn delete_vm(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError>;

    /// Lists snapshots.
    async fn list_snapshots(&self, vm: &VmRef) -> Result<Vec<SnapshotInfo>, IntegrationError>;

    /// Creates a snapshot.
    async fn create_snapshot(
        &self,
        vm: &VmRef,
        request: &SnapshotRequest,
    ) -> Result<SnapshotCreated, IntegrationError>;

    /// Rolls the VM back to a snapshot.
    async fn rollback_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError>;

    /// Deletes a snapshot.
    async fn delete_snapshot(
        &self,
        vm: &VmRef,
        snapshot_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError>;

    /// Opens a console session.
    async fn console(&self, vm: &VmRef) -> Result<ConsoleAccess, IntegrationError>;

    /// Current resource usage.
    async fn metrics(&self, vm: &VmRef) -> Result<VmMetrics, IntegrationError>;

    /// Polls an asynchronous task. Default: `Unsupported` (for providers that
    /// never return [`TaskRef`]s).
    async fn task_status(&self, task: &TaskRef) -> Result<TaskState, IntegrationError> {
        let _ = task;
        Err(IntegrationError::Unsupported("task_status"))
    }

    /// Billing suspension. Default: hard power-off ([`PowerAction::Stop`]).
    /// Providers with a native suspend (Virtualizor) override this.
    async fn suspend(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        self.power(vm, PowerAction::Stop).await
    }

    /// Lifts a billing suspension. Default: [`PowerAction::Start`].
    async fn unsuspend(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        self.power(vm, PowerAction::Start).await
    }

    // -- extended operations (default: Unsupported) -------------------------

    /// Extended feature flags. Default: none.
    fn features(&self) -> VirtFeatures {
        VirtFeatures::default()
    }

    /// Prepares a console session HostingOS can relay (the VNC server, how to
    /// reach it and how to authenticate). Must be opened promptly.
    async fn console_upstream(&self, vm: &VmRef) -> Result<ConsoleUpstream, IntegrationError> {
        let _ = vm;
        Err(IntegrationError::Unsupported("console relay"))
    }

    /// Sets a new password for a guest user (root by default).
    async fn reset_password(
        &self,
        vm: &VmRef,
        request: &PasswordResetRequest,
    ) -> Result<PasswordReset, IntegrationError> {
        let _ = (vm, request);
        Err(IntegrationError::Unsupported("password reset"))
    }

    /// Resource usage history from the provider's metrics endpoint.
    async fn metrics_history(
        &self,
        vm: &VmRef,
        range: MetricsRange,
    ) -> Result<MetricsHistory, IntegrationError> {
        let _ = (vm, range);
        Err(IntegrationError::Unsupported("metrics history"))
    }

    /// Traffic of the current period.
    async fn bandwidth_usage(&self, vm: &VmRef) -> Result<BandwidthUsage, IntegrationError> {
        let _ = vm;
        Err(IntegrationError::Unsupported("bandwidth usage"))
    }

    /// Provider-side backups of this VM (only this VM's).
    async fn list_backups(&self, vm: &VmRef) -> Result<Vec<BackupInfo>, IntegrationError> {
        let _ = vm;
        Err(IntegrationError::Unsupported("backups"))
    }

    /// Restores a backup of this VM over the VM. Implementations must check
    /// that `backup_id` is a backup **of `vm`** (`NotFound` otherwise).
    async fn restore_backup(
        &self,
        vm: &VmRef,
        backup_id: &str,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let _ = (vm, backup_id);
        Err(IntegrationError::Unsupported("backup restore"))
    }

    /// Provider state that is not part of [`VmInfo`] (rescue, ISO, PTR).
    async fn vm_extras(&self, vm: &VmRef) -> Result<VmExtras, IntegrationError> {
        let _ = vm;
        Err(IntegrationError::Unsupported("vm extras"))
    }

    /// Sets (`Some`) or resets (`None`) the PTR record of one of the VM's
    /// own addresses at the provider (`InvalidInput` for any other address).
    async fn set_reverse_dns(
        &self,
        vm: &VmRef,
        ip: IpAddr,
        ptr: Option<&str>,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let _ = (vm, ip, ptr);
        Err(IntegrationError::Unsupported("reverse DNS"))
    }

    /// ISO images that could be attached to this VM.
    async fn list_isos(&self, vm: &VmRef) -> Result<Vec<IsoImage>, IntegrationError> {
        let _ = vm;
        Err(IntegrationError::Unsupported("ISO images"))
    }

    /// Attaches an ISO (from [`VirtProvider::list_isos`]); `boot_first` puts
    /// the CD-ROM first in the boot order where the provider allows choosing.
    async fn mount_iso(
        &self,
        vm: &VmRef,
        iso_id: &str,
        boot_first: bool,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let _ = (vm, iso_id, boot_first);
        Err(IntegrationError::Unsupported("ISO images"))
    }

    /// Detaches the ISO and restores booting from disk.
    async fn unmount_iso(&self, vm: &VmRef) -> Result<Option<TaskRef>, IntegrationError> {
        let _ = vm;
        Err(IntegrationError::Unsupported("ISO images"))
    }

    /// Arms (`enable`) or disarms rescue mode for the next boot.
    async fn set_rescue(
        &self,
        vm: &VmRef,
        enable: bool,
        ssh_keys: &[String],
    ) -> Result<RescueResult, IntegrationError> {
        let _ = (vm, enable, ssh_keys);
        Err(IntegrationError::Unsupported("rescue mode"))
    }

    /// The VM's firewall as managed by HostingOS.
    async fn firewall(&self, vm: &VmRef) -> Result<FirewallState, IntegrationError> {
        let _ = vm;
        Err(IntegrationError::Unsupported("firewall"))
    }

    /// Replaces the VM's firewall rules (and switches filtering on or off).
    async fn set_firewall(
        &self,
        vm: &VmRef,
        state: &FirewallState,
    ) -> Result<Option<TaskRef>, IntegrationError> {
        let _ = (vm, state);
        Err(IntegrationError::Unsupported("firewall"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vm_spec_serde_defaults() {
        let spec: VmSpec = serde_json::from_value(json!({
            "name": "svc-1", "hostname": "vps1.example.com", "template": "ubuntu-24.04",
            "network": {"ipv4": {"mode": "static", "address": "203.0.113.10", "prefix_len": 24, "gateway": "203.0.113.1"},
                        "ipv6": {"mode": "auto"}},
            "root_password": "pw"
        }))
        .unwrap();
        assert!(spec.start_after_create);
        assert_eq!(spec.cpu_cores, 1);
        assert_eq!(spec.network.ipv6, Some(IpConfig::Auto));
        let Some(IpConfig::Static(ip)) = &spec.network.ipv4 else {
            panic!("expected static ip");
        };
        assert_eq!(ip.prefix_len, 24);
        assert!(!format!("{spec:?}").contains("\"pw\""));
        let back: VmSpec = serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn vm_ref_display() {
        assert_eq!(VmRef::on_node("pve1", "101").to_string(), "pve1/101");
        assert_eq!(VmRef::new("42").to_string(), "42");
    }

    #[test]
    fn task_state_serde() {
        let v = serde_json::to_value(TaskState::Running {
            progress_percent: Some(40),
        })
        .unwrap();
        assert_eq!(v, json!({"state": "running", "progress_percent": 40}));
    }
}
