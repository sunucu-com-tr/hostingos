//! Stripe: hosted Checkout Sessions, signed webhooks, refunds and off-session
//! charges of payment methods saved during Checkout.
//!
//! * Checkout: `POST /v1/checkout/sessions` (form-encoded, `mode=payment`,
//!   `client_reference_id` = [`CheckoutRequest::reference`], metadata on both the
//!   session and its PaymentIntent, `Idempotency-Key`). Card data stays on
//!   Stripe's page (PCI SAQ-A).
//! * Webhooks: `Stripe-Signature: t=…,v1=…` = HMAC-SHA256(`"{t}.{raw body}"`,
//!   endpoint secret), constant-time compare, ±5 min tolerance.
//! * Amounts use Stripe's own minor-unit rules ([`to_stripe_amount`]): its
//!   zero-decimal list, ISK/UGX as two-decimal whole units, ISO exponent otherwise.
//! * `PaymentEvent::gateway_txn_id` is the PaymentIntent id (`pi_…`), which is
//!   what [`PaymentGateway::refund`] needs.
//! * Stored methods: Checkout with `payment_intent_data[setup_future_usage]=off_session`;
//!   `payment_method_ref` = `"{cus_…}:{pm_…}"`, consumed by `charge_stored`. The
//!   card brand / last four of the saved method travel to billing as a display
//!   label ([`RAW_PAYMENT_METHOD_LABEL`](crate::RAW_PAYMENT_METHOD_LABEL)).
//! * Webhook secret rotation (docs.stripe.com/webhooks, "Roll endpoint
//!   secrets", read 2026-11-18): when a secret is rolled with a delay, Stripe
//!   keeps signing with the previous secret too (up to 7 days) and sends one
//!   `v1` per active secret. `webhook_secret_previous` is accepted as well
//!   until it is cleared, so the rotation needs no downtime.
//! * Wallets: Apple Pay, Google Pay and Link are offered by Checkout itself
//!   (Dashboard › Payment methods; Apple Pay needs the domain verified there);
//!   no API parameter is needed — the session uses dynamic payment methods.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hos_core::crypto;
use hos_core::domain::{IntegrationCategory, Locale};
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{
    CardSetupRequest, CheckoutRequest, CheckoutSession, GatewayCapabilities, PaymentEvent,
    PaymentEventKind, PaymentGateway, RefundRequest, RefundResult, RefundStatus,
    StoredChargeRequest, WebhookOutcome, WebhookReply, WebhookRequest,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, kinds,
};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::Value;

use crate::common::{
    self, DEFAULT_TIMEOUT, Mode, Response, auth, ensure_positive, itemizable, now_unix, strip_keys,
    truncate,
};

/// Card details removed from `PaymentEvent::raw` (charge `payment_method_details`,
/// legacy `source`, nested `card` objects).
const CARD_KEYS: &[&str] = &["payment_method_details", "source", "card"];
/// Checkout `expires_at` window (30 min – 24 h after creation), with a minute of
/// margin on both ends for clock skew and request latency.
const MIN_EXPIRY_SECS: i64 = 31 * 60;
const MAX_EXPIRY_SECS: i64 = 24 * 3600 - 60;

/// Production API base URL.
pub const API_BASE: &str = "https://api.stripe.com";
/// Default webhook timestamp tolerance (Stripe libraries use 5 minutes).
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

/// Stripe's zero-decimal currencies (docs.stripe.com/currencies#zero-decimal).
const ZERO_DECIMAL: &[&str] = &[
    "BIF", "CLP", "DJF", "GNF", "JPY", "KMF", "KRW", "MGA", "PYG", "RWF", "VND", "VUV", "XAF",
    "XOF", "XPF",
];
/// Zero-decimal in ISO 4217 but sent as two-decimal values whose fraction is
/// always `00` (Stripe "special cases": ISK, UGX).
const WHOLE_UNITS_TWO_DECIMAL: &[&str] = &["ISK", "UGX"];

/// Metadata keys set by HostingOS.
const META_REFERENCE: &str = "hos_reference";
const META_INVOICE: &str = "hos_invoice_id";
const META_SAVE_PM: &str = "hos_save_pm";
const META_SOURCE: &str = "hos_source";
/// Session metadata flag of a card setup (`mode=setup`) started by HostingOS.
const META_SETUP: &str = "hos_setup";
const SOURCE_STORED: &str = "charge_stored";

fn stripe_exponent(currency: Currency) -> u32 {
    let code = currency.as_str();
    if ZERO_DECIMAL.contains(&code) {
        0
    } else if WHOLE_UNITS_TWO_DECIMAL.contains(&code) {
        2
    } else {
        currency.minor_units()
    }
}

/// Converts to the integer `amount` Stripe expects, rejecting values that
/// Stripe cannot represent (e.g. `10.50 MGA`, `5.5 ISK`) instead of silently
/// rounding them.
pub fn to_stripe_amount(money: &Money) -> Result<i64, IntegrationError> {
    let rounded = money.round();
    let code = rounded.currency.as_str();
    let whole = ZERO_DECIMAL.contains(&code) || WHOLE_UNITS_TWO_DECIMAL.contains(&code);
    if !whole {
        return rounded
            .to_minor_units()
            .map_err(|_| IntegrationError::InvalidInput("amount is out of range".into()));
    }
    if !rounded.amount.fract().is_zero() {
        return Err(IntegrationError::InvalidInput(format!(
            "Stripe only accepts whole amounts in {code}"
        )));
    }
    let units = rounded
        .amount
        .to_i64()
        .ok_or_else(|| IntegrationError::InvalidInput("amount is out of range".into()))?;
    if WHOLE_UNITS_TWO_DECIMAL.contains(&code) {
        units
            .checked_mul(100)
            .ok_or_else(|| IntegrationError::InvalidInput("amount is out of range".into()))
    } else {
        Ok(units)
    }
}

/// Inverse of [`to_stripe_amount`] for amounts reported by Stripe.
pub fn from_stripe_amount(units: i64, currency: Currency) -> Money {
    Money::new(Decimal::new(units, stripe_exponent(currency)), currency)
}

/// Builds a `Stripe-Signature` header value (`t=…,v1=…`) for `payload`.
/// Used by tests and local tooling; Stripe signs real deliveries.
pub fn sign_payload(payload: &[u8], secret: &[u8], timestamp: i64) -> String {
    let mut signed = format!("{timestamp}.").into_bytes();
    signed.extend_from_slice(payload);
    format!(
        "t={timestamp},v1={}",
        crypto::hmac_sha256_hex(secret, &signed)
    )
}

/// Verifies a `Stripe-Signature` header over the raw body (docs.stripe.com/webhooks,
/// "Verify signatures manually"): any `v1` may match; the timestamp must be
/// within `tolerance_secs` of `now` (`0` disables the check — don't).
/// Returns the signed timestamp. Every failure is [`IntegrationError::Auth`].
pub fn verify_signature(
    payload: &[u8],
    header: &str,
    secret: &[u8],
    now: i64,
    tolerance_secs: i64,
) -> Result<i64, IntegrationError> {
    let mut timestamp: Option<&str> = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        match key {
            "t" => timestamp = Some(value),
            "v1" => signatures.push(value),
            _ => {}
        }
    }
    let t_str = timestamp.ok_or_else(|| auth("Stripe-Signature header has no timestamp"))?;
    let t: i64 = t_str
        .parse()
        .map_err(|_| auth("Stripe-Signature timestamp is malformed"))?;
    if signatures.is_empty() {
        return Err(auth("Stripe-Signature header has no v1 signature"));
    }
    let mut signed = Vec::with_capacity(t_str.len() + 1 + payload.len());
    signed.extend_from_slice(t_str.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(payload);
    if !signatures
        .iter()
        .any(|sig| crypto::verify_hmac_sha256_hex(secret, &signed, sig))
    {
        return Err(auth("Stripe webhook signature mismatch"));
    }
    if tolerance_secs > 0 && (now - t).abs() > tolerance_secs {
        return Err(auth("Stripe webhook timestamp outside the tolerance"));
    }
    Ok(t)
}

/// Stripe settings (from `integrations.config` / `secrets`).
#[derive(Debug, Clone)]
pub struct StripeSettings {
    /// Secret (`sk_…`) or restricted (`rk_…`) API key.
    pub secret_key: SecretString,
    /// Webhook endpoint signing secret (`whsec_…`).
    pub webhook_secret: SecretString,
    /// The secret before the last roll, still accepted while set.
    pub webhook_secret_previous: Option<SecretString>,
    /// `Stripe-Version` header; `None` = account default.
    pub api_version: Option<String>,
    /// Webhook timestamp tolerance in seconds.
    pub webhook_tolerance_secs: i64,
    /// Checkout Session lifetime (30–1440 minutes); `None` = Stripe default (24 h).
    pub session_expiry_minutes: Option<u64>,
    /// API base URL (overridable for tests).
    pub api_base: String,
    /// HTTP timeout.
    pub timeout: Duration,
}

