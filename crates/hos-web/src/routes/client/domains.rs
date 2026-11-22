//! Client domains: list, detail (expiry, renewal, auto renew, nameservers,
//! registrar lock, WHOIS privacy, EPP code behind password re-entry),
//! contacts and DNS records. Registrar calls go through `hos_billing`
//! (`domain_*`), which checks ownership and domain state.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use hos_core::domain::DomainStatus;
use hos_core::ids::DomainId;
use hos_core::integrations::registrar::{DnsRecord, DnsRecordType};
use hos_db::repo::clients;
use hos_db::repo::domains::{self as domain_repo, DomainContactInput, DomainFilter, DomainRow};
use serde::Deserialize;

use super::{
    Fmt as _, MemberAccess, audit_client, credentials_locked, friendly_error, no_store, outcome,
    parse_id, setup,
};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, PageQuery, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::routes::cart::checkout::country_options;
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, Pager, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/domains", get(list))
        .route("/client/domains/{id}", get(show))
        .route("/client/domains/{id}/renew", post(renew))
        .route("/client/domains/{id}/autorenew", post(auto_renew))
        .route("/client/domains/{id}/nameservers", post(nameservers))
        .route("/client/domains/{id}/lock", post(lock))
        .route("/client/domains/{id}/privacy", post(privacy))
        .route("/client/domains/{id}/epp", post(epp))
        .route(
            "/client/domains/{id}/contacts",
            get(contacts_form).post(contacts_save),
        )
        .route("/client/domains/{id}/dns", get(dns).post(dns_save))
        .route("/client/domains/{id}/dns/delete", post(dns_delete))
}

async fn own_domain(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    id: &str,
) -> AppResult<DomainRow> {
    let id: DomainId = parse_id(id)?;
    domain_repo::get_domain(&state.pool, tenant.id, id)
        .await?
        .filter(|d| d.client_id == client.client_id)
        .ok_or(AppError::NotFound)
}

fn days_left(d: &DomainRow) -> Option<i64> {
    d.expires_at
        .map(|e| (e.date_naive() - Utc::now().date_naive()).num_days())
}

fn manageable(d: &DomainRow) -> bool {
    matches!(d.status, DomainStatus::Active | DomainStatus::Grace)
}

fn crumbs(ctx: &PageCtx, d: &DomainRow, leaf: Option<String>) -> Vec<Crumb> {
    let name = hos_domains::name::to_unicode(&d.name);
    let mut out = vec![Crumb::link(
        ctx.t("client.domains.title"),
        ctx.url("/client/domains"),
    )];
    match leaf {
        Some(l) => {
            out.push(Crumb::link(
                name,
                ctx.url(format!("/client/domains/{}", d.id)),
            ));
            out.push(Crumb::current(l));
        }
        None => out.push(Crumb::current(name)),
    }
    out
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/domains.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<DomainRow>,
    pager: Pager,
    q: String,
}
crate::page!(ListPage);

impl ListPage {
    fn days(&self, d: &DomainRow) -> i64 {
        days_left(d).unwrap_or(9999)
    }
}

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    pq: PageQuery,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let q = pq.q.clone().unwrap_or_default().trim().to_owned();
    let paged = domain_repo::list_domains(
        &state.pool,
        tenant.id,
        &DomainFilter {
            client_id: Some(client.client_id),
            status: None,
            search: (!q.is_empty()).then(|| q.clone()),
        },
        pq.db_page(),
    )
    .await?;
    let pager = Pager::new(&paged, &ctx.url("/client/domains"), &[("q", q.as_str())]);
    let title = ctx.t("client.domains.title");
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
        q,
    })
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/domain.html")]
struct DetailPage {
    ctx: PageCtx,
    d: DomainRow,
    days: Option<i64>,
    years: Vec<(String, String)>,
    supports_privacy: bool,
    nameservers: String,
    epp: Option<String>,
    epp_error: String,
    locked: bool,
    has_password: bool,
}
crate::page!(DetailPage);

impl DetailPage {
    fn manageable(&self) -> bool {
        manageable(&self.d)
    }
    fn expiring(&self) -> bool {
        self.days.is_some_and(|d| d <= 30)
    }
    fn days_text(&self) -> String {
        self.days
            .map(|d| self.tn("client.domains.days_left", d.max(0)))
            .unwrap_or_default()
    }
    fn renewable(&self) -> bool {
        matches!(
            self.d.status,
            DomainStatus::Active
                | DomainStatus::Grace
                | DomainStatus::Redemption
                | DomainStatus::Expired
        ) && !self.years.is_empty()
    }
}

