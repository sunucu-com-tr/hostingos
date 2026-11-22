//! NestPay / Payten ("EST") bank virtual POS — **3D Pay Hosting** model with
//! the **ver3** hash. One configurable implementation for the banks running
//! the Payten (ex Asseco SEE) NestPay gateway (`/fim/est3Dgate`).
//!
//! ## Sources (read 2026-11-18)
//!
//! * Payten "Üye İş Yeri Entegrasyon 3D Modeli", v1.3 of 31 Jan 2024 ("Hash
//!   versiyonu ve Hash algoritması değiştirildi"; hash ver3 / SHA-512) —
//!   <https://github.com/mewebstudio/pos/files/14418533/Nestpay_Entegrasyon_Dokumani_3D.ver3.pdf>
//!   (Payten hands this PDF to merchants; the copy read is the one attached to
//!   the `mewebstudio/pos` issue tracker — no public Payten-hosted URL was found).
//! * Nestpay "Entegrasyon Dokümanı 3D Pay Hosting Modeli", v1.3 of 24 Apr 2014
//!   (v1.2: `callbackUrl` added) —
//!   <https://github.com/mewebstudio/pos/files/14705215/Nestpay_Entegrasyon_Dokumani_3D_PayHosting.pdf>
//! * Ziraat Bankası NestPay test information —
//!   <https://github.com/mewebstudio/pos/files/14705222/Nestpay_ZiraatBankasi_Test_Bilgileri-.pdf>
//!
//! **API version:** gate model `storetype=3d_pay_hosting`, `hashAlgorithm=ver3`.
//!
//! ## Flow
//!
//! 1. [`create_checkout`](PaymentGateway::create_checkout) returns a
//!    [`CheckoutSession::Form`] that the browser POSTs to the bank's
//!    `https://{host}/fim/est3Dgate`. Card data is typed on the bank's hosted
//!    page; nothing card-related reaches HostingOS.
//! 2. The gate authenticates (3-D Secure) **and** provisions the payment
//!    itself (pay hosting: no second provision call by the merchant), then
//!    POSTs every request parameter plus the result to `okUrl` / `failUrl`
//!    (browser) and to `callbackUrl` (server to server, retried every five
//!    minutes until the merchant answers `Approved`).
//!
//! ## Hash ver3 (request and response, identical construction)
//!
//! * take every posted parameter except `hash` and `encoding` (names compared
//!   case-insensitively; the response's own `HASH` is therefore excluded too);
//! * sort by parameter **name**, case-insensitively, A→Z;
//! * escape each **value**: `\` → `\\`, then `|` → `\|`; empty values stay;
//! * join the values with `|`, append `|` + store key;
//! * `hash = Base64(SHA-512(UTF-8 plaintext))`.
//!
//! Doc example (verified in the tests): clientid `100200127`, amount `95.93`,
//! … store key `TEST1234` →
//! `95.93|billToCompany|name|…|3D|Auth|TEST1234`.
//!
//! ## Callback verification (fail closed)
//!
//! A response is accepted only when the store key is set, `HASH` matches in
//! constant time, `hashAlgorithm` is `ver3`, `clientid` is ours, `oid` is
//! present and `ReturnOid` (when sent) equals it, and no parameter name occurs
//! twice. `Response=Approved` **and** `ProcReturnCode=00` → `Paid`, otherwise
//! `Failed`. The signed `amount`/`currency` become the event amount and the
//! event is flagged [`RAW_EXACT_ATTEMPT`](crate::RAW_EXACT_ATTEMPT) so billing
//! applies it only to the pending attempt with that exact amount. Browser and
//! server deliveries share the event id `nestpay:{oid}:{status}`, so whichever
//! arrives second is a replay.
//!
//! ## Hosts
//!
//! * Test gate (Payten integration environment):
//!   `https://entegrasyon.asseco-see.com.tr/fim/est3Dgate`
//! * Ziraat Bankası live: `https://sanalpos2.ziraatbank.com.tr/fim/est3Dgate`
//! * Other banks: the live gate URL from the bank's welcome pack; it must be
//!   `https`, end in `/fim/est3Dgate` and (for a named bank) be on the bank's
//!   own domain.
//!
//! ## Not supported
//!
//! Refunds / cancels: the NestPay XML API (`/fim/api`, `CC5Request`) is not in
//! a public document, so refunds are made in the bank's merchant panel and
//! recorded in HostingOS by staff. Stored cards: not part of 3D Pay Hosting.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hos_core::crypto;
use hos_core::domain::{IntegrationCategory, Locale};
use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{
    CheckoutRequest, CheckoutSession, FormMethod, GatewayCapabilities, PaymentEvent,
    PaymentEventKind, PaymentGateway, RefundRequest, RefundResult, WebhookOutcome, WebhookReply,
    WebhookRequest,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, SelectOption, kinds,
};
use hos_core::money::{Currency, Money};
use hos_core::secret::SecretString;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha512};

use crate::common::{self, Mode, ReturnSigner, auth, ensure_positive, is_alphanumeric, truncate};

