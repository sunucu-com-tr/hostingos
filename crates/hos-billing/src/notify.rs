//! Notifications: e-mail (template → layout → `email_log` → job → mailer),
//! in-app `notifications` for staff and client users, and signed outgoing
//! webhooks (`webhook_endpoints`) — everything delivered through the job queue.
//!
//! ## Outgoing webhook format
//!
//! `POST {endpoint.url}` with a JSON body
//! `{"id": "<event uuid>", "event": "invoice.paid", "created_at": "...", "tenant_id": "...", "data": {...}}`
//! and the headers `X-HostingOS-Event`, `X-HostingOS-Event-Id` and
//! `X-HostingOS-Signature: t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`.
//! Non-2xx answers are retried with exponential backoff.

use chrono::{DateTime, Utc};
use hos_core::CoreError;
use hos_core::domain::Locale;
use hos_core::ids::{ClientId, TenantId, TicketId, UserId, WebhookEndpointId};
use hos_core::integrations::mailer::{EmailMessage, Mailbox};
use hos_core::secret::SecretString;
use hos_core::tenant::TenantContext;
use hos_db::enums::EmailStatus;
use hos_db::ids::EmailLogId;
use hos_db::repo::mail::{self, NewEmailLog};
use hos_db::repo::webhooks::{self, WebhookEndpointRow};
use hos_db::repo::{billing_support, clients, identity, notifications, platform, support};
use hos_mail::layout::{self, Brand, LayoutOptions};
use hos_mail::{EmailTemplateSource, render_email};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::jobs::{self, DeliverWebhookPayload, JobKind, JobRequest};
use crate::secrets;
use crate::{Billing, BillingResult};

/// An e-mail to render and queue.
#[derive(Debug, Clone)]
pub struct OutgoingEmail {
    /// `email_templates.key` (see `hos_mail::defaults::keys`).
    pub template_key: String,
    pub to: Mailbox,
    pub locale: Locale,
    /// Template variables besides `company` and `user` (added automatically).
    pub context: Value,
    pub client_id: Option<ClientId>,
    pub user_id: Option<UserId>,
    /// `(entity type, id)` stored in `email_log.related_*`.
    pub related: Option<(String, Uuid)>,
    /// Per-message Reply-To (ticket e-mail piping: a plus address carrying
    /// a reply token). Sealed with the message, never logged in plain text.
    pub reply_to: Option<String>,
}

/// Result of sending a logged e-mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Handed to the mailer.
    Sent,
    /// Was already sent (job re-run).
    AlreadySent,
    /// Permanently failed (recorded in `email_log`).
    Failed(String),
}

/// What `email_log` stores in place of a one-time secret.
pub const REDACTED: &str = "[redacted]";

/// Query parameters that carry a one-time secret in any link.
const SECRET_PARAMS: &[&str] = &[
    "token",
    "code",
    "secret",
    "key",
    "signature",
    "sig",
    "otp",
    "access_token",
    "auth",
    "hash",
];

/// Context keys whose string value is itself a secret.
fn is_secret_key(key: &str) -> bool {
    matches!(key, "token" | "otp" | "password" | "secret")
        || key.ends_with("_token")
        || key.ends_with("_password")
        || key.ends_with("_secret")
}

