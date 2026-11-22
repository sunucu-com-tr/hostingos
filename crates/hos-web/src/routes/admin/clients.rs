//! `/admin/clients` — client list (search, filters, sorting, CSV export),
//! create / edit, the tabbed detail page, credit adjustments, notes,
//! contacts, client e-mails, client groups and "log in as client".

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_auth::rbac::Permission;
use hos_core::domain::{Locale, UserKind};
use hos_core::ids::{ClientGroupId, ClientId, ContactId, UserId};
use hos_core::money::Currency;
use hos_db::Page;
use hos_db::enums::{ClientKind, ClientStatus, CreditKind};
use hos_db::ids::ClientNoteId;
use hos_db::repo::admin::{self as arepo, ClientLogin};
use hos_db::repo::audit::{self, AuditFilter};
use hos_db::repo::clients::{
    self, ClientFilter, ClientGroupRow, ClientInput, ClientListRow, ClientNoteRow, ClientRow,
    ContactInput, ContactRow,
};
use hos_db::repo::domains::{self, DomainRow};
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::invoices::{self, InvoiceFilter, InvoiceListRow};
use hos_db::repo::mail::{self as mail_repo, EmailLogRow};
use hos_db::repo::payments::{self, CreditRow, TransactionFilter, TransactionRow};
use hos_db::repo::services::{self, ServiceListRow};
use hos_db::repo::stats::{self, ClientSummary};
use hos_db::repo::support::{self, TicketFilter, TicketListRow};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{
    self, Audit, FormData, SortHeader, Ui as _, csv_response, done, failed, sort_headers,
};
use super::dashboard::{Activity, activity_lines};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, PageQuery, RequirePermission, perm};
use crate::forms::{self, FormErrors};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, Pager, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/clients", get(list).post(create))
        .route("/admin/clients/export.csv", get(export))
        .route("/admin/clients/new", get(new_form))
        .route("/admin/clients/groups", get(groups).post(create_group))
        .route("/admin/clients/groups/{id}", post(update_group))
        .route("/admin/clients/groups/{id}/delete", post(delete_group))
        .route("/admin/clients/{id}", get(show))
        .route("/admin/clients/{id}/edit", get(edit_form).post(update))
        .route("/admin/clients/{id}/status", post(set_status))
        .route("/admin/clients/{id}/credit", post(credit))
        .route("/admin/clients/{id}/notes", get(notes_tab).post(add_note))
        .route("/admin/clients/{id}/notes/{note}/delete", post(delete_note))
        .route(
            "/admin/clients/{id}/contacts",
            get(contacts_tab).post(add_contact),
        )
        .route(
            "/admin/clients/{id}/contacts/{contact}/delete",
            post(delete_contact),
        )
        .route("/admin/clients/{id}/email", post(send_email))
        .route("/admin/clients/{id}/logins", post(add_login))
        .route("/admin/clients/{id}/{tab}", get(show_tab))
}

// ---------------------------------------------------------------------------
// List + export
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    status: Option<String>,
    group: Option<String>,
}

impl ListFilter {
    fn to_filter(&self, q: &PageQuery) -> ClientFilter {
        ClientFilter {
            search: q.q.clone(),
            status: self.status.as_deref().and_then(|s| s.parse().ok()),
            group_id: self
                .group
                .as_deref()
                .and_then(|g| Uuid::parse_str(g).ok())
                .map(ClientGroupId::from),
        }
    }
}

