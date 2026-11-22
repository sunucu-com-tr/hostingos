//! Daily billing automation (the `daily_automation` job, one per tenant):
//!
//! 1. renewal invoices `invoice_days_before_due` days ahead (one invoice per
//!    client, currency and due date; `separate_invoices` clients get one per
//!    item), limited recurring promotions consumed;
//! 2. payment reminders before the due date and the 1st/2nd/3rd overdue notices;
//! 3. optional late fee;
//! 4. suspension `suspend_after_days` after the due date, termination
//!    `terminate_after_days` after it (plus end-of-period cancellations);
//! 5. domain expiry notices and stored-card charges of due invoices.
//!
//! Decisions are pure functions ([`reminder_for`], [`late_fee_for`],
//! [`expiry_notice`], [`group_renewals`]) applied by [`Billing::run_daily`].
//! Every effect is idempotent (open-invoice checks, `automation_marks`, job
//! dedupe keys), so re-running a day is harmless. The business date comes
//! from the injected [`Clock`](crate::Clock).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use chrono::{Days, NaiveDate};
use hos_core::CoreError;
use hos_core::domain::{DomainStatus, IntegrationCategory, InvoiceStatus, ServiceStatus};
use hos_core::ids::{ClientId, DomainId, IntegrationId, InvoiceId, ServiceId, TenantId};
use hos_core::integrations::panel::AccountRef;
use hos_core::integrations::registrar::TldPrice;
use hos_core::integrations::virt::VmRef;
use hos_core::money::{Currency, Money};
use hos_db::enums::{InvoiceItemType, TldAction};
use hos_db::repo::billing_support;
use hos_db::repo::domains::{self, DomainRow, TldPriceInput};
use hos_db::repo::invoices::{self, InvoiceRow, NewInvoice, NewInvoiceItem};
use hos_db::repo::services::{self, ProvisioningUpdate, ServiceRow};
use hos_db::repo::{catalog, clients, integrations, payments};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use sqlx::PgConnection;

use crate::checkout::billing_snapshot;
use crate::jobs::{self, ChargeInvoicePayload, JobKind, JobRequest, ServicePayload};
use crate::payments::SUSPEND_REASON_OVERDUE;
use crate::pricing::{self, TaxContext};
use crate::settings::{BillingSettings, LateFee};
use crate::{Billing, BillingResult};

const BATCH: i64 = 500;

// ---------------------------------------------------------------------------
// pure decisions
// ---------------------------------------------------------------------------

/// Which reminder an open invoice gets today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReminderKind {
    /// `days` before the due date.
    BeforeDue { days: i32 },
    /// The `notice`-th (1-based) overdue notice, `days` after the due date.
    Overdue { notice: usize, days: i32 },
}

/// Reminder due for an invoice with `due_date` on `today`, if any.
pub fn reminder_for(
    due_date: NaiveDate,
    today: NaiveDate,
    settings: &BillingSettings,
) -> Option<ReminderKind> {
    let diff = i32::try_from((due_date - today).num_days()).ok()?;
    if diff > 0 && settings.reminder_days_before_due.contains(&diff) {
        return Some(ReminderKind::BeforeDue { days: diff });
    }
    if diff < 0 {
        let overdue = -diff;
        if let Some(i) = settings
            .overdue_notice_days
            .iter()
            .position(|d| *d == overdue)
        {
            return Some(ReminderKind::Overdue {
                notice: i + 1,
                days: overdue,
            });
        }
    }
    None
}

/// Late fee for an invoice `days_overdue` days past due (`None` when not due
/// yet or disabled). Percent fees use the invoice total with `min_amount` as
/// a floor; the result is rounded to the currency.
pub fn late_fee_for(
    total: Decimal,
    days_overdue: i64,
    fee: &LateFee,
    currency: Currency,
) -> Option<Decimal> {
    if !fee.enabled() || days_overdue <= i64::from(fee.after_days.max(0)) {
        return None;
    }
    let amount = if fee.percent {
        (total * fee.amount / Decimal::ONE_HUNDRED).max(fee.min_amount)
    } else {
        fee.amount
    };
    let rounded = Money::new(amount, currency).round().amount;
    (rounded > Decimal::ZERO).then_some(rounded)
}

/// Days left until `expires` when today is one of the notice days.
pub fn expiry_notice(expires: NaiveDate, today: NaiveDate, notice_days: &[i32]) -> Option<i32> {
    let left = i32::try_from((expires - today).num_days()).ok()?;
    notice_days.contains(&left).then_some(left)
}

/// One line of a renewal invoice to create.
#[derive(Debug, Clone)]
pub struct RenewalLine {
    pub client_id: ClientId,
    pub currency: Currency,
    pub due_date: NaiveDate,
    pub item: NewInvoiceItem,
    /// Service whose limited promotion is consumed by this renewal.
    pub service_id: Option<ServiceId>,
}

