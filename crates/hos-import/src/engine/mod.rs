//! The import engine: reads a [`Source`] entity by entity (parents first),
//! maps every row and writes it silently, or — in a dry run — only
//! validates and reports.
//!
//! * **Idempotent.** Every written row is recorded in `import_id_map`
//!   (source + entity + source id → HostingOS id). A re-run updates the same
//!   rows; rows that already existed in HostingOS (same currency code, user
//!   e-mail, TLD, …) are *adopted*: linked, never modified, never rolled back.
//! * **Resumable.** Progress (per-entity cursor and counters) is saved after
//!   every batch; a run picked up again after a crash continues at the
//!   cursor, and the id map makes a repeated batch harmless.
//! * **Batches.** Each batch is one transaction. When a row fails, the batch
//!   is rolled back and replayed one row per transaction, so one bad row
//!   costs only itself.
//! * **Silent.** Only `hos_db::repo::import_write` is used: no billing
//!   lifecycle calls, no jobs, e-mails, webhooks or provisioning.

mod analysis;
mod extras;
mod finish;
mod handlers;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{NaiveDate, Utc};
use hos_core::crypto::SecretBox;
use hos_core::ids::TenantId;
use hos_core::secret::SecretString;
use hos_db::repo::import::{self as repo, ImportRunRow, NewRunMessage};
use hos_db::repo::saas;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::config::{ImportOptions, Mappings, SourceSecrets};
use crate::entity::{Entity, UNSUPPORTED};
use crate::error::{ImportError, ImportResult};
use crate::row::Row;
use crate::source::{Flavor, Source, Support};

pub use analysis::Analysis;

/// Report lines kept per run phase (later lines are only counted).
pub const MAX_MESSAGES: usize = 20_000;

/// Counters of one entity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityProgress {
    /// `pending` | `running` | `done` | `off` | `unsupported`.
    pub status: String,
    /// Source rows (when the source can count).
    pub total: Option<u64>,
    pub read: u64,
    pub created: u64,
    pub updated: u64,
    pub adopted: u64,
    pub skipped: u64,
    pub errors: u64,
    /// Opaque source cursor of the next batch.
    pub cursor: i64,
    /// Why the entity is off / unsupported / partial.
    #[serde(default)]
    pub note: Option<String>,
}

/// Progress of a run (`import_runs.progress`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub entities: BTreeMap<String, EntityProgress>,
    pub current: Option<String>,
    /// Post-import steps done (`credit_balances`, `sequences`, `cutover`).
    #[serde(default)]
    pub steps: BTreeSet<String>,
}

impl Progress {
    /// Parsed from the stored JSON (empty on garbage).
    pub fn from_json(v: &serde_json::Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Rows read so far / known total (for the progress bar).
    pub fn fraction(&self) -> (u64, u64) {
        self.entities.values().fold((0, 0), |(r, t), e| {
            (r + e.read, t + e.total.unwrap_or(e.read).max(e.read))
        })
    }
}

/// What a handler did with a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Created,
    Updated,
    Adopted,
    Skipped,
}

/// Plan-limit headroom of one limit key.
#[derive(Debug, Clone, Copy)]
struct Headroom {
    max: Option<i64>,
    used: i64,
}

impl Headroom {
    fn take(&mut self) -> bool {
        if self.max.is_some_and(|m| self.used >= m) {
            return false;
        }
        self.used += 1;
        true
    }
}

/// Reference data read before the entities.
#[derive(Debug, Clone, Default)]
struct RefData {
    /// WHMCS currency id → code.
    currencies: HashMap<i64, String>,
    default_currency: String,
    default_country: String,
    default_locale: String,
    /// WHMCS < 8: logins live in `tblclients` / `tblcontacts`.
    legacy_logins: bool,
    l2_compound: bool,
}

/// Run parameters.
pub struct EngineParams<'a> {
    pub pool: &'a PgPool,
    pub secret_box: &'a SecretBox,
    pub run: &'a ImportRunRow,
    pub secrets: &'a SourceSecrets,
    /// Worker id holding the run lease.
    pub worker: String,
    pub today: NaiveDate,
}

