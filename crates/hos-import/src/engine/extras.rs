//! Handlers of the WHMCS data that maps onto the newer HostingOS features:
//! quotes, the affiliate programme (accounts, referrals, commission
//! history, withdrawals and balances), the staff to-do list, the ban lists,
//! the download library and network issues. Only the database connector
//! reads these tables (the API connector reports them as unsupported).

use chrono::{Duration, Utc};
use hos_db::repo::import_write as w;
use rust_decimal::Decimal;
use sqlx::PgConnection;
use uuid::Uuid;

use super::handlers::clean_filename;
use super::{Engine, Outcome, sid, today};
use crate::entity::{Entity, derived};
use crate::error::ImportResult;
use crate::map;
use crate::row::Row;
use crate::sanitize::{clean_line, clean_text, opt_line};
use crate::source::Source;

const NAME: usize = 120;
const TITLE: usize = 200;
const LINE: usize = 255;
const BODY: usize = 10_000;

fn outcome(inserted: bool, adopted: bool) -> Outcome {
    if adopted {
        Outcome::Adopted
    } else if inserted {
        Outcome::Created
    } else {
        Outcome::Updated
    }
}

fn i32_of(v: Option<i64>) -> i32 {
    v.and_then(|x| i32::try_from(x).ok()).unwrap_or(0)
}

