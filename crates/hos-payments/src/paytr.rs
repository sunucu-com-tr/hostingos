//! PayTR iFrame API (dev.paytr.com › iFrame API, İade API).
//!
//! * Step 1: `POST https://www.paytr.com/odeme/api/get-token` (form) with
//!   `paytr_token = base64(HMAC-SHA256(merchant_id + user_ip + merchant_oid + email +
//!   payment_amount + user_basket + no_installment + max_installment + currency +
//!   test_mode + merchant_salt, merchant_key))`; the customer pays inside
//!   `https://www.paytr.com/odeme/guvenli/{token}`.
//! * Step 2: PayTR POSTs the result to the "Bildirim URL" configured in the
//!   merchant panel; `hash = base64(HMAC-SHA256(merchant_oid + merchant_salt +
//!   status + total_amount, merchant_key))`, and the reply must be exactly `OK`.
//! * Refund: `POST https://www.paytr.com/odeme/iade` with
//!   `paytr_token = base64(HMAC-SHA256(merchant_id + merchant_oid + return_amount + merchant_salt, merchant_key))`.
//!
//! `merchant_oid` (= [`CheckoutRequest::reference`], alphanumeric ≤ 64) is both
//! the invoice reference and `gateway_txn_id`; PayTR has no other transaction id.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hos_core::crypto;
use hos_core::domain::{IntegrationCategory, Locale};
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{
    CheckoutRequest, CheckoutSession, GatewayCapabilities, PaymentEvent, PaymentEventKind,
    PaymentGateway, RefundRequest, RefundResult, RefundStatus, WebhookOutcome, WebhookReply,
    WebhookRequest,
};
use hos_core::integrations::{FieldReader, FieldSpec, IntegrationDescriptor, kinds};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use serde_json::{Value, json};

use crate::common::{self, Mode, auth, ensure_positive, is_alphanumeric, json_id, truncate};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// API base URL.
pub const API_BASE: &str = "https://www.paytr.com";
/// iFrame page prefix; the token is appended.
pub const IFRAME_BASE: &str = "https://www.paytr.com/odeme/guvenli/";
/// Currencies accepted by the iFrame API (`TL` on the wire for TRY).
pub const CURRENCIES: [&str; 5] = ["TRY", "USD", "EUR", "GBP", "RUB"];

/// `base64(HMAC-SHA256(message, merchant_key))`, PayTR's token/hash format.
pub fn paytr_hash(merchant_key: &str, message: &str) -> String {
    crypto::hmac_sha256_base64(merchant_key.as_bytes(), message.as_bytes())
}

/// Expected `hash` of a payment notification.
pub fn notification_hash(
    merchant_key: &str,
    merchant_salt: &str,
    merchant_oid: &str,
    status: &str,
    total_amount: &str,
) -> String {
    paytr_hash(
        merchant_key,
        &format!("{merchant_oid}{merchant_salt}{status}{total_amount}"),
    )
}

fn wire_currency(currency: Currency) -> Result<&'static str, IntegrationError> {
    match currency.as_str() {
        "TRY" => Ok("TL"),
        "USD" => Ok("USD"),
        "EUR" => Ok("EUR"),
        "GBP" => Ok("GBP"),
        "RUB" => Ok("RUB"),
        other => Err(IntegrationError::InvalidInput(format!(
            "PayTR does not support {other}"
        ))),
    }
}

fn currency_from_wire(code: &str) -> Option<Currency> {
    match code.trim() {
        "" | "TL" => Some(Currency::TRY),
        other => Currency::new(other).ok(),
    }
}

/// PayTR settings.
#[derive(Debug, Clone)]
pub struct PaytrSettings {
    /// Mağaza no.
    pub merchant_id: String,
    /// Merchant key.
    pub merchant_key: SecretString,
    /// Merchant salt.
    pub merchant_salt: SecretString,
    /// `test_mode=1` on every request.
    pub test_mode: bool,
    /// `debug_on=1` (PayTR shows integration errors).
    pub debug_on: bool,
    /// Single payment only.
    pub no_installment: bool,
    /// Max installments (0 = PayTR default).
    pub max_installment: u32,
    /// Payment timeout in minutes.
    pub timeout_limit_minutes: u32,
    /// API base URL (overridable for tests).
    pub api_base: String,
    /// HTTP timeout.
    pub timeout: Duration,
}

