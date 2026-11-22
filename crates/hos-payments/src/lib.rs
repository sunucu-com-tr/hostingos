//! `PaymentGateway` implementations for HostingOS (docs/ARCHITECTURE.md §4).
//!
//! | kind | module | checkout | webhook / callback | refund | stored card |
//! |---|---|---|---|---|---|
//! | `stripe` | [`stripe`] | Checkout Session redirect | `Stripe-Signature` HMAC, ±5 min, previous secret during rotation | `/v1/refunds` | off-session PaymentIntent |
//! | `iyzico` | [`iyzico`] | Checkout Form redirect / embed | callback token → CF detail; `X-IYZ-SIGNATURE-V3` | `/v2/payment/refund` | — |
//! | `paytr` | [`paytr`] | iFrame token | notification hash, reply `OK` | `/odeme/iade` | — |
//! | `paypal` | [`paypal`] | Orders v2 redirect, capture on return | `verify-webhook-signature` postback | `/v2/payments/captures/{id}/refund` | vault `vault_id` (opt-in) |
//! | `nestpay` | [`nestpay`] | 3D Pay Hosting form to the bank gate | hash ver3 (SHA-512) over the result, reply `Approved` | — (bank panel) | — |
//! | `paynet` | [`paynet`] | Paynet.js embed | signed form action → server-side `charge` | `reversed_request` + signed `succeedUrl` | — |
//! | `papara` | [`papara`] | redirect | IPN secret + `GET /payments` | `/payments/refund` | — |
//! | `bank_transfer` | [`bank_transfer`] | offline instructions | — | — | — |
//! | `mock` | [`mock`] | auto-posted signed form | `X-HOS-Mock-Signature` HMAC | simulated | simulated |
//!
//! Every online gateway uses a hosted / redirect / iframe payment page, so card
//! data never touches HostingOS servers (PCI DSS SAQ-A). Webhooks are verified
//! over the raw body; anything that cannot be verified is
//! [`IntegrationError::Auth`]. Amounts and currencies reported by the gateway
//! are carried unchanged into [`PaymentEvent`](hos_core::integrations::gateway::PaymentEvent);
//! comparing them with the invoice is `hos-billing`'s job.
//!
//! Web layer contract: `/webhooks/gateway/{integration_id}` must accept both
//! `POST` and `GET` (PayPal returns the buyer with a GET), pass the raw body,
//! headers and query string in a [`WebhookRequest`](hos_core::integrations::gateway::WebhookRequest),
//! and render [`WebhookReply`](hos_core::integrations::gateway::WebhookReply) as given
//! (PayTR needs a bare `OK`, iyzico/PayPal/mock browser returns need the 303).
//!
//! ## Extra event data in `PaymentEvent::raw`
//!
//! The gateway refund id of a `Refunded` event travels in
//! [`PaymentEvent::gateway_refund_id`](hos_core::integrations::gateway::PaymentEvent::gateway_refund_id)
//! (refunds are matched by it, so a refund completed synchronously is not
//! recorded a second time by its webhook). A few informational facts have no
//! field; gateways add them to `raw` under reserved top-level keys (read with
//! the helpers below):
//!
//! * [`RAW_REFUND_REFERENCE`] — the `RefundRequest::reference` the gateway
//!   echoes back for refunds started by HostingOS;
//! * [`RAW_INSTALLMENT_INTEREST`] / [`RAW_COLLECTED_TOTAL`] — installment
//!   interest the customer paid on top of the order amount (PayTR, iyzico).
//!   It is informational only: `PaymentEvent::amount` is the order amount, so
//!   the interest never becomes client credit;
//! * [`RAW_EXACT_ATTEMPT`] — billing must match the event to the pending
//!   attempt of the same integration with the same amount and currency;
//! * [`RAW_PAYMENT_METHOD_LABEL`] — brand / last four of a saved method
//!   (removed before the payload is persisted, see [`raw_for_storage`]).
//!
//! Gateway HTTP clients enforce `hos_core::net` ([`OutboundPolicy`](hos_core::net::OutboundPolicy)
//! resolver, no redirects, no proxies), 30 s timeouts and a 16 MiB body cap.

