//! Tenant settings (`/admin/settings/*`, `settings.manage`).
//!
//! | Tab | Route | Storage |
//! |---|---|---|
//! | General | `/admin/settings/general` | `tenants` company columns + logo / favicon (`branding`, `tenant_assets`), see [`general`] |
//! | Appearance | `/admin/settings/appearance` | `appearance.*` + `branding` colours, see [`appearance`] and [`crate::theme`] |
//! | Domains | `/admin/settings/domains` | `tenant_domains`, see [`domains`] |
//! | Localisation | `/admin/settings/localization` | `tenants.default_locale / default_currency / timezone` |
//! | Billing | `/admin/settings/billing` | `billing.*` keys read by `hos_billing::settings::BillingSettings` + `invoice_sequences` |
//! | e-Invoice | `/admin/settings/einvoice` | `einvoice.*`, `invoice_pdf.*`, see [`einvoice`] |
//! | Orders | `/admin/settings/orders` | `orders.*`, `domains.*` |
//! | Automation | `/admin/settings/automation` | `scheduled_tasks`, see [`automation`] |
//! | E-mail | `/admin/settings/email` | `branding.email_from_*`, `mail.reply_to`, `email_templates`, see [`email`] |
//! | Security | `/admin/settings/security` | `security.*`, see [`security`] |
//! | Registration | `/admin/settings/registration` | `auth.*` |
//!
//! The key/value tabs share one generic form ([`Def`] → [`FieldView`],
//! `templates/admin/settings/form.html`): values are validated per kind,
//! only changed keys are written, `audit_log` gets `{key: {from, to}}` and
//! the tenant / storefront / security caches are dropped.

pub mod appearance;
pub mod automation;
pub mod domains;
pub mod einvoice;
pub mod email;
pub mod general;
pub mod security;

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use hos_core::domain::Locale;
use hos_core::ids::TenantId;
use hos_core::money::Currency;
use hos_db::repo::{catalog, invoices, platform};
use http::StatusCode;
use serde_json::{Value, json};

use super::common::{self, Audit, FormData, done};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::forms::FormErrors;
use crate::routes::platform::limits::{Limit, check_plan_limit};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/settings", get(index))
        .route(
            "/admin/settings/localization",
            get(localization).post(localization_save),
        )
        .route("/admin/settings/billing", get(billing).post(billing_save))
        .route("/admin/settings/orders", get(orders).post(orders_save))
        .route(
            "/admin/settings/security",
            get(security_page).post(security_save),
        )
        .route(
            "/admin/settings/registration",
            get(registration).post(registration_save),
        )
        .merge(general::routes())
        .merge(appearance::routes())
        .merge(domains::routes())
        .merge(automation::routes())
        .merge(email::routes())
        .merge(einvoice::routes())
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// The settings tabs.
pub(crate) fn tabs(ctx: &PageCtx) -> Vec<NavItem> {
    let mut list = vec![
        (
            "admin_settings.tabs.general",
            "/admin/settings/general".into(),
        ),
        (
            "admin_settings.tabs.appearance",
            "/admin/settings/appearance".into(),
        ),
        (
            "admin_settings.tabs.domains",
            "/admin/settings/domains".into(),
        ),
        (
            "admin_settings.tabs.localization",
            "/admin/settings/localization".into(),
        ),
        (
            "admin_settings.tabs.billing",
            "/admin/settings/billing".into(),
        ),
        ("einvoice.settings.tab", "/admin/settings/einvoice".into()),
        (
            "admin_settings.tabs.orders",
            "/admin/settings/orders".into(),
        ),
        (
            "admin_settings.tabs.automation",
            "/admin/settings/automation".into(),
        ),
        ("admin_settings.tabs.email", "/admin/settings/email".into()),
        (
            "admin_settings.tabs.security",
            "/admin/settings/security".into(),
        ),
        (
            "admin_settings.tabs.registration",
            "/admin/settings/registration".into(),
        ),
    ];
    // Single-company mode: every host is the company; there are no
    // domains to add or verify.
    if ctx.single_company() {
        list.retain(|(_, href)| href != "/admin/settings/domains");
    }
    common::tabs(ctx, &list)
}

/// Title + breadcrumbs of a settings tab (`Settings › <tab>`).
pub(crate) fn page_chrome(ctx: &mut PageCtx, staff: &CurrentStaff, tab_key: &str) {
    let tab = ctx.t(tab_key);
    let title = ctx.tv("admin_settings.title_tab", "tab", &tab);
    ctx.set_title(title);
    let trail = vec![
        (ctx.t("admin_settings.title"), "/admin/settings".to_owned()),
        (tab, String::new()),
    ];
    ctx.panel = Some(common::chrome(ctx, staff, &trail));
}

/// Drops every cache that holds tenant settings.
pub(crate) fn invalidate_caches(state: &AppState, tenant: &Tenant) {
    state.tenants.invalidate_all();
    state.store_chrome.invalidate_all();
    security::invalidate(tenant.id);
    crate::theme::invalidate(tenant.id);
}

