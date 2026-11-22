//! Payments: hosted checkout start, gateway webhooks, manual payments, credit,
//! refunds, chargebacks and stored-method charges — and what happens once an
//! invoice is paid ([`Billing::after_invoice_paid`]).
//!
//! ## Matching gateway events to invoices
//!
//! Every hosted payment gets a `payment_attempts` row whose `reference`
//! (32 alphanumerics) is passed as [`CheckoutRequest::reference`] and echoed
//! by the gateway as [`PaymentEvent::invoice_ref`]. Events without a reference
//! are matched by the attempt's gateway session reference, then by an earlier
//! transaction with the same `gateway_txn_id` (refunds, chargebacks).
//!
//! ## Safety rules
//!
//! * The webhook is verified by the gateway implementation (signature or API
//!   re-fetch); unverifiable requests → [`CoreError::Unauthorized`].
//! * Idempotent twice: `webhooks_in (integration, event id)` and
//!   `transactions (integration, gateway_txn_id)`.
//! * A currency mismatch never touches the invoice: the money goes to the
//!   client's credit (in the paid currency) and staff are notified.
//! * Partial payments leave the invoice open; overpayments become credit.

use std::net::IpAddr;

use chrono::{DateTime, Days, Utc};
use hos_core::CoreError;
use hos_core::domain::{DomainStatus, InvoiceStatus, Locale, OrderStatus, ServiceStatus};
use hos_core::error::IntegrationError;
use hos_core::ids::{ClientId, IntegrationId, InvoiceId, TenantId, TransactionId, UserId};
use hos_core::integrations::PostalAddress;
use hos_core::integrations::gateway::{
    CardSetupRequest, CheckoutItem, CheckoutRequest, CheckoutSession, PaymentCustomer,
    PaymentEvent, PaymentEventKind, PaymentGateway, RefundRequest, RefundStatus,
    StoredChargeRequest, WebhookReply, WebhookRequest,
};
use hos_core::money::Money;
use hos_core::secret::SecretString;
use hos_db::enums::{InvoiceItemType, TransactionKind, TransactionStatus, WebhookInStatus};
use hos_db::repo::billing_support::{self, NewPaymentAttempt, NewPaymentMethod, PaymentAttemptRow};
use hos_db::repo::clients::ClientRow;
use hos_db::repo::invoices::{self, InvoiceRow};
use hos_db::repo::payments::{self, NewPayment, RefundRow, TransactionRow};
use hos_db::repo::{clients, domains, orders, services, webhooks};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::checkout::ProvisionTrigger;
use crate::jobs::{self, JobKind};
use crate::secrets;
use crate::settings::BillingSettings;
use crate::{Billing, BillingResult};

/// `services.suspended_reason` set by overdue automation; only services
/// suspended for this reason are reactivated automatically on payment.
pub const SUSPEND_REASON_OVERDUE: &str = "overdue";
/// `services.suspended_reason` set after a chargeback.
pub const SUSPEND_REASON_CHARGEBACK: &str = "chargeback";

/// Input of [`Billing::start_payment`].
#[derive(Debug, Clone, Default)]
pub struct StartPayment {
    /// Gateway to use; `None` → the invoice's gateway, the client's default,
    /// then the tenant's default gateway.
    pub integration_id: Option<IntegrationId>,
    /// When set (client area), the invoice must belong to this client.
    pub client_id: Option<ClientId>,
    /// Defaults to `{base}/client/invoices/{id}?payment=return`.
    pub return_url: Option<String>,
    /// Defaults to `{base}/client/invoices/{id}?payment=cancel`.
    pub cancel_url: Option<String>,
    /// Buyer IP (required by some gateways).
    pub ip: Option<IpAddr>,
    /// Ask the gateway to store the card for automatic renewals.
    pub save_payment_method: bool,
    /// Installment count chosen on the pay page; must be one of the
    /// gateway's [`PaymentGateway::installment_choices`] for the amount.
    /// The charged amount stays the invoice balance (exact attempt match),
    /// so installment interest never becomes credit.
    pub installments: Option<u32>,
}

/// `CheckoutRequest::metadata` key of the chosen installment count (read by
/// gateways that take it from the merchant, e.g. NestPay).
pub const INSTALLMENTS_METADATA: &str = hos_payments::nestpay::INSTALLMENTS_METADATA;

/// Result of [`Billing::start_card_setup`].
#[derive(Debug, Clone, Serialize)]
pub struct StartedCardSetup {
    pub reference: String,
    pub integration_id: IntegrationId,
    pub session: CheckoutSession,
}

/// Result of [`Billing::start_payment`].
#[derive(Debug, Clone, Serialize)]
pub struct StartedPayment {
    /// Merchant reference of the attempt.
    pub reference: String,
    pub integration_id: IntegrationId,
    pub amount: Money,
    /// How to send the customer to the gateway.
    pub session: CheckoutSession,
}

/// A gateway callback whose signature [`Billing::verify_webhook`] accepted,
/// ready for [`Billing::apply_webhook`].
#[derive(Debug, Clone)]
pub struct VerifiedWebhook {
    gateway_kind: String,
    headers: Value,
    outcome: hos_core::integrations::gateway::WebhookOutcome,
}

impl VerifiedWebhook {
    /// Number of payment events in the request.
    pub fn event_count(&self) -> usize {
        self.outcome.events.len()
    }
}

/// What a webhook call did.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookResult {
    /// What to answer to the gateway.
    pub reply: WebhookReply,
    /// One entry per event in the request.
    pub events: Vec<WebhookEventResult>,
}

/// Outcome of one gateway event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebhookEventResult {
    pub kind: PaymentEventKind,
    pub gateway_txn_id: String,
    pub outcome: EventOutcome,
}

/// How an event was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventOutcome {
    /// Already processed earlier (webhook replay).
    Duplicate,
    /// Payment recorded; `invoice_paid` when it settled the invoice.
    PaymentRecorded {
        invoice_id: Option<InvoiceId>,
        invoice_paid: bool,
        credited: Decimal,
    },
    /// Paid currency differs from the invoice: amount credited, invoice untouched.
    CurrencyMismatch { credited: Decimal },
    /// Invoice moved to `payment_pending`.
    Pending,
    /// Attempt failed; invoice stays open.
    Failed,
    /// Refund recorded.
    Refunded { refund_id: hos_core::ids::RefundId },
    /// Chargeback recorded; invoice reopened.
    Chargeback { invoice_id: Option<InvoiceId> },
    /// Could not be matched / applied (stored as failed for staff review).
    Rejected { reason: String },
    /// A card saved without a payment was stored for the client.
    MethodSaved { client_id: ClientId },
}

/// Input of [`Billing::record_manual_payment`] (bank transfer confirmation, cash, ...).
#[derive(Debug, Clone)]
pub struct ManualPayment {
    pub invoice_id: InvoiceId,
    pub amount: Decimal,
    /// Offline gateway integration (bank transfer), if any.
    pub integration_id: Option<IntegrationId>,
    /// Shown in transaction lists, e.g. `bank_transfer`, `cash`; default `manual`.
    pub method: Option<String>,
    /// Bank reference; makes the entry idempotent when set.
    pub reference: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub staff_user_id: Option<UserId>,
}

/// Input of [`Billing::refund_payment`].
#[derive(Debug, Clone)]
pub struct RefundInput {
    pub transaction_id: TransactionId,
    pub amount: Decimal,
    pub reason: Option<String>,
    /// Refund to client credit instead of the card / account.
    pub to_credit: bool,
    pub staff_user_id: Option<UserId>,
    pub ip: Option<IpAddr>,
}

/// Result of an automatic stored-method charge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ChargeOutcome {
    /// Charged; the invoice is paid (or partially paid).
    Charged { invoice_paid: bool },
    /// Accepted by the gateway, completes asynchronously.
    Pending,
    /// Declined / failed.
    Declined { message: String },
    /// Nothing to do (paid already, no stored method, gateway cannot charge).
    Skipped { reason: String },
}

/// A new 32-character alphanumeric attempt reference.
pub fn new_reference() -> String {
    Uuid::now_v7().simple().to_string()
}

