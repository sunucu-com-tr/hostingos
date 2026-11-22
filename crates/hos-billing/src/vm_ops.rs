//! VPS management beyond power / reinstall / snapshots: console sessions for
//! the HostingOS console relay, root password reset, backups, reverse DNS,
//! ISO images, rescue mode, firewall, metrics, bandwidth, SSH keys and the
//! `vm_operations` progress log.
//!
//! Every entry point takes `client_id`: `Some` = a client (must own the
//! service, the service must be active), `None` = staff. The rules enforced
//! here hold for every caller (client area, REST API, admin):
//!
//! * ids coming from a request (snapshot, backup, ISO, IP, SSH key) are
//!   looked up **on the service's own VM / client** — a value that is not in
//!   that list is `NotFound` / `Validation` and the provider is not called;
//! * ISO images must be on the staff-approved allow-list (`vm_iso_images`),
//!   like OS templates;
//! * restores, rollbacks, reinstalls, password resets, rescue and ISO
//!   changes are refused while another operation of the VM runs
//!   ([`Billing::ensure_vm_idle`]);
//! * snapshots are capped per product (`snapshot_limit` in the product's
//!   module settings, default [`DEFAULT_SNAPSHOT_LIMIT`]; `snapshot_slots` of
//!   active addons add to it; staff are not capped);
//! * passwords (reset, rescue) are generated server-side, returned once and
//!   never stored, logged or e-mailed;
//! * a feature the driver lacks is `IntegrationError::Unsupported` (the UI
//!   shows "not supported by this provider").

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::str::FromStr as _;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use chrono::{DateTime, Utc};
use hos_core::CoreError;
use hos_core::domain::ServiceStatus;
use hos_core::error::IntegrationError;
use hos_core::ids::{ClientId, IntegrationId, ServiceId, TenantId, UserId};
use hos_core::integrations::VirtProvider;
use hos_core::integrations::virt::{
    BackupInfo, BandwidthUsage, ConsoleUpstream, FirewallDirection, FirewallProtocol, FirewallRule,
    FirewallState, IsoImage, MetricsHistory, MetricsRange, PasswordReset, PasswordResetRequest,
    RescueResult, TaskRef, TaskState, VirtFeatures, VmExtras,
};
use hos_db::repo::catalog;
use hos_db::repo::services::{self, NewServiceEvent, ServiceRow, VmRow};
use hos_db::repo::vm_ops::{
    self, ConsoleClaim, ConsoleSessionRow, IsoRow, NewConsoleSession, NewVmOperation, SshKeyRow,
    VmOperationRow,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::lifecycle::vm_ref;
use crate::{Billing, BillingResult};

/// Console tokens are valid this long (the page opens the websocket at once).
pub const CONSOLE_TOKEN_TTL_SECS: i64 = 60;
/// Guard of [`Billing::vm_op_lock`]: the per-service lock is released
/// when it is dropped (its transaction rolls back).
pub struct VmOpLock {
    _tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl std::fmt::Debug for VmOpLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VmOpLock")
    }
}

/// Absolute console session limit (the relay closes the session after it).
pub const CONSOLE_MAX_SESSION_SECS: i64 = 4 * 3600;
/// Concurrent console sessions per user.
pub const CONSOLE_SESSION_CAP: i64 = 3;
/// Password resets per service and hour (all callers).
pub const PASSWORD_RESETS_PER_HOUR: i64 = 3;
/// Snapshots per VM when the product does not set `snapshot_limit`.
pub const DEFAULT_SNAPSHOT_LIMIT: u32 = 3;
/// Firewall rules per VM.
pub const MAX_FIREWALL_RULES: usize = 50;
/// Source / destination networks per firewall rule.
pub const MAX_RULE_CIDRS: usize = 20;
/// SSH keys per client.
pub const MAX_SSH_KEYS: usize = 50;

/// Who asks for a console session.
#[derive(Debug, Clone)]
pub struct ConsoleRequester {
    /// Signed-in user.
    pub user_id: UserId,
    /// The client acting (owner account of a sub-user too); `None` = staff.
    pub client_id: Option<ClientId>,
    /// Request IP (audit).
    pub ip: Option<IpAddr>,
    /// Browser user agent (audit).
    pub user_agent: Option<String>,
}

impl ConsoleRequester {
    fn is_staff(&self) -> bool {
        self.client_id.is_none()
    }
}

/// A console token handed to the browser (single use, short lived).
#[derive(Debug, Clone)]
pub struct IssuedConsole {
    /// Token (only its SHA-256 is stored).
    pub token: String,
    /// `vm_console_sessions.id`.
    pub session_id: Uuid,
    /// Token expiry.
    pub expires_at: DateTime<Utc>,
}

/// What the relay needs after the token was claimed.
#[derive(Debug, Clone)]
pub struct ConsoleGrant {
    /// The (now connected) session.
    pub session: ConsoleSessionRow,
    /// Upstream connection of the VM's driver (never sent to the browser).
    pub upstream: ConsoleUpstream,
    /// VM display name.
    pub vm_name: String,
    /// Absolute session limit.
    pub max_duration: std::time::Duration,
}

/// Bandwidth of the current period against the quota.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BandwidthView {
    /// Provider figures.
    pub usage: BandwidthUsage,
    /// Quota (GB): the VM's, the product's `bandwidth_gb`, or the provider's.
    pub quota_gb: Option<f64>,
    /// Used share of the quota (0–100+).
    pub used_percent: Option<f64>,
    /// GB above the quota (0 when within).
    pub overage_gb: f64,
}

/// An IP address of the VM with its PTR record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IpPtr {
    /// Address.
    pub ip: IpAddr,
    /// Current PTR (provider, else the IP pool).
    pub ptr: Option<String>,
    /// Whether the PTR can be changed (provider API or IP pool row).
    pub editable: bool,
}

/// An ISO image a client may mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IsoOption {
    /// Provider reference.
    pub iso_ref: String,
    /// Name shown.
    pub name: String,
}