/// Lines grouped into one invoice.
#[derive(Debug, Clone)]
pub struct RenewalGroup {
    pub client_id: ClientId,
    pub currency: Currency,
    pub due_date: NaiveDate,
    pub lines: Vec<RenewalLine>,
}

/// Groups renewal lines per client, currency and due date (one invoice
/// each); clients in `separate` get one invoice per line.
pub fn group_renewals(lines: Vec<RenewalLine>, separate: &HashSet<ClientId>) -> Vec<RenewalGroup> {
    let mut groups: BTreeMap<(String, String, NaiveDate, usize), RenewalGroup> = BTreeMap::new();
    for (i, line) in lines.into_iter().enumerate() {
        let split = if separate.contains(&line.client_id) {
            i
        } else {
            usize::MAX
        };
        let key = (
            line.client_id.to_string(),
            line.currency.to_string(),
            line.due_date,
            split,
        );
        groups
            .entry(key)
            .or_insert_with(|| RenewalGroup {
                client_id: line.client_id,
                currency: line.currency,
                due_date: line.due_date,
                lines: Vec::new(),
            })
            .lines
            .push(line);
    }
    groups.into_values().collect()
}

/// A domain renewal invoice line for `[start, start + years)`.
pub fn domain_renewal_item(
    d: &DomainRow,
    years: i16,
    start: NaiveDate,
    amount: Decimal,
) -> NewInvoiceItem {
    let years = years.max(1);
    NewInvoiceItem {
        item_type: InvoiceItemType::Domain,
        domain_id: Some(d.id),
        period_start: Some(start),
        period_end: crate::lifecycle::years_period_end(start, years),
        ..NewInvoiceItem::manual(
            &format!(
                "{} - {} {}",
                d.name,
                years,
                if years == 1 { "year" } else { "years" }
            ),
            amount,
        )
    }
}

/// What a daily run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DailyReport {
    pub as_of: Option<NaiveDate>,
    pub invoices_created: usize,
    pub reminders_sent: usize,
    pub overdue_notices_sent: usize,
    pub late_fees_added: usize,
    pub suspensions_queued: usize,
    pub terminations_queued: usize,
    pub domain_notices_sent: usize,
    pub charges_queued: usize,
}

/// What a sync run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    pub checked: usize,
    pub updated: usize,
    pub errors: usize,
}

