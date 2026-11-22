//! # hos-mail
//!
//! Outgoing email for HostingOS. Pure logic + HTTP/SMTP; never touches the DB.
//!
//! | Module | Contents |
//! |---|---|
//! | [`smtp`], [`resend`], [`postmark`], [`mailgun`], [`log_mailer`] | [`Mailer`] implementations |
//! | [`build`] / [`descriptors`] | integration factory (`integrations.category = 'mail'`) |
//! | [`template`] | safe `{{var}}` / `{{#if}}` engine for `email_templates` rows |
//! | [`layout`] | tenant-branded responsive HTML frame, HTML → text |
//! | [`defaults`] | built-in tr/en templates (fallback when a tenant has none) |
//!
//! ## Typical flow (in `hos-web` / `hos-jobs`)
//!
//! 1. Load the tenant's `email_templates` row for `(key, locale)`, else
//!    [`defaults::default_template`].
//! 2. [`template::render_email`] with a JSON context (see [`defaults::sample_context`]
//!    for the expected variable shapes).
//! 3. [`layout::apply`] with [`layout::Brand::from_tenant`].
//! 4. [`template::RenderedEmail::into_message`], then queue / `Mailer::send`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;

use hos_core::domain::IntegrationCategory;
use hos_core::error::IntegrationError;
use hos_core::integrations::{FieldReader, FieldSpec, IntegrationDescriptor, Mailer, SelectOption};
pub use hos_core::net::OutboundPolicy;

mod common;
pub mod defaults;
pub mod layout;
pub mod log_mailer;
pub mod mailgun;
pub mod postmark;
pub mod resend;
pub mod smtp;
pub mod template;

pub use defaults::{DefaultTemplate, default_template, default_templates, sample_context};
pub use layout::{Brand, LayoutOptions};
pub use log_mailer::LogMailer;
pub use mailgun::{MailgunMailer, MailgunRegion};
pub use postmark::PostmarkMailer;
pub use resend::ResendMailer;
pub use smtp::{SmtpConfig, SmtpMailer, SmtpSecurity};
pub use template::{
    EmailTemplateSource, Escape, RenderedEmail, Template, TemplateError, render_email,
};

/// `integrations.kind` values handled by this crate.
pub mod kinds {
    pub use hos_core::integrations::kinds::SMTP;
    /// Resend HTTP API.
    pub const RESEND: &str = "resend";
    /// Postmark HTTP API.
    pub const POSTMARK: &str = "postmark";
    /// Mailgun HTTP API.
    pub const MAILGUN: &str = "mailgun";
    /// Development: log instead of sending.
    pub const LOG: &str = "log";
    /// Every kind, in admin display order.
    pub const ALL: &[&str] = &[SMTP, RESEND, POSTMARK, MAILGUN, LOG];
}

fn sender_fields(d: IntegrationDescriptor, required: bool) -> IntegrationDescriptor {
    let from_email = FieldSpec::text("from_email", "Sender address")
        .placeholder("noreply@example.com")
        .help("Default From address; must be verified with the provider.");
    d.config_field(if required {
        from_email.required()
    } else {
        from_email
    })
    .config_field(FieldSpec::text("from_name", "Sender name").placeholder("Example Hosting"))
}

fn timeout_field() -> FieldSpec {
    FieldSpec::number("timeout_secs", "Timeout (seconds)")
        .default_value(common::DEFAULT_TIMEOUT_SECS)
        .help("Network timeout per request (1–120).")
}