/// A provider ISO with its allow-list state (staff view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIso {
    /// Provider image.
    pub image: IsoImage,
    /// Allow-list row, when approved (inactive rows included).
    pub approved: Option<IsoRow>,
}

/// A parsed, validated OpenSSH public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSshKey {
    /// `type base64 [comment]`, normalized.
    pub normalized: String,
    /// `SHA256:…` (OpenSSH format).
    pub fingerprint: String,
    /// Key type (`ssh-ed25519`, …).
    pub kind: String,
}

impl Billing {
    // -----------------------------------------------------------------------
    // shared helpers
    // -----------------------------------------------------------------------

    /// What the service's virtualization driver supports (UI switches).
    pub async fn vm_features(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<VirtFeatures> {
        let (_, _, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Ok(virt.features())
    }

    /// Starts a VM operation that checks state or a limit and then calls
    /// the provider: holds a per-service Postgres advisory lock (in a
    /// transaction of its own) until the returned guard is dropped — after
    /// the operation is recorded in `vm_operations` — and checks the VM is
    /// idle under it. A parallel request gets `Conflict` instead of also
    /// passing the check (snapshot limit, password resets per hour, one
    /// operation at a time).
    pub async fn vm_op_lock(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
    ) -> BillingResult<VmOpLock> {
        let mut tx = self.pool().begin().await?;
        let locked: bool =
            sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1, 7151))")
                .bind(format!("hos:vm-op:{tenant_id}:{service_id}"))
                .fetch_one(&mut *tx)
                .await?;
        if !locked {
            return Err(CoreError::Conflict(
                "another operation is being started on this server".into(),
            ));
        }
        self.ensure_vm_idle(tenant_id, service_id).await?;
        Ok(VmOpLock { _tx: tx })
    }

    /// `Conflict` while another operation of the VM is queued or running.
    pub async fn ensure_vm_idle(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
    ) -> BillingResult<()> {
        let open = vm_ops::open_operations(self.pool(), tenant_id, service_id).await?;
        match open.first() {
            Some(op) => Err(CoreError::Conflict(format!(
                "another operation ({}) is still running on this server",
                op.kind.replace('_', " ")
            ))),
            None => Ok(()),
        }
    }

    /// Records the outcome of a provider call in `vm_operations`: a task is
    /// polled (`poll_vm_task` with the operation id), no task = done, an error
    /// is recorded and returned.
    pub(crate) async fn track_vm_op(
        &self,
        tenant_id: TenantId,
        vm: &VmRow,
        kind: &str,
        detail: &str,
        res: Result<Option<TaskRef>, IntegrationError>,
    ) -> BillingResult<Option<TaskRef>> {
        let task = match res {
            Ok(t) => t,
            Err(e) => return Err(self.vm_op_failed(tenant_id, vm, kind, detail, e).await),
        };
        let Some(service_id) = vm.service_id else {
            return Ok(task);
        };
        let op = vm_ops::create_operation(
            self.pool(),
            tenant_id,
            &NewVmOperation {
                service_id,
                kind,
                status: if task.is_some() {
                    "running"
                } else {
                    "succeeded"
                },
                detail,
                task: task.as_ref().and_then(|t| serde_json::to_value(t).ok()),
                error: None,
                actor_kind: None,
                actor_id: None,
            },
        )
        .await?;
        if let Some(t) = &task {
            self.queue_vm_poll(tenant_id, service_id, Some(vm.id), t.clone(), Some(op.id))
                .await?;
        }
        Ok(task)
    }

    /// Records a failed provider call (not `Unsupported`: nothing happened)
    /// and returns the error.
    pub(crate) async fn vm_op_failed(
        &self,
        tenant_id: TenantId,
        vm: &VmRow,
        kind: &str,
        detail: &str,
        err: IntegrationError,
    ) -> CoreError {
        if let (Some(service_id), false) = (
            vm.service_id,
            matches!(err, IntegrationError::Unsupported(_)),
        ) {
            let message = err.to_string();
            if let Err(e) = vm_ops::create_operation(
                self.pool(),
                tenant_id,
                &NewVmOperation {
                    service_id,
                    kind,
                    status: "failed",
                    detail,
                    task: None,
                    error: Some(&message),
                    actor_kind: None,
                    actor_id: None,
                },
            )
            .await
            {
                tracing::warn!(error = %e, "recording a failed VM operation");
            }
        }
        err.into()
    }

    /// Updates the operation a polled task belongs to (`poll_vm_task`).
    pub(crate) async fn note_vm_task_state(
        &self,
        tenant_id: TenantId,
        operation_id: Option<Uuid>,
        state: &TaskState,
    ) {
        let Some(id) = operation_id else { return };
        let res = match state {
            TaskState::Running { progress_percent } => {
                vm_ops::update_operation(
                    self.pool(),
                    tenant_id,
                    id,
                    "running",
                    progress_percent.map(i16::from),
                    None,
                )
                .await
            }
            TaskState::Succeeded => {
                vm_ops::update_operation(self.pool(), tenant_id, id, "succeeded", None, None).await
            }
            TaskState::Failed { message } => {
                vm_ops::update_operation(self.pool(), tenant_id, id, "failed", None, Some(message))
                    .await
            }
        };
        if let Err(e) = res {
            tracing::warn!(error = %e, "updating a VM operation");
        }
    }

    /// Latest operations of the VM (progress / failures for the client).
    pub async fn vm_operations(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        limit: i64,
    ) -> BillingResult<Vec<VmOperationRow>> {
        self.authorize_service(tenant_id, service_id, client_id)
            .await?;
        vm_ops::operations(self.pool(), tenant_id, service_id, limit).await
    }

    fn require_active(svc: &ServiceRow) -> BillingResult<()> {
        if svc.status != ServiceStatus::Active {
            return Err(CoreError::Forbidden(format!(
                "the service is {}",
                svc.status
            )));
        }
        Ok(())
    }

    fn require(enabled: bool, what: &'static str) -> BillingResult<()> {
        if enabled {
            Ok(())
        } else {
            Err(IntegrationError::Unsupported(what).into())
        }
    }

