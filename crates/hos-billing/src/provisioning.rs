//! Provisioning: turns paid services and domains into real accounts, VMs and
//! registrations. Runs inside jobs (`provision_service`, `poll_vm_task`,
//! `register_domain`, `transfer_domain`, `renew_domain`).
//!
//! | product module | steps |
//! |---|---|
//! | panel | username (from the domain) + password, persisted encrypted **before** the call → `create_account` |
//! | virt | IP from the tenant's pool → `create_vm` → task polling → `vms` row |
//! | none | `provisioning` + `module_data.manual = true` → staff queue ([`Billing::complete_manual_provisioning`]) |
//!
//! ## Idempotency
//!
//! Credentials and the chosen username are stored before the provider call.
//! A re-run after a crash finds the account through `Conflict` →
//! `get_account` (same domain) and adopts it instead of opening a second
//! one; VMs are found by their deterministic name; an active service is never
//! provisioned again. Every step is written to `service_events`.
//!
//! ## Errors
//!
//! Retryable provider errors (timeouts, 5xx, rate limits) are returned so the
//! job is retried with backoff. Permanent errors — or the last attempt —
//! move the service to `failed`, release its IPs and notify staff.

use std::net::IpAddr;

use chrono::{Days, Months, Utc};
use hos_core::CoreError;
use hos_core::domain::{DomainStatus, IntegrationCategory, ProductType, ServiceStatus};
use hos_core::error::IntegrationError;
use hos_core::ids::{DomainId, IntegrationId, JobId, ServiceId, TenantId, UserId};
use hos_core::integrations::panel::{AccountCreated, AccountOwner, AccountRef, AccountSpec};
use hos_core::integrations::registrar::{
    ContactInfo, DomainContacts, DomainOperation, DomainOperationStatus, DomainRegistration,
    DomainRenewal, DomainTransfer,
};
use hos_core::integrations::virt::{
    IpConfig, NetworkSpec, StaticIp, TaskState, VmProvisioned, VmRef, VmSpec, VmStatus,
};
use hos_core::integrations::{PhoneNumber, PostalAddress};
use hos_core::secret::SecretString;
use hos_db::enums::{ActorKind, DomainRegistrationType};
use hos_db::repo::billing_support;
use hos_db::repo::catalog::{self, ProductRow};
use hos_db::repo::clients;
use hos_db::repo::clients::ClientRow;
use hos_db::repo::domains::{self, DomainRow};
use hos_db::repo::services::{self, NewServiceEvent, ProvisioningUpdate, ServiceRow, VmInput};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::jobs::{self, PollVmTaskPayload, VmTaskPurpose};
use crate::secrets;
use crate::settings::BillingSettings;
use crate::{Billing, BillingResult};

/// Job context of a provisioning step.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunContext {
    /// Job running the step (recorded in `service_events`).
    pub job_id: Option<JobId>,
    /// No retry follows: permanent failure handling applies to every error.
    pub final_attempt: bool,
}

/// Outcome of [`Billing::provision_service`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ProvisionOutcome {
    /// The account / VM exists and the service is active.
    Activated,
    /// Nothing to do: the service was already active.
    AlreadyActive,
    /// Waiting for an asynchronous provider task (`poll_vm_task` queued).
    InProgress,
    /// No module: waiting in the staff queue.
    Manual,
    /// Not provisionable in its current state.
    Skipped { reason: String },
    /// Permanently failed (service `failed`, staff notified).
    Failed { reason: String },
}

/// Outcome of domain registration / transfer / renewal steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DomainOutcome {
    /// Done at the registrar; domain `active` with the new expiry.
    Completed,
    /// Accepted, completes later (`sync_domain_status` finishes it).
    Pending,
    /// Already done earlier.
    AlreadyDone,
    /// No registrar: staff were asked to do it manually.
    Manual,
    Skipped {
        reason: String,
    },
    Failed {
        reason: String,
    },
}

/// Details entered by staff when finishing a manual setup.
#[derive(Debug, Clone, Default)]
pub struct ManualProvisioning {
    pub username: Option<String>,
    pub password: Option<SecretString>,
    pub external_id: Option<String>,
    pub dedicated_ip: Option<IpAddr>,
    pub server_hostname: Option<String>,
    pub note: Option<String>,
    pub staff_user_id: Option<UserId>,
}

