//! Mass payment: several unpaid invoices paid with one payment.
//!
//! [`Billing::create_mass_payment`] makes one combined invoice whose lines
//! are non-taxable `credit` lines, one per selected invoice, each for that
//! invoice's balance (computed here, never taken from the request). The
//! combined invoice is paid through the normal payment path, so redirect
//! gateways and the strict attempt matching apply unchanged. Once it is
//! paid, [`Billing::after_invoice_paid`] turns the lines into client credit
//! tied to the payment and then calls [`Billing::apply_mass_payment`], which
//! applies that credit to every selected invoice exactly once
//! (`mass_payment_items.applied_at`). Anything an invoice no longer needs
//! (paid elsewhere meanwhile) stays as client credit; refunding the payment
//! takes the credit back like any top-up.
//!
//! Combined invoices are left out of reminders, late fees, auto-charge and
//! the add-funds cap; an unpaid one is cancelled when a newer mass payment
//! takes over one of its invoices, when one of its invoices is paid some
//! other way ([`Billing::cancel_superseded_mass_payments`]; paying it is
//! refused then, [`Billing::check_mass_payment_payable`]), and by the
//! daily run after [`STALE_AFTER_DAYS`].
//!
//! A combined invoice that already has money on it when one of its
//! invoices is paid elsewhere stays: at payment time only what each
//! invoice still owes is applied, and the leftover stays as credit under
//! the add-funds limits ([`crate::funds::FundsLimits::excess`]): staff are
//! told to refund any part a top-up would not have been allowed.
//!
//! Refunds and chargebacks of the payment reach the invoices it paid
//! (`hos_db::repo::payments::settle_refund` / `settle_chargeback`, migration
//! 0052): their credit applications are undone newest first (refund: the
//! invoice is refunded; chargeback: reopened, services suspended by the
//! chargeback policy, affiliate commissions reversed by the daily run).

use chrono::Duration;
use hos_core::CoreError;
use hos_core::domain::{InvoiceStatus, Locale};
use hos_core::ids::{ClientId, InvoiceId, TenantId};
use hos_db::enums::{ClientStatus, InvoiceItemType};
use hos_db::repo::billing_support;
use hos_db::repo::invoices::{self, InvoiceRow};
use hos_db::repo::mass_pay::{self, MassPaymentItemRow, MassPaymentRow, MassPaymentStatus};
use hos_db::repo::payments;
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::PgConnection;

use crate::{Billing, BillingResult};

/// Most invoices one mass payment may cover.
pub const MAX_INVOICES: usize = 50;
/// An unpaid combined invoice is cancelled after this many days.
pub const STALE_AFTER_DAYS: i64 = 3;

/// A mass payment with its lines (client invoice page).
#[derive(Debug, Clone)]
pub struct MassPayment {
    pub row: MassPaymentRow,
    pub items: Vec<MassPaymentItemRow>,
}

