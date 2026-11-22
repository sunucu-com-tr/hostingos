//! [Postmark](https://postmarkapp.com/developer/api/email-api) HTTP API mailer.
//!
//! `POST https://api.postmarkapp.com/email` with `X-Postmark-Server-Token`.
//! Success: `{"MessageID": "…", "ErrorCode": 0, "Message": "OK"}`; failures
//! carry a non-zero `ErrorCode` (usually with HTTP 422). `test_connection` uses
//! `GET /server`.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hos_core::async_trait;
use hos_core::error::IntegrationError;
use hos_core::integrations::ConnectionInfo;
use hos_core::integrations::mailer::{EmailMessage, Mailbox, Mailer, SendReceipt};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use serde_json::{Map, Value, json};
use url::Url;

use crate::common::{self, error_from_response, json_body};
use crate::kinds;

/// Default API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.postmarkapp.com";
/// Default message stream for transactional email.
pub const DEFAULT_MESSAGE_STREAM: &str = "outbound";
const TOKEN_HEADER: &str = "X-Postmark-Server-Token";

/// Postmark mailer. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct PostmarkMailer {
    client: reqwest::Client,
    policy: OutboundPolicy,
    timeout: Duration,
    base_url: Url,
    server_token: SecretString,
    default_from: Mailbox,
    message_stream: String,
}

