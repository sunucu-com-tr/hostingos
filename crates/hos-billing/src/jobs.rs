//! Typed background jobs: [`JobKind`], their serde payloads and `enqueue_*`
//! helpers.
//!
//! The kinds and payloads live here (not in `hos-jobs`) so the billing engine
//! can queue work without depending on the job runtime; `hos-jobs` depends on
//! this crate and implements one handler per kind. Every helper writes the
//! `jobs` row with the caller's connection — inside the caller's transaction
//! when there is one, so the job exists iff the business change commits — and
//! sends `pg_notify(`[`NOTIFY_CHANNEL`]`, queue)` to wake listening workers.

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::ids::{DomainId, InvoiceId, ServiceId, TenantId, UserId, VmId, WebhookEndpointId};
use hos_core::integrations::virt::TaskRef;
use hos_db::ids::{AiGenerationId, EmailLogId};
use hos_db::repo::billing_support;
use hos_db::repo::jobs::{self as repo, Enqueued, NewJob};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::BillingResult;

/// `pg_notify` channel of the job queue (payload: queue name).
/// Pre-rename "hf" name, kept on purpose: every running worker LISTENs on it.
pub const NOTIFY_CHANNEL: &str = "hf_jobs";

/// Queue names.
pub mod queues {
    /// Everything not listed below.
    pub const DEFAULT: &str = "default";
    /// Calls to panels / hypervisors / registrars.
    pub const PROVISIONING: &str = "provisioning";
    /// E-mails and outgoing webhooks.
    pub const NOTIFICATIONS: &str = "notifications";
    /// Scheduled maintenance (automation, syncs, cleanup).
    pub const MAINTENANCE: &str = "maintenance";
    /// All queues, for a worker that serves everything.
    pub const ALL: &[&str] = &[DEFAULT, PROVISIONING, NOTIFICATIONS, MAINTENANCE];
}

hos_core::string_enum! {
    /// `jobs.kind` values understood by `hos-jobs`.
    pub enum JobKind {
        /// Create the account / VM of a service ([`ServicePayload`]).
        ProvisionService => "provision_service",
        /// Suspend a service at its provider ([`ServicePayload`]).
        SuspendService => "suspend_service",
        /// Unsuspend a service ([`ServicePayload`]).
        UnsuspendService => "unsuspend_service",
        /// Terminate a service ([`ServicePayload`]).
        TerminateService => "terminate_service",
        /// Apply a paid plan change ([`ChangePackagePayload`]).
        ChangePackage => "change_package",
        /// Register a domain ([`DomainPayload`]).
        RegisterDomain => "register_domain",
        /// Renew a domain ([`DomainPayload`]).
        RenewDomain => "renew_domain",
        /// Start an incoming transfer ([`DomainPayload`]).
        TransferDomain => "transfer_domain",
        /// Poll an asynchronous hypervisor task ([`PollVmTaskPayload`]).
        PollVmTask => "poll_vm_task",
        /// Send a logged e-mail ([`SendEmailPayload`]).
        SendEmail => "send_email",
        /// Deliver an outgoing webhook ([`DeliverWebhookPayload`]).
        DeliverWebhook => "deliver_webhook",
        /// Daily billing automation of a tenant ([`TenantPayload`]).
        DailyAutomation => "daily_automation",
        /// Synchronise domain statuses / expiry dates with registrars ([`TenantPayload`]).
        SyncDomainStatus => "sync_domain_status",
        /// Refresh TLD prices from registrars with the margin rule ([`TenantPayload`]).
        SyncTldPricing => "sync_tld_pricing",
        /// Collect usage statistics from panels / hypervisors ([`TenantPayload`]).
        SyncUsage => "sync_usage",
        /// Charge a stored payment method for an invoice ([`ChargeInvoicePayload`]).
        ChargeInvoice => "charge_invoice",
        /// Submit URLs to IndexNow ([`IndexNowPayload`]).
        SeoIndexnowPing => "seo_indexnow_ping",
        /// Run an AI content task and store it in `ai_generations` ([`AiGenerationPayload`]).
        AiGeneration => "ai_generation",
        /// Purge expired carts / sessions and old jobs ([`CleanupPayload`]).
        Cleanup => "cleanup",
        /// Submit / refresh / cancel an e-document
        /// ([`crate::einvoice::EInvoicePayload`]).
        EinvoiceProcess => "einvoice_process",
        /// Tenant e-invoice sweep (pending documents, refunds, reconciliation).
        EinvoiceSweep => "einvoice_sweep",
    }
}

