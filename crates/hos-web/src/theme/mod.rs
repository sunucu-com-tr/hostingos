//! Tenant theming: design template, brand colours, fonts, corner radius and
//! colour mode, edited at `/admin/settings/appearance`.
//!
//! * **Storage.** `tenant_settings` keys `appearance.template`,
//!   `appearance.font_body`, `appearance.font_heading`, `appearance.radius`,
//!   `appearance.mode`, `appearance.mode_toggle` (a missing / empty value
//!   means "the template's default") + the brand colours in the existing
//!   `tenants.branding` JSON (`primary_color`, `accent_color`; also used by
//!   e-mails). See [`Choice`].
//! * **Delivery.** [`attach`] (called by `tenant::resolve_tenant`) puts the
//!   tenant's [`Theme`] (cached per tenant, [`invalidate`]d by the settings
//!   pages) into the request; `PageCtx.theme` carries it to templates. The
//!   theme is a tiny per-tenant stylesheet, `GET /brand.css?v=<hash>`
//!   ([`stylesheet`]): CSS custom properties (brand + server-derived AA
//!   shades, font stacks, radius scale) and the `@font-face` rules of the
//!   selected families only. The hash covers the tenant id and the CSS, so a
//!   versioned URL is cached for a year (`immutable`) and never serves one
//!   tenant's theme to another. `layouts/base.html` links it render-blocking,
//!   preloads the selected font files ([`Theme::preloads`], the exact URLs
//!   of the `@font-face` rules) and sets `data-template` / `data-theme` on
//!   `<html>` for customer surfaces. No inline styles: the CSP is untouched.
//! * **Preview.** Staff with `settings.manage` may add
//!   `?theme_preview=1&tpl=…&brand=…` ([`Choice::preview_query`]) to any
//!   customer page (and to `/brand.css`): the page renders with those unsaved
//!   values, `no-store`, `noindex`, and may be framed by the same origin (the
//!   Appearance page's preview iframe). For anyone else the parameters are
//!   ignored.

pub mod color;
pub mod fonts;
pub mod presets;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use hos_auth::rbac::Permission;
use hos_core::ids::TenantId;
use hos_core::tenant::{Branding, HexColor, TenantContext};
use hos_db::repo::platform;
use http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;

pub use fonts::{FONTS, Font};
pub use presets::{Mode, PRESETS, Preset, Radius};

use crate::assets::Assets;
use crate::session::SessionInfo;
use crate::state::AppState;
use crate::view::PageCtx;
use color::Rgb;

/// Setting key: template.
pub const KEY_TEMPLATE: &str = "appearance.template";
/// Setting key: body font.
pub const KEY_FONT_BODY: &str = "appearance.font_body";
/// Setting key: heading font.
pub const KEY_FONT_HEADING: &str = "appearance.font_heading";
/// Setting key: radius scale.
pub const KEY_RADIUS: &str = "appearance.radius";
/// Setting key: default colour mode.
pub const KEY_MODE: &str = "appearance.mode";
/// Setting key: visitors may toggle light / dark.
pub const KEY_MODE_TOGGLE: &str = "appearance.mode_toggle";

/// Geist / Geist Mono URLs used by tokens.css (`@font-face`) — the preload
/// must request exactly these.
const GEIST_VERSION: &str = "1.9.2";
const GEIST_MONO: &str = "fonts/GeistMono-Variable.woff2";

/// What the tenant chose. `None` = the template's default.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// Design template.
    pub template: &'static Preset,
    /// Brand colour (`#rrggbb` …, validated).
    pub brand: Option<HexColor>,
    /// Secondary / accent colour.
    pub accent: Option<HexColor>,
    /// Body font.
    pub font_body: Option<&'static Font>,
    /// Heading font.
    pub font_heading: Option<&'static Font>,
    /// Corner radius scale.
    pub radius: Option<Radius>,
    /// Default colour mode.
    pub mode: Option<Mode>,
    /// Visitors may switch light / dark.
    pub toggle: bool,
}

