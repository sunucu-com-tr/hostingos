//! Command line: `serve` (default; web + embedded job worker / scheduler),
//! `worker` (background runtime only), `setup` (first installation),
//! `migrate`, `create-staff`, `seed-demo`, `jobs run-once`,
//! `import whmcs|list|rollback`, `version [--json]`,
//! `update check|status|apply|rollback` (docs/UPDATES.md),
//! `demo-reset` (docs/DEMO.md), `healthcheck` (container health checks).
//! Multi-company mode only (hidden from `--help`): `tenant create|list`,
//! `create-platform-admin`, `disable-signup`.

use std::net::SocketAddr;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use hos_core::config::AppConfig;
use hos_core::domain::{Locale, UserKind};
use hos_db::repo::platform::TenantFilter;
use hos_db::repo::{identity, platform, saas};
use hos_db::{DbConfig, PgPool};
use tracing_subscriber::EnvFilter;

use crate::routes::platform::provision::{self, NewTenantRequest};
use crate::state::AppState;
use crate::worker::{BackgroundOptions, Shutdown};

/// `hostingos` — HostingOS web application.
#[derive(Debug, Parser)]
#[command(
    name = "hostingos",
    version,
    about = "HostingOS web application and admin CLI"
)]
pub struct Cli {
    /// What to do (default: `serve`).
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Sub-commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the HTTP server (default) with the embedded job worker, cron
    /// scheduler and maintenance loop.
    Serve {
        /// Do not run the background runtime in this process (use a separate
        /// `hostingos worker`).
        #[arg(long)]
        no_worker: bool,
        /// Apply pending database migrations before serving (also
        /// `HOS_MIGRATE_ON_START=1`; always on in demo mode). Replicas
        /// starting together take turns; a failed migration stops the start.
        #[arg(long)]
        migrate: bool,
    },
    /// Run only the background runtime: job worker, cron scheduler and
    /// maintenance (domain checks, idempotency-key cleanup).
    Worker,
    /// First installation: applies migrations and creates the company and
    /// its owner account (the password is printed when not given).
    Setup {
        /// Owner's e-mail (login).
        #[arg(long)]
        email: String,
        /// Owner's password; omitted → a random one is generated and printed.
        #[arg(long, env = "HOS_NEW_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Company name.
        #[arg(long, default_value = "My Hosting Company")]
        company: String,
        /// Default language (`tr` or `en`).
        #[arg(long, default_value = "tr")]
        locale: String,
        /// Default currency (ISO code).
        #[arg(long, default_value = "TRY")]
        currency: String,
    },
    /// Multi-company mode: company administration.
    #[command(subcommand, hide = true)]
    Tenant(TenantCommand),
    /// Job queue utilities.
    #[command(subcommand)]
    Jobs(JobsCommand),
    /// Stored gateway callbacks: `webhooks scrub`.
    #[command(subcommand)]
    Webhooks(WebhooksCommand),
    /// Multi-company mode: close self-service company sign-up (platform
    /// setting `signup.enabled`); the way out when the server refuses to
    /// start because sign-up is open under `HOS_OUTBOUND_POLICY=private`.
    #[command(hide = true)]
    DisableSignup,
    /// Data import (WHMCS): `import whmcs|list|rollback`.
    #[command(subcommand)]
    Import(crate::routes::admin::import::cli::ImportCommand),
    /// Apply database migrations.
    Migrate,
    /// Multi-company mode: create an operator-console account.
    #[command(hide = true)]
    CreatePlatformAdmin {
        /// E-mail (login).
        #[arg(long)]
        email: String,
        /// Password; omitted → a random one is generated and printed.
        #[arg(long, env = "HOS_NEW_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Display name.
        #[arg(long, default_value = "Platform Admin")]
        name: String,
    },
    /// Create a staff user.
    CreateStaff {
        /// Multi-company mode: the company's slug (single-company mode: the
        /// one company).
        #[arg(long, hide = true)]
        tenant: Option<String>,
        /// E-mail (login).
        #[arg(long)]
        email: String,
        /// Password; omitted → a random one is generated and printed.
        #[arg(long, env = "HOS_NEW_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Role slug (`owner`, `admin`, `billing`, `support`, `sales`).
        #[arg(long, default_value = "owner")]
        role: String,
        /// First name.
        #[arg(long, default_value = "Admin")]
        first_name: String,
        /// Last name.
        #[arg(long, default_value = "")]
        last_name: String,
    },
    /// Idempotent demo data: hos-db seed + demo accounts, mock integrations, blog posts.
    SeedDemo {
        /// Give the demo accounts new random passwords (they are printed once).
        #[arg(long)]
        reset_passwords: bool,
    },
    /// Print the version, commit, build date and target (needs no
    /// configuration; the updater's smoke test runs `version --json`).
    Version {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Self-update (docs/UPDATES.md).
    #[command(subcommand)]
    Update(UpdateCommand),
    /// Public demo (`HOS_DEMO_MODE=1`, docs/DEMO.md): delete everything
    /// visitors entered and restore the sample data now. A running server
    /// notices within a minute and signs visitors out; restarting it resets
    /// too.
    DemoReset,
    /// Probe the local server for a container health check: `GET /healthz`
    /// on `HOS_BIND` (`0.0.0.0` / `[::]` → loopback). Exit 0 on a 2xx
    /// answer, 1 otherwise. Reads no other configuration, needs no database,
    /// writes no files.
    Healthcheck {
        /// Address to probe (default: `HOS_BIND`, else 127.0.0.1:8080).
        #[arg(long)]
        addr: Option<SocketAddr>,
        /// Path to request.
        #[arg(long, default_value = "/healthz")]
        path: String,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 5)]
        timeout: u64,
    },
}

