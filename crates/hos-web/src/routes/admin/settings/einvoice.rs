//! `/admin/settings/einvoice` (`settings.manage`): the `einvoice.*` and
//! `invoice_pdf.*` keys read by `hos_billing::einvoice::EInvoiceSettings`
//! (switch, integrator, issue timing, series, default VAT exemption, test
//! mode, start date, PDF e-mail attachment and bank details).
//!
//! Uses the generic key/value form of [`super`]; extra checks: series are
//! `[A-Z0-9]{3}` and differ, the exemption code is a supported GİB code
//! with a reason, the start date parses. The integrator select lists the
//! tenant's `einvoice` integrations only (connection details and secrets
//! live on the integration, encrypted). Notes warn when no integration
//! exists or the company tax data is incomplete.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use chrono::NaiveDate;
use hos_billing::einvoice::codes::{ExemptionClass, Series, exemption_class};
use hos_billing::einvoice::{keys, taxid};
use hos_core::domain::IntegrationCategory;
use hos_db::repo::{integrations, platform};
use serde_json::Value;

use super::{
    Kind, Section, Values, current_values, def, parse_form, render_form, save_settings,
    stored_pairs,
};
use crate::error::AppResult;
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::forms::FormErrors;
use crate::routes::admin::common::{Audit, FormData, done, today};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::PageCtx;

const PATH: &str = "/admin/settings/einvoice";
const TAB: &str = "einvoice.settings.tab";
const LEAD: &str = "einvoice.settings.lead";

pub(super) fn routes() -> Router<AppState> {
    Router::new().route(PATH, get(show).post(save))
}

const SECTIONS: &[Section] = &[
    Section {
        title: "einvoice.settings.section_general",
        lead: "einvoice.settings.section_general_lead",
        defs: &[
            def(
                keys::ENABLED,
                "einvoice.settings.enabled",
                "einvoice.settings.enabled_hint",
                Kind::Bool,
                "false",
            ),
            def(
                keys::INTEGRATION_ID,
                "einvoice.settings.integration",
                "einvoice.settings.integration_hint",
                Kind::Select,
                "\"\"",
            ),
            def(
                keys::ISSUE_ON,
                "einvoice.settings.issue_on",
                "einvoice.settings.issue_on_hint",
                Kind::Select,
                "\"payment\"",
            ),
            def(
                keys::START_DATE,
                "einvoice.settings.start_date",
                "einvoice.settings.start_date_hint",
                Kind::Text(10),
                "\"\"",
            ),
            def(
                keys::TEST_MODE,
                "einvoice.settings.test_mode",
                "einvoice.settings.test_mode_hint",
                Kind::Bool,
                "false",
            ),
        ],
    },
    Section {
        title: "einvoice.settings.section_numbering",
        lead: "einvoice.settings.section_numbering_lead",
        defs: &[
            def(
                keys::SERIES_EARSIV,
                "einvoice.settings.series_earsiv",
                "einvoice.settings.series_hint",
                Kind::Text(3),
                "\"\"",
            ),
            def(
                keys::SERIES_EFATURA,
                "einvoice.settings.series_efatura",
                "einvoice.settings.series_hint",
                Kind::Text(3),
                "\"\"",
            ),
        ],
    },
    Section {
        title: "einvoice.settings.section_exemption",
        lead: "einvoice.settings.section_exemption_lead",
        defs: &[
            def(
                keys::EXEMPTION_CODE,
                "einvoice.settings.exemption_code",
                "einvoice.settings.exemption_code_hint",
                Kind::Text(3),
                "\"\"",
            ),
            def(
                keys::EXEMPTION_REASON,
                "einvoice.settings.exemption_reason",
                "",
                Kind::Text(250),
                "\"\"",
            ),
        ],
    },
    Section {
        title: "einvoice.settings.section_pdf",
        lead: "einvoice.settings.section_pdf_lead",
        defs: &[
            def(
                keys::ATTACH_PDF,
                "einvoice.settings.attach_pdf",
                "",
                Kind::Bool,
                "false",
            ),
            def(
                keys::BANK_DETAILS,
                "einvoice.settings.bank_details",
                "einvoice.settings.bank_details_hint",
                Kind::Text(500),
                "\"\"",
            ),
        ],
    },
];

type Options = BTreeMap<&'static str, Vec<(String, String)>>;

