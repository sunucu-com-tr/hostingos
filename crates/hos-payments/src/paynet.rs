//! Paynet — "Hazır Form" (Paynet.js hosted card form) + server-side charge.
//!
//! ## Sources (read 2026-11-18, <https://doc.paynet.com.tr>, GitBook `.md` pages)
//!
//! * Hazır Form — <https://doc.paynet.com.tr/oedeme-metotlari/hazir-form.md>
//! * Ödeme Onaylama `v1/transaction/charge` — <https://doc.paynet.com.tr/servisler/islem/odeme-onaylama.md>
//! * Ödeme Kontrol `v1/transaction/check` — <https://doc.paynet.com.tr/servisler/islem/odeme-kontrol.md>
//! * İade `v1/transaction/reversed_request` — <https://doc.paynet.com.tr/servisler/iptal-iade/iade.md>
//! * Authentication — <https://doc.paynet.com.tr/authentication.md>; environments —
//!   <https://doc.paynet.com.tr/baslarken-1.md>; return codes —
//!   <https://doc.paynet.com.tr/genel-bilgiler/hata-kodlari/doenues-kodlari.md>;
//!   installment codes — <https://doc.paynet.com.tr/genel-bilgiler/taksit-kodlari.md>
//!
//! **API version:** REST `v1`. Auth header `Authorization: Basic {secret_key}`
//! (the secret key verbatim, as documented — not base64 of `user:pass`).
//!
//! ## Flow and verification
//!
//! 1. Checkout is an [`CheckoutSession::Embed`]: `<form action=… method=POST>`
//!    wrapping Paynet's `<script class="paynet-button" src=…paynet.min.js
//!    data-key=… data-amount=…>`. The card is typed into Paynet's own form;
//!    Paynet.js then posts only `session_id` and `token_id` to the form
//!    action (our webhook URL).
//! 2. That browser post is **not trusted**. Paynet has no callback signature;
//!    the payment exists only after the merchant's server-side
//!    `POST v1/transaction/charge` (secret key) succeeds, and Paynet compares
//!    the charge's `amount`, `add_comission_amount`, `installments`,
//!    `no_instalment` and `ratio_code` with the `data-*` values the form was
//!    opened with. Because a customer could alter `data-amount` in their own
//!    browser, HostingOS signs the reference, amount and installment options
//!    into the form action (`hos_tag`, HMAC-SHA256 with a key derived from the
//!    secret key) and sends **those** values to `charge`; a tampered form makes
//!    the charge fail at Paynet. Only `is_succeed=true` from that API response
//!    (plus `agent_reference_no` = our reference) produces a `Paid` event,
//!    flagged [`RAW_EXACT_ATTEMPT`](crate::RAW_EXACT_ATTEMPT).
//! 3. A replayed post (`code 17`, used session) or a charge that timed out is
//!    resolved with `v1/transaction/check` by `reference_no`; the event id
//!    `paynet:{reference}:{status}` makes the second delivery a no-op.
//!
//! Amounts: `data-amount` / `charge.amount` / refund `amount` are minor units
//! ("10.00 TL" → `1000`); responses carry decimals. Paynet is TRY-only.
//! With `add_comission_amount=true` the customer also pays the commission:
//! the event amount stays the order amount (difference in
//! [`RAW_INSTALLMENT_INTEREST`](crate::RAW_INSTALLMENT_INTEREST)).
//!
//! ## Refunds
//!
//! `v1/transaction/reversed_request` (`xact_id`, `amount` in minor units,
//! `succeedUrl`) is a *request*: `code 0` = accepted → [`RefundStatus::Pending`].
//! Paynet later POSTs JSON `{xact_id, code, status, message}` to `succeedUrl`
//! (`code 0` = approved). That post is unsigned, so HostingOS signs the
//! original transaction, refund reference and amount into the URL and only
//! a URL with a valid tag can complete the refund.
//!
//! ## Hosts
//!
//! Test API `https://pts-api.paynet.com.tr`, script
//! `https://pts-pj.paynet.com.tr/public/js/paynet.min.js`; live
//! `https://api.paynet.com.tr`, `https://pj.paynet.com.tr/public/js/paynet.min.js`.
//! Paynet requires a fixed server IP registered with them.

use std::time::Duration;

use async_trait::async_trait;
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
use serde_json::{Value, json};

use crate::common::{
    self, Mode, ReturnSigner, auth, ensure_positive, html_escape, is_alphanumeric, json_decimal,
    json_id, pair, truncate,
};

/// Test API base.
pub const TEST_API: &str = "https://pts-api.paynet.com.tr";
/// Live API base.
pub const LIVE_API: &str = "https://api.paynet.com.tr";
/// Test Paynet.js.
pub const TEST_JS: &str = "https://pts-pj.paynet.com.tr/public/js/paynet.min.js";
/// Live Paynet.js.
pub const LIVE_JS: &str = "https://pj.paynet.com.tr/public/js/paynet.min.js";

