//! Process configuration loaded from environment variables (docs/ARCHITECTURE.md §7).
//!
//! [`AppConfig::from_env`] reads `std::env`; loading a `.env` file (e.g. with
//! `dotenvy`) is the binary's job and must happen first. Empty values count as
//! unset. Secrets are wrapped in [`Secret`] and the `Debug` output masks the
//! database password.
//!
//! | Variable | Required | Default |
//! |---|---|---|
//! | `DATABASE_URL` | yes | — (pooled; `channel_binding` stripped) |
//! | `DATABASE_URL_DIRECT` | no | `DATABASE_URL` (migrations, no pooler) |
//! | `HOS_SECRET_KEY` | yes | — (base64 of exactly 32 bytes) |
//! | `HOS_BIND` | no | `127.0.0.1:8080` |
//! | `HOS_BASE_URL` | no | `http://localhost:<port of HOS_BIND>` |
//! | `HOS_BASE_DOMAIN` | no | `localhost` |
//! | `HOS_DEV_TENANT` | no | `demo` |
//! | `GOOGLE_CLIENT_ID` + `GOOGLE_CLIENT_SECRET` | no (both or neither) | — |
//! | `GITHUB_CLIENT_ID` + `GITHUB_CLIENT_SECRET` | no (both or neither) | — |
//! | `ANTHROPIC_API_KEY` | no | — |
//! | `HOS_AI_MODEL` | no | `claude-opus-5-5` |

use std::fmt;
use std::net::SocketAddr;

use base64::Engine as _;
use url::Url;

use crate::crypto::SecretBox;
use crate::secret::{Secret, SecretString};

/// Environment variable names.
pub mod env {
    /// Pooled application database URL.
    pub const DATABASE_URL: &str = "DATABASE_URL";
    /// Direct (non-pooled) database URL for migrations.
    pub const DATABASE_URL_DIRECT: &str = "DATABASE_URL_DIRECT";
    /// Base64-encoded 32-byte master key for [`SecretBox`](crate::crypto::SecretBox).
    pub const HOS_SECRET_KEY: &str = "HOS_SECRET_KEY";
    /// Public base URL of the platform.
    pub const HOS_BASE_URL: &str = "HOS_BASE_URL";
    /// Base domain for tenant subdomains (`*.{HOS_BASE_DOMAIN}`).
    pub const HOS_BASE_DOMAIN: &str = "HOS_BASE_DOMAIN";
    /// Tenant slug served on `localhost` during development.
    pub const HOS_DEV_TENANT: &str = "HOS_DEV_TENANT";
    /// Listen address.
    pub const HOS_BIND: &str = "HOS_BIND";
    /// Platform-default Google OAuth client id.
    pub const GOOGLE_CLIENT_ID: &str = "GOOGLE_CLIENT_ID";
    /// Platform-default Google OAuth client secret.
    pub const GOOGLE_CLIENT_SECRET: &str = "GOOGLE_CLIENT_SECRET";
    /// Platform-default GitHub OAuth client id.
    pub const GITHUB_CLIENT_ID: &str = "GITHUB_CLIENT_ID";
    /// Platform-default GitHub OAuth client secret.
    pub const GITHUB_CLIENT_SECRET: &str = "GITHUB_CLIENT_SECRET";
    /// Anthropic API key for the default AI provider.
    pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
    /// Claude model id for the default AI provider.
    pub const HOS_AI_MODEL: &str = "HOS_AI_MODEL";
}

/// Default `HOS_BIND`.
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// Default `HOS_DEV_TENANT`.
pub const DEFAULT_DEV_TENANT: &str = "demo";
/// Default `HOS_BASE_DOMAIN`.
pub const DEFAULT_BASE_DOMAIN: &str = "localhost";
/// Default `HOS_AI_MODEL`.
pub const DEFAULT_AI_MODEL: &str = "claude-opus-5-5";

/// Configuration loading error. Never contains secret values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A required variable is missing or empty.
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    /// A variable is present but invalid.
    #[error("invalid environment variable {key}: {reason}")]
    Invalid {
        /// Variable name.
        key: &'static str,
        /// What is wrong (no secret values).
        reason: String,
    },
}

/// OAuth application credentials (client id + secret).
#[derive(Clone, Debug)]
pub struct OAuthAppConfig {
    /// Public client id.
    pub client_id: String,
    /// Client secret.
    pub client_secret: SecretString,
}

