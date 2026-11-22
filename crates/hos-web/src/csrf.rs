//! CSRF protection for every unsafe request (POST/PUT/PATCH/DELETE).
//!
//! * Tokens come from [`hos_auth::csrf::CsrfProtector`] and are bound to the
//!   session (`token_hash`) or, for anonymous visitors, to the random
//!   `hos_anon` cookie this middleware sets.
//! * Accepted from the `X-CSRF-Token` header (htmx: `static/js/app.js` adds it
//!   from `<meta name="csrf-token">`) or the `csrf_token` form field
//!   (urlencoded and multipart bodies).
//! * A present `Origin` header must be same-origin.
//! * Exempt: `/webhooks/*` (signed by the provider) and `/api/*` requests that
//!   authenticate with an `Authorization: Bearer <api key>` header (API keys
//!   are not ambient credentials; the API gate never falls back to the
//!   session cookie for them). Any other `Authorization` value does not
//!   exempt the request: the gate then authenticates with the cookie.
//!
//! Handlers / templates read the token for the current request with the
//! [`CsrfToken`] extension (`PageCtx.csrf`, `{% call ui::csrf() %}`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hos_auth::csrf::{CSRF_FORM_FIELD, CSRF_HEADER, extract_token, is_safe_method, is_same_origin};
use hos_auth::session::{CookieAttributes, build_cookie, cookie_value, is_well_formed_token};
use http::header::{CONTENT_TYPE, SET_COOKIE};
use http::request::Parts;
use http::{HeaderMap, HeaderValue};

use crate::error::AppError;
use crate::session::SessionInfo;
use crate::state::AppState;

/// Anonymous pre-session cookie (CSRF binding for login / signup forms).
pub const ANON_COOKIE: &str = "hos_anon";

/// CSRF token for the current request (request extension + extractor).
#[derive(Debug, Clone, Default)]
pub struct CsrfToken(pub String);

/// What the request's [`CsrfToken`] is bound to (the session, or the
/// anonymous cookie — possibly one issued with this response). Request
/// extension; `middleware::cache_control` puts it into the page ETag.
#[derive(Debug, Clone, Default)]
pub struct CsrfBinding(pub String);

impl<S: Send + Sync> FromRequestParts<S> for CsrfToken {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<CsrfToken>()
            .cloned()
            .unwrap_or_default())
    }
}

fn anon_binding(value: &str) -> String {
    // Pre-rename "hf" label, kept on purpose: hash label of the anonymous CSRF binding.
    hos_core::crypto::sha256_hex(format!("hf-anon:{value}").as_bytes())
}

/// The request authenticates with an API key (the gate's Bearer path).
fn has_bearer_key(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(hos_auth::tokens::bearer_token)
        .is_some()
}

fn is_exempt(path: &str, headers: &HeaderMap) -> bool {
    path.starts_with("/webhooks/")
        || (path.starts_with("/api/") && has_bearer_key(headers))
        || path == "/healthz"
        || path == "/readyz"
}

/// The origin this request was addressed to (`scheme://host[:port]`), built
/// from the validated [`RequestHost`](crate::tenant::RequestHost) that
/// tenant resolution used — never from a re-parse of the raw header. `None`
/// without a host.
pub fn request_origin(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &http::Extensions,
) -> Option<url::Url> {
    let host = extensions.get::<crate::tenant::RequestHost>()?;
    let base_scheme = state.config.base_url.scheme();
    // An https deployment is reached over https only (Secure cookies, HSTS);
    // the scheme a proxy chain reports for its last hop says nothing about
    // the browser (e.g. Cloudflare → Traefik over plain HTTP).
    let scheme = if base_scheme == "https" {
        base_scheme.to_owned()
    } else if state
        .web
        .forwarded_headers_trusted(crate::extract::peer_ip(extensions))
    {
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| v == "http" || v == "https")
            .unwrap_or_else(|| base_scheme.to_owned())
    } else {
        base_scheme.to_owned()
    };
    url::Url::parse(&format!("{scheme}://{}", host.authority())).ok()
}