#[allow(clippy::too_many_arguments)]
async fn detail(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    d: DomainRow,
    epp: Option<String>,
    epp_error: String,
) -> AppResult<DetailPage> {
    let tld = domain_repo::match_tld(&state.pool, tenant.id, &d.name).await?;
    let years = match &tld {
        Some(t) => {
            let min = t.min_years.max(1);
            (min..=t.max_years.clamp(min, 10))
                .map(|y| (y.to_string(), ctx.tn("cart.line.years", i64::from(y))))
                .collect()
        }
        None => Vec::new(),
    };
    let crumbs = crumbs(&ctx, &d, None);
    let title = hos_domains::name::to_unicode(&d.name);
    setup(&mut ctx, client, access, title, crumbs);
    Ok(DetailPage {
        days: days_left(&d),
        years,
        supports_privacy: tld.as_ref().is_some_and(|t| t.supports_privacy),
        nameservers: d.nameservers.join("\n"),
        epp,
        epp_error,
        locked: credentials_locked(client),
        has_password: client.user.password_hash.is_some(),
        ctx,
        d,
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
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let page = detail(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        d,
        None,
        String::new(),
    )
    .await?;
    render(&page)
}

#[derive(Debug, Deserialize)]
struct YearsForm {
    years: i16,
}

#[allow(clippy::too_many_arguments)]
async fn renew(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<YearsForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/domains/{}", d.id));
    match state
        .billing
        .order_domain_renewal(tenant.id, d.id, Some(client.client_id), f.years)
        .await
    {
        Ok(inv) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.domain.renewal_ordered",
                Some(("domain", d.id.into_uuid())),
            )
            .await;
            Ok(flash::redirect(
                &ctx.url(format!("/client/invoices/{inv}")),
                SetFlash::success(ctx.t("client.domains.renewal_invoiced")),
            ))
        }
        Err(e) => match friendly_error(&ctx, &e) {
            Some(m) => Ok(flash::redirect(&back, SetFlash::error(m))),
            None => Err(e.into()),
        },
    }
}

#[derive(Debug, Deserialize)]
struct ToggleForm {
    #[serde(default)]
    enabled: String,
}

impl ToggleForm {
    fn on(&self) -> bool {
        self.enabled == "1" || self.enabled == "true"
    }
}

async fn auto_renew(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<ToggleForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let on = f.on();
    let res = state
        .billing
        .domain_set_auto_renew(tenant.id, d.id, Some(client.client_id), on)
        .await;
    let back = ctx.url(format!("/client/domains/{}", d.id));
    outcome(&ctx, &back, res, |()| {
        ctx.t(if on {
            "client.domains.auto_renew_on"
        } else {
            "client.domains.auto_renew_off"
        })
    })
}

#[allow(clippy::too_many_arguments)]
async fn lock(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<ToggleForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let on = f.on();
    let res = state
        .billing
        .domain_set_lock(tenant.id, d.id, Some(client.client_id), on)
        .await;
    if res.is_ok() {
        audit_client(
            &state,
            &tenant,
            &client,
            ip,
            &ua,
            "client.domain.lock",
            Some(("domain", d.id.into_uuid())),
        )
        .await;
    }
    let back = ctx.url(format!("/client/domains/{}", d.id));
    outcome(&ctx, &back, res, |()| {
        ctx.t(if on {
            "client.domains.locked"
        } else {
            "client.domains.unlocked"
        })
    })
}

async fn privacy(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<ToggleForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let on = f.on();
    let res = state
        .billing
        .domain_set_privacy(tenant.id, d.id, Some(client.client_id), on)
        .await;
    let back = ctx.url(format!("/client/domains/{}", d.id));
    outcome(&ctx, &back, res, |()| {
        ctx.t(if on {
            "client.domains.privacy_on"
        } else {
            "client.domains.privacy_off"
        })
    })
}

#[derive(Debug, Deserialize)]
struct NsForm {
    #[serde(default)]
    nameservers: String,
}

