//! hos-billing integration tests, compiled into one test binary.
//!
//! Every integration test of the crate is a module of this binary (rather
//! than a `tests/<name>.rs` file of its own): each extra test binary is
//! another full static link of the crate and its dependency tree, which
//! made `target/` grow by gigabytes. Run one group with
//! `cargo test -p hos-billing --test it <module>::`.

mod card_setup;
mod common;
mod einvoice;
mod flows;
mod gateways;
mod invoice_cancel;
mod kpi;
mod mass_pay;
mod security;
mod vm;
