//! Service lifecycle after provisioning: suspend / unsuspend / terminate
//! (provider + state machine), plan changes with prorated invoices,
//! cancellation requests, and client-area wrappers for domain and VM actions.
//!
//! Client-facing wrappers take `client_id: Option<ClientId>`: `Some` (client
//! area) requires the object to belong to that client and the service to be
//! `active`; `None` is a staff call (only the state rules apply).

use chrono::{Days, Months, Utc};
use hos_core::CoreError;
use hos_core::domain::{DomainStatus, IntegrationCategory, InvoiceStatus, ServiceStatus};
use hos_core::error::IntegrationError;
use hos_core::ids::{
    ClientId, DomainId, IntegrationId, InvoiceId, ProductId, ServiceId, TenantId, UserId,
};
use hos_core::integrations::panel::{AccountRef, SsoLogin, SsoTarget};
use hos_core::integrations::virt::{
    ConsoleAccess, PowerAction, SnapshotInfo, SnapshotRequest, TaskRef, VmRef, VmReinstall,
    VmResize, VmStatus,
};
use hos_core::money::BillingCycle;
use hos_core::secret::SecretString;
use hos_db::enums::{ActorKind, CancellationType, CreditKind, InvoiceItemType};
use hos_db::repo::billing_support::{self, NewServiceUpgrade, ServiceUpgradeRow};
use hos_db::repo::domains::{self, DomainRow};
use hos_db::repo::invoices::{self, NewInvoice, NewInvoiceItem};
use hos_db::repo::services::{self, NewServiceEvent, ServiceRow, VmRow};
use hos_db::repo::{catalog, clients, payments};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::checkout::billing_snapshot;
use crate::jobs::{self, JobKind, PollVmTaskPayload, ServicePayload, VmTaskPurpose};
use crate::pricing::{self, Proration, QuoteItem, QuoteRequest};
use crate::provisioning::{RunContext, module_settings};
use crate::secrets;
use crate::settings::BillingSettings;
use crate::{Billing, BillingResult};

/// Outcome of a lifecycle job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LifecycleOutcome {
    Done,
    /// Nothing to do in the current state.
    Skipped {
        reason: String,
    },
    /// The provider call failed permanently; staff were notified.
    Failed {
        reason: String,
    },
}

/// A prorated plan change offer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanChangeQuote {
    pub service_id: ServiceId,
    pub from_product_id: ProductId,
    pub to_product_id: ProductId,
    pub billing_cycle: BillingCycle,
    /// New recurring price per period (excl. tax).
    pub new_recurring_amount: Decimal,
    pub proration: Proration,
}

/// Result of [`Billing::order_plan_change`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanChangeOrder {
    pub upgrade_id: Uuid,
    /// Invoice for the prorated difference (upgrades); `None` when nothing is due.
    pub invoice_id: Option<InvoiceId>,
    /// Prorated credit of a downgrade, granted once the change is applied.
    pub credited: Decimal,
}

fn ev(e: &str, msg: &str, ctx: RunContext) -> NewServiceEvent {
    NewServiceEvent {
        job_id: ctx.job_id,
        actor_kind: Some(ActorKind::System),
        ..NewServiceEvent::info(e, msg)
    }
}

fn account_ref(svc: &ServiceRow) -> Option<AccountRef> {
    svc.username.clone().map(|u| AccountRef {
        username: u,
        domain: svc.domain.clone(),
        id: svc.external_id.clone(),
    })
}

pub(crate) fn vm_ref(vm: &VmRow) -> VmRef {
    VmRef {
        node: vm.node.clone(),
        id: vm.external_id.clone(),
    }
}