impl JobKind {
    /// Queue a kind runs on.
    pub fn queue(&self) -> &'static str {
        match self {
            Self::ProvisionService
            | Self::SuspendService
            | Self::UnsuspendService
            | Self::TerminateService
            | Self::ChangePackage
            | Self::RegisterDomain
            | Self::RenewDomain
            | Self::TransferDomain
            | Self::PollVmTask
            | Self::ChargeInvoice => queues::PROVISIONING,
            Self::SendEmail | Self::DeliverWebhook => queues::NOTIFICATIONS,
            Self::DailyAutomation
            | Self::SyncDomainStatus
            | Self::SyncTldPricing
            | Self::SyncUsage
            | Self::Cleanup => queues::MAINTENANCE,
            Self::SeoIndexnowPing | Self::AiGeneration => queues::DEFAULT,
            Self::EinvoiceProcess => queues::NOTIFICATIONS,
            Self::EinvoiceSweep => queues::MAINTENANCE,
        }
    }

    /// Default attempt budget.
    pub fn max_attempts(&self) -> i32 {
        match self {
            Self::PollVmTask => 120,
            Self::DeliverWebhook => 10,
            Self::SendEmail => 8,
            Self::DailyAutomation | Self::Cleanup | Self::EinvoiceSweep => 3,
            Self::EinvoiceProcess => 8,
            Self::AiGeneration => 2,
            _ => 5,
        }
    }

    /// Default priority (higher first).
    pub fn priority(&self) -> i16 {
        match self {
            Self::ProvisionService | Self::UnsuspendService | Self::ChargeInvoice => 10,
            Self::SendEmail | Self::PollVmTask => 5,
            Self::Cleanup | Self::SyncUsage | Self::SyncTldPricing => -5,
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// payloads
// ---------------------------------------------------------------------------

/// Payload of service jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePayload {
    pub service_id: ServiceId,
    /// Suspension reason / note.
    #[serde(default)]
    pub reason: Option<String>,
    /// Staff member who asked for it.
    #[serde(default)]
    pub actor: Option<UserId>,
}

/// Payload of [`JobKind::ChangePackage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePackagePayload {
    pub service_id: ServiceId,
    /// `service_upgrades.id`.
    pub upgrade_id: Uuid,
}

/// Payload of domain jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainPayload {
    pub domain_id: DomainId,
    /// Renewal period (defaults to the registration period).
    #[serde(default)]
    pub years: Option<i16>,
}

/// What an asynchronous hypervisor task is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmTaskPurpose {
    /// VM creation: the service becomes active when it finishes.
    Create,
    /// Power / snapshot / resize: only the VM status is refreshed.
    Other,
}

/// Payload of [`JobKind::PollVmTask`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollVmTaskPayload {
    pub service_id: ServiceId,
    #[serde(default)]
    pub vm_id: Option<VmId>,
    pub task: TaskRef,
    pub purpose: VmTaskPurpose,
    /// `vm_operations` row that tracks this task (progress / failure shown
    /// to the client), when the action recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<uuid::Uuid>,
}

/// Payload of [`JobKind::SendEmail`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendEmailPayload {
    pub email_log_id: EmailLogId,
}

/// Payload of [`JobKind::DeliverWebhook`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliverWebhookPayload {
    pub endpoint_id: WebhookEndpointId,
    /// Event name, e.g. `invoice.paid`.
    pub event: String,
    /// Unique event id (`X-HostingOS-Event-Id`, receivers dedupe on it).
    pub event_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub data: Value,
}

/// Payload of tenant-wide maintenance jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantPayload {
    /// Business date to run for (default: the billing clock's today).
    #[serde(default)]
    pub as_of: Option<NaiveDate>,
}

/// Payload of [`JobKind::ChargeInvoice`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargeInvoicePayload {
    pub invoice_id: InvoiceId,
}

/// Payload of [`JobKind::SeoIndexnowPing`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexNowPayload {
    /// Site host (`www.example.com`).
    pub host: String,
    /// IndexNow key.
    pub key: String,
    /// Key file URL when not at the default location.
    #[serde(default)]
    pub key_location: Option<String>,
    pub urls: Vec<String>,
}

/// Payload of [`JobKind::AiGeneration`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiGenerationPayload {
    pub generation_id: AiGenerationId,
}

/// Payload of [`JobKind::Cleanup`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPayload {
    /// Keep finished jobs this many days (default 30).
    #[serde(default)]
    pub job_retention_days: Option<u32>,
}

/// Parses a job payload.
pub fn parse_payload<T: DeserializeOwned>(payload: &Value) -> Result<T, serde_json::Error> {
    T::deserialize(payload)
}

// ---------------------------------------------------------------------------
// enqueue
// ---------------------------------------------------------------------------

/// A job to queue (defaults from [`JobKind`]).
#[derive(Debug, Clone)]
pub struct JobRequest {
    pub kind: JobKind,
    pub tenant_id: Option<TenantId>,
    pub payload: Value,
    pub run_at: Option<DateTime<Utc>>,
    pub dedupe_key: Option<String>,
    pub priority: Option<i16>,
    pub max_attempts: Option<i32>,
}

impl JobRequest {
    /// A job of `kind` for `tenant_id` with a serializable payload.
    pub fn new(kind: JobKind, tenant_id: Option<TenantId>, payload: &impl Serialize) -> Self {
        Self {
            kind,
            tenant_id,
            payload: serde_json::to_value(payload).unwrap_or(Value::Null),
            run_at: None,
            dedupe_key: None,
            priority: None,
            max_attempts: None,
        }
    }

    /// Runs no earlier than `at`.
    #[must_use]
    pub fn at(mut self, at: DateTime<Utc>) -> Self {
        self.run_at = Some(at);
        self
    }

