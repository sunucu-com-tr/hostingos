//! Mock gateway for demos and end-to-end tests. **Never enable in production.**
//!
//! `create_checkout` returns a [`CheckoutSession::Form`] that auto-posts a
//! signed simulated event straight to the integration's webhook URL, so the
//! full flow (webhook verification → `PaymentEvent` → billing → 303 back to the
//! invoice) runs without any external service.
//!
//! Outcome selection (first match wins):
//! 1. `CheckoutRequest::metadata["mock_outcome"]` = `success` | `failure` | `pending`;
//! 2. amount minor units ending in `13` (e.g. `10.13`) → failure, `77` → pending;
//! 3. the integration's `default_outcome`.
//!
//! Signatures use the Stripe scheme (`t=…,v1=hex(HMAC-SHA256("{t}.{payload}"))`)
//! carried in `X-HOS-Mock-Signature` (server-to-server JSON) or in the
//! `signature` form field next to `payload` (browser form post).
//! [`webhook_request`] and [`form_fields`] build signed fake webhooks for tests.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::DateTime;
use hos_core::domain::IntegrationCategory;
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{
    CardSetupRequest, CheckoutRequest, CheckoutSession, FormMethod, GatewayCapabilities,
    PaymentEvent, PaymentEventKind, PaymentGateway, RefundRequest, RefundResult, RefundStatus,
    StoredChargeRequest, WebhookOutcome, WebhookReply, WebhookRequest,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, SelectOption,
};
use hos_core::money::Money;
use hos_core::secret::SecretString;
use http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{auth, ensure_positive, http_url, now_unix};
use crate::stripe;

/// Integration kind of the mock gateway (not in `hos_core::integrations::kinds`).
pub const KIND: &str = "mock";
/// Header carrying the signature of server-to-server mock webhooks.
pub const SIGNATURE_HEADER: &str = "x-hos-mock-signature";
/// A request carrying this header stands for a provider whose webhooks are
/// verified by calling its API ([`PaymentGateway::verifies_remotely`]).
pub const REMOTE_VERIFY_HEADER: &str = "x-hos-mock-remote-verify";
/// Metadata key selecting the outcome.
pub const OUTCOME_KEY: &str = "mock_outcome";
/// Default signature tolerance.
pub const DEFAULT_TOLERANCE_SECS: i64 = 600;

/// A simulated gateway event (the signed payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockEvent {
    /// Event id (stable per reference + outcome).
    pub id: String,
    /// What happened.
    pub kind: PaymentEventKind,
    /// Simulated transaction id.
    pub txn_id: String,
    /// Invoice reference.
    pub reference: Option<String>,
    /// Amount.
    pub amount: Money,
    /// Failure reason.
    pub message: Option<String>,
    /// Simulated stored payment method token.
    pub payment_method_ref: Option<String>,
    /// Browser redirect after success / pending.
    pub return_url: Option<String>,
    /// Browser redirect after failure.
    pub cancel_url: Option<String>,
    /// Unix time.
    pub created: i64,
    /// Simulated gateway refund id (`Refunded` events; [`MockGateway`]'s
    /// refunds are `mock_rf_{reference}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund_id: Option<String>,
}

impl MockEvent {
    /// A new event with `id = "mock:{txn_id}:{kind}"`.
    pub fn new(kind: PaymentEventKind, txn_id: impl Into<String>, amount: Money) -> Self {
        let txn_id = txn_id.into();
        Self {
            id: format!("mock:{txn_id}:{kind}"),
            kind,
            txn_id,
            reference: None,
            amount,
            message: None,
            payment_method_ref: None,
            return_url: None,
            cancel_url: None,
            created: now_unix(),
            refund_id: None,
        }
    }

    /// Sets the gateway refund id (`Refunded` events).
    #[must_use]
    pub fn with_refund_id(mut self, refund_id: impl Into<String>) -> Self {
        self.refund_id = Some(refund_id.into());
        self
    }

