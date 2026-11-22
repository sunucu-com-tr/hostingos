//! Appearance / theming integration tests: the settings page (save,
//! validation, reset, permission), the per-tenant `/brand.css` (content,
//! caching, tenant isolation), the staff-only live preview and every design
//! template rendering the customer pages.
//!
//! Like the other web tests they need the database from `.env`; without it
//! they print a notice and pass. Every test works on its own throw-away
//! tenant (never the demo tenant).

use crate::common::{Res, TestApp};
use hos_core::domain::{ProductType, UserKind};
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::identity::{self, NewUser};
use hos_db::repo::{catalog, platform};
use http::{Method, StatusCode};
use rust_decimal::Decimal;
use serde_json::json;

const PASSWORD: &str = "Correct-Horse-Battery-9";
const PAGE: &str = "/admin/settings/appearance";
const TEMPLATES: [&str; 6] = [
    "classic",
    "cloud",
    "corporate",
    "terminal",
    "studio",
    "editorial",
];

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Logs in a fresh staff member with `role`; returns the session's CSRF token.
async fn staff(app: &mut TestApp, role: &str) -> String {
    let email = format!(
        "theme-{role}-{}@staff.test",
        &uuid::Uuid::now_v7().simple().to_string()[16..]
    );
    let user = identity::create_user(
        app.pool(),
        app.tenant_id,
        &NewUser {
            email: email.clone(),
            password_hash: Some(hos_auth::password::hash_password(PASSWORD).unwrap()),
            kind: UserKind::Staff,
            first_name: "Tema".into(),
            last_name: "Personel".into(),
            phone: None,
            locale: hos_core::domain::Locale::Tr,
            email_verified: true,
        },
    )
    .await
    .expect("user");
    let r = identity::role_by_slug(app.pool(), app.tenant_id, role)
        .await
        .unwrap()
        .expect("role");
    identity::assign_role(app.pool(), app.tenant_id, user.id, r.id)
        .await
        .unwrap();
    let token = app.get("/auth/login").await.csrf();
    let res = app
        .post(
            "/auth/login",
            &[
                ("csrf_token", &token),
                ("email", &email),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "login: {}", res.body);
    let dash = app.get("/admin").await;
    assert_eq!(dash.status, StatusCode::OK, "{}", dash.body);
    dash.csrf()
}

/// Posts the appearance form: `fields` on top of an all-defaults form.
async fn save(app: &mut TestApp, csrf: &str, fields: &[(&str, &str)]) -> Res {
    let mut all: Vec<(&str, &str)> = vec![("csrf_token", csrf)];
    for key in [
        "template",
        "brand_color",
        "accent_color",
        "font_body",
        "font_heading",
        "radius",
        "mode",
    ] {
        let value = fields
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(if key == "template" { "classic" } else { "" }, |(_, v)| *v);
        all.push((key, value));
    }
    all.extend(fields.iter().filter(|(k, _)| *k == "mode_toggle"));
    app.post(PAGE, &all).await
}

async fn appearance(app: &TestApp) -> std::collections::BTreeMap<String, serde_json::Value> {
    platform::get_settings(app.pool(), app.tenant_id, "appearance.")
        .await
        .expect("settings")
}

async fn brand_colors(app: &TestApp) -> (Option<String>, Option<String>) {
    let b = platform::get_tenant(app.pool(), app.tenant_id)
        .await
        .unwrap()
        .expect("tenant")
        .branding();
    (
        b.primary_color.map(|c| c.to_string()),
        b.accent_color.map(|c| c.to_string()),
    )
}

async fn audits(app: &TestApp, action: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "select count(*) from audit_log where tenant_id = $1 and action = $2",
    )
    .bind(app.tenant_id)
    .bind(action)
    .fetch_one(app.pool())
    .await
    .expect("audit count")
}

/// Sets the stored template directly (as the settings page would).
async fn use_template(app: &TestApp, key: &str) {
    app.setting("appearance.template", json!(key)).await;
    hos_web::theme::invalidate(app.tenant_id);
}

/// The `href` of the first `<link>` whose `href` contains `needle`.
fn link_href(html: &str, needle: &str) -> Option<String> {
    html.split("href=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap_or_default())
        .find(|href| href.contains(needle))
        .map(|href| href.replace("&amp;", "&").replace("&#38;", "&"))
}

fn header<'a>(res: &'a Res, name: &str) -> &'a str {
    res.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

