//! Quotes (estimates): pricing, sending with a secret accept link, the
//! recipient's accept / decline, conversion into an order + invoice, PDF and
//! the daily expiry.
//!
//! Rules:
//!
//! * every amount is computed here from the lines (`quantity × unit price ×
//!   (1 − discount %)`, rounded to the currency) and taxes come from the tax
//!   rules of the recipient (the tenant's home country for a prospect); the
//!   web layer never supplies totals;
//! * the accept link carries a 256-bit random token; only its SHA-256 is
//!   stored, it expires the day after `valid_until` and is cleared once the
//!   quote is accepted, declined, expired or cancelled;
//! * acceptance locks the quote row and only moves a `sent`, unexpired quote:
//!   a replayed accept finds it `accepted` and creates nothing;
//! * product lines become pending services of a new order (provisioned like
//!   any order when its invoice is paid); free lines become plain invoice
//!   lines.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::{Days, NaiveDate, NaiveTime};
use hos_core::CoreError;
use hos_core::domain::{InvoiceStatus, Locale};
use hos_core::ids::{ClientId, InvoiceId, OrderId, ProductId, TenantId, UserId};
use hos_core::integrations::mailer::Mailbox;
use hos_core::money::{BillingCycle, Currency, Money};
use hos_db::enums::{ClientStatus, LineItemType};
use hos_db::repo::clients::ClientRow;
use hos_db::repo::invoices::{self, NewInvoice, NewInvoiceItem, TotalsLine, TotalsTax};
use hos_db::repo::orders::{self, NewOrder, NewOrderItem};
use hos_db::repo::quotes::{
    self as qrepo, QuoteId, QuoteInput, QuoteItemInput, QuoteItemRow, QuoteRow, QuoteStatus,
};
use hos_db::repo::services::{self, NewService, NewServiceEvent};
use hos_db::repo::{admin_settings, catalog, clients, payments, platform};
use hos_pdf::format::{date, money, percent, quantity};
use hos_pdf::labels::for_locale;
use hos_pdf::{
    InvoicePdf, Logo, LogoFormat, MAX_FIELD_CHARS, MAX_NOTE_CHARS, PdfLine, PdfTotal, Stamp,
    StampTone, clean, paragraphs,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};

use crate::checkout::{ProvisionTrigger, billing_snapshot};
use crate::invoice_pdf::RenderedPdf;
use crate::notify::OutgoingEmail;
use crate::pricing::{TaxContext, tax_rates};
use crate::settings::BillingSettings;
use crate::{Billing, BillingResult};

/// Most lines on one quote.
pub const MAX_LINES: usize = 100;
/// Longest validity a quote can be given.
pub const MAX_VALID_DAYS: u64 = 366;
/// `email_templates.key` of the quote e-mail.
pub const EMAIL_TEMPLATE: &str = hos_mail::defaults::keys::QUOTE;

/// A line as entered by staff.
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteLine {
    pub product_id: Option<ProductId>,
    pub billing_cycle: Option<BillingCycle>,
    pub description: String,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub discount_percent: Decimal,
    pub taxable: bool,
}

/// A quote as entered by staff.
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteDraft {
    pub client_id: Option<ClientId>,
    pub prospect_name: Option<String>,
    pub prospect_email: Option<String>,
    pub prospect_company: Option<String>,
    pub subject: String,
    pub currency: Currency,
    pub valid_until: NaiveDate,
    pub notes: Option<String>,
    pub admin_notes: Option<String>,
    pub lines: Vec<QuoteLine>,
}

/// Result of [`Billing::accept_quote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteAcceptance {
    pub quote_id: QuoteId,
    pub order_id: OrderId,
    pub invoice_id: InvoiceId,
    /// Settled at once (zero total or account credit).
    pub paid: bool,
}

/// `round(quantity × unit_price × (1 − discount / 100))` and the discount
/// it removed, both rounded to the currency's minor unit.
pub fn line_amount(currency: Currency, l: &QuoteLine) -> (Decimal, Decimal) {
    let round = |d: Decimal| Money::new(d, currency).round().amount;
    let gross = round(l.quantity * l.unit_price);
    let net = round(
        l.quantity * l.unit_price * (Decimal::ONE_HUNDRED - l.discount_percent)
            / Decimal::ONE_HUNDRED,
    );
    (net, gross - net)
}

