//! CMS page slugs that belong to another storefront object by design
//! (README "Default content and blocks"): the copy pages of routes (`home`,
//! `domains`, `contact`) and category landings (a page whose slug equals a
//! product group's slug). The page editor must save them; the route and
//! area slugs (`admin`, `cart` …) stay reserved; a product group may take a
//! slug a landing page already uses, and the storefront serves one URL.

use crate::biz_support::{post, staff};
use crate::common::TestApp;
use hos_core::domain::{Locale, ProductType};
use hos_core::ids::ProductGroupId;
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::catalog;
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

/// `(translation group, tr title, en title)` of the seeded page `slug`.
async fn seeded(app: &TestApp, slug: &str) -> (Uuid, String, String) {
    let title = |locale: Locale| {
        let pool = app.pool().clone();
        let tenant = app.tenant_id;
        let slug = slug.to_owned();
        async move {
            sqlx::query_as::<_, (Uuid, String)>(
                "select translation_group_id, title from pages
                  where tenant_id = $1 and locale = $2 and slug = $3",
            )
            .bind(tenant)
            .bind(locale)
            .bind(slug)
            .fetch_one(&pool)
            .await
            .expect("seeded page")
        }
    };
    let (group, tr) = title(Locale::Tr).await;
    let (group_en, en) = title(Locale::En).await;
    assert_eq!(group, group_en, "{slug}: one translation group");
    (group, tr, en)
}

/// A priced product in `group` (empty groups are not on the storefront).
async fn product_in(app: &TestApp, group: ProductGroupId, slug: &str) {
    let p = catalog::create_product(
        app.pool(),
        app.tenant_id,
        &catalog::ProductInput::new(slug, slug, ProductType::Dedicated, Some(group)),
    )
    .await
    .expect("product");
    let currency = catalog::list_currencies(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.is_default)
        .map_or(Currency::TRY, |c| c.code);
    catalog::set_product_price(
        app.pool(),
        app.tenant_id,
        p.id,
        currency,
        BillingCycle::Monthly,
        Decimal::from(100),
        Decimal::ZERO,
    )
    .await
    .expect("price");
}

async fn body_of(app: &TestApp, slug: &str, locale: Locale) -> String {
    sqlx::query_scalar::<_, String>(
        "select body from pages where tenant_id = $1 and locale = $2 and slug = $3",
    )
    .bind(app.tenant_id)
    .bind(locale)
    .bind(slug)
    .fetch_one(app.pool())
    .await
    .expect("page body")
}

/// The editor form of a page in both languages.
fn form<'a>(
    titles: (&'a str, &'a str),
    slugs: (&'a str, &'a str),
    bodies: (&'a str, &'a str),
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("tr_title", titles.0),
        ("tr_slug", slugs.0),
        ("tr_body", bodies.0),
        ("tr_status", "published"),
        ("en_title", titles.1),
        ("en_slug", slugs.1),
        ("en_body", bodies.1),
        ("en_status", "published"),
    ]
}

