//! Locale handling.
//!
//! URLs of the tenant's default language have no prefix; the other language
//! is prefixed (`/en/...` when the default is `tr`). [`locale_prefix`] strips
//! the prefix before routing (routes are declared once) and redirects
//! `/<default>/...` to the unprefixed URL.
//!
//! Resolution order ([`resolve`]):
//!
//! * **storefront** (SEO: one URL = one language): path prefix › tenant default;
//! * **app areas** (`/auth`, `/client`, `/admin`, `/platform`, `/api`):
//!   path prefix › `hos_locale` cookie › the user's saved preference ›
//!   `Accept-Language` › tenant default.
//!
//! The language switcher (`GET /lang/{code}?next=`) sets the cookie (and the
//! user's preference when logged in) and redirects to the localized URL.

use std::convert::Infallible;

use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use hos_core::domain::Locale;
use http::request::Parts;
use http::{Method, Uri};

use crate::session::SessionInfo;
use crate::state::AppState;
use crate::tenant::Tenant;

/// Cookie remembering the chosen language.
pub const LOCALE_COOKIE: &str = "hos_locale";

/// Locale found in the URL prefix (request extension set by [`locale_prefix`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathLocale(pub Option<Locale>);

/// Where the effective locale came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocaleSource {
    /// `/en/...` prefix.
    Path,
    /// `hos_locale` cookie.
    Cookie,
    /// `users.locale`.
    User,
    /// `Accept-Language`.
    AcceptLanguage,
    /// Tenant default.
    Default,
}

/// The effective locale of a request (extractor; cached in extensions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReqLocale {
    /// Effective locale.
    pub locale: Locale,
    /// Tenant default (unprefixed URLs).
    pub default: Locale,
    /// Source.
    pub source: LocaleSource,
}