fn validate_line(l: &QuoteLine) -> BillingResult<()> {
    let desc = l.description.trim();
    if desc.is_empty() || desc.chars().count() > 500 {
        return Err(CoreError::validation(
            "every line needs a description (up to 500 characters)",
        ));
    }
    if l.quantity <= Decimal::ZERO || l.quantity > Decimal::from(100_000) || l.quantity.scale() > 4
    {
        return Err(CoreError::validation(
            "the quantity must be between 0 and 100,000",
        ));
    }
    if l.unit_price < Decimal::ZERO || l.unit_price > Decimal::from(1_000_000_000) {
        return Err(CoreError::validation(
            "the unit price must be between 0 and 1,000,000,000",
        ));
    }
    if l.discount_percent < Decimal::ZERO || l.discount_percent > Decimal::ONE_HUNDRED {
        return Err(CoreError::validation(
            "the discount must be between 0 and 100 %",
        ));
    }
    if l.product_id.is_some() {
        if l.billing_cycle.is_none() {
            return Err(CoreError::validation(
                "a product line needs a billing cycle",
            ));
        }
        if l.quantity.fract() != Decimal::ZERO || l.quantity > Decimal::from(1000) {
            return Err(CoreError::validation(
                "a product line needs a whole quantity (1–1000)",
            ));
        }
    }
    Ok(())
}

/// A 256-bit accept-link token and its stored hash.
pub fn new_accept_token() -> (String, String) {
    let token = hos_core::crypto::generate_token();
    let hash = hos_core::crypto::hash_token(&token);
    (token, hash)
}

/// The stored hash of an accept-link token from a URL (`None` when the value
/// cannot be a token).
pub fn accept_token_hash(token: &str) -> Option<String> {
    let t = token.trim();
    (t.len() == 43
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then(|| hos_core::crypto::hash_token(t))
}

fn display_name(c: &ClientRow) -> String {
    let person = format!("{} {}", c.first_name, c.last_name)
        .trim()
        .to_owned();
    match c
        .company_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(company) if person.is_empty() => company.to_owned(),
        _ => person,
    }
}

impl Billing {
    /// Tax context of a quote's recipient.
    async fn quote_tax_context(
        &self,
        conn: &mut sqlx::PgConnection,
        tenant_id: TenantId,
        client: Option<&ClientRow>,
        settings: &BillingSettings,
    ) -> BillingResult<Vec<crate::pricing::TaxRate>> {
        let ctx = match client {
            Some(c) => TaxContext::for_client(c, settings),
            None => TaxContext {
                country_code: settings.tax_home_country.clone(),
                region: None,
                exempt: false,
            },
        };
        tax_rates(conn, tenant_id, &ctx).await
    }

