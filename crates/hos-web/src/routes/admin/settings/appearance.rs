//! Appearance settings (`/admin/settings/appearance`, `settings.manage`):
//! the customer-side design template, brand colours, body / heading fonts,
//! corner radius, default colour mode (+ whether visitors may toggle) and a
//! live preview. Storage and delivery are described in [`crate::theme`];
//! logo and favicon stay on the General tab (linked from here).
//!
//! | Route | |
//! |---|---|
//! | `GET  /admin/settings/appearance` | the form + preview iframe |
//! | `POST /admin/settings/appearance` | validate, save (`settings.appearance` / `settings.branding` audit) |
//! | `POST /admin/settings/appearance/reset` | back to the template's defaults (template kept) |
//! | `POST /admin/settings/appearance/preview` | no-JS preview: 303 to the storefront with the preview query |
//! | `GET  /admin/settings/appearance/fonts.css` | `@font-face` + specimen classes of the whole library (this page only) |

use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use hos_core::tenant::HexColor;
use hos_db::repo::platform;
use http::{HeaderValue, StatusCode, header};
use serde_json::json;

use super::{invalidate_caches, page_chrome, save_settings, tabs};
use crate::error::AppResult;
use crate::extract::{RequirePermission, perm};
use crate::forms::FormErrors;
use crate::routes::admin::common::{Audit, FormData, done};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::theme::{self, Choice, FONTS, Mode, PRESETS, Radius, fonts, presets};
use crate::view::{NavItem, PageCtx, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/appearance", get(show).post(save))
        .route("/admin/settings/appearance/reset", post(reset))
        .route("/admin/settings/appearance/preview", post(preview))
        .route("/admin/settings/appearance/fonts.css", get(specimens))
}

/// A template card in the picker.
pub struct TemplateCard {
    /// Key.
    pub key: &'static str,
    /// Translated name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Thumbnail URL.
    pub thumbnail: String,
    /// Selected.
    pub selected: bool,
    /// Default body font family.
    pub body: &'static str,
    /// Default heading font family.
    pub heading: &'static str,
    /// Default heading font key (specimen class).
    pub heading_key: &'static str,
    /// Default body font key (specimen class).
    pub body_key: &'static str,
    /// Translated default radius.
    pub radius: String,
    /// Translated default mode.
    pub mode: String,
    /// Default brand colour.
    pub brand: &'static str,
    /// Default accent colour.
    pub accent: &'static str,
}

/// One option of a font list (`key` = `""`: template default).
pub struct FontOption {
    /// Stored key.
    pub key: &'static str,
    /// Family (or "Template default (…)").
    pub label: String,
    /// Specimen class suffix (the family shown).
    pub specimen: &'static str,
    /// "Sans" / "Serif".
    pub category: String,
    /// Selected.
    pub selected: bool,
}

/// A radio option.
pub struct Opt {
    /// Value.
    pub value: &'static str,
    /// Label.
    pub label: String,
    /// Selected.
    pub selected: bool,
}

/// Submitted values (strings, so a rejected form re-renders as typed).
#[derive(Debug, Clone, Default)]
pub struct AppearanceForm {
    /// Template key.
    pub template: String,
    /// Brand colour (`""` = template default).
    pub brand_color: String,
    /// Accent colour.
    pub accent_color: String,
    /// Body font key.
    pub font_body: String,
    /// Heading font key.
    pub font_heading: String,
    /// Radius key.
    pub radius: String,
    /// Mode key.
    pub mode: String,
    /// Visitors may toggle.
    pub mode_toggle: bool,
}

impl AppearanceForm {
    fn of(c: &Choice) -> Self {
        Self {
            template: c.template.key.into(),
            brand_color: c.brand.as_ref().map(|h| h.to_string()).unwrap_or_default(),
            accent_color: c.accent.as_ref().map(|h| h.to_string()).unwrap_or_default(),
            font_body: c.font_body.map(|f| f.key.to_owned()).unwrap_or_default(),
            font_heading: c.font_heading.map(|f| f.key.to_owned()).unwrap_or_default(),
            radius: c.radius.map(|r| r.key().to_owned()).unwrap_or_default(),
            mode: c.mode.map(|m| m.key().to_owned()).unwrap_or_default(),
            mode_toggle: c.toggle,
        }
    }

