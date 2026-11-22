//! Payment gateways: Stripe (Checkout Sessions + webhooks), iyzico (Checkout Form),
//! PayTR (iFrame API), PayPal (Orders v2), bank transfer (offline).
//!
//! ## Flow
//!
//! 1. `hos-billing` builds a [`CheckoutRequest`] for an unpaid invoice;
//!    [`PaymentGateway::create_checkout`] returns a [`CheckoutSession`] telling the
//!    web layer how to send the customer to the gateway.
//! 2. The gateway calls `/webhooks/gateway/{integration_id}` (server-to-server
//!    notification or browser POST-back). `hos-web` wraps the raw request in a
//!    [`WebhookRequest`] and calls [`PaymentGateway::parse_webhook`], which
//!    **must verify authenticity** (signature/hash, or by re-fetching the payment
//!    from the gateway API) before returning events.
//! 3. `hos-billing` applies each [`PaymentEvent`] idempotently (keyed by
//!    `event_id` / `gateway_txn_id`) and `hos-web` answers with
//!    [`WebhookOutcome::reply`].
//!
//! `CheckoutRequest::reference` is echoed back as [`PaymentEvent::invoice_ref`];
//! it is how events are matched to invoices.

use std::collections::BTreeMap;
use std::net::IpAddr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ConnectionInfo, PostalAddress};
use crate::domain::Locale;
use crate::error::IntegrationError;
use crate::ids::{ClientId, InvoiceId};
use crate::money::{Currency, Money};
use crate::secret::SecretString;

/// The paying customer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaymentCustomer {
    /// HostingOS client.
    pub client_id: Option<ClientId>,
    /// Full name (or company name).
    pub name: String,
    /// Email.
    pub email: String,
    /// Phone in E.164 (`+905551234567`); required by PayTR / iyzico.
    pub phone: Option<String>,
    /// Buyer IP (required by iyzico and PayTR).
    pub ip: Option<IpAddr>,
    /// Billing address (iyzico requires one).
    pub billing_address: Option<PostalAddress>,
    /// National id / tax number (iyzico `identityNumber`).
    pub identity_number: Option<String>,
    /// Customer id at the gateway, if previously created (Stripe `cus_...`).
    pub gateway_customer_id: Option<String>,
}

/// One line of the basket (iyzico `basketItems`, PayTR `user_basket`, PayPal `items`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutItem {
    /// Stable item reference (invoice item id).
    pub reference: String,
    /// Description shown to the customer.
    pub name: String,
    /// Category (iyzico requires `category1`).
    pub category: Option<String>,
    /// Quantity.
    pub quantity: u32,
    /// Unit price.
    pub unit_price: Money,
    /// Line total (rounded). The sum of `total`s equals [`CheckoutRequest::amount`].
    pub total: Money,
}

/// Request to start a hosted payment for an invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRequest {
    /// Invoice being paid.
    pub invoice_id: InvoiceId,
    /// Unique merchant reference **per payment attempt**: alphanumeric, ≤ 64 chars
    /// (PayTR `merchant_oid`, iyzico `conversationId`/`basketId`, Stripe
    /// `client_reference_id`, PayPal `custom_id`). Echoed as [`PaymentEvent::invoice_ref`].
    pub reference: String,
    /// Amount to charge (rounded to the currency's minor unit).
    pub amount: Money,
    /// Short description (`"Invoice #2026-0042"`).
    pub description: String,
    /// Customer.
    pub customer: PaymentCustomer,
    /// Basket lines.
    #[serde(default)]
    pub items: Vec<CheckoutItem>,
    /// Where the customer lands after paying.
    pub return_url: String,
    /// Where the customer lands after cancelling / failing.
    pub cancel_url: String,
    /// Server-to-server notification URL (`/webhooks/gateway/{integration_id}`).
    pub webhook_url: String,
    /// Payment page language.
    pub locale: Option<Locale>,
    /// Ask the gateway to store the payment method for future automatic charges.
    #[serde(default)]
    pub save_payment_method: bool,
    /// Extra key/values to attach at the gateway (Stripe metadata).
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// HTTP method of a [`CheckoutSession::Form`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FormMethod {
    /// `POST` (default).
    #[default]
    Post,
    /// `GET`.
    Get,
}