    /// Validates, prices and stores a quote (`id = None` creates one).
    pub async fn save_quote(
        &self,
        tenant_id: TenantId,
        id: Option<QuoteId>,
        draft: &QuoteDraft,
        staff: Option<UserId>,
    ) -> BillingResult<QuoteRow> {
        let subject = draft.subject.trim();
        if subject.is_empty() || subject.chars().count() > 200 {
            return Err(CoreError::validation(
                "the subject is required (up to 200 characters)",
            ));
        }
        if draft.lines.is_empty() || draft.lines.len() > MAX_LINES {
            return Err(CoreError::validation(format!(
                "a quote has 1 to {MAX_LINES} lines"
            )));
        }
        let today = self.clock().today();
        if draft.valid_until < today
            || draft.valid_until
                > today
                    .checked_add_days(Days::new(MAX_VALID_DAYS))
                    .unwrap_or(today)
        {
            return Err(CoreError::validation(
                "the validity date must be within the next year",
            ));
        }
        for l in &draft.lines {
            validate_line(l)?;
        }
        let mut conn = self.pool().acquire().await?;
        let currencies = catalog::list_currencies(&mut *conn, tenant_id, true).await?;
        if !currencies.iter().any(|c| c.code == draft.currency) {
            return Err(CoreError::validation("the currency is not enabled"));
        }
        let client = match draft.client_id {
            Some(cid) => Some(
                clients::get_client(&mut *conn, tenant_id, cid)
                    .await?
                    .ok_or_else(|| CoreError::not_found("client", cid))?,
            ),
            None => {
                let email = draft.prospect_email.as_deref().unwrap_or("").trim();
                if !is_email(email) {
                    return Err(CoreError::validation(
                        "a client or a valid prospect e-mail is required",
                    ));
                }
                None
            }
        };
        for l in &draft.lines {
            if let Some(pid) = l.product_id {
                catalog::get_product(&mut *conn, tenant_id, pid)
                    .await?
                    .ok_or_else(|| {
                        CoreError::validation("a product of the quote does not exist")
                    })?;
            }
        }
        let settings = BillingSettings::load(&mut *conn, tenant_id).await?;
        let rates = self
            .quote_tax_context(&mut conn, tenant_id, client.as_ref(), &settings)
            .await?;
        let mut items = Vec::with_capacity(draft.lines.len());
        let mut totals_lines = Vec::with_capacity(draft.lines.len());
        let mut discount_total = Decimal::ZERO;
        for l in &draft.lines {
            let (amount, discount) = line_amount(draft.currency, l);
            discount_total += discount;
            totals_lines.push(TotalsLine {
                amount,
                taxable: l.taxable,
                is_discount: false,
            });
            items.push(QuoteItemInput {
                product_id: l.product_id,
                billing_cycle: l.product_id.and(l.billing_cycle),
                description: l.description.trim().to_owned(),
                quantity: l.quantity,
                unit_price: l.unit_price,
                discount_percent: l.discount_percent,
                amount,
                taxable: l.taxable,
            });
        }
        let taxes: Vec<TotalsTax> = rates
            .iter()
            .map(|r| TotalsTax {
                rate: r.rate,
                is_compound: r.is_compound,
            })
            .collect();
        let totals = invoices::compute_totals(draft.currency, &totals_lines, &taxes);
        let tax_lines: Vec<Value> = rates
            .iter()
            .zip(&totals.taxes)
            .map(|(r, (base, tax))| {
                json!({"name": r.name, "rate": r.rate, "is_compound": r.is_compound,
                       "taxable_amount": base, "tax_amount": tax})
            })
            .collect();
        let trim = |s: &Option<String>, max: usize| {
            s.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(max).collect::<String>())
        };
        let input = QuoteInput {
            client_id: client.as_ref().map(|c| c.id),
            prospect_name: client
                .is_none()
                .then(|| trim(&draft.prospect_name, 200))
                .flatten(),
            prospect_email: client
                .is_none()
                .then(|| trim(&draft.prospect_email, 254).map(|e| e.to_lowercase()))
                .flatten(),
            prospect_company: client
                .is_none()
                .then(|| trim(&draft.prospect_company, 200))
                .flatten(),
            subject: subject.to_owned(),
            currency: draft.currency,
            valid_until: draft.valid_until,
            notes: trim(&draft.notes, 10_000),
            admin_notes: trim(&draft.admin_notes, 10_000),
            subtotal: totals.subtotal,
            discount_total,
            tax_total: totals.tax_total,
            total: totals.total,
            tax_lines: Value::Array(tax_lines),
            items,
        };
        qrepo::save_quote(&mut conn, tenant_id, id, &input, staff).await
    }

    /// Issues a new accept link (the old one stops working), marks the quote
    /// sent and queues the e-mail to the client or the prospect. Returns the
    /// quote and the link (shown once to staff, e.g. to share by hand).
    pub async fn send_quote(
        &self,
        tenant_id: TenantId,
        id: QuoteId,
    ) -> BillingResult<(QuoteRow, String)> {
        let today = self.clock().today();
        let now = self.clock().now();
        let mut tx = self.pool().begin().await?;
        let quote = qrepo::lock_quote(&mut tx, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("quote", id))?;
        if !quote.status.is_editable() {
            return Err(CoreError::Conflict(format!(
                "a {} quote cannot be sent",
                quote.status
            )));
        }
        if quote.is_past_validity(today) {
            return Err(CoreError::validation(
                "the quote's validity date has passed",
            ));
        }
        if qrepo::quote_items(&mut *tx, tenant_id, id)
            .await?
            .is_empty()
        {
            return Err(CoreError::validation("the quote has no lines"));
        }
        let (token, hash) = new_accept_token();
        let expires = quote
            .valid_until
            .checked_add_days(Days::new(1))
            .unwrap_or(quote.valid_until)
            .and_time(NaiveTime::MIN)
            .and_utc();
        let quote = qrepo::mark_sent(&mut *tx, tenant_id, id, &hash, expires, now)
            .await?
            .ok_or_else(|| CoreError::Conflict("the quote can no longer be sent".into()))?;
        let base = self.tenant_url(tenant_id).await?;
        let link = format!("{base}/quote/{}?token={token}", quote.id);
        let tenant = platform::tenant_context(&mut *tx, tenant_id)
            .await?
            .ok_or_else(|| CoreError::not_found("tenant", tenant_id))?;
        let (to, locale, client_id) = match quote.client_id {
            Some(cid) => {
                let c = clients::get_client(&mut *tx, tenant_id, cid)
                    .await?
                    .ok_or_else(|| CoreError::not_found("client", cid))?;
                (
                    Mailbox {
                        name: Some(display_name(&c)),
                        email: c.email.clone(),
                    },
                    c.locale,
                    Some(cid),
                )
            }
            None => (
                Mailbox {
                    name: quote.prospect_name.clone(),
                    email: quote.prospect_email.clone().unwrap_or_default(),
                },
                tenant.default_locale,
                None,
            ),
        };
        let total = crate::format::money(&Money::new(quote.total, quote.currency), locale);
        self.queue_email(
            &mut tx,
            tenant_id,
            &OutgoingEmail {
                template_key: EMAIL_TEMPLATE.to_owned(),
                to,
                locale,
                context: json!({
                    "quote": {
                        "number": quote.number(),
                        "subject": quote.subject,
                        "total": total,
                        "valid_until": crate::format::date(quote.valid_until, locale),
                    },
                    "action_url": link,
                }),
                client_id,
                user_id: None,
                related: Some(("quote".to_owned(), *quote.id.as_uuid())),
                reply_to: None,
            },
        )
        .await?;
        tx.commit().await?;
        Ok((quote, link))
    }

    /// The quote an accept-link token opens (`None`: unknown, expired or
    /// already answered). Marks the first view.
    pub async fn quote_for_token(
        &self,
        tenant_id: TenantId,
        id: QuoteId,
        token: &str,
    ) -> BillingResult<Option<QuoteRow>> {
        let Some(hash) = accept_token_hash(token) else {
            return Ok(None);
        };
        let now = self.clock().now();
        let quote = qrepo::quote_by_token_hash(self.pool(), tenant_id, &hash, now)
            .await?
            .filter(|q| q.id == id && q.status == QuoteStatus::Sent);
        if let Some(q) = &quote {
            qrepo::mark_viewed(self.pool(), tenant_id, q.id, now).await?;
        }
        Ok(quote)
    }

    /// The recipient declines a sent quote. `client_id` (client area) must
    /// own the quote; a token link passes `None` after checking the token.
    pub async fn decline_quote(
        &self,
        tenant_id: TenantId,
        id: QuoteId,
        client_id: Option<ClientId>,
        reason: Option<&str>,
    ) -> BillingResult<QuoteRow> {
        let mut tx = self.pool().begin().await?;
        let quote = qrepo::lock_quote(&mut tx, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("quote", id))?;
        if client_id.is_some() && quote.client_id != client_id {
            return Err(CoreError::not_found("quote", id));
        }
        let reason = reason
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|r| r.chars().take(1000).collect::<String>());
        let row = qrepo::transition_quote(
            &mut *tx,
            tenant_id,
            id,
            &[QuoteStatus::Sent],
            QuoteStatus::Declined,
            reason.as_deref(),
            self.clock().now(),
        )
        .await?
        .ok_or_else(|| {
            CoreError::Conflict(format!("a {} quote cannot be declined", quote.status))
        })?;
        self.notify_staff(
            &mut tx,
            tenant_id,
            "quote.declined",
            &format!("Quote {} declined", row.number()),
            reason.as_deref(),
            Some(&format!("/admin/quotes/{}", row.id)),
            &json!({"quote_id": row.id}),
        )
        .await?;
        self.emit_event(
            &mut tx,
            tenant_id,
            "quote.declined",
            json!({"quote_id": row.id, "number": row.number()}),
        )
        .await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Staff withdraws a draft / sent quote.
    pub async fn cancel_quote(&self, tenant_id: TenantId, id: QuoteId) -> BillingResult<QuoteRow> {
        qrepo::transition_quote(
            self.pool(),
            tenant_id,
            id,
            &[QuoteStatus::Draft, QuoteStatus::Sent],
            QuoteStatus::Cancelled,
            None,
            self.clock().now(),
        )
        .await?
        .ok_or_else(|| CoreError::Conflict("only a draft or sent quote can be cancelled".into()))
    }

    /// Accepts a sent quote for `client_id` and converts it into an order
    /// (pending services for product lines) and an issued invoice.
    ///
    /// A quote addressed to a client can only be accepted for that client; a
    /// prospect quote is attached to the accepting client (the caller checked
    /// the secret link). Replays find the quote accepted and fail with
    /// `Conflict` without creating anything.
    pub async fn accept_quote(
        &self,
        tenant_id: TenantId,
        id: QuoteId,
        client_id: ClientId,
        user_id: Option<UserId>,
        ip: Option<IpAddr>,
    ) -> BillingResult<QuoteAcceptance> {
        let today = self.clock().today();
        let now = self.clock().now();
        let mut tx = self.pool().begin().await?;
        let conn = &mut *tx;
        let quote = qrepo::lock_quote(conn, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("quote", id))?;
        if quote.client_id.is_some_and(|c| c != client_id) {
            return Err(CoreError::not_found("quote", id));
        }
        if quote.status != QuoteStatus::Sent {
            return Err(CoreError::Conflict(format!(
                "a {} quote cannot be accepted",
                quote.status
            )));
        }
        if quote.is_past_validity(today) {
            return Err(CoreError::validation("the quote has expired"));
        }
        let client = clients::get_client(&mut *conn, tenant_id, client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", client_id))?;
        if client.status != ClientStatus::Active {
            return Err(CoreError::Forbidden(
                "the client account is not active".into(),
            ));
        }
        let lines = qrepo::quote_items(&mut *conn, tenant_id, id).await?;
        if lines.is_empty() {
            return Err(CoreError::validation("the quote has no lines"));
        }
        let settings = BillingSettings::load(&mut *conn, tenant_id).await?;
        let rates = self
            .quote_tax_context(conn, tenant_id, Some(&client), &settings)
            .await?;
        let currency = quote.currency;
        let round = |d: Decimal| Money::new(d, currency).round().amount;

        // ---- order ---------------------------------------------------------------
        let order_items: Vec<NewOrderItem> = lines.iter().map(|l| order_item(l, round)).collect();
        let totals_lines: Vec<TotalsLine> = lines
            .iter()
            .map(|l| TotalsLine {
                amount: l.amount,
                taxable: l.taxable,
                is_discount: false,
            })
            .collect();
        let taxes: Vec<TotalsTax> = rates
            .iter()
            .map(|r| TotalsTax {
                rate: r.rate,
                is_compound: r.is_compound,
            })
            .collect();
        let totals = invoices::compute_totals(currency, &totals_lines, &taxes);
        let (order, order_rows) = orders::create_order(
            &mut *conn,
            tenant_id,
            &NewOrder {
                client_id,
                user_id,
                currency,
                coupon_id: None,
                coupon_code: None,
                payment_integration_id: None,
                tax_total: totals.tax_total,
                ip,
                user_agent: None,
                notes: Some(format!("Quote {}", quote.number())),
                cart_id: None,
                items: order_items,
            },
        )
        .await?;

        // ---- services + invoice lines -----------------------------------------------
        let mut items = Vec::with_capacity(lines.len());
        for (line, order_item) in lines.iter().zip(&order_rows) {
            let (qty, unit, description) = invoice_qty(line, round);
            match (line.product_id, line.billing_cycle) {
                (Some(pid), Some(cycle)) => {
                    let product = catalog::get_product(&mut *conn, tenant_id, pid)
                        .await?
                        .ok_or_else(|| {
                            CoreError::validation(format!(
                                "{} is no longer offered",
                                line.description
                            ))
                        })?;
                    let next_due = cycle.next_due(today);
                    let next_invoice = next_due.and_then(|d| {
                        d.checked_sub_days(Days::new(
                            u64::try_from(settings.invoice_days_before_due).unwrap_or(0),
                        ))
                    });
                    let svc = services::create_service(
                        &mut *conn,
                        tenant_id,
                        &NewService {
                            client_id,
                            product_id: pid,
                            order_id: Some(order.id),
                            billing_cycle: cycle,
                            first_payment_amount: line.amount,
                            recurring_amount: if cycle == BillingCycle::OneTime {
                                Decimal::ZERO
                            } else {
                                line.amount
                            },
                            currency,
                            quantity: line.quantity.trunc().to_i32().unwrap_or(1).max(1),
                            registration_date: today,
                            next_due_date: next_due,
                            next_invoice_date: next_invoice,
                            integration_id: product.integration_id,
                            domain: None,
                            username: None,
                            config_selections: json!({}),
                            custom_fields: json!({}),
                        },
                    )
                    .await?;
                    services::log_event(
                        &mut *conn,
                        tenant_id,
                        svc.id,
                        &NewServiceEvent {
                            to_status: Some(svc.status),
                            ..NewServiceEvent::info(
                                "created",
                                &format!(
                                    "ordered with quote {} (order {})",
                                    quote.number(),
                                    order.order_number
                                ),
                            )
                        },
                    )
                    .await?;
                    orders::link_order_item(
                        &mut *conn,
                        tenant_id,
                        order_item.id,
                        Some(svc.id),
                        None,
                        None,
                    )
                    .await?;
                    items.push(NewInvoiceItem {
                        quantity: qty,
                        unit_price: unit,
                        taxable: line.taxable,
                        order_item_id: Some(order_item.id),
                        ..NewInvoiceItem::service(
                            svc.id,
                            &description,
                            line.amount,
                            today,
                            next_due.and_then(|d| d.pred_opt()),
                        )
                    });
                }
                _ => items.push(NewInvoiceItem {
                    quantity: qty,
                    unit_price: unit,
                    taxable: line.taxable,
                    order_item_id: Some(order_item.id),
                    ..NewInvoiceItem::manual(&description, line.amount)
                }),
            }
        }
        let due = today
            .checked_add_days(Days::new(
                u64::try_from(settings.order_invoice_due_days).unwrap_or(0),
            ))
            .unwrap_or(today);
        let mut invoice = invoices::create_invoice(
            &mut *conn,
            tenant_id,
            &NewInvoice {
                client_id,
                currency,
                issue_date: today,
                due_date: due,
                issue: true,
                payment_integration_id: None,
                notes: Some(format!("{} — {}", quote.number(), quote.subject)),
                billing_snapshot: billing_snapshot(&client),
                items,
                taxes: rates
                    .iter()
                    .map(|r| invoices::TaxLineInput {
                        tax_rule_id: r.tax_rule_id,
                        name: r.name.clone(),
                        rate: r.rate,
                        is_compound: r.is_compound,
                    })
                    .collect(),
            },
        )
        .await?;
        orders::set_order_invoice(&mut *conn, tenant_id, order.id, invoice.id).await?;
        let accepted = qrepo::mark_accepted(
            &mut *conn, tenant_id, id, client_id, order.id, invoice.id, ip, user_id, now,
        )
        .await?
        .ok_or_else(|| CoreError::Conflict("the quote was answered meanwhile".into()))?;

        // ---- settle: credit / zero total ------------------------------------------
        if settings.apply_credit_automatically && invoice.balance > Decimal::ZERO {
            invoice =
                payments::apply_credit_to_invoice(&mut *conn, tenant_id, invoice.id, None, user_id)
                    .await?
                    .0;
        }
        if invoice.status == InvoiceStatus::Unpaid && invoice.balance <= Decimal::ZERO {
            invoices::transition_invoice(
                &mut *conn,
                tenant_id,
                invoice.id,
                &[InvoiceStatus::Unpaid],
                InvoiceStatus::Paid,
            )
            .await?;
            invoice = invoices::get_invoice(&mut *conn, tenant_id, invoice.id)
                .await?
                .ok_or_else(|| CoreError::not_found("invoice", invoice.id))?;
        }
        let paid = invoice.status == InvoiceStatus::Paid;
        self.notify_staff(
            conn,
            tenant_id,
            "quote.accepted",
            &format!(
                "Quote {} accepted (order {})",
                accepted.number(),
                order.order_number
            ),
            Some(&quote.subject),
            Some(&format!("/admin/orders/{}", order.id)),
            &json!({"quote_id": id, "order_id": order.id, "invoice_id": invoice.id}),
        )
        .await?;
        self.emit_event(
            conn,
            tenant_id,
            "quote.accepted",
            json!({"quote_id": id, "number": accepted.number(), "client_id": client_id,
                   "order_id": order.id, "invoice_id": invoice.id, "total": invoice.total,
                   "currency": invoice.currency}),
        )
        .await?;
        if paid {
            self.after_invoice_paid(conn, tenant_id, invoice.id).await?;
        } else {
            self.queue_invoice_created_email(conn, tenant_id, &invoice)
                .await?;
            let order = orders::get_order(&mut *conn, tenant_id, order.id)
                .await?
                .ok_or_else(|| CoreError::not_found("order", order.id))?;
            self.queue_order_provisioning(
                conn,
                tenant_id,
                &order,
                ProvisionTrigger::Checkout,
                false,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(QuoteAcceptance {
            quote_id: id,
            order_id: order.id,
            invoice_id: invoice.id,
            paid,
        })
    }

    /// Expires sent quotes whose validity date has passed (daily automation).
    pub async fn expire_quotes(
        &self,
        tenant_id: TenantId,
        today: NaiveDate,
    ) -> BillingResult<usize> {
        let expired = qrepo::expire_due(self.pool(), tenant_id, today).await?;
        if !expired.is_empty() {
            let mut conn = self.pool().acquire().await?;
            for q in &expired {
                self.emit_event(
                    &mut conn,
                    tenant_id,
                    "quote.expired",
                    json!({"quote_id": q.id, "number": q.number()}),
                )
                .await?;
            }
        }
        Ok(expired.len())
    }

    /// Renders a quote as PDF (the invoice layout with a quote heading).
    pub async fn quote_pdf(
        &self,
        tenant_id: TenantId,
        id: QuoteId,
        locale: Option<Locale>,
    ) -> BillingResult<RenderedPdf> {
        let mut conn = self.pool().acquire().await?;
        let quote = qrepo::get_quote(&mut *conn, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("quote", id))?;
        let lines = qrepo::quote_items(&mut *conn, tenant_id, id).await?;
        let tenant = platform::get_tenant(&mut *conn, tenant_id)
            .await?
            .ok_or_else(|| CoreError::not_found("tenant", tenant_id))?;
        let client = match quote.client_id {
            Some(c) => clients::get_client(&mut *conn, tenant_id, c).await?,
            None => None,
        };
        let locale = locale
            .or_else(|| client.as_ref().map(|c| c.locale))
            .unwrap_or(tenant.default_locale);
        let logo = admin_settings::get_tenant_asset(&mut *conn, tenant_id, "logo")
            .await?
            .filter(|a| a.data.len() <= 2 * 1024 * 1024)
            .and_then(|a| {
                LogoFormat::from_content_type(&a.content_type).map(|format| Logo {
                    data: Arc::new(a.data),
                    format,
                })
            });
        drop(conn);
        let l = for_locale(locale);
        let ql = quote_labels(locale);
        let cur = quote.currency;
        let m = |d: &Decimal| money(*d, cur, locale);
        let number = quote.number();
        let mut meta = vec![
            (
                l.issued.to_owned(),
                date(quote.created_at.date_naive(), locale),
            ),
            (ql.valid_until.to_owned(), date(quote.valid_until, locale)),
        ];
        meta.push((ql.subject.to_owned(), clean(&quote.subject, 120)));
        let legal_name = tenant
            .legal_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| tenant.name.clone());
        let seller: Vec<String> = [
            legal_name.clone(),
            tenant.contact_email.clone().unwrap_or_default(),
            tenant.contact_phone.clone().unwrap_or_default(),
        ]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| clean(&s, MAX_FIELD_CHARS))
        .collect();
        let buyer: Vec<String> = match &client {
            Some(c) => [
                c.company_name.clone().unwrap_or_default(),
                format!("{} {}", c.first_name, c.last_name),
                c.email.clone(),
            ]
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|s| clean(&s, MAX_FIELD_CHARS))
            .collect(),
            None => [
                quote.prospect_company.clone().unwrap_or_default(),
                quote.prospect_name.clone().unwrap_or_default(),
                quote.prospect_email.clone().unwrap_or_default(),
            ]
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|s| clean(&s, MAX_FIELD_CHARS))
            .collect(),
        };
        let pdf_lines: Vec<PdfLine> = lines
            .iter()
            .map(|i| PdfLine {
                description: clean(&i.description, MAX_FIELD_CHARS),
                detail: if i.discount_percent.is_zero() {
                    String::new()
                } else {
                    format!("{} −{}", l.discount, percent(i.discount_percent))
                },
                quantity: quantity(i.quantity, locale),
                unit_price: m(&i.unit_price),
                amount: m(&i.amount),
            })
            .collect();
        let mut totals = vec![PdfTotal {
            label: l.subtotal.to_owned(),
            value: m(&quote.subtotal),
            emphasis: false,
        }];
        if let Some(arr) = quote.tax_lines.as_array() {
            for t in arr {
                let name = t.get("name").and_then(Value::as_str).unwrap_or("");
                let rate = t
                    .get("rate")
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .or_else(|| v.as_f64().map(|f| f.to_string()))
                    })
                    .and_then(|s| s.parse::<Decimal>().ok())
                    .unwrap_or_default();
                let amount = t
                    .get("tax_amount")
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .or_else(|| v.as_f64().map(|f| f.to_string()))
                    })
                    .and_then(|s| s.parse::<Decimal>().ok())
                    .unwrap_or_default();
                totals.push(PdfTotal {
                    label: clean(&format!("{name} ({})", percent(rate)), 80),
                    value: m(&amount),
                    emphasis: false,
                });
            }
        }
        totals.push(PdfTotal {
            label: l.total.to_owned(),
            value: m(&quote.total),
            emphasis: true,
        });
        let stamp = match quote.status {
            QuoteStatus::Accepted => Some((ql.accepted, StampTone::Success)),
            QuoteStatus::Declined => Some((ql.declined, StampTone::Danger)),
            QuoteStatus::Expired => Some((ql.expired, StampTone::Neutral)),
            QuoteStatus::Cancelled => Some((ql.cancelled, StampTone::Neutral)),
            QuoteStatus::Draft => Some((l.stamp_draft, StampTone::Neutral)),
            QuoteStatus::Sent => None,
        }
        .map(|(text, tone)| Stamp {
            text: text.to_owned(),
            tone,
        });
        let doc = InvoicePdf {
            lang: locale.as_str().to_owned(),
            heading: l.quote.to_owned(),
            number: clean(&number, 64),
            brand: clean(&tenant.name, 120),
            logo,
            meta,
            seller_heading: l.from.to_owned(),
            seller,
            buyer_heading: ql.prepared_for.to_owned(),
            buyer,
            legal: Vec::new(),
            columns: [
                l.item.to_owned(),
                l.quantity.to_owned(),
                l.unit_price.to_owned(),
                l.amount.to_owned(),
            ],
            lines: pdf_lines,
            totals,
            stamp,
            notes_heading: l.notes.to_owned(),
            notes: quote
                .notes
                .as_deref()
                .map(|n| paragraphs(n, MAX_NOTE_CHARS))
                .unwrap_or_default(),
            bank_heading: String::new(),
            bank: Vec::new(),
            disclaimer: ql.disclaimer.to_owned(),
            footer: clean(&legal_name, 120),
            page_label: l.page.to_owned(),
            continued_label: l.continued.to_owned(),
            title: clean(&format!("{} {number}", l.quote), 120),
        };
        let data = tokio::task::spawn_blocking(move || hos_pdf::render_invoice(&doc))
            .await
            .map_err(|e| CoreError::Internal(format!("PDF task: {e}")))?
            .map_err(|e| CoreError::Internal(format!("PDF rendering failed: {e}")))?;
        let word = match locale {
            Locale::Tr => "teklif",
            Locale::En => "quote",
        };
        Ok(RenderedPdf {
            filename: format!("{word}-Q{:05}.pdf", quote.seq),
            data,
        })
    }
}

