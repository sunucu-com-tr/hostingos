//! [Mailgun](https://documentation.mailgun.com/docs/mailgun/api-reference/) HTTP API mailer.
//!
//! `POST {base}/v3/{domain}/messages` (multipart form) with HTTP basic auth
//! `api:<api key>`. Base URL by region: `https://api.mailgun.net` (US) or
//! `https://api.eu.mailgun.net` (EU). Success: `{"id": "<…>", "message": "Queued. Thank you."}`.
//! `test_connection` uses `GET /v4/domains/{domain}`.

use std::time::Duration;

use hos_core::async_trait;
use hos_core::error::IntegrationError;
use hos_core::integrations::ConnectionInfo;
use hos_core::integrations::mailer::{EmailMessage, Mailbox, Mailer, SendReceipt};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use url::Url;

use crate::common::{self, error_from_response, json_body};
use crate::kinds;

/// US region API base URL.
pub const US_BASE_URL: &str = "https://api.mailgun.net";
/// EU region API base URL.
pub const EU_BASE_URL: &str = "https://api.eu.mailgun.net";

/// Mailgun account region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MailgunRegion {
    /// `api.mailgun.net`.
    #[default]
    Us,
    /// `api.eu.mailgun.net`.
    Eu,
}

impl MailgunRegion {
    /// API base URL of the region.
    pub const fn base_url(self) -> &'static str {
        match self {
            Self::Us => US_BASE_URL,
            Self::Eu => EU_BASE_URL,
        }
    }
}

/// Mailgun mailer. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct MailgunMailer {
    client: reqwest::Client,
    policy: OutboundPolicy,
    timeout: Duration,
    base_url: Url,
    domain: String,
    api_key: SecretString,
    default_from: Mailbox,
}

fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.contains('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

impl MailgunMailer {
    /// Creates a mailer for a sending `domain`.
    pub fn new(
        api_key: SecretString,
        domain: &str,
        region: MailgunRegion,
        default_from: Mailbox,
        timeout: Duration,
    ) -> Result<Self, IntegrationError> {
        let domain = domain.trim().to_ascii_lowercase();
        if !valid_domain(&domain) {
            return Err(IntegrationError::Config(
                "config.domain must be a domain name".into(),
            ));
        }
        Ok(Self {
            client: common::http_client(timeout, OutboundPolicy::global())?,
            policy: OutboundPolicy::global(),
            timeout,
            base_url: Url::parse(region.base_url())
                .map_err(|e| IntegrationError::Config(e.to_string()))?,
            domain,
            api_key,
            default_from,
        })
    }

    /// Overrides the API base URL (tests / proxies).
    #[must_use]
    pub fn with_base_url(mut self, base_url: Url) -> Self {
        self.base_url = base_url;
        self
    }

    /// Rebuilds the HTTP client under an explicit outbound policy (the
    /// default is the process-wide `HOS_OUTBOUND_POLICY`; tests against a
    /// local mock server use `OutboundPolicy::Unrestricted`).
    pub fn with_outbound_policy(
        mut self,
        policy: OutboundPolicy,
    ) -> Result<Self, IntegrationError> {
        self.client = common::http_client(self.timeout, policy)?;
        self.policy = policy;
        Ok(self)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.as_str().trim_end_matches('/'))
    }

    /// Text fields of the multipart form, in order (without attachments).
    pub fn form_fields(
        &self,
        message: &EmailMessage,
    ) -> Result<Vec<(String, String)>, IntegrationError> {
        let from = common::prepare(message, Some(&self.default_from))?;
        let mut fields = vec![("from".to_owned(), from.to_string())];
        let mut push_all = |name: &str, boxes: &[Mailbox]| {
            for b in boxes {
                fields.push((name.to_owned(), b.to_string()));
            }
        };
        push_all("to", &message.to);
        push_all("cc", &message.cc);
        push_all("bcc", &message.bcc);
        fields.push(("subject".into(), message.subject.clone()));
        if let Some(text) = &message.text_body {
            fields.push(("text".into(), text.clone()));
        }
        if let Some(html) = &message.html_body {
            fields.push(("html".into(), html.clone()));
        }
        if let Some(reply_to) = &message.reply_to {
            fields.push(("h:Reply-To".into(), reply_to.to_string()));
        }
        for (k, v) in &message.headers {
            fields.push((format!("h:{k}"), v.clone()));
        }
        for tag in message.tags.iter().take(10) {
            fields.push(("o:tag".into(), tag.clone()));
        }
        Ok(fields)
    }
}

fn extract_error(v: &Value) -> (Option<String>, Option<String>) {
    (
        None,
        v.get("message").and_then(Value::as_str).map(str::to_owned),
    )
}

