//! Papara Checkout ("Ödeme Kabul Entegrasyonu") — redirect to Papara, payment
//! notification (IPN) confirmed through the API.
//!
//! ## Sources (read 2026-11-18)
//!
//! * Merchant API docs, <https://merchant-api.papara.com> (content served from
//!   <https://merchant-api.papara.com/resource/api/tr-TR>): "Authentication",
//!   "Canlı/Test Ortamı", "Ödeme Kabul Entegrasyonu (Papara Checkout)" —
//!   create payment, redirect, IPN, query, cancel, refund, status values.
//!
//! **API version:** Papara Merchant API (unversioned REST paths).
//! Auth: header `ApiKey: {api key}` on every request.
//!
//! ## Flow
//!
//! 1. `POST /payments` `{amount, referenceId, orderDescription, notificationUrl,
//!    redirectUrl, currency}` → `data.paymentUrl`, `data.id`. Unpaid records
//!    expire after 15 minutes.
//! 2. The customer pays on Papara (balance or card — card data stays on
//!    Papara). Before redirecting the customer back, Papara POSTs the payment
//!    object to `notificationUrl` (IPN) and repeats it until it gets `200 OK`
//!    (for 24 hours). IPNs are only sent for completed payments (`status` 1).
//! 3. The redirect back carries `paymentId/status/amount` in the query, which
//!    the docs flag as untrusted; HostingOS sends the customer straight back to
//!    the invoice page and never acts on it.
//!
//! ## IPN verification (fail closed)
//!
//! * The IPN body's `merchantSecretKey` must equal the merchant panel's
//!   secret key (constant-time). Papara's optional "secret key hashing"
//!   (`SHA256(PaymentId + Amount + SecretKey)`) does not document the amount
//!   or output formatting, so it is **not** supported — keep it disabled in
//!   the Papara panel.
//! * The payment is then re-read with `GET /payments?id={id}` (as the docs
//!   recommend) and only that API answer is used: `status` 1, its `amount`,
//!   `currency` and `referenceId`. The event carries
//!   [`RAW_EXACT_ATTEMPT`](crate::RAW_EXACT_ATTEMPT); event id
//!   `papara:{id}:paid` makes repeated IPNs no-ops.
//!
//! Status values: 0 pending, 1 completed, 2 refunded by the merchant.
//! Currencies: TRY (0), USD (1), EUR (2). Amount 1.00 – 500,000.00.
//!
//! ## Hosts
//!
//! Test `https://merchant-api.test.papara.com`, live `https://merchant-api.papara.com`.
//!
//! ## Refunds
//!
//! `POST /payments/refund` `{paymentId, refundAmount}` → `succeeded` (partial
//! amounts allowed). (`PUT /payments?id=` cancels; not used.)

use std::time::Duration;

use async_trait::async_trait;
use hos_core::crypto;
use hos_core::domain::IntegrationCategory;
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{
    CheckoutRequest, CheckoutSession, GatewayCapabilities, PaymentEvent, PaymentEventKind,
    PaymentGateway, RefundRequest, RefundResult, RefundStatus, WebhookOutcome, WebhookReply,
    WebhookRequest,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, kinds,
};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::common::{self, Mode, auth, ensure_positive, json_decimal, json_id, truncate};

/// Test API base.
pub const TEST_API: &str = "https://merchant-api.test.papara.com";
/// Live API base.
pub const LIVE_API: &str = "https://merchant-api.papara.com";
/// Supported currencies and their API codes.
pub const CURRENCIES: [(&str, i64); 3] = [("TRY", 0), ("USD", 1), ("EUR", 2)];
/// Unpaid payment records are deleted after this long.
const PAYMENT_LIFETIME_MINUTES: i64 = 15;
const MIN_AMOUNT: Decimal = Decimal::from_parts(100, 0, 0, false, 2);
const MAX_AMOUNT: Decimal = Decimal::from_parts(50_000_000, 0, 0, false, 2);

