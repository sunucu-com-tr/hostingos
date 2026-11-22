//! `/admin/support/inbound` — e-mail piping: the log of received messages
//! with the quarantine review (release into a ticket, or discard; needs
//! `support.manage`) and the settings (switch, unknown-sender policy,
//! default department, Mailgun signing key, raw-endpoint token; needs
//! `settings.manage`). Secrets are write-only: the signing key is sealed,
//! the raw token is shown once and only its SHA-256 is stored. Audited.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_core::ids::TicketDepartmentId;
use hos_db::repo::mail_inbound::{
    self as mrepo, InboundMessageId, InboundMessageRow, InboundSettingsRow, InboundStatus,
    UnknownSenders,
};
use hos_db::repo::support::{self, TicketDepartmentRow};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::routes::webhooks::mail::{
    ClientOrGuest, MailTicket, account_label, open_ticket_from, sender_account,
};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

const BACK: &str = "/admin/support/inbound";
const SETTINGS: &str = "/admin/support/inbound/settings";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/support/inbound", get(list))
        .route(
            "/admin/support/inbound/settings",
            get(settings).post(save_settings),
        )
        .route("/admin/support/inbound/settings/token", post(new_token))
        .route(
            "/admin/support/inbound/settings/token/delete",
            post(delete_token),
        )
        .route("/admin/support/inbound/bulk", post(bulk))
        .route("/admin/support/inbound/{id}", get(show))
        .route("/admin/support/inbound/{id}/release", post(release))
        .route("/admin/support/inbound/{id}/append", post(append))
        .route("/admin/support/inbound/{id}/discard", post(discard))
}

fn trail() -> [(&'static str, &'static str); 1] {
    [("admin.menu.tickets", "/admin/support/tickets")]
}

fn status_tone(s: &InboundStatus) -> &'static str {
    match s {
        InboundStatus::TicketCreated | InboundStatus::ReplyAdded | InboundStatus::Released => {
            "success"
        }
        InboundStatus::Quarantined => "warning",
        InboundStatus::Rejected => "danger",
        InboundStatus::Discarded => "",
    }
}

// ---------------------------------------------------------------------------
// Log + quarantine
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    show: String,
}

#[derive(Template)]
#[template(path = "admin/mail_inbound.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<InboundMessageRow>,
    show: String,
    quarantined: i64,
    enabled: bool,
}
crate::page!(ListPage);

impl ListPage {
    fn sbadge_of(&self, s: &InboundStatus) -> askama::filters::Safe<String> {
        self.badge(
            self.t(&format!("inbound.status.{}", s.as_str())),
            status_tone(s),
        )
    }
    fn reason(&self, r: &Option<String>) -> String {
        match r.as_deref() {
            Some(r) => self.lbl("inbound.reason", r),
            None => String::new(),
        }
    }
    fn tab_items(&self) -> Vec<(String, String, bool)> {
        [
            ("", "inbound.admin.show_quarantine"),
            ("all", "inbound.admin.show_all"),
            ("rejected", "inbound.admin.show_rejected"),
        ]
        .into_iter()
        .map(|(key, label)| {
            let href = if key.is_empty() {
                self.url(BACK)
            } else {
                self.url(format!("{BACK}?show={key}"))
            };
            let mut text = self.t(label);
            if key.is_empty() && self.quarantined > 0 {
                text = format!("{text} ({})", self.quarantined);
            }
            (href, text, self.show == key)
        })
        .collect()
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    Query(q): Query<ListQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status = match q.show.as_str() {
        "all" => None,
        "rejected" => Some(InboundStatus::Rejected),
        _ => Some(InboundStatus::Quarantined),
    };
    let show = match q.show.as_str() {
        "all" | "rejected" => q.show.clone(),
        _ => String::new(),
    };
    let rows = mrepo::list(&state.pool, tenant.id, status, 200).await?;
    let quarantined = mrepo::status_counts(&state.pool, tenant.id)
        .await?
        .into_iter()
        .find(|(s, _)| *s == InboundStatus::Quarantined)
        .map_or(0, |(_, n)| n);
    let enabled = mrepo::settings(&state.pool, tenant.id)
        .await?
        .is_some_and(|s| s.enabled);
    let title = ctx.t("inbound.admin.title");
    common::page(&mut ctx, &staff, &title, &trail());
    render(&ListPage {
        ctx,
        rows,
        show,
        quarantined,
        enabled,
    })
}

#[derive(Template)]
#[template(path = "admin/mail_inbound_message.html")]
struct MessagePage {
    ctx: PageCtx,
    m: InboundMessageRow,
    departments: Vec<TicketDepartmentRow>,
    default_department: Option<TicketDepartmentId>,
    account: Option<String>,
    ticket_number: Option<String>,
}
crate::page!(MessagePage);

impl MessagePage {
    fn sbadge_of(&self, s: &InboundStatus) -> askama::filters::Safe<String> {
        self.badge(
            self.t(&format!("inbound.status.{}", s.as_str())),
            status_tone(s),
        )
    }
    fn reason(&self) -> String {
        match self.m.reason.as_deref() {
            Some(r) => self.lbl("inbound.reason", r),
            None => "—".into(),
        }
    }
    fn is_quarantined(&self) -> bool {
        self.m.status == InboundStatus::Quarantined
    }
    fn dept_sel(&self, d: &TicketDepartmentRow) -> &'static str {
        if self.default_department == Some(d.id) {
            " selected"
        } else {
            ""
        }
    }
    fn body(&self) -> String {
        self.m.body_text.clone().unwrap_or_default()
    }
    fn attachments(&self) -> String {
        self.m.attachment_names.join(", ")
    }
}

