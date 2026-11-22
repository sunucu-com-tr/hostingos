//! HTTPS downloads for the updater.
//!
//! * Every request goes through [`hos_core::net::OutboundPolicy`]: the
//!   client resolves names with the policy's checked resolver, ignores
//!   system proxies and never follows redirects by itself.
//! * Redirects are followed here, at most [`NetConfig::max_redirects`], and
//!   **every** hop must be HTTPS and on the host allow-list (the release
//!   source and GitHub's release-asset CDN). Anything else is refused.
//! * Bodies are capped ([`Fetcher::get_bytes`]) and artefacts are streamed to
//!   disk while hashed, refused as soon as they exceed the signed size
//!   ([`Fetcher::download`]).
//! * Connect / request / download timeouts.
//!
//! Only a plain `GET` with `User-Agent: HostingOS-Updater/<version>` is sent:
//! no identifiers, no telemetry.

use std::path::Path;
use std::time::Duration;

use hos_core::net::OutboundPolicy;
use reqwest::header::{ACCEPT, LOCATION};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use url::Url;

use crate::error::{UpdateError, UpdateResult};
use crate::manifest::{Artifact, MAX_ARTIFACT_BYTES};

/// GitHub hosts a release download may touch: the API, the release
/// download URL and the asset CDN it redirects to.
pub const GITHUB_HOSTS: &[&str] = &[
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Network settings of the updater.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Address policy (the process policy: `private` or `public`).
    pub policy: OutboundPolicy,
    /// Refuse plain `http://` (always on outside tests).
    pub https_only: bool,
    /// Hosts the updater may contact (lower case, exact match).
    pub allowed_hosts: Vec<String>,
    /// TCP + TLS connect timeout.
    pub connect_timeout: Duration,
    /// Whole-request timeout of API / manifest / signature requests.
    pub request_timeout: Duration,
    /// Whole-request timeout of an artefact download.
    pub download_timeout: Duration,
    /// Redirects followed per request.
    pub max_redirects: usize,
}

impl NetConfig {
    /// Production settings: the process outbound policy, HTTPS only.
    pub fn new(allowed_hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            policy: OutboundPolicy::global(),
            https_only: true,
            allowed_hosts: normalize_hosts(allowed_hosts),
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            download_timeout: Duration::from_secs(600),
            max_redirects: 5,
        }
    }

    /// Tests only: plain HTTP to `127.0.0.1` (wiremock), no address policy.
    #[doc(hidden)]
    pub fn insecure_for_tests(allowed_hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            policy: OutboundPolicy::Unrestricted,
            https_only: false,
            ..Self::new(allowed_hosts)
        }
    }

    /// Adds hosts to the allow-list.
    pub fn allow(&mut self, hosts: impl IntoIterator<Item = String>) {
        for h in normalize_hosts(hosts) {
            if !self.allowed_hosts.contains(&h) {
                self.allowed_hosts.push(h);
            }
        }
    }
}