/// Payten integration (test) gate.
pub const TEST_GATE: &str = "https://entegrasyon.asseco-see.com.tr/fim/est3Dgate";
/// Ziraat Bankası live gate.
pub const ZIRAAT_GATE: &str = "https://sanalpos2.ziraatbank.com.tr/fim/est3Dgate";
/// Currencies with ISO 4217 numeric codes sent as `currency`.
pub const CURRENCIES: [(&str, &str); 4] = [
    ("TRY", "949"),
    ("USD", "840"),
    ("EUR", "978"),
    ("GBP", "826"),
];
/// `metadata` key of a [`CheckoutRequest`] carrying the customer's chosen
/// installment count (`"2"`…`"12"`); absent / `"1"` = single payment.
pub const INSTALLMENTS_METADATA: &str = "installments";

const GATE_PATH: &str = "/fim/est3Dgate";
const STORE_TYPE: &str = "3d_pay_hosting";
const HASH_ALGORITHM: &str = "ver3";
const SOURCE_PARAM: &str = "hos_src";
const SOURCE_CALLBACK: &str = "callback";
const SIGNER_DOMAIN: &str = "nestpay";

/// A bank on the NestPay platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BankPreset {
    /// Config value.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Documented live gate, when published.
    pub live_gate: Option<&'static str>,
    /// Domain the live gate must be on (`None` = any host).
    pub domain: Option<&'static str>,
}

/// Bank presets. Only Ziraat's live gate is in a document we could read; the
/// others take the gate URL from the bank's welcome pack, restricted to the
/// bank's own domain.
pub const BANKS: [BankPreset; 6] = [
    BankPreset {
        id: "ziraat",
        name: "Ziraat Bankası",
        live_gate: Some(ZIRAAT_GATE),
        domain: Some("ziraatbank.com.tr"),
    },
    BankPreset {
        id: "isbank",
        name: "Türkiye İş Bankası",
        live_gate: None,
        domain: Some("isbank.com.tr"),
    },
    BankPreset {
        id: "halkbank",
        name: "Halkbank",
        live_gate: None,
        domain: Some("halkbank.com.tr"),
    },
    BankPreset {
        id: "teb",
        name: "TEB",
        live_gate: None,
        domain: Some("teb.com.tr"),
    },
    BankPreset {
        id: "sekerbank",
        name: "Şekerbank",
        live_gate: None,
        domain: Some("sekerbank.com.tr"),
    },
    BankPreset {
        id: "other",
        name: "Other NestPay bank",
        live_gate: None,
        domain: None,
    },
];

fn bank(id: &str) -> Option<&'static BankPreset> {
    BANKS.iter().find(|b| b.id == id)
}

fn numeric_currency(currency: Currency) -> Result<&'static str, IntegrationError> {
    CURRENCIES
        .iter()
        .find(|(alpha, _)| *alpha == currency.as_str())
        .map(|(_, numeric)| *numeric)
        .ok_or_else(|| {
            IntegrationError::InvalidInput(format!("NestPay does not support {currency}"))
        })
}

fn currency_from_numeric(code: &str) -> Option<Currency> {
    CURRENCIES
        .iter()
        .find(|(_, numeric)| *numeric == code.trim())
        .and_then(|(alpha, _)| Currency::new(alpha).ok())
}

fn excluded_from_hash(name: &str) -> bool {
    name.eq_ignore_ascii_case("hash") || name.eq_ignore_ascii_case("encoding")
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

/// The ver3 plaintext of `params` (see the module docs).
pub fn ver3_plaintext(params: &[(String, String)], store_key: &str) -> String {
    let mut sorted: Vec<&(String, String)> = params
        .iter()
        .filter(|(name, _)| !excluded_from_hash(name))
        .collect();
    sorted.sort_by(|(a, _), (b, _)| {
        a.to_ascii_lowercase()
            .cmp(&b.to_ascii_lowercase())
            .then_with(|| a.cmp(b))
    });
    let mut parts: Vec<String> = sorted.iter().map(|(_, v)| escape(v)).collect();
    parts.push(escape(store_key));
    parts.join("|")
}

/// `Base64(SHA-512(ver3_plaintext))`.
pub fn ver3_hash(params: &[(String, String)], store_key: &str) -> String {
    STANDARD.encode(Sha512::digest(ver3_plaintext(params, store_key).as_bytes()))
}

/// NestPay settings.
#[derive(Debug, Clone)]
pub struct NestpaySettings {
    /// Bank preset id ([`BANKS`]).
    pub bank: String,
    /// Merchant id (`clientid`).
    pub client_id: String,
    /// Store key (İşyeri güvenlik anahtarı).
    pub store_key: SecretString,
    /// Gate URL the browser posts to.
    pub gate_url: String,
    /// Test or live.
    pub mode: Mode,
    /// Highest installment count offered (`0`/`1` = single payment only).
    pub max_installment: u32,
    /// HTTP timeout (connection test only).
    pub timeout: Duration,
}

impl NestpaySettings {
    /// Reads settings from integration JSON.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let bank_id = c.string_or("bank", "ziraat")?;
        let preset = bank(&bank_id)
            .ok_or_else(|| IntegrationError::Config("config.bank is not a known bank".into()))?;
        let client_id = c.str("client_id")?.trim().to_owned();
        if !is_alphanumeric(&client_id) || client_id.len() > 15 {
            return Err(IntegrationError::Config(
                "config.client_id must be alphanumeric, at most 15 characters".into(),
            ));
        }
        let store_key = s.secret("store_key")?;
        if store_key.expose_str().trim().is_empty() {
            return Err(IntegrationError::Config(
                "secrets.store_key is required".into(),
            ));
        }
        let mode = Mode::from_config(&c)?;
        let configured = c
            .opt_str("gate_url")?
            .map(str::trim)
            .filter(|u| !u.is_empty());
        let gate_url = match (mode, configured) {
            (Mode::Test, None) => TEST_GATE.to_owned(),
            (Mode::Live, None) => preset.live_gate.map(str::to_owned).ok_or_else(|| {
                IntegrationError::Config(format!(
                    "config.gate_url is required for {} (live gate URL from the bank)",
                    preset.name
                ))
            })?,
            (Mode::Test, Some(url)) => validate_gate(url, None)?,
            (Mode::Live, Some(url)) => validate_gate(url, preset.domain)?,
        };
        let max_installment = u32::try_from(c.u64_or("max_installment", 0)?)
            .ok()
            .filter(|n| *n <= 12)
            .ok_or_else(|| {
                IntegrationError::Config("config.max_installment must be 0–12".into())
            })?;
        Ok(Self {
            bank: bank_id,
            client_id,
            store_key,
            gate_url,
            mode,
            max_installment,
            timeout: common::DEFAULT_TIMEOUT,
        })
    }
}