/// Final figures of a run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunResult {
    pub phase: String,
    pub progress: Progress,
    /// Dry run: conflicts / discovery / plan-limit check.
    pub analysis: Option<Analysis>,
    /// `(entity, level) → count`.
    pub messages: BTreeMap<String, BTreeMap<String, i64>>,
    pub unsupported: Vec<(String, String, Option<u64>)>,
    pub version: Option<String>,
    pub post: BTreeMap<String, serde_json::Value>,
}

/// The engine.
pub struct Engine<'a> {
    pool: &'a PgPool,
    secret_box: &'a SecretBox,
    tenant: TenantId,
    run_id: Uuid,
    source_key: String,
    worker: String,
    started_by: Option<Uuid>,
    dry: bool,
    today: NaiveDate,
    opts: ImportOptions,
    maps: Mappings,
    cc_hash: Option<SecretString>,
    flavor: Flavor,
    progress: Progress,
    refdata: RefData,
    /// Committed id map, loaded per entity on first use.
    ids: HashMap<&'static str, HashMap<String, repo::Mapped>>,
    /// Ids written in the current batch (or the whole dry run).
    pending: HashMap<(&'static str, String), (Uuid, bool)>,
    msgs: Vec<NewRunMessage>,
    msg_total: usize,
    client_currency: HashMap<Uuid, String>,
    service_cycle: HashMap<Uuid, String>,
    product_pay_type: HashMap<String, String>,
    expected_credit: HashMap<Uuid, (Decimal, String)>,
    max_invoice_seq: Option<i64>,
    headroom: HashMap<&'static str, Headroom>,
    weak_hashes: u64,
    no_hashes: u64,
    analysis: Analysis,
    warned: HashSet<&'static str>,
}

/// Source id text of a row.
fn sid(row: &Row) -> String {
    row.id().to_string()
}

impl<'a> Engine<'a> {
    /// Engine for `params.run` (phase, options and mappings come from the run).
    pub fn new(p: EngineParams<'a>) -> ImportResult<Self> {
        let opts: ImportOptions = serde_json::from_value(p.run.options.clone())
            .map_err(|e| ImportError::Config(format!("options: {e}")))?;
        let maps: Mappings = serde_json::from_value(p.run.mappings.clone()).unwrap_or_default();
        Ok(Self {
            pool: p.pool,
            secret_box: p.secret_box,
            tenant: p.run.tenant_id,
            run_id: p.run.id,
            source_key: p.run.source_key.clone(),
            worker: p.worker,
            started_by: p.run.started_by,
            dry: p.run.phase == "dry_run",
            today: p.today,
            opts,
            maps,
            cc_hash: p.secrets.cc_hash(),
            flavor: Flavor::Database,
            progress: Progress::from_json(&p.run.progress),
            refdata: RefData::default(),
            ids: HashMap::new(),
            pending: HashMap::new(),
            msgs: Vec::new(),
            msg_total: 0,
            client_currency: HashMap::new(),
            service_cycle: HashMap::new(),
            product_pay_type: HashMap::new(),
            expected_credit: HashMap::new(),
            max_invoice_seq: None,
            headroom: HashMap::new(),
            weak_hashes: 0,
            no_hashes: 0,
            analysis: Analysis::default(),
            warned: HashSet::new(),
        })
    }