impl ReqLocale {
    /// `path` (unprefixed, starting with `/`) in this locale.
    pub fn url(&self, path: &str) -> String {
        localize_path(path, self.locale, self.default)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ReqLocale {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(resolve(parts))
    }
}

/// `path` with the locale prefix when `locale` is not the default.
pub fn localize_path(path: &str, locale: Locale, default: Locale) -> String {
    let path = if path.is_empty() { "/" } else { path };
    if locale == default || !path.starts_with('/') {
        return path.to_owned();
    }
    if path == "/" {
        format!("/{}/", locale.as_str())
    } else {
        format!("/{}{}", locale.as_str(), path)
    }
}

/// Splits a leading locale segment: `/en/x` → `(En, "/x")`.
pub fn split_locale_prefix(path: &str) -> Option<(Locale, &str)> {
    let rest = path.strip_prefix('/')?;
    let (seg, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let locale: Locale = seg.parse().ok()?;
    Some((locale, if tail.is_empty() { "/" } else { tail }))
}

/// Whether a browser would read `path` as a scheme-relative URL
/// (`//evil.com`, `/\evil.com`) instead of a path on this host.
pub fn is_scheme_relative(path: &str) -> bool {
    path.starts_with("//") || path.starts_with("/\\")
}

/// `target` (`/path[?query]`) when it is a safe same-host redirect target
/// both as given and once a leading locale segment is stripped (or added):
/// `/en//evil.com` passes [`hos_auth::oauth::safe_return_path`] but becomes
/// `//evil.com` after the prefix is removed, so it is refused here.
pub fn safe_local_target(target: &str) -> Option<&str> {
    let target = hos_auth::oauth::safe_return_path(target)?;
    let path = target.split('?').next().unwrap_or(target);
    let bare = split_locale_prefix(path).map_or(path, |(_, rest)| rest);
    (!is_scheme_relative(bare)).then_some(target)
}

/// Whether `path` belongs to an application area (not the storefront).
pub fn is_app_path(path: &str) -> bool {
    [
        "/auth",
        "/client",
        "/admin",
        "/platform",
        "/api",
        "/webhooks",
    ]
    .iter()
    .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// Tenant default locale for the request (`tr` without a tenant).
pub fn default_locale(parts_ext: &http::Extensions) -> Locale {
    parts_ext
        .get::<Tenant>()
        .map(|t| t.default_locale)
        .unwrap_or_default()
}

fn cookie_locale(headers: &http::HeaderMap) -> Option<Locale> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| hos_auth::session::cookie_value(h, LOCALE_COOKIE))
        .and_then(|v| v.parse().ok())
}

/// Resolves (and caches) the request locale.
pub fn resolve(parts: &mut Parts) -> ReqLocale {
    if let Some(r) = parts.extensions.get::<ReqLocale>() {
        return *r;
    }
    let default = default_locale(&parts.extensions);
    let path_locale = parts.extensions.get::<PathLocale>().and_then(|p| p.0);
    let app = is_app_path(parts.uri.path());
    let (locale, source) = if let Some(l) = path_locale {
        (l, LocaleSource::Path)
    } else if !app {
        (default, LocaleSource::Default)
    } else if let Some(l) = cookie_locale(&parts.headers) {
        (l, LocaleSource::Cookie)
    } else if let Some(l) = parts
        .extensions
        .get::<std::sync::Arc<SessionInfo>>()
        .and_then(|s| s.user.as_ref().map(|u| u.locale))
    {
        (l, LocaleSource::User)
    } else if let Some(l) = parts
        .headers
        .get(http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(Locale::negotiate)
    {
        (l, LocaleSource::AcceptLanguage)
    } else {
        (default, LocaleSource::Default)
    };
    let r = ReqLocale {
        locale,
        default,
        source,
    };
    parts.extensions.insert(r);
    r
}

/// Middleware: strips the locale prefix (see module docs).
pub async fn locale_prefix(
    State(_state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let default = default_locale(req.extensions());
    let path = req.uri().path().to_owned();
    let mut found = None;
    if let Some((locale, rest)) = split_locale_prefix(&path) {
        // `/tr//evil.com` must not become a redirect to (or a request for)
        // the scheme-relative `//evil.com`.
        if is_scheme_relative(rest) {
            return crate::error::AppError::NotFound.into_response();
        }
        if locale == default {
            if matches!(*req.method(), Method::GET | Method::HEAD) {
                let target = match req.uri().query() {
                    Some(q) => format!("{rest}?{q}"),
                    None => rest.to_owned(),
                };
                return match safe_local_target(&target) {
                    Some(t) => Redirect::permanent(t).into_response(),
                    None => crate::error::AppError::NotFound.into_response(),
                };
            }
        } else {
            found = Some(locale);
        }
        let new_pq = match req.uri().query() {
            Some(q) => format!("{rest}?{q}"),
            None => rest.to_owned(),
        };
        let mut uri_parts = req.uri().clone().into_parts();
        if let Ok(pq) = new_pq.parse() {
            uri_parts.path_and_query = Some(pq);
            if let Ok(uri) = Uri::from_parts(uri_parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    req.extensions_mut().insert(PathLocale(found));
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(localize_path("/", Locale::En, Locale::Tr), "/en/");
        assert_eq!(
            localize_path("/kb/x", Locale::En, Locale::Tr),
            "/kb/x".replace("/kb", "/en/kb")
        );
        assert_eq!(localize_path("/kb/x", Locale::Tr, Locale::Tr), "/kb/x");
        assert_eq!(
            localize_path("https://x", Locale::En, Locale::Tr),
            "https://x"
        );
        assert_eq!(split_locale_prefix("/en/kb"), Some((Locale::En, "/kb")));
        assert_eq!(split_locale_prefix("/en"), Some((Locale::En, "/")));
        assert_eq!(split_locale_prefix("/en/"), Some((Locale::En, "/")));
        assert_eq!(split_locale_prefix("/entry"), None);
        assert!(is_app_path("/client/x"));
        assert!(is_app_path("/auth"));
        assert!(!is_app_path("/authors"));
        assert!(!is_app_path("/"));
    }

    #[test]
    fn redirect_targets_stay_on_this_host() {
        assert_eq!(safe_local_target("/kb?q=x"), Some("/kb?q=x"));
        assert_eq!(safe_local_target("/en/kb"), Some("/en/kb"));
        assert_eq!(safe_local_target("/en"), Some("/en"));
        for bad in [
            "//evil.com",
            "/\\evil.com",
            "/en//evil.com",
            "/tr//evil.com?x=1",
            "/en/\\evil.com",
            "https://evil.com",
            "evil.com",
        ] {
            assert_eq!(safe_local_target(bad), None, "{bad}");
        }
        assert!(is_scheme_relative("//x") && is_scheme_relative("/\\x"));
        assert!(!is_scheme_relative("/x//y"));
    }
}