fn html_tag(res: &Res) -> &str {
    let start = res.body.find("<html").expect("<html>");
    let end = start + res.body[start..].find('>').expect("<html …>");
    &res.body[start..end]
}

/// A product group with one priced product: `/tema-paketleri`,
/// `/tema-paketleri/tema-pro`.
async fn catalog_fixture(app: &TestApp) {
    let group = catalog::create_product_group(
        app.pool(),
        app.tenant_id,
        &catalog::ProductGroupInput {
            slug: "tema-paketleri".into(),
            name: "Tema Paketleri".into(),
            tagline: Some("Tema testleri için paketler".into()),
            description: None,
            translations: json!({}),
            icon: None,
            is_hidden: false,
            sort_order: 0,
        },
    )
    .await
    .expect("group");
    let mut input = catalog::ProductInput::new(
        "tema-pro",
        "Tema Pro",
        ProductType::SharedHosting,
        Some(group.id),
    );
    input.features = json!(["10 GB NVMe", "Ücretsiz SSL"]);
    input.is_featured = true;
    let product = catalog::create_product(app.pool(), app.tenant_id, &input)
        .await
        .expect("product");
    let currency = catalog::list_currencies(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.is_default)
        .map_or(Currency::TRY, |c| c.code);
    for (cycle, price) in [(BillingCycle::Monthly, 120), (BillingCycle::Annual, 1200)] {
        catalog::set_product_price(
            app.pool(),
            app.tenant_id,
            product.id,
            currency,
            cycle,
            Decimal::from(price),
            Decimal::ZERO,
        )
        .await
        .expect("price");
    }
}

// ---------------------------------------------------------------------------
// settings page
// ---------------------------------------------------------------------------

#[tokio::test]
async fn appearance_saves_audits_and_applies() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;

    let page = app.get(PAGE).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    for key in TEMPLATES {
        assert!(
            page.body.contains(&format!("value=\"{key}\"")),
            "template {key} offered"
        );
    }
    assert!(
        page.body.contains("fonts.css"),
        "specimen stylesheet linked"
    );
    assert!(page.body.contains("data-preview-frame"), "live preview");

    let res = save(
        &mut app,
        &csrf,
        &[
            ("template", "cloud"),
            ("brand_color", "0A7D4F"),
            ("accent_color", "#abc"),
            ("font_body", "inter"),
            ("font_heading", "fraunces"),
            ("radius", "sharp"),
            ("mode", "dark"),
        ],
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(res.location().ends_with(PAGE), "{}", res.location());

    let stored = appearance(&app).await;
    assert_eq!(stored["appearance.template"], json!("cloud"));
    assert_eq!(stored["appearance.font_body"], json!("inter"));
    assert_eq!(stored["appearance.font_heading"], json!("fraunces"));
    assert_eq!(stored["appearance.radius"], json!("sharp"));
    assert_eq!(stored["appearance.mode"], json!("dark"));
    // An unchecked checkbox: visitors may not toggle.
    assert_eq!(stored["appearance.mode_toggle"], json!(false));
    assert_eq!(
        brand_colors(&app).await,
        (Some("#0a7d4f".into()), Some("#abc".into()))
    );
    assert_eq!(audits(&app, "settings.appearance").await, 1);
    assert_eq!(audits(&app, "settings.branding").await, 1);

    // The form shows the stored values.
    let page = app.get(PAGE).await;
    assert!(page.body.contains("#0a7d4f"));

    // Customer pages: template, fixed dark mode, no toggle, theme assets.
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    let tag = html_tag(&home).to_owned();
    assert!(tag.contains("data-template=\"cloud\""), "{tag}");
    assert!(tag.contains("data-theme=\"dark\""), "{tag}");
    assert!(tag.contains("data-mode-locked"), "{tag}");
    assert!(!tag.contains("data-theme-preview"), "{tag}");
    assert!(!home.body.contains("data-theme-toggle"), "toggle hidden");
    assert!(home.body.contains("hero--cloud"));
    assert!(link_href(&home.body, "css/themes/cloud.css").is_some());
    assert!(link_href(&home.body, "/brand.css?v=").is_some());
    // Only the selected families are preloaded (+ Geist Mono for code).
    assert!(home.body.contains("inter-latin-wght.woff2"));
    assert!(home.body.contains("fraunces-latin-wght.woff2"));
    assert!(!home.body.contains("manrope-latin"));

    // Admin keeps its own look: no template, but the fonts / accent sheet.
    let admin = app.get("/admin").await;
    assert!(!html_tag(&admin).contains("data-template"));
    assert!(link_href(&admin.body, "css/themes/").is_none());
    assert!(link_href(&admin.body, "/brand.css?v=").is_some());
    assert!(
        admin.body.contains("data-theme-toggle"),
        "admin keeps its toggle"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn appearance_rejects_invalid_values() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;
    let cases: [&[(&str, &str)]; 8] = [
        &[("template", "nope")],
        &[("brand_color", "red")],
        &[("brand_color", "#12345")],
        &[("accent_color", "#1234567z")],
        &[("font_body", "comic-sans")],
        &[("font_heading", "../../etc/passwd")],
        &[("radius", "huge")],
        &[("mode", "neon")],
    ];
    for fields in cases {
        let res = save(&mut app, &csrf, fields).await;
        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{fields:?} must be rejected"
        );
        assert!(res.body.contains("field__error"), "{fields:?}: error shown");
    }
    // An invalid field rejects the whole form: nothing was stored.
    let res = save(
        &mut app,
        &csrf,
        &[("template", "terminal"), ("brand_color", "javascript:1")],
    )
    .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(appearance(&app).await.is_empty(), "nothing saved");
    assert_eq!(brand_colors(&app).await, (None, None));
    assert_eq!(audits(&app, "settings.appearance").await, 0);
    // Values typed in are kept for correction.
    assert!(res.body.contains("javascript:1"));

    // CSRF is enforced.
    let res = app
        .post(PAGE, &[("csrf_token", "forged"), ("template", "cloud")])
        .await;
    assert!(res.status.is_client_error(), "{}", res.status);
    assert!(appearance(&app).await.is_empty());

    app.cleanup().await;
}

