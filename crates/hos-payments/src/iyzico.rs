//! iyzico Checkout Form (hosted page / embeddable form).
//!
//! * Auth: `Authorization: IYZWSv2 base64("apiKey:…&randomKey:…&signature:…")`
//!   with `signature = hex(HMAC-SHA256(randomKey + uriPath + body, secretKey))`
//!   (docs.iyzico.com › Authentication › HMACSHA256 Auth) and `x-iyzi-rnd`.
//! * Checkout: `POST /payment/iyzipos/checkoutform/initialize/auth/ecom`; the
//!   customer pays on `paymentPageUrl` (or the embedded `checkoutFormContent`).
//! * Result: iyzico POSTs the browser to `callbackUrl` with a `token`. The token
//!   proves nothing by itself, so it is always re-checked with
//!   `POST /payment/iyzipos/checkoutform/auth/ecom/detail`; the response
//!   `signature` (HMAC-SHA256 over `:`-joined fields, trailing zeros trimmed) is
//!   verified for successful payments. The browser is then redirected (303) to
//!   the invoice via URLs signed into the callback URL.
//! * Merchant notifications (optional, configured in the iyzico panel) are
//!   verified with `X-IYZ-SIGNATURE-V3` and re-checked the same way.
//! * Refunds use `POST /v2/payment/refund` (amount-based, by `paymentId`); the
//!   v1 `/payment/refund` needs a per-basket-line `paymentTransactionId`.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hos_core::crypto;
use hos_core::domain::{IntegrationCategory, Locale};
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{
    CheckoutRequest, CheckoutSession, GatewayCapabilities, PaymentEvent, PaymentEventKind,
    PaymentGateway, RefundRequest, RefundResult, RefundStatus, WebhookOutcome, WebhookReply,
    WebhookRequest,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, SelectOption, kinds,
};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use serde_json::{Map, Value, json};

use crate::common::{
    self, DEFAULT_TIMEOUT, Mode, Response, ReturnSigner, auth, ensure_positive, itemizable,
    json_decimal, json_id, json_money, split_name, strip_keys, trim_zeros, truncate,
};

/// Sandbox API base URL.
pub const SANDBOX_BASE: &str = "https://sandbox-api.iyzipay.com";
/// Live API base URL.
pub const LIVE_BASE: &str = "https://api.iyzipay.com";
/// Checkout Form currencies (CF initialize `currency` enum).
pub const CURRENCIES: [&str; 6] = ["TRY", "USD", "EUR", "GBP", "NOK", "CHF"];
/// Commonly used placeholder when the buyer has no Turkish national id.
pub const DEFAULT_IDENTITY_NUMBER: &str = "11111111111";

const INIT_PATH: &str = "/payment/iyzipos/checkoutform/initialize/auth/ecom";
const DETAIL_PATH: &str = "/payment/iyzipos/checkoutform/auth/ecom/detail";
const REFUND_PATH: &str = "/v2/payment/refund";
const BIN_CHECK_PATH: &str = "/payment/bin/check";
const SIGNATURE_HEADER: &str = "x-iyz-signature-v3";
/// Card details iyzico returns that must not be stored with the event.
const CARD_FIELDS: [&str; 4] = ["binNumber", "lastFourDigits", "cardToken", "cardUserKey"];

/// `hex(HMAC-SHA256(randomKey + uriPath + body, secretKey))` (IYZWSv2).
pub fn signature(secret_key: &str, random_key: &str, uri_path: &str, body: &str) -> String {
    let payload = format!("{random_key}{uri_path}{body}");
    crypto::hmac_sha256_hex(secret_key.as_bytes(), payload.as_bytes())
}

/// The `Authorization` header value: `IYZWSv2 base64(apiKey:…&randomKey:…&signature:…)`.
pub fn authorization(api_key: &str, random_key: &str, signature_hex: &str) -> String {
    let plain = format!("apiKey:{api_key}&randomKey:{random_key}&signature:{signature_hex}");
    format!("IYZWSv2 {}", STANDARD.encode(plain))
}

/// Response signature: `hex(HMAC-SHA256(parts.join(":"), secretKey))`.
pub fn response_signature(secret_key: &str, parts: &[&str]) -> String {
    crypto::hmac_sha256_hex(secret_key.as_bytes(), parts.join(":").as_bytes())
}

/// `X-IYZ-SIGNATURE-V3` of a merchant notification:
/// HPP (Checkout Form) format `secretKey + iyziEventType + iyziPaymentId + token +
/// paymentConversationId + status`, direct format without `token`; HMAC-SHA256
/// keyed with the secret key, hex.
pub fn webhook_signature(
    secret_key: &str,
    event_type: &str,
    payment_id: &str,
    token: Option<&str>,
    conversation_id: &str,
    status: &str,
) -> String {
    let message = format!(
        "{secret_key}{event_type}{payment_id}{}{conversation_id}{status}",
        token.unwrap_or_default()
    );
    crypto::hmac_sha256_hex(secret_key.as_bytes(), message.as_bytes())
}

fn random_key() -> String {
    let n = u64::from_le_bytes(crypto::random_bytes::<8>()) % 1_000_000_000;
    format!("{}{n:09}", Utc::now().timestamp_millis())
}

/// How the payment page is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    /// Redirect to iyzico's hosted `paymentPageUrl`.
    Redirect,
    /// Embed `checkoutFormContent` in the invoice page.
    Embed,
}