/// Whether one more `limit` item fits the tenant's plan: the README's
/// `check_plan_limit` (called right before creating the counted record),
/// with its `Conflict` turned into `false` so callers can show their own
/// translated message (customer-facing flows a generic one).
pub(crate) async fn has_room(
    state: &AppState,
    tenant_id: TenantId,
    limit: Limit,
) -> AppResult<bool> {
    match check_plan_limit(state, tenant_id, limit).await {
        Ok(()) => Ok(true),
        Err(AppError::Conflict(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Writes the changed `(key, value)` settings, audits the diff
/// (`settings.<section>`) and drops the caches. Returns the changed keys.
pub(crate) async fn save_settings(
    state: &AppState,
    tenant: &Tenant,
    au: &Audit,
    section: &str,
    values: &[(String, Value)],
) -> AppResult<Vec<String>> {
    let current = platform::get_settings(&state.pool, tenant.id, "").await?;
    let mut diff = serde_json::Map::new();
    let mut tx = state.pool.begin().await?;
    for (key, value) in values {
        let old = current.get(key).cloned().unwrap_or(Value::Null);
        if &old == value {
            continue;
        }
        platform::set_setting(&mut *tx, tenant.id, key, value).await?;
        diff.insert(key.clone(), json!({"from": old, "to": value}));
    }
    tx.commit().await?;
    let changed: Vec<String> = diff.keys().cloned().collect();
    if !diff.is_empty() {
        au.log(
            state,
            &format!("settings.{section}"),
            None,
            Value::Object(diff),
        )
        .await?;
    }
    invalidate_caches(state, tenant);
    Ok(changed)
}

/// `GET /admin/settings` → the first tab.
async fn index(
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> Response {
    Redirect::to(&ctx.url("/admin/settings/general")).into_response()
}

// ---------------------------------------------------------------------------
// generic key/value form
// ---------------------------------------------------------------------------

/// How a field is validated and stored.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Kind {
    /// Integer in `[min, max]`.
    Int(i64, i64),
    /// Checkbox.
    Bool,
    /// Comma separated integers in `[min, max]` (sorted, deduplicated).
    IntList(i64, i64),
    /// Free text (≤ n chars, may be empty).
    Text(usize),
    /// `https://…` URL or a local `/path` (may be empty).
    Link,
    /// Non-negative decimal (stored as a string).
    Decimal,
    /// ISO 3166 alpha-2 country code.
    Country,
    /// One of fixed values; labels are `admin_settings.options.<value>`.
    Choice(&'static [&'static str]),
    /// One of the options supplied by the page.
    Select,
    /// Host names, one per line (stored as an array).
    Hosts,
}

/// Where a field's value lives.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Store {
    /// `tenant_settings` key = field name.
    Setting,
    /// A member of a JSON object setting: `(setting key, member)`.
    Member(&'static str, &'static str),
    /// Handled by the page (tenant columns, invoice sequence).
    Virtual,
}

/// A field definition.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Def {
    /// Form field name (also the setting key for [`Store::Setting`]).
    pub name: &'static str,
    /// Label key.
    pub label: &'static str,
    /// Hint key (`""`: none).
    pub hint: &'static str,
    pub kind: Kind,
    pub store: Store,
    /// Default value as JSON text.
    pub default: &'static str,
}

const fn def(
    name: &'static str,
    label: &'static str,
    hint: &'static str,
    kind: Kind,
    default: &'static str,
) -> Def {
    Def {
        name,
        label,
        hint,
        kind,
        store: Store::Setting,
        default,
    }
}

const fn member(
    name: &'static str,
    parent: &'static str,
    sub: &'static str,
    label: &'static str,
    kind: Kind,
    default: &'static str,
) -> Def {
    Def {
        name,
        label,
        hint: "",
        kind,
        store: Store::Member(parent, sub),
        default,
    }
}

const fn virt(name: &'static str, label: &'static str, hint: &'static str, kind: Kind) -> Def {
    Def {
        name,
        label,
        hint,
        kind,
        store: Store::Virtual,
        default: "null",
    }
}

/// A titled group of fields.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Section {
    pub title: &'static str,
    pub lead: &'static str,
    pub defs: &'static [Def],
}

/// One rendered field.
#[derive(Debug, Clone, Default)]
pub struct FieldView {
    pub name: String,
    pub label: String,
    /// `number`, `text`, `email`, `url`, `checkbox`, `select`, `textarea`.
    pub kind: &'static str,
    pub value: String,
    pub checked: bool,
    pub options: Vec<(String, String)>,
    pub hint: String,
    pub error: String,
    pub inputmode: &'static str,
    pub wide: bool,
}

/// One rendered section.
#[derive(Debug, Clone, Default)]
pub struct SectionView {
    pub title: String,
    pub lead: String,
    pub fields: Vec<FieldView>,
}

/// Values of a form by field name.
pub(crate) type Values = BTreeMap<String, Value>;

fn default_of(d: &Def) -> Value {
    serde_json::from_str(d.default).unwrap_or(Value::Null)
}

/// Current values of `sections` from the settings map (virtual fields from `virtuals`).
pub(crate) fn current_values(
    sections: &[Section],
    settings: &BTreeMap<String, Value>,
    virtuals: &Values,
) -> Values {
    let mut out = Values::new();
    for d in sections.iter().flat_map(|s| s.defs.iter()) {
        let v = match d.store {
            Store::Setting => settings.get(d.name).cloned(),
            Store::Member(parent, sub) => settings.get(parent).and_then(|o| o.get(sub)).cloned(),
            Store::Virtual => virtuals.get(d.name).cloned(),
        }
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| default_of(d));
        out.insert(d.name.to_owned(), v);
    }
    out
}

fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.as_str(), "true" | "1" | "on" | "yes"),
        Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
        _ => false,
    }
}

/// Builds the view of `sections`.
pub(crate) fn build_view(
    ctx: &PageCtx,
    sections: &[Section],
    values: &Values,
    errors: &FormErrors,
    options: &BTreeMap<&'static str, Vec<(String, String)>>,
) -> Vec<SectionView> {
    sections
        .iter()
        .map(|s| SectionView {
            title: ctx.t(s.title),
            lead: if s.lead.is_empty() {
                String::new()
            } else {
                ctx.t(s.lead)
            },
            fields: s
                .defs
                .iter()
                .map(|d| {
                    let v = values.get(d.name).cloned().unwrap_or(Value::Null);
                    let (kind, inputmode, opts, wide) = match d.kind {
                        Kind::Int(..) => ("number", "numeric", Vec::new(), false),
                        Kind::Bool => ("checkbox", "", Vec::new(), true),
                        Kind::IntList(..) => ("text", "numeric", Vec::new(), false),
                        Kind::Text(n) => ("text", "", Vec::new(), n > 200),
                        Kind::Link => ("url", "url", Vec::new(), false),
                        Kind::Decimal => ("text", "decimal", Vec::new(), false),
                        Kind::Country => ("text", "", Vec::new(), false),
                        Kind::Choice(values) => (
                            "select",
                            "",
                            values
                                .iter()
                                .map(|o| {
                                    (
                                        (*o).to_owned(),
                                        ctx.t(&format!("admin_settings.options.{o}")),
                                    )
                                })
                                .collect(),
                            false,
                        ),
                        Kind::Select => (
                            "select",
                            "",
                            options.get(d.name).cloned().unwrap_or_default(),
                            false,
                        ),
                        Kind::Hosts => ("textarea", "", Vec::new(), true),
                    };
                    let value = match d.kind {
                        Kind::Hosts => v
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default(),
                        _ => display(&v),
                    };
                    FieldView {
                        name: d.name.to_owned(),
                        label: ctx.t(d.label),
                        kind,
                        checked: truthy(&v),
                        value,
                        options: opts,
                        hint: if d.hint.is_empty() {
                            String::new()
                        } else {
                            ctx.t(d.hint)
                        },
                        error: errors.get(d.name).to_owned(),
                        inputmode,
                        wide,
                    }
                })
                .collect(),
        })
        .collect()
}

fn valid_link(s: &str) -> bool {
    if s.starts_with('/') {
        return hos_auth::oauth::safe_return_path(s).is_some();
    }
    url::Url::parse(s)
        .ok()
        .is_some_and(|u| matches!(u.scheme(), "https" | "http") && u.host().is_some())
}

/// Validates the submitted form. Returns the parsed values (also for the
/// re-rendered form) and the errors.
pub(crate) fn parse_form(
    ctx: &PageCtx,
    sections: &[Section],
    f: &FormData,
    options: &BTreeMap<&'static str, Vec<(String, String)>>,
) -> (Values, FormErrors) {
    let mut out = Values::new();
    let mut errors = FormErrors::default();
    for d in sections.iter().flat_map(|s| s.defs.iter()) {
        let raw = f.text(d.name, 4000);
        let range_err =
            |min: i64, max: i64| ctx.tv2("admin_settings.errors.range", "min", min, "max", max);
        let v = match d.kind {
            Kind::Bool => Value::Bool(f.flag(d.name)),
            Kind::Int(min, max) => match raw.parse::<i64>() {
                Ok(n) if (min..=max).contains(&n) => json!(n),
                _ => {
                    errors.add(d.name, range_err(min, max));
                    Value::String(raw)
                }
            },
            Kind::IntList(min, max) => {
                let parts: Vec<&str> = raw
                    .split([',', ';', ' '])
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .collect();
                let mut nums = Vec::new();
                let mut bad = parts.len() > 10;
                for p in &parts {
                    match p.parse::<i64>() {
                        Ok(n) if (min..=max).contains(&n) => nums.push(n),
                        _ => bad = true,
                    }
                }
                if bad {
                    errors.add(
                        d.name,
                        ctx.tv2("admin_settings.errors.list", "min", min, "max", max),
                    );
                    Value::String(raw)
                } else {
                    nums.sort_unstable();
                    nums.dedup();
                    json!(nums)
                }
            }
            Kind::Text(max) => Value::String(raw.chars().take(max).collect()),
            Kind::Link => {
                if !raw.is_empty() && !valid_link(&raw) {
                    errors.add(d.name, ctx.t("admin_settings.errors.link"));
                }
                Value::String(raw)
            }
            Kind::Decimal => match common::parse_decimal(&raw) {
                Ok(Some(n)) if n >= rust_decimal::Decimal::ZERO => {
                    Value::String(n.normalize().to_string())
                }
                Ok(None) => Value::String("0".into()),
                _ => {
                    errors.add(d.name, ctx.t("admin.common.invalid_amount"));
                    Value::String(raw)
                }
            },
            Kind::Country => {
                let cc = raw.to_ascii_uppercase();
                if cc.len() != 2 || !cc.bytes().all(|b| b.is_ascii_uppercase()) {
                    errors.add(d.name, ctx.t("admin_settings.errors.country"));
                }
                Value::String(cc)
            }
            Kind::Choice(values) => {
                if !values.contains(&raw.as_str()) {
                    errors.add(d.name, ctx.t("admin.common.invalid_value"));
                }
                Value::String(raw)
            }
            Kind::Select => {
                let ok = options
                    .get(d.name)
                    .is_some_and(|o| o.iter().any(|(v, _)| *v == raw));
                if !ok {
                    errors.add(d.name, ctx.t("admin.common.invalid_value"));
                }
                Value::String(raw)
            }
            Kind::Hosts => {
                let body = f.body(d.name, 2000);
                let hosts: Vec<String> = body
                    .split(['\n', ',', ' '])
                    .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect();
                if hosts.len() > 13
                    || hosts
                        .iter()
                        .any(|h| !crate::routes::platform::tenants::valid_host(h))
                {
                    errors.add(d.name, ctx.t("admin_settings.errors.hosts"));
                }
                json!(hosts)
            }
        };
        out.insert(d.name.to_owned(), v);
    }
    (out, errors)
}