    // -----------------------------------------------------------------------
    // console
    // -----------------------------------------------------------------------

    /// Issues a one-time console token for the relay. Clients need an active
    /// service; staff may open the console of a suspended one.
    pub async fn vm_console_session(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        who: &ConsoleRequester,
    ) -> BillingResult<IssuedConsole> {
        let (_, _, virt) = self.vm_of(tenant_id, service_id, who.client_id).await?;
        Self::require(virt.features().console_relay, "embedded console")?;
        let token = hos_core::crypto::generate_token();
        let row = vm_ops::create_console_session(
            self.pool(),
            tenant_id,
            &NewConsoleSession {
                service_id,
                user_id: who.user_id,
                is_staff: who.is_staff(),
                token_hash: hos_core::crypto::sha256(token.as_bytes()).to_vec(),
                ip: who.ip,
                user_agent: who.user_agent.clone(),
                ttl_secs: CONSOLE_TOKEN_TTL_SECS,
            },
        )
        .await?;
        Ok(IssuedConsole {
            token,
            session_id: row.id,
            expires_at: row.expires_at,
        })
    }

    /// Claims a console token (single use) for the websocket that `who`
    /// opened: the token must belong to the same user and role, the
    /// per-user session cap must not be reached, and the service is
    /// authorized again (ownership, status) before the driver's upstream is
    /// resolved. Every refusal ends the session row with its reason.
    pub async fn vm_console_connect(
        &self,
        tenant_id: TenantId,
        token: &str,
        who: &ConsoleRequester,
        cap: i64,
    ) -> BillingResult<ConsoleGrant> {
        let hash = hos_core::crypto::sha256(token.trim().as_bytes());
        let mut conn = self.pool().acquire().await?;
        let row = match vm_ops::claim_console_session(
            &mut conn,
            tenant_id,
            &hash,
            cap,
            CONSOLE_MAX_SESSION_SECS,
        )
        .await?
        {
            ConsoleClaim::Claimed(row) => row,
            ConsoleClaim::Invalid => return Err(CoreError::Unauthorized),
            ConsoleClaim::CapReached(_) => {
                return Err(CoreError::Conflict(format!(
                    "too many open console sessions (at most {cap})"
                )));
            }
        };
        drop(conn);
        let end = |reason: &'static str| {
            let pool = self.pool().clone();
            async move {
                let _ = vm_ops::end_console_session(&pool, tenant_id, row.id, 0, 0, reason).await;
            }
        };
        if row.user_id != who.user_id || row.is_staff != who.is_staff() {
            end("wrong_user").await;
            return Err(CoreError::Unauthorized);
        }
        let (_, vm, virt) = match self.vm_of(tenant_id, row.service_id, who.client_id).await {
            Ok(v) => v,
            Err(e) => {
                end("forbidden").await;
                return Err(e);
            }
        };
        let upstream = match virt.console_upstream(&vm_ref(&vm)).await {
            Ok(u) => u,
            Err(e) => {
                end("upstream_error").await;
                return Err(e.into());
            }
        };
        Ok(ConsoleGrant {
            session: row,
            upstream,
            vm_name: vm.name.clone(),
            max_duration: std::time::Duration::from_secs(CONSOLE_MAX_SESSION_SECS as u64),
        })
    }

    /// Ends a console session (relay closed it).
    pub async fn vm_console_end(
        &self,
        tenant_id: TenantId,
        session_id: Uuid,
        bytes_in: u64,
        bytes_out: u64,
        reason: &str,
    ) -> BillingResult<()> {
        vm_ops::end_console_session(
            self.pool(),
            tenant_id,
            session_id,
            i64::try_from(bytes_in).unwrap_or(i64::MAX),
            i64::try_from(bytes_out).unwrap_or(i64::MAX),
            reason,
        )
        .await?;
        Ok(())
    }

    /// Console sessions of a service (staff audit view).
    pub async fn vm_console_sessions(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        limit: i64,
    ) -> BillingResult<Vec<ConsoleSessionRow>> {
        vm_ops::console_sessions(self.pool(), tenant_id, service_id, limit).await
    }

    // -----------------------------------------------------------------------
    // password reset
    // -----------------------------------------------------------------------

    /// Resets the root (or `username`'s) password. The password is generated
    /// here, returned once and never stored (the stale stored password of the
    /// service is cleared), never logged, never e-mailed. At most
    /// [`PASSWORD_RESETS_PER_HOUR`] per service and hour.
    pub async fn vm_reset_password(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        username: Option<&str>,
    ) -> BillingResult<PasswordReset> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require_active(&svc)?;
        Self::require(virt.features().password_reset, "password reset")?;
        let username = match username.map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) => Some(validate_username(u)?),
            None => None,
        };
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let since = Utc::now() - chrono::Duration::hours(1);
        let recent = vm_ops::count_operations_since(
            self.pool(),
            tenant_id,
            service_id,
            "password_reset",
            since,
        )
        .await?;
        if recent >= PASSWORD_RESETS_PER_HOUR {
            return Err(CoreError::Conflict(
                "too many password resets for this server; try again in an hour".into(),
            ));
        }
        let detail = username.clone().unwrap_or_else(|| "root".into());
        let password = hos_panels::password::generate_password("virt");
        let res = virt
            .reset_password(
                &vm_ref(&vm),
                &PasswordResetRequest {
                    username,
                    password: Some(password),
                },
            )
            .await;
        let reset = match res {
            Ok(r) => r,
            Err(e) => {
                return Err(self
                    .vm_op_failed(tenant_id, &vm, "password_reset", &detail, e)
                    .await);
            }
        };
        self.track_vm_op(
            tenant_id,
            &vm,
            "password_reset",
            &reset.username,
            Ok(reset.task.clone()),
        )
        .await?;
        let mut conn = self.pool().acquire().await?;
        vm_ops::clear_stored_vm_password(&mut conn, tenant_id, service_id).await?;
        services::log_event(
            self.pool(),
            tenant_id,
            service_id,
            &NewServiceEvent::info(
                "vm_password_reset",
                &format!("password of {} reset", reset.username),
            ),
        )
        .await?;
        Ok(reset)
    }

    // -----------------------------------------------------------------------
    // snapshots (limits; the actions live in `lifecycle`)
    // -----------------------------------------------------------------------

    /// Snapshot quota of a service: the product's `snapshot_limit` (default
    /// [`DEFAULT_SNAPSHOT_LIMIT`], 0 = none) plus `snapshot_slots` of its
    /// active addons.
    pub async fn vm_snapshot_limit(
        &self,
        tenant_id: TenantId,
        svc: &ServiceRow,
    ) -> BillingResult<u32> {
        let base = catalog::get_product(self.pool(), tenant_id, svc.product_id)
            .await?
            .and_then(|p| config_u32(&p.module_config, "snapshot_limit"))
            .unwrap_or(DEFAULT_SNAPSHOT_LIMIT);
        let mut extra = 0u32;
        for a in services::service_addons(self.pool(), tenant_id, svc.id).await? {
            if a.status != ServiceStatus::Active {
                continue;
            }
            if let Some(addon) = catalog::get_addon(self.pool(), tenant_id, a.addon_id).await? {
                extra = extra.saturating_add(
                    config_u32(&addon.module_config, "snapshot_slots").unwrap_or(0),
                );
            }
        }
        Ok(base.saturating_add(extra).min(100))
    }

    /// Snapshot quota of a service by id (`None` for staff: not capped).
    pub async fn vm_snapshot_quota(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<u32> {
        let svc = self
            .authorize_service(tenant_id, service_id, client_id)
            .await?;
        self.vm_snapshot_limit(tenant_id, &svc).await
    }

    /// The snapshot `id` **of this VM** (never trusts the id from a request).
    pub(crate) async fn own_snapshot(
        &self,
        virt: &dyn VirtProvider,
        vm: &VmRow,
        id: &str,
    ) -> BillingResult<String> {
        let id = id.trim();
        virt.list_snapshots(&vm_ref(vm))
            .await?
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.id)
            .ok_or_else(|| CoreError::not_found("snapshot", id))
    }

    // -----------------------------------------------------------------------
    // backups
    // -----------------------------------------------------------------------

    /// Provider backups of the VM.
    pub async fn vm_backups(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<Vec<BackupInfo>> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require(virt.features().backups, "backups")?;
        Ok(virt.list_backups(&vm_ref(&vm)).await?)
    }

    /// Restores a backup **of this VM** (destroys the current disk state).
    pub async fn vm_restore_backup(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        backup_id: &str,
    ) -> BillingResult<()> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require_active(&svc)?;
        Self::require(virt.features().backup_restore, "backup restore")?;
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let backup_id = backup_id.trim();
        let backup = virt
            .list_backups(&vm_ref(&vm))
            .await?
            .into_iter()
            .find(|b| b.id == backup_id)
            .ok_or_else(|| CoreError::not_found("backup", backup_id))?;
        let res = virt.restore_backup(&vm_ref(&vm), &backup.id).await;
        self.track_vm_op(tenant_id, &vm, "backup_restore", &backup.name, res)
            .await?;
        services::log_event(
            self.pool(),
            tenant_id,
            service_id,
            &NewServiceEvent::info("vm_action", &format!("restore backup {}", backup.name)),
        )
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // state, reverse DNS
    // -----------------------------------------------------------------------

    /// Rescue / ISO / PTR state reported by the driver (empty when the driver
    /// reports nothing).
    pub async fn vm_extras(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<VmExtras> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        match virt.vm_extras(&vm_ref(&vm)).await {
            Ok(x) => Ok(x),
            Err(IntegrationError::Unsupported(_)) => Ok(VmExtras::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// The VM's addresses with their PTR records: IP pool rows of the
    /// service, the VM's primary addresses and what the driver reports.
    pub async fn vm_reverse_dns(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<Vec<IpPtr>> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        self.ip_ptrs(tenant_id, service_id, &vm, &*virt).await
    }

    async fn ip_ptrs(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        vm: &VmRow,
        virt: &dyn VirtProvider,
    ) -> BillingResult<Vec<IpPtr>> {
        let provider = virt.features().reverse_dns;
        let mut out: BTreeMap<IpAddr, IpPtr> = BTreeMap::new();
        for row in services::service_ips(self.pool(), tenant_id, service_id).await? {
            out.insert(
                row.address,
                IpPtr {
                    ip: row.address,
                    ptr: row.ptr.clone(),
                    editable: true,
                },
            );
        }
        for ip in [vm.primary_ipv4, vm.primary_ipv6].into_iter().flatten() {
            out.entry(ip).or_insert(IpPtr {
                ip,
                ptr: None,
                editable: provider,
            });
        }
        if provider {
            match virt.vm_extras(&vm_ref(vm)).await {
                Ok(x) => {
                    for e in x.reverse_dns {
                        let entry = out.entry(e.ip).or_insert(IpPtr {
                            ip: e.ip,
                            ptr: None,
                            editable: true,
                        });
                        entry.editable = true;
                        if e.ptr.is_some() {
                            entry.ptr = e.ptr;
                        }
                    }
                }
                Err(IntegrationError::Unsupported(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out.into_values().collect())
    }

    /// Sets (or with `None` resets) the PTR of an address **assigned to this
    /// service** (an IPv6 address may be any address of an assigned /64).
    /// Uses the provider's API when it has one, else the IP pool row.
    pub async fn vm_set_reverse_dns(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        ip: IpAddr,
        ptr: Option<&str>,
    ) -> BillingResult<()> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require_active(&svc)?;
        let ptr = match ptr.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => Some(validate_ptr(p)?),
            None => None,
        };
        let known = self.ip_ptrs(tenant_id, service_id, &vm, &*virt).await?;
        let assigned = known.iter().find(|k| k.ip == ip);
        let in_v6_net = known.iter().any(|k| same_v6_64(k.ip, ip));
        if assigned.is_none() && !(virt.features().reverse_dns && in_v6_net) {
            return Err(CoreError::not_found("ip address", ip));
        }
        let detail = format!("{ip} → {}", ptr.as_deref().unwrap_or("default"));
        let pool_row =
            vm_ops::set_service_ip_ptr(self.pool(), tenant_id, service_id, ip, ptr.as_deref())
                .await?;
        if virt.features().reverse_dns {
            let res = virt.set_reverse_dns(&vm_ref(&vm), ip, ptr.as_deref()).await;
            self.track_vm_op(tenant_id, &vm, "reverse_dns", &detail, res)
                .await?;
        } else if pool_row {
            self.track_vm_op(tenant_id, &vm, "reverse_dns", &detail, Ok(None))
                .await?;
        } else {
            return Err(IntegrationError::Unsupported("reverse DNS for this address").into());
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ISO images
    // -----------------------------------------------------------------------

    /// ISO images the VM may mount: the staff-approved list of the
    /// integration that the provider still has.
    pub async fn vm_isos(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<Vec<IsoOption>> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        self.offered_isos(tenant_id, &vm, &*virt).await
    }

    async fn offered_isos(
        &self,
        tenant_id: TenantId,
        vm: &VmRow,
        virt: &dyn VirtProvider,
    ) -> BillingResult<Vec<IsoOption>> {
        Self::require(virt.features().iso, "ISO images")?;
        let approved = vm_ops::isos(self.pool(), tenant_id, vm.integration_id, true).await?;
        if approved.is_empty() {
            return Ok(Vec::new());
        }
        let available = virt.list_isos(&vm_ref(vm)).await?;
        Ok(approved
            .into_iter()
            .filter(|a| available.iter().any(|i| i.id == a.iso_ref))
            .map(|a| IsoOption {
                iso_ref: a.iso_ref,
                name: a.name,
            })
            .collect())
    }

    /// Staff: the provider's ISO images for the VM with their allow-list state.
    pub async fn vm_provider_isos(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
    ) -> BillingResult<(IntegrationId, Vec<ProviderIso>)> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, None).await?;
        Self::require(virt.features().iso, "ISO images")?;
        let approved = vm_ops::isos(self.pool(), tenant_id, vm.integration_id, false).await?;
        let list = virt
            .list_isos(&vm_ref(&vm))
            .await?
            .into_iter()
            .map(|image| ProviderIso {
                approved: approved.iter().find(|a| a.iso_ref == image.id).cloned(),
                image,
            })
            .collect();
        Ok((vm.integration_id, list))
    }

    /// Staff: approves a provider ISO (it must exist on the provider).
    pub async fn vm_approve_iso(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        iso_ref: &str,
    ) -> BillingResult<IsoRow> {
        let (integration_id, list) = self.vm_provider_isos(tenant_id, service_id).await?;
        let iso = list
            .into_iter()
            .find(|i| i.image.id == iso_ref.trim())
            .ok_or_else(|| CoreError::not_found("iso image", iso_ref))?;
        let name: String = iso.image.name.chars().take(128).collect();
        vm_ops::upsert_iso(self.pool(), tenant_id, integration_id, &iso.image.id, &name).await
    }

    /// Staff: removes an ISO from the allow-list of `integration_id`.
    pub async fn vm_remove_iso(
        &self,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        id: Uuid,
    ) -> BillingResult<()> {
        if !vm_ops::delete_iso(self.pool(), tenant_id, integration_id, id).await? {
            return Err(CoreError::not_found("iso image", id));
        }
        Ok(())
    }

    /// Mounts an allow-listed ISO (optionally booting from it).
    pub async fn vm_mount_iso(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        iso_ref: &str,
        boot: bool,
    ) -> BillingResult<()> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require_active(&svc)?;
        let offered = self.offered_isos(tenant_id, &vm, &*virt).await?;
        let iso = offered
            .into_iter()
            .find(|i| i.iso_ref == iso_ref.trim())
            .ok_or_else(|| CoreError::validation("this ISO image is not offered"))?;
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let res = virt.mount_iso(&vm_ref(&vm), &iso.iso_ref, boot).await;
        self.track_vm_op(tenant_id, &vm, "iso_mount", &iso.name, res)
            .await?;
        Ok(())
    }

    /// Ejects the mounted ISO.
    pub async fn vm_unmount_iso(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<()> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require_active(&svc)?;
        Self::require(virt.features().iso, "ISO images")?;
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let res = virt.unmount_iso(&vm_ref(&vm)).await;
        self.track_vm_op(tenant_id, &vm, "iso_unmount", "", res)
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // rescue
    // -----------------------------------------------------------------------

    /// Enables (with the client's own SSH keys among `key_ids`) or disables
    /// rescue mode. The rescue password is returned once, never stored.
    pub async fn vm_set_rescue(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        enable: bool,
        key_ids: &[Uuid],
    ) -> BillingResult<RescueResult> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require_active(&svc)?;
        Self::require(virt.features().rescue, "rescue mode")?;
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let keys = if enable {
            self.resolve_client_ssh_keys(tenant_id, svc.client_id, key_ids)
                .await?
        } else {
            Vec::new()
        };
        let detail = if enable { "enable" } else { "disable" };
        let res = match virt.set_rescue(&vm_ref(&vm), enable, &keys).await {
            Ok(r) => r,
            Err(e) => return Err(self.vm_op_failed(tenant_id, &vm, "rescue", detail, e).await),
        };
        self.track_vm_op(tenant_id, &vm, "rescue", detail, Ok(res.task.clone()))
            .await?;
        Ok(res)
    }

    // -----------------------------------------------------------------------
    // firewall
    // -----------------------------------------------------------------------

    /// The VM's firewall (rules HostingOS manages).
    pub async fn vm_firewall(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<FirewallState> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require(virt.features().firewall, "firewall")?;
        Ok(virt.firewall(&vm_ref(&vm)).await?)
    }

    /// Replaces the VM's firewall after validation ([`validate_firewall`]);
    /// returns the state that was applied.
    pub async fn vm_set_firewall(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        state: &FirewallState,
    ) -> BillingResult<FirewallState> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require_active(&svc)?;
        Self::require(virt.features().firewall, "firewall")?;
        let state = validate_firewall(state)?;
        let detail = format!(
            "{} ({} rules)",
            if state.enabled { "enabled" } else { "disabled" },
            state.rules.len()
        );
        let res = virt.set_firewall(&vm_ref(&vm), &state).await;
        self.track_vm_op(tenant_id, &vm, "firewall", &detail, res)
            .await?;
        Ok(state)
    }

    // -----------------------------------------------------------------------
    // metrics, bandwidth
    // -----------------------------------------------------------------------

    /// Resource usage history (CPU, memory, disk I/O, network).
    pub async fn vm_metrics_history(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        range: MetricsRange,
    ) -> BillingResult<MetricsHistory> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require(virt.features().metrics_history, "usage graphs")?;
        Ok(virt.metrics_history(&vm_ref(&vm), range).await?)
    }

    /// Bandwidth of the current period against the quota.
    pub async fn vm_bandwidth(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<BandwidthView> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Self::require(virt.features().bandwidth, "bandwidth usage")?;
        let usage = virt.bandwidth_usage(&vm_ref(&vm)).await?;
        let product_quota = catalog::get_product(self.pool(), tenant_id, svc.product_id)
            .await?
            .and_then(|p| config_u32(&p.module_config, "bandwidth_gb"));
        let quota_gb = vm
            .bandwidth_gb
            .filter(|q| *q > 0)
            .map(f64::from)
            .or(product_quota.filter(|q| *q > 0).map(f64::from))
            .or(usage.provider_limit_gb);
        Ok(bandwidth_view(usage, quota_gb))
    }

    // -----------------------------------------------------------------------
    // SSH keys
    // -----------------------------------------------------------------------

    /// The client's SSH keys.
    pub async fn client_ssh_keys(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
    ) -> BillingResult<Vec<SshKeyRow>> {
        vm_ops::ssh_keys(self.pool(), tenant_id, client_id).await
    }

    /// Adds a validated public key to the client's profile (the count is
    /// checked under a per-client lock, so parallel adds cannot exceed
    /// [`MAX_SSH_KEYS`]).
    pub async fn add_client_ssh_key(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
        name: &str,
        public_key: &str,
        created_by: Option<UserId>,
    ) -> BillingResult<SshKeyRow> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(CoreError::validation(
                "give the key a name (at most 64 characters)",
            ));
        }
        let parsed = parse_ssh_public_key(public_key)?;
        let mut tx = self.pool().begin().await?;
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 7152))")
            .bind(format!("hos:ssh-keys:{tenant_id}:{client_id}"))
            .execute(&mut *tx)
            .await?;
        let existing = vm_ops::ssh_keys(&mut *tx, tenant_id, client_id).await?;
        if existing.len() >= MAX_SSH_KEYS {
            return Err(CoreError::Conflict(format!(
                "at most {MAX_SSH_KEYS} SSH keys per account"
            )));
        }
        let row = vm_ops::add_ssh_key(
            &mut *tx,
            tenant_id,
            client_id,
            name,
            &parsed.normalized,
            &parsed.fingerprint,
            created_by,
        )
        .await?
        .ok_or_else(|| CoreError::Conflict("this key is already saved".into()))?;
        tx.commit().await?;
        Ok(row)
    }

    /// Deletes one of the client's keys.
    pub async fn delete_client_ssh_key(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
        id: Uuid,
    ) -> BillingResult<()> {
        if !vm_ops::delete_ssh_key(self.pool(), tenant_id, client_id, id).await? {
            return Err(CoreError::not_found("ssh key", id));
        }
        Ok(())
    }

    /// Public keys among `ids` that belong to `client_id` (unknown or foreign
    /// ids are `NotFound`).
    pub async fn resolve_client_ssh_keys(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
        ids: &[Uuid],
    ) -> BillingResult<Vec<String>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        let rows = vm_ops::ssh_keys_by_ids(self.pool(), tenant_id, client_id, &unique).await?;
        if rows.len() != unique.len() {
            return Err(CoreError::not_found("ssh key", "selection"));
        }
        Ok(rows.into_iter().map(|r| r.public_key).collect())
    }
}