/// Merges `products.module_config` with the service's configurable options
/// (`module_data.options`, which win).
pub fn module_settings(product: &ProductRow, svc: &ServiceRow) -> Map<String, Value> {
    let mut out = product
        .module_config
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(opts) = svc.module_data.get("options").and_then(Value::as_object) {
        for (k, v) in opts {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn setting_str(m: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match m.get(*k) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// Module-setting keys the OS template of a VM is read from (product
/// `module_config` and the service's configurable options alike).
pub(crate) const TEMPLATE_KEYS: [&str; 3] = ["template", "os", "os_template"];

/// OS templates a client can get for `product`: the template staff set in
/// its module settings, and the values of its choice-type configurable
/// options that set a template key ([`TEMPLATE_KEYS`]). Quantity options are
/// never a source: their module value is computed from a client-chosen
/// quantity, which would let a client pick any template / VM id.
pub(crate) async fn offered_templates(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    product: &ProductRow,
) -> BillingResult<Vec<String>> {
    let mut offered: Vec<String> = product
        .module_config
        .as_object()
        .map(|m| {
            TEMPLATE_KEYS
                .iter()
                .filter_map(|k| setting_str(m, &[k]))
                .collect()
        })
        .unwrap_or_default();
    offered.extend(
        catalog::product_option_values_for_keys(&mut *conn, tenant_id, product.id, &TEMPLATE_KEYS)
            .await?,
    );
    Ok(offered)
}

/// Checks that every OS template a line's configurable options set
/// (`options` as built by [`crate::checkout::module_options`]) is offered for
/// `product` ([`offered_templates`]); `Validation` otherwise.
pub(crate) async fn check_option_templates(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    product: &ProductRow,
    options: &Value,
) -> BillingResult<()> {
    let chosen: Vec<&Value> = TEMPLATE_KEYS
        .iter()
        .filter_map(|k| options.get(*k))
        .collect();
    if chosen.is_empty() {
        return Ok(());
    }
    let offered = offered_templates(conn, tenant_id, product).await?;
    for value in chosen {
        let ok = matches!(value, Value::String(s) if offered.iter().any(|o| o == s.trim()));
        if !ok {
            return Err(CoreError::validation(format!(
                "{}: the selected OS template is not offered for this product",
                product.name
            )));
        }
    }
    Ok(())
}

fn setting_u64(m: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| match m.get(*k) {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

/// Country calling codes for national phone numbers.
fn calling_code(country: &str) -> Option<&'static str> {
    Some(match country.to_ascii_uppercase().as_str() {
        "TR" => "90",
        "US" | "CA" => "1",
        "GB" => "44",
        "DE" => "49",
        "FR" => "33",
        "NL" => "31",
        "IT" => "39",
        "ES" => "34",
        "AZ" => "994",
        "RU" | "KZ" => "7",
        "UA" => "380",
        "AT" => "43",
        "CH" => "41",
        "BE" => "32",
        "SE" => "46",
        "NO" => "47",
        "DK" => "45",
        "FI" => "358",
        "PL" => "48",
        "GR" => "30",
        "BG" => "359",
        "RO" => "40",
        "CY" => "357",
        "IE" => "353",
        "PT" => "351",
        "CZ" => "420",
        "HU" => "36",
        "AE" => "971",
        "SA" => "966",
        "QA" => "974",
        "GE" => "995",
        "IN" => "91",
        "CN" => "86",
        "JP" => "81",
        "KR" => "82",
        "AU" => "61",
        "BR" => "55",
        "MX" => "52",
        _ => return None,
    })
}

/// Splits a free-form phone number into calling code + national number,
/// using the client's country for national (`0...`) numbers.
pub fn parse_phone(raw: &str, country: &str) -> Option<PhoneNumber> {
    let trimmed = raw.trim();
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 6 {
        return None;
    }
    let international = trimmed.starts_with('+') || digits.starts_with("00");
    let digits = if trimmed.starts_with('+') {
        digits
    } else if let Some(rest) = digits.strip_prefix("00") {
        rest.to_owned()
    } else {
        digits
    };
    if international {
        if let Some(cc) = calling_code(country)
            && let Some(rest) = digits.strip_prefix(cc)
        {
            return Some(PhoneNumber::new(cc, rest));
        }
        let (cc, rest) = digits.split_at(2.min(digits.len()));
        return Some(PhoneNumber::new(cc, rest));
    }
    let cc = calling_code(country)?;
    Some(PhoneNumber::new(cc, digits.trim_start_matches('0')))
}

/// A registrar contact from the client profile.
pub fn client_contact(c: &ClientRow) -> Result<ContactInfo, CoreError> {
    let missing = |what: &str| CoreError::validation(format!("client profile has no {what}"));
    let phone = c
        .phone
        .as_deref()
        .and_then(|p| parse_phone(p, &c.country_code))
        .ok_or_else(|| missing("valid phone number"))?;
    let line1 = c
        .address_line1
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| missing("address"))?;
    let city = c
        .city
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| missing("city"))?;
    let (first, last) = match (c.first_name.trim(), c.last_name.trim()) {
        ("", "") => {
            let name = c.display_name();
            (name.clone(), name)
        }
        (f, "") => (f.to_owned(), f.to_owned()),
        (f, l) => (f.to_owned(), l.to_owned()),
    };
    Ok(ContactInfo {
        first_name: first,
        last_name: last,
        company: c.company_name.clone().filter(|s| !s.trim().is_empty()),
        email: c.email.clone(),
        phone,
        fax: None,
        address: PostalAddress {
            line1,
            line2: c.address_line2.clone(),
            city,
            state: c.state.clone(),
            postal_code: c.postal_code.clone(),
            country_code: c.country_code.to_ascii_uppercase(),
        },
        tax_id: c.tax_number.clone().or_else(|| c.national_id.clone()),
        extra: Value::Null,
    })
}

fn event(e: &str, msg: &str, ctx: RunContext) -> NewServiceEvent {
    NewServiceEvent {
        job_id: ctx.job_id,
        actor_kind: Some(ActorKind::System),
        ..NewServiceEvent::info(e, msg)
    }
}

fn is_retryable(e: &CoreError) -> bool {
    match e {
        CoreError::Integration(i) => i.is_retryable(),
        CoreError::Database(_) => true,
        _ => false,
    }
}

fn expiry_after_years(from: chrono::DateTime<Utc>, years: i16) -> chrono::DateTime<Utc> {
    let months = u32::try_from(years.max(1)).unwrap_or(1) * 12;
    from.checked_add_months(Months::new(months)).unwrap_or(from)
}

impl Billing {
    async fn product_of(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        svc: &ServiceRow,
    ) -> BillingResult<ProductRow> {
        catalog::get_product(&mut *conn, tenant_id, svc.product_id)
            .await?
            .ok_or_else(|| CoreError::not_found("product", svc.product_id))
    }

    /// Provisions a service (the `provision_service` job). Safe to re-run.
    pub async fn provision_service(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        ctx: RunContext,
    ) -> BillingResult<ProvisionOutcome> {
        let mut conn = self.pool().acquire().await?;
        let svc = services::get_service(&mut *conn, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        match svc.status {
            ServiceStatus::Active | ServiceStatus::Suspended => {
                return Ok(ProvisionOutcome::AlreadyActive);
            }
            ServiceStatus::Terminated | ServiceStatus::Cancelled => {
                return Ok(ProvisionOutcome::Skipped {
                    reason: format!("service is {}", svc.status),
                });
            }
            ServiceStatus::Provisioning
                if svc.module_data.get("manual").and_then(Value::as_bool) == Some(true) =>
            {
                return Ok(ProvisionOutcome::Manual);
            }
            _ => {}
        }
        let product = self.product_of(&mut conn, tenant_id, &svc).await?;
        drop(conn);
        let integration_id = svc.integration_id.or(product.integration_id);
        let loaded = match integration_id {
            Some(id) => Some(self.registry().get(tenant_id, id).await),
            None => None,
        };
        let result = match loaded {
            None => {
                self.queue_manual(tenant_id, &svc, &product, "no provisioning module", ctx)
                    .await
            }
            Some(Err(e)) => Err(e),
            Some(Ok(l)) => match l.category {
                IntegrationCategory::Panel => {
                    self.provision_panel(tenant_id, &svc, &product, l.id, &l.kind, ctx)
                        .await
                }
                IntegrationCategory::Virt => {
                    self.provision_vm(tenant_id, &svc, &product, l.id, ctx)
                        .await
                }
                other => {
                    self.queue_manual(
                        tenant_id,
                        &svc,
                        &product,
                        &format!("{other} integrations do not provision services"),
                        ctx,
                    )
                    .await
                }
            },
        };
        match result {
            Ok(o) => Ok(o),
            Err(e) => self.provisioning_error(tenant_id, service_id, e, ctx).await,
        }
    }

    /// Retryable errors bubble up (job retry); others fail the service.
    async fn provisioning_error(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        err: CoreError,
        ctx: RunContext,
    ) -> BillingResult<ProvisionOutcome> {
        if is_retryable(&err) && !ctx.final_attempt {
            services::log_event(
                self.pool(),
                tenant_id,
                service_id,
                &NewServiceEvent {
                    level: hos_db::enums::EventLevel::Warning,
                    ..event("provisioning_retry", &err.to_string(), ctx)
                },
            )
            .await?;
            return Err(err);
        }
        let reason = err.to_string();
        self.fail_service(tenant_id, service_id, &reason, ctx)
            .await?;
        Ok(ProvisionOutcome::Failed { reason })
    }

    /// Moves a pending / provisioning service to `failed`, frees its IPs and
    /// notifies staff.
    pub async fn fail_service(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        reason: &str,
        ctx: RunContext,
    ) -> BillingResult<()> {
        let mut tx = self.pool().begin().await?;
        let Some(svc) = billing_support::lock_service(&mut tx, tenant_id, service_id).await? else {
            return Ok(());
        };
        let mut e = NewServiceEvent {
            data: json!({"error": reason}),
            job_id: ctx.job_id,
            actor_kind: Some(ActorKind::System),
            ..NewServiceEvent::error("provisioning_failed", reason)
        };
        if matches!(
            svc.status,
            ServiceStatus::Pending | ServiceStatus::Provisioning
        ) {
            services::transition_service(
                &mut *tx,
                tenant_id,
                service_id,
                svc.status,
                ServiceStatus::Failed,
                None,
            )
            .await?;
            e.from_status = Some(svc.status);
            e.to_status = Some(ServiceStatus::Failed);
            services::release_service_ips(&mut *tx, tenant_id, service_id).await?;
        }
        services::log_event(&mut *tx, tenant_id, service_id, &e).await?;
        self.notify_staff(
            &mut tx,
            tenant_id,
            "service.provisioning_failed",
            &format!(
                "Provisioning failed for {}",
                svc.domain.as_deref().unwrap_or("a service")
            ),
            Some(reason),
            Some(&format!("/admin/services/{service_id}")),
            &json!({"service_id": service_id}),
        )
        .await?;
        self.emit_event(
            &mut tx,
            tenant_id,
            "service.provisioning_failed",
            json!({"service_id": service_id, "error": reason}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Puts a service in the manual setup queue (`provisioning` with
    /// `module_data.manual = true`) and notifies staff.
    async fn queue_manual(
        &self,
        tenant_id: TenantId,
        svc: &ServiceRow,
        product: &ProductRow,
        why: &str,
        ctx: RunContext,
    ) -> BillingResult<ProvisionOutcome> {
        let mut tx = self.pool().begin().await?;
        let Some(cur) = billing_support::lock_service(&mut tx, tenant_id, svc.id).await? else {
            return Err(CoreError::not_found("service", svc.id));
        };
        if !matches!(cur.status, ServiceStatus::Pending | ServiceStatus::Failed) {
            tx.commit().await?;
            return Ok(ProvisionOutcome::Skipped {
                reason: format!("service is {}", cur.status),
            });
        }
        services::transition_service(
            &mut *tx,
            tenant_id,
            svc.id,
            cur.status,
            ServiceStatus::Provisioning,
            None,
        )
        .await?;
        services::update_provisioning(
            &mut *tx,
            tenant_id,
            svc.id,
            &ProvisioningUpdate {
                module_data: Some(json!({"manual": true, "manual_reason": why})),
                ..Default::default()
            },
        )
        .await?;
        services::log_event(
            &mut *tx,
            tenant_id,
            svc.id,
            &NewServiceEvent {
                from_status: Some(cur.status),
                to_status: Some(ServiceStatus::Provisioning),
                ..event("manual_provisioning_required", why, ctx)
            },
        )
        .await?;
        self.notify_staff(
            &mut tx,
            tenant_id,
            "service.manual_setup",
            &format!("{} needs manual setup", product.name),
            Some(why),
            Some(&format!("/admin/services/{}", svc.id)),
            &json!({"service_id": svc.id}),
        )
        .await?;
        tx.commit().await?;
        Ok(ProvisionOutcome::Manual)
    }

    /// Finishes a manually provisioned service (staff action) and activates it.
    pub async fn complete_manual_provisioning(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        details: &ManualProvisioning,
    ) -> BillingResult<ServiceRow> {
        let mut tx = self.pool().begin().await?;
        let svc = billing_support::lock_service(&mut tx, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        if !matches!(
            svc.status,
            ServiceStatus::Pending | ServiceStatus::Provisioning | ServiceStatus::Failed
        ) {
            return Err(CoreError::Conflict(format!("service is {}", svc.status)));
        }
        let password_enc = match &details.password {
            Some(p) if !p.is_empty() => Some(secrets::seal(
                self.secret_box(),
                p.expose_str(),
                &secrets::service_password_aad(tenant_id, service_id),
            )?),
            _ => None,
        };
        let update = ProvisioningUpdate {
            external_id: details.external_id.clone(),
            username: details.username.clone(),
            password_enc,
            server_hostname: details.server_hostname.clone(),
            dedicated_ip: details.dedicated_ip,
            module_data: Some(json!({"manual": false})),
            ..Default::default()
        };
        let ctx = RunContext::default();
        let mut e = event(
            "manual_provisioning_completed",
            details.note.as_deref().unwrap_or("completed by staff"),
            ctx,
        );
        e.actor_kind = details.staff_user_id.map(|_| ActorKind::User);
        e.actor_id = details.staff_user_id.map(|u| *u.as_uuid());
        self.activate_locked(&mut tx, tenant_id, &svc, &update, e)
            .await?;
        let row = services::get_service(&mut *tx, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        tx.commit().await?;
        Ok(row)
    }

    /// Writes provisioning results, moves the (locked) service to `active`,
    /// logs `e` and queues the activation e-mails / webhook.
    async fn activate_locked(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        svc: &ServiceRow,
        update: &ProvisioningUpdate,
        mut e: NewServiceEvent,
    ) -> BillingResult<()> {
        services::update_provisioning(&mut *conn, tenant_id, svc.id, update).await?;
        if svc.status != ServiceStatus::Provisioning && svc.status != ServiceStatus::Active {
            // pending / failed → provisioning → active keeps the state machine honest
            services::transition_service(
                &mut *conn,
                tenant_id,
                svc.id,
                svc.status,
                ServiceStatus::Provisioning,
                None,
            )
            .await?;
        }
        if svc.status != ServiceStatus::Active {
            services::transition_service(
                &mut *conn,
                tenant_id,
                svc.id,
                ServiceStatus::Provisioning,
                ServiceStatus::Active,
                None,
            )
            .await?;
        }
        e.from_status = Some(svc.status);
        e.to_status = Some(ServiceStatus::Active);
        services::log_event(&mut *conn, tenant_id, svc.id, &e).await?;

        let product = self.product_of(conn, tenant_id, svc).await?;
        let fresh = services::get_service(&mut *conn, tenant_id, svc.id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", svc.id))?;
        let url = self.tenant_url(tenant_id).await?;
        let ctx_json = json!({
            "service": {
                "name": product.name,
                "domain": fresh.domain.clone().unwrap_or_default(),
                "username": fresh.username.clone().unwrap_or_default(),
            },
            "action_url": format!("{url}/client/services/{}", svc.id),
        });
        self.queue_client_email(
            conn,
            tenant_id,
            svc.client_id,
            hos_mail::defaults::keys::SERVICE_ACTIVATED,
            ctx_json.clone(),
            Some(("service", *svc.id.as_uuid())),
        )
        .await?;
        if let Some(key) = product
            .welcome_email_template
            .as_deref()
            .filter(|k| !k.trim().is_empty() && *k != hos_mail::defaults::keys::SERVICE_ACTIVATED)
        {
            self.queue_client_email(
                conn,
                tenant_id,
                svc.client_id,
                key,
                ctx_json,
                Some(("service", *svc.id.as_uuid())),
            )
            .await?;
        }
        self.emit_event(
            conn,
            tenant_id,
            "service.activated",
            json!({"service_id": svc.id, "client_id": svc.client_id, "product_id": svc.product_id}),
        )
        .await?;
        Ok(())
    }

    /// Activates a provisioning service with provider results, unless a
    /// concurrent run did it already.
    async fn finish_activation(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        update: &ProvisioningUpdate,
        e: NewServiceEvent,
    ) -> BillingResult<ProvisionOutcome> {
        let mut tx = self.pool().begin().await?;
        let svc = billing_support::lock_service(&mut tx, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        match svc.status {
            ServiceStatus::Active | ServiceStatus::Suspended => {
                tx.commit().await?;
                return Ok(ProvisionOutcome::AlreadyActive);
            }
            ServiceStatus::Terminated | ServiceStatus::Cancelled => {
                tx.commit().await?;
                return Ok(ProvisionOutcome::Skipped {
                    reason: format!("service became {} while provisioning", svc.status),
                });
            }
            _ => {}
        }
        self.activate_locked(&mut tx, tenant_id, &svc, update, e)
            .await?;
        tx.commit().await?;
        Ok(ProvisionOutcome::Activated)
    }

    /// Locks the service, moves it to `provisioning` and runs `prepare` on it.
    /// Returns `None` when it is no longer provisionable.
    async fn begin_provisioning(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        ctx: RunContext,
        message: &str,
    ) -> BillingResult<Option<(sqlx::Transaction<'static, sqlx::Postgres>, ServiceRow)>> {
        let mut tx = self.pool().begin().await?;
        let svc = billing_support::lock_service(&mut tx, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        match svc.status {
            ServiceStatus::Pending | ServiceStatus::Failed => {
                services::transition_service(
                    &mut *tx,
                    tenant_id,
                    service_id,
                    svc.status,
                    ServiceStatus::Provisioning,
                    None,
                )
                .await?;
                services::log_event(
                    &mut *tx,
                    tenant_id,
                    service_id,
                    &NewServiceEvent {
                        from_status: Some(svc.status),
                        to_status: Some(ServiceStatus::Provisioning),
                        ..event("provisioning_started", message, ctx)
                    },
                )
                .await?;
                Ok(Some((tx, svc)))
            }
            ServiceStatus::Provisioning => {
                services::log_event(
                    &mut *tx,
                    tenant_id,
                    service_id,
                    &event("provisioning_resumed", message, ctx),
                )
                .await?;
                Ok(Some((tx, svc)))
            }
            _ => Ok(None),
        }
    }

    // -----------------------------------------------------------------------
    // panel
    // -----------------------------------------------------------------------

    async fn provision_panel(
        &self,
        tenant_id: TenantId,
        svc: &ServiceRow,
        product: &ProductRow,
        integration_id: IntegrationId,
        kind: &str,
        ctx: RunContext,
    ) -> BillingResult<ProvisionOutcome> {
        let settings = module_settings(product, svc);
        let domain = svc
            .domain
            .clone()
            .filter(|d| !d.trim().is_empty())
            .ok_or_else(|| CoreError::validation("a hosting account needs a domain"))?
            .trim()
            .to_ascii_lowercase();
        let package = setting_str(&settings, &["package", "plan"]).ok_or_else(|| {
            CoreError::validation(format!("product {} has no panel package", product.name))
        })?;
        let rules = hos_panels::username::UsernameRules::for_kind(kind);
        let candidates = rules.candidates(&domain);

        // 1. choose + persist username and password before calling the panel
        let Some((mut tx, cur)) = self
            .begin_provisioning(
                tenant_id,
                svc.id,
                ctx,
                &format!("creating {domain} on {kind}"),
            )
            .await?
        else {
            return Ok(ProvisionOutcome::AlreadyActive);
        };
        let mut username = match cur.username.clone().filter(|u| !u.is_empty()) {
            Some(u) => u,
            None => candidates.first().cloned().ok_or_else(|| {
                CoreError::validation("no username can be derived from the domain")
            })?,
        };
        let aad = secrets::service_password_aad(tenant_id, svc.id);
        let password = match &cur.password_enc {
            Some(sealed) => secrets::open(self.secret_box(), sealed, &aad)?,
            None => hos_panels::password::generate_password(kind),
        };
        services::update_provisioning(
            &mut *tx,
            tenant_id,
            svc.id,
            &ProvisioningUpdate {
                integration_id: Some(integration_id),
                username: Some(username.clone()),
                password_enc: match &cur.password_enc {
                    Some(_) => None,
                    None => Some(secrets::seal(
                        self.secret_box(),
                        password.expose_str(),
                        &aad,
                    )?),
                },
                ..Default::default()
            },
        )
        .await?;
        tx.commit().await?;

        // 2. create (or adopt) the account
        let mut conn = self.pool().acquire().await?;
        let client = clients::get_client(&mut *conn, tenant_id, svc.client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", svc.client_id))?;
        drop(conn);
        let panel = self.registry().panel(tenant_id, integration_id).await?;
        let mut tried = 0;
        let (created, adopted) = loop {
            tried += 1;
            let spec = AccountSpec {
                username: username.clone(),
                domain: domain.clone(),
                password: password.clone(),
                package: package.clone(),
                owner: AccountOwner {
                    name: Some(client.display_name()),
                    company: client.company_name.clone(),
                    email: client.email.clone(),
                },
                ip: None,
                reseller: product.product_type == ProductType::ResellerHosting,
                shell_access: settings
                    .get("shell_access")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                locale: Some(client.locale),
                limit_overrides: None,
                extra: settings.get("extra").cloned().unwrap_or(Value::Null),
            };
            match panel.create_account(&spec).await {
                Ok(c) => break (c, false),
                Err(IntegrationError::Conflict(msg)) => {
                    let probe = AccountRef {
                        username: username.clone(),
                        domain: Some(domain.clone()),
                        id: None,
                    };
                    match panel.get_account(&probe).await {
                        Ok(info)
                            if info
                                .account
                                .domain
                                .as_deref()
                                .is_some_and(|d| d.eq_ignore_ascii_case(&domain)) =>
                        {
                            // our earlier run created it: adopt, never create twice
                            break (
                                AccountCreated {
                                    account: info.account,
                                    ip: info.ip,
                                    nameservers: Vec::new(),
                                    login_url: None,
                                    extra: info.extra,
                                },
                                true,
                            );
                        }
                        Ok(_) => {
                            // username belongs to another account: next candidate
                            let next = candidates
                                .iter()
                                .skip_while(|c| **c != username)
                                .nth(1)
                                .cloned();
                            match next {
                                Some(n) if tried < 8 => {
                                    username = n;
                                    billing_support::set_service_identity(
                                        self.pool(),
                                        tenant_id,
                                        svc.id,
                                        None,
                                        Some(&username),
                                    )
                                    .await?;
                                }
                                _ => return Err(IntegrationError::Conflict(msg).into()),
                            }
                        }
                        Err(_) => {
                            return Err(CoreError::Integration(IntegrationError::Conflict(
                                format!(
                                    "{domain} already exists on the panel under another account: {msg}"
                                ),
                            )));
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        };
        let update = ProvisioningUpdate {
            integration_id: Some(integration_id),
            external_id: Some(
                created
                    .account
                    .id
                    .clone()
                    .unwrap_or_else(|| created.account.username.clone()),
            ),
            username: Some(created.account.username.clone()),
            dedicated_ip: created.ip,
            module_data: Some(json!({
                "package": package,
                "login_url": created.login_url,
                "nameservers": created.nameservers,
            })),
            ..Default::default()
        };
        let msg = if adopted {
            format!(
                "adopted existing account {} on {kind}",
                created.account.username
            )
        } else {
            format!("account {} created on {kind}", created.account.username)
        };
        self.finish_activation(
            tenant_id,
            svc.id,
            &update,
            event("account_created", &msg, ctx),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // virtualization
    // -----------------------------------------------------------------------

    /// Allocates (or finds) the service's IPv4 from the tenant's pools.
    /// `Ok(None)`: no pool is configured for the integration → provider-assigned.
    async fn allocate_ip(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        svc: &ServiceRow,
        integration_id: IntegrationId,
    ) -> BillingResult<Option<(IpAddr, StaticIp, Vec<IpAddr>)>> {
        let pools = services::ip_pool_usage(&mut *conn, tenant_id).await?;
        let own: Vec<_> = pools
            .iter()
            .filter(|p| p.pool.is_active && p.pool.integration_id == Some(integration_id))
            .collect();
        let candidates: Vec<_> = if own.is_empty() {
            pools
                .iter()
                .filter(|p| p.pool.is_active && p.pool.integration_id.is_none())
                .collect()
        } else {
            own
        };
        if candidates.is_empty() {
            return Ok(None);
        }
        let to_static = |ip: IpAddr| -> Option<(IpAddr, StaticIp, Vec<IpAddr>)> {
            let p = candidates.iter().find(|p| p.pool.network.contains(&ip))?;
            Some((
                ip,
                StaticIp {
                    address: ip,
                    prefix_len: p.pool.network.prefix_len(),
                    gateway: p.pool.gateway,
                },
                p.pool.nameservers.clone(),
            ))
        };
        if let Some(ip) = svc.dedicated_ip
            && let Some(found) = to_static(ip)
        {
            return Ok(Some(found));
        }
        for p in candidates.iter().filter(|p| p.free > 0) {
            if let Some(addr) =
                services::assign_free_ip(&mut *conn, tenant_id, p.pool.id, Some(svc.id), None)
                    .await?
            {
                return Ok(to_static(addr.address));
            }
        }
        Err(CoreError::validation("the IP pool is exhausted"))
    }

    async fn provision_vm(
        &self,
        tenant_id: TenantId,
        svc: &ServiceRow,
        product: &ProductRow,
        integration_id: IntegrationId,
        ctx: RunContext,
    ) -> BillingResult<ProvisionOutcome> {
        let virt = self.registry().virt(tenant_id, integration_id).await?;
        // A VM row means an earlier run created it: only finish the job.
        if let Some(vm) = services::vm_for_service(self.pool(), tenant_id, svc.id).await? {
            let info = virt
                .get_vm(&VmRef {
                    node: vm.node.clone(),
                    id: vm.external_id.clone(),
                })
                .await?;
            if info.status == VmStatus::Provisioning {
                if let Some(task) = svc
                    .module_data
                    .get("vm_task")
                    .and_then(|t| serde_json::from_value(t.clone()).ok())
                {
                    let mut conn = self.pool().acquire().await?;
                    jobs::enqueue_poll_vm_task(
                        &mut conn,
                        tenant_id,
                        &PollVmTaskPayload {
                            service_id: svc.id,
                            vm_id: Some(vm.id),
                            task,
                            purpose: VmTaskPurpose::Create,
                            operation_id: None,
                        },
                        Utc::now() + self.poll_delay(),
                    )
                    .await?;
                    return Ok(ProvisionOutcome::InProgress);
                }
                return Err(IntegrationError::Transport("VM is still being created".into()).into());
            }
            services::set_vm_status(self.pool(), tenant_id, vm.id, info.status).await?;
            return self
                .finish_activation(
                    tenant_id,
                    svc.id,
                    &ProvisioningUpdate {
                        external_id: Some(vm.external_id.clone()),
                        ..Default::default()
                    },
                    event(
                        "vm_ready",
                        &format!("VM {} is {}", vm.name, info.status),
                        ctx,
                    ),
                )
                .await;
        }

        let settings = module_settings(product, svc);
        let template = setting_str(&settings, &TEMPLATE_KEYS).ok_or_else(|| {
            CoreError::validation(format!("product {} has no OS template", product.name))
        })?;
        // Checked at order time too; again here because the stored options
        // outlive the product's settings (and older orders were not checked).
        // A provider would clone whatever VM / disk id it is given.
        {
            let mut conn = self.pool().acquire().await?;
            if !offered_templates(&mut conn, tenant_id, product)
                .await?
                .contains(&template)
            {
                return Err(CoreError::validation(format!(
                    "OS template {template:?} is not offered for product {}",
                    product.name
                )));
            }
        }
        let windows = template.to_ascii_lowercase().contains("win");

        // 1. IP + root password, persisted before the call
        let Some((mut tx, cur)) = self
            .begin_provisioning(tenant_id, svc.id, ctx, "creating VM")
            .await?
        else {
            return Ok(ProvisionOutcome::AlreadyActive);
        };
        let ip = self
            .allocate_ip(&mut tx, tenant_id, &cur, integration_id)
            .await?;
        let aad = secrets::service_password_aad(tenant_id, svc.id);
        let password = match &cur.password_enc {
            Some(sealed) => secrets::open(self.secret_box(), sealed, &aad)?,
            None => hos_panels::password::generate_password("virt"),
        };
        services::update_provisioning(
            &mut *tx,
            tenant_id,
            svc.id,
            &ProvisioningUpdate {
                integration_id: Some(integration_id),
                username: Some(if windows { "Administrator" } else { "root" }.to_owned()),
                password_enc: match &cur.password_enc {
                    Some(_) => None,
                    None => Some(secrets::seal(
                        self.secret_box(),
                        password.expose_str(),
                        &aad,
                    )?),
                },
                dedicated_ip: ip.as_ref().map(|(a, _, _)| *a),
                assigned_ips: ip.as_ref().map(|(a, _, _)| vec![*a]),
                ..Default::default()
            },
        )
        .await?;
        if let Some((addr, _, _)) = &ip {
            services::log_event(
                &mut *tx,
                tenant_id,
                svc.id,
                &event("ip_assigned", &format!("assigned {addr}"), ctx),
            )
            .await?;
        }
        tx.commit().await?;

        // 2. create (or adopt) the VM
        let name = format!("svc-{}", svc.id.as_uuid().simple());
        let hostname = cur
            .domain
            .clone()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| format!("{name}.vps.local"));
        let spec = VmSpec {
            name: name.clone(),
            hostname: hostname.clone(),
            node: setting_str(&settings, &["node"]),
            location: setting_str(&settings, &["location"]),
            template: template.clone(),
            plan: setting_str(&settings, &["plan"]),
            cpu_cores: setting_u64(&settings, &["cpu_cores", "cpu", "vcpu"])
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(1),
            memory_mb: setting_u64(&settings, &["memory_mb", "memory", "ram"]).unwrap_or(1024),
            disk_gb: setting_u64(&settings, &["disk_gb", "disk"]).unwrap_or(20),
            storage: setting_str(&settings, &["storage"]),
            bandwidth_gb: setting_u64(&settings, &["bandwidth_gb", "bandwidth"]),
            network: NetworkSpec {
                bridge: setting_str(&settings, &["bridge"]),
                ipv4: Some(match &ip {
                    Some((_, s, _)) => IpConfig::Static(s.clone()),
                    None => IpConfig::Auto,
                }),
                dns_servers: ip.as_ref().map(|(_, _, ns)| ns.clone()).unwrap_or_default(),
                ..NetworkSpec::default()
            },
            root_password: Some(password),
            // SSH public keys entered on the order form (`custom_fields.ssh_keys`).
            ssh_keys: cur
                .custom_fields
                .get("ssh_keys")
                .and_then(Value::as_array)
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            start_after_create: true,
            tags: vec![format!("tenant-{tenant_id}"), format!("service-{}", svc.id)],
            extra: settings.get("extra").cloned().unwrap_or(Value::Null),
            ..VmSpec::default()
        };
        let provisioned = match virt.create_vm(&spec).await {
            Ok(p) => p,
            Err(IntegrationError::Conflict(msg)) => {
                let existing = virt
                    .list_vms(None)
                    .await?
                    .into_iter()
                    .find(|v| v.name == name)
                    .ok_or(IntegrationError::Conflict(msg))?;
                VmProvisioned {
                    vm: existing.vm,
                    task: None,
                    root_password: None,
                    ipv4: Vec::new(),
                    ipv6: Vec::new(),
                    extra: existing.extra,
                }
            }
            Err(e) => return Err(e.into()),
        };
        let primary = ip
            .as_ref()
            .map(|(a, _, _)| *a)
            .or_else(|| provisioned.ipv4.first().copied());
        let vm_row = services::upsert_vm(
            self.pool(),
            tenant_id,
            &VmInput {
                service_id: Some(svc.id),
                integration_id,
                node: provisioned.vm.node.clone(),
                external_id: provisioned.vm.id.clone(),
                name: name.clone(),
                hostname: Some(hostname),
                os_template: Some(template),
                cpu_cores: i32::try_from(spec.cpu_cores).ok(),
                memory_mb: i32::try_from(spec.memory_mb).ok(),
                disk_gb: i32::try_from(spec.disk_gb).ok(),
                status: if provisioned.task.is_some() {
                    VmStatus::Provisioning
                } else {
                    VmStatus::Running
                },
                primary_ipv4: primary,
                primary_ipv6: provisioned.ipv6.first().copied(),
                metadata: provisioned.extra.clone(),
            },
        )
        .await?;
        let mut assigned: Vec<IpAddr> = provisioned.ipv4.clone();
        assigned.extend(provisioned.ipv6.iter().copied());
        if let Some(a) = primary
            && !assigned.contains(&a)
        {
            assigned.insert(0, a);
        }
        let update = ProvisioningUpdate {
            integration_id: Some(integration_id),
            external_id: Some(provisioned.vm.id.clone()),
            server_hostname: provisioned.vm.node.clone(),
            dedicated_ip: primary,
            assigned_ips: Some(assigned),
            password_enc: match &provisioned.root_password {
                Some(p) if !p.is_empty() => Some(secrets::seal(
                    self.secret_box(),
                    p.expose_str(),
                    &secrets::service_password_aad(tenant_id, svc.id),
                )?),
                _ => None,
            },
            module_data: Some(json!({"vm_id": vm_row.id, "vm_task": provisioned.task})),
            ..Default::default()
        };
        match provisioned.task {
            Some(task) => {
                let mut tx = self.pool().begin().await?;
                services::update_provisioning(&mut *tx, tenant_id, svc.id, &update).await?;
                services::log_event(
                    &mut *tx,
                    tenant_id,
                    svc.id,
                    &NewServiceEvent {
                        data: json!({"task": task}),
                        ..event("vm_creating", &format!("VM {name} is being created"), ctx)
                    },
                )
                .await?;
                jobs::enqueue_poll_vm_task(
                    &mut tx,
                    tenant_id,
                    &PollVmTaskPayload {
                        service_id: svc.id,
                        vm_id: Some(vm_row.id),
                        task,
                        purpose: VmTaskPurpose::Create,
                        operation_id: None,
                    },
                    Utc::now() + self.poll_delay(),
                )
                .await?;
                tx.commit().await?;
                Ok(ProvisionOutcome::InProgress)
            }
            None => {
                self.finish_activation(
                    tenant_id,
                    svc.id,
                    &update,
                    event("vm_created", &format!("VM {name} created"), ctx),
                )
                .await
            }
        }
    }

    fn poll_delay(&self) -> chrono::Duration {
        chrono::Duration::from_std(self.config().vm_poll_interval).unwrap_or_default()
    }

    /// Polls a hypervisor task (the `poll_vm_task` job). `Ok(None)` = still
    /// running (the job is retried after the poll interval).
    pub async fn poll_vm_task(
        &self,
        tenant_id: TenantId,
        payload: &PollVmTaskPayload,
        ctx: RunContext,
    ) -> BillingResult<Option<ProvisionOutcome>> {
        let svc = services::get_service(self.pool(), tenant_id, payload.service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", payload.service_id))?;
        let integration_id = svc
            .integration_id
            .ok_or_else(|| CoreError::validation("service has no integration"))?;
        let virt = self.registry().virt(tenant_id, integration_id).await?;
        let state = match virt.task_status(&payload.task).await {
            Ok(s) => s,
            Err(e) if payload.purpose == VmTaskPurpose::Create => {
                return self
                    .provisioning_error(tenant_id, svc.id, e.into(), ctx)
                    .await
                    .map(Some);
            }
            Err(e) => return Err(e.into()),
        };
        self.note_vm_task_state(tenant_id, payload.operation_id, &state)
            .await;
        let vm = services::vm_for_service(self.pool(), tenant_id, svc.id).await?;
        match state {
            TaskState::Running { .. } => Ok(None),
            TaskState::Failed { message } => {
                if payload.purpose == VmTaskPurpose::Create {
                    self.fail_service(tenant_id, svc.id, &message, ctx).await?;
                    return Ok(Some(ProvisionOutcome::Failed { reason: message }));
                }
                services::log_event(
                    self.pool(),
                    tenant_id,
                    svc.id,
                    &NewServiceEvent::error("vm_task_failed", &message),
                )
                .await?;
                Ok(Some(ProvisionOutcome::Failed { reason: message }))
            }
            TaskState::Succeeded => {
                if let Some(vm) = &vm {
                    let info = virt
                        .get_vm(&VmRef {
                            node: vm.node.clone(),
                            id: vm.external_id.clone(),
                        })
                        .await?;
                    services::set_vm_status(self.pool(), tenant_id, vm.id, info.status).await?;
                }
                if payload.purpose == VmTaskPurpose::Create {
                    let out = self
                        .finish_activation(
                            tenant_id,
                            svc.id,
                            &ProvisioningUpdate {
                                module_data: Some(json!({"vm_task": null})),
                                ..Default::default()
                            },
                            event("vm_created", "VM creation finished", ctx),
                        )
                        .await?;
                    return Ok(Some(out));
                }
                Ok(Some(ProvisionOutcome::AlreadyActive))
            }
        }
    }

    // -----------------------------------------------------------------------
    // domains
    // -----------------------------------------------------------------------

    async fn domain_registrar(
        &self,
        tenant_id: TenantId,
        d: &DomainRow,
    ) -> BillingResult<Option<IntegrationId>> {
        if let Some(id) = d.registrar_integration_id {
            return Ok(Some(id));
        }
        let tld = domains::match_tld(self.pool(), tenant_id, &d.name).await?;
        if let Some(id) = tld.and_then(|t| t.registrar_integration_id) {
            return Ok(Some(id));
        }
        self.registry()
            .default_id(tenant_id, IntegrationCategory::Registrar)
            .await
    }

    async fn domain_manual(
        &self,
        tenant_id: TenantId,
        d: &DomainRow,
        what: &str,
    ) -> BillingResult<DomainOutcome> {
        let mut conn = self.pool().acquire().await?;
        self.notify_staff(
            &mut conn,
            tenant_id,
            "domain.manual_action",
            &format!("{} needs manual {what}", d.name),
            Some("No registrar integration is configured for this TLD."),
            Some(&format!("/admin/domains/{}", d.id)),
            &json!({"domain_id": d.id, "action": what}),
        )
        .await?;
        Ok(DomainOutcome::Manual)
    }

    async fn domain_failed(
        &self,
        tenant_id: TenantId,
        d: &DomainRow,
        what: &str,
        err: CoreError,
        ctx: RunContext,
    ) -> BillingResult<DomainOutcome> {
        if is_retryable(&err) && !ctx.final_attempt {
            domains::record_sync_error(self.pool(), tenant_id, d.id, &err.to_string()).await?;
            return Err(err);
        }
        let reason = err.to_string();
        let mut tx = self.pool().begin().await?;
        let cur = domains::get_domain(&mut *tx, tenant_id, d.id)
            .await?
            .ok_or_else(|| CoreError::not_found("domain", d.id))?;
        if matches!(
            cur.status,
            DomainStatus::PendingRegistration | DomainStatus::PendingTransfer
        ) {
            domains::transition_domain(
                &mut *tx,
                tenant_id,
                d.id,
                cur.status,
                DomainStatus::Pending,
            )
            .await?;
        }
        domains::record_sync_error(&mut *tx, tenant_id, d.id, &reason).await?;
        self.notify_staff(
            &mut tx,
            tenant_id,
            "domain.failed",
            &format!("Domain {what} failed for {}", d.name),
            Some(&reason),
            Some(&format!("/admin/domains/{}", d.id)),
            &json!({"domain_id": d.id, "action": what}),
        )
        .await?;
        tx.commit().await?;
        Ok(DomainOutcome::Failed { reason })
    }

    /// Stores a completed registration / transfer / renewal and activates the domain.
    async fn domain_completed(
        &self,
        tenant_id: TenantId,
        d: &DomainRow,
        op: &DomainOperation,
        years: i16,
        event_name: &str,
    ) -> BillingResult<()> {
        let now = self.clock().now();
        let base = match (event_name, d.expires_at) {
            ("domain.renewed", Some(e)) => e,
            _ => now,
        };
        let expires = op
            .expires_at
            .unwrap_or_else(|| expiry_after_years(base, years));
        let settings = BillingSettings::load(self.pool(), tenant_id).await?;
        let next_due = expires.date_naive();
        let next_invoice = next_due.checked_sub_days(Days::new(
            u64::try_from(settings.invoice_days_before_due.max(14)).unwrap_or(14),
        ));
        let mut tx = self.pool().begin().await?;
        let cur = domains::get_domain(&mut *tx, tenant_id, d.id)
            .await?
            .ok_or_else(|| CoreError::not_found("domain", d.id))?;
        domains::update_registration(
            &mut *tx,
            tenant_id,
            d.id,
            op.registrar_ref.as_deref(),
            (event_name != "domain.renewed" && cur.registered_at.is_none()).then_some(now),
            Some(expires),
            Some(next_due),
            None,
        )
        .await?;
        billing_support::set_domain_due_dates(
            &mut *tx,
            tenant_id,
            d.id,
            Some(next_due),
            next_invoice,
        )
        .await?;
        if cur.status != DomainStatus::Active && cur.status.can_transition_to(DomainStatus::Active)
        {
            domains::transition_domain(&mut *tx, tenant_id, d.id, cur.status, DomainStatus::Active)
                .await?;
        }
        self.emit_event(
            &mut tx,
            tenant_id,
            event_name,
            json!({"domain_id": d.id, "name": d.name, "expires_at": expires}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Registers or transfers in a domain (the `register_domain` /
    /// `transfer_domain` jobs). Safe to re-run.
    pub async fn register_domain(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        ctx: RunContext,
    ) -> BillingResult<DomainOutcome> {
        let d = domains::get_domain(self.pool(), tenant_id, domain_id)
            .await?
            .ok_or_else(|| CoreError::not_found("domain", domain_id))?;
        match d.status {
            DomainStatus::Active | DomainStatus::Grace | DomainStatus::Redemption => {
                return Ok(DomainOutcome::AlreadyDone);
            }
            DomainStatus::Pending
            | DomainStatus::PendingRegistration
            | DomainStatus::PendingTransfer => {}
            other => {
                return Ok(DomainOutcome::Skipped {
                    reason: format!("domain is {other}"),
                });
            }
        }
        let transfer = d.registration_type == DomainRegistrationType::Transfer;
        let what = if transfer { "transfer" } else { "registration" };
        let Some(registrar_id) = self.domain_registrar(tenant_id, &d).await? else {
            return self.domain_manual(tenant_id, &d, what).await;
        };
        match self
            .register_domain_inner(tenant_id, &d, registrar_id, transfer)
            .await
        {
            Ok(o) => Ok(o),
            Err(e) => self.domain_failed(tenant_id, &d, what, e, ctx).await,
        }
    }

    async fn register_domain_inner(
        &self,
        tenant_id: TenantId,
        d: &DomainRow,
        registrar_id: IntegrationId,
        transfer: bool,
    ) -> BillingResult<DomainOutcome> {
        let registrar = self.registry().registrar(tenant_id, registrar_id).await?;
        let client = clients::get_client(self.pool(), tenant_id, d.client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", d.client_id))?;
        let contacts = DomainContacts::same(client_contact(&client)?);
        let settings = BillingSettings::load(self.pool(), tenant_id).await?;
        let nameservers = if d.nameservers.is_empty() {
            settings.default_nameservers.clone()
        } else {
            d.nameservers.clone()
        };
        let years = d.registration_period.max(1);
        let pending_status = if transfer {
            DomainStatus::PendingTransfer
        } else {
            DomainStatus::PendingRegistration
        };
        if d.registrar_integration_id.is_none() {
            billing_support::set_domain_registrar(self.pool(), tenant_id, d.id, Some(registrar_id))
                .await?;
        }
        if d.status == DomainStatus::Pending {
            domains::transition_domain(
                self.pool(),
                tenant_id,
                d.id,
                DomainStatus::Pending,
                pending_status,
            )
            .await?;
        }
        let result = if transfer {
            let sealed = d
                .epp_code_enc
                .as_deref()
                .ok_or_else(|| CoreError::validation("a transfer needs the EPP / auth code"))?;
            let auth_code = secrets::open(
                self.secret_box(),
                sealed,
                &secrets::epp_code_aad(tenant_id, &d.name),
            )?;
            registrar
                .transfer(&DomainTransfer {
                    domain: d.name.clone(),
                    auth_code,
                    years: u32::try_from(years).unwrap_or(1),
                    nameservers,
                    contacts,
                    privacy: d.privacy,
                    auto_renew: false,
                    extra: Value::Null,
                })
                .await
        } else {
            registrar
                .register(&DomainRegistration {
                    domain: d.name.clone(),
                    years: u32::try_from(years).unwrap_or(1),
                    nameservers,
                    contacts,
                    privacy: d.privacy,
                    auto_renew: false,
                    premium_price: None,
                    extra: Value::Null,
                })
                .await
        };
        let op = match result {
            Ok(op) => op,
            Err(IntegrationError::Conflict(msg)) => {
                // Already ours at this registrar (earlier run)? Adopt it.
                match registrar.info(&d.name).await {
                    Ok(info)
                        if info.status.is_renewable() || info.status == DomainStatus::Active =>
                    {
                        DomainOperation {
                            domain: d.name.clone(),
                            status: DomainOperationStatus::Completed,
                            registrar_ref: info.registrar_ref,
                            expires_at: info.expires_at,
                            cost: None,
                            message: Some("adopted existing registration".into()),
                        }
                    }
                    _ => return Err(IntegrationError::Conflict(msg).into()),
                }
            }
            Err(e) => return Err(e.into()),
        };
        match op.status {
            DomainOperationStatus::Completed => {
                let name = if transfer {
                    "domain.transferred"
                } else {
                    "domain.registered"
                };
                self.domain_completed(tenant_id, d, &op, years, name)
                    .await?;
                Ok(DomainOutcome::Completed)
            }
            DomainOperationStatus::Pending => {
                domains::update_registration(
                    self.pool(),
                    tenant_id,
                    d.id,
                    op.registrar_ref.as_deref(),
                    None,
                    None,
                    None,
                    None,
                )
                .await?;
                Ok(DomainOutcome::Pending)
            }
        }
    }

    /// Renews a domain at its registrar (the `renew_domain` job, after the
    /// renewal invoice is paid). A re-run after a lost response detects the
    /// new expiry instead of renewing twice.
    pub async fn renew_domain(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
        years: Option<i16>,
        ctx: RunContext,
    ) -> BillingResult<DomainOutcome> {
        let d = domains::get_domain(self.pool(), tenant_id, domain_id)
            .await?
            .ok_or_else(|| CoreError::not_found("domain", domain_id))?;
        if !(d.status.is_renewable() || d.status == DomainStatus::Expired) {
            return Ok(DomainOutcome::Skipped {
                reason: format!("domain is {}", d.status),
            });
        }
        let years = years.unwrap_or(d.registration_period).max(1);
        let Some(registrar_id) = self.domain_registrar(tenant_id, &d).await? else {
            return self.domain_manual(tenant_id, &d, "renewal").await;
        };
        let res: BillingResult<DomainOutcome> = async {
            let registrar = self.registry().registrar(tenant_id, registrar_id).await?;
            let op = match registrar
                .renew(&DomainRenewal {
                    domain: d.name.clone(),
                    years: u32::try_from(years).unwrap_or(1),
                    current_expiry: d.expires_at,
                    restore_if_needed: matches!(
                        d.status,
                        DomainStatus::Redemption | DomainStatus::Expired
                    ),
                })
                .await
            {
                Ok(op) => op,
                Err(IntegrationError::Conflict(msg)) => {
                    let info = registrar.info(&d.name).await?;
                    match (info.expires_at, d.expires_at) {
                        (Some(new), Some(old)) if new > old => DomainOperation {
                            domain: d.name.clone(),
                            status: DomainOperationStatus::Completed,
                            registrar_ref: info.registrar_ref,
                            expires_at: Some(new),
                            cost: None,
                            message: Some("renewal already applied".into()),
                        },
                        _ => return Err(IntegrationError::Conflict(msg).into()),
                    }
                }
                Err(e) => return Err(e.into()),
            };
            if op.status == DomainOperationStatus::Pending && op.expires_at.is_none() {
                return Ok(DomainOutcome::Pending);
            }
            self.domain_completed(tenant_id, &d, &op, years, "domain.renewed")
                .await?;
            Ok(DomainOutcome::Completed)
        }
        .await;
        match res {
            Ok(o) => Ok(o),
            Err(e) => self.domain_failed(tenant_id, &d, "renewal", e, ctx).await,
        }
    }

    /// Staff action: queue provisioning again for a pending / failed service.
    pub async fn retry_provisioning(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
    ) -> BillingResult<()> {
        let mut conn = self.pool().acquire().await?;
        let svc = services::get_service(&mut *conn, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        if !matches!(
            svc.status,
            ServiceStatus::Pending | ServiceStatus::Failed | ServiceStatus::Provisioning
        ) {
            return Err(CoreError::Conflict(format!("service is {}", svc.status)));
        }
        jobs::enqueue_provision(&mut conn, tenant_id, service_id, None).await?;
        Ok(())
    }
}
