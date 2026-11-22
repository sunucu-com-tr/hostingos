//! Transactional e-mail.
//!
//! [`send_email`] is the single entry point of the web layer. It hands the
//! message to the billing engine's notification pipeline
//! (`Billing::queue_email`): tenant template (or the built-in default) →
//! `hos_mail::render_email` → branded layout → `email_log` row (`queued`) →
//! `send_email` job. The job worker (`hostingos serve` / `hostingos worker`)
//! delivers it through the tenant's mailer integration from the registry
//! (no mailer configured → written to the log), with retries.

use hos_billing::notify::OutgoingEmail;
use hos_core::domain::Locale;
use hos_core::ids::{ClientId, UserId};
use hos_core::integrations::mailer::Mailbox;
use hos_core::tenant::TenantContext;
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Who receives a message.
#[derive(Debug, Clone)]
pub struct Recipient {
    /// Address.
    pub email: String,
    /// Display name.
    pub name: Option<String>,
    /// Related user (email_log).
    pub user_id: Option<UserId>,
    /// Related client (email_log).
    pub client_id: Option<ClientId>,
}

impl Recipient {
    /// Recipient without relations.
    pub fn new(email: impl Into<String>, name: Option<String>) -> Self {
        Self {
            email: email.into(),
            name,
            user_id: None,
            client_id: None,
        }
    }

    fn mailbox(&self) -> Mailbox {
        match &self.name {
            Some(n) if !n.trim().is_empty() => Mailbox::named(n.trim(), self.email.trim()),
            _ => Mailbox::new(self.email.trim()),
        }
    }
}

/// Renders template `key` for `to` and queues it for delivery. `ctx` is
/// merged over the standard `company` / `user` variables. Fails when the
/// template is missing or does not render (recorded in `email_log`).
pub async fn send_email(
    state: &AppState,
    tenant: &TenantContext,
    key: &str,
    locale: Locale,
    to: &Recipient,
    ctx: Value,
) -> AppResult<()> {
    send_email_reply_to(state, tenant, key, locale, to, ctx, None).await
}

/// [`send_email`] with a per-message Reply-To (ticket e-mails carrying a
/// reply token; sealed with the message, see `OutgoingEmail::reply_to`).
pub async fn send_email_reply_to(
    state: &AppState,
    tenant: &TenantContext,
    key: &str,
    locale: Locale,
    to: &Recipient,
    ctx: Value,
    reply_to: Option<String>,
) -> AppResult<()> {
    let email = OutgoingEmail {
        template_key: key.to_owned(),
        to: to.mailbox(),
        locale,
        context: ctx,
        client_id: to.client_id,
        user_id: to.user_id,
        related: None,
        reply_to,
    };
    let mut conn = state.pool.acquire().await?;
    match state
        .billing
        .queue_email(&mut conn, tenant.id, &email)
        .await?
    {
        Some(_) => Ok(()),
        None => Err(AppError::internal(format!(
            "e-mail {key} could not be queued (no template or render error)"
        ))),
    }
}

/// Like [`send_email`] but only logs failures (auth flows must not leak
/// delivery problems to anonymous users).
pub async fn send_email_logged(
    state: &AppState,
    tenant: &TenantContext,
    key: &str,
    locale: Locale,
    to: &Recipient,
    ctx: Value,
) {
    if let Err(e) = send_email(state, tenant, key, locale, to, ctx).await {
        tracing::error!(error = %e, template = key, "queueing e-mail failed");
    }
}
