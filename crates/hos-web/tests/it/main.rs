//! hos-web integration tests, compiled into one test binary.
//!
//! Every integration test of the crate is a module of this binary (rather
//! than a `tests/<name>.rs` file of its own): each extra test binary is
//! another full static link of the crate and its dependency tree, which
//! made `target/` grow by gigabytes. Run one group with
//! `cargo test -p hos-web --test it <module>::`.

mod addons;
mod admin;
mod admin_content;
mod admin_settings;
mod affiliates;
mod api;
mod appearance;
mod biz_api;
mod biz_support;
mod bundles;
mod card_pay;
mod client;
mod common;
mod currency_switch;
mod demo;
mod downloads;
mod einvoice;
mod import;
mod inbound_mail;
mod mass_pay;
mod option_prices;
mod page_slugs;
mod platform;
mod quotes;
mod schema;
mod security;
mod single;
mod staff_tools;
mod status_page;
mod storefront_seo;
mod updates;
mod vm_console;
mod web;
mod webhooks;