/// `hostingos update …`.
#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    /// Check the release source now and record what was found.
    Check,
    /// Version, install type, release source, keys, last check, active run.
    Status,
    /// Install a release in place (bare-binary installs only). The running
    /// service notices the new release and restarts into it.
    Apply {
        /// Version to install (as shown by `update check`).
        version: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Confirm a current database backup exists (needed when neither
        /// HOS_UPDATE_BACKUP_COMMAND nor pg_dump is available).
        #[arg(long)]
        backup_confirmed: bool,
    },
    /// Point `current` back to the previous release (keeps the database as
    /// it is; see docs/UPDATES.md for restoring a backup).
    Rollback {
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

/// `hostingos tenant …`.
// Parsed once per process; boxing the big variant buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum TenantCommand {
    /// Provision a tenant: `<slug>.<HOS_BASE_DOMAIN>` (+ optional custom
    /// domain), default roles / settings / templates / currencies /
    /// departments, a subscription and the owner account (invited by e-mail
    /// unless `HOS_NEW_PASSWORD` is set).
    Create {
        /// Short name (`acme` → `acme.<base domain>`).
        #[arg(long)]
        slug: String,
        /// Company name.
        #[arg(long)]
        name: String,
        /// Custom domain (added unverified unless `--verify-domain`).
        #[arg(long)]
        domain: Option<String>,
        /// Mark the custom domain verified right away (you manage DNS / TLS).
        #[arg(long)]
        verify_domain: bool,
        /// Owner e-mail.
        #[arg(long)]
        owner_email: String,
        /// Owner display name (`First Last`).
        #[arg(long, default_value = "")]
        owner_name: String,
        /// Owner password; omitted → an invitation link is e-mailed and printed.
        #[arg(long, env = "HOS_NEW_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Plan code (default: platform setting `signup.default_plan`).
        #[arg(long)]
        plan: Option<String>,
        /// Trial days (default: platform setting; 0 = active right away).
        #[arg(long)]
        trial_days: Option<i64>,
        /// `tr` or `en`.
        #[arg(long, default_value = "tr")]
        locale: String,
        /// Default currency.
        #[arg(long, default_value = "TRY")]
        currency: String,
    },
    /// List tenants.
    List {
        /// Filter by name / slug / domain.
        #[arg(long)]
        search: Option<String>,
    },
}

/// `hostingos webhooks …`.
#[derive(Debug, Subcommand)]
pub enum WebhooksCommand {
    /// Remove card and 3-D Secure fields (masked PAN, md, cavv, xid, eci …)
    /// from gateway callbacks stored before they were kept scrubbed.
    Scrub,
}

/// `hostingos jobs …`.
#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    /// Tick the scheduler once and run every due job, then exit.
    RunOnce {
        /// Stop after this many jobs.
        #[arg(long, default_value_t = 1000)]
        max_jobs: usize,
    },
}

/// Initializes `tracing` (`RUST_LOG`, default `info`; `HOS_LOG_FORMAT=json`).
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,hos_web=debug,sqlx=warn,tower_http=info,askama=warn")
    });
    let json = std::env::var("HOS_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);
    let result = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
    if let Err(e) = result {
        eprintln!("tracing already initialized: {e}");
    }
}

/// `HOS_JOB_QUEUE_NAMESPACE`: puts every job queue of this process into a
/// namespace (`<ns>.<queue>`), for a second installation (staging, a QA
/// run) that shares a database with another: neither claims the other's
/// jobs. Every process of an installation must use the same value.
fn job_queue_namespace() -> anyhow::Result<()> {
    let Ok(ns) = std::env::var("HOS_JOB_QUEUE_NAMESPACE") else {
        return Ok(());
    };
    let ns = ns.trim();
    if ns.is_empty() {
        return Ok(());
    }
    anyhow::ensure!(
        ns.len() <= 32
            && ns
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'),
        "HOS_JOB_QUEUE_NAMESPACE must be 1–32 characters of a–z, 0–9, `_` or `-`"
    );
    hos_db::repo::jobs::isolate_queue(ns);
    tracing::warn!(
        namespace = ns,
        "job queues are namespaced (HOS_JOB_QUEUE_NAMESPACE)"
    );
    Ok(())
}