/// The order line of a quote line (amounts already rounded).
fn order_item(l: &QuoteItemRow, round: impl Fn(Decimal) -> Decimal) -> NewOrderItem {
    let whole = l.quantity.fract().is_zero() && l.quantity >= Decimal::ONE;
    let (qty, unit) = if whole {
        (
            l.quantity.trunc().to_i32().unwrap_or(1).max(1),
            l.unit_price,
        )
    } else {
        (1, round(l.quantity * l.unit_price))
    };
    let gross = Decimal::from(qty) * unit;
    let mut item = match (l.product_id, l.billing_cycle) {
        (Some(pid), Some(cycle)) => NewOrderItem::product(pid, cycle, &l.description, unit),
        _ => NewOrderItem {
            item_type: LineItemType::Other,
            product_id: None,
            billing_cycle: None,
            ..NewOrderItem::product(
                ProductId::new(),
                BillingCycle::OneTime,
                &l.description,
                unit,
            )
        },
    };
    item.quantity = qty;
    item.discount = (gross - l.amount).max(Decimal::ZERO);
    item
}

/// Quantity / unit price / description of the invoice line: the quote's
/// quantity and price when undiscounted, else one line of the net amount.
fn invoice_qty(l: &QuoteItemRow, round: impl Fn(Decimal) -> Decimal) -> (Decimal, Decimal, String) {
    if l.discount_percent.is_zero() && round(l.quantity * l.unit_price) == l.amount {
        (l.quantity, l.unit_price, l.description.clone())
    } else {
        let qty = if l.quantity == Decimal::ONE {
            String::new()
        } else {
            format!("{} × ", l.quantity.normalize())
        };
        (
            Decimal::ONE,
            l.amount,
            format!(
                "{qty}{} (−{}%)",
                l.description,
                l.discount_percent.normalize()
            ),
        )
    }
}

