//! Storefront SEO glue on top of `hos-seo`.
//!
//! A storefront handler describes its page with [`SeoPage`] and calls
//! [`apply`]: that renders `<head>` metadata (title template, description,
//! canonical, hreflang tr/en + x-default, Open Graph, robots) into
//! `ctx.seo_head` and a JSON-LD `@graph` (`Organization` + `WebSite` on every
//! page, plus the page's own nodes and a `BreadcrumbList`) into `ctx.jsonld`.
//! Hand-written values from the `seo_meta` table override generated ones.

use hos_core::domain::Locale;
use hos_core::integrations::PostalAddress;
use hos_core::tenant::TenantContext;
use hos_db::repo::{platform, seo as seo_repo};
use hos_seo::jsonld::{
    BreadcrumbList, ContactPoint, Crumb as LdCrumb, Graph, Organization, OrganizationKind, Schema,
    WebSite,
};
use hos_seo::meta::{
    Alternate, ArticleMeta, OgImage, OgType, OpenGraph, PageMeta, RobotsMeta, TitleTemplate,
    description_from_text, og_locale,
};
use serde_json::Value;

pub use hos_db::repo::seo::SeoTarget;

use crate::error::AppResult;
use crate::locale::localize_path;
use crate::view::PageCtx;

/// Description of one storefront page.
#[derive(Debug, Clone)]
pub struct SeoPage {
    /// Page title (without the site suffix).
    pub title: String,
    /// Meta description (plain text; truncated to 160 graphemes).
    pub description: Option<String>,
    /// Unprefixed path of this page in the current locale.
    pub path: String,
    /// Per-locale paths when slugs differ (content translations); `None` =
    /// same path in every locale; `Some(vec![])` = no alternates.
    pub alternates: Option<Vec<(Locale, String)>>,
    /// `og:type`.
    pub og_type: OgType,
    /// Absolute image URL.
    pub image: Option<String>,
    /// `article:*` tags.
    pub article: Option<ArticleMeta>,
    /// `noindex`.
    pub noindex: bool,
    /// Key for `seo_meta` overrides.
    pub target: SeoTarget,
    /// Page-specific JSON-LD nodes (without `@context`).
    pub jsonld: Vec<Value>,
    /// Breadcrumbs `(name, unprefixed path)`; the home crumb is added automatically.
    pub breadcrumbs: Vec<(String, String)>,
    /// The home page (title pattern `Site — Page`).
    pub home: bool,
}

impl SeoPage {
    /// A page at `path` with `title`.
    pub fn new(title: impl Into<String>, path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            title: title.into(),
            description: None,
            target: SeoTarget::Route(path.clone()),
            path,
            alternates: None,
            og_type: OgType::Website,
            image: None,
            article: None,
            noindex: false,
            jsonld: Vec::new(),
            breadcrumbs: Vec::new(),
            home: false,
        }
    }

    /// Sets the description.
    #[must_use]
    pub fn description(mut self, d: impl Into<String>) -> Self {
        let d = d.into();
        if !d.trim().is_empty() {
            self.description = Some(d);
        }
        self
    }

    /// Sets the `seo_meta` target.
    #[must_use]
    pub fn target(mut self, t: SeoTarget) -> Self {
        self.target = t;
        self
    }

    /// Adds a JSON-LD node.
    #[must_use]
    pub fn node(mut self, schema: &dyn Schema) -> Self {
        self.jsonld.push(schema.to_value());
        self
    }

    /// Adds a breadcrumb.
    #[must_use]
    pub fn crumb(mut self, name: impl Into<String>, path: impl Into<String>) -> Self {
        self.breadcrumbs.push((name.into(), path.into()));
        self
    }
}