/// Checks a gate URL: `https`, a domain host (on `domain` when given), path
/// `/fim/est3Dgate`, no query / credentials.
fn validate_gate(raw: &str, domain: Option<&str>) -> Result<String, IntegrationError> {
    let bad = |why: &str| IntegrationError::Config(format!("config.gate_url {why}"));
    let url = url::Url::parse(raw).map_err(|_| bad("is not a URL"))?;
    if url.scheme() != "https" {
        return Err(bad("must use https"));
    }
    let Some(url::Host::Domain(host)) = url.host() else {
        return Err(bad("must have a domain name host"));
    };
    let host = host.to_ascii_lowercase();
    if let Some(domain) = domain
        && host != domain
        && !host.ends_with(&format!(".{domain}"))
    {
        return Err(bad(&format!("must be on {domain}")));
    }
    if !url.path().eq_ignore_ascii_case(GATE_PATH) {
        return Err(bad("must end in /fim/est3Dgate"));
    }
    if url.query().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(bad("must not carry a query or credentials"));
    }
    Ok(url.to_string())
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(
        kinds::NESTPAY,
        "NestPay / Payten 3D Pay Hosting",
        IntegrationCategory::Gateway,
    )
    .description(
        "Bank virtual POS on the Payten NestPay platform (Ziraat, İş Bankası, Halkbank, TEB, \
         Şekerbank …), 3D Pay Hosting model with hash ver3: the card is entered on the bank's \
         page. Ask the bank to enable 3D_PAY_HOSTING and hash ver3. The callback URL is sent \
         with every payment (/webhooks/gateway/{integration_id}); nothing to set in the bank \
         panel. Refunds are made in the bank's merchant panel.",
    )
    .docs_url("https://github.com/mewebstudio/pos/files/14418533/Nestpay_Entegrasyon_Dokumani_3D.ver3.pdf")
    .config_field(
        FieldSpec::select(
            "bank",
            "Bank",
            BANKS
                .iter()
                .map(|b| SelectOption::new(b.id, b.name))
                .collect(),
        )
        .required()
        .default_value("ziraat"),
    )
    .config_field(
        common::mode_field()
            .help("Test uses Payten's integration gate (entegrasyon.asseco-see.com.tr)."),
    )
    .config_field(
        FieldSpec::text("client_id", "Merchant id (clientid)")
            .required()
            .placeholder("100200127"),
    )
    .config_field(
        FieldSpec::url("gate_url", "Gate URL")
            .placeholder("https://…/fim/est3Dgate")
            .help(
                "Leave empty for the test gate or Ziraat live. Other banks: the live 3D gate \
                 URL from the bank's welcome pack (must be on the bank's domain and end in \
                 /fim/est3Dgate).",
            ),
    )
    .config_field(
        FieldSpec::number("max_installment", "Max installments")
            .default_value(0)
            .help("0 = single payment only; otherwise the highest count (2–12) offered."),
    )
    .secret_field(
        FieldSpec::password("store_key", "Store key (storekey)")
            .required()
            .help("İşyeri güvenlik anahtarı, set in the bank's merchant panel."),
    )
}

/// Static capabilities.
pub fn capabilities() -> GatewayCapabilities {
    GatewayCapabilities {
        refunds: false,
        partial_refunds: false,
        stored_payment_methods: false,
        installments: true,
        offline: false,
        currencies: CURRENCIES
            .iter()
            .filter_map(|(c, _)| Currency::new(c).ok())
            .collect(),
    }
}

/// The NestPay gateway.
#[derive(Debug)]
pub struct NestpayGateway {
    settings: NestpaySettings,
    signer: ReturnSigner,
    http: reqwest::Client,
}

impl NestpayGateway {
    /// Creates the gateway.
    pub fn new(settings: NestpaySettings) -> Result<Self, IntegrationError> {
        if settings.store_key.expose_str().is_empty() {
            return Err(IntegrationError::Config(
                "secrets.store_key is required".into(),
            ));
        }
        let http = common::http_client(settings.timeout)?;
        let signer = ReturnSigner::new(settings.store_key.expose_str().as_bytes(), SIGNER_DOMAIN);
        Ok(Self {
            settings,
            signer,
            http,
        })
    }