// ---------------------------------------------------------------------------
// validation helpers (pure)
// ---------------------------------------------------------------------------

/// A product / addon module setting as a non-negative integer (number or
/// numeric string).
fn config_u32(config: &Value, key: &str) -> Option<u32> {
    match config.get(key)? {
        Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Snapshot names: 2–40 characters, letters / digits / `-` / `_`, starting
/// with a letter (the strictest of the drivers, Proxmox).
pub fn validate_snapshot_name(name: &str) -> BillingResult<String> {
    let name = name.trim();
    let ok = (2..=40).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok {
        Ok(name.to_owned())
    } else {
        Err(CoreError::validation(
            "snapshot names are 2–40 letters, digits, - or _, starting with a letter",
        ))
    }
}

/// Linux user names for a password reset.
pub fn validate_username(name: &str) -> BillingResult<String> {
    let ok = (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if ok {
        Ok(name.to_owned())
    } else {
        Err(CoreError::validation("invalid user name"))
    }
}

/// A PTR host name: lower-cased, no trailing dot, at least two labels of
/// 1–63 letters / digits / `-` (not at either end), at most 253 characters.
pub fn validate_ptr(ptr: &str) -> BillingResult<String> {
    let host = ptr.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let ok = !host.is_empty()
        && host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        && !labels
            .last()
            .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()));
    if ok {
        Ok(host)
    } else {
        Err(CoreError::validation(
            "enter a valid host name (e.g. server.example.com)",
        ))
    }
}

fn same_v6_64(a: IpAddr, b: IpAddr) -> bool {
    match (a, b) {
        (IpAddr::V6(a), IpAddr::V6(b)) => a.segments()[..4] == b.segments()[..4],
        _ => false,
    }
}

/// Default inbound rules added when a firewall is enabled without any:
/// SSH and ICMP from anywhere (so enabling it does not lock the client out).
pub fn default_firewall_rules() -> Vec<FirewallRule> {
    vec![
        FirewallRule {
            direction: FirewallDirection::In,
            protocol: FirewallProtocol::Tcp,
            port: Some("22".into()),
            cidrs: Vec::new(),
            comment: Some("SSH".into()),
        },
        FirewallRule {
            direction: FirewallDirection::In,
            protocol: FirewallProtocol::Icmp,
            port: None,
            cidrs: Vec::new(),
            comment: Some("ping".into()),
        },
    ]
}

/// Validates and normalizes a firewall: at most [`MAX_FIREWALL_RULES`]
/// rules; ports `N` or `N-M` (1–65535, none for ICMP); networks as CIDRs or
/// addresses (at most [`MAX_RULE_CIDRS`], normalized; none = anywhere);
/// comments of at most 60 printable characters. Enabling without inbound
/// rules adds [`default_firewall_rules`].
pub fn validate_firewall(state: &FirewallState) -> BillingResult<FirewallState> {
    if state.rules.len() > MAX_FIREWALL_RULES {
        return Err(CoreError::validation(format!(
            "at most {MAX_FIREWALL_RULES} firewall rules"
        )));
    }
    let mut rules = Vec::with_capacity(state.rules.len());
    for (i, r) in state.rules.iter().enumerate() {
        let n = i + 1;
        let port = match r.port.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            None => None,
            Some(_) if r.protocol == FirewallProtocol::Icmp => {
                return Err(CoreError::validation(format!(
                    "rule {n}: ICMP rules have no port"
                )));
            }
            Some(p) => Some(validate_port(p).ok_or_else(|| {
                CoreError::validation(format!("rule {n}: ports are N or N-M (1–65535)"))
            })?),
        };
        if r.cidrs.len() > MAX_RULE_CIDRS {
            return Err(CoreError::validation(format!(
                "rule {n}: at most {MAX_RULE_CIDRS} networks"
            )));
        }
        let mut cidrs = Vec::with_capacity(r.cidrs.len());
        for c in r.cidrs.iter().map(|c| c.trim()).filter(|c| !c.is_empty()) {
            let net = ipnet::IpNet::from_str(c)
                .or_else(|_| IpAddr::from_str(c).map(ipnet::IpNet::from))
                .map_err(|_| CoreError::validation(format!("rule {n}: invalid network {c}")))?;
            cidrs.push(net.trunc().to_string());
        }
        cidrs.dedup();
        let comment = r
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| {
                if c.chars().count() > 60 || c.chars().any(char::is_control) {
                    Err(CoreError::validation(format!(
                        "rule {n}: comments are at most 60 characters"
                    )))
                } else {
                    Ok(c.to_owned())
                }
            })
            .transpose()?;
        rules.push(FirewallRule {
            direction: r.direction,
            protocol: r.protocol,
            port,
            cidrs,
            comment,
        });
    }
    if state.enabled && !rules.iter().any(|r| r.direction == FirewallDirection::In) {
        let mut with_defaults = default_firewall_rules();
        with_defaults.extend(rules);
        rules = with_defaults;
    }
    Ok(FirewallState {
        enabled: state.enabled,
        rules,
    })
}