    fn phase(&self) -> &'static str {
        if self.dry { "dry_run" } else { "import" }
    }

    // ---------------------------------------------------------------------
    // messages
    // ---------------------------------------------------------------------

    pub(crate) fn note(
        &mut self,
        entity: &str,
        level: &'static str,
        source_id: Option<&str>,
        code: &str,
        message: impl Into<String>,
    ) {
        self.msg_total += 1;
        if self.msg_total > MAX_MESSAGES && level != "error" {
            return;
        }
        self.msgs.push(NewRunMessage {
            entity: entity.to_owned(),
            level,
            source_id: source_id.map(str::to_owned),
            code: code.chars().take(60).collect(),
            message: crate::sanitize::clean_line(&message.into(), 1000),
        });
    }

    /// A warning reported once per run.
    pub(crate) fn note_once(
        &mut self,
        key: &'static str,
        entity: &str,
        level: &'static str,
        message: impl Into<String>,
    ) {
        if self.warned.insert(key) {
            self.note(entity, level, None, key, message);
        }
    }

    async fn flush_messages(&mut self) -> ImportResult<()> {
        if !self.msgs.is_empty() {
            let msgs = std::mem::take(&mut self.msgs);
            for chunk in msgs.chunks(500) {
                repo::add_messages(self.pool, self.tenant, self.run_id, self.phase(), chunk)
                    .await?;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // id map
    // ---------------------------------------------------------------------

    async fn ensure_loaded(&mut self, entity: &'static str) -> ImportResult<()> {
        if !self.ids.contains_key(entity) {
            let m = repo::load_id_map(self.pool, self.tenant, &self.source_key, entity).await?;
            self.ids.insert(entity, m);
        }
        Ok(())
    }

    /// HostingOS id of a source row (pending writes first).
    pub(crate) async fn lookup(
        &mut self,
        entity: &'static str,
        source_id: &str,
    ) -> ImportResult<Option<repo::Mapped>> {
        if let Some((id, adopted)) = self.pending.get(&(entity, source_id.to_owned())) {
            return Ok(Some(repo::Mapped {
                target_id: *id,
                adopted: *adopted,
            }));
        }
        self.ensure_loaded(entity).await?;
        Ok(self.ids.get(entity).and_then(|m| m.get(source_id)).copied())
    }

    /// Id of a referenced row (`col` holds a positive WHMCS id).
    pub(crate) async fn reference(
        &mut self,
        entity: &'static str,
        row: &Row,
        col: &str,
    ) -> ImportResult<Option<Uuid>> {
        match row.reference(col) {
            Some(id) => Ok(self
                .lookup(entity, &id.to_string())
                .await?
                .map(|m| m.target_id)),
            None => Ok(None),
        }
    }

    /// Target id for a source row: the mapped one or a new one.
    pub(crate) async fn target(
        &mut self,
        entity: &'static str,
        source_id: &str,
    ) -> ImportResult<(Uuid, Option<repo::Mapped>)> {
        Ok(match self.lookup(entity, source_id).await? {
            Some(m) => (m.target_id, Some(m)),
            None => (Uuid::now_v7(), None),
        })
    }

    /// Records a mapping (inside the batch transaction).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record(
        &mut self,
        conn: &mut PgConnection,
        entity: &'static str,
        source_id: &str,
        table: &str,
        id: Uuid,
        created: bool,
        adopted: bool,
    ) -> ImportResult<()> {
        if !self.dry {
            repo::record_id(
                &mut *conn,
                self.tenant,
                &self.source_key,
                entity,
                source_id,
                table,
                id,
                self.run_id,
                created && !adopted,
            )
            .await?;
        }
        self.pending
            .insert((entity, source_id.to_owned()), (id, adopted));
        Ok(())
    }

    fn commit_pending(&mut self) {
        if self.dry {
            return;
        }
        for ((e, s), (id, adopted)) in std::mem::take(&mut self.pending) {
            if let Some(m) = self.ids.get_mut(e) {
                m.entry(s).or_insert(repo::Mapped {
                    target_id: id,
                    adopted,
                });
            }
        }
    }

    // ---------------------------------------------------------------------
    // shared helpers for handlers
    // ---------------------------------------------------------------------

    /// Currency code of a WHMCS currency id (default when unknown / 0).
    pub(crate) fn currency_of(&self, id: Option<i64>) -> String {
        id.and_then(|i| self.refdata.currencies.get(&i).cloned())
            .unwrap_or_else(|| self.refdata.default_currency.clone())
    }

    /// Currency of an imported client.
    pub(crate) async fn currency_of_client(
        &mut self,
        conn: &mut PgConnection,
        client: Uuid,
    ) -> ImportResult<String> {
        if let Some(c) = self.client_currency.get(&client) {
            return Ok(c.clone());
        }
        let c = hos_db::repo::import_write::client_currency(conn, self.tenant, client)
            .await?
            .unwrap_or_else(|| self.refdata.default_currency.clone());
        self.client_currency.insert(client, c.clone());
        Ok(c)
    }

    /// Decrypts a WHMCS-encrypted column (database flavour) or returns the
    /// API's plain value. Failures are reported, never stored.
    pub(crate) fn secret(&mut self, entity: &str, row: &Row, col: &str) -> Option<String> {
        let raw = row.raw_str(col)?;
        if raw.trim().is_empty() {
            return None;
        }
        if self.flavor == Flavor::Api {
            return Some(raw);
        }
        let Some(hash) = self.cc_hash.clone() else {
            self.note_once(
                "no_cc_hash",
                entity,
                "warning",
                "no cc_encryption_hash given: encrypted WHMCS fields (service and server passwords) are not imported",
            );
            return None;
        };
        match crate::crypto::whmcs_decrypt(&raw, hash.expose_str()) {
            Ok(v) => v,
            Err(e) => {
                let id = sid(row);
                self.note(
                    entity,
                    "warning",
                    Some(&id),
                    "decrypt_failed",
                    format!("{col}: {e}; not imported"),
                );
                None
            }
        }
    }

    /// Takes plan-limit headroom (`true` = allowed).
    pub(crate) fn within_limit(&mut self, key: &'static str) -> bool {
        self.headroom.get_mut(key).is_none_or(Headroom::take)
    }

    // ---------------------------------------------------------------------
    // run
    // ---------------------------------------------------------------------

    async fn load_refdata(&mut self, source: &mut dyn Source) -> ImportResult<Option<String>> {
        let settings = source.settings().await.unwrap_or_default();
        let version = settings.get("Version").cloned();
        self.refdata.default_country = crate::map::country(
            settings.get("DefaultCountry").map_or("", String::as_str),
            "TR",
        );
        self.refdata.default_locale =
            crate::map::locale(settings.get("Language").map_or("", String::as_str), "en");
        self.refdata.l2_compound = settings
            .get("TaxL2Compound")
            .is_some_and(|v| v.eq_ignore_ascii_case("on"));
        let mut cursor = 0;
        loop {
            let b = source.fetch(Entity::Currencies, cursor, 200).await?;
            for r in &b.rows {
                if let Some(code) = r.str("code").map(|c| c.to_ascii_uppercase()) {
                    if r.flag("default") || self.refdata.default_currency.is_empty() {
                        self.refdata.default_currency.clone_from(&code);
                    }
                    self.refdata.currencies.insert(r.id(), code);
                }
            }
            match b.next {
                Some(n) => cursor = n,
                None => break,
            }
        }
        if self.refdata.default_currency.is_empty() {
            self.refdata.default_currency = "USD".into();
        }
        self.refdata.legacy_logins = match source.supports(Entity::Users) {
            Support::No(_) => true,
            _ => matches!(source.count(Entity::Users).await, Ok(Some(0)) | Err(_)),
        };
        Ok(version)
    }

    async fn load_headroom(&mut self) -> ImportResult<()> {
        for key in ["max_clients", "max_services", "max_staff", "integrations"] {
            let s = saas::plan_limit(self.pool, self.tenant, key).await?;
            self.headroom.insert(
                key,
                Headroom {
                    max: s.max,
                    used: s.used,
                },
            );
        }
        Ok(())
    }

    async fn checkpoint(&mut self) -> ImportResult<()> {
        self.flush_messages().await?;
        let progress = serde_json::to_value(&self.progress).unwrap_or_default();
        if !repo::save_progress(self.pool, self.tenant, self.run_id, &self.worker, &progress)
            .await?
        {
            return Err(ImportError::LeaseLost);
        }
        if repo::cancel_requested(self.pool, self.tenant, self.run_id).await? {
            return Err(ImportError::Cancelled);
        }
        Ok(())
    }

    /// First statement of every writing transaction: locks the run row and
    /// stops with [`ImportError::LeaseLost`] when another worker holds it.
    async fn hold_lease(&self, conn: &mut PgConnection) -> ImportResult<()> {
        if !repo::hold_lease(conn, self.tenant, self.run_id, &self.worker).await? {
            return Err(ImportError::LeaseLost);
        }
        Ok(())
    }

    /// Current progress snapshot.
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// Runs the phase of the run against `source`.
    pub async fn run(&mut self, source: &mut dyn Source) -> ImportResult<RunResult> {
        self.flavor = source.flavor();
        let fresh = self.progress.entities.is_empty();
        if fresh {
            repo::clear_messages(self.pool, self.tenant, self.run_id, self.phase()).await?;
        }
        let version = self.load_refdata(source).await?;
        if !self.dry {
            self.load_headroom().await?;
            if self.opts.pause_automation && !self.progress.steps.contains("paused") {
                let mut conn = self.pool.acquire().await?;
                repo::set_automation_active(&mut conn, self.tenant, false).await?;
                repo::set_automation_paused(self.pool, self.tenant, self.run_id, true).await?;
                self.progress.steps.insert("paused".into());
            }
        } else {
            self.analysis.begin(self.pool, self.tenant).await?;
        }
        for &entity in Entity::ORDER {
            self.run_entity(source, entity).await?;
            if entity == Entity::Credits && !self.dry && self.opts.wants(Entity::Clients) {
                self.reconcile_credits(source).await?;
            }
            if entity == Entity::Transactions && !self.dry && self.opts.wants(Entity::Invoices) {
                self.reconcile_invoice_payments().await?;
            }
            if entity == Entity::AffiliateWithdrawals
                && self.opts.wants(Entity::Affiliates)
                && !matches!(source.supports(Entity::Affiliates), Support::No(_))
            {
                self.reconcile_affiliates(source).await?;
            }
        }
        let mut unsupported = Vec::new();
        for (key, table, reason) in UNSUPPORTED {
            let n = source.count_table(table).await.ok().flatten();
            if n.is_none_or(|n| n > 0) {
                let msg = match n {
                    Some(n) => format!("{n} rows in {table} not imported: {reason}"),
                    None => format!("{table} not imported: {reason}"),
                };
                self.note(key, "skipped", None, "unsupported", msg);
            }
            unsupported.push(((*key).to_owned(), (*reason).to_owned(), n));
        }
        if self.weak_hashes > 0 {
            let n = self.weak_hashes;
            self.note(
                "users",
                "warning",
                None,
                "weak_password_hash",
                format!("{n} logins have legacy MD5 password hashes: they must reset their password at first login"),
            );
        }
        if self.no_hashes > 0 {
            let n = self.no_hashes;
            self.note(
                "users",
                "info",
                None,
                "no_password",
                format!("{n} logins have no importable password and use \"forgot password\""),
            );
        }
        let mut post = BTreeMap::new();
        let analysis = if self.dry {
            let a = self.finish_analysis(source).await?;
            Some(a)
        } else {
            post = self.post_import().await?;
            None
        };
        self.progress.current = None;
        self.flush_messages().await?;
        let counts =
            repo::message_counts(self.pool, self.tenant, self.run_id, self.phase()).await?;
        let mut messages: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        for ((e, l), n) in counts {
            messages.entry(e).or_default().insert(l, n);
        }
        Ok(RunResult {
            phase: self.phase().to_owned(),
            progress: self.progress.clone(),
            analysis,
            messages,
            unsupported,
            version,
            post,
        })
    }

    async fn run_entity(&mut self, source: &mut dyn Source, entity: Entity) -> ImportResult<()> {
        let key = entity.key().to_owned();
        let mut ep = self
            .progress
            .entities
            .get(&key)
            .cloned()
            .unwrap_or_else(|| EntityProgress {
                status: "pending".into(),
                ..EntityProgress::default()
            });
        if matches!(ep.status.as_str(), "done" | "off" | "unsupported") {
            return Ok(());
        }
        if !self.opts.wants(entity) {
            ep.status = "off".into();
            self.progress.entities.insert(key, ep);
            return Ok(());
        }
        match source.supports(entity) {
            Support::No(reason) => {
                ep.status = "unsupported".into();
                ep.note = Some(reason.to_owned());
                self.note(&key, "skipped", None, "unsupported_by_source", reason);
                self.progress.entities.insert(key, ep);
                return self.checkpoint().await;
            }
            Support::Partial(reason) => {
                if ep.note.is_none() {
                    self.note(&key, "warning", None, "partial", reason);
                }
                ep.note = Some(reason.to_owned());
            }
            Support::Full => {}
        }
        if ep.total.is_none() {
            ep.total = source.count(entity).await.unwrap_or(None);
        }
        ep.status = "running".into();
        self.progress.current = Some(key.clone());
        self.progress.entities.insert(key.clone(), ep);
        self.checkpoint().await?;
        let limit = self.opts.batch();
        loop {
            let cursor = self.progress.entities[&key].cursor;
            let batch = source.fetch(entity, cursor, limit).await?;
            let mut rows = batch.rows;
            // Oversized values (the database connector cuts them already,
            // the API's are bounded by the response size) are cut and listed.
            for r in &mut rows {
                r.cap(crate::row::MAX_FIELD_CHARS);
                if let Some(cols) = r.truncated() {
                    let id = sid(r);
                    self.note(
                        &key,
                        "warning",
                        Some(&id),
                        "truncated",
                        format!(
                            "{cols}: longer than {} characters, cut",
                            crate::row::MAX_FIELD_CHARS
                        ),
                    );
                }
            }
            if entity == Entity::ActivityLog {
                let read = self.progress.entities[&key].read;
                let left = u64::from(self.opts.activity_log_limit).saturating_sub(read);
                rows.truncate(usize::try_from(left).unwrap_or(usize::MAX));
            }
            let n = rows.len() as u64;
            let stats = self.process_batch(source, entity, &rows).await?;
            let ep = self
                .progress
                .entities
                .get_mut(&key)
                .expect("entity progress present");
            ep.read += n;
            ep.created += stats[0];
            ep.updated += stats[1];
            ep.adopted += stats[2];
            ep.skipped += stats[3];
            ep.errors += stats[4];
            let done = batch.next.is_none()
                || (entity == Entity::ActivityLog
                    && ep.read >= u64::from(self.opts.activity_log_limit));
            match batch.next {
                Some(next) if !done => ep.cursor = next,
                _ => ep.status = "done".into(),
            }
            self.checkpoint().await?;
            if done {
                break;
            }
        }
        Ok(())
    }

    /// `[created, updated, adopted, skipped, errors]` of a batch.
    async fn process_batch(
        &mut self,
        source: &mut dyn Source,
        entity: Entity,
        rows: &[Row],
    ) -> ImportResult<[u64; 5]> {
        let mut stats = [0u64; 5];
        if rows.is_empty() {
            return Ok(stats);
        }
        if self.dry {
            let mut conn = self.pool.acquire().await?;
            for row in rows {
                self.analysis.observe(entity, row);
                match self.handle(&mut conn, source, entity, row).await {
                    Ok(o) => count(&mut stats, o),
                    Err(e) if is_fatal(&e) => return Err(e),
                    Err(e) => {
                        stats[4] += 1;
                        let id = sid(row);
                        self.note(entity.key(), "error", Some(&id), e.code(), e.to_string());
                    }
                }
            }
            return Ok(stats);
        }
        // Fast path: the whole batch in one transaction.
        let msg_mark = self.msgs.len();
        let total_mark = self.msg_total;
        let (cc, sc, pc, ec) = (
            self.client_currency.len(),
            self.service_cycle.len(),
            self.product_pay_type.len(),
            self.expected_credit.len(),
        );
        let mut tx = self.pool.begin().await?;
        self.hold_lease(&mut tx).await?;
        let mut failed = false;
        for row in rows {
            match self.handle(&mut tx, source, entity, row).await {
                Ok(o) => count(&mut stats, o),
                Err(e) if is_fatal(&e) => return Err(e),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            tx.commit().await?;
            self.commit_pending();
            return Ok(stats);
        }
        tx.rollback().await?;
        self.pending.clear();
        self.msgs.truncate(msg_mark);
        self.msg_total = total_mark;
        if self.client_currency.len() != cc
            || self.service_cycle.len() != sc
            || self.product_pay_type.len() != pc
            || self.expected_credit.len() != ec
        {
            // Caches may hold ids of rolled-back rows; they are refilled on demand.
            self.client_currency.clear();
            self.service_cycle.clear();
            self.product_pay_type.clear();
            self.expected_credit.clear();
        }
        // Slow path: one transaction per row.
        stats = [0; 5];
        for row in rows {
            let mut tx = self.pool.begin().await?;
            self.hold_lease(&mut tx).await?;
            let mark = self.msgs.len();
            match self.handle(&mut tx, source, entity, row).await {
                Ok(o) => {
                    tx.commit().await?;
                    self.commit_pending();
                    count(&mut stats, o);
                }
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => {
                    tx.rollback().await?;
                    self.pending.clear();
                    self.msgs.truncate(mark);
                    stats[4] += 1;
                    let id = sid(row);
                    tracing::warn!(entity = entity.key(), source_id = %id, error = %e, "import row failed");
                    self.note(entity.key(), "error", Some(&id), e.code(), e.to_string());
                }
            }
        }
        Ok(stats)
    }

    /// Opening credit balances: WHMCS's `tblclients.credit` is the truth; the
    /// imported credit log is completed with one adjustment row per client.
    async fn reconcile_credits(&mut self, source: &mut dyn Source) -> ImportResult<()> {
        if self.progress.steps.contains("credit_balances") {
            return Ok(());
        }
        if self.expected_credit.is_empty() {
            // Resumed run: read the balances again.
            let mut cursor = 0;
            loop {
                let b = source
                    .fetch(Entity::Clients, cursor, self.opts.batch())
                    .await?;
                for r in &b.rows {
                    if let Some(c) = self.lookup(Entity::Clients.key(), &sid(r)).await? {
                        let mut conn = self.pool.acquire().await?;
                        let cur = self.currency_of_client(&mut conn, c.target_id).await?;
                        self.expected_credit
                            .insert(c.target_id, (r.money("credit"), cur));
                    }
                }
                match b.next {
                    Some(n) => cursor = n,
                    None => break,
                }
            }
        }
        let expected: Vec<(Uuid, (Decimal, String))> = self.expected_credit.drain().collect();
        for chunk in expected.chunks(200) {
            let mut tx = self.pool.begin().await?;
            self.hold_lease(&mut tx).await?;
            for (client, (amount, currency)) in chunk {
                self.adjust_credit(&mut tx, *client, *amount, currency)
                    .await?;
            }
            tx.commit().await?;
            self.commit_pending();
        }
        self.progress.steps.insert("credit_balances".into());
        self.checkpoint().await
    }

    /// Amount paid of the imported invoices, once the transactions are in.
    /// Open invoices (`draft`, `unpaid`, `payment_pending`, `collections`)
    /// get payments − refunds / chargebacks in the invoice currency, clamped
    /// to `[0, total − credit applied]`; a paid / refunded invoice keeps what
    /// its WHMCS status says (fully paid), because WHMCS's status is the
    /// record staff and clients have seen. Transactions that do not add up are
    /// listed in the report. Recomputed from the stored rows on every run, so
    /// a re-run gives the same figures.
    async fn reconcile_invoice_payments(&mut self) -> ImportResult<()> {
        if self.progress.steps.contains("invoice_payments") {
            return Ok(());
        }
        let key = Entity::Invoices.key();
        let mut tx = self.pool.begin().await?;
        self.hold_lease(&mut tx).await?;
        let (changed, mismatches) = hos_db::repo::import_write::reconcile_invoice_payments(
            &mut tx,
            self.tenant,
            &self.source_key,
            1000,
        )
        .await?;
        tx.commit().await?;
        for m in mismatches {
            let (code, text) = if m.foreign_txns > 0 {
                (
                    "foreign_currency_payment",
                    format!(
                        "{} payment(s) in another currency than the invoice are not counted as paid",
                        m.foreign_txns
                    ),
                )
            } else if m.status == "paid" {
                (
                    "paid_amount_mismatch",
                    format!(
                        "paid in WHMCS: kept as fully paid ({}), but its transactions add up to {}",
                        m.due, m.net
                    ),
                )
            } else {
                (
                    "overpaid",
                    format!(
                        "transactions ({}) exceed the amount due ({}); the excess is not moved to credit (the WHMCS credit balance is imported as is)",
                        m.net, m.due
                    ),
                )
            };
            self.note(key, "warning", Some(&m.source_id), code, text);
        }
        if changed > 0 {
            self.note(
                key,
                "info",
                None,
                "payments_applied",
                format!("{changed} open invoice(s) carry partial payments from their transactions"),
            );
        }
        self.progress.steps.insert("invoice_payments".into());
        self.checkpoint().await
    }
}

fn count(stats: &mut [u64; 5], o: Outcome) {
    match o {
        Outcome::Created => stats[0] += 1,
        Outcome::Updated => stats[1] += 1,
        Outcome::Adopted => stats[2] += 1,
        Outcome::Skipped => stats[3] += 1,
    }
}

/// Errors that end the run instead of only the row.
fn is_fatal(e: &ImportError) -> bool {
    matches!(
        e,
        ImportError::Cancelled
            | ImportError::LeaseLost
            | ImportError::Connect(_)
            | ImportError::Auth(_)
            | ImportError::Policy(_)
            | ImportError::Source(_)
            | ImportError::SecretsGone
    )
}

/// Today's date (UTC).
pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}