/// iyzico settings.
#[derive(Debug, Clone)]
pub struct IyzicoSettings {
    /// API key.
    pub api_key: SecretString,
    /// Secret key.
    pub secret_key: SecretString,
    /// API base URL ([`SANDBOX_BASE`] / [`LIVE_BASE`]).
    pub base_url: String,
    /// Redirect or embedded form.
    pub display: Display,
    /// `basketItems[].category1` when an item has no category.
    pub category: String,
    /// `buyer.identityNumber` when the customer has none.
    pub fallback_identity_number: String,
    /// `enabledInstallments` (empty = iyzico default).
    pub enabled_installments: Vec<u32>,
    /// HTTP timeout.
    pub timeout: Duration,
}

impl IyzicoSettings {
    /// Settings with defaults (sandbox).
    pub fn new(api_key: SecretString, secret_key: SecretString) -> Self {
        Self {
            api_key,
            secret_key,
            base_url: SANDBOX_BASE.to_owned(),
            display: Display::Redirect,
            category: "Hosting".to_owned(),
            fallback_identity_number: DEFAULT_IDENTITY_NUMBER.to_owned(),
            enabled_installments: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Reads settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let mut settings = Self::new(s.secret("api_key")?, s.secret("secret_key")?);
        settings.base_url = match Mode::from_config(&c)? {
            Mode::Test => SANDBOX_BASE,
            Mode::Live => LIVE_BASE,
        }
        .to_owned();
        settings.display = match c.string_or("display", "redirect")?.as_str() {
            "redirect" => Display::Redirect,
            "embed" => Display::Embed,
            _ => {
                return Err(IntegrationError::Config(
                    "config.display must be `redirect` or `embed`".into(),
                ));
            }
        };
        settings.category = c.string_or("category", "Hosting")?;
        settings.fallback_identity_number =
            c.string_or("fallback_identity_number", DEFAULT_IDENTITY_NUMBER)?;
        if let Some(list) = c.opt_str("enabled_installments")? {
            settings.enabled_installments = list
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| {
                    p.parse::<u32>()
                        .ok()
                        .filter(|n| (1..=12).contains(n))
                        .ok_or_else(|| {
                            IntegrationError::Config(
                                "config.enabled_installments must be numbers 1–12".into(),
                            )
                        })
                })
                .collect::<Result<_, _>>()?;
        }
        Ok(settings)
    }
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::IYZICO, "iyzico", IntegrationCategory::Gateway)
        .description(
            "iyzico Checkout Form (hosted or embedded; card data never touches HostingOS). \
             Currencies: TRY, USD, EUR, GBP, NOK, CHF. Requires the customer's phone and billing \
             address. Optional: set Merchant Notifications to /webhooks/gateway/{integration_id} \
             in the iyzico panel (X-IYZ-SIGNATURE-V3).",
        )
        .docs_url("https://docs.iyzico.com/en/payment-methods/checkoutform")
        .config_field(common::mode_field())
        .config_field(
            FieldSpec::select(
                "display",
                "Payment page",
                vec![
                    SelectOption::new("redirect", "Redirect to iyzico"),
                    SelectOption::new("embed", "Embedded form"),
                ],
            )
            .default_value("redirect"),
        )
        .config_field(FieldSpec::text("category", "Basket item category").default_value("Hosting"))
        .config_field(
            FieldSpec::text("fallback_identity_number", "Fallback national id (TCKN)")
                .default_value(DEFAULT_IDENTITY_NUMBER)
                .help("Sent as buyer.identityNumber when the customer has none on file."),
        )
        .config_field(
            FieldSpec::text("enabled_installments", "Installments offered")
                .placeholder("1,2,3,6,9,12")
                .help("Empty = iyzico default."),
        )
        .secret_field(FieldSpec::password("api_key", "API key").required())
        .secret_field(FieldSpec::password("secret_key", "Secret key").required())
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

/// The iyzico gateway.
#[derive(Debug)]
pub struct IyzicoGateway {
    settings: IyzicoSettings,
    http: reqwest::Client,
    returns: ReturnSigner,
}

fn locale_code(locale: Option<Locale>) -> &'static str {
    match locale {
        Some(Locale::En) => "en",
        _ => "tr",
    }
}

fn sig_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn sig_price(value: &Value) -> String {
    json_decimal(value).map(trim_zeros).unwrap_or_default()
}

fn sanitized(detail: &Value) -> Value {
    strip_keys(detail.clone(), &CARD_FIELDS)
}

fn iyzico_error(response: &Response, json: &Value) -> IntegrationError {
    let message = json["errorMessage"]
        .as_str()
        .unwrap_or("iyzico request failed")
        .to_owned();
    let code = json_id(&json["errorCode"]);
    match (response.status, code.as_deref()) {
        (401 | 403, _) | (_, Some("1000" | "1001")) => IntegrationError::Auth(message),
        (429, _) => IntegrationError::RateLimited {
            retry_after: response.retry_after,
        },
        (s, _) if s >= 500 => IntegrationError::Remote {
            status: Some(s),
            code,
            message,
        },
        // Business / validation errors: not retryable.
        _ => IntegrationError::Remote {
            status: None,
            code,
            message,
        },
    }
}

impl IyzicoGateway {
    /// Creates the gateway.
    pub fn new(settings: IyzicoSettings) -> Result<Self, IntegrationError> {
        let http = common::http_client(settings.timeout)?;
        let returns = ReturnSigner::new(settings.secret_key.expose_str().as_bytes(), "iyzico");
        Ok(Self {
            settings,
            http,
            returns,
        })
    }