#[async_trait]
impl Mailer for MailgunMailer {
    fn kind(&self) -> &'static str {
        kinds::MAILGUN
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let request = self
            .client
            .get(self.endpoint(&format!("/v4/domains/{}", self.domain)))
            .basic_auth("api", Some(self.api_key.expose_str()));
        let resp = common::send(&self.client, self.policy, request).await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp, extract_error).await);
        }
        let body = json_body(resp).await?;
        let state = body
            .pointer("/domain/state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        Ok(ConnectionInfo {
            server_version: None,
            details: Some(format!("Mailgun domain {} ({state})", self.domain)),
        })
    }

    async fn send(&self, message: &EmailMessage) -> Result<SendReceipt, IntegrationError> {
        let mut form = Form::new();
        for (k, v) in self.form_fields(message)? {
            form = form.text(k, v);
        }
        for a in &message.attachments {
            let part = Part::bytes(a.data.clone())
                .file_name(a.filename.clone())
                .mime_str(&a.content_type)
                .map_err(|_| {
                    IntegrationError::InvalidInput(format!(
                        "attachment {} has an invalid content type",
                        a.filename
                    ))
                })?;
            form = form.part("attachment", part);
        }
        let request = self
            .client
            .post(self.endpoint(&format!("/v3/{}/messages", self.domain)))
            .basic_auth("api", Some(self.api_key.expose_str()))
            .multipart(form);
        let resp = common::send(&self.client, self.policy, request).await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp, extract_error).await);
        }
        let body = json_body(resp).await?;
        Ok(SendReceipt {
            message_id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::integrations::mailer::Attachment;
    use serde_json::json;
    use wiremock::matchers::{basic_auth, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mailer(server: &MockServer) -> MailgunMailer {
        MailgunMailer::new(
            "key-123".into(),
            "mg.acme.example",
            MailgunRegion::Eu,
            Mailbox::named("Acme", "noreply@mg.acme.example"),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap())
        .with_outbound_policy(OutboundPolicy::Unrestricted)
        .unwrap()
    }

    fn message() -> EmailMessage {
        EmailMessage {
            to: vec![Mailbox::new("a@example.com"), Mailbox::new("b@example.com")],
            reply_to: Some(Mailbox::new("support@acme.example")),
            subject: "Merhaba".into(),
            text_body: Some("Selam".into()),
            html_body: Some("<p>Selam</p>".into()),
            headers: vec![("X-HostingOS-Template".into(), "welcome".into())],
            tags: vec!["welcome".into()],
            attachments: vec![Attachment {
                filename: "a.txt".into(),
                content_type: "text/plain".into(),
                data: b"attached-bytes".to_vec(),
            }],
            ..EmailMessage::default()
        }
    }

    #[tokio::test]
    async fn sends_multipart_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/mg.acme.example/messages"))
            .and(basic_auth("api", "key-123"))
            .and(header_regex(
                "content-type",
                "^multipart/form-data; boundary=",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "<20261116.1@mg.acme.example>", "message": "Queued. Thank you."
            })))
            .expect(1)
            .mount(&server)
            .await;
        let r = mailer(&server).send(&message()).await.unwrap();
        assert_eq!(
            r.message_id.as_deref(),
            Some("<20261116.1@mg.acme.example>")
        );

        let req = &server.received_requests().await.unwrap()[0];
        let body = String::from_utf8_lossy(&req.body);
        for needle in [
            "name=\"from\"\r\n\r\n\"Acme\" <noreply@mg.acme.example>",
            "name=\"to\"\r\n\r\na@example.com",
            "name=\"to\"\r\n\r\nb@example.com",
            "name=\"subject\"\r\n\r\nMerhaba",
            "name=\"text\"\r\n\r\nSelam",
            "name=\"html\"\r\n\r\n<p>Selam</p>",
            "name=\"h:Reply-To\"\r\n\r\nsupport@acme.example",
            "name=\"h:X-HostingOS-Template\"\r\n\r\nwelcome",
            "name=\"o:tag\"\r\n\r\nwelcome",
            "name=\"attachment\"; filename=\"a.txt\"",
            "attached-bytes",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in {body}");
        }
    }

    #[tokio::test]
    async fn maps_errors_and_tests_connection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Forbidden"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"message": "to parameter is not a valid address"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v4/domains/mg.acme.example"))
            .and(basic_auth("api", "key-123"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"domain": {"name": "mg.acme.example", "state": "active"}}),
                ),
            )
            .mount(&server)
            .await;
        let m = mailer(&server);
        let msg = message();
        // Plain-text bodies are never quoted: only the status is reported.
        assert_eq!(
            m.send(&msg).await.unwrap_err(),
            IntegrationError::Auth("HTTP 401".into())
        );
        assert_eq!(
            m.send(&msg).await.unwrap_err(),
            IntegrationError::InvalidInput("to parameter is not a valid address".into())
        );
        assert!(m.send(&msg).await.unwrap_err().is_retryable());
        let info = m.test_connection().await.unwrap();
        assert_eq!(
            info.details.as_deref(),
            Some("Mailgun domain mg.acme.example (active)")
        );
    }

    #[test]
    fn region_and_domain_validation() {
        assert_eq!(MailgunRegion::Eu.base_url(), "https://api.eu.mailgun.net");
        let err = MailgunMailer::new(
            "k".into(),
            "../evil",
            MailgunRegion::Us,
            Mailbox::new("a@b.co"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(err, IntegrationError::Config(_)));
    }
}