#[tokio::test]
async fn copy_pages_and_group_landings_are_saveable() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;

    // The seeded copy of /contact and /domains: only the body changes.
    for slug in ["contact", "domains"] {
        let (group, tr, en) = seeded(&app, slug).await;
        let (tr_body, en_body) = (format!("Yeni {slug} metni."), format!("New {slug} copy."));
        let r = post(
            &mut app,
            &format!("/admin/content/pages/{group}"),
            &csrf,
            &form((&tr, &en), (slug, slug), (&tr_body, &en_body)),
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{slug}: {}", r.body);
        assert_eq!(body_of(&app, slug, Locale::Tr).await, tr_body, "{slug} tr");
        assert_eq!(body_of(&app, slug, Locale::En).await, en_body, "{slug} en");
    }

    // A deleted copy page can be created again (and only once per language).
    sqlx::query("delete from pages where tenant_id = $1 and slug = 'contact'")
        .bind(app.tenant_id)
        .execute(app.pool())
        .await
        .unwrap();
    let new_contact = form(
        ("İletişim", "Contact"),
        ("contact", "contact"),
        ("Bize yazın.", "Write to us."),
    );
    let r = post(&mut app, "/admin/content/pages", &csrf, &new_contact).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "re-created: {}", r.body);
    let r = post(&mut app, "/admin/content/pages", &csrf, &new_contact).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "a second copy");

    // A landing for an existing product group, in both languages.
    let group = catalog::create_product_group(
        app.pool(),
        app.tenant_id,
        &catalog::ProductGroupInput {
            slug: "atlas-sunucular".into(),
            name: "Atlas Sunucular".into(),
            tagline: None,
            description: None,
            translations: json!({}),
            icon: None,
            is_hidden: false,
            sort_order: 0,
        },
    )
    .await
    .expect("group");
    product_in(&app, group.id, "atlas-d1").await;
    let r = post(
        &mut app,
        "/admin/content/pages",
        &csrf,
        &form(
            ("Atlas sunucular", "Atlas servers"),
            (&group.slug, &group.slug),
            ("Kabinde hazır sunucular.", "Servers ready in the rack."),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "landing: {}", r.body);
    // One URL: the group page, with the landing's first paragraph as lead.
    let res = app.get("/atlas-sunucular").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body.contains("Kabinde hazır sunucular."),
        "landing merged"
    );
    assert_eq!(res.body.matches("<h1").count(), 1);

    // Route and area slugs stay reserved.
    for slug in ["admin", "cart", "checkout", "auth", "kb"] {
        let r = post(
            &mut app,
            "/admin/content/pages",
            &csrf,
            &form(("Ayrılmış", "Reserved"), (slug, slug), ("x", "x")),
        )
        .await;
        assert_eq!(
            r.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{slug} reserved"
        );
    }

    app.cleanup().await;
}

#[tokio::test]
async fn a_product_group_may_take_a_landing_page_slug() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let csrf = staff(&mut app, "owner").await;
    // The seed ships draft landings such as `vps` and `dedicated`.
    seeded(&app, "vps").await;
    seeded(&app, "dedicated").await;

    let r = post(
        &mut app,
        "/admin/catalog/groups",
        &csrf,
        &[("name", "Sanal Sunucular"), ("slug", "vps")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let groups = catalog::list_product_groups(app.pool(), app.tenant_id, true)
        .await
        .unwrap();
    let vps = groups
        .iter()
        .find(|g| g.slug == "vps")
        .expect("group created with the landing's slug");
    product_in(&app, vps.id, "fiziksel-d1").await;

    // Renaming to another landing's slug works as well.
    let r = post(
        &mut app,
        &format!("/admin/catalog/groups/{}", vps.id),
        &csrf,
        &[("name", "Fiziksel Sunucular"), ("slug", "dedicated")],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let groups = catalog::list_product_groups(app.pool(), app.tenant_id, true)
        .await
        .unwrap();
    assert!(
        groups
            .iter()
            .any(|g| g.slug == "dedicated" && g.id == vps.id)
    );
    assert!(!groups.iter().any(|g| g.slug == "vps"));

    // The landing page is still editable with the group's slug, and the
    // storefront serves the group (one URL, not a second page).
    let (page, tr, en) = seeded(&app, "dedicated").await;
    let r = post(
        &mut app,
        &format!("/admin/content/pages/{page}"),
        &csrf,
        &form(
            (&tr, &en),
            ("dedicated", "dedicated"),
            ("Fiziksel sunucu açılış metni.", "Dedicated landing copy."),
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let res = app.get("/dedicated").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Fiziksel Sunucular"), "the group page");
    assert!(
        res.body.contains("Fiziksel sunucu açılış metni."),
        "with its landing"
    );
    assert_eq!(res.body.matches("<h1").count(), 1);

    app.cleanup().await;
}