/// Descriptors of every mail integration kind (admin "add integration" form).
pub fn descriptors() -> Vec<IntegrationDescriptor> {
    let smtp = IntegrationDescriptor::new(kinds::SMTP, "SMTP", IntegrationCategory::Mail)
        .description("Any SMTP server or relay (STARTTLS, implicit TLS or plain for local relays).")
        .config_field(
            FieldSpec::text("host", "Host")
                .required()
                .placeholder("smtp.example.com"),
        )
        .config_field(
            FieldSpec::select(
                "security",
                "Security",
                vec![
                    SelectOption::new("starttls", "STARTTLS (587)"),
                    SelectOption::new("tls", "TLS / SMTPS (465)"),
                    SelectOption::new("none", "None (local relay only, 25)"),
                ],
            )
            .default_value("starttls"),
        )
        .config_field(
            FieldSpec::number("port", "Port").help("Empty = default port of the security mode."),
        )
        .config_field(FieldSpec::text("username", "Username"));
    let smtp = sender_fields(smtp, true)
        .config_field(timeout_field())
        .secret_field(FieldSpec::password("password", "Password"));

    let resend = IntegrationDescriptor::new(kinds::RESEND, "Resend", IntegrationCategory::Mail)
        .docs_url("https://resend.com/docs/api-reference/emails/send-email");
    let resend = sender_fields(resend, true)
        .config_field(timeout_field())
        .secret_field(
            FieldSpec::password("api_key", "API key")
                .required()
                .placeholder("re_…"),
        );

    let postmark =
        IntegrationDescriptor::new(kinds::POSTMARK, "Postmark", IntegrationCategory::Mail)
            .docs_url("https://postmarkapp.com/developer/api/email-api");
    let postmark = sender_fields(postmark, true)
        .config_field(
            FieldSpec::text("message_stream", "Message stream")
                .default_value(postmark::DEFAULT_MESSAGE_STREAM),
        )
        .config_field(timeout_field())
        .secret_field(FieldSpec::password("server_token", "Server API token").required());

    let mailgun = IntegrationDescriptor::new(kinds::MAILGUN, "Mailgun", IntegrationCategory::Mail)
        .docs_url("https://documentation.mailgun.com/docs/mailgun/api-reference/")
        .config_field(
            FieldSpec::text("domain", "Sending domain")
                .required()
                .placeholder("mg.example.com"),
        )
        .config_field(
            FieldSpec::select(
                "region",
                "Region",
                vec![SelectOption::new("us", "US"), SelectOption::new("eu", "EU")],
            )
            .default_value("us"),
        );
    let mailgun = sender_fields(mailgun, true)
        .config_field(timeout_field())
        .secret_field(FieldSpec::password("api_key", "API key").required());

    let log = IntegrationDescriptor::new(
        kinds::LOG,
        "Log only (development)",
        IntegrationCategory::Mail,
    )
    .description("Writes emails to the application log; nothing is delivered. Development only.");
    let log = sender_fields(log, false);

    vec![smtp, resend, postmark, mailgun, log]
}

/// Builds a [`Mailer`] from an `integrations` row (`config` plain JSON,
/// `secrets` decrypted JSON). Unknown kinds → [`IntegrationError::Config`].
/// The SMTP host (and the HTTP APIs) are checked against the process-wide
/// outbound policy (`HOS_OUTBOUND_POLICY`, see [`hos_core::net`]).
pub fn build(
    kind: &str,
    config: &serde_json::Value,
    secrets: &serde_json::Value,
) -> Result<Arc<dyn Mailer>, IntegrationError> {
    build_with_policy(kind, config, secrets, OutboundPolicy::global())
}

/// [`build`] with an explicit outbound policy.
pub fn build_with_policy(
    kind: &str,
    config: &serde_json::Value,
    secrets: &serde_json::Value,
    policy: OutboundPolicy,
) -> Result<Arc<dyn Mailer>, IntegrationError> {
    let descriptor = descriptors()
        .into_iter()
        .find(|d| d.kind == kind)
        .ok_or_else(|| {
            IntegrationError::Config(format!("unknown mail integration kind {kind:?}"))
        })?;
    descriptor.validate(config, secrets)?;
    let cfg = FieldReader::config(config);
    let sec = FieldReader::secrets(secrets);

    let mailer: Arc<dyn Mailer> = match descriptor.kind {
        k if k == kinds::SMTP => {
            let security = match cfg.opt_str("security")? {
                Some(s) => SmtpSecurity::parse(s).ok_or_else(|| {
                    IntegrationError::Config("config.security must be starttls, tls or none".into())
                })?,
                None => SmtpSecurity::default(),
            };
            let port = cfg
                .opt_u64("port")?
                .map(|p| {
                    u16::try_from(p).ok().filter(|p| *p > 0).ok_or_else(|| {
                        IntegrationError::Config("config.port must be 1–65535".into())
                    })
                })
                .transpose()?;
            Arc::new(SmtpMailer::new(SmtpConfig {
                host: cfg.str("host")?.to_owned(),
                port,
                security,
                username: cfg.opt_str("username")?.map(str::to_owned),
                password: sec.opt_secret("password")?,
                default_from: required_sender(&cfg)?,
                timeout: common::read_timeout(&cfg)?,
                outbound: policy,
            })?)
        }
        k if k == kinds::RESEND => Arc::new(
            ResendMailer::new(
                sec.secret("api_key")?,
                required_sender(&cfg)?,
                common::read_timeout(&cfg)?,
            )?
            .with_outbound_policy(policy)?,
        ),
        k if k == kinds::POSTMARK => Arc::new(
            PostmarkMailer::new(
                sec.secret("server_token")?,
                required_sender(&cfg)?,
                cfg.string_or("message_stream", postmark::DEFAULT_MESSAGE_STREAM)?,
                common::read_timeout(&cfg)?,
            )?
            .with_outbound_policy(policy)?,
        ),
        k if k == kinds::MAILGUN => {
            let region = match cfg.opt_str("region")? {
                None | Some("us") => MailgunRegion::Us,
                Some("eu") => MailgunRegion::Eu,
                Some(_) => {
                    return Err(IntegrationError::Config(
                        "config.region must be us or eu".into(),
                    ));
                }
            };
            Arc::new(
                MailgunMailer::new(
                    sec.secret("api_key")?,
                    cfg.str("domain")?,
                    region,
                    required_sender(&cfg)?,
                    common::read_timeout(&cfg)?,
                )?
                .with_outbound_policy(policy)?,
            )
        }
        _ => Arc::new(LogMailer::new(common::read_sender(&cfg, false)?)),
    };
    Ok(mailer)
}

