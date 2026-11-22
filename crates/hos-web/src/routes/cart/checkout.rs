//! Checkout: `/checkout` (sign in / register inline, then billing details),
//! `/checkout/review` (payment method, terms) and `POST /checkout/place`
//! (`hos_billing::Billing::checkout`, then payment). Placing an order twice
//! (double click, back button) never creates a second order: a converted
//! cart leads to the existing order's invoice.

use askama::Template;
use axum::extract::{Form, State};
use axum::response::{IntoResponse, Redirect, Response};
use hos_billing::{CheckoutRequest, StartPayment};
use hos_core::CoreError;
use hos_core::ids::{ClientId, IntegrationId};
use hos_db::enums::{ClientKind, LineItemType};
use hos_db::repo::clients::{self, ClientInput, ClientRow};
use hos_db::repo::{carts, orders, payments};
use http::HeaderMap;
use serde::Deserialize;

use super::{CartView, Resolved, cart_view, client_of, resolve_cart};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::routes::auth::oauth::{ProviderButton, buttons};
use crate::routes::client::billing::{payment_response, usable_gateways};
use crate::routes::client::friendly_error;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

/// `(code, Turkish name, English name)` of the countries offered in billing
/// forms (Turkey first).
pub const COUNTRIES: &[(&str, &str, &str)] = &[
    ("TR", "Türkiye", "Türkiye"),
    ("AZ", "Azerbaycan", "Azerbaijan"),
    ("CY", "Kıbrıs", "Cyprus"),
    ("DE", "Almanya", "Germany"),
    ("NL", "Hollanda", "Netherlands"),
    ("GB", "Birleşik Krallık", "United Kingdom"),
    ("US", "Amerika Birleşik Devletleri", "United States"),
    ("FR", "Fransa", "France"),
    ("AT", "Avusturya", "Austria"),
    ("BE", "Belçika", "Belgium"),
    ("BG", "Bulgaristan", "Bulgaria"),
    ("CH", "İsviçre", "Switzerland"),
    ("DK", "Danimarka", "Denmark"),
    ("ES", "İspanya", "Spain"),
    ("FI", "Finlandiya", "Finland"),
    ("GR", "Yunanistan", "Greece"),
    ("IE", "İrlanda", "Ireland"),
    ("IT", "İtalya", "Italy"),
    ("NO", "Norveç", "Norway"),
    ("PL", "Polonya", "Poland"),
    ("PT", "Portekiz", "Portugal"),
    ("RO", "Romanya", "Romania"),
    ("SE", "İsveç", "Sweden"),
    ("UA", "Ukrayna", "Ukraine"),
    ("GE", "Gürcistan", "Georgia"),
    ("KZ", "Kazakistan", "Kazakhstan"),
    ("UZ", "Özbekistan", "Uzbekistan"),
    ("TM", "Türkmenistan", "Turkmenistan"),
    ("KG", "Kırgızistan", "Kyrgyzstan"),
    ("AE", "Birleşik Arap Emirlikleri", "United Arab Emirates"),
    ("SA", "Suudi Arabistan", "Saudi Arabia"),
    ("QA", "Katar", "Qatar"),
    ("IQ", "Irak", "Iraq"),
    ("IL", "İsrail", "Israel"),
    ("EG", "Mısır", "Egypt"),
    ("CA", "Kanada", "Canada"),
    ("AU", "Avustralya", "Australia"),
    ("JP", "Japonya", "Japan"),
];

/// Country `(code, name)` options in the page language; an unknown stored
/// code is kept as an option so it is not silently changed.
pub fn country_options(ctx: &PageCtx, current: &str) -> Vec<(String, String)> {
    let en = ctx.locale.as_str() == "en";
    let mut out: Vec<(String, String)> = COUNTRIES
        .iter()
        .map(|(c, tr, e)| {
            (
                (*c).to_owned(),
                if en {
                    (*e).to_owned()
                } else {
                    (*tr).to_owned()
                },
            )
        })
        .collect();
    if !current.is_empty() && !out.iter().any(|(c, _)| c == current) {
        out.push((current.to_owned(), current.to_owned()));
    }
    out
}