    async fn post_raw(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<(Response, Value), IntegrationError> {
        let body = serde_json::to_string(body)
            .map_err(|e| IntegrationError::Other(format!("JSON encoding failed: {e}")))?;
        let url = format!("{}{path}", self.settings.base_url.trim_end_matches('/'));
        let uri_path = url::Url::parse(&url)
            .map(|u| u.path().to_owned())
            .unwrap_or_else(|_| path.to_owned());
        let random_key = random_key();
        let sig = signature(
            self.settings.secret_key.expose_str(),
            &random_key,
            &uri_path,
            &body,
        );
        let rb = self
            .http
            .post(url)
            .header(
                http::header::AUTHORIZATION,
                authorization(self.settings.api_key.expose_str(), &random_key, &sig),
            )
            .header("x-iyzi-rnd", &random_key)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "application/json")
            .body(body);
        let response = common::send(rb).await?;
        let json = response.json()?;
        Ok((response, json))
    }

    async fn post_ok(&self, path: &str, body: &Value) -> Result<Value, IntegrationError> {
        let (response, json) = self.post_raw(path, body).await?;
        if response.is_success() && json["status"].as_str() == Some("success") {
            Ok(json)
        } else {
            Err(iyzico_error(&response, &json))
        }
    }

    /// Verifies a response `signature` when present.
    fn check_signature(&self, json: &Value, parts: &[String]) -> Result<(), IntegrationError> {
        let Some(sig) = json["signature"].as_str() else {
            return Ok(());
        };
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let expected = response_signature(self.settings.secret_key.expose_str(), &parts);
        if crypto::constant_time_eq(
            expected.as_bytes(),
            sig.trim().to_ascii_lowercase().as_bytes(),
        ) {
            Ok(())
        } else {
            Err(auth("iyzico response signature mismatch"))
        }
    }

    /// CF retrieve. Unknown / foreign tokens are an authentication failure.
    async fn retrieve(
        &self,
        token: &str,
        conversation_id: Option<&str>,
    ) -> Result<Value, IntegrationError> {
        let mut body = json!({"locale": "tr", "token": token});
        if let Some(conv) = conversation_id {
            body["conversationId"] = Value::String(conv.to_owned());
        }
        let (response, json) = self.post_raw(DETAIL_PATH, &body).await?;
        if json["paymentStatus"].is_string()
            || (response.is_success() && json["status"].as_str() == Some("success"))
        {
            return Ok(json);
        }
        Err(match iyzico_error(&response, &json) {
            IntegrationError::Remote {
                status: None,
                message,
                ..
            } => auth(format!("iyzico could not verify the token: {message}")),
            other => other,
        })
    }

    fn event_from_detail(&self, detail: &Value) -> Result<Option<PaymentEvent>, IntegrationError> {
        let status = detail["paymentStatus"].as_str().unwrap_or_default();
        let (kind, message) = match status {
            "SUCCESS" => match detail["fraudStatus"].as_i64() {
                Some(-1) => (
                    PaymentEventKind::Failed,
                    Some("rejected by iyzico fraud check".to_owned()),
                ),
                Some(0) => (
                    PaymentEventKind::Pending,
                    Some("in iyzico fraud review".to_owned()),
                ),
                _ => (PaymentEventKind::Paid, None),
            },
            "FAILURE" => (
                PaymentEventKind::Failed,
                Some(
                    detail["errorMessage"]
                        .as_str()
                        .unwrap_or("payment failed")
                        .to_owned(),
                ),
            ),
            "INIT_BANK_TRANSFER" | "INIT_CREDIT" | "PENDING_CREDIT" => {
                (PaymentEventKind::Pending, Some(status.to_owned()))
            }
            _ => return Ok(None),
        };
        if status == "SUCCESS" {
            self.check_signature(
                detail,
                &[
                    sig_text(&detail["paymentStatus"]),
                    sig_text(&detail["paymentId"]),
                    sig_text(&detail["currency"]),
                    sig_text(&detail["basketId"]),
                    sig_text(&detail["conversationId"]),
                    sig_price(&detail["paidPrice"]),
                    sig_price(&detail["price"]),
                    sig_text(&detail["token"]),
                ],
            )?;
        }
        let currency = detail["currency"]
            .as_str()
            .and_then(|c| Currency::new(c).ok());
        // `paidPrice` is what the customer paid, including installment
        // interest; `price` is the basket (invoice) amount. Only the basket
        // amount pays the invoice — the interest is recorded in `raw` and never
        // becomes client credit.
        let mut raw = sanitized(detail);
        let amount = currency.and_then(|c| {
            let price = json_money(&detail["price"], c);
            match json_money(&detail["paidPrice"], c) {
                Some(paid) => Some(crate::split_installment_interest(&mut raw, price, paid)),
                None => price,
            }
        });
        let amount = match (amount, kind) {
            (Some(a), _) => a,
            (None, PaymentEventKind::Failed) => Money::zero(currency.unwrap_or(Currency::TRY)),
            (None, _) => {
                return Err(IntegrationError::Transport(
                    "iyzico payment detail has no amount/currency".into(),
                ));
            }
        };
        let fee = [
            &detail["iyziCommissionRateAmount"],
            &detail["iyziCommissionFee"],
        ]
        .into_iter()
        .filter_map(json_decimal)
        .reduce(|a, b| a + b)
        .map(|total| Money::new(total, amount.currency));
        let txn = json_id(&detail["paymentId"])
            .or_else(|| json_id(&detail["token"]))
            .unwrap_or_default();
        Ok(Some(PaymentEvent {
            kind,
            event_id: Some(format!("iyzico:{txn}:{kind}")),
            gateway_txn_id: txn,
            invoice_ref: json_id(&detail["basketId"])
                .or_else(|| json_id(&detail["conversationId"])),
            amount,
            fee,
            occurred_at: None,
            message,
            payment_method_ref: None,
            gateway_refund_id: None,
            raw,
        }))
    }

