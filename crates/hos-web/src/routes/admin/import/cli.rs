//! `hostingos import …` — the WHMCS import from the command line.
//!
//! Source secrets (database password, `cc_encryption_hash`, API secret /
//! access key) are read from environment variables or an interactive
//! no-echo prompt, never from argv (argv is visible in `ps` and shell
//! history). The run goes through the same states as the wizard: connect &
//! probe → dry run (preview + conflicts) → confirmation → import, so the
//! web UI shows CLI runs too and can roll them back.

use std::io::{IsTerminal as _, Write as _};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use hos_core::ids::TenantId;
use hos_core::net::OutboundPolicy;
use hos_db::enums::ActorKind;
use hos_db::repo::audit::{self, AuditEntry};
use hos_db::repo::import::{self as repo, ImportRunRow, NewImportRun};
use hos_db::repo::platform;
use hos_import::config::{
    ApiSourceConfig, DbSourceConfig, ImportOptions, Mappings, SourceConfig, SourceSecrets, TlsMode,
};
use hos_import::engine::{Analysis, Progress};
use hos_import::entity::Entity;
use hos_import::runner::{Runner, build_source};
use serde_json::json;
use uuid::Uuid;

use crate::state::AppState;

/// Environment variable holding the WHMCS database password.
pub const ENV_DB_PASSWORD: &str = "HOS_IMPORT_DB_PASSWORD";
/// Environment variable holding the WHMCS `cc_encryption_hash`.
pub const ENV_CC_HASH: &str = "HOS_IMPORT_CC_HASH";
/// Environment variable holding the WHMCS API secret.
pub const ENV_API_SECRET: &str = "HOS_IMPORT_API_SECRET";
/// Environment variable holding the optional WHMCS API access key.
pub const ENV_API_ACCESS_KEY: &str = "HOS_IMPORT_API_ACCESS_KEY";

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// `hostingos import …`.
// Parsed once per process; boxing the big variant buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum ImportCommand {
    /// Import from WHMCS (database or API connector). Runs a dry run first,
    /// prints the preview and asks before importing (`--yes` skips the
    /// question; `--dry-run` stops after the preview).
    ///
    /// Secrets: HOS_IMPORT_DB_PASSWORD, HOS_IMPORT_CC_HASH,
    /// HOS_IMPORT_API_SECRET, HOS_IMPORT_API_ACCESS_KEY (or a prompt).
    Whmcs(WhmcsArgs),
    /// List the import runs.
    List {
        /// Multi-company mode: the company's slug.
        #[arg(long, hide = true)]
        tenant: Option<String>,
    },
    /// Roll back an import: deletes every row it created (adopted rows stay).
    Rollback {
        /// Multi-company mode: the company's slug.
        #[arg(long, hide = true)]
        tenant: Option<String>,
        /// Run id.
        #[arg(long)]
        run: Uuid,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

/// TLS mode of the database connection.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliTls {
    /// Verify the server certificate (default).
    Verify,
    /// Encrypt without verifying the certificate.
    Insecure,
    /// Plain text (only for a private network).
    Disabled,
}