/// Billing details (client profile) form, shared with the client area.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct DetailsForm {
    /// `individual` / `corporate`.
    #[serde(default)]
    pub kind: String,
    /// First name.
    #[serde(default)]
    pub first_name: String,
    /// Last name.
    #[serde(default)]
    pub last_name: String,
    /// Company (corporate).
    #[serde(default)]
    pub company_name: String,
    /// Tax office.
    #[serde(default)]
    pub tax_office: String,
    /// Tax number (VKN) / for individuals optional national id (TCKN).
    #[serde(default)]
    pub tax_number: String,
    /// National id.
    #[serde(default)]
    pub national_id: String,
    /// Phone.
    #[serde(default)]
    pub phone: String,
    /// Address.
    #[serde(default)]
    pub address_line1: String,
    /// Address 2.
    #[serde(default)]
    pub address_line2: String,
    /// City.
    #[serde(default)]
    pub city: String,
    /// State / district.
    #[serde(default)]
    pub state: String,
    /// Postal code.
    #[serde(default)]
    pub postal_code: String,
    /// ISO country.
    #[serde(default)]
    pub country_code: String,
}

impl DetailsForm {
    /// Prefill from the client record.
    pub fn from_client(c: &ClientRow) -> Self {
        Self {
            kind: c.kind.as_str().to_owned(),
            first_name: c.first_name.clone(),
            last_name: c.last_name.clone(),
            company_name: c.company_name.clone().unwrap_or_default(),
            tax_office: c.tax_office.clone().unwrap_or_default(),
            tax_number: c.tax_number.clone().unwrap_or_default(),
            national_id: c.national_id.clone().unwrap_or_default(),
            phone: c.phone.clone().unwrap_or_default(),
            address_line1: c.address_line1.clone().unwrap_or_default(),
            address_line2: c.address_line2.clone().unwrap_or_default(),
            city: c.city.clone().unwrap_or_default(),
            state: c.state.clone().unwrap_or_default(),
            postal_code: c.postal_code.clone().unwrap_or_default(),
            country_code: c.country_code.clone(),
        }
    }

    /// Whether the corporate fields apply.
    pub fn corporate(&self) -> bool {
        self.kind == "corporate"
    }