    async fn handle_notification(
        &self,
        request: &WebhookRequest,
        header: &str,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let note: Value = serde_json::from_slice(&request.body)
            .map_err(|_| auth("iyzico notification is not JSON"))?;
        let text = |key: &str| json_id(&note[key]);
        let token = text("token");
        let payment_id = if token.is_some() {
            text("iyziPaymentId")
        } else {
            text("paymentId").or_else(|| text("iyziPaymentId"))
        };
        let (Some(event_type), Some(payment_id), Some(conversation), Some(status)) = (
            text("iyziEventType"),
            payment_id,
            text("paymentConversationId"),
            text("status"),
        ) else {
            return Err(auth("iyzico notification is missing signed fields"));
        };
        let expected = webhook_signature(
            self.settings.secret_key.expose_str(),
            &event_type,
            &payment_id,
            token.as_deref(),
            &conversation,
            &status,
        );
        if !crypto::constant_time_eq(
            expected.as_bytes(),
            header.trim().to_ascii_lowercase().as_bytes(),
        ) {
            return Err(auth("iyzico notification signature mismatch"));
        }
        let relevant = matches!(
            status.as_str(),
            "SUCCESS" | "FAILURE" | "INIT_BANK_TRANSFER" | "INIT_CREDIT" | "PENDING_CREDIT"
        );
        // Direct-API notifications (no token) do not belong to Checkout Form payments.
        let (Some(token), true) = (token, relevant) else {
            return Ok(WebhookOutcome::default());
        };
        // The notification carries no amount: the detail query is authoritative.
        let detail = self.retrieve(&token, Some(&conversation)).await?;
        let mut event = self.event_from_detail(&detail)?;
        if let Some(ev) = event.as_mut() {
            ev.occurred_at = note["iyziEventTime"]
                .as_i64()
                .and_then(DateTime::from_timestamp_millis);
            ev.raw = json!({"notification": note, "detail": sanitized(&detail)});
        }
        Ok(WebhookOutcome {
            events: event.into_iter().collect(),
            reply: WebhookReply::Empty,
        })
    }

    async fn handle_callback(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let pairs = request.query_pairs();
        let targets = self
            .returns
            .verify(&pairs)
            .ok_or_else(|| auth("iyzico callback without a valid HostingOS return signature"))?;
        let token = request
            .form_field("token")
            .or_else(|| {
                pairs
                    .iter()
                    .find(|(k, _)| k == "token")
                    .map(|(_, v)| v.clone())
            })
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| auth("iyzico callback without token"))?;
        let detail = self.retrieve(token.trim(), None).await?;
        let event = self.event_from_detail(&detail)?;
        let succeeded = event
            .as_ref()
            .is_some_and(|e| matches!(e.kind, PaymentEventKind::Paid | PaymentEventKind::Pending));
        Ok(WebhookOutcome {
            events: event.into_iter().collect(),
            reply: WebhookReply::Redirect(if succeeded { targets.ok } else { targets.fail }),
        })
    }

    fn buyer_and_addresses(
        &self,
        request: &CheckoutRequest,
    ) -> Result<(Value, Value), IntegrationError> {
        let customer = &request.customer;
        let email = customer.email.trim();
        if email.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "iyzico requires the customer email".into(),
            ));
        }
        let phone = customer
            .phone
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                IntegrationError::InvalidInput("iyzico requires the customer phone".into())
            })?;
        let address = customer.billing_address.as_ref().ok_or_else(|| {
            IntegrationError::InvalidInput("iyzico requires a billing address".into())
        })?;
        if address.line1.trim().is_empty()
            || address.city.trim().is_empty()
            || address.country_code.trim().is_empty()
        {
            return Err(IntegrationError::InvalidInput(
                "iyzico requires billing address line, city and country".into(),
            ));
        }
        let street = match address.line2.as_deref().map(str::trim) {
            Some(l2) if !l2.is_empty() => format!("{}, {l2}", address.line1.trim()),
            _ => address.line1.trim().to_owned(),
        };
        let (name, surname) = split_name(&customer.name);
        let identity = customer
            .identity_number
            .as_deref()
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .unwrap_or(&self.settings.fallback_identity_number);
        let mut buyer = json!({
            "id": customer.client_id.map(|c| c.to_string()).unwrap_or_else(|| request.reference.clone()),
            "name": name,
            "surname": surname,
            "identityNumber": identity,
            "email": email,
            "gsmNumber": phone,
            "registrationAddress": street,
            "city": address.city.trim(),
            "country": address.country_code.trim(),
        });
        if let Some(zip) = address
            .postal_code
            .as_deref()
            .filter(|z| !z.trim().is_empty())
        {
            buyer["zipCode"] = Value::String(zip.trim().to_owned());
        }
        if let Some(ip) = customer.ip {
            buyer["ip"] = Value::String(ip.to_string());
        }
        let mut addr = Map::new();
        addr.insert("address".into(), Value::String(street));
        addr.insert(
            "contactName".into(),
            Value::String(customer.name.trim().to_owned()),
        );
        addr.insert("city".into(), Value::String(address.city.trim().to_owned()));
        addr.insert(
            "country".into(),
            Value::String(address.country_code.trim().to_owned()),
        );
        if let Some(zip) = address
            .postal_code
            .as_deref()
            .filter(|z| !z.trim().is_empty())
        {
            addr.insert("zipCode".into(), Value::String(zip.trim().to_owned()));
        }
        Ok((buyer, Value::Object(addr)))
    }
}

fn ensure_currency(currency: Currency) -> Result<(), IntegrationError> {
    if CURRENCIES.contains(&currency.as_str()) {
        Ok(())
    } else {
        Err(IntegrationError::InvalidInput(format!(
            "iyzico does not support {currency}"
        )))
    }
}