struct QuoteLabels {
    valid_until: &'static str,
    subject: &'static str,
    prepared_for: &'static str,
    accepted: &'static str,
    declined: &'static str,
    expired: &'static str,
    cancelled: &'static str,
    disclaimer: &'static str,
}

fn quote_labels(locale: Locale) -> QuoteLabels {
    match locale {
        Locale::Tr => QuoteLabels {
            valid_until: "Geçerlilik",
            subject: "Konu",
            prepared_for: "Hazırlanan",
            accepted: "KABUL EDİLDİ",
            declined: "REDDEDİLDİ",
            expired: "SÜRESİ DOLDU",
            cancelled: "İPTAL",
            disclaimer: "Bu belge bir fiyat teklifidir, fatura yerine geçmez.",
        },
        Locale::En => QuoteLabels {
            valid_until: "Valid until",
            subject: "Subject",
            prepared_for: "Prepared for",
            accepted: "ACCEPTED",
            declined: "DECLINED",
            expired: "EXPIRED",
            cancelled: "CANCELLED",
            disclaimer: "This document is a quotation, not an invoice.",
        },
    }
}

/// Pragmatic e-mail check (one `@`, a dot in the domain, no spaces).
fn is_email(email: &str) -> bool {
    let e = email.trim();
    if e.len() > 254 || e.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn line(q: Decimal, p: Decimal, d: Decimal) -> QuoteLine {
        QuoteLine {
            product_id: None,
            billing_cycle: None,
            description: "x".into(),
            quantity: q,
            unit_price: p,
            discount_percent: d,
            taxable: true,
        }
    }

    #[test]
    fn line_amounts_round_to_the_currency() {
        assert_eq!(
            line_amount(Currency::TRY, &line(dec!(3), dec!(10), dec!(0))),
            (dec!(30), dec!(0))
        );
        assert_eq!(
            line_amount(Currency::TRY, &line(dec!(1), dec!(99.99), dec!(15))),
            (dec!(84.99), dec!(15.00))
        );
        assert_eq!(
            line_amount(Currency::TRY, &line(dec!(0.5), dec!(0.01), dec!(0))).0,
            dec!(0.01)
        );
        assert_eq!(
            line_amount(Currency::TRY, &line(dec!(2), dec!(50), dec!(100))),
            (dec!(0), dec!(100))
        );
    }

    #[test]
    fn lines_are_validated() {
        assert!(validate_line(&line(dec!(1), dec!(1), dec!(0))).is_ok());
        assert!(validate_line(&line(dec!(0), dec!(1), dec!(0))).is_err());
        assert!(validate_line(&line(dec!(1), dec!(-1), dec!(0))).is_err());
        assert!(validate_line(&line(dec!(1), dec!(1), dec!(101))).is_err());
        let mut p = line(dec!(1.5), dec!(1), dec!(0));
        p.product_id = Some(ProductId::new());
        p.billing_cycle = Some(BillingCycle::Monthly);
        assert!(validate_line(&p).is_err(), "fractional product quantity");
        p.quantity = dec!(2);
        assert!(validate_line(&p).is_ok());
        p.billing_cycle = None;
        assert!(validate_line(&p).is_err(), "product line without cycle");
    }

    #[test]
    fn accept_tokens_are_checked_for_shape() {
        let (t, h) = new_accept_token();
        assert_eq!(accept_token_hash(&t).as_deref(), Some(h.as_str()));
        assert!(accept_token_hash("short").is_none());
        assert!(accept_token_hash(&format!("{}'", &t[..42])).is_none());
    }
}
