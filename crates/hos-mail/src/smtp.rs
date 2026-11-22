//! SMTP mailer (lettre, async tokio + rustls).
//!
//! Security modes: `starttls` (default, port 587; STARTTLS is *required*, never
//! opportunistic), `tls` (implicit TLS, port 465) and `none` (plain text, port 25
//! — local relays only; credentials are refused in this mode so a password never
//! crosses the network unencrypted).
//!
//! Each send opens its own connection (`EHLO` → TLS → `AUTH` → `MAIL`/`RCPT`/`DATA`
//! → `QUIT`) via lettre's `AsyncSmtpConnection`. lettre's pooled
//! `AsyncSmtpTransport` is deliberately not used: it spawns Tokio tasks when it
//! is built *and* when it is dropped, so constructing it in the synchronous
//! [`crate::build`] factory (or dropping it after the runtime is gone) panics.
//! Mail is sent from background jobs, where a connection per message is fine.
//!
//! The host is tenant-configured: it is resolved by HostingOS and every
//! address is checked against the outbound policy
//! ([`hos_core::net::OutboundPolicy`], `HOS_OUTBOUND_POLICY`) before connecting;
//! the connection then goes to exactly those addresses (no second lookup that
//! DNS rebinding could redirect to loopback or a metadata service).

use std::net::SocketAddr;
use std::time::Duration;

use hos_core::async_trait;
use hos_core::error::IntegrationError;
use hos_core::integrations::ConnectionInfo;
use hos_core::integrations::mailer::{EmailMessage, Mailbox, Mailer, SendReceipt};
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment as LettreAttachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, DEFAULT_MECHANISMS};
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{Address, Message};

use crate::common;
use crate::kinds;

/// Transport security of an SMTP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SmtpSecurity {
    /// Plain connection upgraded with `STARTTLS` (required). Default port 587.
    #[default]
    StartTls,
    /// Implicit TLS from the first byte ("SMTPS"). Default port 465.
    Tls,
    /// No encryption. Default port 25. Only for local relays.
    None,
}

impl SmtpSecurity {
    /// Parses the `security` config value (`starttls` | `tls` | `none`).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "starttls" => Some(Self::StartTls),
            "tls" | "ssl" => Some(Self::Tls),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Conventional port for the mode.
    pub const fn default_port(self) -> u16 {
        match self {
            Self::StartTls => 587,
            Self::Tls => 465,
            Self::None => 25,
        }
    }
}

/// Settings for [`SmtpMailer::new`].
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    /// Server host name.
    pub host: String,
    /// Port; `None` → [`SmtpSecurity::default_port`].
    pub port: Option<u16>,
    /// Transport security.
    pub security: SmtpSecurity,
    /// Username for `AUTH` (with `password`).
    pub username: Option<String>,
    /// Password for `AUTH`.
    pub password: Option<SecretString>,
    /// Sender used when a message has no `from`.
    pub default_from: Mailbox,
    /// Connection / command timeout.
    pub timeout: Duration,
    /// Which addresses the host may resolve to (normally
    /// [`OutboundPolicy::global`]).
    pub outbound: OutboundPolicy,
}

#[derive(Clone)]
enum TlsMode {
    StartTls(TlsParameters),
    Wrapper(TlsParameters),
    None,
}

/// SMTP mailer. See the [module docs](self).
#[derive(Clone)]
pub struct SmtpMailer {
    host: String,
    port: u16,
    tls: TlsMode,
    credentials: Option<(String, SecretString)>,
    timeout: Duration,
    default_from: Mailbox,
    policy: OutboundPolicy,
}

impl std::fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let security = match self.tls {
            TlsMode::StartTls(_) => "starttls",
            TlsMode::Wrapper(_) => "tls",
            TlsMode::None => "none",
        };
        f.debug_struct("SmtpMailer")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &security)
            .field("username", &self.credentials.as_ref().map(|(u, _)| u))
            .field("default_from", &self.default_from)
            .finish_non_exhaustive()
    }
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