/// Runs the parsed command.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    if let Some(Command::Healthcheck {
        addr,
        path,
        timeout,
    }) = &cli.command
    {
        let addr = match addr {
            Some(a) => *a,
            None => probe_addr(
                std::env::var(hos_core::config::env::HOS_BIND)
                    .ok()
                    .as_deref(),
            )?,
        };
        let status = healthcheck(addr, path, std::time::Duration::from_secs(*timeout)).await?;
        println!("ok: HTTP {status} from http://{addr}{path}");
        return Ok(());
    }
    job_queue_namespace()?;
    match cli.command.unwrap_or(Command::Serve {
        no_worker: false,
        migrate: false,
    }) {
        Command::Serve { no_worker, migrate } => serve(!no_worker, migrate).await,
        Command::Worker => worker().await,
        Command::Tenant(cmd) => tenant(cmd).await,
        Command::Import(cmd) => crate::routes::admin::import::cli::run(cmd).await,
        Command::Webhooks(WebhooksCommand::Scrub) => {
            let state = AppState::from_env().await?;
            let changed = crate::routes::webhooks::scrub_stored_payloads(&state.pool, None).await?;
            println!("webhooks: {changed} stored payload(s) scrubbed");
            state.pool.close().await;
            Ok(())
        }
        Command::DisableSignup => {
            let state = AppState::from_env().await?;
            hos_db::repo::saas::set_platform_setting(
                &state.pool,
                crate::routes::platform::PlatformSettings::SIGNUP_ENABLED,
                &serde_json::json!(false),
            )
            .await?;
            println!("self-service sign-up disabled");
            state.pool.close().await;
            Ok(())
        }
        Command::Jobs(JobsCommand::RunOnce { max_jobs }) => {
            let state = AppState::from_env().await?;
            let stats = crate::worker::run_once(&state, max_jobs).await?;
            println!(
                "jobs: {} completed, {} retried, {} dead",
                stats.completed, stats.retried, stats.dead
            );
            state.pool.close().await;
            Ok(())
        }
        Command::Migrate => {
            let config = AppConfig::from_env().context("loading configuration")?;
            let pool = migration_pool(&config).await?;
            hos_db::migrate(&pool).await.context("running migrations")?;
            println!("migrations applied");
            Ok(())
        }
        Command::CreatePlatformAdmin {
            email,
            password,
            name,
        } => {
            let config = AppConfig::from_env().context("loading configuration")?;
            let pool = migration_pool(&config).await?;
            let (password, generated) = password_or_random(password);
            let created = create_platform_admin(&pool, &email, &name, &password).await?;
            if !created {
                bail!("a platform admin with e-mail {email} already exists");
            }
            println!("platform admin created: {email}");
            if generated {
                println!("password: {password}");
            }
            Ok(())
        }
        Command::CreateStaff {
            tenant,
            email,
            password,
            role,
            first_name,
            last_name,
        } => {
            let config = AppConfig::from_env().context("loading configuration")?;
            let pool = migration_pool(&config).await?;
            let tenant = match tenant {
                Some(slug) => slug,
                None => {
                    let slugs: Vec<String> = sqlx::query_scalar(
                        "select slug from tenants order by created_at, slug limit 3",
                    )
                    .fetch_all(&pool)
                    .await?;
                    crate::mode::pick_company(&slugs).map_err(anyhow::Error::msg)?
                }
            };
            let tenant_ctx = platform::tenant_context_by_slug(&pool, &tenant)
                .await?
                .with_context(|| format!("company {tenant:?} not found"))?;
            let (password, generated) = password_or_random(password);
            let created = create_staff(
                &pool,
                tenant_ctx.id,
                &StaffSpec {
                    email: &email,
                    password: &password,
                    role: &role,
                    first_name: &first_name,
                    last_name: &last_name,
                },
            )
            .await?;
            if !created {
                bail!("user {email} already exists");
            }
            println!("staff user created: {email} ({role})");
            if generated {
                println!("password: {password}");
            }
            Ok(())
        }
        Command::SeedDemo { reset_passwords } => {
            let config = AppConfig::from_env().context("loading configuration")?;
            let pool = migration_pool(&config).await?;
            crate::seed_demo::run(&pool, &config, reset_passwords).await
        }
        Command::Version { json } => {
            version(json);
            Ok(())
        }
        Command::Update(cmd) => update(cmd).await,
        Command::Setup {
            email,
            password,
            company,
            locale,
            currency,
        } => {
            crate::mode::setup(crate::mode::SetupRequest {
                email,
                password,
                company,
                locale: locale
                    .parse()
                    .map_err(|_| anyhow::anyhow!("unknown locale {locale:?}"))?,
                currency: currency
                    .to_ascii_uppercase()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("unknown currency {currency:?}"))?,
            })
            .await
        }
        Command::DemoReset => demo_reset().await,
        Command::Healthcheck { .. } => unreachable!("handled above"),
    }
}

