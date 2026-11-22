//! Job handlers: one function per [`JobKind`], all delegating to the billing
//! engine. A handler returns [`Outcome`] (stored in `jobs.result`) or a
//! [`HandlerError`] that tells the worker whether to retry.

use std::time::Duration;

use chrono::Utc;
use hos_billing::Billing;
use hos_billing::jobs::{
    AiGenerationPayload, ChangePackagePayload, ChargeInvoicePayload, CleanupPayload,
    DeliverWebhookPayload, DomainPayload, IndexNowPayload, JobKind, PollVmTaskPayload,
    SendEmailPayload, ServicePayload, TenantPayload, parse_payload,
};
use hos_billing::notify::{DeliveryOutcome, webhook_body, webhook_signature};
use hos_billing::provisioning::RunContext;
use hos_core::CoreError;
use hos_core::error::IntegrationError;
use hos_core::ids::TenantId;
use hos_db::repo::jobs::JobRow;
use hos_db::repo::webhooks;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::ai;

/// Successful handler result.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Finished; the value is stored in `jobs.result`.
    Done(Value),
    /// Not finished yet (e.g. a hypervisor task still running): run the job
    /// again after the delay (counts as an attempt).
    Again { after: Duration, note: String },
}

/// Handler failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerError {
    /// Transient: retry with backoff (or after `after` when the provider said so).
    Retry {
        message: String,
        after: Option<Duration>,
    },
    /// Permanent: the job is dead (`failed`) immediately.
    Permanent(String),
}

impl HandlerError {
    /// Message for `jobs.last_error`.
    pub fn message(&self) -> &str {
        match self {
            Self::Retry { message, .. } | Self::Permanent(message) => message,
        }
    }
}

impl From<CoreError> for HandlerError {
    fn from(e: CoreError) -> Self {
        match &e {
            CoreError::Integration(i) if i.is_retryable() => Self::Retry {
                message: e.to_string(),
                after: i.retry_after(),
            },
            CoreError::Database(_) | CoreError::Internal(_) => Self::Retry {
                message: e.to_string(),
                after: None,
            },
            _ => Self::Permanent(e.to_string()),
        }
    }
}

impl From<IntegrationError> for HandlerError {
    fn from(e: IntegrationError) -> Self {
        CoreError::from(e).into()
    }
}

/// What handlers get.
#[derive(Clone)]
pub struct HandlerContext {
    pub billing: Billing,
    pub http: reqwest::Client,
    /// Retention of finished jobs for `cleanup` (default 30 days).
    pub job_retention: Duration,
    /// Skip the SSRF guard of outgoing webhooks (tests only).
    pub allow_private_webhook_targets: bool,
}

impl std::fmt::Debug for HandlerContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerContext").finish_non_exhaustive()
    }
}

fn payload<T: DeserializeOwned>(job: &JobRow) -> Result<T, HandlerError> {
    parse_payload(&job.payload)
        .map_err(|e| HandlerError::Permanent(format!("invalid {} payload: {e}", job.kind)))
}

fn tenant(job: &JobRow) -> Result<TenantId, HandlerError> {
    job.tenant_id
        .ok_or_else(|| HandlerError::Permanent(format!("{} needs a tenant", job.kind)))
}

fn done(v: impl serde::Serialize) -> Result<Outcome, HandlerError> {
    Ok(Outcome::Done(
        serde_json::to_value(v).unwrap_or(Value::Null),
    ))
}

