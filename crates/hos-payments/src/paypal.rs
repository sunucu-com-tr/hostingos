//! PayPal Orders v2 (redirect to PayPal, capture on return) with webhooks.
//!
//! * OAuth 2.0 client credentials (`POST /v1/oauth2/token`); the access token is
//!   cached until shortly before `expires_in` and refreshed once on a 401.
//! * Checkout: `POST /v2/checkout/orders` (`intent=CAPTURE`,
//!   `payment_source.paypal.experience_context`) → redirect to the `payer-action`
//!   link. PayPal sends the buyer back to `return_url?token={order id}&PayerID=…`.
//!   `return_url` points at `/webhooks/gateway/{id}` (with signed invoice URLs),
//!   so [`PaymentGateway::parse_webhook`] captures the order
//!   (`POST /v2/checkout/orders/{id}/capture`) and answers with a 303 to the invoice.
//! * Webhooks: verified by PayPal itself (`POST /v1/notifications/verify-webhook-signature`)
//!   with the raw body spliced in verbatim as `webhook_event` (never re-serialized).
//! * `gateway_txn_id` is the capture id. Capture/refund/dispute events carry
//!   synthetic, stable `event_id`s (`paypal:{capture}:{kind}`, `paypal:refund:{id}`,
//!   `paypal:dispute:{id}`) so the return path and the webhook dedupe to one event;
//!   the PayPal webhook id stays in `raw`.
//! * Vault (saved PayPal account for renewals; `vault_enabled`, needs PayPal's
//!   approval for vaulting / reference transactions). Docs read 2026-11-18:
//!   <https://developer.paypal.com/docs/checkout/save-payment-methods/during-purchase/orders-api/paypal/>
//!   (`/api/orders-v2/save-with-purchase/paypal.md`) and
//!   "Use payment method token" (`/api/payment-tokens/save-without-purchase/paypal.md`).
//!   Save during purchase: `payment_source.paypal.attributes.vault =
//!   {store_in_vault: ON_SUCCESS, usage_type: MERCHANT}`; after capture
//!   `payment_source.paypal.attributes.vault.status == VAULTED` → `vault.id` is
//!   the payment-method token (taken before card/payer data is stripped).
//!   Renewal: `POST /v2/checkout/orders` with `payment_source.paypal.vault_id`
//!   and `intent=CAPTURE` (captured in the same call; `PayPal-Request-Id`
//!   keeps it idempotent). PayPal recommends sending FraudNet/RDA data for
//!   customer-present payments; merchant-initiated renewals do not need it.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hos_core::domain::{IntegrationCategory, Locale};
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{
    CheckoutRequest, CheckoutSession, GatewayCapabilities, PaymentEvent, PaymentEventKind,
    PaymentGateway, RefundRequest, RefundResult, RefundStatus, StoredChargeRequest, WebhookOutcome,
    WebhookReply, WebhookRequest,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, kinds,
};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::common::{
    self, DEFAULT_TIMEOUT, Mode, Response, ReturnSigner, auth, ensure_positive, is_alphanumeric,
    itemizable, json_id, json_money, now_unix, strip_keys, truncate,
};

/// Funding-instrument details removed from `PaymentEvent::raw`.
const CARD_KEYS: &[&str] = &["payment_source", "card"];

/// Sandbox REST base URL.
pub const SANDBOX_BASE: &str = "https://api-m.sandbox.paypal.com";
/// Live REST base URL.
pub const LIVE_BASE: &str = "https://api-m.paypal.com";
/// Currencies PayPal accepts (developer.paypal.com › Currency codes).
pub const CURRENCIES: [&str; 25] = [
    "AUD", "BRL", "CAD", "CNY", "CZK", "DKK", "EUR", "HKD", "HUF", "ILS", "JPY", "MYR", "MXN",
    "TWD", "NZD", "NOK", "PHP", "PLN", "GBP", "RUB", "SGD", "SEK", "CHF", "THB", "USD",
];
/// Currencies PayPal rejects decimals for.
const NO_DECIMALS: [&str; 3] = ["HUF", "JPY", "TWD"];
/// Default webhook transmission-time tolerance (PayPal's sample uses 5 minutes).
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

/// Formats an amount as PayPal's `value` string (`"12.50"`, `"1500"` for HUF/JPY/TWD).
pub fn to_paypal_value(money: &Money) -> Result<String, IntegrationError> {
    let rounded = money.round();
    if NO_DECIMALS.contains(&rounded.currency.as_str()) {
        if !rounded.amount.fract().is_zero() {
            return Err(IntegrationError::InvalidInput(format!(
                "PayPal does not accept decimals for {}",
                rounded.currency
            )));
        }
        return Ok(rounded.amount.trunc().normalize().to_string());
    }
    Ok(rounded.to_decimal_string())
}

/// PayPal settings.
#[derive(Debug, Clone)]
pub struct PaypalSettings {
    /// REST app client id.
    pub client_id: String,
    /// REST app secret.
    pub client_secret: SecretString,
    /// Webhook id from the developer dashboard (needed for verification).
    pub webhook_id: String,
    /// REST base URL.
    pub base_url: String,
    /// Brand name shown on PayPal pages.
    pub brand_name: Option<String>,
    /// Webhook transmission-time tolerance; `0` disables the check.
    pub webhook_tolerance_secs: i64,
    /// Save PayPal accounts in the PayPal vault for automatic renewals.
    pub vault_enabled: bool,
    /// HTTP timeout.
    pub timeout: Duration,
}

