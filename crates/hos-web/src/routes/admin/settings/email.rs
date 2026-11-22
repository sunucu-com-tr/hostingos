//! E-mail settings (`/admin/settings/email`): sender name / address
//! (`branding.email_from_*`, used as `From:` by `Billing::send_logged_email`),
//! reply-to (`mail.reply_to`), the template overrides (`email_templates`,
//! one per built-in key × tr/en: edit, preview with sample data, reset to
//! the default, send a test) and the recent e-mail log.
//!
//! Previews are rendered by the same engine as real mail
//! (`hos_mail::render_email` → variables HTML-escaped → branded layout) and
//! served as a separate document with its own sandboxing CSP, shown in an
//! `<iframe>` of the editor (the template HTML never runs in the admin page).

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use hos_core::domain::Locale;
use hos_core::tenant::Branding;
use hos_db::Page;
use hos_db::enums::EmailCategory;
use hos_db::repo::admin_settings as srepo;
use hos_db::repo::mail::{self as mail_repo, EmailLogRow, EmailTemplateInput};
use hos_db::repo::platform;
use hos_mail::defaults::keys;
use hos_mail::template::{
    EmailTemplateSource, RenderedEmail, Template as MailTemplate, render_email,
};
use hos_mail::{Brand, LayoutOptions};
use http::{HeaderValue, StatusCode, header};
use serde_json::{Value, json};

use super::{invalidate_caches, page_chrome, save_settings, tabs};
use crate::error::{AppError, AppResult};
use crate::extract::{CurrentStaff, RequirePermission, perm};
use crate::forms::{self, FormErrors};
use crate::routes::admin::common::{Audit, FormData, Ui as _, done, failed};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, escape_html, render, render_status};

const BACK: &str = "/admin/settings/email";
/// `mail.reply_to` setting.
pub const KEY_REPLY_TO: &str = "mail.reply_to";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/email", get(show).post(save_sender))
        .route(
            "/admin/settings/email/templates/{key}/{locale}",
            get(edit).post(save_template),
        )
        .route(
            "/admin/settings/email/templates/{key}/{locale}/preview",
            post(preview),
        )
        .route(
            "/admin/settings/email/templates/{key}/{locale}/reset",
            post(reset),
        )
        .route(
            "/admin/settings/email/templates/{key}/{locale}/test",
            post(send_test),
        )
}

fn category(key: &str) -> EmailCategory {
    match key {
        keys::WELCOME => EmailCategory::Client,
        keys::EMAIL_VERIFICATION | keys::PASSWORD_RESET => EmailCategory::Auth,
        keys::INVOICE_CREATED | keys::INVOICE_PAID | keys::INVOICE_OVERDUE => {
            EmailCategory::Invoice
        }
        keys::SERVICE_ACTIVATED | keys::SERVICE_SUSPENDED => EmailCategory::Product,
        keys::TICKET_REPLY => EmailCategory::Support,
        keys::DOMAIN_EXPIRING => EmailCategory::Domain,
        keys::STAFF_INVITE | keys::STAFF_TICKET => EmailCategory::Admin,
        _ => EmailCategory::General,
    }
}

fn parse_key(raw: &str) -> AppResult<&'static str> {
    keys::ALL
        .iter()
        .copied()
        .find(|k| *k == raw)
        .ok_or(AppError::NotFound)
}

fn parse_locale(raw: &str) -> AppResult<Locale> {
    raw.parse().map_err(|_| AppError::NotFound)
}

// ---------------------------------------------------------------------------
// overview + sender
// ---------------------------------------------------------------------------

/// A row of the template table.
#[derive(Debug, Clone)]
pub struct TemplateRowView {
    pub key: &'static str,
    pub name: String,
    /// `(locale, customized)` for tr and en.
    pub locales: Vec<(&'static str, bool)>,
}

/// Sender form.
#[derive(Debug, Clone, Default)]
pub struct SenderForm {
    pub from_name: String,
    pub from_address: String,
    pub reply_to: String,
}

#[derive(Template)]
#[template(path = "admin/settings/email.html")]
struct EmailPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    sender: SenderForm,
    errors: FormErrors,
    templates: Vec<TemplateRowView>,
    log: Vec<EmailLogRow>,
    has_mailer: bool,
}
crate::page!(EmailPage);

