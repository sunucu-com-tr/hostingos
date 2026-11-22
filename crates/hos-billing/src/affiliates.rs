//! Affiliate programme: settings, activation, referral attribution with
//! self-referral rules, commission accrual on paid invoices, clearance,
//! automatic reversal and payouts.
//!
//! * **Accrual** ([`Billing::accrue_commissions`], called from
//!   [`Billing::after_invoice_paid`]): only for a client that was referred,
//!   only on lines of services (product orders and renewals, plan changes,
//!   setup fees), only in the affiliate's currency, once per invoice line
//!   (unique index). The rate comes from the product override, then the
//!   affiliate override, then the tenant default. Non-recurring rates pay on
//!   a service's first paid invoice only.
//! * **Anti-fraud**: the affiliate's own client never earns; a referral is
//!   flagged (its commissions `held` for staff review) when the referred
//!   client shares a stored payment method with the affiliate, signed up
//!   from the same IP, or uses the same non-free-mail e-mail domain.
//! * **Clearance / reversal** ([`Billing::run_affiliate_maintenance`], daily):
//!   commissions of refunded, charged-back, cancelled or collected invoices
//!   (and pending ones whose service was cancelled) are reversed first, then
//!   pending commissions older than the clearance delay become available.
//!   Commission already paid out is taken back by a negative adjustment.
//! * **Payouts**: the whole cleared balance, at least the minimum; to
//!   account credit or as a manual payout, both approved by staff.

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use hos_core::CoreError;
use hos_core::domain::InvoiceStatus;
use hos_core::ids::{ClientId, TenantId, UserId};
use hos_core::money::{Currency, Money};
use hos_db::enums::{CreditKind, InvoiceItemType};
use hos_db::repo::affiliates::{
    self as arepo, AffiliateId, AffiliateRow, AffiliateStatus, CommissionStatus, NewCommission,
    PayoutId, PayoutMethod, PayoutRow, ReferralRow,
};
use hos_db::repo::invoices::InvoiceRow;
use hos_db::repo::{clients, invoices, payments, platform, services};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgExecutor};

use crate::{Billing, BillingResult};

/// Free-mail providers: a shared domain there says nothing about the people.
const FREE_MAIL: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "hotmail.com",
    "outlook.com",
    "live.com",
    "msn.com",
    "yahoo.com",
    "yahoo.co.uk",
    "ymail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "gmx.com",
    "gmx.net",
    "gmx.de",
    "web.de",
    "yandex.com",
    "yandex.ru",
    "mail.ru",
    "zoho.com",
    "hotmail.com.tr",
    "outlook.com.tr",
    "windowslive.com",
    "mynet.com",
    "superonline.com",
    "ttmail.com",
    "fastmail.com",
    "tutanota.com",
    "hey.com",
];

/// Tenant settings of the programme (`affiliates.*`).
#[derive(Debug, Clone, PartialEq)]
pub struct AffiliateSettings {
    /// Programme open (links count, commissions accrue).
    pub enabled: bool,
    /// Clients may activate their affiliate account themselves.
    pub self_signup: bool,
    /// Days a referral cookie lasts.
    pub cookie_days: i64,
    /// `true`: the last referral link visited wins; `false`: the first.
    pub last_click: bool,
    /// `percent` or `fixed`.
    pub commission_type: String,
    pub commission_value: Decimal,
    /// Pay on every renewal, not only the first invoice of a service.
    pub recurring: bool,
    /// Days before a commission becomes available.
    pub clearance_days: i64,
    /// Smallest payout.
    pub min_payout: Decimal,
    /// Payouts to account credit allowed.
    pub payout_credit: bool,
    /// Manual payouts (bank transfer by staff) allowed.
    pub payout_manual: bool,
}

impl Default for AffiliateSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            self_signup: true,
            cookie_days: 30,
            last_click: false,
            commission_type: "percent".into(),
            commission_value: Decimal::from(10),
            recurring: false,
            clearance_days: 30,
            min_payout: Decimal::from(100),
            payout_credit: true,
            payout_manual: true,
        }
    }
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

