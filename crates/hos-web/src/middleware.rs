//! Cross-cutting middleware: security headers, caching / ETag and the error
//! page renderer (which also applies the `redirects` table on 404s).

use std::sync::Arc;

use askama::Template;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use hos_db::repo::content;
use http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH, VARY};
use http::{HeaderValue, Method, StatusCode};
use serde_json::json;

use crate::csrf::CsrfToken;
use crate::error::{AppError, ErrorInfo};
use crate::extract::Htmx;
use crate::locale::is_app_path;
use crate::session::{SessionArea, SessionInfo};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::PageCtx;

/// Content-Security-Policy. No inline scripts or styles anywhere: page data
/// goes into `data-*` attributes, tenant colours come from `/brand.css`, htmx
/// runs with `allowEval: false` and its indicator styles live in our CSS.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: https:; \
     font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; \
     frame-ancestors 'none'";

/// Middleware: security headers on every response.
pub async fn security_headers(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let csp = if state.web.secure {
        format!("{CSP}; upgrade-insecure-requests")
    } else {
        CSP.to_owned()
    };
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.entry("content-security-policy").or_insert(v);
    }
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    // `or_insert`: pages reached through a secret link (public quote
    // links) tighten this to `no-referrer`.
    h.entry("referrer-policy")
        .or_insert(HeaderValue::from_static("strict-origin-when-cross-origin"));
    // `or_insert`: a staff theme preview (crate::theme::finish) is framed by
    // the Appearance page and sets SAMEORIGIN together with its own CSP.
    h.entry("x-frame-options")
        .or_insert(HeaderValue::from_static("DENY"));
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=(), usb=()"),
    );
    h.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    if state.web.secure {
        h.insert(
            "strict-transport-security",
            HeaderValue::from_static("max-age=63072000; includeSubDomains"),
        );
    }
    res
}

/// Middleware: `Cache-Control` defaults + weak ETag / `304` for HTML pages.
///
/// * anonymous storefront pages: `private, max-age=60` (+ `Vary: Cookie`);
/// * logged-in pages and app areas: `private, no-cache`; `/auth/*`: `no-store`.
///
/// The ETag ignores the per-request CSRF token, so revalidation works even
/// though every render embeds a fresh token, but includes what the token is
/// bound to (session / anonymous cookie), so a page is never revalidated
/// across sessions. Handlers may set their own `Cache-Control` (left
/// untouched).
pub async fn cache_control(req: Request, next: Next) -> Response {
    let cacheable_method = matches!(*req.method(), Method::GET | Method::HEAD);
    let path = req.uri().path().to_owned();
    let logged_in = req.extensions().get::<Arc<SessionInfo>>().is_some();
    let csrf = req.extensions().get::<CsrfToken>().map(|c| c.0.clone());
    // What the embedded CSRF token is bound to (the session, else the
    // anonymous cookie, also when it is issued with this response). It is
    // part of the ETag: after a new login the page bytes can be identical
    // apart from the token, and a `304` would leave the browser with a form
    // whose token belongs to the previous session (every POST then fails).
    let csrf_binding = req
        .extensions()
        .get::<crate::csrf::CsrfBinding>()
        .map(|b| b.0.clone())
        .unwrap_or_default();
    let if_none_match = req
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let res = next.run(req).await;
    if !cacheable_method
        || res.status() != StatusCode::OK
        || res.headers().contains_key(CACHE_CONTROL)
    {
        return res;
    }
    let is_html = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if !is_html {
        return res;
    }
    let policy = if path == "/auth" || path.starts_with("/auth/") {
        "no-store"
    } else if logged_in || is_app_path(&path) {
        "private, no-cache"
    } else {
        "private, max-age=60"
    };
    let (mut parts, body) = res.into_parts();
    parts
        .headers
        .insert(CACHE_CONTROL, HeaderValue::from_static(policy));
    parts
        .headers
        .insert(VARY, HeaderValue::from_static("Cookie"));
    if policy == "no-store" {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, 4 * 1024 * 1024).await else {
        return AppError::internal("response body too large for etag").into_response();
    };
    let digest = match &csrf {
        Some(token) if !token.is_empty() => {
            let mut text = String::from_utf8_lossy(&bytes).replace(token.as_str(), "");
            text.push('\0');
            text.push_str(&csrf_binding);
            hos_core::crypto::sha256_hex(text.as_bytes())
        }
        _ => hos_core::crypto::sha256_hex(&bytes),
    };
    let etag = format!("W/\"{}\"", &digest[..20]);
    if let Ok(v) = HeaderValue::from_str(&etag) {
        parts.headers.insert(ETAG, v);
    }
    if if_none_match.is_some_and(|inm| inm.split(',').any(|t| t.trim() == etag)) {
        parts.headers.remove(CONTENT_TYPE);
        parts.headers.remove(http::header::CONTENT_LENGTH);
        let mut not_modified = Response::from_parts(parts, Body::empty());
        *not_modified.status_mut() = StatusCode::NOT_MODIFIED;
        return not_modified;
    }
    Response::from_parts(parts, Body::from(bytes))
}

// ---------------------------------------------------------------------------
// Error pages
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "errors/error.html")]
struct ErrorPage {
    ctx: PageCtx,
    status: u16,
    code: String,
    heading: String,
    text: String,
    detail: String,
    storefront: bool,
}
crate::page!(ErrorPage);

