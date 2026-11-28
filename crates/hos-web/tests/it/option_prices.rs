//! Configurable option prices per currency. Option values are priced in
//! every currency separately (no conversion), like products:
//!
//! * the admin forms show and accept `price_<CUR>_<cycle>` for every active
//!   currency; the older un-prefixed `price_<cycle>` still means the default
//!   currency, an absent field leaves a price alone, an empty one clears it;
//! * pricing refuses a value the cart currency does not price for the cycle
//!   while another currency does (never free), keeps a value no currency
//!   prices free, and treats an explicit zero in one currency as free in all;
//! * the storefront does not offer a value priced only in other currencies.

use crate::biz_support::{post, staff};
use crate::common::TestApp;
use hos_billing::{QuoteItem, QuoteRequest};
use hos_core::domain::ProductType;
use hos_core::ids::{ConfigOptionGroupId, ConfigOptionId, ProductId};
use hos_core::money::{BillingCycle, Currency};
use hos_db::enums::ConfigOptionType;
use hos_db::ids::ConfigOptionValueId;
use hos_db::repo::{admin, catalog};
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;

/// The tenant's default currency and a second, active one.
async fn currencies(app: &TestApp) -> (Currency, Currency) {
    let default = catalog::list_currencies(app.pool(), app.tenant_id, true)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.is_default)
        .map_or(Currency::TRY, |c| c.code);
    let other = if default == Currency::EUR {
        Currency::USD
    } else {
        Currency::EUR
    };
    catalog::upsert_currency(
        app.pool(),
        app.tenant_id,
        &catalog::CurrencyInput {
            code: other,
            name: other.as_str().into(),
            symbol: other.as_str().into(),
            prefix: String::new(),
            suffix: format!(" {}", other.as_str()),
            decimals: 2,
            exchange_rate: Decimal::ONE,
            is_default: false,
            sort_order: 5,
        },
    )
    .await
    .expect("currency");
    admin::set_currency_active(app.pool(), app.tenant_id, other, true)
        .await
        .expect("active");
    (default, other)
}