/// `(setting key, value)` pairs of the stored fields (members merged into
/// their parent objects on top of the current value).
pub(crate) fn stored_pairs(
    sections: &[Section],
    values: &Values,
    settings: &BTreeMap<String, Value>,
) -> Vec<(String, Value)> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for d in sections.iter().flat_map(|s| s.defs.iter()) {
        let Some(v) = values.get(d.name) else {
            continue;
        };
        match d.store {
            Store::Setting => {
                out.insert(d.name.to_owned(), v.clone());
            }
            Store::Member(parent, sub) => {
                let entry = out.entry(parent.to_owned()).or_insert_with(|| {
                    settings
                        .get(parent)
                        .filter(|o| o.is_object())
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                });
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert(sub.to_owned(), v.clone());
                }
            }
            Store::Virtual => {}
        }
    }
    out.into_iter().collect()
}

/// The generic settings form page.
#[derive(Template)]
#[template(path = "admin/settings/form.html")]
pub struct FormPage {
    pub ctx: PageCtx,
    pub tabs: Vec<NavItem>,
    pub active: String,
    pub action: String,
    pub lead: String,
    pub sections: Vec<SectionView>,
    pub errors: FormErrors,
    pub notes: Vec<String>,
}
crate::page!(FormPage);

/// Renders a key/value tab (422 when `errors` has any).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_form(
    mut ctx: PageCtx,
    staff: &CurrentStaff,
    tab_key: &str,
    path: &str,
    lead_key: &str,
    sections: &[Section],
    values: &Values,
    errors: FormErrors,
    options: &BTreeMap<&'static str, Vec<(String, String)>>,
    notes: Vec<String>,
) -> AppResult<Response> {
    page_chrome(&mut ctx, staff, tab_key);
    let view = build_view(&ctx, sections, values, &errors, options);
    let page = FormPage {
        tabs: tabs(&ctx),
        active: ctx.url(path),
        action: ctx.url(path),
        lead: ctx.t(lead_key),
        sections: view,
        errors,
        notes,
        ctx,
    };
    if page.errors.any() {
        render_status(StatusCode::UNPROCESSABLE_ENTITY, &page)
    } else {
        render(&page)
    }
}

// ---------------------------------------------------------------------------
// localisation
// ---------------------------------------------------------------------------

const LOCALIZATION: &[Section] = &[Section {
    title: "admin_settings.localization.section",
    lead: "admin_settings.localization.section_lead",
    defs: &[
        virt(
            "default_locale",
            "admin_settings.localization.default_locale",
            "admin_settings.localization.default_locale_hint",
            Kind::Choice(&["tr", "en"]),
        ),
        virt(
            "default_currency",
            "admin_settings.localization.default_currency",
            "admin_settings.localization.default_currency_hint",
            Kind::Select,
        ),
        virt(
            "timezone",
            "admin_settings.localization.timezone",
            "admin_settings.localization.timezone_hint",
            Kind::Select,
        ),
    ],
}];

/// Time zones offered in the select (IANA names).
const TIMEZONES: &[&str] = &[
    "Europe/Istanbul",
    "UTC",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Paris",
    "Europe/Amsterdam",
    "Europe/Moscow",
    "Europe/Kyiv",
    "Asia/Baku",
    "Asia/Dubai",
    "Asia/Tbilisi",
    "Asia/Almaty",
    "Asia/Tashkent",
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "Asia/Singapore",
    "Asia/Tokyo",
    "Australia/Sydney",
];

async fn localization_options(
    state: &AppState,
    tenant: &Tenant,
) -> AppResult<BTreeMap<&'static str, Vec<(String, String)>>> {
    let currencies = catalog::list_currencies(&state.pool, tenant.id, false)
        .await?
        .into_iter()
        .map(|c| {
            (
                c.code.as_str().to_owned(),
                format!("{} — {}", c.code.as_str(), c.name),
            )
        })
        .collect::<Vec<_>>();
    let mut tz: Vec<(String, String)> = TIMEZONES
        .iter()
        .map(|z| ((*z).to_owned(), (*z).to_owned()))
        .collect();
    if !TIMEZONES.contains(&tenant.timezone.as_str()) {
        tz.insert(0, (tenant.timezone.clone(), tenant.timezone.clone()));
    }
    let mut m = BTreeMap::new();
    m.insert("default_currency", currencies);
    m.insert("timezone", tz);
    Ok(m)
}