async fn message(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<InboundMessageRow> {
    mrepo::get(&state.pool, tenant.id, InboundMessageId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let m = message(&state, &tenant, id).await?;
    let departments = support::list_departments(&state.pool, tenant.id, true).await?;
    let default_department = mrepo::settings(&state.pool, tenant.id)
        .await?
        .and_then(|s| s.default_department_id);
    let account = match m.sender.as_deref() {
        Some(s) if m.status == InboundStatus::Quarantined => {
            let who = sender_account(&state, &tenant, s).await?;
            account_label(&state, &tenant, who).await?
        }
        _ => None,
    };
    let ticket_number = match m.ticket_id {
        Some(t) => support::get_ticket(&state.pool, tenant.id, t)
            .await?
            .map(|t| t.number),
        None => None,
    };
    let title = m
        .subject
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ctx.t("inbound.no_subject"));
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[trail()[0], ("inbound.admin.title", BACK)],
    );
    render(&MessagePage {
        ctx,
        m,
        departments,
        default_department,
        account,
        ticket_number,
    })
}

async fn release(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let m = message(&state, &tenant, id).await?;
    let back = format!("{BACK}/{}", m.id);
    let Some(sender) = m.sender.clone() else {
        return Ok(failed(&ctx, &back, ctx.t("inbound.admin.no_sender")));
    };
    let department = f
        .id::<TicketDepartmentId>("department_id")
        .ok_or(AppError::NotFound)?;
    support::get_department(&state.pool, tenant.id, department)
        .await?
        .ok_or(AppError::NotFound)?;
    let who = sender_account(&state, &tenant, &sender).await?;
    // Claim, open the ticket and link it in one transaction: a second click
    // (or another staff member) gets nothing, and a failure leaves the
    // message in quarantine instead of released without a ticket.
    let mut tx = state.pool.begin().await?;
    if !mrepo::review(
        &mut *tx,
        tenant.id,
        m.id,
        InboundStatus::Released,
        None,
        staff.user_id,
        Utc::now(),
    )
    .await?
    {
        return Ok(failed(&ctx, &back, ctx.t("inbound.admin.already_reviewed")));
    }
    let (ticket, _) = open_ticket_from(
        &state,
        &tenant,
        &mut tx,
        MailTicket {
            department,
            sender: &sender,
            name: &sender,
            subject: m.subject.as_deref().unwrap_or_default(),
            body: m.body_text.as_deref().unwrap_or_default(),
            message_id: None,
            who,
            sender_verified: m.sender_verified == Some(true),
        },
    )
    .await?;
    mrepo::set_ticket(&mut *tx, tenant.id, m.id, ticket.id).await?;
    tx.commit().await?;
    au.log(
        &state,
        "mail_inbound.release",
        Some(("mail_inbound_message", m.id.into_uuid())),
        json!({"ticket": ticket.number, "as_guest": matches!(who, ClientOrGuest::Guest)}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/support/tickets/{}", ticket.id),
        "inbound.admin.released",
    ))
}