fn api_currency(currency: Currency) -> Result<i64, IntegrationError> {
    CURRENCIES
        .iter()
        .find(|(c, _)| *c == currency.as_str())
        .map(|(_, code)| *code)
        .ok_or_else(|| {
            IntegrationError::InvalidInput(format!("Papara does not support {currency}"))
        })
}

/// Currency of a payment object: numeric code or ISO string.
fn currency_of(value: &Value) -> Option<Currency> {
    match value {
        Value::Number(n) => {
            let code = n.as_i64()?;
            CURRENCIES
                .iter()
                .find(|(_, c)| *c == code)
                .and_then(|(iso, _)| Currency::new(iso).ok())
        }
        Value::String(s) => CURRENCIES
            .iter()
            .find(|(iso, _)| iso.eq_ignore_ascii_case(s.trim()))
            .and_then(|(iso, _)| Currency::new(iso).ok()),
        _ => None,
    }
}

/// Papara settings.
#[derive(Debug, Clone)]
pub struct PaparaSettings {
    /// API key (`ApiKey` header).
    pub api_key: SecretString,
    /// Merchant secret key (IPN `merchantSecretKey`).
    pub secret_key: SecretString,
    /// Test or live.
    pub mode: Mode,
    /// API base URL.
    pub api_base: String,
    /// HTTP timeout.
    pub timeout: Duration,
}

impl PaparaSettings {
    /// Reads settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let api_key = s.secret("api_key")?;
        let secret_key = s.secret("secret_key")?;
        for (name, value) in [("api_key", &api_key), ("secret_key", &secret_key)] {
            if value.expose_str().trim().is_empty() {
                return Err(IntegrationError::Config(format!(
                    "secrets.{name} is required"
                )));
            }
        }
        let mode = Mode::from_config(&c)?;
        let base = if mode.is_live() { LIVE_API } else { TEST_API };
        Ok(Self {
            api_key,
            secret_key,
            mode,
            api_base: common::api_base_override(config).unwrap_or_else(|| base.to_owned()),
            timeout: common::DEFAULT_TIMEOUT,
        })
    }
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::PAPARA, "Papara", IntegrationCategory::Gateway)
        .description(
            "Papara Checkout: the customer pays on Papara (balance or card). Payments are \
             confirmed by Papara's notification (IPN) to /webhooks/gateway/{integration_id}, \
             sent with every payment, and re-checked through the API. Keep \"secret key \
             hashing\" disabled in the Papara merchant panel. TRY, USD, EUR.",
        )
        .docs_url("https://merchant-api.papara.com")
        .config_field(common::mode_field().help("Test = merchant-api.test.papara.com."))
        .secret_field(
            FieldSpec::password("api_key", "API key")
                .required()
                .help("Merchant panel API key; live and test keys differ."),
        )
        .secret_field(
            FieldSpec::password("secret_key", "Secret key")
                .required()
                .help("Merchant panel secret key, sent by Papara in every notification."),
        )
}

/// Static capabilities.
pub fn capabilities() -> GatewayCapabilities {
    GatewayCapabilities {
        refunds: true,
        partial_refunds: true,
        stored_payment_methods: false,
        installments: false,
        offline: false,
        currencies: CURRENCIES
            .iter()
            .filter_map(|(c, _)| Currency::new(c).ok())
            .collect(),
    }
}

/// The Papara gateway.
#[derive(Debug)]
pub struct PaparaGateway {
    settings: PaparaSettings,
    http: reqwest::Client,
}