impl StripeSettings {
    /// Settings with defaults.
    pub fn new(secret_key: SecretString, webhook_secret: SecretString) -> Self {
        Self {
            secret_key,
            webhook_secret,
            webhook_secret_previous: None,
            api_version: None,
            webhook_tolerance_secs: DEFAULT_TOLERANCE_SECS,
            session_expiry_minutes: None,
            api_base: API_BASE.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Reads and validates settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let mode = Mode::from_config(&c)?;
        let mut settings = Self::new(s.secret("secret_key")?, s.secret("webhook_secret")?);
        let key = settings.secret_key.expose_str();
        if !(key.starts_with("sk_") || key.starts_with("rk_")) {
            return Err(IntegrationError::Config(
                "secrets.secret_key must be a Stripe secret (sk_) or restricted (rk_) key".into(),
            ));
        }
        if mode.is_live() != settings.is_live() {
            return Err(IntegrationError::Config(
                "config.mode does not match the secrets.secret_key test/live prefix".into(),
            ));
        }
        settings.webhook_secret_previous = s
            .opt_secret("webhook_secret_previous")?
            .filter(|p| !p.expose_str().trim().is_empty());
        if settings.webhook_secret.expose_str().trim().is_empty() {
            return Err(IntegrationError::Config(
                "secrets.webhook_secret is required".into(),
            ));
        }
        settings.api_version = c.opt_str("api_version")?.map(str::to_owned);
        settings.webhook_tolerance_secs = i64::try_from(c.u64_or(
            "webhook_tolerance_secs",
            DEFAULT_TOLERANCE_SECS.unsigned_abs(),
        )?)
        .map_err(|_| {
            IntegrationError::Config("config.webhook_tolerance_secs is too large".into())
        })?;
        settings.session_expiry_minutes = c.opt_u64("session_expiry_minutes")?;
        if let Some(m) = settings.session_expiry_minutes
            && !(30..=1440).contains(&m)
        {
            return Err(IntegrationError::Config(
                "config.session_expiry_minutes must be between 30 and 1440".into(),
            ));
        }
        Ok(settings)
    }

    /// Whether the key is a live key.
    pub fn is_live(&self) -> bool {
        let key = self.secret_key.expose_str();
        key.starts_with("sk_live_") || key.starts_with("rk_live_")
    }
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::STRIPE, "Stripe", IntegrationCategory::Gateway)
        .description(
            "Stripe Checkout (hosted page; card data never touches HostingOS). Add a webhook \
             endpoint at /webhooks/gateway/{integration_id} with events checkout.session.completed, \
             checkout.session.async_payment_succeeded, checkout.session.async_payment_failed, \
             payment_intent.payment_failed, payment_intent.succeeded, charge.refunded and \
             charge.dispute.created. Currencies: every Stripe-supported currency.",
        )
        .docs_url("https://docs.stripe.com/api/checkout/sessions/create")
        .config_field(
            common::mode_field().help("Must match the API key prefix (sk_test_ / sk_live_)."),
        )
        .config_field(
            FieldSpec::text("api_version", "API version (Stripe-Version)")
                .placeholder("2026-10-28.endive")
                .help("Optional. Pin it and use the same version on the webhook endpoint."),
        )
        .config_field(
            FieldSpec::number("session_expiry_minutes", "Checkout session lifetime (minutes)")
                .help("30–1440. Empty = Stripe default (24 hours)."),
        )
        .config_field(
            FieldSpec::number("webhook_tolerance_secs", "Webhook timestamp tolerance (seconds)")
                .default_value(DEFAULT_TOLERANCE_SECS),
        )
        .secret_field(
            FieldSpec::password("secret_key", "Secret key")
                .required()
                .placeholder("sk_live_…"),
        )
        .secret_field(
            FieldSpec::password("webhook_secret", "Webhook signing secret")
                .required()
                .placeholder("whsec_…"),
        )
        .secret_field(
            FieldSpec::password("webhook_secret_previous", "Previous webhook signing secret")
                .placeholder("whsec_…")
                .help(
                    "While rotating: the old secret, still accepted. Roll the secret in Stripe \
                     with a delay, put the new one above and the old one here, clear this \
                     field when the delay has passed.",
                ),
        )
}

/// Static capabilities.
pub fn capabilities() -> GatewayCapabilities {
    GatewayCapabilities {
        refunds: true,
        partial_refunds: true,
        stored_payment_methods: true,
        installments: false,
        offline: false,
        currencies: Vec::new(),
    }
}

/// The Stripe gateway.
#[derive(Debug)]
pub struct StripeGateway {
    settings: StripeSettings,
    http: reqwest::Client,
}

impl StripeGateway {
    /// Creates the gateway.
    pub fn new(settings: StripeSettings) -> Result<Self, IntegrationError> {
        let http = common::http_client(settings.timeout)?;
        Ok(Self { settings, http })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.settings.api_base.trim_end_matches('/'));
        let mut rb = self
            .http
            .request(method, url)
            .bearer_auth(self.settings.secret_key.expose_str());
        if let Some(v) = &self.settings.api_version {
            rb = rb.header("Stripe-Version", v);
        }
        rb
    }

    async fn call(&self, rb: reqwest::RequestBuilder) -> Result<Value, IntegrationError> {
        let response = common::send(rb).await?;
        let json = response.json()?;
        if response.is_success() {
            Ok(json)
        } else {
            Err(stripe_error(&response, &json))
        }
    }

    async fn fetch_intent(&self, id: &str) -> Result<Value, IntegrationError> {
        let rb = self
            .request(reqwest::Method::GET, &format!("/v1/payment_intents/{id}"))
            .query(&[("expand[]", "latest_charge.balance_transaction")]);
        self.call(rb).await
    }

    async fn map_event(&self, event: &Value) -> Result<Vec<PaymentEvent>, IntegrationError> {
        let event_type = event["type"].as_str().unwrap_or_default();
        let object = &event["data"]["object"];
        let raw = strip_keys(event.clone(), CARD_KEYS);
        let ctx = EventCtx {
            event_id: event["id"].as_str().map(str::to_owned),
            occurred_at: event["created"]
                .as_i64()
                .and_then(|t| DateTime::from_timestamp(t, 0)),
            raw: &raw,
        };
        let mapped = match event_type {
            "checkout.session.completed"
            | "checkout.session.async_payment_succeeded"
            | "checkout.session.async_payment_failed" => {
                self.session_event(event_type, object, &ctx).await?
            }
            "payment_intent.payment_failed" => Some(intent_event(object, &ctx, None)?),
            "payment_intent.succeeded"
                if object["metadata"][META_SOURCE].as_str() == Some(SOURCE_STORED) =>
            {
                Some(intent_event(object, &ctx, None)?)
            }
            "charge.refunded" => {
                let mut ev = refund_event(object, &event["data"]["previous_attributes"], &ctx)?;
                if ev.gateway_refund_id.is_none() {
                    self.lookup_refund(object, &mut ev).await;
                }
                Some(ev)
            }
            "charge.dispute.created" => Some(dispute_event(object, &ctx)?),
            _ => None,
        };
        Ok(mapped.into_iter().collect())
    }

    /// Recent API versions no longer embed `charge.refunds`: look the refund
    /// of a `charge.refunded` event up (best effort — without it billing falls
    /// back to amount matching).
    async fn lookup_refund(&self, charge: &Value, event: &mut PaymentEvent) {
        let Some(charge_id) =
            id_of(charge).filter(|c| c.starts_with("ch_") || c.starts_with("py_"))
        else {
            return;
        };
        let Ok(units) = to_stripe_amount(&event.amount) else {
            return;
        };
        let rb = self
            .request(reqwest::Method::GET, "/v1/refunds")
            .query(&[("charge", charge_id.as_str()), ("limit", "20")]);
        match self.call(rb).await {
            Ok(list) => {
                let refunds = list["data"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if let Some(r) = pick_refund(refunds, &[], units) {
                    tag_refund_of(event, r);
                }
            }
            Err(err) => {
                tracing::warn!(charge = %charge_id, error = %err, "Stripe: refund lookup failed");
            }
        }
    }

    /// A completed `mode=setup` session of ours → [`PaymentEventKind::MethodSaved`]
    /// with `"{customer}:{payment_method}"` read from the SetupIntent.
    async fn setup_event(
        &self,
        event_type: &str,
        session: &Value,
        ctx: &EventCtx<'_>,
    ) -> Result<Option<PaymentEvent>, IntegrationError> {
        if event_type != "checkout.session.completed"
            || session["metadata"][META_SETUP].as_str() != Some("1")
        {
            return Ok(None);
        }
        let Some(seti) = id_of(&session["setup_intent"]) else {
            return Ok(None);
        };
        let rb = self
            .request(reqwest::Method::GET, &format!("/v1/setup_intents/{seti}"))
            .query(&[("expand[]", "payment_method")]);
        // An error lets Stripe redeliver; the method would be lost otherwise.
        let intent = self.call(rb).await?;
        if intent["status"].as_str() != Some("succeeded") {
            return Ok(None);
        }
        let customer = id_of(&intent["customer"]).or_else(|| id_of(&session["customer"]));
        let method = id_of(&intent["payment_method"]);
        let (Some(customer), Some(method)) = (customer, method) else {
            tracing::warn!(setup_intent = %seti, "Stripe: setup without customer or payment method");
            return Ok(None);
        };
        let currency = currency_of(&session["currency"])?;
        let mut raw = ctx.raw.clone();
        if let Some(label) = payment_method_label(&intent["payment_method"]) {
            crate::tag_raw(
                &mut raw,
                crate::RAW_PAYMENT_METHOD_LABEL,
                Value::String(label),
            );
        }
        Ok(Some(PaymentEvent {
            kind: PaymentEventKind::MethodSaved,
            event_id: ctx.event_id.clone(),
            gateway_txn_id: seti,
            invoice_ref: session["client_reference_id"]
                .as_str()
                .or_else(|| session["metadata"][META_REFERENCE].as_str())
                .map(str::to_owned),
            amount: Money::zero(currency),
            fee: None,
            occurred_at: ctx.occurred_at,
            message: None,
            payment_method_ref: Some(SecretString::from(format!("{customer}:{method}"))),
            gateway_refund_id: None,
            raw,
        }))
    }

    /// Creates a Stripe customer for a card setup (idempotent per reference).
    async fn create_customer(
        &self,
        customer: &hos_core::integrations::gateway::PaymentCustomer,
        reference: &str,
    ) -> Result<String, IntegrationError> {
        let mut form: Vec<(String, String)> = Vec::new();
        if !customer.email.trim().is_empty() {
            form.push(("email".into(), customer.email.trim().into()));
        }
        if !customer.name.trim().is_empty() {
            form.push(("name".into(), truncate(customer.name.trim(), 250)));
        }
        if let Some(c) = customer.client_id {
            form.push(("metadata[hos_client_id]".into(), c.to_string()));
        }
        let rb = self
            .request(reqwest::Method::POST, "/v1/customers")
            .header("Idempotency-Key", format!("hos-customer-{reference}"))
            .form(&form);
        let created = self.call(rb).await?;
        id_of(&created)
            .filter(|id| id.starts_with("cus_"))
            .ok_or_else(|| IntegrationError::Transport("Stripe customer has no id".into()))
    }

    async fn session_event(
        &self,
        event_type: &str,
        session: &Value,
        ctx: &EventCtx<'_>,
    ) -> Result<Option<PaymentEvent>, IntegrationError> {
        if session["mode"].as_str() == Some("setup") {
            return self.setup_event(event_type, session, ctx).await;
        }
        if session["mode"].as_str() != Some("payment") {
            return Ok(None);
        }
        let payment_status = session["payment_status"].as_str();
        let kind = match (event_type, payment_status) {
            ("checkout.session.completed", Some("paid")) => PaymentEventKind::Paid,
            ("checkout.session.completed", Some("unpaid")) => PaymentEventKind::Pending,
            ("checkout.session.async_payment_succeeded", _) => PaymentEventKind::Paid,
            ("checkout.session.async_payment_failed", _) => PaymentEventKind::Failed,
            _ => return Ok(None),
        };
        let currency = currency_of(&session["currency"])?;
        let amount = from_stripe_amount(int_of(&session["amount_total"])?, currency);
        let intent_id = id_of(&session["payment_intent"]);
        let session_id = id_of(session).unwrap_or_default();
        let mut fee = None;
        let mut payment_method_ref = None;
        let mut label = None;
        if kind == PaymentEventKind::Paid
            && let Some(pi) = &intent_id
        {
            let wants_pm = session["metadata"][META_SAVE_PM].as_str() == Some("1");
            match self.fetch_intent(pi).await {
                Ok(intent) => {
                    fee = fee_of(&intent);
                    if wants_pm {
                        payment_method_ref = stored_ref(&intent, session);
                        label = method_label(&intent);
                        if payment_method_ref.is_none() {
                            tracing::warn!(
                                payment_intent = %pi,
                                "Stripe: saved payment method requested but none attached"
                            );
                        }
                    }
                }
                // The stored method would be lost: let Stripe redeliver.
                Err(err) if wants_pm => return Err(err),
                Err(err) => {
                    tracing::warn!(payment_intent = %pi, error = %err, "Stripe: fee lookup failed");
                }
            }
        }
        let mut raw = ctx.raw.clone();
        if let Some(label) = label.filter(|_| payment_method_ref.is_some()) {
            crate::tag_raw(
                &mut raw,
                crate::RAW_PAYMENT_METHOD_LABEL,
                Value::String(label),
            );
        }
        Ok(Some(PaymentEvent {
            kind,
            event_id: ctx.event_id.clone(),
            gateway_txn_id: intent_id.unwrap_or(session_id),
            invoice_ref: session["client_reference_id"]
                .as_str()
                .or_else(|| session["metadata"][META_REFERENCE].as_str())
                .map(str::to_owned),
            amount,
            fee,
            occurred_at: ctx.occurred_at,
            message: (kind == PaymentEventKind::Failed)
                .then(|| "asynchronous payment failed".to_owned()),
            payment_method_ref,
            gateway_refund_id: None,
            raw,
        }))
    }
}