fn customer(client: &ClientRow, ip: Option<IpAddr>) -> PaymentCustomer {
    let address = match (&client.address_line1, &client.city) {
        (Some(l1), Some(city)) if !l1.trim().is_empty() && !city.trim().is_empty() => {
            Some(PostalAddress {
                line1: l1.clone(),
                line2: client.address_line2.clone(),
                city: city.clone(),
                state: client.state.clone(),
                postal_code: client.postal_code.clone(),
                country_code: client.country_code.to_ascii_uppercase(),
            })
        }
        _ => None,
    };
    PaymentCustomer {
        client_id: Some(client.id),
        name: client.display_name(),
        email: client.email.clone(),
        phone: client.phone.clone(),
        ip,
        billing_address: address,
        identity_number: client
            .tax_number
            .clone()
            .or_else(|| client.national_id.clone()),
        gateway_customer_id: None,
    }
}

/// Why an event flagged [`hos_payments::RAW_EXACT_ATTEMPT`] cannot be
/// applied: it must belong to a payment attempt of this integration with the
/// same amount and currency (the gateway signed the amount it charged).
fn exact_attempt_mismatch(
    attempt: Option<&PaymentAttemptRow>,
    integration_id: IntegrationId,
    event: &PaymentEvent,
) -> Option<&'static str> {
    let Some(a) = attempt else {
        return Some("no payment attempt matches the reference");
    };
    if a.integration_id != Some(integration_id) {
        return Some("the payment attempt belongs to another gateway");
    }
    let amount = event.amount.round();
    if a.currency != amount.currency {
        return Some("currency differs from the payment attempt");
    }
    if a.amount != amount.amount {
        return Some("amount differs from the payment attempt");
    }
    None
}

fn invoice_label(invoice: &InvoiceRow, locale: Locale) -> String {
    let n = invoice
        .number
        .clone()
        .unwrap_or_else(|| invoice.id.to_string());
    match locale {
        Locale::Tr => format!("Fatura #{n}"),
        Locale::En => format!("Invoice #{n}"),
    }
}

fn is_rejectable(e: &CoreError) -> bool {
    matches!(
        e,
        CoreError::Validation(_)
            | CoreError::NotFound { .. }
            | CoreError::InvalidTransition { .. }
            | CoreError::Money(_)
    )
}

impl Billing {
    pub(crate) async fn load_invoice(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        id: InvoiceId,
    ) -> BillingResult<InvoiceRow> {
        invoices::get_invoice(&mut *conn, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("invoice", id))
    }