impl PaytrSettings {
    /// Settings with defaults (test mode).
    pub fn new(
        merchant_id: impl Into<String>,
        merchant_key: SecretString,
        merchant_salt: SecretString,
    ) -> Self {
        Self {
            merchant_id: merchant_id.into(),
            merchant_key,
            merchant_salt,
            test_mode: true,
            debug_on: false,
            no_installment: false,
            max_installment: 0,
            timeout_limit_minutes: 30,
            api_base: API_BASE.to_owned(),
            // PayTR's own samples allow 20 s (get-token) and 90 s (refund).
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// Reads settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let mut settings = Self::new(
            c.str("merchant_id")?,
            s.secret("merchant_key")?,
            s.secret("merchant_salt")?,
        );
        if !settings.merchant_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(IntegrationError::Config(
                "config.merchant_id must be numeric".into(),
            ));
        }
        settings.test_mode = !Mode::from_config(&c)?.is_live();
        settings.debug_on = c.bool_or("debug_on", false)?;
        settings.no_installment = c.bool_or("no_installment", false)?;
        settings.max_installment = u32::try_from(c.u64_or("max_installment", 0)?)
            .ok()
            .filter(|n| *n == 0 || (2..=12).contains(n))
            .ok_or_else(|| {
                IntegrationError::Config("config.max_installment must be 0 or 2–12".into())
            })?;
        settings.timeout_limit_minutes = u32::try_from(c.u64_or("timeout_limit", 30)?)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| IntegrationError::Config("config.timeout_limit must be > 0".into()))?;
        Ok(settings)
    }
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::PAYTR, "PayTR", IntegrationCategory::Gateway)
        .description(
            "PayTR iFrame API (card data stays on PayTR). Set the notification URL \
             (Bildirim URL) in the PayTR panel to /webhooks/gateway/{integration_id}. \
             Currencies: TRY, USD, EUR, GBP, RUB (non-TRY must be enabled by PayTR). \
             Requires the customer's IP, phone and address.",
        )
        .docs_url("https://dev.paytr.com/iframe-api")
        .config_field(FieldSpec::text("merchant_id", "Merchant id (Mağaza no)").required())
        .config_field(common::mode_field().help("Test = test_mode=1 on every request."))
        .config_field(
            FieldSpec::boolean("no_installment", "Single payment only").default_value(false),
        )
        .config_field(
            FieldSpec::number("max_installment", "Max installments")
                .default_value(0)
                .help("0 = PayTR default, otherwise 2–12."),
        )
        .config_field(
            FieldSpec::number("timeout_limit", "Payment timeout (minutes)").default_value(30),
        )
        .config_field(
            FieldSpec::boolean("debug_on", "Show integration errors").default_value(false),
        )
        .secret_field(FieldSpec::password("merchant_key", "Merchant key").required())
        .secret_field(FieldSpec::password("merchant_salt", "Merchant salt").required())
}

/// Static capabilities.
pub fn capabilities() -> GatewayCapabilities {
    GatewayCapabilities {
        refunds: true,
        partial_refunds: true,
        stored_payment_methods: false,
        installments: true,
        offline: false,
        currencies: CURRENCIES
            .iter()
            .filter_map(|c| Currency::new(c).ok())
            .collect(),
    }
}

/// The PayTR gateway.
#[derive(Debug)]
pub struct PaytrGateway {
    settings: PaytrSettings,
    http: reqwest::Client,
}

impl PaytrGateway {
    /// Creates the gateway.
    pub fn new(settings: PaytrSettings) -> Result<Self, IntegrationError> {
        let http = common::http_client(settings.timeout)?;
        Ok(Self { settings, http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.settings.api_base.trim_end_matches('/'))
    }

    fn key(&self) -> &str {
        self.settings.merchant_key.expose_str()
    }

    fn salt(&self) -> &str {
        self.settings.merchant_salt.expose_str()
    }

    /// `user_basket`: base64 of `[[name, unit price, quantity], …]`.
    fn basket(request: &CheckoutRequest) -> Result<String, IntegrationError> {
        let mut lines: Vec<(String, String, u32)> = request
            .items
            .iter()
            .filter(|i| i.quantity > 0)
            .map(|i| {
                (
                    truncate(&i.name, 100),
                    i.unit_price.to_decimal_string(),
                    i.quantity,
                )
            })
            .collect();
        if lines.is_empty() {
            lines.push((
                truncate(&request.description, 100),
                request.amount.to_decimal_string(),
                1,
            ));
        }
        let json = serde_json::to_string(&lines)
            .map_err(|e| IntegrationError::Other(format!("JSON encoding failed: {e}")))?;
        Ok(STANDARD.encode(json))
    }
}

#[async_trait]
impl PaymentGateway for PaytrGateway {
    fn kind(&self) -> &'static str {
        kinds::PAYTR
    }