use std::sync::Arc;

use hos_core::error::IntegrationError;
use hos_core::integrations::gateway::{GatewayCapabilities, PaymentEvent, PaymentGateway};
use hos_core::integrations::{IntegrationDescriptor, kinds};
use hos_core::money::Money;
use serde_json::Value;

/// `raw` key: HostingOS refund reference echoed by the gateway.
pub const RAW_REFUND_REFERENCE: &str = "hos_refund_reference";
/// `raw` key: installment interest paid by the customer (decimal string).
pub const RAW_INSTALLMENT_INTEREST: &str = "hos_installment_interest";
/// `raw` key: total collected from the customer incl. interest (decimal string).
pub const RAW_COLLECTED_TOTAL: &str = "hos_collected_total";
/// `raw` key (`true`): the event must match a pending payment attempt of the
/// same integration exactly — same reference, amount and currency — or it is
/// rejected instead of being applied as a partial payment / credit. Set by
/// gateways whose callback carries the signed order amount (NestPay, Paynet,
/// Papara).
pub const RAW_EXACT_ATTEMPT: &str = "hos_exact_attempt";
/// `raw` key: display label of the stored payment method created by a
/// payment (`"VISA •••• 4242"`, `"PayPal"`) — brand and last four only.
pub const RAW_PAYMENT_METHOD_LABEL: &str = "hos_payment_method_label";

/// Whether the event asks billing for an exact attempt match ([`RAW_EXACT_ATTEMPT`]).
pub fn event_requires_exact_attempt(event: &PaymentEvent) -> bool {
    event.raw.get(RAW_EXACT_ATTEMPT).and_then(Value::as_bool) == Some(true)
}

/// Stored payment method label of a `Paid` event ([`RAW_PAYMENT_METHOD_LABEL`]).
pub fn event_payment_method_label(event: &PaymentEvent) -> Option<&str> {
    raw_str(event, RAW_PAYMENT_METHOD_LABEL)
}

/// An event's raw payload as it may be persisted (`webhooks_in`,
/// `transactions`): the stored-method label lives on the payment method
/// only, and card / 3-D Secure fields ([`CARD_FIELDS`]) are removed whatever
/// the gateway put there.
pub fn raw_for_storage(raw: &Value) -> Value {
    let mut raw = raw.clone();
    if let Some(map) = raw.as_object_mut() {
        map.remove(RAW_PAYMENT_METHOD_LABEL);
    }
    scrub_card_fields(&mut raw);
    raw
}

/// Card and 3-D Secure fields that are never persisted, compared without
/// case, `_` or `-` (`MaskedPan`, `masked_pan`): the card number in any form,
/// BIN / last four, expiry, security code, card tokens and the 3-D Secure
/// authentication data (`md`, `cavv`, `xid`, `eci`) of bank gates such as
/// NestPay.
pub const CARD_FIELDS: &[&str] = &[
    "pan",
    "maskedpan",
    "cardnumber",
    "cardno",
    "ccnumber",
    "binnumber",
    "lastfourdigits",
    "cardlastfour",
    "expiry",
    "expirydate",
    "expmonth",
    "expyear",
    "cardexpiry",
    "cvv",
    "cvv2",
    "cvc",
    "securitycode",
    "cardtoken",
    "carduserkey",
    "md",
    "cavv",
    "xid",
    "eci",
];

/// Whether `key` names a card / 3-D Secure field ([`CARD_FIELDS`]).
pub fn is_card_field(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect();
    CARD_FIELDS.contains(&norm.as_str())
}

/// Removes [`CARD_FIELDS`] from every JSON object in `value` (any depth).
/// Returns whether anything was removed.
pub fn scrub_card_fields(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let before = map.len();
            map.retain(|k, _| !is_card_field(k));
            let mut changed = map.len() != before;
            for child in map.values_mut() {
                changed |= scrub_card_fields(child);
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, child| scrub_card_fields(child) | acc),
        _ => false,
    }
}