struct EventCtx<'a> {
    event_id: Option<String>,
    occurred_at: Option<DateTime<Utc>>,
    raw: &'a Value,
}

fn stripe_error(response: &Response, body: &Value) -> IntegrationError {
    let err = &body["error"];
    let message = err["message"]
        .as_str()
        .unwrap_or("Stripe request failed")
        .to_owned();
    let code = err["code"]
        .as_str()
        .or_else(|| err["type"].as_str())
        .map(str::to_owned);
    if response.status == 402 {
        return IntegrationError::Remote {
            status: Some(402),
            code,
            message,
        };
    }
    common::status_error(response.status, response.retry_after, message, code)
}

/// Id of an expandable field: `"pi_…"` or `{"id": "pi_…", …}`.
fn id_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(map) => map.get("id").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn currency_of(value: &Value) -> Result<Currency, IntegrationError> {
    value
        .as_str()
        .and_then(|c| Currency::new(c).ok())
        .ok_or_else(|| IntegrationError::Transport("Stripe object has no valid currency".into()))
}

fn int_of(value: &Value) -> Result<i64, IntegrationError> {
    value
        .as_i64()
        .ok_or_else(|| IntegrationError::Transport("Stripe object has no integer amount".into()))
}

/// Fee from an expanded `latest_charge.balance_transaction` (settlement currency).
fn fee_of(intent: &Value) -> Option<Money> {
    let bt = &intent["latest_charge"]["balance_transaction"];
    let currency = bt["currency"]
        .as_str()
        .and_then(|c| Currency::new(c).ok())?;
    Some(from_stripe_amount(bt["fee"].as_i64()?, currency))
}

/// Display label of the method that paid (brand and last four only), from
/// the expanded `latest_charge.payment_method_details`.
fn method_label(intent: &Value) -> Option<String> {
    let details = &intent["latest_charge"]["payment_method_details"];
    let card = &details["card"];
    match (card["brand"].as_str(), card["last4"].as_str()) {
        (Some(brand), Some(last4))
            if last4.len() == 4 && last4.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some(format!(
                "{} •••• {last4}",
                truncate(&brand.to_uppercase(), 20)
            ))
        }
        _ => details["type"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 30)
            .map(str::to_owned),
    }
}

/// Display label of an expanded PaymentMethod (brand and last four only).
fn payment_method_label(pm: &Value) -> Option<String> {
    let card = &pm["card"];
    match (card["brand"].as_str(), card["last4"].as_str()) {
        (Some(brand), Some(last4))
            if last4.len() == 4 && last4.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some(format!(
                "{} •••• {last4}",
                truncate(&brand.to_uppercase(), 20)
            ))
        }
        _ => pm["type"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 30)
            .map(str::to_owned),
    }
}

/// `"{customer}:{payment_method}"` of a PaymentIntent created with `setup_future_usage`.
fn stored_ref(intent: &Value, session: &Value) -> Option<SecretString> {
    let customer = id_of(&intent["customer"]).or_else(|| id_of(&session["customer"]))?;
    let method = id_of(&intent["payment_method"])?;
    Some(SecretString::from(format!("{customer}:{method}")))
}

fn intent_event(
    intent: &Value,
    ctx: &EventCtx<'_>,
    fee: Option<Money>,
) -> Result<PaymentEvent, IntegrationError> {
    let status = intent["status"].as_str().unwrap_or_default();
    let kind = match status {
        "succeeded" => PaymentEventKind::Paid,
        "processing" => PaymentEventKind::Pending,
        _ => PaymentEventKind::Failed,
    };
    let currency = currency_of(&intent["currency"])?;
    let units = if kind == PaymentEventKind::Paid {
        intent["amount_received"]
            .as_i64()
            .unwrap_or(int_of(&intent["amount"])?)
    } else {
        int_of(&intent["amount"])?
    };
    let error = &intent["last_payment_error"];
    let message = (kind == PaymentEventKind::Failed).then(|| {
        let base = error["message"]
            .as_str()
            .unwrap_or(match status {
                "requires_action" => "customer authentication required",
                _ => "payment failed",
            })
            .to_owned();
        match error["decline_code"].as_str().or(error["code"].as_str()) {
            Some(code) => format!("{base} ({code})"),
            None => base,
        }
    });
    Ok(PaymentEvent {
        kind,
        event_id: ctx.event_id.clone(),
        gateway_txn_id: id_of(intent).unwrap_or_default(),
        invoice_ref: intent["metadata"][META_REFERENCE]
            .as_str()
            .map(str::to_owned),
        amount: from_stripe_amount(units, currency),
        fee: fee.or_else(|| fee_of(intent)),
        occurred_at: ctx.occurred_at,
        message,
        payment_method_ref: None,
        gateway_refund_id: None,
        raw: ctx.raw.clone(),
    })
}