    /// Idempotency key (a second enqueue with the same key is a no-op while
    /// the first job row exists).
    #[must_use]
    pub fn dedupe(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    /// Overrides the attempt budget.
    #[must_use]
    pub fn max_attempts(mut self, n: i32) -> Self {
        self.max_attempts = Some(n);
        self
    }
}

/// Queues a job and notifies workers (on commit, when inside a transaction).
pub async fn enqueue(conn: &mut PgConnection, req: JobRequest) -> BillingResult<Enqueued> {
    let queue = req.kind.queue();
    let mut job = NewJob::new(req.kind.as_str(), req.payload)
        .queue(queue)
        .priority(req.priority.unwrap_or_else(|| req.kind.priority()))
        .max_attempts(req.max_attempts.unwrap_or_else(|| req.kind.max_attempts()));
    if let Some(t) = req.tenant_id {
        job = job.tenant(t);
    }
    if let Some(at) = req.run_at {
        job = job.run_at(at);
    }
    if let Some(key) = req.dedupe_key {
        job = job.dedupe(key);
    }
    let res = repo::enqueue(&mut *conn, &job).await?;
    if res.is_created() {
        billing_support::notify_channel(&mut *conn, NOTIFY_CHANNEL, queue).await?;
    }
    Ok(res)
}

/// Queues a service job (`provision_service`, `suspend_service`, ...).
pub async fn enqueue_service_job(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    kind: JobKind,
    payload: &ServicePayload,
    dedupe: Option<String>,
) -> BillingResult<Enqueued> {
    let mut req = JobRequest::new(kind, Some(tenant_id), payload);
    req.dedupe_key = dedupe;
    enqueue(conn, req).await
}

/// Queues provisioning of a service.
pub async fn enqueue_provision(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    service_id: ServiceId,
    dedupe: Option<String>,
) -> BillingResult<Enqueued> {
    enqueue_service_job(
        conn,
        tenant_id,
        JobKind::ProvisionService,
        &ServicePayload {
            service_id,
            reason: None,
            actor: None,
        },
        dedupe,
    )
    .await
}

/// Queues a domain job (`register_domain`, `transfer_domain`, `renew_domain`).
pub async fn enqueue_domain_job(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    kind: JobKind,
    domain_id: DomainId,
    years: Option<i16>,
    dedupe: Option<String>,
) -> BillingResult<Enqueued> {
    let mut req = JobRequest::new(kind, Some(tenant_id), &DomainPayload { domain_id, years });
    req.dedupe_key = dedupe;
    enqueue(conn, req).await
}

/// Queues sending of a logged e-mail.
pub async fn enqueue_send_email(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    email_log_id: EmailLogId,
) -> BillingResult<Enqueued> {
    enqueue(
        conn,
        JobRequest::new(
            JobKind::SendEmail,
            Some(tenant_id),
            &SendEmailPayload { email_log_id },
        ),
    )
    .await
}

/// Queues polling of a hypervisor task (first poll after `delay`).
pub async fn enqueue_poll_vm_task(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    payload: &PollVmTaskPayload,
    run_at: DateTime<Utc>,
) -> BillingResult<Enqueued> {
    enqueue(
        conn,
        JobRequest::new(JobKind::PollVmTask, Some(tenant_id), payload).at(run_at),
    )
    .await
}

/// Queues an AI generation run.
pub async fn enqueue_ai_generation(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    generation_id: AiGenerationId,
) -> BillingResult<Enqueued> {
    enqueue(
        conn,
        JobRequest::new(
            JobKind::AiGeneration,
            Some(tenant_id),
            &AiGenerationPayload { generation_id },
        )
        .dedupe(format!("ai_generation:{generation_id}")),
    )
    .await
}

/// Queues an IndexNow submission.
pub async fn enqueue_indexnow(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    payload: &IndexNowPayload,
) -> BillingResult<Enqueued> {
    enqueue(
        conn,
        JobRequest::new(JobKind::SeoIndexnowPing, Some(tenant_id), payload),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_roundtrip_and_route() {
        for k in JobKind::ALL {
            assert_eq!(k.as_str().parse::<JobKind>().unwrap(), *k);
            assert!(queues::ALL.contains(&k.queue()));
            assert!(k.max_attempts() >= 1);
        }
        assert_eq!(JobKind::ProvisionService.as_str(), "provision_service");
        assert_eq!(JobKind::SendEmail.queue(), queues::NOTIFICATIONS);
    }

    #[test]
    fn payloads_roundtrip() {
        let p = ServicePayload {
            service_id: ServiceId::new(),
            reason: Some("overdue".into()),
            actor: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(parse_payload::<ServicePayload>(&v).unwrap(), p);
        let minimal = serde_json::json!({"service_id": p.service_id});
        assert_eq!(
            parse_payload::<ServicePayload>(&minimal).unwrap().reason,
            None
        );
        assert!(parse_payload::<DomainPayload>(&serde_json::json!({})).is_err());
        let t: TenantPayload = parse_payload(&serde_json::json!({})).unwrap();
        assert_eq!(t.as_of, None);
    }
}
