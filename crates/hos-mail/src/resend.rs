//! [Resend](https://resend.com/docs/api-reference/emails/send-email) HTTP API mailer.
//!
//! `POST https://api.resend.com/emails` with `Authorization: Bearer <api key>`;
//! the response is `{"id": "…"}`. Errors are `{"statusCode", "name", "message"}`.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hos_core::async_trait;
use hos_core::error::IntegrationError;
use hos_core::integrations::mailer::{EmailMessage, Mailbox, Mailer, SendReceipt};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use serde_json::{Map, Value, json};
use url::Url;

use crate::common::{self, error_from_response, json_body};
use crate::kinds;

/// Default API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.resend.com";

/// Resend mailer. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct ResendMailer {
    client: reqwest::Client,
    policy: OutboundPolicy,
    timeout: Duration,
    base_url: Url,
    api_key: SecretString,
    default_from: Mailbox,
}

impl ResendMailer {
    /// Creates a mailer with the given API key, default sender and timeout.
    pub fn new(
        api_key: SecretString,
        default_from: Mailbox,
        timeout: Duration,
    ) -> Result<Self, IntegrationError> {
        Ok(Self {
            client: common::http_client(timeout, OutboundPolicy::global())?,
            policy: OutboundPolicy::global(),
            timeout,
            base_url: Url::parse(DEFAULT_BASE_URL)
                .map_err(|e| IntegrationError::Config(e.to_string()))?,
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

    /// The JSON payload for `message` (public for inspection in tests).
    pub fn payload(&self, message: &EmailMessage) -> Result<Value, IntegrationError> {
        let from = common::prepare(message, Some(&self.default_from))?;
        let list = |boxes: &[Mailbox]| boxes.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut body = Map::new();
        body.insert("from".into(), json!(from.to_string()));
        body.insert("to".into(), json!(list(&message.to)));
        if !message.cc.is_empty() {
            body.insert("cc".into(), json!(list(&message.cc)));
        }
        if !message.bcc.is_empty() {
            body.insert("bcc".into(), json!(list(&message.bcc)));
        }
        if let Some(reply_to) = &message.reply_to {
            body.insert("reply_to".into(), json!([reply_to.to_string()]));
        }
        body.insert("subject".into(), json!(message.subject));
        if let Some(html) = &message.html_body {
            body.insert("html".into(), json!(html));
        }
        if let Some(text) = &message.text_body {
            body.insert("text".into(), json!(text));
        }
        if !message.headers.is_empty() {
            let headers: Map<String, Value> = message
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            body.insert("headers".into(), Value::Object(headers));
        }
        if !message.attachments.is_empty() {
            let atts: Vec<Value> = message
                .attachments
                .iter()
                .map(|a| {
                    json!({
                        "filename": a.filename,
                        "content": STANDARD.encode(&a.data),
                        "content_type": a.content_type,
                    })
                })
                .collect();
            body.insert("attachments".into(), json!(atts));
        }
        let tags: Vec<Value> = message
            .tags
            .iter()
            .map(|t| sanitize_tag(t))
            .filter(|t| !t.is_empty())
            .map(|t| json!({"name": t, "value": "1"}))
            .collect();
        if !tags.is_empty() {
            body.insert("tags".into(), json!(tags));
        }
        Ok(Value::Object(body))
    }
}

/// Resend tag names/values may only contain ASCII letters, digits, `_` and `-`
/// (max 256 chars).
fn sanitize_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(256)
        .collect()
}

fn extract_error(v: &Value) -> (Option<String>, Option<String>) {
    (
        v.get("name").and_then(Value::as_str).map(str::to_owned),
        v.get("message").and_then(Value::as_str).map(str::to_owned),
    )
}

#[async_trait]
impl Mailer for ResendMailer {
    fn kind(&self) -> &'static str {
        kinds::RESEND
    }

    async fn send(&self, message: &EmailMessage) -> Result<SendReceipt, IntegrationError> {
        let payload = self.payload(message)?;
        let request = self
            .client
            .post(self.endpoint("/emails"))
            .bearer_auth(self.api_key.expose_str())
            .json(&payload);
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
    use wiremock::matchers::{bearer_token, body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mailer(server: &MockServer) -> ResendMailer {
        ResendMailer::new(
            "re_test_key".into(),
            Mailbox::named("Acme", "noreply@acme.example"),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap())
        .with_outbound_policy(OutboundPolicy::Unrestricted)
        .unwrap()
    }

    fn message() -> EmailMessage {
        EmailMessage {
            to: vec![Mailbox::named("Ayşe", "ayse@example.com")],
            cc: vec![Mailbox::new("cc@example.com")],
            reply_to: Some(Mailbox::new("support@acme.example")),
            subject: "Faturanız hazır".into(),
            html_body: Some("<p>Merhaba</p>".into()),
            text_body: Some("Merhaba".into()),
            headers: vec![("X-HostingOS-Template".into(), "invoice_created".into())],
            tags: vec!["invoice created".into()],
            attachments: vec![Attachment {
                filename: "f.pdf".into(),
                content_type: "application/pdf".into(),
                data: vec![1, 2, 3],
            }],
            ..EmailMessage::default()
        }
    }

    #[tokio::test]
    async fn sends_expected_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .and(bearer_token("re_test_key"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "from": "\"Acme\" <noreply@acme.example>",
                "to": ["\"Ayşe\" <ayse@example.com>"],
                "cc": ["cc@example.com"],
                "reply_to": ["support@acme.example"],
                "subject": "Faturanız hazır",
                "html": "<p>Merhaba</p>",
                "text": "Merhaba",
                "headers": {"X-HostingOS-Template": "invoice_created"},
                "attachments": [{"filename": "f.pdf", "content": "AQID"}],
                "tags": [{"name": "invoice_created", "value": "1"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "re_123"})))
            .expect(1)
            .mount(&server)
            .await;
        let receipt = mailer(&server).send(&message()).await.unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("re_123"));
    }

    #[tokio::test]
    async fn maps_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "statusCode": 422, "name": "validation_error", "message": "Invalid `to` field."
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "statusCode": 401, "name": "missing_api_key", "message": "Missing API key"
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "2"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let m = mailer(&server);
        let msg = message();
        assert_eq!(
            m.send(&msg).await.unwrap_err(),
            IntegrationError::InvalidInput("[validation_error] Invalid `to` field.".into())
        );
        let auth = m.send(&msg).await.unwrap_err();
        assert_eq!(auth.kind(), "auth");
        assert!(!auth.to_string().contains("re_test_key"));
        assert_eq!(
            m.send(&msg).await.unwrap_err(),
            IntegrationError::RateLimited {
                retry_after: Some(Duration::from_secs(2))
            }
        );
        let err = m.send(&msg).await.unwrap_err();
        // A non-JSON body is never quoted.
        assert_eq!(err, IntegrationError::remote(Some(500), "HTTP 500"));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn outbound_policy_and_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/elsewhere", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/elsewhere"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
            .expect(0)
            .mount(&server)
            .await;
        // Redirects are not followed.
        assert!(mailer(&server).send(&message()).await.is_err());

        // Under `private`, a loopback API URL is refused before connecting.
        let guarded = ResendMailer::new(
            "re_test_key".into(),
            Mailbox::named("Acme", "noreply@acme.example"),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_outbound_policy(OutboundPolicy::Private)
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());
        let err = guarded.send(&message()).await.unwrap_err();
        assert_eq!(err.kind(), "config", "{err}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invalid_message_is_not_sent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let mut msg = message();
        msg.subject = "x\nBcc: evil@example.com".into();
        assert!(matches!(
            mailer(&server).send(&msg).await,
            Err(IntegrationError::InvalidInput(_))
        ));
    }
}