/// Parses one nameserver per line (or comma separated).
pub fn parse_nameservers(raw: &str) -> Vec<String> {
    raw.split(['\n', ',', ' '])
        .map(|s| s.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn nameservers(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<NsForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/domains/{}", d.id));
    let list = parse_nameservers(&f.nameservers);
    if list.len() < 2
        || list.len() > 13
        || list
            .iter()
            .any(|n| !crate::routes::cart::configure::valid_hostname(n))
    {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.domains.ns_invalid")),
        ));
    }
    let res = state
        .billing
        .domain_set_nameservers(tenant.id, d.id, Some(client.client_id), &list)
        .await;
    if res.is_ok() {
        audit_client(
            &state,
            &tenant,
            &client,
            ip,
            &ua,
            "client.domain.nameservers",
            Some(("domain", d.id.into_uuid())),
        )
        .await;
    }
    outcome(&ctx, &back, res, |()| ctx.t("client.domains.ns_saved"))
}

#[derive(Debug, Deserialize)]
struct EppForm {
    #[serde(default)]
    password: String,
}

/// `POST /client/domains/{id}/epp`: the transfer code, after the user
/// re-enters their password (not available while impersonating).
#[allow(clippy::too_many_arguments)]
async fn epp(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<EppForm>,
) -> AppResult<Response> {
    super::require_own_session(&client)?;
    check_rate(&state.limits.login, &format!("epp:{}", client.user_id))?;
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let hash = client.user.password_hash.clone();
    let pw = f.password.clone();
    let ok = tokio::task::spawn_blocking(move || {
        hos_auth::password::verify_user_password(&pw, hash.as_deref())
    })
    .await
    .map_err(AppError::internal)?;
    let (code, error) = if !ok {
        (None, ctx.t("client.domains.epp_bad_password"))
    } else {
        match state
            .billing
            .domain_epp_code(tenant.id, d.id, Some(client.client_id))
            .await
        {
            Ok(c) => {
                audit_client(
                    &state,
                    &tenant,
                    &client,
                    ip,
                    &ua,
                    "client.domain.epp_viewed",
                    Some(("domain", d.id.into_uuid())),
                )
                .await;
                (Some(c.expose_str().to_owned()), String::new())
            }
            Err(e) => (
                None,
                friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?,
            ),
        }
    };
    let failed = code.is_none();
    let page = detail(&state, &tenant, &client, &access, ctx, d, code, error).await?;
    let res = if failed {
        render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)?
    } else {
        render(&page)?
    };
    Ok(no_store(res))
}

// ---------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------

/// Registrant contact form.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ContactForm {
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
    #[serde(default)]
    organization: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    phone: String,
    #[serde(default)]
    address_line1: String,
    #[serde(default)]
    address_line2: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    postal_code: String,
    #[serde(default)]
    country_code: String,
}

#[derive(Template)]
#[template(path = "client/domain_contacts.html")]
struct ContactsPage {
    ctx: PageCtx,
    d: DomainRow,
    form: ContactForm,
    errors: FormErrors,
    countries: Vec<(String, String)>,
}
crate::page!(ContactsPage);

async fn contacts_page(
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    d: DomainRow,
    form: ContactForm,
    errors: FormErrors,
) -> ContactsPage {
    let crumbs = crumbs(&ctx, &d, Some(ctx.t("client.domains.contacts")));
    let title = ctx.t("client.domains.contacts");
    setup(&mut ctx, client, access, title, crumbs);
    let countries = country_options(&ctx, &form.country_code);
    ContactsPage {
        ctx,
        d,
        form,
        errors,
        countries,
    }
}

async fn contacts_form(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let contacts = domain_repo::domain_contacts(&state.pool, tenant.id, d.id).await?;
    let form = match contacts
        .iter()
        .find(|c| c.role.as_str() == "registrant")
        .or(contacts.first())
    {
        Some(c) => ContactForm {
            first_name: c.first_name.clone(),
            last_name: c.last_name.clone(),
            organization: c.organization.clone().unwrap_or_default(),
            email: c.email.clone(),
            phone: c.phone.clone().unwrap_or_default(),
            address_line1: c.address_line1.clone().unwrap_or_default(),
            address_line2: c.address_line2.clone().unwrap_or_default(),
            city: c.city.clone().unwrap_or_default(),
            state: c.state.clone().unwrap_or_default(),
            postal_code: c.postal_code.clone().unwrap_or_default(),
            country_code: c.country_code.clone(),
        },
        None => {
            let c = clients::get_client(&state.pool, tenant.id, client.client_id)
                .await?
                .ok_or(AppError::NotFound)?;
            ContactForm {
                first_name: c.first_name,
                last_name: c.last_name,
                organization: c.company_name.unwrap_or_default(),
                email: c.email,
                phone: c.phone.unwrap_or_default(),
                address_line1: c.address_line1.unwrap_or_default(),
                address_line2: c.address_line2.unwrap_or_default(),
                city: c.city.unwrap_or_default(),
                state: c.state.unwrap_or_default(),
                postal_code: c.postal_code.unwrap_or_default(),
                country_code: c.country_code,
            }
        }
    };
    render(&contacts_page(&client, &access, ctx, d, form, FormErrors::default()).await)
}