fn localization_virtuals(tenant: &Tenant) -> Values {
    let mut v = Values::new();
    v.insert(
        "default_locale".into(),
        json!(tenant.default_locale.as_str()),
    );
    v.insert(
        "default_currency".into(),
        json!(tenant.default_currency.as_str()),
    );
    v.insert("timezone".into(), json!(tenant.timezone));
    v
}

async fn localization(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let options = localization_options(&state, &tenant).await?;
    let values = current_values(
        LOCALIZATION,
        &BTreeMap::new(),
        &localization_virtuals(&tenant),
    );
    render_form(
        ctx,
        &staff,
        "admin_settings.tabs.localization",
        "/admin/settings/localization",
        "admin_settings.localization.lead",
        LOCALIZATION,
        &values,
        FormErrors::default(),
        &options,
        Vec::new(),
    )
}

async fn localization_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let options = localization_options(&state, &tenant).await?;
    let (values, mut errors) = parse_form(&ctx, LOCALIZATION, &f, &options);
    let tz = values
        .get("timezone")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if tz.parse::<chrono_tz::Tz>().is_err() {
        errors.add("timezone", ctx.t("admin.common.invalid_value"));
    }
    let locale: Option<Locale> = values
        .get("default_locale")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok());
    let currency: Option<Currency> = values
        .get("default_currency")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok());
    let (Some(locale), Some(currency), false) = (locale, currency, errors.any()) else {
        return render_form(
            ctx,
            &staff,
            "admin_settings.tabs.localization",
            "/admin/settings/localization",
            "admin_settings.localization.lead",
            LOCALIZATION,
            &values,
            errors,
            &options,
            Vec::new(),
        );
    };
    let before = json!({
        "default_locale": tenant.default_locale.as_str(),
        "default_currency": tenant.default_currency.as_str(),
        "timezone": tenant.timezone,
    });
    let after = json!({
        "default_locale": locale.as_str(),
        "default_currency": currency.as_str(),
        "timezone": tz,
    });
    platform::update_tenant_profile(&state.pool, tenant.id, &tenant.name, locale, currency, &tz)
        .await?;
    if before != after {
        au.log(
            &state,
            "settings.localization",
            None,
            json!({"from": before, "to": after}),
        )
        .await?;
    }
    invalidate_caches(&state, &tenant);
    Ok(done(
        &ctx,
        "/admin/settings/localization",
        "admin.common.saved",
    ))
}

// ---------------------------------------------------------------------------
// billing
// ---------------------------------------------------------------------------

const LATE_FEE_TYPES: &[&str] = &["percent", "fixed"];

const BILLING: &[Section] = &[
    Section {
        title: "admin_settings.billing.numbering",
        lead: "admin_settings.billing.numbering_lead",
        defs: &[
            virt(
                "invoice_prefix",
                "admin_settings.billing.invoice_prefix",
                "admin_settings.billing.invoice_prefix_hint",
                Kind::Text(12),
            ),
            virt(
                "invoice_padding",
                "admin_settings.billing.invoice_padding",
                "",
                Kind::Int(1, 18),
            ),
            virt(
                "invoice_reset_yearly",
                "admin_settings.billing.invoice_reset_yearly",
                "",
                Kind::Bool,
            ),
        ],
    },
    Section {
        title: "admin_settings.billing.terms",
        lead: "",
        defs: &[
            def(
                "billing.payment_terms_days",
                "admin_settings.billing.payment_terms_days",
                "admin_settings.billing.payment_terms_days_hint",
                Kind::Int(0, 365),
                "7",
            ),
            def(
                "billing.order_invoice_due_days",
                "admin_settings.billing.order_invoice_due_days",
                "",
                Kind::Int(0, 365),
                "0",
            ),
            def(
                "billing.invoice_days_before_due",
                "admin_settings.billing.invoice_days_before_due",
                "admin_settings.billing.invoice_days_before_due_hint",
                Kind::Int(0, 90),
                "7",
            ),
            def(
                "billing.reminder_days_before_due",
                "admin_settings.billing.reminder_days_before_due",
                "admin_settings.billing.days_list_hint",
                Kind::IntList(0, 60),
                "[3, 1]",
            ),
            def(
                "billing.overdue_notice_days",
                "admin_settings.billing.overdue_notice_days",
                "admin_settings.billing.days_list_hint",
                Kind::IntList(1, 365),
                "[1, 3, 7]",
            ),
            def(
                "billing.suspend_after_days",
                "admin_settings.billing.suspend_after_days",
                "admin_settings.billing.zero_never",
                Kind::Int(0, 365),
                "5",
            ),
            def(
                "billing.terminate_after_days",
                "admin_settings.billing.terminate_after_days",
                "admin_settings.billing.zero_never",
                Kind::Int(0, 730),
                "30",
            ),
        ],
    },
    Section {
        title: "admin_settings.billing.late_fee",
        lead: "admin_settings.billing.late_fee_lead",
        defs: &[
            member(
                "late_fee_type",
                "billing.late_fee",
                "type",
                "admin_settings.billing.late_fee_type",
                Kind::Choice(LATE_FEE_TYPES),
                "\"percent\"",
            ),
            member(
                "late_fee_amount",
                "billing.late_fee",
                "amount",
                "admin_settings.billing.late_fee_amount",
                Kind::Decimal,
                "\"0\"",
            ),
            member(
                "late_fee_min_amount",
                "billing.late_fee",
                "min_amount",
                "admin_settings.billing.late_fee_min_amount",
                Kind::Decimal,
                "\"0\"",
            ),
            member(
                "late_fee_after_days",
                "billing.late_fee",
                "after_days",
                "admin_settings.billing.late_fee_after_days",
                Kind::Int(0, 365),
                "0",
            ),
        ],
    },
    Section {
        title: "admin_settings.billing.funds",
        lead: "admin_settings.billing.funds_lead",
        defs: &[
            def(
                "billing.add_funds_min",
                "admin_settings.billing.add_funds_min",
                "",
                Kind::Decimal,
                "\"1\"",
            ),
            def(
                "billing.add_funds_max",
                "admin_settings.billing.add_funds_max",
                "",
                Kind::Decimal,
                "\"1000000\"",
            ),
            def(
                "billing.credit_balance_max",
                "admin_settings.billing.credit_balance_max",
                "admin_settings.billing.credit_balance_max_hint",
                Kind::Decimal,
                "\"0\"",
            ),
        ],
    },
    Section {
        title: "admin_settings.billing.tax_credit",
        lead: "",
        defs: &[
            def(
                "billing.prices_include_tax",
                "admin_settings.billing.prices_include_tax",
                "admin_settings.billing.prices_include_tax_hint",
                Kind::Bool,
                "false",
            ),
            def(
                "billing.apply_credit_automatically",
                "admin_settings.billing.apply_credit_automatically",
                "",
                Kind::Bool,
                "true",
            ),
            def(
                "billing.auto_charge_stored",
                "admin_settings.billing.auto_charge_stored",
                "",
                Kind::Bool,
                "true",
            ),
            def(
                "billing.suspend_on_chargeback",
                "admin_settings.billing.suspend_on_chargeback",
                "",
                Kind::Bool,
                "true",
            ),
            def(
                "billing.tax_domains",
                "admin_settings.billing.tax_domains",
                "",
                Kind::Bool,
                "true",
            ),
            def(
                "billing.b2b_tax_exempt",
                "admin_settings.billing.b2b_tax_exempt",
                "admin_settings.billing.b2b_tax_exempt_hint",
                Kind::Bool,
                "false",
            ),
            def(
                "billing.tax_home_country",
                "admin_settings.billing.tax_home_country",
                "",
                Kind::Country,
                "\"TR\"",
            ),
        ],
    },
    Section {
        title: "admin_settings.billing.payments",
        lead: "admin_settings.billing.payments_lead",
        // Read by `routes::api::billing::allowed_return_hosts`.
        defs: &[def(
            "payments.return_url_hosts",
            "admin_settings.billing.return_url_hosts",
            "admin_settings.billing.return_url_hosts_hint",
            Kind::Hosts,
            "[]",
        )],
    },
];

