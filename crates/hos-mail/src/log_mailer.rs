//! Development mailer: writes emails to `tracing` instead of sending them.
//!
//! Bodies carry secrets (password-reset and verification links, sign-in
//! tokens, invoice links), so only the envelope is logged unless the
//! operator opts in with `HOS_DEBUG_MAIL=1` ([`DEBUG_MAIL_ENV`]).

use hos_core::async_trait;
use hos_core::error::IntegrationError;
use hos_core::integrations::ConnectionInfo;
use hos_core::integrations::mailer::{EmailMessage, Mailbox, Mailer, SendReceipt};

use crate::common;
use crate::kinds;

/// Environment variable that lets [`LogMailer`] write message bodies to
/// the log (`1` / `true`; development only).
pub const DEBUG_MAIL_ENV: &str = "HOS_DEBUG_MAIL";

/// Whether `HOS_DEBUG_MAIL` asks for message bodies in the log.
pub fn debug_mail_enabled() -> bool {
    std::env::var(DEBUG_MAIL_ENV)
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Logs every message (sender, recipients, subject, sizes at `info`; the
/// text body at `debug` only with [`DEBUG_MAIL_ENV`]) and reports success.
/// Never use it in production: nothing is sent.
#[derive(Debug, Clone)]
pub struct LogMailer {
    default_from: Option<Mailbox>,
    log_bodies: bool,
}

impl Default for LogMailer {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LogMailer {
    /// Creates the mailer with an optional default sender; bodies are logged
    /// only when [`DEBUG_MAIL_ENV`] is set.
    pub fn new(default_from: Option<Mailbox>) -> Self {
        Self {
            default_from,
            log_bodies: debug_mail_enabled(),
        }
    }

    /// Overrides whether message bodies are logged.
    #[must_use]
    pub fn with_bodies(mut self, log_bodies: bool) -> Self {
        self.log_bodies = log_bodies;
        self
    }

    /// Whether message bodies are logged.
    pub fn logs_bodies(&self) -> bool {
        self.log_bodies
    }
}

fn join(boxes: &[Mailbox]) -> String {
    boxes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl Mailer for LogMailer {
    fn kind(&self) -> &'static str {
        kinds::LOG
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        Ok(ConnectionInfo {
            server_version: None,
            details: Some("log mailer: emails are written to the application log".into()),
        })
    }

    async fn send(&self, message: &EmailMessage) -> Result<SendReceipt, IntegrationError> {
        let fallback = Mailbox::new("noreply@localhost.invalid");
        let from = common::prepare(
            message,
            Some(self.default_from.as_ref().unwrap_or(&fallback)),
        )?;
        let id = format!("log-{}", uuid::Uuid::now_v7());
        tracing::info!(
            target: "hos_mail",
            message_id = %id,
            kind = kinds::LOG,
            from = %from,
            to = %join(&message.to),
            cc = %join(&message.cc),
            bcc = %join(&message.bcc),
            subject = %message.subject,
            attachments = message.attachments.len(),
            text_bytes = message.text_body.as_ref().map_or(0, String::len),
            html_bytes = message.html_body.as_ref().map_or(0, String::len),
            "email logged (not sent)"
        );
        if self.log_bodies
            && let Some(text) = &message.text_body
        {
            tracing::debug!(target: "hos_mail", message_id = %id, body = %text, "email text body");
        }
        Ok(SendReceipt {
            message_id: Some(id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logs_and_returns_id() {
        let m = LogMailer::new(None);
        let msg = EmailMessage {
            to: vec![Mailbox::new("a@example.com")],
            subject: "s".into(),
            text_body: Some("t".into()),
            ..EmailMessage::default()
        };
        let r = m.send(&msg).await.unwrap();
        assert!(r.message_id.unwrap().starts_with("log-"));
        assert!(m.test_connection().await.is_ok());
        assert_eq!(m.kind(), "log");
        let mut bad = msg.clone();
        bad.to.clear();
        assert!(m.send(&bad).await.is_err());
    }

    /// Log output captured in memory.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Bodies (reset links, tokens) stay out of the log unless opted in.
    #[tokio::test(flavor = "current_thread")]
    async fn bodies_are_logged_only_on_request() {
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let msg = EmailMessage {
            to: vec![Mailbox::new("a@example.com")],
            subject: "Reset your password".into(),
            text_body: Some("https://shop.example/auth/reset?token=SECRET-TOKEN-123".into()),
            html_body: Some(
                "<a href=\"https://shop.example/auth/reset?token=SECRET-TOKEN-123\">".into(),
            ),
            ..EmailMessage::default()
        };
        let read = || String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        // With a single scoped subscriber, tracing caches a log line's
        // interest from whichever thread reaches it first: `logs_and_returns_id`
        // running in parallel without a subscriber can cache the mailer's
        // lines as disabled. Register both lines once, then recompute their
        // interest on this thread, so the parallel test cannot race us.
        LogMailer::new(None)
            .with_bodies(true)
            .send(&msg)
            .await
            .unwrap();
        tracing::callsite::rebuild_interest_cache();
        logs.0.lock().unwrap().clear();
        LogMailer::new(None)
            .with_bodies(false)
            .send(&msg)
            .await
            .unwrap();
        let out = read();
        assert!(out.contains("email logged"), "{out}");
        assert!(!out.contains("SECRET-TOKEN-123"), "{out}");
        LogMailer::new(None)
            .with_bodies(true)
            .send(&msg)
            .await
            .unwrap();
        assert!(read().contains("SECRET-TOKEN-123"));
        // Off unless HOS_DEBUG_MAIL is set (the test environment does not).
        if std::env::var(DEBUG_MAIL_ENV).is_err() {
            assert!(!LogMailer::default().logs_bodies());
        }
    }
}