    /// Sets the invoice reference.
    #[must_use]
    pub fn with_reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    /// Sets the failure message.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    fn payload(&self) -> String {
        // Serializing plain strings/numbers cannot fail; fall back to an empty object.
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// `X-HOS-Mock-Signature` value for `payload` at `timestamp`.
pub fn sign(secret: &[u8], payload: &str, timestamp: i64) -> String {
    stripe::sign_payload(payload.as_bytes(), secret, timestamp)
}

/// A signed server-to-server webhook (JSON body + header) for `event`.
pub fn webhook_request(secret: &[u8], event: &MockEvent, timestamp: i64) -> WebhookRequest {
    let payload = event.payload();
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&sign(secret, &payload, timestamp)) {
        headers.insert(SIGNATURE_HEADER, v);
    }
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    WebhookRequest {
        headers,
        body: payload.into_bytes(),
        query: None,
        remote_ip: None,
    }
}

/// Signed browser form fields (`payload`, `signature`) for `event`.
pub fn form_fields(secret: &[u8], event: &MockEvent, timestamp: i64) -> Vec<(String, String)> {
    let payload = event.payload();
    let signature = sign(secret, &payload, timestamp);
    vec![
        ("payload".to_owned(), payload),
        ("signature".to_owned(), signature),
    ]
}

/// The simulated outcome for an amount (see the module docs).
pub fn outcome_for(
    amount: &Money,
    metadata: &BTreeMap<String, String>,
    default: PaymentEventKind,
) -> PaymentEventKind {
    match metadata
        .get(OUTCOME_KEY)
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("success" | "paid") => return PaymentEventKind::Paid,
        Some("failure" | "failed") => return PaymentEventKind::Failed,
        Some("pending") => return PaymentEventKind::Pending,
        _ => {}
    }
    match amount.to_minor_units().map(|u| u.rem_euclid(100)) {
        Ok(13) => PaymentEventKind::Failed,
        Ok(77) => PaymentEventKind::Pending,
        _ => default,
    }
}

/// Mock settings.
#[derive(Debug, Clone)]
pub struct MockSettings {
    /// HMAC key of simulated webhooks.
    pub webhook_secret: SecretString,
    /// Outcome when nothing else selects one.
    pub default_outcome: PaymentEventKind,
    /// Signature timestamp tolerance (seconds).
    pub tolerance_secs: i64,
    /// Highest installment count offered on the pay page (`0`/`1`: none),
    /// to exercise the installment picker without a bank gateway.
    pub max_installment: u32,
}

impl MockSettings {
    /// Reads settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let default_outcome = match c.string_or("default_outcome", "success")?.as_str() {
            "success" => PaymentEventKind::Paid,
            "failure" => PaymentEventKind::Failed,
            "pending" => PaymentEventKind::Pending,
            _ => {
                return Err(IntegrationError::Config(
                    "config.default_outcome must be success, failure or pending".into(),
                ));
            }
        };
        Ok(Self {
            webhook_secret: s.secret("webhook_secret")?,
            default_outcome,
            tolerance_secs: i64::try_from(c.u64_or(
                "webhook_tolerance_secs",
                DEFAULT_TOLERANCE_SECS.unsigned_abs(),
            )?)
            .map_err(|_| {
                IntegrationError::Config("config.webhook_tolerance_secs is too large".into())
            })?,
            max_installment: u32::try_from(c.u64_or("max_installment", 0)?)
                .ok()
                .filter(|n| *n <= 12)
                .ok_or_else(|| {
                    IntegrationError::Config("config.max_installment must be 0–12".into())
                })?,
        })
    }
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(
        KIND,
        "Mock gateway (demo / testing)",
        IntegrationCategory::Gateway,
    )
    .description(
        "Simulates payments instantly through a signed form post to the webhook URL. NEVER \
             enable for real customers. Outcome: metadata mock_outcome=success|failure|pending, or \
             amounts ending in .13 fail and .77 stay pending. Any currency.",
    )
    .config_field(
        FieldSpec::select(
            "default_outcome",
            "Default outcome",
            vec![
                SelectOption::new("success", "Success"),
                SelectOption::new("failure", "Failure"),
                SelectOption::new("pending", "Pending"),
            ],
        )
        .default_value("success"),
    )
    .config_field(
        FieldSpec::number("webhook_tolerance_secs", "Signature tolerance (seconds)")
            .default_value(DEFAULT_TOLERANCE_SECS),
    )
    .config_field(
        FieldSpec::number("max_installment", "Max installments (0 = none)").default_value(0),
    )
    .secret_field(FieldSpec::password("webhook_secret", "Webhook signing secret").required())
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

