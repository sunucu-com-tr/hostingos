//! Run configuration: the source connection (non-secret part stored in
//! `import_runs.source_config`, secrets sealed in `secrets_enc`), the
//! options of the wizard and the mappings (gateway / registrar / server /
//! product / staff role).

use std::collections::{BTreeMap, BTreeSet};

use hos_core::secret::SecretString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::entity::Entity;

/// TLS to the WHMCS database server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Plain TCP (only sensible on a private network).
    #[default]
    Disabled,
    /// TLS with certificate and host-name verification (webpki roots).
    Verify,
    /// TLS, accepting any certificate (self-signed MySQL servers).
    Insecure,
}

/// WHMCS database connection (MySQL / MariaDB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbSourceConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub tls: TlsMode,
    /// Optional API access used only to read ticket attachments, which
    /// WHMCS stores as files, not in the database.
    #[serde(default)]
    pub attachments_api: Option<ApiSourceConfig>,
}

/// WHMCS API connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSourceConfig {
    /// WHMCS system URL (`https://billing.example.com/`) or the full
    /// `…/includes/api.php` URL.
    pub url: String,
    /// API credential identifier (not secret on its own).
    pub identifier: String,
}

/// Non-secret source settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    WhmcsDb(DbSourceConfig),
    WhmcsApi(ApiSourceConfig),
}

impl SourceConfig {
    /// `whmcs_db` | `whmcs_api` (`import_runs.source_kind`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WhmcsDb(_) => "whmcs_db",
            Self::WhmcsApi(_) => "whmcs_api",
        }
    }

    /// Stable identity of the source installation (`import_id_map.source_key`):
    /// re-running an import with the same key updates instead of duplicating.
    pub fn key(&self) -> String {
        match self {
            Self::WhmcsDb(c) => format!(
                "whmcs-db:{}:{}/{}",
                c.host.trim().to_ascii_lowercase(),
                if c.port == 0 { 3306 } else { c.port },
                c.database.trim()
            ),
            Self::WhmcsApi(c) => {
                let u = url::Url::parse(c.url.trim()).ok();
                let host = u
                    .as_ref()
                    .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                    .unwrap_or_default();
                let path = u
                    .as_ref()
                    .map(|u| {
                        u.path()
                            .trim_end_matches("api.php")
                            .trim_end_matches("includes/")
                            .trim_end_matches('/')
                            .to_owned()
                    })
                    .unwrap_or_default();
                format!("whmcs-api:{host}{path}")
            }
        }
    }

    /// Short label for lists (`db.example.com/whmcs`, `billing.example.com`).
    pub fn label(&self) -> String {
        match self {
            Self::WhmcsDb(c) => format!("{}/{}", c.host, c.database),
            Self::WhmcsApi(c) => url::Url::parse(&c.url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_else(|| c.url.clone()),
        }
    }
}

/// Source credentials. Never logged, never serialised into a non-sealed column.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct SourceSecrets {
    #[serde(default)]
    pub db_password: Option<String>,
    /// `$cc_encryption_hash` from WHMCS `configuration.php`.
    #[serde(default)]
    pub cc_encryption_hash: Option<String>,
    #[serde(default)]
    pub api_secret: Option<String>,
    /// Optional `$api_access_key` of `configuration.php` (bypasses the API IP allow-list).
    #[serde(default)]
    pub api_access_key: Option<String>,
}

impl std::fmt::Debug for SourceSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceSecrets")
            .field("db_password", &self.db_password.as_ref().map(|_| "***"))
            .field(
                "cc_encryption_hash",
                &self.cc_encryption_hash.as_ref().map(|_| "***"),
            )
            .field("api_secret", &self.api_secret.as_ref().map(|_| "***"))
            .field(
                "api_access_key",
                &self.api_access_key.as_ref().map(|_| "***"),
            )
            .finish()
    }
}

impl SourceSecrets {
    /// The encryption hash as a secret.
    pub fn cc_hash(&self) -> Option<SecretString> {
        self.cc_encryption_hash
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| SecretString::new(s.to_owned()))
    }
}