    /// Validates; with `full_address` (domains, invoices) phone and postal
    /// address are required. Returns the input to store.
    pub fn validate(
        &self,
        ctx: &PageCtx,
        current: &ClientRow,
        full_address: bool,
        errors: &mut FormErrors,
    ) -> ClientInput {
        let opt = |s: &str, max: usize| {
            let v = forms::clean(s, max);
            (!v.is_empty()).then_some(v)
        };
        let kind = if self.corporate() {
            ClientKind::Corporate
        } else {
            ClientKind::Individual
        };
        let first_name = forms::clean(&self.first_name, 100);
        let last_name = forms::clean(&self.last_name, 100);
        let required = ctx.t("client.common.required");
        if first_name.is_empty() {
            errors.add("first_name", required.clone());
        }
        if last_name.is_empty() {
            errors.add("last_name", required.clone());
        }
        let company_name = opt(&self.company_name, 200);
        let tax_number = opt(&self.tax_number, 32);
        let tax_office = opt(&self.tax_office, 100);
        if kind == ClientKind::Corporate {
            if company_name.is_none() {
                errors.add("company_name", required.clone());
            }
            match &tax_number {
                None => errors.add("tax_number", required.clone()),
                Some(t) if !t.chars().all(|c| c.is_ascii_alphanumeric()) || t.len() < 8 => {
                    errors.add("tax_number", ctx.t("cart.checkout.tax_number_invalid"));
                }
                _ => {}
            }
        }
        let national_id = opt(&self.national_id, 20);
        if let Some(n) = &national_id
            && !(n.len() == 11 && n.chars().all(|c| c.is_ascii_digit()))
            && self.country_code == "TR"
        {
            errors.add("national_id", ctx.t("cart.checkout.national_id_invalid"));
        }
        let country = self.country_code.trim().to_ascii_uppercase();
        if country.len() != 2 || !country.chars().all(|c| c.is_ascii_uppercase()) {
            errors.add("country_code", required.clone());
        }
        let phone = opt(&self.phone, 32);
        if let Some(p) = &phone
            && hos_billing::provisioning::parse_phone(p, &country).is_none()
        {
            errors.add("phone", ctx.t("cart.checkout.phone_invalid"));
        }
        let address_line1 = opt(&self.address_line1, 200);
        let city = opt(&self.city, 100);
        let postal_code = opt(&self.postal_code, 20);
        if full_address {
            if phone.is_none() {
                errors.add("phone", required.clone());
            }
            if address_line1.is_none() {
                errors.add("address_line1", required.clone());
            }
            if city.is_none() {
                errors.add("city", required.clone());
            }
            if postal_code.is_none() {
                errors.add("postal_code", required);
            }
        }
        ClientInput {
            kind,
            first_name,
            last_name,
            company_name: if kind == ClientKind::Corporate {
                company_name
            } else {
                None
            },
            email: current.email.clone(),
            phone,
            address_line1,
            address_line2: opt(&self.address_line2, 200),
            city,
            state: opt(&self.state, 100),
            postal_code,
            country_code: country,
            tax_office: if kind == ClientKind::Corporate {
                tax_office
            } else {
                None
            },
            tax_number: if kind == ClientKind::Corporate {
                tax_number
            } else {
                None
            },
            national_id,
            currency: current.currency,
            locale: current.locale,
            group_id: current.group_id,
            tax_exempt: current.tax_exempt,
            marketing_opt_in: current.marketing_opt_in,
        }
    }
}

/// Whether the cart contains a domain registration / transfer.
async fn cart_has_domains(
    state: &AppState,
    tenant: &Tenant,
    cart: &carts::CartRow,
) -> AppResult<bool> {
    Ok(carts::cart_items(&state.pool, tenant.id, cart.id)
        .await?
        .iter()
        .any(|i| {
            matches!(
                i.item_type,
                LineItemType::DomainRegister | LineItemType::DomainTransfer
            )
        }))
}

#[derive(Template)]
#[template(path = "cart/checkout.html")]
struct CheckoutPage {
    ctx: PageCtx,
    view: CartView,
    /// `login` (guest) | `staff` | `details`.
    step: &'static str,
    providers: Vec<ProviderButton>,
    form: DetailsForm,
    errors: FormErrors,
    countries: Vec<(String, String)>,
    email: String,
    has_domains: bool,
    terms_href: String,
}
crate::page!(CheckoutPage);

impl CheckoutPage {
    fn step_no(&self) -> usize {
        if self.step == "details" { 2 } else { 1 }
    }

    fn steps(&self) -> Vec<String> {
        vec![
            self.t("cart.checkout.step_account"),
            self.t("cart.checkout.step_details"),
            self.t("cart.checkout.step_review"),
        ]
    }
}

/// The terms of service link: the `orders.terms_url` setting, else the
/// footer's terms page (empty when the tenant has neither).
fn terms_link(ctx: &PageCtx) -> String {
    if let Some(url) = ctx.store.as_ref().map(|s| s.terms_url.as_str())
        && !url.is_empty()
    {
        return if url.starts_with('/') {
            ctx.url(url)
        } else {
            url.to_owned()
        };
    }
    ctx.store
        .as_ref()
        .and_then(|s| {
            s.footer_pages
                .iter()
                .find(|p| {
                    let h = p.href.to_ascii_lowercase();
                    h.contains("terms") || h.contains("sozlesme") || h.contains("kosul")
                })
                .or(s.footer_pages.first())
                .map(|p| p.href.clone())
        })
        .unwrap_or_default()
}

