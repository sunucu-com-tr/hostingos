//! `/admin/support/*` — the support desk: ticket queue (department / status
//! / priority / assignee filters, search, paging, bulk close / assign),
//! ticket detail (thread with internal notes, reply with canned responses
//! and attachments, status / priority / department / assignee changes, the
//! client context sidebar and an **AI reply draft** that is only ever put
//! into the editable reply box), tickets opened on behalf of a client,
//! departments and canned responses.
//!
//! Status changes go through the ticket state machine
//! (`hos_db::repo::support::set_ticket_status` → `TicketStatus::transition`).
//! Staff replies queue the `ticket_reply` e-mail; internal notes never leave
//! the admin (the client area reads messages with `include_internal = false`).

use std::collections::HashMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hos_auth::rbac::Permission;
use hos_core::domain::{InvoiceStatus, Locale, TicketPriority, TicketStatus, UserKind};
use hos_core::ids::{ClientId, TicketDepartmentId, TicketId, UserId};
use hos_db::Page;
use hos_db::enums::{AiGenerationStatus, AiPurpose, AuthorKind, MessageSource};
use hos_db::ids::{AiGenerationId, CannedResponseId, TicketAttachmentId};
use hos_db::repo::billing_support;
use hos_db::repo::clients::{self, ClientFilter, ClientRow};
use hos_db::repo::content as content_repo;
use hos_db::repo::identity;
use hos_db::repo::invoices::{self, InvoiceFilter, InvoiceListRow};
use hos_db::repo::seo::AiGenerationRow;
use hos_db::repo::services::{self, ServiceListRow};
use hos_db::repo::support::{
    self, CannedResponseRow, DepartmentInput, NewTicket, NewTicketMessage, TicketAttachmentRow,
    TicketDepartmentRow, TicketFilter, TicketListRow, TicketMessageRow, TicketRow,
};
use hos_seo::ai_tasks::{KbSnippet, SupportContext, TicketAuthor, TicketMessage};
use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::ai_studio::{self, StartError};
use super::common::{self, Audit, FormData, Multipart, Ui as _};
use super::content::valid_slug;
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentStaff, Htmx, PageQuery, RequirePermission, perm};
use crate::forms::{self, FormErrors};
use crate::mail::{self, Recipient};
use crate::routes::client::support::{MAX_FILE_BYTES, MAX_FILES, clean_filename, sniff};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/support/tickets", get(list))
        .route("/admin/support/tickets/bulk", post(bulk))
        .route("/admin/support/tickets/new", get(new_form).post(create))
        .route("/admin/support/tickets/{id}", get(show))
        .route("/admin/support/tickets/{id}/reply", post(reply))
        .route("/admin/support/tickets/{id}/update", post(update))
        .route(
            "/admin/support/tickets/{id}/attachments/{aid}",
            get(download),
        )
        .route("/admin/support/tickets/{id}/ai-draft", post(ai_draft))
        .route(
            "/admin/support/tickets/{id}/ai-draft/{gid}",
            get(ai_draft_status),
        )
        .route(
            "/admin/support/departments",
            get(departments).post(create_department),
        )
        .route(
            "/admin/support/departments/{id}",
            get(edit_department).post(update_department),
        )
        .route(
            "/admin/support/departments/{id}/delete",
            post(delete_department),
        )
        .route("/admin/support/canned", get(canned).post(create_canned))
        .route(
            "/admin/support/canned/{id}",
            get(edit_canned).post(update_canned),
        )
        .route("/admin/support/canned/{id}/delete", post(delete_canned))
}

const MAX_BODY_CHARS: usize = 20_000;

fn priority_options(ctx: &PageCtx) -> Vec<(String, String)> {
    common::enum_options(ctx, TicketPriority::ALL, |p| p.as_str(), "admin.status")
}

fn status_options(ctx: &PageCtx) -> Vec<(String, String)> {
    common::enum_options(ctx, TicketStatus::ALL, |s| s.as_str(), "admin.status")
}

async fn staff_names(state: &AppState, tenant: &Tenant) -> AppResult<Vec<(UserId, String)>> {
    Ok(identity::list_staff(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|u| (u.id, u.display_name()))
        .collect())
}

