//! # hos-update
//!
//! Self-update of the `hostingos` binary (docs/UPDATES.md). This crate holds
//! everything that does not need the database; `hos-web` wires it to the
//! platform console (`/platform/updates`), the CLI (`hostingos update …`),
//! the background check loop and the tables of migration 0045.
//!
//! | Module | Contents |
//! |---|---|
//! | [`build_info`] | version, git commit, build date and target compiled into the binary |
//! | [`keys`] | trusted minisign public keys (compiled in + endorsed by signed manifests), signature checks |
//! | [`manifest`] | the signed release manifest format |
//! | [`verify`] | manifest policy: signature, product, downgrade / replay, compatibility |
//! | [`fetch`] | HTTPS-only downloads through the outbound policy, host allow-list, redirect checks, size limits, timeouts |
//! | [`source`] | where releases come from (`HOS_UPDATE_REPO` / `HOS_UPDATE_MANIFEST_URL`) and discovery |
//! | [`check`] | one update check: discover → download manifests → verify → report |
//! | [`install`] | install-type detection (bare binary under a supervisor, container, unknown) |
//! | [`layout`] | the on-disk release layout (`releases/<v>`, `current`, `previous`), archive extraction, atomic swaps |
//! | [`watchdog`] | the on-disk marker that lets a freshly started binary roll back a failed update |
//! | [`apply`] | the apply state machine (pre-flight → download → verify → backup → swap → migrate → restart) |
//! | [`backup`] | database backup before an update (`HOS_UPDATE_BACKUP_COMMAND` or `pg_dump`) |
//! | [`webhook`] | deploy webhooks for container installs (Coolify, Watchtower, CI) |
//! | [`restart`] | process-wide "restart me" request and the exit code the supervisor restarts on |
//!
//! Nothing here phones home: an update check is a plain HTTPS `GET` of the
//! release list / manifest with a generic user agent (`HostingOS-Updater/<version>`).

#![forbid(unsafe_code)]

pub mod apply;
pub mod backup;
pub mod build_info;
pub mod check;
pub mod error;
pub mod fetch;
pub mod install;
pub mod keys;
pub mod layout;
pub mod manifest;
pub mod restart;
pub mod source;
pub mod verify;
pub mod watchdog;
pub mod webhook;

pub use build_info::BuildInfo;
pub use error::{UpdateError, UpdateResult};
pub use keys::{KeyRing, TrustedKey};
pub use manifest::{Artifact, Channel, Manifest, Severity};

/// Product name every manifest must carry.
pub const PRODUCT: &str = "hostingos";

/// Name of the manifest asset in a release.
pub const MANIFEST_ASSET: &str = "hostingos-manifest.json";

/// Suffix of a detached minisign signature.
pub const SIGNATURE_SUFFIX: &str = ".minisig";
