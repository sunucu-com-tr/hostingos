//! `/admin/tools/import` — the WHMCS import wizard: source → connect & test
//! → preview (dry run) → mapping → import with live progress → report,
//! plus cancel, rollback and the cut-over checklist.
//!
//! Every page needs `import.manage` (owner only by default), every POST is
//! CSRF-checked by the global middleware and audited. Source credentials are
//! sealed into the run (`hos_import::crypto`) the moment they are entered
//! and never rendered back.

pub mod cli;

use std::collections::BTreeMap;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use hos_auth::rbac::Permission;
use hos_core::ids::TenantId;
use hos_core::net::OutboundPolicy;
use hos_db::repo::identity;
use hos_db::repo::import::{self as repo, CutoverStats, ImportRunRow, NewImportRun, RunMessage};
use hos_db::repo::integrations;
use hos_import::config::{
    ApiSourceConfig, DbSourceConfig, ImportOptions, Mappings, SourceConfig, SourceSecrets, TlsMode,
};
use hos_import::engine::{Analysis, Progress};
use hos_import::entity::Entity;
use hos_import::runner::{Runner, build_source};
use http::{HeaderValue, header};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{Htmx, PermissionMarker, RequirePermission};
use crate::forms::FormErrors;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// `import.manage`.
#[derive(Debug, Clone, Copy)]
pub struct ImportManage;
impl PermissionMarker for ImportManage {
    const PERMISSION: Permission = Permission::IMPORT_MANAGE;
}

type Staff = RequirePermission<ImportManage>;

const BASE: &str = "/admin/tools/import";
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(BASE, get(index))
        .route("/admin/tools/import/new", get(new_form).post(create))
        .route("/admin/tools/import/automation", post(resume_automation))
        .route("/admin/tools/import/{id}", get(show))
        .route("/admin/tools/import/{id}/progress", get(progress))
        .route("/admin/tools/import/{id}/options", post(save_options))
        .route("/admin/tools/import/{id}/cancel", post(cancel))
        .route("/admin/tools/import/{id}/rollback", post(rollback))
        .route("/admin/tools/import/{id}/forget", post(forget))
        .route("/admin/tools/import/{id}/report.json", get(report_json))
        .route("/admin/tools/import/{id}/report.csv", get(report_csv))
}

fn policy() -> OutboundPolicy {
    OutboundPolicy::global()
}

/// Starts a queued run in this process right away (the background worker
/// would pick it up too; whoever flips it to `running` first wins).
fn spawn_run(state: &AppState, tenant: TenantId, id: Uuid) {
    let runner = Runner::new(
        state.pool.clone(),
        state.secret_box.clone(),
        policy(),
        format!("web-{}", Uuid::now_v7()),
    );
    tokio::spawn(async move {
        if let Err(e) = runner.run_inline(tenant, id, None).await {
            tracing::debug!(run = %id, error = %e, "import: inline start skipped");
        }
    });
}

// ---------------------------------------------------------------------------
// views
// ---------------------------------------------------------------------------

/// A run in lists.
pub struct RunView {
    pub id: Uuid,
    pub label: String,
    pub kind: String,
    pub phase: String,
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub dry_run_at: Option<DateTime<Utc>>,
    pub has_secrets: bool,
    pub automation_paused: bool,
}

impl From<&ImportRunRow> for RunView {
    fn from(r: &ImportRunRow) -> Self {
        Self {
            id: r.id,
            label: r.source_label.clone(),
            kind: r.source_kind.clone(),
            phase: r.phase.clone(),
            status: r.status.clone(),
            error: r.error.clone(),
            created_at: r.created_at,
            finished_at: r.finished_at,
            dry_run_at: r.dry_run_at,
            has_secrets: r.has_secrets,
            automation_paused: r.automation_paused,
        }
    }
}

impl RunView {
    fn active(&self) -> bool {
        repo::ACTIVE_STATUSES.contains(&self.status.as_str())
    }

    /// Badge tone of the status.
    fn tone(&self) -> &'static str {
        match self.status.as_str() {
            "queued" | "running" | "rollback_queued" | "rolling_back" => "info",
            "rolled_back" => "",
            other => crate::view::status_tone(other),
        }
    }
}

/// One entity line of a progress / result table.
pub struct EntityLine {
    pub key: String,
    pub status: String,
    pub total: String,
    pub read: u64,
    pub created: u64,
    pub updated: u64,
    pub adopted: u64,
    pub skipped: u64,
    pub errors: u64,
    pub note: String,
    pub percent: u64,
}

