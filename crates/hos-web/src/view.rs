//! Template context and rendering helpers.
//!
//! Every page template struct has a `ctx: PageCtx` field and derefs to it
//! (`crate::page!(MyPage);`). Because Askama resolves unknown names to
//! `self.<name>`, templates can call the helpers directly:
//!
//! ```text
//! {{ t("storefront.kb.title") }}            translation
//! {{ tv("client.dashboard.hello", "name", n) }} with a {name} placeholder
//! {{ tn("storefront.kb.articles", n) }}      plural ({count})
//! {{ url("/kb") }}                          localized path (/en/kb)
//! {{ asset("css/app.css") }}                cache-busted static URL
//! {{ money(p.price) }} {{ date(d) }}        locale formatting (tenant currency / time zone)
//! {{ csrf }}                                CSRF token (or {% call ui::csrf() %})
//! ```

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use askama::Template;
use askama::filters::Safe;
use axum::extract::FromRequestParts;
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, Utc};
use hos_auth::rbac::{Permission, Principal};
use hos_core::domain::Locale;
use hos_core::ids::TenantId;
use hos_core::money::{BillingCycle, Currency};
use hos_core::tenant::TenantContext;
use hos_db::Paged;
use hos_db::repo::{content, platform};
use http::StatusCode;
use http::request::Parts;
use rust_decimal::Decimal;

use crate::csrf::CsrfToken;
use crate::error::{AppError, AppResult};
use crate::extract::Htmx;
use crate::flash::{Flash, FlashMessage};
use crate::i18n::{self, Catalog};
use crate::locale::{self, ReqLocale};
use crate::session::SessionInfo;
use crate::state::AppState;
use crate::tenant::Tenant;

/// Implements `Deref<Target = PageCtx>` for template structs with a `ctx` field.
///
/// ```ignore
/// #[derive(Template)]
/// #[template(path = "storefront/home.html")]
/// struct HomePage { ctx: PageCtx, groups: Vec<GroupCard> }
/// crate::page!(HomePage);
/// ```
#[macro_export]
macro_rules! page {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl ::std::ops::Deref for $ty {
                type Target = $crate::view::PageCtx;
                fn deref(&self) -> &$crate::view::PageCtx {
                    &self.ctx
                }
            }
        )+
    };
}

/// Integer-like values accepted by [`PageCtx::tn`] / [`PageCtx::num`].
pub trait AsCount {
    /// As `i64`.
    fn as_count(&self) -> i64;
}

macro_rules! as_count {
    ($($t:ty),+) => {$(
        impl AsCount for $t {
            fn as_count(&self) -> i64 { i64::try_from(*self).unwrap_or(i64::MAX) }
        }
    )+};
}
as_count!(i64, i32, i16, u32, u16, u8, usize, u64);

impl<T: AsCount + ?Sized> AsCount for &T {
    fn as_count(&self) -> i64 {
        (**self).as_count()
    }
}

/// Everything a page template needs.
#[derive(Clone)]
pub struct PageCtx {
    /// Shared state (assets, i18n, config).
    pub state: AppState,
    /// Tenant (absent on `/platform`).
    pub tenant: Option<Arc<TenantContext>>,
    /// Brand name shown in the UI.
    pub site_name: String,
    /// Effective locale.
    pub locale: Locale,
    /// Tenant default locale (unprefixed URLs).
    pub default_locale: Locale,
    /// `tr` / `en` (for `<html lang>`).
    pub lang: &'static str,
    /// Translations of `locale`.
    pub cat: Arc<Catalog>,
    /// CSRF token for forms / `<meta name="csrf-token">`.
    pub csrf: String,
    /// Session (also a pending-2FA one).
    pub session: Option<Arc<SessionInfo>>,
    /// Incoming flash messages.
    pub flashes: Vec<FlashMessage>,
    /// Request path without the locale prefix.
    pub path: String,
    /// Raw query string.
    pub query: String,
    /// `scheme://host` of this request.
    pub origin: String,
    /// htmx headers.
    pub htmx: Htmx,
    /// Tenant currency (prices).
    pub currency: Currency,
    /// IANA time zone for dates.
    pub timezone: String,
    /// `<head>` SEO fragment from `hos-seo` (storefront pages).
    pub seo_head: String,
    /// JSON-LD `<script>` tags (storefront pages).
    pub jsonld: String,
    /// Plain page title (panel / auth pages; the storefront uses `seo_head`).
    pub title: String,
    /// Storefront navigation / footer data.
    pub store: Option<Arc<StoreChrome>>,
    /// Panel shell (sidebar menu, breadcrumbs).
    pub panel: Option<PanelChrome>,
    /// Per-locale hrefs of this page (set by `seo::apply`; used by the
    /// language switcher so translated slugs are followed).
    pub lang_links: Vec<(Locale, String)>,
    /// Extra `<body>` classes.
    pub body_class: String,
    /// `X-Request-Id`.
    pub request_id: String,
    /// Hash of the guest cart cookie (`hos_cart`), if any.
    pub cart_token_hash: Option<String>,
    /// Items in the visitor's cart (set by [`PageCtx::load_store`]).
    pub cart_count: i64,
    /// Tenant theme (template, fonts, colours; `crate::theme`), or a staff
    /// preview of unsaved values. Template helpers: `tpl()`,
    /// `theme_css_url()`, `font_preloads()`, `color_mode()` ….
    pub theme: Arc<crate::theme::Theme>,
}

