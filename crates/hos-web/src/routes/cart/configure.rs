//! Product configuration before it goes into the cart
//! (`/cart/configure/{slug}`): billing cycle, configurable options, addons,
//! the hosting domain (register / transfer / existing) and — for servers —
//! hostname and SSH key. The summary on the right is re-quoted live via htmx
//! (`GET …/quote` with the form fields).

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::Response;
use hos_billing::{Quote, QuoteItem, QuoteRequest};
use hos_core::domain::ProductType;
use hos_core::ids::ProductAddonId;
use hos_core::money::BillingCycle;
use hos_db::enums::{ConfigOptionType, DomainRequirement, LineItemType};
use hos_db::repo::carts::{self, CartRow, NewCartItem};
use hos_db::repo::catalog::{self, CatalogAddon, ProductConfigOption, ProductRow};
use hos_db::repo::domains as domain_repo;
use hos_domains::name::DomainName;
use http::HeaderMap;
use serde_json::{Map, Value, json};

use super::{TotalsView, client_of, resolve_cart, totals_view, user_error};
use crate::error::{AppError, AppResult};
use crate::flash::{self, SetFlash};
use crate::forms::FormErrors;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render, render_status};

/// A configurable option as shown.
pub struct OptionView {
    /// Option id.
    pub id: String,
    /// Label.
    pub name: String,
    /// `select` | `radio` | `check` | `qty`.
    pub kind: &'static str,
    /// `(value id, label)`.
    pub values: Vec<(String, String)>,
    /// Selected value id (`check`: the "yes" value id when ticked).
    pub selected: String,
    /// Quantity (`qty`).
    pub qty: i32,
    /// Bounds (`qty`).
    pub min: i32,
    /// Upper bound (`qty`; 0 = none).
    pub max: i32,
    /// Unit label.
    pub unit: String,
}

/// An addon as shown.
pub struct AddonView {
    /// Addon id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Price for the selected cycle.
    pub price: String,
    /// Ticked.
    pub checked: bool,
}

/// Live summary (fragment).
pub struct SummaryView {
    /// `(label, amount)` per line, children indented.
    pub lines: Vec<(String, String, bool)>,
    /// Totals.
    pub totals: Option<TotalsView>,
    /// Why it cannot be priced.
    pub error: String,
}

/// Parsed configuration form.
#[derive(Debug, Default, Clone)]
pub struct ConfigInput {
    /// Billing cycle.
    pub cycle: Option<BillingCycle>,
    /// `config_selections` JSON.
    pub selections: Map<String, Value>,
    /// Ticked addons.
    pub addons: Vec<ProductAddonId>,
    /// `none` | `register` | `transfer` | `existing`.
    pub domain_mode: String,
    /// Domain as typed.
    pub domain: String,
    /// Transfer code.
    pub epp: String,
    /// Server hostname.
    pub hostname: String,
    /// SSH public key.
    pub ssh_key: String,
}

struct Loaded {
    product: ProductRow,
    cycles: Vec<(BillingCycle, rust_decimal::Decimal, rust_decimal::Decimal)>,
    options: Vec<ProductConfigOption>,
    addons: Vec<CatalogAddon>,
}

impl Loaded {
    fn is_server(&self) -> bool {
        matches!(
            self.product.product_type,
            ProductType::Vps | ProductType::Cloud | ProductType::Dedicated
        )
    }
}

async fn load(state: &AppState, tenant: &Tenant, cart: &CartRow, slug: &str) -> AppResult<Loaded> {
    let product = catalog::product_by_slug(&state.pool, tenant.id, slug)
        .await?
        .filter(|p| !p.is_retired)
        .ok_or(AppError::NotFound)?;
    let cycles = catalog::product_pricing(&state.pool, tenant.id, product.id)
        .await?
        .into_iter()
        .filter(|p| p.currency == cart.currency && p.is_enabled)
        .map(|p| (p.billing_cycle, p.price, p.setup_fee))
        .collect();
    let options =
        catalog::product_config_options(&state.pool, tenant.id, product.id, cart.currency)
            .await?
            .into_iter()
            .filter(|o| !o.option.is_hidden && !o.values.is_empty())
            .collect();
    let addons = catalog::addons_for_product(&state.pool, tenant.id, product.id, cart.currency)
        .await?
        .into_iter()
        .filter(|a| a.addon.show_on_order && !a.addon.is_hidden)
        .collect();
    Ok(Loaded {
        product,
        cycles,
        options,
        addons,
    })
}