/// `POST /admin/support/inbound/{id}/append` — a quarantined reply whose
/// sender did not match its ticket (`sender_mismatch`) is added to that
/// ticket after review, as a message from the sender's address.
async fn append(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let m = message(&state, &tenant, id).await?;
    let back = format!("{BACK}/{}", m.id);
    let Some(sender) = m.sender.clone() else {
        return Ok(failed(&ctx, &back, ctx.t("inbound.admin.no_sender")));
    };
    let Some(ticket) = (match m.ticket_id {
        Some(t) => support::get_ticket(&state.pool, tenant.id, t).await?,
        None => None,
    }) else {
        return Err(AppError::NotFound);
    };
    if m.status != InboundStatus::Quarantined {
        return Ok(failed(&ctx, &back, ctx.t("inbound.admin.already_reviewed")));
    }
    let text = m.body_text.clone().unwrap_or_default();
    let body = if text.trim().is_empty() {
        state.i18n.t(tenant.default_locale, "inbound.empty_body")
    } else {
        crate::inbound::text_to_markdown(&text)
    };
    // Claim, append and link in one transaction: a second click (or another
    // staff member) gets nothing, and a failure leaves the message in
    // quarantine instead of released without being added.
    let mut tx = state.pool.begin().await?;
    if !mrepo::review(
        &mut *tx,
        tenant.id,
        m.id,
        InboundStatus::Released,
        None,
        staff.user_id,
        Utc::now(),
    )
    .await?
    {
        return Ok(failed(&ctx, &back, ctx.t("inbound.admin.already_reviewed")));
    }
    let (msg, _) = support::reply_to_ticket(
        &mut tx,
        tenant.id,
        ticket.id,
        &support::NewTicketMessage {
            author_kind: if ticket.client_id.is_some() {
                hos_db::enums::AuthorKind::Client
            } else {
                hos_db::enums::AuthorKind::Guest
            },
            user_id: None,
            author_name: sender.clone(),
            author_email: Some(sender.clone()),
            body,
            is_internal: false,
            source: hos_db::enums::MessageSource::Email,
            message_id: None,
            ip: None,
        },
    )
    .await?;
    support::set_message_sender_verified(
        &mut *tx,
        tenant.id,
        msg.id,
        m.sender_verified == Some(true),
    )
    .await?;
    mrepo::set_ticket(&mut *tx, tenant.id, m.id, ticket.id).await?;
    tx.commit().await?;
    au.log(
        &state,
        "mail_inbound.append",
        Some(("mail_inbound_message", m.id.into_uuid())),
        json!({"ticket": ticket.number, "sender": sender}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/support/tickets/{}", ticket.id),
        "inbound.admin.appended",
    ))
}

async fn discard(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let m = message(&state, &tenant, id).await?;
    if !mrepo::review(
        &state.pool,
        tenant.id,
        m.id,
        InboundStatus::Discarded,
        None,
        staff.user_id,
        Utc::now(),
    )
    .await?
    {
        return Ok(failed(
            &ctx,
            &format!("{BACK}/{}", m.id),
            ctx.t("inbound.admin.already_reviewed"),
        ));
    }
    au.log(
        &state,
        "mail_inbound.discard",
        Some(("mail_inbound_message", m.id.into_uuid())),
        json!({}),
    )
    .await?;
    Ok(done(&ctx, BACK, "inbound.admin.discarded"))
}

/// `POST /admin/support/inbound/bulk` — discards the selected quarantined
/// messages (their text is dropped). Already reviewed messages and ids of
/// other tenants are skipped.
async fn bulk(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let ids: Vec<InboundMessageId> = f.ids("ids");
    if ids.is_empty() {
        return Ok(failed(&ctx, BACK, ctx.t("admin.common.nothing_selected")));
    }
    if f.text("action", 20) != "discard" {
        return Ok(failed(&ctx, BACK, ctx.t("admin.common.invalid_value")));
    }
    let now = Utc::now();
    let (mut changed, mut skipped) = (0usize, 0usize);
    for id in &ids {
        if mrepo::review(
            &state.pool,
            tenant.id,
            *id,
            InboundStatus::Discarded,
            None,
            staff.user_id,
            now,
        )
        .await?
        {
            au.log(
                &state,
                "mail_inbound.discard",
                Some(("mail_inbound_message", id.into_uuid())),
                json!({"bulk": true}),
            )
            .await?;
            changed += 1;
        } else {
            skipped += 1;
        }
    }
    Ok(crate::flash::redirect(
        &ctx.url(BACK),
        crate::flash::SetFlash::success(ctx.tv2(
            "admin.common.bulk_done",
            "done",
            changed,
            "skipped",
            skipped,
        )),
    ))
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/mail_inbound_settings.html")]
struct SettingsPage {
    ctx: PageCtx,
    s: Option<InboundSettingsRow>,
    departments: Vec<TicketDepartmentRow>,
    mailgun_url: String,
    raw_url: String,
    /// A new raw-endpoint token, shown once.
    new_token: Option<String>,
}
crate::page!(SettingsPage);

impl SettingsPage {
    fn enabled(&self) -> bool {
        self.s.as_ref().is_some_and(|s| s.enabled)
    }
    fn has_key(&self) -> bool {
        self.s.as_ref().is_some_and(|s| s.mailgun_key_enc.is_some())
    }
    fn has_token(&self) -> bool {
        self.s.as_ref().is_some_and(|s| s.raw_token_hash.is_some())
    }
    fn trust_spf(&self) -> bool {
        self.s.as_ref().is_some_and(|s| s.mailgun_trust_spf)
    }
    fn authserv_id(&self) -> String {
        self.s
            .as_ref()
            .and_then(|s| s.trusted_authserv_id.clone())
            .unwrap_or_default()
    }
    fn policy(&self) -> String {
        self.s
            .as_ref()
            .map_or(UnknownSenders::Quarantine, |s| s.unknown_senders)
            .as_str()
            .to_owned()
    }
    fn policy_options(&self) -> Vec<(String, String)> {
        common::enum_options(self, UnknownSenders::ALL, |p| p.as_str(), "inbound.policy")
    }
    fn dept_sel(&self, d: &TicketDepartmentRow) -> &'static str {
        if self.s.as_ref().and_then(|s| s.default_department_id) == Some(d.id) {
            " selected"
        } else {
            ""
        }
    }
}