/// `hostingos import whmcs`.
#[derive(Debug, Args)]
pub struct WhmcsArgs {
    /// Multi-company mode: the slug of the company to import into.
    #[arg(long, hide = true)]
    pub tenant: Option<String>,
    /// WHMCS database host (database connector).
    #[arg(long, conflicts_with = "api_url")]
    pub db_host: Option<String>,
    /// WHMCS database port.
    #[arg(long, default_value_t = 3306)]
    pub db_port: u16,
    /// WHMCS database name.
    #[arg(long)]
    pub db_name: Option<String>,
    /// WHMCS database user (read-only grants are enough).
    #[arg(long)]
    pub db_user: Option<String>,
    /// TLS mode of the database connection.
    #[arg(long, value_enum, default_value_t = CliTls::Verify)]
    pub db_tls: CliTls,
    /// WHMCS URL for the API connector (`https://billing.example.net/`).
    #[arg(long)]
    pub api_url: Option<String>,
    /// WHMCS API identifier.
    #[arg(long)]
    pub api_identifier: Option<String>,
    /// Ticket attachments over the API while importing from the database
    /// (`--attach-url` + `--attach-identifier`, secret in HOS_IMPORT_API_SECRET).
    #[arg(long, requires = "attach_identifier")]
    pub attach_url: Option<String>,
    /// API identifier for `--attach-url`.
    #[arg(long)]
    pub attach_identifier: Option<String>,
    /// Comma-separated entity keys (default: the recommended set).
    #[arg(long, value_delimiter = ',')]
    pub entities: Vec<String>,
    /// Stop after the dry run (nothing is written).
    #[arg(long)]
    pub dry_run: bool,
    /// Import without asking after the preview.
    #[arg(long)]
    pub yes: bool,
    /// Re-import although the shop has been live since the last import
    /// (records were created here since then; `--yes` does not imply it).
    #[arg(long)]
    pub confirm_live: bool,
    /// Keep the billing automation running during the cut-over
    /// (default: paused until resumed in the admin panel).
    #[arg(long)]
    pub keep_automation: bool,
    /// Days imported services are protected from suspension / reminders.
    #[arg(long, default_value_t = 14)]
    pub grace_days: u32,
    /// Do not mark imported e-mail addresses verified.
    #[arg(long)]
    pub unverified_emails: bool,
    /// Do not carry over hosting account passwords.
    #[arg(long)]
    pub no_service_passwords: bool,
    /// Do not create integrations from WHMCS servers.
    #[arg(long)]
    pub no_server_credentials: bool,
    /// Newest activity-log lines to import.
    #[arg(long, default_value_t = 5000)]
    pub activity_log_limit: u32,
    /// Largest ticket attachment in MiB (1–25).
    #[arg(long, default_value_t = 10)]
    pub attachment_max_mb: u64,
    /// Locale of imported KB articles / announcements (`tr` or `en`).
    #[arg(long)]
    pub content_locale: Option<String>,
    /// Mapping `KIND:KEY=VALUE`, repeatable: `gateway:paypal=<integration id>`,
    /// `registrar:enom=<integration id>`, `server:3=create:cpanel|skip|<id>`,
    /// `product:7=create|<product id>`, `role:1=<role slug>`.
    #[arg(long = "map")]
    pub maps: Vec<String>,
    /// Write the report here (`.csv` or `.json`).
    #[arg(long)]
    pub report: Option<std::path::PathBuf>,
}

/// Runs `hostingos import …`.
pub async fn run(cmd: ImportCommand) -> anyhow::Result<()> {
    let state = AppState::from_env().await?;
    let result = match cmd {
        ImportCommand::Whmcs(args) => whmcs(&state, args).await,
        ImportCommand::List { tenant } => list(&state, &company(&state, tenant)?).await,
        ImportCommand::Rollback { tenant, run, yes } => {
            rollback(&state, &company(&state, tenant)?, run, yes).await
        }
    };
    state.pool.close().await;
    result
}

/// `--tenant`, or the one company of a single-company installation.
fn company(state: &AppState, given: Option<String>) -> anyhow::Result<String> {
    given
        .filter(|s| !s.trim().is_empty())
        .or_else(|| state.web.company.clone())
        .context("--tenant <slug> is required in multi-company mode")
}

async fn tenant_id(state: &AppState, slug: &str) -> anyhow::Result<TenantId> {
    Ok(platform::tenant_context_by_slug(&state.pool, slug)
        .await?
        .with_context(|| format!("company {slug:?} not found"))?
        .id)
}

/// A secret from `var`, else a no-echo prompt on a terminal; `None` when
/// neither is available or the value is empty.
fn secret(var: &str, prompt: &str, ask: bool) -> anyhow::Result<Option<String>> {
    if let Ok(v) = std::env::var(var) {
        let v = v.trim().to_owned();
        return Ok(Some(v).filter(|v| !v.is_empty()));
    }
    if !ask || !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let v =
        rpassword::prompt_password(format!("{prompt} ({var}): ")).context("reading the secret")?;
    Ok(Some(v.trim().to_owned()).filter(|v| !v.is_empty()))
}