fn entity_lines(p: &Progress) -> Vec<EntityLine> {
    Entity::ORDER
        .iter()
        .filter_map(|e| p.entities.get(e.key()).map(|ep| (e.key(), ep)))
        .map(|(k, e)| EntityLine {
            key: k.to_owned(),
            status: e.status.clone(),
            total: e.total.map_or_else(|| "—".into(), |t| t.to_string()),
            read: e.read,
            created: e.created,
            updated: e.updated,
            adopted: e.adopted,
            skipped: e.skipped,
            errors: e.errors,
            note: e.note.clone().unwrap_or_default(),
            percent: match (e.status.as_str(), e.total) {
                ("done" | "off" | "unsupported", _) => 100,
                (_, Some(t)) if t > 0 => (e.read * 100 / t).min(100),
                _ => 0,
            },
        })
        .collect()
}

/// A wizard mapping row: source value, current choice, options.
pub struct MapRow {
    pub field: String,
    pub source: String,
    pub detail: String,
    pub selected: String,
    pub options: Vec<(String, String)>,
}

/// Entity checkbox.
pub struct EntityChoice {
    pub key: String,
    pub group: String,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "admin/import/index.html")]
struct IndexPage {
    ctx: PageCtx,
    runs: Vec<RunView>,
    automation_active: bool,
    cutover: Option<(RunView, CutoverStats)>,
}
crate::page!(IndexPage);

#[derive(Template)]
#[template(path = "admin/import/new.html")]
struct NewPage {
    ctx: PageCtx,
    kind: String,
    host: String,
    port: String,
    database: String,
    user: String,
    tls: String,
    url: String,
    identifier: String,
    attach_url: String,
    attach_identifier: String,
    errors: FormErrors,
}
crate::page!(NewPage);

#[derive(Template)]
#[template(path = "admin/import/run.html")]
struct RunPage {
    ctx: PageCtx,
    run: RunView,
    step: usize,
    steps: Vec<String>,
    lines: Vec<EntityLine>,
    probe: Vec<String>,
    version: String,
    entities: Vec<EntityChoice>,
    options: ImportOptions,
    maps: Vec<MapRow>,
    analysis: Option<Analysis>,
    unsupported: Vec<(String, String, String)>,
    messages: Vec<RunMessage>,
    level: String,
    levels: Vec<(String, i64)>,
    post: Vec<(String, String)>,
    cutover: Option<CutoverStats>,
    automation_active: bool,
    blocked: Vec<String>,
    rolled_back: Vec<(String, String)>,
    /// The shop's own activity since the last import (a re-import then
    /// needs `confirm_live`).
    live: Option<String>,
}
crate::page!(RunPage);

impl RunPage {
    /// Badge tone of a report message level.
    fn level_tone(&self, level: &str) -> &'static str {
        match level {
            "error" => "danger",
            "warning" | "conflict" => "warning",
            "info" => "info",
            _ => "",
        }
    }
}

