//! Source connectors. Each yields [`Row`]s keyed by WHMCS database column
//! names, page by page, through the [`Source`] trait:
//!
//! * [`mysql::MysqlSource`] — the WHMCS database (read-only `SELECT`s),
//! * [`api::ApiSource`] — the WHMCS API (`includes/api.php`),
//! * [`fixture::FixtureSource`] — in-memory rows (tests, demos).
//!
//! Every connector is read-only on the source.

pub mod api;
pub mod fixture;
pub mod mysql;

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Serialize;

use crate::entity::Entity;
use crate::error::ImportResult;
use crate::row::Row;

/// How the source's values are shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Flavor {
    /// Raw database rows: password hashes present, credentials encrypted
    /// with `cc_encryption_hash`.
    Database,
    /// API output: no password hashes, service passwords (when present)
    /// already decrypted by WHMCS.
    Api,
}

/// A page of rows.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub rows: Vec<Row>,
    /// Cursor of the next page (`None` = done). Opaque to the engine; it is
    /// stored in the run progress so an interrupted run resumes here.
    pub next: Option<i64>,
}

/// Whether a connector can deliver an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Full,
    /// Delivered, with a limitation the report states.
    Partial(&'static str),
    /// Not available through this connector (reported as skipped).
    No(&'static str),
}

/// Result of a connection test.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Probe {
    /// WHMCS version, when the source tells.
    pub version: Option<String>,
    /// Facts worth showing (server version, TLS, API permissions).
    pub notes: Vec<String>,
}

/// The owner of a ticket attachment (`tbltickets`, `tblticketreplies`, `tblticketnotes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentOwner {
    Ticket,
    Reply,
    Note,
}

impl AttachmentOwner {
    /// WHMCS API `type`.
    pub const fn api_type(self) -> &'static str {
        match self {
            Self::Ticket => "ticket",
            Self::Reply => "reply",
            Self::Note => "note",
        }
    }
}

/// An attachment read from the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    Data {
        filename: String,
        bytes: Vec<u8>,
    },
    /// Larger than the limit (size when known).
    TooLarge(Option<u64>),
    /// The source cannot deliver attachments (reason).
    Unavailable(String),
    /// Not found at the source.
    Missing,
}

/// A read-only WHMCS source.
#[async_trait]
pub trait Source: Send {
    /// Value shapes.
    fn flavor(&self) -> Flavor;

    /// Whether `entity` can be delivered.
    fn supports(&self, entity: Entity) -> Support;

    /// Connects and checks credentials / permissions.
    async fn probe(&mut self) -> ImportResult<Probe>;

    /// Rows of `entity` (`None` when the source cannot count cheaply).
    async fn count(&mut self, entity: Entity) -> ImportResult<Option<u64>>;

    /// Rows of a WHMCS table with no HostingOS destination (`None` = unknown).
    async fn count_table(&mut self, table: &str) -> ImportResult<Option<u64>>;

    /// One page of `entity` from `cursor` (0 = start), at most `limit` rows.
    async fn fetch(&mut self, entity: Entity, cursor: i64, limit: u32) -> ImportResult<Batch>;

    /// `tblconfiguration` settings the import needs (default currency,
    /// language, country, invoice numbering).
    async fn settings(&mut self) -> ImportResult<BTreeMap<String, String>>;

    /// One ticket attachment (`index` within the owner's `attachment` list).
    async fn attachment(
        &mut self,
        owner: AttachmentOwner,
        owner_id: i64,
        index: usize,
        stored_name: &str,
        max_bytes: u64,
    ) -> ImportResult<Attachment>;
}

/// `tblconfiguration` keys read by [`Source::settings`].
pub const SETTING_KEYS: &[&str] = &[
    "Language",
    "DefaultCountry",
    "CompanyName",
    "SystemURL",
    "Version",
    "SequentialInvoiceNumbering",
    "SequentialInvoiceNumberFormat",
    "SequentialInvoiceNumberValue",
    "TaxType",
    "TaxL2Compound",
];

/// Shared filter of WHMCS table columns that are **never** read: card and
/// bank data, 2FA secrets, reset tokens, API credentials of staff.
pub const DENIED_COLUMNS: &[(&str, &str)] = &[
    ("tblclients", "cardtype"),
    ("tblclients", "cardlastfour"),
    ("tblclients", "cardnum"),
    ("tblclients", "startdate"),
    ("tblclients", "expdate"),
    ("tblclients", "issuenumber"),
    ("tblclients", "bankname"),
    ("tblclients", "banktype"),
    ("tblclients", "bankcode"),
    ("tblclients", "bankacct"),
    ("tblclients", "gatewayid"),
    ("tblclients", "securityqid"),
    ("tblclients", "securityqans"),
    ("tblclients", "authmodule"),
    ("tblclients", "authdata"),
    ("tblclients", "pwresetkey"),
    ("tblclients", "pwresetexpiry"),
    ("tblusers", "second_factor"),
    ("tblusers", "second_factor_config"),
    ("tblusers", "remember_token"),
    ("tblusers", "reset_token"),
    ("tblusers", "reset_token_expiry"),
    ("tblusers", "security_question_id"),
    ("tblusers", "security_question_answer"),
    ("tbladmins", "authmodule"),
    ("tbladmins", "authdata"),
    ("tbladmins", "password_reset_key"),
    ("tbladmins", "password_reset_data"),
    ("tbladmins", "password_reset_expiry"),
    ("tbladmins", "passwordhash"),
    ("tbladmins", "password"),
    ("tbladmins", "loginattempts"),
    ("tbladmins", "homewidgets"),
    ("tbladmins", "widgetorder"),
    ("tbladmins", "hidden_widgets"),
    ("tblcontacts", "pwresetkey"),
    ("tblcontacts", "pwresetexpiry"),
    ("tblcontacts", "cardtype"),
    ("tblcontacts", "cardlastfour"),
    ("tblcontacts", "cardnum"),
    ("tblcontacts", "startdate"),
    ("tblcontacts", "expdate"),
    ("tblcontacts", "issuenumber"),
    ("tblcontacts", "bankname"),
    ("tblcontacts", "banktype"),
    ("tblcontacts", "bankcode"),
    ("tblcontacts", "bankacct"),
    ("tblorders", "orderdata"),
];

/// Whether `table.column` must never be read.
pub fn denied(table: &str, column: &str) -> bool {
    DENIED_COLUMNS
        .iter()
        .any(|(t, c)| t.eq_ignore_ascii_case(table) && c.eq_ignore_ascii_case(column))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_and_token_columns_are_denied() {
        assert!(denied("tblclients", "cardnum"));
        assert!(denied("TBLCLIENTS", "BankAcct"));
        assert!(denied("tbladmins", "password"));
        assert!(!denied("tblclients", "password"));
        assert!(!denied("tblclients", "email"));
    }
}
