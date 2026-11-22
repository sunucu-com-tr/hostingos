//! hos-panels control-panel integration tests, compiled into one test binary.
//!
//! Every integration test of the crate is a module of this binary (rather
//! than a `tests/<name>.rs` file of its own): each extra test binary is
//! another full static link of the crate and its dependency tree, which
//! made `target/` grow by gigabytes. Run one group with
//! `cargo test -p hos-panels --test it <module>::`.

mod common;
mod cpanel;
mod cyberpanel;
mod directadmin;
mod mock;
mod outbound;
mod plesk;