impl Default for Choice {
    fn default() -> Self {
        Self {
            template: presets::default_preset(),
            brand: None,
            accent: None,
            font_body: None,
            font_heading: None,
            radius: None,
            mode: None,
            toggle: true,
        }
    }
}

fn str_of(v: Option<&Value>) -> &str {
    v.and_then(Value::as_str).unwrap_or_default()
}

impl Choice {
    /// From the `appearance.*` settings + the branding colours.
    pub fn from_settings(map: &BTreeMap<String, Value>, branding: &Branding) -> Self {
        Self {
            template: presets::by_key(str_of(map.get(KEY_TEMPLATE)))
                .unwrap_or_else(presets::default_preset),
            brand: branding.primary_color.clone(),
            accent: branding.accent_color.clone(),
            font_body: fonts::by_key(str_of(map.get(KEY_FONT_BODY))),
            font_heading: fonts::by_key(str_of(map.get(KEY_FONT_HEADING))),
            radius: Radius::parse(str_of(map.get(KEY_RADIUS))),
            mode: Mode::parse(str_of(map.get(KEY_MODE))),
            toggle: map
                .get(KEY_MODE_TOGGLE)
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }

    /// The `appearance.*` values to store (`""` = template default). The
    /// colours go to `branding` separately.
    pub fn settings(&self) -> Vec<(String, Value)> {
        let s = |v: Option<&str>| Value::String(v.unwrap_or_default().to_owned());
        vec![
            (KEY_TEMPLATE.into(), Value::String(self.template.key.into())),
            (KEY_FONT_BODY.into(), s(self.font_body.map(|f| f.key))),
            (KEY_FONT_HEADING.into(), s(self.font_heading.map(|f| f.key))),
            (KEY_RADIUS.into(), s(self.radius.map(Radius::key))),
            (KEY_MODE.into(), s(self.mode.map(Mode::key))),
            (KEY_MODE_TOGGLE.into(), Value::Bool(self.toggle)),
        ]
    }

    /// Effective body font.
    pub fn body(&self) -> &'static Font {
        self.font_body
            .or_else(|| fonts::by_key(self.template.body_font))
            .unwrap_or_else(fonts::default_font)
    }

    /// Effective heading font.
    pub fn heading(&self) -> &'static Font {
        self.font_heading
            .or_else(|| fonts::by_key(self.template.heading_font))
            .unwrap_or_else(fonts::default_font)
    }

    /// Effective radius.
    pub fn radius(&self) -> Radius {
        self.radius.unwrap_or(self.template.radius)
    }

    /// Effective colour mode.
    pub fn mode(&self) -> Mode {
        self.mode.unwrap_or(self.template.mode)
    }

    /// Effective brand colour.
    pub fn brand_hex(&self) -> &str {
        self.brand
            .as_ref()
            .map_or(self.template.brand, HexColor::as_str)
    }

    /// Effective accent colour.
    pub fn accent_hex(&self) -> &str {
        self.accent
            .as_ref()
            .map_or(self.template.accent, HexColor::as_str)
    }

    /// Unsaved values from a staff preview query (`theme_preview=1&tpl=…`),
    /// on top of `base`; `None` without `theme_preview`. Unknown values fall
    /// back to the template default, so nothing unvalidated reaches the CSS.
    pub fn from_query(query: &str, base: &Choice) -> Option<Self> {
        let pairs: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        if pairs.get("theme_preview").map(String::as_str) != Some("1") {
            return None;
        }
        let get = |k: &str| pairs.get(k).map(String::as_str).unwrap_or_default();
        let color = |k: &str| {
            let v = get(k);
            (!v.is_empty())
                .then(|| HexColor::new(&format!("#{}", v.trim_start_matches('#'))).ok())
                .flatten()
        };
        Some(Self {
            template: presets::by_key(get("tpl")).unwrap_or(base.template),
            brand: color("brand"),
            accent: color("accent"),
            font_body: fonts::by_key(get("fb")),
            font_heading: fonts::by_key(get("fh")),
            radius: Radius::parse(get("r")),
            mode: Mode::parse(get("m")),
            toggle: get("tg") != "0",
        })
    }

    /// The preview query string for these values (see [`Choice::from_query`]).
    pub fn preview_query(&self) -> String {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("theme_preview", "1");
        q.append_pair("tpl", self.template.key);
        let hex = |c: &Option<HexColor>| {
            c.as_ref()
                .map(|c| c.as_str().trim_start_matches('#').to_owned())
                .unwrap_or_default()
        };
        q.append_pair("brand", &hex(&self.brand));
        q.append_pair("accent", &hex(&self.accent));
        q.append_pair("fb", self.font_body.map_or("", |f| f.key));
        q.append_pair("fh", self.font_heading.map_or("", |f| f.key));
        q.append_pair("r", self.radius.map_or("", Radius::key));
        q.append_pair("m", self.mode.map_or("", Mode::key));
        q.append_pair("tg", if self.toggle { "1" } else { "0" });
        q.finish()
    }
}