    fn read(f: &FormData) -> Self {
        Self {
            template: f.text("template", 32),
            brand_color: f.text("brand_color", 16),
            accent_color: f.text("accent_color", 16),
            font_body: f.text("font_body", 40),
            font_heading: f.text("font_heading", 40),
            radius: f.text("radius", 16),
            mode: f.text("mode", 16),
            mode_toggle: f.flag("mode_toggle"),
        }
    }

    /// Validates into a [`Choice`]; every rejected field gets an error.
    fn validate(&self, ctx: &PageCtx, errors: &mut FormErrors) -> Option<Choice> {
        let t = |k: &str| ctx.t(&format!("admin_settings.appearance.errors.{k}"));
        let template = presets::by_key(&self.template);
        if template.is_none() {
            errors.add("template", t("template"));
        }
        let color = |field: &str, raw: &str, errors: &mut FormErrors| {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            let hex = if raw.starts_with('#') {
                raw.to_owned()
            } else {
                format!("#{raw}")
            };
            match HexColor::new(&hex) {
                Ok(c) if matches!(hex.len(), 4 | 7) => Some(c),
                _ => {
                    errors.add(field, ctx.t("admin_settings.errors.color"));
                    None
                }
            }
        };
        let brand = color("brand_color", &self.brand_color, errors);
        let accent = color("accent_color", &self.accent_color, errors);
        let font = |field: &str, raw: &str, errors: &mut FormErrors| {
            if raw.is_empty() {
                return None;
            }
            let f = fonts::by_key(raw);
            if f.is_none() {
                errors.add(field, t("font"));
            }
            f
        };
        let font_body = font("font_body", &self.font_body, errors);
        let font_heading = font("font_heading", &self.font_heading, errors);
        // `""` = template default; anything else must be a known key.
        let radius = Radius::parse(&self.radius);
        if radius.is_none() && !self.radius.is_empty() {
            errors.add("radius", t("choice"));
        }
        let mode = Mode::parse(&self.mode);
        if mode.is_none() && !self.mode.is_empty() {
            errors.add("mode", t("choice"));
        }
        if errors.any() {
            return None;
        }
        Some(Choice {
            template: template?,
            brand,
            accent,
            font_body,
            font_heading,
            radius,
            mode,
            toggle: self.mode_toggle,
        })
    }
}

#[derive(Template)]
#[template(path = "admin/settings/appearance.html")]
struct AppearancePage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    errors: FormErrors,
    form: AppearanceForm,
    templates: Vec<TemplateCard>,
    body_fonts: Vec<FontOption>,
    heading_fonts: Vec<FontOption>,
    radii: Vec<Opt>,
    modes: Vec<Opt>,
    /// Effective colours of the current template (picker / placeholder).
    brand_default: &'static str,
    accent_default: &'static str,
    brand_value: String,
    accent_value: String,
    preview_src: String,
    preview_pages: Vec<Opt>,
    specimen_css: String,
    images_url: String,
}
crate::page!(AppearancePage);

fn radius_label(ctx: &PageCtx, r: Radius) -> String {
    ctx.t(&format!("admin_settings.appearance.radius_{}", r.key()))
}

fn mode_label(ctx: &PageCtx, m: Mode) -> String {
    ctx.t(&format!("admin_settings.appearance.mode_{}", m.key()))
}

fn category(ctx: &PageCtx, f: &fonts::Font) -> String {
    ctx.t(match f.category {
        fonts::Category::Sans => "admin_settings.appearance.font_sans",
        fonts::Category::Serif => "admin_settings.appearance.font_serif",
    })
}

fn font_options(ctx: &PageCtx, selected: &str, default: &'static fonts::Font) -> Vec<FontOption> {
    let mut out = vec![FontOption {
        key: "",
        label: ctx.tv(
            "admin_settings.appearance.template_default_value",
            "value",
            default.family,
        ),
        specimen: default.key,
        category: category(ctx, default),
        selected: selected.is_empty(),
    }];
    out.extend(FONTS.iter().map(|f| FontOption {
        key: f.key,
        label: f.family.to_owned(),
        specimen: f.key,
        category: category(ctx, f),
        selected: selected == f.key,
    }));
    out
}