fn normalize_hosts(hosts: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for h in hosts {
        let h = h.trim().trim_end_matches('.').to_ascii_lowercase();
        if !h.is_empty() && !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

/// URL without query and fragment, for logs and messages.
pub fn redact(url: &Url) -> String {
    let mut u = url.clone();
    u.set_query(None);
    u.set_fragment(None);
    let _ = u.set_password(None);
    let _ = u.set_username("");
    u.to_string()
}

/// The download client.
#[derive(Debug, Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    cfg: NetConfig,
}

impl Fetcher {
    /// Builds the client.
    pub fn new(cfg: NetConfig) -> UpdateResult<Self> {
        let client = cfg
            .policy
            .client_builder()
            .user_agent(crate::build_info::user_agent())
            .connect_timeout(cfg.connect_timeout)
            .https_only(cfg.https_only)
            .build()
            .map_err(|e| UpdateError::Network(format!("building the HTTP client: {e}")))?;
        Ok(Self { client, cfg })
    }

    /// The settings.
    pub fn config(&self) -> &NetConfig {
        &self.cfg
    }

    /// Scheme + address policy (no host allow-list): webhooks.
    pub fn check_endpoint(&self, url: &Url) -> UpdateResult<()> {
        match url.scheme() {
            "https" => {}
            "http" if !self.cfg.https_only => {}
            other => {
                return Err(UpdateError::Refused(format!(
                    "{} is not HTTPS ({other})",
                    redact(url)
                )));
            }
        }
        self.cfg
            .policy
            .check_url(url)
            .map_err(|e| UpdateError::Refused(format!("{}: {e}", redact(url))))
    }

    /// [`check_endpoint`](Self::check_endpoint) plus the host allow-list.
    pub fn check_url(&self, url: &Url) -> UpdateResult<()> {
        self.check_endpoint(url)?;
        let host = url
            .host_str()
            .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
            .unwrap_or_default();
        if !self.cfg.allowed_hosts.contains(&host) {
            return Err(UpdateError::Refused(format!(
                "host {host:?} is not an allowed update host"
            )));
        }
        Ok(())
    }

    fn map_err(&self, url: &Url, e: &reqwest::Error) -> UpdateError {
        if let Some(refused) = hos_core::net::outbound_error_in(e) {
            return UpdateError::Refused(format!("{}: {refused}", redact(url)));
        }
        if e.is_timeout() {
            return UpdateError::Network(format!("{}: timed out", redact(url)));
        }
        UpdateError::Network(format!("{}: {}", redact(url), e))
    }

    /// `GET`, following allowed redirects; returns the final successful
    /// response.
    async fn get(
        &self,
        start: &Url,
        accept: &str,
        timeout: Duration,
    ) -> UpdateResult<reqwest::Response> {
        let mut url = start.clone();
        let mut hops = 0usize;
        loop {
            self.check_url(&url)?;
            let resp = self
                .client
                .get(url.clone())
                .header(ACCEPT, accept)
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| self.map_err(&url, &e))?;
            let status = resp.status();
            if status.is_redirection() {
                if hops >= self.cfg.max_redirects {
                    return Err(UpdateError::Refused(format!(
                        "{}: too many redirects",
                        redact(start)
                    )));
                }
                let location = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        UpdateError::Refused(format!("{}: redirect without Location", redact(&url)))
                    })?;
                let next = url.join(location).map_err(|_| {
                    UpdateError::Refused(format!("{}: invalid redirect target", redact(&url)))
                })?;
                tracing::debug!(from = %redact(&url), to = %redact(&next), "update download redirected");
                url = next;
                hops += 1;
                continue;
            }
            if !status.is_success() {
                return Err(UpdateError::Network(format!(
                    "{} answered HTTP {}",
                    redact(&url),
                    status.as_u16()
                )));
            }
            return Ok(resp);
        }
    }

    /// Downloads a small document (API listing, manifest, signature) into
    /// memory, refusing more than `limit` bytes.
    pub async fn get_bytes(&self, url: &Url, accept: &str, limit: usize) -> UpdateResult<Vec<u8>> {
        let resp = self.get(url, accept, self.cfg.request_timeout).await?;
        hos_core::net::read_body_limited(resp, limit)
            .await
            .map_err(|e| match e {
                hos_core::net::BodyError::TooLarge { limit } => UpdateError::TooLarge {
                    limit: limit as u64,
                },
                hos_core::net::BodyError::Transport(t) => self.map_err(url, &t),
            })
    }

    /// [`get_bytes`](Self::get_bytes) as UTF-8 text.
    pub async fn get_text(&self, url: &Url, accept: &str, limit: usize) -> UpdateResult<String> {
        let bytes = self.get_bytes(url, accept, limit).await?;
        String::from_utf8(bytes)
            .map_err(|_| UpdateError::Network(format!("{}: response is not UTF-8", redact(url))))
    }

    /// Streams `artifact` to `dest` (via `dest.part`), checking the signed
    /// size and SHA-256. Nothing is left behind on failure.
    pub async fn download(&self, artifact: &Artifact, dest: &Path) -> UpdateResult<()> {
        let url = Url::parse(&artifact.url)
            .map_err(|_| UpdateError::Manifest(format!("invalid URL for {}", artifact.name)))?;
        if artifact.size > MAX_ARTIFACT_BYTES {
            return Err(UpdateError::TooLarge {
                limit: MAX_ARTIFACT_BYTES,
            });
        }
        let part = dest.with_extension("part");
        let result = self.download_inner(&url, artifact, &part).await;
        match result {
            Ok(()) => {
                tokio::fs::rename(&part, dest).await?;
                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                Err(e)
            }
        }
    }

    async fn download_inner(
        &self,
        url: &Url,
        artifact: &Artifact,
        part: &Path,
    ) -> UpdateResult<()> {
        let mut resp = self
            .get(url, "application/octet-stream", self.cfg.download_timeout)
            .await?;
        if resp.content_length().is_some_and(|len| len > artifact.size) {
            return Err(UpdateError::TooLarge {
                limit: artifact.size,
            });
        }
        let mut file = tokio::fs::File::create(part).await?;
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| self.map_err(url, &e))? {
            written = written.saturating_add(chunk.len() as u64);
            if written > artifact.size {
                return Err(UpdateError::TooLarge {
                    limit: artifact.size,
                });
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        if written != artifact.size {
            return Err(UpdateError::Checksum(format!(
                "{}: {written} bytes received, {} expected",
                artifact.name, artifact.size
            )));
        }
        let digest = hex::encode(hasher.finalize());
        if !digest.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(UpdateError::Checksum(format!(
                "{}: SHA-256 {digest} does not match the signed {}",
                artifact.name, artifact.sha256
            )));
        }
        Ok(())
    }

    /// The raw client (webhooks; no redirects are followed).
    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Maps a transport error of a request to `url`.
    pub(crate) fn transport_error(&self, url: &Url, e: &reqwest::Error) -> UpdateError {
        self.map_err(url, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_and_scheme() {
        let f = Fetcher::new(NetConfig::new(GITHUB_HOSTS.iter().map(|s| (*s).to_owned())))
            .expect("client");
        let ok = Url::parse("https://github.com/o/r/releases/download/v1/x").unwrap();
        assert!(f.check_url(&ok).is_ok());
        let http = Url::parse("http://github.com/x").unwrap();
        assert!(matches!(f.check_url(&http), Err(UpdateError::Refused(_))));
        let foreign = Url::parse("https://evil.example/x").unwrap();
        assert!(matches!(
            f.check_url(&foreign),
            Err(UpdateError::Refused(_))
        ));
        let loopback = Url::parse("https://127.0.0.1/x").unwrap();
        assert!(
            f.check_endpoint(&loopback).is_err(),
            "policy refuses loopback"
        );
    }

    #[test]
    fn redaction() {
        let u = Url::parse("https://user:pw@h.example/p?token=1#x").unwrap();
        assert_eq!(redact(&u), "https://h.example/p");
    }
}