async fn sender_form(state: &AppState, tenant: &Tenant) -> AppResult<(SenderForm, Branding)> {
    let row = platform::get_tenant(&state.pool, tenant.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let b = row.branding();
    let reply_to = platform::get_setting(&state.pool, tenant.id, KEY_REPLY_TO)
        .await?
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    Ok((
        SenderForm {
            from_name: b.email_from_name.clone().unwrap_or_default(),
            from_address: b.email_from_address.clone().unwrap_or_default(),
            reply_to,
        },
        b,
    ))
}

async fn email_page(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    mut ctx: PageCtx,
    sender: SenderForm,
    errors: FormErrors,
) -> AppResult<Response> {
    let custom = mail_repo::list_templates(&state.pool, tenant.id).await?;
    let templates = keys::ALL
        .iter()
        .map(|k| TemplateRowView {
            key: k,
            name: ctx.t(&format!("admin_settings.email.templates.{k}")),
            locales: ["tr", "en"]
                .into_iter()
                .map(|l| {
                    (
                        l,
                        custom.iter().any(|t| {
                            t.key == *k && t.locale.as_str() == l && differs_from_default(t)
                        }),
                    )
                })
                .collect(),
        })
        .collect();
    let log = mail_repo::list_email_log(&state.pool, tenant.id, None, Page::first(25))
        .await?
        .items;
    let has_mailer = !hos_db::repo::integrations::active_integrations(
        &state.pool,
        tenant.id,
        hos_core::domain::IntegrationCategory::Mail,
    )
    .await?
    .is_empty();
    page_chrome(&mut ctx, staff, "admin_settings.tabs.email");
    let failed = errors.any();
    let page = EmailPage {
        tabs: tabs(&ctx),
        active: ctx.url(BACK),
        sender,
        errors,
        templates,
        log,
        has_mailer,
        ctx,
    };
    if failed {
        render_status(StatusCode::UNPROCESSABLE_ENTITY, &page)
    } else {
        render(&page)
    }
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let (sender, _) = sender_form(&state, &tenant).await?;
    email_page(&state, &tenant, &staff, ctx, sender, FormErrors::default()).await
}

async fn save_sender(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = SenderForm {
        from_name: f
            .text("from_name", 100)
            .replace(['"', '<', '>', '\r', '\n'], ""),
        from_address: f.text("from_address", 254),
        reply_to: f.text("reply_to", 254),
    };
    let mut errors = FormErrors::default();
    for (k, v) in [
        ("from_address", &form.from_address),
        ("reply_to", &form.reply_to),
    ] {
        if !v.is_empty() && !forms::valid_email(v) {
            errors.add(k, ctx.t("admin.common.invalid_email"));
        }
    }
    if errors.any() {
        return email_page(&state, &tenant, &staff, ctx, form, errors).await;
    }
    let (before, branding) = sender_form(&state, &tenant).await?;
    let mut nb = branding.clone();
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    nb.email_from_name = opt(&form.from_name);
    nb.email_from_address = opt(&form.from_address);
    if nb != branding {
        platform::update_branding(&state.pool, tenant.id, &nb).await?;
        au.log(
            &state,
            "settings.email_sender",
            None,
            json!({
                "from_name": {"from": before.from_name, "to": form.from_name},
                "from_address": {"from": before.from_address, "to": form.from_address},
            }),
        )
        .await?;
    }
    save_settings(
        &state,
        &tenant,
        &au,
        "email",
        &[(KEY_REPLY_TO.to_owned(), json!(form.reply_to))],
    )
    .await?;
    invalidate_caches(&state, &tenant);
    Ok(done(&ctx, BACK, "admin.common.saved"))
}

// ---------------------------------------------------------------------------
// template editor
// ---------------------------------------------------------------------------

/// Editor values.
#[derive(Debug, Clone, Default)]
pub struct TemplateForm {
    pub subject: String,
    pub body_html: String,
    pub body_text: String,
}

#[derive(Template)]
#[template(path = "admin/settings/email_template.html")]
struct TemplatePage {
    ctx: PageCtx,
    key: &'static str,
    locale: &'static str,
    name: String,
    form: TemplateForm,
    customized: bool,
    variables: Vec<(String, String)>,
    errors: FormErrors,
    base: String,
}
crate::page!(TemplatePage);

/// Dotted paths and sample values of the context `key` is rendered with.
pub fn sample_variables(key: &str) -> Vec<(String, String)> {
    fn walk(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
        match v {
            Value::Object(map) => {
                for (k, v) in map {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&p, v, out);
                }
            }
            Value::Array(items) => {
                if let Some(first) = items.first() {
                    walk(&format!("{prefix}.0"), first, out);
                }
            }
            Value::String(s) => out.push((prefix.to_owned(), s.clone())),
            Value::Null => out.push((prefix.to_owned(), String::new())),
            other => out.push((prefix.to_owned(), other.to_string())),
        }
    }
    let mut out = Vec::new();
    walk("", &hos_mail::sample_context(key), &mut out);
    out
}