impl PaypalSettings {
    /// Settings with defaults (sandbox).
    pub fn new(
        client_id: impl Into<String>,
        client_secret: SecretString,
        webhook_id: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret,
            webhook_id: webhook_id.into(),
            base_url: SANDBOX_BASE.to_owned(),
            brand_name: None,
            webhook_tolerance_secs: DEFAULT_TOLERANCE_SECS,
            vault_enabled: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Reads settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let mut settings = Self::new(
            c.str("client_id")?,
            s.secret("client_secret")?,
            c.str("webhook_id")?,
        );
        settings.base_url = match Mode::from_config(&c)? {
            Mode::Test => SANDBOX_BASE,
            Mode::Live => LIVE_BASE,
        }
        .to_owned();
        settings.brand_name = c.opt_str("brand_name")?.map(|b| truncate(b, 127));
        settings.vault_enabled = c.bool_or("vault_enabled", false)?;
        settings.webhook_tolerance_secs = i64::try_from(c.u64_or(
            "webhook_tolerance_secs",
            DEFAULT_TOLERANCE_SECS.unsigned_abs(),
        )?)
        .map_err(|_| {
            IntegrationError::Config("config.webhook_tolerance_secs is too large".into())
        })?;
        Ok(settings)
    }
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::PAYPAL, "PayPal", IntegrationCategory::Gateway)
        .description(
            "PayPal Checkout (Orders v2, redirect to PayPal). Create a webhook for \
             /webhooks/gateway/{integration_id} with PAYMENT.CAPTURE.COMPLETED, \
             PAYMENT.CAPTURE.PENDING, PAYMENT.CAPTURE.DENIED, PAYMENT.CAPTURE.REFUNDED and \
             CUSTOMER.DISPUTE.CREATED, and paste its id. The same URL also receives the buyer's \
             return (GET) to capture the order. Currencies: AUD, BRL, CAD, CNY, CZK, DKK, EUR, \
             HKD, HUF, ILS, JPY, MYR, MXN, TWD, NZD, NOK, PHP, PLN, GBP, RUB, SGD, SEK, CHF, THB, USD \
             (no TRY).",
        )
        .docs_url("https://developer.paypal.com/docs/api/orders/v2/")
        .config_field(common::mode_field())
        .config_field(FieldSpec::text("client_id", "Client id").required())
        .config_field(
            FieldSpec::text("webhook_id", "Webhook id")
                .required()
                .help("Developer dashboard → your app → Webhooks."),
        )
        .config_field(FieldSpec::text("brand_name", "Brand name on PayPal pages"))
        .config_field(
            FieldSpec::boolean("vault_enabled", "Save PayPal accounts for renewals")
                .default_value(false)
                .help(
                    "Customers who tick \"save\" are vaulted and charged automatically for \
                     renewals. Requires PayPal to enable vaulting (reference transactions) on \
                     your account.",
                ),
        )
        .config_field(
            FieldSpec::number("webhook_tolerance_secs", "Webhook timestamp tolerance (seconds)")
                .default_value(DEFAULT_TOLERANCE_SECS)
                .help("0 disables the replay window check."),
        )
        .secret_field(FieldSpec::password("client_secret", "Client secret").required())
}

/// Static capabilities.
pub fn capabilities() -> GatewayCapabilities {
    GatewayCapabilities {
        refunds: true,
        partial_refunds: true,
        // With `vault_enabled` (see [`PaypalGateway::capabilities`]).
        stored_payment_methods: true,
        installments: false,
        offline: false,
        currencies: CURRENCIES
            .iter()
            .filter_map(|c| Currency::new(c).ok())
            .collect(),
    }
}