#[derive(Template)]
#[template(path = "admin/import/_progress.html")]
struct ProgressFragment {
    ctx: PageCtx,
    run: RunView,
    lines: Vec<EntityLine>,
    done: u64,
    total: u64,
    current: String,
}
crate::page!(ProgressFragment);

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<ImportRunRow> {
    repo::get_run(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

// ---------------------------------------------------------------------------
// index
// ---------------------------------------------------------------------------

async fn index(
    State(state): State<AppState>,
    tenant: Tenant,
    Staff { staff, .. }: Staff,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = repo::list_runs(&state.pool, tenant.id, 50).await?;
    let automation_active = repo::automation_active(&state.pool, tenant.id).await?;
    let mut cutover = None;
    if let Some(r) = rows
        .iter()
        .find(|r| r.phase == "import" && r.status == "completed")
    {
        let settings = hos_billing::settings::BillingSettings::load(&state.pool, tenant.id).await?;
        let stats = repo::cutover_stats(
            &state.pool,
            tenant.id,
            &r.source_key,
            Utc::now().date_naive(),
            settings.suspend_after_days,
            settings.terminate_after_days,
        )
        .await?;
        cutover = Some((RunView::from(r), stats));
    }
    let title = ctx.t("import.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&IndexPage {
        ctx,
        runs: rows.iter().map(RunView::from).collect(),
        automation_active,
        cutover,
    })
}

async fn resume_automation(
    State(state): State<AppState>,
    tenant: Tenant,
    _s: Staff,
    au: Audit,
    ctx: PageCtx,
) -> AppResult<Response> {
    let mut conn = state.pool.acquire().await?;
    repo::set_automation_active(&mut conn, tenant.id, true).await?;
    drop(conn);
    for r in repo::list_runs(&state.pool, tenant.id, 50).await? {
        if r.automation_paused {
            repo::set_automation_paused(&state.pool, tenant.id, r.id, false).await?;
        }
    }
    au.log(&state, "import.automation_resumed", None, json!({}))
        .await?;
    Ok(done(&ctx, BASE, "import.cutover.resumed"))
}

// ---------------------------------------------------------------------------
// new: source → connect & test
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct KindQuery {
    kind: Option<String>,
}

fn new_page(ctx: PageCtx, kind: &str, f: Option<&FormData>, errors: FormErrors) -> NewPage {
    let v = |k: &str, d: &str| f.map_or_else(|| d.to_owned(), |f| f.text(k, 300));
    NewPage {
        kind: if kind == "api" {
            "api".into()
        } else {
            "db".into()
        },
        host: v("host", ""),
        port: v("port", "3306"),
        database: v("database", ""),
        user: v("user", ""),
        tls: v("tls", "verify"),
        url: v("url", ""),
        identifier: v("identifier", ""),
        attach_url: v("attach_url", ""),
        attach_identifier: v("attach_identifier", ""),
        errors,
        ctx,
    }
}

async fn new_form(
    Staff { staff, .. }: Staff,
    Query(q): Query<KindQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("import.new.title");
    common::page(&mut ctx, &staff, &title, &[("import.title", BASE)]);
    render(&new_page(
        ctx,
        q.kind.as_deref().unwrap_or("db"),
        None,
        FormErrors::default(),
    ))
}

/// Source configuration and secrets from the form (errors keyed by field).
fn parse_source(
    f: &FormData,
    ctx: &PageCtx,
    errors: &mut FormErrors,
) -> Option<(SourceConfig, SourceSecrets)> {
    let secret =
        |k: &str| Some(f.raw(k).trim().to_owned()).filter(|s| !s.is_empty() && s.len() <= 500);
    let required = |errors: &mut FormErrors, k: &str| {
        if f.text(k, 300).is_empty() {
            errors.add(k, ctx.t("admin.common.required"));
        }
    };
    if f.text("kind", 10) == "api" {
        required(errors, "url");
        required(errors, "identifier");
        let api_secret = secret("api_secret");
        if api_secret.is_none() {
            errors.add("api_secret", ctx.t("admin.common.required"));
        }
        if errors.any() {
            return None;
        }
        Some((
            SourceConfig::WhmcsApi(ApiSourceConfig {
                url: f.text("url", 300),
                identifier: f.text("identifier", 200),
            }),
            SourceSecrets {
                api_secret,
                api_access_key: secret("api_access_key"),
                ..SourceSecrets::default()
            },
        ))
    } else {
        for k in ["host", "database", "user"] {
            required(errors, k);
        }
        let port = match f.int("port") {
            Ok(None) => 3306,
            Ok(Some(p)) if (1..=65535).contains(&p) => u16::try_from(p).unwrap_or(3306),
            _ => {
                errors.add("port", ctx.t("import.new.port_invalid"));
                3306
            }
        };
        if errors.any() {
            return None;
        }
        let tls = match f.text("tls", 20).as_str() {
            "disabled" => TlsMode::Disabled,
            "insecure" => TlsMode::Insecure,
            _ => TlsMode::Verify,
        };
        let attachments_api = f.opt("attach_url", 300).map(|url| ApiSourceConfig {
            url,
            identifier: f.text("attach_identifier", 200),
        });
        Some((
            SourceConfig::WhmcsDb(DbSourceConfig {
                host: f.text("host", 253),
                port,
                database: f.text("database", 64),
                user: f.text("user", 80),
                tls,
                attachments_api,
            }),
            SourceSecrets {
                db_password: secret("db_password"),
                cc_encryption_hash: secret("cc_hash"),
                api_secret: secret("attach_secret"),
                ..SourceSecrets::default()
            },
        ))
    }
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    Staff { staff, .. }: Staff,
    au: Audit,
    mut ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let kind = f.text("kind", 10);
    let parsed = parse_source(&f, &ctx, &mut errors);
    let probe = match &parsed {
        Some((config, secrets)) => {
            let attempt = async {
                let mut source = build_source(config, secrets, policy())?;
                source.probe().await
            };
            match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
                Ok(Ok(p)) => Some(p),
                Ok(Err(e)) => {
                    errors.add_general(ctx.tv("import.new.connect_failed", "reason", &e));
                    None
                }
                Err(_) => {
                    errors.add_general(ctx.t("import.new.connect_timeout"));
                    None
                }
            }
        }
        None => None,
    };
    let (Some((config, secrets)), Some(probe)) = (parsed, probe) else {
        let title = ctx.t("import.new.title");
        common::page(&mut ctx, &staff, &title, &[("import.title", BASE)]);
        let page = new_page(ctx, &kind, Some(&f), errors);
        return Ok((
            http::StatusCode::UNPROCESSABLE_ENTITY,
            axum::response::Html(page.render()?),
        )
            .into_response());
    };
    let id = Uuid::now_v7();
    let run = repo::create_run(
        &state.pool,
        tenant.id,
        &NewImportRun {
            id,
            source_kind: config.kind().into(),
            source_key: config.key(),
            source_label: config.label(),
            source_config: serde_json::to_value(&config).unwrap_or_default(),
            options: serde_json::to_value(ImportOptions::default()).unwrap_or_default(),
            started_by: Some(staff.user_id.into()),
        },
    )
    .await?;
    let sealed = hos_import::crypto::seal_secrets(&state.secret_box, tenant.id, run.id, &secrets)?;
    repo::set_run_secrets(&state.pool, tenant.id, run.id, &sealed).await?;
    repo::update_draft(
        &state.pool,
        tenant.id,
        run.id,
        None,
        None,
        Some(&json!({ "probe": probe })),
    )
    .await?;
    au.log(
        &state,
        "import.create",
        Some(("import_run", run.id)),
        json!({ "source": run.source_label, "kind": run.source_kind }),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("{BASE}/{}", run.id),
        "import.new.connected",
    ))
}