/// OS template set by staff in a product's module settings (the keys
/// provisioning reads). Client-chosen configurable options are not trusted here.
fn configured_template(module_config: &Value) -> Option<String> {
    crate::provisioning::TEMPLATE_KEYS.iter().find_map(|k| {
        module_config
            .get(*k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

impl Billing {
    /// Loads a service for an action; `client_id` restricts it to its owner
    /// (others → `NotFound`) and requires an active service.
    pub async fn authorize_service(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<ServiceRow> {
        let svc = services::get_service(self.pool(), tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        if let Some(c) = client_id {
            if svc.client_id != c {
                return Err(CoreError::not_found("service", service_id));
            }
            if svc.status != ServiceStatus::Active {
                return Err(CoreError::Forbidden(format!(
                    "the service is {}",
                    svc.status
                )));
            }
        }
        Ok(svc)
    }

    async fn authorize_domain(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
    ) -> BillingResult<DomainRow> {
        let d = domains::get_domain(self.pool(), tenant_id, domain_id)
            .await?
            .ok_or_else(|| CoreError::not_found("domain", domain_id))?;
        if let Some(c) = client_id {
            if d.client_id != c {
                return Err(CoreError::not_found("domain", domain_id));
            }
            if !matches!(d.status, DomainStatus::Active | DomainStatus::Grace) {
                return Err(CoreError::Forbidden(format!("the domain is {}", d.status)));
            }
        }
        Ok(d)
    }

    async fn integration_category(
        &self,
        tenant_id: TenantId,
        svc: &ServiceRow,
    ) -> BillingResult<Option<(IntegrationId, IntegrationCategory)>> {
        match svc.integration_id {
            None => Ok(None),
            Some(id) => {
                let l = self.registry().get(tenant_id, id).await?;
                Ok(Some((id, l.category)))
            }
        }
    }

    // -----------------------------------------------------------------------
    // requests (queue jobs)
    // -----------------------------------------------------------------------

    /// Queues a suspension (staff action or automation).
    pub async fn request_suspend(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        reason: &str,
        actor: Option<UserId>,
    ) -> BillingResult<()> {
        self.request_service_job(
            tenant_id,
            service_id,
            JobKind::SuspendService,
            Some(reason),
            actor,
        )
        .await
    }

    /// Queues an unsuspension.
    pub async fn request_unsuspend(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        actor: Option<UserId>,
    ) -> BillingResult<()> {
        self.request_service_job(
            tenant_id,
            service_id,
            JobKind::UnsuspendService,
            None,
            actor,
        )
        .await
    }

    /// Queues a termination.
    pub async fn request_terminate(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        actor: Option<UserId>,
    ) -> BillingResult<()> {
        self.request_service_job(
            tenant_id,
            service_id,
            JobKind::TerminateService,
            None,
            actor,
        )
        .await
    }

    async fn request_service_job(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        kind: JobKind,
        reason: Option<&str>,
        actor: Option<UserId>,
    ) -> BillingResult<()> {
        let mut conn = self.pool().acquire().await?;
        services::get_service(&mut *conn, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        jobs::enqueue_service_job(
            &mut conn,
            tenant_id,
            kind,
            &ServicePayload {
                service_id,
                reason: reason.map(str::to_owned),
                actor,
            },
            None,
        )
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // job steps
    // -----------------------------------------------------------------------

    async fn lifecycle_error(
        &self,
        tenant_id: TenantId,
        svc: &ServiceRow,
        what: &str,
        err: CoreError,
        ctx: RunContext,
    ) -> BillingResult<LifecycleOutcome> {
        let retryable = matches!(&err, CoreError::Integration(e) if e.is_retryable())
            || matches!(err, CoreError::Database(_));
        if retryable && !ctx.final_attempt {
            return Err(err);
        }
        let reason = err.to_string();
        let mut conn = self.pool().acquire().await?;
        services::log_event(
            &mut *conn,
            tenant_id,
            svc.id,
            &NewServiceEvent {
                job_id: ctx.job_id,
                data: json!({"error": reason}),
                ..NewServiceEvent::error(&format!("{what}_failed"), &reason)
            },
        )
        .await?;
        self.notify_staff(
            &mut conn,
            tenant_id,
            "service.action_failed",
            &format!(
                "{what} failed for {}",
                svc.domain.as_deref().unwrap_or("a service")
            ),
            Some(&reason),
            Some(&format!("/admin/services/{}", svc.id)),
            &json!({"service_id": svc.id, "action": what}),
        )
        .await?;
        Ok(LifecycleOutcome::Failed { reason })
    }

    /// Suspends an active service at its provider, then in the database
    /// (the `suspend_service` job).
    pub async fn suspend_service(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        reason: Option<&str>,
        ctx: RunContext,
    ) -> BillingResult<LifecycleOutcome> {
        let svc = self.authorize_service(tenant_id, service_id, None).await?;
        if svc.status != ServiceStatus::Active {
            return Ok(LifecycleOutcome::Skipped {
                reason: format!("service is {}", svc.status),
            });
        }
        let reason = reason.unwrap_or("suspended by staff");
        let call: BillingResult<()> = async {
            match self.integration_category(tenant_id, &svc).await? {
                Some((id, IntegrationCategory::Panel)) => {
                    if let Some(acc) = account_ref(&svc) {
                        let panel = self.registry().panel(tenant_id, id).await?;
                        panel.suspend(&acc, Some(reason)).await?;
                    }
                }
                Some((id, IntegrationCategory::Virt)) => {
                    if let Some(vm) =
                        services::vm_for_service(self.pool(), tenant_id, svc.id).await?
                    {
                        let virt = self.registry().virt(tenant_id, id).await?;
                        virt.suspend(&vm_ref(&vm)).await?;
                        services::set_vm_status(self.pool(), tenant_id, vm.id, VmStatus::Suspended)
                            .await?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        .await;
        if let Err(e) = call {
            return self
                .lifecycle_error(tenant_id, &svc, "suspend", e, ctx)
                .await;
        }
        let mut tx = self.pool().begin().await?;
        if !services::transition_service(
            &mut *tx,
            tenant_id,
            service_id,
            ServiceStatus::Active,
            ServiceStatus::Suspended,
            Some(reason),
        )
        .await?
        {
            tx.commit().await?;
            return Ok(LifecycleOutcome::Skipped {
                reason: "service changed concurrently".into(),
            });
        }
        for addon in services::service_addons(&mut *tx, tenant_id, service_id).await? {
            if addon.status == ServiceStatus::Active {
                services::set_service_addon_status(
                    &mut *tx,
                    tenant_id,
                    addon.id,
                    ServiceStatus::Suspended,
                )
                .await?;
            }
        }
        services::log_event(
            &mut *tx,
            tenant_id,
            service_id,
            &NewServiceEvent {
                from_status: Some(ServiceStatus::Active),
                to_status: Some(ServiceStatus::Suspended),
                ..ev("suspended", reason, ctx)
            },
        )
        .await?;
        let product = catalog::get_product(&mut *tx, tenant_id, svc.product_id).await?;
        let url = self.tenant_url(tenant_id).await?;
        self.queue_client_email(
            &mut tx,
            tenant_id,
            svc.client_id,
            hos_mail::defaults::keys::SERVICE_SUSPENDED,
            json!({
                "service": {
                    "name": product.map(|p| p.name).unwrap_or_default(),
                    "domain": svc.domain.clone().unwrap_or_default(),
                    "username": svc.username.clone().unwrap_or_default(),
                    "reason": reason,
                },
                "action_url": format!("{url}/client/invoices"),
            }),
            Some(("service", *service_id.as_uuid())),
        )
        .await?;
        self.emit_event(
            &mut tx,
            tenant_id,
            "service.suspended",
            json!({"service_id": service_id, "reason": reason}),
        )
        .await?;
        tx.commit().await?;
        Ok(LifecycleOutcome::Done)
    }

    /// Lifts a suspension (the `unsuspend_service` job).
    pub async fn unsuspend_service(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        ctx: RunContext,
    ) -> BillingResult<LifecycleOutcome> {
        let svc = self.authorize_service(tenant_id, service_id, None).await?;
        if svc.status != ServiceStatus::Suspended {
            return Ok(LifecycleOutcome::Skipped {
                reason: format!("service is {}", svc.status),
            });
        }
        let call: BillingResult<()> = async {
            match self.integration_category(tenant_id, &svc).await? {
                Some((id, IntegrationCategory::Panel)) => {
                    if let Some(acc) = account_ref(&svc) {
                        self.registry()
                            .panel(tenant_id, id)
                            .await?
                            .unsuspend(&acc)
                            .await?;
                    }
                }
                Some((id, IntegrationCategory::Virt)) => {
                    if let Some(vm) =
                        services::vm_for_service(self.pool(), tenant_id, svc.id).await?
                    {
                        self.registry()
                            .virt(tenant_id, id)
                            .await?
                            .unsuspend(&vm_ref(&vm))
                            .await?;
                        services::set_vm_status(self.pool(), tenant_id, vm.id, VmStatus::Running)
                            .await?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        .await;
        if let Err(e) = call {
            return self
                .lifecycle_error(tenant_id, &svc, "unsuspend", e, ctx)
                .await;
        }
        let mut tx = self.pool().begin().await?;
        services::transition_service(
            &mut *tx,
            tenant_id,
            service_id,
            ServiceStatus::Suspended,
            ServiceStatus::Active,
            None,
        )
        .await?;
        for addon in services::service_addons(&mut *tx, tenant_id, service_id).await? {
            if addon.status == ServiceStatus::Suspended {
                services::set_service_addon_status(
                    &mut *tx,
                    tenant_id,
                    addon.id,
                    ServiceStatus::Active,
                )
                .await?;
            }
        }
        services::log_event(
            &mut *tx,
            tenant_id,
            service_id,
            &NewServiceEvent {
                from_status: Some(ServiceStatus::Suspended),
                to_status: Some(ServiceStatus::Active),
                ..ev("unsuspended", "service reactivated", ctx)
            },
        )
        .await?;
        self.emit_event(
            &mut tx,
            tenant_id,
            "service.unsuspended",
            json!({"service_id": service_id}),
        )
        .await?;
        tx.commit().await?;
        Ok(LifecycleOutcome::Done)
    }

    /// Removes the account / VM and marks the service `terminated` (the
    /// `terminate_service` job). Provider `NotFound` counts as already gone.
    /// Open invoices billing only this service are cancelled.
    pub async fn terminate_service(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        ctx: RunContext,
    ) -> BillingResult<LifecycleOutcome> {
        let svc = self.authorize_service(tenant_id, service_id, None).await?;
        if !matches!(
            svc.status,
            ServiceStatus::Active | ServiceStatus::Suspended | ServiceStatus::Failed
        ) {
            return Ok(LifecycleOutcome::Skipped {
                reason: format!("service is {}", svc.status),
            });
        }
        let gone = |r: Result<(), IntegrationError>| match r {
            Ok(()) | Err(IntegrationError::NotFound(_)) => Ok(()),
            Err(e) => Err(CoreError::from(e)),
        };
        let mut deleted_vm = None;
        let call: BillingResult<()> = async {
            match self.integration_category(tenant_id, &svc).await? {
                Some((id, IntegrationCategory::Panel)) => {
                    if let (Some(acc), Some(_)) = (account_ref(&svc), &svc.external_id) {
                        let panel = self.registry().panel(tenant_id, id).await?;
                        gone(panel.terminate(&acc).await)?;
                    }
                }
                Some((id, IntegrationCategory::Virt)) => {
                    if let Some(vm) =
                        services::vm_for_service(self.pool(), tenant_id, svc.id).await?
                    {
                        let virt = self.registry().virt(tenant_id, id).await?;
                        gone(virt.delete_vm(&vm_ref(&vm)).await.map(|_| ()))?;
                        deleted_vm = Some(vm.id);
                    }
                }
                _ => {}
            }
            Ok(())
        }
        .await;
        if let Err(e) = call {
            return self
                .lifecycle_error(tenant_id, &svc, "terminate", e, ctx)
                .await;
        }
        let mut tx = self.pool().begin().await?;
        if let Some(vm) = deleted_vm {
            services::mark_vm_deleted(&mut *tx, tenant_id, vm).await?;
            // The root password opened the machine that is now gone.
            hos_db::repo::vm_ops::clear_stored_vm_password(&mut tx, tenant_id, service_id).await?;
        }
        services::transition_service(
            &mut *tx,
            tenant_id,
            service_id,
            svc.status,
            ServiceStatus::Terminated,
            None,
        )
        .await?;
        services::release_service_ips(&mut *tx, tenant_id, service_id).await?;
        for addon in services::service_addons(&mut *tx, tenant_id, service_id).await? {
            if !addon.status.is_terminal() {
                services::set_service_addon_status(
                    &mut *tx,
                    tenant_id,
                    addon.id,
                    ServiceStatus::Terminated,
                )
                .await?;
            }
        }
        for inv in
            billing_support::open_invoices_for_service(&mut *tx, tenant_id, service_id).await?
        {
            let items = invoices::invoice_items(&mut *tx, tenant_id, inv.id).await?;
            let only_this = items.iter().all(|i| {
                i.service_id == Some(service_id)
                    || matches!(
                        i.item_type,
                        InvoiceItemType::LateFee | InvoiceItemType::Discount
                    )
            });
            if only_this && inv.amount_paid.is_zero() && inv.credit_applied.is_zero() {
                invoices::cancel_invoice(&mut *tx, tenant_id, inv.id).await?;
            }
        }
        services::log_event(
            &mut *tx,
            tenant_id,
            service_id,
            &NewServiceEvent {
                from_status: Some(svc.status),
                to_status: Some(ServiceStatus::Terminated),
                ..ev("terminated", "service terminated", ctx)
            },
        )
        .await?;
        self.emit_event(
            &mut tx,
            tenant_id,
            "service.terminated",
            json!({"service_id": service_id}),
        )
        .await?;
        tx.commit().await?;
        Ok(LifecycleOutcome::Done)
    }

    // -----------------------------------------------------------------------
    // cancellation
    // -----------------------------------------------------------------------

    /// Records a cancellation request. `immediate` queues termination now;
    /// `end_of_period` lets the daily automation terminate it on the due date
    /// (and stops renewal invoices). Open renewal invoices are cancelled.
    pub async fn request_cancellation(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        kind: CancellationType,
        reason: Option<&str>,
    ) -> BillingResult<()> {
        let svc = services::get_service(self.pool(), tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        if client_id.is_some_and(|c| c != svc.client_id) {
            return Err(CoreError::not_found("service", service_id));
        }
        let mut tx = self.pool().begin().await?;
        if svc.status == ServiceStatus::Pending {
            services::transition_service(
                &mut *tx,
                tenant_id,
                service_id,
                ServiceStatus::Pending,
                ServiceStatus::Cancelled,
                None,
            )
            .await?;
        } else if !services::request_cancellation(&mut *tx, tenant_id, service_id, kind, reason)
            .await?
        {
            return Err(CoreError::Conflict(format!(
                "a {} service cannot be cancelled",
                svc.status
            )));
        }
        for inv in
            billing_support::open_invoices_for_service(&mut *tx, tenant_id, service_id).await?
        {
            let items = invoices::invoice_items(&mut *tx, tenant_id, inv.id).await?;
            let renewal_only = items.iter().all(|i| {
                i.service_id == Some(service_id)
                    && svc.next_due_date.is_some()
                    && i.period_start == svc.next_due_date
            });
            if renewal_only && inv.amount_paid.is_zero() && inv.credit_applied.is_zero() {
                invoices::cancel_invoice(&mut *tx, tenant_id, inv.id).await?;
            }
        }
        services::log_event(
            &mut *tx,
            tenant_id,
            service_id,
            &NewServiceEvent {
                actor_kind: Some(if client_id.is_some() {
                    ActorKind::User
                } else {
                    ActorKind::System
                }),
                data: json!({"type": kind, "reason": reason}),
                ..NewServiceEvent::info("cancellation_requested", reason.unwrap_or(""))
            },
        )
        .await?;
        if kind == CancellationType::Immediate && svc.status != ServiceStatus::Pending {
            jobs::enqueue_service_job(
                &mut tx,
                tenant_id,
                JobKind::TerminateService,
                &ServicePayload {
                    service_id,
                    reason: Some("cancelled".into()),
                    actor: None,
                },
                Some(format!("terminate:{service_id}")),
            )
            .await?;
        }
        self.notify_staff(
            &mut tx,
            tenant_id,
            "service.cancellation",
            &format!("Cancellation requested ({kind})"),
            reason,
            Some(&format!("/admin/services/{service_id}")),
            &json!({"service_id": service_id}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // plan changes
    // -----------------------------------------------------------------------

    /// Prices an upgrade / downgrade for the rest of the current period.
    pub async fn quote_plan_change(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        to_product_id: ProductId,
    ) -> BillingResult<PlanChangeQuote> {
        let svc = self
            .authorize_service(tenant_id, service_id, client_id)
            .await?;
        let mut conn = self.pool().acquire().await?;
        self.quote_plan_change_on(&mut conn, tenant_id, &svc, to_product_id)
            .await
    }

    /// [`Billing::quote_plan_change`] for a loaded (ideally locked) service.
    async fn quote_plan_change_on(
        &self,
        conn: &mut sqlx::PgConnection,
        tenant_id: TenantId,
        svc: &ServiceRow,
        to_product_id: ProductId,
    ) -> BillingResult<PlanChangeQuote> {
        let service_id = svc.id;
        if svc.status != ServiceStatus::Active {
            return Err(CoreError::Conflict(format!(
                "the service is {}",
                svc.status
            )));
        }
        let product = catalog::get_product(&mut *conn, tenant_id, svc.product_id)
            .await?
            .ok_or_else(|| CoreError::not_found("product", svc.product_id))?;
        if to_product_id == svc.product_id || !product.upgrade_product_ids.contains(&to_product_id)
        {
            return Err(CoreError::validation("this plan change is not offered"));
        }
        let cycle = svc.billing_cycle;
        let (Some(months), Some(end)) = (cycle.months(), svc.next_due_date) else {
            return Err(CoreError::validation(
                "one-time services cannot change plan",
            ));
        };
        let start = end
            .checked_sub_months(Months::new(months))
            .ok_or_else(|| CoreError::Internal("invalid billing period".into()))?;
        let quote = self
            .quote_on(
                &mut *conn,
                tenant_id,
                &QuoteRequest {
                    currency: svc.currency,
                    items: vec![QuoteItem::product(to_product_id, cycle)],
                    coupon_code: None,
                    client_id: Some(svc.client_id),
                    country_code: None,
                    region: None,
                },
            )
            .await?;
        let new_recurring = quote
            .lines
            .first()
            .map(|l| l.list_recurring_amount)
            .ok_or_else(|| CoreError::Internal("empty plan quote".into()))?;
        let proration = pricing::prorate(
            svc.recurring_amount,
            new_recurring,
            start,
            end,
            self.clock().today(),
            svc.currency,
        )?;
        Ok(PlanChangeQuote {
            service_id,
            from_product_id: svc.product_id,
            to_product_id,
            billing_cycle: cycle,
            new_recurring_amount: new_recurring,
            proration,
        })
    }

    /// Orders a plan change: an upgrade invoices the prorated difference (the
    /// change is applied by a `change_package` job once it is paid); a
    /// downgrade is queued right away and its prorated credit is granted only
    /// when the `change_package` job applied it ([`Billing::apply_plan_change`]).
    ///
    /// The quote is computed under the service row lock, and a service has at
    /// most one pending plan change (`Conflict` otherwise).
    pub async fn order_plan_change(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        to_product_id: ProductId,
    ) -> BillingResult<PlanChangeOrder> {
        let mut tx = self.pool().begin().await?;
        let svc = billing_support::lock_service(&mut tx, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        if client_id.is_some_and(|c| c != svc.client_id) {
            return Err(CoreError::not_found("service", service_id));
        }
        if let Some(pending) =
            billing_support::pending_upgrade_for_service(&mut *tx, tenant_id, service_id).await?
        {
            // An upgrade whose invoice was cancelled / refunded (or deleted)
            // will never be paid: it no longer blocks a new change.
            let stale = match pending.invoice_id {
                Some(id) => invoices::get_invoice(&mut *tx, tenant_id, id)
                    .await?
                    .is_none_or(|i| !i.status.is_open() && i.status != InvoiceStatus::Paid),
                None => pending.amount > Decimal::ZERO,
            };
            if !stale {
                return Err(CoreError::Conflict(
                    "a plan change for this service is already pending; wait until it is \
                     applied (or its invoice is paid or cancelled)"
                        .into(),
                ));
            }
            billing_support::finish_service_upgrade(
                &mut *tx,
                tenant_id,
                pending.id,
                "cancelled",
                Some("superseded: its invoice was not paid"),
            )
            .await?;
        }
        let q = self
            .quote_plan_change_on(&mut tx, tenant_id, &svc, to_product_id)
            .await?;
        let client = clients::get_client(&mut *tx, tenant_id, svc.client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", svc.client_id))?;
        let to_product = catalog::get_product(&mut *tx, tenant_id, to_product_id)
            .await?
            .ok_or_else(|| CoreError::not_found("product", to_product_id))?;
        let today = self.clock().today();
        let mut invoice_id = None;
        let mut credited = Decimal::ZERO;
        if q.proration.amount > Decimal::ZERO {
            let settings = BillingSettings::load(&mut *tx, tenant_id).await?;
            let taxes = pricing::tax_rates(
                &mut tx,
                tenant_id,
                &pricing::TaxContext::for_client(&client, &settings),
            )
            .await?;
            let description = format!(
                "{} → {} ({} / {} days)",
                catalog::get_product(&mut *tx, tenant_id, svc.product_id)
                    .await?
                    .map(|p| p.name)
                    .unwrap_or_default(),
                to_product.name,
                q.proration.remaining_days,
                q.proration.period_days
            );
            let inv = invoices::create_invoice(
                &mut tx,
                tenant_id,
                &NewInvoice {
                    client_id: client.id,
                    currency: svc.currency,
                    issue_date: today,
                    due_date: today,
                    issue: true,
                    payment_integration_id: client.default_gateway_id,
                    notes: None,
                    billing_snapshot: billing_snapshot(&client),
                    items: vec![NewInvoiceItem {
                        item_type: InvoiceItemType::Upgrade,
                        service_id: Some(service_id),
                        taxable: to_product.is_taxable,
                        ..NewInvoiceItem::manual(&description, q.proration.amount)
                    }],
                    taxes: taxes
                        .iter()
                        .map(|t| invoices::TaxLineInput {
                            tax_rule_id: t.tax_rule_id,
                            name: t.name.clone(),
                            rate: t.rate,
                            is_compound: t.is_compound,
                        })
                        .collect(),
                },
            )
            .await?;
            invoice_id = Some(inv.id);
        } else if q.proration.amount < Decimal::ZERO {
            // Granted by `apply_plan_change` once the change is applied.
            credited = -q.proration.amount;
        }
        let up = billing_support::create_service_upgrade(
            &mut *tx,
            tenant_id,
            &NewServiceUpgrade {
                service_id,
                invoice_id,
                from_product_id: svc.product_id,
                to_product_id,
                from_cycle: svc.billing_cycle,
                to_cycle: q.billing_cycle,
                currency: svc.currency,
                amount: q.proration.amount,
                new_recurring_amount: q.new_recurring_amount,
                config_selections: svc.config_selections.clone(),
            },
        )
        .await?;
        services::log_event(
            &mut *tx,
            tenant_id,
            service_id,
            &NewServiceEvent {
                data: json!({"upgrade_id": up.id, "amount": q.proration.amount}),
                ..NewServiceEvent::info("plan_change_ordered", &format!("to {}", to_product.name))
            },
        )
        .await?;
        match invoice_id {
            Some(id) => {
                let inv = invoices::get_invoice(&mut *tx, tenant_id, id)
                    .await?
                    .ok_or_else(|| CoreError::not_found("invoice", id))?;
                let settings = BillingSettings::load(&mut *tx, tenant_id).await?;
                let mut paid = false;
                if settings.apply_credit_automatically {
                    let (after, _) =
                        payments::apply_credit_to_invoice(&mut tx, tenant_id, id, None, None)
                            .await?;
                    paid = after.status == InvoiceStatus::Paid;
                }
                if paid {
                    self.after_invoice_paid(&mut tx, tenant_id, id).await?;
                } else {
                    self.queue_invoice_created_email(&mut tx, tenant_id, &inv)
                        .await?;
                }
            }
            None => {
                jobs::enqueue(
                    &mut tx,
                    jobs::JobRequest::new(
                        JobKind::ChangePackage,
                        Some(tenant_id),
                        &jobs::ChangePackagePayload {
                            service_id,
                            upgrade_id: up.id,
                        },
                    )
                    .dedupe(format!("change_package:{}", up.id)),
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(PlanChangeOrder {
            upgrade_id: up.id,
            invoice_id,
            credited,
        })
    }

    /// Applies a paid upgrade / ordered downgrade at the provider and in the
    /// database (the `change_package` job). A downgrade's prorated credit is
    /// granted in the same transaction that records the change as applied; a
    /// change that fails permanently is marked `failed` and grants nothing.
    pub async fn apply_plan_change(
        &self,
        tenant_id: TenantId,
        upgrade_id: Uuid,
        ctx: RunContext,
    ) -> BillingResult<LifecycleOutcome> {
        let Some(up): Option<ServiceUpgradeRow> =
            billing_support::get_service_upgrade(self.pool(), tenant_id, upgrade_id).await?
        else {
            return Err(CoreError::not_found("service_upgrade", upgrade_id));
        };
        if up.status != "pending" {
            return Ok(LifecycleOutcome::Skipped {
                reason: format!("plan change is {}", up.status),
            });
        }
        let svc = self
            .authorize_service(tenant_id, up.service_id, None)
            .await?;
        if svc.status.is_terminal() {
            billing_support::finish_service_upgrade(
                self.pool(),
                tenant_id,
                up.id,
                "cancelled",
                Some("service ended"),
            )
            .await?;
            return Ok(LifecycleOutcome::Skipped {
                reason: format!("service is {}", svc.status),
            });
        }
        let to_product = catalog::get_product(self.pool(), tenant_id, up.to_product_id)
            .await?
            .ok_or_else(|| CoreError::not_found("product", up.to_product_id))?;
        let mut manual = false;
        let call: BillingResult<()> = async {
            let settings = module_settings(&to_product, &svc);
            match self.integration_category(tenant_id, &svc).await? {
                Some((id, IntegrationCategory::Panel)) => {
                    let package =
                        settings
                            .get("package")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                CoreError::validation("target product has no panel package")
                            })?;
                    if let Some(acc) = account_ref(&svc) {
                        self.registry()
                            .panel(tenant_id, id)
                            .await?
                            .change_package(&acc, package)
                            .await?;
                    }
                }
                Some((id, IntegrationCategory::Virt)) => {
                    if let Some(vm) =
                        services::vm_for_service(self.pool(), tenant_id, svc.id).await?
                    {
                        let num = |k: &str| {
                            settings
                                .get(k)
                                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
                        };
                        let resize = VmResize {
                            cpu_cores: num("cpu_cores").and_then(|v| u32::try_from(v).ok()),
                            memory_mb: num("memory_mb"),
                            disk_gb: num("disk_gb"),
                            plan: settings
                                .get("plan")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            allow_restart: true,
                        };
                        let task = self
                            .registry()
                            .virt(tenant_id, id)
                            .await?
                            .resize(&vm_ref(&vm), &resize)
                            .await?;
                        if let Some(task) = task {
                            self.queue_vm_poll(tenant_id, svc.id, Some(vm.id), task, None)
                                .await?;
                        }
                    }
                }
                _ => manual = true,
            }
            Ok(())
        }
        .await;
        if let Err(e) = call {
            let out = self
                .lifecycle_error(tenant_id, &svc, "plan_change", e, ctx)
                .await?;
            if let LifecycleOutcome::Failed { reason } = &out {
                billing_support::finish_service_upgrade(
                    self.pool(),
                    tenant_id,
                    up.id,
                    "failed",
                    Some(reason),
                )
                .await?;
            }
            return Ok(out);
        }
        let mut tx = self.pool().begin().await?;
        // Conditional on `pending` (row lock): a concurrent run applies it once.
        if !billing_support::finish_service_upgrade(&mut *tx, tenant_id, up.id, "applied", None)
            .await?
        {
            tx.rollback().await?;
            return Ok(LifecycleOutcome::Skipped {
                reason: "plan change is no longer pending".into(),
            });
        }
        billing_support::change_service_plan(
            &mut *tx,
            tenant_id,
            svc.id,
            up.to_product_id,
            up.to_cycle,
            up.new_recurring_amount,
            None,
        )
        .await?;
        if up.amount < Decimal::ZERO {
            payments::adjust_credit(
                &mut tx,
                tenant_id,
                svc.client_id,
                -up.amount,
                up.currency,
                CreditKind::Manual,
                &format!("Downgrade credit ({})", to_product.name),
                None,
            )
            .await?;
        }
        services::log_event(
            &mut *tx,
            tenant_id,
            svc.id,
            &NewServiceEvent {
                data: json!({"upgrade_id": up.id, "manual": manual}),
                ..ev("plan_changed", &format!("now on {}", to_product.name), ctx)
            },
        )
        .await?;
        if manual {
            self.notify_staff(
                &mut tx,
                tenant_id,
                "service.manual_plan_change",
                &format!("Apply plan {} manually", to_product.name),
                None,
                Some(&format!("/admin/services/{}", svc.id)),
                &json!({"service_id": svc.id}),
            )
            .await?;
        }
        self.emit_event(
            &mut tx,
            tenant_id,
            "service.plan_changed",
            json!({"service_id": svc.id, "product_id": up.to_product_id}),
        )
        .await?;
        tx.commit().await?;
        Ok(LifecycleOutcome::Done)
    }

    // -----------------------------------------------------------------------
    // credentials / SSO
    // -----------------------------------------------------------------------

    /// Decrypted service password (client area "show password", staff view).
    pub async fn service_password(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<Option<SecretString>> {
        let svc = self
            .authorize_service(tenant_id, service_id, client_id)
            .await?;
        svc.password_enc
            .as_deref()
            .map(|sealed| {
                secrets::open(
                    self.secret_box(),
                    sealed,
                    &secrets::service_password_aad(tenant_id, service_id),
                )
            })
            .transpose()
    }

    /// One-time panel login URL for a hosting service.
    pub async fn panel_sso(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        target: &SsoTarget,
    ) -> BillingResult<SsoLogin> {
        let svc = self
            .authorize_service(tenant_id, service_id, client_id)
            .await?;
        let (Some((id, IntegrationCategory::Panel)), Some(acc)) = (
            self.integration_category(tenant_id, &svc).await?,
            account_ref(&svc),
        ) else {
            return Err(CoreError::validation("this service has no control panel"));
        };
        Ok(self
            .registry()
            .panel(tenant_id, id)
            .await?
            .sso_url(&acc, target)
            .await?)
    }

    /// Sets a new control-panel password for a hosting service and stores
    /// it (encrypted) on the service.
    pub async fn change_service_password(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        password: &SecretString,
        actor: Option<UserId>,
    ) -> BillingResult<()> {
        let pw = password.expose_str();
        if pw.chars().count() < 8 || pw.chars().count() > 128 {
            return Err(CoreError::validation(
                "the password must be 8-128 characters long",
            ));
        }
        let svc = self
            .authorize_service(tenant_id, service_id, client_id)
            .await?;
        let (Some((id, IntegrationCategory::Panel)), Some(acc)) = (
            self.integration_category(tenant_id, &svc).await?,
            account_ref(&svc),
        ) else {
            return Err(CoreError::validation("this service has no control panel"));
        };
        self.registry()
            .panel(tenant_id, id)
            .await?
            .change_password(&acc, password)
            .await?;
        let sealed = secrets::seal(
            self.secret_box(),
            pw,
            &secrets::service_password_aad(tenant_id, service_id),
        )?;
        services::update_provisioning(
            self.pool(),
            tenant_id,
            service_id,
            &services::ProvisioningUpdate {
                password_enc: Some(sealed),
                ..services::ProvisioningUpdate::default()
            },
        )
        .await?;
        services::log_event(
            self.pool(),
            tenant_id,
            service_id,
            &NewServiceEvent {
                actor_kind: actor.map(|_| ActorKind::User),
                actor_id: actor.map(Into::into),
                ..NewServiceEvent::info("password_changed", "control panel password changed")
            },
        )
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // VM actions
    // -----------------------------------------------------------------------

    pub(crate) async fn vm_of(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<(
        ServiceRow,
        VmRow,
        std::sync::Arc<dyn hos_core::integrations::VirtProvider>,
    )> {
        let svc = self
            .authorize_service(tenant_id, service_id, client_id)
            .await?;
        let Some((id, IntegrationCategory::Virt)) =
            self.integration_category(tenant_id, &svc).await?
        else {
            return Err(CoreError::validation(
                "this service is not a virtual server",
            ));
        };
        let vm = services::vm_for_service(self.pool(), tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("vm", service_id))?;
        Ok((svc, vm, self.registry().virt(tenant_id, id).await?))
    }

    pub(crate) async fn queue_vm_poll(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        vm_id: Option<hos_core::ids::VmId>,
        task: TaskRef,
        operation_id: Option<Uuid>,
    ) -> BillingResult<()> {
        let mut conn = self.pool().acquire().await?;
        jobs::enqueue_poll_vm_task(
            &mut conn,
            tenant_id,
            &PollVmTaskPayload {
                service_id,
                vm_id,
                task,
                purpose: VmTaskPurpose::Other,
                operation_id,
            },
            Utc::now()
                + chrono::Duration::from_std(self.config().vm_poll_interval).unwrap_or_default(),
        )
        .await?;
        Ok(())
    }

    async fn vm_action_log(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        what: &str,
    ) -> BillingResult<()> {
        services::log_event(
            self.pool(),
            tenant_id,
            service_id,
            &NewServiceEvent::info("vm_action", what),
        )
        .await?;
        Ok(())
    }

    /// Power action on the service's VM.
    pub async fn vm_power(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        action: PowerAction,
    ) -> BillingResult<()> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        if svc.status == ServiceStatus::Suspended {
            return Err(CoreError::Forbidden("the service is suspended".into()));
        }
        let res = virt.power(&vm_ref(&vm), action).await;
        let task = self
            .track_vm_op(tenant_id, &vm, "power", &action.to_string(), res)
            .await?;
        self.vm_action_log(tenant_id, service_id, &format!("power {action}"))
            .await?;
        match task {
            Some(_) => Ok(()),
            None => {
                let info = virt.get_vm(&vm_ref(&vm)).await?;
                services::set_vm_status(self.pool(), tenant_id, vm.id, info.status).await?;
                Ok(())
            }
        }
    }

    /// Reinstalls the OS (destroys data); the new root password is stored encrypted.
    ///
    /// `template` must be one of the templates offered for the service — the
    /// hypervisor's templates on the VM's node ([`Billing::vm_templates`], the
    /// client-area list) or the product's configured OS template — for every
    /// caller (client area, REST API, staff): a provider would otherwise clone
    /// whatever disk / VM id it is given. Anything else is a `Validation` error
    /// and the provider is not called.
    pub async fn vm_reinstall(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        template: &str,
        ssh_keys: Vec<String>,
    ) -> BillingResult<()> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        if svc.status != ServiceStatus::Active {
            return Err(CoreError::Forbidden(format!(
                "the service is {}",
                svc.status
            )));
        }
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let template = template.trim();
        let offered = match virt.list_templates(vm.node.as_deref()).await {
            Ok(list) => list,
            Err(IntegrationError::Unsupported(_)) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let product_template = catalog::get_product(self.pool(), tenant_id, svc.product_id)
            .await?
            .and_then(|p| configured_template(&p.module_config));
        let allowed = !template.is_empty()
            && (offered.iter().any(|t| t.id == template)
                || product_template.as_deref() == Some(template));
        if !allowed {
            return Err(CoreError::validation(
                "this OS template is not offered for the service",
            ));
        }
        let password = hos_panels::password::generate_password("virt");
        let res = match virt
            .reinstall(
                &vm_ref(&vm),
                &VmReinstall {
                    template: template.to_owned(),
                    hostname: vm.hostname.clone(),
                    root_password: Some(password.clone()),
                    ssh_keys,
                    cloud_init_user_data: None,
                    extra: Value::Null,
                },
            )
            .await
        {
            Ok(res) => res,
            Err(e) => {
                return Err(self
                    .vm_op_failed(tenant_id, &vm, "reinstall", template, e)
                    .await);
            }
        };
        let pw = res.root_password.unwrap_or(password);
        services::update_provisioning(
            self.pool(),
            tenant_id,
            service_id,
            &services::ProvisioningUpdate {
                password_enc: Some(secrets::seal(
                    self.secret_box(),
                    pw.expose_str(),
                    &secrets::service_password_aad(tenant_id, service_id),
                )?),
                ..Default::default()
            },
        )
        .await?;
        self.vm_action_log(tenant_id, service_id, &format!("reinstall with {template}"))
            .await?;
        self.track_vm_op(tenant_id, &vm, "reinstall", template, Ok(res.task))
            .await?;
        Ok(())
    }

    /// Snapshots of the VM.
    pub async fn vm_snapshots(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<Vec<SnapshotInfo>> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Ok(virt.list_snapshots(&vm_ref(&vm)).await?)
    }

    /// Creates a snapshot; returns its id.
    pub async fn vm_create_snapshot(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        name: &str,
    ) -> BillingResult<String> {
        let (svc, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        if svc.status != ServiceStatus::Active {
            return Err(CoreError::Forbidden(format!(
                "the service is {}",
                svc.status
            )));
        }
        let name = crate::vm_ops::validate_snapshot_name(name)?;
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let existing = virt.list_snapshots(&vm_ref(&vm)).await?;
        if client_id.is_some() {
            let limit = self.vm_snapshot_limit(tenant_id, &svc).await?;
            if existing.len() >= limit as usize {
                return Err(CoreError::Conflict(if limit == 0 {
                    "snapshots are not included in this plan".to_owned()
                } else {
                    format!("snapshot limit reached ({limit}); delete an old snapshot first")
                }));
            }
        }
        if existing.iter().any(|s| s.name == name || s.id == name) {
            return Err(CoreError::Conflict(format!(
                "a snapshot named {name} already exists"
            )));
        }
        let created = match virt
            .create_snapshot(
                &vm_ref(&vm),
                &SnapshotRequest {
                    name: name.clone(),
                    description: None,
                    include_memory: false,
                },
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                return Err(self
                    .vm_op_failed(tenant_id, &vm, "snapshot_create", &name, e)
                    .await);
            }
        };
        self.vm_action_log(tenant_id, service_id, &format!("snapshot {name}"))
            .await?;
        self.track_vm_op(tenant_id, &vm, "snapshot_create", &name, Ok(created.task))
            .await?;
        Ok(created.snapshot_id)
    }

    /// Rolls the VM back to a snapshot.
    pub async fn vm_rollback_snapshot(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        snapshot_id: &str,
    ) -> BillingResult<()> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let snapshot_id = self.own_snapshot(&*virt, &vm, snapshot_id).await?;
        let res = virt.rollback_snapshot(&vm_ref(&vm), &snapshot_id).await;
        self.track_vm_op(tenant_id, &vm, "snapshot_rollback", &snapshot_id, res)
            .await?;
        self.vm_action_log(tenant_id, service_id, &format!("rollback to {snapshot_id}"))
            .await?;
        Ok(())
    }

    /// Deletes a snapshot.
    pub async fn vm_delete_snapshot(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        snapshot_id: &str,
    ) -> BillingResult<()> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        let _op = self.vm_op_lock(tenant_id, service_id).await?;
        let snapshot_id = self.own_snapshot(&*virt, &vm, snapshot_id).await?;
        let res = virt.delete_snapshot(&vm_ref(&vm), &snapshot_id).await;
        self.track_vm_op(tenant_id, &vm, "snapshot_delete", &snapshot_id, res)
            .await?;
        self.vm_action_log(
            tenant_id,
            service_id,
            &format!("delete snapshot {snapshot_id}"),
        )
        .await?;
        Ok(())
    }

    /// Opens a provider console session (URL / ticket handed to the browser).
    ///
    /// Only for drivers without the HostingOS console relay (vCenter WebMKS):
    /// where [`VirtFeatures::console_relay`](hos_core::integrations::virt::VirtFeatures)
    /// is set, the browser must never receive hypervisor tickets, so this is
    /// refused and callers use [`Billing::vm_console_session`] instead.
    pub async fn vm_console(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<ConsoleAccess> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        if virt.features().console_relay {
            return Err(IntegrationError::Unsupported(
                "direct console links (use the embedded console)",
            )
            .into());
        }
        Ok(virt.console(&vm_ref(&vm)).await?)
    }

    // -----------------------------------------------------------------------
    // domain actions
    // -----------------------------------------------------------------------

    async fn domain_registrar_for(
        &self,
        tenant_id: TenantId,
        d: &DomainRow,
    ) -> BillingResult<std::sync::Arc<dyn hos_core::integrations::DomainRegistrar>> {
        let id = d
            .registrar_integration_id
            .ok_or_else(|| CoreError::validation("this domain is not managed by a registrar"))?;
        self.registry().registrar(tenant_id, id).await
    }

    /// Changes the nameservers at the registrar.
    pub async fn domain_set_nameservers(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        nameservers: &[String],
    ) -> BillingResult<()> {
        let ns: Vec<String> = nameservers
            .iter()
            .map(|n| n.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|n| !n.is_empty())
            .collect();
        if !(2..=13).contains(&ns.len()) {
            return Err(CoreError::validation("2 to 13 nameservers are required"));
        }
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        self.domain_registrar_for(tenant_id, &d)
            .await?
            .set_nameservers(&d.name, &ns)
            .await?;
        billing_support::set_domain_nameservers(self.pool(), tenant_id, domain_id, &ns).await?;
        Ok(())
    }

    /// Registrar lock on / off.
    pub async fn domain_set_lock(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        locked: bool,
    ) -> BillingResult<()> {
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        self.domain_registrar_for(tenant_id, &d)
            .await?
            .set_lock(&d.name, locked)
            .await?;
        domains::set_domain_flags(
            self.pool(),
            tenant_id,
            domain_id,
            d.auto_renew,
            d.privacy,
            locked,
        )
        .await?;
        Ok(())
    }

    /// WHOIS privacy on / off.
    pub async fn domain_set_privacy(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        enabled: bool,
    ) -> BillingResult<()> {
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        self.domain_registrar_for(tenant_id, &d)
            .await?
            .set_privacy(&d.name, enabled)
            .await?;
        domains::set_domain_flags(
            self.pool(),
            tenant_id,
            domain_id,
            d.auto_renew,
            enabled,
            d.lock,
        )
        .await?;
        Ok(())
    }

    /// Auto-renew (renewal invoices) on / off — billing only.
    pub async fn domain_set_auto_renew(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        enabled: bool,
    ) -> BillingResult<()> {
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        domains::set_domain_flags(
            self.pool(),
            tenant_id,
            domain_id,
            enabled,
            d.privacy,
            d.lock,
        )
        .await?;
        Ok(())
    }

    /// EPP / auth code for an outgoing transfer (fetched from the registrar and
    /// stored encrypted).
    pub async fn domain_epp_code(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
    ) -> BillingResult<SecretString> {
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        let code = self
            .domain_registrar_for(tenant_id, &d)
            .await?
            .get_auth_code(&d.name)
            .await?;
        let sealed = secrets::seal(
            self.secret_box(),
            code.expose_str(),
            &secrets::epp_code_aad(tenant_id, &d.name),
        )?;
        domains::set_epp_code(self.pool(), tenant_id, domain_id, Some(&sealed)).await?;
        Ok(code)
    }

    /// Orders a manual renewal invoice for a domain (client "renew now").
    pub async fn order_domain_renewal(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        years: i16,
    ) -> BillingResult<InvoiceId> {
        let d = domains::get_domain(self.pool(), tenant_id, domain_id)
            .await?
            .ok_or_else(|| CoreError::not_found("domain", domain_id))?;
        if client_id.is_some_and(|c| c != d.client_id) {
            return Err(CoreError::not_found("domain", domain_id));
        }
        if !(d.status.is_renewable() || d.status == DomainStatus::Expired) {
            return Err(CoreError::Conflict(format!("the domain is {}", d.status)));
        }
        let years = years.clamp(1, 10);
        let start = d
            .next_due_date
            .or_else(|| d.expires_at.map(|e| e.date_naive()))
            .unwrap_or_else(|| self.clock().today());
        let mut tx = self.pool().begin().await?;
        let id = self
            .create_renewal_invoice(
                &mut tx,
                tenant_id,
                d.client_id,
                d.currency,
                start.max(self.clock().today()),
                vec![crate::automation::domain_renewal_item(
                    &d,
                    years,
                    start,
                    d.recurring_amount * Decimal::from(years),
                )],
            )
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // client-area reads: live VM state, DNS records, domain contacts
    // -----------------------------------------------------------------------

    /// Live state of the service's VM as reported by the hypervisor, with its
    /// stored row (a changed power state is written back).
    pub async fn vm_info(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<(
        VmRow,
        hos_core::integrations::virt::VmInfo,
        hos_core::integrations::virt::VirtCapabilities,
    )> {
        let (_, mut vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        let info = virt.get_vm(&vm_ref(&vm)).await?;
        if info.status != vm.status {
            services::set_vm_status(self.pool(), tenant_id, vm.id, info.status).await?;
            vm.status = info.status;
        }
        Ok((vm, info, virt.capabilities()))
    }

    /// Current resource usage of the service's VM.
    pub async fn vm_metrics(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<hos_core::integrations::virt::VmMetrics> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Ok(virt.metrics(&vm_ref(&vm)).await?)
    }

    /// OS templates the service's VM can be reinstalled with.
    pub async fn vm_templates(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
    ) -> BillingResult<Vec<hos_core::integrations::virt::TemplateInfo>> {
        let (_, vm, virt) = self.vm_of(tenant_id, service_id, client_id).await?;
        Ok(virt.list_templates(vm.node.as_deref()).await?)
    }

    /// DNS records of a domain at its registrar (`Unsupported` when the
    /// registrar offers no DNS hosting).
    pub async fn domain_dns_records(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
    ) -> BillingResult<Vec<hos_core::integrations::registrar::DnsRecord>> {
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        Ok(self
            .domain_registrar_for(tenant_id, &d)
            .await?
            .list_dns_records(&d.name)
            .await?)
    }

    /// Creates (`record.id == None`) or replaces a DNS record after
    /// [`validate_dns_record`].
    pub async fn domain_upsert_dns_record(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        record: &hos_core::integrations::registrar::DnsRecord,
    ) -> BillingResult<hos_core::integrations::registrar::DnsRecord> {
        let record = validate_dns_record(record)?;
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        Ok(self
            .domain_registrar_for(tenant_id, &d)
            .await?
            .upsert_dns_record(&d.name, &record)
            .await?)
    }

    /// Deletes a DNS record (identified by its registrar id).
    pub async fn domain_delete_dns_record(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        record_id: &str,
    ) -> BillingResult<()> {
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        let registrar = self.domain_registrar_for(tenant_id, &d).await?;
        let record = registrar
            .list_dns_records(&d.name)
            .await?
            .into_iter()
            .find(|r| r.id.as_deref() == Some(record_id))
            .ok_or_else(|| CoreError::not_found("dns record", record_id))?;
        registrar.delete_dns_record(&d.name, &record).await?;
        Ok(())
    }

    /// Replaces the WHOIS contacts of a domain (one contact for all four
    /// roles) at the registrar and in `domain_contacts`.
    pub async fn domain_update_contacts(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        client_id: Option<ClientId>,
        contact: &domains::DomainContactInput,
    ) -> BillingResult<()> {
        use hos_db::enums::ContactRole;
        let d = self
            .authorize_domain(tenant_id, domain_id, client_id)
            .await?;
        let missing = |what: &str| CoreError::validation(format!("the contact has no {what}"));
        let country = contact.country_code.trim().to_ascii_uppercase();
        let info = hos_core::integrations::registrar::ContactInfo {
            first_name: contact.first_name.trim().to_owned(),
            last_name: contact.last_name.trim().to_owned(),
            company: contact
                .organization
                .clone()
                .filter(|s| !s.trim().is_empty()),
            email: contact.email.trim().to_owned(),
            phone: contact
                .phone
                .as_deref()
                .and_then(|p| crate::provisioning::parse_phone(p, &country))
                .ok_or_else(|| missing("valid phone number"))?,
            fax: None,
            address: hos_core::integrations::PostalAddress {
                line1: contact
                    .address_line1
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| missing("address"))?,
                line2: contact.address_line2.clone(),
                city: contact
                    .city
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| missing("city"))?,
                state: contact.state.clone(),
                postal_code: contact.postal_code.clone(),
                country_code: country.clone(),
            },
            tax_id: None,
            extra: Value::Null,
        };
        if info.first_name.is_empty() || info.last_name.is_empty() {
            return Err(missing("name"));
        }
        if !info.email.contains('@') {
            return Err(missing("valid e-mail address"));
        }
        self.domain_registrar_for(tenant_id, &d)
            .await?
            .update_contacts(
                &d.name,
                &hos_core::integrations::registrar::DomainContacts::same(info),
            )
            .await?;
        let stored = domains::DomainContactInput {
            country_code: country,
            ..contact.clone()
        };
        for role in [
            ContactRole::Registrant,
            ContactRole::Admin,
            ContactRole::Tech,
            ContactRole::Billing,
        ] {
            domains::upsert_contact(self.pool(), tenant_id, domain_id, role, &stored).await?;
        }
        Ok(())
    }
}

/// Validates and normalises a DNS record a client submitted: supported types
/// `A AAAA CNAME MX TXT SRV CAA`, a relative name (`@` = apex), a TTL of
/// 60–86400 s (default 3600) and type-specific content checks.
pub fn validate_dns_record(
    r: &hos_core::integrations::registrar::DnsRecord,
) -> Result<hos_core::integrations::registrar::DnsRecord, CoreError> {
    use hos_core::integrations::registrar::DnsRecordType as T;
    let bad = |m: &str| CoreError::validation(m.to_owned());
    let host_ok = |h: &str| {
        let h = h.trim_end_matches('.');
        !h.is_empty()
            && h.len() <= 253
            && h.split('.').all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
    };
    let mut out = r.clone();
    let name = r.name.trim().to_ascii_lowercase();
    out.name = if name.is_empty() {
        "@".to_owned()
    } else {
        name
    };
    if out.name != "@"
        && (out.name.len() > 253
            || !out.name.split('.').all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && l.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '*'))
            }))
    {
        return Err(bad("invalid record name"));
    }
    let ttl = r.ttl.unwrap_or(3600);
    if !(60..=86_400).contains(&ttl) {
        return Err(bad("the TTL must be between 60 and 86400 seconds"));
    }
    out.ttl = Some(ttl);
    let value = r.value.trim().to_owned();
    if value.is_empty() || value.len() > 2048 {
        return Err(bad("the record value is empty or too long"));
    }
    match r.record_type {
        T::A => {
            value
                .parse::<std::net::Ipv4Addr>()
                .map_err(|_| bad("an A record needs an IPv4 address"))?;
            out.priority = None;
        }
        T::Aaaa => {
            value
                .parse::<std::net::Ipv6Addr>()
                .map_err(|_| bad("an AAAA record needs an IPv6 address"))?;
            out.priority = None;
        }
        T::Cname => {
            if out.name == "@" {
                return Err(bad("a CNAME record cannot be placed at the apex"));
            }
            if !host_ok(&value) {
                return Err(bad("a CNAME record needs a host name"));
            }
            out.priority = None;
        }
        T::Mx => {
            if !host_ok(&value) {
                return Err(bad("an MX record needs a mail server host name"));
            }
            out.priority = Some(r.priority.unwrap_or(10));
        }
        T::Txt => {
            out.priority = None;
        }
        T::Srv => {
            let mut labels = out.name.split('.');
            let (Some(svc), Some(proto)) = (labels.next(), labels.next()) else {
                return Err(bad("an SRV name looks like _service._proto"));
            };
            if !svc.starts_with('_') || !proto.starts_with('_') {
                return Err(bad("an SRV name looks like _service._proto"));
            }
            let parts: Vec<&str> = value.split_whitespace().collect();
            let numbers_ok = parts.len() == 3
                && parts[0].parse::<u16>().is_ok()
                && parts[1].parse::<u16>().is_ok();
            if !numbers_ok || !(parts[2] == "." || host_ok(parts[2])) {
                return Err(bad("an SRV value is: weight port target"));
            }
            out.priority = Some(r.priority.unwrap_or(0));
        }
        T::Caa => {
            let mut parts = value.splitn(3, ' ');
            let flags = parts.next().and_then(|f| f.parse::<u8>().ok());
            let tag = parts.next().unwrap_or_default();
            let tail = parts.next().unwrap_or_default().trim();
            if flags.is_none()
                || !matches!(tag, "issue" | "issuewild" | "iodef")
                || tail.len() < 2
                || !tail.starts_with('"')
                || !tail.ends_with('"')
            {
                return Err(bad(
                    "a CAA value is: flags tag \"value\" (tag: issue, issuewild or iodef)",
                ));
            }
            out.priority = None;
        }
        T::Ns => return Err(bad("NS records are managed through the nameservers")),
    }
    out.value = value;
    Ok(out)
}

/// Last day of a `years`-long period starting at `start`.
pub fn years_period_end(start: chrono::NaiveDate, years: i16) -> Option<chrono::NaiveDate> {
    start
        .checked_add_months(Months::new(u32::try_from(years.max(1)).ok()? * 12))?
        .checked_sub_days(Days::new(1))
}