/// A callback body as it may be stored for diagnosis: JSON (or a form,
/// turned into an object) without [`CARD_FIELDS`]. A body that is neither
/// is not kept (`None`): it cannot be checked for card data.
pub fn storable_body(body: &[u8]) -> Option<Value> {
    if body.is_empty() {
        return Some(Value::Null);
    }
    let mut value = match serde_json::from_slice::<Value>(body) {
        Ok(v) => v,
        Err(_) => {
            let text = std::str::from_utf8(body).ok()?;
            if text.contains(char::is_whitespace) && !text.contains('=') {
                return None;
            }
            let pairs: serde_json::Map<String, Value> =
                url::form_urlencoded::parse(text.as_bytes())
                    .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                    .collect();
            if pairs.is_empty() {
                return None;
            }
            Value::Object(pairs)
        }
    };
    scrub_card_fields(&mut value);
    Some(value)
}

/// A query string as it may be stored: its pairs without [`CARD_FIELDS`].
pub fn storable_query(query: &str) -> Value {
    Value::Object(
        url::form_urlencoded::parse(query.as_bytes())
            .filter(|(k, _)| !is_card_field(k))
            .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
            .collect(),
    )
}

/// Adds `key = value` to an event's raw payload (object).
pub(crate) fn tag_raw(raw: &mut Value, key: &str, value: Value) {
    if !raw.is_object() {
        *raw = Value::Object(serde_json::Map::new());
    }
    if let Some(map) = raw.as_object_mut() {
        map.insert(key.to_owned(), value);
    }
}

fn raw_str<'a>(event: &'a PaymentEvent, key: &str) -> Option<&'a str> {
    event
        .raw
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// HostingOS refund reference (`RefundRequest::reference`) of a `Refunded`
/// event, when the gateway echoed it.
pub fn event_refund_reference(event: &PaymentEvent) -> Option<&str> {
    raw_str(event, RAW_REFUND_REFERENCE)
}

/// Installment interest the customer paid on top of the event amount.
pub fn event_installment_interest(event: &PaymentEvent) -> Option<rust_decimal::Decimal> {
    raw_str(event, RAW_INSTALLMENT_INTEREST).and_then(|s| s.parse().ok())
}

/// Adds the HostingOS refund reference key to an event's raw payload.
pub(crate) fn tag_refund_reference(raw: &mut Value, reference: Option<&str>) {
    let Some(reference) = reference.map(str::trim).filter(|v| !v.is_empty()) else {
        return;
    };
    if !raw.is_object() {
        *raw = Value::Object(serde_json::Map::new());
    }
    if let Some(map) = raw.as_object_mut() {
        map.insert(
            RAW_REFUND_REFERENCE.to_owned(),
            Value::String(reference.to_owned()),
        );
    }
}

/// Splits a collected amount into the order amount and the installment
/// interest on top of it: `amount = min(order, collected)` (never more than
/// was collected), the rest is recorded in `raw` and is not part of the payment.
pub(crate) fn split_installment_interest(
    raw: &mut Value,
    order: Option<Money>,
    collected: Money,
) -> Money {
    let Some(order) = order.filter(|o| o.currency == collected.currency) else {
        return collected;
    };
    if order.amount >= collected.amount {
        return collected;
    }
    let interest = collected.amount - order.amount;
    if let Some(map) = raw.as_object_mut() {
        map.insert(
            RAW_INSTALLMENT_INTEREST.to_owned(),
            Value::String(interest.normalize().to_string()),
        );
        map.insert(
            RAW_COLLECTED_TOTAL.to_owned(),
            Value::String(collected.amount.normalize().to_string()),
        );
    }
    order
}

mod common;

pub mod bank_transfer;
pub mod iyzico;
pub mod mock;
pub mod nestpay;
pub mod papara;
pub mod paynet;
pub mod paypal;
pub mod paytr;
pub mod stripe;

pub use common::Mode;

/// Kind string of the [`mock`] gateway.
pub const MOCK: &str = mock::KIND;

