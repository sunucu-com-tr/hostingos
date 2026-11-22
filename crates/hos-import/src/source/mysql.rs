//! WHMCS database connector (MySQL / MariaDB) on `mysql_async`.
//!
//! * **Crate choice.** `sqlx-mysql` depends on `rsa` (RUSTSEC-2023-0071,
//!   Marvin timing side channel); `mysql_async` 0.37 → `mysql_common` 0.38
//!   has no `rsa` dependency (`caching_sha2_password` full authentication
//!   then needs TLS, or the server's `sha256`-less fast path).
//! * **Outbound policy.** The host is resolved once through
//!   [`OutboundPolicy::resolve_host`] (every address must be allowed: no
//!   loopback / link-local / metadata unless `HOS_OUTBOUND_POLICY` says
//!   so) and the connection is pinned to those addresses
//!   (`resolved_ips`), so a DNS answer cannot change between the check and
//!   the connect.
//! * **Read-only.** Only `SELECT`s are issued; the session is additionally
//!   switched to `READ ONLY` where the server supports it.
//! * **Columns.** Discovered per table from `information_schema`, so WHMCS
//!   6–8 schemas all work; card / bank / 2FA / reset-token columns
//!   ([`super::DENIED_COLUMNS`]) are never selected.
//! * **Limits.** Connect and per-query timeouts, keyset pagination with a
//!   bounded page size, `max_allowed_packet` capped. A hostile source cannot
//!   make the web process hold much memory: long columns (`text`, `blob`,
//!   `json` …) are cut on the server (`left(col, MAX_FIELD_CHARS + 1)`, so no
//!   packet carries more) and marked for the report ([`Row::cap`]); rows are
//!   streamed and a batch ends early once it holds [`MAX_BATCH_BYTES`] (the
//!   rest of the page is read and discarded row by row).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use chrono::NaiveDate;
use hos_core::net::OutboundPolicy;
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder, SslOpts};

use super::api::ApiClient;
use super::{
    Attachment, AttachmentOwner, Batch, Flavor, Probe, SETTING_KEYS, Source, Support, denied,
};
use crate::config::{DbSourceConfig, SourceSecrets, TlsMode};
use crate::entity::Entity;
use crate::error::{ImportError, ImportResult};
use crate::row::{MAX_FIELD_CHARS, Row, Value};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const QUERY_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest packet (one row) accepted from the server. Long columns are cut
/// to [`MAX_FIELD_CHARS`] characters (≤ 1 MiB) on the server first.
const MAX_PACKET: usize = 16 * 1024 * 1024;
/// A batch stops taking rows once they hold this much.
pub const MAX_BATCH_BYTES: usize = 32 * 1024 * 1024;
/// Column types whose values can be large.
const LONG_TYPES: &[&str] = &[
    "text",
    "mediumtext",
    "longtext",
    "blob",
    "mediumblob",
    "longblob",
    "json",
];

/// How an entity is read.
struct Spec {
    table: &'static str,
    /// Joins after `from <table> t`.
    joins: &'static str,
    /// Extra select expressions (aliased).
    extras: &'static [&'static str],
    /// Conditions applied when the main table has the column.
    optional_filters: &'static [(&'static str, &'static str)],
    /// Unconditional condition (`""` = none).
    filter: &'static str,
}

const fn spec(table: &'static str) -> Spec {
    Spec {
        table,
        joins: "",
        extras: &[],
        optional_filters: &[],
        filter: "",
    }
}