/// The mock gateway.
#[derive(Debug, Clone)]
pub struct MockGateway {
    settings: MockSettings,
}

impl MockGateway {
    /// Creates the gateway.
    pub fn new(settings: MockSettings) -> Self {
        Self { settings }
    }

    fn secret(&self) -> &[u8] {
        self.settings.webhook_secret.expose_str().as_bytes()
    }
}

fn message_for(kind: PaymentEventKind) -> Option<String> {
    match kind {
        PaymentEventKind::Failed => Some("simulated decline".to_owned()),
        PaymentEventKind::Pending => Some("simulated pending payment".to_owned()),
        _ => None,
    }
}

#[async_trait]
impl PaymentGateway for MockGateway {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn capabilities(&self) -> GatewayCapabilities {
        GatewayCapabilities {
            installments: self.settings.max_installment >= 2,
            ..capabilities()
        }
    }

    /// `2..=max_installment` in any currency (the amount stays the same).
    fn installment_choices(&self, amount: &Money) -> Vec<u32> {
        let _ = amount;
        if self.settings.max_installment < 2 {
            return Vec::new();
        }
        (2..=self.settings.max_installment).collect()
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        Ok(ConnectionInfo {
            server_version: None,
            details: Some("mock gateway (no remote API)".into()),
        })
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        ensure_positive(&request.amount)?;
        http_url(&request.webhook_url, "webhook_url")?;
        http_url(&request.return_url, "return_url")?;
        http_url(&request.cancel_url, "cancel_url")?;
        if let Some(n) = request.metadata.get(crate::nestpay::INSTALLMENTS_METADATA) {
            let ok = n
                .parse::<u32>()
                .is_ok_and(|n| n >= 2 && n <= self.settings.max_installment);
            if !ok {
                return Err(IntegrationError::InvalidInput(
                    "installment count not offered by this gateway".into(),
                ));
            }
        }
        let kind = outcome_for(
            &request.amount,
            &request.metadata,
            self.settings.default_outcome,
        );
        let txn = format!("mock_{}", request.reference);
        let mut event = MockEvent::new(kind, txn.clone(), request.amount.round())
            .with_reference(request.reference.clone());
        event.message = message_for(kind);
        event.return_url = Some(request.return_url.clone());
        event.cancel_url = Some(request.cancel_url.clone());
        if request.save_payment_method && kind == PaymentEventKind::Paid {
            event.payment_method_ref = Some(format!("mock_pm_{}", request.reference));
        }
        Ok(CheckoutSession::Form {
            action: request.webhook_url.clone(),
            method: FormMethod::Post,
            fields: form_fields(self.secret(), &event, now_unix()),
            gateway_ref: Some(txn),
        })
    }