#[allow(clippy::too_many_arguments)]
async fn contacts_save(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<ContactForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let mut errors = FormErrors::default();
    let req = ctx.t("client.common.required");
    let opt = |s: &str, max: usize| {
        let v = forms::clean(s, max);
        (!v.is_empty()).then_some(v)
    };
    let country = f.country_code.trim().to_ascii_uppercase();
    let input = DomainContactInput {
        first_name: forms::clean(&f.first_name, 100),
        last_name: forms::clean(&f.last_name, 100),
        organization: opt(&f.organization, 200),
        email: forms::normalize_email(&f.email),
        phone: opt(&f.phone, 32),
        address_line1: opt(&f.address_line1, 200),
        address_line2: opt(&f.address_line2, 200),
        city: opt(&f.city, 100),
        state: opt(&f.state, 100),
        postal_code: opt(&f.postal_code, 20),
        country_code: country.clone(),
        external_contact_id: None,
    };
    for (field, empty) in [
        ("first_name", input.first_name.is_empty()),
        ("last_name", input.last_name.is_empty()),
        ("address_line1", input.address_line1.is_none()),
        ("city", input.city.is_none()),
        ("postal_code", input.postal_code.is_none()),
    ] {
        if empty {
            errors.add(field, req.clone());
        }
    }
    if !forms::valid_email(&input.email) {
        errors.add("email", ctx.t("client.common.email_invalid"));
    }
    if input
        .phone
        .as_deref()
        .and_then(|p| hos_billing::provisioning::parse_phone(p, &country))
        .is_none()
    {
        errors.add("phone", ctx.t("cart.checkout.phone_invalid"));
    }
    if country.len() != 2 {
        errors.add("country_code", req);
    }
    if errors.any() {
        let page = contacts_page(&client, &access, ctx, d, f, errors).await;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let res = state
        .billing
        .domain_update_contacts(tenant.id, d.id, Some(client.client_id), &input)
        .await;
    if res.is_ok() {
        audit_client(
            &state,
            &tenant,
            &client,
            ip,
            &ua,
            "client.domain.contacts",
            Some(("domain", d.id.into_uuid())),
        )
        .await;
    }
    let back = ctx.url(format!("/client/domains/{}/contacts", d.id));
    outcome(&ctx, &back, res, |()| {
        ctx.t("client.domains.contacts_saved")
    })
}

// ---------------------------------------------------------------------------
// DNS
// ---------------------------------------------------------------------------

/// DNS record types a client can manage.
pub const DNS_TYPES: &[&str] = &["A", "AAAA", "CNAME", "MX", "TXT", "SRV", "CAA"];

#[derive(Template)]
#[template(path = "client/domain_dns.html")]
struct DnsPage {
    ctx: PageCtx,
    d: DomainRow,
    records: Vec<DnsRecord>,
    error: String,
    form: DnsForm,
    types: Vec<(String, String)>,
}
crate::page!(DnsPage);

/// A record form.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct DnsForm {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "type")]
    record_type: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    ttl: String,
    #[serde(default)]
    priority: String,
}

#[derive(Debug, Default, Deserialize)]
struct EditQuery {
    #[serde(default)]
    edit: String,
}

