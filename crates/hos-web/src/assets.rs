//! Static files (`/static/*`) with cache-busting URLs.
//!
//! [`Assets::url`] returns `/static/<path>?v=<hash>`; responses for versioned
//! URLs are cached for a year (`immutable`), unversioned ones for 5 minutes.
//! Release builds hash file contents once at startup; debug builds use the
//! file's modification time so edits show up without a restart.
//!
//! The stylesheets are concatenated into one bundle served at
//! `/assets/app.css?v=<hash>` (one render-blocking request instead of six);
//! the source files stay individually addressable under `/static/css/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use http::HeaderValue;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use tower_http::services::ServeDir;

/// Stylesheets of the bundle, in cascade order (`tokens.css` declares the layers).
pub const CSS_BUNDLE: &[&str] = &[
    "css/tokens.css",
    "css/base.css",
    "css/layout.css",
    "css/components.css",
    "css/pages.css",
    "css/utilities.css",
];

/// Concatenated stylesheet + its content hash.
#[derive(Debug, Clone)]
pub struct CssBundle {
    /// First 10 hex chars of the SHA-256 of `body`.
    pub version: String,
    /// The CSS.
    pub body: Bytes,
}

fn build_bundle(dir: &Path) -> CssBundle {
    let mut css = String::new();
    for file in CSS_BUNDLE {
        match std::fs::read_to_string(dir.join(file)) {
            Ok(text) => {
                css.push_str("/* ");
                css.push_str(file);
                css.push_str(" */\n");
                css.push_str(&text);
                css.push('\n');
            }
            Err(e) => tracing::warn!(file, error = %e, "stylesheet missing from bundle"),
        }
    }
    let hash = hos_core::crypto::sha256_hex(css.as_bytes());
    CssBundle {
        version: hash[..10].to_owned(),
        body: Bytes::from(css),
    }
}

/// Asset manifest.
#[derive(Debug, Clone)]
pub struct Assets {
    dir: PathBuf,
    hashes: HashMap<String, String>,
    css: CssBundle,
    live: bool,
}

impl Assets {
    /// Scans `dir`. With `live`, versions are computed from mtimes on demand.
    pub fn scan(dir: &Path, live: bool) -> Self {
        let mut hashes = HashMap::new();
        if !live {
            walk(dir, dir, &mut hashes);
        }
        Self {
            dir: dir.to_path_buf(),
            hashes,
            css: build_bundle(dir),
            live,
        }
    }

    /// The CSS bundle (rebuilt on every call in live mode).
    pub fn css_bundle(&self) -> CssBundle {
        if self.live {
            build_bundle(&self.dir)
        } else {
            self.css.clone()
        }
    }

    /// Versioned URL of the CSS bundle.
    pub fn css_url(&self) -> String {
        format!("/assets/app.css?v={}", self.css_bundle().version)
    }

    /// Directory served at `/static`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Versioned URL of `path` (relative to the static dir, e.g. `css/app.css`).
    pub fn url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        let version = if self.live {
            std::fs::metadata(self.dir.join(path))
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| format!("{:x}", d.as_secs()))
        } else {
            self.hashes.get(path).cloned()
        };
        match version {
            Some(v) => format!("/static/{path}?v={v}"),
            None => format!("/static/{path}"),
        }
    }
}

fn walk(root: &Path, dir: &Path, out: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(bytes) = std::fs::read(&path)
            && let Ok(rel) = path.strip_prefix(root)
        {
            let key = rel.to_string_lossy().replace('\\', "/");
            let hash = hos_core::crypto::sha256_hex(&bytes);
            out.insert(key, hash[..10].to_owned());
        }
    }
}

/// `/static/*` + `/assets/app.css` router (no tenant needed).
pub fn router(assets: &Assets) -> Router {
    Router::new()
        .route("/assets/app.css", get(css_bundle))
        .with_state(assets.clone())
        .nest_service(
            "/static",
            ServeDir::new(&assets.dir).append_index_html_on_directories(false),
        )
        .layer(middleware::from_fn(cache_headers))
}

async fn css_bundle(State(assets): State<Assets>) -> Response {
    let bundle = assets.css_bundle();
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("text/css; charset=utf-8"),
        )],
        bundle.body,
    )
        .into_response()
}

async fn cache_headers(req: Request, next: Next) -> Response {
    let versioned = req
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|p| p.starts_with("v=")));
    let mut res = next.run(req).await;
    if res.status().is_success() || res.status() == http::StatusCode::NOT_MODIFIED {
        let value = if versioned {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=300"
        };
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static(value));
    }
    res
}