impl FromRequestParts<AppState> for PageCtx {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let mut ctx = Self::from_parts(parts, state);
        if !locale::is_app_path(parts.uri.path()) {
            ctx.resolve_shop_currency(&parts.headers).await;
        }
        Ok(ctx)
    }
}

/// Storefront display currency chosen by a guest (`/currency/{code}`).
pub const CURRENCY_COOKIE: &str = "hos_currency";

impl PageCtx {
    /// Storefront prices: a signed-in client sees the account currency; a
    /// guest the currency of the `hos_currency` cookie when it is one of the
    /// tenant's active currencies; otherwise the tenant default. URLs never
    /// carry the currency (one canonical URL per page); cached pages vary on
    /// `Cookie`. Public machine-readable files keep the default currency.
    async fn resolve_shop_currency(&mut self, headers: &http::HeaderMap) {
        let Some(t) = self.tenant.clone() else {
            return;
        };
        if let Some(client_id) = self.session.as_ref().and_then(|s| s.client_id()) {
            if let Ok(Some(c)) =
                hos_db::repo::clients::get_client(&self.state.pool, t.id, client_id).await
            {
                self.currency = c.currency;
            }
            return;
        }
        let wanted = headers
            .get_all(http::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|h| hos_auth::session::cookie_value(h, CURRENCY_COOKIE))
            .and_then(|v| Currency::new(v.trim()).ok());
        let Some(wanted) = wanted.filter(|c| *c != self.currency) else {
            return;
        };
        if let Ok(chrome) = self
            .state
            .store_chrome
            .get(&self.state, &t, self.locale)
            .await
            && chrome.currencies.contains(&wanted)
        {
            self.currency = wanted;
        }
    }

    /// Active currencies a guest can switch between (empty: no switcher —
    /// one currency only, or a signed-in client, who pays in the account
    /// currency).
    pub fn currency_choices(&self) -> Vec<String> {
        let store = self.store();
        let client = self.session.as_ref().and_then(|s| s.client_id()).is_some();
        if client || store.currencies.len() < 2 {
            return Vec::new();
        }
        store
            .currencies
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect()
    }

    /// Whether `code` is the currency prices are shown in.
    pub fn is_currency(&self, code: &str) -> bool {
        self.currency.as_str() == code
    }

    /// `/currency/{code}?next=<current path and query>` (the staff theme
    /// preview parameters are left out: they only apply to the preview).
    pub fn currency_url(&self, code: &str) -> String {
        const PREVIEW: [&str; 9] = [
            "theme_preview",
            "tpl",
            "brand",
            "accent",
            "fb",
            "fh",
            "r",
            "m",
            "tg",
        ];
        let preview = self.query.contains("theme_preview=");
        let query: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(
                url::form_urlencoded::parse(self.query.as_bytes())
                    .filter(|(k, _)| !(preview && PREVIEW.contains(&k.as_ref()))),
            )
            .finish();
        let next = if query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{query}", self.path)
        };
        format!(
            "/currency/{code}?next={}",
            url::form_urlencoded::byte_serialize(next.as_bytes()).collect::<String>()
        )
    }
}

