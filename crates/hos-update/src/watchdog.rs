//! The update marker (`<root>/state/update-marker.json`): what the old
//! process leaves behind for the new one, so a release that fails to come
//! up healthy is rolled back even though the process that installed it is
//! gone.
//!
//! Phases, written by [`crate::apply`]:
//!
//! 1. `swapping` — written before the `current` link changes;
//! 2. `migrating` — the link changed, `hostingos migrate` of the new
//!    release runs;
//! 3. `restarting` — the process exits for the supervisor to start the new
//!    binary, with a deadline.
//!
//! Every `hostingos serve` / `worker` start calls [`on_startup`]:
//!
//! * no marker → [`Startup::Clean`];
//! * `restarting` and we are the new version → attempt counter + 1; within
//!   the deadline and attempt limit → [`Startup::Trial`]: the caller runs
//!   its health check and calls [`confirm_healthy`] or [`fail_trial`].
//!   Otherwise the old release is made current again → [`Startup::RestartNow`]
//!   (exit; the supervisor starts the old binary);
//! * `swapping` / `migrating` (the installer crashed half-way) → the old
//!   release is made current again (recover), as above;
//! * `rolled_back` and we are the old version → [`Startup::RolledBack`]: the
//!   caller records the outcome and calls [`clear`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{UpdateError, UpdateResult};
use crate::layout::{Layout, atomic_write};

/// Default number of starts of a new binary before it is given up.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Update phase recorded in the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// About to switch `current`.
    Swapping,
    /// `current` switched; migrations running.
    Migrating,
    /// Waiting for the new binary to become healthy.
    Restarting,
    /// The old release was put back.
    RolledBack,
}

impl Phase {
    /// Snake-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Swapping => "swapping",
            Self::Migrating => "migrating",
            Self::Restarting => "restarting",
            Self::RolledBack => "rolled_back",
        }
    }
}

/// The marker file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// `update_runs.id`.
    pub run_id: String,
    /// Version before the update.
    pub from_version: String,
    /// Version being installed.
    pub to_version: String,
    /// Release directory of `from_version`.
    pub old_release: PathBuf,
    /// Release directory of `to_version`.
    pub new_release: PathBuf,
    /// What `previous` pointed to before the update.
    #[serde(default)]
    pub prior_previous: Option<PathBuf>,
    /// Phase.
    pub phase: Phase,
    /// Unix time by which the new binary must be healthy.
    #[serde(default)]
    pub deadline: i64,
    /// Starts of the new binary so far.
    #[serde(default)]
    pub attempts: u32,
    /// Starts allowed.
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    /// Manifest flag: the old binary runs on the migrated schema.
    #[serde(default)]
    pub rollback_safe: bool,
    /// Why it was rolled back.
    #[serde(default)]
    pub message: Option<String>,
}

const fn default_attempts() -> u32 {
    DEFAULT_MAX_ATTEMPTS
}