    fn verifies_remotely(&self, request: &WebhookRequest) -> bool {
        request.header(REMOTE_VERIFY_HEADER).is_some()
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let (payload, signature, browser) = match request.header(SIGNATURE_HEADER) {
            Some(sig) => (
                request
                    .body_str()
                    .map_err(|_| auth("mock webhook body is not UTF-8"))?
                    .to_owned(),
                sig.to_owned(),
                false,
            ),
            None => match (
                request.form_field("payload"),
                request.form_field("signature"),
            ) {
                (Some(p), Some(s)) => (p, s, true),
                _ => return Err(auth("mock webhook without signature")),
            },
        };
        stripe::verify_signature(
            payload.as_bytes(),
            &signature,
            self.secret(),
            now_unix(),
            self.settings.tolerance_secs,
        )?;
        let event: MockEvent = serde_json::from_str(&payload)
            .map_err(|_| IntegrationError::InvalidInput("mock payload is malformed".into()))?;
        let reply = match (browser, event.kind) {
            (false, _) => WebhookReply::Empty,
            (
                true,
                PaymentEventKind::Paid | PaymentEventKind::Pending | PaymentEventKind::MethodSaved,
            ) => event
                .return_url
                .clone()
                .map_or(WebhookReply::Empty, WebhookReply::Redirect),
            (true, _) => event
                .cancel_url
                .clone()
                .map_or(WebhookReply::Empty, WebhookReply::Redirect),
        };
        let mut raw = serde_json::to_value(&event).unwrap_or(Value::Null);
        if let Some(map) = raw.as_object_mut() {
            map.remove("payment_method_ref");
        }
        let gateway_refund_id = event
            .refund_id
            .map(|r| r.trim().to_owned())
            .filter(|r| event.kind == PaymentEventKind::Refunded && !r.is_empty());
        Ok(WebhookOutcome {
            events: vec![PaymentEvent {
                kind: event.kind,
                event_id: Some(event.id),
                gateway_txn_id: event.txn_id,
                invoice_ref: event.reference,
                amount: event.amount,
                fee: None,
                occurred_at: DateTime::from_timestamp(event.created, 0),
                message: event.message,
                payment_method_ref: event.payment_method_ref.map(SecretString::from),
                gateway_refund_id,
                raw,
            }],
            reply,
        })
    }

    fn supports_card_setup(&self) -> bool {
        true
    }