impl PageCtx {
    /// Builds the context from request parts (also used by the error pages).
    pub fn from_parts(parts: &mut Parts, state: &AppState) -> Self {
        let rl: ReqLocale = locale::resolve(parts);
        let tenant = parts.extensions.get::<Tenant>().map(|t| t.0.clone());
        let session = parts.extensions.get::<Arc<SessionInfo>>().cloned();
        let csrf = parts
            .extensions
            .get::<CsrfToken>()
            .map(|c| c.0.clone())
            .unwrap_or_default();
        let flashes = parts
            .extensions
            .get::<Flash>()
            .map(|f| f.0.clone())
            .unwrap_or_default();
        let origin = crate::csrf::request_origin(state, &parts.headers, &parts.extensions)
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or_else(|| state.config.base_url.origin().ascii_serialization());
        let request_id = parts
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let cart_token_hash = parts
            .headers
            .get_all(http::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|h| hos_auth::session::cookie_value(h, crate::routes::cart::CART_COOKIE))
            .filter(|t| !t.is_empty() && t.len() <= 128)
            .map(hos_core::crypto::hash_token);
        let mut ctx = Self::build(
            state,
            tenant,
            rl,
            csrf,
            session,
            flashes,
            // The owner's server pages are routed as `/platform/…`
            // (`crate::mode`); links and the language switch use the path
            // the browser asked for.
            parts
                .extensions
                .get::<crate::mode::ServerArea>()
                .map_or_else(|| parts.uri.path().to_owned(), |a| a.path.clone()),
            parts.uri.query().unwrap_or_default().to_owned(),
            origin,
            Htmx::from_headers(&parts.headers),
            request_id,
        );
        ctx.cart_token_hash = cart_token_hash;
        ctx.theme = crate::theme::for_request(parts, &state.assets, ctx.session.as_deref());
        ctx
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        state: &AppState,
        tenant: Option<Arc<TenantContext>>,
        rl: ReqLocale,
        csrf: String,
        session: Option<Arc<SessionInfo>>,
        flashes: Vec<FlashMessage>,
        path: String,
        query: String,
        origin: String,
        htmx: Htmx,
        request_id: String,
    ) -> Self {
        let site_name = tenant
            .as_ref()
            .map(|t| t.display_name().to_owned())
            .unwrap_or_else(|| "HostingOS".to_owned());
        let currency = tenant
            .as_ref()
            .map(|t| t.default_currency)
            .unwrap_or(Currency::USD);
        let timezone = tenant
            .as_ref()
            .map(|t| t.timezone.clone())
            .unwrap_or_else(|| "UTC".to_owned());
        Self {
            state: state.clone(),
            tenant,
            site_name,
            locale: rl.locale,
            default_locale: rl.default,
            lang: rl.locale.as_str(),
            cat: state.i18n.catalog(rl.locale),
            csrf,
            session,
            flashes,
            path,
            query,
            origin,
            htmx,
            currency,
            timezone,
            seo_head: String::new(),
            jsonld: String::new(),
            title: String::new(),
            store: None,
            panel: None,
            lang_links: Vec::new(),
            body_class: String::new(),
            request_id,
            cart_token_hash: None,
            cart_count: 0,
            theme: crate::theme::default_theme(&state.assets),
        }
    }

    /// A context for `state` without a request (CLI / tests).
    pub fn detached(state: &AppState, tenant: Option<Arc<TenantContext>>, locale: Locale) -> Self {
        let default = tenant
            .as_ref()
            .map(|t| t.default_locale)
            .unwrap_or_default();
        Self::build(
            state,
            tenant,
            ReqLocale {
                locale,
                default,
                source: locale::LocaleSource::Default,
            },
            String::new(),
            None,
            Vec::new(),
            "/".into(),
            String::new(),
            state.config.base_url.origin().ascii_serialization(),
            Htmx::default(),
            String::new(),
        )
    }

    // ---------------------------------------------------------------- i18n

    /// Translation of `key`.
    pub fn t(&self, key: &str) -> String {
        self.cat.t(key)
    }

    /// Translation with one `{name}` placeholder.
    pub fn tv(&self, key: &str, name: &str, value: impl Display) -> String {
        self.cat.tv(key, &[(name, &value)])
    }

    /// Translation with two placeholders.
    pub fn tv2(&self, key: &str, n1: &str, v1: impl Display, n2: &str, v2: impl Display) -> String {
        self.cat.tv(key, &[(n1, &v1), (n2, &v2)])
    }

    /// Plural translation (`{count}`).
    pub fn tn(&self, key: &str, count: impl AsCount) -> String {
        self.cat.tn(key, count.as_count())
    }

    // ---------------------------------------------------------------- URLs

    /// Localized path (`/kb` → `/en/kb`). Accepts anything printable so
    /// templates can write `url("/kb/" ~ slug)`.
    pub fn url(&self, path: impl Display) -> String {
        locale::localize_path(&path.to_string(), self.locale, self.default_locale)
    }

    /// Localized path in another locale (hreflang links, language switcher).
    pub fn url_in(&self, path: &str, locale: &Locale) -> String {
        locale::localize_path(path, *locale, self.default_locale)
    }

    /// Absolute URL of a localized path.
    pub fn abs(&self, path: &str) -> String {
        format!("{}{}", self.origin, self.url(path))
    }

    /// Versioned static asset URL.
    pub fn asset(&self, path: &str) -> String {
        self.state.assets.url(path)
    }

    /// Current path + query, localized (e.g. for `next=` parameters).
    pub fn current_url(&self) -> String {
        if self.query.is_empty() {
            self.url(&self.path)
        } else {
            format!("{}?{}", self.url(&self.path), self.query)
        }
    }

    /// Whether the current path is `prefix` or below it (`"/"` matches exactly).
    pub fn is_current(&self, prefix: &str) -> bool {
        if prefix == "/" {
            self.path == "/"
        } else {
            self.path == prefix || self.path.starts_with(&format!("{prefix}/"))
        }
    }

    /// Language switch link (`/lang/en?next=/hosting`).
    pub fn lang_url(&self, code: &str) -> String {
        format!(
            "/lang/{code}?next={}",
            url::form_urlencoded::byte_serialize(self.path.as_bytes()).collect::<String>()
        )
    }

    /// Link to this page in locale `code`: the page's own translation when
    /// known (storefront), otherwise `/lang/{code}?next=…` (sets the cookie).
    pub fn switch_url(&self, code: &str) -> String {
        self.lang_links
            .iter()
            .find(|(l, _)| l.as_str() == code)
            .map(|(_, href)| href.clone())
            .unwrap_or_else(|| self.lang_url(code))
    }