fn source(args: &WhmcsArgs) -> anyhow::Result<(SourceConfig, SourceSecrets)> {
    if let Some(host) = &args.db_host {
        let database = args.db_name.clone().context("--db-name is required")?;
        let user = args.db_user.clone().context("--db-user is required")?;
        let attachments_api = args.attach_url.clone().map(|url| ApiSourceConfig {
            url,
            identifier: args.attach_identifier.clone().unwrap_or_default(),
        });
        let secrets = SourceSecrets {
            db_password: secret(ENV_DB_PASSWORD, "WHMCS database password", true)?,
            cc_encryption_hash: secret(
                ENV_CC_HASH,
                "WHMCS cc_encryption_hash (empty: skip encrypted fields)",
                true,
            )?,
            api_secret: if attachments_api.is_some() {
                secret(ENV_API_SECRET, "WHMCS API secret (attachments)", true)?
            } else {
                None
            },
            ..SourceSecrets::default()
        };
        let config = SourceConfig::WhmcsDb(DbSourceConfig {
            host: host.trim().to_owned(),
            port: args.db_port,
            database,
            user,
            tls: match args.db_tls {
                CliTls::Verify => TlsMode::Verify,
                CliTls::Insecure => TlsMode::Insecure,
                CliTls::Disabled => TlsMode::Disabled,
            },
            attachments_api,
        });
        return Ok((config, secrets));
    }
    let url = args
        .api_url
        .clone()
        .context("either --db-host or --api-url is required")?;
    let identifier = args
        .api_identifier
        .clone()
        .context("--api-identifier is required")?;
    let api_secret = secret(ENV_API_SECRET, "WHMCS API secret", true)?
        .with_context(|| format!("the API secret is required ({ENV_API_SECRET} or the prompt)"))?;
    Ok((
        SourceConfig::WhmcsApi(ApiSourceConfig { url, identifier }),
        SourceSecrets {
            api_secret: Some(api_secret),
            api_access_key: secret(
                ENV_API_ACCESS_KEY,
                "WHMCS API access key (empty: none)",
                true,
            )?,
            ..SourceSecrets::default()
        },
    ))
}

fn options(args: &WhmcsArgs) -> anyhow::Result<(ImportOptions, Mappings)> {
    let mut o = ImportOptions::default();
    for key in &args.entities {
        let e = Entity::parse(key.trim()).with_context(|| {
            let known: Vec<&str> = Entity::ORDER.iter().map(|e| e.key()).collect();
            format!("unknown entity {key:?}; known: {}", known.join(", "))
        })?;
        o.entities.insert(e);
    }
    o.pause_automation = !args.keep_automation;
    o.grace_days = args.grace_days.min(365);
    o.verified_emails = !args.unverified_emails;
    o.service_passwords = !args.no_service_passwords;
    o.server_credentials = !args.no_server_credentials;
    o.activity_log_limit = args.activity_log_limit.min(100_000);
    o.attachment_max_bytes = args.attachment_max_mb.clamp(1, 25) * 1024 * 1024;
    o.content_locale = match args.content_locale.as_deref() {
        None => None,
        Some(l @ ("tr" | "en")) => Some(l.to_owned()),
        Some(other) => bail!("--content-locale must be tr or en, not {other:?}"),
    };
    let mut m = Mappings::default();
    for spec in &args.maps {
        let (key, value) = spec
            .split_once('=')
            .with_context(|| format!("--map {spec:?}: expected KIND:KEY=VALUE"))?;
        if !super::apply_mapping(&mut m, key.trim(), value) {
            bail!("--map {spec:?}: KIND must be gateway, registrar, server, product or role");
        }
    }
    Ok((o, m))
}

async fn log(
    state: &AppState,
    tenant: TenantId,
    action: &str,
    run: Uuid,
    changes: serde_json::Value,
) -> anyhow::Result<()> {
    let entry = AuditEntry::new(Some(tenant), ActorKind::System, None, action)
        .request(None, Some("hostingos-cli"))
        .changes(changes)
        .entity("import_run", run);
    audit::write(&state.pool, &entry).await?;
    Ok(())
}

fn runner(state: &AppState) -> Runner {
    Runner::new(
        state.pool.clone(),
        state.secret_box.clone(),
        OutboundPolicy::global(),
        format!("cli-{}", Uuid::now_v7()),
    )
}