impl Billing {
    /// Creates the combined invoice paying `invoice_ids` (all of `client`,
    /// open, unpaid, same currency, no add-funds or combined invoices).
    pub async fn create_mass_payment(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
        invoice_ids: &[InvoiceId],
    ) -> BillingResult<InvoiceRow> {
        let mut ids = invoice_ids.to_vec();
        ids.sort_by_key(|i| *i.as_uuid());
        ids.dedup();
        if ids.len() < 2 {
            return Err(CoreError::validation("select at least two invoices"));
        }
        if ids.len() > MAX_INVOICES {
            return Err(CoreError::validation(format!(
                "at most {MAX_INVOICES} invoices can be paid together"
            )));
        }
        let mut tx = self.pool().begin().await?;
        let client = self.load_client(&mut tx, tenant_id, client_id).await?;
        if client.status != ClientStatus::Active {
            return Err(CoreError::Forbidden(
                "the client account is not active".into(),
            ));
        }

        // Lock and check every invoice (ids sorted: stable lock order).
        let mut picked: Vec<InvoiceRow> = Vec::with_capacity(ids.len());
        for id in &ids {
            let inv = billing_support::lock_invoice_row(&mut tx, tenant_id, *id)
                .await?
                .filter(|i| i.client_id == client_id)
                .ok_or_else(|| CoreError::not_found("invoice", *id))?;
            if inv.status != InvoiceStatus::Unpaid || inv.balance <= Decimal::ZERO {
                return Err(CoreError::validation(format!(
                    "invoice {} is not awaiting payment",
                    inv.number.as_deref().unwrap_or("-")
                )));
            }
            if mass_pay::for_invoice(&mut *tx, tenant_id, inv.id)
                .await?
                .is_some()
                || invoices::invoice_items(&mut *tx, tenant_id, inv.id)
                    .await?
                    .iter()
                    .any(|i| i.item_type == InvoiceItemType::Credit)
            {
                return Err(CoreError::validation(format!(
                    "invoice {} cannot be paid together with others",
                    inv.number.as_deref().unwrap_or("-")
                )));
            }
            if picked.first().is_some_and(|f| f.currency != inv.currency) {
                return Err(CoreError::validation(
                    "invoices in different currencies cannot be paid together",
                ));
            }
            picked.push(inv);
        }

        // A newer selection replaces an unpaid earlier one.
        let now = self.clock().now();
        for open in mass_pay::open_containing(&mut *tx, tenant_id, client_id, &ids).await? {
            let Some(open) = mass_pay::lock(&mut tx, tenant_id, open.id).await? else {
                continue;
            };
            let combined = billing_support::lock_invoice_row(&mut tx, tenant_id, open.invoice_id)
                .await?
                .ok_or_else(|| CoreError::not_found("invoice", open.invoice_id))?;
            let untouched = combined.status == InvoiceStatus::Unpaid
                && combined.amount_paid == Decimal::ZERO
                && combined.credit_applied == Decimal::ZERO;
            if !untouched {
                return Err(CoreError::Conflict(
                    "a combined payment for these invoices is already in progress".into(),
                ));
            }
            invoices::cancel_invoice(&mut *tx, tenant_id, combined.id).await?;
            mass_pay::close(
                &mut *tx,
                tenant_id,
                open.id,
                MassPaymentStatus::Cancelled,
                now,
            )
            .await?;
        }

        let currency = picked[0].currency;
        let today = self.clock().today();
        let items = picked
            .iter()
            .map(|inv| {
                let number = inv.number.as_deref().unwrap_or("-");
                let label = match client.locale {
                    Locale::Tr => format!("{number} numaralı faturanın ödemesi"),
                    Locale::En => format!("Payment of invoice {number}"),
                };
                invoices::NewInvoiceItem {
                    item_type: InvoiceItemType::Credit,
                    taxable: false,
                    ..invoices::NewInvoiceItem::manual(&label, inv.balance)
                }
            })
            .collect();
        let invoice = invoices::create_invoice(
            &mut tx,
            tenant_id,
            &invoices::NewInvoice {
                client_id,
                currency,
                issue_date: today,
                due_date: today,
                issue: true,
                payment_integration_id: None,
                notes: None,
                billing_snapshot: crate::checkout::billing_snapshot(&client),
                items,
                taxes: Vec::new(),
            },
        )
        .await?;
        let lines: Vec<(InvoiceId, Decimal)> = picked.iter().map(|i| (i.id, i.balance)).collect();
        mass_pay::create(&mut tx, tenant_id, client_id, invoice.id, &lines).await?;
        tx.commit().await?;
        Ok(invoice)
    }

    /// The mass payment whose combined invoice is `invoice_id`, if any.
    pub async fn mass_payment_for_invoice(
        &self,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
    ) -> BillingResult<Option<MassPayment>> {
        let Some(row) = mass_pay::for_invoice(self.pool(), tenant_id, invoice_id).await? else {
            return Ok(None);
        };
        let items = mass_pay::items(self.pool(), tenant_id, row.id).await?;
        Ok(Some(MassPayment { row, items }))
    }