/// Process-wide configuration. Build with [`AppConfig::from_env`].
#[derive(Clone)]
pub struct AppConfig {
    /// Pooled database URL, with `channel_binding` removed (sqlx rejects it).
    pub database_url: SecretString,
    /// Direct database URL for migrations (falls back to `database_url`), sanitized.
    pub database_url_direct: SecretString,
    /// 32-byte master key for integration secrets; see [`AppConfig::secret_box`].
    pub secret_key: Secret<[u8; 32]>,
    /// Public base URL of the platform (no trailing slash semantics implied).
    pub base_url: Url,
    /// Base domain for tenant subdomains (lower-case, no leading dot).
    pub base_domain: String,
    /// Tenant slug used for `localhost` requests in development.
    pub dev_tenant: String,
    /// Address the HTTP server listens on.
    pub bind: SocketAddr,
    /// Platform-default Google OAuth app.
    pub google_oauth: Option<OAuthAppConfig>,
    /// Platform-default GitHub OAuth app.
    pub github_oauth: Option<OAuthAppConfig>,
    /// Anthropic API key for the default [`AiProvider`](crate::integrations::ai::AiProvider).
    pub anthropic_api_key: Option<SecretString>,
    /// Claude model id (`HOS_AI_MODEL`).
    pub ai_model: String,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field(
                "database_url",
                &redact_url_password(self.database_url.expose()),
            )
            .field(
                "database_url_direct",
                &redact_url_password(self.database_url_direct.expose()),
            )
            .field("secret_key", &self.secret_key)
            .field("base_url", &self.base_url.as_str())
            .field("base_domain", &self.base_domain)
            .field("dev_tenant", &self.dev_tenant)
            .field("bind", &self.bind)
            .field("google_oauth", &self.google_oauth)
            .field("github_oauth", &self.github_oauth)
            .field("anthropic_api_key", &self.anthropic_api_key)
            .field("ai_model", &self.ai_model)
            .finish()
    }
}

impl AppConfig {
    /// Loads from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Loads using an arbitrary lookup function (handy for tests).
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let get = |key: &'static str| -> Option<String> {
            lookup(key)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let require = |key: &'static str| get(key).ok_or(ConfigError::Missing(key));

        let database_url = parse_database_url(env::DATABASE_URL, &require(env::DATABASE_URL)?)?;
        let database_url_direct = match get(env::DATABASE_URL_DIRECT) {
            Some(v) => parse_database_url(env::DATABASE_URL_DIRECT, &v)?,
            None => database_url.clone(),
        };

        let secret_key = decode_secret_key(&require(env::HOS_SECRET_KEY)?).map_err(|reason| {
            ConfigError::Invalid {
                key: env::HOS_SECRET_KEY,
                reason,
            }
        })?;

        let bind_raw = get(env::HOS_BIND).unwrap_or_else(|| DEFAULT_BIND.to_owned());
        let bind: SocketAddr = bind_raw.parse().map_err(|e| ConfigError::Invalid {
            key: env::HOS_BIND,
            reason: format!("{e} (expected host:port, e.g. {DEFAULT_BIND})"),
        })?;

        let base_url_raw =
            get(env::HOS_BASE_URL).unwrap_or_else(|| format!("http://localhost:{}", bind.port()));
        let base_url = Url::parse(&base_url_raw).map_err(|e| ConfigError::Invalid {
            key: env::HOS_BASE_URL,
            reason: e.to_string(),
        })?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(ConfigError::Invalid {
                key: env::HOS_BASE_URL,
                reason: "scheme must be http or https".into(),
            });
        }

        let base_domain = get(env::HOS_BASE_DOMAIN)
            .unwrap_or_else(|| DEFAULT_BASE_DOMAIN.to_owned())
            .trim_start_matches('.')
            .to_ascii_lowercase();

        Ok(Self {
            database_url: Secret::new(database_url),
            database_url_direct: Secret::new(database_url_direct),
            secret_key: Secret::new(secret_key),
            base_url,
            base_domain,
            dev_tenant: get(env::HOS_DEV_TENANT).unwrap_or_else(|| DEFAULT_DEV_TENANT.to_owned()),
            bind,
            google_oauth: oauth_pair(&get, env::GOOGLE_CLIENT_ID, env::GOOGLE_CLIENT_SECRET)?,
            github_oauth: oauth_pair(&get, env::GITHUB_CLIENT_ID, env::GITHUB_CLIENT_SECRET)?,
            anthropic_api_key: get(env::ANTHROPIC_API_KEY).map(Secret::new),
            ai_model: get(env::HOS_AI_MODEL).unwrap_or_else(|| DEFAULT_AI_MODEL.to_owned()),
        })
    }

    /// The [`SecretBox`] keyed with `HOS_SECRET_KEY`.
    pub fn secret_box(&self) -> SecretBox {
        SecretBox::new(self.secret_key.expose())
    }

    /// `true` when `base_domain` is `localhost` (development mode heuristics).
    pub fn is_local(&self) -> bool {
        self.base_domain == "localhost"
    }
}