impl Engine<'_> {
    // -----------------------------------------------------------------
    // quotes
    // -----------------------------------------------------------------

    pub(super) async fn h_quote(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::Quotes;
        let s = sid(row);
        let client_id = self.reference(Entity::Clients.key(), row, "userid").await?;
        let prospect_email = if client_id.is_none() {
            row.str("email").and_then(|m| map::email(&m))
        } else {
            None
        };
        if client_id.is_none() && prospect_email.is_none() {
            return Ok(self.skip(
                e,
                row,
                "warning",
                "no_recipient",
                "the quote has neither an imported client nor a valid recipient e-mail",
            ));
        }
        let subject = clean_line(&row.text("subject"), TITLE);
        let subject = if subject.is_empty() {
            format!("Quote #{s}")
        } else {
            subject
        };
        let currency = match client_id {
            Some(c) if !self.dry => self.currency_of_client(conn, c).await?,
            _ => self.currency_of(row.int("currency")),
        };
        let created = row.datetime("datecreated");
        let valid_until = row
            .date("validuntil")
            .unwrap_or_else(|| created.map_or_else(today, |d| d.date_naive()) + Duration::days(30));
        let mut status = map::quote_status(&row.text("stage"));
        if status == "sent" && valid_until < today() {
            status = "expired";
        }
        let name = clean_line(
            &format!("{} {}", row.text("firstname"), row.text("lastname")),
            NAME,
        );
        let notes = [row.text("proposal"), row.text("customernotes")]
            .iter()
            .map(|t| clean_text(t, BODY))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let input = w::QuoteInput {
            client_id,
            prospect_name: client_id
                .is_none()
                .then_some(name)
                .filter(|n| !n.is_empty()),
            prospect_email,
            prospect_company: if client_id.is_none() {
                opt_line(row.str("companyname"), NAME)
            } else {
                None
            },
            subject,
            status: status.into(),
            currency,
            valid_until,
            notes: (!notes.is_empty()).then_some(notes),
            admin_notes: Some(clean_text(&row.text("adminnotes"), BODY)).filter(|t| !t.is_empty()),
            subtotal: row.money("subtotal"),
            tax_total: row.money("tax1") + row.money("tax2"),
            total: row.money("total"),
            sent_at: row.datetime("datesent"),
            accepted_at: (status == "accepted")
                .then(|| row.datetime("dateaccepted").or(created))
                .flatten(),
            declined_at: (status == "declined")
                .then(|| row.datetime("lastmodified").or(created))
                .flatten(),
            created_at: created,
        };
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_quote(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "quotes", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    pub(super) async fn h_quote_item(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::QuoteItems;
        let s = sid(row);
        let Some(quote_id) = self.reference(Entity::Quotes.key(), row, "quoteid").await? else {
            return Ok(self.skip(
                e,
                row,
                "info",
                "orphan",
                "the quote of this line was not imported",
            ));
        };
        let unit_price = row.dec("unitprice").unwrap_or(Decimal::ZERO);
        if unit_price < Decimal::ZERO {
            return Ok(self.skip(
                e,
                row,
                "warning",
                "negative_price",
                "a quote line with a negative price is not imported; add it as a discount by hand",
            ));
        }
        let quantity = row
            .dec("quantity")
            .filter(|q| *q > Decimal::ZERO)
            .unwrap_or(Decimal::ONE)
            .min(Decimal::from(100_000));
        let (discount, amount) = map::quote_line(
            quantity,
            unit_price,
            row.dec("discount").unwrap_or(Decimal::ZERO),
        );
        let description = clean_line(&row.text("description"), 500);
        let input = w::QuoteItemInput {
            quote_id,
            description: if description.is_empty() {
                "—".into()
            } else {
                description
            },
            quantity,
            unit_price,
            discount_percent: discount,
            amount,
            taxable: row.flag("taxable"),
            sort_order: i32_of(row.int("id")),
        };
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_quote_item(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "quote_items", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    // -----------------------------------------------------------------
    // affiliates
    // -----------------------------------------------------------------

    pub(super) async fn h_affiliate(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::Affiliates;
        let s = sid(row);
        let Some(client_id) = self
            .reference(Entity::Clients.key(), row, "clientid")
            .await?
        else {
            return Ok(self.skip(
                e,
                row,
                "info",
                "orphan",
                "the affiliate's client was not imported",
            ));
        };
        let key = e.key();
        let (mut id, existing) = self.target(key, &s).await?;
        let mut adopted = existing.is_some_and(|m| m.adopted);
        if existing.is_none() && !self.dry {
            // The client opened an affiliate account in HostingOS already.
            if let Some(found) = w::affiliate_of_client(conn, self.tenant, client_id).await? {
                id = found;
                adopted = true;
            }
        }
        let commission = map::affiliate_commission(&row.text("paytype"), row.dec("payamount"));
        let currency = if self.dry {
            self.refdata.default_currency.clone()
        } else {
            self.currency_of_client(conn, client_id).await?
        };
        let input = w::AffiliateInput {
            client_id,
            code: map::affiliate_code(id),
            commission_type: commission.map(|(t, _)| t.to_owned()),
            commission_value: commission.map(|(_, v)| v),
            commission_recurring: row.flag("onetime").then_some(false),
            currency,
            created_at: row.datetime("date"),
        };
        let inserted = if self.dry {
            existing.is_none() && !adopted
        } else if adopted {
            // An account opened in HostingOS keeps its own commission terms,
            // code and currency; only the WHMCS id is linked to it.
            false
        } else {
            w::upsert_affiliate(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "affiliates", id, inserted, adopted)
            .await?;
        Ok(outcome(inserted, adopted))
    }

    pub(super) async fn h_affiliate_account(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::AffiliateAccounts;
        let s = sid(row);
        let Some(affiliate_id) = self
            .reference(Entity::Affiliates.key(), row, "affiliateid")
            .await?
        else {
            return Ok(self.skip(e, row, "info", "orphan", "the affiliate was not imported"));
        };
        let Some(client_id) = self
            .reference(Entity::Clients.key(), row, "service_userid")
            .await?
        else {
            return Ok(self.skip(
                e,
                row,
                "info",
                "orphan",
                "the referred service (and its client) was not imported",
            ));
        };
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        if existing.is_none()
            && !self.dry
            && let Some((rid, aff)) = w::referral_of_client(conn, self.tenant, client_id).await?
        {
            if aff != affiliate_id {
                return Ok(self.skip(
                    e,
                    row,
                    "warning",
                    "referred_elsewhere",
                    "the client is already referred by another affiliate",
                ));
            }
            // Another service of the same referred client: one referral.
            self.record(conn, key, &s, "affiliate_referrals", rid, false, true)
                .await?;
            return Ok(Outcome::Adopted);
        }
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_affiliate_referral(conn, self.tenant, id, affiliate_id, client_id, None)
                .await?
        };
        let adopted = existing.is_some_and(|m| m.adopted);
        self.record(conn, key, &s, "affiliate_referrals", id, inserted, adopted)
            .await?;
        Ok(outcome(inserted, adopted))
    }

    /// Whether the row's affiliate (`affiliateid`) is an account that was
    /// opened in HostingOS and adopted: the import never adds WHMCS money
    /// (commissions, payouts, balance) to it.
    async fn native_affiliate(&mut self, row: &Row) -> ImportResult<bool> {
        let Some(id) = row.reference("affiliateid") else {
            return Ok(false);
        };
        Ok(self
            .lookup(Entity::Affiliates.key(), &id.to_string())
            .await?
            .is_some_and(|m| m.adopted))
    }

    async fn affiliate_currency(
        &mut self,
        conn: &mut PgConnection,
        affiliate_id: Uuid,
    ) -> ImportResult<String> {
        if self.dry {
            return Ok(self.refdata.default_currency.clone());
        }
        Ok(w::affiliate_currency(conn, self.tenant, affiliate_id)
            .await?
            .unwrap_or_else(|| self.refdata.default_currency.clone()))
    }

    pub(super) async fn h_affiliate_history(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::AffiliateHistory;
        let s = sid(row);
        let Some(affiliate_id) = self
            .reference(Entity::Affiliates.key(), row, "affiliateid")
            .await?
        else {
            return Ok(self.skip(e, row, "info", "orphan", "the affiliate was not imported"));
        };
        if self.native_affiliate(row).await? {
            return Ok(self.skip(
                e,
                row,
                "info",
                "native_affiliate",
                "the affiliate account was opened in HostingOS; WHMCS commissions are not added to it",
            ));
        }
        let amount = row.money("amount");
        if amount <= Decimal::ZERO {
            return Ok(self.skip(
                e,
                row,
                "info",
                "not_a_commission",
                "zero or negative history lines (reversals) are not imported; the balance is taken from WHMCS",
            ));
        }
        let Some(referral_id) = self
            .reference(Entity::AffiliateAccounts.key(), row, "affaccid")
            .await?
        else {
            return Ok(self.skip(
                e,
                row,
                "info",
                "no_referral",
                "a history line without an imported referral is not imported; the balance is taken from WHMCS",
            ));
        };
        let client_id = if self.dry {
            Some(Uuid::nil())
        } else {
            w::referral_client(conn, self.tenant, referral_id).await?
        };
        let Some(client_id) = client_id else {
            return Ok(self.skip(e, row, "info", "orphan", "the referral was not imported"));
        };
        let input = w::CommissionInput {
            affiliate_id,
            referral_id,
            client_id,
            service_id: self
                .reference(Entity::Services.key(), row, "account_relid")
                .await?,
            amount,
            currency: self.affiliate_currency(conn, affiliate_id).await?,
            created_at: row.datetime("date").unwrap_or_else(Utc::now),
        };
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_commission(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "affiliate_commissions", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    pub(super) async fn h_affiliate_withdrawal(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::AffiliateWithdrawals;
        let s = sid(row);
        let Some(affiliate_id) = self
            .reference(Entity::Affiliates.key(), row, "affiliateid")
            .await?
        else {
            return Ok(self.skip(e, row, "info", "orphan", "the affiliate was not imported"));
        };
        if self.native_affiliate(row).await? {
            return Ok(self.skip(
                e,
                row,
                "info",
                "native_affiliate",
                "the affiliate account was opened in HostingOS; WHMCS payouts are not added to it",
            ));
        }
        let amount = row.money("amount");
        if amount <= Decimal::ZERO {
            return Ok(self.skip(e, row, "info", "empty", "zero withdrawal"));
        }
        let currency = self.affiliate_currency(conn, affiliate_id).await?;
        let paid_at = row.datetime("date").unwrap_or_else(Utc::now);
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_payout(
                conn,
                self.tenant,
                id,
                affiliate_id,
                amount,
                &currency,
                paid_at,
            )
            .await?
        };
        self.record(conn, key, &s, "affiliate_payouts", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    /// After the history: each affiliate's cleared balance becomes its
    /// WHMCS balance — the newest imported commissions are `available` up to
    /// it, and a part the history does not explain is one opening-balance
    /// commission on the affiliate's first referral (reported when there is
    /// none to carry it).
    pub(super) async fn reconcile_affiliates(
        &mut self,
        source: &mut dyn Source,
    ) -> ImportResult<()> {
        if self.dry || self.progress.steps.contains("affiliate_balances") {
            return Ok(());
        }
        let key = Entity::Affiliates.key();
        let mut cursor = 0;
        loop {
            let b = source
                .fetch(Entity::Affiliates, cursor, self.opts.batch())
                .await?;
            let mut tx = self.pool.begin().await?;
            self.hold_lease(&mut tx).await?;
            for r in &b.rows {
                let s = sid(r);
                let Some(m) = self.lookup(key, &s).await? else {
                    continue;
                };
                let balance = r.money("balance").max(Decimal::ZERO);
                if m.adopted {
                    // A HostingOS account keeps its own balance.
                    if balance > Decimal::ZERO {
                        self.note(
                            key,
                            "warning",
                            Some(&s),
                            "native_affiliate_balance",
                            format!(
                                "an affiliate balance of {balance} was not imported: the affiliate account was opened in HostingOS and keeps its own balance; settle the WHMCS balance by hand"
                            ),
                        );
                    }
                    continue;
                }
                let opening = self.lookup(derived::AFFILIATE_OPENING_BALANCE, &s).await?;
                let rest = w::reconcile_affiliate_balance(
                    &mut tx,
                    self.tenant,
                    m.target_id,
                    balance,
                    opening.map(|o| o.target_id),
                )
                .await?;
                if rest <= Decimal::ZERO {
                    continue;
                }
                let Some((referral_id, client_id)) =
                    w::first_referral(&mut tx, self.tenant, m.target_id).await?
                else {
                    self.note(
                        key,
                        "warning",
                        Some(&s),
                        "balance_not_imported",
                        format!(
                            "an affiliate balance of {rest} was not imported: the affiliate has no imported referral to carry it; pay it out or credit it by hand"
                        ),
                    );
                    continue;
                };
                let id = opening.map_or_else(Uuid::now_v7, |o| o.target_id);
                let currency = w::affiliate_currency(&mut tx, self.tenant, m.target_id)
                    .await?
                    .unwrap_or_else(|| self.refdata.default_currency.clone());
                let inserted = w::upsert_opening_balance(
                    &mut tx,
                    self.tenant,
                    id,
                    &w::CommissionInput {
                        affiliate_id: m.target_id,
                        referral_id,
                        client_id,
                        service_id: None,
                        amount: rest,
                        currency,
                        created_at: Utc::now(),
                    },
                )
                .await?;
                self.record(
                    &mut tx,
                    derived::AFFILIATE_OPENING_BALANCE,
                    &s,
                    "affiliate_commissions",
                    id,
                    inserted,
                    false,
                )
                .await?;
            }
            tx.commit().await?;
            self.commit_pending();
            match b.next {
                Some(n) => cursor = n,
                None => break,
            }
        }
        self.progress.steps.insert("affiliate_balances".into());
        self.checkpoint().await
    }

    // -----------------------------------------------------------------
    // to-dos, bans
    // -----------------------------------------------------------------

    pub(super) async fn h_todo(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::Todos;
        let s = sid(row);
        let title = clean_line(&row.text("title"), TITLE);
        if title.is_empty() {
            return Ok(self.skip(e, row, "info", "empty", "untitled to-do"));
        }
        let status = map::todo_status(&row.text("status"));
        let created = row.datetime("date");
        let input = w::TaskInput {
            title,
            body: Some(clean_text(&row.text("description"), BODY)).filter(|t| !t.is_empty()),
            status: status.into(),
            due_date: row.date("duedate"),
            assigned_to: self.reference(Entity::Staff.key(), row, "admin").await?,
            completed_at: (status == "done").then(|| created.unwrap_or_else(Utc::now)),
            created_at: created,
        };
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_task(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "staff_tasks", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    pub(super) async fn h_ban(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
        ip: bool,
    ) -> ImportResult<Outcome> {
        let e = if ip {
            Entity::BannedIps
        } else {
            Entity::BannedEmails
        };
        let s = sid(row);
        let (kind, value) = if ip {
            ("ip", map::ban_ip(&row.text("ip")))
        } else {
            ("domain", map::ban_domain(&row.text("domain")))
        };
        let Some(value) = value else {
            return Ok(self.skip(e, row, "info", "invalid", "not a valid address or domain"));
        };
        if ip && row.datetime("expires").is_some_and(|x| x <= Utc::now()) {
            return Ok(self.skip(e, row, "info", "expired", "the ban has expired"));
        }
        let reason = if ip {
            opt_line(row.str("reason"), 500)
        } else {
            None
        };
        let key = e.key();
        let existing = self.lookup(key, &s).await?;
        if self.dry {
            let inserted = existing.is_none();
            self.record(conn, key, &s, "fraud_bans", Uuid::now_v7(), inserted, false)
                .await?;
            return Ok(outcome(inserted, false));
        }
        // Keyed by (kind, value): a ban that exists already is adopted.
        let (id, inserted) = w::upsert_ban(
            conn,
            self.tenant,
            Uuid::now_v7(),
            kind,
            &value,
            reason.as_deref(),
        )
        .await?;
        let adopted = !inserted && existing.is_none_or(|m| m.target_id != id || m.adopted);
        self.record(conn, key, &s, "fraud_bans", id, inserted, adopted)
            .await?;
        Ok(outcome(inserted, adopted))
    }

    // -----------------------------------------------------------------
    // downloads
    // -----------------------------------------------------------------

    pub(super) async fn h_download_category(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::DownloadCategories;
        let s = sid(row);
        let name = clean_line(&row.text("name"), TITLE);
        let name = if name.is_empty() {
            format!("Category {s}")
        } else {
            name
        };
        if row.reference("parentid").is_some() {
            self.note_once(
                "download_categories_flattened",
                e.key(),
                "info",
                "nested download categories are imported as top-level categories",
            );
        }
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let description = opt_line(row.str("description"), LINE);
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_download_category(
                conn,
                self.tenant,
                id,
                &name,
                description.as_deref(),
                row.flag("hidden"),
                i32_of(row.int("id")),
            )
            .await?
        };
        self.record(conn, key, &s, "download_categories", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    pub(super) async fn h_download(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::Downloads;
        let s = sid(row);
        let title = clean_line(&row.text("title"), TITLE);
        if title.is_empty() {
            return Ok(self.skip(e, row, "info", "empty", "untitled download"));
        }
        let location = row.text("location");
        if location.trim().is_empty() {
            return Ok(self.skip(e, row, "info", "no_file", "the download has no file"));
        }
        let filename = clean_filename(&location);
        let product = row.flag("productdownload");
        self.note_once(
            "download_files_missing",
            e.key(),
            "warning",
            "downloads are imported without their files (WHMCS keeps them in its downloads directory): upload each file again under Admin → Downloads; until then they are listed but not served",
        );
        if product {
            self.note_once(
                "download_products_unlinked",
                e.key(),
                "warning",
                "product downloads keep the \"product\" access but their product links are not imported: link the products under Admin → Downloads",
            );
        }
        let input = w::DownloadInput {
            category_id: self
                .reference(Entity::DownloadCategories.key(), row, "category")
                .await?,
            title,
            description: Some(clean_text(&row.text("description"), BODY)).filter(|t| !t.is_empty()),
            content_type: map::download_content_type(&filename).into(),
            filename,
            access: map::download_access(row.flag("clientsonly"), product).into(),
            is_hidden: row.flag("hidden"),
            download_count: row.int("downloads").unwrap_or(0).max(0),
            created_at: None,
        };
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_download(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "downloads", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    // -----------------------------------------------------------------
    // network issues
    // -----------------------------------------------------------------

    pub(super) async fn h_network_issue(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let e = Entity::NetworkIssues;
        let s = sid(row);
        let title = clean_line(&row.text("title"), TITLE);
        if title.is_empty() {
            return Ok(self.skip(e, row, "info", "empty", "untitled network issue"));
        }
        let now = Utc::now();
        let start = row.datetime("startdate").unwrap_or(now);
        let end = row.datetime("enddate").filter(|x| *x > start);
        let (kind, status, impact) =
            map::network_issue(&row.text("status"), &row.text("priority"), start > now);
        let maintenance = kind == "maintenance";
        let resolved = matches!(status, "resolved" | "completed");
        let mut body = clean_text(&row.text("description"), 18_000);
        for (label, col) in [("Affecting", "affecting"), ("Server", "server_name")] {
            let v = clean_line(&row.text(col), LINE);
            if !v.is_empty() {
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(&format!("{label}: {v}"));
            }
        }
        if body.is_empty() {
            body.clone_from(&title);
        }
        let input = w::IncidentInput {
            kind: kind.into(),
            title,
            impact: impact.into(),
            status: status.into(),
            scheduled_start: maintenance.then_some(start),
            scheduled_end: if maintenance { end } else { None },
            started_at: start,
            resolved_at: resolved
                .then(|| end.or_else(|| row.datetime("lastupdate")).unwrap_or(start)),
            body,
        };
        let key = e.key();
        let (id, existing) = self.target(key, &s).await?;
        let (update_id, _) = self.target(derived::NETWORK_ISSUE_UPDATE, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_incident(conn, self.tenant, id, update_id, &input).await?
        };
        self.record(conn, key, &s, "status_incidents", id, inserted, false)
            .await?;
        self.record(
            conn,
            derived::NETWORK_ISSUE_UPDATE,
            &s,
            "status_incident_updates",
            update_id,
            inserted,
            false,
        )
        .await?;
        Ok(outcome(inserted, false))
    }
}
