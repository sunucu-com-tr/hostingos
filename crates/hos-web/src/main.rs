//! `hostingos` binary: the HostingOS web application and its admin CLI.
//!
//! ```text
//! hostingos setup --email owner@example.com   # first installation: schema, company, owner
//! hostingos                      # = hostingos serve (web + job worker + scheduler)
//! hostingos serve --migrate      # apply pending migrations first (HOS_MIGRATE_ON_START=1)
//! hostingos serve --no-worker    # web only; run `hostingos worker` separately
//! hostingos worker               # job worker + cron scheduler + maintenance
//! hostingos migrate
//! hostingos create-staff --email admin@example.com [--password …] --role owner
//! hostingos seed-demo
//! hostingos jobs run-once [--max-jobs 1000]
//! hostingos version [--json]
//! hostingos healthcheck [--path /readyz]                   # container health check
//! hostingos demo-reset                                     # public demo (docs/DEMO.md)
//! hostingos update check|status|apply <version>|rollback   # docs/UPDATES.md
//! ```
//!
//! Exit code 75 means "restart me": an update was installed or rolled back
//! and the supervisor must start the process again (docs/deploy).

use clap::Parser as _;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    hos_web::config::load_dotenv();
    hos_web::cli::init_tracing();
    let cli = hos_web::cli::Cli::parse();
    match hos_web::cli::run(cli).await {
        // An installed (or rolled back) update: the supervisor restarts the
        // process into the active release (systemd: RestartForceExitStatus=75).
        Ok(()) if hos_update::restart::requested().is_some() => {
            std::process::ExitCode::from(hos_update::restart::EXIT_CODE)
        }
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "hostingos failed");
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