/// A resolved theme: the choice, its effective values and the stylesheet.
#[derive(Debug, Clone)]
pub struct Theme {
    /// What was chosen (template defaults still `None`).
    pub choice: Choice,
    /// Effective body font.
    pub body: &'static Font,
    /// Effective heading font.
    pub heading: &'static Font,
    /// Effective radius.
    pub radius: Radius,
    /// Effective colour mode.
    pub mode: Mode,
    /// The `/brand.css` body.
    pub css: String,
    /// Content hash (tenant id + CSS), the `?v=` of the stylesheet URL.
    pub version: String,
    /// Preview query when this theme renders unsaved staff values.
    pub preview: Option<String>,
}

impl Theme {
    /// Resolves `choice` and renders its stylesheet.
    pub fn build(assets: &Assets, tenant: Option<TenantId>, choice: Choice) -> Self {
        let css = render_css(assets, &choice);
        let mut seed = tenant.map(|t| t.to_string()).unwrap_or_default();
        seed.push('\n');
        seed.push_str(&css);
        let version = hos_core::crypto::sha256_hex(seed.as_bytes())[..12].to_owned();
        Self {
            body: choice.body(),
            heading: choice.heading(),
            radius: choice.radius(),
            mode: choice.mode(),
            css,
            version,
            preview: None,
            choice,
        }
    }

    /// URL of the theme stylesheet.
    pub fn css_url(&self) -> String {
        match &self.preview {
            Some(q) => format!("/brand.css?{q}"),
            None => format!("/brand.css?v={}", self.version),
        }
    }

    /// The font files to `<link rel=preload>`: body + heading (`latin`, and
    /// `latin-ext` for Turkish pages, which always need ğ ş İ) and Geist Mono.
    pub fn preloads(&self, assets: &Assets, lang: &str) -> Vec<String> {
        let mut out = Vec::new();
        for f in [self.body, self.heading] {
            let mut files = vec![f.latin_file()];
            if lang == "tr"
                && let Some(ext) = f.latin_ext_file()
            {
                files.push(ext);
            }
            for file in files {
                let url = font_url(assets, f, file);
                if !out.contains(&url) {
                    out.push(url);
                }
            }
        }
        out.push(format!("/static/{GEIST_MONO}?v={GEIST_VERSION}"));
        out
    }
}

impl Theme {
    /// Share-image description for a page (`seed`: the page path), drawn in
    /// the template's composition with the effective brand colours.
    pub fn share_image(&self, seed: &str) -> hos_seo::og::Spec {
        let brand = hex_or(self.choice.brand_hex(), self.choice.template.brand);
        let accent = hex_or(self.choice.accent_hex(), self.choice.template.accent);
        let rgb = |c: Rgb| {
            let ch = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            hos_seo::og::Rgb(ch(c.r), ch(c.g), ch(c.b))
        };
        hos_seo::og::Spec {
            brand: rgb(brand),
            brand_2: rgb(color::gradient_partner(brand)),
            accent: rgb(accent),
            style: self.choice.template.share_style,
            seed: seed.to_owned(),
        }
    }