impl PaparaGateway {
    /// Creates the gateway.
    pub fn new(settings: PaparaSettings) -> Result<Self, IntegrationError> {
        if settings.api_key.expose_str().is_empty() || settings.secret_key.expose_str().is_empty() {
            return Err(IntegrationError::Config(
                "secrets.api_key and secrets.secret_key are required".into(),
            ));
        }
        let http = common::http_client(settings.timeout)?;
        Ok(Self { settings, http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.settings.api_base.trim_end_matches('/'))
    }

    /// Sends a request; returns `data` of a `succeeded` envelope.
    async fn call(
        &self,
        request: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<Value, IntegrationError> {
        let response = common::send(
            request
                .header("ApiKey", self.settings.api_key.expose_str())
                .header(http::header::ACCEPT, "application/json"),
        )
        .await?;
        let json = response.json()?;
        let message = json["error"]["message"]
            .as_str()
            .map(|m| truncate(m, 200))
            .unwrap_or_else(|| format!("Papara {what} failed"));
        if !response.is_success() {
            return Err(common::status_error(
                response.status,
                response.retry_after,
                message,
                json_id(&json["error"]["code"]),
            ));
        }
        if json["succeeded"].as_bool() != Some(true) {
            return Err(IntegrationError::Remote {
                status: Some(response.status),
                code: json_id(&json["error"]["code"]),
                message,
            });
        }
        Ok(json["data"].clone())
    }

    async fn payment(&self, id: &str) -> Result<Value, IntegrationError> {
        self.call(
            self.http.get(self.url("/payments")).query(&[("id", id)]),
            "payment query",
        )
        .await
    }
}

#[async_trait]
impl PaymentGateway for PaparaGateway {
    fn kind(&self) -> &'static str {
        kinds::PAPARA
    }

    fn capabilities(&self) -> GatewayCapabilities {
        capabilities()
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let data = self
            .call(self.http.get(self.url("/account")), "account query")
            .await?;
        Ok(ConnectionInfo {
            server_version: Some("Papara Merchant API".into()),
            details: Some(format!(
                "{} ({})",
                data["legalName"]
                    .as_str()
                    .or(data["brandName"].as_str())
                    .map_or_else(|| "authenticated".to_owned(), |n| truncate(n, 80)),
                if self.settings.mode.is_live() {
                    "live"
                } else {
                    "test"
                }
            )),
        })
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        ensure_positive(&request.amount)?;
        let amount = request.amount.round();
        if amount.amount < MIN_AMOUNT || amount.amount > MAX_AMOUNT {
            return Err(IntegrationError::InvalidInput(
                "Papara accepts 1.00 – 500,000.00 per payment".into(),
            ));
        }
        let currency = api_currency(amount.currency)?;
        let reference = request.reference.trim();
        if reference.is_empty() || reference.len() > 100 {
            return Err(IntegrationError::InvalidInput(
                "Papara referenceId must be 1–100 characters".into(),
            ));
        }
        common::http_url(&request.webhook_url, "webhook_url")?;
        common::http_url(&request.return_url, "return_url")?;
        let decimal: f64 = amount
            .to_decimal_string()
            .parse()
            .map_err(|_| IntegrationError::InvalidInput("amount is out of range".into()))?;
        let body = json!({
            "amount": decimal,
            "referenceId": reference,
            "orderDescription": truncate(&request.description, 100),
            "notificationUrl": request.webhook_url,
            "redirectUrl": request.return_url,
            "currency": currency,
        });
        let data = self
            .call(
                self.http.post(self.url("/payments")).json(&body),
                "payment creation",
            )
            .await?;
        let (Some(url), Some(id)) = (data["paymentUrl"].as_str(), json_id(&data["id"])) else {
            return Err(IntegrationError::Remote {
                status: None,
                code: None,
                message: "Papara returned no payment URL".into(),
            });
        };
        let url = common::http_url(url, "paymentUrl")?;
        if url.scheme() != "https" {
            return Err(IntegrationError::Remote {
                status: None,
                code: None,
                message: "Papara payment URL is not https".into(),
            });
        }
        Ok(CheckoutSession::Redirect {
            url: url.into(),
            gateway_ref: Some(id),
            expires_at: Some(
                chrono::Utc::now() + chrono::Duration::minutes(PAYMENT_LIFETIME_MINUTES),
            ),
        })
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let secret = self.settings.secret_key.expose_str();
        if secret.is_empty() {
            return Err(auth("Papara secret key is not configured"));
        }
        let body: Value = serde_json::from_slice(&request.body)
            .map_err(|_| auth("Papara notification is not JSON"))?;
        let Some(sent) = body["merchantSecretKey"].as_str().filter(|s| !s.is_empty()) else {
            return Err(auth("Papara notification has no merchantSecretKey"));
        };
        if !crypto::constant_time_eq(sent.as_bytes(), secret.as_bytes()) {
            return Err(auth("Papara notification secret mismatch"));
        }
        let Some(id) = json_id(&body["id"]) else {
            return Err(auth("Papara notification has no payment id"));
        };
        // Authoritative state from the API, never from the notification body.
        let data = self.payment(&id).await?;
        if json_id(&data["id"]).as_deref() != Some(id.as_str()) {
            return Err(auth("Papara API returned another payment"));
        }
        if data["status"].as_i64() != Some(1) {
            tracing::info!(payment_id = %id, status = %data["status"], "Papara: notification for a payment that is not completed");
            return Ok(WebhookOutcome::default());
        }
        let currency = currency_of(&data["currency"])
            .ok_or_else(|| auth("Papara payment has an unknown currency"))?;
        let amount = json_decimal(&data["amount"])
            .filter(|a| *a > Decimal::ZERO)
            .map(|a| Money::new(a, currency))
            .ok_or_else(|| auth("Papara payment has no amount"))?;
        let reference = data["referenceId"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_owned);
        let fee = json_decimal(&data["fee"])
            .filter(|f| *f > Decimal::ZERO)
            .map(|f| Money::new(f, currency));
        let mut raw = json!({
            "id": id,
            "referenceId": reference,
            "status": data["status"],
            "amount": data["amount"],
            "fee": data["fee"],
            "currency": data["currency"],
            "paymentMethod": data["paymentMethod"],
            "createdAt": data["createdAt"],
        });
        crate::tag_raw(&mut raw, crate::RAW_EXACT_ATTEMPT, Value::Bool(true));
        Ok(WebhookOutcome {
            events: vec![PaymentEvent {
                kind: PaymentEventKind::Paid,
                event_id: Some(format!("papara:{id}:paid")),
                gateway_txn_id: id,
                invoice_ref: reference,
                amount,
                fee,
                occurred_at: data["createdAt"]
                    .as_str()
                    .and_then(|t| t.parse::<chrono::DateTime<chrono::Utc>>().ok()),
                message: None,
                payment_method_ref: None,
                gateway_refund_id: None,
                raw,
            }],
            reply: WebhookReply::Empty,
        })
    }

    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        ensure_positive(&request.amount)?;
        api_currency(request.amount.currency)?;
        let body = json!({
            "paymentId": request.gateway_txn_id.trim(),
            "refundAmount": request.amount.round().to_decimal_string(),
        });
        match self
            .call(
                self.http.post(self.url("/payments/refund")).json(&body),
                "refund",
            )
            .await
        {
            Ok(data) => Ok(RefundResult {
                gateway_refund_id: None,
                status: RefundStatus::Succeeded,
                amount: request.amount,
                message: None,
                raw: json!({"succeeded": true, "data": data}),
            }),
            Err(IntegrationError::Remote { code, message, .. }) => Ok(RefundResult {
                gateway_refund_id: None,
                status: RefundStatus::Failed,
                amount: request.amount,
                message: Some(message),
                raw: json!({"succeeded": false, "code": code}),
            }),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::ids::InvoiceId;
    use hos_core::integrations::gateway::PaymentCustomer;
    use http::HeaderMap;
    use rust_decimal_macros::dec;
    use std::collections::BTreeMap;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "papara-secret-1";

    fn settings(base: &str) -> PaparaSettings {
        let mut s = PaparaSettings::from_values(
            &json!({"mode": "test"}),
            &json!({"api_key": "apikey-1", "secret_key": SECRET}),
        )
        .unwrap();
        s.api_base = base.to_owned();
        s
    }

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "HOS55PAP".into(),
            amount: Money::new(dec!(99.99), Currency::TRY),
            description: "Fatura INV-55".into(),
            customer: PaymentCustomer::default(),
            items: Vec::new(),
            return_url: "https://shop.example/client/invoices/55".into(),
            cancel_url: "https://shop.example/client/invoices/55".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: None,
            save_payment_method: false,
            metadata: BTreeMap::new(),
        }
    }