/// A token for a request that did not pass through [`csrf_protect`] (error
/// pages rendered by an outer middleware). Empty when the visitor has no
/// binding yet (no session and no `hos_anon` cookie).
pub fn token_for_parts(state: &AppState, parts: &Parts) -> String {
    if let Some(t) = parts.extensions.get::<CsrfToken>() {
        return t.0.clone();
    }
    let binding = parts
        .extensions
        .get::<Arc<SessionInfo>>()
        .map(|s| s.token_hash.clone())
        .or_else(|| {
            parts
                .headers
                .get_all(http::header::COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find_map(|h| cookie_value(h, ANON_COOKIE))
                .filter(|v| is_well_formed_token(v))
                .map(anon_binding)
        });
    binding.map(|b| state.csrf.generate(&b)).unwrap_or_default()
}

/// Middleware (see module docs).
pub async fn csrf_protect(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let session_binding = req
        .extensions()
        .get::<Arc<SessionInfo>>()
        .map(|s| s.token_hash.clone());
    let anon = req
        .headers()
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| cookie_value(h, ANON_COOKIE))
        .filter(|v| is_well_formed_token(v))
        .map(str::to_owned);
    let mut new_anon = None;
    let binding = match (&session_binding, &anon) {
        (Some(b), _) => b.clone(),
        (None, Some(a)) => anon_binding(a),
        (None, None) => {
            let fresh = hos_core::crypto::generate_token();
            let b = anon_binding(&fresh);
            new_anon = Some(fresh);
            b
        }
    };

    let mut req = req;
    if !is_safe_method(req.method().as_str()) && !is_exempt(req.uri().path(), req.headers()) {
        if let Some(origin) = req.headers().get(http::header::ORIGIN) {
            let ok = origin.to_str().ok().is_some_and(|o| {
                request_origin(&state, req.headers(), req.extensions())
                    .is_some_and(|expected| is_same_origin(o, &expected))
            });
            if !ok {
                return AppError::Csrf.into_response();
            }
        }
        let header_token = req
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let (token, rebuilt) = if header_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            (header_token, req)
        } else {
            match form_token(req, state.web.max_body_bytes).await {
                Ok(pair) => pair,
                Err(e) => return e.into_response(),
            }
        };
        req = rebuilt;
        let presented = extract_token(token.as_deref(), None);
        // Without an existing binding cookie no token can be valid.
        let valid = new_anon.is_none() && presented.is_some_and(|t| state.csrf.verify(&binding, t));
        if !valid {
            return AppError::Csrf.into_response();
        }
    }

    req.extensions_mut()
        .insert(CsrfToken(state.csrf.generate(&binding)));
    req.extensions_mut().insert(CsrfBinding(binding));
    let mut res = next.run(req).await;
    if let Some(value) = new_anon {
        let cookie = build_cookie(
            ANON_COOKIE,
            &value,
            &CookieAttributes {
                secure: state.web.secure,
                max_age_secs: Some(30 * 24 * 3600),
                ..CookieAttributes::default()
            },
            chrono::Utc::now(),
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            res.headers_mut().append(SET_COOKIE, v);
        }
    }
    res
}

/// Buffers the body (up to `limit`) and extracts the `csrf_token` field.
async fn form_token(req: Request, limit: usize) -> Result<(Option<String>, Request), AppError> {
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let urlencoded = content_type.starts_with("application/x-www-form-urlencoded");
    let multipart = content_type.starts_with("multipart/form-data");
    if !urlencoded && !multipart {
        return Ok((None, req));
    }
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| AppError::PayloadTooLarge)?;
    let token = if urlencoded {
        url::form_urlencoded::parse(&bytes)
            .find(|(k, _)| k == CSRF_FORM_FIELD)
            .map(|(_, v)| v.into_owned())
    } else {
        multipart_field(&bytes, CSRF_FORM_FIELD)
    };
    Ok((token, Request::from_parts(parts, Body::from(bytes))))
}

/// Minimal multipart scan for a small text field.
fn multipart_field(body: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let marker = format!("name=\"{name}\"");
    let start = text.find(&marker)?;
    let after = &text[start..];
    let value_start = after.find("\r\n\r\n")? + 4;
    let value = &after[value_start..];
    let end = value.find("\r\n").unwrap_or(value.len());
    Some(value[..end].trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_scan() {
        let body = b"--x\r\nContent-Disposition: form-data; name=\"csrf_token\"\r\n\r\nabc123\r\n--x\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n--x--";
        assert_eq!(
            multipart_field(body, "csrf_token").as_deref(),
            Some("abc123")
        );
        assert_eq!(multipart_field(body, "nope"), None);
    }

    #[test]
    fn exemptions() {
        let mut h = HeaderMap::new();
        assert!(is_exempt("/webhooks/gateway/x", &h));
        assert!(!is_exempt("/api/v1/x", &h));
        // Not an API key: the gate authenticates with the cookie, so CSRF applies.
        for value in ["Basic dXNlcjpwYXNz", "Bearer", "Bearer   ", "x"] {
            h.insert(http::header::AUTHORIZATION, HeaderValue::from_static(value));
            assert!(!is_exempt("/api/v1/x", &h), "{value}");
        }
        h.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer x"),
        );
        assert!(is_exempt("/api/v1/x", &h));
        assert!(!is_exempt("/auth/login", &h));
    }
}
