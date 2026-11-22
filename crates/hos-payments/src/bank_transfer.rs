//! Bank transfer / EFT (offline gateway).
//!
//! `create_checkout` returns [`CheckoutSession::Offline`] with Markdown
//! instructions: amount, account holder, the IBANs for the invoice currency and
//! a per-invoice reference code to write in the transfer description. There is
//! no webhook and no API refund; staff match incoming transfers by the code and
//! confirm the payment manually in billing.
//!
//! The reference code is deterministic per invoice (`HOS` + 10 Crockford base32
//! characters derived from the invoice id + 2 ISO 7064 MOD 97-10 check digits,
//! e.g. `HOS7K3M9QX2PA25`), so every attempt for the same invoice shows the same
//! code and typos are detected by [`verify_reference_code`].

use async_trait::async_trait;
use hos_core::crypto;
use hos_core::domain::{IntegrationCategory, Locale};
use hos_core::error::IntegrationError;
use hos_core::ids::InvoiceId;
use hos_core::integrations::gateway::{
    CheckoutRequest, CheckoutSession, GatewayCapabilities, PaymentGateway, RefundRequest,
    RefundResult, WebhookOutcome, WebhookRequest,
};
use hos_core::integrations::{
    ConnectionInfo, FieldReader, FieldSpec, IntegrationDescriptor, kinds,
};
use hos_core::money::Currency;
use serde_json::Value;

use crate::common::ensure_positive;

/// Default reference prefix.
pub const DEFAULT_PREFIX: &str = "HOS";
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_BODY_LEN: usize = 10;

/// One receiving account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankAccount {
    /// Bank name.
    pub bank: String,
    /// IBAN, normalised (upper-case, no spaces).
    pub iban: String,
    /// Account currency; `None` = shown for every currency.
    pub currency: Option<Currency>,
    /// SWIFT/BIC (for foreign transfers).
    pub bic: Option<String>,
}

/// Bank transfer settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankTransferSettings {
    /// Account holder (company legal name).
    pub account_holder: String,
    /// Receiving accounts.
    pub accounts: Vec<BankAccount>,
    /// Reference code prefix (1–6 alphanumerics).
    pub reference_prefix: String,
    /// Extra note appended to the instructions.
    pub note: Option<String>,
}

/// ISO 7064 MOD 97 remainder of an alphanumeric string (`A`=10 … `Z`=35), as
/// used by IBAN and ISO 11649 creditor references. `None` for other characters.
fn mod97(s: &str) -> Option<u32> {
    let mut rem: u32 = 0;
    for c in s.chars() {
        let v = c.to_digit(36)?;
        rem = if v < 10 {
            (rem * 10 + v) % 97
        } else {
            (rem * 100 + v) % 97
        };
    }
    Some(rem)
}

/// Validates an IBAN (structure + MOD 97 check digits). Spaces are ignored.
pub fn validate_iban(iban: &str) -> bool {
    let iban: String = iban
        .split_whitespace()
        .collect::<String>()
        .to_ascii_uppercase();
    let bytes = iban.as_bytes();
    if !(15..=34).contains(&bytes.len())
        || !bytes[..2].iter().all(u8::is_ascii_uppercase)
        || !bytes[2..4].iter().all(u8::is_ascii_digit)
        || !bytes.iter().all(u8::is_ascii_alphanumeric)
    {
        return false;
    }
    let rearranged = format!("{}{}", &iban[4..], &iban[..4]);
    mod97(&rearranged) == Some(1)
}

/// `TR33 0006 1005 …` (groups of four).
pub fn format_iban(iban: &str) -> String {
    let compact: Vec<char> = iban.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

fn check_digits(body: &str) -> Option<u32> {
    mod97(&format!("{body}00")).map(|r| 98 - r)
}

/// The transfer reference for an invoice: `prefix` + 10 base32 chars + 2 check digits.
pub fn reference_code(prefix: &str, invoice_id: InvoiceId) -> String {
    // Pre-rename "hf" label, kept on purpose: codes already issued derive from it.
    let mut seed = b"hf-payments/bank-transfer/v1:".to_vec();
    seed.extend_from_slice(invoice_id.as_uuid().as_bytes());
    let digest = crypto::sha256(&seed);
    let mut bits = u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ]);
    let mut body = String::with_capacity(prefix.len() + CODE_BODY_LEN + 2);
    body.push_str(&prefix.to_ascii_uppercase());
    for _ in 0..CODE_BODY_LEN {
        body.push(char::from(CROCKFORD[(bits >> 59) as usize]));
        bits <<= 5;
    }
    let check = check_digits(&body).unwrap_or(0);
    format!("{body}{check:02}")
}

/// Checks a reference code typed by the customer / bank (spaces, dashes and case
/// are ignored).
pub fn verify_reference_code(code: &str) -> bool {
    let normalized: String = code
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_uppercase();
    normalized.len() > 2 && mod97(&normalized) == Some(1)
}