    /// `tel:` link for a display phone number (spaces / punctuation removed).
    pub fn tel_href(&self, phone: &str) -> String {
        let digits: String = phone
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '+')
            .collect();
        format!("tel:{digits}")
    }

    /// The locale that is not the current one.
    pub fn other_locale(&self) -> Locale {
        match self.locale {
            Locale::Tr => Locale::En,
            Locale::En => Locale::Tr,
        }
    }

    // ---------------------------------------------------------- formatting

    /// Money in the tenant currency.
    pub fn money(&self, amount: &Decimal) -> String {
        i18n::format_money(*amount, self.currency, self.locale)
    }

    /// Money in an explicit currency.
    pub fn money_in(&self, amount: &Decimal, currency: &Currency) -> String {
        i18n::format_money(*amount, *currency, self.locale)
    }

    /// Grouped integer.
    pub fn num(&self, n: impl AsCount) -> String {
        i18n::format_int(n.as_count(), self.locale)
    }

    /// Long date in the tenant time zone.
    pub fn date(&self, dt: &DateTime<Utc>) -> String {
        i18n::format_date(*dt, &self.timezone, self.locale)
    }

    /// Long date or `—`.
    pub fn date_opt(&self, dt: &Option<DateTime<Utc>>) -> String {
        dt.map(|d| self.date(&d)).unwrap_or_else(|| "—".into())
    }

    /// Date and time.
    pub fn datetime(&self, dt: &DateTime<Utc>) -> String {
        i18n::format_datetime(*dt, &self.timezone, self.locale)
    }

    /// Current year in the tenant time zone (footer copyright).
    pub fn year(&self) -> i32 {
        use chrono::Datelike as _;
        Utc::now()
            .with_timezone(&i18n::parse_tz(&self.timezone))
            .year()
    }

    /// ISO 8601 (for `<time datetime>`).
    pub fn iso(&self, dt: &DateTime<Utc>) -> String {
        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// Billing cycle label (`Aylık`).
    pub fn cycle(&self, c: &BillingCycle) -> String {
        self.t(&i18n::cycle_key(*c))
    }

    /// "per period" suffix (`/ay`).
    pub fn per(&self, c: &BillingCycle) -> String {
        self.t(&i18n::per_cycle_key(*c))
    }

    // ------------------------------------------------------------- session

    /// Fully logged in.
    pub fn logged_in(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_authenticated())
    }

    /// Display name of the user ("" when anonymous).
    pub fn user_name(&self) -> String {
        self.session
            .as_ref()
            .filter(|s| s.is_authenticated())
            .map(|s| s.display_name())
            .unwrap_or_default()
    }

    /// Initials for the avatar.
    pub fn user_initials(&self) -> String {
        let name = self.user_name();
        let mut out: String = name
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(2)
            .collect();
        if out.is_empty() {
            out = "?".into();
        }
        out.to_uppercase()
    }

    /// Panel home of the logged-in user (`/client`, `/admin`, `/platform`).
    pub fn home_url(&self) -> String {
        self.session
            .as_ref()
            .filter(|s| s.is_authenticated())
            .map(|s| self.url(s.home_path()))
            .unwrap_or_else(|| self.url("/auth/login"))
    }

    /// Public demo banner (`None` outside demo mode).
    pub fn demo_banner(&self) -> Option<crate::demo::Banner> {
        let tenant_user = self.tenant.is_some()
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.is_authenticated() && s.user.is_some());
        crate::demo::banner(&self.state, tenant_user)
    }

    /// Demo logins for a login page (`platform`: the platform console's).
    pub fn demo_logins(&self, platform: bool) -> Vec<crate::demo::LoginHint> {
        crate::demo::login_hints(
            &self.state,
            self.tenant.as_ref().map(|t| t.slug.as_str()),
            platform,
        )
    }

    /// Staff principal?
    pub fn is_staff(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| matches!(s.principal, Principal::Staff { .. }))
    }

    /// Whether the staff user has `permission` (always false for others).
    pub fn can(&self, permission: &str) -> bool {
        Permission::parse(permission)
            .is_some_and(|p| self.session.as_ref().is_some_and(|s| s.principal.can(p)))
    }

    /// Whether the signed-in staff member holds `p` (false for others).
    pub fn allows(&self, p: Permission) -> bool {
        self.session.as_ref().is_some_and(|s| s.principal.can(p))
    }

    /// Single-company mode (`HOS_MODE=single`): no multi-company wording or
    /// links.
    pub fn single_company(&self) -> bool {
        self.state.web.single()
    }

    /// Development extras enabled.
    pub fn debug(&self) -> bool {
        self.state.web.debug
    }

    // ----------------------------------------------------------- builders

    /// Sets the plain `<title>` (panel / auth pages).
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    /// Full `<title>` text for non-SEO pages (`Page · Site`).
    pub fn full_title(&self) -> String {
        if self.title.is_empty() {
            self.site_name.clone()
        } else {
            format!("{} · {}", self.title, self.site_name)
        }
    }

    /// Loads the storefront chrome (cached) — call in storefront handlers.
    pub async fn load_store(&mut self) -> AppResult<()> {
        if let Some(t) = &self.tenant {
            self.store = Some(
                self.state
                    .store_chrome
                    .get(&self.state, t, self.locale)
                    .await?,
            );
            // Per visitor, so not part of the cached chrome.
            let client_user = self.session.as_ref().and_then(|s| match &s.principal {
                Principal::Client { user_id, .. } => Some(*user_id),
                _ => None,
            });
            self.cart_count = hos_db::repo::admin_settings::cart_item_count(
                &self.state.pool,
                t.id,
                client_user,
                self.cart_token_hash.as_deref(),
            )
            .await?;
        }
        Ok(())
    }

    /// The storefront chrome (empty default when not loaded).
    pub fn store(&self) -> Arc<StoreChrome> {
        self.store.clone().unwrap_or_default()
    }

    // -------------------------------------------------------------- atoms
    //
    // Tiny markup pieces used everywhere; they return `Safe` so `{{ icon("x") }}`
    // is not escaped (every interpolated value is escaped inside).

    /// URL of the icon sprite.
    pub fn icons_url(&self) -> String {
        self.asset("img/icons.svg")
    }

    /// Staff session opened from the platform console ("sign in as tenant
    /// owner"); the admin top bar then shows an exit button.
    pub fn platform_impersonation(&self) -> bool {
        self.session.as_ref().is_some_and(|s| {
            s.data
                .get("platform_impersonation")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
    }

    /// The tenant's uploaded favicon (`branding.favicon_url`, set by
    /// `/admin/settings/general`), if any.
    pub fn favicon_url(&self) -> Option<String> {
        self.tenant
            .as_ref()
            .and_then(|t| t.branding.favicon_url.clone())
            .filter(|u| u.starts_with('/') && !u.starts_with("//"))
    }

    /// `<svg class="icon">` referencing `static/img/icons.svg#name`.
    pub fn icon(&self, name: &str) -> Safe<String> {
        self.icon_cls(name, "")
    }

    /// Icon with extra classes (`icon--lg`). The intrinsic 16×16 size keeps
    /// the box fixed even before the stylesheet sizes it (no layout shift).
    pub fn icon_cls(&self, name: &str, class: &str) -> Safe<String> {
        let class = if class.is_empty() {
            "icon".to_owned()
        } else {
            format!("icon {}", escape_html(class))
        };
        Safe(format!(
            r##"<svg class="{class}" width="16" height="16" aria-hidden="true" focusable="false"><use href="{}#{}"></use></svg>"##,
            escape_html(&self.icons_url()),
            escape_html(name)
        ))
    }

    /// `<span class="badge badge--{tone}">text</span>` (tone: brand, success,
    /// warning, danger, info, accent or "" for neutral).
    pub fn badge(&self, text: impl Display, tone: &str) -> Safe<String> {
        let tone = if tone.is_empty() {
            String::new()
        } else {
            format!(" badge--{}", escape_html(tone))
        };
        Safe(format!(
            r#"<span class="badge badge--dot{tone}">{}</span>"#,
            escape_html(&text.to_string())
        ))
    }

    /// Status badge for the usual entity states (`active`, `unpaid`, `open` …):
    /// label from `common.status.<status>`, colour from [`status_tone`].
    pub fn status_badge(&self, status: &str) -> Safe<String> {
        let key = format!("common.status.{status}");
        let label = if self.cat.has(&key) {
            self.t(&key)
        } else {
            status.replace('_', " ")
        };
        self.badge(&label, status_tone(status))
    }
}

/// Badge tone for an entity status string.
pub fn status_tone(status: &str) -> &'static str {
    match status {
        "active" | "paid" | "completed" | "published" | "verified" | "answered" | "success"
        | "registered" => "success",
        "pending" | "unpaid" | "draft" | "in_progress" | "pending_transfer" | "customer_reply"
        | "awaiting" | "on_hold" => "warning",
        "overdue" | "suspended" | "failed" | "fraud" | "cancelled" | "terminated" | "expired"
        | "refunded" | "closed_failed" => "danger",
        "open" | "new" | "scheduled" | "info" => "info",
        // closed, inactive, archived, … → neutral
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Renders a template into a `200 text/html` response.
pub fn render<T: Template>(tpl: &T) -> AppResult<Response> {
    Ok(Html(tpl.render()?).into_response())
}

/// Renders with an explicit status (e.g. `422` for a form with errors).
pub fn render_status<T: Template>(status: StatusCode, tpl: &T) -> AppResult<Response> {
    Ok((status, Html(tpl.render()?)).into_response())
}

/// HTML-escapes text for hand-built markup.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Navigation types
// ---------------------------------------------------------------------------

/// A link (navigation, footer, breadcrumbs).
#[derive(Debug, Clone, Default)]
pub struct NavItem {
    /// Label (translated).
    pub label: String,
    /// Localized href.
    pub href: String,
    /// Optional one-line description.
    pub description: String,
    /// Icon id in `static/img/icons.svg`.
    pub icon: String,
}

/// Breadcrumb entry (last one: current page, no link).
#[derive(Debug, Clone)]
pub struct Crumb {
    /// Label.
    pub label: String,
    /// Localized href (`None` for the current page).
    pub href: Option<String>,
}

impl Crumb {
    /// Linked crumb.
    pub fn link(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: Some(href.into()),
        }
    }

    /// Current page.
    pub fn current(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: None,
        }
    }
}

