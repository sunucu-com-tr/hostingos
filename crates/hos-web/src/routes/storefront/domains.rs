//! Domain search (`/domains`), per-domain availability fragments
//! (`/domains/check`, loaded by htmx one row at a time so results stream in)
//! and the RDAP / whois page (`/whois`).
//!
//! Availability uses the tenant's default registrar integration (secrets
//! decrypted with `SecretBox` + `integration_aad`), falling back to RDAP.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use hos_core::crypto::integration_aad;
use hos_core::domain::IntegrationCategory;
use hos_core::integrations::registrar::DomainRegistrar;
use hos_db::repo::{domains, integrations};
use hos_domains::{DomainName, RdapResult};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppResult;
use crate::extract::{ClientIp, Htmx};
use crate::seo::{self, SeoPage};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// Max. suggestions per search.
const MAX_SUGGESTIONS: usize = 12;

/// One result row.
#[derive(Debug, Clone)]
pub struct DomainRow {
    /// DOM id (`d-example-com`).
    pub id: String,
    /// ASCII (punycode) name.
    pub ascii: String,
    /// Second level label for display (Unicode).
    pub sld: String,
    /// TLD (`com.tr`).
    pub tld: String,
    /// `pending` (htmx loads it), `available`, `taken` or `unknown`.
    pub state: &'static str,
    /// Registration price for one year.
    pub price: Option<Decimal>,
    /// Premium name.
    pub premium: bool,
    /// The exact domain the visitor typed.
    pub exact: bool,
}

impl DomainRow {
    fn pending(d: &DomainName, exact: bool) -> Self {
        Self {
            id: format!("d-{}", d.as_str().replace(['.', ':'], "-")),
            ascii: d.as_str().to_owned(),
            sld: hos_domains::name::to_unicode(d.sld()),
            tld: d.tld().to_owned(),
            state: "pending",
            price: None,
            premium: false,
            exact,
        }
    }
}