    /// URL of the share image of `path` (versioned by the theme hash).
    pub fn share_image_url(&self, origin: &str, path: &str) -> String {
        format!(
            "{origin}/og/{}.png?v={}",
            share_image_key(path),
            self.version
        )
    }
}

/// The `/og/{key}.png` key of a page path: the path without its outer
/// slashes, every character but `[A-Za-z0-9-]` replaced by `_`, at most 120
/// characters; `home` for `/` (`hos_db::repo::content::share_key_exists`
/// derives the same key in SQL).
pub fn share_image_key(path: &str) -> String {
    let key: String = path
        .trim_matches('/')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if key.is_empty() {
        "home".to_owned()
    } else {
        key
    }
}

fn font_url(assets: &Assets, font: &Font, file: &str) -> String {
    if font.is_builtin() {
        format!("/static/{file}?v={GEIST_VERSION}")
    } else {
        assets.url(file)
    }
}

fn hex_or(raw: &str, fallback: &str) -> Rgb {
    Rgb::parse(raw)
        .or_else(|| Rgb::parse(fallback))
        .unwrap_or(color::BLACK)
}

/// The theme stylesheet (see the module docs). Only validated values are
/// interpolated: hex colours re-serialised from [`Rgb`], font names and
/// numbers from the compiled-in registry.
fn render_css(assets: &Assets, choice: &Choice) -> String {
    let brand = hex_or(choice.brand_hex(), choice.template.brand);
    let accent = hex_or(choice.accent_hex(), choice.template.accent);
    let (body, heading) = (choice.body(), choice.heading());
    let mut css = format!("/* tenant theme · template: {} */\n", choice.template.key);
    let url = |p: &str| assets.url(p);
    css.push_str(&body.face_css(&url));
    if heading.key != body.key {
        css.push_str(&heading.face_css(&url));
    }
    let light = Rgb::parse("#f2f2f2").unwrap_or(color::WHITE);
    let dark = Rgb::parse("#232323").unwrap_or(color::BLACK);
    css.push_str(":root {\n");
    css.push_str(&format!("  --brand: {};\n", brand.hex()));
    css.push_str(&format!("  --accent: {};\n", accent.hex()));
    css.push_str(&format!("  --brand-ink: {};\n", color::ink_on(brand).hex()));
    css.push_str(&format!(
        "  --brand-hover: {};\n",
        color::hover(brand).hex()
    ));
    css.push_str(&format!(
        "  --brand-2: {};\n",
        color::gradient_partner(brand).hex()
    ));
    css.push_str(&format!(
        "  --accent-ink: {};\n",
        color::ink_on(accent).hex()
    ));
    css.push_str(&format!(
        "  --accent-hover: {};\n",
        color::hover(accent).hex()
    ));
    css.push_str(&format!(
        "  --brand-text-light: {};\n  --brand-text-dark: {};\n",
        color::readable_on(brand, light, 4.6).hex(),
        color::readable_on(brand, dark, 4.6).hex()
    ));
    css.push_str(&format!(
        "  --accent-text-light: {};\n  --accent-text-dark: {};\n",
        color::readable_on(accent, light, 4.6).hex(),
        color::readable_on(accent, dark, 4.6).hex()
    ));
    if !body.is_builtin() {
        css.push_str(&format!("  --font-sans: {};\n", body.stack()));
    }
    if heading.key != body.key {
        css.push_str(&format!("  --font-heading: {};\n", heading.stack()));
    }
    css.push_str("}\n");
    let [sm, md, lg, xl] = choice.radius().scale();
    css.push_str(&format!(
        ":root[data-template] {{\n  --radius-sm: {sm}px;\n  --radius: {md}px;\n  --radius-lg: {lg}px;\n  --radius-xl: {xl}px;\n}}\n"
    ));
    // Browsers without relative colour syntax: tokens.css cannot clamp the
    // link colour in OKLCH, so use the server-computed AA shades.
    css.push_str(
        "@supports not (color: oklch(from red l c h)) {\n  \
         :root { --brand-strong: var(--brand-text-light); }\n  \
         :root[data-theme=\"dark\"] { --brand-strong: var(--brand-text-dark); }\n  \
         @media (prefers-color-scheme: dark) {\n    \
         :root:not([data-theme=\"light\"]) { --brand-strong: var(--brand-text-dark); }\n  }\n}\n",
    );
    css
}

