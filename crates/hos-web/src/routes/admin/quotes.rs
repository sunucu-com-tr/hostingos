//! `/admin/quotes` — quotes (estimates): list, create / edit with line
//! items, send with a secret accept link, PDF, accept on the client's
//! behalf, cancel, delete drafts.
//!
//! Reading needs `quotes.view`, every change `quotes.manage`. Prices and
//! totals are computed by hos-billing (`Billing::save_quote`); the form only
//! carries the line inputs.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Days;
use hos_auth::rbac::Permission;
use hos_billing::quotes::{QuoteDraft, QuoteLine};
use hos_core::ids::{ClientId, ProductId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::Page;
use hos_db::repo::catalog;
use hos_db::repo::clients::{self, ClientRow};
use hos_db::repo::quotes::{
    self as qrepo, QuoteFilter, QuoteId, QuoteItemRow, QuoteListRow, QuoteRow, QuoteStatus,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, PageQuery, PermissionMarker, RequirePermission};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render};

/// `quotes.view`.
#[derive(Debug, Clone, Copy)]
pub struct QuotesView;
impl PermissionMarker for QuotesView {
    const PERMISSION: Permission = Permission::QUOTES_VIEW;
}
/// `quotes.manage`.
#[derive(Debug, Clone, Copy)]
pub struct QuotesManage;
impl PermissionMarker for QuotesManage {
    const PERMISSION: Permission = Permission::QUOTES_MANAGE;
}

/// Blank line rows offered below the existing lines of the editor.
const BLANK_ROWS: usize = 3;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/quotes", get(list).post(create))
        .route("/admin/quotes/new", get(new_form))
        .route("/admin/quotes/{id}", get(show).post(update))
        .route("/admin/quotes/{id}/edit", get(edit_form))
        .route("/admin/quotes/{id}/send", post(send))
        .route("/admin/quotes/{id}/accept", post(accept))
        .route("/admin/quotes/{id}/cancel", post(cancel))
        .route("/admin/quotes/{id}/delete", post(delete))
        .route("/admin/quotes/{id}/pdf", get(pdf))
}

/// Badge tone of a quote status.
pub fn tone(s: QuoteStatus) -> &'static str {
    match s {
        QuoteStatus::Accepted => "success",
        QuoteStatus::Sent => "info",
        QuoteStatus::Draft => "warning",
        QuoteStatus::Declined | QuoteStatus::Expired => "danger",
        QuoteStatus::Cancelled => "",
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    status: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/quotes/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<QuoteListRow>,
    pager: Pager,
    q: String,
    status: String,
    statuses: Vec<(String, String)>,
}
crate::page!(ListPage);

