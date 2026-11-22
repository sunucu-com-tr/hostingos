//! Outgoing email (SMTP or HTTP email APIs).
//!
//! [`EmailMessage`] is fully serializable so `hos-jobs` can queue it as a job
//! payload; attachments are base64 in JSON. Rendering (templates, i18n) happens
//! before the message reaches a [`Mailer`].

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::ConnectionInfo;
use crate::error::IntegrationError;

/// An address with an optional display name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    /// Display name.
    pub name: Option<String>,
    /// Address (`user@example.com`).
    pub email: String,
}

impl Mailbox {
    /// Address without display name.
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }

    /// Address with display name.
    pub fn named(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            email: email.into(),
        }
    }
}

/// RFC 5322 form: `"Name" <email>` (quoted, with `"`/`\` escaped) or bare `email`.
impl fmt::Display for Mailbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name.as_deref().filter(|n| !n.trim().is_empty()) {
            Some(name) => {
                let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
                write!(f, "\"{escaped}\" <{}>", self.email)
            }
            None => f.write_str(&self.email),
        }
    }
}

/// A file attached to an email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// File name (`invoice-2026-0042.pdf`).
    pub filename: String,
    /// MIME type (`application/pdf`).
    pub content_type: String,
    /// File contents (base64 in JSON).
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

/// A rendered email ready to send.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailMessage {
    /// Sender; `None` = the mailer's configured default (tenant sender).
    pub from: Option<Mailbox>,
    /// Reply-To.
    pub reply_to: Option<Mailbox>,
    /// Recipients (at least one of `to`/`cc`/`bcc`).
    pub to: Vec<Mailbox>,
    /// Carbon copy.
    pub cc: Vec<Mailbox>,
    /// Blind carbon copy.
    pub bcc: Vec<Mailbox>,
    /// Subject.
    pub subject: String,
    /// Plain-text body.
    pub text_body: Option<String>,
    /// HTML body.
    pub html_body: Option<String>,
    /// Attachments.
    pub attachments: Vec<Attachment>,
    /// Extra headers (`List-Unsubscribe`, `X-HostingOS-Template`...).
    pub headers: Vec<(String, String)>,
    /// Provider tags / categories for analytics.
    pub tags: Vec<String>,
}

impl EmailMessage {
    /// Checks the message is sendable: a recipient, a subject, a body, and no
    /// CR/LF in subject or header values (header injection).
    pub fn validate(&self) -> Result<(), IntegrationError> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err(IntegrationError::InvalidInput(
                "email has no recipients".into(),
            ));
        }
        if self.text_body.is_none() && self.html_body.is_none() {
            return Err(IntegrationError::InvalidInput("email has no body".into()));
        }
        let has_newline = |s: &str| s.contains(['\r', '\n']);
        if has_newline(&self.subject)
            || self
                .headers
                .iter()
                .any(|(k, v)| has_newline(k) || has_newline(v))
        {
            return Err(IntegrationError::InvalidInput(
                "email subject/headers must not contain line breaks".into(),
            ));
        }
        Ok(())
    }
}

/// Result of a successful send.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendReceipt {
    /// Provider / SMTP message id, for `email_log`.
    pub message_id: Option<String>,
}

/// An outgoing email transport. See the [module docs](self).
#[async_trait]
pub trait Mailer: Send + Sync {
    /// The integration kind (e.g. [`kinds::SMTP`](super::kinds::SMTP)).
    fn kind(&self) -> &'static str;

    /// Verifies connectivity / credentials (SMTP `NOOP` after auth). Default: `Unsupported`.
    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        Err(IntegrationError::Unsupported("test_connection"))
    }

    /// Sends one message. Implementations should call [`EmailMessage::validate`].
    async fn send(&self, message: &EmailMessage) -> Result<SendReceipt, IntegrationError>;
}

mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_display() {
        assert_eq!(Mailbox::new("a@b.c").to_string(), "a@b.c");
        assert_eq!(
            Mailbox::named("Acme", "a@b.c").to_string(),
            "\"Acme\" <a@b.c>"
        );
        assert_eq!(
            Mailbox::named("Say \"hi\"", "a@b.c").to_string(),
            "\"Say \\\"hi\\\"\" <a@b.c>"
        );
    }

    #[test]
    fn message_serde_and_validate() {
        let msg = EmailMessage {
            to: vec![Mailbox::new("c@example.com")],
            subject: "Invoice".into(),
            text_body: Some("hi".into()),
            attachments: vec![Attachment {
                filename: "a.pdf".into(),
                content_type: "application/pdf".into(),
                data: vec![0, 1, 2, 255],
            }],
            ..EmailMessage::default()
        };
        msg.validate().unwrap();
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["attachments"][0]["data"], "AAEC/w==");
        let back: EmailMessage = serde_json::from_value(json).unwrap();
        assert_eq!(back, msg);

        let mut bad = msg.clone();
        bad.subject = "x\r\nBcc: evil@example.com".into();
        assert!(bad.validate().is_err());
        assert!(EmailMessage::default().validate().is_err());
    }
}