const SIGNER_DOMAIN: &str = "paynet";
const Q_SOURCE: &str = "hos_src";
const SOURCE_PAY: &str = "pay";
const SOURCE_REFUND: &str = "refund";
const Q_REF: &str = "hos_ref";
const Q_AMOUNT: &str = "hos_amt";
const Q_OPTIONS: &str = "hos_opt";
const Q_TXN: &str = "hos_txn";
const Q_TAG: &str = "hos_tag";
/// Paynet return code: the Paynet.js session was already charged.
const CODE_USED_SESSION: i64 = 17;
/// Paynet return code: unauthorized.
const CODE_UNAUTHORIZED: i64 = 7;

/// Paynet settings.
#[derive(Debug, Clone)]
pub struct PaynetSettings {
    /// Publishable key (`data-key`, public).
    pub publishable_key: String,
    /// Secret key (`Authorization: Basic …`).
    pub secret_key: SecretString,
    /// Test or live.
    pub mode: Mode,
    /// Allowed installment codes (`data-installments`); empty = single payment.
    pub installments: Vec<u32>,
    /// `data-add_commission_amount`: the customer pays Paynet's commission.
    pub add_commission: bool,
    /// API base URL.
    pub api_base: String,
    /// Paynet.js URL.
    pub js_url: String,
    /// HTTP timeout.
    pub timeout: Duration,
}

impl PaynetSettings {
    /// Reads settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let publishable_key = c.str("publishable_key")?.trim().to_owned();
        if publishable_key.is_empty() || publishable_key.chars().any(char::is_whitespace) {
            return Err(IntegrationError::Config(
                "config.publishable_key is required".into(),
            ));
        }
        let secret_key = s.secret("secret_key")?;
        if secret_key.expose_str().trim().is_empty() {
            return Err(IntegrationError::Config(
                "secrets.secret_key is required".into(),
            ));
        }
        let mode = Mode::from_config(&c)?;
        let installments = parse_installments(&c.string_or("installments", "")?)?;
        let (api_base, js_url) = match mode {
            Mode::Test => (TEST_API, TEST_JS),
            Mode::Live => (LIVE_API, LIVE_JS),
        };
        Ok(Self {
            publishable_key,
            secret_key,
            mode,
            installments,
            add_commission: c.bool_or("add_commission", false)?,
            api_base: common::api_base_override(config).unwrap_or_else(|| api_base.to_owned()),
            js_url: js_url.to_owned(),
            timeout: common::DEFAULT_TIMEOUT,
        })
    }

    fn no_instalment(&self) -> bool {
        self.installments.is_empty()
    }

    /// `data-installments` / `charge.installments`.
    fn installments_csv(&self) -> String {
        self.installments
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn parse_installments(raw: &str) -> Result<Vec<u32>, IntegrationError> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let n: u32 = part
            .parse()
            .ok()
            .filter(|n| *n == 0 || (2..=12).contains(n))
            .ok_or_else(|| {
                IntegrationError::Config(
                    "config.installments must be a comma list of 0 and 2–12".into(),
                )
            })?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    if !out.is_empty() && !out.contains(&0) {
        out.insert(0, 0);
    }
    Ok(out)
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(kinds::PAYNET, "Paynet", IntegrationCategory::Gateway)
        .description(
            "Paynet hosted card form (Paynet.js); the card never reaches HostingOS and every \
             payment is confirmed server-side with the secret key. TRY only. Paynet requires \
             the server's fixed IP to be registered with them. No URL to set in the Paynet \
             panel: the form posts to /webhooks/gateway/{integration_id}.",
        )
        .docs_url("https://doc.paynet.com.tr/oedeme-metotlari/hazir-form")
        .config_field(common::mode_field().help("Test = pts-api.paynet.com.tr with test cards."))
        .config_field(
            FieldSpec::text("publishable_key", "Publishable key")
                .required()
                .help("Used as data-key in the payment page (public)."),
        )
        .config_field(
            FieldSpec::text("installments", "Installments")
                .placeholder("0,3,6,9")
                .help("Allowed installment codes; empty = single payment only."),
        )
        .config_field(
            FieldSpec::boolean("add_commission", "Customer pays the commission")
                .default_value(false)
                .help(
                    "Adds Paynet's commission to the card charge. The invoice is credited \
                     with the invoice amount only.",
                ),
        )
        .secret_field(
            FieldSpec::password("secret_key", "Secret key")
                .required()
                .help("sck_… key for the REST API."),
        )
}

/// Static capabilities.
pub fn capabilities() -> GatewayCapabilities {
    GatewayCapabilities {
        refunds: true,
        partial_refunds: true,
        stored_payment_methods: false,
        installments: true,
        offline: false,
        currencies: vec![Currency::TRY],
    }
}

/// The Paynet gateway.
#[derive(Debug)]
pub struct PaynetGateway {
    settings: PaynetSettings,
    signer: ReturnSigner,
    http: reqwest::Client,
}

/// Options signed into the form action: `{add_commission}:{no_instalment}:{installments}`.
fn options_string(add_commission: bool, no_instalment: bool, installments: &str) -> String {
    format!("{add_commission}:{no_instalment}:{installments}")
}