fn oauth_pair<G>(
    get: &G,
    id_key: &'static str,
    secret_key: &'static str,
) -> Result<Option<OAuthAppConfig>, ConfigError>
where
    G: Fn(&'static str) -> Option<String>,
{
    match (get(id_key), get(secret_key)) {
        (Some(client_id), Some(secret)) => Ok(Some(OAuthAppConfig {
            client_id,
            client_secret: Secret::new(secret),
        })),
        (None, None) => Ok(None),
        (Some(_), None) => Err(ConfigError::Missing(secret_key)),
        (None, Some(_)) => Err(ConfigError::Missing(id_key)),
    }
}

fn parse_database_url(key: &'static str, raw: &str) -> Result<String, ConfigError> {
    let cleaned = strip_channel_binding(raw);
    let parsed = Url::parse(&cleaned).map_err(|e| ConfigError::Invalid {
        key,
        reason: e.to_string(),
    })?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql") {
        return Err(ConfigError::Invalid {
            key,
            reason: "scheme must be postgres:// or postgresql://".into(),
        });
    }
    Ok(cleaned)
}

/// Decodes a base64 (standard or URL-safe, padded or not) 32-byte key.
pub fn decode_secret_key(encoded: &str) -> Result<[u8; 32], String> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let encoded = encoded.trim();
    let bytes = [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
        .iter()
        .find_map(|engine| engine.decode(encoded).ok())
        .ok_or_else(|| "not valid base64".to_owned())?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| format!("must decode to exactly 32 bytes, got {}", bytes.len()))
}

/// Removes every `channel_binding=...` query parameter from a Postgres URL
/// (sqlx does not understand it; Neon and similar providers add it). All other
/// characters — including percent-encoding and other parameters — are kept
/// verbatim; a trailing `?` is dropped when nothing remains.
///
/// ```
/// use hos_core::config::strip_channel_binding;
/// assert_eq!(
///     strip_channel_binding("postgres://u:p@h/db?sslmode=require&channel_binding=require"),
///     "postgres://u:p@h/db?sslmode=require"
/// );
/// ```
pub fn strip_channel_binding(url: &str) -> String {
    let (before_fragment, fragment) = match url.split_once('#') {
        Some((a, f)) => (a, Some(f)),
        None => (url, None),
    };
    let Some((base, query)) = before_fragment.split_once('?') else {
        return url.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(k, _)| k);
            !pair.is_empty() && !key.eq_ignore_ascii_case("channel_binding")
        })
        .collect();
    let mut out = String::with_capacity(url.len());
    out.push_str(base);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// Replaces the password of a URL with `***` for logging
