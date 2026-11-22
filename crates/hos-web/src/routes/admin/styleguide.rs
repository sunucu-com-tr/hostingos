//! `/admin/_styleguide` (debug builds only): every component of the design
//! system on one page — the visual reference for the area templates.

use askama::Template;
use axum::response::Response;
use hos_db::Paged;
use rust_decimal::Decimal;

use super::menu;
use crate::error::AppResult;
use crate::extract::CurrentStaff;
use crate::forms::FormErrors;
use crate::view::{Crumb, NavItem, PageCtx, Pager, render};

#[derive(Template)]
#[template(path = "admin/styleguide.html")]
pub(super) struct StyleguidePage {
    ctx: PageCtx,
    pager: Pager,
    tabs: Vec<NavItem>,
    steps: Vec<String>,
    options: Vec<(String, String)>,
    errors: FormErrors,
    price: Decimal,
    statuses: Vec<&'static str>,
    sample_code: String,
}
crate::page!(StyleguidePage);

pub(super) async fn show(staff: CurrentStaff, mut ctx: PageCtx) -> AppResult<Response> {
    ctx.set_title("Styleguide");
    ctx.panel = Some(menu::chrome(
        &ctx,
        &staff,
        vec![Crumb::current("Styleguide")],
    ));
    let paged: Paged<()> = Paged {
        items: Vec::new(),
        total: 240,
        limit: 20,
        offset: 80,
    };
    let base = ctx.url("/admin/_styleguide");
    let mut errors = FormErrors::default();
    errors.add("email", ctx.t("auth.errors.email_invalid"));
    let tab = |label: &str, href: &str, icon: &str| NavItem {
        label: label.to_owned(),
        href: href.to_owned(),
        description: String::new(),
        icon: icon.to_owned(),
    };
    render(&StyleguidePage {
        pager: Pager::new(&paged, &base, &[]),
        tabs: vec![
            tab("Genel", "#general", "grid"),
            tab("Faturalar", "#invoices", "receipt"),
            tab("Ayarlar", "#settings", "settings"),
        ],
        steps: vec![
            "Paket".into(),
            "Alan adı".into(),
            "Ödeme".into(),
            "Onay".into(),
        ],
        options: vec![
            ("monthly".into(), ctx.t("common.cycle.monthly")),
            ("annual".into(), ctx.t("common.cycle.annual")),
        ],
        errors,
        price: Decimal::new(123_456, 2),
        statuses: vec![
            "active",
            "pending",
            "suspended",
            "cancelled",
            "paid",
            "unpaid",
            "overdue",
            "open",
            "answered",
            "closed",
        ],
        sample_code: "curl -H \"Authorization: Bearer hos_live_…\" https://example.com/api/v1/me"
            .into(),
        ctx,
    })
}