async fn whmcs(state: &AppState, args: WhmcsArgs) -> anyhow::Result<()> {
    let tenant = tenant_id(state, &company(state, args.tenant.clone())?).await?;
    let (config, secrets) = source(&args)?;
    let (options, mappings) = options(&args)?;
    if !super::validate_mappings(&state.pool, tenant, &mappings)
        .await
        .map_err(|e| anyhow!("{e}"))?
    {
        bail!("a --map value does not name an integration / product / role of this tenant");
    }
    // The CLI acts as the server operator; the owner role is still never a
    // target, and imported staff start deactivated either way.
    if super::role_mapping_error(&state.pool, tenant, None, &mappings)
        .await
        .map_err(|e| anyhow!("{e}"))?
        .is_some()
    {
        bail!(
            "--map role:…=owner is not allowed: imported staff never get the owner role (grant it in Staff after the import)"
        );
    }

    println!("connecting to {} …", config.label());
    let attempt = async {
        let mut source = build_source(&config, &secrets, OutboundPolicy::global())?;
        source.probe().await
    };
    let probe = match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => bail!("connection failed: {e}"),
        Err(_) => bail!("connection timed out after {}s", PROBE_TIMEOUT.as_secs()),
    };
    if let Some(v) = &probe.version {
        println!("  WHMCS {v}");
    }
    for n in &probe.notes {
        println!("  {n}");
    }

    let run = repo::create_run(
        &state.pool,
        tenant,
        &NewImportRun {
            id: Uuid::now_v7(),
            source_kind: config.kind().into(),
            source_key: config.key(),
            source_label: config.label(),
            source_config: serde_json::to_value(&config)?,
            options: serde_json::to_value(&options)?,
            started_by: None,
        },
    )
    .await?;
    let id = run.id;
    let sealed = hos_import::crypto::seal_secrets(&state.secret_box, tenant, id, &secrets)?;
    drop(secrets);
    repo::set_run_secrets(&state.pool, tenant, id, &sealed).await?;
    repo::update_draft(
        &state.pool,
        tenant,
        id,
        None,
        Some(&serde_json::to_value(&mappings)?),
        Some(&json!({ "probe": probe })),
    )
    .await?;
    log(
        state,
        tenant,
        "import.create",
        id,
        json!({ "source": run.source_label, "kind": run.source_kind }),
    )
    .await?;
    println!("run {id} created");

    let outcome = execute(state, tenant, id, "dry_run").await;
    let row = match outcome {
        Ok(row) => row,
        Err(e) => {
            repo::wipe_run_secrets(&state.pool, tenant, id).await?;
            return Err(e);
        }
    };
    print_preview(&row);
    if row.status != "draft" {
        repo::wipe_run_secrets(&state.pool, tenant, id).await?;
        bail!(
            "the dry run ended {}: {}",
            row.status,
            row.error.unwrap_or_default()
        );
    }
    if args.dry_run {
        repo::wipe_run_secrets(&state.pool, tenant, id).await?;
        println!("dry run only: nothing was written, the source credentials were wiped");
        return write_report(state, tenant, id, args.report.as_deref()).await;
    }
    if let Some(live) = repo::live_activity(&state.pool, tenant).await?
        && !args.confirm_live
    {
        repo::wipe_run_secrets(&state.pool, tenant, id).await?;
        bail!(
            "the shop has been live since the last import ({} invoices, payments, orders or credit entries created here after {}); re-run with --confirm-live to import anyway (credit used here is never added back)",
            live.records,
            live.since
        );
    }
    let proceed =
        args.yes || confirm("Import now? Existing rows are adopted, never overwritten. [y/N] ")?;
    if !proceed {
        repo::wipe_run_secrets(&state.pool, tenant, id).await?;
        println!("not imported; the source credentials were wiped");
        return Ok(());
    }
    log(
        state,
        tenant,
        "import.start",
        id,
        json!({ "source": row.source_label, "options": options, "mappings": mappings }),
    )
    .await?;
    let row = execute(state, tenant, id, "import").await?;
    print_progress(&Progress::from_json(&row.progress));
    for (k, v) in row.summary["post"].as_object().into_iter().flatten() {
        println!("  {k}: {v}");
    }
    write_report(state, tenant, id, args.report.as_deref()).await?;
    match row.status.as_str() {
        "completed" => {
            if options.pause_automation {
                println!(
                    "billing automation is paused for the cut-over: review the checklist under /admin/tools/import and resume it there"
                );
            }
            Ok(())
        }
        other => bail!(
            "the import ended {other}: {}",
            row.error.unwrap_or_default()
        ),
    }
}

/// Queues `phase` and runs it here, printing progress while it works.
async fn execute(
    state: &AppState,
    tenant: TenantId,
    id: Uuid,
    phase: &str,
) -> anyhow::Result<ImportRunRow> {
    println!(
        "{} …",
        if phase == "import" {
            "importing"
        } else {
            "dry run"
        }
    );
    let runner = runner(state);
    let pool = state.pool.clone();
    let ticker = tokio::spawn(async move {
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let Ok(Some(row)) = repo::get_run(&pool, tenant, id).await else {
                continue;
            };
            let p = Progress::from_json(&row.progress);
            let (done, total) = p.fraction();
            let line = format!("  {} {done}/{total}", p.current.unwrap_or_default());
            if line != last {
                println!("{line}");
                last = line;
            }
        }
    });
    // Queued and claimed in one transaction: a `serve` worker on the same
    // database never picks the run up first.
    let result = runner.queue_and_run(tenant, id, phase, None, None).await;
    ticker.abort();
    result.map_err(|e| anyhow!("{e}"))?.context(
        "the run cannot be queued (another import of this tenant is active, or the credentials are gone)",
    )
}