fn spec_of(entity: Entity) -> Spec {
    let table = entity.info().table;
    match entity {
        Entity::Staff => Spec {
            extras: &["(select r.name from tbladminroles r where r.id = t.roleid) as role_name"],
            ..spec(table)
        },
        Entity::Products => Spec {
            extras: &[
                "(select r.serverid from tblservergroupsrel r where r.groupid = t.servergroup order by r.serverid limit 1) as group_server_id",
            ],
            ..spec(table)
        },
        Entity::Services => Spec {
            extras: &[
                "(select s.hostname from tblservers s where s.id = t.server) as server_hostname",
            ],
            ..spec(table)
        },
        Entity::ServiceOptions => Spec {
            joins: "left join tblproductconfigoptions o on o.id = t.configid \
                    left join tblproductconfigoptionssub s on s.id = t.optionid",
            extras: &[
                "o.optionname as opt_name",
                "o.optiontype as opt_type",
                "s.optionname as sub_name",
            ],
            ..spec(table)
        },
        Entity::CustomFieldValues => Spec {
            joins: "join tblcustomfields f on f.id = t.fieldid",
            extras: &[
                "f.type as cf_type",
                "f.relid as cf_relid",
                "f.fieldname as cf_name",
                "f.fieldtype as cf_fieldtype",
                "f.adminonly as cf_adminonly",
            ],
            filter: "f.type in ('client', 'product')",
            ..spec(table)
        },
        Entity::Domains => Spec {
            extras: &[
                "(select o.nameservers from tblorders o where o.id = t.orderid) as order_nameservers",
            ],
            ..spec(table)
        },
        Entity::Pricing => Spec {
            extras: &[
                "(select d.extension from tbldomainpricing d where d.id = t.relid and t.type like 'domain%') as tld_extension",
            ],
            ..spec(table)
        },
        Entity::InvoiceItems => Spec {
            extras: &[
                "(select h.billingcycle from tblhosting h where h.id = t.relid and t.type = 'Hosting') as service_cycle",
            ],
            ..spec(table)
        },
        Entity::CannedResponses => Spec {
            extras: &[
                "(select c.name from tblticketpredefinedcats c where c.id = t.catid) as category_name",
            ],
            ..spec(table)
        },
        Entity::KbCategories => Spec {
            optional_filters: &[("catid", "t.catid = 0")],
            ..spec(table)
        },
        Entity::KbArticles => Spec {
            extras: &[
                "(select l.categoryid from tblknowledgebaselinks l where l.articleid = t.id order by l.categoryid limit 1) as categoryid",
            ],
            optional_filters: &[("parentid", "t.parentid = 0")],
            ..spec(table)
        },
        Entity::Announcements => Spec {
            optional_filters: &[("parentid", "t.parentid = 0")],
            ..spec(table)
        },
        Entity::AffiliateAccounts => Spec {
            extras: &["(select h.userid from tblhosting h where h.id = t.relid) as service_userid"],
            ..spec(table)
        },
        Entity::AffiliateHistory => Spec {
            extras: &[
                "(select a.relid from tblaffiliatesaccounts a where a.id = t.affaccid) as account_relid",
            ],
            ..spec(table)
        },
        Entity::NetworkIssues => Spec {
            extras: &["(select s.name from tblservers s where s.id = t.server) as server_name"],
            ..spec(table)
        },
        _ => spec(table),
    }
}

fn ident(s: &str) -> ImportResult<&str> {
    if !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(s)
    } else {
        Err(ImportError::Source(format!("unexpected identifier {s:?}")))
    }
}

fn mysql_err(e: &mysql_async::Error) -> String {
    // Server messages can echo the user name, never the password.
    match e {
        mysql_async::Error::Server(s) => format!("MySQL error {}: {}", s.code, s.message),
        other => other.to_string(),
    }
    .chars()
    .take(300)
    .collect()
}

fn is_missing_object(e: &mysql_async::Error) -> bool {
    matches!(e, mysql_async::Error::Server(s) if s.code == 1054 || s.code == 1146)
}

fn is_auth_failure(e: &mysql_async::Error) -> bool {
    matches!(e, mysql_async::Error::Server(s) if matches!(s.code, 1044 | 1045 | 1698 | 1049))
}