impl ListPage {
    fn qbadge(&self, s: &QuoteStatus) -> askama::filters::Safe<String> {
        self.badge(self.lbl("quotes.status", s.as_str()), tone(*s))
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<QuotesView>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<QuoteStatus> = f.status.as_deref().and_then(|s| s.parse().ok());
    let order = q.sort_column(
        &[
            ("number", "q.seq"),
            ("total", "q.total"),
            ("valid", "q.valid_until"),
        ],
        "q.created_at",
    );
    let paged = qrepo::list_quotes(
        &state.pool,
        tenant.id,
        &QuoteFilter {
            status,
            client_id: None,
            search: q.q.clone(),
        },
        order,
        q.db_page(),
    )
    .await?;
    let title = ctx.t("quotes.admin.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let qv = q.q.clone().unwrap_or_default();
    let sv = status.map(|s| s.as_str().to_owned()).unwrap_or_default();
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/quotes"),
        &[("q", qv.as_str()), ("status", sv.as_str())],
    );
    let statuses = common::enum_options(&ctx, QuoteStatus::ALL, |s| s.as_str(), "quotes.status");
    render(&ListPage {
        rows: paged.items,
        pager,
        q: qv,
        status: sv,
        statuses,
        ctx,
    })
}

// ---------------------------------------------------------------------------
// editor
// ---------------------------------------------------------------------------

/// One editor row (strings as typed, so a failed save keeps the input).
#[derive(Debug, Clone, Default)]
pub struct LineForm {
    pub product: String,
    pub cycle: String,
    pub description: String,
    pub quantity: String,
    pub price: String,
    pub discount: String,
    pub taxable: bool,
}

/// The editor's fields.
#[derive(Debug, Clone, Default)]
pub struct QuoteForm {
    pub client_email: String,
    pub prospect_name: String,
    pub prospect_email: String,
    pub prospect_company: String,
    pub subject: String,
    pub currency: String,
    pub valid_until: String,
    pub notes: String,
    pub admin_notes: String,
    pub lines: Vec<LineForm>,
}

#[derive(Template)]
#[template(path = "admin/quotes/form.html")]
struct FormPage {
    ctx: PageCtx,
    id: Option<QuoteId>,
    title: String,
    form: QuoteForm,
    error: String,
    currencies: Vec<String>,
    products: Vec<(String, String)>,
    cycles: Vec<(String, String)>,
}
crate::page!(FormPage);

async fn options(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
) -> AppResult<(Vec<String>, Vec<(String, String)>, Vec<(String, String)>)> {
    let currencies = catalog::list_currencies(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .map(|c| c.code.to_string())
        .collect();
    let products = catalog::list_products(
        &state.pool,
        tenant.id,
        &catalog::ProductFilter::default(),
        Page::first(500),
    )
    .await?
    .items
    .into_iter()
    .map(|p| (p.id.to_string(), p.name))
    .collect();
    let cycles = BillingCycle::ALL
        .iter()
        .map(|c| (c.as_str().to_owned(), ctx.cycle(c)))
        .collect();
    Ok((currencies, products, cycles))
}

#[allow(clippy::too_many_arguments)]
async fn form_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    id: Option<QuoteId>,
    mut form: QuoteForm,
    error: String,
) -> AppResult<Response> {
    let title = match id {
        Some(_) => ctx.t("quotes.admin.edit_title"),
        None => ctx.t("quotes.admin.new_title"),
    };
    common::page(
        &mut ctx,
        staff,
        &title,
        &[("quotes.admin.title", "/admin/quotes")],
    );
    let (currencies, products, cycles) = options(state, tenant, &ctx).await?;
    let blanks = BLANK_ROWS.min(hos_billing::quotes::MAX_LINES.saturating_sub(form.lines.len()));
    form.lines.extend((0..blanks).map(|_| LineForm {
        quantity: "1".into(),
        taxable: true,
        ..LineForm::default()
    }));
    let page = FormPage {
        id,
        title,
        form,
        currencies,
        products,
        cycles,
        error: error.clone(),
        ctx,
    };
    if error.is_empty() {
        render(&page)
    } else {
        crate::view::render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)
    }
}

#[derive(Debug, Default, Deserialize)]
struct NewQuery {
    client_id: Option<Uuid>,
}

async fn new_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<QuotesManage>,
    Query(nq): Query<NewQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let mut form = QuoteForm {
        currency: tenant.default_currency.to_string(),
        valid_until: common::today(&ctx.timezone)
            .checked_add_days(Days::new(30))
            .map(|d| d.to_string())
            .unwrap_or_default(),
        ..QuoteForm::default()
    };
    if let Some(cid) = nq.client_id
        && let Some(c) = clients::get_client(&state.pool, tenant.id, ClientId::from(cid)).await?
    {
        form.client_email = c.email.clone();
        form.currency = c.currency.to_string();
    }
    form_page(&state, &tenant, &staff, ctx, None, form, String::new()).await
}

fn form_from(f: &FormData) -> QuoteForm {
    // Positional columns: empty cells are kept so the rows stay aligned
    // (`FormData::all` drops empty values).
    let col = |k: &str| -> Vec<String> {
        f.0.iter()
            .filter(|(key, _)| key == k)
            .map(|(_, v)| v.trim().to_owned())
            .collect()
    };
    let (prods, cycles, descs, qtys, prices, discs, taxes) = (
        col("line_product"),
        col("line_cycle"),
        col("line_description"),
        col("line_quantity"),
        col("line_price"),
        col("line_discount"),
        col("line_taxable"),
    );
    let n = descs.len().min(hos_billing::quotes::MAX_LINES + BLANK_ROWS);
    let at = |v: &Vec<String>, i: usize| v.get(i).cloned().unwrap_or_default();
    let lines = (0..n)
        .map(|i| LineForm {
            product: at(&prods, i),
            cycle: at(&cycles, i),
            description: at(&descs, i).chars().take(500).collect(),
            quantity: at(&qtys, i).chars().take(20).collect(),
            price: at(&prices, i).chars().take(30).collect(),
            discount: at(&discs, i).chars().take(10).collect(),
            taxable: at(&taxes, i) != "0",
        })
        // A row without description and price is an unused blank row.
        .filter(|l| !l.description.trim().is_empty() || !l.price.trim().is_empty() || !l.product.is_empty())
        .collect();
    QuoteForm {
        client_email: f.text("client_email", 254).to_lowercase(),
        prospect_name: f.text("prospect_name", 200),
        prospect_email: f.text("prospect_email", 254).to_lowercase(),
        prospect_company: f.text("prospect_company", 200),
        subject: f.text("subject", 200),
        currency: f.text("currency", 3).to_ascii_uppercase(),
        valid_until: f.text("valid_until", 10),
        notes: f.body("notes", 10_000),
        admin_notes: f.body("admin_notes", 10_000),
        lines,
    }
}