/// Runs one job.
pub async fn dispatch(ctx: &HandlerContext, job: &JobRow) -> Result<Outcome, HandlerError> {
    let kind: JobKind = job
        .kind
        .parse()
        .map_err(|_| HandlerError::Permanent(format!("unknown job kind {}", job.kind)))?;
    let run = RunContext {
        job_id: Some(job.id),
        final_attempt: job.attempts >= job.max_attempts,
    };
    let b = &ctx.billing;
    match kind {
        JobKind::ProvisionService => {
            let p: ServicePayload = payload(job)?;
            done(b.provision_service(tenant(job)?, p.service_id, run).await?)
        }
        JobKind::SuspendService => {
            let p: ServicePayload = payload(job)?;
            done(
                b.suspend_service(tenant(job)?, p.service_id, p.reason.as_deref(), run)
                    .await?,
            )
        }
        JobKind::UnsuspendService => {
            let p: ServicePayload = payload(job)?;
            done(b.unsuspend_service(tenant(job)?, p.service_id, run).await?)
        }
        JobKind::TerminateService => {
            let p: ServicePayload = payload(job)?;
            done(b.terminate_service(tenant(job)?, p.service_id, run).await?)
        }
        JobKind::ChangePackage => {
            let p: ChangePackagePayload = payload(job)?;
            done(b.apply_plan_change(tenant(job)?, p.upgrade_id, run).await?)
        }
        JobKind::RegisterDomain | JobKind::TransferDomain => {
            let p: DomainPayload = payload(job)?;
            done(b.register_domain(tenant(job)?, p.domain_id, run).await?)
        }
        JobKind::RenewDomain => {
            let p: DomainPayload = payload(job)?;
            done(
                b.renew_domain(tenant(job)?, p.domain_id, p.years, run)
                    .await?,
            )
        }
        JobKind::PollVmTask => {
            let p: PollVmTaskPayload = payload(job)?;
            match b.poll_vm_task(tenant(job)?, &p, run).await? {
                Some(o) => done(o),
                None => Ok(Outcome::Again {
                    after: b.config().vm_poll_interval,
                    note: "task still running".into(),
                }),
            }
        }
        JobKind::SendEmail => {
            let p: SendEmailPayload = payload(job)?;
            match b
                .send_logged_email(tenant(job)?, p.email_log_id, run.final_attempt)
                .await?
            {
                DeliveryOutcome::Sent => done(json!({"sent": true})),
                DeliveryOutcome::AlreadySent => done(json!({"sent": true, "duplicate": true})),
                DeliveryOutcome::Failed(e) => Err(HandlerError::Permanent(e)),
            }
        }
        JobKind::DeliverWebhook => {
            let p: DeliverWebhookPayload = payload(job)?;
            deliver_webhook(ctx, tenant(job)?, &p).await
        }
        JobKind::DailyAutomation => {
            let p: TenantPayload = payload(job)?;
            done(b.run_daily(tenant(job)?, p.as_of).await?)
        }
        JobKind::SyncDomainStatus => done(b.sync_domain_status(tenant(job)?, 200).await?),
        JobKind::SyncTldPricing => done(b.sync_tld_pricing(tenant(job)?).await?),
        JobKind::SyncUsage => done(b.sync_usage(tenant(job)?, 500).await?),
        JobKind::ChargeInvoice => {
            let p: ChargeInvoicePayload = payload(job)?;
            done(b.charge_stored_method(tenant(job)?, p.invoice_id).await?)
        }
        JobKind::SeoIndexnowPing => {
            let p: IndexNowPayload = payload(job)?;
            let client = hos_seo::indexnow::IndexNowClient::new()?;
            let report = client
                .submit(&p.host, &p.key, p.key_location.as_deref(), &p.urls)
                .await?;
            done(report)
        }
        JobKind::AiGeneration => {
            let p: AiGenerationPayload = payload(job)?;
            ai::run_generation(b, tenant(job)?, p.generation_id, run.final_attempt).await
        }
        JobKind::Cleanup => {
            let p: CleanupPayload = payload(job)?;
            let retention = p.job_retention_days.map_or(ctx.job_retention, |d| {
                Duration::from_secs(u64::from(d) * 86_400)
            });
            done(b.cleanup(retention).await?)
        }
        JobKind::EinvoiceProcess => {
            let p: hos_billing::einvoice::EInvoicePayload = payload(job)?;
            done(
                b.einvoice_process(tenant(job)?, p.document_id, p.action, run.final_attempt)
                    .await?,
            )
        }
        JobKind::EinvoiceSweep => done(b.einvoice_sweep(tenant(job)?).await?),
    }
}

/// POSTs an outgoing webhook with the HostingOS signature headers.
async fn deliver_webhook(
    ctx: &HandlerContext,
    tenant_id: TenantId,
    p: &DeliverWebhookPayload,
) -> Result<Outcome, HandlerError> {
    let b = &ctx.billing;
    let Some(endpoint) = b.webhook_endpoint(tenant_id, p.endpoint_id).await? else {
        return done(json!({"skipped": "endpoint deleted"}));
    };
    if !endpoint.is_active {
        return done(json!({"skipped": "endpoint disabled"}));
    }
    let secret = b.webhook_secret(tenant_id, &endpoint)?;
    let body = webhook_body(tenant_id, p);
    // SSRF guard: https only, and every resolved address must be public; the
    // connection is pinned to the checked addresses (no DNS rebinding) and
    // redirects are not followed.
    let http = if ctx.allow_private_webhook_targets {
        ctx.http.clone()
    } else {
        match hos_billing::notify::resolve_webhook_target(&endpoint.url).await {
            Ok((host, addrs)) => reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .connect_timeout(std::time::Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .resolve_to_addrs(&host, &addrs)
                .build()
                .map_err(|e| HandlerError::Permanent(format!("http client: {e}")))?,
            Err(e) => {
                webhooks::record_delivery(b.pool(), tenant_id, endpoint.id, None, false)
                    .await
                    .map_err(HandlerError::from)?;
                return match e {
                    hos_billing::notify::WebhookTargetError::Unresolvable => {
                        Err(HandlerError::Retry {
                            message: e.to_string(),
                            after: None,
                        })
                    }
                    _ => Err(HandlerError::Permanent(e.to_string())),
                };
            }
        }
    };
    let mut req = http
        .post(&endpoint.url)
        .header("content-type", "application/json")
        .header("user-agent", "HostingOS-Webhooks/1")
        .header("x-hostingos-event", &p.event)
        .header("x-hostingos-event-id", p.event_id.to_string());
    if let Some(secret) = secret {
        req = req.header(
            "x-hostingos-signature",
            webhook_signature(secret.expose_str(), Utc::now(), &body),
        );
    }
    let res = req.body(body).send().await;
    match res {
        Ok(r) => {
            let status = r.status();
            let ok = status.is_success();
            webhooks::record_delivery(
                b.pool(),
                tenant_id,
                endpoint.id,
                Some(i32::from(status.as_u16())),
                ok,
            )
            .await
            .map_err(HandlerError::from)?;
            if ok {
                done(json!({"status": status.as_u16()}))
            } else {
                Err(HandlerError::Retry {
                    message: format!("endpoint answered HTTP {}", status.as_u16()),
                    after: None,
                })
            }
        }
        Err(e) => {
            webhooks::record_delivery(b.pool(), tenant_id, endpoint.id, None, false)
                .await
                .map_err(HandlerError::from)?;
            Err(HandlerError::Retry {
                message: format!("delivery failed: {e}"),
                after: None,
            })
        }
    }
}