async fn billing_values(
    state: &AppState,
    tenant: &Tenant,
) -> AppResult<(Values, BTreeMap<String, Value>)> {
    let settings = platform::get_settings(&state.pool, tenant.id, "").await?;
    let seq = hos_db::repo::admin_settings::invoice_sequence(&state.pool, tenant.id).await?;
    let mut v = Values::new();
    v.insert(
        "invoice_prefix".into(),
        json!(seq.as_ref().map(|s| s.prefix.clone()).unwrap_or_default()),
    );
    v.insert(
        "invoice_padding".into(),
        json!(seq.as_ref().map_or(6, |s| i64::from(s.padding))),
    );
    v.insert(
        "invoice_reset_yearly".into(),
        json!(seq.as_ref().is_some_and(|s| s.reset_yearly)),
    );
    Ok((current_values(BILLING, &settings, &v), settings))
}

fn number_example(ctx: &PageCtx, values: &Values) -> String {
    let prefix = values
        .get("invoice_prefix")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let padding = values
        .get("invoice_padding")
        .and_then(Value::as_i64)
        .and_then(|p| usize::try_from(p).ok())
        .unwrap_or(6);
    let yearly = values.get("invoice_reset_yearly").is_some_and(truthy);
    let year = chrono::Datelike::year(&common::today(&ctx.timezone));
    let example = invoices::format_invoice_number(prefix, yearly.then_some(year), 1, padding);
    ctx.tv("admin_settings.billing.number_example", "example", example)
}

async fn billing(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let (values, _) = billing_values(&state, &tenant).await?;
    let notes = vec![number_example(&ctx, &values)];
    render_form(
        ctx,
        &staff,
        "admin_settings.tabs.billing",
        "/admin/settings/billing",
        "admin_settings.billing.lead",
        BILLING,
        &values,
        FormErrors::default(),
        &BTreeMap::new(),
        notes,
    )
}

