//! cloud-init user-data builder for providers that take raw user data
//! (Hetzner Cloud, vSphere cloud-init guest customization).
//!
//! HostingOS's own settings (hostname, root password, root SSH keys) go into a
//! generated `#cloud-config` document. JSON is valid YAML 1.2, so the document
//! body is emitted as JSON — no YAML serializer needed and no quoting bugs.
//! When the caller also supplies user data, both are combined into a MIME
//! multi-part archive (a format cloud-init documents and merges natively); the
//! caller's part comes last so its keys win.

use hos_core::secret::SecretString;
use serde_json::{Map, Value, json};

use crate::util::split_fqdn;

/// Boundary of the multi-part archive (must not appear in the parts).
const BOUNDARY: &str = "==HostingOSCloudInitBoundary==";

/// Inputs of [`CloudConfig::render`].
#[derive(Debug, Default)]
pub(crate) struct CloudConfig<'a> {
    /// Guest FQDN (sets `hostname` + `fqdn`).
    pub hostname: Option<&'a str>,
    /// Root password (plain; cloud-init hashes it).
    pub root_password: Option<&'a SecretString>,
    /// Keys for root's `authorized_keys`.
    pub ssh_keys: &'a [String],
    /// Caller-supplied user data (`#cloud-config`, `#!` script, MIME, ...).
    pub user_data: Option<&'a str>,
}

impl CloudConfig<'_> {
    /// Whether HostingOS needs to inject anything itself.
    fn has_own_settings(&self) -> bool {
        self.hostname.is_some_and(|h| !h.trim().is_empty())
            || self.root_password.is_some()
            || !self.ssh_keys.is_empty()
    }

    /// The generated `#cloud-config` document (without caller data).
    pub(crate) fn own_document(&self) -> String {
        let mut doc = Map::new();
        if let Some(fqdn) = self.hostname.map(str::trim).filter(|h| !h.is_empty()) {
            let (host, _) = split_fqdn(fqdn);
            doc.insert("hostname".into(), json!(host));
            doc.insert("fqdn".into(), json!(fqdn));
            doc.insert("preserve_hostname".into(), json!(false));
            doc.insert("manage_etc_hosts".into(), json!(true));
        }
        if self.root_password.is_some() || !self.ssh_keys.is_empty() {
            // Otherwise cloud-init's default (`disable_root: true`) locks root out.
            doc.insert("disable_root".into(), json!(false));
        }
        if let Some(pw) = self.root_password {
            doc.insert("ssh_pwauth".into(), json!(true));
            doc.insert(
                "chpasswd".into(),
                json!({
                    "expire": false,
                    "users": [{"name": "root", "password": pw.expose_str(), "type": "text"}]
                }),
            );
        }
        if !self.ssh_keys.is_empty() {
            let keys: Vec<&str> = self.ssh_keys.iter().map(|k| k.trim()).collect();
            doc.insert(
                "users".into(),
                json!(["default", {"name": "root", "ssh_authorized_keys": keys}]),
            );
        }
        format!("#cloud-config\n{}\n", Value::Object(doc))
    }

    /// Renders the user data to send, or `None` when there is nothing to send.
    pub(crate) fn render(&self) -> Option<String> {
        let user = self.user_data.map(str::trim).filter(|u| !u.is_empty());
        match (self.has_own_settings(), user) {
            (false, None) => None,
            (false, Some(u)) => Some(format!("{u}\n")),
            (true, None) => Some(self.own_document()),
            (true, Some(u)) => Some(multipart(&[
                ("text/cloud-config", "hostingos.cfg", &self.own_document()),
                (content_type_of(u), "user-data", u),
            ])),
        }
    }
}

/// cloud-init part type of a user-data blob, from its first line.
fn content_type_of(data: &str) -> &'static str {
    let first = data.lines().next().unwrap_or_default().trim();
    if first.starts_with("#cloud-config") {
        "text/cloud-config"
    } else if first.starts_with("#!") {
        "text/x-shellscript"
    } else if first.starts_with("#cloud-boothook") {
        "text/cloud-boothook"
    } else if first.starts_with("#include") {
        "text/x-include-url"
    } else if first.starts_with("## template: jinja") {
        "text/jinja2"
    } else {
        "text/plain"
    }
}

fn multipart(parts: &[(&str, &str, &str)]) -> String {
    let mut out =
        format!("Content-Type: multipart/mixed; boundary=\"{BOUNDARY}\"\nMIME-Version: 1.0\n\n");
    for (content_type, filename, body) in parts {
        out.push_str(&format!(
            "--{BOUNDARY}\nContent-Type: {content_type}; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: 8bit\nContent-Disposition: attachment; filename=\"{filename}\"\n\n{}\n",
            body.trim_end()
        ));
    }
    out.push_str(&format!("--{BOUNDARY}--\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_render() {
        assert_eq!(CloudConfig::default().render(), None);
    }

    #[test]
    fn passthrough_user_data() {
        let c = CloudConfig {
            user_data: Some("#!/bin/sh\necho hi"),
            ..Default::default()
        };
        assert_eq!(c.render().unwrap(), "#!/bin/sh\necho hi\n");
    }

    #[test]
    fn own_settings_are_json_cloud_config() {
        let pw = SecretString::from("s3cr\"et");
        let keys = vec!["ssh-ed25519 AAAAC3Nza me@x".to_owned()];
        let c = CloudConfig {
            hostname: Some("vps1.example.com"),
            root_password: Some(&pw),
            ssh_keys: &keys,
            user_data: None,
        };
        let out = c.render().unwrap();
        assert!(out.starts_with("#cloud-config\n"));
        let doc: Value = serde_json::from_str(out.trim_start_matches("#cloud-config\n")).unwrap();
        assert_eq!(doc["hostname"], "vps1");
        assert_eq!(doc["fqdn"], "vps1.example.com");
        assert_eq!(doc["chpasswd"]["users"][0]["password"], "s3cr\"et");
        assert_eq!(
            doc["users"][1]["ssh_authorized_keys"][0],
            "ssh-ed25519 AAAAC3Nza me@x"
        );
        assert_eq!(doc["disable_root"], false);
    }

    #[test]
    fn combined_is_multipart() {
        let keys: Vec<String> = Vec::new();
        let c = CloudConfig {
            hostname: Some("h.example.com"),
            root_password: None,
            ssh_keys: &keys,
            user_data: Some("#cloud-config\npackages: [htop]"),
        };
        let out = c.render().unwrap();
        assert!(out.starts_with("Content-Type: multipart/mixed"));
        assert_eq!(out.matches(&format!("--{BOUNDARY}\n")).count(), 2);
        assert!(out.contains("Content-Type: text/cloud-config; charset=\"utf-8\"\nMIME-Version: 1.0\nContent-Transfer-Encoding: 8bit\nContent-Disposition: attachment; filename=\"user-data\"\n\n#cloud-config\npackages: [htop]"));
        assert!(out.trim_end().ends_with(&format!("--{BOUNDARY}--")));
    }
}