impl AffiliateSettings {
    /// From the `affiliates.*` settings map (unknown / invalid values keep defaults).
    pub fn from_map(map: &BTreeMap<String, Value>) -> Self {
        let mut s = Self::default();
        let get = |k: &str| map.get(&format!("affiliates.{k}"));
        if let Some(v) = get("enabled").and_then(Value::as_bool) {
            s.enabled = v;
        }
        if let Some(v) = get("self_signup").and_then(Value::as_bool) {
            s.self_signup = v;
        }
        if let Some(v) = get("cookie_days")
            .and_then(dec)
            .and_then(|d| d.trunc().to_i64())
        {
            s.cookie_days = v.clamp(1, 365);
        }
        if let Some(v) = get("attribution").and_then(Value::as_str) {
            s.last_click = v == "last";
        }
        if let Some(v) = get("commission_type").and_then(Value::as_str)
            && matches!(v, "percent" | "fixed")
        {
            s.commission_type = v.to_owned();
        }
        if let Some(v) = get("commission_value").and_then(dec)
            && v >= Decimal::ZERO
            && (s.commission_type != "percent" || v <= Decimal::ONE_HUNDRED)
        {
            s.commission_value = v;
        }
        if let Some(v) = get("recurring").and_then(Value::as_bool) {
            s.recurring = v;
        }
        if let Some(v) = get("clearance_days")
            .and_then(dec)
            .and_then(|d| d.trunc().to_i64())
        {
            s.clearance_days = v.clamp(0, 365);
        }
        if let Some(v) = get("min_payout").and_then(dec)
            && v >= Decimal::ZERO
        {
            s.min_payout = v;
        }
        if let Some(v) = get("payout_credit").and_then(Value::as_bool) {
            s.payout_credit = v;
        }
        if let Some(v) = get("payout_manual").and_then(Value::as_bool) {
            s.payout_manual = v;
        }
        s
    }

    /// Loads the settings of a tenant.
    pub async fn load(db: impl PgExecutor<'_>, tenant_id: TenantId) -> BillingResult<Self> {
        Ok(Self::from_map(
            &platform::get_settings(db, tenant_id, "affiliates.").await?,
        ))
    }

    /// The settings as `(key, value)` pairs for `tenant_settings`.
    pub fn to_pairs(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("affiliates.enabled", json!(self.enabled)),
            ("affiliates.self_signup", json!(self.self_signup)),
            ("affiliates.cookie_days", json!(self.cookie_days)),
            (
                "affiliates.attribution",
                json!(if self.last_click { "last" } else { "first" }),
            ),
            ("affiliates.commission_type", json!(self.commission_type)),
            (
                "affiliates.commission_value",
                json!(self.commission_value.normalize().to_string()),
            ),
            ("affiliates.recurring", json!(self.recurring)),
            ("affiliates.clearance_days", json!(self.clearance_days)),
            (
                "affiliates.min_payout",
                json!(self.min_payout.normalize().to_string()),
            ),
            ("affiliates.payout_credit", json!(self.payout_credit)),
            ("affiliates.payout_manual", json!(self.payout_manual)),
        ]
    }
}

/// A commission rule: `(type, value, recurring)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rate {
    pub kind: String,
    pub value: Decimal,
    pub recurring: bool,
}

impl Rate {
    /// Commission for `base` (rounded to `currency`), never more than `base`.
    pub fn commission(&self, base: Decimal, currency: Currency) -> Decimal {
        let raw = match self.kind.as_str() {
            "percent" => base * self.value / Decimal::ONE_HUNDRED,
            "fixed" => self.value.min(base),
            _ => Decimal::ZERO,
        };
        Money::new(raw.max(Decimal::ZERO), currency).round().amount
    }
}

/// A random referral code (10 characters, no look-alikes).
pub fn new_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    hos_core::crypto::random_bytes::<10>()
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
        .collect()
}