/// Company facts for the storefront footer (all from the DB).
#[derive(Debug, Clone, Default)]
pub struct CompanyInfo {
    /// Legal name.
    pub legal_name: String,
    /// One-line address.
    pub address: String,
    /// Contact e-mail.
    pub email: String,
    /// Contact phone.
    pub phone: String,
    /// Tax office.
    pub tax_office: String,
    /// Tax number.
    pub tax_number: String,
    /// Support e-mail from branding.
    pub support_email: String,
}

/// Storefront navigation + footer data.
#[derive(Debug, Clone, Default)]
pub struct StoreChrome {
    /// Product groups (main navigation).
    pub groups: Vec<NavItem>,
    /// CMS pages flagged `show_in_nav`.
    pub nav_pages: Vec<NavItem>,
    /// CMS pages flagged `show_in_footer` (all of them, in nav order).
    pub footer_pages: Vec<NavItem>,
    /// The `legal` template subset of `footer_pages` (footer "Legal").
    pub legal_pages: Vec<NavItem>,
    /// The other footer pages (footer "Company", after `nav_pages`).
    pub company_pages: Vec<NavItem>,
    /// Legal / contact facts.
    pub company: CompanyInfo,
    /// Logo URL from branding.
    pub logo_url: String,
    /// Short company description (`seo.default_description` setting).
    pub tagline: String,
    /// Terms of service link accepted at checkout (`orders.terms_url`
    /// setting; empty = fall back to a footer page).
    pub terms_url: String,
    /// Catalogue prices are gross (`billing.prices_include_tax`): the
    /// storefront's price notes say so instead of "plus VAT".
    pub prices_include_tax: bool,
    /// Active currencies (default first): the guest currency switcher.
    pub currencies: Vec<Currency>,
    /// A storefront bundle exists (footer link to `/bundles`).
    pub has_bundles: bool,
    /// A public download exists (footer link to `/downloads`).
    pub has_downloads: bool,
}