// ---------------------------------------------------------------------------
// cache
// ---------------------------------------------------------------------------

const TTL: Duration = Duration::from_secs(60);

struct Entry {
    at: Instant,
    colors: (Option<HexColor>, Option<HexColor>),
    theme: Arc<Theme>,
}

fn cache() -> &'static RwLock<HashMap<TenantId, Entry>> {
    static CACHE: OnceLock<RwLock<HashMap<TenantId, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Drops the cached theme of a tenant (after its settings change).
pub fn invalidate(tenant_id: TenantId) {
    cache()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&tenant_id);
}

/// The default (Classic) theme, used without a tenant.
pub fn default_theme(assets: &Assets) -> Arc<Theme> {
    static DEFAULT: OnceLock<Arc<Theme>> = OnceLock::new();
    DEFAULT
        .get_or_init(|| Arc::new(Theme::build(assets, None, Choice::default())))
        .clone()
}

/// The tenant's saved theme (cached; the brand colours of `tenant` are part
/// of the cache key, so a branding change is picked up at once).
pub async fn for_tenant(state: &AppState, tenant: &TenantContext) -> Arc<Theme> {
    let colors = (
        tenant.branding.primary_color.clone(),
        tenant.branding.accent_color.clone(),
    );
    if let Some(e) = cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&tenant.id)
        && e.at.elapsed() < TTL
        && e.colors == colors
    {
        return e.theme.clone();
    }
    let map = match platform::get_settings(&state.pool, tenant.id, "appearance.").await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "appearance settings unavailable, default theme");
            return default_theme(&state.assets);
        }
    };
    let choice = Choice::from_settings(&map, &tenant.branding);
    let theme = Arc::new(Theme::build(&state.assets, Some(tenant.id), choice));
    let mut w = cache().write().unwrap_or_else(|e| e.into_inner());
    if w.len() > 10_000 {
        w.clear();
    }
    w.insert(
        tenant.id,
        Entry {
            at: Instant::now(),
            colors,
            theme: theme.clone(),
        },
    );
    theme
}

// ---------------------------------------------------------------------------
// request integration
// ---------------------------------------------------------------------------

/// Request extension present when the query asks for a theme preview; set
/// to true once the preview was granted (staff with `settings.manage`).
#[derive(Debug, Clone, Default)]
pub struct PreviewSlot(Arc<AtomicBool>);

/// Called by `tenant::resolve_tenant`: stores the tenant's [`Theme`] in the
/// request (and a [`PreviewSlot`] when a preview is asked for).
pub async fn attach(
    state: &AppState,
    tenant: &TenantContext,
    req: &mut Request,
) -> Option<PreviewSlot> {
    let theme = for_tenant(state, tenant).await;
    req.extensions_mut().insert(theme);
    let wants = req
        .uri()
        .query()
        .is_some_and(|q| q.contains("theme_preview=1"));
    wants.then(|| {
        let slot = PreviewSlot::default();
        req.extensions_mut().insert(slot.clone());
        slot
    })
}

/// Response side of [`attach`]: a granted preview is never cached or
/// indexed and may be framed by the same origin only.
pub fn finish(state: &AppState, slot: Option<PreviewSlot>, mut res: Response) -> Response {
    if !slot.is_some_and(|s| s.0.load(Ordering::Relaxed)) {
        return res;
    }
    let mut csp =
        crate::middleware::CSP.replace("frame-ancestors 'none'", "frame-ancestors 'self'");
    if state.web.secure {
        csp.push_str("; upgrade-insecure-requests");
    }
    let h = res.headers_mut();
    h.remove(ETAG);
    h.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert("content-security-policy", v);
    }
    res
}