    /// Simulates saving a card: a signed `method_saved` form post to the
    /// webhook URL carrying the token `mock_pm_{reference}`.
    async fn create_card_setup(
        &self,
        request: &CardSetupRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        http_url(&request.webhook_url, "webhook_url")?;
        http_url(&request.return_url, "return_url")?;
        http_url(&request.cancel_url, "cancel_url")?;
        let txn = format!("mock_seti_{}", request.reference);
        let mut event = MockEvent::new(
            PaymentEventKind::MethodSaved,
            txn.clone(),
            Money::zero(request.currency),
        )
        .with_reference(request.reference.clone());
        event.payment_method_ref = Some(format!("mock_pm_{}", request.reference));
        event.return_url = Some(request.return_url.clone());
        event.cancel_url = Some(request.cancel_url.clone());
        Ok(CheckoutSession::Form {
            action: request.webhook_url.clone(),
            method: FormMethod::Post,
            fields: form_fields(self.secret(), &event, now_unix()),
            gateway_ref: Some(txn),
        })
    }

    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        ensure_positive(&request.amount)?;
        let failed = outcome_for(&request.amount, &BTreeMap::new(), PaymentEventKind::Paid)
            == PaymentEventKind::Failed;
        Ok(RefundResult {
            gateway_refund_id: Some(format!("mock_rf_{}", request.reference)),
            status: if failed {
                RefundStatus::Failed
            } else {
                RefundStatus::Succeeded
            },
            amount: request.amount,
            message: failed.then(|| "simulated refund failure".to_owned()),
            raw: serde_json::json!({"gateway_txn_id": request.gateway_txn_id, "simulated": true}),
        })
    }

    async fn charge_stored(
        &self,
        request: &StoredChargeRequest,
    ) -> Result<PaymentEvent, IntegrationError> {
        ensure_positive(&request.amount)?;
        if !request
            .payment_method_ref
            .expose_str()
            .starts_with("mock_pm_")
        {
            return Err(IntegrationError::InvalidInput(
                "not a mock payment method".into(),
            ));
        }
        let kind = outcome_for(
            &request.amount,
            &BTreeMap::new(),
            self.settings.default_outcome,
        );
        let txn = format!("mock_{}", request.reference);
        Ok(PaymentEvent {
            kind,
            event_id: Some(format!("mock:{txn}:{kind}")),
            gateway_txn_id: txn,
            invoice_ref: Some(request.reference.clone()),
            amount: request.amount.round(),
            fee: None,
            occurred_at: DateTime::from_timestamp(now_unix(), 0),
            message: message_for(kind),
            payment_method_ref: None,
            gateway_refund_id: None,
            raw: serde_json::json!({"simulated": true}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::ids::InvoiceId;
    use hos_core::integrations::gateway::PaymentCustomer;
    use hos_core::money::Currency;
    use rust_decimal_macros::dec;

    const SECRET: &[u8] = b"mock-secret";

    fn gateway() -> MockGateway {
        MockGateway::new(
            MockSettings::from_values(
                &serde_json::json!({}),
                &serde_json::json!({"webhook_secret": "mock-secret"}),
            )
            .unwrap(),
        )
    }

    fn request(amount: Money) -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "INV42A1".into(),
            amount,
            description: "Demo".into(),
            customer: PaymentCustomer::default(),
            items: vec![],
            return_url: "https://shop.example/ok".into(),
            cancel_url: "https://shop.example/fail".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: None,
            save_payment_method: true,
            metadata: BTreeMap::new(),
        }
    }

    /// Simulates the browser auto-submitting the checkout form.
    fn post_form(fields: &[(String, String)]) -> WebhookRequest {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        WebhookRequest {
            body: body.into_bytes(),
            ..Default::default()
        }
    }

    #[test]
    fn outcome_selection() {
        let try_ = |a| Money::new(a, Currency::TRY);
        let none = BTreeMap::new();
        assert_eq!(
            outcome_for(&try_(dec!(10.00)), &none, PaymentEventKind::Paid),
            PaymentEventKind::Paid
        );
        assert_eq!(
            outcome_for(&try_(dec!(10.13)), &none, PaymentEventKind::Paid),
            PaymentEventKind::Failed
        );
        assert_eq!(
            outcome_for(&try_(dec!(10.77)), &none, PaymentEventKind::Paid),
            PaymentEventKind::Pending
        );
        let meta = BTreeMap::from([(OUTCOME_KEY.to_owned(), "Failure".to_owned())]);
        assert_eq!(
            outcome_for(&try_(dec!(10)), &meta, PaymentEventKind::Paid),
            PaymentEventKind::Failed
        );
    }

    #[tokio::test]
    async fn end_to_end_form_flow() {
        let gw = gateway();
        let session = gw
            .create_checkout(&request(Money::new(dec!(25), Currency::EUR)))
            .await
            .unwrap();
        let CheckoutSession::Form {
            action,
            method,
            fields,
            gateway_ref,
        } = session
        else {
            panic!("expected a form");
        };
        assert_eq!(action, "https://shop.example/webhooks/gateway/abc");
        assert_eq!(method, FormMethod::Post);
        assert_eq!(gateway_ref.as_deref(), Some("mock_INV42A1"));
        let out = gw.parse_webhook(&post_form(&fields)).await.unwrap();
        assert_eq!(
            out.reply,
            WebhookReply::Redirect("https://shop.example/ok".into())
        );
        let ev = &out.events[0];
        assert_eq!(ev.kind, PaymentEventKind::Paid);
        assert_eq!(ev.amount, Money::new(dec!(25), Currency::EUR));
        assert_eq!(ev.invoice_ref.as_deref(), Some("INV42A1"));
        assert_eq!(ev.event_id.as_deref(), Some("mock:mock_INV42A1:paid"));
        let pm = ev.payment_method_ref.clone().unwrap();
        assert!(ev.raw.get("payment_method_ref").is_none());

        // Stored-card renewal with the token from the first payment.
        let charge = gw
            .charge_stored(&StoredChargeRequest {
                invoice_id: InvoiceId::new(),
                reference: "INV43".into(),
                amount: Money::new(dec!(25), Currency::EUR),
                description: "Renewal".into(),
                customer: PaymentCustomer::default(),
                payment_method_ref: pm,
            })
            .await
            .unwrap();
        assert_eq!(charge.kind, PaymentEventKind::Paid);

        // Declined amount → redirected to the cancel URL.
        let CheckoutSession::Form { fields, .. } = gw
            .create_checkout(&request(Money::new(dec!(9.13), Currency::EUR)))
            .await
            .unwrap()
        else {
            panic!("expected a form");
        };
        let out = gw.parse_webhook(&post_form(&fields)).await.unwrap();
        assert_eq!(
            out.reply,
            WebhookReply::Redirect("https://shop.example/fail".into())
        );
        assert_eq!(out.events[0].kind, PaymentEventKind::Failed);
    }

    #[tokio::test]
    async fn signed_webhook_helper_and_rejections() {
        let gw = gateway();
        let event = MockEvent::new(
            PaymentEventKind::Refunded,
            "mock_INV1",
            Money::new(dec!(5), Currency::USD),
        )
        .with_reference("INV1");
        let ok = webhook_request(SECRET, &event, now_unix());
        let out = gw.parse_webhook(&ok).await.unwrap();
        assert_eq!(out.reply, WebhookReply::Empty);
        assert_eq!(out.events[0].kind, PaymentEventKind::Refunded);
        assert_eq!(out.events[0].gateway_refund_id, None);
        let with_id = event.clone().with_refund_id("mock_rf_R1");
        let out = gw
            .parse_webhook(&webhook_request(SECRET, &with_id, now_unix()))
            .await
            .unwrap();
        assert_eq!(
            out.events[0].gateway_refund_id.as_deref(),
            Some("mock_rf_R1")
        );

        let stale = webhook_request(SECRET, &event, now_unix() - 3600);
        assert!(matches!(
            gw.parse_webhook(&stale).await,
            Err(IntegrationError::Auth(_))
        ));
        let wrong = webhook_request(b"other", &event, now_unix());
        assert!(matches!(
            gw.parse_webhook(&wrong).await,
            Err(IntegrationError::Auth(_))
        ));
        let mut tampered = webhook_request(SECRET, &event, now_unix());
        tampered.body = String::from_utf8(tampered.body)
            .unwrap()
            .replace("INV1\"", "INV2\"")
            .into_bytes();
        assert!(matches!(
            gw.parse_webhook(&tampered).await,
            Err(IntegrationError::Auth(_))
        ));
        let mut fields = form_fields(SECRET, &event, now_unix());
        fields[0].1 = fields[0].1.replace("refunded", "paid");
        assert!(matches!(
            gw.parse_webhook(&post_form(&fields)).await,
            Err(IntegrationError::Auth(_))
        ));
        assert!(matches!(
            gw.parse_webhook(&WebhookRequest::default()).await,
            Err(IntegrationError::Auth(_))
        ));
        let refund = gw
            .refund(&RefundRequest {
                gateway_txn_id: "mock_INV1".into(),
                amount: Money::new(dec!(1.13), Currency::USD),
                reason: None,
                reference: "R1".into(),
                ip: None,
            })
            .await
            .unwrap();
        assert_eq!(refund.status, RefundStatus::Failed);
    }

    #[tokio::test]
    async fn card_setup_round_trip_yields_method_saved() {
        let gw = gateway();
        assert!(gw.supports_card_setup());
        let session = gw
            .create_card_setup(&CardSetupRequest {
                reference: "SETUPMOCK0001".into(),
                currency: Currency::TRY,
                customer: PaymentCustomer::default(),
                return_url: "https://shop.example/client/billing?card=saved".into(),
                cancel_url: "https://shop.example/client/billing?card=cancelled".into(),
                webhook_url: "https://shop.example/webhooks/gateway/x".into(),
                locale: None,
            })
            .await
            .unwrap();
        let CheckoutSession::Form { fields, .. } = session else {
            panic!("form expected");
        };
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let out = gw
            .parse_webhook(&WebhookRequest {
                body: body.into_bytes(),
                ..Default::default()
            })
            .await
            .unwrap();
        let ev = &out.events[0];
        assert_eq!(ev.kind, PaymentEventKind::MethodSaved);
        assert_eq!(ev.invoice_ref.as_deref(), Some("SETUPMOCK0001"));
        assert_eq!(
            ev.payment_method_ref
                .as_ref()
                .map(|s| s.expose_str().to_owned()),
            Some("mock_pm_SETUPMOCK0001".to_owned())
        );
        assert_eq!(
            out.reply,
            WebhookReply::Redirect("https://shop.example/client/billing?card=saved".into())
        );
    }
}