async fn open_cart(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    headers: &HeaderMap,
) -> AppResult<Result<(Resolved, carts::CartRow), Response>> {
    let resolved = resolve_cart(state, tenant, ctx, headers, false).await?;
    let cart = match &resolved.cart {
        Some(c)
            if !carts::cart_items(&state.pool, tenant.id, c.id)
                .await?
                .is_empty() =>
        {
            c.clone()
        }
        _ => {
            return Ok(Err(resolved.apply(flash::redirect(
                &ctx.url("/cart"),
                SetFlash::info(ctx.t("cart.checkout.empty")),
            ))));
        }
    };
    Ok(Ok((resolved, cart)))
}

/// `GET /checkout`: account step (guests) or billing details (clients).
pub async fn start(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let (resolved, cart) = match open_cart(&state, &tenant, &ctx, &headers).await? {
        Ok(x) => x,
        Err(res) => return Ok(res),
    };
    let client = client_of(&ctx, tenant.id);
    let view = cart_view(&state, &tenant, &ctx, Some(&cart), client.map(|c| c.1)).await?;
    let has_domains = cart_has_domains(&state, &tenant, &cart).await?;
    ctx.load_store().await?;
    let title = ctx.t("cart.checkout.title");
    ctx.set_title(title);
    let (step, form, email) = match client {
        Some((_, client_id)) => {
            let c = clients::get_client(&state.pool, tenant.id, client_id)
                .await?
                .ok_or(AppError::NotFound)?;
            ("details", DetailsForm::from_client(&c), c.email)
        }
        None if ctx.logged_in() => ("staff", DetailsForm::default(), String::new()),
        None => ("login", DetailsForm::default(), String::new()),
    };
    let providers = if step == "login" {
        buttons(&state, &tenant, &ctx, Some("/checkout")).await?
    } else {
        Vec::new()
    };
    let countries = country_options(&ctx, &form.country_code);
    let terms_href = terms_link(&ctx);
    let res = render(&CheckoutPage {
        ctx,
        view,
        step,
        providers,
        form,
        errors: FormErrors::default(),
        countries,
        email,
        has_domains,
        terms_href,
    })?;
    Ok(resolved.apply(res))
}