    fn ipn(secret: &str, id: &str) -> WebhookRequest {
        WebhookRequest {
            headers: HeaderMap::new(),
            body: json!({
                "merchantId": "123-4564-8484", "id": id, "referenceId": "HOS55PAP",
                "status": 1, "amount": 99.99, "currency": "TRY", "merchantSecretKey": secret,
                "cardIssuer": "Ziraat Bankasi", "cardBrand": "Bankkart"
            })
            .to_string()
            .into_bytes(),
            query: None,
            remote_ip: None,
        }
    }

    async fn payment_mock(server: &MockServer, id: &str, status: i64, amount: f64) {
        Mock::given(method("GET"))
            .and(path("/payments"))
            .and(query_param("id", id))
            .and(header("apikey", "apikey-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": id, "referenceId": "HOS55PAP", "status": status,
                         "amount": amount, "fee": 1.98, "currency": 0,
                         "createdAt": "2026-11-18T10:00:00Z", "merchantSecretKey": null},
                "succeeded": true
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn create_payment_and_redirect() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/payments"))
            .and(header("apikey", "apikey-1"))
            .and(body_json(json!({
                "amount": 99.99, "referenceId": "HOS55PAP", "orderDescription": "Fatura INV-55",
                "notificationUrl": "https://shop.example/webhooks/gateway/abc",
                "redirectUrl": "https://shop.example/client/invoices/55", "currency": 0
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "0854a018-34b2", "paymentUrl": "https://test.papara.com/checkout/0854a018-34b2",
                         "status": 0, "amount": 99.99, "currency": 0, "merchantSecretKey": null},
                "succeeded": true
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = PaparaGateway::new(settings(&server.uri())).unwrap();
        let CheckoutSession::Redirect {
            url, gateway_ref, ..
        } = gw.create_checkout(&request()).await.unwrap()
        else {
            panic!("redirect expected")
        };
        assert_eq!(url, "https://test.papara.com/checkout/0854a018-34b2");
        assert_eq!(gateway_ref.as_deref(), Some("0854a018-34b2"));
        // Amount limits.
        let mut small = request();
        small.amount = Money::new(dec!(0.50), Currency::TRY);
        assert!(gw.create_checkout(&small).await.is_err());
        small.amount = Money::new(dec!(5), Currency::GBP);
        assert!(gw.create_checkout(&small).await.is_err());
    }

    #[tokio::test]
    async fn ipn_verified_through_api() {
        let server = MockServer::start().await;
        payment_mock(&server, "pay-1", 1, 99.99).await;
        let gw = PaparaGateway::new(settings(&server.uri())).unwrap();
        let out = gw.parse_webhook(&ipn(SECRET, "pay-1")).await.unwrap();
        assert_eq!(out.reply, WebhookReply::Empty);
        let e = &out.events[0];
        assert_eq!(e.kind, PaymentEventKind::Paid);
        assert_eq!(e.event_id.as_deref(), Some("papara:pay-1:paid"));
        assert_eq!(e.invoice_ref.as_deref(), Some("HOS55PAP"));
        assert_eq!(e.amount, Money::new(dec!(99.99), Currency::TRY));
        assert_eq!(e.fee, Some(Money::new(dec!(1.98), Currency::TRY)));
        assert!(crate::event_requires_exact_attempt(e));
        assert!(!e.raw.to_string().contains("Bankkart"));
        // Replayed IPN: same event id (billing dedupes).
        let again = gw.parse_webhook(&ipn(SECRET, "pay-1")).await.unwrap();
        assert_eq!(again.events[0].event_id, e.event_id);
    }

    #[tokio::test]
    async fn ipn_amount_comes_from_api_not_body() {
        let server = MockServer::start().await;
        // The body claims 99.99; the API says 9.99 — the API wins.
        payment_mock(&server, "pay-2", 1, 9.99).await;
        payment_mock(&server, "pay-3", 0, 99.99).await;
        let gw = PaparaGateway::new(settings(&server.uri())).unwrap();
        let out = gw.parse_webhook(&ipn(SECRET, "pay-2")).await.unwrap();
        assert_eq!(out.events[0].amount, Money::new(dec!(9.99), Currency::TRY));
        // Not completed per the API: nothing is applied.
        let out = gw.parse_webhook(&ipn(SECRET, "pay-3")).await.unwrap();
        assert!(out.events.is_empty());
    }

    #[tokio::test]
    async fn bad_or_foreign_secret_rejected_without_api_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let gw = PaparaGateway::new(settings(&server.uri())).unwrap();
        for secret in ["", "wrong", "papara-secret-2"] {
            assert!(matches!(
                gw.parse_webhook(&ipn(secret, "pay-1")).await,
                Err(IntegrationError::Auth(_))
            ));
        }
        let junk = WebhookRequest {
            headers: HeaderMap::new(),
            body: b"paymentId=pay-1&status=1".to_vec(),
            query: None,
            remote_ip: None,
        };
        assert!(gw.parse_webhook(&junk).await.is_err());
    }

    #[tokio::test]
    async fn unknown_payment_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/payments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null, "succeeded": false, "error": {"message": "Payment not found", "code": 997}
            })))
            .mount(&server)
            .await;
        let gw = PaparaGateway::new(settings(&server.uri())).unwrap();
        assert!(
            gw.parse_webhook(&ipn(SECRET, "other-merchant"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn refund_success_and_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/payments/refund"))
            .and(body_json(
                json!({"paymentId": "pay-1", "refundAmount": "40.00"}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": null, "succeeded": true})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/payments/refund"))
            .and(body_json(json!({"paymentId": "pay-9", "refundAmount": "40.00"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null, "succeeded": false, "error": {"message": "Refund amount too high", "code": 105}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = PaparaGateway::new(settings(&server.uri())).unwrap();
        let req = RefundRequest {
            gateway_txn_id: "pay-1".into(),
            amount: Money::new(dec!(40), Currency::TRY),
            reason: None,
            reference: "r1".into(),
            ip: None,
        };
        assert_eq!(
            gw.refund(&req).await.unwrap().status,
            RefundStatus::Succeeded
        );
        let failed = gw
            .refund(&RefundRequest {
                gateway_txn_id: "pay-9".into(),
                ..req
            })
            .await
            .unwrap();
        assert_eq!(failed.status, RefundStatus::Failed);
        assert_eq!(failed.message.as_deref(), Some("Refund amount too high"));
    }

    #[tokio::test]
    async fn connection_test() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/account"))
            .and(header("apikey", "apikey-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"legalName": "Örnek A.Ş.", "balances": []}, "succeeded": true
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/account"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "succeeded": false, "error": {"message": "Unauthorized", "code": 401}
            })))
            .mount(&server)
            .await;
        let gw = PaparaGateway::new(settings(&server.uri())).unwrap();
        assert!(
            gw.test_connection()
                .await
                .unwrap()
                .details
                .unwrap()
                .contains("Örnek")
        );
        let mut s = settings(&server.uri());
        s.api_key = "bad".into();
        assert!(matches!(
            PaparaGateway::new(s).unwrap().test_connection().await,
            Err(IntegrationError::Auth(_))
        ));
    }

    #[test]
    fn empty_secrets_rejected() {
        let c = json!({"mode": "test"});
        assert!(
            PaparaSettings::from_values(&c, &json!({"api_key": "k", "secret_key": ""})).is_err()
        );
        assert!(
            PaparaSettings::from_values(&c, &json!({"api_key": " ", "secret_key": "s"})).is_err()
        );
        assert!(PaparaSettings::from_values(&c, &json!({"api_key": "k"})).is_err());
        assert_eq!(
            PaparaSettings::from_values(
                &json!({"mode": "live"}),
                &json!({"api_key": "k", "secret_key": "s"})
            )
            .unwrap()
            .api_base,
            LIVE_API
        );
        assert_eq!(currency_of(&json!("EUR")), Some(Currency::EUR));
        assert_eq!(currency_of(&json!(1)), Some(Currency::USD));
        assert_eq!(currency_of(&json!(7)), None);
    }
}
