//! Append-only audit trail (`audit_log`). Every admin / platform action is
//! written here (docs/ARCHITECTURE.md §6).

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use hos_core::ids::{AuditLogId, TenantId};
use sqlx::{Acquire, FromRow, PgExecutor, Postgres};
use uuid::Uuid;

use super::{Json, empty_object};
use crate::enums::ActorKind;
use crate::error::DbResult;
use crate::page::{Page, Paged, fetch_paged};

/// `audit_log` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct AuditLogRow {
    pub id: AuditLogId,
    pub tenant_id: Option<TenantId>,
    pub actor_kind: ActorKind,
    pub actor_id: Option<Uuid>,
    pub impersonator_id: Option<Uuid>,
    pub action: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub changes: Json,
    pub metadata: Json,
    pub created_at: DateTime<Utc>,
}

/// One audit entry to write. Build with [`AuditEntry::new`] and the builder methods.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// `None` for platform-level actions.
    pub tenant_id: Option<TenantId>,
    pub actor_kind: ActorKind,
    pub actor_id: Option<Uuid>,
    pub impersonator_id: Option<Uuid>,
    /// Dotted verb, e.g. `invoice.mark_paid`, `client.update`.
    pub action: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub changes: Json,
    pub metadata: Json,
}

impl AuditEntry {
    /// Entry for `action` by `actor`.
    pub fn new(
        tenant_id: Option<TenantId>,
        actor_kind: ActorKind,
        actor_id: Option<Uuid>,
        action: &str,
    ) -> Self {
        Self {
            tenant_id,
            actor_kind,
            actor_id,
            impersonator_id: None,
            action: action.to_owned(),
            entity_type: None,
            entity_id: None,
            ip: None,
            user_agent: None,
            changes: empty_object(),
            metadata: empty_object(),
        }
    }

    /// Sets the affected entity.
    #[must_use]
    pub fn entity(mut self, entity_type: &str, entity_id: impl Into<Uuid>) -> Self {
        self.entity_type = Some(entity_type.to_owned());
        self.entity_id = Some(entity_id.into());
        self
    }

    /// Sets request info.
    #[must_use]
    pub fn request(mut self, ip: Option<IpAddr>, user_agent: Option<&str>) -> Self {
        self.ip = ip;
        self.user_agent = user_agent.map(str::to_owned);
        self
    }

    /// Sets the change set (`{"before": .., "after": ..}` or a diff).
    #[must_use]
    pub fn changes(mut self, changes: Json) -> Self {
        self.changes = changes;
        self
    }

    /// Sets extra metadata.
    #[must_use]
    pub fn metadata(mut self, metadata: Json) -> Self {
        self.metadata = metadata;
        self
    }

    /// Sets the impersonator.
    #[must_use]
    pub fn impersonator(mut self, id: Option<Uuid>) -> Self {
        self.impersonator_id = id;
        self
    }
}

/// Writes an audit entry.
pub async fn write(db: impl PgExecutor<'_>, entry: &AuditEntry) -> DbResult<AuditLogId> {
    let id = AuditLogId::new();
    sqlx::query(
        "insert into audit_log (id, tenant_id, actor_kind, actor_id, impersonator_id, action, entity_type,
                                entity_id, ip, user_agent, changes, metadata)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id)
    .bind(entry.tenant_id)
    .bind(entry.actor_kind)
    .bind(entry.actor_id)
    .bind(entry.impersonator_id)
    .bind(&entry.action)
    .bind(&entry.entity_type)
    .bind(entry.entity_id)
    .bind(entry.ip)
    .bind(&entry.user_agent)
    .bind(&entry.changes)
    .bind(&entry.metadata)
    .execute(db)
    .await?;
    Ok(id)
}

/// Filters of [`list`].
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub actor_id: Option<Uuid>,
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    /// Prefix match on `action` (`invoice.` → all invoice actions).
    pub action_prefix: Option<String>,
}

/// Audit entries of a tenant, newest first.
pub async fn list<'a>(
    db: impl Acquire<'a, Database = Postgres>,
    tenant_id: TenantId,
    filter: &AuditFilter,
    page: Page,
) -> DbResult<Paged<AuditLogRow>> {
    let f = filter.clone();
    fetch_paged(
        db,
        "select *",
        |qb| {
            qb.push("from audit_log where tenant_id = ")
                .push_bind(tenant_id);
            if let Some(a) = f.actor_id {
                qb.push(" and actor_id = ").push_bind(a);
            }
            if let Some(t) = &f.entity_type {
                qb.push(" and entity_type = ").push_bind(t.clone());
            }
            if let Some(e) = f.entity_id {
                qb.push(" and entity_id = ").push_bind(e);
            }
            if let Some(p) = &f.action_prefix {
                qb.push(" and starts_with(action, ")
                    .push_bind(p.clone())
                    .push(")");
            }
        },
        "created_at desc, id desc",
        page,
    )
    .await
}

/// Table of each audited entity type that has an admin page.
const ENTITY_TABLES: &[(&str, &str)] = &[
    ("client", "clients"),
    ("invoice", "invoices"),
    ("order", "orders"),
    ("service", "services"),
    ("domain", "domains"),
    ("ticket", "tickets"),
    ("product", "products"),
    ("bundle", "product_bundles"),
    ("integration", "integrations"),
    ("page", "pages"),
    ("blog_post", "blog_posts"),
    ("kb_article", "kb_articles"),
    ("announcement", "announcements"),
    ("quote", "quotes"),
    ("task", "staff_tasks"),
    ("affiliate", "affiliates"),
    ("status_incident", "status_incidents"),
    ("status_component", "status_components"),
    ("mail_inbound_message", "mail_inbound_messages"),
];

/// Which of `ids` (audited entities of `entity_type`) still exist in the
/// tenant; an entry whose record was deleted must not link to it. Unknown
/// types have no records.
pub async fn existing_entities(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    entity_type: &str,
    ids: &[Uuid],
) -> DbResult<Vec<Uuid>> {
    let Some((_, table)) = ENTITY_TABLES.iter().find(|(t, _)| *t == entity_type) else {
        return Ok(Vec::new());
    };
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // `table` comes from the list above, never from input.
    Ok(sqlx::query_scalar(&format!(
        "select id from {table} where tenant_id = $1 and id = any($2)"
    ))
    .bind(tenant_id)
    .bind(ids)
    .fetch_all(db)
    .await?)
}