/// Builds the gateway for an `integrations` row (`category = gateway`).
/// `config` / `secrets` are validated against [`descriptors`] first.
pub fn build(
    kind: &str,
    config: &Value,
    secrets: &Value,
) -> Result<Arc<dyn PaymentGateway>, IntegrationError> {
    let descriptor = descriptors()
        .into_iter()
        .find(|d| d.kind == kind)
        .ok_or_else(|| {
            IntegrationError::Config(format!("unknown payment gateway kind `{kind}`"))
        })?;
    descriptor.validate(config, secrets)?;
    Ok(match kind {
        kinds::STRIPE => Arc::new(stripe::StripeGateway::new(
            stripe::StripeSettings::from_values(config, secrets)?,
        )?),
        kinds::IYZICO => Arc::new(iyzico::IyzicoGateway::new(
            iyzico::IyzicoSettings::from_values(config, secrets)?,
        )?),
        kinds::PAYTR => Arc::new(paytr::PaytrGateway::new(
            paytr::PaytrSettings::from_values(config, secrets)?,
        )?),
        kinds::PAYPAL => Arc::new(paypal::PaypalGateway::new(
            paypal::PaypalSettings::from_values(config, secrets)?,
        )?),
        kinds::NESTPAY => Arc::new(nestpay::NestpayGateway::new(
            nestpay::NestpaySettings::from_values(config, secrets)?,
        )?),
        kinds::PAYNET => Arc::new(paynet::PaynetGateway::new(
            paynet::PaynetSettings::from_values(config, secrets)?,
        )?),
        kinds::PAPARA => Arc::new(papara::PaparaGateway::new(
            papara::PaparaSettings::from_values(config, secrets)?,
        )?),
        kinds::BANK_TRANSFER => Arc::new(bank_transfer::BankTransferGateway::new(
            bank_transfer::BankTransferSettings::from_values(config)?,
        )),
        mock::KIND => Arc::new(mock::MockGateway::new(mock::MockSettings::from_values(
            config, secrets,
        )?)),
        other => {
            return Err(IntegrationError::Config(format!(
                "unknown payment gateway kind `{other}`"
            )));
        }
    })
}

/// Admin form descriptors of every gateway kind.
pub fn descriptors() -> Vec<IntegrationDescriptor> {
    vec![
        stripe::descriptor(),
        iyzico::descriptor(),
        paytr::descriptor(),
        paypal::descriptor(),
        nestpay::descriptor(),
        paynet::descriptor(),
        papara::descriptor(),
        bank_transfer::descriptor(),
        mock::descriptor(),
    ]
}

