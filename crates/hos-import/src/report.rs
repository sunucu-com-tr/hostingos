//! Downloadable run report (JSON and CSV): per-entity counters, the
//! unsupported list and every report line (skipped / warning / conflict /
//! error) with its WHMCS id.

use hos_core::ids::TenantId;
use hos_db::repo::import::{self as repo, ImportRunRow};
use serde_json::{Value as Json, json};
use sqlx::PgPool;

use crate::engine::Progress;
use crate::error::ImportResult;

/// Lines included in a downloaded report.
pub const REPORT_LINES: i64 = 50_000;

/// The report as JSON.
pub async fn json_report(
    pool: &PgPool,
    tenant: TenantId,
    run: &ImportRunRow,
) -> ImportResult<Json> {
    let messages = repo::list_messages(pool, tenant, run.id, None, REPORT_LINES).await?;
    Ok(json!({
        "run": {
            "id": run.id,
            "source": run.source_label,
            "source_kind": run.source_kind,
            "phase": run.phase,
            "status": run.status,
            "error": run.error,
            "created_at": run.created_at,
            "started_at": run.started_at,
            "finished_at": run.finished_at,
            "rolled_back_at": run.rolled_back_at,
        },
        "options": run.options,
        "progress": run.progress,
        "preview": run.preview,
        "summary": run.summary,
        "messages": messages.iter().map(|m| json!({
            "phase": m.phase,
            "entity": m.entity,
            "level": m.level,
            "source_id": m.source_id,
            "code": m.code,
            "message": m.message,
            "at": m.created_at,
        })).collect::<Vec<_>>(),
    }))
}

/// One CSV field: quoted, quotes doubled, and spreadsheet formulas
/// neutralised (`=`, `+`, `-`, `@`, tab, CR at the start).
pub fn csv_field(s: &str) -> String {
    let guarded = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{s}")
    } else {
        s.to_owned()
    };
    format!("\"{}\"", guarded.replace('"', "\"\""))
}

fn csv_line(fields: &[&str]) -> String {
    let mut line = fields
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",");
    line.push_str("\r\n");
    line
}

/// The report as CSV.
pub async fn csv_report(
    pool: &PgPool,
    tenant: TenantId,
    run: &ImportRunRow,
) -> ImportResult<String> {
    let mut out = csv_line(&[
        "section",
        "entity",
        "level",
        "source_id",
        "code",
        "message",
        "read",
        "created",
        "updated",
        "adopted",
        "skipped",
        "errors",
    ]);
    let progress = Progress::from_json(&run.progress);
    for (entity, e) in &progress.entities {
        out.push_str(&csv_line(&[
            "entity",
            entity,
            &e.status,
            "",
            "",
            e.note.as_deref().unwrap_or(""),
            &e.read.to_string(),
            &e.created.to_string(),
            &e.updated.to_string(),
            &e.adopted.to_string(),
            &e.skipped.to_string(),
            &e.errors.to_string(),
        ]));
    }
    for m in repo::list_messages(pool, tenant, run.id, None, REPORT_LINES).await? {
        out.push_str(&csv_line(&[
            &m.phase,
            &m.entity,
            &m.level,
            m.source_id.as_deref().unwrap_or(""),
            &m.code,
            &m.message,
            "",
            "",
            "",
            "",
            "",
            "",
        ]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_are_quoted_and_formulas_neutralised() {
        assert_eq!(csv_field("plain"), "\"plain\"");
        assert_eq!(csv_field("a \"b\""), "\"a \"\"b\"\"\"");
        assert_eq!(csv_field("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(csv_field("-1"), "\"'-1\"");
        assert_eq!(csv_field("@x"), "\"'@x\"");
    }
}
