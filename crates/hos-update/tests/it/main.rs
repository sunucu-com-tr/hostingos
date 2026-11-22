//! hos-update integration tests, one test binary (see crates/hos-web/tests/it
//! for why). Keys are generated inside the tests; nothing touches the
//! network except wiremock on 127.0.0.1.

mod apply;
mod check;
mod detect;
mod fetch;
mod support;
mod verify;
mod webhook;
