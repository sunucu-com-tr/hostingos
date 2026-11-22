//! Checkout: cart → order + services/domains + invoice in **one transaction**,
//! plus order acceptance (automatic or by staff, with a fraud hook).
//!
//! `products.auto_setup` decides when provisioning is queued:
//!
//! | auto_setup | queued when |
//! |---|---|
//! | `on_order` | at checkout, even before payment |
//! | `on_payment` | the order invoice is paid |
//! | `on_accept` | the order is accepted (automatically on payment when `orders.auto_accept`, else by staff) |
//! | `manual` | never automatically — staff calls [`Billing::request_provision`] |
//!
//! Domain registrations / transfers are queued once the order is accepted and
//! its invoice is paid.

use std::collections::HashMap;
use std::net::IpAddr;

use chrono::Days;
use hos_core::CoreError;
use hos_core::domain::{Locale, OrderStatus, ProductType};
use hos_core::ids::{
    CartId, ClientId, DomainId, IntegrationId, OrderId, ServiceAddonId, ServiceId, TenantId, UserId,
};
use hos_core::money::BillingCycle;
use hos_db::enums::{
    AutoSetup, ClientStatus, DomainRegistrationType, InvoiceItemType, LineItemType, PayType,
};
use hos_db::repo::catalog::{self, ProductRow};
use hos_db::repo::clients::ClientRow;
use hos_db::repo::domains::{self, NewDomain};
use hos_db::repo::invoices::{self, InvoiceRow, NewInvoice, NewInvoiceItem};
use hos_db::repo::orders::{self, NewOrder, NewOrderItem, OrderRow};
use hos_db::repo::services::{self, NewService, NewServiceEvent};
use hos_db::repo::{billing_support, carts, clients, payments};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::jobs::{self, JobKind};
use crate::pricing::{Quote, QuoteLine, cart_request};
use crate::settings::BillingSettings;
use crate::{Billing, BillingResult};

/// Input of [`Billing::checkout`].
#[derive(Debug, Clone)]
pub struct CheckoutRequest {
    pub cart_id: CartId,
    /// The paying client (must own the cart when the cart has a client).
    pub client_id: ClientId,
    pub user_id: Option<UserId>,
    /// Gateway chosen at checkout (stored on order and invoice).
    pub payment_integration_id: Option<IntegrationId>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub notes: Option<String>,
}

/// Result of [`Billing::checkout`].
#[derive(Debug, Clone, Serialize)]
pub struct CheckoutOutcome {
    pub order_id: OrderId,
    pub order_number: String,
    pub invoice_id: hos_core::ids::InvoiceId,
    pub invoice_number: Option<String>,
    pub quote: Quote,
    pub services: Vec<ServiceId>,
    pub domains: Vec<DomainId>,
    /// Client credit used on the invoice.
    pub credit_applied: Decimal,
    /// The invoice is already settled (zero total or credit): provisioning is queued.
    pub paid: bool,
    /// Remaining balance to pay.
    pub balance: Decimal,
}

/// Snapshot of the client's billing identity stored on the invoice.
pub fn billing_snapshot(c: &ClientRow) -> Value {
    json!({
        "client_id": c.id,
        "kind": c.kind,
        "name": format!("{} {}", c.first_name, c.last_name).trim(),
        "company_name": c.company_name,
        "email": c.email,
        "phone": c.phone,
        "address_line1": c.address_line1,
        "address_line2": c.address_line2,
        "city": c.city,
        "state": c.state,
        "postal_code": c.postal_code,
        "country_code": c.country_code,
        "tax_office": c.tax_office,
        "tax_number": c.tax_number,
        "national_id": c.national_id,
    })
}

fn discount_label(locale: Locale, code: Option<&str>) -> String {
    let word = match locale {
        Locale::Tr => "İndirim",
        Locale::En => "Discount",
    };
    match code {
        Some(c) => format!("{word} ({c})"),
        None => word.to_owned(),
    }
}

fn setup_label(locale: Locale, what: &str) -> String {
    match locale {
        Locale::Tr => format!("Kurulum ücreti - {what}"),
        Locale::En => format!("Setup fee - {what}"),
    }
}

/// Provisioning values of a line's configurable options, keyed by
/// `config_options.module_key`: the selected value, or the quantity for
/// quantity options without a value.
pub fn module_options(line: &QuoteLine) -> Value {
    let mut map = serde_json::Map::new();
    for o in &line.config_options {
        let Some(key) = o.module_key.as_deref().filter(|k| !k.trim().is_empty()) else {
            continue;
        };
        let v = if o.value.trim().is_empty() {
            json!(o.quantity)
        } else if o.quantity > 1 {
            match o.value.trim().parse::<i64>() {
                Ok(n) => json!(n * i64::from(o.quantity)),
                Err(_) => json!(o.value),
            }
        } else {
            json!(o.value)
        };
        map.insert(key.to_owned(), v);
    }
    Value::Object(map)
}

/// What a quote line created.
#[derive(Debug, Clone, Copy, Default)]
struct Created {
    service_id: Option<ServiceId>,
    service_addon_id: Option<ServiceAddonId>,
    domain_id: Option<DomainId>,
    order_item_id: Option<hos_core::ids::OrderItemId>,
}