// ---------------------------------------------------------------------------
// run page
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct LevelQuery {
    level: Option<String>,
}

fn step_of(r: &RunView) -> usize {
    match (r.phase.as_str(), r.status.as_str()) {
        (_, "rolled_back" | "rollback_queued" | "rolling_back") => 6,
        ("import", "completed" | "failed" | "cancelled") => 6,
        ("import", _) => 5,
        ("dry_run", "queued" | "running") => 3,
        (_, "draft") if r.dry_run_at.is_some() => 4,
        _ => 2,
    }
}

async fn map_rows(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    a: &Analysis,
    maps: &Mappings,
) -> AppResult<Vec<MapRow>> {
    let ints = integrations::list_integrations(&state.pool, tenant.id, None).await?;
    let of_category = |cat: &str| -> Vec<(String, String)> {
        ints.iter()
            .filter(|i| i.category.as_str() == cat)
            .map(|i| {
                (
                    i.id.into_uuid().to_string(),
                    format!("{} ({})", i.name, i.kind),
                )
            })
            .collect()
    };
    let none = (String::new(), ctx.t("import.map.none"));
    let mut rows = Vec::new();
    for (module, u) in &a.gateways {
        let mut options = vec![none.clone()];
        options.extend(of_category("gateway"));
        let selected = maps
            .gateways
            .get(module)
            .cloned()
            .or_else(|| u.suggested.map(|s| s.to_string()))
            .unwrap_or_default();
        rows.push(MapRow {
            field: format!("map_gateway:{module}"),
            source: module.clone(),
            detail: ctx.tv("import.map.rows", "count", u.count),
            selected,
            options,
        });
    }
    for (module, u) in &a.registrars {
        let mut options = vec![none.clone()];
        options.extend(of_category("registrar"));
        let selected = maps
            .registrars
            .get(module)
            .cloned()
            .or_else(|| u.suggested.map(|s| s.to_string()))
            .unwrap_or_default();
        rows.push(MapRow {
            field: format!("map_registrar:{module}"),
            source: module.clone(),
            detail: ctx.tv("import.map.rows", "count", u.count),
            selected,
            options,
        });
    }
    for s in &a.servers {
        let mut options = vec![("skip".to_owned(), ctx.t("import.map.skip"))];
        for k in [
            "cpanel",
            "plesk",
            "directadmin",
            "cyberpanel",
            "virtualizor",
            "proxmox",
            "hetzner",
            "vmware",
        ] {
            options.push((
                format!("create:{k}"),
                ctx.tv("import.map.create_as", "kind", k),
            ));
        }
        let mut existing = of_category("panel");
        existing.extend(of_category("virt"));
        options.extend(existing);
        let default = s
            .kind
            .as_ref()
            .map_or_else(|| "skip".to_owned(), |k| format!("create:{k}"));
        rows.push(MapRow {
            field: format!("map_server:{}", s.id),
            source: format!("{} — {}", s.name, s.host),
            detail: s.module.clone(),
            selected: maps
                .servers
                .get(&s.id.to_string())
                .cloned()
                .unwrap_or(default),
            options,
        });
    }
    if !a.products.is_empty() {
        let products = repo::product_choices(&state.pool, tenant.id).await?;
        for p in &a.products {
            let mut options = vec![("create".to_owned(), ctx.t("import.map.create"))];
            options.extend(
                products
                    .iter()
                    .map(|(id, name)| (id.to_string(), name.clone())),
            );
            rows.push(MapRow {
                field: format!("map_product:{}", p.id),
                source: p.name.clone(),
                detail: format!("{} · {}", p.product_type, p.module),
                selected: maps
                    .products
                    .get(&p.id.to_string())
                    .cloned()
                    .unwrap_or_else(|| "create".into()),
                options,
            });
        }
    }
    if !a.roles.is_empty() {
        let roles = identity::list_roles(&state.pool, tenant.id).await?;
        let staff_hint = ctx.t("import.map.staff_inactive");
        for (id, name) in &a.roles {
            let mut options = vec![none.clone()];
            options.extend(
                roles
                    .iter()
                    .filter(|r| !crate::routes::admin::staff::is_owner_role(r))
                    .map(|r| (r.slug.clone(), r.name.clone())),
            );
            rows.push(MapRow {
                field: format!("map_role:{id}"),
                source: name.clone(),
                detail: staff_hint.clone(),
                selected: maps.roles.get(id).cloned().unwrap_or_default(),
                options,
            });
        }
    }
    Ok(rows)
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    Staff { staff, .. }: Staff,
    Path(id): Path<Uuid>,
    Query(q): Query<LevelQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let run = RunView::from(&row);
    let progress = Progress::from_json(&row.progress);
    let options: ImportOptions = serde_json::from_value(row.options.clone()).unwrap_or_default();
    let maps: Mappings = serde_json::from_value(row.mappings.clone()).unwrap_or_default();
    let analysis: Option<Analysis> = row
        .preview
        .get("analysis")
        .and_then(|a| serde_json::from_value(a.clone()).ok());
    let probe: Vec<String> = row.discovery["probe"]["notes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let version = row.discovery["probe"]["version"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let maps_rows = match (&analysis, run.status.as_str()) {
        (Some(a), "draft") => map_rows(&state, &tenant, &ctx, a, &maps).await?,
        _ => Vec::new(),
    };
    let entities = Entity::ORDER
        .iter()
        .map(|e| EntityChoice {
            key: e.key().into(),
            group: e.info().group.into(),
            checked: options.wants(*e),
        })
        .collect();
    let result = if row.phase == "dry_run" {
        &row.preview
    } else {
        &row.summary
    };
    let unsupported = result["unsupported"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| {
                    let key = u.get(0)?.as_str()?.to_owned();
                    let reason = u.get(1)?.as_str()?.to_owned();
                    let n = u
                        .get(2)
                        .and_then(Value::as_u64)
                        .map_or_else(|| "—".into(), |n| n.to_string());
                    Some((key, reason, n))
                })
                .collect()
        })
        .unwrap_or_default();
    let phase = if run.status == "draft" && run.dry_run_at.is_some() {
        "dry_run"
    } else {
        row.phase.as_str()
    };
    let all = repo::list_messages(&state.pool, tenant.id, id, Some(phase), 2000).await?;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for m in &all {
        *counts.entry(m.level.clone()).or_default() += 1;
    }
    let level = q.level.unwrap_or_default();
    let messages: Vec<RunMessage> = all
        .into_iter()
        .filter(|m| level.is_empty() || m.level == level)
        .take(300)
        .collect();
    let post: Vec<(String, String)> = row.summary["post"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.to_string().trim_matches('"').to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let cutover = if row.phase == "import" && run.status == "completed" {
        let settings = hos_billing::settings::BillingSettings::load(&state.pool, tenant.id).await?;
        Some(
            repo::cutover_stats(
                &state.pool,
                tenant.id,
                &row.source_key,
                Utc::now().date_naive(),
                settings.suspend_after_days,
                settings.terminate_after_days,
            )
            .await?,
        )
    } else {
        None
    };
    let blocked = row.summary["rollback_blocked"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|b| {
                    format!(
                        "{} × {}.{} → {}",
                        b["count"],
                        b["table"].as_str().unwrap_or(""),
                        b["column"].as_str().unwrap_or(""),
                        b["references"].as_str().unwrap_or("")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let rolled_back = row.summary["rolled_back"]
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.to_string())).collect())
        .unwrap_or_default();
    let steps = [
        "source", "connect", "preview", "mapping", "import", "report",
    ]
    .iter()
    .map(|s| ctx.t(&format!("import.steps.{s}")))
    .collect();
    let live = match (run.status.as_str(), run.dry_run_at) {
        ("draft", Some(_)) => repo::live_activity(&state.pool, tenant.id)
            .await?
            .map(|l| live_text(&ctx, &l)),
        _ => None,
    };
    let title = ctx.tv("import.run.title", "source", &run.label);
    common::page(&mut ctx, &staff, &title, &[("import.title", BASE)]);
    render(&RunPage {
        live,
        step: step_of(&run),
        steps,
        lines: entity_lines(&progress),
        probe,
        version,
        entities,
        options,
        maps: maps_rows,
        analysis,
        unsupported,
        messages,
        level,
        levels: counts.into_iter().collect(),
        post,
        cutover,
        automation_active: repo::automation_active(&state.pool, tenant.id).await?,
        blocked,
        rolled_back,
        run,
        ctx,
    })
}