/// TLD price table row.
#[derive(Debug, Clone)]
pub struct TldPriceRow {
    /// `com.tr`.
    pub tld: String,
    /// Register / transfer / renew prices (1 year).
    pub register: Option<Decimal>,
    /// Transfer.
    pub transfer: Option<Decimal>,
    /// Renewal.
    pub renew: Option<Decimal>,
    /// Featured.
    pub featured: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SearchQuery {
    #[serde(default)]
    q: String,
    /// `1`: check every suggestion now (no-JS fallback).
    #[serde(default)]
    check: Option<String>,
}

#[derive(Template)]
#[template(path = "storefront/domains.html")]
struct SearchPage {
    ctx: PageCtx,
    q: String,
    rows: Vec<DomainRow>,
    sync: bool,
    invalid: bool,
    prices: Vec<TldPriceRow>,
    /// Editable copy (the `domains` CMS page): transfer steps, FAQ …
    blocks: Vec<super::blocks::Block>,
    /// Lead paragraph (the copy's first paragraph, or the default).
    lead: String,
}
crate::page!(SearchPage);

#[derive(Template)]
#[template(path = "storefront/domain_check.html")]
struct RowFragment {
    ctx: PageCtx,
    row: DomainRow,
}
crate::page!(RowFragment);

async fn registrar(
    state: &AppState,
    tenant: &Tenant,
) -> AppResult<Option<Arc<dyn DomainRegistrar>>> {
    let Some(row) =
        integrations::default_integration(&state.pool, tenant.id, IntegrationCategory::Registrar)
            .await?
    else {
        return Ok(None);
    };
    if !row.is_active {
        return Ok(None);
    }
    let secrets: Value = match &row.secrets_enc {
        Some(enc) => state
            .secret_box
            .decrypt_json(enc, &integration_aad(tenant.id, row.id))
            .map_err(crate::AppError::internal)?,
        None => json!({}),
    };
    match hos_domains::build(&row.kind, &row.config, &secrets) {
        Ok(r) => Ok(Some(r)),
        Err(e) => {
            tracing::warn!(error = %e, integration = %row.id, "registrar integration unusable");
            Ok(None)
        }
    }
}

/// Availability of `domains` (`available` / `taken` / `unknown`, premium price).
async fn check_domains(
    state: &AppState,
    tenant: &Tenant,
    domains: &[String],
) -> AppResult<Vec<(String, &'static str, Option<Decimal>)>> {
    if let Some(reg) = registrar(state, tenant).await? {
        match reg.check_availability(domains).await {
            Ok(list) => {
                return Ok(domains
                    .iter()
                    .map(|d| {
                        let hit = list.iter().find(|a| a.domain.eq_ignore_ascii_case(d));
                        match hit {
                            Some(a) if a.available => (
                                d.clone(),
                                "available",
                                a.premium
                                    .then(|| a.register_price.as_ref().map(|m| m.amount))
                                    .flatten(),
                            ),
                            Some(_) => (d.clone(), "taken", None),
                            None => (d.clone(), "unknown", None),
                        }
                    })
                    .collect());
            }
            Err(e) => tracing::warn!(error = %e, "registrar availability failed; trying RDAP"),
        }
    }
    let mut out = Vec::with_capacity(domains.len());
    for d in domains {
        let state_str = match state.rdap.lookup(d).await {
            Ok(RdapResult::Available) => "available",
            Ok(RdapResult::Registered(_)) => "taken",
            Ok(RdapResult::Unknown { .. }) | Err(_) => "unknown",
        };
        out.push((d.clone(), state_str, None));
    }
    Ok(out)
}

async fn price_table(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
) -> AppResult<Vec<TldPriceRow>> {
    Ok(
        domains::tld_price_table(&state.pool, tenant.id, ctx.currency)
            .await?
            .into_iter()
            .map(|r| TldPriceRow {
                tld: r.tld,
                register: r.register,
                transfer: r.transfer,
                renew: r.renew,
                featured: r.is_featured,
            })
            .collect(),
    )
}

fn apply_result(
    row: &mut DomainRow,
    state: &'static str,
    premium: Option<Decimal>,
    prices: &[TldPriceRow],
) {
    row.state = state;
    row.premium = premium.is_some();
    row.price = premium.or_else(|| {
        prices
            .iter()
            .find(|p| p.tld == row.tld)
            .and_then(|p| p.register)
    });
}

/// `GET /domains?q=`.
pub(super) async fn search(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    Query(query): Query<SearchQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let q = crate::forms::clean(&query.q, 253);
    let prices = price_table(&state, &tenant, &ctx).await?;
    let tlds: Vec<String> = domains::list_tlds(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .map(|t| t.tld)
        .collect();
    let suggestions: Vec<DomainName> = if q.is_empty() {
        Vec::new()
    } else {
        hos_domains::suggest(&q, &tlds)
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .collect()
    };
    let exact_input = q.to_lowercase();
    let mut rows: Vec<DomainRow> = suggestions
        .iter()
        .map(|d| {
            let exact = d.as_str() == exact_input || d.to_unicode() == exact_input;
            DomainRow::pending(d, exact)
        })
        .collect();
    let sync = query.check.as_deref() == Some("1") && !rows.is_empty();
    if sync {
        check_rate(&state.limits.domain_check, &ip.key("domain-check"))?;
        let names: Vec<String> = rows.iter().map(|r| r.ascii.clone()).collect();
        let results = check_domains(&state, &tenant, &names).await?;
        for row in &mut rows {
            if let Some((_, st, premium)) = results.iter().find(|(d, _, _)| *d == row.ascii) {
                apply_result(row, st, *premium, &prices);
            }
        }
    }
    let invalid = !q.is_empty() && rows.is_empty();
    let copy = super::landing::load(&state, &tenant, &ctx, "domains").await?;
    let title = copy
        .as_ref()
        .map(|c| c.page.title.clone())
        .unwrap_or_else(|| ctx.t("storefront.domains.title"));
    let mut blocks = copy
        .as_ref()
        .map(super::landing::Landing::body)
        .unwrap_or_default();
    super::landing::resolve(&state, &tenant, &ctx, &mut blocks, None).await?;
    let lead = match blocks.first() {
        Some(b) if b.kind == "markdown" => {
            let first = b
                .source
                .split("\n\n")
                .next()
                .unwrap_or("")
                .trim()
                .to_owned();
            let rest = b.source[first.len()..].trim().to_owned();
            let text = crate::markdown::plain_text(&first);
            if rest.is_empty() {
                blocks.remove(0);
            } else if let Some(b) = blocks.first_mut() {
                b.html = crate::markdown::render(&rest);
                b.source = rest;
            }
            text
        }
        _ => ctx.t("storefront.domains.lead"),
    };
    let origin = ctx.origin.clone();
    let abs = format!("{origin}{}", ctx.url("/domains"));
    let mut page = SeoPage::new(title.clone(), "/domains").description(lead.clone());
    if let Some(faq) = super::catalog::faq_node(&abs, super::blocks::faq_entries(&blocks)) {
        page = page.node(&faq);
    }
    page.noindex = !q.is_empty();
    seo::apply(&mut ctx, page).await?;
    render(&SearchPage {
        ctx,
        q,
        rows,
        sync,
        invalid,
        prices,
        blocks,
        lead,
    })
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CheckQuery {
    #[serde(default)]
    d: String,
    #[serde(default)]
    exact: Option<String>,
}

/// `GET /domains/check?d=example.com` — one result row (htmx fragment).
pub(super) async fn check(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    htmx: Htmx,
    Query(query): Query<CheckQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    if !htmx.request {
        // Without htmx the full page does the work.
        let target = format!(
            "{}?q={}&check=1",
            ctx.url("/domains"),
            url::form_urlencoded::byte_serialize(query.d.as_bytes()).collect::<String>()
        );
        return Ok(Redirect::to(&target).into_response());
    }
    check_rate(&state.limits.domain_check, &ip.key("domain-check"))?;
    let name = DomainName::parse(query.d.trim())
        .map_err(|_| crate::AppError::bad_request("invalid domain"))?;
    let prices = price_table(&state, &tenant, &ctx).await?;
    let mut row = DomainRow::pending(&name, query.exact.as_deref() == Some("1"));
    let results = check_domains(&state, &tenant, std::slice::from_ref(&row.ascii)).await?;
    let (st, premium) = results
        .first()
        .map(|(_, s, p)| (*s, *p))
        .unwrap_or(("unknown", None));
    apply_result(&mut row, st, premium, &prices);
    render(&RowFragment { ctx, row })
}

// ---------------------------------------------------------------------------
// whois
// ---------------------------------------------------------------------------

/// RDAP facts shown on `/whois`.
#[derive(Debug, Clone, Default)]
pub struct WhoisView {
    /// `available`, `registered`, `unknown`, `invalid` or "" (no query).
    pub state: String,
    /// Domain (Unicode).
    pub domain: String,
    /// Sponsoring registrar.
    pub registrar: String,
    /// Created.
    pub created_at: Option<DateTime<Utc>>,
    /// Expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Nameservers.
    pub nameservers: Vec<String>,
    /// EPP statuses.
    pub statuses: Vec<String>,
    /// Why unknown.
    pub reason: String,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct WhoisQuery {
    #[serde(default)]
    domain: String,
}

#[derive(Template)]
#[template(path = "storefront/whois.html")]
struct WhoisPage {
    ctx: PageCtx,
    q: String,
    result: WhoisView,
}
crate::page!(WhoisPage);

/// `GET /whois?domain=`.
pub(super) async fn whois(
    State(state): State<AppState>,
    ip: ClientIp,
    Query(query): Query<WhoisQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let q = crate::forms::clean(&query.domain, 253);
    let mut result = WhoisView::default();
    if !q.is_empty() {
        check_rate(&state.limits.whois, &ip.key("whois"))?;
        match DomainName::from_user_input(&q) {
            Err(_) => result.state = "invalid".into(),
            Ok(name) => {
                result.domain = name.to_unicode();
                match state.rdap.lookup(name.as_str()).await {
                    Ok(RdapResult::Available) => result.state = "available".into(),
                    Ok(RdapResult::Registered(r)) => {
                        result.state = "registered".into();
                        result.registrar = r.registrar.unwrap_or_default();
                        result.created_at = r.created_at;
                        result.expires_at = r.expires_at;
                        result.nameservers = r.nameservers;
                        result.statuses = r.statuses;
                    }
                    Ok(RdapResult::Unknown { reason }) => {
                        result.state = "unknown".into();
                        result.reason = reason;
                    }
                    Err(e) => {
                        tracing::info!(error = %e, "rdap lookup failed");
                        result.state = "unknown".into();
                    }
                }
            }
        }
    }
    let mut page = SeoPage::new(ctx.t("storefront.whois.title"), "/whois")
        .description(ctx.t("storefront.whois.lead"));
    page.noindex = !q.is_empty();
    seo::apply(&mut ctx, page).await?;
    render(&WhoisPage { ctx, q, result })
}