/// How the web layer should hand the customer to the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckoutSession {
    /// Redirect the browser (Stripe Checkout, PayPal approve link, iyzico `paymentPageUrl`).
    Redirect {
        /// Target URL.
        url: String,
        /// Gateway session / order id (Stripe `cs_...`, PayPal order id, iyzico token).
        gateway_ref: Option<String>,
        /// Session expiry.
        expires_at: Option<DateTime<Utc>>,
    },
    /// Auto-submit an HTML form to the gateway (3-D Secure style POST).
    Form {
        /// Form action URL.
        action: String,
        /// Method.
        method: FormMethod,
        /// Hidden fields, in order.
        fields: Vec<(String, String)>,
        /// Gateway reference.
        gateway_ref: Option<String>,
    },
    /// Show the gateway page inside an iframe (PayTR `https://www.paytr.com/odeme/guvenli/{token}`).
    Iframe {
        /// Iframe source URL.
        url: String,
        /// Gateway reference (PayTR token).
        gateway_ref: Option<String>,
    },
    /// Embed gateway-provided HTML/JS (iyzico `checkoutFormContent`). Trusted gateway output.
    Embed {
        /// HTML snippet.
        html: String,
        /// Gateway reference.
        gateway_ref: Option<String>,
    },
    /// No online step (bank transfer): show instructions; staff confirm payment manually.
    Offline {
        /// Payment instructions (plain text / Markdown: IBAN, account holder, reference).
        instructions: String,
    },
}

impl CheckoutSession {
    /// The gateway reference, if any.
    pub fn gateway_ref(&self) -> Option<&str> {
        match self {
            Self::Redirect { gateway_ref, .. }
            | Self::Form { gateway_ref, .. }
            | Self::Iframe { gateway_ref, .. }
            | Self::Embed { gateway_ref, .. } => gateway_ref.as_deref(),
            Self::Offline { .. } => None,
        }
    }
}

/// Raw inbound webhook / callback, as received by `hos-web`.
#[derive(Debug, Clone, Default)]
pub struct WebhookRequest {
    /// Request headers.
    pub headers: HeaderMap,
    /// Raw body bytes (signatures are computed over these — never re-serialize).
    pub body: Vec<u8>,
    /// Raw query string without `?`.
    pub query: Option<String>,
    /// Peer IP (after trusted-proxy resolution), for IP allow-lists.
    pub remote_ip: Option<IpAddr>,
}

impl WebhookRequest {
    /// A header value as UTF-8 (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Body as UTF-8.
    pub fn body_str(&self) -> Result<&str, IntegrationError> {
        std::str::from_utf8(&self.body)
            .map_err(|_| IntegrationError::InvalidInput("webhook body is not UTF-8".into()))
    }

    /// `application/x-www-form-urlencoded` body as ordered pairs (PayTR, iyzico callback).
    pub fn form_fields(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }

    /// First value of a form field.
    pub fn form_field(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(&self.body)
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }

    /// Query string as ordered pairs.
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        self.query
            .as_deref()
            .map(|q| {
                url::form_urlencoded::parse(q.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Body parsed as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, IntegrationError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| IntegrationError::InvalidInput(format!("invalid webhook JSON: {e}")))
    }
}

crate::string_enum! {
    /// What happened to a payment.
    pub enum PaymentEventKind {
        /// Money captured — mark the invoice paid.
        Paid => "paid",
        /// Started but not final (bank transfer pending, async capture).
        Pending => "pending",
        /// Payment attempt failed / declined.
        Failed => "failed",
        /// Full or partial refund completed (amount says how much).
        Refunded => "refunded",
        /// Dispute / chargeback opened.
        Chargeback => "chargeback",
        /// A payment method was saved without a payment
        /// ([`PaymentGateway::create_card_setup`]); `invoice_ref` is the
        /// setup reference, `amount` is zero, `payment_method_ref` is set.
        MethodSaved => "method_saved",
    }
}

/// A normalised payment event extracted from a webhook (or a stored-card charge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEvent {
    /// Kind.
    pub kind: PaymentEventKind,
    /// Gateway event id for idempotency (Stripe `evt_...`); `None` if the gateway has none.
    pub event_id: Option<String>,
    /// Gateway transaction / payment id (Stripe PaymentIntent, iyzico `paymentId`,
    /// PayPal capture id). Used for refunds.
    pub gateway_txn_id: String,
    /// The [`CheckoutRequest::reference`] this event belongs to, when known.
    pub invoice_ref: Option<String>,
    /// Amount of this event (captured, refunded or disputed).
    pub amount: Money,
    /// Gateway fee, when reported.
    pub fee: Option<Money>,
    /// When it happened at the gateway.
    pub occurred_at: Option<DateTime<Utc>>,
    /// Failure / decline reason.
    pub message: Option<String>,
    /// Token of a stored payment method created by this payment (for auto-charge).
    pub payment_method_ref: Option<SecretString>,
    /// Gateway refund id of a `Refunded` event (Stripe `re_…`, PayPal refund
    /// id …), when the gateway reported one. Billing matches refunds by it,
    /// so a refund completed synchronously is not recorded a second time by
    /// its webhook. `None` for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_refund_id: Option<String>,
    /// Raw gateway payload (stored in `webhooks_in` / `transactions`). Must not contain card data.
    #[serde(default)]
    pub raw: Value,
}

/// What `hos-web` answers to the webhook caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum WebhookReply {
    /// `200 OK`, empty body.
    #[default]
    Empty,
    /// `200 OK` with a plain-text body (PayTR requires exactly `OK`).
    Text(String),
    /// `200 OK` with a JSON body.
    Json(Value),
    /// `303 See Other` to this URL (browser POST-backs such as iyzico's callback).
    Redirect(String),
}

/// Result of [`PaymentGateway::parse_webhook`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookOutcome {
    /// Events to apply (may be empty for ignored event types).
    pub events: Vec<PaymentEvent>,
    /// Response to send.
    pub reply: WebhookReply,
}

/// Refund request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundRequest {
    /// [`PaymentEvent::gateway_txn_id`] of the original payment.
    pub gateway_txn_id: String,
    /// Amount to refund (≤ captured amount; partial allowed if supported).
    pub amount: Money,
    /// Reason (shown in the gateway dashboard).
    pub reason: Option<String>,
    /// Idempotency key, unique per refund attempt.
    pub reference: String,
    /// Requesting staff member's IP (iyzico requires one).
    pub ip: Option<IpAddr>,
}

crate::string_enum! {
    /// Refund outcome.
    pub enum RefundStatus {
        /// Completed.
        Succeeded => "succeeded",
        /// Accepted, completes asynchronously (a `Refunded` webhook will follow).
        Pending => "pending",
        /// Rejected.
        Failed => "failed",
    }
}

/// Result of [`PaymentGateway::refund`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundResult {
    /// Gateway refund id.
    pub gateway_refund_id: Option<String>,
    /// Outcome.
    pub status: RefundStatus,
    /// Amount refunded / requested.
    pub amount: Money,
    /// Failure reason.
    pub message: Option<String>,
    /// Raw gateway payload.
    #[serde(default)]
    pub raw: Value,
}

/// Off-session charge of a stored payment method (automatic renewal payments).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredChargeRequest {
    /// Invoice being paid.
    pub invoice_id: InvoiceId,
    /// Unique merchant reference for this attempt.
    pub reference: String,
    /// Amount.
    pub amount: Money,
    /// Description.
    pub description: String,
    /// Customer (with `gateway_customer_id` where required).
    pub customer: PaymentCustomer,
    /// Stored method token from [`PaymentEvent::payment_method_ref`].
    pub payment_method_ref: SecretString,
}