/// Invoice lines of a quote line (base, setup fee, discount).
fn invoice_lines(
    line: &QuoteLine,
    created: Created,
    today: chrono::NaiveDate,
    locale: Locale,
    coupon_code: Option<&str>,
) -> Vec<NewInvoiceItem> {
    let item_type = match line.item_type {
        LineItemType::Product => InvoiceItemType::Service,
        LineItemType::Addon => InvoiceItemType::Addon,
        LineItemType::DomainRegister | LineItemType::DomainTransfer | LineItemType::DomainRenew => {
            InvoiceItemType::Domain
        }
        LineItemType::Other => InvoiceItemType::Manual,
    };
    let period_end = match (line.billing_cycle, line.years) {
        (Some(c), _) => c.period_end(today),
        (None, Some(y)) => u32::try_from(y)
            .ok()
            .and_then(|y| today.checked_add_months(chrono::Months::new(y * 12)))
            .and_then(|d| d.checked_sub_days(Days::new(1))),
        _ => None,
    };
    let base = NewInvoiceItem {
        item_type,
        service_id: created.service_id,
        service_addon_id: created.service_addon_id,
        domain_id: created.domain_id,
        order_item_id: created.order_item_id,
        description: line.description.clone(),
        quantity: Decimal::from(line.quantity),
        unit_price: line.unit_price,
        taxable: line.taxable,
        period_start: Some(today),
        period_end,
    };
    let mut out = vec![base.clone()];
    if !line.setup_fee.is_zero() {
        out.push(NewInvoiceItem {
            item_type: InvoiceItemType::SetupFee,
            description: setup_label(locale, &line.description),
            quantity: Decimal::ONE,
            unit_price: line.setup_fee,
            period_start: None,
            period_end: None,
            ..base.clone()
        });
    }
    if !line.discount.is_zero() {
        out.push(NewInvoiceItem {
            item_type: InvoiceItemType::Discount,
            description: format!(
                "{} - {}",
                discount_label(locale, coupon_code),
                line.description
            ),
            quantity: Decimal::ONE,
            unit_price: -line.discount,
            period_start: None,
            period_end: None,
            ..base
        });
    }
    out
}

/// Which auto-setup modes to provision now.
fn provision_modes(trigger: ProvisionTrigger, auto_accepted: bool) -> &'static [AutoSetup] {
    match (trigger, auto_accepted) {
        (ProvisionTrigger::Checkout, _) => &[AutoSetup::OnOrder],
        (ProvisionTrigger::Payment, true) => &[
            AutoSetup::OnOrder,
            AutoSetup::OnPayment,
            AutoSetup::OnAccept,
        ],
        (ProvisionTrigger::Payment, false) => &[AutoSetup::OnOrder, AutoSetup::OnPayment],
        (ProvisionTrigger::Accept, _) => &[AutoSetup::OnAccept],
    }
}

/// What caused provisioning to be considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProvisionTrigger {
    Checkout,
    Payment,
    Accept,
}

impl Billing {
    /// Turns a cart into an order, its services / domains and an issued
    /// invoice, atomically. Applies client credit (setting), settles zero
    /// invoices immediately and queues provisioning / notification jobs in the
    /// same transaction.
    pub async fn checkout(
        &self,
        tenant_id: TenantId,
        req: &CheckoutRequest,
    ) -> BillingResult<CheckoutOutcome> {
        let mut tx = self.pool().begin().await?;
        let out = self.checkout_in(&mut tx, tenant_id, req).await?;
        tx.commit().await?;
        Ok(out)
    }