#[allow(clippy::too_many_arguments)]
async fn dns_page(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    d: DomainRow,
    form: Option<DnsForm>,
    edit: &str,
) -> AppResult<DnsPage> {
    let (records, error) = match state
        .billing
        .domain_dns_records(tenant.id, d.id, Some(client.client_id))
        .await
    {
        Ok(r) => (r, String::new()),
        Err(e) => (
            Vec::new(),
            friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?,
        ),
    };
    let form = form.unwrap_or_else(|| {
        records
            .iter()
            .find(|r| !edit.is_empty() && r.id.as_deref() == Some(edit))
            .map(|r| DnsForm {
                id: r.id.clone().unwrap_or_default(),
                name: r.name.clone(),
                record_type: r.record_type.as_str().to_owned(),
                value: r.value.clone(),
                ttl: r.ttl.map(|t| t.to_string()).unwrap_or_default(),
                priority: r.priority.map(|p| p.to_string()).unwrap_or_default(),
            })
            .unwrap_or_else(|| DnsForm {
                record_type: "A".into(),
                ttl: "3600".into(),
                ..DnsForm::default()
            })
    });
    let crumbs = crumbs(&ctx, &d, Some(ctx.t("client.domains.dns")));
    let title = ctx.t("client.domains.dns");
    setup(&mut ctx, client, access, title, crumbs);
    Ok(DnsPage {
        ctx,
        d,
        records,
        error,
        form,
        types: DNS_TYPES
            .iter()
            .map(|t| ((*t).to_owned(), (*t).to_owned()))
            .collect(),
    })
}

async fn dns(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    Path(id): Path<String>,
    Query(q): Query<EditQuery>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    if !manageable(&d) {
        return Err(AppError::NotFound);
    }
    let page = dns_page(&state, &tenant, &client, &access, ctx, d, None, &q.edit).await?;
    render(&page)
}

#[allow(clippy::too_many_arguments)]
async fn dns_save(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<DnsForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let back = ctx.url(format!("/client/domains/{}/dns", d.id));
    let record_type: Option<DnsRecordType> = DNS_TYPES
        .contains(&f.record_type.as_str())
        .then(|| f.record_type.parse().ok())
        .flatten();
    let parse_num = |s: &str| s.trim().parse::<u32>().ok();
    let record = record_type.map(|t| DnsRecord {
        id: (!f.id.trim().is_empty()).then(|| f.id.trim().to_owned()),
        name: f.name.trim().to_owned(),
        record_type: t,
        value: f.value.trim().to_owned(),
        ttl: parse_num(&f.ttl),
        priority: parse_num(&f.priority).and_then(|p| u16::try_from(p).ok()),
    });
    let checked = record
        .as_ref()
        .ok_or_else(|| ctx.t("client.domains.dns_type_invalid"))
        .and_then(|r| {
            hos_billing::lifecycle::validate_dns_record(r)
                .map_err(|e| friendly_error(&ctx, &e).unwrap_or_else(|| e.to_string()))
        });
    let record = match checked {
        Ok(r) => r,
        Err(msg) => {
            let mut page = dns_page(&state, &tenant, &client, &access, ctx, d, Some(f), "").await?;
            page.ctx.flashes.push(crate::flash::FlashMessage {
                kind: crate::flash::FlashKind::Error,
                text: msg,
            });
            return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
        }
    };
    let res = state
        .billing
        .domain_upsert_dns_record(tenant.id, d.id, Some(client.client_id), &record)
        .await;
    if res.is_ok() {
        audit_client(
            &state,
            &tenant,
            &client,
            ip,
            &ua,
            "client.domain.dns_saved",
            Some(("domain", d.id.into_uuid())),
        )
        .await;
    }
    outcome(&ctx, &back, res, |_| ctx.t("client.domains.dns_saved"))
}

#[derive(Debug, Deserialize)]
struct DnsDeleteForm {
    record: String,
}

#[allow(clippy::too_many_arguments)]
async fn dns_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
    Form(f): Form<DnsDeleteForm>,
) -> AppResult<Response> {
    let d = own_domain(&state, &tenant, &client, &id).await?;
    let res = state
        .billing
        .domain_delete_dns_record(tenant.id, d.id, Some(client.client_id), f.record.trim())
        .await;
    if res.is_ok() {
        audit_client(
            &state,
            &tenant,
            &client,
            ip,
            &ua,
            "client.domain.dns_deleted",
            Some(("domain", d.id.into_uuid())),
        )
        .await;
    }
    let back = ctx.url(format!("/client/domains/{}/dns", d.id));
    outcome(&ctx, &back, res, |()| ctx.t("client.domains.dns_deleted"))
}

#[cfg(test)]
mod tests {
    use super::parse_nameservers;

    #[test]
    fn nameserver_parsing() {
        assert_eq!(
            parse_nameservers("NS1.Example.com.\nns2.example.com, ns3.example.com"),
            vec!["ns1.example.com", "ns2.example.com", "ns3.example.com"]
        );
    }
}
