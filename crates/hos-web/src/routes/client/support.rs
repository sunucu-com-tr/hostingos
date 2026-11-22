//! Support tickets: list, new ticket (department, priority, related service
//! or domain, Markdown body, attachments), thread with replies, close /
//! reopen, satisfaction rating, attachment download and knowledge-base
//! suggestions while typing the subject.
//!
//! Attachments are stored in the database (`ticket_attachments.content`).
//! Their type is decided by sniffing the bytes (images, PDF, ZIP, plain
//! text); the browser's claim is ignored. Downloads are always
//! `Content-Disposition: attachment` with `nosniff`.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Multipart, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hos_core::domain::{TicketPriority, TicketStatus};
use hos_core::ids::{DomainId, ServiceId, TicketDepartmentId, TicketId};
use hos_db::enums::{AuthorKind, MessageSource};
use hos_db::ids::TicketAttachmentId;
use hos_db::repo::content::{self, KbArticleRow};
use hos_db::repo::domains::DomainRow;
use hos_db::repo::services::ServiceListRow;
use hos_db::repo::support::{
    self, NewTicket, NewTicketMessage, TicketAttachmentRow, TicketDepartmentRow, TicketFilter,
    TicketListRow, TicketMessageRow, TicketRow,
};
use hos_db::repo::{domains as domain_repo, services as service_repo};
use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};
use serde::Deserialize;

use super::{Fmt as _, MemberAccess, audit_client, parse_id, setup};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, PageQuery, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, Pager, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/tickets", get(list).post(create))
        .route("/client/tickets/new", get(new_form))
        .route("/client/tickets/{id}", get(show))
        .route("/client/tickets/{id}/reply", post(reply))
        .route("/client/tickets/{id}/close", post(close))
        .route("/client/tickets/{id}/reopen", post(reopen))
        .route("/client/tickets/{id}/rate", post(rate))
        .route("/client/tickets/{id}/attachments/{aid}", get(download))
        .route("/client/kb-suggest", get(kb_suggest))
}

/// Files per message.
pub const MAX_FILES: usize = 3;
/// Bytes per file (the request body limit is 2 MiB in total).
pub const MAX_FILE_BYTES: usize = 600 * 1024;
const MAX_BODY_CHARS: usize = 20_000;

/// Content type of an accepted upload, from its bytes (and, for plain
/// text, its extension). `None`: rejected.
pub fn sniff(name: &str, bytes: &[u8]) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"PK\x03\x04") && lower.ends_with(".zip") {
        Some("application/zip")
    } else if (lower.ends_with(".txt") || lower.ends_with(".log"))
        && std::str::from_utf8(bytes).is_ok_and(|s| !s.contains('\0'))
    {
        Some("text/plain; charset=utf-8")
    } else {
        None
    }
}

/// A safe display / download file name.
pub fn clean_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_owned();
    let cleaned: String = cleaned
        .chars()
        .rev()
        .take(100)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if cleaned.is_empty() {
        "file".to_owned()
    } else {
        cleaned
    }
}

async fn own_ticket(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    id: &str,
) -> AppResult<TicketRow> {
    let id: TicketId = parse_id(id)?;
    support::get_ticket(&state.pool, tenant.id, id)
        .await?
        .filter(|t| t.client_id == Some(client.client_id))
        .ok_or(AppError::NotFound)
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    #[serde(default)]
    show: String,
}

#[derive(Template)]
#[template(path = "client/tickets.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<TicketListRow>,
    pager: Pager,
    show: String,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    pq: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let show = match f.show.as_str() {
        "closed" | "all" => f.show.clone(),
        _ => "open".to_owned(),
    };
    let paged = support::list_tickets(
        &state.pool,
        tenant.id,
        &TicketFilter {
            client_id: Some(client.client_id),
            open_only: show == "open",
            status: (show == "closed").then_some(TicketStatus::Closed),
            ..TicketFilter::default()
        },
        pq.db_page(),
    )
    .await?;
    let pager = Pager::new(
        &paged,
        &ctx.url("/client/tickets"),
        &[("show", show.as_str())],
    );
    let title = ctx.t("client.tickets.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    render(&ListPage {
        ctx,
        rows: paged.items,
        pager,
        show,
    })
}

// ---------------------------------------------------------------------------
// New ticket
// ---------------------------------------------------------------------------