fn convert(value: mysql_async::Value, ty: ColumnType, charset: u16) -> Value {
    use mysql_async::Value as M;
    match value {
        M::NULL => Value::Null,
        M::Int(i) => Value::Int(i),
        M::UInt(u) => Value::Int(i64::try_from(u).unwrap_or(i64::MAX)),
        M::Float(f) => Value::Float(f64::from(f)),
        M::Double(f) => Value::Float(f),
        M::Bytes(b) => {
            let binary = charset == 63
                && matches!(
                    ty,
                    ColumnType::MYSQL_TYPE_TINY_BLOB
                        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                        | ColumnType::MYSQL_TYPE_LONG_BLOB
                        | ColumnType::MYSQL_TYPE_BLOB
                        | ColumnType::MYSQL_TYPE_VAR_STRING
                        | ColumnType::MYSQL_TYPE_STRING
                );
            if binary {
                Value::Bytes(b)
            } else {
                Value::Text(String::from_utf8_lossy(&b).into_owned())
            }
        }
        M::Date(y, m, d, h, mi, s, _) => {
            match NaiveDate::from_ymd_opt(i32::from(y), u32::from(m), u32::from(d)) {
                None => Value::Null,
                Some(date) if ty == ColumnType::MYSQL_TYPE_DATE => Value::Date(date),
                Some(date) => date
                    .and_hms_opt(u32::from(h), u32::from(mi), u32::from(s))
                    .map_or(Value::Null, Value::DateTime),
            }
        }
        M::Time(neg, d, h, m, s, _) => Value::Text(format!(
            "{}{:02}:{m:02}:{s:02}",
            if neg { "-" } else { "" },
            u64::from(d) * 24 + u64::from(h)
        )),
    }
}

/// Select expression of a column; a long one is cut on the server, one
/// character past the limit so that [`Row::cap`] notices and reports it.
fn column_expr(c: &str, long: bool) -> String {
    if long {
        format!("left(t.`{c}`, {}) as `{c}`", MAX_FIELD_CHARS + 1)
    } else {
        format!("t.`{c}`")
    }
}

fn to_row(row: mysql_async::Row) -> Row {
    let cols: Vec<(String, ColumnType, u16)> = row
        .columns_ref()
        .iter()
        .map(|c| {
            (
                c.name_str().into_owned(),
                c.column_type(),
                c.character_set(),
            )
        })
        .collect();
    let mut out = Row::new();
    for ((name, ty, cs), v) in cols.into_iter().zip(row.unwrap()) {
        out.set(&name, convert(v, ty, cs));
    }
    out
}

/// The WHMCS database connector.
pub struct MysqlSource {
    config: DbSourceConfig,
    password: Option<String>,
    policy: OutboundPolicy,
    conn: Option<Conn>,
    columns: HashMap<&'static str, Option<Vec<String>>>,
    /// Columns of a [`LONG_TYPES`] type, per table.
    long_columns: HashMap<&'static str, Vec<String>>,
    no_extras: HashMap<Entity, bool>,
    attachments: Option<ApiClient>,
}

impl MysqlSource {
    /// A connector (not connected yet). `attachments` reads ticket
    /// attachments through the API when its credentials were given —
    /// WHMCS keeps attachment files on disk, not in the database.
    pub fn new(
        config: DbSourceConfig,
        secrets: &SourceSecrets,
        policy: OutboundPolicy,
        attachments: Option<ApiClient>,
    ) -> Self {
        Self {
            config,
            password: secrets.db_password.clone(),
            policy,
            conn: None,
            columns: HashMap::new(),
            long_columns: HashMap::new(),
            no_extras: HashMap::new(),
            attachments,
        }
    }