fn field<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Reads the form (or query) fields; defaults come from the catalog.
fn parse(l: &Loaded, pairs: &[(String, String)], defaults: bool) -> ConfigInput {
    let mut input = ConfigInput {
        cycle: field(pairs, "cycle")
            .and_then(|c| c.parse().ok())
            .filter(|c| l.cycles.iter().any(|(cc, _, _)| cc == c))
            .or_else(|| l.cycles.first().map(|c| c.0)),
        domain_mode: field(pairs, "domain_mode").unwrap_or("").to_owned(),
        domain: field(pairs, "domain").unwrap_or("").trim().to_owned(),
        epp: field(pairs, "epp").unwrap_or("").trim().to_owned(),
        hostname: field(pairs, "hostname")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase(),
        ssh_key: field(pairs, "ssh_key").unwrap_or("").trim().to_owned(),
        ..ConfigInput::default()
    };
    if input.domain_mode.is_empty() {
        input.domain_mode = match l.product.requires_domain {
            DomainRequirement::None => "none",
            DomainRequirement::Optional => {
                if input.domain.is_empty() {
                    "none"
                } else {
                    "register"
                }
            }
            DomainRequirement::Required => "register",
        }
        .to_owned();
    }
    for o in &l.options {
        let id = o.option.id.to_string();
        let first = o.values[0].id.to_string();
        match o.option.option_type {
            ConfigOptionType::Dropdown | ConfigOptionType::Radio => {
                let v = field(pairs, &format!("opt_{id}"))
                    .filter(|v| o.values.iter().any(|x| x.id.to_string() == *v))
                    .map(str::to_owned)
                    .unwrap_or(first);
                input.selections.insert(id, json!({ "value_id": v }));
            }
            ConfigOptionType::YesNo => {
                if field(pairs, &format!("opt_{id}")).is_some_and(|v| v == first) {
                    input.selections.insert(id, json!({ "value_id": first }));
                }
            }
            ConfigOptionType::Quantity => {
                let q = field(pairs, &format!("qty_{id}"))
                    .and_then(|v| v.trim().parse::<i32>().ok())
                    .unwrap_or(o.option.min_quantity);
                if q > 0 || o.option.min_quantity > 0 {
                    input
                        .selections
                        .insert(id, json!({ "value_id": first, "quantity": q }));
                }
            }
        }
    }
    for a in &l.addons {
        let key = format!("addon_{}", a.addon.id);
        if field(pairs, &key).is_some() && !defaults {
            input.addons.push(a.addon.id);
        }
    }
    input
}