    /// Cancels the untouched combined invoices of open mass payments that
    /// include `invoice`, which was just paid some other way: paying them
    /// would only buy credit past the add-funds limits.
    pub(crate) async fn cancel_superseded_mass_payments(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
    ) -> BillingResult<()> {
        let open =
            mass_pay::open_containing(&mut *conn, tenant_id, invoice.client_id, &[invoice.id])
                .await?;
        for found in open {
            if found.invoice_id == invoice.id {
                continue;
            }
            self.cancel_if_untouched(conn, tenant_id, found.id).await?;
        }
        Ok(())
    }

    /// Cancels an open mass payment and its combined invoice when nothing
    /// was paid on it yet. Returns whether it did.
    async fn cancel_if_untouched(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        id: hos_db::repo::mass_pay::MassPaymentId,
    ) -> BillingResult<bool> {
        let Some(mp) = mass_pay::lock(conn, tenant_id, id).await? else {
            return Ok(false);
        };
        let Some(combined) =
            billing_support::lock_invoice_row(conn, tenant_id, mp.invoice_id).await?
        else {
            return Ok(false);
        };
        let untouched = mp.status == MassPaymentStatus::Open
            && matches!(
                combined.status,
                InvoiceStatus::Unpaid | InvoiceStatus::PaymentPending
            )
            && combined.amount_paid == Decimal::ZERO
            && combined.credit_applied == Decimal::ZERO;
        if !untouched {
            return Ok(false);
        }
        invoices::cancel_invoice(&mut *conn, tenant_id, combined.id).await?;
        mass_pay::close(
            &mut *conn,
            tenant_id,
            mp.id,
            MassPaymentStatus::Cancelled,
            self.clock().now(),
        )
        .await?;
        Ok(true)
    }

