//! Storefront content, SEO and GEO integration tests: the default page set a
//! new tenant gets, the structure of every key page in every design
//! template (one `<h1>`, canonical, hreflang pairs, JSON-LD that parses and
//! carries the required fields, no duplicate ids, FAQ markup only for
//! visible questions), the sitemap / robots / llms outputs, the Markdown
//! twins and the generated share image.
//!
//! Like the other web tests they need the database from `.env`; without it
//! they print a notice and pass. Every test works on its own throw-away
//! tenant (never the demo tenant).

use std::collections::BTreeSet;

use crate::common::{Res, TestApp};
use hos_core::domain::ProductType;
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::catalog;
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::{Value, json};

const TEMPLATES: [&str; 6] = [
    "classic",
    "cloud",
    "corporate",
    "terminal",
    "studio",
    "editorial",
];

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// The `web-hosting` group (same slug as the seeded landing page) with two
/// priced plans, and the landing page published so the group page merges
/// its blocks.
async fn catalog_fixture(app: &TestApp) {
    let group = catalog::create_product_group(
        app.pool(),
        app.tenant_id,
        &catalog::ProductGroupInput {
            slug: "web-hosting".into(),
            name: "Web Hosting".into(),
            tagline: Some("Paylaşımlı hosting paketleri".into()),
            description: None,
            translations: json!({"en": {"name": "Web Hosting", "tagline": "Shared hosting plans"}}),
            icon: Some("globe".into()),
            is_hidden: false,
            sort_order: 0,
        },
    )
    .await
    .expect("group");
    let currency = catalog::list_currencies(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.is_default)
        .map_or(Currency::TRY, |c| c.code);
    for (slug, name, monthly, features) in [
        (
            "hosting-start",
            "Başlangıç",
            60,
            json!(["5 GB NVMe disk", "1 web sitesi"]),
        ),
        (
            "hosting-pro",
            "Pro",
            120,
            json!(["20 GB NVMe disk", "10 web sitesi"]),
        ),
    ] {
        let mut input =
            catalog::ProductInput::new(slug, name, ProductType::SharedHosting, Some(group.id));
        input.features = features;
        input.is_featured = slug == "hosting-pro";
        let product = catalog::create_product(app.pool(), app.tenant_id, &input)
            .await
            .expect("product");
        for (cycle, price) in [
            (BillingCycle::Monthly, monthly),
            (BillingCycle::Annual, monthly * 10),
        ] {
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
    sqlx::query(
        "update pages set status = 'published', published_at = now()
          where tenant_id = $1 and slug = 'web-hosting'",
    )
    .bind(app.tenant_id)
    .execute(app.pool())
    .await
    .expect("publish landing");
}

async fn use_template(app: &TestApp, key: &str) {
    app.setting("appearance.template", json!(key)).await;
    hos_web::theme::invalidate(app.tenant_id);
}

// ---------------------------------------------------------------------------
// structural checks
// ---------------------------------------------------------------------------

/// Every JSON-LD node, `@graph` flattened.
fn nodes(res: &Res) -> Vec<Value> {
    let mut out = Vec::new();
    for doc in res.json_ld() {
        match doc.get("@graph").and_then(Value::as_array) {
            Some(graph) => out.extend(graph.iter().cloned()),
            None => out.push(doc),
        }
    }
    out
}

fn types(node: &Value) -> Vec<String> {
    match node.get("@type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn of_type<'a>(nodes: &'a [Value], kind: &str) -> Vec<&'a Value> {
    nodes
        .iter()
        .filter(|n| types(n).iter().any(|t| t == kind))
        .collect()
}

fn text(node: &Value, key: &str) -> String {
    node.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Askama's HTML escaping, enough to find a question in the markup.
fn escaped(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// `attr="value"` occurrences of `attr` in the document.
fn attr_values<'a>(body: &'a str, attr: &str) -> Vec<&'a str> {
    let marker = format!(" {attr}=\"");
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(&marker) {
        rest = &rest[i + marker.len()..];
        if let Some(end) = rest.find('"') {
            out.push(&rest[..end]);
            rest = &rest[end..];
        }
    }
    out
}

/// `hreflang → href` of the `<link rel="alternate">` tags in `<head>`.
fn hreflangs(body: &str) -> Vec<(String, String)> {
    let head = &body[..body.find("</head>").unwrap_or(body.len())];
    head.split("<link ")
        .skip(1)
        .filter(|tag| tag.contains("rel=\"alternate\"") && tag.contains("hreflang="))
        .filter_map(|tag| {
            let lang = attr_values(&format!(" {tag}"), "hreflang")
                .first()?
                .to_string();
            let href = attr_values(&format!(" {tag}"), "href").first()?.to_string();
            Some((lang, href))
        })
        .collect()
}

/// The checks every indexable storefront page must pass. `pair`: the
/// other-locale path that must be announced with hreflang.
fn assert_sound(res: &Res, label: &str, pair: Option<&str>) {
    assert_eq!(res.status, StatusCode::OK, "{label}: {}", res.body);
    let body = &res.body;
    assert_eq!(body.matches("<h1").count(), 1, "{label}: exactly one <h1>");
    assert_eq!(body.matches("<main").count(), 1, "{label}: one <main>");
    assert!(body.contains("class=\"skip-link\""), "{label}: skip link");

    // Canonical: absolute, one.
    let head = &body[..body.find("</head>").expect("</head>")];
    let canonicals: Vec<&str> = head
        .split("<link ")
        .filter(|t| t.starts_with("rel=\"canonical\""))
        .collect();
    assert_eq!(canonicals.len(), 1, "{label}: one canonical");
    assert!(
        canonicals[0].contains("href=\"http"),
        "{label}: absolute canonical {}",
        canonicals[0]
    );
    assert!(!head.contains("noindex"), "{label}: indexable");

    // hreflang: tr + en + x-default, the pair pointing at the translation.
    let langs = hreflangs(body);
    let has = |l: &str| langs.iter().any(|(lang, _)| lang == l);
    if let Some(pair) = pair {
        assert!(
            has("tr") && has("en") && has("x-default"),
            "{label}: {langs:?}"
        );
        assert!(
            langs.iter().any(|(_, href)| href.ends_with(pair)),
            "{label}: hreflang to {pair}: {langs:?}"
        );
        let x_default = langs.iter().find(|(l, _)| l == "x-default").unwrap();
        let tr = langs.iter().find(|(l, _)| l == "tr").unwrap();
        assert_eq!(
            x_default.1, tr.1,
            "{label}: x-default is the default locale"
        );
    }

    // No duplicate ids (fragment links, aria references, label targets).
    let mut seen = BTreeSet::new();
    for id in attr_values(body, "id") {
        assert!(seen.insert(id), "{label}: duplicate id {id:?}");
    }
    // Every aria-labelledby / label[for] target exists.
    for target in attr_values(body, "aria-labelledby")
        .into_iter()
        .chain(attr_values(body, "for"))
        .flat_map(|v| v.split_whitespace())
    {
        assert!(
            seen.contains(target),
            "{label}: reference to missing id {target:?}"
        );
    }
    // Images carry dimensions (CLS) and alt text.
    for tag in body.split("<img ").skip(1) {
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        assert!(
            tag.contains("width=") && tag.contains("height="),
            "{label}: <img {tag}>"
        );
        assert!(tag.contains("alt="), "{label}: <img {tag}> alt");
    }

    // JSON-LD: parses (json_ld() panics otherwise), required fields.
    let nodes = nodes(res);
    assert!(!nodes.is_empty(), "{label}: JSON-LD");
    for n in &nodes {
        assert!(!types(n).is_empty(), "{label}: node without @type: {n}");
    }
    for org in of_type(&nodes, "Organization") {
        assert!(!text(org, "name").is_empty(), "{label}: Organization.name");
        assert!(
            text(org, "url").starts_with("http"),
            "{label}: Organization.url"
        );
    }
    for list in of_type(&nodes, "BreadcrumbList") {
        let items = list["itemListElement"].as_array().expect("itemListElement");
        assert!(!items.is_empty(), "{label}: breadcrumb items");
        for (i, item) in items.iter().enumerate() {
            assert_eq!(
                item["position"],
                json!(i + 1),
                "{label}: breadcrumb position"
            );
            assert!(!text(item, "name").is_empty(), "{label}: breadcrumb name");
        }
    }
    for product in of_type(&nodes, "Product") {
        assert!(!text(product, "name").is_empty(), "{label}: Product.name");
        let offers = &product["offers"];
        assert!(
            offers.is_object() || offers.is_array(),
            "{label}: Product.offers"
        );
        let first = if offers.is_array() {
            &offers[0]
        } else {
            offers
        };
        assert!(
            first.get("price").is_some() || first.get("lowPrice").is_some(),
            "{label}: offer price {first}"
        );
        assert!(
            !text(first, "priceCurrency").is_empty(),
            "{label}: priceCurrency"
        );
    }
    for faq in of_type(&nodes, "FAQPage") {
        let questions = faq["mainEntity"].as_array().expect("mainEntity");
        assert!(!questions.is_empty(), "{label}: FAQ entries");
        for q in questions {
            let name = text(q, "name");
            assert!(!name.is_empty(), "{label}: Question.name");
            assert!(
                !text(&q["acceptedAnswer"], "text").is_empty(),
                "{label}: acceptedAnswer.text"
            );
            // FAQ markup only for questions the visitor can read.
            assert!(
                body.contains(&escaped(&name)) || body.contains(&name),
                "{label}: FAQ question not visible: {name}"
            );
        }
    }
    for kind in ["Article", "BlogPosting", "TechArticle"] {
        for a in of_type(&nodes, kind) {
            assert!(!text(a, "headline").is_empty(), "{label}: {kind}.headline");
            assert!(a.get("author").is_some(), "{label}: {kind}.author");
            assert!(a.get("publisher").is_some(), "{label}: {kind}.publisher");
            assert!(
                a.get("dateModified").is_some() || a.get("datePublished").is_some(),
                "{label}: {kind} date"
            );
        }
    }
    // Ratings only from real review data, which these tenants do not have.
    assert!(
        of_type(&nodes, "AggregateRating").is_empty() && !body.contains("aggregateRating"),
        "{label}: no invented ratings"
    );
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_tenants_get_the_default_page_set() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "select locale, slug, template, status::text from pages where tenant_id = $1",
    )
    .bind(app.tenant_id)
    .fetch_all(app.pool())
    .await
    .expect("pages");
    let find = |locale: &str, slug: &str| {
        rows.iter()
            .find(|(l, s, _, _)| l == locale && s == slug)
            .unwrap_or_else(|| panic!("{locale}/{slug} seeded: {rows:?}"))
    };
    // Legal templates and company pages: published, both locales.
    for (tr, en) in [
        ("kullanim-kosullari", "terms-of-service"),
        ("gizlilik-politikasi", "privacy-policy"),
        ("iade-politikasi", "refund-policy"),
        ("hizmet-seviyesi-sozlesmesi", "service-level-agreement"),
    ] {
        assert_eq!(find("tr", tr).2, "legal");
        assert_eq!(find("tr", tr).3, "published");
        assert_eq!(find("en", en).3, "published");
    }
    for (tr, en) in [
        ("hakkimizda", "about-us"),
        ("hosting-turleri", "hosting-types-compared"),
    ] {
        assert_eq!(find("tr", tr).3, "published");
        assert_eq!(find("en", en).3, "published");
    }
    // Category landings wait for the tenant's own catalogue: drafts.
    for slug in [
        "web-hosting",
        "vps",
        "ssl",
        "email-hosting",
        "reseller-hosting",
    ] {
        assert_eq!(find("tr", slug).3, "draft", "{slug}");
        assert_eq!(find("en", slug).3, "draft", "{slug}");
    }
    assert_eq!(rows.len() % 2, 0, "every page has its translation");

    // Seeding again is a no-op (keyed on tenant + locale + slug).
    let mut conn = app.pool().acquire().await.unwrap();
    hos_db::seed::seed_tenant_defaults(&mut conn, app.tenant_id)
        .await
        .expect("reseed");
    drop(conn);
    let again: i64 = sqlx::query_scalar("select count(*) from pages where tenant_id = $1")
        .bind(app.tenant_id)
        .fetch_one(app.pool())
        .await
        .unwrap();
    assert_eq!(again as usize, rows.len());

    // Visible: legal pages render with their review note hidden from
    // visitors; draft landings do not exist publicly.
    let terms = app.get("/kullanim-kosullari").await;
    assert_sound(&terms, "terms", Some("/en/terms-of-service"));
    assert!(!terms.body.contains("callout--todo"), "staff note hidden");
    assert!(!terms.body.contains("{{"), "tokens replaced");
    assert_eq!(app.get("/web-hosting").await.status, StatusCode::NOT_FOUND);
    // Pages merged into routes are never served twice.
    let home = app.get("/home").await;
    assert_eq!(home.status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(home.location(), "/");
    let en_home = app.get("/en/home").await;
    assert_eq!(en_home.status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(en_home.location(), "/en/");
    app.cleanup().await;
}

#[tokio::test]
async fn every_template_key_page_is_structurally_sound() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    catalog_fixture(&app).await;
    let pages: [(&str, Option<&str>); 11] = [
        ("/", Some("/en/")),
        ("/en/", Some("/")),
        ("/web-hosting", Some("/en/web-hosting")),
        (
            "/web-hosting/hosting-pro",
            Some("/en/web-hosting/hosting-pro"),
        ),
        ("/domains", Some("/en/domains")),
        ("/contact", Some("/en/contact")),
        ("/kullanim-kosullari", Some("/en/terms-of-service")),
        ("/hakkimizda", Some("/en/about-us")),
        ("/hosting-turleri", Some("/en/hosting-types-compared")),
        ("/kb", Some("/en/kb")),
        ("/blog", Some("/en/blog")),
    ];
    for key in TEMPLATES {
        use_template(&app, key).await;
        for (path, pair) in pages {
            let res = app.get(path).await;
            assert!(
                res.body.contains(&format!("data-template=\"{key}\"")),
                "{key} {path}"
            );
            assert_sound(&res, &format!("{key} {path}"), pair);
        }
    }

    // Page-specific content (template-independent).
    let home = app.get("/").await;
    let n = nodes(&home);
    let site = of_type(&n, "WebSite");
    assert_eq!(site.len(), 1, "WebSite node");
    assert!(
        site[0]["potentialAction"]["target"]
            .to_string()
            .contains("search_term_string"),
        "SearchAction"
    );
    let title = home.body
        [home.body.find("<title>").unwrap() + 7..home.body.find("</title>").unwrap()]
        .to_owned();
    assert!(title.chars().count() <= 70, "home title fits: {title}");

    // The group page merges its landing: definition lead, FAQ block with
    // matching FAQPage markup, the plans, a real comparison table.
    let group = app.get("/web-hosting").await;
    assert!(
        group.body.contains("barındıran hizmettir"),
        "definition lead"
    );
    assert!(group.body.contains("Kaç web sitesi barındırabilirim?"));
    assert_eq!(of_type(&nodes(&group), "FAQPage").len(), 1);
    assert!(group.body.contains("<table class=\"table table--compare\""));
    assert!(
        group.body.contains("<time datetime="),
        "visible last-updated date"
    );
    assert!(
        group.body.contains("type=\"text/markdown\""),
        "markdown alternate"
    );
    assert!(!group.body.contains("{{"), "tokens replaced");

    // The price note follows `billing.prices_include_tax`.
    assert!(group.body.contains("KDV dahil değildir"), "net prices");
    app.setting("billing.prices_include_tax", json!(true)).await;
    app.state.store_chrome.invalidate_all();
    let gross = app.get("/web-hosting").await;
    assert!(
        gross.body.contains("Fiyatlara KDV dahildir."),
        "gross prices"
    );
    assert!(!gross.body.contains("KDV dahil değildir"));

    // Product: offers per cycle, the price table, a data-derived FAQ.
    let product = app.get("/web-hosting/hosting-pro").await;
    let product_nodes = nodes(&product);
    let offers = &of_type(&product_nodes, "Product")[0]["offers"];
    assert!(offers.to_string().contains("120"), "{offers}");
    assert!(product.body.contains("<table"), "price table");
    assert_eq!(of_type(&nodes(&product), "FAQPage").len(), 1);

    // A draft landing must not leak into the merged group page in English
    // either; the English group renders the English landing.
    let en_group = app.get("/en/web-hosting").await;
    assert_sound(&en_group, "en group", Some("/web-hosting"));
    assert!(en_group.body.contains("lang=\"en\""));

    // Markdown twins.
    for path in [
        "/web-hosting.md",
        "/web-hosting/hosting-pro.md",
        "/hakkimizda.md",
    ] {
        let md = app.get(path).await;
        assert_eq!(md.status, StatusCode::OK, "{path}");
        assert!(md.content_type().starts_with("text/markdown"), "{path}");
        assert!(
            md.body.starts_with("# "),
            "{path}: {}",
            &md.body[..md.body.len().min(80)]
        );
    }
    let md = app.get("/web-hosting.md").await;
    assert!(
        md.body.contains("Pro") && md.body.contains("| "),
        "{}",
        md.body
    );

    app.cleanup().await;
}

#[tokio::test]
async fn sitemap_robots_llms_and_share_image() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    catalog_fixture(&app).await;

    let sitemap = app.get("/sitemap.xml").await;
    assert_eq!(sitemap.status, StatusCode::OK);
    let xml = &sitemap.body;
    assert!(xml.contains("<lastmod>"), "lastmod");
    assert!(xml.contains("hreflang=\"x-default\""), "x-default");
    assert!(xml.contains("<image:image>"), "share images");
    let locs: Vec<&str> = xml
        .split("<loc>")
        .skip(1)
        .map(|s| &s[..s.find("</loc>").unwrap()])
        .collect();
    let unique: BTreeSet<&str> = locs.iter().copied().collect();
    let pages: Vec<&&str> = locs.iter().filter(|l| !l.contains("/og/")).collect();
    assert_eq!(
        pages.len(),
        unique.iter().filter(|l| !l.contains("/og/")).count(),
        "no duplicate URLs"
    );
    for l in &pages {
        assert!(
            !l.ends_with("/home") && !l.ends_with("/en/home"),
            "merged page listed: {l}"
        );
        assert!(!l.ends_with("/domains-page"), "{l}");
    }
    assert!(pages.iter().any(|l| l.ends_with("/web-hosting")), "group");
    assert!(
        pages
            .iter()
            .any(|l| l.ends_with("/web-hosting/hosting-pro")),
        "product"
    );
    assert!(
        pages.iter().any(|l| l.ends_with("/kullanim-kosullari")),
        "legal"
    );
    assert!(
        !pages.iter().any(|l| l.ends_with("/vps")),
        "draft landing without a group is not listed"
    );

    let robots = app.get("/robots.txt").await;
    assert_eq!(robots.status, StatusCode::OK);
    assert!(robots.body.contains("Sitemap: "));
    assert!(robots.body.contains("Disallow: /admin/"));

    let llms = app.get("/llms.txt").await;
    assert_eq!(llms.status, StatusCode::OK);
    assert!(llms.body.starts_with("# "));
    assert!(llms.body.contains("/web-hosting.md"), "group as Markdown");
    assert!(
        llms.body.contains("/web-hosting/hosting-pro.md"),
        "product as Markdown"
    );
    assert!(
        llms.body.contains("/kullanim-kosullari.md"),
        "legal pages listed"
    );
    assert!(
        !llms.body.contains("/home.md"),
        "merged page not listed twice"
    );
    let full = app.get("/llms-full.txt").await;
    assert_eq!(full.status, StatusCode::OK);
    assert!(full.body.contains("Pro"), "plans in the full text");
    assert!(full.body.contains("| "), "prices as tables");
    assert!(
        !full.body.contains(":::"),
        "blocks rendered, not raw directives"
    );
    assert!(!full.body.contains("{{"), "tokens replaced");

    // The share image: referenced by og:image, a real PNG, cacheable.
    let home = app.get("/").await;
    let og = home
        .body
        .split("property=\"og:image\" content=\"")
        .nth(1)
        .map(|s| &s[..s.find('"').unwrap()])
        .expect("og:image")
        .replace("&amp;", "&");
    let path = &og[og.find("/og/").expect("generated share image")..];
    let png = app.get(path).await;
    assert_eq!(png.status, StatusCode::OK, "{path}");
    assert_eq!(png.content_type(), "image/png");
    assert!(
        png.headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .contains("immutable"),
        "versioned share image is immutable"
    );
    assert!(home.body.contains("summary_large_image"), "twitter card");
    assert_eq!(
        app.get("/og/..%2Fsecret.png").await.status,
        StatusCode::NOT_FOUND
    );

    app.cleanup().await;
}

/// `GET path` on the tenant host from client address `ip`; the status.
async fn get_from(app: &TestApp, path: &str, ip: &str) -> StatusCode {
    use tower::ServiceExt as _;
    let mut request = http::Request::builder()
        .uri(path)
        .header("host", &app.host)
        .body(axum::body::Body::empty())
        .expect("request");
    let addr: std::net::SocketAddr = format!("{ip}:40000").parse().expect("ip");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(addr));
    app.router
        .clone()
        .oneshot(request)
        .await
        .expect("infallible")
        .status()
}

/// Share images exist only for real pages (fixed routes and published,
/// visible content); made-up keys are `404` without rendering, and the
/// endpoint is rate-limited per client IP.
#[tokio::test]
async fn share_images_only_for_real_pages() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    catalog_fixture(&app).await;
    sqlx::query(
        "update products set is_hidden = true where tenant_id = $1 and slug = 'hosting-start'",
    )
    .bind(app.tenant_id)
    .execute(app.pool())
    .await
    .unwrap();
    let og = |path: &str| format!("/og/{}.png", hos_web::theme::share_image_key(path));
    for path in [
        "/",
        "/blog",
        "/kb/search",
        "/web-hosting",
        "/web-hosting/hosting-pro",
    ] {
        assert_eq!(app.get(&og(path)).await.status, StatusCode::OK, "{path}");
    }
    // The link a real page carries resolves.
    let product = app.get("/web-hosting/hosting-pro").await;
    let link = product
        .body
        .split("property=\"og:image\" content=\"")
        .nth(1)
        .map(|s| s[..s.find('"').unwrap()].replace("&amp;", "&"))
        .expect("og:image");
    let link = &link[link.find("/og/").unwrap()..];
    assert_eq!(app.get(link).await.status, StatusCode::OK, "{link}");
    for path in [
        "/made-up-page",
        "/web-hosting/hosting-start",
        "/blog/never-written",
        &format!("/status/incidents/{}", uuid::Uuid::now_v7()),
    ] {
        assert_eq!(
            app.get(&og(path)).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }

    // Per-IP limit: one address runs out, another is unaffected.
    let mut limited = false;
    let started = std::time::Instant::now();
    let mut sent = 0;
    while sent < 400 {
        sent += 1;
        match get_from(&app, "/og/not.a.key.png", "198.51.100.7").await {
            StatusCode::NOT_FOUND => {}
            StatusCode::TOO_MANY_REQUESTS => {
                limited = true;
                break;
            }
            other => panic!("unexpected {other}"),
        }
    }
    assert!(
        limited,
        "share images are rate-limited ({sent} requests in {:?})",
        started.elapsed()
    );
    assert_eq!(
        get_from(&app, &og("/"), "198.51.100.8").await,
        StatusCode::OK
    );
    app.cleanup().await;
}