    fn installments(&self, request: &CheckoutRequest) -> Result<String, IntegrationError> {
        let Some(raw) = request
            .metadata
            .get(INSTALLMENTS_METADATA)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && *s != "1" && *s != "0")
        else {
            return Ok(String::new());
        };
        let count: u32 = raw.parse().map_err(|_| {
            IntegrationError::InvalidInput("installment count must be a number".into())
        })?;
        if count < 2 || count > self.settings.max_installment {
            return Err(IntegrationError::InvalidInput(format!(
                "installments must be 2–{} for this gateway",
                self.settings.max_installment.max(1)
            )));
        }
        Ok(count.to_string())
    }

    /// The gate form fields without `hash` (pure; `rnd` injected for tests).
    fn gate_fields(
        &self,
        request: &CheckoutRequest,
        rnd: &str,
    ) -> Result<Vec<(String, String)>, IntegrationError> {
        ensure_positive(&request.amount)?;
        let oid = request.reference.as_str();
        if !is_alphanumeric(oid) || oid.len() > 64 {
            return Err(IntegrationError::InvalidInput(
                "NestPay oid (reference) must be alphanumeric, at most 64 characters".into(),
            ));
        }
        let currency = numeric_currency(request.amount.currency)?;
        let taksit = self.installments(request)?;
        let back = self.signer.signed_url(
            &request.webhook_url,
            &request.return_url,
            &request.cancel_url,
        )?;
        let mut callback = common::http_url(&request.webhook_url, "webhook_url")?;
        callback
            .query_pairs_mut()
            .append_pair(SOURCE_PARAM, SOURCE_CALLBACK);
        let lang = match request.locale {
            Some(Locale::En) => "en",
            _ => "tr",
        };
        let field = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        Ok(vec![
            field("clientid", &self.settings.client_id),
            field("storetype", STORE_TYPE),
            field("hashAlgorithm", HASH_ALGORITHM),
            field("islemtipi", "Auth"),
            field("amount", &request.amount.round().to_decimal_string()),
            field("currency", currency),
            field("oid", oid),
            field("okUrl", &back),
            field("failUrl", &back),
            field("callbackUrl", callback.as_str()),
            field("lang", lang),
            field("rnd", rnd),
            field("taksit", &taksit),
        ])
    }

    /// Verifies a gate response (see the module docs) and returns its fields.
    fn verify_response(
        &self,
        fields: &[(String, String)],
    ) -> Result<Map<String, Value>, IntegrationError> {
        let store_key = self.settings.store_key.expose_str();
        if store_key.is_empty() {
            return Err(auth("NestPay store key is not configured"));
        }
        let mut seen = std::collections::HashSet::new();
        for (name, _) in fields {
            if !excluded_from_hash(name) && !seen.insert(name.to_ascii_lowercase()) {
                return Err(auth("NestPay response repeats a parameter"));
            }
        }
        let Some(received) = fields
            .iter()
            .find(|(name, _)| name == "HASH")
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
        else {
            return Err(auth("NestPay response is not signed"));
        };
        let expected = ver3_hash(fields, store_key);
        if !crypto::constant_time_eq(expected.as_bytes(), received.as_bytes()) {
            return Err(auth("NestPay response hash mismatch"));
        }
        let mut map = Map::new();
        for (name, value) in fields {
            if !excluded_from_hash(name) {
                map.insert(name.to_ascii_lowercase(), Value::String(value.clone()));
            }
        }
        let get = |k: &str| map.get(k).and_then(Value::as_str).unwrap_or_default();
        if !get("hashalgorithm").eq_ignore_ascii_case(HASH_ALGORITHM) {
            return Err(auth("NestPay response is not hash ver3"));
        }
        if get("clientid") != self.settings.client_id {
            return Err(auth("NestPay response is for another merchant"));
        }
        let oid = get("oid");
        if !is_alphanumeric(oid) {
            return Err(auth("NestPay response has no valid oid"));
        }
        if let Some(r) = map.get("returnoid").and_then(Value::as_str)
            && !r.is_empty()
            && r != oid
        {
            return Err(auth("NestPay ReturnOid differs from oid"));
        }
        Ok(map)
    }
}