/// Parses the editor into a draft for hos-billing; `Err(message)` for input
/// that cannot be interpreted (hos-billing validates the rest).
async fn draft_from(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    form: &QuoteForm,
) -> AppResult<Result<QuoteDraft, String>> {
    let client: Option<ClientRow> = if form.client_email.is_empty() {
        None
    } else {
        let c = qrepo::client_for_email(&state.pool, tenant.id, &form.client_email).await?;
        match c {
            Some(c) => Some(c),
            None => return Ok(Err(ctx.t("quotes.admin.client_not_found"))),
        }
    };
    let Ok(currency) = form.currency.parse::<Currency>() else {
        return Ok(Err(ctx.t("quotes.admin.invalid_currency")));
    };
    let Ok(valid_until) = form.valid_until.parse::<chrono::NaiveDate>() else {
        return Ok(Err(ctx.t("admin.common.invalid_date")));
    };
    let mut lines = Vec::with_capacity(form.lines.len());
    for (i, l) in form.lines.iter().enumerate() {
        let n = i + 1;
        let bad =
            |what: &str| Err(ctx.tv2("quotes.admin.bad_line", "line", n, "field", ctx.t(what)));
        let quantity = match common::parse_decimal(&l.quantity) {
            Ok(Some(q)) => q,
            Ok(None) => Decimal::ONE,
            Err(()) => return Ok(bad("quotes.fields.quantity")),
        };
        let unit_price = match common::parse_decimal(&l.price) {
            Ok(Some(p)) => p,
            _ => return Ok(bad("quotes.fields.unit_price")),
        };
        let discount_percent = match common::parse_decimal(&l.discount) {
            Ok(d) => d.unwrap_or(Decimal::ZERO),
            Err(()) => return Ok(bad("quotes.fields.discount")),
        };
        let product_id = match l.product.trim() {
            "" => None,
            p => match p.parse::<Uuid>() {
                Ok(u) => Some(ProductId::from(u)),
                Err(_) => return Ok(bad("quotes.fields.product")),
            },
        };
        let billing_cycle = match (product_id, l.cycle.parse::<BillingCycle>()) {
            (Some(_), Ok(c)) => Some(c),
            (Some(_), Err(_)) => return Ok(bad("quotes.fields.cycle")),
            (None, _) => None,
        };
        lines.push(QuoteLine {
            product_id,
            billing_cycle,
            description: l.description.trim().to_owned(),
            quantity,
            unit_price,
            discount_percent,
            taxable: l.taxable,
        });
    }
    let opt = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_owned());
    Ok(Ok(QuoteDraft {
        client_id: client.as_ref().map(|c| c.id),
        prospect_name: client.is_none().then(|| opt(&form.prospect_name)).flatten(),
        prospect_email: client
            .is_none()
            .then(|| opt(&form.prospect_email))
            .flatten(),
        prospect_company: client
            .is_none()
            .then(|| opt(&form.prospect_company))
            .flatten(),
        subject: form.subject.clone(),
        currency,
        valid_until,
        notes: opt(&form.notes),
        admin_notes: opt(&form.admin_notes),
        lines,
    }))
}

