//! Product bundles: staff CRUD (products.manage, CSRF, two distinct
//! products of the tenant), the storefront list with prices, ordering a
//! bundle into the cart with its coupon, the coupon refused for a partial
//! cart, an inactive bundle, and another tenant.

use crate::biz_support::{csrf_of, post, product, staff};
use crate::common::TestApp;
use hos_billing::pricing::{QuoteItem, QuoteRequest};
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::bundles as brepo;
use http::StatusCode;
use rust_decimal::Decimal;

async fn bundles(app: &TestApp) -> Vec<brepo::BundleRow> {
    brepo::list(app.pool(), app.tenant_id).await.unwrap()
}

#[tokio::test]
async fn bundles_admin_storefront_and_coupon_rules() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let p1 = product(&app).await;
    let p2 = product(&app).await;
    let (p1s, p2s) = (p1.to_string(), p2.to_string());
    let form = |slug: &'static str, second: String| {
        vec![
            ("name".to_owned(), "Başlangıç paketi".to_owned()),
            ("slug".to_owned(), slug.to_owned()),
            ("discount_value".to_owned(), "20".to_owned()),
            ("item_product_1".to_owned(), p1s.clone()),
            ("item_cycle_1".to_owned(), "monthly".to_owned()),
            ("item_product_2".to_owned(), second),
            ("item_cycle_2".to_owned(), "monthly".to_owned()),
            ("is_active".to_owned(), "1".to_owned()),
            ("show_on_storefront".to_owned(), "1".to_owned()),
            ("sort_order".to_owned(), "0".to_owned()),
        ]
    };

    // ---- staff without products.manage
    let csrf = staff(&mut app, "billing").await;
    assert_eq!(
        app.get("/admin/catalog/bundles").await.status,
        StatusCode::FORBIDDEN
    );
    let f = form("baslangic", p2s.clone());
    let pairs: Vec<(&str, &str)> = f.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        post(&mut app, "/admin/catalog/bundles", &csrf, &pairs)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // ---- admin
    staff(&mut app, "admin").await;
    let csrf = csrf_of(&mut app, "/admin/catalog/bundles").await;
    assert_eq!(
        app.post("/admin/catalog/bundles", &pairs).await.status,
        StatusCode::FORBIDDEN,
        "missing CSRF"
    );
    // the same product twice / a single product / a bad discount: refused
    for bad in [
        form("baslangic", p1s.clone()),
        form("baslangic", String::new()),
    ] {
        let pairs: Vec<(&str, &str)> = bad.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let r = post(&mut app, "/admin/catalog/bundles", &csrf, &pairs).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
    }
    let mut over = form("baslangic", p2s.clone());
    over[2].1 = "150".into();
    let pairs_over: Vec<(&str, &str)> =
        over.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    post(&mut app, "/admin/catalog/bundles", &csrf, &pairs_over).await;
    assert!(bundles(&app).await.is_empty(), "invalid bundles stored");
    // a forged product id → 404
    let forged = form("baslangic", uuid::Uuid::now_v7().to_string());
    let pairs_forged: Vec<(&str, &str)> = forged
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        post(&mut app, "/admin/catalog/bundles", &csrf, &pairs_forged)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let r = post(&mut app, "/admin/catalog/bundles", &csrf, &pairs).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let list = bundles(&app).await;
    assert_eq!(list.len(), 1);
    let bundle = list[0].clone();
    assert!(bundle.coupon_code.starts_with("BND-") && bundle.coupon_code.len() == 20);
    assert_eq!(bundle.discount_value, Decimal::from(20));
    assert_eq!(
        brepo::items(app.pool(), app.tenant_id, bundle.id)
            .await
            .unwrap()
            .len(),
        2
    );
    // a second bundle cannot reuse the slug
    post(&mut app, "/admin/catalog/bundles", &csrf, &pairs).await;
    assert_eq!(bundles(&app).await.len(), 1);
    assert_eq!(
        app.get(&format!("/admin/catalog/bundles/{}", bundle.id))
            .await
            .status,
        StatusCode::OK
    );

    // ---- another tenant cannot see or change it
    let Some(mut other) = TestApp::new().await else {
        return;
    };
    let other_csrf = staff(&mut other, "owner").await;
    assert_eq!(
        other
            .get(&format!("/admin/catalog/bundles/{}", bundle.id))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let r = post(
        &mut other,
        &format!("/admin/catalog/bundles/{}/delete", bundle.id),
        &other_csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    other.cookies.clear();
    let r = other.get("/bundles").await;
    assert!(!r.body.contains("Başlangıç paketi"));
    other.cleanup().await;

    // ---- the coupon needs every product of the bundle
    let quote = |items: Vec<QuoteItem>| QuoteRequest {
        currency: Currency::TRY,
        items,
        coupon_code: Some(bundle.coupon_code.clone()),
        client_id: None,
        country_code: None,
        region: None,
    };
    let full = app
        .state
        .billing
        .quote(
            app.tenant_id,
            &quote(vec![
                QuoteItem::product(p1, BillingCycle::Monthly),
                QuoteItem::product(p2, BillingCycle::Monthly),
            ]),
        )
        .await
        .unwrap();
    assert!(full.coupon.is_some());
    assert_eq!(
        full.discount_total,
        Decimal::new(1996, 2),
        "20% of 2 × 49.90"
    );
    let partial = app
        .state
        .billing
        .quote(
            app.tenant_id,
            &quote(vec![QuoteItem::product(p1, BillingCycle::Monthly)]),
        )
        .await
        .unwrap();
    assert!(
        partial.coupon.is_none(),
        "bundle coupon refused for one product"
    );
    assert_eq!(partial.discount_total, Decimal::ZERO);
    // Extra units of a bundled product are not discounted: one unit each.
    sqlx::query("update products set allow_quantity = true where tenant_id = $1 and id = $2")
        .bind(app.tenant_id)
        .bind(p1)
        .execute(app.pool())
        .await
        .unwrap();
    let more = app
        .state
        .billing
        .quote(
            app.tenant_id,
            &quote(vec![
                QuoteItem {
                    quantity: 4,
                    ..QuoteItem::product(p1, BillingCycle::Monthly)
                },
                QuoteItem::product(p1, BillingCycle::Monthly),
                QuoteItem::product(p2, BillingCycle::Monthly),
            ]),
        )
        .await
        .unwrap();
    assert!(more.coupon.is_some());
    assert_eq!(more.discount_total, Decimal::new(1996, 2), "still 2 units");
    // The bundle's billing cycle is part of it.
    hos_db::repo::catalog::set_product_price(
        app.pool(),
        app.tenant_id,
        p2,
        Currency::TRY,
        BillingCycle::Annual,
        Decimal::new(49900, 2),
        Decimal::ZERO,
    )
    .await
    .unwrap();
    let other_cycle = app
        .state
        .billing
        .quote(
            app.tenant_id,
            &quote(vec![
                QuoteItem::product(p1, BillingCycle::Monthly),
                QuoteItem::product(p2, BillingCycle::Annual),
            ]),
        )
        .await
        .unwrap();
    assert!(
        other_cycle.coupon.is_none(),
        "another cycle is not the bundle"
    );
    let cycles: Vec<String> =
        sqlx::query_scalar("select billing_cycles from coupons where tenant_id = $1 and code = $2")
            .bind(app.tenant_id)
            .bind(&bundle.coupon_code)
            .fetch_one(app.pool())
            .await
            .unwrap();
    assert_eq!(cycles, vec!["monthly".to_owned()], "cycles stored");

    // ---- storefront: list, order (CSRF), cart
    app.cookies.clear();
    let page = app.get("/bundles").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Başlangıç paketi"));
    let shown = hos_web::i18n::format_money(
        Decimal::new(7984, 2),
        Currency::TRY,
        hos_core::domain::Locale::Tr,
    );
    assert!(page.body.contains(&shown), "bundle price {shown}");
    let home = app.get("/").await;
    assert!(home.body.contains("/bundles\""), "footer link");
    let order = format!("/bundles/{}/order", bundle.slug);
    assert_eq!(
        app.post(&order, &[]).await.status,
        StatusCode::FORBIDDEN,
        "missing CSRF"
    );
    let csrf = page.csrf();
    let r = post(&mut app, &order, &csrf, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().ends_with("/cart"), "{}", r.location());
    let cart = app.get("/cart").await;
    assert_eq!(cart.status, StatusCode::OK);
    assert!(cart.body.contains(&bundle.coupon_code), "coupon applied");
    // Typed in by hand, the bundle's code is refused.
    let csrf = cart.csrf();
    let r = post(&mut app, "/cart/coupon/remove", &csrf, &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = post(
        &mut app,
        "/cart/coupon",
        &csrf,
        &[("code", bundle.coupon_code.as_str())],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let cart = app.get("/cart").await;
    assert!(
        !cart.body.contains(&bundle.coupon_code),
        "typed-in bundle code refused"
    );
    let unknown = post(&mut app, "/bundles/yok-boyle/order", &csrf, &[]).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    // ---- an inactive bundle is neither listed nor orderable
    staff(&mut app, "admin").await;
    let csrf = csrf_of(&mut app, "/admin/catalog/bundles").await;
    let mut off = form("baslangic", p2s.clone());
    off.retain(|(k, _)| k != "is_active");
    let pairs_off: Vec<(&str, &str)> = off.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let r = post(
        &mut app,
        &format!("/admin/catalog/bundles/{}", bundle.id),
        &csrf,
        &pairs_off,
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    app.cookies.clear();
    let page = app.get("/bundles").await;
    assert!(!page.body.contains("Başlangıç paketi"));
    let csrf = page.csrf();
    assert_eq!(
        post(&mut app, &order, &csrf, &[]).await.status,
        StatusCode::NOT_FOUND
    );
    let inactive = app
        .state
        .billing
        .quote(
            app.tenant_id,
            &quote(vec![
                QuoteItem::product(p1, BillingCycle::Monthly),
                QuoteItem::product(p2, BillingCycle::Monthly),
            ]),
        )
        .await
        .unwrap();
    assert!(inactive.coupon.is_none(), "deactivated with the bundle");

    // ---- delete
    staff(&mut app, "admin").await;
    let csrf = csrf_of(&mut app, "/admin/catalog/bundles").await;
    let r = post(
        &mut app,
        &format!("/admin/catalog/bundles/{}/delete", bundle.id),
        &csrf,
        &[],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(bundles(&app).await.is_empty());
    app.cleanup().await;
}