    pub(crate) async fn load_client(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        id: ClientId,
    ) -> BillingResult<ClientRow> {
        clients::get_client(&mut *conn, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", id))
    }

    /// Starts a hosted payment for an invoice's open balance.
    pub async fn start_payment(
        &self,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
        req: &StartPayment,
    ) -> BillingResult<StartedPayment> {
        let mut conn = self.pool().acquire().await?;
        let invoice = self.load_invoice(&mut conn, tenant_id, invoice_id).await?;
        if req.client_id.is_some_and(|c| c != invoice.client_id) {
            return Err(CoreError::not_found("invoice", invoice_id));
        }
        if !invoice.status.is_open() || invoice.balance <= Decimal::ZERO {
            return Err(CoreError::Conflict(format!(
                "invoice is {} and has nothing to pay",
                invoice.status
            )));
        }
        self.check_mass_payment_payable(tenant_id, &invoice).await?;
        let client = self
            .load_client(&mut conn, tenant_id, invoice.client_id)
            .await?;
        let integration_id = match req
            .integration_id
            .or(invoice.payment_integration_id)
            .or(client.default_gateway_id)
        {
            Some(id) => id,
            None => self
                .registry()
                .default_id(tenant_id, hos_core::domain::IntegrationCategory::Gateway)
                .await?
                .ok_or_else(|| CoreError::validation("no payment gateway is configured"))?,
        };
        let loaded = self.registry().get(tenant_id, integration_id).await?;
        let gateway = self.registry().gateway(tenant_id, integration_id).await?;
        let caps = gateway.capabilities();
        if !caps.supports_currency(invoice.currency) {
            return Err(CoreError::validation(format!(
                "{} does not accept {}",
                loaded.name, invoice.currency
            )));
        }
        let amount = invoice.balance_money().round();
        if let Some(n) = req.installments.filter(|n| *n > 1)
            && !gateway.installment_choices(&amount).contains(&n)
        {
            return Err(CoreError::validation(format!(
                "{n} installments are not offered for this payment"
            )));
        }
        let reference = new_reference();
        let attempt = billing_support::create_payment_attempt(
            &mut *conn,
            tenant_id,
            &NewPaymentAttempt {
                invoice_id,
                integration_id: Some(integration_id),
                reference: reference.clone(),
                amount: amount.amount,
                currency: amount.currency,
                save_payment_method: req.save_payment_method && caps.stored_payment_methods,
            },
        )
        .await?;
        let base = self.tenant_url(tenant_id).await?;
        let label = invoice_label(&invoice, client.locale);
        let request =
            CheckoutRequest {
                invoice_id,
                reference: reference.clone(),
                amount,
                description: label.clone(),
                customer: customer(&client, req.ip),
                items: vec![CheckoutItem {
                    reference: invoice_id.to_string(),
                    name: label,
                    category: Some("hosting".into()),
                    quantity: 1,
                    unit_price: amount,
                    total: amount,
                }],
                return_url: req.return_url.clone().unwrap_or_else(|| {
                    format!("{base}/client/invoices/{invoice_id}?payment=return")
                }),
                cancel_url: req.cancel_url.clone().unwrap_or_else(|| {
                    format!("{base}/client/invoices/{invoice_id}?payment=cancel")
                }),
                webhook_url: format!("{base}/webhooks/gateway/{integration_id}"),
                locale: Some(client.locale),
                save_payment_method: attempt.save_payment_method,
                metadata: [
                    ("tenant_id".to_owned(), tenant_id.to_string()),
                    ("invoice_id".to_owned(), invoice_id.to_string()),
                ]
                .into_iter()
                .chain(
                    req.installments
                        .filter(|n| *n > 1)
                        .map(|n| (INSTALLMENTS_METADATA.to_owned(), n.to_string())),
                )
                .collect(),
            };
        match gateway.create_checkout(&request).await {
            Ok(session) => {
                billing_support::set_attempt_gateway_ref(
                    &mut *conn,
                    tenant_id,
                    attempt.id,
                    session.gateway_ref(),
                )
                .await?;
                if invoice.payment_integration_id != Some(integration_id) {
                    invoices::update_invoice_meta(
                        &mut *conn,
                        tenant_id,
                        invoice_id,
                        invoice.notes.as_deref(),
                        invoice.admin_notes.as_deref(),
                        Some(integration_id),
                    )
                    .await?;
                }
                Ok(StartedPayment {
                    reference,
                    integration_id,
                    amount,
                    session,
                })
            }
            Err(e) => {
                billing_support::set_attempt_status(&mut *conn, tenant_id, attempt.id, "failed")
                    .await?;
                Err(e.into())
            }
        }
    }

    /// Starts saving a card without a payment (client area "add a card"):
    /// a `payment_setups` row plus the gateway hand-off. The card is stored
    /// only when the gateway's signed completion event arrives
    /// ([`PaymentEventKind::MethodSaved`]).
    pub async fn start_card_setup(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
        integration_id: IntegrationId,
        user_id: Option<UserId>,
        ip: Option<IpAddr>,
    ) -> BillingResult<StartedCardSetup> {
        let mut conn = self.pool().acquire().await?;
        let client = self.load_client(&mut conn, tenant_id, client_id).await?;
        if client.status != hos_db::enums::ClientStatus::Active {
            return Err(CoreError::Forbidden(
                "the client account is not active".into(),
            ));
        }
        let loaded = self.registry().get(tenant_id, integration_id).await?;
        let gateway = self.registry().gateway(tenant_id, integration_id).await?;
        if !gateway.supports_card_setup() || !gateway.capabilities().stored_payment_methods {
            return Err(CoreError::validation(format!(
                "{} cannot save a card without a payment",
                loaded.name
            )));
        }
        if !gateway.capabilities().supports_currency(client.currency) {
            return Err(CoreError::validation(format!(
                "{} does not accept {}",
                loaded.name, client.currency
            )));
        }
        let reference: String = hos_core::crypto::random_bytes::<16>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let setup = billing_support::create_payment_setup(
            &mut *conn,
            tenant_id,
            client_id,
            integration_id,
            &reference,
            user_id,
        )
        .await?;
        let base = self.tenant_url(tenant_id).await?;
        let request = CardSetupRequest {
            reference: reference.clone(),
            currency: client.currency,
            customer: customer(&client, ip),
            return_url: format!("{base}/client/billing?card=saved"),
            cancel_url: format!("{base}/client/billing?card=cancelled"),
            webhook_url: format!("{base}/webhooks/gateway/{integration_id}"),
            locale: Some(client.locale),
        };
        match gateway.create_card_setup(&request).await {
            Ok(session) => {
                billing_support::set_payment_setup_gateway_ref(
                    &mut *conn,
                    tenant_id,
                    setup.id,
                    session.gateway_ref(),
                )
                .await?;
                Ok(StartedCardSetup {
                    reference,
                    integration_id,
                    session,
                })
            }
            Err(e) => {
                billing_support::finish_payment_setup(&mut *conn, tenant_id, setup.id, "failed")
                    .await?;
                Err(e.into())
            }
        }
    }

    /// Stores the payment method of a completed card setup (once).
    async fn apply_method_saved(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        event: &PaymentEvent,
    ) -> BillingResult<EventOutcome> {
        let Some(setup) = (match event.invoice_ref.as_deref().filter(|r| !r.is_empty()) {
            Some(r) => billing_support::lock_payment_setup(conn, tenant_id, r).await?,
            None => None,
        }) else {
            return Ok(EventOutcome::Rejected {
                reason: "card setup event for an unknown setup".into(),
            });
        };
        if setup.integration_id != integration_id {
            return Err(CoreError::validation(
                "card setup reference belongs to another gateway",
            ));
        }
        if setup.status != "pending" {
            return Ok(EventOutcome::Duplicate);
        }
        let Some(token) = event.payment_method_ref.as_ref().filter(|t| !t.is_empty()) else {
            billing_support::finish_payment_setup(&mut *conn, tenant_id, setup.id, "failed")
                .await?;
            return Ok(EventOutcome::Rejected {
                reason: "card setup event without a payment method".into(),
            });
        };
        if !self
            .store_payment_method(
                conn,
                tenant_id,
                setup.client_id,
                integration_id,
                token,
                hos_payments::event_payment_method_label(event),
            )
            .await?
        {
            billing_support::finish_payment_setup(&mut *conn, tenant_id, setup.id, "failed")
                .await?;
            return Ok(EventOutcome::Rejected {
                reason: "the card is stored for another client".into(),
            });
        }
        billing_support::finish_payment_setup(&mut *conn, tenant_id, setup.id, "succeeded").await?;
        Ok(EventOutcome::MethodSaved {
            client_id: setup.client_id,
        })
    }

    /// Verifies and applies a gateway webhook (`/webhooks/gateway/{integration_id}`).
    ///
    /// Each event is applied in its own transaction and recorded in
    /// `webhooks_in`; replays are answered without side effects. Events that
    /// cannot be matched are stored as `failed` for staff review and still
    /// acknowledged; transient (database) errors are returned so the gateway
    /// retries.
    pub async fn handle_webhook(
        &self,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        request: &WebhookRequest,
    ) -> BillingResult<WebhookResult> {
        let verified = self
            .verify_webhook(tenant_id, integration_id, request)
            .await?;
        self.apply_webhook(tenant_id, integration_id, verified)
            .await
    }

    /// Whether verifying `request` makes the gateway call its provider
    /// ([`PaymentGateway::verifies_remotely`]); the webhook endpoint rate
    /// limits those before [`Billing::verify_webhook`].
    pub async fn webhook_verifies_remotely(
        &self,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        request: &WebhookRequest,
    ) -> BillingResult<bool> {
        let gateway = self.registry().gateway(tenant_id, integration_id).await?;
        Ok(gateway.verifies_remotely(request))
    }

    /// First half of [`Billing::handle_webhook`]: the gateway checks the
    /// signature and parses the events. Nothing is written. A forged or
    /// unverifiable request is `Unauthorized`.
    pub async fn verify_webhook(
        &self,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        request: &WebhookRequest,
    ) -> BillingResult<VerifiedWebhook> {
        let loaded = self.registry().get(tenant_id, integration_id).await?;
        let gateway = self.registry().gateway(tenant_id, integration_id).await?;
        let outcome = match gateway.parse_webhook(request).await {
            Ok(o) => o,
            Err(IntegrationError::Auth(msg)) => {
                tracing::warn!(%tenant_id, %integration_id, error = %msg, "webhook signature rejected");
                return Err(CoreError::Unauthorized);
            }
            Err(e) => return Err(e.into()),
        };
        let headers: Value = request
            .headers
            .iter()
            .filter(|(k, _)| {
                let k = k.as_str();
                k.starts_with("content-") || k.contains("signature") || k.starts_with("x-")
            })
            .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or_default())))
            .collect::<serde_json::Map<_, _>>()
            .into();
        Ok(VerifiedWebhook {
            gateway_kind: loaded.kind,
            headers,
            outcome,
        })
    }

    /// Second half of [`Billing::handle_webhook`]: applies the events of a
    /// verified request.
    pub async fn apply_webhook(
        &self,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        verified: VerifiedWebhook,
    ) -> BillingResult<WebhookResult> {
        let VerifiedWebhook {
            gateway_kind,
            headers,
            outcome,
        } = verified;
        let mut results = Vec::with_capacity(outcome.events.len());
        for event in &outcome.events {
            let key = event.event_id.clone().unwrap_or_else(|| {
                format!(
                    "{}:{}:{}",
                    event.kind,
                    event.gateway_txn_id,
                    event.amount.amount.normalize()
                )
            });
            let (row, is_new) = webhooks::record_incoming(
                self.pool(),
                tenant_id,
                integration_id,
                &key,
                Some(event.kind.as_str()),
                &hos_payments::raw_for_storage(&event.raw),
                &headers,
                true,
            )
            .await?;
            if !is_new
                && matches!(
                    row.status,
                    WebhookInStatus::Processed | WebhookInStatus::Ignored
                )
            {
                results.push(WebhookEventResult {
                    kind: event.kind,
                    gateway_txn_id: event.gateway_txn_id.clone(),
                    outcome: EventOutcome::Duplicate,
                });
                continue;
            }
            let mut tx = self.pool().begin().await?;
            // The same event delivered concurrently: the second delivery waits
            // here for the first to commit, then sees it processed.
            let current = webhooks::lock_incoming(&mut tx, tenant_id, row.id).await?;
            if current.is_some_and(|r| {
                matches!(
                    r.status,
                    WebhookInStatus::Processed | WebhookInStatus::Ignored
                )
            }) {
                tx.rollback().await?;
                results.push(WebhookEventResult {
                    kind: event.kind,
                    gateway_txn_id: event.gateway_txn_id.clone(),
                    outcome: EventOutcome::Duplicate,
                });
                continue;
            }
            match self
                .apply_payment_event(&mut tx, tenant_id, integration_id, &gateway_kind, event)
                .await
            {
                Ok(outcome) => {
                    let (status, error) = match &outcome {
                        EventOutcome::Rejected { reason } => {
                            (WebhookInStatus::Failed, Some(reason.as_str()))
                        }
                        EventOutcome::Duplicate => (WebhookInStatus::Ignored, None),
                        _ => (WebhookInStatus::Processed, None),
                    };
                    webhooks::set_webhook_status(&mut *tx, tenant_id, row.id, status, error)
                        .await?;
                    tx.commit().await?;
                    results.push(WebhookEventResult {
                        kind: event.kind,
                        gateway_txn_id: event.gateway_txn_id.clone(),
                        outcome,
                    });
                }
                Err(e) => {
                    tx.rollback().await?;
                    let msg = e.to_string();
                    webhooks::set_webhook_status(
                        self.pool(),
                        tenant_id,
                        row.id,
                        WebhookInStatus::Failed,
                        Some(&msg),
                    )
                    .await?;
                    if !is_rejectable(&e) {
                        return Err(e);
                    }
                    tracing::warn!(%tenant_id, %integration_id, error = %msg, "payment event rejected");
                    results.push(WebhookEventResult {
                        kind: event.kind,
                        gateway_txn_id: event.gateway_txn_id.clone(),
                        outcome: EventOutcome::Rejected { reason: msg },
                    });
                }
            }
        }
        Ok(WebhookResult {
            reply: outcome.reply,
            events: results,
        })
    }

    /// Finds the attempt (and so the invoice) an event belongs to.
    async fn match_attempt(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        event: &PaymentEvent,
    ) -> BillingResult<Option<PaymentAttemptRow>> {
        if let Some(r) = event.invoice_ref.as_deref().filter(|r| !r.is_empty())
            && let Some(a) = billing_support::attempt_by_reference(&mut *conn, tenant_id, r).await?
        {
            if a.integration_id.is_some_and(|i| i != integration_id) {
                return Err(CoreError::validation(
                    "payment reference belongs to another gateway",
                ));
            }
            return Ok(Some(a));
        }
        billing_support::attempt_by_gateway_ref(
            &mut *conn,
            tenant_id,
            integration_id,
            &event.gateway_txn_id,
        )
        .await
    }

    /// Applies one verified gateway event inside the caller's transaction.
    pub async fn apply_payment_event(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        gateway_kind: &str,
        event: &PaymentEvent,
    ) -> BillingResult<EventOutcome> {
        match event.kind {
            PaymentEventKind::Paid => {
                self.apply_paid(conn, tenant_id, integration_id, gateway_kind, event)
                    .await
            }
            PaymentEventKind::Pending => {
                let Some(attempt) = self
                    .match_attempt(conn, tenant_id, integration_id, event)
                    .await?
                else {
                    return Ok(EventOutcome::Rejected {
                        reason: "pending event for an unknown payment".into(),
                    });
                };
                billing_support::set_attempt_status(&mut *conn, tenant_id, attempt.id, "pending")
                    .await?;
                invoices::transition_invoice(
                    &mut *conn,
                    tenant_id,
                    attempt.invoice_id,
                    &[InvoiceStatus::Unpaid],
                    InvoiceStatus::PaymentPending,
                )
                .await?;
                Ok(EventOutcome::Pending)
            }
            PaymentEventKind::Failed => {
                let Some(attempt) = self
                    .match_attempt(conn, tenant_id, integration_id, event)
                    .await?
                else {
                    return Ok(EventOutcome::Rejected {
                        reason: "failure event for an unknown payment".into(),
                    });
                };
                billing_support::set_attempt_status(&mut *conn, tenant_id, attempt.id, "failed")
                    .await?;
                invoices::transition_invoice(
                    &mut *conn,
                    tenant_id,
                    attempt.invoice_id,
                    &[InvoiceStatus::PaymentPending],
                    InvoiceStatus::Unpaid,
                )
                .await?;
                let invoice = self
                    .load_invoice(conn, tenant_id, attempt.invoice_id)
                    .await?;
                // Audit trail only: no gateway id, so a later success for the
                // same transaction is never mistaken for a replay.
                payments::record_transaction(
                    &mut *conn,
                    tenant_id,
                    invoice.client_id,
                    Some(invoice.id),
                    Some(integration_id),
                    gateway_kind,
                    None,
                    TransactionKind::Payment,
                    TransactionStatus::Failed,
                    event.amount.amount.abs(),
                    event.amount.currency,
                    &json!({"message": event.message, "gateway_txn_id": event.gateway_txn_id}),
                )
                .await?;
                Ok(EventOutcome::Failed)
            }
            PaymentEventKind::Refunded => {
                self.apply_refund_event(conn, tenant_id, integration_id, event)
                    .await
            }
            PaymentEventKind::Chargeback => {
                self.apply_chargeback(conn, tenant_id, integration_id, gateway_kind, event)
                    .await
            }
            PaymentEventKind::MethodSaved => {
                self.apply_method_saved(conn, tenant_id, integration_id, event)
                    .await
            }
        }
    }

    async fn apply_paid(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        gateway_kind: &str,
        event: &PaymentEvent,
    ) -> BillingResult<EventOutcome> {
        if event.amount.amount <= Decimal::ZERO {
            return Ok(EventOutcome::Rejected {
                reason: "payment amount must be positive".into(),
            });
        }
        let attempt = self
            .match_attempt(conn, tenant_id, integration_id, event)
            .await?;
        if hos_payments::event_requires_exact_attempt(event)
            && let Some(reason) = exact_attempt_mismatch(attempt.as_ref(), integration_id, event)
        {
            self.notify_staff(
                conn,
                tenant_id,
                "payment.attempt_mismatch",
                "A gateway payment did not match its payment attempt",
                Some(&format!(
                    "{reason}: {} {} ({gateway_kind} {})",
                    event.amount.amount, event.amount.currency, event.gateway_txn_id
                )),
                attempt
                    .as_ref()
                    .map(|a| format!("/admin/invoices/{}", a.invoice_id))
                    .as_deref(),
                &json!({"integration_id": integration_id, "gateway_txn_id": event.gateway_txn_id,
                        "reference": event.invoice_ref}),
            )
            .await?;
            return Ok(EventOutcome::Rejected {
                reason: format!("{reason} (payment {})", event.gateway_txn_id),
            });
        }
        let invoice_id = match &attempt {
            Some(a) => Some(a.invoice_id),
            None => payments::transaction_by_gateway_id(
                &mut *conn,
                tenant_id,
                Some(integration_id),
                &event.gateway_txn_id,
            )
            .await?
            .and_then(|t| t.invoice_id),
        };
        let Some(invoice_id) = invoice_id else {
            self.notify_staff(
                conn,
                tenant_id,
                "payment.unmatched",
                "A payment could not be matched to an invoice",
                Some(&format!(
                    "{} {} ({gateway_kind} {})",
                    event.amount.amount, event.amount.currency, event.gateway_txn_id
                )),
                None,
                &json!({"integration_id": integration_id, "gateway_txn_id": event.gateway_txn_id}),
            )
            .await?;
            return Ok(EventOutcome::Rejected {
                reason: format!("payment {} matches no invoice", event.gateway_txn_id),
            });
        };
        let invoice = billing_support::lock_invoice_row(&mut *conn, tenant_id, invoice_id)
            .await?
            .ok_or_else(|| CoreError::not_found("invoice", invoice_id))?;
        let amount = event.amount.round();
        let fee = event
            .fee
            .filter(|f| f.currency == amount.currency)
            .map_or(Decimal::ZERO, |f| f.amount);
        let currency_ok = amount.currency == invoice.currency;
        let payment = NewPayment {
            client_id: invoice.client_id,
            invoice_id: currency_ok.then_some(invoice.id),
            integration_id: Some(integration_id),
            gateway: gateway_kind.to_owned(),
            gateway_txn_id: Some(event.gateway_txn_id.clone()),
            amount: amount.amount,
            fee,
            currency: amount.currency,
            description: Some(match currency_ok {
                true => format!(
                    "Payment for invoice {}",
                    invoice.number.as_deref().unwrap_or("-")
                ),
                false => format!(
                    "Payment for invoice {} in {} (credited: currency mismatch)",
                    invoice.number.as_deref().unwrap_or("-"),
                    amount.currency
                ),
            }),
            raw: hos_payments::raw_for_storage(&event.raw),
            occurred_at: event.occurred_at,
            created_by: None,
        };
        let outcome = payments::record_payment(&mut *conn, tenant_id, &payment).await?;
        if outcome.duplicate {
            return Ok(EventOutcome::Duplicate);
        }
        // Money for a combined invoice cancelled meanwhile (its invoices
        // were paid some other way) only buys credit: add-funds limits.
        if outcome.credited > Decimal::ZERO
            && currency_ok
            && hos_db::repo::mass_pay::for_invoice(&mut *conn, tenant_id, invoice.id)
                .await?
                .is_some_and(|m| m.status == hos_db::repo::mass_pay::MassPaymentStatus::Cancelled)
        {
            let balance = payments::credit_balance(
                &mut *conn,
                tenant_id,
                invoice.client_id,
                invoice.currency,
            )
            .await?;
            self.check_leftover_credit(
                conn,
                tenant_id,
                &invoice,
                outcome.credited,
                balance - outcome.credited,
            )
            .await?;
        }
        if let Some(a) = &attempt {
            billing_support::set_attempt_status(&mut *conn, tenant_id, a.id, "succeeded").await?;
            if a.save_payment_method
                && let Some(token) = &event.payment_method_ref
            {
                // A token of another client is left alone (logged); the
                // payment itself is recorded either way.
                self.store_payment_method(
                    conn,
                    tenant_id,
                    invoice.client_id,
                    integration_id,
                    token,
                    hos_payments::event_payment_method_label(event),
                )
                .await?;
            }
        }
        if !currency_ok {
            self.notify_staff(
                conn,
                tenant_id,
                "payment.currency_mismatch",
                &format!(
                    "Payment in {} for invoice {} ({}) was credited",
                    amount.currency,
                    invoice.number.as_deref().unwrap_or("-"),
                    invoice.currency
                ),
                None,
                Some(&format!("/admin/invoices/{}", invoice.id)),
                &json!({"invoice_id": invoice.id, "amount": amount.amount, "currency": amount.currency}),
            )
            .await?;
            return Ok(EventOutcome::CurrencyMismatch {
                credited: outcome.credited,
            });
        }
        let paid_invoice = outcome.invoice.as_ref();
        if !outcome.invoice_paid
            && paid_invoice.is_some_and(|i| i.status.is_open() && i.balance > Decimal::ZERO)
        {
            self.notify_staff(
                conn,
                tenant_id,
                "payment.partial",
                &format!(
                    "Partial payment for invoice {}",
                    invoice.number.as_deref().unwrap_or("-")
                ),
                Some(&format!(
                    "Received {} {}, {} still open",
                    amount.amount,
                    amount.currency,
                    paid_invoice.map_or(Decimal::ZERO, |i| i.balance)
                )),
                Some(&format!("/admin/invoices/{}", invoice.id)),
                &json!({"invoice_id": invoice.id}),
            )
            .await?;
        }
        if outcome.invoice_paid {
            self.after_invoice_paid(conn, tenant_id, invoice.id).await?;
        }
        Ok(EventOutcome::PaymentRecorded {
            invoice_id: Some(invoice.id),
            invoice_paid: outcome.invoice_paid,
            credited: outcome.credited,
        })
    }

    /// Stores `token` as the client's default card; `false` when the token
    /// is stored for another client (it is never moved between clients).
    async fn store_payment_method(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        client_id: ClientId,
        integration_id: IntegrationId,
        token: &SecretString,
        label: Option<&str>,
    ) -> BillingResult<bool> {
        if token.is_empty() {
            return Ok(true);
        }
        let id = Uuid::now_v7();
        let sealed = secrets::seal(
            self.secret_box(),
            token.expose_str(),
            &secrets::payment_method_aad(tenant_id, id),
        )?;
        let saved = billing_support::save_payment_method(
            &mut *conn,
            tenant_id,
            &NewPaymentMethod {
                id,
                client_id,
                integration_id,
                token_enc: sealed,
                token_hash: hos_core::crypto::sha256_hex(
                    format!("{integration_id}:{}", token.expose_str()).as_bytes(),
                ),
                label: label
                    .map(|l| l.chars().take(64).collect::<String>())
                    .filter(|l| !l.trim().is_empty()),
            },
        )
        .await?
        .is_some();
        if !saved {
            tracing::warn!(%tenant_id, %client_id, %integration_id,
                "payment method token is stored for another client; not saved");
        }
        Ok(saved)
    }

    async fn apply_refund_event(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        event: &PaymentEvent,
    ) -> BillingResult<EventOutcome> {
        let Some(original) = payments::transaction_by_gateway_id(
            &mut *conn,
            tenant_id,
            Some(integration_id),
            &event.gateway_txn_id,
        )
        .await?
        .filter(|t| t.kind == TransactionKind::Payment) else {
            return Ok(EventOutcome::Rejected {
                reason: format!("refund for unknown payment {}", event.gateway_txn_id),
            });
        };
        let amount = event.amount.amount.abs();
        let gateway_refund_id = event
            .gateway_refund_id
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        // 1. The refund this gateway refund id was recorded as (a refund
        //    completed synchronously, or an earlier delivery of the event).
        let mut known = match gateway_refund_id {
            Some(rid) => {
                payments::refund_by_gateway_id(&mut *conn, tenant_id, integration_id, rid).await?
            }
            None => None,
        };
        // 2. A refund started here, identified by the reference the gateway echoes.
        if known.is_none()
            && let Some(id) = hos_payments::event_refund_reference(event)
                .and_then(|r| Uuid::parse_str(r).ok())
                .map(hos_core::ids::RefundId::from_uuid)
        {
            known = payments::get_refund(&mut *conn, tenant_id, id)
                .await?
                .filter(|r| r.transaction_id == original.id && r.status != RefundStatus::Failed);
        }
        if let Some(r) = &known
            && r.status == RefundStatus::Succeeded
        {
            return Ok(EventOutcome::Duplicate);
        }
        // 3. A refund started from the admin (pending, same amount, not yet
        //    tied to another gateway refund); otherwise the refund was made in
        //    the gateway dashboard.
        if known.is_none() {
            known = payments::transaction_refunds(&mut *conn, tenant_id, original.id)
                .await?
                .into_iter()
                .find(|r| {
                    r.status == RefundStatus::Pending
                        && r.amount == amount
                        && !r.to_credit
                        && (r.gateway_refund_id.is_none()
                            || r.gateway_refund_id.as_deref() == gateway_refund_id)
                });
        }
        let refund = match known {
            Some(r) => r,
            None => {
                payments::create_gateway_refund(
                    &mut *conn,
                    tenant_id,
                    original.id,
                    amount,
                    event.message.as_deref().or(Some("Refunded at the gateway")),
                    gateway_refund_id,
                )
                .await?
            }
        };
        // The money is back with the customer: record it even when the
        // credit the payment created was spent (balance goes negative).
        let settled = payments::settle_refund(
            &mut *conn,
            tenant_id,
            refund.id,
            true,
            gateway_refund_id,
            None,
        )
        .await?;
        self.flag_refund_shortfall(conn, tenant_id, &settled)
            .await?;
        let done = settled.refund;
        self.emit_event(
            conn,
            tenant_id,
            "payment.refunded",
            json!({"transaction_id": original.id, "refund_id": done.id, "amount": amount,
                   "currency": done.currency, "invoice_id": done.invoice_id}),
        )
        .await?;
        Ok(EventOutcome::Refunded { refund_id: done.id })
    }

    async fn apply_chargeback(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        integration_id: IntegrationId,
        gateway_kind: &str,
        event: &PaymentEvent,
    ) -> BillingResult<EventOutcome> {
        let Some(original) = payments::transaction_by_gateway_id(
            &mut *conn,
            tenant_id,
            Some(integration_id),
            &event.gateway_txn_id,
        )
        .await?
        else {
            return Ok(EventOutcome::Rejected {
                reason: format!("chargeback for unknown payment {}", event.gateway_txn_id),
            });
        };
        if original.kind != TransactionKind::Payment {
            return Ok(EventOutcome::Rejected {
                reason: format!("chargeback for unknown payment {}", event.gateway_txn_id),
            });
        }
        // Settled like a refund made at the gateway: the overpayment / top-up
        // credit the payment created is taken back (the balance may go
        // negative), the invoice loses only the part applied to it.
        let settled = payments::settle_chargeback(
            &mut *conn,
            tenant_id,
            original.id,
            event.amount.amount.abs(),
            Some(integration_id),
            gateway_kind,
            &json!({"original_txn": event.gateway_txn_id, "message": event.message,
                    "raw": hos_payments::raw_for_storage(&event.raw)}),
        )
        .await?;
        if settled.transaction.is_none() {
            // Nothing left of the payment (refunded / charged back before).
            return Ok(EventOutcome::Duplicate);
        }
        let amount = settled.amount;
        self.flag_chargeback_shortfall(conn, tenant_id, &original, &settled)
            .await?;
        let reopened = settled.invoice.clone();
        // Services are suspended only when an invoice lost payment (not
        // when only overpayment credit was charged back): the paid invoice
        // itself, and the invoices a mass payment paid with this money.
        let mut lost: Vec<InvoiceId> = settled.members.iter().map(|(i, _)| *i).collect();
        if let Some(invoice_id) = original.invoice_id
            && settled.split.from_invoice > Decimal::ZERO
        {
            lost.insert(0, invoice_id);
        }
        if !lost.is_empty()
            && BillingSettings::load(&mut *conn, tenant_id)
                .await?
                .suspend_on_chargeback
        {
            for invoice_id in lost {
                for item in invoices::invoice_items(&mut *conn, tenant_id, invoice_id).await? {
                    let Some(service_id) = item.service_id else {
                        continue;
                    };
                    let Some(svc) =
                        services::get_service(&mut *conn, tenant_id, service_id).await?
                    else {
                        continue;
                    };
                    if svc.status == ServiceStatus::Active {
                        jobs::enqueue_service_job(
                            conn,
                            tenant_id,
                            JobKind::SuspendService,
                            &jobs::ServicePayload {
                                service_id,
                                reason: Some(SUSPEND_REASON_CHARGEBACK.to_owned()),
                                actor: None,
                            },
                            Some(format!("suspend:{service_id}:chargeback:{}", original.id)),
                        )
                        .await?;
                    }
                }
            }
        }
        self.notify_staff(
            conn,
            tenant_id,
            "payment.chargeback",
            &format!("Chargeback of {amount} {}", original.currency),
            event.message.as_deref(),
            original
                .invoice_id
                .map(|i| format!("/admin/invoices/{i}"))
                .as_deref(),
            &json!({"transaction_id": original.id, "invoice_id": original.invoice_id,
                    "reopened_invoices": settled.members.iter().map(|(i, _)| *i).collect::<Vec<_>>()}),
        )
        .await?;
        self.emit_event(
            conn,
            tenant_id,
            "payment.chargeback",
            json!({"transaction_id": original.id, "invoice_id": original.invoice_id,
                   "amount": amount, "currency": original.currency}),
        )
        .await?;
        Ok(EventOutcome::Chargeback {
            invoice_id: reopened.map(|i| i.id),
        })
    }

    /// Records an offline payment confirmed by staff (bank transfer, cash).
    /// Partial amounts leave the invoice open; excess becomes credit.
    pub async fn record_manual_payment(
        &self,
        tenant_id: TenantId,
        p: &ManualPayment,
    ) -> BillingResult<payments::PaymentOutcome> {
        let mut tx = self.pool().begin().await?;
        let invoice = billing_support::lock_invoice_row(&mut tx, tenant_id, p.invoice_id)
            .await?
            .ok_or_else(|| CoreError::not_found("invoice", p.invoice_id))?;
        if !invoice.status.is_open() {
            return Err(CoreError::Conflict(format!(
                "invoice is {}, not open",
                invoice.status
            )));
        }
        let amount = Money::new(p.amount, invoice.currency).round().amount;
        let outcome = payments::record_payment(
            &mut tx,
            tenant_id,
            &NewPayment {
                client_id: invoice.client_id,
                invoice_id: Some(invoice.id),
                integration_id: p.integration_id,
                gateway: p.method.clone().unwrap_or_else(|| "manual".into()),
                gateway_txn_id: p.reference.clone().filter(|r| !r.trim().is_empty()),
                amount,
                fee: Decimal::ZERO,
                currency: invoice.currency,
                description: p.note.clone(),
                raw: json!({}),
                occurred_at: p.occurred_at,
                created_by: p.staff_user_id,
            },
        )
        .await?;
        if outcome.invoice_paid {
            self.after_invoice_paid(&mut tx, tenant_id, invoice.id)
                .await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    /// Pays (part of) an invoice from the client's credit balance.
    pub async fn pay_with_credit(
        &self,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
        client_id: Option<ClientId>,
        max_amount: Option<Decimal>,
        user_id: Option<UserId>,
    ) -> BillingResult<(InvoiceRow, Decimal)> {
        let mut tx = self.pool().begin().await?;
        let invoice = self.load_invoice(&mut tx, tenant_id, invoice_id).await?;
        if client_id.is_some_and(|c| c != invoice.client_id) {
            return Err(CoreError::not_found("invoice", invoice_id));
        }
        let (updated, applied) =
            payments::apply_credit_to_invoice(&mut tx, tenant_id, invoice_id, max_amount, user_id)
                .await?;
        if applied > Decimal::ZERO && updated.status == InvoiceStatus::Paid {
            self.after_invoice_paid(&mut tx, tenant_id, invoice_id)
                .await?;
        }
        tx.commit().await?;
        Ok((updated, applied))
    }

    /// Issues an "add funds" invoice for a client (client area "top up
    /// credit"): one non-taxable `credit` line of `amount` in the client's
    /// currency. Once the invoice is paid, [`Billing::after_invoice_paid`]
    /// turns the line into client credit.
    pub async fn order_add_funds(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
        amount: Decimal,
    ) -> BillingResult<InvoiceRow> {
        let mut tx = self.pool().begin().await?;
        let client = self.load_client(&mut tx, tenant_id, client_id).await?;
        // Tenant limits (crate::funds): min / max per top-up, balance cap.
        let limits = crate::funds::FundsLimits::load(&mut *tx, tenant_id).await?;
        let balance = payments::credit_balance(&mut *tx, tenant_id, client_id, client.currency)
            .await?
            + payments::open_topups_total(&mut *tx, tenant_id, client_id, client.currency).await?;
        limits.check(amount, balance)?;
        if client.status != hos_db::enums::ClientStatus::Active {
            return Err(CoreError::Forbidden(
                "the client account is not active".into(),
            ));
        }
        let amount = Money::new(amount, client.currency).round().amount;
        let label = match client.locale {
            Locale::Tr => "Hesaba bakiye yükleme",
            Locale::En => "Account credit top-up",
        };
        let today = self.clock().today();
        let invoice = invoices::create_invoice(
            &mut tx,
            tenant_id,
            &invoices::NewInvoice {
                client_id,
                currency: client.currency,
                issue_date: today,
                due_date: today,
                issue: true,
                payment_integration_id: None,
                notes: None,
                billing_snapshot: crate::checkout::billing_snapshot(&client),
                items: vec![invoices::NewInvoiceItem {
                    item_type: InvoiceItemType::Credit,
                    taxable: false,
                    ..invoices::NewInvoiceItem::manual(label, amount)
                }],
                taxes: Vec::new(),
            },
        )
        .await?;
        self.queue_invoice_created_email(&mut tx, tenant_id, &invoice)
            .await?;
        tx.commit().await?;
        Ok(invoice)
    }

    /// Turns the paid "add funds" lines of an invoice into client credit,
    /// linked to the payments that paid for it (in payment order, each up to
    /// the part it applied to the invoice), so refunding such a payment takes
    /// the credit back. A part paid from credit stays unlinked.
    async fn credit_top_ups(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
        lines: &[&invoices::InvoiceItemRow],
    ) -> BillingResult<()> {
        let mut funding: Vec<(TransactionId, Decimal)> = Vec::new();
        for t in payments::invoice_transactions(&mut *conn, tenant_id, invoice.id).await? {
            if t.kind != TransactionKind::Payment
                || t.status != TransactionStatus::Succeeded
                || t.currency != invoice.currency
            {
                continue;
            }
            let (over, funds) =
                payments::payment_credit_origin(&mut *conn, tenant_id, t.id).await?;
            let available = t.amount - over - funds;
            if available > Decimal::ZERO {
                funding.push((t.id, available));
            }
        }
        let number = invoice.number.as_deref().unwrap_or("-");
        for line in lines {
            let description = format!("{} ({number})", line.description);
            let mut left = line.amount;
            for (txn, available) in &mut funding {
                if left <= Decimal::ZERO {
                    break;
                }
                let take = left.min(*available);
                if take <= Decimal::ZERO {
                    continue;
                }
                payments::add_payment_credit(
                    conn,
                    tenant_id,
                    invoice.client_id,
                    take,
                    invoice.currency,
                    &description,
                    Some(invoice.id),
                    Some(*txn),
                )
                .await?;
                *available -= take;
                left -= take;
            }
            if left > Decimal::ZERO {
                payments::add_payment_credit(
                    conn,
                    tenant_id,
                    invoice.client_id,
                    left,
                    invoice.currency,
                    &description,
                    Some(invoice.id),
                    None,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Tells staff that a refund took back credit the client had already
    /// spent (the client's credit balance went negative).
    async fn flag_refund_shortfall(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        settled: &payments::RefundSettlement,
    ) -> BillingResult<()> {
        if settled.shortfall <= Decimal::ZERO {
            return Ok(());
        }
        let r = &settled.refund;
        tracing::warn!(%tenant_id, refund_id = %r.id, shortfall = %settled.shortfall,
            "refund took back credit the client had already used");
        self.notify_staff(
            conn,
            tenant_id,
            "payment.refund_credit_shortfall",
            &format!(
                "Refund took back {} {} of credit the client had already used",
                settled.shortfall.normalize(),
                r.currency
            ),
            Some(
                "The payment was refunded at the gateway after the credit it created \
                 (overpayment or top-up) was spent. The client's credit balance is now \
                 negative: collect the difference.",
            ),
            Some(&format!("/admin/clients/{}", r.client_id)),
            &json!({"refund_id": r.id, "transaction_id": r.transaction_id,
                    "client_id": r.client_id, "shortfall": settled.shortfall,
                    "credit_reversed": settled.credit_reversed, "currency": r.currency}),
        )
        .await?;
        Ok(())
    }

    /// Tells staff that a chargeback took back credit the client had already
    /// spent (the client's credit balance went negative).
    async fn flag_chargeback_shortfall(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        original: &payments::TransactionRow,
        settled: &payments::ChargebackSettlement,
    ) -> BillingResult<()> {
        if settled.shortfall <= Decimal::ZERO {
            return Ok(());
        }
        tracing::warn!(%tenant_id, transaction_id = %original.id, shortfall = %settled.shortfall,
            "chargeback took back credit the client had already used");
        self.notify_staff(
            conn,
            tenant_id,
            "payment.chargeback_credit_shortfall",
            &format!(
                "Chargeback took back {} {} of credit the client had already used",
                settled.shortfall.normalize(),
                original.currency
            ),
            Some(
                "The payment was charged back after the credit it created \
                 (overpayment or top-up) was spent. The client's credit balance is now \
                 negative: collect the difference.",
            ),
            Some(&format!("/admin/clients/{}", original.client_id)),
            &json!({"transaction_id": original.id, "client_id": original.client_id,
                    "shortfall": settled.shortfall, "credit_reversed": settled.credit_reversed,
                    "currency": original.currency,
                    "chargeback_id": settled.transaction.as_ref().map(|t| t.id)}),
        )
        .await?;
        Ok(())
    }

    /// Refunds (part of) a payment: to credit immediately, or through the
    /// gateway (completed now, or by the gateway's `refunded` webhook).
    ///
    /// A refund back to the card / account first takes back the credit the
    /// payment created (overpayment, top-up) and lowers the invoice only by
    /// the rest; when the client already spent that credit the refund is
    /// refused with `Validation` before the gateway is called.
    pub async fn refund_payment(
        &self,
        tenant_id: TenantId,
        input: &RefundInput,
    ) -> BillingResult<RefundRow> {
        let mut conn = self.pool().acquire().await?;
        let refund = payments::create_refund(
            &mut conn,
            tenant_id,
            input.transaction_id,
            input.amount,
            input.reason.as_deref(),
            input.to_credit,
            input.staff_user_id,
        )
        .await?;
        if input.to_credit {
            return Ok(
                payments::settle_refund(&mut conn, tenant_id, refund.id, true, None, None)
                    .await?
                    .refund,
            );
        }
        let original: TransactionRow =
            sqlx::query_as("select * from transactions where tenant_id = $1 and id = $2")
                .bind(tenant_id)
                .bind(input.transaction_id)
                .fetch_one(&mut *conn)
                .await?;
        let (Some(integration_id), Some(txn_ref)) =
            (original.integration_id, original.gateway_txn_id.clone())
        else {
            let msg = "the payment was not made through a gateway; refund to credit instead";
            payments::complete_refund(&mut conn, tenant_id, refund.id, false, None, Some(msg))
                .await?;
            return Err(CoreError::validation(msg));
        };
        let gateway = self.registry().gateway(tenant_id, integration_id).await?;
        let base = self.tenant_url(tenant_id).await?;
        let result = gateway
            .refund_with_callback(
                &RefundRequest {
                    gateway_txn_id: txn_ref,
                    amount: Money::new(refund.amount, refund.currency),
                    reason: input.reason.clone(),
                    reference: refund.id.as_uuid().simple().to_string(),
                    ip: input.ip,
                },
                &format!("{base}/webhooks/gateway/{integration_id}"),
            )
            .await;
        let row = match result {
            Ok(r) if r.status == RefundStatus::Succeeded => {
                let settled = payments::settle_refund(
                    &mut conn,
                    tenant_id,
                    refund.id,
                    true,
                    r.gateway_refund_id.as_deref(),
                    None,
                )
                .await?;
                // The credit was checked before the gateway call; it can
                // only have been spent meanwhile.
                self.flag_refund_shortfall(&mut conn, tenant_id, &settled)
                    .await?;
                settled.refund
            }
            Ok(r) if r.status == RefundStatus::Pending => {
                // Completed by the gateway's webhook; keep its refund id so
                // the webhook is matched to this refund.
                match r.gateway_refund_id.as_deref() {
                    Some(rid) => {
                        payments::set_refund_gateway_id(&mut *conn, tenant_id, refund.id, rid)
                            .await?
                            .unwrap_or(refund)
                    }
                    None => refund,
                }
            }
            Ok(r) => {
                let msg = r.message.unwrap_or_else(|| "refund rejected".into());
                payments::complete_refund(&mut conn, tenant_id, refund.id, false, None, Some(&msg))
                    .await?
            }
            Err(e) => {
                let msg = e.to_string();
                payments::complete_refund(&mut conn, tenant_id, refund.id, false, None, Some(&msg))
                    .await?;
                return Err(e.into());
            }
        };
        Ok(row)
    }

    /// Charges an open invoice with the client's default stored payment
    /// method (automatic renewals; the `charge_invoice` job).
    pub async fn charge_stored_method(
        &self,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
    ) -> BillingResult<ChargeOutcome> {
        let mut conn = self.pool().acquire().await?;
        let invoice = self.load_invoice(&mut conn, tenant_id, invoice_id).await?;
        if !invoice.status.is_open() || invoice.balance <= Decimal::ZERO {
            return Ok(ChargeOutcome::Skipped {
                reason: format!("invoice is {}", invoice.status),
            });
        }
        let Some(method) =
            billing_support::default_payment_method(&mut *conn, tenant_id, invoice.client_id)
                .await?
        else {
            return Ok(ChargeOutcome::Skipped {
                reason: "no stored payment method".into(),
            });
        };
        let loaded = self
            .registry()
            .get(tenant_id, method.integration_id)
            .await?;
        let gateway = self
            .registry()
            .gateway(tenant_id, method.integration_id)
            .await?;
        if !gateway.capabilities().stored_payment_methods {
            return Ok(ChargeOutcome::Skipped {
                reason: format!("{} cannot charge stored methods", loaded.name),
            });
        }
        let token = secrets::open(
            self.secret_box(),
            &method.token_enc,
            &secrets::payment_method_aad(tenant_id, method.id),
        )?;
        let client = self
            .load_client(&mut conn, tenant_id, invoice.client_id)
            .await?;
        let amount = invoice.balance_money().round();
        let reference = new_reference();
        let attempt = billing_support::create_payment_attempt(
            &mut *conn,
            tenant_id,
            &NewPaymentAttempt {
                invoice_id,
                integration_id: Some(method.integration_id),
                reference: reference.clone(),
                amount: amount.amount,
                currency: amount.currency,
                save_payment_method: false,
            },
        )
        .await?;
        let now = self.clock().now();
        let result = gateway
            .charge_stored(&StoredChargeRequest {
                invoice_id,
                reference: reference.clone(),
                amount,
                description: invoice_label(&invoice, client.locale),
                customer: customer(&client, None),
                payment_method_ref: token,
            })
            .await;
        drop(conn);
        match result {
            Ok(mut event) => {
                if event.invoice_ref.is_none() {
                    event.invoice_ref = Some(reference);
                }
                let kind = event.kind;
                let mut tx = self.pool().begin().await?;
                let outcome = self
                    .apply_payment_event(
                        &mut tx,
                        tenant_id,
                        method.integration_id,
                        &loaded.kind,
                        &event,
                    )
                    .await?;
                let error = (kind == PaymentEventKind::Failed)
                    .then(|| event.message.clone().unwrap_or_else(|| "declined".into()));
                billing_support::record_payment_method_use(
                    &mut *tx,
                    tenant_id,
                    method.id,
                    now,
                    error.as_deref(),
                )
                .await?;
                tx.commit().await?;
                Ok(match (kind, outcome) {
                    (_, EventOutcome::PaymentRecorded { invoice_paid, .. }) => {
                        ChargeOutcome::Charged { invoice_paid }
                    }
                    (PaymentEventKind::Pending, _) => ChargeOutcome::Pending,
                    (_, EventOutcome::Rejected { reason }) => {
                        ChargeOutcome::Declined { message: reason }
                    }
                    _ => ChargeOutcome::Declined {
                        message: error.unwrap_or_else(|| "declined".into()),
                    },
                })
            }
            Err(e) => {
                let msg = e.to_string();
                billing_support::set_attempt_status(self.pool(), tenant_id, attempt.id, "failed")
                    .await?;
                billing_support::record_payment_method_use(
                    self.pool(),
                    tenant_id,
                    method.id,
                    now,
                    Some(&msg),
                )
                .await?;
                if e.is_retryable() {
                    return Err(e.into());
                }
                Ok(ChargeOutcome::Declined { message: msg })
            }
        }
    }

    /// Everything that follows a fully paid invoice, inside the caller's
    /// transaction: order acceptance + provisioning, renewal of services
    /// (due date advance, reactivation) and domains, paid plan changes,
    /// the `invoice_paid` e-mail and the `invoice.paid` webhook event.
    pub async fn after_invoice_paid(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
    ) -> BillingResult<()> {
        // A paid mass payment pays further invoices (crate::mass_pay); they
        // are handled here in turn instead of recursively.
        let mut queue = vec![invoice_id];
        while let Some(id) = queue.pop() {
            self.after_one_invoice_paid(conn, tenant_id, id, &mut queue)
                .await?;
        }
        Ok(())
    }

    async fn after_one_invoice_paid(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice_id: InvoiceId,
        queue: &mut Vec<InvoiceId>,
    ) -> BillingResult<()> {
        let invoice = self.load_invoice(conn, tenant_id, invoice_id).await?;
        if invoice.status != InvoiceStatus::Paid {
            return Ok(());
        }
        let settings = BillingSettings::load(&mut *conn, tenant_id).await?;

        // ---- order: accept + provision -------------------------------------
        if let Some(order) =
            billing_support::order_for_invoice(&mut *conn, tenant_id, invoice_id).await?
        {
            let accepted = match order.status {
                OrderStatus::Accepted => true,
                OrderStatus::Pending if settings.auto_accept_orders => {
                    self.accept_order_in(conn, tenant_id, &order, true).await?
                }
                _ => false,
            };
            let order = orders::get_order(&mut *conn, tenant_id, order.id)
                .await?
                .ok_or_else(|| CoreError::not_found("order", order.id))?;
            self.queue_order_provisioning(
                conn,
                tenant_id,
                &order,
                ProvisionTrigger::Payment,
                accepted,
            )
            .await?;
        }

        // ---- renewals ----------------------------------------------------------
        let items = invoices::invoice_items(&mut *conn, tenant_id, invoice_id).await?;
        let mut top_ups = Vec::new();
        for item in &items {
            match (
                item.item_type,
                item.service_id,
                item.domain_id,
                item.period_start,
            ) {
                (InvoiceItemType::Service, Some(service_id), _, Some(start)) => {
                    let Some(svc) =
                        billing_support::lock_service(conn, tenant_id, service_id).await?
                    else {
                        continue;
                    };
                    if svc.next_due_date != Some(start) || svc.status.is_terminal() {
                        continue;
                    }
                    let anchor = svc
                        .anchor_day
                        .and_then(|d| u32::try_from(d).ok())
                        .unwrap_or_else(|| chrono::Datelike::day(&start));
                    let Some(next) = svc.billing_cycle.next_due_anchored(start, anchor) else {
                        continue;
                    };
                    let next_invoice = next.checked_sub_days(Days::new(
                        u64::try_from(settings.invoice_days_before_due).unwrap_or(0),
                    ));
                    services::advance_due_date(
                        &mut *conn,
                        tenant_id,
                        service_id,
                        start,
                        next,
                        next_invoice,
                    )
                    .await?;
                    services::log_event(
                        &mut *conn,
                        tenant_id,
                        service_id,
                        &services::NewServiceEvent::info(
                            "renewed",
                            &format!(
                                "renewed until {next} (invoice {})",
                                invoice.number.as_deref().unwrap_or("-")
                            ),
                        ),
                    )
                    .await?;
                    if svc.status == ServiceStatus::Suspended
                        && svc.suspended_reason.as_deref() == Some(SUSPEND_REASON_OVERDUE)
                        && billing_support::open_invoices_for_service(
                            &mut *conn, tenant_id, service_id,
                        )
                        .await?
                        .iter()
                        .all(|i| !i.is_overdue(self.clock().today()) || i.id == invoice_id)
                    {
                        jobs::enqueue_service_job(
                            conn,
                            tenant_id,
                            JobKind::UnsuspendService,
                            &jobs::ServicePayload {
                                service_id,
                                reason: Some("payment received".to_owned()),
                                actor: None,
                            },
                            Some(format!("unsuspend:{service_id}:{invoice_id}")),
                        )
                        .await?;
                    }
                }
                (InvoiceItemType::Domain, _, Some(domain_id), Some(start)) => {
                    let Some(d) = domains::get_domain(&mut *conn, tenant_id, domain_id).await?
                    else {
                        continue;
                    };
                    let renewable = d.status.is_renewable() || d.status == DomainStatus::Expired;
                    if renewable && d.next_due_date == Some(start) {
                        let years = item
                            .period_end
                            .map(|end| {
                                let months = (chrono::Datelike::year(&end)
                                    - chrono::Datelike::year(&start))
                                    * 12
                                    + i32::try_from(chrono::Datelike::month(&end)).unwrap_or(0)
                                    - i32::try_from(chrono::Datelike::month(&start)).unwrap_or(0)
                                    + 1;
                                i16::try_from((months / 12).max(1)).unwrap_or(1)
                            })
                            .unwrap_or(d.registration_period.max(1));
                        jobs::enqueue_domain_job(
                            conn,
                            tenant_id,
                            JobKind::RenewDomain,
                            domain_id,
                            Some(years),
                            Some(format!("renew_domain:{domain_id}:{start}")),
                        )
                        .await?;
                    }
                }
                (InvoiceItemType::Credit, _, _, _) if item.amount > Decimal::ZERO => {
                    // "Add funds" line (`order_add_funds`): the paid amount
                    // becomes client credit, once per line.
                    if billing_support::mark_once(
                        &mut *conn,
                        tenant_id,
                        &format!("add_funds:{}", item.id),
                    )
                    .await?
                    {
                        top_ups.push(item);
                    }
                }
                _ => {}
            }
        }
        if !top_ups.is_empty() {
            self.credit_top_ups(conn, tenant_id, &invoice, &top_ups)
                .await?;
        }
        // Mass payment (crate::mass_pay): the credit just added pays the
        // invoices it was made for, each exactly once.
        queue.extend(self.apply_mass_payment(conn, tenant_id, &invoice).await?);
        // A combined invoice still waiting for this invoice is void now.
        self.cancel_superseded_mass_payments(conn, tenant_id, &invoice)
            .await?;

        // ---- paid plan changes -----------------------------------------------
        for up in
            billing_support::pending_upgrades_for_invoice(&mut *conn, tenant_id, invoice_id).await?
        {
            jobs::enqueue(
                conn,
                jobs::JobRequest::new(
                    JobKind::ChangePackage,
                    Some(tenant_id),
                    &jobs::ChangePackagePayload {
                        service_id: up.service_id,
                        upgrade_id: up.id,
                    },
                )
                .dedupe(format!("change_package:{}", up.id)),
            )
            .await?;
        }

        // ---- affiliate commission (own savepoint; never blocks the payment) ---
        self.accrue_commissions_isolated(conn, tenant_id, &invoice)
            .await?;

        // ---- e-invoice (own savepoint; never blocks the payment) ---------------
        self.einvoice_on_paid(conn, tenant_id, invoice_id).await;

        // ---- notifications -----------------------------------------------------
        let client = self.load_client(conn, tenant_id, invoice.client_id).await?;
        let url = self.tenant_url(tenant_id).await?;
        let paid_at = invoice.paid_at.unwrap_or_else(|| self.clock().now());
        let method = invoice_payment_method(&mut *conn, tenant_id, invoice_id).await?;
        self.queue_client_email(
            conn,
            tenant_id,
            invoice.client_id,
            hos_mail::defaults::keys::INVOICE_PAID,
            json!({
                "invoice": {
                    "number": invoice.number.clone().unwrap_or_default(),
                    "total": crate::format::money(&invoice.total_money(), client.locale),
                    "due_date": crate::format::date(invoice.due_date, client.locale),
                    "paid_at": crate::format::date(paid_at.date_naive(), client.locale),
                    "url": format!("{url}/client/invoices/{}", invoice.id),
                },
                "payment": { "method": method},
            }),
            Some(("invoice", *invoice.id.as_uuid())),
        )
        .await?;
        self.emit_event(
            conn,
            tenant_id,
            "invoice.paid",
            json!({"invoice_id": invoice.id, "number": invoice.number, "client_id": invoice.client_id,
                   "total": invoice.total, "currency": invoice.currency}),
        )
        .await?;
        Ok(())
    }
}

/// Name of the last successful payment method of an invoice (`credit` when
/// paid from credit only).
async fn invoice_payment_method(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    invoice_id: InvoiceId,
) -> BillingResult<String> {
    let txns = payments::invoice_transactions(&mut *conn, tenant_id, invoice_id).await?;
    Ok(txns
        .iter()
        .rev()
        .find(|t| t.kind == TransactionKind::Payment && t.status == TransactionStatus::Succeeded)
        .map_or_else(|| "credit".to_owned(), |t| t.gateway.clone()))
}

/// Whether a gateway is offline (bank transfer): payments are confirmed by staff.
pub fn is_offline(gateway: &dyn PaymentGateway) -> bool {
    gateway.capabilities().offline
}