/// Capabilities of a gateway kind before it is configured (supported
/// currencies, refunds, stored cards, installments, offline). A built gateway's
/// [`PaymentGateway::capabilities`] may be narrower (e.g. bank transfer
/// currencies follow the configured accounts, PayTR installments can be off).
pub fn capabilities(kind: &str) -> Option<GatewayCapabilities> {
    Some(match kind {
        kinds::STRIPE => stripe::capabilities(),
        kinds::IYZICO => iyzico::capabilities(),
        kinds::PAYTR => paytr::capabilities(),
        kinds::PAYPAL => paypal::capabilities(),
        kinds::NESTPAY => nestpay::capabilities(),
        kinds::PAYNET => paynet::capabilities(),
        kinds::PAPARA => papara::capabilities(),
        kinds::BANK_TRANSFER => bank_transfer::capabilities(),
        mock::KIND => mock::capabilities(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::domain::IntegrationCategory;
    use hos_core::money::Currency;
    use serde_json::json;

    #[test]
    fn card_fields_never_reach_storage() {
        let raw = json!({
            "oid": "INV1", "MaskedPan": "435508***4358", "md": "abc", "cavv": "AAAB",
            "xid": "x", "ECI": "05", "nested": {"card_number": "4111", "ok": 1},
            "list": [{"cvv2": "123", "amount": "10"}],
            RAW_PAYMENT_METHOD_LABEL: "VISA •••• 4358",
        });
        let stored = raw_for_storage(&raw);
        let text = stored.to_string();
        for leak in [
            "435508", "\"md\"", "AAAB", "\"xid\"", "ECI", "4111", "cvv2", "VISA",
        ] {
            assert!(!text.contains(leak), "{leak} kept: {text}");
        }
        assert_eq!(stored["oid"], "INV1");
        assert_eq!(stored["nested"]["ok"], 1);
        assert_eq!(stored["list"][0]["amount"], "10");
        // A different key that merely contains a card word is kept.
        assert!(!is_card_field("mdStatus") && !is_card_field("amount"));

        let form = storable_body(b"oid=INV1&MaskedPan=435508***4358&md=zz&amount=10").unwrap();
        assert_eq!(form, json!({"oid": "INV1", "amount": "10"}));
        assert_eq!(storable_body(b"not a form body"), None);
        assert_eq!(storable_query("cavv=1&ok=2&Eci=05"), json!({"ok": "2"}));
    }

    #[test]
    fn descriptors_cover_every_kind_and_serialize() {
        let ds = descriptors();
        let kinds: Vec<&str> = ds.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            [
                "stripe",
                "iyzico",
                "paytr",
                "paypal",
                "nestpay",
                "paynet",
                "papara",
                "bank_transfer",
                "mock"
            ]
        );
        for d in &ds {
            assert_eq!(d.category, IntegrationCategory::Gateway);
            let v = serde_json::to_value(d).unwrap();
            assert_eq!(v["kind"], d.kind);
            assert!(capabilities(d.kind).is_some());
        }
        assert!(capabilities("nope").is_none());
        assert!(
            capabilities("paypal")
                .unwrap()
                .supports_currency(Currency::USD)
        );
        assert!(
            !capabilities("paypal")
                .unwrap()
                .supports_currency(Currency::TRY)
        );
        assert!(capabilities("bank_transfer").unwrap().offline);
    }

    #[test]
    fn build_each_kind() {
        let cases = [
            (
                "stripe",
                json!({"mode": "test"}),
                json!({"secret_key": "sk_test_1", "webhook_secret": "whsec_1"}),
            ),
            (
                "iyzico",
                json!({"mode": "test"}),
                json!({"api_key": "k", "secret_key": "s"}),
            ),
            (
                "paytr",
                json!({"mode": "test", "merchant_id": "123456"}),
                json!({"merchant_key": "k", "merchant_salt": "s"}),
            ),
            (
                "paypal",
                json!({"mode": "live", "client_id": "c", "webhook_id": "WH-1"}),
                json!({"client_secret": "s"}),
            ),
            (
                "nestpay",
                json!({"bank": "ziraat", "mode": "test", "client_id": "100200127"}),
                json!({"store_key": "k"}),
            ),
            (
                "paynet",
                json!({"mode": "test", "publishable_key": "pbk"}),
                json!({"secret_key": "sck"}),
            ),
            (
                "papara",
                json!({"mode": "test"}),
                json!({"api_key": "k", "secret_key": "s"}),
            ),
            (
                "bank_transfer",
                json!({"account_holder": "ACME", "accounts": "Bank | TR33 0006 1005 1978 6457 8413 26 | TRY"}),
                json!({}),
            ),
            ("mock", json!({}), json!({"webhook_secret": "x"})),
        ];
        for (kind, config, secrets) in cases {
            let gw = build(kind, &config, &secrets).unwrap();
            assert_eq!(gw.kind(), kind);
        }
    }

    #[test]
    fn build_errors_never_echo_secrets() {
        let err = match build(
            "stripe",
            &json!({"mode": "test"}),
            &json!({"secret_key": "sk_test_SECRET"}),
        ) {
            Err(e) => e,
            Ok(_) => panic!("missing webhook secret must fail"),
        };
        assert_eq!(
            err,
            IntegrationError::Config("secrets.webhook_secret is required".into())
        );
        let err = match build(
            "stripe",
            &json!({"mode": "live"}),
            &json!({"secret_key": "sk_test_SECRET", "webhook_secret": "whsec_SECRET"}),
        ) {
            Err(e) => e,
            Ok(_) => panic!("mode mismatch must fail"),
        };
        assert!(!err.to_string().contains("SECRET"));
        assert!(matches!(
            build("bitcoin", &json!({}), &json!({})),
            Err(IntegrationError::Config(_))
        ));
        assert!(
            build(
                "paytr",
                &json!({"merchant_id": "12a"}),
                &json!({"merchant_key": "k", "merchant_salt": "s"})
            )
            .is_err()
        );
    }
}