fn page(
    mut ctx: PageCtx,
    staff: &crate::extract::CurrentStaff,
    form: AppearanceForm,
    errors: FormErrors,
) -> AppResult<Response> {
    page_chrome(&mut ctx, staff, "admin_settings.tabs.appearance");
    let preset = presets::by_key(&form.template).unwrap_or_else(presets::default_preset);
    let font_name = |k: &str| fonts::by_key(k).map_or("", |f| f.family);
    let templates = PRESETS
        .iter()
        .map(|p| TemplateCard {
            key: p.key,
            name: ctx.t(&format!(
                "admin_settings.appearance.templates.{}.name",
                p.key
            )),
            description: ctx.t(&format!(
                "admin_settings.appearance.templates.{}.description",
                p.key
            )),
            thumbnail: ctx.asset(p.thumbnail),
            selected: p.key == preset.key,
            body: font_name(p.body_font),
            heading: font_name(p.heading_font),
            body_key: p.body_font,
            heading_key: p.heading_font,
            radius: radius_label(&ctx, p.radius),
            mode: mode_label(&ctx, p.mode),
            brand: p.brand,
            accent: p.accent,
        })
        .collect();
    let default_font = |k: &str| fonts::by_key(k).unwrap_or_else(fonts::default_font);
    let body_fonts = font_options(&ctx, &form.font_body, default_font(preset.body_font));
    let heading_fonts = font_options(&ctx, &form.font_heading, default_font(preset.heading_font));
    let mut radii = vec![Opt {
        value: "",
        label: ctx.tv(
            "admin_settings.appearance.template_default_value",
            "value",
            radius_label(&ctx, preset.radius),
        ),
        selected: form.radius.is_empty(),
    }];
    radii.extend(Radius::ALL.iter().map(|r| Opt {
        value: r.key(),
        label: radius_label(&ctx, *r),
        selected: form.radius == r.key(),
    }));
    let mut modes = vec![Opt {
        value: "",
        label: ctx.tv(
            "admin_settings.appearance.template_default_value",
            "value",
            mode_label(&ctx, preset.mode),
        ),
        selected: form.mode.is_empty(),
    }];
    modes.extend(Mode::ALL.iter().map(|m| Opt {
        value: m.key(),
        label: mode_label(&ctx, *m),
        selected: form.mode == m.key(),
    }));
    // The preview shows the values as they stand in the form.
    let mut scratch = FormErrors::default();
    let preview_choice = form.validate(&ctx, &mut scratch).unwrap_or_default();
    let preview_src = format!("{}?{}", ctx.url("/"), preview_choice.preview_query());
    let preview_pages = [
        ("/", "admin_settings.appearance.page_home"),
        ("/domains", "admin_settings.appearance.page_domains"),
        ("/cart", "admin_settings.appearance.page_cart"),
        ("/kb", "admin_settings.appearance.page_kb"),
    ]
    .iter()
    .map(|(path, key)| Opt {
        value: path,
        label: ctx.t(key),
        selected: *path == "/",
    })
    .collect();
    let valid_hex = |s: &str| {
        let s = s.trim();
        (s.len() == 7 && HexColor::new(s).is_ok()).then(|| s.to_ascii_lowercase())
    };
    let brand_value = valid_hex(&form.brand_color).unwrap_or_else(|| preset.brand.into());
    let accent_value = valid_hex(&form.accent_color).unwrap_or_else(|| preset.accent.into());
    let tabs = tabs(&ctx);
    let active = ctx.url("/admin/settings/appearance");
    let specimen_css = ctx.url("/admin/settings/appearance/fonts.css");
    let images_url = format!("{}#images", ctx.url("/admin/settings/general"));
    let status = if errors.any() {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::OK
    };
    let p = AppearancePage {
        tabs,
        active,
        errors,
        form,
        templates,
        body_fonts,
        heading_fonts,
        radii,
        modes,
        brand_default: preset.brand,
        accent_default: preset.accent,
        brand_value,
        accent_value,
        preview_src,
        preview_pages,
        specimen_css,
        images_url,
        ctx,
    };
    if status == StatusCode::OK {
        render(&p)
    } else {
        render_status(status, &p)
    }
}