/// `hostingos demo-reset`: the same reset the server runs on its schedule,
/// with the same refusals (demo mode off, a database with other data).
async fn demo_reset() -> anyhow::Result<()> {
    let state = AppState::from_env().await?;
    let cfg = state.web.demo.clone().with_context(|| {
        format!(
            "{} is not set: there is no demo to reset",
            crate::demo::MODE_ENV
        )
    })?;
    crate::demo::check_database(&state.pool, &cfg).await?;
    let report = crate::demo::reset(&state).await?;
    println!(
        "demo company `{}` reset in {} ms",
        cfg.tenant_slug,
        report.took.as_millis()
    );
    state.pool.close().await;
    Ok(())
}

/// The address `healthcheck` connects to for a `HOS_BIND` value: the
/// unspecified address (listen on all interfaces) becomes loopback.
fn probe_addr(bind: Option<&str>) -> anyhow::Result<SocketAddr> {
    let raw = bind
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(hos_core::config::DEFAULT_BIND);
    let mut addr: SocketAddr = raw
        .parse()
        .with_context(|| format!("HOS_BIND={raw:?} is not an address:port"))?;
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr {
            SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    Ok(addr)
}

/// `GET path` over plain HTTP/1.1; the status code of a 2xx answer.
async fn healthcheck(
    addr: SocketAddr,
    path: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<u16> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    anyhow::ensure!(
        path.starts_with('/') && path.bytes().all(|b| b.is_ascii_graphic()),
        "--path must be an absolute path without spaces"
    );
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to {addr}"))?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: hostingos-healthcheck\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        let mut head = Vec::with_capacity(256);
        let mut buf = [0u8; 256];
        while !head.windows(2).any(|w| w == b"\r\n") && head.len() < 4096 {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        anyhow::Ok(head)
    };
    let head = tokio::time::timeout(timeout, probe)
        .await
        .with_context(|| format!("no answer from {addr} within {} s", timeout.as_secs()))??;
    let line = String::from_utf8_lossy(&head);
    let status = line
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .with_context(|| format!("not an HTTP answer from {addr}"))?;
    anyhow::ensure!(
        (200..300).contains(&status),
        "unhealthy: HTTP {status} from http://{addr}{path}"
    );
    Ok(status)
}

fn version(json: bool) {
    let b = hos_update::BuildInfo::current();
    if json {
        let out = serde_json::json!({
            "product": hos_update::PRODUCT,
            "version": b.version,
            "commit": b.commit,
            "build_date": b.build_date.map(|d| d.to_rfc3339()),
            "target": b.target,
            "profile": b.profile,
        });
        println!("{out}");
    } else {
        println!("{}", b.describe());
    }
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "evet" | "e"))
}