/// Request to save a payment method without charging it (Stripe Checkout
/// `mode=setup`). The completion arrives as a signed
/// [`PaymentEventKind::MethodSaved`] event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardSetupRequest {
    /// Unique merchant reference of this setup (alphanumeric, ≤ 64 chars),
    /// echoed as [`PaymentEvent::invoice_ref`].
    pub reference: String,
    /// Currency the method will be charged in.
    pub currency: Currency,
    /// The customer (a gateway customer is created when it has none).
    pub customer: PaymentCustomer,
    /// Where the customer lands after saving.
    pub return_url: String,
    /// Where the customer lands after cancelling.
    pub cancel_url: String,
    /// Server-to-server notification URL (`/webhooks/gateway/{integration_id}`).
    pub webhook_url: String,
    /// Page language.
    pub locale: Option<Locale>,
}

/// Feature flags of a gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayCapabilities {
    /// API refunds supported.
    pub refunds: bool,
    /// Partial refunds supported.
    pub partial_refunds: bool,
    /// Stored payment methods + [`PaymentGateway::charge_stored`] supported.
    pub stored_payment_methods: bool,
    /// Installments (taksit) offered on the payment page.
    pub installments: bool,
    /// Offline gateway (bank transfer): no online payment, manual confirmation.
    pub offline: bool,
    /// Supported currencies; empty = any.
    pub currencies: Vec<Currency>,
}

impl GatewayCapabilities {
    /// Whether `currency` is accepted.
    pub fn supports_currency(&self, currency: Currency) -> bool {
        self.currencies.is_empty() || self.currencies.contains(&currency)
    }
}

/// A payment gateway. See the [module docs](self).
#[async_trait]
pub trait PaymentGateway: Send + Sync {
    /// The integration kind (e.g. [`kinds::STRIPE`](super::kinds::STRIPE)).
    fn kind(&self) -> &'static str;

    /// Feature flags.
    fn capabilities(&self) -> GatewayCapabilities;