async fn current(state: &AppState, tenant: &Tenant) -> AppResult<Choice> {
    let map = platform::get_settings(&state.pool, tenant.id, "appearance.").await?;
    let branding = platform::get_tenant(&state.pool, tenant.id)
        .await?
        .map(|t| t.branding())
        .unwrap_or_default();
    Ok(Choice::from_settings(&map, &branding))
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let choice = current(&state, &tenant).await?;
    page(
        ctx,
        &staff,
        AppearanceForm::of(&choice),
        FormErrors::default(),
    )
}

/// Stores `choice` (settings diff + branding colours, both audited).
async fn store(state: &AppState, tenant: &Tenant, au: &Audit, choice: &Choice) -> AppResult<()> {
    save_settings(state, tenant, au, "appearance", &choice.settings()).await?;
    let Some(row) = platform::get_tenant(&state.pool, tenant.id).await? else {
        return Ok(());
    };
    let branding = row.branding();
    let mut nb = branding.clone();
    nb.primary_color = choice.brand.clone();
    nb.accent_color = choice.accent.clone();
    if nb != branding {
        platform::update_branding(&state.pool, tenant.id, &nb).await?;
        au.log(
            state,
            "settings.branding",
            None,
            json!({
                "primary_color": {"from": branding.primary_color, "to": nb.primary_color},
                "accent_color": {"from": branding.accent_color, "to": nb.accent_color},
            }),
        )
        .await?;
        invalidate_caches(state, tenant);
    }
    Ok(())
}

async fn save(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let form = AppearanceForm::read(&f);
    let mut errors = FormErrors::default();
    let Some(choice) = form.validate(&ctx, &mut errors) else {
        return page(ctx, &staff, form, errors);
    };
    store(&state, &tenant, &au, &choice).await?;
    Ok(done(
        &ctx,
        "/admin/settings/appearance",
        "admin_settings.appearance.saved",
    ))
}

async fn reset(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
) -> AppResult<Response> {
    let current = current(&state, &tenant).await?;
    let defaults = Choice {
        template: current.template,
        ..Choice::default()
    };
    store(&state, &tenant, &au, &defaults).await?;
    Ok(done(
        &ctx,
        "/admin/settings/appearance",
        "admin_settings.appearance.reset_done",
    ))
}

/// No-JS preview (the form's "Preview" button targets the iframe): the
/// submitted values as a preview query on the chosen storefront page.
/// Invalid values fall back to the template defaults (nothing is saved).
async fn preview(
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    ctx: PageCtx,
    f: FormData,
) -> Response {
    let form = AppearanceForm::read(&f);
    let mut errors = FormErrors::default();
    let choice = form.validate(&ctx, &mut errors).unwrap_or_else(|| Choice {
        template: presets::by_key(&form.template).unwrap_or_else(presets::default_preset),
        ..Choice::default()
    });
    let page = match f.text("preview_page", 16).as_str() {
        p @ ("/domains" | "/cart" | "/kb") => p.to_owned(),
        _ => "/".to_owned(),
    };
    Redirect::to(&format!("{}?{}", ctx.url(&page), choice.preview_query())).into_response()
}

async fn specimens(
    State(state): State<AppState>,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
) -> Response {
    let mut css = theme::specimen_css(&state.assets);
    for f in FONTS {
        css.push_str(&format!(
            ".font-specimen--{} {{ font-family: {}; }}\n",
            f.key,
            f.stack()
        ));
    }
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/css; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600"),
            ),
        ],
        css,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_values_round_trip() {
        let c = Choice {
            template: presets::by_key("corporate").unwrap(),
            brand: HexColor::new("#123456").ok(),
            font_heading: fonts::by_key("fraunces"),
            radius: Some(Radius::Round),
            toggle: false,
            ..Choice::default()
        };
        let map = c.settings().into_iter().collect();
        let branding = hos_core::tenant::Branding {
            primary_color: c.brand.clone(),
            ..Default::default()
        };
        assert_eq!(Choice::from_settings(&map, &branding), c);
        let f = AppearanceForm::of(&c);
        assert_eq!(f.brand_color, "#123456");
        assert_eq!(f.font_body, "");
        assert_eq!(f.font_heading, "fraunces");
    }
}