#[derive(Clone)]
struct ChromeEntry {
    at: Instant,
    value: Arc<StoreChrome>,
}

/// TTL cache of [`StoreChrome`] per tenant + locale.
pub struct StoreChromeCache {
    ttl: Duration,
    map: RwLock<HashMap<(TenantId, Locale), ChromeEntry>>,
}

impl StoreChromeCache {
    /// New cache.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            map: RwLock::new(HashMap::new()),
        }
    }

    /// Drops everything (after catalog / page edits).
    pub fn invalidate_all(&self) {
        self.map.write().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Cached chrome for `tenant` in `locale`.
    pub async fn get(
        &self,
        state: &AppState,
        tenant: &TenantContext,
        locale: Locale,
    ) -> AppResult<Arc<StoreChrome>> {
        let key = (tenant.id, locale);
        if let Some(e) = self
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .filter(|e| e.at.elapsed() < self.ttl)
        {
            return Ok(e.value.clone());
        }
        let value = Arc::new(load_store_chrome(state, tenant, locale).await?);
        self.map.write().unwrap_or_else(|e| e.into_inner()).insert(
            key,
            ChromeEntry {
                at: Instant::now(),
                value: value.clone(),
            },
        );
        Ok(value)
    }
}

async fn load_store_chrome(
    state: &AppState,
    tenant: &TenantContext,
    locale: Locale,
) -> AppResult<StoreChrome> {
    let lp = |p: &str| locale::localize_path(p, locale, tenant.default_locale);
    let lang = locale.as_str();
    let groups = hos_db::repo::catalog::list_product_groups(&state.pool, tenant.id, false)
        .await?
        .into_iter()
        .map(|g| NavItem {
            label: hos_db::repo::catalog::translation(&g.translations, lang, "name")
                .unwrap_or(&g.name)
                .to_owned(),
            href: lp(&format!("/{}", g.slug)),
            description: hos_db::repo::catalog::translation(&g.translations, lang, "tagline")
                .map(str::to_owned)
                .or(g.tagline.clone())
                .unwrap_or_default(),
            icon: g.icon.clone().unwrap_or_else(|| "box".into()),
        })
        .collect();
    let pages = content::nav_pages(&state.pool, tenant.id, locale).await?;
    let to_item = |p: &content::PageRow| NavItem {
        label: p.title.clone(),
        href: lp(&format!("/{}", p.slug)),
        description: String::new(),
        icon: String::new(),
    };
    let nav_pages = pages
        .iter()
        .filter(|p| p.show_in_nav)
        .map(to_item)
        .collect();
    let footer_pages = pages
        .iter()
        .filter(|p| p.show_in_footer)
        .map(to_item)
        .collect();
    let legal_pages = pages
        .iter()
        .filter(|p| p.show_in_footer && p.template == "legal")
        .map(to_item)
        .collect();
    let company_pages = pages
        .iter()
        .filter(|p| p.show_in_footer && p.template != "legal" && !p.show_in_nav)
        .map(to_item)
        .collect();
    let row = platform::get_tenant(&state.pool, tenant.id).await?;
    let company = row
        .map(|r| {
            let addr = &r.address;
            let part = |k: &str| {
                addr.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            };
            let address = [
                part("line1"),
                part("city"),
                part("state"),
                part("postal_code"),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
            CompanyInfo {
                legal_name: r.legal_name.unwrap_or_else(|| r.name.clone()),
                address,
                email: r.contact_email.unwrap_or_default(),
                phone: r.contact_phone.unwrap_or_default(),
                tax_office: r.tax_office.unwrap_or_default(),
                tax_number: r.tax_number.unwrap_or_default(),
                support_email: tenant.branding.support_email.clone().unwrap_or_default(),
            }
        })
        .unwrap_or_default();
    let tagline = platform::get_setting(&state.pool, tenant.id, "seo.default_description")
        .await?
        .and_then(|v| crate::seo::localized_setting(&v, locale))
        .unwrap_or_default();
    let terms_url = platform::get_setting(&state.pool, tenant.id, "orders.terms_url")
        .await?
        .and_then(|v| v.as_str().map(str::trim).map(str::to_owned))
        .unwrap_or_default();
    // Same reading as hos_billing::settings (bool, "true"/"1"/"on", 0/1).
    let prices_include_tax =
        platform::get_setting(&state.pool, tenant.id, "billing.prices_include_tax")
            .await?
            .is_some_and(|v| match v {
                serde_json::Value::Bool(b) => b,
                serde_json::Value::String(s) => matches!(s.trim(), "true" | "1" | "on" | "yes"),
                serde_json::Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
                _ => false,
            });
    let currencies = hos_db::repo::catalog::list_currencies(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .map(|c| c.code)
        .collect();
    let (has_bundles, has_downloads): (bool, bool) = sqlx::query_as(
        "select exists(select 1 from product_bundles b where b.tenant_id = $1 and b.is_active
                         and b.show_on_storefront),
                exists(select 1 from downloads d where d.tenant_id = $1 and d.access = 'public'
                         and not d.is_hidden)",
    )
    .bind(tenant.id)
    .fetch_one(&state.pool)
    .await?;
    Ok(StoreChrome {
        has_bundles,
        has_downloads,
        currencies,
        groups,
        nav_pages,
        footer_pages,
        legal_pages,
        company_pages,
        company,
        logo_url: tenant.branding.logo_url.clone().unwrap_or_default(),
        tagline,
        terms_url,
        prices_include_tax,
    })
}

// ---------------------------------------------------------------------------
// Panel shell
// ---------------------------------------------------------------------------

/// One sidebar entry.
#[derive(Debug, Clone)]
pub struct MenuItem {
    /// Label (translated).
    pub label: String,
    /// Localized href.
    pub href: String,
    /// Icon id.
    pub icon: &'static str,
    /// Badge text (counts), empty for none.
    pub badge: String,
    /// Currently active.
    pub active: bool,
}

/// A titled group of sidebar entries.
#[derive(Debug, Clone)]
pub struct MenuSection {
    /// Section title ("" for none).
    pub label: String,
    /// Entries.
    pub items: Vec<MenuItem>,
}

/// Static menu definition (each area keeps its own list in its `menu.rs`).
#[derive(Debug, Clone, Copy)]
pub struct MenuDef {
    /// Translation key of the label.
    pub key: &'static str,
    /// Unlocalized href.
    pub href: &'static str,
    /// Icon id.
    pub icon: &'static str,
    /// Staff permission needed to see it (`None` = everyone in the area).
    pub permission: Option<Permission>,
    /// Other path prefixes whose pages belong to this entry (pages without
    /// an entry of their own, e.g. canned replies under "Tickets").
    pub also: &'static [&'static str],
}

