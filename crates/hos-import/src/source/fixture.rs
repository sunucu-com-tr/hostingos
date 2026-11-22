//! In-memory source: rows per entity, shaped like the database connector's
//! output. Used by the integration tests (no MySQL needed in CI) and handy
//! for reproducing a customer's data problem from a few rows.

use std::collections::BTreeMap;

use async_trait::async_trait;

use super::{Attachment, AttachmentOwner, Batch, Flavor, Probe, Source, Support};
use crate::entity::Entity;
use crate::error::{ImportError, ImportResult};
use crate::row::Row;

/// Fixture rows.
#[derive(Debug, Clone)]
pub struct FixtureSource {
    flavor: Flavor,
    rows: BTreeMap<Entity, Vec<Row>>,
    tables: BTreeMap<String, u64>,
    settings: BTreeMap<String, String>,
    attachments: BTreeMap<(i64, usize, &'static str), (String, Vec<u8>)>,
    unsupported: BTreeMap<Entity, &'static str>,
    /// Fail every fetch of this entity (error-path tests).
    pub fail_on: Option<Entity>,
}

impl FixtureSource {
    /// Empty fixture of the given flavour.
    pub fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            rows: BTreeMap::new(),
            tables: BTreeMap::new(),
            settings: BTreeMap::new(),
            attachments: BTreeMap::new(),
            unsupported: BTreeMap::new(),
            fail_on: None,
        }
    }

    /// Adds rows of `entity`.
    #[must_use]
    pub fn with(mut self, entity: Entity, rows: Vec<Row>) -> Self {
        self.rows.entry(entity).or_default().extend(rows);
        self
    }

    /// Row count of an unsupported WHMCS table.
    #[must_use]
    pub fn with_table(mut self, table: &str, count: u64) -> Self {
        self.tables.insert(table.to_owned(), count);
        self
    }

    /// A `tblconfiguration` setting.
    #[must_use]
    pub fn with_setting(mut self, key: &str, value: &str) -> Self {
        self.settings.insert(key.to_owned(), value.to_owned());
        self
    }

    /// An attachment.
    #[must_use]
    pub fn with_attachment(
        mut self,
        owner: AttachmentOwner,
        owner_id: i64,
        index: usize,
        filename: &str,
        bytes: &[u8],
    ) -> Self {
        self.attachments.insert(
            (owner_id, index, owner.api_type()),
            (filename.to_owned(), bytes.to_vec()),
        );
        self
    }

    /// Marks an entity as not deliverable.
    #[must_use]
    pub fn without(mut self, entity: Entity, reason: &'static str) -> Self {
        self.unsupported.insert(entity, reason);
        self
    }

    /// Mutable rows of `entity` (to change data between runs).
    pub fn rows_mut(&mut self, entity: Entity) -> &mut Vec<Row> {
        self.rows.entry(entity).or_default()
    }

    fn sorted(&self, entity: Entity) -> Vec<Row> {
        let mut rows = self.rows.get(&entity).cloned().unwrap_or_default();
        rows.sort_by_key(Row::id);
        if entity == Entity::ActivityLog {
            rows.reverse();
        }
        rows
    }
}

#[async_trait]
impl Source for FixtureSource {
    fn flavor(&self) -> Flavor {
        self.flavor
    }

    fn supports(&self, entity: Entity) -> Support {
        self.unsupported
            .get(&entity)
            .map_or(Support::Full, |r| Support::No(r))
    }

    async fn probe(&mut self) -> ImportResult<Probe> {
        Ok(Probe {
            version: self.settings.get("Version").cloned(),
            notes: vec!["fixture".into()],
        })
    }

    async fn count(&mut self, entity: Entity) -> ImportResult<Option<u64>> {
        Ok(Some(self.rows.get(&entity).map_or(0, |r| r.len() as u64)))
    }

    async fn count_table(&mut self, table: &str) -> ImportResult<Option<u64>> {
        Ok(Some(self.tables.get(table).copied().unwrap_or(0)))
    }

    async fn fetch(&mut self, entity: Entity, cursor: i64, limit: u32) -> ImportResult<Batch> {
        if self.fail_on == Some(entity) {
            return Err(ImportError::Source(format!(
                "fixture failure on {}",
                entity.key()
            )));
        }
        let rows = self.sorted(entity);
        let start = usize::try_from(cursor.max(0)).unwrap_or(0);
        let end = (start + limit as usize).min(rows.len());
        let page: Vec<Row> = rows
            .get(start..end)
            .map(<[Row]>::to_vec)
            .unwrap_or_default();
        let next = (end < rows.len()).then(|| i64::try_from(end).unwrap_or(i64::MAX));
        Ok(Batch { rows: page, next })
    }

    async fn settings(&mut self) -> ImportResult<BTreeMap<String, String>> {
        Ok(self.settings.clone())
    }

    async fn attachment(
        &mut self,
        owner: AttachmentOwner,
        owner_id: i64,
        index: usize,
        _stored_name: &str,
        max_bytes: u64,
    ) -> ImportResult<Attachment> {
        Ok(
            match self.attachments.get(&(owner_id, index, owner.api_type())) {
                Some((_, b)) if b.len() as u64 > max_bytes => {
                    Attachment::TooLarge(Some(b.len() as u64))
                }
                Some((filename, bytes)) => Attachment::Data {
                    filename: filename.clone(),
                    bytes: bytes.clone(),
                },
                None => Attachment::Missing,
            },
        )
    }
}
