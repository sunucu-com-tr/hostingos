//! One handler per entity: WHMCS row → HostingOS input → silent write.
//! In a dry run the same code runs without the writes, so the preview
//! reports exactly the rows the import would skip.

use std::net::IpAddr;

use chrono::{DateTime, NaiveDate, Utc};
use hos_core::ids::{ClientId, IntegrationId, ServiceId};
use hos_db::repo::import_write::{self as w, NaturalKey, PriceTarget, SlugTable};
use rust_decimal::Decimal;
use serde_json::{Map, Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Engine, Outcome, sid};
use crate::config::ServerTarget;
use crate::entity::{Entity, derived};
use crate::error::{ImportError, ImportResult};
use crate::map;
use crate::row::Row;
use crate::sanitize::{clean_line, clean_text, opt_line, rich_text, slugify};
use crate::source::{Attachment, AttachmentOwner, Source};

const NAME: usize = 120;
const LINE: usize = 255;
const BODY: usize = 100_000;
/// Absolute ceiling for one imported attachment, whatever the option says.
const ATTACHMENT_CEILING: u64 = 25 * 1024 * 1024;

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

fn non_negative(v: Option<Decimal>) -> Decimal {
    v.filter(|d| *d >= Decimal::ZERO).unwrap_or(Decimal::ZERO)
}

fn ip(row: &Row, col: &str) -> Option<IpAddr> {
    row.str(col).and_then(|s| s.parse().ok())
}

fn tld_key(s: &str) -> String {
    s.trim().trim_start_matches('.').to_ascii_lowercase()
}

fn valid_tld(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Content type of an accepted attachment from its bytes (same allow-list
/// as uploads in the client area: images, PDF, ZIP, plain text).
pub(crate) fn sniff(name: &str, bytes: &[u8]) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"PK\x03\x04") && lower.ends_with(".zip") {
        Some("application/zip")
    } else if (lower.ends_with(".txt") || lower.ends_with(".log"))
        && std::str::from_utf8(bytes).is_ok_and(|s| !s.contains('\0'))
    {
        Some("text/plain; charset=utf-8")
    } else {
        None
    }
}

/// A safe stored file name.
pub(super) fn clean_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_owned();
    if cleaned.is_empty() {
        "attachment".into()
    } else {
        cleaned
    }
}