impl PostmarkMailer {
    /// Creates a mailer with a server token, default sender, message stream and timeout.
    pub fn new(
        server_token: SecretString,
        default_from: Mailbox,
        message_stream: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, IntegrationError> {
        Ok(Self {
            client: common::http_client(timeout, OutboundPolicy::global())?,
            policy: OutboundPolicy::global(),
            timeout,
            base_url: Url::parse(DEFAULT_BASE_URL)
                .map_err(|e| IntegrationError::Config(e.to_string()))?,
            server_token,
            default_from,
            message_stream: message_stream.into(),
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

    /// The JSON payload for `message`.
    pub fn payload(&self, message: &EmailMessage) -> Result<Value, IntegrationError> {
        let from = common::prepare(message, Some(&self.default_from))?;
        let join = |boxes: &[Mailbox]| {
            boxes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut body = Map::new();
        body.insert("From".into(), json!(from.to_string()));
        body.insert("To".into(), json!(join(&message.to)));
        if !message.cc.is_empty() {
            body.insert("Cc".into(), json!(join(&message.cc)));
        }
        if !message.bcc.is_empty() {
            body.insert("Bcc".into(), json!(join(&message.bcc)));
        }
        if let Some(reply_to) = &message.reply_to {
            body.insert("ReplyTo".into(), json!(reply_to.to_string()));
        }
        body.insert("Subject".into(), json!(message.subject));
        if let Some(html) = &message.html_body {
            body.insert("HtmlBody".into(), json!(html));
        }
        if let Some(text) = &message.text_body {
            body.insert("TextBody".into(), json!(text));
        }
        if !message.headers.is_empty() {
            let headers: Vec<Value> = message
                .headers
                .iter()
                .map(|(k, v)| json!({"Name": k, "Value": v}))
                .collect();
            body.insert("Headers".into(), json!(headers));
        }
        if let Some(tag) = message.tags.first() {
            body.insert(
                "Tag".into(),
                json!(tag.chars().take(1000).collect::<String>()),
            );
        }
        if !message.attachments.is_empty() {
            let atts: Vec<Value> = message
                .attachments
                .iter()
                .map(|a| {
                    json!({
                        "Name": a.filename,
                        "Content": STANDARD.encode(&a.data),
                        "ContentType": a.content_type,
                    })
                })
                .collect();
            body.insert("Attachments".into(), json!(atts));
        }
        body.insert("MessageStream".into(), json!(self.message_stream));
        Ok(Value::Object(body))
    }
}

fn extract_error(v: &Value) -> (Option<String>, Option<String>) {
    (
        v.get("ErrorCode")
            .and_then(Value::as_i64)
            .map(|c| c.to_string()),
        v.get("Message").and_then(Value::as_str).map(str::to_owned),
    )
}

#[async_trait]
impl Mailer for PostmarkMailer {
    fn kind(&self) -> &'static str {
        kinds::POSTMARK
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let request = self
            .client
            .get(self.endpoint("/server"))
            .header(TOKEN_HEADER, self.server_token.expose_str())
            .header("Accept", "application/json");
        let resp = common::send(&self.client, self.policy, request).await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp, extract_error).await);
        }
        let body = json_body(resp).await?;
        Ok(ConnectionInfo {
            server_version: None,
            details: body
                .get("Name")
                .and_then(Value::as_str)
                .map(|n| format!("Postmark server: {n}")),
        })
    }

    async fn send(&self, message: &EmailMessage) -> Result<SendReceipt, IntegrationError> {
        let payload = self.payload(message)?;
        let request = self
            .client
            .post(self.endpoint("/email"))
            .header(TOKEN_HEADER, self.server_token.expose_str())
            .header("Accept", "application/json")
            .json(&payload);
        let resp = common::send(&self.client, self.policy, request).await?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp, extract_error).await);
        }
        let body = json_body(resp).await?;
        match body.get("ErrorCode").and_then(Value::as_i64) {
            None | Some(0) => Ok(SendReceipt {
                message_id: body
                    .get("MessageID")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            Some(code) => Err(IntegrationError::remote_with_code(
                None,
                code.to_string(),
                body.get("Message")
                    .and_then(Value::as_str)
                    .unwrap_or("Postmark rejected the message"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mailer(server: &MockServer) -> PostmarkMailer {
        PostmarkMailer::new(
            "pm-token".into(),
            Mailbox::named("Acme", "noreply@acme.example"),
            "outbound",
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
            bcc: vec![Mailbox::new("audit@acme.example")],
            subject: "Hello".into(),
            html_body: Some("<p>Hi</p>".into()),
            tags: vec!["welcome".into(), "ignored".into()],
            headers: vec![("X-HostingOS-Template".into(), "welcome".into())],
            ..EmailMessage::default()
        }
    }

    #[tokio::test]
    async fn sends_expected_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/email"))
            .and(header("x-postmark-server-token", "pm-token"))
            .and(header("accept", "application/json"))
            .and(body_partial_json(json!({
                "From": "\"Acme\" <noreply@acme.example>",
                "To": "a@example.com, b@example.com",
                "Bcc": "audit@acme.example",
                "Subject": "Hello",
                "HtmlBody": "<p>Hi</p>",
                "Tag": "welcome",
                "Headers": [{"Name": "X-HostingOS-Template", "Value": "welcome"}],
                "MessageStream": "outbound"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "To": "a@example.com", "SubmittedAt": "2026-11-16T10:00:00Z",
                "MessageID": "b7bc2f4a-e38e-4336-af7d-e6c392c2f817", "ErrorCode": 0, "Message": "OK"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let r = mailer(&server).send(&message()).await.unwrap();
        assert_eq!(
            r.message_id.as_deref(),
            Some("b7bc2f4a-e38e-4336-af7d-e6c392c2f817")
        );
    }

    #[tokio::test]
    async fn maps_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/email"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "ErrorCode": 300, "Message": "Invalid 'From' address."
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/email"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "ErrorCode": 10, "Message": "No Account or Server API tokens were supplied in the HTTP headers."
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/email"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ErrorCode": 406, "Message": "You tried to send to a recipient that has been marked as inactive."
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/email"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let m = mailer(&server);
        let msg = message();
        assert_eq!(
            m.send(&msg).await.unwrap_err(),
            IntegrationError::InvalidInput("[300] Invalid 'From' address.".into())
        );
        let auth = m.send(&msg).await.unwrap_err();
        assert!(matches!(auth, IntegrationError::Auth(ref s) if s.starts_with("[10]")));
        let inactive = m.send(&msg).await.unwrap_err();
        assert!(matches!(
            inactive,
            IntegrationError::Remote { code: Some(ref c), .. } if c == "406"
        ));
        assert!(!inactive.is_retryable());
        assert_eq!(
            m.send(&msg).await.unwrap_err(),
            IntegrationError::RateLimited { retry_after: None }
        );
    }

    #[tokio::test]
    async fn test_connection_reads_server_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/server"))
            .and(header("x-postmark-server-token", "pm-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ID": 1, "Name": "Production"})),
            )
            .mount(&server)
            .await;
        let info = mailer(&server).test_connection().await.unwrap();
        assert_eq!(info.details.as_deref(), Some("Postmark server: Production"));
    }
}
