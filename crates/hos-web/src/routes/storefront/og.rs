//! `GET /og/{key}.png`: the generated share image of a page
//! (`hos_seo::og`), in the tenant's template composition and colours.
//! `?v=<theme hash>` (what pages link to) → cached for a year; the PNG is
//! also kept in a small in-process LRU cache keyed by theme hash + page key.
//!
//! Only keys of real pages get an image (`404` otherwise): the fixed
//! storefront routes and published content
//! (`hos_db::repo::content::share_key_exists`), so made-up keys cost no
//! rendering and cannot push real images out of the cache. Requests are
//! rate-limited per client IP (`Limits::og`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hos_db::repo::content;
use http::HeaderValue;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};

use crate::error::{AppError, AppResult};
use crate::extract::ClientIp;
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::PageCtx;

/// Images kept in memory (a PNG is some tens of KiB).
pub const MAX_CACHED: usize = 256;

/// Keys of the fixed storefront routes that link a share image.
const FIXED_KEYS: &[&str] = &[
    "home",
    "domains",
    "whois",
    "bundles",
    "downloads",
    "contact",
    "status",
    "kb",
    "kb_search",
    "blog",
    "announcements",
];

/// Least-recently-used map of rendered images.
#[derive(Default)]
struct Lru {
    entries: HashMap<String, (Bytes, u64)>,
    clock: u64,
}

impl Lru {
    fn get(&mut self, key: &str) -> Option<Bytes> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(key).map(|(png, used)| {
            *used = clock;
            png.clone()
        })
    }

    fn insert(&mut self, key: String, png: Bytes) {
        self.clock += 1;
        if self.entries.len() >= MAX_CACHED
            && !self.entries.contains_key(&key)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, (png, self.clock));
    }
}

fn cache() -> &'static Mutex<Lru> {
    static CACHE: OnceLock<Mutex<Lru>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Lru::default()))
}

/// `GET /og/{file}`.
pub(super) async fn image(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ctx: PageCtx,
    Path(file): Path<String>,
) -> AppResult<Response> {
    check_rate(&state.limits.og, &ip.key("og"))?;
    let key = file.strip_suffix(".png").ok_or(AppError::NotFound)?;
    if key.is_empty()
        || key.len() > 120
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AppError::NotFound);
    }
    if !FIXED_KEYS.contains(&key) && !content::share_key_exists(&state.pool, tenant.id, key).await?
    {
        return Err(AppError::NotFound);
    }
    let theme = ctx.theme.clone();
    let cache_key = format!("{}:{key}", theme.version);
    let hit = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_key);
    let png = match hit {
        Some(b) => b,
        None => {
            let spec = theme.share_image(key);
            let bytes = tokio::task::spawn_blocking(move || hos_seo::og::render(&spec))
                .await
                .map_err(AppError::internal)?;
            let bytes = Bytes::from(bytes);
            cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(cache_key, bytes.clone());
            bytes
        }
    };
    let current = url::form_urlencoded::parse(ctx.query.as_bytes())
        .any(|(k, v)| k == "v" && v == theme.version.as_str());
    let policy = if theme.preview.is_some() {
        "private, no-store"
    } else if current {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    Ok((
        [
            (CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (CACHE_CONTROL, HeaderValue::from_static(policy)),
        ],
        png,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_keeps_recent_images_within_the_bound() {
        let mut lru = Lru::default();
        for i in 0..MAX_CACHED {
            lru.insert(format!("k{i}"), Bytes::from_static(b"png"));
        }
        // Touch the oldest so it is no longer the least recently used.
        assert!(lru.get("k0").is_some());
        lru.insert("new".into(), Bytes::from_static(b"png"));
        assert_eq!(lru.entries.len(), MAX_CACHED);
        assert!(lru.get("k0").is_some(), "recently used entry kept");
        assert!(lru.get("k1").is_none(), "least recently used entry evicted");
        assert!(lru.get("new").is_some());
    }
}
