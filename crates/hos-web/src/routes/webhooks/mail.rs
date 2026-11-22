//! E-mail piping into tickets, on the tenant host (CSRF-exempt under
//! `/webhooks/`):
//!
//! * `POST /webhooks/mail/mailgun-mime` — a Mailgun inbound route
//!   (`forward("https://<host>/webhooks/mail/mailgun-mime")`; the URL ends
//!   in `mime`, so the raw message comes in `body-mime`). Signed with the
//!   account's HTTP webhook signing key: HMAC-SHA256 of `timestamp + token`,
//!   compared in constant time, the timestamp within
//!   [`inbound::MAILGUN_WINDOW_SECS`], each token honoured once (stored in
//!   `mail_inbound_replay`, so across restarts and processes).
//! * `POST /webhooks/mail/raw` — the raw RFC 5322 message as the body,
//!   `Authorization: Bearer <token>` (only its SHA-256 is stored), optional
//!   `?recipient=` / `?sender=` envelope values.
//!
//! Authentication comes first: a client IP with too many failed calls is
//! answered `429` before anything else, a failed signature / token counts
//! against it, and only authenticated calls take from the per-tenant
//! limit (so forged posts cannot lock out the real ones).
//!
//! A message is threaded into a ticket only by the unguessable reply token
//! in a recipient's plus address (`support+t<token>@…`), and only from the
//! ticket's own people; a ticket number in the subject alone is quarantined
//! (`no_reply_token`). Otherwise it opens a ticket for the client the
//! sender belongs to, but only when sender authentication passed for the
//! From domain ([`inbound::authenticate_sender`]: the raw endpoint's
//! trusted `Authentication-Results`, Mailgun's `X-Mailgun-Spf`); the `From`
//! header alone proves nothing. Unknown and unverified senders follow the
//! tenant setting (quarantine by default), and messages record whether
//! their sender was verified (shown on the ticket). Automated mail
//! (auto-replies, bounces, lists, loops) and floods are
//! rejected, and a Message-ID is processed once (replays and retries).
//! Every message is logged in `mail_inbound_messages`; only quarantined
//! ones keep their text. Attachments must be of an accepted type (by
//! their bytes) and within the limits.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{Duration, Utc};
use hos_auth::ratelimit::RateDecision;
use hos_core::domain::TicketPriority;
use hos_core::ids::{ClientId, TicketDepartmentId, UserId};
use hos_db::enums::{AuthorKind, MessageSource};
use hos_db::repo::clients;
use hos_db::repo::mail_inbound::{
    self as mrepo, InboundSettingsRow, InboundSource, InboundStatus, NewInbound, UnknownSenders,
};
use hos_db::repo::support::{self, NewTicket, NewTicketMessage, TicketRow};
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::{ip_blocked, note_failure, problem};
use crate::error::AppResult;
use crate::extract::ClientIp;
use crate::inbound::{self, ParsedMail};
use crate::routes::client::support::sniff;
use crate::state::AppState;
use crate::tenant::Tenant;

/// Messages from one sender per hour before the rest are refused.
pub const FLOOD_PER_HOUR: i64 = 30;
/// Lifetime of a reply token sent with a ticket e-mail.
pub const REPLY_TOKEN_DAYS: i64 = 120;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/mail/mailgun-mime", post(mailgun))
        .route("/webhooks/mail/raw", post(raw))
}

/// Envelope values given next to the message.
#[derive(Debug, Clone, Default)]
pub struct Envelope {
    pub recipient: Option<String>,
    /// How the sender is authenticated.
    pub check: inbound::SenderCheck,
}