fn default_true() -> bool {
    true
}
fn default_grace() -> u32 {
    14
}
fn default_activity_limit() -> u32 {
    5000
}
fn default_attachment_bytes() -> u64 {
    10 * 1024 * 1024
}
fn default_batch() -> u32 {
    250
}

/// Wizard options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportOptions {
    /// Entities to import (empty = every entity whose `default_on` is set).
    #[serde(default)]
    pub entities: BTreeSet<Entity>,
    /// Imported e-mail addresses count as verified (the accounts come from
    /// the company's own billing system, not from anonymous sign-ups).
    #[serde(default = "default_true")]
    pub verified_emails: bool,
    /// Days after the import during which imported services are not
    /// auto-suspended and reminders / late fees that would fall into the
    /// window are marked as already sent.
    #[serde(default = "default_grace")]
    pub grace_days: u32,
    /// Pause the tenant's daily billing automation and TLD price sync until
    /// the admin resumes it from the cut-over checklist.
    #[serde(default = "default_true")]
    pub pause_automation: bool,
    /// Decrypt and carry over hosting account passwords (needs the
    /// `cc_encryption_hash`, database connector only).
    #[serde(default = "default_true")]
    pub service_passwords: bool,
    /// Create (inactive) integrations from WHMCS servers with their decrypted credentials.
    #[serde(default = "default_true")]
    pub server_credentials: bool,
    /// Newest activity-log lines to import (when the entity is selected).
    #[serde(default = "default_activity_limit")]
    pub activity_log_limit: u32,
    /// Largest ticket attachment imported.
    #[serde(default = "default_attachment_bytes")]
    pub attachment_max_bytes: u64,
    /// Locale of imported KB articles, announcements and canned replies.
    #[serde(default)]
    pub content_locale: Option<String>,
    /// Rows per batch (each batch is one transaction).
    #[serde(default = "default_batch")]
    pub batch_size: u32,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            entities: BTreeSet::new(),
            verified_emails: true,
            grace_days: default_grace(),
            pause_automation: true,
            service_passwords: true,
            server_credentials: true,
            activity_log_limit: default_activity_limit(),
            attachment_max_bytes: default_attachment_bytes(),
            content_locale: None,
            batch_size: default_batch(),
        }
    }
}

impl ImportOptions {
    /// Whether `entity` is part of the run.
    pub fn wants(&self, entity: Entity) -> bool {
        if self.entities.is_empty() {
            entity.info().default_on
        } else {
            self.entities.contains(&entity)
        }
    }

    /// Batch size within sane bounds.
    pub fn batch(&self) -> u32 {
        self.batch_size.clamp(10, 1000)
    }
}

/// What to do with a WHMCS server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerTarget {
    /// Create an inactive integration of this provider kind.
    Create(String),
    /// Use an existing integration.
    Existing(Uuid),
    /// Do not import; services keep no integration.
    Skip,
}

/// Wizard mappings. Keys are WHMCS identifiers (module names, ids as text).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mappings {
    /// Payment gateway module (`paypal`) → gateway integration id (`""` = none).
    #[serde(default)]
    pub gateways: BTreeMap<String, String>,
    /// Registrar module (`enom`) → registrar integration id (`""` = none).
    #[serde(default)]
    pub registrars: BTreeMap<String, String>,
    /// Server id → `create:<kind>` | `<integration id>` | `skip`.
    #[serde(default)]
    pub servers: BTreeMap<String, String>,
    /// Product id → `create` | `<product id>`.
    #[serde(default)]
    pub products: BTreeMap<String, String>,
    /// Admin role id → HostingOS role slug (`""` = no role).
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
}

fn uuid_of(v: &str) -> Option<Uuid> {
    Uuid::parse_str(v.trim()).ok()
}

impl Mappings {
    /// Gateway integration of a WHMCS module.
    pub fn gateway(&self, module: &str) -> Option<Uuid> {
        self.gateways.get(module).and_then(|v| uuid_of(v))
    }

    /// Registrar integration of a WHMCS module.
    pub fn registrar(&self, module: &str) -> Option<Uuid> {
        self.registrars.get(module).and_then(|v| uuid_of(v))
    }