#[async_trait]
impl PaymentGateway for IyzicoGateway {
    fn kind(&self) -> &'static str {
        kinds::IYZICO
    }

    fn capabilities(&self) -> GatewayCapabilities {
        capabilities()
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let body =
            json!({"locale": "tr", "conversationId": "hos-connection-test", "binNumber": "554960"});
        self.post_ok(BIN_CHECK_PATH, &body).await?;
        Ok(ConnectionInfo {
            server_version: None,
            details: Some(format!(
                "iyzico credentials accepted ({})",
                self.settings.base_url
            )),
        })
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        ensure_positive(&request.amount)?;
        ensure_currency(request.amount.currency)?;
        let (buyer, address) = self.buyer_and_addresses(request)?;
        let callback = self.returns.signed_url(
            &request.webhook_url,
            &request.return_url,
            &request.cancel_url,
        )?;
        let price = trim_zeros(request.amount.round().amount);
        let item_json = |id: &str, name: &str, category: &str, total: &Money| {
            json!({
                "id": truncate(id, 64),
                "name": truncate(if name.trim().is_empty() { id } else { name }, 128),
                "category1": category,
                "itemType": "VIRTUAL",
                "price": trim_zeros(total.round().amount),
            })
        };
        let basket: Vec<Value> = if itemizable(request) {
            request
                .items
                .iter()
                .map(|item| {
                    item_json(
                        &item.reference,
                        &item.name,
                        item.category.as_deref().unwrap_or(&self.settings.category),
                        &item.total,
                    )
                })
                .collect()
        } else {
            vec![item_json(
                &request.reference,
                &request.description,
                &self.settings.category,
                &request.amount,
            )]
        };
        let mut body = json!({
            "locale": locale_code(request.locale),
            "conversationId": request.reference,
            "price": price,
            "paidPrice": price,
            "currency": request.amount.currency.as_str(),
            "basketId": request.reference,
            "paymentGroup": "PRODUCT",
            "callbackUrl": callback,
            "buyer": buyer,
            "shippingAddress": address.clone(),
            "billingAddress": address,
            "basketItems": basket,
        });
        if !self.settings.enabled_installments.is_empty() {
            body["enabledInstallments"] = json!(self.settings.enabled_installments);
        }
        let response = self.post_ok(INIT_PATH, &body).await?;
        let token = response["token"]
            .as_str()
            .ok_or_else(|| IntegrationError::Transport("iyzico response has no token".into()))?
            .to_owned();
        self.check_signature(
            &response,
            &[sig_text(&response["conversationId"]), token.clone()],
        )?;
        match self.settings.display {
            Display::Redirect => {
                let url = response["paymentPageUrl"].as_str().ok_or_else(|| {
                    IntegrationError::Transport("iyzico response has no paymentPageUrl".into())
                })?;
                let expires_at = response["tokenExpireTime"]
                    .as_i64()
                    .map(|secs| Utc::now() + chrono::Duration::seconds(secs));
                Ok(CheckoutSession::Redirect {
                    url: url.to_owned(),
                    gateway_ref: Some(token),
                    expires_at,
                })
            }
            Display::Embed => {
                let content = response["checkoutFormContent"].as_str().ok_or_else(|| {
                    IntegrationError::Transport("iyzico response has no checkoutFormContent".into())
                })?;
                Ok(CheckoutSession::Embed {
                    html: format!(
                        "<div id=\"iyzipay-checkout-form\" class=\"responsive\"></div>{content}"
                    ),
                    gateway_ref: Some(token),
                })
            }
        }
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        match request.header(SIGNATURE_HEADER) {
            Some(sig) => self.handle_notification(request, sig).await,
            None => self.handle_callback(request).await,
        }
    }

    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        ensure_positive(&request.amount)?;
        ensure_currency(request.amount.currency)?;
        let price = trim_zeros(request.amount.round().amount);
        let mut body = json!({
            "locale": "tr",
            "conversationId": request.reference,
            "paymentId": request.gateway_txn_id.trim(),
            "price": price,
            "currency": request.amount.currency.as_str(),
        });
        if let Some(ip) = request.ip {
            body["ip"] = Value::String(ip.to_string());
        }
        let response = self.post_ok(REFUND_PATH, &body).await?;
        self.check_signature(
            &response,
            &[
                sig_text(&response["paymentId"]),
                sig_price(&response["price"]),
                sig_text(&response["currency"]),
                sig_text(&response["conversationId"]),
            ],
        )?;
        let amount =
            json_money(&response["price"], request.amount.currency).unwrap_or(request.amount);
        Ok(RefundResult {
            gateway_refund_id: json_id(&response["refundHostReference"])
                .or_else(|| json_id(&response["paymentId"])),
            status: RefundStatus::Succeeded,
            amount,
            message: None,
            raw: response,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::ids::InvoiceId;
    use hos_core::integrations::PostalAddress;
    use hos_core::integrations::gateway::{CheckoutItem, PaymentCustomer};
    use http::{HeaderMap, HeaderValue};
    use rust_decimal_macros::dec;
    use std::collections::BTreeMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const SECRET: &str = "sandbox-secret";
    const API_KEY: &str = "sandbox-apikey";

    fn gateway(server: &MockServer) -> IyzicoGateway {
        let mut s = IyzicoSettings::new(API_KEY.into(), SECRET.into());
        s.base_url = server.uri();
        IyzicoGateway::new(s).unwrap()
    }

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "INV42A1".into(),
            amount: Money::new(dec!(150.50), Currency::TRY),
            description: "Fatura #2026-0042".into(),
            customer: PaymentCustomer {
                name: "Ayşe Yılmaz".into(),
                email: "ayse@example.com".into(),
                phone: Some("+905551234567".into()),
                ip: Some("85.34.78.112".parse().unwrap()),
                billing_address: Some(PostalAddress {
                    line1: "Bora Sok. No:1".into(),
                    city: "İstanbul".into(),
                    postal_code: Some("34000".into()),
                    country_code: "TR".into(),
                    ..Default::default()
                }),
                identity_number: None,
                ..Default::default()
            },
            items: vec![
                CheckoutItem {
                    reference: "L1".into(),
                    name: "Hosting".into(),
                    category: None,
                    quantity: 1,
                    unit_price: Money::new(dec!(100), Currency::TRY),
                    total: Money::new(dec!(100), Currency::TRY),
                },
                CheckoutItem {
                    reference: "L2".into(),
                    name: "Domain".into(),
                    category: Some("Domain".into()),
                    quantity: 1,
                    unit_price: Money::new(dec!(50.50), Currency::TRY),
                    total: Money::new(dec!(50.50), Currency::TRY),
                },
            ],
            return_url: "https://shop.example/client/invoices/42?paid=1".into(),
            cancel_url: "https://shop.example/client/invoices/42".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: Some(Locale::Tr),
            save_payment_method: false,
            metadata: BTreeMap::new(),
        }
    }

    /// Recomputes the IYZWSv2 signature of a received request.
    fn assert_signed(req: &Request) {
        let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
        let decoded = String::from_utf8(
            STANDARD
                .decode(auth.strip_prefix("IYZWSv2 ").unwrap())
                .unwrap(),
        )
        .unwrap();
        let rnd = req.headers.get("x-iyzi-rnd").unwrap().to_str().unwrap();
        let body = std::str::from_utf8(&req.body).unwrap();
        let expected_sig = signature(SECRET, rnd, req.url.path(), body);
        assert_eq!(
            decoded,
            format!("apiKey:{API_KEY}&randomKey:{rnd}&signature:{expected_sig}")
        );
    }

    fn detail(status: &str, token: &str) -> Value {
        detail_paid(status, token, "150.50", "150.5")
    }

    /// Detail of a 150.5 basket of which the customer paid `paid` (`paid_sig`
    /// is its signature form without trailing zeros).
    fn detail_paid(status: &str, token: &str, paid: &str, paid_sig: &str) -> Value {
        let mut d = json!({
            "status": "success", "paymentStatus": status, "paymentId": "22416032", "fraudStatus": 1,
            "currency": "TRY", "basketId": "INV42A1", "conversationId": "INV42A1",
            "price": 150.5, "paidPrice": paid, "token": token,
            "iyziCommissionRateAmount": "4.21", "iyziCommissionFee": "0.25",
            "binNumber": "55496600", "lastFourDigits": "0006",
        });
        let sig = response_signature(
            SECRET,
            &[
                status, "22416032", "TRY", "INV42A1", "INV42A1", paid_sig, "150.5", token,
            ],
        );
        d["signature"] = Value::String(sig);
        d
    }

    #[tokio::test]
    async fn installment_interest_is_not_part_of_the_payment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(DETAIL_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(detail_paid("SUCCESS", "tok9", "162.54", "162.54")),
            )
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let outcome = gw
            .parse_webhook(&callback_request(&gw, Some("tok9")))
            .await
            .unwrap();
        let ev = &outcome.events[0];
        assert_eq!(ev.kind, PaymentEventKind::Paid);
        // The basket price pays the invoice; 12.04 interest is informational.
        assert_eq!(ev.amount, Money::new(dec!(150.5), Currency::TRY));
        assert_eq!(crate::event_installment_interest(ev), Some(dec!(12.04)));
        assert_eq!(ev.raw[crate::RAW_COLLECTED_TOTAL], "162.54");
    }

    #[test]
    fn documented_vectors() {
        // docs.iyzico.com › HMACSHA256 Auth (Postman sample).
        assert_eq!(
            authorization(
                "sandbox-3uHv0LccjcWDyFHTvJpiACKPcJwbczmZ",
                "1722246017090123456789",
                "91e491486d3aa951b4f387cc93d67fc754c4729af95344b694435f56447819e9"
            ),
            "IYZWSv2 YXBpS2V5OnNhbmRib3gtM3VIdjBMY2NqY1dEeUZIVHZKcGlBQ0tQY0p3YmN6bVomcmFuZG9tS2V5OjE3MjIyNDYwMTcwOTAxMjM0NTY3ODkmc2lnbmF0dXJlOjkxZTQ5MTQ4NmQzYWE5NTFiNGYzODdjYzkzZDY3ZmM3NTRjNDcyOWFmOTUzNDRiNjk0NDM1ZjU2NDQ3ODE5ZTk="
        );
        // docs.iyzico.com › Response Signature Validation (/payment/auth sample).
        assert_eq!(
            response_signature(
                "sandbox-qaIiLIxhjMgx3LSKIVvp6j17NunHOFtD",
                &[
                    "22416032",
                    "TRY",
                    "basketId",
                    "conversationId",
                    "10.5",
                    "10.5"
                ]
            ),
            "836c3a6c8db86c81043f2ca74edb13518b54a813f454f8dd762f0dd658610173"
        );
        assert_eq!(sig_price(&json!("10.50")), "10.5");
        assert_eq!(sig_price(&json!(50.0)), "50");
        assert_eq!(sig_price(&json!("10.51050")), "10.5105");
        let rk = random_key();
        assert!(rk.len() >= 22 && rk.bytes().all(|b| b.is_ascii_digit()));
    }

    #[tokio::test]
    async fn create_checkout_signs_request_and_verifies_response() {
        let server = MockServer::start().await;
        let token = "a5b67652-c24a-4347-b61e-6c957bf30f1b";
        Mock::given(method("POST"))
            .and(path(INIT_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "conversationId": "INV42A1", "token": token,
                "paymentPageUrl": "https://sandbox-cpp.iyzipay.com?token=x&lang=tr",
                "checkoutFormContent": "<script>iyzi</script>", "tokenExpireTime": 1800,
                "signature": response_signature(SECRET, &["INV42A1", token]),
            })))
            .expect(1)
            .mount(&server)
            .await;
        let session = gateway(&server).create_checkout(&request()).await.unwrap();
        assert!(matches!(
            &session,
            CheckoutSession::Redirect { url, gateway_ref: Some(t), expires_at: Some(_) }
                if url.starts_with("https://sandbox-cpp.iyzipay.com") && t == token
        ));
        let received = &server.received_requests().await.unwrap()[0];
        assert_signed(received);
        let body: Value = serde_json::from_slice(&received.body).unwrap();
        assert_eq!(body["price"], "150.5");
        assert_eq!(body["paidPrice"], "150.5");
        assert_eq!(body["currency"], "TRY");
        assert_eq!(body["basketId"], "INV42A1");
        assert_eq!(body["buyer"]["name"], "Ayşe");
        assert_eq!(body["buyer"]["surname"], "Yılmaz");
        assert_eq!(body["buyer"]["identityNumber"], DEFAULT_IDENTITY_NUMBER);
        assert_eq!(body["buyer"]["gsmNumber"], "+905551234567");
        assert_eq!(body["buyer"]["ip"], "85.34.78.112");
        assert_eq!(body["billingAddress"]["city"], "İstanbul");
        assert_eq!(body["basketItems"].as_array().unwrap().len(), 2);
        assert_eq!(body["basketItems"][1]["price"], "50.5");
        assert_eq!(body["basketItems"][1]["category1"], "Domain");
        assert_eq!(body["basketItems"][0]["category1"], "Hosting");
        let callback = body["callbackUrl"].as_str().unwrap();
        assert!(callback.starts_with("https://shop.example/webhooks/gateway/abc?hos_ok="));
        assert!(callback.contains("hos_sig="));
    }

    #[tokio::test]
    async fn create_checkout_rejects_forged_response_signature_and_bad_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(INIT_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "conversationId": "INV42A1", "token": "t",
                "paymentPageUrl": "https://x", "signature": "00".repeat(32),
            })))
            .mount(&server)
            .await;
        let gw = gateway(&server);
        assert!(matches!(
            gw.create_checkout(&request()).await,
            Err(IntegrationError::Auth(_))
        ));
        let mut no_phone = request();
        no_phone.customer.phone = None;
        assert!(matches!(
            gw.create_checkout(&no_phone).await,
            Err(IntegrationError::InvalidInput(_))
        ));
        let mut jpy = request();
        jpy.amount = Money::new(dec!(100), Currency::new("JPY").unwrap());
        assert!(gw.create_checkout(&jpy).await.is_err());
    }

    fn callback_request(gw: &IyzicoGateway, token: Option<&str>) -> WebhookRequest {
        let url = gw
            .returns
            .signed_url(
                "https://shop.example/webhooks/gateway/abc",
                "https://shop.example/ok",
                "https://shop.example/fail",
            )
            .unwrap();
        WebhookRequest {
            headers: HeaderMap::new(),
            body: token
                .map(|t| format!("token={t}"))
                .unwrap_or_default()
                .into_bytes(),
            query: url::Url::parse(&url).unwrap().query().map(str::to_owned),
            remote_ip: None,
        }
    }

    #[tokio::test]
    async fn callback_verifies_token_with_detail_and_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(DETAIL_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail("SUCCESS", "tok1")))
            .expect(1)
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let outcome = gw
            .parse_webhook(&callback_request(&gw, Some("tok1")))
            .await
            .unwrap();
        assert_eq!(
            outcome.reply,
            WebhookReply::Redirect("https://shop.example/ok".into())
        );
        let ev = &outcome.events[0];
        assert_eq!(ev.kind, PaymentEventKind::Paid);
        assert_eq!(ev.gateway_txn_id, "22416032");
        assert_eq!(ev.event_id.as_deref(), Some("iyzico:22416032:paid"));
        assert_eq!(ev.invoice_ref.as_deref(), Some("INV42A1"));
        assert_eq!(ev.amount, Money::new(dec!(150.50), Currency::TRY));
        assert_eq!(ev.fee, Some(Money::new(dec!(4.46), Currency::TRY)));
        assert!(ev.raw.get("binNumber").is_none());
        assert!(ev.raw.get("lastFourDigits").is_none());
        let sent: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["token"], "tok1");
        assert_signed(&server.received_requests().await.unwrap()[0]);
    }

    #[tokio::test]
    async fn callback_failures() {
        let server = MockServer::start().await;
        let mut tampered = detail("SUCCESS", "tok2");
        tampered["paidPrice"] = json!("1.00");
        Mock::given(method("POST"))
            .and(path(DETAIL_PATH))
            .and(wiremock::matchers::body_string_contains("tok2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tampered))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(DETAIL_PATH))
            .and(wiremock::matchers::body_string_contains("tok3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "failure", "errorCode": "5010", "errorMessage": "token bulunamadı"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(DETAIL_PATH))
            .and(wiremock::matchers::body_string_contains("tok4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "failure", "paymentStatus": "FAILURE", "token": "tok4",
                "basketId": "INV42A1", "currency": "TRY", "price": "150.5",
                "errorCode": "10051", "errorMessage": "Kart limiti yetersiz"
            })))
            .mount(&server)
            .await;
        let gw = gateway(&server);
        // Amount tampered in transit → response signature fails.
        assert!(matches!(
            gw.parse_webhook(&callback_request(&gw, Some("tok2"))).await,
            Err(IntegrationError::Auth(_))
        ));
        // Unknown token.
        assert!(matches!(
            gw.parse_webhook(&callback_request(&gw, Some("tok3"))).await,
            Err(IntegrationError::Auth(_))
        ));
        // Missing token / missing or forged return signature.
        assert!(matches!(
            gw.parse_webhook(&callback_request(&gw, None)).await,
            Err(IntegrationError::Auth(_))
        ));
        let mut forged = callback_request(&gw, Some("tok1"));
        forged.query = forged
            .query
            .map(|q| q.replace("shop.example%2Fok", "evil.example%2Fok"));
        assert!(matches!(
            gw.parse_webhook(&forged).await,
            Err(IntegrationError::Auth(_))
        ));
        // Declined payment → Failed event, back to the cancel URL.
        let out = gw
            .parse_webhook(&callback_request(&gw, Some("tok4")))
            .await
            .unwrap();
        assert_eq!(
            out.reply,
            WebhookReply::Redirect("https://shop.example/fail".into())
        );
        assert_eq!(out.events[0].kind, PaymentEventKind::Failed);
        assert_eq!(
            out.events[0].message.as_deref(),
            Some("Kart limiti yetersiz")
        );
        assert_eq!(out.events[0].amount, Money::new(dec!(150.5), Currency::TRY));
    }

    #[tokio::test]
    async fn merchant_notification_signature() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(DETAIL_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail("SUCCESS", "tok5")))
            .mount(&server)
            .await;
        let gw = gateway(&server);
        let body = json!({
            "paymentConversationId": "INV42A1", "merchantId": "123", "token": "tok5", "status": "SUCCESS",
            "iyziReferenceCode": "ref-1", "iyziEventType": "CHECKOUT_FORM_AUTH",
            "iyziEventTime": 1_700_000_000_000_i64, "iyziPaymentId": 22416032
        })
        .to_string();
        let sig = webhook_signature(
            SECRET,
            "CHECKOUT_FORM_AUTH",
            "22416032",
            Some("tok5"),
            "INV42A1",
            "SUCCESS",
        );
        let mut headers = HeaderMap::new();
        headers.insert("X-IYZ-SIGNATURE-V3", HeaderValue::from_str(&sig).unwrap());
        let req = WebhookRequest {
            headers: headers.clone(),
            body: body.clone().into_bytes(),
            ..Default::default()
        };
        let out = gw.parse_webhook(&req).await.unwrap();
        assert_eq!(out.reply, WebhookReply::Empty);
        assert_eq!(out.events[0].kind, PaymentEventKind::Paid);
        assert_eq!(
            out.events[0].event_id.as_deref(),
            Some("iyzico:22416032:paid")
        );
        assert_eq!(
            out.events[0].occurred_at.unwrap().timestamp(),
            1_700_000_000
        );

        // Tampered status with the original signature.
        let tampered = WebhookRequest {
            headers,
            body: body.replace("\"SUCCESS\"", "\"FAILURE\"").into_bytes(),
            ..Default::default()
        };
        assert!(matches!(
            gw.parse_webhook(&tampered).await,
            Err(IntegrationError::Auth(_))
        ));
        let mut bad = HeaderMap::new();
        bad.insert("X-IYZ-SIGNATURE-V3", HeaderValue::from_static("deadbeef"));
        let bad = WebhookRequest {
            headers: bad,
            body: body.into_bytes(),
            ..Default::default()
        };
        assert!(matches!(
            gw.parse_webhook(&bad).await,
            Err(IntegrationError::Auth(_))
        ));
    }

    #[tokio::test]
    async fn refund_v2_by_payment_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(REFUND_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "success", "paymentId": "22416032", "price": 50.5, "currency": "TRY",
                "conversationId": "RF1", "refundHostReference": "host-9",
                "signature": response_signature(SECRET, &["22416032", "50.5", "TRY", "RF1"]),
            })))
            .mount(&server)
            .await;
        let result = gateway(&server)
            .refund(&RefundRequest {
                gateway_txn_id: "22416032".into(),
                amount: Money::new(dec!(50.50), Currency::TRY),
                reason: None,
                reference: "RF1".into(),
                ip: Some("10.0.0.1".parse().unwrap()),
            })
            .await
            .unwrap();
        assert_eq!(result.status, RefundStatus::Succeeded);
        assert_eq!(result.gateway_refund_id.as_deref(), Some("host-9"));
        let received = &server.received_requests().await.unwrap()[0];
        assert_signed(received);
        let sent: Value = serde_json::from_slice(&received.body).unwrap();
        assert_eq!(sent["paymentId"], "22416032");
        assert_eq!(sent["price"], "50.5");
        assert_eq!(sent["ip"], "10.0.0.1");
    }

    #[tokio::test]
    async fn auth_errors_are_mapped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(BIN_CHECK_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "failure", "errorCode": "1001", "errorMessage": "api bilgileri bulunamadı"
            })))
            .mount(&server)
            .await;
        assert!(matches!(
            gateway(&server).test_connection().await,
            Err(IntegrationError::Auth(_))
        ));
    }

    #[test]
    fn settings_from_values() {
        let s = IyzicoSettings::from_values(
            &json!({"mode": "live", "display": "embed", "enabled_installments": "1, 3,6"}),
            &json!({"api_key": "k", "secret_key": "s"}),
        )
        .unwrap();
        assert_eq!(s.base_url, LIVE_BASE);
        assert_eq!(s.display, Display::Embed);
        assert_eq!(s.enabled_installments, vec![1, 3, 6]);
        assert!(
            IyzicoSettings::from_values(
                &json!({"enabled_installments": "1,24"}),
                &json!({"api_key": "k", "secret_key": "s"})
            )
            .is_err()
        );
    }
}