/// A section of [`MenuDef`]s.
#[derive(Debug, Clone, Copy)]
pub struct MenuSectionDef {
    /// Translation key of the title (`""` for none).
    pub key: &'static str,
    /// Entries.
    pub items: &'static [MenuDef],
}

/// The panel shell data.
#[derive(Debug, Clone)]
pub struct PanelChrome {
    /// `client` / `admin` / `platform` (CSS hook, logout target).
    pub area: &'static str,
    /// Area title ("Müşteri Paneli").
    pub area_label: String,
    /// Sidebar.
    pub sections: Vec<MenuSection>,
    /// Breadcrumbs.
    pub crumbs: Vec<Crumb>,
}

impl PanelChrome {
    /// Builds the sidebar from definitions; entries are filtered by
    /// `allowed(permission)` and the active entry is the one with the longest
    /// matching path prefix (its href or one of its `also` prefixes). The
    /// area root (`/admin`, `/client` …, the dashboard) only matches itself,
    /// so a page without an entry highlights nothing rather than the
    /// dashboard.
    pub fn build(
        ctx: &PageCtx,
        area: &'static str,
        area_label_key: &str,
        defs: &[MenuSectionDef],
        allowed: impl Fn(Option<Permission>) -> bool,
    ) -> Self {
        let root = format!("/{area}");
        let matched = |d: &MenuDef| -> Option<usize> {
            std::iter::once(d.href)
                .chain(d.also.iter().copied())
                .filter(|p| {
                    if *p == root {
                        ctx.path == *p
                    } else {
                        ctx.is_current(p)
                    }
                })
                .map(str::len)
                .max()
        };
        let active_href = defs
            .iter()
            .flat_map(|s| s.items.iter())
            .filter_map(|d| matched(d).map(|len| (len, d.href)))
            .max_by_key(|(len, _)| *len)
            .map(|(_, href)| href);
        let sections = defs
            .iter()
            .map(|s| MenuSection {
                label: if s.key.is_empty() {
                    String::new()
                } else {
                    ctx.t(s.key)
                },
                items: s
                    .items
                    .iter()
                    .filter(|d| allowed(d.permission))
                    .map(|d| MenuItem {
                        label: ctx.t(d.key),
                        href: ctx.url(d.href),
                        icon: d.icon,
                        badge: String::new(),
                        active: Some(d.href) == active_href,
                    })
                    .collect::<Vec<_>>(),
            })
            .filter(|s| !s.items.is_empty())
            .collect();
        Self {
            area,
            area_label: ctx.t(area_label_key),
            sections,
            crumbs: Vec::new(),
        }
    }