/// The warning about live activity since the last import.
fn live_text(ctx: &PageCtx, l: &repo::LiveActivity) -> String {
    ctx.tv2(
        "import.run.live_activity",
        "count",
        l.records,
        "since",
        ctx.datetime(&l.since),
    )
}

async fn progress(
    State(state): State<AppState>,
    tenant: Tenant,
    _s: Staff,
    Path(id): Path<Uuid>,
    hx: Htmx,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let run = RunView::from(&row);
    let p = Progress::from_json(&row.progress);
    let (done, total) = p.fraction();
    let fragment = ProgressFragment {
        lines: entity_lines(&p),
        done,
        total,
        current: p.current.clone().unwrap_or_default(),
        run,
        ctx,
    };
    let mut res = axum::response::Html(fragment.render()?).into_response();
    if hx.request && !fragment.run.active() {
        // Finished: reload the page to show the report.
        res.headers_mut()
            .insert("HX-Refresh", HeaderValue::from_static("true"));
    }
    Ok(res)
}

// ---------------------------------------------------------------------------
// options / mappings → preview / import
// ---------------------------------------------------------------------------

/// Options and mappings from the form.
fn parse_options(f: &FormData) -> (ImportOptions, Mappings) {
    let mut o = ImportOptions {
        entities: f
            .all("entity")
            .iter()
            .filter_map(|k| Entity::parse(k))
            .collect(),
        ..ImportOptions::default()
    };
    if o.entities.is_empty() {
        // Nothing ticked would mean "defaults": keep the explicit choice.
        o.entities.insert(Entity::Currencies);
    }
    o.verified_emails = f.flag("verified_emails");
    o.pause_automation = f.flag("pause_automation");
    o.service_passwords = f.flag("service_passwords");
    o.server_credentials = f.flag("server_credentials");
    o.grace_days = f
        .int("grace_days")
        .ok()
        .flatten()
        .map_or(14, |d| u32::try_from(d.clamp(0, 365)).unwrap_or(14));
    o.activity_log_limit = f
        .int("activity_log_limit")
        .ok()
        .flatten()
        .map_or(5000, |d| u32::try_from(d.clamp(0, 100_000)).unwrap_or(5000));
    o.attachment_max_bytes = f
        .int("attachment_max_mb")
        .ok()
        .flatten()
        .map_or(10, |d| u64::try_from(d.clamp(1, 25)).unwrap_or(10))
        * 1024
        * 1024;
    o.content_locale = f
        .opt("content_locale", 2)
        .filter(|l| l == "tr" || l == "en");
    let mut m = Mappings::default();
    for (k, v) in &f.0 {
        if let Some(rest) = k.strip_prefix("map_") {
            apply_mapping(&mut m, rest, v);
        }
    }
    (o, m)
}