impl SmtpMailer {
    /// Validates the settings. No network I/O happens until a send.
    pub fn new(config: SmtpConfig) -> Result<Self, IntegrationError> {
        let host = config.host.trim().to_owned();
        if !valid_host(&host) {
            return Err(IntegrationError::Config(
                "config.host must be a host name".into(),
            ));
        }
        // Literal addresses (any notation) and `localhost` are refused here;
        // names are resolved and checked before every connection.
        config
            .outbound
            .check_host(host.trim_start_matches('[').trim_end_matches(']'))?;
        let port = config
            .port
            .unwrap_or_else(|| config.security.default_port());
        let tls_params = || {
            TlsParameters::new(host.clone())
                .map_err(|e| IntegrationError::Config(format!("TLS setup failed: {e}")))
        };
        let tls = match config.security {
            SmtpSecurity::StartTls => TlsMode::StartTls(tls_params()?),
            SmtpSecurity::Tls => TlsMode::Wrapper(tls_params()?),
            SmtpSecurity::None => TlsMode::None,
        };
        let credentials = match (config.username, config.password) {
            (Some(user), Some(pass)) if !user.trim().is_empty() => {
                if config.security == SmtpSecurity::None {
                    return Err(IntegrationError::Config(
                        "SMTP credentials require security starttls or tls".into(),
                    ));
                }
                Some((user.trim().to_owned(), pass))
            }
            (Some(user), None) if !user.trim().is_empty() => {
                return Err(IntegrationError::Config(
                    "secrets.password is required when config.username is set".into(),
                ));
            }
            _ => None,
        };
        Ok(Self {
            host,
            port,
            tls,
            credentials,
            timeout: config.timeout,
            default_from: config.default_from,
            policy: config.outbound,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Resolves the host and checks every address against the policy.
    async fn addresses(&self) -> Result<Vec<SocketAddr>, IntegrationError> {
        Ok(self.policy.resolve_host(&self.host, self.port).await?)
    }

    /// Opens an authenticated connection (EHLO, TLS, AUTH) to one of the
    /// pre-checked `addrs` (TLS still verifies the configured host name).
    async fn connect(
        &self,
        addrs: &[SocketAddr],
    ) -> Result<AsyncSmtpConnection, lettre::transport::smtp::Error> {
        let hello = ClientId::default();
        let wrapper = match &self.tls {
            TlsMode::Wrapper(params) => Some(params.clone()),
            TlsMode::StartTls(_) | TlsMode::None => None,
        };
        let mut conn =
            AsyncSmtpConnection::connect_tokio1(addrs, Some(self.timeout), &hello, wrapper, None)
                .await?;
        if let TlsMode::StartTls(params) = &self.tls {
            conn.starttls(params.clone(), &hello).await?;
        }
        if let Some((user, pass)) = &self.credentials {
            let creds = Credentials::new(user.clone(), pass.expose_str().to_owned());
            conn.auth(DEFAULT_MECHANISMS, &creds).await?;
        }
        Ok(conn)
    }

    /// Builds the MIME message and returns it with its `Message-ID`.
    pub fn build_message(
        &self,
        message: &EmailMessage,
    ) -> Result<(Message, String), IntegrationError> {
        let from = common::prepare(message, Some(&self.default_from))?;
        let domain: String = from
            .email
            .rsplit_once('@')
            .map(|(_, d)| d)
            .unwrap_or("localhost")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
            .collect::<String>()
            .to_ascii_lowercase();
        let message_id = format!("<{}@{domain}>", uuid::Uuid::now_v7().simple());

        let mut builder = Message::builder()
            .from(to_lettre(&from)?)
            .subject(message.subject.clone())
            .message_id(Some(message_id.clone()));
        if let Some(r) = &message.reply_to {
            builder = builder.reply_to(to_lettre(r)?);
        }
        for m in &message.to {
            builder = builder.to(to_lettre(m)?);
        }
        for m in &message.cc {
            builder = builder.cc(to_lettre(m)?);
        }
        for m in &message.bcc {
            builder = builder.bcc(to_lettre(m)?);
        }
        for (name, value) in &message.headers {
            let name = HeaderName::new_from_ascii(name.clone()).map_err(|_| {
                IntegrationError::InvalidInput(format!("invalid header name {name}"))
            })?;
            builder = builder.raw_header(HeaderValue::new(name, value.clone()));
        }

        enum Body {
            Single(SinglePart),
            Multi(MultiPart),
        }
        let body = match (&message.text_body, &message.html_body) {
            (Some(t), Some(h)) => {
                Body::Multi(MultiPart::alternative_plain_html(t.clone(), h.clone()))
            }
            (Some(t), None) => Body::Single(SinglePart::plain(t.clone())),
            (None, Some(h)) => Body::Single(SinglePart::html(h.clone())),
            (None, None) => {
                return Err(IntegrationError::InvalidInput("email has no body".into()));
            }
        };
        let built = if message.attachments.is_empty() {
            match body {
                Body::Single(p) => builder.singlepart(p),
                Body::Multi(m) => builder.multipart(m),
            }
        } else {
            let mut mixed = match body {
                Body::Single(p) => MultiPart::mixed().singlepart(p),
                Body::Multi(m) => MultiPart::mixed().multipart(m),
            };
            for a in &message.attachments {
                let ct = ContentType::parse(&a.content_type).map_err(|_| {
                    IntegrationError::InvalidInput(format!(
                        "attachment {} has an invalid content type",
                        a.filename
                    ))
                })?;
                mixed = mixed
                    .singlepart(LettreAttachment::new(a.filename.clone()).body(a.data.clone(), ct));
            }
            builder.multipart(mixed)
        };
        let built = built
            .map_err(|e| IntegrationError::InvalidInput(format!("cannot build email: {e}")))?;
        Ok((built, message_id))
    }
}

fn to_lettre(m: &Mailbox) -> Result<lettre::message::Mailbox, IntegrationError> {
    let address: Address = m.email.trim().parse().map_err(|_| {
        IntegrationError::InvalidInput(format!(
            "invalid email address {:?}",
            m.email.chars().take(80).collect::<String>()
        ))
    })?;
    let name = m
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);
    Ok(lettre::message::Mailbox::new(name, address))
}

/// Maps a lettre SMTP error to an [`IntegrationError`].
fn map_smtp_error(err: lettre::transport::smtp::Error) -> IntegrationError {
    if err.is_timeout() {
        return IntegrationError::Timeout;
    }
    let message = err.to_string();
    if let Some(code) = err.status() {
        let code = code.to_string();
        if err.is_permanent() {
            // 530 auth required, 534 mechanism too weak, 535 credentials invalid, 538 encryption required.
            return if matches!(code.as_str(), "530" | "534" | "535" | "538") {
                IntegrationError::Auth(message)
            } else {
                IntegrationError::remote_with_code(None, code, message)
            };
        }
        if err.is_transient() {
            return IntegrationError::Transport(format!(
                "temporary SMTP failure ({code}): {message}"
            ));
        }
    }
    if err.is_client() {
        return IntegrationError::InvalidInput(message);
    }
    IntegrationError::Transport(message)
}

#[async_trait]
impl Mailer for SmtpMailer {
    fn kind(&self) -> &'static str {
        kinds::SMTP
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let addrs = self.addresses().await?;
        let mut conn = self.connect(&addrs).await.map_err(map_smtp_error)?;
        let alive = conn.test_connected().await;
        let _ = conn.quit().await;
        if alive {
            Ok(ConnectionInfo {
                server_version: None,
                details: Some(format!("SMTP {} reachable", self.endpoint())),
            })
        } else {
            Err(IntegrationError::Transport(format!(
                "SMTP {} did not answer NOOP",
                self.endpoint()
            )))
        }
    }

    async fn send(&self, message: &EmailMessage) -> Result<SendReceipt, IntegrationError> {
        let (email, message_id) = self.build_message(message)?;
        let addrs = self.addresses().await?;
        let mut conn = self.connect(&addrs).await.map_err(map_smtp_error)?;
        let sent = conn.send(email.envelope(), &email.formatted()).await;
        match sent {
            Ok(_) => {
                // The message is accepted; a failing QUIT does not matter.
                let _ = conn.quit().await;
                Ok(SendReceipt {
                    message_id: Some(message_id),
                })
            }
            Err(e) => {
                conn.abort().await;
                Err(map_smtp_error(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::integrations::mailer::Attachment;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::oneshot;

    fn config(port: u16) -> SmtpConfig {
        SmtpConfig {
            host: "127.0.0.1".into(),
            port: Some(port),
            security: SmtpSecurity::None,
            username: None,
            password: None,
            default_from: Mailbox::named("Acme Hosting", "noreply@acme.example"),
            timeout: Duration::from_secs(5),
            outbound: OutboundPolicy::Unrestricted,
        }
    }

    fn message() -> EmailMessage {
        EmailMessage {
            to: vec![Mailbox::named("Ayşe Yılmaz", "ayse@example.com")],
            bcc: vec![Mailbox::new("audit@acme.example")],
            reply_to: Some(Mailbox::new("support@acme.example")),
            subject: "Faturanız hazır".into(),
            text_body: Some("Merhaba".into()),
            html_body: Some("<p>Merhaba</p>".into()),
            headers: vec![("X-HostingOS-Template".into(), "invoice_created".into())],
            attachments: vec![Attachment {
                filename: "fatura.pdf".into(),
                content_type: "application/pdf".into(),
                data: b"%PDF-1.4".to_vec(),
            }],
            ..EmailMessage::default()
        }
    }

    /// Minimal SMTP server on 127.0.0.1 for one connection; sends the DATA payload.
    async fn fake_smtp(rcpt_reply: &'static str) -> (u16, oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (r, mut w) = sock.into_split();
            let mut r = BufReader::new(r);
            w.write_all(b"220 fake.example ESMTP\r\n").await.unwrap();
            let mut tx = Some(tx);
            let mut data = String::new();
            let mut in_data = false;
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                if in_data {
                    if line == ".\r\n" {
                        in_data = false;
                        w.write_all(b"250 2.0.0 queued\r\n").await.unwrap();
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(std::mem::take(&mut data));
                        }
                    } else {
                        data.push_str(&line);
                    }
                    continue;
                }
                let cmd = line.to_ascii_uppercase();
                let reply = if cmd.starts_with("EHLO") {
                    "250-fake.example\r\n250 8BITMIME\r\n"
                } else if cmd.starts_with("RCPT TO") {
                    rcpt_reply
                } else if cmd.starts_with("DATA") {
                    in_data = true;
                    "354 go ahead\r\n"
                } else if cmd.starts_with("QUIT") {
                    let _ = w.write_all(b"221 bye\r\n").await;
                    break;
                } else {
                    "250 ok\r\n"
                };
                w.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (port, rx)
    }

    #[tokio::test]
    async fn sends_through_smtp_server() {
        let (port, rx) = fake_smtp("250 ok\r\n").await;
        let mailer = SmtpMailer::new(config(port)).unwrap();
        let receipt = mailer.send(&message()).await.unwrap();
        let id = receipt.message_id.unwrap();
        assert!(id.starts_with('<') && id.ends_with("@acme.example>"));
        let data = rx.await.unwrap();
        assert!(data.contains(&format!("Message-ID: {id}")));
        assert!(data.contains("From: \"Acme Hosting\" <noreply@acme.example>"));
        assert!(data.contains("Reply-To: support@acme.example"));
        assert!(data.contains("X-HostingOS-Template: invoice_created"));
        assert!(data.contains("multipart/alternative"));
        assert!(data.contains("multipart/mixed"));
        assert!(data.contains("fatura.pdf"));
        // Bcc recipients get the mail but are not disclosed in the headers.
        assert!(!data.contains("audit@acme.example"));
    }

    #[tokio::test]
    async fn permanent_rejection_is_mapped() {
        let (port, _rx) = fake_smtp("550 5.1.1 no such user\r\n").await;
        let mailer = SmtpMailer::new(config(port)).unwrap();
        let err = mailer.send(&message()).await.unwrap_err();
        assert!(
            matches!(err, IntegrationError::Remote { code: Some(ref c), .. } if c == "550"),
            "{err:?}"
        );
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn test_connection_and_unreachable_server() {
        let (port, _rx) = fake_smtp("250 ok\r\n").await;
        let info = SmtpMailer::new(config(port))
            .unwrap()
            .test_connection()
            .await
            .unwrap();
        assert!(info.details.unwrap().contains("reachable"));

        // Bind and drop to get a closed port.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = SmtpMailer::new(config(closed))
            .unwrap()
            .send(&message())
            .await
            .unwrap_err();
        assert!(err.is_retryable(), "{err:?}");
    }

    #[test]
    fn message_building_encodes_and_rejects_injection() {
        let mailer = SmtpMailer::new(config(2525)).unwrap();
        let (msg, _) = mailer.build_message(&message()).unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        // Non-ASCII subject / names are RFC 2047 encoded.
        assert!(raw.contains("Subject: =?utf-8?"));
        assert!(!raw.contains("Bcc:"));

        let mut bad = message();
        bad.subject = "Hi\r\nBcc: evil@example.com".into();
        assert!(mailer.build_message(&bad).is_err());
        let mut bad = message();
        bad.headers = vec![("X-Evil".into(), "a\r\nBcc: evil@example.com".into())];
        assert!(mailer.build_message(&bad).is_err());
        let mut bad = message();
        bad.to = vec![Mailbox::named("x\nBcc: y", "a@example.com")];
        assert!(mailer.build_message(&bad).is_err());
        let mut bad = message();
        bad.attachments[0].content_type = "not a type".into();
        assert!(mailer.build_message(&bad).is_err());
    }

    #[test]
    fn config_validation() {
        let mut c = config(25);
        c.username = Some("user".into());
        c.password = Some("secret".into());
        assert!(matches!(
            SmtpMailer::new(c.clone()),
            Err(IntegrationError::Config(_))
        ));
        c.security = SmtpSecurity::StartTls;
        let m = SmtpMailer::new(c.clone()).unwrap();
        assert!(!format!("{m:?}").contains("secret"));
        c.password = None;
        assert!(SmtpMailer::new(c.clone()).is_err());
        c.host = "bad host".into();
        assert!(SmtpMailer::new(c).is_err());
        assert_eq!(SmtpSecurity::parse("TLS"), Some(SmtpSecurity::Tls));
        assert_eq!(SmtpSecurity::parse("x"), None);
        assert_eq!(SmtpSecurity::StartTls.default_port(), 587);
    }

    #[tokio::test]
    async fn smtp_host_is_checked_against_the_outbound_policy() {
        // Literal loopback / link-local / metadata hosts, in any notation,
        // are refused when the mailer is built.
        for host in [
            "127.0.0.1",
            "2130706433",
            "0x7f.1",
            "[::1]",
            "::ffff:127.0.0.1",
            "169.254.169.254",
            "localhost",
        ] {
            let mut c = config(25);
            c.host = host.into();
            c.outbound = OutboundPolicy::Private;
            assert!(
                matches!(SmtpMailer::new(c), Err(IntegrationError::Config(ref m)) if m.contains("HOS_OUTBOUND_POLICY")),
                "{host}"
            );
        }
        // `public` also refuses private relays.
        let mut c = config(25);
        c.host = "10.0.0.25".into();
        c.outbound = OutboundPolicy::Public;
        assert!(SmtpMailer::new(c.clone()).is_err());
        c.outbound = OutboundPolicy::Private;
        assert!(SmtpMailer::new(c).is_ok());

        // A local relay is reachable only with an explicit policy that allows it:
        // the connection-time check refuses it under `private` (the mailer
        // is built unrestricted, then the policy is swapped to show the
        // connect-time check on its own).
        let (port, _rx) = fake_smtp("250 ok\r\n").await;
        let mut mailer = SmtpMailer::new(config(port)).unwrap();
        mailer.policy = OutboundPolicy::Private;
        let err = mailer.send(&message()).await.unwrap_err();
        assert_eq!(err.kind(), "config", "{err}");
        assert!(!err.is_retryable());
    }
}
