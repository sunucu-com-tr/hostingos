//! `hos-migrate` — applies the embedded migrations and optionally seeds demo
//! data.
//!
//! ```text
//! cargo run -p hos-db --bin hos-migrate            # migrate only
//! cargo run -p hos-db --bin hos-migrate -- --seed  # migrate + idempotent seed
//! ```
//!
//! Reads `.env` (if present). Connects with `DATABASE_URL_DIRECT` (Neon direct
//! endpoint: migrations take a session-level advisory lock, which a
//! transaction-mode pooler cannot hold) and falls back to `DATABASE_URL`.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use hos_db::{DbConfig, connect, migrate, seed};

const USAGE: &str = "usage: hos-migrate [--seed]

  --seed     after migrating, insert demo data (idempotent)
  -h --help  show this help

environment: DATABASE_URL_DIRECT (preferred) or DATABASE_URL";

struct Args {
    seed: bool,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args { seed: false };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--seed" => args.seed = true,
            "-h" | "--help" => return Ok(None),
            other => bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
    }
    Ok(Some(args))
}

fn database_url() -> Result<(String, &'static str)> {
    for var in ["DATABASE_URL_DIRECT", "DATABASE_URL"] {
        if let Ok(url) = std::env::var(var)
            && !url.trim().is_empty()
        {
            return Ok((url, var));
        }
    }
    bail!("neither DATABASE_URL_DIRECT nor DATABASE_URL is set")
}

async fn run(args: Args) -> Result<()> {
    let (url, var) = database_url()?;
    let cfg = DbConfig::new(&url)
        .with_max_connections(2)
        .with_application_name("hos-migrate");
    // Never print the URL: it carries the password.
    tracing::info!(source = var, pooled = cfg.is_pooled(), "connecting");
    if cfg.is_pooled() {
        tracing::warn!(
            "{var} points at a connection pooler; prefer DATABASE_URL_DIRECT for migrations"
        );
    }
    let pool = connect(&cfg)
        .await
        .with_context(|| format!("connecting with {var}"))?;

    let before = hos_db::MIGRATOR.iter().count();
    migrate(&pool).await.context("applying migrations")?;
    let applied: i64 = sqlx::query_scalar("select count(*) from _sqlx_migrations where success")
        .fetch_one(&pool)
        .await
        .context("reading migration history")?;
    println!("migrations: {applied}/{before} applied");

    if args.seed {
        let report = seed::run(&pool).await.context("seeding")?;
        println!(
            "seed: {} new rows{}",
            report.total(),
            report
                .tenant_id
                .map(|t| format!(" (demo tenant {t})"))
                .unwrap_or_default()
        );
        for (table, n) in report.inserted.iter().filter(|(_, n)| *n > 0) {
            println!("  {table:<30} +{n}");
        }
    }
    pool.close().await;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