fn refund_event(
    charge: &Value,
    previous: &Value,
    ctx: &EventCtx<'_>,
) -> Result<PaymentEvent, IntegrationError> {
    let currency = currency_of(&charge["currency"])?;
    let total = int_of(&charge["amount_refunded"])?;
    // `previous_attributes.amount_refunded` turns the cumulative total into
    // the amount refunded by this event (partial refunds).
    let units = match previous["amount_refunded"].as_i64() {
        Some(before) if total > before => total - before,
        _ => total,
    };
    let mut event = PaymentEvent {
        kind: PaymentEventKind::Refunded,
        event_id: ctx.event_id.clone(),
        gateway_txn_id: id_of(&charge["payment_intent"])
            .or_else(|| id_of(charge))
            .unwrap_or_default(),
        invoice_ref: charge["metadata"][META_REFERENCE]
            .as_str()
            .map(str::to_owned),
        amount: from_stripe_amount(units, currency),
        fee: None,
        occurred_at: ctx.occurred_at,
        message: None,
        payment_method_ref: None,
        gateway_refund_id: None,
        raw: ctx.raw.clone(),
    };
    // The refund this event reports: new in `charge.refunds` compared with
    // `previous_attributes.refunds` (older API versions embed both lists).
    if let Some(list) = charge["refunds"]["data"].as_array() {
        let before: Vec<&str> = previous["refunds"]["data"]
            .as_array()
            .map(|l| l.iter().filter_map(|r| r["id"].as_str()).collect())
            .unwrap_or_default();
        if let Some(r) = pick_refund(list, &before, units) {
            tag_refund_of(&mut event, r);
        }
    }
    Ok(event)
}

/// The refund (newest first, as Stripe lists them) of `units` among those not
/// in `before`; the only candidate when none has that amount.
fn pick_refund<'a>(refunds: &'a [Value], before: &[&str], units: i64) -> Option<&'a Value> {
    let candidates: Vec<&Value> = refunds
        .iter()
        .filter(|r| r["id"].as_str().is_some_and(|id| !before.contains(&id)))
        .filter(|r| !matches!(r["status"].as_str(), Some("failed" | "canceled")))
        .collect();
    candidates
        .iter()
        .find(|r| r["amount"].as_i64() == Some(units))
        .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))
        .copied()
}

