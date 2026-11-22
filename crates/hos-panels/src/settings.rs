//! Settings shared by every HTTP panel: descriptor fields and their parsing.

use std::net::IpAddr;
use std::time::Duration;

use hos_core::IntegrationError;
use hos_core::integrations::{FieldReader, FieldSpec, IntegrationDescriptor};
use hos_core::net::OutboundPolicy;
use url::Url;

use crate::http::{Endpoint, base_url};

/// Default whole-request timeout. Account creation on a busy cPanel server
/// routinely takes 30–60 s.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Appends the connection fields every panel shares.
pub(crate) fn connection_fields(
    descriptor: IntegrationDescriptor,
    default_port: u16,
    host_placeholder: &'static str,
) -> IntegrationDescriptor {
    descriptor
        .config_field(
            FieldSpec::text("host", "Hostname or IP")
                .required()
                .placeholder(host_placeholder)
                .help("Server hostname / IP, or a full URL such as https://server.example.com:2087"),
        )
        .config_field(
            FieldSpec::number("port", "API port")
                .default_value(default_port)
                .help("Ignored when the host field already contains a port"),
        )
        .config_field(FieldSpec::boolean("use_ssl", "Use HTTPS").default_value(true))
        .config_field(
            FieldSpec::boolean("skip_tls_verify", "Skip TLS certificate verification")
                .default_value(false)
                .help("Only for servers with self-signed certificates. Insecure: credentials can be intercepted"),
        )
        .config_field(
            FieldSpec::number("timeout_secs", "Request timeout (seconds)")
                .default_value(DEFAULT_TIMEOUT_SECS),
        )
}

/// Appends the provisioning defaults every panel shares (`default_ip` only
/// for panels that take an IP when creating accounts).
pub(crate) fn provisioning_fields(
    descriptor: IntegrationDescriptor,
    with_ip: bool,
) -> IntegrationDescriptor {
    let descriptor = if with_ip {
        descriptor.config_field(
            FieldSpec::text("default_ip", "Default IP address")
                .help("Used for new accounts when the order does not request a dedicated IP"),
        )
    } else {
        descriptor
    };
    descriptor
        .config_field(
            FieldSpec::text("nameservers", "Nameservers")
                .placeholder("ns1.example.com, ns2.example.com")
                .help("Comma-separated; reported to the client in the welcome email"),
        )
        .config_field(
            FieldSpec::url("login_url", "Client login URL")
                .help("Public panel URL for welcome emails; derived from the host when empty"),
        )
}

/// Parsed shared settings.
#[derive(Debug, Clone)]
pub(crate) struct CommonSettings {
    pub endpoint: Endpoint,
    pub https: bool,
    pub default_ip: Option<IpAddr>,
    pub nameservers: Vec<String>,
    pub login_url: Option<Url>,
}

impl CommonSettings {
    pub fn read(
        config: &FieldReader<'_>,
        default_port: u16,
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        let https = config.bool_or("use_ssl", true)?;
        let port = config.u64_or("port", u64::from(default_port))?;
        let port = u16::try_from(port)
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| IntegrationError::Config("config.port must be 1-65535".into()))?;
        let base = base_url(config.str("host")?, port, https)?;
        let timeout_secs = config.u64_or("timeout_secs", DEFAULT_TIMEOUT_SECS)?;
        if !(1..=3600).contains(&timeout_secs) {
            return Err(IntegrationError::Config(
                "config.timeout_secs must be 1-3600".into(),
            ));
        }
        let default_ip = config
            .opt_str("default_ip")?
            .map(|s| {
                s.parse::<IpAddr>().map_err(|_| {
                    IntegrationError::Config("config.default_ip must be an IP address".into())
                })
            })
            .transpose()?;
        let nameservers = parse_list(config.opt_str("nameservers")?.unwrap_or_default());
        Ok(Self {
            https: base.scheme() == "https",
            endpoint: Endpoint {
                base,
                timeout: Duration::from_secs(timeout_secs),
                skip_tls_verify: config.bool_or("skip_tls_verify", false)?,
                policy,
            },
            default_ip,
            nameservers,
            login_url: config.opt_url("login_url")?,
        })
    }

    /// Configured login URL, or `scheme://<api host>:<port>/` with the panel's
    /// end-user port (`https_port` / `http_port`).
    pub fn login_url_or(&self, https_port: u16, http_port: u16) -> String {
        if let Some(url) = &self.login_url {
            return url.to_string();
        }
        let mut url = self.endpoint.base.clone();
        let port = if self.https { https_port } else { http_port };
        // `set_port` only fails for cannot-be-a-base URLs, which `base` never is.
        let _ = url.set_port(Some(port));
        url.to_string()
    }
}

/// Splits a comma/whitespace separated list, dropping blanks.
pub(crate) fn parse_list(s: &str) -> Vec<String> {
    s.split([',', ';', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_common_settings() {
        let cfg = json!({
            "host": "srv.example.com", "port": "2087", "use_ssl": "on",
            "default_ip": "192.0.2.10", "nameservers": "ns1.example.com, ns2.example.com",
            "timeout_secs": 30
        });
        let s =
            CommonSettings::read(&FieldReader::config(&cfg), 1, OutboundPolicy::Private).unwrap();
        assert_eq!(s.endpoint.base.as_str(), "https://srv.example.com:2087/");
        assert_eq!(s.endpoint.timeout, Duration::from_secs(30));
        assert!(!s.endpoint.skip_tls_verify);
        assert_eq!(s.default_ip, Some("192.0.2.10".parse().unwrap()));
        assert_eq!(s.nameservers, vec!["ns1.example.com", "ns2.example.com"]);
        assert_eq!(s.login_url_or(2083, 2082), "https://srv.example.com:2083/");

        let bad = json!({"host": "x", "default_ip": "nope"});
        assert!(
            CommonSettings::read(&FieldReader::config(&bad), 1, OutboundPolicy::Private).is_err()
        );
        let bad_port = json!({"host": "x", "port": 70000});
        assert!(
            CommonSettings::read(&FieldReader::config(&bad_port), 1, OutboundPolicy::Private)
                .is_err()
        );
    }
}