/// One `gateway:<module>` / `registrar:<module>` / `server:<id>` /
/// `product:<id>` / `role:<id>` mapping; `false` for an unknown kind.
pub(crate) fn apply_mapping(m: &mut Mappings, key: &str, value: &str) -> bool {
    let v = value.trim().chars().take(80).collect::<String>();
    let Some((kind, id)) = key.split_once(':') else {
        return false;
    };
    let (map, max) = match kind {
        "gateway" => (&mut m.gateways, 80),
        "registrar" => (&mut m.registrars, 80),
        "server" => (&mut m.servers, 20),
        "product" => (&mut m.products, 20),
        "role" => (&mut m.roles, 20),
        _ => return false,
    };
    map.insert(id.chars().take(max).collect(), v);
    true
}

/// Mapped integrations / products / roles must belong to this tenant.
pub(crate) async fn validate_mappings(
    pool: &hos_db::PgPool,
    tenant: TenantId,
    m: &Mappings,
) -> AppResult<bool> {
    let ints: Vec<Uuid> = integrations::list_integrations(pool, tenant, None)
        .await?
        .into_iter()
        .map(|i| i.id.into_uuid())
        .collect();
    let products: Vec<Uuid> = repo::product_choices(pool, tenant)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let roles: Vec<String> = identity::list_roles(pool, tenant)
        .await?
        .into_iter()
        .map(|r| r.slug)
        .collect();
    let uuid_ok =
        |v: &str, set: &[Uuid]| v.is_empty() || Uuid::parse_str(v).is_ok_and(|u| set.contains(&u));
    let ok = m.gateways.values().all(|v| uuid_ok(v, &ints))
        && m.registrars.values().all(|v| uuid_ok(v, &ints))
        && m.servers
            .values()
            .all(|v| v == "skip" || v.starts_with("create:") || uuid_ok(v, &ints))
        && m.products
            .values()
            .all(|v| v == "create" || uuid_ok(v, &products))
        && m.roles.values().all(|v| v.is_empty() || roles.contains(v));
    Ok(ok)
}