/// A product group page `/opsiyon-sunucular/opsiyon-vps` priced monthly and
/// annually in both currencies, with an option group "RAM" attached.
async fn product_with_option(
    app: &TestApp,
    (default, other): (Currency, Currency),
) -> (ProductId, ConfigOptionGroupId, ConfigOptionId) {
    let group = catalog::create_product_group(
        app.pool(),
        app.tenant_id,
        &catalog::ProductGroupInput {
            slug: "opsiyon-sunucular".into(),
            name: "Opsiyon Sunucular".into(),
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
    let product = catalog::create_product(
        app.pool(),
        app.tenant_id,
        &catalog::ProductInput::new(
            "opsiyon-vps",
            "Opsiyon VPS",
            ProductType::Vps,
            Some(group.id),
        ),
    )
    .await
    .expect("product");
    for (cur, monthly, annual) in [(default, 600, 6000), (other, 20, 200)] {
        for (cycle, price) in [
            (BillingCycle::Monthly, monthly),
            (BillingCycle::Annual, annual),
        ] {
            catalog::set_product_price(
                app.pool(),
                app.tenant_id,
                product.id,
                cur,
                cycle,
                Decimal::from(price),
                Decimal::ZERO,
            )
            .await
            .expect("price");
        }
    }
    let options = catalog::create_config_option_group(app.pool(), app.tenant_id, "Kaynak", None)
        .await
        .expect("option group");
    catalog::attach_config_group(app.pool(), app.tenant_id, options, product.id)
        .await
        .expect("attach");
    let ram = catalog::create_config_option(
        app.pool(),
        app.tenant_id,
        options,
        "RAM",
        ConfigOptionType::Dropdown,
        Some("memory_mb"),
        &json!({}),
        0,
    )
    .await
    .expect("option");
    (product.id, options, ram)
}

async fn value(
    app: &TestApp,
    option: ConfigOptionId,
    label: &str,
    sort: i32,
) -> ConfigOptionValueId {
    catalog::create_config_option_value(
        app.pool(),
        app.tenant_id,
        option,
        label,
        label,
        &json!({}),
        sort,
    )
    .await
    .expect("value")
}

async fn price(
    app: &TestApp,
    vid: ConfigOptionValueId,
    cur: Currency,
    cycle: BillingCycle,
    amount: i64,
) {
    catalog::set_config_option_price(
        app.pool(),
        app.tenant_id,
        vid,
        cur,
        cycle,
        Decimal::from(amount),
        Decimal::ZERO,
    )
    .await
    .expect("option price");
}

/// `(currency, cycle) → price` of one value.
async fn stored(
    app: &TestApp,
    group: ConfigOptionGroupId,
    vid: ConfigOptionValueId,
) -> Vec<(Currency, BillingCycle, Decimal)> {
    let mut rows: Vec<_> = admin::group_value_prices(app.pool(), app.tenant_id, group)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.value_id == vid)
        .map(|r| (r.currency, r.billing_cycle, r.price.normalize()))
        .collect();
    rows.sort_by_key(|(c, cy, _)| (c.as_str().to_owned(), cy.as_str().to_owned()));
    rows
}

#[tokio::test]
async fn option_value_prices_are_edited_in_every_currency() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (default, other) = currencies(&app).await;
    let (_product, group, ram) = product_with_option(&app, (default, other)).await;
    let csrf = staff(&mut app, "owner").await;
    let (d, o) = (default.as_str(), other.as_str());
    let page = format!("/admin/catalog/options/{group}");

    // A new value with prices in both currencies.
    let r = post(
        &mut app,
        &format!("/admin/catalog/options/{group}/options/{ram}/values"),
        &csrf,
        &[
            ("label", "8 GB"),
            ("value", "8192"),
            (&format!("price_{d}_monthly"), "100"),
            (&format!("price_{o}_monthly"), "4"),
            (&format!("price_{o}_annual"), ""),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let vid = admin::group_option_values(app.pool(), app.tenant_id, group, default)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.label.as_deref() == Some("8 GB"))
        .and_then(|r| r.value_id)
        .expect("value created");
    let m = BillingCycle::Monthly;
    let hundred = Decimal::from(100);
    assert_eq!(
        stored(&app, group, vid).await,
        {
            let mut v = vec![(default, m, hundred), (other, m, Decimal::from(4))];
            v.sort_by_key(|(c, _, _)| c.as_str().to_owned());
            v
        },
        "both currencies stored"
    );

    // The form shows one row per active currency with its stored prices.
    let res = app.get(&page).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    for (name, value) in [
        (format!("price_{d}_monthly"), "100"),
        (format!("price_{o}_monthly"), "4"),
        (format!("price_{o}_annual"), ""),
    ] {
        assert!(
            res.body
                .contains(&format!("name=\"{name}\" value=\"{value}\"")),
            "{name} = {value:?} shown"
        );
    }
    assert!(
        res.body.contains(&format!("name=\"setup_{o}_monthly\"")),
        "setup fees kept per currency"
    );
    assert!(
        !res.body.contains("name=\"price_monthly\""),
        "no legacy names"
    );

    // Older forms: the un-prefixed fields are the default currency and
    // leave the other currency alone.
    let prices = format!("/admin/catalog/options/{group}/values/{vid}/prices");
    let r = post(&mut app, &prices, &csrf, &[("price_monthly", "120")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let rows = stored(&app, group, vid).await;
    assert!(rows.contains(&(default, m, Decimal::from(120))), "{rows:?}");
    assert!(rows.contains(&(other, m, Decimal::from(4))), "{rows:?}");

    // Prefixed fields: an empty one clears, a new one adds; the default
    // currency is untouched when its fields are absent.
    let r = post(
        &mut app,
        &prices,
        &csrf,
        &[
            (&format!("price_{o}_monthly"), ""),
            (&format!("price_{o}_quarterly"), "10.5"),
            (&format!("setup_{o}_quarterly"), "2"),
        ],
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let rows = stored(&app, group, vid).await;
    assert!(
        !rows.iter().any(|(c, cy, _)| *c == other && *cy == m),
        "{rows:?}"
    );
    assert!(
        rows.contains(&(other, BillingCycle::Quarterly, Decimal::new(105, 1))),
        "{rows:?}"
    );
    assert!(rows.contains(&(default, m, Decimal::from(120))), "{rows:?}");

    // An invalid amount in any currency changes nothing.
    let before = stored(&app, group, vid).await;
    for bad in ["-1", "abc"] {
        let r = post(
            &mut app,
            &prices,
            &csrf,
            &[
                (&format!("price_{d}_monthly"), "130"),
                (&format!("price_{o}_monthly"), bad),
            ],
        )
        .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        assert_eq!(stored(&app, group, vid).await, before, "{bad} refused");
    }

    app.cleanup().await;
}

#[tokio::test]
async fn option_value_without_price_in_the_cart_currency_is_not_free() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let (default, other) = currencies(&app).await;
    let (product, _group, ram) = product_with_option(&app, (default, other)).await;
    let m = BillingCycle::Monthly;
    // Sorted first, so the configure page would preselect it.
    let only_default = value(&app, ram, "opt-only-default", 0).await;
    price(&app, only_default, default, m, 100).await;
    let both = value(&app, ram, "opt-both-currencies", 1).await;
    price(&app, both, default, m, 100).await;
    price(&app, both, other, m, 4).await;
    let zero = value(&app, ram, "opt-zero-in-default", 2).await;
    price(&app, zero, default, m, 0).await;
    let unpriced = value(&app, ram, "opt-never-priced", 3).await;

    let quote = |cur: Currency, cycle: BillingCycle, vid: ConfigOptionValueId| {
        let billing = app.state.billing.clone();
        let tenant = app.tenant_id;
        async move {
            let mut item = QuoteItem::product(product, cycle);
            item.config_selections = json!({ ram.to_string(): { "value_id": vid.to_string() } });
            billing
                .quote(
                    tenant,
                    &QuoteRequest {
                        currency: cur,
                        items: vec![item],
                        coupon_code: None,
                        client_id: None,
                        country_code: None,
                        region: None,
                    },
                )
                .await
                .map(|q| q.lines[0].unit_price.normalize())
        }
    };

    // Priced in the default currency only: charged there, refused (never
    // free) in the other one.
    assert_eq!(
        quote(default, m, only_default).await.unwrap(),
        Decimal::from(700)
    );
    let err = quote(other, m, only_default)
        .await
        .expect_err("not free in the other currency");
    let msg = err.to_string();
    assert!(
        msg.contains("opt-only-default")
            && msg.contains("not available")
            && msg.contains(other.as_str()),
        "{msg}"
    );
    // A cycle no currency prices stays free in every currency.
    let a = BillingCycle::Annual;
    assert_eq!(
        quote(other, a, only_default).await.unwrap(),
        Decimal::from(200)
    );
    assert_eq!(
        quote(default, a, only_default).await.unwrap(),
        Decimal::from(6000)
    );
    // Priced in both: each currency its own price.
    assert_eq!(quote(other, m, both).await.unwrap(), Decimal::from(24));
    assert_eq!(quote(default, m, both).await.unwrap(), Decimal::from(700));
    // An explicit zero is free in any currency; no price rows at all = free.
    assert_eq!(quote(other, m, zero).await.unwrap(), Decimal::from(20));
    assert_eq!(quote(other, m, unpriced).await.unwrap(), Decimal::from(20));
    assert_eq!(
        quote(default, m, unpriced).await.unwrap(),
        Decimal::from(600)
    );

    // The storefront does not offer the value in the other currency.
    app.cookies
        .insert("hos_currency".into(), other.as_str().into());
    for path in [
        "/opsiyon-sunucular/opsiyon-vps",
        "/cart/configure/opsiyon-vps",
    ] {
        let res = app.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}: {}", res.body);
        assert!(
            !res.body.contains("opt-only-default"),
            "{path}: not offered"
        );
        assert!(res.body.contains("opt-both-currencies"), "{path}: offered");
        assert!(res.body.contains("opt-never-priced"), "{path}: free value");
    }
    // …but still is in the default currency.
    app.cookies
        .insert("hos_currency".into(), default.as_str().into());
    let res = app.get("/opsiyon-sunucular/opsiyon-vps").await;
    assert!(res.body.contains("opt-only-default"));

    app.cleanup().await;
}