/// Records the Stripe refund object a `Refunded` event reports: its id
/// ([`PaymentEvent::gateway_refund_id`]) and the HostingOS reference it
/// carries (in `raw`).
fn tag_refund_of(event: &mut PaymentEvent, refund: &Value) {
    event.gateway_refund_id = refund["id"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    crate::tag_refund_reference(
        &mut event.raw,
        refund["metadata"]["hos_refund_reference"].as_str(),
    );
}

fn dispute_event(dispute: &Value, ctx: &EventCtx<'_>) -> Result<PaymentEvent, IntegrationError> {
    let currency = currency_of(&dispute["currency"])?;
    Ok(PaymentEvent {
        kind: PaymentEventKind::Chargeback,
        event_id: ctx.event_id.clone(),
        gateway_txn_id: id_of(&dispute["payment_intent"])
            .or_else(|| id_of(&dispute["charge"]))
            .unwrap_or_default(),
        invoice_ref: dispute["metadata"][META_REFERENCE]
            .as_str()
            .map(str::to_owned),
        amount: from_stripe_amount(int_of(&dispute["amount"])?, currency),
        fee: None,
        occurred_at: ctx.occurred_at,
        message: dispute["reason"].as_str().map(|r| format!("dispute: {r}")),
        payment_method_ref: None,
        gateway_refund_id: None,
        raw: ctx.raw.clone(),
    })
}

/// Metadata Stripe accepts: ≤ 50 keys, key ≤ 40 chars without brackets, value ≤ 500 chars.
fn metadata_pairs(
    user: &BTreeMap<String, String>,
    ours: &[(&str, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = ours
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    out.extend(
        user.iter()
            .filter(|(k, _)| !k.is_empty() && k.len() <= 40 && !k.contains(['[', ']']))
            .filter(|(k, _)| !ours.iter().any(|(o, _)| o == k))
            .take(50usize.saturating_sub(ours.len()))
            .map(|(k, v)| (k.clone(), truncate(v, 500))),
    );
    out
}

fn locale_code(locale: Option<Locale>) -> &'static str {
    match locale {
        Some(Locale::Tr) => "tr",
        Some(Locale::En) => "en",
        None => "auto",
    }
}

/// Splits `"cus_…:pm_…"` (or a bare `pm_…` plus the customer's gateway id).
fn parse_stored_ref(
    reference: &str,
    customer: Option<&str>,
) -> Result<(String, String), IntegrationError> {
    let (cus, pm) = match reference.split_once(':') {
        Some((c, p)) => (Some(c), p),
        None => (customer, reference),
    };
    match cus {
        Some(c) if !c.is_empty() && !pm.is_empty() => Ok((c.to_owned(), pm.to_owned())),
        _ => Err(IntegrationError::InvalidInput(
            "Stripe stored payment method needs a customer id".into(),
        )),
    }
}

#[async_trait]
impl PaymentGateway for StripeGateway {
    fn kind(&self) -> &'static str {
        kinds::STRIPE
    }

    fn capabilities(&self) -> GatewayCapabilities {
        capabilities()
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let balance = self
            .call(self.request(reqwest::Method::GET, "/v1/balance"))
            .await?;
        let currencies: Vec<String> = balance["available"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|b| b["currency"].as_str().map(str::to_ascii_uppercase))
                    .collect()
            })
            .unwrap_or_default();
        Ok(ConnectionInfo {
            server_version: self.settings.api_version.clone(),
            details: Some(format!(
                "livemode={}, balance currencies: {}",
                balance["livemode"].as_bool().unwrap_or(false),
                currencies.join(", ")
            )),
        })
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        ensure_positive(&request.amount)?;
        if request.reference.is_empty() || request.reference.len() > 200 {
            return Err(IntegrationError::InvalidInput(
                "reference must be 1–200 characters for Stripe".into(),
            ));
        }
        common::http_url(&request.return_url, "return_url")?;
        common::http_url(&request.cancel_url, "cancel_url")?;
        let currency = request.amount.currency.as_str().to_ascii_lowercase();
        let mut form: Vec<(String, String)> = vec![
            ("mode".into(), "payment".into()),
            ("success_url".into(), request.return_url.clone()),
            ("cancel_url".into(), request.cancel_url.clone()),
            ("client_reference_id".into(), request.reference.clone()),
            ("locale".into(), locale_code(request.locale).into()),
            (
                "payment_intent_data[description]".into(),
                truncate(&request.description, 1000),
            ),
        ];

        let lines: Vec<(String, Money)> = if itemizable(request) {
            request
                .items
                .iter()
                .map(|item| {
                    let name = if item.quantity > 1 {
                        format!("{} × {}", item.name, item.quantity)
                    } else {
                        item.name.clone()
                    };
                    (name, item.total)
                })
                .collect()
        } else {
            vec![(request.description.clone(), request.amount)]
        };
        for (i, (name, total)) in lines.iter().enumerate() {
            let p = format!("line_items[{i}]");
            form.push((format!("{p}[price_data][currency]"), currency.clone()));
            form.push((
                format!("{p}[price_data][product_data][name]"),
                truncate(
                    if name.trim().is_empty() {
                        &request.reference
                    } else {
                        name
                    },
                    250,
                ),
            ));
            form.push((
                format!("{p}[price_data][unit_amount]"),
                to_stripe_amount(total)?.to_string(),
            ));
            form.push((format!("{p}[quantity]"), "1".into()));
        }

        match &request.customer.gateway_customer_id {
            Some(customer) if !customer.is_empty() => {
                form.push(("customer".into(), customer.clone()));
            }
            _ => {
                if !request.customer.email.trim().is_empty() {
                    form.push((
                        "customer_email".into(),
                        request.customer.email.trim().into(),
                    ));
                }
                if request.save_payment_method {
                    form.push(("customer_creation".into(), "always".into()));
                }
            }
        }
        let mut ours = vec![
            (META_REFERENCE, request.reference.clone()),
            (META_INVOICE, request.invoice_id.to_string()),
        ];
        if request.save_payment_method {
            form.push((
                "payment_intent_data[setup_future_usage]".into(),
                "off_session".into(),
            ));
            ours.push((META_SAVE_PM, "1".into()));
        }
        for (k, v) in metadata_pairs(&request.metadata, &ours) {
            form.push((format!("metadata[{k}]"), v.clone()));
            form.push((format!("payment_intent_data[metadata][{k}]"), v));
        }
        if let Some(minutes) = self.settings.session_expiry_minutes {
            let lifetime = i64::try_from(minutes)
                .unwrap_or(1440)
                .saturating_mul(60)
                .clamp(MIN_EXPIRY_SECS, MAX_EXPIRY_SECS);
            let expires = now_unix() + lifetime;
            form.push(("expires_at".into(), expires.to_string()));
        }

        let rb = self
            .request(reqwest::Method::POST, "/v1/checkout/sessions")
            .header(
                "Idempotency-Key",
                format!("hos-checkout-{}", request.reference),
            )
            .form(&form);
        let session = self.call(rb).await?;
        let url = session["url"]
            .as_str()
            .ok_or_else(|| IntegrationError::Transport("Stripe session has no url".into()))?;
        Ok(CheckoutSession::Redirect {
            url: url.to_owned(),
            gateway_ref: id_of(&session),
            expires_at: session["expires_at"]
                .as_i64()
                .and_then(|t| DateTime::from_timestamp(t, 0)),
        })
    }

    fn supports_card_setup(&self) -> bool {
        true
    }

    /// Checkout `mode=setup` for an existing or newly created customer, so
    /// the saved method is attached to it and usable by `charge_stored`.
    async fn create_card_setup(
        &self,
        request: &CardSetupRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        if request.reference.is_empty()
            || request.reference.len() > 64
            || !request.reference.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(IntegrationError::InvalidInput(
                "setup reference must be 1–64 alphanumerics".into(),
            ));
        }
        common::http_url(&request.return_url, "return_url")?;
        common::http_url(&request.cancel_url, "cancel_url")?;
        let customer = match request
            .customer
            .gateway_customer_id
            .as_deref()
            .filter(|c| !c.is_empty())
        {
            Some(c) => c.to_owned(),
            None => {
                self.create_customer(&request.customer, &request.reference)
                    .await?
            }
        };
        let mut form: Vec<(String, String)> = vec![
            ("mode".into(), "setup".into()),
            (
                "currency".into(),
                request.currency.as_str().to_ascii_lowercase(),
            ),
            ("customer".into(), customer),
            ("success_url".into(), request.return_url.clone()),
            ("cancel_url".into(), request.cancel_url.clone()),
            ("client_reference_id".into(), request.reference.clone()),
            ("locale".into(), locale_code(request.locale).into()),
            (
                format!("metadata[{META_REFERENCE}]"),
                request.reference.clone(),
            ),
            (format!("metadata[{META_SETUP}]"), "1".into()),
        ];
        if let Some(minutes) = self.settings.session_expiry_minutes {
            let lifetime = i64::try_from(minutes)
                .unwrap_or(1440)
                .saturating_mul(60)
                .clamp(MIN_EXPIRY_SECS, MAX_EXPIRY_SECS);
            form.push(("expires_at".into(), (now_unix() + lifetime).to_string()));
        }
        let rb = self
            .request(reqwest::Method::POST, "/v1/checkout/sessions")
            .header(
                "Idempotency-Key",
                format!("hos-setup-{}", request.reference),
            )
            .form(&form);
        let session = self.call(rb).await?;
        let url = session["url"]
            .as_str()
            .ok_or_else(|| IntegrationError::Transport("Stripe session has no url".into()))?;
        Ok(CheckoutSession::Redirect {
            url: url.to_owned(),
            gateway_ref: id_of(&session),
            expires_at: session["expires_at"]
                .as_i64()
                .and_then(|t| DateTime::from_timestamp(t, 0)),
        })
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let header = request
            .header("stripe-signature")
            .ok_or_else(|| auth("missing Stripe-Signature header"))?;
        let now = now_unix();
        let verify = |secret: &SecretString| {
            verify_signature(
                &request.body,
                header,
                secret.expose_str().as_bytes(),
                now,
                self.settings.webhook_tolerance_secs,
            )
        };
        if let Err(err) = verify(&self.settings.webhook_secret) {
            match &self.settings.webhook_secret_previous {
                Some(previous) => {
                    verify(previous)?;
                }
                None => return Err(err),
            }
        }
        let event: Value = request.json()?;
        if let Some(live) = event["livemode"].as_bool()
            && live != self.settings.is_live()
        {
            tracing::warn!(
                event_id = event["id"].as_str().unwrap_or_default(),
                "Stripe: ignoring event from the other (test/live) mode"
            );
            return Ok(WebhookOutcome::default());
        }
        Ok(WebhookOutcome {
            events: self.map_event(&event).await?,
            reply: WebhookReply::Empty,
        })
    }

    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        ensure_positive(&request.amount)?;
        let txn = request.gateway_txn_id.trim();
        let target = if txn.starts_with("ch_") || txn.starts_with("py_") {
            "charge"
        } else {
            "payment_intent"
        };
        let mut form: Vec<(String, String)> = vec![
            (target.into(), txn.into()),
            (
                "amount".into(),
                to_stripe_amount(&request.amount)?.to_string(),
            ),
            (
                "metadata[hos_refund_reference]".into(),
                truncate(&request.reference, 500),
            ),
        ];
        match request.reason.as_deref().map(str::trim) {
            Some(r @ ("duplicate" | "fraudulent" | "requested_by_customer")) => {
                form.push(("reason".into(), r.into()));
            }
            Some(r) if !r.is_empty() => {
                form.push(("metadata[hos_reason]".into(), truncate(r, 500)));
            }
            _ => {}
        }
        let rb = self
            .request(reqwest::Method::POST, "/v1/refunds")
            .header(
                "Idempotency-Key",
                format!("hos-refund-{}", request.reference),
            )
            .form(&form);
        let refund = self.call(rb).await?;
        let status = match refund["status"].as_str() {
            Some("succeeded") => RefundStatus::Succeeded,
            Some("pending" | "requires_action") => RefundStatus::Pending,
            _ => RefundStatus::Failed,
        };
        let amount = match (refund["amount"].as_i64(), currency_of(&refund["currency"])) {
            (Some(units), Ok(currency)) => from_stripe_amount(units, currency),
            _ => request.amount,
        };
        Ok(RefundResult {
            gateway_refund_id: id_of(&refund),
            status,
            amount,
            message: refund["failure_reason"].as_str().map(str::to_owned),
            raw: refund,
        })
    }

    async fn charge_stored(
        &self,
        request: &StoredChargeRequest,
    ) -> Result<PaymentEvent, IntegrationError> {
        ensure_positive(&request.amount)?;
        let (customer, method) = parse_stored_ref(
            request.payment_method_ref.expose_str(),
            request.customer.gateway_customer_id.as_deref(),
        )?;
        let form: Vec<(String, String)> = vec![
            (
                "amount".into(),
                to_stripe_amount(&request.amount)?.to_string(),
            ),
            (
                "currency".into(),
                request.amount.currency.as_str().to_ascii_lowercase(),
            ),
            ("customer".into(), customer),
            ("payment_method".into(), method),
            ("off_session".into(), "true".into()),
            ("confirm".into(), "true".into()),
            ("description".into(), truncate(&request.description, 1000)),
            ("automatic_payment_methods[enabled]".into(), "true".into()),
            (
                "automatic_payment_methods[allow_redirects]".into(),
                "never".into(),
            ),
            (
                format!("metadata[{META_REFERENCE}]"),
                request.reference.clone(),
            ),
            (
                format!("metadata[{META_INVOICE}]"),
                request.invoice_id.to_string(),
            ),
            (format!("metadata[{META_SOURCE}]"), SOURCE_STORED.into()),
            (
                "expand[]".into(),
                "latest_charge.balance_transaction".into(),
            ),
        ];
        let rb = self
            .request(reqwest::Method::POST, "/v1/payment_intents")
            .header(
                "Idempotency-Key",
                format!("hos-charge-{}", request.reference),
            )
            .form(&form);
        let response = common::send(rb).await?;
        let body = response.json()?;
        // A declined off-session charge is a business outcome, not an error:
        // Stripe answers 402 with the failed PaymentIntent inside the error.
        let intent = if response.is_success() {
            body
        } else if response.status == 402 && body["error"]["payment_intent"].is_object() {
            let mut intent = body["error"]["payment_intent"].clone();
            if intent["last_payment_error"].is_null() {
                intent["last_payment_error"] = body["error"].clone();
            }
            intent
        } else {
            return Err(stripe_error(&response, &body));
        };
        let status = intent["status"].as_str().unwrap_or("unknown").to_owned();
        let event_id = id_of(&intent).map(|id| format!("stripe:{id}:{status}"));
        let raw = strip_keys(intent.clone(), CARD_KEYS);
        let ctx = EventCtx {
            event_id,
            occurred_at: intent["created"]
                .as_i64()
                .and_then(|t| DateTime::from_timestamp(t, 0)),
            raw: &raw,
        };
        let mut event = intent_event(&intent, &ctx, None)?;
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
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const WHSEC: &str = "whsec_test_secret";

    fn gateway(server: &MockServer) -> StripeGateway {
        let mut s = StripeSettings::new("sk_test_123".into(), WHSEC.into());
        s.api_base = server.uri();
        StripeGateway::new(s).unwrap()
    }

    fn checkout_request() -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "INV42A1".into(),
            amount: Money::new(dec!(30.00), Currency::USD),
            description: "Invoice #2026-0042".into(),
            customer: PaymentCustomer {
                name: "Ada Lovelace".into(),
                email: "ada@example.com".into(),
                ..Default::default()
            },
            items: vec![
                CheckoutItem {
                    reference: "1".into(),
                    name: "VPS S".into(),
                    category: None,
                    quantity: 2,
                    unit_price: Money::new(dec!(10), Currency::USD),
                    total: Money::new(dec!(20), Currency::USD),
                },
                CheckoutItem {
                    reference: "2".into(),
                    name: "Backup".into(),
                    category: None,
                    quantity: 1,
                    unit_price: Money::new(dec!(10), Currency::USD),
                    total: Money::new(dec!(10), Currency::USD),
                },
            ],
            return_url: "https://shop.example/client/invoices/42?paid=1".into(),
            cancel_url: "https://shop.example/client/invoices/42".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: Some(Locale::Tr),
            save_payment_method: true,
            metadata: BTreeMap::from([("tenant".to_owned(), "demo".to_owned())]),
        }
    }

    fn form_of(body: &[u8]) -> Vec<(String, String)> {
        url::form_urlencoded::parse(body).into_owned().collect()
    }

    fn get<'a>(form: &'a [(String, String)], key: &str) -> Option<&'a str> {
        form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn signed_request(body: &str, timestamp: i64) -> WebhookRequest {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Stripe-Signature",
            HeaderValue::from_str(&sign_payload(body.as_bytes(), WHSEC.as_bytes(), timestamp))
                .unwrap(),
        );
        WebhookRequest {
            headers,
            body: body.as_bytes().to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn stripe_amount_rules() {
        let m = |a, c: &str| Money::new(a, Currency::new(c).unwrap());
        assert_eq!(to_stripe_amount(&m(dec!(12.50), "USD")).unwrap(), 1250);
        assert_eq!(to_stripe_amount(&m(dec!(500), "JPY")).unwrap(), 500);
        assert_eq!(to_stripe_amount(&m(dec!(5), "ISK")).unwrap(), 500);
        assert_eq!(to_stripe_amount(&m(dec!(5), "UGX")).unwrap(), 500);
        assert_eq!(to_stripe_amount(&m(dec!(1200), "MGA")).unwrap(), 1200);
        assert!(to_stripe_amount(&m(dec!(10.50), "MGA")).is_err());
        assert_eq!(to_stripe_amount(&m(dec!(1.234), "KWD")).unwrap(), 1234);
        assert_eq!(
            from_stripe_amount(500, Currency::new("ISK").unwrap()).amount,
            dec!(5)
        );
        assert_eq!(
            from_stripe_amount(500, Currency::new("JPY").unwrap()).amount,
            dec!(500)
        );
        assert_eq!(from_stripe_amount(1250, Currency::TRY).amount, dec!(12.50));
    }

    #[test]
    fn signature_known_vectors() {
        let body = br#"{"id":"evt_1","object":"event"}"#;
        let secret = b"whsec_test_secret";
        let now = 1_700_000_000;
        // Independently computed: HMAC-SHA256("1700000000.{body}", secret).
        let expected = crypto::hmac_sha256_hex(
            secret,
            b"1700000000.{\"id\":\"evt_1\",\"object\":\"event\"}",
        );
        let header = format!("t={now},v1={expected}");
        assert_eq!(header, sign_payload(body, secret, now));
        assert_eq!(
            verify_signature(body, &header, secret, now, 300).unwrap(),
            now
        );
        // Extra schemes and several v1 values (secret rotation) are fine.
        let rotated = format!("t={now},v0=deadbeef,v1={},v1={expected}", "00".repeat(32));
        assert!(verify_signature(body, &rotated, secret, now + 10, 300).is_ok());
        // Corrupted signature.
        let bad = header.replace(&expected[..4], "0000");
        assert!(matches!(
            verify_signature(body, &bad, secret, now, 300),
            Err(IntegrationError::Auth(_))
        ));
        // Old timestamp (replay) and future timestamp.
        assert!(matches!(
            verify_signature(body, &header, secret, now + 301, 300),
            Err(IntegrationError::Auth(_))
        ));
        assert!(verify_signature(body, &header, secret, now - 301, 300).is_err());
        // Modified body.
        assert!(
            verify_signature(
                br#"{"id":"evt_2","object":"event"}"#,
                &header,
                secret,
                now,
                300
            )
            .is_err()
        );
        // Wrong secret, missing parts.
        assert!(verify_signature(body, &header, b"whsec_other", now, 300).is_err());
        assert!(verify_signature(body, &format!("v1={expected}"), secret, now, 300).is_err());
        assert!(verify_signature(body, &format!("t={now}"), secret, now, 300).is_err());
        assert!(verify_signature(body, "garbage", secret, now, 300).is_err());
    }

    #[tokio::test]
    async fn create_checkout_posts_form_with_idempotency_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/checkout/sessions"))
            .and(header("authorization", "Bearer sk_test_123"))
            .and(header("idempotency-key", "hos-checkout-INV42A1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cs_test_1",
                "url": "https://checkout.stripe.com/c/pay/cs_test_1",
                "expires_at": 1_800_000_000
            })))
            .expect(1)
            .mount(&server)
            .await;
        let session = gateway(&server)
            .create_checkout(&checkout_request())
            .await
            .unwrap();
        match session {
            CheckoutSession::Redirect {
                url,
                gateway_ref,
                expires_at,
            } => {
                assert_eq!(url, "https://checkout.stripe.com/c/pay/cs_test_1");
                assert_eq!(gateway_ref.as_deref(), Some("cs_test_1"));
                assert_eq!(expires_at.unwrap().timestamp(), 1_800_000_000);
            }
            other => panic!("unexpected session {other:?}"),
        }
        let received = server.received_requests().await.unwrap();
        let form = form_of(&received[0].body);
        assert_eq!(get(&form, "mode"), Some("payment"));
        assert_eq!(get(&form, "client_reference_id"), Some("INV42A1"));
        assert_eq!(get(&form, "locale"), Some("tr"));
        assert_eq!(get(&form, "customer_email"), Some("ada@example.com"));
        assert_eq!(get(&form, "customer_creation"), Some("always"));
        assert_eq!(
            get(&form, "payment_intent_data[setup_future_usage]"),
            Some("off_session")
        );
        assert_eq!(
            get(&form, "line_items[0][price_data][currency]"),
            Some("usd")
        );
        assert_eq!(
            get(&form, "line_items[0][price_data][unit_amount]"),
            Some("2000")
        );
        assert_eq!(
            get(&form, "line_items[0][price_data][product_data][name]"),
            Some("VPS S × 2")
        );
        assert_eq!(
            get(&form, "line_items[1][price_data][unit_amount]"),
            Some("1000")
        );
        assert_eq!(get(&form, "metadata[hos_reference]"), Some("INV42A1"));
        assert_eq!(get(&form, "metadata[tenant]"), Some("demo"));
        assert_eq!(
            get(&form, "payment_intent_data[metadata][hos_reference]"),
            Some("INV42A1")
        );
        assert_eq!(
            get(&form, "payment_intent_data[metadata][hos_save_pm]"),
            Some("1")
        );
    }

    #[tokio::test]
    async fn create_checkout_single_line_when_items_do_not_add_up_and_maps_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/checkout/sessions"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": {"type": "invalid_request_error", "message": "Invalid API Key provided: sk_test_****123"}
            })))
            .mount(&server)
            .await;
        let mut req = checkout_request();
        req.amount = Money::new(dec!(25), Currency::USD); // discount not itemized
        req.customer.gateway_customer_id = Some("cus_9".into());
        let err = gateway(&server).create_checkout(&req).await.unwrap_err();
        assert!(matches!(err, IntegrationError::Auth(_)));
        let form = form_of(&server.received_requests().await.unwrap()[0].body);
        assert_eq!(
            get(&form, "line_items[0][price_data][unit_amount]"),
            Some("2500")
        );
        assert_eq!(get(&form, "line_items[1][price_data][unit_amount]"), None);
        assert_eq!(get(&form, "customer"), Some("cus_9"));
        assert_eq!(get(&form, "customer_creation"), None);
    }

    #[tokio::test]
    async fn webhook_checkout_completed_fetches_intent_for_fee_and_saved_method() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/payment_intents/pi_1"))
            .and(query_param("expand[]", "latest_charge.balance_transaction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "pi_1", "customer": "cus_1", "payment_method": "pm_1",
                "latest_charge": {"id": "ch_1", "balance_transaction": {"fee": 117, "currency": "usd"},
                    "payment_method_details": {"type": "card", "card": {"brand": "visa", "last4": "4242", "exp_year": 2030}}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let body = json!({
            "id": "evt_1", "type": "checkout.session.completed", "created": 1_700_000_000, "livemode": false,
            "data": {"object": {
                "id": "cs_1", "object": "checkout.session", "mode": "payment", "payment_status": "paid",
                "amount_total": 3000, "currency": "usd", "client_reference_id": "INV42A1",
                "payment_intent": "pi_1", "customer": "cus_1",
                "metadata": {"hos_reference": "INV42A1", "hos_save_pm": "1"}
            }}
        })
        .to_string();
        let outcome = gateway(&server)
            .parse_webhook(&signed_request(&body, now_unix()))
            .await
            .unwrap();
        assert_eq!(outcome.reply, WebhookReply::Empty);
        let ev = &outcome.events[0];
        assert_eq!(ev.kind, PaymentEventKind::Paid);
        assert_eq!(ev.event_id.as_deref(), Some("evt_1"));
        assert_eq!(ev.gateway_txn_id, "pi_1");
        assert_eq!(ev.invoice_ref.as_deref(), Some("INV42A1"));
        assert_eq!(ev.amount, Money::new(dec!(30), Currency::USD));
        assert_eq!(ev.fee, Some(Money::new(dec!(1.17), Currency::USD)));
        assert_eq!(
            ev.payment_method_ref
                .as_ref()
                .map(|s| s.expose_str().to_owned()),
            Some("cus_1:pm_1".to_owned())
        );
        assert_eq!(ev.occurred_at.unwrap().timestamp(), 1_700_000_000);
        assert_eq!(
            crate::event_payment_method_label(ev),
            Some("VISA •••• 4242")
        );
        assert!(!ev.raw.to_string().contains("exp_year"));
    }

    fn setup_request() -> CardSetupRequest {
        CardSetupRequest {
            reference: "SETUP7Q2W9E4R".into(),
            currency: Currency::EUR,
            customer: PaymentCustomer {
                client_id: Some(hos_core::ids::ClientId::new()),
                name: "Ada Lovelace".into(),
                email: "ada@example.com".into(),
                ..Default::default()
            },
            return_url: "https://shop.example/client/billing?card=saved".into(),
            cancel_url: "https://shop.example/client/billing?card=cancelled".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: Some(Locale::En),
        }
    }

    #[tokio::test]
    async fn card_setup_creates_customer_then_setup_session() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/customers"))
            .and(header("idempotency-key", "hos-customer-SETUP7Q2W9E4R"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cus_new"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/checkout/sessions"))
            .and(header("idempotency-key", "hos-setup-SETUP7Q2W9E4R"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cs_setup_1", "url": "https://checkout.stripe.com/c/pay/cs_setup_1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = gateway(&server);
        assert!(gw.supports_card_setup());
        let session = gw.create_card_setup(&setup_request()).await.unwrap();
        assert_eq!(session.gateway_ref(), Some("cs_setup_1"));
        let received = server.received_requests().await.unwrap();
        let customer = form_of(&received[0].body);
        assert_eq!(get(&customer, "email"), Some("ada@example.com"));
        let form = form_of(&received[1].body);
        assert_eq!(get(&form, "mode"), Some("setup"));
        assert_eq!(get(&form, "currency"), Some("eur"));
        assert_eq!(get(&form, "customer"), Some("cus_new"));
        assert_eq!(get(&form, "client_reference_id"), Some("SETUP7Q2W9E4R"));
        assert_eq!(get(&form, "metadata[hos_setup]"), Some("1"));
        assert_eq!(get(&form, "line_items[0][price_data][currency]"), None);

        // An existing customer is reused; a bad reference is refused.
        let mut req = setup_request();
        req.customer.gateway_customer_id = Some("cus_old".into());
        req.reference = "SETUPEXISTING01".into();
        Mock::given(method("POST"))
            .and(path("/v1/checkout/sessions"))
            .and(header("idempotency-key", "hos-setup-SETUPEXISTING01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cs_setup_2", "url": "https://checkout.stripe.com/c/pay/cs_setup_2"
            })))
            .mount(&server)
            .await;
        gw.create_card_setup(&req).await.unwrap();
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 3, "no second customer");
        assert_eq!(
            get(&form_of(&received[2].body), "customer"),
            Some("cus_old")
        );
        req.reference = "bad ref".into();
        assert!(gw.create_card_setup(&req).await.is_err());
    }

    #[tokio::test]
    async fn webhook_setup_session_maps_to_method_saved() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/setup_intents/seti_1"))
            .and(query_param("expand[]", "payment_method"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "seti_1", "status": "succeeded", "customer": "cus_9",
                "payment_method": {"id": "pm_9", "type": "card",
                    "card": {"brand": "mastercard", "last4": "4444", "exp_year": 2031}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let event = |meta: Value| {
            json!({
                "id": "evt_setup", "type": "checkout.session.completed", "created": 1_700_000_000,
                "livemode": false,
                "data": {"object": {
                    "id": "cs_setup_1", "object": "checkout.session", "mode": "setup",
                    "status": "complete", "currency": "eur", "customer": "cus_9",
                    "client_reference_id": "SETUP7Q2W9E4R", "setup_intent": "seti_1",
                    "metadata": meta
                }}
            })
            .to_string()
        };
        let gw = gateway(&server);
        // Not started by HostingOS: ignored without an API call.
        let foreign = gw
            .parse_webhook(&signed_request(&event(json!({})), now_unix()))
            .await
            .unwrap();
        assert!(foreign.events.is_empty());
        let outcome = gw
            .parse_webhook(&signed_request(
                &event(json!({"hos_setup": "1", "hos_reference": "SETUP7Q2W9E4R"})),
                now_unix(),
            ))
            .await
            .unwrap();
        let ev = &outcome.events[0];
        assert_eq!(ev.kind, PaymentEventKind::MethodSaved);
        assert_eq!(ev.invoice_ref.as_deref(), Some("SETUP7Q2W9E4R"));
        assert_eq!(ev.amount, Money::zero(Currency::EUR));
        assert_eq!(
            ev.payment_method_ref
                .as_ref()
                .map(|s| s.expose_str().to_owned()),
            Some("cus_9:pm_9".to_owned())
        );
        assert_eq!(
            crate::event_payment_method_label(ev),
            Some("MASTERCARD •••• 4444")
        );
        assert!(!ev.raw.to_string().contains("exp_year"));
    }

    #[tokio::test]
    async fn webhook_secret_rotation_accepts_previous_secret_only_while_set() {
        let server = MockServer::start().await;
        let body = r#"{"id":"evt_r","type":"customer.created","livemode":false}"#;
        let now = now_unix();
        let old_secret = b"whsec_previous";
        let old_only = |secret: &[u8]| {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Stripe-Signature",
                HeaderValue::from_str(&sign_payload(body.as_bytes(), secret, now)).unwrap(),
            );
            WebhookRequest {
                headers,
                body: body.as_bytes().to_vec(),
                ..Default::default()
            }
        };
        // Without the previous secret configured, a delivery signed with it fails.
        assert!(
            gateway(&server)
                .parse_webhook(&old_only(old_secret))
                .await
                .is_err()
        );
        let mut s = StripeSettings::new("sk_test_1".into(), WHSEC.into());
        s.api_base = server.uri();
        s.webhook_secret_previous = Some("whsec_previous".into());
        let rotating = StripeGateway::new(s).unwrap();
        assert!(rotating.parse_webhook(&old_only(old_secret)).await.is_ok());
        assert!(
            rotating
                .parse_webhook(&old_only(WHSEC.as_bytes()))
                .await
                .is_ok()
        );
        // Both v1 signatures in one header (Stripe during the roll window).
        let both = format!(
            "{},v1={}",
            sign_payload(body.as_bytes(), WHSEC.as_bytes(), now),
            sign_payload(body.as_bytes(), old_secret, now)
                .split("v1=")
                .nth(1)
                .unwrap()
        );
        let mut req = old_only(WHSEC.as_bytes());
        req.headers
            .insert("Stripe-Signature", HeaderValue::from_str(&both).unwrap());
        assert!(rotating.parse_webhook(&req).await.is_ok());
        assert!(
            rotating
                .parse_webhook(&old_only(b"whsec_third"))
                .await
                .is_err()
        );
        // Read from secrets JSON; blank means not set.
        let parsed = StripeSettings::from_values(
            &json!({"mode": "test"}),
            &json!({"secret_key": "sk_test_1", "webhook_secret": "whsec_1", "webhook_secret_previous": " "}),
        )
        .unwrap();
        assert!(parsed.webhook_secret_previous.is_none());
    }

    #[tokio::test]
    async fn webhook_rejects_bad_signature_old_timestamp_and_tampered_body() {
        let server = MockServer::start().await;
        let gw = gateway(&server);
        let body = r#"{"id":"evt_1","type":"charge.refunded","livemode":false}"#;
        // Valid signature over a different body.
        let mut req = signed_request(body, now_unix());
        req.body = br#"{"id":"evt_1","type":"charge.refunded","livemode":true}"#.to_vec();
        assert!(matches!(
            gw.parse_webhook(&req).await,
            Err(IntegrationError::Auth(_))
        ));
        // Replayed (10 minutes old).
        let old = signed_request(body, now_unix() - 600);
        assert!(matches!(
            gw.parse_webhook(&old).await,
            Err(IntegrationError::Auth(_))
        ));
        // No header at all.
        let none = WebhookRequest {
            body: body.as_bytes().to_vec(),
            ..Default::default()
        };
        assert!(matches!(
            gw.parse_webhook(&none).await,
            Err(IntegrationError::Auth(_))
        ));
        // Signed with another secret.
        let mut other = signed_request(body, now_unix());
        other.headers.insert(
            "Stripe-Signature",
            HeaderValue::from_str(&sign_payload(body.as_bytes(), b"whsec_other", now_unix()))
                .unwrap(),
        );
        assert!(matches!(
            gw.parse_webhook(&other).await,
            Err(IntegrationError::Auth(_))
        ));
    }

    #[tokio::test]
    async fn webhook_maps_failure_refund_dispute_and_ignores_others() {
        let server = MockServer::start().await;
        let gw = gateway(&server);
        let failed = json!({
            "id": "evt_f", "type": "payment_intent.payment_failed", "created": 1, "livemode": false,
            "data": {"object": {"id": "pi_2", "object": "payment_intent", "status": "requires_payment_method",
                "amount": 500, "currency": "jpy", "metadata": {"hos_reference": "INV7"},
                "last_payment_error": {"message": "Your card was declined.", "decline_code": "insufficient_funds"}}}
        });
        let ev = &gw
            .parse_webhook(&signed_request(&failed.to_string(), now_unix()))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.kind, PaymentEventKind::Failed);
        assert_eq!(
            ev.amount,
            Money::new(dec!(500), Currency::new("JPY").unwrap())
        );
        assert_eq!(ev.invoice_ref.as_deref(), Some("INV7"));
        assert_eq!(
            ev.message.as_deref(),
            Some("Your card was declined. (insufficient_funds)")
        );

        let refunded = json!({
            "id": "evt_r", "type": "charge.refunded", "created": 2, "livemode": false,
            "data": {
                "object": {"id": "ch_1", "object": "charge", "payment_intent": "pi_1",
                    "amount": 3000, "amount_refunded": 2500, "currency": "usd", "metadata": {},
                    "payment_method_details": {"type": "card", "card": {"last4": "4242", "exp_year": 2030}}},
                "previous_attributes": {"amount_refunded": 1000}
            }
        });
        let ev = &gw
            .parse_webhook(&signed_request(&refunded.to_string(), now_unix()))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.kind, PaymentEventKind::Refunded);
        assert_eq!(ev.gateway_txn_id, "pi_1");
        assert_eq!(ev.amount, Money::new(dec!(15), Currency::USD));
        assert!(
            ev.raw["data"]["object"]
                .get("payment_method_details")
                .is_none()
        );
        assert!(!ev.raw.to_string().contains("4242"));
        // No embedded refund list and the lookup failed: no refund id.
        assert_eq!(ev.gateway_refund_id.as_deref(), None);

        let dispute = json!({
            "id": "evt_d", "type": "charge.dispute.created", "created": 3, "livemode": false,
            "data": {"object": {"id": "dp_1", "object": "dispute", "charge": "ch_1", "payment_intent": "pi_1",
                "amount": 3000, "currency": "usd", "reason": "fraudulent"}}
        });
        let ev = &gw
            .parse_webhook(&signed_request(&dispute.to_string(), now_unix()))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.kind, PaymentEventKind::Chargeback);
        assert_eq!(ev.gateway_txn_id, "pi_1");
        assert_eq!(ev.message.as_deref(), Some("dispute: fraudulent"));

        // Ignored types and checkout-originated payment_intent.succeeded.
        for body in [
            json!({"id": "evt_x", "type": "customer.created", "livemode": false, "data": {"object": {}}}),
            json!({"id": "evt_y", "type": "payment_intent.succeeded", "livemode": false,
                "data": {"object": {"id": "pi_1", "status": "succeeded", "amount": 1, "currency": "usd", "metadata": {}}}}),
            // Live event reaching a test-mode integration.
            json!({"id": "evt_z", "type": "charge.refunded", "livemode": true, "data": {"object": {}}}),
        ] {
            let out = gw
                .parse_webhook(&signed_request(&body.to_string(), now_unix()))
                .await
                .unwrap();
            assert!(out.events.is_empty());
        }
    }

    #[tokio::test]
    async fn refunded_event_carries_the_refund_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/refunds"))
            .and(wiremock::matchers::query_param("charge", "ch_9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [
                    {"id": "re_new", "amount": 500, "status": "succeeded",
                     "metadata": {"hos_refund_reference": "abc123"}},
                    {"id": "re_old", "amount": 1000, "status": "succeeded", "metadata": {}}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = gateway(&server);
        // Older API versions: the refund list is embedded in the charge.
        let embedded = json!({
            "id": "evt_r2", "type": "charge.refunded", "created": 2, "livemode": false,
            "data": {
                "object": {"id": "ch_1", "object": "charge", "payment_intent": "pi_1",
                    "amount": 3000, "amount_refunded": 2500, "currency": "usd", "metadata": {},
                    "refunds": {"object": "list", "data": [
                        {"id": "re_2", "amount": 1500, "status": "succeeded",
                         "metadata": {"hos_refund_reference": "ref2"}},
                        {"id": "re_1", "amount": 1000, "status": "succeeded", "metadata": {}}
                    ]}},
                "previous_attributes": {"amount_refunded": 1000, "refunds": {"object": "list", "data": [
                    {"id": "re_1", "amount": 1000, "status": "succeeded", "metadata": {}}
                ]}}
            }
        });
        let ev = &gw
            .parse_webhook(&signed_request(&embedded.to_string(), now_unix()))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.amount, Money::new(dec!(15), Currency::USD));
        assert_eq!(ev.gateway_refund_id.as_deref(), Some("re_2"));
        assert_eq!(crate::event_refund_reference(ev), Some("ref2"));

        // Current API versions: looked up by charge, matched by amount.
        let bare = json!({
            "id": "evt_r3", "type": "charge.refunded", "created": 3, "livemode": false,
            "data": {
                "object": {"id": "ch_9", "object": "charge", "payment_intent": "pi_9",
                    "amount": 3000, "amount_refunded": 1500, "currency": "usd", "metadata": {}},
                "previous_attributes": {"amount_refunded": 1000}
            }
        });
        let ev = &gw
            .parse_webhook(&signed_request(&bare.to_string(), now_unix()))
            .await
            .unwrap()
            .events[0];
        assert_eq!(ev.amount, Money::new(dec!(5), Currency::USD));
        assert_eq!(ev.gateway_refund_id.as_deref(), Some("re_new"));
        assert_eq!(crate::event_refund_reference(ev), Some("abc123"));
    }

    #[tokio::test]
    async fn refund_posts_payment_intent_and_maps_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/refunds"))
            .and(header("idempotency-key", "hos-refund-RF1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "re_1", "object": "refund", "status": "pending", "amount": 1000, "currency": "usd"
            })))
            .mount(&server)
            .await;
        let result = gateway(&server)
            .refund(&RefundRequest {
                gateway_txn_id: "pi_1".into(),
                amount: Money::new(dec!(10), Currency::USD),
                reason: Some("customer changed plan".into()),
                reference: "RF1".into(),
                ip: None,
            })
            .await
            .unwrap();
        assert_eq!(result.status, RefundStatus::Pending);
        assert_eq!(result.gateway_refund_id.as_deref(), Some("re_1"));
        assert_eq!(result.amount, Money::new(dec!(10), Currency::USD));
        let form = form_of(&server.received_requests().await.unwrap()[0].body);
        assert_eq!(get(&form, "payment_intent"), Some("pi_1"));
        assert_eq!(get(&form, "amount"), Some("1000"));
        assert_eq!(get(&form, "reason"), None);
        assert_eq!(
            get(&form, "metadata[hos_reason]"),
            Some("customer changed plan")
        );
    }

    #[tokio::test]
    async fn charge_stored_success_and_decline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/payment_intents"))
            .and(header("idempotency-key", "hos-charge-R1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "pi_9", "status": "succeeded", "amount": 1999, "amount_received": 1999,
                "currency": "eur", "created": 1_700_000_000, "metadata": {"hos_reference": "R1"},
                "latest_charge": {"balance_transaction": {"fee": 50, "currency": "eur"}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/payment_intents"))
            .and(header("idempotency-key", "hos-charge-R2"))
            .respond_with(ResponseTemplate::new(402).set_body_json(json!({
                "error": {"type": "card_error", "code": "authentication_required",
                    "message": "This payment requires authentication.",
                    "payment_intent": {"id": "pi_10", "status": "requires_payment_method", "amount": 1999,
                        "currency": "eur", "metadata": {"hos_reference": "R2"}}}
            })))
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let mut req = StoredChargeRequest {
            invoice_id: InvoiceId::new(),
            reference: "R1".into(),
            amount: Money::new(dec!(19.99), Currency::EUR),
            description: "Renewal".into(),
            customer: PaymentCustomer::default(),
            payment_method_ref: "cus_1:pm_1".into(),
        };
        let ok = gw.charge_stored(&req).await.unwrap();
        assert_eq!(ok.kind, PaymentEventKind::Paid);
        assert_eq!(ok.gateway_txn_id, "pi_9");
        assert_eq!(ok.fee, Some(Money::new(dec!(0.50), Currency::EUR)));
        assert_eq!(ok.event_id.as_deref(), Some("stripe:pi_9:succeeded"));
        let form = form_of(&server.received_requests().await.unwrap()[0].body);
        assert_eq!(get(&form, "customer"), Some("cus_1"));
        assert_eq!(get(&form, "payment_method"), Some("pm_1"));
        assert_eq!(get(&form, "off_session"), Some("true"));
        assert_eq!(get(&form, "confirm"), Some("true"));

        req.reference = "R2".into();
        let declined = gw.charge_stored(&req).await.unwrap();
        assert_eq!(declined.kind, PaymentEventKind::Failed);
        assert_eq!(declined.gateway_txn_id, "pi_10");
        assert!(
            declined
                .message
                .unwrap()
                .contains("authentication_required")
        );

        req.payment_method_ref = "pm_only".into();
        assert!(matches!(
            gw.charge_stored(&req).await,
            Err(IntegrationError::InvalidInput(_))
        ));
    }

    #[test]
    fn settings_validation() {
        let ok = StripeSettings::from_values(
            &json!({"mode": "test", "session_expiry_minutes": "60"}),
            &json!({"secret_key": "sk_test_1", "webhook_secret": "whsec_1"}),
        )
        .unwrap();
        assert_eq!(ok.session_expiry_minutes, Some(60));
        assert!(!ok.is_live());
        let mismatch = StripeSettings::from_values(
            &json!({"mode": "live"}),
            &json!({"secret_key": "sk_test_1", "webhook_secret": "whsec_1"}),
        )
        .unwrap_err();
        assert!(matches!(mismatch, IntegrationError::Config(_)));
        assert!(!format!("{mismatch}").contains("sk_test_1"));
        assert!(
            StripeSettings::from_values(
                &json!({"mode": "test", "session_expiry_minutes": 5}),
                &json!({"secret_key": "sk_test_1", "webhook_secret": "whsec_1"}),
            )
            .is_err()
        );
        assert!(!format!("{ok:?}").contains("sk_test_1"));
    }
}