async fn billing_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let (_, settings) = billing_values(&state, &tenant).await?;
    let (values, mut errors) = parse_form(&ctx, BILLING, &f, &BTreeMap::new());
    let prefix = values
        .get("invoice_prefix")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
    {
        errors.add("invoice_prefix", ctx.t("admin_settings.errors.prefix"));
    }
    if errors.any() {
        let notes = vec![number_example(&ctx, &values)];
        return render_form(
            ctx,
            &staff,
            "admin_settings.tabs.billing",
            "/admin/settings/billing",
            "admin_settings.billing.lead",
            BILLING,
            &values,
            errors,
            &BTreeMap::new(),
            notes,
        );
    }
    let pairs = stored_pairs(BILLING, &values, &settings);
    save_settings(&state, &tenant, &au, "billing", &pairs).await?;
    let padding = values
        .get("invoice_padding")
        .and_then(Value::as_i64)
        .and_then(|p| i16::try_from(p).ok())
        .unwrap_or(6);
    let yearly = values.get("invoice_reset_yearly").is_some_and(truthy);
    let before = hos_db::repo::admin_settings::invoice_sequence(&state.pool, tenant.id).await?;
    let changed = before
        .as_ref()
        .is_none_or(|s| s.prefix != prefix || s.padding != padding || s.reset_yearly != yearly);
    if changed {
        invoices::configure_invoice_sequence(
            &state.pool,
            tenant.id,
            &prefix,
            padding,
            yearly,
            None,
        )
        .await?;
        au.log(
            &state,
            "settings.invoice_numbering",
            None,
            json!({
                "from": before.map(|s| json!({"prefix": s.prefix, "padding": s.padding, "reset_yearly": s.reset_yearly})),
                "to": {"prefix": prefix, "padding": padding, "reset_yearly": yearly},
            }),
        )
        .await?;
    }
    Ok(done(&ctx, "/admin/settings/billing", "admin.common.saved"))
}

// ---------------------------------------------------------------------------
// orders
// ---------------------------------------------------------------------------

const ORDERS: &[Section] = &[
    Section {
        title: "admin_settings.orders.section",
        lead: "",
        defs: &[
            def(
                "orders.auto_accept",
                "admin_settings.orders.auto_accept",
                "admin_settings.orders.auto_accept_hint",
                Kind::Bool,
                "true",
            ),
            def(
                "orders.terms_url",
                "admin_settings.orders.terms_url",
                "admin_settings.orders.terms_url_hint",
                Kind::Link,
                "\"\"",
            ),
        ],
    },
    Section {
        title: "admin_settings.orders.domains",
        lead: "",
        defs: &[
            def(
                "domains.default_nameservers",
                "admin_settings.orders.nameservers",
                "admin_settings.orders.nameservers_hint",
                Kind::Hosts,
                "[]",
            ),
            def(
                "domains.expiry_notice_days",
                "admin_settings.orders.expiry_notice_days",
                "admin_settings.billing.days_list_hint",
                Kind::IntList(1, 365),
                "[30, 14, 7, 1]",
            ),
            member(
                "price_margin_percent",
                "domains.price_margin",
                "percent",
                "admin_settings.orders.margin_percent",
                Kind::Decimal,
                "\"20\"",
            ),
            member(
                "price_margin_fixed",
                "domains.price_margin",
                "fixed",
                "admin_settings.orders.margin_fixed",
                Kind::Decimal,
                "\"0\"",
            ),
        ],
    },
];

async fn orders(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let settings = platform::get_settings(&state.pool, tenant.id, "").await?;
    let values = current_values(ORDERS, &settings, &Values::new());
    render_form(
        ctx,
        &staff,
        "admin_settings.tabs.orders",
        "/admin/settings/orders",
        "admin_settings.orders.lead",
        ORDERS,
        &values,
        FormErrors::default(),
        &BTreeMap::new(),
        Vec::new(),
    )
}

async fn orders_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let settings = platform::get_settings(&state.pool, tenant.id, "").await?;
    let (values, errors) = parse_form(&ctx, ORDERS, &f, &BTreeMap::new());
    if errors.any() {
        return render_form(
            ctx,
            &staff,
            "admin_settings.tabs.orders",
            "/admin/settings/orders",
            "admin_settings.orders.lead",
            ORDERS,
            &values,
            errors,
            &BTreeMap::new(),
            Vec::new(),
        );
    }
    let pairs = stored_pairs(ORDERS, &values, &settings);
    save_settings(&state, &tenant, &au, "orders", &pairs).await?;
    Ok(done(&ctx, "/admin/settings/orders", "admin.common.saved"))
}

// ---------------------------------------------------------------------------
// security
// ---------------------------------------------------------------------------

const SECURITY: &[Section] = &[
    Section {
        title: "admin_settings.security.passwords",
        lead: "",
        defs: &[
            def(
                security::KEY_PASSWORD_MIN,
                "admin_settings.security.password_min_length",
                "",
                Kind::Int(
                    security::PASSWORD_MIN_RANGE.0,
                    security::PASSWORD_MIN_RANGE.1,
                ),
                "10",
            ),
            def(
                security::KEY_PASSWORD_COMMON,
                "admin_settings.security.password_reject_common",
                "",
                Kind::Bool,
                "true",
            ),
        ],
    },
    Section {
        title: "admin_settings.security.staff",
        lead: "",
        defs: &[def(
            security::KEY_STAFF_2FA,
            "admin_settings.security.staff_2fa_required",
            "admin_settings.security.staff_2fa_required_hint",
            Kind::Bool,
            "false",
        )],
    },
    Section {
        title: "admin_settings.security.sessions",
        lead: "admin_settings.security.sessions_lead",
        defs: &[
            def(
                security::KEY_IDLE_MINUTES,
                "admin_settings.security.session_idle_minutes",
                "",
                Kind::Int(security::IDLE_RANGE.0, security::IDLE_RANGE.1),
                "60",
            ),
            def(
                security::KEY_ABSOLUTE_HOURS,
                "admin_settings.security.session_absolute_hours",
                "",
                Kind::Int(security::ABSOLUTE_RANGE.0, security::ABSOLUTE_RANGE.1),
                "12",
            ),
        ],
    },
];