#[tokio::test]
async fn appearance_reset_restores_template_defaults() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;
    let res = save(
        &mut app,
        &csrf,
        &[
            ("template", "corporate"),
            ("brand_color", "#ff0066"),
            ("font_body", "manrope"),
            ("radius", "round"),
            ("mode", "light"),
        ],
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let res = app
        .post(&format!("{PAGE}/reset"), &[("csrf_token", &csrf)])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let stored = appearance(&app).await;
    assert_eq!(stored["appearance.template"], json!("corporate"), "kept");
    assert_eq!(stored["appearance.font_body"], json!(""));
    assert_eq!(stored["appearance.radius"], json!(""));
    assert_eq!(stored["appearance.mode"], json!(""));
    assert_eq!(stored["appearance.mode_toggle"], json!(true));
    assert_eq!(brand_colors(&app).await, (None, None));

    // The theme follows at once (cache invalidated).
    let home = app.get("/").await;
    let css = link_href(&home.body, "/brand.css?v=").expect("brand.css");
    let sheet = app.get(&css).await;
    assert!(!sheet.body.contains("#ff0066"));
    assert!(!sheet.body.contains("Manrope"));
    assert!(
        sheet.body.contains("IBM Plex Sans"),
        "corporate default font"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn appearance_requires_settings_permission() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    // Anonymous: to the login page.
    let res = app.get(PAGE).await;
    assert_ne!(res.status, StatusCode::OK);
    assert!(
        res.status.is_redirection() || res.status.is_client_error(),
        "{}",
        res.status
    );

    // Support staff (no settings.manage).
    let csrf = staff(&mut app, "support").await;
    assert_eq!(app.get(PAGE).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.get(&format!("{PAGE}/fonts.css")).await.status,
        StatusCode::FORBIDDEN
    );
    let res = save(&mut app, &csrf, &[("template", "terminal")]).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app
        .post(&format!("{PAGE}/reset"), &[("csrf_token", &csrf)])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app
        .post(
            &format!("{PAGE}/preview"),
            &[("csrf_token", &csrf), ("template", "terminal")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(appearance(&app).await.is_empty());

    // Nor can they preview unsaved values on the storefront.
    let home = app.get("/?theme_preview=1&tpl=terminal").await;
    assert!(html_tag(&home).contains("data-template=\"classic\""));

    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// /brand.css
// ---------------------------------------------------------------------------

#[tokio::test]
async fn brand_css_content_and_caching() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;
    let res = save(
        &mut app,
        &csrf,
        &[
            ("template", "terminal"),
            ("brand_color", "#f5d90a"),
            ("font_body", "dm-sans"),
            ("radius", "round"),
        ],
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    app.cookies.clear();

    let home = app.get("/").await;
    let href = link_href(&home.body, "/brand.css?v=").expect("brand.css link");
    let version = href.rsplit("v=").next().unwrap().to_owned();
    assert_eq!(version.len(), 12, "{href}");
    assert!(version.bytes().all(|b| b.is_ascii_hexdigit()));

    let sheet = app.get(&href).await;
    assert_eq!(sheet.status, StatusCode::OK);
    assert!(sheet.content_type().starts_with("text/css"));
    assert_eq!(
        header(&sheet, "cache-control"),
        "public, max-age=31536000, immutable"
    );
    let etag = header(&sheet, "etag").to_owned();
    assert_eq!(etag, format!("\"{version}\""));
    let css = &sheet.body;
    assert!(css.contains("--brand: #f5d90a;"), "{css}");
    // A pale brand gets dark ink (WCAG AA), computed on the server.
    assert!(css.contains("--brand-ink: #000000;"), "{css}");
    assert!(css.contains("--radius-lg:"), "{css}");
    // Only the selected families: DM Sans (body) + Space Grotesk (terminal
    // heading); nothing else from the library.
    assert!(css.contains("font-family: \"DM Sans\""));
    assert!(css.contains("font-family: \"Space Grotesk\""));
    for other in ["Inter\"", "Manrope", "Fraunces", "IBM Plex Sans", "Outfit"] {
        assert!(!css.contains(other), "{other} must not be declared");
    }
    // Metric-matched fallbacks keep the swap from shifting the layout.
    assert!(css.contains("\"DM Sans Fallback\""));
    assert!(css.contains("size-adjust:"));
    // No inline / remote content in the sheet.
    assert!(!css.contains("http://") && !css.contains("https://"));

    // Revalidation.
    let host = app.host.clone();
    let res = app
        .send_to(&host, Method::GET, &href, None, &[("if-none-match", &etag)])
        .await;
    assert_eq!(res.status, StatusCode::NOT_MODIFIED);
    assert!(res.body.is_empty());

    // Old or missing versions: same (current) CSS, short cache.
    for stale in ["/brand.css", "/brand.css?v=000000000000"] {
        let res = app.get(stale).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(header(&res, "cache-control"), "public, max-age=300");
        assert_eq!(res.body, *css);
    }

    // A change produces a new version (cache busting).
    let csrf = staff(&mut app, "owner").await;
    let res = save(
        &mut app,
        &csrf,
        &[("template", "terminal"), ("brand_color", "#1d4ed8")],
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let home = app.get("/").await;
    let next = link_href(&home.body, "/brand.css?v=").expect("brand.css link");
    assert_ne!(next, href);
    let sheet = app.get(&next).await;
    assert!(sheet.body.contains("--brand: #1d4ed8;"));
    assert!(sheet.body.contains("--brand-ink: #ffffff;"));

    app.cleanup().await;
}

#[tokio::test]
async fn brand_css_does_not_leak_between_tenants() {
    let Some(mut a) = TestApp::new().await else {
        return;
    };
    let Some(b) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut a, "owner").await;
    let res = save(
        &mut a,
        &csrf,
        &[
            ("template", "cloud"),
            ("brand_color", "#7c3aed"),
            ("font_heading", "fraunces"),
        ],
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    a.cookies.clear();

    // One router (one process-wide theme cache) serves both hosts.
    let host_a = a.host.clone();
    let host_b = b.host.clone();
    let page_a = a.send_to(&host_a, Method::GET, "/", None, &[]).await;
    let page_b = a.send_to(&host_b, Method::GET, "/", None, &[]).await;
    assert!(html_tag(&page_a).contains("data-template=\"cloud\""));
    assert!(html_tag(&page_b).contains("data-template=\"classic\""));
    assert!(link_href(&page_b.body, "css/themes/").is_none());
    let href_a = link_href(&page_a.body, "/brand.css?v=").expect("a css");
    let href_b = link_href(&page_b.body, "/brand.css?v=").expect("b css");
    assert_ne!(href_a, href_b, "versions are per tenant");

    let css_a = a.send_to(&host_a, Method::GET, &href_a, None, &[]).await;
    let css_b = a.send_to(&host_b, Method::GET, &href_b, None, &[]).await;
    assert!(css_a.body.contains("#7c3aed") && css_a.body.contains("Fraunces"));
    assert!(!css_b.body.contains("#7c3aed") && !css_b.body.contains("Fraunces"));

    // Tenant A's URL on tenant B's host: B's own CSS, never cached long.
    let cross = a.send_to(&host_b, Method::GET, &href_a, None, &[]).await;
    assert_eq!(cross.status, StatusCode::OK);
    assert_eq!(cross.body, css_b.body);
    assert_eq!(header(&cross, "cache-control"), "public, max-age=300");
    // …and the other way round, after B's sheet was served (and cached).
    let again = a.send_to(&host_a, Method::GET, &href_a, None, &[]).await;
    assert_eq!(again.body, css_a.body);

    a.cleanup().await;
    b.cleanup().await;
}

// ---------------------------------------------------------------------------
// staff preview
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_is_staff_only_and_never_cached() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let query = "/?theme_preview=1&tpl=terminal&brand=%23ff0066&accent=&fb=fraunces&fh=&r=round&m=dark&tg=0";

    // Anonymous: the parameters are ignored.
    let anon = app.get(query).await;
    assert_eq!(anon.status, StatusCode::OK);
    let tag = html_tag(&anon).to_owned();
    assert!(tag.contains("data-template=\"classic\""), "{tag}");
    assert!(!tag.contains("data-theme-preview"));
    assert!(!anon.body.contains("fraunces"));
    assert_eq!(header(&anon, "x-frame-options"), "DENY");
    assert!(header(&anon, "content-security-policy").contains("frame-ancestors 'none'"));
    let preview_css = "/brand.css?theme_preview=1&tpl=terminal&brand=%23ff0066&fb=fraunces";
    let sheet = app.get(preview_css).await;
    assert!(!sheet.body.contains("#ff0066"));
    assert_eq!(header(&sheet, "cache-control"), "public, max-age=300");

    // Staff with settings.manage: the unsaved values, private and unindexed.
    let csrf = staff(&mut app, "owner").await;
    let res = app.get(query).await;
    assert_eq!(res.status, StatusCode::OK);
    let tag = html_tag(&res).to_owned();
    assert!(tag.contains("data-template=\"terminal\""), "{tag}");
    assert!(tag.contains("data-theme=\"dark\""), "{tag}");
    assert!(tag.contains("data-mode-locked"), "{tag}");
    assert!(tag.contains("data-theme-preview"), "{tag}");
    assert!(res.body.contains("hero--terminal"));
    assert_eq!(header(&res, "cache-control"), "private, no-store");
    assert!(header(&res, "x-robots-tag").contains("noindex"));
    assert_eq!(header(&res, "x-frame-options"), "SAMEORIGIN");
    let csp = header(&res, "content-security-policy");
    assert!(csp.contains("frame-ancestors 'self'"), "{csp}");
    assert!(csp.contains("script-src 'self'") && csp.contains("style-src 'self'"));
    assert!(!csp.contains("unsafe-inline"), "{csp}");
    let href = link_href(&res.body, "/brand.css?").expect("preview css");
    assert!(href.contains("theme_preview=1"), "{href}");
    let sheet = app.get(&href).await;
    assert!(sheet.body.contains("--brand: #ff0066;"));
    assert!(sheet.body.contains("Fraunces"));
    assert_eq!(header(&sheet, "cache-control"), "private, no-store");

    // Nothing was saved, and the tenant's own pages are unaffected.
    assert!(appearance(&app).await.is_empty());
    let home = app.get("/").await;
    assert!(html_tag(&home).contains("data-template=\"classic\""));

    // The no-JS preview button: 303 to the storefront with the query.
    let res = app
        .post(
            &format!("{PAGE}/preview"),
            &[
                ("csrf_token", &csrf),
                ("template", "cloud"),
                ("brand_color", "#00aa55"),
                ("font_body", ""),
                ("font_heading", ""),
                ("radius", ""),
                ("mode", ""),
                ("preview_page", "/cart"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let to = res.location().to_owned();
    assert!(to.contains("/cart?theme_preview=1"), "{to}");
    assert!(to.contains("tpl=cloud") && to.contains("00aa55"), "{to}");
    // Only whitelisted pages.
    let res = app
        .post(
            &format!("{PAGE}/preview"),
            &[
                ("csrf_token", &csrf),
                ("template", "cloud"),
                ("preview_page", "https://evil.example/"),
            ],
        )
        .await;
    assert!(!res.location().contains("evil"), "{}", res.location());

    app.cleanup().await;
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_template_renders_customer_pages() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("auth.require_email_verification", json!(false))
        .await;
    catalog_fixture(&app).await;
    let hero = |key: &str| format!("<section class=\"hero hero--{key}\">");

    // Visitors: home, product group (pricing), product, domains, cart, login.
    for key in TEMPLATES {
        use_template(&app, key).await;
        for path in [
            "/",
            "/tema-paketleri",
            "/tema-paketleri/tema-pro",
            "/domains",
            "/cart",
            "/auth/login",
        ] {
            let res = app.get(path).await;
            assert_eq!(res.status, StatusCode::OK, "{key} {path}: {}", res.body);
            let tag = html_tag(&res).to_owned();
            assert!(
                tag.contains(&format!("data-template=\"{key}\"")),
                "{key} {path}: {tag}"
            );
            let sheet = link_href(&res.body, &format!("css/themes/{key}.css"));
            assert_eq!(sheet.is_some(), key != "classic", "{key} {path}");
            assert!(
                link_href(&res.body, "/brand.css?v=").is_some(),
                "{key} {path}"
            );
            // One page title per page whatever the template (SEO).
            assert_eq!(res.body.matches("<h1").count(), 1, "{key} {path}");
        }
        let home = app.get("/").await;
        assert!(home.body.contains(&hero(key)), "{key}: its own hero");
        assert!(home.body.contains("application/ld+json"), "{key}: JSON-LD");
        assert_eq!(
            home.body.contains("utility-bar"),
            key == "corporate",
            "{key}: utility bar"
        );
        let pricing = app.get("/tema-paketleri").await;
        assert!(pricing.body.contains("Tema Pro"), "{key}: plan card");
    }

    // A client: the dashboard in every template.
    let token = app.get("/auth/register").await.csrf();
    let res = app
        .post(
            "/auth/register",
            &[
                ("csrf_token", &token),
                ("first_name", "Tema"),
                ("last_name", "Müşteri"),
                (
                    "email",
                    &format!(
                        "tema-{}@client.test",
                        &uuid::Uuid::now_v7().simple().to_string()[16..]
                    ),
                ),
                ("password", PASSWORD),
                ("terms", "1"),
                ("next", "/client"),
                ("website", ""),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "register: {}", res.body);
    for key in TEMPLATES {
        use_template(&app, key).await;
        let res = app.get("/client").await;
        assert_eq!(res.status, StatusCode::OK, "{key} /client: {}", res.body);
        assert!(
            html_tag(&res).contains(&format!("data-template=\"{key}\"")),
            "{key} /client"
        );
    }

    app.cleanup().await;
}

/// Phone and desktop layout defects: the client area's section tabs wrap at
/// phone widths instead of a row that scrolls sideways, and the corporate
/// home art is scaled to fit its hero instead of being cropped. (Measured in
/// a browser at 375 px and 1024–1920 px; this pins the served markup/CSS.)
#[tokio::test]
async fn client_tabs_wrap_on_phones_and_corporate_art_is_not_cropped() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    app.setting("auth.require_email_verification", json!(false))
        .await;
    use_template(&app, "corporate").await;
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    let start = home.body.find("hero-art--network").expect("network art");
    let svg_tag = &home.body[start..start + home.body[start..].find('>').unwrap()];
    assert!(
        svg_tag.contains("preserveAspectRatio=\"xMidYMid meet\""),
        "{svg_tag}"
    );

    let token = app.get("/auth/register").await.csrf();
    let res = app
        .post(
            "/auth/register",
            &[
                ("csrf_token", &token),
                ("first_name", "Sekme"),
                ("last_name", "Müşteri"),
                (
                    "email",
                    &format!(
                        "sekme-{}@client.test",
                        &uuid::Uuid::now_v7().simple().to_string()[16..]
                    ),
                ),
                ("password", PASSWORD),
                ("terms", "1"),
                ("next", "/client"),
                ("website", ""),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "register: {}", res.body);
    let account = app.get("/client/account").await;
    assert_eq!(account.status, StatusCode::OK);
    assert!(account.body.contains("<nav class=\"tabs\""));
    let sheet = link_href(&account.body, "css/client.css").expect("client.css");
    let css = app.get(&sheet).await;
    assert_eq!(css.status, StatusCode::OK);
    let phone = css
        .body
        .split("@media (max-width: 47.99rem)")
        .skip(1)
        .find(|block| {
            let block = block.split("@media").next().unwrap_or_default();
            block.contains(".panel-content .tabs")
                && block.contains("flex-wrap: wrap")
                && block.contains("overflow-x: visible")
        });
    assert!(phone.is_some(), "client.css wraps the tabs on phones");
    app.cleanup().await;
}