    async fn conn(&mut self) -> ImportResult<&mut Conn> {
        if self.conn.is_none() {
            let c = &self.config;
            if c.host.trim().is_empty() || c.database.trim().is_empty() || c.user.trim().is_empty()
            {
                return Err(ImportError::Config(
                    "host, database and user are required".into(),
                ));
            }
            let port = if c.port == 0 { 3306 } else { c.port };
            let addrs = self.policy.resolve_host(&c.host, port).await?;
            let ips: Vec<std::net::IpAddr> = addrs.iter().map(std::net::SocketAddr::ip).collect();
            let ssl = match c.tls {
                TlsMode::Disabled => None,
                TlsMode::Verify => Some(SslOpts::default()),
                TlsMode::Insecure => Some(
                    SslOpts::default()
                        .with_danger_accept_invalid_certs(true)
                        .with_danger_skip_domain_validation(true),
                ),
            };
            let opts = OptsBuilder::default()
                .ip_or_hostname(c.host.trim().trim_start_matches('[').trim_end_matches(']'))
                .tcp_port(port)
                .resolved_ips(Some(ips))
                .prefer_socket(false)
                .user(Some(c.user.clone()))
                .pass(self.password.clone())
                .db_name(Some(c.database.clone()))
                .ssl_opts(ssl)
                .max_allowed_packet(Some(MAX_PACKET))
                .wait_timeout(Some(600))
                .stmt_cache_size(64);
            let mut conn = match tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts)).await {
                Err(_) => return Err(ImportError::Connect("connection timed out".into())),
                Ok(Err(e)) if is_auth_failure(&e) => return Err(ImportError::Auth(mysql_err(&e))),
                Ok(Err(e)) => return Err(ImportError::Connect(mysql_err(&e))),
                Ok(Ok(conn)) => conn,
            };
            // Defence in depth: the connector only ever reads. Best effort —
            // older servers lack these settings.
            for stmt in [
                "SET SESSION TRANSACTION READ ONLY",
                "SET SESSION MAX_EXECUTION_TIME = 110000",
                "SET SESSION max_statement_time = 110",
            ] {
                let _ = conn.query_drop(stmt).await;
            }
            self.conn = Some(conn);
        }
        self.conn
            .as_mut()
            .ok_or_else(|| ImportError::Connect("not connected".into()))
    }

    async fn exec_rows(
        &mut self,
        sql: String,
        params: Vec<mysql_async::Value>,
    ) -> Result<Vec<Row>, mysql_async::Error> {
        let conn = match self.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(mysql_async::Error::Other(Box::new(std::io::Error::other(
                    e.to_string(),
                ))));
            }
        };
        let fut = conn.exec::<mysql_async::Row, _, _>(sql, params);
        match tokio::time::timeout(QUERY_TIMEOUT, fut).await {
            Ok(r) => r.map(|rows| rows.into_iter().map(to_row).collect()),
            Err(_) => {
                // A timed-out connection is in an unknown state.
                self.conn = None;
                Err(mysql_async::Error::Other(Box::new(std::io::Error::other(
                    "query timed out",
                ))))
            }
        }
    }

    /// Runs a page query streaming the rows: values are capped
    /// ([`Row::cap`]) and the page ends early once its rows hold
    /// `max_bytes` (`true` = cut short; the remaining rows are discarded).
    async fn exec_capped(
        &mut self,
        sql: String,
        params: Vec<mysql_async::Value>,
        max_bytes: usize,
    ) -> Result<(Vec<Row>, bool), mysql_async::Error> {
        let conn = match self.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(mysql_async::Error::Other(Box::new(std::io::Error::other(
                    e.to_string(),
                ))));
            }
        };
        let fut = async {
            let mut result = conn.exec_iter(sql, params).await?;
            let (mut rows, mut total, mut cut) = (Vec::new(), 0usize, false);
            while let Some(r) = result.next().await? {
                let mut row = to_row(r);
                row.cap(MAX_FIELD_CHARS);
                total += row.approx_bytes();
                rows.push(row);
                if total >= max_bytes {
                    cut = true;
                    break;
                }
            }
            result.drop_result().await?;
            Ok((rows, cut))
        };
        match tokio::time::timeout(QUERY_TIMEOUT, fut).await {
            Ok(r) => r,
            Err(_) => {
                self.conn = None;
                Err(mysql_async::Error::Other(Box::new(std::io::Error::other(
                    "query timed out",
                ))))
            }
        }
    }

    async fn query(
        &mut self,
        sql: String,
        params: Vec<mysql_async::Value>,
    ) -> ImportResult<Vec<Row>> {
        self.conn().await?;
        self.exec_rows(sql, params)
            .await
            .map_err(|e| ImportError::Source(mysql_err(&e)))
    }

    /// Columns of a table (`None` when the table does not exist).
    async fn table_columns(&mut self, table: &'static str) -> ImportResult<Option<Vec<String>>> {
        if let Some(c) = self.columns.get(table) {
            return Ok(c.clone());
        }
        let rows = self
            .query(
                "select column_name as c, data_type as t from information_schema.columns \
                 where table_schema = database() and table_name = ? order by ordinal_position"
                    .into(),
                vec![table.into()],
            )
            .await?;
        let cols: Vec<String> = rows.iter().filter_map(|r| r.str("c")).collect();
        let long: Vec<String> = rows
            .iter()
            .filter(|r| LONG_TYPES.contains(&r.text("t").to_ascii_lowercase().as_str()))
            .filter_map(|r| r.str("c"))
            .collect();
        let cols = (!cols.is_empty()).then_some(cols);
        self.columns.insert(table, cols.clone());
        self.long_columns.insert(table, long);
        Ok(cols)
    }

    fn where_clause(spec: &Spec, cols: &[String], extra: Option<&str>) -> String {
        let mut conds: Vec<String> = Vec::new();
        if let Some(e) = extra {
            conds.push(e.to_owned());
        }
        if !spec.filter.is_empty() {
            conds.push(spec.filter.to_owned());
        }
        for (col, cond) in spec.optional_filters {
            if cols.iter().any(|c| c.eq_ignore_ascii_case(col)) {
                conds.push((*cond).to_owned());
            }
        }
        if conds.is_empty() {
            String::new()
        } else {
            format!(" where {}", conds.join(" and "))
        }
    }

    async fn fetch_inner(
        &mut self,
        entity: Entity,
        cursor: i64,
        limit: u32,
    ) -> ImportResult<Batch> {
        let spec = spec_of(entity);
        let Some(cols) = self.table_columns(spec.table).await? else {
            return Ok(Batch::default());
        };
        let has_id = cols.iter().any(|c| c == "id");
        let long = self
            .long_columns
            .get(spec.table)
            .cloned()
            .unwrap_or_default();
        let mut select: Vec<String> = Vec::new();
        for c in &cols {
            if !denied(spec.table, c) {
                let c = ident(c)?;
                select.push(column_expr(c, long.iter().any(|l| l == c)));
            }
        }
        let use_extras = !self.no_extras.get(&entity).copied().unwrap_or(false);
        if use_extras {
            select.extend(spec.extras.iter().map(|e| (*e).to_owned()));
        }
        let limit = limit.clamp(1, 1000);
        let offset_paging = !has_id || entity == Entity::ActivityLog;
        let (sql, params): (String, Vec<mysql_async::Value>) = if offset_paging {
            let order = if entity == Entity::ActivityLog {
                "t.id desc".to_owned()
            } else {
                cols.iter()
                    .take(3)
                    .map(|c| format!("t.`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            (
                format!(
                    "select {} from `{}` t {}{} order by {order} limit ? offset ?",
                    select.join(", "),
                    ident(spec.table)?,
                    spec.joins,
                    Self::where_clause(&spec, &cols, None),
                ),
                vec![limit.into(), cursor.max(0).into()],
            )
        } else {
            (
                format!(
                    "select {} from `{}` t {}{} order by t.id limit ?",
                    select.join(", "),
                    ident(spec.table)?,
                    spec.joins,
                    Self::where_clause(&spec, &cols, Some("t.id > ?")),
                ),
                vec![cursor.into(), limit.into()],
            )
        };
        self.conn().await?;
        let (rows, cut) = match self.exec_capped(sql, params, MAX_BATCH_BYTES).await {
            Ok(r) => r,
            Err(e) if is_missing_object(&e) && use_extras && !spec.extras.is_empty() => {
                // An optional sub-select refers to a table / column this
                // WHMCS version lacks: read the plain table instead.
                self.no_extras.insert(entity, true);
                return Box::pin(self.fetch_inner(entity, cursor, limit)).await;
            }
            Err(e) => return Err(ImportError::Source(mysql_err(&e))),
        };
        let full = rows.len() >= limit as usize || cut;
        let next = if !full {
            None
        } else if offset_paging {
            Some(cursor.max(0) + i64::try_from(rows.len()).unwrap_or(i64::from(limit)))
        } else {
            rows.last().map(Row::id)
        };
        Ok(Batch { rows, next })
    }
}

#[async_trait]
impl Source for MysqlSource {
    fn flavor(&self) -> Flavor {
        Flavor::Database
    }

    fn supports(&self, entity: Entity) -> Support {
        match entity {
            Entity::Tickets | Entity::TicketReplies | Entity::TicketNotes
                if self.attachments.is_none() =>
            {
                Support::Partial(
                    "attachments are files on the WHMCS server; add API credentials to import them",
                )
            }
            _ => Support::Full,
        }
    }

    async fn probe(&mut self) -> ImportResult<Probe> {
        let server = self.query("select version() as v".into(), vec![]).await?;
        let mut notes = vec![format!(
            "server {}",
            server.first().and_then(|r| r.str("v")).unwrap_or_default()
        )];
        if self.table_columns("tblclients").await?.is_none() {
            return Err(ImportError::Config(
                "the database has no tblclients table — is this the WHMCS database?".into(),
            ));
        }
        let settings = self.settings().await?;
        if self.table_columns("tblusers").await?.is_some() {
            notes.push("WHMCS 8 user accounts (tblusers)".into());
        }
        if self.attachments.is_some() {
            notes.push("ticket attachments through the API".into());
        }
        Ok(Probe {
            version: settings.get("Version").cloned(),
            notes,
        })
    }

    async fn count(&mut self, entity: Entity) -> ImportResult<Option<u64>> {
        let spec = spec_of(entity);
        let Some(cols) = self.table_columns(spec.table).await? else {
            return Ok(Some(0));
        };
        let sql = format!(
            "select count(*) as n from `{}` t {}{}",
            ident(spec.table)?,
            spec.joins,
            Self::where_clause(&spec, &cols, None)
        );
        let rows = self.query(sql, vec![]).await?;
        Ok(rows
            .first()
            .and_then(|r| r.int("n"))
            .map(|n| u64::try_from(n).unwrap_or(0)))
    }

    async fn count_table(&mut self, table: &str) -> ImportResult<Option<u64>> {
        let table = ident(table)?.to_owned();
        let exists = self
            .query(
                "select count(*) as n from information_schema.tables where table_schema = database() and table_name = ?"
                    .into(),
                vec![table.clone().into()],
            )
            .await?;
        if exists.first().and_then(|r| r.int("n")).unwrap_or(0) == 0 {
            return Ok(Some(0));
        }
        let rows = self
            .query(format!("select count(*) as n from `{table}`"), vec![])
            .await?;
        Ok(rows
            .first()
            .and_then(|r| r.int("n"))
            .map(|n| u64::try_from(n).unwrap_or(0)))
    }

    async fn fetch(&mut self, entity: Entity, cursor: i64, limit: u32) -> ImportResult<Batch> {
        self.fetch_inner(entity, cursor, limit).await
    }

    async fn settings(&mut self) -> ImportResult<BTreeMap<String, String>> {
        let placeholders = vec!["?"; SETTING_KEYS.len()].join(", ");
        let rows = self
            .query(
                format!(
                    "select setting, value from tblconfiguration where setting in ({placeholders})"
                ),
                SETTING_KEYS.iter().map(|k| (*k).into()).collect(),
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| Some((r.str("setting")?, r.text("value"))))
            .collect())
    }

    async fn attachment(
        &mut self,
        owner: AttachmentOwner,
        owner_id: i64,
        index: usize,
        _stored_name: &str,
        max_bytes: u64,
    ) -> ImportResult<Attachment> {
        match self.attachments.as_mut() {
            Some(api) => api.attachment(owner, owner_id, index, max_bytes).await,
            None => Ok(Attachment::Unavailable(
                "attachment files live on the WHMCS server; the database connector needs API credentials to read them"
                    .into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(host: &str) -> DbSourceConfig {
        DbSourceConfig {
            host: host.into(),
            port: 3306,
            database: "whmcs".into(),
            user: "reader".into(),
            tls: TlsMode::Disabled,
            attachments_api: None,
        }
    }

    #[tokio::test]
    async fn loopback_and_metadata_hosts_are_refused_before_connecting() {
        for host in ["127.0.0.1", "169.254.169.254", "[::1]", "0.0.0.0"] {
            let mut s = MysqlSource::new(
                cfg(host),
                &SourceSecrets::default(),
                OutboundPolicy::Private,
                None,
            );
            let err = s.probe().await.unwrap_err();
            assert!(matches!(err, ImportError::Policy(_)), "{host}: {err:?}");
        }
        let mut s = MysqlSource::new(
            cfg("10.0.0.5"),
            &SourceSecrets::default(),
            OutboundPolicy::Public,
            None,
        );
        assert!(matches!(
            s.probe().await.unwrap_err(),
            ImportError::Policy(_)
        ));
    }

    #[test]
    fn long_columns_are_cut_on_the_server() {
        assert_eq!(column_expr("firstname", false), "t.`firstname`");
        assert_eq!(
            column_expr("message", true),
            format!("left(t.`message`, {}) as `message`", MAX_FIELD_CHARS + 1)
        );
        const { assert!(MAX_PACKET <= 16 * 1024 * 1024) };
        assert!(LONG_TYPES.contains(&"longtext") && LONG_TYPES.contains(&"longblob"));
    }

    #[test]
    fn identifiers_are_validated() {
        assert!(ident("tblclients").is_ok());
        assert!(ident("tbl`x").is_err());
        assert!(ident("a b").is_err());
        assert!(ident("").is_err());
    }

    #[test]
    fn values_convert() {
        use mysql_async::Value as M;
        assert_eq!(
            convert(M::Bytes(b"abc".to_vec()), ColumnType::MYSQL_TYPE_BLOB, 45),
            Value::Text("abc".into())
        );
        assert_eq!(
            convert(M::Bytes(vec![0, 1]), ColumnType::MYSQL_TYPE_BLOB, 63),
            Value::Bytes(vec![0, 1])
        );
        assert_eq!(
            convert(
                M::Bytes(b"12.50".to_vec()),
                ColumnType::MYSQL_TYPE_NEWDECIMAL,
                63
            ),
            Value::Text("12.50".into())
        );
        assert_eq!(
            convert(
                M::Date(0, 0, 0, 0, 0, 0, 0),
                ColumnType::MYSQL_TYPE_DATE,
                63
            ),
            Value::Null
        );
        assert_eq!(
            convert(
                M::Date(2024, 2, 29, 0, 0, 0, 0),
                ColumnType::MYSQL_TYPE_DATE,
                63
            ),
            Value::Date(NaiveDate::from_ymd_opt(2024, 2, 29).unwrap())
        );
        assert_eq!(
            convert(M::UInt(u64::MAX), ColumnType::MYSQL_TYPE_LONGLONG, 63),
            Value::Int(i64::MAX)
        );
    }

    #[test]
    fn every_entity_has_a_spec() {
        for e in Entity::ORDER {
            assert!(spec_of(*e).table.starts_with("tbl"), "{e:?}");
        }
    }
}