#[derive(Template)]
#[template(path = "admin/clients/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ClientListRow>,
    pager: Pager,
    headers: Vec<SortHeader>,
    q: String,
    status: String,
    group: String,
    groups: Vec<ClientGroupRow>,
    statuses: Vec<(String, String)>,
    export_href: String,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsView>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let filter = f.to_filter(&q);
    let paged = arepo::list_clients(
        &state.pool,
        tenant.id,
        &filter,
        q.sort.as_deref(),
        q.dir == crate::extract::SortDir::Desc,
        q.db_page(),
    )
    .await?;
    let title = ctx.t("admin.clients.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let keep = vec![
        ("q", q.q.clone().unwrap_or_default()),
        ("status", f.status.clone().unwrap_or_default()),
        ("group", f.group.clone().unwrap_or_default()),
    ];
    let headers = sort_headers(
        &ctx,
        "/admin/clients",
        &q,
        &[
            ("name", "admin.clients.col_name", false),
            ("company", "admin.clients.col_company", false),
            ("email", "admin.clients.col_email", false),
            ("services", "admin.clients.col_services", true),
            ("unpaid", "admin.clients.col_unpaid", true),
            ("created", "admin.clients.col_created", false),
        ],
        &keep,
    );
    let mut pager_keep: Vec<(&str, &str)> = keep.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let sort = q.sort.clone().unwrap_or_default();
    pager_keep.push(("sort", &sort));
    pager_keep.push(("dir", q.dir.as_str()));
    let pager = Pager::new(&paged, &ctx.url("/admin/clients"), &pager_keep);
    let export_href = format!(
        "{}?{}",
        ctx.url("/admin/clients/export.csv"),
        common::query_string(&keep)
    );
    let groups = clients::list_groups(&state.pool, tenant.id).await?;
    let statuses = common::enum_options(&ctx, ClientStatus::ALL, |s| s.as_str(), "admin.status");
    render(&ListPage {
        statuses,
        rows: paged.items,
        pager,
        headers,
        q: q.q.unwrap_or_default(),
        status: f.status.unwrap_or_default(),
        group: f.group.unwrap_or_default(),
        groups,
        export_href,
        ctx,
    })
}

async fn export(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsView>,
    au: Audit,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let filter = f.to_filter(&q);
    let mut rows = Vec::new();
    let mut offset = 0;
    loop {
        let page = arepo::list_clients(
            &state.pool,
            tenant.id,
            &filter,
            Some("created"),
            false,
            Page::new(200, offset),
        )
        .await?;
        let n = page.items.len() as i64;
        for r in page.items {
            let c = r.client;
            rows.push(vec![
                c.id.to_string(),
                c.first_name,
                c.last_name,
                c.company_name.unwrap_or_default(),
                c.email,
                c.phone.unwrap_or_default(),
                c.city.unwrap_or_default(),
                c.country_code,
                c.tax_number.unwrap_or_default(),
                c.status.as_str().to_owned(),
                c.currency.to_string(),
                r.active_services.to_string(),
                r.unpaid_total.round_dp(2).to_string(),
                c.created_at.format("%Y-%m-%d").to_string(),
            ]);
        }
        offset += n;
        if n < 200 || offset >= page.total || offset >= 50_000 {
            break;
        }
    }
    au.log(
        &state,
        "client.export",
        None,
        json!({"rows": rows.len(), "q": q.q}),
    )
    .await?;
    let header = [
        "id",
        "first_name",
        "last_name",
        "company",
        "email",
        "phone",
        "city",
        "country",
        "tax_number",
        "status",
        "currency",
        "active_services",
        "unpaid_total",
        "created",
    ]
    .iter()
    .map(|h| ctx.t(&format!("admin.clients.csv_{h}")))
    .collect::<Vec<_>>();
    Ok(csv_response("clients.csv", &header, &rows))
}

// ---------------------------------------------------------------------------
// Create / edit
// ---------------------------------------------------------------------------

/// Form values (strings so a failed submit re-renders what was typed).
#[derive(Debug, Clone, Default)]
pub struct ClientForm {
    pub kind: String,
    pub first_name: String,
    pub last_name: String,
    pub company_name: String,
    pub email: String,
    pub phone: String,
    pub address_line1: String,
    pub address_line2: String,
    pub city: String,
    pub state: String,
    pub postal_code: String,
    pub country_code: String,
    pub tax_office: String,
    pub tax_number: String,
    pub national_id: String,
    pub currency: String,
    pub locale: String,
    pub group_id: String,
    pub tax_exempt: bool,
    pub marketing_opt_in: bool,
    pub late_fee_exempt: bool,
    pub overdue_notices: bool,
    pub separate_invoices: bool,
    pub create_login: bool,
    pub password: String,
}

impl ClientForm {
    fn from_row(c: &ClientRow) -> Self {
        Self {
            kind: c.kind.as_str().into(),
            first_name: c.first_name.clone(),
            last_name: c.last_name.clone(),
            company_name: c.company_name.clone().unwrap_or_default(),
            email: c.email.clone(),
            phone: c.phone.clone().unwrap_or_default(),
            address_line1: c.address_line1.clone().unwrap_or_default(),
            address_line2: c.address_line2.clone().unwrap_or_default(),
            city: c.city.clone().unwrap_or_default(),
            state: c.state.clone().unwrap_or_default(),
            postal_code: c.postal_code.clone().unwrap_or_default(),
            country_code: c.country_code.clone(),
            tax_office: c.tax_office.clone().unwrap_or_default(),
            tax_number: c.tax_number.clone().unwrap_or_default(),
            national_id: c.national_id.clone().unwrap_or_default(),
            currency: c.currency.to_string(),
            locale: c.locale.as_str().into(),
            group_id: c.group_id.map(|g| g.to_string()).unwrap_or_default(),
            tax_exempt: c.tax_exempt,
            marketing_opt_in: c.marketing_opt_in,
            late_fee_exempt: c.late_fee_exempt,
            overdue_notices: c.overdue_notices,
            separate_invoices: c.separate_invoices,
            create_login: false,
            password: String::new(),
        }
    }