/// Select options and the notes (no integration, incomplete company data).
async fn options_and_notes(
    state: &AppState,
    tenant: &Tenant,
    ctx: &PageCtx,
) -> AppResult<(Options, Vec<String>)> {
    let rows = integrations::list_integrations(
        &state.pool,
        tenant.id,
        Some(IntegrationCategory::Einvoice),
    )
    .await?;
    let mut notes = Vec::new();
    if rows.is_empty() {
        notes.push(ctx.t("einvoice.settings.no_integration"));
    }
    let mut providers = vec![(
        String::new(),
        ctx.t("einvoice.settings.integration_default"),
    )];
    providers.extend(
        rows.iter()
            .map(|r| (r.id.to_string(), format!("{} ({})", r.name, r.kind))),
    );
    let mut options = Options::new();
    options.insert(keys::INTEGRATION_ID, providers);
    options.insert(
        keys::ISSUE_ON,
        vec![
            (
                "payment".into(),
                ctx.t("einvoice.settings.issue_on_payment"),
            ),
            (
                "creation".into(),
                ctx.t("einvoice.settings.issue_on_creation"),
            ),
        ],
    );
    if let Some(t) = platform::get_tenant(&state.pool, tenant.id).await? {
        let number = taxid::normalize(t.tax_number.as_deref().unwrap_or("")).unwrap_or_default();
        let city = t
            .address
            .get("city")
            .and_then(Value::as_str)
            .is_some_and(|c| !c.trim().is_empty());
        let office = t
            .tax_office
            .as_deref()
            .is_some_and(|o| !o.trim().is_empty());
        if !taxid::is_valid_tax_number(&number) || !office || !city {
            notes.push(ctx.t("einvoice.settings.company_incomplete"));
        }
    }
    Ok((options, notes))
}

/// Extra checks on top of the generic ones; normalises the series.
fn validate(ctx: &PageCtx, values: &mut Values, errors: &mut FormErrors) {
    let text = |v: &Values, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_owned()
    };
    let enabled = values
        .get(keys::ENABLED)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut series = Vec::new();
    for key in [keys::SERIES_EARSIV, keys::SERIES_EFATURA] {
        let raw = text(values, key);
        if raw.is_empty() {
            if enabled {
                errors.add(key, ctx.t("einvoice.settings.series_required"));
            }
            continue;
        }
        match Series::parse(&raw) {
            Some(s) => {
                values.insert(key.to_owned(), Value::String(s.as_str().to_owned()));
                series.push(s);
            }
            None => errors.add(key, ctx.t("einvoice.settings.invalid_series")),
        }
    }
    if series.len() == 2 && series[0] == series[1] {
        errors.add(keys::SERIES_EFATURA, ctx.t("einvoice.settings.same_series"));
    }
    let code = text(values, keys::EXEMPTION_CODE);
    if !code.is_empty() {
        match exemption_class(&code) {
            ExemptionClass::Unknown => {
                errors.add(
                    keys::EXEMPTION_CODE,
                    ctx.t("einvoice.settings.invalid_exemption"),
                );
            }
            ExemptionClass::OzelMatrah | ExemptionClass::IhracKayitli => {
                errors.add(
                    keys::EXEMPTION_CODE,
                    ctx.t("einvoice.settings.unsupported_exemption"),
                );
            }
            ExemptionClass::Istisna | ExemptionClass::Satis => {
                if text(values, keys::EXEMPTION_REASON).is_empty() {
                    errors.add(
                        keys::EXEMPTION_REASON,
                        ctx.t("einvoice.settings.reason_required"),
                    );
                }
            }
        }
    }
    let date = text(values, keys::START_DATE);
    if !date.is_empty() && NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
        errors.add(keys::START_DATE, ctx.t("einvoice.settings.invalid_date"));
    }
}

fn page(
    ctx: PageCtx,
    staff: &CurrentStaff,
    values: &Values,
    errors: FormErrors,
    options: &Options,
    notes: Vec<String>,
) -> AppResult<Response> {
    render_form(
        ctx, staff, TAB, PATH, LEAD, SECTIONS, values, errors, options, notes,
    )
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let settings = platform::get_settings(&state.pool, tenant.id, "").await?;
    let values = current_values(SECTIONS, &settings, &Values::new());
    let (options, notes) = options_and_notes(&state, &tenant, &ctx).await?;
    page(ctx, &staff, &values, FormErrors::default(), &options, notes)
}

async fn save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let settings = platform::get_settings(&state.pool, tenant.id, "").await?;
    let (options, notes) = options_and_notes(&state, &tenant, &ctx).await?;
    let (mut values, mut errors) = parse_form(&ctx, SECTIONS, &f, &options);
    validate(&ctx, &mut values, &mut errors);
    if errors.any() {
        return page(ctx, &staff, &values, errors, &options, notes);
    }
    // Switching on without a start date: from today (never the backlog).
    let enabled = values
        .get(keys::ENABLED)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let no_date = values
        .get(keys::START_DATE)
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty());
    if enabled && no_date {
        values.insert(
            keys::START_DATE.to_owned(),
            Value::String(today(&ctx.timezone).format("%Y-%m-%d").to_string()),
        );
    }
    let pairs = stored_pairs(SECTIONS, &values, &settings);
    save_settings(&state, &tenant, &au, "einvoice", &pairs).await?;
    Ok(done(&ctx, PATH, "einvoice.settings.saved"))
}
