//! JSON shapes of API resources. Money is a decimal string (two places)
//! next to its `currency`; ids are UUID strings; times RFC 3339.

use hos_billing::Quote;
use hos_core::integrations::gateway::CheckoutSession;
use hos_db::repo::catalog::{
    CatalogProduct, PriceTag, ProductConfigOption, ProductPricingRow, ProductRow,
};
use hos_db::repo::clients::ClientRow;
use hos_db::repo::domains::{DomainRow, TldPriceTableRow};
use hos_db::repo::integrations::IntegrationRow;
use hos_db::repo::invoices::{InvoiceItemRow, InvoiceRow};
use hos_db::repo::orders::{OrderItemRow, OrderRow};
use hos_db::repo::payments::TransactionRow;
use hos_db::repo::services::{ServiceListRow, ServiceRow};
use hos_db::repo::support::{TicketDepartmentRow, TicketMessageRow, TicketRow};
use hos_db::repo::webhooks::WebhookEndpointRow;
use rust_decimal::Decimal;
use serde_json::{Value, json};

/// `12.5` → `"12.50"`.
pub fn money(d: Decimal) -> String {
    format!("{:.2}", d.round_dp(2))
}

fn opt_money(d: Option<Decimal>) -> Value {
    d.map_or(Value::Null, |d| Value::String(money(d)))
}

/// Client.
pub fn client(c: &ClientRow) -> Value {
    json!({
        "id": c.id,
        "kind": c.kind.as_str(),
        "status": c.status.as_str(),
        "first_name": c.first_name,
        "last_name": c.last_name,
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
        "currency": c.currency.as_str(),
        "locale": c.locale.as_str(),
        "tax_exempt": c.tax_exempt,
        "marketing_opt_in": c.marketing_opt_in,
        "created_at": c.created_at,
        "updated_at": c.updated_at,
    })
}

/// Price of a product (catalog).
pub fn price_tag(p: &PriceTag, currency: &str) -> Value {
    json!({
        "billing_cycle": p.billing_cycle.as_str(),
        "price": money(p.price),
        "setup_fee": money(p.setup_fee),
        "currency": currency,
    })
}

/// Price row (staff product view).
pub fn pricing(p: &ProductPricingRow) -> Value {
    json!({
        "billing_cycle": p.billing_cycle.as_str(),
        "price": money(p.price),
        "setup_fee": money(p.setup_fee),
        "currency": p.currency.as_str(),
        "enabled": p.is_enabled,
    })
}

/// Product (staff view; `prices` optional).
pub fn product(p: &ProductRow, prices: Option<&[ProductPricingRow]>) -> Value {
    let mut v = json!({
        "id": p.id,
        "slug": p.slug,
        "name": p.name,
        "type": p.product_type.as_str(),
        "group_id": p.group_id,
        "tagline": p.tagline,
        "description": p.description,
        "features": p.features,
        "pay_type": p.pay_type.as_str(),
        "hidden": p.is_hidden,
        "retired": p.is_retired,
        "featured": p.is_featured,
        "stock_control": p.stock_control,
        "stock_quantity": p.stock_quantity,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    });
    if let Some(prices) = prices {
        v["prices"] = prices.iter().map(pricing).collect();
    }
    v
}

/// Public catalog product.
pub fn catalog_product(c: &CatalogProduct, currency: &str) -> Value {
    let p = &c.product;
    json!({
        "id": p.id,
        "slug": p.slug,
        "name": p.name,
        "type": p.product_type.as_str(),
        "tagline": p.tagline,
        "description": p.description,
        "features": p.features,
        "featured": p.is_featured,
        "requires_domain": p.requires_domain.as_str(),
        "prices": c.prices.iter().map(|t| price_tag(t, currency)).collect::<Vec<_>>(),
    })
}