/// What became of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Created(TicketRow),
    Replied(TicketRow),
    Quarantined(&'static str),
    Rejected(&'static str),
    Duplicate,
}

impl Outcome {
    fn json(&self) -> serde_json::Value {
        match self {
            Self::Created(t) => json!({"status": "ticket_created", "ticket": t.number}),
            Self::Replied(t) => json!({"status": "reply_added", "ticket": t.number}),
            Self::Quarantined(r) => json!({"status": "quarantined", "reason": r}),
            Self::Rejected(r) => json!({"status": "rejected", "reason": r}),
            Self::Duplicate => json!({"status": "duplicate"}),
        }
    }
}

fn limited(state: &AppState, tenant: &Tenant) -> Option<Response> {
    match state
        .limits
        .inbound_mail
        .check(&format!("inbound-mail:{}", tenant.id))
    {
        RateDecision::Limited { .. } => {
            Some(problem(StatusCode::TOO_MANY_REQUESTS, "rate_limited", None))
        }
        RateDecision::Allowed { .. } => None,
    }
}

fn failed(e: &crate::error::AppError) -> Response {
    tracing::error!(error = %e, "inbound e-mail processing failed");
    problem(StatusCode::SERVICE_UNAVAILABLE, "unavailable", None)
}

/// Claims a Mailgun token in the database (`mail_inbound_replay`):
/// `false` when it was already used (a replayed request).
async fn claim_token(state: &AppState, tenant: &Tenant, token: &str, now: i64) -> AppResult<bool> {
    let expires = chrono::DateTime::from_timestamp(now + 2 * inbound::MAILGUN_WINDOW_SECS, 0)
        .unwrap_or_else(|| Utc::now() + Duration::hours(1));
    let mut conn = state.pool.acquire().await?;
    Ok(mrepo::claim_replay_token(
        &mut conn,
        tenant.id,
        &hos_core::crypto::sha256_hex(token.as_bytes()),
        expires,
    )
    .await?)
}

/// Gives a token back after a failure, so Mailgun's retry is accepted.
async fn release_token(state: &AppState, tenant: &Tenant, token: &str) {
    if let Err(e) = mrepo::release_replay_token(
        &state.pool,
        tenant.id,
        &hos_core::crypto::sha256_hex(token.as_bytes()),
    )
    .await
    {
        tracing::warn!(tenant = %tenant.id, error = %e, "releasing a mailgun token failed");
    }
}

/// `application/x-www-form-urlencoded` value → bytes (`+` is a space).
fn form_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => out.push(b' '),
            b'%' => match s
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(b) => {
                    out.push(b);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    out
}

/// The fields of a Mailgun route POST this endpoint reads, as raw bytes
/// (the MIME text may be in an 8-bit legacy charset). Mailgun documents
/// `application/x-www-form-urlencoded` for `…mime` URLs and signs both
/// that and `multipart/form-data` posts; both are accepted. `None`: any
/// other content type.
fn mailgun_fields(content_type: &str, body: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    const WANTED: [&str; 6] = [
        "timestamp",
        "token",
        "signature",
        "body-mime",
        "recipient",
        "sender",
    ];
    let ct = content_type.trim().to_ascii_lowercase();
    let mut out = HashMap::new();
    if ct.starts_with("application/x-www-form-urlencoded") {
        for pair in body.split(|b| *b == b'&') {
            let (k, v) = match pair.iter().position(|b| *b == b'=') {
                Some(i) => (&pair[..i], &pair[i + 1..]),
                None => (pair, &[][..]),
            };
            let k = String::from_utf8_lossy(&form_decode(k)).into_owned();
            if WANTED.contains(&k.as_str()) {
                out.insert(k, form_decode(v));
            }
        }
    } else if ct.starts_with("multipart/form-data") {
        let boundary = content_type
            .split(';')
            .map(str::trim)
            .find_map(|p| p.strip_prefix("boundary="))?
            .trim_matches('"');
        for part in crate::routes::admin::common::parse_multipart(body, boundary) {
            if WANTED.contains(&part.name.as_str()) {
                out.insert(part.name, part.data);
            }
        }
    } else {
        return None;
    }
    Some(out)
}

async fn mailgun(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(r) = ip_blocked(&state, &ip) {
        return r;
    }
    let settings = match mrepo::settings(&state.pool, tenant.id).await {
        Ok(s) => s,
        Err(e) => return failed(&e.into()),
    };
    // 406: Mailgun does not retry.
    let Some((settings, sealed)) = settings
        .filter(|s| s.enabled)
        .and_then(|s| s.mailgun_key_enc.clone().map(|k| (s, k)))
    else {
        return problem(StatusCode::NOT_ACCEPTABLE, "not_configured", None);
    };
    let ct = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let Some(fields) = mailgun_fields(ct, &body) else {
        return problem(
            StatusCode::NOT_ACCEPTABLE,
            "use_mime_url",
            Some("forward to the …/mailgun-mime URL"),
        );
    };
    let text = |k: &str| {
        fields
            .get(k)
            .map(|v| String::from_utf8_lossy(v).trim().to_owned())
            .unwrap_or_default()
    };
    let key = match hos_billing::secrets::open(
        &state.secret_box,
        &sealed,
        &hos_billing::secrets::mail_inbound_aad(tenant.id),
    ) {
        Ok(k) => k,
        Err(e) => return failed(&e.into()),
    };
    let now = Utc::now().timestamp();
    let token = text("token");
    if let Err(why) = inbound::verify_mailgun(
        key.expose_str(),
        &text("timestamp"),
        &token,
        &text("signature"),
        now,
    ) {
        tracing::warn!(tenant = %tenant.id, why, "mailgun inbound signature refused");
        note_failure(&state, &ip);
        return problem(StatusCode::UNAUTHORIZED, "invalid_signature", None);
    }
    if let Some(r) = limited(&state, &tenant) {
        return r;
    }
    let Some(raw) = fields.get("body-mime").filter(|b| !b.is_empty()) else {
        return problem(StatusCode::NOT_ACCEPTABLE, "body_mime_missing", None);
    };
    match claim_token(&state, &tenant, &token, now).await {
        Ok(true) => {}
        Ok(false) => return axum::Json(Outcome::Duplicate.json()).into_response(),
        Err(e) => return failed(&e),
    }
    let env = Envelope {
        recipient: inbound::bare_address(&text("recipient")),
        check: if settings.mailgun_trust_spf {
            inbound::SenderCheck::MailgunSpf {
                envelope_sender: inbound::bare_address(&text("sender")),
            }
        } else {
            inbound::SenderCheck::None
        },
    };
    match process(
        &state,
        &tenant,
        &settings,
        InboundSource::Mailgun,
        raw,
        &env,
    )
    .await
    {
        Ok(o) => axum::Json(o.json()).into_response(),
        Err(e) => {
            release_token(&state, &tenant, &token).await;
            failed(&e)
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawQuery {
    #[serde(default)]
    recipient: String,
}

async fn raw(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    headers: HeaderMap,
    Query(q): Query<RawQuery>,
    body: Bytes,
) -> Response {
    if let Some(r) = ip_blocked(&state, &ip) {
        return r;
    }
    let settings = match mrepo::settings(&state.pool, tenant.id).await {
        Ok(s) => s,
        Err(e) => return failed(&e.into()),
    };
    let Some((settings, stored)) = settings
        .filter(|s| s.enabled)
        .and_then(|s| s.raw_token_hash.clone().map(|h| (s, h)))
    else {
        return problem(StatusCode::NOT_FOUND, "not_found", None);
    };
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();
    if bearer.is_empty() || !hos_core::crypto::verify_token_hash(bearer, &stored) {
        note_failure(&state, &ip);
        let mut res = problem(StatusCode::UNAUTHORIZED, "unauthorized", None);
        res.headers_mut().insert(
            http::header::WWW_AUTHENTICATE,
            http::HeaderValue::from_static("Bearer"),
        );
        return res;
    }
    if let Some(r) = limited(&state, &tenant) {
        return r;
    }
    if body.is_empty() {
        return problem(StatusCode::BAD_REQUEST, "empty_body", None);
    }
    let env = Envelope {
        recipient: inbound::bare_address(&q.recipient),
        check: match settings.trusted_authserv_id.as_deref() {
            Some(id) if !id.trim().is_empty() => inbound::SenderCheck::AuthResults(id.to_owned()),
            _ => inbound::SenderCheck::None,
        },
    };
    match process(&state, &tenant, &settings, InboundSource::Raw, &body, &env).await {
        Ok(o) => axum::Json(o.json()).into_response(),
        Err(e) => failed(&e),
    }
}

/// Who a sender is.
#[derive(Debug, Clone)]
enum Sender {
    /// A client user who may open tickets for the client.
    Member(ClientId, UserId),
    /// The account e-mail of a client without a login.
    Client(ClientId),
    Unknown,
}

async fn identify(state: &AppState, tenant: &Tenant, email: &str) -> AppResult<Sender> {
    if let Some((c, u)) = mrepo::ticket_accounts_of(&state.pool, tenant.id, email)
        .await?
        .into_iter()
        .next()
    {
        return Ok(Sender::Member(c, u));
    }
    Ok(
        match mrepo::client_by_email(&state.pool, tenant.id, email).await? {
            Some(c) => Sender::Client(c),
            None => Sender::Unknown,
        },
    )
}

/// Whether `email` may write into `ticket`: its guest address, the client's
/// account address, or a user of the client allowed to use tickets.
async fn may_reply(
    state: &AppState,
    tenant: &Tenant,
    ticket: &TicketRow,
    email: &str,
) -> AppResult<bool> {
    if ticket
        .guest_email
        .as_deref()
        .is_some_and(|g| g.eq_ignore_ascii_case(email))
    {
        return Ok(true);
    }
    let Some(client) = ticket.client_id else {
        return Ok(false);
    };
    if mrepo::ticket_accounts_of(&state.pool, tenant.id, email)
        .await?
        .iter()
        .any(|(c, _)| *c == client)
    {
        return Ok(true);
    }
    Ok(mrepo::client_by_email(&state.pool, tenant.id, email).await? == Some(client))
}

/// Addresses this tenant sends from (mail from them is a loop).
fn own_addresses(tenant: &Tenant, reply_to: Option<&str>) -> Vec<String> {
    [
        tenant.branding.email_from_address.as_deref(),
        tenant.branding.support_email.as_deref(),
        reply_to,
    ]
    .into_iter()
    .flatten()
    .filter_map(inbound::bare_address)
    .collect()
}

/// The `mail.reply_to` setting (a bare address), if set.
async fn reply_to_setting(state: &AppState, tenant: &Tenant) -> AppResult<Option<String>> {
    Ok(
        hos_db::repo::platform::get_setting(&state.pool, tenant.id, "mail.reply_to")
            .await?
            .and_then(|v| v.as_str().and_then(inbound::bare_address)),
    )
}

/// The department of a new ticket: the one whose address is the
/// recipient, else the configured default, else the first visible one.
async fn department_for(
    state: &AppState,
    tenant: &Tenant,
    settings: &InboundSettingsRow,
    recipients: &[String],
) -> AppResult<Option<TicketDepartmentId>> {
    let all = support::list_departments(&state.pool, tenant.id, true).await?;
    for r in recipients {
        let Some(plain) = inbound::without_detail(r) else {
            continue;
        };
        if let Some(d) = all.iter().find(|d| {
            d.email
                .as_deref()
                .and_then(inbound::bare_address)
                .is_some_and(|e| e == plain)
        }) {
            return Ok(Some(d.id));
        }
    }
    if let Some(d) = settings
        .default_department_id
        .filter(|id| all.iter().any(|d| d.id == *id))
    {
        return Ok(Some(d));
    }
    Ok(all.iter().find(|d| !d.is_hidden).map(|d| d.id))
}

fn author_name(mail: &ParsedMail, email: &str) -> String {
    mail.from_name.clone().unwrap_or_else(|| email.to_owned())
}

/// Saves accepted attachments; returns the names of refused ones.
async fn save_attachments(
    state: &AppState,
    tenant: &Tenant,
    ticket: &TicketRow,
    message: hos_core::ids::TicketMessageId,
    mail: &ParsedMail,
) -> AppResult<Vec<String>> {
    let mut refused: Vec<String> = mail
        .attachment_names
        .iter()
        .filter(|n| !mail.attachments.iter().any(|a| &a.filename == *n))
        .cloned()
        .collect();
    for a in &mail.attachments {
        let name = crate::routes::client::support::clean_filename(&a.filename);
        let Some(ct) = sniff(&name, &a.bytes) else {
            refused.push(name);
            continue;
        };
        support::add_attachment_content(
            &state.pool,
            tenant.id,
            ticket.id,
            Some(message),
            &name,
            ct,
            &a.bytes,
            &hos_core::crypto::sha256_hex(&a.bytes),
        )
        .await?;
    }
    Ok(refused)
}

/// A staff-only note listing attachments that were not taken.
async fn note_refused(
    state: &AppState,
    tenant: &Tenant,
    ticket: &TicketRow,
    refused: &[String],
) -> AppResult<()> {
    if refused.is_empty() {
        return Ok(());
    }
    let body = format!(
        "{}: {}",
        state
            .i18n
            .t(tenant.default_locale, "inbound.refused_attachments"),
        refused.join(", ")
    );
    let mut conn = state.pool.acquire().await?;
    support::reply_to_ticket(
        &mut conn,
        tenant.id,
        ticket.id,
        &NewTicketMessage {
            author_kind: AuthorKind::System,
            user_id: None,
            author_name: "HostingOS".into(),
            author_email: None,
            body,
            is_internal: true,
            source: MessageSource::Email,
            message_id: None,
            ip: None,
        },
    )
    .await?;
    Ok(())
}

/// A ticket to open from an e-mail.
pub(crate) struct MailTicket<'a> {
    pub department: TicketDepartmentId,
    pub sender: &'a str,
    pub name: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    pub message_id: Option<String>,
    pub who: ClientOrGuest,
    /// Whether sender authentication passed (shown on the message).
    pub sender_verified: bool,
}

/// Opens a ticket from a message for `sender` (`Guest`: a guest ticket), on
/// `conn` (a caller's transaction makes it atomic with its own changes; the
/// staff notifications are queued on the same connection).
pub(crate) async fn open_ticket_from(
    state: &AppState,
    tenant: &Tenant,
    conn: &mut sqlx::PgConnection,
    mail: MailTicket<'_>,
) -> AppResult<(TicketRow, hos_core::ids::TicketMessageId)> {
    let MailTicket {
        department,
        sender,
        name,
        subject,
        body,
        message_id,
        who,
        sender_verified,
    } = mail;
    let (client_id, user_id, author_kind) = match who {
        ClientOrGuest::Member(c, u) => (Some(c), Some(u), AuthorKind::Client),
        ClientOrGuest::Client(c) => (Some(c), None, AuthorKind::Client),
        ClientOrGuest::Guest => (None, None, AuthorKind::Guest),
    };
    let subject = if subject.trim().is_empty() {
        state.i18n.t(tenant.default_locale, "inbound.no_subject")
    } else {
        subject.trim().to_owned()
    };
    let body = if body.trim().is_empty() {
        state.i18n.t(tenant.default_locale, "inbound.empty_body")
    } else {
        inbound::text_to_markdown(body)
    };
    let (ticket, message) = support::open_ticket(
        &mut *conn,
        tenant.id,
        &NewTicket {
            department_id: department,
            client_id,
            user_id,
            guest_name: (author_kind == AuthorKind::Guest).then(|| name.to_owned()),
            guest_email: (author_kind == AuthorKind::Guest).then(|| sender.to_owned()),
            subject,
            priority: TicketPriority::Medium,
            service_id: None,
            domain_id: None,
            first_message: NewTicketMessage {
                author_kind,
                user_id,
                author_name: name.to_owned(),
                author_email: Some(sender.to_owned()),
                body: body.clone(),
                is_internal: false,
                source: MessageSource::Email,
                message_id,
                ip: None,
            },
        },
    )
    .await?;
    support::set_message_sender_verified(&mut *conn, tenant.id, message.id, sender_verified)
        .await?;
    state
        .billing
        .notify_ticket_staff(conn, tenant.id, ticket.id, true, &body)
        .await?;
    Ok((ticket, message.id))
}

/// The account a new ticket is opened for.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ClientOrGuest {
    Member(ClientId, UserId),
    Client(ClientId),
    Guest,
}

/// Message-IDs being processed in this process (see [`process`]).
struct InFlight(String);

impl InFlight {
    fn set() -> std::sync::MutexGuard<'static, std::collections::HashSet<String>> {
        static SET: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
        SET.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn claim(key: String) -> Option<Self> {
        Self::set().insert(key.clone()).then_some(Self(key))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        Self::set().remove(&self.0);
    }
}

/// Processes one raw message for `tenant` (piping enabled).
pub async fn process(
    state: &AppState,
    tenant: &Tenant,
    settings: &InboundSettingsRow,
    source: InboundSource,
    raw: &[u8],
    env: &Envelope,
) -> AppResult<Outcome> {
    let now = Utc::now();
    let size = i32::try_from(raw.len()).unwrap_or(i32::MAX);
    let synthetic = format!("sha256:{}", hos_core::crypto::sha256_hex(raw));
    let Some(mail) = inbound::parse(raw) else {
        let logged = mrepo::record(
            &state.pool,
            tenant.id,
            &NewInbound {
                source,
                message_id: Some(synthetic),
                sender: None,
                recipient: env.recipient.clone(),
                subject: None,
                status: InboundStatus::Rejected,
                reason: Some("unparseable".into()),
                ticket_id: None,
                body_text: None,
                attachment_names: Vec::new(),
                size_bytes: size,
                sender_verified: None,
            },
        )
        .await?;
        return Ok(match logged {
            Some(_) => Outcome::Rejected("unparseable"),
            None => Outcome::Duplicate,
        });
    };
    let message_id = mail.message_id.clone().unwrap_or(synthetic);
    // A concurrent delivery of the same message (a retry racing the first
    // attempt) is turned away until this one is logged; its retry is then
    // answered as a duplicate instead of opening a second ticket.
    let Some(_in_flight) = InFlight::claim(format!("{}:{message_id}", tenant.id)) else {
        return Err(crate::error::AppError::Unavailable(
            "the same message is being processed".into(),
        ));
    };
    if mrepo::seen(&state.pool, tenant.id, &message_id).await? {
        return Ok(Outcome::Duplicate);
    }
    let mut recipients: Vec<String> = env.recipient.iter().cloned().collect();
    recipients.extend(mail.recipients.iter().cloned());
    // The From header alone proves nothing: only a passing sender
    // authentication for its domain makes the sender a known client.
    let verified = inbound::authenticate_sender(&mail, &env.check).is_some();
    let log = |status: InboundStatus, reason: Option<&'static str>, ticket: Option<&TicketRow>| {
        NewInbound {
            source,
            message_id: Some(message_id.clone()),
            sender: mail.from_email.clone(),
            recipient: recipients.first().cloned(),
            subject: Some(mail.subject.clone()).filter(|s| !s.is_empty()),
            status,
            reason: reason.map(str::to_owned),
            ticket_id: ticket.map(|t| t.id),
            // Only quarantined mail keeps its text (for the review).
            body_text: (status == InboundStatus::Quarantined).then(|| mail.body.clone()),
            attachment_names: mail.attachment_names.clone(),
            size_bytes: size,
            sender_verified: Some(verified),
        }
    };
    let finish = |row: NewInbound, outcome: Outcome| async move {
        match mrepo::record(&state.pool, tenant.id, &row).await? {
            Some(_) => Ok::<Outcome, crate::error::AppError>(outcome),
            None => Ok(Outcome::Duplicate),
        }
    };
    let Some(sender) = mail.from_email.clone() else {
        return finish(
            log(InboundStatus::Rejected, Some("no_sender"), None),
            Outcome::Rejected("no_sender"),
        )
        .await;
    };
    if let Some(reason) = mail.automated {
        return finish(
            log(InboundStatus::Rejected, Some(reason), None),
            Outcome::Rejected(reason),
        )
        .await;
    }
    let reply_to = reply_to_setting(state, tenant).await?;
    if own_addresses(tenant, reply_to.as_deref())
        .iter()
        .any(|a| inbound::without_detail(a) == inbound::without_detail(&sender))
    {
        return finish(
            log(InboundStatus::Rejected, Some("loop"), None),
            Outcome::Rejected("loop"),
        )
        .await;
    }
    if mrepo::count_from(&state.pool, tenant.id, &sender, now - Duration::hours(1)).await?
        >= FLOOD_PER_HOUR
    {
        return finish(
            log(InboundStatus::Rejected, Some("flood"), None),
            Outcome::Rejected("flood"),
        )
        .await;
    }

    // ---- threading: reply token, then the ticket number in the subject
    let mut thread: Option<TicketRow> = None;
    // The reply came back from the address its token was sent to.
    let mut token_recipient = false;
    for r in &recipients {
        let Some(token) = inbound::reply_token(r) else {
            continue;
        };
        let hash = hos_core::crypto::hash_token(&token);
        if let Some((ticket_id, sent_to)) =
            mrepo::ticket_for_token(&state.pool, tenant.id, &hash, now).await?
            && let Some(t) = support::get_ticket(&state.pool, tenant.id, ticket_id).await?
        {
            token_recipient = sent_to
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(&sender));
            let allowed = token_recipient || may_reply(state, tenant, &t, &sender).await?;
            if !allowed {
                return finish(
                    log(
                        InboundStatus::Quarantined,
                        Some("sender_mismatch"),
                        Some(&t),
                    ),
                    Outcome::Quarantined("sender_mismatch"),
                )
                .await;
            }
            thread = Some(t);
            break;
        }
    }
    // A ticket number in the subject is guessable: without the reply token
    // the message is kept for staff, who may append it.
    if thread.is_none()
        && let Some(number) = inbound::ticket_number_in(&mail.subject)
        && let Some(t) = support::ticket_by_number(&state.pool, tenant.id, &number).await?
    {
        return finish(
            log(InboundStatus::Quarantined, Some("no_reply_token"), Some(&t)),
            Outcome::Quarantined("no_reply_token"),
        )
        .await;
    }

    if let Some(t) = thread {
        let body = if mail.body.trim().is_empty() {
            state.i18n.t(tenant.default_locale, "inbound.empty_body")
        } else {
            inbound::text_to_markdown(&mail.body)
        };
        let user_id = if verified || token_recipient {
            match identify(state, tenant, &sender).await? {
                Sender::Member(c, u) if Some(c) == t.client_id => Some(u),
                _ => None,
            }
        } else {
            None
        };
        let mut conn = state.pool.acquire().await?;
        let (msg, ticket) = support::reply_to_ticket(
            &mut conn,
            tenant.id,
            t.id,
            &NewTicketMessage {
                author_kind: if t.client_id.is_some() {
                    AuthorKind::Client
                } else {
                    AuthorKind::Guest
                },
                user_id,
                author_name: author_name(&mail, &sender),
                author_email: Some(sender.clone()),
                body: body.clone(),
                is_internal: false,
                source: MessageSource::Email,
                message_id: Some(message_id.clone()),
                ip: None,
            },
        )
        .await?;
        support::set_message_sender_verified(&mut *conn, tenant.id, msg.id, verified).await?;
        state
            .billing
            .notify_ticket_staff(&mut conn, tenant.id, ticket.id, false, &body)
            .await?;
        drop(conn);
        let refused = save_attachments(state, tenant, &ticket, msg.id, &mail).await?;
        note_refused(state, tenant, &ticket, &refused).await?;
        return finish(
            log(InboundStatus::ReplyAdded, None, Some(&ticket)),
            Outcome::Replied(ticket),
        )
        .await;
    }

    // ---- a new ticket
    let (known, unknown_reason) = match (identify(state, tenant, &sender).await?, verified) {
        (Sender::Unknown, _) => (Sender::Unknown, "unknown_sender"),
        // A client's address, but nothing proves the client sent it.
        (_, false) => (Sender::Unknown, "sender_not_verified"),
        (k, true) => (k, "unknown_sender"),
    };
    let who = match known {
        Sender::Member(c, u) => ClientOrGuest::Member(c, u),
        Sender::Client(c) => ClientOrGuest::Client(c),
        Sender::Unknown => match settings.unknown_senders {
            UnknownSenders::Guest => ClientOrGuest::Guest,
            UnknownSenders::Quarantine => {
                return finish(
                    log(InboundStatus::Quarantined, Some(unknown_reason), None),
                    Outcome::Quarantined(unknown_reason),
                )
                .await;
            }
            UnknownSenders::Reject => {
                return finish(
                    log(InboundStatus::Rejected, Some(unknown_reason), None),
                    Outcome::Rejected(unknown_reason),
                )
                .await;
            }
        },
    };
    let Some(department) = department_for(state, tenant, settings, &recipients).await? else {
        return finish(
            log(InboundStatus::Quarantined, Some("no_department"), None),
            Outcome::Quarantined("no_department"),
        )
        .await;
    };
    let name = author_name(&mail, &sender);
    let mut conn = state.pool.acquire().await?;
    let (ticket, msg) = open_ticket_from(
        state,
        tenant,
        &mut conn,
        MailTicket {
            department,
            sender: &sender,
            name: &name,
            subject: &mail.subject,
            body: &mail.body,
            message_id: Some(message_id.clone()),
            who,
            sender_verified: verified,
        },
    )
    .await?;
    drop(conn);
    let refused = save_attachments(state, tenant, &ticket, msg, &mail).await?;
    note_refused(state, tenant, &ticket, &refused).await?;
    finish(
        log(InboundStatus::TicketCreated, None, Some(&ticket)),
        Outcome::Created(ticket),
    )
    .await
}