/// Reads the marker, if any.
pub fn read(layout: &Layout) -> UpdateResult<Option<Marker>> {
    match std::fs::read(layout.marker_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            UpdateError::Preflight(format!(
                "{} is unreadable: {e}",
                layout.marker_path().display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes the marker atomically.
pub fn write(layout: &Layout, marker: &Marker) -> UpdateResult<()> {
    std::fs::create_dir_all(layout.state_dir())?;
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|e| UpdateError::Preflight(format!("serialising the marker: {e}")))?;
    atomic_write(&layout.marker_path(), &bytes)?;
    Ok(())
}

/// Removes the marker.
pub fn clear(layout: &Layout) -> UpdateResult<()> {
    match std::fs::remove_file(layout.marker_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// What a starting process has to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// No update in flight.
    Clean,
    /// We are the new release on trial: check health, then
    /// [`confirm_healthy`] or [`fail_trial`].
    Trial(Marker),
    /// We are the old release after a rollback: record it, then [`clear`].
    RolledBack(Marker),
    /// The old release was just made current again; exit so the supervisor
    /// starts it ([`crate::restart::EXIT_CODE`]).
    RestartNow(Marker),
    /// The marker is about versions other than ours: left alone.
    Unrelated(Marker),
    /// Another process on this host is installing right now (it holds
    /// `state/update.lock`): left alone.
    InProgress(Marker),
}

fn installer_alive(layout: &Layout) -> bool {
    std::fs::metadata(layout.lock_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < crate::apply::STALE_LOCK)
}

/// Puts the old release back (`current` := old, `previous` := what it was
/// before the update) and marks the marker rolled back.
pub fn roll_back(layout: &Layout, marker: &mut Marker, reason: &str) -> UpdateResult<()> {
    layout.point_current(&marker.old_release)?;
    layout.point_previous(marker.prior_previous.as_deref())?;
    marker.phase = Phase::RolledBack;
    marker.message = Some(reason.to_owned());
    write(layout, marker)
}

/// Startup decision (see the module docs). `now` is a Unix timestamp.
pub fn on_startup(layout: &Layout, own_version: &str, now: i64) -> UpdateResult<Startup> {
    let Some(mut m) = read(layout)? else {
        return Ok(Startup::Clean);
    };
    let is_new = own_version == m.to_version;
    let is_old = own_version == m.from_version;
    if !is_new && !is_old {
        return Ok(Startup::Unrelated(m));
    }
    match m.phase {
        Phase::Swapping | Phase::Migrating if installer_alive(layout) => Ok(Startup::InProgress(m)),
        Phase::Swapping | Phase::Migrating => {
            let reason = format!(
                "the update was interrupted while {}; the previous release was restored",
                if m.phase == Phase::Swapping {
                    "switching releases"
                } else {
                    "running migrations"
                }
            );
            roll_back(layout, &mut m, &reason)?;
            Ok(if is_new {
                Startup::RestartNow(m)
            } else {
                Startup::RolledBack(m)
            })
        }
        Phase::Restarting if is_new => {
            m.attempts += 1;
            if m.attempts > m.max_attempts {
                let reason = format!(
                    "the new release failed to start {} times; rolled back",
                    m.max_attempts
                );
                roll_back(layout, &mut m, &reason)?;
                return Ok(Startup::RestartNow(m));
            }
            if m.deadline > 0 && now > m.deadline {
                roll_back(
                    layout,
                    &mut m,
                    "the new release did not become healthy in time; rolled back",
                )?;
                return Ok(Startup::RestartNow(m));
            }
            write(layout, &m)?;
            Ok(Startup::Trial(m))
        }
        Phase::Restarting => {
            // The supervisor started the old binary again (the link was not
            // followed, or it was changed by hand).
            roll_back(
                layout,
                &mut m,
                "the previous release was started instead of the new one",
            )?;
            Ok(Startup::RolledBack(m))
        }
        Phase::RolledBack if is_old => Ok(Startup::RolledBack(m)),
        Phase::RolledBack => {
            let reason = m.message.clone().unwrap_or_else(|| "rolled back".into());
            roll_back(layout, &mut m, &reason)?;
            Ok(Startup::RestartNow(m))
        }
    }
}

/// The trial succeeded: removes the marker and returns it.
pub fn confirm_healthy(layout: &Layout) -> UpdateResult<Option<Marker>> {
    let marker = read(layout)?;
    match &marker {
        Some(m) if m.phase == Phase::Restarting => {
            clear(layout)?;
            Ok(marker)
        }
        _ => Ok(None),
    }
}

/// The trial failed: rolls back and returns the marker (the caller exits
/// with [`crate::restart::EXIT_CODE`]).
pub fn fail_trial(layout: &Layout, reason: &str) -> UpdateResult<Option<Marker>> {
    let Some(mut m) = read(layout)? else {
        return Ok(None);
    };
    if m.phase != Phase::Restarting {
        return Ok(None);
    }
    roll_back(layout, &mut m, reason)?;
    Ok(Some(m))
}