/// Vault token of a captured order saved with `store_in_vault`, if PayPal vaulted it.
fn vault_token(order: &Value) -> Option<SecretString> {
    let vault = &order["payment_source"]["paypal"]["attributes"]["vault"];
    if vault["status"].as_str() != Some("VAULTED") {
        return None;
    }
    json_id(&vault["id"])
        .filter(|id| id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(SecretString::from)
}

#[derive(Debug)]
struct CachedToken {
    value: SecretString,
    expires_at: Instant,
}

/// The PayPal gateway.
#[derive(Debug)]
pub struct PaypalGateway {
    settings: PaypalSettings,
    http: reqwest::Client,
    returns: ReturnSigner,
    token: Mutex<Option<CachedToken>>,
}

fn paypal_error(response: &Response, json: &Value) -> IntegrationError {
    let detail = &json["details"][0];
    let mut message = detail["description"]
        .as_str()
        .or_else(|| json["message"].as_str())
        .or_else(|| json["error_description"].as_str())
        .unwrap_or("PayPal request failed")
        .to_owned();
    if let Some(debug_id) = json["debug_id"].as_str() {
        message = format!("{message} (debug_id {debug_id})");
    }
    let code = detail["issue"]
        .as_str()
        .or_else(|| json["name"].as_str())
        .or_else(|| json["error"].as_str())
        .map(str::to_owned);
    common::status_error(response.status, response.retry_after, message, code)
}

fn issue(json: &Value) -> Option<&str> {
    json["details"][0]["issue"].as_str()
}

fn rfc3339(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn amount_of(value: &Value) -> Option<Money> {
    let currency = value["currency_code"]
        .as_str()
        .and_then(|c| Currency::new(c).ok())?;
    json_money(&value["value"], currency)
}

fn capture_kind(status: &str) -> PaymentEventKind {
    match status {
        "COMPLETED" | "REFUNDED" | "PARTIALLY_REFUNDED" => PaymentEventKind::Paid,
        "PENDING" => PaymentEventKind::Pending,
        _ => PaymentEventKind::Failed,
    }
}

fn capture_event(
    capture: &Value,
    kind: PaymentEventKind,
    raw: Value,
) -> Result<PaymentEvent, IntegrationError> {
    let id = json_id(&capture["id"])
        .ok_or_else(|| IntegrationError::Transport("PayPal capture has no id".into()))?;
    let amount = amount_of(&capture["amount"])
        .ok_or_else(|| IntegrationError::Transport("PayPal capture has no amount".into()))?;
    let message = (kind != PaymentEventKind::Paid).then(|| {
        capture["status_details"]["reason"]
            .as_str()
            .or_else(|| capture["status"].as_str())
            .unwrap_or("not completed")
            .to_owned()
    });
    Ok(PaymentEvent {
        kind,
        event_id: Some(format!("paypal:{id}:{kind}")),
        gateway_txn_id: id,
        invoice_ref: json_id(&capture["custom_id"]).or_else(|| json_id(&capture["invoice_id"])),
        amount,
        fee: amount_of(&capture["seller_receivable_breakdown"]["paypal_fee"]),
        occurred_at: rfc3339(&capture["update_time"]).or_else(|| rfc3339(&capture["create_time"])),
        message,
        payment_method_ref: None,
        gateway_refund_id: None,
        raw,
    })
}

fn refund_event(refund: &Value, raw: Value) -> Result<PaymentEvent, IntegrationError> {
    let refund_id = json_id(&refund["id"])
        .ok_or_else(|| IntegrationError::Transport("PayPal refund has no id".into()))?;
    // `links[rel=up]` → …/v2/payments/captures/{capture_id}
    let capture_id = refund["links"].as_array().and_then(|links| {
        links.iter().find_map(|l| {
            let href = l["href"].as_str()?;
            (l["rel"].as_str() == Some("up") && href.contains("/captures/"))
                .then(|| {
                    href.trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .map(str::to_owned)
                })
                .flatten()
        })
    });
    Ok(PaymentEvent {
        kind: PaymentEventKind::Refunded,
        event_id: Some(format!("paypal:refund:{refund_id}")),
        gateway_txn_id: capture_id.unwrap_or_else(|| refund_id.clone()),
        invoice_ref: json_id(&refund["custom_id"]).or_else(|| json_id(&refund["invoice_id"])),
        amount: amount_of(&refund["amount"])
            .ok_or_else(|| IntegrationError::Transport("PayPal refund has no amount".into()))?,
        fee: None,
        occurred_at: rfc3339(&refund["create_time"]),
        message: refund["note_to_payer"].as_str().map(str::to_owned),
        payment_method_ref: None,
        gateway_refund_id: Some(refund_id),
        raw,
    })
}

fn dispute_event(dispute: &Value, raw: Value) -> Result<PaymentEvent, IntegrationError> {
    let dispute_id = json_id(&dispute["dispute_id"])
        .ok_or_else(|| IntegrationError::Transport("PayPal dispute has no id".into()))?;
    let tx = &dispute["disputed_transactions"][0];
    Ok(PaymentEvent {
        kind: PaymentEventKind::Chargeback,
        event_id: Some(format!("paypal:dispute:{dispute_id}")),
        gateway_txn_id: json_id(&tx["seller_transaction_id"]).unwrap_or_else(|| dispute_id.clone()),
        invoice_ref: json_id(&tx["custom"]).or_else(|| json_id(&tx["invoice_number"])),
        amount: amount_of(&dispute["dispute_amount"])
            .ok_or_else(|| IntegrationError::Transport("PayPal dispute has no amount".into()))?,
        fee: None,
        occurred_at: rfc3339(&dispute["create_time"]),
        message: dispute["reason"].as_str().map(|r| format!("dispute: {r}")),
        payment_method_ref: None,
        gateway_refund_id: None,
        raw,
    })
}

enum CaptureOutcome {
    Order(Value),
    Declined(Value, String),
}

impl PaypalGateway {
    /// Creates the gateway.
    pub fn new(settings: PaypalSettings) -> Result<Self, IntegrationError> {
        let http = common::http_client(settings.timeout)?;
        let returns = ReturnSigner::new(settings.client_secret.expose_str().as_bytes(), "paypal");
        Ok(Self {
            settings,
            http,
            returns,
            token: Mutex::new(None),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.settings.base_url.trim_end_matches('/'))
    }

    async fn access_token(&self, force_refresh: bool) -> Result<SecretString, IntegrationError> {
        let mut cached = self.token.lock().await;
        if !force_refresh
            && let Some(token) = cached.as_ref()
            && token.expires_at > Instant::now()
        {
            return Ok(token.value.clone());
        }
        let rb = self
            .http
            .post(self.url("/v1/oauth2/token"))
            .basic_auth(
                &self.settings.client_id,
                Some(self.settings.client_secret.expose_str()),
            )
            .header(http::header::ACCEPT, "application/json")
            .form(&[("grant_type", "client_credentials")]);
        let response = common::send(rb).await?;
        let json = response.json()?;
        if !response.is_success() {
            return Err(paypal_error(&response, &json));
        }
        let value = json["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                IntegrationError::Transport("PayPal OAuth response has no token".into())
            })?;
        let lifetime = json["expires_in"].as_u64().unwrap_or(300);
        let token = SecretString::from(value);
        *cached = Some(CachedToken {
            value: token.clone(),
            // Refresh a minute early so in-flight calls never carry an expired token.
            expires_at: Instant::now() + Duration::from_secs(lifetime.saturating_sub(60).max(30)),
        });
        Ok(token)
    }

    /// Authenticated JSON call; retries once with a fresh token on 401.
    async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<String>,
        request_id: Option<String>,
    ) -> Result<(Response, Value), IntegrationError> {
        let mut refreshed = false;
        loop {
            let token = self.access_token(refreshed).await?;
            let mut rb = self
                .http
                .request(method.clone(), self.url(path))
                .bearer_auth(token.expose_str())
                .header(http::header::ACCEPT, "application/json");
            if let Some(id) = &request_id {
                rb = rb.header("PayPal-Request-Id", truncate(id, 108));
            }
            if let Some(b) = &body {
                rb = rb
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header("Prefer", "return=representation")
                    .body(b.clone());
            }
            let response = common::send(rb).await?;
            if response.status == 401 && !refreshed {
                refreshed = true;
                continue;
            }
            let json = if response.body.is_empty() {
                Value::Null
            } else {
                response.json()?
            };
            return Ok((response, json));
        }
    }

    async fn get_order(&self, order_id: &str) -> Result<Value, IntegrationError> {
        let (response, json) = self
            .api(
                reqwest::Method::GET,
                &format!("/v2/checkout/orders/{order_id}"),
                None,
                None,
            )
            .await?;
        if response.is_success() {
            Ok(json)
        } else {
            Err(paypal_error(&response, &json))
        }
    }

    async fn capture(&self, order_id: &str) -> Result<CaptureOutcome, IntegrationError> {
        let (response, json) = self
            .api(
                reqwest::Method::POST,
                &format!("/v2/checkout/orders/{order_id}/capture"),
                Some("{}".to_owned()),
                Some(format!("hos-capture-{order_id}")),
            )
            .await?;
        if response.is_success() {
            return Ok(CaptureOutcome::Order(json));
        }
        match (response.status, issue(&json)) {
            (422, Some("ORDER_ALREADY_CAPTURED")) => {
                Ok(CaptureOutcome::Order(self.get_order(order_id).await?))
            }
            (
                422,
                Some(
                    reason @ ("INSTRUMENT_DECLINED"
                    | "PAYER_ACTION_REQUIRED"
                    | "TRANSACTION_REFUSED"),
                ),
            ) => {
                let order = self.get_order(order_id).await.unwrap_or(Value::Null);
                Ok(CaptureOutcome::Declined(order, reason.to_owned()))
            }
            (404, _) | (422, Some("ORDER_NOT_APPROVED")) => Err(auth(format!(
                "PayPal order {order_id} is unknown or not approved"
            ))),
            _ => Err(paypal_error(&response, &json)),
        }
    }

    async fn handle_return(
        &self,
        pairs: &[(String, String)],
    ) -> Result<WebhookOutcome, IntegrationError> {
        let targets = self
            .returns
            .verify(pairs)
            .ok_or_else(|| auth("PayPal return without a valid HostingOS return signature"))?;
        let order_id = pairs
            .iter()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.trim().to_owned())
            .filter(|t| is_alphanumeric(t) && t.len() <= 64)
            .ok_or_else(|| auth("PayPal return without a valid order token"))?;
        let event = match self.capture(&order_id).await? {
            CaptureOutcome::Order(order) => {
                let capture = &order["purchase_units"][0]["payments"]["captures"][0];
                if !capture.is_object() {
                    return Err(IntegrationError::Transport(
                        "PayPal order has no capture".into(),
                    ));
                }
                let kind = capture_kind(capture["status"].as_str().unwrap_or_default());
                let mut event = capture_event(capture, kind, strip_keys(order.clone(), CARD_KEYS))?;
                if kind == PaymentEventKind::Paid
                    && self.settings.vault_enabled
                    && let Some(token) = vault_token(&order)
                {
                    event.payment_method_ref = Some(token);
                    crate::tag_raw(
                        &mut event.raw,
                        crate::RAW_PAYMENT_METHOD_LABEL,
                        Value::from("PayPal"),
                    );
                }
                Some(event)
            }
            CaptureOutcome::Declined(order, reason) => {
                let unit = &order["purchase_units"][0];
                amount_of(&unit["amount"]).map(|amount| PaymentEvent {
                    kind: PaymentEventKind::Failed,
                    event_id: Some(format!("paypal:{order_id}:failed")),
                    gateway_txn_id: order_id.clone(),
                    invoice_ref: json_id(&unit["custom_id"]),
                    amount,
                    fee: None,
                    occurred_at: None,
                    message: Some(reason),
                    payment_method_ref: None,
                    gateway_refund_id: None,
                    raw: strip_keys(order.clone(), CARD_KEYS),
                })
            }
        };
        let succeeded = event
            .as_ref()
            .is_some_and(|e| matches!(e.kind, PaymentEventKind::Paid | PaymentEventKind::Pending));
        Ok(WebhookOutcome {
            events: event.into_iter().collect(),
            reply: WebhookReply::Redirect(if succeeded { targets.ok } else { targets.fail }),
        })
    }

    async fn handle_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let header = |name: &str| {
            request
                .header(name)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| auth(format!("missing {name} header")))
        };
        let auth_algo = header("paypal-auth-algo")?;
        let cert_url = header("paypal-cert-url")?;
        let transmission_id = header("paypal-transmission-id")?;
        let transmission_sig = header("paypal-transmission-sig")?;
        let transmission_time = header("paypal-transmission-time")?;
        if self.settings.webhook_tolerance_secs > 0 {
            let sent = DateTime::parse_from_rfc3339(transmission_time)
                .map_err(|_| auth("PAYPAL-TRANSMISSION-TIME is malformed"))?;
            if (now_unix() - sent.timestamp()).abs() > self.settings.webhook_tolerance_secs {
                return Err(auth(
                    "PayPal webhook transmission time outside the tolerance",
                ));
            }
        }
        let event: Value = serde_json::from_slice(&request.body)
            .map_err(|_| auth("PayPal webhook body is not JSON"))?;
        if !event.is_object() {
            return Err(auth("PayPal webhook body is not a JSON object"));
        }
        let raw_body = request
            .body_str()
            .map_err(|_| auth("PayPal webhook body is not UTF-8"))?;
        // Splice the raw body in verbatim: re-serializing could change bytes PayPal signed.
        let verify_body = format!(
            "{{\"auth_algo\":{},\"cert_url\":{},\"transmission_id\":{},\"transmission_sig\":{},\"transmission_time\":{},\"webhook_id\":{},\"webhook_event\":{}}}",
            Value::from(auth_algo),
            Value::from(cert_url),
            Value::from(transmission_id),
            Value::from(transmission_sig),
            Value::from(transmission_time),
            Value::from(self.settings.webhook_id.as_str()),
            raw_body.trim(),
        );
        let (response, json) = self
            .api(
                reqwest::Method::POST,
                "/v1/notifications/verify-webhook-signature",
                Some(verify_body),
                None,
            )
            .await?;
        if !response.is_success() {
            return Err(match paypal_error(&response, &json) {
                IntegrationError::InvalidInput(m) | IntegrationError::NotFound(m) => {
                    auth(format!("PayPal could not verify the webhook: {m}"))
                }
                other => other,
            });
        }
        if json["verification_status"].as_str() != Some("SUCCESS") {
            return Err(auth("PayPal webhook signature verification failed"));
        }
        let resource = &event["resource"];
        let raw = || strip_keys(event.clone(), CARD_KEYS);
        let event_out = match event["event_type"].as_str().unwrap_or_default() {
            "PAYMENT.CAPTURE.COMPLETED" => {
                Some(capture_event(resource, PaymentEventKind::Paid, raw())?)
            }
            "PAYMENT.CAPTURE.PENDING" => {
                Some(capture_event(resource, PaymentEventKind::Pending, raw())?)
            }
            "PAYMENT.CAPTURE.DENIED" => {
                Some(capture_event(resource, PaymentEventKind::Failed, raw())?)
            }
            "PAYMENT.CAPTURE.REFUNDED" => Some(refund_event(resource, raw())?),
            "CUSTOMER.DISPUTE.CREATED" => Some(dispute_event(resource, raw())?),
            _ => None,
        };
        Ok(WebhookOutcome {
            events: event_out
                .map(|mut e| {
                    e.occurred_at = e.occurred_at.or_else(|| rfc3339(&event["create_time"]));
                    e
                })
                .into_iter()
                .collect(),
            reply: WebhookReply::Empty,
        })
    }
}