impl Billing {
    /// Creates an issued renewal invoice for one client (taxes from the
    /// client's profile), applies credit when enabled and queues the
    /// `invoice_created` e-mail. Returns the invoice id.
    pub async fn create_renewal_invoice(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        client_id: ClientId,
        currency: Currency,
        due_date: NaiveDate,
        items: Vec<NewInvoiceItem>,
    ) -> BillingResult<InvoiceId> {
        let client = clients::get_client(&mut *conn, tenant_id, client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", client_id))?;
        let settings = BillingSettings::load(&mut *conn, tenant_id).await?;
        let taxes =
            pricing::tax_rates(conn, tenant_id, &TaxContext::for_client(&client, &settings))
                .await?;
        let today = self.clock().today();
        let invoice = invoices::create_invoice(
            &mut *conn,
            tenant_id,
            &NewInvoice {
                client_id,
                currency,
                issue_date: today,
                due_date: due_date.max(today),
                issue: true,
                payment_integration_id: client.default_gateway_id,
                notes: None,
                billing_snapshot: billing_snapshot(&client),
                items,
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
        let mut paid = false;
        if settings.apply_credit_automatically && invoice.balance > Decimal::ZERO {
            let (after, _) =
                payments::apply_credit_to_invoice(&mut *conn, tenant_id, invoice.id, None, None)
                    .await?;
            paid = after.status == InvoiceStatus::Paid;
        }
        if invoice.balance <= Decimal::ZERO && invoice.status == InvoiceStatus::Unpaid {
            invoices::transition_invoice(
                &mut *conn,
                tenant_id,
                invoice.id,
                &[InvoiceStatus::Unpaid],
                InvoiceStatus::Paid,
            )
            .await?;
            paid = true;
        }
        if paid {
            self.after_invoice_paid(conn, tenant_id, invoice.id).await?;
        } else {
            self.queue_invoice_created_email(conn, tenant_id, &invoice)
                .await?;
        }
        self.emit_event(
            conn,
            tenant_id,
            "invoice.created",
            json!({"invoice_id": invoice.id, "client_id": client_id, "total": invoice.total, "currency": currency}),
        )
        .await?;
        Ok(invoice.id)
    }

    async fn service_renewal_lines(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        svc: &ServiceRow,
    ) -> BillingResult<Vec<RenewalLine>> {
        let Some(start) = svc.next_due_date else {
            return Ok(Vec::new());
        };
        let product = catalog::get_product(&mut *conn, tenant_id, svc.product_id)
            .await?
            .ok_or_else(|| CoreError::not_found("product", svc.product_id))?;
        let end = svc.billing_cycle.period_end(start);
        let label = match (&svc.domain, end) {
            (Some(d), Some(e)) => format!("{} - {d} ({start} - {e})", product.name),
            (None, Some(e)) => format!("{} ({start} - {e})", product.name),
            (Some(d), None) => format!("{} - {d}", product.name),
            (None, None) => product.name.clone(),
        };
        let mut out = vec![RenewalLine {
            client_id: svc.client_id,
            currency: svc.currency,
            due_date: start,
            item: NewInvoiceItem {
                taxable: product.is_taxable,
                ..NewInvoiceItem::service(svc.id, &label, svc.recurring_amount, start, end)
            },
            service_id: Some(svc.id),
        }];
        for addon in services::service_addons(&mut *conn, tenant_id, svc.id).await? {
            if !matches!(
                addon.status,
                ServiceStatus::Active | ServiceStatus::Suspended
            ) || addon.recurring_amount <= Decimal::ZERO
                || addon.currency != svc.currency
            {
                continue;
            }
            let name = catalog::get_addon(&mut *conn, tenant_id, addon.addon_id)
                .await?
                .map(|a| a.name)
                .unwrap_or_else(|| "Addon".into());
            out.push(RenewalLine {
                client_id: svc.client_id,
                currency: svc.currency,
                due_date: start,
                item: NewInvoiceItem {
                    item_type: InvoiceItemType::Addon,
                    service_id: Some(svc.id),
                    service_addon_id: Some(addon.id),
                    period_start: Some(start),
                    period_end: end,
                    taxable: product.is_taxable,
                    ..NewInvoiceItem::manual(&name, addon.recurring_amount)
                },
                service_id: None,
            });
        }
        Ok(out)
    }

    /// Runs the daily automation of a tenant for `as_of` (default: the
    /// clock's today).
    pub async fn run_daily(
        &self,
        tenant_id: TenantId,
        as_of: Option<NaiveDate>,
    ) -> BillingResult<DailyReport> {
        let today = as_of.unwrap_or_else(|| self.clock().today());
        let now = self.clock().now();
        let mut conn = self.pool().acquire().await?;
        let settings = BillingSettings::load(&mut *conn, tenant_id).await?;
        let mut report = DailyReport {
            as_of: Some(today),
            ..DailyReport::default()
        };

        // 1. renewal invoices --------------------------------------------------
        let mut lines = Vec::new();
        for svc in services::services_due_for_invoice(
            &mut *conn,
            tenant_id,
            today,
            settings.invoice_days_before_due,
            BATCH,
        )
        .await?
        {
            lines.extend(
                self.service_renewal_lines(&mut conn, tenant_id, &svc)
                    .await?,
            );
        }
        for d in domains::domains_due_for_invoice(
            &mut *conn,
            tenant_id,
            today,
            settings.invoice_days_before_due.max(14),
            BATCH,
        )
        .await?
        {
            let Some(start) = d.next_due_date else {
                continue;
            };
            let years = d.registration_period.max(1);
            let mut item = domain_renewal_item(&d, years, start, d.recurring_amount);
            item.taxable = settings.tax_domains;
            lines.push(RenewalLine {
                client_id: d.client_id,
                currency: d.currency,
                due_date: start,
                item,
                service_id: None,
            });
        }
        let mut separate = HashSet::new();
        for client_id in lines.iter().map(|l| l.client_id).collect::<HashSet<_>>() {
            if clients::get_client(&mut *conn, tenant_id, client_id)
                .await?
                .is_some_and(|c| c.separate_invoices)
            {
                separate.insert(client_id);
            }
        }
        for group in group_renewals(lines, &separate) {
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            let promo: Vec<ServiceId> = group.lines.iter().filter_map(|l| l.service_id).collect();
            self.create_renewal_invoice(
                &mut tx,
                tenant_id,
                group.client_id,
                group.currency,
                group.due_date,
                group.lines.into_iter().map(|l| l.item).collect(),
            )
            .await?;
            for s in promo {
                billing_support::consume_promo_cycle(&mut *tx, tenant_id, s).await?;
            }
            tx.commit().await?;
            report.invoices_created += 1;
        }

        // 2. reminders & overdue notices ----------------------------------------
        let horizon = settings
            .reminder_days_before_due
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        let upcoming = invoices::invoices_due_between(
            &mut *conn,
            tenant_id,
            today,
            today + Days::new(u64::try_from(horizon).unwrap_or(0)),
        )
        .await?;
        let overdue = invoices::overdue_invoices(&mut *conn, tenant_id, today, BATCH).await?;
        for inv in upcoming.iter().chain(overdue.iter()) {
            let Some(kind) = reminder_for(inv.due_date, today, &settings) else {
                continue;
            };
            let key = match kind {
                ReminderKind::BeforeDue { days } => format!("reminder:{}:before:{days}", inv.id),
                ReminderKind::Overdue { notice, .. } => {
                    format!("reminder:{}:overdue:{notice}", inv.id)
                }
            };
            let client = clients::get_client(&mut *conn, tenant_id, inv.client_id)
                .await?
                .ok_or_else(|| CoreError::not_found("client", inv.client_id))?;
            if matches!(kind, ReminderKind::Overdue { .. }) && !client.overdue_notices {
                continue;
            }
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            if !billing_support::mark_once(&mut *tx, tenant_id, &key).await? {
                tx.commit().await?;
                continue;
            }
            self.send_reminder(&mut tx, tenant_id, inv, kind, &settings)
                .await?;
            billing_support::record_reminder_at(&mut *tx, tenant_id, inv.id, now).await?;
            tx.commit().await?;
            match kind {
                ReminderKind::BeforeDue { .. } => report.reminders_sent += 1,
                ReminderKind::Overdue { .. } => report.overdue_notices_sent += 1,
            }
        }

        // 3. late fees ----------------------------------------------------------
        if settings.late_fee.enabled() {
            for inv in &overdue {
                if inv.late_fee_applied_at.is_some() || inv.status != InvoiceStatus::Unpaid {
                    continue;
                }
                let days = (today - inv.due_date).num_days();
                let Some(fee) = late_fee_for(inv.total, days, &settings.late_fee, inv.currency)
                else {
                    continue;
                };
                let exempt = clients::get_client(&mut *conn, tenant_id, inv.client_id)
                    .await?
                    .is_none_or(|c| c.late_fee_exempt);
                if exempt {
                    continue;
                }
                let mut tx = sqlx::Connection::begin(&mut *conn).await?;
                if billing_support::mark_late_fee_applied(&mut *tx, tenant_id, inv.id, now).await? {
                    invoices::add_invoice_items(
                        &mut tx,
                        tenant_id,
                        inv.id,
                        &[NewInvoiceItem {
                            item_type: InvoiceItemType::LateFee,
                            taxable: false,
                            ..NewInvoiceItem::manual("Late fee", fee)
                        }],
                    )
                    .await?;
                    report.late_fees_added += 1;
                }
                tx.commit().await?;
            }
        }

        // 4. suspension / termination --------------------------------------------
        for o in services::services_to_suspend(
            &mut *conn,
            tenant_id,
            today,
            settings.suspend_after_days,
            BATCH,
        )
        .await?
        {
            let res = jobs::enqueue_service_job(
                &mut conn,
                tenant_id,
                JobKind::SuspendService,
                &ServicePayload {
                    service_id: o.service.id,
                    reason: Some(SUSPEND_REASON_OVERDUE.into()),
                    actor: None,
                },
                Some(format!(
                    "suspend:{}:{}",
                    o.service.id,
                    o.overdue_invoice_id
                        .map(|i| i.to_string())
                        .unwrap_or_default()
                )),
            )
            .await?;
            report.suspensions_queued += usize::from(res.is_created());
        }
        let terminate_days = if settings.terminate_after_days == 0 {
            i32::MAX / 2
        } else {
            settings.terminate_after_days
        };
        for o in
            services::services_to_terminate(&mut *conn, tenant_id, today, terminate_days, BATCH)
                .await?
        {
            let res = jobs::enqueue_service_job(
                &mut conn,
                tenant_id,
                JobKind::TerminateService,
                &ServicePayload {
                    service_id: o.service.id,
                    reason: Some(if o.service.cancellation_requested_at.is_some() {
                        "cancelled".into()
                    } else {
                        "overdue".into()
                    }),
                    actor: None,
                },
                Some(format!("terminate:{}", o.service.id)),
            )
            .await?;
            report.terminations_queued += usize::from(res.is_created());
        }

        // 5. domain expiry notices ------------------------------------------------
        let max_notice = settings
            .domain_expiry_notice_days
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        let from = today
            .and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc())
            .unwrap_or(now);
        let to = from + chrono::Duration::days(i64::from(max_notice) + 1);
        let url = self.tenant_url(tenant_id).await?;
        for d in billing_support::domains_expiring_between(&mut *conn, tenant_id, from, to).await? {
            let Some(exp) = d.expires_at else { continue };
            let Some(left) =
                expiry_notice(exp.date_naive(), today, &settings.domain_expiry_notice_days)
            else {
                continue;
            };
            let key = format!("domain_notice:{}:{}:{left}", d.id, exp.date_naive());
            let client = clients::get_client(&mut *conn, tenant_id, d.client_id)
                .await?
                .ok_or_else(|| CoreError::not_found("client", d.client_id))?;
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            if billing_support::mark_once(&mut *tx, tenant_id, &key).await? {
                self.queue_client_email(
                    &mut tx,
                    tenant_id,
                    d.client_id,
                    hos_mail::defaults::keys::DOMAIN_EXPIRING,
                    json!({"domain": {
                        "name": d.name,
                        "expires_on": crate::format::date(exp.date_naive(), client.locale),
                        "days_left": left,
                        "auto_renew": d.auto_renew,
                        "renew_url": format!("{url}/client/domains/{}", d.id),
                    }}),
                    Some(("domain", *d.id.as_uuid())),
                )
                .await?;
                report.domain_notices_sent += 1;
            }
            tx.commit().await?;
        }

        // 6. stored-method charges of due invoices -------------------------------
        if settings.auto_charge_stored {
            for inv in billing_support::invoices_for_autocharge(&mut *conn, tenant_id, today, BATCH)
                .await?
            {
                let res = jobs::enqueue(
                    &mut conn,
                    JobRequest::new(
                        JobKind::ChargeInvoice,
                        Some(tenant_id),
                        &ChargeInvoicePayload { invoice_id: inv.id },
                    )
                    .dedupe(format!("charge:{}:{today}", inv.id)),
                )
                .await?;
                report.charges_queued += usize::from(res.is_created());
            }
        }

        // Quotes and the affiliate programme (failures never stop the run).
        drop(conn);
        match self.expire_quotes(tenant_id, today).await {
            Ok(n) if n > 0 => tracing::info!(%tenant_id, expired = n, "quotes expired"),
            Ok(_) => {}
            Err(e) => tracing::warn!(%tenant_id, error = %e, "quote expiry failed"),
        }
        match self.cancel_stale_mass_payments(tenant_id).await {
            Ok(n) if n > 0 => {
                tracing::info!(%tenant_id, cancelled = n, "stale mass payments cancelled")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%tenant_id, error = %e, "mass payment clean-up failed"),
        }
        match self.run_affiliate_maintenance(tenant_id).await {
            Ok((reversed, cleared)) if reversed + usize::try_from(cleared).unwrap_or(0) > 0 => {
                tracing::info!(%tenant_id, reversed, cleared, "affiliate commissions updated");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%tenant_id, error = %e, "affiliate maintenance failed"),
        }

        tracing::info!(%tenant_id, %today, ?report, "daily automation finished");
        Ok(report)
    }

    async fn send_reminder(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        inv: &InvoiceRow,
        kind: ReminderKind,
        settings: &BillingSettings,
    ) -> BillingResult<()> {
        let client = clients::get_client(&mut *conn, tenant_id, inv.client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", inv.client_id))?;
        let url = self.tenant_url(tenant_id).await?;
        let loc = client.locale;
        let base = json!({
            "number": inv.number.clone().unwrap_or_default(),
            "total": crate::format::money(&inv.balance_money(), loc),
            "due_date": crate::format::date(inv.due_date, loc),
            "url": format!("{url}/client/invoices/{}", inv.id),
            "notes": inv.notes.clone().unwrap_or_default(),
        });
        let (key, ctx) = match kind {
            ReminderKind::BeforeDue { .. } => (
                hos_mail::defaults::keys::INVOICE_CREATED,
                json!({"invoice": base}),
            ),
            ReminderKind::Overdue { days, .. } => {
                let mut b = base;
                b["days_overdue"] = json!(days);
                b["suspension_date"] = json!(crate::format::date(
                    inv.due_date
                        + Days::new(u64::try_from(settings.suspend_after_days).unwrap_or(0) + 1),
                    loc
                ));
                (
                    hos_mail::defaults::keys::INVOICE_OVERDUE,
                    json!({"invoice": b}),
                )
            }
        };
        self.queue_client_email(
            conn,
            tenant_id,
            inv.client_id,
            key,
            ctx,
            Some(("invoice", *inv.id.as_uuid())),
        )
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // registrar / provider synchronisation
    // -----------------------------------------------------------------------

    /// Synchronises registrar status and expiry of domains (the
    /// `sync_domain_status` job); finishes pending registrations / transfers.
    pub async fn sync_domain_status(
        &self,
        tenant_id: TenantId,
        limit: i64,
    ) -> BillingResult<SyncReport> {
        let mut report = SyncReport::default();
        let settings = BillingSettings::load(self.pool(), tenant_id).await?;
        for d in billing_support::domains_to_sync(self.pool(), tenant_id, limit).await? {
            report.checked += 1;
            let Some(registrar_id) = d.registrar_integration_id else {
                continue;
            };
            let res = self
                .sync_one_domain(tenant_id, &d, registrar_id, &settings)
                .await;
            match res {
                Ok(changed) => report.updated += usize::from(changed),
                Err(e) => {
                    report.errors += 1;
                    domains::record_sync_error(self.pool(), tenant_id, d.id, &e.to_string())
                        .await?;
                }
            }
        }
        Ok(report)
    }

    /// Syncs one domain with its registrar (status, expiry, nameservers);
    /// `true` when something changed. Errors are recorded on the domain.
    pub async fn sync_domain(
        &self,
        tenant_id: TenantId,
        domain_id: DomainId,
    ) -> BillingResult<bool> {
        let d = domains::get_domain(self.pool(), tenant_id, domain_id)
            .await?
            .ok_or_else(|| CoreError::not_found("domain", domain_id))?;
        let Some(registrar_id) = d.registrar_integration_id else {
            return Err(CoreError::validation("this domain has no registrar"));
        };
        let settings = BillingSettings::load(self.pool(), tenant_id).await?;
        let res = self
            .sync_one_domain(tenant_id, &d, registrar_id, &settings)
            .await;
        if let Err(e) = &res {
            domains::record_sync_error(self.pool(), tenant_id, d.id, &e.to_string()).await?;
        }
        res
    }

    async fn sync_one_domain(
        &self,
        tenant_id: TenantId,
        d: &DomainRow,
        registrar_id: IntegrationId,
        settings: &BillingSettings,
    ) -> BillingResult<bool> {
        let registrar = self.registry().registrar(tenant_id, registrar_id).await?;
        let info = registrar.info(&d.name).await?;
        let mut changed = false;
        let mut tx = self.pool().begin().await?;
        let expiry_moved = match (info.expires_at, d.expires_at) {
            (Some(new), Some(old)) => new.date_naive() != old.date_naive(),
            (Some(_), None) => true,
            _ => false,
        };
        if expiry_moved || info.registrar_ref.is_some() {
            domains::update_registration(
                &mut *tx,
                tenant_id,
                d.id,
                info.registrar_ref.as_deref(),
                info.created_at,
                info.expires_at,
                info.expires_at.map(|e| e.date_naive()),
                Some(&info.nameservers),
            )
            .await?;
            if let Some(e) = info.expires_at.filter(|_| expiry_moved) {
                let due = e.date_naive();
                let next_invoice = due.checked_sub_days(Days::new(
                    u64::try_from(settings.invoice_days_before_due.max(14)).unwrap_or(14),
                ));
                billing_support::set_domain_due_dates(
                    &mut *tx,
                    tenant_id,
                    d.id,
                    Some(due),
                    next_invoice,
                )
                .await?;
            }
            changed |= expiry_moved;
        }
        let target = match (d.status, info.status) {
            (
                DomainStatus::PendingRegistration | DomainStatus::PendingTransfer,
                DomainStatus::Active,
            ) => Some(DomainStatus::Active),
            (cur, s)
                if cur != s && cur.can_transition_to(s) && !matches!(s, DomainStatus::Pending) =>
            {
                Some(s)
            }
            _ => None,
        };
        if let Some(t) = target {
            changed |= domains::transition_domain(&mut *tx, tenant_id, d.id, d.status, t).await?;
            if t == DomainStatus::Active && d.status != DomainStatus::Active {
                self.emit_event(
                    &mut tx,
                    tenant_id,
                    "domain.registered",
                    json!({"domain_id": d.id, "name": d.name}),
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Refreshes TLD selling prices from each registrar's cost with the
    /// tenant's margin (`domains.price_margin`), in every active tenant
    /// currency (converted with the tenant's exchange rates).
    pub async fn sync_tld_pricing(&self, tenant_id: TenantId) -> BillingResult<SyncReport> {
        let mut report = SyncReport::default();
        let settings = BillingSettings::load(self.pool(), tenant_id).await?;
        let rule = hos_domains::pricing::PricingRule {
            margin: hos_domains::pricing::Margin::PercentPlusFixed {
                percent: settings.price_margin.percent,
                amount: settings.price_margin.fixed,
            },
            ..hos_domains::pricing::PricingRule::default()
        };
        let currencies: Vec<_> = catalog::list_currencies(self.pool(), tenant_id, true)
            .await?
            .into_iter()
            .filter(|c| c.is_active)
            .collect();
        let default_registrar = self
            .registry()
            .default_id(tenant_id, IntegrationCategory::Registrar)
            .await?;
        let tlds = domains::list_tlds(self.pool(), tenant_id, true).await?;
        for reg in integrations::active_integrations(
            self.pool(),
            tenant_id,
            IntegrationCategory::Registrar,
        )
        .await?
        {
            let mine: Vec<_> = tlds
                .iter()
                .filter(|t| t.registrar_integration_id.or(default_registrar) == Some(reg.id))
                .collect();
            if mine.is_empty() {
                continue;
            }
            let names: Vec<String> = mine.iter().map(|t| t.tld.clone()).collect();
            let costs: Vec<TldPrice> = match self.registry().registrar(tenant_id, reg.id).await {
                Ok(r) => match r.tld_pricing(&names).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(%tenant_id, registrar = %reg.id, error = %e, "TLD price sync failed");
                        report.errors += 1;
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(%tenant_id, registrar = %reg.id, error = %e, "registrar unavailable");
                    report.errors += 1;
                    continue;
                }
            };
            for cost in costs {
                report.checked += 1;
                let Some(tld) = mine
                    .iter()
                    .find(|t| t.tld.eq_ignore_ascii_case(cost.tld.trim_start_matches('.')))
                else {
                    continue;
                };
                let source = [&cost.register, &cost.renew, &cost.transfer, &cost.restore]
                    .into_iter()
                    .flatten()
                    .map(|m| m.currency)
                    .next();
                let Some(source) = source else { continue };
                let source_rate = currencies
                    .iter()
                    .find(|c| c.code == source)
                    .map(|c| c.exchange_rate);
                let mut prices = Vec::new();
                for cur in &currencies {
                    let rate = if cur.code == source {
                        None
                    } else {
                        match source_rate {
                            Some(sr) if sr > Decimal::ZERO => Some(cur.exchange_rate / sr),
                            _ => continue,
                        }
                    };
                    let one = |m: &Option<Money>| -> Option<Decimal> {
                        m.as_ref()
                            .and_then(|m| rule.apply(m, cur.code, rate).ok())
                            .map(|p| p.amount)
                    };
                    for (action, price) in [
                        (TldAction::Register, one(&cost.register)),
                        (TldAction::Renew, one(&cost.renew)),
                        (TldAction::Transfer, one(&cost.transfer)),
                        (TldAction::Restore, one(&cost.restore)),
                    ] {
                        let Some(p) = price else { continue };
                        for years in tld.min_years.max(1)..=tld.max_years.clamp(1, 10) {
                            let years_mul =
                                if matches!(action, TldAction::Restore | TldAction::Transfer)
                                    && years > 1
                                {
                                    continue;
                                } else {
                                    Decimal::from(years)
                                };
                            prices.push(TldPriceInput {
                                currency: cur.code,
                                action,
                                years,
                                price: Money::new(p * years_mul, cur.code).round().amount,
                            });
                        }
                    }
                }
                if !prices.is_empty() {
                    domains::set_tld_prices(self.pool(), tenant_id, tld.id, &prices).await?;
                    report.updated += 1;
                }
            }
        }
        Ok(report)
    }

    /// Collects usage from panels / hypervisors into `services.module_data.usage`.
    pub async fn sync_usage(&self, tenant_id: TenantId, limit: i64) -> BillingResult<SyncReport> {
        let mut report = SyncReport::default();
        for svc in billing_support::services_with_integration(
            self.pool(),
            tenant_id,
            &[ServiceStatus::Active, ServiceStatus::Suspended],
            limit,
        )
        .await?
        {
            let Some(integration_id) = svc.integration_id else {
                continue;
            };
            report.checked += 1;
            let res: BillingResult<Option<serde_json::Value>> = async {
                let l = self.registry().get(tenant_id, integration_id).await?;
                Ok(match l.category {
                    IntegrationCategory::Panel => {
                        let Some(username) = svc.username.clone() else {
                            return Ok(None);
                        };
                        let panel = self.registry().panel(tenant_id, integration_id).await?;
                        let usage = panel
                            .usage(&AccountRef {
                                username,
                                domain: svc.domain.clone(),
                                id: svc.external_id.clone(),
                            })
                            .await?;
                        Some(serde_json::to_value(usage).unwrap_or_default())
                    }
                    IntegrationCategory::Virt => {
                        let Some(vm) =
                            services::vm_for_service(self.pool(), tenant_id, svc.id).await?
                        else {
                            return Ok(None);
                        };
                        let virt = self.registry().virt(tenant_id, integration_id).await?;
                        let m = virt
                            .metrics(&VmRef {
                                node: vm.node.clone(),
                                id: vm.external_id.clone(),
                            })
                            .await?;
                        services::set_vm_status(self.pool(), tenant_id, vm.id, m.status).await?;
                        Some(serde_json::to_value(m).unwrap_or_default())
                    }
                    _ => None,
                })
            }
            .await;
            match res {
                Ok(Some(usage)) => {
                    services::update_provisioning(
                        self.pool(),
                        tenant_id,
                        svc.id,
                        &ProvisioningUpdate {
                            module_data: Some(json!({"usage": usage})),
                            ..Default::default()
                        },
                    )
                    .await?;
                    report.updated += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    report.errors += 1;
                    tracing::debug!(%tenant_id, service = %svc.id, error = %e, "usage sync failed");
                }
            }
        }
        Ok(report)
    }

    /// Housekeeping (all tenants): expired carts and sessions, finished jobs
    /// older than `job_retention`, old automation markers.
    pub async fn cleanup(&self, job_retention: Duration) -> BillingResult<serde_json::Value> {
        let carts = hos_db::repo::carts::purge_expired_carts(self.pool()).await?;
        let sessions = hos_db::repo::identity::purge_expired_sessions(self.pool()).await?;
        let jobs = hos_db::repo::jobs::purge_finished_jobs(self.pool(), job_retention).await?;
        let marks =
            billing_support::purge_automation_marks(self.pool(), Duration::from_secs(400 * 86_400))
                .await?;
        Ok(json!({"carts": carts, "sessions": sessions, "jobs": jobs, "automation_marks": marks}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn reminders_follow_settings() {
        let s = BillingSettings::default(); // before [1, 3], overdue [1, 3, 7]
        let due = d("2026-03-10");
        assert_eq!(
            reminder_for(due, d("2026-03-07"), &s),
            Some(ReminderKind::BeforeDue { days: 3 })
        );
        assert_eq!(reminder_for(due, d("2026-03-08"), &s), None);
        assert_eq!(reminder_for(due, d("2026-03-10"), &s), None);
        assert_eq!(
            reminder_for(due, d("2026-03-11"), &s),
            Some(ReminderKind::Overdue { notice: 1, days: 1 })
        );
        assert_eq!(
            reminder_for(due, d("2026-03-13"), &s),
            Some(ReminderKind::Overdue { notice: 2, days: 3 })
        );
        assert_eq!(
            reminder_for(due, d("2026-03-17"), &s),
            Some(ReminderKind::Overdue { notice: 3, days: 7 })
        );
        assert_eq!(reminder_for(due, d("2026-03-18"), &s), None);
    }

    #[test]
    fn late_fee_percent_fixed_and_floor() {
        let pct = LateFee {
            percent: true,
            amount: dec!(10),
            min_amount: dec!(25),
            after_days: 3,
        };
        assert_eq!(late_fee_for(dec!(100), 3, &pct, Currency::TRY), None);
        assert_eq!(
            late_fee_for(dec!(100), 4, &pct, Currency::TRY),
            Some(dec!(25))
        );
        assert_eq!(
            late_fee_for(dec!(1000.55), 4, &pct, Currency::TRY),
            Some(dec!(100.06))
        );
        let fixed = LateFee {
            percent: false,
            amount: dec!(15),
            min_amount: Decimal::ZERO,
            after_days: 0,
        };
        assert_eq!(
            late_fee_for(dec!(1), 1, &fixed, Currency::TRY),
            Some(dec!(15))
        );
        let off = LateFee {
            amount: Decimal::ZERO,
            ..fixed
        };
        assert_eq!(late_fee_for(dec!(1), 10, &off, Currency::TRY), None);
    }

    #[test]
    fn expiry_notices() {
        let days = [30, 14, 7, 1];
        assert_eq!(
            expiry_notice(d("2026-05-31"), d("2026-05-01"), &days),
            Some(30)
        );
        assert_eq!(expiry_notice(d("2026-05-31"), d("2026-05-02"), &days), None);
        assert_eq!(
            expiry_notice(d("2026-05-31"), d("2026-05-30"), &days),
            Some(1)
        );
    }

    #[test]
    fn renewals_group_per_client_currency_and_day() {
        let a = ClientId::new();
        let b = ClientId::new();
        let line = |c: ClientId, cur: Currency, due: &str| RenewalLine {
            client_id: c,
            currency: cur,
            due_date: d(due),
            item: NewInvoiceItem::manual("x", dec!(1)),
            service_id: None,
        };
        let lines = vec![
            line(a, Currency::TRY, "2026-04-01"),
            line(a, Currency::TRY, "2026-04-01"),
            line(a, Currency::USD, "2026-04-01"),
            line(a, Currency::TRY, "2026-04-02"),
            line(b, Currency::TRY, "2026-04-01"),
            line(b, Currency::TRY, "2026-04-01"),
        ];
        let groups = group_renewals(lines.clone(), &HashSet::new());
        assert_eq!(groups.len(), 4);
        assert_eq!(groups.iter().map(|g| g.lines.len()).sum::<usize>(), 6);
        assert!(
            groups
                .iter()
                .any(|g| g.client_id == a && g.lines.len() == 2)
        );
        let sep: HashSet<_> = [b].into_iter().collect();
        assert_eq!(group_renewals(lines, &sep).len(), 5);
    }
}