async fn update(cmd: UpdateCommand) -> anyhow::Result<()> {
    use crate::routes::platform::updates::runtime::{self as upd, ApplyMode, Requester};
    use hos_db::repo::updates as repo;
    use hos_update::install::InstallKind;

    let state = AppState::from_env().await?;
    let rt = &state.updates;
    let result = match cmd {
        UpdateCommand::Check => match upd::check_now(&state).await {
            Ok(s) => {
                match &s.newest {
                    Some(v) => println!("newest installable version: {v}"),
                    None => println!("no newer version (running {})", rt.build.version),
                }
                if s.problems > 0 {
                    println!(
                        "{} release(s) were rejected; see `hostingos update status`",
                        s.problems
                    );
                }
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("{}: {e}", e.kind())),
        },
        UpdateCommand::Status => {
            println!("version:   {}", rt.build.describe());
            let install = rt.install();
            println!("install:   {}", install.as_str());
            if let InstallKind::Binary(b) = &install {
                println!(
                    "root:      {} (supervised: {})",
                    b.root.display(),
                    b.supervised
                );
            }
            match rt.source() {
                Ok(Some(src)) => println!("source:    {}", src.describe()),
                Ok(None) => println!(
                    "source:    not configured (set HOS_UPDATE_REPO or HOS_UPDATE_MANIFEST_URL)"
                ),
                Err(e) => println!("source:    invalid: {e}"),
            }
            let keys = rt.builtin_keys().map(|r| r.len()).unwrap_or(0);
            println!(
                "keys:      {keys} built in{}",
                if keys == 0 {
                    " — installs disabled"
                } else {
                    ""
                }
            );
            let mut conn = state.pool.acquire().await?;
            let st = repo::state(&mut conn).await?;
            drop(conn);
            println!(
                "channel:   {} (automatic checks: {})",
                st.channel, st.auto_check
            );
            println!(
                "checked:   {}{}",
                st.last_check_at
                    .map_or_else(|| "never".to_owned(), |d| d.to_rfc3339()),
                st.last_check_error
                    .map(|e| format!(" — failed: {e}"))
                    .unwrap_or_default()
            );
            println!("maintenance: {}", st.maintenance);
            for r in repo::list_releases(&state.pool).await? {
                println!(
                    "release:   {} [{}{}{}]",
                    r.version,
                    r.trust,
                    if r.severity == "security" {
                        ", security"
                    } else {
                        ""
                    },
                    r.blocked
                        .map(|b| format!(", blocked: {b}"))
                        .unwrap_or_default()
                );
            }
            if let Some(run) = repo::active_run(&state.pool).await? {
                println!(
                    "active:    {} → {} ({}, {})",
                    run.from_version, run.to_version, run.status, run.id
                );
            }
            Ok(())
        }
        UpdateCommand::Apply {
            version,
            yes,
            backup_confirmed,
        } => match upd::prepare_apply(&state, &version, backup_confirmed, ApplyMode::Cli).await {
            Err(e) => Err(anyhow::anyhow!("{}: {e}", e.kind())),
            Ok(prepared) => {
                if !yes
                    && !confirm(&format!(
                        "Install {version} over {}? The site is in maintenance mode meanwhile.",
                        rt.build.version
                    ))?
                {
                    println!("cancelled");
                    state.pool.close().await;
                    return Ok(());
                }
                let who = Requester {
                    id: None,
                    email: "cli".to_owned(),
                };
                let run = upd::start_apply(&state, prepared, &who, ApplyMode::Cli)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}: {e}", e.kind()))?;
                match repo::get_run(&state.pool, run).await? {
                    Some(r) if r.status == "restarting" => {
                        println!(
                            "{version} installed. The running service restarts into it and confirms its health \
                                 (or rolls back); follow /platform/updates/runs/{run}. If the service is stopped, start it."
                        );
                        Ok(())
                    }
                    Some(r) => Err(anyhow::anyhow!(
                        "update {}: {}",
                        r.status,
                        r.message.unwrap_or_default()
                    )),
                    None => Err(anyhow::anyhow!("update run {run} disappeared")),
                }
            }
        },
        UpdateCommand::Rollback { yes } => match rt.install() {
            InstallKind::Binary(b) => {
                let layout = b.layout();
                let previous = layout
                    .previous()
                    .context("there is no previous release to go back to")?;
                if !yes
                    && !confirm(&format!(
                        "Switch back to {}? The database is not changed; if the newer release migrated it \
                         in a way that is not rollback-safe, restore the backup taken before the update.",
                        previous.display()
                    ))?
                {
                    println!("cancelled");
                    state.pool.close().await;
                    return Ok(());
                }
                let name = |p: &std::path::Path| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                };
                // Take the one active-run slot first: never switch `current`
                // under an update that is installing or confirming its health.
                let current = layout
                    .current()
                    .map(|p| name(&p))
                    .unwrap_or_else(|_| rt.build.version.clone());
                let run = match repo::create_run(
                    &state.pool,
                    &repo::NewRun {
                        kind: "rollback",
                        from_version: &current,
                        to_version: &name(&previous),
                        status: "running",
                        requested_by: None,
                        requested_by_email: Some("cli"),
                        node: &rt.node,
                    },
                )
                .await
                {
                    Ok(run) => run,
                    Err(hos_core::CoreError::Conflict(_)) => {
                        state.pool.close().await;
                        anyhow::bail!(
                            "an update is in progress (see `hostingos update status`); roll back once it has finished"
                        );
                    }
                    Err(e) => return Err(e.into()),
                };
                let (_, to) = match hos_update::apply::rollback_to_previous(&layout) {
                    Ok(pair) => pair,
                    Err(e) => {
                        let msg = format!("{}: {e}", e.kind());
                        let _ =
                            repo::set_run_status(&state.pool, run.id, "failed", Some(&msg)).await;
                        state.pool.close().await;
                        anyhow::bail!(msg);
                    }
                };
                let _ = repo::set_run_status(
                    &state.pool,
                    run.id,
                    "succeeded",
                    Some("rolled back from the command line"),
                )
                .await;
                println!(
                    "current now points to {}. The running service restarts into it; if it is stopped, start it.",
                    to.display()
                );
                Ok(())
            }
            other => Err(anyhow::anyhow!(
                "rollback works on bare-binary installs only (this is: {}); for containers deploy the previous image tag",
                other.as_str()
            )),
        },
    };
    state.pool.close().await;
    result
}