/// (`postgres://u:secret@h/db` → `postgres://u:***@h/db`). Unparsable input
/// is fully replaced by `"<redacted>"`.
pub fn redact_url_password(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) => {
            if parsed.password().is_some() && parsed.set_password(Some("***")).is_err() {
                return "<redacted>".to_owned();
            }
            parsed.to_string()
        }
        Err(_) => "<redacted>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEY_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn strip_channel_binding_cases() {
        let cases = [
            (
                "postgres://u:p@h:5432/db?sslmode=require&channel_binding=require",
                "postgres://u:p@h:5432/db?sslmode=require",
            ),
            (
                "postgres://u:p@h/db?channel_binding=require&sslmode=require",
                "postgres://u:p@h/db?sslmode=require",
            ),
            (
                "postgres://u:p@h/db?channel_binding=require",
                "postgres://u:p@h/db",
            ),
            ("postgres://u:p@h/db", "postgres://u:p@h/db"),
            (
                "postgres://u:p%40ss@h/db?a=1&channel_binding=disable&b=%20x",
                "postgres://u:p%40ss@h/db?a=1&b=%20x",
            ),
            (
                "postgresql://h/db?CHANNEL_BINDING=prefer&sslmode=verify-full#frag",
                "postgresql://h/db?sslmode=verify-full#frag",
            ),
            (
                "postgres://h/db?&&sslmode=require&",
                "postgres://h/db?sslmode=require",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_channel_binding(input), expected, "input: {input}");
        }
    }

    #[test]
    fn redact_password() {
        assert_eq!(
            redact_url_password("postgres://user:s3cr3t@db.example.com/app?sslmode=require"),
            "postgres://user:***@db.example.com/app?sslmode=require"
        );
        assert_eq!(redact_url_password("postgres://h/db"), "postgres://h/db");
        assert_eq!(redact_url_password("not a url"), "<redacted>");
    }

    #[test]
    fn loads_minimal_config_with_defaults() {
        let cfg = AppConfig::from_lookup(lookup(&[
            (
                "DATABASE_URL",
                "postgres://app:pw@db/hostingos?sslmode=require&channel_binding=require",
            ),
            ("HOS_SECRET_KEY", KEY_B64),
            ("GOOGLE_CLIENT_ID", ""),
        ]))
        .unwrap();
        assert_eq!(
            cfg.database_url.expose(),
            "postgres://app:pw@db/hostingos?sslmode=require"
        );
        assert_eq!(cfg.database_url_direct.expose(), cfg.database_url.expose());
        assert_eq!(cfg.secret_key.expose()[31], 31);
        assert_eq!(cfg.bind.to_string(), DEFAULT_BIND);
        assert_eq!(cfg.base_url.as_str(), "http://localhost:8080/");
        assert_eq!(cfg.base_domain, "localhost");
        assert_eq!(cfg.dev_tenant, "demo");
        assert_eq!(cfg.ai_model, DEFAULT_AI_MODEL);
        assert!(cfg.google_oauth.is_none());
        assert!(cfg.anthropic_api_key.is_none());
        assert!(cfg.is_local());
    }

    #[test]
    fn loads_full_config() {
        let cfg = AppConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "postgres://app:pw@pooler/hostingos"),
            (
                "DATABASE_URL_DIRECT",
                "postgres://app:pw@direct/hostingos?channel_binding=require",
            ),
            ("HOS_SECRET_KEY", KEY_B64),
            ("HOS_BIND", "0.0.0.0:3000"),
            ("HOS_BASE_URL", "https://panel.example.com"),
            ("HOS_BASE_DOMAIN", ".Example.com"),
            ("HOS_DEV_TENANT", "acme"),
            ("GITHUB_CLIENT_ID", "gh-id"),
            ("GITHUB_CLIENT_SECRET", "gh-secret"),
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("HOS_AI_MODEL", "claude-sonnet-5-5"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.database_url_direct.expose(),
            "postgres://app:pw@direct/hostingos"
        );
        assert_eq!(cfg.bind.port(), 3000);
        assert_eq!(cfg.base_domain, "example.com");
        assert_eq!(cfg.dev_tenant, "acme");
        let gh = cfg.github_oauth.as_ref().unwrap();
        assert_eq!(gh.client_id, "gh-id");
        assert_eq!(gh.client_secret.expose(), "gh-secret");
        assert_eq!(cfg.ai_model, "claude-sonnet-5-5");
        assert!(!cfg.is_local());
    }

    #[test]
    fn debug_masks_secrets() {
        let cfg = AppConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "postgres://app:topsecretpw@db/hostingos"),
            ("HOS_SECRET_KEY", KEY_B64),
            ("GOOGLE_CLIENT_ID", "gid"),
            ("GOOGLE_CLIENT_SECRET", "googlesecretvalue"),
            ("ANTHROPIC_API_KEY", "sk-ant-verysecret"),
        ]))
        .unwrap();
        let dbg = format!("{cfg:?}");
        for secret in [
            "topsecretpw",
            "googlesecretvalue",
            "sk-ant-verysecret",
            KEY_B64,
        ] {
            assert!(!dbg.contains(secret), "leaked {secret} in {dbg}");
        }
        assert!(dbg.contains("postgres://app:***@db/hostingos"));
        assert!(dbg.contains("gid"));
    }

    #[test]
    fn errors() {
        let err = AppConfig::from_lookup(lookup(&[("HOS_SECRET_KEY", KEY_B64)])).unwrap_err();
        assert_eq!(err, ConfigError::Missing("DATABASE_URL"));

        let err = AppConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "mysql://x/y"),
            ("HOS_SECRET_KEY", KEY_B64),
        ]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "DATABASE_URL",
                ..
            }
        ));

        let err = AppConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "postgres://x/y"),
            ("HOS_SECRET_KEY", "c2hvcnQ="),
        ]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "HOS_SECRET_KEY",
                ..
            }
        ));

        let err = AppConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "postgres://x/y"),
            ("HOS_SECRET_KEY", KEY_B64),
            ("GOOGLE_CLIENT_ID", "only-id"),
        ]))
        .unwrap_err();
        assert_eq!(err, ConfigError::Missing("GOOGLE_CLIENT_SECRET"));

        let err = AppConfig::from_lookup(lookup(&[
            ("DATABASE_URL", "postgres://x/y"),
            ("HOS_SECRET_KEY", KEY_B64),
            ("HOS_BIND", "localhost"),
        ]))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "HOS_BIND",
                ..
            }
        ));
    }

    #[test]
    fn secret_key_decoding_variants() {
        let url_safe_no_pad = KEY_B64
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_");
        assert_eq!(decode_secret_key(&url_safe_no_pad).unwrap()[0], 0);
        assert!(decode_secret_key("!!!").is_err());
    }
}