    /// Target of a WHMCS server (`default_kind` when unmapped; `None` kind → skip).
    pub fn server(&self, id: i64, default_kind: Option<&str>) -> ServerTarget {
        match self.servers.get(&id.to_string()).map(|s| s.trim()) {
            Some("skip") => ServerTarget::Skip,
            Some(v) if v.starts_with("create:") => {
                ServerTarget::Create(v["create:".len()..].to_owned())
            }
            Some(v) if uuid_of(v).is_some() => {
                uuid_of(v).map_or(ServerTarget::Skip, ServerTarget::Existing)
            }
            _ => default_kind.map_or(ServerTarget::Skip, |k| ServerTarget::Create(k.to_owned())),
        }
    }

    /// Existing product a WHMCS product is mapped to (`None` = create).
    pub fn product(&self, id: i64) -> Option<Uuid> {
        self.products.get(&id.to_string()).and_then(|v| uuid_of(v))
    }

    /// HostingOS role slug of a WHMCS admin role.
    pub fn role(&self, id: i64) -> Option<&str> {
        self.roles
            .get(&id.to_string())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_defaults_and_selection() {
        let o = ImportOptions::default();
        assert!(o.wants(Entity::Clients));
        assert!(!o.wants(Entity::ActivityLog));
        let mut o2 = o.clone();
        o2.entities.insert(Entity::ActivityLog);
        assert!(o2.wants(Entity::ActivityLog));
        assert!(!o2.wants(Entity::Clients));
        let parsed: ImportOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, o);
        assert_eq!(o.batch(), 250);
    }

    #[test]
    fn mappings() {
        let id = Uuid::now_v7();
        let mut m = Mappings::default();
        m.servers.insert("1".into(), "skip".into());
        m.servers.insert("2".into(), "create:plesk".into());
        m.servers.insert("3".into(), id.to_string());
        m.gateways.insert("paypal".into(), id.to_string());
        m.gateways.insert("mailin".into(), String::new());
        m.roles.insert("1".into(), "support".into());
        assert_eq!(m.server(1, Some("cpanel")), ServerTarget::Skip);
        assert_eq!(
            m.server(2, Some("cpanel")),
            ServerTarget::Create("plesk".into())
        );
        assert_eq!(m.server(3, None), ServerTarget::Existing(id));
        assert_eq!(
            m.server(4, Some("cpanel")),
            ServerTarget::Create("cpanel".into())
        );
        assert_eq!(m.server(4, None), ServerTarget::Skip);
        assert_eq!(m.gateway("paypal"), Some(id));
        assert_eq!(m.gateway("mailin"), None);
        assert_eq!(m.role(1), Some("support"));
        assert_eq!(m.role(2), None);
    }

    #[test]
    fn source_keys_are_stable() {
        let db = SourceConfig::WhmcsDb(DbSourceConfig {
            host: " DB.Example.com ".into(),
            port: 0,
            database: "whmcs".into(),
            user: "u".into(),
            tls: TlsMode::Disabled,
            attachments_api: None,
        });
        assert_eq!(db.key(), "whmcs-db:db.example.com:3306/whmcs");
        let api = |url: &str| {
            SourceConfig::WhmcsApi(ApiSourceConfig {
                url: url.into(),
                identifier: "x".into(),
            })
            .key()
        };
        assert_eq!(
            api("https://Billing.example.com/"),
            "whmcs-api:billing.example.com"
        );
        assert_eq!(
            api("https://billing.example.com/includes/api.php"),
            "whmcs-api:billing.example.com"
        );
        assert_eq!(
            api("https://example.com/whmcs/"),
            "whmcs-api:example.com/whmcs"
        );
    }

    #[test]
    fn secrets_never_debug_printed() {
        let s = SourceSecrets {
            db_password: Some("hunter2".into()),
            cc_encryption_hash: Some("abc".into()),
            ..SourceSecrets::default()
        };
        let d = format!("{s:?}");
        assert!(!d.contains("hunter2") && !d.contains("abc"));
        assert_eq!(
            SourceConfig::WhmcsApi(ApiSourceConfig {
                url: "https://billing.example.com/includes/api.php".into(),
                identifier: "x".into()
            })
            .label(),
            "billing.example.com"
        );
    }
}
