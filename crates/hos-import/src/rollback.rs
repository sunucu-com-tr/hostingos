//! Rolling an import back: deletes every row the run **created** (adopted
//! rows and rows only updated by a re-sync stay), in one transaction, and
//! only while nothing outside the run references them — an invoice paid,
//! a ticket opened or a service ordered since the import blocks it.

use std::collections::BTreeMap;

use hos_core::ids::TenantId;
use hos_db::enums::ActorKind;
use hos_db::repo::audit::{self, AuditEntry};
use hos_db::repo::import::{self as repo, BlockingReference};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ImportResult;

/// Target tables, children first.
pub const ROLLBACK_ORDER: &[&str] = &[
    "audit_log",
    "status_incident_updates",
    "status_incidents",
    "downloads",
    "download_categories",
    "fraud_bans",
    "staff_tasks",
    "affiliate_commissions",
    "affiliate_payouts",
    "affiliate_referrals",
    "affiliates",
    "quote_items",
    "quotes",
    "ticket_attachments",
    "ticket_messages",
    "tickets",
    "canned_responses",
    "kb_articles",
    "kb_categories",
    "announcements",
    "ticket_departments",
    "refunds",
    "credits",
    "transactions",
    "invoice_items",
    "service_addons",
    "domains",
    "services",
    "orders",
    "invoices",
    "coupons",
    "tlds",
    "product_addons",
    "config_option_values",
    "config_options",
    "config_option_groups",
    "products",
    "product_groups",
    "client_notes",
    "contacts",
    "clients",
    "users",
    "integrations",
    "client_groups",
    "tax_rules",
    "currencies",
];

/// Result of a rollback attempt.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RollbackOutcome {
    /// Rows deleted per table (empty when blocked).
    pub deleted: BTreeMap<String, u64>,
    /// Why it could not run.
    pub blocked: Vec<BlockingReference>,
}

impl RollbackOutcome {
    /// Human summary of the blocking references.
    pub fn blocked_message(&self) -> Option<String> {
        if self.blocked.is_empty() {
            return None;
        }
        Some(format!(
            "rollback blocked: {}",
            self.blocked
                .iter()
                .map(|b| format!(
                    "{} {} row(s) reference imported {}",
                    b.count, b.table, b.references
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

/// What a rollback of `run_id` would hit (no changes).
pub async fn check(pool: &PgPool, tenant: TenantId, run_id: Uuid) -> ImportResult<RollbackOutcome> {
    let created = repo::created_by_run(pool, tenant, run_id).await?;
    let mut conn = pool.acquire().await?;
    let blocked = repo::blocking_references(&mut conn, tenant, &created).await?;
    let deleted = created
        .iter()
        .map(|(t, ids)| (t.clone(), ids.len() as u64))
        .collect();
    Ok(RollbackOutcome { deleted, blocked })
}

/// Deletes what `run_id` created. Blocked → nothing changes.
pub async fn rollback(
    pool: &PgPool,
    tenant: TenantId,
    run_id: Uuid,
    restore_automation: bool,
) -> ImportResult<RollbackOutcome> {
    let created = repo::created_by_run(pool, tenant, run_id).await?;
    let mut tx = pool.begin().await?;
    let blocked = repo::blocking_references(&mut tx, tenant, &created).await?;
    if !blocked.is_empty() {
        tx.rollback().await?;
        return Ok(RollbackOutcome {
            deleted: BTreeMap::new(),
            blocked,
        });
    }
    let deleted = repo::delete_created(&mut tx, tenant, &created, ROLLBACK_ORDER).await?;
    repo::forget_created(&mut tx, tenant, run_id).await?;
    if restore_automation {
        repo::set_automation_active(&mut tx, tenant, true).await?;
    }
    let entry = AuditEntry::new(Some(tenant), ActorKind::System, None, "import.rolled_back")
        .metadata(json!({ "run_id": run_id, "deleted": deleted }));
    audit::write(&mut *tx, &entry).await?;
    tx.commit().await?;
    if restore_automation {
        repo::set_automation_paused(pool, tenant, run_id, false).await?;
    }
    Ok(RollbackOutcome {
        deleted,
        blocked: Vec::new(),
    })
}
