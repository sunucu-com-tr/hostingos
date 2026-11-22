//! hos-domains registrar integration tests, compiled into one test binary.
//!
//! Every integration test of the crate is a module of this binary (rather
//! than a `tests/<name>.rs` file of its own): each extra test binary is
//! another full static link of the crate and its dependency tree, which
//! made `target/` grow by gigabytes. Run one group with
//! `cargo test -p hos-domains --test it <module>::`.

mod mock_and_rdap;
mod namecheap;
mod openprovider;
mod resellerclub;
mod support;