async fn security_page(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let settings = platform::get_settings(&state.pool, tenant.id, "security.").await?;
    let values = current_values(SECURITY, &settings, &Values::new());
    render_form(
        ctx,
        &staff,
        "admin_settings.tabs.security",
        "/admin/settings/security",
        "admin_settings.security.lead",
        SECURITY,
        &values,
        FormErrors::default(),
        &BTreeMap::new(),
        Vec::new(),
    )
}

async fn security_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let (values, mut errors) = parse_form(&ctx, SECURITY, &f, &BTreeMap::new());
    // Requiring 2FA while the acting staff member has none would lock them
    // out of the page right after saving: ask them to enable it first.
    if values.get(security::KEY_STAFF_2FA).is_some_and(truthy) && !staff.user.totp_enabled {
        errors.add(
            security::KEY_STAFF_2FA,
            ctx.t("admin_settings.security.enable_own_2fa_first"),
        );
    }
    if errors.any() {
        return render_form(
            ctx,
            &staff,
            "admin_settings.tabs.security",
            "/admin/settings/security",
            "admin_settings.security.lead",
            SECURITY,
            &values,
            errors,
            &BTreeMap::new(),
            Vec::new(),
        );
    }
    let pairs = stored_pairs(SECURITY, &values, &BTreeMap::new());
    save_settings(&state, &tenant, &au, "security", &pairs).await?;
    Ok(done(&ctx, "/admin/settings/security", "admin.common.saved"))
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------

/// `auth.oauth_google` — Google sign-in offered (when configured).
pub const KEY_OAUTH_GOOGLE: &str = "auth.oauth_google";
/// `auth.oauth_github` — GitHub sign-in offered (when configured).
pub const KEY_OAUTH_GITHUB: &str = "auth.oauth_github";

const REGISTRATION: &[Section] = &[
    Section {
        title: "admin_settings.registration.section",
        lead: "",
        defs: &[
            def(
                "auth.allow_registration",
                "admin_settings.registration.allow_registration",
                "admin_settings.registration.allow_registration_hint",
                Kind::Bool,
                "true",
            ),
            def(
                "auth.require_email_verification",
                "admin_settings.registration.require_email_verification",
                "",
                Kind::Bool,
                "true",
            ),
        ],
    },
    Section {
        title: "admin_settings.registration.social",
        lead: "admin_settings.registration.social_lead",
        defs: &[
            def(
                KEY_OAUTH_GOOGLE,
                "admin_settings.registration.oauth_google",
                "",
                Kind::Bool,
                "true",
            ),
            def(
                KEY_OAUTH_GITHUB,
                "admin_settings.registration.oauth_github",
                "",
                Kind::Bool,
                "true",
            ),
        ],
    },
];

async fn registration(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let settings = platform::get_settings(&state.pool, tenant.id, "auth.").await?;
    let values = current_values(REGISTRATION, &settings, &Values::new());
    render_form(
        ctx,
        &staff,
        "admin_settings.tabs.registration",
        "/admin/settings/registration",
        "admin_settings.registration.lead",
        REGISTRATION,
        &values,
        FormErrors::default(),
        &BTreeMap::new(),
        Vec::new(),
    )
}

async fn registration_save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let (values, _) = parse_form(&ctx, REGISTRATION, &f, &BTreeMap::new());
    let pairs = stored_pairs(REGISTRATION, &values, &BTreeMap::new());
    save_settings(&state, &tenant, &au, "registration", &pairs).await?;
    Ok(done(
        &ctx,
        "/admin/settings/registration",
        "admin.common.saved",
    ))
}

/// Whether the OAuth provider `kind` (`google` / `github`) is enabled.
pub async fn oauth_enabled(state: &AppState, tenant_id: TenantId, kind: &str) -> AppResult<bool> {
    let key = match kind {
        "google" => KEY_OAUTH_GOOGLE,
        "github" => KEY_OAUTH_GITHUB,
        _ => return Ok(false),
    };
    Ok(platform::get_setting(&state.pool, tenant_id, key)
        .await?
        .and_then(|v| v.as_bool())
        .unwrap_or(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_merge_into_objects() {
        let mut settings = BTreeMap::new();
        settings.insert(
            "billing.late_fee".to_owned(),
            json!({"type": "percent", "amount": "0", "keep": 1}),
        );
        let mut values = Values::new();
        values.insert("late_fee_type".into(), json!("fixed"));
        values.insert("late_fee_amount".into(), json!("25"));
        values.insert("billing.payment_terms_days".into(), json!(14));
        let pairs = stored_pairs(BILLING, &values, &settings);
        let fee = pairs
            .iter()
            .find(|(k, _)| k == "billing.late_fee")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(fee["type"], "fixed");
        assert_eq!(fee["amount"], "25");
        assert_eq!(fee["keep"], 1);
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "billing.payment_terms_days" && *v == json!(14))
        );
        assert!(!pairs.iter().any(|(k, _)| k == "invoice_prefix"));
    }

    #[test]
    fn links() {
        assert!(valid_link("https://example.com/terms"));
        assert!(valid_link("/sozlesme"));
        assert!(!valid_link("//evil.example"));
        assert!(!valid_link("javascript:alert(1)"));
    }
}