#[async_trait]
impl PaymentGateway for NestpayGateway {
    fn kind(&self) -> &'static str {
        kinds::NESTPAY
    }

    fn capabilities(&self) -> GatewayCapabilities {
        GatewayCapabilities {
            installments: self.settings.max_installment >= 2,
            ..capabilities()
        }
    }

    /// `2..=max_installment` for TRY amounts (Turkish card installments);
    /// none otherwise. The chosen count travels as
    /// `metadata["installments"]` and is checked again in the gate fields.
    fn installment_choices(&self, amount: &Money) -> Vec<u32> {
        if amount.currency != Currency::TRY || self.settings.max_installment < 2 {
            return Vec::new();
        }
        (2..=self.settings.max_installment).collect()
    }

    /// Reachability of the gate only: NestPay has no public API that checks
    /// the store key; the first test payment does.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let response = common::send(self.http.get(&self.settings.gate_url)).await?;
        if response.status >= 500 {
            return Err(IntegrationError::Remote {
                status: Some(response.status),
                code: None,
                message: "NestPay gate is not available".into(),
            });
        }
        Ok(ConnectionInfo {
            server_version: Some(format!("{STORE_TYPE} / hash {HASH_ALGORITHM}")),
            details: Some(format!(
                "gate reachable ({}, {}); the store key is verified by the first test payment",
                bank(&self.settings.bank).map_or("NestPay", |b| b.name),
                if self.settings.mode.is_live() {
                    "live"
                } else {
                    "test"
                },
            )),
        })
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        let rnd = hex_rnd();
        let mut fields = self.gate_fields(request, &rnd)?;
        let hash = ver3_hash(&fields, self.settings.store_key.expose_str());
        fields.push(("hash".to_owned(), hash));
        Ok(CheckoutSession::Form {
            action: self.settings.gate_url.clone(),
            method: FormMethod::Post,
            fields,
            gateway_ref: Some(request.reference.clone()),
        })
    }

    async fn parse_webhook(
        &self,
        request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        let fields = request.form_fields();
        let map = self.verify_response(&fields)?;
        let get = |k: &str| {
            map.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let oid = get("oid").unwrap_or_default().to_owned();
        let currency = get("currency")
            .and_then(currency_from_numeric)
            .ok_or_else(|| auth("NestPay response has an unknown currency"))?;
        let amount = get("amount")
            .map(|a| a.replace(',', "."))
            .and_then(|a| a.parse::<rust_decimal::Decimal>().ok())
            .filter(|a| *a > rust_decimal::Decimal::ZERO)
            .map(|a| Money::new(a, currency))
            .ok_or_else(|| auth("NestPay response has no valid amount"))?;
        let approved = get("response") == Some("Approved") && get("procreturncode") == Some("00");
        let (kind, status) = if approved {
            (PaymentEventKind::Paid, "paid")
        } else {
            (PaymentEventKind::Failed, "failed")
        };
        let message = (!approved).then(|| {
            let text = get("errmsg")
                .or(get("mderrormsg"))
                .unwrap_or("payment declined");
            format!(
                "{} ({}, mdStatus {})",
                truncate(text, 200),
                get("procreturncode").unwrap_or("-"),
                get("mdstatus").unwrap_or("-")
            )
        });
        // Card / 3-D Secure data (MaskedPan, md, cavv, xid, eci) is not kept.
        let mut raw = json!({
            "oid": oid,
            "Response": get("response"),
            "ProcReturnCode": get("procreturncode"),
            "mdStatus": get("mdstatus"),
            "TransId": get("transid"),
            "AuthCode": get("authcode"),
            "HostRefNum": get("hostrefnum"),
            "ErrMsg": get("errmsg").map(|m| truncate(m, 255)),
            "amount": get("amount"),
            "currency": get("currency"),
            "taksit": get("taksit"),
            "trxdate": get("extra.trxdate"),
        });
        crate::tag_raw(&mut raw, crate::RAW_EXACT_ATTEMPT, Value::Bool(true));
        let event = PaymentEvent {
            kind,
            event_id: Some(format!("nestpay:{oid}:{status}")),
            gateway_txn_id: oid.clone(),
            invoice_ref: Some(oid),
            amount,
            fee: None,
            occurred_at: None,
            message,
            payment_method_ref: None,
            gateway_refund_id: None,
            raw,
        };
        let pairs = request.query_pairs();
        let from_callback = pairs
            .iter()
            .any(|(k, v)| k == SOURCE_PARAM && v == SOURCE_CALLBACK);
        let reply = if from_callback {
            // The gate retries until it reads exactly this.
            WebhookReply::Text("Approved".into())
        } else {
            match self.signer.verify(&pairs) {
                Some(targets) if approved => WebhookReply::Redirect(targets.ok),
                Some(targets) => WebhookReply::Redirect(targets.fail),
                None => WebhookReply::Empty,
            }
        };
        Ok(WebhookOutcome {
            events: vec![event],
            reply,
        })
    }

    async fn refund(&self, _request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        Err(IntegrationError::Unsupported(
            "NestPay refunds are made in the bank's merchant panel",
        ))
    }
}

