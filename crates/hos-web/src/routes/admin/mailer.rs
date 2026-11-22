//! Staff-written e-mails (client messages, template tests): rendered into
//! the tenant's branded layout, logged in `email_log` and handed to the job
//! queue (`send_email` job), which delivers through the tenant's mailer.

use hos_core::domain::Locale;
use hos_core::ids::ClientId;
use hos_db::ids::EmailLogId;
use hos_db::repo::mail::{self as mail_repo, NewEmailLog};
use hos_mail::template::RenderedEmail;
use hos_mail::{Brand, LayoutOptions};

use crate::error::AppResult;
use crate::state::AppState;
use crate::tenant::Tenant;

/// Queues an e-mail whose body is Markdown written by staff.
#[allow(clippy::too_many_arguments)]
pub async fn queue_custom_email(
    state: &AppState,
    tenant: &Tenant,
    to: &str,
    name: Option<String>,
    locale: Locale,
    client_id: Option<ClientId>,
    subject: &str,
    markdown: &str,
) -> AppResult<EmailLogId> {
    let rendered = RenderedEmail {
        subject: subject.trim().replace(['\r', '\n'], " "),
        html: crate::markdown::render(markdown),
        text: markdown.to_owned(),
        headers: Vec::new(),
    };
    queue_rendered(state, tenant, to, name, locale, client_id, None, rendered).await
}

/// Lays out, logs and queues an already rendered e-mail.
#[allow(clippy::too_many_arguments)]
pub async fn queue_rendered(
    state: &AppState,
    tenant: &Tenant,
    to: &str,
    _name: Option<String>,
    locale: Locale,
    client_id: Option<ClientId>,
    template_key: Option<String>,
    rendered: RenderedEmail,
) -> AppResult<EmailLogId> {
    let brand = Brand::from_tenant(&tenant.0);
    let email = hos_mail::layout::apply(
        &brand,
        rendered,
        &LayoutOptions {
            preheader: None,
            locale,
        },
    );
    let log_id = mail_repo::log_email(
        &state.pool,
        tenant.id,
        &NewEmailLog {
            client_id,
            template_key,
            to_address: to.trim().to_owned(),
            from_address: tenant.branding.email_from_address.clone(),
            subject: email.subject.clone(),
            body_html: Some(email.html),
            body_text: Some(email.text),
            ..NewEmailLog::default()
        },
    )
    .await?;
    let mut conn = state.pool.acquire().await?;
    hos_billing::jobs::enqueue_send_email(&mut conn, tenant.id, log_id).await?;
    Ok(log_id)
}