async fn migration_pool(config: &AppConfig) -> anyhow::Result<PgPool> {
    hos_db::connect(&DbConfig::for_migrations(config))
        .await
        .context("connecting to the database")
}

/// A random 20-character password (letters + digits, unambiguous).
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let bytes = hos_core::crypto::random_bytes::<20>();
    bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect()
}

fn password_or_random(password: Option<String>) -> (String, bool) {
    match password.filter(|p| !p.is_empty()) {
        Some(p) => (p, false),
        None => (random_password(), true),
    }
}

async fn hash(password: &str) -> anyhow::Result<String> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hos_auth::password::hash_password(&password))
        .await
        .context("hashing task")?
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

/// Creates a platform admin; `false` if the e-mail exists.
pub async fn create_platform_admin(
    pool: &PgPool,
    email: &str,
    name: &str,
    password: &str,
) -> anyhow::Result<bool> {
    if platform::platform_admin_by_email(pool, email)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    let hash = hash(password).await?;
    platform::create_platform_admin(pool, email.trim(), name, Some(&hash)).await?;
    Ok(true)
}

/// Input of [`create_staff`].
#[derive(Debug, Clone, Copy)]
pub struct StaffSpec<'a> {
    /// E-mail.
    pub email: &'a str,
    /// Clear-text password (hashed here).
    pub password: &'a str,
    /// Role slug.
    pub role: &'a str,
    /// First name.
    pub first_name: &'a str,
    /// Last name.
    pub last_name: &'a str,
}

/// Creates a verified staff user with `role`; `false` if the e-mail exists.
pub async fn create_staff(
    pool: &PgPool,
    tenant_id: hos_core::ids::TenantId,
    spec: &StaffSpec<'_>,
) -> anyhow::Result<bool> {
    let email = crate::forms::normalize_email(spec.email);
    if identity::user_by_email(pool, tenant_id, &email)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    let role = identity::role_by_slug(pool, tenant_id, spec.role)
        .await?
        .with_context(|| format!("role {:?} not found", spec.role))?;
    let hash = hash(spec.password).await?;
    let mut tx = pool.begin().await?;
    let user = identity::create_user(
        &mut *tx,
        tenant_id,
        &identity::NewUser {
            email,
            password_hash: Some(hash),
            kind: UserKind::Staff,
            first_name: spec.first_name.to_owned(),
            last_name: spec.last_name.to_owned(),
            phone: None,
            locale: Locale::Tr,
            email_verified: true,
        },
    )
    .await?;
    identity::assign_role(&mut *tx, tenant_id, user.id, role.id).await?;
    tx.commit().await?;
    Ok(true)
}

/// How long background tasks get to finish after the shutdown signal.
const BACKGROUND_GRACE: std::time::Duration = std::time::Duration::from_secs(45);

/// Startup log of the security-relevant process settings: the outbound
/// policy and — loudly — enabled mock integrations.
fn log_security_settings(state: &AppState) {
    tracing::info!(
        outbound_policy = %hos_core::net::OutboundPolicy::global(),
        "outbound policy for admin-configured endpoints (HOS_OUTBOUND_POLICY)"
    );
    match state.web.mock_integrations {
        crate::config::MockIntegrations::Refused => {}
        crate::config::MockIntegrations::AllowedInDebug => tracing::warn!(
            "DEBUG MODE (HOS_DEBUG): the `mock` payment gateway, hypervisor, panel and registrar \
             are enabled — a mock gateway lets anyone pay any invoice for free. Never run debug \
             mode in production (HOS_DEBUG=0)."
        ),
        crate::config::MockIntegrations::AllowedByDemo => tracing::warn!(
            "DEMO MODE (HOS_DEMO_MODE): only the `mock` payment gateway, hypervisor, panel and \
             registrar are available; outbound connections are disabled."
        ),
        crate::config::MockIntegrations::AllowedByEnv => tracing::warn!(
            "HOS_ALLOW_MOCK_INTEGRATIONS is set: the `mock` payment gateway, hypervisor, panel and \
             registrar are enabled — a mock gateway lets anyone pay any invoice for free. Demo \
             and test deployments only."
        ),
    }
}