    fn read(f: &FormData) -> Self {
        Self {
            kind: f.text("kind", 20),
            first_name: f.text("first_name", 100),
            last_name: f.text("last_name", 100),
            company_name: f.text("company_name", 200),
            email: forms::normalize_email(&f.text("email", 254)),
            phone: f.text("phone", 40),
            address_line1: f.text("address_line1", 200),
            address_line2: f.text("address_line2", 200),
            city: f.text("city", 100),
            state: f.text("state", 100),
            postal_code: f.text("postal_code", 20),
            country_code: f.text("country_code", 2).to_uppercase(),
            tax_office: f.text("tax_office", 100),
            tax_number: f.text("tax_number", 40),
            national_id: f.text("national_id", 20),
            currency: f.text("currency", 3).to_uppercase(),
            locale: f.text("locale", 5),
            group_id: f.text("group_id", 40),
            tax_exempt: f.flag("tax_exempt"),
            marketing_opt_in: f.flag("marketing_opt_in"),
            late_fee_exempt: f.flag("late_fee_exempt"),
            overdue_notices: f.flag("overdue_notices"),
            separate_invoices: f.flag("separate_invoices"),
            create_login: f.flag("create_login"),
            password: f.raw("password").to_owned(),
        }
    }

    fn validate(&self, ctx: &PageCtx, errors: &mut FormErrors) -> Option<ClientInput> {
        let req = |errors: &mut FormErrors, k: &str, v: &str| {
            if v.is_empty() {
                errors.add(k, ctx.t("admin.common.required"));
            }
        };
        req(errors, "first_name", &self.first_name);
        req(errors, "last_name", &self.last_name);
        if !forms::valid_email(&self.email) {
            errors.add("email", ctx.t("admin.common.invalid_email"));
        }
        if self.country_code.len() != 2
            || !self.country_code.chars().all(|c| c.is_ascii_alphabetic())
        {
            errors.add("country_code", ctx.t("admin.clients.invalid_country"));
        }
        let currency: Option<Currency> = self.currency.parse().ok();
        if currency.is_none() {
            errors.add("currency", ctx.t("admin.common.invalid_value"));
        }
        let kind: ClientKind = self.kind.parse().unwrap_or(ClientKind::Individual);
        if kind == ClientKind::Corporate && self.company_name.is_empty() {
            errors.add("company_name", ctx.t("admin.common.required"));
        }
        if self.create_login && !self.password.is_empty() && self.password.chars().count() < 10 {
            errors.add("password", ctx.t("admin.clients.password_short"));
        }
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        Some(ClientInput {
            kind,
            first_name: self.first_name.clone(),
            last_name: self.last_name.clone(),
            company_name: opt(&self.company_name),
            email: self.email.clone(),
            phone: opt(&self.phone),
            address_line1: opt(&self.address_line1),
            address_line2: opt(&self.address_line2),
            city: opt(&self.city),
            state: opt(&self.state),
            postal_code: opt(&self.postal_code),
            country_code: self.country_code.clone(),
            tax_office: opt(&self.tax_office),
            tax_number: opt(&self.tax_number),
            national_id: opt(&self.national_id),
            currency: currency?,
            locale: self.locale.parse().unwrap_or(Locale::Tr),
            group_id: Uuid::parse_str(&self.group_id)
                .ok()
                .map(ClientGroupId::from),
            tax_exempt: self.tax_exempt,
            marketing_opt_in: self.marketing_opt_in,
        })
    }
}

#[derive(Template)]
#[template(path = "admin/clients/form.html")]
struct FormPage {
    ctx: PageCtx,
    form: ClientForm,
    errors: FormErrors,
    editing: Option<ClientId>,
    groups: Vec<ClientGroupRow>,
    currencies: Vec<String>,
}
crate::page!(FormPage);

