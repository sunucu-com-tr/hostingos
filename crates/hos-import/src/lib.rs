//! # hos-import — bring an existing installation into HostingOS
//!
//! Imports a WHMCS installation into one HostingOS tenant, either straight
//! from its MySQL/MariaDB database ([`source::mysql`]) or through the WHMCS
//! API ([`source::api`]). Both connectors hand the engine the same row
//! model ([`row::Row`], keyed by WHMCS column names), so every entity has
//! exactly one mapper.
//!
//! | Module | Contents |
//! |---|---|
//! | [`config`] | source configuration, secrets, options, wizard mappings |
//! | [`entity`] | the entities imported, their order and what is unsupported |
//! | [`row`] | source-neutral row values |
//! | [`map`] | pure WHMCS → HostingOS value mappings |
//! | [`crypto`] | WHMCS field decryption, sealing of run secrets |
//! | [`sanitize`] | untrusted text / HTML → plain text or safe Markdown |
//! | [`source`] | the [`source::Source`] trait, DB and API connectors, fixtures |
//! | [`engine`] | dry-run analysis and the silent, idempotent import |
//! | [`rollback`] | deletes what a run created when nothing references it |
//! | [`runner`] | background worker loop over `import_runs` |
//! | [`report`] | JSON / CSV run reports |
//!
//! Guarantees (see `docs/IMPORT-WHMCS.md`): read-only on the source;
//! idempotent through `import_id_map`; silent (no provisioning, e-mails,
//! webhooks or jobs); tenant-scoped; resumable and cancellable; reversible
//! while nothing references the imported rows.

pub mod config;
pub mod crypto;
pub mod engine;
pub mod entity;
pub mod error;
pub mod map;
pub mod report;
pub mod rollback;
pub mod row;
pub mod runner;
pub mod sanitize;
pub mod source;

pub use config::{ImportOptions, Mappings, SourceConfig, SourceSecrets};
pub use entity::Entity;
pub use error::{ImportError, ImportResult};