/// Staff role mappings `actor` may make (the rule of granting a role in
/// Staff, `staff::covers`): mapping roles at all needs `staff.manage`, a
/// mapped role's permissions must all be the actor's own, and the owner role
/// is never a target. `actor = None` is the CLI, which acts as the server
/// operator (shell + database access): only the owner rule applies there.
/// Returns the locale key of the refusal.
pub(crate) async fn role_mapping_error(
    pool: &hos_db::PgPool,
    tenant: TenantId,
    actor: Option<&hos_auth::rbac::PermissionSet>,
    m: &Mappings,
) -> AppResult<Option<&'static str>> {
    let wanted: Vec<&String> = m.roles.values().filter(|v| !v.is_empty()).collect();
    if wanted.is_empty() {
        return Ok(None);
    }
    if let Some(actor) = actor
        && !actor.allows(hos_auth::rbac::Permission::STAFF_MANAGE)
    {
        return Ok(Some("import.map.role_needs_staff_manage"));
    }
    let roles = identity::list_roles(pool, tenant).await?;
    for slug in wanted {
        let Some(role) = roles.iter().find(|r| &r.slug == slug) else {
            return Ok(Some("import.map.invalid"));
        };
        if crate::routes::admin::staff::is_owner_role(role) {
            return Ok(Some("import.map.role_owner"));
        }
        if let Some(actor) = actor {
            let grants = identity::role_permissions(pool, tenant, role.id).await?;
            if !crate::routes::admin::staff::covers(actor, &grants) {
                return Ok(Some("import.map.role_escalation"));
            }
        }
    }
    Ok(None)
}

async fn save_options(
    State(state): State<AppState>,
    tenant: Tenant,
    Staff { staff, .. }: Staff,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let back = format!("{BASE}/{id}");
    if row.status != "draft" {
        return Ok(failed(&ctx, &back, ctx.t("import.run.not_draft")));
    }
    let (options, mappings) = parse_options(&f);
    if !validate_mappings(&state.pool, tenant.id, &mappings).await? {
        return Ok(failed(&ctx, &back, ctx.t("import.map.invalid")));
    }
    if let Some(key) =
        role_mapping_error(&state.pool, tenant.id, Some(&staff.permissions), &mappings).await?
    {
        return Ok(failed(&ctx, &back, ctx.t(key)));
    }
    let o = serde_json::to_value(&options).unwrap_or_default();
    let m = serde_json::to_value(&mappings).unwrap_or_default();
    repo::update_draft(&state.pool, tenant.id, id, Some(&o), Some(&m), None).await?;
    let action = f.text("action", 20);
    let phase = match action.as_str() {
        "preview" => "dry_run",
        "start" => {
            if row.dry_run_at.is_none() {
                return Ok(failed(&ctx, &back, ctx.t("import.run.preview_first")));
            }
            if !f.flag("confirm_cutover") {
                return Ok(failed(&ctx, &back, ctx.t("import.run.confirm_required")));
            }
            // The shop is live since the last import: a re-import only
            // with the explicit confirmation.
            if let Some(live) = repo::live_activity(&state.pool, tenant.id).await?
                && !f.flag("confirm_live")
            {
                return Ok(failed(
                    &ctx,
                    &back,
                    format!(
                        "{} {}",
                        live_text(&ctx, &live),
                        ctx.t("import.run.confirm_live_required")
                    ),
                ));
            }
            "import"
        }
        _ => {
            au.log(
                &state,
                "import.options",
                Some(("import_run", id)),
                json!({ "options": o, "mappings": m }),
            )
            .await?;
            return Ok(done(&ctx, &back, "import.run.saved"));
        }
    };
    if !row.has_secrets {
        return Ok(failed(&ctx, &back, ctx.t("import.run.secrets_gone")));
    }
    match repo::queue_run(
        &state.pool,
        tenant.id,
        id,
        phase,
        Some(staff.user_id.into()),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return Ok(failed(&ctx, &back, ctx.t("import.run.not_draft"))),
        Err(e) => return Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
    au.log(
        &state,
        if phase == "import" {
            "import.start"
        } else {
            "import.preview"
        },
        Some(("import_run", id)),
        json!({ "source": row.source_label, "options": o, "mappings": m }),
    )
    .await?;
    spawn_run(&state, tenant.id, id);
    Ok(done(
        &ctx,
        &back,
        if phase == "import" {
            "import.run.started"
        } else {
            "import.run.preview_started"
        },
    ))
}

async fn cancel(
    State(state): State<AppState>,
    tenant: Tenant,
    _s: Staff,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    load(&state, &tenant, id).await?;
    let back = format!("{BASE}/{id}");
    if !repo::request_cancel(&state.pool, tenant.id, id).await? {
        return Ok(failed(&ctx, &back, ctx.t("import.run.not_running")));
    }
    au.log(&state, "import.cancel", Some(("import_run", id)), json!({}))
        .await?;
    Ok(done(&ctx, &back, "import.run.cancel_requested"))
}

async fn rollback(
    State(state): State<AppState>,
    tenant: Tenant,
    _s: Staff,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let back = format!("{BASE}/{id}");
    if !f.flag("confirm") {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("import.rollback.confirm_required"),
        ));
    }
    let check = hos_import::rollback::check(&state.pool, tenant.id, id)
        .await
        .map_err(import_error)?;
    if let Some(msg) = check.blocked_message() {
        return Ok(failed(
            &ctx,
            &back,
            ctx.tv("import.rollback.blocked", "reason", msg),
        ));
    }
    match repo::queue_rollback(&state.pool, tenant.id, id).await {
        Ok(true) => {}
        Ok(false) => return Ok(failed(&ctx, &back, ctx.t("import.rollback.not_possible"))),
        Err(e) => return Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
    au.log(
        &state,
        "import.rollback",
        Some(("import_run", id)),
        json!({ "source": row.source_label }),
    )
    .await?;
    spawn_run(&state, tenant.id, id);
    Ok(done(&ctx, &back, "import.rollback.queued"))
}