fn validate_port(p: &str) -> Option<String> {
    let parse = |s: &str| s.trim().parse::<u16>().ok().filter(|v| *v > 0);
    match p.split_once(['-', ':']) {
        Some((a, b)) => {
            let (a, b) = (parse(a)?, parse(b)?);
            (a <= b).then(|| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}-{b}")
                }
            })
        }
        None => parse(p).map(|v| v.to_string()),
    }
}

/// Bandwidth figures against a quota.
pub fn bandwidth_view(usage: BandwidthUsage, quota_gb: Option<f64>) -> BandwidthView {
    let used = usage.counted_gb.max(0.0);
    let used_percent = quota_gb.filter(|q| *q > 0.0).map(|q| used / q * 100.0);
    let overage_gb = quota_gb.map(|q| (used - q).max(0.0)).unwrap_or(0.0);
    BandwidthView {
        usage,
        quota_gb,
        used_percent,
        overage_gb,
    }
}

const SSH_KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// Parses an OpenSSH public key line (`type base64 [comment]`): known type,
/// valid base64 whose embedded type matches, RSA keys of at least 2048 bits;
/// options prefixes and private keys are refused. Returns the normalized line
/// and the `SHA256:` fingerprint.
pub fn parse_ssh_public_key(input: &str) -> BillingResult<ParsedSshKey> {
    let invalid =
        || CoreError::validation("paste an OpenSSH public key (ssh-ed25519 AAAA… comment)");
    let line = input.trim();
    if line.len() > 16_000 || line.contains("PRIVATE KEY") || line.lines().count() != 1 {
        return Err(invalid());
    }
    let mut parts = line.split_whitespace();
    let kind = parts.next().ok_or_else(invalid)?;
    if !SSH_KEY_TYPES.contains(&kind) {
        return Err(invalid());
    }
    let b64 = parts.next().ok_or_else(invalid)?;
    let comment: String = parts
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    let blob = STANDARD.decode(b64).map_err(|_| invalid())?;
    let mut rd = SshReader(&blob);
    if rd.string().ok_or_else(invalid)? != kind.as_bytes() {
        return Err(invalid());
    }
    match kind {
        "ssh-rsa" => {
            let _e = rd.string().ok_or_else(invalid)?;
            let n = rd.string().ok_or_else(invalid)?;
            let n = match n.iter().position(|b| *b != 0) {
                Some(i) => &n[i..],
                None => return Err(invalid()),
            };
            let bits = n.len() * 8 - n[0].leading_zeros() as usize;
            if bits < 2048 {
                return Err(CoreError::validation(
                    "RSA keys must have at least 2048 bits",
                ));
            }
        }
        "ssh-ed25519" => {
            if rd.string().ok_or_else(invalid)?.len() != 32 {
                return Err(invalid());
            }
        }
        _ => {
            rd.string().ok_or_else(invalid)?;
        }
    }
    let fingerprint = format!(
        "SHA256:{}",
        STANDARD_NO_PAD.encode(hos_core::crypto::sha256(&blob))
    );
    let normalized = if comment.is_empty() {
        format!("{kind} {}", STANDARD.encode(&blob))
    } else {
        format!("{kind} {} {comment}", STANDARD.encode(&blob))
    };
    Ok(ParsedSshKey {
        normalized,
        fingerprint,
        kind: kind.to_owned(),
    })
}