/// Whether a referral code has the shape of one (before any lookup).
pub fn plausible_code(code: &str) -> bool {
    (6..=32).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn email_domain(email: &str) -> Option<String> {
    email
        .rsplit_once('@')
        .map(|(_, d)| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
}

impl Billing {
    /// Activates the affiliate account of a client (idempotent: returns the
    /// existing one).
    pub async fn activate_affiliate(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
    ) -> BillingResult<AffiliateRow> {
        if let Some(a) = arepo::affiliate_for_client(self.pool(), tenant_id, client_id).await? {
            return Ok(a);
        }
        let client = clients::get_client(self.pool(), tenant_id, client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", client_id))?;
        if client.status != hos_db::enums::ClientStatus::Active {
            return Err(CoreError::Forbidden(
                "the client account is not active".into(),
            ));
        }
        // A collision of a 50-bit random code is unlikely; retry a few times.
        for _ in 0..5 {
            match arepo::create_affiliate(
                self.pool(),
                tenant_id,
                client_id,
                &new_code(),
                client.currency,
            )
            .await
            {
                Ok(a) => return Ok(a),
                Err(CoreError::Conflict(_)) => {
                    if let Some(a) =
                        arepo::affiliate_for_client(self.pool(), tenant_id, client_id).await?
                    {
                        return Ok(a);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(CoreError::Conflict(
            "could not allocate a referral code".into(),
        ))
    }

    /// Why a referral of `client_id` by `affiliate` looks like self-referral
    /// (`None`: clean). The same client is refused outright by the caller.
    pub async fn referral_flag(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        affiliate: &AffiliateRow,
        client_id: ClientId,
    ) -> BillingResult<Option<&'static str>> {
        let (Some(owner), Some(referred)) = (
            clients::get_client(&mut *conn, tenant_id, affiliate.client_id).await?,
            clients::get_client(&mut *conn, tenant_id, client_id).await?,
        ) else {
            return Ok(None);
        };
        if arepo::shares_payment_method(&mut *conn, tenant_id, owner.id, referred.id).await? {
            return Ok(Some("same_payment_method"));
        }
        if owner.signup_ip.is_some() && owner.signup_ip == referred.signup_ip {
            return Ok(Some("same_signup_ip"));
        }
        if let (Some(a), Some(b)) = (email_domain(&owner.email), email_domain(&referred.email))
            && a == b
            && !FREE_MAIL.contains(&a.as_str())
        {
            return Ok(Some("same_email_domain"));
        }
        Ok(None)
    }

    /// Attributes a new client to the affiliate with `code` (from the referral
    /// cookie). Does nothing when the programme is off, the code is unknown /
    /// suspended, it is the client's own code, the click is older than the
    /// cookie window or the client already has a referrer.
    pub async fn attribute_referral(
        &self,
        tenant_id: TenantId,
        client_id: ClientId,
        code: &str,
        clicked_at: Option<chrono::DateTime<Utc>>,
    ) -> BillingResult<Option<ReferralRow>> {
        if !plausible_code(code) {
            return Ok(None);
        }
        let mut conn = self.pool().acquire().await?;
        let settings = AffiliateSettings::load(&mut *conn, tenant_id).await?;
        if !settings.enabled {
            return Ok(None);
        }
        let now = self.clock().now();
        if clicked_at.is_some_and(|t| t > now || now - t > Duration::days(settings.cookie_days)) {
            return Ok(None);
        }
        let Some(affiliate) = arepo::active_affiliate_by_code(&mut *conn, tenant_id, code).await?
        else {
            return Ok(None);
        };
        if affiliate.client_id == client_id {
            return Ok(None);
        }
        let flag = self
            .referral_flag(&mut conn, tenant_id, &affiliate, client_id)
            .await?;
        let row = arepo::create_referral(
            &mut *conn,
            tenant_id,
            affiliate.id,
            client_id,
            "cookie",
            clicked_at,
            flag,
        )
        .await?;
        Ok(row)
    }

    /// The rate for a service line of `affiliate`.
    async fn rate_for(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        affiliate: &AffiliateRow,
        product_id: Option<hos_core::ids::ProductId>,
        settings: &AffiliateSettings,
    ) -> BillingResult<Rate> {
        if let Some(pid) = product_id
            && let Some((kind, value, recurring)) =
                arepo::product_rate(&mut *conn, tenant_id, pid).await?
        {
            return Ok(Rate {
                kind,
                value,
                recurring,
            });
        }
        let recurring = affiliate.commission_recurring.unwrap_or(settings.recurring);
        Ok(
            match (&affiliate.commission_type, affiliate.commission_value) {
                (Some(kind), Some(value)) => Rate {
                    kind: kind.clone(),
                    value,
                    recurring,
                },
                _ => Rate {
                    kind: settings.commission_type.clone(),
                    value: settings.commission_value,
                    recurring,
                },
            },
        )
    }

    /// Accrues the commissions of a paid invoice (idempotent). Called inside
    /// the payment transaction by [`Billing::after_invoice_paid`].
    pub async fn accrue_commissions(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
    ) -> BillingResult<usize> {
        if invoice.status != InvoiceStatus::Paid {
            return Ok(0);
        }
        let Some(referral) =
            arepo::referral_for_client(&mut *conn, tenant_id, invoice.client_id).await?
        else {
            return Ok(0);
        };
        let settings = AffiliateSettings::load(&mut *conn, tenant_id).await?;
        if !settings.enabled {
            return Ok(0);
        }
        let Some(affiliate) =
            arepo::get_affiliate(&mut *conn, tenant_id, referral.affiliate_id).await?
        else {
            return Ok(0);
        };
        if affiliate.status != AffiliateStatus::Active
            || affiliate.client_id == invoice.client_id
            || affiliate.currency != invoice.currency
        {
            return Ok(0);
        }
        // Re-check the self-referral signals: a card stored after sign-up counts.
        let mut flag = referral.flag.clone();
        if flag.is_none()
            && let Some(f) = self
                .referral_flag(conn, tenant_id, &affiliate, invoice.client_id)
                .await?
        {
            arepo::set_referral_flag(&mut *conn, tenant_id, referral.id, Some(f)).await?;
            flag = Some(f.to_owned());
        }
        let order =
            hos_db::repo::billing_support::order_for_invoice(&mut *conn, tenant_id, invoice.id)
                .await?;
        let items = invoices::invoice_items(&mut *conn, tenant_id, invoice.id).await?;
        // Net amount per service (discount lines of the service included).
        let mut per_service: BTreeMap<hos_core::ids::ServiceId, (Decimal, Option<uuid::Uuid>)> =
            BTreeMap::new();
        for i in &items {
            let Some(sid) = i.service_id else { continue };
            let eligible = matches!(
                i.item_type,
                InvoiceItemType::Service
                    | InvoiceItemType::SetupFee
                    | InvoiceItemType::Upgrade
                    | InvoiceItemType::Addon
                    | InvoiceItemType::Discount
            );
            if !eligible {
                continue;
            }
            let e = per_service.entry(sid).or_insert((Decimal::ZERO, None));
            e.0 += i.amount;
            if e.1.is_none() && i.amount > Decimal::ZERO {
                e.1 = Some(*i.id.as_uuid());
            }
        }
        let now = self.clock().now();
        let mut created = 0;
        for (service_id, (base, key)) in per_service {
            let (Some(key), true) = (key, base > Decimal::ZERO) else {
                continue;
            };
            let Some(svc) = services::get_service(&mut *conn, tenant_id, service_id).await? else {
                continue;
            };
            if svc.client_id != invoice.client_id {
                continue;
            }
            let rate = self
                .rate_for(conn, tenant_id, &affiliate, Some(svc.product_id), &settings)
                .await?;
            let first = order.as_ref().is_some_and(|o| svc.order_id == Some(o.id))
                && !arepo::service_has_commission(&mut *conn, tenant_id, service_id).await?;
            if !first && !rate.recurring {
                continue;
            }
            let amount = rate.commission(base, invoice.currency);
            if amount <= Decimal::ZERO {
                continue;
            }
            let row = arepo::insert_commission(
                &mut *conn,
                tenant_id,
                &NewCommission {
                    affiliate_id: affiliate.id,
                    referral_id: referral.id,
                    client_id: invoice.client_id,
                    invoice_id: invoice.id,
                    invoice_item_id: key,
                    service_id: Some(service_id),
                    kind: if first { "one_time" } else { "recurring" },
                    base_amount: base,
                    amount,
                    currency: invoice.currency,
                    status: if flag.is_some() {
                        CommissionStatus::Held
                    } else {
                        CommissionStatus::Pending
                    },
                    clears_at: now + Duration::days(settings.clearance_days),
                },
            )
            .await?;
            created += usize::from(row.is_some());
        }
        Ok(created)
    }

    /// [`Billing::accrue_commissions`] in its own savepoint: a failure is
    /// logged and never blocks the payment that triggered it.
    pub(crate) async fn accrue_commissions_isolated(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        invoice: &InvoiceRow,
    ) -> BillingResult<()> {
        let mut sp = sqlx::Connection::begin(&mut *conn).await?;
        match self.accrue_commissions(&mut sp, tenant_id, invoice).await {
            Ok(_) => sp.commit().await?,
            Err(e) => {
                tracing::warn!(%tenant_id, invoice_id = %invoice.id, error = %e, "affiliate commission accrual failed");
                sp.rollback().await?;
            }
        }
        Ok(())
    }

    /// Daily: reverses commissions of refunded / charged-back / cancelled
    /// invoices and cancelled services, then clears the due ones. Returns
    /// `(reversed, cleared)`.
    pub async fn run_affiliate_maintenance(
        &self,
        tenant_id: TenantId,
    ) -> BillingResult<(usize, u64)> {
        let now = self.clock().now();
        let mut conn = self.pool().acquire().await?;
        let mut reversed = 0;
        for (c, reason) in arepo::commissions_to_reverse(&mut *conn, tenant_id, 500).await? {
            if arepo::reverse_commission(&mut conn, tenant_id, c.id, &reason, now).await? {
                reversed += 1;
            }
        }
        let cleared = arepo::clear_due(&mut *conn, tenant_id, now).await?;
        Ok((reversed, cleared))
    }

    /// The affiliate requests a payout of the cleared balance.
    pub async fn request_affiliate_payout(
        &self,
        tenant_id: TenantId,
        affiliate_id: AffiliateId,
        method: PayoutMethod,
        details: Option<&str>,
    ) -> BillingResult<PayoutRow> {
        let mut conn = self.pool().acquire().await?;
        let settings = AffiliateSettings::load(&mut *conn, tenant_id).await?;
        let allowed = match method {
            PayoutMethod::Credit => settings.payout_credit,
            PayoutMethod::Manual => settings.payout_manual,
        };
        if !allowed {
            return Err(CoreError::validation("this payout method is not offered"));
        }
        let details = details
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| d.chars().take(500).collect::<String>());
        if method == PayoutMethod::Manual && details.is_none() {
            return Err(CoreError::validation(
                "payout details are required for a manual payout",
            ));
        }
        let payout = arepo::request_payout(
            &mut conn,
            tenant_id,
            affiliate_id,
            method,
            details.as_deref(),
            settings.min_payout,
        )
        .await?;
        self.notify_staff(
            &mut conn,
            tenant_id,
            "affiliate.payout_requested",
            &format!(
                "Affiliate payout requested ({} {})",
                payout.amount.normalize(),
                payout.currency
            ),
            None,
            Some("/admin/affiliates/payouts"),
            &json!({"payout_id": payout.id, "affiliate_id": affiliate_id}),
        )
        .await?;
        Ok(payout)
    }

    /// Staff approves a payout: marks it paid exactly once and, for a credit
    /// payout, adds the amount to the affiliate's account credit in the same
    /// transaction.
    pub async fn approve_affiliate_payout(
        &self,
        tenant_id: TenantId,
        payout_id: PayoutId,
        staff: Option<UserId>,
        note: Option<&str>,
    ) -> BillingResult<PayoutRow> {
        let mut tx = self.pool().begin().await?;
        let payout = arepo::mark_payout_paid(
            &mut *tx,
            tenant_id,
            payout_id,
            staff,
            note,
            self.clock().now(),
        )
        .await?
        .ok_or_else(|| CoreError::Conflict("the payout is not awaiting approval".into()))?;
        let affiliate = arepo::get_affiliate(&mut *tx, tenant_id, payout.affiliate_id)
            .await?
            .ok_or_else(|| CoreError::not_found("affiliate", payout.affiliate_id))?;
        if payout.method == PayoutMethod::Credit {
            let credit = payments::adjust_credit(
                &mut tx,
                tenant_id,
                affiliate.client_id,
                payout.amount,
                payout.currency,
                CreditKind::Affiliate,
                "Affiliate commission payout",
                staff,
            )
            .await?;
            arepo::set_payout_credit(&mut *tx, tenant_id, payout.id, *credit.id.as_uuid()).await?;
        }
        self.notify_client(
            &mut tx,
            tenant_id,
            affiliate.client_id,
            "affiliate.payout_paid",
            &format!(
                "Affiliate payout of {} {} completed",
                payout.amount.normalize(),
                payout.currency
            ),
            None,
            Some("/client/affiliate"),
        )
        .await?;
        tx.commit().await?;
        Ok(payout)
    }

    /// Staff rejects a payout: the commissions return to the balance.
    pub async fn reject_affiliate_payout(
        &self,
        tenant_id: TenantId,
        payout_id: PayoutId,
        staff: Option<UserId>,
        note: Option<&str>,
    ) -> BillingResult<PayoutRow> {
        let mut conn = self.pool().acquire().await?;
        arepo::reject_payout(
            &mut conn,
            tenant_id,
            payout_id,
            staff,
            note,
            self.clock().now(),
        )
        .await?
        .ok_or_else(|| CoreError::Conflict("the payout is not awaiting approval".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn rates_round_and_cap() {
        let pct = Rate {
            kind: "percent".into(),
            value: dec!(12.5),
            recurring: false,
        };
        assert_eq!(pct.commission(dec!(99.99), Currency::TRY), dec!(12.50));
        let fixed = Rate {
            kind: "fixed".into(),
            value: dec!(50),
            recurring: true,
        };
        assert_eq!(
            fixed.commission(dec!(20), Currency::TRY),
            dec!(20),
            "never more than the base"
        );
        assert_eq!(fixed.commission(dec!(80), Currency::TRY), dec!(50));
        let none = Rate {
            kind: "none".into(),
            value: dec!(10),
            recurring: true,
        };
        assert_eq!(none.commission(dec!(80), Currency::TRY), dec!(0));
    }

    #[test]
    fn settings_parse_with_bounds() {
        let mut m = BTreeMap::new();
        m.insert("affiliates.enabled".into(), json!(true));
        m.insert("affiliates.commission_value".into(), json!("150"));
        m.insert("affiliates.cookie_days".into(), json!(9999));
        m.insert("affiliates.attribution".into(), json!("last"));
        let s = AffiliateSettings::from_map(&m);
        assert!(s.enabled && s.last_click);
        assert_eq!(
            s.commission_value,
            dec!(10),
            "a percent above 100 is ignored"
        );
        assert_eq!(s.cookie_days, 365);
        let back: BTreeMap<String, Value> = s
            .to_pairs()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        assert_eq!(AffiliateSettings::from_map(&back), s);
    }

    #[test]
    fn codes() {
        let c = new_code();
        assert_eq!(c.len(), 10);
        assert!(plausible_code(&c));
        assert!(!plausible_code("ab"));
        assert!(!plausible_code("abc-def-ghi"));
        assert_eq!(
            email_domain("a@Example.COM").as_deref(),
            Some("example.com")
        );
    }
}