async fn form_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    form: ClientForm,
    errors: FormErrors,
    editing: Option<ClientId>,
) -> AppResult<FormPage> {
    let title = if editing.is_some() {
        ctx.t("admin.clients.edit_title")
    } else {
        ctx.t("admin.clients.new_title")
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("admin.clients.title", "/admin/clients")],
    );
    let groups = clients::list_groups(&state.pool, tenant.id).await?;
    let currencies = hos_db::repo::catalog::list_currencies(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .map(|c| c.code.to_string())
        .collect();
    Ok(FormPage {
        ctx,
        form,
        errors,
        editing,
        groups,
        currencies,
    })
}

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let form = ClientForm {
        kind: "individual".into(),
        country_code: "TR".into(),
        currency: tenant.default_currency.to_string(),
        locale: tenant.default_locale.as_str().into(),
        overdue_notices: true,
        create_login: true,
        ..ClientForm::default()
    };
    let page = form_page(
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

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = ClientForm::read(&f);
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &mut errors);
    if let Some(i) = &input {
        crate::einvoice::check_tax_ids(&state, tenant.id, &ctx, i, &mut errors).await?;
    }
    let existing_user = if forms::valid_email(&form.email) {
        identity::user_by_email(&state.pool, tenant.id, &form.email).await?
    } else {
        None
    };
    if form.create_login
        && let Some(u) = &existing_user
        && u.kind != UserKind::Client
    {
        errors.add("email", ctx.t("admin.clients.email_staff"));
    }
    // Plan limit (`Clients`), checked right before the record is created.
    if !errors.any()
        && !super::settings::has_room(
            &state,
            tenant.id,
            crate::routes::platform::limits::Limit::Clients,
        )
        .await?
    {
        errors.add_general(ctx.t("admin_settings.limits.clients"));
    }
    let Some(input) = input.filter(|_| !errors.any()) else {
        let page = form_page(&state, &tenant, &staff, ctx, form, errors, None).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    let client = if form.create_login {
        let user = match existing_user {
            Some(u) => u,
            None => {
                let hash = if form.password.is_empty() {
                    None
                } else {
                    Some(hos_auth::password::hash_password(&form.password)?)
                };
                identity::create_user(
                    &state.pool,
                    tenant.id,
                    &NewUser {
                        email: input.email.clone(),
                        password_hash: hash,
                        kind: UserKind::Client,
                        first_name: input.first_name.clone(),
                        last_name: input.last_name.clone(),
                        phone: input.phone.clone(),
                        locale: input.locale,
                        email_verified: false,
                    },
                )
                .await?
            }
        };
        let mut conn = state.pool.acquire().await?;
        clients::create_client_with_owner(&mut conn, tenant.id, &input, user.id).await?
    } else {
        clients::create_client(&state.pool, tenant.id, &input).await?
    };
    arepo::set_client_billing_flags(
        &state.pool,
        tenant.id,
        client.id,
        form.late_fee_exempt,
        form.overdue_notices,
        form.separate_invoices,
    )
    .await?;
    au.log(
        &state,
        "client.create",
        Some(("client", client.id.into())),
        json!({"email": client.email, "login": form.create_login}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/clients/{}", client.id),
        "admin.clients.created",
    ))
}

async fn edit_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let client = load(&state, &tenant, id.into()).await?;
    let page = form_page(
        &state,
        &tenant,
        &staff,
        ctx,
        ClientForm::from_row(&client),
        FormErrors::default(),
        Some(client.id),
    )
    .await?;
    render(&page)
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let before = load(&state, &tenant, id.into()).await?;
    let mut form = ClientForm::read(&f);
    form.create_login = false;
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &mut errors);
    if let Some(i) = &input {
        crate::einvoice::check_tax_ids(&state, tenant.id, &ctx, i, &mut errors).await?;
    }
    let Some(input) = input.filter(|_| !errors.any()) else {
        let page = form_page(&state, &tenant, &staff, ctx, form, errors, Some(before.id)).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    clients::update_client(&state.pool, tenant.id, before.id, &input)
        .await?
        .ok_or(AppError::NotFound)?;
    arepo::set_client_billing_flags(
        &state.pool,
        tenant.id,
        before.id,
        form.late_fee_exempt,
        form.overdue_notices,
        form.separate_invoices,
    )
    .await?;
    au.log(
        &state,
        "client.update",
        Some(("client", before.id.into())),
        json!({
            "before": {"email": before.email, "group": before.group_id, "currency": before.currency.to_string()},
            "after": {"email": input.email, "group": input.group_id, "currency": input.currency.to_string()},
        }),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/clients/{}", before.id),
        "admin.common.saved",
    ))
}