struct SshReader<'a>(&'a [u8]);

impl<'a> SshReader<'a> {
    fn string(&mut self) -> Option<&'a [u8]> {
        let len = u32::from_be_bytes(self.0.get(..4)?.try_into().ok()?) as usize;
        let data = self.0.get(4..4 + len)?;
        self.0 = &self.0[4 + len..];
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl user@host";

    #[test]
    fn ssh_keys() {
        let k = parse_ssh_public_key(ED25519).unwrap();
        assert_eq!(k.kind, "ssh-ed25519");
        assert!(k.fingerprint.starts_with("SHA256:"));
        assert_eq!(k.fingerprint.len(), 7 + 43);
        assert!(k.normalized.ends_with(" user@host"));
        // Embedded type must match the declared one.
        let wrong = ED25519.replacen("ssh-ed25519", "ssh-rsa", 1);
        assert!(parse_ssh_public_key(&wrong).is_err());
        for bad in [
            "",
            "ssh-ed25519",
            "ssh-ed25519 !!!",
            "ssh-dss AAAAB3NzaC1kc3M= x",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "command=\"x\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
        ] {
            assert!(parse_ssh_public_key(bad).is_err(), "{bad}");
        }
        // A 1024-bit RSA key is refused.
        let mut blob = Vec::new();
        for part in [&b"ssh-rsa"[..], &[1, 0, 1][..], &[0xC0u8; 128][..]] {
            blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
            blob.extend_from_slice(part);
        }
        let short = format!("ssh-rsa {}", STANDARD.encode(&blob));
        assert!(parse_ssh_public_key(&short).is_err());
    }