async fn forget(
    State(state): State<AppState>,
    tenant: Tenant,
    _s: Staff,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let back = format!("{BASE}/{id}");
    if row.status != "draft" {
        return Ok(failed(&ctx, &back, ctx.t("import.run.not_draft")));
    }
    repo::wipe_run_secrets(&state.pool, tenant.id, id).await?;
    au.log(
        &state,
        "import.forget_credentials",
        Some(("import_run", id)),
        json!({}),
    )
    .await?;
    Ok(done(&ctx, &back, "import.run.forgotten"))
}

fn import_error(e: hos_import::ImportError) -> AppError {
    match e {
        hos_import::ImportError::Core(c) => AppError::from(c),
        other => AppError::internal(other),
    }
}

fn attachment(body: Vec<u8>, content_type: &'static str, filename: String) -> Response {
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn report_json(
    State(state): State<AppState>,
    tenant: Tenant,
    _s: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let report = hos_import::report::json_report(&state.pool, tenant.id, &row)
        .await
        .map_err(import_error)?;
    let body = serde_json::to_vec_pretty(&report).map_err(AppError::internal)?;
    Ok(attachment(
        body,
        "application/json",
        format!("import-{}.json", row.id.simple()),
    ))
}

async fn report_csv(
    State(state): State<AppState>,
    tenant: Tenant,
    _s: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let row = load(&state, &tenant, id).await?;
    let csv = hos_import::report::csv_report(&state.pool, tenant.id, &row)
        .await
        .map_err(import_error)?;
    let mut body = "\u{feff}".as_bytes().to_vec();
    body.extend_from_slice(csv.as_bytes());
    Ok(attachment(
        body,
        "text/csv; charset=utf-8",
        format!("import-{}.csv", row.id.simple()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_and_mappings_from_form() {
        let f = FormData::from_bytes(
            b"entity=clients&entity=invoices&entity=bogus&grace_days=999&pause_automation=1&attachment_max_mb=100\
              &map_gateway%3Apaypal=&map_server%3A3=skip&map_product%3A1=create&map_role%3A1=support&content_locale=xx",
        );
        let (o, m) = parse_options(&f);
        assert_eq!(o.entities.len(), 2);
        assert_eq!(o.grace_days, 365);
        assert!(o.pause_automation);
        assert!(!o.verified_emails);
        assert_eq!(o.attachment_max_bytes, 25 * 1024 * 1024);
        assert_eq!(o.content_locale, None);
        assert_eq!(m.gateways.get("paypal").map(String::as_str), Some(""));
        assert_eq!(m.servers.get("3").map(String::as_str), Some("skip"));
        assert_eq!(m.roles.get("1").map(String::as_str), Some("support"));
    }
}