/// The reference code an integration with `config` shows for `invoice_id`
/// (for billing / staff matching without building the gateway).
pub fn reference_code_for(
    config: &Value,
    invoice_id: InvoiceId,
) -> Result<String, IntegrationError> {
    let prefix = parse_prefix(&FieldReader::config(config))?;
    Ok(reference_code(&prefix, invoice_id))
}

fn parse_prefix(c: &FieldReader<'_>) -> Result<String, IntegrationError> {
    let prefix = c
        .string_or("reference_prefix", DEFAULT_PREFIX)?
        .to_ascii_uppercase();
    if prefix.is_empty() || prefix.len() > 6 || !prefix.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(IntegrationError::Config(
            "config.reference_prefix must be 1–6 letters/digits".into(),
        ));
    }
    Ok(prefix)
}

/// Parses the `accounts` text area: one account per line,
/// `Bank name | IBAN | currency (optional) | BIC (optional)` (`;` also works).
pub fn parse_accounts(text: &str) -> Result<Vec<BankAccount>, IntegrationError> {
    let mut accounts = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split(['|', ';']).map(str::trim).collect();
        let invalid = |what: &str| {
            IntegrationError::Config(format!("config.accounts line {}: {what}", n + 1))
        };
        let (bank, iban) = match parts.as_slice() {
            [bank, iban, ..] if !bank.is_empty() => (*bank, *iban),
            _ => return Err(invalid("expected `Bank | IBAN | currency | BIC`")),
        };
        let iban: String = iban
            .split_whitespace()
            .collect::<String>()
            .to_ascii_uppercase();
        if !validate_iban(&iban) {
            return Err(invalid("invalid IBAN"));
        }
        let currency = match parts.get(2).copied().filter(|c| !c.is_empty()) {
            Some(c) => Some(Currency::new(c).map_err(|_| invalid("invalid currency"))?),
            None => None,
        };
        let bic = parts
            .get(3)
            .copied()
            .filter(|b| !b.is_empty())
            .map(str::to_ascii_uppercase);
        accounts.push(BankAccount {
            bank: bank.to_owned(),
            iban,
            currency,
            bic,
        });
    }
    if accounts.is_empty() {
        return Err(IntegrationError::Config(
            "config.accounts must list at least one bank account".into(),
        ));
    }
    Ok(accounts)
}

impl BankTransferSettings {
    /// Reads settings from integration JSON (no secrets).
    pub fn from_values(config: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        Ok(Self {
            account_holder: c.str("account_holder")?.to_owned(),
            accounts: parse_accounts(c.str("accounts")?)?,
            reference_prefix: parse_prefix(&c)?,
            note: c.opt_str("note")?.map(str::to_owned),
        })
    }
}

/// Admin form descriptor.
pub fn descriptor() -> IntegrationDescriptor {
    IntegrationDescriptor::new(
        kinds::BANK_TRANSFER,
        "Bank transfer / EFT",
        IntegrationCategory::Gateway,
    )
    .description(
        "Offline payment: the customer sees your IBANs and a per-invoice reference code; staff \
         confirm received transfers manually. No webhook, no API refunds. Currencies: those of \
         the configured accounts (accounts without a currency are shown for every currency).",
    )
    .config_field(FieldSpec::text("account_holder", "Account holder (legal name)").required())
    .config_field(
        FieldSpec::text_area("accounts", "Bank accounts")
            .required()
            .placeholder("Garanti BBVA | TR33 0006 1005 1978 6457 8413 26 | TRY")
            .help("One per line: Bank | IBAN | currency (optional) | BIC (optional)."),
    )
    .config_field(
        FieldSpec::text("reference_prefix", "Reference code prefix")
            .default_value(DEFAULT_PREFIX)
            .help("1–6 letters/digits, e.g. HOS → HOS7K3M9QX2PA25."),
    )
    .config_field(FieldSpec::text_area(
        "note",
        "Extra note shown to the customer",
    ))
}

/// Static capabilities (currencies depend on the configured accounts).
pub fn capabilities() -> GatewayCapabilities {
    GatewayCapabilities {
        offline: true,
        ..Default::default()
    }
}

/// The offline bank transfer gateway.
#[derive(Debug, Clone)]
pub struct BankTransferGateway {
    settings: BankTransferSettings,
}

fn escape_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '|' | '[' | ']' | '<' | '>' | '#'
        ) {
            out.push('\\');
        }
        out.push(if c == '\n' { ' ' } else { c });
    }
    out
}

impl BankTransferGateway {
    /// Creates the gateway.
    pub fn new(settings: BankTransferSettings) -> Self {
        Self { settings }
    }