    #[test]
    fn names_and_ptrs() {
        assert!(validate_snapshot_name("before-upgrade_1").is_ok());
        for bad in [
            "",
            "a",
            "1abc",
            "has space",
            "x".repeat(41).as_str(),
            "semi;colon",
        ] {
            assert!(validate_snapshot_name(bad).is_err(), "{bad}");
        }
        assert_eq!(
            validate_ptr("Mail.Example.com.").unwrap(),
            "mail.example.com"
        );
        for bad in [
            "localhost",
            "-a.example.com",
            "a..b",
            "1.2.3.4",
            "a_b.example.com",
            "",
        ] {
            assert!(validate_ptr(bad).is_err(), "{bad}");
        }
        assert!(validate_username("deploy_1").is_ok());
        assert!(validate_username("Root").is_err());
        assert!(validate_username("1abc").is_err());
    }

    #[test]
    fn firewall_validation() {
        let rule = |protocol, port: Option<&str>, cidrs: &[&str]| FirewallRule {
            direction: FirewallDirection::In,
            protocol,
            port: port.map(str::to_owned),
            cidrs: cidrs.iter().map(|c| (*c).to_owned()).collect(),
            comment: None,
        };
        let ok = validate_firewall(&FirewallState {
            enabled: true,
            rules: vec![rule(
                FirewallProtocol::Tcp,
                Some("8000:8080"),
                &["10.0.0.7/8", "2001:db8::1"],
            )],
        })
        .unwrap();
        assert_eq!(ok.rules[0].port.as_deref(), Some("8000-8080"));
        assert_eq!(ok.rules[0].cidrs, ["10.0.0.0/8", "2001:db8::1/128"]);
        for bad in [
            rule(FirewallProtocol::Tcp, Some("0"), &[]),
            rule(FirewallProtocol::Tcp, Some("90-80"), &[]),
            rule(FirewallProtocol::Udp, Some("70000"), &[]),
            rule(FirewallProtocol::Icmp, Some("22"), &[]),
            rule(FirewallProtocol::Tcp, None, &["10.0.0.0/33"]),
            rule(FirewallProtocol::Tcp, None, &["example.com"]),
        ] {
            let st = FirewallState {
                enabled: true,
                rules: vec![bad.clone()],
            };
            assert!(validate_firewall(&st).is_err(), "{bad:?}");
        }
        let too_many = FirewallState {
            enabled: true,
            rules: vec![rule(FirewallProtocol::Tcp, Some("80"), &[]); MAX_FIREWALL_RULES + 1],
        };
        assert!(validate_firewall(&too_many).is_err());
        // Enabling without inbound rules keeps SSH reachable.
        let defaults = validate_firewall(&FirewallState {
            enabled: true,
            rules: Vec::new(),
        })
        .unwrap();
        assert_eq!(defaults.rules, default_firewall_rules());
        // Disabled: nothing added.
        assert!(
            validate_firewall(&FirewallState::default())
                .unwrap()
                .rules
                .is_empty()
        );
    }

    #[test]
    fn bandwidth_overage() {
        let usage = BandwidthUsage {
            outgoing_gb: Some(1200.0),
            incoming_gb: None,
            counted_gb: 1200.0,
            provider_limit_gb: None,
            estimated: false,
            period_start: None,
        };
        let v = bandwidth_view(usage.clone(), Some(1000.0));
        assert_eq!(v.overage_gb, 200.0);
        assert_eq!(v.used_percent, Some(120.0));
        let v = bandwidth_view(usage, None);
        assert_eq!(v.overage_gb, 0.0);
        assert_eq!(v.used_percent, None);
    }

    #[test]
    fn config_numbers() {
        let c = serde_json::json!({"a": 5, "b": " 7 ", "c": -1, "d": "x"});
        assert_eq!(config_u32(&c, "a"), Some(5));
        assert_eq!(config_u32(&c, "b"), Some(7));
        assert_eq!(config_u32(&c, "c"), None);
        assert_eq!(config_u32(&c, "d"), None);
        assert_eq!(config_u32(&c, "missing"), None);
    }
}
