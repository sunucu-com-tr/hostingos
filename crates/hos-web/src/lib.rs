//! # hos-web
//!
//! The HostingOS axum application (binary `hostingos`). It wires every other
//! crate together: tenant resolution, sessions, CSRF, i18n, the design
//! system (Askama templates + `static/`), the SEO-first storefront, the auth
//! flows and the skeletons of the client area, admin panel, platform console,
//! REST API and webhooks.
//!
//! **Conventions for contributors are documented in `crates/hos-web/README.md`.**
//!
//! | Module | Contents |
//! |---|---|
//! | [`state`] | [`AppState`] (pool, config, secret box, CSRF, limiters, i18n, assets, caches) |
//! | [`routes`] | the router: one sub-module per area (`storefront`, `auth`, `client`, `admin`, `platform`, `api`, `webhooks`) |
//! | [`extract`] | request extractors (`Tenant`, `ReqLocale`, `MaybeUser`, `CurrentClient`, `CurrentStaff`, `RequirePermission`, `PlatformAdmin`, `ClientIp`, `Htmx`, `PageQuery`, …) |
//! | [`view`] | [`view::PageCtx`] (shared template context + helpers), menus, pagination, rendering |
//! | [`error`] | [`AppError`] → pretty HTML error pages or `application/problem+json` |
//! | [`i18n`] | locale catalogs (`locales/{tr,en}/*.json`), money / date formatting |
//! | [`session`], [`csrf`], [`flash`] | session cookies + principal loading, CSRF middleware, signed flash cookies |
//! | [`einvoice`] | e-invoice / invoice-PDF web helpers: VKN / TCKN form rules, safe file downloads |
//! | [`tenant`], [`locale`] | `Host` → tenant (cached), locale prefix handling |
//! | [`theme`] | tenant theming: design templates, brand colours, fonts, `/brand.css` |
//! | [`middleware`] | security headers, caching / ETag, error pages, redirects |
//! | [`mail`] | [`mail::send_email`] (template → layout → `email_log` → mailer) |
//! | [`seo`] | page metadata / JSON-LD helpers on top of `hos-seo` |
//! | [`worker`] | background runtime: job worker + scheduler + platform maintenance, graceful shutdown |
//! | [`demo`] | public demo mode (`HOS_DEMO_MODE`): request policy, scheduled reset, mailbox — docs/DEMO.md |
//! | [`server`] | the accept loop (header-read timeout) and the request limits (body / request timeouts) |
//! | [`schema`] | migrate on start (`serve --migrate`, `HOS_MIGRATE_ON_START`), schema check behind `/readyz` |

#![forbid(unsafe_code)]

pub mod assets;
pub mod cli;
pub mod config;
pub mod console_relay;
pub mod csrf;
pub mod demo;
pub mod einvoice;
pub mod error;
pub mod extract;
pub mod flash;
pub mod forms;
pub mod i18n;
pub mod inbound;
pub mod locale;
pub mod mail;
pub mod markdown;
pub mod middleware;
pub mod mode;
pub mod routes;
pub mod schema;
pub mod seed_demo;
pub mod seo;
pub mod server;
pub mod session;
pub mod state;
pub mod tenant;
pub mod theme;
pub mod view;
pub mod vm_charts;
pub mod worker;

pub use error::{AppError, AppResult};
pub use routes::app;
pub use state::AppState;