impl PaynetGateway {
    /// Creates the gateway.
    pub fn new(settings: PaynetSettings) -> Result<Self, IntegrationError> {
        if settings.secret_key.expose_str().is_empty() {
            return Err(IntegrationError::Config(
                "secrets.secret_key is required".into(),
            ));
        }
        let http = common::http_client(settings.timeout)?;
        let signer = ReturnSigner::new(settings.secret_key.expose_str().as_bytes(), SIGNER_DOMAIN);
        Ok(Self {
            settings,
            signer,
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.settings.api_base.trim_end_matches('/'))
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, IntegrationError> {
        let response = common::send(
            self.http
                .post(self.url(path))
                .header(
                    http::header::AUTHORIZATION,
                    format!("Basic {}", self.settings.secret_key.expose_str()),
                )
                .header(http::header::ACCEPT, "application/json")
                .json(body),
        )
        .await?;
        let json = response.json()?;
        if !response.is_success() {
            return Err(common::status_error(
                response.status,
                response.retry_after,
                format!("Paynet {path} failed"),
                json_id(&json["code"]),
            ));
        }
        if json["code"].as_i64() == Some(CODE_UNAUTHORIZED) {
            return Err(IntegrationError::Auth(
                "Paynet rejected the secret key".into(),
            ));
        }
        Ok(json)
    }

    /// The successful transaction with `reference_no`, if Paynet has one.
    async fn check_reference(&self, reference: &str) -> Result<Option<Value>, IntegrationError> {
        let json = self
            .post("/v1/transaction/check", &json!({"reference_no": reference}))
            .await?;
        Ok(json["Data"].as_array().and_then(|rows| {
            rows.iter()
                .find(|r| {
                    r["is_succeed"].as_bool() == Some(true)
                        && r["agent_reference"].as_str() == Some(reference)
                })
                .cloned()
        }))
    }

    /// Paid event from a charge / check row (amount decimals in TRY).
    fn paid_event(
        &self,
        reference: &str,
        order: Money,
        row: &Value,
        amount_key: &str,
    ) -> Result<PaymentEvent, IntegrationError> {
        let xact_id = row["xact_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| IntegrationError::Remote {
                status: None,
                code: None,
                message: "Paynet response has no xact_id".into(),
            })?
            .to_owned();
        let currency = row["currency"]
            .as_str()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map_or(Ok(Currency::TRY), Currency::new)
            .map_err(|_| IntegrationError::Remote {
                status: None,
                code: None,
                message: "Paynet response has an unknown currency".into(),
            })?;
        let collected = json_decimal(&row[amount_key])
            .map(|a| Money::new(a, currency))
            .ok_or_else(|| IntegrationError::Remote {
                status: None,
                code: None,
                message: "Paynet response has no amount".into(),
            })?;
        let mut raw = json!({
            "xact_id": xact_id,
            "id": row["id"],
            "xact_date": row["xact_date"],
            "is_succeed": row["is_succeed"],
            "code": row["code"],
            "message": row["message"],
            "amount": row[amount_key],
            "net_amount": row.get("net_amount").or(row.get("netAmount")),
            "comission": row["comission"],
            "currency": row["currency"],
            "instalment": row["instalment"],
            "bank_id": row["bank_id"],
            "authorization_code": row["authorization_code"],
            "reference_no": reference,
        });
        crate::tag_raw(&mut raw, crate::RAW_EXACT_ATTEMPT, Value::Bool(true));
        let amount = crate::split_installment_interest(&mut raw, Some(order), collected);
        let fee = json_decimal(&row["comission"])
            .filter(|f| *f > rust_decimal::Decimal::ZERO)
            .map(|f| Money::new(f, currency));
        Ok(PaymentEvent {
            kind: PaymentEventKind::Paid,
            event_id: Some(format!("paynet:{reference}:paid")),
            gateway_txn_id: xact_id,
            invoice_ref: Some(reference.to_owned()),
            amount,
            fee,
            occurred_at: None,
            message: None,
            payment_method_ref: None,
            gateway_refund_id: None,
            raw,
        })
    }

    fn failed_event(reference: &str, order: Money, message: String, raw: Value) -> PaymentEvent {
        PaymentEvent {
            kind: PaymentEventKind::Failed,
            event_id: Some(format!("paynet:{reference}:failed")),
            gateway_txn_id: reference.to_owned(),
            invoice_ref: Some(reference.to_owned()),
            amount: order,
            fee: None,
            occurred_at: None,
            message: Some(message),
            payment_method_ref: None,
            gateway_refund_id: None,
            raw,
        }
    }

    /// Browser post from Paynet.js: verify the signed action, then charge.
    async fn handle_payment(
        &self,
        request: &WebhookRequest,
        pairs: &[(String, String)],
    ) -> Result<WebhookOutcome, IntegrationError> {
        let (Some(reference), Some(units), Some(options), Some(tag)) = (
            pair(pairs, Q_REF),
            pair(pairs, Q_AMOUNT),
            pair(pairs, Q_OPTIONS),
            pair(pairs, Q_TAG),
        ) else {
            return Err(auth("Paynet post has no signed order"));
        };
        if !self
            .signer
            .verify_tag(SOURCE_PAY, &[reference, units, options], tag)
        {
            return Err(auth("Paynet post signature mismatch"));
        }
        let units: i64 = units
            .parse()
            .map_err(|_| auth("Paynet signed amount is invalid"))?;
        let order = Money::from_minor_units(units, Currency::TRY);
        let mut opts = options.splitn(3, ':');
        let add_commission = opts.next() == Some("true");
        let no_instalment = opts.next() == Some("true");
        let installments = opts.next().unwrap_or_default();
        let session_id = request.form_field("session_id").unwrap_or_default();
        let token_id = request.form_field("token_id").unwrap_or_default();
        let targets = self.signer.verify(pairs);
        if session_id.trim().is_empty() || token_id.trim().is_empty() {
            // Closed / abandoned form: nothing was charged.
            return Ok(WebhookOutcome {
                events: Vec::new(),
                reply: targets.map_or(WebhookReply::Empty, |t| WebhookReply::Redirect(t.fail)),
            });
        }
        let body = json!({
            "session_id": session_id.trim(),
            "token_id": token_id.trim(),
            "amount": units,
            "transaction_type": 1,
            "add_comission_amount": add_commission,
            "ratio_code": "",
            "installments": installments,
            "no_instalment": no_instalment,
            "tds_required": true,
            "reference_no": reference,
        });
        let event = match self.post("/v1/transaction/charge", &body).await {
            Ok(json) if json["is_succeed"].as_bool() == Some(true) => {
                if json["agent_reference_no"]
                    .as_str()
                    .is_some_and(|r| !r.is_empty() && r != reference)
                {
                    return Err(auth("Paynet charge is for another reference"));
                }
                self.paid_event(reference, order, &json, "amount")?
            }
            Ok(json) => {
                if json["code"].as_i64() == Some(CODE_USED_SESSION)
                    && let Some(row) = self.check_reference(reference).await?
                {
                    self.paid_event(reference, order, &row, "amount")?
                } else {
                    let message = ["bank_error_message", "paynet_error_message", "message"]
                        .iter()
                        .filter_map(|k| json[*k].as_str())
                        .map(str::trim)
                        .find(|m| !m.is_empty())
                        .unwrap_or("payment declined");
                    let raw = json!({
                        "code": json["code"],
                        "message": json["message"],
                        "paynet_error_id": json["paynet_error_id"],
                        "bank_error_id": json["bank_error_id"],
                        "reference_no": reference,
                    });
                    Self::failed_event(reference, order, truncate(message, 200), raw)
                }
            }
            // The charge may have gone through before the connection broke.
            Err(e @ (IntegrationError::Timeout | IntegrationError::Transport(_))) => {
                match self.check_reference(reference).await {
                    Ok(Some(row)) => self.paid_event(reference, order, &row, "amount")?,
                    _ => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        let paid = event.kind == PaymentEventKind::Paid;
        Ok(WebhookOutcome {
            events: vec![event],
            reply: match targets {
                Some(t) if paid => WebhookReply::Redirect(t.ok),
                Some(t) => WebhookReply::Redirect(t.fail),
                None => WebhookReply::Empty,
            },
        })
    }

    /// Refund approval post to a signed `succeedUrl`.
    fn handle_refund(
        &self,
        request: &WebhookRequest,
        pairs: &[(String, String)],
    ) -> Result<WebhookOutcome, IntegrationError> {
        let (Some(txn), Some(reference), Some(units), Some(tag)) = (
            pair(pairs, Q_TXN),
            pair(pairs, Q_REF),
            pair(pairs, Q_AMOUNT),
            pair(pairs, Q_TAG),
        ) else {
            return Err(auth("Paynet refund notification has no signed refund"));
        };
        if !self
            .signer
            .verify_tag(SOURCE_REFUND, &[txn, reference, units], tag)
        {
            return Err(auth("Paynet refund notification signature mismatch"));
        }
        let units: i64 = units
            .parse()
            .map_err(|_| auth("Paynet signed refund amount is invalid"))?;
        let body: Value = request.json()?;
        let code = json_id(&body["code"]).unwrap_or_default();
        if code != "0" {
            tracing::warn!(code = %code, "Paynet refund request was not approved");
            return Ok(WebhookOutcome::default());
        }
        let mut raw = json!({
            "xact_id": body["xact_id"],
            "code": body["code"],
            "status": body["status"],
            "message": body["message"],
            "original_xact_id": txn,
        });
        crate::tag_refund_reference(&mut raw, Some(reference));
        Ok(WebhookOutcome {
            events: vec![PaymentEvent {
                kind: PaymentEventKind::Refunded,
                event_id: Some(format!("paynet:refund:{reference}")),
                gateway_txn_id: txn.to_owned(),
                invoice_ref: None,
                amount: Money::from_minor_units(units, Currency::TRY),
                fee: None,
                occurred_at: None,
                message: body["message"].as_str().map(|m| truncate(m, 200)),
                payment_method_ref: None,
                gateway_refund_id: json_id(&body["xact_id"]),
                raw,
            }],
            reply: WebhookReply::Empty,
        })
    }

    async fn reversed_request(
        &self,
        request: &RefundRequest,
        succeed_url: Option<String>,
    ) -> Result<RefundResult, IntegrationError> {
        ensure_positive(&request.amount)?;
        if request.amount.currency != Currency::TRY {
            return Err(IntegrationError::InvalidInput(
                "Paynet refunds are TRY only".into(),
            ));
        }
        let units = request
            .amount
            .to_minor_units()
            .map_err(|_| IntegrationError::InvalidInput("amount is out of range".into()))?;
        let mut body = json!({
            "xact_id": request.gateway_txn_id.trim(),
            "amount": units.to_string(),
        });
        if let Some(url) = succeed_url {
            body["succeedUrl"] = Value::String(url);
        }
        let json = self.post("/v1/transaction/reversed_request", &body).await?;
        let code = json["code"].as_i64();
        let raw = json!({"code": json["code"], "message": json["message"], "object_name": json["object_name"]});
        Ok(if code == Some(0) {
            RefundResult {
                gateway_refund_id: None,
                status: RefundStatus::Pending,
                amount: request.amount,
                message: Some("Paynet accepted the refund request".into()),
                raw,
            }
        } else {
            RefundResult {
                gateway_refund_id: None,
                status: RefundStatus::Failed,
                amount: request.amount,
                message: Some(
                    json["message"]
                        .as_str()
                        .map_or_else(|| "Paynet rejected the refund".into(), |m| truncate(m, 200)),
                ),
                raw,
            }
        })
    }
}

#[async_trait]
impl PaymentGateway for PaynetGateway {
    fn kind(&self) -> &'static str {
        kinds::PAYNET
    }

    fn capabilities(&self) -> GatewayCapabilities {
        GatewayCapabilities {
            installments: !self.settings.no_instalment(),
            ..capabilities()
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        // An unknown reference answers "no data" when the key is valid.
        let json = self
            .post(
                "/v1/transaction/check",
                &json!({"reference_no": "hosconnectiontest"}),
            )
            .await?;
        Ok(ConnectionInfo {
            server_version: Some("Paynet API v1".into()),
            details: Some(format!(
                "{} ({})",
                json["message"].as_str().unwrap_or("authenticated"),
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
        if request.amount.currency != Currency::TRY {
            return Err(IntegrationError::InvalidInput(format!(
                "Paynet does not support {}",
                request.amount.currency
            )));
        }
        let reference = request.reference.as_str();
        if !is_alphanumeric(reference) || reference.len() > 64 {
            return Err(IntegrationError::InvalidInput(
                "Paynet reference must be alphanumeric, at most 64 characters".into(),
            ));
        }
        let units = request
            .amount
            .round()
            .to_minor_units()
            .map_err(|_| IntegrationError::InvalidInput("amount is out of range".into()))?
            .to_string();
        let installments = self.settings.installments_csv();
        let no_instalment = self.settings.no_instalment();
        let options = options_string(self.settings.add_commission, no_instalment, &installments);
        let tag = self.signer.tag(SOURCE_PAY, &[reference, &units, &options]);
        let back = self.signer.signed_url(
            &request.webhook_url,
            &request.return_url,
            &request.cancel_url,
        )?;
        let action = common::with_query(
            &back,
            &[
                (Q_SOURCE, SOURCE_PAY),
                (Q_REF, reference),
                (Q_AMOUNT, &units),
                (Q_OPTIONS, &options),
                (Q_TAG, &tag),
            ],
        )?;
        let label = match request.locale {
            Some(hos_core::domain::Locale::En) => "Pay",
            _ => "Ödeme Yap",
        };
        let mut attrs: Vec<(&str, String)> = vec![
            ("data-key", self.settings.publishable_key.clone()),
            ("data-amount", units.clone()),
            ("data-reference_no", reference.to_owned()),
            ("data-description", truncate(&request.description, 100)),
            ("data-button_label", label.to_owned()),
            (
                "data-add_commission_amount",
                self.settings.add_commission.to_string(),
            ),
            ("data-no_instalment", no_instalment.to_string()),
            ("data-tds_required", "true".to_owned()),
        ];
        if !installments.is_empty() {
            attrs.push(("data-installments", installments));
        }
        let attrs: String = attrs
            .iter()
            .map(|(k, v)| format!(" {k}=\"{}\"", html_escape(v)))
            .collect();
        let html = format!(
            "<form action=\"{}\" method=\"POST\"><script class=\"paynet-button\" \
             type=\"text/javascript\" src=\"{}\"{attrs}></script></form>",
            html_escape(&action),
            html_escape(&self.settings.js_url),
        );
        Ok(CheckoutSession::Embed {
            html,
            gateway_ref: Some(request.reference.clone()),
        })
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let pairs = request.query_pairs();
        match pair(&pairs, Q_SOURCE) {
            Some(SOURCE_PAY) => self.handle_payment(request, &pairs).await,
            Some(SOURCE_REFUND) => self.handle_refund(request, &pairs),
            _ => Err(auth("not a Paynet notification")),
        }
    }

    async fn refund(&self, request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        self.reversed_request(request, None).await
    }

    async fn refund_with_callback(
        &self,
        request: &RefundRequest,
        webhook_url: &str,
    ) -> Result<RefundResult, IntegrationError> {
        let units = request
            .amount
            .to_minor_units()
            .map_err(|_| IntegrationError::InvalidInput("amount is out of range".into()))?
            .to_string();
        let txn = request.gateway_txn_id.trim();
        let reference = request.reference.trim();
        let tag = self.signer.tag(SOURCE_REFUND, &[txn, reference, &units]);
        let url = common::with_query(
            webhook_url,
            &[
                (Q_SOURCE, SOURCE_REFUND),
                (Q_TXN, txn),
                (Q_REF, reference),
                (Q_AMOUNT, &units),
                (Q_TAG, &tag),
            ],
        )?;
        self.reversed_request(request, Some(url)).await
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
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "sck_test_SECRET";

    fn settings(base: &str) -> PaynetSettings {
        let mut s = PaynetSettings::from_values(
            &json!({"mode": "test", "publishable_key": "pbk_test_1"}),
            &json!({"secret_key": SECRET}),
        )
        .unwrap();
        s.api_base = base.to_owned();
        s
    }

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "HOS77PAY".into(),
            amount: Money::new(dec!(125.60), Currency::TRY),
            description: "Fatura <INV-7>".into(),
            customer: PaymentCustomer::default(),
            items: Vec::new(),
            return_url: "https://shop.example/client/invoices/7?payment=return".into(),
            cancel_url: "https://shop.example/client/invoices/7?payment=cancel".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: None,
            save_payment_method: false,
            metadata: BTreeMap::new(),
        }
    }

    /// The form action from the embed HTML (unescaped).
    fn action_of(html: &str) -> String {
        let start = html.find("action=\"").unwrap() + 8;
        let end = start + html[start..].find('"').unwrap();
        html[start..end].replace("&amp;", "&")
    }

    fn browser_post(action: &str, session: &str, token: &str) -> WebhookRequest {
        let url = url::Url::parse(action).unwrap();
        WebhookRequest {
            headers: HeaderMap::new(),
            body: format!("session_id={session}&token_id={token}").into_bytes(),
            query: url.query().map(str::to_owned),
            remote_ip: None,
        }
    }

    fn charge_ok(amount: f64) -> Value {
        json!({
            "id": 2526145, "xact_id": "xk_EAAAAFZ59jeB/zqMRhd0yygD", "transaction_type": 1,
            "is_succeed": true, "amount": amount, "net_amount": amount - 3.15, "comission": 3.15,
            "currency": "TRY", "instalment": 0, "card_no_masked": "435508******4358",
            "card_holder": "TEST", "card_hash": "pch_x", "agent_reference_no": "HOS77PAY",
            "code": 0, "message": "Başarılı İşlem"
        })
    }

    async fn checkout_action(gw: &PaynetGateway) -> String {
        let CheckoutSession::Embed { html, .. } = gw.create_checkout(&request()).await.unwrap()
        else {
            panic!("embed expected")
        };
        action_of(&html)
    }

    #[tokio::test]
    async fn embed_form_and_signed_action() {
        let gw = PaynetGateway::new(settings("https://unused.example")).unwrap();
        let CheckoutSession::Embed { html, gateway_ref } =
            gw.create_checkout(&request()).await.unwrap()
        else {
            panic!("embed expected")
        };
        assert_eq!(gateway_ref.as_deref(), Some("HOS77PAY"));
        assert!(html.contains(&format!("src=\"{TEST_JS}\"")));
        assert!(html.contains("class=\"paynet-button\""));
        assert!(html.contains("data-key=\"pbk_test_1\""));
        assert!(html.contains("data-amount=\"12560\""));
        assert!(html.contains("data-reference_no=\"HOS77PAY\""));
        assert!(html.contains("data-no_instalment=\"true\""));
        assert!(html.contains("data-add_commission_amount=\"false\""));
        assert!(html.contains("Fatura &lt;INV-7&gt;"));
        assert!(!html.contains(SECRET));
        let action = url::Url::parse(&action_of(&html)).unwrap();
        assert_eq!(action.path(), "/webhooks/gateway/abc");
        let pairs: Vec<(String, String)> = action.query_pairs().into_owned().collect();
        assert_eq!(pair(&pairs, Q_AMOUNT), Some("12560"));
        // Independent vector: HMAC-SHA256(key, message) with
        // key = HMAC-SHA256(secret, "hf-payments/return-url/v1/paynet") and the
        // length-prefixed tag message, computed with hos_core::crypto directly.
        let key =
            hos_core::crypto::hmac_sha256(SECRET.as_bytes(), b"hf-payments/return-url/v1/paynet");
        let message = "tag:3:pay|8:HOS77PAY|5:12560|11:false:true:";
        assert_eq!(
            pair(&pairs, Q_TAG).unwrap(),
            hos_core::crypto::hmac_sha256_hex(&key, message.as_bytes())
        );
    }

    #[tokio::test]
    async fn valid_post_is_charged_server_side() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/charge"))
            .and(header("authorization", format!("Basic {SECRET}").as_str()))
            .and(body_partial_json(json!({
                "session_id": "js_1", "token_id": "tok_1", "amount": 12560,
                "transaction_type": 1, "reference_no": "HOS77PAY", "tds_required": true,
                "no_instalment": true, "add_comission_amount": false
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(charge_ok(125.60)))
            .expect(1)
            .mount(&server)
            .await;
        let gw = PaynetGateway::new(settings(&server.uri())).unwrap();
        let action = checkout_action(&gw).await;
        let out = gw
            .parse_webhook(&browser_post(&action, "js_1", "tok_1"))
            .await
            .unwrap();
        let e = &out.events[0];
        assert_eq!(e.kind, PaymentEventKind::Paid);
        assert_eq!(e.event_id.as_deref(), Some("paynet:HOS77PAY:paid"));
        assert_eq!(e.gateway_txn_id, "xk_EAAAAFZ59jeB/zqMRhd0yygD");
        assert_eq!(e.amount, Money::new(dec!(125.60), Currency::TRY));
        assert_eq!(e.fee, Some(Money::new(dec!(3.15), Currency::TRY)));
        assert!(crate::event_requires_exact_attempt(e));
        let raw = e.raw.to_string();
        assert!(!raw.contains("435508") && !raw.contains("pch_x"));
        assert_eq!(
            out.reply,
            WebhookReply::Redirect("https://shop.example/client/invoices/7?payment=return".into())
        );
    }

    #[tokio::test]
    async fn commission_on_top_is_not_credited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/charge"))
            .and(body_partial_json(json!({"add_comission_amount": true, "installments": "0,3,6", "no_instalment": false})))
            .respond_with(ResponseTemplate::new(200).set_body_json(charge_ok(131.88)))
            .expect(1)
            .mount(&server)
            .await;
        let mut s = settings(&server.uri());
        s.add_commission = true;
        s.installments = parse_installments("3,6").unwrap();
        let gw = PaynetGateway::new(s).unwrap();
        let action = checkout_action(&gw).await;
        let out = gw
            .parse_webhook(&browser_post(&action, "js_1", "tok_1"))
            .await
            .unwrap();
        let e = &out.events[0];
        assert_eq!(e.amount, Money::new(dec!(125.60), Currency::TRY));
        assert_eq!(crate::event_installment_interest(e), Some(dec!(6.28)));
    }

    #[tokio::test]
    async fn tampered_action_never_reaches_paynet() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(charge_ok(1.0)))
            .expect(0)
            .mount(&server)
            .await;
        let gw = PaynetGateway::new(settings(&server.uri())).unwrap();
        let action = checkout_action(&gw).await;
        let cheap = action.replace("hos_amt=12560", "hos_amt=100");
        assert!(matches!(
            gw.parse_webhook(&browser_post(&cheap, "js_1", "tok_1"))
                .await,
            Err(IntegrationError::Auth(_))
        ));
        let other_ref = action.replace("hos_ref=HOS77PAY", "hos_ref=HOS99XYZ");
        assert!(
            gw.parse_webhook(&browser_post(&other_ref, "js_1", "tok_1"))
                .await
                .is_err()
        );
        // Integration B's signed form posted to integration A.
        let mut s = settings(&server.uri());
        s.secret_key = "sck_other".into();
        let other = PaynetGateway::new(s).unwrap();
        let foreign = checkout_action(&other).await;
        assert!(matches!(
            gw.parse_webhook(&browser_post(&foreign, "js_1", "tok_1"))
                .await,
            Err(IntegrationError::Auth(_))
        ));
        // No query at all.
        let bare = WebhookRequest {
            headers: HeaderMap::new(),
            body: b"session_id=a&token_id=b".to_vec(),
            query: None,
            remote_ip: None,
        };
        assert!(gw.parse_webhook(&bare).await.is_err());
    }

    #[tokio::test]
    async fn declined_and_replayed_posts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/charge"))
            .and(body_partial_json(json!({"session_id": "js_bad"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "is_succeed": false, "code": 1, "message": "unsuccessful",
                "bank_error_message": "Yetersiz bakiye", "card_no_masked": "435508******4358"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/charge"))
            .and(body_partial_json(json!({"session_id": "js_used"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "is_succeed": false, "code": 17, "message": "paynetjusedsession"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/check"))
            .and(body_partial_json(json!({"reference_no": "HOS77PAY"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Data": [{"xact_id": "xk_EAAAAFZ59jeB/zqMRhd0yygD", "is_succeed": true,
                          "agent_reference": "HOS77PAY", "amount": 125.6, "currency": "TRY",
                          "card_no": "435508******4358"}],
                "code": 0, "message": "Başarılı İşlem"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = PaynetGateway::new(settings(&server.uri())).unwrap();
        let action = checkout_action(&gw).await;
        let out = gw
            .parse_webhook(&browser_post(&action, "js_bad", "tok"))
            .await
            .unwrap();
        assert_eq!(out.events[0].kind, PaymentEventKind::Failed);
        assert_eq!(out.events[0].message.as_deref(), Some("Yetersiz bakiye"));
        assert!(!out.events[0].raw.to_string().contains("435508"));
        assert_eq!(
            out.reply,
            WebhookReply::Redirect("https://shop.example/client/invoices/7?payment=cancel".into())
        );
        // The same session posted again: resolved via check, same event id.
        let replay = gw
            .parse_webhook(&browser_post(&action, "js_used", "tok"))
            .await
            .unwrap();
        assert_eq!(replay.events[0].kind, PaymentEventKind::Paid);
        assert_eq!(
            replay.events[0].event_id.as_deref(),
            Some("paynet:HOS77PAY:paid")
        );
        // Abandoned form (no token): nothing charged, back to the invoice.
        let none = gw
            .parse_webhook(&browser_post(&action, "", ""))
            .await
            .unwrap();
        assert!(none.events.is_empty());
    }

    #[tokio::test]
    async fn refunds_pending_then_signed_completion() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/reversed_request"))
            .and(body_partial_json(json!({"xact_id": "xk_1", "amount": "5000"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object_name": "transaction_reversed_request", "message": "Başarılı İşlem", "code": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/reversed_request"))
            .and(body_partial_json(json!({"xact_id": "xk_2"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object_name": "transaction_reversed_request", "message": "unsuccessful", "code": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        let gw = PaynetGateway::new(settings(&server.uri())).unwrap();
        let req = RefundRequest {
            gateway_txn_id: "xk_1".into(),
            amount: Money::new(dec!(50), Currency::TRY),
            reason: None,
            reference: "0190aaaabbbbccccddddeeeeffff0000".into(),
            ip: None,
        };
        let result = gw
            .refund_with_callback(&req, "https://shop.example/webhooks/gateway/abc")
            .await
            .unwrap();
        assert_eq!(result.status, RefundStatus::Pending);
        let sent: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        let succeed = url::Url::parse(sent["succeedUrl"].as_str().unwrap()).unwrap();
        let notify = |body: Value, query: Option<&str>| WebhookRequest {
            headers: HeaderMap::new(),
            body: body.to_string().into_bytes(),
            query: query.map(str::to_owned),
            remote_ip: None,
        };
        let out = gw
            .parse_webhook(&notify(
                json!({"xact_id": "xk_r1", "code": "0", "status": "approved", "message": "ok"}),
                succeed.query(),
            ))
            .await
            .unwrap();
        let e = &out.events[0];
        assert_eq!(e.kind, PaymentEventKind::Refunded);
        assert_eq!(e.gateway_txn_id, "xk_1");
        assert_eq!(e.amount, Money::new(dec!(50), Currency::TRY));
        assert_eq!(e.gateway_refund_id.as_deref(), Some("xk_r1"));
        assert_eq!(
            crate::event_refund_reference(e),
            Some("0190aaaabbbbccccddddeeeeffff0000")
        );
        // Rejected approval: no event. Forged amount: refused.
        let out = gw
            .parse_webhook(&notify(
                json!({"xact_id": "xk_r1", "code": "1"}),
                succeed.query(),
            ))
            .await
            .unwrap();
        assert!(out.events.is_empty());
        let forged = succeed
            .query()
            .unwrap()
            .replace("hos_amt=5000", "hos_amt=9000");
        assert!(
            gw.parse_webhook(&notify(json!({"code": "0"}), Some(&forged)))
                .await
                .is_err()
        );
        // Refund failure.
        let failed = gw
            .refund(&RefundRequest {
                gateway_txn_id: "xk_2".into(),
                ..req.clone()
            })
            .await
            .unwrap();
        assert_eq!(failed.status, RefundStatus::Failed);
    }

    #[tokio::test]
    async fn connection_test_and_bad_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/check"))
            .and(header("authorization", format!("Basic {SECRET}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Data": [], "code": 12, "message": "nodata"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/transaction/check"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(json!({"code": 7, "message": "unauthorized"})),
            )
            .mount(&server)
            .await;
        let gw = PaynetGateway::new(settings(&server.uri())).unwrap();
        assert!(gw.test_connection().await.is_ok());
        let mut s = settings(&server.uri());
        s.secret_key = "sck_wrong".into();
        assert!(matches!(
            PaynetGateway::new(s).unwrap().test_connection().await,
            Err(IntegrationError::Auth(_))
        ));
    }

    #[test]
    fn empty_secret_and_config_rejected() {
        let c = json!({"mode": "test", "publishable_key": "pbk"});
        assert!(PaynetSettings::from_values(&c, &json!({"secret_key": ""})).is_err());
        assert!(PaynetSettings::from_values(&c, &json!({"secret_key": "  "})).is_err());
        assert!(
            PaynetSettings::from_values(&json!({"mode": "test"}), &json!({"secret_key": "s"}))
                .is_err()
        );
        assert!(
            PaynetSettings::from_values(
                &json!({"mode": "test", "publishable_key": "p", "installments": "3,x"}),
                &json!({"secret_key": "s"})
            )
            .is_err()
        );
        assert_eq!(parse_installments("6, 3,6").unwrap(), vec![0, 6, 3]);
        let mut s = settings("https://x.example");
        s.secret_key = "".into();
        assert!(PaynetGateway::new(s).is_err());
    }

    #[tokio::test]
    async fn non_try_rejected() {
        let gw = PaynetGateway::new(settings("https://x.example")).unwrap();
        let mut req = request();
        req.amount = Money::new(dec!(10), Currency::USD);
        assert!(gw.create_checkout(&req).await.is_err());
    }
}
