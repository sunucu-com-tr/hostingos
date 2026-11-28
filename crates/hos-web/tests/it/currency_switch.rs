//! Storefront currency switcher (a CSRF-protected POST form, never a
//! link): a guest's choice persists in a cookie, is
//! limited to active currencies, changes prices and JSON-LD together, keeps
//! one canonical URL, varies cached pages on the cookie, never leaks into
//! public machine-readable files, and is not offered to signed-in clients.

use crate::biz_support::{login, owner};
use crate::common::TestApp;
use hos_core::domain::{Locale, ProductType};
use hos_core::money::{BillingCycle, Currency};
use hos_db::repo::{admin, catalog};
use http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;

const TEMPLATES: [&str; 7] = [
    "classic",
    "cloud",
    "corporate",
    "terminal",
    "studio",
    "editorial",
    "atlas",
];

async fn currency(app: &TestApp, code: Currency, active: bool) {
    catalog::upsert_currency(
        app.pool(),
        app.tenant_id,
        &catalog::CurrencyInput {
            code,
            name: code.as_str().into(),
            symbol: code.as_str().into(),
            prefix: String::new(),
            suffix: format!(" {}", code.as_str()),
            decimals: 2,
            exchange_rate: Decimal::ONE,
            is_default: false,
            sort_order: 5,
        },
    )
    .await
    .expect("currency");
    admin::set_currency_active(app.pool(), app.tenant_id, code, active)
        .await
        .expect("active");
}

async fn fixture(app: &TestApp) -> Currency {
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
    currency(app, other, true).await;
    currency(app, Currency::GBP, false).await;
    let group = catalog::create_product_group(
        app.pool(),
        app.tenant_id,
        &catalog::ProductGroupInput {
            slug: "sunucular".into(),
            name: "Sunucular".into(),
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
        &catalog::ProductInput::new("vps-kucuk", "VPS Küçük", ProductType::Vps, Some(group.id)),
    )
    .await
    .expect("product");
    for (cur, price) in [(default, 600), (other, 20)] {
        catalog::set_product_price(
            app.pool(),
            app.tenant_id,
            product.id,
            cur,
            BillingCycle::Monthly,
            Decimal::from(price),
            Decimal::ZERO,
        )
        .await
        .expect("price");
    }
    other
}

fn price_currency(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("\"priceCurrency\":\"") {
        let tail = &rest[i + 17..];
        out.push(tail.chars().take(3).collect());
        rest = tail;
    }
    out
}

fn canonical(body: &str) -> String {
    body.split("<link")
        .find(|t| t.trim_start().starts_with("rel=\"canonical\""))
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn guest_currency_switch_is_consistent_and_safe() {
    let Some(mut app) = TestApp::new().await else {
        return;
    };
    let other = fixture(&app).await;
    let page = "/sunucular/vps-kucuk";

    let before = app.get(page).await;
    assert_eq!(before.status, StatusCode::OK);
    assert!(
        before.body.contains("class=\"currency-switch\""),
        "switcher shown"
    );
    assert!(
        before.body.contains(&format!(
            "method=\"post\" action=\"/currency/{}?next=",
            other.as_str()
        )),
        "a form, not a link"
    );
    assert!(
        !before
            .body
            .contains(&format!("/currency/{}", Currency::GBP.as_str())),
        "inactive hidden"
    );
    let default_cur = price_currency(&before.body);
    assert!(!default_cur.is_empty() && !default_cur.contains(&other.as_str().to_owned()));
    let csrf = before.csrf();
    let switch = format!("/currency/{}?next=%2Fsunucular%2Fvps-kucuk", other.as_str());

    // A link (GET) or a post without the CSRF token changes nothing.
    let r = app.get(&switch).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    let r = app.post(&switch, &[]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "missing CSRF");
    assert!(!app.cookies.contains_key("hos_currency"));

    // Switch: cookie set, safe redirect back.
    let r = app.post(&switch, &[("csrf_token", &csrf)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), page);
    assert_eq!(
        app.cookies.get("hos_currency").map(String::as_str),
        Some(other.as_str())
    );

    for tpl in TEMPLATES {
        app.setting("appearance.template", json!(tpl)).await;
        hos_web::theme::invalidate(app.tenant_id);
        let after = app.get(page).await;
        assert_eq!(after.status, StatusCode::OK, "{tpl}");
        let curs = price_currency(&after.body);
        assert!(!curs.is_empty(), "{tpl}: JSON-LD offers");
        assert!(curs.iter().all(|c| c == other.as_str()), "{tpl}: {curs:?}");
        let shown = hos_web::i18n::format_money(Decimal::from(20), other, Locale::Tr);
        assert!(
            after.body.contains(&shown),
            "{tpl}: price shown in the chosen currency ({shown})"
        );
        assert_eq!(
            canonical(&after.body),
            canonical(&before.body),
            "{tpl}: one canonical URL"
        );
        assert!(!canonical(&after.body).contains("currency"), "{tpl}");
        let vary = after
            .headers
            .get("vary")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            vary.to_ascii_lowercase().contains("cookie"),
            "{tpl}: varies on the cookie"
        );
    }

    // Public files keep the default currency.
    let llms = app.get("/llms-full.txt").await;
    assert_eq!(llms.status, StatusCode::OK);
    for locale in [Locale::Tr, Locale::En] {
        let leaked = hos_web::i18n::format_money(Decimal::from(20), other, locale);
        assert!(
            !llms.body.contains(&leaked),
            "llms-full.txt keeps the default currency"
        );
    }

    // Inactive / unknown codes and an open redirect change nothing.
    for path in [
        "/currency/GBP?next=%2F".to_owned(),
        "/currency/XYZ?next=%2F".to_owned(),
        "/currency/zzzz".to_owned(),
    ] {
        let r = app.post(&path, &[("csrf_token", &csrf)]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(
            app.cookies.get("hos_currency").map(String::as_str),
            Some(other.as_str()),
            "{path}"
        );
    }
    let r = app
        .post(
            &format!("/currency/{}?next=%2F%2Fevil.example%2F", other.as_str()),
            &[("csrf_token", &csrf)],
        )
        .await;
    assert_eq!(r.location(), "/");
    // A forged cookie for an inactive currency falls back to the default.
    app.cookies.insert("hos_currency".into(), "GBP".into());
    let forged = app.get(page).await;
    assert_eq!(price_currency(&forged.body), default_cur);

    // Signed-in clients see their account currency and no switcher.
    app.cookies
        .insert("hos_currency".into(), other.as_str().into());
    let (_u, _c, email) = owner(&app, "kur").await;
    login(&mut app, &email).await;
    app.cookies
        .insert("hos_currency".into(), other.as_str().into());
    let client_page = app.get(page).await;
    assert!(!client_page.body.contains("class=\"currency-switch\""));
    assert!(
        price_currency(&client_page.body)
            .iter()
            .all(|c| c == Currency::TRY.as_str())
    );
    app.cleanup().await;
}
