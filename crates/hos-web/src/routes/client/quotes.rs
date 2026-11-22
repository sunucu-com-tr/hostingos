//! Client quotes (`/client/quotes`, member area `orders`): list, view,
//! accept (→ order + invoice), decline, PDF. Drafts are never shown; every
//! id is checked against the signed-in client account.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_db::repo::quotes::{self as qrepo, QuoteId, QuoteItemRow, QuoteRow, QuoteStatus};
use serde::Deserialize;

use super::{MemberAccess, audit_client, friendly_error, no_store, parse_id, setup};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, UserAgent};
use crate::flash::{self, SetFlash};
use crate::routes::admin::common::Ui as _;
use crate::routes::admin::quotes::{TaxView, pdf_response, tax_views, tone};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/quotes", get(list))
        .route("/client/quotes/{id}", get(show))
        .route("/client/quotes/{id}/accept", post(accept))
        .route("/client/quotes/{id}/decline", post(decline))
        .route("/client/quotes/{id}/pdf", get(pdf))
}

async fn own_quote(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    id: &str,
) -> AppResult<QuoteRow> {
    let id: QuoteId = parse_id(id)?;
    qrepo::get_quote(&state.pool, tenant.id, id)
        .await?
        .filter(|q| q.client_id == Some(client.client_id) && q.status != QuoteStatus::Draft)
        .ok_or(AppError::NotFound)
}

#[derive(Template)]
#[template(path = "client/quotes.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<QuoteRow>,
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
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = qrepo::client_quotes(&state.pool, tenant.id, client.client_id, 100).await?;
    let title = ctx.t("quotes.client.title");
    setup(
        &mut ctx,
        &client,
        &access,
        title.clone(),
        vec![Crumb::current(title)],
    );
    render(&ListPage { rows, ctx })
}

#[derive(Template)]
#[template(path = "client/quote.html")]
struct ShowPage {
    ctx: PageCtx,
    quote: QuoteRow,
    number: String,
    items: Vec<QuoteItemRow>,
    taxes: Vec<TaxView>,
    can_answer: bool,
    action_base: String,
    token: String,
}
crate::page!(ShowPage);

impl ShowPage {
    fn qbadge(&self) -> askama::filters::Safe<String> {
        self.badge(
            self.lbl("quotes.status", self.quote.status.as_str()),
            tone(self.quote.status),
        )
    }
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let quote = own_quote(&state, &tenant, &client, &id).await?;
    let items = qrepo::quote_items(&state.pool, tenant.id, quote.id).await?;
    let number = quote.number();
    let title = ctx.tv("quotes.client.show_title", "number", &number);
    let crumbs = vec![
        Crumb::link(ctx.t("quotes.client.title"), ctx.url("/client/quotes")),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    let today = crate::routes::admin::common::today(&ctx.timezone);
    let can_answer = quote.status == QuoteStatus::Sent && !quote.is_past_validity(today);
    let action_base = ctx.url(format!("/client/quotes/{}", quote.id));
    Ok(no_store(render(&ShowPage {
        taxes: tax_views(&quote),
        quote,
        number,
        items,
        can_answer,
        action_base,
        token: String::new(),
        ctx,
    })?))
}

#[derive(Debug, Deserialize)]
struct AnswerForm {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    agree: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn accept(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<AnswerForm>,
) -> AppResult<Response> {
    let quote = own_quote(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/quotes/{}", quote.id));
    if f.agree.is_none() {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("quotes.public.agree_required")),
        ));
    }
    match state
        .billing
        .accept_quote(
            tenant.id,
            quote.id,
            client.client_id,
            Some(client.user_id),
            Some(ip.0),
        )
        .await
    {
        Ok(a) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "quote.accept",
                Some(("quote", quote.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(
                &ctx.url(format!("/client/invoices/{}", a.invoice_id)),
                SetFlash::success(ctx.t("quotes.public.accepted")),
            ))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(msg) => Ok(flash::redirect(&back, SetFlash::error(msg))),
            None => Err(e.into()),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn decline(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<AnswerForm>,
) -> AppResult<Response> {
    let quote = own_quote(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/quotes/{}", quote.id));
    let reason: String = f.reason.chars().take(1000).collect();
    match state
        .billing
        .decline_quote(tenant.id, quote.id, Some(client.client_id), Some(&reason))
        .await
    {
        Ok(_) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "quote.decline",
                Some(("quote", quote.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(
                &back,
                SetFlash::success(ctx.t("quotes.public.declined")),
            ))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(msg) => Ok(flash::redirect(&back, SetFlash::error(msg))),
            None => Err(e.into()),
        },
    }
}

async fn pdf(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let quote = own_quote(&state, &tenant, &client, &id).await?;
    let r = state
        .billing
        .quote_pdf(tenant.id, quote.id, Some(ctx.locale))
        .await?;
    Ok(pdf_response(&r.filename, r.data))
}