fn print_progress(p: &Progress) {
    println!(
        "{:<28} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
        "entity", "read", "created", "updated", "adopted", "skipped", "errors"
    );
    for e in Entity::ORDER {
        let Some(l) = p.entities.get(e.key()) else {
            continue;
        };
        let mut name = e.key().to_owned();
        if l.status == "off" || l.status == "unsupported" {
            name = format!("{name} ({})", l.status);
        }
        println!(
            "{name:<28} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
            l.read, l.created, l.updated, l.adopted, l.skipped, l.errors
        );
    }
}

fn print_preview(row: &ImportRunRow) {
    print_progress(&Progress::from_json(&row.progress));
    let Some(a) = row
        .preview
        .get("analysis")
        .and_then(|a| serde_json::from_value::<Analysis>(a.clone()).ok())
    else {
        return;
    };
    if a.conflicts.is_empty() {
        println!("no conflicts");
    } else {
        println!("conflicts:");
        for c in &a.conflicts {
            println!("  [{}] {}", c.code, c.message);
        }
    }
    for l in &a.limits {
        let max = l.max.map_or_else(|| "∞".to_owned(), |m| m.to_string());
        let fits = if l.fits() { "ok" } else { "OVER LIMIT" };
        println!(
            "  limit {}: {} + {} / {max} {fits}",
            l.key, l.used, l.incoming
        );
    }
    for u in row.preview["unsupported"].as_array().into_iter().flatten() {
        println!(
            "  skipped: {} — {}",
            u[0].as_str().unwrap_or(""),
            u[1].as_str().unwrap_or("")
        );
    }
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: pass --yes to import without the question, or --dry-run");
    }
    print!("{question}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "evet" | "e"))
}

async fn write_report(
    state: &AppState,
    tenant: TenantId,
    id: Uuid,
    path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let Some(path) = path else { return Ok(()) };
    let row = repo::get_run(&state.pool, tenant, id)
        .await?
        .context("run not found")?;
    let body = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        let report = hos_import::report::json_report(&state.pool, tenant, &row).await?;
        serde_json::to_vec_pretty(&report)?
    } else {
        hos_import::report::csv_report(&state.pool, tenant, &row)
            .await?
            .into_bytes()
    };
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    println!("report written to {}", path.display());
    Ok(())
}

async fn list(state: &AppState, slug: &str) -> anyhow::Result<()> {
    let tenant = tenant_id(state, slug).await?;
    let runs = repo::list_runs(&state.pool, tenant, 100).await?;
    if runs.is_empty() {
        println!("no import runs");
    }
    for r in runs {
        println!(
            "{}  {:<8} {:<16} {}  {}",
            r.id,
            r.phase,
            r.status,
            r.created_at.format("%Y-%m-%d %H:%M"),
            r.source_label
        );
    }
    Ok(())
}

async fn rollback(state: &AppState, slug: &str, run: Uuid, yes: bool) -> anyhow::Result<()> {
    let tenant = tenant_id(state, slug).await?;
    let row = repo::get_run(&state.pool, tenant, run)
        .await?
        .context("run not found")?;
    let check = hos_import::rollback::check(&state.pool, tenant, run).await?;
    if let Some(msg) = check.blocked_message() {
        bail!("rollback blocked: {msg}");
    }
    if !yes
        && !confirm(&format!(
            "Delete everything run {run} ({}) created? [y/N] ",
            row.source_label
        ))?
    {
        println!("nothing changed");
        return Ok(());
    }
    log(
        state,
        tenant,
        "import.rollback",
        run,
        json!({ "source": row.source_label }),
    )
    .await?;
    let row = runner(state)
        .rollback_now(tenant, run)
        .await
        .map_err(|e| anyhow!("{e}"))?
        .context("this run cannot be rolled back (not a finished import)")?;
    for (k, v) in row.summary["rolled_back"].as_object().into_iter().flatten() {
        println!("  {k}: {v}");
    }
    println!("rollback {}", row.status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source secrets have no command-line flag (argv shows in `ps` and the
    /// shell history): only the environment or the no-echo prompt.
    #[test]
    fn no_secret_is_taken_from_argv() {
        let cmd = WhmcsArgs::augment_args(clap::Command::new("whmcs"));
        for a in cmd
            .get_arguments()
            .filter(|a| a.get_action().takes_values())
        {
            let name = a.get_long().unwrap_or_default();
            for word in ["password", "secret", "access-key", "cc-hash", "token"] {
                assert!(!name.contains(word), "--{name} takes a secret");
            }
        }
        for flag in ["--db-password", "--api-secret", "--cc-encryption-hash"] {
            let parsed = cmd
                .clone()
                .try_get_matches_from(["whmcs", "--tenant", "t", flag, "x"]);
            assert!(parsed.is_err(), "{flag} accepted");
        }
    }
}