    /// Before a payment of `invoice` starts: a combined invoice whose
    /// invoices are no longer all owed in full is not paid. Untouched, it is
    /// cancelled (`Conflict`); with money on it already, the rest may be paid
    /// (only what is still owed is applied).
    pub(crate) async fn check_mass_payment_payable(
        &self,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
    ) -> BillingResult<()> {
        let Some(found) = mass_pay::for_invoice(self.pool(), tenant_id, invoice.id).await? else {
            return Ok(());
        };
        if found.status != MassPaymentStatus::Open {
            return Ok(());
        }
        let mut owed = true;
        for item in mass_pay::items(self.pool(), tenant_id, found.id).await? {
            let member = invoices::get_invoice(self.pool(), tenant_id, item.invoice_id).await?;
            if member.is_none_or(|m| !m.status.is_open() || m.balance < item.amount) {
                owed = false;
                break;
            }
        }
        if owed {
            return Ok(());
        }
        let mut tx = self.pool().begin().await?;
        let cancelled = self
            .cancel_if_untouched(&mut tx, tenant_id, found.id)
            .await?;
        tx.commit().await?;
        if cancelled {
            return Err(CoreError::Conflict(
                "some of the invoices in this combined payment were paid meanwhile; \
                 it was cancelled, select the open invoices again"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Applies the credit of a paid combined invoice to the invoices it was
    /// made for, each exactly once (inside the payment's transaction).
    /// Returns the invoices this fully paid; the caller runs
    /// [`Billing::after_invoice_paid`] for them.
    pub(crate) async fn apply_mass_payment(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
    ) -> BillingResult<Vec<InvoiceId>> {
        let mut paid = Vec::new();
        let Some(found) = mass_pay::for_invoice(&mut *conn, tenant_id, invoice.id).await? else {
            return Ok(paid);
        };
        let Some(mp) = mass_pay::lock(conn, tenant_id, found.id).await? else {
            return Ok(paid);
        };
        if mp.status != MassPaymentStatus::Open {
            return Ok(paid);
        }
        let now = self.clock().now();
        let balance_before =
            payments::credit_balance(&mut *conn, tenant_id, invoice.client_id, invoice.currency)
                .await?
                - invoice.total;
        let mut leftover = Decimal::ZERO;
        for item in mass_pay::items(&mut *conn, tenant_id, mp.id).await? {
            if item.applied_at.is_some() {
                continue;
            }
            let (updated, applied) = payments::apply_credit_to_invoice(
                conn,
                tenant_id,
                item.invoice_id,
                Some(item.amount),
                None,
            )
            .await?;
            if !mass_pay::mark_item_applied(&mut *conn, tenant_id, item.id, applied, now).await? {
                continue;
            }
            leftover += item.amount - applied;
            if applied > Decimal::ZERO && updated.status == InvoiceStatus::Paid {
                paid.push(item.invoice_id);
            }
        }
        mass_pay::close(
            &mut *conn,
            tenant_id,
            mp.id,
            MassPaymentStatus::Applied,
            now,
        )
        .await?;
        if leftover > Decimal::ZERO {
            self.check_leftover_credit(conn, tenant_id, invoice, leftover, balance_before)
                .await?;
        }
        Ok(paid)
    }

    /// Credit a combined invoice's payment left behind (its invoices were
    /// paid some other way meanwhile) stays with the client, but only as far
    /// as the add-funds limits allow a top-up: staff are told to refund the
    /// rest.
    pub(crate) async fn check_leftover_credit(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
        leftover: Decimal,
        balance_before: Decimal,
    ) -> BillingResult<()> {
        let limits = crate::funds::FundsLimits::load(&mut *conn, tenant_id).await?;
        let excess = limits.excess(leftover, balance_before);
        if excess <= Decimal::ZERO {
            return Ok(());
        }
        tracing::warn!(%tenant_id, invoice_id = %invoice.id, %excess, "mass payment left credit over the add-funds limits");
        self.notify_staff(
            conn,
            tenant_id,
            "payment.mass_pay_excess_credit",
            &format!(
                "A combined payment left {} {} of credit over the add-funds limits",
                excess.normalize(),
                invoice.currency
            ),
            Some(
                "Invoices of the combined payment were paid some other way before its payment \
                 arrived. What they no longer needed became client credit; this part is more \
                 than a credit top-up may be. Refund it to the client.",
            ),
            Some(&format!("/admin/clients/{}", invoice.client_id)),
            &json!({"invoice_id": invoice.id, "client_id": invoice.client_id,
                    "leftover": leftover, "excess": excess, "currency": invoice.currency}),
        )
        .await?;
        Ok(())
    }

    /// Cancels combined invoices left unpaid for [`STALE_AFTER_DAYS`]
    /// (daily run). Returns how many were cancelled.
    pub async fn cancel_stale_mass_payments(&self, tenant_id: TenantId) -> BillingResult<usize> {
        let before = self.clock().now() - Duration::days(STALE_AFTER_DAYS);
        let mut done = 0;
        for stale in mass_pay::stale(self.pool(), tenant_id, before).await? {
            let mut tx = self.pool().begin().await?;
            let Some(mp) = mass_pay::lock(&mut tx, tenant_id, stale.id).await? else {
                continue;
            };
            let Some(inv) =
                billing_support::lock_invoice_row(&mut tx, tenant_id, mp.invoice_id).await?
            else {
                continue;
            };
            if mp.status == MassPaymentStatus::Open
                && inv.status == InvoiceStatus::Unpaid
                && inv.amount_paid == Decimal::ZERO
                && inv.credit_applied == Decimal::ZERO
            {
                invoices::cancel_invoice(&mut *tx, tenant_id, inv.id).await?;
                mass_pay::close(
                    &mut *tx,
                    tenant_id,
                    mp.id,
                    MassPaymentStatus::Cancelled,
                    self.clock().now(),
                )
                .await?;
                done += 1;
            }
            tx.commit().await?;
        }
        Ok(done)
    }
}