    async fn checkout_in(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        req: &CheckoutRequest,
    ) -> BillingResult<CheckoutOutcome> {
        let today = self.clock().today();
        let cart = billing_support::lock_cart(&mut *conn, tenant_id, req.cart_id)
            .await?
            .ok_or_else(|| CoreError::not_found("cart", req.cart_id))?;
        if cart.converted_order_id.is_some() {
            return Err(CoreError::Conflict(
                "this cart was already checked out".into(),
            ));
        }
        if cart.expires_at < self.clock().now() {
            return Err(CoreError::validation("the cart has expired"));
        }
        if cart.client_id.is_some_and(|c| c != req.client_id) {
            return Err(CoreError::Forbidden(
                "the cart belongs to another client".into(),
            ));
        }
        let client = clients::get_client(&mut *conn, tenant_id, req.client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", req.client_id))?;
        if client.status != ClientStatus::Active {
            return Err(CoreError::Forbidden(
                "the client account is not active".into(),
            ));
        }
        let cart_items = carts::cart_items(&mut *conn, tenant_id, req.cart_id).await?;
        if cart_items.is_empty() {
            return Err(CoreError::validation("the cart is empty"));
        }
        let settings = BillingSettings::load(&mut *conn, tenant_id).await?;
        let qreq = cart_request(conn, tenant_id, req.cart_id, Some(req.client_id), None).await?;
        let quote = self.quote_on(conn, tenant_id, &qreq).await?;
        if let Some(e) = &quote.coupon_error {
            return Err(CoreError::validation(e.clone()));
        }
        if quote.lines.len() != cart_items.len() {
            return Err(CoreError::Internal("quote does not match the cart".into()));
        }
        // quote line i ↔ cart item source[i] (the parent-first order of the request)
        let source = crate::pricing::line_order(&qreq)?;

        // ---- product rules: stock, one-per-client, required domains --------
        let mut products: HashMap<hos_core::ids::ProductId, ProductRow> = HashMap::new();
        for line in quote
            .lines
            .iter()
            .filter(|l| l.item_type == LineItemType::Product)
        {
            let Some(pid) = line.product_id else { continue };
            let product = match products.get(&pid) {
                Some(p) => p.clone(),
                None => {
                    let p = catalog::get_product(&mut *conn, tenant_id, pid)
                        .await?
                        .ok_or_else(|| CoreError::not_found("product", pid))?;
                    products.insert(pid, p.clone());
                    p
                }
            };
            // An OS template chosen through configurable options must be one
            // the product offers (checked again at provisioning time).
            if !line.config_options.is_empty() {
                crate::provisioning::check_option_templates(
                    &mut *conn,
                    tenant_id,
                    &product,
                    &module_options(line),
                )
                .await?;
            }
            if product.stock_control
                && !catalog::take_stock(&mut *conn, tenant_id, pid, line.quantity).await?
            {
                return Err(CoreError::Conflict(format!(
                    "{} is out of stock",
                    product.name
                )));
            }
            let once = product.pay_type == PayType::Free
                || product
                    .module_config
                    .get("once_per_client")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            if once
                && billing_support::client_has_product(&mut *conn, tenant_id, client.id, pid)
                    .await?
            {
                return Err(CoreError::Conflict(format!(
                    "{} can only be ordered once per client",
                    product.name
                )));
            }
        }

        // ---- coupon usage (atomic limit check; serialises on the coupon row) --
        if let Some(coupon_id) = quote.coupon.as_ref().and_then(|c| c.id)
            && !catalog::increment_coupon_use(&mut *conn, tenant_id, coupon_id).await?
        {
            return Err(CoreError::Conflict(
                "the coupon has reached its usage limit".into(),
            ));
        }

        // ---- order -----------------------------------------------------------
        let order_items: Vec<NewOrderItem> = quote
            .lines
            .iter()
            .zip(&source)
            .map(|(l, src)| {
                let ci = &cart_items[*src];
                NewOrderItem {
                    parent_index: l.parent,
                    item_type: l.item_type,
                    product_id: l.product_id,
                    addon_id: l.addon_id,
                    domain_name: l.domain_name.clone().or_else(|| ci.domain_name.clone()),
                    billing_cycle: l.billing_cycle,
                    years: l.years,
                    quantity: l.quantity,
                    description: l.description.clone(),
                    unit_price: l.unit_price,
                    setup_fee: l.setup_fee,
                    discount: l.discount,
                    config_selections: ci.config_selections.clone(),
                    custom_fields: ci.custom_fields.clone(),
                    hostname: ci.hostname.clone(),
                    epp_code_enc: ci.epp_code_enc.clone(),
                }
            })
            .collect();
        let (order, order_rows) = orders::create_order(
            &mut *conn,
            tenant_id,
            &NewOrder {
                client_id: client.id,
                user_id: req.user_id,
                currency: quote.currency,
                coupon_id: quote.coupon.as_ref().and_then(|c| c.id),
                coupon_code: quote.coupon.as_ref().map(|c| c.code.clone()),
                payment_integration_id: req.payment_integration_id,
                tax_total: quote.tax_total,
                ip: req.ip,
                user_agent: req.user_agent.clone(),
                notes: req.notes.clone(),
                cart_id: Some(req.cart_id),
                items: order_items,
            },
        )
        .await?;

        // ---- services, addons, domains --------------------------------------
        let default_registrar = self
            .registry()
            .default_id(tenant_id, hos_core::domain::IntegrationCategory::Registrar)
            .await?;
        let mut created: Vec<Created> = vec![Created::default(); quote.lines.len()];
        let mut service_ids = Vec::new();
        let mut domain_ids = Vec::new();
        for (i, line) in quote.lines.iter().enumerate() {
            let order_item = &order_rows[i];
            created[i].order_item_id = Some(order_item.id);
            match line.item_type {
                LineItemType::Product => {
                    let pid = line.product_id.ok_or_else(|| {
                        CoreError::Internal("product line without product".into())
                    })?;
                    let product = products
                        .get(&pid)
                        .cloned()
                        .ok_or_else(|| CoreError::not_found("product", pid))?;
                    let cycle = line.billing_cycle.unwrap_or(BillingCycle::OneTime);
                    // Hosting domain: the line's own domain, else a bundled domain line.
                    let domain = order_item.domain_name.clone().or_else(|| {
                        quote
                            .lines
                            .iter()
                            .find(|l| l.parent == Some(i) && l.domain_name.is_some())
                            .and_then(|l| l.domain_name.clone())
                    });
                    if product.requires_domain == hos_db::enums::DomainRequirement::Required
                        && domain.is_none()
                    {
                        return Err(CoreError::validation(format!(
                            "{} requires a domain name",
                            product.name
                        )));
                    }
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
                            client_id: client.id,
                            product_id: pid,
                            order_id: Some(order.id),
                            billing_cycle: cycle,
                            first_payment_amount: line.amount,
                            recurring_amount: line.recurring_amount,
                            currency: quote.currency,
                            quantity: line.quantity,
                            registration_date: today,
                            next_due_date: next_due,
                            next_invoice_date: next_invoice,
                            integration_id: product.integration_id,
                            domain: domain.clone().or_else(|| order_item.hostname.clone()),
                            username: None,
                            config_selections: order_item.config_selections.clone(),
                            custom_fields: order_item.custom_fields.clone(),
                        },
                    )
                    .await?;
                    if !line.config_options.is_empty() {
                        services::update_provisioning(
                            &mut *conn,
                            tenant_id,
                            svc.id,
                            &services::ProvisioningUpdate {
                                module_data: Some(json!({ "options": module_options(line) })),
                                ..Default::default()
                            },
                        )
                        .await?;
                    }
                    if let Some(c) = quote
                        .coupon
                        .as_ref()
                        .filter(|c| c.recurring && line.coupon_applied)
                        && c.recurring_limit > 0
                        && line.recurring_amount != line.list_recurring_amount
                    {
                        billing_support::set_service_promo(
                            &mut *conn,
                            tenant_id,
                            svc.id,
                            c.id,
                            Some(c.recurring_limit),
                            Some(line.list_recurring_amount),
                        )
                        .await?;
                    }
                    services::log_event(
                        &mut *conn,
                        tenant_id,
                        svc.id,
                        &NewServiceEvent {
                            to_status: Some(svc.status),
                            ..NewServiceEvent::info(
                                "created",
                                &format!("ordered with order {}", order.order_number),
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
                    created[i].service_id = Some(svc.id);
                    service_ids.push(svc.id);
                }
                LineItemType::Addon => {
                    let parent_service = line
                        .parent
                        .and_then(|p| created.get(p))
                        .and_then(|c| c.service_id)
                        .ok_or_else(|| {
                            CoreError::validation("an addon must belong to a product")
                        })?;
                    let addon_id = line
                        .addon_id
                        .ok_or_else(|| CoreError::Internal("addon line without addon".into()))?;
                    let cycle = line.billing_cycle.unwrap_or(BillingCycle::Monthly);
                    let sa = services::create_service_addon(
                        &mut *conn,
                        tenant_id,
                        parent_service,
                        addon_id,
                        Some(order.id),
                        cycle,
                        line.setup_fee,
                        line.recurring_amount,
                        quote.currency,
                        cycle.next_due(today),
                    )
                    .await?;
                    orders::link_order_item(
                        &mut *conn,
                        tenant_id,
                        order_item.id,
                        None,
                        Some(sa.id),
                        None,
                    )
                    .await?;
                    created[i].service_id = Some(parent_service);
                    created[i].service_addon_id = Some(sa.id);
                }
                LineItemType::DomainRegister | LineItemType::DomainTransfer => {
                    let name = line
                        .domain_name
                        .clone()
                        .ok_or_else(|| CoreError::Internal("domain line without name".into()))?;
                    let tld = domains::match_tld(&mut *conn, tenant_id, &name)
                        .await?
                        .ok_or_else(|| CoreError::validation(format!("{name} is not offered")))?;
                    let d = domains::create_domain(
                        &mut *conn,
                        tenant_id,
                        &NewDomain {
                            client_id: client.id,
                            order_id: Some(order.id),
                            name: name.clone(),
                            tld: tld.tld.clone(),
                            registrar_integration_id: tld
                                .registrar_integration_id
                                .or(default_registrar),
                            registration_type: if line.item_type == LineItemType::DomainTransfer {
                                DomainRegistrationType::Transfer
                            } else {
                                DomainRegistrationType::Register
                            },
                            registration_period: line.years.unwrap_or(1),
                            first_payment_amount: line.amount,
                            recurring_amount: line.recurring_amount,
                            currency: quote.currency,
                            privacy: order_item
                                .custom_fields
                                .get("privacy")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            nameservers: settings.default_nameservers.clone(),
                            epp_code_enc: order_item.epp_code_enc.clone(),
                        },
                    )
                    .await?;
                    orders::link_order_item(
                        &mut *conn,
                        tenant_id,
                        order_item.id,
                        None,
                        None,
                        Some(d.id),
                    )
                    .await?;
                    created[i].domain_id = Some(d.id);
                    domain_ids.push(d.id);
                }
                LineItemType::DomainRenew => {
                    let name = line.domain_name.clone().unwrap_or_default();
                    let d = domains::domain_by_name(&mut *conn, tenant_id, &name)
                        .await?
                        .filter(|d| d.client_id == client.id)
                        .ok_or_else(|| {
                            CoreError::validation(format!("{name} is not a domain of this account"))
                        })?;
                    orders::link_order_item(
                        &mut *conn,
                        tenant_id,
                        order_item.id,
                        None,
                        None,
                        Some(d.id),
                    )
                    .await?;
                    created[i].domain_id = Some(d.id);
                }
                LineItemType::Other => {}
            }
        }

        // ---- invoice -----------------------------------------------------------
        let coupon_code = quote.coupon.as_ref().map(|c| c.code.as_str());
        let mut items = Vec::new();
        for (i, line) in quote.lines.iter().enumerate() {
            items.extend(invoice_lines(
                line,
                created[i],
                today,
                client.locale,
                coupon_code,
            ));
        }
        let due = today
            .checked_add_days(Days::new(
                u64::try_from(settings.order_invoice_due_days).unwrap_or(0),
            ))
            .unwrap_or(today);
        let invoice = invoices::create_invoice(
            &mut *conn,
            tenant_id,
            &NewInvoice {
                client_id: client.id,
                currency: quote.currency,
                issue_date: today,
                due_date: due,
                issue: true,
                payment_integration_id: req.payment_integration_id,
                notes: None,
                billing_snapshot: billing_snapshot(&client),
                items,
                taxes: quote.tax_inputs(),
            },
        )
        .await?;
        if invoice.total != quote.total {
            tracing::warn!(%tenant_id, invoice = %invoice.id, invoice_total = %invoice.total,
                quote_total = %quote.total, "invoice total differs from the quote");
        }
        orders::set_order_invoice(&mut *conn, tenant_id, order.id, invoice.id).await?;

        // ---- settle: credit / zero total -------------------------------------
        let mut invoice = invoice;
        let mut credit_applied = Decimal::ZERO;
        if settings.apply_credit_automatically && invoice.balance > Decimal::ZERO {
            let (inv, applied) = payments::apply_credit_to_invoice(
                &mut *conn,
                tenant_id,
                invoice.id,
                None,
                req.user_id,
            )
            .await?;
            invoice = inv;
            credit_applied = applied;
        }
        if invoice.status == hos_core::domain::InvoiceStatus::Unpaid
            && invoice.balance <= Decimal::ZERO
        {
            invoices::transition_invoice(
                &mut *conn,
                tenant_id,
                invoice.id,
                &[hos_core::domain::InvoiceStatus::Unpaid],
                hos_core::domain::InvoiceStatus::Paid,
            )
            .await?;
            invoice = invoices::get_invoice(&mut *conn, tenant_id, invoice.id)
                .await?
                .ok_or_else(|| CoreError::not_found("invoice", invoice.id))?;
        }
        let paid = invoice.status == hos_core::domain::InvoiceStatus::Paid;

        // ---- notifications & provisioning -------------------------------------
        let url = self.tenant_url(tenant_id).await?;
        let summary = quote
            .lines
            .iter()
            .map(|l| format!("{} × {}", l.quantity, l.description))
            .collect::<Vec<_>>()
            .join(", ");
        self.queue_client_email(
            conn,
            tenant_id,
            client.id,
            hos_mail::defaults::keys::ORDER_CONFIRMATION,
            json!({"order": {
                "number": order.order_number,
                "total": crate::format::money(&quote.total_money(), client.locale),
                "summary": summary,
                "url": format!("{url}/client/orders/{}", order.id),
            }}),
            Some(("order", *order.id.as_uuid())),
        )
        .await?;
        self.emit_event(
            conn,
            tenant_id,
            "order.created",
            json!({"order_id": order.id, "order_number": order.order_number, "client_id": client.id,
                   "invoice_id": invoice.id, "total": quote.total, "currency": quote.currency}),
        )
        .await?;
        self.notify_staff(
            conn,
            tenant_id,
            "order.new",
            &format!(
                "New order {} ({})",
                order.order_number,
                crate::format::money(&quote.total_money(), client.locale)
            ),
            Some(&summary),
            Some(&format!("/admin/orders/{}", order.id)),
            &json!({"order_id": order.id, "client_id": client.id}),
        )
        .await?;
        if paid {
            self.after_invoice_paid(conn, tenant_id, invoice.id).await?;
        } else {
            self.queue_invoice_created_email(conn, tenant_id, &invoice)
                .await?;
            self.queue_order_provisioning(
                conn,
                tenant_id,
                &order,
                ProvisionTrigger::Checkout,
                false,
            )
            .await?;
        }

        Ok(CheckoutOutcome {
            order_id: order.id,
            order_number: order.order_number,
            invoice_id: invoice.id,
            invoice_number: invoice.number.clone(),
            quote,
            services: service_ids,
            domains: domain_ids,
            credit_applied,
            paid,
            balance: invoice.balance,
        })
    }

    /// Queues the `invoice_created` e-mail of an issued invoice.
    pub(crate) async fn queue_invoice_created_email(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
    ) -> BillingResult<()> {
        let client = clients::get_client(&mut *conn, tenant_id, invoice.client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", invoice.client_id))?;
        let url = self.tenant_url(tenant_id).await?;
        self.queue_client_email(
            conn,
            tenant_id,
            invoice.client_id,
            hos_mail::defaults::keys::INVOICE_CREATED,
            json!({"invoice": {
                "number": invoice.number.clone().unwrap_or_default(),
                "total": crate::format::money(&invoice.total_money(), client.locale),
                "due_date": crate::format::date(invoice.due_date, client.locale),
                "url": format!("{url}/client/invoices/{}", invoice.id),
                "notes": invoice.notes.clone().unwrap_or_default(),
            }}),
            Some(("invoice", *invoice.id.as_uuid())),
        )
        .await?;
        Ok(())
    }

    /// Queues provisioning of an order's pending services (by `auto_setup`)
    /// and, once its invoice is paid, its domain registrations / transfers.
    pub(crate) async fn queue_order_provisioning(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        order: &OrderRow,
        trigger: ProvisionTrigger,
        auto_accepted: bool,
    ) -> BillingResult<usize> {
        if matches!(order.status, OrderStatus::Fraud | OrderStatus::Cancelled) {
            return Ok(0);
        }
        let modes = provision_modes(trigger, auto_accepted);
        let mut queued = 0;
        for svc in billing_support::order_services(&mut *conn, tenant_id, order.id).await? {
            if svc.status != hos_core::domain::ServiceStatus::Pending {
                continue;
            }
            let Some(product) = catalog::get_product(&mut *conn, tenant_id, svc.product_id).await?
            else {
                continue;
            };
            if modes.contains(&product.auto_setup) {
                let res = jobs::enqueue_provision(
                    conn,
                    tenant_id,
                    svc.id,
                    Some(format!("provision:{}", svc.id)),
                )
                .await?;
                queued += usize::from(res.is_created());
            }
        }
        let invoice_paid = match order.invoice_id {
            Some(id) => invoices::get_invoice(&mut *conn, tenant_id, id)
                .await?
                .is_some_and(|i| i.status == hos_core::domain::InvoiceStatus::Paid),
            None => false,
        };
        let accepted = order.status == OrderStatus::Accepted || auto_accepted;
        if invoice_paid && accepted {
            for d in billing_support::order_domains(&mut *conn, tenant_id, order.id).await? {
                if d.status != hos_core::domain::DomainStatus::Pending {
                    continue;
                }
                let kind = match d.registration_type {
                    DomainRegistrationType::Transfer => JobKind::TransferDomain,
                    DomainRegistrationType::Register => JobKind::RegisterDomain,
                };
                let res = jobs::enqueue_domain_job(
                    conn,
                    tenant_id,
                    kind,
                    d.id,
                    None,
                    Some(format!("{kind}:{}", d.id)),
                )
                .await?;
                queued += usize::from(res.is_created());
            }
            self.activate_order_addons(conn, tenant_id, order).await?;
        }
        Ok(queued)
    }

    /// Paid addons of an accepted order: activated when the addon is set to
    /// activate automatically and has no module; otherwise staff are told
    /// (once) to set it up.
    async fn activate_order_addons(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        order: &OrderRow,
    ) -> BillingResult<()> {
        for sa in services::order_service_addons(&mut *conn, tenant_id, order.id).await? {
            if sa.status != hos_core::domain::ServiceStatus::Pending {
                continue;
            }
            let Some(addon) = catalog::get_addon(&mut *conn, tenant_id, sa.addon_id).await? else {
                continue;
            };
            if addon.auto_activate && addon.integration_id.is_none() {
                services::set_service_addon_status(
                    &mut *conn,
                    tenant_id,
                    sa.id,
                    hos_core::domain::ServiceStatus::Active,
                )
                .await?;
                services::log_event(
                    &mut *conn,
                    tenant_id,
                    sa.service_id,
                    &NewServiceEvent::info(
                        "addon_activated",
                        &format!("{} (order {})", addon.name, order.order_number),
                    ),
                )
                .await?;
            } else if billing_support::mark_once(
                &mut *conn,
                tenant_id,
                &format!("addon_setup:{}", sa.id),
            )
            .await?
            {
                self.notify_staff(
                    conn,
                    tenant_id,
                    "addon.setup",
                    &format!("Addon {} was paid and awaits setup", addon.name),
                    Some(&format!("order {}", order.order_number)),
                    Some(&format!("/admin/services/{}", sa.service_id)),
                    &json!({"service_id": sa.service_id, "service_addon_id": sa.id}),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Accepts a pending order inside a transaction (optionally running the
    /// fraud hook). Returns `true` when the order is accepted.
    pub(crate) async fn accept_order_in(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        order: &OrderRow,
        run_fraud_check: bool,
    ) -> BillingResult<bool> {
        if order.status == OrderStatus::Accepted {
            return Ok(true);
        }
        if order.status != OrderStatus::Pending {
            return Ok(false);
        }
        if run_fraud_check {
            let client = clients::get_client(&mut *conn, tenant_id, order.client_id)
                .await?
                .ok_or_else(|| CoreError::not_found("client", order.client_id))?;
            // The pluggable hook plus the built-in rules (crate::fraud).
            let verdict = self
                .combined_fraud_verdict(conn, tenant_id, order, &client)
                .await?;
            orders::set_fraud_result(
                &mut *conn,
                tenant_id,
                order.id,
                verdict.score,
                &verdict.details,
            )
            .await?;
            if verdict.flagged {
                orders::transition_order(
                    &mut *conn,
                    tenant_id,
                    order.id,
                    OrderStatus::Pending,
                    OrderStatus::Fraud,
                )
                .await?;
                self.notify_staff(
                    conn,
                    tenant_id,
                    "order.fraud",
                    &format!("Order {} was flagged as fraud", order.order_number),
                    None,
                    Some(&format!("/admin/orders/{}", order.id)),
                    &json!({"order_id": order.id, "score": verdict.score}),
                )
                .await?;
                return Ok(false);
            }
        }
        let moved = orders::transition_order(
            &mut *conn,
            tenant_id,
            order.id,
            OrderStatus::Pending,
            OrderStatus::Accepted,
        )
        .await?;
        if moved {
            self.emit_event(
                conn,
                tenant_id,
                "order.accepted",
                json!({"order_id": order.id, "order_number": order.order_number}),
            )
            .await?;
        }
        Ok(moved)
    }

    /// Staff acceptance of a pending (or fraud-flagged) order: provisions
    /// `on_accept` products, plus everything paid-for when the invoice is paid.
    pub async fn accept_order(
        &self,
        tenant_id: TenantId,
        order_id: OrderId,
    ) -> BillingResult<OrderRow> {
        let mut tx = self.pool().begin().await?;
        let mut order = orders::get_order(&mut *tx, tenant_id, order_id)
            .await?
            .ok_or_else(|| CoreError::not_found("order", order_id))?;
        if order.status == OrderStatus::Fraud {
            orders::transition_order(
                &mut *tx,
                tenant_id,
                order_id,
                OrderStatus::Fraud,
                OrderStatus::Pending,
            )
            .await?;
            order.status = OrderStatus::Pending;
        }
        if !self
            .accept_order_in(&mut tx, tenant_id, &order, false)
            .await?
        {
            return Err(CoreError::Conflict(format!(
                "order {} cannot be accepted while {}",
                order.order_number, order.status
            )));
        }
        let order = orders::get_order(&mut *tx, tenant_id, order_id)
            .await?
            .ok_or_else(|| CoreError::not_found("order", order_id))?;
        self.queue_order_provisioning(&mut tx, tenant_id, &order, ProvisionTrigger::Accept, false)
            .await?;
        let paid = match order.invoice_id {
            Some(id) => invoices::get_invoice(&mut *tx, tenant_id, id)
                .await?
                .is_some_and(|i| i.status == hos_core::domain::InvoiceStatus::Paid),
            None => false,
        };
        if paid {
            self.queue_order_provisioning(
                &mut tx,
                tenant_id,
                &order,
                ProvisionTrigger::Payment,
                false,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(order)
    }

    /// Cancels an order (pending / accepted / fraud), its unpaid invoice and
    /// its not-yet-provisioned services and domains.
    pub async fn cancel_order(
        &self,
        tenant_id: TenantId,
        order_id: OrderId,
    ) -> BillingResult<OrderRow> {
        let mut tx = self.pool().begin().await?;
        let order = orders::get_order(&mut *tx, tenant_id, order_id)
            .await?
            .ok_or_else(|| CoreError::not_found("order", order_id))?;
        if order.status == OrderStatus::Cancelled {
            tx.commit().await?;
            return Ok(order);
        }
        orders::transition_order(
            &mut *tx,
            tenant_id,
            order_id,
            order.status,
            OrderStatus::Cancelled,
        )
        .await?;
        if let Some(inv) = order.invoice_id {
            // Applied credit goes back to the client; money paid on the
            // invoice or a payment in flight stops the cancellation.
            match hos_db::repo::payments::cancel_invoice_with_credit(
                &mut tx, tenant_id, inv, false, None,
            )
            .await?
            {
                hos_db::repo::payments::CancelOutcome::Blocked(
                    hos_db::repo::payments::CancelBlock::AmountPaid(_),
                ) => {
                    return Err(CoreError::Conflict(
                        "the order's invoice has payments: refund them or cancel the invoice with the payment moved to credit first".into(),
                    ));
                }
                hos_db::repo::payments::CancelOutcome::Blocked(
                    hos_db::repo::payments::CancelBlock::PaymentInFlight,
                ) => {
                    return Err(CoreError::Conflict(
                        "a payment of the order's invoice is still pending at the gateway".into(),
                    ));
                }
                _ => {}
            }
        }
        for svc in billing_support::order_services(&mut *tx, tenant_id, order_id).await? {
            if matches!(
                svc.status,
                hos_core::domain::ServiceStatus::Pending | hos_core::domain::ServiceStatus::Failed
            ) {
                services::transition_service(
                    &mut *tx,
                    tenant_id,
                    svc.id,
                    svc.status,
                    hos_core::domain::ServiceStatus::Cancelled,
                    None,
                )
                .await?;
            }
        }
        for d in billing_support::order_domains(&mut *tx, tenant_id, order_id).await? {
            if d.status == hos_core::domain::DomainStatus::Pending {
                domains::transition_domain(
                    &mut *tx,
                    tenant_id,
                    d.id,
                    d.status,
                    hos_core::domain::DomainStatus::Cancelled,
                )
                .await?;
            }
        }
        let order = orders::get_order(&mut *tx, tenant_id, order_id)
            .await?
            .ok_or_else(|| CoreError::not_found("order", order_id))?;
        tx.commit().await?;
        Ok(order)
    }
}

/// `true` for product types whose services are provisioned by a module.
pub fn is_module_product(t: ProductType) -> bool {
    t.default_integration_category().is_some()
}

// ---------------------------------------------------------------------------
// addon for an existing service
// ---------------------------------------------------------------------------

/// Outcome of [`Billing::order_service_addon`].
#[derive(Debug, Clone, Serialize)]
pub struct AddonOrder {
    pub order_id: OrderId,
    pub order_number: String,
    pub invoice_id: hos_core::ids::InvoiceId,
    pub service_addon_id: ServiceAddonId,
    pub total: Decimal,
    /// Settled at once (free addon or account credit).
    pub paid: bool,
}

impl Billing {
    /// Orders `addon_id` for an existing active service: order → invoice →
    /// on payment the addon is activated ([`Billing::after_invoice_paid`] →
    /// order acceptance) and from then on renews together with the service.
    /// A recurring addon follows the service's billing cycle; its first
    /// charge covers the days until the service's next due date (plus any
    /// setup fee). Prices come from the catalogue, never from the caller.
    pub async fn order_service_addon(
        &self,
        tenant_id: TenantId,
        service_id: ServiceId,
        client_id: Option<ClientId>,
        addon_id: hos_core::ids::ProductAddonId,
        user_id: Option<UserId>,
        ip: Option<IpAddr>,
    ) -> BillingResult<AddonOrder> {
        let mut tx = self.pool().begin().await?;
        let svc = billing_support::lock_service(&mut tx, tenant_id, service_id)
            .await?
            .ok_or_else(|| CoreError::not_found("service", service_id))?;
        if client_id.is_some_and(|c| c != svc.client_id) {
            return Err(CoreError::not_found("service", service_id));
        }
        if svc.status != hos_core::domain::ServiceStatus::Active {
            return Err(CoreError::validation(
                "addons can only be added to an active service",
            ));
        }
        let addon = catalog::addons_for_product(&mut *tx, tenant_id, svc.product_id, svc.currency)
            .await?
            .into_iter()
            .find(|a| a.addon.id == addon_id)
            .ok_or_else(|| CoreError::not_found("addon", addon_id))?
            .addon;
        for existing in services::service_addons(&mut *tx, tenant_id, service_id).await? {
            if existing.addon_id != addon_id {
                continue;
            }
            match existing.status {
                hos_core::domain::ServiceStatus::Active
                | hos_core::domain::ServiceStatus::Suspended
                | hos_core::domain::ServiceStatus::Provisioning => {
                    return Err(CoreError::Conflict(
                        "this addon is already on the service".into(),
                    ));
                }
                hos_core::domain::ServiceStatus::Pending => {
                    // Still awaiting payment → refuse; its invoice gone → drop it.
                    let open = match existing.order_id {
                        Some(o) => match orders::get_order(&mut *tx, tenant_id, o).await? {
                            Some(order) => match order.invoice_id {
                                Some(i) => invoices::get_invoice(&mut *tx, tenant_id, i)
                                    .await?
                                    .is_some_and(|i| {
                                        i.status.is_open()
                                            || i.status == hos_core::domain::InvoiceStatus::Paid
                                    }),
                                None => false,
                            },
                            None => false,
                        },
                        None => false,
                    };
                    if open {
                        return Err(CoreError::Conflict(
                            "this addon was already ordered and awaits payment".into(),
                        ));
                    }
                    services::set_service_addon_status(
                        &mut *tx,
                        tenant_id,
                        existing.id,
                        hos_core::domain::ServiceStatus::Cancelled,
                    )
                    .await?;
                }
                _ => {}
            }
        }
        let client = clients::get_client(&mut *tx, tenant_id, svc.client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", svc.client_id))?;
        if client.status != ClientStatus::Active {
            return Err(CoreError::Forbidden(
                "the client account is not active".into(),
            ));
        }
        let settings = BillingSettings::load(&mut *tx, tenant_id).await?;
        let today = self.clock().today();
        let currency = svc.currency;
        let priced_cycle = match addon.pay_type {
            PayType::OneTime => BillingCycle::OneTime,
            _ => svc.billing_cycle,
        };
        let catalogue_price =
            catalog::addon_price(&mut *tx, tenant_id, addon_id, currency, priced_cycle).await?;
        let (cycle, price, setup) = match addon.pay_type {
            PayType::Free => (svc.billing_cycle, Decimal::ZERO, Decimal::ZERO),
            PayType::OneTime => {
                let (p, s) = catalogue_price.ok_or_else(|| {
                    CoreError::validation(format!("{} is not available in {currency}", addon.name))
                })?;
                (BillingCycle::OneTime, p, s)
            }
            PayType::Recurring => {
                if !svc.billing_cycle.is_recurring() {
                    return Err(CoreError::validation(
                        "recurring addons need a service with a billing period",
                    ));
                }
                let (p, s) = catalogue_price.ok_or_else(|| {
                    CoreError::validation(format!(
                        "{} is not available for this billing cycle",
                        addon.name
                    ))
                })?;
                (svc.billing_cycle, p, s)
            }
        };
        let recurring = cycle.is_recurring() && addon.pay_type != PayType::OneTime;
        // First period: from today until the service's next due date (every
        // period of a service paid ahead, not just one).
        let first = match (recurring, svc.next_due_date, cycle.months()) {
            (true, Some(end), Some(months)) => {
                crate::pricing::charge_until(price, months, today, end, currency)?
            }
            _ => price,
        };
        let taxes = crate::pricing::tax_rates(
            &mut tx,
            tenant_id,
            &crate::pricing::TaxContext::for_client(&client, &settings),
        )
        .await?;
        let quote = crate::pricing::price(&crate::pricing::PricingInput {
            currency,
            lines: vec![crate::pricing::PriceLine {
                parent: None,
                item_type: LineItemType::Addon,
                product_id: None,
                addon_id: Some(addon_id),
                domain_name: None,
                tld: None,
                description: match svc.domain.as_deref().filter(|d| !d.is_empty()) {
                    Some(d) => format!(
                        "{} — {d} ({})",
                        addon.name,
                        crate::pricing::cycle_label(cycle, client.locale)
                    ),
                    None => format!(
                        "{} ({})",
                        addon.name,
                        crate::pricing::cycle_label(cycle, client.locale)
                    ),
                },
                billing_cycle: Some(cycle),
                years: None,
                quantity: 1,
                unit_price: first,
                recurring_unit_price: if recurring { price } else { Decimal::ZERO },
                setup_fee: setup,
                taxable: addon.is_taxable,
                config_options: Vec::new(),
            }],
            coupon: None,
            taxes,
            prices_include_tax: settings.prices_include_tax,
        })?;
        let line = &quote.lines[0];
        let (order, rows) = orders::create_order(
            &mut tx,
            tenant_id,
            &NewOrder {
                client_id: client.id,
                user_id,
                currency,
                coupon_id: None,
                coupon_code: None,
                payment_integration_id: client.default_gateway_id,
                tax_total: quote.tax_total,
                ip,
                user_agent: None,
                notes: None,
                cart_id: None,
                items: vec![NewOrderItem {
                    parent_index: None,
                    item_type: LineItemType::Addon,
                    product_id: None,
                    addon_id: Some(addon_id),
                    domain_name: svc.domain.clone(),
                    billing_cycle: Some(cycle),
                    years: None,
                    quantity: 1,
                    description: line.description.clone(),
                    unit_price: line.unit_price,
                    setup_fee: line.setup_fee,
                    discount: Decimal::ZERO,
                    config_selections: json!({}),
                    custom_fields: json!({}),
                    hostname: None,
                    epp_code_enc: None,
                }],
            },
        )
        .await?;
        let sa = services::create_service_addon(
            &mut *tx,
            tenant_id,
            service_id,
            addon_id,
            Some(order.id),
            cycle,
            line.setup_fee,
            line.recurring_amount,
            currency,
            if recurring { svc.next_due_date } else { None },
        )
        .await?;
        orders::link_order_item(&mut *tx, tenant_id, rows[0].id, None, Some(sa.id), None).await?;
        let created = Created {
            service_id: Some(service_id),
            service_addon_id: Some(sa.id),
            domain_id: None,
            order_item_id: Some(rows[0].id),
        };
        let mut items = invoice_lines(line, created, today, client.locale, None);
        if recurring
            && let (Some(first_line), Some(end)) = (items.first_mut(), svc.next_due_date)
            && end > today
        {
            first_line.period_end = end.checked_sub_days(Days::new(1));
        }
        let invoice = invoices::create_invoice(
            &mut tx,
            tenant_id,
            &NewInvoice {
                client_id: client.id,
                currency,
                issue_date: today,
                due_date: today,
                issue: true,
                payment_integration_id: client.default_gateway_id,
                notes: None,
                billing_snapshot: billing_snapshot(&client),
                items,
                taxes: quote.tax_inputs(),
            },
        )
        .await?;
        orders::set_order_invoice(&mut *tx, tenant_id, order.id, invoice.id).await?;
        services::log_event(
            &mut *tx,
            tenant_id,
            service_id,
            &NewServiceEvent {
                data: json!({"order_id": order.id, "service_addon_id": sa.id}),
                ..NewServiceEvent::info(
                    "addon_ordered",
                    &format!("{} (order {})", addon.name, order.order_number),
                )
            },
        )
        .await?;

        // ---- settle: credit / zero total -------------------------------------
        let mut invoice = invoice;
        if settings.apply_credit_automatically && invoice.balance > Decimal::ZERO {
            invoice =
                payments::apply_credit_to_invoice(&mut tx, tenant_id, invoice.id, None, user_id)
                    .await?
                    .0;
        }
        if invoice.status == hos_core::domain::InvoiceStatus::Unpaid
            && invoice.balance <= Decimal::ZERO
        {
            invoices::transition_invoice(
                &mut *tx,
                tenant_id,
                invoice.id,
                &[hos_core::domain::InvoiceStatus::Unpaid],
                hos_core::domain::InvoiceStatus::Paid,
            )
            .await?;
            invoice = invoices::get_invoice(&mut *tx, tenant_id, invoice.id)
                .await?
                .ok_or_else(|| CoreError::not_found("invoice", invoice.id))?;
        }
        let paid = invoice.status == hos_core::domain::InvoiceStatus::Paid;
        self.emit_event(
            &mut tx,
            tenant_id,
            "order.created",
            json!({"order_id": order.id, "order_number": order.order_number, "client_id": client.id,
                   "invoice_id": invoice.id, "total": quote.total, "currency": currency}),
        )
        .await?;
        self.notify_staff(
            &mut tx,
            tenant_id,
            "order.new",
            &format!(
                "New addon order {} ({})",
                order.order_number,
                crate::format::money(&quote.total_money(), client.locale)
            ),
            Some(&line.description),
            Some(&format!("/admin/orders/{}", order.id)),
            &json!({"order_id": order.id, "client_id": client.id}),
        )
        .await?;
        if paid {
            self.after_invoice_paid(&mut tx, tenant_id, invoice.id)
                .await?;
        } else {
            self.queue_invoice_created_email(&mut tx, tenant_id, &invoice)
                .await?;
        }
        tx.commit().await?;
        Ok(AddonOrder {
            order_id: order.id,
            order_number: order.order_number,
            invoice_id: invoice.id,
            service_addon_id: sa.id,
            total: quote.total,
            paid,
        })
    }
}