fn ensure_currency(currency: Currency) -> Result<(), IntegrationError> {
    if CURRENCIES.contains(&currency.as_str()) {
        Ok(())
    } else {
        Err(IntegrationError::InvalidInput(format!(
            "PayPal does not support {currency}"
        )))
    }
}

fn locale_tag(locale: Option<Locale>) -> Option<&'static str> {
    match locale {
        Some(Locale::Tr) => Some("tr-TR"),
        Some(Locale::En) => Some("en-US"),
        None => None,
    }
}

#[async_trait]
impl PaymentGateway for PaypalGateway {
    fn kind(&self) -> &'static str {
        kinds::PAYPAL
    }

    fn capabilities(&self) -> GatewayCapabilities {
        GatewayCapabilities {
            stored_payment_methods: self.settings.vault_enabled,
            ..capabilities()
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        self.access_token(true).await?;
        Ok(ConnectionInfo {
            server_version: None,
            details: Some(format!(
                "PayPal OAuth token issued ({})",
                self.settings.base_url
            )),
        })
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        ensure_positive(&request.amount)?;
        let currency = request.amount.currency;
        ensure_currency(currency)?;
        common::http_url(&request.cancel_url, "cancel_url")?;
        let return_url = self.returns.signed_url(
            &request.webhook_url,
            &request.return_url,
            &request.cancel_url,
        )?;
        let value = to_paypal_value(&request.amount)?;
        let mut unit = json!({
            "reference_id": truncate(&request.reference, 256),
            "custom_id": truncate(&request.reference, 127),
            "invoice_id": truncate(&request.reference, 127),
            "description": truncate(&request.description, 127),
            "amount": {"currency_code": currency.as_str(), "value": value},
        });
        let exact_lines = request.items.iter().all(|i| {
            i.unit_price
                .round()
                .try_mul(Decimal::from(i.quantity))
                .is_ok_and(|t| t == i.total.round())
        });
        if itemizable(request) && exact_lines {
            let items = request
                .items
                .iter()
                .map(|i| {
                    Ok(json!({
                        "name": truncate(&i.name, 127),
                        "quantity": i.quantity.to_string(),
                        "unit_amount": {"currency_code": currency.as_str(), "value": to_paypal_value(&i.unit_price)?},
                    }))
                })
                .collect::<Result<Vec<Value>, IntegrationError>>()?;
            unit["items"] = Value::Array(items);
            unit["amount"]["breakdown"] =
                json!({"item_total": {"currency_code": currency.as_str(), "value": value}});
        }
        let mut context = json!({
            "return_url": return_url,
            "cancel_url": request.cancel_url,
            "user_action": "PAY_NOW",
            "shipping_preference": "NO_SHIPPING",
        });
        if let Some(tag) = locale_tag(request.locale) {
            context["locale"] = Value::from(tag);
        }
        if let Some(brand) = &self.settings.brand_name {
            context["brand_name"] = Value::from(brand.as_str());
        }
        let mut paypal = json!({"experience_context": context});
        if request.save_payment_method && self.settings.vault_enabled {
            paypal["attributes"] =
                json!({"vault": {"store_in_vault": "ON_SUCCESS", "usage_type": "MERCHANT"}});
        }
        let body = json!({
            "intent": "CAPTURE",
            "purchase_units": [unit],
            "payment_source": {"paypal": paypal},
        });
        let (response, order) = self
            .api(
                reqwest::Method::POST,
                "/v2/checkout/orders",
                Some(body.to_string()),
                Some(format!("hos-order-{}", request.reference)),
            )
            .await?;
        if !response.is_success() {
            return Err(paypal_error(&response, &order));
        }
        let link = order["links"].as_array().and_then(|links| {
            ["payer-action", "approve"].iter().find_map(|rel| {
                links
                    .iter()
                    .find(|l| l["rel"].as_str() == Some(rel))
                    .and_then(|l| l["href"].as_str())
            })
        });
        let url = link.ok_or_else(|| {
            IntegrationError::Transport("PayPal order has no payer-action link".into())
        })?;
        Ok(CheckoutSession::Redirect {
            url: url.to_owned(),
            gateway_ref: json_id(&order["id"]),
            expires_at: None,
        })
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        if request.header("paypal-transmission-id").is_some() {
            return self.handle_webhook(request).await;
        }
        let pairs = request.query_pairs();
        if pairs.iter().any(|(k, _)| k == "token") {
            return self.handle_return(&pairs).await;
        }
        Err(auth(
            "unrecognised PayPal callback (no webhook headers, no order token)",
        ))
    }

    /// Webhooks are verified by PayPal's API; returns (`token=`) locally.
    fn verifies_remotely(&self, request: &WebhookRequest) -> bool {
        request.header("paypal-transmission-id").is_some()
    }

    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        ensure_positive(&request.amount)?;
        ensure_currency(request.amount.currency)?;
        let capture_id = request.gateway_txn_id.trim();
        if !is_alphanumeric(capture_id) {
            return Err(IntegrationError::InvalidInput(
                "invalid PayPal capture id".into(),
            ));
        }
        let mut body = json!({
            "amount": {
                "value": to_paypal_value(&request.amount)?,
                "currency_code": request.amount.currency.as_str(),
            },
        });
        if let Some(reason) = request
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            body["note_to_payer"] = Value::from(truncate(reason, 255));
        }
        let (response, refund) = self
            .api(
                reqwest::Method::POST,
                &format!("/v2/payments/captures/{capture_id}/refund"),
                Some(body.to_string()),
                Some(format!("hos-refund-{}", request.reference)),
            )
            .await?;
        if !response.is_success() {
            return Err(paypal_error(&response, &refund));
        }
        let status = match refund["status"].as_str() {
            Some("COMPLETED") => RefundStatus::Succeeded,
            Some("PENDING") => RefundStatus::Pending,
            _ => RefundStatus::Failed,
        };
        Ok(RefundResult {
            gateway_refund_id: json_id(&refund["id"]),
            status,
            amount: amount_of(&refund["amount"]).unwrap_or(request.amount),
            message: refund["status_details"]["reason"]
                .as_str()
                .map(str::to_owned),
            raw: refund,
        })
    }

    /// Renewal charge of a vaulted PayPal account (`payment_source.paypal.vault_id`).
    async fn charge_stored(
        &self,
        request: &StoredChargeRequest,
    ) -> Result<PaymentEvent, IntegrationError> {
        if !self.settings.vault_enabled {
            return Err(IntegrationError::Unsupported(
                "PayPal vaulting is disabled for this integration",
            ));
        }
        ensure_positive(&request.amount)?;
        ensure_currency(request.amount.currency)?;
        let vault_id = request.payment_method_ref.expose_str().trim();
        if !is_alphanumeric(vault_id) || vault_id.len() > 64 {
            return Err(IntegrationError::InvalidInput(
                "invalid PayPal vault token".into(),
            ));
        }
        let body = json!({
            "intent": "CAPTURE",
            "purchase_units": [{
                "reference_id": truncate(&request.reference, 256),
                "custom_id": truncate(&request.reference, 127),
                "invoice_id": truncate(&request.reference, 127),
                "description": truncate(&request.description, 127),
                "amount": {
                    "currency_code": request.amount.currency.as_str(),
                    "value": to_paypal_value(&request.amount)?,
                },
            }],
            "payment_source": {"paypal": {"vault_id": vault_id}},
        });
        let (response, order) = self
            .api(
                reqwest::Method::POST,
                "/v2/checkout/orders",
                Some(body.to_string()),
                Some(format!("hos-stored-{}", request.reference)),
            )
            .await?;
        if !response.is_success() {
            if response.status == 422
                && let Some(
                    reason @ ("INSTRUMENT_DECLINED"
                    | "PAYER_ACTION_REQUIRED"
                    | "TRANSACTION_REFUSED"
                    | "AGREEMENT_ALREADY_CANCELLED"
                    | "PAYMENT_SOURCE_DECLINED_BY_PROCESSOR"),
                ) = issue(&order)
            {
                return Ok(PaymentEvent {
                    kind: PaymentEventKind::Failed,
                    event_id: Some(format!("paypal:stored:{}:failed", request.reference)),
                    gateway_txn_id: request.reference.clone(),
                    invoice_ref: Some(request.reference.clone()),
                    amount: request.amount,
                    fee: None,
                    occurred_at: None,
                    message: Some(reason.to_owned()),
                    payment_method_ref: None,
                    gateway_refund_id: None,
                    raw: json!({"issue": reason, "debug_id": order["debug_id"]}),
                });
            }
            return Err(paypal_error(&response, &order));
        }
        let capture = &order["purchase_units"][0]["payments"]["captures"][0];
        if !capture.is_object() {
            return Err(IntegrationError::Transport(
                "PayPal vault order has no capture".into(),
            ));
        }
        let kind = capture_kind(capture["status"].as_str().unwrap_or_default());
        let mut event = capture_event(capture, kind, strip_keys(order.clone(), CARD_KEYS))?;
        if event.invoice_ref.is_none() {
            event.invoice_ref = Some(request.reference.clone());
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::ids::InvoiceId;
    use hos_core::integrations::gateway::{CheckoutItem, PaymentCustomer};
    use http::{HeaderMap, HeaderValue};
    use rust_decimal_macros::dec;
    use std::collections::BTreeMap;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server_with_token() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth2/token"))
            .and(header("authorization", "Basic Y2xpZW50OnNlY3JldA==")) // client:secret
            .and(body_string_contains("grant_type=client_credentials"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "A21AA-token", "token_type": "Bearer", "expires_in": 32400
            })))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    fn gateway(server: &MockServer) -> PaypalGateway {
        let mut s = PaypalSettings::new("client", "secret".into(), "WH-ID-1");
        s.base_url = server.uri();
        PaypalGateway::new(s).unwrap()
    }

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "INV42A1".into(),
            amount: Money::new(dec!(30), Currency::USD),
            description: "Invoice #2026-0042".into(),
            customer: PaymentCustomer::default(),
            items: vec![CheckoutItem {
                reference: "1".into(),
                name: "VPS".into(),
                category: None,
                quantity: 3,
                unit_price: Money::new(dec!(10), Currency::USD),
                total: Money::new(dec!(30), Currency::USD),
            }],
            return_url: "https://shop.example/ok".into(),
            cancel_url: "https://shop.example/fail".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: Some(Locale::En),
            save_payment_method: false,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn value_formatting() {
        assert_eq!(
            to_paypal_value(&Money::new(dec!(12.5), Currency::USD)).unwrap(),
            "12.50"
        );
        let huf = Currency::new("HUF").unwrap();
        assert_eq!(
            to_paypal_value(&Money::new(dec!(1500.00), huf)).unwrap(),
            "1500"
        );
        assert!(to_paypal_value(&Money::new(dec!(1500.50), huf)).is_err());
        let jpy = Currency::new("JPY").unwrap();
        assert_eq!(to_paypal_value(&Money::new(dec!(700), jpy)).unwrap(), "700");
    }

    #[tokio::test]
    async fn create_order_with_cached_token() {
        let server = server_with_token().await;
        Mock::given(method("POST"))
            .and(path("/v2/checkout/orders"))
            .and(header("authorization", "Bearer A21AA-token"))
            .and(header("paypal-request-id", "hos-order-INV42A1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "5O190127TN364715T", "status": "PAYER_ACTION_REQUIRED",
                "links": [
                    {"href": "https://api-m.paypal.com/v2/checkout/orders/5O190127TN364715T", "rel": "self", "method": "GET"},
                    {"href": "https://www.paypal.com/checkoutnow?token=5O190127TN364715T", "rel": "payer-action", "method": "GET"}
                ]
            })))
            .expect(2)
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let session = gw.create_checkout(&request()).await.unwrap();
        assert_eq!(
            session,
            CheckoutSession::Redirect {
                url: "https://www.paypal.com/checkoutnow?token=5O190127TN364715T".into(),
                gateway_ref: Some("5O190127TN364715T".into()),
                expires_at: None,
            }
        );
        // Second call reuses the cached OAuth token (`expect(1)` on the token mock).
        gw.create_checkout(&request()).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let order: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(order["intent"], "CAPTURE");
        let unit = &order["purchase_units"][0];
        assert_eq!(unit["custom_id"], "INV42A1");
        assert_eq!(unit["amount"]["value"], "30.00");
        assert_eq!(unit["amount"]["breakdown"]["item_total"]["value"], "30.00");
        assert_eq!(unit["items"][0]["quantity"], "3");
        assert_eq!(unit["items"][0]["unit_amount"]["value"], "10.00");
        let ctx = &order["payment_source"]["paypal"]["experience_context"];
        assert_eq!(ctx["cancel_url"], "https://shop.example/fail");
        assert_eq!(ctx["locale"], "en-US");
        assert_eq!(ctx["shipping_preference"], "NO_SHIPPING");
        let ret = url::Url::parse(ctx["return_url"].as_str().unwrap()).unwrap();
        assert_eq!(ret.path(), "/webhooks/gateway/abc");
        let pairs: Vec<(String, String)> = ret.query_pairs().into_owned().collect();
        assert!(gw.returns.verify(&pairs).is_some());
    }

    fn vault_gateway(server: &MockServer) -> PaypalGateway {
        let mut s = PaypalSettings::new("client", "secret".into(), "WH-ID-1");
        s.base_url = server.uri();
        s.vault_enabled = true;
        PaypalGateway::new(s).unwrap()
    }

    #[tokio::test]
    async fn vault_save_during_purchase_and_token_on_return() {
        let server = server_with_token().await;
        Mock::given(method("POST"))
            .and(path("/v2/checkout/orders"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "9YF83379T2523751N", "status": "PAYER_ACTION_REQUIRED",
                "links": [{"href": "https://www.paypal.com/checkoutnow?token=9YF83379T2523751N", "rel": "payer-action"}]
            })))
            .mount(&server)
            .await;
        // Capture response shaped like PayPal's save-with-purchase sample.
        Mock::given(method("POST"))
            .and(path("/v2/checkout/orders/9YF83379T2523751N/capture"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "9YF83379T2523751N", "status": "COMPLETED",
                "payment_source": {"paypal": {"email_address": "email@example.com", "account_id": "AJM9JTWQJCFTA",
                    "attributes": {"vault": {"id": "nkq2y9g", "customer": {"id": "ROaPMoZUaV"}, "status": "VAULTED"}}}},
                "purchase_units": [{"payments": {"captures": [{
                    "id": "9LY87817BF120310A", "status": "COMPLETED", "custom_id": "INV42A1",
                    "amount": {"currency_code": "USD", "value": "30.00"}
                }]}}]
            })))
            .mount(&server)
            .await;
        let gw = vault_gateway(&server);
        assert!(gw.capabilities().stored_payment_methods);
        assert!(!gateway(&server).capabilities().stored_payment_methods);
        let mut req = request();
        req.save_payment_method = true;
        gw.create_checkout(&req).await.unwrap();
        let order: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[1].body).unwrap();
        assert_eq!(
            order["payment_source"]["paypal"]["attributes"]["vault"],
            json!({"store_in_vault": "ON_SUCCESS", "usage_type": "MERCHANT"})
        );
        let out = gw
            .parse_webhook(&return_request(&gw, "9YF83379T2523751N"))
            .await
            .unwrap();
        let e = &out.events[0];
        assert_eq!(e.kind, PaymentEventKind::Paid);
        assert_eq!(
            e.payment_method_ref
                .as_ref()
                .map(|t| t.expose_str().to_owned()),
            Some("nkq2y9g".to_owned())
        );
        assert_eq!(crate::event_payment_method_label(e), Some("PayPal"));
        assert!(!e.raw.to_string().contains("email@example.com"));
    }

    #[tokio::test]
    async fn vault_charge_stored_success_and_decline() {
        let server = server_with_token().await;
        Mock::given(method("POST"))
            .and(path("/v2/checkout/orders"))
            .and(header("paypal-request-id", "hos-stored-REN1"))
            .and(body_string_contains("\"vault_id\":\"jwgvx42\""))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "4TH21426N05692944", "status": "COMPLETED",
                "payment_source": {"paypal": {"email_address": "email@example.com"}},
                "purchase_units": [{"payments": {"captures": [{
                    "id": "3B017991HX624902V", "status": "COMPLETED", "custom_id": "REN1",
                    "amount": {"currency_code": "USD", "value": "30.00"},
                    "seller_receivable_breakdown": {"paypal_fee": {"currency_code": "USD", "value": "1.20"}}
                }]}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/checkout/orders"))
            .and(header("paypal-request-id", "hos-stored-REN2"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "name": "UNPROCESSABLE_ENTITY", "debug_id": "abc",
                "details": [{"issue": "INSTRUMENT_DECLINED", "description": "declined"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = vault_gateway(&server);
        let stored = |reference: &str| StoredChargeRequest {
            invoice_id: InvoiceId::new(),
            reference: reference.into(),
            amount: Money::new(dec!(30), Currency::USD),
            description: "Renewal".into(),
            customer: PaymentCustomer::default(),
            payment_method_ref: "jwgvx42".into(),
        };
        let paid = gw.charge_stored(&stored("REN1")).await.unwrap();
        assert_eq!(paid.kind, PaymentEventKind::Paid);
        assert_eq!(paid.gateway_txn_id, "3B017991HX624902V");
        assert_eq!(paid.invoice_ref.as_deref(), Some("REN1"));
        assert_eq!(paid.fee, Some(Money::new(dec!(1.20), Currency::USD)));
        let declined = gw.charge_stored(&stored("REN2")).await.unwrap();
        assert_eq!(declined.kind, PaymentEventKind::Failed);
        assert_eq!(declined.message.as_deref(), Some("INSTRUMENT_DECLINED"));
        // Vault disabled: unsupported, nothing sent.
        assert!(matches!(
            gateway(&server).charge_stored(&stored("REN3")).await,
            Err(IntegrationError::Unsupported(_))
        ));
    }

    fn return_request(gw: &PaypalGateway, token: &str) -> WebhookRequest {
        let url = gw
            .returns
            .signed_url(
                "https://shop.example/webhooks/gateway/abc",
                "https://shop.example/ok",
                "https://shop.example/fail",
            )
            .unwrap();
        let query = format!(
            "{}&token={token}&PayerID=PAYER1",
            url::Url::parse(&url).unwrap().query().unwrap()
        );
        WebhookRequest {
            query: Some(query),
            ..Default::default()
        }
    }

    fn captured_order(order: &str, status: &str) -> Value {
        json!({
            "id": order, "status": "COMPLETED",
            "purchase_units": [{"reference_id": "INV42A1", "payments": {"captures": [{
                "id": "3C679366HH908993F", "status": status,
                "amount": {"currency_code": "USD", "value": "30.00"},
                "custom_id": "INV42A1",
                "seller_receivable_breakdown": {"gross_amount": {"currency_code": "USD", "value": "30.00"},
                    "paypal_fee": {"currency_code": "USD", "value": "1.17"}},
                "create_time": "2026-11-16T10:00:00Z"
            }]}}]
        })
    }

    #[tokio::test]
    async fn return_captures_order_and_redirects() {
        let server = server_with_token().await;
        Mock::given(method("POST"))
            .and(path("/v2/checkout/orders/ORDER1/capture"))
            .and(header("paypal-request-id", "hos-capture-ORDER1"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(captured_order("ORDER1", "COMPLETED")),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/checkout/orders/ORDER2/capture"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "name": "UNPROCESSABLE_ENTITY", "details": [{"issue": "ORDER_ALREADY_CAPTURED"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/checkout/orders/ORDER2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(captured_order("ORDER2", "PENDING")),
            )
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let out = gw
            .parse_webhook(&return_request(&gw, "ORDER1"))
            .await
            .unwrap();
        assert_eq!(
            out.reply,
            WebhookReply::Redirect("https://shop.example/ok".into())
        );
        let ev = &out.events[0];
        assert_eq!(ev.kind, PaymentEventKind::Paid);
        assert_eq!(ev.gateway_txn_id, "3C679366HH908993F");
        assert_eq!(
            ev.event_id.as_deref(),
            Some("paypal:3C679366HH908993F:paid")
        );
        assert_eq!(ev.invoice_ref.as_deref(), Some("INV42A1"));
        assert_eq!(ev.amount, Money::new(dec!(30), Currency::USD));
        assert_eq!(ev.fee, Some(Money::new(dec!(1.17), Currency::USD)));

        let out = gw
            .parse_webhook(&return_request(&gw, "ORDER2"))
            .await
            .unwrap();
        assert_eq!(out.events[0].kind, PaymentEventKind::Pending);

        // Forged return signature or junk order id.
        let mut forged = return_request(&gw, "ORDER1");
        forged.query = forged.query.map(|q| q.replace("hos_sig=", "hos_sig=00"));
        assert!(matches!(
            gw.parse_webhook(&forged).await,
            Err(IntegrationError::Auth(_))
        ));
        assert!(matches!(
            gw.parse_webhook(&return_request(&gw, "..%2Fx")).await,
            Err(IntegrationError::Auth(_))
        ));
        assert!(matches!(
            gw.parse_webhook(&WebhookRequest::default()).await,
            Err(IntegrationError::Auth(_))
        ));
    }

    fn webhook(body: &str, time: &str) -> WebhookRequest {
        let mut headers = HeaderMap::new();
        for (k, v) in [
            ("PAYPAL-AUTH-ALGO", "SHA256withRSA"),
            (
                "PAYPAL-CERT-URL",
                "https://api.paypal.com/v1/notifications/certs/CERT-1",
            ),
            (
                "PAYPAL-TRANSMISSION-ID",
                "69cd13f0-d67a-11e5-baa3-778b53f4ae55",
            ),
            (
                "PAYPAL-TRANSMISSION-SIG",
                "lmI95Jx3Y9nhR5SJWlHVIWpg4AgFk7n9bCHSRxbrd8A9zrhdu2rMyFrmz",
            ),
        ] {
            headers.insert(k, HeaderValue::from_static(v));
        }
        headers.insert(
            "PAYPAL-TRANSMISSION-TIME",
            HeaderValue::from_str(time).unwrap(),
        );
        WebhookRequest {
            headers,
            body: body.as_bytes().to_vec(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn webhook_verification_splices_raw_body_and_maps_events() {
        let server = server_with_token().await;
        Mock::given(method("POST"))
            .and(path("/v1/notifications/verify-webhook-signature"))
            .and(body_string_contains("\"webhook_id\":\"WH-ID-1\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"verification_status": "SUCCESS"})),
            )
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let now = Utc::now().to_rfc3339();
        // Deliberately unusual formatting: must reach PayPal byte-for-byte.
        let completed = r#"{"id":"WH-1","event_type":"PAYMENT.CAPTURE.COMPLETED","create_time":"2026-11-16T10:00:00Z",
            "resource":{"id":"CAP1","status":"COMPLETED","amount":{"currency_code":"EUR","value":"10.00"},"custom_id":"INV9",
            "seller_receivable_breakdown":{"paypal_fee":{"currency_code":"EUR","value":"0.64"}}}}"#;
        let out = gw.parse_webhook(&webhook(completed, &now)).await.unwrap();
        assert_eq!(out.reply, WebhookReply::Empty);
        let ev = &out.events[0];
        assert_eq!(
            (ev.kind, ev.gateway_txn_id.as_str()),
            (PaymentEventKind::Paid, "CAP1")
        );
        assert_eq!(ev.event_id.as_deref(), Some("paypal:CAP1:paid"));
        assert_eq!(ev.amount, Money::new(dec!(10), Currency::EUR));
        assert_eq!(ev.fee, Some(Money::new(dec!(0.64), Currency::EUR)));
        assert_eq!(ev.raw["id"], "WH-1");
        let verify = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path() == "/v1/notifications/verify-webhook-signature")
            .unwrap();
        let sent = String::from_utf8(verify.body).unwrap();
        assert!(sent.ends_with(&format!("\"webhook_event\":{completed}}}")));
        let parsed: Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["transmission_time"], now);

        let refunded = json!({"id": "WH-2", "event_type": "PAYMENT.CAPTURE.REFUNDED", "resource": {
            "id": "REF1", "status": "COMPLETED", "amount": {"currency_code": "EUR", "value": "4.00"},
            "custom_id": "INV9", "create_time": "2026-11-16T11:00:00Z",
            "links": [{"rel": "self", "href": "https://api.paypal.com/v2/payments/refunds/REF1"},
                      {"rel": "up", "href": "https://api.paypal.com/v2/payments/captures/CAP1"}]}});
        let ev = &gw
            .parse_webhook(&webhook(&refunded.to_string(), &now))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.kind, PaymentEventKind::Refunded);
        assert_eq!(ev.gateway_txn_id, "CAP1");
        assert_eq!(ev.amount, Money::new(dec!(4), Currency::EUR));
        // Matched with the id returned by the synchronous refund call.
        assert_eq!(ev.gateway_refund_id.as_deref(), Some("REF1"));

        let dispute = json!({"id": "WH-3", "event_type": "CUSTOMER.DISPUTE.CREATED", "resource": {
            "dispute_id": "PP-D-1", "reason": "UNAUTHORISED", "create_time": "2026-11-16T12:00:00Z",
            "dispute_amount": {"currency_code": "EUR", "value": "10.00"},
            "disputed_transactions": [{"seller_transaction_id": "CAP1", "custom": "INV9"}]}});
        let ev = &gw
            .parse_webhook(&webhook(&dispute.to_string(), &now))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.kind, PaymentEventKind::Chargeback);
        assert_eq!(ev.gateway_txn_id, "CAP1");
        assert_eq!(ev.invoice_ref.as_deref(), Some("INV9"));

        let denied = json!({"id": "WH-4", "event_type": "PAYMENT.CAPTURE.DENIED", "resource": {
            "id": "CAP2", "status": "DECLINED", "amount": {"currency_code": "EUR", "value": "10.00"}}});
        let ev = &gw
            .parse_webhook(&webhook(&denied.to_string(), &now))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.kind, PaymentEventKind::Failed);

        let other = json!({"id": "WH-5", "event_type": "CHECKOUT.ORDER.APPROVED", "resource": {}});
        assert!(
            gw.parse_webhook(&webhook(&other.to_string(), &now))
                .await
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[tokio::test]
    async fn webhook_rejections() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "t", "expires_in": 3600})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/notifications/verify-webhook-signature"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"verification_status": "FAILURE"})),
            )
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let body = r#"{"id":"WH-1","event_type":"PAYMENT.CAPTURE.COMPLETED","resource":{}}"#;
        let now = Utc::now().to_rfc3339();
        assert!(matches!(
            gw.parse_webhook(&webhook(body, &now)).await,
            Err(IntegrationError::Auth(_))
        ));
        let old = (Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        assert!(matches!(
            gw.parse_webhook(&webhook(body, &old)).await,
            Err(IntegrationError::Auth(_))
        ));
        let mut missing = webhook(body, &now);
        missing.headers.remove("PAYPAL-TRANSMISSION-SIG");
        assert!(matches!(
            gw.parse_webhook(&missing).await,
            Err(IntegrationError::Auth(_))
        ));
        assert!(matches!(
            gw.parse_webhook(&webhook("not json", &now)).await,
            Err(IntegrationError::Auth(_))
        ));
    }

    #[tokio::test]
    async fn refund_capture_and_token_refresh_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "t", "expires_in": 3600})),
            )
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/payments/captures/CAP1/refund"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_token"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/payments/captures/CAP1/refund"))
            .and(header("paypal-request-id", "hos-refund-RF1"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "REF1", "status": "COMPLETED", "amount": {"currency_code": "USD", "value": "5.00"}
            })))
            .mount(&server)
            .await;
        let result = gateway(&server)
            .refund(&RefundRequest {
                gateway_txn_id: "CAP1".into(),
                amount: Money::new(dec!(5), Currency::USD),
                reason: Some("Plan downgrade".into()),
                reference: "RF1".into(),
                ip: None,
            })
            .await
            .unwrap();
        assert_eq!(result.status, RefundStatus::Succeeded);
        assert_eq!(result.gateway_refund_id.as_deref(), Some("REF1"));
        let last = server.received_requests().await.unwrap().pop().unwrap();
        let sent: Value = serde_json::from_slice(&last.body).unwrap();
        assert_eq!(sent["amount"]["value"], "5.00");
        assert_eq!(sent["note_to_payer"], "Plan downgrade");
    }

    #[tokio::test]
    async fn oauth_failure_is_auth_error_and_try_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth2/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "invalid_client", "error_description": "Client Authentication failed"
            })))
            .mount(&server)
            .await;
        let gw = gateway(&server);
        assert!(matches!(
            gw.test_connection().await,
            Err(IntegrationError::Auth(_))
        ));
        let mut req = request();
        req.amount = Money::new(dec!(30), Currency::TRY);
        assert!(matches!(
            gw.create_checkout(&req).await,
            Err(IntegrationError::InvalidInput(_))
        ));
    }
}
