//! Database backup before an update.
//!
//! 1. `HOS_UPDATE_BACKUP_COMMAND` — run with `sh -c`; it gets
//!    `HOS_BACKUP_LABEL` (e.g. `0.1.0-to-0.2.0-20261201T100000Z`) and the
//!    process environment (`DATABASE_URL_DIRECT`, …). Exit status 0 = done.
//! 2. otherwise `pg_dump` when it is on `PATH`: a custom-format dump to
//!    `<root>/state/backups/hostingos-<label>.dump` (mode 0600). The
//!    connection settings go through `PG*` environment variables, never
//!    the command line (which other users can read).
//! 3. otherwise there is no automatic backup and the operator has to
//!    confirm "I have a backup" before an update starts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::error::{UpdateError, UpdateResult};

/// How long a backup may take.
pub const BACKUP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How the database is backed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupPlan {
    /// `HOS_UPDATE_BACKUP_COMMAND`.
    Command(String),
    /// `pg_dump` into `dir`.
    PgDump {
        /// The `pg_dump` executable.
        binary: PathBuf,
        /// Output directory.
        dir: PathBuf,
    },
    /// No automatic backup: the operator confirms one exists.
    Manual,
}

impl BackupPlan {
    /// `command` / `pg_dump` / `manual`.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Command(_) => "command",
            Self::PgDump { .. } => "pg_dump",
            Self::Manual => "manual",
        }
    }

    /// From the environment, dumps going to `backup_dir`.
    pub fn from_env(backup_dir: &Path) -> Self {
        if let Ok(cmd) = std::env::var("HOS_UPDATE_BACKUP_COMMAND")
            && !cmd.trim().is_empty()
        {
            return Self::Command(cmd.trim().to_owned());
        }
        match find_in_path("pg_dump") {
            Some(binary) => Self::PgDump {
                binary,
                dir: backup_dir.to_path_buf(),
            },
            None => Self::Manual,
        }
    }
}

/// Looks `name` up on `PATH`.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `PG*` variables for a `postgres://` URL (`channel_binding` is dropped).
pub fn pg_env(database_url: &str) -> UpdateResult<Vec<(String, String)>> {
    let url = url::Url::parse(database_url)
        .map_err(|_| UpdateError::Backup("the database URL does not parse".into()))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return Err(UpdateError::Backup("not a postgres:// URL".into()));
    }
    let decode = pct_decode;
    let mut env = Vec::new();
    if let Some(host) = url.host_str() {
        env.push(("PGHOST".into(), host.trim_matches(['[', ']']).to_owned()));
    }
    if let Some(port) = url.port() {
        env.push(("PGPORT".into(), port.to_string()));
    }
    if !url.username().is_empty() {
        env.push(("PGUSER".into(), decode(url.username())));
    }
    if let Some(pw) = url.password() {
        env.push(("PGPASSWORD".into(), decode(pw)));
    }
    let db = url.path().trim_start_matches('/');
    if !db.is_empty() {
        env.push(("PGDATABASE".into(), decode(db)));
    }
    for (k, v) in url.query_pairs() {
        let var = match k.as_ref() {
            "sslmode" => "PGSSLMODE",
            "sslrootcert" => "PGSSLROOTCERT",
            "options" => "PGOPTIONS",
            _ => continue,
        };
        env.push((var.into(), v.into_owned()));
    }
    Ok(env)
}

/// Percent-decoding of URL user info / path (`%40` → `@`; `+` stays `+`).
fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

/// Runs the backup; returns a description (file / command) for the log.
pub async fn run(plan: &BackupPlan, database_url: &str, label: &str) -> UpdateResult<String> {
    match plan {
        BackupPlan::Manual => Ok("confirmed by the operator".into()),
        BackupPlan::Command(cmd) => {
            let mut c = Command::new("sh");
            c.arg("-c")
                .arg(cmd)
                .env("HOS_BACKUP_LABEL", label)
                .kill_on_drop(true);
            let out = tokio::time::timeout(BACKUP_TIMEOUT, c.output())
                .await
                .map_err(|_| UpdateError::Backup("the backup command timed out".into()))?
                .map_err(|e| UpdateError::Backup(format!("starting the backup command: {e}")))?;
            if !out.status.success() {
                return Err(UpdateError::Backup(format!(
                    "the backup command exited with {}: {}",
                    out.status,
                    tail(&out.stderr)
                )));
            }
            Ok(format!("HOS_UPDATE_BACKUP_COMMAND ({label})"))
        }
        BackupPlan::PgDump { binary, dir } => {
            std::fs::create_dir_all(dir)?;
            let file = dir.join(format!("hostingos-{label}.dump"));
            let mut c = Command::new(binary);
            c.arg("--format=custom")
                .arg("--no-owner")
                .arg("--file")
                .arg(&file)
                .env_remove("DATABASE_URL")
                .envs(pg_env(database_url)?)
                .kill_on_drop(true);
            let out = tokio::time::timeout(BACKUP_TIMEOUT, c.output())
                .await
                .map_err(|_| UpdateError::Backup("pg_dump timed out".into()))?
                .map_err(|e| UpdateError::Backup(format!("starting pg_dump: {e}")))?;
            if !out.status.success() {
                let _ = std::fs::remove_file(&file);
                return Err(UpdateError::Backup(format!(
                    "pg_dump exited with {}: {}",
                    out.status,
                    tail(&out.stderr)
                )));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
            }
            let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            if size == 0 {
                return Err(UpdateError::Backup("pg_dump wrote an empty file".into()));
            }
            Ok(format!("{} ({size} bytes)", file.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_env_from_url() {
        let env = pg_env("postgres://us%40er:p%23ss@db.example:6543/host_os?sslmode=require&channel_binding=require")
            .unwrap();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("PGHOST").as_deref(), Some("db.example"));
        assert_eq!(get("PGPORT").as_deref(), Some("6543"));
        assert_eq!(get("PGUSER").as_deref(), Some("us@er"));
        assert_eq!(get("PGPASSWORD").as_deref(), Some("p#ss"));
        assert_eq!(get("PGDATABASE").as_deref(), Some("host_os"));
        assert_eq!(get("PGSSLMODE").as_deref(), Some("require"));
        assert!(get("PGCHANNELBINDING").is_none());
        assert!(pg_env("mysql://x@y/z").is_err());
    }

    #[tokio::test]
    async fn command_backup_reports_failure() {
        let ok = run(
            &BackupPlan::Command("test -n \"$HOS_BACKUP_LABEL\"".into()),
            "",
            "lbl",
        )
        .await;
        assert!(ok.is_ok(), "{ok:?}");
        let err = run(
            &BackupPlan::Command("echo boom >&2; exit 3".into()),
            "",
            "lbl",
        )
        .await;
        assert!(matches!(err, Err(UpdateError::Backup(m)) if m.contains("boom")));
    }
}