async fn render_settings(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    new_token: Option<String>,
) -> AppResult<Response> {
    let title = ctx.t("inbound.admin.settings_title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[trail()[0], ("inbound.admin.title", BACK)],
    );
    let page = SettingsPage {
        s: mrepo::settings(&state.pool, tenant.id).await?,
        departments: support::list_departments(&state.pool, tenant.id, true).await?,
        mailgun_url: format!("{}/webhooks/mail/mailgun-mime", ctx.origin),
        raw_url: format!("{}/webhooks/mail/raw", ctx.origin),
        new_token,
        ctx,
    };
    Ok(crate::routes::client::no_store(render(&page)?))
}

async fn settings(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    render_settings(&state, &tenant, &staff, ctx, None).await
}

async fn save_settings(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let Some(policy) = f.parse::<UnknownSenders>("unknown_senders") else {
        return Ok(failed(&ctx, SETTINGS, ctx.t("inbound.admin.invalid")));
    };
    let department = match f.id::<TicketDepartmentId>("default_department_id") {
        Some(d) => {
            support::get_department(&state.pool, tenant.id, d)
                .await?
                .ok_or(AppError::NotFound)?;
            Some(d)
        }
        None => None,
    };
    let enabled = f.flag("enabled");
    let key = f.text("mailgun_key", 200);
    let clear_key = f.flag("clear_mailgun_key");
    let trust_spf = f.flag("mailgun_trust_spf");
    let authserv = f.text("trusted_authserv_id", 253).to_ascii_lowercase();
    let valid_authserv = authserv.is_empty()
        || (authserv
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && authserv.as_bytes()[0].is_ascii_alphanumeric());
    if !valid_authserv {
        return Ok(failed(&ctx, SETTINGS, ctx.t("inbound.admin.bad_authserv")));
    }
    let mut tx = state.pool.begin().await?;
    mrepo::save_settings(&mut *tx, tenant.id, enabled, policy, department).await?;
    mrepo::save_sender_auth(
        &mut *tx,
        tenant.id,
        Some(authserv.as_str()).filter(|a| !a.is_empty()),
        trust_spf,
    )
    .await?;
    let key_change = if clear_key {
        mrepo::set_mailgun_key(&mut *tx, tenant.id, None).await?;
        Some("cleared")
    } else if !key.is_empty() {
        if key.len() < 16 || key.chars().any(char::is_whitespace) {
            return Ok(failed(&ctx, SETTINGS, ctx.t("inbound.admin.bad_key")));
        }
        let sealed = hos_billing::secrets::seal(
            &state.secret_box,
            &key,
            &hos_billing::secrets::mail_inbound_aad(tenant.id),
        )?;
        mrepo::set_mailgun_key(&mut *tx, tenant.id, Some(&sealed)).await?;
        Some("set")
    } else {
        None
    };
    tx.commit().await?;
    au.log(
        &state,
        "mail_inbound.settings",
        None,
        json!({"enabled": enabled, "unknown_senders": policy, "default_department": department,
               "mailgun_key": key_change, "mailgun_trust_spf": trust_spf,
               "trusted_authserv_id": authserv}),
    )
    .await?;
    Ok(done(&ctx, SETTINGS, "inbound.admin.saved"))
}

async fn new_token(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
) -> AppResult<Response> {
    let token = hos_core::crypto::generate_prefixed_token("hosmail_");
    mrepo::set_raw_token_hash(
        &state.pool,
        tenant.id,
        Some(&hos_core::crypto::hash_token(&token)),
    )
    .await?;
    au.log(
        &state,
        "mail_inbound.raw_token",
        None,
        json!({"action": "created"}),
    )
    .await?;
    render_settings(&state, &tenant, &staff, ctx, Some(token)).await
}

async fn delete_token(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
) -> AppResult<Response> {
    mrepo::set_raw_token_hash(&state.pool, tenant.id, None).await?;
    au.log(
        &state,
        "mail_inbound.raw_token",
        None,
        json!({"action": "deleted"}),
    )
    .await?;
    Ok(done(&ctx, SETTINGS, "inbound.admin.token_deleted"))
}