/// `POST /checkout/details`: saves the billing profile.
pub async fn save_details(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    mut ctx: PageCtx,
    Form(form): Form<DetailsForm>,
) -> AppResult<Response> {
    let Some((_, client_id)) = client_of(&ctx, tenant.id) else {
        return Ok(Redirect::to(&ctx.url("/checkout")).into_response());
    };
    if ctx
        .session
        .as_ref()
        .is_some_and(|s| !s.principal.may_change_credentials())
    {
        // Staff impersonating the client may place orders but the billing
        // identity stays the client's own.
        return Ok(Redirect::to(&ctx.url("/checkout/review")).into_response());
    }
    let (resolved, cart) = match open_cart(&state, &tenant, &ctx, &headers).await? {
        Ok(x) => x,
        Err(res) => return Ok(res),
    };
    let current = clients::get_client(&state.pool, tenant.id, client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let has_domains = cart_has_domains(&state, &tenant, &cart).await?;
    let mut errors = FormErrors::default();
    let input = form.validate(&ctx, &current, has_domains, &mut errors);
    crate::einvoice::check_tax_ids(&state, tenant.id, &ctx, &input, &mut errors).await?;
    if !errors.any() {
        clients::update_client(&state.pool, tenant.id, client_id, &input).await?;
        return Ok(resolved.apply(Redirect::to(&ctx.url("/checkout/review")).into_response()));
    }
    let view = cart_view(&state, &tenant, &ctx, Some(&cart), Some(client_id)).await?;
    ctx.load_store().await?;
    let title = ctx.t("cart.checkout.title");
    ctx.set_title(title);
    let countries = country_options(&ctx, &form.country_code);
    let terms_href = terms_link(&ctx);
    let res = render_status(
        http::StatusCode::UNPROCESSABLE_ENTITY,
        &CheckoutPage {
            ctx,
            view,
            step: "details",
            providers: Vec::new(),
            form,
            errors,
            countries,
            email: current.email,
            has_domains,
            terms_href,
        },
    )?;
    Ok(resolved.apply(res))
}

/// A payment choice on the review page.
pub struct PayChoice {
    /// Form value (integration id or `credit`).
    pub value: String,
    /// Label.
    pub label: String,
    /// Note (credit balance).
    pub note: String,
}

#[derive(Template)]
#[template(path = "cart/review.html")]
struct ReviewPage {
    ctx: PageCtx,
    view: CartView,
    cart_id: String,
    choices: Vec<PayChoice>,
    selected: String,
    billing_name: String,
    billing_lines: Vec<String>,
    terms_href: String,
}
crate::page!(ReviewPage);

impl ReviewPage {
    fn steps(&self) -> Vec<String> {
        vec![
            self.t("cart.checkout.step_account"),
            self.t("cart.checkout.step_details"),
            self.t("cart.checkout.step_review"),
        ]
    }
}

/// Payment choices for an amount: usable gateways plus account credit when
/// it covers everything.
pub async fn pay_choices(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    client_id: ClientId,
    currency: hos_core::money::Currency,
    amount: rust_decimal::Decimal,
) -> AppResult<Vec<PayChoice>> {
    let mut out: Vec<PayChoice> = usable_gateways(state, tenant, currency)
        .await?
        .into_iter()
        .map(|g| PayChoice {
            value: g.id.to_string(),
            label: g.name,
            note: String::new(),
        })
        .collect();
    let credit = payments::credit_balance(&state.pool, tenant.id, client_id, currency).await?;
    if credit > rust_decimal::Decimal::ZERO && credit >= amount {
        out.push(PayChoice {
            value: "credit".into(),
            label: ctx.t("cart.checkout.pay_credit"),
            note: ctx.tv(
                "cart.checkout.credit_balance",
                "amount",
                ctx.money_in(&credit, &currency),
            ),
        });
    }
    Ok(out)
}

/// `GET /checkout/review`.
pub async fn review(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let Some((_, client_id)) = client_of(&ctx, tenant.id) else {
        return Ok(Redirect::to(&ctx.url("/checkout")).into_response());
    };
    let (resolved, cart) = match open_cart(&state, &tenant, &ctx, &headers).await? {
        Ok(x) => x,
        Err(res) => return Ok(res),
    };
    let client = clients::get_client(&state.pool, tenant.id, client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if cart_has_domains(&state, &tenant, &cart).await?
        && hos_billing::provisioning::client_contact(&client).is_err()
    {
        return Ok(resolved.apply(flash::redirect(
            &ctx.url("/checkout"),
            SetFlash::warning(ctx.t("cart.checkout.complete_details")),
        )));
    }
    let view = cart_view(&state, &tenant, &ctx, Some(&cart), Some(client_id)).await?;
    let total = state
        .billing
        .quote_cart(tenant.id, cart.id, Some(client_id), None)
        .await
        .map(|q| q.total)
        .unwrap_or_default();
    let choices = pay_choices(&state, &tenant, &ctx, client_id, cart.currency, total).await?;
    let selected = cart
        .payment_integration_id
        .map(|i| i.to_string())
        .filter(|s| choices.iter().any(|c| c.value == *s))
        .or_else(|| {
            client
                .default_gateway_id
                .map(|i| i.to_string())
                .filter(|s| choices.iter().any(|c| c.value == *s))
        })
        .or_else(|| choices.first().map(|c| c.value.clone()))
        .unwrap_or_default();
    ctx.load_store().await?;
    let title = ctx.t("cart.checkout.review_title");
    ctx.set_title(title);
    let billing_name = match &client.company_name {
        Some(c) => c.clone(),
        None => format!("{} {}", client.first_name, client.last_name),
    };
    let billing_lines = [
        client.tax_number.as_ref().map(|t| {
            format!("{} {}", client.tax_office.clone().unwrap_or_default(), t)
                .trim()
                .to_owned()
        }),
        client.address_line1.clone(),
        client.address_line2.clone(),
        Some(
            [
                client.postal_code.clone(),
                client.city.clone(),
                client.state.clone(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" "),
        )
        .filter(|s| !s.is_empty()),
        Some(client.country_code.clone()),
        Some(client.email.clone()),
    ]
    .into_iter()
    .flatten()
    .collect();
    let terms_href = terms_link(&ctx);
    let res = render(&ReviewPage {
        ctx,
        view,
        cart_id: cart.id.to_string(),
        choices,
        selected,
        billing_name,
        billing_lines,
        terms_href,
    })?;
    Ok(resolved.apply(res))
}

/// `POST /checkout/place`.
#[derive(Debug, Deserialize)]
pub struct PlaceForm {
    cart: String,
    #[serde(default)]
    payment: String,
    terms: Option<String>,
    #[serde(default)]
    notes: String,
}

/// Where an already placed order continues: its invoice.
async fn existing_order(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    order_id: hos_core::ids::OrderId,
) -> AppResult<Response> {
    let order = orders::get_order(&state.pool, tenant.id, order_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let to = match order.invoice_id {
        Some(i) => ctx.url(format!("/client/invoices/{i}")),
        None => ctx.url("/client/orders"),
    };
    Ok(flash::redirect(
        &to,
        SetFlash::info(ctx.tv(
            "cart.checkout.already_placed",
            "number",
            &order.order_number,
        )),
    ))
}

/// Places the order.
pub async fn place(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<PlaceForm>,
) -> AppResult<Response> {
    let Some((user_id, client_id)) = client_of(&ctx, tenant.id) else {
        return Ok(Redirect::to(&ctx.url("/checkout")).into_response());
    };
    let cart_id: hos_core::ids::CartId = f.cart.parse().map_err(|_| AppError::NotFound)?;
    let cart = carts::get_cart(&state.pool, tenant.id, cart_id)
        .await?
        .filter(|c| c.user_id == Some(user_id) || c.client_id == Some(client_id))
        .ok_or(AppError::NotFound)?;
    if let Some(order_id) = cart.converted_order_id {
        return existing_order(&state, &tenant, &ctx, order_id).await;
    }
    let review = ctx.url("/checkout/review");
    if !forms::checked(f.terms.as_deref()) {
        return Ok(flash::redirect(
            &review,
            SetFlash::error(ctx.t("cart.checkout.terms_required")),
        ));
    }
    // Plan limit (`Services`) before the order is created; the visitor gets a
    // generic message that reveals nothing about the store's plan.
    let has_products = carts::cart_items(&state.pool, tenant.id, cart.id)
        .await?
        .iter()
        .any(|i| i.product_id.is_some() && i.parent_item_id.is_none());
    if has_products
        && !crate::routes::admin::settings::has_room(
            &state,
            tenant.id,
            crate::routes::platform::limits::Limit::Services,
        )
        .await?
    {
        return Ok(flash::redirect(
            &review,
            SetFlash::error(ctx.t("admin_settings.limits.checkout_unavailable")),
        ));
    }
    let total = match state
        .billing
        .quote_cart(tenant.id, cart.id, Some(client_id), None)
        .await
    {
        Ok(q) => q.total,
        Err(e) => {
            let msg = friendly_error(&ctx, &e).ok_or(AppError::from(e))?;
            return Ok(flash::redirect(&ctx.url("/cart"), SetFlash::error(msg)));
        }
    };
    let choices = pay_choices(&state, &tenant, &ctx, client_id, cart.currency, total).await?;
    let payment = f.payment.trim();
    let gateway: Option<IntegrationId> = if total.is_zero() || payment == "credit" {
        None
    } else {
        match payment.parse::<IntegrationId>() {
            Ok(id) if choices.iter().any(|c| c.value == payment) => Some(id),
            _ => {
                return Ok(flash::redirect(
                    &review,
                    SetFlash::error(ctx.t("cart.checkout.payment_required")),
                ));
            }
        }
    };
    if payment == "credit" && !total.is_zero() && !choices.iter().any(|c| c.value == "credit") {
        return Ok(flash::redirect(
            &review,
            SetFlash::error(ctx.t("cart.checkout.credit_short")),
        ));
    }
    let notes = forms::clean(&f.notes, 2000);
    carts::update_cart_checkout(
        &state.pool,
        tenant.id,
        cart.id,
        cart.currency,
        gateway,
        (!notes.is_empty()).then_some(notes.as_str()),
    )
    .await?;
    let req = CheckoutRequest {
        cart_id: cart.id,
        client_id,
        user_id: Some(user_id),
        payment_integration_id: gateway,
        ip: Some(ip.0),
        user_agent: ua.0.clone(),
        notes: (!notes.is_empty()).then_some(notes),
    };
    let out = match state.billing.checkout(tenant.id, &req).await {
        Ok(o) => o,
        Err(CoreError::Conflict(_)) => {
            if let Some(order_id) = carts::get_cart(&state.pool, tenant.id, cart.id)
                .await?
                .and_then(|c| c.converted_order_id)
            {
                return existing_order(&state, &tenant, &ctx, order_id).await;
            }
            return Ok(flash::redirect(
                &review,
                SetFlash::error(ctx.t("client.errors.state")),
            ));
        }
        Err(e) => {
            let msg = friendly_error(&ctx, &e).ok_or(AppError::from(e))?;
            return Ok(flash::redirect(&review, SetFlash::error(msg)));
        }
    };
    let invoice_url = ctx.url(format!("/client/invoices/{}", out.invoice_id));
    let placed = ctx.tv("cart.checkout.placed", "number", &out.order_number);
    if out.paid || out.balance.is_zero() {
        return Ok(flash::redirect(&invoice_url, SetFlash::success(placed)));
    }
    if payment == "credit" {
        return Ok(
            match state
                .billing
                .pay_with_credit(
                    tenant.id,
                    out.invoice_id,
                    Some(client_id),
                    None,
                    Some(user_id),
                )
                .await
            {
                Ok(_) => flash::redirect(
                    &invoice_url,
                    SetFlash::success(placed).and(
                        crate::flash::FlashKind::Success,
                        ctx.t("client.invoices.paid_with_credit"),
                    ),
                ),
                Err(e) => {
                    let msg = friendly_error(&ctx, &e).ok_or(AppError::from(e))?;
                    flash::redirect(
                        &invoice_url,
                        SetFlash::success(placed).and(crate::flash::FlashKind::Error, msg),
                    )
                }
            },
        );
    }
    let invoice = hos_db::repo::invoices::get_invoice(&state.pool, tenant.id, out.invoice_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let started = state
        .billing
        .start_payment(
            tenant.id,
            out.invoice_id,
            &StartPayment {
                integration_id: gateway,
                client_id: Some(client_id),
                return_url: None,
                cancel_url: None,
                ip: Some(ip.0),
                save_payment_method: false,
                installments: None,
            },
        )
        .await;
    match started {
        Ok(s) => payment_response(&state, &tenant, ctx, &invoice, s).await,
        Err(e) => {
            let msg = friendly_error(&ctx, &e).ok_or(AppError::from(e))?;
            Ok(flash::redirect(
                &invoice_url,
                SetFlash::success(placed).and(crate::flash::FlashKind::Error, msg),
            ))
        }
    }
}
