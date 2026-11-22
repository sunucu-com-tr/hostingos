//! # hos-auth
//!
//! Authentication and authorization building blocks for HostingOS. This crate
//! is **pure logic + outbound HTTP** (OAuth): it never touches the database.
//! `hos-db` persists the rows, `hos-web` wires everything together by handing
//! DB rows in and acting on the returned decisions.
//!
//! | Module | Contents |
//! |---|---|
//! | [`password`] | argon2id hash/verify, `needs_rehash`, password policy, dummy verification |
//! | [`session`] | session tokens (cookie = plain, DB = SHA-256), cookies, idle/absolute expiry, "remember me" |
//! | [`csrf`] | session-bound HMAC CSRF tokens (form field + `X-CSRF-Token`), origin check |
//! | [`oauth`] | Google (OIDC: code + PKCE + state + nonce, id_token verified with JWKS) and GitHub |
//! | [`totp`] | RFC 6238 TOTP with replay protection, recovery codes |
//! | [`rbac`] | permission catalog, [`PermissionSet`](rbac::PermissionSet), default roles, [`Principal`](rbac::Principal), impersonation |
//! | [`tokens`] | one-time tokens (password reset, email verification, invite) and API keys |
//! | [`ratelimit`] | in-memory token-bucket limiter and login lockout policy |
//! | [`legacy`] | verify-only checks of imported (WHMCS) bcrypt / MD5 hashes, `verify_any` for logins |
//!
//! CPU-heavy calls ([`password::hash_password`], [`password::verify_password`],
//! [`password::verify_user_password`]) take tens of milliseconds by design; call
//! them from `tokio::task::spawn_blocking` in async handlers.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod csrf;
pub mod error;
pub mod legacy;
pub mod oauth;
pub mod password;
pub mod ratelimit;
pub mod rbac;
pub mod session;
pub mod tokens;
pub mod totp;

pub use error::AuthError;
pub use oauth::{OAuthError, OAuthProfile, OAuthProvider};
pub use rbac::{Permission, PermissionSet, Principal};