/// Startup log of the reverse-proxy settings: which peers may set the
/// client address (`HOS_TRUSTED_PROXIES`), and a warning when every peer
/// may (`HOS_TRUST_PROXY` without a list).
fn log_proxy_settings(web: &crate::config::WebConfig) {
    let list = crate::config::TrustedProxies::ENV;
    match (web.trust_proxy, web.trusted_proxies.is_empty()) {
        (true, true) => tracing::warn!(
            "HOS_TRUST_PROXY is on and {list} is empty: the client address and scheme are \
             taken from the forwarding headers (X-Forwarded-For, X-Forwarded-Proto{}) of \
             every connection, so anyone who reaches this port directly can choose their \
             address. Set {list} to the address range of your reverse proxy \
             (docs/DEPLOYMENT.md)",
            web.client_ip_header
                .as_ref()
                .map(|h| format!(", {h}"))
                .unwrap_or_default()
        ),
        (true, false) => tracing::info!(
            trusted_proxies = ?web.trusted_proxies.nets(),
            "forwarding headers are believed only from these peers ({list})"
        ),
        (false, false) => tracing::warn!(
            "{list} is set but HOS_TRUST_PROXY is off: forwarding headers are ignored"
        ),
        (false, true) => {}
    }
}

async fn serve(background: bool, migrate: bool) -> anyhow::Result<()> {
    use crate::routes::platform::updates::runtime::{self as upd, Early};
    // Before anything that can fail: the watchdog of an in-flight update.
    let early = match upd::early_startup() {
        Early::Continue(startup) => startup,
        Early::Exit(message) => {
            tracing::warn!(%message, "update rolled back; restarting into the previous release");
            hos_update::restart::request(message);
            return Ok(());
        }
    };
    let state = AppState::from_env().await?;
    log_security_settings(&state);
    log_proxy_settings(&state.web);
    // Demo mode: refuses debug mode and foreign data before anything is
    // changed (docs/DEMO.md).
    crate::demo::preflight(&state).await?;
    // Migrations before the listener opens (`--migrate`,
    // HOS_MIGRATE_ON_START; demo mode always: its database is its own).
    if migrate || crate::schema::migrate_on_start_from_env() || state.web.demo.is_some() {
        crate::schema::migrate_on_start(&state.config).await?;
    }
    // A schema behind this release: pages answer 503 until it is migrated.
    if let Err(e) = state.schema.check(&state.pool).await {
        tracing::warn!(error = %e, "checking the database schema at startup failed");
    }
    // Demo mode: resets the demo once before the listener opens.
    crate::demo::start(&state).await?;
    crate::routes::platform::check_signup_policy(&state).await?;
    let addr: SocketAddr = state.config.bind;
    let app = crate::routes::app(state.clone());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let (shutdown, signal) = Shutdown::new();
    let handles = if background {
        crate::worker::spawn(&state, BackgroundOptions::default(), &signal)
    } else {
        // The update loop also watches for a release switched by another
        // process; the worker runtime starts it otherwise.
        vec![upd::spawn_background(&state, &signal)]
    };
    let mut handles = handles;
    handles.extend(crate::demo::spawn(&state, &signal));
    let startup = upd::late_startup(&state, early, Some(addr));
    tracing::info!(%addr, base_url = %state.config.base_url, debug = state.web.debug, background, "hostingos listening");
    let server = crate::server::serve(
        listener,
        app,
        state.web.http_timeouts,
        signal.clone().wait(),
    );
    let mut server = std::pin::pin!(server);
    let result = tokio::select! {
        r = &mut server => r,
        () = shutdown_signal() => {
            shutdown.trigger();
            server.await
        }
        () = hos_update::restart::wait() => {
            tracing::warn!(reason = ?hos_update::restart::requested(), "restarting for an update");
            shutdown.trigger();
            server.await
        }
    };
    // Also stops the background runtime when the server ended on its own.
    shutdown.trigger();
    startup.abort();
    crate::worker::join(handles, BACKGROUND_GRACE).await;
    result.context("server error")?;
    state.pool.close().await;
    tracing::info!("shut down cleanly");
    Ok(())
}

async fn worker() -> anyhow::Result<()> {
    let state = AppState::from_env().await?;
    log_security_settings(&state);
    let (shutdown, signal) = Shutdown::new();
    let handles = crate::worker::spawn(&state, BackgroundOptions::default(), &signal);
    tokio::select! {
        () = shutdown_signal() => {}
        () = hos_update::restart::wait() => {
            tracing::warn!(reason = ?hos_update::restart::requested(), "restarting for an update");
        }
    }
    shutdown.trigger();
    crate::worker::join(handles, BACKGROUND_GRACE).await;
    state.pool.close().await;
    tracing::info!("worker shut down cleanly");
    Ok(())
}