/// Text of a localized setting value (`{"tr": "...", "en": "..."}` or a plain string).
pub fn localized_setting(v: &Value, locale: Locale) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m
            .get(locale.as_str())
            .or_else(|| m.get("tr"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// The tenant's `Organization` node (shared by every page).
pub async fn organization(ctx: &PageCtx, tenant: &TenantContext) -> AppResult<Organization> {
    let row = platform::get_tenant(&ctx.state.pool, tenant.id).await?;
    let origin = &ctx.origin;
    let mut org = Organization::new(tenant.display_name(), format!("{origin}/"));
    org.kind = OrganizationKind::Organization;
    org.id = Some(format!("{origin}/#organization"));
    org.logo = Some(
        tenant
            .branding
            .logo_url
            .clone()
            .unwrap_or_else(|| format!("{origin}{}", ctx.asset("img/logo.svg"))),
    );
    if let Some(r) = row {
        org.legal_name = r.legal_name.clone();
        org.email = r.contact_email.clone();
        org.telephone = r.contact_phone.clone();
        org.tax_id = r.tax_number.clone();
        if let Ok(addr) = serde_json::from_value::<PostalAddress>(r.address.clone())
            && !addr.line1.is_empty()
        {
            org.address = Some(addr);
        }
    }
    if let Some(support) = tenant.branding.support_email.clone() {
        org.contact_points.push(ContactPoint {
            contact_type: "customer support".into(),
            email: Some(support),
            url: Some(format!("{origin}{}", ctx.url("/contact"))),
            available_language: vec!["tr".into(), "en".into()],
            ..ContactPoint::default()
        });
    }
    Ok(org)
}

/// Renders `<head>` metadata and JSON-LD into `ctx` (see module docs).
pub async fn apply(ctx: &mut PageCtx, page: SeoPage) -> AppResult<()> {
    let Some(tenant) = ctx.tenant.clone() else {
        return Ok(());
    };
    let pool = &ctx.state.pool;
    let over = seo_repo::get_seo_meta(pool, tenant.id, &page.target, ctx.locale).await?;
    let settings = platform::get_settings(pool, tenant.id, "seo.").await?;
    let site = tenant.display_name().to_owned();

    let page_title = over
        .as_ref()
        .and_then(|o| o.title.clone())
        .unwrap_or(page.title.clone());
    let title = if over.as_ref().and_then(|o| o.title.as_ref()).is_some() {
        page_title.clone()
    } else if page.home {
        TitleTemplate::new("{site} — {page}").render(&page_title, &site)
    } else {
        let suffix = settings
            .get("seo.title_suffix")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!(" | {site}"));
        TitleTemplate::new(format!("{{page}}{}", suffix.replace(['{', '}'], "")))
            .render(&page_title, &site)
    };
    let description = over
        .as_ref()
        .and_then(|o| o.description.clone())
        .or(page.description.clone())
        .or_else(|| {
            settings
                .get("seo.default_description")
                .and_then(|v| localized_setting(v, ctx.locale))
        })
        .map(|d| description_from_text(&d, hos_seo::meta::DESCRIPTION_MAX));

    let origin = ctx.origin.clone();
    let abs = |locale: Locale, path: &str| {
        format!(
            "{origin}{}",
            localize_path(path, locale, ctx.default_locale)
        )
    };
    let canonical = over
        .as_ref()
        .and_then(|o| o.canonical.clone())
        .unwrap_or_else(|| abs(ctx.locale, &page.path));
    let alternates: Vec<(Locale, String)> = match &page.alternates {
        Some(list) => list.clone(),
        None => Locale::ALL
            .iter()
            .map(|l| (*l, page.path.clone()))
            .collect(),
    };
    ctx.lang_links = Locale::ALL
        .iter()
        .map(|l| {
            let path = alternates
                .iter()
                .find(|(al, _)| al == l)
                .map_or("/", |(_, p)| p.as_str());
            (*l, localize_path(path, *l, ctx.default_locale))
        })
        .collect();
    let mut meta = PageMeta::new(title);
    meta.description = description.clone();
    meta.canonical = Some(canonical.clone());
    meta.locale = Some(ctx.locale.as_str().to_owned());
    if alternates.len() > 1 {
        meta.alternates = alternates
            .iter()
            .map(|(l, p)| Alternate::new(l.as_str(), abs(*l, p)))
            .collect();
        meta.x_default = alternates
            .iter()
            .find(|(l, _)| *l == ctx.default_locale)
            .map(|(l, p)| abs(*l, p));
    }
    let noindex = page.noindex || over.as_ref().is_some_and(|o| o.noindex);
    let nofollow = over.as_ref().is_some_and(|o| o.nofollow);
    if noindex || nofollow {
        meta.robots = RobotsMeta {
            index: !noindex,
            follow: !nofollow,
            ..RobotsMeta::default()
        };
    }
    // The generated share image (brand colours, template composition;
    // routes::storefront::og) unless the page brings its own.
    let image = page
        .image
        .clone()
        .or_else(|| Some(ctx.theme.share_image_url(&origin, &page.path)));
    meta.og = OpenGraph {
        kind: page.og_type,
        title: Some(page_title.clone()),
        description: description.clone(),
        url: Some(canonical.clone()),
        site_name: Some(site.clone()),
        image: image.map(|url| OgImage {
            url,
            width: Some(1200),
            height: Some(630),
            alt: Some(site.clone()),
            mime: None,
        }),
    };
    if let Some(o) = over
        .as_ref()
        .and_then(|o| serde_json::from_value::<OpenGraph>(o.og.clone()).ok())
    {
        if o.title.is_some() {
            meta.og.title = o.title;
        }
        if o.description.is_some() {
            meta.og.description = o.description;
        }
        if o.image.is_some() {
            meta.og.image = o.image;
        }
    }
    meta.article = page.article.clone();
    let mut head = meta.render_head();
    if let Some(l) = og_locale(ctx.locale.as_str())
        && !head.contains("og:locale")
    {
        head.push_str(&format!(
            "<meta property=\"og:locale\" content=\"{}\">\n",
            crate::view::escape_html(&l)
        ));
    }
    ctx.seo_head = head;

    // JSON-LD
    let org = organization(ctx, &tenant).await?;
    let website = WebSite {
        id: Some(format!("{origin}/#website")),
        name: site.clone(),
        url: format!("{origin}{}", ctx.url("/")),
        description: settings
            .get("seo.default_description")
            .and_then(|v| localized_setting(v, ctx.locale)),
        in_language: vec![ctx.locale.as_str().to_owned()],
        publisher_id: org.id.clone(),
        search_url_template: Some(format!(
            "{origin}{}?q={{search_term_string}}",
            ctx.url("/kb/search")
        )),
        ..WebSite::default()
    };
    let mut graph = Graph::new().with(&org).with(&website);
    for node in page.jsonld {
        graph.push_value(node);
    }
    if !page.home {
        let mut items = vec![LdCrumb::new(ctx.t("common.nav.home"), abs(ctx.locale, "/"))];
        for (name, path) in &page.breadcrumbs {
            items.push(LdCrumb::new(name.clone(), abs(ctx.locale, path)));
        }
        if page.breadcrumbs.last().is_none_or(|(_, p)| *p != page.path) {
            items.push(LdCrumb::new(page_title.clone(), canonical.clone()));
        }
        graph.push(&BreadcrumbList { items });
    }
    if let Some(custom) = over.as_ref().and_then(|o| o.jsonld.clone()) {
        graph.push_value(custom);
    }
    ctx.jsonld = graph.to_script();
    Ok(())
}
