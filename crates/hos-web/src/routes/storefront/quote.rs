//! The secret quote link (`/quote/{id}?token=…`): view, accept, decline and
//! PDF without signing in.
//!
//! The 256-bit token is the authorisation (only its SHA-256 is stored; it
//! expires the day after the quote's validity and dies with any answer). It
//! travels in the query of the e-mailed link and in the body of the
//! accept / decline forms; responses are `no-store`, `noindex` and send no
//! referrer. A quote addressed to a client is accepted for that client; a
//! prospect's quote needs a signed-in client account (an owner, or a
//! sub-user with the `orders` area), to which it is then attached. Every
//! request is rate limited per IP.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::ids::ClientId;
use hos_db::repo::quotes::{self as qrepo, QuoteId, QuoteItemRow, QuoteRow};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, MaybeUser};
use crate::flash::{self, SetFlash};
use crate::routes::admin::common::Ui as _;
use crate::routes::admin::quotes::{TaxView, pdf_response, tax_views, tone};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/quote/{id}", get(show))
        .route("/quote/{id}/accept", post(accept))
        .route("/quote/{id}/decline", post(decline))
        .route("/quote/{id}/pdf", get(pdf))
}

#[derive(Debug, Default, Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: String,
}

#[derive(Debug, Default, Deserialize)]
struct AnswerForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    agree: Option<String>,
}

fn private(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow"),
    );
    res
}

async fn by_token(
    state: &AppState,
    tenant: &Tenant,
    ip: &ClientIp,
    id: &str,
    token: &str,
) -> AppResult<QuoteRow> {
    check_rate(&state.limits.public_token, &ip.key("quote"))?;
    let id: QuoteId = id.parse().map_err(|_| AppError::NotFound)?;
    state
        .billing
        .quote_for_token(tenant.id, id, token)
        .await?
        .ok_or(AppError::NotFound)
}

/// The signed-in client allowed to place orders, if any: `(user, client)`.
async fn ordering_client(
    state: &AppState,
    tenant: &Tenant,
    user: &MaybeUser,
) -> AppResult<Option<(hos_core::ids::UserId, ClientId)>> {
    let Some(s) = user.0.as_ref() else {
        return Ok(None);
    };
    let Ok((user_id, client_id)) = s.principal.require_client(tenant.id) else {
        return Ok(None);
    };
    let access =
        crate::routes::client::load_member_access(state, tenant.id, client_id, user_id).await?;
    Ok(access
        .filter(|a| a.allows("orders"))
        .map(|_| (user_id, client_id)))
}

#[derive(Template)]
#[template(path = "storefront/quote.html")]
struct QuotePage {
    ctx: PageCtx,
    quote: QuoteRow,
    number: String,
    items: Vec<QuoteItemRow>,
    taxes: Vec<TaxView>,
    can_answer: bool,
    action_base: String,
    token: String,
    /// Prospect quote and nobody (allowed) signed in.
    needs_login: bool,
    login_url: String,
    register_url: String,
}
crate::page!(QuotePage);

impl QuotePage {
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
    ip: ClientIp,
    user: MaybeUser,
    Path(id): Path<String>,
    Query(q): Query<TokenQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let quote = by_token(&state, &tenant, &ip, &id, &q.token).await?;
    let items = qrepo::quote_items(&state.pool, tenant.id, quote.id).await?;
    let number = quote.number();
    ctx.load_store().await?;
    let title = ctx.tv("quotes.public.title", "number", &number);
    let mut seo_page = crate::seo::SeoPage::new(title, format!("/quote/{}", quote.id));
    seo_page.noindex = true;
    // A private link: the site's own share image, not one of its own.
    seo_page.image = Some(ctx.theme.share_image_url(&ctx.origin, "/"));
    crate::seo::apply(&mut ctx, seo_page).await?;
    let needs_login =
        quote.client_id.is_none() && ordering_client(&state, &tenant, &user).await?.is_none();
    let here = format!("/quote/{}?token={}", quote.id, q.token.trim());
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let login_url = ctx.url(format!("/auth/login?next={}", enc(&here)));
    let register_url = ctx.url(format!("/auth/register?next={}", enc(&here)));
    let action_base = ctx.url(format!("/quote/{}", quote.id));
    Ok(private(render(&QuotePage {
        taxes: tax_views(&quote),
        can_answer: true,
        quote,
        number,
        items,
        action_base,
        token: q.token.trim().to_owned(),
        needs_login,
        login_url,
        register_url,
        ctx,
    })?))
}