fn wants_json(path: &str, headers: &http::HeaderMap) -> bool {
    if path == "/api" || path.starts_with("/api/") || path.starts_with("/webhooks/") {
        return true;
    }
    let accept = headers
        .get(http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    accept.contains("application/json") && !accept.contains("text/html")
}

fn synthesize(status: StatusCode) -> ErrorInfo {
    let code = match status.as_u16() {
        404 | 405 => "not_found",
        413 => "payload_too_large",
        429 => "rate_limited",
        400..=499 => "bad_request",
        _ => "internal",
    };
    ErrorInfo {
        status,
        code,
        detail: None,
    }
}

/// Middleware: turns error responses into HTML pages / problem+json /
/// login redirects / htmx toasts, and applies the `redirects` table on 404.
pub async fn error_pages(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let extensions = req.extensions().clone();
    let res = next.run(req).await;
    let status = res.status();
    if status.as_u16() < 400 {
        return res;
    }
    let info = match res.extensions().get::<ErrorInfo>() {
        Some(i) => i.clone(),
        None => {
            let ct = res
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if ct.starts_with("text/html") || ct.contains("json") {
                return res;
            }
            synthesize(status)
        }
    };
    let path = uri.path().to_owned();
    let tenant = extensions.get::<Tenant>().cloned();

    // Configured redirects (only looked up on 404s).
    if status == StatusCode::NOT_FOUND
        && method == Method::GET
        && let Some(t) = &tenant
    {
        match content::hit_redirect(&state.pool, t.id, &path).await {
            Ok(Some(r)) => {
                let target = r.to_url;
                return match r.status_code {
                    // `Redirect::permanent` is 308; a configured 301 must stay 301.
                    301 => (
                        StatusCode::MOVED_PERMANENTLY,
                        [(http::header::LOCATION, target)],
                    )
                        .into_response(),
                    307 => Redirect::temporary(&target).into_response(),
                    308 => (
                        StatusCode::PERMANENT_REDIRECT,
                        [(http::header::LOCATION, target)],
                    )
                        .into_response(),
                    _ => (StatusCode::FOUND, [(http::header::LOCATION, target)]).into_response(),
                };
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "redirect lookup failed"),
        }
    }

    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(h) = builder.headers_mut() {
        *h = headers.clone();
    }
    let (mut parts, ()) = match builder.body(()) {
        Ok(r) => r.into_parts(),
        Err(_) => return res,
    };
    parts.extensions = extensions;

    if wants_json(&path, &headers) {
        return problem_json(&info, res.headers().get(http::header::RETRY_AFTER).cloned());
    }

    let htmx = Htmx::from_headers(&headers);
    if info.status == StatusCode::UNAUTHORIZED {
        let ctx = PageCtx::from_parts(&mut parts, &state);
        let login = match SessionArea::for_path(&path) {
            SessionArea::Platform => ctx.url("/platform/login"),
            SessionArea::Tenant => {
                let next: String =
                    url::form_urlencoded::byte_serialize(ctx.current_url().as_bytes()).collect();
                format!("{}?next={next}", ctx.url("/auth/login"))
            }
        };
        if htmx.request {
            let mut r = StatusCode::UNAUTHORIZED.into_response();
            if let Ok(v) = HeaderValue::from_str(&login) {
                r.headers_mut().insert("hx-redirect", v);
            }
            return r;
        }
        return Redirect::to(&login).into_response();
    }

    let mut ctx = PageCtx::from_parts(&mut parts, &state);
    if ctx.csrf.is_empty() {
        ctx.csrf = crate::csrf::token_for_parts(&state, &parts);
    }
    let code = info.code;
    let heading = ctx.t(&format!("errors.{code}.title"));
    let text = ctx.t(&format!("errors.{code}.text"));
    let detail = info.detail.clone().unwrap_or_default();

    if htmx.request {
        let msg = if detail.is_empty() { text } else { detail };
        let payload = json!({ "hos:toast": [{ "kind": "error", "text": msg }] });
        let mut r = info.status.into_response();
        if let Ok(v) = HeaderValue::from_str(&payload.to_string()) {
            r.headers_mut().insert("hx-trigger", v);
        }
        return r;
    }

    let storefront = tenant.is_some() && !is_app_path(&path);
    if storefront && let Err(e) = ctx.load_store().await {
        tracing::warn!(error = %e, "store chrome for error page");
    }
    ctx.set_title(heading.clone());
    ctx.body_class = "page-error".into();
    let page = ErrorPage {
        ctx,
        status: info.status.as_u16(),
        code: code.to_owned(),
        heading,
        text,
        detail,
        storefront,
    };
    let retry = res.headers().get(http::header::RETRY_AFTER).cloned();
    match page.render() {
        Ok(html) => {
            let mut r = (info.status, Html(html)).into_response();
            r.headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Some(v) = retry {
                r.headers_mut().insert(http::header::RETRY_AFTER, v);
            }
            r
        }
        Err(e) => {
            tracing::error!(error = %e, "rendering error page failed");
            (info.status, "Error").into_response()
        }
    }
}

fn problem_json(info: &ErrorInfo, retry: Option<HeaderValue>) -> Response {
    let body = json!({
        "type": "about:blank",
        "title": info.status.canonical_reason().unwrap_or("Error"),
        "status": info.status.as_u16(),
        "code": info.code,
        "detail": info.detail,
    });
    let mut r = (info.status, body.to_string()).into_response();
    r.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    if let Some(v) = retry {
        r.headers_mut().insert(http::header::RETRY_AFTER, v);
    }
    r
}

/// Router fallback: a 404 rendered by [`error_pages`].
pub async fn not_found() -> AppError {
    AppError::NotFound
}

/// `CatchPanicLayer` handler: logs and answers 500 (rendered by [`error_pages`]).
pub fn panic_response(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("panic");
    AppError::internal(format!("handler panicked: {msg}")).into_response()
}