    /// Adds breadcrumbs.
    #[must_use]
    pub fn with_crumbs(mut self, crumbs: Vec<Crumb>) -> Self {
        self.crumbs = crumbs;
        self
    }
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

/// One page link.
#[derive(Debug, Clone)]
pub struct PageLink {
    /// Page number (0 for a gap).
    pub n: i64,
    /// Href.
    pub href: String,
    /// Current page.
    pub current: bool,
    /// "…" placeholder.
    pub gap: bool,
}

/// Pagination view model (`{% call ui::pagination(pager) %}`).
#[derive(Debug, Clone, Default)]
pub struct Pager {
    /// Current page.
    pub page: i64,
    /// Total pages.
    pub total_pages: i64,
    /// Total items.
    pub total: i64,
    /// Previous page href.
    pub prev: Option<String>,
    /// Next page href.
    pub next: Option<String>,
    /// Numbered links (with gaps).
    pub links: Vec<PageLink>,
}

impl Pager {
    /// Builds links for `paged`; `base` is the localized path, `params` the
    /// other query parameters to keep (`q`, `sort`, …).
    pub fn new<T>(paged: &Paged<T>, base: &str, params: &[(&str, &str)]) -> Self {
        let total_pages = paged.total_pages();
        let page = paged.page_number().min(total_pages);
        let href = |n: i64| {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in params.iter().filter(|(_, v)| !v.is_empty()) {
                ser.append_pair(k, v);
            }
            if n > 1 {
                ser.append_pair("page", &n.to_string());
            }
            let q = ser.finish();
            if q.is_empty() {
                base.to_owned()
            } else {
                format!("{base}?{q}")
            }
        };
        let mut links = Vec::new();
        let mut last = 0;
        for n in 1..=total_pages {
            let show = n == 1 || n == total_pages || (n - page).abs() <= 2;
            if show {
                if n - last > 1 {
                    links.push(PageLink {
                        n: 0,
                        href: String::new(),
                        current: false,
                        gap: true,
                    });
                }
                links.push(PageLink {
                    n,
                    href: href(n),
                    current: n == page,
                    gap: false,
                });
                last = n;
            }
        }
        Self {
            page,
            total_pages,
            total: paged.total,
            prev: (page > 1).then(|| href(page - 1)),
            next: (page < total_pages).then(|| href(page + 1)),
            links,
        }
    }

    /// More than one page.
    pub fn visible(&self) -> bool {
        self.total_pages > 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pager_links() {
        let paged: Paged<u8> = Paged {
            items: vec![],
            total: 250,
            limit: 25,
            offset: 100,
        };
        let p = Pager::new(&paged, "/kb", &[("q", "ssl"), ("x", "")]);
        assert_eq!(p.page, 5);
        assert_eq!(p.total_pages, 10);
        assert_eq!(p.prev.as_deref(), Some("/kb?q=ssl&page=4"));
        assert_eq!(p.next.as_deref(), Some("/kb?q=ssl&page=6"));
        let nums: Vec<i64> = p.links.iter().map(|l| l.n).collect();
        assert_eq!(nums, vec![1, 0, 3, 4, 5, 6, 7, 0, 10]);
        assert_eq!(p.links[0].href, "/kb?q=ssl");
        assert!(p.visible());
    }

    #[test]
    fn escaping() {
        assert_eq!(
            escape_html("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