    /// The reference code for an invoice.
    pub fn reference_code(&self, invoice_id: InvoiceId) -> String {
        reference_code(&self.settings.reference_prefix, invoice_id)
    }

    fn instructions(&self, request: &CheckoutRequest, accounts: &[&BankAccount]) -> String {
        let en = request.locale == Some(Locale::En);
        let code = self.reference_code(request.invoice_id);
        let amount = format!(
            "{} {}",
            request.amount.to_decimal_string(),
            request.amount.currency
        );
        let holder = escape_md(&self.settings.account_holder);
        let (title, l_amount, l_holder, l_ref, h_bank, h_cur, footer) = if en {
            (
                "Pay by bank transfer",
                "Amount",
                "Account holder",
                "Payment reference (required)",
                "Bank",
                "Currency",
                format!(
                    "Write only **{code}** in the transfer description. Your invoice is marked \
                     paid once our staff have matched and confirmed the transfer."
                ),
            )
        } else {
            (
                "Havale / EFT ile ödeme",
                "Tutar",
                "Alıcı",
                "Açıklama (zorunlu)",
                "Banka",
                "Para birimi",
                format!(
                    "Açıklama alanına yalnızca **{code}** yazın. Ödemeniz personelimiz tarafından \
                     eşleştirilip onaylandıktan sonra faturanız ödendi olarak işaretlenir."
                ),
            )
        };
        let mut md = format!(
            "**{title}**\n\n- {l_amount}: **{amount}**\n- {l_holder}: **{holder}**\n- {l_ref}: **{code}**\n\n| {h_bank} | IBAN | {h_cur} | BIC |\n|---|---|---|---|\n"
        );
        for account in accounts {
            md.push_str(&format!(
                "| {} | `{}` | {} | {} |\n",
                escape_md(&account.bank),
                format_iban(&account.iban),
                account
                    .currency
                    .map_or_else(|| request.amount.currency.to_string(), |c| c.to_string()),
                account.bic.as_deref().map(escape_md).unwrap_or_default(),
            ));
        }
        md.push('\n');
        md.push_str(&footer);
        if let Some(note) = self
            .settings
            .note
            .as_deref()
            .filter(|n| !n.trim().is_empty())
        {
            md.push_str("\n\n");
            md.push_str(&escape_md(note.trim()));
        }
        md
    }
}