async fn load(state: &AppState, tenant: &Tenant, id: ClientId) -> AppResult<ClientRow> {
    clients::get_client(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

/// A note with its author's name.
pub struct NoteView {
    pub note: ClientNoteRow,
    pub author: String,
}

#[derive(Template)]
#[template(path = "admin/clients/show.html")]
struct ShowPage {
    ctx: PageCtx,
    c: ClientRow,
    tab: String,
    tabs: Vec<NavItem>,
    summary: ClientSummary,
    credit: Decimal,
    group_name: String,
    groups: Vec<ClientGroupRow>,
    logins: Vec<ClientLogin>,
    services: Vec<ServiceListRow>,
    domains: Vec<DomainRow>,
    invoices: Vec<InvoiceListRow>,
    transactions: Vec<TransactionRow>,
    credits: Vec<CreditRow>,
    tickets: Vec<TicketListRow>,
    notes: Vec<NoteView>,
    contacts: Vec<ContactRow>,
    emails: Vec<EmailLogRow>,
    activity: Vec<Activity>,
    base: String,
    statuses: Vec<(String, String)>,
}
crate::page!(ShowPage);

impl ShowPage {
    fn group_sel(&self, id: &ClientGroupId) -> &'static str {
        if self.c.group_id == Some(*id) {
            " selected"
        } else {
            ""
        }
    }
}

const TABS: &[&str] = &[
    "summary",
    "services",
    "domains",
    "invoices",
    "transactions",
    "tickets",
    "notes",
    "contacts",
    "emails",
    "activity",
];

async fn show(
    state: State<AppState>,
    tenant: Tenant,
    rp: RequirePermission<perm::ClientsView>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    detail(state, tenant, rp, id, "summary".into(), ctx).await
}

async fn notes_tab(
    state: State<AppState>,
    tenant: Tenant,
    rp: RequirePermission<perm::ClientsView>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    detail(state, tenant, rp, id, "notes".into(), ctx).await
}

async fn contacts_tab(
    state: State<AppState>,
    tenant: Tenant,
    rp: RequirePermission<perm::ClientsView>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    detail(state, tenant, rp, id, "contacts".into(), ctx).await
}

async fn show_tab(
    state: State<AppState>,
    tenant: Tenant,
    rp: RequirePermission<perm::ClientsView>,
    Path((id, tab)): Path<(Uuid, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if !TABS.contains(&tab.as_str()) {
        return Err(AppError::NotFound);
    }
    detail(state, tenant, rp, id, tab, ctx).await
}

async fn detail(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsView>,
    id: Uuid,
    tab: String,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let pool = &state.pool;
    let summary = stats::client_summary(pool, tenant.id, c.id).await?;
    let credit = payments::credit_balance(pool, tenant.id, c.id, c.currency).await?;
    let groups = clients::list_groups(pool, tenant.id).await?;
    let group_name = groups
        .iter()
        .find(|g| Some(g.id) == c.group_id)
        .map(|g| g.name.clone())
        .unwrap_or_default();
    let logins = arepo::client_logins(pool, tenant.id, c.id).await?;
    let base = format!("/admin/clients/{}", c.id);
    let tabs = common::tabs(
        &ctx,
        &TABS
            .iter()
            .map(|t| {
                (
                    match *t {
                        "summary" => "admin.clients.tab_summary",
                        "services" => "admin.clients.tab_services",
                        "domains" => "admin.clients.tab_domains",
                        "invoices" => "admin.clients.tab_invoices",
                        "transactions" => "admin.clients.tab_transactions",
                        "tickets" => "admin.clients.tab_tickets",
                        "notes" => "admin.clients.tab_notes",
                        "contacts" => "admin.clients.tab_contacts",
                        "emails" => "admin.clients.tab_emails",
                        _ => "admin.clients.tab_activity",
                    },
                    if *t == "summary" {
                        base.clone()
                    } else {
                        format!("{base}/{t}")
                    },
                )
            })
            .collect::<Vec<_>>(),
    );
    let mut page = ShowPage {
        ctx: ctx.clone(),
        c: c.clone(),
        tab: tab.clone(),
        tabs,
        summary,
        credit,
        group_name,
        groups,
        logins,
        services: Vec::new(),
        domains: Vec::new(),
        invoices: Vec::new(),
        transactions: Vec::new(),
        credits: Vec::new(),
        tickets: Vec::new(),
        notes: Vec::new(),
        contacts: Vec::new(),
        emails: Vec::new(),
        activity: Vec::new(),
        base: base.clone(),
        statuses: common::enum_options(&ctx, ClientStatus::ALL, |s| s.as_str(), "admin.status"),
    };
    let can_billing = staff.can(Permission::BILLING_VIEW);
    match tab.as_str() {
        "summary" => {
            page.services = services::client_services(pool, tenant.id, c.id)
                .await?
                .into_iter()
                .take(5)
                .collect();
            page.notes = notes_of(&state, &tenant, c.id)
                .await?
                .into_iter()
                .filter(|n| n.note.is_sticky)
                .collect();
            if can_billing {
                page.invoices = invoices::list_invoices(
                    pool,
                    tenant.id,
                    &InvoiceFilter {
                        client_id: Some(c.id),
                        ..InvoiceFilter::default()
                    },
                    Page::first(5),
                )
                .await?
                .items;
            }
        }
        "services" => page.services = services::client_services(pool, tenant.id, c.id).await?,
        "domains" => page.domains = domains::client_domains(pool, tenant.id, c.id).await?,
        "invoices" if can_billing => {
            page.invoices = invoices::list_invoices(
                pool,
                tenant.id,
                &InvoiceFilter {
                    client_id: Some(c.id),
                    ..InvoiceFilter::default()
                },
                Page::first(100),
            )
            .await?
            .items;
        }
        "transactions" if can_billing => {
            page.transactions = payments::list_transactions(
                pool,
                tenant.id,
                &TransactionFilter {
                    client_id: Some(c.id),
                    ..TransactionFilter::default()
                },
                Page::first(100),
            )
            .await?
            .items;
            page.credits = payments::list_credits(pool, tenant.id, c.id).await?;
        }
        "tickets" if staff.can(Permission::SUPPORT_VIEW) => {
            page.tickets = support::list_tickets(
                pool,
                tenant.id,
                &TicketFilter {
                    client_id: Some(c.id),
                    ..TicketFilter::default()
                },
                Page::first(100),
            )
            .await?
            .items;
        }
        "notes" => page.notes = notes_of(&state, &tenant, c.id).await?,
        "contacts" => page.contacts = clients::list_contacts(pool, tenant.id, c.id).await?,
        "emails" => {
            page.emails = mail_repo::list_email_log(pool, tenant.id, Some(c.id), Page::first(100))
                .await?
                .items;
        }
        "activity" => {
            let rows = audit::list(
                pool,
                tenant.id,
                &AuditFilter {
                    entity_type: Some("client".into()),
                    entity_id: Some(c.id.into()),
                    ..AuditFilter::default()
                },
                Page::first(100),
            )
            .await?
            .items;
            page.activity = activity_lines(&state, &ctx, &tenant, rows).await?;
        }
        _ => {}
    }
    let name = c.display_name();
    common::page(
        &mut ctx,
        &staff,
        &name,
        &[("admin.clients.title", "/admin/clients")],
    );
    page.ctx = ctx;
    render(&page)
}

async fn notes_of(state: &AppState, tenant: &Tenant, id: ClientId) -> AppResult<Vec<NoteView>> {
    let staff_names: std::collections::HashMap<UserId, String> =
        identity::list_staff(&state.pool, tenant.id)
            .await?
            .into_iter()
            .map(|u| (u.id, u.display_name()))
            .collect();
    Ok(clients::list_notes(&state.pool, tenant.id, id)
        .await?
        .into_iter()
        .map(|n| NoteView {
            author: n
                .author_user_id
                .and_then(|a| staff_names.get(&a).cloned())
                .unwrap_or_else(|| "—".into()),
            note: n,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async fn set_status(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/clients/{}", c.id);
    let status: Option<ClientStatus> = f.parse("status");
    let group: Option<ClientGroupId> = f.id("group_id");
    if let Some(status) = status
        && status != c.status
    {
        clients::set_client_status(&state.pool, tenant.id, c.id, status).await?;
        au.log(
            &state,
            "client.status",
            Some(("client", c.id.into())),
            json!({"from": c.status.as_str(), "to": status.as_str()}),
        )
        .await?;
    }
    if f.has("group_id") && group != c.group_id {
        let mut input = client_input(&c);
        input.group_id = group;
        clients::update_client(&state.pool, tenant.id, c.id, &input).await?;
        au.log(
            &state,
            "client.group",
            Some(("client", c.id.into())),
            json!({"from": c.group_id, "to": group}),
        )
        .await?;
    }
    Ok(done(&ctx, &back, "admin.common.saved"))
}

fn client_input(c: &ClientRow) -> ClientInput {
    ClientInput {
        kind: c.kind,
        first_name: c.first_name.clone(),
        last_name: c.last_name.clone(),
        company_name: c.company_name.clone(),
        email: c.email.clone(),
        phone: c.phone.clone(),
        address_line1: c.address_line1.clone(),
        address_line2: c.address_line2.clone(),
        city: c.city.clone(),
        state: c.state.clone(),
        postal_code: c.postal_code.clone(),
        country_code: c.country_code.clone(),
        tax_office: c.tax_office.clone(),
        tax_number: c.tax_number.clone(),
        national_id: c.national_id.clone(),
        currency: c.currency,
        locale: c.locale,
        group_id: c.group_id,
        tax_exempt: c.tax_exempt,
        marketing_opt_in: c.marketing_opt_in,
    }
}

async fn credit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/clients/{}/transactions", c.id);
    let Ok(Some(amount)) = f.decimal("amount") else {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
    };
    if amount <= Decimal::ZERO {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_amount")));
    }
    let signed = if f.raw("direction") == "remove" {
        -amount
    } else {
        amount
    };
    let description = f
        .opt("description", 300)
        .unwrap_or_else(|| ctx.t("admin.clients.credit_default_desc"));
    let mut conn = state.pool.acquire().await?;
    let result = payments::adjust_credit(
        &mut conn,
        tenant.id,
        c.id,
        signed,
        c.currency,
        CreditKind::Manual,
        &description,
        Some(staff.user_id),
    )
    .await;
    drop(conn);
    if result.is_ok() {
        au.log(
            &state,
            "client.credit",
            Some(("client", c.id.into())),
            json!({"amount": signed.to_string(), "currency": c.currency.to_string(), "description": description}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.clients.credit_saved")
}

async fn add_note(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/clients/{}/notes", c.id);
    let body = f.body("body", 10_000);
    if body.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.required")));
    }
    let note = clients::add_note(
        &state.pool,
        tenant.id,
        c.id,
        Some(staff.user_id),
        &body,
        f.flag("is_sticky"),
    )
    .await?;
    au.log(
        &state,
        "client.note_add",
        Some(("client", c.id.into())),
        json!({"note": note.id}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.clients.note_added"))
}

async fn delete_note(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path((id, note)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    if !arepo::delete_client_note(&state.pool, tenant.id, c.id, ClientNoteId::from(note)).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "client.note_delete",
        Some(("client", c.id.into())),
        json!({"note": note}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/clients/{}/notes", c.id),
        "admin.common.deleted",
    ))
}

async fn add_contact(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/clients/{}/contacts", c.id);
    let email = forms::normalize_email(&f.text("email", 254));
    let first = f.text("first_name", 100);
    let last = f.text("last_name", 100);
    if first.is_empty() || last.is_empty() || !forms::valid_email(&email) {
        return Ok(failed(&ctx, &back, ctx.t("admin.clients.contact_invalid")));
    }
    let contact = clients::create_contact(
        &state.pool,
        tenant.id,
        c.id,
        &ContactInput {
            first_name: first,
            last_name: last,
            company_name: f.opt("company_name", 200),
            email,
            phone: f.opt("phone", 40),
            address_line1: None,
            address_line2: None,
            city: None,
            state: None,
            postal_code: None,
            country_code: c.country_code.clone(),
            is_billing: f.flag("is_billing"),
            is_domain_contact: f.flag("is_domain_contact"),
            email_preferences: f.all("email_preferences"),
        },
    )
    .await?;
    au.log(
        &state,
        "client.contact_add",
        Some(("client", c.id.into())),
        json!({"contact": contact.id}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.clients.contact_added"))
}

async fn delete_contact(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path((id, contact)): Path<(Uuid, Uuid)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let contact = ContactId::from(contact);
    let owned = clients::list_contacts(&state.pool, tenant.id, c.id)
        .await?
        .iter()
        .any(|x| x.id == contact);
    if !owned || !clients::delete_contact(&state.pool, tenant.id, contact).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "client.contact_delete",
        Some(("client", c.id.into())),
        json!({"contact": contact}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/clients/{}/contacts", c.id),
        "admin.common.deleted",
    ))
}

async fn add_login(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/clients/{}", c.id);
    let email = forms::normalize_email(&f.text("email", 254));
    if !forms::valid_email(&email) {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_email")));
    }
    let user = match identity::user_by_email(&state.pool, tenant.id, &email).await? {
        Some(u) if u.kind == UserKind::Client => u,
        Some(_) => return Ok(failed(&ctx, &back, ctx.t("admin.clients.email_staff"))),
        None => {
            identity::create_user(
                &state.pool,
                tenant.id,
                &NewUser {
                    email: email.clone(),
                    password_hash: None,
                    kind: UserKind::Client,
                    first_name: c.first_name.clone(),
                    last_name: c.last_name.clone(),
                    phone: None,
                    locale: c.locale,
                    email_verified: false,
                },
            )
            .await?
        }
    };
    let owner = arepo::client_logins(&state.pool, tenant.id, c.id)
        .await?
        .is_empty();
    clients::link_user(&state.pool, tenant.id, c.id, user.id, owner, &[]).await?;
    au.log(
        &state,
        "client.login_add",
        Some(("client", c.id.into())),
        json!({"user": user.id, "email": email}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.clients.login_added"))
}

async fn send_email(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let c = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/clients/{}/emails", c.id);
    let subject = f.text("subject", 200);
    let body = f.body("body", 20_000);
    if subject.is_empty() || body.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.clients.email_invalid")));
    }
    let log_id = super::mailer::queue_custom_email(
        &state,
        &tenant,
        &c.email,
        Some(c.display_name()),
        c.locale,
        Some(c.id),
        &subject,
        &body,
    )
    .await?;
    au.log(
        &state,
        "client.email",
        Some(("client", c.id.into())),
        json!({"subject": subject, "email_log": log_id, "by": staff.user_id}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.clients.email_queued"))
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/clients/groups.html")]
struct GroupsPage {
    ctx: PageCtx,
    groups: Vec<ClientGroupRow>,
}
crate::page!(GroupsPage);

async fn groups(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ClientsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin.clients.groups_title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.clients.title", "/admin/clients")],
    );
    let groups = clients::list_groups(&state.pool, tenant.id).await?;
    render(&GroupsPage { ctx, groups })
}

/// Name, description, colour and discount of a client group form.
type GroupFields = (String, Option<String>, Option<String>, Decimal);

fn group_fields(f: &FormData) -> Result<GroupFields, &'static str> {
    let name = f.text("name", 100);
    if name.is_empty() {
        return Err("admin.common.required");
    }
    let discount = f
        .decimal("discount_percent")
        .map_err(|_| "admin.common.invalid_amount")?
        .unwrap_or_default();
    if discount < Decimal::ZERO || discount > Decimal::from(100) {
        return Err("admin.common.invalid_amount");
    }
    let color = f.opt("color", 7).filter(|c| {
        c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit())
    });
    Ok((name, f.opt("description", 500), color, discount))
}

async fn create_group(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/clients/groups";
    let (name, _desc, color, discount) = match group_fields(&f) {
        Ok(v) => v,
        Err(k) => return Ok(failed(&ctx, back, ctx.t(k))),
    };
    let result =
        clients::create_group(&state.pool, tenant.id, &name, discount, color.as_deref()).await;
    if let Ok(g) = &result {
        au.log(
            &state,
            "client_group.create",
            None,
            json!({"group": g.id, "name": name}),
        )
        .await?;
    }
    common::outcome(&ctx, back, result, "admin.common.created")
}

async fn update_group(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/clients/groups";
    let (name, desc, color, discount) = match group_fields(&f) {
        Ok(v) => v,
        Err(k) => return Ok(failed(&ctx, back, ctx.t(k))),
    };
    let result = arepo::update_client_group(
        &state.pool,
        tenant.id,
        id.into(),
        &name,
        desc.as_deref(),
        color.as_deref(),
        discount,
    )
    .await;
    match result {
        Ok(false) => return Err(AppError::NotFound),
        Ok(true) => {
            au.log(
                &state,
                "client_group.update",
                None,
                json!({"group": id, "name": name}),
            )
            .await?;
        }
        Err(_) => {}
    }
    common::outcome(&ctx, back, result, "admin.common.saved")
}

async fn delete_group(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::ClientsManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if !arepo::delete_client_group(&state.pool, tenant.id, id.into()).await? {
        return Err(AppError::NotFound);
    }
    au.log(&state, "client_group.delete", None, json!({"group": id}))
        .await?;
    Ok(done(&ctx, "/admin/clients/groups", "admin.common.deleted"))
}