impl Engine<'_> {
    pub(super) fn skip(
        &mut self,
        entity: Entity,
        row: &Row,
        level: &'static str,
        code: &str,
        message: impl Into<String>,
    ) -> Outcome {
        let id = sid(row);
        self.note(entity.key(), level, Some(&id), code, message);
        Outcome::Skipped
    }

    pub(super) async fn handle(
        &mut self,
        conn: &mut PgConnection,
        source: &mut dyn Source,
        entity: Entity,
        row: &Row,
    ) -> ImportResult<Outcome> {
        match entity {
            Entity::Currencies => self.h_currency(conn, row).await,
            Entity::TaxRules => self.h_tax_rule(conn, row).await,
            Entity::ClientGroups => self.h_client_group(conn, row).await,
            Entity::Servers => self.h_server(conn, row).await,
            Entity::Staff => self.h_staff(conn, row).await,
            Entity::Clients => self.h_client(conn, row).await,
            Entity::Users => self.h_user(conn, row).await,
            Entity::UserLinks => self.h_user_link(conn, row).await,
            Entity::Contacts => self.h_contact(conn, row).await,
            Entity::ClientNotes => self.h_note(conn, row).await,
            Entity::Credits => self.h_credit(conn, row).await,
            Entity::ProductGroups => self.h_product_group(conn, row).await,
            Entity::Products => self.h_product(conn, row).await,
            Entity::ConfigGroups => self.h_config_group(conn, row).await,
            Entity::ConfigLinks => self.h_config_link(conn, row).await,
            Entity::ConfigOptions => self.h_config_option(conn, row).await,
            Entity::ConfigValues => self.h_config_value(conn, row).await,
            Entity::Addons => self.h_addon(conn, row).await,
            Entity::Promotions => self.h_promotion(conn, row).await,
            Entity::Tlds => self.h_tld(conn, row).await,
            Entity::Pricing => self.h_pricing(conn, row).await,
            Entity::Invoices => self.h_invoice(conn, row).await,
            Entity::Orders => self.h_order(conn, row).await,
            Entity::Services => self.h_service(conn, row).await,
            Entity::ServiceOptions => self.h_service_option(conn, row).await,
            Entity::CustomFieldValues => self.h_custom_field(conn, row).await,
            Entity::ServiceAddons => self.h_service_addon(conn, row).await,
            Entity::CancelRequests => self.h_cancel_request(conn, row).await,
            Entity::Domains => self.h_domain(conn, row).await,
            Entity::InvoiceItems => self.h_invoice_item(conn, row).await,
            Entity::Transactions => self.h_transaction(conn, row).await,
            Entity::Departments => self.h_department(conn, row).await,
            Entity::Tickets => self.h_ticket(conn, source, row).await,
            Entity::TicketReplies => self.h_ticket_message(conn, source, row, false).await,
            Entity::TicketNotes => self.h_ticket_message(conn, source, row, true).await,
            Entity::CannedResponses => self.h_canned(conn, row).await,
            Entity::KbCategories => self.h_kb_category(conn, row).await,
            Entity::KbArticles => self.h_article(conn, row, false).await,
            Entity::Announcements => self.h_article(conn, row, true).await,
            Entity::Quotes => self.h_quote(conn, row).await,
            Entity::QuoteItems => self.h_quote_item(conn, row).await,
            Entity::Affiliates => self.h_affiliate(conn, row).await,
            Entity::AffiliateAccounts => self.h_affiliate_account(conn, row).await,
            Entity::AffiliateHistory => self.h_affiliate_history(conn, row).await,
            Entity::AffiliateWithdrawals => self.h_affiliate_withdrawal(conn, row).await,
            Entity::Todos => self.h_todo(conn, row).await,
            Entity::BannedIps => self.h_ban(conn, row, true).await,
            Entity::BannedEmails => self.h_ban(conn, row, false).await,
            Entity::DownloadCategories => self.h_download_category(conn, row).await,
            Entity::Downloads => self.h_download(conn, row).await,
            Entity::NetworkIssues => self.h_network_issue(conn, row).await,
            Entity::ActivityLog => self.h_activity(conn, row).await,
        }
    }

    /// Resolves `(id, existing, adopted)` for a row, adopting the row found by
    /// `natural` when it is not mapped yet.
    async fn resolve(
        &mut self,
        conn: &mut PgConnection,
        entity: &'static str,
        source_id: &str,
        natural: Option<(NaturalKey, &str)>,
    ) -> ImportResult<(Uuid, bool, bool)> {
        let (id, existing) = self.target(entity, source_id).await?;
        if let Some(m) = existing {
            return Ok((id, true, m.adopted));
        }
        if let Some((key, value)) = natural
            && let Some(found) = w::find_id(conn, self.tenant, key, value).await?
        {
            return Ok((found, true, true));
        }
        Ok((id, false, false))
    }

    // -----------------------------------------------------------------
    // setup
    // -----------------------------------------------------------------

    async fn h_currency(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Currencies.key();
        let s = sid(row);
        let Some(code) = row
            .str("code")
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()))
        else {
            return Ok(self.skip(
                Entity::Currencies,
                row,
                "error",
                "invalid_code",
                "not a 3-letter ISO currency code",
            ));
        };
        let (id, existing, adopted) = self
            .resolve(conn, key, &s, Some((NaturalKey::CurrencyCode, &code)))
            .await?;
        let inserted = if adopted || self.dry {
            !existing
        } else {
            let input = w::CurrencyInput {
                code: code.clone(),
                name: code.clone(),
                prefix: clean_line(&row.raw_str("prefix").unwrap_or_default(), 10),
                suffix: clean_line(&row.raw_str("suffix").unwrap_or_default(), 10),
                exchange_rate: row
                    .dec("rate")
                    .filter(|r| *r > Decimal::ZERO)
                    .unwrap_or(Decimal::ONE),
                sort_order: i32_of(row.int("id")),
            };
            w::upsert_currency(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "currencies", id, inserted, adopted)
            .await?;
        Ok(outcome(inserted, adopted))
    }

    async fn h_tax_rule(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::TaxRules.key();
        let s = sid(row);
        let (id, existing, _) = self.resolve(conn, key, &s, None).await?;
        let level = if row.int("level") == Some(2) { 2 } else { 1 };
        let rate = row.dec("taxrate").unwrap_or_default();
        if !(Decimal::ZERO..=Decimal::ONE_HUNDRED).contains(&rate) {
            return Ok(self.skip(
                Entity::TaxRules,
                row,
                "error",
                "invalid_rate",
                "tax rate outside 0–100",
            ));
        }
        let country = row
            .str("country")
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()));
        let input = w::TaxRuleInput {
            name: clean_line(&row.text("name"), NAME)
                .chars()
                .take(NAME)
                .collect::<String>(),
            level,
            country_code: country,
            region: opt_line(row.str("state"), NAME),
            rate,
            is_compound: level == 2 && self.refdata.l2_compound,
        };
        let input = if input.name.is_empty() {
            w::TaxRuleInput {
                name: format!("Tax {s}"),
                ..input
            }
        } else {
            input
        };
        let inserted = if self.dry {
            !existing
        } else {
            w::upsert_tax_rule(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "tax_rules", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_client_group(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::ClientGroups.key();
        let s = sid(row);
        let name = clean_line(&row.text("groupname"), NAME);
        if name.is_empty() {
            return Ok(self.skip(
                Entity::ClientGroups,
                row,
                "warning",
                "no_name",
                "client group without a name",
            ));
        }
        let (id, existing, adopted) = self
            .resolve(conn, key, &s, Some((NaturalKey::ClientGroupName, &name)))
            .await?;
        let inserted = if adopted || self.dry {
            !existing
        } else {
            let input = w::ClientGroupInput {
                name,
                color: map::color(&row.text("groupcolour")),
                discount_percent: row
                    .dec("discountpercent")
                    .unwrap_or_default()
                    .clamp(Decimal::ZERO, Decimal::ONE_HUNDRED),
            };
            w::upsert_client_group(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "client_groups", id, inserted, adopted)
            .await?;
        Ok(outcome(inserted, adopted))
    }

    async fn h_server(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Servers.key();
        let s = sid(row);
        let module = row.text("type");
        let suggested = map::server_kind(&module).map(|(_, k)| k);
        let target = self.maps.server(row.id(), suggested);
        let kind = match target {
            ServerTarget::Skip => {
                let msg = if suggested.is_none() {
                    format!(
                        "server module {module:?} has no HostingOS provider; services keep no integration"
                    )
                } else {
                    "server mapped to \"skip\"".to_owned()
                };
                return Ok(self.skip(Entity::Servers, row, "skipped", "server_skipped", msg));
            }
            ServerTarget::Existing(id) => {
                self.record(conn, key, &s, "integrations", id, false, true)
                    .await?;
                return Ok(Outcome::Adopted);
            }
            ServerTarget::Create(kind) => kind,
        };
        let category = if matches!(
            kind.as_str(),
            "cpanel" | "plesk" | "directadmin" | "cyberpanel"
        ) {
            "panel"
        } else {
            "virt"
        };
        let (id, existing) = self.target(key, &s).await?;
        if existing.is_some() {
            // Created earlier: the admin may have configured it since — leave it.
            self.pending.insert((key, s.clone()), (id, false));
            return Ok(Outcome::Updated);
        }
        if !self.dry && !self.within_limit("integrations") {
            return Ok(self.skip(
                Entity::Servers,
                row,
                "error",
                "plan_limit",
                "plan limit reached: integrations",
            ));
        }
        let host = row
            .str("hostname")
            .or_else(|| row.str("ipaddress"))
            .unwrap_or_default();
        let secure = row.flag("secure");
        let username = row.text("username");
        let (password, access) = if self.opts.server_credentials {
            (
                self.secret("servers", row, "password"),
                self.server_access_hash(row),
            )
        } else {
            (None, None)
        };
        let port = row.int("port").filter(|p| *p > 0);
        let mut config = Map::new();
        let mut secrets = Map::new();
        match kind.as_str() {
            "cpanel" | "plesk" | "directadmin" | "cyberpanel" => {
                config.insert("host".into(), json!(host));
                if let Some(p) = port {
                    config.insert("port".into(), json!(p));
                }
                config.insert("use_ssl".into(), json!(secure || port.is_none()));
                config.insert("username".into(), json!(username));
                match kind.as_str() {
                    "cpanel" => {
                        if let Some(t) = access.clone() {
                            secrets.insert("api_token".into(), json!(t));
                        }
                    }
                    "plesk" => {
                        if let Some(t) = access.clone() {
                            secrets.insert("api_key".into(), json!(t));
                        }
                        if let Some(p) = password.clone() {
                            secrets.insert("password".into(), json!(p));
                        }
                    }
                    "directadmin" => {
                        if let Some(p) = access.clone().or_else(|| password.clone()) {
                            secrets.insert("login_key".into(), json!(p));
                        }
                    }
                    _ => {
                        if let Some(p) = password.clone() {
                            secrets.insert("password".into(), json!(p));
                        }
                    }
                }
            }
            "virtualizor" => {
                config.insert(
                    "api_url".into(),
                    json!(format!("https://{host}:{}", port.unwrap_or(4085))),
                );
                if !username.is_empty() {
                    secrets.insert("api_key".into(), json!(username));
                }
                if let Some(p) = password.clone() {
                    secrets.insert("api_pass".into(), json!(p));
                }
            }
            "proxmox" => {
                config.insert(
                    "api_url".into(),
                    json!(format!("https://{host}:{}", port.unwrap_or(8006))),
                );
                if !username.is_empty() {
                    config.insert("token_id".into(), json!(username));
                }
            }
            "hetzner" => {
                if let Some(t) = access.clone().or_else(|| password.clone()) {
                    secrets.insert("api_token".into(), json!(t));
                }
            }
            _ => {
                config.insert("api_url".into(), json!(format!("https://{host}")));
                config.insert("username".into(), json!(username));
            }
        }
        if self.opts.server_credentials && secrets.is_empty() {
            self.note(
                key,
                "warning",
                Some(&s),
                "credentials_missing",
                format!("{kind}: enter the credentials of this integration before activating it"),
            );
        }
        let base = format!(
            "WHMCS: {}",
            clean_line(&row.str("name").unwrap_or_else(|| host.clone()), 80)
        );
        let name = match w::find_id(conn, self.tenant, NaturalKey::IntegrationName, &base).await? {
            Some(_) => format!("{base} #{s}"),
            None => base,
        };
        let inserted = if self.dry {
            true
        } else {
            let secrets_enc = if secrets.is_empty() {
                None
            } else {
                let aad =
                    hos_core::crypto::integration_aad(self.tenant, IntegrationId::from_uuid(id));
                Some(
                    self.secret_box
                        .encrypt_json(&Json::Object(secrets), &aad)
                        .map_err(hos_core::CoreError::from)?,
                )
            };
            let input = w::IntegrationInput {
                category: category.into(),
                kind: kind.clone(),
                name,
                config: Json::Object(config),
                secrets_enc,
            };
            w::insert_integration(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "integrations", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    /// `tblservers.accesshash`: encrypted in current WHMCS versions, plain in old ones.
    fn server_access_hash(&mut self, row: &Row) -> Option<String> {
        let raw = row.str("accesshash")?;
        if self.flavor == crate::source::Flavor::Api {
            return Some(raw);
        }
        if let Some(hash) = self.cc_hash.clone()
            && let Ok(Some(v)) = crate::crypto::whmcs_decrypt(&raw, hash.expose_str())
        {
            return Some(v);
        }
        // Old versions store the WHM access hash in clear (multi-line hex).
        let compact: String = raw.split_whitespace().collect();
        (compact.len() >= 32 && compact.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then_some(compact)
    }

    async fn h_staff(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Staff.key();
        let s = sid(row);
        let Some(email) = map::email(&row.text("email")) else {
            return Ok(self.skip(
                Entity::Staff,
                row,
                "error",
                "invalid_email",
                "staff member without a valid e-mail address",
            ));
        };
        let (mut id, mut existing, mut adopted) = self.resolve(conn, key, &s, None).await?;
        if !existing && let Some((uid, kind)) = w::find_user(conn, self.tenant, &email).await? {
            if kind != "staff" {
                return Ok(self.skip(
                    Entity::Staff,
                    row,
                    "conflict",
                    "email_in_use",
                    format!("{email} is already a client login"),
                ));
            }
            (id, existing, adopted) = (uid, true, true);
        }
        let inserted = if adopted || self.dry {
            !existing
        } else {
            // New staff start deactivated, unverified and without a password:
            // whoever controls the source must not get a working staff login
            // (with the mapped role) by importing an e-mail address of theirs.
            // Someone with `staff.manage` activates them in Staff (plan limit
            // checked there); they then set a password through "forgot
            // password", which also verifies the address. A re-run keeps the
            // status and e-mail address an admin gave them.
            let input = w::UserInput {
                email: email.clone(),
                password_hash: None,
                kind: "staff".into(),
                first_name: clean_line(&row.text("firstname"), NAME),
                last_name: clean_line(&row.text("lastname"), NAME),
                phone: None,
                locale: map::locale(&row.text("language"), &self.refdata.default_locale),
                email_verified_at: None,
                status: "disabled".into(),
                last_login_at: None,
                created_at: row.datetime("created_at"),
            };
            w::upsert_user(conn, self.tenant, id, &input).await?
        };
        if !self.dry && !adopted {
            match self.maps.role(row.int("roleid").unwrap_or(0)).map(str::to_owned) {
                Some(slug) => match w::find_id(conn, self.tenant, NaturalKey::RoleSlug, &slug).await? {
                    Some(role) => w::grant_role(conn, self.tenant, id, role).await?,
                    None => self.note(key, "warning", Some(&s), "role_missing", format!("role {slug:?} does not exist")),
                },
                None => self.note(key, "warning", Some(&s), "no_role", "no HostingOS role mapped: the staff member has no permissions until one is assigned"),
            }
        }
        self.note_once(
            "staff_passwords",
            key,
            "info",
            "staff passwords are never imported and imported staff start deactivated: someone with staff management activates them in Staff, then they use \"forgot password\" to set a password",
        );
        self.record(conn, key, &s, "users", id, inserted, adopted)
            .await?;
        Ok(outcome(inserted, adopted))
    }

    // -----------------------------------------------------------------
    // clients & logins
    // -----------------------------------------------------------------

    /// Password hash of a WHMCS row, counted for the report.
    fn password_of(&mut self, row: &Row, col: &str) -> Option<String> {
        if self.flavor == crate::source::Flavor::Api {
            self.no_hashes += 1;
            return None;
        }
        match row
            .raw_str(col)
            .as_deref()
            .and_then(hos_auth::legacy::from_whmcs)
        {
            Some(h) => {
                if h.scheme.is_weak() {
                    self.weak_hashes += 1;
                }
                Some(h.stored)
            }
            None => {
                self.no_hashes += 1;
                None
            }
        }
    }

    /// Creates / updates a client login; returns its id (`None` when the
    /// e-mail belongs to staff or to a login that may not be adopted).
    ///
    /// A login that already exists in HostingOS with the same e-mail address
    /// (registered before the import) is adopted only when it is already a
    /// member of `client`, or when its address is verified — then its
    /// password is cleared and its sessions end ([`w::reset_adopted_login`]),
    /// so it reaches the imported account only after "forgot password". An
    /// unverified one is never adopted: anyone can register an address they
    /// do not own, and adopting it would hand them the imported account.
    #[allow(clippy::too_many_arguments)]
    async fn login(
        &mut self,
        conn: &mut PgConnection,
        entity: &'static str,
        source_id: &str,
        email: &str,
        client: Option<Uuid>,
        input: w::UserInput,
    ) -> ImportResult<Option<(Uuid, bool, bool)>> {
        let (mut id, mut existing, mut adopted) =
            self.resolve(conn, entity, source_id, None).await?;
        if !existing && let Some((uid, kind)) = w::find_user(conn, self.tenant, email).await? {
            if kind != "client" {
                self.note(
                    entity,
                    "conflict",
                    Some(source_id),
                    "email_in_use",
                    format!("{email} is a staff login; the client login was not created"),
                );
                return Ok(None);
            }
            let (verified, member) = w::login_standing(conn, self.tenant, uid, client).await?;
            if !member && !verified {
                self.note(
                    entity,
                    "conflict",
                    Some(source_id),
                    "login_unverified",
                    format!(
                        "{email} already has a login here whose address was never verified: it was not linked to the imported account. Once its owner has verified the address, add it to the client's users"
                    ),
                );
                return Ok(None);
            }
            if !member {
                if !self.dry {
                    w::reset_adopted_login(conn, self.tenant, uid).await?;
                }
                self.note(
                    entity,
                    "warning",
                    Some(source_id),
                    "login_adopted",
                    format!(
                        "{email} already had a login here: it was linked to the imported account, its password was cleared and its sessions ended (it signs in again with \"forgot password\")"
                    ),
                );
            }
            (id, existing, adopted) = (uid, true, true);
        }
        let inserted = if adopted || self.dry {
            !existing
        } else {
            w::upsert_user(conn, self.tenant, id, &input).await?
        };
        self.record(conn, entity, source_id, "users", id, inserted, adopted)
            .await?;
        Ok(Some((id, inserted, adopted)))
    }

    fn verified(
        &self,
        at: Option<DateTime<Utc>>,
        created: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        at.or_else(|| {
            self.opts
                .verified_emails
                .then(|| created.unwrap_or_else(Utc::now))
        })
    }

    async fn h_client(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Clients.key();
        let s = sid(row);
        let Some(email) = map::email(&row.text("email")) else {
            return Ok(self.skip(
                Entity::Clients,
                row,
                "error",
                "invalid_email",
                "client without a valid e-mail address",
            ));
        };
        let (id, existing) = self.target(key, &s).await?;
        let status = map::client_status(&row.text("status"));
        let currency = self.currency_of(row.int("currency"));
        let created = row
            .datetime("datecreated")
            .or_else(|| row.datetime("created_at"));
        let company = opt_line(row.str("companyname"), NAME);
        let group_id = self
            .reference(Entity::ClientGroups.key(), row, "groupid")
            .await?;
        if row.reference("groupid").is_some() && group_id.is_none() {
            self.note(
                key,
                "warning",
                Some(&s),
                "group_missing",
                "client group not imported; client has no group",
            );
        }
        let gateway = row.text("defaultgateway");
        let input = w::ClientInput {
            kind: if company.is_some() {
                "corporate".into()
            } else {
                "individual".into()
            },
            first_name: clean_line(&row.text("firstname"), NAME),
            last_name: clean_line(&row.text("lastname"), NAME),
            company_name: company,
            email: email.clone(),
            phone: opt_line(row.str("phonenumber"), 40),
            address_line1: opt_line(row.str("address1"), LINE),
            address_line2: opt_line(row.str("address2"), LINE),
            city: opt_line(row.str("city"), NAME),
            state: opt_line(row.str("state"), NAME),
            postal_code: opt_line(row.str("postcode"), 20),
            country_code: map::country(&row.text("country"), &self.refdata.default_country),
            tax_number: opt_line(row.str("tax_id"), 40),
            currency: currency.clone(),
            locale: map::locale(&row.text("language"), &self.refdata.default_locale),
            group_id,
            status: status.into(),
            tax_exempt: row.flag("taxexempt"),
            late_fee_exempt: row.flag("latefeeoveride"),
            overdue_notices: !row.flag("overideduenotices"),
            separate_invoices: row.flag("separateinvoices"),
            marketing_opt_in: if row.has("marketing_emails_opt_in") {
                row.flag("marketing_emails_opt_in")
            } else {
                !row.flag("emailoptout")
            },
            default_gateway_id: (!gateway.is_empty())
                .then(|| self.maps.gateway(&gateway))
                .flatten(),
            signup_ip: ip(row, "ip"),
            closed_at: (status == "closed")
                .then(|| row.datetime("updated_at").unwrap_or_else(Utc::now)),
            created_at: created,
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            if existing.is_none() && status != "closed" && !self.within_limit("max_clients") {
                return Ok(self.skip(
                    Entity::Clients,
                    row,
                    "error",
                    "plan_limit",
                    "plan limit reached: clients",
                ));
            }
            w::upsert_client(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "clients", id, inserted, false)
            .await?;
        self.client_currency.insert(id, currency.clone());
        self.expected_credit
            .insert(id, (row.money("credit"), currency));
        if !self.dry {
            if let Some(cf) = row.str("customfields_json") {
                let mut fields = Map::new();
                if let Ok(Json::Array(list)) = serde_json::from_str::<Json>(&cf) {
                    for f in list {
                        let fr = crate::source::api::row_of(&f, &[]);
                        if let Some(v) = fr.str("value") {
                            let name = fr
                                .str("name")
                                .unwrap_or_else(|| format!("field_{}", fr.int("id").unwrap_or(0)));
                            fields.insert(clean_line(&name, NAME), json!(clean_text(&v, 2000)));
                        }
                    }
                }
                if !fields.is_empty() {
                    w::merge_custom_fields(conn, self.tenant, false, id, &Json::Object(fields))
                        .await?;
                }
            }
            if let Some(notes) = row.str("notes") {
                let (nid, _) = self.target(derived::CLIENT_ADMIN_NOTE, &s).await?;
                let note = w::NoteInput {
                    client_id: id,
                    author_user_id: None,
                    body: clean_text(&notes, BODY),
                    is_sticky: true,
                    created_at: created,
                };
                let ins = w::upsert_note(conn, self.tenant, nid, &note).await?;
                self.record(
                    conn,
                    derived::CLIENT_ADMIN_NOTE,
                    &s,
                    "client_notes",
                    nid,
                    ins,
                    false,
                )
                .await?;
            }
        }
        if self.refdata.legacy_logins && status != "closed" {
            let user = w::UserInput {
                email: email.clone(),
                password_hash: self.password_of(row, "password"),
                kind: "client".into(),
                first_name: input.first_name.clone(),
                last_name: input.last_name.clone(),
                phone: input.phone.clone(),
                locale: input.locale.clone(),
                email_verified_at: self.verified(None, created),
                status: "active".into(),
                last_login_at: row.datetime("lastlogin"),
                created_at: created,
            };
            if let Some((uid, _, _)) = self
                .login(conn, derived::CLIENT_OWNER_USER, &s, &email, Some(id), user)
                .await?
                && !self.dry
            {
                w::link_client_user(conn, self.tenant, id, uid, true, &[]).await?;
            }
        }
        Ok(outcome(inserted, false))
    }

    async fn h_user(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Users.key();
        let s = sid(row);
        let Some(email) = map::email(&row.text("email")) else {
            return Ok(self.skip(
                Entity::Users,
                row,
                "error",
                "invalid_email",
                "login without a valid e-mail address",
            ));
        };
        let created = row.datetime("created_at");
        let input = w::UserInput {
            email: email.clone(),
            password_hash: self.password_of(row, "password"),
            kind: "client".into(),
            first_name: clean_line(&row.text("first_name"), NAME),
            last_name: clean_line(&row.text("last_name"), NAME),
            phone: None,
            locale: map::locale(&row.text("language"), &self.refdata.default_locale),
            email_verified_at: self.verified(row.datetime("email_verified_at"), created),
            status: "active".into(),
            last_login_at: row.datetime("last_login"),
            created_at: created,
        };
        Ok(
            match self.login(conn, key, &s, &email, None, input).await? {
                Some((_, inserted, adopted)) => outcome(inserted, adopted),
                None => Outcome::Skipped,
            },
        )
    }

    async fn h_user_link(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let user = self
            .reference(Entity::Users.key(), row, "auth_user_id")
            .await?;
        let client = self
            .reference(Entity::Clients.key(), row, "client_id")
            .await?;
        let (Some(user), Some(client)) = (user, client) else {
            return Ok(self.skip(
                Entity::UserLinks,
                row,
                "warning",
                "parent_missing",
                "login or client not imported",
            ));
        };
        let owner = row.flag("owner");
        if !self.dry {
            let perms = map::member_permissions(&row.text("permissions"), owner);
            w::link_client_user(conn, self.tenant, client, user, owner, &perms).await?;
        }
        Ok(Outcome::Updated)
    }

    async fn h_contact(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Contacts.key();
        let s = sid(row);
        let Some(client) = self.reference(Entity::Clients.key(), row, "userid").await? else {
            return Ok(self.skip(
                Entity::Contacts,
                row,
                "warning",
                "parent_missing",
                "client not imported",
            ));
        };
        let Some(email) = map::email(&row.text("email")) else {
            return Ok(self.skip(
                Entity::Contacts,
                row,
                "error",
                "invalid_email",
                "contact without a valid e-mail address",
            ));
        };
        let (id, existing) = self.target(key, &s).await?;
        let mut prefs = Vec::new();
        for (col, kind) in [
            ("invoiceemails", "invoices"),
            ("productemails", "services"),
            ("domainemails", "domains"),
            ("supportemails", "support"),
        ] {
            if row.flag(col) {
                prefs.push(kind.to_owned());
            }
        }
        let input = w::ContactInput {
            client_id: client,
            first_name: clean_line(&row.text("firstname"), NAME),
            last_name: clean_line(&row.text("lastname"), NAME),
            company_name: opt_line(row.str("companyname"), NAME),
            email: email.clone(),
            phone: opt_line(row.str("phonenumber"), 40),
            address_line1: opt_line(row.str("address1"), LINE),
            address_line2: opt_line(row.str("address2"), LINE),
            city: opt_line(row.str("city"), NAME),
            state: opt_line(row.str("state"), NAME),
            postal_code: opt_line(row.str("postcode"), 20),
            country_code: map::country(&row.text("country"), &self.refdata.default_country),
            email_preferences: prefs,
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_contact(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "contacts", id, inserted, false)
            .await?;
        if self.refdata.legacy_logins && row.flag("subaccount") {
            let user = w::UserInput {
                email: email.clone(),
                password_hash: self.password_of(row, "password"),
                kind: "client".into(),
                first_name: input.first_name.clone(),
                last_name: input.last_name.clone(),
                phone: input.phone.clone(),
                locale: self.refdata.default_locale.clone(),
                email_verified_at: self.verified(None, row.datetime("created_at")),
                status: "active".into(),
                last_login_at: None,
                created_at: row.datetime("created_at"),
            };
            if let Some((uid, _, _)) = self
                .login(conn, derived::CONTACT_USER, &s, &email, Some(client), user)
                .await?
                && !self.dry
            {
                let perms = map::member_permissions(&row.text("permissions"), false);
                w::link_client_user(conn, self.tenant, client, uid, false, &perms).await?;
            }
        }
        Ok(outcome(inserted, false))
    }

    async fn h_note(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::ClientNotes.key();
        let s = sid(row);
        let Some(client) = self.reference(Entity::Clients.key(), row, "userid").await? else {
            return Ok(self.skip(
                Entity::ClientNotes,
                row,
                "warning",
                "parent_missing",
                "client not imported",
            ));
        };
        let body = clean_text(&row.text("note"), BODY);
        if body.is_empty() {
            return Ok(self.skip(Entity::ClientNotes, row, "info", "empty", "empty note"));
        }
        let (id, existing) = self.target(key, &s).await?;
        let input = w::NoteInput {
            client_id: client,
            author_user_id: self.reference(Entity::Staff.key(), row, "adminid").await?,
            body,
            is_sticky: row.flag("sticky"),
            created_at: row.datetime("created"),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_note(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "client_notes", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_credit(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Credits.key();
        let s = sid(row);
        let Some(client) = self
            .reference(Entity::Clients.key(), row, "clientid")
            .await?
        else {
            return Ok(self.skip(
                Entity::Credits,
                row,
                "warning",
                "parent_missing",
                "client not imported",
            ));
        };
        let amount = row.money("amount");
        if amount.is_zero() {
            return Ok(self.skip(Entity::Credits, row, "info", "zero", "zero credit line"));
        }
        let description = clean_line(&row.text("description"), LINE);
        let lower = description.to_ascii_lowercase();
        let invoice = self.reference(Entity::Invoices.key(), row, "relid").await?;
        let kind = if lower.contains("overpayment") {
            "overpayment"
        } else if lower.contains("refund") {
            "refund"
        } else if amount < Decimal::ZERO && invoice.is_some() {
            "invoice_payment"
        } else {
            "manual"
        };
        let currency = self.currency_of_client(conn, client).await?;
        let (id, existing) = self.target(key, &s).await?;
        let input = w::CreditInput {
            client_id: client,
            amount,
            currency,
            kind: kind.into(),
            description,
            invoice_id: invoice,
            created_at: row.datetime("date"),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_credit(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "credits", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    /// Writes the opening-balance adjustment of a client: the imported
    /// entries plus the adjustment equal the WHMCS credit. Entries made in
    /// HostingOS are not part of that sum, so credit used after go-live is
    /// never added back by a re-import.
    pub(super) async fn adjust_credit(
        &mut self,
        conn: &mut PgConnection,
        client: Uuid,
        expected: Decimal,
        currency: &str,
    ) -> ImportResult<()> {
        let s = client.to_string();
        let (id, existing) = self.target(derived::CREDIT_ADJUSTMENT, &s).await?;
        let current = w::credit_balance(conn, self.tenant, client, currency).await?;
        let own = if existing.is_some() {
            w::credit_amount(conn, self.tenant, id)
                .await?
                .unwrap_or_default()
        } else {
            Decimal::ZERO
        };
        let diff = expected - (current - own);
        if diff.is_zero() {
            if existing.is_some() && !own.is_zero() {
                w::delete_credit(conn, self.tenant, id).await?;
            }
            return Ok(());
        }
        let input = w::CreditInput {
            client_id: client,
            amount: diff,
            currency: currency.to_owned(),
            kind: "manual".into(),
            description: "Opening balance (WHMCS import)".into(),
            invoice_id: None,
            created_at: None,
        };
        let inserted = w::upsert_credit(conn, self.tenant, id, &input).await?;
        self.record(
            conn,
            derived::CREDIT_ADJUSTMENT,
            &s,
            "credits",
            id,
            inserted,
            false,
        )
        .await
    }

    // -----------------------------------------------------------------
    // catalog
    // -----------------------------------------------------------------

    async fn h_product_group(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::ProductGroups.key();
        let s = sid(row);
        let name = clean_line(&row.text("name"), NAME);
        let name = if name.is_empty() {
            format!("Group {s}")
        } else {
            name
        };
        let (id, existing) = self.target(key, &s).await?;
        let base = slugify(&row.str("slug").unwrap_or_else(|| name.clone()));
        let base = if base.is_empty() {
            format!("group-{s}")
        } else {
            base
        };
        let slug = match (existing.is_some(), self.dry) {
            (true, false) => w::current_slug(conn, self.tenant, SlugTable::ProductGroups, id)
                .await?
                .unwrap_or(base),
            _ => {
                w::unique_slug(conn, self.tenant, SlugTable::ProductGroups, &base, None, id).await?
            }
        };
        let input = w::ProductGroupInput {
            slug,
            name,
            tagline: opt_line(row.str("tagline").or_else(|| row.str("headline")), LINE),
            is_hidden: row.flag("hidden"),
            sort_order: i32_of(row.int("order")),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_product_group(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "product_groups", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_product(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Products.key();
        let s = sid(row);
        let pay = map::pay_type(&row.text("paytype"));
        self.product_pay_type.insert(format!("p{s}"), pay.into());
        if let Some(existing) = self.maps.product(row.id()) {
            self.record(conn, key, &s, "products", existing, false, true)
                .await?;
            return Ok(Outcome::Adopted);
        }
        let (id, mapped) = self.target(key, &s).await?;
        if mapped.is_some_and(|m| m.adopted) {
            self.pending.insert((key, s.clone()), (id, true));
            return Ok(Outcome::Adopted);
        }
        let name = clean_line(&row.text("name"), NAME);
        let name = if name.is_empty() {
            format!("Product {s}")
        } else {
            name
        };
        let module = row.text("servertype");
        let kind = map::product_type(&row.text("type"), &module, &name);
        let base = slugify(&row.str("slug").unwrap_or_else(|| name.clone()));
        let base = if base.is_empty() {
            format!("product-{s}")
        } else {
            base
        };
        let slug = match (mapped.is_some(), self.dry) {
            (true, false) => w::current_slug(conn, self.tenant, SlugTable::Products, id)
                .await?
                .unwrap_or(base),
            _ => w::unique_slug(conn, self.tenant, SlugTable::Products, &base, None, id).await?,
        };
        let integration_id = self
            .reference(Entity::Servers.key(), row, "group_server_id")
            .await?;
        let mut options = Map::new();
        for i in 1..=24 {
            let col = format!("configoption{i}");
            if let Some(v) = row.str(&col) {
                options.insert(col, json!(clean_line(&v, 500)));
            }
        }
        let requires_domain = if row.flag("showdomainoptions") {
            if matches!(kind, "shared_hosting" | "reseller_hosting") {
                "required"
            } else {
                "optional"
            }
        } else {
            "none"
        };
        let input = w::ProductInput {
            group_id: self
                .reference(Entity::ProductGroups.key(), row, "gid")
                .await?,
            slug,
            product_type: kind.into(),
            name,
            description: Some(rich_text(&row.text("description"), BODY)).filter(|d| !d.is_empty()),
            integration_id,
            module_config: json!({ "whmcs": { "module": module, "options": options } }),
            pay_type: pay.into(),
            auto_setup: map::auto_setup(&row.text("autosetup")).into(),
            requires_domain: requires_domain.into(),
            stock_control: row.flag("stockcontrol"),
            stock_quantity: i32_of(row.int("qty")).max(0),
            allow_quantity: row.int("allowqty").unwrap_or(0) > 0,
            is_taxable: row.flag("tax"),
            is_hidden: row.flag("hidden"),
            is_retired: row.flag("retired"),
            sort_order: i32_of(row.int("order")),
        };
        let inserted = if self.dry {
            mapped.is_none()
        } else {
            w::upsert_product(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "products", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_config_group(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::ConfigGroups.key();
        let s = sid(row);
        let name = clean_line(&row.text("name"), NAME);
        let name = if name.is_empty() {
            format!("Options {s}")
        } else {
            name
        };
        let (id, existing, adopted) = self
            .resolve(conn, key, &s, Some((NaturalKey::ConfigGroupName, &name)))
            .await?;
        let inserted = if adopted || self.dry {
            !existing
        } else {
            let desc = opt_line(row.str("description"), LINE);
            w::upsert_config_group(conn, self.tenant, id, &name, desc.as_deref()).await?
        };
        self.record(conn, key, &s, "config_option_groups", id, inserted, adopted)
            .await?;
        Ok(outcome(inserted, adopted))
    }

    async fn h_config_link(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let group = self
            .lookup(Entity::ConfigGroups.key(), &row.text("gid"))
            .await?;
        let product = self
            .lookup(Entity::Products.key(), &row.text("pid"))
            .await?;
        let (Some(g), Some(p)) = (group, product) else {
            return Ok(self.skip(
                Entity::ConfigLinks,
                row,
                "warning",
                "parent_missing",
                "option group or product not imported",
            ));
        };
        if g.adopted && p.adopted {
            return Ok(Outcome::Skipped);
        }
        if !self.dry {
            w::link_product(conn, self.tenant, false, g.target_id, p.target_id).await?;
        }
        Ok(Outcome::Updated)
    }

    async fn h_config_option(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::ConfigOptions.key();
        let s = sid(row);
        let Some(group) = self
            .lookup(Entity::ConfigGroups.key(), &row.text("gid"))
            .await?
        else {
            return Ok(self.skip(
                Entity::ConfigOptions,
                row,
                "warning",
                "parent_missing",
                "option group not imported",
            ));
        };
        if group.adopted {
            return Ok(self.skip(
                Entity::ConfigOptions,
                row,
                "info",
                "adopted_parent",
                "option group already existed; its options are left as they are",
            ));
        }
        let (module_key, label) = map::split_pipe(&clean_line(&row.text("optionname"), NAME));
        let (id, existing) = self.target(key, &s).await?;
        let input = w::ConfigOptionInput {
            group_id: group.target_id,
            name: if label.is_empty() {
                format!("Option {s}")
            } else {
                label
            },
            option_type: map::config_option_type(row.int("optiontype").unwrap_or(1)).into(),
            module_key,
            min_quantity: i32_of(row.int("qtyminimum")).max(0),
            max_quantity: i32_of(row.int("qtymaximum")).max(0),
            is_hidden: row.flag("hidden"),
            sort_order: i32_of(row.int("order")),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_config_option(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "config_options", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_config_value(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::ConfigValues.key();
        let s = sid(row);
        let Some(option) = self
            .lookup(Entity::ConfigOptions.key(), &row.text("configid"))
            .await?
        else {
            return Ok(self.skip(
                Entity::ConfigValues,
                row,
                "warning",
                "parent_missing",
                "configurable option not imported",
            ));
        };
        let (value_key, label) = map::split_pipe(&clean_line(&row.text("optionname"), NAME));
        let label = if label.is_empty() {
            format!("Value {s}")
        } else {
            label
        };
        let value = value_key.unwrap_or_else(|| label.clone());
        let (id, existing) = self.target(key, &s).await?;
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_config_value(
                conn,
                self.tenant,
                id,
                option.target_id,
                &label,
                &value,
                row.flag("hidden"),
                i32_of(row.int("sortorder")),
            )
            .await?
        };
        self.record(conn, key, &s, "config_option_values", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_addon(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Addons.key();
        let s = sid(row);
        let name = clean_line(&row.text("name"), NAME);
        let name = if name.is_empty() {
            format!("Addon {s}")
        } else {
            name
        };
        let cycle = row.text("billingcycle");
        let pay = match cycle.to_ascii_lowercase().as_str() {
            "free" | "free account" => "free",
            "one time" | "onetime" => "one_time",
            _ => "recurring",
        };
        self.product_pay_type.insert(format!("a{s}"), pay.into());
        let (id, existing) = self.target(key, &s).await?;
        let base = slugify(&name);
        let base = if base.is_empty() {
            format!("addon-{s}")
        } else {
            base
        };
        let slug = match (existing.is_some(), self.dry) {
            (true, false) => w::current_slug(conn, self.tenant, SlugTable::ProductAddons, id)
                .await?
                .unwrap_or(base),
            _ => {
                w::unique_slug(conn, self.tenant, SlugTable::ProductAddons, &base, None, id).await?
            }
        };
        let input = w::AddonInput {
            slug,
            name,
            description: Some(rich_text(&row.text("description"), BODY)).filter(|d| !d.is_empty()),
            pay_type: pay.into(),
            auto_activate: row.flag("autoactivate"),
            show_on_order: row.flag("showorder"),
            is_taxable: row.flag("tax"),
            is_hidden: row.flag("hidden") || row.flag("retired"),
            sort_order: i32_of(row.int("weight")),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_addon(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "product_addons", id, inserted, false)
            .await?;
        if !self.dry {
            for pid in row
                .text("packages")
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                if let Some(p) = self.lookup(Entity::Products.key(), pid).await? {
                    w::link_product(conn, self.tenant, true, id, p.target_id).await?;
                }
            }
        }
        Ok(outcome(inserted, false))
    }

    async fn h_promotion(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Promotions.key();
        let s = sid(row);
        let code = clean_line(&row.text("code"), 64);
        if code.is_empty() || code.contains(char::is_whitespace) {
            return Ok(self.skip(
                Entity::Promotions,
                row,
                "error",
                "invalid_code",
                "promotion code is empty or contains spaces",
            ));
        }
        let Some(discount) = map::discount_type(&row.text("type")) else {
            return Ok(self.skip(
                Entity::Promotions,
                row,
                "error",
                "unknown_type",
                format!(
                    "promotion type {:?} has no HostingOS counterpart",
                    row.text("type")
                ),
            ));
        };
        let (id, existing, adopted) = self
            .resolve(conn, key, &s, Some((NaturalKey::CouponCode, &code)))
            .await?;
        if adopted {
            self.record(conn, key, &s, "coupons", id, false, true)
                .await?;
            return Ok(Outcome::Adopted);
        }
        let mut value = non_negative(row.dec("value"));
        if discount == "percent" {
            value = value.min(Decimal::ONE_HUNDRED);
        }
        let applies = map::applies_to(&row.text("appliesto"));
        let mut product_ids = Vec::new();
        for p in &applies.products {
            if let Some(m) = self.lookup(Entity::Products.key(), &p.to_string()).await? {
                product_ids.push(m.target_id);
            }
        }
        let mut addon_ids = Vec::new();
        for a in &applies.addons {
            if let Some(m) = self.lookup(Entity::Addons.key(), &a.to_string()).await? {
                addon_ids.push(m.target_id);
            }
        }
        let expires = row.date("expirationdate");
        let input = w::CouponInput {
            code,
            description: opt_line(row.str("notes"), LINE),
            discount_type: discount.into(),
            value,
            currency: matches!(discount, "fixed" | "price_override")
                .then(|| self.refdata.default_currency.clone()),
            product_ids,
            addon_ids,
            tlds: applies.tlds,
            billing_cycles: map::promo_cycles(&row.text("cycles")),
            recurring: row.flag("recurring"),
            recurring_limit: i32_of(row.int("recurfor")).max(0),
            new_clients_only: row.flag("newsignups"),
            once_per_client: row.flag("onceperclient"),
            max_uses: row
                .int("maxuses")
                .filter(|m| *m > 0)
                .and_then(|m| i32::try_from(m).ok()),
            uses_count: i32_of(row.int("uses")).max(0),
            starts_at: row.datetime("startdate"),
            expires_at: expires
                .and_then(|d| d.and_hms_opt(23, 59, 59))
                .map(|d| d.and_utc()),
            is_active: expires.is_none_or(|d| d >= self.today),
        };
        if input.currency.is_some() {
            self.note_once(
                "promo_currency",
                key,
                "info",
                "fixed-amount promotions use the default currency",
            );
        }
        let inserted = if self.dry {
            !existing
        } else {
            w::upsert_coupon(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "coupons", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_tld(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Tlds.key();
        let tld = tld_key(&row.text("extension"));
        if !valid_tld(&tld) {
            return Ok(self.skip(
                Entity::Tlds,
                row,
                "error",
                "invalid_tld",
                format!("invalid extension {tld:?}"),
            ));
        }
        let (id, existing, adopted) = self
            .resolve(conn, key, &tld, Some((NaturalKey::Tld, &tld)))
            .await?;
        if adopted {
            self.record(conn, key, &tld, "tlds", id, false, true)
                .await?;
            return Ok(Outcome::Adopted);
        }
        let registrar = row.text("autoreg");
        let input = w::TldInput {
            tld: tld.clone(),
            registrar_integration_id: (!registrar.is_empty())
                .then(|| self.maps.registrar(&registrar))
                .flatten(),
            requires_epp: row.flag("eppcode"),
            supports_privacy: row.flag("idprotection"),
            supports_dns_management: row.flag("dnsmanagement"),
            grace_period_days: i32_of(row.int("grace_period")).max(0),
            redemption_period_days: i32_of(row.int("redemption_grace_period")).max(0),
            sort_order: i32_of(row.int("order")),
        };
        let inserted = if self.dry {
            !existing
        } else {
            w::upsert_tld(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &tld, "tlds", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_pricing(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let kind = row.text("type").to_ascii_lowercase();
        let currency = self.currency_of(row.int("currency"));
        let relid = row.reference("relid").unwrap_or(0).to_string();
        match kind.as_str() {
            "product" | "addon" | "configoptions" => {
                let (entity, target, pay_key) = match kind.as_str() {
                    "product" => (
                        Entity::Products,
                        PriceTarget::Product,
                        Some(format!("p{relid}")),
                    ),
                    "addon" => (
                        Entity::Addons,
                        PriceTarget::Addon,
                        Some(format!("a{relid}")),
                    ),
                    _ => (Entity::ConfigValues, PriceTarget::ConfigValue, None),
                };
                let Some(owner) = self.lookup(entity.key(), &relid).await? else {
                    return Ok(Outcome::Skipped);
                };
                if owner.adopted {
                    return Ok(Outcome::Skipped);
                }
                let pay = pay_key
                    .and_then(|k| self.product_pay_type.get(&k).cloned())
                    .unwrap_or_else(|| "recurring".into());
                if self.dry || pay == "free" {
                    return Ok(Outcome::Updated);
                }
                let setup = |col: &str| non_negative(row.dec(col));
                if pay == "one_time" {
                    let p = map::price(row.dec("monthly")).map(|p| (p, setup("msetupfee")));
                    w::upsert_price(
                        conn,
                        self.tenant,
                        target,
                        owner.target_id,
                        &currency,
                        "one_time",
                        p,
                    )
                    .await?;
                } else {
                    for (cycle, pcol, scol) in map::RECURRING_PRICE_COLUMNS {
                        let p = map::price(row.dec(pcol)).map(|p| (p, setup(scol)));
                        w::upsert_price(
                            conn,
                            self.tenant,
                            target,
                            owner.target_id,
                            &currency,
                            cycle,
                            p,
                        )
                        .await?;
                    }
                }
                Ok(Outcome::Updated)
            }
            "domainregister" | "domaintransfer" | "domainrenew" => {
                let tld = tld_key(
                    &row.str("tld")
                        .or_else(|| row.str("tld_extension"))
                        .unwrap_or_default(),
                );
                let Some(t) = self.lookup(Entity::Tlds.key(), &tld).await? else {
                    return Ok(Outcome::Skipped);
                };
                if t.adopted {
                    self.note_once(
                        "tld_prices_kept",
                        "pricing",
                        "info",
                        "TLDs that already existed keep their HostingOS prices",
                    );
                    return Ok(Outcome::Skipped);
                }
                if self.dry {
                    return Ok(Outcome::Updated);
                }
                let action = &kind["domain".len()..];
                for (years, col) in map::DOMAIN_PRICE_COLUMNS {
                    w::upsert_tld_price(
                        conn,
                        self.tenant,
                        t.target_id,
                        &currency,
                        action,
                        *years,
                        map::price(row.dec(col)),
                    )
                    .await?;
                }
                Ok(Outcome::Updated)
            }
            _ => Ok(Outcome::Skipped),
        }
    }

    // -----------------------------------------------------------------
    // billing & services
    // -----------------------------------------------------------------

    async fn snapshot(&mut self, conn: &mut PgConnection, client: Uuid) -> ImportResult<Json> {
        Ok(
            match hos_db::repo::clients::get_client(
                &mut *conn,
                self.tenant,
                ClientId::from_uuid(client),
            )
            .await?
            {
                Some(c) => hos_billing::checkout::billing_snapshot(&c),
                None => json!({}),
            },
        )
    }

    async fn h_invoice(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Invoices.key();
        let s = sid(row);
        let Some(client) = self.reference(Entity::Clients.key(), row, "userid").await? else {
            return Ok(self.skip(
                Entity::Invoices,
                row,
                "warning",
                "parent_missing",
                "client not imported",
            ));
        };
        let Some(status) = map::invoice_status(&row.text("status")) else {
            return Ok(self.skip(
                Entity::Invoices,
                row,
                "error",
                "unknown_status",
                format!("invoice status {:?}", row.text("status")),
            ));
        };
        let (id, existing) = self.target(key, &s).await?;
        let (mut number, mut seq) = map::invoice_number(row.str("invoicenum").as_deref(), row.id());
        if let Some(other) =
            w::find_id(conn, self.tenant, NaturalKey::InvoiceNumber, &number).await?
            && other != id
        {
            self.note(
                key,
                "conflict",
                Some(&s),
                "number_taken",
                format!("invoice number {number} already exists; imported as WHMCS-{number}"),
            );
            number = format!("WHMCS-{number}");
            seq = None;
        }
        if let Some(n) = seq {
            self.max_invoice_seq = Some(self.max_invoice_seq.map_or(n, |m| m.max(n)));
        }
        let currency = match row.str("currencycode").map(|c| c.to_ascii_uppercase()) {
            Some(c) if c.len() == 3 => c,
            _ => self.currency_of_client(conn, client).await?,
        };
        let issue = row
            .date("date")
            .or_else(|| row.date("created_at"))
            .unwrap_or(self.today);
        let due = row.date("duedate").unwrap_or(issue).max(issue);
        let subtotal = row.money("subtotal");
        let tax1 = row.money("tax");
        let tax2 = row.money("tax2");
        let credit = non_negative(row.dec("credit"));
        let total_due = row.money("total");
        // WHMCS `total` is after applied credit; HostingOS keeps credit separately.
        let total = (subtotal + tax1 + tax2)
            .max(total_due + credit)
            .max(Decimal::ZERO);
        let paid = matches!(status, "paid" | "refunded");
        let mut tax_lines = Vec::new();
        for (n, rate_col, amount) in [(1, "taxrate", tax1), (2, "taxrate2", tax2)] {
            let rate = row.dec(rate_col).unwrap_or_default();
            if rate > Decimal::ZERO && rate <= Decimal::ONE_HUNDRED {
                tax_lines.push((
                    format!("Tax {n}"),
                    rate,
                    n == 2 && self.refdata.l2_compound,
                    subtotal,
                    amount,
                ));
            }
        }
        let open = matches!(status, "unpaid" | "payment_pending" | "collections");
        let snapshot = if self.dry {
            json!({})
        } else {
            self.snapshot(conn, client).await?
        };
        let gateway = row.text("paymentmethod");
        let input = w::InvoiceInput {
            client_id: client,
            number: Some(number),
            seq,
            status: status.into(),
            currency,
            issue_date: issue,
            due_date: due,
            paid_at: if paid {
                row.datetime("datepaid")
                    .or_else(|| due.and_hms_opt(0, 0, 0).map(|d| d.and_utc()))
            } else {
                None
            },
            cancelled_at: (status == "cancelled")
                .then(|| row.datetime("date_cancelled").unwrap_or_else(Utc::now)),
            refunded_at: (status == "refunded")
                .then(|| row.datetime("date_refunded").unwrap_or_else(Utc::now)),
            subtotal,
            tax_total: tax1 + tax2,
            total,
            amount_paid: if paid {
                (total - credit).max(Decimal::ZERO)
            } else {
                Decimal::ZERO
            },
            credit_applied: credit.min(total),
            payment_integration_id: (!gateway.is_empty())
                .then(|| self.maps.gateway(&gateway))
                .flatten(),
            billing_snapshot: snapshot,
            notes: opt_line(row.str("notes"), 2000),
            // HostingOS must not add its own late fee to an invoice WHMCS issued.
            late_fee_applied_at: open.then(Utc::now),
            created_at: issue.and_hms_opt(0, 0, 0).map(|d| d.and_utc()),
            tax_lines,
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_invoice(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "invoices", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_order(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Orders.key();
        let s = sid(row);
        let Some(client) = self.reference(Entity::Clients.key(), row, "userid").await? else {
            return Ok(self.skip(
                Entity::Orders,
                row,
                "warning",
                "parent_missing",
                "client not imported",
            ));
        };
        let (id, existing) = self.target(key, &s).await?;
        let mut number = row
            .str("ordernum")
            .map(|n| clean_line(&n, 40))
            .unwrap_or_else(|| s.clone());
        if let Some(other) = w::find_id(conn, self.tenant, NaturalKey::OrderNumber, &number).await?
            && other != id
        {
            number = format!("WHMCS-{number}");
        }
        let status = map::order_status(&row.text("status"));
        let date = row.datetime("date");
        let gateway = row.text("paymentmethod");
        let input = w::OrderInput {
            order_number: number,
            client_id: client,
            status: status.into(),
            currency: self.currency_of_client(conn, client).await?,
            total: non_negative(row.dec("amount")),
            coupon_code: opt_line(row.str("promocode"), 64),
            payment_integration_id: (!gateway.is_empty())
                .then(|| self.maps.gateway(&gateway))
                .flatten(),
            invoice_id: self
                .reference(Entity::Invoices.key(), row, "invoiceid")
                .await?,
            ip: ip(row, "ipaddress"),
            fraud_details: json!({
                "module": row.str("fraudmodule"),
                "output": row.str("fraudoutput").map(|o| clean_text(&o, 4000)),
            }),
            notes: opt_line(row.str("notes"), 2000),
            accepted_at: (status == "accepted").then_some(date).flatten(),
            cancelled_at: (status == "cancelled").then_some(date).flatten(),
            created_at: date,
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_order(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "orders", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    /// Suspension protection: WHMCS's own override, extended by the grace period.
    fn suspend_override(&self, row: &Row) -> Option<NaiveDate> {
        let whmcs = row.flag("overideautosuspend").then(|| {
            row.date("overidesuspenduntil")
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(2099, 12, 31).unwrap_or(self.today))
        });
        let grace = (self.opts.grace_days > 0)
            .then(|| self.today + chrono::Days::new(u64::from(self.opts.grace_days)));
        whmcs.max(grace)
    }

    async fn h_service(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Services.key();
        let s = sid(row);
        let Some(client) = self.reference(Entity::Clients.key(), row, "userid").await? else {
            return Ok(self.skip(
                Entity::Services,
                row,
                "warning",
                "parent_missing",
                "client not imported",
            ));
        };
        let Some(product) = self
            .reference(Entity::Products.key(), row, "packageid")
            .await?
        else {
            return Ok(self.skip(
                Entity::Services,
                row,
                "error",
                "product_missing",
                "product not imported or not mapped",
            ));
        };
        let Some(status) = map::service_status(&row.text("domainstatus")) else {
            return Ok(self.skip(
                Entity::Services,
                row,
                "error",
                "unknown_status",
                format!("service status {:?}", row.text("domainstatus")),
            ));
        };
        let Some(cycle) = map::billing_cycle(&row.text("billingcycle")) else {
            return Ok(self.skip(
                Entity::Services,
                row,
                "error",
                "unknown_cycle",
                format!("billing cycle {:?}", row.text("billingcycle")),
            ));
        };
        let (id, existing) = self.target(key, &s).await?;
        let password_enc = if self.opts.service_passwords && !self.dry {
            match self.secret("services", row, "password") {
                Some(p) => Some(hos_billing::secrets::seal(
                    self.secret_box,
                    &p,
                    &hos_billing::secrets::service_password_aad(
                        self.tenant,
                        ServiceId::from_uuid(id),
                    ),
                )?),
                None => None,
            }
        } else {
            None
        };
        let regdate = row
            .date("regdate")
            .or_else(|| row.date("created_at"))
            .unwrap_or(self.today);
        let one_time = cycle == "one_time";
        let next_due = if one_time {
            None
        } else {
            row.date("nextduedate")
        };
        let changed = row
            .datetime("updated_at")
            .or_else(|| row.datetime("lastupdate"));
        let input = w::ServiceInput {
            client_id: client,
            product_id: product,
            order_id: self.reference(Entity::Orders.key(), row, "orderid").await?,
            status: status.into(),
            billing_cycle: cycle.into(),
            first_payment_amount: non_negative(row.dec("firstpaymentamount")),
            recurring_amount: non_negative(row.dec("amount")),
            currency: self.currency_of_client(conn, client).await?,
            quantity: i32_of(row.int("qty")).max(1),
            registration_date: regdate,
            next_due_date: next_due,
            next_invoice_date: if one_time {
                None
            } else {
                row.date("nextinvoicedate")
            },
            integration_id: self.reference(Entity::Servers.key(), row, "server").await?,
            external_id: None,
            username: opt_line(row.str("username"), 120),
            password_enc,
            domain: opt_line(row.str("domain"), 253).map(|d| d.to_ascii_lowercase()),
            server_hostname: opt_line(row.str("server_hostname"), 253),
            dedicated_ip: ip(row, "dedicatedip"),
            assigned_ips: map::ip_list(&row.text("assignedips")),
            module_data: json!({ "whmcs": {
                "subscription_id": row.str("subscriptionid"),
                "ns1": row.str("ns1"),
                "ns2": row.str("ns2"),
                "promo_id": row.reference("promoid"),
            }}),
            admin_notes: Some(clean_text(&row.text("notes"), BODY)).filter(|n| !n.is_empty()),
            override_suspend_until: if matches!(status, "active" | "suspended" | "pending") {
                self.suspend_override(row)
            } else {
                None
            },
            suspended_at: (status == "suspended").then(|| changed.unwrap_or_else(Utc::now)),
            suspended_reason: if status == "suspended" {
                opt_line(row.str("suspendreason"), LINE)
            } else {
                None
            },
            terminated_at: (status == "terminated").then(|| {
                row.datetime("termination_date")
                    .or(row.datetime("completed_date"))
                    .or(changed)
                    .unwrap_or_else(Utc::now)
            }),
            cancelled_at: (status == "cancelled").then(|| changed.unwrap_or_else(Utc::now)),
            created_at: row
                .datetime("created_at")
                .or_else(|| regdate.and_hms_opt(0, 0, 0).map(|d| d.and_utc())),
        };
        let counts = matches!(status, "pending" | "active" | "suspended");
        let inserted = if self.dry {
            existing.is_none()
        } else {
            if existing.is_none() && counts && !self.within_limit("max_services") {
                return Ok(self.skip(
                    Entity::Services,
                    row,
                    "error",
                    "plan_limit",
                    "plan limit reached: services",
                ));
            }
            w::upsert_service(conn, self.tenant, id, &input).await?
        };
        self.service_cycle.insert(id, cycle.into());
        self.record(conn, key, &s, "services", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_service_option(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let Some(service) = self.reference(Entity::Services.key(), row, "relid").await? else {
            return Ok(Outcome::Skipped);
        };
        let option = self
            .reference(Entity::ConfigOptions.key(), row, "configid")
            .await?;
        let value = self
            .reference(Entity::ConfigValues.key(), row, "optionid")
            .await?;
        let qty = row.int("qty").unwrap_or(0);
        let opt_type = row.text("opt_type");
        let yes_no = opt_type == "3" || opt_type.eq_ignore_ascii_case("yesno");
        if yes_no && qty == 0 {
            return Ok(Outcome::Skipped);
        }
        if self.dry {
            return Ok(Outcome::Updated);
        }
        match (option, value) {
            (Some(o), Some(v)) => {
                let mut sel = Map::new();
                sel.insert("value_id".into(), json!(v.to_string()));
                if opt_type == "4" || opt_type.eq_ignore_ascii_case("quantity") {
                    sel.insert("quantity".into(), json!(qty.max(0)));
                }
                let mut m = Map::new();
                m.insert(o.to_string(), Json::Object(sel));
                w::merge_config_selection(conn, self.tenant, service, &Json::Object(m)).await?;
            }
            _ => {
                // Not mapped to the catalog (API source / deleted option):
                // keep the selection readable on the service.
                let name = map::split_pipe(&clean_line(&row.text("opt_name"), NAME)).1;
                if name.is_empty() {
                    return Ok(Outcome::Skipped);
                }
                let shown = if opt_type == "4" || opt_type.eq_ignore_ascii_case("quantity") {
                    qty.to_string()
                } else {
                    map::split_pipe(&clean_line(&row.text("sub_name"), NAME)).1
                };
                let mut m = Map::new();
                m.insert(format!("Option: {name}"), json!(shown));
                w::merge_custom_fields(conn, self.tenant, true, service, &Json::Object(m)).await?;
            }
        }
        Ok(Outcome::Updated)
    }

    async fn h_custom_field(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let kind = row.text("cf_type").to_ascii_lowercase();
        let (entity, service) = match kind.as_str() {
            "client" => (Entity::Clients, false),
            "product" => (Entity::Services, true),
            _ => return Ok(Outcome::Skipped),
        };
        let Some(owner) = self.reference(entity.key(), row, "relid").await? else {
            return Ok(Outcome::Skipped);
        };
        if row.text("cf_fieldtype").eq_ignore_ascii_case("password") {
            self.note_once(
                "cf_password",
                "custom_field_values",
                "warning",
                "password-type custom fields are not imported",
            );
            return Ok(Outcome::Skipped);
        }
        let value = clean_text(&row.text("value"), 2000);
        if value.is_empty() {
            return Ok(Outcome::Skipped);
        }
        let name = map::split_pipe(&clean_line(&row.text("cf_name"), NAME)).1;
        let name = if name.is_empty() {
            format!("field_{}", row.int("fieldid").unwrap_or(0))
        } else {
            name
        };
        if !self.dry {
            let mut m = Map::new();
            m.insert(name, json!(value));
            w::merge_custom_fields(conn, self.tenant, service, owner, &Json::Object(m)).await?;
        }
        Ok(Outcome::Updated)
    }

    async fn custom_addon(&mut self, conn: &mut PgConnection, name: &str) -> ImportResult<Uuid> {
        let base = slugify(name);
        let base = if base.is_empty() {
            "custom-addon".to_owned()
        } else {
            base
        };
        let (id, existing) = self.target(derived::CUSTOM_ADDON, &base).await?;
        if existing.is_none() && !self.dry {
            let slug = w::unique_slug(conn, self.tenant, SlugTable::ProductAddons, &base, None, id)
                .await?;
            let input = w::AddonInput {
                slug,
                name: name.to_owned(),
                description: None,
                pay_type: "recurring".into(),
                auto_activate: false,
                show_on_order: false,
                is_taxable: true,
                is_hidden: true,
                sort_order: 0,
            };
            let ins = w::upsert_addon(conn, self.tenant, id, &input).await?;
            self.record(
                conn,
                derived::CUSTOM_ADDON,
                &base,
                "product_addons",
                id,
                ins,
                false,
            )
            .await?;
        } else if existing.is_none() {
            self.pending
                .insert((derived::CUSTOM_ADDON, base), (id, false));
        }
        Ok(id)
    }

    async fn h_service_addon(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::ServiceAddons.key();
        let s = sid(row);
        let Some(service) = self
            .reference(Entity::Services.key(), row, "hostingid")
            .await?
        else {
            return Ok(self.skip(
                Entity::ServiceAddons,
                row,
                "warning",
                "parent_missing",
                "service not imported",
            ));
        };
        let Some(status) = map::service_status(&row.text("status")) else {
            return Ok(self.skip(
                Entity::ServiceAddons,
                row,
                "error",
                "unknown_status",
                format!("addon status {:?}", row.text("status")),
            ));
        };
        let Some(cycle) = map::billing_cycle(&row.text("billingcycle")) else {
            return Ok(self.skip(
                Entity::ServiceAddons,
                row,
                "error",
                "unknown_cycle",
                format!("billing cycle {:?}", row.text("billingcycle")),
            ));
        };
        let addon = match self.reference(Entity::Addons.key(), row, "addonid").await? {
            Some(a) => a,
            None => {
                let name = clean_line(&row.text("name"), NAME);
                let name = if name.is_empty() {
                    "Custom addon".to_owned()
                } else {
                    name
                };
                self.custom_addon(conn, &name).await?
            }
        };
        let client = self.reference(Entity::Clients.key(), row, "userid").await?;
        let currency = match client {
            Some(c) => self.currency_of_client(conn, c).await?,
            None => self.refdata.default_currency.clone(),
        };
        let (id, existing) = self.target(key, &s).await?;
        let regdate = row.date("regdate").unwrap_or(self.today);
        let one_time = cycle == "one_time";
        let input = w::ServiceAddonInput {
            service_id: service,
            addon_id: addon,
            order_id: self.reference(Entity::Orders.key(), row, "orderid").await?,
            status: status.into(),
            billing_cycle: cycle.into(),
            setup_fee: non_negative(row.dec("setupfee")),
            recurring_amount: non_negative(row.dec("recurring")),
            currency,
            registration_date: regdate,
            next_due_date: if one_time {
                None
            } else {
                row.date("nextduedate")
            },
            next_invoice_date: if one_time {
                None
            } else {
                row.date("nextinvoicedate")
            },
            notes: Some(clean_text(&row.text("notes"), BODY)).filter(|n| !n.is_empty()),
            terminated_at: (status == "terminated")
                .then(|| row.datetime("termination_date").unwrap_or_else(Utc::now)),
            created_at: regdate.and_hms_opt(0, 0, 0).map(|d| d.and_utc()),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_service_addon(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "service_addons", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_cancel_request(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let Some(service) = self.reference(Entity::Services.key(), row, "relid").await? else {
            return Ok(Outcome::Skipped);
        };
        if !self.dry {
            let kind = if row
                .text("type")
                .to_ascii_lowercase()
                .starts_with("immediate")
            {
                "immediate"
            } else {
                "end_of_period"
            };
            let reason = opt_line(row.str("reason"), 2000);
            let at = row.datetime("date").unwrap_or_else(Utc::now);
            w::set_cancellation(conn, self.tenant, service, at, kind, reason.as_deref()).await?;
        }
        Ok(Outcome::Updated)
    }

    async fn h_domain(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Domains.key();
        let s = sid(row);
        let Some(client) = self.reference(Entity::Clients.key(), row, "userid").await? else {
            return Ok(self.skip(
                Entity::Domains,
                row,
                "warning",
                "parent_missing",
                "client not imported",
            ));
        };
        let name = row
            .text("domain")
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let Some((_, tld)) = name.split_once('.') else {
            return Ok(self.skip(
                Entity::Domains,
                row,
                "error",
                "invalid_domain",
                format!("invalid domain {name:?}"),
            ));
        };
        if !valid_tld(tld) || name.len() > 253 || name.contains(char::is_whitespace) {
            return Ok(self.skip(
                Entity::Domains,
                row,
                "error",
                "invalid_domain",
                format!("invalid domain {name:?}"),
            ));
        }
        let Some(status) = map::domain_status(&row.text("status")) else {
            return Ok(self.skip(
                Entity::Domains,
                row,
                "error",
                "unknown_status",
                format!("domain status {:?}", row.text("status")),
            ));
        };
        let (id, existing) = self.target(key, &s).await?;
        let live = !matches!(
            status,
            "expired" | "transferred_away" | "cancelled" | "fraud"
        );
        if existing.is_none()
            && live
            && w::find_id(conn, self.tenant, NaturalKey::LiveDomainName, &name)
                .await?
                .is_some()
        {
            return Ok(self.skip(
                Entity::Domains,
                row,
                "conflict",
                "domain_exists",
                format!("{name} already exists in HostingOS"),
            ));
        }
        let registrar = row.text("registrar");
        let registrar_id = (!registrar.is_empty())
            .then(|| self.maps.registrar(&registrar))
            .flatten();
        if !registrar.is_empty() && registrar_id.is_none() {
            self.note_once(
                "registrar_unmapped",
                key,
                "warning",
                "domains of unmapped registrars are imported without a registrar integration",
            );
        }
        let mut nameservers: Vec<String> = Vec::new();
        for n in row.text("order_nameservers").split([',', '\n', ' ', ';']) {
            if let Some(ns) = map::nameserver(n)
                && !nameservers.contains(&ns)
            {
                nameservers.push(ns);
            }
        }
        let input = w::DomainInput {
            client_id: client,
            order_id: self.reference(Entity::Orders.key(), row, "orderid").await?,
            tld: tld.to_owned(),
            name: name.clone(),
            registrar_integration_id: registrar_id,
            status: status.into(),
            registration_type: if row.text("type").eq_ignore_ascii_case("transfer") {
                "transfer".into()
            } else {
                "register".into()
            },
            registration_period: i16::try_from(
                row.int("registrationperiod").unwrap_or(1).clamp(1, 10),
            )
            .unwrap_or(1),
            registered_at: row.datetime("registrationdate"),
            expires_at: row.datetime("expirydate"),
            next_due_date: row.date("nextduedate"),
            next_invoice_date: row.date("nextinvoicedate"),
            first_payment_amount: non_negative(row.dec("firstpaymentamount")),
            recurring_amount: non_negative(row.dec("recurringamount")),
            currency: self.currency_of_client(conn, client).await?,
            auto_renew: !row.flag("donotrenew"),
            privacy: row.flag("idprotection"),
            dns_management: row.flag("dnsmanagement"),
            email_forwarding: row.flag("emailforwarding"),
            nameservers,
            admin_notes: Some(clean_text(&row.text("additionalnotes"), BODY))
                .filter(|n| !n.is_empty()),
            created_at: row
                .datetime("created_at")
                .or_else(|| row.datetime("registrationdate")),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_domain(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "domains", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_invoice_item(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::InvoiceItems.key();
        let s = sid(row);
        let Some(invoice) = self
            .reference(Entity::Invoices.key(), row, "invoiceid")
            .await?
        else {
            return Ok(self.skip(
                Entity::InvoiceItems,
                row,
                "warning",
                "parent_missing",
                "invoice not imported",
            ));
        };
        let raw_type = row.text("type");
        let item_type = map::invoice_item_type(&raw_type);
        let lower = raw_type.to_ascii_lowercase();
        let mut service_id = None;
        let mut addon_id = None;
        let mut domain_id = None;
        match lower.as_str() {
            "hosting" | "setup" | "promohosting" | "upgrade" => {
                service_id = self.reference(Entity::Services.key(), row, "relid").await?;
            }
            "addon" => {
                addon_id = self
                    .reference(Entity::ServiceAddons.key(), row, "relid")
                    .await?
            }
            t if t.starts_with("domain") || t == "promodomain" => {
                domain_id = self.reference(Entity::Domains.key(), row, "relid").await?;
            }
            _ => {}
        }
        // The renewal period an item pays for: lets HostingOS see that the
        // period WHMCS already invoiced must not be invoiced again.
        let (mut start, mut end) = (None, None);
        if lower == "hosting"
            && let Some(svc) = service_id
            && let Some(due) = row.date("duedate")
        {
            let cycle = match row
                .str("service_cycle")
                .and_then(|c| map::billing_cycle(&c))
            {
                Some(c) => Some(c.to_owned()),
                None => match self.service_cycle.get(&svc).cloned() {
                    Some(c) => Some(c),
                    None => w::service_cycle(conn, self.tenant, svc).await?,
                },
            };
            if let Some(c) = cycle {
                start = Some(due);
                end = map::period_end(due, &c);
            }
        }
        let (id, existing) = self.target(key, &s).await?;
        let input = w::InvoiceItemInput {
            invoice_id: invoice,
            item_type: item_type.into(),
            service_id,
            service_addon_id: addon_id,
            domain_id,
            description: {
                let d = clean_text(&row.text("description"), 2000);
                if d.is_empty() { raw_type.clone() } else { d }
            },
            amount: row.money("amount"),
            taxable: row.flag("taxed"),
            period_start: start,
            period_end: end,
            sort_order: i32_of(row.int("id")),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_invoice_item(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "invoice_items", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_transaction(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Transactions.key();
        let s = sid(row);
        let Some(client) = self.reference(Entity::Clients.key(), row, "userid").await? else {
            return Ok(self.skip(
                Entity::Transactions,
                row,
                "warning",
                "parent_missing",
                "client not imported (or no client)",
            ));
        };
        let amount_in = row.money("amountin");
        let amount_out = row.money("amountout");
        let (kind, amount) = if amount_in > Decimal::ZERO {
            ("payment", amount_in)
        } else if amount_out > Decimal::ZERO {
            ("refund", amount_out)
        } else {
            return Ok(self.skip(
                Entity::Transactions,
                row,
                "info",
                "zero",
                "zero-amount transaction",
            ));
        };
        let module = row.text("gateway");
        let integration = (!module.is_empty())
            .then(|| self.maps.gateway(&module))
            .flatten();
        let (id, existing) = self.target(key, &s).await?;
        let mut txn = opt_line(row.str("transid"), 200);
        if let Some(t) = txn.clone()
            && w::gateway_txn_taken(conn, self.tenant, integration, &t, id).await?
        {
            txn = None;
            self.note(
                key,
                "warning",
                Some(&s),
                "duplicate_transid",
                format!("gateway transaction id {t} already used; stored in the description"),
            );
        }
        let currency = match row.int("currency").filter(|c| *c > 0) {
            Some(c) => self.currency_of(Some(c)),
            None => self.currency_of_client(conn, client).await?,
        };
        let invoice = self
            .reference(Entity::Invoices.key(), row, "invoiceid")
            .await?;
        let input = w::TransactionInput {
            client_id: client,
            invoice_id: invoice,
            integration_id: integration,
            gateway: map::gateway_kind(&module),
            gateway_txn_id: txn,
            kind: kind.into(),
            amount,
            fee: row.money("fees").abs(),
            currency: currency.clone(),
            description: opt_line(row.str("description"), LINE),
            raw: json!({ "whmcs": { "gateway": module, "transid": row.str("transid"), "rate": row.str("rate") } }),
            occurred_at: row.datetime("date").unwrap_or_else(Utc::now),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_transaction(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "transactions", id, inserted, false)
            .await?;
        if kind == "refund"
            && let Some(original) = self
                .reference(Entity::Transactions.key(), row, "refundid")
                .await?
            && !self.dry
        {
            let (rid, _) = self.target(derived::REFUND, &s).await?;
            let refund = w::RefundInput {
                client_id: client,
                transaction_id: original,
                invoice_id: invoice,
                refund_transaction_id: id,
                integration_id: integration,
                amount,
                currency,
                processed_at: input.occurred_at,
            };
            let ins = w::upsert_refund(conn, self.tenant, rid, &refund).await?;
            self.record(conn, derived::REFUND, &s, "refunds", rid, ins, false)
                .await?;
        }
        Ok(outcome(inserted, false))
    }

    // -----------------------------------------------------------------
    // support
    // -----------------------------------------------------------------

    async fn h_department(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::Departments.key();
        let s = sid(row);
        let name = clean_line(&row.text("name"), NAME);
        let name = if name.is_empty() {
            format!("Department {s}")
        } else {
            name
        };
        let base = slugify(&name);
        let base = if base.is_empty() {
            format!("department-{s}")
        } else {
            base
        };
        let (id, existing, adopted) = self
            .resolve(conn, key, &s, Some((NaturalKey::DepartmentSlug, &base)))
            .await?;
        if adopted {
            self.record(conn, key, &s, "ticket_departments", id, false, true)
                .await?;
            return Ok(Outcome::Adopted);
        }
        let slug = match (existing, self.dry) {
            (true, false) => w::current_slug(conn, self.tenant, SlugTable::TicketDepartments, id)
                .await?
                .unwrap_or(base),
            _ => base,
        };
        let input = w::DepartmentInput {
            slug,
            name,
            description: opt_line(row.str("description"), LINE),
            email: row.str("email").and_then(|e| map::email(&e)),
            clients_only: row.flag("clientsonly"),
            auto_respond: !row.flag("noautoresponder"),
            is_hidden: row.flag("hidden"),
            sort_order: i32_of(row.int("order")),
        };
        let inserted = if self.dry {
            !existing
        } else {
            w::upsert_department(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "ticket_departments", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_ticket(
        &mut self,
        conn: &mut PgConnection,
        source: &mut dyn Source,
        row: &Row,
    ) -> ImportResult<Outcome> {
        let key = Entity::Tickets.key();
        let s = sid(row);
        let Some(dept) = self
            .reference(Entity::Departments.key(), row, "did")
            .await?
            .or(self
                .reference(Entity::Departments.key(), row, "deptid")
                .await?)
        else {
            return Ok(self.skip(
                Entity::Tickets,
                row,
                "warning",
                "parent_missing",
                "department not imported",
            ));
        };
        let client = self.reference(Entity::Clients.key(), row, "userid").await?;
        let guest_email = row.str("email").and_then(|e| map::email(&e));
        if client.is_none() && guest_email.is_none() {
            return Ok(self.skip(
                Entity::Tickets,
                row,
                "warning",
                "no_requester",
                "ticket has neither an imported client nor a valid e-mail",
            ));
        }
        let (id, existing) = self.target(key, &s).await?;
        let mut number = row
            .str("tid")
            .map(|t| clean_line(&t, 40))
            .unwrap_or_else(|| s.clone());
        if let Some(other) =
            w::find_id(conn, self.tenant, NaturalKey::TicketNumber, &number).await?
            && other != id
        {
            number = format!("WHMCS-{number}");
        }
        let (status, known) = map::ticket_status(&row.text("status"));
        if !known {
            self.note(
                key,
                "info",
                Some(&s),
                "custom_status",
                format!("custom status {:?} imported as open", row.text("status")),
            );
        }
        let created = row.datetime("date").unwrap_or_else(Utc::now);
        let last = row.datetime("lastreply").unwrap_or(created).max(created);
        let (mut service_id, mut domain_id) = (None, None);
        match row.str("service").as_deref().and_then(map::ticket_ref) {
            Some(map::TicketRef::Service(i)) => {
                service_id = self
                    .lookup(Entity::Services.key(), &i.to_string())
                    .await?
                    .map(|m| m.target_id)
            }
            Some(map::TicketRef::Domain(i)) => {
                domain_id = self
                    .lookup(Entity::Domains.key(), &i.to_string())
                    .await?
                    .map(|m| m.target_id)
            }
            None => {}
        }
        let cc: Vec<String> = row
            .text("cc")
            .split([',', ';', ' '])
            .filter_map(map::email)
            .collect();
        let by_staff = status == "answered";
        let input = w::TicketInput {
            number,
            department_id: dept,
            client_id: client,
            user_id: None,
            guest_name: if client.is_none() {
                opt_line(row.str("name"), NAME)
            } else {
                None
            },
            guest_email: if client.is_none() {
                guest_email.clone()
            } else {
                None
            },
            subject: {
                let t = clean_line(&row.text("title"), LINE);
                if t.is_empty() {
                    format!("Ticket {s}")
                } else {
                    t
                }
            },
            status: status.into(),
            priority: map::ticket_priority(&row.text("urgency")).into(),
            service_id,
            domain_id,
            assigned_to: self.reference(Entity::Staff.key(), row, "flag").await?,
            last_reply_at: last,
            last_reply_by: if by_staff {
                "staff".into()
            } else if client.is_some() {
                "client".into()
            } else {
                "guest".into()
            },
            closed_at: (status == "closed").then_some(last),
            cc_emails: cc,
            created_at: created,
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_ticket(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "tickets", id, inserted, false)
            .await?;
        // The opening message.
        let admin = row.text("admin");
        let staff_opened = !admin.is_empty();
        let (mid, _) = self.target(derived::TICKET_FIRST_MESSAGE, &s).await?;
        let msg = w::TicketMessageInput {
            ticket_id: id,
            author_kind: if staff_opened {
                "staff".into()
            } else if client.is_some() {
                "client".into()
            } else {
                "guest".into()
            },
            user_id: None,
            author_name: clean_line(
                &if staff_opened {
                    admin.clone()
                } else {
                    row.text("name")
                },
                NAME,
            ),
            author_email: if staff_opened { None } else { guest_email },
            body: {
                let b = rich_text(&row.text("message"), BODY);
                if b.is_empty() { "—".into() } else { b }
            },
            is_internal: false,
            created_at: created,
        };
        if !self.dry {
            let ins = w::upsert_ticket_message(conn, self.tenant, mid, &msg).await?;
            self.record(
                conn,
                derived::TICKET_FIRST_MESSAGE,
                &s,
                "ticket_messages",
                mid,
                ins,
                false,
            )
            .await?;
        }
        self.attachments(
            conn,
            source,
            AttachmentOwner::Ticket,
            row,
            "attachment",
            id,
            Some(mid),
        )
        .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_ticket_message(
        &mut self,
        conn: &mut PgConnection,
        source: &mut dyn Source,
        row: &Row,
        note: bool,
    ) -> ImportResult<Outcome> {
        let entity = if note {
            Entity::TicketNotes
        } else {
            Entity::TicketReplies
        };
        let key = entity.key();
        let s = sid(row);
        let parent_col = if note { "ticketid" } else { "tid" };
        let Some(ticket) = self
            .reference(Entity::Tickets.key(), row, parent_col)
            .await?
        else {
            return Ok(self.skip(
                entity,
                row,
                "warning",
                "parent_missing",
                "ticket not imported",
            ));
        };
        let admin = row.text("admin");
        let staff = note || !admin.is_empty();
        let client = self.reference(Entity::Clients.key(), row, "userid").await?;
        let (id, existing) = self.target(key, &s).await?;
        let input = w::TicketMessageInput {
            ticket_id: ticket,
            author_kind: if staff {
                "staff".into()
            } else if client.is_some() {
                "client".into()
            } else {
                "guest".into()
            },
            user_id: None,
            author_name: clean_line(
                &if staff {
                    admin.clone()
                } else {
                    row.text("name")
                },
                NAME,
            ),
            author_email: if staff {
                None
            } else {
                row.str("email").and_then(|e| map::email(&e))
            },
            body: {
                let b = rich_text(&row.text("message"), BODY);
                if b.is_empty() { "—".into() } else { b }
            },
            is_internal: note,
            created_at: row.datetime("date").unwrap_or_else(Utc::now),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_ticket_message(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "ticket_messages", id, inserted, false)
            .await?;
        let (owner, col) = if note {
            (AttachmentOwner::Note, "attachments")
        } else {
            (AttachmentOwner::Reply, "attachment")
        };
        self.attachments(conn, source, owner, row, col, ticket, Some(id))
            .await?;
        Ok(outcome(inserted, false))
    }

    /// Imports the attachments listed in `col` (size and type checked).
    #[allow(clippy::too_many_arguments)]
    async fn attachments(
        &mut self,
        conn: &mut PgConnection,
        source: &mut dyn Source,
        owner: AttachmentOwner,
        row: &Row,
        col: &str,
        ticket: Uuid,
        message: Option<Uuid>,
    ) -> ImportResult<()> {
        let names = map::attachment_names(&row.text(col));
        if names.is_empty() || row.flag("attachments_removed") {
            return Ok(());
        }
        let entity = match owner {
            AttachmentOwner::Ticket => "tickets",
            AttachmentOwner::Reply => "ticket_replies",
            AttachmentOwner::Note => "ticket_notes",
        };
        let s = sid(row);
        let max = self.opts.attachment_max_bytes.min(ATTACHMENT_CEILING);
        for (i, (stored, display)) in names.iter().enumerate() {
            let akey = format!("{}:{s}:{i}", owner.api_type());
            if self.lookup(derived::ATTACHMENT, &akey).await?.is_some() {
                continue;
            }
            if self.dry {
                continue;
            }
            let fetched = match source.attachment(owner, row.id(), i, stored, max).await {
                Ok(a) => a,
                Err(e @ (ImportError::Policy(_) | ImportError::Auth(_))) => return Err(e),
                Err(e) => Attachment::Unavailable(e.to_string()),
            };
            match fetched {
                Attachment::Data { filename, bytes } => {
                    let name = clean_filename(if filename.is_empty() {
                        display
                    } else {
                        &filename
                    });
                    let Some(content_type) = sniff(&name, &bytes) else {
                        self.note(
                            entity,
                            "skipped",
                            Some(&s),
                            "attachment_type",
                            format!("{name}: file type not allowed"),
                        );
                        continue;
                    };
                    let sha256 = {
                        use sha2::Digest as _;
                        hex::encode(sha2::Sha256::digest(&bytes))
                    };
                    let id = Uuid::now_v7();
                    let input = w::AttachmentInput {
                        ticket_id: ticket,
                        message_id: message,
                        filename: name,
                        content_type: content_type.into(),
                        content: bytes,
                        sha256,
                    };
                    let ins = w::upsert_attachment(conn, self.tenant, id, &input).await?;
                    self.record(
                        conn,
                        derived::ATTACHMENT,
                        &akey,
                        "ticket_attachments",
                        id,
                        ins,
                        false,
                    )
                    .await?;
                }
                Attachment::TooLarge(size) => {
                    let size = size.map_or_else(String::new, |n| format!(" ({n} bytes)"));
                    self.note(
                        entity,
                        "skipped",
                        Some(&s),
                        "attachment_too_large",
                        format!("{display}{size} exceeds the attachment limit"),
                    );
                }
                Attachment::Missing => {
                    self.note(
                        entity,
                        "warning",
                        Some(&s),
                        "attachment_missing",
                        format!("{display} not found at the source"),
                    );
                }
                Attachment::Unavailable(reason) => {
                    self.note(
                        entity,
                        "skipped",
                        Some(&s),
                        "attachment_unavailable",
                        format!("{display}: {reason}"),
                    );
                }
            }
        }
        Ok(())
    }

    async fn h_canned(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::CannedResponses.key();
        let s = sid(row);
        let title = clean_line(&row.text("name"), NAME);
        let body = rich_text(&row.text("reply"), BODY);
        if title.is_empty() || body.is_empty() {
            return Ok(self.skip(
                Entity::CannedResponses,
                row,
                "info",
                "empty",
                "empty predefined reply",
            ));
        }
        let (id, existing) = self.target(key, &s).await?;
        let locale = self.content_locale();
        let category = opt_line(row.str("category_name"), NAME);
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_canned(
                conn,
                self.tenant,
                id,
                &title,
                &body,
                &locale,
                category.as_deref(),
            )
            .await?
        };
        self.record(conn, key, &s, "canned_responses", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    fn content_locale(&self) -> String {
        match self.opts.content_locale.as_deref() {
            Some("tr") => "tr".into(),
            Some("en") => "en".into(),
            _ => self.refdata.default_locale.clone(),
        }
    }

    async fn h_kb_category(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::KbCategories.key();
        let s = sid(row);
        let name = clean_line(&row.text("name"), NAME);
        let name = if name.is_empty() {
            format!("Category {s}")
        } else {
            name
        };
        let (id, existing) = self.target(key, &s).await?;
        let base = slugify(&name);
        let base = if base.is_empty() {
            format!("category-{s}")
        } else {
            base
        };
        let slug = match (existing.is_some(), self.dry) {
            (true, false) => w::current_slug(conn, self.tenant, SlugTable::KbCategories, id)
                .await?
                .unwrap_or(base),
            _ => {
                w::unique_slug(conn, self.tenant, SlugTable::KbCategories, &base, None, id).await?
            }
        };
        let input = w::KbCategoryInput {
            parent_id: self
                .reference(key, row, "parentid")
                .await?
                .filter(|p| *p != id),
            slug,
            name,
            description: opt_line(row.str("description"), LINE),
            is_hidden: row.flag("hidden"),
            sort_order: i32_of(row.int("id")),
        };
        let inserted = if self.dry {
            existing.is_none()
        } else {
            w::upsert_kb_category(conn, self.tenant, id, &input).await?
        };
        self.record(conn, key, &s, "kb_categories", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_article(
        &mut self,
        conn: &mut PgConnection,
        row: &Row,
        announcement: bool,
    ) -> ImportResult<Outcome> {
        let entity = if announcement {
            Entity::Announcements
        } else {
            Entity::KbArticles
        };
        let key = entity.key();
        let s = sid(row);
        let title = clean_line(&row.text("title"), LINE);
        if title.is_empty() {
            return Ok(self.skip(entity, row, "info", "empty", "untitled entry"));
        }
        let body_col = if announcement {
            "announcement"
        } else {
            "article"
        };
        let locale = self.content_locale();
        let (id, existing) = self.target(key, &s).await?;
        let table = if announcement {
            SlugTable::Announcements
        } else {
            SlugTable::KbArticles
        };
        let base = slugify(&title);
        let base = if base.is_empty() {
            format!("entry-{s}")
        } else {
            base
        };
        let slug = match (existing.is_some(), self.dry) {
            (true, false) => w::current_slug(conn, self.tenant, table, id)
                .await?
                .unwrap_or(base),
            _ => w::unique_slug(conn, self.tenant, table, &base, Some(&locale), id).await?,
        };
        let published = if announcement {
            row.flag("published")
        } else {
            !row.flag("private")
        };
        let date = row.datetime("date").or_else(|| row.datetime("created_at"));
        let input = w::ArticleInput {
            category_id: if announcement {
                None
            } else {
                self.reference(Entity::KbCategories.key(), row, "categoryid")
                    .await?
            },
            slug,
            locale,
            title,
            body: rich_text(&row.text(body_col), BODY),
            status: if published {
                "published".into()
            } else {
                "draft".into()
            },
            published_at: if published {
                Some(date.unwrap_or_else(Utc::now))
            } else {
                None
            },
            sort_order: i32_of(row.int("order")),
            views: row.int("views").unwrap_or(0).max(0),
            helpful_yes: i32_of(row.int("useful")).max(0),
            created_at: date,
        };
        let inserted = if self.dry {
            existing.is_none()
        } else if announcement {
            w::upsert_announcement(conn, self.tenant, id, &input).await?
        } else {
            w::upsert_kb_article(conn, self.tenant, id, &input).await?
        };
        self.record(
            conn,
            key,
            &s,
            if announcement {
                "announcements"
            } else {
                "kb_articles"
            },
            id,
            inserted,
            false,
        )
        .await?;
        Ok(outcome(inserted, false))
    }

    async fn h_activity(&mut self, conn: &mut PgConnection, row: &Row) -> ImportResult<Outcome> {
        let key = Entity::ActivityLog.key();
        let s = sid(row);
        let (id, existing) = self.target(key, &s).await?;
        if existing.is_some() {
            return Ok(Outcome::Updated);
        }
        let metadata = json!({
            "source": "whmcs",
            "description": clean_line(&row.text("description"), 1000),
            "user": opt_line(row.str("user"), NAME),
            "ip": row.str("ipaddr").and_then(|i| i.parse::<IpAddr>().ok()).map(|i| i.to_string()),
            "whmcs_client_id": row.reference("userid"),
        });
        let at = row.datetime("date").unwrap_or_else(Utc::now);
        let inserted = if self.dry {
            true
        } else {
            w::upsert_activity(conn, self.tenant, id, &metadata, at).await?
        };
        self.record(conn, key, &s, "audit_log", id, inserted, false)
            .await?;
        Ok(outcome(inserted, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_types_and_names() {
        assert_eq!(sniff("a.png", b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff("a.pdf", b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff("a.html", b"<script>alert(1)</script>"), None);
        assert_eq!(sniff("a.txt", b"hello"), Some("text/plain; charset=utf-8"));
        assert_eq!(sniff("a.svg", b"<svg onload=alert(1)>"), None);
        assert_eq!(clean_filename("../../etc/passwd"), "passwd");
        assert_eq!(clean_filename("<img src=x>.png"), "_img src_x_.png");
        assert_eq!(clean_filename(".htaccess"), "htaccess");
        assert_eq!(clean_filename(""), "attachment");
    }

    #[test]
    fn tlds() {
        assert_eq!(tld_key(".COM.tr"), "com.tr");
        assert!(valid_tld("com.tr"));
        assert!(!valid_tld("com..tr"));
        assert!(!valid_tld("c m"));
    }
}
