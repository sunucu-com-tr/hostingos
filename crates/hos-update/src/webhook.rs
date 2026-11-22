//! Deploy webhook for container installs: when the operator clicks
//! "Update" on a Docker / Compose / Kubernetes / Coolify install, HostingOS
//! does not touch its own (immutable) image; it calls a URL that redeploys
//! it — a Coolify deploy webhook, Watchtower's HTTP API behind TLS, a CI
//! pipeline trigger …
//!
//! * HTTPS only, through the outbound policy, no redirects followed.
//! * `GET`: just the URL (Coolify: `…/api/v1/deploy?uuid=…`) with
//!   `Authorization: Bearer <secret>` when a secret is set.
//! * `POST`: JSON `{"event":"hostingos.update","version":…,"image":…,
//!   "channel":…,"timestamp":…}` with the bearer token and
//!   `X-HostingOS-Signature: sha256=<hex HMAC-SHA256(secret, body)>`.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use url::Url;

use crate::error::{UpdateError, UpdateResult};
use crate::fetch::{Fetcher, redact};

/// HTTP method of the webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebhookMethod {
    /// `GET` (Coolify, Watchtower).
    #[default]
    Get,
    /// `POST` with a signed JSON body.
    Post,
}

impl WebhookMethod {
    /// `GET` / `POST`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }

    /// Parses `get` / `post`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            _ => None,
        }
    }
}

/// A configured deploy webhook.
#[derive(Clone)]
pub struct DeployWebhook {
    /// Target URL.
    pub url: Url,
    /// Method.
    pub method: WebhookMethod,
    /// Bearer token / HMAC key (stored encrypted by the caller).
    pub secret: Option<String>,
}

impl std::fmt::Debug for DeployWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeployWebhook")
            .field("url", &redact(&self.url))
            .field("method", &self.method)
            .field("secret", &self.secret.as_ref().map(|_| "***"))
            .finish()
    }
}

/// What the webhook announces.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookPayload {
    /// Always `hostingos.update`.
    pub event: &'static str,
    /// Version to deploy.
    pub version: String,
    /// Image reference, when the manifest names one.
    pub image: Option<String>,
    /// Channel.
    pub channel: String,
    /// Unix time (replay window for receivers).
    pub timestamp: i64,
}

impl WebhookPayload {
    /// A payload for `version`.
    pub fn new(version: &str, image: Option<&str>, channel: &str) -> Self {
        Self {
            event: "hostingos.update",
            version: version.to_owned(),
            image: image.map(str::to_owned),
            channel: channel.to_owned(),
            timestamp: chrono::Utc::now().timestamp(),
        }
    }
}

/// Validates a webhook URL (HTTPS, outbound policy, no credentials in it).
pub fn validate_url(fetcher: &Fetcher, raw: &str) -> UpdateResult<Url> {
    let url = Url::parse(raw.trim())
        .map_err(|_| UpdateError::Config("the deploy webhook URL is not a URL".into()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UpdateError::Config(
            "put credentials in the secret, not in the webhook URL".into(),
        ));
    }
    fetcher.check_endpoint(&url)?;
    Ok(url)
}

/// Calls the webhook; returns the HTTP status (2xx only).
pub async fn call(
    fetcher: &Fetcher,
    hook: &DeployWebhook,
    payload: &WebhookPayload,
) -> UpdateResult<u16> {
    fetcher.check_endpoint(&hook.url)?;
    let client = fetcher.client();
    let mut req = match hook.method {
        WebhookMethod::Get => client.get(hook.url.clone()),
        WebhookMethod::Post => {
            let body = serde_json::to_vec(payload)
                .map_err(|e| UpdateError::Config(format!("webhook payload: {e}")))?;
            let mut r = client
                .post(hook.url.clone())
                .header(CONTENT_TYPE, "application/json")
                .header("X-HostingOS-Event", payload.event);
            if let Some(secret) = &hook.secret {
                let sig = hos_core::crypto::hmac_sha256_hex(secret.as_bytes(), &body);
                r = r.header("X-HostingOS-Signature", format!("sha256={sig}"));
            }
            r.body(body)
        }
    };
    if let Some(secret) = &hook.secret {
        req = req.header(AUTHORIZATION, format!("Bearer {secret}"));
    }
    let resp = req
        .timeout(fetcher.config().request_timeout)
        .send()
        .await
        .map_err(|e| fetcher.transport_error(&hook.url, &e.without_url()))?;
    let status = resp.status();
    if status.is_success() {
        Ok(status.as_u16())
    } else if status.is_redirection() {
        Err(UpdateError::Refused(format!(
            "the deploy webhook answered with a redirect (HTTP {}); use the final URL",
            status.as_u16()
        )))
    } else {
        Err(UpdateError::Network(format!(
            "the deploy webhook answered HTTP {}",
            status.as_u16()
        )))
    }
}