fn required_sender(
    cfg: &FieldReader<'_>,
) -> Result<hos_core::integrations::mailer::Mailbox, IntegrationError> {
    common::read_sender(cfg, true)?
        .ok_or_else(|| IntegrationError::Config("config.from_email is required".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptors_cover_all_kinds() {
        let d = descriptors();
        let kinds_listed: Vec<&str> = d.iter().map(|d| d.kind).collect();
        assert_eq!(kinds_listed, kinds::ALL);
        assert!(d.iter().all(|d| d.category == IntegrationCategory::Mail));
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v[0]["config_fields"][1]["kind"]["type"], "select");
    }

    #[tokio::test]
    async fn build_each_kind() {
        let from = json!({"from_email": "noreply@acme.example", "from_name": "Acme"});
        let smtp = build(
            kinds::SMTP,
            &json!({"host": "smtp.acme.example", "port": "2525", "security": "tls", "username": "u",
                    "from_email": "noreply@acme.example"}),
            &json!({"password": "p"}),
        )
        .unwrap();
        assert_eq!(smtp.kind(), "smtp");
        assert_eq!(
            build(kinds::RESEND, &from, &json!({"api_key": "re_x"}))
                .unwrap()
                .kind(),
            "resend"
        );
        assert_eq!(
            build(kinds::POSTMARK, &from, &json!({"server_token": "t"}))
                .unwrap()
                .kind(),
            "postmark"
        );
        let mut mg = from.clone();
        mg["domain"] = json!("mg.acme.example");
        mg["region"] = json!("eu");
        assert_eq!(
            build(kinds::MAILGUN, &mg, &json!({"api_key": "k"}))
                .unwrap()
                .kind(),
            "mailgun"
        );
        let log = build(kinds::LOG, &json!({}), &json!({})).unwrap();
        assert_eq!(log.kind(), "log");
        assert!(log.test_connection().await.is_ok());
    }

    #[test]
    fn build_rejects_bad_config() {
        let err = |kind: &str, c: serde_json::Value, s: serde_json::Value| match build(kind, &c, &s)
        {
            Err(e) => e,
            Ok(_) => panic!("expected error for {kind} {c}"),
        };
        assert!(matches!(
            err("sendgrid", json!({}), json!({})),
            IntegrationError::Config(_)
        ));
        assert_eq!(
            err(kinds::RESEND, json!({"from_email": "a@b.co"}), json!({})),
            IntegrationError::Config("secrets.api_key is required".into())
        );
        assert_eq!(
            err(kinds::RESEND, json!({}), json!({"api_key": "x"})),
            IntegrationError::Config("config.from_email is required".into())
        );
        assert!(matches!(
            err(
                kinds::RESEND,
                json!({"from_email": "nope"}),
                json!({"api_key": "x"})
            ),
            IntegrationError::Config(_)
        ));
        assert!(matches!(
            err(
                kinds::SMTP,
                json!({"host": "h.example", "security": "bogus", "from_email": "a@b.co"}),
                json!({})
            ),
            IntegrationError::Config(_)
        ));
        assert!(matches!(
            err(
                kinds::SMTP,
                json!({"host": "h.example", "port": 70000, "from_email": "a@b.co"}),
                json!({})
            ),
            IntegrationError::Config(_)
        ));
        assert!(matches!(
            err(
                kinds::SMTP,
                json!({"host": "h.example", "security": "none", "username": "u", "from_email": "a@b.co"}),
                json!({"password": "p"})
            ),
            IntegrationError::Config(_)
        ));
        assert!(matches!(
            err(
                kinds::MAILGUN,
                json!({"from_email": "a@b.co", "domain": "x.example", "region": "asia"}),
                json!({"api_key": "k"})
            ),
            IntegrationError::Config(_)
        ));
    }
}