/// Configurable option of a product (send as
/// `config_options: { "<option id>": { "value_id": "<value id>", "quantity": n } }`).
pub fn config_option(o: &ProductConfigOption, currency: &str) -> Value {
    json!({
        "id": o.option.id,
        "name": o.option.name,
        "type": o.option.option_type.as_str(),
        "module_key": o.option.module_key,
        "unit": o.option.unit,
        "min_quantity": o.option.min_quantity,
        "max_quantity": o.option.max_quantity,
        "values": o.values.iter().map(|v| json!({
            "id": v.id,
            "label": v.label,
            "value": v.value,
            "prices": v.prices.iter().map(|t| price_tag(t, currency)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// TLD with prices.
pub fn tld(t: &TldPriceTableRow, currency: &str) -> Value {
    json!({
        "tld": t.tld,
        "label": t.label,
        "featured": t.is_featured,
        "register": opt_money(t.register),
        "transfer": opt_money(t.transfer),
        "renew": opt_money(t.renew),
        "currency": currency,
    })
}

/// Order (`items` optional).
pub fn order(o: &OrderRow, items: Option<&[OrderItemRow]>) -> Value {
    let mut v = json!({
        "id": o.id,
        "number": o.order_number,
        "client_id": o.client_id,
        "status": o.status.as_str(),
        "currency": o.currency.as_str(),
        "subtotal": money(o.subtotal),
        "discount_total": money(o.discount_total),
        "tax_total": money(o.tax_total),
        "total": money(o.total),
        "coupon_code": o.coupon_code,
        "invoice_id": o.invoice_id,
        "notes": o.notes,
        "accepted_at": o.accepted_at,
        "cancelled_at": o.cancelled_at,
        "created_at": o.created_at,
    });
    if let Some(items) = items {
        v["items"] = items
            .iter()
            .map(|i| {
                json!({
                    "id": i.id,
                    "type": i.item_type.as_str(),
                    "description": i.description,
                    "product_id": i.product_id,
                    "service_id": i.service_id,
                    "domain_id": i.domain_id,
                    "domain_name": i.domain_name,
                    "billing_cycle": i.billing_cycle.map(|c| c.as_str()),
                    "years": i.years,
                    "quantity": i.quantity,
                    "unit_price": money(i.unit_price),
                    "setup_fee": money(i.setup_fee),
                    "discount": money(i.discount),
                    "amount": money(i.amount),
                })
            })
            .collect();
    }
    v
}

/// Invoice (`items` / `transactions` optional).
pub fn invoice(
    i: &InvoiceRow,
    items: Option<&[InvoiceItemRow]>,
    transactions: Option<&[TransactionRow]>,
) -> Value {
    let mut v = json!({
        "id": i.id,
        "number": i.number,
        "client_id": i.client_id,
        "status": i.status.as_str(),
        "currency": i.currency.as_str(),
        "issue_date": i.issue_date,
        "due_date": i.due_date,
        "paid_at": i.paid_at,
        "subtotal": money(i.subtotal),
        "discount_total": money(i.discount_total),
        "tax_total": money(i.tax_total),
        "total": money(i.total),
        "amount_paid": money(i.amount_paid),
        "credit_applied": money(i.credit_applied),
        "balance": money(i.balance),
        "notes": i.notes,
        "created_at": i.created_at,
    });
    if let Some(items) = items {
        v["items"] = items
            .iter()
            .map(|it| {
                json!({
                    "id": it.id,
                    "type": it.item_type.as_str(),
                    "description": it.description,
                    "quantity": it.quantity.normalize().to_string(),
                    "unit_price": money(it.unit_price),
                    "amount": money(it.amount),
                    "taxable": it.taxable,
                    "service_id": it.service_id,
                    "domain_id": it.domain_id,
                    "period_start": it.period_start,
                    "period_end": it.period_end,
                })
            })
            .collect();
    }
    if let Some(txns) = transactions {
        v["transactions"] = txns.iter().map(transaction).collect();
    }
    v
}

/// Transaction.
pub fn transaction(t: &TransactionRow) -> Value {
    json!({
        "id": t.id,
        "client_id": t.client_id,
        "invoice_id": t.invoice_id,
        "gateway": t.gateway,
        "gateway_txn_id": t.gateway_txn_id,
        "kind": t.kind.as_str(),
        "status": t.status.as_str(),
        "amount": money(t.amount),
        "fee": money(t.fee),
        "currency": t.currency.as_str(),
        "description": t.description,
        "occurred_at": t.occurred_at,
    })
}

fn service_core(s: &ServiceRow) -> Value {
    json!({
        "id": s.id,
        "client_id": s.client_id,
        "product_id": s.product_id,
        "order_id": s.order_id,
        "status": s.status.as_str(),
        "billing_cycle": s.billing_cycle.as_str(),
        "first_payment_amount": money(s.first_payment_amount),
        "recurring_amount": money(s.recurring_amount),
        "currency": s.currency.as_str(),
        "quantity": s.quantity,
        "domain": s.domain,
        "username": s.username,
        "server_hostname": s.server_hostname,
        "dedicated_ip": s.dedicated_ip,
        "assigned_ips": s.assigned_ips,
        "registration_date": s.registration_date,
        "next_due_date": s.next_due_date,
        "suspended_at": s.suspended_at,
        "suspended_reason": s.suspended_reason,
        "terminated_at": s.terminated_at,
        "created_at": s.created_at,
    })
}

/// Service (list row: with product / client names).
pub fn service(s: &ServiceListRow) -> Value {
    let mut v = service_core(&s.service);
    v["product_name"] = json!(s.product_name);
    v["product_type"] = json!(s.product_type.as_str());
    v["client_name"] = json!(s.client_name);
    v
}

/// Service (bare row).
pub fn service_row(s: &ServiceRow) -> Value {
    service_core(s)
}

/// Domain.
pub fn domain(d: &DomainRow) -> Value {
    json!({
        "id": d.id,
        "client_id": d.client_id,
        "name": d.name,
        "tld": d.tld,
        "status": d.status.as_str(),
        "registration_type": d.registration_type.as_str(),
        "registration_period": d.registration_period,
        "registered_at": d.registered_at,
        "expires_at": d.expires_at,
        "next_due_date": d.next_due_date,
        "recurring_amount": money(d.recurring_amount),
        "currency": d.currency.as_str(),
        "auto_renew": d.auto_renew,
        "privacy": d.privacy,
        "locked": d.lock,
        "nameservers": d.nameservers,
        "last_synced_at": d.last_synced_at,
        "created_at": d.created_at,
    })
}

/// Ticket.
pub fn ticket(t: &TicketRow) -> Value {
    json!({
        "id": t.id,
        "number": t.number,
        "subject": t.subject,
        "status": t.status.as_str(),
        "priority": t.priority.as_str(),
        "department_id": t.department_id,
        "client_id": t.client_id,
        "service_id": t.service_id,
        "domain_id": t.domain_id,
        "last_reply_at": t.last_reply_at,
        "last_reply_by": t.last_reply_by.as_str(),
        "closed_at": t.closed_at,
        "created_at": t.created_at,
    })
}

/// Ticket message.
pub fn ticket_message(m: &TicketMessageRow) -> Value {
    json!({
        "id": m.id,
        "author_kind": m.author_kind.as_str(),
        "author_name": m.author_name,
        "body": m.body,
        "internal": m.is_internal,
        "source": m.source.as_str(),
        "created_at": m.created_at,
    })
}

/// Support department.
pub fn department(d: &TicketDepartmentRow) -> Value {
    json!({
        "id": d.id,
        "slug": d.slug,
        "name": d.name,
        "description": d.description,
        "clients_only": d.clients_only,
    })
}

/// Integration metadata (never config or secrets).
pub fn integration(i: &IntegrationRow) -> Value {
    json!({
        "id": i.id,
        "category": i.category.as_str(),
        "kind": i.kind,
        "name": i.name,
        "active": i.is_active,
        "default": i.is_default,
        "has_secrets": i.secrets_enc.is_some(),
        "last_test_at": i.last_test_at,
        "last_test_ok": i.last_test_ok,
        "created_at": i.created_at,
    })
}

/// Outgoing webhook endpoint (secret never included).
pub fn webhook_endpoint(e: &WebhookEndpointRow) -> Value {
    json!({
        "id": e.id,
        "url": e.url,
        "description": e.description,
        "events": e.events,
        "active": e.is_active,
        "last_delivery_at": e.last_delivery_at,
        "last_status_code": e.last_status_code,
        "failure_count": e.failure_count,
        "created_at": e.created_at,
    })
}

/// Price quote.
pub fn quote(q: &Quote) -> Value {
    json!({
        "currency": q.currency.as_str(),
        "lines": q.lines.iter().map(|l| json!({
            "index": l.index,
            "parent": l.parent,
            "type": l.item_type.as_str(),
            "description": l.description,
            "product_id": l.product_id,
            "domain_name": l.domain_name,
            "billing_cycle": l.billing_cycle.map(|c| c.as_str()),
            "years": l.years,
            "quantity": l.quantity,
            "unit_price": money(l.unit_price),
            "setup_fee": money(l.setup_fee),
            "discount": money(l.discount),
            "amount": money(l.amount),
            "recurring_amount": money(l.recurring_amount),
        })).collect::<Vec<_>>(),
        "coupon_error": q.coupon_error,
        "prices_include_tax": q.prices_include_tax,
        "subtotal": money(q.subtotal),
        "discount_total": money(q.discount_total),
        "setup_total": money(q.setup_total),
        "tax_total": money(q.tax_total),
        "total": money(q.total),
    })
}

/// How to continue a payment.
pub fn checkout_session(s: &CheckoutSession) -> Value {
    match s {
        CheckoutSession::Redirect {
            url, expires_at, ..
        } => {
            json!({ "type": "redirect", "url": url, "expires_at": expires_at })
        }
        CheckoutSession::Form {
            action,
            method,
            fields,
            ..
        } => json!({
            "type": "form",
            "action": action,
            "method": format!("{method:?}").to_ascii_uppercase(),
            "fields": fields.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect::<Vec<_>>(),
        }),
        CheckoutSession::Iframe { url, .. } => json!({ "type": "iframe", "url": url }),
        CheckoutSession::Embed { html, .. } => json!({ "type": "embed", "html": html }),
        CheckoutSession::Offline { instructions } => {
            json!({ "type": "offline", "instructions": instructions })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_is_two_place_string() {
        assert_eq!(money(Decimal::new(125, 1)), "12.50");
        assert_eq!(money(Decimal::new(1, 3)), "0.00");
        assert_eq!(money(Decimal::new(19999, 2)), "199.99");
    }
}