/// 20 random hex characters (the documented fixed `rnd` length).
fn hex_rnd() -> String {
    crypto::random_bytes::<10>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::ids::InvoiceId;
    use hos_core::integrations::gateway::PaymentCustomer;
    use http::HeaderMap;
    use rust_decimal_macros::dec;
    use std::collections::BTreeMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const KEY: &str = "TEST1234";

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn settings() -> NestpaySettings {
        NestpaySettings::from_values(
            &json!({"bank": "ziraat", "mode": "test", "client_id": "100200127", "max_installment": 6}),
            &json!({"store_key": KEY}),
        )
        .unwrap()
    }

    fn gateway() -> NestpayGateway {
        NestpayGateway::new(settings()).unwrap()
    }

    #[test]
    fn installment_choices_follow_max_installment_for_try_only() {
        let gw = gateway();
        assert_eq!(
            gw.installment_choices(&Money::new(dec!(100), Currency::TRY)),
            vec![2, 3, 4, 5, 6]
        );
        assert!(
            gw.installment_choices(&Money::new(dec!(100), Currency::EUR))
                .is_empty()
        );
        let single = NestpayGateway::new(
            NestpaySettings::from_values(
                &json!({"bank": "ziraat", "mode": "test", "client_id": "100200127"}),
                &json!({"store_key": KEY}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            single
                .installment_choices(&Money::new(dec!(100), Currency::TRY))
                .is_empty()
        );
    }

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "HOS42ABC".into(),
            amount: Money::new(dec!(1250.5), Currency::TRY),
            description: "Fatura".into(),
            customer: PaymentCustomer::default(),
            items: Vec::new(),
            return_url: "https://shop.example/client/invoices/7?paid=1".into(),
            cancel_url: "https://shop.example/client/invoices/7".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale: None,
            save_payment_method: false,
            metadata: BTreeMap::new(),
        }
    }

    /// Fields the gate posts back: the request fields plus the result,
    /// signed with `key` like Payten does.
    fn gate_response(
        gw: &NestpayGateway,
        req: &CheckoutRequest,
        result: &[(&str, &str)],
        key: &str,
    ) -> Vec<(String, String)> {
        let mut fields = gw.gate_fields(req, "0123456789abcdef0123").unwrap();
        fields.push(("hash".into(), "requesthash".into()));
        fields.extend(pairs(result));
        let hash = ver3_hash(&fields, key);
        fields.push(("HASH".into(), hash));
        fields
    }

    fn webhook(fields: &[(String, String)], query: Option<&str>) -> WebhookRequest {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        WebhookRequest {
            headers: HeaderMap::new(),
            body: body.into_bytes(),
            query: query.map(str::to_owned),
            remote_ip: None,
        }
    }

    const APPROVED: [(&str, &str); 6] = [
        ("Response", "Approved"),
        ("ProcReturnCode", "00"),
        ("mdStatus", "1"),
        ("TransId", "24123ABC"),
        ("ReturnOid", "HOS42ABC"),
        ("MaskedPan", "454360***4360"),
    ];

    #[test]
    fn ver3_doc_example_vector() {
        // Parameters of the Payten 3D ver3 document's example (names as listed).
        let params = pairs(&[
            ("clientId", "100200127"),
            ("amount", "95.93"),
            (
                "okurl",
                "http://localhost:8080/SampleCodeJSPTest/GenericVer3ResponseHandler",
            ),
            (
                "failUrl",
                "http://localhost:8080/SampleCodeJSPTest/GenericVer3ResponseHandler",
            ),
            ("TranType", "Auth"),
            ("Instalment", ""),
            (
                "callbackUrl",
                "http://localhost:8080/SampleCodeJSPTest/GateResponseControl.jsp",
            ),
            ("currency", "949"),
            ("rnd", "87954458746"),
            ("storeType", "3D"),
            ("lang", "tr"),
            ("hashAlgorithm", "ver3"),
            ("BillToName", "name"),
            ("BillTocompany", "billToCompany"),
            ("refreshTime", "5"),
            ("encoding", "utf-8"),
            ("hash", "ignored"),
        ]);
        assert_eq!(
            ver3_plaintext(&params, KEY),
            "95.93|billToCompany|name|http://localhost:8080/SampleCodeJSPTest/GateResponseControl.jsp|100200127|949|http://localhost:8080/SampleCodeJSPTest/GenericVer3ResponseHandler|ver3||tr|http://localhost:8080/SampleCodeJSPTest/GenericVer3ResponseHandler|5|87954458746|3D|Auth|TEST1234"
        );
        // Python: base64.b64encode(hashlib.sha512(plaintext.encode()).digest())
        assert_eq!(
            ver3_hash(&params, KEY),
            "Lq4rSjZrfKHIdfglyEv1M3/YcP5kSkDOPXftDfIadqq6P7QVXqAclz++B/7bm7+UYtML6fI59oqoxnvGEx10JQ=="
        );
    }

    #[test]
    fn ver3_escaping_vector() {
        // The document's escaping example: `ORDER-256712jbs\j6b|`.
        let params = pairs(&[("b", "x"), ("a", "ORDER-256712jbs\\j6b|")]);
        assert_eq!(
            ver3_plaintext(&params, "K"),
            "ORDER-256712jbs\\\\j6b\\||x|K"
        );
        assert_eq!(
            ver3_hash(&params, "K"),
            "7S7ibhY2rd0QKjDcLpQ4dcU3OCb4FZlaiXOUrRTbNpwU5cpKm3CHZ4c5RQ1EGqBALFZ65uM1VwxA/ETKYCvIGw=="
        );
    }

    #[tokio::test]
    async fn checkout_form_fields_and_hash() {
        let gw = gateway();
        let CheckoutSession::Form {
            action,
            method,
            fields,
            gateway_ref,
        } = gw.create_checkout(&request()).await.unwrap()
        else {
            panic!("form expected");
        };
        assert_eq!(action, TEST_GATE);
        assert_eq!(method, FormMethod::Post);
        assert_eq!(gateway_ref.as_deref(), Some("HOS42ABC"));
        let f = |k: &str| {
            fields
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(f("clientid"), "100200127");
        assert_eq!(f("storetype"), "3d_pay_hosting");
        assert_eq!(f("hashAlgorithm"), "ver3");
        assert_eq!(f("islemtipi"), "Auth");
        assert_eq!(f("amount"), "1250.50");
        assert_eq!(f("currency"), "949");
        assert_eq!(f("oid"), "HOS42ABC");
        assert_eq!(f("taksit"), "");
        assert_eq!(f("rnd").len(), 20);
        assert!(f("callbackUrl").ends_with("/webhooks/gateway/abc?hos_src=callback"));
        assert!(f("okUrl").contains("hos_sig="));
        // Independent recomputation: the documented order, typed out by hand.
        let plain = [
            f("amount"),
            f("callbackUrl"),
            f("clientid"),
            f("currency"),
            f("failUrl"),
            f("hashAlgorithm"),
            f("islemtipi"),
            f("lang"),
            f("oid"),
            f("okUrl"),
            f("rnd"),
            f("storetype"),
            f("taksit"),
            KEY,
        ]
        .join("|");
        assert_eq!(f("hash"), STANDARD.encode(Sha512::digest(plain.as_bytes())));
        // No card fields are ever produced.
        assert!(!fields.iter().any(|(k, _)| k == "pan"));
    }

    #[tokio::test]
    async fn installments_are_sent_and_amount_unchanged() {
        let gw = gateway();
        let mut req = request();
        req.metadata
            .insert(INSTALLMENTS_METADATA.into(), "3".into());
        let fields = gw.gate_fields(&req, "0123456789abcdef0123").unwrap();
        let get = |k: &str| fields.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(get("taksit"), "3");
        assert_eq!(get("amount"), "1250.50");
        // Signed response with taksit 3: the event pays the order amount only.
        let resp = gate_response(&gw, &req, &APPROVED, KEY);
        let out = gw.parse_webhook(&webhook(&resp, None)).await.unwrap();
        assert_eq!(
            out.events[0].amount,
            Money::new(dec!(1250.50), Currency::TRY)
        );
        assert_eq!(out.events[0].raw["taksit"], "3");
        // Above the configured maximum, or junk: refused before the gate.
        req.metadata
            .insert(INSTALLMENTS_METADATA.into(), "9".into());
        assert!(gw.gate_fields(&req, "x").is_err());
        req.metadata
            .insert(INSTALLMENTS_METADATA.into(), "abc".into());
        assert!(gw.gate_fields(&req, "x").is_err());
    }

    #[tokio::test]
    async fn approved_callback_and_browser_return() {
        let gw = gateway();
        let req = request();
        let resp = gate_response(&gw, &req, &APPROVED, KEY);
        // Server callback: answered with the literal `Approved`.
        let out = gw
            .parse_webhook(&webhook(&resp, Some("hos_src=callback")))
            .await
            .unwrap();
        assert_eq!(out.reply, WebhookReply::Text("Approved".into()));
        let event = &out.events[0];
        assert_eq!(event.kind, PaymentEventKind::Paid);
        assert_eq!(event.event_id.as_deref(), Some("nestpay:HOS42ABC:paid"));
        assert_eq!(event.invoice_ref.as_deref(), Some("HOS42ABC"));
        assert_eq!(event.amount, Money::new(dec!(1250.50), Currency::TRY));
        assert_eq!(event.raw[crate::RAW_EXACT_ATTEMPT], true);
        assert!(crate::event_requires_exact_attempt(event));
        assert!(!event.raw.to_string().contains("454360"));
        // Browser return to okUrl: same event id (replay-safe), 303 to return_url.
        let ok_url = url::Url::parse(&resp.iter().find(|(k, _)| k == "okUrl").unwrap().1).unwrap();
        let out2 = gw
            .parse_webhook(&webhook(&resp, ok_url.query()))
            .await
            .unwrap();
        assert_eq!(out2.events[0].event_id, event.event_id);
        assert_eq!(
            out2.reply,
            WebhookReply::Redirect("https://shop.example/client/invoices/7?paid=1".into())
        );
    }

    #[tokio::test]
    async fn declined_is_failed_with_reason() {
        let gw = gateway();
        let resp = gate_response(
            &gw,
            &request(),
            &[
                ("Response", "Declined"),
                ("ProcReturnCode", "05"),
                ("mdStatus", "1"),
                ("ErrMsg", "Red-Onaylanmadı"),
            ],
            KEY,
        );
        let out = gw.parse_webhook(&webhook(&resp, None)).await.unwrap();
        assert_eq!(out.events[0].kind, PaymentEventKind::Failed);
        assert_eq!(
            out.events[0].event_id.as_deref(),
            Some("nestpay:HOS42ABC:failed")
        );
        assert!(out.events[0].message.as_deref().unwrap().contains("05"));
        // "Approved" text with a non-00 code is not a payment.
        let resp = gate_response(
            &gw,
            &request(),
            &[("Response", "Approved"), ("ProcReturnCode", "99")],
            KEY,
        );
        let out = gw.parse_webhook(&webhook(&resp, None)).await.unwrap();
        assert_eq!(out.events[0].kind, PaymentEventKind::Failed);
    }

    #[tokio::test]
    async fn tampered_amount_or_hash_rejected() {
        let gw = gateway();
        let resp = gate_response(&gw, &request(), &APPROVED, KEY);
        let mut cheap = resp.clone();
        for (k, v) in &mut cheap {
            if k == "amount" {
                *v = "1.00".into();
            }
        }
        assert!(matches!(
            gw.parse_webhook(&webhook(&cheap, None)).await,
            Err(IntegrationError::Auth(_))
        ));
        let mut forged = resp.clone();
        forged.last_mut().unwrap().1 = STANDARD.encode([0u8; 64]);
        assert!(matches!(
            gw.parse_webhook(&webhook(&forged, None)).await,
            Err(IntegrationError::Auth(_))
        ));
        let unsigned: Vec<_> = resp.iter().filter(|(k, _)| k != "HASH").cloned().collect();
        assert!(gw.parse_webhook(&webhook(&unsigned, None)).await.is_err());
        // A repeated parameter (parser ambiguity) is refused even if signed.
        let mut dup = resp.clone();
        dup.insert(0, ("Response".into(), "Approved".into()));
        assert!(gw.parse_webhook(&webhook(&dup, None)).await.is_err());
        // A different hash algorithm is refused.
        let mut fields = gw.gate_fields(&request(), "r").unwrap();
        for (k, v) in &mut fields {
            if k == "hashAlgorithm" {
                *v = "ver2".into();
            }
        }
        fields.extend(pairs(&APPROVED));
        let h = ver3_hash(&fields, KEY);
        fields.push(("HASH".into(), h));
        assert!(gw.parse_webhook(&webhook(&fields, None)).await.is_err());
    }

    #[tokio::test]
    async fn other_integration_or_merchant_rejected() {
        let gw = gateway();
        // Signed with another store's key (integration B's callback sent to A).
        let resp = gate_response(&gw, &request(), &APPROVED, "OTHERKEY");
        assert!(matches!(
            gw.parse_webhook(&webhook(&resp, None)).await,
            Err(IntegrationError::Auth(_))
        ));
        // Validly signed with our key but for another clientid.
        let other = NestpayGateway::new(
            NestpaySettings::from_values(
                &json!({"bank": "ziraat", "mode": "test", "client_id": "999"}),
                &json!({"store_key": KEY}),
            )
            .unwrap(),
        )
        .unwrap();
        let resp = gate_response(&other, &request(), &APPROVED, KEY);
        assert!(matches!(
            gw.parse_webhook(&webhook(&resp, None)).await,
            Err(IntegrationError::Auth(_))
        ));
        // ReturnOid that differs from oid.
        let mut result = APPROVED.to_vec();
        result[4] = ("ReturnOid", "HOS99XYZ");
        let resp = gate_response(&gw, &request(), &result, KEY);
        assert!(gw.parse_webhook(&webhook(&resp, None)).await.is_err());
    }

    #[test]
    fn empty_secret_and_bad_gates_rejected() {
        let config = json!({"bank": "ziraat", "mode": "test", "client_id": "100200127"});
        assert!(NestpaySettings::from_values(&config, &json!({"store_key": ""})).is_err());
        assert!(NestpaySettings::from_values(&config, &json!({"store_key": "   "})).is_err());
        assert!(NestpaySettings::from_values(&config, &json!({})).is_err());
        let mut s = settings();
        s.store_key = SecretString::from("");
        assert!(NestpayGateway::new(s).is_err());
        let live = |bank: &str, gate: Option<&str>| {
            let mut c = json!({"bank": bank, "mode": "live", "client_id": "100200127"});
            if let Some(g) = gate {
                c["gate_url"] = json!(g);
            }
            NestpaySettings::from_values(&c, &json!({"store_key": KEY}))
        };
        assert_eq!(live("ziraat", None).unwrap().gate_url, ZIRAAT_GATE);
        assert!(live("isbank", None).is_err());
        assert!(
            live(
                "isbank",
                Some("https://sanalpos.isbank.com.tr/fim/est3Dgate")
            )
            .is_ok()
        );
        assert!(
            live(
                "isbank",
                Some("https://isbank.com.tr.evil.example/fim/est3Dgate")
            )
            .is_err()
        );
        assert!(
            live(
                "isbank",
                Some("http://sanalpos.isbank.com.tr/fim/est3Dgate")
            )
            .is_err()
        );
        assert!(live("isbank", Some("https://sanalpos.isbank.com.tr/pay")).is_err());
        assert!(live("other", Some("https://1.2.3.4/fim/est3Dgate")).is_err());
        assert!(live("other", Some("https://pos.example-bank.com/fim/est3Dgate")).is_ok());
        assert!(live("nope", None).is_err());
    }

    #[tokio::test]
    async fn refunds_unsupported_and_currency_checked() {
        let gw = gateway();
        let err = gw
            .refund(&RefundRequest {
                gateway_txn_id: "HOS42ABC".into(),
                amount: Money::new(dec!(10), Currency::TRY),
                reason: None,
                reference: "r1".into(),
                ip: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, IntegrationError::Unsupported(_)));
        let mut req = request();
        req.amount = Money::new(dec!(10), Currency::new("JPY").unwrap());
        assert!(gw.create_checkout(&req).await.is_err());
        req.amount = Money::new(dec!(10), Currency::USD);
        let CheckoutSession::Form { fields, .. } = gw.create_checkout(&req).await.unwrap() else {
            panic!()
        };
        assert!(fields.contains(&("currency".to_owned(), "840".to_owned())));
    }

    #[tokio::test]
    async fn connection_test_reaches_gate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fim/est3Dgate"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
            .expect(1)
            .mount(&server)
            .await;
        let mut s = settings();
        s.gate_url = format!("{}/fim/est3Dgate", server.uri());
        let info = NestpayGateway::new(s)
            .unwrap()
            .test_connection()
            .await
            .unwrap();
        assert!(info.details.unwrap().contains("reachable"));
    }
}