async fn load_ticket(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<TicketRow> {
    support::get_ticket(&state.pool, tenant.id, TicketId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    #[serde(default)]
    status: String,
    #[serde(default)]
    department: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    assigned: String,
}

/// A queue chip (`status` filter shortcut).
pub struct Chip {
    pub value: String,
    pub label: String,
    pub count: i64,
    pub href: String,
}

/// A queue row with the assignee name.
pub struct QueueRow {
    pub t: TicketListRow,
    pub assignee: String,
}

#[derive(Template)]
#[template(path = "admin/support/tickets.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<QueueRow>,
    pager: Pager,
    chips: Vec<Chip>,
    q: String,
    status: String,
    department: String,
    priority: String,
    assigned: String,
    departments: Vec<TicketDepartmentRow>,
    priorities: Vec<(String, String)>,
    staff: Vec<(String, String)>,
    back: String,
    can_manage: bool,
    me: String,
}
crate::page!(ListPage);

/// Status filter value of "every open ticket" (the default chip). Distinct
/// from `open`, the status of new tickets, which has its own chip.
const OPEN_ANY: &str = "active";

impl ListFilter {
    fn to_filter(&self, q: &PageQuery, me: UserId) -> TicketFilter {
        let mut f = TicketFilter {
            search: q.q.clone().filter(|s| !s.trim().is_empty()),
            department_id: Uuid::parse_str(&self.department)
                .ok()
                .map(TicketDepartmentId::from),
            priority: self.priority.parse().ok(),
            ..TicketFilter::default()
        };
        match self.status.as_str() {
            // Every open state (the default). `open` alone is the `open`
            // ticket status, like every other status chip.
            "" | OPEN_ANY => f.open_only = true,
            "awaiting" => f.awaiting_staff = true,
            "all" => {}
            s => f.status = s.parse().ok(),
        }
        match self.assigned.as_str() {
            "me" => f.assigned_to = Some(me),
            "none" => f.unassigned = true,
            s => f.assigned_to = Uuid::parse_str(s).ok().map(UserId::from),
        }
        f
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportView>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let filter = f.to_filter(&q, staff.user_id);
    let paged = support::list_tickets(&state.pool, tenant.id, &filter, q.db_page()).await?;
    let title = ctx.t("admin_support.tickets.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let names = staff_names(&state, &tenant).await?;
    let name_of: HashMap<UserId, String> = names.iter().cloned().collect();
    let counts = support::ticket_status_counts(&state.pool, tenant.id).await?;
    let count = |pred: &dyn Fn(TicketStatus) -> bool| -> i64 {
        counts
            .iter()
            .filter(|(s, _)| pred(*s))
            .map(|(_, n)| n)
            .sum()
    };
    let status = if f.status.is_empty() {
        OPEN_ANY.to_owned()
    } else {
        f.status.clone()
    };
    let keep = |s: &str| -> String {
        format!(
            "{}?{}",
            ctx.url("/admin/support/tickets"),
            common::query_string(&[
                ("status", s.to_owned()),
                ("department", f.department.clone()),
                ("priority", f.priority.clone()),
                ("assigned", f.assigned.clone()),
                ("q", q.q.clone().unwrap_or_default()),
            ])
        )
    };
    let mut chips = vec![
        Chip {
            value: "awaiting".into(),
            label: ctx.t("admin_support.tickets.chip_awaiting"),
            count: count(&|s| s.awaits_staff()),
            href: keep("awaiting"),
        },
        Chip {
            value: OPEN_ANY.into(),
            label: ctx.t("admin_support.tickets.chip_open"),
            count: count(&|s| s.is_open()),
            href: keep(OPEN_ANY),
        },
    ];
    for (s, (_, label)) in TicketStatus::ALL.iter().zip(status_options(&ctx)) {
        chips.push(Chip {
            value: s.as_str().into(),
            label,
            count: count(&|x| x == *s),
            href: keep(s.as_str()),
        });
    }
    chips.push(Chip {
        value: "all".into(),
        label: ctx.t("admin.common.all"),
        count: count(&|_| true),
        href: keep("all"),
    });
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/support/tickets"),
        &[
            ("q", q.q.as_deref().unwrap_or("")),
            ("status", &f.status),
            ("department", &f.department),
            ("priority", &f.priority),
            ("assigned", &f.assigned),
        ],
    );
    let back = format!(
        "/admin/support/tickets?{}",
        common::query_string(&[
            ("status", f.status.clone()),
            ("department", f.department.clone()),
            ("priority", f.priority.clone()),
            ("assigned", f.assigned.clone()),
            ("q", q.q.clone().unwrap_or_default()),
            ("page", q.page.to_string()),
        ])
    );
    let rows = paged
        .items
        .into_iter()
        .map(|t| QueueRow {
            assignee: t
                .ticket
                .assigned_to
                .and_then(|a| name_of.get(&a).cloned())
                .unwrap_or_default(),
            t,
        })
        .collect();
    render(&ListPage {
        rows,
        pager,
        chips,
        q: q.q.unwrap_or_default(),
        status,
        department: f.department,
        priority: f.priority,
        assigned: f.assigned,
        departments: support::list_departments(&state.pool, tenant.id, true).await?,
        priorities: priority_options(&ctx),
        staff: names
            .into_iter()
            .map(|(id, n)| (id.to_string(), n))
            .collect(),
        back,
        can_manage: staff.can(Permission::SUPPORT_MANAGE),
        me: staff.user_id.to_string(),
        ctx,
    })
}

/// `POST /admin/support/tickets/bulk` — close / assign the selected tickets.
async fn bulk(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = Some(f.text("back", 600))
        .filter(|b| b.starts_with("/admin/support/tickets"))
        .unwrap_or_else(|| "/admin/support/tickets".to_owned());
    let ids: Vec<TicketId> = f.ids("ids");
    if ids.is_empty() {
        return Ok(common::failed(
            &ctx,
            &back,
            ctx.t("admin.common.nothing_selected"),
        ));
    }
    let action = f.text("action", 20);
    let staff_ids: Vec<UserId> = identity::list_staff(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|u| u.id)
        .collect();
    let assignee: Option<UserId> = f.id("assignee").filter(|u| staff_ids.contains(u));
    let mut done = 0usize;
    let mut skipped = 0usize;
    for id in &ids {
        let ok = match action.as_str() {
            "close" => {
                match support::set_ticket_status(&state.pool, tenant.id, *id, TicketStatus::Closed)
                    .await
                {
                    Ok(changed) => changed,
                    Err(e) if e.http_status() < 500 => false,
                    Err(e) => return Err(e.into()),
                }
            }
            "assign" if assignee.is_some() => {
                support::assign_ticket(&state.pool, tenant.id, *id, assignee).await?
            }
            "unassign" => support::assign_ticket(&state.pool, tenant.id, *id, None).await?,
            _ => {
                return Ok(common::failed(
                    &ctx,
                    &back,
                    ctx.t("admin_support.tickets.bulk_invalid"),
                ));
            }
        };
        if ok {
            done += 1;
        } else {
            skipped += 1;
        }
    }
    au.log(
        &state,
        "ticket.bulk",
        None,
        json!({"action": action, "tickets": ids, "assignee": assignee, "done": done, "skipped": skipped}),
    )
    .await?;
    Ok(crate::flash::redirect(
        &ctx.url(&back),
        crate::flash::SetFlash::success(ctx.tv2(
            "admin_support.tickets.bulk_done",
            "done",
            done,
            "skipped",
            skipped,
        )),
    ))
}

// ---------------------------------------------------------------------------
// Ticket detail
// ---------------------------------------------------------------------------

/// A message with its rendered body and files.
pub struct MessageView {
    pub m: TicketMessageRow,
    pub html: String,
    pub files: Vec<TicketAttachmentRow>,
}

#[derive(Template)]
#[template(path = "admin/support/ticket.html")]
struct TicketPage {
    ctx: PageCtx,
    tk: TicketRow,
    department: String,
    assignee: String,
    requester: String,
    requester_email: String,
    messages: Vec<MessageView>,
    canned: Vec<CannedResponseRow>,
    departments: Vec<TicketDepartmentRow>,
    staff: Vec<(String, String)>,
    statuses: Vec<(String, String)>,
    priorities: Vec<(String, String)>,
    client: Option<ClientRow>,
    client_services: Vec<ServiceListRow>,
    unpaid: Vec<InvoiceListRow>,
    recent: Vec<TicketListRow>,
    related: String,
    related_href: String,
    can_manage: bool,
    can_billing: bool,
    ai_ready: bool,
    ai_box: String,
    body: String,
    errors: FormErrors,
    max_files: usize,
    max_kb: usize,
    base: String,
}
crate::page!(TicketPage);

impl TicketPage {
    fn size(&self, f: &TicketAttachmentRow) -> String {
        if f.size_bytes >= 1024 * 1024 {
            format!("{:.1} MB", f.size_bytes as f64 / 1_048_576.0)
        } else {
            format!("{} KB", (f.size_bytes + 1023) / 1024)
        }
    }

    fn author_label(&self, m: &TicketMessageRow) -> String {
        if m.author_name.trim().is_empty() {
            self.lbl("admin_support.authors", m.author_kind.as_str())
        } else {
            m.author_name.clone()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ShowQuery {
    #[serde(default)]
    draft: String,
    #[serde(default)]
    ai: String,
}

#[allow(clippy::too_many_arguments)]
async fn ticket_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    t: TicketRow,
    body: String,
    errors: FormErrors,
    ai: Option<AiGenerationRow>,
) -> AppResult<TicketPage> {
    let pool = &state.pool;
    let messages = support::ticket_messages(pool, tenant.id, t.id, true).await?;
    let attachments = support::ticket_attachments(pool, tenant.id, t.id).await?;
    let departments = support::list_departments(pool, tenant.id, true).await?;
    let department = departments
        .iter()
        .find(|d| d.id == t.department_id)
        .map(|d| d.name.clone())
        .unwrap_or_default();
    let names = staff_names(state, tenant).await?;
    let assignee = t
        .assigned_to
        .and_then(|a| {
            names
                .iter()
                .find(|(id, _)| *id == a)
                .map(|(_, n)| n.clone())
        })
        .unwrap_or_default();
    let client = match t.client_id {
        Some(c) => clients::get_client(pool, tenant.id, c).await?,
        None => None,
    };
    let (requester, requester_email) = match &client {
        Some(c) => (c.display_name(), c.email.clone()),
        None => (
            t.guest_name.clone().unwrap_or_default(),
            t.guest_email.clone().unwrap_or_default(),
        ),
    };
    let can_billing = staff.can(Permission::BILLING_VIEW);
    let (client_services, unpaid, recent) = match &client {
        Some(c) => {
            let services: Vec<ServiceListRow> = services::client_services(pool, tenant.id, c.id)
                .await?
                .into_iter()
                .take(6)
                .collect();
            let unpaid = if can_billing {
                invoices::list_invoices(
                    pool,
                    tenant.id,
                    &InvoiceFilter {
                        client_id: Some(c.id),
                        status: Some(InvoiceStatus::Unpaid),
                        ..InvoiceFilter::default()
                    },
                    Page::first(5),
                )
                .await?
                .items
            } else {
                Vec::new()
            };
            let recent = support::list_tickets(
                pool,
                tenant.id,
                &TicketFilter {
                    client_id: Some(c.id),
                    ..TicketFilter::default()
                },
                Page::first(6),
            )
            .await?
            .items
            .into_iter()
            .filter(|r| r.ticket.id != t.id)
            .take(5)
            .collect();
            (services, unpaid, recent)
        }
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    let (related, related_href) = match (t.service_id, t.domain_id) {
        (Some(sid), _) => match services::get_service(pool, tenant.id, sid).await? {
            Some(s) => (
                s.domain
                    .clone()
                    .unwrap_or_else(|| ctx.t("admin_support.ticket.related_service")),
                format!("/admin/services/{sid}"),
            ),
            None => (String::new(), String::new()),
        },
        (None, Some(did)) => match hos_db::repo::domains::get_domain(pool, tenant.id, did).await? {
            Some(d) => (
                hos_domains::name::to_unicode(&d.name),
                format!("/admin/domains/{did}"),
            ),
            None => (String::new(), String::new()),
        },
        _ => (String::new(), String::new()),
    };
    let messages = messages
        .into_iter()
        .map(|m| MessageView {
            html: crate::routes::client::user_markdown(state, &m.body),
            files: attachments
                .iter()
                .filter(|a| a.message_id == Some(m.id))
                .cloned()
                .collect(),
            m,
        })
        .collect();
    let mut statuses: Vec<(String, String)> = status_options(&ctx)
        .into_iter()
        .filter(|(v, _)| {
            v.parse::<TicketStatus>()
                .is_ok_and(|s| s == t.status || t.status.can_transition_to(s))
        })
        .collect();
    statuses.sort_by_key(|(v, _)| v != t.status.as_str());
    let ai_ready =
        staff.can(Permission::SUPPORT_MANAGE) && ai_studio::provider_ready(state, tenant).await;
    let base = format!("/admin/support/tickets/{}", t.id);
    let ai_box = match ai {
        Some(g) => draft_fragment(&ctx, &t, &g)?,
        None => String::new(),
    };
    let title = format!("#{} {}", t.number, t.subject);
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_support.tickets.title", "/admin/support/tickets")],
    );
    Ok(TicketPage {
        canned: support::list_canned_responses(pool, tenant.id, None).await?,
        statuses,
        priorities: priority_options(&ctx),
        staff: names
            .into_iter()
            .map(|(id, n)| (id.to_string(), n))
            .collect(),
        department,
        departments,
        assignee,
        requester,
        requester_email,
        messages,
        client,
        client_services,
        unpaid,
        recent,
        related,
        related_href,
        can_manage: staff.can(Permission::SUPPORT_MANAGE),
        can_billing,
        ai_ready,
        ai_box,
        body,
        errors,
        max_files: MAX_FILES,
        max_kb: MAX_FILE_BYTES / 1024,
        base,
        tk: t,
        ctx,
    })
}

async fn ticket_generation(
    state: &AppState,
    tenant: &Tenant,
    t: &TicketRow,
    raw: &str,
) -> AppResult<Option<AiGenerationRow>> {
    let Ok(id) = Uuid::parse_str(raw.trim()) else {
        return Ok(None);
    };
    Ok(
        billing_support::get_ai_generation(&state.pool, tenant.id, AiGenerationId::from(id))
            .await?
            .filter(|g| {
                g.entity_type.as_deref() == Some("ticket")
                    && g.entity_id == Some(t.id.into())
                    && g.purpose == AiPurpose::TicketReply
            }),
    )
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportView>,
    Path(id): Path<Uuid>,
    Query(sq): Query<ShowQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let t = load_ticket(&state, &tenant, id).await?;
    let mut body = String::new();
    if let Some(g) = ticket_generation(&state, &tenant, &t, &sq.draft).await?
        && g.status == AiGenerationStatus::Completed
    {
        body = draft_reply(&g);
    }
    let ai = ticket_generation(&state, &tenant, &t, &sq.ai).await?;
    let page = ticket_page(
        &state,
        &tenant,
        &staff,
        ctx,
        t,
        body,
        FormErrors::default(),
        ai,
    )
    .await?;
    render(&page)
}

/// Who receives the client-facing e-mails of a ticket: the opening client
/// user, else the client's e-mail, else the guest address.
async fn recipient(
    state: &AppState,
    tenant: &Tenant,
    t: &TicketRow,
) -> AppResult<Option<(Recipient, Locale)>> {
    if let Some(uid) = t.user_id
        && let Some(u) = identity::get_user(&state.pool, tenant.id, uid).await?
        && u.kind == UserKind::Client
    {
        let mut r = Recipient::new(u.email.clone(), Some(u.display_name()));
        r.user_id = Some(u.id);
        r.client_id = t.client_id;
        return Ok(Some((r, u.locale)));
    }
    if let Some(cid) = t.client_id
        && let Some(c) = clients::get_client(&state.pool, tenant.id, cid).await?
    {
        let mut r = Recipient::new(c.email.clone(), Some(c.display_name()));
        r.client_id = Some(c.id);
        return Ok(Some((r, c.locale)));
    }
    Ok(t.guest_email.clone().map(|e| {
        (
            Recipient::new(e, t.guest_name.clone()),
            tenant.default_locale,
        )
    }))
}

/// Queues the `ticket_reply` e-mail; `false` when it could not be queued.
async fn notify_client(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    t: &TicketRow,
    staff_name: &str,
    body: &str,
) -> AppResult<bool> {
    let Some((to, locale)) = recipient(state, tenant, t).await? else {
        return Ok(false);
    };
    let url = if t.client_id.is_some() {
        format!("{}/client/tickets/{}", ctx.origin, t.id)
    } else {
        format!("{}/contact", ctx.origin)
    };
    let excerpt = common::ellipsis(&crate::markdown::plain_text(body), 1200);
    let vars = json!({
        "user": {"name": to.name.clone().unwrap_or_default()},
        "ticket": {
            "number": t.number,
            "subject": t.subject,
            "staff_name": staff_name,
            "reply_excerpt": excerpt,
            "url": url,
        },
    });
    // With e-mail piping on, the Reply-To carries a reply token so that an
    // answer by e-mail lands in this ticket.
    let reply_to =
        crate::routes::webhooks::mail::reply_address_for(state, tenant, t, &to.email).await?;
    match mail::send_email_reply_to(state, tenant, "ticket_reply", locale, &to, vars, reply_to)
        .await
    {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::warn!(error = %e, ticket = %t.id, "ticket_reply e-mail not queued");
            Ok(false)
        }
    }
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

/// Accepted uploads of a multipart body; problems become field errors.
fn read_files(
    ctx: &PageCtx,
    mp: &Multipart,
    errors: &mut FormErrors,
) -> Vec<(String, &'static str, Vec<u8>)> {
    let mut out = Vec::new();
    for p in
        mp.0.iter()
            .filter(|p| p.name == "files" && p.filename.is_some() && !p.data.is_empty())
    {
        let name = clean_filename(p.filename.as_deref().unwrap_or_default());
        if out.len() >= MAX_FILES {
            errors.add(
                "files",
                ctx.tv("admin_support.ticket.too_many_files", "max", MAX_FILES),
            );
            break;
        }
        if p.data.len() > MAX_FILE_BYTES {
            errors.add(
                "files",
                ctx.tv("admin_support.ticket.file_too_large", "name", &name),
            );
            continue;
        }
        match sniff(&name, &p.data).filter(|ct| crate::demo::upload_type_allowed(&ctx.state, ct)) {
            Some(ct) => out.push((name, ct, p.data.clone())),
            None => errors.add(
                "files",
                ctx.tv("admin_support.ticket.file_type", "name", &name),
            ),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn reply(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    ip: ClientIp,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    mp: Multipart,
) -> AppResult<Response> {
    let t = load_ticket(&state, &tenant, id).await?;
    let mut errors = FormErrors::default();
    let body = mp.text("body").replace("\r\n", "\n");
    let internal = mp.text("mode") == "note";
    let after: Option<TicketStatus> = mp.text("after_status").parse().ok();
    let files = read_files(&ctx, &mp, &mut errors);
    if body.trim().is_empty() {
        errors.add("body", ctx.t("admin.common.required"));
    } else if body.chars().count() > MAX_BODY_CHARS {
        errors.add("body", ctx.t("admin_support.ticket.body_too_long"));
    }
    if errors.any() {
        let page = ticket_page(&state, &tenant, &staff, ctx, t, body, errors, None).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let staff_name = staff.user.display_name();
    let mut conn = state.pool.acquire().await?;
    let (message, updated) = support::reply_to_ticket(
        &mut conn,
        tenant.id,
        t.id,
        &NewTicketMessage {
            author_kind: AuthorKind::Staff,
            user_id: Some(staff.user_id),
            author_name: staff_name.clone(),
            author_email: Some(staff.user.email.clone()),
            body: body.trim().to_owned(),
            is_internal: internal,
            source: MessageSource::Web,
            message_id: None,
            ip: Some(ip.0),
        },
    )
    .await?;
    drop(conn);
    save_files(&state, &tenant, t.id, message.id, &files).await?;
    let mut warnings = Vec::new();
    if let Some(next) = after
        && next != updated.status
        && let Err(e) = support::set_ticket_status(&state.pool, tenant.id, t.id, next).await
    {
        warnings.push(common::failure_text(&ctx, &e)?);
    }
    let mut emailed = false;
    if !internal {
        emailed = notify_client(&state, &tenant, &ctx, &updated, &staff_name, &body).await?;
        if !emailed {
            warnings.push(ctx.t("admin_support.ticket.email_not_queued"));
        }
    }
    au.log(
        &state,
        if internal {
            "ticket.note"
        } else {
            "ticket.reply"
        },
        Some(("ticket", t.id.into())),
        json!({"message": message.id, "files": files.len(), "emailed": emailed, "status": after.map(|s| s.as_str())}),
    )
    .await?;
    let mut flash = crate::flash::SetFlash::success(ctx.t(if internal {
        "admin_support.ticket.note_added"
    } else {
        "admin_support.ticket.replied"
    }));
    for w in warnings {
        flash = flash.and(crate::flash::FlashKind::Warning, w);
    }
    Ok(crate::flash::redirect(
        &ctx.url(format!("/admin/support/tickets/{}#m-{}", t.id, message.id)),
        flash,
    ))
}

/// `POST /admin/support/tickets/{id}/update` — status, priority, department, assignee.
async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let t = load_ticket(&state, &tenant, id).await?;
    let back = format!("/admin/support/tickets/{}", t.id);
    let departments = support::list_departments(&state.pool, tenant.id, true).await?;
    let department: TicketDepartmentId = f
        .id("department")
        .filter(|d| departments.iter().any(|x| x.id == *d))
        .unwrap_or(t.department_id);
    let priority: TicketPriority = f.parse("priority").unwrap_or(t.priority);
    if department != t.department_id || priority != t.priority {
        support::update_ticket_routing(&state.pool, tenant.id, t.id, priority, department).await?;
    }
    if f.has("assigned_to") {
        let staff_ids: Vec<UserId> = identity::list_staff(&state.pool, tenant.id)
            .await?
            .into_iter()
            .map(|u| u.id)
            .collect();
        let assignee: Option<UserId> = f.id("assigned_to").filter(|u| staff_ids.contains(u));
        if assignee != t.assigned_to {
            support::assign_ticket(&state.pool, tenant.id, t.id, assignee).await?;
        }
    }
    let status: Option<TicketStatus> = f.parse("status");
    let result = match status {
        Some(s) if s != t.status => {
            support::set_ticket_status(&state.pool, tenant.id, t.id, s).await
        }
        _ => Ok(true),
    };
    au.log(
        &state,
        "ticket.update",
        Some(("ticket", t.id.into())),
        json!({
            "before": {"status": t.status.as_str(), "priority": t.priority.as_str(), "department": t.department_id, "assigned_to": t.assigned_to},
            "after": {"status": status.map(|s| s.as_str()), "priority": priority.as_str(), "department": department},
        }),
    )
    .await?;
    common::outcome(&ctx, &back, result, "admin.common.saved")
}

/// `GET /admin/support/tickets/{id}/attachments/{aid}` — always a download.
async fn download(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SupportView>,
    Path((id, aid)): Path<(Uuid, Uuid)>,
) -> AppResult<Response> {
    let t = load_ticket(&state, &tenant, id).await?;
    let (row, bytes) =
        support::attachment_content(&state.pool, tenant.id, t.id, TicketAttachmentId::from(aid))
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
// AI reply draft
// ---------------------------------------------------------------------------

fn draft_reply(g: &AiGenerationRow) -> String {
    g.output_json
        .as_ref()
        .and_then(|j| j.get("reply"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| g.output.clone())
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "admin/support/_ai_draft.html")]
struct DraftFragment {
    ctx: PageCtx,
    ticket_id: TicketId,
    g: AiGenerationRow,
    reply: String,
    note: String,
    confidence: String,
    needs_human: bool,
    used: Vec<String>,
}
crate::page!(DraftFragment);

fn draft_fragment(ctx: &PageCtx, t: &TicketRow, g: &AiGenerationRow) -> AppResult<String> {
    let j = g.output_json.clone().unwrap_or(Value::Null);
    let frag = DraftFragment {
        ctx: ctx.clone(),
        ticket_id: t.id,
        reply: draft_reply(g),
        note: j
            .get("internal_note")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        confidence: j
            .get("confidence")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        needs_human: j
            .get("needs_human")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        used: j
            .get("used_kb_urls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter(|u| super::content::is_http_url(u))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        g: g.clone(),
    };
    frag.render().map_err(AppError::internal)
}

/// Related KB articles (published, ticket language) for the AI context.
async fn kb_context(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    locale: Locale,
    t: &TicketRow,
    thread: &[TicketMessage],
) -> AppResult<Vec<KbSnippet>> {
    let mut words: Vec<String> = t
        .subject
        .split(|c: char| !c.is_alphanumeric())
        .chain(
            thread
                .iter()
                .rev()
                .find(|m| m.author == TicketAuthor::Customer)
                .map(|m| {
                    m.body
                        .split(|c: char| !c.is_alphanumeric())
                        .take(40)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
        .filter(|w| w.chars().count() >= 5)
        .map(str::to_lowercase)
        .collect();
    words.dedup();
    let mut found: Vec<content_repo::KbArticleRow> = Vec::new();
    for w in words.iter().take(12) {
        for a in content_repo::search_kb(&state.pool, tenant.id, locale, w, 3).await? {
            if !found.iter().any(|f| f.id == a.id) {
                found.push(a);
            }
        }
        if found.len() >= 4 {
            break;
        }
    }
    Ok(found
        .into_iter()
        .take(4)
        .map(|a| KbSnippet {
            url: format!(
                "{}{}",
                ctx.origin,
                crate::locale::localize_path(
                    &format!("/kb/{}", a.slug),
                    locale,
                    tenant.default_locale
                )
            ),
            title: a.title,
            content: common::ellipsis(&crate::markdown::plain_text(&a.body), 1500),
        })
        .collect())
}

/// `POST /admin/support/tickets/{id}/ai-draft` — queues a reply draft.
async fn ai_draft(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    htmx: Htmx,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let t = load_ticket(&state, &tenant, id).await?;
    let back = format!("/admin/support/tickets/{}", t.id);
    let messages = support::ticket_messages(&state.pool, tenant.id, t.id, false).await?;
    let thread: Vec<TicketMessage> = messages
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|m| TicketMessage {
            author: if m.author_kind == AuthorKind::Staff {
                TicketAuthor::Staff
            } else {
                TicketAuthor::Customer
            },
            name: Some(m.author_name.clone()).filter(|n| !n.trim().is_empty()),
            body: common::ellipsis(&crate::markdown::plain_text(&m.body), 4000),
            sent_at: Some(m.created_at),
        })
        .collect();
    let locale = match recipient(&state, &tenant, &t).await? {
        Some((_, l)) => l,
        None => tenant.default_locale,
    };
    if !thread.iter().any(|m| m.author == TicketAuthor::Customer) {
        let msg = ctx.t("admin_support.ai.no_customer_message");
        return Ok(if htmx.request {
            common::fragment(format!(
                "<div class=\"alert alert--warning\" role=\"status\">{}</div>",
                crate::view::escape_html(&msg)
            ))
        } else {
            common::failed(&ctx, &back, msg)
        });
    }
    let kb = kb_context(&state, &tenant, &ctx, locale, &t, &thread).await?;
    let input = json!({
        "task": "support_reply",
        "thread": thread,
        "kb": kb,
        "context": SupportContext {
            company: tenant.display_name().to_owned(),
            subject: Some(t.subject.clone()),
            agent_name: Some(staff.user.display_name()),
            signature: None,
        },
    });
    let started = ai_studio::start_generation(
        &state,
        &tenant,
        staff.user_id,
        AiPurpose::TicketReply,
        Some(("ticket", t.id.into())),
        Some(locale),
        "support_reply_draft",
        input,
    )
    .await?;
    let gid = match started {
        Ok(g) => g,
        Err(e) => {
            let msg = ctx.t(match e {
                StartError::NotConfigured => "admin_content.ai.not_configured",
                StartError::RateLimited => "admin_content.ai.rate_limited",
            });
            return Ok(if htmx.request {
                common::fragment(format!(
                    "<div class=\"alert alert--warning\" role=\"status\">{}</div>",
                    crate::view::escape_html(&msg)
                ))
            } else {
                common::failed(&ctx, &back, msg)
            });
        }
    };
    au.log(
        &state,
        "ticket.ai_draft",
        Some(("ticket", t.id.into())),
        json!({"generation": gid}),
    )
    .await?;
    if htmx.request {
        let g = billing_support::get_ai_generation(&state.pool, tenant.id, gid)
            .await?
            .ok_or(AppError::NotFound)?;
        return Ok(common::fragment(draft_fragment(&ctx, &t, &g)?));
    }
    Ok(crate::flash::redirect(
        &ctx.url(format!("{back}?ai={gid}#ai-draft")),
        crate::flash::SetFlash::info(ctx.t("admin_support.ai.queued")),
    ))
}

/// `GET /admin/support/tickets/{id}/ai-draft/{gid}` (htmx polling).
async fn ai_draft_status(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SupportManage>,
    Path((id, gid)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let t = load_ticket(&state, &tenant, id).await?;
    let g = ticket_generation(&state, &tenant, &t, &gid.to_string())
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(common::fragment(draft_fragment(&ctx, &t, &g)?))
}

// ---------------------------------------------------------------------------
// New ticket on behalf of a client
// ---------------------------------------------------------------------------

/// New-ticket form values.
#[derive(Debug, Clone, Default)]
pub struct NewTicketForm {
    pub client_id: String,
    pub client_email: String,
    pub department: String,
    pub priority: String,
    pub subject: String,
    pub body: String,
    pub notify: bool,
}

#[derive(Template)]
#[template(path = "admin/support/ticket_new.html")]
struct NewPage {
    ctx: PageCtx,
    form: NewTicketForm,
    client: Option<ClientRow>,
    departments: Vec<TicketDepartmentRow>,
    priorities: Vec<(String, String)>,
    errors: FormErrors,
}
crate::page!(NewPage);

#[derive(Debug, Default, Deserialize)]
struct NewQuery {
    #[serde(default)]
    client: String,
}

async fn new_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    form: NewTicketForm,
    errors: FormErrors,
) -> AppResult<NewPage> {
    let client = match Uuid::parse_str(&form.client_id) {
        Ok(id) => clients::get_client(&state.pool, tenant.id, ClientId::from(id)).await?,
        Err(_) => None,
    };
    let title = ctx.t("admin_support.new.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_support.tickets.title", "/admin/support/tickets")],
    );
    Ok(NewPage {
        form,
        client,
        departments: support::list_departments(&state.pool, tenant.id, true).await?,
        priorities: priority_options(&ctx),
        errors,
        ctx,
    })
}

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    Query(q): Query<NewQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = NewTicketForm {
        client_id: q.client,
        priority: "medium".into(),
        notify: true,
        ..NewTicketForm::default()
    };
    let page = new_page(&state, &tenant, &staff, ctx, form, FormErrors::default()).await?;
    render(&page)
}

async fn find_client(
    state: &AppState,
    tenant: &Tenant,
    form: &NewTicketForm,
) -> AppResult<Option<ClientRow>> {
    if let Ok(id) = Uuid::parse_str(&form.client_id) {
        return Ok(clients::get_client(&state.pool, tenant.id, ClientId::from(id)).await?);
    }
    let email = forms::normalize_email(&form.client_email);
    if !forms::valid_email(&email) {
        return Ok(None);
    }
    let found = hos_db::repo::admin::list_clients(
        &state.pool,
        tenant.id,
        &ClientFilter {
            search: Some(email.clone()),
            ..ClientFilter::default()
        },
        None,
        false,
        Page::first(20),
    )
    .await?;
    Ok(found
        .items
        .into_iter()
        .map(|r| r.client)
        .find(|c| c.email.eq_ignore_ascii_case(&email)))
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    ip: ClientIp,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut form = NewTicketForm {
        client_id: f.text("client_id", 40),
        client_email: f.text("client_email", 254),
        department: f.text("department", 40),
        priority: f.text("priority", 20),
        subject: forms::clean(f.raw("subject"), 200),
        body: f.body("body", MAX_BODY_CHARS + 1),
        notify: f.flag("notify"),
    };
    let mut errors = FormErrors::default();
    let client = find_client(&state, &tenant, &form).await?;
    match &client {
        Some(c) => form.client_id = c.id.to_string(),
        None => errors.add("client_email", ctx.t("admin_support.new.client_not_found")),
    }
    let departments = support::list_departments(&state.pool, tenant.id, true).await?;
    let department: Option<TicketDepartmentId> = Uuid::parse_str(&form.department)
        .ok()
        .map(TicketDepartmentId::from)
        .filter(|d| departments.iter().any(|x| x.id == *d));
    if department.is_none() {
        errors.add("department", ctx.t("admin.common.required"));
    }
    if form.subject.is_empty() {
        errors.add("subject", ctx.t("admin.common.required"));
    }
    if form.body.is_empty() {
        errors.add("body", ctx.t("admin.common.required"));
    } else if form.body.chars().count() > MAX_BODY_CHARS {
        errors.add("body", ctx.t("admin_support.ticket.body_too_long"));
    }
    let (Some(client), Some(department), false) = (client, department, errors.any()) else {
        let page = new_page(&state, &tenant, &staff, ctx, form, errors).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    let priority: TicketPriority = form.priority.parse().unwrap_or_default();
    let staff_name = staff.user.display_name();
    let mut conn = state.pool.acquire().await?;
    let (ticket, _) = support::open_ticket(
        &mut conn,
        tenant.id,
        &NewTicket {
            department_id: department,
            client_id: Some(client.id),
            user_id: Some(staff.user_id),
            guest_name: None,
            guest_email: None,
            subject: form.subject.clone(),
            priority,
            service_id: None,
            domain_id: None,
            first_message: NewTicketMessage {
                author_kind: AuthorKind::Staff,
                user_id: Some(staff.user_id),
                author_name: staff_name.clone(),
                author_email: Some(staff.user.email.clone()),
                body: form.body.clone(),
                is_internal: false,
                source: MessageSource::Web,
                message_id: None,
                ip: Some(ip.0),
            },
        },
    )
    .await?;
    drop(conn);
    support::set_ticket_status(&state.pool, tenant.id, ticket.id, TicketStatus::Answered).await?;
    let emailed = if form.notify {
        notify_client(&state, &tenant, &ctx, &ticket, &staff_name, &form.body).await?
    } else {
        false
    };
    au.log(
        &state,
        "ticket.open_for_client",
        Some(("ticket", ticket.id.into())),
        json!({"client": client.id, "number": ticket.number, "emailed": emailed}),
    )
    .await?;
    Ok(crate::flash::redirect(
        &ctx.url(format!("/admin/support/tickets/{}", ticket.id)),
        crate::flash::SetFlash::success(ctx.tv(
            "admin_support.new.opened",
            "number",
            &ticket.number,
        )),
    ))
}

// ---------------------------------------------------------------------------
// Departments
// ---------------------------------------------------------------------------

/// Department form values.
#[derive(Debug, Clone, Default)]
pub struct DepartmentForm {
    pub name: String,
    pub name_en: String,
    pub slug: String,
    pub description: String,
    pub description_en: String,
    pub email: String,
    pub sort_order: String,
    pub clients_only: bool,
    pub auto_respond: bool,
    pub is_hidden: bool,
}

impl DepartmentForm {
    fn from_row(d: &TicketDepartmentRow) -> Self {
        let en = |k: &str| {
            d.translations
                .get("en")
                .and_then(|e| e.get(k))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            name: d.name.clone(),
            name_en: en("name"),
            slug: d.slug.clone(),
            description: d.description.clone().unwrap_or_default(),
            description_en: en("description"),
            email: d.email.clone().unwrap_or_default(),
            sort_order: d.sort_order.to_string(),
            clients_only: d.clients_only,
            auto_respond: d.auto_respond,
            is_hidden: d.is_hidden,
        }
    }

    fn read(f: &FormData) -> Self {
        Self {
            name: f.text("name", 120),
            name_en: f.text("name_en", 120),
            slug: f.text("slug", 60).to_lowercase(),
            description: f.text("description", 500),
            description_en: f.text("description_en", 500),
            email: forms::normalize_email(&f.text("email", 254)),
            sort_order: f.text("sort_order", 10),
            clients_only: f.flag("clients_only"),
            auto_respond: f.flag("auto_respond"),
            is_hidden: f.flag("is_hidden"),
        }
    }

    fn validate(&mut self, ctx: &PageCtx, errors: &mut FormErrors) -> DepartmentInput {
        if self.name.is_empty() {
            errors.add("name", ctx.t("admin.common.required"));
        }
        if self.slug.is_empty() {
            self.slug = crate::markdown::slugify(&self.name);
        }
        if !valid_slug(&self.slug) {
            errors.add("slug", ctx.t("admin_content.editor.slug_invalid"));
        }
        if !self.email.is_empty() && !forms::valid_email(&self.email) {
            errors.add("email", ctx.t("admin.common.invalid_email"));
        }
        let sort_order = match self.sort_order.trim() {
            "" => 0,
            s => s.parse().unwrap_or_else(|_| {
                errors.add("sort_order", ctx.t("admin.common.invalid_value"));
                0
            }),
        };
        let mut en = serde_json::Map::new();
        if !self.name_en.is_empty() {
            en.insert("name".into(), json!(self.name_en));
        }
        if !self.description_en.is_empty() {
            en.insert("description".into(), json!(self.description_en));
        }
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        DepartmentInput {
            slug: self.slug.clone(),
            name: self.name.clone(),
            description: opt(&self.description),
            translations: if en.is_empty() {
                json!({})
            } else {
                json!({ "en": en })
            },
            email: opt(&self.email),
            clients_only: self.clients_only,
            auto_respond: self.auto_respond,
            is_hidden: self.is_hidden,
            sort_order,
        }
    }
}

/// A department with its ticket count.
pub struct DepartmentView {
    pub d: TicketDepartmentRow,
    pub tickets: i64,
}

#[derive(Template)]
#[template(path = "admin/support/departments.html")]
struct DepartmentsPage {
    ctx: PageCtx,
    rows: Vec<DepartmentView>,
    form: DepartmentForm,
    errors: FormErrors,
}
crate::page!(DepartmentsPage);

async fn departments_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    form: DepartmentForm,
    errors: FormErrors,
) -> AppResult<DepartmentsPage> {
    let title = ctx.t("admin_support.departments.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_support.tickets.title", "/admin/support/tickets")],
    );
    let counts: HashMap<TicketDepartmentId, i64> =
        support::department_ticket_counts(&state.pool, tenant.id)
            .await?
            .into_iter()
            .collect();
    let rows = support::list_departments(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .map(|d| DepartmentView {
            tickets: counts.get(&d.id).copied().unwrap_or(0),
            d,
        })
        .collect();
    Ok(DepartmentsPage {
        ctx,
        rows,
        form,
        errors,
    })
}

async fn departments(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = DepartmentForm {
        sort_order: "0".into(),
        auto_respond: true,
        ..DepartmentForm::default()
    };
    let page = departments_page(&state, &tenant, &staff, ctx, form, FormErrors::default()).await?;
    render(&page)
}

async fn create_department(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let mut form = DepartmentForm::read(&f);
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &mut errors);
    if !errors.any()
        && support::department_by_slug(&state.pool, tenant.id, &input.slug)
            .await?
            .is_some()
    {
        errors.add("slug", ctx.t("admin_content.editor.slug_taken"));
    }
    if errors.any() {
        let page = departments_page(&state, &tenant, &staff, ctx, form, errors).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let d = support::create_department_full(&state.pool, tenant.id, &input).await?;
    au.log(
        &state,
        "department.create",
        Some(("department", d.id.into())),
        json!({"slug": d.slug, "name": d.name}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/departments",
        "admin.common.created",
    ))
}

#[derive(Template)]
#[template(path = "admin/support/department.html")]
struct DepartmentPage {
    ctx: PageCtx,
    id: TicketDepartmentId,
    form: DepartmentForm,
    errors: FormErrors,
}
crate::page!(DepartmentPage);

async fn load_department(
    state: &AppState,
    tenant: &Tenant,
    id: Uuid,
) -> AppResult<TicketDepartmentRow> {
    support::get_department(&state.pool, tenant.id, TicketDepartmentId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

fn department_page(
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    id: TicketDepartmentId,
    form: DepartmentForm,
    errors: FormErrors,
) -> DepartmentPage {
    let title = if form.name.is_empty() {
        ctx.t("admin_support.departments.title")
    } else {
        form.name.clone()
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[
            ("admin_support.tickets.title", "/admin/support/tickets"),
            (
                "admin_support.departments.title",
                "/admin/support/departments",
            ),
        ],
    );
    DepartmentPage {
        ctx,
        id,
        form,
        errors,
    }
}

async fn edit_department(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let d = load_department(&state, &tenant, id).await?;
    render(&department_page(
        &staff,
        ctx,
        d.id,
        DepartmentForm::from_row(&d),
        FormErrors::default(),
    ))
}

async fn update_department(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let before = load_department(&state, &tenant, id).await?;
    let mut form = DepartmentForm::read(&f);
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &mut errors);
    if !errors.any()
        && support::department_by_slug(&state.pool, tenant.id, &input.slug)
            .await?
            .is_some_and(|d| d.id != before.id)
    {
        errors.add("slug", ctx.t("admin_content.editor.slug_taken"));
    }
    if errors.any() {
        let page = department_page(&staff, ctx, before.id, form, errors);
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    support::update_department(&state.pool, tenant.id, before.id, &input)
        .await?
        .ok_or(AppError::NotFound)?;
    au.log(
        &state,
        "department.update",
        Some(("department", before.id.into())),
        json!({"before": {"slug": before.slug, "name": before.name}, "after": {"slug": input.slug, "name": input.name}}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/departments",
        "admin.common.saved",
    ))
}

async fn delete_department(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let d = load_department(&state, &tenant, id).await?;
    if !support::delete_department(&state.pool, tenant.id, d.id).await? {
        return Ok(common::failed(
            &ctx,
            "/admin/support/departments",
            ctx.t("admin_support.departments.in_use"),
        ));
    }
    au.log(
        &state,
        "department.delete",
        Some(("department", d.id.into())),
        json!({"slug": d.slug}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/departments",
        "admin.common.deleted",
    ))
}

// ---------------------------------------------------------------------------
// Canned responses
// ---------------------------------------------------------------------------

/// Canned response form values.
#[derive(Debug, Clone, Default)]
pub struct CannedForm {
    pub title: String,
    pub body: String,
    pub locale: String,
    pub category: String,
}

impl CannedForm {
    fn read(f: &FormData) -> Self {
        Self {
            title: f.text("title", 150),
            body: f.body("body", 10_000),
            locale: f.text("locale", 5),
            category: f.text("category", 80),
        }
    }

    fn validate(&self, ctx: &PageCtx, errors: &mut FormErrors) -> Locale {
        if self.title.is_empty() {
            errors.add("title", ctx.t("admin.common.required"));
        }
        if self.body.is_empty() {
            errors.add("body", ctx.t("admin.common.required"));
        }
        self.locale.parse().unwrap_or(Locale::Tr)
    }
}

#[derive(Template)]
#[template(path = "admin/support/canned.html")]
struct CannedPage {
    ctx: PageCtx,
    rows: Vec<CannedResponseRow>,
    form: CannedForm,
    errors: FormErrors,
    editing: Option<CannedResponseId>,
}
crate::page!(CannedPage);

impl CannedPage {
    fn is_editing(&self, id: &CannedResponseId) -> bool {
        self.editing == Some(*id)
    }

    fn form_title(&self) -> String {
        self.t(if self.editing.is_some() {
            "admin_support.canned.edit"
        } else {
            "admin_support.canned.new"
        })
    }

    fn form_action(&self) -> String {
        match self.editing {
            Some(id) => format!("/admin/support/canned/{id}"),
            None => "/admin/support/canned".to_owned(),
        }
    }
}

async fn canned_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    form: CannedForm,
    errors: FormErrors,
    editing: Option<CannedResponseId>,
) -> AppResult<CannedPage> {
    let title = ctx.t("admin_support.canned.title");
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin_support.tickets.title", "/admin/support/tickets")],
    );
    Ok(CannedPage {
        rows: support::list_canned_responses(&state.pool, tenant.id, None).await?,
        ctx,
        form,
        errors,
        editing,
    })
}

async fn canned(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = CannedForm {
        locale: tenant.default_locale.as_str().into(),
        ..CannedForm::default()
    };
    let page = canned_page(
        &state,
        &tenant,
        &staff,
        ctx,
        form,
        FormErrors::default(),
        None,
    )
    .await?;
    render(&page)
}

async fn create_canned(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = CannedForm::read(&f);
    let mut errors = FormErrors::default();
    let locale = form.validate(&ctx, &mut errors);
    if errors.any() {
        let page = canned_page(&state, &tenant, &staff, ctx, form, errors, None).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let row = support::create_canned_response(
        &state.pool,
        tenant.id,
        &form.title,
        &form.body,
        locale,
        Some(staff.user_id),
    )
    .await?;
    if !form.category.is_empty() {
        support::update_canned_response(
            &state.pool,
            tenant.id,
            row.id,
            &row.title,
            &row.body,
            locale,
            Some(&form.category),
        )
        .await?;
    }
    au.log(
        &state,
        "canned_response.create",
        Some(("canned_response", row.id.into())),
        json!({"title": row.title}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/canned",
        "admin.common.created",
    ))
}

async fn load_canned(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<CannedResponseRow> {
    support::get_canned_response(&state.pool, tenant.id, CannedResponseId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

async fn edit_canned(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = load_canned(&state, &tenant, id).await?;
    let form = CannedForm {
        title: c.title.clone(),
        body: c.body.clone(),
        locale: c.locale.as_str().into(),
        category: c.category.clone().unwrap_or_default(),
    };
    let page = canned_page(
        &state,
        &tenant,
        &staff,
        ctx,
        form,
        FormErrors::default(),
        Some(c.id),
    )
    .await?;
    render(&page)
}

async fn update_canned(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = load_canned(&state, &tenant, id).await?;
    let form = CannedForm::read(&f);
    let mut errors = FormErrors::default();
    let locale = form.validate(&ctx, &mut errors);
    if errors.any() {
        let page = canned_page(&state, &tenant, &staff, ctx, form, errors, Some(c.id)).await?;
        return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    support::update_canned_response(
        &state.pool,
        tenant.id,
        c.id,
        &form.title,
        &form.body,
        locale,
        (!form.category.is_empty()).then_some(form.category.as_str()),
    )
    .await?
    .ok_or(AppError::NotFound)?;
    au.log(
        &state,
        "canned_response.update",
        Some(("canned_response", c.id.into())),
        json!({"title": form.title}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/canned",
        "admin.common.saved",
    ))
}

async fn delete_canned(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SupportManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = load_canned(&state, &tenant, id).await?;
    support::delete_canned_response(&state.pool, tenant.id, c.id).await?;
    au.log(
        &state,
        "canned_response.delete",
        Some(("canned_response", c.id.into())),
        json!({"title": c.title}),
    )
    .await?;
    Ok(common::done(
        &ctx,
        "/admin/support/canned",
        "admin.common.deleted",
    ))
}