async fn save(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    au: &Audit,
    ctx: PageCtx,
    id: Option<QuoteId>,
    f: &FormData,
) -> AppResult<Response> {
    let form = form_from(f);
    let draft = match draft_from(state, tenant, &ctx, &form).await? {
        Ok(d) => d,
        Err(msg) => return form_page(state, tenant, staff, ctx, id, form, msg).await,
    };
    match state
        .billing
        .save_quote(tenant.id, id, &draft, Some(staff.user_id))
        .await
    {
        Ok(q) => {
            au.log(
                state,
                if id.is_some() {
                    "quote.update"
                } else {
                    "quote.create"
                },
                Some(("quote", q.id.into_uuid())),
                json!({"total": q.total, "currency": q.currency, "lines": draft.lines.len()}),
            )
            .await?;
            Ok(done(
                &ctx,
                &format!("/admin/quotes/{}", q.id),
                "quotes.admin.saved",
            ))
        }
        Err(e) => {
            let msg = common::failure_text(&ctx, &e)?;
            form_page(state, tenant, staff, ctx, id, form, msg).await
        }
    }
}

async fn create(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<QuotesManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    save(&state, &tenant, &staff, &au, ctx, None, &f).await
}

async fn load(state: &AppState, tenant: &Tenant, id: Uuid) -> AppResult<QuoteRow> {
    qrepo::get_quote(&state.pool, tenant.id, QuoteId::from(id))
        .await?
        .ok_or(AppError::NotFound)
}

async fn update(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<QuotesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let q = load(&state, &tenant, id).await?;
    save(&state, &tenant, &staff, &au, ctx, Some(q.id), &f).await
}

async fn edit_form(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<QuotesManage>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let q = load(&state, &tenant, id).await?;
    if !q.status.is_editable() {
        return Ok(failed(
            &ctx,
            &format!("/admin/quotes/{}", q.id),
            ctx.t("quotes.admin.not_editable"),
        ));
    }
    let items = qrepo::quote_items(&state.pool, tenant.id, q.id).await?;
    let client_email = match q.client_id {
        Some(c) => clients::get_client(&state.pool, tenant.id, c)
            .await?
            .map(|c| c.email)
            .unwrap_or_default(),
        None => String::new(),
    };
    let dec = |d: &Decimal| d.normalize().to_string();
    let form = QuoteForm {
        client_email,
        prospect_name: q.prospect_name.clone().unwrap_or_default(),
        prospect_email: q.prospect_email.clone().unwrap_or_default(),
        prospect_company: q.prospect_company.clone().unwrap_or_default(),
        subject: q.subject.clone(),
        currency: q.currency.to_string(),
        valid_until: q.valid_until.to_string(),
        notes: q.notes.clone().unwrap_or_default(),
        admin_notes: q.admin_notes.clone().unwrap_or_default(),
        lines: items
            .iter()
            .map(|i| LineForm {
                product: i.product_id.map(|p| p.to_string()).unwrap_or_default(),
                cycle: i
                    .billing_cycle
                    .map(|c| c.as_str().to_owned())
                    .unwrap_or_default(),
                description: i.description.clone(),
                quantity: dec(&i.quantity),
                price: dec(&i.unit_price),
                discount: dec(&i.discount_percent),
                taxable: i.taxable,
            })
            .collect(),
    };
    form_page(
        &state,
        &tenant,
        &staff,
        ctx,
        Some(q.id),
        form,
        String::new(),
    )
    .await
}

// ---------------------------------------------------------------------------
// detail + actions
// ---------------------------------------------------------------------------

/// A tax line of the stored breakdown.
#[derive(Debug, Clone)]
pub struct TaxView {
    pub name: String,
    pub rate: String,
    pub amount: Decimal,
}

/// Tax lines stored on a quote.
pub fn tax_views(q: &QuoteRow) -> Vec<TaxView> {
    let num = |v: Option<&serde_json::Value>| {
        v.and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_f64().map(|f| f.to_string()))
        })
        .and_then(|s| s.parse::<Decimal>().ok())
        .unwrap_or_default()
    };
    q.tax_lines
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| TaxView {
                    name: t
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned(),
                    rate: num(t.get("rate")).normalize().to_string(),
                    amount: num(t.get("tax_amount")),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "admin/quotes/show.html")]
struct ShowPage {
    ctx: PageCtx,
    quote: QuoteRow,
    client: Option<ClientRow>,
    items: Vec<QuoteItemRow>,
    taxes: Vec<TaxView>,
    expired: bool,
}
crate::page!(ShowPage);