/// Whether `h` is a plausible server hostname (`vps1.example.com`).
pub fn valid_hostname(h: &str) -> bool {
    h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Whether `k` looks like one OpenSSH public key line.
pub fn valid_ssh_key(k: &str) -> bool {
    let mut parts = k.split_whitespace();
    let kind = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    k.len() <= 8192
        && !k.contains('\n')
        && [
            "ssh-ed25519",
            "ssh-rsa",
            "ecdsa-sha2-nistp256",
            "ecdsa-sha2-nistp384",
            "ecdsa-sha2-nistp521",
            "sk-ssh-ed25519@openssh.com",
            "sk-ecdsa-sha2-nistp256@openssh.com",
        ]
        .contains(&kind)
        && body.len() >= 32
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
}

/// What goes into the cart / the quote.
struct Planned {
    items: Vec<QuoteItem>,
    hosting_domain: Option<String>,
    domain_child: Option<(LineItemType, String, i16)>,
}

/// Validates the configuration (field errors) and plans the items.
async fn plan(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    l: &Loaded,
    input: &ConfigInput,
    errors: &mut FormErrors,
) -> AppResult<Option<Planned>> {
    let Some(cycle) = input.cycle else {
        errors.add_general(ctx.t("cart.errors.not_available"));
        return Ok(None);
    };
    let mut product = QuoteItem::product(l.product.id, cycle);
    product.config_selections = Value::Object(input.selections.clone());
    let mut items = vec![product];
    for a in &input.addons {
        items.push(QuoteItem {
            parent: Some(0),
            item_type: LineItemType::Addon,
            product_id: None,
            addon_id: Some(*a),
            domain_name: None,
            billing_cycle: None,
            years: None,
            quantity: 1,
            config_selections: json!({}),
        });
    }
    let mut hosting_domain = None;
    let mut domain_child = None;
    let mode = input.domain_mode.as_str();
    let allowed = match l.product.requires_domain {
        DomainRequirement::None => ["none"].as_slice(),
        DomainRequirement::Optional => ["none", "register", "transfer", "existing"].as_slice(),
        DomainRequirement::Required => ["register", "transfer", "existing"].as_slice(),
    };
    if !allowed.contains(&mode) {
        errors.add("domain", ctx.t("cart.errors.domain_required"));
    } else if mode != "none" {
        match DomainName::from_user_input(&input.domain) {
            Err(_) => errors.add("domain", ctx.t("cart.errors.domain_invalid")),
            Ok(name) => {
                let ascii = name.as_str().to_owned();
                if mode == "register" || mode == "transfer" {
                    match domain_repo::match_tld(&state.pool, tenant.id, &ascii)
                        .await?
                        .filter(|t| t.is_active)
                    {
                        None => errors.add(
                            "domain",
                            ctx.tv("cart.errors.tld_unsupported", "domain", name.to_unicode()),
                        ),
                        Some(tld) => {
                            if mode == "transfer" && tld.requires_epp && input.epp.is_empty() {
                                errors.add(
                                    "epp",
                                    ctx.tv("cart.errors.epp_required", "domain", name.to_unicode()),
                                );
                            }
                            let (kind, years) = if mode == "transfer" {
                                (LineItemType::DomainTransfer, 1)
                            } else {
                                (LineItemType::DomainRegister, tld.min_years.max(1))
                            };
                            let mut d = QuoteItem::domain(kind, &ascii, years);
                            d.parent = Some(0);
                            items.push(d);
                            domain_child = Some((kind, ascii.clone(), years));
                        }
                    }
                }
                hosting_domain = Some(ascii);
            }
        }
    }
    if input.epp.len() > 128 {
        errors.add("epp", ctx.t("cart.errors.epp_invalid"));
    }
    if l.is_server() {
        if !valid_hostname(&input.hostname) {
            errors.add("hostname", ctx.t("cart.errors.hostname"));
        }
        if !input.ssh_key.is_empty() && !valid_ssh_key(&input.ssh_key) {
            errors.add("ssh_key", ctx.t("cart.errors.ssh_key"));
        }
    }
    items[0].domain_name = hosting_domain.clone();
    Ok(Some(Planned {
        items,
        hosting_domain,
        domain_child,
    }))
}

async fn summary(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
    cart: &CartRow,
    items: Vec<QuoteItem>,
) -> AppResult<(SummaryView, Option<Quote>)> {
    let req = QuoteRequest {
        currency: cart.currency,
        items,
        coupon_code: cart.coupon_code.clone(),
        client_id: client_of(ctx, tenant.id).map(|(_, c)| c),
        country_code: None,
        region: None,
    };
    match state.billing.quote(tenant.id, &req).await {
        Ok(q) => {
            let mut lines = Vec::new();
            for line in &q.lines {
                lines.push((
                    line.description.clone(),
                    ctx.money_in(&line.amount, &q.currency),
                    line.parent.is_some(),
                ));
                for c in &line.config_options {
                    let label = if c.quantity > 1 {
                        format!("{}: {} × {}", c.option_name, c.quantity, c.value_label)
                    } else {
                        format!("{}: {}", c.option_name, c.value_label)
                    };
                    let price = c.price * rust_decimal::Decimal::from(c.quantity);
                    lines.push((label, ctx.money_in(&price, &q.currency), true));
                }
            }
            Ok((
                SummaryView {
                    lines,
                    totals: Some(totals_view(ctx, &q)),
                    error: String::new(),
                },
                Some(q),
            ))
        }
        Err(e) => Ok((
            SummaryView {
                lines: Vec::new(),
                totals: None,
                error: user_error(ctx, e)?,
            },
            None,
        )),
    }
}

#[derive(Template)]
#[template(path = "cart/configure.html")]
struct ConfigurePage {
    ctx: PageCtx,
    slug: String,
    name: String,
    tagline: String,
    cycles: Vec<(String, String, String)>,
    options: Vec<OptionView>,
    addons: Vec<AddonView>,
    domain_modes: Vec<(String, String)>,
    is_server: bool,
    input: ConfigInput,
    errors: FormErrors,
    summary: SummaryView,
}
crate::page!(ConfigurePage);

impl ConfigurePage {
    fn cycle_value(&self) -> &str {
        self.input.cycle.map_or("", |c| c.as_str())
    }
}

#[derive(Template)]
#[template(path = "cart/_config_summary.html")]
struct SummaryFragment {
    ctx: PageCtx,
    summary: SummaryView,
}
crate::page!(SummaryFragment);

fn page(
    mut ctx: PageCtx,
    l: &Loaded,
    input: ConfigInput,
    errors: FormErrors,
    summary: SummaryView,
) -> ConfigurePage {
    let title = ctx.tv("cart.configure.title", "name", &l.product.name);
    ctx.set_title(title);
    let locale = ctx.locale.as_str().to_owned();
    let cycles = l
        .cycles
        .iter()
        .map(|(c, price, setup)| {
            let mut label = ctx.money(price);
            if !setup.is_zero() {
                label = format!(
                    "{} · {}",
                    label,
                    ctx.tv("cart.configure.setup_fee", "amount", ctx.money(setup))
                );
            }
            (c.as_str().to_owned(), ctx.cycle(c), label)
        })
        .collect();
    let options = l
        .options
        .iter()
        .map(|o| {
            let id = o.option.id.to_string();
            let sel = input.selections.get(&id);
            let selected = sel
                .and_then(|s| s.get("value_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            OptionView {
                name: catalog::translation(&o.option.translations, &locale, "name")
                    .unwrap_or(&o.option.name)
                    .to_owned(),
                kind: match o.option.option_type {
                    ConfigOptionType::Dropdown => "select",
                    ConfigOptionType::Radio => "radio",
                    ConfigOptionType::YesNo => "check",
                    ConfigOptionType::Quantity => "qty",
                },
                values: o
                    .values
                    .iter()
                    .map(|v| {
                        (
                            v.id.to_string(),
                            catalog::translation(&v.translations, &locale, "label")
                                .unwrap_or(&v.label)
                                .to_owned(),
                        )
                    })
                    .collect(),
                selected,
                qty: sel
                    .and_then(|s| s.get("quantity"))
                    .and_then(Value::as_i64)
                    .and_then(|q| i32::try_from(q).ok())
                    .unwrap_or(o.option.min_quantity),
                min: o.option.min_quantity,
                max: o.option.max_quantity,
                unit: o.option.unit.clone().unwrap_or_default(),
                id,
            }
        })
        .collect();
    let addons = l
        .addons
        .iter()
        .map(|a| {
            let tag = input
                .cycle
                .and_then(|c| a.prices.iter().find(|p| p.billing_cycle == c))
                .or(a.prices.first());
            AddonView {
                id: a.addon.id.to_string(),
                name: catalog::translation(&a.addon.translations, &locale, "name")
                    .unwrap_or(&a.addon.name)
                    .to_owned(),
                description: a.addon.description.clone().unwrap_or_default(),
                price: tag.map_or_else(String::new, |t| {
                    format!("{} {}", ctx.money(&t.price), ctx.per(&t.billing_cycle))
                }),
                checked: input.addons.contains(&a.addon.id),
            }
        })
        .collect();
    let domain_modes = match l.product.requires_domain {
        DomainRequirement::None => Vec::new(),
        DomainRequirement::Optional => vec!["none", "register", "transfer", "existing"],
        DomainRequirement::Required => vec!["register", "transfer", "existing"],
    }
    .into_iter()
    .map(|m| (m.to_owned(), ctx.t(&format!("cart.configure.domain_{m}"))))
    .collect();
    ConfigurePage {
        slug: l.product.slug.clone(),
        name: catalog::translation(&l.product.translations, &locale, "name")
            .unwrap_or(&l.product.name)
            .to_owned(),
        tagline: l.product.tagline.clone().unwrap_or_default(),
        cycles,
        options,
        addons,
        domain_modes,
        is_server: l.is_server(),
        input,
        errors,
        summary,
        ctx,
    }
}

/// `GET /cart/configure/{slug}`.
pub async fn form(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
    let cart = resolved.require()?.clone();
    let l = load(&state, &tenant, &cart, &slug).await?;
    if l.cycles.is_empty() {
        return Ok(resolved.apply(flash::redirect(
            &ctx.url("/cart"),
            SetFlash::error(ctx.t("cart.errors.not_available")),
        )));
    }
    let input = parse(&l, &q, true);
    let mut errors = FormErrors::default();
    let items = match plan(&state, &tenant, &ctx, &l, &input, &mut errors).await? {
        Some(p) if !errors.any() => p.items,
        _ => {
            let mut p = QuoteItem::product(l.product.id, input.cycle.unwrap_or(l.cycles[0].0));
            p.config_selections = Value::Object(input.selections.clone());
            vec![p]
        }
    };
    let (summary, _) = summary(&state, &tenant, &ctx, &cart, items).await?;
    ctx.currency = cart.currency;
    ctx.load_store().await?;
    let res = render(&page(ctx, &l, input, FormErrors::default(), summary))?;
    Ok(resolved.apply(res))
}

/// `GET /cart/configure/{slug}/quote` (htmx): the summary for the current
/// form state.
pub async fn live_quote(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
    let cart = resolved.require()?.clone();
    let l = load(&state, &tenant, &cart, &slug).await?;
    let input = parse(&l, &q, false);
    let mut errors = FormErrors::default();
    let items = match plan(&state, &tenant, &ctx, &l, &input, &mut errors).await? {
        Some(p) => p
            .items
            .into_iter()
            .filter(|i| {
                !matches!(
                    i.item_type,
                    LineItemType::DomainRegister | LineItemType::DomainTransfer
                ) || !errors.has("domain")
            })
            .collect(),
        None => Vec::new(),
    };
    let summary = if items.is_empty() {
        SummaryView {
            lines: Vec::new(),
            totals: None,
            error: ctx.t("cart.errors.not_available"),
        }
    } else {
        summary(&state, &tenant, &ctx, &cart, items).await?.0
    };
    let res = render(&SummaryFragment { ctx, summary })?;
    Ok(resolved.apply(res))
}

/// `POST /cart/configure/{slug}`: validates and adds to the cart.
pub async fn submit(
    State(state): State<AppState>,
    tenant: Tenant,
    headers: HeaderMap,
    Path(slug): Path<String>,
    mut ctx: PageCtx,
    Form(pairs): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let resolved = resolve_cart(&state, &tenant, &ctx, &headers, true).await?;
    let cart = resolved.require()?.clone();
    let l = load(&state, &tenant, &cart, &slug).await?;
    let input = parse(&l, &pairs, false);
    let mut errors = FormErrors::default();
    let planned = plan(&state, &tenant, &ctx, &l, &input, &mut errors).await?;
    if let Some((_, name, _)) = planned.as_ref().and_then(|p| p.domain_child.as_ref()) {
        let lines = carts::cart_lines(&state.pool, tenant.id, cart.id).await?;
        if lines.iter().any(|x| {
            x.item.domain_name.as_deref() == Some(name.as_str())
                && matches!(
                    x.item.item_type,
                    LineItemType::DomainRegister | LineItemType::DomainTransfer
                )
        }) {
            errors.add(
                "domain",
                ctx.tv(
                    "cart.flash.domain_duplicate",
                    "domain",
                    hos_domains::name::to_unicode(name),
                ),
            );
        }
    }
    let (summary_view, quote) = match &planned {
        Some(p) if !errors.any() => summary(&state, &tenant, &ctx, &cart, p.items.clone()).await?,
        _ => (
            SummaryView {
                lines: Vec::new(),
                totals: None,
                error: String::new(),
            },
            None,
        ),
    };
    let (Some(planned), Some(_), false) = (planned, quote, errors.any()) else {
        if !summary_view.error.is_empty() {
            errors.add_general(summary_view.error.clone());
        }
        ctx.currency = cart.currency;
        ctx.load_store().await?;
        let res = render_status(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            &page(ctx, &l, input, errors, summary_view),
        )?;
        return Ok(resolved.apply(res));
    };
    let cycle = input.cycle;
    let mut custom = Map::new();
    if !input.ssh_key.is_empty() {
        custom.insert("ssh_keys".into(), json!([input.ssh_key]));
    }
    let mut tx = state.pool.begin().await?;
    let parent = carts::add_item(
        &mut *tx,
        tenant.id,
        cart.id,
        &NewCartItem {
            parent_item_id: None,
            item_type: LineItemType::Product,
            product_id: Some(l.product.id),
            addon_id: None,
            domain_name: planned.hosting_domain.clone(),
            billing_cycle: cycle,
            years: None,
            quantity: 1,
            config_selections: Value::Object(input.selections.clone()),
            custom_fields: Value::Object(custom),
            hostname: (!input.hostname.is_empty()).then(|| input.hostname.clone()),
            epp_code_enc: None,
        },
    )
    .await?;
    for a in &input.addons {
        carts::add_item(
            &mut *tx,
            tenant.id,
            cart.id,
            &NewCartItem {
                parent_item_id: Some(parent.id),
                item_type: LineItemType::Addon,
                product_id: None,
                addon_id: Some(*a),
                domain_name: None,
                billing_cycle: None,
                years: None,
                quantity: 1,
                config_selections: json!({}),
                custom_fields: json!({}),
                hostname: None,
                epp_code_enc: None,
            },
        )
        .await?;
    }
    if let Some((kind, name, years)) = &planned.domain_child {
        let epp_code_enc = if *kind == LineItemType::DomainTransfer && !input.epp.is_empty() {
            Some(hos_billing::secrets::seal(
                &state.secret_box,
                &input.epp,
                &hos_billing::secrets::epp_code_aad(tenant.id, name),
            )?)
        } else {
            None
        };
        carts::add_item(
            &mut *tx,
            tenant.id,
            cart.id,
            &NewCartItem {
                parent_item_id: Some(parent.id),
                item_type: *kind,
                product_id: None,
                addon_id: None,
                domain_name: Some(name.clone()),
                billing_cycle: None,
                years: Some(*years),
                quantity: 1,
                config_selections: json!({}),
                custom_fields: json!({}),
                hostname: None,
                epp_code_enc,
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(resolved.apply(flash::redirect(
        &ctx.url("/cart"),
        SetFlash::success(ctx.tv("cart.flash.added", "name", &l.product.name)),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames() {
        assert!(valid_hostname("vps1.example.com"));
        assert!(!valid_hostname("vps1"));
        assert!(!valid_hostname("-a.example.com"));
        assert!(!valid_hostname("a..b"));
        assert!(!valid_hostname("a_b.example.com"));
    }

    #[test]
    fn ssh_keys() {
        let k = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl user@host";
        assert!(valid_ssh_key(k));
        assert!(!valid_ssh_key("ssh-ed25519 short"));
        assert!(!valid_ssh_key(
            "rm -rf / AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9ok"
        ));
        assert!(!valid_ssh_key(&format!("{k}\nssh-rsa AAAA")));
    }
}