/// A reply token for an e-mail about `ticket` to `recipient`: the Reply-To
/// address carrying it, when piping is enabled and a base address
/// (`mail.reply_to` setting, else the support address) is known.
pub async fn reply_address_for(
    state: &AppState,
    tenant: &Tenant,
    ticket: &TicketRow,
    recipient: &str,
) -> AppResult<Option<String>> {
    if !mrepo::settings(&state.pool, tenant.id)
        .await?
        .is_some_and(|s| s.enabled)
    {
        return Ok(None);
    }
    let base = match reply_to_setting(state, tenant).await? {
        Some(r) => Some(r),
        None => tenant
            .branding
            .support_email
            .as_deref()
            .and_then(inbound::bare_address),
    };
    let Some(base) = base else {
        return Ok(None);
    };
    let token = inbound::new_reply_token();
    let Some(address) = inbound::reply_address(&base, &token) else {
        return Ok(None);
    };
    mrepo::create_reply_token(
        &state.pool,
        tenant.id,
        ticket.id,
        &hos_core::crypto::hash_token(&token),
        recipient,
        Utc::now() + Duration::days(REPLY_TOKEN_DAYS),
    )
    .await?;
    Ok(Some(address))
}

/// Client account lookups used by the admin release action.
pub(crate) async fn sender_account(
    state: &AppState,
    tenant: &Tenant,
    email: &str,
) -> AppResult<ClientOrGuest> {
    Ok(match identify(state, tenant, email).await? {
        Sender::Member(c, u) => ClientOrGuest::Member(c, u),
        Sender::Client(c) => ClientOrGuest::Client(c),
        Sender::Unknown => ClientOrGuest::Guest,
    })
}

/// Display name of the client a ticket would go to (admin review).
pub(crate) async fn account_label(
    state: &AppState,
    tenant: &Tenant,
    who: ClientOrGuest,
) -> AppResult<Option<String>> {
    let id = match who {
        ClientOrGuest::Member(c, _) | ClientOrGuest::Client(c) => c,
        ClientOrGuest::Guest => return Ok(None),
    };
    Ok(clients::get_client(&state.pool, tenant.id, id)
        .await?
        .map(|c| c.display_name()))
}