/// Whether a stored template differs from the built-in default. New stores
/// get a copy of every default, which is not a customization.
fn differs_from_default(row: &hos_db::repo::mail::EmailTemplateRow) -> bool {
    hos_mail::default_template(&row.key, row.locale).is_none_or(|d| {
        d.subject != row.subject || d.body_html != row.body_html || d.body_text != row.body_text
    })
}

async fn current_template(
    state: &AppState,
    tenant: &Tenant,
    key: &str,
    locale: Locale,
) -> AppResult<(TemplateForm, bool)> {
    if let Some(row) = srepo::email_template(&state.pool, tenant.id, key, locale).await? {
        let customized = differs_from_default(&row);
        return Ok((
            TemplateForm {
                subject: row.subject,
                body_html: row.body_html,
                body_text: row.body_text,
            },
            customized,
        ));
    }
    let d = hos_mail::default_template(key, locale).ok_or(AppError::NotFound)?;
    Ok((
        TemplateForm {
            subject: d.subject.to_owned(),
            body_html: d.body_html.to_owned(),
            body_text: d.body_text.to_owned(),
        },
        false,
    ))
}

#[allow(clippy::too_many_arguments)]
fn template_page(
    mut ctx: PageCtx,
    staff: &CurrentStaff,
    key: &'static str,
    locale: Locale,
    form: TemplateForm,
    customized: bool,
    errors: FormErrors,
) -> AppResult<Response> {
    let name = ctx.t(&format!("admin_settings.email.templates.{key}"));
    let title = format!("{name} ({})", locale.as_str().to_uppercase());
    ctx.set_title(title.clone());
    let trail = vec![
        (ctx.t("admin_settings.title"), "/admin/settings".to_owned()),
        (ctx.t("admin_settings.tabs.email"), BACK.to_owned()),
        (title, String::new()),
    ];
    ctx.panel = Some(crate::routes::admin::common::chrome(&ctx, staff, &trail));
    let failed = errors.any();
    let page = TemplatePage {
        base: format!("/admin/settings/email/templates/{key}/{}", locale.as_str()),
        key,
        locale: if locale == Locale::En { "en" } else { "tr" },
        name,
        form,
        customized,
        variables: sample_variables(key),
        errors,
        ctx,
    };
    if failed {
        render_status(StatusCode::UNPROCESSABLE_ENTITY, &page)
    } else {
        render(&page)
    }
}