async fn accept(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    user: MaybeUser,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<AnswerForm>,
) -> AppResult<Response> {
    let quote = by_token(&state, &tenant, &ip, &id, &f.token).await?;
    let back = ctx.url(format!("/quote/{}?token={}", quote.id, f.token.trim()));
    if f.agree.is_none() {
        return Ok(private(flash::redirect(
            &back,
            SetFlash::error(ctx.t("quotes.public.agree_required")),
        )));
    }
    let session = ordering_client(&state, &tenant, &user).await?;
    let (client_id, user_id) = match (quote.client_id, session) {
        (Some(c), Some((u, sc))) if sc == c => (c, Some(u)),
        (Some(c), _) => (c, None),
        (None, Some((u, sc))) => (sc, Some(u)),
        (None, None) => {
            // A prospect quote is accepted into an account: sign in first,
            // then come back to this link.
            let here = format!("/quote/{}?token={}", quote.id, f.token.trim());
            let next = url::form_urlencoded::byte_serialize(here.as_bytes()).collect::<String>();
            return Ok(private(flash::redirect(
                &ctx.url(format!("/auth/login?next={next}")),
                SetFlash::error(ctx.t("quotes.public.login_needed")),
            )));
        }
    };
    match state
        .billing
        .accept_quote(tenant.id, quote.id, client_id, user_id, Some(ip.0))
        .await
    {
        Ok(a) => {
            let entry = hos_db::repo::audit::AuditEntry::new(
                Some(tenant.id),
                hos_db::enums::ActorKind::User,
                user_id.map(|u| u.into_uuid()),
                "quote.accept",
            )
            .request(Some(ip.0), None)
            .entity("quote", quote.id.into_uuid())
            .changes(
                serde_json::json!({"order": a.order_id, "invoice": a.invoice_id, "via": "link"}),
            );
            hos_db::repo::audit::write(&state.pool, &entry).await?;
            let to = if user_id.is_some() {
                ctx.url(format!("/client/invoices/{}", a.invoice_id))
            } else {
                ctx.url("/auth/login")
            };
            Ok(private(flash::redirect(
                &to,
                SetFlash::success(ctx.t("quotes.public.accepted")),
            )))
        }
        Err(e) => {
            let status = e.http_status();
            if status < 500 {
                Ok(private(flash::redirect(
                    &ctx.url("/"),
                    SetFlash::error(ctx.tv("quotes.public.failed", "reason", &e)),
                )))
            } else {
                Err(e.into())
            }
        }
    }
}

async fn decline(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<AnswerForm>,
) -> AppResult<Response> {
    let quote = by_token(&state, &tenant, &ip, &id, &f.token).await?;
    let reason: String = f.reason.chars().take(1000).collect();
    state
        .billing
        .decline_quote(tenant.id, quote.id, None, Some(&reason))
        .await?;
    let entry = hos_db::repo::audit::AuditEntry::new(
        Some(tenant.id),
        hos_db::enums::ActorKind::System,
        None,
        "quote.decline",
    )
    .request(Some(ip.0), None)
    .entity("quote", quote.id.into_uuid());
    hos_db::repo::audit::write(&state.pool, &entry).await?;
    Ok(private(flash::redirect(
        &ctx.url("/"),
        SetFlash::success(ctx.t("quotes.public.declined")),
    )))
}

async fn pdf(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    Path(id): Path<String>,
    Query(q): Query<TokenQuery>,
) -> AppResult<Response> {
    let quote = by_token(&state, &tenant, &ip, &id, &q.token).await?;
    let r = state.billing.quote_pdf(tenant.id, quote.id, None).await?;
    Ok(private(pdf_response(&r.filename, r.data)))
}