    fn capabilities(&self) -> GatewayCapabilities {
        GatewayCapabilities {
            installments: !self.settings.no_installment,
            ..capabilities()
        }
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        ensure_positive(&request.amount)?;
        let oid = request.reference.as_str();
        if !is_alphanumeric(oid) || oid.len() > 64 {
            return Err(IntegrationError::InvalidInput(
                "PayTR merchant_oid (reference) must be alphanumeric, at most 64 characters".into(),
            ));
        }
        let currency = wire_currency(request.amount.currency)?;
        let customer = &request.customer;
        let user_ip = customer.ip.map(|ip| ip.to_string()).ok_or_else(|| {
            IntegrationError::InvalidInput("PayTR requires the customer IP".into())
        })?;
        let email = customer.email.trim();
        if email.is_empty() || email.len() > 100 || !email.is_ascii() {
            return Err(IntegrationError::InvalidInput(
                "PayTR requires an ASCII customer email of at most 100 characters".into(),
            ));
        }
        let phone = customer
            .phone
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                IntegrationError::InvalidInput("PayTR requires the customer phone".into())
            })?;
        let address = customer
            .billing_address
            .as_ref()
            .map(|a| {
                [
                    Some(a.line1.as_str()),
                    a.line2.as_deref(),
                    a.postal_code.as_deref(),
                    Some(a.city.as_str()),
                    a.state.as_deref(),
                    Some(a.country_code.as_str()),
                ]
                .into_iter()
                .flatten()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
            })
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                IntegrationError::InvalidInput("PayTR requires the customer address".into())
            })?;
        common::http_url(&request.return_url, "return_url")?;
        common::http_url(&request.cancel_url, "cancel_url")?;

        let payment_amount = request
            .amount
            .to_minor_units()
            .map_err(|_| IntegrationError::InvalidInput("amount is out of range".into()))?
            .to_string();
        let basket = Self::basket(request)?;
        let flag = |b: bool| if b { "1" } else { "0" };
        let no_installment = flag(self.settings.no_installment);
        let max_installment = self.settings.max_installment.to_string();
        let test_mode = flag(self.settings.test_mode);
        let hash_str = format!(
            "{}{user_ip}{oid}{email}{payment_amount}{basket}{no_installment}{max_installment}{currency}{test_mode}{}",
            self.settings.merchant_id,
            self.salt()
        );
        let token = paytr_hash(self.key(), &hash_str);
        let lang = match request.locale {
            Some(Locale::En) => "en",
            _ => "tr",
        };
        let form: Vec<(&str, String)> = vec![
            ("merchant_id", self.settings.merchant_id.clone()),
            ("user_ip", user_ip),
            ("merchant_oid", oid.to_owned()),
            ("email", email.to_owned()),
            ("payment_amount", payment_amount),
            ("paytr_token", token),
            ("user_basket", basket),
            ("debug_on", flag(self.settings.debug_on).to_owned()),
            ("no_installment", no_installment.to_owned()),
            ("max_installment", max_installment),
            ("user_name", truncate(customer.name.trim(), 60)),
            ("user_address", truncate(&address, 400)),
            ("user_phone", truncate(phone, 20)),
            ("merchant_ok_url", request.return_url.clone()),
            ("merchant_fail_url", request.cancel_url.clone()),
            (
                "timeout_limit",
                self.settings.timeout_limit_minutes.to_string(),
            ),
            ("currency", currency.to_owned()),
            ("test_mode", test_mode.to_owned()),
            ("lang", lang.to_owned()),
        ];
        let response =
            common::send(self.http.post(self.url("/odeme/api/get-token")).form(&form)).await?;
        let json = response.json()?;
        if !response.is_success() {
            return Err(common::status_error(
                response.status,
                response.retry_after,
                "PayTR get-token failed".into(),
                None,
            ));
        }
        match (json["status"].as_str(), json["token"].as_str()) {
            (Some("success"), Some(token)) if !token.is_empty() => Ok(CheckoutSession::Iframe {
                url: format!("{IFRAME_BASE}{token}"),
                gateway_ref: Some(token.to_owned()),
            }),
            _ => Err(IntegrationError::Remote {
                status: None,
                code: None,
                message: json["reason"]
                    .as_str()
                    .unwrap_or("PayTR did not return a token")
                    .to_owned(),
            }),
        }
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let field = |name: &str| request.form_field(name);
        let (Some(oid), Some(status), Some(total), Some(hash)) = (
            field("merchant_oid"),
            field("status"),
            field("total_amount"),
            field("hash"),
        ) else {
            return Err(auth("PayTR notification is missing signed fields"));
        };
        let expected = notification_hash(self.key(), self.salt(), &oid, &status, &total);
        if !crypto::constant_time_eq(expected.as_bytes(), hash.trim().as_bytes()) {
            return Err(auth("PayTR notification hash mismatch"));
        }
        let kind = match status.as_str() {
            "success" => PaymentEventKind::Paid,
            "failed" => PaymentEventKind::Failed,
            // Signed but unknown: acknowledge so PayTR stops retrying, apply nothing.
            other => {
                tracing::warn!(merchant_oid = %oid, status = other, "PayTR: unknown notification status");
                return Ok(WebhookOutcome {
                    events: Vec::new(),
                    reply: WebhookReply::Text("OK".into()),
                });
            }
        };
        let units: i64 = total.trim().parse().map_err(|_| {
            IntegrationError::InvalidInput("PayTR total_amount is not an integer".into())
        })?;
        let currency =
            currency_from_wire(&field("currency").unwrap_or_default()).ok_or_else(|| {
                IntegrationError::InvalidInput("PayTR notification has an unknown currency".into())
            })?;
        let message = (kind == PaymentEventKind::Failed).then(|| {
            match (field("failed_reason_code"), field("failed_reason_msg")) {
                (Some(code), Some(msg)) => format!("{msg} ({code})"),
                (None, Some(msg)) => msg,
                (Some(code), None) => format!("failed ({code})"),
                (None, None) => "payment failed".to_owned(),
            }
        });
        let payment_amount = field("payment_amount");
        let mut raw = json!({
            "merchant_oid": oid,
            "status": status,
            "total_amount": total,
            "payment_amount": payment_amount,
            "currency": field("currency"),
            "payment_type": field("payment_type"),
            "test_mode": field("test_mode"),
            "failed_reason_code": field("failed_reason_code"),
            "failed_reason_msg": field("failed_reason_msg"),
        });
        // `total_amount` (signed) is what was collected, including installment
        // interest the customer paid; `payment_amount` is the order amount we
        // asked for. Only the order amount pays the invoice — the interest is
        // recorded in `raw` and never becomes client credit. `payment_amount`
        // is not covered by the hash, so it can only lower the amount
        // (`min(order, collected)`).
        let collected = Money::from_minor_units(units, currency);
        let order = payment_amount
            .as_deref()
            .and_then(|p| p.trim().parse::<i64>().ok())
            .filter(|u| *u > 0)
            .map(|u| Money::from_minor_units(u, currency));
        let amount = crate::split_installment_interest(&mut raw, order, collected);
        Ok(WebhookOutcome {
            events: vec![PaymentEvent {
                kind,
                event_id: Some(format!("paytr:{oid}:{status}")),
                gateway_txn_id: oid.clone(),
                invoice_ref: Some(oid),
                amount,
                fee: None,
                occurred_at: None,
                message,
                payment_method_ref: None,
                gateway_refund_id: None,
                raw,
            }],
            reply: WebhookReply::Text("OK".into()),
        })
    }

    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        ensure_positive(&request.amount)?;
        wire_currency(request.amount.currency)?;
        let oid = request.gateway_txn_id.trim();
        let return_amount = request.amount.to_decimal_string();
        let token = paytr_hash(
            self.key(),
            &format!(
                "{}{oid}{return_amount}{}",
                self.settings.merchant_id,
                self.salt()
            ),
        );
        let mut form: Vec<(&str, String)> = vec![
            ("merchant_id", self.settings.merchant_id.clone()),
            ("merchant_oid", oid.to_owned()),
            ("return_amount", return_amount),
            ("paytr_token", token),
        ];
        let reference: String = request
            .reference
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(64)
            .collect();
        if !reference.is_empty() {
            form.push(("reference_no", reference));
        }
        let response = common::send(self.http.post(self.url("/odeme/iade")).form(&form)).await?;
        let json = response.json()?;
        if !response.is_success() {
            return Err(common::status_error(
                response.status,
                response.retry_after,
                "PayTR refund failed".into(),
                None,
            ));
        }
        if json["status"].as_str() == Some("success") {
            Ok(RefundResult {
                gateway_refund_id: json_id(&json["reference_no"]),
                status: RefundStatus::Succeeded,
                amount: request.amount,
                message: None,
                raw: json,
            })
        } else {
            Err(IntegrationError::Remote {
                status: None,
                code: json_id(&json["err_no"]),
                message: json["err_msg"]
                    .as_str()
                    .unwrap_or("PayTR refund rejected")
                    .to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::ids::InvoiceId;
    use hos_core::integrations::PostalAddress;
    use hos_core::integrations::gateway::{CheckoutItem, PaymentCustomer};
    use rust_decimal_macros::dec;
    use std::collections::BTreeMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn settings(base: &str) -> PaytrSettings {
        let mut s = PaytrSettings::new("123456", "KEYKEYKEY".into(), "SALTSALT".into());
        s.api_base = base.to_owned();
        s
    }

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "INV42A1".into(),
            amount: Money::new(dec!(34.56), Currency::TRY),
            description: "Fatura".into(),
            customer: PaymentCustomer {
                name: "Ayşe Yılmaz".into(),
                email: "ayse@example.com".into(),
                phone: Some("05551234567".into()),
                ip: Some("85.34.78.112".parse().unwrap()),
                billing_address: Some(PostalAddress {
                    line1: "Bora Sok. 1".into(),
                    city: "İstanbul".into(),
                    country_code: "TR".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            items: vec![CheckoutItem {
                reference: "1".into(),
                name: "Örnek ürün".into(),
                category: None,
                quantity: 2,
                unit_price: Money::new(dec!(17.28), Currency::TRY),
                total: Money::new(dec!(34.56), Currency::TRY),
            }],
            return_url: "https://shop.example/ok".into(),
            cancel_url: "https://shop.example/fail".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: None,
            save_payment_method: false,
            metadata: BTreeMap::new(),
        }
    }

    fn form(body: &[u8]) -> Vec<(String, String)> {
        url::form_urlencoded::parse(body).into_owned().collect()
    }

    fn get<'a>(f: &'a [(String, String)], k: &str) -> &'a str {
        f.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    #[test]
    fn hash_known_vectors() {
        // Python: base64.b64encode(hmac.new(b"KEYKEYKEY", b"INV42A1SALTSALTsuccess3456", sha256).digest())
        assert_eq!(
            notification_hash("KEYKEYKEY", "SALTSALT", "INV42A1", "success", "3456"),
            crypto::hmac_sha256_base64(b"KEYKEYKEY", b"INV42A1SALTSALTsuccess3456")
        );
        // RFC 4231 test case 2 in PayTR's base64 form.
        assert_eq!(
            paytr_hash("Jefe", "what do ya want for nothing?"),
            "W9zBRr9gdU5qBCQmCJV1x1oAPwidJzmDnexYuWTsOEM="
        );
    }

    #[tokio::test]
    async fn get_token_request_and_iframe() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/odeme/api/get-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "token": "28cc613c3d7633cfa4ed0956fdf901e05cf9d9cc0c2ef8db54fa"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = PaytrGateway::new(settings(&server.uri())).unwrap();
        let session = gw.create_checkout(&request()).await.unwrap();
        assert_eq!(
            session,
            CheckoutSession::Iframe {
                url: "https://www.paytr.com/odeme/guvenli/28cc613c3d7633cfa4ed0956fdf901e05cf9d9cc0c2ef8db54fa".into(),
                gateway_ref: Some("28cc613c3d7633cfa4ed0956fdf901e05cf9d9cc0c2ef8db54fa".into()),
            }
        );
        let f = form(&server.received_requests().await.unwrap()[0].body);
        assert_eq!(get(&f, "payment_amount"), "3456");
        assert_eq!(get(&f, "currency"), "TL");
        assert_eq!(get(&f, "test_mode"), "1");
        assert_eq!(get(&f, "merchant_oid"), "INV42A1");
        let basket = String::from_utf8(STANDARD.decode(get(&f, "user_basket")).unwrap()).unwrap();
        assert_eq!(basket, r#"[["Örnek ürün","17.28",2]]"#);
        // paytr_token recomputed from the sent fields exactly as the docs specify.
        let hash_str = format!(
            "123456{}{}{}{}{}{}{}{}{}SALTSALT",
            get(&f, "user_ip"),
            get(&f, "merchant_oid"),
            get(&f, "email"),
            get(&f, "payment_amount"),
            get(&f, "user_basket"),
            get(&f, "no_installment"),
            get(&f, "max_installment"),
            get(&f, "currency"),
            get(&f, "test_mode"),
        );
        assert_eq!(get(&f, "paytr_token"), paytr_hash("KEYKEYKEY", &hash_str));
        assert_eq!(get(&f, "user_phone"), "05551234567");
        assert_eq!(get(&f, "merchant_ok_url"), "https://shop.example/ok");
    }

    #[tokio::test]
    async fn get_token_failure_and_input_validation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/odeme/api/get-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "failed", "reason": "Zorunlu alan degeri gecersiz: merchant_id"
            })))
            .mount(&server)
            .await;
        let gw = PaytrGateway::new(settings(&server.uri())).unwrap();
        let err = gw.create_checkout(&request()).await.unwrap_err();
        assert!(err.to_string().contains("merchant_id"));
        let mut bad = request();
        bad.reference = "INV-42".into();
        assert!(matches!(
            gw.create_checkout(&bad).await,
            Err(IntegrationError::InvalidInput(_))
        ));
        let mut no_ip = request();
        no_ip.customer.ip = None;
        assert!(gw.create_checkout(&no_ip).await.is_err());
        let mut jpy = request();
        jpy.amount = Money::new(dec!(100), Currency::new("JPY").unwrap());
        assert!(gw.create_checkout(&jpy).await.is_err());
    }

    fn notification(status: &str, total: &str, hash: &str) -> WebhookRequest {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("merchant_oid", "INV42A1")
            .append_pair("status", status)
            .append_pair("total_amount", total)
            .append_pair("hash", hash)
            .append_pair("payment_type", "card")
            .append_pair("currency", "TL")
            .append_pair("payment_amount", "3456")
            .append_pair(
                "failed_reason_code",
                if status == "failed" { "6" } else { "" },
            )
            .append_pair(
                "failed_reason_msg",
                if status == "failed" {
                    "Müşteri vazgeçti"
                } else {
                    ""
                },
            )
            .finish();
        WebhookRequest {
            body: body.into_bytes(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn notification_ok_reply_and_events() {
        let gw = PaytrGateway::new(settings("http://unused.invalid")).unwrap();
        let hash = notification_hash("KEYKEYKEY", "SALTSALT", "INV42A1", "success", "3600");
        let out = gw
            .parse_webhook(&notification("success", "3600", &hash))
            .await
            .unwrap();
        assert_eq!(out.reply, WebhookReply::Text("OK".into()));
        let ev = &out.events[0];
        assert_eq!(ev.kind, PaymentEventKind::Paid);
        // 36.00 collected for a 34.56 order: the 1.44 installment interest is
        // informational and never part of the paid amount (no client credit).
        assert_eq!(ev.amount, Money::new(dec!(34.56), Currency::TRY));
        assert_eq!(crate::event_installment_interest(ev), Some(dec!(1.44)));
        assert_eq!(ev.raw[crate::RAW_COLLECTED_TOTAL], "36");
        assert_eq!(ev.gateway_txn_id, "INV42A1");
        assert_eq!(ev.event_id.as_deref(), Some("paytr:INV42A1:success"));
        assert_eq!(ev.raw["payment_amount"], "3456");

        // Without interest the order amount is the collected amount.
        let hash = notification_hash("KEYKEYKEY", "SALTSALT", "INV42A1", "success", "3456");
        let out = gw
            .parse_webhook(&notification("success", "3456", &hash))
            .await
            .unwrap();
        assert_eq!(out.events[0].amount, Money::new(dec!(34.56), Currency::TRY));
        assert_eq!(crate::event_installment_interest(&out.events[0]), None);

        let hash = notification_hash("KEYKEYKEY", "SALTSALT", "INV42A1", "failed", "3456");
        let out = gw
            .parse_webhook(&notification("failed", "3456", &hash))
            .await
            .unwrap();
        assert_eq!(out.events[0].kind, PaymentEventKind::Failed);
        assert_eq!(
            out.events[0].message.as_deref(),
            Some("Müşteri vazgeçti (6)")
        );
        assert_eq!(out.reply, WebhookReply::Text("OK".into()));
    }

    #[tokio::test]
    async fn notification_rejects_bad_hash_and_tampering() {
        let gw = PaytrGateway::new(settings("http://unused.invalid")).unwrap();
        let hash = notification_hash("KEYKEYKEY", "SALTSALT", "INV42A1", "success", "3456");
        // Amount raised after signing.
        assert!(matches!(
            gw.parse_webhook(&notification("success", "999999", &hash))
                .await,
            Err(IntegrationError::Auth(_))
        ));
        // Status flipped after signing.
        let failed = notification_hash("KEYKEYKEY", "SALTSALT", "INV42A1", "failed", "3456");
        assert!(
            gw.parse_webhook(&notification("success", "3456", &failed))
                .await
                .is_err()
        );
        // Wrong key.
        let other = notification_hash("OTHER", "SALTSALT", "INV42A1", "success", "3456");
        assert!(
            gw.parse_webhook(&notification("success", "3456", &other))
                .await
                .is_err()
        );
        // Missing hash.
        let missing = WebhookRequest {
            body: b"merchant_oid=INV42A1&status=success&total_amount=3456".to_vec(),
            ..Default::default()
        };
        assert!(matches!(
            gw.parse_webhook(&missing).await,
            Err(IntegrationError::Auth(_))
        ));
    }

    #[tokio::test]
    async fn refund_request_token_and_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/odeme/iade"))
            .and(wiremock::matchers::body_string_contains("return_amount=11.97"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "is_test": 1, "merchant_oid": "INV42A1", "return_amount": "11.97"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/odeme/iade"))
            .and(wiremock::matchers::body_string_contains("return_amount=999.00"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "error", "err_no": "006", "err_msg": "Toplam iade tutarı odeme tutarindan fazla olamaz"
            })))
            .mount(&server)
            .await;
        let gw = PaytrGateway::new(settings(&server.uri())).unwrap();
        let mut req = RefundRequest {
            gateway_txn_id: "INV42A1".into(),
            amount: Money::new(dec!(11.97), Currency::TRY),
            reason: None,
            reference: "RF-1".into(),
            ip: None,
        };
        let ok = gw.refund(&req).await.unwrap();
        assert_eq!(ok.status, RefundStatus::Succeeded);
        let f = form(&server.received_requests().await.unwrap()[0].body);
        assert_eq!(
            get(&f, "paytr_token"),
            paytr_hash("KEYKEYKEY", "123456INV42A111.97SALTSALT")
        );
        assert_eq!(get(&f, "reference_no"), "RF1");
        req.amount = Money::new(dec!(999), Currency::TRY);
        let err = gw.refund(&req).await.unwrap_err();
        assert!(matches!(err, IntegrationError::Remote { code: Some(ref c), .. } if c == "006"));
    }

    #[test]
    fn settings_from_values() {
        let s = PaytrSettings::from_values(
            &json!({"merchant_id": "123456", "mode": "live", "max_installment": "6", "no_installment": "on"}),
            &json!({"merchant_key": "k", "merchant_salt": "s"}),
        )
        .unwrap();
        assert!(!s.test_mode);
        assert!(s.no_installment);
        assert_eq!(s.max_installment, 6);
        assert!(
            PaytrSettings::from_values(
                &json!({"merchant_id": "123456", "max_installment": 1}),
                &json!({"merchant_key": "k", "merchant_salt": "s"})
            )
            .is_err()
        );
    }
}