/// Parsed ticket / reply form (multipart).
#[derive(Debug, Default)]
pub struct TicketInput {
    /// Department id.
    pub department: String,
    /// Priority.
    pub priority: String,
    /// Related `service:<id>` / `domain:<id>`.
    pub related: String,
    /// Subject.
    pub subject: String,
    /// Markdown body.
    pub body: String,
    /// Uploads `(name, content type, bytes)`.
    pub files: Vec<(String, &'static str, Vec<u8>)>,
}

/// Reads a multipart ticket form; upload problems become field errors.
async fn read_form(
    ctx: &PageCtx,
    mut mp: Multipart,
    errors: &mut FormErrors,
) -> AppResult<TicketInput> {
    let mut input = TicketInput::default();
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(format!("multipart: {e}")))?
    {
        let name = field.name().unwrap_or("").to_owned();
        if name == "files" {
            let file_name = field.file_name().unwrap_or("").to_owned();
            let bytes = field
                .bytes()
                .await
                .map_err(|e| AppError::bad_request(format!("multipart: {e}")))?;
            if file_name.is_empty() && bytes.is_empty() {
                continue;
            }
            if input.files.len() >= MAX_FILES {
                errors.add(
                    "files",
                    ctx.tv("client.tickets.too_many_files", "max", MAX_FILES),
                );
                continue;
            }
            if bytes.len() > MAX_FILE_BYTES {
                errors.add(
                    "files",
                    ctx.tv(
                        "client.tickets.file_too_large",
                        "name",
                        clean_filename(&file_name),
                    ),
                );
                continue;
            }
            match sniff(&file_name, &bytes)
                .filter(|ct| crate::demo::upload_type_allowed(&ctx.state, ct))
            {
                Some(ct) => input
                    .files
                    .push((clean_filename(&file_name), ct, bytes.to_vec())),
                None => errors.add(
                    "files",
                    ctx.tv(
                        "client.tickets.file_type",
                        "name",
                        clean_filename(&file_name),
                    ),
                ),
            }
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|e| AppError::bad_request(format!("multipart: {e}")))?;
        match name.as_str() {
            "department" => input.department = value,
            "priority" => input.priority = value,
            "related" => input.related = value,
            "subject" => input.subject = value,
            "body" => input.body = value,
            _ => {}
        }
    }
    Ok(input)
}

#[derive(Template)]
#[template(path = "client/ticket_new.html")]
struct NewPage {
    ctx: PageCtx,
    departments: Vec<TicketDepartmentRow>,
    priorities: Vec<(String, String)>,
    related: Vec<(String, String)>,
    input: TicketInput,
    errors: FormErrors,
    max_files: usize,
    max_kb: usize,
}
crate::page!(NewPage);

async fn new_page(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    input: TicketInput,
    errors: FormErrors,
) -> AppResult<NewPage> {
    let departments = support::list_departments(&state.pool, tenant.id, false).await?;
    let mut related = Vec::new();
    if access.allows("services") {
        let list: Vec<ServiceListRow> =
            service_repo::client_services(&state.pool, tenant.id, client.client_id).await?;
        related.extend(list.into_iter().map(|s| {
            let label = match &s.service.domain {
                Some(d) => format!("{} — {}", s.product_name, hos_domains::name::to_unicode(d)),
                None => s.product_name.clone(),
            };
            (format!("service:{}", s.service.id), label)
        }));
    }
    if access.allows("domains") {
        let list: Vec<DomainRow> =
            domain_repo::client_domains(&state.pool, tenant.id, client.client_id).await?;
        related.extend(list.into_iter().map(|d| {
            (
                format!("domain:{}", d.id),
                hos_domains::name::to_unicode(&d.name),
            )
        }));
    }
    let priorities = ["low", "medium", "high", "urgent"]
        .iter()
        .map(|p| {
            (
                (*p).to_owned(),
                ctx.t(&format!("client.tickets.priority_{p}")),
            )
        })
        .collect();
    let title = ctx.t("client.tickets.new_title");
    let crumbs = vec![
        Crumb::link(ctx.t("client.tickets.title"), ctx.url("/client/tickets")),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, client, access, title, crumbs);
    Ok(NewPage {
        ctx,
        departments,
        priorities,
        related,
        input,
        errors,
        max_files: MAX_FILES,
        max_kb: MAX_FILE_BYTES / 1024,
    })
}