#[async_trait]
impl PaymentGateway for BankTransferGateway {
    fn kind(&self) -> &'static str {
        kinds::BANK_TRANSFER
    }

    fn capabilities(&self) -> GatewayCapabilities {
        let mut currencies: Vec<Currency> = Vec::new();
        for account in &self.settings.accounts {
            match account.currency {
                Some(c) if !currencies.contains(&c) => currencies.push(c),
                Some(_) => {}
                // An account without a currency accepts anything.
                None => return capabilities(),
            }
        }
        GatewayCapabilities {
            currencies,
            ..capabilities()
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        Ok(ConnectionInfo {
            server_version: None,
            details: Some(format!(
                "{} bank account(s) configured; offline gateway (no remote API)",
                self.settings.accounts.len()
            )),
        })
    }

    async fn create_checkout(
        &self,
        request: &CheckoutRequest,
    ) -> Result<CheckoutSession, IntegrationError> {
        ensure_positive(&request.amount)?;
        let currency = request.amount.currency;
        let accounts: Vec<&BankAccount> = self
            .settings
            .accounts
            .iter()
            .filter(|a| a.currency.is_none_or(|c| c == currency))
            .collect();
        if accounts.is_empty() {
            return Err(IntegrationError::InvalidInput(format!(
                "no bank account is configured for {currency}"
            )));
        }
        Ok(CheckoutSession::Offline {
            instructions: self.instructions(request, &accounts),
        })
    }

    async fn parse_webhook(
        &self,
        _request: &WebhookRequest,
    ) -> Result<WebhookOutcome, IntegrationError> {
        Err(IntegrationError::Unsupported("parse_webhook"))
    }

    async fn refund(&self, _request: &RefundRequest) -> Result<RefundResult, IntegrationError> {
        Err(IntegrationError::Unsupported("refund"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::integrations::gateway::PaymentCustomer;
    use hos_core::money::Money;
    use rust_decimal_macros::dec;
    use serde_json::json;
    use std::collections::BTreeMap;

    const TR_IBAN: &str = "TR33 0006 1005 1978 6457 8413 26";

    fn settings() -> BankTransferSettings {
        BankTransferSettings::from_values(&json!({
            "account_holder": "ACME Hosting Ltd. Şti.",
            "accounts": format!("# main\nGaranti BBVA | {TR_IBAN} | TRY\nWise | GB82 WEST 1234 5698 7654 32 | EUR | WESTGB2L\n"),
            "reference_prefix": "hos",
            "note": "Thanks!"
        }))
        .unwrap()
    }

    fn request(currency: Currency, locale: Option<Locale>) -> CheckoutRequest {
        CheckoutRequest {
            invoice_id: InvoiceId::new(),
            reference: "INV42A1".into(),
            amount: Money::new(dec!(1250), currency),
            description: "Fatura".into(),
            customer: PaymentCustomer::default(),
            items: vec![],
            return_url: "https://shop.example/ok".into(),
            cancel_url: "https://shop.example/fail".into(),
            webhook_url: "https://shop.example/webhooks/gateway/abc".into(),
            locale,
            save_payment_method: false,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn iban_validation() {
        assert!(validate_iban(TR_IBAN));
        assert!(validate_iban("GB82WEST12345698765432"));
        assert!(validate_iban("DE89 3704 0044 0532 0130 00"));
        assert!(!validate_iban("TR34 0006 1005 1978 6457 8413 26"));
        assert!(!validate_iban("GB82WEST1234569876543"));
        assert!(!validate_iban("XX"));
        assert_eq!(format_iban("TR330006100519786457841326"), TR_IBAN);
    }

    #[test]
    fn reference_codes_are_stable_and_checked() {
        let id = InvoiceId::new();
        let code = reference_code("HOS", id);
        assert_eq!(code, reference_code("HOS", id));
        assert_ne!(code, reference_code("HOS", InvoiceId::new()));
        assert_eq!(code.len(), 3 + CODE_BODY_LEN + 2);
        assert!(code.starts_with("HOS"));
        assert!(verify_reference_code(&code));
        assert!(verify_reference_code(&format!(
            " {}-{} ",
            &code[..6],
            code[6..].to_lowercase()
        )));
        // Any single-character change or adjacent swap is detected.
        let mut chars: Vec<char> = code.chars().collect();
        chars[5] = if chars[5] == 'A' { 'B' } else { 'A' };
        assert!(!verify_reference_code(&chars.iter().collect::<String>()));
        let mut swapped: Vec<char> = code.chars().collect();
        if swapped[3] != swapped[4] {
            swapped.swap(3, 4);
            assert!(!verify_reference_code(&swapped.iter().collect::<String>()));
        }
        assert!(!verify_reference_code("HO"));
        assert_eq!(
            reference_code_for(&json!({"reference_prefix": "acme"}), id).unwrap(),
            reference_code("ACME", id)
        );
        assert!(reference_code_for(&json!({"reference_prefix": "TOOLONG1"}), id).is_err());
    }

    #[tokio::test]
    async fn offline_instructions_per_currency_and_locale() {
        let gw = BankTransferGateway::new(settings());
        let req = request(Currency::TRY, Some(Locale::Tr));
        let CheckoutSession::Offline { instructions } = gw.create_checkout(&req).await.unwrap()
        else {
            panic!("expected offline session");
        };
        assert!(instructions.contains("Havale / EFT"));
        assert!(instructions.contains("1250.00 TRY"));
        assert!(instructions.contains(TR_IBAN));
        assert!(!instructions.contains("GB82"));
        assert!(instructions.contains(&gw.reference_code(req.invoice_id)));
        assert!(instructions.contains("ACME Hosting Ltd. Şti."));
        assert!(instructions.ends_with("Thanks!"));

        let en = request(Currency::EUR, Some(Locale::En));
        let CheckoutSession::Offline { instructions } = gw.create_checkout(&en).await.unwrap()
        else {
            panic!("expected offline session");
        };
        assert!(instructions.contains("Pay by bank transfer"));
        assert!(instructions.contains("GB82 WEST 1234 5698 7654 32"));
        assert!(instructions.contains("WESTGB2L"));

        assert!(matches!(
            gw.create_checkout(&request(Currency::USD, None)).await,
            Err(IntegrationError::InvalidInput(_))
        ));
        assert_eq!(
            gw.capabilities().currencies,
            vec![Currency::TRY, Currency::EUR]
        );
        assert!(gw.capabilities().offline);
        assert!(matches!(
            gw.parse_webhook(&WebhookRequest::default()).await,
            Err(IntegrationError::Unsupported(_))
        ));
    }

    #[test]
    fn account_parsing_errors() {
        assert!(parse_accounts("Bank | TR34 0006 1005 1978 6457 8413 26").is_err());
        assert!(parse_accounts("# only comments").is_err());
        assert!(parse_accounts("just-one-field").is_err());
        assert!(parse_accounts(&format!("Bank | {TR_IBAN} | TL")).is_err());
        let any = parse_accounts(&format!("Bank; {TR_IBAN}")).unwrap();
        assert_eq!(any[0].currency, None);
        assert_eq!(
            BankTransferGateway::new(BankTransferSettings {
                accounts: any,
                ..settings()
            })
            .capabilities()
            .currencies,
            Vec::<Currency>::new()
        );
    }
}
