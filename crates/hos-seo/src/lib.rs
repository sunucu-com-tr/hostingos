//! # hos-seo
//!
//! HostingOS SEO / GEO / AI toolkit. Pure functions over plain structs: this
//! crate never touches the database — `hos-web` reads tenant data and passes it
//! in; everything here returns HTML fragments, XML, text or reports.
//!
//! | Module | Purpose |
//! |---|---|
//! | [`meta`] | `<head>` tags ([`meta::PageMeta::render_head`]), title templates, grapheme-safe truncation, canonical URL normalisation |
//! | [`jsonld`] | schema.org builders → `serde_json::Value` → XSS-safe `<script type="application/ld+json">`, Google rich-result validation |
//! | [`sitemap`] | streaming sitemap / sitemap-index writers with 50 000 URL / 50 MiB splitting, hreflang and image extensions |
//! | [`og`] | Share images (1200×630 PNG from the brand colours, pure Rust) |
//! | [`robots`] | `robots.txt` from a tenant policy, with an AI crawler catalogue (training / search / user-fetch) |
//! | [`llms`] | `llms.txt`, `llms-full.txt` and per-page `.md` renditions |
//! | [`markdown`] | Clean HTML → Markdown conversion |
//! | [`geo`] | Generative Engine Optimization analyser (citability score 0–100) |
//! | [`audit`] | Classic on-page SEO auditor + site-wide report |
//! | [`indexnow`] | IndexNow key handling and batch submission |
//! | [`ai`] | `AiProvider` implementations (Anthropic Messages API, OpenAI-compatible) + `build`/`descriptors` |
//! | [`ai_tasks`] | Structured AI content tasks (meta, product copy, FAQ, blog, KB, GEO rewrite, translation, …) |
//! | [`i18n`] | Default tr/en texts for every report message key |
//! | [`escape`] | HTML / attribute / XML / JSON-in-script escaping |

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ai;
pub mod ai_tasks;
pub mod audit;
pub mod error;
pub mod escape;
pub mod geo;
pub mod i18n;
pub mod indexnow;
pub mod jsonld;
pub mod llms;
pub mod markdown;
pub mod meta;
pub mod og;
pub mod robots;
pub mod sitemap;

mod page;
mod text;

pub use ai::{OutboundPolicy, build, build_with_policy, descriptors};
pub use error::SeoError;