#[derive(Debug, Default, Deserialize)]
struct NewQuery {
    #[serde(default)]
    related: String,
}

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Query(q): Query<NewQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let input = TicketInput {
        priority: "medium".into(),
        related: q.related,
        ..TicketInput::default()
    };
    let page = new_page(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        input,
        FormErrors::default(),
    )
    .await?;
    render(&page)
}

fn author_name(client: &CurrentClient) -> String {
    format!("{} {}", client.user.first_name, client.user.last_name)
        .trim()
        .to_owned()
}

async fn save_files(
    state: &AppState,
    tenant: &Tenant,
    ticket: TicketId,
    message: hos_core::ids::TicketMessageId,
    files: &[(String, &'static str, Vec<u8>)],
) -> AppResult<()> {
    for (name, ct, bytes) in files {
        let sha = hos_core::crypto::sha256_hex(bytes);
        support::add_attachment_content(
            &state.pool,
            tenant.id,
            ticket,
            Some(message),
            name,
            ct,
            bytes,
            &sha,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    mp: Multipart,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let input = read_form(&ctx, mp, &mut errors).await?;
    let departments = support::list_departments(&state.pool, tenant.id, false).await?;
    let department: Option<TicketDepartmentId> = input
        .department
        .parse()
        .ok()
        .filter(|d| departments.iter().any(|x| x.id == *d));
    if department.is_none() {
        errors.add("department", ctx.t("client.common.required"));
    }
    let subject = forms::clean(&input.subject, 200);
    if subject.is_empty() {
        errors.add("subject", ctx.t("client.common.required"));
    }
    let body = input.body.trim().to_owned();
    if body.is_empty() {
        errors.add("body", ctx.t("client.common.required"));
    } else if body.chars().count() > MAX_BODY_CHARS {
        errors.add("body", ctx.t("client.tickets.body_too_long"));
    }
    let priority: TicketPriority = input.priority.parse().unwrap_or(TicketPriority::Medium);
    let (mut service_id, mut domain_id): (Option<ServiceId>, Option<DomainId>) = (None, None);
    if let Some(raw) = input.related.strip_prefix("service:") {
        service_id = raw.parse().ok();
        let ok = match service_id {
            Some(s) => service_repo::get_service(&state.pool, tenant.id, s)
                .await?
                .is_some_and(|s| s.client_id == client.client_id),
            None => false,
        };
        if !ok {
            return Err(AppError::NotFound);
        }
    } else if let Some(raw) = input.related.strip_prefix("domain:") {
        domain_id = raw.parse().ok();
        let ok = match domain_id {
            Some(d) => domain_repo::get_domain(&state.pool, tenant.id, d)
                .await?
                .is_some_and(|d| d.client_id == client.client_id),
            None => false,
        };
        if !ok {
            return Err(AppError::NotFound);
        }
    }
    let (Some(department), false) = (department, errors.any()) else {
        let page = new_page(&state, &tenant, &client, &access, ctx, input, errors).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    let excerpt = body.clone();
    let mut conn = state.pool.acquire().await?;
    let (ticket, message) = support::open_ticket(
        &mut conn,
        tenant.id,
        &NewTicket {
            department_id: department,
            client_id: Some(client.client_id),
            user_id: Some(client.user_id),
            guest_name: None,
            guest_email: None,
            subject,
            priority,
            service_id,
            domain_id,
            first_message: NewTicketMessage {
                author_kind: AuthorKind::Client,
                user_id: Some(client.user_id),
                author_name: author_name(&client),
                author_email: Some(client.user.email.clone()),
                body,
                is_internal: false,
                source: MessageSource::Web,
                message_id: None,
                ip: Some(ip.0),
            },
        },
    )
    .await?;
    // Department staff: in-app notification + e-mail.
    state
        .billing
        .notify_ticket_staff(&mut conn, tenant.id, ticket.id, true, &excerpt)
        .await?;
    drop(conn);
    save_files(&state, &tenant, ticket.id, message.id, &input.files).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.ticket.opened",
        Some(("ticket", ticket.id.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url(format!("/client/tickets/{}", ticket.id)),
        SetFlash::success(ctx.tv("client.tickets.opened", "number", &ticket.number)),
    ))
}

// ---------------------------------------------------------------------------
// Thread
// ---------------------------------------------------------------------------

/// A message with its rendered body and files.
pub struct MessageView {
    /// Row.
    pub m: TicketMessageRow,
    /// Sanitized HTML.
    pub html: String,
    /// Attachments.
    pub files: Vec<TicketAttachmentRow>,
}

#[derive(Template)]
#[template(path = "client/ticket.html")]
struct ThreadPage {
    ctx: PageCtx,
    tk: TicketRow,
    department: String,
    related: String,
    related_href: String,
    messages: Vec<MessageView>,
    errors: FormErrors,
    body: String,
    max_files: usize,
    max_kb: usize,
}
crate::page!(ThreadPage);

impl ThreadPage {
    fn closed(&self) -> bool {
        self.tk.status == TicketStatus::Closed
    }
    fn size(&self, f: &TicketAttachmentRow) -> String {
        if f.size_bytes >= 1024 * 1024 {
            format!("{:.1} MB", f.size_bytes as f64 / 1_048_576.0)
        } else {
            format!("{} KB", (f.size_bytes + 1023) / 1024)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn thread_page(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    t: TicketRow,
    errors: FormErrors,
    body: String,
) -> AppResult<ThreadPage> {
    let messages = support::ticket_messages(&state.pool, tenant.id, t.id, false).await?;
    let attachments = support::ticket_attachments(&state.pool, tenant.id, t.id).await?;
    let department = support::list_departments(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .find(|d| d.id == t.department_id)
        .map(|d| d.name)
        .unwrap_or_default();
    let (related, related_href) = if let Some(sid) = t.service_id {
        match service_repo::get_service(&state.pool, tenant.id, sid).await? {
            Some(s) => (
                s.domain
                    .clone()
                    .unwrap_or_else(|| ctx.t("client.tickets.related_service")),
                ctx.url(format!("/client/services/{sid}")),
            ),
            None => (String::new(), String::new()),
        }
    } else if let Some(did) = t.domain_id {
        match domain_repo::get_domain(&state.pool, tenant.id, did).await? {
            Some(d) => (
                hos_domains::name::to_unicode(&d.name),
                ctx.url(format!("/client/domains/{did}")),
            ),
            None => (String::new(), String::new()),
        }
    } else {
        (String::new(), String::new())
    };
    let messages = messages
        .into_iter()
        .map(|m| MessageView {
            html: super::user_markdown(state, &m.body),
            files: attachments
                .iter()
                .filter(|a| a.message_id == Some(m.id))
                .cloned()
                .collect(),
            m,
        })
        .collect();
    let title = format!("#{} {}", t.number, t.subject);
    let crumbs = vec![
        Crumb::link(ctx.t("client.tickets.title"), ctx.url("/client/tickets")),
        Crumb::current(format!("#{}", t.number)),
    ];
    setup(&mut ctx, client, access, title, crumbs);
    Ok(ThreadPage {
        ctx,
        tk: t,
        department,
        related,
        related_href,
        messages,
        errors,
        body,
        max_files: MAX_FILES,
        max_kb: MAX_FILE_BYTES / 1024,
    })
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let t = own_ticket(&state, &tenant, &client, &id).await?;
    let page = thread_page(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        t,
        FormErrors::default(),
        String::new(),
    )
    .await?;
    render(&page)
}

#[allow(clippy::too_many_arguments)]
async fn reply(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    mp: Multipart,
) -> AppResult<Response> {
    let t = own_ticket(&state, &tenant, &client, &id).await?;
    let mut errors = FormErrors::default();
    let input = read_form(&ctx, mp, &mut errors).await?;
    let body = input.body.trim().to_owned();
    if body.is_empty() {
        errors.add("body", ctx.t("client.common.required"));
    } else if body.chars().count() > MAX_BODY_CHARS {
        errors.add("body", ctx.t("client.tickets.body_too_long"));
    }
    if errors.any() {
        let page = thread_page(
            &state, &tenant, &client, &access, ctx, t, errors, input.body,
        )
        .await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let excerpt = body.clone();
    let mut conn = state.pool.acquire().await?;
    let (message, _) = support::reply_to_ticket(
        &mut conn,
        tenant.id,
        t.id,
        &NewTicketMessage {
            author_kind: AuthorKind::Client,
            user_id: Some(client.user_id),
            author_name: author_name(&client),
            author_email: Some(client.user.email.clone()),
            body,
            is_internal: false,
            source: MessageSource::Web,
            message_id: None,
            ip: Some(ip.0),
        },
    )
    .await?;
    state
        .billing
        .notify_ticket_staff(&mut conn, tenant.id, t.id, false, &excerpt)
        .await?;
    drop(conn);
    save_files(&state, &tenant, t.id, message.id, &input.files).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.ticket.replied",
        Some(("ticket", t.id.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url(format!("/client/tickets/{}#m-{}", t.id, message.id)),
        SetFlash::success(ctx.t("client.tickets.replied")),
    ))
}

async fn close(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let t = own_ticket(&state, &tenant, &client, &id).await?;
    support::set_ticket_status(&state.pool, tenant.id, t.id, TicketStatus::Closed).await?;
    Ok(flash::redirect(
        &ctx.url(format!("/client/tickets/{}", t.id)),
        SetFlash::success(ctx.t("client.tickets.closed")),
    ))
}

async fn reopen(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let t = own_ticket(&state, &tenant, &client, &id).await?;
    support::set_ticket_status(&state.pool, tenant.id, t.id, TicketStatus::CustomerReply).await?;
    Ok(flash::redirect(
        &ctx.url(format!("/client/tickets/{}", t.id)),
        SetFlash::success(ctx.t("client.tickets.reopened")),
    ))
}

#[derive(Debug, Deserialize)]
struct RateForm {
    rating: i16,
}

async fn rate(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<RateForm>,
) -> AppResult<Response> {
    let t = own_ticket(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/tickets/{}", t.id));
    if !(1..=5).contains(&f.rating) || t.status != TicketStatus::Closed {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.tickets.rating_invalid")),
        ));
    }
    support::set_ticket_rating(&state.pool, tenant.id, t.id, f.rating).await?;
    Ok(flash::redirect(
        &back,
        SetFlash::success(ctx.t("client.tickets.rated")),
    ))
}

/// `GET /client/tickets/{id}/attachments/{aid}`.
async fn download(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path((id, aid)): Path<(String, String)>,
) -> AppResult<Response> {
    let t = own_ticket(&state, &tenant, &client, &id).await?;
    let aid: TicketAttachmentId = parse_id(&aid)?;
    // Files of internal staff notes are never served to the client.
    let (row, bytes) = support::public_attachment_content(&state.pool, tenant.id, t.id, aid)
        .await?
        .ok_or(AppError::NotFound)?;
    let name = clean_filename(&row.filename);
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
    let disposition = format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        encoded.replace('+', "%20")
    );
    let content_type = if sniff(&row.filename, &bytes).is_some() {
        row.content_type.clone()
    } else {
        "application/octet-stream".to_owned()
    };
    let mut res = bytes.into_response();
    let h = res.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("attachment")),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    Ok(res)
}

// ---------------------------------------------------------------------------
// KB suggestions
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct SuggestQuery {
    #[serde(default)]
    subject: String,
}

#[derive(Template)]
#[template(path = "client/_kb_suggest.html")]
struct SuggestFragment {
    ctx: PageCtx,
    articles: Vec<KbArticleRow>,
}
crate::page!(SuggestFragment);

/// `GET /client/kb-suggest?subject=…` (htmx while typing the subject).
async fn kb_suggest(
    State(state): State<AppState>,
    tenant: Tenant,
    Query(q): Query<SuggestQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let term = q.subject.trim();
    let articles = if term.chars().count() >= 3 {
        content::search_kb(
            &state.pool,
            tenant.id,
            ctx.locale,
            &forms::clean(term, 100),
            5,
        )
        .await?
    } else {
        Vec::new()
    };
    render(&SuggestFragment { ctx, articles })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        assert_eq!(sniff("a.png", b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff("a.pdf", b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(
            sniff("notes.txt", b"hello"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(sniff("x.html", b"<script>alert(1)</script>"), None);
        assert_eq!(sniff("x.txt", b"a\0b"), None);
        assert_eq!(sniff("x.exe", b"MZ\x90\x00"), None);
    }

    #[test]
    fn filenames() {
        assert_eq!(clean_filename("../../etc/passwd"), "passwd");
        assert_eq!(clean_filename("C:\\x\\rapor 1.pdf"), "rapor 1.pdf");
        assert_eq!(clean_filename("<b>.png"), "_b_.png");
        assert_eq!(clean_filename(".."), "file");
    }
}