/// The theme for a request: the tenant's, or the staff preview of unsaved
/// values when the query asks for one and the session may manage settings.
pub fn for_request(parts: &Parts, assets: &Assets, session: Option<&SessionInfo>) -> Arc<Theme> {
    let base = parts
        .extensions
        .get::<Arc<Theme>>()
        .cloned()
        .unwrap_or_else(|| default_theme(assets));
    let Some(slot) = parts.extensions.get::<PreviewSlot>() else {
        return base;
    };
    let allowed = session.is_some_and(|s| {
        s.is_authenticated()
            && Permission::parse("settings.manage").is_some_and(|p| s.principal.can(p))
    });
    let Some(choice) = allowed
        .then(|| Choice::from_query(parts.uri.query().unwrap_or_default(), &base.choice))
        .flatten()
    else {
        return base;
    };
    slot.0.store(true, Ordering::Relaxed);
    let tenant = parts
        .extensions
        .get::<crate::tenant::Tenant>()
        .map(|t| t.id);
    let mut theme = Theme::build(assets, tenant, choice);
    theme.preview = Some(theme.choice.preview_query());
    Arc::new(theme)
}

/// `GET /brand.css`: the tenant theme stylesheet. `?v=<current hash>` →
/// cached for a year; any other `v` (an older page) or none → 5 minutes;
/// a staff preview → `no-store`.
pub async fn stylesheet(ctx: PageCtx, headers: HeaderMap) -> Response {
    let theme = ctx.theme.clone();
    let css_type = (
        CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    if theme.preview.is_some() {
        return (
            [
                css_type,
                (CACHE_CONTROL, HeaderValue::from_static("private, no-store")),
            ],
            theme.css.clone(),
        )
            .into_response();
    }
    let current = url::form_urlencoded::parse(ctx.query.as_bytes())
        .any(|(k, v)| k == "v" && v == theme.version.as_str());
    let cache = if current {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=300"
    };
    let etag = format!("\"{}\"", theme.version);
    let etag_value = HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\""));
    let fresh = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().trim_start_matches("W/") == etag)
        });
    if fresh {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (ETAG, etag_value),
                (CACHE_CONTROL, HeaderValue::from_static(cache)),
            ],
        )
            .into_response();
    }
    (
        [
            css_type,
            (CACHE_CONTROL, HeaderValue::from_static(cache)),
            (ETAG, etag_value),
        ],
        theme.css.clone(),
    )
        .into_response()
}

/// `@font-face` rules of every family (both subsets), for the font picker on
/// the Appearance page — the only page that loads them all; each file is
/// fetched only when its specimen is rendered.
pub fn specimen_css(assets: &Assets) -> String {
    let mut css = String::from("/* font picker specimens */\n");
    for f in FONTS {
        css.push_str(&f.face_css(&|p| assets.url(p)));
    }
    css
}

// ---------------------------------------------------------------------------
// template helpers
// ---------------------------------------------------------------------------

impl PageCtx {
    /// Storefront, cart, auth and client area (not `/admin`, `/platform`).
    pub fn customer_surface(&self) -> bool {
        self.tenant.is_some()
            && !(self.path == "/admin"
                || self.path.starts_with("/admin/")
                || self.path == "/platform"
                || self.path.starts_with("/platform/"))
    }

    /// Active template key on customer surfaces, `""` elsewhere.
    pub fn tpl(&self) -> &str {
        if self.customer_surface() {
            self.theme.choice.template.key
        } else {
            ""
        }
    }

    /// The theme stylesheet URL.
    pub fn theme_css_url(&self) -> String {
        self.theme.css_url()
    }

    /// The active template's stylesheet (customer surfaces, not Classic).
    pub fn template_css_url(&self) -> Option<String> {
        if !self.customer_surface() {
            return None;
        }
        self.theme.choice.template.css.map(|p| self.asset(p))
    }

    /// Font files to preload.
    pub fn font_preloads(&self) -> Vec<String> {
        self.theme.preloads(&self.state.assets, self.lang)
    }

    /// `light` / `dark` when the tenant fixes a default colour mode for
    /// customer pages, `""` to follow the system.
    pub fn color_mode(&self) -> &'static str {
        if !self.customer_surface() {
            return "";
        }
        match self.theme.mode {
            Mode::System => "",
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }

    /// Whether the light / dark toggle is shown (always in admin).
    pub fn mode_toggle(&self) -> bool {
        !self.customer_surface() || self.theme.choice.toggle
    }

    /// A granted staff preview: theme.js then shows the previewed default
    /// mode instead of the viewer's own stored light / dark preference.
    pub fn theme_preview(&self) -> bool {
        self.customer_surface() && self.theme.preview.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets() -> Assets {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../static");
        Assets::scan(&dir, false)
    }

    #[test]
    fn classic_default_matches_tokens() {
        let t = Theme::build(&assets(), None, Choice::default());
        assert_eq!(t.choice.template.key, "classic");
        assert!(t.css.contains("--brand: #2563eb;"));
        assert!(t.css.contains("--radius: 6px;"));
        // Geist comes from tokens.css: no extra faces, no font override.
        assert!(!t.css.contains("@font-face"));
        assert!(!t.css.contains("--font-sans"));
        let pre = t.preloads(&assets(), "tr");
        assert_eq!(
            pre,
            vec![
                "/static/fonts/Geist-Variable.woff2?v=1.9.2".to_owned(),
                "/static/fonts/GeistMono-Variable.woff2?v=1.9.2".to_owned()
            ]
        );
    }

    #[test]
    fn only_selected_fonts_are_declared() {
        let a = assets();
        let choice = Choice {
            template: presets::by_key("cloud").unwrap(),
            font_body: fonts::by_key("figtree"),
            ..Choice::default()
        };
        let t = Theme::build(&a, None, choice);
        assert!(t.css.contains("font-family: \"Figtree\""));
        assert!(t.css.contains("font-family: \"Plus Jakarta Sans\""));
        assert!(!t.css.contains("\"Inter\""));
        assert!(
            t.css
                .contains("--font-sans: \"Figtree\", \"Figtree Fallback\"")
        );
        let pre = t.preloads(&a, "tr");
        for url in &pre[..pre.len() - 1] {
            assert!(t.css.contains(url.as_str()), "{url} not in @font-face");
        }
        assert_eq!(t.preloads(&a, "en").len(), 3);
    }

    #[test]
    fn version_depends_on_tenant_and_values() {
        let a = assets();
        let one = TenantId::new();
        let two = TenantId::new();
        let x = Theme::build(&a, Some(one), Choice::default());
        let y = Theme::build(&a, Some(two), Choice::default());
        assert_ne!(x.version, y.version);
        let z = Theme::build(
            &a,
            Some(one),
            Choice {
                brand: HexColor::new("#0f5d75").ok(),
                ..Choice::default()
            },
        );
        assert_ne!(x.version, z.version);
        assert!(z.css.contains("--brand: #0f5d75;"));
    }

    #[test]
    fn preview_query_round_trip() {
        let c = Choice {
            template: presets::by_key("terminal").unwrap(),
            brand: HexColor::new("#ff00aa").ok(),
            font_heading: fonts::by_key("fraunces"),
            radius: Some(Radius::Round),
            mode: Some(Mode::Light),
            toggle: false,
            ..Choice::default()
        };
        let q = c.preview_query();
        assert_eq!(Choice::from_query(&q, &Choice::default()), Some(c));
        assert_eq!(Choice::from_query("tpl=cloud", &Choice::default()), None);
        let junk = Choice::from_query(
            "theme_preview=1&tpl=nope&brand=red;}&fb=comic&r=x",
            &Choice::default(),
        )
        .unwrap();
        assert_eq!(junk.template.key, "classic");
        assert!(junk.brand.is_none() && junk.font_body.is_none() && junk.radius.is_none());
    }

    #[test]
    fn pale_brand_gets_dark_ink() {
        let t = Theme::build(
            &assets(),
            None,
            Choice {
                brand: HexColor::new("#fef08a").ok(),
                ..Choice::default()
            },
        );
        assert!(t.css.contains("--brand-ink: #000000;"));
    }
}