async fn tenant(cmd: TenantCommand) -> anyhow::Result<()> {
    let state = AppState::from_env().await?;
    let result = match cmd {
        TenantCommand::Create {
            slug,
            name,
            domain,
            verify_domain,
            owner_email,
            owner_name,
            password,
            plan,
            trial_days,
            locale,
            currency,
        } => {
            let mut req = NewTenantRequest::new(&slug, &name, &owner_email);
            let (first, last) = owner_name
                .trim()
                .split_once(' ')
                .map_or((owner_name.trim(), ""), |(f, l)| (f, l.trim()));
            req.owner_first_name = first.to_owned();
            req.owner_last_name = last.to_owned();
            req.custom_domain = domain
                .map(|d| d.trim().to_lowercase())
                .filter(|d| !d.is_empty());
            req.verify_custom_domain = verify_domain;
            req.owner_password = password.filter(|p| !p.is_empty());
            req.plan_code = plan;
            req.trial_days = trial_days;
            req.locale = locale
                .parse()
                .map_err(|_| anyhow::anyhow!("unknown locale {locale:?}"))?;
            req.currency = currency
                .to_ascii_uppercase()
                .parse()
                .map_err(|_| anyhow::anyhow!("unknown currency {currency:?}"))?;
            if let Some(p) = &req.owner_password {
                hos_auth::password::PasswordPolicy::default()
                    .validate(p, &[&req.owner_email, &req.slug, &req.name])
                    .map_err(|v| {
                        let reasons: Vec<String> =
                            v.iter().map(|x| x.message(Locale::En)).collect();
                        anyhow::anyhow!("owner password rejected: {}", reasons.join("; "))
                    })?;
            }
            match provision::provision(&state, &req).await {
                Ok(done) => {
                    let t = &done.provisioned.tenant;
                    println!("tenant created: {} ({})", t.slug, t.id);
                    println!("storefront: {}", done.tenant_url);
                    println!("admin:      {}/admin", done.tenant_url);
                    if let Some(c) = &done.provisioned.custom_domain {
                        let state_txt = if c.verified_at.is_some() {
                            "verified"
                        } else {
                            "pending DNS verification"
                        };
                        println!("custom domain: {} ({state_txt})", c.host);
                    }
                    match &done.invite_url {
                        Some(link) => println!(
                            "owner {} invited; set-password link: {link}",
                            req.owner_email
                        ),
                        None => println!(
                            "owner {} can sign in with the given password",
                            req.owner_email
                        ),
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
        TenantCommand::List { search } => {
            let filter = TenantFilter {
                search: search.filter(|s| !s.trim().is_empty()),
                ..TenantFilter::default()
            };
            let page = saas::list_tenant_summaries(
                &state.pool,
                &filter,
                hos_db::Page {
                    limit: 200,
                    offset: 0,
                },
            )
            .await?;
            println!(
                "{:<20} {:<10} {:<14} {:>8} {:>9}  host",
                "slug", "status", "plan", "clients", "services"
            );
            for r in &page.items {
                println!(
                    "{:<20} {:<10} {:<14} {:>8} {:>9}  {}",
                    r.tenant.slug,
                    r.tenant.status.as_str(),
                    r.plan_name.as_deref().unwrap_or("-"),
                    r.clients,
                    r.active_services,
                    r.primary_host.as_deref().unwrap_or("-"),
                );
            }
            println!("{} tenant(s)", page.total);
            Ok(())
        }
    };
    state.pool.close().await;
    result
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "ctrl-c handler");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received, draining connections");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_address_follows_hos_bind() {
        assert_eq!(probe_addr(None).unwrap().to_string(), "127.0.0.1:8080");
        assert_eq!(
            probe_addr(Some("0.0.0.0:9000")).unwrap().to_string(),
            "127.0.0.1:9000"
        );
        assert_eq!(
            probe_addr(Some("[::]:8080")).unwrap().to_string(),
            "[::1]:8080"
        );
        assert_eq!(
            probe_addr(Some("10.0.0.5:80")).unwrap().to_string(),
            "10.0.0.5:80"
        );
        assert!(probe_addr(Some("localhost:8080")).is_err());
    }

    /// A one-shot server answering `status_line`; returns its address.
    async fn answer(status_line: &'static str) -> SocketAddr {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            assert!(buf[..n].starts_with(b"GET /healthz HTTP/1.1\r\n"));
            let body = format!("{status_line}\r\nContent-Length: 2\r\n\r\nok");
            s.write_all(body.as_bytes()).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn healthcheck_needs_a_2xx_answer() {
        let t = std::time::Duration::from_secs(5);
        let ok = answer("HTTP/1.1 200 OK").await;
        assert_eq!(healthcheck(ok, "/healthz", t).await.unwrap(), 200);
        let down = answer("HTTP/1.1 503 Service Unavailable").await;
        assert!(healthcheck(down, "/healthz", t).await.is_err());
        // Nothing listening.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        assert!(healthcheck(closed, "/healthz", t).await.is_err());
        assert!(healthcheck(closed, "/x y", t).await.is_err());
    }
}