/// `url` with secret query values (all of them for a one-time action link)
/// and an action link's fragment replaced by [`REDACTED`]; `None` when
/// nothing changed.
fn redact_url(raw: &str, action_link: bool) -> Option<String> {
    let mut url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let mut changed = false;
    if url.query().is_some() {
        let query = url
            .query_pairs()
            .map(|(k, v)| {
                if action_link || SECRET_PARAMS.contains(&k.to_ascii_lowercase().as_str()) {
                    changed = true;
                    format!("{}={REDACTED}", enc(&k))
                } else {
                    format!("{}={}", enc(&k), enc(&v))
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        if changed {
            url.set_query(Some(&query));
        }
    }
    if action_link && url.fragment().is_some_and(|f| !f.is_empty()) {
        url.set_fragment(Some(REDACTED));
        changed = true;
    }
    changed.then(|| url.to_string())
}

/// Replaces one-time secrets in an e-mail context (links with tokens, the
/// `action_url` of password-reset / verification / invitation mails, values
/// of `*token` / `*password` / `*secret` keys) with [`REDACTED`]. Returns
/// whether anything was replaced.
pub fn redact_secrets(key: Option<&str>, value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut any = false;
            for (k, v) in map.iter_mut() {
                any |= redact_secrets(Some(k), v);
            }
            any
        }
        Value::Array(items) => {
            let mut any = false;
            for v in items.iter_mut() {
                any |= redact_secrets(key, v);
            }
            any
        }
        Value::String(s) => {
            let key = key.map(str::to_ascii_lowercase);
            if key.as_deref().is_some_and(is_secret_key) && !s.trim().is_empty() {
                *s = REDACTED.to_owned();
                return true;
            }
            let action_link = key
                .as_deref()
                .is_some_and(|k| k == "action_url" || k.ends_with("_action_url"));
            match redact_url(s, action_link) {
                Some(r) => {
                    *s = r;
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

fn merge(base: &mut Value, extra: &Value) {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
}

/// One footer line from the company columns of `tenants` (the same legal
/// name / address the invoices show), e.g. `"Acme Ltd · Main St 1, Ankara 06000"`.
fn footer_address(row: &platform::TenantRow) -> Option<String> {
    let part = |k: &str| {
        row.address
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned()
    };
    let address = [
        part("line1"),
        part("line2"),
        part("city"),
        part("postal_code"),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(", ");
    let legal = row.legal_name.as_deref().map(str::trim).unwrap_or_default();
    let line = [legal, address.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    (!line.is_empty()).then_some(line)
}

/// `"Name" <address>` sender of the tenant (`branding.email_from_*`).
fn tenant_sender(tenant: &TenantContext) -> Option<Mailbox> {
    let email = tenant
        .branding
        .email_from_address
        .as_deref()
        .map(str::trim)
        .filter(|e| e.contains('@'))?;
    Some(Mailbox {
        name: tenant
            .branding
            .email_from_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        email: email.to_owned(),
    })
}

fn company_context(tenant: &TenantContext, url: &str) -> Value {
    json!({
        "name": tenant.display_name(),
        "url": url,
        "support_email": tenant.branding.support_email.clone().unwrap_or_default(),
    })
}

impl Billing {
    /// Renders an e-mail from the tenant's template (or the built-in default),
    /// applies the branded layout, stores it in `email_log` (`queued`) and
    /// queues a `send_email` job. Returns `None` when no template exists or
    /// rendering failed (the failure is recorded in `email_log`).
    pub async fn queue_email(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        email: &OutgoingEmail,
    ) -> BillingResult<Option<EmailLogId>> {
        if email.to.email.trim().is_empty() {
            return Ok(None);
        }
        let tenant = platform::tenant_context(&mut *conn, tenant_id)
            .await?
            .ok_or_else(|| CoreError::not_found("tenant", tenant_id))?;
        let url = self.config().tenant_url(Some(&tenant));
        let stored = mail::get_template(&mut *conn, tenant_id, &email.template_key, email.locale)
            .await?
            .filter(|t| t.is_active);
        let default = hos_mail::default_template(&email.template_key, email.locale)
            .or_else(|| hos_mail::default_template(&email.template_key, tenant.default_locale));
        let (subject, body_html, body_text) = match (&stored, default) {
            (Some(t), _) => (t.subject.clone(), t.body_html.clone(), t.body_text.clone()),
            (None, Some(d)) => (
                d.subject.to_owned(),
                d.body_html.to_owned(),
                d.body_text.to_owned(),
            ),
            (None, None) => {
                tracing::warn!(%tenant_id, key = %email.template_key, "no e-mail template");
                return Ok(None);
            }
        };
        let mut ctx = json!({
            "company": company_context(&tenant, &url),
            "user": {
                "name": email.to.name.clone().unwrap_or_else(|| email.to.email.clone()),
                "email": email.to.email,
            },
        });
        merge(&mut ctx, &email.context);
        // One-time links / tokens are not stored in plain text: `email_log`
        // keeps a copy rendered with them redacted, the real message is sealed
        // until the send job delivered it.
        let mut redacted_ctx = ctx.clone();
        // A per-message Reply-To carries a reply token: seal it like a secret.
        let has_secrets = redact_secrets(None, &mut redacted_ctx) | email.reply_to.is_some();
        let source = EmailTemplateSource {
            subject: &subject,
            body_html: &body_html,
            body_text: Some(&body_text),
        };
        let base_log = |subject: String, html: Option<String>, text: Option<String>| NewEmailLog {
            user_id: email.user_id,
            client_id: email.client_id,
            template_key: Some(email.template_key.clone()),
            to_address: email.to.email.clone(),
            cc: Vec::new(),
            bcc: Vec::new(),
            from_address: None,
            subject,
            body_html: html,
            body_text: text,
            related: email.related.clone(),
        };
        let rendered = match render_email(&source, &ctx) {
            Ok(r) => r,
            Err(e) => {
                let id = mail::log_email(
                    &mut *conn,
                    tenant_id,
                    &base_log(email.template_key.clone(), None, None),
                )
                .await?;
                mail::mark_email_failed(&mut *conn, id, EmailStatus::Failed, &e.to_string())
                    .await?;
                tracing::warn!(%tenant_id, key = %email.template_key, error = %e, "e-mail template failed to render");
                return Ok(None);
            }
        };
        let mut brand = Brand::from_tenant(&tenant);
        if let Some(row) = platform::get_tenant(&mut *conn, tenant_id).await? {
            brand.address = footer_address(&row);
        }
        let options = LayoutOptions {
            preheader: None,
            locale: email.locale,
        };
        let rendered = layout::apply(&brand, rendered, &options);
        let logged = match has_secrets {
            false => None,
            true => {
                let r = render_email(&source, &redacted_ctx).map_err(|e| {
                    CoreError::Internal(format!("redacted e-mail failed to render: {e}"))
                })?;
                Some(layout::apply(&brand, r, &options))
            }
        };
        let id = match &logged {
            None => {
                mail::log_email(
                    &mut *conn,
                    tenant_id,
                    &base_log(
                        rendered.subject.clone(),
                        Some(rendered.html.clone()),
                        Some(rendered.text.clone()),
                    ),
                )
                .await?
            }
            Some(r) => {
                let id = mail::log_email(
                    &mut *conn,
                    tenant_id,
                    &base_log(
                        r.subject.clone(),
                        Some(r.html.clone()),
                        Some(r.text.clone()),
                    ),
                )
                .await?;
                let body = json!({
                    "subject": rendered.subject,
                    "html": rendered.html,
                    "text": rendered.text,
                    "reply_to": email.reply_to,
                });
                let sealed = secrets::seal(
                    self.secret_box(),
                    &body.to_string(),
                    &secrets::email_body_aad(tenant_id, *id.as_uuid()),
                )?;
                mail::set_sealed_body(&mut *conn, tenant_id, id, Some(&sealed)).await?;
                id
            }
        };
        jobs::enqueue_send_email(conn, tenant_id, id).await?;
        Ok(Some(id))
    }

    /// The message a logged e-mail is delivered as: the `email_log` row, with
    /// the sealed unredacted subject / bodies when the logged copy had
    /// one-time links redacted (until it was sent). `None` for an unknown id.
    pub async fn logged_email_message(
        &self,
        tenant_id: TenantId,
        id: EmailLogId,
    ) -> BillingResult<Option<EmailMessage>> {
        let Some(row) = billing_support::get_email_log(self.pool(), tenant_id, id).await? else {
            return Ok(None);
        };
        Ok(Some(self.email_message(tenant_id, &row).await?))
    }

    async fn email_message(
        &self,
        tenant_id: TenantId,
        row: &hos_db::repo::mail::EmailLogRow,
    ) -> BillingResult<EmailMessage> {
        // Sender / reply-to from the tenant's e-mail settings (else the
        // mail integration's default sender).
        let from = platform::tenant_context(self.pool(), tenant_id)
            .await?
            .as_ref()
            .and_then(tenant_sender);
        let reply_to = platform::get_setting(self.pool(), tenant_id, "mail.reply_to")
            .await?
            .and_then(|v| v.as_str().map(str::trim).map(str::to_owned))
            .filter(|e| e.contains('@'))
            .map(Mailbox::new);
        let mut message = EmailMessage {
            from,
            reply_to,
            to: vec![Mailbox::new(row.to_address.clone())],
            cc: row.cc.iter().map(Mailbox::new).collect(),
            bcc: row.bcc.iter().map(Mailbox::new).collect(),
            subject: row.subject.clone(),
            html_body: row.body_html.clone(),
            text_body: row.body_text.clone(),
            tags: row.template_key.iter().cloned().collect(),
            ..EmailMessage::default()
        };
        self.attach_invoice_pdf(tenant_id, row, &mut message).await;
        if let Some(sealed) = mail::sealed_body(self.pool(), tenant_id, row.id).await? {
            let plain = secrets::open(
                self.secret_box(),
                &sealed,
                &secrets::email_body_aad(tenant_id, *row.id.as_uuid()),
            )?;
            let body: Value = serde_json::from_str(plain.expose_str())
                .map_err(|e| CoreError::Internal(format!("sealed e-mail body: {e}")))?;
            let part = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
            if let Some(s) = part("subject") {
                message.subject = s;
            }
            message.html_body = part("html").or(message.html_body);
            message.text_body = part("text").or(message.text_body);
            if let Some(r) = part("reply_to").filter(|r| r.contains('@')) {
                message.reply_to = Some(Mailbox::new(r));
            }
        }
        Ok(message)
    }

    /// Queues an e-mail to a client's primary address in the client's locale.
    pub async fn queue_client_email(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        client_id: ClientId,
        template_key: &str,
        context: Value,
        related: Option<(&str, Uuid)>,
    ) -> BillingResult<Option<EmailLogId>> {
        let client = clients::get_client(&mut *conn, tenant_id, client_id)
            .await?
            .ok_or_else(|| CoreError::not_found("client", client_id))?;
        let name = match client
            .company_name
            .as_deref()
            .filter(|c| !c.trim().is_empty())
        {
            Some(c) if client.first_name.trim().is_empty() => c.to_owned(),
            _ => format!("{} {}", client.first_name, client.last_name)
                .trim()
                .to_owned(),
        };
        let email = OutgoingEmail {
            template_key: template_key.to_owned(),
            to: Mailbox {
                name: Some(name),
                email: client.email.clone(),
            },
            locale: client.locale,
            context,
            client_id: Some(client_id),
            user_id: None,
            related: related.map(|(t, id)| (t.to_owned(), id)),
            reply_to: None,
        };
        self.queue_email(conn, tenant_id, &email).await
    }

    /// Sends a logged e-mail (the `send_email` job). Transport errors are
    /// returned for retry unless `final_attempt`, which records the failure.
    pub async fn send_logged_email(
        &self,
        tenant_id: TenantId,
        id: EmailLogId,
        final_attempt: bool,
    ) -> BillingResult<DeliveryOutcome> {
        let row = billing_support::get_email_log(self.pool(), tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("email_log", id))?;
        match row.status {
            EmailStatus::Sent => return Ok(DeliveryOutcome::AlreadySent),
            EmailStatus::Failed | EmailStatus::Bounced => {
                return Ok(DeliveryOutcome::Failed(row.error.unwrap_or_default()));
            }
            EmailStatus::Queued => {}
        }
        let message = match self.email_message(tenant_id, &row).await {
            Ok(m) => m,
            // The sealed body cannot be opened (key rotated): never send the
            // redacted copy with dead links.
            Err(CoreError::Crypto(e)) => {
                let msg = format!("sealed e-mail body could not be decrypted: {e}");
                mail::mark_email_failed(self.pool(), id, EmailStatus::Failed, &msg).await?;
                mail::set_sealed_body(self.pool(), tenant_id, id, None).await?;
                return Ok(DeliveryOutcome::Failed(msg));
            }
            Err(e) => return Err(e),
        };
        let mailer = self.registry().mailer(tenant_id).await;
        match mailer.send(&message).await {
            Ok(receipt) => {
                mail::mark_email_sent(self.pool(), id, receipt.message_id.as_deref()).await?;
                mail::set_sealed_body(self.pool(), tenant_id, id, None).await?;
                Ok(DeliveryOutcome::Sent)
            }
            Err(e) if e.is_retryable() && !final_attempt => Err(e.into()),
            Err(e) => {
                let msg = e.to_string();
                mail::mark_email_failed(self.pool(), id, EmailStatus::Failed, &msg).await?;
                mail::set_sealed_body(self.pool(), tenant_id, id, None).await?;
                Ok(DeliveryOutcome::Failed(msg))
            }
        }
    }

    /// In-app notification for every active staff member.
    #[allow(clippy::too_many_arguments)]
    pub async fn notify_staff(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        kind: &str,
        title: &str,
        body: Option<&str>,
        link_url: Option<&str>,
        data: &Value,
    ) -> BillingResult<usize> {
        let staff = identity::list_staff(&mut *conn, tenant_id).await?;
        let mut n = 0;
        for u in staff
            .iter()
            .filter(|u| u.status == hos_db::enums::AccountStatus::Active)
        {
            notifications::notify(
                &mut *conn, tenant_id, u.id, kind, title, body, link_url, data,
            )
            .await?;
            n += 1;
        }
        Ok(n)
    }

    /// A client opened (`is_new`) or replied to a ticket: in-app
    /// notification + `staff_ticket` e-mail (in each recipient's language) to
    /// the department's staff, or to every staff member with support access
    /// when nobody is assigned. Returns the number of staff notified.
    pub async fn notify_ticket_staff(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        ticket_id: TicketId,
        is_new: bool,
        message: &str,
    ) -> BillingResult<usize> {
        let Some(ticket) = support::get_ticket(&mut *conn, tenant_id, ticket_id).await? else {
            return Ok(0);
        };
        let department = support::list_departments(&mut *conn, tenant_id, true)
            .await?
            .into_iter()
            .find(|d| d.id == ticket.department_id)
            .map(|d| d.name)
            .unwrap_or_default();
        let client_name = match ticket.client_id {
            Some(c) => clients::get_client(&mut *conn, tenant_id, c)
                .await?
                .map(|c| {
                    let person = format!("{} {}", c.first_name, c.last_name)
                        .trim()
                        .to_owned();
                    c.company_name
                        .filter(|n| !n.trim().is_empty() && person.is_empty())
                        .unwrap_or(person)
                })
                .unwrap_or_default(),
            None => ticket.guest_name.clone().unwrap_or_default(),
        };
        let excerpt: String = {
            let t = message.trim();
            let mut e: String = t.chars().take(400).collect();
            if t.chars().count() > 400 {
                e.push('…');
            }
            e
        };
        let tenant = platform::tenant_context(&mut *conn, tenant_id).await?;
        let base = self.config().tenant_url(tenant.as_ref());
        let link = format!("/admin/support/tickets/{}", ticket.id);
        let (kind, data) = (
            if is_new {
                "ticket.opened"
            } else {
                "ticket.client_reply"
            },
            json!({"ticket_id": ticket.id, "number": ticket.number}),
        );
        let recipients = hos_db::repo::admin_settings::ticket_staff_recipients(
            &mut *conn,
            tenant_id,
            ticket.department_id,
        )
        .await?;
        for u in &recipients {
            let title = match (u.locale, is_new) {
                (Locale::Tr, true) => format!("Yeni talep #{}: {}", ticket.number, ticket.subject),
                (Locale::Tr, false) => {
                    format!("Müşteri yanıtı #{}: {}", ticket.number, ticket.subject)
                }
                (_, true) => format!("New ticket #{}: {}", ticket.number, ticket.subject),
                (_, false) => format!("Client reply #{}: {}", ticket.number, ticket.subject),
            };
            notifications::notify(
                &mut *conn,
                tenant_id,
                u.id,
                kind,
                &title,
                Some(&excerpt),
                Some(&link),
                &data,
            )
            .await?;
            let email = OutgoingEmail {
                template_key: hos_mail::defaults::keys::STAFF_TICKET.to_owned(),
                to: Mailbox {
                    name: Some(u.display_name()),
                    email: u.email.clone(),
                },
                locale: u.locale,
                context: json!({
                    "ticket": {
                        "number": ticket.number,
                        "subject": ticket.subject,
                        "department": department,
                        "client_name": client_name,
                        "excerpt": excerpt,
                        "is_new": is_new,
                    },
                    "action_url": format!("{}{link}", base.trim_end_matches('/')),
                }),
                client_id: None,
                user_id: Some(u.id),
                related: Some(("ticket".to_owned(), ticket.id.into())),
                reply_to: None,
            };
            self.queue_email(conn, tenant_id, &email).await?;
        }
        Ok(recipients.len())
    }

    /// In-app notification for the users of a client.
    #[allow(clippy::too_many_arguments)]
    pub async fn notify_client(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        client_id: ClientId,
        kind: &str,
        title: &str,
        body: Option<&str>,
        link_url: Option<&str>,
    ) -> BillingResult<usize> {
        let users = billing_support::client_recipients(&mut *conn, tenant_id, client_id).await?;
        for u in &users {
            notifications::notify(
                &mut *conn,
                tenant_id,
                u.user_id,
                kind,
                title,
                body,
                link_url,
                &Value::Object(Map::new()),
            )
            .await?;
        }
        Ok(users.len())
    }

    /// Queues a `deliver_webhook` job per subscribed endpoint.
    pub async fn emit_event(
        &self,
        conn: &mut PgConnection,
        tenant_id: TenantId,
        event: &str,
        data: Value,
    ) -> BillingResult<usize> {
        let endpoints = webhooks::endpoints_for_event(&mut *conn, tenant_id, event).await?;
        let occurred_at = self.clock().now();
        for ep in &endpoints {
            let payload = DeliverWebhookPayload {
                endpoint_id: ep.id,
                event: event.to_owned(),
                event_id: Uuid::now_v7(),
                occurred_at,
                data: data.clone(),
            };
            jobs::enqueue(
                conn,
                JobRequest::new(JobKind::DeliverWebhook, Some(tenant_id), &payload),
            )
            .await?;
        }
        Ok(endpoints.len())
    }

    /// Creates an outgoing webhook endpoint with a fresh signing secret
    /// (returned once, stored encrypted).
    pub async fn create_webhook_endpoint(
        &self,
        tenant_id: TenantId,
        url: &str,
        description: Option<&str>,
        events: &[String],
    ) -> BillingResult<(WebhookEndpointRow, SecretString)> {
        let parsed = url::Url::parse(url.trim())
            .map_err(|_| CoreError::validation("webhook URL is invalid"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
            return Err(CoreError::validation("webhook URL must be http(s)"));
        }
        let secret = hos_core::crypto::generate_prefixed_token("whsec");
        let sealed = secrets::seal(
            self.secret_box(),
            &secret,
            &secrets::webhook_endpoint_aad(tenant_id),
        )?;
        let row = webhooks::create_endpoint(
            self.pool(),
            tenant_id,
            parsed.as_str(),
            description,
            Some(&sealed),
            events,
        )
        .await?;
        Ok((row, SecretString::new(secret)))
    }

    /// Decrypted signing secret of an endpoint (`None` when it has none).
    pub fn webhook_secret(
        &self,
        tenant_id: TenantId,
        endpoint: &WebhookEndpointRow,
    ) -> BillingResult<Option<SecretString>> {
        endpoint
            .secret_enc
            .as_deref()
            .map(|s| {
                secrets::open(
                    self.secret_box(),
                    s,
                    &secrets::webhook_endpoint_aad(tenant_id),
                )
            })
            .transpose()
    }

    /// Loads an endpoint of a tenant.
    pub async fn webhook_endpoint(
        &self,
        tenant_id: TenantId,
        id: WebhookEndpointId,
    ) -> BillingResult<Option<WebhookEndpointRow>> {
        Ok(webhooks::list_endpoints(self.pool(), tenant_id)
            .await?
            .into_iter()
            .find(|e| e.id == id))
    }
}

/// Body of an outgoing webhook.
pub fn webhook_body(tenant_id: TenantId, p: &DeliverWebhookPayload) -> String {
    json!({
        "id": p.event_id,
        "event": p.event,
        "created_at": p.occurred_at,
        "tenant_id": tenant_id,
        "data": p.data,
    })
    .to_string()
}

/// `t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`.
pub fn webhook_signature(secret: &str, timestamp: DateTime<Utc>, body: &str) -> String {
    let t = timestamp.timestamp();
    let mac =
        hos_core::crypto::hmac_sha256_hex(secret.as_bytes(), format!("{t}.{body}").as_bytes());
    format!("t={t},v1={mac}")
}

/// Verifies a [`webhook_signature`] header (receivers / tests).
pub fn verify_webhook_signature(secret: &str, header: &str, body: &str) -> bool {
    let mut t = None;
    let mut v1 = None;
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = Some(v.to_owned()),
            Some(("v1", v)) => v1 = Some(v.to_owned()),
            _ => {}
        }
    }
    match (t, v1) {
        (Some(t), Some(sig)) => hos_core::crypto::verify_hmac_sha256_hex(
            secret.as_bytes(),
            format!("{t}.{body}").as_bytes(),
            &sig,
        ),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Outgoing webhook targets (SSRF guard)
// ---------------------------------------------------------------------------

/// Why an outgoing webhook target is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookTargetError {
    /// Not an absolute URL with a host (or it carries credentials).
    #[error("webhook URL is invalid")]
    Invalid,
    /// Only `https://` targets are accepted.
    #[error("webhook URL must use https")]
    NotHttps,
    /// The host is, or resolves to, a loopback / private / link-local /
    /// otherwise non-public address.
    #[error("webhook URL points to a non-public address")]
    NonPublic,
    /// The host name does not resolve.
    #[error("webhook host does not resolve")]
    Unresolvable,
    /// Outbound connections are switched off in this process (demo mode,
    /// `hos_core::net::go_offline`).
    #[error("outgoing webhooks are disabled on this server (demo mode)")]
    Offline,
}

/// Whether `ip` is a public unicast address (no loopback, private, link-local
/// — e.g. the `169.254.169.254` metadata service —, CGNAT, multicast,
/// documentation, benchmarking or reserved ranges; IPv4-mapped IPv6 is
/// checked as IPv4). The one address classifier of the workspace:
/// [`OutboundPolicy::Public`](hos_core::net::OutboundPolicy) in `hos_core::net`.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    hos_core::net::OutboundPolicy::Public.allows_ip(ip)
}

/// Static checks of an outgoing webhook URL: absolute `https://` URL with a
/// host, no user info, and — for literal IP hosts — a public address.
/// Returns `(host, port)`.
pub fn check_webhook_url(raw: &str) -> Result<(String, u16), WebhookTargetError> {
    let url = url::Url::parse(raw.trim()).map_err(|_| WebhookTargetError::Invalid)?;
    if url.scheme() != "https" {
        return Err(WebhookTargetError::NotHttps);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebhookTargetError::Invalid);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    match url.host() {
        Some(url::Host::Ipv4(ip)) if !is_public_ip(ip.into()) => Err(WebhookTargetError::NonPublic),
        Some(url::Host::Ipv6(ip)) if !is_public_ip(ip.into()) => Err(WebhookTargetError::NonPublic),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "localhost"
                || d.ends_with(".localhost")
                || d.ends_with(".local")
                || d.ends_with(".internal")
            {
                return Err(WebhookTargetError::NonPublic);
            }
            Ok((d, port))
        }
        Some(h) => Ok((h.to_string(), port)),
        None => Err(WebhookTargetError::Invalid),
    }
}

/// [`check_webhook_url`] plus DNS: every address the host resolves to must
/// be public. The returned addresses are the ones to connect to (pin them,
/// so a second lookup cannot be rebound to an internal address).
pub async fn resolve_webhook_target(
    raw: &str,
) -> Result<(String, Vec<std::net::SocketAddr>), WebhookTargetError> {
    if hos_core::net::is_offline() {
        // Before the DNS lookup: an offline process sends nothing at all.
        return Err(WebhookTargetError::Offline);
    }
    let (host, port) = check_webhook_url(raw)?;
    let lookup = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((lookup.as_str(), port))
        .await
        .map_err(|_| WebhookTargetError::Unresolvable)?
        .collect();
    if addrs.is_empty() {
        return Err(WebhookTargetError::Unresolvable);
    }
    if addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(WebhookTargetError::NonPublic);
    }
    Ok((lookup, addrs))
}

#[cfg(test)]
mod ssrf_tests {
    use super::*;

    #[test]
    fn public_ip_classification() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn webhook_url_rules() {
        assert_eq!(
            check_webhook_url("http://example.com/hook"),
            Err(WebhookTargetError::NotHttps)
        );
        assert_eq!(
            check_webhook_url("https://127.0.0.1/hook"),
            Err(WebhookTargetError::NonPublic)
        );
        assert_eq!(
            check_webhook_url("https://169.254.169.254/latest/meta-data"),
            Err(WebhookTargetError::NonPublic)
        );
        assert_eq!(
            check_webhook_url("https://[::1]/x"),
            Err(WebhookTargetError::NonPublic)
        );
        assert_eq!(
            check_webhook_url("https://localhost/x"),
            Err(WebhookTargetError::NonPublic)
        );
        assert_eq!(
            check_webhook_url("https://user:pw@example.com/x"),
            Err(WebhookTargetError::Invalid)
        );
        assert_eq!(
            check_webhook_url("https://hooks.example.com:8443/x"),
            Ok(("hooks.example.com".into(), 8443))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_roundtrip() {
        let now = Utc::now();
        let sig = webhook_signature("whsec_x", now, "{\"a\":1}");
        assert!(sig.starts_with(&format!("t={},v1=", now.timestamp())));
        assert!(verify_webhook_signature("whsec_x", &sig, "{\"a\":1}"));
        assert!(!verify_webhook_signature("whsec_y", &sig, "{\"a\":1}"));
        assert!(!verify_webhook_signature("whsec_x", &sig, "{\"a\":2}"));
        assert!(!verify_webhook_signature("whsec_x", "garbage", "{}"));
    }

    #[test]
    fn one_time_links_are_redacted() {
        let mut ctx = json!({
            "company": {"url": "https://panel.example"},
            "action_url": "https://panel.example/auth/reset?token=abc123&lang=tr#x",
            "invoice": {"url": "https://panel.example/client/invoices/1?view=pdf"},
            "links": ["https://cdn.example/f?sig=zzz&w=1"],
            "invite_token": "tok",
            "expires_in_minutes": 60,
        });
        assert!(redact_secrets(None, &mut ctx));
        assert_eq!(
            ctx["action_url"],
            "https://panel.example/auth/reset?token=[redacted]&lang=[redacted]#[redacted]"
        );
        assert_eq!(ctx["company"]["url"], "https://panel.example");
        assert_eq!(
            ctx["invoice"]["url"],
            "https://panel.example/client/invoices/1?view=pdf"
        );
        assert_eq!(ctx["links"][0], "https://cdn.example/f?sig=[redacted]&w=1");
        assert_eq!(ctx["invite_token"], REDACTED);
        assert_eq!(ctx["expires_in_minutes"], 60);
        let mut plain = json!({"action_url": "https://panel.example/client"});
        assert!(!redact_secrets(None, &mut plain));
    }

    #[test]
    fn context_merge() {
        let mut a = json!({"company": {"name": "x"}, "user": {}});
        merge(&mut a, &json!({"invoice": {"number": "1"}}));
        assert_eq!(a["invoice"]["number"], "1");
        assert_eq!(a["company"]["name"], "x");
    }
}