async fn edit(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    Path((key, locale)): Path<(String, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let key = parse_key(&key)?;
    let locale = parse_locale(&locale)?;
    let (form, customized) = current_template(&state, &tenant, key, locale).await?;
    template_page(
        ctx,
        &staff,
        key,
        locale,
        form,
        customized,
        FormErrors::default(),
    )
}

fn read_form(f: &FormData) -> TemplateForm {
    TemplateForm {
        subject: f.text("subject", 300),
        body_html: f.body("body_html", hos_mail::template::MAX_TEMPLATE_BYTES),
        body_text: f.body("body_text", hos_mail::template::MAX_TEMPLATE_BYTES),
    }
}

/// Checks the three parts parse and render with the sample context.
fn validate(ctx: &PageCtx, key: &str, form: &TemplateForm) -> FormErrors {
    let mut errors = FormErrors::default();
    if form.subject.trim().is_empty() {
        errors.add("subject", ctx.t("admin.common.required"));
    }
    if form.body_html.trim().is_empty() {
        errors.add("body_html", ctx.t("admin.common.required"));
    }
    for (field, src) in [
        ("subject", &form.subject),
        ("body_html", &form.body_html),
        ("body_text", &form.body_text),
    ] {
        if let Err(e) = MailTemplate::parse(src) {
            errors.add(
                field,
                ctx.tv("admin_settings.email.syntax_error", "error", e),
            );
        }
    }
    if !errors.any() {
        let source = EmailTemplateSource {
            subject: &form.subject,
            body_html: &form.body_html,
            body_text: Some(&form.body_text),
        };
        if let Err(e) = render_email(&source, &hos_mail::sample_context(key)) {
            errors.add_general(ctx.tv("admin_settings.email.render_error", "error", e));
        }
    }
    errors
}

async fn save_template(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path((key, locale)): Path<(String, String)>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let key = parse_key(&key)?;
    let locale = parse_locale(&locale)?;
    let form = read_form(&f);
    let errors = validate(&ctx, key, &form);
    let (_, customized) = current_template(&state, &tenant, key, locale).await?;
    if errors.any() {
        return template_page(ctx, &staff, key, locale, form, customized, errors);
    }
    let name = ctx.t(&format!("admin_settings.email.templates.{key}"));
    mail_repo::upsert_template(
        &state.pool,
        tenant.id,
        &EmailTemplateInput {
            key: key.to_owned(),
            locale,
            category: category(key),
            name,
            subject: form.subject.clone(),
            body_html: form.body_html.clone(),
            body_text: form.body_text.clone(),
            is_system: true,
        },
        true,
    )
    .await?;
    au.log(
        &state,
        "settings.email_template",
        None,
        json!({"key": key, "locale": locale.as_str(), "subject": form.subject}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/settings/email/templates/{key}/{}", locale.as_str()),
        "admin.common.saved",
    ))
}

async fn reset(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path((key, locale)): Path<(String, String)>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let key = parse_key(&key)?;
    let locale = parse_locale(&locale)?;
    srepo::delete_email_template(&state.pool, tenant.id, key, locale).await?;
    au.log(
        &state,
        "settings.email_template_reset",
        None,
        json!({"key": key, "locale": locale.as_str()}),
    )
    .await?;
    Ok(done(
        &ctx,
        &format!("/admin/settings/email/templates/{key}/{}", locale.as_str()),
        "admin_settings.email.reset_done",
    ))
}

/// The sample context of `key` with the tenant's real company facts and the
/// acting staff member as recipient (exactly what the engine merges).
async fn preview_context(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    key: &str,
) -> Value {
    let mut ctx = hos_mail::sample_context(key);
    let url = state.billing.config().tenant_url(Some(&tenant.0));
    if let Some(obj) = ctx.as_object_mut() {
        obj.insert(
            "company".into(),
            json!({
                "name": tenant.display_name(),
                "url": url,
                "support_email": tenant.branding.support_email.clone().unwrap_or_default(),
            }),
        );
        obj.insert(
            "user".into(),
            json!({"name": staff.user.display_name(), "email": staff.user.email}),
        );
    }
    ctx
}

/// Renders `form` through the engine and the branded layout.
async fn render_preview(
    state: &AppState,
    tenant: &Tenant,
    staff: &CurrentStaff,
    key: &str,
    locale: Locale,
    form: &TemplateForm,
) -> Result<RenderedEmail, String> {
    let source = EmailTemplateSource {
        subject: &form.subject,
        body_html: &form.body_html,
        body_text: Some(&form.body_text),
    };
    let ctx = preview_context(state, tenant, staff, key).await;
    let rendered = render_email(&source, &ctx).map_err(|e| e.to_string())?;
    let brand = Brand::from_tenant(&tenant.0);
    Ok(hos_mail::layout::apply(
        &brand,
        rendered,
        &LayoutOptions {
            preheader: None,
            locale,
        },
    ))
}

/// Sandbox for the preview document: inline styles of the e-mail allowed,
/// no scripts, no forms, only framed by the admin itself.
const PREVIEW_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src https: data:; \
     font-src https: data:; frame-ancestors 'self'; base-uri 'none'; form-action 'none'; sandbox";

/// `POST …/preview` — the rendered e-mail as a standalone document (opened in
/// the editor's preview `<iframe>` through `formtarget`).
async fn preview(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    Path((key, locale)): Path<(String, String)>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let key = parse_key(&key)?;
    let locale = parse_locale(&locale)?;
    let form = read_form(&f);
    let doc = match render_preview(&state, &tenant, &staff, key, locale, &form).await {
        Ok(r) => format!(
            "<!doctype html><html lang=\"{lang}\"><head><meta charset=\"utf-8\"><title>{subject}</title></head>\
             <body style=\"margin:0;font-family:system-ui,sans-serif\">\
             <div style=\"padding:10px 14px;border-bottom:1px solid #ddd;background:#f8fafc;font-size:13px\">\
             <strong>{subject_label}:</strong> <span data-preview-subject>{subject}</span></div>\
             {html}\
             <details style=\"margin:12px 14px;font-size:13px\"><summary>{text_label}</summary>\
             <pre style=\"white-space:pre-wrap\">{text}</pre></details></body></html>",
            lang = locale.as_str(),
            subject = escape_html(&r.subject),
            subject_label = escape_html(&ctx.t("admin_settings.email.subject")),
            text_label = escape_html(&ctx.t("admin_settings.email.body_text")),
            html = r.html,
            text = escape_html(&r.text),
        ),
        Err(e) => format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"></head><body style=\"font-family:system-ui,sans-serif;padding:14px\">\
             <p style=\"color:#b91c1c\">{}</p></body></html>",
            escape_html(&ctx.tv("admin_settings.email.render_error", "error", e))
        ),
    };
    let mut res = Html(doc).into_response();
    let h = res.headers_mut();
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(PREVIEW_CSP),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

/// `POST …/test` — sends the saved (or default) template with sample data
/// to the acting staff member (or the given address).
async fn send_test(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path((key, locale)): Path<(String, String)>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let key = parse_key(&key)?;
    let locale = parse_locale(&locale)?;
    let back = format!("/admin/settings/email/templates/{key}/{}", locale.as_str());
    let to = f.opt("to", 254).unwrap_or_else(|| staff.user.email.clone());
    if !forms::valid_email(&to) {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_email")));
    }
    let (form, _) = current_template(&state, &tenant, key, locale).await?;
    let source = EmailTemplateSource {
        subject: &form.subject,
        body_html: &form.body_html,
        body_text: Some(&form.body_text),
    };
    let sample = preview_context(&state, &tenant, &staff, key).await;
    let mut rendered = match render_email(&source, &sample) {
        Ok(r) => r,
        Err(e) => {
            return Ok(failed(
                &ctx,
                &back,
                ctx.tv("admin_settings.email.render_error", "error", e),
            ));
        }
    };
    rendered.subject = format!("[TEST] {}", rendered.subject);
    crate::routes::admin::mailer::queue_rendered(
        &state,
        &tenant,
        &to,
        None,
        locale,
        None,
        Some(key.to_owned()),
        rendered,
    )
    .await?;
    au.log(
        &state,
        "settings.email_test",
        None,
        json!({"key": key, "locale": locale.as_str(), "to": to}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin_settings.email.test_queued"))
}

impl TemplatePage {
    fn locale_label(&self) -> String {
        self.t(&format!("admin.common.lang_{}", self.locale))
    }
}

impl EmailPage {
    fn template_name(&self, key: &Option<String>) -> String {
        match key {
            Some(k) => {
                let tk = format!("admin_settings.email.templates.{k}");
                if self.cat.has(&tk) {
                    self.t(&tk)
                } else {
                    k.clone()
                }
            }
            None => "—".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_sample_variables() {
        for k in keys::ALL {
            let vars = sample_variables(k);
            assert!(vars.iter().any(|(p, _)| p == "company.name"), "{k}");
            assert!(vars.iter().any(|(p, _)| p == "user.email"), "{k}");
        }
        assert!(
            sample_variables(keys::STAFF_INVITE)
                .iter()
                .any(|(p, _)| p == "action_url")
        );
    }

    #[test]
    fn a_copy_of_the_default_is_not_a_customization() {
        let d = hos_mail::default_template(keys::WELCOME, Locale::Tr).expect("default");
        let mut row = hos_db::repo::mail::EmailTemplateRow {
            id: hos_core::ids::EmailTemplateId::new(),
            tenant_id: hos_core::ids::TenantId::new(),
            key: d.key.into(),
            locale: Locale::Tr,
            category: hos_db::enums::EmailCategory::Client,
            name: "Welcome".into(),
            subject: d.subject.into(),
            body_html: d.body_html.into(),
            body_text: d.body_text.into(),
            is_active: true,
            is_system: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert!(!differs_from_default(&row));
        row.subject.push_str(" (edited)");
        assert!(differs_from_default(&row));
        row.key = "not_a_builtin".into();
        assert!(differs_from_default(&row), "no default to compare with");
    }
}