impl ShowPage {
    fn qbadge(&self) -> askama::filters::Safe<String> {
        self.badge(
            self.lbl("quotes.status", self.quote.status.as_str()),
            tone(self.quote.status),
        )
    }
    fn editable(&self) -> bool {
        self.quote.status.is_editable()
    }
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<QuotesView>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let quote = load(&state, &tenant, id).await?;
    let items = qrepo::quote_items(&state.pool, tenant.id, quote.id).await?;
    let client = match quote.client_id {
        Some(c) => clients::get_client(&state.pool, tenant.id, c).await?,
        None => None,
    };
    let number = quote.number();
    let title = ctx.tv("quotes.admin.show_title", "number", &number);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("quotes.admin.title", "/admin/quotes")],
    );
    let expired = quote.is_past_validity(common::today(&ctx.timezone));
    render(&ShowPage {
        taxes: tax_views(&quote),
        quote,
        client,
        items,
        expired,
        ctx,
    })
}

async fn send(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<QuotesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let q = load(&state, &tenant, id).await?;
    let back = format!("/admin/quotes/{}", q.id);
    match state.billing.send_quote(tenant.id, q.id).await {
        Ok((sent, _link)) => {
            au.log(
                &state,
                "quote.send",
                Some(("quote", sent.id.into_uuid())),
                json!({}),
            )
            .await?;
            Ok(done(&ctx, &back, "quotes.admin.sent"))
        }
        Err(e) => Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
}

async fn accept(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<QuotesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    // Staff need billing.manage too: acceptance creates an order and an invoice.
    staff.require(Permission::BILLING_MANAGE)?;
    let q = load(&state, &tenant, id).await?;
    let back = format!("/admin/quotes/{}", q.id);
    let Some(client_id) = q.client_id else {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("quotes.admin.accept_needs_client"),
        ));
    };
    match state
        .billing
        .accept_quote(tenant.id, q.id, client_id, None, Some(au.ip()))
        .await
    {
        Ok(a) => {
            au.log(
                &state,
                "quote.accept",
                Some(("quote", q.id.into_uuid())),
                json!({"order": a.order_id, "invoice": a.invoice_id, "by": "staff"}),
            )
            .await?;
            Ok(done(
                &ctx,
                &format!("/admin/invoices/{}", a.invoice_id),
                "quotes.admin.accepted",
            ))
        }
        Err(e) => Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
}

async fn cancel(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<QuotesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let q = load(&state, &tenant, id).await?;
    let back = format!("/admin/quotes/{}", q.id);
    match state.billing.cancel_quote(tenant.id, q.id).await {
        Ok(_) => {
            au.log(
                &state,
                "quote.cancel",
                Some(("quote", q.id.into_uuid())),
                json!({}),
            )
            .await?;
            Ok(done(&ctx, &back, "quotes.admin.cancelled"))
        }
        Err(e) => Ok(failed(&ctx, &back, common::failure_text(&ctx, &e)?)),
    }
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<QuotesManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let q = load(&state, &tenant, id).await?;
    if qrepo::delete_draft(&state.pool, tenant.id, q.id).await? {
        au.log(
            &state,
            "quote.delete",
            Some(("quote", q.id.into_uuid())),
            json!({}),
        )
        .await?;
        Ok(done(&ctx, "/admin/quotes", "quotes.admin.deleted"))
    } else {
        Ok(failed(
            &ctx,
            &format!("/admin/quotes/{}", q.id),
            ctx.t("quotes.admin.delete_draft_only"),
        ))
    }
}

/// `Content-Disposition: attachment` with an ASCII-only file name.
pub fn pdf_response(filename: &str, data: Vec<u8>) -> Response {
    let safe: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(80)
        .collect();
    (
        [
            (header::CONTENT_TYPE, "application/pdf".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{safe}\""),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ],
        data,
    )
        .into_response()
}

async fn pdf(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<QuotesView>,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let q = load(&state, &tenant, id).await?;
    let rendered = state
        .billing
        .quote_pdf(tenant.id, q.id, Some(ctx.locale))
        .await?;
    Ok(pdf_response(&rendered.filename, rendered.data))
}

/// Quotes of a client for its admin page.
pub async fn client_quotes(
    state: &AppState,
    tenant: &Tenant,
    client_id: ClientId,
) -> AppResult<Vec<QuoteListRow>> {
    Ok(qrepo::list_quotes(
        &state.pool,
        tenant.id,
        &QuoteFilter {
            client_id: Some(client_id),
            ..QuoteFilter::default()
        },
        "q.created_at desc",
        Page::first(20),
    )
    .await?
    .items)
}