    /// Verifies credentials. Default: `Unsupported`.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        Err(IntegrationError::Unsupported("test_connection"))
    }

    /// Starts a hosted payment.
    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError>;

    /// Verifies and parses an inbound webhook / callback. Unverifiable requests
    /// → [`IntegrationError::Auth`]; irrelevant event types → empty `events`.
    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError>;

    /// Whether [`parse_webhook`](Self::parse_webhook) has to call the
    /// provider before it can tell a forged `request` from a genuine one
    /// (PayPal's webhook verification API). The webhook endpoint keeps a
    /// rate limit in front of such requests; locally verified ones (HMAC,
    /// signed return URLs) are verified first. Default: `false`.
    fn verifies_remotely(&self, _request: &WebhookRequest) -> bool {
        false
    }

    /// Refunds (part of) a payment. Offline gateways return `Unsupported`.
    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError>;

    /// [`refund`](Self::refund) for gateways that report the outcome of a
    /// refund asynchronously to a URL given with the request (Paynet's
    /// `succeedUrl`). `webhook_url` is this integration's
    /// `/webhooks/gateway/{integration_id}`. Default: [`refund`](Self::refund).
    async fn refund_with_callback(
        &self,
        request: &RefundRequest,
        webhook_url: &str,
    ) -> Result<RefundResult, IntegrationError> {
        let _ = webhook_url;
        self.refund(request).await
    }

    /// Installment counts (2…n) the customer may choose on the client pay
    /// page for `amount`, for gateways that take the choice from the
    /// merchant (NestPay `taksit`). Empty (default): single payment, or the
    /// gateway offers installments on its own page.
    fn installment_choices(&self, amount: &Money) -> Vec<u32> {
        let _ = amount;
        Vec::new()
    }

    /// Whether [`create_card_setup`](Self::create_card_setup) is supported.
    fn supports_card_setup(&self) -> bool {
        false
    }

    /// Starts saving a payment method without a payment. Default: `Unsupported`.
    async fn create_card_setup(
        &self,
        request: &CardSetupRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        let _ = request;
        Err(IntegrationError::Unsupported("create_card_setup"))
    }

    /// Charges a stored payment method off-session. Default: `Unsupported`.
    async fn charge_stored(
        &self,
        request: &StoredChargeRequest,
    ) -> Result<PaymentEvent, IntegrationError> {
        let _ = request;
        Err(IntegrationError::Unsupported("charge_stored"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    #[test]
    fn checkout_session_serde_and_ref() {
        let s = CheckoutSession::Redirect {
            url: "https://checkout.stripe.com/c/pay/cs_1".into(),
            gateway_ref: Some("cs_1".into()),
            expires_at: None,
        };
        assert_eq!(s.gateway_ref(), Some("cs_1"));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "redirect");
        let off = CheckoutSession::Offline {
            instructions: "IBAN TR..".into(),
        };
        assert_eq!(off.gateway_ref(), None);
        let form = CheckoutSession::Form {
            action: "https://x".into(),
            method: FormMethod::Post,
            fields: vec![("a".into(), "1".into())],
            gateway_ref: None,
        };
        let back: CheckoutSession =
            serde_json::from_value(serde_json::to_value(&form).unwrap()).unwrap();
        assert_eq!(back, form);
        assert_eq!(
            serde_json::to_value(FormMethod::Post).unwrap(),
            json!("POST")
        );
    }

    #[test]
    fn webhook_request_helpers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Stripe-Signature",
            http::HeaderValue::from_static("t=1,v1=abc"),
        );
        let req = WebhookRequest {
            headers,
            body: b"merchant_oid=INV42&status=success&total_amount=1250&hash=x%2By".to_vec(),
            query: Some("a=1&b=two".into()),
            remote_ip: None,
        };
        assert_eq!(req.header("stripe-signature"), Some("t=1,v1=abc"));
        assert_eq!(req.form_field("hash").as_deref(), Some("x+y"));
        assert_eq!(req.form_fields().len(), 4);
        assert_eq!(req.query_pairs()[1], ("b".to_owned(), "two".to_owned()));
        assert!(req.json::<Value>().is_err());
    }

    #[test]
    fn webhook_reply_serde() {
        assert_eq!(
            serde_json::to_value(WebhookReply::Empty).unwrap(),
            json!({"type": "empty"})
        );
        assert_eq!(
            serde_json::to_value(WebhookReply::Text("OK".into())).unwrap(),
            json!({"type": "text", "value": "OK"})
        );
    }

    #[test]
    fn capabilities_currency() {
        let any = GatewayCapabilities::default();
        assert!(any.supports_currency(Currency::TRY));
        let only_try = GatewayCapabilities {
            currencies: vec![Currency::TRY],
            ..Default::default()
        };
        assert!(!only_try.supports_currency(Currency::USD));
        let ev = PaymentEvent {
            kind: PaymentEventKind::Paid,
            event_id: None,
            gateway_txn_id: "pi_1".into(),
            invoice_ref: Some("INV42".into()),
            amount: Money::new(dec!(12.50), Currency::TRY),
            fee: None,
            occurred_at: None,
            message: None,
            payment_method_ref: Some("pm_secret".into()),
            gateway_refund_id: None,
            raw: Value::Null,
        };
        assert!(!format!("{ev:?}").contains("pm_secret"));
        // Older serialized events (without the refund id) still deserialize.
        let mut v = serde_json::to_value(&ev).unwrap();
        assert!(v.get("gateway_refund_id").is_none());
        v.as_object_mut().unwrap().remove("payment_method_ref");
        let back: PaymentEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back.gateway_refund_id, None);
    }
}
